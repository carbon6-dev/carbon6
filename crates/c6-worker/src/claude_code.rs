//! Claude Code backend: each turn runs the official `claude` CLI headless on
//! this worker, signed in with the user's own account, and its stream-json
//! output is mapped onto carbon6 session events. Claude Code runs its own
//! agent loop and tools and keeps its own transcript, resumed by session id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use c6_agent::{EventSink, PendingInput};
use c6_proto::{
    AssistantMessage, ContentBlock, CreateSession, Delegation, ErrorEvent, SessionEvent, SessionStarted, SubagentSpawned, TextDelta,
    ToolResult, ToolUse, TurnEndReason, TurnEnded, TurnStarted, Usage, UserMessage, content_block, ephemeral_event, session_event,
};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Tool profile recorded for Claude Code sessions.
pub const PROFILE: &str = "claude_code";

/// The `claude` binary: `$C6_CLAUDE_BIN`, then `PATH`, then the usual
/// install locations (services often run without the user's PATH).
pub fn find_claude() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(p) = tests::BIN.lock().unwrap().clone() {
        return Some(p);
    }
    if let Some(p) = std::env::var_os("C6_CLAUDE_BIN").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let on_path =
        std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).map(|d| d.join("claude"));
    let home = dirs::home_dir();
    let usual = home.iter().flat_map(|h| [h.join(".local/bin/claude"), h.join(".claude/local/claude")]);
    on_path.chain(usual).chain([PathBuf::from("/opt/homebrew/bin/claude"), PathBuf::from("/usr/local/bin/claude")]).find(|p| p.is_file())
}

/// What Claude Code's own subagents are mirrored into: sessions of their
/// own, read-only, under the session (docs/subagents.md §8).
pub struct Observer {
    pub journal: Arc<crate::journal::Journal>,
    pub link: Arc<crate::link::HeadLink>,
    /// The session running Claude Code, as it was created.
    pub session_id: String,
    pub spec: CreateSession,
    pub started: SessionStarted,
}

/// Runs one Claude Code turn for `inputs`. Inputs that arrive mid-turn are
/// left in the inbox and start the next turn.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    turn: u64,
    session_id: &str,
    cwd: &Path,
    model: &str,
    effort: Option<&str>,
    inputs: Vec<PendingInput>,
    preamble: Option<&str>,
    // Hooks from the head, as Claude Code settings JSON.
    settings: Option<&str>,
    // carbon6's subagents as an MCP server (`--mcp-config` JSON).
    mcp_config: Option<&str>,
    sink: &Arc<dyn EventSink>,
    observer: Option<&Observer>,
    cancel: &CancellationToken,
) -> anyhow::Result<TurnEndReason> {
    let texts: Vec<String> = inputs.iter().map(|i| c6_agent::user_text(i.model_text(), &i.attachments)).collect();
    let prompt: Vec<&str> = preamble.into_iter().chain(texts.iter().map(String::as_str)).collect();
    let prompt = prompt.join("\n\n");
    for input in inputs {
        sink.append(input.event()).await?;
    }
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;

    let reason = match find_claude() {
        None => {
            fail(sink, "Claude Code (`claude`) is not installed on this worker, or not on its PATH. Install it and sign in with `claude auth login`, or set C6_CLAUDE_BIN.".into()).await?
        }
        Some(bin) => {
            // The carbon6 session id doubles as Claude Code's session id.
            let mut reason =
                run_process(&bin, turn, session_id, cwd, model, effort, settings, mcp_config, &prompt, turn > 1, sink, observer, cancel).await?;
            if let Outcome::NoConversation = reason {
                // The first turn never created the conversation (e.g. it failed early).
                reason = run_process(&bin, turn, session_id, cwd, model, effort, settings, mcp_config, &prompt, false, sink, observer, cancel).await?;
            }
            match reason {
                Outcome::Ended(r) => r,
                Outcome::NoConversation => fail(sink, "Claude Code could not find or create this conversation.".into()).await?,
            }
        }
    };
    sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: reason as i32 })).await?;
    Ok(reason)
}

