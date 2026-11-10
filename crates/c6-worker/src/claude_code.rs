//! Claude Code backend: each turn runs the official `claude` CLI headless on
//! this worker, signed in with the user's own account, and its stream-json
//! output is mapped onto carbon6 session events. Claude Code runs its own
//! agent loop and tools and keeps its own transcript, resumed by session id.

use std::collections::{HashMap, HashSet};
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
use tracing::{info, warn};

/// Tool profile recorded for Claude Code sessions.
pub const PROFILE: &str = "claude_code";

/// What the log shows as a Claude Code session's system prompt.
pub const SYSTEM_PROMPT: &str = "(Claude Code's own system prompt)";

#[cfg(test)]
pub(crate) fn replace_test_bin(bin: Option<PathBuf>) -> Option<PathBuf> {
    std::mem::replace(&mut *tests::BIN.lock().unwrap(), bin)
}

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

/// Main-chain IDs, not API message IDs or the result event's UUID. A tool
/// turn ends at its final assistant entry, not the tool-call entry.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Boundary {
    pub session_id: String,
    pub input_uuids: Vec<String>,
    pub cutoff_uuid: String,
}

/// Durable reset intent. Only an untouched intent can be abandoned safely.
/// Old session files are never removed.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Reset {
    pub command_id: String,
    pub to_seq: u64,
    pub restore_files: bool,
    pub session_id: String,
    pub fresh: bool,
    #[serde(default)]
    pub withdrawn: Vec<String>,
    /// Persisted before dispatch; cleared BEFORE stopping the old runtime.
    /// Missing on older rows means potentially destructive, never untouched.
    #[serde(default)]
    pub untouched: bool,
    /// Restore the last confirmed branch if preflight fails. Pending resets
    /// never nest: only a confirmed mapping can become this snapshot.
    #[serde(default)]
    pub previous: Option<Box<(Reset, u64)>>,
    /// A destructive failure waits for an explicit retry, not each new input.
    #[serde(default)]
    pub failure: String,
    /// Explicit retry receipts authorizing this recovery. They must survive
    /// a crash during initialization, and fail with it rather than retry again.
    #[serde(default)]
    pub retries: Vec<String>,
}

/// How long an idle process with no background tasks stays up, unless
/// the subagent-limits setting says otherwise (docs/background.md §4).
pub const IDLE: Duration = Duration::from_secs(15 * 60);
/// How long an interrupted turn has to end before its process is killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// After you stop a turn, how long what ends or starts is taken as the
/// stop's doing rather than news for the agent (docs/background.md §4,
/// Stopping): a turn Claude Code starts by itself then is stopped too.
pub const STOP_QUIET: Duration = Duration::from_secs(5);
/// How long a closing process has to exit before it is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// What a process was started with: a turn that needs something else
/// starts a new one. The model is not here: it is switched in place.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Launch {
    /// The Claude Code conversation it runs.
    pub conversation: String,
    pub effort: Option<String>,
    /// Hooks from the head, as Claude Code settings JSON.
    pub settings: Option<String>,
    /// carbon6's guidance, added to Claude Code's system prompt
    /// (`--append-system-prompt`): the same for the life of the process, so
    /// its prompt stays cached.
    pub system_prompt: Option<String>,
    /// carbon6's tools on its MCP bridge, and those loaded up front: a
    /// process registered with others is not reused for them.
    pub tools: Vec<String>,
    /// The worker that started it: a new one starts it again (with what it
    /// passes now).
    pub worker: String,
    /// More arguments: the head's Ask passes `--tools ""` and
    /// `--strict-mcp-config`, so Claude Code has only carbon6's tools there
    /// (docs/ask.md §3).
    pub extra_args: Vec<String>,
    /// Claude Code's own tools it keeps (`None`: all of them, as without
    /// carbon6). With a list it gets nothing else: no other MCP server, no
    /// settings files, skills or plugins but `agents-md`, and a hook denies
    /// any other tool (docs/background.md §4, Claude Code's own tools).
    pub native: Option<Vec<String>>,
}

/// The built-in plugins a locked-down process keeps: `agents-md`, which
/// gives a project with no CLAUDE.md its AGENTS.md (carbon6 leaves project
/// instructions to Claude Code).
const KEPT_PLUGINS: &[&str] = &["cc-plugin-agents-md"];
/// The built-in plugins it turns off, which carbon6 has no use for: usage
/// events for plugins, and writing Claude Code mods.
const OFF_PLUGINS: &[&str] = &["telemetry@builtin", "plugin-authoring@builtin"];

/// The arguments that keep a process to `native` of Claude Code's own tools
/// and carbon6's MCP server.
fn locked_args(native: &[String]) -> Vec<String> {
    ["--tools", &native.join(","), "--strict-mcp-config", "--setting-sources", "", "--disable-slash-commands"].map(str::to_owned).to_vec()
}

/// Its settings (`--settings`): the head's hooks (`hooks`, settings JSON),
/// plus a `PreToolUse` hook on every tool that runs [`guard`] through
/// `guard_cmd`, and the built-in plugins turned off.
fn locked_settings(hooks: Option<&str>, guard_cmd: &str) -> String {
    let mut v: Value = hooks.and_then(|h| serde_json::from_str(h).ok()).unwrap_or_else(|| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    let guard = serde_json::json!({"matcher": "*", "hooks": [{"type": "command", "command": guard_cmd, "timeout": 10}]});
    match v["hooks"]["PreToolUse"].as_array_mut() {
        Some(groups) => groups.insert(0, guard),
        None => v["hooks"]["PreToolUse"] = serde_json::json!([guard]),
    }
    let plugins = &mut v["enabledPlugins"];
    for p in OFF_PLUGINS {
        plugins[*p] = Value::Bool(false);
    }
    v.to_string()
}

/// The hook command: this worker's `c6w claude-guard` with the tools it
/// allows.
fn guard_command(native: &[String]) -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "c6w".into());
    // A worker updated in place still names its new binary.
    let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
    let mut cmd = shell_quote(exe) + " claude-guard";
    for t in native {
        cmd.push(' ');
        cmd.push_str(&shell_quote(t));
    }
    cmd
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+=:,@".contains(&b)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// `c6w claude-guard`: a `PreToolUse` hook's verdict on the call in `input`
/// (the hook's JSON). `Ok` lets it run: one of `allowed`, or carbon6's
/// over MCP. `Err` is why not, which Claude Code shows the model (the hook
/// exits 2); anything it cannot read is refused, so it never fails open.
pub fn guard(allowed: &[String], input: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(input).map_err(|e| format!("carbon6 could not read this tool call ({e}), so it is not run"))?;
    let Some(name) = v["tool_name"].as_str().filter(|n| !n.is_empty()) else {
        return Err("carbon6 could not tell which tool this is, so it is not run".into());
    };
    if name.starts_with("mcp__carbon6__") || allowed.iter().any(|a| a == name) {
        return Ok(());
    }
    Err(format!(
        "{name} is not available in carbon6 sessions. Use {} or carbon6's tools (mcp__carbon6__*) instead.",
        if allowed.is_empty() { "none of Claude Code's own tools".to_owned() } else { allowed.join(", ") }
    ))
}

/// What a locked-down process has that it should not (its `system/init`):
/// its tools beyond `native` and carbon6's, other MCP servers, plugins
/// beyond those kept. Empty: as it should be.
fn unexpected(init: &Value, native: &[String]) -> Vec<String> {
    let names = |key: &str, field: Option<&str>| -> Vec<String> {
        init[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| match field {
                Some(f) => x[f].as_str(),
                None => x.as_str(),
            })
            .map(str::to_owned)
            .collect()
    };
    let mut out = Vec::new();
    let tools: Vec<String> = names("tools", None).into_iter().filter(|t| !t.starts_with("mcp__carbon6__") && !native.contains(t)).collect();
    if !tools.is_empty() {
        out.push(format!("tools {}", tools.join(", ")));
    }
    let servers: Vec<String> = names("mcp_servers", Some("name")).into_iter().filter(|s| s != "carbon6").collect();
    if !servers.is_empty() {
        out.push(format!("MCP servers {}", servers.join(", ")));
    }
    let plugins: Vec<String> = names("plugins", Some("name")).into_iter().filter(|p| !KEPT_PLUGINS.contains(&p.as_str())).collect();
    if !plugins.is_empty() {
        out.push(format!("plugins {}", plugins.join(", ")));
    }
    out
}

/// Where a session's process takes its input. Shared with the session's
/// actor, which can stop a background task while a turn holds the process.
pub type Stdin = Arc<tokio::sync::Mutex<Option<ClaudeInput>>>;

/// Where a process's input goes: its pipe, or the named pipe a held process
/// reads (docs/restarts.md §3).
pub enum ClaudeInput {
    Pipe(tokio::process::ChildStdin),
    Fifo(tokio::fs::File),
}

impl tokio::io::AsyncWrite for ClaudeInput {
    fn poll_write(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ClaudeInput::Pipe(p) => std::pin::Pin::new(p).poll_write(cx, buf),
            ClaudeInput::Fifo(f) => std::pin::Pin::new(f).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClaudeInput::Pipe(p) => std::pin::Pin::new(p).poll_flush(cx),
            ClaudeInput::Fifo(f) => std::pin::Pin::new(f).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClaudeInput::Pipe(p) => std::pin::Pin::new(p).poll_shutdown(cx),
            ClaudeInput::Fifo(f) => std::pin::Pin::new(f).poll_shutdown(cx),
        }
    }
}

/// A held process (docs/restarts.md §3): it runs apart from the worker,
/// reading a named pipe a keeper process holds open and writing to files,
/// so it goes on when the worker restarts and the next worker takes it up.
/// Its folder: `in` (the named pipe), `out`, `err`, `code` (its exit code,
/// once it ends), `offset` (how far its output was taken) and `meta.json`.
pub(crate) struct Held {
    dir: PathBuf,
    keeper: Pid,
    offset: Arc<std::sync::atomic::AtomicU64>,
    /// Stopped or closed on purpose: its folder goes with it.
    done: bool,
    /// Explicit rewind cleanup must kill the group even if its future is
    /// cancelled. Ordinary worker shutdown still leaves the keeper running.
    stop_on_drop: bool,
}

/// What a held process records about itself.
#[derive(serde::Serialize, serde::Deserialize)]
struct HeldMeta {
    session: String,
    conversation: String,
    pgid: i32,
    keeper: i32,
    output_root: Option<PathBuf>,
    token: Option<String>,
    #[serde(default)]
    launch: Launch,
}

/// Follows a held process's output file from `offset`: each JSON line, with
/// where it ends; closed once its exit code is written and all is read.
fn follow_held(dir: PathBuf, offset: u64) -> tokio::sync::mpsc::UnboundedReceiver<(u64, Value)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let Ok(mut f) = tokio::fs::File::open(dir.join("out")).await else { return };
        if f.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
            return;
        }
        let (mut pos, mut partial, mut buf) = (offset, Vec::<u8>::new(), vec![0u8; 64 * 1024]);
        let code = dir.join("code");
        // Its exit code is written after its last output: one more read
        // once it is there, then nothing more comes.
        let mut last = false;
        loop {
            match f.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    partial.extend_from_slice(&buf[..n]);
                    while let Some(i) = partial.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = partial.drain(..=i).collect();
                        pos += line.len() as u64;
                        let text = String::from_utf8_lossy(&line);
                        if text.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(text.trim()) {
                            Ok(v) => {
                                if tx.send((pos, v)).is_err() {
                                    return;
                                }
                            }
                            Err(_) => warn!(line = %text.chars().take(200).collect::<String>(), "non-JSON output from claude"),
                        }
                    }
                }
                Ok(_) if last => return,
                Ok(_) if code.exists() => last = true,
                Ok(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                Err(_) => return,
            }
        }
    });
    rx
}

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

/// What joins a running turn: inputs to write, and inputs written already
/// to deliver now (Send now on a message with the agent), by command id.
#[derive(Default)]
pub struct Joining {
    pub inputs: Vec<PendingInput>,
    pub now: Vec<String>,
}

/// A Send now the process was asked for: its request, and the turn it
/// was asked in.
struct SendNow {
    request_id: String,
    turn: u64,
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
    /// Its process, when this worker started it (a held one taken up after
    /// a restart is no child of this worker's).
    child: Option<tokio::process::Child>,
    stdin: Stdin,
    /// Its stdout, a JSON value per line with where it ends in a held
    /// process's output file; closed when it exits.
    lines: tokio::sync::mpsc::UnboundedReceiver<(u64, Value)>,
    held: Option<Held>,
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
    notified: Vec<(String, c6_proto::TaskNotice)>,
    /// SDK task ids are not session ids. Keep their structured labels separately.
    task_labels: HashMap<String, (String, String)>,
    /// Tasks Claude Code's subagents own: their ends are theirs to hear.
    subagent_tasks: HashSet<String>,
    /// Its own tasks that ended while no turn ran, as notices for the agent
    /// ([`Process::take_unwoken`]), and when to deliver them if Claude Code
    /// hasn't started a turn for them by itself.
    unwoken: Vec<(String, c6_proto::TaskNotice)>,
    wake_due: Option<tokio::time::Instant>,
    /// You stopped its last turn: until when a turn it starts by itself is
    /// taken as the stop's doing (`None` once one of its tasks ends on its
    /// own: a turn then is for that).
    after_stop: Option<tokio::time::Instant>,
    /// The turn about to run is one it started by itself right after your
    /// stop: it is stopped as it begins.
    quiet: bool,
    /// The last turn was such a one, stopped before the model said
    /// anything: it logged the notices it was for, but no turn.
    unshown: bool,
    /// Where it runs and where Claude Code keeps its transcripts
    /// ([`transcripts_root`]): the only place the text of a notice it starts
    /// a turn for is written (its stream doesn't carry it).
    cwd: PathBuf,
    transcripts: Option<PathBuf>,
    /// The last message of the main conversation it showed (a `user` or
    /// `assistant` line's uuid, as in its transcript), and the tasks it
    /// reported ended during the running turn.
    last_uuid: Option<String>,
    boundary: Boundary,
    turn_tasks: Vec<(String, String)>,
    /// Of the turn you stopped: its last message, when it ended, and the
    /// tasks it reported ended (to find, or else name, what it queued).
    stop_mark: Option<StopMark>,
    /// When its last turn ended, it last wrote between turns, or it started.
    idle_since: tokio::time::Instant,
    /// A Send now it hasn't answered yet.
    send_now: Option<SendNow>,
    /// It answered a Send now without saying how it took it (an older
    /// Claude Code, which may have stopped the whole turn): said once.
    old_send_now: bool,
    /// Its `system/init` was checked against what it was allowed (once).
    checked_init: bool,
    registration: Option<crate::mcp_bridge::Registration>,
    /// Where its process group is recorded while it runs, so a worker that
    /// starts after a crash can stop it ([`sweep_orphans`]).
    pidfile: Option<PathBuf>,
    _output_root: Option<OutputRoot>,
}

