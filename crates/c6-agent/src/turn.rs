//! One agent turn: model call → tools → model call … until the model stops.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use c6_llm::{Accumulator, Block, Message, ModelClient, ModelEvent, ModelRequest, Role, ToolSpec};
use c6_proto::{
    AssistantMessage, ErrorEvent, TextDelta, ToolOutputDelta, ToolResult, TurnEndReason, TurnEnded, TurnStarted, Usage, UserMessage,
    ephemeral_event, session_event,
};
use c6_tools::{Concurrency, SessionTools, Tool, ToolCtx, ToolOutput};
use futures::StreamExt;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::history::block_to_proto;
use crate::{EventSink, PendingInput};

/// Guards against runaway tool loops.
const MAX_STEPS: usize = 500;

pub struct TurnConfig {
    pub model: String,
    pub system_prompt: String,
    pub max_output_tokens: Option<u32>,
    pub cache_key: String,
}

/// Runs one turn and records it. `history` is updated in place. `pending`
/// is drained at each model-request boundary so input sent mid-turn reaches
/// the model as soon as possible.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    turn: u64,
    cfg: &TurnConfig,
    client: &dyn ModelClient,
    tools: &[Arc<dyn Tool>],
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    history: &mut Vec<Message>,
    pending: &mut (dyn FnMut() -> Vec<PendingInput> + Send),
    cancel: &CancellationToken,
) -> anyhow::Result<TurnEndReason> {
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;
    let reason = turn_loop(turn, cfg, client, tools, session_tools, sink, history, pending, cancel).await?;
    sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: reason as i32 })).await?;
    Ok(reason)
}

#[allow(clippy::too_many_arguments)]
async fn turn_loop(
    turn: u64,
    cfg: &TurnConfig,
    client: &dyn ModelClient,
    tools: &[Arc<dyn Tool>],
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    history: &mut Vec<Message>,
    pending: &mut (dyn FnMut() -> Vec<PendingInput> + Send),
    cancel: &CancellationToken,
) -> anyhow::Result<TurnEndReason> {
    let specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();
    let by_name: HashMap<String, Arc<dyn Tool>> = specs.iter().map(|s| s.name.clone()).zip(tools.iter().cloned()).collect();

    for _ in 0..MAX_STEPS {
        if cancel.is_cancelled() {
            return Ok(TurnEndReason::Interrupted);
        }
        let req = ModelRequest {
            model: cfg.model.clone(),
            system: cfg.system_prompt.clone(),
            messages: history.clone(),
            tools: specs.clone(),
            max_output_tokens: cfg.max_output_tokens,
            cache_key: Some(cfg.cache_key.clone()),
        };

        let stream = tokio::select! {
            _ = cancel.cancelled() => return Ok(TurnEndReason::Interrupted),
            r = client.stream(&req) => r,
        };
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => return fail(sink.as_ref(), format!("model request failed: {e}")).await,
        };

        let mut acc = Accumulator::default();
        let mut stream_error = None;
        let mut interrupted = false;
        loop {
            let ev = tokio::select! {
                _ = cancel.cancelled() => { interrupted = true; break; }
                ev = stream.next() => ev,
            };
            match ev {
                None => break,
                Some(Err(e)) => {
                    stream_error = Some(e);
                    break;
                }
                Some(Ok(ev)) => {
                    match &ev {
                        ModelEvent::TextDelta(t) => sink.ephemeral(ephemeral_event::Kind::Text(TextDelta { turn, text: t.clone() })),
                        ModelEvent::ThinkingDelta(t) => {
                            sink.ephemeral(ephemeral_event::Kind::Thinking(TextDelta { turn, text: t.clone() }))
                        }
                        _ => {}
                    }
                    acc.push(&ev);
                }
            }
        }
        drop(stream);

        let complete = !interrupted && stream_error.is_none();
        let usage = acc.usage;
        let stop_reason = acc.stop_reason.clone().unwrap_or_default();
        if complete || acc.has_content() {
            let msg = acc.finish(complete);
            sink.append(session_event::Kind::Assistant(AssistantMessage {
                turn,
                blocks: msg.blocks.iter().filter_map(block_to_proto).collect(),
                stop_reason: if complete { stop_reason } else { "interrupted".into() },
            }))
            .await?;
            if !msg.blocks.is_empty() {
                history.push(msg);
            }
        }
        if usage != Default::default() {
            sink.append(session_event::Kind::Usage(Usage {
                model: cfg.model.clone(),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            }))
            .await?;
        }
        if interrupted {
            return Ok(TurnEndReason::Interrupted);
        }
        if let Some(e) = stream_error {
            return fail(sink.as_ref(), format!("model stream failed: {e}")).await;
        }

        let calls: Vec<(String, String, String)> = history
            .last()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| m.tool_uses().map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned())).collect())
            .unwrap_or_default();

        let mut user_blocks: Vec<Block> = Vec::new();
        if !calls.is_empty() {
            for (id, name, output, elapsed) in run_tools(&calls, &by_name, session_tools, sink, cancel).await {
                sink.append(session_event::Kind::ToolResult(ToolResult {
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
        }

        let inputs = pending();
        for input in &inputs {
            sink.append(session_event::Kind::User(UserMessage { command_id: input.command_id.clone(), text: input.text.clone() })).await?;
            user_blocks.push(Block::Text(input.text.clone()));
        }
        if user_blocks.is_empty() {
            return Ok(TurnEndReason::Completed);
        }
        history.push(Message { role: Role::User, blocks: user_blocks });
        if cancel.is_cancelled() {
            return Ok(TurnEndReason::Interrupted);
        }
    }
    fail(sink.as_ref(), format!("stopped after {MAX_STEPS} model calls in one turn")).await
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
                let ctx = ToolCtx { session: session_tools.clone(), cancel: cancel.clone(), output: stream_to };
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
