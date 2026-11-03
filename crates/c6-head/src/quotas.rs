//! Plan limits per connection (docs/analytics.md, "Plan limits"): how much
//! of a Claude Code subscription's or a ChatGPT plan's usage windows is
//! used, and when each starts afresh.
//!
//! - **Claude Code** reports them itself (`rate_limit_event` in its
//!   stream), so they arrive with its sessions' turns, for free.
//! - **ChatGPT plans** are read from the usage endpoint Codex reads for its
//!   `/status`, every few minutes and soon after a session on one ends a
//!   turn.
//!
//! Kept in memory, and the last seen in a setting (`quotas_last`), so a
//! restarted head shows them until fresh ones come: Claude Code reports
//! only now and then, not every turn.
//! A notification (event `quota`) goes out when a window passes 80% and
//! 95%, and when it is used up, once per window.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::State;
use axum::{Json, Router, routing::get};
use c6_api as api;
use c6_core::now_ms;
use serde_json::Value;
use tracing::warn;

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
/// Warn when a window passes these.
const THRESHOLDS: [f64; 2] = [80.0, 95.0];

/// The head's view of every connection's plan limits.
#[derive(Default)]
pub struct Quotas {
    by_connection: Mutex<HashMap<String, api::ConnectionQuota>>,
    /// What was already said: connection, window, the window's reset and
    /// the threshold (100 for used up).
    told: Mutex<HashSet<(String, String, i64, u32)>>,
    /// A ChatGPT read is wanted soon (a turn ended on it).
    wake: tokio::sync::Notify,
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
            api::QuotaWindow { name, used_percent: used.clamp(0.0, 100.0), resets_at, status: status.into() }
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
}

/// Keeps what's known now for the next start.
async fn keep(st: &HeadState) {
    let Ok(json) = serde_json::to_string(&st.quotas.all()) else { return };
    if let Err(e) = st.store.set_setting(KEPT, &json).await {
        warn!("keeping plan limits: {e:#}");
    }
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
    };
    record(st, q, &c.name);
    keep(st).await;
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

/// Reads one ChatGPT plan's windows with the head's own sign-in.
async fn read_chatgpt(st: &HeadState, c: &crate::store::ConnectionRow, url: &str) -> anyhow::Result<Vec<api::QuotaWindow>> {
    let tokens = crate::sessions::chatgpt_tokens(st, &c.id).await?;
    let mut req = st.http.get(url).bearer_auth(&tokens.access_token).header("accept", "application/json");
    if !tokens.workspace.is_empty() {
        req = req.header("chatgpt-account-id", &tokens.workspace);
    }
    let r = req.send().await?;
    let status = r.status();
    let body: Value = r.json().await.unwrap_or(Value::Null);
    anyhow::ensure!(status.is_success(), "the usage endpoint answered {status}");
    let windows = parse_chatgpt(&body, now_ms());
    anyhow::ensure!(!windows.is_empty(), "the usage endpoint named no limits");
    Ok(windows)
}

/// Reads every ChatGPT plan's windows now.
pub async fn poll_chatgpt(st: &HeadState) {
    let Ok(connections) = st.store.connections().await else { return };
    let url = usage_url(st).await;
    for c in connections.iter().filter(|c| c.kind == "chatgpt") {
        let q = match read_chatgpt(st, c, &url).await {
            Ok(windows) => api::ConnectionQuota {
                connection_id: c.id.clone(),
                source: "chatgpt".into(),
                windows,
                updated_at: now_ms(),
                error: String::new(),
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
        keep(st).await;
    }
}

/// Reads ChatGPT plans every few minutes, and soon after a turn on one.
pub fn spawn(st: std::sync::Arc<HeadState>) {
    tokio::spawn(async move {
        restore(&st).await;
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

    #[test]
    fn claude_codes_report_as_it_comes() {
        // As Claude Code 2.1.314 wrote it.
        let info = json!({"status":"allowed","resetsAt":1793750400,"rateLimitType":"seven_day","overageStatus":"rejected",
            "overageDisabledReason":"org_level_disabled","isUsingOverage":false,
            "unifiedWindows":{"seven_day":{"utilization":0.5,"resetsAt":1793750400,"status":"allowed"}}});
        assert_eq!(
            parse_claude_code(&info),
            [api::QuotaWindow { name: "week".into(), used_percent: 50.0, resets_at: Some(1_793_750_400_000), status: "allowed".into() }]
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
                api::QuotaWindow { name: "5h".into(), used_percent: 34.0, resets_at: Some(1_000 + 7_800_000), status: String::new() },
                api::QuotaWindow { name: "week".into(), used_percent: 71.5, resets_at: Some(1_793_750_400_000), status: String::new() },
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
}
