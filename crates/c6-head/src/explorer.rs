//! The realm explorer (docs/explorer.md): what a realm holds, what
//! happened in it lately, lists of its objects, and each object with
//! everything connected to it, across data, knowledge, work, configuration
//! and the flow between realms. It only reads; editing stays with the pages
//! that own each thing.

use std::collections::HashMap;

use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row;

use crate::realms::{COMMONS, ROOT};
use crate::{HeadState, bad_request, not_found};

/// Settings lists the inventory shows, with their labels.
const LISTS: [(&str, &str); 13] = [
    ("views", "Views"),
    ("templates", "Templates"),
    ("workflows", "Workflows"),
    ("skills", "Skills"),
    ("agents", "Agents"),
    ("commands", "Commands"),
    ("hooks", "Hooks"),
    ("mcp_servers", "MCP servers"),
    ("policies", "Policies"),
    ("budgets", "Budgets"),
    ("routes", "Model routes"),
    ("prices", "Prices"),
    ("prompts", "Prompts"),
];

fn r(kind: &str, id: impl Into<String>, title: impl Into<String>, sub: impl Into<String>, at: Option<i64>) -> api::ExRef {
    api::ExRef { kind: kind.into(), id: id.into(), title: title.into(), sub: sub.into(), at }
}

fn entry(group: &str, section: impl Into<String>, label: impl Into<String>, count: i64, changed_at: Option<i64>) -> api::ExEntry {
    api::ExEntry { group: group.into(), section: section.into(), label: label.into(), count, changed_at, ..Default::default() }
}

/// A record's name: its first text field, else its id.
fn record_title(fields: &Value, id: &str) -> String {
    let o = fields.as_object();
    for k in ["name", "title"] {
        if let Some(s) = o.and_then(|o| o.get(k)).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return s.to_owned();
        }
    }
    o.and_then(|o| o.values().find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned))).unwrap_or_else(|| id.to_owned())
}

fn clip(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n { format!("{t}…") } else { t }
}

async fn knowledge(st: &HeadState, realm: &str, method: &str, params: Value) -> Option<Value> {
    if realm == ROOT {
        return None;
    }
    crate::memex::call_in(st, realm, method, params).await.ok()
}

fn month_start() -> i64 {
    use chrono::{Datelike, Local, NaiveTime, TimeZone};
    let d = Local::now().date_naive().with_day(1).unwrap_or_else(|| Local::now().date_naive());
    Local.from_local_datetime(&d.and_time(NaiveTime::MIN)).earliest().map(|t| t.timestamp_millis()).unwrap_or(0)
}

async fn scalar(st: &HeadState, sql: &str, realm: &str) -> (i64, Option<i64>) {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .bind(realm)
        .fetch_one(&st.store.pool)
        .await
        .map(|r| (r.get::<i64, _>(0), r.try_get::<Option<i64>, _>(1).ok().flatten()))
        .unwrap_or((0, None))
}

