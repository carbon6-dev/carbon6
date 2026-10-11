//! One agent turn: model call → tools → model call … until the model stops.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use c6_llm::{Accumulator, Block, LlmError, Message, ModelClient, ModelEvent, ModelRequest, Role, ToolSpec, stop};
use c6_proto::{
    AssistantMessage, Compaction, ErrorEvent, TextDelta, ToolOutputDelta, ToolResult, TurnEndReason, TurnEnded, TurnStarted, Usage,
    ephemeral_event, session_event,
};
use c6_tools::{Concurrency, SessionTools, Tool, ToolCtx, ToolOutput};
use futures::StreamExt;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::history::{block_to_proto, compacted_context};
use crate::{EventSink, PendingInput};

/// Guards against runaway tool loops.
const MAX_STEPS: usize = 500;
/// Compact once the context reaches this share of the window.
const COMPACT_AT: f64 = 0.85;
const COMPACT_MAX_OUTPUT: u32 = 16_000;
const COMPACT_PROMPT: &str = "Your context window is nearly full, so this conversation will be replaced by a summary that you write now. \
Write a summary detailed enough to continue the work without the original conversation. Include: the user's requests and goals \
(quote any precise instructions), key decisions and their reasons, files examined and changed with their paths, the current state \
of the work, errors encountered and how they were resolved, and the exact next steps. Do not call any tools; reply with only the summary.";

#[derive(Clone)]
pub struct TurnConfig {
    pub model: String,
    pub system_prompt: String,
    pub max_output_tokens: Option<u32>,
    pub cache_key: String,
    pub effort: Option<String>,
    /// The model's context window, for automatic compaction.
    pub context_window: u64,
    /// Offer the provider's built-in web search, where it has one.
    pub web_search: bool,
}

/// Mutable conversation state carried across turns.
pub struct Conversation {
    pub history: Vec<Message>,
    /// Size of the last request (input + cache + output), for compaction.
    pub context_tokens: u64,
}

/// Runs one turn and records it. `pending` is drained at each model-request
/// boundary so input sent mid-turn reaches the model as soon as possible.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    turn: u64,
    cfg: &TurnConfig,
    client: &dyn ModelClient,
    tools: &[Arc<dyn Tool>],
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    conv: &mut Conversation,
    pending: &mut (dyn FnMut() -> Vec<PendingInput> + Send),
    cancel: &CancellationToken,
) -> anyhow::Result<TurnEndReason> {
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;
    let t = Turn { turn, cfg, client, specs: tools.iter().map(|t| t.spec()).collect(), sink, cancel };
    let reason = t.run(tools, session_tools, conv, pending).await?;
    sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: reason as i32 })).await?;
    Ok(reason)
}

struct Turn<'a> {
    turn: u64,
    cfg: &'a TurnConfig,
    client: &'a dyn ModelClient,
    specs: Vec<ToolSpec>,
    sink: &'a Arc<dyn EventSink>,
    cancel: &'a CancellationToken,
}

/// Outcome of one streamed model call.
struct Response {
    message: Option<Message>,
    usage: c6_llm::Usage,
    stop_reason: String,
    interrupted: bool,
    error: Option<LlmError>,
}

