//! Importing sessions from Claude Code and Codex (docs/imports.md): this
//! worker reads their session files (`~/.claude/projects/**/*.jsonl`,
//! `~/.codex/sessions/**/*.jsonl`), converts each into carbon6 session
//! events, and sends the head the ones it doesn't have yet. The head keeps
//! them as sessions; continuing one is sending it a message.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use c6_proto::session_event::Kind;
use c6_proto::{
    AssistantMessage, ContentBlock, ModelSpec, SessionEvent, SessionStarted, ToolResult, ToolUse, TurnEndReason, TurnEnded, TurnStarted,
    Usage, UserMessage, content_block,
};
use prost::Message;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::Worker;

/// A session read from a file: where it came from, and its events.
#[derive(Debug, Default)]
pub struct Imported {
    /// `claude_code` or `codex`.
    pub source: &'static str,
    /// The tool's own id for it.
    pub id: String,
    pub cwd: String,
    pub branch: String,
    pub title: String,
    pub model: String,
    pub started_at: i64,
    pub events: Vec<SessionEvent>,
}

/// Longest tool output kept.
const MAX_OUTPUT: usize = 30_000;

fn ms(ts: &Value) -> i64 {
    ts.as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|d| d.timestamp_millis()).unwrap_or(0)
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} more bytes]", &s[..end], s.len() - end)
}

/// Builds the events of a conversation, turn by turn.
#[derive(Default)]
struct Builder {
    events: Vec<SessionEvent>,
    ts: i64,
    turn: u64,
    open: bool,
    /// The assistant message being gathered: its id, blocks, usage.
    current: Option<(String, Vec<ContentBlock>, Option<Usage>)>,
    /// Tool names by call id (results name their tool).
    tools: HashMap<String, String>,
    model: String,
}

impl Builder {
    fn push(&mut self, kind: Kind) {
        let seq = self.events.len() as u64 + 1;
        self.events.push(SessionEvent { seq, ts_ms: self.ts, kind: Some(kind) });
    }

    fn flush(&mut self) {
        if let Some((_, blocks, usage)) = self.current.take() {
            if !blocks.is_empty() {
                self.ensure_turn();
                self.push(Kind::Assistant(AssistantMessage { turn: self.turn, blocks, stop_reason: String::new() }));
            }
            if let Some(u) = usage {
                self.push(Kind::Usage(u));
            }
        }
    }

    fn ensure_turn(&mut self) {
        if !self.open {
            self.turn += 1;
            self.open = true;
            self.push(Kind::TurnStarted(TurnStarted { turn: self.turn }));
        }
    }

    fn end_turn(&mut self) {
        self.flush();
        if self.open {
            self.push(Kind::TurnEnded(TurnEnded { turn: self.turn, reason: TurnEndReason::Completed as i32 }));
            self.open = false;
        }
    }

    fn user(&mut self, text: String) {
        self.end_turn();
        self.push(Kind::User(UserMessage { command_id: format!("import-{}", self.events.len() + 1), text, ..Default::default() }));
        self.ensure_turn();
    }

    /// Adds a block to the assistant message `id` (a new id starts another).
    fn block(&mut self, id: &str, b: content_block::Block) {
        if self.current.as_ref().is_some_and(|(cur, _, _)| cur != id) {
            self.flush();
        }
        if let content_block::Block::ToolUse(t) = &b {
            self.tools.insert(t.id.clone(), t.name.clone());
        }
        self.current.get_or_insert_with(|| (id.to_owned(), Vec::new(), None)).1.push(ContentBlock { block: Some(b) });
    }

    fn usage(&mut self, id: &str, u: Usage) {
        if self.current.as_ref().is_some_and(|(cur, _, _)| cur != id) {
            self.flush();
        }
        self.current.get_or_insert_with(|| (id.to_owned(), Vec::new(), None)).2 = Some(u);
    }

    fn result(&mut self, call_id: &str, output: String, is_error: bool) {
        self.flush();
        self.ensure_turn();
        let tool = self.tools.get(call_id).cloned().unwrap_or_default();
        let truncated = output.len() > MAX_OUTPUT;
        self.push(Kind::ToolResult(ToolResult {
            call_id: call_id.to_owned(),
            tool,
            output: clip(&output, MAX_OUTPUT),
            is_error,
            truncated,
            ..Default::default()
        }));
    }

