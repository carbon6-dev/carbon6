//! HTTP and WebSocket access to the head.

use std::time::Duration;

use c6_api::{CreateEnrollment, Enrollment, LiveEvent, Worker};
use futures::StreamExt;
use gloo_net::http::{Request, Response};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use leptos::prelude::*;
use leptos::task::spawn_local;

/// Fleet state kept current over `WS /api/v1/live`, reconnecting forever.
#[derive(Clone, Copy)]
pub struct LiveFeed {
    pub workers: ReadSignal<Vec<Worker>>,
    pub connected: ReadSignal<bool>,
}

impl LiveFeed {
    pub fn connect() -> Self {
        let (workers, set_workers) = signal(Vec::new());
        let (connected, set_connected) = signal(false);
        spawn_local(async move {
            let mut backoff = 500;
            loop {
                if let Ok(ws) = WebSocket::open(&ws_url("/api/v1/live")) {
                    let (_tx, mut rx) = ws.split();
                    while let Some(Ok(msg)) = rx.next().await {
                        let Message::Text(text) = msg else { continue };
                        match serde_json::from_str::<LiveEvent>(&text) {
                            Ok(LiveEvent::Workers { workers }) => {
                                set_connected.set(true);
                                backoff = 500;
                                set_workers.set(workers);
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
        Self { workers, connected }
    }
}

fn ws_url(path: &str) -> String {
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

pub async fn create_enrollment(name: Option<String>) -> Result<Enrollment, String> {
    let req = Request::post("/api/v1/workers/enrollments").json(&CreateEnrollment { name }).map_err(|e| e.to_string())?;
    check(req.send().await).await?.json().await.map_err(|e| e.to_string())
}

pub async fn revoke_worker(id: String) -> Result<(), String> {
    check(Request::post(&format!("/api/v1/workers/{id}/revoke")).send().await).await.map(drop)
}

pub async fn delete_worker(id: String) -> Result<(), String> {
    check(Request::delete(&format!("/api/v1/workers/{id}")).send().await).await.map(drop)
}
