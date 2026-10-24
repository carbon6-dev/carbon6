//! Calls nodes make to the head (`HeadCall`), by role. Workers reach the
//! memex through here, with their session's scope stamped on by the head;
//! the memex asks for its settings and to notify you.

use c6_core::WorkerId;
use serde_json::{Value, json};

use crate::{HeadState, bad_request};

/// What a session's tools may ask the memex.
const SESSION_METHODS: [&str; 27] = [
    "memex.search",
    // Claims (docs/apps.md §9): owned by the session's app, or the realm.
    "memex.claims.add",
    "memex.claims.retract",
    "memex.claims.list",
    "memex.things.create",
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
    "memex.chat.messages",
    "memex.chat.thread",
    "memex.chat.post",
    "memex.mail.threads",
    "memex.mail.thread",
    "memex.mail.draft",
    "memex.mail.send",
    "memex.calendar.events",
    "memex.calendar.busy",
    "memex.calendar.create",
    "memex.calendar.rsvp",
];

/// What the memex asks of the head (docs/memex.md §2.4): its settings,
/// events for triggers, decisions, notifications.
pub async fn memex_call(st: &HeadState, method: &str, params: Value) -> anyhow::Result<Value> {
    match method {
        "config.get" => crate::memex::config(st).await,
        "event" => {
            let kind = params["type"].as_str().unwrap_or_default().to_owned();
            let realm = params["realm"].as_str().unwrap_or(crate::realms::HOME).to_owned();
            crate::triggers::on_memex_event(st, &realm, &kind, &params["data"]).await;
            // Realms granted reads of that knowledge hear it too.
            let item_kind = match kind.as_str() {
                "mail.received" => "email",
                other => other.split('.').next().unwrap_or(other),
            };
            for (grant, to) in crate::grants::memex_readers(st, &realm, item_kind).await {
                crate::grants::log_memex_read(st, &grant, "memex event", &kind).await;
                crate::triggers::on_memex_event(st, &to, &kind, &params["data"]).await;
            }
            Ok(json!({}))
        }
        // An app's operation, for the knowledge store's mail, calendar and
        // contacts: what the account's app does (docs/knowledge.md §6).
        "app.call" => {
            let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
            let realm = params["realm"].as_str().unwrap_or(crate::realms::HOME).to_owned();
            let by = params["by"].as_str().filter(|b| !b.is_empty()).unwrap_or("you");
            crate::operations::call(st, &realm, &s("app"), &s("operation"), &params["input"], by).await
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
    }
}

pub async fn handle(st: &HeadState, from: WorkerId, method: &str, mut params: Value) -> anyhow::Result<Value> {
    let role = st.registry.get(from).map(|l| l.role).unwrap_or_default();
    if role == "memex" {
        return memex_call(st, method, params).await;
    }
    // Sessions this worker imports from Claude Code and Codex (docs/imports.md).
    match method {
        "imports.config" => return crate::imports::config(st, from).await,
        // How its update goes (docs/updates.md §5).
        "update.status" => return crate::updates::worker_status(st, from, &params).await,
        "imports.status" => return crate::imports::status(st, from, params["id"].as_str().unwrap_or_default()).await,
        "imports.append" => return crate::imports::append(st, from, &params).await,
        _ => {}
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
            // Task cards (docs/task-cards.md).
            "task_propose" | "task_withdraw" => crate::decisions::task_card(st, &target, what, &params).await,
            // The API reference, for agents writing integrations.
            "api" => {
                let urls = crate::notify::api_urls(st).await;
                let text = crate::catalog::reference(params["query"].as_str().unwrap_or(""), params["op"].as_str().unwrap_or(""), &urls)
                    .map_err(bad_request)?;
                Ok(json!({"text": text}))
            }
            // Kits the session's realm may use (docs/packages.md §5).
            "kits" | "kit_spec" => {
                let realm = crate::realms::of_session(st, &target).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
                if what == "kits" {
                    crate::packages::kits_text(st, &realm).await
                } else {
                    crate::packages::kit_spec(st, &realm, params["kit"].as_str().unwrap_or_default()).await
                }
            }
            // A template, in the session's realm or one whose app exports it
            // to this one (docs/packages.md §6).
            "template_start" => {
                let own = crate::realms::of_session(st, &target).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
                let realm = params["realm"].as_str().filter(|r| !r.is_empty()).unwrap_or(&own).to_owned();
                let template = params["template"].as_str().unwrap_or_default();
                if realm != own && !crate::grants::may_start(st, &realm, &own, template, &format!("session:{target}")).await {
                    return Err(bad_request(format!("realm {own} may not start {realm}'s template {template:?}; no grant allows it")));
                }
                let run = crate::templates::run(
                    st,
                    template,
                    params["input"].as_str().unwrap_or_default(),
                    &[format!("started-by:{target}")],
                    &realm,
                )
                .await?;
                Ok(
                    json!({"run_id": run.run_id, "realm": realm, "sessions": run.sessions.iter().map(|s| &s.id).collect::<Vec<_>>(), "errors": run.errors}),
                )
            }
            // An app's operation (docs/apps.md §7).
            "app_call" => crate::operations::for_session(st, &target, &params).await,
            // Series, in the session's realm (docs/os.md §4.4).
            "series_record" | "series_query" => {
                let realm = crate::realms::of_session(st, &target).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
                crate::series::for_session(st, &realm, &target, what, &params).await
            }
            // Asking for another realm's data (docs/os.md §3.7).
            "grant_request" => {
                let realm = crate::realms::of_session(st, &target).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
                crate::grants::request(st, &target, &realm, &params).await
            }
            // Proposing a record or fact for commons (docs/os.md §3.3).
            "commons_publish" => {
                let realm = crate::realms::of_session(st, &target).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
                crate::commons::from_session(st, &target, &realm, &params).await
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
    // An app's session sees the item kinds its install approved (docs/apps.md §5.2).
    if let Some(i) = crate::instances::of_session(&st.store.pool, &session_id).await? {
        params["scope"] = json!(crate::instances::approved_reads(&st.store.pool, i.id).await?.items.kinds);
    }
    params["made_by"] = json!(format!("session:{session_id}"));
    let owner = crate::claims::owner_of_session(st, &session_id).await;
    if method.starts_with("memex.claims.") || method == "memex.things.create" {
        return crate::claims::call(st, &session.realm, method, params, &owner, &format!("session:{session_id}")).await;
    }
    params["owner"] = json!(owner);
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