impl Drop for Process {
    fn drop(&mut self) {
        // A held one goes on without this worker; the next takes it up
        // (docs/restarts.md §3).
        if let Some(h) = &self.held
            && !h.done
        {
            if !h.stop_on_drop {
                return;
            }
            if let Some(pg) = self.pgid
                && (keeper_holds_fifo(h.keeper, pg, &h.dir) || is_claude_for(pg.as_raw(), &self.launch.conversation))
            {
                let _ = killpg(pg, Signal::SIGKILL);
            }
        }
        // Its background tasks are in its process group: none outlive it.
        if let (Some(Ok(None)), Some(pg)) = (self.child.as_mut().map(|c| c.try_wait()), self.pgid) {
            let _ = killpg(pg, Signal::SIGKILL);
        }
        if let Some(p) = &self.pidfile {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Stops the Claude Code processes a previous worker left running (it
/// crashed, so their input closed and nobody reads them), from the records
/// in `dir`. A record whose process is gone, or is now some other program,
/// is only removed.
pub async fn sweep_orphans(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "pid")
            && let Some((pgid, conversation)) = std::fs::read_to_string(&path).ok().and_then(|s| {
                let mut lines = s.lines();
                Some((lines.next()?.trim().parse::<i32>().ok()?, lines.next()?.trim().to_owned()))
            })
            && pgid > 0
            && is_claude_for(pgid, &conversation)
        {
            warn!(pgid, %conversation, "stopping a Claude Code process left by the previous worker");
            let pg = Pid::from_raw(pgid);
            let _ = killpg(pg, Signal::SIGINT);
            for _ in 0..30 {
                if nix::sys::signal::kill(pg, None).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = killpg(pg, Signal::SIGKILL);
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// The group leader is either the CLI or its keeper wrapper. Inspect argv,
/// not a substring of the shell's command text: both must carry an actual
/// conversation option. A reused PID is never signal authority.
fn is_claude_for(pid: i32, conversation: &str) -> bool {
    if pid <= 0 || nix::unistd::getpgid(Some(Pid::from_raw(pid))).ok() != Some(Pid::from_raw(pid)) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(data) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return false };
        let Ok(args) = std::str::from_utf8(&data) else { return false };
        claude_args_for(&args.split('\0').filter(|a| !a.is_empty()).collect::<Vec<_>>(), conversation)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Ok(out) = std::process::Command::new("ps").args(["-ww", "-o", "args=", "-p", &pid.to_string()]).output() else { return false };
        let line = String::from_utf8_lossy(&out.stdout);
        out.status.success() && claude_args_for(&line.split_whitespace().collect::<Vec<_>>(), conversation)
    }
}

fn claude_args_for(args: &[&str], conversation: &str) -> bool {
    let claude_program = |arg: &str| {
        let path = Path::new(arg);
        path.file_name().and_then(|s| s.to_str()).is_some_and(|s| s == "claude" || s.starts_with("claude-"))
            || path.components().any(|c| matches!(c.as_os_str().to_str(), Some("claude" | "claude-code")))
    };
    let options =
        match args {
            [_, "-c", r#""$@"; echo $? > "$C6_HELD_CODE""#, "sh", program, options @ ..] if claude_program(program) => options,
            [program, options @ ..] if claude_program(program) => options,
            [interpreter, program, options @ ..]
                if Path::new(interpreter).file_name().and_then(|s| s.to_str()).is_some_and(|s| {
                    matches!(s, "sh" | "bash" | "node" | "nodejs" | "python" | "python3") || s.starts_with("python3.")
                }) && claude_program(program) =>
            {
                options
            }
            _ => return false,
        };
    !conversation.is_empty() && options.windows(2).any(|a| matches!(a[0], "--resume" | "--session-id") && a[1] == conversation)
}

/// The keeper's open FIFO is stronger evidence than a saved PID: it still
/// belongs to this exact runtime even after the CLI/group leader has exited.
fn keeper_holds_fifo(keeper: Pid, pgid: Pid, dir: &Path) -> bool {
    if keeper.as_raw() <= 0 || nix::unistd::getpgid(Some(keeper)).ok() != Some(pgid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let Ok(fifo) = std::fs::metadata(dir.join("in")) else { return false };
        let Ok(open) = std::fs::metadata(format!("/proc/{}/fd/3", keeper.as_raw())) else { return false };
        fifo.file_type().is_fifo() && (fifo.dev(), fifo.ino()) == (open.dev(), open.ino())
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let Ok(fifo) = std::fs::metadata(dir.join("in")) else { return false };
        if !fifo.file_type().is_fifo() {
            return false;
        }
        let Ok(out) = std::process::Command::new("/usr/sbin/lsof").args(["-a", "-p", &keeper.to_string(), "-d", "3", "-FfniD"]).output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let lines = text.lines().collect::<Vec<_>>();
        out.status.success()
            && lines.contains(&"f3")
            && lines.iter().any(|line| line.strip_prefix('n').is_some_and(|p| Path::new(p) == dir.join("in")))
            && lines.iter().any(|line| line.strip_prefix('i').and_then(|n| n.parse::<u64>().ok()) == Some(fifo.ino()))
            && lines.iter().any(|line| line.strip_prefix("D0x").and_then(|n| u64::from_str_radix(n, 16).ok()) == Some(fifo.dev()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = dir;
        false
    }
}

fn held_group_owned(meta: &HeldMeta, dir: &Path) -> bool {
    meta.conversation == meta.launch.conversation
        && (is_claude_for(meta.pgid, &meta.conversation) || keeper_holds_fifo(Pid::from_raw(meta.keeper), Pid::from_raw(meta.pgid), dir))
}

/// Retain an identified kernel group member across SIGINT grace. If the
/// keeper has not opened its FIFO yet, wait before signaling anything. When
/// only an identified leader remains, kill immediately rather than losing
/// authority by first letting that leader exit during a grace period.
async fn stop_held_group(pg: Pid, keeper: Pid, dir: &Path, conversation: &str) -> anyhow::Result<()> {
    for _ in 0..100 {
        if matches!(killpg(pg, None), Err(nix::errno::Errno::ESRCH)) {
            return Ok(());
        }
        if keeper_holds_fifo(keeper, pg, dir) {
            nix::sys::signal::kill(keeper, Signal::SIGSTOP)?;
            anyhow::ensure!(keeper_holds_fifo(keeper, pg, dir), "Held Claude Code keeper ownership changed");
            let _ = killpg(pg, Signal::SIGINT);
            for _ in 0..30 {
                if nix::sys::signal::kill(pg, None).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            anyhow::ensure!(keeper_holds_fifo(keeper, pg, dir), "Held Claude Code keeper disappeared during stop");
            let _ = killpg(pg, Signal::SIGKILL);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if is_claude_for(pg.as_raw(), conversation) {
        let _ = killpg(pg, Signal::SIGKILL);
        return Ok(());
    }
    anyhow::bail!("Cannot retain ownership of held Claude Code group {}; its cleanup record was kept", pg)
}

/// A persisted PGID may have been reused: never signal it here. Only ESRCH
/// proves that an old group's writers (including orphaned children) are gone.
/// Live groups, zombies and permission errors all keep the durable barrier.
pub async fn confirm_stopped(journal: &crate::journal::Journal, session_id: &str, wait: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let mut pending = Vec::new();
        for pgid in journal.claude_processes(session_id).await? {
            match killpg(Pid::from_raw(pgid), None) {
                Err(nix::errno::Errno::ESRCH) => journal.untrack_claude_process(session_id, pgid).await?,
                Ok(()) => pending.push(pgid),
                Err(e) => anyhow::bail!("Cannot confirm Claude Code process group {pgid} stopped: {e}"),
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "Claude Code process groups have not confirmed that they stopped: {pending:?}"
        );
        tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_millis(25))).await;
    }
}

/// Recover ownership for cleanup, never for model input. Pending rewinds may
/// name neither the old branch nor an interrupted reset's launch identity.
/// Stop all of this session's held runtimes before the durable PGID barrier.
/// A reused or unidentifiable group is left alone and keeps that barrier shut.
pub async fn stop_held_for_rewind(held: &Path, session: &str, journal: &crate::journal::Journal) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(held) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut stopped = Vec::new();
    for entry in entries.into_iter().flatten() {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let meta: HeldMeta = serde_json::from_slice(&std::fs::read(dir.join("meta.json"))?)?;
        if meta.session != session {
            continue;
        }
        anyhow::ensure!(meta.pgid > 0, "Held Claude Code process group is unknown");
        journal.track_claude_process(session, meta.pgid).await?;
        let pg = Pid::from_raw(meta.pgid);
        match killpg(pg, None) {
            Err(nix::errno::Errno::ESRCH) => {}
            Ok(()) => {
                anyhow::ensure!(
                    held_group_owned(&meta, &dir),
                    "Cannot prove ownership of held Claude Code process group {}; stop confirmation is required",
                    meta.pgid
                );
                stop_held_group(pg, Pid::from_raw(meta.keeper), &dir, &meta.conversation).await?;
            }
            Err(e) => anyhow::bail!("Cannot inspect held Claude Code process group {}: {e}", meta.pgid),
        }
        stopped.push((dir, meta.output_root));
    }
    confirm_stopped(journal, session, Duration::from_secs(3)).await?;
    for (dir, output_root) in stopped {
        let _ = std::fs::remove_dir_all(dir);
        if let Some(root) = output_root {
            let _ = std::fs::remove_dir_all(root);
        }
    }
    Ok(())
}

impl Process {
    /// Persist before any input, including reuse after a turn and initialize
    /// attempts that fail before a PID file or fork transcript is written.
    async fn track(&self) -> anyhow::Result<()> {
        if let Some(observer) = &self.mapper.observer {
            let pgid = self.pgid.ok_or_else(|| anyhow::anyhow!("Claude Code process group is unknown"))?;
            observer.journal.track_claude_process(&observer.session_id, pgid.as_raw()).await?;
        }
        Ok(())
    }

    pub fn needs_preamble(&self) -> bool {
        !self.resumed && self.mapper.turn == 0
    }

    /// The SDK's no-model initialization handshake. In print mode Claude
    /// loads/validates the requested resume before acknowledging initialize.
    /// This also detects the hidden cutoff flag on versions that omit it from
    /// --help: an unsupported flag exits instead of acknowledging the reset.
    pub async fn ready(&mut self, session_id: &str) -> anyhow::Result<String> {
        self.track().await?;
        let source = self.launch.conversation.clone();
        let mut confirmed_id = session_id.to_owned();
        let request_id = uuid::Uuid::now_v7().to_string();
        write_line(
            &self.stdin,
            &serde_json::json!({"type":"control_request", "request_id":request_id,
            "request":{"subtype":"initialize"}}),
        )
        .await?;
        let ready = async {
            while let Some((end, v)) = self.lines.recv().await {
                self.consumed(end, &v);
                if v["type"] == "assistant" || v["type"] == "result" {
                    anyhow::bail!("Claude Code started a turn before confirming its reset");
                }
                if let Some(id) = v["session_id"].as_str().filter(|id| !id.is_empty()) {
                    anyhow::ensure!(
                        uuid::Uuid::parse_str(id).is_ok() && (id == session_id || id != source),
                        "Claude Code resumed an unexpected conversation"
                    );
                    confirmed_id = id.to_owned();
                }
                if v["type"] == "control_response" && v["response"]["request_id"] == request_id {
                    anyhow::ensure!(
                        v["response"]["subtype"] == "success",
                        "Claude Code refused initialization: {}",
                        v["response"]["error"]
                    );
                    return Ok(());
                }
            }
            anyhow::bail!("Claude Code exited before confirming its reset")
        };
        // A product startup limit, generous enough for a cold CLI on a loaded
        // worker. Failure never permits resuming the abandoned conversation.
        tokio::time::timeout(Duration::from_secs(60), ready).await??;
        self.launch.conversation = confirmed_id.clone();
        self.launch.extra_args.clear();
        self.mapper.scope = confirmed_id.clone();
        Ok(confirmed_id)
    }

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
        Self::spawn_with(bin, launch, resume, cwd, model, stdin, mcp, observer, None)
    }

    /// `spawn`, held in `dir` for session `session` (docs/restarts.md §3):
    /// it outlives this worker, and the next one takes it up ([`Process::attach`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_held(
        dir: &Path,
        session: &str,
        bin: &Path,
        launch: Launch,
        resume: bool,
        cwd: &Path,
        model: &str,
        stdin: Stdin,
        mcp: Option<crate::mcp_bridge::Registration>,
        observer: Option<Arc<Observer>>,
    ) -> std::io::Result<Self> {
        Self::spawn_with(bin, launch, resume, cwd, model, stdin, mcp, observer, Some((dir, session)))
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_with(
        bin: &Path,
        launch: Launch,
        resume: bool,
        cwd: &Path,
        model: &str,
        stdin: Stdin,
        mcp: Option<crate::mcp_bridge::Registration>,
        observer: Option<Arc<Observer>>,
        held_in: Option<(&Path, &str)>,
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
        match &launch.native {
            Some(native) => {
                let settings = locked_settings(launch.settings.as_deref(), &guard_command(native));
                cmd.args(["--settings", &*no_nul(&settings)]).args(locked_args(native)).env("ENABLE_CLAUDEAI_MCP_SERVERS", "false");
            }
            None => {
                if let Some(s) = &launch.settings {
                    cmd.args(["--settings", &*no_nul(s)]);
                }
            }
        }
        if let Some(p) = launch.system_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            // From a file in its private folder: the guidance carries memory
            // notes and the memex brief, which can be long (past the limit on
            // arguments) and hold any text (a NUL refuses to start it).
            let p = no_nul(p);
            let file = output_root.as_ref().map(|r| r.0.join("carbon6-system-prompt.md"));
            match file.filter(|f| std::fs::write(f, p.as_bytes()).is_ok()) {
                Some(f) => cmd.arg("--append-system-prompt-file").arg(f),
                None => cmd.args(["--append-system-prompt", &*p]),
            };
        }
        cmd.args(&launch.extra_args);
        if let Some(m) = &mcp {
            // A subagent may work for hours without a word on the bridge:
            // Claude Code waits for it (its idle timeout is 5 minutes).
            cmd.args(["--mcp-config", &m.mcp_config()])
                .env("MCP_TOOL_TIMEOUT", "86400000")
                .env("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "86400000");
        }
        // Once its input closes, the CLI waits this long for its background
        // tasks, then stops them. Its input closes only when it is idle with
        // no tasks, or when the worker is gone: then nobody reads it, so it
        // should not go on (docs/background.md §4).
        cmd.env("CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS", "5000");
        let mut output_root = output_root;
        let (child, lines, stderr, pgid, held) = match held_in {
            Some((dir, session)) => {
                let (child, held, pgid) =
                    Self::start_held(&mut cmd, cwd, dir, session, &launch, &stdin, mcp.as_ref(), output_root.as_ref())?;
                // Its folder (the guidance file) lives as long as it does.
                if let Some(root) = output_root.take() {
                    std::mem::forget(root);
                }
                let lines = follow_held(held.dir.clone(), 0);
                (Some(child), lines, None, pgid, Some(held))
            }
            None => {
                cmd.current_dir(cwd)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .process_group(0)
                    .kill_on_drop(true);
                let mut child = cmd.spawn()?;
                let pgid = child.id().map(|id| Pid::from_raw(id as i32));
                *stdin.try_lock().map_err(|_| std::io::Error::other("the session's Claude Code input is busy"))? =
                    child.stdin.take().map(ClaudeInput::Pipe);
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
                                if tx.send((0u64, v)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => warn!(line = %l.chars().take(200).collect::<String>(), "non-JSON output from claude"),
                        }
                    }
                });
                (Some(child), lines, Some(stderr), pgid, None)
            }
        };
        let mapper = Mapper {
            turn: 0,
            scope: launch.conversation.clone(),
            output_root: output_root.as_ref().map(|r| r.0.clone()),
            saw_output: false,
            result: None,
            interrupted: false,
            observer,
            calls: HashMap::new(),
            mcp_calls: HashSet::new(),
            children: HashMap::new(),
            backgrounded: HashSet::new(),
            task_calls: HashMap::new(),
            call_tasks: HashMap::new(),
            call_usage: HashMap::new(),
            per_call: false,
        };
        Ok(Self {
            child,
            stdin,
            lines,
            held,
            stderr,
            pgid,
            launch,
            resumed: resume,
            model: model.to_owned(),
            mapper,
            unconfirmed: Default::default(),
            pending: None,
            notified: Vec::new(),
            task_labels: HashMap::new(),
            subagent_tasks: HashSet::new(),
            unwoken: Vec::new(),
            wake_due: None,
            after_stop: None,
            quiet: false,
            unshown: false,
            cwd: cwd.to_owned(),
            transcripts: transcripts_root(),
            last_uuid: None,
            boundary: Boundary::default(),
            turn_tasks: Vec::new(),
            stop_mark: None,
            idle_since: tokio::time::Instant::now(),
            send_now: None,
            old_send_now: false,
            checked_init: false,
            registration: mcp,
            pidfile: None,
            _output_root: output_root,
        })
    }

    pub fn launch(&self) -> &Launch {
        &self.launch
    }

    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// Cleanup-only recording for a non-held initialization. Track the PGID
    /// durably before any initialize input, and retain the original launch
    /// conversation in the orphan record: ready may confirm a different fork
    /// ID, but ownership checks must match the process's actual argv.
    pub async fn record_in(&mut self, dir: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(self.held.is_none(), "Held Claude Code already has its own process record");
        self.track().await?;
        let pg = self.pgid.ok_or_else(|| anyhow::anyhow!("Claude Code process group is unknown"))?;
        let path = dir.join(format!("{}.pid", self.launch.conversation));
        std::fs::create_dir_all(dir)?;
        std::fs::write(&path, format!("{pg}\n{}\n", self.launch.conversation))?;
        self.pidfile = Some(path);
        Ok(())
    }

    /// Starts `cmd` held in `dir`/<conversation> (see [`Held`]): through a
    /// shell that writes its exit code, its input the named pipe (which a
    /// keeper in its process group holds open), its output in files.
    #[allow(clippy::too_many_arguments)]
    fn start_held(
        cmd: &mut tokio::process::Command,
        cwd: &Path,
        dir: &Path,
        session: &str,
        launch: &Launch,
        stdin: &Stdin,
        mcp: Option<&crate::mcp_bridge::Registration>,
        output_root: Option<&OutputRoot>,
    ) -> std::io::Result<(tokio::process::Child, Held, Option<Pid>)> {
        // A finished leader/code file is not group absence. This also checks
        // aliases whose authoritative stream ID differs from storage, and
        // refuses corrupt/ambiguous records instead of overwriting them.
        let record = Self::held_record(dir, &launch.conversation)?.map(|(path, _)| path);
        let dir = dir.join(&launch.conversation);
        let mut prior = record.into_iter().collect::<Vec<_>>();
        if dir.try_exists()? && !prior.contains(&dir) {
            prior.push(dir.clone());
        }
        for path in prior {
            let meta: HeldMeta = serde_json::from_slice(&std::fs::read(path.join("meta.json"))?).map_err(std::io::Error::other)?;
            if meta.pgid <= 0 || !matches!(killpg(Pid::from_raw(meta.pgid), None), Err(nix::errno::Errno::ESRCH)) {
                return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "Held Claude Code group has not confirmed absence"));
            }
            std::fs::remove_dir_all(&path)?;
            if let Some(root) = meta.output_root {
                let _ = std::fs::remove_dir_all(root);
            }
        }
        std::fs::create_dir_all(dir.parent().expect("held runtime parent"))?;
        // Reserve exclusively: another publisher after the absence check
        // cannot have its record or FIFO silently replaced.
        std::fs::create_dir(&dir)?;
        let fifo = dir.join("in");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).map_err(std::io::Error::from)?;
        // The worker's end reads too, so opening it never waits; with it
        // open, opening the process's end doesn't either.
        let writer = std::fs::OpenOptions::new().read(true).write(true).open(&fifo)?;
        let reader = std::fs::OpenOptions::new().read(true).open(&fifo)?;
        let std_cmd = cmd.as_std();
        let program = std_cmd.get_program().to_owned();
        let args: Vec<std::ffi::OsString> = std_cmd.get_args().map(|a| a.to_owned()).collect();
        let envs: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> =
            std_cmd.get_envs().map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned()))).collect();
        let mut wrapped = tokio::process::Command::new("/bin/sh");
        wrapped.arg("-c").arg(r#""$@"; echo $? > "$C6_HELD_CODE""#).arg("sh").arg(&program).args(&args);
        for (k, v) in envs {
            match v {
                Some(v) => wrapped.env(k, v),
                None => wrapped.env_remove(k),
            };
        }
        wrapped
            .env("C6_HELD_CODE", dir.join("code"))
            .current_dir(cwd)
            .stdin(Stdio::from(reader))
            .stdout(Stdio::from(std::fs::File::create(dir.join("out"))?))
            .stderr(Stdio::from(std::fs::File::create(dir.join("err"))?))
            .process_group(0);
        let child = wrapped.spawn()?;
        let pgid = child.id().map(|id| Pid::from_raw(id as i32));
        // Holds the pipe open while no worker does, so the process never
        // reads an end of its input by a restart; in its process group, so
        // stopping it stops the keeper too.
        let mut keeper = {
            tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(r#"exec 3<>"$0"; exec sleep 2147483647"#)
                .arg(&fifo)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(pgid.map_or(0, |p| p.as_raw()))
                .spawn()?
        };
        let keeper_pid = Pid::from_raw(keeper.id().expect("spawned keeper") as i32);
        // Reap it while this worker lives, without owning its lifetime. A
        // dropped wait on worker restart must not kill the detached keeper.
        tokio::spawn(async move {
            let _ = keeper.wait().await;
        });
        let keeper = keeper_pid;
        *stdin.try_lock().map_err(|_| std::io::Error::other("the session's Claude Code input is busy"))? =
            Some(ClaudeInput::Fifo(tokio::fs::File::from_std(writer)));
        let meta = HeldMeta {
            session: session.to_owned(),
            conversation: launch.conversation.clone(),
            pgid: pgid.map_or(0, |p| p.as_raw()),
            keeper: keeper.as_raw(),
            output_root: output_root.map(|r| r.0.clone()),
            token: mcp.map(|m| m.token().to_owned()),
            launch: launch.clone(),
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap_or_default())?;
        std::fs::write(dir.join("offset"), "0")?;
        Ok((child, Held { dir, keeper, offset: Arc::new(std::sync::atomic::AtomicU64::new(0)), done: false, stop_on_drop: false }, pgid))
    }

    /// The storage path stays fixed: the wrapper still writes its exit code
    /// there. A streamed authoritative session ID may change, so resolve by
    /// metadata, never by falling back to a different conversation's process.
    pub(crate) fn held_record(held: &Path, conversation: &str) -> std::io::Result<Option<(PathBuf, Option<String>)>> {
        let entries = match std::fs::read_dir(held) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut found = None;
        for entry in entries {
            let dir = entry?.path();
            if !dir.is_dir() {
                continue;
            }
            let meta: HeldMeta = serde_json::from_slice(&std::fs::read(dir.join("meta.json"))?).map_err(std::io::Error::other)?;
            if meta.conversation == conversation && meta.launch.conversation == conversation {
                if found.is_some() {
                    return Err(std::io::Error::other("Multiple held runtimes claim the same Claude Code conversation"));
                }
                found = Some((dir, meta.token));
            }
        }
        Ok(found)
    }

    /// The sessions previous workers left held processes for.
    pub(crate) fn held_sessions(held: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(held) else { return Vec::new() };
        entries
            .flatten()
            .filter_map(|e| serde_json::from_slice::<HeldMeta>(&std::fs::read(e.path().join("meta.json")).ok()?).ok())
            .map(|m| m.session)
            .collect()
    }

    /// Takes up the held process a previous worker left for `launch`'s
    /// conversation, where it had read up to (docs/restarts.md §3). None:
    /// there is none, or it has ended (its folder is cleared).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn attach(
        held: &Path,
        conversation: &str,
        cwd: &Path,
        model: &str,
        stdin: Stdin,
        mcp: Option<crate::mcp_bridge::Registration>,
        observer: Option<Arc<Observer>>,
    ) -> Option<Self> {
        let (dir, _) = Self::held_record(held, conversation).ok()??;
        let meta: HeldMeta = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()?;
        if meta.conversation != conversation || meta.launch.conversation != conversation {
            return None;
        }
        let launch = meta.launch.clone();
        let pg = Pid::from_raw(meta.pgid);
        if meta.pgid <= 0 {
            return None;
        }
        if matches!(killpg(pg, None), Err(nix::errno::Errno::ESRCH)) {
            let _ = std::fs::remove_dir_all(&dir);
            if let Some(r) = &meta.output_root {
                let _ = std::fs::remove_dir_all(r);
            }
            return None;
        }
        if !held_group_owned(&meta, &dir) {
            return None;
        }
        if dir.join("code").exists() {
            // Finished while detached: close only a freshly identified group,
            // never signal the persisted keeper PID blindly. Keep its record
            // if any descendants still prevent absence confirmation.
            let _ = killpg(pg, Signal::SIGKILL);
            let absent = async {
                loop {
                    match killpg(pg, None) {
                        Err(nix::errno::Errno::ESRCH) => break,
                        Ok(()) => tokio::time::sleep(Duration::from_millis(25)).await,
                        Err(_) => return false,
                    }
                }
                true
            };
            if matches!(tokio::time::timeout(Duration::from_secs(3), absent).await, Ok(true)) {
                let _ = std::fs::remove_dir_all(&dir);
                if let Some(r) = &meta.output_root {
                    let _ = std::fs::remove_dir_all(r);
                }
            }
            return None;
        }
        let writer = std::fs::OpenOptions::new().read(true).write(true).open(dir.join("in")).ok()?;
        *stdin.try_lock().ok()? = Some(ClaudeInput::Fifo(tokio::fs::File::from_std(writer)));
        let offset = std::fs::read_to_string(dir.join("offset")).ok().and_then(|o| o.trim().parse::<u64>().ok()).unwrap_or(0);
        let lines = follow_held(dir.clone(), offset);
        info!(conversation = %launch.conversation, offset, "took up a Claude Code process the previous worker held");
        let mapper = Mapper {
            turn: 0,
            scope: launch.conversation.clone(),
            output_root: meta.output_root.clone(),
            saw_output: true,
            result: None,
            interrupted: false,
            observer,
            calls: HashMap::new(),
            mcp_calls: HashSet::new(),
            children: HashMap::new(),
            backgrounded: HashSet::new(),
            task_calls: HashMap::new(),
            call_tasks: HashMap::new(),
            call_usage: HashMap::new(),
            per_call: false,
        };
        Some(Self {
            child: None,
            stdin,
            lines,
            held: Some(Held {
                dir,
                keeper: Pid::from_raw(meta.keeper),
                offset: Arc::new(std::sync::atomic::AtomicU64::new(offset)),
                done: false,
                stop_on_drop: false,
            }),
            stderr: None,
            pgid: Some(pg),
            launch,
            resumed: true,
            model: model.to_owned(),
            mapper,
            unconfirmed: Default::default(),
            pending: None,
            notified: Vec::new(),
            task_labels: HashMap::new(),
            subagent_tasks: HashSet::new(),
            unwoken: Vec::new(),
            wake_due: None,
            after_stop: None,
            quiet: false,
            unshown: false,
            cwd: cwd.to_owned(),
            transcripts: transcripts_root(),
            last_uuid: None,
            boundary: Boundary::default(),
            turn_tasks: Vec::new(),
            stop_mark: None,
            idle_since: tokio::time::Instant::now(),
            send_now: None,
            old_send_now: false,
            checked_init: false,
            registration: mcp,
            pidfile: None,
            _output_root: None,
        })
    }

    /// A line was taken from a held process's output: a restarted worker
    /// reads on from after it (deltas as they stream aren't saved one by
    /// one: they're shown only while live).
    fn consumed(&mut self, end: u64, v: &Value) {
        if v["parent_tool_use_id"].is_null()
            && let Some(id) = v["session_id"].as_str().filter(|id| !id.is_empty())
        {
            if let Some(h) = &self.held {
                // Persist the identity before the offset: a crash may replay
                // this line, but must never skip the only proof of its branch.
                let update = || -> std::io::Result<()> {
                    let path = h.dir.join("meta.json");
                    let mut meta: HeldMeta = serde_json::from_slice(&std::fs::read(&path)?).map_err(std::io::Error::other)?;
                    if meta.conversation != id || meta.launch.conversation != id {
                        meta.conversation = id.into();
                        meta.launch.conversation = id.into();
                        let tmp = h.dir.join("meta.next");
                        std::fs::write(&tmp, serde_json::to_vec(&meta).map_err(std::io::Error::other)?)?;
                        std::fs::rename(tmp, path)?;
                    }
                    Ok(())
                };
                if let Err(e) = update() {
                    warn!("persisting held Claude Code conversation identity: {e}");
                    return;
                }
            }
            self.launch.conversation = id.into();
        }
        if let Some(h) = &self.held
            && end > 0
        {
            h.offset.store(end, std::sync::atomic::Ordering::SeqCst);
            if v["type"] != "stream_event" {
                let _ = std::fs::write(h.dir.join("offset"), end.to_string());
            }
        }
    }

    /// Waits for it to end: as this worker's child, or, held by a previous
    /// worker, until its exit code is written or its process group is gone.
    async fn wait_exit(&mut self) -> String {
        if let Some(c) = self.child.as_mut() {
            return c.wait().await.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
        }
        let Some(h) = &self.held else { return String::new() };
        let code = h.dir.join("code");
        loop {
            if let Ok(c) = std::fs::read_to_string(&code) {
                return format!("exit status: {}", c.trim());
            }
            if self.pgid.is_none_or(|g| nix::sys::signal::kill(g, None).is_err()) {
                return "gone".into();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// What it wrote to stderr: its pipe's, or a held process's file.
    async fn stderr_text(&mut self) -> String {
        if let Some(t) = self.stderr.take() {
            return t.await.unwrap_or_default();
        }
        self.held.as_ref().and_then(|h| std::fs::read_to_string(h.dir.join("err")).ok()).unwrap_or_default()
    }

    /// It has ended (or is stopped on purpose): a held process's folder,
    /// keeper and guidance go with it.
    fn release_held(&mut self) {
        if self.pgid.is_some_and(|pg| !matches!(killpg(pg, None), Err(nix::errno::Errno::ESRCH))) {
            // Leader exit is not group absence. Keep cleanup authority and
            // its durable barrier until every descendant/keeper is gone.
            return;
        }
        if let Some(h) = &mut self.held
            && !h.done
        {
            h.done = true;
            let _ = std::fs::remove_dir_all(&h.dir);
            if let Some(r) = &self.mapper.output_root {
                let _ = std::fs::remove_dir_all(r);
            }
        }
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
        let (end, v) = self.lines.recv().await?;
        self.consumed(end, &v);
        Some(v)
    }

    /// A line that came while no turn ran. Background-task updates and
    /// Claude Code's subagents' traffic are recorded at once; anything else
    /// begins a turn (Claude Code woke on a task's notification, or took an
    /// input written during the last turn): it is kept for that turn, and
    /// this returns true.
    pub async fn idle_line(&mut self, v: Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<bool> {
        // A long task can end after the last turn's idle deadline. Once
        // its update clears tasks_running(), give Claude Code a fresh idle
        // interval to reply, rather than closing stdin on the next actor
        // iteration and leaving that reply unread during shutdown.
        self.idle_since = tokio::time::Instant::now();
        if self.begins_turn(&v) {
            // Right after you stopped a turn, with nothing of yours waiting
            // and none of its tasks ended since, Claude Code starts a turn
            // for the notifications it queued during the one you stopped
            // (seen with 2.1.311): that is the stop's doing, so it is stopped
            // too, and they reach the agent with your next message.
            self.quiet = self.unconfirmed.is_empty() && self.after_stop.is_some_and(|t| tokio::time::Instant::now() < t);
            self.pending = Some(v);
            // It woke by itself: that turn hears about what ended.
            self.unwoken.clear();
            self.wake_due = None;
            return Ok(true);
        }
        self.side_line(&v, sink).await?;
        if v["type"] == "system"
            && v["subtype"] == "task_notification"
            && v["parent_tool_use_id"].is_null()
            && let Some(status) = v["status"].as_str().filter(|s| matches!(*s, "completed" | "failed"))
        {
            // A task that ended on its own (a stop's are stopped or killed):
            // a turn Claude Code starts now is for it, and goes ahead.
            self.after_stop = None;
            // One of its own tasks ended while it waits. Claude Code usually
            // starts a turn for it; when it doesn't (seen with a long build),
            // the agent would wait for good, so the session tells it.
            if !v["task_id"].as_str().is_some_and(|t| self.subagent_tasks.contains(t)) {
                self.unwoken.push((unwoken_notice(&v, status), sdk_task_notice(&v, &self.task_labels)));
                self.wake_due.get_or_insert_with(|| tokio::time::Instant::now() + WAKE_GRACE);
            }
        }
        Ok(false)
    }

    /// When the session should tell the agent about tasks that ended while
    /// no turn ran, if Claude Code hasn't started one for them by then.
    pub fn wake_due(&self) -> Option<tokio::time::Instant> {
        self.wake_due
    }

    /// The notice for those tasks (`None`: nothing is waiting).
    pub fn take_unwoken(&mut self) -> Option<(String, c6_proto::TaskNotice)> {
        self.wake_due = None;
        let notices = std::mem::take(&mut self.unwoken);
        (!notices.is_empty())
            .then(|| (notices.iter().map(|(text, _)| text.as_str()).collect::<Vec<_>>().join("\n"), combined_task_notice(&notices)))
    }

    /// Whether its last turn was one it started by itself right after your
    /// stop, which was stopped before the model said anything and logged no
    /// turn (so its number is free again). Asking clears it.
    pub fn take_unshown(&mut self) -> bool {
        std::mem::take(&mut self.unshown)
    }

    /// The notices Claude Code started a turn for right after your stop, as
    /// they reached the model. Its stream doesn't carry them (2.1.311: only
    /// the turn's `system/init`, then the model's answer), so they are read
    /// from its transcript, where it writes them before asking the model.
    /// Never made up: if they can't be read, a note says so and names what
    /// is known.
    async fn queued_notices(&mut self) -> Vec<String> {
        let mark = self.stop_mark.take();
        let after = mark.as_ref().and_then(|m| m.uuid.clone());
        let since = mark.as_ref().map_or_else(std::time::SystemTime::now, |m| m.at);
        let file = self.transcripts.as_deref().and_then(|root| transcript_of(root, &self.cwd, &self.launch.conversation));
        if let Some(file) = &file {
            let until = tokio::time::Instant::now() + TRANSCRIPT_WAIT;
            loop {
                let text = tokio::fs::read_to_string(file).await.unwrap_or_default();
                if let Some(texts) = queued_in(&text, after.as_deref(), since) {
                    return texts;
                }
                if tokio::time::Instant::now() >= until {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let session = &self.launch.conversation;
        warn!(%session, transcript = ?file, "the notice Claude Code started a turn for is not in its transcript");
        let mut note = "Claude Code queued a notice we couldn't read".to_owned();
        let tasks = mark.map(|m| m.tasks).unwrap_or_default();
        if !tasks.is_empty() {
            let named: Vec<String> = tasks.iter().map(|(id, status)| format!("{id} ({status})")).collect();
            note.push_str(&format!(". In the turn you stopped it reported these tasks ended: {}", named.join(", ")));
        }
        note.push('.');
        vec![note]
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
        if v["type"] == "system"
            && v["subtype"] == "task_started"
            && let Some(id) = v["task_id"].as_str()
        {
            self.task_labels.insert(
                id.into(),
                (v["description"].as_str().unwrap_or_default().into(), v["task_type"].as_str().unwrap_or_default().into()),
            );
        }
        if v["type"] == "system"
            && v["subtype"] == "task_started"
            && v["owned_by_subagent"].as_bool() == Some(true)
            && let Some(t) = v["task_id"].as_str()
        {
            self.subagent_tasks.insert(t.to_owned());
        }
        if v["type"] == "control_response" {
            return self.control_response(v, false, sink).await;
        }
        if v["type"] == "system" && v["subtype"] == "task_notification" && v["parent_tool_use_id"].is_null() {
            let summary = v["summary"].as_str().or(v["description"].as_str()).unwrap_or("A background task ended.");
            self.notified.push((summary.to_owned(), sdk_task_notice(v, &self.task_labels)));
            let field = |k: &str| v[k].as_str().unwrap_or("").to_owned();
            self.turn_tasks.push((field("task_id"), field("status")));
        }
        // An echo nobody waits for (such as a model switch's) is not a turn.
        if v["type"] == "user" && v["isReplay"].as_bool() == Some(true) {
            return Ok(());
        }
        self.mapper.handle(v, sink).await
    }

    /// Writes an input; its uuid. The first input of a new conversation
    /// gets `preamble` in front.
    async fn send(&mut self, input: PendingInput, preamble: Option<&str>, recorded: bool) -> std::io::Result<String> {
        let uuid = uuid::Uuid::now_v7().to_string();
        let mut line = serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": user_content(&input, preamble)},
        });
        // Only an explicitly authenticated browser message is a person's.
        // API, internal and legacy inputs must not acquire human provenance.
        if is_yours(&input) {
            line["origin"] = serde_json::json!({"kind": "human"});
        }
        self.unconfirmed.push_back(Written { uuid: uuid.clone(), input, recorded });
        write_line(&self.stdin, &line).await?;
        Ok(uuid)
    }

    /// Asks it to deliver a message written already (`uuid`) now, rather than
    /// at the agent's next step. Claude Code moves what holds the turn
    /// (a command, a subagent, an MCP call) to the background and delivers it
    /// into the turn, or stops only the turn and starts the next with it;
    /// either way its background agents run on, which a plain interrupt
    /// would stop (docs/background.md §1).
    async fn deliver_now(&mut self, uuid: &str) -> std::io::Result<()> {
        let request_id = uuid::Uuid::now_v7().to_string();
        let request = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "interrupt", "send_now": true, "message_uuid": uuid},
        });
        self.send_now = Some(SendNow { request_id, turn: self.mapper.turn });
        write_line(&self.stdin, &request).await
    }

    /// Its answer to a control request; only a Send now's matters. `running`:
    /// a turn is under way (the one the Send now was asked in, if its turn).
    /// Its first `system/init` (one comes with each turn): what it has,
    /// logged, and for a locked-down process, a note in the transcript if
    /// it has anything carbon6 did not allow (a Claude Code that ignores
    /// a flag, or a tool it added since).
    async fn check_init(&mut self, init: &Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        if std::mem::replace(&mut self.checked_init, true) {
            return Ok(());
        }
        let session = &self.launch.conversation;
        info!(
            %session,
            version = %init["claude_code_version"].as_str().unwrap_or("?"),
            tools = %init["tools"],
            mcp_servers = %init["mcp_servers"],
            plugins = %init["plugins"],
            "Claude Code started"
        );
        let Some(native) = &self.launch.native else { return Ok(()) };
        let extra = unexpected(init, native);
        if extra.is_empty() {
            return Ok(());
        }
        warn!(%session, extra = %extra.join("; "), "Claude Code has what carbon6 did not allow");
        sink.append(session_event::Kind::Notice(c6_proto::Notice {
            title: "Claude Code has tools carbon6 did not allow".into(),
            message: format!(
                "Besides {} and carbon6's tools, this Claude Code process has {}. carbon6 refuses their calls, but this Claude Code may not honour all of carbon6's launch flags.",
                native.join(", "),
                extra.join("; ")
            ),
            priority: "normal".into(),
            quiet: true,
        }))
        .await?;
        Ok(())
    }

    async fn control_response(&mut self, v: &Value, running: bool, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        let r = &v["response"];
        let Some(asked) = self.send_now.take_if(|s| r["request_id"].as_str() == Some(s.request_id.as_str())) else { return Ok(()) };
        let this_turn = running && asked.turn == self.mapper.turn;
        let session = &self.launch.conversation;
        if r["subtype"] != "success" {
            warn!(%session, error = %r["error"], "Claude Code refused Send now: the message reaches the agent at its next step");
            return Ok(());
        }
        match r["response"]["send_now"].as_str() {
            // Delivered into the running turn: it goes on.
            Some("delivering") => info!(%session, "Send now: Claude Code delivers the message into the running turn"),
            // Only the turn stopped; the message starts the next.
            Some("stopped") => {
                info!(%session, "Send now: Claude Code stopped the turn; the message starts the next");
                if this_turn {
                    self.mapper.interrupted = true;
                }
            }
            Some("nothing_waiting") => info!(%session, "Send now: Claude Code had taken the message already"),
            Some(other) => warn!(%session, outcome = other, "Send now: an outcome carbon6 doesn't know"),
            // An older Claude Code: it took it as a plain interrupt, which
            // stops its background agents too.
            None => {
                warn!(%session, "Send now: this Claude Code doesn't say how it took it (an older one): it may have stopped the turn and its background agents");
                if this_turn {
                    self.mapper.interrupted = true;
                }
                if !std::mem::replace(&mut self.old_send_now, true) {
                    sink.append(session_event::Kind::Notice(c6_proto::Notice {
                        title: "Send now stopped the turn".into(),
                        message: "This Claude Code doesn't know Send now, so it may have taken it as Stop: the turn ended, and Claude Code's own background agents may have stopped with it. Your message starts the next turn. Update Claude Code (claude update) so Send now spares them.".into(),
                        priority: "low".into(),
                        quiet: true,
                    }))
                    .await?;
                }
            }
        }
        Ok(())
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
        self.boundary.input_uuids.push(uuid.to_owned());
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
    /// background tasks' notifications. One it started right after your stop
    /// (`quiet`) shows the notices it was for as they reached the model.
    async fn woke(&mut self, started: &mut bool, quiet: bool, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        if *started {
            return Ok(());
        }
        let texts = if quiet {
            self.queued_notices().await
        } else if self.notified.is_empty() {
            vec!["A background task ended.".to_owned()]
        } else {
            vec![self.notified.iter().map(|(text, _)| text.as_str()).collect::<Vec<_>>().join("\n")]
        };
        let task_notice = (!quiet && !self.notified.is_empty()).then(|| combined_task_notice(&self.notified));
        told(sink, texts, task_notice).await?;
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
        feed: &(dyn Fn() -> Joining + Send + Sync),
        more: &tokio::sync::Notify,
        cancel: &CancellationToken,
    ) -> anyhow::Result<TurnEnd> {
        self.track().await?;
        self.mapper.turn = turn;
        self.boundary = Boundary { session_id: self.launch.conversation.clone(), ..Default::default() };
        // This turn hears about whatever ended before it.
        self.unwoken.clear();
        self.wake_due = None;
        self.mapper.result = None;
        self.mapper.interrupted = false;
        // One it started by itself right after your stop (`idle_line`).
        let quiet = std::mem::take(&mut self.quiet) && inputs.is_empty() && !opened;
        self.turn_tasks.clear();
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
        // You had just stopped the agent: this turn stops as it begins. It
        // shows only if the model got to say something first.
        if quiet {
            info!(session = %self.launch.conversation, "Claude Code started a turn by itself right after you stopped one: stopping it too");
            self.mapper.interrupted = true;
            alive &= write_line(&self.stdin, &interrupt_request()).await.is_ok();
        }
        let mut interrupted_at: Option<tokio::time::Instant> = None;
        while alive {
            let v = match self.pending.take() {
                Some(v) => v,
                None => tokio::select! {
                    _ = cancel.cancelled(), if interrupted_at.is_none() => {
                        self.mapper.interrupted = true;
                        interrupted_at = Some(tokio::time::Instant::now());
                        alive = write_line(&self.stdin, &interrupt_request()).await.is_ok();
                        continue;
                    }
                    _ = tokio::time::sleep_until(interrupted_at.map_or_else(tokio::time::Instant::now, |t| t + INTERRUPT_GRACE)), if interrupted_at.is_some() => {
                        warn!(session = %self.launch.conversation, "Claude Code did not stop the turn when asked: stopping the process");
                        self.kill().await;
                        alive = false;
                        continue;
                    }
                    // A turn stopped as it began takes no input: what comes
                    // meanwhile starts the next.
                    _ = more.notified(), if !quiet => {
                        let Joining { inputs, now } = feed();
                        for input in inputs {
                            let (command_id, send_now) = (input.command_id.clone(), input.send_now);
                            let sent = self.send(input, None, false).await;
                            alive &= sent.is_ok();
                            // It is with the agent now: it reads it at its next step.
                            if sent.is_ok() && !command_id.is_empty() {
                                sink.append(session_event::Kind::InputDelivered(c6_proto::InputDelivered { command_id })).await?;
                            }
                            // Send now: at once.
                            if let Ok(uuid) = &sent
                                && send_now
                            {
                                alive &= self.deliver_now(uuid).await.is_ok();
                            }
                        }
                        for command_id in now {
                            match self.unconfirmed.iter().find(|w| w.input.command_id == command_id).map(|w| w.uuid.clone()) {
                                Some(uuid) => alive &= self.deliver_now(&uuid).await.is_ok(),
                                None => info!(session = %self.launch.conversation, command = %command_id, "Send now: the agent has taken that message already"),
                            }
                        }
                        continue;
                    }
                    line = self.lines.recv() => match line {
                        Some((end, v)) => {
                            self.consumed(end, &v);
                            v
                        }
                        None => { alive = false; continue; }
                    },
                },
            };
            let main = v["parent_tool_use_id"].is_null();
            if main {
                if let Some(id) = v["session_id"].as_str().filter(|id| !id.is_empty()) {
                    self.boundary.session_id = id.to_owned();
                    self.launch.conversation = id.to_owned();
                }
                if v["type"] == "assistant" {
                    // Never carry the preceding tool-call UUID forward if
                    // this final entry lacks one: that requires fresh-history
                    // fallback rather than an unsafe, incomplete cut.
                    let blocks = v["message"]["content"].as_array();
                    let final_reply =
                        blocks.is_some_and(|bs| bs.iter().any(|b| b["type"] == "text") && !bs.iter().any(|b| b["type"] == "tool_use"));
                    self.boundary.cutoff_uuid = if final_reply { v["uuid"].as_str().unwrap_or_default().to_owned() } else { String::new() };
                }
            }
            if main
                && matches!(v["type"].as_str(), Some("user" | "assistant"))
                && let Some(uuid) = v["uuid"].as_str()
            {
                self.last_uuid = Some(uuid.to_owned());
            }
            match v["type"].as_str().unwrap_or("") {
                "user" if v["isReplay"].as_bool() == Some(true) => {
                    if let Some(uuid) = v["uuid"].as_str() {
                        self.confirm(uuid, &mut started, sink).await?;
                    }
                }
                "system" if v["subtype"] == "init" => self.check_init(&v, sink).await?,
                "control_response" => self.control_response(&v, true, sink).await?,
                "result" if main => {
                    // A Send now not answered yet whose turn ends stopped: the
                    // turn was stopped for it, not failed.
                    if self.send_now.as_ref().is_some_and(|s| s.turn == turn) && v["subtype"] == "error_during_execution" {
                        self.mapper.interrupted = true;
                    }
                    // Inputs it took without echoing them.
                    for uuid in v["user_message_uuids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                        self.confirm(uuid, &mut started, sink).await?;
                    }
                    // You stopped it (or it is the turn stopped right after):
                    // for a moment, what follows is the stop's doing.
                    self.after_stop = (interrupted_at.is_some() || quiet).then(|| tokio::time::Instant::now() + STOP_QUIET);
                    // Where what it queues next is found (after the notices of
                    // this turn, if it is a stopped one, are read).
                    let mark = (interrupted_at.is_some() || quiet).then(|| StopMark {
                        uuid: self.last_uuid.clone(),
                        at: std::time::SystemTime::now(),
                        tasks: std::mem::take(&mut self.turn_tasks),
                    });
                    // Stopped before the model said anything: no turn, only
                    // the notices it put into the conversation, which the
                    // agent reads with your next message.
                    self.unshown = quiet && !started;
                    if self.unshown {
                        let texts = self.queued_notices().await;
                        told(sink, texts, None).await?;
                        sink.append(session_event::Kind::Notice(c6_proto::Notice {
                            title: "Stopped before replying".into(),
                            message: "Right after you stopped the agent, Claude Code started a turn by itself for this notice; carbon6 stopped it before the agent replied. The agent reads it with your next message.".into(),
                            priority: "low".into(),
                            quiet: true,
                        }))
                        .await?;
                    } else {
                        self.woke(&mut started, quiet, sink).await?;
                        self.mapper.handle(&v, sink).await?;
                    }
                    let reason = if self.mapper.interrupted {
                        TurnEndReason::Interrupted
                    } else {
                        self.mapper.result.unwrap_or(TurnEndReason::Completed)
                    };
                    if !self.unshown {
                        let seq = sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: reason as i32 })).await?;
                        if let Some(obs) = &self.mapper.observer {
                            // Seq, not the CLI/actor's turn counter: quiet turns
                            // need not be shown, so counters can repeat after a
                            // worker restart. A crash before this write leaves
                            // no trusted cutoff and must use fresh history.
                            obs.journal.record_claude_turn(&obs.session_id, seq, &self.boundary).await?;
                        }
                    }
                    self.stop_mark = mark;
                    self.notified.clear();
                    self.idle_since = tokio::time::Instant::now();
                    return Ok(TurnEnd { reason: Some(reason), alive: true, no_conversation: false });
                }
                // In a turn stopped as it began, Claude Code's "[Request
                // interrupted by user]" and the stream's first frames are not
                // the model's word: only its message makes the turn show.
                "stream_event" | "user" if main && quiet && !started => {}
                "assistant" | "stream_event" | "user" if main => {
                    self.woke(&mut started, quiet, sink).await?;
                    self.mapper.handle(&v, sink).await?;
                }
                _ => self.side_line(&v, sink).await?,
            }
        }
        // The process is gone (or going): it ended without a result.
        let interrupted = self.mapper.interrupted;
        self.mapper.close_children(if interrupted { TurnEndReason::Interrupted } else { TurnEndReason::Error }).await?;
        *self.stdin.lock().await = None;
        let status = match tokio::time::timeout(CLOSE_GRACE, self.wait_exit()).await {
            Ok(s) => s,
            Err(_) => {
                self.kill().await;
                "killed".into()
            }
        };
        let stderr = self.stderr_text().await;
        self.release_held();
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
        let status = self.wait_exit().await;
        let stderr = self.stderr_text().await;
        self.release_held();
        warn!(session = %self.launch.conversation, %status, stderr = %stderr.trim().lines().last().unwrap_or(""), "Claude Code exited between turns");
        *self.stdin.lock().await = None;
        self.take_unsent()
    }

    /// Closes it: no more input, then it exits by itself (or is killed).
    pub async fn close(mut self) {
        if self.held.is_some() {
            // Keep the FIFO writer until ownership-preserving cleanup: EOF
            // could otherwise retire the only provably owned group leader.
            self.kill().await;
            *self.stdin.lock().await = None;
        } else {
            *self.stdin.lock().await = None;
            if tokio::time::timeout(CLOSE_GRACE, self.wait_exit()).await.is_err() {
                self.kill().await;
            }
        }
        self.release_held();
        let _ = self.mapper.close_children(TurnEndReason::Completed).await;
    }

    /// Replacement may drain/send its inbox only after the old entire group
    /// is absent. A failed stop retains held metadata and the durable PGID;
    /// it is not permission to spawn another runtime over that authority.
    pub async fn close_for_replacement(mut self) -> anyhow::Result<()> {
        self.track().await?;
        if self.held.is_none() {
            *self.stdin.lock().await = None;
        }
        self.kill().await;
        let pg = self.pgid.ok_or_else(|| anyhow::anyhow!("Claude Code process group is unknown"))?;
        anyhow::ensure!(
            matches!(killpg(pg, None), Err(nix::errno::Errno::ESRCH)),
            "Claude Code replacement is blocked until process group {} confirms absence",
            pg
        );
        *self.stdin.lock().await = None;
        self.release_held();
        let _ = self.mapper.close_children(TurnEndReason::Completed).await;
        Ok(())
    }

    /// Stops it and its background tasks now.
    pub async fn stop(mut self) {
        if self.held.is_none() {
            *self.stdin.lock().await = None;
        }
        self.kill().await;
        *self.stdin.lock().await = None;
        if let Some(obs) = &self.mapper.observer {
            obs.tools.background.close_native(&self.mapper.scope, c6_tools::TaskState::Lost);
        }
        let _ = self.mapper.close_children(TurnEndReason::Interrupted).await;
    }

    /// Rewind must not wait forever if signaling or child cleanup fails.
    /// Its durable PGID survives timeout/Drop; confirm_stopped gates retries.
    pub async fn stop_for_rewind(mut self) -> anyhow::Result<()> {
        if let Some(h) = &mut self.held {
            h.stop_on_drop = true;
        }
        tokio::time::timeout(Duration::from_secs(5), async move {
            self.track().await?;
            self.stop().await;
            Ok(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("Claude Code did not stop within the rewind cleanup limit"))?
    }

    async fn kill(&mut self) {
        if let Some(pg) = self.pgid {
            if let Some(h) = &mut self.held {
                h.stop_on_drop = true;
                if let Err(e) = stop_held_group(pg, h.keeper, &h.dir, &self.launch.conversation).await {
                    warn!("stopping held Claude Code: {e:#}");
                    return;
                }
            } else {
                // Non-held child ownership retains the existing stop path.
                let _ = killpg(pg, Signal::SIGINT);
                let _ = tokio::time::timeout(Duration::from_secs(3), self.wait_exit()).await;
                let _ = killpg(pg, Signal::SIGKILL);
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), self.wait_exit()).await;
        if let Some(pg) = self.pgid {
            let absent = async {
                while !matches!(killpg(pg, None), Err(nix::errno::Errno::ESRCH)) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(1), absent).await;
        }
        self.release_held();
    }
}

/// An input as Claude Code's user message content: its text (or
/// expansion) with files inline, and images as image blocks.
fn user_content(input: &PendingInput, preamble: Option<&str>) -> Value {
    let (images, files): (Vec<_>, Vec<_>) = input.attachments.iter().cloned().partition(|a| a.mime.starts_with("image/"));
    let text = c6_agent::user_text(&input.attributed_text(), &files);
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

/// The input is explicitly human, not API automation or one of carbon6's
/// notices. Missing provenance never implies a person.
fn is_yours(input: &PendingInput) -> bool {
    input.human_origin
        && input.api_origin.is_none()
        && input.from_session_id.is_empty()
        && input.delegated_by.is_empty()
        && !matches!(input.command.as_str(), "task-notification" | "session-message")
}

/// CLI 2.1.314's official installed source resolves CLAUDE_CODE_TMPDIR
/// before creating its claude-uid/session/tasks/*.output tree. Give each
/// process a private root instead of trusting paths into the host filesystem.
struct OutputRoot(PathBuf);

/// Of a turn you stopped: its last message Claude Code showed (`uuid`), when
/// it ended, and the tasks it reported ended in it (id, status).
struct StopMark {
    uuid: Option<String>,
    at: std::time::SystemTime,
    tasks: Vec<(String, String)>,
}

/// How long Claude Code has to write a notice it starts a turn for into its
/// transcript (it does so before asking the model) before carbon6 gives up
/// reading it.
const TRANSCRIPT_WAIT: Duration = Duration::from_secs(2);

/// Where Claude Code keeps its transcripts: `$CLAUDE_CONFIG_DIR/projects`,
/// else `~/.claude/projects` (the CLI runs with the worker's environment).
fn transcripts_root() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
        .map(|d| d.join("projects"))
}

/// A conversation's transcript: `<root>/<cwd, each character but a letter or
/// digit as ->/<conversation>.jsonl`, or wherever under the root a file of
/// that name is (conversation ids are unique).
fn transcript_of(root: &Path, cwd: &Path, conversation: &str) -> Option<PathBuf> {
    let name = format!("{conversation}.jsonl");
    let folder: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let direct = root.join(folder).join(&name);
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.path().join(&name)).find(|p| p.is_file())
}