impl Turn<'_> {
    fn request(&self, messages: Vec<Message>, max_output_tokens: Option<u32>) -> ModelRequest {
        ModelRequest {
            model: self.cfg.model.clone(),
            system: self.cfg.system_prompt.clone(),
            messages,
            tools: self.specs.clone(),
            max_output_tokens,
            cache_key: Some(self.cfg.cache_key.clone()),
            effort: self.cfg.effort.clone(),
            web_search: self.cfg.web_search,
        }
    }

    /// Streams one model call, forwarding deltas live when `live` is set.
    async fn call(&self, req: &ModelRequest, live: bool) -> Result<Response, LlmError> {
        let mut stream = tokio::select! {
            _ = self.cancel.cancelled() => {
                return Ok(Response { message: None, usage: Default::default(), stop_reason: String::new(), interrupted: true, error: None });
            }
            r = self.client.stream(req) => r?,
        };
        let mut acc = Accumulator::default();
        let mut error = None;
        let mut interrupted = false;
        loop {
            let ev = tokio::select! {
                _ = self.cancel.cancelled() => { interrupted = true; break; }
                ev = stream.next() => ev,
            };
            match ev {
                None => break,
                Some(Err(e)) => {
                    error = Some(e);
                    break;
                }
                Some(Ok(ev)) => {
                    if live {
                        match &ev {
                            ModelEvent::TextDelta(t) => {
                                self.sink.ephemeral(ephemeral_event::Kind::Text(TextDelta { turn: self.turn, text: t.clone() }))
                            }
                            ModelEvent::ThinkingDelta(t) => {
                                self.sink.ephemeral(ephemeral_event::Kind::Thinking(TextDelta { turn: self.turn, text: t.clone() }))
                            }
                            _ => {}
                        }
                    }
                    acc.push(&ev);
                }
            }
        }
        let complete = !interrupted && error.is_none();
        let usage = acc.usage;
        let stop_reason = acc.stop_reason.clone().unwrap_or_default();
        let message = (complete || acc.has_content()).then(|| acc.finish(complete));
        Ok(Response { message, usage, stop_reason, interrupted, error })
    }

    async fn run(
        &self,
        tools: &[Arc<dyn Tool>],
        session_tools: &Arc<SessionTools>,
        conv: &mut Conversation,
        pending: &mut (dyn FnMut() -> Vec<PendingInput> + Send),
    ) -> anyhow::Result<TurnEndReason> {
        let by_name: HashMap<String, Arc<dyn Tool>> = self.specs.iter().map(|s| s.name.clone()).zip(tools.iter().cloned()).collect();
        let mut compacted_for_overflow = false;

        for _ in 0..MAX_STEPS {
            if self.cancel.is_cancelled() {
                return Ok(TurnEndReason::Interrupted);
            }
            if self.cfg.context_window > 0
                && conv.context_tokens as f64 > self.cfg.context_window as f64 * COMPACT_AT
                && let Err(e) = self.compact(conv).await?
            {
                return self.fail(format!("compaction failed: {e}")).await;
            }

            let req = self.request(conv.history.clone(), self.cfg.max_output_tokens);
            let resp = match self.call(&req, true).await {
                Ok(r) => r,
                Err(LlmError::ContextOverflow(_)) if !compacted_for_overflow => {
                    compacted_for_overflow = true;
                    if let Err(e) = self.compact(conv).await? {
                        return self.fail(format!("context window exceeded and compaction failed: {e}")).await;
                    }
                    continue;
                }
                Err(e) => return self.fail(format!("model request failed: {e}")).await,
            };

            let complete = !resp.interrupted && resp.error.is_none();
            if let Some(msg) = resp.message {
                self.sink
                    .append(session_event::Kind::Assistant(AssistantMessage {
                        turn: self.turn,
                        blocks: msg.blocks.iter().filter_map(block_to_proto).collect(),
                        stop_reason: if complete { resp.stop_reason.clone() } else { "interrupted".into() },
                    }))
                    .await?;
                if !msg.blocks.is_empty() {
                    conv.history.push(msg);
                }
            }
            self.record_usage(&resp.usage).await?;
            if resp.usage != Default::default() {
                let u = resp.usage;
                conv.context_tokens = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens;
            }
            if resp.interrupted {
                return Ok(TurnEndReason::Interrupted);
            }
            if let Some(e) = resp.error {
                return self.fail(format!("model stream failed: {e}")).await;
            }
            if resp.stop_reason == stop::REFUSAL {
                return self.fail("The model declined to continue this request.".into()).await;
            }

            let calls: Vec<(String, String, String)> = conv
                .history
                .last()
                .filter(|m| m.role == Role::Assistant)
                .map(|m| m.tool_uses().map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned())).collect())
                .unwrap_or_default();
            if calls.is_empty() && resp.stop_reason == stop::PAUSE_TURN {
                // A long server-tool turn paused; resend to let it continue.
                continue;
            }
            if calls.is_empty() && resp.stop_reason == stop::MAX_TOKENS {
                return self.fail("The response hit the output token limit before finishing.".into()).await;
            }

            let mut user_blocks: Vec<Block> = Vec::new();
            let results = if resp.stop_reason == stop::MAX_TOKENS {
                // The last call's input may be cut off; run none of them.
                calls
                    .iter()
                    .map(|(id, name, _)| {
                        let out = ToolOutput::err("Not run: your response hit the output token limit, so this call may be incomplete. Do the work in smaller steps.");
                        (id.clone(), name.clone(), out, 0)
                    })
                    .collect()
            } else {
                run_tools(&calls, &by_name, session_tools, self.sink, self.cancel).await
            };
            for (id, name, output, elapsed) in results {
                self.sink
                    .append(session_event::Kind::ToolResult(ToolResult {
                        call_id: id.clone(),
                        tool: name,
                        output: output.content.clone(),
                        is_error: output.is_error,
                        truncated: output.truncated,
                        duration_ms: elapsed,
                    }))
                    .await?;
                user_blocks.push(Block::ToolResult { id, content: output.content, is_error: output.is_error });
            }

            for input in pending() {
                self.sink.append(input.event()).await?;
                user_blocks.extend(input.blocks());
            }
            if user_blocks.is_empty() {
                return Ok(TurnEndReason::Completed);
            }
            conv.history.push(Message { role: Role::User, blocks: user_blocks });
        }
        self.fail(format!("stopped after {MAX_STEPS} model calls in one turn")).await
    }

    async fn record_usage(&self, u: &c6_llm::Usage) -> anyhow::Result<()> {
        if *u == Default::default() {
            return Ok(());
        }
        self.sink
            .append(session_event::Kind::Usage(Usage {
                model: self.cfg.model.clone(),
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_tokens: u.cache_read_tokens,
                cache_write_tokens: u.cache_write_tokens,
                reasoning_tokens: u.reasoning_tokens,
            }))
            .await?;
        Ok(())
    }

    /// Simple compaction: the model summarizes the conversation (same model,
    /// system and tools, so the cached prefix is reused) and the summary
    /// replaces the whole history. Nothing older is replayed, so no earlier
    /// reasoning blocks are sent again. The outer `Result` is a journal
    /// failure; the inner one a compaction failure.
    async fn compact(&self, conv: &mut Conversation) -> anyhow::Result<Result<(), String>> {
        let mut msgs = conv.history.clone();
        match msgs.last_mut() {
            Some(m) if m.role == Role::User => m.blocks.push(Block::Text(COMPACT_PROMPT.into())),
            _ => msgs.push(Message::user_text(COMPACT_PROMPT)),
        }
        let req = self.request(msgs, Some(COMPACT_MAX_OUTPUT));
        let resp = match self.call(&req, false).await {
            Ok(r) => r,
            Err(e) => return Ok(Err(e.to_string())),
        };
        self.record_usage(&resp.usage).await?;
        if let Some(e) = resp.error {
            return Ok(Err(e.to_string()));
        }
        let summary: String = resp
            .message
            .map(|m| {
                m.blocks.into_iter().filter_map(|b| if let Block::Text(t) = b { Some(t) } else { None }).collect::<Vec<_>>().join("\n")
            })
            .unwrap_or_default();
        if summary.trim().is_empty() {
            return Ok(Err("the model returned no summary".into()));
        }
        self.sink
            .append(session_event::Kind::Compaction(Compaction { summary: summary.clone(), tokens_before: conv.context_tokens }))
            .await?;
        conv.history = vec![Message::user_text(compacted_context(&summary))];
        conv.context_tokens = 0;
        Ok(Ok(()))
    }

    async fn fail(&self, message: String) -> anyhow::Result<TurnEndReason> {
        fail(self.sink.as_ref(), message).await
    }
}

