//! OpenAI-compatible Chat Completions (`POST {base}/chat/completions`).
//! Works with OpenAI and the many servers that mimic it (vLLM, llama.cpp,
//! Ollama, LM Studio, OpenRouter, ...).

use std::collections::{HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::http::{self, truncate};
use crate::sse::SseDecoder;
use crate::{Block, EventStream, LlmError, ModelClient, ModelEvent, ModelRequest, Role, Usage};

#[derive(Clone)]
pub struct OpenAiChat {
    http: reqwest::Client,
    base_url: String,
    bearer: String,
    headers: HashMap<String, String>,
}

impl OpenAiChat {
    pub fn new(base_url: &str, bearer: &str, headers: HashMap<String, String>) -> Result<Self, LlmError> {
        let http = http::client()?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_owned(), bearer: bearer.to_owned(), headers })
    }

    fn is_openai(&self) -> bool {
        url::Url::parse(&self.base_url).ok().and_then(|u| u.host_str().map(|h| h == "api.openai.com")).unwrap_or(false)
    }

    pub fn request_body(&self, req: &ModelRequest) -> Value {
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        for m in &req.messages {
            match m.role {
                Role::User => {
                    let mut text = String::new();
                    for b in &m.blocks {
                        match b {
                            Block::ToolResult { id, content, .. } => {
                                messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                            }
                            Block::Text(t) => text.push_str(t),
                            _ => {}
                        }
                    }
                    if !text.is_empty() {
                        messages.push(json!({"role": "user", "content": text}));
                    }
                }
                Role::Assistant => {
                    let text: String =
                        m.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect();
                    let calls: Vec<Value> = m
                        .tool_uses()
                        .map(|(id, name, args)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}}))
                        .collect();
                    // `null` content is only valid alongside tool calls; a
                    // thinking-only turn must send "" (Ollama rejects null).
                    let content = if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) };
                    let mut msg = json!({"role": "assistant", "content": content});
                    if !calls.is_empty() {
                        msg["tool_calls"] = Value::Array(calls);
                    }
                    messages.push(msg);
                }
            }
        }

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(
                    |t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}),
                )
                .collect();
        }
        if let Some(n) = req.max_output_tokens {
            body["max_tokens"] = n.into();
        }
        // Only OpenAI is known to accept this; strict servers reject unknown fields.
        if self.is_openai() {
            if let Some(key) = &req.cache_key {
                body["prompt_cache_key"] = key.clone().into();
            }
            if let Some(e) = &req.effort {
                body["reasoning_effort"] = e.clone().into();
            }
        }
        body
    }
}

#[async_trait]
impl ModelClient for OpenAiChat {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let body = self.request_body(req);
        let mut headers = self.headers.clone();
        if !self.bearer.is_empty() {
            headers.insert("authorization".into(), format!("Bearer {}", self.bearer));
        }
        let resp = http::post_json(&self.http, &format!("{}/chat/completions", self.base_url), &headers, &body).await?;

        let state = StreamState::new(resp.bytes_stream().boxed());
        Ok(futures::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(ev) = st.pending.pop_front() {
                    return Some((ev, st));
                }
                if st.finished {
                    return None;
                }
                match st.bytes.next().await {
                    Some(Ok(chunk)) => {
                        for data in st.decoder.push(&chunk) {
                            st.handle(&data);
                        }
                    }
                    Some(Err(e)) => {
                        st.pending.push_back(Err(LlmError::Network(e.to_string())));
                        st.finished = true;
                    }
                    None => {
                        for data in st.decoder.finish() {
                            st.handle(&data);
                        }
                        st.end();
                    }
                }
            }
        })
        .boxed())
    }
}

struct StreamState {
    bytes: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    decoder: SseDecoder,
    pending: VecDeque<Result<ModelEvent, LlmError>>,
    seen_calls: HashSet<u32>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    finished: bool,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<ChunkUsage>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    /// DeepSeek, vLLM and llama.cpp use `reasoning_content`; OpenRouter and
    /// Ollama use `reasoning`.
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: u32,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    prompt_tokens_details: Option<PromptDetails>,
    completion_tokens_details: Option<CompletionDetails>,
}

#[derive(Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl StreamState {
    fn new(bytes: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>) -> Self {
        Self {
            bytes,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            seen_calls: HashSet::new(),
            usage: None,
            finish_reason: None,
            finished: false,
        }
    }

    fn handle(&mut self, data: &str) {
        if self.finished {
            return;
        }
        if data.trim() == "[DONE]" {
            self.end();
            return;
        }
        let chunk: Chunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => {
                debug!(%data, "unparseable chunk");
                self.fail(LlmError::Protocol(format!("unparseable chunk: {e}")));
                return;
            }
        };
        if let Some(err) = chunk.error {
            self.fail(LlmError::Protocol(format!("provider error: {err}")));
            return;
        }
        if let Some(u) = chunk.usage {
            let cached = u.prompt_tokens_details.map(|d| d.cached_tokens).unwrap_or(0);
            self.usage = Some(Usage {
                // OpenAI counts cached tokens inside prompt_tokens; report them separately.
                input_tokens: u.prompt_tokens.saturating_sub(cached),
                output_tokens: u.completion_tokens,
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                reasoning_tokens: u.completion_tokens_details.map(|d| d.reasoning_tokens).unwrap_or(0),
            });
        }
        for choice in chunk.choices {
            let d = choice.delta;
            if let Some(t) = d.reasoning_content.or(d.reasoning).filter(|t| !t.is_empty()) {
                self.pending.push_back(Ok(ModelEvent::ThinkingDelta(t)));
            }
            if let Some(t) = d.content.filter(|t| !t.is_empty()) {
                self.pending.push_back(Ok(ModelEvent::TextDelta(t)));
            }
            for tc in d.tool_calls {
                let (name, args) = tc.function.map(|f| (f.name, f.arguments)).unwrap_or((None, None));
                if self.seen_calls.insert(tc.index) {
                    let id = tc.id.filter(|s| !s.is_empty()).unwrap_or_else(|| format!("call_{}", tc.index));
                    self.pending.push_back(Ok(ModelEvent::ToolCallStart { index: tc.index, id, name: name.unwrap_or_default() }));
                }
                if let Some(a) = args.filter(|a| !a.is_empty()) {
                    self.pending.push_back(Ok(ModelEvent::ToolCallDelta { index: tc.index, args: a }));
                }
            }
            if let Some(r) = choice.finish_reason {
                self.finish_reason = Some(r);
            }
        }
    }

    fn end(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(u) = self.usage.take() {
            self.pending.push_back(Ok(ModelEvent::Usage(u)));
        }
        match self.finish_reason.take() {
            Some(stop_reason) => self.pending.push_back(Ok(ModelEvent::Done { stop_reason })),
            None => self.pending.push_back(Err(LlmError::Protocol("stream ended without a finish reason".into()))),
        }
    }

    fn fail(&mut self, e: LlmError) {
        self.pending.push_back(Err(e));
        self.finished = true;
    }
}