/// The notices Claude Code put into its conversation to start a turn after
/// the turn you stopped, verbatim from its transcript: the main
/// conversation's task-notification messages after that turn's last message
/// (`after`), or, when that message isn't there, written since it ended
/// (`since`). `None`: none there (yet).
fn queued_in(transcript: &str, after: Option<&str>, since: std::time::SystemTime) -> Option<Vec<String>> {
    let entries = transcript.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let entries: Vec<Value> = entries.filter(|v| v["isSidechain"].as_bool() != Some(true)).collect();
    let start = match after.and_then(|u| entries.iter().position(|v| v["uuid"].as_str() == Some(u))) {
        Some(i) => i + 1,
        None => {
            // Its clock and ours: allow a second.
            let since = chrono::DateTime::<chrono::Utc>::from(since) - chrono::TimeDelta::seconds(1);
            let after_stop =
                |v: &Value| v["timestamp"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).is_some_and(|t| t >= since);
            entries.iter().position(|v| is_task_notice(v) && after_stop(v))?
        }
    };
    let mut texts = Vec::new();
    for v in &entries[start..] {
        match v["type"].as_str() {
            // The model's answer: the notices came before it.
            Some("assistant") => break,
            Some("user") if is_task_notice(v) => match &v["message"]["content"] {
                Value::String(s) => texts.push(s.clone()),
                Value::Array(blocks) => texts.extend(blocks.iter().filter_map(|b| b["text"].as_str()).map(str::to_owned)),
                _ => {}
            },
            // Something else after them (such as "[Request interrupted by user]").
            Some("user") if !texts.is_empty() => break,
            _ => {}
        }
    }
    (!texts.is_empty()).then_some(texts)
}

