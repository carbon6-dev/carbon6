//! Calls nodes make to the head (`HeadCall`), by role. Workers reach the
//! memex through here, with their session's scope stamped on by the head;
//! the memex asks for its settings and to notify you.

use c6_core::WorkerId;
use serde_json::{Value, json};

use crate::{HeadState, bad_request};

/// What a session's tools may ask the memex.
const SESSION_METHODS: [&str; 20] = [
    "memex.search",
    // Environment knowledge.
    "memex.knowledge.record",
    "memex.knowledge.verify",
    "memex.knowledge.set",
    "memex.knowledge.list",
    "memex.knowledge.page",
    "memex.get",
    "memex.related",
    "memex.note",
    "memex.timeline",
    "memex.trail_append",
    "memex.trail",
    // Mail and calendars: within the session's scope (email, event).
    "memex.mail.threads",
    "memex.mail.thread",
    "memex.mail.draft",
    "memex.mail.send",
    "memex.calendar.events",
    "memex.calendar.busy",
    "memex.calendar.create",
    "memex.calendar.rsvp",
];

pub async fn handle(st: &HeadState, from: WorkerId, method: &str, mut params: Value) -> anyhow::Result<Value> {
    let role = st.registry.get(from).map(|l| l.role).unwrap_or_default();
    if role == "memex" {
        return match method {
            "config.get" => crate::memex::config(st).await,
            "event" => {
                let kind = params["type"].as_str().unwrap_or_default().to_owned();
                let realm = params["realm"].as_str().unwrap_or(crate::realms::HOME).to_owned();
                crate::automations::on_memex_event(st, &realm, &kind, &params["data"]).await;
                Ok(json!({}))
            }
            // A disputed fact, for you to settle.
            "todo.add" => {
                // From the memex, about one realm's knowledge.
                let realm = params["realm"].as_str().unwrap_or(crate::realms::HOME).to_owned();
                let d: c6_api::DecisionIn = serde_json::from_value(params).map_err(|e| bad_request(format!("{e}")))?;
                Ok(json!({"id": crate::decisions::open(st, &d, &realm, "").await?.id}))
            }
            "notify" => {
                let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
                st.notifier.send(crate::notify::Note {
                    event: "notice",
                    title: s("title"),
                    message: s("message"),
                    priority: if s("priority") == "high" { "high" } else { "normal" },
                    labels: vec!["memex".into()],
                    ..Default::default()
                });
                Ok(json!({}))
            }
            other => Err(bad_request(format!("the memex may not call {other}"))),
        };
    }
    // Action items and the API reference, for one of this worker's sessions.
    if let Some(what) = method.strip_prefix("session.") {
        let session_id = params["session_id"].as_str().ok_or_else(|| bad_request("session_id is required"))?.to_owned();
        let session = st.store.session(&session_id).await?.ok_or_else(|| bad_request("no such session"))?;
        if session.worker_id != from.to_string() {
            return Err(bad_request("that session is not on this worker"));
        }
        // A side chat or subagent works for its parent's conversation.
        let target = session.parent_id.clone().unwrap_or(session_id);
        return match what {
            "todo" | "todo_resolve" | "decision" => crate::decisions::from_session(st, &target, what, &params).await,
            // The API reference, for agents writing integrations.
            "api" => {
                let urls = crate::notify::api_urls(st).await;
                let text = crate::catalog::reference(params["query"].as_str().unwrap_or(""), params["op"].as_str().unwrap_or(""), &urls)
                    .map_err(bad_request)?;
                Ok(json!({"text": text}))
            }
            // Asking for another realm's data (docs/os.md §3.7).
            "grant_request" => {
                let realm = crate::realms::of_session(st, &target).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
                crate::grants::request(st, &target, &realm, &params).await
            }
            "openapi" => Ok(crate::catalog::openapi(&crate::notify::api_urls(st).await)),
            // Collections, in the session's realm (docs/os.md §4.1).
            "collections" | "records_query" | "records_get" | "records_put" | "records_delete" => {
                let realm = crate::realms::of_session(st, &target).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
                crate::collections::for_session(st, &realm, &target, what, &params).await
            }
            other => Err(bad_request(format!("a worker may not call session.{other}"))),
        };
    }
    if !SESSION_METHODS.contains(&method) {
        return Err(bad_request(format!("a worker may not call {method}")));
    }
    // On behalf of one of this worker's sessions, which decides the scope.
    let session_id = params["session_id"].as_str().ok_or_else(|| bad_request("session_id is required"))?.to_owned();
    let session = st.store.session(&session_id).await?.ok_or_else(|| bad_request("no such session"))?;
    if session.worker_id != from.to_string() {
        return Err(bad_request("that session is not on this worker"));
    }
    let labels = session.all_labels();
    params["scope"] = json!(crate::memex::scope_for(st, &labels).await);
    params["made_by"] = json!(format!("session:{session_id}"));
    if method == "memex.note" && params["labels"].as_array().is_none_or(|a| a.is_empty()) {
        params["labels"] = json!(labels.into_iter().filter(|l| !l.starts_with("model:")).collect::<Vec<_>>());
    }
    let by = format!("session:{session_id}");
    // Another realm's operation, through a grant (docs/os.md §3.7): it runs
    // there, as that realm (mail drafted through home's accounts).
    if let Some(owner) = crate::grants::caller_of(st, &session.realm, method, &by).await {
        if let Some(kinds) = params["scope"].as_array_mut() {
            kinds.extend([json!("email"), json!("event")]);
        }
        return crate::memex::call_in(st, &owner, method, params).await;
    }
    match method {
        "memex.search" => search_granted(st, &session.realm, &by, params).await,
        "memex.get" => get_granted(st, &session.realm, &by, params).await,
        _ => crate::memex::call_in(st, &session.realm, method, params).await,
    }
}

