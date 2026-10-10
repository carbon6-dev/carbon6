//! Wire protocol (protobuf) → HTTP API (JSON) types.

use c6_api as api;
use c6_proto::{SessionEvent, content_block, ephemeral_event, session_event};

use crate::store::{CommandRow, SessionRow};

pub fn event(ev: &SessionEvent) -> Option<api::Event> {
    use session_event::Kind;
    let kind = match ev.kind.as_ref()? {
        Kind::Started(s) => {
            let m = s.model.clone().unwrap_or_default();
            api::EventKind::Started {
                connection_id: m.connection_id,
                model: m.model,
                cwd: s.cwd.clone(),
                system_prompt: s.system_prompt.clone(),
                tools: s.tools.clone(),
                worktree_branch: s.worktree_branch.clone(),
                repo_root: s.repo_root.clone(),
            }
        }
        Kind::User(u) => api::EventKind::User { command_id: u.command_id.clone(), text: u.text.clone() },
        Kind::TurnStarted(t) => api::EventKind::TurnStarted { turn: t.turn },
        Kind::Assistant(a) => api::EventKind::Assistant {
            turn: a.turn,
            blocks: a
                .blocks
                .iter()
                .filter_map(|b| {
                    Some(match b.block.as_ref()? {
                        content_block::Block::Text(t) => api::ContentBlock::Text { text: t.clone() },
                        content_block::Block::Thinking(t) => api::ContentBlock::Thinking { text: t.clone() },
                        content_block::Block::ToolUse(u) => {
                            api::ContentBlock::ToolUse { id: u.id.clone(), name: u.name.clone(), input_json: u.input_json.clone() }
                        }
                        // Only the readable part goes to the UI; signatures and
                        // encrypted reasoning stay in the log.
                        content_block::Block::SignedThinking(t) if !t.text.is_empty() => {
                            api::ContentBlock::Thinking { text: t.text.clone() }
                        }
                        content_block::Block::Reasoning(r) if !r.summary.is_empty() => {
                            api::ContentBlock::Thinking { text: r.summary.clone() }
                        }
                        _ => return None,
                    })
                })
                .collect(),
            stop_reason: a.stop_reason.clone(),
        },
        Kind::ToolResult(r) => api::EventKind::ToolResult {
            call_id: r.call_id.clone(),
            tool: r.tool.clone(),
            output: r.output.clone(),
            is_error: r.is_error,
            truncated: r.truncated,
            duration_ms: r.duration_ms,
        },
        Kind::Usage(u) => api::EventKind::Usage {
            model: u.model.clone(),
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            reasoning_tokens: u.reasoning_tokens,
        },
        Kind::TurnEnded(t) => api::EventKind::TurnEnded {
            turn: t.turn,
            reason: match c6_proto::TurnEndReason::try_from(t.reason) {
                Ok(c6_proto::TurnEndReason::Completed) => api::TurnEndReason::Completed,
                Ok(c6_proto::TurnEndReason::Interrupted) => api::TurnEndReason::Interrupted,
                Ok(c6_proto::TurnEndReason::Error) => api::TurnEndReason::Error,
                Ok(c6_proto::TurnEndReason::WorkerRestarted) => api::TurnEndReason::WorkerRestarted,
                _ => api::TurnEndReason::Unknown,
            },
        },
        Kind::Error(e) => api::EventKind::Error { message: e.message.clone() },
        Kind::Compaction(c) => api::EventKind::Compaction { summary: c.summary.clone(), tokens_before: c.tokens_before },
    };
    Some(api::Event { seq: ev.seq, ts: ev.ts_ms, kind })
}

pub fn ephemeral(kind: &ephemeral_event::Kind) -> api::StreamMsg {
    match kind {
        ephemeral_event::Kind::Text(d) => api::StreamMsg::Text { turn: d.turn, text: d.text.clone() },
        ephemeral_event::Kind::Thinking(d) => api::StreamMsg::Thinking { turn: d.turn, text: d.text.clone() },
        ephemeral_event::Kind::ToolOutput(d) => api::StreamMsg::ToolOutput { call_id: d.call_id.clone(), text: d.text.clone() },
        ephemeral_event::Kind::Activity(a) => api::StreamMsg::Activity { running: a.running },
    }
}

pub fn session(r: &SessionRow) -> api::Session {
    api::Session {
        id: r.id.clone(),
        worker_id: r.worker_id.parse().unwrap_or_default(),
        title: r.title.clone(),
        cwd: r.cwd.clone(),
        connection_id: r.connection_id.clone(),
        model: r.model.clone(),
        status: match r.status.as_str() {
            "running" => api::SessionStatus::Running,
            "idle" => api::SessionStatus::Idle,
            "failed" => api::SessionStatus::Failed,
            _ => api::SessionStatus::Starting,
        },
        error: r.error.clone(),
        effort: r.effort.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        last_seq: r.last_seq as u64,
    }
}

pub fn queued(c: &CommandRow) -> api::QueuedInput {
    api::QueuedInput {
        command_id: c.id.clone(),
        text: c.text.clone().unwrap_or_default(),
        created_at: c.created_at,
        rejected: (c.state == "rejected").then(|| c.error.clone().unwrap_or_default()),
    }
}
