//! Rebuilds the model conversation from a session's event log.

use std::borrow::Cow;
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
                for b in user_blocks_with_generated_attachments(&attributed_message_text(u), &u.attachments, u.generated_attachment_count) {
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
            Some(session_event::Kind::PlaceChanged(p)) if !p.notice.is_empty() => {
                push_user_block(&mut msgs, Block::Text(p.notice.clone()));
            }
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

/// What the model got for a user message: its text, or the expansion of
/// the slash command it invoked.
pub fn model_text(u: &c6_proto::UserMessage) -> &str {
    if u.prompt.is_empty() { &u.text } else { &u.prompt }
}

/// Provenance is server-derived, never inferred from the text. Another
/// session's existing wrapper takes precedence, then API, then known human.
/// Missing provenance (including old logs) is never presented as human.
pub(crate) fn attributed_input_text<'a>(
    text: &'a str,
    from_session_id: &str,
    api_origin: Option<&c6_proto::ApiMessageOrigin>,
    human_origin: bool,
) -> Cow<'a, str> {
    if !from_session_id.is_empty() {
        Cow::Borrowed(text)
    } else if let Some(origin) = api_origin {
        let token = if origin.token_name.is_empty() { String::new() } else { format!(" (token {:?})", origin.token_name) };
        Cow::Owned(format!("Message sent via the API{token}, not typed by the user:\n{text}"))
    } else if human_origin {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("Message sent by an automation, not typed by the user:\n{text}"))
    }
}

fn attributed_message_text(message: &c6_proto::UserMessage) -> Cow<'_, str> {
    attributed_input_text(model_text(message), &message.from_session_id, message.api_origin.as_ref(), message.human_origin)
}

fn media_type(attachment: &c6_proto::Attachment) -> &str {
    attachment.mime.split(';').next().unwrap_or_default().trim()
}

fn is_pdf(attachment: &c6_proto::Attachment) -> bool {
    media_type(attachment).eq_ignore_ascii_case("application/pdf")
        || attachment.name.rsplit_once('.').is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("pdf"))
        || c6_llm::is_native_pdf(&attachment.data)
}

fn is_extracted_document(attachment: &c6_proto::Attachment) -> bool {
    let mime = media_type(attachment);
    is_pdf(attachment)
        || [
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.oasis.opendocument.text",
            "application/rtf",
            "text/rtf",
            "text/html",
            "application/xhtml+xml",
        ]
        .iter()
        .any(|t| mime.eq_ignore_ascii_case(t))
        || attachment.name.rsplit_once('.').is_some_and(|(_, ext)| {
            ["docx", "pptx", "xlsx", "odt", "rtf", "html", "htm", "xhtml"].iter().any(|t| ext.eq_ignore_ascii_case(t))
        })
}

/// Machine-readable line on a worker-generated note when the source PDF is
/// known to be invalid or encrypted, not merely missing extractable text.
pub const NATIVE_PDF_REJECTED_NOTE: &str = "Native PDF unavailable: invalid or encrypted PDF.";

/// Match the name and exact original metadata, and look only at metadata
/// lines before any excerpt. Missing text alone doesn't disqualify a scan.
fn pdf_rejected_by_worker(pdf: &c6_proto::Attachment, notes: &[c6_proto::Attachment]) -> bool {
    notes.iter().any(|note| {
        note.mime == "text/plain"
            && note.name.strip_suffix(" (extracted text)") == Some(&pdf.name)
            && std::str::from_utf8(&note.data).is_ok_and(|text| {
                let mut lines = text.lines();
                lines.next() == Some(format!("Attachment: {:?}, MIME {:?}, {} bytes.", pdf.name, pdf.mime, pdf.data.len()).as_str())
                    && lines.next().is_some_and(|line| line.starts_with("Worker-local path: "))
                    && lines.take_while(|line| !line.is_empty()).any(|line| line == NATIVE_PDF_REJECTED_NOTE)
            })
    })
}

fn image_media_type(attachment: &c6_proto::Attachment) -> Option<&'static str> {
    let mime = media_type(attachment);
    ["image/png", "image/jpeg", "image/gif", "image/webp"].into_iter().find(|t| mime.eq_ignore_ascii_case(t))
}

fn attachment_text(attachment: &c6_proto::Attachment) -> String {
    if image_media_type(attachment).is_some() {
        return format!("\n\n[image {} not shown: this backend takes text only]", attachment.name);
    }
    // Do not turn arbitrary binary data into replacement characters or
    // control codes. The worker can provide extracted text separately.
    if !is_extracted_document(attachment)
        && let Ok(text) = std::str::from_utf8(&attachment.data)
        && !text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0c'))
    {
        return format!("\n\n<file name=\"{}\">\n{}\n</file>", attachment.name, text);
    }
    format!("\n\n[attachment {:?} ({} bytes): original contents not shown in text]", attachment.name, attachment.data.len())
}