enum Outcome {
    Ended(TurnEndReason),
    /// `--resume` failed because Claude Code has no such conversation yet.
    NoConversation,
}

#[allow(clippy::too_many_arguments)]
async fn run_process(
    bin: &Path,
    turn: u64,
    session_id: &str,
    cwd: &Path,
    model: &str,
    effort: Option<&str>,
    settings: Option<&str>,
    mcp_config: Option<&str>,
    prompt: &str,
    resume: bool,
    sink: &Arc<dyn EventSink>,
    observer: Option<&Observer>,
    cancel: &CancellationToken,
) -> anyhow::Result<Outcome> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--dangerously-skip-permissions"]);
    cmd.args(if resume { ["--resume", session_id] } else { ["--session-id", session_id] });
    if !model.is_empty() && model != "default" {
        cmd.args(["--model", model]);
    }
    if let Some(e) = effort {
        cmd.args(["--effort", e]);
    }
    if let Some(s) = settings {
        cmd.args(["--settings", s]);
    }
    if let Some(m) = mcp_config {
        // A subagent may work for a long time: Claude Code waits for it.
        cmd.args(["--mcp-config", m]).env("MCP_TOOL_TIMEOUT", "86400000");
    }
    // The prompt goes on stdin so it can never be mistaken for a flag.
    cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Ok(Outcome::Ended(fail(sink, format!("failed to start Claude Code: {e}")).await?)),
    };
    let pgid = child.id().map(|id| Pid::from_raw(id as i32));
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(prompt.as_bytes()).await.ok();
        // Dropping stdin closes it: one prompt per process.
    }
    let mut stderr = child.stderr.take().expect("piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    });
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();

    let mut mapper = Mapper { turn, saw_output: false, result: None, observer, calls: HashMap::new(), children: HashMap::new() };
    let interrupted = loop {
        tokio::select! {
            _ = cancel.cancelled() => break true,
            line = lines.next_line() => match line {
                Ok(Some(l)) if !l.trim().is_empty() => {
                    match serde_json::from_str::<Value>(&l) {
                        Ok(v) => mapper.handle(&v, sink).await?,
                        Err(_) => warn!(line = %l.chars().take(200).collect::<String>(), "non-JSON output from claude"),
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break false,
            }
        }
    };

    if interrupted {
        if let Some(pg) = pgid {
            // SIGINT lets Claude Code save its transcript; then make sure.
            let _ = killpg(pg, Signal::SIGINT);
            if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
                let _ = killpg(pg, Signal::SIGKILL);
            }
        }
        let _ = child.wait().await;
        mapper.close_children(TurnEndReason::Interrupted).await?;
        return Ok(Outcome::Ended(TurnEndReason::Interrupted));
    }
    // Subagents whose call never got a result ended with the process.
    mapper
        .close_children(if mapper.result == Some(TurnEndReason::Completed) { TurnEndReason::Completed } else { TurnEndReason::Error })
        .await?;
    let status = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    if let Some(r) = mapper.result {
        return Ok(Outcome::Ended(r));
    }
    if resume && !mapper.saw_output && stderr.to_ascii_lowercase().contains("no conversation found") {
        return Ok(Outcome::NoConversation);
    }
    let detail = stderr.trim().lines().last().unwrap_or("").to_owned();
    Ok(Outcome::Ended(fail(sink, format!("Claude Code exited ({status}) without a result. {detail}")).await?))
}

async fn fail(sink: &Arc<dyn EventSink>, message: String) -> anyhow::Result<TurnEndReason> {
    sink.append(session_event::Kind::Error(ErrorEvent { message })).await?;
    Ok(TurnEndReason::Error)
}

/// A subagent mirrored from the stream: its session, log and depth, and
/// whether its turn is still open.
struct Child {
    id: String,
    log: Arc<dyn EventSink>,
    depth: u32,
    open: bool,
}

/// Maps stream-json lines to session events.
struct Mapper<'a> {
    turn: u64,
    saw_output: bool,
    result: Option<TurnEndReason>,
    observer: Option<&'a Observer>,
    /// Tool calls seen, by id: the subagent they were made in (`None`: the
    /// session itself) and their input.
    calls: HashMap<String, (Option<String>, Value)>,
    /// Mirrored subagents, by the call that started each.
    children: HashMap<String, Child>,
}

