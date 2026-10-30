//! OpenAI Responses API (`POST {base}/responses`), streaming and stateless:
//! `store: false` with encrypted reasoning carried in the history, so any
//! worker can continue any session. Also used for ChatGPT-plan tokens from
//! "Sign in with ChatGPT".

use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::http;
use crate::sse::SseDecoder;
use crate::{Block, EventStream, LlmError, ModelClient, ModelEvent, ModelRequest, Role, Usage};

pub struct OpenAiResponses {
    http: reqwest::Client,
    base_url: String,
    headers: HashMap<String, String>,
    /// ChatGPT-plan tokens: the backend ignores sampling and output caps.
    plan: bool,
    /// api.openai.com (not a compatible server): built-in tools are available.
    official: bool,
    /// Attempts at each request (`http::post_json`).
    attempts: u32,
}

impl OpenAiResponses {
    pub fn new(base_url: &str, bearer: &str, extra: HashMap<String, String>, plan: bool) -> Result<Self, LlmError> {
        let mut headers = extra;
        if !bearer.is_empty() {
            headers.insert("authorization".into(), format!("Bearer {bearer}"));
        }
        let official = url::Url::parse(base_url).ok().and_then(|u| u.host_str().map(|h| h == "api.openai.com")).unwrap_or(false);
        let base_url = base_url.trim_end_matches('/').to_owned();
        Ok(Self { http: http::client()?, base_url, headers, plan, official, attempts: http::MAX_ATTEMPTS })
    }

    /// Gives up after the first failed attempt.
    pub fn once(self) -> Self {
        Self { attempts: 1, ..self }
    }

    pub fn request_body(&self, req: &ModelRequest) -> Value {
        let mut body = json!({
            "model": req.model,
            "instructions": req.system,
            "input": input_items(&req.messages),
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "parallel_tool_calls": true,
            "reasoning": {"summary": "auto"},
        });
        if let Some(e) = req.effort.as_deref() {
            body["reasoning"]["effort"] = e.into();
        }
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false}))
            .collect();
        if req.web_search && self.official {
            tools.push(json!({"type": "web_search"}));
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = "auto".into();
        }
        if let Some(k) = &req.cache_key {
            body["prompt_cache_key"] = k.clone().into();
        }
        if let (false, Some(n)) = (self.plan, req.max_output_tokens) {
            body["max_output_tokens"] = n.into();
        }
        body
    }
}

fn input_items(msgs: &[crate::Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for m in msgs {
        match m.role {
            Role::User => {
                // Text and images go together in one message.
                let mut parts: Vec<Value> = Vec::new();
                let flush = |parts: &mut Vec<Value>, items: &mut Vec<Value>| {
                    if !parts.is_empty() {
                        items.push(json!({"type": "message", "role": "user", "content": std::mem::take(parts)}));
                    }
                };
                for b in &m.blocks {
                    match b {
                        Block::ToolResult { id, content, .. } => {
                            flush(&mut parts, &mut items);
                            items.push(json!({"type": "function_call_output", "call_id": id, "output": content}))
                        }
                        Block::Text(t) if !t.is_empty() => parts.push(json!({"type": "input_text", "text": t})),
                        Block::Image { media_type, data } => {
                            parts.push(json!({"type": "input_image", "image_url": crate::data_url(media_type, data)}))
                        }
                        _ => {}
                    }
                }
                flush(&mut parts, &mut items);
            }
            Role::Assistant => {
                for b in &m.blocks {
                    match b {
                        Block::Reasoning { id, encrypted, summary } => {
                            let summary: Vec<Value> =
                                if summary.is_empty() { vec![] } else { vec![json!({"type": "summary_text", "text": summary})] };
                            items.push(json!({"type": "reasoning", "id": id, "encrypted_content": encrypted, "summary": summary}));
                        }
                        Block::Text(t) if !t.is_empty() => {
                            items.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": t}]}))
                        }
                        Block::ToolUse { id, name, input_json } => {
                            items.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": input_json}))
                        }
                        Block::Opaque { provider, json, .. } if provider == "openai" => {
                            if let Ok(v) = serde_json::from_str::<Value>(json) {
                                items.push(v);
                            }
                        }
                        // Other providers' reasoning is not replayable here.
                        _ => {}
                    }
                }
            }
        }
    }
    items
}

#[async_trait]
impl ModelClient for OpenAiResponses {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let body = self.request_body(req);
        let resp = http::post_json(&self.http, &format!("{}/responses", self.base_url), &self.headers, &body, self.attempts).await?;
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
                            st.fail(LlmError::Protocol("stream ended before response.completed".into()));
                        }
                    }
                }
            }
        })
        .boxed())
    }
}

