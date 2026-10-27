//! Triggers (docs/os.md §4.3; automations before): an event, an optional
//! filter on it, and an action.
//!
//! - **Events:** a schedule (cron, in the head's local time), a webhook
//!   call, a memex event, a record created, updated or deleted, a decision
//!   opened, answered or expired, a session's turn finished or failed, or a
//!   session needing approval.
//! - **Actions:** start a template or workflow (its sessions are labelled
//!   automation:<name> and run:<id>), write a record, ask (open a
//!   decision), or notify. Parameters take the event's values by JSONPath.
//! - **Loop protection:** an action carries the chain of triggers that led
//!   to it; a chain deeper than `MAX_DEPTH`, or a trigger firing more than
//!   its `per_hour`, stops and tells you.

use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use c6_core::now_ms;
use chrono::Local;
#[cfg(test)]
use chrono::TimeZone;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::cron::Schedule;
use crate::{HeadState, bad_request, not_found};

/// Webhook bodies beyond this are refused.
pub const MAX_WEBHOOK_BODY: usize = 1024 * 1024;

/// Triggers whose actions cause other triggers, at most this deep.
pub const MAX_DEPTH: usize = 5;

pub const EVENTS: [&str; 7] = ["schedule", "webhook", "memex", "record", "decision", "session", "series"];
pub const ACTIONS: [&str; 7] = ["start", "write", "ask", "notify", "record", "operation", "kit"];
const OPS: [(&str, &[&str]); 4] = [
    ("series", &["crossed", "cleared"]),
    ("record", &["created", "updated", "deleted"]),
    ("decision", &["opened", "answered", "expired", "dismissed"]),
    ("session", &["finished", "failed", "needs_approval"]),
];

tokio::task_local! {
    /// The triggers whose actions are running, outermost first.
    static CHAIN: Vec<String>;
}

fn json_col(r: &sqlx::sqlite::SqliteRow, col: &str) -> Value {
    serde_json::from_str(&r.get::<String, _>(col)).unwrap_or(json!({}))
}

fn row_to_api(r: &sqlx::sqlite::SqliteRow) -> api::Automation {
    let trigger: String = r.get("event");
    let cron: String = r.get("cron");
    let enabled = r.get::<i64, _>("enabled") != 0;
    let next_run_at = (trigger == "schedule" && enabled)
        .then(|| Schedule::parse(&cron).ok()?.next_after(Local::now()).map(|t| t.timestamp_millis()))
        .flatten();
    api::Automation {
        id: r.get("id"),
        name: r.get("name"),
        template: r.get("template"),
        trigger,
        cron,
        event_params: json_col(r, "event_params"),
        filter: json_col(r, "filter"),
        action: r.get("action"),
        action_params: json_col(r, "action_params"),
        per_hour: r.get::<i64, _>("per_hour").max(0) as u32,
        input: r.get("input"),
        enabled,
        created_at: r.get("created_at"),
        last_run_at: r.get("last_run_at"),
        last_run_id: r.get("last_run_id"),
        last_error: r.get("last_error"),
        next_run_at,
        webhook_url: None,
        disable_after: r.get::<i64, _>("disable_after").max(0) as u32,
        recent: Vec::new(),
        owner: r.try_get("owner").unwrap_or_default(),
    }
}

/// With how its latest runs went.
async fn with_recent(st: &HeadState, mut a: api::Automation) -> anyhow::Result<api::Automation> {
    a.recent = sqlx::query_scalar("SELECT outcome FROM trigger_runs WHERE trigger_id = ? ORDER BY started_at DESC LIMIT 10")
        .bind(&a.id)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(a)
}

/// The triggers the caller may see.
pub async fn list(st: &HeadState, caller: &crate::realms::Caller) -> anyhow::Result<Vec<api::Automation>> {
    let rows = sqlx::query("SELECT * FROM triggers WHERE (? IS NULL OR realm = ?) ORDER BY name")
        .bind(&caller.realm)
        .bind(&caller.realm)
        .fetch_all(&st.store.pool)
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        out.push(with_recent(st, row_to_api(r)).await?);
    }
    Ok(out)
}