/// What a realm holds, and what needs attention.
pub async fn summary(st: &HeadState, realm: &str) -> anyhow::Result<api::RealmSummary> {
    let info = crate::realms::list(st, &crate::realms::Caller::root())
        .await?
        .into_iter()
        .find(|r| r.id == realm)
        .ok_or_else(|| not_found("no such realm"))?;
    let mut entries = Vec::new();
    let holds = realm != ROOT;
    if holds {
        // Data.
        for c in crate::collections::list(st, realm).await?.into_iter().filter(|c| !c.system) {
            let last: Option<i64> = sqlx::query_scalar("SELECT max(updated_at) FROM records WHERE realm = ? AND collection = ?")
                .bind(realm)
                .bind(&c.name)
                .fetch_one(&st.store.pool)
                .await?;
            entries.push(entry("data", format!("collection:{}", c.name), &c.name, c.records, last));
        }
        let (n, last) =
            scalar(st, "SELECT count(*), (SELECT max(ts) FROM points WHERE realm = ?1) FROM series WHERE realm = ?1", realm).await;
        entries.push(entry("data", "series", "Series", n + crate::series::USAGE.len() as i64, last));
    }
    // Knowledge.
    let inventory = if holds { knowledge(st, realm, "memex.explore.inventory", json!({})).await } else { None };
    if let Some(inv) = &inventory {
        for (what, label) in [("things", "things"), ("items", "items"), ("facts", "facts")] {
            for g in inv[what].as_array().into_iter().flatten() {
                let k = g["kind"].as_str().unwrap_or("");
                let mut e = entry(
                    "knowledge",
                    format!("{what}:{k}"),
                    format!("{k} ({label})"),
                    g["count"].as_i64().unwrap_or(0),
                    g["last"].as_i64(),
                );
                // Mail, calendar events and photos have pages of their own.
                e.link = match k {
                    "email" if realm == crate::realms::HOME => Some("/mail".into()),
                    "event" if realm == crate::realms::HOME => Some("/calendar".into()),
                    "photo" | "video" if realm == crate::realms::HOME => Some("/photos".into()),
                    _ => None,
                };
                entries.push(e);
            }
        }
    }
    if holds {
        // Work.
        let (n, last) =
            scalar(st, "SELECT count(*), max(updated_at) FROM sessions WHERE realm = ? AND parent_call_id IS NULL", realm).await;
        entries.push(entry("work", "sessions", "Sessions", n, last));
        let (n, last) =
            scalar(st, "SELECT count(*), max(updated_at) FROM decisions WHERE realm = ? AND state IN ('open', 'snoozed')", realm).await;
        entries.push(entry("work", "decisions:open", "Open decisions", n, last));
        let (n, last) =
            scalar(st, "SELECT count(*), max(updated_at) FROM decisions WHERE realm = ? AND state NOT IN ('open', 'snoozed')", realm).await;
        entries.push(entry("work", "decisions:closed", "Settled decisions", n, last));
        let (n, last) = scalar(st, "SELECT count(*), max(last_run_at) FROM triggers WHERE realm = ?", realm).await;
        entries.push(entry("work", "triggers", "Triggers", n, last));
    }
    // Configuration: the realm's own entries, and root's it inherits.
    for (key, label) in LISTS {
        let own = crate::lists::read(&st.store.pool, key, realm).await?.len() as i64;
        let inherited = if realm == ROOT { 0 } else { crate::lists::read(&st.store.pool, key, ROOT).await?.len() as i64 };
        if own + inherited > 0 {
            entries.push(api::ExEntry { inherited, ..entry("config", format!("list:{key}"), label, own, None) });
        }
    }
    if holds {
        let (n, last) = scalar(st, "SELECT count(*), max(updated_at) FROM records WHERE realm = ? AND collection = 'memory'", realm).await;
        entries.push(entry("config", "memory", "Memory notes", n, last));
        let (n, _) = scalar(st, "SELECT count(*), NULL FROM secrets WHERE realm = ?", realm).await;
        entries.push(entry("config", "secrets", "Secrets", n, None));
        // Flow.
        let (n, _) = scalar(st, "SELECT count(*), NULL FROM grants WHERE to_realm = ? AND state IN ('active', 'requested')", realm).await;
        entries.push(entry("flow", "grants:in", "Grants to this realm", n, None));
        let (n, _) = scalar(st, "SELECT count(*), NULL FROM grants WHERE from_realm = ? AND state IN ('active', 'requested')", realm).await;
        entries.push(entry("flow", "grants:out", "Grants from this realm", n, None));
    }
    // Infrastructure.
    let workers = crate::api::workers_snapshot(st).await.unwrap_or_default();
    let serving = workers
        .iter()
        .filter(|w| w.role != "memex" && w.role != "head" && (realm == ROOT || w.realms.is_empty() || w.realms.iter().any(|r| r == realm)))
        .count();
    entries.push(entry("infrastructure", "workers", "Workers serving it", serving as i64, None));
    let conns = st.store.connections().await?;
    let serving = conns.iter().filter(|c| realm == ROOT || crate::realms::available(&c.realms, realm)).count();
    entries.push(entry("infrastructure", "connections", "Model connections", serving as i64, None));

    // What needs attention.
    let mut health = Vec::new();
    if holds {
        for t in crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await? {
            if let Some(e) = t.last_error.as_deref().filter(|_| t.enabled) {
                health.push(r("trigger", &t.id, format!("Trigger {} is failing", t.name), clip(e, 160), t.last_run_at));
            }
        }
        let crossed = sqlx::query(
            "SELECT t.id, t.name, a.at FROM series_alerts a JOIN triggers t ON t.id = a.trigger_id WHERE t.realm = ? AND a.crossed = 1",
        )
        .bind(realm)
        .fetch_all(&st.store.pool)
        .await?;
        for c in crossed {
            health.push(r(
                "trigger",
                c.get::<String, _>("id"),
                format!("Threshold crossed: {}", c.get::<String, _>("name")),
                "",
                Some(c.get("at")),
            ));
        }
        let waiting =
            sqlx::query("SELECT id, title, updated_at FROM sessions WHERE realm = ? AND awaiting > 0 ORDER BY updated_at DESC LIMIT 10")
                .bind(realm)
                .fetch_all(&st.store.pool)
                .await?;
        for s in waiting {
            health.push(r(
                "session",
                s.get::<String, _>("id"),
                format!("Waiting for approval: {}", s.get::<String, _>("title")),
                "",
                Some(s.get("updated_at")),
            ));
        }
        let open = sqlx::query("SELECT id, title, priority, created_at FROM decisions WHERE realm = ? AND state = 'open' AND priority = 'high' ORDER BY created_at DESC LIMIT 10")
            .bind(realm)
            .fetch_all(&st.store.pool)
            .await?;
        for d in open {
            health.push(r(
                "decision",
                d.get::<i64, _>("id").to_string(),
                format!("Decision: {}", d.get::<String, _>("title")),
                "high priority",
                Some(d.get("created_at")),
            ));
        }
    }
    let spend_month = if holds {
        crate::usage::as_points(st, realm, "model_spend", month_start(), now_ms() + 1).await?.iter().map(|p| p.1).sum()
    } else {
        0.0
    } + 0.0;
    let app = crate::apps::list(st).await?.into_iter().find(|a| a.realm == realm);
    let last_activity = entries.iter().filter_map(|e| e.changed_at).max();
    Ok(api::RealmSummary { realm: info, app, entries, health, spend_month, last_activity, knowledge: inventory.is_some() })
}

/// What happened in a realm since `since`, newest first.
pub async fn activity(st: &HeadState, realm: &str, since: i64, limit: usize) -> anyhow::Result<Vec<api::ExRef>> {
    let mut out = Vec::new();
    let pool = &st.store.pool;
    let rows = sqlx::query(
        "SELECT h.record_id, h.op, h.changed_by, h.changed_at, h.fields, r.collection FROM record_history h JOIN records r ON r.id = h.record_id \
         WHERE r.realm = ? AND h.changed_at >= ? AND r.collection NOT IN (SELECT name FROM collections WHERE realm = r.realm AND system = 1) \
         ORDER BY h.changed_at DESC LIMIT ?",
    )
    .bind(realm)
    .bind(since)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    for h in rows {
        let (col, id): (String, String) = (h.get("collection"), h.get("record_id"));
        let fields: Value = serde_json::from_str(&h.get::<String, _>("fields")).unwrap_or(Value::Null);
        out.push(r(
            "record",
            format!("{col}/{id}"),
            format!("{}: {}", col, record_title(&fields, &id)),
            format!("{}d by {}", h.get::<String, _>("op"), h.get::<String, _>("changed_by")),
            Some(h.get("changed_at")),
        ));
    }
    let rows = sqlx::query("SELECT id, title, state, created_at, answered_at, answered_via FROM decisions WHERE realm = ? AND (created_at >= ?2 OR answered_at >= ?2) ORDER BY updated_at DESC LIMIT ?3")
        .bind(realm)
        .bind(since)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
    for d in rows {
        let id = d.get::<i64, _>("id").to_string();
        let answered: Option<i64> = d.get("answered_at");
        match answered.filter(|a| *a >= since) {
            Some(at) => out.push(r(
                "decision",
                &id,
                d.get::<String, _>("title"),
                format!("{} via {}", d.get::<String, _>("state"), d.get::<String, _>("answered_via")),
                Some(at),
            )),
            None => out.push(r("decision", &id, d.get::<String, _>("title"), "opened", Some(d.get("created_at")))),
        }
    }
    let rows = sqlx::query("SELECT id, title, status, updated_at FROM sessions WHERE realm = ? AND updated_at >= ? AND parent_call_id IS NULL ORDER BY updated_at DESC LIMIT ?")
        .bind(realm)
        .bind(since)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
    for s in rows {
        out.push(r(
            "session",
            s.get::<String, _>("id"),
            s.get::<String, _>("title"),
            format!("session {}", s.get::<String, _>("status")),
            Some(s.get("updated_at")),
        ));
    }
    let rows = sqlx::query(
        "SELECT t.id, t.name, x.outcome, x.trigger, x.started_at FROM trigger_runs x JOIN triggers t ON t.id = x.trigger_id WHERE t.realm = ? AND x.started_at >= ? \
         ORDER BY x.started_at DESC LIMIT ?",
    )
    .bind(realm)
    .bind(since)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    for t in rows {
        out.push(r(
            "trigger",
            t.get::<String, _>("id"),
            format!("Trigger {}", t.get::<String, _>("name")),
            format!("{} run, {}", t.get::<String, _>("trigger"), t.get::<String, _>("outcome")),
            Some(t.get("started_at")),
        ));
    }
    let rows = sqlx::query("SELECT series, count(*) AS n, max(ts) AS last FROM points WHERE realm = ? AND ts >= ? GROUP BY series")
        .bind(realm)
        .bind(since)
        .fetch_all(pool)
        .await?;
    for p in rows {
        let name: String = p.get("series");
        out.push(r("series", &name, format!("Series {name}"), format!("{} points", p.get::<i64, _>("n")), Some(p.get("last"))));
    }
    if let Some(inv) = knowledge(st, realm, "memex.explore.inventory", json!({})).await {
        for i in inv["recent"].as_array().into_iter().flatten().filter(|i| i["added_at"].as_i64().unwrap_or(0) >= since) {
            out.push(r(
                "item",
                i["id"].as_str().unwrap_or(""),
                i["title"].as_str().unwrap_or(""),
                format!("{} added to knowledge", i["kind"].as_str().unwrap_or("")),
                i["added_at"].as_i64(),
            ));
        }
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.at));
    out.truncate(limit);
    Ok(out)
}

