//! Plan limits per connection (docs/analytics.md, "Plan limits"): how much
//! of a Claude Code subscription's or a ChatGPT plan's usage windows is
//! used, and when each starts afresh.
//!
//! - **Claude Code** reports them itself (`rate_limit_event` in its
//!   stream), so they arrive with its sessions' turns, for free.
//! - **ChatGPT plans** come from `x-codex` rate-limit headers on model
//!   replies to the head (including pool members). Workers do not yet
//!   forward these headers. The usage endpoint is a fallback until headers
//!   arrive or it rejects this connection's OAuth app; that refusal is
//!   logged once, not shown as an error on header data.
//!   The first header snapshot per connection logs its window names at
//!   INFO; c6-llm separately diagnoses a missing snapshot once per process
//!   with filtered header names (never values) and whether a hook is set.
//!
//! Kept in memory, and the last seen in a setting (`quotas_last`), so a
//! restarted head shows them until fresh ones come: Claude Code reports
//! only now and then, not every turn. Saves are coalesced to at most one a
//! minute, including a trailing save if replies keep arriving.
//! A notification (event `quota`) goes out when a window passes 80% and
//! 95%, and when it is used up, once per window.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::{Json, Router, routing::get};
use c6_api as api;
use c6_core::now_ms;
use serde_json::Value;
use tracing::{info, warn};

use crate::HeadState;
use crate::api::ApiResult;

/// A head setting: the last windows seen, kept across restarts.
const KEPT: &str = "quotas_last";
/// A head setting: the ChatGPT usage endpoint, for a stand-in or a proxy.
pub const CHATGPT_USAGE_URL: &str = "chatgpt_usage_url";
/// Where Codex reads a ChatGPT plan's usage windows.
const CHATGPT_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
/// How often ChatGPT plans are read.
const POLL: Duration = Duration::from_secs(180);
/// A read this recent is not repeated when a turn ends.
const FRESH_MS: i64 = 60_000;
const KEEP_EVERY: Duration = Duration::from_secs(60);
/// Warn when a window passes these.
const THRESHOLDS: [f64; 2] = [80.0, 95.0];
/// A header snapshot's first successful recording, per connection per process.
static CHATGPT_HEADERS_LOGGED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// The head's view of every connection's plan limits.
#[derive(Default)]
pub struct Quotas {
    by_connection: Mutex<HashMap<String, api::ConnectionQuota>>,
    /// What was already said: connection, window, the window's reset and
    /// the threshold (100 for used up).
    told: Mutex<HashSet<(String, String, i64, u32)>>,
    /// Headers have superseded polling, or the endpoint refused this app.
    endpoint_disabled: Mutex<HashSet<String>>,
    dirty: AtomicBool,
    last_kept: tokio::sync::Mutex<Option<Instant>>,
    /// A ChatGPT read is wanted soon (a turn ended on it).
    wake: tokio::sync::Notify,
    /// Serializes partial ChatGPT snapshots from headers and worker calls.
    updates: tokio::sync::Mutex<()>,
}

impl Quotas {
    pub fn all(&self) -> Vec<api::ConnectionQuota> {
        let mut v: Vec<_> = self.by_connection.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
        v
    }

    pub fn get(&self, connection: &str) -> Option<api::ConnectionQuota> {
        self.by_connection.lock().unwrap().get(connection).cloned()
    }
}

/// Claude Code's `rate_limit_info`: the windows it names (`unifiedWindows`,
/// else the one window the event is about). Utilization comes as a share
/// (0 to 1); a value above 1 is taken as a percentage already.
pub fn parse_claude_code(info: &Value) -> Vec<api::QuotaWindow> {
    let pct = |u: &Value| u.as_f64().map(|u| if u <= 1.0 { u * 100.0 } else { u }).unwrap_or(0.0);
    let at = |v: &Value| v.as_i64().filter(|s| *s > 0).map(|s| s * 1000);
    let mut out = Vec::new();
    if let Some(ws) = info["unifiedWindows"].as_object() {
        for (key, w) in ws {
            out.push(api::QuotaWindow {
                name: window_name(key),
                used_percent: pct(&w["utilization"]),
                resets_at: at(&w["resetsAt"]),
                status: status_of(w["status"].as_str().unwrap_or("")),
                ..Default::default()
            });
        }
    }
    if out.is_empty()
        && let Some(kind) = info["rateLimitType"].as_str()
    {
        out.push(api::QuotaWindow {
            name: window_name(kind),
            used_percent: pct(&info["utilization"]),
            resets_at: at(&info["resetsAt"]),
            status: status_of(info["status"].as_str().unwrap_or("")),
            ..Default::default()
        });
    }
    // The one the event is about may be rejected while its window says
    // nothing of it.
    if info["status"] == "rejected"
        && let Some(kind) = info["rateLimitType"].as_str()
        && let Some(w) = out.iter_mut().find(|w| w.name == window_name(kind))
    {
        w.status = "rejected".into();
    }
    out.sort_by_key(|w| order(&w.name));
    out
}

