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

/// What the log shows as a Claude Code session's system prompt.
pub const SYSTEM_PROMPT: &str = "(Claude Code's own system prompt)";

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
    /// The same task manager used by session inspection and the durable sink.
    pub tools: Arc<c6_tools::SessionTools>,
    pub journal: Arc<crate::journal::Journal>,
    pub link: Arc<crate::link::HeadLink>,
    /// The session running Claude Code, as it was created.
    pub session_id: String,
    pub spec: CreateSession,
    pub started: SessionStarted,
}

/// Which Claude Code conversation a turn continues: Claude Code keeps its
/// own history, resumed by id. A session created on Claude Code uses its
/// carbon6 id; a session handed over to Claude Code (a switch from
/// carbon6's own loop) starts a conversation of its own at each handoff.
pub struct ClaudeConversation {
    pub id: String,
    /// It already exists (an earlier turn ran in it): `--resume`.
    pub resume: bool,
    /// What a new conversation is told first, before the inputs: a side
    /// chat's branched-from context, or a handoff's transcript.
    pub preamble: Option<String>,
}

/// How long an idle process with no background tasks stays up, unless
/// the subagent-limits setting says otherwise (docs/background.md §4).
pub const IDLE: Duration = Duration::from_secs(15 * 60);
/// How long an interrupted turn has to end before its process is killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// How long a closing process has to exit before it is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// What a process was started with: a turn that needs something else
/// starts a new one. The model is not here: it is switched in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// The Claude Code conversation it runs.
    pub conversation: String,
    pub effort: Option<String>,
    /// Hooks from the head, as Claude Code settings JSON.
    pub settings: Option<String>,
}

/// Where a session's process takes its input. Shared with the session's
/// actor, which can stop a background task while a turn holds the process.
pub type Stdin = Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>;

/// Writes one stream-json line.
async fn write_line(stdin: &Stdin, v: &Value) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut guard = stdin.lock().await;
    let Some(w) = guard.as_mut() else { return Err(std::io::ErrorKind::BrokenPipe.into()) };
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

/// Asks the process to stop one of its background tasks (the CLI's id).
pub async fn stop_task(stdin: &Stdin, task_id: &str) -> std::io::Result<()> {
    write_line(stdin, &serde_json::json!({"type": "control_request", "request_id": uuid::Uuid::now_v7().to_string(), "request": {"subtype": "stop_task", "task_id": task_id}})).await
}

/// An input written to the process, until Claude Code takes it.
struct Written {
    uuid: String,
    input: PendingInput,
    /// Its `UserMessage` is in the log already (it started the turn).
    recorded: bool,
}

/// How a turn on the process ended.
#[derive(Debug, PartialEq, Eq)]
pub struct TurnEnd {
    /// `None`: no turn started (nothing was logged).
    pub reason: Option<TurnEndReason>,
    /// The process is still up.
    pub alive: bool,
    /// `--resume` failed because Claude Code has no such conversation yet.
    pub no_conversation: bool,
}

/// A session's Claude Code process (docs/background.md §4). It stays up
/// between turns, so input reaches a running turn, background tasks
/// outlive the turn that started them, and a task that ends while the
/// session is idle wakes the agent: the CLI starts a turn of its own.
pub(crate) struct Process {
    child: tokio::process::Child,
    stdin: Stdin,
    /// Its stdout, a JSON value per line; closed when it exits.
    lines: tokio::sync::mpsc::UnboundedReceiver<Value>,
    stderr: Option<tokio::task::JoinHandle<String>>,
    pgid: Option<Pid>,
    launch: Launch,
    /// Started with `--resume`.
    resumed: bool,
    model: String,
    mapper: Mapper,
    /// Inputs written that Claude Code has not taken yet, oldest first.
    unconfirmed: std::collections::VecDeque<Written>,
    /// A line read while idle that begins a turn.
    pending: Option<Value>,
    /// Background-task notifications since the last turn ended.
    notified: Vec<String>,
    /// When its last turn ended (or it started).
    idle_since: tokio::time::Instant,
    registration: Option<crate::mcp_bridge::Registration>,
    _output_root: Option<OutputRoot>,
}

