//! HTTP and WebSocket access to the head.

use std::time::Duration;

use c6_api::{
    Connection, CreateConnection, CreateEnrollment, CreateSession, Enrollment, LiveEvent, QueuedInput, SendMessage, Session, SessionDetail,
    StreamMsg, Worker,
};
use futures::StreamExt;
use futures::future::{AbortHandle, abortable};
use gloo_net::http::{Request, RequestBuilder, Response};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Fleet and session lists kept current over `WS /api/v1/live`.
#[derive(Clone, Copy)]
pub struct LiveFeed {
    pub workers: ReadSignal<Vec<Worker>>,
    pub sessions: ReadSignal<Vec<Session>>,
    pub connected: ReadSignal<bool>,
}

impl LiveFeed {
    pub fn connect() -> Self {
        let (workers, set_workers) = signal(Vec::new());
        let (sessions, set_sessions) = signal(Vec::new());
        let (connected, set_connected) = signal(false);
        spawn_local(async move {
            let mut backoff = 500;
            loop {
                if let Ok(ws) = WebSocket::open(&ws_url("/api/v1/live")) {
                    let (_tx, mut rx) = ws.split();
                    while let Some(Ok(msg)) = rx.next().await {
                        let Message::Text(text) = msg else { continue };
                        match serde_json::from_str::<LiveEvent>(&text) {
                            Ok(ev) => {
                                set_connected.set(true);
                                backoff = 500;
                                match ev {
                                    LiveEvent::Workers { workers } => set_workers.set(workers),
                                    LiveEvent::Sessions { sessions } => set_sessions.set(sessions),
                                }
                            }
                            Err(e) => leptos::logging::warn!("bad live event: {e}"),
                        }
                    }
                }
                set_connected.set(false);
                gloo_timers::future::sleep(Duration::from_millis(backoff)).await;
                backoff = (backoff * 2).min(10_000);
            }
        });
        Self { workers, sessions, connected }
    }

    pub fn worker_name(&self, id: &str) -> String {
        self.workers.with(|ws| ws.iter().find(|w| w.id.to_string() == id).map(|w| w.name.clone()).unwrap_or_else(|| "unknown".into()))
    }
}

pub fn live() -> LiveFeed {
    expect_context::<LiveFeed>()
}

/// Streams a session's events and live deltas, resuming from the last seen
/// seq after reconnects. Returns a handle that stops it.
pub fn stream_session(session_id: String, on_msg: impl Fn(StreamMsg) + 'static, last_seq: impl Fn() -> u64 + 'static) -> AbortHandle {
    let (fut, handle) = abortable(async move {
        let mut backoff = 500;
        loop {
            let url = ws_url(&format!("/api/v1/sessions/{session_id}/stream?after={}", last_seq()));
            if let Ok(ws) = WebSocket::open(&url) {
                let (_tx, mut rx) = ws.split();
                while let Some(Ok(msg)) = rx.next().await {
                    let Message::Text(text) = msg else { continue };
                    match serde_json::from_str::<StreamMsg>(&text) {
                        Ok(m) => {
                            backoff = 500;
                            on_msg(m);
                        }
                        Err(e) => leptos::logging::warn!("bad stream message: {e}"),
                    }
                }
            }
            gloo_timers::future::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(10_000);
        }
    });
    spawn_local(async move {
        let _ = fut.await;
    });
    handle
}

pub fn ws_url(path: &str) -> String {
    let loc = window().location();
    let scheme = if loc.protocol().as_deref() == Ok("https:") { "wss" } else { "ws" };
    format!("{scheme}://{}{path}", loc.host().unwrap_or_default())
}

async fn check(resp: Result<Response, gloo_net::Error>) -> Result<Response, String> {
    let resp = resp.map_err(|e| e.to_string())?;
    if resp.ok() {
        return Ok(resp);
    }
    let msg = resp.json::<c6_api::ApiError>().await.map(|e| e.error).unwrap_or_else(|_| resp.status_text());
    Err(msg)
}

async fn json<T: DeserializeOwned>(req: Result<Response, gloo_net::Error>) -> Result<T, String> {
    check(req).await?.json().await.map_err(|e| e.to_string())
}

