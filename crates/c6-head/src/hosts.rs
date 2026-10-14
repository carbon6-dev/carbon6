//! The host inventory: machines reached over SSH through a worker, with
//! labels. Sessions on a host get its labels; templates target hosts by
//! label; scheduled checks tell you when one stops answering.

use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use sqlx::Row;
use tracing::warn;

use crate::{HeadState, bad_request, not_found};

pub const HOSTS: &str = "hosts";

/// Hosts being checked, so a slow check is not started twice.
static CHECKING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);

pub async fn hosts(st: &HeadState) -> anyhow::Result<Vec<api::Host>> {
    Ok(serde_json::from_str(&st.store.setting(HOSTS).await?.unwrap_or_default()).unwrap_or_default())
}

pub fn check_hosts(hosts: &[api::Host]) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for h in hosts {
        let n = h.name.trim();
        if n.is_empty() || !n.chars().all(|c| c.is_alphanumeric() || "_-.".contains(c)) {
            return Err(bad_request(format!("host name {n:?} is not valid (letters, digits, - _ .)")));
        }
        if !names.insert(n) {
            return Err(bad_request(format!("two hosts are named {n:?}")));
        }
        let t = h.target.trim();
        if t.is_empty() || t.starts_with('-') || t.contains(char::is_whitespace) {
            return Err(bad_request(format!("host {n:?}: the target is user@host[:port] or an ssh_config alias")));
        }
        if h.check_every.is_some_and(|m| !m.is_finite() || m < 0.0) {
            return Err(bad_request(format!("host {n:?}: check every is a number of minutes")));
        }
        crate::sessions::check_labels(&h.labels)?;
    }
    Ok(())
}

/// The labels templates select hosts by: the host's own and `host:<name>`.
pub fn selector_labels(h: &api::Host) -> Vec<String> {
    let mut l = h.labels.clone();
    l.push(format!("host:{}", h.name));
    l
}

/// Hosts matching a template's selector (`*`: every one), enabled ones.
pub fn select<'a>(hosts: &'a [api::Host], selector: &[String]) -> Vec<&'a api::Host> {
    let all = selector.iter().any(|s| s.trim() == "*");
    hosts.iter().filter(|h| h.enabled && (all || api::labels::applies(selector, &selector_labels(h)))).collect()
}

/// The inventory entry for an SSH target given as a host's name or target.
pub async fn lookup(st: &HeadState, target: &str) -> anyhow::Result<Option<api::Host>> {
    let target = target.trim();
    Ok(hosts(st).await?.into_iter().find(|h| h.enabled && (h.name == target || h.target == target)))
}

/// The worker that reaches a host: its own (if online), else the first
/// online one.
pub fn worker_for<'a>(h: &api::Host, workers: &'a [api::Worker]) -> Option<&'a api::Worker> {
    let online = |w: &&api::Worker| w.status == api::WorkerStatus::Online;
    if h.worker.trim().is_empty() { workers.iter().find(online) } else { workers.iter().filter(online).find(|w| w.name == h.worker.trim()) }
}

pub async fn list(st: &HeadState) -> anyhow::Result<Vec<api::HostStatus>> {
    let rows = sqlx::query("SELECT * FROM host_checks").fetch_all(&st.store.pool).await?;
    let check = |name: &str| {
        rows.iter().find(|r| r.get::<String, _>("name") == name).map(|r| api::HostCheck {
            at_ms: r.get("at"),
            ok: r.get::<i64, _>("ok") != 0,
            worker: r.get("worker"),
            message: r.get("message"),
            latency_ms: r.get::<i64, _>("latency_ms") as u64,
        })
    };
    Ok(hosts(st).await?.into_iter().map(|h| api::HostStatus { check: check(&h.name), host: h }).collect())
}