async fn get(st: &HeadState, id: &str) -> anyhow::Result<api::Automation> {
    let row = sqlx::query("SELECT * FROM triggers WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?;
    with_recent(st, row.as_ref().map(row_to_api).ok_or_else(|| not_found("no such trigger"))?).await
}

async fn realm_of(st: &HeadState, id: &str) -> String {
    sqlx::query_scalar("SELECT realm FROM triggers WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| crate::realms::HOME.to_owned())
}

fn new_token() -> String {
    hex::encode(rand::random::<[u8; 24]>())
}

fn webhook_path(id: &str, token: &str) -> String {
    format!("/api/v1/webhooks/{id}/{token}")
}

async fn check(st: &HeadState, a: &api::Automation, realm: &str) -> anyhow::Result<()> {
    if a.name.trim().is_empty() {
        return Err(bad_request("a trigger needs a name"));
    }
    match a.trigger.as_str() {
        "schedule" => Schedule::parse(&a.cron).map(drop).map_err(bad_request)?,
        "webhook" => {}
        "memex" => EventFilter::parse(&a.cron).map(drop).map_err(bad_request)?,
        "series" => {
            let p = &a.event_params;
            let name = p["series"].as_str().unwrap_or_default();
            crate::series::get(st, realm, name).await.map_err(|_| bad_request(format!("no series {name:?} in realm {realm}")))?;
            let silent = p["silent_minutes"].as_f64().is_some_and(|m| m > 0.0);
            if !silent && (!matches!(p["op"].as_str(), Some(">" | ">=" | "<" | "<=")) || !p["value"].is_number()) {
                return Err(bad_request(
                    "a threshold is {series, agg (sum, avg, min, max, last, count), minutes, op (>, >=, <, <=), value}, or {series, silent_minutes}",
                ));
            }
        }
        "record" | "decision" | "session" => {
            let allowed = OPS.iter().find(|(k, _)| *k == a.trigger).map(|(_, o)| *o).unwrap_or_default();
            for op in a.event_params["ops"].as_array().into_iter().flatten() {
                if !op.as_str().is_some_and(|o| allowed.contains(&o)) {
                    return Err(bad_request(format!("{op} is not a {} event ({})", a.trigger, allowed.join(", "))));
                }
            }
            if a.trigger == "record" {
                let c = a.event_params["collection"].as_str().unwrap_or_default();
                crate::collections::get(st, realm, c).await.map_err(|_| bad_request(format!("no collection {c:?} in realm {realm}")))?;
            }
        }
        other => return Err(bad_request(format!("{other:?} is not an event ({})", EVENTS.join(", ")))),
    }
    match &a.filter {
        Value::Null => {}
        Value::Object(f) if f.keys().all(|k| k.starts_with('$')) => {}
        _ => return Err(bad_request("a filter is an object of JSONPath conditions: {\"$.record.fields.stage\": \"won\"}")),
    }
    let p = &a.action_params;
    match a.action.as_str() {
        "start" => {
            if !crate::templates::templates_for(st, realm).await?.iter().any(|t| t.name == a.template)
                && !crate::workflows::workflows_for(st, realm).await?.iter().any(|w| w.name == a.template)
            {
                return Err(bad_request(format!("no template or workflow named {:?}", a.template)));
            }
        }
        "write" => {
            let c = p["collection"].as_str().unwrap_or_default();
            let col =
                crate::collections::get(st, realm, c).await.map_err(|_| bad_request(format!("no collection {c:?} in realm {realm}")))?;
            if col.system {
                return Err(bad_request(format!("{c} is carbon6's own configuration")));
            }
            if !p["fields"].is_object() {
                return Err(bad_request("a write gives the record's fields: {\"fields\": {\"amount\": \"$.body.amount\"}}"));
            }
        }
        "ask" if p["title"].as_str().is_none_or(|t| t.trim().is_empty()) => return Err(bad_request("an ask gives the decision's title")),
        "notify" if p["title"].as_str().or(p["message"].as_str()).is_none_or(|t| t.trim().is_empty()) => {
            return Err(bad_request("a notify gives a title or a message"));
        }
        "ask" | "notify" => {}
        // A kit tool of an app, with no agent (docs/knowledge.md §6.2): the
        // trigger's own app, or the one it names.
        "kit" => {
            let inst = if a.owner.is_empty() { p["instance"].as_str().unwrap_or_default() } else { a.owner.as_str() };
            let i = crate::instances::get(&st.store.pool, realm, inst).await?.ok_or_else(|| {
                bad_request(format!("a kit action names an app in realm {realm}: {{\"instance\", \"kit\", \"tool\", \"args\"}}"))
            })?;
            let kit = p["kit"].as_str().unwrap_or_default();
            if !i.package.requires.contains_key(kit) {
                return Err(bad_request(format!("the app {inst} does not require the kit {kit:?}")));
            }
        }
        "operation" => {
            let (inst, op) = (p["instance"].as_str().unwrap_or_default(), p["operation"].as_str().unwrap_or_default());
            let ops = crate::operations::list(st, realm, inst).await.map_err(|_| {
                bad_request(format!(
                    "an operation action names an app in realm {realm}: {{\"instance\": …, \"operation\": …, \"input\": …}}"
                ))
            })?;
            if !ops.iter().any(|o| o.name == op) {
                return Err(bad_request(format!("the app {inst} has no operation {op:?}")));
            }
        }
        "record" => {
            let name = p["series"].as_str().unwrap_or_default();
            let s = crate::series::get(st, realm, name).await.map_err(|_| bad_request(format!("no series {name:?} in realm {realm}")))?;
            if s.computed {
                return Err(bad_request(format!("{name} is computed from model usage")));
            }
            if p["value"].is_null() {
                return Err(bad_request("record a value gives the value: {\"series\": …, \"value\": \"$.body.amount\"}"));
            }
        }
        other => return Err(bad_request(format!("{other:?} is not an action ({})", ACTIONS.join(", ")))),
    }
    Ok(())
}

/// What a memex trigger waits for: an event type and conditions, e.g.
/// `mail.received from:alice@acme.com` or `mail.received from:@acme.com
/// subject:invoice` (all must hold; case-insensitive).
#[derive(Debug, PartialEq)]
pub struct EventFilter {
    pub kind: String,
    pub conditions: Vec<(String, String)>,
}

pub const MEMEX_EVENTS: [&str; 2] = ["mail.received", "request.opened"];

impl EventFilter {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut words = s.split_whitespace();
        let kind = words.next().ok_or("say which event, e.g. mail.received from:alice@acme.com")?.to_owned();
        if !MEMEX_EVENTS.contains(&kind.as_str()) {
            return Err(format!("{kind:?} is not an event ({})", MEMEX_EVENTS.join(", ")));
        }
        let mut conditions = Vec::new();
        for w in words {
            let (k, v) = w.split_once(':').ok_or_else(|| format!("{w:?}: conditions are key:value (from:, to:, subject:, account:)"))?;
            if !matches!(k, "from" | "to" | "subject" | "account") {
                return Err(format!("{k:?} is not a condition (from, to, subject, account)"));
            }
            conditions.push((k.to_owned(), v.to_lowercase()));
        }
        Ok(Self { kind, conditions })
    }

    pub fn matches(&self, kind: &str, data: &Value) -> bool {
        self.kind == kind
            && self.conditions.iter().all(|(k, v)| {
                let field = data[k.as_str()].as_str().unwrap_or_default().to_lowercase();
                match k.as_str() {
                    // The addresses in "Name <a@b>, c@d"; from:@acme.com is a
                    // domain.
                    "from" | "to" => field
                        .split(',')
                        .map(|p| p.rsplit_once('<').map(|(_, a)| a.trim_end_matches(['>', ' '])).unwrap_or(p).trim())
                        .any(|a| if v.starts_with('@') { a.ends_with(v.as_str()) } else { a.contains(v.as_str()) }),
                    _ => field.contains(v),
                }
            })
    }
}

// --- JSONPath, filters and mapping --------------------------------------------

/// The value at `$.a.b[0].c` (or `$.a.b.0.c`) in `v`.
pub fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let rest = path.strip_prefix('$')?;
    let mut cur = v;
    for part in rest.replace('[', ".").replace(']', "").split('.').filter(|p| !p.is_empty()) {
        cur = match cur {
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            Value::Object(o) => o.get(part)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Whether the event passes a filter: each JSONPath's value equals the
/// given one, or meets its comparisons ($eq, $ne, $in, $nin, $gt, $gte,
/// $lt, $lte, $contains, $exists).
pub fn passes(filter: &Value, event: &Value) -> bool {
    let Some(conds) = filter.as_object() else { return true };
    conds.iter().all(|(path, cond)| {
        let v = at(event, path);
        let ops: Vec<(&str, &Value)> = match cond {
            Value::Object(o) if !o.is_empty() && o.keys().all(|k| k.starts_with('$')) => o.iter().map(|(k, v)| (k.as_str(), v)).collect(),
            c => vec![("$eq", c)],
        };
        ops.into_iter().all(|(op, want)| match op {
            "$eq" => v == Some(want),
            "$ne" => v != Some(want),
            "$in" => want.as_array().is_some_and(|l| v.is_some_and(|v| l.contains(v))),
            "$nin" => !want.as_array().is_some_and(|l| v.is_some_and(|v| l.contains(v))),
            "$gt" => v.and_then(|v| cmp(v, want)).is_some_and(|o| o.is_gt()),
            "$gte" => v.and_then(|v| cmp(v, want)).is_some_and(|o| o.is_ge()),
            "$lt" => v.and_then(|v| cmp(v, want)).is_some_and(|o| o.is_lt()),
            "$lte" => v.and_then(|v| cmp(v, want)).is_some_and(|o| o.is_le()),
            "$exists" => v.is_some_and(|v| !v.is_null()) == want.as_bool().unwrap_or(true),
            "$contains" => match (v, want) {
                (Some(Value::Array(items)), w) => items.contains(w),
                (Some(Value::String(s)), Value::String(w)) => s.to_lowercase().contains(&w.to_lowercase()),
                _ => false,
            },
            _ => false,
        })
    })
}

/// Parameters with the event's values in: a string that is a JSONPath
/// (`$.body.amount`) becomes that value; `{{$.path}}` inside text becomes
/// it as text.
/// Apps' kit tools running from triggers now: realm/app/kit.tool.
static KIT_RUNS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);

pub fn render(template: &Value, event: &Value) -> Value {
    match template {
        Value::String(s) if s.starts_with("$.") && !s.contains(char::is_whitespace) => at(event, s).cloned().unwrap_or(Value::Null),
        Value::String(s) => Value::String(interpolate(s, event)),
        Value::Array(a) => Value::Array(a.iter().map(|x| render(x, event)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), render(v, event))).collect()),
        other => other.clone(),
    }
}