impl Process {
    /// Starts `claude` for a conversation. `stdin` is the session's (see
    /// [`Stdin`]); `mcp` serves carbon6's tools to it for as long as it runs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        bin: &Path,
        launch: Launch,
        resume: bool,
        cwd: &Path,
        model: &str,
        stdin: Stdin,
        mcp: Option<crate::mcp_bridge::Registration>,
        observer: Option<Arc<Observer>>,
    ) -> std::io::Result<Self> {
        let output_root = match OutputRoot::new() {
            Ok(root) => Some(root),
            Err(e) => {
                warn!(%e, "cannot create private Claude Code output directory; file inspection unavailable");
                None
            }
        };
        let mut cmd = tokio::process::Command::new(bin);
        if let Some(root) = &output_root {
            cmd.env("CLAUDE_CODE_TMPDIR", &root.0);
        }
        cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
        cmd.args(["--replay-user-messages", "--dangerously-skip-permissions"]);
        cmd.args(if resume { ["--resume", &launch.conversation] } else { ["--session-id", &launch.conversation] });
        if !model.is_empty() && model != "default" {
            cmd.args(["--model", model]);
        }
        if let Some(e) = &launch.effort {
            cmd.args(["--effort", e]);
        }
        if let Some(s) = &launch.settings {
            cmd.args(["--settings", s]);
        }
        if let Some(m) = &mcp {
            // A subagent may work for a long time: Claude Code waits for it.
            cmd.args(["--mcp-config", &m.mcp_config()]).env("MCP_TOOL_TIMEOUT", "86400000");
        }
        // In print mode the CLI stops waiting for background tasks after
        // ten minutes by default, and kills them.
        cmd.env("CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS", "86400000");
        cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let pgid = child.id().map(|id| Pid::from_raw(id as i32));
        *stdin.try_lock().map_err(|_| std::io::Error::other("the session's Claude Code input is busy"))? = child.stdin.take();
        let mut stderr = child.stderr.take().expect("piped");
        let stderr = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf).await;
            buf
        });
        let (tx, lines) = tokio::sync::mpsc::unbounded_channel();
        let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
        tokio::spawn(async move {
            while let Ok(Some(l)) = out.next_line().await {
                if l.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&l) {
                    Ok(v) => {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                    Err(_) => warn!(line = %l.chars().take(200).collect::<String>(), "non-JSON output from claude"),
                }
            }
        });
        let mapper = Mapper {
            turn: 0,
            scope: launch.conversation.clone(),
            output_root: output_root.as_ref().map(|r| r.0.clone()),
            saw_output: false,
            result: None,
            interrupted: false,
            observer,
            calls: HashMap::new(),
            children: HashMap::new(),
        };
        Ok(Self {
            child,
            stdin,
            lines,
            stderr: Some(stderr),
            pgid,
            launch,
            resumed: resume,
            model: model.to_owned(),
            mapper,
            unconfirmed: Default::default(),
            pending: None,
            notified: Vec::new(),
            idle_since: tokio::time::Instant::now(),
            registration: mcp,
            _output_root: output_root,
        })
    }

    pub fn launch(&self) -> &Launch {
        &self.launch
    }

    /// carbon6's tools it calls from now on belong to the turn `cancel` stops.
    pub fn set_turn_cancel(&self, cancel: &CancellationToken) {
        if let Some(r) = &self.registration {
            r.set_cancel(cancel);
        }
    }

    /// One of its background tasks is running.
    pub fn tasks_running(&self) -> bool {
        self.mapper
            .observer
            .as_ref()
            .is_some_and(|o| o.tools.background.list().iter().any(|t| t.kind == c6_tools::TaskKind::ClaudeCode && t.running))
    }

    /// When it may be closed, after `idle` with nothing to do; `None` while
    /// it has work.
    pub fn idle_until(&self, idle: Duration) -> Option<tokio::time::Instant> {
        (self.unconfirmed.is_empty() && self.pending.is_none() && !self.tasks_running()).then_some(self.idle_since + idle)
    }

    /// The next line it wrote (`None`: it exited). For a session's actor
    /// while no turn runs; pass the line to [`Process::idle_line`].
    pub async fn next_line(&mut self) -> Option<Value> {
        self.lines.recv().await
    }

    /// A line that came while no turn ran. Background-task updates and
    /// Claude Code's subagents' traffic are recorded at once; anything else
    /// begins a turn (Claude Code woke on a task's notification, or took an
    /// input written during the last turn): it is kept for that turn, and
    /// this returns true.
    pub async fn idle_line(&mut self, v: Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<bool> {
        if self.begins_turn(&v) {
            self.pending = Some(v);
            return Ok(true);
        }
        self.side_line(&v, sink).await?;
        Ok(false)
    }

    fn begins_turn(&self, v: &Value) -> bool {
        if !v["parent_tool_use_id"].is_null() {
            return false;
        }
        match v["type"].as_str().unwrap_or("") {
            "system" => v["subtype"] == "init",
            "user" if v["isReplay"].as_bool() == Some(true) => self.unconfirmed.iter().any(|w| v["uuid"].as_str() == Some(w.uuid.as_str())),
            "assistant" | "stream_event" | "result" | "user" => true,
            _ => false,
        }
    }

    /// A line that is not part of a turn's transcript.
    async fn side_line(&mut self, v: &Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        if v["type"] == "system" && v["subtype"] == "task_notification" && v["parent_tool_use_id"].is_null() {
            let summary = v["summary"].as_str().or(v["description"].as_str()).unwrap_or("A background task ended.");
            self.notified.push(summary.to_owned());
        }
        // An echo nobody waits for (such as a model switch's) is not a turn.
        if v["type"] == "user" && v["isReplay"].as_bool() == Some(true) {
            return Ok(());
        }
        self.mapper.handle(v, sink).await
    }

    /// Writes an input. The first input of a new conversation gets
    /// `preamble` in front.
    async fn send(&mut self, input: PendingInput, preamble: Option<&str>, recorded: bool) -> std::io::Result<()> {
        let uuid = uuid::Uuid::now_v7().to_string();
        let line = serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": user_content(&input, preamble)},
        });
        self.unconfirmed.push_back(Written { uuid, input, recorded });
        write_line(&self.stdin, &line).await
    }

    /// Inputs written that Claude Code never took and that are not in the
    /// log yet: for the next process.
    pub fn take_unsent(&mut self) -> Vec<PendingInput> {
        std::mem::take(&mut self.unconfirmed).into_iter().filter(|w| !w.recorded).map(|w| w.input).collect()
    }

    /// Records a confirmed input; the turn starts with it if it has not.
    async fn confirm(&mut self, uuid: &str, started: &mut bool, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        let Some(i) = self.unconfirmed.iter().position(|w| w.uuid == uuid) else { return Ok(()) };
        let w = self.unconfirmed.remove(i).expect("found");
        if !w.recorded {
            sink.append(w.input.event()).await?;
        }
        if !*started {
            sink.append(session_event::Kind::TurnStarted(TurnStarted { turn: self.mapper.turn, ..Default::default() })).await?;
            *started = true;
        }
        Ok(())
    }

    /// Starts a turn no input of ours began: Claude Code woke on its
    /// background tasks' notifications.
    async fn woke(&mut self, started: &mut bool, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        if *started {
            return Ok(());
        }
        let text = if self.notified.is_empty() { "A background task ended.".to_owned() } else { self.notified.join("\n") };
        let message = UserMessage {
            command_id: format!("task:{}", uuid::Uuid::now_v7()),
            text,
            command: "task-notification".into(),
            ..Default::default()
        };
        sink.append(session_event::Kind::User(message)).await?;
        sink.append(session_event::Kind::TurnStarted(TurnStarted { turn: self.mapper.turn, ..Default::default() })).await?;
        *started = true;
        Ok(())
    }

    /// Runs turn `turn` until Claude Code's `result` for it.
    ///
    /// - `inputs` begin it (logged now, with `TurnStarted`, unless `opened`:
    ///   a retry whose first attempt logged them). Without inputs it is the
    ///   turn an idle line began.
    /// - `more` is signalled when `feed` has inputs that join the turn; they
    ///   are logged when Claude Code takes them.
    /// - `cancel` interrupts the turn; its background tasks keep running.
    #[allow(clippy::too_many_arguments)]
    pub async fn turn(
        &mut self,
        turn: u64,
        inputs: Vec<PendingInput>,
        opened: bool,
        preamble: Option<&str>,
        model: &str,
        sink: &Arc<dyn EventSink>,
        feed: &(dyn Fn() -> Vec<PendingInput> + Send + Sync),
        more: &tokio::sync::Notify,
        cancel: &CancellationToken,
    ) -> anyhow::Result<TurnEnd> {
        self.mapper.turn = turn;
        self.mapper.result = None;
        self.mapper.interrupted = false;
        let mut started = opened;
        if !inputs.is_empty() && !opened {
            for input in &inputs {
                sink.append(input.event()).await?;
            }
            sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })).await?;
            started = true;
        }
        let mut alive = true;
        if !model.is_empty() && model != "default" && model != self.model {
            let request = serde_json::json!({"subtype": "set_model", "model": model});
            alive &= write_line(
                &self.stdin,
                &serde_json::json!({"type": "control_request", "request_id": uuid::Uuid::now_v7().to_string(), "request": request}),
            )
            .await
            .is_ok();
            self.model = model.to_owned();
        }
        for (i, input) in inputs.into_iter().enumerate() {
            alive &= self.send(input, if i == 0 { preamble } else { None }, true).await.is_ok();
        }
        let mut interrupted_at: Option<tokio::time::Instant> = None;
        while alive {
            let v = match self.pending.take() {
                Some(v) => v,
                None => tokio::select! {
                    _ = cancel.cancelled(), if interrupted_at.is_none() => {
                        self.mapper.interrupted = true;
                        interrupted_at = Some(tokio::time::Instant::now());
                        let request = serde_json::json!({"type": "control_request", "request_id": uuid::Uuid::now_v7().to_string(), "request": {"subtype": "interrupt"}});
                        alive = write_line(&self.stdin, &request).await.is_ok();
                        continue;
                    }
                    _ = tokio::time::sleep_until(interrupted_at.map_or_else(tokio::time::Instant::now, |t| t + INTERRUPT_GRACE)), if interrupted_at.is_some() => {
                        warn!(session = %self.launch.conversation, "Claude Code did not stop the turn when asked: stopping the process");
                        self.kill().await;
                        alive = false;
                        continue;
                    }
                    _ = more.notified() => {
                        for input in feed() {
                            alive &= self.send(input, None, false).await.is_ok();
                        }
                        continue;
                    }
                    line = self.lines.recv() => match line {
                        Some(v) => v,
                        None => { alive = false; continue; }
                    },
                },
            };
            let main = v["parent_tool_use_id"].is_null();
            match v["type"].as_str().unwrap_or("") {
                "user" if v["isReplay"].as_bool() == Some(true) => {
                    if let Some(uuid) = v["uuid"].as_str() {
                        self.confirm(uuid, &mut started, sink).await?;
                    }
                }
                "system" if v["subtype"] == "init" => {}
                "result" if main => {
                    // Inputs it took without echoing them.
                    for uuid in v["user_message_uuids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                        self.confirm(uuid, &mut started, sink).await?;
                    }
                    self.woke(&mut started, sink).await?;
                    self.mapper.handle(&v, sink).await?;
                    let reason = if self.mapper.interrupted {
                        TurnEndReason::Interrupted
                    } else {
                        self.mapper.result.unwrap_or(TurnEndReason::Completed)
                    };
                    sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: reason as i32 })).await?;
                    self.notified.clear();
                    self.idle_since = tokio::time::Instant::now();
                    return Ok(TurnEnd { reason: Some(reason), alive: true, no_conversation: false });
                }
                "assistant" | "stream_event" | "user" if main => {
                    self.woke(&mut started, sink).await?;
                    self.mapper.handle(&v, sink).await?;
                }
                _ => self.side_line(&v, sink).await?,
            }
        }
        // The process is gone (or going): it ended without a result.
        let interrupted = self.mapper.interrupted;
        self.mapper.close_children(if interrupted { TurnEndReason::Interrupted } else { TurnEndReason::Error }).await?;
        *self.stdin.lock().await = None;
        let status = match tokio::time::timeout(CLOSE_GRACE, self.child.wait()).await {
            Ok(s) => s.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()),
            Err(_) => {
                self.kill().await;
                "killed".into()
            }
        };
        let stderr = match self.stderr.take() {
            Some(t) => t.await.unwrap_or_default(),
            None => String::new(),
        };
        if let Some(obs) = &self.mapper.observer {
            // Only an upstream notification can say how a task ended.
            obs.tools.background.close_native(&self.mapper.scope, c6_tools::TaskState::Lost);
        }
        if self.resumed && !self.mapper.saw_output && stderr.to_ascii_lowercase().contains("no conversation found") {
            return Ok(TurnEnd { reason: None, alive: false, no_conversation: true });
        }
        if !started {
            warn!(session = %self.launch.conversation, %status, "Claude Code exited between turns");
            return Ok(TurnEnd { reason: None, alive: false, no_conversation: false });
        }
        let reason = if interrupted {
            TurnEndReason::Interrupted
        } else {
            let detail = stderr.trim().lines().last().unwrap_or("").to_owned();
            fail(sink, format!("Claude Code exited ({status}) without a result. {detail}")).await?
        };
        sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: reason as i32 })).await?;
        Ok(TurnEnd { reason: Some(reason), alive: false, no_conversation: false })
    }

    /// It exited while no turn ran: what it was running is lost.
    pub async fn exited(mut self) -> Vec<PendingInput> {
        if let Err(e) = self.mapper.close_children(TurnEndReason::Error).await {
            warn!("closing Claude Code's subagents: {e:#}");
        }
        let status = self.child.wait().await.map(|s| s.to_string()).unwrap_or_default();
        let stderr = match self.stderr.take() {
            Some(t) => t.await.unwrap_or_default(),
            None => String::new(),
        };
        warn!(session = %self.launch.conversation, %status, stderr = %stderr.trim().lines().last().unwrap_or(""), "Claude Code exited between turns");
        *self.stdin.lock().await = None;
        self.take_unsent()
    }

    /// Closes it: no more input, then it exits by itself (or is killed).
    pub async fn close(mut self) {
        *self.stdin.lock().await = None;
        if tokio::time::timeout(CLOSE_GRACE, self.child.wait()).await.is_err() {
            self.kill().await;
        }
        let _ = self.mapper.close_children(TurnEndReason::Completed).await;
    }

    /// Stops it and its background tasks now.
    pub async fn stop(mut self) {
        *self.stdin.lock().await = None;
        self.kill().await;
        if let Some(obs) = &self.mapper.observer {
            obs.tools.background.close_native(&self.mapper.scope, c6_tools::TaskState::Lost);
        }
        let _ = self.mapper.close_children(TurnEndReason::Interrupted).await;
    }

    async fn kill(&mut self) {
        if let Some(pg) = self.pgid {
            // SIGINT lets Claude Code save its transcript; then make sure.
            let _ = killpg(pg, Signal::SIGINT);
            if tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await.is_err() {
                let _ = killpg(pg, Signal::SIGKILL);
            }
        }
        let _ = self.child.wait().await;
    }
}

