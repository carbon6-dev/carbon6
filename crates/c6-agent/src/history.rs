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
                push_user_block(&mut msgs, Block::ToolResult { id: r.call_id.clone(), content: r.output.clone(), is_error: r.is_error });
                let images: Vec<(String, Vec<u8>)> = r.images.iter().map(|a| (a.mime.clone(), a.data.clone())).collect();
                for b in tool_image_blocks(&r.tool, &images) {
                    push_user_block(&mut msgs, b);
                }
            }
            // Simple compaction: the summary replaces everything before it.
            Some(session_event::Kind::Compaction(c)) => msgs = vec![Message::user_text(compacted_context(&c.summary))],
            _ => {}
        }
    }
    let mut msgs = repair_tool_results(msgs);
    forget_old_tool_images(&mut msgs);
    msgs
}

/// The latest reported request size since compaction or a runtime handoff.
pub fn context_tokens_from_events(events: &[SessionEvent]) -> u64 {
    effective_events(events)
        .iter()
        .rev()
        .take_while(|e| match &e.kind {
            Some(session_event::Kind::Compaction(_)) => false,
            Some(session_event::Kind::ModelSwitched(m)) => m.handoff.is_none(),
            _ => true,
        })
        .find_map(|e| match &e.kind {
            Some(session_event::Kind::Usage(u)) => Some(u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens),
            _ => None,
        })
        .unwrap_or(0)
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

/// How much transcript a handoff carries at most (characters); older
/// steps beyond it are left out, and the text says so.
pub const HANDOFF_BUDGET: usize = 120_000;

/// The context a session hands to the other runtime (Claude Code, or
/// carbon6's own loop) when it switches between them: a transcript of the
/// conversation so far, rebuilt from the log (rewinds and compactions
/// applied), clearly marked as such. Nothing native carries over: the
/// earlier runtime's reasoning, provider blocks and tool state stay
/// behind. `from` names the runtime it leaves. Within `budget` characters,
/// keeping the most recent steps.
pub fn handoff_context(events: &[SessionEvent], from: &str, budget: usize) -> String {
    let mut transcript = transcript_text(&effective_events(events));
    let total = transcript.chars().count();
    if total > budget {
        // Keep the tail, from the first step that starts within it.
        let skip = total - budget;
        let start = transcript.char_indices().nth(skip).map_or(transcript.len(), |(i, _)| i);
        let start = transcript[start..].find("\n## ").map_or(start, |p| start + p);
        transcript =
            format!("[{} earlier characters of the transcript left out]\n{}", transcript[..start].chars().count(), &transcript[start..]);
    }
    let transcript = transcript.trim();
    let transcript = if transcript.is_empty() { "(nothing yet)" } else { transcript };
    format!(
        "<carbon6-handoff from=\"{from}\">\n\
         This session was handed over to you from {from} at this point. You did not take part in the conversation below and have \
         none of its native state: it is a transcript carbon6 rebuilt from the session's log, for context only.\n\
         - Tool calls in it were made by the earlier runtime with its own tools; their results are as recorded (long ones shortened).\n\
         - Its hidden reasoning, provider-side state and any unfinished tool calls did not carry over.\n\
         - The working directory holds everything done so far: check the files rather than trusting the transcript.\n\
         <transcript>\n{transcript}\n</transcript>\n\
         </carbon6-handoff>"
    )
}

/// A plain-text digest of an in-memory conversation within about `budget`
/// characters: the most recent steps in full (long tool input and output
/// shortened), and before them only what the user asked.
pub(crate) fn messages_digest(msgs: &[Message], budget: usize) -> String {
    fn clip(s: &str, max: usize) -> String {
        if s.chars().count() <= max { s.to_owned() } else { format!("{}… (shortened)", s.chars().take(max).collect::<String>()) }
    }
    // One chunk per message, and whether it is something the user wrote.
    let chunks: Vec<(String, bool)> = msgs
        .iter()
        .map(|m| {
            let mut out = String::new();
            let mut typed = false;
            for b in &m.blocks {
                match (m.role, b) {
                    (Role::User, Block::Text(t)) => {
                        typed = true;
                        out.push_str(&format!("\n## User\n{}\n", clip(t, 4000)));
                    }
                    (Role::User, Block::Image { .. }) => out.push_str("[image]\n"),
                    (_, Block::ToolResult { content, is_error, .. }) => {
                        out.push_str(&format!("[tool {}]\n{}\n", if *is_error { "error" } else { "result" }, clip(content, 1500)))
                    }
                    (Role::Assistant, Block::Text(t)) => out.push_str(&format!("\n## Assistant\n{t}\n")),
                    (_, Block::ToolUse { name, input_json, .. }) => {
                        out.push_str(&format!("\n[tool call {name}: {}]\n", clip(input_json, 1500)))
                    }
                    _ => {}
                }
            }
            (out, typed)
        })
        .collect();
    // Recent steps, newest first, while they fit.
    let mut used = 0;
    let mut start = chunks.len();
    while start > 0 && used + chunks[start - 1].0.len() <= budget {
        start -= 1;
        used += chunks[start].0.len();
    }
    let mut out = String::new();
    if start > 0 {
        out.push_str(&format!("# Earlier ({start} messages, shortened to what the user wrote)\n"));
        for (c, _) in chunks[..start].iter().filter(|(_, typed)| *typed) {
            if used + c.len() > budget + budget / 4 {
                break;
            }
            used += c.len();
            out.push_str(c);
        }
        out.push_str("\n# Most recent\n");
    }
    for (c, _) in &chunks[start..] {
        out.push_str(c);
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

/// How a tool's images reach the model: each after a line saying where it
/// came from.
pub fn tool_image_blocks(tool: &str, images: &[(String, Vec<u8>)]) -> Vec<Block> {
    images
        .iter()
        .flat_map(|(mime, data)| {
            [Block::Text(format!("{TOOL_IMAGE}{tool}]")), Block::Image { media_type: mime.clone(), data: data.clone() }]
        })
        .collect()
}

const TOOL_IMAGE: &str = "[image from ";

/// Tool images the model still sees: the latest few. Older ones give way
/// to a line saying so (images are large, and a review loop makes many).
pub const KEEP_TOOL_IMAGES: usize = 12;

pub fn forget_old_tool_images(history: &mut [Message]) {
    let mut seen = 0;
    for m in history.iter_mut().rev().filter(|m| m.role == Role::User) {
        let mut after_label = false;
        for b in m.blocks.iter_mut().rev() {
            // Walking backwards: an image, then its label.
            match b {
                Block::Image { .. } => after_label = true,
                Block::Text(t) if after_label && t.starts_with(TOOL_IMAGE) => {
                    after_label = false;
                    seen += 1;
                }
                _ => after_label = false,
            }
        }
        if seen <= KEEP_TOOL_IMAGES {
            continue;
        }
        // Over the limit: drop this message's tool images (oldest first
        // overall, as we walk back).
        let mut out = Vec::with_capacity(m.blocks.len());
        let mut blocks = std::mem::take(&mut m.blocks).into_iter().peekable();
        while let Some(b) = blocks.next() {
            if let Block::Text(t) = &b
                && t.starts_with(TOOL_IMAGE)
                && matches!(blocks.peek(), Some(Block::Image { .. }))
            {
                blocks.next();
                out.push(Block::Text(format!("{t} (an earlier image, no longer shown)")));
                continue;
            }
            out.push(b);
        }
        m.blocks = out;
    }
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
    #[test]
    fn only_the_latest_tool_images_stay() {
        use c6_llm::{Block, Message, Role};
        let img = |n: u8| super::tool_image_blocks("browser", &[("image/png".into(), vec![n])]);
        let mut h: Vec<Message> = (0..super::KEEP_TOOL_IMAGES as u8 + 3)
            .map(|n| {
                let mut blocks = vec![Block::ToolResult { id: format!("t{n}"), content: "ok".into(), is_error: false }];
                blocks.extend(img(n));
                Message { role: Role::User, blocks }
            })
            .collect();
        // A user's own image is never dropped.
        h.insert(0, Message { role: Role::User, blocks: vec![Block::Image { media_type: "image/png".into(), data: vec![9] }] });
        super::forget_old_tool_images(&mut h);
        let images = h.iter().flat_map(|m| &m.blocks).filter(|b| matches!(b, Block::Image { .. })).count();
        assert_eq!(images, super::KEEP_TOOL_IMAGES + 1);
        assert!(matches!(&h[1].blocks[1], Block::Text(t) if t.contains("no longer shown")));
        assert!(matches!(&h[0].blocks[0], Block::Image { .. }));
    }

    use c6_proto::{AssistantMessage, ContentBlock, ToolResult, ToolUse, UserMessage};

    use super::*;

    #[test]
    fn digest_keeps_recent_steps_and_earlier_requests() {
        let step = |i: usize| {
            vec![
                Message {
                    role: Role::Assistant,
                    blocks: vec![Block::ToolUse { id: format!("t{i}"), name: "bash".into(), input_json: "{}".into() }],
                },
                Message {
                    role: Role::User,
                    blocks: vec![Block::ToolResult { id: format!("t{i}"), content: format!("output {i} ").repeat(500), is_error: false }],
                },
            ]
        };
        let mut msgs = vec![Message::user_text("build the card")];
        msgs.extend((0..20).flat_map(step));
        msgs.push(Message::user_text("now flip the blade"));
        let d = messages_digest(&msgs, 5000);
        assert!(d.contains("# Earlier") && d.contains("build the card") && d.contains("now flip the blade"), "{d}");
        assert!(d.contains("output 19") && !d.contains("output 0 "), "{d}");
        assert!(d.contains("(shortened)"));
        assert!(d.len() < 8000, "{}", d.len());
        // Everything fits: no omission note.
        assert!(!messages_digest(&msgs[..3], 100_000).contains("# Earlier"));
    }

    fn ev(kind: session_event::Kind) -> SessionEvent {
        SessionEvent { seq: 0, ts_ms: 0, kind: Some(kind) }
    }

    #[test]
    fn restored_usage_follows_compactions_handoffs_and_rewinds() {
        let usage = |input| {
            session_event::Kind::Usage(c6_proto::Usage {
                input_tokens: input,
                output_tokens: 3,
                cache_read_tokens: 5,
                cache_write_tokens: 7,
                reasoning_tokens: 2,
                ..Default::default()
            })
        };
        let mut events = vec![ev(usage(100))];
        assert_eq!(context_tokens_from_events(&events), 115, "reasoning is already counted in output");
        events.push(ev(session_event::Kind::Compaction(Default::default())));
        assert_eq!(context_tokens_from_events(&events), 0);
        events.push(ev(usage(20)));
        assert_eq!(context_tokens_from_events(&events), 35);
        events.push(ev(usage(50)));
        for (i, event) in events.iter_mut().enumerate() {
            event.seq = i as u64 + 1;
        }
        events.push(ev(session_event::Kind::Rewound(c6_proto::Rewound { to_seq: 4, ..Default::default() })));
        assert_eq!(context_tokens_from_events(&events), 35);
        events.push(ev(session_event::Kind::ModelSwitched(c6_proto::ModelSwitched {
            handoff: Some(Default::default()),
            ..Default::default()
        })));
        assert_eq!(context_tokens_from_events(&events), 0);
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

    #[test]
    fn a_handoff_is_a_marked_transcript_without_native_state() {
        let at = |seq: u64, kind: session_event::Kind| SessionEvent { seq, ts_ms: 0, kind: Some(kind) };
        let user = |t: &str| session_event::Kind::User(UserMessage { command_id: String::new(), text: t.into(), ..Default::default() });
        let events = vec![
            at(1, session_event::Kind::Started(Default::default())),
            at(2, user("fix the cache")),
            at(
                3,
                session_event::Kind::Assistant(AssistantMessage {
                    turn: 1,
                    blocks: vec![
                        ContentBlock {
                            block: Some(content_block::Block::SignedThinking(c6_proto::SignedThinking {
                                text: "secret plan".into(),
                                signature: "SIG-123".into(),
                            })),
                        },
                        ContentBlock {
                            block: Some(content_block::Block::Reasoning(c6_proto::Reasoning {
                                id: "rs_1".into(),
                                encrypted_content: "opaque-blob".into(),
                                summary: String::new(),
                            })),
                        },
                        ContentBlock { block: Some(content_block::Block::Text("Looking.".into())) },
                        tool_use("t1"),
                    ],
                    stop_reason: "tool_calls".into(),
                }),
            ),
            at(
                4,
                session_event::Kind::ToolResult(ToolResult {
                    call_id: "t1".into(),
                    tool: "bash".into(),
                    output: "cache.rs".into(),
                    ..Default::default()
                }),
            ),
            at(5, user("never mind this")),
            at(6, session_event::Kind::Rewound(c6_proto::Rewound { to_seq: 5, ..Default::default() })),
        ];
        let h = handoff_context(&events, "carbon6's agent (gpt-x)", HANDOFF_BUDGET);
        assert!(h.starts_with("<carbon6-handoff from=\"carbon6's agent (gpt-x)\">") && h.ends_with("</carbon6-handoff>"), "{h}");
        assert!(h.contains("for context only") && h.contains("did not carry over"), "{h}");
        assert!(h.contains("## User\nfix the cache") && h.contains("## Assistant\nLooking.") && h.contains("[tool call bash: {}]"), "{h}");
        assert!(h.contains("[tool result]\ncache.rs"), "{h}");
        // Reasoning, signatures and encrypted blocks stay behind; rewinds apply.
        for gone in ["secret plan", "SIG-123", "opaque-blob", "never mind this"] {
            assert!(!h.contains(gone), "{gone} leaked: {h}");
        }
        // Over budget: the latest steps, and a note on what was left out.
        let long: Vec<SessionEvent> = (0..200).map(|i| at(i + 1, user(&format!("message {i} {}", "x".repeat(100))))).collect();
        let h = handoff_context(&long, "Claude Code", 2_000);
        assert!(
            h.contains("earlier characters of the transcript left out") && h.contains("message 199") && !h.contains("message 0 "),
            "{h}"
        );
        assert!(h.len() < 3_000, "{}", h.len());
        assert!(handoff_context(&[], "Claude Code", 100).contains("(nothing yet)"));
    }
}