/// A message as model blocks: its attributed text, then each attachment.
/// Images and PDFs can be sent natively; other files get a text rendering.
pub fn user_blocks(text: &str, attachments: &[c6_proto::Attachment]) -> Vec<Block> {
    user_blocks_with_generated_attachments(text, attachments, 0)
}

pub(crate) fn user_blocks_with_generated_attachments(
    text: &str,
    attachments: &[c6_proto::Attachment],
    generated_attachment_count: u32,
) -> Vec<Block> {
    // The worker appends its notes; never infer note provenance from a name,
    // MIME type, or text supplied by the user. A bad count trusts no notes.
    let end = attachments.len().checked_sub(generated_attachment_count as usize).unwrap_or(attachments.len());
    let (originals, notes) = attachments.split_at(end);
    let mut out = vec![Block::Text(text.to_owned())];
    for a in originals {
        if let Some(media_type) = image_media_type(a) {
            out.push(Block::Image { media_type: media_type.into(), data: a.data.clone() });
        } else if is_pdf(a) && !pdf_rejected_by_worker(a, notes) && c6_llm::is_native_pdf(&a.data) {
            out.push(Block::Document { media_type: "application/pdf".into(), filename: a.name.clone(), data: a.data.clone() });
        } else {
            out.push(Block::Text(attachment_text(a)));
        }
    }
    out.extend(notes.iter().map(|note| Block::Text(attachment_text(note))));
    out
}

/// A user message as plain text: text files inline, binary files named.
pub fn user_text(text: &str, attachments: &[c6_proto::Attachment]) -> String {
    let mut out = text.to_owned();
    for a in attachments {
        out.push_str(&attachment_text(a));
    }
    out
}

