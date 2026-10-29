//! Model calls as they happen, for the activity panel (docs/activity.md):
//! kept in this process's memory, the last few hundred, never written
//! anywhere. A call is tracked by wrapping its client (`track`); the panel
//! reads `calls()`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Serialize;

use crate::{Block, EventStream, LlmError, ModelClient, ModelEvent, ModelRequest};

/// How many calls are kept (the oldest finished ones go first).
const KEEP: usize = 200;
/// How much of a prompt, thinking and text is kept, each with its own cap.
const PROMPT_CHARS: usize = 20_000;
const THINKING_CHARS: usize = 16_000;
const OUTPUT_CHARS: usize = 8_000;

/// What a call is for.
#[derive(Clone, Debug, Default)]
pub struct Origin {
    /// e.g. sorting, draft, understanding, vision, assistant, suggestion.
    pub what: String,
    /// What it's about: a conversation, a session, an item.
    pub about: String,
    /// The connection's id.
    pub connection: String,
    pub realm: String,
    /// The pool it went through, if any (`connection` is its member).
    pub via: String,
}

/// A model call, ongoing or recent.
#[derive(Clone, Debug, Serialize)]
pub struct Call {
    pub id: u64,
    pub what: String,
    pub about: String,
    pub connection: String,
    /// The pool it went through (its id), if any.
    pub via: String,
    pub realm: String,
    pub model: String,
    pub effort: String,
    pub started_at: i64,
    /// When the first token came (thinking or text).
    pub first_at: Option<i64>,
    pub ended_at: Option<i64>,
    /// waiting (no answer yet), thinking, answering, done, failed or
    /// stopped (its caller gave up on it).
    pub state: &'static str,
    pub error: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub prompt: String,
    pub thinking: String,
    pub text: String,
}

static CALLS: Mutex<VecDeque<Call>> = Mutex::new(VecDeque::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The calls kept, newest first.
pub fn calls() -> Vec<Call> {
    CALLS.lock().unwrap().iter().rev().cloned().collect()
}

fn update(id: u64, f: impl FnOnce(&mut Call)) {
    if let Some(c) = CALLS.lock().unwrap().iter_mut().rev().find(|c| c.id == id) {
        f(c);
    }
}

fn push_capped(s: &mut String, more: &str, cap: usize) {
    if s.len() < cap {
        let room = cap - s.len();
        let cut = more.char_indices().map(|(i, _)| i).take_while(|i| *i <= room).last().unwrap_or(0);
        s.push_str(if more.len() <= room { more } else { &more[..cut] });
    }
}

/// The prompt as text: the system prompt and each message's text, its
/// start and end when long.
fn prompt_of(req: &ModelRequest) -> String {
    let mut parts = vec![format!("[system]\n{}", req.system)];
    for m in &req.messages {
        let text: Vec<&str> = m.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect();
        let images = m.blocks.iter().filter(|b| matches!(b, Block::Image { .. })).count();
        let role = if m.role == crate::Role::User { "user" } else { "assistant" };
        let mut part = format!("[{role}]\n{}", text.join("\n"));
        if images > 0 {
            part.push_str(&format!("\n({images} image{})", if images == 1 { "" } else { "s" }));
        }
        parts.push(part);
    }
    let all = parts.join("\n\n");
    if all.len() <= PROMPT_CHARS {
        return all;
    }
    let head: String = all.chars().take(PROMPT_CHARS / 5).collect();
    let tail: String = all.chars().rev().take(PROMPT_CHARS * 4 / 5).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}\n\n…\n\n{tail}")
}

