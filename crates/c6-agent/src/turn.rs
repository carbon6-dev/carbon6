//! One agent turn: model call → tools → model call … until the model stops.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use c6_llm::{Accumulator, Block, LlmError, Message, ModelClient, ModelEvent, ModelRequest, Role, ToolSpec, stop};
use c6_proto::{
    AssistantMessage, Compaction, ErrorEvent, Notice, SessionActivity, TextDelta, ToolOutputDelta, ToolResult, TurnEndReason, TurnEnded,
    TurnStarted, Usage, ephemeral_event, session_event,
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
/// Compact once the context reaches this share of the window, unless the
/// connection says otherwise ([`TurnConfig::compact_at`]). Large windows
/// compact sooner: every call sends the whole conversation again, so a
/// long one near the top of a 272k window costs far more per step than its
/// summary does; small windows need the room to write the summary at all.
pub const COMPACT_AT: f64 = 0.85;
pub const COMPACT_AT_LARGE: f64 = 0.7;
/// Windows of at least this many tokens are large.
pub const LARGE_WINDOW: u64 = 100_000;

/// The share of `window` at which a conversation compacts: `setting` (a
/// percentage) when given, else the default for its size.
pub fn compact_share(window: u64, setting: u32) -> f64 {
    match setting {
        1..=100 => f64::from(setting) / 100.0,
        _ if window >= LARGE_WINDOW => COMPACT_AT_LARGE,
        _ => COMPACT_AT,
    }
}
/// Model calls that fail on the way (a dropped stream, an overloaded or
/// erroring provider) are tried again this many times, waiting longer each
/// time, before the turn stops with the error.
const MODEL_RETRIES: u32 = 6;
const MODEL_RETRY_MAX_WAIT_SECS: u64 = 60;

/// Whether trying a failed model call again may work: the network, the
/// provider's own failures, a response that broke off. Not: a request the
/// provider rejects (4xx), which fails the same way every time.
fn transient(e: &LlmError) -> bool {
    matches!(e, LlmError::Network(_) | LlmError::Protocol(_)) || e.retryable()
}
const COMPACT_MAX_OUTPUT: u32 = 16_000;
const COMPACT_MIN_OUTPUT: u32 = 256;
const COMPACT_FRAMING: u64 = 64;
/// Default prompts (each an editable record of the `prompts` collection,
/// docs/os.md §4.6): why the conversation is compacted, automatically or
/// when asked, and what the summary must hold.
pub const COMPACT_WHY: &str = "Your context window is nearly full, so this conversation will be replaced by a summary that you write now.";
pub const COMPACT_WHY_ASKED: &str = "Compaction was requested for this conversation: it will be replaced by a summary that you write now.";
pub const COMPACT_PROMPT: &str = "Write a summary detailed enough to continue the work without the original conversation. Include: the user's requests and goals \
(quote any precise instructions), key decisions and their reasons, files examined and changed with their paths, the current state \
of the work, errors encountered and how they were resolved, the ids of subagents that may need continuing, and the exact next steps. Preserve the attribution of API, automation and other-session messages; do not turn them into human requests. Do not call any tools; reply with only the summary.";
/// Default prompt for compacting a conversation while it is waiting on the user.
pub const IDLE_COMPACT_PROMPT: &str = "This session is idle and waiting for the user. Summarize what will matter on resumption: the goal and agreed plan, \
decisions and constraints, relevant files and why they matter, failed approaches, and open questions or next steps. The recent exchange will be appended \
verbatim separately. Preserve the attribution of instructions; do not call tools or continue the task. Reply only with the summary.";

const IDLE_KEEP_USER_TURNS: usize = 3;
const IDLE_TAIL_HEADER: &str = "\n\n## Recent conversation (verbatim)\nHistorical transcript; tool records below are not new tool calls.";

/// What the model is told when the conversation is compacted.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactPrompts {
    /// Before the summary instructions, when the context is full.
    pub why_full: String,
    /// Before the summary instructions, when the user asked (/compact).
    pub why_asked: String,
    /// What the summary must hold.
    pub instructions: String,
    /// What a trimmed tool output says instead (`{{call}}`).
    pub trimmed: String,
}

impl Default for CompactPrompts {
    fn default() -> Self {
        Self {
            why_full: COMPACT_WHY.into(),
            why_asked: COMPACT_WHY_ASKED.into(),
            instructions: COMPACT_PROMPT.into(),
            trimmed: crate::history::TRIMMED_OUTPUT.into(),
        }
    }
}

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
    pub compact: CompactPrompts,
    /// Percent of the window at which to compact; 0: [`compact_share`]'s
    /// default for the window.
    pub compact_at: u32,
    /// Tokens above which old tool output is trimmed from the conversation
    /// before a request (`history::trim_old_tool_output`); 0: never. Below
    /// the compaction threshold, so trimming comes first.
    pub trim_at: u64,
}

/// Mutable conversation state carried across turns.
pub struct Conversation {
    pub history: Vec<Message>,
    /// Reported size of the last request (input + cache + output).
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
    run_turn_with_tool_reminders(turn, cfg, client, tools, session_tools, sink, conv, pending, cancel, None).await
}

/// Renders an optional model-only advisory from the latest tool exchange, in
/// result order. Each entry is (tool name, input JSON, result text).
pub type ToolReminders<'a> = dyn Fn(&[(&str, &str, &str)]) -> Option<String> + Send + Sync + 'a;

