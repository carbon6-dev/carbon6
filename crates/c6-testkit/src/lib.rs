//! Test support: a scripted OpenAI-compatible server, and waiting on
//! conditions (docs/testing.md).

pub mod wait;
pub use wait::{eventually, have, need, port, within};

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
    /// Half of the text, then the provider's error in the stream (a
    /// response that breaks off).
    Broken(String),
}

struct Step {
    reply: Reply,
    /// If set, the first chunk is sent, then the response waits for this.
    gate: Option<Arc<Notify>>,
    /// If not empty, only for a request whose messages contain all of these.
    about: Vec<String>,
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
            .route("/v1/embeddings", post(embeddings))
            .route("/v1/audio/transcriptions", post(transcriptions))
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
        self.inner.script.lock().unwrap().push_back(Step { reply, gate: None, about: Vec::new() });
    }

    /// Queues a reply for the first request whose messages contain `about`,
    /// whenever it comes (for callers whose order isn't the test's).
    pub fn push_about(&self, about: &str, reply: Reply) {
        self.push_about_all(&[about], reply);
    }

    /// Queues a reply for the first request whose messages contain all of
    /// `about` (the kind of call, and what it's about).
    pub fn push_about_all(&self, about: &[&str], reply: Reply) {
        let about = about.iter().map(|a| a.to_string()).collect();
        self.inner.script.lock().unwrap().push_back(Step { reply, gate: None, about });
    }

    /// Queues a reply that stalls after its first chunk until the returned
    /// handle is notified.
    pub fn push_gated(&self, reply: Reply) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.inner.script.lock().unwrap().push_back(Step { reply, gate: Some(gate.clone()), about: Vec::new() });
        gate
    }

    pub fn requests(&self) -> Vec<Value> {
        self.inner.requests.lock().unwrap().clone()
    }
}

/// A transcript (whatever the audio), with timestamps.
async fn transcriptions(body: axum::body::Bytes) -> Json<Value> {
    let multipart = String::from_utf8_lossy(&body).contains("name=\"file\"");
    Json(
        json!({"text": "hello from paris", "segments": [{"start": 0.0, "text": " hello"}, {"start": 61.5, "text": " from paris"}], "multipart": multipart}),
    )
}

/// Embeddings by hashed words (64 dimensions): texts sharing words are
/// near each other, which is enough to test search by meaning. Words are
/// cut to 5 letters, so "deployment" and "deploying" count as one.
async fn embeddings(Json(body): Json<Value>) -> Json<Value> {
    let inputs: Vec<String> = match &body["input"] {
        Value::Array(a) => a.iter().map(|v| v.as_str().unwrap_or("").to_owned()).collect(),
        v => vec![v.as_str().unwrap_or("").to_owned()],
    };
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut v = [0f32; 64];
            for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2) {
                let stem: String = w.chars().take(5).collect();
                let h = stem.bytes().fold(5381u64, |h, b| h.wrapping_mul(33) ^ b as u64);
                v[(h % 64) as usize] += 1.0;
            }
            json!({"index": i, "embedding": v.to_vec()})
        })
        .collect();
    Json(json!({"data": data, "model": body["model"]}))
}

async fn completions(State(inner): State<Arc<Inner>>, Json(body): Json<Value>) -> Response {
    let messages = body["messages"].to_string();
    inner.requests.lock().unwrap().push(body);
    let step = {
        let mut script = inner.script.lock().unwrap();
        let at = script.iter().position(|s| s.about.iter().all(|a| messages.contains(a.as_str())));
        at.and_then(|i| script.remove(i))
    };
    let Some(step) = step else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "mock script exhausted").into_response();
    };
    let chunks = match step.reply {
        Reply::Status(code) => return axum::http::StatusCode::from_u16(code).unwrap().into_response(),
        Reply::Text(text) => {
            let mid = text.char_indices().nth(text.chars().count() / 2).map(|(i, _)| i).unwrap_or(0);
            vec![delta(json!({"role": "assistant", "content": &text[..mid]})), delta(json!({"content": &text[mid..]})), finish("stop")]
        }
        Reply::Broken(text) => {
            let mid = text.char_indices().nth(text.chars().count() / 2).map(|(i, _)| i).unwrap_or(0);
            vec![delta(json!({"role": "assistant", "content": &text[..mid]})), json!({"error": {"message": "overloaded"}})]
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