/// An input as Claude Code's user message content: its text (or
/// expansion) with files inline, and images as image blocks.
fn user_content(input: &PendingInput, preamble: Option<&str>) -> Value {
    let (images, files): (Vec<_>, Vec<_>) = input.attachments.iter().cloned().partition(|a| a.mime.starts_with("image/"));
    let text = c6_agent::user_text(input.model_text(), &files);
    let text = match preamble {
        Some(p) => format!("{p}\n\n{text}"),
        None => text,
    };
    if images.is_empty() {
        return Value::String(text);
    }
    use base64::Engine;
    let mut blocks = vec![serde_json::json!({"type": "text", "text": text})];
    blocks.extend(images.iter().map(|a| {
        serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": a.mime, "data": base64::engine::general_purpose::STANDARD.encode(&a.data)}})
    }));
    Value::Array(blocks)
}

/// CLI 2.1.314's official installed source resolves CLAUDE_CODE_TMPDIR
/// before creating its claude-uid/session/tasks/*.output tree. Give each
/// process a private root instead of trusting paths into the host filesystem.
struct OutputRoot(PathBuf);

impl OutputRoot {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let base = std::env::temp_dir().canonicalize()?;
        let path = base.join(format!("c6-claude-{}", uuid::Uuid::now_v7()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for OutputRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

/// Maps stream-json lines to session events, for as long as its process
/// runs.
struct Mapper {
    /// The turn running (or that last ran).
    turn: u64,
    /// CLI conversation identity, stable across carbon6 turns.
    scope: String,
    output_root: Option<PathBuf>,
    saw_output: bool,
    result: Option<TurnEndReason>,
    /// The turn was interrupted: its error result is not a failure.
    interrupted: bool,
    observer: Option<Arc<Observer>>,
    /// Tool calls seen, by id: the subagent they were made in (`None`: the
    /// session itself) and their input.
    calls: HashMap<String, (Option<String>, Value)>,
    /// Mirrored subagents, by the call that started each.
    children: HashMap<String, Child>,
}

impl Drop for Mapper {
    fn drop(&mut self) {
        if let Some(obs) = &self.observer {
            obs.tools.background.close_native(&self.scope, c6_tools::TaskState::Lost);
        }
    }
}

impl Mapper {
    /// Official SDKTaskStarted/Progress/Notification/UpdatedMessage protocol:
    /// https://code.claude.com/docs/en/agent-sdk/typescript#sdktaskstartedmessage
    /// Process these before parent_tool_use_id routing: nested tasks belong
    /// in the common task registry too, even when their traffic is mirrored.
    fn native_task(&self, v: &Value) -> bool {
        if v["type"] != "system" {
            return false;
        }
        let subtype = v["subtype"].as_str().unwrap_or("");
        if !matches!(subtype, "task_started" | "task_progress" | "task_notification" | "task_updated") {
            return false;
        }
        let Some(obs) = &self.observer else { return true };
        let Some(task_id) = v["task_id"].as_str().filter(|id| !id.is_empty()) else { return true };
        let run = v["run_id"].as_str();
        let bg = &obs.tools.background;
        if subtype == "task_started" {
            // Description, not the originating tool's command: the protocol
            // does not promise a shell command for every kind of task.
            bg.start_native(&self.scope, task_id, run, v["description"].as_str().unwrap_or(""));
            return true;
        }
        let state = match subtype {
            "task_notification" => match v["status"].as_str() {
                Some("completed") => c6_tools::TaskState::Finished,
                Some("failed") => c6_tools::TaskState::Failed,
                Some("stopped") => c6_tools::TaskState::Stopped,
                _ => return true, // Do not guess future terminal statuses.
            },
            "task_updated" => match v["patch"]["status"].as_str() {
                Some("completed") => c6_tools::TaskState::Finished,
                Some("failed") => c6_tools::TaskState::Failed,
                Some("killed") => c6_tools::TaskState::Stopped,
                _ => c6_tools::TaskState::Started,
            },
            _ => c6_tools::TaskState::Started,
        };
        let description = if subtype == "task_updated" { v["patch"]["description"].as_str() } else { v["description"].as_str() };
        // The summary is metadata, never fabricated stdout or stderr.
        let summary = v["summary"].as_str().or_else(|| v["patch"]["error"].as_str());
        let elapsed = v["usage"]["duration_ms"].as_u64().map(Duration::from_millis);
        let notification = subtype == "task_notification";
        let id = bg.observe_native(
            &self.scope,
            task_id,
            run,
            description,
            summary,
            elapsed,
            if notification { c6_tools::TaskState::Started } else { state },
        );
        // Only an official terminal notification can supply an inspectable
        // path. Model-generated tool text is never interpreted as a path.
        if notification
            && let Some(root) = self.output_root.as_deref()
            && let Some(path) = v["output_file"].as_str().filter(|s| !s.is_empty())
        {
            bg.capture_native_output(&id, root, Path::new(path), task_id);
        }
        if notification {
            // A completion-driven UI snapshot must see the captured output,
            // not race the file read and retain a metadata-only fallback.
            bg.observe_native(&self.scope, task_id, run, None, None, elapsed, state);
        }
        true
    }

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
        let Some(obs) = self.observer.clone() else { return Ok(None) };
        let (made_in, input) = self.calls.get(call).cloned().unwrap_or((None, Value::Null));
        // Its parent: the session, or the subagent that made the call.
        let (parent_id, parent_log, parent_depth) = match made_in.as_deref().and_then(|m| self.children.values().find(|c| c.id == m)) {
            Some(c) => (c.id.clone(), c.log.clone(), c.depth),
            None => (obs.session_id.clone(), sink.clone(), crate::subagent::depth_of(&obs.spec)),
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
                remote: None,
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
        log.append(session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })).await?;
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
        if self.native_task(v) {
            self.saw_output = true;
            return Ok(());
        }
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
                if v["is_error"].as_bool().unwrap_or(false) && self.interrupted {
                    self.result = Some(TurnEndReason::Interrupted);
                } else if v["is_error"].as_bool().unwrap_or(false) {
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

    #[tokio::test]
    async fn native_sdk_task_protocol_maps_runs_progress_and_terminal_states() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tools.background.set_sink(Some(tx));
        let observer = Arc::new(Observer {
            tools: tools.clone(),
            journal: Arc::new(crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap()),
            link: Arc::default(),
            session_id: "carbon6-session".into(),
            spec: CreateSession::default(),
            started: SessionStarted::default(),
        });
        let sink: Arc<dyn EventSink> = Arc::new(VecSink::default());
        let mut mapper = Mapper {
            turn: 1,
            scope: "cli-conversation".into(),
            output_root: Some(dir.path().to_owned()),
            saw_output: false,
            result: None,
            interrupted: false,
            observer: Some(observer),
            calls: HashMap::new(),
            children: HashMap::new(),
        };
        // Shapes from the official SDKTask*Message definitions. Lifecycle
        // routing precedes subagent traffic routing, including nested tasks.
        for line in [
            r#"{"type":"system","subtype":"task_started","task_id":"native-task","run_id":"001","description":"Run checks","task_type":"local_bash","is_backgrounded":true,"parent_tool_use_id":"nested-call","session_id":"cli-conversation"}"#,
            r#"{"type":"system","subtype":"task_progress","task_id":"native-task","run_id":"001","description":"Run checks","usage":{"total_tokens":42,"tool_uses":1,"duration_ms":27},"summary":"Still checking","session_id":"cli-conversation"}"#,
        ] {
            mapper.handle(&serde_json::from_str(line).unwrap(), &sink).await.unwrap();
        }
        let start = rx.try_recv().unwrap();
        assert_eq!(start.kind, c6_tools::TaskKind::ClaudeCode);
        assert_eq!(start.description, "Run checks");
        assert!(!start.notify);
        assert!(mapper.children.is_empty());
        let snapshot = tools.background.snapshot(&start.id).unwrap();
        assert!(snapshot.running);
        assert_eq!(snapshot.elapsed_ms, 27);
        assert!(snapshot.stdout.text.is_empty());
        assert_eq!(snapshot.combined.as_ref().unwrap().text, "Still checking");
        // A notification's existing file OUTSIDE our output tree must not
        // be read, even though its path came from the trusted CLI stream.
        let private = dir.path().join("private.txt");
        std::fs::write(&private, "do not expose these bytes").unwrap();
        mapper
            .handle(
                &serde_json::json!({
                    "type": "system", "subtype": "task_notification", "task_id": "native-task", "run_id": "001",
                    "status": "failed", "summary": "Checks failed", "output_file": private,
                    "usage": {"total_tokens": 42, "tool_uses": 1, "duration_ms": 31}
                }),
                &sink,
            )
            .await
            .unwrap();
        let done = rx.try_recv().unwrap();
        assert_eq!((done.state, done.exit_code, done.notify), (c6_tools::TaskState::Failed, None, false));
        let snapshot = tools.background.snapshot(&start.id).unwrap();
        assert!(!snapshot.running);
        assert_eq!(snapshot.elapsed_ms, 31);
        assert_eq!(snapshot.combined.as_ref().unwrap().text, "Checks failed");
        assert!(snapshot.combined.as_ref().unwrap().note.contains("unavailable"));
        assert!(!serde_json::to_string(&snapshot).unwrap().contains("do not expose"));
        mapper
            .handle(
                &serde_json::json!({
                    "type":"system", "subtype":"task_started", "task_id":"native-task", "run_id":"002", "description":"Resume checks"
                }),
                &sink,
            )
            .await
            .unwrap();
        let resumed = rx.try_recv().unwrap();
        assert_ne!(resumed.id, start.id);
        let tasks = dir.path().join("claude-501/cli-conversation/tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        let output = tasks.join("native-task.output");
        std::fs::write(&output, "real combined bytes").unwrap();
        mapper.handle(&serde_json::json!({
            "type":"system", "subtype":"task_notification", "task_id":"native-task", "run_id":"002", "status":"stopped", "output_file":output, "summary":"Stopped"
        }), &sink).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().state, c6_tools::TaskState::Stopped);
        assert_eq!(tools.background.snapshot(&resumed.id).unwrap().combined.unwrap().text, "real combined bytes");
        mapper
            .handle(
                &serde_json::json!({
                    "type":"system", "subtype":"task_started", "task_id":"agent-task", "run_id":"agent-run",
                    "description":"Background subagent", "task_type":"local_agent", "is_backgrounded":true
                }),
                &sink,
            )
            .await
            .unwrap();
        let agent = rx.try_recv().unwrap();
        mapper
            .handle(
                &serde_json::json!({
                    "type":"system", "subtype":"task_updated", "task_id":"agent-task", "run_id":"agent-run",
                    "patch":{"status":"completed"}
                }),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(rx.try_recv().unwrap().state, c6_tools::TaskState::Finished);
        assert!(!tools.background.snapshot(&agent.id).unwrap().running);
        mapper
            .handle(
                &serde_json::json!({
                    "type":"system", "subtype":"task_started", "task_id":"orphan", "description":"No notification"
                }),
                &sink,
            )
            .await
            .unwrap();
        let orphan = rx.try_recv().unwrap();
        drop(mapper);
        let lost = rx.try_recv().unwrap();
        assert_eq!(lost.id, orphan.id);
        assert_eq!((lost.state, lost.exit_code, lost.notify), (c6_tools::TaskState::Lost, None, false));
        assert!(!tools.background.snapshot(&orphan.id).unwrap().running);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn native_cli_output_is_captured_before_private_root_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            "claude-native",
            r#"#!/bin/sh
read -r line
tasks="$CLAUDE_CODE_TMPDIR/claude-501/session/tasks"
mkdir -p "$tasks"
printf 'actual combined bytes' > "$tasks/task.output"
printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"task","description":"Native shell","task_type":"local_bash","is_backgrounded":true}'
printf '{"type":"system","subtype":"task_notification","task_id":"task","status":"completed","summary":"Completed","output_file":"%s"}\n' "$tasks/task.output"
printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"orphan","description":"Unnotified task","task_type":"local_agent"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{}}'
"#,
        );
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tools.background.set_sink(Some(tx));
        let observer = observer(dir.path(), tools.clone()).await;
        let sink: Arc<dyn EventSink> = Arc::new(VecSink::default());
        let mut p = spawn(&bin, dir.path(), "session", false, Some(observer));
        let end = run(&mut p, 1, &[input("c1", "task")], &sink).await;
        assert_eq!(end, TurnEnd { reason: Some(TurnEndReason::Completed), alive: true, no_conversation: false });
        // It exits after its result: what it was running is lost.
        assert!(p.next_line().await.is_none());
        p.exited().await;
        let started = rx.try_recv().unwrap();
        let finished = rx.try_recv().unwrap();
        let orphan = rx.try_recv().unwrap();
        let lost = rx.try_recv().unwrap();
        assert_eq!((finished.state, lost.state), (c6_tools::TaskState::Finished, c6_tools::TaskState::Lost));
        assert_eq!(orphan.id, lost.id);
        assert!(!finished.notify && !lost.notify);
        assert_eq!(finished.exit_code, None);
        assert_eq!(tools.background.snapshot(&started.id).unwrap().combined.unwrap().text, "actual combined bytes");
        assert!(!tools.background.snapshot(&orphan.id).unwrap().running);
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn observer(dir: &Path, tools: Arc<c6_tools::SessionTools>) -> Arc<Observer> {
        Arc::new(Observer {
            tools,
            journal: Arc::new(crate::journal::Journal::open(&dir.join("journal.db")).await.unwrap()),
            link: Arc::default(),
            session_id: "0199-main".into(),
            spec: CreateSession::default(),
            started: SessionStarted { cwd: dir.display().to_string(), tool_profile: PROFILE.into(), ..Default::default() },
        })
    }

    fn spawn(bin: &Path, cwd: &Path, conversation: &str, resume: bool, observer: Option<Arc<Observer>>) -> Process {
        let launch = Launch { conversation: conversation.into(), effort: None, settings: None };
        Process::spawn(bin, launch, resume, cwd, "opus", Arc::default(), None, observer).unwrap()
    }

    fn input(id: &str, text: &str) -> PendingInput {
        PendingInput { command_id: id.into(), text: text.into(), ..Default::default() }
    }

    /// A turn with nothing joining it.
    async fn run(p: &mut Process, turn: u64, inputs: &[PendingInput], sink: &Arc<dyn EventSink>) -> TurnEnd {
        let more = tokio::sync::Notify::new();
        p.turn(turn, inputs.to_vec(), false, None, "opus", sink, &Vec::new, &more, &CancellationToken::new()).await.unwrap()
    }

    /// Waits for the turn's first reply.
    async fn replied(sink: &VecSink) {
        c6_testkit::eventually("a reply", async || kinds(sink).contains(&"assistant").then_some(())).await;
    }

    fn kinds(sink: &VecSink) -> Vec<&'static str> {
        sink.0.lock().unwrap().iter().map(kind).collect()
    }

