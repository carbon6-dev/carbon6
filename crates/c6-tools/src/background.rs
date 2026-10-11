//! Background tasks: shell commands started by `bash` with
//! `run_in_background`, and subagents started by Task with
//! `run_in_background`. Read with `bash_output`, stopped with `kill_shell`.
//!
//! The runtime can register a sink to hear when tasks start and end; the
//! worker logs them and, when a task asked to be noticed, tells the agent.

use std::collections::HashMap;
use std::io::Read as _;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use c6_llm::ToolSpec;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// Output kept per background command; older output is dropped.
const KEEP_BYTES: usize = 1024 * 1024;
/// How much of the end of a task's output goes into its notice.
const TAIL_LINES: usize = 40;
const TAIL_BYTES: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Agent,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Shell => "shell",
            TaskKind::Agent => "agent",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Started,
    /// Ended by itself (exit code in `exit_code`).
    Finished,
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

pub struct BackgroundProc {
    pub command: String,
    pub kind: TaskKind,
    pub notify: bool,
    started: Instant,
    pgid: Option<Pid>,
    cancel: Option<CancellationToken>,
    out: Mutex<Buf>,
    status: Mutex<Option<i32>>,
    /// Set by kill: by whom.
    stopped: Mutex<Option<TaskState>>,
    /// The output pipe reached its end.
    eof: AtomicBool,
    /// On an SSH target: where it recorded its process group there.
    remote: Option<(Arc<crate::remote::SshHost>, String)>,
}

#[derive(Default)]
struct Buf {
    data: Vec<u8>,
    /// Bytes dropped from the front, so `read` positions stay absolute.
    dropped: usize,
    /// Absolute position up to which the model has seen output.
    read: usize,
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
            stopped: Mutex::new(None),
            eof: AtomicBool::new(false),
            remote: None,
        }
    }

    fn push_output(&self, bytes: &[u8]) {
        let mut out = self.out.lock().unwrap();
        out.data.extend_from_slice(bytes);
        if out.data.len() > KEEP_BYTES {
            let excess = out.data.len() - KEEP_BYTES;
            out.data.drain(..excess);
            out.dropped += excess;
        }
    }

    /// The last lines of its output (without marking them read).
    fn tail(&self) -> String {
        let out = self.out.lock().unwrap();
        let text = String::from_utf8_lossy(&out.data);
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
    next: std::sync::atomic::AtomicU64,
    sink: Arc<Mutex<Option<TaskSink>>>,
    secrets: crate::Secrets,
    /// Commands run here, or on this SSH target.
    ssh: Option<Arc<crate::remote::SshHost>>,
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
    let output = if state == TaskState::Started { String::new() } else { secrets.redact(&p.tail()) };
    let _ = tx.send(TaskEvent {
        id: id.to_owned(),
        kind: p.kind,
        description: p.command.clone(),
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

    fn new_id(&self) -> String {
        format!("bg{}", self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Starts a shell command in the background; `notify`: tell the agent
    /// when it ends.
    pub fn spawn(&self, command: &str, cwd: &std::path::Path, notify: bool) -> std::io::Result<String> {
        let (reader, writer) = std::io::pipe()?;
        let (mut child, remote_pidfile) = match &self.ssh {
            // On the SSH target: the script records its process group so
            // stopping the task stops it there, not just the local ssh.
            Some(h) => {
                let pidfile = crate::remote::SshHost::pidfile();
                let script = crate::remote::SshHost::script(command, cwd, &self.secrets.env(), false, &pidfile);
                let mut child = h
                    .command(crate::remote::SshHost::script_runner())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::from(writer.try_clone()?))
                    .stderr(Stdio::from(writer))
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
                    .stdout(Stdio::from(writer.try_clone()?))
                    .stderr(Stdio::from(writer))
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
        let p = proc.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 16 * 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                p.push_output(&buf[..n]);
            }
            p.eof.store(true, Ordering::Release);
        });
        self.procs.lock().unwrap().insert(id.clone(), proc.clone());
        emit(&self.sink, &self.secrets, &id, &proc, TaskState::Started);
        let (sink, secrets, task_id) = (self.sink.clone(), self.secrets.clone(), id.clone());
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
            // Let the last output arrive (unless a child keeps the pipe open).
            for _ in 0..20 {
                if proc.eof.load(Ordering::Acquire) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
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

    /// Records a background subagent's report (or failure).
    pub fn finish_agent(&self, id: &str, result: Result<String, String>) {
        let Some(p) = self.get(id) else { return };
        let (code, text) = match result {
            Ok(report) => (0, report),
            Err(e) => (1, format!("The subagent failed: {e}")),
        };
        p.push_output(text.as_bytes());
        *p.status.lock().unwrap() = Some(code);
        let state = p.stopped.lock().unwrap().unwrap_or(TaskState::Finished);
        let Some(tx) = self.sink.lock().unwrap().clone() else { return };
        // A report goes to the agent whole (up to the inline limit), not
        // just its end.
        let _ = tx.send(TaskEvent {
            id: id.to_owned(),
            kind: p.kind,
            description: p.command.clone(),
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

    /// Output since the last read, and the exit status if finished.
    pub fn read_new(&self, id: &str) -> Option<(String, Option<i32>, bool)> {
        let p = self.get(id)?;
        let status = *p.status.lock().unwrap();
        let mut out = p.out.lock().unwrap();
        let start = out.read.max(out.dropped);
        let skipped = start > out.read;
        let text = self.secrets.redact(&String::from_utf8_lossy(&out.data[start - out.dropped..]));
        out.read = out.dropped + out.data.len();
        Some((text, status, skipped))
    }

    pub fn command(&self, id: &str) -> Option<String> {
        self.get(id).map(|p| p.command.clone())
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
                description: p.command.clone(),
                running: p.status.lock().unwrap().is_none(),
            })
            .collect();
        v.sort_by_key(|t| t.id[2..].parse::<u64>().unwrap_or(0));
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
            description: "Returns new output from a background task (a command or subagent started with run_in_background), and whether it has finished.".into(),
            parameters: json!({"type": "object", "properties": {self.id_key: {"type": "string", "description": "The background command's id"}},
                "required": [self.id_key], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let id = input[self.id_key].as_str().unwrap_or_default();
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
        match ctx.session.background.kill(id) {
            None => ToolOutput::err(format!("no background command {id:?}")),
            Some(false) => ToolOutput::ok(format!("{id} had already exited")),
            Some(true) => ToolOutput::ok(format!("Stopped {id}")),
        }
    }
}