struct State {
    bytes: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    decoder: SseDecoder,
    pending: VecDeque<Result<ModelEvent, LlmError>>,
    saw_tool_call: bool,
    finished: bool,
}

impl State {
    fn new(bytes: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>) -> Self {
        Self { bytes, decoder: SseDecoder::default(), pending: VecDeque::new(), saw_tool_call: false, finished: false }
    }

    fn handle(&mut self, data: &str) {
        if self.finished || data.trim() == "[DONE]" {
            return;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => return self.fail(LlmError::Protocol(format!("unparseable event: {e}"))),
        };
        match v["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    self.pending.push_back(Ok(ModelEvent::TextDelta(d.to_owned())));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    self.pending.push_back(Ok(ModelEvent::ThinkingDelta(d.to_owned())));
                }
            }
            // Finished items define the message; deltas above are display only.
            "response.output_item.done" => {
                let index = v["output_index"].as_u64().unwrap_or(0) as u32;
                if let Some(block) = item_block(&v["item"]) {
                    self.saw_tool_call |= matches!(block, Block::ToolUse { .. });
                    self.pending.push_back(Ok(ModelEvent::Block { index, block }));
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = &v["response"];
                let u = &r["usage"];
                let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                self.pending.push_back(Ok(ModelEvent::Usage(Usage {
                    input_tokens: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
                    output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                    cache_read_tokens: cached,
                    cache_write_tokens: 0,
                    reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
                })));
                let stop_reason = match r["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => crate::stop::MAX_TOKENS.to_owned(),
                    Some("content_filter") => crate::stop::REFUSAL.to_owned(),
                    Some(other) => other.to_owned(),
                    None if self.saw_tool_call => "tool_calls".to_owned(),
                    None => "stop".to_owned(),
                };
                self.pending.push_back(Ok(ModelEvent::Done { stop_reason }));
                self.finished = true;
            }
            "response.failed" => {
                let e = &v["response"]["error"];
                let msg = format!("{}: {}", e["code"].as_str().unwrap_or("failed"), e["message"].as_str().unwrap_or(""));
                self.fail(classify(&msg));
            }
            "error" => {
                let msg = format!("{}: {}", v["code"].as_str().unwrap_or("error"), v["message"].as_str().unwrap_or(""));
                self.fail(classify(&msg));
            }
            _ => {}
        }
    }

    fn fail(&mut self, e: LlmError) {
        self.pending.push_back(Err(e));
        self.finished = true;
    }
}

fn classify(msg: &str) -> LlmError {
    if http::is_context_overflow(msg) { LlmError::ContextOverflow(msg.to_owned()) } else { LlmError::Protocol(msg.to_owned()) }
}