#[allow(clippy::too_many_arguments)]
pub async fn run_turn_with_tool_reminders(
    turn: u64,
    cfg: &TurnConfig,
    client: &dyn ModelClient,
    tools: &[Arc<dyn Tool>],
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    conv: &mut Conversation,
    pending: &mut (dyn FnMut() -> Vec<PendingInput> + Send),
    cancel: &CancellationToken,
    reminders: Option<&ToolReminders<'_>>,
) -> anyhow::Result<TurnEndReason> {
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, context_window: cfg.context_window })).await?;
    let t = Turn { turn, cfg, client, specs: tools.iter().map(|t| t.spec()).collect(), sink, cancel };
    let reason = t.run(tools, session_tools, conv, pending, reminders).await?;
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
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, context_window: cfg.context_window })).await?;
    let t = Turn { turn, cfg, client, specs: tools.iter().map(|t| t.spec()).collect(), sink, cancel };
    let reason = if conv.history.is_empty() {
        t.fail("there is nothing to compact yet".into()).await?
    } else {
        let estimate = next_tokens(conv.context_tokens, t.estimate_request(&conv.history, &t.specs), t.reported_estimate(conv));
        match t.compact(conv, &cfg.compact.why_asked, focus, false, estimate).await? {
            Ok(()) => TurnEndReason::Completed,
            Err(_) if cancel.is_cancelled() => TurnEndReason::Interrupted,
            Err(e) => t.fail(format!("compaction failed: {e}")).await?,
        }
    };
    sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: reason as i32 })).await?;
    Ok(reason)
}