/// The scope a read grant allows: its kinds, else what the session may see.
fn granted_scope(params: &Value, kinds: &[String]) -> Value {
    if kinds.is_empty() { params["scope"].clone() } else { json!(kinds) }
}

/// A search over the session's realm and the realms that granted it reads
/// of their knowledge; hits from those say which realm they are from.
pub async fn search_granted(st: &HeadState, realm: &str, by: &str, params: Value) -> anyhow::Result<Value> {
    let mut hits: Vec<Value> = match crate::memex::call_in(st, realm, "memex.search", params.clone()).await? {
        Value::Array(h) => h,
        other => return Ok(other),
    };
    let grants = crate::grants::memex_reads(st, realm).await;
    if grants.is_empty() {
        return Ok(json!(hits));
    }
    for (grant, from, kinds) in grants {
        let mut p = params.clone();
        p["scope"] = granted_scope(&params, &kinds);
        match crate::memex::call_in(st, &from, "memex.search", p).await {
            Ok(Value::Array(found)) => {
                if !found.is_empty() {
                    crate::grants::log_memex_read(
                        st,
                        &grant,
                        by,
                        &format!("search {:?}: {} hits", params["query"].as_str().unwrap_or(""), found.len()),
                    )
                    .await;
                }
                hits.extend(found.into_iter().map(|mut h| {
                    h["realm"] = json!(from);
                    h
                }));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("searching realm {from} through grant {grant}: {e:#}"),
        }
    }
    hits.sort_by(|a, b| b["score"].as_f64().unwrap_or(0.0).total_cmp(&a["score"].as_f64().unwrap_or(0.0)));
    hits.truncate(params["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize);
    Ok(json!(hits))
}

/// An item or thing by id: the session's realm's, else one a granting
/// realm lets it read.
pub async fn get_granted(st: &HeadState, realm: &str, by: &str, params: Value) -> anyhow::Result<Value> {
    let own = crate::memex::call_in(st, realm, "memex.get", params.clone()).await;
    if own.as_ref().is_ok_and(|v| !v.is_null() && v.as_object().is_none_or(|o| !o.is_empty())) {
        return own;
    }
    for (grant, from, kinds) in crate::grants::memex_reads(st, realm).await {
        let mut p = params.clone();
        p["scope"] = granted_scope(&params, &kinds);
        if let Ok(mut v) = crate::memex::call_in(st, &from, "memex.get", p).await
            && v.as_object().is_some_and(|o| !o.is_empty())
        {
            crate::grants::log_memex_read(st, &grant, by, &format!("get {}", params["id"].as_str().unwrap_or(""))).await;
            v["realm"] = json!(from);
            return Ok(v);
        }
    }
    own
}
