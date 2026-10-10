//! Claude Code backend: each turn runs the official `claude` CLI headless on
//! this worker, signed in with the user's own account, and its stream-json
//! output is mapped onto carbon6 session events. Claude Code runs its own
//! agent loop and tools and keeps its own transcript, resumed by session id.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use c6_agent::{EventSink, PendingInput};
use c6_proto::{
    AssistantMessage, ContentBlock, ErrorEvent, TextDelta, ToolResult, ToolUse, TurnEndReason, TurnEnded, TurnStarted, Usage, UserMessage,
    content_block, ephemeral_event, session_event,
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
    if let Some(p) = std::env::var_os("C6_CLAUDE_BIN").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let on_path =
        std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).map(|d| d.join("claude"));
    let home = dirs::home_dir();
    let usual = home.iter().flat_map(|h| [h.join(".local/bin/claude"), h.join(".claude/local/claude")]);
    on_path.chain(usual).chain([PathBuf::from("/opt/homebrew/bin/claude"), PathBuf::from("/usr/local/bin/claude")]).find(|p| p.is_file())
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
    sink: &Arc<dyn EventSink>,
    cancel: &CancellationToken,
) -> anyhow::Result<TurnEndReason> {
    let prompt: Vec<&str> = inputs.iter().map(|i| i.text.as_str()).collect();
    let prompt = prompt.join("\n\n");
    for input in inputs {
        sink.append(session_event::Kind::User(UserMessage { command_id: input.command_id, text: input.text })).await?;
    }
    sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;

    let reason = match find_claude() {
        None => {
            fail(sink, "Claude Code (`claude`) is not installed on this worker, or not on its PATH. Install it and sign in with `claude auth login`, or set C6_CLAUDE_BIN.".into()).await?
        }
        Some(bin) => {
            // The carbon6 session id doubles as Claude Code's session id.
            let mut reason = run_process(&bin, turn, session_id, cwd, model, effort, &prompt, turn > 1, sink, cancel).await?;
            if let Outcome::NoConversation = reason {
                // The first turn never created the conversation (e.g. it failed early).
                reason = run_process(&bin, turn, session_id, cwd, model, effort, &prompt, false, sink, cancel).await?;
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
    prompt: &str,
    resume: bool,
    sink: &Arc<dyn EventSink>,
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

    let mut mapper = Mapper { turn, saw_output: false, result: None };
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
        return Ok(Outcome::Ended(TurnEndReason::Interrupted));
    }
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

/// Maps stream-json lines to session events.
struct Mapper {
    turn: u64,
    saw_output: bool,
    result: Option<TurnEndReason>,
}

impl Mapper {
    async fn handle(&mut self, v: &Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        // Subagent traffic is summarized by its Task tool result.
        if !v["parent_tool_use_id"].is_null() {
            return Ok(());
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
                    let output = match &b["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    sink.append(session_event::Kind::ToolResult(ToolResult {
                        call_id: b["tool_use_id"].as_str().unwrap_or("").into(),
                        tool: String::new(),
                        output,
                        is_error: b["is_error"].as_bool().unwrap_or(false),
                        truncated: false,
                        duration_ms: 0,
                    }))
                    .await?;
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
        // SAFETY: tests in this module do not run concurrently with others reading this var.
        unsafe { std::env::set_var("C6_CLAUDE_BIN", &bin) };
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let inputs = vec![PendingInput { command_id: "c1".into(), text: "list files".into() }];
        // Turn 2 tries --resume first; the fake has no conversation, so it falls back.
        let reason = run_turn(2, "0199-session", dir.path(), "opus", Some("high"), inputs, &sink, &CancellationToken::new()).await.unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
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
}