    /// A stand-in `claude` that checks its arguments and replays a transcript.
    fn fake_claude(dir: &Path) -> PathBuf {
        let script_body = r#"#!/bin/sh
case "$*" in *--dangerously-skip-permissions*) ;; *) echo "missing yolo flag" >&2; exit 2;; esac
case "$*" in *"--resume"*) echo "No conversation found with session ID" >&2; exit 1;; esac
case "$*" in *"--input-format stream-json"*) ;; *) echo "missing stream-json input" >&2; exit 2;; esac
read -r prompt
pwd > cwd.txt
printf '%s' "$CLAUDE_CODE_TMPDIR" > output-root.txt
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
        script(dir, "claude", script_body)
    }

    #[tokio::test]
    async fn maps_a_turn_and_falls_back_from_resume() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let inputs = vec![input("c1", "list files")];
        let launch = Launch { conversation: "0199-session".into(), effort: Some("high".into()), settings: Some(r#"{"hooks":{}}"#.into()) };
        let bridge = crate::mcp_bridge::Bridge::start().await.unwrap();
        let entry = || crate::mcp_bridge::Entry {
            session_id: "0199-session".into(),
            session: Arc::new(c6_tools::SessionTools::new(dir.path().to_owned())),
            tools: Vec::new(),
            cancel: Default::default(),
            journal: Arc::new(futures::executor::block_on(crate::journal::Journal::open(&dir.path().join("journal.db"))).unwrap()),
            matched: Default::default(),
        };
        let more = tokio::sync::Notify::new();
        // A later turn tries --resume first; the fake has no conversation...
        let mut p =
            Process::spawn(&bin, launch.clone(), true, dir.path(), "opus", Arc::default(), Some(bridge.register(entry())), None).unwrap();
        let end = p.turn(2, inputs.clone(), false, None, "opus", &sink, &Vec::new, &more, &CancellationToken::new()).await.unwrap();
        assert_eq!(end, TurnEnd { reason: None, alive: false, no_conversation: true });
        assert_eq!(kinds(&vec_sink), ["user", "turn_started"], "the turn is logged once, before either attempt");
        // ... so a new one starts, which gets the preamble.
        let registration = bridge.register(entry());
        let mcp = registration.mcp_config();
        let mut p = Process::spawn(&bin, launch, false, dir.path(), "opus", Arc::default(), Some(registration), None).unwrap();
        let end = p.turn(2, inputs, true, Some("CONTEXT"), "opus", &sink, &Vec::new, &more, &CancellationToken::new()).await.unwrap();
        assert_eq!(end.reason, Some(TurnEndReason::Completed));
        assert_eq!(std::fs::read_to_string(dir.path().join("settings.txt")).unwrap(), r#"{"hooks":{}}"#, "the head's hooks are passed on");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mcp.txt")).unwrap(),
            format!("{mcp} 86400000"),
            "carbon6's subagents are offered over MCP, with time to finish"
        );
        let line: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("prompt.txt")).unwrap()).unwrap();
        assert_eq!(line["message"]["content"], "CONTEXT\n\nlist files");
        let output_root = PathBuf::from(std::fs::read_to_string(dir.path().join("output-root.txt")).unwrap());
        assert!(output_root.is_absolute());
        assert!(output_root.file_name().unwrap().to_string_lossy().starts_with("c6-claude-"));
        drop(p);
        assert!(!output_root.exists(), "the process's private directory is cleaned up");

        let events = vec_sink.0.lock().unwrap().clone();
        assert_eq!(
            events.iter().map(kind).collect::<Vec<_>>(),
            ["user", "turn_started", "assistant", "tool_result", "assistant", "usage", "turn_ended"]
        );
        let Some(session_event::Kind::ToolResult(r)) = &events[3].kind else { panic!() };
        assert_eq!((r.call_id.as_str(), r.output.as_str()), ("toolu_1", "a.txt"));
        let Some(session_event::Kind::Usage(u)) = &events[5].kind else { panic!() };
        assert_eq!((u.cache_read_tokens, u.model.as_str()), (900, "claude-opus-5-5"));
    }

    /// A stand-in `claude` whose turn runs a subagent (that runs its own).
    fn fake_claude_with_subagents(dir: &Path) -> PathBuf {
        let script = r#"#!/bin/sh
read -r line
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
        let observer = observer(dir.path(), Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()))).await;
        let journal = observer.journal.clone();
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "0199-main", false, Some(observer));
        let end = run(&mut p, 1, &[input("c1", "go")], &sink).await;
        assert_eq!(end.reason, Some(TurnEndReason::Completed));
        p.exited().await;
        let main = vec_sink.0.lock().unwrap().clone();
        assert_eq!(
            main.iter().map(kind).collect::<Vec<_>>(),
            ["user", "turn_started", "assistant", "subagent_spawned", "tool_result", "assistant", "usage", "turn_ended"]
        );
        let Some(session_event::Kind::SubagentSpawned(sp)) = &main[3].kind else { panic!() };
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

    /// A stand-in `claude` that stays up and answers each message: it echoes
    /// it, then replies, with "join" it waits for a second message first,
    /// and with "slow" it starts a background task and waits to be
    /// interrupted, after which the task ends and it wakes for it.
    fn live_claude(dir: &Path) -> PathBuf {
        script(
            dir,
            "claude-live",
            r#"#!/bin/sh
echo $$ >> spawns.txt
n=0
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
echo_of() { printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$1")"; }
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}\n' "$1"; }
done_ok() { printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{}}'; }
while read -r line; do
  case "$line" in
    *'"interrupt"'*)
      printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"usage":{}}'
      sleep 0.3
      printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"bg1","status":"completed","summary":"Build finished"}'
      printf '%s\n' '{"type":"system","subtype":"init"}'
      say "The build passed."
      done_ok
      continue;;
    *'"control_request"'*) continue;;
  esac
  n=$((n+1))
  printf '%s\n' '{"type":"system","subtype":"init"}'
  echo_of "$line"
  case "$line" in
    *slow*)
      printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"bg1","description":"Build","task_type":"local_bash"}'
      say "Working.";;
    *join*)
      say "Reading."
      read -r second
      echo_of "$second"
      say "Both done."
      done_ok;;
    *)
      say "Reply $n."
      done_ok;;
  esac
