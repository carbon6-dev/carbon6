//! OpenAI Responses API (`POST {base}/responses`), streaming and stateless:
//! `store: false` with encrypted reasoning carried in the history, so any
//! worker can continue any session. Also used for ChatGPT-plan tokens from
//! "Sign in with ChatGPT".

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Once};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tracing::info;

use crate::http;
use crate::sse::SseDecoder;
use crate::{Block, EventStream, LlmError, ModelClient, ModelEvent, ModelRequest, Role, Usage};

/// ChatGPT's default Codex plan windows, as reported on a model response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatGptLimits {
    pub primary: Option<ChatGptWindow>,
    pub secondary: Option<ChatGptWindow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatGptWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    /// Unix seconds (not milliseconds or seconds from now).
    pub resets_at: Option<i64>,
}

/// Called once response headers arrive, before the stream is consumed.
pub type ChatGptLimitsHook = Arc<dyn Fn(ChatGptLimits) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// The legacy/default `x-codex` family, as in Codex's
/// `codex-rs/codex-api/src/rate_limits.rs`. A window needs a finite usage
/// percentage; malformed optional fields do not discard its usage.
pub fn parse_chatgpt_limits(headers: &reqwest::header::HeaderMap) -> Option<ChatGptLimits> {
    let window = |which: &str| {
        let prefix = format!("x-codex-{which}");
        let value = |suffix: &str| headers.get(format!("{prefix}-{suffix}")).and_then(|v| v.to_str().ok());
        let used_percent = value("used-percent")?.parse::<f64>().ok().filter(|v| v.is_finite())?;
        let window_minutes = value("window-minutes").and_then(|v| v.parse::<i64>().ok()).filter(|m| *m > 0);
        let resets_at = value("reset-at").and_then(|v| v.parse::<i64>().ok()).filter(|s| *s > 0);
        // Codex ignores an empty all-zero window too.
        if used_percent == 0.0 && window_minutes.is_none() && resets_at.is_none() {
            return None;
        }
        Some(ChatGptWindow { used_percent, window_minutes, resets_at })
    };
    let limits = ChatGptLimits { primary: window("primary"), secondary: window("secondary") };
    (limits.primary.is_some() || limits.secondary.is_some()).then_some(limits)
}

/// Diagnostic names only: never retain or log response header values.
fn usage_header_names(headers: &reqwest::header::HeaderMap) -> Vec<&str> {
    let mut names: Vec<_> = headers
        .keys()
        .map(|name| name.as_str())
        .filter(|name| name.starts_with("x-") || ["codex", "rate", "limit", "usage", "quota"].iter().any(|part| name.contains(part)))
        .collect();
    names.sort_unstable();
    names
}

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
    limits_hook: Option<ChatGptLimitsHook>,
}

impl OpenAiResponses {
    pub fn new(base_url: &str, bearer: &str, extra: HashMap<String, String>, plan: bool) -> Result<Self, LlmError> {
        let mut headers = extra;
        if !bearer.is_empty() {
            headers.insert("authorization".into(), format!("Bearer {bearer}"));
        }
        let official = url::Url::parse(base_url).ok().and_then(|u| u.host_str().map(|h| h == "api.openai.com")).unwrap_or(false);
        let base_url = base_url.trim_end_matches('/').to_owned();
        Ok(Self { http: http::client()?, base_url, headers, plan, official, attempts: http::MAX_ATTEMPTS, limits_hook: None })
    }

    /// Gives up after the first failed attempt.
    pub fn once(self) -> Self {
        Self { attempts: 1, ..self }
    }

