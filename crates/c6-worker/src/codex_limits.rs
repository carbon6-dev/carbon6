//! Opt-in plan usage through Codex's own app server, never its credentials.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use c6_api::{CodexLimitsReport, CodexPlanWindow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::Worker;

const POLL: Duration = Duration::from_secs(300);
const TIMEOUT: Duration = Duration::from_secs(10);
const CONFIG_EVERY: Duration = Duration::from_secs(30);
const MAX_LINE: u64 = 64 * 1024;

/// Duration, not primary/secondary, names a window. Newer multi-limit
/// responses take precedence over the older default snapshot.
pub fn parse(result: &Value) -> anyhow::Result<CodexLimitsReport> {
    let limits = result["rateLimitsByLimitId"].get("codex").filter(|v| v.is_object()).unwrap_or(&result["rateLimits"]);
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let w = &limits[key];
        if w.is_null() {
            continue;
        }
        // Some versions/backend buckets omit durations. Keep any other
        // usable window, but never guess its name from primary/secondary.
        if w["windowDurationMins"].is_null() {
            continue;
        }
        let used_percent = w["usedPercent"].as_f64().context("Codex returned an invalid percentage")?;
        let window_minutes = w["windowDurationMins"].as_i64().context("Codex returned no window duration")?;
        ensure!(used_percent.is_finite() && (0.0..=100.0).contains(&used_percent), "Codex returned an invalid percentage");
        ensure!(window_minutes > 0 && window_minutes <= i64::MAX / 60, "Codex returned an invalid window duration");
        let resets_at = w["resetsAt"].as_i64().filter(|s| *s > 0 && *s <= i64::MAX / 1000);
        windows.push(CodexPlanWindow { used_percent, window_minutes, resets_at });
    }
    ensure!(!windows.is_empty(), "Codex returned no plan windows (is it signed in with ChatGPT?)");
    let text = |key: &str| limits[key].as_str().or_else(|| result[key].as_str()).filter(|s| !s.is_empty()).map(str::to_owned);
    Ok(CodexLimitsReport {
        observed_at: c6_core::now_ms(),
        account_id: text("accountId"),
        plan_type: text("planType"),
        windows,
        error: String::new(),
    })
}

async fn response(rx: &mut mpsc::Receiver<Result<Value, &'static str>>, id: u64) -> anyhow::Result<Value> {
    while let Some(v) = rx.recv().await {
        let v = v.map_err(anyhow::Error::msg)?;
        if v["id"].as_u64() == Some(id) {
            return Ok(v);
        }
    }
    bail!("Codex app server closed stdout before replying")
}

/// Runs only initialize, initialized and account/rateLimits/read. Inherits
/// PATH, HOME and CODEX_HOME unchanged. Output never enters diagnostics.
pub async fn read(binary: &Path, timeout: Duration, stop: &CancellationToken) -> anyhow::Result<CodexLimitsReport> {
    let mut child = tokio::process::Command::new(binary)
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("codex not found on this worker's PATH")
            } else {
                anyhow::anyhow!("could not start Codex app server ({})", e.kind())
            }
        })?;
    let pid = child.id();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let mut out = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let value = if n as u64 == MAX_LINE {
                        Err("Codex app server returned an oversized line")
                    } else {
                        serde_json::from_slice::<Value>(&line).map_err(|_| "Codex app server returned malformed JSON")
                    };
                    let invalid = value.is_err();
                    if tx.send(value).await.is_err() || invalid {
                        break;
                    }
                }
            }
        }
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    });
    let mut err = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    let exchange = async {
        let init = json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"carbon6","version":c6_core::build()}}});
        stdin.write_all(format!("{init}\n").as_bytes()).await?;
        let init = response(&mut rx, 1).await?;
        ensure!(init["error"].is_null() && init.get("result").is_some(), "Codex app server refused initialization");
        stdin.write_all(b"{\"method\":\"initialized\"}\n{\"id\":2,\"method\":\"account/rateLimits/read\",\"params\":{\"excludeResetCreditDetails\":true}}\n").await?;
        let mut reply = response(&mut rx, 2).await?;
        // Codex's request deserializer uses Invalid Request (-32600),
        // while some versions use Invalid Params (-32602). Retry neither
        // authentication nor network errors, and never send a turn request.
        if matches!(reply["error"]["code"].as_i64(), Some(-32600 | -32602)) {
            stdin.write_all(b"{\"id\":3,\"method\":\"account/rateLimits/read\"}\n").await?;
            reply = response(&mut rx, 3).await?;
        }
        ensure!(reply["error"].is_null(), "Codex could not read plan limits (check its ChatGPT sign-in)");
        parse(&reply["result"])
    };
    let result = tokio::select! {
        _ = stop.cancelled() => Err(anyhow::anyhow!("Codex plan read cancelled")),
        r = tokio::time::timeout(timeout, exchange) => r.unwrap_or_else(|_| Err(anyhow::anyhow!("Codex plan read timed out"))),
    };
    drop(stdin);
    drop(rx);
    // EOF normally stops app-server. Bound shutdown and kill its process
    // group too, so a hung child cannot retain either output pipe.
    let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
    if let Some(pid) = pid {
        let group = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGTERM);
        if tokio::time::timeout(Duration::from_millis(250), child.wait()).await.is_err() {
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            let _ = child.kill().await;
        }
    }
    // Descendants can keep pipes open even after the server exits.
    if tokio::time::timeout(Duration::from_millis(250), async {
        let _ = tokio::join!(&mut out, &mut err);
    })
    .await
    .is_err()
        && let Some(pid) = pid
    {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
    }
    out.abort();
    err.abort();
    result
}

pub fn spawn(worker: Arc<Worker>, stop: CancellationToken) {
    tokio::spawn(async move {
        let mut next = Instant::now();
        let mut backoff = POLL;
        let mut was_enabled = false;
        loop {
            if worker.link.is_connected() {
                let config = tokio::select! {
                    _ = stop.cancelled() => return,
                    c = worker.link.call("quotas.codex.config", json!({})) => c,
                };
                let enabled = config.as_ref().is_ok_and(|c| c["enabled"] == true);
                if enabled && (!was_enabled || Instant::now() >= next) {
                    let (binary, timeout, every) = {
                        let o = worker.overrides.lock().unwrap();
                        (
                            o.codex_bin.clone().unwrap_or_else(|| "codex".into()),
                            o.codex_timeout.unwrap_or(TIMEOUT),
                            o.codex_every.unwrap_or(POLL),
                        )
                    };
                    let report = match read(&binary, timeout, &stop).await {
                        Ok(r) => {
                            backoff = every;
                            r
                        }
                        Err(e) => {
                            backoff = (backoff * 2).min(Duration::from_secs(3600));
                            tracing::warn!("reading Codex plan limits: {e}");
                            CodexLimitsReport { observed_at: c6_core::now_ms(), error: e.to_string(), ..Default::default() }
                        }
                    };
                    if stop.is_cancelled() {
                        return;
                    }
                    tokio::select! {
                        _ = stop.cancelled() => return,
                        _ = worker.link.call("quotas.codex.report", serde_json::to_value(report).unwrap()) => {}
                    }
                    next = Instant::now() + backoff;
                }
                if config.is_ok() {
                    was_enabled = enabled;
                }
            }
            let delay = if worker.link.is_connected() {
                worker.overrides.lock().unwrap().codex_config_every.unwrap_or(CONFIG_EVERY)
            } else {
                Duration::from_secs(1)
            };
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    });
}
