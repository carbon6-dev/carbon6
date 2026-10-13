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
                crate::automations::on_memex_event(st, &kind, &params["data"]).await;
                Ok(json!({}))
            }
            // A disputed fact, for you to settle.
            "todo.add" => {
                let t: c6_api::TodoIn = serde_json::from_value(params).map_err(|e| bad_request(format!("{e}")))?;
                Ok(json!({"id": crate::todos::add(st, &t).await?.id}))
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
    // The operation catalog, for one of this worker's sessions (within the
    // rules for sessions).
    if let Some(what) = method.strip_prefix("c6.") {
        let session_id = params["session_id"].as_str().ok_or_else(|| bad_request("session_id is required"))?.to_owned();
        let session = st.store.session(&session_id).await?.ok_or_else(|| bad_request("no such session"))?;
        if session.worker_id != from.to_string() {
            return Err(bad_request("that session is not on this worker"));
        }
        return crate::selfconfig::session_call(st, &session, what, &params).await;
    }
    // Next steps and trackers, for one of this worker's sessions.
    if let Some(what) = method.strip_prefix("session.") {
        let session_id = params["session_id"].as_str().ok_or_else(|| bad_request("session_id is required"))?.to_owned();
        let session = st.store.session(&session_id).await?.ok_or_else(|| bad_request("no such session"))?;
        if session.worker_id != from.to_string() {
            return Err(bad_request("that session is not on this worker"));
        }
        // A side chat or subagent works for its parent's conversation.
        let target = session.parent_id.clone().unwrap_or(session_id);
        return match what {
            "suggest" => {
                let text = params["text"].as_str().unwrap_or_default();
                let prompt = params["prompt"].as_str().filter(|p| !p.trim().is_empty()).unwrap_or(text);
                let anchor = st.store.session(&target).await?.map(|s| s.last_seq as u64).unwrap_or(0);
                let id = crate::followups::suggest(st, &target, anchor, "next", text, prompt, "agent").await?;
                Ok(json!({"offered": id.is_some()}))
            }
            "followups" => Ok(json!(crate::followups::for_session(st, &target).await?)),
            "todo" | "todo_resolve" => crate::todos::from_session(st, &target, what, &params).await,
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
    crate::memex::call(st, method, params).await
}