    /// ChatGPT's backend keeps a conversation's prompt cache on one machine
    /// only when told which conversation a request belongs to, as Codex does
    /// with `session_id`; `prompt_cache_key` alone left about half of the
    /// calls of a session missing everything but the instructions.
    pub fn request_headers(&self, req: &ModelRequest) -> HashMap<String, String> {
        let mut headers = self.headers.clone();
        if self.plan
            && let Some(k) = req.cache_key.as_deref().filter(|k| !k.is_empty())
        {
            headers.entry("session_id".into()).or_insert_with(|| k.to_owned());
        }
        headers
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
                        Block::Text(t) | Block::AnnotatedText { text: t, .. } if !t.is_empty() => {
                            parts.push(json!({"type": "input_text", "text": t}));
                        }
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
                        Block::Text(t) | Block::AnnotatedText { text: t, .. } if !t.is_empty() => {
                            items.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": t}]}))
                        }
                        Block::ToolUse { id, name, input_json } => {
                            items.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": input_json}))
                        }
                        Block::Opaque { provider, json, .. } if provider == "openai" => {
                            if let Ok(v) = serde_json::from_str::<Value>(json) {
                                items.push(replayable(v));
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
    fn set_chatgpt_limits_hook(&mut self, hook: ChatGptLimitsHook) {
        self.limits_hook = Some(hook);
    }

    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let body = self.request_body(req);
        let headers = self.request_headers(req);
        let resp = http::post_json(&self.http, &format!("{}/responses", self.base_url), &headers, &body, self.attempts).await?;
        if self.plan {
            match parse_chatgpt_limits(resp.headers()) {
                Some(limits) => {
                    if let Some(hook) = &self.limits_hook {
                        hook(limits).await;
                    }
                }
                None => {
                    // Clients are rebuilt for head calls; this must be
                    // process-wide, not once per client or connection.
                    static MISSING_LIMITS: Once = Once::new();
                    MISSING_LIMITS.call_once(|| {
                        info!(
                            header_names = ?usage_header_names(resp.headers()),
                            limits_hook_installed = self.limits_hook.is_some(),
                            "ChatGPT reply has no usable x-codex usage headers"
                        );
                    });
                }
            }
        }
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
    annotations: HashMap<(u32, usize), Vec<Value>>,
    saw_tool_call: bool,
    finished: bool,
}

impl State {
    fn new(bytes: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>) -> Self {
        Self {
            bytes,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            annotations: HashMap::new(),
            saw_tool_call: false,
            finished: false,
        }
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
            "response.output_text.annotation.added" => {
                if let Some(annotation) = v.get("annotation").filter(|a| a.is_object()) {
                    let index = v["output_index"].as_u64().unwrap_or(0) as u32;
                    let part = v["content_index"].as_u64().unwrap_or(0) as usize;
                    self.annotations.entry((index, part)).or_default().push(annotation.clone());
                }
            }
            // Finished items define the message; deltas above are display only.
            "response.output_item.done" => {
                let index = v["output_index"].as_u64().unwrap_or(0) as u32;
                let mut item = v["item"].clone();
                if let Some(parts) = item["content"].as_array_mut() {
                    for (part, content) in parts.iter_mut().enumerate() {
                        if let Some(annotations) = self.annotations.remove(&(index, part))
                            && content["annotations"].as_array().is_none_or(Vec::is_empty)
                        {
                            content["annotations"] = json!(annotations);
                        }
                    }
                }
                if let Some(block) = item_block(&item) {
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

/// A kept built-in item as the API takes it back. Replies can carry `content`
/// on items whose input form refuses it (HTTP 400 "Unknown parameter"), which
/// would fail every later turn of the session.
fn replayable(mut item: Value) -> Value {
    if let Some(fields) = item.as_object_mut() {
        fields.remove("content");
    }
    item
}

fn item_block(item: &Value) -> Option<Block> {
    match item["type"].as_str()? {
        "message" => {
            let mut text = String::new();
            let mut annotations = Vec::new();
            for part in item["content"].as_array()? {
                let t = match part["type"].as_str() {
                    Some("output_text") => part["text"].as_str(),
                    Some("refusal") => part["refusal"].as_str(),
                    _ => None,
                };
                let Some(t) = t else { continue };
                let offset = text.chars().count() as u64;
                for mut annotation in part["annotations"].as_array().into_iter().flatten().cloned() {
                    for key in ["start_index", "end_index"] {
                        if let Some(n) = annotation[key].as_u64() {
                            annotation[key] = json!(n + offset);
                        }
                    }
                    annotations.push(annotation);
                }
                text.push_str(t);
            }
            Some(if annotations.is_empty() {
                Block::Text(text)
            } else {
                Block::AnnotatedText { text, annotations_json: serde_json::to_string(&annotations).unwrap() }
            })
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
    fn streamed_citations_survive_finished_items_without_changing_model_replay() {
        let text = "A fact. \u{e200}cite\u{e202}turn347046search1\u{e201}";
        let annotation = json!({"type": "url_citation", "url": "https://source.test/fact", "title": "The source", "start_index": 8, "end_index": text.chars().count()});
        let mut st = State::new(futures::stream::empty().boxed());
        for event in [
            json!({"type": "response.output_text.delta", "delta": text}),
            json!({"type": "response.output_text.annotation.added", "annotation": annotation}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "content": [{"type": "output_text", "text": text, "annotations": [annotation]}]}}),
        ] {
            st.handle(&event.to_string());
        }
        let mut acc = Accumulator::default();
        for event in st.pending {
            acc.push(&event.unwrap());
        }
        // Finished annotations are authoritative, including after interruption.
        let message = acc.finish(false);
        let [Block::AnnotatedText { text: kept, annotations_json }] = message.blocks.as_slice() else { panic!("missing annotations") };
        assert_eq!(kept, text);
        assert_eq!(serde_json::from_str::<Value>(annotations_json).unwrap(), json!([annotation]));
        assert_eq!(input_items(&[message]), input_items(&[Message { role: Role::Assistant, blocks: vec![Block::Text(text.into())] }]));
    }

    #[test]
    fn annotation_events_are_kept_when_the_finished_item_omits_them() {
        let annotation = json!({"type": "url_citation", "url": "https://source.test", "title": "Source", "start_index": 0, "end_index": 4});
        let mut st = State::new(futures::stream::empty().boxed());
        st.handle(
            &json!({"type": "response.output_text.annotation.added", "output_index": 3, "content_index": 1, "annotation": annotation})
                .to_string(),
        );
        st.handle(
            &json!({"type": "response.output_item.done", "output_index": 3, "item": {"type": "message", "content": [
                {"type": "output_text", "text": "Été. "},
                {"type": "output_text", "text": "Fact."}
            ]}})
            .to_string(),
        );
        let mut acc = Accumulator::default();
        for event in st.pending {
            acc.push(&event.unwrap());
        }
        let message = acc.finish(true);
        let [Block::AnnotatedText { text, annotations_json }] = message.blocks.as_slice() else { panic!("missing streamed annotation") };
        assert_eq!(text, "Été. Fact.");
        let annotations: Value = serde_json::from_str(annotations_json).unwrap();
        assert_eq!(annotations[0]["url"], annotation["url"]);
        assert_eq!(annotations[0]["start_index"], 5);
        assert_eq!(annotations[0]["end_index"], 9);
    }

    #[test]
    fn kept_built_in_items_replay_without_content_the_api_refuses() {
        let msgs = [crate::Message {
            role: Role::Assistant,
            blocks: vec![Block::Opaque {
                provider: "openai".into(),
                json: json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q"}, "content": [{"type": "output_text", "text": "x"}]}).to_string(),
                label: "web_search: q".into(),
            }],
        }];
        let items = input_items(&msgs);
        assert_eq!(
            items,
            [json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q"}})]
        );
    }

    #[test]
    fn annotations_follow_concatenated_text_offsets_and_plain_messages_stay_plain() {
        let block = item_block(&json!({"type": "message", "content": [
            {"type": "output_text", "text": "Été. "},
            {"type": "output_text", "text": "Fact.", "annotations": [{"type": "url_citation", "url": "https://source.test", "title": "Source", "start_index": 0, "end_index": 4}]}
        ]})).unwrap();
        let Block::AnnotatedText { text, annotations_json } = block else { panic!("missing annotations") };
        assert_eq!(text, "Été. Fact.");
        let annotations: Value = serde_json::from_str(&annotations_json).unwrap();
        assert_eq!(annotations[0]["start_index"], 5);
        assert_eq!(annotations[0]["end_index"], 9);
        assert_eq!(
            item_block(&json!({"type": "message", "content": [{"type": "output_text", "text": "Plain."}]})),
            Some(Block::Text("Plain.".into()))
        );
    }

    #[test]
    fn usage_diagnostic_filters_and_sorts_only_header_names() {
        let mut headers = reqwest::header::HeaderMap::new();
        for name in [
            "x-request-id",
            "codex-status",
            "ratelimit-remaining",
            "request-limit",
            "server-usage",
            "quota-window",
            "content-type",
            "authorization",
            "set-cookie",
            "chatgpt-account-id",
        ] {
            headers.insert(name, "sensitive-header-value".parse().unwrap());
        }
        assert_eq!(
            usage_header_names(&headers),
            ["codex-status", "quota-window", "ratelimit-remaining", "request-limit", "server-usage", "x-request-id"]
        );
        assert!(usage_header_names(&reqwest::header::HeaderMap::new()).is_empty());
    }

    #[test]
    fn chatgpt_limit_headers_and_units() {
        let headers = reqwest::header::HeaderMap::from_iter([
            ("x-codex-primary-used-percent".parse().unwrap(), "34.5".parse().unwrap()),
            ("x-codex-primary-window-minutes".parse().unwrap(), "300".parse().unwrap()),
            ("x-codex-primary-reset-at".parse().unwrap(), "1793750400".parse().unwrap()),
            ("x-codex-secondary-used-percent".parse().unwrap(), "0".parse().unwrap()),
            ("x-codex-secondary-window-minutes".parse().unwrap(), "10080".parse().unwrap()),
            ("x-codex-secondary-reset-at".parse().unwrap(), "1794355200".parse().unwrap()),
        ]);
        assert_eq!(
            parse_chatgpt_limits(&headers),
            Some(ChatGptLimits {
                primary: Some(ChatGptWindow { used_percent: 34.5, window_minutes: Some(300), resets_at: Some(1_793_750_400) }),
                secondary: Some(ChatGptWindow { used_percent: 0.0, window_minutes: Some(10080), resets_at: Some(1_794_355_200) }),
            })
        );
    }

    #[test]
    fn chatgpt_missing_and_malformed_limit_headers() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        assert_eq!(parse_chatgpt_limits(&headers), None);
        // Length and reset alone must not invent zero usage.
        headers.insert("x-codex-primary-window-minutes", HeaderValue::from_static("300"));
        headers.insert("x-codex-primary-reset-at", HeaderValue::from_static("1793750400"));
        assert_eq!(parse_chatgpt_limits(&headers), None);
        for bad in ["bad", "NaN", "inf", "-inf", ""] {
            headers.insert("x-codex-primary-used-percent", HeaderValue::from_str(bad).unwrap());
            assert_eq!(parse_chatgpt_limits(&headers), None, "{bad}");
        }
        headers.insert("x-codex-primary-used-percent", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(parse_chatgpt_limits(&headers), None);
        headers.insert("x-codex-primary-used-percent", HeaderValue::from_static("101.5"));
        for bad in ["bad", "-1", "0", "1.5", "999999999999999999999999"] {
            headers.insert("x-codex-primary-window-minutes", HeaderValue::from_str(bad).unwrap());
            headers.insert("x-codex-primary-reset-at", HeaderValue::from_str(bad).unwrap());
            assert_eq!(
                parse_chatgpt_limits(&headers),
                Some(ChatGptLimits {
                    primary: Some(ChatGptWindow { used_percent: 101.5, window_minutes: None, resets_at: None }),
                    secondary: None,
                }),
                "{bad}"
            );
        }
        headers.clear();
        headers.insert("x-codex-primary-used-percent", HeaderValue::from_static("bad"));
        headers.insert("x-codex-secondary-used-percent", HeaderValue::from_static("12"));
        let limits = parse_chatgpt_limits(&headers).unwrap();
        assert!(limits.primary.is_none());
        assert_eq!(limits.secondary.unwrap().used_percent, 12.0, "a malformed primary doesn't discard the secondary");
        headers.insert("x-codex-secondary-used-percent", HeaderValue::from_static("0"));
        assert_eq!(parse_chatgpt_limits(&headers), None, "Codex ignores an all-zero window");
    }

    #[test]
    fn incomplete_output_limit_is_normalized() {
        let mut st = State::new(futures::stream::empty().boxed());
        st.handle(
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
        );
        let mut acc = Accumulator::default();
        for e in &st.pending {
            acc.push(e.as_ref().unwrap());
        }
        assert_eq!(acc.stop_reason.as_deref(), Some(crate::stop::MAX_TOKENS));
    }

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

    #[test]
    fn plan_requests_name_their_conversation() {
        let req = ModelRequest {
            model: "gpt-5.5".into(),
            system: String::new(),
            messages: vec![Message::user_text("go")],
            tools: vec![],
            max_output_tokens: None,
            cache_key: Some("sess".into()),
            effort: None,
            web_search: false,
            json: false,
        };
        let plan = OpenAiResponses::new("https://chatgpt.example/backend", "tok", HashMap::new(), true).unwrap();
        let h = plan.request_headers(&req);
        assert_eq!(h.get("session_id").map(String::as_str), Some("sess"));
        assert_eq!(h.get("authorization").map(String::as_str), Some("Bearer tok"));
        // Nothing to name: no header, rather than an empty one.
        assert!(!plan.request_headers(&ModelRequest { cache_key: Some(String::new()), ..req.clone() }).contains_key("session_id"));
        assert!(!plan.request_headers(&ModelRequest { cache_key: None, ..req.clone() }).contains_key("session_id"));
        // A connection's own header wins.
        let own =
            OpenAiResponses::new("https://chatgpt.example/backend", "tok", HashMap::from([("session_id".into(), "mine".into())]), true)
                .unwrap();
        assert_eq!(own.request_headers(&req)["session_id"], "mine");
        // API keys route by prompt_cache_key alone.
        let api = OpenAiResponses::new("https://api.openai.com/v1", "tok", HashMap::new(), false).unwrap();
        assert!(!api.request_headers(&req).contains_key("session_id"));
    }
}