/// Compact at an idle boundary using the normal session prefix, retaining the
/// latest user turns as literal text in the durable compaction record.
#[allow(clippy::too_many_arguments)]
pub async fn idle_compact_turn(
    turn: u64,
    cfg: &TurnConfig,
    client: &dyn ModelClient,
    tools: &[Arc<dyn Tool>],
    sink: &Arc<dyn EventSink>,
    conv: &mut Conversation,
    focus: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<TurnEndReason> {
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, context_window: cfg.context_window })).await?;
    let t = Turn { turn, cfg, client, specs: tools.iter().map(|t| t.spec()).collect(), sink, cancel };
    let prompt = if focus.trim().is_empty() { IDLE_COMPACT_PROMPT } else { focus };
    let reason = match t.compact_idle(conv, prompt).await? {
        Ok(()) => TurnEndReason::Completed,
        Err(_) if cancel.is_cancelled() => TurnEndReason::Interrupted,
        Err(e) => t.fail(format!("idle compaction failed: {e}")).await?,
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
    account: Option<c6_llm::ModelAccount>,
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
            json: false,
        }
    }

    /// Estimate unreported requests and growth since the last report at 3.5
    /// characters per token, with fixed allowances for media instead of
    /// counting serialized bytes (which do not reflect model context usage).
    fn estimate_request(&self, history: &[Message], specs: &[ToolSpec]) -> u64 {
        let mut chars = self.cfg.system_prompt.chars().count() as u64;
        chars += serde_json::to_string(specs).unwrap_or_default().chars().count() as u64;
        for message in history {
            chars += 16;
            for block in &message.blocks {
                chars += match block {
                    Block::Image { .. } => 3_584,
                    Block::Document { .. } => 14_336,
                    _ => serde_json::to_string(block).unwrap_or_default().chars().count() as u64,
                };
            }
        }
        (chars * 2).div_ceil(7)
    }

    /// A report includes the last assistant reply, but not the tool results
    /// and user messages that follow it.
    fn reported_estimate(&self, conv: &Conversation) -> Option<u64> {
        (conv.context_tokens > 0).then(|| {
            let end = conv.history.iter().rposition(|m| m.role == Role::Assistant).map_or(0, |i| i + 1);
            self.estimate_request(&conv.history[..end], &self.specs)
        })
    }

    /// Summary space below the compaction threshold, beside the normal
    /// system prompt, tools and summary wrapper that return afterwards,
    /// with a little room for framing.
    fn summary_room(&self) -> u64 {
        if self.cfg.context_window == 0 {
            return u64::MAX;
        }
        let prefix = self.estimate_request(&[Message::user_text(compacted_context(""))], &self.specs);
        ((self.cfg.context_window as f64 * compact_share(self.cfg.context_window, self.cfg.compact_at)) as u64)
            .saturating_sub(prefix)
            .saturating_sub(COMPACT_FRAMING)
    }

    fn no_summary_room(&self) -> String {
        let prefix = self.estimate_request(&[], &self.specs);
        format!(
            "the system prompt and tools take about {prefix} of {}'s {} tokens, leaving no room to compact this conversation: \
             give the session fewer tools (a routing rule's tool list) or a larger context window",
            self.cfg.model, self.cfg.context_window
        )
    }

    /// At any request boundary (including the first request after a cold
    /// resume), replace only old tool-result contents, never pending user text.
    async fn trim_for_request(&self, conv: &mut Conversation, estimate: u64, next: u64, extra_tokens: u64) -> anyhow::Result<Option<u64>> {
        if self.cfg.trim_at == 0 || next <= self.cfg.trim_at {
            return Ok(None);
        }
        let mut history = conv.history.clone();
        if crate::history::trim_old_tool_output(&mut history, &self.cfg.compact.trimmed) == 0 {
            return Ok(None);
        }
        let after = self.estimate_request(&history, &self.specs) + extra_tokens;
        // Rewriting a prefix is only worthwhile when it saves at least a fifth.
        if u128::from(after) * 5 > u128::from(estimate) * 4 {
            return Ok(None);
        }
        self.context_notice("Trimmed old tool output", format!("Estimated context: {estimate} → {after} tokens.")).await?;
        conv.history = history;
        Ok(Some(after))
    }

    async fn context_notice(&self, title: &str, message: String) -> anyhow::Result<()> {
        self.sink.append(session_event::Kind::Notice(Notice { title: title.into(), message, priority: "low".into(), quiet: true })).await?;
        Ok(())
    }

    /// Streams one model call, forwarding deltas live when `live` is set.
    async fn call(&self, req: &ModelRequest, live: bool) -> Result<Response, LlmError> {
        let mut stream = tokio::select! {
            _ = self.cancel.cancelled() => {
                return Ok(Response { message: None, usage: Default::default(), account: None, stop_reason: String::new(), interrupted: true, error: None });
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
                            ModelEvent::TextDelta(t) => self.sink.ephemeral(ephemeral_event::Kind::Text(TextDelta {
                                turn: self.turn,
                                text: t.clone(),
                                ..Default::default()
                            })),
                            ModelEvent::ThinkingDelta(t) => self.sink.ephemeral(ephemeral_event::Kind::Thinking(TextDelta {
                                turn: self.turn,
                                text: t.clone(),
                                ..Default::default()
                            })),
                            _ => {}
                        }
                    }
                    acc.push(&ev);
                }
            }
        }
        let complete = !interrupted && error.is_none();
        let usage = acc.usage;
        let account = acc.account.take();
        let stop_reason = acc.stop_reason.clone().unwrap_or_default();
        let message = (complete || acc.has_content()).then(|| acc.finish(complete));
        Ok(Response { message, usage, account, stop_reason, interrupted, error })
    }

    async fn run(
        &self,
        tools: &[Arc<dyn Tool>],
        session_tools: &Arc<SessionTools>,
        conv: &mut Conversation,
        pending: &mut (dyn FnMut() -> Vec<PendingInput> + Send),
        reminders: Option<&ToolReminders<'_>>,
    ) -> anyhow::Result<TurnEndReason> {
        let by_name: HashMap<String, Arc<dyn Tool>> = self.specs.iter().map(|s| s.name.clone()).zip(tools.iter().cloned()).collect();
        let mut compacted_for_overflow = false;
        let mut stop_hook_runs = 0;
        let mut reported = self.reported_estimate(conv);
        // Recompacting unchanged history cannot help, even if the fixed
        // prefix alone exceeds the threshold.
        let mut compacted_estimate = None;
        let mut reminder = None;

        for _ in 0..MAX_STEPS {
            if self.cancel.is_cancelled() {
                return Ok(TurnEndReason::Interrupted);
            }
            // Derive before compaction can remove the tool exchange. Carry the
            // advisory through overflow recovery, but not into later requests.
            if reminder.is_none() {
                reminder = reminders.and_then(|render| latest_tool_reminder(&conv.history, render));
            }
            let reminder_tokens = reminder.as_ref().map_or(0, |text| (2 * (text.chars().count() as u64 + 48)).div_ceil(7));
            let mut estimate = self.estimate_request(&conv.history, &self.specs) + reminder_tokens;
            let mut next = next_tokens(conv.context_tokens, estimate, reported);
            if let Some(after) = self.trim_for_request(conv, estimate, next, reminder_tokens).await? {
                reported = None;
                estimate = after;
                next = after;
            }
            if self.cfg.context_window > 0
                && next as f64 > self.cfg.context_window as f64 * compact_share(self.cfg.context_window, self.cfg.compact_at)
                && compacted_estimate != Some(estimate)
            {
                if self.summary_room() < u64::from(COMPACT_MIN_OUTPUT) {
                    if next > self.cfg.context_window {
                        return self.fail(self.no_summary_room()).await;
                    }
                } else {
                    if let Err(e) = self.compact(conv, &self.cfg.compact.why_full, "", true, next).await? {
                        return self.fail(format!("compaction failed: {e}")).await;
                    }
                    reported = None;
                    compacted_estimate = Some(self.estimate_request(&conv.history, &self.specs) + reminder_tokens);
                }
            }

            let mut messages = conv.history.clone();
            if let Some(text) = &reminder {
                match messages.last_mut() {
                    Some(last) if last.role == Role::User => last.blocks.push(Block::Text(text.clone())),
                    _ => messages.push(Message::user_text(text.clone())),
                }
            }
            let req = self.request(messages, self.cfg.max_output_tokens);
            let resp = match self.call_retrying(&req).await {
                Ok(r) => r,
                Err(LlmError::ContextOverflow(_)) if !compacted_for_overflow => {
                    compacted_for_overflow = true;
                    if let Err(e) = self.compact(conv, &self.cfg.compact.why_full, "", true, u64::MAX).await? {
                        return self.fail(format!("context window exceeded and compaction failed: {e}")).await;
                    }
                    reported = None;
                    compacted_estimate = Some(self.estimate_request(&conv.history, &self.specs) + reminder_tokens);
                    continue;
                }
                Err(e) => return self.fail(format!("model request failed: {e}")).await,
            };
            reminder = None;

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
            self.record_usage(&resp.usage, resp.account.as_ref()).await?;
            if resp.usage != Default::default() {
                let u = resp.usage;
                conv.context_tokens = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens;
                reported = (conv.context_tokens > 0).then(|| self.estimate_request(&conv.history, &self.specs) + reminder_tokens);
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
                    .map(|_| {
                        let output = ToolOutput::err("Not run: your response hit the output token limit, so this call may be incomplete. Do the work in smaller steps.");
                        ToolRun { output, elapsed: 0, failure_output_len: None }
                    })
                    .collect()
            } else {
                run_tools(&calls, &by_name, session_tools, self.sink, self.cancel).await
            };
            // Images tools return go after all the results, each labelled.
            let mut images: Vec<Block> = Vec::new();
            for ((id, name, input), ToolRun { output, elapsed, failure_output_len }) in calls.into_iter().zip(results) {
                images.extend(crate::history::tool_image_blocks(&name, &output.images));
                self.sink
                    .append(session_event::Kind::ToolResult(ToolResult {
                        call_id: id.clone(),
                        tool: name.clone(),
                        output: output.content.clone(),
                        is_error: output.is_error,
                        truncated: output.truncated,
                        duration_ms: elapsed,
                        images: output
                            .images
                            .iter()
                            .enumerate()
                            .map(|(i, (mime, data))| c6_proto::Attachment {
                                name: format!("image-{}", i + 1),
                                mime: mime.clone(),
                                data: data.clone(),
                            })
                            .collect(),
                    }))
                    .await?;
                if let Some(len) = failure_output_len {
                    self.sink.tool_failed(&name, &input, &output.content[..len]);
                }
                user_blocks.push(Block::ToolResult { id, content: output.content, is_error: output.is_error });
            }
            user_blocks.extend(images);

            // Interrupted: what was sent meanwhile starts the next turn rather
            // than going into this one, which ends here (Send now relies on it).
            if !self.cancel.is_cancelled() {
                for input in pending() {
                    self.sink.append(input.event()).await?;
                    user_blocks.extend(input.blocks());
                }
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
            crate::history::forget_old_tool_images(&mut conv.history);
        }
        self.fail(format!("stopped after {MAX_STEPS} model calls in one turn")).await
    }

    async fn record_usage(&self, u: &c6_llm::Usage, account: Option<&c6_llm::ModelAccount>) -> anyhow::Result<()> {
        if *u == Default::default() {
            return Ok(());
        }
        self.sink
            .append(session_event::Kind::Usage(Usage {
                model: self.cfg.model.clone(),
                keep_warm: false,
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_tokens: u.cache_read_tokens,
                cache_write_tokens: u.cache_write_tokens,
                reasoning_tokens: u.reasoning_tokens,
                context_window: 0,
                resolved_connection_id: account.map(|a| a.resolved_connection_id.clone()).unwrap_or_default(),
                resolved_connection_name: account.map(|a| a.resolved_connection_name.clone()).unwrap_or_default(),
                alias_connection_name: account.map(|a| a.alias_connection_name.clone()).unwrap_or_default(),
            }))
            .await?;
        Ok(())
    }

    async fn compact_idle(&self, conv: &mut Conversation, prompt: &str) -> anyhow::Result<Result<(), String>> {
        if self.cancel.is_cancelled() {
            return Ok(Err("interrupted".into()));
        }
        let Some(tail) = idle_tail(&conv.history) else {
            return Ok(Ok(()));
        };
        let before = self.estimate_request(&conv.history, &self.specs);
        let to_request = |summary: &str| self.estimate_request(&[Message::user_text(compacted_context(summary))], &self.specs);
        let base = to_request("");
        // Idle compaction is optional: only send a warm request if there's
        // room to write a useful summary and keep the full tail below both
        // the normal compaction limit and the size of the original request.
        let room = self.summary_room().min(before.saturating_sub(base).saturating_sub(1));
        let tail_tokens = to_request(&tail).saturating_sub(base);
        let output = room.saturating_sub(tail_tokens).min(u64::from(COMPACT_MAX_OUTPUT)) as u32;
        if output < COMPACT_MIN_OUTPUT {
            return Ok(Ok(()));
        }
        let estimate = next_tokens(conv.context_tokens, before, self.reported_estimate(conv));
        let mut summary = match self.summarize_history(&conv.history, prompt, output, estimate).await? {
            Ok(summary) => summary,
            // Don't recast the prefix as a cold digest for optional maintenance.
            Err(NoSummary::Overflow(_)) => return Ok(Ok(())),
            Err(NoSummary::Answered(e) | NoSummary::Failed(e)) => return Ok(Err(e)),
        };
        if self.cancel.is_cancelled() {
            return Ok(Err("interrupted".into()));
        }
        summary.push_str(&tail);
        let after = to_request(&summary);
        if after.saturating_sub(base) > room {
            return Ok(Ok(()));
        }
        let result = self.compacted(conv, summary).await?;
        self.context_notice("Compacted while idle", format!("Estimated context: {before} → {after} tokens. Rewind is available.")).await?;
        Ok(result)
    }

    /// Append only the summarization instruction to retain the session's
    /// cache key, system message, tools, and original conversation prefix.
    async fn summarize_history(
        &self,
        history: &[Message],
        prompt: &str,
        output: u32,
        estimated_tokens: u64,
    ) -> anyhow::Result<Result<String, NoSummary>> {
        let prompt_tokens = self.estimate_request(&[Message::user_text(prompt)], &[]).saturating_sub(self.estimate_request(&[], &[]));
        let summary_tokens = estimated_tokens.saturating_add(prompt_tokens);
        if self.cfg.context_window > 0 && summary_tokens >= self.cfg.context_window {
            return Ok(Err(NoSummary::Overflow("the conversation is too large to summarize in full".into())));
        }
        let mut msgs = history.to_vec();
        match msgs.last_mut() {
            Some(m) if m.role == Role::User => m.blocks.push(Block::Text(prompt.to_owned())),
            _ => msgs.push(Message::user_text(prompt)),
        }
        self.summarize(self.request(msgs, Some(output)), summary_tokens).await
    }

    /// Simple compaction: the model summarizes the conversation (same model,
    /// system and tools, so the cached prefix is reused) and the summary
    /// replaces the whole history. Nothing older is replayed, so no earlier
    /// reasoning blocks are sent again. If the model gives no complete summary
    /// (often a tool call: it carries on with the work), it is asked once
    /// more with the conversation as text and no tools. With `fallback`
    /// (automatic compaction), a mechanical digest stands in when that fails
    /// too, so the session is not stuck over its window. The outer `Result`
    /// is a journal failure; the inner one a compaction failure. The size
    /// passed in uses reported usage plus growth when available. Both calls
    /// reserve room for output and for restoring the normal prefix afterwards.
    async fn compact(
        &self,
        conv: &mut Conversation,
        why: &str,
        focus: &str,
        fallback: bool,
        estimated_tokens: u64,
    ) -> anyhow::Result<Result<(), String>> {
        if self.cancel.is_cancelled() {
            return Ok(Err("interrupted".into()));
        }
        let room = self.summary_room();
        let output = room.min(u64::from(COMPACT_MAX_OUTPUT)) as u32;
        if output < COMPACT_MIN_OUTPUT {
            return Ok(Err(self.no_summary_room()));
        }
        let mut prompt = format!("{why} {}", self.cfg.compact.instructions);
        if !focus.trim().is_empty() {
            prompt.push_str(&format!("\n\nThe requested focus for the summary is: {}", focus.trim()));
        }
        let first = match self.summarize_history(&conv.history, &prompt, output, estimated_tokens).await? {
            Ok(summary) => return self.compacted(conv, summary).await,
            // A request that failed may fail again; an interrupt is final.
            Err(NoSummary::Failed(e)) => return Ok(Err(e)),
            Err(NoSummary::Answered(e) | NoSummary::Overflow(e)) => e,
        };
        warn!(error = %first, "compaction: asking with a bounded digest and no tools");

        let text = |digest: &str| {
            format!(
                "Below is a conversation with you, an agent working on a task, as text. Inputs may be from humans, automation or other sessions; preserve their recorded attribution.\n\n{digest}\n\n---\n\n{prompt}"
            )
        };
        let mut req = self.request(vec![Message::user_text(text(""))], Some(output));
        req.tools.clear();
        req.web_search = false;
        let fixed = self.estimate_request(&req.messages, &req.tools);
        let digest_room = if self.cfg.context_window > 0 {
            self.cfg.context_window.saturating_sub(fixed).saturating_sub(u64::from(output)).saturating_sub(COMPACT_FRAMING)
        } else {
            u64::MAX
        };
        let budget = self.digest_budget(2).min((digest_room.saturating_mul(7) / 2) as usize);
        req.messages = vec![Message::user_text(text(&fit_text(&messages_digest(&conv.history, budget), digest_room)))];
        let digest_tokens = self.estimate_request(&req.messages, &req.tools);
        let (second, answered) = match self.summarize(req, digest_tokens).await? {
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

        self.mechanical_digest(conv, &why, room).await
    }

    async fn mechanical_digest(&self, conv: &mut Conversation, why: &str, room: u64) -> anyhow::Result<Result<(), String>> {
        warn!(error = %why, "compaction: falling back to a mechanical digest");
        let notice = format!(
            "(The model could not write a summary: {why}. This is a mechanical digest instead: the most recent steps, \
             with long tool input and output shortened, and before them input text with its recorded attribution. Re-read files rather \
             than trusting shortened output.)\n\n"
        );
        let fixed = serde_json::to_string(&notice).unwrap_or_default().chars().count() as u64;
        let budget = self.digest_budget(1).min((room.saturating_mul(7) / 2).saturating_sub(fixed) as usize);
        let digest = format!("{notice}{}", messages_digest(&conv.history, budget));
        self.compacted(conv, fit_text(&digest, room)).await
    }

    /// Leave room for a complete summary in this call and beside the normal
    /// prefix afterwards. A response cut off at the output limit is unusable.
    async fn summarize(&self, mut req: ModelRequest, estimated_tokens: u64) -> anyhow::Result<Result<String, NoSummary>> {
        if self.cfg.context_window > 0 {
            let room = self.cfg.context_window.saturating_sub(estimated_tokens).saturating_sub(COMPACT_FRAMING);
            let output = room.min(u64::from(req.max_output_tokens.unwrap_or(COMPACT_MAX_OUTPUT))) as u32;
            if output < COMPACT_MIN_OUTPUT {
                return Ok(Err(NoSummary::Overflow("the summary prompt leaves too little room for a useful summary".into())));
            }
            req.max_output_tokens = Some(output);
        }
        let resp = match self.call(&req, false).await {
            Ok(r) => r,
            Err(e @ LlmError::ContextOverflow(_)) => return Ok(Err(NoSummary::Overflow(e.to_string()))),
            Err(e) => return Ok(Err(NoSummary::Failed(e.to_string()))),
        };
        self.record_usage(&resp.usage, resp.account.as_ref()).await?;
        if resp.interrupted {
            return Ok(Err(NoSummary::Failed("interrupted".into())));
        }
        if let Some(e) = resp.error {
            return Ok(Err(NoSummary::Failed(e.to_string())));
        }
        if resp.stop_reason == stop::MAX_TOKENS {
            return Ok(Err(NoSummary::Answered("the summary hit the output token limit before finishing".into())));
        }
        let blocks = resp.message.map(|m| m.blocks).unwrap_or_default();
        let summary = blocks.iter().filter_map(Block::text).collect::<Vec<_>>().join("\n");
        if !summary.trim().is_empty() {
            let tokens = self
                .estimate_request(&[Message::user_text(summary.clone())], &[])
                .saturating_sub(self.estimate_request(&[Message::user_text("")], &[]));
            if tokens > self.summary_room() {
                return Ok(Err(NoSummary::Answered("the summary does not fit beside the system prompt and tools".into())));
            }
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

    /// [`Self::call`], tried again when it fails in a way that may pass:
    /// what a failed attempt streamed is dropped, and the same request goes
    /// again. The session says it is waiting meanwhile.
    async fn call_retrying(&self, req: &ModelRequest) -> Result<Response, LlmError> {
        let mut attempt = 0;
        loop {
            let r = self.call(req, true).await;
            let failed = match &r {
                Ok(resp) if !resp.interrupted => resp.error.as_ref().filter(|e| transient(e)),
                Err(e) if transient(e) => Some(e),
                _ => None,
            };
            let Some(e) = failed else {
                if attempt > 0 {
                    self.sink.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true, status: String::new() }));
                }
                return r;
            };
            if attempt >= MODEL_RETRIES {
                return r;
            }
            attempt += 1;
            let wait = std::time::Duration::from_secs((2u64 << (attempt - 1)).min(MODEL_RETRY_MAX_WAIT_SECS));
            let why = e.to_string();
            warn!(error = %why, attempt, ?wait, "model call failed; trying again");
            if let Ok(resp) = &r
                && let Err(e) = self.record_usage(&resp.usage, resp.account.as_ref()).await
            {
                warn!("recording usage: {e:#}");
            }
            let status =
                format!("The model call failed ({}); trying again in {}s ({attempt} of {MODEL_RETRIES})", clip(&why, 200), wait.as_secs());
            self.sink.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true, status }));
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    return Ok(Response { message: None, usage: Default::default(), account: None, stop_reason: String::new(), interrupted: true, error: None });
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }
}