fn interpolate(s: &str, event: &Value) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("{{$") {
        let Some(j) = rest[i..].find("}}") else { break };
        out.push_str(&rest[..i]);
        let path = rest[i + 2..i + j].trim();
        out.push_str(&match at(event, path) {
            Some(Value::String(v)) => v.clone(),
            Some(Value::Null) | None => String::new(),
            // 105, not 105.0.
            Some(Value::Number(n)) if n.as_f64().is_some_and(|f| f.fract() == 0.0 && f.abs() < 1e15) => {
                format!("{}", n.as_f64().unwrap_or(0.0) as i64)
            }
            Some(v) => v.to_string(),
        });
        rest = &rest[i + j + 2..];
    }
    out.push_str(rest);
    out
}

// --- Firing ---------------------------------------------------------------------

/// An event in `realm`: runs the triggers waiting for it whose filters it
/// passes. `kind`: the event (record, decision, session…); `op`: what
/// happened (created, answered…), matched against the trigger's ops.
pub async fn dispatch(st: &HeadState, realm: &str, kind: &str, op: &str, event: Value) {
    let all = match list(st, &crate::realms::Caller::in_realm(realm)).await {
        Ok(a) => a,
        Err(e) => {
            warn!("loading triggers: {e:#}");
            return;
        }
    };
    for a in all.into_iter().filter(|a| a.enabled && a.trigger == kind) {
        let ops = a.event_params["ops"].as_array();
        if ops.is_some_and(|o| !o.is_empty() && !o.iter().any(|x| x == op)) {
            continue;
        }
        let wants = match kind {
            "record" => a.event_params["collection"] == event["collection"],
            "session" => {
                let labels: Vec<String> = serde_json::from_value(a.event_params["labels"].clone()).unwrap_or_default();
                let have: Vec<String> = serde_json::from_value(event["session"]["labels"].clone()).unwrap_or_default();
                api::labels::applies(&labels, &have)
            }
            _ => true,
        };
        if wants && passes(&a.filter, &event) {
            let input = event_text(&event);
            Box::pin(act(st, &a, &event, &input, kind)).await;
        }
    }
}

/// A record changed (after it is written).
pub async fn on_record(st: &HeadState, realm: &str, op: &str, rec: &api::Record, before: Option<&Value>) {
    let changed: Vec<String> = match before.and_then(Value::as_object) {
        Some(old) => {
            let new = rec.fields.as_object().cloned().unwrap_or_default();
            let mut keys: Vec<String> = old.keys().chain(new.keys()).filter(|k| old.get(*k) != new.get(*k)).cloned().collect();
            keys.sort();
            keys.dedup();
            keys
        }
        None => Vec::new(),
    };
    let event = json!({"type": "record", "op": op, "collection": rec.collection, "record": rec, "before": before, "changed": changed});
    dispatch(st, realm, "record", op, event).await;
}

/// A decision was opened, answered, expired or dismissed.
pub async fn on_decision(st: &HeadState, d: &api::Decision, op: &str) {
    let option = d.answer.as_ref().and_then(|a| a["option"].as_str()).unwrap_or_default();
    let event = json!({"type": "decision", "op": op, "decision": d, "option": option});
    dispatch(st, &d.realm, "decision", op, event).await;
}

/// A session's turn finished or failed, or it waits for an approval.
pub async fn on_session(st: &HeadState, session_id: &str, op: &str, message: &str) {
    let Ok(Some(s)) = st.store.session(session_id).await else { return };
    let event = json!({"type": "session", "op": op, "message": message,
                       "session": {"id": s.id, "title": s.title, "labels": s.all_labels(), "realm": s.realm}});
    dispatch(st, &s.realm, "session", op, event).await;
}

/// Checks series thresholds (in `realm`, of `series`, or all): a trigger
/// fires when its threshold becomes crossed ("crossed") and when it stops
/// being so ("cleared").
pub async fn watch_series(st: &HeadState, realm: Option<&str>, series: Option<&str>) {
    let rows = match sqlx::query("SELECT id, realm FROM triggers WHERE event = 'series' AND enabled = 1 AND (?1 IS NULL OR realm = ?1)")
        .bind(realm)
        .fetch_all(&st.store.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("loading series triggers: {e:#}");
            return;
        }
    };
    for r in rows {
        let (id, realm): (String, String) = (r.get("id"), r.get("realm"));
        let Ok(a) = get(st, &id).await else { continue };
        let p = &a.event_params;
        let name = p["series"].as_str().unwrap_or_default();
        if series.is_some_and(|s| s != name) {
            continue;
        }
        let filter = p["where"].as_object().cloned().unwrap_or_default();
        // The state before the data: a watcher that read the data before a
        // point arrived also read the state before any change it caused.
        let was: Option<bool> = sqlx::query_scalar("SELECT crossed FROM series_alerts WHERE trigger_id = ?")
            .bind(&id)
            .fetch_optional(&st.store.pool)
            .await
            .ok()
            .flatten()
            .map(|c: i64| c != 0);
        let (crossed, value) = if let Some(m) = p["silent_minutes"].as_f64().filter(|m| *m > 0.0) {
            let last = crate::series::latest_ts(st, &realm, name, &filter).await.ok().flatten();
            // Silent since the last point, or since the trigger was made.
            let since = last.unwrap_or(a.created_at);
            (now_ms() - since > (m * 60_000.0) as i64, last.map(|t| json!(t)).unwrap_or(Value::Null))
        } else {
            let q = api::SeriesQuery {
                agg: p["agg"].as_str().unwrap_or("sum").to_owned(),
                minutes: Some(p["minutes"].as_u64().unwrap_or(60)),
                filter,
                ..Default::default()
            };
            let Ok(res) = crate::series::query(st, &realm, name, &q).await else { continue };
            let Some(v) = res.value else { continue };
            let t = p["value"].as_f64().unwrap_or(0.0);
            let crossed = match p["op"].as_str() {
                Some(">") => v > t,
                Some(">=") => v >= t,
                Some("<") => v < t,
                _ => v <= t,
            };
            (crossed, json!(v))
        };
        if was == Some(crossed) {
            continue;
        }
        // Only the watcher that moves the state from what it read acts on
        // the change: two at once (a point arriving during the sweep) can't
        // both notify, or put back a state the other already moved on from.
        let moved = match was {
            None => {
                sqlx::query("INSERT OR IGNORE INTO series_alerts (trigger_id, crossed, at) VALUES (?, ?, ?)")
                    .bind(&id)
                    .bind(crossed as i64)
                    .bind(now_ms())
                    .execute(&st.store.pool)
                    .await
            }
            Some(was) => {
                sqlx::query("UPDATE series_alerts SET crossed = ?, at = ? WHERE trigger_id = ? AND crossed = ?")
                    .bind(crossed as i64)
                    .bind(now_ms())
                    .bind(&id)
                    .bind(was as i64)
                    .execute(&st.store.pool)
                    .await
            }
        };
        // Not crossed to start with is no change to tell about.
        if !moved.is_ok_and(|r| r.rows_affected() == 1) || (was.is_none() && !crossed) {
            continue;
        }
        let op = if crossed { "crossed" } else { "cleared" };
        if a.event_params["ops"].as_array().is_some_and(|o| !o.is_empty() && !o.iter().any(|x| x == op)) {
            continue;
        }
        let event = json!({"type": "series", "op": op, "series": name, "value": value, "threshold": a.event_params, "realm": realm});
        if passes(&a.filter, &event) {
            Box::pin(act(st, &a, &event, &event_text(&event), "series")).await;
        }
    }
}

