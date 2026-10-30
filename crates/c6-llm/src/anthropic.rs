//! Anthropic Messages API (`POST {base}/v1/messages`), streaming.
//!
//! - Adaptive thinking with summarized display, effort via `output_config`.
//! - Thinking blocks (with signatures, even when empty) and redacted thinking
//!   are replayed unchanged; the history is append-only, so they stay valid.
//! - Caching: an explicit breakpoint on the system prompt (tools + system)
//!   plus top-level automatic caching for the growing conversation.
//! - Tools stream eagerly; inputs are parsed strictly by the agent loop.

use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::http::{self, truncate};
use crate::models::{Thinking, model_info};
use crate::sse::SseDecoder;
use crate::{Block, EventStream, LlmError, ModelClient, ModelEvent, ModelRequest, Provider, Role, Usage};

const API_VERSION: &str = "2023-06-01";
/// Thinking budget for older models that still require one.
const LEGACY_THINKING_BUDGET: u32 = 16_000;

pub struct Anthropic {
    http: reqwest::Client,
    base_url: String,
    headers: HashMap<String, String>,
    /// Attempts at each request (`http::post_json`).
    attempts: u32,
}

impl Anthropic {
    pub fn new(base_url: &str, api_key: &str, extra: HashMap<String, String>) -> Result<Self, LlmError> {
        let mut headers = extra;
        headers.insert("anthropic-version".into(), API_VERSION.into());
        if !api_key.is_empty() {
            headers.insert("x-api-key".into(), api_key.into());
        }
        let base = base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base).to_owned();
        Ok(Self { http: http::client()?, base_url: base, headers, attempts: http::MAX_ATTEMPTS })
    }

    /// Gives up after the first failed attempt.
    pub fn once(self) -> Self {
        Self { attempts: 1, ..self }
    }

    pub fn request_body(&self, req: &ModelRequest) -> Value {
        let info = model_info(Provider::Anthropic, &req.model);
        let max_tokens = req.max_output_tokens.unwrap_or(64_000).min(info.max_output);
        let mut body = json!({
            "model": req.model,
            "max_tokens": max_tokens,
            "stream": true,
            "cache_control": {"type": "ephemeral"},
            "messages": messages(&req.messages),
        });
        // An empty system prompt is no block: Anthropic refuses cache_control
        // on an empty text block (calls that are all in the message, such as
        // suggestions and work's).
        if !req.system.is_empty() {
            body["system"] = json!([{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}]);
        }
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters, "eager_input_streaming": true}))
            .collect();
        if req.web_search && info.tool_profile == crate::ToolProfile::Claude {
            // Dynamic filtering needs the 4.6+ family; older models get the basic tool.
            let kind = if info.thinking == Thinking::Adaptive { "web_search_20260209" } else { "web_search_20250305" };
            tools.push(json!({"type": kind, "name": "web_search"}));
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        match info.thinking {
            // No reasoning, whatever the model would do by default.
            _ if req.effort.as_deref() == Some("none") => body["thinking"] = json!({"type": "disabled"}),
            Thinking::Adaptive => {
                body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
                if let Some(e) = req.effort.as_deref().or(info.default_effort) {
                    body["output_config"] = json!({"effort": e});
                }
            }
            Thinking::Budget => {
                let budget = LEGACY_THINKING_BUDGET.min(max_tokens.saturating_sub(1024));
                if budget >= 1024 {
                    body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
                }
            }
            Thinking::None => {}
        }
        body
    }
}

fn messages(msgs: &[crate::Message]) -> Vec<Value> {
    msgs.iter()
        .filter_map(|m| {
            let content: Vec<Value> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Text(t) if !t.is_empty() => Some(json!({"type": "text", "text": t})),
                    Block::Image { media_type, data } if m.role == Role::User => {
                        use base64::Engine;
                        let data = base64::engine::general_purpose::STANDARD.encode(data);
                        Some(json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}))
                    }
                    Block::SignedThinking { text, signature } if m.role == Role::Assistant => {
                        Some(json!({"type": "thinking", "thinking": text, "signature": signature}))
                    }
                    Block::RedactedThinking(data) if m.role == Role::Assistant => Some(json!({"type": "redacted_thinking", "data": data})),
                    Block::Opaque { provider, json, .. } if provider == "anthropic" && m.role == Role::Assistant => {
                        serde_json::from_str(json).ok()
                    }
                    Block::ToolUse { id, name, input_json } => {
                        let input: Value = serde_json::from_str(input_json).unwrap_or_else(|_| json!({}));
                        Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
                    }
                    Block::ToolResult { id, content, is_error } => Some(json!({
                        "type": "tool_result", "tool_use_id": id, "is_error": is_error,
                        "content": if content.is_empty() { "(no output)" } else { content.as_str() },
                    })),
                    // Unsigned thinking and other providers' reasoning are not replayable here.
                    _ => None,
                })
                .collect();
            if content.is_empty() {
                return None;
            }
            let role = if m.role == Role::User { "user" } else { "assistant" };
            Some(json!({"role": role, "content": content}))
        })
        .collect()
}