fn start(origin: &Origin, req: &ModelRequest) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let call = Call {
        id,
        what: origin.what.clone(),
        about: origin.about.clone(),
        connection: origin.connection.clone(),
        via: origin.via.clone(),
        realm: origin.realm.clone(),
        model: req.model.clone(),
        effort: req.effort.clone().unwrap_or_default(),
        started_at: now_ms(),
        first_at: None,
        ended_at: None,
        state: "waiting",
        error: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        prompt: prompt_of(req),
        thinking: String::new(),
        text: String::new(),
    };
    let mut calls = CALLS.lock().unwrap();
    calls.push_back(call);
    // Over the limit: the oldest finished go; ongoing ones stay.
    while calls.len() > KEEP {
        match calls.iter().position(|c| c.ended_at.is_some()) {
            Some(i) => {
                calls.remove(i);
            }
            None => break,
        }
    }
    id
}

fn end(id: u64, state: &'static str, error: String) {
    update(id, |c| {
        if c.ended_at.is_none() {
            c.ended_at = Some(now_ms());
            c.state = state;
            c.error = error;
        }
    });
}

/// Ends the call as stopped if its stream is dropped before it ends.
struct Guard(u64);

impl Drop for Guard {
    fn drop(&mut self) {
        end(self.0, "stopped", String::new());
    }
}

struct Tracked {
    inner: Box<dyn ModelClient>,
    origin: Origin,
}

#[async_trait]
impl ModelClient for Tracked {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let id = start(&self.origin, req);
        let guard = Guard(id);
        let stream = match self.inner.stream(req).await {
            Ok(s) => s,
            Err(e) => {
                end(id, "failed", e.to_string());
                return Err(e);
            }
        };
        Ok(stream
            .map(move |ev| {
                let _ = &guard;
                match &ev {
                    Ok(ModelEvent::ThinkingDelta(t)) => update(id, |c| {
                        c.first_at.get_or_insert_with(now_ms);
                        c.state = "thinking";
                        push_capped(&mut c.thinking, t, THINKING_CHARS);
                    }),
                    Ok(ModelEvent::TextDelta(t)) => update(id, |c| {
                        c.first_at.get_or_insert_with(now_ms);
                        c.state = "answering";
                        push_capped(&mut c.text, t, OUTPUT_CHARS);
                    }),
                    Ok(ModelEvent::ToolCallStart { name, .. }) => update(id, |c| {
                        c.first_at.get_or_insert_with(now_ms);
                        c.state = "answering";
                        push_capped(&mut c.text, &format!("\n[tool call: {name}]"), OUTPUT_CHARS);
                    }),
                    Ok(ModelEvent::Block { block: Block::Text(t), .. }) => update(id, |c| {
                        // Providers that send whole blocks may send no deltas.
                        if c.text.is_empty() {
                            c.first_at.get_or_insert_with(now_ms);
                            push_capped(&mut c.text, t, OUTPUT_CHARS);
                        }
                    }),
                    Ok(ModelEvent::Usage(u)) => update(id, |c| {
                        c.input_tokens = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
                        c.output_tokens = u.output_tokens;
                    }),
                    Ok(ModelEvent::Done { .. }) => end(id, "done", String::new()),
                    Err(e) => end(id, "failed", e.to_string()),
                    _ => {}
                }
                ev
            })
            .boxed())
    }
}

