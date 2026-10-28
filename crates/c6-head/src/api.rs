//! HTTP API (`/api/v1`) and the embedded web UI.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use c6_api as api;
use c6_core::{WorkerId, now_ms};
use rust_embed::RustEmbed;

use crate::HeadState;
use crate::realms::Caller;

pub fn router(state: Arc<HeadState>) -> Router {
    Router::new()
        .route("/api/v1/workers", get(list_workers))
        .route("/api/v1/workers/enrollments", post(create_enrollment))
        .route("/api/v1/workers/{id}/revoke", post(revoke_worker))
        .route("/api/v1/workers/{id}", delete(delete_worker))
        .route("/api/v1/connections", get(list_connections).post(create_connection))
        .route("/api/v1/connections/test", post(test_connection))
        .route("/api/v1/connections/{id}", delete(delete_connection))
        .route("/api/v1/connections/{id}/models", get(connection_models))
        .route("/api/v1/connections/chatgpt/start", post(chatgpt_start))
        .route("/api/v1/connections/chatgpt/complete", post(chatgpt_complete))
        .route("/callback", get(chatgpt_callback))
        // Google's apps sign in through the address registered before they were apps.
        .route("/auth/google/callback", get(crate::providers::callback))
        .route("/auth/oauth/callback", get(crate::providers::callback))
        .route("/api/v1/google/client", get(get_google_client).put(put_google_client))
        .route("/api/v1/slack", get(slack_setup))
        .route("/api/v1/slack/client", put(slack_client))
        .route("/api/v1/slack/install", post(slack_install))
        .route("/api/v1/slack/connect", post(slack_connect))
        .route("/api/v1/slack/sync", post(slack_sync))
        .route("/api/v1/slack/accounts/{id}", delete(slack_remove))
        .route("/api/v1/google/connect", post(google_connect))
        .route("/api/v1/google/install", post(google_install))
        .route("/api/v1/settings/session_imports", get(get_session_imports).put(put_session_imports))
        .route("/api/v1/imports", get(list_imports))
        .route("/api/v1/google/accounts", get(google_accounts))
        .route("/api/v1/google/accounts/{id}", delete(google_remove))
        .route("/ca.pem", get(ui_ca_pem))
        .route("/ca.crt", get(ui_ca_crt))
        .route("/ca.mobileconfig", get(ui_ca_profile))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route("/api/v1/sessions/{id}", get(get_session).patch(update_session))
        .route("/api/v1/sessions/{id}/events", get(session_events))
        .route("/api/v1/sessions/{id}/children", get(session_children))
        .route("/api/v1/sessions/{id}/side-chats", post(create_side_chat))
        .route("/api/v1/settings/mcp-servers", get(get_mcp_servers).put(put_mcp_servers))
        .route("/api/v1/settings/skills", get(get_skills).put(put_skills))
        .route("/api/v1/settings/hooks", get(get_hooks).put(put_hooks))
        .route("/api/v1/settings/secrets", get(list_secrets))
        .route("/api/v1/settings/templates", get(get_templates).put(put_templates))
        .route("/api/v1/templates", get(all_templates))
        .route("/api/v1/templates/{name}/run", post(run_template))
        .route("/api/v1/memories", get(list_memories).post(create_memory))
        .route("/api/v1/memories/{id}", put(update_memory).delete(delete_memory))
        .route("/api/v1/automations", get(list_automations).post(create_automation))
        .route("/api/v1/automations/{id}", axum::routing::patch(update_automation).delete(delete_automation))
        .route("/api/v1/automations/{id}/run", post(run_automation))
        .route("/api/v1/automations/{id}/runs", get(automation_runs))
        .route("/api/v1/automations/{id}/rotate", post(rotate_automation))
        // Triggers (docs/os.md §4.3): what automations are called now.
        .route("/api/v1/triggers", get(list_automations).post(create_automation))
        .route("/api/v1/triggers/{id}", axum::routing::patch(update_automation).delete(delete_automation))
        .route("/api/v1/triggers/{id}/run", post(run_automation))
        .route("/api/v1/triggers/{id}/runs", get(automation_runs))
        .route("/api/v1/triggers/{id}/rotate", post(rotate_automation))
        .route("/api/v1/webhooks/{id}/{token}", post(call_webhook))
        .route("/api/v1/settings/web-search", get(get_web_search).put(put_web_search))
        .route("/api/v1/settings/brief", get(get_brief).put(put_brief))
        .route("/api/v1/settings/subagent-limits", get(get_subagent_limits).put(put_subagent_limits))
        .route("/api/v1/settings/secrets/{name}", put(put_secret).delete(delete_secret))
        .route("/api/v1/settings/commands", get(get_commands_setting).put(put_commands))
        .route("/api/v1/settings/agents", get(get_agents).put(put_agents))
        .route("/api/v1/commands", get(list_commands))
        .route("/api/v1/sessions/{id}/messages", post(send_message))
        .route("/api/v1/sessions/{id}/interrupt", post(interrupt_session))
        .route("/api/v1/sessions/{id}/resume", post(resume_session))
        .route("/api/v1/sessions/{id}/rewind", post(rewind_session))
        .route("/api/v1/sessions/{id}/attachments/{seq}/{index}", get(attachment))
        .route("/api/v1/sessions/{id}/export", get(export_session))
        .route("/api/v1/sessions/{id}/outputs/{seq}", get(output))
        .route("/api/v1/sessions/{id}/labels", put(set_session_labels))
        .route("/api/v1/sessions/{id}/colour", put(set_session_colour))
        .route("/api/v1/sessions/{id}/subagent-model", put(set_subagent_model))
        .route("/api/v1/sessions/{id}/tasks/{task}/stop", post(stop_task))
        .route("/api/v1/sessions/{id}/approvals/{call_id}", post(decide_approval))
        .route("/api/v1/settings/policies", get(get_policies).put(put_policies))
        .route("/api/v1/settings/routes", get(get_routes).put(put_routes))
        .route("/api/v1/settings/channels", get(get_channels).put(put_channels))
        .route("/api/v1/settings/prices", get(get_prices).put(put_prices))
        .route("/api/v1/settings/hosts", get(get_hosts_setting).put(put_hosts))
        .route("/api/v1/settings/workflows", get(get_workflows).put(put_workflows))
        .route("/api/v1/settings/memex", get(get_memex_settings).put(put_memex_settings))
        .route("/api/v1/settings/memex_scopes", get(get_memex_scopes).put(put_memex_scopes))
        .route("/api/v1/settings/claim_policies", get(get_claim_policies).put(put_claim_policies))
        .route("/api/v1/settings/vocabulary", get(get_vocabulary).put(put_vocabulary))
        .route("/api/v1/memex/claims", get(list_claims).post(add_claim))
        .route("/api/v1/memex/claims/{id}", axum::routing::delete(retract_claim))
        .route("/api/v1/memex/things", post(create_thing))
        .route("/api/v1/memex/status", get(memex_status))
        .route("/api/v1/memex/search", get(memex_search))
        .route("/api/v1/memex/get", get(memex_get))
        .route("/api/v1/memex/mentions", get(memex_mentions))
        .route("/api/v1/memex/notes", post(memex_note))
        .route("/api/v1/memex/items/{id}", delete(memex_delete))
        .route("/api/v1/memex/items/{id}/content", get(memex_content))
        .route("/api/v1/memex/upload", post(memex_upload).layer(axum::extract::DefaultBodyLimit::max(80 * 1024 * 1024)))
        .route("/api/v1/memex/scan", post(memex_scan))
        .route("/api/v1/memex/media/{id}/thumb", get(memex_thumb))
        .route("/api/v1/memex/face/{id}", get(memex_face))
        .route("/api/v1/memex/rpc", post(memex_rpc).layer(axum::extract::DefaultBodyLimit::max(40 * 1024 * 1024)))
        .route("/api/v1/mail/attachment", get(mail_attachment))
        .route("/api/v1/memex/trails", get(memex_trails).post(memex_trail_append))
        .route("/api/v1/memex/trails/{id}", get(memex_trail).patch(memex_trail_update).delete(memex_trail_delete))
        .route("/api/v1/memex/trails/{id}/steps/{step}", delete(memex_trail_remove_step))
        .route("/api/v1/settings/memex_folders", get(get_memex_folders).put(put_memex_folders))
        .route("/api/v1/workflows/runs", get(list_workflow_runs))
        .route("/api/v1/workflows/runs/{id}", get(get_workflow_run))
        .route("/api/v1/workflows/runs/{id}/steps/{step}/decide", post(decide_workflow_step))
        .route("/api/v1/workflows/{name}/run", post(run_workflow))
        .route("/api/v1/hosts", get(list_hosts))
        .route("/api/v1/hosts/{name}/check", post(check_host))
        .route("/api/v1/settings/budgets", get(get_budgets).put(put_budgets))
        .route("/api/v1/usage", get(get_usage))
        .route("/api/v1/analytics", get(get_analytics))
        .route("/api/v1/settings/channels/test", post(test_channel))
        .route("/api/v1/push/key", get(push_key))
        .route("/api/v1/push/subscriptions", get(push_devices).post(push_subscribe))
        .route("/api/v1/push/subscriptions/{id}", delete(push_unsubscribe))
        .route("/api/v1/settings/public_url", get(get_public_url).put(put_public_url))
        .route("/approve/{token}", get(crate::notify::approve_page).post(crate::notify::approve_answer))
        .route("/api/v1/sessions/{id}/config", get(session_config))
        .route("/api/v1/sessions/{id}/queue/rejected", delete(dismiss_rejected))
        .route("/api/v1/sessions/{id}/stream", get(session_stream))
        .route("/api/v1/head/fs/list", get(head_fs_list))
        .route("/api/v1/head/fs/read", get(head_fs_read))
        .route("/api/v1/workers/{id}/fs/list", get(fs_list))
        .route("/api/v1/workers/{id}/fs/read", get(fs_read))
        .route("/api/v1/workers/{id}/fs/find", get(fs_find))
        .route("/api/v1/workers/{id}/fs/mkdir", post(fs_mkdir))
        .route("/api/v1/workers/{id}/git/status", get(git_status))
        .route("/api/v1/workers/{id}/git/diff", get(git_diff))
        .route("/api/v1/workers/{id}/git/log", get(git_log))
        .route("/api/v1/workers/{id}/terminals", get(list_terminals).post(open_terminal))
        .route("/api/v1/workers/{id}/terminals/{tid}", delete(close_terminal))
        .route("/api/v1/workers/{id}/terminals/{tid}/attach", get(attach_terminal))
        .route("/api/v1/live", get(live))
        .route("/api/v1/search", get(search))
        .route("/api/v1/sessions/{id}/matches", get(session_matches))
        .route("/api/v1/sessions/{id}/decisions", get(session_decisions))
        .route("/api/v1/sessions/{id}/suggestion", get(session_suggestion))
        .route("/api/v1/sessions/{id}/suggestion/seen", post(suggestion_seen))
        .route("/api/v1/settings/suggestions", get(get_suggestion_settings).put(put_suggestion_settings))
        .route("/api/v1/settings/session_grouping", get(get_session_grouping).put(put_session_grouping))
        .route("/api/v1/settings/own/{name}", get(get_own_setting).delete(drop_own_setting))
        .route("/api/v1/decisions", get(list_decisions).post(add_decision))
        .route("/api/v1/decisions/count", get(count_decisions))
        .route("/api/v1/decisions/{id}", get(get_decision).patch(edit_decision))
        .route("/api/v1/decisions/{id}/answer", post(answer_decision))
        .route("/api/v1/decisions/{id}/session", post(decision_session))
        .route("/api/v1/assistant", post(ask_assistant))
        .route("/api/v1/settings/assistant", get(get_assistant_settings).put(put_assistant_settings))
        .route("/api/v1/catalog", get(get_catalog))
        .route("/api/v1/openapi.json", get(get_openapi))
        .route("/api/v1/system/upgrades", get(get_upgrades))
        .route("/api/v1/system/updates", get(get_updates))
        .route("/api/v1/system/activity", get(get_activity))
        .route("/api/v1/system/activity/counts", get(get_activity_counts))
        .route("/api/v1/system/activity/calls/{id}", get(get_activity_call))
        .route("/api/v1/version", get(get_version))
        .route("/api/v1/elsewhere", get(get_elsewhere))
        .route("/api/v1/locate/sessions/{id}", get(locate_session))
        .route("/api/v1/system/updates/check", post(check_updates))
        .route("/api/v1/system/updates/{version}/apply", post(apply_update))
        .route("/api/v1/system/updates/{version}/skip", post(skip_update))
        .route("/api/v1/settings/updates", get(get_update_settings).put(put_update_settings))
        .route("/api/v1/realms", get(list_realms).post(create_realm))
        .route("/api/v1/realms/{id}", axum::routing::patch(update_realm).delete(delete_realm))
        .route("/api/v1/explorer/map", get(explorer_map))
        .route("/api/v1/realms/{id}/summary", get(realm_summary))
        .route("/api/v1/realms/{id}/activity", get(realm_activity))
        .route("/api/v1/realms/{id}/list", get(realm_list))
        .route("/api/v1/realms/{id}/object", get(realm_object))
        .route("/api/v1/realms/{id}/search", get(realm_search))
        .route("/api/v1/realms/{id}/neighborhood", get(realm_neighborhood))
        .route("/api/v1/packages", get(list_packages).post(add_package))
        .route("/api/v1/packages/{name}/{version}", get(get_package).delete(delete_package))
        .route("/api/v1/packages/{name}/{version}/files/{*path}", get(package_file))
        .route("/api/v1/installs", get(list_installs).post(add_install))
        .route("/api/v1/installs/plan", post(plan_install))
        .route("/api/v1/installs/{id}/upgrade", post(upgrade_install))
        .route("/api/v1/installs/{id}/settings", get(install_settings).put(put_install_settings))
        .route("/api/v1/realms/{id}/effective/{key}", get(effective_list))
        .route("/api/v1/apps/{realm}/{instance}/operations", get(list_operations))
        .route("/api/v1/apps/{realm}/{instance}/accounts", get(list_accounts))
        .route("/api/v1/apps/{realm}/{instance}/accounts/{id}", axum::routing::delete(remove_account))
        .route("/api/v1/apps/{realm}/{instance}/providers/{provider}/connect", post(connect_account))
        .route("/api/v1/apps/{realm}/{instance}/operations/{name}", post(call_operation))
        .route("/api/v1/installs/{id}", axum::routing::delete(remove_install))
        .route("/api/v1/apps", get(list_apps).post(install_app))
        .route("/api/v1/apps/plan", post(plan_app))
        .route("/api/v1/apps/export", get(export_app))
        .route("/api/v1/apps/{realm}", axum::routing::delete(remove_app))
        .route("/api/v1/apps/{realm}/upgrade", post(upgrade_app))
        .route("/api/v1/settings/views", get(get_views_setting).put(put_views))
        .route("/api/v1/views", get(list_views))
        .route("/api/v1/views/{name}/act", post(view_act))
        .route("/api/v1/views/{name}/items", get(view_items))
        .route("/api/v1/views/{name}/items/{id}/thumb", get(view_item_thumb))
        .route("/api/v1/series", get(list_series).post(create_series))
        .route("/api/v1/series/{name}", get(get_series).patch(update_series).delete(delete_series))
        .route("/api/v1/series/{name}/points", post(record_point))
        .route("/api/v1/series/{name}/query", post(query_series))
        .route("/api/v1/grants", get(list_grants).post(create_grant))
        .route("/api/v1/grants/{id}", get(get_grant).delete(revoke_grant))
        .route("/api/v1/grants/{id}/uses", get(grant_uses))
        .route("/api/v1/workers/{id}/realms", put(set_worker_realms))
        .route("/api/v1/connections/{id}/realms", put(set_connection_realms))
        .route("/api/v1/settings/prompts", get(list_prompts))
        .route("/api/v1/settings/prompts/{name}", put(put_prompt))
        .route("/api/v1/collections", get(list_collections).post(create_collection))
        .route("/api/v1/collections/{name}", get(get_collection).patch(update_collection).delete(drop_collection))
        .route("/api/v1/collections/{name}/query", post(query_records))
        .route("/api/v1/collections/{name}/records", get(list_records).post(create_record))
        .route("/api/v1/collections/{name}/records/{id}", get(get_record).put(put_record).delete(delete_record))
        .route("/api/v1/collections/{name}/records/{id}/history", get(record_history))
        .route("/api/v1/catalog/call", post(call_catalog))
        .route("/api/v1/sessions/{id}/answers/{call_id}", post(answer_question))
        .merge(crate::auth::routes())
        .fallback(static_asset)
        .layer(axum::middleware::from_fn_with_state(state.clone(), settings_realm))
        .layer(axum::middleware::from_fn_with_state(state.clone(), crate::auth::require))
        .with_state(state)
}

