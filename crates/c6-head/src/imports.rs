//! Sessions imported from Claude Code and Codex (docs/imports.md): workers
//! you turn imports on for read their session files and send the events;
//! the head keeps them as sessions of that worker, in the realm their
//! folder maps to. They are history: storing them starts no trigger,
//! notification or budget count. Once continued in carbon6, a session is
//! carbon6's, and imports leave it alone.

use base64::Engine;
use c6_api as api;
use c6_core::WorkerId;
use prost::Message;
use serde_json::{Value, json};
use sqlx::Row;

use crate::store::SessionRow;
use crate::{HeadState, bad_request};

/// The setting: which workers import, from which tools, into which realms.
pub const KEY: &str = "session_imports";
pub const SOURCES: [&str; 2] = ["claude_code", "codex"];

pub async fn settings(st: &HeadState) -> anyhow::Result<api::SessionImports> {
    Ok(st.store.setting(KEY).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

pub async fn set_settings(st: &HeadState, s: &api::SessionImports) -> anyhow::Result<()> {
    for src in &s.sources {
        if !SOURCES.contains(&src.as_str()) {
            return Err(bad_request(format!("{src:?}: sources are claude_code and codex")));
        }
    }
    for r in &s.rules {
        if r.folder.trim().is_empty() || !crate::realms::exists(st, &r.realm).await? {
            return Err(bad_request(format!("a rule is a folder and a realm that exists ({:?} → {:?})", r.folder, r.realm)));
        }
    }
    st.store.set_setting(KEY, &serde_json::to_string(s)?).await
}

/// `imports.config`: whether this worker imports, and from what.
pub async fn config(st: &HeadState, from: WorkerId) -> anyhow::Result<Value> {
    let s = settings(st).await?;
    let me = st.store.worker(from).await?.map(|w| w.name).unwrap_or_default();
    let enabled = s.workers.iter().any(|w| *w == me || *w == from.to_string());
    let sources = if s.sources.is_empty() { SOURCES.iter().map(|x| x.to_string()).collect() } else { s.sources };
    Ok(json!({"enabled": enabled, "sources": sources}))
}

/// `imports.status`: how much of a session the head has, and whether it
/// was continued in carbon6 since.
pub async fn status(st: &HeadState, from: WorkerId, id: &str) -> anyhow::Result<Value> {
    let Some(r) = sqlx::query("SELECT i.imported_seq, s.last_seq, s.worker_id FROM session_imports i JOIN sessions s ON s.id = i.session_id WHERE i.session_id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
    else {
        // Not imported; a carbon6 session by that id (unlikely) is never touched.
        let exists = st.store.session(id).await?.is_some();
        return Ok(json!({"known": 0, "taken": exists}));
    };
    let (imported, last): (i64, i64) = (r.get("imported_seq"), r.get("last_seq"));
    let mine = r.get::<String, _>("worker_id") == from.to_string();
    Ok(json!({"known": imported, "taken": last > imported || !mine}))
}

/// The realm a folder goes to: the first rule it is under, else home.
fn realm_for(rules: &[api::ImportRule], cwd: &str, home: &str) -> String {
    let expand = |f: &str| match f.trim().strip_prefix("~") {
        Some(rest) => format!("{}{}", home.trim_end_matches('/'), rest),
        None => f.trim().to_owned(),
    };
    for r in rules {
        let base = expand(&r.folder);
        let base = base.trim_end_matches("/**").trim_end_matches('/');
        if cwd == base || cwd.starts_with(&format!("{base}/")) {
            return r.realm.clone();
        }
    }
    crate::realms::HOME.to_owned()
}

/// `imports.append`: the next events of an imported session (the first
/// call makes it).
pub async fn append(st: &HeadState, from: WorkerId, p: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
    let (source, id) = (s("source"), s("id"));
    if !SOURCES.contains(&source.as_str()) || id.is_empty() || id.len() > 100 {
        return Err(bad_request("an import names its source and the session's id"));
    }
    let status = status(st, from, &id).await?;
    if status["taken"] == true {
        return Err(bad_request(format!("session {id} is carbon6's now (it was continued here)")));
    }
    let m = &p["meta"];
    let ms = |k: &str| m[k].as_str().unwrap_or_default().to_owned();
    let worker = st.store.worker(from).await?.ok_or_else(|| bad_request("an unknown worker"))?;
    if st.store.session(&id).await?.is_none() {
        let rules = settings(st).await?.rules;
        let now = c6_core::now_ms();
        let started = m["started_at"].as_i64().filter(|t| *t > 0).unwrap_or(now);
        let title = ms("title");
        let label = if source == "codex" { "imported:codex" } else { "imported:claude-code" };
        let row = SessionRow {
            id: id.clone(),
            worker_id: from.to_string(),
            title: if title.is_empty() {
                format!("Imported from {}", if source == "codex" { "Codex" } else { "Claude Code" })
            } else {
                title
            },
            cwd: ms("cwd"),
            connection_id: String::new(),
            model: ms("model"),
            status: "idle".into(),
            error: None,
            effort: None,
            parent_id: None,
            parent_call_id: None,
            fork_seq: None,
            created_at: started,
            updated_at: now,
            last_seq: 0,
            archived: 0,
            labels: serde_json::to_string(&[label])?,
            worker_name: worker.name.clone(),
            repo: String::new(),
            folder: String::new(),
            home: String::new(),
            ssh_target: String::new(),
            awaiting: 0,
            subagent_model: String::new(),
            subagent_connection_id: String::new(),
            subagent_effort: String::new(),
            realm: realm_for(&rules, &ms("cwd"), &ms("home")),
            agent: String::new(),
            end_reason: String::new(),
            root_id: None,
            observed: false,
            instance: String::new(),
            colour: String::new(),
        };
        st.store.create_session(&row, &[]).await?;
        sqlx::query("INSERT INTO session_imports (session_id, source, file, imported_seq, imported_at) VALUES (?, ?, ?, 0, ?)")
            .bind(&id)
            .bind(&source)
            .bind(ms("file"))
            .bind(now)
            .execute(&st.store.pool)
            .await?;
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut events = Vec::new();
    for e in p["events"].as_array().into_iter().flatten() {
        let bytes = b64.decode(e.as_str().unwrap_or_default()).map_err(|_| bad_request("events are base64"))?;
        events.push(c6_proto::SessionEvent::decode(bytes.as_slice()).map_err(|e| bad_request(format!("an event: {e}")))?);
    }
    let Some(ing) = st.store.ingest(from, &id, &events).await? else {
        return Err(bad_request(format!("session {id} is not this worker's")));
    };
    sqlx::query("UPDATE session_imports SET imported_seq = ?, imported_at = ? WHERE session_id = ?")
        .bind(ing.last_seq as i64)
        .bind(c6_core::now_ms())
        .bind(&id)
        .execute(&st.store.pool)
        .await?;
    // Open pages follow; the memex gets its digests. Nothing else: history.
    for ev in &ing.events {
        if let Some(event) = crate::convert::event(ev) {
            st.hub.publish(&id, api::StreamMsg::Event { event });
        }
    }
    if !ing.events.is_empty() {
        st.registry.bump();
    }
    crate::memex::on_events(st, &id, &ing.events).await;
    Ok(json!({"known": ing.last_seq}))
}

/// What was imported, for Settings: sessions by source and realm.
pub async fn summary(st: &HeadState) -> anyhow::Result<Value> {
    let rows = sqlx::query(
        "SELECT i.source, s.realm, count(*) AS n, max(i.imported_at) AS at FROM session_imports i JOIN sessions s ON s.id = i.session_id \
         GROUP BY i.source, s.realm ORDER BY i.source, s.realm",
    )
    .fetch_all(&st.store.pool)
    .await?;
    Ok(json!(
        rows.iter()
            .map(|r| json!({"source": r.get::<String, _>("source"), "realm": r.get::<String, _>("realm"),
        "sessions": r.get::<i64, _>("n"), "last": r.get::<i64, _>("at")}))
            .collect::<Vec<_>>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_map_to_realms() {
        let rules = vec![
            api::ImportRule { folder: "~/work/**".into(), realm: "work".into() },
            api::ImportRule { folder: "/srv/acme".into(), realm: "acme".into() },
        ];
        assert_eq!(realm_for(&rules, "/Users/me/work/app", "/Users/me"), "work");
        assert_eq!(realm_for(&rules, "/Users/me/work", "/Users/me"), "work");
        assert_eq!(realm_for(&rules, "/Users/me/workshop", "/Users/me"), "home");
        assert_eq!(realm_for(&rules, "/srv/acme/api", "/Users/me"), "acme");
    }
}
