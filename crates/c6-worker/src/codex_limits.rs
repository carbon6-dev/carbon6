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
        ..Default::default()
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

/// Runs only initialize, initialized, account/rateLimits/read and account/read. Inherits
/// the environment except that an absolute executable's own folder comes
/// first on the child's PATH (e.g. nvm's node beside codex). Output never
/// enters diagnostics; the worker's environment is never modified.
pub async fn read(binary: &Path, timeout: Duration, stop: &CancellationToken) -> anyhow::Result<CodexLimitsReport> {
    let mut child = command(binary)?
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("Codex not found: set its path")
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
        let mut report = parse(&reply["result"])?;
        // Codex app-server-protocol GetAccountParams/GetAccountResponse,
        // pinned at 9fcaeb733522adf09b4e1c594bc90f344374a926. Read only:
        // do not proactively refresh auth, start login, or request a turn.
        stdin.write_all(b"{\"id\":4,\"method\":\"account/read\",\"params\":{\"refreshToken\":false}}\n").await?;
        let account = response(&mut rx, 4).await?;
        if account["error"].is_null() && account["result"]["account"]["type"] == "chatgpt" {
            let account = &account["result"]["account"];
            report.email = account["email"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_owned);
            if report.plan_type.is_none() {
                report.plan_type = account["planType"].as_str().map(str::to_owned);
            }
        }
        // An unsupported account/read still permits the stronger id match.
        Ok(report)
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
        let mut previous_command = None;
        let mut discovered: Option<std::path::PathBuf> = None;
        loop {
            if worker.link.is_connected() {
                let config = tokio::select! {
                    _ = stop.cancelled() => return,
                    c = worker.link.call("quotas.codex.config", json!({})) => c,
                };
                let enabled = config.as_ref().is_ok_and(|c| c["enabled"] == true);
                let command =
                    config.as_ref().ok().and_then(|c| c["command"].as_str()).filter(|c| !c.trim().is_empty()).map(std::path::PathBuf::from);
                let command_changed = command != previous_command;
                if config.is_ok() && (command_changed || enabled != was_enabled) {
                    discovered = None;
                }
                if enabled && (!was_enabled || command_changed || Instant::now() >= next) {
                    if !was_enabled || command_changed {
                        backoff = POLL;
                    }
                    let (binary, shell, shell_timeout, timeout, every) = {
                        let o = worker.overrides.lock().unwrap();
                        (
                            o.codex_bin.clone().or_else(|| command.clone()),
                            std::env::var_os("SHELL"),
                            o.codex_shell_timeout.unwrap_or(Duration::from_secs(5)),
                            o.codex_timeout.unwrap_or(TIMEOUT),
                            o.codex_every.unwrap_or(POLL),
                        )
                    };
                    let command_from_shell = binary.is_none();
                    let binary = match binary {
                        Some(binary) => Ok(binary),
                        None => match &discovered {
                            Some(binary) => Ok(binary.clone()),
                            None => {
                                let result = discovery::from_shell(shell.as_deref(), shell_timeout, &stop).await;
                                discovered = result.as_ref().ok().cloned();
                                result
                            }
                        },
                    };
                    let selected = binary.as_ref().ok().map(|p| p.to_string_lossy().into_owned());
                    let result = match binary {
                        Ok(binary) => read(&binary, timeout, &stop).await,
                        Err(e) => Err(e),
                    };
                    let mut report = match result {
                        Ok(r) => {
                            backoff = every;
                            r
                        }
                        Err(e) => {
                            discovered = None;
                            backoff = (backoff * 2).min(Duration::from_secs(3600));
                            tracing::warn!("reading Codex plan limits: {e}");
                            CodexLimitsReport { observed_at: c6_core::now_ms(), error: e.to_string(), ..Default::default() }
                        }
                    };
                    report.command = selected;
                    report.command_from_shell = command_from_shell;
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
                    previous_command = command;
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

fn command(binary: &Path) -> anyhow::Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(binary);
    if binary.is_absolute()
        && let Some(folder) = binary.parent()
    {
        let inherited = std::env::var_os("PATH");
        let folders = std::iter::once(folder.to_owned()).chain(inherited.iter().flat_map(std::env::split_paths));
        command.env("PATH", std::env::join_paths(folders).context("could not build Codex command's PATH")?);
    }
    Ok(command)
}

mod discovery;
