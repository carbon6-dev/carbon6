//! Catching up on a session: what it did since you last looked, counted
//! (never by the model) and in a paragraph a small model writes, for the
//! catch-up card. Opt-in per session (`/api/v1/sessions/{id}/view`, with
//! folding finished turns); the paragraph is made when you look, not per
//! turn, by the model a `task:catchup` routing rule picks (else the
//! session's own), and kept in the head's memory.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use c6_api::{self as api, CatchupItem, CatchupRow, ContentBlock, Event, EventKind};
use c6_core::now_ms;
use c6_llm::{Accumulator, Block, Message, ModelRequest, Role};
use futures::StreamExt;
use tracing::warn;

use crate::HeadState;
use crate::realms::Caller;
use crate::store::SessionRow;

/// The routing label a rule names to pick the catch-up's model.
pub const ROUTE_LABEL: &str = "task:catchup";
/// Steps since you looked worth a card (anything waiting on you is, too).
pub const THRESHOLD: u64 = 5;
/// What the model is given of what happened at most: about 6,000 tokens,
/// at about 4 characters each.
pub const MAX_INPUT_CHARS: usize = 24_000;
/// Events read back at most (older ones are left out, even uncounted).
const MAX_EVENTS: u64 = 20_000;
/// Entries a cache keeps before it starts over.
const CACHE_LIMIT: usize = 2_000;

/// What the head keeps in memory of catch-ups and timelines: made when
/// looked at, kept until it restarts.
#[derive(Default)]
pub struct Cache {
    /// Catch-up paragraphs, by (session, since, last event).
    summaries: Mutex<HashMap<(String, u64, u64), String>>,
    /// Each session's turns, as far as they were read.
    pub(crate) turns: Mutex<HashMap<String, crate::timeline::Turns>>,
    /// Turns' lines, by (session, first event, last event).
    pub(crate) lines: Mutex<HashMap<(String, u64, u64), String>>,
    /// One model call at a time per session: two devices looking at once
    /// ask once.
    making: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Cache {
    /// The lock that keeps a session's model calls one at a time.
    pub(crate) fn making(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.making.lock().unwrap();
        if m.len() > CACHE_LIMIT {
            m.retain(|_, l| Arc::strong_count(l) > 1);
        }
        m.entry(session.to_owned()).or_default().clone()
    }
}

/// Puts `v` in a cache, which starts over once it holds too much.
pub(crate) fn remember<K: std::hash::Hash + Eq, V>(cache: &Mutex<HashMap<K, V>>, k: K, v: V) {
    let mut c = cache.lock().unwrap();
    if c.len() >= CACHE_LIMIT {
        c.clear();
    }
    c.insert(k, v);
}

/// How the session shows itself to you, if the caller may see it.
pub async fn view(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<api::SessionViewSettings> {
    visible(st, id, caller).await?;
    st.store.session_view(id).await
}

pub async fn set_view(st: &HeadState, id: &str, v: &api::SessionViewSettings, caller: &Caller) -> anyhow::Result<()> {
    visible(st, id, caller).await?;
    st.store.set_session_view(id, v).await
}

pub(crate) async fn visible(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<SessionRow> {
    match st.store.session(id).await? {
        Some(row) if caller.sees(&row.realm) => Ok(row),
        _ => Err(crate::not_found("no such session")),
    }
}

/// What the session did since you last looked (or since event `since`;
/// `looked_at`: when the page last had it on screen); None while its
/// catch-up is off.
pub async fn get(st: &HeadState, id: &str, q: &api::CatchupQuery, caller: &Caller) -> anyhow::Result<Option<api::Catchup>> {
    let row = visible(st, id, caller).await?;
    if !st.store.session_view(id).await?.catchup {
        return Ok(None);
    }
    let last = st.store.last_seq(id).await?;
    let (since, since_at, events) = match q.since {
        Some(s) => {
            let s = s.min(last);
            let at = st.store.events_after(id, s.saturating_sub(1), 1).await?.first().filter(|e| e.seq == s).map_or(0, |e| e.ts_ms);
            (s, at, read(st, id, s, last).await?)
        }
        None => {
            // What you last looked at: the last finished turn you saw, or
            // when you last had it on screen (a turn still running), never
            // later than now.
            let (_, seen_at) = st.store.turn_seen(id).await?;
            let seen_at = seen_at.max(q.looked_at.unwrap_or(0).min(now_ms()));
            let seen = if seen_at > 0 { st.store.seq_at(id, seen_at).await? } else { 0 };
            let after = read(st, id, seen, last).await?;
            let (s, at) = since(&after, seen, seen_at);
            (s, at, after.into_iter().filter(|e| e.seq > s).collect())
        }
    };
    let mut waiting = open_asks(&events);
    let approvals = waiting.iter().filter(|w| w.text.starts_with("Approve")).count() as i64;
    if row.awaiting > approvals {
        waiting.push(CatchupItem { text: "A tool call waits for your approval".into(), ..Default::default() });
    }
    for d in crate::decisions::of_session(st, id, caller).await?.into_iter().filter(|d| d.kind != "task") {
        waiting.push(CatchupItem { text: d.title, ..Default::default() });
    }
    let steps = events.iter().map(|e| e.kind.steps()).sum();
    let offer = worth_a_card(steps, waiting.len());
    let rows = rows(&events, waiting);
    let mut out = api::Catchup { since_seq: since, since_at, last_seq: last, steps, offer, rows, summary: String::new() };
    if q.summary && offer {
        out.summary = summary(st, &row, &out, &events).await;
    }
    Ok(Some(out))
}

/// The events after `after`, up to `last` (the newest [`MAX_EVENTS`] at most).
async fn read(st: &HeadState, id: &str, after: u64, last: u64) -> anyhow::Result<Vec<Event>> {
    let mut at = after.max(last.saturating_sub(MAX_EVENTS));
    let mut out = Vec::new();
    while at < last {
        let page = st.store.events_after(id, at, 1000).await?;
        let Some(end) = page.last() else { break };
        at = end.seq;
        out.extend(page.iter().filter_map(crate::convert::event));
    }
    Ok(out)
}

/// A card is worth it for [`THRESHOLD`] steps or more, or anything
/// waiting on you.
pub(crate) fn worth_a_card(steps: u64, waiting: usize) -> bool {
    steps >= THRESHOLD || waiting > 0
}

/// Your own message to the session: not another session's, a task's, a
/// document's comments or the agent's that started it.
fn yours(kind: &EventKind) -> bool {
    matches!(kind, EventKind::User { text, command, from_session_id, delegated_by, .. }
        if from_session_id.is_empty() && delegated_by.is_empty() && command != "task-notification" && command != "stop-hook" && doc_comment(text).is_none())
}

/// Where a catch-up starts, and when: after `seen`, the last event before
/// you last looked (at `seen_at`), or your last message after it (the
/// events `after_seen`), whichever is later.
pub(crate) fn since(after_seen: &[Event], seen: u64, seen_at: i64) -> (u64, i64) {
    after_seen.iter().rev().find(|e| e.seq > seen && yours(&e.kind)).map_or((seen, seen_at), |e| (e.seq, e.ts))
}

/// Approvals and questions asked and not answered yet.
fn open_asks(events: &[Event]) -> Vec<CatchupItem> {
    let settled: HashSet<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Approval { call_id, state, .. } if state != "requested" => Some(call_id.as_str()),
            EventKind::Question { call_id, state, .. } if state != "asked" => Some(call_id.as_str()),
            EventKind::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Approval { call_id, state, tool, .. } if state == "requested" && !settled.contains(call_id.as_str()) => {
                Some(CatchupItem { text: format!("Approve {tool}"), seq: e.seq, call: String::new(), failed: false })
            }
            EventKind::Question { call_id, state, title, .. } if state == "asked" && !settled.contains(call_id.as_str()) => {
                Some(CatchupItem { text: format!("Answer: {title}"), seq: e.seq, call: String::new(), failed: false })
            }
            _ => None,
        })
        .collect()
}

/// A tool call: its event, id, name and input.
struct Call<'a> {
    seq: u64,
    id: &'a str,
    name: &'a str,
    input: serde_json::Value,
}

