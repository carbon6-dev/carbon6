//! Wire protocol (protobuf) → HTTP API (JSON) types.

use c6_api as api;
use c6_proto::{SessionEvent, content_block, ephemeral_event, session_event};

use crate::store::{CommandRow, SessionRow};
use c6_core::WorkerId;

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
            from_session_id: u.from_session_id.clone(),
            from_title: u.from_title.clone(),
            origin: (if !u.from_session_id.is_empty() {
                Some(api::MessageOrigin::Session)
            } else if let Some(origin) = &u.api_origin {
                Some(api::MessageOrigin::Api { token_name: origin.token_name.clone() })
            } else if u.human_origin {
                Some(api::MessageOrigin::Human)
            } else if u.task_card_origin == c6_proto::TaskCardOrigin::Start as i32 {
                Some(api::MessageOrigin::TaskCard)
            } else if u.task_card_origin == c6_proto::TaskCardOrigin::Answer as i32 {
                Some(api::MessageOrigin::TaskCardAnswer)
            } else {
                u.origin_recorded.then_some(api::MessageOrigin::Unknown)
            })
            .map(Box::new),
            task_notice: u.task_notice.as_ref().map(|n| {
                Box::new(api::TaskNotice {
                    kind: n.kind.clone(),
                    title: n.title.clone(),
                    session_id: n.session_id.clone(),
                    status: n.status.clone(),
                    markdown: n.markdown.clone(),
                })
            }),
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
            remote: s.remote.as_ref().map(|r| {
                Box::new(api::RemoteSubagent {
                    template: r.template.clone(),
                    worker_id: r.worker_id.clone(),
                    worker_name: r.worker_name.clone(),
                    target: r.target.clone(),
                    cwd: r.cwd.clone(),
                })
            }),
        },
        // A receipt: the report itself shows as the call's result.
        Kind::SubagentReported(_) => return None,
        Kind::TurnStarted(t) => api::EventKind::TurnStarted { turn: t.turn, context_window: t.context_window },
        Kind::Assistant(a) => api::EventKind::Assistant {
            turn: a.turn,
            blocks: a
                .blocks
                .iter()
                .filter_map(|b| {
                    Some(match b.block.as_ref()? {
                        content_block::Block::Text(t) => {
                            api::ContentBlock::Text { text: t.clone(), citations: api::citations::annotations(&b.annotations_json) }
                        }
                        content_block::Block::Thinking(t) => api::ContentBlock::Thinking { text: t.clone() },
                        content_block::Block::Narration(t) => api::ContentBlock::Narration { text: t.clone() },
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
                        content_block::Block::Opaque(o) => api::ContentBlock::ServerTool { label: o.label.clone() },
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
            context_window: u.context_window,
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
        Kind::Notice(n) => api::EventKind::Notice {
            title: api::citations::clean(&n.title).into_owned(),
            message: api::citations::clean(&n.message).into_owned(),
            priority: n.priority.clone(),
        },
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
            native_type: t.native_type.clone(),
            foreground: t.foreground,
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
                handoff: m.handoff.as_ref().map(|h| api::Handoff {
                    tool_profile: h.tool_profile.clone(),
                    tools: h.tools.clone(),
                    system_prompt: h.system_prompt.clone(),
                }),
            }
        }
        Kind::SubagentModelSet(m) => {
            let spec = m.model.clone().unwrap_or_default();
            api::EventKind::SubagentModelSet { connection_id: spec.connection_id, model: spec.model, effort: spec.effort }
        }
        // Shown in the queue (as with the agent), not the transcript.
        Kind::InputDelivered(_) => return None,
        Kind::NoticeQueued(n) => api::EventKind::NoticeQueued {
            command_id: n.command_id.clone(),
            text: n.text.clone(),
            command: n.command.clone(),
            from_title: n.from_title.clone(),
            deliver_at_ms: n.deliver_at_ms,
        },
    };
    Some(api::Event { seq: ev.seq, ts: ev.ts_ms, kind })
}

/// What a session's viewers see of it; `None` for what is the head's only
/// (a plan's limits).
pub fn ephemeral(kind: &ephemeral_event::Kind) -> Option<api::StreamMsg> {
    Some(match kind {
        ephemeral_event::Kind::Text(d) => api::StreamMsg::Text { turn: d.turn, text: d.text.clone(), seq: d.seq },
        ephemeral_event::Kind::Thinking(d) => api::StreamMsg::Thinking { turn: d.turn, text: d.text.clone(), seq: d.seq },
        ephemeral_event::Kind::ToolOutput(d) => api::StreamMsg::ToolOutput { call_id: d.call_id.clone(), text: d.text.clone() },
        ephemeral_event::Kind::Activity(a) => api::StreamMsg::Activity { running: a.running, status: a.status.clone() },
        ephemeral_event::Kind::Secret(c) => {
            api::StreamMsg::Secret { call_id: c.call_id.clone(), title: c.title.clone(), value: c.value.clone(), note: c.note.clone() }
        }
        ephemeral_event::Kind::RateLimit(_) => return None,
    })
}

/// A session as the API shows it. `head`: the head's own worker (its
/// sessions run in the head: the assistant's).
pub fn session(r: &SessionRow, head: Option<WorkerId>) -> api::Session {
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
        archived: r.archived != 0,
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
        side_chats_running: 0,
        tasks_running: 0,
        servers_running: 0,
        observed: r.observed,
        remote_template: r.remote_template.clone(),
        on_head: head.is_some_and(|h| h.to_string() == r.worker_id),
        instance: r.instance.clone(),
        colour: r.colour.clone(),
        work: None,
        subagent_connection_id: r.subagent_connection_id.clone(),
        subagent_model: r.subagent_model.clone(),
        subagent_effort: r.subagent_effort.clone(),
    }
}