async fn fail(sink: &dyn EventSink, message: String) -> anyhow::Result<TurnEndReason> {
    warn!(%message, "turn failed");
    sink.append(session_event::Kind::Error(ErrorEvent { message })).await?;
    Ok(TurnEndReason::Error)
}

/// Runs calls in order: consecutive shared (read-only) calls concurrently,
/// exclusive ones alone. Returns results in call order.
async fn run_tools(
    calls: &[(String, String, String)],
    by_name: &HashMap<String, Arc<dyn Tool>>,
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    cancel: &CancellationToken,
) -> Vec<(String, String, ToolOutput, u64)> {
    let mut out = Vec::with_capacity(calls.len());
    let mut i = 0;
    while i < calls.len() {
        let shared = |c: &(String, String, String)| by_name.get(&c.1).is_some_and(|t| t.concurrency() == Concurrency::Shared);
        let end = if shared(&calls[i]) { i + calls[i..].iter().take_while(|c| shared(c)).count() } else { i + 1 };
        let batch = calls[i..end].iter().map(|(id, name, input)| run_one(id, name, input, by_name, session_tools, sink, cancel));
        out.extend(join_all(batch).await);
        i = end;
    }
    out
}

async fn run_one(
    id: &str,
    name: &str,
    input: &str,
    by_name: &HashMap<String, Arc<dyn Tool>>,
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    cancel: &CancellationToken,
) -> (String, String, ToolOutput, u64) {
    let started = Instant::now();
    let output = if cancel.is_cancelled() {
        ToolOutput::err("Not run: the turn was interrupted.")
    } else {
        match (by_name.get(name), serde_json::from_str::<serde_json::Value>(input)) {
            (None, _) => ToolOutput::err(format!("Unknown tool `{name}`.")),
            (_, Err(e)) => ToolOutput::err(format!("Tool input is not valid JSON: {e}")),
            (Some(tool), Ok(value)) => {
                let call_id = id.to_owned();
                let stream_to = sink_output(sink, call_id);
                let ctx = ToolCtx { session: session_tools.clone(), cancel: cancel.clone(), output: stream_to, call_id: id.to_owned() };
                tool.call(&ctx, value).await
            }
        }
    };
    (id.to_owned(), name.to_owned(), output, started.elapsed().as_millis() as u64)
}

fn sink_output(sink: &Arc<dyn EventSink>, call_id: String) -> c6_tools::OutputSink {
    let sink = sink.clone();
    Arc::new(move |text: &str| {
        sink.ephemeral(ephemeral_event::Kind::ToolOutput(ToolOutputDelta { call_id: call_id.clone(), text: text.to_owned() }))
    })
}