/// ChatGPT's usage: `primary_window` (the short one) and `secondary_window`
/// (the week), wherever they are in the answer (`rate_limit`, or the top
/// level), each with `used_percent`, `limit_window_seconds` and
/// `reset_after_seconds` or `resets_at` (seconds since 1970).
pub fn parse_chatgpt(body: &Value, now: i64) -> Vec<api::QuotaWindow> {
    let mut found: Vec<(&str, &Value)> = Vec::new();
    fn walk<'a>(v: &'a Value, found: &mut Vec<(&'a str, &'a Value)>) {
        if let Some(o) = v.as_object() {
            for (k, x) in o {
                if (k == "primary_window" || k == "secondary_window") && x.is_object() {
                    found.push((k.as_str(), x));
                } else {
                    walk(x, found);
                }
            }
        }
    }
    walk(body, &mut found);
    let limited = body["rate_limit"]["limit_reached"].as_bool() == Some(true) || body["limit_reached"].as_bool() == Some(true);
    let mut out: Vec<api::QuotaWindow> = found
        .into_iter()
        .map(|(key, w)| {
            let secs = w["limit_window_seconds"].as_i64().unwrap_or(0);
            let name = match secs {
                0 if key == "primary_window" => "5h".to_owned(),
                0 => "week".to_owned(),
                s => span_name(s),
            };
            let resets_at = w["resets_at"]
                .as_i64()
                .filter(|s| *s > 0)
                .map(|s| s * 1000)
                .or_else(|| w["reset_after_seconds"].as_i64().map(|s| now + s.max(0) * 1000));
            let used = w["used_percent"].as_f64().unwrap_or(0.0);
            let status = if used >= 100.0 || (limited && used >= 99.0) { "rejected" } else { "" };
            api::QuotaWindow { name, used_percent: used.clamp(0.0, 100.0), resets_at, status: status.into(), ..Default::default() }
        })
        .collect();
    out.sort_by_key(|w| order(&w.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn status_of(s: &str) -> String {
    match s {
        "allowed" => "allowed",
        "allowed_warning" => "warning",
        "rejected" => "rejected",
        _ => "",
    }
    .into()
}

fn window_name(kind: &str) -> String {
    match kind {
        "five_hour" => "5h".into(),
        "seven_day" => "week".into(),
        "seven_day_opus" => "week (Opus)".into(),
        "seven_day_sonnet" => "week (Sonnet)".into(),
        "seven_day_overage_included" => "week (with overage)".into(),
        "overage" => "overage".into(),
        other => other.replace('_', " "),
    }
}

/// A window of `secs` seconds by its length: "5h", "day", "week".
fn span_name(secs: i64) -> String {
    match secs {
        604_800 => "week".into(),
        86_400 => "day".into(),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s => format!("{}m", s / 60),
    }
}

/// Shorter windows first.
fn order(name: &str) -> (u8, String) {
    let rank = match name {
        n if n.ends_with('h') || n.ends_with('m') => 0,
        "day" => 1,
        "week" => 2,
        n if n.starts_with("week") => 3,
        _ => 4,
    };
    (rank, name.to_owned())
}

/// Keeps a connection's windows and says what passed a threshold.
fn record(st: &HeadState, q: api::ConnectionQuota, name: &str) {
    let mut notes = Vec::new();
    {
        let mut told = st.quotas.told.lock().unwrap();
        for w in &q.windows {
            let reset = w.resets_at.unwrap_or(0);
            let reached = if w.status == "rejected" || w.used_percent >= 100.0 {
                Some(100)
            } else {
                THRESHOLDS.iter().rev().find(|t| w.used_percent >= **t).map(|t| *t as u32)
            };
            let Some(level) = reached else { continue };
            // Once per window (its reset) and level; a higher level says
            // the lower ones too.
            if !told.insert((q.connection_id.clone(), w.name.clone(), reset, level)) {
                continue;
            }
            for lower in [80, 95].into_iter().filter(|l| *l < level) {
                told.insert((q.connection_id.clone(), w.name.clone(), reset, lower));
            }
            notes.push((w.clone(), level));
        }
    }
    for (w, level) in notes {
        let when = w.resets_at.map(|t| format!(" It starts afresh {}.", in_words(t - now_ms()))).unwrap_or_default();
        let (title, priority) = if level == 100 {
            (format!("{name}: its {} limit is used up", w.name), "high")
        } else {
            (format!("{name}: {:.0}% of its {} limit used", w.used_percent, w.name), "normal")
        };
        st.notifier.send(crate::notify::Note {
            event: "quota",
            title,
            message: format!("Sessions on {name} share this plan's {} window.{when}", w.name),
            priority,
            path: Some("/admin/settings/connections".into()),
            ..Default::default()
        });
    }
    st.quotas.by_connection.lock().unwrap().insert(q.connection_id.clone(), q);
    st.quotas.dirty.store(true, Ordering::Relaxed);
}

/// Keeps what's known now for the next start.
async fn keep(st: &HeadState) {
    // Serialize writers, including concurrent model replies, and keep a
    // dirty snapshot for the periodic trailing save rather than losing it.
    let mut last = st.quotas.last_kept.lock().await;
    if last.is_some_and(|at| at.elapsed() < KEEP_EVERY) || !st.quotas.dirty.swap(false, Ordering::Relaxed) {
        return;
    }
    let Ok(json) = serde_json::to_string(&st.quotas.all()) else { return };
    if let Err(e) = st.store.set_setting(KEPT, &json).await {
        warn!("keeping plan limits: {e:#}");
        st.quotas.dirty.store(true, Ordering::Relaxed);
    }
    *last = Some(Instant::now());
}

/// What was known before a restart, under anything already reported since.
async fn restore(st: &HeadState) {
    let Ok(Some(json)) = st.store.setting(KEPT).await else { return };
    let Ok(kept) = serde_json::from_str::<Vec<api::ConnectionQuota>>(&json) else { return };
    let mut by = st.quotas.by_connection.lock().unwrap();
    for q in kept {
        by.entry(q.connection_id.clone()).or_insert(q);
    }
}

/// "in 2h 10m", "in 3 days".
fn in_words(ms: i64) -> String {
    let m = (ms / 60_000).max(0);
    match m {
        0 => "now".into(),
        m if m < 60 => format!("in {m}m"),
        m if m < 48 * 60 => format!("in {}h {}m", m / 60, m % 60),
        m => format!("in {} days", m / (24 * 60)),
    }
}

/// A Claude Code session's report: its connection's windows.
pub async fn from_claude_code(st: &HeadState, session_id: &str, json: &str) {
    let Ok(info) = serde_json::from_str::<Value>(json) else { return };
    let windows = parse_claude_code(&info);
    if windows.is_empty() {
        return;
    }
    let Ok(Some(s)) = st.store.session(session_id).await else { return };
    let Ok(Some(c)) = st.store.connection(&s.connection_id).await else { return };
    if c.kind != "claude_code" {
        return;
    }
    // Merge: a report about one window keeps the others it didn't name.
    let mut merged = st.quotas.get(&c.id).filter(|q| q.source == "claude_code").map(|q| q.windows).unwrap_or_default();
    for w in windows {
        match merged.iter_mut().find(|x| x.name == w.name) {
            Some(x) => *x = w,
            None => merged.push(w),
        }
    }
    merged.sort_by_key(|w| order(&w.name));
    let q = api::ConnectionQuota {
        connection_id: c.id.clone(),
        source: "claude_code".into(),
        windows: merged,
        updated_at: now_ms(),
        error: String::new(),
        ..Default::default()
    };
    record(st, q, &c.name);
    keep(st).await;
}

/// Installed on head clients before their wrappers. A weak head avoids a
/// cycle through its pools; recording is awaited before yielding the stream.
pub(crate) fn chatgpt_hook(st: &HeadState, connection: &str) -> Option<c6_llm::openai_responses::ChatGptLimitsHook> {
    let head = std::sync::Arc::downgrade(&st.arc()?);
    let connection = connection.to_owned();
    Some(std::sync::Arc::new(move |limits| {
        let (head, connection) = (head.clone(), connection.clone());
        Box::pin(async move {
            if let Some(st) = head.upgrade() {
                from_chatgpt(&st, &connection, limits).await;
            }
        })
    }))
}

/// ChatGPT headers use minutes for lengths and Unix seconds for resets.
/// Partial snapshots keep windows the response didn't report.
async fn from_chatgpt(st: &HeadState, connection: &str, limits: c6_llm::openai_responses::ChatGptLimits) {
    let Ok(Some(c)) = st.store.connection(connection).await else { return };
    if c.kind != "chatgpt" || (limits.primary.is_none() && limits.secondary.is_none()) {
        return;
    }
    let origin =
        api::QuotaOrigin { source: "chatgpt".into(), observed_at: now_ms(), account_id: chatgpt_account(st, &c), ..Default::default() };
    let _guard = st.quotas.updates.lock().await;
    record_chatgpt(st, &c.id, &c.name, limits, origin, true);
    if CHATGPT_HEADERS_LOGGED.lock().unwrap().insert(c.id) {
        let names =
            st.quotas.get(connection).map(|q| q.windows.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        info!("ChatGPT plan limits from reply headers: {names}");
    }
    drop(_guard);
    keep(st).await;
}

fn record_chatgpt(
    st: &HeadState,
    connection: &str,
    name: &str,
    limits: c6_llm::openai_responses::ChatGptLimits,
    origin: api::QuotaOrigin,
    matched: bool,
) {
    if matched {
        st.quotas.endpoint_disabled.lock().unwrap().insert(connection.to_owned());
    }
    let previous = st.quotas.get(connection).filter(|q| {
        !q.origin.as_ref().is_some_and(|o| o.account_id.is_some() && origin.account_id.is_some() && o.account_id != origin.account_id)
    });
    let previous_at = previous.as_ref().map(|q| q.updated_at).unwrap_or(0);
    let mut windows = previous.as_ref().map(|q| q.windows.clone()).unwrap_or_default();
    for (window, fallback) in [(limits.primary, "5h"), (limits.secondary, "week")] {
        let Some(w) = window else { continue };
        let name = w.window_minutes.and_then(|m| m.checked_mul(60)).map(span_name).unwrap_or_else(|| fallback.into());
        let used_percent = w.used_percent.clamp(0.0, 100.0);
        let window = api::QuotaWindow {
            name,
            used_percent,
            resets_at: w.resets_at.and_then(|s| s.checked_mul(1000)),
            status: if used_percent >= 100.0 { "rejected".into() } else { String::new() },
            origin: Some(origin.clone()),
        };
        match windows.iter_mut().find(|x| x.name == window.name) {
            Some(x) if x.origin.as_ref().map(|o| o.observed_at).unwrap_or(previous_at) <= origin.observed_at => *x = window,
            Some(_) => {}
            None => windows.push(window),
        }
    }
    windows.sort_by_key(|w| order(&w.name));
    let newest = windows.iter().filter_map(|w| w.origin.as_ref()).max_by_key(|o| o.observed_at).cloned().unwrap_or(origin);
    let q = api::ConnectionQuota {
        connection_id: connection.into(),
        source: newest.source.clone(),
        windows,
        updated_at: newest.observed_at.max(previous_at),
        error: String::new(),
        origin: Some(newest),
    };
    if matched {
        record(st, q, name);
    } else {
        // Unattributed data must not notify as though sessions use this plan.
        st.quotas.by_connection.lock().unwrap().insert(connection.into(), q);
        st.quotas.dirty.store(true, Ordering::Relaxed);
    }
}

pub const CODEX_SETTINGS: &str = "codex_limits";

pub async fn codex_settings(st: &HeadState) -> anyhow::Result<api::CodexLimitsSettings> {
    Ok(st.store.setting(CODEX_SETTINGS).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

async fn codex_enabled(st: &HeadState, from: c6_core::WorkerId) -> anyhow::Result<bool> {
    Ok(codex_settings(st).await?.workers.contains(&from.to_string()))
}

pub async fn codex_config(st: &HeadState, from: c6_core::WorkerId) -> anyhow::Result<Value> {
    Ok(serde_json::json!({"enabled": codex_enabled(st, from).await?}))
}

fn chatgpt_account(st: &HeadState, c: &crate::store::ConnectionRow) -> Option<String> {
    c.api_key
        .as_deref()
        .and_then(|blob| st.secrets.decrypt(blob).ok())
        .and_then(|s| serde_json::from_str::<crate::oauth::ChatGptTokens>(&s).ok())
        .map(|t| t.workspace)
        .filter(|s| !s.is_empty())
}

/// Only exact, nonempty verified workspace ids match. Never infer from an
/// email, plan type, worker name, or the existence of just one connection.
pub async fn from_codex(st: &HeadState, from: c6_core::WorkerId, report: api::CodexLimitsReport) -> anyhow::Result<()> {
    anyhow::ensure!(codex_enabled(st, from).await?, "Codex plan reads are not enabled for this worker");
    let worker = st.store.worker(from).await?.ok_or_else(|| crate::bad_request("worker not found"))?;
    anyhow::ensure!(report.windows.len() <= 2 && report.error.len() <= 256, "invalid Codex snapshot");
    anyhow::ensure!(
        report.account_id.as_ref().is_none_or(|s| s.len() <= 256) && report.plan_type.as_ref().is_none_or(|s| s.len() <= 80),
        "invalid Codex account metadata"
    );
    let origin = api::QuotaOrigin {
        source: "codex-cli".into(),
        observed_at: report.observed_at.clamp(0, now_ms()),
        worker_id: worker.id.clone(),
        worker_name: worker.name,
        account_id: report.account_id.filter(|s| !s.is_empty()),
        plan_type: report.plan_type,
    };
    let key = format!("codex-cli:{}", worker.id);
    let _guard = st.quotas.updates.lock().await;
    if !report.error.is_empty() {
        let mut q = st.quotas.get(&key).unwrap_or(api::ConnectionQuota {
            connection_id: key.clone(),
            source: "codex-cli".into(),
            origin: Some(origin),
            ..Default::default()
        });
        q.error = report.error;
        st.quotas.by_connection.lock().unwrap().insert(key, q);
        st.quotas.dirty.store(true, Ordering::Relaxed);
    } else {
        let mut limits = c6_llm::openai_responses::ChatGptLimits::default();
        for (i, w) in report.windows.into_iter().enumerate() {
            anyhow::ensure!(
                w.used_percent.is_finite()
                    && (0.0..=100.0).contains(&w.used_percent)
                    && w.window_minutes > 0
                    && w.window_minutes <= i64::MAX / 60,
                "invalid Codex window"
            );
            let w = c6_llm::openai_responses::ChatGptWindow {
                used_percent: w.used_percent,
                window_minutes: Some(w.window_minutes),
                resets_at: w.resets_at,
            };
            if i == 0 {
                limits.primary = Some(w);
            } else {
                limits.secondary = Some(w);
            }
        }
        anyhow::ensure!(limits.primary.is_some(), "Codex snapshot has no windows");
        let mut matches = Vec::new();
        if let Some(account) = &origin.account_id {
            for c in st.store.connections().await?.into_iter().filter(|c| c.kind == "chatgpt") {
                if chatgpt_account(st, &c).as_ref() == Some(account) {
                    matches.push(c);
                }
            }
        }
        if matches.is_empty() {
            // A different Codex login replaces the previous unmatched account,
            // rather than mixing its missing windows into the new account.
            if let Some(old) = st.quotas.get(&key).and_then(|q| q.origin)
                && old.account_id != origin.account_id
            {
                if old.observed_at > origin.observed_at {
                    return Ok(());
                }
                st.quotas.by_connection.lock().unwrap().remove(&key);
            }
            record_chatgpt(st, &key, "", limits, origin, false);
        } else {
            st.quotas.by_connection.lock().unwrap().remove(&key);
            for c in matches {
                record_chatgpt(st, &c.id, &c.name, limits.clone(), origin.clone(), true);
            }
        }
    }
    drop(_guard);
    keep(st).await;
    Ok(())
}

/// A turn ended on `connection`: a ChatGPT plan's windows are read soon.
pub async fn turn_ended(st: &HeadState, session_id: &str) {
    let Ok(Some(s)) = st.store.session(session_id).await else { return };
    let stale = st.quotas.get(&s.connection_id).is_none_or(|q| now_ms() - q.updated_at > FRESH_MS);
    if stale && st.store.connection(&s.connection_id).await.ok().flatten().is_some_and(|c| c.kind == "chatgpt") {
        st.quotas.wake.notify_one();
    }
}

async fn usage_url(st: &HeadState) -> String {
    st.store.setting(CHATGPT_USAGE_URL).await.ok().flatten().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| CHATGPT_USAGE.into())
}

/// Reads one ChatGPT plan's windows with the head's own sign-in. `None`
/// means the endpoint refuses this OAuth app, not that the plan is used up.
async fn read_chatgpt(st: &HeadState, c: &crate::store::ConnectionRow, url: &str) -> anyhow::Result<Option<Vec<api::QuotaWindow>>> {
    let tokens = crate::sessions::chatgpt_tokens(st, &c.id).await?;
    let mut req = st.http.get(url).bearer_auth(&tokens.access_token).header("accept", "application/json");
    if !tokens.workspace.is_empty() {
        req = req.header("chatgpt-account-id", &tokens.workspace);
    }
    let r = req.send().await?;
    let status = r.status();
    let body: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success()
        && (body["type"] == "rejected_by_access_enforcement" || body["error"]["type"] == "rejected_by_access_enforcement")
    {
        return Ok(None);
    }
    anyhow::ensure!(status.is_success(), "the usage endpoint answered {status}");
    let windows = parse_chatgpt(&body, now_ms());
    anyhow::ensure!(!windows.is_empty(), "the usage endpoint named no limits");
    Ok(Some(windows))
}

/// Reads every ChatGPT plan's windows now.
pub async fn poll_chatgpt(st: &HeadState) {
    let Ok(connections) = st.store.connections().await else { return };
    let url = usage_url(st).await;
    for c in connections.iter().filter(|c| c.kind == "chatgpt") {
        if st.quotas.endpoint_disabled.lock().unwrap().contains(&c.id) {
            continue;
        }
        let result = read_chatgpt(st, c, &url).await;
        {
            // Hold this through the update: a response hook must either
            // supersede the poll afterwards or prevent its write entirely.
            let mut disabled = st.quotas.endpoint_disabled.lock().unwrap();
            let q = match result {
                Ok(None) => {
                    if disabled.insert(c.id.clone()) {
                        info!(connection = %c.name, "ChatGPT's usage endpoint refused this OAuth app; using model response headers instead");
                    }
                    // An earlier failed poll must not poison the header path.
                    if let Some(mut q) = st.quotas.get(&c.id) {
                        q.error.clear();
                        st.quotas.by_connection.lock().unwrap().insert(c.id.clone(), q);
                    }
                    continue;
                }
                // A model reply can arrive while this request is in flight.
                _ if disabled.contains(&c.id) => continue,
                Ok(Some(windows)) => api::ConnectionQuota {
                    connection_id: c.id.clone(),
                    source: "chatgpt".into(),
                    windows,
                    updated_at: now_ms(),
                    error: String::new(),
                    ..Default::default()
                },
                Err(e) => {
                    warn!(connection = %c.name, "reading the plan's usage: {e:#}");
                    // The last good windows stay, marked with why they're old.
                    let mut q = st.quotas.get(&c.id).unwrap_or(api::ConnectionQuota {
                        connection_id: c.id.clone(),
                        source: "chatgpt".into(),
                        ..Default::default()
                    });
                    q.error = format!("{e:#}");
                    st.quotas.by_connection.lock().unwrap().insert(c.id.clone(), q);
                    continue;
                }
            };
            record(st, q, &c.name);
        }
        keep(st).await;
    }
}

/// Reads ChatGPT plans every few minutes, and soon after a turn on one.
pub fn spawn(st: std::sync::Arc<HeadState>) {
    tokio::spawn(async move {
        restore(&st).await;
        let keeper = st.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = keeper.shutdown.cancelled() => return,
                    _ = tokio::time::sleep(KEEP_EVERY) => keep(&keeper).await,
                }
            }
        });
        loop {
            poll_chatgpt(&st).await;
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = tokio::time::sleep(POLL) => {}
                _ = st.quotas.wake.notified() => {}
            }
        }
    });
}

