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
}

pub type TaskSink = tokio::sync::mpsc::UnboundedSender<TaskEvent>;

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
    secrets: crate::Secrets,
    /// Commands run here, or on this SSH target.
    ssh: Option<Arc<crate::remote::SshHost>>,
}

#[derive(Default)]
struct NativeTasks {
    tasks: HashMap<String, NativeTask>,
    latest: HashMap<(String, String), String>,
}

struct NativeTask {
    scope: String,
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
    });
}

impl Background {
    pub fn with_secrets(secrets: crate::Secrets, ssh: Option<Arc<crate::remote::SshHost>>) -> Self {
        Self { secrets, ssh, ..Default::default() }
    }

    /// Where task starts and ends are reported.
    pub fn set_sink(&self, sink: Option<TaskSink>) {
        *self.sink.lock().unwrap() = sink;
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
    pub fn start_native(&self, scope: &str, task_id: &str, run_id: Option<&str>, description: &str) -> String {
        if run_id.is_none() {
            let mut native = self.native.lock().unwrap();
            let key = (scope.to_owned(), task_id.to_owned());
            if native.latest.get(&key).and_then(|id| native.tasks.get(id)).is_some_and(|t| t.state != TaskState::Started) {
                native.latest.remove(&key);
            }
        }
        let id = self.observe_native(scope, task_id, run_id, Some(description), None, None, TaskState::Started);
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
                note: format!("Claude Code lifecycle state: {:?}. Separate stdout/stderr and individual Stop are unavailable.", task.state),
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
    pub fn spawn(&self, command: &str, cwd: &std::path::Path, notify: bool) -> std::io::Result<String> {
        let (stdout, stdout_writer) = std::io::pipe()?;
        let (stderr, stderr_writer) = std::io::pipe()?;
        let (mut child, remote_pidfile) = match &self.ssh {
            // On the SSH target: the script records its process group so
            // stopping the task stops it there, not just the local ssh.
            Some(h) => {
                let pidfile = crate::remote::SshHost::pidfile();
                let script = crate::remote::SshHost::script(command, cwd, &self.secrets.env(), false, &pidfile);
                let mut child = h
                    .command(crate::remote::SshHost::script_runner())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::from(stdout_writer))
                    .stderr(Stdio::from(stderr_writer))
                    .process_group(0)
                    .spawn()?;
                let mut stdin = child.stdin.take().expect("piped");
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(script.as_bytes()).await;
                });
                (child, Some((h.clone(), pidfile)))
            }
            None => {
                let shell = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
                let child = tokio::process::Command::new(shell)
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
                    .spawn()?;
                (child, None)
            }
        };
        let id = self.new_id();
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
            })
            .collect();
        v.sort_by_key(|t| t.id[2..].parse::<u64>().unwrap_or(0));
        v.extend(self.native.lock().unwrap().tasks.iter().map(|(id, t)| TaskInfo {
            id: id.clone(),
            kind: TaskKind::ClaudeCode,
            description: self.secrets.redact(&t.description),
            running: t.state == TaskState::Started,
        }));
        v
    }
}

pub struct BashOutput {
    name: &'static str,
    id_key: &'static str,
}

impl BashOutput {
    pub fn generic() -> Self {
        Self { name: "bash_output", id_key: "id" }
    }

    pub fn claude() -> Self {
        Self { name: "BashOutput", id_key: "bash_id" }
    }
}

#[async_trait]
impl Tool for BashOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "Returns new output from a background task (a command or subagent started with run_in_background), and whether it has finished. Completion is pushed automatically when notify is true (the default); do not poll just to wait.".into(),
            parameters: json!({"type": "object", "properties": {self.id_key: {"type": "string", "description": "The background command's id"}},
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
}

impl KillShell {
    pub fn generic() -> Self {
        Self { name: "kill_shell", id_key: "id" }
    }

    pub fn claude() -> Self {
        Self { name: "KillShell", id_key: "shell_id" }
    }
}

#[async_trait]
impl Tool for KillShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "Stops a background task: a command (with its child processes) or a subagent.".into(),
            parameters: json!({"type": "object", "properties": {self.id_key: {"type": "string", "description": "The background command's id"}},
                "required": [self.id_key], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let id = input[self.id_key].as_str().unwrap_or_default();
        if ctx.session.background.native_snapshot(id).is_some() {
            return ToolOutput::err("Claude Code owns this task; stopping individual native tasks from carbon6 is unsupported.");
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
        let id = bg.start_native("conversation", "bg1", Some("001"), "check private-value");
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
        let old = bg.start_native("session", "task", Some("zzz"), "first");
        bg.observe_native("session", "task", Some("zzz"), None, None, None, TaskState::Finished);
        let current = bg.start_native("session", "task", Some("aaa"), "resumed");
        assert_ne!(old, current);
        // A delayed progress event from the old run does not revive it or
        // change the target of a run-id-less notification in a later turn.
        bg.observe_native("session", "task", Some("zzz"), None, Some("late"), None, TaskState::Started);
        assert!(!bg.snapshot(&old).unwrap().running);
        assert_eq!(bg.observe_native("session", "task", None, None, None, None, TaskState::Stopped), current);
        assert!(!bg.snapshot(&current).unwrap().running);
        assert_ne!(bg.start_native("other", "task", Some("002"), "other session"), current);
        let legacy = bg.start_native("session", "legacy", None, "legacy");
        assert_eq!(bg.start_native("session", "legacy", None, "replayed"), legacy);
        bg.close_native("session", TaskState::Lost);
        let lost = bg.snapshot(&legacy).unwrap();
        assert!(!lost.running);
        assert_eq!(lost.exit_code, None);
        assert!(lost.stderr.note.contains("Lost"));
        assert_ne!(bg.start_native("session", "legacy", None, "new legacy run"), legacy);
        assert_ne!(bg.start_native("a:b", "c", Some("d"), ""), bg.start_native("a", "b:c", Some("d"), ""));
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
        let id = bg.start_native("s", "t", None, "native");
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
        let id = bg.start_native("s", "task", None, "command");
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
            )
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
        let id =
            bg.spawn("if read -r line; then echo unexpected-input; else echo closed; fi; echo remote-error >&2", dir.path(), true).unwrap();
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
}