/// A message Claude Code wrote into the conversation for a task's
/// notification.
fn is_task_notice(v: &Value) -> bool {
    v["type"] == "user" && (v["origin"]["kind"] == "task-notification" || v["turnOrigin"] == "task_notification")
}

/// Logs what the agent was told, each as a task notification.
async fn told(sink: &Arc<dyn EventSink>, texts: Vec<String>, task_notice: Option<c6_proto::TaskNotice>) -> anyhow::Result<()> {
    for text in texts {
        let message = UserMessage {
            command_id: format!("task:{}", uuid::Uuid::now_v7()),
            text,
            command: "task-notification".into(),
            task_notice: task_notice.clone(),
            origin_recorded: true,
            ..Default::default()
        };
        sink.append(session_event::Kind::User(message)).await?;
    }
    Ok(())
}

/// Only SDK fields and task-start metadata are used, never notification prose.
fn sdk_task_notice(v: &Value, labels: &HashMap<String, (String, String)>) -> c6_proto::TaskNotice {
    let label = v["task_id"].as_str().and_then(|id| labels.get(id));
    let title =
        v["description"].as_str().filter(|s| !s.trim().is_empty()).or_else(|| label.map(|(title, _)| title.as_str())).unwrap_or_default();
    let native_type =
        v["task_type"].as_str().filter(|s| !s.is_empty()).or_else(|| label.map(|(_, kind)| kind.as_str())).unwrap_or_default();
    let status = v["status"].as_str().unwrap_or_default();
    let markdown = v["summary"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_owned).unwrap_or_else(|| format!("Task {status}."));
    c6_proto::TaskNotice {
        kind: match native_type {
            "local_bash" => "command",
            "local_agent" => "subagent",
            _ => "task",
        }
        .into(),
        title: title.into(),
        status: status.into(),
        markdown,
        ..Default::default()
    }
}

fn combined_task_notice(notices: &[(String, c6_proto::TaskNotice)]) -> c6_proto::TaskNotice {
    if notices.len() == 1 {
        return notices[0].1.clone();
    }
    c6_proto::TaskNotice {
        kind: "task".into(),
        title: "Background tasks".into(),
        markdown: notices.iter().map(|(_, n)| format!("{} · {}\n\n{}", n.title, n.status, n.markdown)).collect::<Vec<_>>().join("\n\n"),
        ..Default::default()
    }
}

/// Asks Claude Code to stop its turn (Esc). Its background agents stop with
/// it; its background commands don't.
fn interrupt_request() -> Value {
    serde_json::json!({"type": "control_request", "request_id": uuid::Uuid::now_v7().to_string(), "request": {"subtype": "interrupt"}})
}

/// How long Claude Code has to start a turn by itself for a task that ended
/// while it waited, before the session tells the agent.
pub const WAKE_GRACE: Duration = Duration::from_secs(10);

/// What the agent is told about one of its tasks that ended while it waited
/// (Claude Code's own notification, as it would have given it).
fn unwoken_notice(v: &Value, status: &str) -> String {
    let what = v["summary"].as_str().or(v["description"].as_str()).unwrap_or("A background task");
    let mut s = format!("<background-task kind=\"claude-code\" status=\"{status}\">\nTask: {what}\n");
    if let Some(out) = v["output_file"].as_str().filter(|o| !o.is_empty()) {
        s.push_str(&format!("Output: {out}\n"));
    }
    s.push_str("</background-task>");
    s
}

/// Text for an argument or file of Claude Code's: a NUL can't be passed to
/// a process, so it is left out.
fn no_nul(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\0') { s.replace('\0', "").into() } else { s.into() }
}

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
    /// Its original parent/spawn, retained so a later native task start
    /// can link this same mirror without inventing another child.
    parent_log: Arc<dyn EventSink>,
    spawn: SubagentSpawned,
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
    /// session itself), tool name and input.
    calls: HashMap<String, (Option<String>, String, Value)>,
    /// Calls of carbon6's own tools over its MCP server: what Claude Code
    /// says under them is not one of its subagents (carbon6 shows a
    /// subagent such a call starts itself).
    mcp_calls: HashSet<String>,
    /// Mirrored subagents, by the call that started each.
    children: HashMap<String, Child>,
    /// Calls that started a subagent in the background: their tool result
    /// comes at once ("launched"), so the subagent ends with its task's
    /// notification instead.
    backgrounded: HashSet<String>,
    /// The call that started each task, by the CLI's task id: a task the
    /// CLI moves to the background later says only its id.
    task_calls: HashMap<String, String>,
    /// Exact native registry ID by originating tool call, not the SDK's
    /// task ID. The task start may precede or follow its mirror's creation.
    call_tasks: HashMap<String, String>,
    /// Each conversation's model call so far, by whose it is (`None`: the
    /// session's; else the call that started a subagent): its message id,
    /// model and usage. Claude Code sends a message per content block, each
    /// with its call's usage to date; a call is recorded once the next one
    /// starts, its results come back, or the turn ends.
    call_usage: HashMap<Option<String>, (String, String, Value)>,
    /// The session's own calls this turn said their usage: the turn's total
    /// (`result`) is not recorded on top.
    per_call: bool,
}

impl Drop for Mapper {
    fn drop(&mut self) {
        if let Some(obs) = &self.observer {
            obs.tools.background.close_native(&self.scope, c6_tools::TaskState::Lost);
        }
    }
}

impl Mapper {
    /// Notes a model call's usage to date from one of its messages; the call
    /// before it in the same conversation is recorded once this one starts.
    async fn note_usage(&mut self, who: Option<&str>, message: &Value, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        let (Some(id), u) = (message["id"].as_str(), &message["usage"]) else { return Ok(()) };
        if !u.is_object() {
            return Ok(());
        }
        let key = who.map(str::to_owned);
        if self.call_usage.get(&key).is_some_and(|(open, _, _)| open != id) {
            self.record_usage(who, sink).await?;
        }
        let model = message["model"].as_str().unwrap_or_default().to_owned();
        self.call_usage.insert(key, (id.to_owned(), model, u.clone()));
        if who.is_none() {
            self.per_call = true;
        }
        Ok(())
    }

    /// Records a conversation's open model call, in the session's log or its
    /// subagent's mirror.
    async fn record_usage(&mut self, who: Option<&str>, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        let Some((_, model, u)) = self.call_usage.remove(&who.map(str::to_owned)) else { return Ok(()) };
        let window = window_of(&model);
        let event = session_event::Kind::Usage(usage(model, &u, window));
        match who {
            None => {
                sink.append(event).await?;
            }
            Some(call) => {
                if let Some((_, log)) = self.child(call, sink).await? {
                    log.append(event).await?;
                }
            }
        }
        Ok(())
    }