fn item_block(item: &Value) -> Option<Block> {
    match item["type"].as_str()? {
        "message" => {
            let text: String = item["content"]
                .as_array()?
                .iter()
                .filter_map(|c| match c["type"].as_str() {
                    Some("output_text") => c["text"].as_str(),
                    Some("refusal") => c["refusal"].as_str(),
                    _ => None,
                })
                .collect();
            Some(Block::Text(text))
        }
        "function_call" => Some(Block::ToolUse {
            id: item["call_id"].as_str()?.to_owned(),
            name: item["name"].as_str()?.to_owned(),
            input_json: item["arguments"].as_str().unwrap_or("{}").to_owned(),
        }),
        "reasoning" => {
            let summary: Vec<&str> =
                item["summary"].as_array().map(|a| a.iter().filter_map(|s| s["text"].as_str()).collect()).unwrap_or_default();
            Some(Block::Reasoning {
                id: item["id"].as_str().unwrap_or("").to_owned(),
                encrypted: item["encrypted_content"].as_str().unwrap_or("").to_owned(),
                summary: summary.join("\n\n"),
            })
        }
        // Built-in tool calls (web_search_call …) are kept verbatim.
        kind => {
            let label = match item["action"]["query"].as_str() {
                Some(q) => format!("{}: {q}", kind.trim_end_matches("_call")),
                None => kind.trim_end_matches("_call").to_owned(),
            };
            Some(Block::Opaque { provider: "openai".into(), json: item.to_string(), label })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Accumulator, Message, ToolSpec};

    #[test]
    fn parses_reasoning_call_and_usage() {
        let mut st = State::new(futures::stream::empty().boxed());
        for e in [
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Look at files."}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"ENC","summary":[{"type":"summary_text","text":"Look at files."}]}}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":"{\"command\":[\"ls\"]}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":800},"output_tokens":50,"output_tokens_details":{"reasoning_tokens":30}}}}"#,
        ] {
            st.handle(e);
        }
        let mut acc = Accumulator::default();
        for e in &st.pending {
            acc.push(e.as_ref().unwrap());
        }
        assert_eq!(
            acc.usage,
            Usage { input_tokens: 200, output_tokens: 50, cache_read_tokens: 800, cache_write_tokens: 0, reasoning_tokens: 30 }
        );
        assert_eq!(acc.stop_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            acc.finish(true).blocks,
            vec![
                Block::Reasoning { id: "rs_1".into(), encrypted: "ENC".into(), summary: "Look at files.".into() },
                Block::ToolUse { id: "call_1".into(), name: "shell".into(), input_json: "{\"command\":[\"ls\"]}".into() },
            ]
        );
    }

    #[test]
    fn request_is_stateless_and_round_trips_items() {
        let c = OpenAiResponses::new("https://api.openai.com/v1", "tok", HashMap::new(), true).unwrap();
        let req = ModelRequest {
            model: "gpt-5.5".into(),
            system: "sys".into(),
            messages: vec![
                Message::user_text("go"),
                Message {
                    role: Role::Assistant,
                    blocks: vec![
                        Block::SignedThinking { text: "x".into(), signature: "s".into() },
                        Block::Reasoning { id: "rs_1".into(), encrypted: "ENC".into(), summary: String::new() },
                        Block::ToolUse { id: "call_1".into(), name: "shell".into(), input_json: "{}".into() },
                    ],
                },
                Message {
                    role: Role::User,
                    blocks: vec![Block::ToolResult { id: "call_1".into(), content: "ok".into(), is_error: false }],
                },
            ],
            tools: vec![ToolSpec { name: "shell".into(), description: "d".into(), parameters: json!({"type":"object"}) }],
            max_output_tokens: Some(1000),
            cache_key: Some("sess".into()),
            effort: Some("high".into()),
            web_search: true,
            json: false,
        };
        let b = c.request_body(&req);
        assert_eq!(c.request_body(&ModelRequest { json: true, ..req }), b);
        assert_eq!(b["store"], false);
        assert_eq!(b["include"][0], "reasoning.encrypted_content");
        assert_eq!(b["reasoning"]["effort"], "high");
        assert!(b.get("max_output_tokens").is_none(), "plan tokens: no output cap");
        let types: Vec<&str> = b["input"].as_array().unwrap().iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["message", "reasoning", "function_call", "function_call_output"]);
        assert_eq!(b["prompt_cache_key"], "sess");
        assert_eq!(b["tools"][1]["type"], "web_search");
    }
}
