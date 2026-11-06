//! A session's timeline: its turns, which the web UI shows as an outline
//! by hour in the session's glance and as the rows of its folded turns,
//! each with who started it, its steps and a line saying what it did. The
//! lines come from a small model (a `task:timeline` routing rule's, else
//! the session's own), one call for the turns of an hour when you open it,
//! and are kept in the head's memory, as are the turns read so far.

use std::collections::HashMap;

use c6_api::{self as api, ContentBlock, Event, EventKind, TimelineTurn};
use tracing::warn;

use crate::HeadState;
use crate::catchup::{MAX_INPUT_CHARS, clip, doc_comment, remember, target, task_words, visible};
use crate::realms::Caller;
use crate::store::SessionRow;

/// The routing label a rule names to pick the timeline's model.
pub const ROUTE_LABEL: &str = "task:timeline";
const HOUR: i64 = 3_600_000;
/// Whether a turn's line is made (off until the timeline is redesigned).
const LINES: bool = false;
/// A turn's events read at most to describe it to the model.
const TURN_EVENTS: u64 = 2_000;
/// A turn's tool calls told to the model at most.
const TURN_TOOLS: usize = 20;

/// A session's turns, as far as its log was read.
#[derive(Clone, Default)]
pub struct Turns {
    read: u64,
    list: Vec<TimelineTurn>,
    /// The input the next turn takes (written before it starts): its
    /// event, and who sent it.
    next: Option<(u64, Who)>,
    /// The last turn hasn't ended.
    open: bool,
}

/// Who sent a message: `by` as [`TimelineTurn::by`] says.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Who {
    by: String,
    from: String,
    words: String,
}

/// Who sent a message that starts a turn.
pub(crate) fn who(kind: &EventKind) -> Option<Who> {
    let EventKind::User { text, command, from_session_id, from_title, delegated_by, .. } = kind else { return None };
    let words = |t: &str| clip(t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(""), 60);
    let (by, from, words) = if !from_session_id.is_empty() {
        ("session", from_title.clone(), words(text))
    } else if command == "task-notification" {
        ("task", String::new(), clip(&task_words(text), 60))
    } else if command == "stop-hook" {
        ("", String::new(), words(text))
    } else if let Some(doc) = doc_comment(text) {
        ("doc", doc, String::new())
    } else if !delegated_by.is_empty() {
        ("agent", String::new(), words(text))
    } else {
        ("you", String::new(), words(text))
    };
    Some(Who { by: by.into(), from, words })
}

/// A reply's first sentence, on one line.
pub(crate) fn first_sentence(text: &str) -> String {
    let line = text.lines().map(|l| l.trim().trim_start_matches(['#', '*', '-', '>', '`', ' '])).find(|l| !l.is_empty()).unwrap_or("");
    let end = [". ", "! ", "? "].iter().filter_map(|p| line.find(p).map(|i| i + 1)).min().unwrap_or(line.len());
    clip(&line[..end], 140)
}