    /// Official SDKTaskStarted/Progress/Notification/UpdatedMessage protocol:
    /// https://code.claude.com/docs/en/agent-sdk/typescript#sdktaskstartedmessage
    /// Process these before parent_tool_use_id routing: nested tasks belong
    /// in the common task registry too, even when their traffic is mirrored.
    fn native_task(&mut self, v: &Value) -> bool {
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
            // does not promise a shell command for every kind of task. A
            // foreground one holds the turn until it ends or the CLI moves it.
            let foreground = v["is_backgrounded"].as_bool() == Some(false);
            let id = bg.start_native(
                &self.scope,
                task_id,
                run,
                v["description"].as_str().unwrap_or(""),
                v["task_type"].as_str().unwrap_or(""),
                foreground,
            );
            if let Some(call) = v["tool_use_id"].as_str().filter(|c| !c.is_empty()) {
                self.call_tasks.insert(call.to_owned(), id);
            }
            return true;
        }
        // The CLI moved a foreground command or subagent to the background
        // (Send now, docs/background.md §4): a background task from now on.
        if subtype == "task_updated" && v["patch"]["is_backgrounded"].as_bool() == Some(true) {
            bg.background_native(&self.scope, task_id, run);
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
                self.calls
                    .insert(id.to_owned(), (made_in.map(str::to_owned), b["name"].as_str().unwrap_or("").to_owned(), b["input"].clone()));
                if b["name"].as_str().is_some_and(|n| n.starts_with("mcp__")) {
                    self.mcp_calls.insert(id.to_owned());
                }
            }
        }
    }

    /// The subagent started by call `call`, mirrored from now on.
    async fn child(&mut self, call: &str, sink: &Arc<dyn EventSink>) -> anyhow::Result<Option<(String, Arc<dyn EventSink>)>> {
        if let Some(c) = self.children.get(call) {
            return Ok(Some((c.id.clone(), c.log.clone())));
        }
        let Some(obs) = self.observer.clone() else { return Ok(None) };
        let Some((made_in, tool, input)) = self.calls.get(call).cloned() else { return Ok(None) };
        // parent_tool_use_id is also present on ordinary tool traffic (notably
        // Bash); only Claude's agent-spawning tools represent mirrored children.
        if !matches!(tool.as_str(), "Agent" | "Task") {
            return Ok(None);
        }
        // Its parent: the session, or the subagent that made the call.
        let (parent_id, parent_log, parent_depth) = match made_in.as_deref().and_then(|m| self.children.values().find(|c| c.id == m)) {
            Some(c) => (c.id.clone(), c.log.clone(), c.depth),
            None => (obs.session_id.clone(), sink.clone(), crate::subagent::depth_of(&obs.spec)),
        };
        let id = uuid::Uuid::now_v7().to_string();
        let description = input["description"].as_str().unwrap_or("Subagent").to_owned();
        let agent = input["subagent_type"].as_str().filter(|a| *a != "general-purpose").unwrap_or_default().to_owned();
        let spawn = SubagentSpawned {
            call_id: call.to_owned(),
            child_session_id: id.clone(),
            agent: agent.clone(),
            description: description.clone(),
            background_task_id: self.call_tasks.get(call).cloned().unwrap_or_default(),
            model: None,
            remote: None,
        };
        parent_log.append(session_event::Kind::SubagentSpawned(spawn.clone())).await?;
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
        let log: Arc<dyn EventSink> = Arc::new(crate::session::Writer::new(id.clone(), obs.journal.clone(), obs.link.clone(), 2));
        log.append(session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })).await?;
        self.children
            .insert(call.to_owned(), Child { id: id.clone(), log: log.clone(), depth: parent_depth + 1, open: true, parent_log, spawn });
        Ok(Some((id, log)))
    }

    /// The call that started a subagent got its result: its turn is over,
    /// unless it runs in the background (that result only says it started).
    async fn ended(&mut self, call: &str, is_error: bool) -> anyhow::Result<()> {
        if self.backgrounded.contains(call) {
            return Ok(());
        }
        let reason = if is_error { TurnEndReason::Error } else { TurnEndReason::Completed };
        self.end_child(call, reason).await
    }

    async fn end_child(&mut self, call: &str, reason: TurnEndReason) -> anyhow::Result<()> {
        if let Some(c) = self.children.get_mut(call).filter(|c| c.open) {
            c.open = false;
            c.log.append(session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: reason as i32 })).await?;
        }
        Ok(())
    }

    /// A task Claude Code started for a call (`tool_use_id`): a subagent in
    /// the background is noted as such when it starts, and its mirror ends
    /// with the task's last status.
    async fn task_of_call(&mut self, v: &Value) -> anyhow::Result<()> {
        // A subagent the CLI moved to the background: its call's result comes
        // now, and the subagent goes on.
        if v["subtype"] == "task_updated"
            && v["patch"]["is_backgrounded"].as_bool() == Some(true)
            && let Some(call) = v["task_id"].as_str().and_then(|t| self.task_calls.get(t))
        {
            self.backgrounded.insert(call.clone());
        }
        let Some(call) = v["tool_use_id"].as_str().filter(|c| !c.is_empty()) else { return Ok(()) };
        let reason = match v["subtype"].as_str().unwrap_or("") {
            "task_started" => {
                if v["is_backgrounded"].as_bool() == Some(true) {
                    self.backgrounded.insert(call.to_owned());
                }
                if let Some(task) = v["task_id"].as_str() {
                    self.task_calls.insert(task.to_owned(), call.to_owned());
                }
                if let (Some(id), Some(child)) = (self.call_tasks.get(call), self.children.get_mut(call))
                    && child.spawn.background_task_id != *id
                {
                    let mut spawn = child.spawn.clone();
                    spawn.background_task_id = id.clone();
                    child.parent_log.append(session_event::Kind::SubagentSpawned(spawn.clone())).await?;
                    child.spawn = spawn;
                }
                return Ok(());
            }
            "task_notification" => v["status"].as_str(),
            "task_updated" => v["patch"]["status"].as_str(),
            _ => None,
        };
        let reason = match reason {
            Some("completed") => TurnEndReason::Completed,
            Some("failed") => TurnEndReason::Error,
            Some("stopped" | "killed") => TurnEndReason::Interrupted,
            _ => return Ok(()),
        };
        self.end_child(call, reason).await
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
                self.note_usage(Some(call), &v["message"], sink).await?;
                self.note_calls(&v["message"]["content"], Some(&id));
                let blocks = assistant_blocks(v);
                if !blocks.is_empty() {
                    let stop_reason = v["message"]["stop_reason"].as_str().unwrap_or("").to_owned();
                    log.append(session_event::Kind::Assistant(AssistantMessage { turn: 1, blocks, stop_reason })).await?;
                }
            }
            "user" => {
                self.record_usage(Some(call), sink).await?;
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
                    log.append(session_event::Kind::User(UserMessage {
                        text,
                        delegated_by: call.to_owned(),
                        origin_recorded: true,
                        ..Default::default()
                    }))
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
        // The subscription's limits: for the head, not the transcript.
        if v["type"] == "rate_limit_event" {
            sink.ephemeral(ephemeral_event::Kind::RateLimit(c6_proto::RateLimit { json: v["rate_limit_info"].to_string() }));
            return Ok(());
        }
        if self.native_task(v) {
            self.saw_output = true;
            return self.task_of_call(v).await;
        }
        // A subagent's traffic goes to its mirror (its report also comes as
        // the call's tool result).
        if let Some(call) = v["parent_tool_use_id"].as_str() {
            // Progress of an MCP tool's call, not a subagent of Claude Code's.
            if self.mcp_calls.contains(call) {
                return Ok(());
            }
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
                            ..Default::default()
                        })),
                        Some("thinking_delta") => sink.ephemeral(ephemeral_event::Kind::Thinking(TextDelta {
                            turn: self.turn,
                            text: d["thinking"].as_str().unwrap_or("").into(),
                            ..Default::default()
                        })),
                        _ => {}
                    }
                }
            }
            "assistant" => {
                self.note_usage(None, &v["message"], sink).await?;
                self.note_calls(&v["message"]["content"], None);
                let blocks = assistant_blocks(v);
                if !blocks.is_empty() {
                    let stop_reason = v["message"]["stop_reason"].as_str().unwrap_or("").to_owned();
                    sink.append(session_event::Kind::Assistant(AssistantMessage { turn: self.turn, blocks, stop_reason })).await?;
                }
            }
            "user" => {
                self.record_usage(None, sink).await?;
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
                let open: Vec<Option<String>> = self.call_usage.keys().cloned().collect();
                for who in open {
                    self.record_usage(who.as_deref(), sink).await?;
                }
                // A Claude Code that doesn't say each call's usage: the turn's
                // total stands in (its context then reads high).
                if !std::mem::take(&mut self.per_call) {
                    let u = &v["usage"];
                    let model = v["modelUsage"].as_object().and_then(|m| m.keys().next().cloned()).unwrap_or_default();
                    let window = v["modelUsage"][&model]["contextWindow"].as_u64().unwrap_or_else(|| window_of(&model));
                    sink.append(session_event::Kind::Usage(usage(model, u, window))).await?;
                }
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

/// A model call's usage (Anthropic's names, as Claude Code reports them).
fn usage(model: String, u: &Value, context_window: u64) -> Usage {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Usage {
        model,
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read_tokens: n("cache_read_input_tokens"),
        cache_write_tokens: n("cache_creation_input_tokens"),
        reasoning_tokens: 0,
        context_window,
    }
}

/// The window of the model a call says it ran on (`claude-opus-5-5`, not
/// the session's alias, `opus`); 0 when it doesn't say.
fn window_of(model: &str) -> u64 {
    if model.is_empty() { 0 } else { c6_llm::window::built_in("anthropic", model) }
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

/// Narration indexes name summarized updates encoded as thinking, not reasoning.
/// Use original content positions before filtering empty or unsupported blocks.
fn assistant_blocks(v: &Value) -> Vec<ContentBlock> {
    v["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, b)| {
            let narration =
                v["narration_block_indexes"].as_array().is_some_and(|indexes| indexes.iter().any(|n| n.as_u64() == Some(i as u64)));
            assistant_block(b, narration)
        })
        .collect()
}

fn assistant_block(b: &Value, narration: bool) -> Option<ContentBlock> {
    let block = match b["type"].as_str()? {
        "text" => content_block::Block::Text(b["text"].as_str()?.to_owned()),
        "thinking" if narration => content_block::Block::Narration(b["thinking"].as_str().filter(|t| !t.is_empty())?.to_owned()),
        "thinking" => content_block::Block::Thinking(b["thinking"].as_str().filter(|t| !t.is_empty())?.to_owned()),
        "tool_use" => content_block::Block::ToolUse(ToolUse {
            id: b["id"].as_str()?.to_owned(),
            name: b["name"].as_str()?.to_owned(),
            input_json: b["input"].to_string(),
        }),
        _ => return None,
    };
    Some(ContentBlock { block: Some(block), ..Default::default() })
}

#[cfg(test)]
pub(crate) mod tests {
    /// The fake `claude` these tests use (not the process's environment,
    /// which tests running beside them read).
    pub(super) static BIN: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use c6_proto::SessionEvent;

    use super::*;

    #[test]
    fn sdk_notice_uses_structured_task_labels_not_summary_prose_or_ids() {
        let labels = HashMap::from([("sdk-id".into(), ("Run checks".into(), "local_bash".into()))]);
        let event = serde_json::json!({"task_id": "sdk-id", "status": "failed", "summary": "## Actual result\nOne failed."});
        let notice = sdk_task_notice(&event, &labels);
        assert_eq!((notice.kind.as_str(), notice.title.as_str(), notice.status.as_str()), ("command", "Run checks", "failed"));
        assert!(notice.session_id.is_empty(), "a CLI task id is not a session id");
        assert_eq!(notice.markdown, "## Actual result\nOne failed.");
        let agent =
            sdk_task_notice(&serde_json::json!({"task_type": "local_agent", "description": "Review", "status": "completed"}), &labels);
        assert_eq!((agent.kind.as_str(), agent.title.as_str()), ("subagent", "Review"));
        let unknown = sdk_task_notice(&serde_json::json!({"task_id": "raw-id", "summary": "Agent XYZ completed"}), &labels);
        assert_eq!(unknown.kind, "task");
        assert!(unknown.title.is_empty(), "do not parse prose or display the SDK id");
    }

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
    async fn narration_and_text_survive_interleaved_tools_in_parent_and_child() {
        for child in [false, true] {
            let recorded = Arc::new(VecSink::default());
            let sink: Arc<dyn EventSink> = if child { Arc::new(VecSink::default()) } else { recorded.clone() };
            let mut mapper = Mapper {
                turn: 436,
                scope: "conversation".into(),
                output_root: None,
                saw_output: false,
                result: None,
                interrupted: false,
                observer: None,
                calls: HashMap::new(),
                mcp_calls: HashSet::new(),
                children: HashMap::new(),
                backgrounded: HashSet::new(),
                task_calls: HashMap::new(),
                call_tasks: HashMap::new(),
                call_usage: HashMap::new(),
                per_call: false,
            };
            if child {
                mapper.children.insert(
                    "task".into(),
                    Child {
                        id: "child".into(),
                        log: recorded.clone(),
                        depth: 1,
                        open: true,
                        parent_log: recorded.clone(),
                        spawn: SubagentSpawned::default(),
                    },
                );
            }
            // Recorded Claude Code 2.1.311 shape: narration indexes refer to
            // the original content array, including empty/unsupported blocks.
            let frames = [
                serde_json::json!({"type": "assistant", "narration_block_indexes": [3], "message": {"content": [
                    {"type": "thinking", "thinking": ""},
                    {"type": "thinking", "thinking": "Private reasoning"},
                    {"type": "future_block"},
                    {"type": "thinking", "thinking": "The loop limit protects against cycles."},
                    {"type": "text", "text": "Checking the setting now."},
                    {"type": "tool_use", "id": "bash", "name": "Bash", "input": {"command": "true"}}
                ], "stop_reason": "tool_use"}}),
                serde_json::json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "bash", "content": "ok"}
                ]}}),
                serde_json::json!({"type": "assistant", "message": {"content": [
                    {"type": "text", "text": "Cleanup is under way."}
                ], "stop_reason": "end_turn"}}),
            ];
            for mut frame in frames {
                if child {
                    frame["parent_tool_use_id"] = "task".into();
                }
                mapper.handle(&frame, &sink).await.unwrap();
            }
            let events = recorded.0.lock().unwrap();
            assert_eq!(events.len(), 3);
            let blocks: Vec<_> = events
                .iter()
                .filter_map(|e| match &e.kind {
                    Some(session_event::Kind::Assistant(a)) => Some(&a.blocks),
                    _ => None,
                })
                .flatten()
                .map(|b| b.block.as_ref().unwrap())
                .collect();
            use content_block::Block;
            assert!(
                matches!(blocks.as_slice(), [Block::Thinking(t), Block::Narration(n), Block::Text(before), Block::ToolUse(_), Block::Text(after)]
                if t == "Private reasoning" && n == "The loop limit protects against cycles."
                    && before == "Checking the setting now." && after == "Cleanup is under way.")
            );
        }
    }

    #[test]
    fn narration_metadata_is_optional_and_only_changes_indexed_thinking() {
        for indexes in
            [Value::Null, serde_json::json!([]), serde_json::json!([1]), serde_json::json!(["0", -1, 99]), serde_json::json!("invalid")]
        {
            let frame = serde_json::json!({"narration_block_indexes": indexes, "message": {"content": [
                {"type": "thinking", "thinking": "Reasoning"}, {"type": "text", "text": "Reply"}
            ]}});
            let blocks = assistant_blocks(&frame);
            assert!(matches!(&blocks[0].block, Some(content_block::Block::Thinking(t)) if t == "Reasoning"));
            assert!(matches!(&blocks[1].block, Some(content_block::Block::Text(t)) if t == "Reply"));
        }
    }

    #[test]
    fn recorded_coordinator_summary_keeps_narration_provenance() {
        // Relevant fields from the Coordinator's Claude Code 2.1.311 JSONL,
        // lines 28939–28943, 2026-11-08. Opaque signatures, usage and tool
        // inputs are omitted. The actual signatures classify the empty block
        // as "thinking" and the 264-character summary as "narration".
        // Local JSONL has no narration_block_indexes; the SDK adds them from
        // those signatures. They index this frame, not apiBlockIndex or the
        // non-empty blocks remaining after filtering.
        let frames: Vec<Value> = serde_json::from_str(
            r#"[
                {"type":"assistant","thinkingDisplay":"updates","apiBlockIndex":0,"message":{
                    "id":"msg_011Cgq1iDTmVz2S73WksHj73","content":[{"type":"thinking","thinking":""}]
                }},
                {"type":"assistant","thinkingDisplay":"updates","apiBlockIndex":1,"message":{
                    "id":"msg_011Cgq1iDTmVz2S73WksHj73","content":[{"type":"thinking","thinking":"Carbon6 is capping how often helper notices can wake me while I'm idle, and I'm going to ask both helpers to report only on changes, not progress. Each update currently costs two wake-ups because the helpers send both a session message and an end-of-turn report.\n\n"}]
                }},
                {"type":"assistant","apiBlockIndex":2,"message":{
                    "id":"msg_011Cgq1iDTmVz2S73WksHj73","content":[{"type":"tool_use","id":"toolu_01YaxdNrRaZikJDjiqutx9oA","name":"mcp__carbon6__SendMessage","input":{}}]
                }},
                {"type":"assistant","apiBlockIndex":3,"message":{
                    "id":"msg_011Cgq1iDTmVz2S73WksHj73","content":[{"type":"tool_use","id":"toolu_01MhE8gNMSvYn8xtDpSpa6PD","name":"mcp__carbon6__SendMessage","input":{}}]
                }}
            ]"#,
        )
        .unwrap();
        let summary = frames[1]["message"]["content"][0]["thinking"].as_str().unwrap();
        assert_eq!(summary.len(), 264);

        for batched in [false, true] {
            for narration in [false, true] {
                let mut frames = frames.clone();
                if batched {
                    let content: Vec<_> = frames.iter().flat_map(|f| f["message"]["content"].as_array().unwrap().clone()).collect();
                    frames.truncate(1);
                    frames[0]["message"]["content"] = content.into();
                }
                if narration {
                    // SDK metadata reconstructed from the actual signature
                    // classification, not copied from the local JSONL.
                    let (frame, block) = if batched { (0, 1) } else { (1, 0) };
                    frames[frame]["narration_block_indexes"] = serde_json::json!([block]);
                }
                let blocks: Vec<_> = frames.iter().flat_map(assistant_blocks).collect();
                assert_eq!(blocks.len(), 3, "empty thinking is omitted, not the summary or either tool");
                match &blocks[0].block {
                    Some(content_block::Block::Narration(text)) if narration => assert_eq!(text, summary),
                    Some(content_block::Block::Thinking(text)) if !narration => assert_eq!(text, summary),
                    other => panic!("unexpected summary classification: {other:?}"),
                }
                for (block, id) in blocks[1..].iter().zip(["toolu_01YaxdNrRaZikJDjiqutx9oA", "toolu_01MhE8gNMSvYn8xtDpSpa6PD"]) {
                    assert!(matches!(&block.block, Some(content_block::Block::ToolUse(tool)) if tool.id == id));
                }
            }
        }
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
            mcp_calls: HashSet::new(),
            children: HashMap::new(),
            backgrounded: HashSet::new(),
            task_calls: HashMap::new(),
            call_tasks: HashMap::new(),
            call_usage: HashMap::new(),
            per_call: false,
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
    async fn native_agent_mirrors_link_exact_registry_ids_in_either_start_order() {
        for (task_first, backgrounded) in [(true, true), (false, true), (true, false), (false, false)] {
            let dir = tempfile::tempdir().unwrap();
            let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tools.background.set_sink(Some(tx));
            let observer = observer(dir.path(), tools.clone()).await;
            let journal = observer.journal.clone();
            let recorded = Arc::new(VecSink::default());
            let sink: Arc<dyn EventSink> = recorded.clone();
            let mut mapper = Mapper {
                turn: 1,
                scope: "cli-conversation".into(),
                output_root: None,
                saw_output: false,
                result: None,
                interrupted: false,
                observer: Some(observer),
                calls: HashMap::new(),
                mcp_calls: HashSet::new(),
                children: HashMap::new(),
                backgrounded: HashSet::new(),
                task_calls: HashMap::new(),
                call_tasks: HashMap::new(),
                call_usage: HashMap::new(),
                per_call: false,
            };
            let call = serde_json::json!({"type":"assistant","message":{"content":[{
                "type":"tool_use","id":"agent-call","name":"Agent",
                "input":{"description":"Review session layout","subagent_type":"explorer","prompt":"Review it."}
            }]}});
            let start = serde_json::json!({
                "type":"system","subtype":"task_started","task_id":"native-agent","run_id":"run-1",
                "tool_use_id":"agent-call","description":"SDK task description","task_type":"local_agent",
                "is_backgrounded":backgrounded
            });
            let traffic = serde_json::json!({"type":"assistant","parent_tool_use_id":"agent-call",
                "message":{"content":[{"type":"text","text":"Checking the layout."}]}});
            mapper.handle(&call, &sink).await.unwrap();
            for frame in if task_first { [&start, &traffic] } else { [&traffic, &start] } {
                mapper.handle(frame, &sink).await.unwrap();
            }
            let task = rx.try_recv().unwrap();
            assert_eq!(task.native_type, "local_agent");
            assert_eq!(task.foreground, !backgrounded, "linking does not promote foreground work to background work");
            assert_eq!(task.id, "claude:[\"cli-conversation\",\"native-agent\",\"run\",\"run-1\"]");
            let spawns = || {
                recorded
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|e| match &e.kind {
                        Some(session_event::Kind::SubagentSpawned(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let linked = spawns();
            assert_eq!(linked.len(), if task_first { 1 } else { 2 });
            let last = linked.last().unwrap();
            assert_eq!(
                (last.call_id.as_str(), last.description.as_str(), last.agent.as_str()),
                ("agent-call", "Review session layout", "explorer")
            );
            assert_eq!(last.background_task_id, task.id, "the exact registry ID, not the native task ID or an inferred name");
            if !task_first {
                assert!(linked[0].background_task_id.is_empty());
                let mut original = linked[0].clone();
                original.background_task_id = task.id.clone();
                assert_eq!(original, *last, "the late link preserves the original child, call and metadata");
            }
            assert_eq!(mapper.children.len(), 1);
            assert_eq!(mapper.children["agent-call"].id, last.child_session_id);
            let (spec, child_events) = journal.load_session(&last.child_session_id).await.unwrap().unwrap();
            assert!(spec.delegation.unwrap().observed, "the mirror stays read-only");
            assert_eq!(child_events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::Started(_)))).count(), 1);
            // A replayed start must not append another link or create a child.
            mapper.handle(&start, &sink).await.unwrap();
            assert_eq!(spawns(), linked);
            if !backgrounded {
                mapper
                    .handle(
                        &serde_json::json!({"type":"system","subtype":"task_updated",
                    "task_id":"native-agent","run_id":"run-1","patch":{"is_backgrounded":true}}),
                        &sink,
                    )
                    .await
                    .unwrap();
                let moved = rx.try_recv().unwrap();
                assert_eq!(moved.id, task.id);
                assert!(!moved.foreground, "only the native lifecycle moves the task into the background");
                assert_eq!(spawns(), linked, "backgrounding keeps the existing mirror identity link");
            }
            assert_eq!(
                tools.background.native_stop(&task.id).as_deref(),
                Some("native-agent"),
                "the underlying native task retains its real Stop target"
            );
        }
    }

    #[tokio::test]
    async fn bash_native_tasks_are_not_mirrored_as_subagents() {
        for (backgrounded, moved) in [(false, false), (false, true), (true, false)] {
            let dir = tempfile::tempdir().unwrap();
            let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tools.background.set_sink(Some(tx));
            let journal = Arc::new(crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap());
            let observer = Observer {
                tools,
                journal: journal.clone(),
                link: Arc::default(),
                session_id: "carbon6-session".into(),
                spec: CreateSession::default(),
                started: SessionStarted::default(),
            };
            let vec_sink = Arc::new(VecSink::default());
            let sink: Arc<dyn EventSink> = vec_sink.clone();
            let mut mapper = Mapper {
                turn: 1,
                scope: "cli-conversation".into(),
                output_root: Some(dir.path().to_owned()),
                saw_output: false,
                result: None,
                interrupted: false,
                observer: Some(Arc::new(observer)),
                calls: HashMap::new(),
                mcp_calls: HashSet::new(),
                children: HashMap::new(),
                backgrounded: HashSet::new(),
                task_calls: HashMap::new(),
                call_tasks: HashMap::new(),
                call_usage: HashMap::new(),
                per_call: false,
            };
            // Recorded stream shapes for a foreground Bash tool call and its
            // native local_bash lifecycle; Bash can also carry parent_tool_use_id.
            for line in [
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"bash-call","name":"Bash","input":{"command":"pwd","description":"Run pwd"}}]},"parent_tool_use_id":null}"#,
                r#"{"type":"system","subtype":"task_started","task_id":"bash-task","run_id":"run-1","tool_use_id":"bash-call","description":"Run pwd","task_type":"local_bash","is_backgrounded":false,"parent_tool_use_id":"bash-call"}"#,
                r#"{"type":"assistant","parent_tool_use_id":"bash-call","message":{"content":[{"type":"text","text":"pwd running"}]}}"#,
            ] {
                let mut v: Value = serde_json::from_str(line).unwrap();
                if v["subtype"] == "task_started" {
                    v["is_backgrounded"] = backgrounded.into();
                }
                mapper.handle(&v, &sink).await.unwrap();
            }
            if moved {
                mapper.handle(&serde_json::json!({"type":"system","subtype":"task_updated","task_id":"bash-task","run_id":"run-1","patch":{"is_backgrounded":true}}), &sink).await.unwrap();
                mapper.handle(&serde_json::json!({"type":"assistant","parent_tool_use_id":"bash-call","message":{"content":[{"type":"text","text":"pwd backgrounded"}]}}), &sink).await.unwrap();
            }
            for line in [
                r#"{"type":"system","subtype":"task_notification","task_id":"bash-task","run_id":"run-1","status":"completed","summary":"Done","parent_tool_use_id":"bash-call"}"#,
                r#"{"type":"assistant","parent_tool_use_id":"bash-call","message":{"content":[{"type":"text","text":"pwd completed"}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"bash-call","content":"Done"}]},"parent_tool_use_id":null}"#,
            ] {
                mapper.handle(&serde_json::from_str(line).unwrap(), &sink).await.unwrap();
            }
            assert!(mapper.children.is_empty());
            assert!(!vec_sink.0.lock().unwrap().iter().any(|event| matches!(event.kind, Some(session_event::Kind::SubagentSpawned(_)))));
            assert!(journal.unacked().await.unwrap().is_empty(), "no child session was journaled");
            let tasks: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            assert_eq!(tasks.len(), if moved { 3 } else { 2 });
            assert_eq!((tasks[0].native_type.as_str(), tasks[0].foreground), ("local_bash", !backgrounded));
            assert_eq!(tasks.last().unwrap().state, c6_tools::TaskState::Finished, "Bash still has its native task lifecycle");
        }
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

    /// Private fixture roots only. Held Drop intentionally survives worker
    /// loss, so tests must separately kill freshly revalidated groups even
    /// when an assertion unwinds before their async success-path cleanup.
    pub(crate) struct HeldFixtureCleanup(pub PathBuf);

    impl Drop for HeldFixtureCleanup {
        fn drop(&mut self) {
            let Ok(entries) = std::fs::read_dir(&self.0) else { return };
            for entry in entries.flatten() {
                let dir = entry.path();
                let Some(meta) = std::fs::read(dir.join("meta.json")).ok().and_then(|data| serde_json::from_slice::<HeldMeta>(&data).ok())
                else {
                    continue;
                };
                if meta.pgid > 0 && held_group_owned(&meta, &dir) {
                    let _ = killpg(Pid::from_raw(meta.pgid), Signal::SIGKILL);
                }
            }
        }
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
        let launch = Launch { conversation: conversation.into(), ..Default::default() };
        Process::spawn(bin, launch, resume, cwd, "opus", Arc::default(), None, observer).unwrap()
    }

    fn input(id: &str, text: &str) -> PendingInput {
        PendingInput { command_id: id.into(), text: text.into(), human_origin: true, ..Default::default() }
    }

    #[test]
    fn only_explicit_human_inputs_are_yours() {
        let human = input("human", "Continue.");
        assert!(is_yours(&human));
        assert!(!is_yours(&PendingInput { human_origin: false, ..human.clone() }));
        // Even an inconsistent human flag cannot override server provenance.
        assert!(!is_yours(&PendingInput {
            api_origin: Some(c6_proto::ApiMessageOrigin { token_name: "build-bot".into() }),
            ..human.clone()
        }));
        assert!(!is_yours(&PendingInput { from_session_id: "other".into(), ..human.clone() }));
        assert!(!is_yours(&PendingInput { delegated_by: "call".into(), ..human.clone() }));
        for command in ["task-notification", "session-message"] {
            assert!(!is_yours(&PendingInput { command: command.into(), ..human.clone() }));
        }
    }

    #[test]
    fn input_content_labels_api_and_unknown_origins_without_changing_humans_or_sessions() {
        let human = input("human", "Raw message");
        assert_eq!(user_content(&human, None), "Raw message");
        let unknown = PendingInput { human_origin: false, ..human.clone() };
        assert_eq!(user_content(&unknown, None), "Message sent by an automation, not typed by the user:\nRaw message");
        let api = PendingInput {
            api_origin: Some(c6_proto::ApiMessageOrigin { token_name: "build\n\"bot\"".into() }),
            prompt: "Expanded command".into(),
            ..human.clone()
        };
        assert_eq!(
            user_content(&api, Some("Preamble")),
            "Preamble\n\nMessage sent via the API (token \"build\\n\\\"bot\\\"\"), not typed by the user:\nExpanded command"
        );
        assert!(api.attributed_text().starts_with("Message sent via the API"), "API overrides even an inconsistent human flag");
        let unnamed = PendingInput { api_origin: Some(Default::default()), ..unknown.clone() };
        assert_eq!(user_content(&unnamed, None), "Message sent via the API, not typed by the user:\nRaw message");
        let session =
            PendingInput { from_session_id: "other".into(), prompt: "<session-message>Existing wrapper</session-message>".into(), ..api };
        assert_eq!(user_content(&session, None), "<session-message>Existing wrapper</session-message>");
        for command in ["task-notification", "session-message"] {
            assert_eq!(
                user_content(&PendingInput { command: command.into(), ..unknown.clone() }, None),
                "Message sent by an automation, not typed by the user:\nRaw message"
            );
        }
    }

    /// A dead leader is not proof that its group's writers stopped. The
    /// durable barrier survives reopening the journal and never signals the
    /// surviving member (both children are ours so the test can reap them).
    #[tokio::test]
    async fn pgid_barrier_survives_leader_loss_and_journal_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = crate::journal::Journal::open(&path).await.unwrap();
        let sid = "carbon6-owner";
        let mut leader = tokio::process::Command::new("sleep").arg("300").process_group(0).kill_on_drop(true).spawn().unwrap();
        let pgid = leader.id().unwrap() as i32;
        let mut member = tokio::process::Command::new("sleep").arg("300").process_group(pgid).kill_on_drop(true).spawn().unwrap();
        journal.track_claude_process(sid, pgid).await.unwrap();
        journal.track_claude_process(sid, pgid).await.unwrap();
        assert!(journal.claude_processes("not-the-owner").await.unwrap().is_empty());
        leader.kill().await.unwrap();
        assert!(confirm_stopped(&journal, sid, Duration::ZERO).await.is_err());
        assert_eq!(journal.claude_processes(sid).await.unwrap(), [pgid]);
        drop(journal);

        let journal = crate::journal::Journal::open(&path).await.unwrap();
        assert!(confirm_stopped(&journal, sid, Duration::from_millis(30)).await.is_err());
        assert_eq!(journal.claude_processes(sid).await.unwrap(), [pgid]);
        assert!(member.try_wait().unwrap().is_none(), "checking a persisted PGID must not signal it");
        member.kill().await.unwrap();
        confirm_stopped(&journal, sid, Duration::ZERO).await.unwrap();
        assert!(journal.claude_processes(sid).await.unwrap().is_empty());
    }

    /// The fake itself checks SQLite when input arrives: a Rust assertion
    /// after ready/turn would not prove the record preceded model input.
    fn pgid_checking_claude(dir: &Path) -> PathBuf {
        script(
            dir,
            "claude-pgid",
            r#"#!/usr/bin/env python3
import json, os, sqlite3, sys
for line in sys.stdin:
    request = json.loads(line)
    with sqlite3.connect('journal.db') as db:
        assert db.execute('SELECT pgid FROM claude_processes WHERE session_id = ?', ('0199-main',)).fetchall() == [(os.getpgrp(),)]
    with open('checked-inputs', 'a') as out:
        out.write(request['type'] + '\n')
    if request['type'] == 'control_request':
        print(json.dumps({'type': 'control_response', 'response': {'subtype': 'error', 'request_id': request['request_id'], 'error': 'test initialization rejected'}}), flush=True)
    else:
        print(json.dumps({'type': 'result', 'subtype': 'success', 'is_error': False, 'usage': {}}), flush=True)
"#,
        )
    }

    #[tokio::test]
    async fn nonheld_initialization_is_cleanup_recorded_before_handshake() {
        if !c6_testkit::have("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = pgid_checking_claude(dir.path());
        let observer = observer(dir.path(), Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()))).await;
        let mut p = spawn(&bin, dir.path(), "launch-conversation", false, Some(observer.clone()));
        let pgid = p.pgid.unwrap().as_raw();
        let records = dir.path().join("claude");
        p.record_in(&records).await.unwrap();
        let path = records.join("launch-conversation.pid");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{pgid}\nlaunch-conversation\n"));
        assert_eq!(observer.journal.claude_processes(&observer.session_id).await.unwrap(), [pgid]);
        assert!(!dir.path().join("checked-inputs").exists(), "recording precedes initialize input");
        assert!(!p.is_held(), "the cleanup record must not make initialization adoptable");
        assert!(Process::held_record(&records.join("held"), "launch-conversation").unwrap().is_none());
        assert!(p.ready(&uuid::Uuid::now_v7().to_string()).await.is_err());
        p.stop_for_rewind().await.unwrap();
        assert!(!path.exists());
        assert_eq!(observer.journal.claude_processes(&observer.session_id).await.unwrap(), [pgid]);
        confirm_stopped(&observer.journal, &observer.session_id, Duration::ZERO).await.unwrap();
    }

    #[tokio::test]
    async fn pgid_is_durable_before_rejected_initialization() {
        if !c6_testkit::have("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = pgid_checking_claude(dir.path());
        let observer = observer(dir.path(), Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()))).await;
        let mut p = spawn(&bin, dir.path(), "cli-conversation-not-owner", false, Some(observer.clone()));
        let pgid = p.pgid.unwrap().as_raw();
        let error = p.ready(&uuid::Uuid::now_v7().to_string()).await.unwrap_err();
        assert!(error.to_string().contains("test initialization rejected"), "{error:#}");
        assert_eq!(std::fs::read_to_string(dir.path().join("checked-inputs")).unwrap(), "control_request\n");
        assert_eq!(observer.journal.claude_processes(&observer.session_id).await.unwrap(), [pgid]);
        assert!(observer.journal.claude_processes("cli-conversation-not-owner").await.unwrap().is_empty());
        // Failed initialize attempts have no pidfile, but stopping/Drop must
        // not retire their durable barrier without a separate ESRCH check.
        assert!(p.pidfile.is_none());
        p.stop_for_rewind().await.unwrap();
        assert_eq!(observer.journal.claude_processes(&observer.session_id).await.unwrap(), [pgid]);
        confirm_stopped(&observer.journal, &observer.session_id, Duration::ZERO).await.unwrap();
        assert!(observer.journal.claude_processes(&observer.session_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pgid_is_durable_before_turn_input_and_process_reuse() {
        if !c6_testkit::have("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = pgid_checking_claude(dir.path());
        let observer = observer(dir.path(), Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()))).await;
        let mut p = spawn(&bin, dir.path(), "cli-conversation-not-owner", false, Some(observer.clone()));
        let sink: Arc<dyn EventSink> = Arc::new(VecSink::default());
        for turn in 1..=2 {
            let end = run(&mut p, turn, &[input(&format!("c{turn}"), "hi")], &sink).await;
            assert_eq!(end.reason, Some(TurnEndReason::Completed));
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("checked-inputs")).unwrap(), "user\nuser\n");
        p.stop_for_rewind().await.unwrap();
        confirm_stopped(&observer.journal, &observer.session_id, Duration::ZERO).await.unwrap();
    }

    /// A turn with nothing joining it.
    async fn run(p: &mut Process, turn: u64, inputs: &[PendingInput], sink: &Arc<dyn EventSink>) -> TurnEnd {
        let more = tokio::sync::Notify::new();
        p.turn(turn, inputs.to_vec(), false, None, "opus", sink, &Joining::default, &more, &CancellationToken::new()).await.unwrap()
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
printf '%s\n' "$@" > args.txt
while [ $# -gt 0 ]; do [ "$1" = "--settings" ] && printf '%s' "$2" > settings.txt; [ "$1" = "--append-system-prompt" ] && printf '%s' "$2" > system.txt; [ "$1" = "--append-system-prompt-file" ] && cat "$2" > system.txt; [ "$1" = "--mcp-config" ] && printf '%s %s %s' "$2" "$MCP_TOOL_TIMEOUT" "$CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT" > mcp.txt; shift; done
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

    /// A process kept to some of Claude Code's own tools gets the flags for
    /// it, the guard hook beside the head's hooks, the built-in plugins off
    /// and claude.ai's connectors off; what its `system/init` shows beyond
    /// that is noted once in the transcript.
    #[tokio::test]
    async fn a_locked_down_process_gets_only_its_tools() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            "claude",
            r#"#!/bin/sh
read -r prompt
printf '%s\n' "$@" > args.txt
printf '%s' "$ENABLE_CLAUDEAI_MCP_SERVERS" > claudeai.txt
while [ $# -gt 0 ]; do [ "$1" = "--settings" ] && printf '%s' "$2" > settings.txt; shift; done
cat <<'EOF'
{"type":"system","subtype":"init","session_id":"s","tools":["Bash","Read","WebSearch","mcp__carbon6__doc","mcp__claude_ai_Gmail__send_message"],"mcp_servers":[{"name":"carbon6","status":"connected"},{"name":"claude.ai Gmail","status":"connected"}],"plugins":[{"name":"cc-plugin-agents-md"},{"name":"cc-plugin-telemetry"}]}
{"type":"system","subtype":"init","session_id":"s","tools":["Bash","Read","WebSearch"]}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"Done.","usage":{"input_tokens":1,"output_tokens":1}}
EOF
"#,
        );
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let launch = Launch {
            conversation: "0199-locked".into(),
            settings: Some(r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"check"}]}]}}"#.into()),
            native: Some(vec!["Bash".into(), "Read".into()]),
            ..Default::default()
        };
        let mut p = Process::spawn(&bin, launch, false, dir.path(), "opus", Arc::default(), None, None).unwrap();
        let end = run(&mut p, 1, &[input("c1", "hi")], &sink).await;
        assert_eq!(end.reason, Some(TurnEndReason::Completed));
        let args: Vec<String> = std::fs::read_to_string(dir.path().join("args.txt")).unwrap().lines().map(str::to_owned).collect();
        let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].clone());
        assert_eq!(after("--tools").as_deref(), Some("Bash,Read"));
        assert_eq!(after("--setting-sources").as_deref(), Some(""), "no settings files");
        assert!(args.iter().any(|a| a == "--strict-mcp-config") && args.iter().any(|a| a == "--disable-slash-commands"), "{args:?}");
        assert_eq!(std::fs::read_to_string(dir.path().join("claudeai.txt")).unwrap(), "false");
        let settings: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.txt")).unwrap()).unwrap();
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "the guard, then the head's hook");
        assert_eq!(pre[0]["matcher"], "*");
        assert!(pre[0]["hooks"][0]["command"].as_str().unwrap().ends_with(" claude-guard Bash Read"), "{}", pre[0]);
        assert_eq!(pre[1]["hooks"][0]["command"], "check");
        assert_eq!(settings["enabledPlugins"], serde_json::json!({"telemetry@builtin": false, "plugin-authoring@builtin": false}));
        let notes: Vec<String> = vec_sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::Notice(n)) => Some(n.message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 1, "once a process: {notes:?}");
        for extra in ["WebSearch", "mcp__claude_ai_Gmail__send_message", "MCP servers claude.ai Gmail", "plugins cc-plugin-telemetry"] {
            assert!(notes[0].contains(extra), "{extra}: {}", notes[0]);
        }
        assert!(!notes[0].contains("mcp__carbon6__doc") && !notes[0].contains("agents-md"), "{}", notes[0]);
    }

    /// Without a list it starts as it would without carbon6.
    #[tokio::test]
    async fn an_unlocked_process_keeps_claude_codes_own_setup() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "0199-open", false, None);
        assert_eq!(run(&mut p, 1, &[input("c1", "list")], &sink).await.reason, Some(TurnEndReason::Completed));
        let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        for flag in ["--tools", "--strict-mcp-config", "--setting-sources", "--disable-slash-commands", "--settings"] {
            assert!(!args.lines().any(|a| a == flag), "{flag} in {args}");
        }
        assert!(!vec_sink.0.lock().unwrap().iter().any(|e| matches!(e.kind, Some(session_event::Kind::Notice(_)))), "nothing to note");
    }

    /// The guard lets through the tools it was given and carbon6's, and
    /// refuses the rest and anything it cannot read.
    #[test]
    fn the_guard_refuses_other_tools() {
        let allowed = vec!["Bash".to_owned(), "Read".to_owned()];
        let call = |name: &str| {
            guard(&allowed, &serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": name, "tool_input": {}}).to_string())
        };
        assert_eq!(call("Bash"), Ok(()));
        assert_eq!(call("mcp__carbon6__Task"), Ok(()));
        let err = call("WebSearch").unwrap_err();
        assert!(err.contains("WebSearch") && err.contains("Bash, Read") && err.contains("mcp__carbon6__"), "{err}");
        assert!(call("mcp__claude_ai_Gmail__send_message").is_err());
        assert!(call("bash").is_err(), "names exactly");
        assert!(guard(&allowed, "not json").is_err());
        assert!(guard(&allowed, r#"{"tool_input":{"tool_name":"Bash"}}"#).is_err());
        assert!(guard(&[], &serde_json::json!({"tool_name": "Read"}).to_string()).is_err(), "none allowed");
    }

    #[test]
    fn guard_commands_quote_what_needs_it() {
        assert_eq!(shell_quote("/usr/local/bin/c6w"), "/usr/local/bin/c6w");
        assert_eq!(shell_quote("/opt/my tools/c6w"), "'/opt/my tools/c6w'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert!(guard_command(&["Bash".into()]).ends_with(" claude-guard Bash"));
    }

    /// Usage is recorded per model call, from its messages (the last says the
    /// most), with its model's window; the turn's total isn't added on top.
    #[tokio::test]
    async fn usage_is_recorded_per_model_call() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            "claude",
            r#"#!/bin/sh
read -r prompt
cat <<'EOF'
{"type":"system","subtype":"init","session_id":"s","model":"claude-opus-5-5"}
{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"text","text":"Listing."}],"usage":{"input_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50,"output_tokens":5}},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use","usage":{"input_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50,"output_tokens":30}},"parent_tool_use_id":null}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt"}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_2","model":"claude-opus-5-5","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn","usage":{"input_tokens":3,"cache_read_input_tokens":1080,"output_tokens":7}},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"Done.","usage":{"input_tokens":13,"output_tokens":37,"cache_read_input_tokens":2080,"cache_creation_input_tokens":50},"modelUsage":{"claude-opus-5-5":{}}}
EOF
"#,
        );
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let launch = Launch { conversation: "0199-usage".into(), ..Default::default() };
        let mut p = Process::spawn(&bin, launch, false, dir.path(), "opus", Arc::default(), None, None).unwrap();
        let more = tokio::sync::Notify::new();
        let end = p
            .turn(1, vec![input("c1", "list")], false, None, "opus", &sink, &Joining::default, &more, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(end.reason, Some(TurnEndReason::Completed));
        let events = vec_sink.0.lock().unwrap().clone();
        assert_eq!(
            events.iter().map(kind).collect::<Vec<_>>(),
            ["user", "turn_started", "assistant", "assistant", "usage", "tool_result", "assistant", "usage", "turn_ended"]
        );
        let usage: Vec<&Usage> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::Usage(u)) => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(
            usage.iter().map(|u| (u.input_tokens, u.cache_read_tokens, u.cache_write_tokens, u.output_tokens)).collect::<Vec<_>>(),
            [(10, 1000, 50, 30), (3, 1080, 0, 7)],
            "each call once, with its own numbers (the first's last message says its output)"
        );
        assert!(
            usage.iter().all(|u| u.model == "claude-opus-5-5" && u.context_window == 1_000_000),
            "the real model's window, not the alias's"
        );
    }

    #[tokio::test]
    async fn maps_a_turn_and_falls_back_from_resume() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let inputs = vec![input("c1", "list files")];
        let launch = Launch {
            conversation: "0199-session".into(),
            effort: Some("high".into()),
            settings: Some(r#"{"hooks":{}}"#.into()),
            // A NUL (as in a mail quoted in the memex brief) can't go on a
            // command line: it's left out, and the guidance goes by file.
            system_prompt: Some("carbon6's\0 guidance.".into()),
            ..Default::default()
        };
        let bridge = crate::mcp_bridge::Bridge::start().await.unwrap();
        let entry = || crate::mcp_bridge::Entry {
            session_id: "0199-session".into(),
            session: Arc::new(c6_tools::SessionTools::new(dir.path().to_owned())),
            tools: Vec::new(),
            cancel: Default::default(),
            journal: Arc::new(futures::executor::block_on(crate::journal::Journal::open(&dir.path().join("journal.db"))).unwrap()),
            matched: Default::default(),
            policies: None,
            instructions: String::new(),
            core: Vec::new(),
        };
        let more = tokio::sync::Notify::new();
        // A later turn tries --resume first; the fake has no conversation...
        let mut p =
            Process::spawn(&bin, launch.clone(), true, dir.path(), "opus", Arc::default(), Some(bridge.register(entry())), None).unwrap();
        let end = p.turn(2, inputs.clone(), false, None, "opus", &sink, &Joining::default, &more, &CancellationToken::new()).await.unwrap();
        assert_eq!(end, TurnEnd { reason: None, alive: false, no_conversation: true });
        assert_eq!(kinds(&vec_sink), ["user", "turn_started"], "the turn is logged once, before either attempt");
        // ... so a new one starts, which gets the preamble.
        let registration = bridge.register(entry());
        let mcp = registration.mcp_config();
        let mut p = Process::spawn(&bin, launch, false, dir.path(), "opus", Arc::default(), Some(registration), None).unwrap();
        let end =
            p.turn(2, inputs, true, Some("CONTEXT"), "opus", &sink, &Joining::default, &more, &CancellationToken::new()).await.unwrap();
        assert_eq!(end.reason, Some(TurnEndReason::Completed));
        assert_eq!(std::fs::read_to_string(dir.path().join("settings.txt")).unwrap(), r#"{"hooks":{}}"#, "the head's hooks are passed on");
        assert_eq!(std::fs::read_to_string(dir.path().join("system.txt")).unwrap(), "carbon6's guidance.", "in its system prompt");
        assert!(std::fs::read_to_string(dir.path().join("args.txt")).unwrap().contains("--append-system-prompt-file"), "from a file");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mcp.txt")).unwrap(),
            format!("{mcp} 86400000 86400000"),
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
            session_event::Kind::InputDelivered(_) => "input_delivered",
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

    /// A stand-in `claude` that starts two subagents in the background: their
    /// calls return at once, they keep working, and their tasks' notifications
    /// say how each ended (one completed, one failed).
    fn fake_claude_with_background_subagents(dir: &Path) -> PathBuf {
        script(
            dir,
            "claude-bg",
            r#"#!/bin/sh
read -r line
cat <<'JSON'
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_bg","name":"Task","input":{"description":"long build","prompt":"Build it.","run_in_background":true}},{"type":"tool_use","id":"toolu_bad","name":"Task","input":{"description":"doomed","prompt":"Try.","run_in_background":true}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}
{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"toolu_bg","description":"long build","task_type":"local_agent","is_backgrounded":true}
{"type":"system","subtype":"task_started","task_id":"a2","tool_use_id":"toolu_bad","description":"doomed","task_type":"local_agent","is_backgrounded":true}
{"type":"user","message":{"role":"user","content":"Build it."},"parent_tool_use_id":"toolu_bg"}
{"type":"user","message":{"role":"user","content":"Try."},"parent_tool_use_id":"toolu_bad"}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_bg","content":"Async agent launched"},{"type":"tool_result","tool_use_id":"toolu_bad","content":"Async agent launched"}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"content":[{"type":"text","text":"Building."}],"stop_reason":"end_turn"},"parent_tool_use_id":"toolu_bg"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Built."}],"stop_reason":"end_turn"},"parent_tool_use_id":"toolu_bg"}
{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"toolu_bg","status":"completed","summary":"Built."}
{"type":"system","subtype":"task_notification","task_id":"a2","tool_use_id":"toolu_bad","status":"failed","summary":"It broke."}
{"type":"assistant","message":{"content":[{"type":"text","text":"Both are done."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"Both are done.","usage":{"input_tokens":5,"output_tokens":20}}
JSON
"#,
        )
    }

    /// A subagent in the background keeps running after its call returns,
    /// and ends with its task: completed or failed, not when the call said
    /// it launched, and not left running for the parent to look busy.
    #[tokio::test]
    async fn background_subagents_end_with_their_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude_with_background_subagents(dir.path());
        let observer = observer(dir.path(), Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()))).await;
        let journal = observer.journal.clone();
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "0199-main", false, Some(observer));
        let end = run(&mut p, 1, &[input("c1", "go")], &sink).await;
        assert_eq!(end.reason, Some(TurnEndReason::Completed));
        // Before the process goes: nothing closes them on the way out.
        let main = vec_sink.0.lock().unwrap().clone();
        let spawned: Vec<SubagentSpawned> = main
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::SubagentSpawned(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(spawned.len(), 2);
        for (sp, want, last_text) in [(&spawned[0], TurnEndReason::Completed, "Built."), (&spawned[1], TurnEndReason::Error, "")] {
            let (_, child) = journal.load_session(&sp.child_session_id).await.unwrap().unwrap();
            let ends: Vec<i32> = child
                .iter()
                .filter_map(|e| match &e.kind {
                    Some(session_event::Kind::TurnEnded(t)) => Some(t.reason),
                    _ => None,
                })
                .collect();
            assert_eq!(ends, [want as i32], "{}: one end, with its task's status", sp.description);
            // It ended after its own work, not when its call returned.
            assert_eq!(kind(child.last().unwrap()), "turn_ended");
            if !last_text.is_empty() {
                let texts: Vec<String> = child
                    .iter()
                    .filter_map(|e| match &e.kind {
                        Some(session_event::Kind::Assistant(a)) => Some(format!("{:?}", a.blocks)),
                        _ => None,
                    })
                    .collect();
                assert!(texts.last().unwrap().contains(last_text), "{texts:?}");
            }
        }
        p.exited().await;
        for sp in &spawned {
            let (_, child) = journal.load_session(&sp.child_session_id).await.unwrap().unwrap();
            assert_eq!(child.iter().filter(|e| kind(e) == "turn_ended").count(), 1, "exiting doesn't end it twice");
        }
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
        let feed = || Joining { inputs: std::mem::take(&mut *queue.lock().unwrap()), now: Vec::new() };
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
        // With the agent once written, logged as yours once it takes it.
        assert_eq!(kinds(&vec_sink), ["user", "turn_started", "assistant", "input_delivered", "user", "assistant", "usage", "turn_ended"]);
        let events = vec_sink.0.lock().unwrap().clone();
        let Some(session_event::Kind::InputDelivered(d)) = &events[3].kind else { panic!() };
        assert_eq!(d.command_id, "c2");
        let Some(session_event::Kind::User(u)) = &events[4].kind else { panic!() };
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
        let turn = p.turn(1, vec![input("c1", "slow build")], false, None, "opus", &sink, &Joining::default, &more, &cancel);
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
        let metadata = u.task_notice.as_ref().expect("live SDK notification metadata");
        assert_eq!(metadata.markdown, "Build finished");
        assert_eq!(metadata.status, "completed");
        p.close().await;
    }

    /// A stand-in `claude` whose first turn leaves a build and a subagent's
    /// command running and ends; both end while it waits. With `wake` it
    /// starts a turn for the build by itself, as Claude Code usually does;
    /// without, it stays quiet (as seen with a 20-minute build). It answers
    /// whatever it is sent next.
    fn waiting_claude(dir: &Path, wake: bool) -> PathBuf {
        let woke = if wake {
            r#"printf '%s\n' '{"type":"system","subtype":"init"}'
say "The build passed."
done_ok"#
        } else {
            ""
        };
        script(
            dir,
            if wake { "claude-wakes" } else { "claude-quiet" },
            &format!(
                r#"#!/bin/sh
uuid_of() {{ printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }}
echo_of() {{ printf '{{"type":"user","isReplay":true,"uuid":"%s","message":{{"role":"user","content":"echo"}},"parent_tool_use_id":null}}\n' "$(uuid_of "$1")"; }}
say() {{ printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"%s"}}],"stop_reason":"end_turn"}},"parent_tool_use_id":null}}\n' "$1"; }}
done_ok() {{ printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"usage":{{}}}}'; }}
read -r line
printf '%s\n' '{{"type":"system","subtype":"init"}}'
echo_of "$line"
printf '%s\n' '{{"type":"system","subtype":"task_started","task_id":"build","tool_use_id":"toolu_b","description":"cargo build","task_type":"local_bash","is_backgrounded":true}}'
printf '%s\n' '{{"type":"system","subtype":"task_started","task_id":"sub","description":"the subagent tests","task_type":"local_bash","owned_by_subagent":true}}'
say "The build runs in the background, waiting for it."
done_ok
sleep 0.2
printf '%s\n' '{{"type":"system","subtype":"task_notification","task_id":"sub","status":"completed","summary":"The subagent tests passed"}}'
printf '%s\n' '{{"type":"system","subtype":"task_notification","task_id":"build","tool_use_id":"toolu_b","status":"completed","summary":"Build finished","output_file":"/tmp/build.out"}}'
{woke}
while read -r line; do
  printf '%s\n' '{{"type":"system","subtype":"init"}}'
  echo_of "$line"
  say "Continuing."
  done_ok
