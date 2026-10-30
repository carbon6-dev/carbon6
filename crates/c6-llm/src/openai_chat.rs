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
    /// Attempts at each request (`http::post_json`).
    attempts: u32,
}

impl OpenAiChat {
    pub fn new(base_url: &str, bearer: &str, headers: HashMap<String, String>) -> Result<Self, LlmError> {
        let http = http::client()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            bearer: bearer.to_owned(),
            headers,
            attempts: http::MAX_ATTEMPTS,
        })
    }

    /// Gives up after the first failed attempt.
    pub fn once(self) -> Self {
        Self { attempts: 1, ..self }
    }

    fn is_openai(&self) -> bool {
        crate::is_openai_api(&self.base_url)
    }

    pub fn request_body(&self, req: &ModelRequest) -> Value {
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        for m in &req.messages {
            match m.role {
                Role::User => {
                    let mut text = String::new();
                    let mut images = Vec::new();
                    for b in &m.blocks {
                        match b {
                            Block::ToolResult { id, content, .. } => {
                                messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                            }
                            Block::Text(t) => text.push_str(t),
                            Block::Image { media_type, data } => {
                                images.push(json!({"type": "image_url", "image_url": {"url": crate::data_url(media_type, data)}}))
                            }
                            _ => {}
                        }
                    }
                    if !images.is_empty() {
                        // With images, content is a list of parts.
                        let mut parts = vec![json!({"type": "text", "text": text})];
                        parts.extend(images);
                        messages.push(json!({"role": "user", "content": parts}));
                    } else if !text.is_empty() {
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
        if self.is_openai()
            && let Some(key) = &req.cache_key
        {
            body["prompt_cache_key"] = key.clone().into();
        }
        // Ollama and others take it too ("none" turns a thinking model's
        // reasoning off); a server that rejects it is asked again without.
        if let Some(e) = &req.effort {
            body["reasoning_effort"] = e.clone().into();
        }
        if req.json {
            body["response_format"] = json!({"type": "json_object"});
        }
        body
    }
}

/// Removes one rejected option, using locations before names or concepts.
fn remove_rejected(body: &mut Value, why: &str) -> bool {
    const OPTIONAL: &[(&str, &[&str])] = &[("response_format", &["json"]), ("reasoning_effort", &["reasoning", "think"])];
    let Some(fields) = body.as_object_mut() else { return false };
    let why = if let Ok(mut error) = serde_json::from_str::<Value>(why) {
        without_input(&mut error);
        let locations = [error.get("param"), error.pointer("/error/param")]
            .into_iter()
            .flatten()
            .chain(error["detail"].as_array().into_iter().flatten().flat_map(|d| d["loc"].as_array().into_iter().flatten()));
        for location in locations.filter_map(Value::as_str) {
            if OPTIONAL.iter().any(|(field, _)| *field == location) && fields.remove(location).is_some() {
                return true;
            }
        }
        error.to_string()
    } else {
        why.to_owned()
    }
    .to_lowercase();
    for exact in [true, false] {
        for (field, words) in OPTIONAL {
            let mentioned = if exact { why.contains(field) } else { words.iter().any(|word| why.contains(word)) };
            if mentioned && fields.remove(*field).is_some() {
                return true;
            }
        }
    }
    false
}

/// Leaves the error's explanation, without echoed request input.
fn without_input(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.remove("input");
            fields.remove("input_value");
            for value in fields.values_mut() {
                without_input(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(without_input),
        _ => {}
    }
}

#[async_trait]
impl ModelClient for OpenAiChat {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let mut body = self.request_body(req);
        let mut headers = self.headers.clone();
        if !self.bearer.is_empty() {
            headers.insert("authorization".into(), format!("Bearer {}", self.bearer));
        }
        let url = format!("{}/chat/completions", self.base_url);
        let resp = loop {
            match http::post_json(&self.http, &url, &headers, &body, self.attempts).await {
                // Each retry removes one field, so at most two retries.
                Err(LlmError::Http { status: 400 | 422, body: ref why, .. }) if remove_rejected(&mut body, why) => {}
                r => break r?,
            }
        };

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
    parse_models(&text)
}

/// A model list: OpenAI's `data` of `{id}` (sorted), or a ChatGPT plan's
/// `models` of `{slug, visibility}` (those listed, in its order).
fn parse_models(text: &str) -> Result<Vec<String>, LlmError> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| LlmError::Protocol(e.to_string()))?;
    if let Some(data) = v["data"].as_array() {
        let mut ids: Vec<String> = data.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect();
        ids.sort();
        return Ok(ids);
    }
    if let Some(models) = v["models"].as_array() {
        return Ok(models
            .iter()
            .filter(|m| m["visibility"].as_str().is_none_or(|vis| vis == "list"))
            .filter_map(|m| m["slug"].as_str().or_else(|| m["id"].as_str()).map(str::to_owned))
            .collect());
    }
    Err(LlmError::Protocol("the model list has neither data nor models".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Accumulator, Message, ToolSpec};

    #[test]
    fn optional_field_rejections_include_ollama_thinking_and_json_errors() {
        let body = json!({"reasoning_effort": "high", "response_format": {"type": "json_object"}, "model": "m"});
        for (why, field) in [
            ("Unknown parameter: reasoning_effort", "reasoning_effort"),
            (r#""qwen" does not support thinking"#, "reasoning_effort"),
            (r#"think value "high" is not supported for this model"#, "reasoning_effort"),
            ("Thinking is not supported", "reasoning_effort"),
            ("Unknown parameter: response_format", "response_format"),
            ("JSON mode is not supported", "response_format"),
        ] {
            let mut got = body.clone();
            assert!(remove_rejected(&mut got, why), "{why}");
            let mut expected = body.clone();
            expected.as_object_mut().unwrap().remove(field);
            assert_eq!(got, expected);
            assert!(!remove_rejected(&mut got, why), "already removed");
        }
        let mut got = body.clone();
        assert!(!remove_rejected(&mut got, "model not found"));
        assert!(!remove_rejected(&mut got, "max_tokens is too large"));
        assert_eq!(got, body);
        assert!(remove_rejected(&mut got, "reasoning and json are unsupported"));
        assert_eq!(got, json!({"model": "m", "reasoning_effort": "high"}));
        assert!(remove_rejected(&mut got, "reasoning and json are unsupported"));
        assert_eq!(got, json!({"model": "m"}));
    }

    #[test]
    fn exact_option_names_take_priority_over_concepts() {
        let body = json!({"reasoning_effort": "none", "response_format": {"type": "json_object"}});
        for (why, field) in [
            ("response_format is not supported with reasoning", "response_format"),
            ("reasoning_effort is not supported with JSON", "reasoning_effort"),
            ("response_format and reasoning_effort are not supported", "response_format"),
        ] {
            let mut got = body.clone();
            assert!(remove_rejected(&mut got, why));
            let mut expected = body.clone();
            expected.as_object_mut().unwrap().remove(field);
            assert_eq!(got, expected, "{why}");
        }
    }

    #[test]
    fn structured_locations_take_priority_over_echoed_input() {
        let body = json!({"reasoning_effort": "none", "response_format": {"type": "json_object"}});
        for error in [
            json!({"param": "reasoning_effort", "message": "JSON is not supported", "input": body}),
            json!({"error": {"param": "reasoning_effort", "message": "JSON is not supported", "input_value": body}}),
            json!({"detail": [{"loc": ["body", "reasoning_effort"], "msg": "JSON is not supported", "input": body}]}),
        ] {
            let mut got = body.clone();
            assert!(remove_rejected(&mut got, &error.to_string()));
            assert_eq!(got, json!({"response_format": {"type": "json_object"}}));
        }
        let mut got = body.clone();
        let error = json!({"detail": [{"msg": "does not support thinking", "context": {"input_value": body}}]});
        assert!(remove_rejected(&mut got, &error.to_string()));
        assert_eq!(got, json!({"response_format": {"type": "json_object"}}));
    }

    fn json_request(effort: &str) -> ModelRequest {
        ModelRequest {
            model: "mock-1".into(),
            system: "Return JSON.".into(),
            messages: vec![Message::user_text("Read this.")],
            tools: vec![],
            max_output_tokens: Some(1000),
            cache_key: None,
            effort: Some(effort.into()),
            web_search: false,
            json: true,
        }
    }

    async fn after_rejections(errors: &[(u16, &str)], effort: &str) -> Vec<Value> {
        use c6_testkit::{MockLlm, Reply, within};

        crate::install_crypto_provider();
        let mock = MockLlm::start().await;
        for (status, why) in errors {
            mock.push(Reply::Error(*status, (*why).into()));
        }
        mock.push(Reply::Text(r#"{"summary":"Read."}"#.into()));
        let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap().once();
        let req = json_request(effort);
        within("the answer after rejected options", async {
            let mut stream = client.stream(&req).await.unwrap();
            let mut acc = Accumulator::default();
            while let Some(event) = stream.next().await {
                acc.push(&event.unwrap());
            }
            assert_eq!(acc.finish(true).blocks, vec![Block::Text(r#"{"summary":"Read."}"#.into())]);
        })
        .await;
        mock.requests()
    }

    #[tokio::test]
    async fn a_server_rejecting_json_is_asked_again_without_it() {
        let requests = after_rejections(&[(400, "response_format is not supported")], "high").await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["response_format"], json!({"type": "json_object"}));
        let mut expected = requests[0].clone();
        expected.as_object_mut().unwrap().remove("response_format");
        assert_eq!(requests[1], expected);
        assert_eq!(requests[1]["reasoning_effort"], "high");
    }

    #[tokio::test]
    async fn a_server_rejecting_both_options_is_asked_without_each_in_turn() {
        let requests = after_rejections(&[(400, "does not support thinking"), (422, "JSON mode is not supported")], "high").await;
        assert_eq!(requests.len(), 3);
        let mut expected = requests[0].clone();
        assert_eq!(expected["reasoning_effort"], "high");
        assert_eq!(expected["response_format"], json!({"type": "json_object"}));
        expected.as_object_mut().unwrap().remove("reasoning_effort");
        assert_eq!(requests[1], expected);
        expected.as_object_mut().unwrap().remove("response_format");
        assert_eq!(requests[2], expected);
    }

    #[tokio::test]
    async fn an_echoed_request_keeps_reasoning_when_json_is_rejected() {
        let why = json!({"detail": [{"loc": ["body", "response_format"], "msg": "unsupported",
            "input": {"reasoning_effort": "none", "response_format": {"type": "json_object"}}}]})
        .to_string();
        let requests = after_rejections(&[(422, &why)], "none").await;
        assert_eq!(requests.len(), 2);
        let mut expected = requests[0].clone();
        expected.as_object_mut().unwrap().remove("response_format");
        assert_eq!(requests[1], expected);
        assert_eq!(requests[1]["reasoning_effort"], "none");
    }

    #[tokio::test]
    async fn an_echoed_reasoning_rejection_keeps_json() {
        let why = json!({"detail": [{"loc": ["body", "reasoning_effort"], "msg": "unsupported",
            "input": {"reasoning_effort": "none", "response_format": {"type": "json_object"}}}]})
        .to_string();
        let requests = after_rejections(&[(422, &why)], "none").await;
        assert_eq!(requests.len(), 2);
        let mut expected = requests[0].clone();
        expected.as_object_mut().unwrap().remove("reasoning_effort");
        assert_eq!(requests[1], expected);
        assert_eq!(requests[1]["response_format"], json!({"type": "json_object"}));
    }

    #[tokio::test]
    async fn an_unrelated_rejection_is_returned_without_retrying() {
        use c6_testkit::{MockLlm, Reply, within};

        crate::install_crypto_provider();
        let mock = MockLlm::start().await;
        let echoed = json!({"detail": [{"loc": ["body", "model"], "msg": "model not found",
            "input": {"reasoning_effort": "none", "response_format": {"type": "json_object"}},
            "context": {"input_value": {"reasoning_effort": "none", "response_format": {"type": "json_object"}}}}]})
        .to_string();
        let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap().once();
        for (n, (code, why)) in [(400, "model not found"), (422, echoed.as_str())].into_iter().enumerate() {
            mock.push(Reply::Error(code, why.into()));
            let result = within("the unrelated rejection", client.stream(&json_request("none"))).await;
            assert!(matches!(result, Err(LlmError::Http { status, body, .. }) if status == code && body == why));
            assert_eq!(mock.requests().len(), n + 1);
        }
    }

    #[test]
    fn request_sends_effort_output_limit_and_only_requested_json() {
        let c = OpenAiChat::new("http://localhost:1/v1", "", HashMap::new()).unwrap();
        let req = ModelRequest {
            model: "qwen3.8:27b".into(),
            system: "Draft a reply.".into(),
            messages: vec![Message::user_text("Can we meet tomorrow?")],
            tools: vec![],
            max_output_tokens: Some(9692),
            cache_key: None,
            effort: Some("high".into()),
            web_search: false,
            json: false,
        };
        let body = c.request_body(&req);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 9692);
        assert!(body.get("response_format").is_none());
        let body = c.request_body(&ModelRequest { json: true, ..req });
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 9692);
    }

    #[test]
    fn model_lists() {
        let openai = r#"{"object":"list","data":[{"id":"gpt-5","object":"model"},{"id":"gpt-4.1","object":"model"}]}"#;
        assert_eq!(parse_models(openai).unwrap(), vec!["gpt-4.1", "gpt-5"]);
        let plan = r#"{"models":[{"slug":"gpt-5.5","display_name":"GPT-5.5","visibility":"list"},{"slug":"internal","visibility":"hide"},{"slug":"gpt-5.5-mini","display_name":"GPT-5.5 mini","visibility":"list"}]}"#;
        assert_eq!(parse_models(plan).unwrap(), vec!["gpt-5.5", "gpt-5.5-mini"]);
        assert!(parse_models(r#"{"x":1}"#).is_err());
    }

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
            web_search: false,
            json: false,
        };
        let body = c.request_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["content"], Value::Null);
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(msgs[3]["role"], "tool");
        assert!(body.get("prompt_cache_key").is_none(), "not sent to non-OpenAI hosts");
        assert!(body.get("reasoning_effort").is_none());
        let body = c.request_body(&ModelRequest { effort: Some("none".into()), ..req });
        assert_eq!(body["reasoning_effort"], "none", "sent to every server (Ollama turns thinking off with it)");
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
            web_search: false,
            json: false,
        };
        let body = c.request_body(&req);
        assert_eq!(body["messages"][2], json!({"role": "assistant", "content": ""}));
    }
}