/// The events that still count: a `Rewound` drops the events of its own
/// session from its `to_seq` on, except for placement changes. Each session's
/// part of the list starts at its `Started` event (a side chat's list begins with its parents' events,
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
                // Placement is not rolled back with the conversation. Keep
                // the facts the model needs about where it will run next.
                let places: Vec<_> = kept.drain(cut..).filter(|e| matches!(e.kind, Some(session_event::Kind::PlaceChanged(_)))).collect();
                kept.extend(places);
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
                a.blocks.retain(|b| {
                    matches!(
                        b.block,
                        Some(content_block::Block::Text(_) | content_block::Block::Narration(_) | content_block::Block::ToolUse(_))
                    )
                });
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
            m.blocks.retain(|b| b.text().is_some() || matches!(b, Block::ToolUse { .. }));
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
/// given a history (Claude Code). Rewinds and compactions apply, including
/// separately numbered ancestor segments. Long tool output is shortened.
pub fn transcript_text(events: &[SessionEvent]) -> String {
    fn clip(s: &str, max: usize) -> String {
        if s.chars().count() <= max { s.to_owned() } else { format!("{}… (shortened)", s.chars().take(max).collect::<String>()) }
    }
    let mut out = String::new();
    for ev in &effective_events(events) {
        match &ev.kind {
            Some(session_event::Kind::User(u)) if !u.blocked.is_empty() => {}
            Some(session_event::Kind::User(u)) => {
                let heading = if u.human_origin && u.api_origin.is_none() && u.from_session_id.is_empty() { "User" } else { "Message" };
                out.push_str(&format!("\n## {heading}\n{}\n", user_text(&attributed_message_text(u), &u.attachments)));
            }
            Some(session_event::Kind::Assistant(a)) => {
                for b in &a.blocks {
                    match &b.block {
                        Some(content_block::Block::Text(t)) => out.push_str(&format!("\n## Assistant\n{t}\n")),
                        Some(content_block::Block::Narration(t)) => out.push_str(&format!("\n## Assistant (summarized update)\n{t}\n")),
                        Some(content_block::Block::ToolUse(u)) => {
                            out.push_str(&format!("\n[tool call {}: {}]\n", u.name, clip(&u.input_json, 2000)))
                        }
                        _ => {}
                    }
                }
            }
            Some(session_event::Kind::ToolResult(r)) => out.push_str(&format!("[tool result]\n{}\n", clip(&r.output, 2000))),
            Some(session_event::Kind::Compaction(c)) => out = format!("## Summary of the earlier conversation\n{}\n", c.summary),
            Some(session_event::Kind::PlaceChanged(p)) if !p.notice.is_empty() => {
                out.push_str(&format!("\n## Session placement\n{}\n", p.notice));
            }
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
    history_context(events, &format!("This session was handed over to you from {from} at this point."), from, budget)
}

/// Fresh-runtime rewind fallback: only the kept transcript, never a claim
/// that a runtime switch or deletion of files/logs happened.
pub fn rewind_context(events: &[SessionEvent], budget: usize) -> String {
    history_context(events, "The user rewound this conversation. Continue from only the kept history below.", "kept history", budget)
}

fn history_context(events: &[SessionEvent], introduction: &str, from: &str, budget: usize) -> String {
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
         {introduction} You did not take part in the conversation below and have \
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
/// shortened), and before them the earlier requests and notices.
pub(crate) fn messages_digest(msgs: &[Message], budget: usize) -> String {
    fn clip(s: &str, max: usize) -> String {
        if s.chars().count() <= max { s.to_owned() } else { format!("{}… (shortened)", s.chars().take(max).collect::<String>()) }
    }
    // One chunk per message, and whether it contains input text. Provider
    // Role::User also carries automation and tool results, not just humans.
    let chunks: Vec<(String, bool)> = msgs
        .iter()
        .map(|m| {
            let mut out = String::new();
            let mut has_input = false;
            for b in &m.blocks {
                match (m.role, b) {
                    (Role::User, Block::Text(t) | Block::AnnotatedText { text: t, .. }) => {
                        has_input = true;
                        out.push_str(&format!("\n## Message\n{}\n", clip(t, 4000)));
                    }
                    (Role::User, Block::Image { .. }) => out.push_str("[image]\n"),
                    (Role::User, Block::Document { filename, .. }) => {
                        has_input = true;
                        out.push_str(&format!("[document {filename:?}]\n"));
                    }
                    (_, Block::ToolResult { content, is_error, .. }) => {
                        out.push_str(&format!("[tool {}]\n{}\n", if *is_error { "error" } else { "result" }, clip(content, 1500)))
                    }
                    (Role::Assistant, Block::Text(t) | Block::AnnotatedText { text: t, .. }) => {
                        out.push_str(&format!("\n## Assistant\n{t}\n"));
                    }
                    (_, Block::ToolUse { name, input_json, .. }) => {
                        out.push_str(&format!("\n[tool call {name}: {}]\n", clip(input_json, 1500)))
                    }
                    _ => {}
                }
            }
            (out, has_input)
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
        out.push_str(&format!("# Earlier ({start} messages, shortened to input text with its recorded attribution)\n"));
        for (c, _) in chunks[..start].iter().filter(|(_, has_input)| *has_input) {
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
        content_block::Block::Text(t) => Some(if b.annotations_json.is_empty() {
            Block::Text(t.clone())
        } else {
            Block::AnnotatedText { text: t.clone(), annotations_json: b.annotations_json.clone() }
        }),
        content_block::Block::Thinking(t) => Some(Block::Thinking(t.clone())),
        // Display provenance must not drop visible context on a handoff to
        // another model. The Claude CLI owns replay of its original blocks.
        content_block::Block::Narration(t) => Some(Block::Text(t.clone())),
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
        Block::Text(t) | Block::AnnotatedText { text: t, .. } => content_block::Block::Text(t.clone()),
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
        // User media is already kept in the event's attachments.
        Block::ToolResult { .. } | Block::Image { .. } | Block::Document { .. } => return None,
    };
    let annotations_json = match b {
        Block::AnnotatedText { annotations_json, .. } => annotations_json.clone(),
        _ => String::new(),
    };
    Some(c6_proto::ContentBlock { block: Some(block), annotations_json })
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

    // A compact cross-reference table fixture. The provider preflight does not
    // parse page objects; full document parsing is the worker's responsibility.
    fn pdf_fixture() -> Vec<u8> {
        b"%PDF-1.4\n1 0 obj\n<</Type /Catalog>>\nendobj\nxref\n0 2\n0000000000 65535 f \n0000000009 00000 n \ntrailer\n<</Root 1 0 R /Size 2>>\nstartxref\n43\n%%EOF\n".to_vec()
    }

    #[test]
    fn attachments_survive_live_input_and_history_replay() {
        let pdf = pdf_fixture();
        let input = crate::PendingInput {
            text: "Read the files".into(),
            human_origin: true,
            attachments: vec![
                c6_proto::Attachment { name: "report.pdf".into(), mime: "application/pdf".into(), data: pdf.clone() },
                c6_proto::Attachment { name: "report.pdf.txt".into(), mime: "text/plain".into(), data: b"Extracted report".to_vec() },
                c6_proto::Attachment { name: "unknown.bin".into(), mime: "application/octet-stream".into(), data: vec![0xff, 0] },
                c6_proto::Attachment { name: "nul.bin".into(), mime: "application/octet-stream".into(), data: b"a\0b".to_vec() },
            ],
            ..Default::default()
        };
        let blocks = input.blocks();
        assert_eq!(blocks[1], Block::Document { media_type: "application/pdf".into(), filename: "report.pdf".into(), data: pdf });
        assert!(matches!(&blocks[2], Block::Text(t) if t.contains("Extracted report")));
        assert!(matches!(&blocks[3], Block::Text(t) if t.contains("unknown.bin") && t.contains("2 bytes") && !t.contains('�')));
        assert!(matches!(&blocks[4], Block::Text(t) if t.contains("nul.bin") && !t.contains('\0')));

        let events = [ev(input.event())];
        let replayed = history_from_events(&events);
        assert_eq!(replayed[0].blocks, blocks);
        assert_eq!(portable_messages(replayed.clone()), replayed);
        assert!(messages_digest(&replayed, 10_000).contains("[document \"report.pdf\"]"));
        let transcript = transcript_text(&events);
        assert!(transcript.contains("report.pdf") && transcript.contains("Extracted report") && transcript.contains("unknown.bin"));
        assert!(!transcript.contains("%PDF-1.4") && !transcript.contains('�'));
    }

    #[test]
    fn only_pdf_with_a_header_and_supported_image_formats_become_native_blocks() {
        let attachments = [
            c6_proto::Attachment { name: "other.PDF".into(), mime: "application/octet-stream".into(), data: pdf_fixture() },
            c6_proto::Attachment { name: "bad.pdf".into(), mime: "application/pdf".into(), data: b"%PDF-1.7 not a PDF".to_vec() },
            c6_proto::Attachment { name: "drawing.svg".into(), mime: "image/svg+xml".into(), data: b"<svg/>".to_vec() },
            c6_proto::Attachment { name: "photo.png".into(), mime: "Image/PNG; x-example=test".into(), data: vec![0, 1] },
            c6_proto::Attachment { name: "report".into(), mime: "Application/PDF; x-example=test".into(), data: pdf_fixture() },
            c6_proto::Attachment { name: "upload.bin".into(), mime: "application/octet-stream".into(), data: pdf_fixture() },
        ];
        let blocks = user_blocks("Read these.", &attachments);
        assert!(matches!(&blocks[1], Block::Document { media_type, .. } if media_type == "application/pdf"));
        assert!(matches!(&blocks[2], Block::Text(t) if t.contains("bad.pdf") && !t.contains("not a PDF")));
        assert!(matches!(&blocks[3], Block::Text(t) if t.contains("drawing.svg") && t.contains("<svg/>")));
        assert!(matches!(&blocks[4], Block::Image { media_type, data } if media_type == "image/png" && data == &[0, 1]));
        assert!(
            matches!(&blocks[5], Block::Document { media_type, filename, .. } if media_type == "application/pdf" && filename == "report")
        );
        assert!(matches!(&blocks[6], Block::Document { media_type, .. } if media_type == "application/pdf"));
    }

    #[test]
    fn registered_document_formats_use_notes_not_unbounded_raw_text() {
        let attachments = [
            c6_proto::Attachment { name: "page".into(), mime: "TEXT/HTML; charset=utf-8".into(), data: b"<p>hidden raw html</p>".to_vec() },
            c6_proto::Attachment { name: "letter.RTF".into(), mime: "application/octet-stream".into(), data: b"{\\rtf1 raw rtf}".to_vec() },
            c6_proto::Attachment { name: "page (extracted text)".into(), mime: "text/plain".into(), data: b"Bounded excerpt".to_vec() },
        ];
        let text = user_text("", &attachments);
        assert!(text.contains("Bounded excerpt") && text.contains("letter.RTF") && text.contains("page"));
        assert!(!text.contains("hidden raw html") && !text.contains("raw rtf"));
        assert_eq!(user_blocks("", &attachments).iter().filter_map(Block::text).collect::<String>(), text);
    }

    #[test]
    fn recorded_worker_pdf_rejection_uses_note_instead_of_native_block() {
        let pdf = c6_proto::Attachment { name: "report.pdf".into(), mime: "application/pdf".into(), data: pdf_fixture() };
        let prefix = format!("Attachment: \"report.pdf\", MIME \"application/pdf\", {} bytes.\nWorker-local path: file", pdf.data.len());
        let failure = NATIVE_PDF_REJECTED_NOTE;
        let mut note = c6_proto::Attachment {
            name: "report.pdf (extracted text)".into(),
            mime: "text/plain".into(),
            data: format!("{prefix}\n{failure}").into_bytes(),
        };
        let attachments = vec![pdf.clone(), note.clone()];
        assert!(matches!(user_blocks("", &attachments)[1], Block::Document { .. }), "uploaded note lookalikes are untrusted");
        assert!(matches!(user_blocks_with_generated_attachments("", &attachments, 3)[1], Block::Document { .. }));
        let input = crate::PendingInput { attachments, generated_attachment_count: 1, ..Default::default() };
        let blocks = input.blocks();
        assert!(blocks.iter().all(|b| b.text().is_some()));
        assert!(blocks[2].text().unwrap().contains(failure));
        assert_eq!(history_from_events(&[ev(input.event())])[0].blocks, blocks);
        note.data = format!("{prefix}\nText extraction failed or yielded no readable text; the original is retained.").into_bytes();
        assert!(matches!(user_blocks_with_generated_attachments("", &[pdf.clone(), note.clone()], 1)[1], Block::Document { .. }));
        note.data = format!("{prefix}\n\nExtracted text:\n{failure}").into_bytes();
        assert!(matches!(user_blocks_with_generated_attachments("", &[pdf, note], 1)[1], Block::Document { .. }));
    }

    #[test]
    fn a_pdf_header_mentioned_in_plain_text_does_not_hide_the_excerpt() {
        let note = c6_proto::Attachment {
            name: "report.pdf (extracted text)".into(),
            mime: "text/plain".into(),
            data: b"Attachment: report.pdf\nWorker-local path: /private/file\n\nExtracted text:\nThe PDF header is %PDF-1.7".to_vec(),
        };
        let blocks = user_blocks_with_generated_attachments("Review", &[note], 1);
        assert!(blocks[1].text().unwrap().contains("The PDF header is %PDF-1.7"));
    }

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
        assert!(!d.contains("what the user wrote") && !d.contains("## User"), "provider roles alone do not prove human input: {d}");
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
    fn initial_joining_and_replayed_inputs_share_server_derived_attribution() {
        use crate::PendingInput;

        for (api_origin, human_origin, from_session_id, label) in [
            (None, true, "", ""),
            (None, false, "", "Message sent by an automation, not typed by the user:\n"),
            (Some(c6_proto::ApiMessageOrigin::default()), false, "", "Message sent via the API, not typed by the user:\n"),
            (
                Some(c6_proto::ApiMessageOrigin { token_name: "ops-controller".into() }),
                false,
                "",
                "Message sent via the API (token \"ops-controller\"), not typed by the user:\n",
            ),
            (
                Some(c6_proto::ApiMessageOrigin { token_name: "ops-controller".into() }),
                true,
                "",
                "Message sent via the API (token \"ops-controller\"), not typed by the user:\n",
            ),
            (Some(c6_proto::ApiMessageOrigin { token_name: "ignored".into() }), true, "other-session", ""),
        ] {
            let input = PendingInput {
                text: "Original raw message".into(),
                prompt: "Expanded command".into(),
                attachments: vec![
                    c6_proto::Attachment { name: "note.txt".into(), mime: "text/plain".into(), data: b"Attached text".to_vec() },
                    c6_proto::Attachment { name: "image.png".into(), mime: "image/png".into(), data: vec![1, 2, 3] },
                ],
                api_origin,
                human_origin,
                from_session_id: from_session_id.into(),
                ..Default::default()
            };
            // Both initial and mid-turn native delivery consume blocks().
            let blocks = input.blocks();
            assert_eq!(blocks[0], Block::Text(format!("{label}Expanded command")));
            assert!(matches!(&blocks[1], Block::Text(t) if t.contains("Attached text")));
            assert!(matches!(&blocks[2], Block::Image { data, .. } if data == &[1, 2, 3]));
            let event = input.event();
            let session_event::Kind::User(message) = &event else { panic!("input event") };
            assert!(message.origin_recorded);
            assert_eq!(message.text, "Original raw message");
            assert_eq!(message.prompt, "Expanded command");
            assert_eq!(history_from_events(&[ev(event)])[0].blocks, blocks);
        }
    }

    #[test]
    fn transcript_and_compaction_do_not_promote_automation_or_legacy_input_to_user() {
        let api = UserMessage {
            text: "API request".into(),
            api_origin: Some(c6_proto::ApiMessageOrigin { token_name: "ops-controller".into() }),
            human_origin: true, // An inconsistent old flag must not win.
            origin_recorded: true,
            ..Default::default()
        };
        let unknown = UserMessage { text: "Legacy request".into(), ..Default::default() };
        let session = UserMessage {
            text: "<session-message>Existing attribution</session-message>".into(),
            from_session_id: "other-session".into(),
            ..Default::default()
        };
        let automated = [api, unknown, session].map(|u| ev(session_event::Kind::User(u)));
        let transcript = transcript_text(&automated);
        assert!(!transcript.contains("## User"), "{transcript}");
        assert!(
            transcript.contains("## Message\nMessage sent via the API (token \"ops-controller\"), not typed by the user:\nAPI request")
        );
        assert!(transcript.contains("## Message\nMessage sent by an automation, not typed by the user:\nLegacy request"));
        assert!(transcript.contains("## Message\n<session-message>Existing attribution</session-message>"));
        let digest = messages_digest(&history_from_events(&automated), 100_000);
        assert!(!digest.contains("## User"), "{digest}");
        assert!(digest.contains("not typed by the user:\nAPI request") && digest.contains("not typed by the user:\nLegacy request"));
        let human = ev(session_event::Kind::User(UserMessage { text: "Human request".into(), human_origin: true, ..Default::default() }));
        assert_eq!(transcript_text(&[human]), "\n## User\nHuman request\n");
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
        ContentBlock {
            block: Some(content_block::Block::ToolUse(ToolUse { id: id.into(), name: "bash".into(), input_json: "{}".into() })),
            ..Default::default()
        }
    }

    #[test]
    fn citation_annotations_round_trip_through_the_journal_block() {
        let block = Block::AnnotatedText {
            text: "Fact. \u{e200}cite\u{e202}turn1search1\u{e201}".into(),
            annotations_json: r#"[{"type":"url_citation","url":"https://source.test","title":"Source","start_index":6,"end_index":33}]"#
                .into(),
        };
        let proto = block_to_proto(&block).unwrap();
        assert_eq!(block_from_proto(&proto), Some(block));
        let old = ContentBlock { block: Some(content_block::Block::Text("Old reply".into())), ..Default::default() };
        assert_eq!(block_from_proto(&old), Some(Block::Text("Old reply".into())));
    }

    #[test]
    fn narration_is_kept_in_handoff_context_without_becoming_private_reasoning() {
        let block = ContentBlock { block: Some(content_block::Block::Narration("Visible summary".into())), ..Default::default() };
        assert_eq!(block_from_proto(&block), Some(Block::Text("Visible summary".into())));
        let events = vec![ev(session_event::Kind::Assistant(c6_proto::AssistantMessage {
            blocks: vec![
                block,
                ContentBlock { block: Some(content_block::Block::Thinking("Private reasoning".into())), ..Default::default() },
            ],
            ..Default::default()
        }))];
        let text = transcript_text(&events);
        assert!(text.contains("## Assistant (summarized update)\nVisible summary"), "{text}");
        let portable = portable_events(&events);
        assert!(
            matches!(&events[0].kind, Some(session_event::Kind::Assistant(a)) if a.blocks.len() == 2),
            "the original transcript remains intact"
        );
        let history = history_from_events(&portable);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].blocks, vec![Block::Text("Visible summary".into())], "cross-model side chats keep summaries, not reasoning");
    }

    #[test]
    fn repairs_missing_results_and_keeps_order() {
        let events = vec![
            ev(session_event::Kind::User(UserMessage {
                command_id: "c1".into(),
                text: "go".into(),
                human_origin: true,
                ..Default::default()
            })),
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
            ev(session_event::Kind::User(UserMessage {
                command_id: "c2".into(),
                text: "again".into(),
                human_origin: true,
                ..Default::default()
            })),
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
    fn text_transcripts_apply_rewinds_before_compactions_in_each_seed_segment() {
        let user = |text: &str| session_event::Kind::User(UserMessage { text: text.into(), ..Default::default() });
        let mut events = vec![
            ev(session_event::Kind::Started(Default::default())),
            ev(user("KEEP_PARENT")),
            ev(user("DROP_PRIVATE")),
            ev(session_event::Kind::Compaction(c6_proto::Compaction { summary: "PRIVATE_SUMMARY".into(), ..Default::default() })),
            ev(session_event::Kind::Rewound(c6_proto::Rewound { to_seq: 3, ..Default::default() })),
            ev(user("KEEP_REPLACEMENT")),
        ];
        for (i, e) in events.iter_mut().enumerate() {
            e.seq = i as u64 + 1;
        }
        let mut own = vec![
            ev(session_event::Kind::Started(Default::default())),
            ev(user("KEEP_CHILD")),
            ev(user("PRIVATE_CHILD")),
            ev(session_event::Kind::Rewound(c6_proto::Rewound { to_seq: 3, ..Default::default() })),
        ];
        for (i, e) in own.iter_mut().enumerate() {
            e.seq = i as u64 + 1;
        }
        events.extend(own);
        for text in [transcript_text(&events), handoff_context(&events, "Claude Code", HANDOFF_BUDGET)] {
            assert!(text.contains("KEEP_PARENT") && text.contains("KEEP_REPLACEMENT") && text.contains("KEEP_CHILD"), "{text}");
            assert!(!text.contains("PRIVATE"), "{text}");
        }
    }

    #[test]
    fn rewinds_drop_their_own_session_events() {
        let at = |seq: u64, kind: session_event::Kind| SessionEvent { seq, ts_ms: 0, kind: Some(kind) };
        let user = |t: &str| session_event::Kind::User(UserMessage { text: t.into(), human_origin: true, ..Default::default() });
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

    fn placement_events(kinds: impl IntoIterator<Item = session_event::Kind>) -> Vec<SessionEvent> {
        kinds.into_iter().enumerate().map(|(i, kind)| SessionEvent { seq: i as u64 + 1, ..ev(kind) }).collect()
    }

    fn placement_user(text: &str) -> session_event::Kind {
        session_event::Kind::User(UserMessage { text: text.into(), human_origin: true, ..Default::default() })
    }

    fn move_place(from: &str, to: &str, notice: &str) -> session_event::Kind {
        let place = |cwd: &str| Some(c6_proto::SessionPlace { cwd: cwd.into(), ..Default::default() });
        session_event::Kind::PlaceChanged(c6_proto::PlaceChanged {
            from: place(from),
            to: place(to),
            system_prompt: format!("Working directory: {to}"),
            notice: notice.into(),
            ..Default::default()
        })
    }

    fn placement_rewind(to_seq: u64) -> session_event::Kind {
        session_event::Kind::Rewound(c6_proto::Rewound { to_seq, ..Default::default() })
    }

    #[test]
    fn a_move_keeps_conversation_and_replays_the_edited_notice_verbatim() {
        let notice = "I moved the scratch files.\nContinue in /new.";
        let events = placement_events([
            placement_user("keep working on this"),
            session_event::Kind::Assistant(AssistantMessage {
                blocks: vec![block_to_proto(&Block::Text("I've made a start".into())).unwrap()],
                ..Default::default()
            }),
            move_place("/old", "/new", notice),
            move_place("/new", "/quiet", ""),
            placement_user("carry on"),
        ]);
        let history = history_from_events(&events);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].blocks, [Block::Text("keep working on this".into())]);
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[1].blocks, [Block::Text("I've made a start".into())]);
        assert_eq!(history[2].role, Role::User);
        assert_eq!(history[2].blocks, [Block::Text(notice.into()), Block::Text("carry on".into())]);

        let transcript = transcript_text(&events);
        assert!(transcript.contains(&format!("\n## Session placement\n{notice}\n")));
        assert_eq!(transcript.matches("## Session placement").count(), 1, "a blank notice stays silent");
    }

    #[test]
    fn rewinding_across_a_move_keeps_its_notice_and_current_place_facts() {
        let notice = "Continue in /new";
        let events = placement_events([
            session_event::Kind::Started(Default::default()),
            placement_user("keep this"),
            placement_user("rewind this"),
            move_place("/old", "/new", notice),
            placement_user("also rewind this"),
            placement_rewind(3),
            placement_user("replacement"),
        ]);
        let effective = effective_events(&events);
        assert_eq!(effective.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 4, 7]);
        let Some(session_event::Kind::PlaceChanged(place)) = &effective[2].kind else { panic!("missing move") };
        assert_eq!(place.to.as_ref().unwrap().cwd, "/new");
        assert_eq!(place.system_prompt, "Working directory: /new");
        let history = history_from_events(&events);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].blocks, [Block::Text("keep this".into()), Block::Text(notice.into()), Block::Text("replacement".into())]);
        let handoff = handoff_context(&events, "carbon6", HANDOFF_BUDGET);
        assert!(handoff.contains(notice));
        assert!(!handoff.contains("rewind this"));
    }

    #[test]
    fn multiple_moves_and_rewinds_keep_each_notice_once_in_order() {
        let events = placement_events([
            placement_user("keep this"),
            placement_user("drop first"),
            move_place("/one", "/two", "Moved to /two"),
            placement_rewind(2),
            placement_user("drop second"),
            move_place("/two", "/three", "Moved to /three"),
            placement_rewind(5),
            placement_rewind(2),
            placement_user("keep this too"),
        ]);
        assert_eq!(effective_events(&events).iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 3, 6, 9]);
        let history = history_from_events(&events);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].blocks,
            [
                Block::Text("keep this".into()),
                Block::Text("Moved to /two".into()),
                Block::Text("Moved to /three".into()),
                Block::Text("keep this too".into())
            ]
        );
    }

    #[test]
    fn compaction_replaces_earlier_notices_but_rewind_can_remove_the_compaction() {
        let summary = "Earlier work summarized";
        let mut events = placement_events([
            placement_user("earlier conversation"),
            move_place("/one", "/two", "First move"),
            session_event::Kind::Compaction(c6_proto::Compaction { summary: summary.into(), ..Default::default() }),
            move_place("/two", "/three", "Second move"),
        ]);
        let history = history_from_events(&events);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].blocks, [Block::Text(compacted_context(summary)), Block::Text("Second move".into())]);
        let transcript = transcript_text(&events);
        assert!(transcript.contains(summary) && transcript.contains("Second move"));
        assert!(!transcript.contains("First move") && !transcript.contains("\nearlier conversation\n"));
        assert_eq!(effective_events(&events).iter().filter(|e| matches!(e.kind, Some(session_event::Kind::PlaceChanged(_)))).count(), 2);

        events.push(SessionEvent { seq: 5, ..ev(placement_rewind(3)) });
        let history = history_from_events(&events);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].blocks,
            [Block::Text("earlier conversation".into()), Block::Text("First move".into()), Block::Text("Second move".into())]
        );
        let transcript = transcript_text(&events);
        assert!(!transcript.contains(summary));
        assert!(transcript.contains("First move") && transcript.contains("Second move"));
    }

    #[test]
    fn a_handoff_is_a_marked_transcript_without_native_state() {
        let at = |seq: u64, kind: session_event::Kind| SessionEvent { seq, ts_ms: 0, kind: Some(kind) };
        let user = |t: &str| session_event::Kind::User(UserMessage { text: t.into(), human_origin: true, ..Default::default() });
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
                            ..Default::default()
                        },
                        ContentBlock {
                            block: Some(content_block::Block::Reasoning(c6_proto::Reasoning {
                                id: "rs_1".into(),
                                encrypted_content: "opaque-blob".into(),
                                summary: String::new(),
                            })),
                            ..Default::default()
                        },
                        ContentBlock { block: Some(content_block::Block::Text("Looking.".into())), ..Default::default() },
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