done
"#
            ),
        )
    }

    /// Reads it while no turn runs until `until` holds or a turn begins
    /// (true).
    async fn idle_until_or_turn(p: &mut Process, sink: &Arc<dyn EventSink>, until: impl Fn(&Process) -> bool) -> bool {
        while !until(p) {
            let v = p.next_line().await.expect("still up");
            if p.idle_line(v, sink).await.unwrap() {
                return true;
            }
        }
        false
    }

    /// A task of its own that ended while it waited, with no turn from
    /// Claude Code for it: the session can tell the agent (once), and the
    /// agent goes on. A subagent's task ending doesn't count.
    #[tokio::test]
    async fn a_finished_task_wakes_the_agent_when_claude_code_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let bin = waiting_claude(dir.path(), false);
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, Some(observer(dir.path(), tools).await));
        assert_eq!(run(&mut p, 1, &[input("c1", "build it")], &sink).await.reason, Some(TurnEndReason::Completed));
        assert!(p.wake_due().is_none(), "nothing has ended yet");
        let began = idle_until_or_turn(&mut p, &sink, |p| p.wake_due().is_some()).await;
        assert!(!began, "this Claude Code starts no turn for it");
        let (notice, task_notice) = p.take_unwoken().expect("the build's end is waiting");
        assert_eq!(task_notice.status, "completed");
        assert_eq!(task_notice.markdown, "Build finished");
        assert!(task_notice.session_id.is_empty());
        assert!(
            notice.contains(r#"status="completed""#)
                && notice.contains("Task: Build finished")
                && notice.contains("Output: /tmp/build.out"),
            "{notice}"
        );
        assert!(!notice.contains("subagent"), "its subagent's task is its subagent's to hear about: {notice}");
        assert!(p.wake_due().is_none() && p.take_unwoken().is_none(), "told once");
        // The session sends it as a notice; the agent goes on.
        let wake = PendingInput {
            command_id: "task:claude-wake:1".into(),
            text: notice,
            command: "task-notification".into(),
            task_notice: Some(task_notice),
            ..Default::default()
        };
        assert_eq!(run(&mut p, 2, &[wake], &sink).await.reason, Some(TurnEndReason::Completed));
        assert!(
            vec_sink
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(&e.kind, Some(session_event::Kind::Assistant(a)) if format!("{:?}", a.blocks).contains("Continuing.")))
        );
        p.close().await;
    }

    /// When Claude Code starts a turn for the task by itself, the session has
    /// nothing to add.
    #[tokio::test]
    async fn no_wake_is_needed_when_claude_code_wakes_by_itself() {
        let dir = tempfile::tempdir().unwrap();
        let bin = waiting_claude(dir.path(), true);
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, Some(observer(dir.path(), tools).await));
        assert_eq!(run(&mut p, 1, &[input("c1", "build it")], &sink).await.reason, Some(TurnEndReason::Completed));
        assert!(idle_until_or_turn(&mut p, &sink, |_| false).await, "it starts a turn for the build");
        assert!(p.wake_due().is_none() && p.take_unwoken().is_none(), "nothing for the session to send");
        assert_eq!(run(&mut p, 2, &[], &sink).await.reason, Some(TurnEndReason::Completed));
        p.close().await;
    }

    /// A task outlives the idle interval. Its terminal notification must
    /// not re-arm the last turn's expired deadline and close stdin before
    /// the assistant-first reply (no init or requested turn) arrives.
    #[tokio::test]
    async fn a_long_background_task_ending_keeps_its_self_started_reply() {
        for (subtype, status) in [
            ("task_notification", "completed"),
            ("task_notification", "failed"),
            ("task_notification", "stopped"),
            ("task_updated", "completed"),
            ("task_updated", "failed"),
            ("task_updated", "killed"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut terminal = serde_json::json!({"type": "system", "subtype": subtype, "task_id": "build", "tool_use_id": "toolu_build"});
            if subtype == "task_updated" {
                terminal["patch"] = serde_json::json!({"status": status});
            } else {
                terminal["status"] = status.into();
                terminal["summary"] = "Build ended".into();
            }
            let bin = script(
                dir.path(),
                "claude-task-end",
                &format!(
                    r#"#!/bin/sh
[ "$1" = --version ] && exit 0
read -r prompt
uuid=$(printf '%s' "$prompt" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
printf '{{"type":"user","isReplay":true,"uuid":"%s","message":{{"content":"echo"}}}}\n' "$uuid"
cat <<'EOF'
{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_build","name":"Bash","input":{{"command":"cargo build","run_in_background":true}}}}]}}}}
{{"type":"system","subtype":"task_started","task_id":"build","tool_use_id":"toolu_build","description":"cargo build","task_type":"local_bash","is_backgrounded":true}}
{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_build","content":"Running in background"}}]}}}}
{{"type":"result","subtype":"success","is_error":false,"usage":{{}}}}
EOF
# Fixture controls, not user inputs: let the test age the idle clock,
# then observe the task's end before the assistant starts replying.
read -r control
printf '%s\n' '{terminal}'
read -r control
cat <<'EOF'
{{"type":"assistant","message":{{"content":[{{"type":"text","text":"The build ended; checking the output."}}]}}}}
{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_after","name":"Bash","input":{{"command":"git status --short"}}}}]}}}}
{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_after","content":"clean"}}]}}}}
{{"type":"result","subtype":"success","is_error":false,"usage":{{}}}}
EOF
cat > /dev/null
"#
                ),
            );
            c6_testkit::runnable(&bin, &["--version"]).await;
            let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
            let vec_sink = Arc::new(VecSink::default());
            let sink: Arc<dyn EventSink> = vec_sink.clone();
            let mut p = spawn(&bin, dir.path(), "s", false, Some(observer(dir.path(), tools).await));
            assert_eq!(run(&mut p, 1, &[input("c1", "build it")], &sink).await.reason, Some(TurnEndReason::Completed));
            assert!(p.tasks_running());
            assert!(p.idle_until(IDLE).is_none(), "running tasks keep the process up");
            // Backdate only the product clock: no wall-clock wait or race
            // with a real child process on a paused Tokio clock.
            p.idle_since = tokio::time::Instant::now() - IDLE - Duration::from_secs(1);
            write_line(&p.stdin, &serde_json::json!({"fixture": "finish task"})).await.unwrap();
            c6_testkit::within("the long task's terminal notification", async {
                while p.tasks_running() {
                    let v = p.next_line().await.expect("still up");
                    assert!(!p.idle_line(v, &sink).await.unwrap(), "only the task ended; no turn yet");
                }
            })
            .await;
            let after_first = vec_sink.0.lock().unwrap().len();
            let deadline = p.idle_until(IDLE).expect("no running tasks");
            write_line(&p.stdin, &serde_json::json!({"fixture": "reply"})).await.unwrap();
            // The actor selects between these same futures. Prefer an
            // expired timer so the old loss is deterministic even if the
            // child already buffered its reply.
            let v = c6_testkit::within("the self-started reply, not idle shutdown", async {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => None,
                    v = p.next_line() => Some(v.expect("still up")),
                }
            })
            .await;
            let Some(v) = v else {
                p.close().await;
                panic!("idle shutdown discarded the reply after {subtype}: {status}");
            };
            assert!(p.idle_line(v, &sink).await.unwrap());
            assert!(p.wake_due().is_none() && p.take_unwoken().is_none(), "no duplicate fallback notice");
            assert_eq!(run(&mut p, 2, &[], &sink).await.reason, Some(TurnEndReason::Completed));
            let events = vec_sink.0.lock().unwrap().clone();
            let reply = &events[after_first..];
            assert_eq!(
                reply.iter().map(kind).collect::<Vec<_>>(),
                ["user", "turn_started", "assistant", "assistant", "tool_result", "usage", "turn_ended"]
            );
            assert!(matches!(&reply[0].kind, Some(session_event::Kind::User(u)) if u.command == "task-notification"));
            assert!(matches!(&reply[1].kind, Some(session_event::Kind::TurnStarted(t)) if t.turn == 2));
            assert!(matches!(&reply[2].kind, Some(session_event::Kind::Assistant(a))
                if a.turn == 2 && matches!(&a.blocks[0].block, Some(content_block::Block::Text(t)) if t == "The build ended; checking the output.")));
            assert!(matches!(&reply[3].kind, Some(session_event::Kind::Assistant(a))
                if a.turn == 2 && matches!(&a.blocks[0].block, Some(content_block::Block::ToolUse(t)) if t.id == "toolu_after" && t.name == "Bash")));
            assert!(matches!(&reply[4].kind, Some(session_event::Kind::ToolResult(r)) if r.call_id == "toolu_after"));
            p.close().await;
        }
    }

    /// A stand-in `claude` that, as 2.1.311 does, starts a turn by itself
    /// right after a turn you stopped, for its agent's notification queued
    /// during it, whose text it writes only to its transcript
    /// (`projects/<cwd>/s.jsonl` in its folder; not with `no-transcript`
    /// there). It records what it reads (`stdin.txt`). Stopped too, it says
    /// "[Request interrupted by user]" and waits; then its build ends on its
    /// own and it wakes for that.
    fn stopping_claude(dir: &Path) -> PathBuf {
        script(
            dir,
            "claude-stopping",
            r#"#!/bin/sh
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
echo_of() { printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$1")"; }
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}\n' "$1"; }
init() { printf '%s\n' '{"type":"system","subtype":"init"}'; }
done_ok() { printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{}}'; }
stopped() { printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"usage":{}}'; }
said() { printf '{"type":"user","uuid":"%s","message":{"role":"user","content":[{"type":"text","text":"%s"}]},"parent_tool_use_id":null}\n' "$1" "$2"; }
transcript() { [ -e no-transcript ] && return; t="projects/$(pwd | sed 's/[^A-Za-z0-9]/-/g')"; mkdir -p "$t"; printf '%s\n' "$1" >> "$t/s.jsonl"; }
stops=0
while read -r line; do
  printf '%s\n' "$line" >> stdin.txt
  case "$line" in
    *'"interrupt"'*)
      stops=$((stops+1))
      if [ "$stops" = 1 ]; then
        printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"fg1","status":"stopped","summary":"Wait"}'
        said u-stop "[Request interrupted by user for tool use]"
        stopped
        transcript '{"type":"user","uuid":"u-stop","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}'
        transcript '{"type":"queue-operation","operation":"dequeue"}'
        transcript '{"type":"user","uuid":"n-ag1","isSidechain":false,"origin":{"kind":"task-notification","producer":"session-task"},"turnOrigin":"task_notification","message":{"role":"user","content":"<task-notification>\n<task-id>ag1</task-id>\n<status>completed</status>\n<summary>Agent \"Review\" is waiting — on its own work</summary>\n</task-notification>"}}'
        transcript '{"type":"attachment","uuid":"a-1","isSidechain":false,"attachment":{"type":"deferred_tools_delta"}}'
        init
      else
        said u-stop2 "[Request interrupted by user]"
        transcript '{"type":"user","uuid":"u-stop2","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}'
        stopped
        sleep 0.3
        printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"bg1","status":"completed","summary":"Build finished"}'
        init
        say "The build passed."
        done_ok
      fi
      continue;;
    *'"control_request"'*) continue;;
  esac
  init
  echo_of "$line"
  case "$line" in
    *hold*)
      printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"bg1","description":"Build","task_type":"local_bash","is_backgrounded":true}'
      printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"ag1","description":"Review","task_type":"local_agent","is_backgrounded":true}'
      printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"ag1","status":"completed","summary":"Reviewed"}'
      printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"fg1","description":"Wait","task_type":"local_bash","is_backgrounded":false}'
      say "Waiting.";;
    *)
      say "Next."
      done_ok;;
  esac