impl Turns {
    pub(crate) fn feed(&mut self, e: &Event) {
        self.read = self.read.max(e.seq);
        let open = self.open;
        match &e.kind {
            EventKind::User { .. } => {
                let Some(w) = who(&e.kind) else { return };
                match self.list.last_mut().filter(|_| open) {
                    // It joined a running turn that had no one's message.
                    Some(t) if t.by.is_empty() => (t.by, t.from, t.words) = (w.by, w.from, w.words),
                    Some(_) => {}
                    None if self.next.is_none() => self.next = Some((e.seq, w)),
                    None => {}
                }
            }
            EventKind::TurnStarted { turn, .. } => {
                // One that never said it ended (its worker went away) ends here.
                if let Some(t) = self.list.last_mut().filter(|_| open) {
                    t.end_seq = e.seq - 1;
                }
                let (start, w) = self.next.take().unwrap_or((e.seq, Who::default()));
                self.list.push(TimelineTurn {
                    turn: *turn,
                    start_seq: start,
                    end_seq: 0,
                    at: e.ts,
                    steps: 0,
                    by: w.by,
                    from: w.from,
                    words: w.words,
                    reply: String::new(),
                    summary: String::new(),
                });
                self.open = true;
            }
            EventKind::Assistant { blocks, .. } if open => {
                if let Some(t) = self.list.last_mut() {
                    t.steps += e.kind.steps();
                    let text = text_of(blocks);
                    if !text.trim().is_empty() {
                        t.reply = first_sentence(&text);
                    }
                }
            }
            EventKind::TurnEnded { .. } if open => {
                if let Some(t) = self.list.last_mut() {
                    t.end_seq = e.seq;
                }
                self.open = false;
            }
            _ => {}
        }
    }
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(api::citations::clean(text)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The session's turns, read on from where they were last.
async fn turns(st: &HeadState, id: &str) -> anyhow::Result<Vec<TimelineTurn>> {
    let mut t = st.catchup.turns.lock().unwrap().get(id).cloned().unwrap_or_default();
    let (last, before) = (st.store.last_seq(id).await?, t.read);
    while t.read < last {
        let page = st.store.events_after(id, t.read, 1000).await?;
        let Some(end) = page.last().map(|e| e.seq) else { break };
        for e in page.iter().filter_map(crate::convert::event) {
            t.feed(&e);
        }
        t.read = end;
    }
    let list = t.list.clone();
    if t.read != before {
        remember(&st.catchup.turns, id.to_owned(), t);
    }
    Ok(list)
}

/// The session's turns (`hour`: those that started in the hour from then,
/// each with its line, made now unless it is already), whatever its
/// switches say: the glance's outline is always there.
pub async fn get(st: &HeadState, id: &str, q: &api::TimelineQuery, caller: &Caller) -> anyhow::Result<Vec<TimelineTurn>> {
    let row = visible(st, id, caller).await?;
    let mut list = turns(st, id).await?;
    if let Some(h) = q.hour {
        list.retain(|t| t.at >= h && t.at < h + HOUR);
        // Off until the timeline is redesigned: no model call, even for a
        // page from before.
        if LINES {
            make_lines(st, &row, &mut list).await;
        } else {
            known_lines(st, &row.id, &mut list);
        }
    } else {
        known_lines(st, &row.id, &mut list);
    }
    Ok(list)
}

fn key(session: &str, t: &TimelineTurn) -> (String, u64, u64) {
    (session.to_owned(), t.start_seq, t.end_seq)
}

/// The lines made already; which turns that ended have none.
fn known_lines(st: &HeadState, session: &str, list: &mut [TimelineTurn]) -> Vec<usize> {
    let lines = st.catchup.lines.lock().unwrap();
    let mut missing = Vec::new();
    for (i, t) in list.iter_mut().enumerate() {
        match lines.get(&key(session, t)) {
            Some(l) => t.summary = api::citations::clean(l).into_owned(),
            None if t.end_seq > 0 => missing.push(i),
            None => {}
        }
    }
    missing
}

/// Gives each ended turn of `list` its line: made already, else by one
/// model call for all that have none.
async fn make_lines(st: &HeadState, row: &SessionRow, list: &mut [TimelineTurn]) {
    if known_lines(st, &row.id, list).is_empty() {
        return;
    }
    let lock = st.catchup.making(&row.id);
    let _one = lock.lock().await;
    let missing = known_lines(st, &row.id, list);
    if missing.is_empty() {
        return;
    }
    let budget = MAX_INPUT_CHARS / missing.len();
    let mut parts = Vec::new();
    for &i in &missing {
        let t = &list[i];
        let events = match st
            .store
            .events_after(&row.id, t.start_seq.saturating_sub(1), (t.end_seq + 1 - t.start_seq).min(TURN_EVENTS) as i64)
            .await
        {
            Ok(evs) => evs.iter().filter_map(crate::convert::event).collect::<Vec<_>>(),
            Err(e) => {
                warn!(session = %row.id, "reading a turn for its line: {e:#}");
                return;
            }
        };
        let part = described(t, &events);
        parts.push(if part.len() > budget { format!("{}…", clip(&part, budget)) } else { part });
    }
    let template = crate::prompts::get(st, "session.timeline", &row.realm).await;
    let prompt = api::prompts::fill(&template, &[("title", &row.title), ("turns", &parts.join("\n\n"))]);
    match crate::catchup::ask(st, row, ROUTE_LABEL, "timeline", prompt, 2048).await {
        Ok(text) => {
            let got = parse_lines(&text);
            for &i in &missing {
                let line = got.get(&list[i].turn).cloned().unwrap_or_default();
                remember(&st.catchup.lines, key(&row.id, &list[i]), line.clone());
                list[i].summary = line;
            }
        }
        Err(e) => warn!(session = %row.id, "a timeline's lines: {e:#}"),
    }
}

/// A turn as the model reads it: who started it, its tool calls (never
/// their output) and its final reply.
fn described(t: &TimelineTurn, events: &[Event]) -> String {
    let started = match t.by.as_str() {
        "you" => format!("the user: \"{}\"", t.words),
        "session" => format!("a message from the session \"{}\": \"{}\"", t.from, t.words),
        "task" => format!("a background task ended: {}", t.words),
        "doc" => format!("comments on the document \"{}\"", t.from),
        "agent" => format!("the agent that started it: \"{}\"", t.words),
        _ => "nothing said".to_owned(),
    };
    let mut out = format!("Turn {} (started by {started}):\n", t.turn);
    let mut tools = Vec::new();
    let mut reply = String::new();
    for e in events {
        if let EventKind::Assistant { blocks, .. } = &e.kind {
            for b in blocks {
                if let ContentBlock::ToolUse { name, input_json, .. } = b {
                    let input = serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null);
                    let what = target(name, &input);
                    tools.push(if what.is_empty() { format!("- {name}") } else { format!("- {name} {what}") });
                }
            }
            let text = text_of(blocks);
            if !text.trim().is_empty() {
                reply = text;
            }
        }
    }
    let more = tools.len().saturating_sub(TURN_TOOLS);
    for l in tools.into_iter().take(TURN_TOOLS) {
        out.push_str(&l);
        out.push('\n');
    }
    if more > 0 {
        out.push_str(&format!("- (and {more} more tool calls)\n"));
    }
    if !reply.is_empty() {
        out.push_str(&format!("Final reply: {}\n", clip(&reply, 300)));
    }
    out
}

/// The model's lines, by turn: `<turn>: <line>`.
pub(crate) fn parse_lines(text: &str) -> HashMap<u64, String> {
    let mut out = HashMap::new();
    for l in text.lines() {
        let l = l.trim().trim_start_matches(['-', '*', ' ']);
        let l = l.strip_prefix("Turn ").or_else(|| l.strip_prefix("turn ")).unwrap_or(l);
        let digits: String = l.chars().take_while(char::is_ascii_digit).collect();
        let Ok(n) = digits.parse::<u64>() else { continue };
        let rest = l[digits.len()..].trim_start_matches([':', '.', ')', ' ', '—', '-']).trim();
        let rest = rest.trim_matches(|c| matches!(c, '"' | '“' | '”' | '`')).trim_end_matches('.').trim();
        if !rest.is_empty() {
            out.insert(n, clip(rest, 120));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catchup::tests::{card_answer, ev, from_session, notice, reply, result, user};
    use serde_json::json;

    #[test]
    fn card_answers_start_and_attribute_turns_as_user_messages() {
        let mut t = Turns::default();
        for e in [ev(1, 0, card_answer()), ev(2, 20, EventKind::TurnStarted { turn: 1, context_window: 0 })] {
            t.feed(&e);
        }
        assert_eq!(
            (t.list[0].start_seq, t.list[0].by.as_str(), t.list[0].words.as_str()),
            (1, "you", "Not now: task card #185 \"Cleanup\".")
        );
        let mut t = Turns::default();
        t.feed(&ev(1, 0, EventKind::TurnStarted { turn: 1, context_window: 0 }));
        t.feed(&ev(2, 10, card_answer()));
        assert_eq!(t.list[0].by, "you", "a card answer can join the running turn");
    }

    #[test]
    fn turns_say_who_started_them_and_how_many_steps() {
        let evs = vec![
            ev(1, 0, user("fix the pager, it skips a line")),
            ev(2, 10, EventKind::TurnStarted { turn: 1, context_window: 0 }),
            ev(3, 11, reply(1, "", &[("c1", "Edit", json!({"file_path": "a.rs"})), ("c2", "Bash", json!({"command": "make"}))])),
            ev(4, 12, result("c1", "ok", false)),
            ev(5, 13, result("c2", "ok", false)),
            ev(6, 14, reply(1, "Fixed the pager. It skipped the last line.", &[])),
            ev(7, 15, EventKind::TurnEnded { turn: 1, reason: api::TurnEndReason::Completed }),
            ev(8, 20, from_session("Release prep", "Is it in?")),
            ev(9, 21, EventKind::TurnStarted { turn: 2, context_window: 0 }),
            ev(10, 22, reply(2, "Yes.", &[])),
            ev(11, 23, EventKind::TurnEnded { turn: 2, reason: api::TurnEndReason::Completed }),
            ev(12, 30, notice("<background-task id=\"b1\">\nCommand: npm test\n</background-task>")),
            ev(13, 31, EventKind::TurnStarted { turn: 3, context_window: 0 }),
            ev(14, 32, reply(3, "", &[("c3", "Read", json!({"file_path": "b.rs"}))])),
        ];
        let mut t = Turns::default();
        for e in &evs {
            t.feed(e);
        }
        let got: Vec<(u64, u64, u64, &str, &str, &str, u64)> =
            t.list.iter().map(|t| (t.turn, t.start_seq, t.end_seq, t.by.as_str(), t.from.as_str(), t.words.as_str(), t.steps)).collect();
        assert_eq!(
            got,
            [
                (1, 1, 7, "you", "", "fix the pager, it skips a line", 3),
                (2, 8, 11, "session", "Release prep", "Is it in?", 1),
                (3, 12, 0, "task", "", "npm test", 1),
            ]
        );
        assert_eq!(t.list[0].reply, "Fixed the pager.");
        assert_eq!(t.list[0].at, 10);
        // Read on later: the running turn ends.
        t.feed(&ev(15, 40, EventKind::TurnEnded { turn: 3, reason: api::TurnEndReason::Completed }));
        assert_eq!(t.list[2].end_seq, 15);
    }

    #[test]
    fn reads_the_model_s_lines() {
        let got = parse_lines("12: Fixed the pager's off-by-one.\nTurn 13 — \"Answered a question\"\n- 14) Ran the tests\nnothing here");
        assert_eq!(got.get(&12).map(String::as_str), Some("Fixed the pager's off-by-one"));
        assert_eq!(got.get(&13).map(String::as_str), Some("Answered a question"));
        assert_eq!(got.get(&14).map(String::as_str), Some("Ran the tests"));
        assert_eq!(got.len(), 3);
        assert_eq!(first_sentence("## Done! All green."), "Done!");
    }
}