/// The objects of one inventory section.
pub async fn list(st: &HeadState, realm: &str, section: &str, q: &str) -> anyhow::Result<Vec<api::ExRef>> {
    let pool = &st.store.pool;
    let like = format!("%{}%", q.replace(['%', '_'], ""));
    let (what, arg) = section.split_once(':').unwrap_or((section, ""));
    let out = match what {
        "collection" => {
            let page = crate::collections::query(
                st,
                realm,
                arg,
                &api::RecordQuery { text: q.to_owned(), sort: "-updated_at".into(), limit: Some(500), ..Default::default() },
            )
            .await?;
            page.records
                .iter()
                .map(|x| r("record", format!("{arg}/{}", x.id), record_title(&x.fields, &x.id), format!("v{} · {}", x.version, x.updated_by), Some(x.updated_at)))
                .collect()
        }
        "series" => crate::series::list(st, realm)
            .await?
            .into_iter()
            .filter(|s| s.name.contains(q))
            .map(|s| {
                let sub = match &s.last {
                    Some(p) => format!("{} points · last {} {}", s.points, p.value, s.unit),
                    None => format!("{} points{}", s.points, if s.computed { " (from model usage)" } else { "" }),
                };
                r("series", &s.name, &s.name, sub, s.last.as_ref().map(|p| p.ts))
            })
            .collect(),
        "things" | "items" | "facts" => {
            let v = knowledge(st, realm, "memex.explore.list", json!({"what": what, "kind": arg, "q": q, "limit": 300}))
                .await
                .ok_or_else(|| bad_request("this realm's knowledge cannot be read now (is a memex connected?)"))?;
            let kind = match what {
                "things" => "thing",
                "items" => "item",
                _ => "fact",
            };
            v.as_array()
                .into_iter()
                .flatten()
                .map(|x| r(kind, x["id"].as_str().unwrap_or(""), x["title"].as_str().unwrap_or(""), x["sub"].as_str().unwrap_or(""), x["at"].as_i64()))
                .collect()
        }
        "sessions" => sqlx::query(
            "SELECT id, title, status, updated_at, labels FROM sessions WHERE realm = ? AND parent_call_id IS NULL AND title LIKE ? ORDER BY updated_at DESC LIMIT 300",
        )
        .bind(realm)
        .bind(&like)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|s| {
            let labels: Vec<String> = serde_json::from_str(&s.get::<String, _>("labels")).unwrap_or_default();
            r("session", s.get::<String, _>("id"), s.get::<String, _>("title"), format!("{} · {}", s.get::<String, _>("status"), labels.join(", ")), Some(s.get("updated_at")))
        })
        .collect(),
        "decisions" => {
            let open = arg != "closed";
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT id, title, kind, state, updated_at FROM decisions WHERE realm = ? AND title LIKE ? AND state {} ('open', 'snoozed') ORDER BY updated_at DESC LIMIT 300",
                if open { "IN" } else { "NOT IN" }
            )))
            .bind(realm)
            .bind(&like)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|d| r("decision", d.get::<i64, _>("id").to_string(), d.get::<String, _>("title"), format!("{} · {}", d.get::<String, _>("kind"), d.get::<String, _>("state")), Some(d.get("updated_at"))))
            .collect()
        }
        "triggers" => crate::triggers::list(st, &crate::realms::Caller::in_realm(realm))
            .await?
            .into_iter()
            .filter(|t| t.name.contains(q))
            .map(|t| {
                let does = if t.action == "start" { format!("start {}", t.template) } else { t.action.clone() };
                r("trigger", &t.id, &t.name, format!("{} → {does}{}", t.trigger, if t.enabled { "" } else { " (off)" }), t.last_run_at)
            })
            .collect(),
        "list" => {
            let l = crate::lists::list(arg).ok_or_else(|| bad_request(format!("no list {arg}")))?;
            let mut out: Vec<api::ExRef> = Vec::new();
            for (from, entries) in [(realm, crate::lists::read(pool, arg, realm).await?), (ROOT, if realm == ROOT { Vec::new() } else { crate::lists::read(pool, arg, ROOT).await? })] {
                for e in entries {
                    let name = e["name"].as_str().or(e["path"].as_str()).unwrap_or("").to_owned();
                    let id = crate::lists::identity(l, &e);
                    let sub = e["description"].as_str().or(e["title"].as_str()).or(e["kind"].as_str()).unwrap_or("").to_owned();
                    out.push(r("entry", format!("{arg}/{from}/{id}"), &name, if from == realm { sub } else { format!("from root · {sub}") }, None));
                }
            }
            out.into_iter().filter(|x| x.title.contains(q)).collect()
        }
        "memory" => sqlx::query("SELECT id, fields, updated_at FROM records WHERE realm = ? AND collection = 'memory' ORDER BY updated_at DESC")
            .bind(realm)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|m| {
                let f: Value = serde_json::from_str(&m.get::<String, _>("fields")).unwrap_or(Value::Null);
                r("record", format!("memory/{}", m.get::<String, _>("id")), clip(f["text"].as_str().unwrap_or(""), 120), f["scope"].as_str().unwrap_or("every session"), Some(m.get("updated_at")))
            })
            .collect(),
        "secrets" => sqlx::query("SELECT name, description FROM secrets WHERE realm = ? ORDER BY name")
            .bind(realm)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|s| r("secret", s.get::<String, _>("name"), s.get::<String, _>("name"), s.get::<String, _>("description"), None))
            .collect(),
        "grants" => crate::grants::list(st, Some(realm))
            .await?
            .into_iter()
            .filter(|g| if arg == "out" { g.from_realm == realm } else { g.to_realm == realm })
            .filter(|g| matches!(g.state.as_str(), "active" | "requested"))
            .map(|g| r("grant", &g.id, format!("{} → {}: {} {}", g.from_realm, g.to_realm, g.mode, crate::grants::describe(&g)), format!("{} · {} uses", g.state, g.uses), Some(g.created_at)))
            .collect(),
        "workers" => crate::api::workers_snapshot(st)
            .await?
            .into_iter()
            .filter(|w| w.role != "memex" && w.role != "head" && (realm == ROOT || w.realms.is_empty() || w.realms.iter().any(|x| x == realm)))
            .map(|w| r("worker", w.id.to_string(), &w.name, format!("{:?}{}", w.status, if w.realms.is_empty() { String::new() } else { format!(" · only {}", w.realms.join(", ")) }), None))
            .collect(),
        "connections" => st
            .store
            .connections()
            .await?
            .into_iter()
            .filter(|c| realm == ROOT || crate::realms::available(&c.realms, realm))
            .map(|c| r("connection", &c.id, &c.name, &c.kind, None))
            .collect(),
        other => return Err(bad_request(format!("no section {other}"))),
    };
    Ok(out)
}

