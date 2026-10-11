//! HTTP API (`/api/v1`) and the embedded web UI.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use c6_api as api;
use c6_core::{WorkerId, now_ms};
use rust_embed::RustEmbed;

use crate::HeadState;

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
        .route("/auth/callback", get(chatgpt_callback))
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
        .route("/api/v1/templates/{name}/run", post(run_template))
        .route("/api/v1/memories", get(list_memories).post(create_memory))
        .route("/api/v1/memories/{id}", put(update_memory).delete(delete_memory))
        .route("/api/v1/automations", get(list_automations).post(create_automation))
        .route("/api/v1/automations/{id}", axum::routing::patch(update_automation).delete(delete_automation))
        .route("/api/v1/automations/{id}/run", post(run_automation))
        .route("/api/v1/automations/{id}/runs", get(automation_runs))
        .route("/api/v1/automations/{id}/rotate", post(rotate_automation))
        .route("/api/v1/webhooks/{id}/{token}", post(call_webhook))
        .route("/api/v1/settings/web-search", get(get_web_search).put(put_web_search))
        .route("/api/v1/settings/secrets/{name}", put(put_secret).delete(delete_secret))
        .route("/api/v1/settings/commands", get(get_commands_setting).put(put_commands))
        .route("/api/v1/settings/agents", get(get_agents).put(put_agents))
        .route("/api/v1/commands", get(list_commands))
        .route("/api/v1/sessions/{id}/messages", post(send_message))
        .route("/api/v1/sessions/{id}/interrupt", post(interrupt_session))
        .route("/api/v1/sessions/{id}/rewind", post(rewind_session))
        .route("/api/v1/sessions/{id}/attachments/{seq}/{index}", get(attachment))
        .route("/api/v1/sessions/{id}/export", get(export_session))
        .route("/api/v1/sessions/{id}/outputs/{seq}", get(output))
        .route("/api/v1/sessions/{id}/labels", put(set_session_labels))
        .route("/api/v1/sessions/{id}/tasks/{task}/stop", post(stop_task))
        .route("/api/v1/sessions/{id}/approvals/{call_id}", post(decide_approval))
        .route("/api/v1/settings/policies", get(get_policies).put(put_policies))
        .route("/api/v1/settings/routes", get(get_routes).put(put_routes))
        .route("/api/v1/settings/channels", get(get_channels).put(put_channels))
        .route("/api/v1/settings/prices", get(get_prices).put(put_prices))
        .route("/api/v1/settings/hosts", get(get_hosts_setting).put(put_hosts))
        .route("/api/v1/hosts", get(list_hosts))
        .route("/api/v1/hosts/{name}/check", post(check_host))
        .route("/api/v1/settings/budgets", get(get_budgets).put(put_budgets))
        .route("/api/v1/usage", get(get_usage))
        .route("/api/v1/settings/channels/test", post(test_channel))
        .route("/api/v1/settings/public_url", get(get_public_url).put(put_public_url))
        .route("/approve/{token}", get(crate::notify::approve_page).post(crate::notify::approve_answer))
        .route("/api/v1/sessions/{id}/config", get(session_config))
        .route("/api/v1/sessions/{id}/queue/rejected", delete(dismiss_rejected))
        .route("/api/v1/sessions/{id}/stream", get(session_stream))
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
        .merge(crate::auth::routes())
        .fallback(static_asset)
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
            })
        })
        .collect())
}