pub struct ApiErr(pub StatusCode, pub String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(api::ApiError { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast_ref::<crate::UserError>() {
            Some(crate::UserError::BadRequest(m)) => return Self(StatusCode::BAD_REQUEST, m.clone()),
            Some(crate::UserError::NotFound(m)) => return Self(StatusCode::NOT_FOUND, m.clone()),
            Some(crate::UserError::Forbidden(m)) => return Self(StatusCode::FORBIDDEN, m.clone()),
            None => {}
        }
        tracing::warn!("api error: {e:#}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

type ApiResult<T> = Result<T, ApiErr>;

fn parse_worker_id(id: &str) -> ApiResult<WorkerId> {
    id.parse().map_err(|_| ApiErr(StatusCode::BAD_REQUEST, "invalid worker id".into()))
}

pub async fn workers_snapshot(st: &HeadState) -> anyhow::Result<Vec<api::Worker>> {
    let rows = st.store.workers().await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let id = r.worker_id()?;
            let live = st.registry.get(id);
            let status = match (&r.revoked_at, &live) {
                (Some(_), _) => api::WorkerStatus::Revoked,
                (None, Some(_)) => api::WorkerStatus::Online,
                (None, None) => api::WorkerStatus::Offline,
            };
            Some(api::Worker {
                id: id.0,
                name: r.name,
                hostname: r.hostname,
                os: r.os,
                arch: r.arch,
                version: r.version,
                status,
                enrolled_at: r.enrolled_at,
                last_seen: live.as_ref().map(|l| l.last_seen).or(r.last_seen),
                connected_since: live.as_ref().map(|l| l.connected_since),
                rtt_us: live.as_ref().and_then(|l| l.rtt_us),
                capabilities: live.map(|l| l.capabilities).unwrap_or_default(),
                role: r.role,
                realms: serde_json::from_str(&r.realms).unwrap_or_default(),
            })
        })
        .collect())
}

async fn list_workers(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<Vec<api::Worker>>> {
    Ok(Json(workers_for(&st, &caller).await?))
}

/// The workers a caller may use: a realm's, those serving it.
async fn workers_for(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<api::Worker>> {
    let mut workers = workers_snapshot(st).await?;
    if let Some(realm) = &caller.realm {
        workers.retain(|w| crate::realms::serves(&w.realms, realm));
    }
    Ok(workers)
}

/// Creates a one-time enrollment and returns its token.
pub async fn new_enrollment(st: &HeadState, name: Option<&str>) -> anyhow::Result<api::Enrollment> {
    crate::create_enrollment(&st.store, st.pki.ca_sha256(), &st.worker_url, name).await
}

async fn create_enrollment(
    State(st): State<Arc<HeadState>>,
    body: Option<Json<api::CreateEnrollment>>,
) -> ApiResult<Json<api::Enrollment>> {
    let name = body.and_then(|Json(b)| b.name).map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    Ok(Json(new_enrollment(&st, name.as_deref()).await?))
}

async fn revoke_worker(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let id = parse_worker_id(&id)?;
    if !st.store.revoke_worker(id, now_ms()).await? {
        return Err(ApiErr(StatusCode::NOT_FOUND, "no such worker".into()));
    }
    st.registry.kick(id);
    st.registry.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// Revokes and forgets a worker.
async fn delete_worker(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let id = parse_worker_id(&id)?;
    if !st.store.delete_worker(id).await? {
        return Err(ApiErr(StatusCode::NOT_FOUND, "no such worker".into()));
    }
    st.registry.kick(id);
    st.registry.bump();
    Ok(StatusCode::NO_CONTENT)
}

async fn live(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| live_socket(st, caller, socket))
}

/// The fleet and the sessions as they change: a realm's page gets its own
/// sessions and the workers it may use.
async fn live_socket(st: Arc<HeadState>, caller: Caller, mut socket: WebSocket) {
    let mut changes = st.registry.subscribe();
    loop {
        changes.borrow_and_update();
        let Ok(workers) = workers_for(&st, &caller).await else { return };
        let Ok(sessions) = sessions_snapshot(&st, &caller).await else { return };
        for ev in [api::LiveEvent::Workers { workers }, api::LiveEvent::Sessions { sessions }] {
            let Ok(text) = serde_json::to_string(&ev) else { return };
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        tokio::select! {
            _ = st.shutdown.cancelled() => return,
            r = changes.changed() => {
                if r.is_err() { return }
                // Coalesce bursts (e.g. many heartbeats) into one snapshot.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

// --- Connections ----------------------------------------------------------------

fn connection_view(c: crate::store::ConnectionRow) -> api::Connection {
    api::Connection {
        supports_effort: c6_llm::supports_effort(&c.kind, &c.base_url),
        id: c.id,
        name: c.name,
        kind: c.kind,
        base_url: c.base_url,
        default_model: c.default_model,
        has_api_key: c.api_key.is_some(),
        created_at: c.created_at,
        realms: serde_json::from_str(&c.realms).unwrap_or_default(),
    }
}

/// The connections a caller may use: a realm's, those serving it.
async fn list_connections(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<Vec<api::Connection>>> {
    let mut all: Vec<api::Connection> = st.store.connections().await?.into_iter().map(connection_view).collect();
    if let Some(realm) = &caller.realm {
        all.retain(|c| crate::realms::serves(&c.realms, realm));
    }
    Ok(Json(all))
}

async fn create_connection(State(st): State<Arc<HeadState>>, Json(req): Json<api::CreateConnection>) -> ApiResult<Json<api::Connection>> {
    let kind = req.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()).unwrap_or("openai_compat");
    let default_url = match kind {
        "openai_compat" => None,
        "claude_code" => Some("local"),
        "openai" => Some("https://api.openai.com/v1"),
        "anthropic" => Some("https://api.anthropic.com"),
        "chatgpt" => return Err(ApiErr(StatusCode::BAD_REQUEST, "ChatGPT connections are created with Sign in with ChatGPT".into())),
        _ => return Err(ApiErr(StatusCode::BAD_REQUEST, "kind must be openai_compat, openai, anthropic or claude_code".into())),
    };
    let name = req.name.trim();
    let base_url = Some(req.base_url.trim().trim_end_matches('/')).filter(|u| !u.is_empty()).or(default_url).unwrap_or("");
    if name.is_empty() {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "name is required".into()));
    }
    let base_url = if kind == "claude_code" { "local" } else { base_url };
    if kind != "claude_code" && !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "base URL must start with http:// or https://".into()));
    }
    let api_key = req.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(|k| st.secrets.encrypt(k)).transpose()?;
    if api_key.is_none() && !matches!(kind, "openai_compat" | "claude_code") {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "an API key is required for this kind of connection".into()));
    }
    let row = crate::store::ConnectionRow {
        id: uuid::Uuid::now_v7().to_string(),
        name: name.to_owned(),
        kind: kind.to_owned(),
        base_url: base_url.to_owned(),
        api_key,
        default_model: req.default_model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()),
        created_at: now_ms(),
        realms: "[]".into(),
    };
    st.store.insert_connection(&row).await?;
    Ok(Json(connection_view(row)))
}

async fn chatgpt_start(State(st): State<Arc<HeadState>>, Json(req): Json<api::StartChatGptSignIn>) -> ApiResult<Json<api::ChatGptSignIn>> {
    let name = req.name.trim().to_owned();
    // Signing an account in again uses its own client id (each account has
    // one); a new account registers afresh.
    let reauth = match req.connection_id.as_deref().filter(|c| !c.is_empty()) {
        Some(id) => {
            let tokens = chatgpt_saved(&st, id).await?.ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such ChatGPT connection".into()))?;
            Some(crate::oauth::Reauth {
                connection_id: id.to_owned(),
                client_id: tokens.client_id,
                id_token: tokens.id_token,
                sub: tokens.sub,
            })
        }
        None => None,
    };
    let host_id = chatgpt_host_id(&st).await?;
    Ok(Json(api::ChatGptSignIn { authorize_url: st.oauth.start(&name, st.ui_port, reauth, &host_id, req.login_hint.as_deref()) }))
}

/// A ChatGPT connection's saved sign-in.
async fn chatgpt_saved(st: &HeadState, id: &str) -> anyhow::Result<Option<crate::oauth::ChatGptTokens>> {
    let Some(c) = st.store.connection(id).await?.filter(|c| c.kind == "chatgpt") else { return Ok(None) };
    let Some(blob) = c.api_key.as_deref() else { return Ok(None) };
    Ok(serde_json::from_str(&st.secrets.decrypt(blob)?).ok())
}

/// This head's `ext_agent_host_id` for ChatGPT sign-in: made once, then kept
/// (OpenAI requires it to stay the same across sign-ins and restarts).
async fn chatgpt_host_id(st: &HeadState) -> anyhow::Result<String> {
    const KEY: &str = "chatgpt_host_id";
    if let Some(id) = st.store.setting(KEY).await?.filter(|s| !s.is_empty()) {
        return Ok(id);
    }
    let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    st.store.set_setting(KEY, &id).await?;
    Ok(id)
}

/// Saves a sign-in: to the connection signed in again, to the connection
/// already holding this account (and workspace), or as a new connection.
async fn finish_chatgpt(st: &HeadState, callback: &str) -> anyhow::Result<api::Connection> {
    let done = st.oauth.complete(&st.http, callback).await?;
    let secret = st.secrets.encrypt(&serde_json::to_string(&done.tokens)?)?;
    let mut existing = done.reauth.as_ref().map(|r| r.connection_id.clone());
    if existing.is_none() {
        for c in st.store.connections().await?.into_iter().filter(|c| c.kind == "chatgpt") {
            if let Some(t) = chatgpt_saved(st, &c.id).await?
                && !t.sub.is_empty()
                && t.sub == done.tokens.sub
                && t.workspace == done.tokens.workspace
            {
                existing = Some(c.id);
                break;
            }
        }
    }
    if let Some(id) = existing {
        st.store.set_connection_secret(&id, &secret).await?;
        let row = st.store.connection(&id).await?.ok_or_else(|| anyhow::anyhow!("the connection went away"))?;
        return Ok(connection_view(row));
    }
    let who = if done.tokens.email.is_empty() { done.tokens.sub.clone() } else { done.tokens.email.clone() };
    let name = if done.name.is_empty() || done.name == "ChatGPT" { format!("ChatGPT ({who})") } else { done.name };
    let row = crate::store::ConnectionRow {
        id: uuid::Uuid::now_v7().to_string(),
        name,
        kind: "chatgpt".into(),
        base_url: crate::oauth::RESOURCE.into(),
        api_key: Some(secret),
        default_model: None,
        created_at: now_ms(),
        realms: "[]".into(),
    };
    st.store.insert_connection(&row).await?;
    Ok(connection_view(row))
}

async fn chatgpt_complete(
    State(st): State<Arc<HeadState>>,
    Json(req): Json<api::CompleteChatGptSignIn>,
) -> ApiResult<Json<api::Connection>> {
    Ok(Json(finish_chatgpt(&st, &req.callback_url).await?))
}

/// The OAuth redirect lands here when the browser runs on the head machine.
async fn chatgpt_callback(State(st): State<Arc<HeadState>>, uri: Uri) -> Response {
    let query = uri.query().unwrap_or_default();
    match finish_chatgpt(&st, query).await {
        Ok(_) => axum::response::Redirect::to("/settings?signed_in=chatgpt").into_response(),
        Err(e) => {
            let msg =
                e.downcast_ref::<crate::UserError>().map(|u| u.to_string()).unwrap_or_else(|| "sign-in failed; see the head's log".into());
            let html = format!(
                "<!doctype html><meta charset=utf-8><title>carbon6</title><p style=\"font-family:sans-serif\">ChatGPT sign-in failed: {}</p><p><a href=\"/settings\">Back to settings</a></p>",
                html_escape(&msg)
            );
            (StatusCode::BAD_REQUEST, axum::response::Html(html)).into_response()
        }
    }
}

