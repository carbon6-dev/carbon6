//! Background tasks: shell commands started by `bash` with
//! `run_in_background`, and subagents started by Task with
//! `run_in_background`. Read with `bash_output`, stopped with `kill_shell`.
//! Claude Code's CLI-owned tasks are also observed here for read-only UI
//! inspection; they keep their own notification and cancellation lifecycle.
//!
//! The runtime can register a sink to hear when tasks start and end; the
//! worker logs them and, when a task asked to be noticed, tells the agent.

use std::collections::HashMap;
use std::io::Read as _;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use c6_llm::ToolSpec;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// Output kept per stream (and combined model buffer); older bytes are dropped.
const KEEP_BYTES: usize = 1024 * 1024;
/// How much of the end of a task's output goes into its notice.
const TAIL_LINES: usize = 40;
const TAIL_BYTES: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Agent,
    /// Observed, not executed or individually stoppable by carbon6.
    ClaudeCode,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Shell => "shell",
            TaskKind::Agent => "agent",
            TaskKind::ClaudeCode => "claude_code",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Started,
    /// Ended by itself (exit code in `exit_code`).
    Finished,
    /// Upstream reported failure without a process exit code.
    Failed,
    /// Its owning CLI exited without reporting an outcome.
    Lost,
    /// Stopped by the agent (`kill_shell`).
    Stopped,
    /// Stopped by the user (from the UI).
    StoppedByUser,
}

/// A background task started or ended.
#[derive(Clone, Debug)]
pub struct TaskEvent {
    pub id: String,
    pub kind: TaskKind,
    /// The command, or the subagent's description.
    pub description: String,
    pub state: TaskState,
    /// Tell the agent when it ends.
    pub notify: bool,
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
    /// The end of its output (a subagent: its report).
    pub output: String,
    /// A shell task's process group.
    pub pgid: Option<i32>,
    /// A Claude Code task's kind as the CLI names it (`local_bash`,
    /// `local_agent`, ...); empty for carbon6's own.
    pub native_type: String,
    /// That Claude Code task holds its turn (a foreground command or
    /// subagent, until the CLI moves it to the background).
    pub foreground: bool,
}

pub type TaskSink = tokio::sync::mpsc::UnboundedSender<TaskEvent>;

/// Worker-local writer ownership, independent of asynchronous task notices.
/// Reserve before creating a process; unknown ownership survives cancellation.
#[async_trait]
pub trait ShellOwner: Send + Sync {
    async fn reserve(&self, task_id: &str) -> std::io::Result<String>;
    async fn launched(&self, reservation: &str, local_pgid: Option<i32>) -> std::io::Result<()>;
    /// Only a failed spawn, before any child exists, may abandon a reservation.
    async fn not_launched(&self, reservation: &str) -> std::io::Result<()>;
}

/// Read-only inspection of a background task; matches the API DTO.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub task_id: String,
    pub kind: String,
    pub command: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub stdin: TaskStream,
    pub stdout: TaskStream,
    pub stderr: TaskStream,
    /// CLI-owned tasks expose a combined file, never separate pipes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combined: Option<TaskStream>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskStream {
    pub text: String,
    pub dropped_bytes: u64,
    pub note: String,
}

pub struct BackgroundProc {
    pub command: String,
    pub kind: TaskKind,
    pub notify: bool,
    started: Instant,
    pgid: Option<Pid>,
    cancel: Option<CancellationToken>,
    out: Mutex<Output>,
    status: Mutex<Option<i32>>,
    finished_elapsed: Mutex<Option<Duration>>,
    /// Set by kill: by whom.
    stopped: Mutex<Option<TaskState>>,
    /// Number of shell output pipes that reached their end.
    eof: AtomicUsize,
    /// On an SSH target: where it recorded its process group there.
    remote: Option<(Arc<crate::remote::SshHost>, String)>,
}

#[derive(Default)]
struct Output {
    combined: Buf,
    stdout: Buf,
    stderr: Buf,
}

#[derive(Default)]
struct Buf {
    data: Vec<u8>,
    /// Bytes dropped from the front, so `read` positions stay absolute.
    dropped: usize,
    /// Absolute position up to which the model has seen output.
    read: usize,
}

impl Buf {
    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > KEEP_BYTES {
            let excess = self.data.len() - KEEP_BYTES;
            self.data.drain(..excess);
            self.dropped += excess;
        }
    }

    fn snapshot(&self, secrets: &crate::Secrets, note: &str) -> TaskStream {
        TaskStream { text: secrets.redact(&String::from_utf8_lossy(&self.data)), dropped_bytes: self.dropped as u64, note: note.to_owned() }
    }
}

impl BackgroundProc {
    fn new(command: &str, kind: TaskKind, notify: bool, pgid: Option<Pid>, cancel: Option<CancellationToken>) -> Self {
        Self {
            command: command.to_owned(),
            kind,
            notify,
            started: Instant::now(),
            pgid,
            cancel,
            out: Mutex::default(),
            status: Mutex::new(None),
            finished_elapsed: Mutex::new(None),
            stopped: Mutex::new(None),
            eof: AtomicUsize::new(0),
            remote: None,
        }
    }

    fn push_output(&self, bytes: &[u8], stderr: bool) {
        let mut out = self.out.lock().unwrap();
        out.combined.push(bytes);
        if stderr {
            out.stderr.push(bytes);
        } else {
            out.stdout.push(bytes);
        }
    }

    /// The last lines of its output (without marking them read).
    fn tail(&self, secrets: &crate::Secrets) -> String {
        let out = self.out.lock().unwrap();
        // Redact before clipping: clipping first could expose part of a secret.
        let text = secrets.redact(&String::from_utf8_lossy(&out.combined.data));
        let lines: Vec<&str> = text.lines().collect();
        let mut t = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
        if t.len() > TAIL_BYTES {
            let mut cut = t.len() - TAIL_BYTES;
            while !t.is_char_boundary(cut) {
                cut += 1;
            }
            t = format!("…{}", &t[cut..]);
        }
        t
    }
}

#[derive(Default)]
pub struct Background {
    procs: Mutex<HashMap<String, Arc<BackgroundProc>>>,
    native: Mutex<NativeTasks>,
    next: std::sync::atomic::AtomicU64,
    sink: Arc<Mutex<Option<TaskSink>>>,
    shell_owner: Mutex<Option<Arc<dyn ShellOwner>>>,
    shell_contexts: Mutex<Vec<CancellationToken>>,
    secrets: crate::Secrets,
    /// Commands run here, or on this SSH target.
    ssh: Option<Arc<crate::remote::SshHost>>,
    /// Where shell commands keep their output and exit code, so they outlive
    /// the worker and a restarted one takes them up again (docs/restarts.md
    /// §3). None: through pipes, ending with the worker.
    held: Mutex<Option<std::path::PathBuf>>,
}

/// What a held shell command records about itself (`meta.json`).
#[derive(Serialize, Deserialize)]
struct HeldMeta {
    command: String,
    notify: bool,
    pgid: i32,
}