/// Why a summarizing call gave no summary.
enum NoSummary {
    /// The model answered, but with no usable summary.
    Answered(String),
    /// The conversation did not fit.
    Overflow(String),
    /// The request failed or was interrupted.
    Failed(String),
}

/// Take the last three user messages containing instructions, not the
/// result-only user-role messages used by the model protocol. Adjacent user
/// inputs can share one model message; keep all of them at a boundary.
/// Neither a previous summary nor a runtime handoff is a user turn: until
/// there are three fresh turns, leave their retained transcripts alone.
fn idle_tail(history: &[Message]) -> Option<String> {
    let compacted = compacted_context("");
    let compacted_header = compacted.split_once("\n\n")?.0;
    let mut start = None;
    let mut users = 0;
    for (i, msg) in history.iter().enumerate().rev().filter(|(_, msg)| msg.role == Role::User) {
        if i == 0
            && msg
                .blocks
                .first()
                .and_then(Block::text)
                .is_some_and(|s| s.starts_with(compacted_header) || s.starts_with("<carbon6-handoff "))
        {
            return None;
        }
        if msg.blocks.iter().enumerate().any(|(j, block)| {
            block.text().is_some_and(|s| {
                let tool_image_label = s.starts_with("[image from ")
                    && (matches!(msg.blocks.get(j + 1), Some(Block::Image { .. })) || s.ends_with("] (an earlier image, no longer shown)"));
                !tool_image_label
            })
        }) {
            start = Some(i);
            users += 1;
            if users == IDLE_KEEP_USER_TURNS {
                break;
            }
        }
    }
    let mut start = start?;
    // A new instruction can be delivered in the same message as results
    // from the preceding turn. Include their corresponding assistant call.
    if history[start].blocks.iter().any(|b| matches!(b, Block::ToolResult { .. })) {
        start = start.checked_sub(1)?;
    }
    let mut tail = IDLE_TAIL_HEADER.to_owned();
    let mut calls = HashSet::new();
    for msg in &history[start..] {
        for block in &msg.blocks {
            let (heading, text) = match block {
                Block::Text(text) | Block::AnnotatedText { text, .. } => (format!("{:?}", msg.role), text),
                Block::ToolUse { id, name, input_json } => {
                    if !calls.insert(id.as_str()) {
                        return None;
                    }
                    (format!("Tool call {name:?} id={id:?}"), input_json)
                }
                Block::ToolResult { id, content, is_error } => {
                    if !calls.remove(id.as_str()) {
                        return None;
                    }
                    (format!("Tool result id={id:?} error={is_error}"), content)
                }
                // A summary's text cannot replay a native attachment. Don't
                // compact rather than silently dropping it from this tail.
                Block::Image { .. } | Block::Document { .. } => return None,
                // Reasoning and provider-specific payloads are not transcript text.
                _ => continue,
            };
            // Frame each payload by its UTF-8 length; don't escape, trim or
            // normalize even quotes, line breaks and trailing whitespace.
            let _ = write!(tail, "\n\n### {heading} ({} bytes)\n{text}", text.len());
        }
    }
    calls.is_empty().then_some(tail)
}

