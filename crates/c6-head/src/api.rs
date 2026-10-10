//! HTTP API (`/api/v1`) and the embedded web UI.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
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
        .route("/api/v1/sessions/{id}/messages", post(send_message))
        .route("/api/v1/sessions/{id}/interrupt", post(interrupt_session))
        .route("/api/v1/sessions/{id}/rewind", post(rewind_session))
        .route("/api/v1/sessions/{id}/queue/rejected", delete(dismiss_rejected))
        .route("/api/v1/sessions/{id}/stream", get(session_stream))
        .route("/api/v1/workers/{id}/fs/list", get(fs_list))
        .route("/api/v1/workers/{id}/fs/read", get(fs_read))
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

#[derive(serde::Deserialize, serde::Serialize)]
struct SkillDef {
    name: String,
    #[serde(default)]
    description: String,
    body: String,
}

async fn get_skills(State(st): State<Arc<HeadState>>) -> ApiResult<Json<serde_json::Value>> {
    get_setting(&st, crate::sessions::SKILLS).await
}

async fn put_skills(State(st): State<Arc<HeadState>>, Json(skills): Json<Vec<SkillDef>>) -> ApiResult<StatusCode> {
    if skills.iter().any(|s| s.name.trim().is_empty() || s.body.trim().is_empty()) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "each skill needs a name and a body".into()));
    }
    st.store.set_setting(crate::sessions::SKILLS, &serde_json::to_string(&skills).map_err(anyhow::Error::from)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn send_message(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::SendMessage>,
) -> ApiResult<Json<api::QueuedInput>> {
    Ok(Json(crate::sessions::send_input(&st, &id, &req).await?))
}

async fn rewind_session(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::RewindRequest>,
) -> ApiResult<StatusCode> {
    crate::sessions::rewind(&st, &id, &req).await?;
    Ok(StatusCode::ACCEPTED)
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