/// What a start action gets as {{input}} when the trigger doesn't say.
fn event_text(event: &Value) -> String {
    match event["type"].as_str() {
        Some("webhook") => match &event["body"] {
            Value::String(s) => s.clone(),
            b => serde_json::to_string_pretty(b).unwrap_or_default(),
        },
        Some("memex") if event["event"] == "mail.received" => {
            let d = &event["data"];
            format!(
                "New email from {} to {}\nSubject: {}\n\n{}\n\n(memex item {}; thread {} in account {})",
                d["from"].as_str().unwrap_or(""),
                d["to"].as_str().unwrap_or(""),
                d["subject"].as_str().unwrap_or(""),
                d["snippet"].as_str().unwrap_or(""),
                d["item_id"].as_str().unwrap_or(""),
                d["thread_id"].as_str().unwrap_or(""),
                d["account"].as_str().unwrap_or("")
            )
        }
        Some("memex") if event["event"] == "request.opened" => {
            let d = &event["data"];
            format!(
                "{} needs something from you: {}\n{}\n\n(request {}; {}; from {})",
                d["from"].as_str().unwrap_or(""),
                d["subject"].as_str().unwrap_or(""),
                d["reason"].as_str().unwrap_or(""),
                d["request_id"],
                d["category"].as_str().unwrap_or(""),
                d["sources"].as_array().into_iter().flatten().filter_map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            )
        }
        Some("schedule") => String::new(),
        _ => serde_json::to_string_pretty(event).unwrap_or_default(),
    }
}

/// The memex reported an event, from one realm's knowledge: only that
/// realm's triggers hear it.
pub async fn on_memex_event(st: &HeadState, realm: &str, kind: &str, data: &Value) {
    let all = match list(st, &crate::realms::Caller::in_realm(realm)).await {
        Ok(a) => a,
        Err(e) => {
            warn!("loading triggers: {e:#}");
            return;
        }
    };
    let event = json!({"type": "memex", "event": kind, "data": data});
    for a in all.into_iter().filter(|a| a.enabled && a.trigger == "memex") {
        if EventFilter::parse(&a.cron).is_ok_and(|f| f.matches(kind, data)) && passes(&a.filter, &event) {
            act(st, &a, &event, &event_text(&event), "memex").await;
        }
    }
}

/// Runs a trigger's action for an event, unless loop protection stops it:
/// the chain of triggers that led here is too deep, or it ran too often in
/// the last hour.
pub async fn act(st: &HeadState, a: &api::Automation, event: &Value, input: &str, via: &str) -> Option<api::TemplateRun> {
    let chain = CHAIN.try_with(Clone::clone).unwrap_or_default();
    let stop = if chain.len() >= MAX_DEPTH {
        Some(format!("loop protection: a chain of {} triggers led here ({})", chain.len(), chain.join(" → ")))
    } else if via != "manual" && a.per_hour > 0 && runs_last_hour(st, &a.id).await >= a.per_hour as i64 {
        Some(format!("loop protection: it ran {} times in the last hour (its limit)", a.per_hour))
    } else {
        None
    };
    if let Some(why) = stop {
        stopped(st, a, &why, via, &chain).await;
        return None;
    }
    let mut next = chain.clone();
    next.push(a.id.clone());
    let run = CHAIN.scope(next, async {
        if a.action == "start" {
            let input = match a.action_params.get("input") {
                Some(t) => match render(t, event) {
                    Value::String(s) => s,
                    v => v.to_string(),
                },
                None if via == "schedule" || via == "manual" => a.input.clone(),
                None => input.to_owned(),
            };
            fire(st, a, &input, via, &chain).await.map(Some)
        } else {
            let result = Box::pin(do_action(st, a, event)).await;
            record_run(st, a, via, &chain, result.as_ref().err().map(|e| format!("{e:#}"))).await;
            result.map(|_| None)
        }
    });
    match run.await {
        Ok(r) => r,
        Err(e) => {
            warn!(trigger = %a.name, "trigger failed: {e:#}");
            None
        }
    }
}

async fn runs_last_hour(st: &HeadState, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM trigger_runs WHERE trigger_id = ? AND started_at > ? AND trigger != 'manual'")
        .bind(id)
        .bind(now_ms() - 3_600_000)
        .fetch_one(&st.store.pool)
        .await
        .unwrap_or(0)
}

/// Loop protection stopped a run: noted on the trigger, and you are told
/// (once, until it runs again).
async fn stopped(st: &HeadState, a: &api::Automation, why: &str, via: &str, chain: &[String]) {
    warn!(trigger = %a.name, "{why}");
    let first = a.last_error.as_deref().is_none_or(|e| !e.starts_with("loop protection"));
    let _ = sqlx::query("UPDATE triggers SET last_error = ? WHERE id = ?").bind(why).bind(&a.id).execute(&st.store.pool).await;
    if first {
        let run_id = uuid::Uuid::now_v7().simple().to_string()[16..].to_owned();
        let _ = sqlx::query(
            "INSERT INTO trigger_runs (run_id, trigger_id, started_at, finished_at, trigger, outcome, error, cause) \
             VALUES (?, ?, ?, ?, ?, 'stopped', ?, ?)",
        )
        .bind(&run_id)
        .bind(&a.id)
        .bind(now_ms())
        .bind(now_ms())
        .bind(via)
        .bind(why)
        .bind(serde_json::to_string(chain).unwrap_or_default())
        .execute(&st.store.pool)
        .await;
        st.notifier.send(crate::notify::Note {
            event: "automation",
            title: format!("Trigger {} stopped", a.name),
            message: why.to_owned(),
            priority: "high",
            labels: automation_labels(a),
            ..Default::default()
        });
    }
}