/// How old tool output is trimmed from a long conversation (docs/replay-eval.md):
/// the latest outputs, and the first after every message to the agent, stay.
pub const TRIM_KEEP_LATEST: usize = 8;
pub const TRIM_KEEP_AFTER_INSTRUCTION: usize = 5;
/// Shorter outputs stay: a stub would save little.
pub const TRIM_MIN_CHARS: usize = 400;

/// What a trimmed tool output says instead (the editable prompt
/// `session.trimmed_output`): `{{call}}` is the tool and its main argument.
pub const TRIMMED_OUTPUT: &str = "[Earlier output of {{call}} removed to save context. Run the tool again if you need it.]";

/// Replaces old tool outputs with `stub` (naming each call), keeping the
/// latest [`TRIM_KEEP_LATEST`], the first [`TRIM_KEEP_AFTER_INSTRUCTION`]
/// after every message to the agent (what it read to learn each
/// instruction), and short ones. Tool calls and the agent's words stay.
/// Trimmed outputs stay trimmed, so a conversation's prefix changes only
/// when it is trimmed again. Returns how many outputs it trimmed.
pub fn trim_old_tool_output(history: &mut [Message], stub: &str) -> usize {
    let mut calls = std::collections::HashMap::new();
    for m in history.iter() {
        for b in &m.blocks {
            if let Block::ToolUse { id, name, input_json } = b {
                let v: serde_json::Value = serde_json::from_str(input_json).unwrap_or_default();
                let arg = ["file_path", "path", "pattern", "command", "cmd", "query", "url"]
                    .iter()
                    .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
                    .unwrap_or("");
                let arg: String = arg.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
                calls.insert(id.clone(), if arg.is_empty() { name.clone() } else { format!("{name} {arg}") });
            }
        }
    }
    // Every tool result, in order, and whether it opens an instruction's
    // first outputs.
    let mut results = Vec::new();
    let mut since_instruction = None::<usize>;
    for (i, m) in history.iter().enumerate() {
        if m.role != Role::User {
            continue;
        }
        for (j, b) in m.blocks.iter().enumerate() {
            match b {
                Block::ToolResult { .. } => {
                    let kept = since_instruction.is_some_and(|n| n < TRIM_KEEP_AFTER_INSTRUCTION);
                    if let Some(n) = since_instruction.as_mut() {
                        *n += 1;
                    }
                    results.push((i, j, kept));
                }
                Block::Text(t) if !t.starts_with(TOOL_IMAGE) => since_instruction = Some(0),
                _ => {}
            }
        }
    }
    let old = results.len().saturating_sub(TRIM_KEEP_LATEST);
    let mut trimmed = 0;
    for &(i, j, kept) in &results[..old] {
        if kept {
            continue;
        }
        if let Block::ToolResult { id, content, .. } = &mut history[i].blocks[j] {
            if content.chars().count() < TRIM_MIN_CHARS || content.starts_with(TRIMMED_MARK) {
                continue;
            }
            let call = calls.get(id).cloned().unwrap_or_else(|| "a tool call".into());
            *content = format!("{TRIMMED_MARK}{}", stub.replace("{{call}}", &call));
            trimmed += 1;
        }
    }
    trimmed
}

