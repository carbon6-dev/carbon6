//! Test support: a scripted OpenAI-compatible server.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub enum Reply {
    Text(String),
    /// (name, arguments JSON) per call.
    ToolCalls(Vec<(String, String)>),
    Status(u16),
}

struct Step {
    reply: Reply,
    /// If set, the first chunk is sent, then the response waits for this.
    gate: Option<Arc<Notify>>,
}

#[derive(Default)]
struct Inner {
    script: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Value>>,
}

#[derive(Clone)]
pub struct MockLlm {
    pub addr: SocketAddr,
    inner: Arc<Inner>,
}

impl MockLlm {
    pub async fn start() -> Self {
        let inner = Arc::new(Inner::default());
        let app = Router::new()
            .route("/v1/chat/completions", post(completions))
            .route("/v1/models", get(|| async { Json(json!({"data": [{"id": "mock-1"}, {"id": "mock-2"}]})) }))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, inner }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn push(&self, reply: Reply) {
        self.inner.script.lock().unwrap().push_back(Step { reply, gate: None });
    }

    /// Queues a reply that stalls after its first chunk until the returned
    /// handle is notified.
    pub fn push_gated(&self, reply: Reply) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.inner.script.lock().unwrap().push_back(Step { reply, gate: Some(gate.clone()) });
        gate
    }

    pub fn requests(&self) -> Vec<Value> {
        self.inner.requests.lock().unwrap().clone()
    }
}

async fn completions(State(inner): State<Arc<Inner>>, Json(body): Json<Value>) -> Response {
    inner.requests.lock().unwrap().push(body);
    let step = inner.script.lock().unwrap().pop_front();
    let Some(step) = step else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "mock script exhausted").into_response();
    };
    let chunks = match step.reply {
        Reply::Status(code) => return axum::http::StatusCode::from_u16(code).unwrap().into_response(),
        Reply::Text(text) => {
            let mid = text.char_indices().nth(text.chars().count() / 2).map(|(i, _)| i).unwrap_or(0);
            vec![delta(json!({"role": "assistant", "content": &text[..mid]})), delta(json!({"content": &text[mid..]})), finish("stop")]
        }
        Reply::ToolCalls(calls) => {
            let mut v = vec![delta(json!({"role": "assistant", "content": null}))];
            for (i, (name, args)) in calls.iter().enumerate() {
                v.push(delta(json!({"tool_calls": [{"index": i, "id": format!("call_{i}_{name}"), "type": "function", "function": {"name": name, "arguments": ""}}]})));
                v.push(delta(json!({"tool_calls": [{"index": i, "function": {"arguments": args}}]})));
            }
            v.push(finish("tool_calls"));
            v
        }
    };
    let usage =
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10, "prompt_tokens_details": {"cached_tokens": 80}}});
    let gate = step.gate;
    let stream = async_stream(chunks, usage, gate);
    Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(stream)).unwrap()
}

fn delta(d: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": d, "finish_reason": null}]})
}

fn finish(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn async_stream(chunks: Vec<Value>, usage: Value, gate: Option<Arc<Notify>>) -> impl futures::Stream<Item = Result<Bytes, Infallible>> {
    let mut frames: Vec<String> = chunks.iter().chain(std::iter::once(&usage)).map(|c| format!("data: {c}\n\n")).collect();
    frames.push("data: [DONE]\n\n".into());
    futures::stream::unfold((frames.into_iter().enumerate(), gate), |(mut it, gate)| async move {
        let (i, frame) = it.next()?;
        if i == 1
            && let Some(g) = &gate
        {
            g.notified().await;
        } else if i > 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Some((Ok(Bytes::from(frame)), (it, gate)))
    })
}