impl Mapper<'_> {
    fn note_calls(&mut self, content: &Value, made_in: Option<&str>) {
        for b in content.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use") {
            if let Some(id) = b["id"].as_str() {
                self.calls.insert(id.to_owned(), (made_in.map(str::to_owned), b["input"].clone()));
            }
        }
    }

    /// The subagent started by call `call`, mirrored from now on.
    async fn child(&mut self, call: &str, sink: &Arc<dyn EventSink>) -> anyhow::Result<Option<(String, Arc<dyn EventSink>)>> {
        if let Some(c) = self.children.get(call) {
            return Ok(Some((c.id.clone(), c.log.clone())));
        }
        let Some(obs) = self.observer else { return Ok(None) };
        let (made_in, input) = self.calls.get(call).cloned().unwrap_or((None, Value::Null));
        // Its parent: the session, or the subagent that made the call.
        let (parent_id, parent_log, parent_depth) = match made_in.as_deref().and_then(|m| self.children.values().find(|c| c.id == m)) {
            Some(c) => (c.id.clone(), c.log.clone(), c.depth),
            None => (obs.session_id.clone(), sink.clone(), obs.spec.delegation.as_ref().map_or(0, |d| d.depth)),
        };
        let id = uuid::Uuid::now_v7().to_string();
        let description = input["description"].as_str().unwrap_or("Subagent").to_owned();
        let agent = input["subagent_type"].as_str().filter(|a| *a != "general-purpose").unwrap_or_default().to_owned();
        parent_log
            .append(session_event::Kind::SubagentSpawned(SubagentSpawned {
                call_id: call.to_owned(),
                child_session_id: id.clone(),
                agent: agent.clone(),
                description: description.clone(),
                background_task_id: String::new(),
                model: None,
            }))
            .await?;
        let started = SessionStarted {
            command_id: String::new(),
            title: if agent.is_empty() { description } else { format!("{description} ({agent})") },
            parent_session_id: parent_id.clone(),
            parent_call_id: call.to_owned(),
            system_prompt: "(Claude Code's own subagent)".into(),
            tools: Vec::new(),
            agent,
            depth: parent_depth + 1,
            ..obs.started.clone()
        };
        let root = obs.spec.delegation.as_ref().map_or(obs.session_id.as_str(), |d| d.root_session_id.as_str()).to_owned();
        let spec = CreateSession {
            seed: Vec::new(),
            delegation: Some(Delegation {
                parent_session_id: parent_id,
                parent_call_id: call.to_owned(),
                root_session_id: root,
                agent_json: String::new(),
                depth: parent_depth + 1,
                observed: true,
            }),
            ..obs.spec.clone()
        };
        let ev = SessionEvent { seq: 1, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Started(started)) };
        obs.journal.create_session(&id, &spec, &ev).await?;
        let log: Arc<dyn EventSink> = Arc::new(crate::session::Writer {
            session_id: id.clone(),
            journal: obs.journal.clone(),
            link: obs.link.clone(),
            next_seq: tokio::sync::Mutex::new(2),
        });
        log.append(session_event::Kind::TurnStarted(TurnStarted { turn: 1 })).await?;
        self.children.insert(call.to_owned(), Child { id: id.clone(), log: log.clone(), depth: parent_depth + 1, open: true });
        Ok(Some((id, log)))
    }

    /// The call that started a subagent got its result: its turn is over.
    async fn ended(&mut self, call: &str, is_error: bool) -> anyhow::Result<()> {
        if let Some(c) = self.children.get_mut(call).filter(|c| c.open) {
            c.open = false;
            let reason = if is_error { TurnEndReason::Error } else { TurnEndReason::Completed };
            c.log.append(session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: reason as i32 })).await?;
        }
        Ok(())
    }

    async fn close_children(&mut self, reason: TurnEndReason) -> anyhow::Result<()> {
        for c in self.children.values_mut().filter(|c| c.open) {
            c.open = false;
            c.log.append(session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: reason as i32 })).await?;
        }
        Ok(())
    }

    /// A message of one of Claude Code's subagents, into its mirror.
    async fn handle_child(&mut self, call: &str, v: &Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        let Some((id, log)) = self.child(call, sink).await? else { return Ok(()) };
        match v["type"].as_str().unwrap_or("") {
            "assistant" => {
                self.note_calls(&v["message"]["content"], Some(&id));
                let blocks: Vec<ContentBlock> =
                    v["message"]["content"].as_array().map(|a| a.iter().filter_map(assistant_block).collect()).unwrap_or_default();
                if !blocks.is_empty() {
                    let stop_reason = v["message"]["stop_reason"].as_str().unwrap_or("").to_owned();
                    log.append(session_event::Kind::Assistant(AssistantMessage { turn: 1, blocks, stop_reason })).await?;
                }
            }
            "user" => {
                let content = &v["message"]["content"];
                // Its task (text), or results of its own calls.
                let text = match content {
                    Value::String(s) => s.clone(),
                    Value::Array(parts) => {
                        parts.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
                    }
                    _ => String::new(),
                };
                if !text.trim().is_empty() {
                    log.append(session_event::Kind::User(UserMessage { text, delegated_by: call.to_owned(), ..Default::default() }))
                        .await?;
                }
                for b in content.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result") {
                    let r = tool_result(b);
                    let (call_id, is_error) = (r.call_id.clone(), r.is_error);
                    log.append(session_event::Kind::ToolResult(r)).await?;
                    self.ended(&call_id, is_error).await?;
                }
            }
            _ => {} // its partial deltas: the mirror shows whole messages
        }
        Ok(())
    }

    async fn handle(&mut self, v: &Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        // A subagent's traffic goes to its mirror (its report also comes as
        // the call's tool result).
        if let Some(call) = v["parent_tool_use_id"].as_str() {
            return self.handle_child(call, v, sink).await;
        }
        self.saw_output = true;
        match v["type"].as_str().unwrap_or("") {
            "stream_event" => {
                let ev = &v["event"];
                if ev["type"] == "content_block_delta" {
                    let d = &ev["delta"];
                    match d["type"].as_str() {
                        Some("text_delta") => sink.ephemeral(ephemeral_event::Kind::Text(TextDelta {
                            turn: self.turn,
                            text: d["text"].as_str().unwrap_or("").into(),
                        })),
                        Some("thinking_delta") => sink.ephemeral(ephemeral_event::Kind::Thinking(TextDelta {
                            turn: self.turn,
                            text: d["thinking"].as_str().unwrap_or("").into(),
                        })),
                        _ => {}
                    }
                }
            }
            "assistant" => {
                self.note_calls(&v["message"]["content"], None);
                let blocks: Vec<ContentBlock> =
                    v["message"]["content"].as_array().map(|a| a.iter().filter_map(assistant_block).collect()).unwrap_or_default();
                if !blocks.is_empty() {
                    let stop_reason = v["message"]["stop_reason"].as_str().unwrap_or("").to_owned();
                    sink.append(session_event::Kind::Assistant(AssistantMessage { turn: self.turn, blocks, stop_reason })).await?;
                }
            }
            "user" => {
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    if b["type"] != "tool_result" {
                        continue;
                    }
                    let r = tool_result(b);
                    let (call_id, is_error) = (r.call_id.clone(), r.is_error);
                    sink.append(session_event::Kind::ToolResult(r)).await?;
                    self.ended(&call_id, is_error).await?;
                }
            }
            "result" => {
                let u = &v["usage"];
                sink.append(session_event::Kind::Usage(Usage {
                    model: v["modelUsage"].as_object().and_then(|m| m.keys().next().cloned()).unwrap_or_default(),
                    input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                    cache_read_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                    cache_write_tokens: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                    reasoning_tokens: 0,
                }))
                .await?;
                if v["is_error"].as_bool().unwrap_or(false) {
                    let msg = v["result"].as_str().filter(|s| !s.is_empty()).or(v["subtype"].as_str()).unwrap_or("error").to_owned();
                    sink.append(session_event::Kind::Error(ErrorEvent { message: format!("Claude Code: {msg}") })).await?;
                    self.result = Some(TurnEndReason::Error);
                } else {
                    self.result = Some(TurnEndReason::Completed);
                }
            }
            _ => {} // system/init and future message types
        }
        Ok(())
    }
}

