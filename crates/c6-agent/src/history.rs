//! Rebuilds the model conversation from a session's event log.

use std::collections::HashSet;

use c6_llm::{Block, Message, Role};
use c6_proto::{SessionEvent, content_block, session_event};

/// Converts durable events to conversation messages, repairing tool calls
/// that never got a result (crash or interrupt) so providers accept them.
pub fn history_from_events(events: &[SessionEvent]) -> Vec<Message> {
    let mut msgs: Vec<Message> = Vec::new();
    for ev in events {
        match &ev.kind {
            Some(session_event::Kind::User(u)) => push_user_block(&mut msgs, Block::Text(u.text.clone())),
            Some(session_event::Kind::Assistant(a)) => {
                let blocks: Vec<Block> = a.blocks.iter().filter_map(block_from_proto).collect();
                if !blocks.is_empty() {
                    msgs.push(Message { role: Role::Assistant, blocks });
                }
            }
            Some(session_event::Kind::ToolResult(r)) => {
                push_user_block(&mut msgs, Block::ToolResult { id: r.call_id.clone(), content: r.output.clone(), is_error: r.is_error })
            }
            _ => {}
        }
    }
    repair_tool_results(msgs)
}

pub(crate) fn block_from_proto(b: &c6_proto::ContentBlock) -> Option<Block> {
    match b.block.as_ref()? {
        content_block::Block::Text(t) => Some(Block::Text(t.clone())),
        content_block::Block::Thinking(t) => Some(Block::Thinking(t.clone())),
        content_block::Block::ToolUse(u) => {
            Some(Block::ToolUse { id: u.id.clone(), name: u.name.clone(), input_json: u.input_json.clone() })
        }
    }
}

pub(crate) fn block_to_proto(b: &Block) -> Option<c6_proto::ContentBlock> {
    let block = match b {
        Block::Text(t) => content_block::Block::Text(t.clone()),
        Block::Thinking(t) => content_block::Block::Thinking(t.clone()),
        Block::ToolUse { id, name, input_json } => {
            content_block::Block::ToolUse(c6_proto::ToolUse { id: id.clone(), name: name.clone(), input_json: input_json.clone() })
        }
        Block::ToolResult { .. } => return None,
    };
    Some(c6_proto::ContentBlock { block: Some(block) })
}

fn push_user_block(msgs: &mut Vec<Message>, block: Block) {
    match msgs.last_mut() {
        Some(m) if m.role == Role::User => m.blocks.push(block),
        _ => msgs.push(Message { role: Role::User, blocks: vec![block] }),
    }
}

/// Every tool call must be answered in the message right after it, before
/// any user text.
fn repair_tool_results(msgs: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    let mut iter = msgs.into_iter().peekable();
    while let Some(m) = iter.next() {
        let call_ids: Vec<String> = m.tool_uses().map(|(id, _, _)| id.to_owned()).collect();
        out.push(m);
        if call_ids.is_empty() {
            continue;
        }
        let next = match iter.peek() {
            Some(n) if n.role == Role::User => iter.next().unwrap(),
            _ => Message { role: Role::User, blocks: Vec::new() },
        };
        let answered: HashSet<&str> =
            next.blocks.iter().filter_map(|b| if let Block::ToolResult { id, .. } = b { Some(id.as_str()) } else { None }).collect();
        let mut results: Vec<Block> = Vec::new();
        for id in &call_ids {
            match next.blocks.iter().find(|b| matches!(b, Block::ToolResult { id: rid, .. } if rid == id)) {
                Some(b) => results.push(b.clone()),
                None if !answered.contains(id.as_str()) => results.push(Block::ToolResult {
                    id: id.clone(),
                    content: "Tool call did not complete (interrupted).".into(),
                    is_error: true,
                }),
                None => {}
            }
        }
        // Results first, then anything else (user text) in original order.
        results.extend(next.blocks.into_iter().filter(|b| !matches!(b, Block::ToolResult { .. })));
        out.push(Message { role: Role::User, blocks: results });
    }
    out
}

#[cfg(test)]
mod tests {
    use c6_proto::{AssistantMessage, ContentBlock, ToolResult, ToolUse, UserMessage};

    use super::*;

    fn ev(kind: session_event::Kind) -> SessionEvent {
        SessionEvent { seq: 0, ts_ms: 0, kind: Some(kind) }
    }

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock { block: Some(content_block::Block::ToolUse(ToolUse { id: id.into(), name: "bash".into(), input_json: "{}".into() })) }
    }

    #[test]
    fn repairs_missing_results_and_keeps_order() {
        let events = vec![
            ev(session_event::Kind::User(UserMessage { command_id: "c1".into(), text: "go".into() })),
            ev(session_event::Kind::Assistant(AssistantMessage {
                turn: 1,
                blocks: vec![tool_use("a"), tool_use("b")],
                stop_reason: "tool_calls".into(),
            })),
            ev(session_event::Kind::ToolResult(ToolResult {
                call_id: "a".into(),
                tool: "bash".into(),
                output: "ok".into(),
                ..Default::default()
            })),
            // Crash before b's result; then the user spoke again.
            ev(session_event::Kind::User(UserMessage { command_id: "c2".into(), text: "again".into() })),
        ];
        let msgs = history_from_events(&events);
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs[2].blocks,
            vec![
                Block::ToolResult { id: "a".into(), content: "ok".into(), is_error: false },
                Block::ToolResult { id: "b".into(), content: "Tool call did not complete (interrupted).".into(), is_error: true },
                Block::Text("again".into()),
            ]
        );
    }
}