/// Reported usage plus estimated growth; without a report, estimate it all.
fn next_tokens(context_tokens: u64, estimate: u64, reported: Option<u64>) -> u64 {
    reported.map_or(estimate, |before| context_tokens.saturating_add(estimate.saturating_sub(before)))
}

/// Fit digest text within the request estimator's budget, including JSON escaping.
fn fit_text(text: &str, tokens: u64) -> String {
    let mut room = tokens.saturating_mul(7) / 2;
    text.chars()
        .take_while(|c| {
            let chars = match c {
                '"' | '\\' | '\n' | '\r' | '\t' => 2,
                c if c.is_control() => 6,
                _ => 1,
            };
            if chars > room {
                return false;
            }
            room -= chars;
            true
        })
        .collect()
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

fn latest_tool_reminder(history: &[Message], render: &ToolReminders<'_>) -> Option<String> {
    let [.., assistant, user] = history else { return None };
    if assistant.role != Role::Assistant || user.role != Role::User {
        return None;
    }
    let calls: HashMap<_, _> = assistant.tool_uses().map(|(id, name, input)| (id, (name, input))).collect();
    let evidence: Vec<_> = user
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::ToolResult { id, content, .. } => {
                let &(name, input) = calls.get(id.as_str())?;
                Some((name, input, content.as_str()))
            }
            _ => None,
        })
        .collect();
    if evidence.is_empty() { None } else { render(&evidence) }
}