done
"#,
        )
    }

    /// What `stopping_claude` put into its conversation for its agent.
    const QUEUED: &str = "<task-notification>\n<task-id>ag1</task-id>\n<status>completed</status>\n<summary>Agent \"Review\" is waiting — on its own work</summary>\n</task-notification>";

    /// Stops `stopping_claude`'s first turn, then runs the turn it starts by
    /// itself at once; the events logged from there.
    async fn stop_then_flush(dir: &Path) -> (Process, Arc<VecSink>, Vec<SessionEvent>) {
        let bin = stopping_claude(dir);
        let tools = Arc::new(c6_tools::SessionTools::new(dir.to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir, "s", false, Some(observer(dir, tools).await));
        p.transcripts = Some(dir.join("projects"));
        let cancel = CancellationToken::new();
        let more = tokio::sync::Notify::new();
        let turn = p.turn(1, vec![input("c1", "hold on")], false, None, "opus", &sink, &Joining::default, &more, &cancel);
        let stop = async {
            replied(&vec_sink).await;
            cancel.cancel();
        };
        let (end, ()) = tokio::join!(turn, stop);
        assert_eq!(end.unwrap().reason, Some(TurnEndReason::Interrupted));
        let before = vec_sink.0.lock().unwrap().len();
        assert!(idle_until_or_turn(&mut p, &sink, |_| false).await);
        assert_eq!(run(&mut p, 2, &[], &sink).await.reason, Some(TurnEndReason::Interrupted));
        assert!(p.take_unshown() && !p.take_unshown(), "said once");
        let flushed = vec_sink.0.lock().unwrap()[before..].to_vec();
        (p, vec_sink, flushed)
    }

    /// The notices a stopped turn logged, and its quiet line.
    fn told_then_stopped(events: &[SessionEvent]) -> Vec<String> {
        let told: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::User(u)) => {
                    assert_eq!(u.command, "task-notification");
                    Some(u.text.clone())
                }
                _ => None,
            })
            .collect();
        let notices: Vec<&c6_proto::Notice> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::Notice(n)) => Some(n),
                _ => None,
            })
            .collect();
        assert!(notices.len() == 1 && notices[0].quiet && notices[0].title == "Stopped before replying", "{notices:?}");
        assert!(matches!(events.last().and_then(|e| e.kind.as_ref()), Some(session_event::Kind::Notice(_))), "the line comes last");
        assert!(
            !events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::TurnStarted(_) | session_event::Kind::TurnEnded(_)))),
            "no turn"
        );
        told
    }

    /// The turn Claude Code starts by itself right after you stop one is
    /// stopped as it begins: no turn shows, only the notice it put into the
    /// conversation, word for word as its transcript has it, and a quiet line
    /// saying the agent reads it with your next message. A task that then
    /// ends on its own still wakes the agent.
    #[tokio::test]
    async fn a_turn_claude_code_starts_right_after_your_stop_is_stopped_too() {
        let dir = tempfile::tempdir().unwrap();
        let (mut p, vec_sink, flushed) = stop_then_flush(dir.path()).await;
        assert_eq!(told_then_stopped(&flushed), [QUEUED], "the notice as it reached the model");
        let interrupts = std::fs::read_to_string(dir.path().join("stdin.txt"))
            .unwrap()
            .lines()
            .filter(|l| l.contains(r#""subtype":"interrupt""#))
            .count();
        assert_eq!(interrupts, 2, "yours, then the one for its own turn");
        // Then its build ends on its own: it wakes for that, as before.
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let before = vec_sink.0.lock().unwrap().len();
        assert!(idle_until_or_turn(&mut p, &sink, |_| false).await);
        assert_eq!(run(&mut p, 2, &[], &sink).await.reason, Some(TurnEndReason::Completed));
        assert!(!p.take_unshown());
        let events = vec_sink.0.lock().unwrap()[before..].to_vec();
        let shown: Vec<&str> = events.iter().map(kind).filter(|k| *k != "other").collect();
        assert_eq!(shown, ["user", "turn_started", "assistant", "usage", "turn_ended"]);
        let Some(session_event::Kind::User(u)) = &events.iter().find(|e| kind(e) == "user").unwrap().kind else { panic!() };
        assert_eq!(u.text, "Build finished");
        p.close().await;
    }

    /// When its transcript doesn't have the notice, nothing is made up: the
    /// log says a notice couldn't be read, and names the tasks it reported.
    #[tokio::test]
    async fn a_notice_that_cannot_be_read_is_said_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("no-transcript"), "").unwrap();
        let (p, _, flushed) = stop_then_flush(dir.path()).await;
        assert_eq!(
            told_then_stopped(&flushed),
            [
                "Claude Code queued a notice we couldn't read. In the turn you stopped it reported these tasks ended: ag1 (completed), fg1 (stopped)."
            ]
        );
        p.close().await;
    }

    /// The notices come from the transcript after the stopped turn's last
    /// message, up to what follows them; failing that, by when they were
    /// written. Subagents' messages are not the conversation's.
    #[test]
    fn queued_notices_are_read_after_the_stopped_turn() {
        let notice = |uuid: &str, text: &str, at: &str| {
            serde_json::json!({"type": "user", "uuid": uuid, "timestamp": at, "isSidechain": false,
                "origin": {"kind": "task-notification"}, "message": {"role": "user", "content": text}})
            .to_string()
        };
        let transcript = [
            notice("old", "an older notice", "2026-11-02T22:00:00.000Z"),
            serde_json::json!({"type": "user", "uuid": "stop", "message": {"content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}}).to_string(),
            serde_json::json!({"type": "queue-operation", "operation": "dequeue"}).to_string(),
            notice("n1", "first", "2026-11-02T22:52:16.815Z"),
            serde_json::json!({"type": "user", "uuid": "side", "isSidechain": true, "origin": {"kind": "task-notification"}, "message": {"content": "a subagent's"}}).to_string(),
            notice("n2", "second", "2026-11-02T22:52:16.816Z"),
            serde_json::json!({"type": "user", "uuid": "stop2", "message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]}}).to_string(),
            notice("later", "a later one", "2026-11-02T22:53:00.000Z"),
        ]
        .join("\n");
        let at = |t: &str| std::time::SystemTime::from(chrono::DateTime::parse_from_rfc3339(t).unwrap());
        let long_ago = at("2026-11-01T00:00:00Z");
        assert_eq!(queued_in(&transcript, Some("stop"), long_ago).unwrap(), ["first", "second"]);
        assert_eq!(queued_in(&transcript, Some("gone"), at("2026-11-02T22:52:16.900Z")).unwrap(), ["first", "second"], "by time");
        assert_eq!(queued_in(&transcript, Some("stop2"), long_ago).unwrap(), ["a later one"]);
        assert!(queued_in(&transcript, Some("later"), long_ago).is_none(), "nothing after it");
        assert!(queued_in("", None, long_ago).is_none());
        let folder = Path::new("/home/me/my project");
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("-home-me-my-project").join("c1.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();
        assert_eq!(transcript_of(root.path(), folder, "c1"), Some(file.clone()));
        assert_eq!(transcript_of(root.path(), Path::new("/elsewhere"), "c1"), Some(file), "found by its name");
        assert_eq!(transcript_of(root.path(), folder, "c2"), None);
    }

    /// A worker that starts after a crash stops the Claude Code processes the
    /// last one recorded, but not a process that merely has the same id now.
    #[tokio::test]
    async fn orphans_are_stopped_but_not_strangers() {
        let dir = tempfile::tempdir().unwrap();
        let records = dir.path().join("claude");
        let bin = script(dir.path(), "claude-orphan", "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nwhile true; do sleep 0.1; done\n");
        c6_testkit::runnable(&bin, &["--version"]).await;
        let orphan = std::process::Command::new(&bin).args(["--resume", "conv-1"]).process_group(0).spawn().unwrap();
        let mut stranger = std::process::Command::new("sleep").arg("300").process_group(0).spawn().unwrap();
        std::fs::create_dir_all(&records).unwrap();
        std::fs::write(records.join("conv-1.pid"), format!("{}\nconv-1\n", orphan.id())).unwrap();
        std::fs::write(records.join("conv-2.pid"), format!("{}\nconv-2\n", stranger.id())).unwrap();
        let mut orphan = orphan;
        sweep_orphans(&records).await;
        assert!(orphan.wait().unwrap().signal().is_some(), "the orphan is stopped");
        assert!(stranger.try_wait().unwrap().is_none(), "another program with a recorded id is left alone");
        assert_eq!(std::fs::read_dir(&records).unwrap().count(), 0, "the records are gone");
        stranger.kill().unwrap();
        let _ = stranger.wait();
    }

    /// A held process has its folder while it runs, outlives the
    /// `Process` that started it when that is merely dropped (a worker
    /// going), is taken up again, and closing it ends it with its folder.
    #[test]
    fn held_wrapper_identity_requires_the_exact_program_and_conversation_option() {
        let wrapper = ["/bin/sh", "-c", r#""$@"; echo $? > "$C6_HELD_CODE""#, "sh", "/tmp/claude-live", "--resume", "fork"];
        assert!(claude_args_for(&wrapper, "fork"));
        assert!(!claude_args_for(&wrapper, "for"));
        assert!(claude_args_for(&["node", "/opt/claude-code/cli.js", "--session-id", "fork"], "fork"));
        assert!(!claude_args_for(&["sleep", "/tmp/claude-notes", "--resume", "fork"], "fork"));
        assert!(!claude_args_for(&["sh", "-c", "echo claude --resume fork"], "fork"));
    }

    #[tokio::test]
    async fn rewind_cleanup_stops_owned_descendants_after_the_wrapper_exits() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held");
        let _cleanup = HeldFixtureCleanup(held.clone());
        let journal = crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let bin = live_claude(dir.path());
        let mut old = Process::spawn_held(
            &held,
            "s",
            &bin,
            Launch { conversation: "old".into(), ..Default::default() },
            false,
            dir.path(),
            "opus",
            Arc::default(),
            None,
            None,
        )
        .unwrap();
        let other = Process::spawn_held(
            &held,
            "other",
            &bin,
            Launch { conversation: "other".into(), ..Default::default() },
            false,
            dir.path(),
            "opus",
            Arc::default(),
            None,
            None,
        )
        .unwrap();
        let pg = old.pgid.unwrap();
        let keeper = old.held.as_ref().unwrap().keeper;
        c6_testkit::eventually("fixture keeper owns its FIFO", async || keeper_holds_fifo(keeper, pg, &held.join("old")).then_some(()))
            .await;
        // Kill/reap only the wrapper created by this fixture, not its group.
        let mut wrapper = old.child.take().unwrap();
        wrapper.kill().await.unwrap();
        drop(old);
        assert!(killpg(pg, None).is_ok(), "the keeper/CLI still own the group");
        stop_held_for_rewind(&held, "s", &journal).await.unwrap();
        assert_eq!(killpg(pg, None), Err(nix::errno::Errno::ESRCH));
        assert!(!held.join("old").exists());
        assert!(journal.claude_processes("s").await.unwrap().is_empty());
        assert!(killpg(other.pgid.unwrap(), None).is_ok(), "another session is untouched");
        other.close().await;
    }

    #[tokio::test]
    async fn held_replacement_requires_group_absence_and_preserves_live_records() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held");
        let _cleanup = HeldFixtureCleanup(held.clone());
        let bin = live_claude(dir.path());
        let launch = || Launch { conversation: "fork".into(), ..Default::default() };
        let p = Process::spawn_held(&held, "s", &bin, launch(), false, dir.path(), "opus", Arc::default(), None, None).unwrap();
        let pg = p.pgid.unwrap();
        let path = held.join("fork/meta.json");
        let before = std::fs::read(&path).unwrap();
        assert!(Process::spawn_held(&held, "s", &bin, launch(), true, dir.path(), "opus", Arc::default(), None, None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before, "a live runtime's authority must not be overwritten");
        assert!(killpg(pg, None).is_ok());
        p.close_for_replacement().await.unwrap();
        assert_eq!(killpg(pg, None), Err(nix::errno::Errno::ESRCH));
        let unknown = held.join("fork");
        std::fs::create_dir(&unknown).unwrap();
        assert!(Process::spawn_held(&held, "s", &bin, launch(), true, dir.path(), "opus", Arc::default(), None, None).is_err());
        assert!(unknown.exists(), "an unconfirmed/corrupt record must not be discarded either");
        std::fs::remove_dir(unknown).unwrap();
        let replacement = Process::spawn_held(&held, "s", &bin, launch(), true, dir.path(), "opus", Arc::default(), None, None).unwrap();
        replacement.close_for_replacement().await.unwrap();
    }

    #[tokio::test]
    async fn rewind_stop_without_a_ready_keeper_preserves_descendant_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held");
        let _cleanup = HeldFixtureCleanup(held.clone());
        let bin = live_claude(dir.path());
        let p = Process::spawn_held(
            &held,
            "s",
            &bin,
            Launch { conversation: "fork".into(), ..Default::default() },
            false,
            dir.path(),
            "opus",
            Arc::default(),
            None,
            None,
        )
        .unwrap();
        let pg = p.pgid.unwrap();
        let keeper = p.held.as_ref().unwrap().keeper;
        c6_testkit::eventually("owned fixture keeper readiness", async || keeper_holds_fifo(keeper, pg, &held.join("fork")).then_some(()))
            .await;
        let mut member = tokio::process::Command::new("sh")
            .args(["-c", "trap '' INT; echo ready > member-ready; exec sleep 300"])
            .current_dir(dir.path())
            .process_group(pg.as_raw())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        c6_testkit::eventually("fixture descendant ignores SIGINT", async || dir.path().join("member-ready").exists().then_some(())).await;
        // Only this fixture's verified keeper is stopped, never an arbitrary
        // persisted PID. The live wrapper still proves group ownership.
        nix::sys::signal::kill(keeper, Signal::SIGKILL).unwrap();
        c6_testkit::eventually("fixture keeper to be reaped", async || nix::unistd::getpgid(Some(keeper)).is_err().then_some(())).await;
        p.stop_for_rewind().await.unwrap();
        if killpg(pg, None).is_ok() {
            assert!(held.join("fork/meta.json").exists(), "live descendants/zombies retain cleanup metadata");
        }
        let status = c6_testkit::within("the SIGINT-ignoring fixture child to be killed", member.wait()).await.unwrap();
        assert!(!status.success());
        let journal = crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap();
        stop_held_for_rewind(&held, "s", &journal).await.unwrap();
        assert_eq!(killpg(pg, None), Err(nix::errno::Errno::ESRCH));
    }

    #[tokio::test]
    async fn rewind_cleanup_does_not_signal_a_reused_held_process_id() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held");
        let _cleanup = HeldFixtureCleanup(held.clone());
        let journal = crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let bin = live_claude(dir.path());
        let p = Process::spawn_held(
            &held,
            "s",
            &bin,
            Launch { conversation: "fork".into(), ..Default::default() },
            false,
            dir.path(),
            "opus",
            Arc::default(),
            None,
            None,
        )
        .unwrap();
        let path = held.join("fork").join("meta.json");
        let mut meta: HeldMeta = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        p.close().await;
        let mut unrelated = tokio::process::Command::new("sleep").arg("300").process_group(0).kill_on_drop(true).spawn().unwrap();
        let pg = unrelated.id().unwrap() as i32;
        meta.pgid = pg;
        meta.keeper = pg;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(Process::attach(&held, "fork", dir.path(), "opus", Arc::default(), None, None).await.is_none());
        assert!(stop_held_for_rewind(&held, "s", &journal).await.is_err());
        assert!(unrelated.try_wait().unwrap().is_none(), "saved IDs are not signal authority");
        assert!(path.exists());
        assert_eq!(journal.claude_processes("s").await.unwrap(), vec![pg]);
        // This child handle, unlike the forged metadata, is fixture ownership.
        unrelated.kill().await.unwrap();
        stop_held_for_rewind(&held, "s", &journal).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn rewind_transfers_only_unconfirmed_unrecorded_inputs_once() {
        let dir = tempfile::tempdir().unwrap();
        let bin = live_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "before", false, None);
        let confirmed = p.send(input("confirmed", "first"), None, false).await.unwrap();
        p.send(input("waiting", "second"), None, false).await.unwrap();
        p.send(input("recorded", "already in journal"), None, true).await.unwrap();
        let mut started = false;
        p.confirm(&confirmed, &mut started, &sink).await.unwrap();
        p.confirm(&confirmed, &mut started, &sink).await.unwrap();
        // Transfer before fallible shutdown, as reset_claude does.
        let waiting = p.take_unsent();
        assert_eq!(waiting.iter().map(|i| i.command_id.as_str()).collect::<Vec<_>>(), ["waiting"]);
        assert!(p.take_unsent().is_empty());
        p.stop_for_rewind().await.unwrap();
        let mut replacement = spawn(&bin, dir.path(), "after", false, None);
        assert_eq!(run(&mut replacement, 2, &waiting, &sink).await.reason, Some(TurnEndReason::Completed));
        {
            let events = vec_sink.0.lock().unwrap();
            let users = events
                .iter()
                .filter_map(|e| match &e.kind {
                    Some(session_event::Kind::User(u)) => Some(u.command_id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(users, ["confirmed", "waiting"]);
        }
        replacement.close().await;
    }

    #[tokio::test]
    async fn a_held_process_outlives_its_worker_and_is_taken_up() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held");
        let _cleanup = HeldFixtureCleanup(held.clone());
        let bin = live_claude(dir.path());
        let launch = Launch { conversation: "conv-9".into(), ..Default::default() };
        let mut p = Process::spawn_held(&held, "s-9", &bin, launch, false, dir.path(), "opus", Arc::default(), None, None).unwrap();
        let meta: Value = serde_json::from_slice(&std::fs::read(held.join("conv-9").join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["session"], "s-9");
        let pg = Pid::from_raw(meta["pgid"].as_i64().unwrap() as i32);
        let keeper = p.held.as_ref().unwrap().keeper;
        c6_testkit::eventually("the held fixture keeper to open its FIFO", async || {
            keeper_holds_fifo(keeper, pg, &held.join("conv-9")).then_some(())
        })
        .await;
        // The authoritative stream ID need not equal the launch/storage ID.
        // Keep the wrapper's paths stable, but persist the identity before
        // advancing its output offset so the next worker can find it.
        p.consumed(42, &serde_json::json!({"type":"system", "session_id":"actual-9"}));
        let (record, _) = Process::held_record(&held, "actual-9").unwrap().unwrap();
        assert_eq!(record, held.join("conv-9"));
        assert_eq!(std::fs::read_to_string(record.join("offset")).unwrap(), "42");
        drop(p);
        assert!(nix::sys::signal::kill(pg, None).is_ok(), "dropped, it goes on");
        assert_eq!(Process::held_sessions(&held), vec!["s-9".to_owned()]);
        let p = Process::attach(&held, "actual-9", dir.path(), "opus", Arc::default(), None, None).await.expect("taken up");
        p.close().await;
        c6_testkit::eventually("it ends", async || nix::sys::signal::kill(pg, None).is_err().then_some(())).await;
        assert!(!held.join("conv-9").exists(), "its folder goes with it");
        assert!(Process::attach(&held, "actual-9", dir.path(), "opus", Arc::default(), None, None).await.is_none());
    }

    /// What Claude Code says under a call of one of carbon6's MCP tools (its
    /// progress) is not one of its own subagents: no mirror session.
    #[tokio::test]
    async fn mcp_tool_calls_are_not_mirrored_as_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut mapper = Mapper {
            turn: 1,
            scope: "s".into(),
            output_root: None,
            saw_output: false,
            result: None,
            interrupted: false,
            observer: Some(observer(dir.path(), tools).await),
            calls: HashMap::new(),
            mcp_calls: HashSet::new(),
            children: HashMap::new(),
            backgrounded: HashSet::new(),
            task_calls: HashMap::new(),
            call_tasks: HashMap::new(),
            call_usage: HashMap::new(),
            per_call: false,
        };
        for line in [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_m","name":"mcp__carbon6__Task","input":{"description":"Publish demo outputs","prompt":"p"}}]},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"progress"}]},"parent_tool_use_id":"toolu_m"}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_m","content":"done"}]},"parent_tool_use_id":null}"#,
        ] {
            mapper.handle(&serde_json::from_str(line).unwrap(), &sink).await.unwrap();
        }
        assert!(mapper.children.is_empty(), "no mirror for carbon6's own subagent");
        assert_eq!(kinds(&vec_sink), ["assistant", "tool_result"]);
    }

    /// A stand-in `claude` for Send now. It records every line it reads
    /// (`stdin.txt`). Its first message starts a turn that waits on a
    /// foreground command; a message written then waits for a Send now,
    /// which it answers as the first message asks: `delivering` (the command
    /// goes to the background and the message joins the turn), `stopped`
    /// (only the turn stops; the message starts the next), or as an older
    /// Claude Code does (no `send_now` in its answer). `quick` just answers.
    fn send_now_claude(dir: &Path) -> PathBuf {
        script(
            dir,
            "claude-send-now",
            r#"#!/bin/sh
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
req_of() { printf '%s' "$1" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p'; }
echo_of() { printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$1"; }
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}\n' "$1"; }
answer() { printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":%s}}\n' "$1" "$2"; }
init() { printf '%s\n' '{"type":"system","subtype":"init"}'; }
done_ok() { printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{}}'; }
stopped() { printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"usage":{}}'; }
mode=""
waiting=""
while read -r line; do
  printf '%s\n' "$line" >> stdin.txt
  case "$line" in
    *'"interrupt"'*)
      req=$(req_of "$line")
      case "$mode" in
        delivering)
          answer "$req" '{"still_queued":[],"send_now":"delivering"}'
          printf '%s\n' '{"type":"system","subtype":"task_updated","task_id":"fg1","patch":{"is_backgrounded":true}}'
          printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_fg","content":"Moved to the background."}]},"parent_tool_use_id":null}'
          echo_of "$waiting"
          say "Got it."
          done_ok;;
        stopped)
          answer "$req" '{"still_queued":["'"$waiting"'"],"send_now":"stopped"}'
          printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"fg1","status":"stopped"}'
          stopped
          init
          echo_of "$waiting"
          say "PING"
          done_ok;;
        *)
          answer "$req" '{"still_queued":["'"$waiting"'"]}'
          stopped
          init
          echo_of "$waiting"
          say "PING"
          done_ok;;
      esac
      continue;;
    *'"control_request"'*) continue;;
  esac
  if [ -n "$mode" ] && [ "$mode" != quick ]; then
    waiting=$(uuid_of "$line")
    continue
  fi
  case "$line" in *delivering*) mode=delivering;; *stopped*) mode=stopped;; *quick*) mode=quick;; *) mode=old;; esac
  init
  echo_of "$(uuid_of "$line")"
  if [ "$mode" = quick ]; then
    say "Done."
    done_ok
    continue
  fi
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_fg","name":"Bash","input":{"command":"make"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
  printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"fg1","tool_use_id":"toolu_fg","description":"Long build","task_type":"local_bash","is_backgrounded":false}'
