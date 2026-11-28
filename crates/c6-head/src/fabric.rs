//! Calls nodes make to the head (`HeadCall`), by role. Workers reach the
//! memex through here, with their session's scope stamped on by the head;
//! the memex asks for its settings and to notify you.

use c6_core::WorkerId;
use serde_json::{Value, json};

use crate::{HeadState, bad_request};

/// What a session's tools may ask the memex.
const SESSION_METHODS: [&str; 30] = [
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
    // What people need from you (docs/brief.md §5).
    "memex.requests.list",
    // A reply box's draft, which the session replaces (docs/conversations.md §3).
    "memex.drafts.get",
    "memex.drafts.set",
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
            crate::work::on_memex_event(st, &realm, &kind, &params["data"]).await;
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
                path: params["path"].as_str().filter(|p| p.starts_with('/')).map(str::to_owned),
                ..Default::default()
            });
            Ok(json!({}))
        }
        other => Err(bad_request(format!("the memex may not call {other}"))),
    }
}

/// The item kinds a session's knowledge calls may see: what its labels'
/// memex scopes allow, or for an app's session what its install approved
/// (docs/apps.md §5.2).
pub async fn session_scope(st: &HeadState, session: &crate::store::SessionRow) -> anyhow::Result<Vec<String>> {
    // The first prompt is resolved before its session is inserted. Its row
    // already names the app: a database lookup by session id would miss it
    // and give that first input the realm's broader knowledge scope.
    if !session.instance.is_empty()
        && let Some(i) = crate::instances::get(&st.store.pool, &session.realm, &session.instance).await?
    {
        return Ok(crate::instances::approved_reads(&st.store.pool, i.id).await?.items.kinds);
    }
    Ok(crate::memex::scope_for(st, &session.realm, &session.all_labels()).await)
}