/// Follows a held command's output file as it grows, until its exit code is
/// written (or its process group is gone), then reads what is left.
fn follow(path: std::path::PathBuf, code: std::path::PathBuf, pgid: Option<Pid>, proc: Arc<BackgroundProc>, is_stderr: bool) {
    std::thread::spawn(move || {
        let Ok(mut f) = std::fs::File::open(&path) else {
            proc.eof.fetch_add(1, Ordering::Release);
            return;
        };
        let mut buf = [0u8; 16 * 1024];
        loop {
            match f.read(&mut buf) {
                Ok(n) if n > 0 => proc.push_output(&buf[..n], is_stderr),
                _ => {
                    let gone = pgid.is_some_and(|g| nix::sys::signal::kill(g, None).is_err());
                    if code.exists() || gone {
                        while let Ok(n) = f.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            proc.push_output(&buf[..n], is_stderr);
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        proc.eof.fetch_add(1, Ordering::Release);
    });
}

#[derive(Default)]
struct NativeTasks {
    tasks: HashMap<String, NativeTask>,
    latest: HashMap<(String, String), String>,
}

struct NativeTask {
    scope: String,
    /// The CLI's id for it.
    task_id: String,
    /// You asked the CLI to stop it.
    stop_asked: bool,
    /// The CLI's `task_type`, and whether it holds the turn.
    native_type: String,
    foreground: bool,
    description: String,
    started: Instant,
    elapsed: Option<Duration>,
    state: TaskState,
    /// Upstream progress/completion summaries, not process output.
    summary: Buf,
    combined: Option<TaskStream>,
}

/// A running task, as the UI lists it.
#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub id: String,
    pub kind: TaskKind,
    pub description: String,
    pub running: bool,
    /// Its end is to be told (false: a server or watcher meant to keep
    /// running, which an update need not wait for).
    pub notify: bool,
}

fn emit(sink: &Mutex<Option<TaskSink>>, secrets: &crate::Secrets, id: &str, p: &BackgroundProc, state: TaskState) {
    let Some(tx) = sink.lock().unwrap().clone() else { return };
    let output = if state == TaskState::Started { String::new() } else { p.tail(secrets) };
    let _ = tx.send(TaskEvent {
        id: id.to_owned(),
        kind: p.kind,
        description: secrets.redact(&p.command),
        state,
        notify: p.notify,
        exit_code: *p.status.lock().unwrap(),
        elapsed: p.started.elapsed(),
        output,
        pgid: p.pgid.map(|g| g.as_raw()),
        native_type: String::new(),
        foreground: false,
    });
}

impl Background {
    pub fn with_secrets(secrets: crate::Secrets, ssh: Option<Arc<crate::remote::SshHost>>) -> Self {
        Self { secrets, ssh, ..Default::default() }
    }

    /// Keeps shell commands' output and exit code in `dir`, so they outlive
    /// the worker (docs/restarts.md §3).
    pub fn set_held_dir(&self, dir: std::path::PathBuf) {
        *self.held.lock().unwrap() = Some(dir);
    }

    /// Takes up a held shell command a previous worker started (`id` as it
    /// was), following its output to its end. False: there is none to take
    /// up (it wasn't held, or its files are gone).
    pub fn adopt(&self, id: &str) -> bool {
        let Some(dir) = self.held.lock().unwrap().clone().map(|d| d.join(id)) else { return false };
        let Some(meta) = std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice::<HeldMeta>(&b).ok()) else {
            return false;
        };
        let pgid = (meta.pgid > 0).then(|| Pid::from_raw(meta.pgid));
        let proc = Arc::new(BackgroundProc::new(&meta.command, TaskKind::Shell, meta.notify, pgid, None));
        let code = dir.join("code");
        follow(dir.join("out"), code.clone(), pgid, proc.clone(), false);
        follow(dir.join("err"), code.clone(), pgid, proc.clone(), true);
        self.procs.lock().unwrap().insert(id.to_owned(), proc.clone());
        let (sink, secrets, task_id) = (self.sink.clone(), self.secrets.clone(), id.to_owned());
        tokio::spawn(async move {
            // Not our child: its end shows as its exit code, or as its
            // process group gone without one (stopped).
            loop {
                if code.exists() || pgid.is_none_or(|g| nix::sys::signal::kill(g, None).is_err()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            for _ in 0..40 {
                if proc.eof.load(Ordering::Acquire) == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let exit = std::fs::read_to_string(&code).ok().and_then(|c| c.trim().parse::<i32>().ok()).unwrap_or(-1);
            *proc.finished_elapsed.lock().unwrap() = Some(proc.started.elapsed());
            *proc.status.lock().unwrap() = Some(exit);
            let state = proc.stopped.lock().unwrap().unwrap_or(TaskState::Finished);
            emit(&sink, &secrets, &task_id, &proc, state);
            let _ = std::fs::remove_dir_all(dir);
        });
        true
    }

    /// Where task starts and ends are reported.
    pub fn set_sink(&self, sink: Option<TaskSink>) {
        *self.sink.lock().unwrap() = sink;
    }

    pub fn set_shell_owner(&self, owner: Arc<dyn ShellOwner>) {
        *self.shell_owner.lock().unwrap() = Some(owner);
    }

    /// Register before exposing a worker turn's tools, including MCP calls.
    pub fn shell_context(&self, cancel: &CancellationToken) {
        self.shell_contexts.lock().unwrap().push(cancel.clone());
    }

    /// Old ToolCtx tokens never reopen when the reset is confirmed. New
    /// turns register new tokens; callbacks from an old actor stay canceled.
    pub fn cancel_shell_contexts(&self) {
        for cancel in self.shell_contexts.lock().unwrap().drain(..) {
            cancel.cancel();
        }
    }

    /// Makes new ids start after `bg{n}` (ids from before a restart stay
    /// unique).
    pub fn reserve_ids(&self, n: u64) {
        self.next.fetch_max(n, Ordering::Relaxed);
    }

    /// Observe an official Claude Code SDK task event. `scope` is the CLI
    /// conversation id; `run_id` distinguishes resumed runs of a task. No
    /// numeric exit code is available in this protocol. These records never
    /// notify the model: Claude Code delivers its own task notifications.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_native(
        &self,
        scope: &str,
        task_id: &str,
        run_id: Option<&str>,
        description: Option<&str>,
        summary: Option<&str>,
        elapsed: Option<Duration>,
        state: TaskState,
    ) -> String {
        self.observe(scope, task_id, run_id, description, summary, elapsed, state, None)
    }

    /// A running native task the CLI moved to the background (it held the
    /// turn until now): reported again as started, no longer in the
    /// foreground. False: no such running task in the foreground.
    pub fn background_native(&self, scope: &str, task_id: &str, run_id: Option<&str>) -> bool {
        let mut native = self.native.lock().unwrap();
        let id = match run_id {
            Some(run) => format!("claude:{}", json!([scope, task_id, "run", run])),
            None => match native.latest.get(&(scope.to_owned(), task_id.to_owned())) {
                Some(id) => id.clone(),
                None => return false,
            },
        };
        let Some(task) = native.tasks.get_mut(&id).filter(|t| t.state == TaskState::Started && t.foreground) else { return false };
        task.foreground = false;
        if let Some(tx) = self.sink.lock().unwrap().clone() {
            let _ = tx.send(TaskEvent {
                id,
                kind: TaskKind::ClaudeCode,
                description: self.secrets.redact(&task.description),
                state: TaskState::Started,
                notify: false,
                exit_code: None,
                elapsed: task.started.elapsed(),
                output: String::new(),
                pgid: None,
                native_type: task.native_type.clone(),
                foreground: false,
            });
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn observe(
        &self,
        scope: &str,
        task_id: &str,
        run_id: Option<&str>,
        description: Option<&str>,
        summary: Option<&str>,
        elapsed: Option<Duration>,
        state: TaskState,
        kind: Option<(&str, bool)>,
    ) -> String {
        let mut native = self.native.lock().unwrap();
        let key = (scope.to_owned(), task_id.to_owned());
        // JSON array encoding is injective even if upstream ids contain our
        // separators. The prefix is disjoint from carbon6's bgN ids.
        let id = match run_id {
            Some(run) => format!("claude:{}", json!([scope, task_id, "run", run])),
            None => {
                native.latest.get(&key).cloned().unwrap_or_else(|| format!("claude:{}", json!([scope, task_id, "legacy", self.new_id()])))
            }
        };
        // Known delayed events never redirect a later run-id-less message.
        // A new explicit start authoritatively selects latest in start_native.
        if !native.tasks.contains_key(&id) {
            native.latest.insert(key, id.clone());
        }
        let is_new = !native.tasks.contains_key(&id);
        let task = native.tasks.entry(id.clone()).or_insert_with(|| NativeTask {
            scope: scope.to_owned(),
            task_id: task_id.to_owned(),
            stop_asked: false,
            native_type: String::new(),
            foreground: false,
            description: String::new(),
            started: Instant::now(),
            elapsed: None,
            state: TaskState::Started,
            summary: Buf::default(),
            combined: None,
        });
        if let Some(description) = description {
            task.description = description.to_owned();
        }
        if let Some((native_type, foreground)) = kind {
            task.native_type = native_type.to_owned();
            task.foreground = foreground;
        }
        if let Some(summary) = summary {
            // Each upstream summary replaces the previous one, rather than
            // pretending repeated progress snapshots are process writes.
            task.summary = Buf::default();
            // Metadata arrives as a complete string: redact BEFORE its
            // bounded tail is retained so trimming cannot expose a suffix.
            task.summary.push(self.secrets.redact(summary).as_bytes());
        }
        if let Some(elapsed) = elapsed {
            task.elapsed = Some(elapsed);
        }
        let finished = state != TaskState::Started && matches!(task.state, TaskState::Started | TaskState::Lost);
        // A stop you asked for reads as yours.
        let state = if state == TaskState::Stopped && task.stop_asked { TaskState::StoppedByUser } else { state };
        if finished {
            task.state = state;
            task.elapsed = Some(task.elapsed.unwrap_or_else(|| task.started.elapsed()));
        }
        if let Some(tx) = self.sink.lock().unwrap().clone() {
            let event = |state| TaskEvent {
                id: id.clone(),
                kind: TaskKind::ClaudeCode,
                description: self.secrets.redact(&task.description),
                state,
                notify: false,
                exit_code: None,
                elapsed: task.elapsed.unwrap_or_else(|| task.started.elapsed()),
                output: if state == TaskState::Started {
                    String::new()
                } else {
                    crate::util::truncate_middle(&self.secrets.redact(&String::from_utf8_lossy(&task.summary.data)), TAIL_BYTES).0
                },
                pgid: None,
                native_type: task.native_type.clone(),
                foreground: task.foreground,
            };
            if is_new {
                let _ = tx.send(event(TaskState::Started));
            }
            if finished {
                let _ = tx.send(event(state));
            }
        }
        id
    }

    /// A running native task you asked to stop: the CLI's id for it, to ask
    /// its process. `None`: not a running native task.
    pub fn native_stop(&self, id: &str) -> Option<String> {
        let mut native = self.native.lock().unwrap();
        let task = native.tasks.get_mut(id).filter(|t| t.state == TaskState::Started)?;
        task.stop_asked = true;
        Some(task.task_id.clone())
    }

    /// The CLI process is gone. Do not manufacture a successful completion
    /// for tasks whose notifications never arrived.
    pub fn close_native(&self, scope: &str, state: TaskState) {
        assert!(matches!(state, TaskState::Lost | TaskState::Stopped));
        let mut native = self.native.lock().unwrap();
        let sink = self.sink.lock().unwrap().clone();
        for (id, task) in native.tasks.iter_mut().filter(|(_, t)| t.scope == scope && t.state == TaskState::Started) {
            task.state = state;
            let elapsed = task.elapsed.unwrap_or_else(|| task.started.elapsed());
            task.elapsed = Some(elapsed);
            if let Some(tx) = &sink {
                let _ = tx.send(TaskEvent {
                    id: id.clone(),
                    kind: TaskKind::ClaudeCode,
                    description: self.secrets.redact(&task.description),
                    state,
                    notify: false,
                    exit_code: None,
                    elapsed,
                    output: "Claude Code exited without a task completion notification; numeric exit status unavailable.".into(),
                    pgid: None,
                    native_type: task.native_type.clone(),
                    foreground: task.foreground,
                });
            }
        }
    }

    /// Capture only a path supplied by the CLI task_notification, rooted in
    /// the private CLAUDE_CODE_TMPDIR assigned to that process. Descriptor
    /// traversal refuses symlinks in EVERY component and never opens files
    /// outside that root, including during concurrent path replacement.
    pub fn capture_native_output(&self, id: &str, root: &std::path::Path, path: &std::path::Path, task_id: &str) {
        let captured = self.read_native_output(root, path, task_id);
        if let Some(task) = self.native.lock().unwrap().tasks.get_mut(id) {
            task.combined = Some(match captured {
                Ok(stream) => stream,
                Err(e) => {
                    task.summary.snapshot(&self.secrets, &format!("Task summary metadata only; combined CLI output file unavailable: {e}"))
                }
            });
        }
    }

    fn read_native_output(&self, root: &std::path::Path, path: &std::path::Path, task_id: &str) -> std::io::Result<TaskStream> {
        use std::io::{Error, ErrorKind, Seek, SeekFrom};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        use std::path::Component;

        let reject = || Error::new(ErrorKind::PermissionDenied, "not a regular CLI task output in its private directory");
        if task_id.is_empty() || !task_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(reject());
        }
        let relative = path.strip_prefix(root).map_err(|_| reject())?;
        let parts: Vec<_> = relative.components().collect();
        if parts.len() < 3
            || parts.iter().any(|c| !matches!(c, Component::Normal(_)))
            || path.file_name() != Some(std::ffi::OsStr::new(&format!("{task_id}.output")))
            || path.parent().and_then(|p| p.file_name()) != Some(std::ffi::OsStr::new("tasks"))
        {
            return Err(reject());
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(root)?;
        for (i, component) in parts.iter().enumerate() {
            let name = std::ffi::CString::new(component.as_os_str().as_bytes()).map_err(|_| reject())?;
            let flags = nix::libc::O_RDONLY
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_CLOEXEC
                | nix::libc::O_NONBLOCK
                | if i + 1 < parts.len() { nix::libc::O_DIRECTORY } else { 0 };
            // SAFETY: the directory fd stays open through openat; name is a
            // NUL-terminated single component, and the returned fd is owned.
            let fd = unsafe { nix::libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(Error::last_os_error());
            }
            file = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(reject());
        }
        let overlap = self.secrets.0.lock().unwrap().iter().map(|(_, value)| value.len()).max().unwrap_or(0);
        if overlap > KEEP_BYTES {
            return Err(Error::new(ErrorKind::InvalidData, "configured secret exceeds safe output inspection limit"));
        }
        let start = metadata.len().saturating_sub((KEEP_BYTES + overlap) as u64);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take((KEEP_BYTES + overlap) as u64).read_to_end(&mut bytes)?;
        // Redact with overlap before clipping. If the read itself began
        // inside a secret, conservatively omit the first overlap redacted
        // bytes too: even shrinking replacements cannot leave that suffix.
        let text = self.secrets.redact(&String::from_utf8_lossy(&bytes));
        let mut cut = if start > 0 { overlap.min(text.len()) } else { 0 };
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        let mut buf = Buf::default();
        buf.push(&text.as_bytes()[cut..]);
        let mut stream =
            buf.snapshot(&self.secrets, "Combined CLI task output file (stdout/stderr are not separable). Read-only cached bounded tail.");
        stream.dropped_bytes = stream.dropped_bytes.saturating_add(start).saturating_add(cut as u64);
        Ok(stream)
    }

    /// A new legacy start after completion is a new run, even when old CLIs
    /// do not supply run_id. Progress/notifications without run_id still map
    /// to the last observed run, including across carbon6 turns.
    /// `native_type` is the CLI's `task_type`; `foreground`: it holds the
    /// turn (`is_backgrounded: false`).
    pub fn start_native(
        &self,
        scope: &str,
        task_id: &str,
        run_id: Option<&str>,
        description: &str,
        native_type: &str,
        foreground: bool,
    ) -> String {
        if run_id.is_none() {
            let mut native = self.native.lock().unwrap();
            let key = (scope.to_owned(), task_id.to_owned());
            if native.latest.get(&key).and_then(|id| native.tasks.get(id)).is_some_and(|t| t.state != TaskState::Started) {
                native.latest.remove(&key);
            }
        }
        let id = self.observe(scope, task_id, run_id, Some(description), None, None, TaskState::Started, Some((native_type, foreground)));
        self.native.lock().unwrap().latest.insert((scope.to_owned(), task_id.to_owned()), id.clone());
        id
    }

    fn native_snapshot(&self, id: &str) -> Option<TaskSnapshot> {
        let native = self.native.lock().unwrap();
        let task = native.tasks.get(id)?;
        Some(TaskSnapshot {
            task_id: id.to_owned(),
            kind: TaskKind::ClaudeCode.as_str().to_owned(),
            command: self.secrets.redact(&task.description),
            running: task.state == TaskState::Started,
            exit_code: None,
            elapsed_ms: task.elapsed.unwrap_or_else(|| task.started.elapsed()).as_millis().min(u64::MAX as u128) as u64,
            stdin: TaskStream { note: "Claude Code owns this task; stdin is unavailable and not writable.".into(), ..Default::default() },
            stdout: TaskStream { note: "Separate stdout is unavailable for CLI-managed tasks.".into(), ..Default::default() },
            stderr: TaskStream {
                note: format!("Claude Code lifecycle state: {:?}. Separate stdout/stderr are unavailable.", task.state),
                ..Default::default()
            },
            combined: Some(task.combined.clone().unwrap_or_else(|| {
                task.summary.snapshot(&self.secrets, "Task summary metadata only; combined CLI output file unavailable.")
            })),
        })
    }

    fn new_id(&self) -> String {
        format!("bg{}", self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Starts a shell command in the background; `notify`: tell the agent
    /// when it ends.
    pub async fn spawn(&self, command: &str, cwd: &std::path::Path, notify: bool, cancel: &CancellationToken) -> std::io::Result<String> {
        if cancel.is_cancelled() {
            return Err(std::io::Error::other("Shell launch canceled"));
        }
        let held = self.held.lock().unwrap().clone();
        if let (Some(held), None) = (held, &self.ssh) {
            return self.spawn_held(&held, command, cwd, notify, cancel).await;
        }
        let (stdout, stdout_writer) = std::io::pipe()?;
        let (stderr, stderr_writer) = std::io::pipe()?;
        let id = self.new_id();
        let owner = self.shell_owner.lock().unwrap().clone();
        let reservation = match &owner {
            Some(owner) => Some(owner.reserve(&id).await?),
            None => None,
        };
        if cancel.is_cancelled() {
            if let (Some(owner), Some(reservation)) = (&owner, &reservation) {
                owner.not_launched(reservation).await?;
            }
            return Err(std::io::Error::other("Shell launch canceled"));
        }
        let spawned = match &self.ssh {
            // On the SSH target: the script records its process group so
            // stopping the task stops it there, not just the local ssh.
            Some(h) => {
                let pidfile = crate::remote::SshHost::pidfile();
                let script = crate::remote::SshHost::script(command, cwd, &self.secrets.env(), false, &pidfile);
                h.command(crate::remote::SshHost::script_runner())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::from(stdout_writer))
                    .stderr(Stdio::from(stderr_writer))
                    .process_group(0)
                    .spawn()
                    .map(|child| (child, Some((h.clone(), pidfile, script))))
            }
            None => {
                let shell = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
                tokio::process::Command::new(shell)
                    .arg("-c")
                    .arg(command)
                    .current_dir(cwd)
                    .env("PAGER", "cat")
                    .env("GIT_PAGER", "cat")
                    .env("NO_COLOR", "1")
                    .envs(self.secrets.env())
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(stdout_writer))
                    .stderr(Stdio::from(stderr_writer))
                    .process_group(0)
                    .kill_on_drop(true)
                    .spawn()
                    .map(|child| (child, None))
            }
        };
        let (mut child, remote_launch) = match spawned {
            Ok(child) => child,
            Err(error) => {
                if let (Some(owner), Some(reservation)) = (&owner, &reservation) {
                    owner.not_launched(reservation).await?;
                }
                return Err(error);
            }
        };
        if let (Some(owner), Some(reservation)) = (&owner, &reservation) {
            // An SSH proxy's local group is not the remote writer's identity.
            let local_pgid = if remote_launch.is_none() { child.id().map(|id| id as i32) } else { None };
            owner.launched(reservation, local_pgid).await?;
        }
        let remote_pidfile = remote_launch.map(|(host, pidfile, script)| {
            let mut stdin = child.stdin.take().expect("piped");
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(script.as_bytes()).await;
            });
            (host, pidfile)
        });
        let mut proc = BackgroundProc::new(command, TaskKind::Shell, notify, child.id().map(|p| Pid::from_raw(p as i32)), None);
        proc.remote = remote_pidfile;
        let proc = Arc::new(proc);
        // Keep the combined incremental view in pipe-read order, while also
        // retaining each stream independently for non-destructive inspection.
        for (mut reader, is_stderr) in [(stdout, false), (stderr, true)] {
            let p = proc.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    p.push_output(&buf[..n], is_stderr);
                }
                p.eof.fetch_add(1, Ordering::Release);
            });
        }
        self.procs.lock().unwrap().insert(id.clone(), proc.clone());
        emit(&self.sink, &self.secrets, &id, &proc, TaskState::Started);
        let (sink, secrets, task_id) = (self.sink.clone(), self.secrets.clone(), id.clone());
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
            // Let the last output arrive (unless a child keeps the pipe open).
            for _ in 0..20 {
                if proc.eof.load(Ordering::Acquire) == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            *proc.finished_elapsed.lock().unwrap() = Some(proc.started.elapsed());
            *proc.status.lock().unwrap() = Some(code);
            let state = proc.stopped.lock().unwrap().unwrap_or(TaskState::Finished);
            emit(&sink, &secrets, &task_id, &proc, state);
        });
        Ok(id)
    }

    /// `spawn`, held: detached from the worker (its own process group, not
    /// killed when the worker goes), its output in files and its exit code
    /// written when it ends, so a restarted worker takes it up (`adopt`).
    async fn spawn_held(
        &self,
        held: &std::path::Path,
        command: &str,
        cwd: &std::path::Path,
        notify: bool,
        cancel: &CancellationToken,
    ) -> std::io::Result<String> {
        let id = self.new_id();
        let dir = held.join(&id);
        std::fs::create_dir_all(&dir)?;
        let (out, err, code) = (dir.join("out"), dir.join("err"), dir.join("code"));
        // Finish fallible file setup before reserving. After the reservation,
        // only cancellation or a failed spawn may release writer ownership.
        let (stdout, stderr) = (std::fs::File::create(&out)?, std::fs::File::create(&err)?);
        let shell = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        let owner = self.shell_owner.lock().unwrap().clone();
        let reservation = match &owner {
            Some(owner) => Some(owner.reserve(&id).await?),
            None => None,
        };
        if cancel.is_cancelled() {
            if let (Some(owner), Some(reservation)) = (&owner, &reservation) {
                owner.not_launched(reservation).await?;
            }
            return Err(std::io::Error::other("Shell launch canceled"));
        }
        // The command in its own shell; this one writes its exit code.
        let spawned = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(r#""$0" -c "$1"; echo $? > "$2""#)
            .arg(shell)
            .arg(command)
            .arg(&code)
            .current_dir(cwd)
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("NO_COLOR", "1")
            .envs(self.secrets.env())
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                if let (Some(owner), Some(reservation)) = (&owner, &reservation) {
                    owner.not_launched(reservation).await?;
                }
                return Err(error);
            }
        };
        let pgid = child.id().map(|p| Pid::from_raw(p as i32));
        if let (Some(owner), Some(reservation)) = (&owner, &reservation) {
            // This detached wrapper owns the writers' group, not a proxy.
            // Keep its reservation even if registration fails or is dropped:
            // a possibly live writer must remain a durable Rewind barrier.
            owner.launched(reservation, pgid.map(|p| p.as_raw())).await?;
        }
        let meta = HeldMeta { command: command.to_owned(), notify, pgid: pgid.map_or(0, |p| p.as_raw()) };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap_or_default())?;
        let proc = Arc::new(BackgroundProc::new(command, TaskKind::Shell, notify, pgid, None));
        follow(out, code.clone(), pgid, proc.clone(), false);
        follow(err, code.clone(), pgid, proc.clone(), true);
        self.procs.lock().unwrap().insert(id.clone(), proc.clone());
        emit(&self.sink, &self.secrets, &id, &proc, TaskState::Started);
        let (sink, secrets, task_id) = (self.sink.clone(), self.secrets.clone(), id.clone());
        tokio::spawn(async move {
            // Its exit code is in the file; none: it was stopped.
            let _ = child.wait().await;
            for _ in 0..40 {
                if proc.eof.load(Ordering::Acquire) == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let exit = std::fs::read_to_string(&code).ok().and_then(|c| c.trim().parse::<i32>().ok()).unwrap_or(-1);
            *proc.finished_elapsed.lock().unwrap() = Some(proc.started.elapsed());
            *proc.status.lock().unwrap() = Some(exit);
            let state = proc.stopped.lock().unwrap().unwrap_or(TaskState::Finished);
            emit(&sink, &secrets, &task_id, &proc, state);
            let _ = std::fs::remove_dir_all(dir);
        });
        Ok(id)
    }

    /// Registers a background subagent; returns its id and the token that
    /// stops it. Call `finish_agent` when it ends.
    pub fn start_agent(&self, description: &str, notify: bool) -> (String, CancellationToken) {
        let id = self.new_id();
        let cancel = CancellationToken::new();
        let proc = Arc::new(BackgroundProc::new(description, TaskKind::Agent, notify, None, Some(cancel.clone())));
        self.procs.lock().unwrap().insert(id.clone(), proc.clone());
        emit(&self.sink, &self.secrets, &id, &proc, TaskState::Started);
        (id, cancel)
    }

    /// Whether `id` is a background shell command (not a subagent).
    pub fn is_command(&self, id: &str) -> bool {
        self.get(id).is_some_and(|p| p.kind == TaskKind::Shell)
    }

    /// Records a background subagent's report (or failure).
    pub fn finish_agent(&self, id: &str, result: Result<String, String>) {
        let Some(p) = self.get(id) else { return };
        let (code, text) = match result {
            Ok(report) => (0, report),
            Err(e) => (1, e),
        };
        p.push_output(text.as_bytes(), code != 0);
        *p.finished_elapsed.lock().unwrap() = Some(p.started.elapsed());
        *p.status.lock().unwrap() = Some(code);
        let state = p.stopped.lock().unwrap().unwrap_or(TaskState::Finished);
        let Some(tx) = self.sink.lock().unwrap().clone() else { return };
        // A report goes to the agent whole (up to the inline limit), not
        // just its end.
        let _ = tx.send(TaskEvent {
            id: id.to_owned(),
            kind: p.kind,
            description: self.secrets.redact(&p.command),
            state,
            notify: p.notify,
            exit_code: Some(code),
            elapsed: p.started.elapsed(),
            output: crate::util::truncate_middle(&self.secrets.redact(&text), crate::MAX_OUTPUT_CHARS).0,
            pgid: None,
            native_type: String::new(),
            foreground: false,
        });
    }

    fn get(&self, id: &str) -> Option<Arc<BackgroundProc>> {
        self.procs.lock().unwrap().get(id).cloned()
    }

    /// Inspect retained streams without consuming the model's `read_new` cursor.
    pub fn snapshot(&self, id: &str) -> Option<TaskSnapshot> {
        if let Some(snapshot) = self.native_snapshot(id) {
            return Some(snapshot);
        }
        let p = self.get(id)?;
        let status = *p.status.lock().unwrap();
        let out = p.out.lock().unwrap();
        let elapsed = p.finished_elapsed.lock().unwrap().unwrap_or_else(|| p.started.elapsed());
        let (stdin, stdout_note, stderr_note) = match p.kind {
            TaskKind::Shell => (
                if p.remote.is_some() {
                    "SSH stdin carries only the launch script, then closes; not interactive or writable.".to_owned()
                } else {
                    "stdin is closed; not interactive or writable.".to_owned()
                },
                "Captured stdout.",
                "Captured stderr (including SSH diagnostics on remote tasks).",
            ),
            TaskKind::Agent => (
                format!("Delegated task: {}. No process stdin.", self.secrets.redact(&p.command)),
                "Completed subagent report; not process stdout.",
                "Subagent errors; not process stderr.",
            ),
            TaskKind::ClaudeCode => unreachable!("native tasks are stored separately"),
        };
        Some(TaskSnapshot {
            task_id: id.to_owned(),
            kind: p.kind.as_str().to_owned(),
            command: self.secrets.redact(&p.command),
            running: status.is_none(),
            exit_code: status,
            elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
            stdin: TaskStream { note: stdin, ..Default::default() },
            stdout: out.stdout.snapshot(&self.secrets, stdout_note),
            stderr: out.stderr.snapshot(&self.secrets, stderr_note),
            combined: None,
        })
    }

    /// Output since the last read, and the exit status if finished.
    pub fn read_new(&self, id: &str) -> Option<(String, Option<i32>, bool)> {
        let p = self.get(id)?;
        let status = *p.status.lock().unwrap();
        let mut output = p.out.lock().unwrap();
        let out = &mut output.combined;
        let start = out.read.max(out.dropped);
        let skipped = start > out.read;
        let text = self.secrets.redact(&String::from_utf8_lossy(&out.data[start - out.dropped..]));
        out.read = out.dropped + out.data.len();
        Some((text, status, skipped))
    }

    pub fn command(&self, id: &str) -> Option<String> {
        self.get(id).map(|p| self.secrets.redact(&p.command))
    }

    /// Stops a task (its whole process group, or the subagent). `None`: no
    /// such task; `Some(false)`: it had already ended.
    pub fn kill(&self, id: &str) -> Option<bool> {
        self.stop(id, TaskState::Stopped)
    }

    /// Stops a task because the user asked to.
    pub fn kill_by_user(&self, id: &str) -> Option<bool> {
        self.stop(id, TaskState::StoppedByUser)
    }

    fn stop(&self, id: &str, by: TaskState) -> Option<bool> {
        let p = self.get(id)?;
        if p.status.lock().unwrap().is_some() {
            return Some(false);
        }
        *p.stopped.lock().unwrap() = Some(by);
        if let Some((h, pidfile)) = &p.remote {
            // Stop it on the target, then the local ssh carrying it.
            let (h, pidfile, local) = (h.clone(), pidfile.clone(), p.pgid);
            tokio::spawn(async move {
                h.kill(&pidfile).await;
                if let Some(pg) = local {
                    let _ = killpg(pg, Signal::SIGTERM);
                }
            });
        } else if let Some(pg) = p.pgid {
            let _ = killpg(pg, Signal::SIGTERM);
        }
        if let Some(c) = &p.cancel {
            c.cancel();
        }
        Some(true)
    }

    pub fn list(&self) -> Vec<TaskInfo> {
        let mut v: Vec<TaskInfo> = self
            .procs
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| TaskInfo {
                id: id.clone(),
                kind: p.kind,
                description: self.secrets.redact(&p.command),
                running: p.status.lock().unwrap().is_none(),
                notify: p.notify,
            })
            .collect();
        v.sort_by_key(|t| t.id[2..].parse::<u64>().unwrap_or(0));
        v.extend(self.native.lock().unwrap().tasks.iter().map(|(id, t)| TaskInfo {
            id: id.clone(),
            kind: TaskKind::ClaudeCode,
            description: self.secrets.redact(&t.description),
            // One that holds the turn is counted as the turn.
            running: t.state == TaskState::Started && !t.foreground,
            notify: true,
        }));
        v
    }
}

