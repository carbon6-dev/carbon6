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
        .route("/api/v1/connections/{id}", delete(delete_connection))
        .route("/api/v1/connections/{id}/models", get(connection_models))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route("/api/v1/sessions/{id}", get(get_session).patch(update_session))
        .route("/api/v1/sessions/{id}/events", get(session_events))
        .route("/api/v1/sessions/{id}/messages", post(send_message))
        .route("/api/v1/sessions/{id}/interrupt", post(interrupt_session))
        .route("/api/v1/sessions/{id}/queue/rejected", delete(dismiss_rejected))
        .route("/api/v1/sessions/{id}/stream", get(session_stream))
        .route("/api/v1/live", get(live))
        .fallback(static_asset)
        .with_state(state)
}

pub struct ApiErr(StatusCode, String);

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
                rtt_us: live.and_then(|l| l.rtt_us),
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
    let name = req.name.trim();
    let base_url = req.base_url.trim().trim_end_matches('/');
    if name.is_empty() {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "name is required".into()));
    }
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "base URL must start with http:// or https://".into()));
    }
    let api_key = req.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(|k| st.secrets.encrypt(k)).transpose()?;
    let row = crate::store::ConnectionRow {
        id: uuid::Uuid::now_v7().to_string(),
        name: name.to_owned(),
        kind: "openai_compat".into(),
        base_url: base_url.to_owned(),
        api_key,
        default_model: req.default_model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()),
        created_at: now_ms(),
    };
    st.store.insert_connection(&row).await?;
    Ok(Json(connection_view(row)))
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
    let key = c.api_key.as_deref().map(|k| st.secrets.decrypt(k)).transpose()?.unwrap_or_default();
    c6_llm::openai_chat::list_models(&c.base_url, &key).await.map(Json).map_err(|e| ApiErr(StatusCode::BAD_GATEWAY, e.to_string()))
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

async fn send_message(
    State(st): State<Arc<HeadState>>,
    Path(id): Path<String>,
    Json(req): Json<api::SendMessage>,
) -> ApiResult<Json<api::QueuedInput>> {
    Ok(Json(crate::sessions::send_input(&st, &id, &req.text).await?))
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
    // Trunk fingerprints asset names; only index.html must be revalidated.
    let cache = if path == "index.html" { "no-cache" } else { "public, max-age=31536000, immutable" };
    ([(header::CONTENT_TYPE, mime.as_ref().to_owned()), (header::CACHE_CONTROL, cache.to_owned())], file.data).into_response()
}