fn group(label: &str, items: Vec<api::ExRef>) -> Option<api::ExGroup> {
    (!items.is_empty()).then(|| {
        let more = items.len().saturating_sub(30) as i64;
        api::ExGroup { label: label.into(), items: items.into_iter().take(30).collect(), more }
    })
}

/// A thing in the knowledge that a record link (`client:acme`) names, by
/// its key.
fn link_refs(links: &[String], realm: &str) -> (Vec<api::ExRef>, Vec<api::ExRef>) {
    let mut things = Vec::new();
    let mut records = Vec::new();
    for l in links {
        if let Some(rest) = l.strip_prefix("record:") {
            // record:<realm>/<id> (copies) or record:<id>
            let id = rest.split_once('/').filter(|(r, _)| *r == realm).map(|(_, i)| i).unwrap_or(rest);
            records.push(r("record", format!("?/{id}"), id, "linked record", None));
        } else if l.contains(':') {
            things.push(r("thing", l.as_str(), l.as_str(), "", None));
        }
    }
    (things, records)
}

/// Sessions labelled with a label.
async fn sessions_labelled(st: &HeadState, realm: &str, label: &str) -> anyhow::Result<Vec<api::ExRef>> {
    let like = format!("%\"{}\"%", label.replace('"', ""));
    Ok(sqlx::query(
        "SELECT id, title, status, updated_at FROM sessions WHERE realm = ? AND labels LIKE ? ORDER BY updated_at DESC LIMIT 50",
    )
    .bind(realm)
    .bind(like)
    .fetch_all(&st.store.pool)
    .await?
    .iter()
    .map(|s| r("session", s.get::<String, _>("id"), s.get::<String, _>("title"), s.get::<String, _>("status"), Some(s.get("updated_at"))))
    .collect())
}

/// Decisions whose context or reference mentions `needle`.
async fn decisions_mentioning(st: &HeadState, realm: &str, needle: &str) -> anyhow::Result<Vec<api::ExRef>> {
    let like = format!("%{}%", needle.replace(['%', '_'], ""));
    Ok(sqlx::query("SELECT id, title, state, updated_at FROM decisions WHERE realm = ? AND (context LIKE ?2 OR ref LIKE ?2 OR source LIKE ?2) ORDER BY updated_at DESC LIMIT 50")
        .bind(realm)
        .bind(like)
        .fetch_all(&st.store.pool)
        .await?
        .iter()
        .map(|d| r("decision", d.get::<i64, _>("id").to_string(), d.get::<String, _>("title"), d.get::<String, _>("state"), Some(d.get("updated_at"))))
        .collect())
}

fn trigger_ref(t: &api::Automation) -> api::ExRef {
    r(
        "trigger",
        &t.id,
        &t.name,
        format!("{} → {}", t.trigger, if t.action == "start" { format!("start {}", t.template) } else { t.action.clone() }),
        t.last_run_at,
    )
}

fn pretty(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn field_list(v: &Value) -> Vec<(String, String)> {
    v.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), pretty(v))).collect()).unwrap_or_default()
}