/// Marks a trimmed output, so it is never trimmed (or counted) again.
const TRIMMED_MARK: &str = "\u{2060}";

#[cfg(test)]
mod trim_tests {
    use super::*;

    fn call(id: &str, path: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse { id: id.into(), name: "Read".into(), input_json: format!(r#"{{"file_path":"{path}"}}"#) }],
        }
    }

    fn result(id: &str) -> Block {
        Block::ToolResult { id: id.into(), content: "x".repeat(1000), is_error: false }
    }

    fn content(m: &[Message], id: &str) -> String {
        m.iter()
            .flat_map(|m| &m.blocks)
            .find_map(|b| match b {
                Block::ToolResult { id: i, content, .. } if i == id => Some(content.clone()),
                _ => None,
            })
            .unwrap()
    }

    /// Old outputs go, but not the latest, the first after each
    /// instruction, or short ones; trimming again changes nothing.
    #[test]
    fn old_tool_output_is_trimmed_keeping_what_teaches_the_task() {
        let mut h = vec![Message::user_text(String::from("Do the task in TASK.md"))];
        for n in 0..30 {
            let id = format!("t{n}");
            h.push(call(&id, &format!("/src/f{n}.rs")));
            if n == 20 {
                // An instruction handed over mid-run, with the result.
                h.push(Message { role: Role::User, blocks: vec![result(&id), Block::Text("Now read FIX.md and fix it".into())] });
            } else {
                h.push(Message { role: Role::User, blocks: vec![result(&id)] });
            }
        }
        h.push(call("short", "/a"));
        let n = trim_old_tool_output(&mut h, TRIMMED_OUTPUT);
        for kept in ["t0", "t4", "t21", "t25", "t22", "t29"] {
            assert_eq!(content(&h, kept).len(), 1000, "{kept} stays");
        }
        for gone in ["t5", "t10", "t20"] {
            let c = content(&h, gone);
            assert!(c.contains("Earlier output of Read /src/f"), "{gone}: {c}");
        }
        // t0..t4 (the task's first), t21..t25 (the later instruction's first)
        // and t22..t29 (the latest) stay: t5..t20 go.
        assert_eq!(n, 16);
        assert_eq!(trim_old_tool_output(&mut h, TRIMMED_OUTPUT), 0, "trimmed outputs stay as they are");
    }
}
