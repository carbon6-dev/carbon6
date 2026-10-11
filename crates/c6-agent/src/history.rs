//! Rebuilds the model conversation from a session's event log.

use std::collections::HashSet;

use c6_llm::{Block, Message, Role};
use c6_proto::{SessionEvent, content_block, session_event};

/// Converts durable events to conversation messages, repairing tool calls
/// that never got a result (crash or interrupt) so providers accept them.
pub fn history_from_events(events: &[SessionEvent]) -> Vec<Message> {
    let mut msgs: Vec<Message> = Vec::new();
    for ev in &effective_events(events) {
        match &ev.kind {
            // `/compact` itself is not part of the conversation.
            Some(session_event::Kind::User(u)) if u.command == "compact" || !u.blocked.is_empty() => {}
            Some(session_event::Kind::User(u)) => {
                for b in user_blocks(model_text(u), &u.attachments) {
                    push_user_block(&mut msgs, b);
                }
            }
            Some(session_event::Kind::Assistant(a)) => {
                let blocks: Vec<Block> = a.blocks.iter().filter_map(block_from_proto).collect();
                if !blocks.is_empty() {
                    msgs.push(Message { role: Role::Assistant, blocks });
                }
            }
            Some(session_event::Kind::ToolResult(r)) => {
                push_user_block(&mut msgs, Block::ToolResult { id: r.call_id.clone(), content: r.output.clone(), is_error: r.is_error })
            }
            // Simple compaction: the summary replaces everything before it.
            Some(session_event::Kind::Compaction(c)) => msgs = vec![Message::user_text(compacted_context(&c.summary))],
            _ => {}
        }
    }
    repair_tool_results(msgs)
}

/// A user message as model blocks: its text, then each attachment (images
/// as images; anything else as a file in text).
/// What the model got for a user message: its text, or the expansion of
/// the slash command it invoked.
pub fn model_text(u: &c6_proto::UserMessage) -> &str {
    if u.prompt.is_empty() { &u.text } else { &u.prompt }
}

pub fn user_blocks(text: &str, attachments: &[c6_proto::Attachment]) -> Vec<Block> {
    let mut out = vec![Block::Text(text.to_owned())];
    for a in attachments {
        if a.mime.starts_with("image/") {
            out.push(Block::Image { media_type: a.mime.clone(), data: a.data.clone() });
        } else {
            out.push(Block::Text(format!("\n\n<file name=\"{}\">\n{}\n</file>", a.name, String::from_utf8_lossy(&a.data))));
        }
    }
    out
}

/// A user message as plain text, for backends without images (Claude
/// Code): files inline, images named.
pub fn user_text(text: &str, attachments: &[c6_proto::Attachment]) -> String {
    let mut out = text.to_owned();
    for a in attachments {
        if a.mime.starts_with("image/") {
            out.push_str(&format!("\n\n[image {} not shown: this backend takes text only]", a.name));
        } else {
            out.push_str(&format!("\n\n<file name=\"{}\">\n{}\n</file>", a.name, String::from_utf8_lossy(&a.data)));
        }
    }
    out
}

/// The events that still count: a `Rewound` drops the events of its own
/// session from its `to_seq` on. Each session's part of the list starts at
/// its `Started` event (a side chat's list begins with its parents' events,
/// whose seqs overlap its own).
pub fn effective_events(events: &[SessionEvent]) -> Vec<SessionEvent> {
    let mut kept: Vec<SessionEvent> = Vec::with_capacity(events.len());
    let mut segment = 0;
    for ev in events {
        match &ev.kind {
            Some(session_event::Kind::Started(_)) => {
                segment = kept.len();
                kept.push(ev.clone());
            }
            Some(session_event::Kind::Rewound(r)) => {
                let cut = kept[segment..].iter().position(|e| e.seq >= r.to_seq).map_or(kept.len(), |p| segment + p);
                kept.truncate(cut);
            }
            _ => kept.push(ev.clone()),
        }
    }
    kept
}

/// Removes provider-bound blocks (signed or encrypted reasoning, provider
/// tool blocks) from assistant events, so a conversation produced by one
/// model can be continued by another.
pub fn portable_events(events: &[SessionEvent]) -> Vec<SessionEvent> {
    events
        .iter()
        .cloned()
        .map(|mut ev| {
            if let Some(session_event::Kind::Assistant(a)) = &mut ev.kind {
                a.blocks.retain(|b| matches!(b.block, Some(content_block::Block::Text(_)) | Some(content_block::Block::ToolUse(_))));
            }
            ev
        })
        .collect()
}

/// `portable_events` for an in-memory conversation.
pub fn portable_messages(msgs: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for mut m in msgs {
        if m.role == Role::Assistant {
            m.blocks.retain(|b| matches!(b, Block::Text(_) | Block::ToolUse { .. }));
            if m.blocks.is_empty() {
                continue;
            }
        }
        match out.last_mut() {
            // Dropping an assistant message can leave two user messages in a row.
            Some(last) if last.role == m.role && m.role == Role::User => last.blocks.extend(m.blocks),
            _ => out.push(m),
        }
    }
    out
}