/// The client, with each call it makes shown in the activity panel.
pub fn track(client: Box<dyn ModelClient>, origin: Origin) -> Box<dyn ModelClient> {
    Box::new(Tracked { inner: client, origin })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Usage};

    struct Canned(Vec<ModelEvent>);

    #[async_trait]
    impl ModelClient for Canned {
        async fn stream(&self, _req: &ModelRequest) -> Result<EventStream, LlmError> {
            Ok(futures::stream::iter(self.0.clone().into_iter().map(Ok)).boxed())
        }
    }

    fn req(model: &str) -> ModelRequest {
        ModelRequest {
            model: model.into(),
            system: "Sort it.".into(),
            messages: vec![Message::user_text("Can you approve the redlines?")],
            tools: vec![],
            max_output_tokens: None,
            cache_key: None,
            effort: Some("none".into()),
            web_search: false,
        }
    }

    #[tokio::test]
    async fn a_call_is_kept_as_it_goes() {
        let events = vec![
            ModelEvent::ThinkingDelta("hmm".into()),
            ModelEvent::TextDelta("{\"category\": ".into()),
            ModelEvent::TextDelta("\"needs_you\"}".into()),
            ModelEvent::Usage(Usage { input_tokens: 40, output_tokens: 9, ..Default::default() }),
            ModelEvent::Done { stop_reason: "stop".into() },
        ];
        let origin = Origin { what: "sorting".into(), about: "thread:a:t-kept".into(), ..Default::default() };
        let client = track(Box::new(Canned(events)), origin);
        let mut s = client.stream(&req("qwen-test-kept")).await.unwrap();
        let first = s.next().await.unwrap().unwrap();
        assert_eq!(first, ModelEvent::ThinkingDelta("hmm".into()));
        let mine = || calls().into_iter().find(|c| c.about == "thread:a:t-kept").unwrap();
        assert_eq!((mine().state, mine().thinking.as_str()), ("thinking", "hmm"));
        assert!(mine().prompt.contains("approve the redlines"), "{}", mine().prompt);
        while s.next().await.is_some() {}
        let c = mine();
        assert_eq!((c.state, c.text.as_str(), c.output_tokens, c.effort.as_str()), ("done", "{\"category\": \"needs_you\"}", 9, "none"));
        assert!(c.ended_at.is_some() && c.first_at.is_some());
    }

    #[tokio::test]
    async fn a_call_dropped_before_its_first_response_is_stopped() {
        struct Waiting;
        #[async_trait]
        impl ModelClient for Waiting {
            async fn stream(&self, _req: &ModelRequest) -> Result<EventStream, LlmError> {
                futures::future::pending().await
            }
        }
        let origin = Origin { what: "draft".into(), about: "request-before-response".into(), ..Default::default() };
        let client = track(Box::new(Waiting), origin);
        let request = req("m");
        let mut call = Box::pin(client.stream(&request));
        assert!(futures::poll!(&mut call).is_pending());
        let mine = || calls().into_iter().find(|c| c.about == "request-before-response").unwrap();
        assert_eq!(mine().state, "waiting");
        drop(call);
        let c = mine();
        assert_eq!(c.state, "stopped");
        assert!(c.ended_at.is_some() && c.first_at.is_none());
    }

    #[tokio::test]
    async fn thinking_and_text_have_separate_caps() {
        let events = vec![
            ModelEvent::ThinkingDelta("t".repeat(11_000)),
            ModelEvent::TextDelta("a".repeat(9_000)),
            ModelEvent::ThinkingDelta("t".repeat(6_000)),
            ModelEvent::Done { stop_reason: "stop".into() },
        ];
        let origin = Origin { what: "draft".into(), about: "request-caps".into(), ..Default::default() };
        let client = track(Box::new(Canned(events)), origin);
        let mut s = client.stream(&req("m")).await.unwrap();
        s.next().await.unwrap().unwrap();
        let mine = || calls().into_iter().find(|c| c.about == "request-caps").unwrap();
        assert_eq!(mine().thinking, "t".repeat(11_000));
        while let Some(ev) = s.next().await {
            ev.unwrap();
        }
        let c = mine();
        assert_eq!(c.thinking, "t".repeat(16_000));
        assert_eq!(c.text, "a".repeat(8_000));
    }

    #[tokio::test]
    async fn a_dropped_call_is_stopped() {
        let origin = Origin { what: "draft".into(), about: "request-dropped".into(), ..Default::default() };
        let client = track(Box::new(Canned(vec![ModelEvent::TextDelta("Hi".into())])), origin);
        let mut s = client.stream(&req("m")).await.unwrap();
        s.next().await;
        drop(s);
        let c = calls().into_iter().find(|c| c.about == "request-dropped").unwrap();
        assert_eq!(c.state, "stopped");
    }
}