pub fn session_with_activity(r: &SessionRow, activity: Option<&crate::store::SessionActivity>, head: Option<WorkerId>) -> api::Session {
    let activity = activity.copied().unwrap_or_default();
    api::Session {
        subagents_running: activity.subagents_running,
        side_chats_running: activity.side_chats_running,
        tasks_running: activity.tasks_running,
        servers_running: activity.servers_running,
        ..session(r, head)
    }
}

pub fn queued(c: &CommandRow) -> api::QueuedInput {
    api::QueuedInput {
        command_id: c.id.clone(),
        text: c.text.clone().unwrap_or_default(),
        created_at: c.created_at,
        rejected: (c.state == "rejected").then(|| c.error.clone().unwrap_or_default()),
        with_agent: c.with_agent != 0 && c.state == "pending",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_origins_are_additive_and_missing_legacy_provenance_stays_absent() {
        let cases = [
            (c6_proto::UserMessage::default(), None),
            (c6_proto::UserMessage { origin_recorded: true, ..Default::default() }, Some(api::MessageOrigin::Unknown)),
            (
                c6_proto::UserMessage { task_card_origin: c6_proto::TaskCardOrigin::Start as i32, ..Default::default() },
                Some(api::MessageOrigin::TaskCard),
            ),
            (
                c6_proto::UserMessage { task_card_origin: c6_proto::TaskCardOrigin::Answer as i32, ..Default::default() },
                Some(api::MessageOrigin::TaskCardAnswer),
            ),
            (
                c6_proto::UserMessage { command: "card-answer".into(), origin_recorded: true, ..Default::default() },
                Some(api::MessageOrigin::Unknown),
            ),
            (c6_proto::UserMessage { human_origin: true, ..Default::default() }, Some(api::MessageOrigin::Human)),
            (
                c6_proto::UserMessage {
                    api_origin: Some(c6_proto::ApiMessageOrigin { token_name: "c6-cli-zmbp".into() }),
                    human_origin: true,
                    ..Default::default()
                },
                Some(api::MessageOrigin::Api { token_name: "c6-cli-zmbp".into() }),
            ),
            (
                c6_proto::UserMessage { from_session_id: "ops".into(), human_origin: true, ..Default::default() },
                Some(api::MessageOrigin::Session),
            ),
        ];
        for (input, expected) in cases {
            let shown = event(&SessionEvent { kind: Some(session_event::Kind::User(input)), ..Default::default() }).unwrap();
            let api::EventKind::User { origin, .. } = &shown.kind else { panic!("user event") };
            assert_eq!(origin.as_deref(), expected.as_ref());
            let json = serde_json::to_value(&shown).unwrap();
            assert_eq!(json.get("origin").is_some(), expected.is_some());
            assert_eq!(serde_json::from_value::<api::Event>(json).unwrap(), shown);
        }
    }

    #[test]
    fn task_notice_metadata_reaches_the_browser_without_changing_the_agent_text() {
        let raw = "Sent via the API.\n<subagent id=\"ops\">Report: exact raw text</subagent>";
        let metadata = c6_proto::TaskNotice {
            kind: "subagent".into(),
            title: "zmbp ops".into(),
            session_id: "ops".into(),
            status: "completed".into(),
            markdown: "## Checks passed\n\nAll **done**.".into(),
        };
        let input = SessionEvent {
            seq: 2,
            ts_ms: 3,
            kind: Some(session_event::Kind::User(c6_proto::UserMessage {
                text: raw.into(),
                command: "task-notification".into(),
                task_notice: Some(metadata.clone()),
                ..Default::default()
            })),
        };
        let shown = event(&input).unwrap();
        let api::EventKind::User { text, task_notice: Some(n), .. } = shown.kind else { panic!("structured task notice") };
        assert_eq!(text, raw);
        assert_eq!(
            (n.kind, n.title, n.session_id, n.status, n.markdown),
            (metadata.kind, metadata.title, metadata.session_id, metadata.status, metadata.markdown)
        );
    }

    #[test]
    fn a_card_answer_is_an_ordinary_user_event() {
        let id = "test";
        let text = "Not now: task card #185 \"Cleanup\".";
        let input = SessionEvent {
            seq: 17,
            ts_ms: 1234,
            kind: Some(session_event::Kind::User(c6_proto::UserMessage {
                command_id: id.into(),
                text: text.into(),
                command: "card-answer".into(),
                ..Default::default()
            })),
        };
        let shown = event(&input).unwrap();
        assert_eq!((shown.seq, shown.ts), (17, 1234));
        assert!(matches!(shown.kind, api::EventKind::User { command_id, text: s, command, prompt, wake: false, .. }
            if command_id == id && s == text && command == "card-answer" && prompt.is_empty()));
    }

    #[test]
    fn narration_reaches_the_api_without_reclassifying_old_text_or_thinking() {
        let saved = SessionEvent {
            kind: Some(session_event::Kind::Assistant(c6_proto::AssistantMessage {
                turn: 1112,
                blocks: vec![
                    c6_proto::ContentBlock { block: Some(content_block::Block::Narration("Summary".into())), ..Default::default() },
                    c6_proto::ContentBlock { block: Some(content_block::Block::Text("Old reply".into())), ..Default::default() },
                    c6_proto::ContentBlock { block: Some(content_block::Block::Thinking("Reasoning".into())), ..Default::default() },
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        let shown = event(&saved).unwrap();
        let json = serde_json::to_value(&shown).unwrap();
        assert_eq!(json["blocks"][0], serde_json::json!({"type": "narration", "text": "Summary"}));
        let decoded: api::Event = serde_json::from_value(json).unwrap();
        let api::EventKind::Assistant { blocks, .. } = decoded.kind else { panic!("not an assistant event") };
        assert_eq!(
            blocks,
            vec![
                api::ContentBlock::Narration { text: "Summary".into() },
                api::ContentBlock::Text { text: "Old reply".into(), citations: vec![] },
                api::ContentBlock::Thinking { text: "Reasoning".into() },
            ]
        );
    }

    #[test]
    fn text_annotations_reach_the_browser_while_raw_text_and_server_tool_icons_are_kept() {
        let text = "Fact. \u{e200}cite\u{e202}turn1search1\u{e201}";
        let saved = SessionEvent {
            kind: Some(session_event::Kind::Assistant(c6_proto::AssistantMessage {
                blocks: vec![
                    c6_proto::ContentBlock {
                        block: Some(content_block::Block::Opaque(c6_proto::Opaque {
                            provider: "openai".into(),
                            json: "{}".into(),
                            label: "web_search: a query".into(),
                        })),
                        ..Default::default()
                    },
                    c6_proto::ContentBlock {
                        block: Some(content_block::Block::Text(text.into())),
                        annotations_json:
                            r#"[{"type":"url_citation","url":"https://source.test","title":"Source","start_index":6,"end_index":33}]"#
                                .into(),
                    },
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        let event = event(&saved).unwrap();
        let api::EventKind::Assistant { blocks, .. } = event.kind else { panic!("not an assistant event") };
        assert!(matches!(&blocks[0], api::ContentBlock::ServerTool { label } if label == "web_search: a query"));
        let api::ContentBlock::Text { text: shown, citations } = &blocks[1] else { panic!("missing text") };
        assert_eq!(shown, text);
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].url, "https://source.test");
        assert_eq!(citations[0].title, "Source");
    }
}