    /// The events, after a Started that says where and what.
    fn finish(mut self, cwd: &str, branch: &str, title: &str, source: &str) -> Vec<SessionEvent> {
        self.end_turn();
        let started = SessionEvent {
            seq: 0,
            ts_ms: self.events.first().map(|e| e.ts_ms).unwrap_or(0),
            kind: Some(Kind::Started(SessionStarted {
                command_id: format!("import-{source}"),
                model: Some(ModelSpec { model: self.model.clone(), ..Default::default() }),
                cwd: cwd.to_owned(),
                worktree_branch: branch.to_owned(),
                title: title.to_owned(),
                ..Default::default()
            })),
        };
        let mut out = vec![started];
        out.extend(self.events);
        for (i, e) in out.iter_mut().enumerate() {
            e.seq = i as u64 + 1;
        }
        out
    }
}

/// A Claude Code tool result's text.
fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// A Claude Code session (`~/.claude/projects/<folder>/<id>.jsonl`).
pub fn claude_code(text: &str) -> Imported {
    let mut b = Builder::default();
    let (mut id, mut cwd, mut branch, mut title, mut first_prompt) =
        (String::new(), String::new(), String::new(), String::new(), String::new());
    let mut started_at = 0;
    for line in text.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
        if d["isSidechain"] == true {
            continue;
        }
        if id.is_empty()
            && let Some(s) = d["sessionId"].as_str()
        {
            id = s.to_owned();
        }
        if cwd.is_empty()
            && let Some(c) = d["cwd"].as_str()
        {
            cwd = c.to_owned();
        }
        if branch.is_empty()
            && let Some(g) = d["gitBranch"].as_str()
        {
            branch = g.to_owned();
        }
        let t = ms(&d["timestamp"]);
        if t > 0 {
            b.ts = t;
            if started_at == 0 {
                started_at = t;
            }
        }
        match d["type"].as_str().unwrap_or_default() {
            "ai-title" => title = d["aiTitle"].as_str().unwrap_or_default().to_owned(),
            "summary" if title.is_empty() => title = d["summary"].as_str().unwrap_or_default().to_owned(),
            "user" if d["isMeta"] != true => {
                let m = &d["message"];
                match &m["content"] {
                    Value::String(s) if !s.trim().is_empty() => {
                        if first_prompt.is_empty() {
                            first_prompt = s.clone();
                        }
                        b.user(s.clone());
                    }
                    Value::Array(items) => {
                        let texts: Vec<&str> = items
                            .iter()
                            .filter(|x| x["type"] == "text")
                            .filter_map(|x| x["text"].as_str())
                            .filter(|s| !s.trim().is_empty())
                            .collect();
                        for r in items.iter().filter(|x| x["type"] == "tool_result") {
                            b.result(r["tool_use_id"].as_str().unwrap_or_default(), result_text(&r["content"]), r["is_error"] == true);
                        }
                        if !texts.is_empty() {
                            let s = texts.join("\n");
                            if first_prompt.is_empty() {
                                first_prompt = s.clone();
                            }
                            b.user(s);
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                let m = &d["message"];
                let mid = m["id"].as_str().unwrap_or_default().to_owned();
                if let Some(model) = m["model"].as_str().filter(|x| !x.starts_with('<')) {
                    b.model = model.to_owned();
                }
                for c in m["content"].as_array().into_iter().flatten() {
                    let block = match c["type"].as_str().unwrap_or_default() {
                        "text" => c["text"].as_str().filter(|s| !s.is_empty()).map(|s| content_block::Block::Text(s.to_owned())),
                        "thinking" => {
                            c["thinking"].as_str().filter(|s| !s.is_empty()).map(|s| content_block::Block::Thinking(s.to_owned()))
                        }
                        "tool_use" => Some(content_block::Block::ToolUse(ToolUse {
                            id: c["id"].as_str().unwrap_or_default().to_owned(),
                            name: c["name"].as_str().unwrap_or_default().to_owned(),
                            input_json: c["input"].to_string(),
                        })),
                        _ => None,
                    };
                    if let Some(bl) = block {
                        b.block(&mid, bl);
                    }
                }
                let u = &m["usage"];
                if u.is_object() {
                    let n = |k: &str| u[k].as_u64().unwrap_or(0);
                    let usage = Usage {
                        model: b.model.clone(),
                        input_tokens: n("input_tokens"),
                        output_tokens: n("output_tokens"),
                        cache_read_tokens: n("cache_read_input_tokens"),
                        cache_write_tokens: n("cache_creation_input_tokens"),
                        reasoning_tokens: 0,
                    };
                    b.usage(&mid, usage);
                }
            }
            _ => {}
        }
    }
    let title = if title.is_empty() { first_line(&first_prompt) } else { title };
    let model = b.model.clone();
    let events = b.finish(&cwd, &branch, &title, "claude_code");
    Imported { source: "claude_code", id, cwd, branch, title, model, started_at, events }
}

fn first_line(s: &str) -> String {
    clip(s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim(), 80)
}

/// What Codex puts in a user message that you didn't write.
fn injected(text: &str) -> bool {
    let t = text.trim_start();
    ["<environment_context", "<user_instructions", "# AGENTS.md", "<permissions", "<turn_aborted", "<user_shell_command"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// A Codex session (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`).
pub fn codex(text: &str) -> Imported {
    let mut b = Builder::default();
    let (mut id, mut cwd, mut first_prompt) = (String::new(), String::new(), String::new());
    let mut started_at = 0;
    // Each assistant step's items share an id here (its position).
    let mut step = 0u64;
    for line in text.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
        let t = ms(&d["timestamp"]);
        if t > 0 {
            b.ts = t;
            if started_at == 0 {
                started_at = t;
            }
        }
        let p = &d["payload"];
        match (d["type"].as_str().unwrap_or_default(), p["type"].as_str().unwrap_or_default()) {
            ("session_meta", _) => {
                id = p["id"].as_str().or(p["session_id"].as_str()).unwrap_or_default().to_owned();
                cwd = p["cwd"].as_str().unwrap_or_default().to_owned();
            }
            ("turn_context", _) => {
                if let Some(m) = p["model"].as_str() {
                    b.model = m.to_owned();
                }
                if cwd.is_empty() {
                    cwd = p["cwd"].as_str().unwrap_or_default().to_owned();
                }
            }
            ("response_item", "message") => {
                let texts: Vec<String> =
                    p["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str().map(str::to_owned)).collect();
                match p["role"].as_str().unwrap_or_default() {
                    "user" => {
                        let mine: Vec<String> = texts.into_iter().filter(|t| !injected(t) && !t.trim().is_empty()).collect();
                        if !mine.is_empty() {
                            let s = mine.join("\n");
                            if first_prompt.is_empty() {
                                first_prompt = s.clone();
                            }
                            step += 1;
                            b.user(s);
                        }
                    }
                    "assistant" => {
                        for t in texts.into_iter().filter(|t| !t.is_empty()) {
                            b.block(&step.to_string(), content_block::Block::Text(t));
                        }
                    }
                    _ => {}
                }
            }
            ("response_item", "reasoning") => {
                let summary: Vec<&str> = p["summary"].as_array().into_iter().flatten().filter_map(|s| s["text"].as_str()).collect();
                if !summary.is_empty() {
                    b.block(&step.to_string(), content_block::Block::Thinking(summary.join("\n")));
                }
            }
            ("response_item", "function_call" | "custom_tool_call" | "local_shell_call") => {
                let input = match p["type"].as_str() {
                    Some("function_call") => p["arguments"].as_str().map(str::to_owned).unwrap_or_else(|| p["arguments"].to_string()),
                    Some("custom_tool_call") => json!({"input": p["input"]}).to_string(),
                    _ => p["action"].to_string(),
                };
                b.block(
                    &step.to_string(),
                    content_block::Block::ToolUse(ToolUse {
                        id: p["call_id"].as_str().unwrap_or_default().to_owned(),
                        name: p["name"].as_str().unwrap_or("shell").to_owned(),
                        input_json: input,
                    }),
                );
            }
            ("response_item", "function_call_output" | "custom_tool_call_output") => {
                let o = &p["output"];
                let text = match o {
                    Value::String(s) => s.clone(),
                    v => v["content"].as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()),
                };
                b.result(p["call_id"].as_str().unwrap_or_default(), text, false);
                // Results end a step: what follows is the next.
                step += 1;
            }
            ("event_msg", "token_count") => {
                let u = &p["info"]["last_token_usage"];
                if u.is_object() {
                    let n = |k: &str| u[k].as_u64().unwrap_or(0);
                    let usage = Usage {
                        model: b.model.clone(),
                        input_tokens: n("input_tokens").saturating_sub(n("cached_input_tokens")),
                        output_tokens: n("output_tokens"),
                        cache_read_tokens: n("cached_input_tokens"),
                        cache_write_tokens: 0,
                        reasoning_tokens: n("reasoning_output_tokens"),
                    };
                    b.usage(&step.to_string(), usage);
                    b.flush();
                    step += 1;
                }
            }
            ("event_msg", "task_complete") => b.end_turn(),
            _ => {}
        }
    }
    let title = first_line(&first_prompt);
    let model = b.model.clone();
    let events = b.finish(&cwd, "", &title, "codex");
    Imported { source: "codex", id, cwd, branch: String::new(), title, model, started_at, events }
}

/// Where each tool keeps its sessions, under a home folder.
fn files(home: &Path, sources: &[String]) -> Vec<(&'static str, PathBuf)> {
    let mut out = Vec::new();
    let mut walk = |source: &'static str, root: PathBuf| {
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "jsonl") {
                    out.push((source, p));
                }
            }
        }
    };
    if sources.iter().any(|s| s == "claude_code") {
        walk("claude_code", home.join(".claude/projects"));
    }
    if sources.iter().any(|s| s == "codex") {
        walk("codex", home.join(".codex/sessions"));
        walk("codex", home.join(".codex/archived_sessions"));
    }
    out
}