pub async fn handle(st: &HeadState, from: WorkerId, method: &str, mut params: Value) -> anyhow::Result<Value> {
    let role = st.registry.get(from).map(|l| l.role).unwrap_or_default();
    if role == "memex" {
        return memex_call(st, method, params).await;
    }
    // Sessions this worker imports from Claude Code and Codex (docs/imports.md).
    match method {
        "disk.pressure" => return disk_pressure(st, from, &params).await,
        "build_folders.context" => return build_folder_context(st, from, &params).await,
        "build_folders.cleaned" => return build_folders_cleaned(st, from, &params).await,
        "quotas.codex.config" => return crate::quotas::codex_config(st, from).await,
        "quotas.codex.report" => {
            crate::quotas::from_codex(st, from, serde_json::from_value(params)?).await?;
            return Ok(json!({}));
        }
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
        if what == "recovery_status" {
            return crate::sessions::report_recovery(st, from, &session, &params).await;
        }
        if session.worker_id != from.to_string() {
            return Err(bad_request("that session is not on this worker"));
        }
        // Its own remote subagents (docs/remote-subagents.md).
        if let Some(what) = what.strip_prefix("subagent_") {
            return crate::remote::for_session(st, &session, what, &params).await;
        }
        // A side chat or subagent works for its parent's conversation.
        let target = session.parent_id.clone().unwrap_or(session_id);
        return match what {
            "capability" => crate::project_contract::capability(st, &session, &params).await,
            "contract_failure" => crate::project_contract::failure(st, &session, &params).await,
            "cache_lifetime" => Ok(serde_json::to_value(crate::cache_lifetime::for_session(st, &session, &params).await?)?),
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
                let run = crate::templates::run_for_session(
                    st,
                    template,
                    params["input"].as_str().unwrap_or_default(),
                    &[format!("started-by:{target}")],
                    &realm,
                    &session,
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
            // Messages to other sessions in its realm (docs/background.md §6),
            // from the session that calls (a subagent's calls come as its
            // top-level session's).
            "message" => crate::sessions::message_from(st, &session, &params).await,
            "sessions" => crate::sessions::sessions_for(st, &session, &params).await,
            // The work it belongs to (docs/work.md §4).
            "work" => crate::work::for_session(st, &target, &params).await,
            // Anything you can do, by your rule for agents (docs/agent-actions.md).
            "op" => crate::agent_ops::for_session(st, &target, &params).await,
            // Documents (docs/docs.md §9).
            "doc" => match params["action"].as_str().unwrap_or("") {
                "archive" | "unarchive" | "archive_many" => crate::docs::archive::for_session(st, &target, &params).await,
                _ => crate::docs::session::for_session(st, &target, &params).await,
            },
            // The guard watcher judges a call a policy matched (docs/watchers.md §5.6).
            "guard" => {
                let row = st.store.session(&target).await?.ok_or_else(|| bad_request("no such session"))?;
                let why =
                    crate::watchers::guard(st, &row, params["tool"].as_str().unwrap_or(""), params["input"].as_str().unwrap_or("")).await?;
                Ok(json!({"risky": why.is_some(), "why": why.unwrap_or_default()}))
            }
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
    params["scope"] = json!(session_scope(st, &session).await?);
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

/// The worker owns threshold transitions and hysteresis; the head routes only
/// active transitions through the existing user notification channels.
async fn disk_pressure(st: &HeadState, from: WorkerId, params: &Value) -> anyhow::Result<Value> {
    let worker = st.store.worker(from).await?.filter(|w| w.revoked_at.is_none()).ok_or_else(|| bad_request("unknown or revoked worker"))?;
    if st.registry.get(from).is_none() {
        return Err(bad_request("disk pressure must come from a connected worker"));
    }
    let active = params["active"].as_bool().ok_or_else(|| bad_request("active must be a boolean"))?;
    let mount = params["mount"].as_str().filter(|s| !s.is_empty() && s.len() <= 4096).ok_or_else(|| bad_request("mount is required"))?;
    let free = params["free_bytes"].as_u64().ok_or_else(|| bad_request("free_bytes is required"))?;
    let total =
        params["total_bytes"].as_u64().filter(|total| *total > 0 && free <= *total).ok_or_else(|| bad_request("invalid total_bytes"))?;
    if active {
        st.notifier.send(crate::notify::Note {
            event: "failed",
            title: format!("Low disk space on {}", worker.name),
            message: format!("Mount {mount} on {}: {free} bytes free of {total} bytes. Sessions wait for disk space and recover automatically if the disk fills.", worker.name),
            priority: "high",
            labels: serde_json::from_str(&worker.labels).unwrap_or_default(),
            ..Default::default()
        });
    }
    Ok(Value::Null)
}

async fn build_folder_context(st: &HeadState, from: WorkerId, params: &Value) -> anyhow::Result<Value> {
    use sqlx::Row;
    require_build_folder_worker(st, from).await?;
    let ids: Vec<String> = serde_json::from_value(params["session_ids"].clone())?;
    if ids.len() > 512 || ids.iter().any(|id| id.is_empty() || id.len() > 128) {
        return Err(bad_request("invalid build folder session ids"));
    }
    let rows = sqlx::query("SELECT id, CASE WHEN archived != 0 THEN archived_at_ms END AS archived_at_ms FROM sessions WHERE worker_id = ? AND id IN (SELECT value FROM json_each(?))")
        .bind(from.to_string()).bind(serde_json::to_string(&ids)?).fetch_all(&st.store.pool).await?;
    let sessions: Vec<_> = ids
        .into_iter()
        .map(|id| {
            let archived =
                rows.iter().find(|row| row.get::<String, _>("id") == id).and_then(|row| row.get::<Option<i64>, _>("archived_at_ms"));
            json!({"session_id": id, "archived_at_ms": archived})
        })
        .collect();
    Ok(json!({"sessions": sessions}))
}

async fn require_build_folder_worker(st: &HeadState, from: WorkerId) -> anyhow::Result<crate::store::WorkerRow> {
    let worker = st.store.worker(from).await?.filter(|w| w.revoked_at.is_none()).ok_or_else(|| bad_request("unknown or revoked worker"))?;
    if !st.registry.get(from).is_some_and(|live| live.role == "worker") {
        return Err(bad_request("build folder reports require a connected worker"));
    }
    Ok(worker)
}

async fn build_folders_cleaned(st: &HeadState, from: WorkerId, params: &Value) -> anyhow::Result<Value> {
    let worker = require_build_folder_worker(st, from).await?;
    let message = params["message"]
        .as_str()
        .filter(|m| !m.is_empty() && m.len() <= 4096)
        .ok_or_else(|| bad_request("cleanup message is required"))?;
    st.notifier.send(crate::notify::Note {
        event: "notice",
        title: format!("Build folders cleaned on {}", worker.name),
        message: message.into(),
        priority: "normal",
        labels: serde_json::from_str(&worker.labels).unwrap_or_default(),
        ..Default::default()
    });
    Ok(Value::Null)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_folder_context_and_cleanup_notice_are_worker_scoped_and_notify_without_a_session() {
        let (_dir, head, _mock, _work) = crate::work::tests::routing_fixture().await;
        let st = &head.state;
        let worker = WorkerId::new();
        let params = json!({"session_ids": ["session", "foreign"]});
        assert!(handle(st, worker, "build_folders.context", params.clone()).await.is_err());
        st.store
            .insert_worker(
                &crate::store::NewWorker {
                    id: worker,
                    name: "build-test",
                    hostname: "host",
                    os: "test",
                    arch: "test",
                    version: "test",
                    cert_serial: "build-test",
                },
                "",
                0,
            )
            .await
            .unwrap();
        assert!(handle(st, worker, "build_folders.context", params.clone()).await.is_err());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let _live = st.registry.attach(worker, 1, tx, vec![], "worker").await;
        sqlx::query("UPDATE sessions SET worker_id = ?, archived = 1, archived_at_ms = 123 WHERE id = 'session'")
            .bind(worker.to_string())
            .execute(&st.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, archived, archived_at_ms) VALUES ('foreign', 'other', 'foreign', '', 'cc', 'opus', 'idle', 1, 1, 1, 999)")
            .execute(&st.store.pool).await.unwrap();
        let context = handle(st, worker, "build_folders.context", params.clone()).await.unwrap();
        assert_eq!(context["sessions"][0]["archived_at_ms"], 123);
        assert!(context["sessions"][1]["archived_at_ms"].is_null(), "foreign archive state is not disclosed");
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let hook = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(value): axum::Json<Value>| {
                let sent = sent.clone();
                async move {
                    sent.send(value).unwrap();
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, hook).await.unwrap() });
        st.store
            .set_setting(
                crate::notify::CHANNELS,
                &json!([{"name":"cleanup", "kind":"webhook", "url":url, "events":["notice"]}]).to_string(),
            )
            .await
            .unwrap();
        assert!(handle(st, worker, "build_folders.cleaned", json!({"message":""})).await.is_err());
        handle(st, worker, "build_folders.cleaned", json!({"message":"Removed archived build output"})).await.unwrap();
        st.notifier.send(crate::notify::Note { event: "notice", title: "cleanup barrier".into(), ..Default::default() });
        let note = c6_testkit::within("machine cleanup notification", received.recv()).await.unwrap();
        assert_eq!(note["title"], "Build folders cleaned on build-test");
        assert_eq!(note["message"], "Removed archived build output");
        assert_eq!(c6_testkit::within("cleanup notice barrier", received.recv()).await.unwrap()["title"], "cleanup barrier");
        sqlx::query("UPDATE workers SET revoked_at = 1 WHERE id = ?").bind(worker.to_string()).execute(&st.store.pool).await.unwrap();
        assert!(handle(st, worker, "build_folders.context", params).await.is_err());
        assert!(handle(st, worker, "build_folders.cleaned", json!({"message":"not allowed"})).await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn disk_pressure_notifies_only_active_transitions_from_known_live_workers() {
        let (_dir, head, _mock, _work) = crate::work::tests::routing_fixture().await;
        let st = &head.state;
        let worker = WorkerId::new();
        let mut params = json!({"active": true, "mount": "/data", "free_bytes": 7, "total_bytes": 100});
        assert!(handle(st, worker, "disk.pressure", params.clone()).await.is_err());
        st.store
            .insert_worker(
                &crate::store::NewWorker {
                    id: worker,
                    name: "disk-test",
                    hostname: "host",
                    os: "test",
                    arch: "test",
                    version: "test",
                    cert_serial: "disk-test",
                },
                "",
                0,
            )
            .await
            .unwrap();
        assert!(handle(st, worker, "disk.pressure", params.clone()).await.is_err(), "offline workers cannot report");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let _live = st.registry.attach(worker, 1, tx, vec![], "worker").await;
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let hook = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(value): axum::Json<Value>| {
                let sent = sent.clone();
                async move {
                    sent.send(value).unwrap();
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, hook).await.unwrap() });
        st.store
            .set_setting(
                crate::notify::CHANNELS,
                &json!([
                    {"name": "disk-test", "kind": "webhook", "url": url, "events": ["failed"]}
                ])
                .to_string(),
            )
            .await
            .unwrap();
        params["active"] = json!(false);
        handle(st, worker, "disk.pressure", params.clone()).await.unwrap();
        params["active"] = json!(true);
        params["free_bytes"] = json!(101);
        assert!(handle(st, worker, "disk.pressure", params.clone()).await.is_err());
        params["free_bytes"] = json!(7);
        handle(st, worker, "disk.pressure", params.clone()).await.unwrap();
        st.notifier.send(crate::notify::Note { event: "failed", title: "disk barrier".into(), ..Default::default() });
        let note = c6_testkit::within("disk pressure notification", received.recv()).await.unwrap();
        assert_eq!(note["title"], "Low disk space on disk-test");
        assert!(note["message"].as_str().unwrap().contains("/data on disk-test: 7 bytes free of 100 bytes"));
        let barrier = c6_testkit::within("disk notification barrier", received.recv()).await.unwrap();
        assert_eq!(barrier["title"], "disk barrier", "clear and invalid reports must not notify");
        sqlx::query("UPDATE workers SET revoked_at = 1 WHERE id = ?").bind(worker.to_string()).execute(st.store.pool()).await.unwrap();
        assert!(handle(st, worker, "disk.pressure", params).await.is_err());
        server.abort();
    }
}
