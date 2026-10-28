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
                context_window: s.context_window,
                effort: m.effort,
            }
        }
        Kind::User(u) => api::EventKind::User {
            command_id: u.command_id.clone(),
            text: u.text.clone(),
            attachments: u
                .attachments
                .iter()
                .map(|a| api::AttachmentInfo { name: a.name.clone(), mime: a.mime.clone(), size: a.data.len() as u64 })
                .collect(),
            command: u.command.clone(),
            prompt: u.prompt.clone(),
            blocked: u.blocked.clone(),
            delegated_by: u.delegated_by.clone(),
            wake: u.wake,
        },
        Kind::SubagentSpawned(s) => api::EventKind::SubagentSpawned {
            call_id: s.call_id.clone(),
            child_session_id: s.child_session_id.clone(),
            agent: s.agent.clone(),
            description: s.description.clone(),
            background_task_id: s.background_task_id.clone(),
            connection_id: s.model.as_ref().map(|m| m.connection_id.clone()).unwrap_or_default(),
            model: s.model.as_ref().map(|m| m.model.clone()).unwrap_or_default(),
            effort: s.model.as_ref().map(|m| m.effort.clone()).unwrap_or_default(),
        },
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
            images: r.images.len(),
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
        Kind::Checkpoint(c) => api::EventKind::Checkpoint { error: c.error.clone() },
        Kind::Approval(a) => api::EventKind::Approval {
            call_id: a.call_id.clone(),
            state: a.state.clone(),
            tool: a.tool.clone(),
            input: a.input.clone(),
            policy: a.policy.clone(),
            message: a.message.clone(),
        },
        Kind::Question(q) => api::EventKind::Question {
            call_id: q.call_id.clone(),
            title: q.title.clone(),
            fields: serde_json::from_str(&q.fields_json).unwrap_or_default(),
            state: q.state.clone(),
            answer: serde_json::from_str(&q.answer_json).unwrap_or_default(),
        },
        Kind::Notice(n) => api::EventKind::Notice { title: n.title.clone(), message: n.message.clone(), priority: n.priority.clone() },
        Kind::Memory(m) => api::EventKind::Memory { op: m.op.clone(), id: m.id.clone(), scope: m.scope.clone(), text: m.text.clone() },
        Kind::Output(o) => {
            api::EventKind::Output { name: o.name.clone(), title: o.title.clone(), mime: o.mime.clone(), size: o.data.len() as u64 }
        }
        Kind::BackgroundTask(t) => api::EventKind::BackgroundTask {
            task_id: t.task_id.clone(),
            kind: t.kind.clone(),
            description: t.description.clone(),
            state: t.state.clone(),
            exit_code: t.exit_code,
            elapsed_ms: t.elapsed_ms,
            notify: t.notify,
        },
        Kind::Rewound(r) => {
            api::EventKind::Rewound { to_seq: r.to_seq, restored_files: r.restored_files, restore_error: r.restore_error.clone() }
        }
        Kind::ModelSwitched(m) => {
            let spec = m.model.clone().unwrap_or_default();
            api::EventKind::ModelSwitched {
                connection_id: spec.connection_id,
                model: spec.model,
                effort: spec.effort,
                context_window: m.context_window,
            }
        }
        Kind::SubagentModelSet(m) => {
            let spec = m.model.clone().unwrap_or_default();
            api::EventKind::SubagentModelSet { connection_id: spec.connection_id, model: spec.model, effort: spec.effort }
        }
    };
    Some(api::Event { seq: ev.seq, ts: ev.ts_ms, kind })
}

pub fn ephemeral(kind: &ephemeral_event::Kind) -> api::StreamMsg {
    match kind {
        ephemeral_event::Kind::Text(d) => api::StreamMsg::Text { turn: d.turn, text: d.text.clone() },
        ephemeral_event::Kind::Thinking(d) => api::StreamMsg::Thinking { turn: d.turn, text: d.text.clone() },
        ephemeral_event::Kind::ToolOutput(d) => api::StreamMsg::ToolOutput { call_id: d.call_id.clone(), text: d.text.clone() },
        ephemeral_event::Kind::Activity(a) => api::StreamMsg::Activity { running: a.running, status: a.status.clone() },
        ephemeral_event::Kind::Secret(c) => {
            api::StreamMsg::Secret { call_id: c.call_id.clone(), title: c.title.clone(), value: c.value.clone(), note: c.note.clone() }
        }
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
        parent_id: r.parent_id.clone(),
        parent_call_id: r.parent_call_id.clone(),
        fork_seq: r.fork_seq.map(|s| s as u64),
        created_at: r.created_at,
        updated_at: r.updated_at,
        last_seq: r.last_seq as u64,
        labels: r.user_labels(),
        auto_labels: r.all_labels().split_off(r.user_labels().len()),
        ssh: Some(r.ssh_target.clone()).filter(|t| !t.is_empty()),
        awaiting_approval: r.awaiting > 0,
        realm: r.realm.clone(),
        agent: r.agent.clone(),
        end_reason: match r.end_reason.as_str() {
            "" => None,
            "completed" => Some(api::TurnEndReason::Completed),
            "interrupted" => Some(api::TurnEndReason::Interrupted),
            "worker_restarted" => Some(api::TurnEndReason::WorkerRestarted),
            _ => Some(api::TurnEndReason::Error),
        },
        root_id: r.root_id.clone(),
        subagents_running: 0,
        observed: r.observed,
        instance: r.instance.clone(),
        colour: r.colour.clone(),
        subagent_connection_id: r.subagent_connection_id.clone(),
        subagent_model: r.subagent_model.clone(),
        subagent_effort: r.subagent_effort.clone(),
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