async fn list_workers(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Worker>>> {
    Ok(Json(workers_snapshot(&st).await?))
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

async fn live(State(st): State<Arc<HeadState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| live_socket(st, socket))
}

async fn live_socket(st: Arc<HeadState>, mut socket: WebSocket) {
    let mut changes = st.registry.subscribe();
    loop {
        changes.borrow_and_update();
        let Ok(workers) = workers_snapshot(&st).await else { return };
        let Ok(sessions) = sessions_snapshot(&st).await else { return };
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
    }
}

async fn list_connections(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Connection>>> {
    Ok(Json(st.store.connections().await?.into_iter().map(connection_view).collect()))
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
    };
    st.store.insert_connection(&row).await?;
    Ok(Json(connection_view(row)))
}

async fn chatgpt_start(State(st): State<Arc<HeadState>>, Json(req): Json<api::StartChatGptSignIn>) -> ApiResult<Json<api::ChatGptSignIn>> {
    let name = Some(req.name.trim()).filter(|n| !n.is_empty()).unwrap_or("ChatGPT").to_owned();
    // Reuse the client id issued to us by an earlier sign-in.
    let mut client_id = None;
    for c in st.store.connections().await?.into_iter().filter(|c| c.kind == "chatgpt") {
        if let Some(t) = c
            .api_key
            .as_deref()
            .and_then(|k| st.secrets.decrypt(k).ok())
            .and_then(|j| serde_json::from_str::<crate::oauth::ChatGptTokens>(&j).ok())
        {
            client_id = Some(t.client_id);
            break;
        }
    }
    Ok(Json(api::ChatGptSignIn { authorize_url: st.oauth.start(&name, st.ui_port, client_id) }))
}

async fn finish_chatgpt(st: &HeadState, callback: &str) -> anyhow::Result<api::Connection> {
    let done = st.oauth.complete(&st.http, callback).await?;
    let row = crate::store::ConnectionRow {
        id: uuid::Uuid::now_v7().to_string(),
        name: done.name,
        kind: "chatgpt".into(),
        base_url: crate::oauth::RESOURCE.into(),
        api_key: Some(st.secrets.encrypt(&serde_json::to_string(&done.tokens)?)?),
        default_model: None,
        created_at: now_ms(),
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

#[derive(serde::Deserialize)]
struct TestConnection {
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

pub async fn sessions_snapshot(st: &HeadState) -> anyhow::Result<Vec<api::Session>> {
    Ok(st.store.sessions(SESSION_LIST_LIMIT).await?.iter().map(crate::convert::session).collect())
}

async fn list_sessions(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Session>>> {
    Ok(Json(sessions_snapshot(&st).await?))
}

async fn create_session(State(st): State<Arc<HeadState>>, Json(req): Json<api::CreateSession>) -> ApiResult<Json<api::Session>> {
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

#[derive(serde::Deserialize)]
struct FindQuery {
    q: String,
}

async fn session_matches(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Query(f): Query<FindQuery>,
) -> ApiResult<Json<api::SessionMatches>> {
    Ok(Json(crate::search::session_matches(&st.store, &id, &f.q).await?))
}

async fn search(State(st): State<Arc<HeadState>>, Query(q): Query<api::SearchQuery>) -> ApiResult<Json<api::SearchResponse>> {
    Ok(Json(crate::search::search(&st.store, &q).await?))
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

#[derive(serde::Deserialize)]
struct UpdateSession {
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

#[derive(serde::Deserialize)]
struct After {
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

async fn session_children(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<Vec<api::Session>>> {
    Ok(Json(st.store.child_sessions(&id).await?.iter().map(crate::convert::session).collect()))
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
#[derive(serde::Deserialize)]
struct ForSession {
    #[serde(default)]
    session: Option<String>,
}

/// `?session=`: only the commands that apply to that session (by its labels),
/// the most specific variant of each name.
async fn list_commands(State(st): State<Arc<HeadState>>, Query(q): Query<ForSession>) -> ApiResult<Json<Vec<api::CommandInfo>>> {
    let all = crate::sessions::custom_commands(&st).await?;
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
            if !st.store.secrets().await?.iter().any(|x| x.name == s.secret) {
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

async fn get_templates(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Template>>> {
    Ok(Json(crate::templates::templates(&st).await?))
}

async fn put_templates(State(st): State<Arc<HeadState>>, Json(templates): Json<Vec<api::Template>>) -> ApiResult<StatusCode> {
    crate::templates::check(&templates)?;
    st.store.set_setting(crate::templates::TEMPLATES, &serde_json::to_string(&templates).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_memories(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Memory>>> {
    Ok(Json(crate::memory::list(&st).await?))
}

async fn create_memory(State(st): State<Arc<HeadState>>, Json(mut m): Json<api::Memory>) -> ApiResult<Json<api::Memory>> {
    m.id = String::new();
    Ok(Json(crate::memory::put(&st, m).await?))
}

async fn update_memory(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(mut m): Json<api::Memory>,
) -> ApiResult<Json<api::Memory>> {
    m.id = id;
    Ok(Json(crate::memory::put(&st, m).await?))
}

async fn delete_memory(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    crate::memory::delete(&st, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_automations(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::Automation>>> {
    Ok(Json(crate::automations::list(&st).await?))
}

async fn create_automation(State(st): State<Arc<HeadState>>, Json(e): Json<api::AutomationEdit>) -> ApiResult<Json<api::Automation>> {
    Ok(Json(crate::automations::create(&st, e).await?))
}

async fn update_automation(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(e): Json<api::AutomationEdit>,
) -> ApiResult<Json<api::Automation>> {
    Ok(Json(crate::automations::update(&st, &id, e).await?))
}

async fn delete_automation(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    crate::automations::delete(&st, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_automation(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::TemplateRun>> {
    Ok(Json(crate::automations::run_now(&st, &id).await?))
}

async fn rotate_automation(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> ApiResult<Json<api::Automation>> {
    Ok(Json(crate::automations::rotate(&st, &id).await?))
}

/// A webhook call from outside (no sign-in: the token in the URL is the
/// credential). 404 for a wrong token, so URLs cannot be probed.
async fn call_webhook(
    State(st): State<Arc<HeadState>>,
    Path((id, token)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<api::TemplateRun>)> {
    if body.len() > crate::automations::MAX_WEBHOOK_BODY {
        return Err(ApiErr(StatusCode::PAYLOAD_TOO_LARGE, "the body is too large".into()));
    }
    match crate::automations::webhook(&st, &id, &token, &body).await? {
        Some(run) => Ok((StatusCode::ACCEPTED, Json(run))),
        None => Err(ApiErr(StatusCode::NOT_FOUND, "not found".into())),
    }
}

async fn run_template(
    State(st): State<Arc<HeadState>>,
    Path(name): Path<String>,
    body: Option<Json<api::RunTemplate>>,
) -> ApiResult<Json<api::TemplateRun>> {
    let input = body.map(|Json(b)| b.input).unwrap_or_default();
    Ok(Json(crate::templates::run(&st, &name, &input, &[]).await?))
}

async fn list_secrets(State(st): State<Arc<HeadState>>) -> ApiResult<Json<Vec<api::SecretInfo>>> {
    Ok(Json(st.store.secrets().await?))
}

async fn put_secret(State(st): State<Arc<HeadState>>, Path(name): Path<String>, Json(req): Json<api::PutSecret>) -> ApiResult<StatusCode> {
    if !api::valid_secret_name(&name) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "a secret's name is an environment variable name: A-Z, 0-9 and _".into()));
    }
    let applies_to = crate::sessions::check_selector(&req.applies_to)?;
    let value = match req.value.as_deref().filter(|v| !v.is_empty()) {
        Some(v) => Some(st.secrets.encrypt(v)?),
        None => None,
    };
    if !st.store.put_secret(&name, value, req.description.trim(), &applies_to).await? {
        return Err(ApiErr(StatusCode::BAD_REQUEST, format!("{name} is new: give it a value")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_secret(State(st): State<Arc<HeadState>>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    if !st.store.delete_secret(&name).await? {
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
        _ => None,
    };
    let Some(a) = a else { return Err(ApiErr(StatusCode::NOT_FOUND, "no such attachment".into())) };
    // Text is served as plain text, never as something a browser would run.
    let mime = if a.mime.starts_with("image/") { a.mime } else { "text/plain; charset=utf-8".into() };
    Ok(([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, max-age=31536000, immutable".into())], a.data).into_response())
}

async fn set_session_labels(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::SetLabels>,
) -> ApiResult<Json<api::Session>> {
    Ok(Json(crate::sessions::set_labels(&st, &id, &req.labels).await?))
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

#[derive(serde::Deserialize)]
struct RunsQuery {
    #[serde(default)]
    limit: Option<u32>,
}

async fn automation_runs(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Vec<api::AutomationRun>>> {
    Ok(Json(crate::automations::runs(&st, &id, q.limit.unwrap_or(20)).await?))
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

#[derive(serde::Deserialize)]
struct UsageQuery {
    #[serde(default)]
    days: Option<u32>,
    #[serde(default)]
    labels: String,
}

async fn get_usage(State(st): State<Arc<HeadState>>, Query(q): Query<UsageQuery>) -> ApiResult<Json<api::UsageReport>> {
    let filter = api::labels::parse_labels(&q.labels);
    Ok(Json(crate::usage::report(&st, q.days.unwrap_or(30), &filter).await?))
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
    if !catch_up(&st, &session_id, &mut last, &mut socket).await {
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