async fn send_json<T: DeserializeOwned>(req: RequestBuilder, body: &impl Serialize) -> Result<T, String> {
    json(req.json(body).map_err(|e| e.to_string())?.send().await).await
}

async fn empty(req: Result<Response, gloo_net::Error>) -> Result<(), String> {
    check(req).await.map(drop)
}

pub async fn create_enrollment(name: Option<String>) -> Result<Enrollment, String> {
    send_json(Request::post("/api/v1/workers/enrollments"), &CreateEnrollment { name }).await
}

pub async fn revoke_worker(id: String) -> Result<(), String> {
    empty(Request::post(&format!("/api/v1/workers/{id}/revoke")).send().await).await
}

pub async fn delete_worker(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/workers/{id}")).send().await).await
}

pub async fn connections() -> Result<Vec<Connection>, String> {
    json(Request::get("/api/v1/connections").send().await).await
}

pub async fn create_connection(req: CreateConnection) -> Result<Connection, String> {
    send_json(Request::post("/api/v1/connections"), &req).await
}

pub async fn start_chatgpt(name: String) -> Result<c6_api::ChatGptSignIn, String> {
    send_json(Request::post("/api/v1/connections/chatgpt/start"), &c6_api::StartChatGptSignIn { name }).await
}

pub async fn complete_chatgpt(callback_url: String) -> Result<Connection, String> {
    send_json(Request::post("/api/v1/connections/chatgpt/complete"), &c6_api::CompleteChatGptSignIn { callback_url }).await
}

pub async fn delete_connection(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/connections/{id}")).send().await).await
}

pub async fn models(connection_id: String) -> Result<Vec<String>, String> {
    json(Request::get(&format!("/api/v1/connections/{connection_id}/models")).send().await).await
}

pub async fn create_session(req: CreateSession) -> Result<Session, String> {
    send_json(Request::post("/api/v1/sessions"), &req).await
}

pub async fn session(id: String) -> Result<SessionDetail, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}")).send().await).await
}

pub async fn send_message(id: String, text: String) -> Result<QueuedInput, String> {
    send_json(Request::post(&format!("/api/v1/sessions/{id}/messages")), &SendMessage { text }).await
}

pub async fn interrupt(id: String) -> Result<(), String> {
    empty(Request::post(&format!("/api/v1/sessions/{id}/interrupt")).send().await).await
}

pub async fn archive(id: String) -> Result<(), String> {
    #[derive(Serialize)]
    struct Body {
        archived: bool,
    }
    empty(Request::patch(&format!("/api/v1/sessions/{id}")).json(&Body { archived: true }).map_err(|e| e.to_string())?.send().await).await
}

pub async fn dismiss_rejected(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/sessions/{id}/queue/rejected")).send().await).await
}

// --- Worker files, git, terminals ---------------------------------------------

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

pub async fn fs_list(worker: String, path: String, hidden: bool) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/fs/list?path={}&hidden={hidden}", enc(&path))).send().await).await
}

pub async fn fs_read(worker: String, path: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/fs/read?path={}", enc(&path))).send().await).await
}

pub async fn git_status(worker: String, path: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/git/status?path={}", enc(&path))).send().await).await
}

pub async fn git_diff(worker: String, path: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/git/diff?path={}", enc(&path))).send().await).await
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TerminalInfo {
    pub id: String,
    pub cwd: String,
    pub exited: Option<i32>,
}

pub async fn terminals(worker: String) -> Result<Vec<TerminalInfo>, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/terminals")).send().await).await
}

pub async fn open_terminal(worker: String, cwd: String) -> Result<TerminalInfo, String> {
    send_json(Request::post(&format!("/api/v1/workers/{worker}/terminals")), &serde_json::json!({"cwd": cwd})).await
}

pub async fn close_terminal(worker: String, id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/workers/{worker}/terminals/{id}")).send().await).await
}

pub async fn children(id: String) -> Result<Vec<Session>, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}/children")).send().await).await
}

pub async fn get_setting(name: &'static str) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/settings/{name}")).send().await).await
}

pub async fn put_setting(name: &'static str, value: serde_json::Value) -> Result<(), String> {
    empty(Request::put(&format!("/api/v1/settings/{name}")).json(&value).map_err(|e| e.to_string())?.send().await).await
}