fn calls(events: &[Event]) -> Vec<Call<'_>> {
    events
        .iter()
        .flat_map(|e| match &e.kind {
            EventKind::Assistant { blocks, .. } => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input_json } => {
                        Some(Call { seq: e.seq, id, name, input: serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null) })
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// A shell tool's command (Claude Code's Bash, carbon6's bash and shell).
fn command_of(name: &str, input: &serde_json::Value) -> Option<String> {
    if !matches!(name, "Bash" | "bash" | "shell") {
        return None;
    }
    Some(match input.get("command") {
        Some(serde_json::Value::Array(argv)) => {
            let argv: Vec<&str> = argv.iter().filter_map(|a| a.as_str()).collect();
            if argv.len() == 3 && argv[1].ends_with('c') { argv[2].to_owned() } else { argv.join(" ") }
        }
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    })
}

/// A command, in a line: what its call says it does (Claude Code's Bash
/// has a `description`), else its first line without a leading
/// `cd <dir> &&` (or `;`).
fn command_words(input: &serde_json::Value, cmd: &str) -> String {
    if let Some(d) = input.get("description").and_then(|d| d.as_str()).filter(|d| !d.trim().is_empty()) {
        return clip(d, 120);
    }
    let mut line = cmd.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    while let Some(after) = line.strip_prefix("cd ") {
        let cut = [after.find("&&").map(|i| i + 2), after.find(';').map(|i| i + 1)].into_iter().flatten().min();
        match cut.map(|i| after[i..].trim_start()) {
            Some(rest) if !rest.is_empty() => line = rest,
            _ => break,
        }
    }
    clip(line, 120)
}

/// The files an editing tool changes (write, edit, a patch).
fn paths_of(name: &str, input: &serde_json::Value) -> Vec<String> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).filter(|p| !p.is_empty()).map(str::to_owned);
    match name {
        "Write" | "write" | "Edit" | "edit" | "MultiEdit" | "NotebookEdit" => {
            s("file_path").or_else(|| s("path")).or_else(|| s("notebook_path")).into_iter().collect()
        }
        "apply_patch" => s("input")
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "]
                    .iter()
                    .find_map(|p| l.strip_prefix(p))
                    .map(|p| p.trim().to_owned())
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// What a tool call is about, in a line: a file, the head of a command, a
/// pattern (never what it returned).
pub(crate) fn target(name: &str, input: &serde_json::Value) -> String {
    if let Some(c) = command_of(name, input) {
        return clip(c.lines().next().unwrap_or(""), 120);
    }
    let paths = paths_of(name, input);
    if !paths.is_empty() {
        return paths.join(", ");
    }
    for k in ["file_path", "path", "pattern", "url", "query", "description", "title", "to"] {
        if let Some(v) = input.get(k).and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty()) {
            return clip(v.lines().next().unwrap_or(""), 120);
        }
    }
    String::new()
}