/// `GET {base}/models`: ids of the models a connection offers.
pub async fn list_models(base_url: &str, bearer: &str) -> Result<Vec<String>, LlmError> {
    crate::install_crypto_provider();
    let http =
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().map_err(|e| LlmError::Config(e.to_string()))?;
    let mut req = http.get(format!("{}/models", base_url.trim_end_matches('/')));
    if !bearer.is_empty() {
        req = req.bearer_auth(bearer);
    }
    let resp = req.send().await.map_err(|e| LlmError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| LlmError::Network(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(LlmError::Http { status, body: truncate(&text, 2000), retry_after: None });
    }
    #[derive(Deserialize)]
    struct Models {
        data: Vec<ModelId>,
    }
    #[derive(Deserialize)]
    struct ModelId {
        id: String,
    }
    let models: Models = serde_json::from_str(&text).map_err(|e| LlmError::Protocol(e.to_string()))?;
    let mut ids: Vec<String> = models.data.into_iter().map(|m| m.id).collect();
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Accumulator, Message, ToolSpec};

    fn parse(chunks: &[&str]) -> Vec<Result<ModelEvent, String>> {
        let mut st = StreamState::new(futures::stream::empty().boxed());
        for c in chunks {
            st.handle(c);
        }
        st.end();
        st.pending.into_iter().map(|r| r.map_err(|e| e.to_string())).collect()
    }

    #[test]
    fn parses_text_reasoning_tools_and_usage() {
        let evs = parse(&[
            r#"{"choices":[{"delta":{"role":"assistant","reasoning_content":"think"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":60}}}"#,
            "[DONE]",
        ]);
        let mut acc = Accumulator::default();
        for e in &evs {
            acc.push(e.as_ref().unwrap());
        }
        assert_eq!(acc.usage, Usage { input_tokens: 40, output_tokens: 20, cache_read_tokens: 60, ..Default::default() });
        assert_eq!(acc.stop_reason.as_deref(), Some("tool_calls"));
        let msg = acc.finish(true);
        assert_eq!(
            msg.blocks,
            vec![
                Block::Thinking("think".into()),
                Block::Text("Hi".into()),
                Block::ToolUse { id: "call_1".into(), name: "read".into(), input_json: "{\"path\":\"a\"}".into() }
            ]
        );
    }

    #[test]
    fn missing_finish_reason_is_an_error() {
        let evs = parse(&[r#"{"choices":[{"delta":{"content":"cut"}}]}"#]);
        assert!(evs.last().unwrap().is_err());
    }

    #[test]
    fn request_maps_tool_round_trip() {
        let c = OpenAiChat::new("http://localhost:1/v1/", "", HashMap::new()).unwrap();
        let req = ModelRequest {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![
                Message::user_text("do it"),
                Message {
                    role: Role::Assistant,
                    blocks: vec![
                        Block::Thinking("t".into()),
                        Block::ToolUse { id: "c1".into(), name: "read".into(), input_json: "{}".into() },
                    ],
                },
                Message { role: Role::User, blocks: vec![Block::ToolResult { id: "c1".into(), content: "ok".into(), is_error: false }] },
            ],
            tools: vec![ToolSpec { name: "read".into(), description: "d".into(), parameters: json!({"type":"object"}) }],
            max_output_tokens: None,
            cache_key: Some("k".into()),
            effort: None,
        };
        let body = c.request_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["content"], Value::Null);
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(msgs[3]["role"], "tool");
        assert!(body.get("prompt_cache_key").is_none(), "not sent to non-OpenAI hosts");
    }

    #[test]
    fn thinking_only_assistant_message_has_empty_content() {
        let c = OpenAiChat::new("http://localhost:1/v1", "", HashMap::new()).unwrap();
        let req = ModelRequest {
            model: "m".into(),
            system: "s".into(),
            messages: vec![
                Message::user_text("hi"),
                Message { role: Role::Assistant, blocks: vec![Block::Thinking("hmm".into())] },
                Message::user_text("again"),
            ],
            tools: vec![],
            max_output_tokens: None,
            cache_key: None,
            effort: None,
        };
        let body = c.request_body(&req);
        assert_eq!(body["messages"][2], json!({"role": "assistant", "content": ""}));
    }
}