/// Checks a host now (through its worker) and records the result.
pub async fn check_now(st: &HeadState, name: &str) -> anyhow::Result<api::HostCheck> {
    let host = hosts(st).await?.into_iter().find(|h| h.name == name).ok_or_else(|| not_found("no such host"))?;
    let workers = crate::api::workers_snapshot(st).await?;
    let check = match worker_for(&host, &workers) {
        None => api::HostCheck {
            at_ms: c6_core::now_ms(),
            ok: false,
            worker: host.worker.clone(),
            message: if host.worker.is_empty() { "no worker is online".into() } else { format!("worker {} is not online", host.worker) },
            latency_ms: 0,
        },
        Some(w) => {
            let started = std::time::Instant::now();
            let r = st
                .registry
                .call(c6_core::WorkerId(w.id), "ssh.check", serde_json::json!({"target": host.target}), Duration::from_secs(40))
                .await;
            let latency_ms = started.elapsed().as_millis() as u64;
            let (ok, message) = match r {
                Ok(v) if v["ok"].as_bool() == Some(true) => (true, v["system"].as_str().unwrap_or_default().to_owned()),
                Ok(v) => (false, v["error"].as_str().unwrap_or("unreachable").to_owned()),
                Err(e) => (false, e),
            };
            api::HostCheck { at_ms: c6_core::now_ms(), ok, worker: w.name.clone(), message, latency_ms }
        }
    };
    let before: Option<bool> = sqlx::query_scalar("SELECT ok FROM host_checks WHERE name = ?")
        .bind(name)
        .fetch_optional(&st.store.pool)
        .await?
        .map(|ok: i64| ok != 0);
    sqlx::query(
        "INSERT INTO host_checks (name, at, ok, worker, message, latency_ms) VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(name) DO UPDATE SET at = excluded.at, ok = excluded.ok, worker = excluded.worker, message = excluded.message, \
         latency_ms = excluded.latency_ms",
    )
    .bind(name)
    .bind(check.at_ms)
    .bind(check.ok as i64)
    .bind(&check.worker)
    .bind(&check.message)
    .bind(check.latency_ms as i64)
    .execute(&st.store.pool)
    .await?;
    // Its history, as series (docs/os.md §4.4).
    let dims = serde_json::Map::from_iter([("host".to_owned(), serde_json::json!(name))]);
    for (series, value) in [("host_up", if check.ok { 1.0 } else { 0.0 }), ("host_check_ms", check.latency_ms as f64)] {
        let p = api::Point { ts: check.at_ms, value, dims: dims.clone() };
        if let Err(e) = crate::series::record(st, crate::realms::HOME, series, &p, "host check").await {
            warn!(host = %name, "recording a host check: {e:#}");
        }
    }
    // Tell you when a host goes down (or its first check fails) and when it
    // comes back.
    if before != Some(check.ok) && !(before.is_none() && check.ok) {
        st.notifier.send(crate::notify::Note {
            event: "host",
            title: if check.ok { format!("{name} is reachable again") } else { format!("{name} is unreachable") },
            message: format!("{} (through {})", check.message, if check.worker.is_empty() { "no worker" } else { &check.worker }),
            priority: if check.ok { "normal" } else { "high" },
            labels: selector_labels(&host),
            ..Default::default()
        });
    }
    Ok(check)
}

/// Checks hosts with `check_every` when due.
pub fn spawn_checks(st: Arc<HeadState>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            let statuses = match list(&st).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("loading hosts: {e:#}");
                    continue;
                }
            };
            let now = c6_core::now_ms();
            for s in statuses {
                let Some(every) = s.host.check_every.filter(|m| *m > 0.0 && s.host.enabled) else { continue };
                let due = s.check.as_ref().is_none_or(|c| now - c.at_ms >= (every * 60_000.0) as i64);
                if due && CHECKING.lock().unwrap().insert(s.host.name.clone()) {
                    let st = st.clone();
                    tokio::spawn(async move {
                        if let Err(e) = check_now(&st, &s.host.name).await {
                            warn!(host = %s.host.name, "checking host: {e:#}");
                        }
                        CHECKING.lock().unwrap().remove(&s.host.name);
                    });
                }
            }
        }
    });
}