done
"#,
        )
    }

    /// One process serves turn after turn.
    #[tokio::test]
    async fn one_process_serves_every_turn() {
        let dir = tempfile::tempdir().unwrap();
        let bin = live_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, None);
        for (turn, text) in [(1, "hello"), (2, "again")] {
            assert_eq!(run(&mut p, turn, &[input(&format!("c{turn}"), text)], &sink).await.reason, Some(TurnEndReason::Completed));
        }
        assert_eq!(kinds(&vec_sink), [["user", "turn_started", "assistant", "usage", "turn_ended"]; 2].concat());
        assert_eq!(std::fs::read_to_string(dir.path().join("spawns.txt")).unwrap().lines().count(), 1);
        assert!(p.idle_until(IDLE).is_some(), "an idle process with no tasks may be closed later");
        p.close().await;
    }

    /// A message sent during a turn joins it, logged when Claude Code takes it.
    #[tokio::test]
    async fn a_message_sent_mid_turn_joins_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let bin = live_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, None);
        let queue = Mutex::new(Vec::new());
        let more = tokio::sync::Notify::new();
        let feed = || std::mem::take(&mut *queue.lock().unwrap());
        let cancel = CancellationToken::new();
        let turn = p.turn(1, vec![input("c1", "join me")], false, None, "opus", &sink, &feed, &more, &cancel);
        // Once the turn is under way.
        let send = async {
            replied(&vec_sink).await;
            queue.lock().unwrap().push(input("c2", "and this"));
            more.notify_one();
        };
        let (end, ()) = tokio::join!(turn, send);
        assert_eq!(end.unwrap().reason, Some(TurnEndReason::Completed));
        assert_eq!(kinds(&vec_sink), ["user", "turn_started", "assistant", "user", "assistant", "usage", "turn_ended"]);
        let Some(session_event::Kind::User(u)) = &vec_sink.0.lock().unwrap()[3].kind else { panic!() };
        assert_eq!((u.command_id.as_str(), u.text.as_str()), ("c2", "and this"));
    }

    /// Esc stops the turn, not its background task; when the task ends,
    /// Claude Code wakes for it in a turn of its own.
    #[tokio::test]
    async fn an_interrupt_keeps_background_tasks_and_their_end_wakes_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let bin = live_claude(dir.path());
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, Some(observer(dir.path(), tools.clone()).await));
        let cancel = CancellationToken::new();
        let more = tokio::sync::Notify::new();
        let turn = p.turn(1, vec![input("c1", "slow build")], false, None, "opus", &sink, &Vec::new, &more, &cancel);
        let stop = async {
            replied(&vec_sink).await;
            cancel.cancel();
        };
        let (end, ()) = tokio::join!(turn, stop);
        assert_eq!(end.unwrap(), TurnEnd { reason: Some(TurnEndReason::Interrupted), alive: true, no_conversation: false });
        assert_eq!(kinds(&vec_sink), ["user", "turn_started", "assistant", "usage", "turn_ended"], "an interrupt is not an error");
        assert!(p.tasks_running(), "its background task still runs");
        assert!(p.idle_until(IDLE).is_none(), "a process with a running task stays up");
        // Idle: the task's end is recorded, and the turn Claude Code starts for it is the agent's.
        loop {
            let v = p.next_line().await.expect("still up");
            if p.idle_line(v, &sink).await.unwrap() {
                break;
            }
        }
        assert!(!p.tasks_running());
        assert_eq!(run(&mut p, 2, &[], &sink).await.reason, Some(TurnEndReason::Completed));
        let events = vec_sink.0.lock().unwrap().clone();
        assert_eq!(events[5..].iter().map(kind).collect::<Vec<_>>(), ["user", "turn_started", "assistant", "usage", "turn_ended"]);
        let Some(session_event::Kind::User(u)) = &events[5].kind else { panic!() };
        assert_eq!((u.command.as_str(), u.text.as_str()), ("task-notification", "Build finished"));
        p.close().await;
    }
}