pub fn routes() -> Router<std::sync::Arc<HeadState>> {
    Router::new().route("/api/v1/connections/quotas", get(list))
}

/// Every connection's plan limits, as last seen.
/// A realm's pages see the connections its sessions may use, as with the
/// connection list.
async fn list(
    State(st): State<std::sync::Arc<HeadState>>,
    axum::Extension(caller): axum::Extension<crate::realms::Caller>,
) -> ApiResult<Json<Vec<api::ConnectionQuota>>> {
    let mut all = st.quotas.all();
    if let Some(realm) = &caller.realm {
        let serving: HashSet<String> =
            st.store.connections().await?.into_iter().filter(|c| crate::realms::available(&c.realms, realm)).map(|c| c.id).collect();
        all.retain(|q| serving.contains(&q.connection_id));
    }
    Ok(Json(all))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn chatgpt_reply_headers_reach_quota_api_and_persist_without_poll_errors() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        use c6_llm::{ModelRequest, activity::Origin};
        use futures::StreamExt;

        let asked = Arc::new(AtomicUsize::new(0));
        let refusal = Arc::new(AtomicBool::new(false));
        let headers = Arc::new(Mutex::new(HeaderMap::from_iter([
            ("x-codex-primary-used-percent".parse().unwrap(), "34.5".parse().unwrap()),
            ("x-codex-primary-window-minutes".parse().unwrap(), "300".parse().unwrap()),
            ("x-codex-primary-reset-at".parse().unwrap(), "4102441200".parse().unwrap()),
            ("x-codex-secondary-used-percent".parse().unwrap(), "85".parse().unwrap()),
            ("x-codex-secondary-window-minutes".parse().unwrap(), "10080".parse().unwrap()),
            ("x-codex-secondary-reset-at".parse().unwrap(), "4102444800".parse().unwrap()),
        ])));
        let (a, r, h) = (asked.clone(), refusal.clone(), headers.clone());
        // The catalog source-scans this module's literal routes; this one
        // is the stand-in's, not a head API operation.
        let models_path = "/models";
        let app = Router::new()
            .route(
                "/usage",
                get(move || {
                    let (a, r) = (a.clone(), r.clone());
                    async move {
                        a.fetch_add(1, Ordering::Relaxed);
                        let body = if r.load(Ordering::Relaxed) {
                            json!({"type":"rejected_by_access_enforcement","code":"no_matching_rule"})
                        } else {
                            json!({"detail":"Unauthorized"})
                        };
                        (StatusCode::UNAUTHORIZED, Json(body))
                    }
                }),
            )
            .route(models_path, get(|| async { Json(json!({"data":[{"id":"test"}]})) }))
            .route(
                "/responses",
                axum::routing::post(move |req: HeaderMap| {
                    let h = h.clone();
                    async move {
                        assert_eq!(req["authorization"], "Bearer tok");
                        (h.lock().unwrap().clone(), "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n")
                            .into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stand = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let config = crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        };
        let head = crate::start(config.clone()).await.unwrap();
        let st = &head.state;
        st.store.set_setting(CHATGPT_USAGE_URL, &format!("{stand}/usage")).await.unwrap();
        let tokens = json!({"client_id":"c", "access_token":"tok", "refresh_token":"r", "expires_at_ms":i64::MAX / 2, "scope":"chatgpt.tokens.use.direct", "workspace":""});
        let row = crate::store::ConnectionRow {
            id: "gpt".into(),
            name: "ChatGPT".into(),
            kind: "chatgpt".into(),
            base_url: stand,
            api_key: Some(st.secrets.encrypt(&tokens.to_string()).unwrap()),
            default_model: None,
            context_window: None,
            created_at: 0,
            realms: "[]".into(),
            members: "[]".into(),
            subagent_model: None,
            compact_at: None,
        };
        st.store.insert_connection(&row).await.unwrap();
        let http = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}/api/v1/connections/quotas", head.ui_addrs[0].port());
        let quotas = async || -> Vec<api::ConnectionQuota> { http.get(&url).send().await.unwrap().json().await.unwrap() };

        poll_chatgpt(st).await;
        assert!(quotas().await[0].error.contains("401"));
        refusal.store(true, Ordering::Relaxed);
        poll_chatgpt(st).await;
        let reads = asked.load(Ordering::Relaxed);
        poll_chatgpt(st).await;
        assert_eq!(asked.load(Ordering::Relaxed), reads, "access enforcement stops further polling");
        assert!(quotas().await[0].error.is_empty());

        let request = ModelRequest {
            model: "test".into(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            max_output_tokens: None,
            cache_key: None,
            effort: None,
            web_search: false,
            json: false,
        };
        let client = crate::pools::client(st, "gpt", "home", Origin::default()).await.unwrap();
        let mut stream = client.stream(&request).await.unwrap();
        // Recording happens before the stream is consumed, even if a caller drops it.
        let q = quotas().await.remove(0);
        assert_eq!(q.source, "chatgpt");
        assert!(q.error.is_empty());
        assert!(st.quotas.told.lock().unwrap().contains(&("gpt".into(), "week".into(), 4_102_444_800_000, 80)));
        assert_eq!(
            q.windows.iter().map(|w| (w.name.as_str(), w.used_percent, w.resets_at)).collect::<Vec<_>>(),
            [("5h", 34.5, Some(4_102_441_200_000)), ("week", 85.0, Some(4_102_444_800_000))]
        );
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        let saved = st.store.setting(KEPT).await.unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Vec<api::ConnectionQuota>>(&saved).unwrap(), vec![q.clone()]);

        // A pool's reply belongs to its ChatGPT member. An omitted primary
        // window keeps the old one; usage is clamped and used-up is explicit.
        let pool = crate::store::ConnectionRow {
            id: "pool".into(),
            name: "Pool".into(),
            kind: "pool".into(),
            api_key: None,
            members: r#"[{"connection_id":"gpt","slots":1}]"#.into(),
            ..row.clone()
        };
        st.store.insert_connection(&pool).await.unwrap();
        {
            let mut h = headers.lock().unwrap();
            h.clear();
            h.insert("x-codex-secondary-used-percent", "101".parse().unwrap());
            h.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
            h.insert("x-codex-secondary-reset-at", "4102444800".parse().unwrap());
        }
        // Pin the save gate ahead, avoiding any assertion about machine speed.
        *st.quotas.last_kept.lock().await = Some(Instant::now() + Duration::from_secs(86400));
        let client = crate::pools::client(st, "pool", "home", Origin::default()).await.unwrap();
        let mut stream = client.stream(&request).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        let current = quotas().await;
        assert_eq!(current.len(), 1, "no limits attributed to the pool itself");
        assert_eq!(current[0].windows[0], q.windows[0]);
        assert_eq!((current[0].windows[1].used_percent, current[0].windows[1].status.as_str()), (100.0, "rejected"));
        assert!(st.quotas.told.lock().unwrap().contains(&("gpt".into(), "week".into(), 4_102_444_800_000, 100)));
        assert_eq!(st.store.setting(KEPT).await.unwrap().unwrap(), saved, "replies inside the save gate only update memory");
        assert!(st.quotas.dirty.load(Ordering::Relaxed));
        *st.quotas.last_kept.lock().await = None;
        keep(st).await; // The periodic trailing save uses this same path.
        assert!(!st.quotas.dirty.load(Ordering::Relaxed));
        assert_eq!(serde_json::from_str::<Vec<api::ConnectionQuota>>(&st.store.setting(KEPT).await.unwrap().unwrap()).unwrap(), current);

        poll_chatgpt(st).await;
        assert_eq!(asked.load(Ordering::Relaxed), reads);
        assert!(quotas().await[0].error.is_empty());
        // A non-plan Responses connection ignores identical headers.
        let api_row = crate::store::ConnectionRow {
            id: "api".into(),
            kind: "openai".into(),
            api_key: Some(st.secrets.encrypt("tok").unwrap()),
            ..row
        };
        st.store.insert_connection(&api_row).await.unwrap();
        let client = crate::pools::client(st, "api", "home", Origin::default()).await.unwrap();
        let mut stream = client.stream(&request).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        assert_eq!(quotas().await.len(), 1);
        head.shutdown().await.unwrap();

        let head = crate::start(config).await.unwrap();
        c6_testkit::eventually("restored ChatGPT windows", async || head.state.quotas.get("gpt")).await;
        assert_eq!(head.state.quotas.get("gpt").unwrap().windows, current[0].windows);
        head.shutdown().await.unwrap();
        server.abort();
    }

    #[test]
    fn claude_codes_report_as_it_comes() {
        // As Claude Code 2.1.314 wrote it.
        let info = json!({"status":"allowed","resetsAt":1793750400,"rateLimitType":"seven_day","overageStatus":"rejected",
            "overageDisabledReason":"org_level_disabled","isUsingOverage":false,
            "unifiedWindows":{"seven_day":{"utilization":0.5,"resetsAt":1793750400,"status":"allowed"}}});
        assert_eq!(
            parse_claude_code(&info),
            [api::QuotaWindow {
                name: "week".into(),
                used_percent: 50.0,
                resets_at: Some(1_793_750_400_000),
                status: "allowed".into(),
                ..Default::default()
            }]
        );
        // Both windows, the short one first; a warning; the one the event is
        // about rejected.
        let info = json!({"status":"rejected","rateLimitType":"five_hour","unifiedWindows":{
            "seven_day":{"utilization":0.81,"resetsAt":10,"status":"allowed_warning"},
            "five_hour":{"utilization":1.0,"resetsAt":5}}});
        let w = parse_claude_code(&info);
        assert_eq!(w.iter().map(|w| (w.name.as_str(), w.status.as_str())).collect::<Vec<_>>(), [("5h", "rejected"), ("week", "warning")]);
        assert_eq!(w[0].used_percent, 100.0);
        // No windows: the event's own.
        let w = parse_claude_code(&json!({"status":"allowed","rateLimitType":"seven_day_opus","utilization":12.5}));
        assert_eq!((w[0].name.as_str(), w[0].used_percent, w[0].resets_at), ("week (Opus)", 12.5, None));
        assert!(parse_claude_code(&json!({})).is_empty());
    }

    #[test]
    fn chatgpt_usage_wherever_its_windows_are() {
        let body = json!({"plan_type":"pro","rate_limit":{"allowed":true,"limit_reached":false,
            "primary_window":{"used_percent":34.0,"limit_window_seconds":18000,"reset_after_seconds":7800},
            "secondary_window":{"used_percent":71.5,"limit_window_seconds":604800,"resets_at":1793750400}},
            "credits":{"has_credits":false}});
        let w = parse_chatgpt(&body, 1_000);
        assert_eq!(
            w,
            [
                api::QuotaWindow {
                    name: "5h".into(),
                    used_percent: 34.0,
                    resets_at: Some(1_000 + 7_800_000),
                    status: String::new(),
                    ..Default::default()
                },
                api::QuotaWindow {
                    name: "week".into(),
                    used_percent: 71.5,
                    resets_at: Some(1_793_750_400_000),
                    status: String::new(),
                    ..Default::default()
                },
            ]
        );
        // At the top level, with fields missing; a used-up window.
        let w = parse_chatgpt(&json!({"limit_reached":true,"secondary_window":{"used_percent":100}}), 0);
        assert_eq!((w[0].name.as_str(), w[0].status.as_str(), w[0].resets_at), ("week", "rejected", None));
        assert!(parse_chatgpt(&json!({"detail":"Unauthorized"}), 0).is_empty());
    }

    #[test]
    fn windows_are_named_by_length() {
        assert_eq!(
            (span_name(18_000), span_name(604_800), span_name(86_400), span_name(1_800)),
            ("5h".into(), "week".into(), "day".into(), "30m".into())
        );
        assert_eq!(in_words(2 * 3_600_000 + 10 * 60_000), "in 2h 10m");
    }

    #[tokio::test]
    async fn chatgpt_sources_merge_newest_per_window_and_keep_provenance() {
        use c6_llm::openai_responses::{ChatGptLimits, ChatGptWindow};
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().into(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let window = |used, mins| Some(ChatGptWindow { used_percent: used, window_minutes: Some(mins), resets_at: Some(4_102_444_800) });
        let origin = |source: &str, at| api::QuotaOrigin {
            source: source.into(),
            observed_at: at,
            worker_name: "test-worker".into(),
            ..Default::default()
        };
        record_chatgpt(
            &head.state,
            "gpt",
            "GPT",
            ChatGptLimits { primary: window(10.0, 300), secondary: window(20.0, 10080) },
            origin("codex-cli", 100),
            true,
        );
        record_chatgpt(
            &head.state,
            "gpt",
            "GPT",
            ChatGptLimits { primary: window(30.0, 300), secondary: None },
            origin("chatgpt", 300),
            true,
        );
        record_chatgpt(
            &head.state,
            "gpt",
            "GPT",
            ChatGptLimits { primary: window(25.0, 300), secondary: window(40.0, 10080) },
            origin("codex-cli", 200),
            true,
        );
        let q = head.state.quotas.get("gpt").unwrap();
        assert_eq!(q.updated_at, 300);
        assert_eq!(q.source, "chatgpt");
        assert_eq!(
            q.windows.iter().map(|w| (w.name.as_str(), w.used_percent, w.origin.as_ref().unwrap().source.as_str())).collect::<Vec<_>>(),
            [("5h", 30.0, "chatgpt"), ("week", 40.0, "codex-cli")]
        );
        keep(&head.state).await;
        let kept = head.state.store.setting(KEPT).await.unwrap().unwrap();
        let restored: Vec<api::ConnectionQuota> = serde_json::from_str(&kept).unwrap();
        assert_eq!(restored, [q]);
        let old: api::ConnectionQuota = serde_json::from_str(
            r#"{"connection_id":"old","source":"chatgpt","windows":[{"name":"5h","used_percent":10}],"updated_at":1}"#,
        )
        .unwrap();
        assert!(old.origin.is_none() && old.windows[0].origin.is_none());
        head.shutdown().await.unwrap();
    }
}