fn is_fingerprinted(path: &str) -> bool {
    let stem = path.rsplit('/').next().unwrap_or(path).split('.').next().unwrap_or("");
    stem.rsplit_once('-').is_some_and(|(_, h)| h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn delete_connection(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if !st.store.delete_connection(&id).await? {
        return Err(ApiErr(StatusCode::NOT_FOUND, "no such connection".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Lists models from the provider. The head calls the provider directly here;
/// all model calls for sessions happen on workers.
async fn connection_models(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<Vec<String>>> {
    let c = st.store.connection(&id).await?.ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such connection".into()))?;
    let key = match c.kind.as_str() {
        "chatgpt" => crate::sessions::chatgpt_tokens(&st, &c.id).await?.access_token,
        _ => c.api_key.as_deref().map(|k| st.secrets.decrypt(k)).transpose()?.unwrap_or_default(),
    };
    list_models(&c.kind, &c.base_url, &key).await.map(Json)
}

/// Lists a provider's models; this doubles as the connection check, since it
/// needs the server to be reachable and the key to be accepted.
async fn list_models(kind: &str, base_url: &str, key: &str) -> ApiResult<Vec<String>> {
    let models = match kind {
        // Claude Code model aliases; full model ids work too.
        "claude_code" => return Ok(["opus", "sonnet", "haiku", "fable"].map(String::from).to_vec()),
        "anthropic" => c6_llm::anthropic::list_models(base_url, key).await,
        _ => c6_llm::openai_chat::list_models(base_url, key).await,
    };
    models.map_err(|e| {
        let msg = match &e {
            c6_llm::LlmError::Http { status: 401 | 403, .. } => "the server rejected the API key".to_owned(),
            c6_llm::LlmError::Http { status: 404, .. } => "the server has no model list at this URL (check the base URL)".to_owned(),
            c6_llm::LlmError::Network(n) => format!("cannot reach the server: {n}"),
            other => other.to_string(),
        };
        ApiErr(StatusCode::BAD_GATEWAY, msg)
    })
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct TestConnection {
    kind: Option<String>,
    #[serde(default)]
    base_url: String,
    api_key: Option<String>,
    /// Test a saved connection, optionally with a new key.
    connection_id: Option<String>,
}

/// Checks connection details before (or after) saving and returns the
/// models the server offers.
async fn test_connection(State(st): State<Arc<HeadState>>, Json(req): Json<TestConnection>) -> ApiResult<Json<Vec<String>>> {
    let kind = req.kind.as_deref().filter(|k| !k.is_empty()).unwrap_or("openai_compat");
    let base = match (req.base_url.trim(), kind) {
        ("", "anthropic") => "https://api.anthropic.com",
        ("", "openai") => "https://api.openai.com/v1",
        ("", "claude_code") => "local",
        ("", _) => return Err(ApiErr(StatusCode::BAD_REQUEST, "enter a base URL".into())),
        (u, _) => u,
    };
    let mut key = req.api_key.unwrap_or_default().trim().to_owned();
    if key.is_empty()
        && let Some(id) = req.connection_id
        && let Some(c) = st.store.connection(&id).await?
    {
        key = c.api_key.as_deref().map(|k| st.secrets.decrypt(k)).transpose()?.unwrap_or_default();
    }
    Ok(Json(list_models(kind, base.trim_end_matches('/'), &key).await?))
}

// --- Sessions -------------------------------------------------------------------

const SESSION_LIST_LIMIT: i64 = 200;
const EVENT_PAGE: i64 = 1000;

/// The session list a caller sees: its realm's, or (you, root) every
/// realm's most recent.
pub async fn sessions_snapshot(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<api::Session>> {
    let working = st.store.subagents_running().await?;
    Ok(st
        .store
        .sessions(caller.realm.as_deref(), SESSION_LIST_LIMIT)
        .await?
        .iter()
        .map(|r| api::Session { subagents_running: working.get(&r.id).copied().unwrap_or(0), ..crate::convert::session(r) })
        .collect())
}

async fn list_sessions(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<Vec<api::Session>>> {
    Ok(Json(sessions_snapshot(&st, &caller).await?))
}

async fn create_session(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(mut req): Json<api::CreateSession>,
) -> ApiResult<Json<api::Session>> {
    req.realm = Some(caller.realm_for_new(req.realm.as_deref())?);
    Ok(Json(crate::sessions::create(&st, req).await?))
}

async fn create_side_chat(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::CreateSideChat>,
) -> ApiResult<Json<api::Session>> {
    Ok(Json(crate::sessions::create_side_chat(&st, &id, req).await?))
}

/// The web UI's CA, for devices to trust (public: it is a certificate).
async fn ui_ca_pem(State(st): State<Arc<HeadState>>) -> Response {
    match &st.ui_ca {
        Some(ca) => (
            [(header::CONTENT_TYPE, "application/x-pem-file"), (header::CONTENT_DISPOSITION, "attachment; filename=\"carbon6-ca.pem\"")],
            ca.cert_pem().to_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "this head does not issue its own HTTPS certificate").into_response(),
    }
}

/// The same CA in DER, as Android's certificate installer wants it.
async fn ui_ca_crt(State(st): State<Arc<HeadState>>) -> Response {
    match &st.ui_ca {
        Some(ca) => (
            [
                (header::CONTENT_TYPE, "application/x-x509-ca-cert"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"carbon6-ca.crt\""),
            ],
            ca.cert_der().to_vec(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "this head does not issue its own HTTPS certificate").into_response(),
    }
}

/// The same CA as an Apple configuration profile (iPhone, iPad, Mac).
async fn ui_ca_profile(State(st): State<Arc<HeadState>>) -> Response {
    match &st.ui_ca {
        Some(ca) => (
            [
                (header::CONTENT_TYPE, "application/x-apple-aspen-config"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"carbon6-ca.mobileconfig\""),
            ],
            ca.mobileconfig(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "this head does not issue its own HTTPS certificate").into_response(),
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct FindQuery {
    q: String,
}

async fn session_matches(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Query(f): Query<FindQuery>,
) -> ApiResult<Json<api::SessionMatches>> {
    Ok(Json(crate::search::session_matches(&st.store, &id, &f.q).await?))
}

/// The session's open decisions, to answer in it.
async fn session_decisions(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<api::Decision>>> {
    Ok(Json(crate::decisions::of_session(&st, &id, &caller).await?))
}

/// What you'd likely type next (docs/suggestions.md): 204 when there's none.
async fn session_suggestion(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    Ok(match crate::suggestions::get(&st, &id, &caller).await? {
        Some(s) => Json(s).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

async fn suggestion_seen(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(s): Json<api::SuggestionSeen>,
) -> ApiResult<StatusCode> {
    crate::suggestions::seen(&st, &id, s.for_seq, s.accepted).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_suggestion_settings(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::SuggestionSettings>> {
    let raw = st.store.setting(crate::suggestions::KEY).await?.unwrap_or_default();
    Ok(Json(serde_json::from_str(&raw).unwrap_or_default()))
}

async fn put_suggestion_settings(State(st): State<Arc<HeadState>>, Json(s): Json<api::SuggestionSettings>) -> ApiResult<StatusCode> {
    st.store.set_setting(crate::suggestions::KEY, &serde_json::to_string(&s).unwrap_or_default()).await?;
    Ok(StatusCode::NO_CONTENT)
}

const SESSION_GROUPING: &str = "session_grouping";

/// The per-realm setting behind a Settings page's name.
fn per_realm_key(name: &str) -> ApiResult<&'static str> {
    match name {
        "suggestions" => Ok(crate::suggestions::KEY),
        "web-search" => Ok(crate::sessions::WEB_SEARCH),
        "assistant" => Ok(crate::assistant::SETTING),
        "subagent-limits" => Ok(crate::sessions::SUBAGENT_LIMITS),
        "brief" => Ok(crate::memex::BRIEF),
        _ => Err(ApiErr(StatusCode::NOT_FOUND, format!("{name:?} is not a setting a realm sets for itself"))),
    }
}

/// Whether the caller's realm sets this itself (else root's applies).
async fn get_own_setting(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let key = per_realm_key(&name)?;
    let own = match &caller.realm {
        Some(r) => st.store.has_own_setting(key, r).await?,
        None => true,
    };
    Ok(Json(serde_json::json!({ "own": own })))
}

/// The caller's realm goes back to root's value.
async fn drop_own_setting(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let key = per_realm_key(&name)?;
    let Some(r) = &caller.realm else {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "root's is the default every realm inherits; change it instead".into()));
    };
    st.store.drop_own_setting(key, r).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Each realm groups its own list.
fn grouping_key(caller: &Caller) -> String {
    match &caller.realm {
        Some(r) => format!("{SESSION_GROUPING}:{r}"),
        None => SESSION_GROUPING.to_owned(),
    }
}

async fn get_session_grouping(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<api::SessionGrouping>> {
    let raw = st.store.setting(&grouping_key(&caller)).await?.unwrap_or_default();
    Ok(Json(serde_json::from_str(&raw).unwrap_or_default()))
}

/// Each level is a label key (`repo`, `client`), at most once.
async fn put_session_grouping(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(mut g): Json<api::SessionGrouping>,
) -> ApiResult<StatusCode> {
    for l in &mut g.levels {
        *l = l.trim().trim_end_matches(':').to_owned();
    }
    let mut seen = std::collections::HashSet::new();
    for l in &g.levels {
        if l.is_empty() || l.len() > 64 || !l.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c)) || !seen.insert(l.clone()) {
            return Err(ApiErr(
                StatusCode::BAD_REQUEST,
                format!("{l:?} is not a label key to group by (letters, digits, - _ ., each once)"),
            ));
        }
    }
    st.store.set_setting(&grouping_key(&caller), &serde_json::to_string(&g).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn search(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<api::SearchQuery>,
) -> ApiResult<Json<api::SearchResponse>> {
    let mut found = crate::search::search(&st.store, &q).await?;
    found.results.retain(|r| caller.sees(&r.session.realm));
    Ok(Json(found))
}

async fn get_session(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::SessionDetail>> {
    let row = st.store.session(&id).await?.ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such session".into()))?;
    let worker_id: Option<WorkerId> = row.worker_id.parse().ok();
    let worker = match worker_id {
        Some(w) => st.store.worker(w).await?,
        None => None,
    };
    let queued = st.store.queued_inputs(&id).await?.iter().map(crate::convert::queued).collect();
    Ok(Json(api::SessionDetail {
        session: crate::convert::session(&row),
        worker_name: worker.map(|w| w.name).unwrap_or_else(|| "(removed)".into()),
        worker_online: worker_id.is_some_and(|w| st.registry.get(w).is_some()),
        queued,
    }))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct UpdateSession {
    title: Option<String>,
    archived: Option<bool>,
}

async fn update_session(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Json(req): Json<UpdateSession>) -> ApiResult<StatusCode> {
    let mut found = true;
    if let Some(t) = req.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        found &= st.store.set_title(&id, t).await?;
    }
    if let Some(a) = req.archived {
        found &= st.store.set_archived(&id, a).await?;
    }
    if !found {
        return Err(ApiErr(StatusCode::NOT_FOUND, "no such session".into()));
    }
    st.registry.bump();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct After {
    #[serde(default)]
    after: u64,
}

async fn session_events(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Query(q): Query<After>,
) -> ApiResult<Json<Vec<api::Event>>> {
    let events = st.store.events_after(&id, q.after, EVENT_PAGE).await?;
    Ok(Json(events.iter().filter_map(crate::convert::event).collect()))
}

#[derive(serde::Deserialize)]
struct ChildrenQuery {
    /// `1` (or `true`): the whole tree below the session, not just its
    /// children.
    #[serde(default)]
    tree: String,
}

async fn session_children(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Query(q): Query<ChildrenQuery>,
) -> ApiResult<Json<Vec<api::Session>>> {
    let tree = matches!(q.tree.as_str(), "1" | "true");
    let rows = if tree { st.store.descendant_sessions(&id).await? } else { st.store.child_sessions(&id).await? };
    Ok(Json(rows.iter().map(crate::convert::session).collect()))
}

/// MCP servers and skills are stored as validated JSON arrays and included in
/// each new session's configuration.
async fn get_setting(st: &HeadState, key: &str) -> ApiResult<Json<serde_json::Value>> {
    let raw = st.store.setting(key).await?.unwrap_or_else(|| "[]".into());
    Ok(Json(serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!([]))))
}

async fn get_mcp_servers(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    get_setting(&st, crate::sessions::MCP_SERVERS).await
}

async fn put_mcp_servers(State(st): State<Arc<HeadState>>, Json(servers): Json<Vec<c6_mcp::ServerConfig>>) -> ApiResult<StatusCode> {
    let mut names = std::collections::HashSet::new();
    for s in &servers {
        if s.name.trim().is_empty() || !names.insert(s.name.clone()) {
            return Err(ApiErr(StatusCode::BAD_REQUEST, "each MCP server needs a unique, non-empty name".into()));
        }
    }
    st.store.set_setting(crate::sessions::MCP_SERVERS, &serde_json::to_string(&servers).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_skills(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    get_setting(&st, crate::sessions::SKILLS).await
}

/// Names in a list must be valid, and two items of the same name must apply
/// to different labels (the more specific one wins where both match).
fn check_names<'a>(what: &str, items: impl Iterator<Item = (&'a str, &'a [String])>, valid: impl Fn(&str) -> bool) -> ApiResult<()> {
    let mut seen = std::collections::HashSet::new();
    for (n, applies_to) in items {
        if !valid(n) {
            return Err(ApiErr(StatusCode::BAD_REQUEST, format!("{what} name {n:?} is not valid")));
        }
        let mut key: Vec<String> = applies_to.iter().map(|s| s.trim().trim_end_matches('/').to_owned()).filter(|s| !s.is_empty()).collect();
        key.sort();
        key.dedup();
        if !seen.insert((n, key)) {
            return Err(ApiErr(StatusCode::BAD_REQUEST, format!("two {what}s named {n:?} apply to the same labels")));
        }
    }
    Ok(())
}

fn plain_name(n: &str) -> bool {
    !n.trim().is_empty() && n.len() <= 80 && n.chars().all(|c| c.is_alphanumeric() || "_-:.".contains(c))
}

async fn put_skills(State(st): State<Arc<HeadState>>, Json(skills): Json<Vec<api::SkillDef>>) -> ApiResult<StatusCode> {
    check_names("skill", skills.iter().map(|s| (s.name.as_str(), s.applies_to.as_slice())), plain_name)?;
    if let Some(s) = skills.iter().find(|s| s.body.trim().is_empty()) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, format!("skill {:?} needs instructions", s.name)));
    }
    st.store.set_setting(crate::sessions::SKILLS, &serde_json::to_string(&skills).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_commands_setting(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    get_setting(&st, crate::sessions::COMMANDS).await
}

async fn put_commands(State(st): State<Arc<HeadState>>, Json(commands): Json<Vec<api::CommandDef>>) -> ApiResult<StatusCode> {
    check_names("command", commands.iter().map(|c| (c.name.as_str(), c.applies_to.as_slice())), api::valid_command_name)?;
    if let Some(c) = commands.iter().find(|c| c.body.trim().is_empty()) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, format!("/{} needs a prompt", c.name)));
    }
    st.store.set_setting(crate::sessions::COMMANDS, &serde_json::to_string(&commands).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_agents(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    get_setting(&st, crate::sessions::AGENTS).await
}

async fn put_agents(State(st): State<Arc<HeadState>>, Json(agents): Json<Vec<api::AgentDef>>) -> ApiResult<StatusCode> {
    check_names("agent", agents.iter().map(|a| (a.name.as_str(), a.applies_to.as_slice())), |n| plain_name(n) && n != "general-purpose")?;
    if let Some(a) = agents.iter().find(|a| a.prompt.trim().is_empty()) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, format!("agent {:?} needs instructions", a.name)));
    }
    st.store.set_setting(crate::sessions::AGENTS, &serde_json::to_string(&agents).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The slash commands the composer offers: built-ins and the enabled custom
/// ones (a custom command replaces a built-in of the same name).
#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ForSession {
    #[serde(default)]
    session: Option<String>,
}

/// `?session=`: only the commands that apply to that session (by its labels),
/// the most specific variant of each name.
async fn list_commands(State(st): State<Arc<HeadState>>, Query(q): Query<ForSession>) -> ApiResult<Json<Vec<api::CommandInfo>>> {
    // A session's realm's commands; otherwise home's.
    let realm = match &q.session {
        Some(id) => crate::realms::of_session(&st, id).await?.unwrap_or_else(|| crate::realms::HOME.to_owned()),
        None => crate::realms::HOME.to_owned(),
    };
    let all = crate::sessions::custom_commands(&st, &realm).await?;
    let custom: Vec<api::CommandDef> = match &q.session {
        Some(id) => {
            let row = st.store.session(id).await?.ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such session".into()))?;
            let labels = row.all_labels();
            let mut names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
            names.dedup();
            let mut out = Vec::new();
            for n in names {
                if !out.iter().any(|c: &api::CommandDef| c.name == n)
                    && let Some(c) = c6_api::labels::best(all.iter().filter(|c| c.name == n), |c| &c.applies_to, &labels)
                {
                    out.push(c.clone());
                }
            }
            out
        }
        None => {
            let mut out: Vec<api::CommandDef> = Vec::new();
            for c in &all {
                if !out.iter().any(|x| x.name == c.name) {
                    out.push(c.clone());
                }
            }
            out
        }
    };
    let mut out: Vec<api::CommandInfo> = api::BUILTIN_COMMANDS
        .iter()
        .filter(|(n, _, _)| !custom.iter().any(|c| c.name == *n))
        .map(|(n, d, h)| api::CommandInfo {
            name: (*n).into(),
            description: (*d).into(),
            argument_hint: (*h).into(),
            source: "built-in".into(),
        })
        .collect();
    out.extend(custom.into_iter().map(|c| {
        let description = if c.description.trim().is_empty() {
            c.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_start_matches('#').trim().chars().take(100).collect()
        } else {
            c.description
        };
        api::CommandInfo { name: c.name, description, argument_hint: c.argument_hint, source: "custom".into() }
    }));
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}

async fn get_brief(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::BriefSetting>> {
    let raw = st.store.setting(crate::memex::BRIEF).await?.unwrap_or_default();
    Ok(Json(serde_json::from_str(&raw).unwrap_or_default()))
}

async fn put_brief(State(st): State<Arc<HeadState>>, Json(mut s): Json<api::BriefSetting>) -> ApiResult<StatusCode> {
    let hhmm = |t: &str| chrono::NaiveTime::parse_from_str(t.trim(), "%H:%M").map(|t| t.format("%H:%M").to_string());
    let mut times = Vec::new();
    for t in s.times.iter().filter(|t| !t.trim().is_empty()) {
        times.push(hhmm(t).map_err(|_| ApiErr(StatusCode::BAD_REQUEST, format!("{t:?}: times are HH:MM")))?);
    }
    times.sort();
    times.dedup();
    s.times = times;
    if !s.close.trim().is_empty() {
        s.close = hhmm(&s.close).map_err(|_| ApiErr(StatusCode::BAD_REQUEST, "the close of day is HH:MM".into()))?;
    }
    st.store.set_setting(crate::memex::BRIEF, &serde_json::to_string(&s).unwrap_or_default()).await?;
    crate::memex::reload(&st).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_web_search(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::WebSearchSetting>> {
    let raw = st.store.setting(crate::sessions::WEB_SEARCH).await?.unwrap_or_default();
    Ok(Json(serde_json::from_str(&raw).unwrap_or_default()))
}

async fn put_web_search(State(st): State<Arc<HeadState>>, Json(mut s): Json<api::WebSearchSetting>) -> ApiResult<StatusCode> {
    s.provider = s.provider.trim().to_owned();
    s.url = s.url.trim().to_owned();
    s.secret = s.secret.trim().to_owned();
    let bad = |m: &str| Err(ApiErr(StatusCode::BAD_REQUEST, m.into()));
    match s.provider.as_str() {
        "" => {}
        "brave" | "tavily" if s.secret.is_empty() => return bad("choose the secret that holds the API key"),
        "brave" | "tavily" => {
            if !st.store.secrets(None).await?.iter().any(|x| x.name == s.secret) {
                return bad("no secret by that name (add it under Secrets first)");
            }
        }
        "searxng" if !(s.url.starts_with("http://") || s.url.starts_with("https://")) => {
            return bad("SearXNG needs the instance's http(s) URL");
        }
        "searxng" => {}
        _ => return bad("the provider is brave, tavily or searxng"),
    }
    st.store.set_setting(crate::sessions::WEB_SEARCH, &serde_json::to_string(&s).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_subagent_limits(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::SubagentLimits>> {
    Ok(Json(crate::sessions::subagent_limits(&st, None).await?))
}

async fn put_subagent_limits(State(st): State<Arc<HeadState>>, Json(l): Json<api::SubagentLimits>) -> ApiResult<StatusCode> {
    if !(1..=8).contains(&l.max_depth) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "the depth is 1 to 8".into()));
    }
    if !(1..=64).contains(&l.max_running) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "the number running at once is 1 to 64".into()));
    }
    st.store.set_setting(crate::sessions::SUBAGENT_LIMITS, &serde_json::to_string(&l).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_templates(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Template>>> {
    Ok(Json(crate::templates::templates(&st).await?))
}

async fn put_templates(State(st): State<Arc<HeadState>>, Json(templates): Json<Vec<api::Template>>) -> ApiResult<StatusCode> {
    crate::templates::check(&templates)?;
    st.store.set_setting(crate::templates::TEMPLATES, &serde_json::to_string(&templates).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_memories(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<Vec<api::Memory>>> {
    Ok(Json(crate::memory::list(&st, &caller).await?))
}

async fn create_memory(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(mut m): Json<api::Memory>,
) -> ApiResult<Json<api::Memory>> {
    m.id = String::new();
    let realm = caller.realm_for_new(None)?;
    Ok(Json(crate::memory::put(&st, m, &realm).await?))
}

async fn update_memory(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(mut m): Json<api::Memory>,
) -> ApiResult<Json<api::Memory>> {
    m.id = id;
    // Admitted only if the note is in the caller's realm; it stays there.
    Ok(Json(crate::memory::put(&st, m, crate::realms::HOME).await?))
}

async fn delete_memory(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    crate::memory::delete(&st, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Triggers the caller may see; `?realm=`: one realm's.
async fn list_automations(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::Automation>>> {
    let caller = match r.realm.as_deref().filter(|x| !x.is_empty()) {
        Some(x) => Caller::in_realm(&caller.realm_for_new(Some(x))?),
        None => caller,
    };
    Ok(Json(crate::triggers::list(&st, &caller).await?))
}

/// A new trigger in `?realm=` (default: the caller's, else home).
async fn create_automation(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(e): Json<api::AutomationEdit>,
) -> ApiResult<Json<api::Automation>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::triggers::create(&st, e, &realm).await?))
}

async fn update_automation(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(e): Json<api::AutomationEdit>,
) -> ApiResult<Json<api::Automation>> {
    crate::triggers::check_edit(&st, &id, Some(&e)).await?;
    Ok(Json(crate::triggers::update(&st, &id, e).await?))
}

async fn delete_automation(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    crate::triggers::check_edit(&st, &id, None).await?;
    crate::triggers::delete(&st, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_automation(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::TemplateRun>> {
    Ok(Json(crate::triggers::run_now(&st, &id).await?))
}

async fn rotate_automation(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::Automation>> {
    Ok(Json(crate::triggers::rotate(&st, &id).await?))
}

/// A webhook call from outside (no sign-in: the token in the URL is the
/// credential). 404 for a wrong token, so URLs cannot be probed.
async fn call_webhook(
    State(st): State<Arc<HeadState>>,
    Path((id, token)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<api::TemplateRun>)> {
    if body.len() > crate::triggers::MAX_WEBHOOK_BODY {
        return Err(ApiErr(StatusCode::PAYLOAD_TOO_LARGE, "the body is too large".into()));
    }
    match crate::triggers::webhook(&st, &id, &token, &body).await? {
        Some(run) => Ok((StatusCode::ACCEPTED, Json(run))),
        None => Err(ApiErr(StatusCode::NOT_FOUND, "not found".into())),
    }
}

async fn run_template(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Option<Json<api::RunTemplate>>,
) -> ApiResult<Json<api::TemplateRun>> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let realm = match body.realm.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => caller.realm_for_new(Some(r))?,
        // You: the realm that has it (an app's template is in the app's).
        None if caller.realm.is_none() => crate::templates::realm_of(&st, &name).await?,
        None => caller.realm_for_new(None)?,
    };
    Ok(Json(crate::templates::run(&st, &name, &body.input, &[], &realm).await?))
}

async fn all_templates(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<Vec<api::RealmTemplate>>> {
    Ok(Json(crate::templates::all(&st, &caller).await?))
}

/// Secrets: a realm's own and root's it has none of (`?realm=`), or every
/// realm's.
async fn list_secrets(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::SecretInfo>>> {
    // A realm's caller sees its own realm's; you, every realm's.
    Ok(Json(match (r.realm.as_deref().filter(|x| !x.trim().is_empty()), &caller.realm) {
        (Some(_), _) | (None, Some(_)) => st.store.secrets_for(&realm_for(&st, &caller, &r).await?).await?,
        (None, None) => st.store.secrets(None).await?,
    }))
}

/// A secret of a realm (`?realm=`, default home).
async fn put_secret(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Path(name): Path<String>,
    Json(req): Json<api::PutSecret>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    if !api::valid_secret_name(&name) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "a secret's name is an environment variable name: A-Z, 0-9 and _".into()));
    }
    let applies_to = crate::sessions::check_selector(&req.applies_to)?;
    let value = match req.value.as_deref().filter(|v| !v.is_empty()) {
        Some(v) => Some(st.secrets.encrypt(v)?),
        None => None,
    };
    if !st.store.put_secret(&realm, &name, value, req.description.trim(), &applies_to).await? {
        return Err(ApiErr(StatusCode::BAD_REQUEST, format!("{name} is new: give it a value")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_secret(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    if !st.store.delete_secret(&realm, &name).await? {
        return Err(ApiErr(StatusCode::NOT_FOUND, "no such secret".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn get_hooks(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    get_setting(&st, crate::sessions::HOOKS).await
}

async fn put_hooks(State(st): State<Arc<HeadState>>, Json(hooks): Json<Vec<api::Hook>>) -> ApiResult<StatusCode> {
    for (i, h) in hooks.iter().enumerate() {
        let which = if h.name.trim().is_empty() { format!("hook {}", i + 1) } else { format!("hook {:?}", h.name) };
        if !api::HOOK_EVENTS.contains(&h.event.as_str()) {
            return Err(ApiErr(StatusCode::BAD_REQUEST, format!("{which}: unknown event {:?}", h.event)));
        }
        if h.command.trim().is_empty() {
            return Err(ApiErr(StatusCode::BAD_REQUEST, format!("{which} needs a command")));
        }
        if h.timeout_s.is_some_and(|t| t == 0 || t > 3600) {
            return Err(ApiErr(StatusCode::BAD_REQUEST, format!("{which}: the timeout must be 1 to 3600 seconds")));
        }
    }
    st.store.set_setting(crate::sessions::HOOKS, &serde_json::to_string(&hooks).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn send_message(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::SendMessage>,
) -> ApiResult<Json<api::QueuedInput>> {
    Ok(Json(crate::sessions::send_input(&st, &id, &req).await?))
}

/// An attachment of a user message, as sent.
async fn attachment(State(st): State<Arc<HeadState>>, Path((id, seq, index)): Path<(String, u64, usize)>) -> ApiResult<Response> {
    let ev = st.store.events_after(&id, seq.saturating_sub(1), 1).await?;
    let a = match ev.first().filter(|e| e.seq == seq).and_then(|e| e.kind.as_ref()) {
        Some(c6_proto::session_event::Kind::User(u)) => u.attachments.get(index).cloned(),
        Some(c6_proto::session_event::Kind::ToolResult(r)) => r.images.get(index).cloned(),
        _ => None,
    };
    let Some(a) = a else { return Err(ApiErr(StatusCode::NOT_FOUND, "no such attachment".into())) };
    // Text is served as plain text, never as something a browser would run;
    // an image opened on its own (an SVG can hold script) gets an origin of
    // its own.
    let mime = if a.mime.starts_with("image/") { a.mime } else { "text/plain; charset=utf-8".into() };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=31536000, immutable".into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            (header::CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'".into()),
        ],
        a.data,
    )
        .into_response())
}

async fn set_session_labels(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::SetLabels>,
) -> ApiResult<Json<api::Session>> {
    Ok(Json(crate::sessions::set_labels(&st, &id, &req.labels).await?))
}

async fn set_session_colour(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::SetColour>,
) -> ApiResult<Json<api::Session>> {
    Ok(Json(crate::sessions::set_colour(&st, &id, &req.colour).await?))
}

async fn set_subagent_model(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::SetSubagentModel>,
) -> ApiResult<Json<api::Session>> {
    Ok(Json(crate::sessions::set_subagent_model(&st, &id, &req).await?))
}

async fn session_config(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::EffectiveConfig>> {
    Ok(Json(crate::sessions::effective_config(&st, &id).await?))
}

#[derive(serde::Deserialize)]
struct Download {
    #[serde(default)]
    download: Option<String>,
}

/// A published output's content. Pages the agent wrote (HTML, SVG) are
/// served sandboxed: an origin of their own, so they cannot act as you.
async fn output(State(st): State<Arc<HeadState>>, Path((id, seq)): Path<(String, u64)>, Query(q): Query<Download>) -> ApiResult<Response> {
    let ev = st.store.events_after(&id, seq.saturating_sub(1), 1).await?;
    let Some(c6_proto::session_event::Kind::Output(o)) = ev.into_iter().find(|e| e.seq == seq).and_then(|e| e.kind) else {
        return Err(ApiErr(StatusCode::NOT_FOUND, "no such output".into()));
    };
    let file = o.name.rsplit('/').next().unwrap_or("output").replace('"', "");
    let mut headers = vec![
        (header::CONTENT_TYPE, if o.mime.starts_with("text/") { format!("{}; charset=utf-8", o.mime) } else { o.mime.clone() }),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        (header::CACHE_CONTROL, "private, max-age=31536000, immutable".to_owned()),
    ];
    // Anything that could run script (HTML, SVG, unknown types) gets an
    // origin of its own. PDFs, images and text cannot, and Chrome does not
    // show PDFs in a sandbox.
    let inert = o.mime == "application/pdf" || (o.mime.starts_with("image/") && o.mime != "image/svg+xml") || o.mime == "text/plain";
    if !inert {
        headers.push((
            header::CONTENT_SECURITY_POLICY,
            "sandbox allow-scripts allow-popups allow-forms allow-modals allow-downloads".to_owned(),
        ));
    }
    if q.download.is_some() {
        headers.push((header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file}\"")));
    }
    let mut resp = o.data.into_response();
    for (k, v) in headers {
        resp.headers_mut().insert(k, v.parse().map_err(anyhow::Error::from)?);
    }
    Ok(resp)
}

/// The session as a Markdown file.
async fn export_session(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Response> {
    let session = st.store.session(&id).await?.ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such session".into()))?;
    let mut events = Vec::new();
    loop {
        let page = st.store.events_after(&id, events.last().map_or(0, |e: &c6_proto::SessionEvent| e.seq), EVENT_PAGE).await?;
        let done = (page.len() as i64) < EVENT_PAGE;
        events.extend(page);
        if done {
            break;
        }
    }
    let name: String = session
        .title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(60)
        .collect();
    let name = if name.is_empty() { "session".to_owned() } else { name };
    Ok((
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8".to_owned()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}.md\"")),
        ],
        crate::export::markdown(&session, &events),
    )
        .into_response())
}

async fn rewind_session(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::RewindRequest>,
) -> ApiResult<StatusCode> {
    crate::sessions::rewind(&st, &id, &req).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn stop_task(State(st): State<Arc<HeadState>>, Path((id, task)): Path<(String, String)>) -> ApiResult<StatusCode> {
    if crate::sessions::stop_task(&st, &id, &task).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiErr(StatusCode::CONFLICT, "worker is not connected".into()))
    }
}

async fn decide_approval(
    State(st): State<Arc<HeadState>>,
    Path((id, call_id)): Path<(String, String)>,
    Json(d): Json<api::ApprovalDecision>,
) -> ApiResult<StatusCode> {
    if crate::sessions::decide(&st, &id, &call_id, d.allow).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiErr(StatusCode::CONFLICT, "worker is not connected".into()))
    }
}

async fn get_memex_settings(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::MemexSettings>> {
    Ok(Json(crate::memex::settings(&st).await?))
}

async fn put_memex_settings(State(st): State<Arc<HeadState>>, Json(s): Json<api::MemexSettings>) -> ApiResult<StatusCode> {
    crate::memex::check_settings(&st, &s).await?;
    st.store.set_setting(crate::memex::SETTINGS, &serde_json::to_string(&s).map_err(anyhow::Error::from)?).await?;
    crate::memex::reload(&st).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_memex_scopes(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::MemexScope>>> {
    Ok(Json(crate::memex::scopes(&st).await?))
}

async fn put_memex_scopes(State(st): State<Arc<HeadState>>, Json(s): Json<Vec<api::MemexScope>>) -> ApiResult<StatusCode> {
    crate::memex::check_scopes(&s)?;
    st.store.set_setting(crate::memex::SCOPES, &serde_json::to_string(&s).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_claim_policies(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::ClaimPolicy>>> {
    Ok(Json(st.store.setting(crate::claims::POLICIES).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()))
}

async fn put_claim_policies(State(st): State<Arc<HeadState>>, Json(p): Json<Vec<api::ClaimPolicy>>) -> ApiResult<StatusCode> {
    crate::claims::check_policies(&p)?;
    st.store.set_setting(crate::claims::POLICIES, &serde_json::to_string(&p).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_vocabulary(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::VocabularyEntry>>> {
    Ok(Json(st.store.setting(crate::claims::VOCABULARY).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()))
}

async fn put_vocabulary(State(st): State<Arc<HeadState>>, Json(v): Json<Vec<api::VocabularyEntry>>) -> ApiResult<StatusCode> {
    crate::claims::check_vocabulary(&v)?;
    st.store.set_setting(crate::claims::VOCABULARY, &serde_json::to_string(&v).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Claims about a thing or an item (docs/apps.md §9): `about`
/// (`thing:<key>` or `item:<id>`), `label`, `owner`, `policy` (`all`: every
/// claim, each saying whether the realm's policies believe it), `same_as`.
#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ClaimsQuery {
    about: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    policy: String,
    #[serde(default)]
    same_as: bool,
    #[serde(default)]
    realm: Option<String>,
}

/// Who asserts what you or a token add: you, or the token's realm.
fn claim_owner(caller: &Caller) -> (&'static str, String) {
    match &caller.realm {
        Some(r) => ("realm", format!("token:{r}")),
        None => ("you", "you".into()),
    }
}

async fn list_claims(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<ClaimsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = realm_for(&st, &caller, &api::InRealm { realm: q.realm.clone() }).await?;
    let (owner, by) = claim_owner(&caller);
    let params = serde_json::json!({"about": q.about, "label": q.label, "filter_owner": q.owner, "policy": q.policy, "same_as": q.same_as});
    Ok(Json(crate::claims::call(&st, &realm, "memex.claims.list", params, owner, &by).await?))
}

async fn add_claim(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(c): Json<api::ClaimIn>,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = realm_for(&st, &caller, &r).await?;
    let (owner, by) = claim_owner(&caller);
    let params = serde_json::to_value(&c).map_err(anyhow::Error::from)?;
    Ok(Json(crate::claims::call(&st, &realm, "memex.claims.add", params, owner, &by).await?))
}

/// Retracts a claim of yours (only the owner retracts).
async fn retract_claim(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<i64>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = realm_for(&st, &caller, &r).await?;
    let (owner, by) = claim_owner(&caller);
    Ok(Json(crate::claims::call(&st, &realm, "memex.claims.retract", serde_json::json!({"id": id}), owner, &by).await?))
}

/// A thing anyone may make (`{kind, name, key?}`).
async fn create_thing(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(t): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = realm_for(&st, &caller, &r).await?;
    let (owner, by) = claim_owner(&caller);
    Ok(Json(crate::claims::call(&st, &realm, "memex.things.create", t, owner, &by).await?))
}

async fn memex_status(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::MemexStatus>> {
    Ok(Json(crate::memex::status(&st).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct MemexSearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    kinds: String,
    #[serde(default)]
    about: String,
    #[serde(default)]
    limit: Option<u32>,
}

/// Comma-separated list.
fn list_param(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()
}

async fn memex_search(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<MemexSearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let query = api::MemexQuery { query: q.q, kinds: list_param(&q.kinds), about: list_param(&q.about), limit: q.limit };
    let params = serde_json::to_value(query).map_err(anyhow::Error::from)?;
    // A realm's token: with what other realms granted it (docs/os.md §3.7).
    Ok(Json(match &caller.realm {
        Some(realm) => crate::fabric::search_granted(&st, realm, &format!("token:{realm}"), params).await?,
        None => crate::memex::call_in(&st, caller.memex_realm(), "memex.search", params).await?,
    }))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct IdQuery {
    #[serde(default)]
    id: String,
    #[serde(default)]
    q: String,
}

async fn memex_get(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<IdQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = serde_json::json!({"id": q.id});
    Ok(Json(match &caller.realm {
        Some(realm) => crate::fabric::get_granted(&st, realm, &format!("token:{realm}"), params).await?,
        None => crate::memex::call_in(&st, caller.memex_realm(), "memex.get", params).await?,
    }))
}

async fn memex_mentions(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<IdQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::memex::call_in(&st, caller.memex_realm(), "memex.mention", serde_json::json!({"q": q.q})).await?))
}

async fn memex_note(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(n): Json<api::MemexNoteIn>,
) -> ApiResult<Json<serde_json::Value>> {
    if n.text.trim().is_empty() {
        return Err(crate::bad_request("a note needs text").into());
    }
    let params = serde_json::json!({"text": n.text, "labels": n.about, "made_by": "you"});
    Ok(Json(crate::memex::call_in(&st, caller.memex_realm(), "memex.note", params).await?))
}

async fn get_google_client(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    headers: HeaderMap,
) -> ApiResult<Json<api::GoogleClient>> {
    let realm = realm_for(&st, &caller, &r).await?;
    let mut c = crate::google::get_client(&st, &realm).await?;
    c.redirect_uri =
        crate::google::redirect_uri(&st, headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()).or(origin_of(&headers).as_deref()))
            .await;
    Ok(Json(c))
}

/// The page's origin, from Host (GET requests carry no Origin).
fn origin_of(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let proto = headers.get("x-forwarded-proto").and_then(|p| p.to_str().ok()).unwrap_or("http");
    Some(format!("{proto}://{host}"))
}

/// A realm's Google OAuth client (its own); from the console or a root
/// token without `?realm=`, root's (every realm's that has none).
async fn put_google_client(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(c): Json<api::GoogleClientIn>,
) -> ApiResult<StatusCode> {
    if c.client_id.trim().is_empty() {
        return Err(crate::bad_request("the client ID is required").into());
    }
    let realm = match (&caller.realm, r.realm.as_deref().filter(|x| !x.trim().is_empty())) {
        (None, None) => crate::realms::ROOT.to_owned(),
        _ => realm_for(&st, &caller, &r).await?,
    };
    crate::google::set_client(&st, &realm, &c).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn google_connect(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = realm_for(&st, &caller, &r).await?;
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()).map(str::to_owned).or_else(|| origin_of(&headers));
    Ok(Json(serde_json::json!({"authorize_url": crate::google::connect(&st, &realm, origin.as_deref()).await?})))
}

/// A page's origin: what it says, else from Host.
fn page_origin(headers: &HeaderMap) -> Option<String> {
    headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()).map(str::to_owned).or_else(|| origin_of(headers))
}

/// Slack (docs/slack.md): a realm's setup, client, app, workspaces.
async fn slack_setup(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    headers: HeaderMap,
) -> ApiResult<Json<api::SlackSetup>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::slack::setup(&st, &realm, page_origin(&headers).as_deref()).await?))
}

async fn slack_client(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(c): Json<api::SlackClientIn>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    crate::slack::set_client(&st, &realm, &c).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn slack_install(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Option<api::Install>>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::slack::install(&st, &realm).await?))
}

async fn slack_connect(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(serde_json::json!({"authorize_url": crate::slack::connect(&st, &realm, page_origin(&headers).as_deref()).await?})))
}

async fn slack_sync(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    crate::slack::sync_now(&st, &realm).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn slack_remove(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    crate::slack::remove(&st, &realm, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Importing sessions from Claude Code and Codex (docs/imports.md).
async fn get_session_imports(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::SessionImports>> {
    Ok(Json(crate::imports::settings(&st).await?))
}

async fn put_session_imports(State(st): State<Arc<HeadState>>, Json(s): Json<api::SessionImports>) -> ApiResult<StatusCode> {
    crate::imports::set_settings(&st, &s).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What waits on you in each realm (docs/realms-ux.md §6): counts only,
/// for the realm switcher. A realm's token learns only about its own.
async fn get_elsewhere(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<Vec<api::RealmWaiting>>> {
    let mut out = Vec::new();
    for r in crate::realms::list(&st, &Caller::root()).await? {
        // Root's is the console's (your browser's only): installs, grants.
        let console = r.id == crate::realms::ROOT;
        if (console && !caller.browser) || !(caller.sees_others() || caller.sees(&r.id)) {
            continue;
        }
        let decisions = crate::decisions::open_count(&st, &Caller::in_realm(&r.id)).await?;
        let approvals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE realm = ? AND awaiting > 0 AND archived = 0")
            .bind(&r.id)
            .fetch_one(&st.store.pool)
            .await
            .map_err(anyhow::Error::from)?;
        out.push(api::RealmWaiting { id: r.id, name: r.name, waiting: (decisions + approvals).max(0) as u64 });
    }
    Ok(Json(out))
}

/// The realm a session is in, for links from before realms had their own
/// pages (`/sessions/<id>`).
async fn locate_session(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    match crate::realms::of_session(&st, &id).await? {
        Some(realm) if caller.sees_others() || caller.sees(&realm) => Ok(Json(serde_json::json!({"realm": realm}))),
        _ => Err(ApiErr(StatusCode::NOT_FOUND, "no such session".into())),
    }
}

async fn get_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({"version": c6_core::build()}))
}

/// Updating carbon6 from GitHub releases (docs/updates.md).
async fn get_activity(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::activity::get(&st).await?))
}

async fn get_activity_counts(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::activity::counts(&st).await?))
}

async fn get_activity_call(State(st): State<Arc<HeadState>>, Path(id): Path<u64>) -> ApiResult<Json<serde_json::Value>> {
    crate::activity::call(&st, id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such call (they're kept in memory, the last 200)".into()))
}

async fn get_updates(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::Updates>> {
    Ok(Json(crate::updates::status(&st).await?))
}

async fn check_updates(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::Updates>> {
    // A failed look is in the status (check_error), not an error here.
    let _ = crate::updates::check(&st).await;
    Ok(Json(crate::updates::status(&st).await?))
}

async fn apply_update(State(st): State<Arc<HeadState>>, Path(version): Path<String>) -> ApiResult<Json<api::Updates>> {
    crate::updates::apply(&st, &version).await?;
    Ok(Json(crate::updates::status(&st).await?))
}

async fn skip_update(State(st): State<Arc<HeadState>>, Path(version): Path<String>) -> ApiResult<Json<api::Updates>> {
    crate::updates::skip(&st, &version).await?;
    Ok(Json(crate::updates::status(&st).await?))
}

async fn get_update_settings(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::UpdateSettings>> {
    Ok(Json(crate::updates::settings(&st).await?))
}

async fn put_update_settings(State(st): State<Arc<HeadState>>, Json(s): Json<api::UpdateSettings>) -> ApiResult<StatusCode> {
    crate::updates::set_settings(&st, &s).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What was imported: sessions by source and realm.
async fn list_imports(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::imports::summary(&st).await?))
}

/// Asks you to install the Gmail, Calendar and Contacts apps in a realm.
async fn google_install(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::Install>>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::google::install(&st, &realm).await?))
}

async fn google_accounts(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::GoogleAccount>>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::google::accounts(&st, &realm).await?))
}

async fn google_remove(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    crate::google::remove(&st, &realm, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct RpcIn {
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

/// The mail client, calendar, contacts, photos and the memex's review,
/// commitments and graph call the memex through here (you, signed in:
/// everything is in scope).
async fn memex_rpc(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(asked): Query<api::InRealm>,
    Json(r): Json<RpcIn>,
) -> ApiResult<Json<serde_json::Value>> {
    // The realm you look at (a realm's token: its own); home by default.
    let realm = match &asked.realm {
        Some(x) if !x.trim().is_empty() => realm_for(&st, &caller, &asked).await?,
        _ => caller.memex_realm().to_owned(),
    };
    let ok = ["memex.mail.", "memex.calendar.", "memex.contacts.", "memex.knowledge.", "memex.chat.", "memex.triage.", "memex.requests.", "memex.relations."]
        .iter()
        .any(|p| r.method.starts_with(p))
        // What apps' kits say about chat is theirs to write.
        && r.method != "memex.chat.put"
        || r.method.starts_with("memex.media.")
        || matches!(
            r.method.as_str(),
            "memex.google"
                | "memex.google_sync"
                | "memex.feeds"
                | "memex.commitments"
                | "memex.commitment"
                | "memex.review"
                | "memex.review_decide"
                | "memex.merge"
                | "memex.summarize"
                | "memex.graph"
                | "memex.months"
                | "memex.timeline"
                | "memex.related"
                | "memex.ingest"
                | "memex.types"
        );
    if !ok {
        return Err(crate::bad_request(format!("{} is not callable here", r.method)).into());
    }
    // Ingest from outside is a batch of typed items, with labels checked as
    // uploads are; the head's own generic items are not sent through here.
    if r.method == "memex.ingest" {
        let batch: api::MemexIngest = serde_json::from_value(r.params.clone())
            .map_err(|e| crate::bad_request(format!("memex.ingest takes {{\"items\": [...]}}: {e}")))?;
        for item in &batch.items {
            crate::sessions::check_labels(&item.labels)?;
        }
    }
    // The mail client's accounts and syncs are the Google apps'.
    match r.method.as_str() {
        "memex.google" => return Ok(Json(crate::google::status(&st, &realm).await?)),
        "memex.google_sync" => {
            crate::google::sync_now(&st, &realm).await?;
            return Ok(Json(serde_json::json!({})));
        }
        _ => {}
    }
    let mut params = if r.params.is_object() { r.params } else { serde_json::json!({}) };
    params["made_by"] = serde_json::json!("you");
    // Only the head writes as an app (its kits' output).
    if let Some(o) = params.as_object_mut() {
        o.remove("as");
    }
    // The graph shows what the realm's reader policies believe (docs/apps.md §9.3).
    if r.method == "memex.graph" {
        params["policies"] = serde_json::json!(crate::claims::policies(&st, &realm).await);
    }
    Ok(Json(crate::memex::call_in(&st, &realm, &r.method, params).await?))
}

#[derive(serde::Deserialize)]
struct AttachmentQuery {
    item: String,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    realm: Option<String>,
}

/// A mail attachment, served like an output (nosniff; sandboxed when it
/// could run script) and as a download unless it is an image or PDF.
async fn mail_attachment(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<AttachmentQuery>,
) -> ApiResult<Response> {
    use base64::Engine;
    let realm = match &q.realm {
        Some(r) if !r.trim().is_empty() => realm_for(&st, &caller, &api::InRealm { realm: q.realm.clone() }).await?,
        _ => caller.memex_realm().to_owned(),
    };
    let v = crate::memex::call_in(&st, &realm, "memex.mail.attachment", serde_json::json!({"item_id": q.item, "index": q.index})).await?;
    let data = base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap_or("")).map_err(anyhow::Error::from)?;
    let name = v["name"].as_str().unwrap_or("attachment").replace(['"', '\\', '\n', '\r'], "_");
    let mime = v["mime"].as_str().unwrap_or("application/octet-stream").to_owned();
    let inline = mime.starts_with("image/") && !mime.contains("svg") || mime == "application/pdf";
    let disposition = format!("{}; filename=\"{name}\"", if inline { "inline" } else { "attachment" });
    let mut resp = (
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, disposition),
            (header::HeaderName::from_static("x-content-type-options"), "nosniff".to_owned()),
        ],
        data,
    )
        .into_response();
    resp.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'"),
    );
    Ok(resp)
}

async fn memex_upload(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(u): Json<api::MemexUpload>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::sessions::check_labels(&u.labels)?;
    Ok(Json(crate::memex::call_in(&st, caller.memex_realm(), "memex.upload", serde_json::to_value(u).map_err(anyhow::Error::from)?).await?))
}

#[derive(serde::Deserialize)]
struct FrameQuery {
    frame: Option<u32>,
}

/// A photo's thumbnail (or a video's keyframe), as JPEG.
async fn memex_thumb(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<FrameQuery>,
) -> ApiResult<Response> {
    let v = crate::memex::call_in(&st, caller.memex_realm(), "memex.media.thumb", serde_json::json!({"id": id, "frame": q.frame})).await?;
    jpeg_response(&v)
}

/// The clearest face of a person, as JPEG.
async fn memex_face(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>, Path(id): Path<String>) -> ApiResult<Response> {
    let v = crate::memex::call_in(&st, caller.memex_realm(), "memex.media.face", serde_json::json!({"id": id})).await?;
    jpeg_response(&v)
}

fn jpeg_response(v: &serde_json::Value) -> ApiResult<Response> {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap_or_default()).map_err(anyhow::Error::from)?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=3600")], data).into_response())
}

async fn memex_scan(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::memex::call_in(&st, caller.memex_realm(), "memex.scan", serde_json::json!({})).await?))
}

/// An item's original (a document's file), served like an output: never
/// sniffed, and sandboxed when it could run script.
async fn memex_content(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Response> {
    use base64::Engine;
    let v = crate::memex::call_in(&st, caller.memex_realm(), "memex.content", serde_json::json!({"id": id})).await?;
    let data = base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap_or("")).map_err(anyhow::Error::from)?;
    let name = v["name"].as_str().unwrap_or("file").replace(['"', '\\', '\n', '\r'], "_");
    let mime = v["mime"].as_str().unwrap_or("application/octet-stream").to_owned();
    let disposition =
        if q.contains_key("download") { format!("attachment; filename=\"{name}\"") } else { format!("inline; filename=\"{name}\"") };
    let mut resp = (
        [
            (header::CONTENT_TYPE, mime.clone()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::HeaderName::from_static("x-content-type-options"), "nosniff".to_owned()),
        ],
        data,
    )
        .into_response();
    if mime.contains("html") || mime.contains("svg") || mime == "application/octet-stream" {
        resp.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static("sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'"),
        );
    }
    Ok(resp)
}

async fn memex_trails(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::memex::call_in(&st, caller.memex_realm(), "memex.trails", serde_json::json!({})).await?))
}

async fn memex_trail(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::memex::call_in(&st, caller.memex_realm(), "memex.trail", serde_json::json!({"id": id})).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct TrailStepIn {
    trail: String,
    target: String,
    #[serde(default)]
    comment: String,
}

async fn memex_trail_append(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(s): Json<TrailStepIn>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = serde_json::json!({"trail": s.trail, "target": s.target, "comment": s.comment, "made_by": "you"});
    Ok(Json(crate::memex::call_in(&st, caller.memex_realm(), "memex.trail_append", params).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct TrailEdit {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

async fn memex_trail_update(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(e): Json<TrailEdit>,
) -> ApiResult<StatusCode> {
    crate::memex::call_in(
        &st,
        caller.memex_realm(),
        "memex.trail_update",
        serde_json::json!({"id": id, "name": e.name, "description": e.description}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn memex_trail_delete(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    crate::memex::call_in(&st, caller.memex_realm(), "memex.trail_delete", serde_json::json!({"id": id})).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn memex_trail_remove_step(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((_, step)): Path<(String, i64)>,
) -> ApiResult<StatusCode> {
    crate::memex::call_in(&st, caller.memex_realm(), "memex.trail_remove_step", serde_json::json!({"step": step})).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_memex_folders(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::MemexFolder>>> {
    Ok(Json(serde_json::from_str(&st.store.setting(crate::memex::FOLDERS).await?.unwrap_or_default()).unwrap_or_default()))
}

async fn put_memex_folders(State(st): State<Arc<HeadState>>, Json(f): Json<Vec<api::MemexFolder>>) -> ApiResult<StatusCode> {
    crate::memex::check_folders(&f)?;
    st.store.set_setting(crate::memex::FOLDERS, &serde_json::to_string(&f).map_err(anyhow::Error::from)?).await?;
    crate::memex::reload(&st).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn memex_delete(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let v = crate::memex::call_in(&st, caller.memex_realm(), "memex.delete", serde_json::json!({"id": id})).await?;
    if v["deleted"].as_bool() == Some(true) { Ok(StatusCode::NO_CONTENT) } else { Err(crate::not_found("no such item").into()) }
}

async fn get_workflows(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Workflow>>> {
    Ok(Json(crate::workflows::workflows(&st).await?))
}

async fn put_workflows(State(st): State<Arc<HeadState>>, Json(w): Json<Vec<api::Workflow>>) -> ApiResult<StatusCode> {
    crate::workflows::check(&st, &w).await?;
    st.store.set_setting(crate::workflows::WORKFLOWS, &serde_json::to_string(&w).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Runs a workflow of the caller's realm (yours: home's).
async fn run_workflow(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Json(r): Json<api::RunWorkflow>,
) -> ApiResult<Json<api::WorkflowRun>> {
    let realm = caller.realm_for_new(None)?;
    Ok(Json(crate::workflows::start(&st, &name, &r.input, "manual", &realm).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct WorkflowRunsQuery {
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

async fn list_workflow_runs(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<WorkflowRunsQuery>,
) -> ApiResult<Json<Vec<api::WorkflowRun>>> {
    let workflow = q.workflow.as_deref().filter(|w| !w.is_empty());
    Ok(Json(crate::workflows::runs(&st, workflow, caller.realm.as_deref(), q.limit.unwrap_or(20)).await?))
}

async fn get_workflow_run(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::WorkflowRun>> {
    Ok(Json(crate::workflows::get(&st, &id).await?))
}

async fn decide_workflow_step(
    State(st): State<Arc<HeadState>>,
    Path((id, step)): Path<(String, String)>,
    Json(d): Json<api::ApprovalDecision>,
) -> ApiResult<StatusCode> {
    if crate::workflows::decide(&st, &id, &step, d.allow).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiErr(StatusCode::CONFLICT, "that step is not waiting for an answer".into()))
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct RunsQuery {
    #[serde(default)]
    limit: Option<u32>,
}

async fn automation_runs(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Vec<api::AutomationRun>>> {
    Ok(Json(crate::triggers::runs(&st, &id, q.limit.unwrap_or(20)).await?))
}

async fn get_hosts_setting(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Host>>> {
    Ok(Json(crate::hosts::hosts(&st).await?))
}

async fn put_hosts(State(st): State<Arc<HeadState>>, Json(h): Json<Vec<api::Host>>) -> ApiResult<StatusCode> {
    crate::hosts::check_hosts(&h)?;
    st.store.set_setting(crate::hosts::HOSTS, &serde_json::to_string(&h).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_hosts(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::HostStatus>>> {
    Ok(Json(crate::hosts::list(&st).await?))
}

async fn check_host(State(st): State<Arc<HeadState>>, Path(name): Path<String>) -> ApiResult<Json<api::HostCheck>> {
    Ok(Json(crate::hosts::check_now(&st, &name).await?))
}

async fn get_prices(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Price>>> {
    Ok(Json(crate::usage::prices(&st).await?))
}

async fn put_prices(State(st): State<Arc<HeadState>>, Json(p): Json<Vec<api::Price>>) -> ApiResult<StatusCode> {
    crate::usage::check_prices(&p)?;
    st.store.set_setting(crate::usage::PRICES, &serde_json::to_string(&p).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_budgets(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Budget>>> {
    Ok(Json(crate::usage::budgets(&st).await?))
}

async fn put_budgets(State(st): State<Arc<HeadState>>, Json(b): Json<Vec<api::Budget>>) -> ApiResult<StatusCode> {
    crate::usage::check_budgets(&b)?;
    st.store.set_setting(crate::usage::BUDGETS, &serde_json::to_string(&b).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct UsageQuery {
    #[serde(default)]
    days: Option<u32>,
    #[serde(default)]
    labels: String,
}

async fn get_usage(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<api::UsageReport>> {
    let filter = api::labels::parse_labels(&q.labels);
    Ok(Json(crate::usage::report(&st, q.days.unwrap_or(30), &filter, caller.realm.as_deref()).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct AnalyticsQuery {
    #[serde(default)]
    days: Option<u32>,
    #[serde(default)]
    labels: String,
    /// The label key to group sessions by (default `repo`).
    #[serde(default)]
    by: String,
}

async fn get_analytics(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<AnalyticsQuery>,
) -> ApiResult<Json<api::AnalyticsReport>> {
    let filter = api::labels::parse_labels(&q.labels);
    Ok(Json(crate::analytics::report(&st, q.days.unwrap_or(30), &filter, &q.by, caller.realm.as_deref()).await?))
}

async fn get_channels(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Channel>>> {
    Ok(Json(crate::notify::channels(&st).await?))
}

/// Saving channels for the first time also takes the page's address as the
/// one links point to, unless one is set.
async fn put_channels(State(st): State<Arc<HeadState>>, headers: HeaderMap, Json(c): Json<Vec<api::Channel>>) -> ApiResult<StatusCode> {
    crate::notify::check_channels(&c)?;
    st.store.set_setting(crate::notify::CHANNELS, &serde_json::to_string(&c).map_err(anyhow::Error::from)?).await?;
    let unset = st.store.setting(crate::notify::PUBLIC_URL).await?.is_none_or(|u| u.trim().is_empty());
    if unset && let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|o| o.to_str().ok()) {
        st.store.set_setting(crate::notify::PUBLIC_URL, origin).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn push_key(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::PushKey>> {
    Ok(Json(api::PushKey { key: crate::push::public_key(&st).await? }))
}

async fn push_devices(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::PushDevice>>> {
    Ok(Json(crate::push::devices(&st).await?))
}

async fn push_subscribe(State(st): State<Arc<HeadState>>, Json(s): Json<api::PushSubscription>) -> ApiResult<Json<api::PushDevice>> {
    Ok(Json(crate::push::subscribe(&st, &s).await?))
}

async fn push_unsubscribe(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if crate::push::unsubscribe(&st, &id).await? { Ok(StatusCode::NO_CONTENT) } else { Err(crate::not_found("no such device").into()) }
}

async fn test_channel(State(st): State<Arc<HeadState>>, Json(t): Json<api::TestChannel>) -> ApiResult<StatusCode> {
    crate::notify::test(&st, &t.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_public_url(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::PublicUrl>> {
    Ok(Json(api::PublicUrl { url: st.store.setting(crate::notify::PUBLIC_URL).await?.unwrap_or_default() }))
}

async fn put_public_url(State(st): State<Arc<HeadState>>, Json(p): Json<api::PublicUrl>) -> ApiResult<StatusCode> {
    let url = p.url.trim().trim_end_matches('/');
    if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(crate::bad_request("the address is http(s)://host[:port]").into());
    }
    st.store.set_setting(crate::notify::PUBLIC_URL, url).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_routes(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Route>>> {
    let raw = st.store.setting(crate::sessions::ROUTES).await?.unwrap_or_default();
    Ok(Json(serde_json::from_str(&raw).unwrap_or_default()))
}

async fn put_routes(State(st): State<Arc<HeadState>>, Json(r): Json<Vec<api::Route>>) -> ApiResult<StatusCode> {
    crate::sessions::check_routes(&st, &r).await?;
    st.store.set_setting(crate::sessions::ROUTES, &serde_json::to_string(&r).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_policies(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Policy>>> {
    let raw = st.store.setting(crate::sessions::POLICIES).await?.unwrap_or_default();
    Ok(Json(serde_json::from_str(&raw).unwrap_or_default()))
}

async fn put_policies(State(st): State<Arc<HeadState>>, Json(p): Json<Vec<api::Policy>>) -> ApiResult<StatusCode> {
    crate::sessions::check_policies(&p)?;
    st.store.set_setting(crate::sessions::POLICIES, &serde_json::to_string(&p).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_session(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::ResumeRequest>,
) -> ApiResult<Json<api::QueuedInput>> {
    Ok(Json(crate::sessions::resume(&st, &id, &req).await?))
}

async fn interrupt_session(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if crate::sessions::interrupt(&st, &id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiErr(StatusCode::CONFLICT, "worker is not connected".into()))
    }
}

async fn dismiss_rejected(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    st.store.dismiss_rejected(&id).await?;
    crate::sessions::publish_queue(&st, &id).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn session_stream(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Query(q): Query<After>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| stream_socket(st, id, q.after, socket))
}

/// Durable events after `after` (gap-free, in order), then live updates.
async fn stream_socket(st: Arc<HeadState>, session_id: String, after: u64, mut socket: WebSocket) {
    // Subscribe before reading history so nothing falls between the two.
    let mut live = st.hub.subscribe(&session_id);
    let mut last = after;

    async fn send(socket: &mut WebSocket, msg: &api::StreamMsg) -> bool {
        match serde_json::to_string(msg) {
            Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
            Err(_) => false,
        }
    }
    async fn catch_up(st: &HeadState, session_id: &str, last: &mut u64, socket: &mut WebSocket) -> bool {
        loop {
            let Ok(events) = st.store.events_after(session_id, *last, EVENT_PAGE).await else { return false };
            if events.is_empty() {
                return true;
            }
            for ev in &events {
                *last = ev.seq;
                if let Some(event) = crate::convert::event(ev)
                    && !send(socket, &api::StreamMsg::Event { event }).await
                {
                    return false;
                }
            }
        }
    }

    if let Ok(rows) = st.store.queued_inputs(&session_id).await
        && !send(&mut socket, &api::StreamMsg::Queue { queued: rows.iter().map(crate::convert::queued).collect() }).await
    {
        return;
    }
    if !catch_up(&st, &session_id, &mut last, &mut socket).await || !send(&mut socket, &api::StreamMsg::Synced).await {
        return;
    }
    loop {
        tokio::select! {
            _ = st.shutdown.cancelled() => return,
            msg = live.recv() => match msg {
                Ok(api::StreamMsg::Event { event }) => {
                    if event.seq <= last { continue }
                    if event.seq != last + 1 {
                        if !catch_up(&st, &session_id, &mut last, &mut socket).await { return }
                        continue;
                    }
                    last = event.seq;
                    if !send(&mut socket, &api::StreamMsg::Event { event }).await { return }
                }
                Ok(other) => if !send(&mut socket, &other).await { return },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if !catch_up(&st, &session_id, &mut last, &mut socket).await { return }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

// --- Worker files, git, terminals (proxied to the worker) ----------------------

const RPC_TIMEOUT: Duration = Duration::from_secs(30);

async fn worker_call(st: &HeadState, id: &str, method: &str, params: serde_json::Value) -> ApiResult<Json<serde_json::Value>> {
    let worker_id = parse_worker_id(id)?;
    match st.registry.call(worker_id, method, params, RPC_TIMEOUT).await {
        Ok(v) => Ok(Json(v)),
        Err(e) if e.contains("not connected") || e.contains("disconnected") => Err(ApiErr(StatusCode::CONFLICT, e)),
        Err(e) => Err(ApiErr(StatusCode::BAD_REQUEST, e)),
    }
}

type Params = Query<std::collections::HashMap<String, String>>;

fn params_json(q: std::collections::HashMap<String, String>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in q {
        let v = match v.as_str() {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(v),
        };
        m.insert(k, v);
    }
    serde_json::Value::Object(m)
}

#[derive(serde::Deserialize)]
struct HeadFsQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    hidden: String,
}

async fn head_fs_list(Query(q): Query<HeadFsQuery>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::files::list(&q.path, matches!(q.hidden.as_str(), "true" | "1")).await?))
}

async fn head_fs_read(State(st): State<Arc<HeadState>>, Query(q): Query<HeadFsQuery>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::files::read(&st, &q.path).await?))
}

async fn fs_list(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Query(q): Params) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "fs.list", params_json(q)).await
}

async fn fs_find(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Query(q): Params) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "fs.find", params_json(q)).await
}

async fn fs_read(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Query(q): Params) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "fs.read", params_json(q)).await
}

async fn fs_mkdir(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "fs.mkdir", body).await
}

async fn git_status(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Query(q): Params) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "git.status", params_json(q)).await
}

async fn git_diff(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Query(q): Params) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "git.diff", params_json(q)).await
}

async fn git_log(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Query(q): Params) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "git.log", params_json(q)).await
}

async fn list_terminals(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "pty.list", serde_json::json!({})).await
}

async fn open_terminal(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "pty.open", body).await
}

async fn close_terminal(State(st): State<Arc<HeadState>>, Path((id, tid)): Path<(String, String)>) -> ApiResult<Json<serde_json::Value>> {
    worker_call(&st, &id, "pty.close", serde_json::json!({ "terminal_id": tid })).await
}

/// Browser ↔ terminal: binary frames carry bytes both ways; text frames are
/// control messages (`{"type":"resize","cols":..,"rows":..}` in,
/// `{"type":"exit","code":..}` / `{"type":"error",..}` out).
async fn attach_terminal(State(st): State<Arc<HeadState>>, Path((id, tid)): Path<(String, String)>, ws: WebSocketUpgrade) -> Response {
    let Ok(worker_id) = parse_worker_id(&id) else { return StatusCode::BAD_REQUEST.into_response() };
    ws.on_upgrade(move |socket| terminal_socket(st, worker_id, tid, socket))
}

async fn terminal_socket(st: Arc<HeadState>, worker_id: WorkerId, tid: String, mut socket: WebSocket) {
    use c6_proto::{PtyFrame, PtySize, pty_frame};
    let control = |v: serde_json::Value| Message::Text(v.to_string().into());

    let (token, waiting) = st.terminals.expect(worker_id);
    if let Err(e) =
        st.registry.call(worker_id, "pty.attach", serde_json::json!({"terminal_id": tid, "token": token}), Duration::from_secs(10)).await
    {
        st.terminals.forget(&token);
        let _ = socket.send(control(serde_json::json!({"type": "error", "message": e}))).await;
        return;
    }
    let (mut inbound, tx) = match tokio::time::timeout(Duration::from_secs(10), waiting).await {
        Ok(Ok(pair)) => pair,
        _ => {
            st.terminals.forget(&token);
            let _ =
                socket.send(control(serde_json::json!({"type": "error", "message": "the worker did not open the terminal stream"}))).await;
            return;
        }
    };
    let frame = |f| Ok(PtyFrame { frame: Some(f) });
    loop {
        tokio::select! {
            _ = st.shutdown.cancelled() => return,
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => if tx.send(frame(pty_frame::Frame::Data(b.to_vec()))).await.is_err() { return },
                Some(Ok(Message::Text(t))) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                    if v["type"] == "resize" {
                        let size = PtySize { cols: v["cols"].as_u64().unwrap_or(0) as u32, rows: v["rows"].as_u64().unwrap_or(0) as u32 };
                        if tx.send(frame(pty_frame::Frame::Resize(size))).await.is_err() { return }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            f = inbound.message() => match f {
                Ok(Some(PtyFrame { frame: Some(pty_frame::Frame::Data(d)) })) => {
                    if socket.send(Message::Binary(d.into())).await.is_err() { return }
                }
                Ok(Some(PtyFrame { frame: Some(pty_frame::Frame::Exit(e)) })) => {
                    let _ = socket.send(control(serde_json::json!({"type": "exit", "code": e.code}))).await;
                    return;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            },
        }
    }
}

#[derive(RustEmbed)]
#[folder = "../c6-ui/dist"]
#[allow_missing = true]
struct UiAssets;

async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return ApiErr(StatusCode::NOT_FOUND, "not found".into()).into_response();
    }
    // SPA: unknown paths serve index.html and the client router takes over.
    let (path, file) = match UiAssets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => (path, f),
        None => match UiAssets::get("index.html") {
            Some(f) => ("index.html", f),
            None => return (StatusCode::NOT_FOUND, "web UI not built; run `make ui`").into_response(),
        },
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Trunk fingerprints its outputs (`name-<16 hex>.ext`); those never
    // change. Everything else (index.html, vendored scripts) is revalidated.
    let cache = if is_fingerprinted(path) { "public, max-age=31536000, immutable" } else { "no-cache" };
    ([(header::CONTENT_TYPE, mime.as_ref().to_owned()), (header::CACHE_CONTROL, cache.to_owned())], file.data).into_response()
}

// --- The assistant and the operation catalog ----------------------------------

/// An assistant conversation: a realm's page gets that realm's assistant.
async fn ask_assistant(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(mut req): Json<api::AskAssistant>,
) -> ApiResult<Json<api::Session>> {
    if let Some(realm) = &caller.realm {
        req.realm = Some(realm.clone());
    }
    Ok(Json(crate::sessions::create_assistant(&st, req).await?))
}

async fn get_assistant_settings(State(st): State<Arc<HeadState>>) -> ApiResult<Json<api::AssistantSettings>> {
    Ok(Json(serde_json::from_str(&st.store.setting(crate::assistant::SETTING).await?.unwrap_or_default()).unwrap_or_default()))
}

async fn put_assistant_settings(State(st): State<Arc<HeadState>>, Json(s): Json<api::AssistantSettings>) -> ApiResult<StatusCode> {
    crate::assistant::check_settings(&st, &s).await?;
    st.store.set_setting(crate::assistant::SETTING, &serde_json::to_string(&s).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Every operation (for the CLI).
async fn get_catalog() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "areas": crate::catalog::areas().iter().map(|(a, d)| serde_json::json!({"area": a, "what": d})).collect::<Vec<_>>(),
        "operations": crate::catalog::ops().iter().map(crate::catalog::describe).collect::<Vec<_>>(),
    }))
}

/// `?realm=` on a Settings request: its lists are that realm's own.
async fn settings_realm(State(st): State<Arc<HeadState>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let realm = (req.uri().path().starts_with("/api/v1/settings/"))
        .then(|| req.uri().query().and_then(|q| q.split('&').find_map(|p| p.strip_prefix("realm=")).map(str::to_owned)))
        .flatten()
        .filter(|r| !r.is_empty());
    let Some(realm) = realm else { return next.run(req).await };
    // A realm's caller stays in its realm.
    if let Some(mine) = req.extensions().get::<Caller>().and_then(|c| c.realm.clone())
        && mine != realm
    {
        return ApiErr(StatusCode::FORBIDDEN, format!("this is limited to realm {mine}")).into_response();
    }
    match crate::realms::exists(&st, &realm).await {
        Ok(true) => crate::store::SETTINGS_REALM.scope(realm, next.run(req)).await,
        _ => ApiErr(StatusCode::BAD_REQUEST, format!("no realm {realm:?}")).into_response(),
    }
}

// --- Collections (docs/os.md §4.1) ---------------------------------------------

/// The realm a collection request is for: `?realm=` (you), the token's
/// realm, else home; it must exist.
async fn realm_for(st: &HeadState, caller: &Caller, asked: &api::InRealm) -> anyhow::Result<String> {
    let realm = caller.realm_for_new(asked.realm.as_deref())?;
    if !crate::realms::exists(st, &realm).await? {
        return Err(crate::bad_request(format!("no realm {realm:?}")));
    }
    Ok(realm)
}

/// A collection request that may reach another realm's collection through
/// a grant (`?realm=` from a realm-scoped token).
async fn reach(st: &HeadState, caller: &Caller, asked: &api::InRealm, name: &str, mode: &str) -> anyhow::Result<crate::grants::Reach> {
    match &caller.realm {
        Some(mine) => crate::grants::reach(st, mine, asked.realm.as_deref(), name, mode, &writer(caller)).await,
        None => Ok(crate::grants::Reach { realm: realm_for(st, caller, asked).await?, within: None, grant: None }),
    }
}

/// Who a write is by, for history.
fn writer(caller: &Caller) -> String {
    match &caller.realm {
        Some(r) => format!("token:{r}"),
        None => "you".into(),
    }
}

async fn list_collections(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::Collection>>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::collections::list(&st, &realm).await?))
}

async fn create_collection(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(c): Json<api::CollectionIn>,
) -> ApiResult<Json<api::Collection>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::collections::create(&st, &realm, &c, &writer(&caller), false).await?))
}

async fn get_collection(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<api::Collection>> {
    let to = reach(&st, &caller, &r, &name, "read").await?;
    Ok(Json(crate::collections::get(&st, &to.realm, &name).await?))
}

async fn update_collection(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(c): Json<api::CollectionIn>,
) -> ApiResult<Json<api::Collection>> {
    let realm = realm_for(&st, &caller, &r).await?;
    crate::collections::check_code(&crate::collections::get(&st, &realm, &name).await?)?;
    Ok(Json(crate::collections::update(&st, &realm, &name, &c).await?))
}

async fn drop_collection(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    crate::collections::check_code(&crate::collections::get(&st, &realm, &name).await?)?;
    crate::collections::drop(&st, &realm, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn query_records(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(mut q): Json<api::RecordQuery>,
) -> ApiResult<Json<api::RecordPage>> {
    let to = reach(&st, &caller, &r, &name, "read").await?;
    if let Some(w) = &to.within {
        crate::grants::narrow(&mut q, w);
    }
    Ok(Json(crate::collections::query(&st, &to.realm, &name, &q).await?))
}

async fn list_records(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<api::RecordPage>> {
    let to = reach(&st, &caller, &r, &name, "read").await?;
    let mut q = api::RecordQuery::default();
    if let Some(w) = &to.within {
        crate::grants::narrow(&mut q, w);
    }
    Ok(Json(crate::collections::query(&st, &to.realm, &name, &q).await?))
}

/// `?force=true`: you, writing an app's collection anyway; it asks you
/// first, as a decision (docs/apps.md §6).
#[derive(Default, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ForceQuery {
    #[serde(default)]
    realm: Option<String>,
    #[serde(default)]
    force: bool,
}

/// A forced write: only you, never a token; answered with the decision.
async fn forced(
    st: &HeadState,
    caller: &Caller,
    internal: bool,
    realm: &str,
    name: &str,
    id: Option<&str>,
    rec: Option<&api::RecordIn>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    if caller.realm.is_some() || internal {
        return Err(crate::forbidden("only you can force a write to an app's collection").into());
    }
    let d = crate::collections::ask_force(st, realm, name, id, rec).await?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"forced": "asked", "decision": d.id}))).into_response())
}

async fn create_record(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    internal: Option<Extension<crate::assistant::InternalCall>>,
    Path(name): Path<String>,
    Query(f): Query<ForceQuery>,
    Json(rec): Json<api::RecordIn>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let r = api::InRealm { realm: f.realm.clone() };
    if f.force {
        let realm = realm_for(&st, &caller, &r).await?;
        return forced(&st, &caller, internal.is_some(), &realm, &name, None, Some(&rec)).await;
    }
    let to = reach(&st, &caller, &r, &name, "append").await?;
    // Another realm's record for commons waits for your review.
    if to.realm == crate::realms::COMMONS && to.grant.is_some() {
        let d = crate::commons::propose_record(&st, caller.realm.as_deref().unwrap_or_default(), &writer(&caller), &name, &rec).await?;
        return Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"proposed": true, "decision": d.id}))).into_response());
    }
    let by = match &to.grant {
        Some(g) => format!("{} grant:{g}", writer(&caller)),
        None => writer(&caller),
    };
    Ok(Json(crate::collections::put(&st, &to.realm, &name, None, &rec, &by).await?).into_response())
}

async fn get_record(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((name, id)): Path<(String, String)>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<api::Record>> {
    let to = reach(&st, &caller, &r, &name, "read").await?;
    crate::grants::check_within(&st, &to, &name, &id).await?;
    Ok(Json(crate::collections::record(&st, &to.realm, &name, &id).await?))
}

async fn put_record(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    internal: Option<Extension<crate::assistant::InternalCall>>,
    Path((name, id)): Path<(String, String)>,
    Query(f): Query<ForceQuery>,
    Json(rec): Json<api::RecordIn>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let realm = realm_for(&st, &caller, &api::InRealm { realm: f.realm.clone() }).await?;
    if f.force {
        return forced(&st, &caller, internal.is_some(), &realm, &name, Some(&id), Some(&rec)).await;
    }
    Ok(Json(crate::collections::put(&st, &realm, &name, Some(&id), &rec, &writer(&caller)).await?).into_response())
}

async fn delete_record(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    internal: Option<Extension<crate::assistant::InternalCall>>,
    Path((name, id)): Path<(String, String)>,
    Query(f): Query<ForceQuery>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let realm = realm_for(&st, &caller, &api::InRealm { realm: f.realm.clone() }).await?;
    if f.force {
        return forced(&st, &caller, internal.is_some(), &realm, &name, Some(&id), None).await;
    }
    crate::collections::delete(&st, &realm, &name, &id, &writer(&caller)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn record_history(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((name, id)): Path<(String, String)>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::RecordChange>>> {
    let to = reach(&st, &caller, &r, &name, "read").await?;
    crate::grants::check_within(&st, &to, &name, &id).await?;
    Ok(Json(crate::collections::changes(&st, &to.realm, &name, &id).await?))
}

/// carbon6's prompts, with their defaults.
async fn list_prompts(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::PromptInfo>>> {
    Ok(Json(crate::prompts::list(&st).await?))
}

async fn put_prompt(State(st): State<Arc<HeadState>>, Path(name): Path<String>, Json(p): Json<api::PromptIn>) -> ApiResult<StatusCode> {
    crate::prompts::set(&st, &name, &p.body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_realm(State(st): State<Arc<HeadState>>, Json(r): Json<api::RealmIn>) -> ApiResult<Json<api::Realm>> {
    Ok(Json(crate::realms::create(&st, &r, "you").await?))
}

async fn update_realm(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(r): Json<api::RealmIn>,
) -> ApiResult<Json<api::Realm>> {
    Ok(Json(crate::realms::update(&st, &id, &r).await?))
}

async fn delete_realm(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    crate::realms::delete(&st, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- The realm explorer (docs/explorer.md) -----------------------------------

async fn explorer_map(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::explorer::map(&st).await?))
}

/// The realm asked for, if the caller may see it.
async fn explored(st: &HeadState, caller: &Caller, id: &str) -> anyhow::Result<()> {
    if !caller.sees(id) {
        return Err(crate::forbidden(format!("this token is limited to realm {}", caller.realm.as_deref().unwrap_or(""))));
    }
    if !crate::realms::exists(st, id).await? {
        return Err(crate::not_found("no such realm"));
    }
    Ok(())
}

async fn realm_summary(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<api::RealmSummary>> {
    explored(&st, &caller, &id).await?;
    Ok(Json(crate::explorer::summary(&st, &id).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ActivityQuery {
    /// ms; default a day ago.
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn realm_activity(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<ActivityQuery>,
) -> ApiResult<Json<Vec<api::ExRef>>> {
    explored(&st, &caller, &id).await?;
    let since = q.since.unwrap_or_else(|| c6_core::now_ms() - 86_400_000);
    Ok(Json(crate::explorer::activity(&st, &id, since, q.limit.unwrap_or(100).clamp(1, 500)).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ExListQuery {
    /// From the summary's entries: collection:<name>, series, things:<kind>, items:<kind>, facts:<status>, sessions, decisions:open, decisions:closed, triggers, list:<key>, memory, secrets, grants:in, grants:out, workers, connections.
    section: String,
    #[serde(default)]
    q: String,
}

async fn realm_list(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<ExListQuery>,
) -> ApiResult<Json<Vec<api::ExRef>>> {
    explored(&st, &caller, &id).await?;
    Ok(Json(crate::explorer::list(&st, &id, &q.section, &q.q).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ExObjectQuery {
    kind: String,
    id: String,
}

async fn realm_object(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<ExObjectQuery>,
) -> ApiResult<Json<api::ExObject>> {
    explored(&st, &caller, &id).await?;
    Ok(Json(crate::explorer::object(&st, &id, &q.kind, &q.id).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ExSearchQuery {
    q: String,
}

async fn realm_search(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<ExSearchQuery>,
) -> ApiResult<Json<Vec<api::ExGroup>>> {
    explored(&st, &caller, &id).await?;
    Ok(Json(crate::explorer::search(&st, &id, &q.q).await?))
}

async fn realm_neighborhood(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<ExObjectQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    explored(&st, &caller, &id).await?;
    Ok(Json(crate::explorer::neighborhood(&st, &id, &q.kind, &q.id).await?))
}

// --- Apps (docs/os.md §5) ----------------------------------------------------

async fn list_apps(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::AppInstall>>> {
    Ok(Json(crate::packages::apps(&st).await?))
}

/// `?realm=`: the realm to install it in (default: the one it suggests if
/// it exists, else home).
async fn plan_app(
    State(st): State<Arc<HeadState>>,
    Query(r): Query<api::InRealm>,
    Json(p): Json<serde_json::Value>,
) -> ApiResult<Json<api::AppPlan>> {
    Ok(Json(crate::packages::plan_manifest(&st, p, r.realm.as_deref().unwrap_or("")).await?))
}

async fn install_app(
    State(st): State<Arc<HeadState>>,
    Query(r): Query<api::InRealm>,
    Json(p): Json<serde_json::Value>,
) -> ApiResult<Json<api::AppInstall>> {
    Ok(Json(crate::packages::install_manifest(&st, p, r.realm.as_deref().unwrap_or("")).await?))
}

async fn upgrade_app(
    State(st): State<Arc<HeadState>>,
    Path(realm): Path<String>,
    Json(p): Json<serde_json::Value>,
) -> ApiResult<Json<api::AppInstall>> {
    Ok(Json(crate::packages::upgrade_manifest(&st, &realm, p).await?))
}

async fn remove_app(State(st): State<Arc<HeadState>>, Path(realm): Path<String>) -> ApiResult<Json<api::AppInstall>> {
    Ok(Json(crate::packages::remove_app(&st, &realm).await?))
}

// --- Packages (docs/packages.md) ---------------------------------------------

async fn list_packages(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::PackageInfo>>> {
    Ok(Json(crate::packages::list(&st).await?))
}

async fn add_package(State(st): State<Arc<HeadState>>, Json(p): Json<api::PackageIn>) -> ApiResult<Json<api::PackageInfo>> {
    Ok(Json(crate::packages::add_in(&st, p).await?))
}

async fn get_package(
    State(st): State<Arc<HeadState>>,
    Path((name, version)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (manifest, info) = crate::packages::get(&st, &name, &version).await?;
    Ok(Json(serde_json::json!({"manifest": manifest, "info": info})))
}

async fn package_file(
    State(st): State<Arc<HeadState>>,
    Path((name, version, path)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let (_, info) = crate::packages::get(&st, &name, &version).await?;
    let data = crate::packages::file(&st, &info.digest, &path).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
}

async fn delete_package(State(st): State<Arc<HeadState>>, Path((name, version)): Path<(String, String)>) -> ApiResult<StatusCode> {
    crate::packages::delete(&st, &name, &version).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_installs(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Install>>> {
    Ok(Json(crate::packages::installs(&st).await?))
}

async fn plan_install(State(st): State<Arc<HeadState>>, Json(i): Json<api::InstallIn>) -> ApiResult<Json<api::AppPlan>> {
    Ok(Json(crate::packages::plan(&st, &i).await?))
}

async fn add_install(State(st): State<Arc<HeadState>>, Json(i): Json<api::InstallIn>) -> ApiResult<Json<api::Install>> {
    Ok(Json(crate::packages::install(&st, &i).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct UpgradeIn {
    /// A version range (an exact older version downgrades); default: the
    /// newest.
    #[serde(default)]
    version: String,
    /// A fork of its package to switch to (docs/apps.md §5.6); default:
    /// its own package.
    #[serde(default)]
    package: String,
}

async fn upgrade_install(State(st): State<Arc<HeadState>>, Path(id): Path<i64>, Json(u): Json<UpgradeIn>) -> ApiResult<Json<api::Install>> {
    Ok(Json(crate::packages::upgrade_to(&st, id, &u.package, &u.version).await?))
}

/// A settings list as a realm's sessions use it (or, `?instance=`, as an
/// app instance's sessions do): each entry with where it comes from (root,
/// the realm, or `app:<name>`), docs/apps.md §5.3.
#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct EffectiveQuery {
    #[serde(default)]
    instance: String,
}

async fn effective_list(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((realm, key)): Path<(String, String)>,
    Query(q): Query<EffectiveQuery>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    if !caller.sees(&realm) || !crate::realms::exists(&st, &realm).await? {
        return Err(crate::not_found("no such realm").into());
    }
    if crate::lists::list(&key).is_none() {
        return Err(crate::bad_request(format!("{key} is not a settings list")).into());
    }
    let inst = Some(q.instance.as_str()).filter(|i| !i.is_empty());
    let out = crate::lists::effective_tagged(&st.store.pool, &key, &realm, inst)
        .await?
        .into_iter()
        .map(|(entry, layer)| {
            let from = match layer {
                crate::lists::Layer::Root => "root".to_owned(),
                crate::lists::Layer::Realm => "realm".to_owned(),
                crate::lists::Layer::Instance(n) => format!("app:{n}"),
            };
            serde_json::json!({"from": from, "entry": entry})
        })
        .collect();
    Ok(Json(out))
}

/// An app instance's operations (docs/apps.md §7).
async fn list_operations(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((realm, instance)): Path<(String, String)>,
) -> ApiResult<Json<Vec<api::Operation>>> {
    if !caller.sees(&realm) {
        return Err(crate::not_found("no such realm").into());
    }
    Ok(Json(crate::operations::list(&st, &realm, &instance).await?))
}

/// The accounts connected for an app's OAuth providers (docs/knowledge.md §6.2).
async fn list_accounts(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((realm, instance)): Path<(String, String)>,
) -> ApiResult<Json<Vec<api::OAuthAccount>>> {
    if caller.realm.is_some() {
        return Err(crate::forbidden("connecting accounts is yours").into());
    }
    Ok(Json(crate::providers::accounts(&st, &realm, &instance).await?))
}

/// Starts a sign-in to one of an app's providers: where to send you.
async fn connect_account(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    Path((realm, instance, provider)): Path<(String, String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    if caller.realm.is_some() {
        return Err(crate::forbidden("connecting accounts is yours").into());
    }
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()).map(str::to_owned).or_else(|| origin_of(&headers));
    Ok(Json(serde_json::json!({"authorize_url": crate::providers::connect(&st, &realm, &instance, &provider, origin.as_deref()).await?})))
}

async fn remove_account(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((realm, instance, id)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    if caller.realm.is_some() {
        return Err(crate::forbidden("connecting accounts is yours").into());
    }
    crate::providers::remove(&st, &realm, &instance, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Calls an operation: the app does the work, and who asked is kept with it.
async fn call_operation(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((realm, instance, name)): Path<(String, String, String)>,
    Json(c): Json<api::OperationCall>,
) -> ApiResult<Json<serde_json::Value>> {
    if !caller.sees(&realm) {
        return Err(crate::not_found("no such realm").into());
    }
    Ok(Json(crate::operations::call(&st, &realm, &instance, &name, &c.input, &writer(&caller)).await?))
}

/// The values of an instance's settings (docs/apps.md §5.4).
async fn install_settings(State(st): State<Arc<HeadState>>, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(crate::packages::install_by_id(&st, id).await?.settings))
}

async fn put_install_settings(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<i64>,
    Json(v): Json<serde_json::Value>,
) -> ApiResult<Json<api::Install>> {
    Ok(Json(crate::packages::set_settings(&st, id, &v).await?))
}

async fn remove_install(State(st): State<Arc<HeadState>>, Path(id): Path<i64>) -> ApiResult<Json<api::Install>> {
    Ok(Json(crate::packages::remove(&st, id).await?))
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ExportQuery {
    realm: String,
    /// An app instance in the realm: its package (default: the realm's own
    /// configuration).
    #[serde(default)]
    instance: String,
    /// With `instance`: as a new package of this name, forked from it.
    #[serde(default)]
    fork: String,
    /// Its records too.
    #[serde(default)]
    data: bool,
}

async fn export_app(State(st): State<Arc<HeadState>>, Query(q): Query<ExportQuery>) -> ApiResult<Json<api::AppPackage>> {
    if !q.instance.is_empty() {
        return Ok(Json(crate::apps::export_instance(&st, &q.realm, &q.instance, &q.fork, q.data).await?));
    }
    Ok(Json(crate::apps::export(&st, &q.realm, q.data).await?))
}

// --- Views (docs/os.md §4.5) -------------------------------------------------

async fn get_views_setting(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::View>>> {
    Ok(Json(crate::views::views(&st).await?))
}

async fn put_views(State(st): State<Arc<HeadState>>, Json(v): Json<Vec<api::View>>) -> ApiResult<StatusCode> {
    crate::views::check_views(&v)?;
    st.store.set_setting(crate::views::KEY, &serde_json::to_string(&v).map_err(anyhow::Error::from)?).await?;
    st.registry.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// The views a realm shows (root's and its own).
async fn list_views(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::View>>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::views::effective(&st, &realm).await?))
}

async fn view_items(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(q): Query<api::ViewItemsQuery>,
) -> ApiResult<Json<Vec<api::ViewItem>>> {
    let realm = realm_for(&st, &caller, &api::InRealm { realm: q.realm.clone() }).await?;
    Ok(Json(crate::views::items(&st, &realm, &name, &q).await?))
}

async fn view_item_thumb(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path((name, id)): Path<(String, String)>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let realm = realm_for(&st, &caller, &r).await?;
    let data = crate::views::thumb(&st, &realm, &name, &id).await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "image/jpeg"), (axum::http::header::CACHE_CONTROL, "private, max-age=3600")], data)
        .into_response())
}

async fn view_act(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(a): Json<api::ViewAct>,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::views::act(&st, &realm, &name, &a, &writer(&caller)).await?))
}

// --- Series (docs/os.md §4.4) ------------------------------------------------

async fn list_series(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<api::Series>>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::series::list(&st, &realm).await?))
}

async fn create_series(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(s): Json<api::SeriesIn>,
) -> ApiResult<Json<api::Series>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::series::create(&st, &realm, &s, &writer(&caller)).await?))
}

async fn get_series(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<api::Series>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::series::get(&st, &realm, &name).await?))
}

async fn update_series(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(s): Json<api::SeriesIn>,
) -> ApiResult<Json<api::Series>> {
    let realm = realm_for(&st, &caller, &r).await?;
    series_code(&crate::series::get(&st, &realm, &name).await?)?;
    Ok(Json(crate::series::update(&st, &realm, &name, &s).await?))
}

async fn delete_series(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<StatusCode> {
    let realm = realm_for(&st, &caller, &r).await?;
    series_code(&crate::series::get(&st, &realm, &name).await?)?;
    crate::series::delete(&st, &realm, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// An app's series changes with the app (docs/apps.md §5.5).
fn series_code(s: &api::Series) -> anyhow::Result<()> {
    if s.owner.is_empty() {
        return Ok(());
    }
    Err(crate::bad_request(format!("{} is the app {}'s: it changes when the app is upgraded", s.name, s.owner)))
}

async fn record_point(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(p): Json<api::Point>,
) -> ApiResult<Json<api::Point>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::series::record(&st, &realm, &name, &p, &writer(&caller)).await?))
}

async fn query_series(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(r): Query<api::InRealm>,
    Json(q): Json<api::SeriesQuery>,
) -> ApiResult<Json<api::SeriesResult>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::series::query(&st, &realm, &name, &q).await?))
}

/// `?realm=`: the grants given by or to one realm.
async fn list_grants(State(st): State<Arc<HeadState>>, Query(r): Query<api::InRealm>) -> ApiResult<Json<Vec<api::Grant>>> {
    Ok(Json(crate::grants::list(&st, r.realm.as_deref().filter(|r| !r.is_empty())).await?))
}

async fn create_grant(State(st): State<Arc<HeadState>>, Json(g): Json<api::GrantIn>) -> ApiResult<Json<api::Grant>> {
    Ok(Json(crate::grants::create(&st, &g, "you", "active").await?))
}

async fn get_grant(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::Grant>> {
    Ok(Json(crate::grants::get(&st, &id).await?.ok_or_else(|| crate::not_found("no such grant"))?))
}

async fn revoke_grant(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    crate::grants::revoke(&st, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn grant_uses(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<Vec<api::GrantUse>>> {
    Ok(Json(crate::grants::uses(&st, &id).await?))
}

async fn set_worker_realms(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(r): Json<api::RealmsIn>,
) -> ApiResult<StatusCode> {
    crate::realms::set_availability(&st, "workers", &id, &r.realms).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_connection_realms(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(r): Json<api::RealmsIn>,
) -> ApiResult<StatusCode> {
    crate::realms::set_availability(&st, "connections", &id, &r.realms).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The realms the caller may see.
async fn list_realms(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<Vec<api::Realm>>> {
    Ok(Json(crate::realms::list(&st, &caller).await?))
}

/// Upgrades of the head's database and, if one is connected, the memex's.
async fn get_upgrades(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Upgrade>>> {
    let as_api = |service: &str, r: c6_core::upgrade::Report| api::Upgrade {
        service: service.to_owned(),
        database: r.database.clone(),
        at: r.at,
        from_version: r.from_version,
        to_version: r.to_version,
        backup: r.backup.clone(),
        report: r.text(),
    };
    let mut out: Vec<api::Upgrade> = st.store.upgrades().await?.into_iter().map(|r| as_api("head", r)).collect();
    if let Ok(v) = crate::memex::call(&st, "memex.upgrades", serde_json::json!({})).await {
        let memex: Vec<c6_core::upgrade::Report> = serde_json::from_value(v).unwrap_or_default();
        out.extend(memex.into_iter().map(|r| as_api("memex", r)));
    }
    out.sort_by_key(|u| std::cmp::Reverse(u.at));
    Ok(Json(out))
}

/// The API as OpenAPI 3.1, for code generation.
async fn get_openapi(State(st): State<Arc<HeadState>>) -> Json<serde_json::Value> {
    Json(crate::catalog::openapi(&crate::notify::api_urls(&st).await))
}

/// Runs an operation (the CLI): destructive ones need `confirmed`.
async fn call_catalog(State(st): State<Arc<HeadState>>, Json(c): Json<api::CatalogCall>) -> ApiResult<Json<serde_json::Value>> {
    let op = crate::catalog::find(&c.op).ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, format!("no operation {:?}", c.op)))?;
    if op.effect == crate::catalog::Effect::Destructive && !c.confirmed {
        return Err(ApiErr(StatusCode::CONFLICT, format!("{} is destructive ({}); confirm to run it", op.name, op.summary)));
    }
    Ok(Json(crate::assistant::execute(&st, op, c.input, "you", None).await?))
}

/// Your answers to the assistant's question (a form).
async fn answer_question(
    State(st): State<Arc<HeadState>>,
    Path((id, call_id)): Path<(String, String)>,
    Json(a): Json<api::AnswerIn>,
) -> ApiResult<StatusCode> {
    if crate::sessions::answer_question(&st, &id, &call_id, a.values, a.cancelled).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiErr(StatusCode::CONFLICT, "the conversation is not running".into()))
    }
}

// --- Decisions (docs/os.md §4.2) ---------------------------------------------

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct DecisionQuery {
    /// open (default; includes snoozed ones now due), snoozed, answered, dismissed, expired or withdrawn.
    #[serde(default)]
    state: String,
}

async fn list_decisions(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<DecisionQuery>,
) -> ApiResult<Json<Vec<api::Decision>>> {
    Ok(Json(crate::decisions::list(&st, &q.state, &caller).await?))
}

async fn add_decision(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(d): Json<api::DecisionIn>,
) -> ApiResult<Json<api::Decision>> {
    let realm = realm_for(&st, &caller, &r).await?;
    Ok(Json(crate::decisions::open(&st, &d, &realm, "").await?))
}

async fn count_decisions(State(st): State<Arc<HeadState>>, Extension(caller): Extension<Caller>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({"open": crate::decisions::open_count(&st, &caller).await?})))
}

async fn get_decision(State(st): State<Arc<HeadState>>, Path(id): Path<i64>) -> ApiResult<Json<api::Decision>> {
    Ok(Json(crate::decisions::get(&st, id).await?.ok_or_else(|| ApiErr(StatusCode::NOT_FOUND, "no such decision".into()))?))
}

async fn edit_decision(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<i64>,
    Json(e): Json<api::DecisionEdit>,
) -> ApiResult<Json<api::Decision>> {
    Ok(Json(crate::decisions::edit(&st, id, &e).await?))
}

async fn answer_decision(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<i64>,
    Json(a): Json<api::DecisionAnswer>,
) -> ApiResult<Json<api::Decision>> {
    let via = if caller.realm.is_some() { "an API token" } else { "carbon6" };
    Ok(Json(crate::decisions::answer(&st, id, &a, via).await?))
}

/// Starts a session that helps you settle the decision.
async fn decision_session(State(st): State<Arc<HeadState>>, Path(id): Path<i64>) -> ApiResult<Json<api::Session>> {
    Ok(Json(crate::decisions::start_session(&st, id).await?))
}