/// The write, ask and notify actions.
async fn do_action(st: &HeadState, a: &api::Automation, event: &Value) -> anyhow::Result<()> {
    let realm = realm_of(st, &a.id).await;
    let p = render(&a.action_params, event);
    // An app's trigger acts as the app (docs/apps.md §6).
    let by = if a.owner.is_empty() {
        format!("trigger:{}", label_safe(&a.name))
    } else {
        format!("app:{} trigger:{}", a.owner, label_safe(&a.name))
    };
    match a.action.as_str() {
        "write" => {
            let collection = p["collection"].as_str().unwrap_or_default();
            let fields = p["fields"].as_object().cloned().unwrap_or_default();
            match p["id"].as_str().filter(|i| !i.is_empty()) {
                // An update: the mapped fields over the record's own.
                Some(id) => {
                    let cur = crate::collections::record(st, &realm, collection, id).await?;
                    let mut merged = cur.fields.as_object().cloned().unwrap_or_default();
                    merged.extend(fields);
                    let rec = api::RecordIn {
                        fields: Value::Object(merged),
                        version: Some(cur.version),
                        derived_from: serde_json::from_value(p["derived_from"].clone()).ok(),
                        ..Default::default()
                    };
                    crate::collections::put(st, &realm, collection, Some(id), &rec, &by).await?;
                }
                None => {
                    let rec = api::RecordIn {
                        fields: Value::Object(fields),
                        links: serde_json::from_value(p["links"].clone()).ok(),
                        labels: serde_json::from_value(p["labels"].clone()).ok(),
                        version: None,
                        derived_from: serde_json::from_value(p["derived_from"].clone()).ok(),
                    };
                    crate::collections::put(st, &realm, collection, None, &rec, &by).await?;
                }
            }
        }
        "ask" => {
            let d = api::DecisionIn {
                kind: p["kind"]
                    .as_str()
                    .unwrap_or(if p["options"].as_array().is_some_and(|o| !o.is_empty()) { "choose" } else { "question" })
                    .to_owned(),
                title: p["title"].as_str().unwrap_or_default().to_owned(),
                document: p["document"].as_str().unwrap_or_default().to_owned(),
                options: serde_json::from_value(p["options"].clone()).unwrap_or_default(),
                priority: p["priority"].as_str().unwrap_or("normal").to_owned(),
                context: json!({"trigger": a.id, "event": event}),
                source: by,
                ..Default::default()
            };
            crate::decisions::open(st, &d, &realm, "").await?;
        }
        "record" => {
            let value = match &p["value"] {
                Value::String(s) => s.trim().parse::<f64>().ok(),
                v => v.as_f64(),
            }
            .ok_or_else(|| anyhow::anyhow!("the value is not a number: {}", p["value"]))?;
            let point = api::Point { ts: p["ts"].as_i64().unwrap_or(0), value, dims: p["dims"].as_object().cloned().unwrap_or_default() };
            crate::series::record(st, &realm, p["series"].as_str().unwrap_or_default(), &point, &by).await?;
        }
        "operation" => {
            let (inst, op) = (p["instance"].as_str().unwrap_or_default(), p["operation"].as_str().unwrap_or_default());
            crate::operations::call(st, &realm, inst, op, &p["input"], &by).await?;
        }
        "kit" => {
            let inst = if a.owner.is_empty() { p["instance"].as_str().unwrap_or_default() } else { a.owner.as_str() };
            let (kit, tool) = (p["kit"].as_str().unwrap_or_default(), p["tool"].as_str().unwrap_or_default());
            let args = p["args"].as_object().cloned().unwrap_or_default();
            let state: Vec<String> = serde_json::from_value(p["state"].clone()).unwrap_or_default();
            // One run of an app's tool at a time; one that asks to go again
            // (an import under way) does, soon.
            let key = format!("{realm}/{inst}/{kit}.{tool}");
            if !KIT_RUNS.lock().unwrap().insert(key.clone()) {
                return Ok(());
            }
            crate::activity::round_started(&key);
            let result = async {
                for _ in 0..500 {
                    let v = crate::kitjobs::for_instance(st, &realm, inst, kit, tool, args.clone(), Default::default(), &by, &state, None)
                        .await?;
                    crate::activity::round_ran(&key, &v);
                    if v["again"] != true {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                anyhow::Ok(())
            }
            .await;
            KIT_RUNS.lock().unwrap().remove(&key);
            crate::activity::round_ended(&key, result.as_ref().err().map(|e| format!("{e:#}")));
            result?;
        }
        "notify" => {
            st.notifier.send(crate::notify::Note {
                event: "trigger",
                title: p["title"].as_str().unwrap_or(&a.name).to_owned(),
                message: p["message"].as_str().unwrap_or_default().to_owned(),
                priority: if p["priority"] == "high" {
                    "high"
                } else if p["priority"] == "low" {
                    "low"
                } else {
                    "normal"
                },
                labels: automation_labels(a),
                ..Default::default()
            });
        }
        other => anyhow::bail!("unknown action {other}"),
    }
    Ok(())
}

/// A finished run of a write, ask or notify action.
async fn record_run(st: &HeadState, a: &api::Automation, via: &str, chain: &[String], error: Option<String>) {
    let run_id = uuid::Uuid::now_v7().simple().to_string()[16..].to_owned();
    let now = now_ms();
    let r = sqlx::query(
        "INSERT INTO trigger_runs (run_id, trigger_id, started_at, finished_at, trigger, outcome, error, cause) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&run_id)
    .bind(&a.id)
    .bind(now)
    .bind(now)
    .bind(via)
    .bind(if error.is_some() { "failed" } else { "ok" })
    .bind(&error)
    .bind(serde_json::to_string(chain).unwrap_or_default())
    .execute(&st.store.pool)
    .await;
    if let Err(e) = r {
        warn!("recording a trigger run: {e:#}");
    }
    let _ = sqlx::query("UPDATE triggers SET last_run_at = ?, last_run_id = ?, last_error = ? WHERE id = ?")
        .bind(now)
        .bind(&run_id)
        .bind(&error)
        .bind(&a.id)
        .execute(&st.store.pool)
        .await;
    if let Some(error) = error {
        st.notifier.send(crate::notify::Note {
            event: "automation",
            title: format!("Trigger {} failed", a.name),
            message: error,
            priority: "high",
            labels: automation_labels(a),
            ..Default::default()
        });
        after_failure(st, &a.id).await;
    }
}

fn edit_into(e: api::AutomationEdit, cur: api::Automation) -> api::Automation {
    api::Automation {
        name: e.name.unwrap_or(cur.name).trim().to_owned(),
        template: e.template.unwrap_or(cur.template),
        trigger: e.trigger.unwrap_or(cur.trigger),
        cron: e.cron.unwrap_or(cur.cron).trim().to_owned(),
        event_params: e.event_params.unwrap_or(cur.event_params),
        filter: e.filter.unwrap_or(cur.filter),
        action: e.action.unwrap_or(cur.action),
        action_params: e.action_params.unwrap_or(cur.action_params),
        per_hour: e.per_hour.unwrap_or(cur.per_hour),
        input: e.input.unwrap_or(cur.input),
        enabled: e.enabled.unwrap_or(cur.enabled),
        disable_after: e.disable_after.unwrap_or(cur.disable_after),
        ..cur
    }
}

fn obj(v: &Value) -> String {
    if v.is_null() { "{}".to_owned() } else { v.to_string() }
}

/// A new trigger in `realm` (its actions work there); a webhook's response
/// carries its URL (once).
pub async fn create(st: &HeadState, e: api::AutomationEdit, realm: &str) -> anyhow::Result<api::Automation> {
    create_owned(st, e, realm, "").await
}

/// A trigger an app instance brings (`owner`): it acts as the instance,
/// and changes only with it (docs/apps.md §5.5).
pub async fn create_owned(st: &HeadState, e: api::AutomationEdit, realm: &str, owner: &str) -> anyhow::Result<api::Automation> {
    let blank = api::Automation {
        id: String::new(),
        name: String::new(),
        template: String::new(),
        trigger: "schedule".into(),
        cron: String::new(),
        event_params: json!({}),
        filter: json!({}),
        action: "start".into(),
        action_params: json!({}),
        per_hour: 60,
        input: String::new(),
        enabled: true,
        created_at: 0,
        last_run_at: None,
        last_run_id: None,
        last_error: None,
        next_run_at: None,
        webhook_url: None,
        disable_after: 0,
        recent: Vec::new(),
        owner: owner.to_owned(),
    };
    let a = edit_into(e, blank);
    check(st, &a, realm).await?;
    let id = uuid::Uuid::now_v7().simple().to_string();
    let token = (a.trigger == "webhook").then(new_token);
    sqlx::query(
        "INSERT INTO triggers (id, name, template, event, cron, input, token_hash, enabled, created_at, disable_after, realm, \
         event_params, filter, action, action_params, per_hour, owner) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&a.name)
    .bind(&a.template)
    .bind(&a.trigger)
    .bind(&a.cron)
    .bind(&a.input)
    .bind(token.as_deref().map(|t| c6_core::sha256_hex(t.as_bytes())).unwrap_or_default())
    .bind(a.enabled as i64)
    .bind(now_ms())
    .bind(a.disable_after as i64)
    .bind(realm)
    .bind(obj(&a.event_params))
    .bind(obj(&a.filter))
    .bind(&a.action)
    .bind(obj(&a.action_params))
    .bind(a.per_hour as i64)
    .bind(owner)
    .execute(&st.store.pool)
    .await?;
    let mut a = get(st, &id).await?;
    a.webhook_url = token.map(|t| webhook_path(&id, &t));
    Ok(a)
}

/// Changes a trigger (its event kind stays: a webhook keeps its URL).
pub async fn update(st: &HeadState, id: &str, mut e: api::AutomationEdit) -> anyhow::Result<api::Automation> {
    let cur = get(st, id).await?;
    e.trigger = None;
    let a = edit_into(e, cur);
    check(st, &a, &realm_of(st, id).await).await?;
    sqlx::query(
        "UPDATE triggers SET name = ?, template = ?, cron = ?, input = ?, enabled = ?, disable_after = ?, event_params = ?, filter = ?, \
         action = ?, action_params = ?, per_hour = ? WHERE id = ?",
    )
    .bind(&a.name)
    .bind(&a.template)
    .bind(&a.cron)
    .bind(&a.input)
    .bind(a.enabled as i64)
    .bind(a.disable_after as i64)
    .bind(obj(&a.event_params))
    .bind(obj(&a.filter))
    .bind(&a.action)
    .bind(obj(&a.action_params))
    .bind(a.per_hour as i64)
    .bind(id)
    .execute(&st.store.pool)
    .await?;
    get(st, id).await
}

/// What you may change of a trigger an app brought: only whether it is on
/// (the rest is the app's code, docs/apps.md §5.5).
pub async fn check_edit(st: &HeadState, id: &str, e: Option<&api::AutomationEdit>) -> anyhow::Result<()> {
    let cur = get(st, id).await?;
    if cur.owner.is_empty() {
        return Ok(());
    }
    let only_enabled = e.is_some_and(|e| {
        let mut rest = e.clone();
        rest.enabled = None;
        let blank = api::AutomationEdit::default();
        serde_json::to_value(&rest).ok() == serde_json::to_value(&blank).ok()
    });
    if only_enabled {
        return Ok(());
    }
    Err(bad_request(format!(
        "trigger {} belongs to the app {}: it changes when the app is upgraded (you can switch it off or on)",
        cur.name, cur.owner
    )))
}

/// A new webhook token (the old URL stops working); the response has the URL.
pub async fn rotate(st: &HeadState, id: &str) -> anyhow::Result<api::Automation> {
    let mut a = get(st, id).await?;
    if a.trigger != "webhook" {
        return Err(bad_request("only webhooks have a URL"));
    }
    let token = new_token();
    sqlx::query("UPDATE triggers SET token_hash = ? WHERE id = ?")
        .bind(c6_core::sha256_hex(token.as_bytes()))
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    a.webhook_url = Some(webhook_path(id, &token));
    Ok(a)
}

pub async fn delete(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query("DELETE FROM triggers WHERE id = ?").bind(id).execute(&mut *tx).await?;
    if done.rows_affected() == 0 {
        return Err(not_found("no such trigger"));
    }
    sqlx::query("DELETE FROM run_sessions WHERE run_id IN (SELECT run_id FROM trigger_runs WHERE trigger_id = ?)")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM trigger_runs WHERE trigger_id = ?").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

fn automation_labels(a: &api::Automation) -> Vec<String> {
    let mut l = vec![format!("automation:{}", label_safe(&a.name))];
    if a.action == "start" {
        l.push(format!("template:{}", a.template));
    }
    l
}

/// The start action: runs the trigger's template (or workflow) now,
/// recording the run. `trigger`: what fired it.
pub async fn fire(st: &HeadState, a: &api::Automation, input: &str, trigger: &str, cause: &[String]) -> anyhow::Result<api::TemplateRun> {
    // A workflow, when no template has the name: the run is the workflow's,
    // and its outcome arrives when it finishes (`workflow_finished`).
    // Its sessions run in the trigger's realm, with the realm's templates.
    let realm = realm_of(st, &a.id).await;
    let instance = Some(a.owner.as_str()).filter(|o| !o.is_empty());
    let is_template = crate::templates::templates_in(st, &realm, instance).await?.iter().any(|(t, _)| t.name == a.template);
    let workflow = !is_template && crate::workflows::workflows_for(st, &realm).await?.iter().any(|w| w.name == a.template);
    let result = if workflow {
        crate::workflows::start(st, &a.template, input, trigger, &realm).await.map(|r| api::TemplateRun {
            run_id: r.id,
            sessions: Vec::new(),
            errors: Vec::new(),
        })
    } else {
        crate::templates::run_as(st, &a.template, input, &[format!("automation:{}", label_safe(&a.name))], &realm, instance).await
    };
    let (run_id, error) = match &result {
        Ok(r) => (Some(r.run_id.clone()), (!r.errors.is_empty()).then(|| r.errors.join("; "))),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    sqlx::query("UPDATE triggers SET last_run_at = ?, last_run_id = COALESCE(?, last_run_id), last_error = ? WHERE id = ?")
        .bind(now_ms())
        .bind(&run_id)
        .bind(&error)
        .bind(&a.id)
        .execute(&st.store.pool)
        .await?;
    match &result {
        Ok(r) => info!(trigger = %a.name, run = %r.run_id, sessions = r.sessions.len(), "trigger ran"),
        Err(e) => warn!(trigger = %a.name, "trigger failed: {e:#}"),
    }
    if let Some(error) = &error {
        st.notifier.send(crate::notify::Note {
            event: "automation",
            title: format!("Trigger {} failed", a.name),
            message: error.clone(),
            priority: "high",
            labels: automation_labels(a),
            ..Default::default()
        });
    }
    // The run, and its sessions (whose first turns decide how it went).
    let started = now_ms();
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let (run_id, running) = match &result {
        Ok(r) => (r.run_id.clone(), workflow || !r.sessions.is_empty()),
        Err(_) => (uuid::Uuid::now_v7().simple().to_string()[24..].to_owned(), false),
    };
    sqlx::query(
        "INSERT INTO trigger_runs (run_id, trigger_id, started_at, finished_at, trigger, outcome, error, cause) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&run_id)
    .bind(&a.id)
    .bind(started)
    .bind((!running).then_some(started))
    .bind(trigger)
    .bind(if running { "running" } else { "failed" })
    .bind(&error)
    .bind(serde_json::to_string(cause).unwrap_or_default())
    .execute(&mut *tx)
    .await?;
    if let Ok(r) = &result {
        for s in &r.sessions {
            sqlx::query("INSERT OR IGNORE INTO run_sessions (session_id, run_id) VALUES (?, ?)")
                .bind(&s.id)
                .bind(&run_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    if !running {
        after_failure(st, &a.id).await;
    }
    result
}

/// A run's session finished its first turn (or failed to start):
/// `outcome` is "ok", "failed" or "stopped". When it was the run's last,
/// the run is done.
pub async fn session_ended(st: &HeadState, session_id: &str, outcome: &str) {
    if let Err(e) = end_session(st, session_id, outcome).await {
        warn!(session = %session_id, "recording a trigger run: {e:#}");
    }
}

async fn end_session(st: &HeadState, session_id: &str, outcome: &str) -> anyhow::Result<()> {
    let Some(run_id) =
        sqlx::query_scalar::<_, String>("UPDATE run_sessions SET outcome = ? WHERE session_id = ? AND outcome IS NULL RETURNING run_id")
            .bind(outcome)
            .bind(session_id)
            .fetch_optional(&st.store.pool)
            .await?
    else {
        return Ok(());
    };
    let outcomes: Vec<Option<String>> =
        sqlx::query_scalar("SELECT outcome FROM run_sessions WHERE run_id = ?").bind(&run_id).fetch_all(&st.store.pool).await?;
    if outcomes.iter().any(Option::is_none) {
        return Ok(());
    }
    let row = sqlx::query("SELECT trigger_id, error FROM trigger_runs WHERE run_id = ? AND outcome = 'running'")
        .bind(&run_id)
        .fetch_optional(&st.store.pool)
        .await?;
    let Some(row) = row else { return Ok(()) };
    let trigger_id: String = row.get("trigger_id");
    // Targets that did not start count as failures too.
    let partly: bool = row.get::<Option<String>, _>("error").is_some();
    let has = |o: &str| outcomes.iter().any(|x| x.as_deref() == Some(o));
    let run_outcome = if has("failed") || partly {
        "failed"
    } else if has("stopped") {
        "stopped"
    } else {
        "ok"
    };
    sqlx::query("UPDATE trigger_runs SET outcome = ?, finished_at = ? WHERE run_id = ?")
        .bind(run_outcome)
        .bind(now_ms())
        .bind(&run_id)
        .execute(&st.store.pool)
        .await?;
    if run_outcome == "failed" {
        if let Ok(a) = get(st, &trigger_id).await {
            let failed = outcomes.iter().filter(|o| o.as_deref() == Some("failed")).count();
            st.notifier.send(crate::notify::Note {
                event: "automation",
                title: format!("Trigger {} failed", a.name),
                message: format!("{failed} of {} session(s) in run {run_id} failed.", outcomes.len()),
                priority: "high",
                labels: automation_labels(&a),
                ..Default::default()
            });
        }
        after_failure(st, &trigger_id).await;
    }
    Ok(())
}

/// A workflow a trigger started has finished: `outcome` is "ok", "failed"
/// or "stopped".
pub async fn workflow_finished(st: &HeadState, run_id: &str, outcome: &str) {
    let r =
        sqlx::query("UPDATE trigger_runs SET outcome = ?, finished_at = ? WHERE run_id = ? AND outcome = 'running' RETURNING trigger_id")
            .bind(outcome)
            .bind(now_ms())
            .bind(run_id)
            .fetch_optional(&st.store.pool)
            .await;
    match r {
        Ok(Some(row)) if outcome == "failed" => after_failure(st, &row.get::<String, _>("trigger_id")).await,
        Ok(_) => {}
        Err(e) => warn!("recording a trigger's workflow run: {e:#}"),
    }
}

/// After a failed run: switches the trigger off once its last
/// `disable_after` runs all failed.
async fn after_failure(st: &HeadState, trigger_id: &str) {
    let Ok(a) = get(st, trigger_id).await else { return };
    let n = a.disable_after as usize;
    if n == 0 || !a.enabled {
        return;
    }
    let finished: Vec<String> = match sqlx::query_scalar(
        "SELECT outcome FROM trigger_runs WHERE trigger_id = ? AND outcome != 'running' ORDER BY started_at DESC LIMIT ?",
    )
    .bind(trigger_id)
    .bind(n as i64)
    .fetch_all(&st.store.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!("loading trigger runs: {e:#}");
            return;
        }
    };
    if finished.len() < n || finished.iter().any(|o| o != "failed") {
        return;
    }
    if let Err(e) = sqlx::query("UPDATE triggers SET enabled = 0 WHERE id = ?").bind(trigger_id).execute(&st.store.pool).await {
        warn!("switching a trigger off: {e:#}");
        return;
    }
    warn!(trigger = %a.name, runs = n, "trigger switched off after failed runs");
    st.notifier.send(crate::notify::Note {
        event: "automation",
        title: format!("Trigger {} switched off", a.name),
        message: format!("Its last {n} runs failed. Switch it back on in Settings → Automation."),
        priority: "high",
        labels: automation_labels(&a),
        ..Default::default()
    });
}

/// A trigger's latest runs, newest first.
pub async fn runs(st: &HeadState, id: &str, limit: u32) -> anyhow::Result<Vec<api::AutomationRun>> {
    get(st, id).await?;
    let rows = sqlx::query("SELECT * FROM trigger_runs WHERE trigger_id = ? ORDER BY started_at DESC LIMIT ?")
        .bind(id)
        .bind(limit.clamp(1, 200) as i64)
        .fetch_all(&st.store.pool)
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let run_id: String = r.get("run_id");
        let members = sqlx::query(
            "SELECT rs.session_id, rs.outcome, COALESCE(s.title, '') AS title FROM run_sessions rs \
             LEFT JOIN sessions s ON s.id = rs.session_id WHERE rs.run_id = ? ORDER BY s.created_at",
        )
        .bind(&run_id)
        .fetch_all(&st.store.pool)
        .await?;
        let sessions: Vec<api::RunSession> = members
            .iter()
            .map(|m| api::RunSession {
                id: m.get("session_id"),
                title: m.get("title"),
                outcome: m.get::<Option<String>, _>("outcome").unwrap_or_default(),
            })
            .collect();
        let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
        let (cost, calls) = crate::usage::cost_of(st, &ids).await?;
        out.push(api::AutomationRun {
            run_id,
            started_at: r.get("started_at"),
            finished_at: r.get("finished_at"),
            trigger: r.get("trigger"),
            cause: serde_json::from_str(&r.get::<String, _>("cause")).unwrap_or_default(),
            outcome: r.get("outcome"),
            error: r.get("error"),
            sessions,
            cost,
            calls,
        });
    }
    Ok(out)
}

/// Runs a trigger's action by hand, as for its own event with nothing in
/// it (a schedule's event, for a schedule).
pub async fn run_now(st: &HeadState, id: &str) -> anyhow::Result<api::TemplateRun> {
    let a = get(st, id).await?;
    if a.action == "start" {
        return fire(st, &a, &a.input, "manual", &[]).await;
    }
    let event = if a.trigger == "schedule" { schedule_event(st, &realm_of(st, id).await).await } else { json!({"type": "manual"}) };
    let result = do_action(st, &a, &event).await;
    record_run(st, &a, "manual", &[], result.as_ref().err().map(|e| format!("{e:#}"))).await;
    result?;
    let a = get(st, id).await?;
    Ok(api::TemplateRun { run_id: a.last_run_id.unwrap_or_default(), sessions: Vec::new(), errors: Vec::new() })
}

/// A webhook call: checks the token and runs the trigger with the body.
/// `None`: no such webhook (or a wrong token).
pub async fn webhook(st: &HeadState, id: &str, token: &str, body: &[u8]) -> anyhow::Result<Option<api::TemplateRun>> {
    let row = sqlx::query("SELECT token_hash FROM triggers WHERE id = ? AND event = 'webhook' AND enabled = 1")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let want: String = row.get("token_hash");
    let got = c6_core::sha256_hex(token.as_bytes());
    // Compare in constant time.
    if want.len() != got.len() || want.bytes().zip(got.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return Ok(None);
    }
    let a = get(st, id).await?;
    let body = serde_json::from_slice::<Value>(body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()));
    let event = json!({"type": "webhook", "body": body});
    if !passes(&a.filter, &event) {
        return Ok(Some(api::TemplateRun { run_id: String::new(), sessions: Vec::new(), errors: vec!["filtered out".into()] }));
    }
    if let Some(run) = act(st, &a, &event, &event_text(&event), "webhook").await {
        return Ok(Some(run));
    }
    let a = get(st, id).await?;
    Ok(Some(api::TemplateRun {
        run_id: a.last_run_id.unwrap_or_default(),
        sessions: Vec::new(),
        errors: a.last_error.into_iter().collect(),
    }))
}

/// A name usable in a label.
fn label_safe(name: &str) -> String {
    name.trim().chars().map(|c| if c.is_whitespace() || c == ',' { '-' } else { c }).collect()
}

/// A schedule's event: the time, and the realm's open decisions (for
/// digests: notify with `{{$.open_decisions.text}}`, filtered on
/// `$.open_decisions.count`).
async fn schedule_event(st: &HeadState, realm: &str) -> Value {
    let open = crate::decisions::list(st, "open", &crate::realms::Caller::in_realm(realm)).await.unwrap_or_default();
    let text = open
        .iter()
        .map(|d| format!("- #{} {}{}", d.id, d.title, if d.priority == "high" { " (high)" } else { "" }))
        .collect::<Vec<_>>()
        .join("\n");
    json!({"type": "schedule", "at": now_ms(), "realm": realm,
           "open_decisions": {"count": open.len(), "high": open.iter().filter(|d| d.priority == "high").count(), "text": text}})
}

/// Runs scheduled triggers: every 30 seconds, each enabled schedule whose
/// time came since the last check runs once. Times missed while the head
/// was down are not made up.
pub fn spawn_scheduler(st: Arc<HeadState>) {
    tokio::spawn(async move {
        let mut last = Local::now();
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            let now = Local::now();
            let due: Vec<api::Automation> = match list(&st, &crate::realms::Caller::root()).await {
                Ok(all) => all.into_iter().filter(|a| a.enabled && a.trigger == "schedule").filter(|a| due(&a.cron, last, now)).collect(),
                Err(e) => {
                    warn!("loading triggers: {e:#}");
                    Vec::new()
                }
            };
            last = now;
            for a in due {
                let st = st.clone();
                tokio::spawn(async move { scheduled(&st, &a).await });
            }
        }
    });
}

/// A schedule's time came.
pub async fn scheduled(st: &HeadState, a: &api::Automation) {
    let event = schedule_event(st, &realm_of(st, &a.id).await).await;
    if passes(&a.filter, &event) {
        act(st, a, &event, &a.input, "schedule").await;
    }
}

/// Whether a schedule had a time in (`last`, `now`].
fn due(cron: &str, last: chrono::DateTime<Local>, now: chrono::DateTime<Local>) -> bool {
    Schedule::parse(cron).ok().and_then(|s| s.next_after(last)).is_some_and(|t| t <= now)
}

#[cfg(test)]
mod filter_tests {
    use serde_json::json;

    use super::{EventFilter, at, passes, render};

    #[test]
    fn filters() {
        let f = EventFilter::parse("mail.received from:@acme.com subject:invoice").unwrap();
        let d = json!({"from": "Alice <alice@acme.com>", "subject": "Invoice 7"});
        assert!(f.matches("mail.received", &d));
        assert!(!f.matches("mail.received", &json!({"from": "Eve <eve@acme.com.evil.org>", "subject": "invoice"})));
        assert!(!f.matches("mail.sent", &d));
        assert!(EventFilter::parse("mail.received size:big").is_err());
        assert!(EventFilter::parse("calendar.changed").is_err());
    }

    #[test]
    fn paths_filters_and_mapping() {
        let e = json!({"body": {"amount": 42.5, "items": [{"sku": "a"}], "who": "Ada"}, "tags": ["vip"], "n": 3});
        assert_eq!(at(&e, "$.body.items[0].sku"), Some(&json!("a")));
        assert_eq!(at(&e, "$.body.items.0.sku"), Some(&json!("a")));
        assert_eq!(at(&e, "$.nope"), None);
        assert!(passes(&json!({"$.body.who": "Ada", "$.n": {"$gte": 3, "$lt": 4}}), &e));
        assert!(passes(&json!({"$.tags": {"$contains": "vip"}, "$.missing": {"$exists": false}}), &e));
        assert!(!passes(&json!({"$.body.amount": {"$gt": 50}}), &e));
        assert!(!passes(&json!({"$.body.who": {"$in": ["Bob"]}}), &e));
        assert!(passes(&json!({}), &e) && passes(&serde_json::Value::Null, &e));
        let p = render(&json!({"fields": {"amount": "$.body.amount", "note": "from {{$.body.who}} ({{$.n}})", "kept": 1}}), &e);
        assert_eq!(p, json!({"fields": {"amount": 42.5, "note": "from Ada (3)", "kept": 1}}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_once_per_scheduled_minute() {
        let t = |h, m, s| Local.with_ymd_and_hms(2026, 10, 12, h, m, s).unwrap();
        // Checks every 30 s around 08:00 on a Monday.
        assert!(!due("0 8 * * mon-fri", t(7, 59, 0), t(7, 59, 30)));
        assert!(due("0 8 * * mon-fri", t(7, 59, 30), t(8, 0, 0)));
        assert!(!due("0 8 * * mon-fri", t(8, 0, 0), t(8, 0, 30)), "not twice");
        assert!(!due("0 8 * * sat", t(7, 59, 30), t(8, 0, 10)));
        assert!(!due("not a schedule", t(7, 0, 0), t(9, 0, 0)));
    }
}