pub struct BashOutput {
    name: &'static str,
    id_key: &'static str,
    /// Only carbon6's subagents (served to Claude Code, whose own tools
    /// read its own tasks).
    carbon6: bool,
}

impl BashOutput {
    pub fn generic() -> Self {
        Self { name: "bash_output", id_key: "id", carbon6: false }
    }

    pub fn claude() -> Self {
        Self { name: "BashOutput", id_key: "bash_id", carbon6: false }
    }

    /// For Claude Code, over MCP: `mcp__carbon6__TaskOutput`.
    pub fn carbon6() -> Self {
        Self { name: "TaskOutput", id_key: "task_id", carbon6: true }
    }
}

#[async_trait]
impl Tool for BashOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: if self.carbon6 {
                "Shows what one of carbon6's background subagents (started by this server's Task or SendMessage with run_in_background) has done so far, and whether it has finished. Its report arrives as a notice when it finishes; do not poll just to wait. Your own background tasks have their own tool."
            } else {
                "Returns new output from a background task (a command or subagent started with run_in_background), and whether it has finished. Completion is pushed automatically when notify is true (the default); do not poll just to wait."
            }
            .into(),
            parameters: json!({"type": "object", "properties": {self.id_key: {"type": "string", "description": if self.carbon6 { "The background task id (bg…) or the subagent's id" } else { "The background command's id" }}},
                "required": [self.id_key], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let id = input[self.id_key].as_str().unwrap_or_default();
        // A subagent (by its task id or session id): what it has done so far.
        if !ctx.session.background.is_command(id)
            && let Some(agents) = ctx.session.subagents()
            && let Ok(digest) = agents.peek(id).await
        {
            return ToolOutput::limited(digest, false);
        }
        let Some((text, status, skipped)) = ctx.session.background.read_new(id) else {
            return ToolOutput::err(format!("no background command {id:?}"));
        };
        let state = match status {
            None => "still running".to_owned(),
            Some(c) => format!("exited with code {c}"),
        };
        let mut out = String::new();
        if skipped {
            out.push_str("[older output was dropped]\n");
        }
        out.push_str(if text.is_empty() { "(no new output)" } else { &text });
        out.push_str(&format!("\n[{id}: {state}]"));
        ToolOutput::limited(out, false)
    }
}