/// A plain-text rendering of a conversation, for backends that cannot be
/// given a history (Claude Code). Long tool output is shortened.
pub fn transcript_text(events: &[SessionEvent]) -> String {
    fn clip(s: &str, max: usize) -> String {
        if s.chars().count() <= max { s.to_owned() } else { format!("{}… (shortened)", s.chars().take(max).collect::<String>()) }
    }
    let mut out = String::new();
    for ev in events {
        match &ev.kind {
            Some(session_event::Kind::User(u)) if !u.blocked.is_empty() => {}
            Some(session_event::Kind::User(u)) => out.push_str(&format!("\n## User\n{}\n", user_text(model_text(u), &u.attachments))),
            Some(session_event::Kind::Assistant(a)) => {
                for b in &a.blocks {
                    match &b.block {
                        Some(content_block::Block::Text(t)) => out.push_str(&format!("\n## Assistant\n{t}\n")),
                        Some(content_block::Block::ToolUse(u)) => {
                            out.push_str(&format!("\n[tool call {}: {}]\n", u.name, clip(&u.input_json, 2000)))
                        }
                        _ => {}
                    }
                }
            }
            Some(session_event::Kind::ToolResult(r)) => out.push_str(&format!("[tool result]\n{}\n", clip(&r.output, 2000))),
            Some(session_event::Kind::Compaction(c)) => out = format!("## Summary of the earlier conversation\n{}\n", c.summary),
            _ => {}
        }
    }
    out
}

/// The user message that stands in for a compacted conversation.
pub fn compacted_context(summary: &str) -> String {
    format!(
        "This session continues from an earlier conversation that was compacted to fit the context window. \
         Summary of the work so far:\n\n{summary}\n\nContinue from where it left off."
    )
}

pub(crate) fn block_from_proto(b: &c6_proto::ContentBlock) -> Option<Block> {
    match b.block.as_ref()? {
        content_block::Block::Text(t) => Some(Block::Text(t.clone())),
        content_block::Block::Thinking(t) => Some(Block::Thinking(t.clone())),
        content_block::Block::ToolUse(u) => {
            Some(Block::ToolUse { id: u.id.clone(), name: u.name.clone(), input_json: u.input_json.clone() })
        }
        content_block::Block::SignedThinking(t) => Some(Block::SignedThinking { text: t.text.clone(), signature: t.signature.clone() }),
        content_block::Block::RedactedThinking(d) => Some(Block::RedactedThinking(d.clone())),
        content_block::Block::Reasoning(r) => {
            Some(Block::Reasoning { id: r.id.clone(), encrypted: r.encrypted_content.clone(), summary: r.summary.clone() })
        }
        content_block::Block::Opaque(o) => {
            Some(Block::Opaque { provider: o.provider.clone(), json: o.json.clone(), label: o.label.clone() })
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
        Block::SignedThinking { text, signature } => {
            content_block::Block::SignedThinking(c6_proto::SignedThinking { text: text.clone(), signature: signature.clone() })
        }
        Block::RedactedThinking(d) => content_block::Block::RedactedThinking(d.clone()),
        Block::Reasoning { id, encrypted, summary } => content_block::Block::Reasoning(c6_proto::Reasoning {
            id: id.clone(),
            encrypted_content: encrypted.clone(),
            summary: summary.clone(),
        }),
        Block::Opaque { provider, json, label } => {
            content_block::Block::Opaque(c6_proto::Opaque { provider: provider.clone(), json: json.clone(), label: label.clone() })
        }
        // Images come from users and are kept as attachments.
        Block::ToolResult { .. } | Block::Image { .. } => return None,
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
            ev(session_event::Kind::User(UserMessage { command_id: "c1".into(), text: "go".into(), ..Default::default() })),
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
            ev(session_event::Kind::User(UserMessage { command_id: "c2".into(), text: "again".into(), ..Default::default() })),
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

    #[test]
    fn rewinds_drop_their_own_session_events() {
        let at = |seq: u64, kind: session_event::Kind| SessionEvent { seq, ts_ms: 0, kind: Some(kind) };
        let user = |t: &str| session_event::Kind::User(UserMessage { command_id: String::new(), text: t.into(), ..Default::default() });
        let started = || session_event::Kind::Started(Default::default());
        let rewound = |to: u64| session_event::Kind::Rewound(c6_proto::Rewound { to_seq: to, ..Default::default() });
        // A side chat: its parent's events (seed), then its own.
        let events = vec![
            at(1, started()),
            at(2, user("parent one")),
            at(3, user("parent two")),
            at(1, started()),
            at(2, user("mine one")),
            at(3, user("mine two")),
            at(4, rewound(3)),
            at(5, user("mine again")),
        ];
        let texts: Vec<String> = history_from_events(&events)
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["parent one", "parent two", "mine one", "mine again"]);
    }
}