struct ToolRun {
    output: ToolOutput,
    elapsed: u64,
    // The failed invocation's output, without appended PostToolUse feedback.
    // Synthetic errors for calls that never ran do not have this marker.
    failure_output_len: Option<usize>,
}

/// Runs calls in order: consecutive shared (read-only) calls concurrently,
/// exclusive ones alone. Returns results in call order.
async fn run_tools(
    calls: &[(String, String, String)],
    by_name: &HashMap<String, Arc<dyn Tool>>,
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    cancel: &CancellationToken,
) -> Vec<ToolRun> {
    let mut out = Vec::with_capacity(calls.len());
    let deferrals = if cancel.is_cancelled() { Vec::new() } else { sink.defer_tools(calls, &|name| by_name.contains_key(name)) };
    let mut i = 0;
    while i < calls.len() {
        let shared = |c: &(String, String, String)| by_name.get(&c.1).is_some_and(|t| t.concurrency() == Concurrency::Shared);
        let end = if shared(&calls[i]) { i + calls[i..].iter().take_while(|c| shared(c)).count() } else { i + 1 };
        let batch = calls[i..end].iter().enumerate().map(|(offset, (id, name, input))| {
            let advisory = deferrals.get(i + offset).and_then(Option::as_ref);
            async move {
                if let Some(advisory) = advisory {
                    // Not an invocation or failure: policies and tool code have
                    // not run. The synthetic reply closes the provider's call ID.
                    ToolRun { output: ToolOutput::ok(advisory.clone()), elapsed: 0, failure_output_len: None }
                } else {
                    run_one(id, name, input, by_name, session_tools, sink, cancel).await
                }
            }
        });
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
) -> ToolRun {
    let started = Instant::now();
    let mut failure_output_len = None;
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
                    if output.is_error && !cancel.is_cancelled() {
                        failure_output_len = Some(output.content.len());
                    }
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
    ToolRun { output, elapsed: started.elapsed().as_millis() as u64, failure_output_len }
}

fn sink_output(sink: &Arc<dyn EventSink>, call_id: String) -> c6_tools::OutputSink {
    let sink = sink.clone();
    Arc::new(move |text: &str| {
        sink.ephemeral(ephemeral_event::Kind::ToolOutput(ToolOutputDelta { call_id: call_id.clone(), text: text.to_owned() }))
    })
}

/// At most `max` characters of `s`.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Journal(Mutex<Vec<session_event::Kind>>);

    #[async_trait::async_trait]
    impl EventSink for Journal {
        async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
            let mut events = self.0.lock().unwrap();
            events.push(kind);
            Ok(events.len() as u64)
        }

        fn ephemeral(&self, _: ephemeral_event::Kind) {}
    }

    #[derive(Default)]
    struct Model(Mutex<Vec<ModelRequest>>);

    #[async_trait::async_trait]
    impl ModelClient for Model {
        async fn stream(&self, req: &ModelRequest) -> Result<c6_llm::EventStream, LlmError> {
            self.0.lock().unwrap().push(req.clone());
            let events =
                [ModelEvent::TextDelta("Plan only; I omitted all quotes.".into()), ModelEvent::Done { stop_reason: "stop".into() }];
            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    fn cfg() -> TurnConfig {
        TurnConfig {
            model: "mock".into(),
            system_prompt: "Same system prefix".into(),
            max_output_tokens: None,
            cache_key: "same-session".into(),
            effort: None,
            context_window: 128_000,
            web_search: true,
            compact: CompactPrompts::default(),
            compact_at: 0,
            trim_at: 0,
        }
    }

    fn assistant(block: Block) -> Message {
        Message { role: Role::Assistant, blocks: vec![block] }
    }

    fn tool_result(id: &str, text: &str) -> Block {
        Block::ToolResult { id: id.into(), content: text.into(), is_error: false }
    }

    #[tokio::test]
    async fn idle_compaction_persists_verbatim_tail_with_matched_tools() {
        let cfg = cfg();
        let model = Model::default();
        let journal = Arc::new(Journal::default());
        let sink: Arc<dyn EventSink> = journal.clone();
        let tools = c6_tools::default_tools();
        let final_instruction = "Message sent by an automation, not typed by the user:\nQuote \"ε\" exactly.\n\tEnd.  ";
        let raw_args = "{ \n  \"path\": \"file\" }";
        let raw_output = "begin\n\"result\"\n  ";
        let history = vec![
            Message::user_text("Old directive that should not be attached"),
            assistant(Block::Text("older words ".repeat(500))),
            assistant(Block::ToolUse { id: "call-old".into(), name: "Read".into(), input_json: raw_args.into() }),
            // The third-most-recent user turn shares the tool result; the
            // matching call belongs to the previous turn.
            Message { role: Role::User, blocks: vec![tool_result("call-old", raw_output), Block::Text("Third instruction".into())] },
            assistant(Block::Text("Third answer".into())),
            Message::user_text("Second instruction"),
            assistant(Block::Text("Second answer".into())),
            Message::user_text(final_instruction),
            assistant(Block::AnnotatedText { text: "Verbatim last answer.".into(), annotations_json: "[]".into() }),
        ];
        let mut conv = Conversation { history: history.clone(), context_tokens: 0 };

        let reason = idle_compact_turn(5, &cfg, &model, &tools, &sink, &mut conv, "", &CancellationToken::new()).await.unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
        let requests = model.0.lock().unwrap();
        let [request] = requests.as_slice() else { panic!("expected one warm summary request") };
        assert_eq!(&request.messages[..history.len()], history.as_slice());
        assert_eq!(request.messages.last().unwrap(), &Message::user_text(IDLE_COMPACT_PROMPT));
        assert_eq!(request.model, cfg.model);
        assert_eq!(request.system, cfg.system_prompt);
        assert_eq!(request.cache_key.as_deref(), Some(cfg.cache_key.as_str()));
        assert!(request.web_search);
        assert!(!tools.is_empty());
        assert_eq!(request.tools.len(), tools.len());

        let events = journal.0.lock().unwrap();
        let [
            session_event::Kind::TurnStarted(_),
            session_event::Kind::Compaction(compacted),
            session_event::Kind::Notice(notice),
            session_event::Kind::TurnEnded(_),
        ] = events.as_slice()
        else {
            panic!("expected durable compaction and quiet notice")
        };
        let summary = &compacted.summary;
        assert!(summary.starts_with("Plan only; I omitted all quotes."));
        for exact in [
            raw_args,
            raw_output,
            "Tool call \"Read\" id=\"call-old\"",
            "Tool result id=\"call-old\"",
            "Third instruction",
            "Third answer",
            "Second instruction",
            "Second answer",
            final_instruction,
            "Verbatim last answer.",
        ] {
            assert!(summary.contains(exact), "forgot {exact:?}: {summary}");
        }
        assert!(!summary.contains("Old directive"));
        assert_eq!(conv.history, [Message::user_text(compacted_context(summary))]);
        assert_eq!(notice.title, "Compacted while idle");
        assert!(notice.quiet && notice.message.contains(" → ") && notice.message.contains("Rewind"));
    }

    #[tokio::test]
    async fn idle_compaction_does_not_drop_a_tail_that_cannot_fit() {
        let cfg = TurnConfig { context_window: 1_000, ..cfg() };
        let model = Model::default();
        let journal = Arc::new(Journal::default());
        let sink: Arc<dyn EventSink> = journal.clone();
        let history = vec![Message::user_text("First"), assistant(Block::Text("answer".into())), Message::user_text("Keep ".repeat(1_000))];
        let mut conv = Conversation { history: history.clone(), context_tokens: 2_000 };

        idle_compact_turn(1, &cfg, &model, &[], &sink, &mut conv, "", &CancellationToken::new()).await.unwrap();

        assert_eq!(conv.history, history);
        assert_eq!(conv.context_tokens, 2_000);
        assert!(model.0.lock().unwrap().is_empty());
        assert!(matches!(journal.0.lock().unwrap().as_slice(), [session_event::Kind::TurnStarted(_), session_event::Kind::TurnEnded(_)]));
    }

    #[tokio::test]
    async fn cold_resume_trims_before_the_first_request_but_keeps_new_user_input() {
        let cfg = TurnConfig { trim_at: 1_000, ..cfg() };
        let model = Model::default();
        let journal = Arc::new(Journal::default());
        let sink: Arc<dyn EventSink> = journal.clone();
        let mut history = vec![Message::user_text("Read all the files")];
        for n in 0..30 {
            let id = format!("call-{n}");
            history.push(assistant(Block::ToolUse { id: id.clone(), name: "Read".into(), input_json: "{}".into() }));
            history.push(Message { role: Role::User, blocks: vec![tool_result(&id, &"x".repeat(500))] });
        }
        let new_instruction = Block::Text("Resume, but do not edit anything.\n  This is the new request.".into());
        history.last_mut().unwrap().blocks.push(new_instruction.clone());
        let mut conv = Conversation { history, context_tokens: 4_500 };
        let dir = tempfile::tempdir().unwrap();
        let session_tools = Arc::new(SessionTools::new(dir.path().to_owned()));

        run_turn(1, &cfg, &model, &[], &session_tools, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();

        let requests = model.0.lock().unwrap();
        let [request] = requests.as_slice() else { panic!("no extra request is needed to trim") };
        assert_eq!(request.messages.last().unwrap().blocks.last(), Some(&new_instruction));
        assert!(request.messages.iter().flat_map(|m| &m.blocks).any(|b| {
            matches!(b, Block::ToolResult { id, content, .. } if id == "call-5" && content.contains("Earlier output of Read removed"))
        }));
        assert!(journal.0.lock().unwrap().iter().any(|event| {
            matches!(event, session_event::Kind::Notice(notice) if notice.quiet && notice.title == "Trimmed old tool output")
        }));
    }

    #[test]
    fn large_windows_compact_sooner_unless_set() {
        assert_eq!(compact_share(16_384, 0), COMPACT_AT);
        assert_eq!(compact_share(272_000, 0), COMPACT_AT_LARGE);
        assert_eq!(compact_share(LARGE_WINDOW, 0), COMPACT_AT_LARGE);
        assert_eq!(compact_share(272_000, 50), 0.5);
        assert_eq!(compact_share(16_384, 90), 0.9);
        assert_eq!(compact_share(16_384, 250), COMPACT_AT, "out of range: the default");
    }
}