/// One object, with everything connected to it.
pub async fn object(st: &HeadState, realm: &str, kind: &str, id: &str) -> anyhow::Result<api::ExObject> {
    let pool = &st.store.pool;
    let triggers = if realm == ROOT { Vec::new() } else { crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await? };
    let views = crate::views::effective(st, realm).await.unwrap_or_default();
    let mut o = api::ExObject { kind: kind.into(), id: id.into(), ..Default::default() };
    let mut groups: Vec<Option<api::ExGroup>> = Vec::new();
    match kind {
        "collection" => {
            let c = crate::collections::get(st, realm, id).await?;
            o.title = c.name.clone();
            o.sub = format!("{} records", c.records);
            o.body = c.description.clone();
            let required: Vec<String> = serde_json::from_value(c.schema["required"].clone()).unwrap_or_default();
            o.fields = c.schema["properties"]
                .as_object()
                .map(|p| {
                    p.iter()
                        .map(|(k, v)| {
                            let ty = v["enum"]
                                .as_array()
                                .map(|e| format!("one of {}", e.iter().map(pretty).collect::<Vec<_>>().join(", ")))
                                .unwrap_or_else(|| pretty(&v["type"]));
                            (
                                format!("{k}{}", if required.contains(k) { " *" } else { "" }),
                                format!("{ty}{}", v["description"].as_str().map(|d| format!(" — {d}")).unwrap_or_default()),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !c.keys.is_empty() {
                o.fields.push(("unique".into(), c.keys.join(", ")));
            }
            o.open = Some(format!("/collections/{}", c.name));
            let recs = list(st, realm, &format!("collection:{id}"), "").await?;
            groups.push(group("Records", recs));
            groups.push(group(
                "Views showing it",
                views
                    .iter()
                    .filter(|v| v.source["collection"] == id)
                    .map(|v| r("view", &v.name, if v.title.is_empty() { &v.name } else { &v.title }, &v.kind, None))
                    .collect(),
            ));
            groups.push(group(
                "Triggers on it",
                triggers
                    .iter()
                    .filter(|t| t.event_params["collection"] == id || t.action_params["collection"] == id)
                    .map(trigger_ref)
                    .collect(),
            ));
            groups.push(group(
                "Grants on it",
                crate::grants::list(st, Some(realm))
                    .await?
                    .into_iter()
                    .filter(|g| g.kind == "collection" && g.scope["collection"] == id && g.from_realm == realm)
                    .map(|g| r("grant", &g.id, format!("{} → {}", g.from_realm, g.to_realm), format!("{} · {}", g.mode, g.state), None))
                    .collect(),
            ));
        }
        "record" => {
            let (col, rid) = id.split_once('/').ok_or_else(|| bad_request("a record is <collection>/<id>"))?;
            let rec = if col == "?" {
                let c: String = sqlx::query_scalar("SELECT collection FROM records WHERE id = ? AND realm = ?")
                    .bind(rid)
                    .bind(realm)
                    .fetch_optional(pool)
                    .await?
                    .ok_or_else(|| not_found("no such record"))?;
                crate::collections::record(st, realm, &c, rid).await?
            } else {
                crate::collections::record(st, realm, col, rid).await?
            };
            o.id = format!("{}/{}", rec.collection, rec.id);
            o.title = record_title(&rec.fields, &rec.id);
            o.sub = format!("{} · v{} · by {}", rec.collection, rec.version, rec.updated_by);
            o.fields = field_list(&rec.fields);
            if !rec.labels.is_empty() {
                o.fields.push(("labels".into(), rec.labels.join(", ")));
            }
            o.open = Some(format!("/collections/{}", rec.collection));
            o.data = json!(rec);
            let (things, records) = link_refs(&rec.links, realm);
            groups.push(group("Collection", vec![r("collection", &rec.collection, &rec.collection, "", None)]));
            groups.push(group("Things it links to", things));
            groups.push(group("Records it links to", records));
            let linking =
                sqlx::query("SELECT id, collection, fields, updated_at FROM records WHERE realm = ? AND links LIKE ? AND id != ? LIMIT 50")
                    .bind(realm)
                    .bind(format!("%{}%", rec.id))
                    .bind(&rec.id)
                    .fetch_all(pool)
                    .await?;
            groups.push(group(
                "Records linking here",
                linking
                    .iter()
                    .map(|x| {
                        let (c, i): (String, String) = (x.get("collection"), x.get("id"));
                        let f: Value = serde_json::from_str(&x.get::<String, _>("fields")).unwrap_or(Value::Null);
                        r("record", format!("{c}/{i}"), record_title(&f, &i), c, Some(x.get("updated_at")))
                    })
                    .collect(),
            ));
            groups.push(group("Sessions about it", sessions_labelled(st, realm, &format!("record:{}", rec.id)).await?));
            groups.push(group("Decisions about it", decisions_mentioning(st, realm, &rec.id).await?));
            let mut writers = Vec::new();
            for by in [&rec.created_by, &rec.updated_by] {
                if let Some(s) = by.split_whitespace().find_map(|w| w.strip_prefix("session:")) {
                    if let Ok(Some(row)) = st.store.session(s).await {
                        writers.push(r("session", &row.id, &row.title, "wrote it", Some(row.updated_at)));
                    }
                }
                if let Some(t) = by.split_whitespace().find_map(|w| w.strip_prefix("trigger:")) {
                    if let Some(t) = triggers.iter().find(|x| x.name.replace(' ', "-") == t) {
                        writers.push(r("trigger", &t.id, &t.name, "wrote it", None));
                    }
                }
            }
            writers.dedup_by(|a, b| a.id == b.id);
            groups.push(group("Written by", writers));
            groups.push(group(
                "Triggers watching its collection",
                triggers
                    .iter()
                    .filter(|t| t.trigger == "record" && t.event_params["collection"] == rec.collection.as_str())
                    .map(trigger_ref)
                    .collect(),
            ));
            let history = crate::collections::changes(st, realm, &rec.collection, &rec.id).await?;
            o.body = history
                .iter()
                .rev()
                .take(20)
                .map(|h| {
                    format!(
                        "- v{} {} by {} ({})",
                        h.version,
                        h.op,
                        h.changed_by,
                        chrono::DateTime::from_timestamp_millis(h.changed_at)
                            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !o.body.is_empty() {
                o.body = format!("**History**\n\n{}", o.body);
            }
        }
        "series" => {
            let s = crate::series::get(st, realm, id).await?;
            o.title = s.name.clone();
            o.sub = format!(
                "{} · {} points{}",
                if s.unit.is_empty() { "no unit" } else { &s.unit },
                s.points,
                if s.computed { " · from model usage" } else { "" }
            );
            o.body = s.description.clone();
            o.fields = vec![
                ("dimensions".into(), s.dimensions.join(", ")),
                ("retention".into(), if s.retention_days == 0 { "kept".into() } else { format!("{} days", s.retention_days) }),
            ];
            o.open = Some("/collections".into());
            o.data = json!(s);
            groups.push(group(
                "Triggers watching it",
                triggers.iter().filter(|t| t.trigger == "series" && t.event_params["series"] == id).map(trigger_ref).collect(),
            ));
            groups.push(group(
                "Triggers recording into it",
                triggers.iter().filter(|t| t.action == "record" && t.action_params["series"] == id).map(trigger_ref).collect(),
            ));
            groups.push(group(
                "Views charting it",
                views
                    .iter()
                    .filter(|v| v.source["series"] == id)
                    .map(|v| r("view", &v.name, if v.title.is_empty() { &v.name } else { &v.title }, &v.kind, None))
                    .collect(),
            ));
        }
        "thing" => {
            let v = knowledge(st, realm, "memex.explore.thing", json!({"id": id}))
                .await
                .ok_or_else(|| not_found(format!("no thing {id} in this realm's knowledge")))?;
            let t = &v["thing"];
            o.id = t["id"].as_str().unwrap_or(id).to_owned();
            o.title = t["name"].as_str().unwrap_or(id).to_owned();
            o.sub = format!("{} · {}", t["kind"].as_str().unwrap_or(""), t["label"].as_str().unwrap_or(""));
            o.open = Some("/memex".into());
            o.data = v.clone();
            groups.push(group(
                "Facts about it",
                v["facts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| {
                        r(
                            "fact",
                            f["id"].as_str().unwrap_or(""),
                            f["claim"].as_str().unwrap_or(""),
                            f["status"].as_str().unwrap_or(""),
                            None,
                        )
                    })
                    .collect(),
            ));
            groups.push(group(
                "Items about it",
                v["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|i| {
                        r(
                            "item",
                            i["id"].as_str().unwrap_or(""),
                            i["title"].as_str().unwrap_or(""),
                            i["kind"].as_str().unwrap_or(""),
                            i["created_at"].as_i64(),
                        )
                    })
                    .collect(),
            ));
            groups.push(group(
                "Related things",
                v["related"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| {
                        r(
                            "thing",
                            x["thing"]["id"].as_str().unwrap_or(""),
                            x["thing"]["name"].as_str().unwrap_or(""),
                            format!("{} · {} shared", x["thing"]["kind"].as_str().unwrap_or(""), x["shared"]),
                            None,
                        )
                    })
                    .collect(),
            ));
            // Records whose links name it.
            let key = t["label"].as_str().unwrap_or("").to_owned();
            if !key.is_empty() {
                let rows = sqlx::query("SELECT id, collection, fields, updated_at FROM records WHERE realm = ? AND links LIKE ? LIMIT 50")
                    .bind(realm)
                    .bind(format!("%\"{key}\"%"))
                    .fetch_all(pool)
                    .await?;
                groups.push(group(
                    "Records linking to it",
                    rows.iter()
                        .map(|x| {
                            let (c, i): (String, String) = (x.get("collection"), x.get("id"));
                            let f: Value = serde_json::from_str(&x.get::<String, _>("fields")).unwrap_or(Value::Null);
                            r("record", format!("{c}/{i}"), record_title(&f, &i), c, Some(x.get("updated_at")))
                        })
                        .collect(),
                ));
                groups.push(group("Sessions labelled with it", sessions_labelled(st, realm, &key).await?));
            }
        }
        "item" => {
            let v = knowledge(st, realm, "memex.get", json!({"id": id}))
                .await
                .ok_or_else(|| not_found(format!("no item {id} in this realm's knowledge")))?;
            let i = &v["item"];
            o.title = i["title"].as_str().unwrap_or(id).to_owned();
            o.sub = format!("{} · {}", i["kind"].as_str().unwrap_or(""), i["source"].as_str().unwrap_or(""));
            o.body = clip(i["text"].as_str().unwrap_or(""), 4000);
            o.open = Some("/memex".into());
            o.data = v.clone();
            groups.push(group(
                "About",
                i["about"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| {
                        r("thing", t["id"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""), t["kind"].as_str().unwrap_or(""), None)
                    })
                    .collect(),
            ));
            // A record's mirror leads back to the record; a session's digest
            // to the session.
            let source = i["source"].as_str().unwrap_or("");
            let ext = i["external_id"].as_str().unwrap_or("");
            if let Some(col) = source.strip_prefix("collection:") {
                groups.push(group("The record", vec![r("record", format!("{col}/{ext}"), ext, col, None)]));
            }
            if source == "session" || source.starts_with("session") {
                if let Ok(Some(s)) = st.store.session(ext).await {
                    groups.push(group("The session", vec![r("session", &s.id, &s.title, &s.status, Some(s.updated_at))]));
                }
            }
        }
        "fact" => {
            let v = knowledge(st, realm, "memex.knowledge.get", json!({"ids": [id]})).await.ok_or_else(|| not_found("no such fact"))?;
            let f = v.get(0).cloned().ok_or_else(|| not_found("no such fact"))?;
            o.title = f["claim"].as_str().unwrap_or("").to_owned();
            o.sub = format!("{} · {}", f["status"].as_str().unwrap_or(""), f["volatility"].as_str().unwrap_or(""));
            o.body = f["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    format!(
                        "- {} {}{}",
                        pretty(&e["by"]),
                        pretty(&e["command"]),
                        e["excerpt"].as_str().map(|x| format!(": {}", clip(x, 200))).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !o.body.is_empty() {
                o.body = format!("**Evidence**\n\n{}", o.body);
            }
            o.fields = vec![("conditions".into(), pretty(&f["conditions"])), ("made by".into(), pretty(&f["made_by"]))];
            o.open = Some("/memex".into());
            o.data = f.clone();
            groups.push(group(
                "About",
                f["about"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|k| r("thing", k, k, "", None)).collect(),
            ));
            if let Some(s) = f["made_by"].as_str().and_then(|m| m.strip_prefix("session:")) {
                if let Ok(Some(s)) = st.store.session(s).await {
                    groups.push(group("Recorded by", vec![r("session", &s.id, &s.title, &s.status, Some(s.updated_at))]));
                }
            }
            groups.push(group("Decisions about it", decisions_mentioning(st, realm, id).await?));
        }
        "session" => {
            let s = st.store.session(id).await?.filter(|s| s.realm == realm).ok_or_else(|| not_found("no such session in this realm"))?;
            o.title = s.title.clone();
            o.sub = format!("{} · {} · {}", s.status, s.worker_name, s.model);
            o.fields = vec![("labels".into(), s.all_labels().join(", ")), ("folder".into(), s.cwd.clone())];
            let (cost, calls) = crate::usage::cost_of(st, &[s.id.clone()]).await.unwrap_or((0.0, 0));
            o.fields.push(("cost".into(), format!("${cost:.2} ({calls} calls)")));
            o.open = Some(format!("/sessions/{}", s.id));
            let wrote = sqlx::query(
                "SELECT DISTINCT r.id, r.collection, r.fields, r.updated_at FROM record_history h JOIN records r ON r.id = h.record_id \
                 WHERE r.realm = ? AND h.changed_by LIKE ? LIMIT 50",
            )
            .bind(realm)
            .bind(format!("%session:{}%", s.id))
            .fetch_all(pool)
            .await?;
            groups.push(group(
                "Records it wrote",
                wrote
                    .iter()
                    .map(|x| {
                        let (c, i): (String, String) = (x.get("collection"), x.get("id"));
                        let f: Value = serde_json::from_str(&x.get::<String, _>("fields")).unwrap_or(Value::Null);
                        r("record", format!("{c}/{i}"), record_title(&f, &i), c, Some(x.get("updated_at")))
                    })
                    .collect(),
            ));
            groups.push(group("Decisions it raised", decisions_mentioning(st, realm, &format!("session:{}", s.id)).await?));
            let started = sqlx::query("SELECT t.id FROM run_sessions rs JOIN trigger_runs x ON x.run_id = rs.run_id JOIN triggers t ON t.id = x.trigger_id WHERE rs.session_id = ?")
                .bind(&s.id)
                .fetch_all(pool)
                .await?;
            let ids: Vec<String> = started.iter().map(|x| x.get("id")).collect();
            groups.push(group("Started by", triggers.iter().filter(|t| ids.contains(&t.id)).map(trigger_ref).collect()));
            let records: Vec<api::ExRef> = s
                .all_labels()
                .iter()
                .filter_map(|l| l.strip_prefix("record:"))
                .map(|rid| r("record", format!("?/{rid}"), rid, "the record it is about", None))
                .collect();
            groups.push(group("About", records));
        }
        "decision" => {
            let d = crate::decisions::get(st, id.parse().map_err(|_| bad_request("a decision's id is a number"))?)
                .await?
                .filter(|d| d.realm == realm)
                .ok_or_else(|| not_found("no such decision in this realm"))?;
            o.title = d.title.clone();
            o.sub = format!(
                "{} · {}{}",
                d.kind,
                d.state,
                d.answer.as_ref().and_then(|a| a["option"].as_str()).map(|x| format!(" · {x}")).unwrap_or_default()
            );
            o.body = d.document.clone();
            o.fields =
                vec![("source".into(), d.source.clone()), ("group".into(), d.group.clone()), ("priority".into(), d.priority.clone())];
            o.open = Some("/decisions".into());
            o.data = json!(d);
            let mut about = Vec::new();
            let refstr: String =
                sqlx::query_scalar("SELECT ref FROM decisions WHERE id = ?").bind(d.id).fetch_one(pool).await.unwrap_or_default();
            let mut parts = refstr.splitn(3, ':');
            match (parts.next(), parts.next()) {
                (Some("approval" | "session" | "question"), Some(s)) => {
                    if let Ok(Some(s)) = st.store.session(s).await {
                        about.push(r("session", &s.id, &s.title, "waits on it", Some(s.updated_at)));
                    }
                }
                (Some("grant"), Some(g)) => about.push(r("grant", g, format!("Grant {g}"), "", None)),
                _ => {}
            }
            if let (Some(c), Some(i)) = (d.context["collection"].as_str(), d.context["id"].as_str()) {
                about.push(r("record", format!("{c}/{i}"), i, c, None));
            }
            groups.push(group("About", about));
            let mut from = Vec::new();
            if let Some(s) = d.source.strip_prefix("session:") {
                if let Ok(Some(s)) = st.store.session(s).await {
                    from.push(r("session", &s.id, &s.title, "asked", Some(s.updated_at)));
                }
            }
            if let Some(t) = d.context["trigger"].as_str().and_then(|t| triggers.iter().find(|x| x.id == t)) {
                from.push(trigger_ref(t));
            }
            groups.push(group("Opened by", from));
            groups.push(group(
                "Triggers listening",
                triggers
                    .iter()
                    .filter(|t| {
                        t.trigger == "decision"
                            && (t.filter["$.decision.group"].is_null() || t.filter["$.decision.group"] == d.group.as_str())
                    })
                    .map(trigger_ref)
                    .collect(),
            ));
        }
        "trigger" => {
            let t = triggers.iter().find(|t| t.id == id).cloned().ok_or_else(|| not_found("no such trigger in this realm"))?;
            o.title = t.name.clone();
            o.sub = format!(
                "{} → {}{}",
                t.trigger,
                if t.action == "start" { format!("start {}", t.template) } else { t.action.clone() },
                if t.enabled { "" } else { " · off" }
            );
            o.fields = vec![
                (
                    "event".into(),
                    if t.cron.is_empty() {
                        t.event_params.to_string()
                    } else {
                        format!(
                            "{} {}",
                            t.cron,
                            if t.event_params.as_object().is_some_and(|m| !m.is_empty()) {
                                t.event_params.to_string()
                            } else {
                                String::new()
                            }
                        )
                    },
                ),
                ("filter".into(), t.filter.to_string()),
                ("action".into(), format!("{} {}", t.action, t.action_params)),
                ("limit".into(), format!("{} runs an hour", t.per_hour)),
            ];
            if let Some(e) = &t.last_error {
                o.fields.push(("last error".into(), e.clone()));
            }
            o.open = Some("/settings/automation".into());
            o.data = json!(t);
            let mut targets = Vec::new();
            for c in [t.event_params["collection"].as_str(), t.action_params["collection"].as_str()].into_iter().flatten() {
                targets.push(r("collection", c, c, "", None));
            }
            for s in [t.event_params["series"].as_str(), t.action_params["series"].as_str()].into_iter().flatten() {
                targets.push(r("series", s, s, "", None));
            }
            if t.action == "start" {
                targets.push(r("entry", format!("templates/{realm}/\"{}\"", t.template), &t.template, "template", None));
            }
            groups.push(group("Involves", targets));
            let runs = crate::triggers::runs(st, &t.id, 50).await?;
            let mut upstream: Vec<String> = runs.iter().flat_map(|x| x.cause.last().cloned()).collect();
            upstream.sort();
            upstream.dedup();
            groups.push(group("Caused by", triggers.iter().filter(|x| upstream.contains(&x.id)).map(trigger_ref).collect()));
            let downstream: Vec<String> = sqlx::query_scalar("SELECT DISTINCT trigger_id FROM trigger_runs WHERE cause LIKE ?")
                .bind(format!("%\"{}\"]%", t.id))
                .fetch_all(pool)
                .await?;
            groups.push(group("Causes", triggers.iter().filter(|x| downstream.contains(&x.id)).map(trigger_ref).collect()));
            groups.push(group(
                "Recent runs' sessions",
                runs.iter()
                    .flat_map(|x| x.sessions.iter().map(|s| r("session", &s.id, &s.title, &s.outcome, Some(x.started_at))))
                    .collect(),
            ));
            o.body = runs
                .iter()
                .take(10)
                .map(|x| {
                    format!(
                        "- {} · {} · {}{}",
                        chrono::DateTime::from_timestamp_millis(x.started_at)
                            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_default(),
                        x.trigger,
                        x.outcome,
                        x.error.as_deref().map(|e| format!(": {}", clip(e, 120))).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !o.body.is_empty() {
                o.body = format!("**Recent runs**\n\n{}", o.body);
            }
        }
        "grant" => {
            let g = crate::grants::get(st, id)
                .await?
                .filter(|g| g.from_realm == realm || g.to_realm == realm)
                .ok_or_else(|| not_found("no such grant for this realm"))?;
            o.title = format!("{} → {}", g.from_realm, g.to_realm);
            o.sub = format!("{} {} · {}", g.mode, crate::grants::describe(&g), g.state);
            o.body = g.note.clone();
            o.open = Some("/settings/system".into());
            o.data = json!(g);
            groups.push(group(
                "Realms",
                [&g.from_realm, &g.to_realm]
                    .iter()
                    .map(|x| r("realm", x.as_str(), x.as_str(), if *x == &g.from_realm { "gives" } else { "receives" }, None))
                    .collect(),
            ));
            if let Some(c) = g.scope["collection"].as_str().filter(|_| g.from_realm == realm) {
                groups.push(group("Opens", vec![r("collection", c, c, "", None)]));
            }
            o.body = format!(
                "{}\n\n**Uses**\n\n{}",
                o.body,
                crate::grants::uses(st, &g.id)
                    .await?
                    .iter()
                    .take(20)
                    .map(|u| format!("- {} · {}", u.by, u.what))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        "view" => {
            let v = views.iter().find(|v| v.name == id).cloned().ok_or_else(|| not_found("no such view"))?;
            o.title = if v.title.is_empty() { v.name.clone() } else { v.title.clone() };
            o.sub = format!("{} view", v.kind);
            o.fields = vec![("source".into(), v.source.to_string())];
            o.body = v.body.clone();
            o.open = Some(format!("/views/{}", v.name));
            o.data = json!(v);
            let mut shows = Vec::new();
            if let Some(c) = v.source["collection"].as_str() {
                shows.push(r("collection", c, c, "", None));
            }
            if let Some(s) = v.source["series"].as_str() {
                shows.push(r("series", s, s, "", None));
            }
            groups.push(group("Shows", shows));
        }
        "entry" => {
            // <list>/<realm it is in>/<identity>
            let mut parts = id.splitn(3, '/');
            let (key, from, ident) = (parts.next().unwrap_or(""), parts.next().unwrap_or(realm), parts.next().unwrap_or(""));
            let l = crate::lists::list(key).ok_or_else(|| bad_request(format!("no list {key}")))?;
            let e = crate::lists::read(pool, key, from)
                .await?
                .into_iter()
                .find(|e| crate::lists::identity(l, e) == ident || e["name"].as_str() == Some(ident.trim_matches('"')))
                .ok_or_else(|| not_found("no such entry"))?;
            o.title = e["name"].as_str().or(e["path"].as_str()).unwrap_or(key).to_owned();
            o.sub = format!("{key}{}", if from != realm { " · from root" } else { "" });
            o.body = e["body"].as_str().or(e["prompt"].as_str()).unwrap_or("").to_owned();
            o.fields = field_list(&e).into_iter().filter(|(k, _)| k != "body" && k != "prompt" && k != "name").collect();
            o.open = Some(match key {
                "views" => "/settings/views".into(),
                "templates" | "workflows" => "/settings/automation".into(),
                "policies" => "/settings/rules".into(),
                "budgets" | "prices" => "/settings/money".into(),
                "routes" => "/settings".into(),
                "prompts" => "/settings/prompts".into(),
                _ => "/settings/tools".into(),
            });
            o.data = e.clone();
            if key == "templates" {
                let name = o.title.clone();
                groups.push(group(
                    "Started by",
                    triggers.iter().filter(|t| t.action == "start" && t.template == name).map(trigger_ref).collect(),
                ));
                groups.push(group("Sessions", sessions_labelled(st, realm, &format!("template:{name}")).await?));
            }
        }
        "secret" => {
            o.title = id.to_owned();
            o.sub = "secret (its value is never shown)".into();
            o.open = Some("/settings/tools".into());
        }
        "realm" => {
            let s = summary(st, id).await?;
            o.title = s.realm.name.clone();
            o.sub = s.realm.id.clone();
            o.body = s.realm.description.clone();
            o.open = Some(format!("/realms/{}", s.realm.id));
        }
        "worker" | "connection" => {
            let refs = list(st, realm, if kind == "worker" { "workers" } else { "connections" }, "").await?;
            let x = refs.into_iter().find(|x| x.id == id).ok_or_else(|| not_found(format!("no such {kind}")))?;
            o.title = x.title;
            o.sub = x.sub;
            o.open = Some(if kind == "worker" { "/workers".into() } else { "/settings".into() });
        }
        other => return Err(bad_request(format!("no kind {other}"))),
    }
    o.connected = groups.into_iter().flatten().collect();
    Ok(o)
}

/// The realms and the grants between them, for the map.
pub async fn map(st: &HeadState) -> anyhow::Result<Value> {
    let realms = crate::realms::list(st, &crate::realms::Caller::root()).await?;
    let mut cards = Vec::new();
    for realm in &realms {
        let s = summary(st, &realm.id).await?;
        let sum = |f: &dyn Fn(&api::ExEntry) -> bool| s.entries.iter().filter(|e| f(e)).map(|e| e.count).sum::<i64>();
        let counts: HashMap<&str, i64> = [
            ("collections", s.entries.iter().filter(|e| e.section.starts_with("collection:")).count() as i64),
            ("records", sum(&|e| e.section.starts_with("collection:"))),
            ("things", sum(&|e| e.section.starts_with("things:"))),
            ("items", sum(&|e| e.section.starts_with("items:"))),
            ("sessions", sum(&|e| e.section == "sessions")),
            ("open_decisions", sum(&|e| e.section == "decisions:open")),
            ("triggers", sum(&|e| e.section == "triggers")),
        ]
        .into_iter()
        .collect();
        cards.push(json!({
            "realm": realm, "app": s.app, "spend_month": s.spend_month, "last_activity": s.last_activity,
            "health": s.health.len(), "counts": counts, "commons": realm.id == COMMONS,
        }));
    }
    let grants: Vec<api::Grant> =
        crate::grants::list(st, None).await?.into_iter().filter(|g| matches!(g.state.as_str(), "active" | "requested")).collect();
    Ok(json!({"realms": cards, "grants": grants}))
}
