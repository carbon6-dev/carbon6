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

use crate::history::{block_to_proto, compacted_context, messages_digest};
use crate::{EventSink, PendingInput};

/// Guards against runaway tool loops.
const MAX_STEPS: usize = 500;
/// How many times Stop hooks may keep one turn going.
const MAX_STOP_HOOK_RUNS: usize = 10;
/// Compact once the context reaches this share of the window.
const COMPACT_AT: f64 = 0.85;
const COMPACT_MAX_OUTPUT: u32 = 16_000;
const COMPACT_WHY: &str = "Your context window is nearly full, so this conversation will be replaced by a summary that you write now.";
const COMPACT_WHY_ASKED: &str = "The user asked to compact this conversation: it will be replaced by a summary that you write now.";
const COMPACT_PROMPT: &str = "Write a summary detailed enough to continue the work without the original conversation. Include: the user's requests and goals \
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

/// `/compact [focus]`: a turn that only summarizes the conversation, which
/// then replaces it.
#[allow(clippy::too_many_arguments)]
pub async fn compact_turn(
    turn: u64,
    cfg: &TurnConfig,
    client: &dyn ModelClient,
    tools: &[Arc<dyn Tool>],
    sink: &Arc<dyn EventSink>,
    conv: &mut Conversation,
    focus: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<TurnEndReason> {
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;
    let t = Turn { turn, cfg, client, specs: tools.iter().map(|t| t.spec()).collect(), sink, cancel };
    let reason = if conv.history.is_empty() {
        t.fail("there is nothing to compact yet".into()).await?
    } else {
        match t.compact(conv, COMPACT_WHY_ASKED, focus, false).await? {
            Ok(()) => TurnEndReason::Completed,
            Err(_) if cancel.is_cancelled() => TurnEndReason::Interrupted,
            Err(e) => t.fail(format!("compaction failed: {e}")).await?,
        }
    };
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
        let mut stop_hook_runs = 0;

        for _ in 0..MAX_STEPS {
            if self.cancel.is_cancelled() {
                return Ok(TurnEndReason::Interrupted);
            }
            if self.cfg.context_window > 0
                && conv.context_tokens as f64 > self.cfg.context_window as f64 * COMPACT_AT
                && let Err(e) = self.compact(conv, COMPACT_WHY, "", true).await?
            {
                return self.fail(format!("compaction failed: {e}")).await;
            }

            let req = self.request(conv.history.clone(), self.cfg.max_output_tokens);
            let resp = match self.call(&req, true).await {
                Ok(r) => r,
                Err(LlmError::ContextOverflow(_)) if !compacted_for_overflow => {
                    compacted_for_overflow = true;
                    if let Err(e) = self.compact(conv, COMPACT_WHY, "", true).await? {
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
            // A Stop hook can send the model back to work, a few times.
            if user_blocks.is_empty()
                && stop_hook_runs < MAX_STOP_HOOK_RUNS
                && let Some(h) = session_tools.hooks()
                && let Some(reason) = h.stop(stop_hook_runs > 0).await
            {
                stop_hook_runs += 1;
                let input =
                    PendingInput { text: format!("Stop hook feedback: {reason}"), command: "stop-hook".into(), ..Default::default() };
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
    /// reasoning blocks are sent again. If the model answers with no text
    /// (often a tool call: it carries on with the work), it is asked once
    /// more with the conversation as text and no tools. With `fallback`
    /// (automatic compaction), a mechanical digest stands in when that fails
    /// too, so the session is not stuck over its window. The outer `Result`
    /// is a journal failure; the inner one a compaction failure.
    async fn compact(&self, conv: &mut Conversation, why: &str, focus: &str, fallback: bool) -> anyhow::Result<Result<(), String>> {
        let mut prompt = format!("{why} {COMPACT_PROMPT}");
        if !focus.trim().is_empty() {
            prompt.push_str(&format!("\n\nThe user asked the summary to focus on: {}", focus.trim()));
        }
        let mut msgs = conv.history.clone();
        match msgs.last_mut() {
            Some(m) if m.role == Role::User => m.blocks.push(Block::Text(prompt.clone())),
            _ => msgs.push(Message::user_text(prompt.clone())),
        }
        let first = match self.summarize(self.request(msgs, Some(COMPACT_MAX_OUTPUT))).await? {
            Ok(summary) => return self.compacted(conv, summary).await,
            // A request that failed may fail again; an interrupt is final.
            Err(NoSummary::Failed(e)) => return Ok(Err(e)),
            Err(NoSummary::Answered(e) | NoSummary::Overflow(e)) => e,
        };
        warn!(error = %first, "compaction: no summary; retrying with the conversation as text and no tools");

        // The window holds the whole conversation, so its text (shorter:
        // tool output is clipped) fits too.
        let budget = self.digest_budget(2);
        let text = format!(
            "Below is a conversation between a user and you, an agent working on their task, as text.\n\n{}\n\n---\n\n{prompt}",
            messages_digest(&conv.history, budget)
        );
        let mut req = self.request(vec![Message::user_text(text)], Some(COMPACT_MAX_OUTPUT));
        req.tools.clear();
        req.web_search = false;
        let (second, answered) = match self.summarize(req).await? {
            Ok(summary) => return self.compacted(conv, summary).await,
            Err(NoSummary::Answered(e)) => (e, true),
            Err(NoSummary::Failed(e) | NoSummary::Overflow(e)) => (e, false),
        };
        let why = if second == first { first } else { format!("{first}; asked again without tools, {second}") };
        // Only when the model answered: a failed request is no reason to
        // drop the history.
        if !fallback || !answered || self.cancel.is_cancelled() {
            return Ok(Err(why));
        }

        warn!(error = %why, "compaction: falling back to a mechanical digest");
        let digest = format!(
            "(The model could not write a summary: {why}. This is a mechanical digest instead: the most recent steps, \
             with long tool input and output shortened, and before them only what the user wrote. Re-read files rather \
             than trusting shortened output.)\n\n{}",
            messages_digest(&conv.history, self.digest_budget(1))
        );
        self.compacted(conv, digest).await
    }

    /// One summarizing call: the summary, or what came back instead.
    async fn summarize(&self, req: ModelRequest) -> anyhow::Result<Result<String, NoSummary>> {
        let resp = match self.call(&req, false).await {
            Ok(r) => r,
            Err(e @ LlmError::ContextOverflow(_)) => return Ok(Err(NoSummary::Overflow(e.to_string()))),
            Err(e) => return Ok(Err(NoSummary::Failed(e.to_string()))),
        };
        self.record_usage(&resp.usage).await?;
        if resp.interrupted {
            return Ok(Err(NoSummary::Failed("interrupted".into())));
        }
        if let Some(e) = resp.error {
            return Ok(Err(NoSummary::Failed(e.to_string())));
        }
        let blocks = resp.message.map(|m| m.blocks).unwrap_or_default();
        let summary =
            blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect::<Vec<_>>().join("\n");
        if !summary.trim().is_empty() {
            return Ok(Ok(summary));
        }
        Ok(Err(NoSummary::Answered(no_summary(&blocks, &resp.stop_reason))))
    }

    async fn compacted(&self, conv: &mut Conversation, summary: String) -> anyhow::Result<Result<(), String>> {
        self.sink
            .append(session_event::Kind::Compaction(Compaction { summary: summary.clone(), tokens_before: conv.context_tokens }))
            .await?;
        conv.history = vec![Message::user_text(compacted_context(&summary))];
        conv.context_tokens = 0;
        Ok(Ok(()))
    }

    /// A digest of about `quarters` quarters of the context window (at about
    /// 3.5 characters a token).
    fn digest_budget(&self, quarters: u64) -> usize {
        let window = if self.cfg.context_window > 0 { self.cfg.context_window } else { 128_000 };
        (window * quarters / 4 * 7 / 2) as usize
    }

    async fn fail(&self, message: String) -> anyhow::Result<TurnEndReason> {
        fail(self.sink.as_ref(), message).await
    }
}

/// Why a summarizing call gave no summary.
enum NoSummary {
    /// The model answered, but with no text.
    Answered(String),
    /// The conversation did not fit.
    Overflow(String),
    /// The request failed or was interrupted.
    Failed(String),
}

/// What the model sent back instead of a summary.
fn no_summary(blocks: &[Block], stop_reason: &str) -> String {
    let calls: Vec<&str> =
        blocks.iter().filter_map(|b| if let Block::ToolUse { name, .. } = b { Some(name.as_str()) } else { None }).collect();
    let reasoned = blocks
        .iter()
        .any(|b| matches!(b, Block::Thinking(_) | Block::SignedThinking { .. } | Block::RedactedThinking(_) | Block::Reasoning { .. }));
    let what = if !calls.is_empty() {
        format!("the model called {} instead of writing a summary", calls.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", "))
    } else if reasoned {
        "the model only reasoned and wrote no summary".to_owned()
    } else {
        "the model returned no summary".to_owned()
    };
    if stop_reason.is_empty() { what } else { format!("{what} (stop reason: {stop_reason})") }
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
                let hooks = session_tools.hooks();
                let blocked = match &hooks {
                    // It may wait for the user's approval; an interrupt
                    // ends the wait.
                    Some(h) => tokio::select! {
                        r = h.pre_tool_use(id, name, &value) => r,
                        _ = cancel.cancelled() => Some("not run: the turn was interrupted".into()),
                    },
                    None => None,
                };
                if let Some(reason) = blocked {
                    ToolOutput::err(format!("Blocked by a PreToolUse hook: {reason}"))
                } else {
                    let call_id = id.to_owned();
                    let stream_to = sink_output(sink, call_id);
                    let ctx = ToolCtx { session: session_tools.clone(), cancel: cancel.clone(), output: stream_to, call_id: id.to_owned() };
                    let mut output = tool.call(&ctx, value.clone()).await;
                    if let Some(h) = &hooks
                        && let Some(feedback) = h.post_tool_use(name, &value, &output.content, output.is_error).await
                    {
                        output.content.push_str(&format!("\n\n<post-tool-use-hook>\n{feedback}\n</post-tool-use-hook>"));
                    }
                    output
                }
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