#[async_trait]
impl ModelClient for Anthropic {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let body = self.request_body(req);
        let resp = http::post_json(&self.http, &format!("{}/v1/messages", self.base_url), &self.headers, &body, self.attempts).await?;
        let state = State::new(resp.bytes_stream().boxed());
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
                    Some(Err(e)) => st.fail(LlmError::Network(e.to_string())),
                    None => {
                        for data in st.decoder.finish() {
                            st.handle(&data);
                        }
                        if !st.finished {
                            st.fail(LlmError::Protocol("stream ended before message_stop".into()));
                        }
                    }
                }
            }
        })
        .boxed())
    }
}

/// A content block being streamed.
enum Open {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    Redacted(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// Server-run tool blocks: kept whole; `input` arrives as JSON deltas.
    Opaque {
        block: Value,
        input_json: String,
    },
}

struct State {
    bytes: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    decoder: SseDecoder,
    pending: VecDeque<Result<ModelEvent, LlmError>>,
    open: HashMap<u32, Open>,
    usage: Usage,
    stop_reason: Option<String>,
    finished: bool,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl State {
    fn new(bytes: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>) -> Self {
        Self {
            bytes,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            open: HashMap::new(),
            usage: Usage::default(),
            stop_reason: None,
            finished: false,
        }
    }

    fn merge_usage(&mut self, u: RawUsage) {
        // message_start carries input counts; message_delta carries the running output count.
        if let Some(v) = u.input_tokens {
            self.usage.input_tokens = v;
        }
        if let Some(v) = u.output_tokens {
            self.usage.output_tokens = v;
        }
        if let Some(v) = u.cache_creation_input_tokens {
            self.usage.cache_write_tokens = v;
        }
        if let Some(v) = u.cache_read_input_tokens {
            self.usage.cache_read_tokens = v;
        }
    }

    fn handle(&mut self, data: &str) {
        if self.finished {
            return;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => return self.fail(LlmError::Protocol(format!("unparseable event: {e}"))),
        };
        let index = v["index"].as_u64().unwrap_or(0) as u32;
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                if let Ok(u) = serde_json::from_value::<RawUsage>(v["message"]["usage"].clone()) {
                    self.merge_usage(u);
                }
            }
            "content_block_start" => {
                let b = &v["content_block"];
                let open = match b["type"].as_str().unwrap_or("") {
                    "text" => Open::Text(b["text"].as_str().unwrap_or("").to_owned()),
                    "thinking" => Open::Thinking {
                        text: b["thinking"].as_str().unwrap_or("").to_owned(),
                        signature: b["signature"].as_str().unwrap_or("").to_owned(),
                    },
                    "redacted_thinking" => Open::Redacted(b["data"].as_str().unwrap_or("").to_owned()),
                    "tool_use" => Open::ToolUse {
                        id: b["id"].as_str().unwrap_or("").to_owned(),
                        name: b["name"].as_str().unwrap_or("").to_owned(),
                        json: String::new(),
                    },
                    _ => Open::Opaque { block: b.clone(), input_json: String::new() },
                };
                self.open.insert(index, open);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                let Some(open) = self.open.get_mut(&index) else { return };
                match (d["type"].as_str().unwrap_or(""), open) {
                    ("text_delta", Open::Text(t)) => {
                        let s = d["text"].as_str().unwrap_or("");
                        t.push_str(s);
                        self.pending.push_back(Ok(ModelEvent::TextDelta(s.to_owned())));
                    }
                    ("thinking_delta", Open::Thinking { text, .. }) => {
                        let s = d["thinking"].as_str().unwrap_or("");
                        text.push_str(s);
                        if !s.is_empty() {
                            self.pending.push_back(Ok(ModelEvent::ThinkingDelta(s.to_owned())));
                        }
                    }
                    ("signature_delta", Open::Thinking { signature, .. }) => signature.push_str(d["signature"].as_str().unwrap_or("")),
                    ("input_json_delta", Open::ToolUse { json, .. }) => json.push_str(d["partial_json"].as_str().unwrap_or("")),
                    ("input_json_delta", Open::Opaque { input_json, .. }) => input_json.push_str(d["partial_json"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            "content_block_stop" => {
                let block = match self.open.remove(&index) {
                    Some(Open::Text(t)) => Block::Text(t),
                    Some(Open::Thinking { text, signature }) => Block::SignedThinking { text, signature },
                    Some(Open::Redacted(d)) => Block::RedactedThinking(d),
                    Some(Open::ToolUse { id, name, json }) => {
                        Block::ToolUse { id, name, input_json: if json.trim().is_empty() { "{}".into() } else { json } }
                    }
                    Some(Open::Opaque { mut block, input_json }) => {
                        if let Ok(input) = serde_json::from_str::<Value>(&input_json) {
                            block["input"] = input;
                        }
                        let label = opaque_label(&block);
                        Block::Opaque { provider: "anthropic".into(), json: block.to_string(), label }
                    }
                    None => return,
                };
                self.pending.push_back(Ok(ModelEvent::Block { index, block }));
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Ok(u) = serde_json::from_value::<RawUsage>(v["usage"].clone()) {
                    self.merge_usage(u);
                }
            }
            "message_stop" => {
                self.finished = true;
                self.pending.push_back(Ok(ModelEvent::Usage(self.usage)));
                let stop_reason = self.stop_reason.take().unwrap_or_else(|| "end_turn".into());
                self.pending.push_back(Ok(ModelEvent::Done { stop_reason }));
            }
            "error" => {
                let msg = v["error"]["message"].as_str().unwrap_or("unknown error").to_owned();
                let kind = v["error"]["type"].as_str().unwrap_or("");
                let err = if http::is_context_overflow(&msg) {
                    LlmError::ContextOverflow(msg)
                } else if kind == "overloaded_error" {
                    LlmError::Http { status: 529, body: truncate(&msg, 500), retry_after: None }
                } else {
                    LlmError::Protocol(format!("{kind}: {msg}"))
                };
                self.fail(err);
            }
            _ => {} // ping and future event types
        }
    }

    fn fail(&mut self, e: LlmError) {
        self.pending.push_back(Err(e));
        self.finished = true;
    }
}

/// `GET {base}/v1/models`: model ids, newest first as returned.
pub async fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>, LlmError> {
    let http = http::client()?;
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let resp = http
        .get(format!("{base}/v1/models?limit=100"))
        .header("x-api-key", api_key)
        .header("anthropic-version", API_VERSION)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| LlmError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| LlmError::Network(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(LlmError::Http { status, body: truncate(&text, 2000), retry_after: None });
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| LlmError::Protocol(e.to_string()))?;
    Ok(v["data"].as_array().map(|a| a.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect()).unwrap_or_default())
}

/// A short description of a server-tool block for the UI.
fn opaque_label(b: &Value) -> String {
    let kind = b["type"].as_str().unwrap_or("block");
    match kind {
        "server_tool_use" => {
            let name = b["name"].as_str().unwrap_or("tool");
            match b["input"]["query"].as_str().or(b["input"]["url"].as_str()) {
                Some(q) => format!("{name}: {q}"),
                None => name.to_owned(),
            }
        }
        k if k.ends_with("_tool_result") => match b["content"].as_array() {
            Some(results) => format!("{} result{}", results.len(), if results.len() == 1 { "" } else { "s" }),
            None => "tool result".to_owned(),
        },
        k => k.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Accumulator, Message, ToolSpec};

    fn run(events: &[&str]) -> Vec<Result<ModelEvent, String>> {
        let mut st = State::new(futures::stream::empty().boxed());
        for e in events {
            st.handle(e);
        }
        st.pending.into_iter().map(|r| r.map_err(|e| e.to_string())).collect()
    }

    #[test]
    fn parses_thinking_text_and_tool_use() {
        let evs = run(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":500,"cache_read_input_tokens":2000,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Reading."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let mut acc = Accumulator::default();
        for e in &evs {
            acc.push(e.as_ref().unwrap());
        }
        assert_eq!(
            acc.usage,
            Usage { input_tokens: 10, output_tokens: 42, cache_read_tokens: 2000, cache_write_tokens: 500, reasoning_tokens: 0 }
        );
        assert_eq!(acc.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(
            acc.finish(true).blocks,
            vec![
                Block::SignedThinking { text: "Plan.".into(), signature: "SIG".into() },
                Block::Text("Reading.".into()),
                Block::ToolUse { id: "toolu_1".into(), name: "Read".into(), input_json: "{\"file_path\":\"a.rs\"}".into() },
            ]
        );
    }

    #[test]
    fn server_tool_blocks_are_kept_verbatim() {
        let evs = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust 2026\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://a","title":"A","encrypted_content":"E"}]}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"pause_turn"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let mut acc = Accumulator::default();
        for e in &evs {
            acc.push(e.as_ref().unwrap());
        }
        assert_eq!(acc.stop_reason.as_deref(), Some("pause_turn"));
        let blocks = acc.finish(true).blocks;
        let Block::Opaque { json, label, .. } = &blocks[0] else { panic!() };
        assert_eq!(label, "web_search: rust 2026");
        assert_eq!(serde_json::from_str::<Value>(json).unwrap()["input"]["query"], "rust 2026");
        let Block::Opaque { label, .. } = &blocks[1] else { panic!() };
        assert_eq!(label, "1 result");
        // Replayed unchanged.
        let msgs = messages(&[crate::Message { role: Role::Assistant, blocks }]);
        assert_eq!(msgs[0]["content"][1]["type"], "web_search_tool_result");
    }

    #[test]
    fn overloaded_error_is_retryable_and_missing_stop_is_error() {
        let evs = run(&[r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#]);
        assert!(evs[0].as_ref().unwrap_err().contains("529"));
    }

    #[test]
    fn no_effort_turns_thinking_off() {
        let c = Anthropic::new("http://127.0.0.1:11434", "k", HashMap::new()).unwrap();
        for model in ["claude-opus-5-5", "qwen3.6:27b"] {
            let req = ModelRequest {
                model: model.into(),
                system: "s".into(),
                messages: vec![Message::user_text("sort this")],
                tools: vec![],
                max_output_tokens: Some(500),
                cache_key: None,
                effort: Some("none".into()),
                web_search: false,
                json: false,
            };
            let b = c.request_body(&req);
            assert_eq!(b["thinking"], json!({"type": "disabled"}), "{model}");
            assert!(b.get("output_config").is_none(), "{model}");
        }
    }

    #[test]
    fn request_replays_thinking_and_sets_caching() {
        let c = Anthropic::new("https://api.anthropic.com/v1", "k", HashMap::new()).unwrap();
        assert_eq!(c.base_url, "https://api.anthropic.com");
        let req = ModelRequest {
            model: "claude-opus-5-5".into(),
            system: "sys".into(),
            messages: vec![
                Message::user_text("go"),
                Message {
                    role: Role::Assistant,
                    blocks: vec![
                        Block::SignedThinking { text: String::new(), signature: "S".into() },
                        Block::Reasoning { id: "rs".into(), encrypted: "x".into(), summary: String::new() },
                        Block::ToolUse { id: "t1".into(), name: "Bash".into(), input_json: "{\"command\":\"ls\"}".into() },
                    ],
                },
                Message { role: Role::User, blocks: vec![Block::ToolResult { id: "t1".into(), content: String::new(), is_error: false }] },
            ],
            tools: vec![ToolSpec { name: "Bash".into(), description: "d".into(), parameters: json!({"type":"object"}) }],
            max_output_tokens: None,
            cache_key: None,
            effort: None,
            web_search: true,
            json: false,
        };
        let b = c.request_body(&req);
        assert_eq!(c.request_body(&ModelRequest { json: true, ..req.clone() }), b);
        assert_eq!(b["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["cache_control"]["type"], "ephemeral");
        // No system prompt: no system block (an empty one with
        // cache_control is refused).
        let b = c.request_body(&ModelRequest { system: String::new(), ..req.clone() });
        assert!(b.get("system").is_none(), "{b}");
        assert_eq!(b["tools"][0]["eager_input_streaming"], true);
        assert_eq!(b["tools"][1], json!({"type": "web_search_20260209", "name": "web_search"}));
        let asst = &b["messages"][1]["content"];
        assert_eq!(asst.as_array().unwrap().len(), 2, "Responses reasoning is skipped");
        assert_eq!(asst[0], json!({"type": "thinking", "thinking": "", "signature": "S"}));
        assert_eq!(asst[1]["input"]["command"], "ls");
        assert_eq!(b["messages"][2]["content"][0]["content"], "(no output)");
    }
}