/// A `tool_result` content block as a tool result event.
fn tool_result(b: &Value) -> ToolResult {
    let output = match &b["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    // Images it showed the model (a Read of an image), for the transcript.
    let images: Vec<c6_proto::Attachment> = b["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == "image" && p["source"]["type"] == "base64")
        .filter_map(|p| {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD.decode(p["source"]["data"].as_str()?).ok()?;
            Some(c6_proto::Attachment {
                name: "image".into(),
                mime: p["source"]["media_type"].as_str().unwrap_or("image/png").into(),
                data,
            })
        })
        .collect();
    ToolResult {
        call_id: b["tool_use_id"].as_str().unwrap_or("").into(),
        tool: String::new(),
        output,
        is_error: b["is_error"].as_bool().unwrap_or(false),
        truncated: false,
        duration_ms: 0,
        images,
    }
}

fn assistant_block(b: &Value) -> Option<ContentBlock> {
    let block = match b["type"].as_str()? {
        "text" => content_block::Block::Text(b["text"].as_str()?.to_owned()),
        "thinking" => content_block::Block::Thinking(b["thinking"].as_str().filter(|t| !t.is_empty())?.to_owned()),
        "tool_use" => content_block::Block::ToolUse(ToolUse {
            id: b["id"].as_str()?.to_owned(),
            name: b["name"].as_str()?.to_owned(),
            input_json: b["input"].to_string(),
        }),
        _ => return None,
    };
    Some(ContentBlock { block: Some(block) })
}

#[cfg(test)]
mod tests {
    /// The fake `claude` these tests use (not the process's environment,
    /// which tests running beside them read).
    pub(super) static BIN: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

    use std::sync::Mutex;

    use async_trait::async_trait;
    use c6_proto::SessionEvent;

    use super::*;

    #[derive(Default)]
    struct VecSink(Mutex<Vec<SessionEvent>>);

    #[async_trait]
    impl EventSink for VecSink {
        async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
            let mut v = self.0.lock().unwrap();
            let seq = v.len() as u64 + 1;
            v.push(SessionEvent { seq, ts_ms: 0, kind: Some(kind) });
            Ok(v.len() as u64)
        }
        fn ephemeral(&self, _: ephemeral_event::Kind) {}
    }

    /// A stand-in `claude` that checks its arguments and replays a transcript.
    fn fake_claude(dir: &Path) -> PathBuf {
        let script = r#"#!/bin/sh
case "$*" in *--dangerously-skip-permissions*) ;; *) echo "missing yolo flag" >&2; exit 2;; esac
case "$*" in *"--resume"*) echo "No conversation found with session ID" >&2; exit 1;; esac
prompt=$(cat)
pwd > cwd.txt
while [ $# -gt 0 ]; do [ "$1" = "--settings" ] && printf '%s' "$2" > settings.txt; [ "$1" = "--mcp-config" ] && printf '%s %s' "$2" "$MCP_TOOL_TIMEOUT" > mcp.txt; shift; done
printf '%s' "$prompt" > prompt.txt
cat <<'EOF'
{"type":"system","subtype":"init","session_id":"s","tools":["Bash"],"model":"claude-opus-5-5"}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Listing"}},"parent_tool_use_id":null}
{"type":"assistant","message":{"content":[{"type":"text","text":"Listing files."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}
{"type":"assistant","message":{"content":[{"type":"text","text":"subagent noise"}]},"parent_tool_use_id":"toolu_9"}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a.txt"}]}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"Done.","usage":{"input_tokens":5,"output_tokens":20,"cache_read_input_tokens":900,"cache_creation_input_tokens":100},"modelUsage":{"claude-opus-5-5":{}}}
EOF
"#;
        let path = dir.join("claude");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn maps_a_turn_and_falls_back_from_resume() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path());
        *BIN.lock().unwrap() = Some(bin.clone());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let inputs = vec![PendingInput { command_id: "c1".into(), text: "list files".into(), ..Default::default() }];
        // Turn 2 tries --resume first; the fake has no conversation, so it falls back.
        let reason = run_turn(
            2,
            "0199-session",
            dir.path(),
            "opus",
            Some("high"),
            inputs,
            None,
            Some(r#"{"hooks":{}}"#),
            Some(r#"{"mcpServers":{}}"#),
            &sink,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
        assert_eq!(std::fs::read_to_string(dir.path().join("settings.txt")).unwrap(), r#"{"hooks":{}}"#, "the head's hooks are passed on");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mcp.txt")).unwrap(),
            r#"{"mcpServers":{}} 86400000"#,
            "carbon6's subagents are offered over MCP, with time to finish"
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("prompt.txt")).unwrap(), "list files");

        let events = vec_sink.0.lock().unwrap().clone();
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e.kind.as_ref().unwrap() {
                session_event::Kind::User(_) => "user",
                session_event::Kind::TurnStarted(_) => "turn_started",
                session_event::Kind::Assistant(_) => "assistant",
                session_event::Kind::ToolResult(_) => "tool_result",
                session_event::Kind::Usage(_) => "usage",
                session_event::Kind::TurnEnded(_) => "turn_ended",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "turn_started", "assistant", "tool_result", "assistant", "usage", "turn_ended"]);
        let Some(session_event::Kind::ToolResult(r)) = &events[3].kind else { panic!() };
        assert_eq!((r.call_id.as_str(), r.output.as_str()), ("toolu_1", "a.txt"));
        let Some(session_event::Kind::Usage(u)) = &events[5].kind else { panic!() };
        assert_eq!((u.cache_read_tokens, u.model.as_str()), (900, "claude-opus-5-5"));
    }

    /// A stand-in `claude` whose turn runs a subagent (that runs its own).
    fn fake_claude_with_subagents(dir: &Path) -> PathBuf {
        let script = r#"#!/bin/sh
cat > /dev/null
cat <<'JSON'
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_9","name":"Task","input":{"description":"look around","prompt":"List files.","subagent_type":"explorer"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"List files."}]},"parent_tool_use_id":"toolu_9"}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Look"}},"parent_tool_use_id":"toolu_9"}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_c1","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","id":"toolu_c2","name":"Task","input":{"description":"deeper","prompt":"Count them."}}],"stop_reason":"tool_use"},"parent_tool_use_id":"toolu_9"}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_c1","content":"a.txt"}]},"parent_tool_use_id":"toolu_9"}
{"type":"user","message":{"content":"Count them."},"parent_tool_use_id":"toolu_c2"}
{"type":"assistant","message":{"content":[{"type":"text","text":"one file"}],"stop_reason":"end_turn"},"parent_tool_use_id":"toolu_c2"}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_c2","content":"one file"}]},"parent_tool_use_id":"toolu_9"}
{"type":"assistant","message":{"content":[{"type":"text","text":"a.txt, one file"}],"stop_reason":"end_turn"},"parent_tool_use_id":"toolu_9"}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_9","content":[{"type":"text","text":"a.txt, one file"}]}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"Done.","usage":{"input_tokens":5,"output_tokens":20},"modelUsage":{"claude-opus-5-5":{}}}
JSON
"#;
        let path = dir.join("claude-sub");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn kind(e: &SessionEvent) -> &'static str {
        match e.kind.as_ref().unwrap() {
            session_event::Kind::Started(_) => "started",
            session_event::Kind::User(_) => "user",
            session_event::Kind::TurnStarted(_) => "turn_started",
            session_event::Kind::Assistant(_) => "assistant",
            session_event::Kind::ToolResult(_) => "tool_result",
            session_event::Kind::TurnEnded(_) => "turn_ended",
            session_event::Kind::SubagentSpawned(_) => "subagent_spawned",
            session_event::Kind::Usage(_) => "usage",
            _ => "other",
        }
    }

    /// Claude Code's subagents become read-only sessions of their own, each
    /// under the session that started it, closed when its call returns.
    #[tokio::test]
    async fn mirrors_claude_codes_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude_with_subagents(dir.path());
        let journal = Arc::new(crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let observer = Observer {
            journal: journal.clone(),
            link: Arc::default(),
            session_id: "0199-main".into(),
            spec: CreateSession::default(),
            started: SessionStarted { cwd: dir.path().display().to_string(), tool_profile: PROFILE.into(), ..Default::default() },
        };
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let reason = run_process(
            &bin,
            1,
            "0199-main",
            dir.path(),
            "opus",
            None,
            None,
            None,
            "go",
            false,
            &sink,
            Some(&observer),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(reason, Outcome::Ended(TurnEndReason::Completed)));
        let main = vec_sink.0.lock().unwrap().clone();
        assert_eq!(main.iter().map(kind).collect::<Vec<_>>(), ["assistant", "subagent_spawned", "tool_result", "assistant", "usage"]);
        let Some(session_event::Kind::SubagentSpawned(sp)) = &main[1].kind else { panic!() };
        assert_eq!((sp.call_id.as_str(), sp.description.as_str(), sp.agent.as_str()), ("toolu_9", "look around", "explorer"));

        let (spec, child) = journal.load_session(&sp.child_session_id).await.unwrap().unwrap();
        assert!(spec.delegation.as_ref().is_some_and(|d| d.observed && d.parent_call_id == "toolu_9" && d.depth == 1));
        assert_eq!(
            child.iter().map(kind).collect::<Vec<_>>(),
            ["started", "turn_started", "user", "assistant", "tool_result", "subagent_spawned", "tool_result", "assistant", "turn_ended"]
        );
        let Some(session_event::Kind::Started(st)) = &child[0].kind else { panic!() };
        assert_eq!(
            (st.parent_session_id.as_str(), st.parent_call_id.as_str(), st.title.as_str()),
            ("0199-main", "toolu_9", "look around (explorer)")
        );
        let Some(session_event::Kind::User(u)) = &child[2].kind else { panic!() };
        assert_eq!((u.text.as_str(), u.delegated_by.as_str()), ("List files.", "toolu_9"));
        let Some(session_event::Kind::TurnEnded(t)) = &child[8].kind else { panic!() };
        assert_eq!(t.reason, TurnEndReason::Completed as i32);

        // The subagent's own subagent is under it.
        let Some(session_event::Kind::SubagentSpawned(deeper)) = &child[5].kind else { panic!() };
        let (spec, grand) = journal.load_session(&deeper.child_session_id).await.unwrap().unwrap();
        assert_eq!(spec.delegation.unwrap().parent_session_id, sp.child_session_id);
        assert_eq!(grand.iter().map(kind).collect::<Vec<_>>(), ["started", "turn_started", "user", "assistant", "turn_ended"]);
    }
}