/// Events sent in one call.
const CHUNK: usize = 400;

/// How often to look, how long a file must be unchanged before it is read
/// (it is still being written), and whose home to look in. Tests set
/// C6_IMPORT_EVERY_MS, C6_IMPORT_SETTLE_MS and C6_IMPORT_HOME.
fn timing() -> (Duration, Duration, Option<PathBuf>) {
    let env_ms = |k: &str, d: u64| Duration::from_millis(std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d));
    (
        env_ms("C6_IMPORT_EVERY_MS", 60_000),
        env_ms("C6_IMPORT_SETTLE_MS", 120_000),
        std::env::var_os("C6_IMPORT_HOME").map(PathBuf::from).or_else(dirs::home_dir),
    )
}

/// Looks every minute (or when asked) for sessions to import, while the
/// head says this worker imports.
pub fn spawn(worker: Arc<Worker>, stop: CancellationToken) {
    tokio::spawn(async move {
        // What was sent, by file: (size, modified).
        let mut seen: HashMap<PathBuf, (u64, std::time::SystemTime)> = HashMap::new();
        let (every, settling, home) = timing();
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
            if !worker.link.is_connected() {
                continue;
            }
            let config = match worker.link.call("imports.config", json!({})).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            if config["enabled"] != true {
                continue;
            }
            let sources: Vec<String> = serde_json::from_value(config["sources"].clone()).unwrap_or_default();
            let Some(home) = home.clone() else { continue };
            let mut sent = 0;
            for (source, path) in files(&home, &sources) {
                let Ok(meta) = std::fs::metadata(&path) else { continue };
                let (len, modified) = (meta.len(), meta.modified().unwrap_or(std::time::UNIX_EPOCH));
                if seen.get(&path) == Some(&(len, modified)) || modified.elapsed().unwrap_or_default() < settling {
                    continue;
                }
                match sync(&worker, source, &path, &home).await {
                    Ok(n) => {
                        sent += n;
                        seen.insert(path, (len, modified));
                    }
                    Err(e) => warn!(file = %path.display(), "importing a session: {e:#}"),
                }
            }
            if sent > 0 {
                info!(events = sent, "sessions imported");
            }
        }
    });
}