pub struct KillShell {
    name: &'static str,
    id_key: &'static str,
    /// Only carbon6's subagents (see `BashOutput::carbon6`).
    carbon6: bool,
}

impl KillShell {
    pub fn generic() -> Self {
        Self { name: "kill_shell", id_key: "id", carbon6: false }
    }

    pub fn claude() -> Self {
        Self { name: "KillShell", id_key: "shell_id", carbon6: false }
    }

    /// For Claude Code, over MCP: `mcp__carbon6__TaskStop`.
    pub fn carbon6() -> Self {
        Self { name: "TaskStop", id_key: "task_id", carbon6: true }
    }
}

#[async_trait]
impl Tool for KillShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: if self.carbon6 {
                "Stops one of carbon6's background subagents (started by this server's Task or SendMessage with run_in_background). Its work so far is kept, and SendMessage can continue it. Your own background tasks have their own tool."
            } else {
                "Stops a background task: a command (with its child processes) or a subagent."
            }
            .into(),
            parameters: json!({"type": "object", "properties": {self.id_key: {"type": "string", "description": if self.carbon6 { "The background task id (bg…)" } else { "The background command's id" }}},
                "required": [self.id_key], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let id = input[self.id_key].as_str().unwrap_or_default();
        if ctx.session.background.native_snapshot(id).is_some() {
            return ToolOutput::err("Claude Code owns this task: stop it with Claude Code's own tool.");
        }
        match ctx.session.background.kill(id) {
            None => ToolOutput::err(format!("no background command {id:?}")),
            Some(false) => ToolOutput::ok(format!("{id} had already exited")),
            Some(true) => ToolOutput::ok(format!("Stopped {id}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_runs_have_real_lifecycle_and_no_fake_io_or_exit_code() {
        let secrets = crate::Secrets::default();
        secrets.0.lock().unwrap().push(("TOKEN".into(), "private-value".into()));
        let bg = Background::with_secrets(secrets, None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bg.set_sink(Some(tx));
        let id = bg.start_native("conversation", "bg1", Some("001"), "check private-value", "local_bash", false);
        let started = rx.try_recv().unwrap();
        assert_eq!(
            (started.kind, started.state, started.notify, started.exit_code),
            (TaskKind::ClaudeCode, TaskState::Started, false, None)
        );
        assert_eq!(started.description, "check [secret:TOKEN]");
        assert_ne!(id, "bg1");
        assert!(bg.snapshot(&id).unwrap().running);
        assert_eq!(bg.kill_by_user(&id), None);
        bg.observe_native(
            "conversation",
            "bg1",
            Some("001"),
            None,
            Some("done private-value"),
            Some(Duration::from_millis(25)),
            TaskState::Failed,
        );
        let end = rx.try_recv().unwrap();
        assert_eq!((end.state, end.notify, end.exit_code), (TaskState::Failed, false, None));
        assert_eq!(end.output, "done [secret:TOKEN]");
        let snapshot = bg.snapshot(&id).unwrap();
        assert!(!snapshot.running);
        assert_eq!(snapshot.exit_code, None);
        assert_eq!(snapshot.elapsed_ms, 25);
        assert_eq!(snapshot.kind, "claude_code");
        assert!(snapshot.stdin.text.is_empty());
        assert!(snapshot.stdin.note.contains("unavailable"));
        assert!(snapshot.stdout.text.is_empty());
        assert_eq!(snapshot.combined.as_ref().unwrap().text, "done [secret:TOKEN]");
        assert!(snapshot.combined.as_ref().unwrap().note.contains("summary metadata only"));
        assert!(snapshot.stderr.text.is_empty());
        assert!(snapshot.stderr.note.contains("Failed"));
        assert_eq!(snapshot, bg.snapshot(&id).unwrap());
        assert_eq!(bg.list()[0].id, id);
        bg.observe_native("conversation", "bg1", Some("001"), None, None, None, TaskState::Failed);
        assert!(rx.try_recv().is_err(), "duplicate notifications must not emit duplicate completions");
    }

    #[test]
    fn native_ids_survive_turns_and_distinguish_runs_scopes_and_legacy_restarts() {
        let bg = Background::default();
        let old = bg.start_native("session", "task", Some("zzz"), "first", "local_bash", false);
        bg.observe_native("session", "task", Some("zzz"), None, None, None, TaskState::Finished);
        let current = bg.start_native("session", "task", Some("aaa"), "resumed", "local_bash", false);
        assert_ne!(old, current);
        // A delayed progress event from the old run does not revive it or
        // change the target of a run-id-less notification in a later turn.
        bg.observe_native("session", "task", Some("zzz"), None, Some("late"), None, TaskState::Started);
        assert!(!bg.snapshot(&old).unwrap().running);
        assert_eq!(bg.observe_native("session", "task", None, None, None, None, TaskState::Stopped), current);
        assert!(!bg.snapshot(&current).unwrap().running);
        assert_ne!(bg.start_native("other", "task", Some("002"), "other session", "local_bash", false), current);
        let legacy = bg.start_native("session", "legacy", None, "legacy", "local_bash", false);
        assert_eq!(bg.start_native("session", "legacy", None, "replayed", "local_bash", false), legacy);
        bg.close_native("session", TaskState::Lost);
        let lost = bg.snapshot(&legacy).unwrap();
        assert!(!lost.running);
        assert_eq!(lost.exit_code, None);
        assert!(lost.stderr.note.contains("Lost"));
        assert_ne!(bg.start_native("session", "legacy", None, "new legacy run", "local_bash", false), legacy);
        assert_ne!(
            bg.start_native("a:b", "c", Some("d"), "", "local_bash", false),
            bg.start_native("a", "b:c", Some("d"), "", "local_bash", false)
        );
    }

    #[test]
    fn native_metadata_is_bounded_redacted_before_trimming_and_closed_honestly() {
        let secrets = crate::Secrets::default();
        // Force the clipping boundary into the middle of a secret if raw
        // bytes were clipped first. Native complete summaries redact first.
        secrets.0.lock().unwrap().push(("TOKEN".into(), "private-value".into()));
        let bg = Background::with_secrets(secrets, None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bg.set_sink(Some(tx));
        let id = bg.start_native("s", "t", None, "native", "local_bash", false);
        let text = format!("private-value{}", "x".repeat(KEEP_BYTES - 5));
        bg.observe_native("s", "t", None, None, Some(&text), None, TaskState::Started);
        let snap = bg.snapshot(&id).unwrap();
        assert!(snap.stdout.text.is_empty());
        let summary = snap.combined.unwrap();
        assert_eq!(summary.text.len(), KEEP_BYTES);
        assert!(summary.dropped_bytes > 0);
        assert!(!summary.text.contains("value"));
        assert!(snap.stderr.text.is_empty());
        bg.close_native("s", TaskState::Stopped);
        assert_eq!(rx.try_recv().unwrap().state, TaskState::Started);
        let stopped = rx.try_recv().unwrap();
        assert_eq!((stopped.state, stopped.exit_code, stopped.notify), (TaskState::Stopped, None, false));
        bg.close_native("s", TaskState::Lost);
        assert!(rx.try_recv().is_err());
        assert!(!bg.snapshot(&id).unwrap().running);
    }

    #[test]
    fn native_output_files_are_combined_bounded_redacted_and_never_follow_links() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("private-root");
        let tasks = root.join("claude-501/session/tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        let secrets = crate::Secrets::default();
        secrets.0.lock().unwrap().push(("TOKEN".into(), "private-value".into()));
        let bg = Background::with_secrets(secrets, None);
        let id = bg.start_native("s", "task", None, "command", "local_bash", false);
        let output = tasks.join("task.output");
        std::fs::write(&output, format!("{}private-value\nreal combined output", "x".repeat(KEEP_BYTES + 100))).unwrap();
        bg.capture_native_output(&id, &root, &output, "task");
        let snapshot = bg.snapshot(&id).unwrap();
        let stream = snapshot.combined.unwrap();
        assert!(stream.text.ends_with("[secret:TOKEN]\nreal combined output"));
        assert!(stream.note.contains("Combined CLI"));
        assert!(stream.text.len() <= KEEP_BYTES);
        assert!(stream.dropped_bytes > 0);
        assert!(!stream.text.contains("private-value"));
        assert!(snapshot.stdout.text.is_empty());
        assert!(snapshot.stderr.text.is_empty());

        let outside = dir.path().join("task.output");
        std::fs::write(&outside, "outside-secret").unwrap();
        let denied = |path: &std::path::Path| {
            bg.capture_native_output(&id, &root, path, "task");
            let stream = bg.snapshot(&id).unwrap().combined.unwrap();
            assert!(stream.text.is_empty(), "{path:?}");
            assert!(stream.note.contains("unavailable"), "{path:?}");
        };
        denied(&outside);
        denied(&tasks.join("../task.output"));
        std::fs::remove_file(&output).unwrap();
        symlink(&outside, &output).unwrap();
        denied(&output);
        std::fs::remove_file(&output).unwrap();
        std::fs::hard_link(&outside, &output).unwrap();
        denied(&output);
        std::fs::remove_file(&output).unwrap();
        std::fs::create_dir(&output).unwrap();
        denied(&output);
        std::fs::remove_dir(&output).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("task.output"), "outside-secret").unwrap();
        std::fs::remove_dir(&tasks).unwrap();
        symlink(&elsewhere, &tasks).unwrap();
        denied(&output);
    }

    #[tokio::test]
    async fn shell_writer_canceled_context_cannot_launch_after_reservation() {
        canceled_context_cannot_launch_after_reservation(false).await;
        canceled_context_cannot_launch_after_reservation(true).await;
    }

    async fn canceled_context_cannot_launch_after_reservation(held: bool) {
        struct Gate {
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
            abandoned: std::sync::atomic::AtomicBool,
        }
        #[async_trait]
        impl ShellOwner for Gate {
            async fn reserve(&self, _: &str) -> std::io::Result<String> {
                self.entered.notify_one();
                self.release.notified().await;
                Ok("reserved".into())
            }
            async fn launched(&self, _: &str, _: Option<i32>) -> std::io::Result<()> {
                panic!("a canceled old callback must not launch")
            }
            async fn not_launched(&self, _: &str) -> std::io::Result<()> {
                self.abandoned.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
        let gate = Arc::new(Gate { entered: Default::default(), release: Default::default(), abandoned: false.into() });
        let bg = Arc::new(Background::default());
        bg.set_shell_owner(gate.clone());
        let dir = tempfile::tempdir().unwrap();
        if held {
            bg.set_held_dir(dir.path().join("held"));
        }
        let cancel = CancellationToken::new();
        bg.shell_context(&cancel);
        let (launch, cwd, token) = (bg.clone(), dir.path().to_owned(), cancel.clone());
        let task = tokio::spawn(async move { launch.spawn("touch escaped", &cwd, false, &token).await });
        c6_testkit::within("shell reservation to wait", gate.entered.notified()).await;
        bg.cancel_shell_contexts();
        gate.release.notify_one();
        assert!(c6_testkit::within("old shell callback to refuse launch", task).await.unwrap().is_err());
        assert!(gate.abandoned.load(Ordering::SeqCst));
        assert!(!dir.path().join("escaped").exists());
        assert!(bg.spawn("touch escaped", dir.path(), false, &cancel).await.is_err(), "old token stays canceled after admission reopens");
    }

    #[derive(Default)]
    struct HeldOwner {
        reserved: Mutex<Vec<String>>,
        launched: Mutex<Vec<(String, Option<i32>)>>,
        abandoned: Mutex<Vec<String>>,
        cleanup: Mutex<Option<Pid>>,
        fail_registration: bool,
    }

    impl Drop for HeldOwner {
        fn drop(&mut self) {
            // Installed inside launched(), before spawn can return or fail:
            // even an early error/panic must not leave this test's writer.
            if let Some(group) = self.cleanup.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = killpg(group, Signal::SIGKILL);
            }
        }
    }

    #[async_trait]
    impl ShellOwner for HeldOwner {
        async fn reserve(&self, task_id: &str) -> std::io::Result<String> {
            self.reserved.lock().unwrap().push(task_id.to_owned());
            Ok(task_id.to_owned())
        }

        async fn launched(&self, reservation: &str, pgid: Option<i32>) -> std::io::Result<()> {
            *self.cleanup.lock().unwrap() = pgid.filter(|pgid| *pgid > 0).map(Pid::from_raw);
            self.launched.lock().unwrap().push((reservation.to_owned(), pgid));
            if self.fail_registration {
                return Err(std::io::Error::other("registration failed"));
            }
            Ok(())
        }

        async fn not_launched(&self, reservation: &str) -> std::io::Result<()> {
            self.abandoned.lock().unwrap().push(reservation.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn held_shell_writer_abandons_only_before_a_child_exists() {
        let dir = tempfile::tempdir().unwrap();
        let bg = Background::default();
        bg.set_held_dir(dir.path().join("held"));
        let owner = Arc::new(HeldOwner::default());
        bg.set_shell_owner(owner.clone());
        let cancel = CancellationToken::new();
        assert!(bg.spawn("true", &dir.path().join("missing-cwd"), false, &cancel).await.is_err());
        assert_eq!(*owner.reserved.lock().unwrap(), ["bg1"]);
        assert_eq!(*owner.abandoned.lock().unwrap(), ["bg1"]);
        assert!(owner.launched.lock().unwrap().is_empty());

        let mut owner = HeldOwner::default();
        owner.fail_registration = true;
        let owner = Arc::new(owner);
        bg.set_shell_owner(owner.clone());
        // Even a command whose leader exits immediately has been launched:
        // failure to persist its group must retain the unknown reservation.
        let error = bg.spawn("true", dir.path(), false, &cancel).await.unwrap_err();
        assert_eq!(error.to_string(), "registration failed");
        assert_eq!(*owner.reserved.lock().unwrap(), ["bg2"]);
        let launched = owner.launched.lock().unwrap();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].0, "bg2");
        assert!(launched[0].1.is_some_and(|pgid| pgid > 0));
        assert!(owner.abandoned.lock().unwrap().is_empty());
    }

    #[test]
    fn held_shell_registers_its_group_and_is_adopted_after_runtime_restart() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held");
        let owner = Arc::new(HeldOwner::default());
        let runtime = || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let first = runtime();
        let id = first.block_on(async {
            let bg = Background::default();
            bg.set_held_dir(held.clone());
            bg.set_shell_owner(owner.clone());
            // Bound the gate too, in case a registration regression bypasses
            // the test owner's cleanup callback entirely.
            bg.spawn(
                "printf before; n=0; while [ ! -e release ]; do n=$((n + 1)); [ \"$n\" -lt 1000 ] || exit 99; sleep 0.01; done; printf after; printf error >&2; exit 7",
                dir.path(),
                true,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
        });
        let meta: HeldMeta = serde_json::from_slice(&std::fs::read(held.join(&id).join("meta.json")).unwrap()).unwrap();
        assert!(meta.pgid > 0);
        assert_eq!(*owner.reserved.lock().unwrap(), [id.clone()]);
        assert_eq!(*owner.launched.lock().unwrap(), [(id.clone(), Some(meta.pgid))]);
        assert!(owner.abandoned.lock().unwrap().is_empty());
        drop(first);
        assert!(nix::sys::signal::kill(Pid::from_raw(meta.pgid), None).is_ok(), "runtime shutdown must not kill a held command");

        let second = runtime();
        second.block_on(async {
            let bg = Background::default();
            bg.set_held_dir(held.clone());
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            bg.set_sink(Some(tx));
            assert!(bg.adopt(&id));
            assert!(bg.snapshot(&id).unwrap().running);
            std::fs::write(dir.path().join("release"), "").unwrap();
            let done = next(&mut rx).await;
            assert_eq!((done.id, done.state, done.exit_code, done.notify), (id.clone(), TaskState::Finished, Some(7), true));
            // This command leaves no background children. Its completion is
            // observed: disarm cleanup rather than signal an old group later.
            owner.cleanup.lock().unwrap().take();
            let snapshot = bg.snapshot(&id).unwrap();
            assert_eq!(snapshot.stdout.text, "beforeafter");
            assert_eq!(snapshot.stderr.text, "error");
            assert!(!snapshot.running);
            assert_eq!(*owner.reserved.lock().unwrap(), [id]);
            assert_eq!(owner.launched.lock().unwrap().len(), 1, "adoption must not create a new ownership reservation");
        });
    }

    async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<TaskEvent>) -> TaskEvent {
        tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn shell_streams_snapshots_and_completion_are_independent() {
        let secrets = crate::Secrets::default();
        secrets.0.lock().unwrap().push(("TOKEN".into(), "private-value".into()));
        let bg = Background::with_secrets(secrets, None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bg.set_sink(Some(tx));
        let dir = tempfile::tempdir().unwrap();
        let id = bg
            .spawn(
                "if read -r line; then exit 99; fi; printf 'out private-value'; printf 'err private-value' >&2; exit 7",
                dir.path(),
                true,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let started = next(&mut rx).await;
        assert_eq!(started.state, TaskState::Started);
        assert!(started.notify);
        assert!(!started.description.contains("private-value"));
        let done = next(&mut rx).await;
        assert_eq!((done.state, done.exit_code), (TaskState::Finished, Some(7)));
        assert!(done.notify);
        assert!(done.output.contains("out [secret:TOKEN]"));
        assert!(done.output.contains("err [secret:TOKEN]"));
        let snapshot = bg.snapshot(&id).unwrap();
        assert_eq!(snapshot.stdout.text, "out [secret:TOKEN]");
        assert_eq!(snapshot.stderr.text, "err [secret:TOKEN]");
        assert_eq!(snapshot.exit_code, Some(7));
        assert!(!snapshot.running);
        assert!(snapshot.stdin.text.is_empty());
        assert!(snapshot.stdin.note.contains("closed"));
        assert!(!serde_json::to_string(&snapshot).unwrap().contains("private-value"));
        assert_eq!(snapshot, bg.snapshot(&id).unwrap());
        let (text, code, skipped) = bg.read_new(&id).unwrap();
        assert!(text.contains(&snapshot.stdout.text));
        assert!(text.contains(&snapshot.stderr.text));
        assert_eq!((code, skipped), (Some(7), false));
        assert!(bg.read_new(&id).unwrap().0.is_empty());
        assert_eq!(snapshot, bg.snapshot(&id).unwrap());
        assert!(bg.snapshot("missing").is_none());
    }

    #[tokio::test]
    async fn ssh_streams_are_separate_and_stdin_is_only_a_launch_script() {
        let dir = tempfile::tempdir().unwrap();
        let program = crate::remote::tests::fake_ssh(dir.path());
        c6_testkit::runnable(&program, &[]).await;
        let host = Arc::new(crate::remote::SshHost::new("fake", "/tmp").with_program(&program));
        let bg = Background::with_secrets(Default::default(), Some(host));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bg.set_sink(Some(tx));
        let id = bg
            .spawn(
                "if read -r line; then echo unexpected-input; else echo closed; fi; echo remote-error >&2",
                dir.path(),
                true,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await.state, TaskState::Started);
        assert_eq!(next(&mut rx).await.exit_code, Some(0));
        let snapshot = bg.snapshot(&id).unwrap();
        assert_eq!(snapshot.stdout.text, "closed\n");
        assert_eq!(snapshot.stderr.text, "remote-error\n");
        assert!(snapshot.stdin.text.is_empty());
        assert!(snapshot.stdin.note.contains("launch script"));
        assert!(snapshot.stdin.note.contains("not interactive or writable"));
        let combined = bg.read_new(&id).unwrap().0;
        assert!(combined.contains("closed\n"));
        assert!(combined.contains("remote-error\n"));
    }

    #[test]
    fn streams_are_bounded_and_snapshots_do_not_consume_new_output() {
        let bg = Background::default();
        let p = Arc::new(BackgroundProc::new("build", TaskKind::Shell, false, None, None));
        bg.procs.lock().unwrap().insert("bg1".into(), p.clone());
        p.push_output(&vec![b'a'; KEEP_BYTES + 10], false);
        p.push_output(b"error", true);
        let snapshot = bg.snapshot("bg1").unwrap();
        assert_eq!(snapshot.stdout.text.len(), KEEP_BYTES);
        assert_eq!(snapshot.stdout.dropped_bytes, 10);
        assert_eq!(snapshot.stderr.text, "error");
        assert_eq!(snapshot.stderr.dropped_bytes, 0);
        assert!(snapshot.running);
        let (text, status, skipped) = bg.read_new("bg1").unwrap();
        assert_eq!(text.len(), KEEP_BYTES);
        assert!(text.ends_with("error"));
        assert_eq!(status, None);
        assert!(skipped);
        p.push_output(b"next", false);
        let snapshot = bg.snapshot("bg1").unwrap();
        assert!(snapshot.stdout.text.ends_with("next"));
        assert_eq!(bg.read_new("bg1").unwrap().0, "next");
        assert_eq!(snapshot.stdout.dropped_bytes, 14);
    }

    #[test]
    fn truncated_streams_and_command_accessors_are_redacted() {
        let secrets = crate::Secrets::default();
        secrets.0.lock().unwrap().push(("TOKEN".into(), "private-value".into()));
        let bg = Background::with_secrets(secrets, None);
        let p = Arc::new(BackgroundProc::new("echo private-value", TaskKind::Shell, false, None, None));
        bg.procs.lock().unwrap().insert("bg1".into(), p.clone());
        p.push_output(&vec![b'a'; KEEP_BYTES + 1], false);
        p.push_output(b"private-value", false);
        p.push_output(b"private-value", true);
        let snapshot = bg.snapshot("bg1").unwrap();
        assert_eq!(snapshot.stdout.dropped_bytes, 14);
        assert!(snapshot.stdout.text.ends_with("[secret:TOKEN]"));
        assert_eq!(snapshot.stderr.text, "[secret:TOKEN]");
        assert!(!serde_json::to_string(&snapshot).unwrap().contains("private-value"));
        assert_eq!(bg.command("bg1").unwrap(), "echo [secret:TOKEN]");
        assert_eq!(bg.list()[0].description, "echo [secret:TOKEN]");
        let (text, _, skipped) = bg.read_new("bg1").unwrap();
        assert!(skipped);
        assert!(!text.contains("private-value"));
    }

    #[test]
    fn agent_snapshots_describe_delegation_not_process_io() {
        let secrets = crate::Secrets::default();
        secrets.0.lock().unwrap().push(("TOKEN".into(), "private-value".into()));
        let bg = Background::with_secrets(secrets, None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bg.set_sink(Some(tx));
        let (id, _) = bg.start_agent("investigate private-value", true);
        assert_eq!(rx.try_recv().unwrap().description, "investigate [secret:TOKEN]");
        let running = bg.snapshot(&id).unwrap();
        assert!(running.running);
        assert_eq!(running.kind, "agent");
        assert!(running.stdin.note.contains("investigate [secret:TOKEN]"));
        assert!(running.stdin.text.is_empty());
        bg.finish_agent(&id, Ok("report private-value".into()));
        let done = rx.try_recv().unwrap();
        assert_eq!(done.description, "investigate [secret:TOKEN]");
        assert_eq!(done.output, "report [secret:TOKEN]");
        let report = bg.snapshot(&id).unwrap();
        assert_eq!(report.stdout.text, "report [secret:TOKEN]");
        assert!(report.stdout.note.contains("report"));
        assert!(report.stderr.text.is_empty());
        assert_eq!(bg.read_new(&id).unwrap().0, report.stdout.text);
        let (id, _) = bg.start_agent("try again", false);
        bg.finish_agent(&id, Err("failure private-value".into()));
        let failure = bg.snapshot(&id).unwrap();
        assert!(failure.stdout.text.is_empty());
        assert_eq!(failure.stderr.text, "failure [secret:TOKEN]");
        assert_eq!(failure.exit_code, Some(1));
        assert_eq!(bg.read_new(&id).unwrap().0, failure.stderr.text);
    }

    #[test]
    fn a_native_task_you_stop_reads_as_stopped_by_you() {
        let bg = Background::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bg.set_sink(Some(tx));
        let id = bg.start_native("s", "t1", Some("r1"), "Build", "local_bash", false);
        assert_eq!(rx.try_recv().unwrap().state, TaskState::Started);
        assert_eq!(bg.native_stop(&id).as_deref(), Some("t1"), "the CLI's own id, to ask it");
        bg.observe_native("s", "t1", Some("r1"), None, Some("Stopped"), None, TaskState::Stopped);
        assert_eq!(rx.try_recv().unwrap().state, TaskState::StoppedByUser);
        assert_eq!(bg.native_stop(&id), None, "an ended task cannot be stopped");
        assert_eq!(bg.native_stop("bg1"), None, "not a native task");
    }
}