/// The first `n` characters of a text, on one line, with … if cut.
pub(crate) fn clip(text: &str, n: usize) -> String {
    let text = api::citations::clean(text);
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        return one;
    }
    let cut: String = one.chars().take(n).collect();
    format!("{}…", cut.trim_end())
}

/// The document a message of comments is about: its title (as the
/// `docs.comment` prompt says it, `… on the document "<title>" (doc:…`).
pub(crate) fn doc_comment(text: &str) -> Option<String> {
    let head: String = text.chars().take(400).collect();
    let start = head.find("on the document \"")? + "on the document \"".len();
    let end = head[start..].find("\" (doc:")?;
    Some(head[start..start + end].to_owned())
}

/// "1 file", "3 files".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The counted rows: waiting on you (`waiting`), what finished, what
/// changed (files, commits), commands run, and messages and comments
/// delivered; each only when it isn't zero.
pub(crate) fn rows(events: &[Event], waiting: Vec<CatchupItem>) -> Vec<CatchupRow> {
    let failed: HashMap<&str, bool> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { call_id, is_error, .. } => Some((call_id.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    let calls = calls(events);
    let mut out = Vec::new();
    if !waiting.is_empty() {
        out.push(CatchupRow { kind: "waiting".into(), text: format!("{} waiting on you", waiting.len()), items: waiting });
    }

    // Background tasks and subagents that ended, each once (as it ended).
    // A Claude Code task still in the foreground as it ended was a command
    // (counted with the commands) or a subagent (counted by its call).
    let mut tasks: Vec<(&str, &str, &Event)> = Vec::new(); // id, first description, last event
    let mut at: HashMap<&str, usize> = HashMap::new();
    for e in events {
        if let EventKind::BackgroundTask { task_id, description, .. } = &e.kind {
            match at.get(task_id.as_str()) {
                Some(&i) => {
                    if tasks[i].1.is_empty() {
                        tasks[i].1 = description;
                    }
                    tasks[i].2 = e;
                }
                None => {
                    at.insert(task_id, tasks.len());
                    tasks.push((task_id, description, e));
                }
            }
        }
    }
    let mut finished = Vec::new();
    // What tasks counted here (or as commands) were called: a subagent's
    // call by the same description is the same work.
    let mut theirs: HashSet<&str> = HashSet::new();
    for (_, description, e) in &tasks {
        let EventKind::BackgroundTask { kind, native_type, foreground, state, exit_code, .. } = &e.kind else { continue };
        let agent = kind == "agent" || native_type.ends_with("_agent");
        if *foreground && agent {
            continue;
        }
        theirs.insert(description);
        if *foreground || state == "started" {
            continue;
        }
        let bad = matches!(state.as_str(), "failed" | "lost") || (state == "finished" && kind != "claude_code" && *exit_code != 0);
        let what = if agent { format!("Subagent: {description}") } else { format!("Task: {description}") };
        finished.push(CatchupItem { text: what, seq: e.seq, call: String::new(), failed: bad });
    }
    let shell: HashSet<&str> = calls.iter().filter(|c| command_of(c.name, &c.input).is_some()).map(|c| c.id).collect();
    for e in events {
        // One that ran in the foreground ends with its call's result. Older
        // workers also mirrored Claude Code shell commands: not subagents.
        if let EventKind::SubagentSpawned { call_id, description, background_task_id, .. } = &e.kind
            && background_task_id.is_empty()
            && !shell.contains(call_id.as_str())
            && !theirs.contains(description.as_str())
            && let Some(bad) = failed.get(call_id.as_str())
        {
            finished.push(CatchupItem { text: format!("Subagent: {description}"), seq: e.seq, call: call_id.clone(), failed: *bad });
        }
    }
    finished.sort_by_key(|f| f.seq);
    if !finished.is_empty() {
        let bad = finished.iter().filter(|f| f.failed).count();
        let mut text = format!("{} finished", finished.len());
        if bad > 0 {
            text.push_str(&format!(" · {bad} failed"));
        }
        out.push(CatchupRow { kind: "finished".into(), text, items: finished });
    }

    // Files changed (the last change to each) and commits.
    let mut files: Vec<CatchupItem> = Vec::new();
    let mut commits = Vec::new();
    let mut commands = Vec::new();
    for c in &calls {
        let bad = failed.get(c.id).copied().unwrap_or(false);
        for p in paths_of(c.name, &c.input) {
            if bad {
                continue;
            }
            files.retain(|f| f.text != p);
            files.push(CatchupItem { text: p, seq: c.seq, call: c.id.to_owned(), failed: false });
        }
        if let Some(cmd) = command_of(c.name, &c.input) {
            let head = command_words(&c.input, &cmd);
            if !bad && cmd.contains("git commit") {
                commits.push(CatchupItem {
                    text: commit_message(&cmd).unwrap_or_else(|| head.clone()),
                    seq: c.seq,
                    call: c.id.to_owned(),
                    failed: false,
                });
            }
            commands.push(CatchupItem { text: head, seq: c.seq, call: c.id.to_owned(), failed: bad });
        }
    }
    if !files.is_empty() || !commits.is_empty() {
        let mut parts = Vec::new();
        if !files.is_empty() {
            parts.push(format!("{} changed", count(files.len(), "file", "files")));
        }
        if !commits.is_empty() {
            parts.push(count(commits.len(), "commit", "commits"));
        }
        let items =
            files.into_iter().chain(commits.into_iter().map(|c| CatchupItem { text: format!("Commit: {}", c.text), ..c })).collect();
        out.push(CatchupRow { kind: "changes".into(), text: parts.join(" · "), items });
    }
    if !commands.is_empty() {
        let bad = commands.iter().filter(|c| c.failed).count();
        let mut text = format!("{} run", count(commands.len(), "command", "commands"));
        if bad > 0 {
            text.push_str(&format!(" · {bad} failed"));
        }
        out.push(CatchupRow { kind: "commands".into(), text, items: commands });
    }

    // Messages from other sessions, and comments on documents.
    let (mut messages, mut comments) = (0, 0);
    let mut items = Vec::new();
    for e in events {
        if let EventKind::User { text, from_session_id, from_title, .. } = &e.kind {
            if !from_session_id.is_empty() {
                messages += 1;
                let from = if from_title.is_empty() { "another session" } else { from_title };
                items.push(CatchupItem { text: format!("From {from}: {}", clip(text, 80)), seq: e.seq, ..Default::default() });
            } else if let Some(doc) = doc_comment(text) {
                comments += 1;
                items.push(CatchupItem { text: format!("Comments on {doc}"), seq: e.seq, ..Default::default() });
            }
        }
    }
    if !items.is_empty() {
        let mut parts = Vec::new();
        if messages > 0 {
            parts.push(count(messages, "message", "messages"));
        }
        if comments > 0 {
            parts.push(count(comments, "doc comment", "doc comments"));
        }
        out.push(CatchupRow { kind: "messages".into(), text: format!("{} delivered", parts.join(" · ")), items });
    }
    out
}

/// A commit's message, from `git commit -m "…"`.
fn commit_message(cmd: &str) -> Option<String> {
    let at = cmd.find("git commit")?;
    let rest = &cmd[at..];
    let m = rest.find(" -m ").map(|i| i + 4).or_else(|| rest.find(" -am ").map(|i| i + 5))?;
    let rest = rest[m..].trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = &rest[1..];
    let end = body.find(quote).unwrap_or(body.len());
    Some(clip(&body[..end], 100))
}

/// What happened since, condensed for the model: your messages and other
/// sessions' (the first 300 characters), each turn's final reply (500),
/// and a line per tool call (never its output). The newest that fit in
/// [`MAX_INPUT_CHARS`], oldest first; what's left out is only counted.
pub(crate) fn condensed(events: &[Event]) -> String {
    // Each turn's final reply: its last with text.
    let mut finals: HashMap<u64, u64> = HashMap::new();
    for e in events {
        if let EventKind::Assistant { turn, blocks, .. } = &e.kind
            && blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if !text.trim().is_empty()))
        {
            finals.insert(*turn, e.seq);
        }
    }
    let failed: HashMap<&str, bool> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { call_id, is_error, .. } => Some((call_id.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    let mut lines: Vec<String> = Vec::new();
    for e in events {
        match &e.kind {
            EventKind::User { text, from_session_id, from_title, command, .. } => {
                if !from_session_id.is_empty() {
                    lines.push(format!("Message from session \"{from_title}\": {}", clip(text, 300)));
                } else if let Some(doc) = doc_comment(text) {
                    lines.push(format!("Comments on the document \"{doc}\": {}", clip(text, 300)));
                } else if yours(&e.kind) {
                    lines.push(format!("User: {}", clip(text, 300)));
                } else if command == "task-notification" {
                    lines.push(format!("(The agent was told: {})", clip(&task_words(text), 120)));
                }
            }
            EventKind::Assistant { turn, blocks, .. } => {
                if finals.get(turn) == Some(&e.seq) {
                    let text: String = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    lines.push(format!("Agent: {}", clip(&text, 500)));
                }
                for b in blocks {
                    if let ContentBlock::ToolUse { id, name, input_json } = b {
                        let input = serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null);
                        let bad = if failed.get(id.as_str()).copied().unwrap_or(false) { " (failed)" } else { "" };
                        let t = target(name, &input);
                        lines.push(if t.is_empty() { format!("- {name}{bad}") } else { format!("- {name} {t}{bad}") });
                    }
                }
            }
            EventKind::TurnEnded { reason, .. } if *reason != api::TurnEndReason::Completed => {
                lines.push(format!("(The turn ended: {reason:?})"));
            }
            _ => {}
        }
    }
    let mut kept = Vec::new();
    let mut size = 0;
    for l in lines.iter().rev() {
        if size + l.len() + 1 > MAX_INPUT_CHARS {
            break;
        }
        size += l.len() + 1;
        kept.push(l.as_str());
    }
    let left = lines.len() - kept.len();
    kept.reverse();
    let mut out = String::new();
    if left > 0 {
        out.push_str(&format!("({left} earlier lines left out; the counts include them.)\n"));
    }
    out.push_str(&kept.join("\n"));
    out
}

/// What a task's notice says it was: its `Task:` or `Command:` line, else
/// its first line that isn't a tag.
pub(crate) fn task_words(text: &str) -> String {
    let lines = || text.lines().map(str::trim).filter(|l| !l.is_empty());
    lines()
        .find_map(|l| l.strip_prefix("Task: ").or_else(|| l.strip_prefix("Command: ")))
        .or_else(|| lines().find(|l| !l.starts_with('<')))
        .unwrap_or("")
        .to_owned()
}

/// "42 minutes ago".
pub(crate) fn ago(ms: i64) -> String {
    let m = ms.max(0) / 60_000;
    match m {
        0 => "a moment ago".into(),
        1 => "a minute ago".into(),
        m if m < 60 => format!("{m} minutes ago"),
        m if m < 120 => "an hour ago".into(),
        m if m < 48 * 60 => format!("{} hours ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

/// The paragraph: cached, else made now (empty when it can't be).
async fn summary(st: &HeadState, row: &SessionRow, c: &api::Catchup, events: &[Event]) -> String {
    let key = (row.id.clone(), c.since_seq, c.last_seq);
    if let Some(s) = st.catchup.summaries.lock().unwrap().get(&key) {
        return s.clone();
    }
    let lock = st.catchup.making(&row.id);
    let _one = lock.lock().await;
    if let Some(s) = st.catchup.summaries.lock().unwrap().get(&key) {
        return s.clone();
    }
    let template = crate::prompts::get(st, "session.catchup", &row.realm).await;
    let counts: String = c.rows.iter().map(|r| format!("- {}\n", r.text)).collect();
    let counts = if counts.is_empty() { format!("- {}\n", count(c.steps as usize, "step", "steps")) } else { counts };
    let since = if c.since_at > 0 { ago(now_ms() - c.since_at) } else { "never (this is the whole session)".to_owned() };
    let prompt = api::prompts::fill(
        &template,
        &[("title", &row.title), ("since", &since), ("counts", counts.trim_end()), ("events", &condensed(events))],
    );
    match ask(st, row, ROUTE_LABEL, "catch-up", prompt, 2048).await {
        Ok(text) => {
            let text = clip(&text, 1200);
            remember(&st.catchup.summaries, key, text.clone());
            text
        }
        Err(e) => {
            warn!(session = %row.id, "catching up: {e:#}");
            String::new()
        }
    }
}

/// Asks the model a routing rule for `label` picks (else the session's
/// own) for text; its tokens count toward the session.
pub(crate) async fn ask(st: &HeadState, row: &SessionRow, label: &str, what: &str, prompt: String, max: u32) -> anyhow::Result<String> {
    if let Some(why) = crate::usage::blocked(st, &row.all_labels(), &row.realm).await? {
        anyhow::bail!("over budget: {why}");
    }
    // Its own rule, else the one for suggested next messages (a small
    // model you already chose), else the session's own model.
    let (connection, model) = match crate::suggestions::rule_for(st, row, label).await? {
        Some(m) => m,
        None => crate::suggestions::model_for(st, row, crate::suggestions::ROUTE_LABEL).await?,
    };
    anyhow::ensure!(!connection.is_empty(), "the session has no connection to ask");
    // Not Claude Code: names and catch-ups come with every session, and
    // would spend the Claude plan's limits.
    let kind = st.store.connections().await?.into_iter().find(|c| c.id == connection).map(|c| c.kind);
    anyhow::ensure!(
        kind.as_deref() != Some("claude_code"),
        "a Claude Code session needs a model for this: add a {label} or {} rule with an API connection in Settings → Model routing",
        crate::suggestions::ROUTE_LABEL
    );
    let origin = c6_llm::activity::Origin {
        what: what.into(),
        about: row.title.clone(),
        connection: connection.clone(),
        realm: row.realm.clone(),
        via: String::new(),
    };
    let client = crate::pools::client(st, &connection, &row.realm, origin).await?;
    let req = ModelRequest {
        model: model.clone(),
        system: String::new(),
        messages: vec![Message { role: Role::User, blocks: vec![Block::Text(prompt)] }],
        tools: Vec::new(),
        // Room for a thinking model's reasoning.
        max_output_tokens: Some(max),
        cache_key: None,
        effort: Some("low".into()),
        web_search: false,
        json: false,
    };
    let mut stream = client.stream(&req).await?;
    let mut acc = Accumulator::default();
    while let Some(ev) = stream.next().await {
        acc.push(&ev?);
    }
    let usage = acc.usage;
    let msg = acc.finish(true);
    // Its tokens count toward the session (a row of its own, below zero).
    sqlx::query(
        "INSERT OR REPLACE INTO usage (session_id, seq, ts, connection_id, model, input, output, cache_read, cache_write, realm) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.id)
    .bind(-now_ms())
    .bind(now_ms())
    .bind(&connection)
    .bind(&model)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_read_tokens as i64)
    .bind(usage.cache_write_tokens as i64)
    .bind(&row.realm)
    .execute(&st.store.pool)
    .await?;
    let text: String = msg.blocks.iter().filter_map(Block::text).collect();
    anyhow::ensure!(!text.trim().is_empty(), "{model} gave no answer");
    Ok(api::citations::clean(text.trim()).into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn catchup_previews_remove_citations_before_clipping() {
        assert_eq!(clip("Fact \u{e200}cite\u{e202}turn347046search1\u{e201}. More", 6), "Fact.…");
    }

    pub(crate) fn ev(seq: u64, ts: i64, kind: EventKind) -> Event {
        Event { seq, ts, kind }
    }

    pub(crate) fn user(text: &str) -> EventKind {
        EventKind::User {
            command_id: String::new(),
            text: text.into(),
            attachments: Vec::new(),
            command: String::new(),
            prompt: String::new(),
            blocked: String::new(),
            delegated_by: String::new(),
            wake: false,
            from_session_id: String::new(),
            from_title: String::new(),
        }
    }

    pub(crate) fn from_session(title: &str, text: &str) -> EventKind {
        let mut k = user(text);
        if let EventKind::User { from_session_id, from_title, .. } = &mut k {
            (*from_session_id, *from_title) = ("other".into(), title.into());
        }
        k
    }

    pub(crate) fn notice(text: &str) -> EventKind {
        let mut k = user(text);
        if let EventKind::User { command, .. } = &mut k {
            *command = "task-notification".into();
        }
        k
    }

    pub(crate) fn reply(turn: u64, text: &str, tools: &[(&str, &str, serde_json::Value)]) -> EventKind {
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text: text.into(), citations: vec![] });
        }
        for (id, name, input) in tools {
            blocks.push(ContentBlock::ToolUse { id: (*id).into(), name: (*name).into(), input_json: input.to_string() });
        }
        EventKind::Assistant { turn, blocks, stop_reason: String::new() }
    }

    pub(crate) fn result(call: &str, output: &str, is_error: bool) -> EventKind {
        EventKind::ToolResult {
            call_id: call.into(),
            tool: String::new(),
            output: output.into(),
            is_error,
            truncated: false,
            duration_ms: 1,
            images: 0,
        }
    }

    fn task(description: &str, state: &str, exit_code: i32) -> EventKind {
        EventKind::BackgroundTask {
            task_id: "t".into(),
            kind: "shell".into(),
            description: description.into(),
            state: state.into(),
            exit_code,
            elapsed_ms: 10,
            notify: true,
            native_type: String::new(),
            foreground: false,
        }
    }

    /// A Claude Code task (`native_type`: the CLI's `local_bash`,
    /// `local_agent`), in the foreground or not.
    fn native(id: &str, native_type: &str, description: &str, state: &str, foreground: bool) -> EventKind {
        EventKind::BackgroundTask {
            task_id: id.into(),
            kind: "claude_code".into(),
            description: description.into(),
            state: state.into(),
            exit_code: 0,
            elapsed_ms: 10,
            notify: false,
            native_type: native_type.into(),
            foreground,
        }
    }

    fn spawned(call: &str, description: &str) -> EventKind {
        EventKind::SubagentSpawned {
            call_id: call.into(),
            child_session_id: format!("child-{call}"),
            agent: String::new(),
            description: description.into(),
            background_task_id: String::new(),
            connection_id: String::new(),
            model: String::new(),
            effort: String::new(),
            remote: None,
        }
    }

    /// A morning's work: your message, edits and commands (one failed),
    /// a commit, a task that failed, another session's message and a
    /// question still open.
    fn fixture() -> Vec<Event> {
        use serde_json::json;
        vec![
            ev(10, 1000, user("fix the pager and commit")),
            ev(11, 1001, EventKind::TurnStarted { turn: 3, context_window: 0 }),
            ev(
                12,
                1002,
                reply(
                    3,
                    "Looking.",
                    &[
                        ("c1", "Edit", json!({"file_path": "src/pager.rs", "old_string": "a", "new_string": "b"})),
                        ("c2", "Write", json!({"file_path": "src/footer.rs", "content": "x"})),
                    ],
                ),
            ),
            ev(13, 1003, result("c1", "ok", false)),
            ev(14, 1004, result("c2", "ok", false)),
            ev(15, 1005, reply(3, "", &[("c3", "Bash", json!({"command": "cargo test"}))])),
            ev(16, 1006, result("c3", "test failed: SECRET_OUTPUT", true)),
            ev(17, 1007, reply(3, "", &[("c4", "Edit", json!({"file_path": "src/pager.rs", "old_string": "b", "new_string": "c"}))])),
            ev(18, 1008, result("c4", "ok", false)),
            ev(19, 1009, reply(3, "", &[("c5", "shell", json!({"command": ["bash", "-lc", "git commit -am \"Fix the pager\""]}))])),
            ev(20, 1010, result("c5", "[main abc] Fix the pager", false)),
            ev(21, 1011, reply(3, "Fixed the pager and committed it.", &[])),
            ev(22, 1012, EventKind::TurnEnded { turn: 3, reason: api::TurnEndReason::Completed }),
            ev(23, 1013, task("npm run e2e", "finished", 2)),
            ev(24, 1014, from_session("Release prep", "Is the pager fix in?")),
            ev(
                25,
                1015,
                EventKind::Question {
                    call_id: "q1".into(),
                    title: "Which branch?".into(),
                    fields: Vec::new(),
                    state: "asked".into(),
                    answer: serde_json::Value::Null,
                },
            ),
        ]
    }

    #[test]
    fn since_is_the_later_of_what_you_saw_and_your_last_message() {
        let evs = fixture();
        // You saw up to event 9 (at 900); your message at 10 is later.
        assert_eq!(since(&evs, 9, 900), (10, 1000));
        // You looked after it (at event 22): what you saw is later.
        let after: Vec<Event> = evs.iter().filter(|e| e.seq > 22).cloned().collect();
        assert_eq!(since(&after, 22, 1012), (22, 1012));
        // Another session's message, a task's notice or doc comments aren't yours.
        let others = vec![
            ev(30, 3000, from_session("Release prep", "hi")),
            ev(31, 3001, notice("<background-task>\nCommand: make\n</background-task>")),
            ev(32, 3002, user("A comment on the document \"Plan\" (doc:d1) is for you. Thread t1")),
        ];
        assert_eq!(since(&others, 29, 2900), (29, 2900));
    }

    #[test]
    fn rows_count_what_it_did() {
        let evs = fixture();
        let waiting = open_asks(&evs);
        let rows = rows(&evs, waiting);
        let texts: Vec<(&str, &str)> = rows.iter().map(|r| (r.kind.as_str(), r.text.as_str())).collect();
        assert_eq!(
            texts,
            [
                ("waiting", "1 waiting on you"),
                ("finished", "1 finished · 1 failed"),
                ("changes", "2 files changed · 1 commit"),
                ("commands", "2 commands run · 1 failed"),
                ("messages", "1 message delivered"),
            ]
        );
        let changes = &rows[2].items;
        let files: Vec<(&str, u64)> = changes.iter().map(|i| (i.text.as_str(), i.seq)).collect();
        // The last change to each file, then the commit.
        assert_eq!(files, [("src/footer.rs", 12), ("src/pager.rs", 17), ("Commit: Fix the pager", 19)]);
        assert_eq!(rows[0].items[0].text, "Answer: Which branch?");
        assert!(rows[3].items[0].failed && !rows[3].items[1].failed);
        // Nothing: no rows.
        assert!(super::rows(&[], Vec::new()).is_empty());
    }

    /// Claude Code's own tasks: a command in the foreground is a command
    /// (not finished work, even if an older worker mirrored it as a subagent),
    /// each task counts once, as it ended, and only agents are subagents.
    #[test]
    fn finished_is_background_work_each_once() {
        use serde_json::json;
        let bash = json!({"command": "cd /home/me/.local/state/c6w/worktrees/x && cargo test", "description": "Run the tests"});
        let evs = vec![
            // An older worker's foreground command: started, mirrored, ended.
            ev(1, 1, reply(1, "", &[("c1", "Bash", bash)])),
            ev(2, 2, native("claude:fg", "local_bash", "Run the tests", "started", true)),
            ev(3, 3, spawned("c1", "Run the tests")),
            ev(4, 4, native("claude:fg", "local_bash", "Run the tests", "finished", true)),
            ev(5, 5, result("c1", "ok", false)),
            // A subagent in the background: its call returns at once.
            ev(6, 6, reply(1, "", &[("c2", "Agent", json!({"description": "Review it", "prompt": "Review."}))])),
            ev(7, 7, native("claude:bg", "local_agent", "Review it", "started", false)),
            ev(8, 8, spawned("c2", "Review it")),
            ev(9, 9, result("c2", "Async agent launched", false)),
            // A subagent in the foreground: counted by its call.
            ev(10, 10, reply(1, "", &[("c3", "Agent", json!({"description": "Plan it", "prompt": "Plan."}))])),
            ev(11, 11, native("claude:plan", "local_agent", "Plan it", "started", true)),
            ev(12, 12, spawned("c3", "Plan it")),
            ev(13, 13, native("claude:plan", "local_agent", "Plan it", "finished", true)),
            ev(14, 14, result("c3", "Planned.", false)),
            // A command moved to the background, then ended.
            ev(15, 15, native("claude:build", "local_bash", "Long build", "started", true)),
            ev(16, 16, native("claude:build", "local_bash", "Long build", "started", false)),
            ev(17, 17, native("claude:build", "local_bash", "Long build", "failed", false)),
            ev(18, 18, native("claude:bg", "local_agent", "Review it", "finished", false)),
            // One of carbon6's own, and one still running.
            ev(19, 19, task("npm run e2e", "finished", 2)),
            ev(20, 20, native("claude:later", "local_bash", "Watch the logs", "started", false)),
        ];
        let rows = rows(&evs, Vec::new());
        let finished = rows.iter().find(|r| r.kind == "finished").unwrap();
        assert_eq!(finished.text, "4 finished · 2 failed");
        let items: Vec<(&str, u64, &str, bool)> =
            finished.items.iter().map(|i| (i.text.as_str(), i.seq, i.call.as_str(), i.failed)).collect();
        assert_eq!(
            items,
            [
                ("Subagent: Plan it", 12, "c3", false),
                ("Task: Long build", 17, "", true),
                ("Subagent: Review it", 18, "", false),
                ("Task: npm run e2e", 19, "", true),
            ]
        );
        let commands = rows.iter().find(|r| r.kind == "commands").unwrap();
        assert_eq!((commands.text.as_str(), commands.items[0].text.as_str()), ("1 command run", "Run the tests"));
    }

    /// A command reads as what its call says it does, else its first line
    /// without the `cd` before it.
    #[test]
    fn commands_read_as_what_they_do() {
        use serde_json::json;
        let words = |input: serde_json::Value| {
            let cmd = command_of("Bash", &input).unwrap();
            command_words(&input, &cmd)
        };
        assert_eq!(words(json!({"command": "cd /a/b && make check", "description": "Run the checks"})), "Run the checks");
        assert_eq!(words(json!({"command": "cd /a/b && make check", "description": " "})), "make check");
        assert_eq!(words(json!({"command": "cd /a/b; cd c && cargo test -q\necho done"})), "cargo test -q");
        assert_eq!(words(json!({"command": "\n  git status"})), "git status");
        assert_eq!(words(json!({"command": "cd /a/b"})), "cd /a/b");
        assert_eq!(words(json!({"command": "cd /a/b &&"})), "cd /a/b &&");
        // carbon6's shell, as argv.
        let input = json!({"command": ["bash", "-lc", "cd /w && npm test"]});
        assert_eq!(command_words(&input, &command_of("shell", &input).unwrap()), "npm test");
        // The commands row says them so.
        let evs = vec![
            ev(1, 1, reply(1, "", &[("c1", "Bash", json!({"command": "cd /home/me/x && cargo build", "description": "Build it"}))])),
            ev(2, 2, reply(1, "", &[("c2", "Bash", json!({"command": "cd /home/me/x && git log -1"}))])),
        ];
        let rows = rows(&evs, Vec::new());
        let texts: Vec<&str> = rows[0].items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, ["Build it", "git log -1"]);
    }

    #[test]
    fn a_card_for_five_steps_or_something_waiting() {
        let evs = fixture();
        let steps: u64 = evs.iter().map(|e| e.kind.steps()).sum();
        // Five tool calls and a reply that called none ("Looking." came with two).
        assert_eq!(steps, 6);
        assert!(worth_a_card(steps, 0));
        assert!(!worth_a_card(4, 0));
        assert!(worth_a_card(0, 1));
        assert!(!worth_a_card(0, 0));
    }

    #[test]
    fn the_model_input_is_capped_and_has_no_tool_output() {
        use serde_json::json;
        let text = condensed(&fixture());
        assert!(!text.contains("SECRET_OUTPUT"), "{text}");
        assert!(text.contains("User: fix the pager and commit"), "{text}");
        assert!(text.contains("- Bash cargo test (failed)"), "{text}");
        assert!(text.contains("Agent: Fixed the pager and committed it."), "{text}");
        assert!(!text.contains("Agent: Looking."), "only a turn's final reply: {text}");
        assert!(text.contains("Message from session \"Release prep\""), "{text}");

        // A long day: only the newest fit; the rest is counted.
        let mut evs = Vec::new();
        for i in 0..4000u64 {
            let call = format!("c{i}");
            evs.push(ev(
                2 * i + 1,
                i as i64,
                reply(i, &"x".repeat(900), &[(call.as_str(), "Bash", json!({"command": format!("step {i}")}))]),
            ));
            evs.push(ev(2 * i + 2, i as i64, result(&call, &"OUTPUT ".repeat(1000), false)));
        }
        let text = condensed(&evs);
        assert!(text.len() <= MAX_INPUT_CHARS + 100, "{}", text.len());
        assert!(text.contains("- Bash step 3999"), "the newest are kept");
        assert!(!text.contains("- Bash step 0\n"), "the oldest are only counted");
        assert!(text.starts_with('(') && text.contains("earlier lines left out"), "{}", &text[..80]);
        assert!(!text.contains("OUTPUT"));
        // A reply is cut at 500 characters.
        assert!(text.lines().filter(|l| l.starts_with("Agent:")).all(|l| l.chars().count() <= 510));
    }

    #[test]
    fn finds_what_a_message_is() {
        assert_eq!(doc_comment("2 comments on the document \"Q3 plan\" (doc:abc) come to you together."), Some("Q3 plan".into()));
        assert_eq!(doc_comment("fix the document"), None);
        assert_eq!(task_words("<background-task id=\"b\">\nCommand: npm test\n</background-task>"), "npm test");
        assert_eq!(commit_message("git add -A && git commit -m 'Tidy up' && git push"), Some("Tidy up".into()));
        assert_eq!(ago(42 * 60_000), "42 minutes ago");
    }
}