/// Sends the head what it doesn't have of one file's session. How many
/// events.
async fn sync(worker: &Worker, source: &'static str, path: &Path, home: &Path) -> anyhow::Result<usize> {
    let text = tokio::fs::read_to_string(path).await?;
    let s = tokio::task::spawn_blocking(move || if source == "codex" { codex(&text) } else { claude_code(&text) }).await?;
    if s.id.is_empty() || s.events.len() < 3 {
        return Ok(0);
    }
    let st = worker.link.call("imports.status", json!({"source": source, "id": s.id})).await?;
    // Continued in carbon6 since: carbon6's now.
    if st["taken"] == true {
        return Ok(0);
    }
    let known = st["known"].as_u64().unwrap_or(0) as usize;
    if known >= s.events.len() {
        return Ok(0);
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let meta = json!({"cwd": s.cwd, "branch": s.branch, "title": s.title, "model": s.model, "started_at": s.started_at,
                      "home": home.display().to_string(), "file": path.display().to_string()});
    let mut n = 0;
    for chunk in s.events[known..].chunks(CHUNK) {
        let events: Vec<String> = chunk.iter().map(|e| b64.encode(e.encode_to_vec())).collect();
        worker.link.call("imports.append", json!({"source": source, "id": s.id, "meta": meta, "events": events})).await?;
        n += chunk.len();
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(i: &Imported) -> Vec<&'static str> {
        i.events
            .iter()
            .map(|e| match e.kind.as_ref().unwrap() {
                Kind::Started(_) => "started",
                Kind::User(_) => "user",
                Kind::TurnStarted(_) => "turn",
                Kind::Assistant(_) => "assistant",
                Kind::ToolResult(_) => "result",
                Kind::Usage(_) => "usage",
                Kind::TurnEnded(_) => "end",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn a_claude_code_session() {
        let lines = [
            json!({"type": "user", "sessionId": "s1", "cwd": "/w/app", "gitBranch": "main", "timestamp": "2026-10-01T10:00:00Z",
                   "message": {"role": "user", "content": "fix the build"}}),
            json!({"type": "assistant", "timestamp": "2026-10-01T10:00:05Z", "message": {"id": "m1", "model": "claude-opus-5",
                   "content": [{"type": "thinking", "thinking": "look at it"}]}}),
            json!({"type": "assistant", "timestamp": "2026-10-01T10:00:06Z", "message": {"id": "m1", "model": "claude-opus-5",
                   "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "make"}}],
                   "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100}}}),
            json!({"type": "user", "timestamp": "2026-10-01T10:00:09Z", "message": {"role": "user",
                   "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false}]}}),
            json!({"type": "assistant", "isSidechain": true, "message": {"id": "x", "content": [{"type": "text", "text": "side"}]}}),
            json!({"type": "assistant", "timestamp": "2026-10-01T10:00:12Z", "message": {"id": "m2", "content": [{"type": "text", "text": "Fixed."}]}}),
            json!({"type": "ai-title", "aiTitle": "Fixing the build"}),
        ];
        let text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        let i = claude_code(&text);
        assert_eq!(
            (i.id.as_str(), i.cwd.as_str(), i.branch.as_str(), i.title.as_str(), i.model.as_str()),
            ("s1", "/w/app", "main", "Fixing the build", "claude-opus-5")
        );
        assert_eq!(kinds(&i), ["started", "user", "turn", "assistant", "usage", "result", "assistant", "end"]);
        let Some(Kind::ToolResult(r)) = &i.events[5].kind else { panic!() };
        assert_eq!((r.tool.as_str(), r.output.as_str()), ("Bash", "ok"));
        assert!(i.events.iter().all(|e| !format!("{e:?}").contains("side")), "side chains are left out");
        // The same file read again, grown: the same events first.
        let more = format!("{text}\n{}", json!({"type": "user", "message": {"role": "user", "content": "thanks"}}));
        assert_eq!(claude_code(&more).events[..7], i.events[..7]);
    }

    #[test]
    fn a_codex_session() {
        let lines = [
            json!({"type": "session_meta", "timestamp": "2026-10-01T10:00:00Z", "payload": {"id": "c1", "cwd": "/w/svc"}}),
            json!({"type": "turn_context", "payload": {"model": "gpt-6-codex"}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>x</environment_context>"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "add a test"}]}}),
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{\"command\":[\"ls\"]}", "call_id": "f1"}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "f1", "output": "a.rs"}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Added."}]}}),
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 50, "cached_input_tokens": 20, "output_tokens": 7}}}}),
            json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
        ];
        let text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        let i = codex(&text);
        assert_eq!((i.id.as_str(), i.cwd.as_str(), i.title.as_str(), i.model.as_str()), ("c1", "/w/svc", "add a test", "gpt-6-codex"));
        assert_eq!(kinds(&i), ["started", "user", "turn", "assistant", "result", "assistant", "usage", "end"]);
        let Some(Kind::Usage(u)) = &i.events[6].kind else { panic!() };
        assert_eq!((u.input_tokens, u.cache_read_tokens, u.output_tokens), (30, 20, 7));
    }
}

#[cfg(test)]
mod probe {
    /// `C6_IMPORT_PROBE=1`: converts this machine's sessions and prints
    /// counts only (no content).
    #[test]
    fn probe_this_machine() {
        if std::env::var("C6_IMPORT_PROBE").is_err() {
            return;
        }
        let home = dirs::home_dir().unwrap();
        let (mut n, mut empty, mut events, mut biggest) = (0, 0, 0, 0);
        for (source, path) in super::files(&home, &["claude_code".into(), "codex".into()]) {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let i = if source == "codex" { super::codex(&text) } else { super::claude_code(&text) };
            n += 1;
            if i.id.is_empty() || i.events.len() < 3 {
                empty += 1;
            }
            events += i.events.len();
            biggest = biggest.max(i.events.len());
            assert!(i.started_at > 0 || i.events.len() < 3, "{}: no time", path.display());
        }
        eprintln!("files {n}, too small to import {empty}, events {events}, largest {biggest}");
    }
}