done
"#,
        )
    }

    /// The lines the stand-in read, as JSON.
    fn read_lines(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("stdin.txt")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// Runs a turn that waits on a foreground command; once it does, your
    /// message is sent with Send now.
    async fn send_now_turn(p: &mut Process, first: &str, sink: &Arc<dyn EventSink>, vec_sink: &VecSink, next: PendingInput) -> TurnEnd {
        let queue = Mutex::new(Vec::new());
        let more = tokio::sync::Notify::new();
        let feed = || Joining { inputs: std::mem::take(&mut *queue.lock().unwrap()), now: Vec::new() };
        let cancel = CancellationToken::new();
        let turn = p.turn(1, vec![input("c1", first)], false, None, "opus", sink, &feed, &more, &cancel);
        let send = async {
            replied(vec_sink).await;
            queue.lock().unwrap().push(PendingInput { send_now: true, ..next });
            more.notify_one();
        };
        let (end, ()) = tokio::join!(turn, send);
        end.unwrap()
    }

    /// Send now writes your message (as a person's), then asks Claude Code
    /// to deliver that message now; "delivering": the foreground command
    /// goes to the background, the turn goes on with the message, and it
    /// isn't stopped.
    #[tokio::test]
    async fn send_now_delivers_into_the_running_turn() {
        let dir = tempfile::tempdir().unwrap();
        let bin = send_now_claude(dir.path());
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tools.background.set_sink(Some(tx));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, Some(observer(dir.path(), tools.clone()).await));
        let end = send_now_turn(&mut p, "delivering", &sink, &vec_sink, input("c2", "PING now")).await;
        assert_eq!(end, TurnEnd { reason: Some(TurnEndReason::Completed), alive: true, no_conversation: false });
        assert_eq!(
            kinds(&vec_sink),
            ["user", "turn_started", "assistant", "input_delivered", "tool_result", "user", "assistant", "usage", "turn_ended"],
            "the message joins the turn, which isn't stopped"
        );
        assert!(!vec_sink.0.lock().unwrap().iter().any(|e| matches!(e.kind, Some(session_event::Kind::Error(_)))));
        let lines = read_lines(dir.path());
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["origin"], serde_json::json!({"kind": "human"}), "your messages are a person's");
        assert_eq!((lines[1]["type"].as_str(), &lines[1]["origin"]), (Some("user"), &serde_json::json!({"kind": "human"})));
        let request = &lines[2]["request"];
        assert_eq!(lines[2]["type"], "control_request");
        assert_eq!((request["subtype"].as_str(), request["send_now"].as_bool()), (Some("interrupt"), Some(true)));
        assert_eq!(request["message_uuid"], lines[1]["uuid"], "it names the message just written");
        assert!(request.get("cancel_queued").is_none());
        // The command it moved is a background task now.
        let started = rx.try_recv().unwrap();
        assert_eq!((started.native_type.as_str(), started.foreground), ("local_bash", true));
        let moved = rx.try_recv().unwrap();
        assert_eq!((moved.id.as_str(), moved.state, moved.foreground), (started.id.as_str(), c6_tools::TaskState::Started, false));
        assert!(p.tasks_running(), "it runs on in the background");
        p.close().await;
    }

    #[tokio::test]
    async fn automation_sent_mid_turn_never_acquires_human_origin() {
        for api_origin in [Some(c6_proto::ApiMessageOrigin { token_name: "build-bot".into() }), Some(Default::default()), None] {
            let dir = tempfile::tempdir().unwrap();
            let bin = send_now_claude(dir.path());
            let vec_sink = Arc::new(VecSink::default());
            let sink: Arc<dyn EventSink> = vec_sink.clone();
            let mut p = spawn(&bin, dir.path(), "s", false, None);
            let next = PendingInput { api_origin: api_origin.clone(), human_origin: false, ..input("c2", "PING now") };
            let end = send_now_turn(&mut p, "delivering", &sink, &vec_sink, next).await;
            assert_eq!(end.reason, Some(TurnEndReason::Completed));
            let lines = read_lines(dir.path());
            assert_eq!(lines[0]["origin"], serde_json::json!({"kind": "human"}));
            assert!(lines[1].get("origin").is_none(), "automation must not be sent as a person: {:?}", lines[1]);
            let text = lines[1]["message"]["content"].as_str().expect("labelled text");
            let label = match &api_origin {
                Some(origin) if !origin.token_name.is_empty() => "Message sent via the API (token \"build-bot\"), not typed by the user:",
                Some(_) => "Message sent via the API, not typed by the user:",
                None => "Message sent by an automation, not typed by the user:",
            };
            assert_eq!(text, format!("{label}\nPING now"));
            assert_eq!(lines[2]["request"]["message_uuid"], lines[1]["uuid"], "Send now still targets the automation input");
            let events = vec_sink.0.lock().unwrap().clone();
            let message = events
                .iter()
                .find_map(|e| match &e.kind {
                    Some(session_event::Kind::User(u)) if u.command_id == "c2" => Some(u),
                    _ => None,
                })
                .expect("the delivered input is persisted");
            assert_eq!(message.api_origin, api_origin);
            assert!(!message.human_origin);
            assert!(message.origin_recorded);
            assert_eq!(message.text, "PING now", "the stored message is not rewritten with a model-only label");
            p.close().await;
        }
    }

    /// "stopped": Claude Code stopped only the turn, which ends interrupted
    /// (not failed); the message starts the next turn.
    #[tokio::test]
    async fn send_now_that_stops_the_turn_ends_it_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let bin = send_now_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, None);
        let end = send_now_turn(&mut p, "stopped", &sink, &vec_sink, input("c2", "PING now")).await;
        assert_eq!(end, TurnEnd { reason: Some(TurnEndReason::Interrupted), alive: true, no_conversation: false });
        assert_eq!(kinds(&vec_sink), ["user", "turn_started", "assistant", "input_delivered", "usage", "turn_ended"], "not an error");
        // The next turn is the message's.
        assert!(idle_until_or_turn(&mut p, &sink, |_| false).await);
        assert_eq!(run(&mut p, 2, &[], &sink).await.reason, Some(TurnEndReason::Completed));
        let events = vec_sink.0.lock().unwrap().clone();
        let Some(session_event::Kind::User(u)) = &events[6].kind else { panic!("{:?}", events[6]) };
        assert_eq!((u.command_id.as_str(), u.text.as_str()), ("c2", "PING now"));
        p.close().await;
    }

    /// An older Claude Code answers without saying how it took Send now (it
    /// may have stopped the turn and its background agents): the turn ends
    /// interrupted, and the session says so, once.
    #[tokio::test]
    async fn an_older_claude_code_taking_send_now_as_stop_is_noted() {
        let dir = tempfile::tempdir().unwrap();
        let bin = send_now_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, None);
        let end = send_now_turn(&mut p, "older", &sink, &vec_sink, input("c2", "PING now")).await;
        assert_eq!(end.reason, Some(TurnEndReason::Interrupted));
        let notices: Vec<c6_proto::Notice> = vec_sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::Notice(n)) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].message.contains("background agents") && notices[0].quiet, "{:?}", notices[0]);
        assert!(!vec_sink.0.lock().unwrap().iter().any(|e| matches!(e.kind, Some(session_event::Kind::Error(_)))));
        p.close().await;
    }

    /// carbon6's notices aren't a person's messages: they carry no origin.
    #[tokio::test]
    async fn notices_are_not_marked_as_yours() {
        let dir = tempfile::tempdir().unwrap();
        let bin = send_now_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, None);
        let notices = [
            PendingInput { command: "task-notification".into(), ..input("task:bg1:finished", "quick: the build ended") },
            PendingInput {
                command: "session-message".into(),
                from_session_id: "0199-other".into(),
                ..input("m1", "quick: from another session")
            },
        ];
        for (turn, notice) in notices.into_iter().enumerate() {
            assert_eq!(run(&mut p, turn as u64 + 1, &[notice], &sink).await.reason, Some(TurnEndReason::Completed));
        }
        assert_eq!(run(&mut p, 3, &[input("c3", "quick: yours")], &sink).await.reason, Some(TurnEndReason::Completed));
        let lines = read_lines(dir.path());
        assert_eq!(lines.iter().map(|l| l.get("origin").is_some()).collect::<Vec<_>>(), [false, false, true]);
        assert!(lines.iter().all(|l| l["uuid"].is_string()), "each with its uuid");
        p.close().await;
    }

    #[tokio::test]
    async fn initial_automation_inputs_keep_their_label_and_never_get_human_origin() {
        let dir = tempfile::tempdir().unwrap();
        let bin = send_now_claude(dir.path());
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut p = spawn(&bin, dir.path(), "s", false, None);
        for (turn, api_origin, human_origin, label) in [
            (
                1,
                Some(c6_proto::ApiMessageOrigin { token_name: "ops-controller".into() }),
                false,
                "Message sent via the API (token \"ops-controller\"), not typed by the user:",
            ),
            (2, Some(Default::default()), false, "Message sent via the API, not typed by the user:"),
            (3, None, false, "Message sent by an automation, not typed by the user:"),
            (
                4,
                Some(c6_proto::ApiMessageOrigin { token_name: "ops-controller".into() }),
                true,
                "Message sent via the API (token \"ops-controller\"), not typed by the user:",
            ),
        ] {
            let next = PendingInput { api_origin, human_origin, ..input(&format!("c{turn}"), "quick: Continue.") };
            assert_eq!(run(&mut p, turn, &[next], &sink).await.reason, Some(TurnEndReason::Completed));
            let line = read_lines(dir.path()).pop().unwrap();
            assert!(line.get("origin").is_none(), "automation must not acquire a human marker: {line}");
            assert_eq!(line["message"]["content"], format!("{label}\nquick: Continue."));
        }
        p.close().await;
    }

    /// A foreground subagent or command Claude Code moves to the background
    /// (`task_updated` with `is_backgrounded`) is a background task from
    /// then on: the subagent's call returning doesn't end it, its task does.
    #[tokio::test]
    async fn a_task_moved_to_the_background_is_a_background_task() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Arc::new(c6_tools::SessionTools::new(dir.path().to_owned()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tools.background.set_sink(Some(tx));
        let observer = observer(dir.path(), tools.clone()).await;
        let journal = observer.journal.clone();
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let mut mapper = Mapper {
            turn: 1,
            scope: "s".into(),
            output_root: None,
            saw_output: false,
            result: None,
            interrupted: false,
            observer: Some(observer),
            calls: HashMap::new(),
            mcp_calls: HashSet::new(),
            children: HashMap::new(),
            backgrounded: HashSet::new(),
            task_calls: HashMap::new(),
            call_tasks: HashMap::new(),
            call_usage: HashMap::new(),
            per_call: false,
        };
        for line in [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_a","name":"Agent","input":{"description":"review","prompt":"Review it."}},{"type":"tool_use","id":"toolu_b","name":"Bash","input":{"command":"make"}}]},"parent_tool_use_id":null}"#,
            r#"{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"toolu_a","description":"review","task_type":"local_agent","is_backgrounded":false}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reviewing."}]},"parent_tool_use_id":"toolu_a"}"#,
            r#"{"type":"system","subtype":"task_started","task_id":"b1","tool_use_id":"toolu_b","description":"make","task_type":"local_bash","is_backgrounded":false}"#,
            r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"is_backgrounded":true}}"#,
            r#"{"type":"system","subtype":"task_updated","task_id":"b1","patch":{"is_backgrounded":true}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"make continues in the background"}]},"parent_tool_use_id":"toolu_b"}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"Moved to the background."},{"type":"tool_result","tool_use_id":"toolu_b","content":"Moved to the background."}]},"parent_tool_use_id":null}"#,
        ] {
            mapper.handle(&serde_json::from_str(line).unwrap(), &sink).await.unwrap();
        }
        let events: Vec<c6_tools::TaskEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            events.iter().map(|e| (e.native_type.as_str(), e.foreground, e.state)).collect::<Vec<_>>(),
            [
                ("local_agent", true, c6_tools::TaskState::Started),
                ("local_bash", true, c6_tools::TaskState::Started),
                ("local_agent", false, c6_tools::TaskState::Started),
                ("local_bash", false, c6_tools::TaskState::Started),
            ],
            "each is reported again once it is in the background"
        );
        assert_eq!((&events[2].id, &events[3].id), (&events[0].id, &events[1].id), "the same tasks");
        assert_eq!(mapper.children.len(), 1, "only the local_agent task has a child session");
        let spawned: Vec<_> = vec_sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::SubagentSpawned(s)) => Some(s.call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(spawned, ["toolu_a"]);
        let child = mapper.children.get("toolu_a").expect("mirrored");
        assert!(child.open, "the subagent goes on after its call returns");
        let child_id = child.id.clone();
        mapper
            .handle(
                &serde_json::json!({"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"toolu_a","status":"completed","summary":"Reviewed."}),
                &sink,
            )
            .await
            .unwrap();
        let (_, log) = journal.load_session(&child_id).await.unwrap().unwrap();
        let ends: Vec<i32> = log
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::TurnEnded(t)) => Some(t.reason),
                _ => None,
            })
            .collect();
        assert_eq!(ends, [TurnEndReason::Completed as i32], "it ends with its task");
    }
}
