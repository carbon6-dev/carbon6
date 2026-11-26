//! Session runtime: one actor task per active session, each the single writer
//! of its event log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, build_system_prompt, history_from_events, run_turn};
use c6_llm::{Block, Message, Provider, Role, ToolProfile, model_info};
use c6_proto::{
    BackgroundTask, Checkpoint, CreateSession, EphemeralEvent, ErrorEvent, ModelSpec, ModelSwitched, Notice, Rewound, SessionActivity,
    SessionCommand, SessionEvent, SessionStarted, TurnEndReason, TurnEnded, TurnStarted, ephemeral_event, session_command, session_event,
};
use c6_tools::{SessionTools, Skill, SkillTool, Tool};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::checkpoint::Checkpoints;
use crate::journal::{Journal, StopUnconfirmed};
use crate::link::HeadLink;

/// Cheap to clone: a session's actor holds one to start its subagents.
#[derive(Clone)]
pub struct SessionManager {
    journal: Arc<Journal>,
    pub(crate) link: Arc<HeadLink>,
    worktrees: PathBuf,
    /// The worker's state directory (checkpoint repositories live here).
    state_dir: PathBuf,
    actors: Arc<Mutex<HashMap<String, Handle>>>,
    loads: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Delegations running per top-level session (for its limit).
    working: Arc<Mutex<HashMap<String, usize>>>,
    pub(crate) launch_pressure: Arc<crate::resource_pressure::Gate>,
    pub(crate) disk: Arc<crate::disk::Monitor>,
    /// The MCP server that gives Claude Code sessions carbon6's subagents
    /// (started on first use; `None` if it could not start).
    bridge: Arc<tokio::sync::OnceCell<Option<Arc<crate::mcp_bridge::Bridge>>>>,
    /// Calls waiting on a remote subagent's turn, by `<session>/<call>`
    /// (docs/remote-subagents.md): the head's DelegationEnded goes there.
    remote_waits: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<c6_proto::DelegationEnded>>>>,
}

/// One delegation counted against its tree's limit until dropped.
pub(crate) struct Working {
    counts: Arc<Mutex<HashMap<String, usize>>>,
    root: String,
}

impl Drop for Working {
    fn drop(&mut self) {
        let mut c = self.counts.lock().unwrap();
        if let Some(n) = c.get_mut(&self.root) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.remove(&self.root);
            }
        }
    }
}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A live actor: where its messages go, and its tools' shared state (a
/// subagent works with its parent's hooks, publisher and the like).
#[derive(Clone)]
struct Handle {
    tx: mpsc::UnboundedSender<ActorMsg>,
    recovery: Arc<RecoveryGate>,
    tools: Arc<SessionTools>,
    /// A turn is running.
    running: Arc<std::sync::atomic::AtomicBool>,
    /// It runs Claude Code now, whose tools carbon6 does not run (so its
    /// subagents do not share them). A handoff changes it.
    claude_code: Arc<std::sync::atomic::AtomicBool>,
    /// Inputs waiting for the next model-request boundary: the head edits
    /// or cancels one before it is delivered.
    inbox: Arc<Mutex<Inbox>>,
}

/// How a delegation's turn ended.
pub(crate) struct Outcome {
    pub reason: TurnEndReason,
    /// The final assistant text of the turn (empty if none).
    pub report: String,
    /// The turn's last error, if any.
    pub error: String,
}

impl Outcome {
    /// The diagnostic is UI text; the agent sees the realm's editable prompt.
    pub(crate) fn with_failure_cause(&self, spec: &CreateSession, report: String) -> String {
        if self.reason != TurnEndReason::Error || self.error != JOURNAL_FAILURE_DIAGNOSTIC {
            return report;
        }
        let cause = spec_prompt(spec, "session.task_lost_after_failure", c6_agent::TASK_LOST_AFTER_FAILURE);
        format!("{cause}\n\n{report}")
    }
}

pub(crate) struct CommandChoice {
    /// The command's variants from the head: [{applies_to, body}].
    candidates_json: String,
    /// The session's labels from the head as of the message.
    labels: Vec<String>,
    /// Claude Code's carbon6 tools as the head's settings have them now.
    claude_code: Option<c6_proto::ClaudeCodeToolChoice>,
}

/// A session's labels: the head's (yours, worker:) and the automatic ones
/// only the worker knows (repo:, folder:) or that change (model:).
pub(crate) fn session_labels(head_labels: &[String], repo: &str, cwd: &str, home: &str, model: &str, ssh: &str) -> Vec<String> {
    let mut l = head_labels.to_vec();
    l.extend(c6_api::labels::auto_labels("", repo, cwd, home, model, ssh));
    l
}

/// Where a new session works: its folder (on the SSH target, if any), the
/// local folder for what runs on the worker, and what labels are made of.
struct Place {
    cwd: PathBuf,
    local: PathBuf,
    home: String,
    repo: String,
    worktree_branch: String,
    repo_root: String,
    ssh_target: String,
    system: String,
    /// What the worktree's setup commands printed.
    setup_log: String,
}

/// The repository's git remote (origin, else the first) as
/// host/owner/name; empty outside a repository or without a remote.
pub(crate) async fn repo_slug(cwd: &Path) -> String {
    let git = |args: &'static [&'static str]| {
        let mut c = c6_tools::process::session_command("git");
        c.arg("-C").arg(cwd).args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        c
    };
    let url = match git(&["remote", "get-url", "origin"]).output().await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        _ => match git(&["remote"]).output().await {
            Ok(o) if o.status.success() => {
                let first = String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_owned();
                if first.is_empty() {
                    return String::new();
                }
                match c6_tools::process::session_command("git").arg("-C").arg(cwd).args(["remote", "get-url", &first]).output().await {
                    Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
                    _ => return String::new(),
                }
            }
            _ => return String::new(),
        },
    };
    normalize_remote(&url)
}

/// `git@github.com:acme/app.git`, `https://user@github.com/acme/app` and
/// `ssh://git@github.com:22/acme/app.git` are all `github.com/acme/app`.
pub(crate) fn normalize_remote(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit('@').next().unwrap_or(authority);
        (host.split(':').next().unwrap_or(host).to_owned(), path.to_owned())
    } else if let Some((authority, path)) = url.split_once(':').filter(|(a, _)| !a.contains('/')) {
        (authority.rsplit('@').next().unwrap_or(authority).to_owned(), path.to_owned())
    } else {
        return url.to_owned(); // a local path
    };
    format!("{}/{}", host.to_ascii_lowercase(), path.trim_start_matches('/'))
}

/// The UI diagnostic also identifies this error when replaying a child report.
pub(crate) const JOURNAL_FAILURE_DIAGNOSTIC: &str = "Session stopped after a journal write failed";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskLossCause {
    WorkerRestart,
    Unknown,
}

impl TaskLossCause {
    fn prompt(self, spec: &CreateSession) -> String {
        match self {
            Self::WorkerRestart => spec_prompt(spec, "session.task_lost_after_restart", c6_agent::TASK_LOST_AFTER_RESTART),
            Self::Unknown => spec_prompt(spec, "session.task_lost_unknown", c6_agent::TASK_LOST_UNKNOWN),
        }
    }

    fn remote_prompt(self, spec: &CreateSession) -> String {
        match self {
            Self::WorkerRestart => spec_prompt(spec, "session.subagent_remote_reattach", c6_agent::SUBAGENT_REMOTE_REATTACH),
            Self::Unknown => {
                spec_prompt(spec, "session.subagent_remote_reattach_after_reload", c6_agent::SUBAGENT_REMOTE_REATTACH_AFTER_RELOAD)
            }
        }
    }
}

pub(crate) enum ActorMsg {
    HealthProbe(tokio::sync::oneshot::Sender<()>),
    FreezeMove {
        move_id: String,
        epoch: u64,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    ChangePlace(String, c6_proto::ChangePlace, tokio::sync::oneshot::Sender<Result<(), String>>),
    /// An up-front refusal still needs a durable Error for the Rewind panel.
    RewindRefused {
        command_id: String,
        reason: String,
    },
    /// Rewind the conversation (after interrupting a running turn).
    Rewind {
        command_id: String,
        to_seq: u64,
        restore_files: bool,
    },
    /// Confirm a local child's queues and work are idle before its parent rewinds.
    RewindIdle(tokio::sync::oneshot::Sender<bool>),
    /// An input, possibly switching the session's model from it on, and
    /// what is needed to pick the variant of a custom command it invokes.
    Input(Box<PendingInput>, Option<ModelSpec>, Box<CommandChoice>),
    Interrupt,
    /// The parent cancelled its wait; stop the child's counted work too.
    InterruptDelegation,
    /// Send now for an input already with the agent (its command id).
    SendNow(String),
    /// A background task started or ended.
    Task(c6_tools::TaskEvent),
    /// All task callbacks submitted before a delegated turn ended precede this.
    TaskFence(tokio::sync::oneshot::Sender<()>),
    /// A background task whose outcome was missing from the loaded journal.
    Lost {
        cause: TaskLossCause,
        task_id: String,
        kind: String,
        description: String,
        native_type: String,
        notify: bool,
        pgid: i32,
        /// A subagent's session.
        child: String,
        /// That subagent is remote: it runs on without this worker.
        remote: bool,
    },
    /// Stop a background task (the user asked).
    StopTask(String),
    /// Task ids up to bg{n} were used before a restart.
    Reserve(u64),
    /// The agent published a file.
    Published(c6_tools::Published),
    /// The agent changed a memory note.
    Memory(c6_tools::MemoryChange),
    /// The session's default for its subagents changed (`None`: its own
    /// model).
    SetSubagentModel {
        command_id: String,
        model: Option<ModelSpec>,
    },
    /// The agent sent you a notification.
    Notice(c6_tools::Notice),
    /// A tool call held for approval, or what became of it.
    Approval(crate::policy::ApprovalEvent),
    /// Your answer for a held tool call.
    Decide {
        call_id: String,
        allow: bool,
    },
    /// A subagent's task from its parent: run it as given (no command
    /// expansion, mentions or prompt hooks) and say how its turn ended.
    /// Without a sender it joins the running turn (or starts one), and the
    /// report goes to the parent as a notice unless a call waits on it.
    Delegate(Box<PendingInput>, Option<tokio::sync::oneshot::Sender<Outcome>>, Option<crate::resource_pressure::Admission>),
    /// What a subagent did in a turn no call waited on, for its parent.
    ChildReport {
        command_id: String,
        text: String,
        wake: bool,
        task_notice: Option<c6_proto::TaskNotice>,
        inline: Option<InlineChildReport>,
        recorded: tokio::sync::oneshot::Sender<()>,
    },
    /// Replay a local report whose turn ended before delivery completed.
    RecoverReport,
    /// Context for the next input, never a reason to start a turn.
    /// How a remote subagent's turn ended (from the head, as a command).
    RemoteReport(String, Box<c6_proto::DelegationEnded>, tokio::sync::oneshot::Sender<Result<(), String>>),
    /// Deliver all held notices now, bypassing automatic holds (you asked).
    ReleaseNotices,
    /// Refresh the notice settings when you explicitly continue a session.
    NoticeLimits(c6_proto::NoticeLimits),
    /// Its turn `.0` was cut short by a worker restart: it goes on
    /// (docs/restarts.md §4).
    Resume(u64),
    /// Its turn was cut short by a journal write failure in this session.
    ResumeFailed(u64),
    /// From the log, on reload (docs/background.md §5): when notices
    /// started turns in the last hour, the ids of every notice recorded,
    /// and those no turn took, to deliver again.
    Reloaded {
        noticed: Vec<String>,
        pending: Vec<(PendingInput, bool)>,
    },
}

/// Only recently active sessions re-arm held notices at worker startup:
/// don't scan dormant logs or wake days-old conversations on upgrade.
const NOTICE_RECOVERY_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Why a rewind may not go back past a handoff (the head says the same).
const HANDOFF_REWIND: &str = "can only rewind to a message after the last switch between Claude Code and carbon6's agent: \
                              the other runtime's conversation cannot be taken back";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryAction {
    Retry,
    Restart,
}

/// Independent of the regular mailbox and the journal pool.
#[derive(Default)]
struct RecoveryGate {
    epoch: u64,
    /// A projection of the actor's timer, not independent work/liveness evidence.
    keep_warm_next_ms: std::sync::atomic::AtomicI64,
    has_turn: std::sync::atomic::AtomicBool,
    waiting_schedule: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicBool,
    adopting: std::sync::atomic::AtomicBool,
    activity: Arc<std::sync::atomic::AtomicI32>,
    disk_wait: Mutex<Option<String>>,
    probe: Mutex<ActorProbe>,
    request: Mutex<Option<RecoveryRequest>>,
}

const ACTOR_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct ActorProbe {
    pending: Option<(tokio::time::Instant, tokio::sync::oneshot::Receiver<()>)>,
}

impl ActorProbe {
    /// Never await an acknowledgment or enqueue a second outstanding probe.
    fn timed_out(&mut self, tx: &mpsc::UnboundedSender<ActorMsg>, now: tokio::time::Instant) -> bool {
        if let Some((sent, ack)) = &mut self.pending
            && matches!(ack.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty))
        {
            return now.saturating_duration_since(*sent) >= ACTOR_PROBE_TIMEOUT;
        }
        let (reply, ack) = tokio::sync::oneshot::channel();
        self.pending = tx.send(ActorMsg::HealthProbe(reply)).ok().map(|()| (now, ack));
        false
    }
}

struct RecoveryRequest {
    incident: String,
    ready: Option<tokio::sync::oneshot::Sender<RecoveryAction>>,
}

/// One escalation, across manual attempts. Drop closes the control lane and
/// tells the independent reporter to clear the head's live status.
struct RecoveryPause {
    id: String,
    gate: Arc<RecoveryGate>,
    reason: tokio::sync::watch::Sender<Option<String>>,
}

impl RecoveryPause {
    fn new(gate: Arc<RecoveryGate>, writer: &Writer, reason: &str) -> Self {
        warn!(session = %writer.session_id, %reason, "automatic session recovery exhausted");
        let id = uuid::Uuid::now_v7().to_string();
        *gate.request.lock().unwrap() = Some(RecoveryRequest { incident: id.clone(), ready: None });
        let (updated, state) = tokio::sync::watch::channel(Some(reason.to_owned()));
        tokio::spawn(report_recovery(
            writer.link.clone(),
            writer.session_id.clone(),
            gate.epoch,
            id.clone(),
            reason.to_owned(),
            state,
            false,
        ));
        Self { id, gate, reason: updated }
    }

    fn action_receiver(&self) -> tokio::sync::oneshot::Receiver<RecoveryAction> {
        let (ready, action) = tokio::sync::oneshot::channel();
        self.gate.request.lock().unwrap().as_mut().expect("this incident is active").ready = Some(ready);
        action
    }

    async fn wait_action(&self) -> RecoveryAction {
        self.action_receiver().await.expect("the gate retains its sender until a user acts")
    }

    fn update_reason(&self, reason: &str) {
        self.reason.send_if_modified(|old| {
            if old.as_deref() == Some(reason) {
                return false;
            }
            *old = Some(reason.to_owned());
            true
        });
    }
}

impl Drop for RecoveryPause {
    fn drop(&mut self) {
        let mut gate = self.gate.request.lock().unwrap();
        if gate.as_ref().is_some_and(|request| request.incident == self.id) {
            gate.take();
        }
        self.reason.send_replace(None);
    }
}

/// The disk wait has no action lane and is never a terminal recovery stop.
struct DiskWait {
    gate: Arc<RecoveryGate>,
    reason: tokio::sync::watch::Sender<Option<String>>,
}

impl DiskWait {
    fn new(gate: Arc<RecoveryGate>, writer: &Writer, reason: String) -> Self {
        *gate.disk_wait.lock().unwrap() = Some(reason.clone());
        let (updated, state) = tokio::sync::watch::channel(Some(reason.clone()));
        tokio::spawn(report_recovery(
            writer.link.clone(),
            writer.session_id.clone(),
            gate.epoch,
            uuid::Uuid::now_v7().to_string(),
            reason,
            state,
            true,
        ));
        Self { gate, reason: updated }
    }

    fn update(&self, space: &crate::disk::Space) {
        let reason = space.waiting_reason();
        *self.gate.disk_wait.lock().unwrap() = Some(reason.clone());
        self.reason.send_if_modified(|old| {
            if old.as_deref() == Some(&reason) {
                return false;
            }
            *old = Some(reason);
            true
        });
    }
}

impl Drop for DiskWait {
    fn drop(&mut self) {
        self.gate.disk_wait.lock().unwrap().take();
        self.reason.send_replace(None);
    }
}

async fn report_recovery(
    link: Arc<HeadLink>,
    session_id: String,
    epoch: u64,
    incident: String,
    initial_reason: String,
    mut state: tokio::sync::watch::Receiver<Option<String>>,
    automatic: bool,
) {
    const CLEAR_BUDGET: Duration = Duration::from_secs(60);
    // No journal writes, and never wait for the head in the supervisor.
    // Refresh after a reconnect/head restart; the head deduplicates reminders.
    let mut announced = false;
    let mut last_reason = initial_reason;
    let mut current = state.borrow_and_update().clone();
    let mut clear_deadline = None;
    loop {
        if let Some(reason) = &current {
            last_reason.clone_from(reason);
        } else {
            let deadline = clear_deadline.get_or_insert_with(|| tokio::time::Instant::now() + CLEAR_BUDGET);
            if tokio::time::Instant::now() >= *deadline {
                return;
            }
        }
        // Even if the actor recovered, don't displace an unacknowledged
        // warning with a clear: the head may not have queued the parent yet.
        let reason = (current.is_some() || !announced).then_some(&last_reason);
        let params = serde_json::json!({"session_id": session_id, "ownership_epoch": epoch, "incident": incident, "reason": reason, "automatic": automatic});
        let sent_clear = reason.is_none();
        let call = async { link.is_connected() && link.call("session.recovery_status", params).await.is_ok() };
        tokio::pin!(call);
        let mut changed_in_flight = false;
        let acked = loop {
            if let Some(deadline) = clear_deadline {
                // Preserve the in-flight warning if it can still be
                // acknowledged. The same deadline bounds every later retry.
                break tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => return,
                    acked = &mut call => acked,
                };
            }
            tokio::select! {
                biased;
                changed = state.changed() => {
                    current = changed.ok().and_then(|()| state.borrow_and_update().clone());
                    changed_in_flight = true;
                    if let Some(reason) = &current {
                        last_reason.clone_from(reason);
                    } else {
                        clear_deadline = Some(tokio::time::Instant::now() + CLEAR_BUDGET);
                    }
                }
                acked = &mut call => break acked,
            }
        };
        if acked {
            if sent_clear {
                return;
            }
            announced = true;
            if current.is_none() {
                continue; // acknowledge then send the clear immediately
            }
        }
        if changed_in_flight {
            continue;
        }
        if let Some(deadline) = clear_deadline {
            tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_secs(5))).await;
        } else {
            tokio::select! {
                biased;
                changed = state.changed() => current = changed.ok().and_then(|()| state.borrow_and_update().clone()),
                _ = tokio::time::sleep(Duration::from_secs(10)) => {},
            }
        }
    }
}

/// Honor the deadline even in an ongoing attempt, but never drop the attempt
/// halfway through an owned stop or a journal/in-memory state transition.
async fn recovery_at_deadline<T>(
    deadline: tokio::time::Instant,
    attempt: impl std::future::Future<Output = T>,
    escalate: impl FnOnce(),
) -> T {
    tokio::pin!(attempt);
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => {
            escalate();
            attempt.await
        }
        result = &mut attempt => result,
    }
}

fn recovery_budget(seconds: u32) -> Duration {
    Duration::from_secs(u64::from(if seconds == 0 { c6_api::DEFAULT_SESSION_RECOVERY_SECONDS } else { seconds }))
}

fn next_recovery_backoff(backoff: Duration) -> Duration {
    (backoff * 2).clamp(Duration::from_millis(100), Duration::from_secs(30))
}

/// Adoption has not lost a held reader's offset, so it retries the ownership
/// check, not journal recovery's process retirement. Between attempts the
/// mailbox stays usable for take-back, callbacks and explicit Rewind.
struct HeldAdoptionRecovery {
    gate: Arc<RecoveryGate>,
    budget: Duration,
    deadline: Option<tokio::time::Instant>,
    retry_at: Option<tokio::time::Instant>,
    backoff: Duration,
    disk_delays: (Duration, Duration),
    disk_wait: Option<DiskWait>,
    disk_retry_at: Option<tokio::time::Instant>,
    disk_was_full: bool,
    reason: String,
    reported_failure: bool,
    pause: Option<RecoveryPause>,
    action: Option<tokio::sync::oneshot::Receiver<RecoveryAction>>,
}

impl HeldAdoptionRecovery {
    fn new(gate: Arc<RecoveryGate>, budget: Duration, disk_delays: (Duration, Duration)) -> Self {
        let now = tokio::time::Instant::now();
        gate.adopting.store(true, Ordering::Release);
        Self {
            gate,
            budget,
            deadline: Some(now + budget),
            retry_at: Some(now),
            backoff: Duration::ZERO,
            disk_delays,
            disk_wait: None,
            disk_retry_at: None,
            disk_was_full: false,
            reason: "The worker has not finished checking ownership of the held Claude Code runtime; queued messages are held".into(),
            reported_failure: false,
            pause: None,
            action: None,
        }
    }

    async fn check(&mut self, writer: &Writer, attempt: impl std::future::Future<Output = anyhow::Result<()>>) -> bool {
        let now = tokio::time::Instant::now();
        if let Some(waiting) = &self.disk_wait {
            if let Some(space) = writer.disk_probe.as_ref().and_then(|(disk, path)| disk.space(path)) {
                waiting.update(&space);
                if self.disk_was_full && space.free >= crate::disk::RESUME_BYTES {
                    self.disk_retry_at = Some(now);
                }
                self.disk_was_full = space.free < crate::disk::RESUME_BYTES;
            }
            let retry = self.disk_retry_at.expect("disk waits retain an attempt deadline");
            if now < retry {
                // Keep the mailbox usable and sample on its timer without
                // spending the exponential attempt delay on every tick.
                self.retry_at = Some(retry.min(now + Duration::from_secs(1)));
                return false;
            }
            self.retry_at = Some(now);
        }
        if self.deadline.is_some_and(|until| now >= until) {
            self.deadline = None;
            self.retry_at = None;
            self.pause.get_or_insert_with(|| RecoveryPause::new(self.gate.clone(), writer, &self.reason));
        }
        if self.retry_at.is_some_and(|at| now >= at) {
            self.retry_at = None;
            let result = if let Some(until) = self.deadline {
                recovery_at_deadline(until, attempt, || {
                    self.pause.get_or_insert_with(|| RecoveryPause::new(self.gate.clone(), writer, &self.reason));
                })
                .await
            } else {
                attempt.await
            };
            let Err(e) = result else { return true };
            let space = if let Some((disk, path)) = &writer.disk_probe { disk.fresh_space(path).await } else { None };
            if crate::disk::resource_error(&e, space.as_ref()) {
                // A journal read can fail before adoption advances any held
                // reader. Wait here; do not retire that untouched process.
                self.pause.take();
                self.action.take();
                self.deadline = None;
                if self.disk_wait.is_none() {
                    let reason = space.as_ref().map_or_else(
                        || "Waiting for disk space: free space unavailable. Recovery continues automatically; no action needed.".into(),
                        crate::disk::Space::waiting_reason,
                    );
                    self.disk_wait = Some(DiskWait::new(self.gate.clone(), writer, reason));
                    self.backoff = self.disk_delays.0;
                }
                self.disk_was_full = space.is_some_and(|s| s.free < crate::disk::RESUME_BYTES);
                let now = tokio::time::Instant::now();
                self.disk_retry_at = Some(now + self.backoff);
                self.retry_at = Some(now + self.backoff.min(Duration::from_secs(1)));
                self.backoff = self.backoff.saturating_mul(2).min(self.disk_delays.1);
                return false;
            }
            if self.disk_wait.take().is_some() {
                self.deadline = Some(tokio::time::Instant::now() + self.budget);
                self.disk_retry_at = None;
                self.backoff = Duration::ZERO;
            }
            let reason: String =
                format!("Claude Code could not be safely taken up; queued messages were not sent: {e:#}").chars().take(1000).collect();
            if reason != self.reason {
                warn!(session = %writer.session_id, %reason);
                if let Some(pause) = &self.pause {
                    pause.update_reason(&reason);
                }
                self.reason = reason;
            }
            if !self.reported_failure {
                // Report the first refusal immediately, without turning
                // retries into durable progress or rearming their deadline.
                self.reported_failure = true;
                let append = writer.append(session_event::Kind::Error(ErrorEvent { message: self.reason.clone() }));
                let recorded = if let Some(until) = self.deadline {
                    recovery_at_deadline(until, append, || {
                        self.pause.get_or_insert_with(|| RecoveryPause::new(self.gate.clone(), writer, &self.reason));
                    })
                    .await
                } else {
                    append.await
                };
                if let Err(e) = recorded {
                    // A failed diagnostic does not authorize retirement of a
                    // held runtime or erase the original ownership/corruption
                    // failure. Recovery status can report without SQLite.
                    writer.resource_failure.store(false, Ordering::Release);
                    warn!(session = %writer.session_id, "recording Claude Code adoption failure: {e:#}");
                }
            }
            if let Some(until) = self.deadline {
                let now = tokio::time::Instant::now();
                if now >= until {
                    self.deadline = None;
                    self.pause.get_or_insert_with(|| RecoveryPause::new(self.gate.clone(), writer, &self.reason));
                } else {
                    self.backoff = next_recovery_backoff(self.backoff);
                    self.retry_at = Some(until.min(now + self.backoff));
                }
            }
        }
        if self.deadline.is_none() && self.action.is_none() {
            self.action = Some(self.pause.as_ref().expect("manual action requires an escalation").action_receiver());
        }
        false
    }

    fn resume(&mut self, action: RecoveryAction, budget: Duration) {
        self.action = None;
        let now = tokio::time::Instant::now();
        if action == RecoveryAction::Restart {
            self.deadline = Some(now + budget);
            self.backoff = Duration::ZERO;
        }
        self.retry_at = Some(now);
    }
}

impl Drop for HeldAdoptionRecovery {
    fn drop(&mut self) {
        if self.gate.adopting.load(Ordering::Acquire) {
            self.gate.probe.lock().unwrap().pending = None;
            self.gate.adopting.store(false, Ordering::Release);
        }
    }
}

fn background_liveness_counts(tasks: &[c6_tools::TaskInfo]) -> (u32, u32) {
    let mut total = 0;
    let mut processes = 0;
    for task in tasks.iter().filter(|t| t.running && crate::subagent::counted_background(t.notify, t.kind.as_str())) {
        total += 1;
        if task.kind != c6_tools::TaskKind::Agent {
            processes += 1;
        }
    }
    (total, processes)
}

fn liveness_activity(state: c6_proto::SessionActorState, activity: i32) -> (i32, bool) {
    use c6_proto::{SessionActivityKind, SessionActorState};
    if !matches!(state, SessionActorState::Running | SessionActorState::Unresponsive) {
        return (SessionActivityKind::Unspecified as i32, false);
    }
    if activity == SessionActivityKind::Subagent as i32 {
        // Older heads know Tool and must not classify dependency waits as a
        // stalled model. New heads use the additive flag for tree silence.
        (SessionActivityKind::Tool as i32, true)
    } else {
        (activity, false)
    }
}

impl SessionManager {
    /// Constructor for in-process tests that require deterministic measurements.
    #[doc(hidden)]
    pub fn new_with_resource_sampler(
        journal: Arc<Journal>,
        link: Arc<HeadLink>,
        worktrees: PathBuf,
        sample: impl Fn() -> crate::ResourceMeasurement + Send + Sync + 'static,
    ) -> Self {
        let mut manager = Self::new(journal, link, worktrees);
        manager.launch_pressure = Arc::new(crate::resource_pressure::Gate::new(sample));
        manager
    }

    pub fn new(journal: Arc<Journal>, link: Arc<HeadLink>, worktrees: PathBuf) -> Self {
        let state_dir = worktrees.parent().map(Path::to_path_buf).unwrap_or_else(|| worktrees.clone());
        prune_browser_profiles(&state_dir.join("browser"), std::time::Duration::from_secs(30 * 24 * 3600));
        Self {
            journal,
            link,
            worktrees,
            state_dir,
            actors: Arc::default(),
            loads: Arc::default(),
            working: Arc::default(),
            disk: Arc::default(),
            launch_pressure: if cfg!(test) {
                Arc::new(crate::resource_pressure::Gate::new(crate::ResourceMeasurement::default))
            } else {
                Arc::default()
            },
            bridge: Arc::default(),
            remote_waits: Arc::default(),
        }
    }

    /// Cached filesystem measurements, independent of the journal connection.
    /// Monitor schedules bounded background probes; this also runs from the
    /// connection's ACK loop and must never perform filesystem I/O inline.
    pub(crate) fn report_disk_pressure(&self) {
        let paths = [self.state_dir.clone(), self.worktrees.clone(), std::env::temp_dir()];
        for (space, active) in self.disk.warnings(&paths) {
            // Clears rearm locally; the head only routes active notices. A
            // failed, delayed clear must not suppress a newer low-space edge.
            if !active {
                continue;
            }
            let link = self.link.clone();
            let disk = self.disk.clone();
            tokio::spawn(async move {
                let params =
                    serde_json::json!({"mount": space.mount, "free_bytes": space.free, "total_bytes": space.total, "active": active});
                if let Err(error) = link.call("disk.pressure", params).await {
                    disk.retry_warning(&space.mount);
                    warn!(%error, "could not report disk pressure");
                }
            });
        }
    }

    pub(crate) fn recovery_action(&self, session: &str, epoch: u64, incident: &str, action: &str) -> anyhow::Result<()> {
        let action = match action {
            "retry" => RecoveryAction::Retry,
            "restart" => RecoveryAction::Restart,
            _ => anyhow::bail!("unknown recovery action"),
        };
        let actors = self.actors.lock().unwrap();
        let handle = actors.get(session).filter(|h| !h.tx.is_closed() && h.recovery.epoch == epoch);
        let handle = handle.ok_or_else(|| anyhow::anyhow!("this session's recovery is no longer active"))?;
        let mut request = handle.recovery.request.lock().unwrap();
        let request = request
            .as_mut()
            .filter(|request| request.incident == incident)
            .ok_or_else(|| anyhow::anyhow!("this session's recovery is no longer waiting for an action"))?;
        request
            .ready
            .take()
            .ok_or_else(|| anyhow::anyhow!("a recovery attempt is still in progress"))?
            .send(action)
            .map_err(|_| anyhow::anyhow!("this session's recovery is no longer active"))
    }

    /// Where the outcome of the remote subagent turn that `session`'s call
    /// `call_id` waits on will arrive.
    pub(crate) fn wait_remote(&self, session: &str, call_id: &str) -> tokio::sync::oneshot::Receiver<c6_proto::DelegationEnded> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.remote_waits.lock().unwrap().insert(format!("{session}/{call_id}"), tx);
        rx
    }

    /// The call waiting on it, if one still does (taken: the outcome goes
    /// there once).
    pub(crate) fn take_remote_wait(&self, session: &str, call_id: &str) -> Option<tokio::sync::oneshot::Sender<c6_proto::DelegationEnded>> {
        self.remote_waits.lock().unwrap().remove(&format!("{session}/{call_id}"))
    }

    /// The MCP server for Claude Code sessions' subagents, started on first
    /// use.
    async fn bridge(&self) -> Option<Arc<crate::mcp_bridge::Bridge>> {
        self.bridge
            .get_or_init(|| async {
                // On the port it had before a restart: held Claude Code
                // processes still call it there (docs/restarts.md §3).
                let file = self.state_dir.join("mcp-bridge-port");
                let port = std::fs::read_to_string(&file).ok().and_then(|p| p.trim().parse().ok());
                crate::mcp_bridge::Bridge::start_at(port)
                    .await
                    .inspect(|b| {
                        let _ = std::fs::write(&file, b.port().to_string());
                    })
                    .map(Arc::new)
                    .inspect_err(|e| warn!("the MCP bridge for Claude Code did not start: {e:#}"))
                    .ok()
            })
            .await
            .clone()
    }

    /// A non-destructive task snapshot. Do not load or wake an idle session
    /// merely to inspect it: restarted sessions no longer have live buffers.
    pub(crate) fn task_snapshot(&self, session_id: &str, task_id: &str) -> Option<serde_json::Value> {
        let tools = self.actors.lock().unwrap().get(session_id)?.tools.clone();
        let snapshot = tools.background.snapshot(task_id)?;
        serde_json::to_value(snapshot).ok()
    }

    /// Whether a session's live actor holds this input in its inbox, not
    /// yet taken: what a cancel needs. Until then a cancel finds it on its
    /// way from the head (`pending`), and tests wait on this, not on time.
    pub fn inbox_holds(&self, session_id: &str, command_id: &str) -> bool {
        let actors = self.actors.lock().unwrap();
        actors
            .get(session_id)
            .filter(|a| !a.tx.is_closed())
            .is_some_and(|a| a.inbox.lock().unwrap().queue.iter().any(|(i, _)| i.command_id == command_id))
    }

    /// Session ids with a live actor.
    pub fn active(&self) -> Vec<String> {
        self.actors.lock().unwrap().iter().filter(|(_, a)| !a.tx.is_closed()).map(|(id, _)| id.clone()).collect()
    }

    pub(crate) fn journal(&self) -> &Arc<Journal> {
        &self.journal
    }

    /// Counts a delegation in `root`'s tree, if fewer than `max` run; zero is unlimited.
    pub(crate) fn working(&self, root: &str, max: usize) -> Option<Working> {
        let mut c = self.working.lock().unwrap();
        let n = c.entry(root.to_owned()).or_default();
        if max != 0 && *n >= max {
            return None;
        }
        *n += 1;
        Some(Working { counts: self.working.clone(), root: root.to_owned() })
    }

    /// What a restart now would cut short, per session: a turn under way,
    /// or background work whose end is awaited (shell tasks, subagents,
    /// Claude Code's own). Servers and watchers started not to be waited
    /// for (`notify: false`) don't count (docs/restarts.md §2).
    pub fn holding(&self) -> Vec<(String, String)> {
        let actors: Vec<(String, Handle)> =
            self.actors.lock().unwrap().iter().filter(|(_, a)| !a.tx.is_closed()).map(|(id, a)| (id.clone(), a.clone())).collect();
        let mut out = Vec::new();
        for (id, a) in actors {
            if a.running.load(std::sync::atomic::Ordering::SeqCst) {
                out.push((id.clone(), "a turn".to_owned()));
            }
            for t in a.tools.background.list().into_iter().filter(|t| t.running && t.notify) {
                let what: String = t.description.chars().take(80).collect();
                out.push((id.clone(), format!("{}: {what}", t.kind.as_str())));
            }
        }
        out
    }

    /// How many sessions are in the middle of a turn.
    pub fn running(&self) -> usize {
        self.actors.lock().unwrap().values().filter(|a| !a.tx.is_closed() && a.running.load(std::sync::atomic::Ordering::SeqCst)).count()
    }

    /// Snapshot the worker's actors without waiting on the journal or actor
    /// mailboxes. A nonblocking mailbox probe detects a stuck run loop.
    pub(crate) fn liveness_report(&self) -> c6_proto::SessionLivenessReport {
        use c6_proto::{SessionActorState, SessionLiveness};

        let actors = self.actors.lock().unwrap().clone();
        let held = if actors.values().any(|a| a.claude_code.load(Ordering::Acquire)) {
            crate::claude_code::held_liveness(&self.claude_dir().join("held"))
        } else {
            HashMap::new()
        };
        let now = tokio::time::Instant::now();
        let sessions = actors
            .into_iter()
            .map(|(session_id, a)| {
                let disk_wait = a.recovery.disk_wait.lock().unwrap().clone();
                let (state, reason) = if a.tx.is_closed() {
                    (SessionActorState::Stopped, "The worker's session actor exited")
                } else if let Some(reason) = disk_wait.as_deref() {
                    (SessionActorState::WaitingForDisk, reason)
                } else if a.recovery.active.load(Ordering::Acquire) {
                    (SessionActorState::Recovering, "The worker is recovering the session journal")
                } else if a.recovery.adopting.load(Ordering::Acquire) {
                    (SessionActorState::Recovering, "The worker cannot yet confirm ownership of the held Claude Code runtime")
                } else if a.recovery.probe.lock().unwrap().timed_out(&a.tx, now) {
                    (SessionActorState::Unresponsive, "The session actor has not answered its mailbox probe for 30 seconds")
                } else if a.running.load(Ordering::SeqCst) {
                    (SessionActorState::Running, "")
                } else {
                    (SessionActorState::Idle, "")
                };
                let held_process = if a.claude_code.load(Ordering::Acquire) { held.get(&session_id).copied() } else { None };
                let (running_background_tasks, running_background_processes) = background_liveness_counts(&a.tools.background.list());
                let (activity_kind, waiting_on_subagents) = liveness_activity(state, a.recovery.activity.load(Ordering::Relaxed));
                SessionLiveness {
                    session_id,
                    ownership_epoch: a.recovery.epoch,
                    actor_state: state as i32,
                    reason: reason.into(),
                    activity_kind,
                    waiting_on_subagents,
                    running_background_tasks,
                    running_background_processes,
                    held_process: held_process.unwrap_or_default() as i32,
                    waiting_on: if state != SessionActorState::Idle {
                        ""
                    } else if running_background_tasks > 0 {
                        "background"
                    } else if a.recovery.waiting_schedule.load(Ordering::Relaxed) {
                        "schedule"
                    } else if !a.recovery.has_turn.load(Ordering::Relaxed) {
                        "nothing"
                    } else {
                        "user"
                    }
                    .into(),
                    keep_warm_next_ms: if state == SessionActorState::Idle {
                        a.recovery.keep_warm_next_ms.load(Ordering::Relaxed)
                    } else {
                        0
                    },
                }
            })
            .collect();
        c6_proto::SessionLivenessReport { sessions }
    }

    /// Whether the session's actor is running a turn.
    pub(crate) fn is_running(&self, session_id: &str) -> bool {
        self.actors
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|a| !a.tx.is_closed() && a.running.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Reuse the ordinary durable Notice event and head notification routing;
    /// never send an ephemeral telemetry message as the only cleanup notice.
    pub(crate) async fn build_cleanup_notice(&self, sessions: &[String], message: String) -> anyhow::Result<()> {
        for session in sessions {
            if let Some(actor) = self.actor(session).await? {
                actor
                    .send(ActorMsg::Notice(c6_tools::Notice {
                        title: "Build folders cleaned under disk pressure".into(),
                        message,
                        priority: "normal".into(),
                    }))
                    .map_err(|_| anyhow::anyhow!("session stopped before receiving cleanup notice"))?;
                return Ok(());
            }
        }
        anyhow::bail!("cleanup notice has no retained local session")
    }

    /// The session's actor, started from the journal if needed (boxed: an
    /// actor reaches others, whose actors it may start).
    pub(crate) fn actor_for<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, anyhow::Result<Option<mpsc::UnboundedSender<ActorMsg>>>> {
        Box::pin(self.actor(session_id))
    }

    fn live(&self, session_id: &str) -> Option<mpsc::UnboundedSender<ActorMsg>> {
        self.actors.lock().unwrap().get(session_id).map(|a| a.tx.clone())
    }

    /// Edit or cancel one of a session's queued inputs, if its actor is
    /// live and the input is still waiting in its inbox. `input` is the
    /// edited payload (for an edit; `None` cancels).
    pub(crate) fn queue_op(&self, session_id: &str, command_id: &str, input: Option<PendingInput>) -> QueueOp {
        let actors = self.actors.lock().unwrap();
        let Some(h) = actors.get(session_id).filter(|a| !a.tx.is_closed()) else {
            return QueueOp::Absent;
        };
        let mut inbox = h.inbox.lock().unwrap();
        match input {
            Some(i) => inbox.replace(command_id, i),
            None => inbox.remove(command_id),
        }
    }

    /// Where running Claude Code processes are recorded.
    pub(crate) fn claude_dir(&self) -> PathBuf {
        self.state_dir.join("claude")
    }

    /// Raw history is authoritative for runtime/placement, even across rewind.
    pub(crate) async fn move_snapshot(&self, session: &str) -> anyhow::Result<(CreateSession, Vec<SessionEvent>, SessionStarted)> {
        self.journal.check_move_history_size(session, crate::move_files::MAX_BYTES).await?;
        let (mut spec, events) =
            self.journal.load_session(session).await?.ok_or_else(|| anyhow::anyhow!("session is not cached on this worker"))?;
        if let Some(model) = events.iter().rev().find_map(|event| match &event.kind {
            Some(session_event::Kind::ModelSwitched(switched)) => switched.model.clone(),
            _ => None,
        }) {
            spec.model = Some(model);
        }
        let started = events
            .iter()
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::Started(s)) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("session has no starting event"))?;
        let runtime = runtime_of(started, &events);
        Ok((spec, events, runtime))
    }

    pub(crate) async fn freeze_for_move(&self, session: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!self.journal.move_aborted(session, epoch, move_id).await?, "this move was aborted");
        let (owned, blocked, previous) = self.journal.ownership(session).await?;
        anyhow::ensure!(owned == epoch, "stale session ownership generation");
        if blocked {
            anyhow::ensure!(previous == move_id, "session is fenced by another move");
            return Ok(());
        }
        let actor = self.actor(session).await?.ok_or_else(|| anyhow::anyhow!("unknown session on source worker"))?;
        let (reply, result) = tokio::sync::oneshot::channel();
        actor
            .send(ActorMsg::FreezeMove { move_id: move_id.to_owned(), epoch, reply })
            .map_err(|_| anyhow::anyhow!("session stopped before releasing ownership"))?;
        result.await.map_err(|_| anyhow::anyhow!("source release was interrupted"))?.map_err(anyhow::Error::msg)?;
        let mut actors = self.actors.lock().unwrap();
        if actors.get(session).is_some_and(|handle| handle.tx.same_channel(&actor)) {
            actors.remove(session);
        }
        Ok(())
    }

    pub(crate) async fn assert_move_inactive(&self, session: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.actors.lock().unwrap().get(session).is_some_and(|a| !a.tx.is_closed()),
            "destination already has a running actor for this session"
        );
        Ok(())
    }

    fn load_gate(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.loads.lock().unwrap().entry(session.to_owned()).or_default().clone()
    }

    pub(crate) async fn import_move(
        &self,
        session: &str,
        spec: &CreateSession,
        events: &[SessionEvent],
        epoch: u64,
        move_id: &str,
    ) -> anyhow::Result<()> {
        let _loading = self.load_gate(session).lock_owned().await;
        self.assert_move_inactive(session).await?;
        self.journal.import_owned(session, spec, events, epoch, move_id).await
    }

    pub(crate) async fn activate_move(&self, session: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.actor(session).await?.is_some(), "adopted session history is missing");
        Ok(())
    }

    /// Probe the same CLI the destination would launch, without opening a
    /// conversation or touching the worker's Claude configuration.
    pub(crate) async fn check_native_move_runtime() -> anyhow::Result<()> {
        use anyhow::Context as _;
        use std::process::Stdio;

        let bin = crate::claude_code::find_claude()
            .context("destination Claude Code CLI is unavailable: install `claude` on the destination worker or set C6_CLAUDE_BIN")?;
        let config = tempfile::tempdir().context("create temporary Claude CLI probe configuration")?;
        let mut command = tokio::process::Command::new(&bin);
        command
            .arg("--version")
            .env("CLAUDE_CONFIG_DIR", config.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), command.status())
            .await
            .context("destination Claude Code CLI is unavailable: `claude --version` timed out")?
            .with_context(|| format!("destination Claude Code CLI is unavailable: cannot run {} --version", bin.display()))?;
        anyhow::ensure!(status.success(), "destination Claude Code CLI is unavailable: `claude --version` ended with {status}");
        Ok(())
    }

    pub(crate) async fn prepare_move_event(
        &self,
        session: &str,
        spec: &CreateSession,
        events: &[SessionEvent],
        mut change: c6_proto::PlaceChanged,
    ) -> anyhow::Result<SessionEvent> {
        let _build_use = self.journal.build_folder_lifecycle.clone().read_owned().await;
        let from = change.from.as_ref().ok_or_else(|| anyhow::anyhow!("move has no source place"))?;
        let to = change.to.as_ref().ok_or_else(|| anyhow::anyhow!("move has no destination place"))?;
        let cwd = Path::new(&to.cwd);
        let (system_prompt, tools, tool_profile) = if events.iter().rev().find_map(|e| match &e.kind {
            Some(session_event::Kind::ModelSwitched(h)) => h.handoff.as_ref().map(|h| h.tool_profile.as_str()),
            Some(session_event::Kind::Started(s)) => Some(s.tool_profile.as_str()),
            _ => None,
        }) == Some(crate::claude_code::PROFILE)
        {
            Self::check_native_move_runtime().await?;
            (crate::claude_code::SYSTEM_PROMPT.to_owned(), Vec::new(), crate::claude_code::PROFILE.to_owned())
        } else {
            let mut preparing = spec.clone();
            preparing.hooks_json.clear();
            // Preparing an inactive destination cannot borrow the old owner's
            // credentials or run arbitrary external hooks/MCP servers.
            preparing.secrets_json = "[]".into();
            let home = dirs::home_dir().unwrap_or_default().display().to_string();
            let labels = session_labels(&spec.labels, &to.repo, &to.cwd, &home, &spawn_model(spec), "");
            let date = time::OffsetDateTime::now_utc().date().to_string();
            let base = build_system_prompt(&spec.base_prompt, cwd, &date);
            let built = own_runtime(&self.link, session, &preparing, cwd, &labels, base, "resume").await;
            anyhow::ensure!(built.errors.is_empty(), "destination tool preparation failed: {}", built.errors.join("; "));
            (built.prompt, built.tools.iter().map(|t| t.spec().name).collect(), built.profile.as_str().to_owned())
        };
        change.notice = change
            .notice
            .replace("{{from}}", &format!("{} on {}", from.cwd, from.worker_id))
            .replace("{{to}}", &format!("{} on {}", to.cwd, to.worker_id));
        change.system_prompt = system_prompt;
        change.tools = tools;
        change.tool_profile = tool_profile;
        Ok(SessionEvent {
            seq: events.last().map_or(1, |e| e.seq + 1),
            ts_ms: c6_core::now_ms(),
            kind: Some(session_event::Kind::PlaceChanged(change)),
        })
    }

    /// Queue edits bypass the actor's normal input expansion path. Prepare
    /// their files before acquiring the inbox lock; immutable filenames keep
    /// an edit racing delivery from changing a file already given to the agent.
    pub(crate) async fn prepare_attachments(&self, session_id: &str, input: &mut PendingInput) -> anyhow::Result<()> {
        let remote = self.actors.lock().unwrap().get(session_id).is_some_and(|h| h.tools.ssh().is_some());
        crate::attachments::prepare(&self.state_dir, session_id, input, remote).await
    }

    /// Only for a completed public rewind whose selected branch has no held
    /// record. Unselected runtimes may be stopped, never adopted. Publication
    /// of a new untouched intent must not race this destructive cleanup.
    async fn stop_unmapped_claude(&self, session: &str, events: &[SessionEvent]) -> anyhow::Result<()> {
        let handoff = events.iter().rev().find(|e| handoff_of(e).is_some()).map_or(0, |e| e.seq);
        let rewound = events.iter().rev().find(|e| matches!(e.kind, Some(session_event::Kind::Rewound(_)))).map_or(0, |e| e.seq);
        if rewound <= handoff {
            return Ok(());
        }
        let _ownership = self.journal.claude_rewind_ownership.lock().await;
        // Branch selection can precede a newly published pending intent.
        // Recheck under the same gate as publication, before any signals.
        let reset = self.journal.claude_rewind(session).await?;
        anyhow::ensure!(!matches!(&reset, Some((_, 0))), "Claude Code rewind is not confirmed; retry Rewind before sending more input");
        if !matches!(reset, Some((_, applied)) if applied > handoff && applied >= rewound) {
            // Reconstruction of completed history, not reset initialization
            // or retry. Cleanup performs journal writes, but never prepares
            // a reset and never holds a database transaction across waits.
            crate::claude_code::stop_held_for_rewind(&self.claude_dir().join("held"), session, &self.journal).await?;
        }
        Ok(())
    }

    /// Closes turns left open by a crash or kill.
    pub async fn recover(&self) -> anyhow::Result<()> {
        // Claude Code processes the previous worker left go first: their
        // sessions resume their conversations in processes of their own.
        crate::claude_code::sweep_orphans(&self.claude_dir()).await;
        let mut cut_short = Vec::new();
        for (session_id, last_seq, turn) in self.journal.open_turns().await? {
            let epoch = self.journal.ownership(&session_id).await?.0;
            warn!(session = %session_id, turn, "closing turn interrupted by worker restart");
            // A subagent the turn was waiting on: its call gets what it did.
            let mut seq = last_seq;
            let lost = self.lost_delegations(&session_id).await?;
            // A turn that waited on a subagent working elsewhere is woken by
            // its report; the others go on now (docs/restarts.md §4).
            if !lost.iter().any(|r| r.output.contains("status=\"running\" on=")) {
                cut_short.push((session_id.clone(), turn));
            }
            for result in lost {
                seq += 1;
                let ev = SessionEvent { seq, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::ToolResult(result)) };
                self.journal.append_owned(&session_id, &ev, epoch).await?;
            }
            let ev = SessionEvent {
                seq: seq + 1,
                ts_ms: c6_core::now_ms(),
                kind: Some(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::WorkerRestarted as i32 })),
            };
            self.journal.append_owned(&session_id, &ev, epoch).await?;
        }
        // Turns the restart cut short go on (docs/restarts.md §4): the agent
        // carries on from its log. (One whose Claude Code process was held
        // carries on by itself.)
        for (session_id, turn) in cut_short {
            let events = self.journal.events_after(&session_id, 0, u32::MAX).await?;
            let pending = matches!(self.journal.claude_rewind(&session_id).await?, Some((_, 0)));
            if resume_superseded(&events, turn, pending) {
                continue;
            }
            match self.load_with_task_loss(&session_id, TaskLossCause::WorkerRestart).await {
                Ok(Some(handle)) => {
                    let _ = handle.tx.send(ActorMsg::Resume(turn));
                }
                Ok(None) => {}
                Err(e) => warn!(session = %session_id, "resuming its turn: {e:#}"),
            }
        }
        // Claude Code processes the previous worker held went on: their
        // sessions take them up (docs/restarts.md §3).
        for session_id in crate::claude_code::Process::held_sessions(&self.claude_dir().join("held")) {
            if let Err(e) = self.load_with_task_loss(&session_id, TaskLossCause::WorkerRestart).await {
                warn!(session = %session_id, "taking up its Claude Code process: {e:#}");
            }
        }
        // Background tasks died with the previous process: resuming those
        // sessions reports them as lost.
        for session_id in self.journal.sessions_with_open_tasks().await? {
            if let Err(e) = self.load_with_task_loss(&session_id, TaskLossCause::WorkerRestart).await {
                warn!(session = %session_id, "resuming for lost background tasks: {e:#}");
            }
        }
        // An idle session with only held notices has no open task to load
        // it above. Restore its actor too, so its hourly timer is armed
        // without waiting for another command from the head. Only scan
        // recently active sessions; dormant notices wait for explicit use.
        for session_id in self.journal.recent_unblocked_session_ids(c6_core::now_ms() - NOTICE_RECOVERY_WINDOW_MS).await? {
            let events = self.journal.events_after(&session_id, 0, u32::MAX).await?;
            if notices_of(&events).2.iter().any(|(_, wake)| *wake)
                && let Err(e) = self.actor(&session_id).await
            {
                warn!(session = %session_id, "resuming for held notices: {e:#}");
            }
        }
        // Report obligations do not expire while the worker is offline.
        // The tracking boundary excludes pre-upgrade, already-told history.
        for session_id in self.journal.local_subagent_ids().await? {
            let Some((spec, events)) = self.journal.load_session(&session_id).await? else { continue };
            let Some(delegation) = spec.delegation else { continue };
            let pending = pending_local_reports(&session_id, &events);
            if pending.is_empty() {
                continue;
            }
            let parent = self.journal.events_after(&delegation.parent_session_id, 0, u32::MAX).await?;
            // Do not recreate tools/MCP processes for completed children
            // whose parents already durably recorded the report. An actor
            // is needed only for an actual undelivered obligation.
            let undelivered = pending.iter().any(|(id, end, inputs)| {
                let outcome = local_report_outcome(&events[..=*end]);
                let status = crate::subagent::status_of(outcome.reason, &outcome.report);
                !inputs.iter().any(|u| LocalReportCall::of(u).is_some_and(|call| call.recorded(&parent, &session_id, status)))
                    && !parent.iter().any(|e| {
                        matches!(&e.kind,
                        Some(session_event::Kind::NoticeQueued(n)) if &n.command_id == id)
                    })
            });
            if undelivered && let Err(e) = self.actor(&session_id).await {
                warn!(session = %session_id, "resuming for a pending subagent report: {e:#}");
            }
        }
        Ok(())
    }

    /// Results for the calls of a session's open turn that wait on a
    /// subagent (foreground, no result yet): the subagent's digest, lost.
    async fn lost_delegations(&self, session_id: &str) -> anyhow::Result<Vec<c6_proto::ToolResult>> {
        let Some((spec, events)) = self.journal.load_session(session_id).await? else { return Ok(Vec::new()) };
        let answered: HashSet<&str> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::ToolResult(r)) => Some(r.call_id.as_str()),
                _ => None,
            })
            .collect();
        let tool_of = |call_id: &str| {
            events.iter().find_map(|e| match &e.kind {
                Some(session_event::Kind::Assistant(a)) => a.blocks.iter().find_map(|b| match &b.block {
                    Some(c6_proto::content_block::Block::ToolUse(u)) if u.id == call_id => Some(u.name.clone()),
                    _ => None,
                }),
                _ => None,
            })
        };
        let mut out = Vec::new();
        for e in &events {
            if let Some(session_event::Kind::SubagentSpawned(sp)) = &e.kind
                && sp.background_task_id.is_empty()
                && !answered.contains(sp.call_id.as_str())
            {
                // Elsewhere, it keeps working; its report comes as a notice.
                if let Some(r) = &sp.remote {
                    let note = spec_prompt(&spec, "session.subagent_remote_reattach", c6_agent::SUBAGENT_REMOTE_REATTACH);
                    out.push(c6_proto::ToolResult {
                        call_id: sp.call_id.clone(),
                        tool: tool_of(&sp.call_id).unwrap_or_else(|| "Task".into()),
                        output: format!("<subagent id=\"{}\" status=\"running\" on=\"{}\"/>\n{note}", sp.child_session_id, r.worker_name),
                        ..Default::default()
                    });
                    continue;
                }
                let digest = crate::subagent::digest(&self.journal, &sp.child_session_id, "lost", "").await;
                out.push(c6_proto::ToolResult {
                    call_id: sp.call_id.clone(),
                    tool: tool_of(&sp.call_id).unwrap_or_else(|| "Task".into()),
                    output: format!("{}\n\n{}", TaskLossCause::WorkerRestart.prompt(&spec), crate::subagent::partial(&spec, &digest)),
                    is_error: true,
                    ..Default::default()
                });
            }
        }
        Ok(out)
    }

    /// Applies a command from the head. `Err` is a rejection reason for the head.
    pub async fn handle(&self, cmd: SessionCommand) -> Result<(), String> {
        let internal = |e: anyhow::Error| format!("worker error: {e:#}");
        // The current owner's journal may have been lost. A head-authorized
        // Restore is the only normal command allowed to install a newer
        // incarnation, and must not reuse an earlier worker cache silently.
        if let Some(session_command::Kind::Restore(restore)) = &cmd.kind {
            let (epoch, blocked, receipt) = self.journal.ownership(&cmd.session_id).await.map_err(internal)?;
            if cmd.ownership_epoch > epoch {
                let spec = restore.spec.as_ref().ok_or("restore without a session spec")?;
                self.import_move(&cmd.session_id, spec, &restore.events, cmd.ownership_epoch, &cmd.command_id).await.map_err(internal)?;
                self.journal.activate_ownership(&cmd.session_id, cmd.ownership_epoch, &cmd.command_id).await.map_err(internal)?;
            } else if cmd.ownership_epoch == epoch && blocked && receipt == cmd.command_id {
                self.journal.activate_ownership(&cmd.session_id, cmd.ownership_epoch, &cmd.command_id).await.map_err(internal)?;
            }
        }
        self.journal.check_ownership(&cmd.session_id, cmd.ownership_epoch).await.map_err(internal)?;
        match cmd.kind {
            Some(session_command::Kind::Create(spec)) => {
                if self.journal.has_session(&cmd.session_id).await.map_err(internal)? {
                    return Ok(()); // redelivered
                }
                self.create(&cmd.session_id, &cmd.command_id, spec).await
            }
            Some(session_command::Kind::Input(input)) => {
                if let Some(reason) = self.journal.command_rejection(&cmd.command_id).await.map_err(internal)? {
                    return Err(reason);
                }
                if self.journal.is_applied(&cmd.command_id).await.map_err(internal)? {
                    return Ok(());
                }
                let actor = self.actor(&cmd.session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
                let pending = PendingInput {
                    command_id: cmd.command_id,
                    text: input.text,
                    attachments: input.attachments,
                    command: input.command,
                    wake: input.wake,
                    delegated_by: input.delegated_by,
                    from_session_id: input.from_session_id,
                    from_title: input.from_title,
                    hops: input.hops,
                    api_origin: input.api_origin,
                    human_origin: input.human_origin,
                    send_now: input.send_now,
                    prompt: input.prompt,
                    ..Default::default()
                };
                let choice =
                    CommandChoice { candidates_json: input.command_candidates_json, labels: input.labels, claude_code: input.claude_code };
                let _ = actor.send(ActorMsg::Input(Box::new(pending), input.model, Box::new(choice)));
                Ok(())
            }
            Some(session_command::Kind::Restore(r)) => {
                if self.journal.has_session(&cmd.session_id).await.map_err(internal)? {
                    return Ok(());
                }
                let spec = r.spec.ok_or("restore without a session spec")?;
                self.journal.import(&cmd.session_id, &spec, &r.events).await.map_err(internal)?;
                info!(session = %cmd.session_id, events = r.events.len(), "session restored from the head");
                Ok(())
            }
            Some(session_command::Kind::Rewind(r)) => {
                if let Some(reason) = self.journal.command_rejection(&cmd.command_id).await.map_err(internal)? {
                    return Err(reason);
                }
                if self.journal.is_applied(&cmd.command_id).await.map_err(internal)? {
                    return Ok(());
                }
                // Only to one of your messages, not automation or another
                // session's. Keep legacy messages eligible as the head does.
                let events = self.journal.events_after(&cmd.session_id, 0, u32::MAX).await.map_err(internal)?;
                let is_user = events.iter().any(|e| e.seq == r.to_seq && is_rewindable_message(e));
                if !is_user {
                    return Err("can only rewind to before one of your messages".into());
                }
                // The other runtime's conversation cannot be taken back.
                if events.iter().any(|e| e.seq >= r.to_seq && handoff_of(e).is_some()) {
                    return Err(HANDOFF_REWIND.into());
                }
                if let Some(started) = events.iter().find_map(|e| match &e.kind {
                    Some(session_event::Kind::Started(s)) => Some(s.clone()),
                    _ => None,
                }) && runtime_of(started, &events).tool_profile == crate::claude_code::PROFILE
                {
                    loop {
                        let previous = self.journal.claude_rewind(&cmd.session_id).await.map_err(internal)?;
                        if let Some((pending, 0)) = &previous {
                            if pending.to_seq != r.to_seq || pending.restore_files != r.restore_files {
                                return Err("Another rewind is awaiting confirmation; retry the same target and file-restore choice".into());
                            }
                            if !pending.failure.is_empty() && pending.command_id == cmd.command_id {
                                return Err(pending.failure.clone());
                            }
                            if !self
                                .journal
                                .authorize_claude_rewind_retry_owned(
                                    &cmd.session_id,
                                    &pending.command_id,
                                    &cmd.command_id,
                                    cmd.ownership_epoch,
                                )
                                .await
                                .map_err(internal)?
                            {
                                continue;
                            }
                            // An explicit HTTP retry has a new command ID. Keep
                            // the old intent: its confirmation atomically retires
                            // all authorized receipts before releasing input.
                        } else {
                            // Refuse known blockers before loading an actor or
                            // interrupting its turn. Check again under the intent.
                            if let Err(e) = self.rewind_preflight(&cmd.session_id, &events, r.to_seq, r.restore_files).await {
                                let reason = format!("Rewind was refused before interrupting the conversation: {e:#}");
                                if let Some(actor) = self.actor(&cmd.session_id).await.map_err(internal)? {
                                    let _ = actor.send(ActorMsg::RewindRefused { command_id: cmd.command_id, reason: reason.clone() });
                                }
                                return Err(reason);
                            }
                            // Before loading an idle actor (which may wake notices),
                            // or interrupting a live turn. Restart must observe the
                            // intent before any further model input.
                            self.journal
                                .prepare_claude_rewind_owned(
                                    &cmd.session_id,
                                    &crate::claude_code::Reset {
                                        command_id: cmd.command_id.clone(),
                                        to_seq: r.to_seq,
                                        restore_files: r.restore_files,
                                        session_id: uuid::Uuid::now_v7().to_string(),
                                        fresh: true,
                                        withdrawn: previous.as_ref().map_or_else(Vec::new, |(r, _)| r.withdrawn.clone()),
                                        untouched: true,
                                        previous: previous.map(Box::new),
                                        failure: String::new(),
                                        retries: Vec::new(),
                                    },
                                    cmd.ownership_epoch,
                                )
                                .await
                                .map_err(internal)?;
                        }
                        break;
                    }
                }
                let actor = self.actor(&cmd.session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
                let _ = actor.send(ActorMsg::Rewind { command_id: cmd.command_id, to_seq: r.to_seq, restore_files: r.restore_files });
                Ok(())
            }
            Some(session_command::Kind::Interrupt(i)) => {
                if let Some(a) = self.live(&cmd.session_id) {
                    let _ = a.send(if i.send_now.is_empty() { ActorMsg::Interrupt } else { ActorMsg::SendNow(i.send_now) });
                }
                Ok(())
            }
            // Questions are the head's assistant's; worker sessions ask none.
            Some(session_command::Kind::Answer(_)) => Ok(()),
            Some(session_command::Kind::SetSubagentModel(m)) => {
                if self.journal.is_applied(&cmd.command_id).await.map_err(internal)? {
                    return Ok(());
                }
                if m.model.as_ref().is_some_and(|m| m.connection_kind == "claude_code") {
                    return Err("subagents run in carbon6's own loop, not on Claude Code".into());
                }
                let actor = self.actor(&cmd.session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
                let _ = actor.send(ActorMsg::SetSubagentModel { command_id: cmd.command_id, model: m.model });
                Ok(())
            }
            Some(session_command::Kind::Approve(a)) => {
                if let Some(actor) = self.live(&cmd.session_id) {
                    let _ = actor.send(ActorMsg::Decide { call_id: a.call_id, allow: a.allow });
                }
                Ok(())
            }
            // A remote subagent's turn ended (docs/remote-subagents.md).
            Some(session_command::Kind::DelegationEnded(d)) => {
                if self.journal.is_applied(&cmd.command_id).await.map_err(internal)? {
                    return Ok(());
                }
                let actor = self.actor(&cmd.session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
                let (recorded, ack) = tokio::sync::oneshot::channel();
                actor
                    .send(ActorMsg::RemoteReport(cmd.command_id, Box::new(d), recorded))
                    .map_err(|_| "parent stopped before taking the report")?;
                // Let conn send CommandRejected if the actor cannot record
                // the report. Logging and silently returning Ok hid the
                // delivery failure from the head and the parent's queue.
                ack.await.unwrap_or_else(|_| Err("parent stopped before recording the report".into()))
            }
            Some(session_command::Kind::UpdateNoticeLimits(limits)) => {
                // A trimmed/lost cache gets current settings in its Restore
                // spec. Rejecting this ephemeral command as unknown would
                // consume the head's restore cooldown before a queued input
                // can trigger the actual restoration.
                if let Some(actor) = self.actor(&cmd.session_id).await.map_err(internal)? {
                    actor.send(ActorMsg::NoticeLimits(limits)).map_err(|_| "session stopped before refreshing settings")?;
                }
                Ok(())
            }
            Some(session_command::Kind::ReleaseNotices(release)) => {
                // Loading it delivers what it can; then the rest, now.
                let actor = self.actor(&cmd.session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
                if let Some(limits) = release.limits {
                    let _ = actor.send(ActorMsg::NoticeLimits(limits));
                }
                let _ = actor.send(ActorMsg::ReleaseNotices);
                Ok(())
            }
            Some(session_command::Kind::StopTask(t)) => {
                // Without a live actor no task of the session is running.
                if let Some(a) = self.live(&cmd.session_id) {
                    let _ = a.send(ActorMsg::StopTask(t.task_id));
                }
                Ok(())
            }
            Some(session_command::Kind::ChangePlace(place)) => {
                match self.enqueue_change_place(&cmd.session_id, &cmd.command_id, place).await? {
                    Some(result) => result.await,
                    None => Ok(()),
                }
            }
            None => Err("unsupported command".into()),
        }
    }

    /// Enqueue in command order, returning a separate wait for the actor's result.
    pub(crate) async fn enqueue_change_place(
        &self,
        session_id: &str,
        command_id: &str,
        place: c6_proto::ChangePlace,
    ) -> Result<Option<impl Future<Output = Result<(), String>> + Send + use<>>, String> {
        let internal = |e: anyhow::Error| format!("worker error: {e:#}");
        if self.journal.is_applied(command_id).await.map_err(internal)? {
            return Ok(None);
        }
        let actor = self.actor(session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
        let (reply, result) = tokio::sync::oneshot::channel();
        let stopped = "session stopped before changing place";
        actor.send(ActorMsg::ChangePlace(command_id.to_owned(), place, reply)).map_err(|_| stopped)?;
        let (journal, command_id) = (self.journal.clone(), command_id.to_owned());
        Ok(Some(async move {
            let result = result.await.unwrap_or_else(|_| Err(stopped.into()));
            // A replay can queue before the first receipt is written and reach the
            // actor after it starts other work. The durable success still wins.
            if result.is_err() && matches!(journal.is_applied(&command_id).await, Ok(true)) {
                return Ok(());
            }
            result
        }))
    }

    async fn create(&self, session_id: &str, command_id: &str, spec: CreateSession) -> Result<(), String> {
        let _build_use = self.journal.build_folder_lifecycle.read().await;
        if !spec.ssh_target.trim().is_empty() {
            return self.create_remote(session_id, command_id, spec).await;
        }
        // No folder: the session gets a scratch workspace of its own.
        let cwd = if spec.cwd.trim().is_empty() {
            if spec.worktree {
                return Err("a git worktree needs a folder".into());
            }
            let dir = self.state_dir.join("scratch").join(session_id);
            std::fs::create_dir_all(&dir).map_err(|e| format!("creating a scratch workspace: {e}"))?;
            dir
        } else {
            crate::expand_home(&spec.cwd)
        };
        if !cwd.is_dir() {
            return Err(format!("working directory {} does not exist on this worker", cwd.display()));
        }
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        // A new worktree, made and set up as its template says
        // (docs/dev-starts.md §2); the setup's output goes in the transcript.
        let mut setup_log = String::new();
        let (cwd, worktree_branch, repo_root) = if spec.worktree {
            let ws = spec.worktree_spec.clone().unwrap_or_default();
            let made = crate::worktree::create(&self.worktrees, session_id, &cwd, &ws).await?;
            match crate::worktree::setup(&made, &ws.setup).await {
                Ok(log) => setup_log = log,
                Err(e) => {
                    crate::worktree::discard(&made).await;
                    return Err(e);
                }
            }
            (made.cwd, made.branch, made.root)
        } else {
            (cwd, String::new(), String::new())
        };
        let date = time::OffsetDateTime::now_utc().date().to_string();
        let repo = repo_slug(&cwd).await;
        let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        let model_name = spec.model.as_ref().map(|m| m.model.clone()).unwrap_or_default();
        let labels = session_labels(&spec.labels, &repo, &cwd.display().to_string(), &home, &model_name, "");
        let system_prompt = build_system_prompt(&spec.base_prompt, &cwd, &date);
        self.finish_create(
            session_id,
            command_id,
            spec,
            Place {
                cwd: cwd.clone(),
                local: cwd,
                home,
                repo,
                worktree_branch,
                repo_root,
                ssh_target: String::new(),
                system: String::new(),
                setup_log,
            },
            labels,
            system_prompt,
        )
        .await
    }

    /// A session on another machine over SSH: checks the target (and the
    /// folder there), and describes it to the model.
    async fn create_remote(&self, session_id: &str, command_id: &str, spec: CreateSession) -> Result<(), String> {
        let target = spec.ssh_target.trim().to_owned();
        if spec.model.as_ref().is_some_and(|m| m.connection_kind == "claude_code") {
            return Err("Claude Code sessions run on the worker itself, not over SSH".into());
        }
        let host = c6_tools::remote::SshHost::new(&target, "");
        let (home, cwd, system) = host.probe(&spec.cwd).await.map_err(|e| format!("cannot work on {target}: {e}"))?;
        // Things that stay on the worker (hooks, MCP servers) run here.
        let local = self.state_dir.join("scratch").join(session_id);
        std::fs::create_dir_all(&local).map_err(|e| format!("creating a local folder: {e}"))?;
        let repo = match host.sh(&format!("git -C {} remote get-url origin 2>/dev/null", c6_tools::remote::quote(&cwd)), b"").await {
            Ok(out) => normalize_remote(&String::from_utf8_lossy(&out)),
            Err(_) => String::new(),
        };
        let date = time::OffsetDateTime::now_utc().date().to_string();
        let mut prompt = spec.base_prompt.trim_end().to_owned();
        prompt.push_str(&format!(
            "\n\n# Environment\n- You are working on the remote machine {target} over SSH: shell commands and file tools act on that machine, not on the one you are hosted on.\n\
             - System: {system}\n- Working directory there: {cwd}\n- Home folder there: {home}\n- Session started: {date}\n\
             - Commands run non-interactively (no password prompts): use sudo -n, and say so if a command needs a password or confirmation.\n"
        ));
        let instructions = host
            .sh(&format!("cd {} && (cat AGENTS.md 2>/dev/null || cat CLAUDE.md 2>/dev/null)", c6_tools::remote::quote(&cwd)), b"")
            .await
            .unwrap_or_default();
        if !instructions.is_empty() {
            prompt.push_str(&format!("\n# Project instructions ({cwd})\n\n{}\n", String::from_utf8_lossy(&instructions).trim_end()));
        }
        let model_name = spec.model.as_ref().map(|m| m.model.clone()).unwrap_or_default();
        let labels = session_labels(&spec.labels, &repo, &cwd, &home, &model_name, &target);
        let place = Place {
            cwd: PathBuf::from(&cwd),
            local,
            home,
            repo,
            worktree_branch: String::new(),
            repo_root: String::new(),
            ssh_target: target,
            system,
            setup_log: String::new(),
        };
        self.finish_create(session_id, command_id, spec, place, labels, prompt).await
    }

    async fn finish_create(
        &self,
        session_id: &str,
        command_id: &str,
        mut spec: CreateSession,
        place: Place,
        labels: Vec<String>,
        system_prompt: String,
    ) -> Result<(), String> {
        let Place { cwd, local, home, repo, worktree_branch, repo_root, ssh_target, system, setup_log } = place;
        crate::attachments::restore(&self.state_dir, session_id, &mut spec.seed, !ssh_target.is_empty()).await;
        let model = spec.model.clone().unwrap_or_default();
        let mut setup_errors = Vec::new();
        let mut session_tools = Vec::new();
        let context_window = self.link.context_window(session_id, &model).await;
        let (profile, tools, system_prompt) = if model.connection_kind == "claude_code" {
            // Claude Code brings its own prompt (incl. CLAUDE.md) and tools.
            (crate::claude_code::PROFILE.to_owned(), Vec::new(), crate::claude_code::SYSTEM_PROMPT.to_owned())
        } else {
            let built = own_runtime(&self.link, session_id, &spec, &local, &labels, system_prompt, "startup").await;
            setup_errors = built.errors;
            let names = visible_tool_names(&spec, &built.tools);
            session_tools = built.tools;
            (built.profile.as_str().to_owned(), names, built.prompt)
        };
        let started = SessionStarted {
            command_id: command_id.to_owned(),
            model: spec.model.clone(),
            cwd: cwd.display().to_string(),
            system_prompt,
            tools,
            hosted_search_off: !c6_tools::tool_allowed(&spec.tools, "web_search"),
            tool_profile: profile.clone(),
            worktree_branch,
            repo_root,
            context_window,
            repo,
            home,
            ssh_target,
            system,
            // A remote subagent's place in its tree.
            depth: spec.depth,
            ..Default::default()
        };
        let ev = SessionEvent { seq: 1, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Started(started.clone())) };
        self.journal.create_session(session_id, &spec, &ev).await.map_err(|e| format!("worker error: {e:#}"))?;
        // Non-fatal setup problems (an MCP server that would not start) are
        // shown in the session.
        let mut next_seq = 2;
        if !setup_log.trim().is_empty() {
            let notice =
                Notice { title: "Worktree set up".into(), message: setup_log.trim_end().to_owned(), priority: "low".into(), quiet: true };
            let ev = SessionEvent { seq: next_seq, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Notice(notice)) };
            self.journal.append(session_id, &ev).await.map_err(|e| format!("worker error: {e:#}"))?;
            next_seq += 1;
        }
        for message in setup_errors {
            let ev =
                SessionEvent { seq: next_seq, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Error(ErrorEvent { message })) };
            self.journal.append(session_id, &ev).await.map_err(|e| format!("worker error: {e:#}"))?;
            next_seq += 1;
        }
        info!(session = %session_id, cwd = %cwd.display(), %profile, "session created");
        let conv = Conversation { history: history_from_events(&seed_events(&spec)), context_tokens: 0 };
        self.spawn(session_id, spec, started, conv, next_seq, 0, Some(session_tools), 0)
            .await
            .map_err(|e| format!("worker error: {e:#}"))?;
        Ok(())
    }

    /// The session's actor, starting it from the journal if needed.
    async fn actor(&self, session_id: &str) -> anyhow::Result<Option<mpsc::UnboundedSender<ActorMsg>>> {
        Ok(self.load(session_id).await?.map(|h| h.tx))
    }

    /// Check the whole local tree, not just a child's current model turn.
    /// Raw task/link history remains relevant after a crash or a rewind.
    /// Non-destructive admission checks: do not interrupt a turn, cancel contexts,
    /// signal processes, or change conversation history here.
    async fn rewind_preflight(&self, sid: &str, events: &[SessionEvent], to_seq: u64, restore_files: bool) -> anyhow::Result<()> {
        self.journal.confirm_shell_writers_stopped(sid).await?;
        let affected = rewind_affected_events(events, to_seq, restore_files);
        self.rewind_children_stopped(&affected).await?;
        let registry = self.actors.lock().unwrap().get(sid).map(|h| h.tools.background.list()).unwrap_or_default();
        let kept: HashSet<_> = c6_agent::effective_events(events)
            .into_iter()
            .filter(|e| !restore_files && e.seq < to_seq)
            .filter_map(|e| match e.kind {
                Some(session_event::Kind::SubagentSpawned(s)) => Some(s.background_task_id),
                _ => None,
            })
            .collect();
        anyhow::ensure!(
            !registry.iter().any(|t| t.running && t.kind != c6_tools::TaskKind::ClaudeCode && !kept.contains(&t.id)),
            "Stop or wait for carbon6 background tasks/subagents before rewinding"
        );
        let mut durable = HashMap::new();
        let ownership = if restore_files { events.to_vec() } else { c6_agent::effective_events(events) };
        for event in ownership {
            if let Some(session_event::Kind::BackgroundTask(t)) = event.kind {
                durable.insert(t.task_id.clone(), t);
            }
        }
        for task in durable.values().filter(|t| t.state == "started" && t.kind != "claude_code" && !kept.contains(&t.task_id)) {
            if registry.iter().any(|t| t.id == task.task_id && !t.running)
                || (task.kind == "agent"
                    && affected.iter().any(|e| {
                        matches!(&e.kind,
                        Some(session_event::Kind::SubagentSpawned(s)) if s.background_task_id == task.task_id)
                    }))
            {
                continue;
            }
            anyhow::ensure!(
                task.pgid > 0 && nix::sys::signal::killpg(nix::unistd::Pid::from_raw(task.pgid), None) == Err(nix::errno::Errno::ESRCH),
                "Background task {} has not confirmed that it stopped; stop it before retrying Rewind",
                task.task_id
            );
        }
        Ok(())
    }

    async fn rewind_children_stopped(&self, events: &[SessionEvent]) -> anyhow::Result<()> {
        self.rewind_children_quiescent(events, false).await
    }

    async fn rewind_children_quiescent(&self, events: &[SessionEvent], seal_contexts: bool) -> anyhow::Result<()> {
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        let mut descendants = events.to_vec();
        loop {
            for event in &descendants {
                if let Some(session_event::Kind::SubagentSpawned(child)) = &event.kind {
                    anyhow::ensure!(
                        child.remote.is_none(),
                        StopUnconfirmed(format!(
                            "Remote subagent {} cannot confirm quiescence; stop confirmation is required before rewinding",
                            child.child_session_id
                        ))
                    );
                    anyhow::ensure!(
                        !child.child_session_id.is_empty(),
                        StopUnconfirmed("A subagent's identity is unknown; cannot confirm it stopped".into())
                    );
                    if seen.insert(child.child_session_id.clone()) {
                        pending.push(child.child_session_id.clone());
                    }
                }
            }
            let Some(id) = pending.pop() else { return Ok(()) };
            self.journal.confirm_shell_writers_stopped(&id).await?;
            // A completed Claude Code turn does not stop its CLI: it can
            // still write files or start work while the child actor is idle,
            // or after that actor/worker has gone away. Check every owned
            // runtime, including unselected forks, without adopting/stopping
            // it. The caller's affected-event selection keeps retained
            // children exempt only for conversation-only Rewind.
            let claude_stopped = async {
                crate::claude_code::confirm_held_stopped(&self.claude_dir().join("held"), &id)?;
                crate::claude_code::confirm_stopped(&self.journal, &id, std::time::Duration::ZERO).await
            }
            .await;
            if let Err(error) = claude_stopped {
                return Err(error.context(format!(
                    "Subagent {id} has a live or unconfirmed Claude Code runtime; stop confirmation is required before rewinding"
                )));
            }
            let handle = self.actors.lock().unwrap().get(&id).filter(|h| !h.tx.is_closed()).cloned();
            let registry = if let Some(handle) = handle {
                anyhow::ensure!(
                    !handle.running.load(std::sync::atomic::Ordering::SeqCst)
                        && handle.inbox.lock().unwrap().queue.is_empty()
                        && !handle.tools.background.list().iter().any(|t| t.running),
                    StopUnconfirmed(format!("Subagent {id} is still working; stop or wait for it before retrying Rewind"))
                );
                // A mailbox barrier also covers inputs/notices not yet moved
                // into Inbox and notices held on the actor's hourly timer.
                let (reply, idle) = tokio::sync::oneshot::channel();
                handle
                    .tx
                    .send(ActorMsg::RewindIdle(reply))
                    .map_err(|_| StopUnconfirmed(format!("Subagent {id} stopped before confirming idle")))?;
                let idle = tokio::time::timeout(std::time::Duration::from_secs(5), idle)
                    .await
                    .map_err(|_| StopUnconfirmed(format!("Subagent {id} did not confirm idle")))?
                    .map_err(|_| StopUnconfirmed(format!("Subagent {id} stopped before confirming idle")))?;
                anyhow::ensure!(
                    idle,
                    StopUnconfirmed(format!("Subagent {id} has pending work; stop or wait for it before retrying Rewind"))
                );
                if seal_contexts {
                    handle.tools.background.cancel_shell_contexts();
                }
                handle.tools.background.list()
            } else {
                Vec::new()
            };
            let Some((_, events)) = self.journal.load_session(&id).await? else {
                anyhow::bail!(StopUnconfirmed(format!("Subagent {id} has no local history; cannot confirm it stopped")));
            };
            rewind_child_history_stopped(&id, &events, &registry)?;
            descendants = events;
        }
    }

    /// The session's live actor and tools, starting it from the journal if
    /// needed.
    async fn load(&self, session_id: &str) -> anyhow::Result<Option<Handle>> {
        self.load_with_task_loss(session_id, TaskLossCause::Unknown).await
    }

    async fn load_with_task_loss(&self, session_id: &str, cause: TaskLossCause) -> anyhow::Result<Option<Handle>> {
        let _build_use = self.journal.build_folder_lifecycle.read().await;
        let _loading = self.load_gate(session_id).lock_owned().await;
        let (_, blocked, _) = self.journal.ownership(session_id).await?;
        anyhow::ensure!(!blocked, "session ownership is released or awaiting adoption");
        if let Some(a) = self.actors.lock().unwrap().get(session_id).filter(|a| !a.tx.is_closed()) {
            return Ok(Some(a.clone()));
        }
        let Some((spec, mut events, epoch)) = self.journal.load_session_owned(session_id).await? else { return Ok(None) };
        let started = events.iter().find_map(|e| match &e.kind {
            Some(session_event::Kind::Started(s)) => Some(s.clone()),
            _ => None,
        });
        let Some(started) = started else { anyhow::bail!("session {session_id} has no SessionStarted event") };
        // What it runs with now: a handoff may have changed it.
        let started = runtime_of(started, &events);
        let mut next_seq = events.last().map(|e| e.seq + 1).unwrap_or(1);
        // A turn left open by a failed turn task: close it so the head and
        // the history agree that it is over.
        let open_turn = events.iter().rev().find_map(|e| match &e.kind {
            Some(session_event::Kind::TurnStarted(t)) => Some(Some(t.turn)),
            Some(session_event::Kind::TurnEnded(_)) => Some(None),
            _ => None,
        });
        if let Some(Some(turn)) = open_turn {
            warn!(session = %session_id, turn, "closing a turn left open by a failed turn");
            let kind = session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 });
            let event = SessionEvent { seq: next_seq, ts_ms: c6_core::now_ms(), kind: Some(kind) };
            self.journal.append_owned(session_id, &event, epoch).await?;
            events.push(event);
            next_seq += 1;
        }
        if spec.delegation.is_some()
            && !events.iter().any(|e| {
                matches!(&e.kind,
            Some(session_event::Kind::SubagentReported(r)) if r.command_id == local_report_tracking(session_id))
            })
        {
            // Adopt old sessions without replaying already-reported history.
            let event = SessionEvent {
                seq: next_seq,
                ts_ms: c6_core::now_ms(),
                kind: Some(session_event::Kind::SubagentReported(local_report_boundary(session_id))),
            };
            self.journal.append_owned(session_id, &event, epoch).await?;
            events.push(event);
            next_seq += 1;
        }
        let turns = events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_)))).count() as u64;
        let mut spec = spec;
        let local_child = spec.delegation.is_some();
        crate::attachments::restore(&self.state_dir, session_id, &mut spec.seed, !started.ssh_target.is_empty()).await;
        crate::attachments::restore(&self.state_dir, session_id, &mut events, !started.ssh_target.is_empty()).await;
        let conv = conversation_from_log(&mut spec, &events);
        let handle = self.spawn(session_id, spec, started, conv, next_seq, turns, None, epoch).await?;
        let tx = handle.tx.clone();
        // Recover held tasks; otherwise report an open task whose outcome
        // is missing. A lazy load need not mean the worker restarted.
        let mut open: Vec<c6_proto::BackgroundTask> = Vec::new();
        let mut highest = 0u64;
        for e in &events {
            if let Some(session_event::Kind::BackgroundTask(t)) = &e.kind {
                highest = highest.max(t.task_id.trim_start_matches("bg").parse().unwrap_or(0));
                open.retain(|o| o.task_id != t.task_id);
                if t.state == "started" {
                    open.push(t.clone());
                }
            }
        }
        let _ = tx.send(ActorMsg::Reserve(highest));
        // Notices no turn took are delivered again.
        let (_, noticed, pending) = notices_of(&events);
        let _ = tx.send(ActorMsg::Reloaded { noticed, pending });
        if local_child && !pending_local_reports(session_id, &events).is_empty() {
            let _ = tx.send(ActorMsg::RecoverReport);
        }
        for t in open {
            let (child, remote) = events
                .iter()
                .find_map(|e| match &e.kind {
                    Some(session_event::Kind::SubagentSpawned(sp)) if sp.background_task_id == t.task_id => {
                        Some((sp.child_session_id.clone(), sp.remote.is_some()))
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let _ = tx.send(ActorMsg::Lost {
                cause,
                task_id: t.task_id,
                kind: t.kind,
                description: t.description,
                native_type: t.native_type,
                notify: t.notify,
                pgid: t.pgid,
                child,
                remote,
            });
        }
        Ok(Some(handle))
    }

    /// Starts a subagent: journals its session (`spec.delegation` set) and
    /// starts its actor with `tools`, the parent's as the subagent may use
    /// them. Returns where to send it work.
    pub(crate) async fn start_subagent(
        &self,
        session_id: &str,
        spec: CreateSession,
        started: SessionStarted,
        tools: Vec<Arc<dyn Tool>>,
    ) -> anyhow::Result<mpsc::UnboundedSender<ActorMsg>> {
        let ev = SessionEvent { seq: 1, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Started(started.clone())) };
        self.journal.create_session(session_id, &spec, &ev).await?;
        self.journal
            .append(
                session_id,
                &SessionEvent {
                    seq: 2,
                    ts_ms: c6_core::now_ms(),
                    kind: Some(session_event::Kind::SubagentReported(local_report_boundary(session_id))),
                },
            )
            .await?;
        let conv = Conversation { history: Vec::new(), context_tokens: 0 };
        Ok(self.spawn(session_id, spec, started, conv, 3, 0, Some(tools), 0).await?.tx)
    }

    /// The shared tool state of a subagent's parent, starting the parent's
    /// actor if needed (after a restart, a subagent may be loaded first).
    /// None for a Claude Code parent: carbon6 runs none of its tools, so
    /// its subagents work with hooks, policies and sinks of their own.
    fn parent_tools<'a>(&'a self, parent_id: &'a str) -> BoxFuture<'a, anyhow::Result<Option<ParentTools>>> {
        Box::pin(async move {
            Ok(self.load(parent_id).await?.map(|h| {
                let claude_code = h.claude_code.load(std::sync::atomic::Ordering::SeqCst);
                (h.tools, claude_code)
            }))
        })
    }

    /// A session's tools in `profile`, built afresh: for a session loaded
    /// from its log, and for a subagent whose model takes another tool
    /// profile than its parent's (whose tools it cannot share).
    pub(crate) async fn session_tool_set(
        &self,
        session_id: &str,
        spec: &CreateSession,
        started: &SessionStarted,
        profile: ToolProfile,
    ) -> BuiltTools {
        // Secrets go by the tree's top-level session.
        let on_behalf_of = spec.delegation.as_ref().map_or(session_id, |d| d.root_session_id.as_str());
        let local = if started.ssh_target.is_empty() {
            PathBuf::from(&started.cwd)
        } else {
            let dir = self.state_dir.join("scratch").join(session_id);
            let _ = std::fs::create_dir_all(&dir);
            dir
        };
        let labels = session_labels(&spec.labels, &started.repo, &started.cwd, &started.home, &spawn_model(spec), &started.ssh_target);
        let names: Vec<String> = applicable_secrets(spec, &labels).into_iter().map(|(n, _)| n).collect();
        let (secret_values, _) = fetch_secrets(&self.link, on_behalf_of, &names).await;
        build_tools(spec, &local, profile, &labels, &secret_values).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn(
        &self,
        session_id: &str,
        spec: CreateSession,
        started: SessionStarted,
        conv: Conversation,
        next_seq: u64,
        turns: u64,
        tools: Option<Vec<Arc<dyn Tool>>>,
        epoch: u64,
    ) -> anyhow::Result<Handle> {
        let (tx, rx) = mpsc::unbounded_channel();
        let delegation = spec.delegation.clone();
        // A subagent's secrets and head calls go by its tree's top-level
        // session (its row always exists on the head).
        let on_behalf_of = delegation.as_ref().map_or(session_id, |d| d.root_session_id.as_str()).to_owned();
        let mut writer = Writer::new(session_id.to_owned(), self.journal.clone(), self.link.clone(), next_seq);
        writer.ownership_epoch = epoch;
        writer.disk_probe = Some((self.disk.clone(), self.state_dir.clone()));
        let writer = Arc::new(writer);
        writer.set_keep_warm_capture(&spec);
        // On an SSH target, what runs on the worker (hooks, MCP servers)
        // runs in a local folder of the session's.
        let remote = !started.ssh_target.is_empty();
        let local = if remote {
            let dir = self.state_dir.join("scratch").join(session_id);
            let _ = std::fs::create_dir_all(&dir);
            dir
        } else {
            PathBuf::from(&started.cwd)
        };
        let tools = match tools {
            Some(t) => t,
            None if started.tool_profile == crate::claude_code::PROFILE => Vec::new(),
            None => {
                let built = self.session_tool_set(session_id, &spec, &started, ToolProfile::parse(&started.tool_profile)).await.tools;
                // A local subagent keeps the tools it was authorized to
                // start with, including any hidden by the old depth limit.
                match &delegation {
                    Some(_) => {
                        let saved = crate::subagent::saved_tools(&spec, &started);
                        built.into_iter().filter(|t| saved.contains(&t.spec().name)).collect()
                    }
                    None => built,
                }
            }
        };
        let started_cwd = started.cwd.clone();
        // The head's hooks. Claude Code sessions get them as Claude Code
        // settings and run them itself.
        let labels = session_labels(&spec.labels, &started.repo, &started.cwd, &started.home, &spawn_model(&spec), &started.ssh_target);
        let ssh = remote.then(|| Arc::new(c6_tools::remote::SshHost::new(&started.ssh_target, &started.home)));
        let launch_pressure =
            Arc::new(if remote { (*self.launch_pressure).clone() } else { self.launch_pressure.scoped(Path::new(&started.cwd)) });
        let session_tools = Arc::new(SessionTools::with_ssh(PathBuf::from(&started.cwd), ssh));
        if !remote {
            session_tools.set_command_env(crate::compile_cache::session_environment(self.state_dir.clone()).await);
        }
        session_tools.set_kit_admission(launch_pressure.clone());
        session_tools.background.set_shell_owner(self.journal.shell_owner(session_id, &on_behalf_of));
        // Unlogged starts still own IDs. Reserve before exposing tools, not
        // through the asynchronous reload mailbox after launches can begin.
        for source in self.journal.shell_writer_sources(session_id).await? {
            if let Some(n) = source.strip_prefix("bg").and_then(|n| n.parse().ok()) {
                session_tools.background.reserve_ids(n);
            }
        }
        session_tools.browser.set_profile(self.state_dir.join("browser").join(session_id));
        // Shell commands outlive the worker, and its next run takes them up
        // (docs/restarts.md §3).
        session_tools.background.set_held_dir(self.state_dir.join("held").join(session_id));
        let mut hooks = None;
        let mut policies = None;
        // A subagent works with its parent's tool hooks and policies (held
        // calls wait in the parent), publisher, memory, notices and way to
        // the head. Without its parent here (it was trimmed), its own. A
        // Claude Code parent's hooks run in its CLI, not in carbon6: a
        // subagent of one runs the head's hooks and policies itself. A
        // Claude Code child also needs its own CLI hooks and MCP policies,
        // even when its parent runs carbon6's loop. What it publishes,
        // remembers and notifies is still its parent's.
        let (parent, parent_claude_code) = match &delegation {
            Some(d) => match self.parent_tools(&d.parent_session_id).await {
                Ok(Some((tools, claude_code))) => (Some(tools), claude_code),
                Ok(None) => (None, false),
                Err(e) => {
                    warn!(session = %session_id, parent = %d.parent_session_id, "a subagent without its parent: {e:#}");
                    (None, false)
                }
            },
            None => (None, false),
        };
        if let Some(parent) = &parent {
            if !parent_claude_code {
                session_tools.set_hooks(parent.hooks().map(|h| Arc::new(crate::subagent::SubagentHooks(h)) as Arc<dyn c6_tools::Hooks>));
            }
            session_tools.set_publisher(parent.publisher());
            session_tools.set_memory(parent.memory());
            session_tools.set_notices(parent.notices());
            session_tools.set_head(parent.head());
        } else {
            session_tools.set_head(Some(Arc::new(SessionHead { link: self.link.clone(), session_id: on_behalf_of.clone() })));
        }
        if parent.is_none() || parent_claude_code || started.tool_profile == crate::claude_code::PROFILE {
            hooks = crate::hooks::HookRunner::new(session_id, &local, &spec.hooks_json, &labels).map(Arc::new);
            // Policies first (a denial wins), then your hooks; installed
            // only while carbon6's loop runs the tools (`own_loop_hooks`).
            let (ap_tx, mut ap_rx) = mpsc::unbounded_channel();
            policies = crate::policy::Policies::new(&spec.policies_json, &labels, ap_tx);
            if let Some(p) = &policies {
                p.set_judge(session_tools.head());
            }
            let actor_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(e) = ap_rx.recv().await {
                    if actor_tx.send(ActorMsg::Approval(e)).is_err() {
                        break;
                    }
                }
            });
            if started.tool_profile != crate::claude_code::PROFILE {
                session_tools.set_hooks(own_loop_hooks(&policies, &hooks));
            }
        }
        // What carbon6's tools report goes to the actor, whichever runtime
        // the session runs on now: a handoff can change that, and in Claude
        // Code these stay quiet (it runs its own tools).
        if parent.is_none() {
            // Memory changes go to the actor, which logs them (the head
            // applies them).
            let (mem_tx, mut mem_rx) = mpsc::unbounded_channel();
            session_tools.set_memory(Some(mem_tx));
            let actor_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(m) = mem_rx.recv().await {
                    if actor_tx.send(ActorMsg::Memory(m)).is_err() {
                        break;
                    }
                }
            });
            // Notices too (the head sends them to your channels).
            let (note_tx, mut note_rx) = mpsc::unbounded_channel();
            session_tools.set_notices(Some(note_tx));
            let actor_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(n) = note_rx.recv().await {
                    if actor_tx.send(ActorMsg::Notice(n)).is_err() {
                        break;
                    }
                }
            });
            // Published files go to the actor, which logs them.
            let (pub_tx, mut pub_rx) = mpsc::unbounded_channel();
            session_tools.set_publisher(Some(pub_tx));
            let actor_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(p) = pub_rx.recv().await {
                    if actor_tx.send(ActorMsg::Published(p)).is_err() {
                        break;
                    }
                }
            });
        }
        let (task_tx, task_rx) = mpsc::unbounded_channel();
        let (task_fence, fence_rx) = mpsc::unbounded_channel();
        session_tools.background.set_sink(Some(task_tx));
        tokio::spawn(forward_tasks(task_rx, fence_rx, tx.clone()));
        let spec_default = spec.clone();
        let claude_code = Arc::new(std::sync::atomic::AtomicBool::new(started.tool_profile == crate::claude_code::PROFILE));
        self.disk.configure(&spec);
        let actor = Actor {
            spec,
            launch_pressure,
            disk_admitted: turns > 0,
            session_tools,
            hooks,
            policies,
            tools,
            journal: self.journal.clone(),
            started,
            conv: Some(conv),
            turns,
            writer,
            task_fence,
            link: self.link.clone(),
            inbox: Arc::new(Mutex::new(Inbox::default())),
            // Files on an SSH target are not checkpointed.
            checkpoints: Arc::new(Checkpoints::new(&self.state_dir, session_id, if remote { &local } else { Path::new(&started_cwd) })),
            notice_arrivals: HashMap::new(),
            notice_deadline: None,
            notice_ready: HashSet::new(),
            notice_pause: None,
            notice_warning: None,
            deferred: Vec::new(),
            unrecorded_notices: Vec::new(),
            pending_task_event: None,
            rewinds: VecDeque::new(),
            noticed: HashSet::new(),
            on_behalf_of,
            waiting: Vec::new(),
            local_report_acks: HashMap::new(),
            local_report_retry_at: None,
            local_report_parent: LocalReportParent::default(),
            remote_receipts: Vec::new(),
            manager: self.clone(),
            subagent_default: Arc::new(Mutex::new(crate::subagent::SubagentDefault::of(&spec_default))),
            launch_limits: Arc::new(Mutex::new(crate::subagent::LaunchLimits::of(&spec_default))),
            running_flag: Arc::default(),
            claude_code: claude_code.clone(),
            claude: None,
            withdrawn: HashSet::new(),
            claude_stdin: Arc::default(),
            handoff_announced: false,
            stopping: false,
            stopped_until: None,
        };
        let recovery = Arc::new(RecoveryGate {
            epoch,
            activity: actor.writer.activity.clone(),
            has_turn: std::sync::atomic::AtomicBool::new(turns > 0),
            ..Default::default()
        });
        let handle = Handle {
            tx,
            recovery: recovery.clone(),
            tools: actor.session_tools.clone(),
            running: actor.running_flag.clone(),
            claude_code,
            inbox: actor.inbox.clone(),
        };
        self.journal.check_ownership(session_id, epoch).await?;
        tokio::spawn(actor.supervise(rx, recovery));
        self.actors.lock().unwrap().insert(session_id.to_owned(), handle.clone());
        Ok(handle)
    }
}

/// The conversation a session's log adds up to (its seed first, rewinds
/// applied), and the model it runs on now (its last switch, set in
/// `spec`). Reasoning from before the current model's run is dropped:
/// another model wrote it. After a handoff between Claude Code and
/// carbon6's own loop, everything before it is a marked transcript at the
/// start of the conversation, not native history.
fn conversation_from_log(spec: &mut CreateSession, events: &[SessionEvent]) -> Conversation {
    let seed = seed_events(spec);
    let seed_len = seed.len();
    let all = [seed, events.to_vec()].concat();
    let model_of = |e: &SessionEvent| match &e.kind {
        Some(session_event::Kind::ModelSwitched(m)) => m.model.clone(),
        Some(session_event::Kind::Started(s)) => s.model.clone(),
        _ => None,
    };
    let key = |m: &ModelSpec| (m.connection_id.clone(), m.model.clone());
    if let Some(m) = events.iter().rev().find_map(|e| match &e.kind {
        Some(session_event::Kind::ModelSwitched(m)) => m.model.clone(),
        _ => None,
    }) {
        spec.model = Some(m);
    }
    // And its subagents' default, as last set.
    if let Some(m) = events.iter().rev().find_map(|e| match &e.kind {
        Some(session_event::Kind::SubagentModelSet(m)) => Some(m.model.clone()),
        _ => None,
    }) {
        set_subagent_default(spec, m);
    }
    let current = spec.model.as_ref().map(key);
    let handoff = events.iter().rposition(|e| handoff_of(e).is_some()).map(|h| seed_len + h);
    let (transcript, all) = match handoff {
        Some(h) => {
            let before = &all[..h];
            let from = runtime_left(before, handoff_of(&all[h]).expect("a handoff"));
            (Some(c6_agent::handoff_context(before, &from, c6_agent::HANDOFF_BUDGET)), all[h..].to_vec())
        }
        None => (None, all),
    };
    // Start of the current model's run: just after the last event naming
    // another model, at the next event naming this one.
    let other = all.iter().rposition(|e| model_of(e).is_some_and(|m| Some(key(&m)) != current));
    let mut history = match other {
        Some(i) => {
            let run = all[i + 1..].iter().position(|e| model_of(e).is_some()).map_or(all.len(), |p| i + 1 + p);
            history_from_events(&[c6_agent::portable_events(&all[..run]), all[run..].to_vec()].concat())
        }
        None => history_from_events(&all),
    };
    if let Some(t) = transcript {
        match history.first_mut() {
            Some(m) if m.role == Role::User => m.blocks.insert(0, Block::Text(t)),
            _ => history.insert(0, Message::user_text(t)),
        }
    }
    let context_tokens = c6_agent::context_tokens_from_events(events);
    Conversation { history, context_tokens }
}

/// What carbon6's loop checks around each tool call of a top-level
/// session: the policies first (a denial wins), then your hooks. Claude
/// Code runs its tools itself, with the hooks as its own settings.
fn own_loop_hooks(
    policies: &Option<Arc<crate::policy::Policies>>,
    hooks: &Option<Arc<crate::hooks::HookRunner>>,
) -> Option<Arc<dyn c6_tools::Hooks>> {
    let mut chain: Vec<Arc<dyn c6_tools::Hooks>> = Vec::new();
    if let Some(p) = policies {
        chain.push(p.clone());
    }
    if let Some(h) = hooks {
        chain.push(h.clone());
    }
    match chain.len() {
        0 => None,
        1 => chain.pop(),
        _ => Some(Arc::new(crate::policy::Chain(chain))),
    }
}

/// Match the head's rewind eligibility, including commands from older
/// heads. Unknown provenance alone cannot bar the person's actions; explicit
/// nonhuman provenance always wins over a human flag.
fn is_rewindable_message(event: &SessionEvent) -> bool {
    matches!(&event.kind, Some(session_event::Kind::User(u))
        if u.api_origin.is_none() && u.from_session_id.is_empty() && u.delegated_by.is_empty()
            && !matches!(u.command.as_str(), "task-notification" | "stop-hook"))
}

/// The handoff a model switch made, if it crossed runtimes.
fn handoff_of(e: &SessionEvent) -> Option<&c6_proto::Handoff> {
    match &e.kind {
        Some(session_event::Kind::ModelSwitched(m)) => m.handoff.as_ref(),
        _ => None,
    }
}

/// What the session runs with from a handoff on.
fn apply_handoff(started: &mut SessionStarted, h: &c6_proto::Handoff) {
    started.tool_profile = h.tool_profile.clone();
    started.tools = h.tools.clone();
    started.system_prompt = h.system_prompt.clone();
}

fn apply_place(started: &mut SessionStarted, place: &c6_proto::PlaceChanged) {
    if let Some(to) = &place.to {
        started.cwd = to.cwd.clone();
        started.repo = to.repo.clone();
        started.repo_root = to.repo_root.clone();
        started.worktree_branch = to.worktree_branch.clone();
        started.system_prompt = place.system_prompt.clone();
        started.tools = place.tools.clone();
        started.tool_profile = place.tool_profile.clone();
    }
}

/// Runtime and placement are not conversation history: neither is rewound.
fn runtime_of(mut started: SessionStarted, events: &[SessionEvent]) -> SessionStarted {
    for event in events {
        if let Some(h) = handoff_of(event) {
            apply_handoff(&mut started, h);
        } else if let Some(session_event::Kind::PlaceChanged(place)) = &event.kind {
            apply_place(&mut started, place);
        }
    }
    started
}

/// Whether a session is one imported from Claude Code (docs/imports.md)
/// that carbon6 has not run yet: everything in its log came from Claude
/// Code's file, so Claude Code's own conversation (by the session's id) is
/// still the whole story.
fn continues_claude_import(started: &SessionStarted, events: &[SessionEvent]) -> bool {
    started.command_id == "import-claude_code"
        && events.iter().all(|e| match &e.kind {
            Some(session_event::Kind::User(u)) => u.command_id.starts_with("import-"),
            Some(session_event::Kind::ModelSwitched(_) | session_event::Kind::Checkpoint(_) | session_event::Kind::Rewound(_)) => false,
            _ => true,
        })
}

/// The runtime a handoff `h` leaves, for its transcript's wording: the
/// other one, with the last model that ran before it (`before`).
fn runtime_left(before: &[SessionEvent], h: &c6_proto::Handoff) -> String {
    let model = before
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            Some(session_event::Kind::ModelSwitched(m)) => m.model.as_ref().map(|m| m.model.clone()),
            Some(session_event::Kind::Started(s)) => s.model.as_ref().map(|m| m.model.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let name = if h.tool_profile == crate::claude_code::PROFILE { "carbon6's own agent loop" } else { "Claude Code" };
    if model.is_empty() { name.to_owned() } else { format!("{name} ({model})") }
}

/// The Claude Code conversation a Claude Code session's next turn
/// continues, as of its log. Handed over to Claude Code, it is the one its
/// last handoff started, which a new conversation begins with the
/// handoff's transcript; created on Claude Code, its own id, with a side
/// chat's context in front.
pub(crate) fn claude_conversation(
    session_id: &str,
    spec: &CreateSession,
    events: &[SessionEvent],
) -> crate::claude_code::ClaudeConversation {
    let turns_since = |from: usize| events[from..].iter().any(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_))));
    match events.iter().rposition(|e| handoff_of(e).is_some()) {
        Some(h) => {
            let handoff = handoff_of(&events[h]).expect("a handoff");
            let before = [seed_events(spec), events[..h].to_vec()].concat();
            let preamble = c6_agent::handoff_context(&before, &runtime_left(&before, handoff), c6_agent::HANDOFF_BUDGET);
            let id = if handoff.claude_session_id.is_empty() { session_id.to_owned() } else { handoff.claude_session_id.clone() };
            crate::claude_code::ClaudeConversation {
                id,
                // Claude Code's own (imported) conversation goes on as it
                // is; the transcript only if it has gone.
                resume: handoff.native || turns_since(h),
                preamble: Some(format!("{preamble}\n\nThe next message follows:")),
            }
        }
        None => crate::claude_code::ClaudeConversation {
            id: session_id.to_owned(),
            resume: turns_since(0),
            // Claude Code keeps its own history: a side chat's context goes
            // in as text with the first question.
            preamble: (!spec.seed.is_empty()).then(|| {
                format!(
                    "<conversation-so-far>\n{}\n</conversation-so-far>\n\nThis session branched off the conversation above. The next message follows:",
                    c6_agent::transcript_text(&spec.seed)
                )
            }),
        },
    }
}

/// Apply the worker's durable CLI branch selection as well as public history.
/// A moved transcript is pinned by public history, even without local chain
/// metadata. Later unmapped rewinds still use kept-history seeding.
pub(crate) async fn journal_claude_conversation(
    journal: &Journal,
    session_id: &str,
    spec: &CreateSession,
    events: &[SessionEvent],
) -> anyhow::Result<crate::claude_code::ClaudeConversation> {
    let mut conversation = claude_conversation(session_id, spec, events);
    let handoff_seq = events.iter().rev().find(|e| handoff_of(e).is_some()).map_or(0, |e| e.seq);
    let mut anchor = handoff_seq;
    if let Some((seq, resume_id)) = events.iter().rev().find_map(|e| match &e.kind {
        Some(session_event::Kind::PlaceChanged(place)) if e.seq > handoff_seq && !place.native_resume_id.is_empty() => {
            Some((e.seq, &place.native_resume_id))
        }
        _ => None,
    }) {
        anchor = seq;
        conversation.id.clone_from(resume_id);
        conversation.resume = true;
    }
    let mut since = anchor;
    let rewound_seq = events.iter().rev().find(|e| matches!(e.kind, Some(session_event::Kind::Rewound(_)))).map_or(0, |e| e.seq);
    let mut reset_found = false;
    if let Some((reset, applied)) = journal.claude_rewind(session_id).await? {
        anyhow::ensure!(applied != 0, "Claude Code rewind is not confirmed; retry Rewind before sending more input");
        if applied > anchor && applied >= rewound_seq {
            reset_found = true;
            since = applied;
            conversation.id = reset.session_id;
            conversation.resume =
                !reset.fresh || events.iter().any(|e| e.seq > applied && matches!(e.kind, Some(session_event::Kind::TurnStarted(_))));
            // Apply earlier rewinds before selecting this reset's cutoff:
            // a later marker can withdraw text before a historical target.
            let before: Vec<_> = events.iter().filter(|e| e.seq < applied).cloned().collect();
            let kept: Vec<_> = c6_agent::effective_events(&before).into_iter().filter(|e| e.seq < reset.to_seq).collect();
            conversation.preamble = Some(c6_agent::rewind_context(&[seed_events(spec), kept].concat(), c6_agent::HANDOFF_BUDGET));
        }
    }
    // A pre-upgrade rewind, or a replacement worker without the old journal:
    // there is no trustworthy Claude branch mapping. Never resume the original
    // session. Fresh kept-history seeding is the only safe reconstruction.
    if rewound_seq > anchor && !reset_found {
        since = rewound_seq;
        conversation.id = uuid::Uuid::now_v7().to_string();
        conversation.resume = false;
        conversation.preamble = Some(c6_agent::rewind_context(&[seed_events(spec), events.to_vec()].concat(), c6_agent::HANDOFF_BUDGET));
    }
    for e in c6_agent::effective_events(events).iter().rev().filter(|e| e.seq > since) {
        if let Some(session_event::Kind::TurnEnded(_)) = &e.kind
            && let Some(boundary) = journal.claude_turn(session_id, e.seq).await?
        {
            conversation.id = boundary.session_id;
            conversation.resume = true;
            break;
        }
    }
    Ok(conversation)
}

/// A restart must not continue a turn whose work a rewind superseded. Check
/// both when recovering and when handling Resume: a rewind can land while
/// that message is waiting in the actor's mailbox.
fn resume_superseded(events: &[SessionEvent], turn: u64, pending: bool) -> bool {
    if pending {
        return true;
    }
    let Some(start) = events.iter().find(|e| matches!(&e.kind, Some(session_event::Kind::TurnStarted(t)) if t.turn == turn)) else {
        return true;
    };
    let end = events
        .iter()
        .find(|e| e.seq > start.seq && matches!(&e.kind, Some(session_event::Kind::TurnEnded(t)) if t.turn == turn))
        .map_or(u64::MAX, |e| e.seq);
    events.iter().any(|e| e.seq > start.seq && matches!(&e.kind, Some(session_event::Kind::Rewound(r)) if r.to_seq <= end))
}

/// Retained children keep their conversation and may continue working, except
/// when restoring files: even a retained writer can race the restored tree.
fn rewind_affected_events(events: &[SessionEvent], to_seq: u64, restore_files: bool) -> Vec<SessionEvent> {
    if restore_files {
        // A withdrawn child still exists and can be resumed independently.
        // Projection is not proof that its writers stopped before file restore.
        events.to_vec()
    } else {
        c6_agent::effective_events(events).into_iter().filter(|e| e.seq >= to_seq).collect()
    }
}

/// Sources removed by any rewind. Late task/subagent notices must never put
/// the withdrawn work back into the conversation, including after restart.
fn withdrawn_sources(events: &[SessionEvent]) -> HashSet<String> {
    let kept: HashSet<_> = c6_agent::effective_events(events).iter().map(|e| e.seq).collect();
    events
        .iter()
        .filter(|e| !kept.contains(&e.seq))
        .filter_map(|e| match &e.kind {
            Some(session_event::Kind::BackgroundTask(t)) if t.state == "started" => Some(t.task_id.clone()),
            Some(session_event::Kind::SubagentSpawned(s)) => Some(s.child_session_id.clone()),
            Some(session_event::Kind::NoticeQueued(n)) => Some(n.command_id.clone()),
            _ => None,
        })
        .filter(|source| !source.is_empty())
        .collect()
}

fn withdrawn_input(sources: &HashSet<String>, input: &PendingInput) -> bool {
    sources.contains(&input.command_id)
        || sources.contains(&input.from_session_id)
        || input.task_notice.as_ref().is_some_and(|n| sources.contains(&n.session_id))
        || input.command_id.strip_prefix("task:").and_then(|s| s.rsplit_once(':')).is_some_and(|(id, _)| sources.contains(id))
}

/// A missing actor after restart is not evidence that its work stopped.
/// Require a durable turn boundary, no pending notices, and closed raw tasks.
fn rewind_child_history_stopped(id: &str, events: &[SessionEvent], registry: &[c6_tools::TaskInfo]) -> anyhow::Result<()> {
    anyhow::ensure!(!registry.iter().any(|t| t.running), StopUnconfirmed(format!("Subagent {id} has running background tasks")));
    let last_work = events.iter().rev().find_map(|event| match &event.kind {
        Some(kind @ (session_event::Kind::User(_) | session_event::Kind::TurnStarted(_) | session_event::Kind::TurnEnded(_))) => Some(kind),
        _ => None,
    });
    anyhow::ensure!(
        matches!(last_work, Some(session_event::Kind::TurnEnded(end))
            if matches!(TurnEndReason::try_from(end.reason),
                Ok(TurnEndReason::Completed | TurnEndReason::Interrupted | TurnEndReason::Error))),
        StopUnconfirmed(format!("Subagent {id} has no confirmed terminal turn boundary"))
    );
    anyhow::ensure!(notices_of(events).2.is_empty(), StopUnconfirmed(format!("Subagent {id} has pending notices")));
    let mut tasks = HashMap::new();
    for event in events {
        if let Some(session_event::Kind::BackgroundTask(task)) = &event.kind {
            tasks.insert(&task.task_id, task);
        }
    }
    for task in tasks.values() {
        anyhow::ensure!(
            matches!(task.state.as_str(), "finished" | "failed" | "lost" | "stopped" | "stopped_by_user"),
            StopUnconfirmed(format!("Subagent {id} has an unclosed background task {}", task.task_id))
        );
        if task.state == "lost" && task.kind != "agent" {
            let pgid = events.iter().find_map(|event| match &event.kind {
                Some(session_event::Kind::BackgroundTask(start)) if start.task_id == task.task_id && start.state == "started" => {
                    Some(start.pgid)
                }
                _ => None,
            });
            anyhow::ensure!(
                pgid.is_some_and(
                    |pgid| pgid > 0 && nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None) == Err(nix::errno::Errno::ESRCH)
                ),
                StopUnconfirmed(format!("Subagent {id} lost background task {} without confirming its process stopped", task.task_id))
            );
        } else if task.state == "lost" {
            anyhow::ensure!(
                events.iter().any(|event| matches!(&event.kind,
                    Some(session_event::Kind::SubagentSpawned(child)) if child.background_task_id == task.task_id)),
                StopUnconfirmed(format!("Subagent {id} lost background task {} without a known child", task.task_id))
            );
        }
    }
    Ok(())
}

/// What a log says about its notices (docs/background.md §5): when notices
/// started turns (a turn whose inputs were all notices), the id of every
/// notice recorded, and the notices no turn took, with whether each wakes
/// the session.
fn notices_of(events: &[SessionEvent]) -> (Vec<i64>, Vec<String>, Vec<(PendingInput, bool)>) {
    let events = c6_agent::effective_events(events);
    let mut noticed = Vec::new();
    let mut latest = HashMap::new();
    let mut taken = HashSet::new();
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::NoticeQueued(n)) => {
                if latest.insert(n.command_id.as_str(), n).is_none() {
                    noticed.push(n.command_id.clone());
                }
            }
            Some(session_event::Kind::User(u)) => {
                taken.insert(u.command_id.clone());
            }
            _ => {}
        }
    }
    let mut auto_turns = Vec::new();
    let mut inputs: Vec<&str> = Vec::new();
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::User(u)) => inputs.push(&u.command_id),
            Some(session_event::Kind::TurnStarted(_)) => {
                if !inputs.is_empty() && inputs.iter().all(|i| latest.contains_key(*i)) {
                    auto_turns.push(e.ts_ms);
                }
                inputs.clear();
            }
            _ => {}
        }
    }
    // Scheduling snapshots update the payload, not the notice's place in line.
    let pending = noticed
        .iter()
        .filter(|id| !taken.contains(*id))
        .map(|id| {
            let n = latest[id.as_str()];
            (
                PendingInput {
                    command_id: n.command_id.clone(),
                    text: n.text.clone(),
                    command: n.command.clone(),
                    prompt: n.prompt.clone(),
                    from_session_id: n.from_session_id.clone(),
                    from_title: n.from_title.clone(),
                    hops: n.hops,
                    task_notice: n.task_notice.clone(),
                    ..Default::default()
                },
                n.wake,
            )
        })
        .collect();
    (auto_turns, noticed, pending)
}

/// Sets a spec's default for its subagents (`None`: the session's own
/// model).
fn set_subagent_default(spec: &mut CreateSession, model: Option<ModelSpec>) {
    spec.subagent_target = model;
    spec.subagent_model.clear();
}

/// Only consumed inputs in a quiescent work interval are ready to report.
/// The receipt is appended after its waiter or parent's journal took it.
fn pending_local_reports<'a>(sid: &str, events: &'a [SessionEvent]) -> Vec<(String, usize, Vec<&'a c6_proto::UserMessage>)> {
    let Some(boundary) = events.iter().position(|e| {
        matches!(&e.kind,
        Some(session_event::Kind::SubagentReported(r)) if r.command_id == local_report_tracking(sid))
    }) else {
        return Vec::new();
    };
    let received: HashSet<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Some(session_event::Kind::SubagentReported(r)) => Some(r.command_id.as_str()),
            _ => None,
        })
        .collect();
    let id = |seq| format!("subagent:local:{sid}:{seq}");
    crate::subagent::local_report_windows(events, boundary + 1, |seq| received.contains(id(seq).as_str()))
        .into_iter()
        .map(|(seq, finish, inputs)| (id(seq), finish, inputs))
        .collect()
}

/// The exact delegated inputs acknowledged by durable local report receipts.
/// Replay each receipt's preceding journal prefix: a later independent report
/// must not acknowledge earlier work, and counted work can span several turns.
/// `pending` contains raw child input command IDs, without the local-wait prefix.
/// Historical receipts preceding every outstanding input need no prefix replay.
pub fn delivered_local_report_inputs<'a>(
    child: &str,
    events: &'a [SessionEvent],
    pending: &HashSet<String>,
) -> Vec<&'a c6_proto::UserMessage> {
    let prefix = format!("subagent:local:{child}:");
    let mut delivered = Vec::new();
    let mut remaining = pending.clone();
    let Some(first) = events.iter().find_map(|event| match &event.kind {
        Some(session_event::Kind::User(input)) if remaining.contains(&input.command_id) => Some(event.seq),
        _ => None,
    }) else {
        return delivered;
    };
    for (i, event) in events.iter().enumerate() {
        let Some(session_event::Kind::SubagentReported(receipt)) = &event.kind else { continue };
        if receipt.child_session_id != child || receipt.status == "waiting" {
            continue;
        }
        let Some(end) = receipt.command_id.strip_prefix(&prefix).and_then(|seq| seq.parse::<u64>().ok()) else { continue };
        if end < first || end >= event.seq {
            continue;
        }
        if let Some((_, _, inputs)) = pending_local_reports(child, &events[..i]).into_iter().find(|(id, _, _)| id == &receipt.command_id) {
            delivered.extend(inputs.into_iter().filter(|input| !input.delegated_by.is_empty() && remaining.remove(&input.command_id)));
            if remaining.is_empty() {
                break;
            }
        }
    }
    delivered
}

/// A successful channel send is not durable delivery. On recovery, look
/// for the call's real result or its background notice before retiring a
/// report. A placeholder saying the child is still running/lost does not
/// acknowledge a later completed report.
fn report_recorded(events: &[SessionEvent], call: &str, child: &str, status: &str) -> bool {
    report_recorded_for(events, call, child, status, None)
}

fn report_recorded_for(events: &[SessionEvent], call: &str, child: &str, status: &str, task: Option<&str>) -> bool {
    if call.is_empty() || child.is_empty() {
        return false;
    }
    if events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Rewound(_)))) {
        // Project the remaining branch without cloning messages or tool output.
        let mut kept: Vec<&SessionEvent> = Vec::with_capacity(events.len());
        let mut segment = 0;
        for event in events {
            match &event.kind {
                Some(session_event::Kind::Started(_)) => {
                    segment = kept.len();
                    kept.push(event);
                }
                Some(session_event::Kind::Rewound(r)) => {
                    kept.truncate(segment + kept[segment..].partition_point(|e| e.seq < r.to_seq));
                }
                _ => kept.push(event),
            }
        }
        return report_recorded_in(kept.iter().copied(), call, child, status, task);
    }
    report_recorded_in(events.iter(), call, child, status, task)
}

fn call_invocation<'a>(kind: Option<&'a session_event::Kind>, call: &str) -> Option<&'a str> {
    let Some(session_event::Kind::Assistant(a)) = kind else { return None };
    a.blocks.iter().find_map(|b| match &b.block {
        Some(c6_proto::content_block::Block::ToolUse(u)) if u.id == call => Some(u.name.as_str()),
        _ => None,
    })
}

fn background_report_task(output: &str) -> Option<&str> {
    let task = if let Some(rest) = output.strip_prefix("Started the subagent in the background as ") {
        rest.strip_suffix(". Its report will arrive as a notice when it finishes; continue with other work meanwhile.")?
    } else {
        output
            .strip_prefix("Continuing the subagent in the background as ")?
            .strip_suffix(". Its report will arrive as a notice when it finishes.")?
    };
    background_task_id(task)
}

fn background_task_id(task: &str) -> Option<&str> {
    let number = task.strip_prefix("bg")?;
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then_some(task)
}

fn task_completion(command_id: &str) -> Option<(&str, &str)> {
    let (task, state) = command_id.strip_prefix("task:")?.rsplit_once(':')?;
    matches!(state, "finished" | "failed" | "lost" | "stopped_by_user").then_some((task, state))
}

fn grouped_run_key(s: &c6_proto::SubagentSpawned) -> (&str, &str) {
    (&s.child_session_id, &s.background_task_id)
}

fn background_report_owner<'a>(events: impl Iterator<Item = &'a SessionEvent>, call: &str, task: &str) -> Result<Option<&'a str>, ()> {
    let mut owners = events.filter_map(|e| match &e.kind {
        Some(session_event::Kind::SubagentSpawned(s)) if s.call_id == call && s.background_task_id == task => {
            Some(s.child_session_id.as_str())
        }
        _ => None,
    });
    let owner = owners.next();
    if owner.is_some_and(|id| id.is_empty() || owners.any(|other| other != id)) { Err(()) } else { Ok(owner) }
}

fn background_report_recorded<'a>(mut events: impl Iterator<Item = &'a SessionEvent>, task: &str, status: &str) -> bool {
    let prefix = format!("task:{task}:");
    events.any(|e| {
        matches!(&e.kind, Some(session_event::Kind::NoticeQueued(n)) if n.command_id.strip_prefix(&prefix).is_some_and(|state| {
            matches!(state, "finished" | "failed" | "stopped_by_user") || (state == "lost" && status == "lost")
        }))
    })
}

fn report_recorded_in<'a>(
    events: impl DoubleEndedIterator<Item = &'a SessionEvent> + Clone,
    call: &str,
    child: &str,
    status: &str,
    task: Option<&str>,
) -> bool {
    if let Some(task) = task {
        // The child's input already identifies the exact task. MCP tool
        // results need not echo our launch placeholder, and may arrive late.
        return background_report_owner(events.clone(), call, task) == Ok(Some(child)) && background_report_recorded(events, task, status);
    }
    // Some providers (and the scripted model) reuse ids across turns.
    // Only an explicit link identifies an older invocation. A newer
    // unlinked send_message may have continued this very child: don't let
    // the earlier linked result acknowledge that newer call.
    let (mut latest, mut has_link, mut linked_here, mut result) = (true, false, false, None);
    for event in events.clone().rev().map(Some).chain(std::iter::once(None)) {
        let kind = event.and_then(|e| e.kind.as_ref());
        match kind {
            Some(session_event::Kind::ToolResult(r)) if r.call_id == call && result.is_none() => result = Some(r),
            Some(session_event::Kind::SubagentSpawned(s))
                if s.call_id == call && !s.child_session_id.is_empty() && s.background_task_id.is_empty() =>
            {
                has_link = true;
                linked_here |= s.child_session_id == child;
            }
            _ => {
                let invocation = call_invocation(kind, call);
                if event.is_some() && invocation.is_none() {
                    continue;
                }
                let output = result.map(|r| r.output.as_str());
                // The report body can quote another footer or even a
                // background placeholder. Only its generated last line counts.
                let footer = output.and_then(|text| {
                    text.lines().next_back()?.strip_prefix("[subagent session ")?.strip_suffix(']').filter(|id| !id.is_empty())
                });
                if let Some(task) = output.filter(|_| footer.is_none()).and_then(background_report_task) {
                    // Background links are appended by a spawned future;
                    // their position does not identify an invocation. Join
                    // the exact task in the placeholder to its durable owner.
                    let owner = background_report_owner(events.clone(), call, task);
                    if owner == Ok(None) && matches!(invocation, Some("task" | "Task")) {
                        // A new Task cannot be a continuation of the child
                        // we're already tracking, even before its link lands.
                        (latest, has_link, linked_here, result) = (false, false, false, None);
                        continue;
                    }
                    let Ok(Some(owner)) = owner else {
                        return false;
                    };
                    if owner == child {
                        return background_report_recorded(events, task, status);
                    }
                    (latest, has_link, linked_here, result) = (false, false, false, None);
                    continue;
                }
                let matches_result = if let Some(footer) = footer {
                    footer == child
                } else {
                    output.is_some_and(|text| text.contains(&format!("<subagent id=\"{child}\" status=\"{status}\"")))
                };
                if linked_here {
                    return matches_result;
                }
                if !has_link {
                    if latest && matches_result {
                        return true;
                    }
                    // A Task always starts a new child, even if its link has
                    // not been recorded yet. An unlinked continuation is
                    // ambiguous unless a successful send identifies a sibling.
                    let sibling = matches!(invocation, Some("send_message" | "SendMessage"))
                        && result.is_some_and(|r| !r.is_error)
                        && footer.is_some_and(|id| id != child);
                    if !matches!(invocation, Some("task" | "Task")) && !sibling {
                        return false;
                    }
                }
                (latest, has_link, linked_here, result) = (false, false, false, None);
            }
        }
    }
    false
}

struct LocalReportCall {
    id: String,
    task: Option<String>,
}

impl LocalReportCall {
    fn of(input: &c6_proto::UserMessage) -> Option<Self> {
        if input.delegated_by.is_empty() {
            return None;
        }
        // Old and foreground input ids have no task suffix.
        let task =
            input.command_id.strip_prefix("delegation:").and_then(|id| id.split_once(':')).and_then(|(_, task)| background_task_id(task));
        Some(Self { id: input.delegated_by.clone(), task: task.map(str::to_owned) })
    }

    fn recorded(&self, events: &[SessionEvent], child: &str, status: &str) -> bool {
        report_recorded_for(events, &self.id, child, status, self.task.as_deref())
    }
}

/// The original call may record its result while the fallback is in flight.
pub(crate) struct InlineChildReport {
    child: String,
    status: &'static str,
    calls: Vec<LocalReportCall>,
    after: u64,
}

impl InlineChildReport {
    fn check_after(calls: &[LocalReportCall], events: &[SessionEvent], cursor: u64) -> u64 {
        events
            .iter()
            .find(|e| {
                calls.iter().any(|call| {
                    call_invocation(e.kind.as_ref(), &call.id).is_some()
                        || matches!(&e.kind, Some(session_event::Kind::SubagentSpawned(s)) if s.call_id == call.id)
                        || matches!(&e.kind, Some(session_event::Kind::ToolResult(r)) if r.call_id == call.id)
                })
            })
            .map_or(cursor, |e| e.seq.saturating_sub(1))
    }

    fn recorded(&self, events: &[SessionEvent]) -> bool {
        self.calls.iter().any(|call| call.recorded(events, &self.child, self.status))
    }

    fn background_pending(&self, events: &[SessionEvent], retained: impl Fn(&str) -> bool) -> bool {
        self.calls.iter().any(|call| {
            let Some(task) = call.task.as_deref() else { return false };
            // finish_agent sets its exit status before the actor receives
            // the callback. Even a finished snapshot still owns the report
            // until the actor handles that callback. After a terminal row,
            // a missing/mismatched notice can fall back (e.g. after loss).
            background_report_owner(events.iter(), &call.id, task) == Ok(Some(self.child.as_str())) && retained(task)
        })
    }

    fn still_running(&self, parent: &Handle, evidence: &LocalReportParent) -> bool {
        if parent.tx.is_closed() {
            return false;
        }
        let events = &evidence.events;
        if self.background_pending(events, |task| {
            !evidence.finished_tasks.contains(task) && parent.tools.background.snapshot(task).is_some_and(|t| t.kind == "agent")
        }) || events.iter().any(|e| {
            matches!(&e.kind, Some(session_event::Kind::SubagentSpawned(s))
                if s.child_session_id == self.child && self.calls.iter().any(|call| call.task.is_none() && call.id == s.call_id)
                && !s.background_task_id.is_empty()
                && parent.tools.background.snapshot(&s.background_task_id).is_some_and(|t| t.running))
        }) {
            return true;
        }
        if !parent.running.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        // A later turn can already be running after recovery. Only the
        // open turn that actually contains the call may still report it.
        events
            .iter()
            .rev()
            .take_while(|e| !matches!(e.kind, Some(session_event::Kind::TurnStarted(_) | session_event::Kind::TurnEnded(_))))
            .any(|e| {
                matches!(&e.kind, Some(session_event::Kind::Assistant(a)) if a.blocks.iter().any(|b| {
                    matches!(&b.block, Some(c6_proto::content_block::Block::ToolUse(u)) if self.calls.iter().any(|call| call.task.is_none() && call.id == u.id))
                }))
            })
    }
}

/// Only receipt evidence is kept; subsequent polls read newly appended rows.
#[derive(Default)]
struct LocalReportParent {
    after: u64,
    calls: HashSet<String>,
    events: Vec<SessionEvent>,
    // Unlike receipts, whether the callback ran is a fact about the raw
    // log. A rewind may remove the notice but cannot make it in flight again.
    finished_tasks: HashSet<String>,
}

impl LocalReportParent {
    async fn update(&mut self, journal: &Journal, parent: &str, child: &str) -> anyhow::Result<bool> {
        let notices = format!("subagent:local:{child}:");
        // Drop the read connection between pages so initial recovery cannot
        // monopolize it. Bound one attempt even if the parent keeps writing.
        const PAGE: u32 = 128;
        for _ in 0..128 {
            let page = journal.events_after(parent, self.after, PAGE).await?;
            let done = page.len() < PAGE as usize;
            for mut event in page {
                self.after = event.seq;
                let keep = match &mut event.kind {
                    Some(session_event::Kind::Assistant(a)) => {
                        a.blocks.retain_mut(|b| match &mut b.block {
                            Some(c6_proto::content_block::Block::ToolUse(u)) if self.calls.contains(&u.id) => {
                                u.input_json = String::new();
                                true
                            }
                            _ => false,
                        });
                        !a.blocks.is_empty()
                    }
                    Some(session_event::Kind::SubagentSpawned(s)) if self.calls.contains(&s.call_id) => {
                        *s = c6_proto::SubagentSpawned {
                            call_id: std::mem::take(&mut s.call_id),
                            child_session_id: std::mem::take(&mut s.child_session_id),
                            background_task_id: std::mem::take(&mut s.background_task_id),
                            ..Default::default()
                        };
                        true
                    }
                    Some(session_event::Kind::Rewound(r)) => {
                        self.events.truncate(self.events.partition_point(|e| e.seq < r.to_seq));
                        true
                    }
                    Some(session_event::Kind::ToolResult(r)) => self.calls.contains(&r.call_id),
                    Some(session_event::Kind::BackgroundTask(t)) if t.kind == "agent" && t.state != "started" => {
                        self.finished_tasks.insert(std::mem::take(&mut t.task_id));
                        false
                    }
                    Some(session_event::Kind::NoticeQueued(n))
                        if n.command_id.starts_with(&notices) || n.command_id.starts_with("task:") =>
                    {
                        *n = c6_proto::NoticeQueued { command_id: std::mem::take(&mut n.command_id), ..Default::default() };
                        true
                    }
                    Some(session_event::Kind::TurnStarted(_) | session_event::Kind::TurnEnded(_)) => true,
                    _ => false,
                };
                if keep {
                    self.events.push(event);
                }
            }
            if done {
                return Ok(true);
            }
            tokio::task::yield_now().await;
        }
        Ok(false)
    }
}

fn local_report_tracking(sid: &str) -> String {
    format!("subagent:local:{sid}:tracking")
}

fn local_report_boundary(sid: &str) -> c6_proto::SubagentReported {
    c6_proto::SubagentReported {
        command_id: local_report_tracking(sid),
        child_session_id: sid.into(),
        status: "tracking".into(),
        ..Default::default()
    }
}

/// How the log's last turn ended: its reason, final assistant text and
/// last error.
fn last_turn_outcome(events: &[SessionEvent]) -> Outcome {
    let start = events.iter().rposition(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_)))).unwrap_or(0);
    let mut out = Outcome { reason: TurnEndReason::Unspecified, report: String::new(), error: String::new() };
    for e in &events[start..] {
        match &e.kind {
            Some(session_event::Kind::Assistant(a)) => {
                let text: Vec<&str> = a
                    .blocks
                    .iter()
                    .filter_map(|b| match &b.block {
                        Some(c6_proto::content_block::Block::Text(t)) if !t.trim().is_empty() => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    out.report = text.join("\n");
                }
            }
            Some(session_event::Kind::Error(err)) => out.error = err.message.clone(),
            Some(session_event::Kind::TurnEnded(t)) => out.reason = TurnEndReason::try_from(t.reason).unwrap_or(TurnEndReason::Unspecified),
            _ => {}
        }
    }
    out
}

fn local_report_outcome(events: &[SessionEvent]) -> Outcome {
    let mut outcome = last_turn_outcome(events);
    crate::subagent::local_background_outcome(events, &mut outcome);
    outcome
}

/// A side chat's branched-from conversation, made portable when the side
/// chat runs on a different model than produced any of it.
fn seed_events(spec: &CreateSession) -> Vec<SessionEvent> {
    let model = spec.model.clone().unwrap_or_default();
    let same_model = spec.seed.iter().all(|e| match &e.kind {
        Some(session_event::Kind::Started(s)) => {
            s.model.as_ref().is_some_and(|m| m.connection_id == model.connection_id && m.model == model.model)
        }
        _ => true,
    });
    if same_model { spec.seed.clone() } else { c6_agent::portable_events(&spec.seed) }
}

/// The outcome of a queue operation (edit or cancel) on one of a session's
/// inputs waiting in its inbox.
pub(crate) enum QueueOp {
    /// The input was in the inbox; its payload was replaced.
    Updated,
    /// The input was in the inbox and is gone now.
    Cancelled,
    /// Not in the inbox (delivered, or never here).
    Absent,
}

/// Inputs waiting for the next model-request boundary.
#[derive(Default)]
struct Inbox {
    /// Inputs with the model switch each asks for.
    queue: VecDeque<(PendingInput, Option<ModelSpec>)>,
    /// Inputs lent to a turn (or its Claude Code process) but not yet logged.
    in_flight: VecDeque<PendingInput>,
    /// Every command id ever queued here, so redeliveries are ignored even
    /// while an input is being recorded.
    seen: HashSet<String>,
    /// Told when an input is queued (a running Claude Code turn writes it
    /// to its process).
    more: Arc<tokio::sync::Notify>,
    /// Inputs already with the agent to deliver now (Send now), by command
    /// id: for a running Claude Code turn.
    now: Vec<String>,
}

impl Inbox {
    fn push(&mut self, input: PendingInput, model: Option<ModelSpec>) {
        if self.seen.insert(input.command_id.clone()) {
            self.queue.push_back((input, model));
            self.more.notify_one();
        }
    }

    /// Send now for an input already with the agent.
    fn send_now(&mut self, command_id: String) {
        self.now.push(command_id);
        self.more.notify_one();
    }

    /// What joins a running Claude Code turn: the inputs that may (`join`),
    /// and those with it already to deliver now.
    fn joining(&mut self) -> crate::claude_code::Joining {
        crate::claude_code::Joining { inputs: self.join(), now: std::mem::take(&mut self.now) }
    }

    /// Inputs a process never took, first again.
    fn requeue(&mut self, inputs: Vec<PendingInput>) {
        for input in inputs.into_iter().rev() {
            self.recorded(&input.command_id);
            self.queue.push_front((input, None));
        }
    }

    fn recorded(&mut self, command_id: &str) {
        self.in_flight.retain(|input| input.command_id != command_id);
    }

    /// User events are written before TurnStarted. If that boundary failed,
    /// these inputs still need an outcome (especially a waiting delegation).
    fn has_unstarted_input(&self, events: &[SessionEvent]) -> bool {
        events
            .iter()
            .rev()
            .take_while(|e| !matches!(e.kind, Some(session_event::Kind::TurnEnded(_) | session_event::Kind::Rewound(_))))
            .any(|e| match &e.kind {
                Some(session_event::Kind::User(u)) => !u.command_id.is_empty() && u.blocked.is_empty() && self.seen.contains(&u.command_id),
                _ => false,
            })
    }

    /// A failed turn was joined before recovery, so it cannot take more
    /// inputs. Replay only the ones that have no User receipt. An idle CLI
    /// we still own can confirm its own outstanding inputs as usual.
    fn recover(&mut self, taken: &HashSet<&str>, withdrawn: &HashSet<String>, idle_cli_owns_inputs: bool) {
        self.seen.extend(taken.iter().filter(|id| !id.is_empty()).map(|id| (*id).to_owned()));
        let pending = |i: &PendingInput| !taken.contains(i.command_id.as_str()) && !withdrawn_input(withdrawn, i);
        self.queue.retain(|(i, _)| pending(i));
        self.in_flight.retain(pending);
        if !idle_cli_owns_inputs {
            let retry = std::mem::take(&mut self.in_flight);
            for input in retry.into_iter().rev() {
                self.queue.push_front((input, None));
            }
            self.now.clear();
        }
    }

    /// Inputs that may join a running turn: none while a `/compact` or a
    /// model switch is next.
    fn join(&mut self) -> Vec<PendingInput> {
        if self.queue.front().is_some_and(|(i, m)| m.is_some() || i.is_compact()) {
            return Vec::new();
        }
        self.drain()
    }

    /// Keep the requested switch on the entry until it has been recorded.
    fn next_switch(&self) -> Option<(String, ModelSpec)> {
        self.queue.front().and_then(|(i, m)| m.clone().map(|model| (i.command_id.clone(), model)))
    }

    fn finish_switch(&mut self, command_id: &str) {
        // A UI cancellation can remove that input while the journal writes.
        if let Some((_, model)) = self.queue.iter_mut().find(|(i, _)| i.command_id == command_id) {
            *model = None;
        }
    }

    /// Swap the input's payload for new text (the head edited a queued
    /// message). Its model switch stays: switching is a separate concern.
    /// The id stays `seen`, so a redelivery of the same id is still
    /// ignored.
    fn replace(&mut self, command_id: &str, input: PendingInput) -> QueueOp {
        let Some(i) = self.queue.iter().position(|(i, _)| i.command_id == command_id) else {
            return QueueOp::Absent;
        };
        self.queue[i].0 = input;
        QueueOp::Updated
    }

    /// Drop the input (the head canceled a queued message). The id stays
    /// `seen`, so a redelivery of the same id cannot push it back.
    fn remove(&mut self, command_id: &str) -> QueueOp {
        let Some(i) = self.queue.iter().position(|(i, _)| i.command_id == command_id) else {
            return QueueOp::Absent;
        };
        self.queue.remove(i);
        QueueOp::Cancelled
    }

    /// Inputs up to the next model switch or `/compact`: those wait for a
    /// turn of their own rather than joining a running one. A `/compact`
    /// at the front is its own turn.
    fn drain(&mut self) -> Vec<PendingInput> {
        let n = if self.queue.front().is_some_and(|(i, m)| i.is_compact() && m.is_none()) {
            1
        } else {
            self.queue.iter().position(|(i, m)| m.is_some() || i.is_compact()).unwrap_or(self.queue.len())
        };
        let inputs: Vec<_> = self.queue.drain(..n).map(|(i, _)| i).collect();
        self.in_flight.extend(inputs.iter().cloned());
        inputs
    }
}

type TaskFenceSender = mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>;

/// Keep input ownership in the actor until the turn's User append commits.
/// Both turn implementations also accept inputs mid-turn through callbacks.
struct TurnSink {
    writer: Arc<Writer>,
    inbox: Arc<Mutex<Inbox>>,
    task_fence: Option<TaskFenceSender>,
}

/// Background starts are sent synchronously by the tool before it returns,
/// but normally make an asynchronous hop to the actor. Drain that hop before
/// forwarding a fence. The actor acks only after journaling the callbacks
/// ahead of it, so the head can trust its task projection at TurnEnded.
async fn forward_tasks(
    mut tasks: mpsc::UnboundedReceiver<c6_tools::TaskEvent>,
    mut fences: mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
    actor: mpsc::UnboundedSender<ActorMsg>,
) {
    loop {
        let msg = tokio::select! {
            event = tasks.recv() => {
                let Some(event) = event else { return };
                ActorMsg::Task(event)
            }
            fence = fences.recv() => {
                let Some(fence) = fence else { return };
                while let Ok(event) = tasks.try_recv() {
                    if actor.send(ActorMsg::Task(event)).is_err() {
                        return;
                    }
                }
                ActorMsg::TaskFence(fence)
            }
        };
        if actor.send(msg).is_err() {
            return;
        }
    }
}

#[async_trait]
impl EventSink for TurnSink {
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
        if matches!(&kind, session_event::Kind::TurnEnded(_))
            && let Some(fence) = &self.task_fence
        {
            let (ack, ready) = tokio::sync::oneshot::channel();
            fence.send(ack).map_err(|_| anyhow::anyhow!("background task relay stopped before the delegation ended"))?;
            ready.await?;
        }
        self.writer.observe_activity(&kind);
        let input = match &kind {
            session_event::Kind::User(u) => Some(u.command_id.clone()),
            _ => None,
        };
        let seq = self.writer.append(kind).await?;
        if let Some(id) = input {
            self.inbox.lock().unwrap().recorded(&id);
        }
        Ok(seq)
    }

    async fn start_turn(&self, turn: u64, inputs: &[PendingInput]) -> anyhow::Result<()> {
        self.writer.start_turn(turn, inputs).await?;
        let mut inbox = self.inbox.lock().unwrap();
        for input in inputs {
            inbox.recorded(&input.command_id);
        }
        Ok(())
    }

    fn ephemeral(&self, kind: ephemeral_event::Kind) {
        self.writer.ephemeral(kind);
    }
}

struct Actor {
    spec: CreateSession,
    launch_pressure: Arc<crate::resource_pressure::Gate>,
    /// Disk admission is for a new child, not subsequent turns or recovery.
    disk_admitted: bool,
    started: SessionStarted,
    session_tools: Arc<SessionTools>,
    hooks: Option<Arc<crate::hooks::HookRunner>>,
    policies: Option<Arc<crate::policy::Policies>>,
    /// `None` while a turn task holds it.
    conv: Option<Conversation>,
    turns: u64,
    writer: Arc<Writer>,
    task_fence: TaskFenceSender,
    link: Arc<HeadLink>,
    journal: Arc<Journal>,
    tools: Vec<Arc<dyn Tool>>,
    inbox: Arc<Mutex<Inbox>>,
    checkpoints: Arc<Checkpoints>,
    /// Idle merge window. Tokio's clock is injectable in scheduler tests.
    notice_arrivals: HashMap<String, tokio::time::Instant>,
    notice_deadline: Option<tokio::time::Instant>,
    notice_ready: HashSet<String>,
    notice_pause: Option<String>,
    notice_warning: Option<String>,
    /// Notices waiting for a merge/group barrier or explicit input, and
    /// whether each may start an unattended turn.
    deferred: Vec<(PendingInput, bool)>,
    /// Notices whose first durable queue write failed, never mixed with the
    /// held (already durable) batch until their own marker is recorded.
    unrecorded_notices: Vec<(PendingInput, bool)>,
    /// The callback already removed from the mailbox when its write failed.
    /// All later callbacks remain in the supervisor's unchanged mailbox.
    pending_task_event: Option<PendingTaskEvent>,
    /// Rewinds admitted to this actor remain a barrier across journal recovery.
    rewinds: VecDeque<(String, u64, bool)>,
    /// Every notice recorded in the log (`NoticeQueued`), by command id.
    noticed: HashSet<String>,
    /// The session secrets and head calls are asked for on behalf of: its
    /// own, or a subagent's top-level session.
    on_behalf_of: String,
    /// Delegations waiting for their turn to end: the input's command id.
    waiting: Vec<(String, tokio::sync::oneshot::Sender<Outcome>)>,
    /// At most one outstanding fallback per report, even if the parent is
    /// stuck in recovery. The child never waits inline for its parent.
    local_report_acks: HashMap<String, tokio::sync::oneshot::Receiver<()>>,
    local_report_retry_at: Option<tokio::time::Instant>,
    local_report_parent: LocalReportParent,
    remote_receipts: Vec<c6_proto::SubagentReported>,
    /// To start subagents.
    manager: SessionManager,
    /// The default for its subagents (as in `spec`), shared with a running
    /// turn's subagent runner.
    subagent_default: crate::subagent::SharedDefault,
    // The active turn can make another Task call after settings change.
    launch_limits: Arc<Mutex<crate::subagent::LaunchLimits>>,
    /// Shared with the manager: a turn is running.
    running_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the manager: it runs Claude Code now.
    claude_code: Arc<std::sync::atomic::AtomicBool>,
    /// Its Claude Code process while no turn holds it (docs/background.md
    /// §4), and that process's input.
    claude: Option<crate::claude_code::Process>,
    withdrawn: HashSet<String>,
    claude_stdin: crate::claude_code::Stdin,
    /// The log says a switch away from Claude Code waits for its tasks.
    handoff_announced: bool,
    /// You stopped the running Claude Code turn, and until when after it
    /// ended its tasks lost with it are the stop's doing: their notices
    /// wait for your next message instead of waking the agent
    /// (docs/background.md §4, Stopping).
    stopping: bool,
    stopped_until: Option<std::time::Instant>,
}

/// A subagent's parent's tools, and whether the parent runs Claude Code.
type ParentTools = (Arc<SessionTools>, bool);

/// What a turn task gives back: the conversation, and the Claude Code
/// process if it is still up. `None`: the turn failed to record itself.
type TurnTask = tokio::task::JoinHandle<Option<(Conversation, Option<crate::claude_code::Process>)>>;

enum CacheWarmAction {
    Due { expires_ms: u64 },
    Recorded,
    Ineligible,
}

/// A timer or a single native request. Dropping the actor (including recovery
/// and moves) cancels it; no detached timer can ping a previous owner.
struct CacheWarmJob(tokio::task::JoinHandle<anyhow::Result<CacheWarmAction>>);

impl Drop for CacheWarmJob {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, PartialEq)]
struct CacheWarmKey {
    call_ms: i64,
    wait_ms: i64,
    connection_id: String,
    model: String,
    settings: c6_api::KeepWarmSettings,
}

fn cache_warm_timer(
    key: CacheWarmKey,
    kind: String,
    observation: CacheObservation,
    writer: Arc<Writer>,
    gate: Arc<RecoveryGate>,
) -> CacheWarmJob {
    CacheWarmJob(tokio::spawn(async move {
        use crate::keep_warm::{CacheEconomics, KeepWarmInput, WaitingOn};
        let Some((_, usage)) = observation.usage else { return Ok(CacheWarmAction::Ineligible) };
        // The head owns learning; a temporary disconnect retains conservative
        // provider defaults. Compatible/local endpoints have no such default.
        let learned = writer
            .link
            .call(
                "session.cache_lifetime",
                serde_json::json!({
                    "session_id": writer.session_id,
                    "connection_id": key.connection_id,
                    "model": key.model,
                }),
            )
            .await
            .ok();
        let ttl = learned.as_ref().and_then(|v| v["lifetime_seconds"].as_u64()).or(match kind.as_str() {
            "claude_code" => Some(3_600),
            "anthropic" => Some(300),
            "openai" | "chatgpt" => Some(600),
            _ => None,
        });
        let observed = usage.cache_read_tokens > 0
            || learned.as_ref().is_some_and(|v| v["cache_observed"].as_bool() == Some(true))
            || matches!(kind.as_str(), "claude_code" | "anthropic" | "openai" | "chatgpt");
        let ratio = learned.as_ref().and_then(|v| v["read_to_rewrite_ratio"].as_f64()).or(match kind.as_str() {
            "claude_code" | "anthropic" | "chatgpt" => Some(0.1),
            // An unknown OpenAI model may discount cache reads by only half.
            "openai" => Some(0.5),
            _ => None,
        });
        let economics = learned
            .as_ref()
            .and_then(|v| serde_json::from_value::<c6_api::Price>(v["price"].clone()).ok())
            .and_then(|price| CacheEconomics::from_price(&price, observed))
            .or_else(|| ratio.map(|read_to_rewrite_ratio| CacheEconomics { read_to_rewrite_ratio, cache_observed: observed }));
        // Usage can be published long after a request began (especially by
        // Claude Code). Never count generation time as fresh cache lifetime.
        let last_call_ms = if usage.keep_warm || kind == "claude_code" {
            observation.last_call_started_ms
        } else {
            writer.journal.keep_warm_call_ms(&writer.session_id).await?
        };
        let Some(last_call_ms) = last_call_ms else { return Ok(CacheWarmAction::Ineligible) };
        let last_call_ms = last_call_ms.min(key.call_ms).max(0) as u64;
        let input = KeepWarmInput {
            connection_id: &key.connection_id,
            waiting_on: WaitingOn::Background,
            context_tokens: crate::keep_warm::context_tokens(&c6_llm::Usage {
                input_tokens: usage.input_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                ..Default::default()
            }),
            lifetime_seconds: ttl,
            last_call_ms,
            wait_start_ms: key.wait_ms.max(0) as u64,
            now_ms: writer.now_ms().max(0) as u64,
            pings_sent: observation.pings_sent,
            projected_remaining_seconds: None,
            economics,
        };
        let Some(due_ms) = crate::keep_warm::schedule(&key.settings, &input) else { return Ok(CacheWarmAction::Ineligible) };
        gate.keep_warm_next_ms.store(due_ms.min(i64::MAX as u64) as i64, Ordering::Relaxed);
        let delay = due_ms.saturating_sub(writer.now_ms().max(0) as u64);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        Ok(CacheWarmAction::Due { expires_ms: last_call_ms.saturating_add(ttl.unwrap_or(0).saturating_mul(1_000)) })
    }))
}

struct PendingTaskEvent {
    record: BackgroundTask,
    notice: Option<String>,
    output: String,
    wake: bool,
    logged: bool,
}

struct CancelShellContexts(Arc<SessionTools>);

impl Drop for CancelShellContexts {
    fn drop(&mut self) {
        // An unrelated actor failure can also strand an in-flight tool call.
        // Recovery registers fresh turn contexts; old callbacks stay canceled.
        self.0.background.cancel_shell_contexts();
    }
}

fn recover_rewind_queue(
    claude_code: bool,
    mut retained: VecDeque<(String, u64, bool)>,
    reset: Option<(crate::claude_code::Reset, u64)>,
) -> VecDeque<(String, u64, bool)> {
    let recovered = match reset {
        Some((r, 0)) if r.failure.is_empty() || !r.retries.is_empty() => {
            let mut pending = VecDeque::from([(r.command_id, r.to_seq, r.restore_files)]);
            pending.extend(r.retries.into_iter().map(|id| (id, r.to_seq, r.restore_files)));
            pending
        }
        _ => VecDeque::new(),
    };
    if claude_code {
        // Only durable authorization can retry a failed reset. Confirm the
        // original first so its confirmation also retires every retry receipt.
        return recovered;
    }
    for rewind in recovered.into_iter().rev() {
        if !retained.iter().any(|(id, _, _)| id == &rewind.0) {
            retained.push_front(rewind);
        }
    }
    retained
}

impl Actor {
    fn cache_warm_candidate(&self) -> Option<(CacheWarmKey, String, CacheObservation)> {
        let settings = self
            .spec
            .keep_warm_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<c6_api::KeepWarmSettings>(json).ok())
            .unwrap_or_default();
        let model = self.spec.model.as_ref()?;
        if !settings.enabled || settings.disabled_connections.contains(&model.connection_id) {
            return None;
        }
        let waiting = crate::keep_warm::waiting_on(
            &self.session_tools.background.list(),
            self.claude.as_ref().is_some_and(|process| process.tasks_running()),
            self.running_flag.load(Ordering::Acquire),
        );
        if waiting != crate::keep_warm::WaitingOn::Background {
            return None;
        }
        let observation = self.writer.cache_observation.lock().unwrap().clone();
        let (call_ms, usage) = observation.usage.as_ref()?;
        let context = usage.input_tokens.saturating_add(usage.cache_read_tokens).saturating_add(usage.cache_write_tokens);
        if context == 0
            || context < settings.threshold_tokens
            || (usage.keep_warm && usage.cache_read_tokens < context.saturating_sub(context / 5))
        {
            // An eviction or a provider that stopped honoring caching can
            // turn a ping into a rewrite. Do not budget subsequent pings as
            // cheap reads after observing that expensive outcome.
            return None;
        }
        let connection_id =
            if usage.resolved_connection_id.is_empty() { model.connection_id.clone() } else { usage.resolved_connection_id.clone() };
        if settings.disabled_connections.contains(&connection_id) {
            return None;
        }
        let key =
            CacheWarmKey { call_ms: *call_ms, wait_ms: observation.wait_started_ms?, connection_id, model: usage.model.clone(), settings };
        Some((key, model.connection_kind.clone(), observation))
    }

    /// Keep the mailbox and tool sinks alive while the journal is unavailable.
    /// In particular, background callbacks cannot be recovered by head input
    /// redelivery. The old turn must finish dropping before its cursor is reset.
    async fn supervise(mut self, mut rx: mpsc::UnboundedReceiver<ActorMsg>, gate: Arc<RecoveryGate>) {
        let mut running = None;
        loop {
            let mut recoverable = true;
            self.run(&mut rx, &mut running, &mut recoverable, &gate).await;
            gate.active.store(recoverable && !rx.is_closed(), std::sync::atomic::Ordering::Release);
            if let Some((task, cancel)) = running.take() {
                cancel.cancel();
                task.abort();
                let _ = task.await;
            }
            self.running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
            if rx.is_closed() || !recoverable {
                return;
            }
            self.recover_supervised(&gate).await;
            // Time spent in deliberate recovery is not mailbox silence.
            gate.probe.lock().unwrap().pending = None;
            gate.active.store(false, std::sync::atomic::Ordering::Release);
        }
    }

    async fn recover_supervised(&mut self, gate: &Arc<RecoveryGate>) {
        let budget = recovery_budget(self.spec.session_recovery_seconds);
        let mut deadline = Some(tokio::time::Instant::now() + budget);
        let mut backoff = Duration::from_millis(100);
        let (disk_initial, disk_max) = crate::disk::retry_delays(&self.spec);
        let mut disk_backoff = disk_initial;
        let mut disk_wait = None::<DiskWait>;
        let mut disk_was_full = false;
        let disk = self.manager.disk.clone();
        let state_dir = self.manager.state_dir.clone();
        disk.configure(&self.spec);
        let mut cli_retired = false;
        let mut pause = None::<RecoveryPause>;
        let writer = self.writer.clone();
        let mut reason = "A recovery attempt has not completed".to_owned();
        if writer.resource_failure.swap(false, Ordering::AcqRel) {
            // Preserve the first write's classification: checking out a new
            // SQLite connection may take longer than a short recovery bound.
            // That must not expose a manual stop while disk space is absent.
            let reason = disk.waiting_reason(&state_dir);
            disk_wait = Some(DiskWait::new(gate.clone(), &writer, reason));
            disk_was_full = true;
        }
        loop {
            let result = if let Some(waiting) = &disk_wait {
                crate::disk::wait(&disk, &state_dir, disk_backoff, disk_was_full, |space| waiting.update(space)).await;
                disk_backoff = disk_backoff.saturating_mul(2).min(disk_max);
                Some(self.recover_once(&mut cli_retired).await)
            } else if let Some(until) = deadline {
                let attempt = async {
                    tokio::time::sleep_until(until.min(tokio::time::Instant::now() + backoff)).await;
                    // A deadline during backoff must not start another retry.
                    if tokio::time::Instant::now() >= until {
                        return None;
                    }
                    Some(self.recover_once(&mut cli_retired).await)
                };
                recovery_at_deadline(until, attempt, || {
                    pause.get_or_insert_with(|| RecoveryPause::new(gate.clone(), &writer, &reason));
                })
                .await
            } else {
                let paused = pause.as_ref().expect("manual action requires an escalation");
                match paused.wait_action().await {
                    RecoveryAction::Retry => Some(self.recover_once(&mut cli_retired).await),
                    RecoveryAction::Restart => {
                        // Recheck retirement from scratch, not from a previous
                        // attempt's in-memory checkpoint. The actor, mailbox,
                        // and journal stay; ownership barriers still apply.
                        cli_retired = false;
                        deadline = Some(tokio::time::Instant::now() + budget);
                        backoff = Duration::ZERO;
                        continue;
                    }
                }
            };
            match result {
                Some(Ok(())) => {
                    writer.resource_failure.store(false, Ordering::Release);
                    return;
                }
                Some(Err(e)) => {
                    warn!(session = %writer.session_id, "waiting to recover its journal: {e:#}");
                    reason = format!("{e:#}").chars().take(1000).collect();
                    let space = disk.fresh_space(&state_dir).await;
                    if crate::disk::resource_error(&e, space.as_ref()) {
                        disk_was_full = space.as_ref().is_some_and(|s| s.free < crate::disk::RESUME_BYTES);
                        // Resource exhaustion must not spend the corruption
                        // budget. Its retry lane survives arbitrarily long
                        // pressure and wakes on space returning.
                        pause.take();
                        if disk_wait.is_none() {
                            let reason = disk.waiting_reason(&state_dir);
                            disk_wait = Some(DiskWait::new(gate.clone(), &writer, reason));
                        }
                        continue;
                    }
                    if disk_wait.take().is_some() {
                        // A different failure (notably corruption) gets its
                        // own bounded recovery period, not an infinite wait.
                        deadline = Some(tokio::time::Instant::now() + budget);
                        backoff = Duration::from_millis(100);
                    }
                    if let Some(paused) = &pause {
                        paused.update_reason(&reason);
                    }
                }
                None => {}
            }
            if deadline.is_some_and(|until| tokio::time::Instant::now() >= until) {
                // The attempt can observe the deadline before Tokio polls
                // the timer ready. Always install the action lane before
                // switching to the manual phase.
                pause.get_or_insert_with(|| RecoveryPause::new(gate.clone(), &writer, &reason));
                deadline = None;
            }
            backoff = next_recovery_backoff(backoff);
        }
    }

    async fn recover_once(&mut self, cli_retired: &mut bool) -> anyhow::Result<()> {
        let _build_use = self.journal.build_folder_lifecycle.clone().read_owned().await;
        self.journal.recover_connection().await?;
        if self.runs_claude_code() && !*cli_retired {
            // A held reader may already have advanced past the
            // line whose journal append failed. Re-adopting that
            // offset could silently omit the reply forever. Keep
            // the FIFO writer alive until ownership-checked cleanup
            // proves the old CLI cannot write, then explicitly
            // continue on a replacement process from the log.
            crate::claude_code::retire_after_journal_failure(
                &self.manager.claude_dir().join("held"),
                &self.writer.session_id,
                &self.journal,
                self.claude.as_mut(),
            )
            .await?;
            self.claude.take();
            self.claude_stdin.lock().await.take();
            // Mirrored native children have no independent actor
            // to settle them once their owning CLI is retired.
            for (child, seq, turn) in self.journal.open_turns().await? {
                let Some((spec, _)) = self.journal.load_session(&child).await? else { continue };
                if !spec.delegation.as_ref().is_some_and(|d| d.observed && d.parent_session_id == self.writer.session_id) {
                    continue;
                }
                let writer = Writer::new(child, self.journal.clone(), self.link.clone(), seq + 1);
                writer.append(session_event::Kind::Error(ErrorEvent { message: JOURNAL_FAILURE_DIAGNOSTIC.into() })).await?;
                writer.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
            }
            *cli_retired = true;
        }
        self.reload_after_failure().await
    }

    async fn reload_after_failure(&mut self) -> anyhow::Result<()> {
        let sid = self.writer.session_id.clone();
        let (mut spec, mut events) = self.journal.load_session(&sid).await?.ok_or_else(|| anyhow::anyhow!("session {sid} is missing"))?;
        let next = events.last().map_or(1, |e| e.seq + 1);
        *self.writer.next_seq.lock().await = next;
        self.writer.logged.store(next, std::sync::atomic::Ordering::SeqCst);
        let open_turn = events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::TurnStarted(t)) => Some(Some(t.turn)),
                Some(session_event::Kind::TurnEnded(_)) => Some(None),
                _ => None,
            })
            .flatten();
        let interrupted_turn = if open_turn.is_some() || !self.inbox.lock().unwrap().has_unstarted_input(&events) {
            open_turn
        } else {
            // The receipt already prevents redelivery. Give the input a
            // failed turn to settle, rather than writing another User event
            // or replaying an input the CLI might already have taken.
            let turn = 1 + events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_)))).count() as u64;
            self.writer.append(session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })).await?;
            Some(turn)
        };
        if let Some(turn) = interrupted_turn {
            self.writer.append(session_event::Kind::Error(ErrorEvent { message: JOURNAL_FAILURE_DIAGNOSTIC.into() })).await?;
            self.writer.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
            events = self.journal.events_after(&sid, 0, u32::MAX).await?;
        }
        self.conv = Some(conversation_from_log(&mut spec, &events));
        self.spec = spec;
        self.writer.set_keep_warm_capture(&self.spec);
        *self.launch_limits.lock().unwrap() = crate::subagent::LaunchLimits::of(&self.spec);
        if let Some(started) = events.iter().find_map(|e| match &e.kind {
            Some(session_event::Kind::Started(s)) => Some(s.clone()),
            _ => None,
        }) {
            self.started = runtime_of(started, &events);
        }
        self.turns = events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_)))).count() as u64;
        // Closing the turn can succeed before a later recovery write fails.
        // Retrying recovery must still schedule that turn's continuation.
        let outcome = last_turn_outcome(&events);
        let resume_turn = (outcome.reason == TurnEndReason::Error && outcome.error == JOURNAL_FAILURE_DIAGNOSTIC)
            .then(|| {
                events.iter().rev().find_map(|e| match &e.kind {
                    Some(session_event::Kind::TurnStarted(t)) => Some(t.turn),
                    _ => None,
                })
            })
            .flatten();
        let taken: HashSet<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::User(u)) => Some(u.command_id.as_str()),
                _ => None,
            })
            .collect();
        self.withdrawn = withdrawn_sources(&events);
        self.inbox.lock().unwrap().recover(&taken, &self.withdrawn, self.claude.is_some());
        self.deferred.clear();
        let (_, noticed, pending) = notices_of(&events);
        self.notice_arrivals.clear();
        self.notice_deadline = None;
        self.notice_ready.clear();
        self.notice_pause = None;
        self.notice_warning = None;
        self.noticed = noticed.into_iter().collect();
        // A background completion has no head redelivery. If its event or
        // notice write failed, finish it before taking another callback.
        self.write_pending_task_event(false).await?;
        for (input, wake) in pending {
            self.deliver_notice(input, false, wake, false).await?;
        }
        // notice() retains even an input whose NoticeQueued write failed.
        while let Some((input, wake)) = self.unrecorded_notices.first().cloned() {
            if !taken.contains(input.command_id.as_str()) && !self.noticed.contains(&input.command_id) {
                self.notice(input, false, wake).await?;
            }
            self.unrecorded_notices.remove(0);
        }
        self.settle_delegations().await?;
        self.record_remote_receipts().await?;
        if let Some(turn) = resume_turn
            && let Some(handle) = self.manager.actors.lock().unwrap().get(&sid)
        {
            let _ = handle.tx.send(ActorMsg::ResumeFailed(turn));
        }
        Ok(())
    }

    async fn run(
        &mut self,
        rx: &mut mpsc::UnboundedReceiver<ActorMsg>,
        running: &mut Option<(TurnTask, CancellationToken)>,
        recoverable: &mut bool,
        gate: &Arc<RecoveryGate>,
    ) {
        // Seal callbacks from the failed run before the supervisor rebuilds
        // its cursor, including when the runtime aborts this actor task.
        let _shell_contexts = CancelShellContexts(self.session_tools.clone());
        match self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await {
            Ok(events) => {
                if let Some(last) = events.last() {
                    self.writer.advance_clock_to(last.ts_ms);
                }
                self.withdrawn = withdrawn_sources(&events);
                let mut observation = CacheObservation::default();
                for event in &events {
                    observation.observe(event);
                }
                *self.writer.cache_observation.lock().unwrap() = observation;
            }
            Err(e) => {
                error!("loading withdrawn task sources: {e:#}");
                return;
            }
        }
        let mut reset = match self.journal.claude_rewind(&self.writer.session_id).await {
            Ok(reset) => reset,
            Err(e) => {
                error!("loading Claude Code rewind: {e:#}");
                return;
            }
        };
        if let Some((r, 0)) = &reset
            && !r.untouched
            && r.failure.is_empty()
        {
            // This actor did not start the destructive attempt. Initialization
            // was deliberately non-held, and neither a crash nor old retry
            // receipts may authorize it to repeat the reset. A live attempt
            // reaches this boundary only inside reset_claude, not startup.
            let interrupted = anyhow::anyhow!("Worker restarted before Claude Code confirmed Rewind; explicitly retry Rewind");
            if let Err(e) = self.rewind_failed(&r.command_id, &interrupted, &r.retries).await {
                error!("recording an interrupted Claude Code rewind: {e:#}");
                return;
            }
            reset = match self.journal.claude_rewind(&self.writer.session_id).await {
                Ok(reset) => reset,
                Err(e) => {
                    error!("loading the interrupted Claude Code rewind: {e:#}");
                    return;
                }
            };
        }
        if let Some((r, _)) = &reset {
            self.withdrawn.extend(r.withdrawn.iter().cloned());
        }
        if let Some((r, 0)) = &reset
            && !r.untouched
            && !r.failure.is_empty()
        {
            // Cleanup does not authorize initialize or retry. Keep the actor
            // and its cancellable inbox even if absence cannot yet be proved.
            if let Err(e) =
                crate::claude_code::stop_held_for_rewind(&self.manager.claude_dir().join("held"), &self.writer.session_id, &self.journal)
                    .await
            {
                warn!(session = %self.writer.session_id, "cleaning up a failed Rewind's held process: {e:#}");
            }
        }
        // Rewinds wait for the running turn to stop. Do not let a retry
        // replace the original receipt while cancellation is still pending.
        let mut rewind_blocked = matches!(&reset, Some((r, 0)) if !r.failure.is_empty());
        let mut adoption = None::<HeldAdoptionRecovery>;
        let adoption_budget = recovery_budget(self.spec.session_recovery_seconds);
        let adoption_writer = self.writer.clone();
        let mut launch_queued = false;
        let mut launch_admission = None::<(String, crate::resource_pressure::Admission)>;
        let mut maintenance_running = false;
        let mut cache_warm_job: Option<CacheWarmJob> = None;
        let mut cache_warm_key: Option<CacheWarmKey> = None;
        self.rewinds = recover_rewind_queue(self.runs_claude_code(), std::mem::take(&mut self.rewinds), reset);
        loop {
            while running.is_none()
                && let Some((command_id, to_seq, restore)) = self.rewinds.front().cloned()
            {
                // Recovery and head redelivery may queue the same receipt.
                // Never replay a confirmed rewind over subsequent input.
                match self.journal.is_applied(&command_id).await {
                    Ok(true) => {
                        self.rewinds.pop_front();
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        error!("checking rewind receipt: {e:#}");
                        return;
                    }
                }
                // An admitted Rewind owns its own safety barrier. It supersedes
                // this adoption incident, never letting adoption bypass it.
                adoption = None;
                if let Err(e) = self.rewind(command_id.clone(), to_seq, restore).await {
                    error!(session = %self.writer.session_id, "rewinding: {e:#}");
                    if !self.runs_claude_code() && !e.is::<StopUnconfirmed>() {
                        // No CLI reset intent to roll back. Preserve the
                        // native actor's existing fail-closed journal-error path.
                        let _ = self
                            .writer
                            .append(session_event::Kind::Error(ErrorEvent {
                                message: format!(
                                    "Rewind could not be confirmed; no further messages were sent. Retry Rewind after resolving this: {e:#}"
                                ),
                            }))
                            .await;
                        // A failed journal is recoverable; an internal
                        // ownership/rewind guard is not authorization to
                        // restart work past that guard.
                        *recoverable = e.chain().any(|cause| cause.downcast_ref::<sqlx::Error>().is_some());
                        return;
                    }
                    let retries: Vec<_> = if self.runs_claude_code() {
                        self.rewinds.iter().map(|(id, _, _)| id.clone()).filter(|id| id != &command_id).collect()
                    } else {
                        Vec::new()
                    };
                    match self.rewind_failed(&command_id, &e, &retries).await {
                        Ok(blocked) => rewind_blocked = blocked,
                        Err(e) => {
                            error!("recording Rewind failure: {e:#}");
                            return;
                        }
                    }
                    if self.runs_claude_code() {
                        self.rewinds.clear();
                        break;
                    }
                    // Ordinary Rewinds are independent commands, not CLI
                    // reset retries. A refused restore must not reject a
                    // queued, exempt conversation-only Rewind.
                    self.rewinds.pop_front();
                    continue;
                }
                self.rewinds.pop_front();
                rewind_blocked = false;
            }
            if !rewind_blocked && running.is_none() && self.runs_claude_code() && self.claude.is_none() {
                let recovery = adoption
                    .get_or_insert_with(|| HeldAdoptionRecovery::new(gate.clone(), adoption_budget, crate::disk::retry_delays(&self.spec)));
                if recovery.check(&adoption_writer, self.take_up_held()).await {
                    adoption = None;
                }
            }
            let adoption_blocked = adoption.is_some();
            let adoption_retry_at = adoption.as_ref().and_then(|r| r.retry_at);
            let waiting_for_adoption_action = adoption.as_ref().is_some_and(|r| r.action.is_some());
            let handoff_waits = running.is_none() && self.handoff_waits();
            if handoff_waits && !self.handoff_announced {
                self.handoff_announced = true;
                let message = "The switch away from Claude Code waits for its background tasks to finish (they would stop with it). \
                               Stop them in the Tasks list to switch now; your message comes after the switch."
                    .to_owned();
                if let Err(e) = self.writer.append(session_event::Kind::Error(ErrorEvent { message })).await {
                    error!(session = %self.writer.session_id, "recording a waiting switch: {e:#}");
                    return;
                }
            }
            let can_start =
                !rewind_blocked && !adoption_blocked && running.is_none() && !handoff_waits && !self.inbox.lock().unwrap().queue.is_empty();
            // New children require disk, memory and load admission. Existing
            // child turns retain memory/load admission, but disk recovery
            // resumes at its small headroom threshold, not the new-work 5%.
            // A fresh Runner admission is good once for its queued input.
            let pressure = if can_start && crate::subagent::depth_of(&self.spec) > 0 {
                let admitted = launch_admission.take().is_some_and(|(id, admission)| {
                    self.inbox.lock().unwrap().queue.iter().any(|(input, _)| input.command_id == id)
                        && admission.redeem(!self.disk_admitted)
                });
                if admitted {
                    None
                } else if self.disk_admitted {
                    self.launch_pressure.try_continue().err()
                } else {
                    self.launch_pressure.try_start().err()
                }
            } else {
                None
            };
            if let Some(pressure) = pressure {
                if !launch_queued && pressure != crate::resource_pressure::Pressure::Sampling {
                    launch_queued = true;
                    pressure.announce(&self.writer).await;
                }
            } else {
                launch_queued = false;
            }
            let launch_retry = pressure.map(|_| crate::resource_pressure::retry_at());
            if can_start && pressure.is_none() {
                let switch = self.inbox.lock().unwrap().next_switch();
                if let Some((command_id, model)) = switch {
                    self.handoff_announced = false;
                    if let Err(e) = self.switch_model(model).await {
                        error!(session = %self.writer.session_id, "recording a model switch: {e:#}");
                        return;
                    }
                    self.inbox.lock().unwrap().finish_switch(&command_id);
                }
                *running = Some(self.start_turn());
                self.running_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let held_background = background_liveness_counts(&self.session_tools.background.list()).0 > 0;
            let idle_until =
                self.claude.as_ref().filter(|_| running.is_none() && !held_background).and_then(|p| p.idle_until(self.claude_idle()));
            // Claude Code's tasks that ended while it waited, if it doesn't
            // start a turn for them by itself.
            let wake_at =
                self.claude.as_ref().filter(|_| !rewind_blocked && !adoption_blocked && running.is_none()).and_then(|p| p.wake_due());
            // A merge window or a ready fan-in group, never an hourly wake count.
            let waiting_to_wake = !rewind_blocked && !adoption_blocked && running.is_none() && self.deferred.iter().any(|(_, wake)| *wake);
            let retry_at = (waiting_to_wake && self.link.is_connected()).then_some(self.notice_deadline).flatten();
            // Maintenance blocks runtime replacement, not idle notice guards:
            // there is no model turn for a callback to join while it runs.
            let turn_running = running.is_some() && !maintenance_running;
            let candidate = if rewind_blocked
                || adoption_blocked
                || running.is_some()
                || !self.inbox.lock().unwrap().queue.is_empty()
                || !self.link.is_connected()
            {
                None
            } else {
                self.cache_warm_candidate()
            };
            gate.has_turn.store(self.turns > 0, Ordering::Relaxed);
            gate.waiting_schedule.store(retry_at.is_some() || wake_at.is_some(), Ordering::Relaxed);
            if candidate.as_ref().map(|(key, _, _)| key) != cache_warm_key.as_ref() {
                cache_warm_job = None;
                gate.keep_warm_next_ms.store(0, Ordering::Relaxed);
                cache_warm_key = candidate.as_ref().map(|(key, _, _)| key.clone());
                if let Some((key, kind, observation)) = candidate
                    && observation.last_attempt_call_ms != Some(key.call_ms)
                {
                    // Wait-boundary trimming integration hook (context-economy
                    // §3.1): trim carbon6-held work here before arming the timer.
                    // A Claude Code process with native tasks must NEVER be
                    // restarted to trim; keep this prefix warm untrimmed and
                    // defer that agent's quiet-turn trim until its tasks finish.
                    cache_warm_job = Some(cache_warm_timer(key, kind, observation, self.writer.clone(), gate.clone()));
                }
            }
            tokio::select! {
                msg = rx.recv() => match msg {
                    None => return,
                    Some(ActorMsg::HealthProbe(reply)) => { let _ = reply.send(()); }
                    // Another session's agent wrote (docs/background.md §6): a
                    // notice, not your message (no commands, mentions or hooks).
                    Some(ActorMsg::Input(input, _, _)) if !input.from_session_id.is_empty() => {
                        let mut input = *input;
                        let note = spec_prompt(&self.spec, "session.message_received", c6_agent::SESSION_MESSAGE_NOTE);
                        input.command = "session-message".into();
                        input.prompt = format!(
                            "<session-message from=\"{}\" title=\"{}\">\n{}\n</session-message>\n\n{note}",
                            input.from_session_id,
                            input.from_title.replace('"', "'"),
                            input.text
                        );
                        if let Err(e) = self.notice(input, turn_running, true).await {
                            error!(session = %self.writer.session_id, "queueing a session message: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Input(mut input, model, choice)) => {
                        let direct = input.delegated_by.is_empty()
                            && !matches!(input.command.as_str(), "task-notification" | "stop-hook");
                        if direct {
                            if let Err(e) = self.continue_notices(false).await {
                                error!(session = %self.writer.session_id, "continuing notice wakes: {e:#}");
                                return;
                            }
                            // Your message resets an earlier session-message chain.
                            self.session_tools.set_message_hops(0);
                        }
                        // Claude Code's carbon6 tools as they are set now
                        // (its process is started again with them when they
                        // changed: `Launch::tools`).
                        if let Some(c) = choice.claude_code.clone() {
                            self.spec.claude_code = Some(c);
                        }
                        // Pick the custom command's variant for this
                        // session, expand the command and attach the files
                        // it mentions, as they are now.
                        let cwd = Path::new(&self.started.cwd);
                        if !choice.candidates_json.is_empty() {
                            #[derive(serde::Deserialize)]
                            struct Variant {
                                #[serde(default)]
                                applies_to: Vec<String>,
                                body: String,
                            }
                            let head = if choice.labels.is_empty() { &self.spec.labels } else { &choice.labels };
                            let model = self.spec.model.as_ref().map(|m| m.model.as_str()).unwrap_or_default();
                            let labels = session_labels(head, &self.started.repo, &self.started.cwd, &self.started.home, model, &self.started.ssh_target);
                            let variants: Vec<Variant> = serde_json::from_str(&choice.candidates_json).unwrap_or_default();
                            match c6_api::labels::best(variants.iter(), |v| &v.applies_to, &labels) {
                                Some(v) => input.template = v.body.clone(),
                                None => input.command.clear(), // none applies here: plain text
                            }
                        }
                        let ssh = self.session_tools.ssh();
                        crate::commands::expand(cwd, ssh.as_deref(), &mut input).await;
                        // @paths name files here, not on an SSH target.
                        if ssh.is_none() {
                            crate::mentions::resolve(cwd, &mut input);
                        }
                        if let Err(e) = crate::attachments::prepare(
                            &self.manager.state_dir,
                            &self.writer.session_id,
                            &mut input,
                            ssh.is_some(),
                        ).await {
                            warn!(session = %self.writer.session_id, "preparing message attachments: {e:#}");
                        }
                        if let Some(h) = self.hooks.as_ref().filter(|_| self.started.tool_profile != crate::claude_code::PROFILE) {
                            let v = h.run("UserPromptSubmit", None, serde_json::json!({"prompt": input.model_text()})).await;
                            if let Some(reason) = v.blocked {
                                // Recorded (so the command is done) but never sent.
                                input.blocked = reason;
                                if let Err(e) = self.writer.append(input.event()).await {
                                    error!(session = %self.writer.session_id, "recording a blocked message: {e:#}");
                                    return;
                                }
                                continue;
                            }
                            if let Some(context) = v.context {
                                input.prompt = format!("{}\n\n<user-prompt-submit-hook>\n{context}\n</user-prompt-submit-hook>", input.model_text());
                            }
                        }
                        let held = if direct { std::mem::take(&mut self.deferred) } else { Vec::new() };
                        let command_id = input.command_id.clone();
                        // Send now in carbon6's own loop: the turn stops (it takes
                        // no input once stopping), and this starts the next. A
                        // Claude Code turn delivers it at once when it writes it
                        // (`Process::turn`).
                        let stops = input.send_now && !self.runs_claude_code();
                        if stops && let Some((_, cancel)) = running.as_ref() {
                            cancel.cancel();
                        }
                        {
                            let mut inbox = self.inbox.lock().unwrap();
                            for (n, _) in held {
                                inbox.push(n, None);
                            }
                            inbox.push(*input, model);
                        }
                        // A running turn of carbon6's own loop takes it at its next
                        // step: it is with the agent. (A Claude Code turn says so
                        // when it writes it to its process.)
                        if running.is_some() && !self.runs_claude_code() && !stops {
                            let delivered = c6_proto::InputDelivered { command_id };
                            if let Err(e) = self.writer.append(session_event::Kind::InputDelivered(delivered)).await {
                                error!(session = %self.writer.session_id, "recording a delivered input: {e:#}");
                                return;
                            }
                        }
                    }
                    Some(msg @ (ActorMsg::Interrupt | ActorMsg::InterruptDelegation)) => {
                        if let Some((_, cancel)) = running.as_ref() {
                            cancel.cancel();
                            self.stopping |= self.runs_claude_code();
                        } else if launch_queued {
                            // An interrupt while waiting for worker headroom
                            // must not leave the parent's launch running later.
                            if let Err(error) = self.interrupt_queued_launch().await {
                                error!(session = %self.writer.session_id, %error, "stopping a queued subagent launch");
                                return;
                            }
                        }
                        if matches!(msg, ActorMsg::InterruptDelegation) {
                            let background = &self.session_tools.background;
                            for task in background.list() {
                                if task.running && crate::subagent::counted_background(task.notify, task.kind.as_str()) {
                                    background.kill_by_user(&task.id);
                                }
                            }
                        }
                    }
                    Some(ActorMsg::SendNow(command_id)) => {
                        if turn_running && self.runs_claude_code() {
                            // The running turn asks its process to deliver it now.
                            self.inbox.lock().unwrap().send_now(command_id);
                        } else if let Some((_, cancel)) = running.as_ref() {
                            // carbon6's own loop: the turn stops, and the agent
                            // takes it as its next turn.
                            cancel.cancel();
                        }
                    }
                    Some(ActorMsg::Delegate(input, done, admission)) => {
                        if (running.is_none() || maintenance_running) && let Some(admission) = admission {
                            launch_admission = Some((input.command_id.clone(), admission));
                        }
                        if let Some(done) = done {
                            self.waiting.push((input.command_id.clone(), done));
                        }
                        // Notices held for the next input come with it.
                        let held = std::mem::take(&mut self.deferred);
                        let mut inbox = self.inbox.lock().unwrap();
                        for (n, _) in held {
                            inbox.push(n, None);
                        }
                        inbox.push(*input, None);
                    }
                    Some(ActorMsg::RemoteReport(command_id, ended, recorded)) => {
                        match self.remote_report(command_id, *ended, turn_running).await {
                            Ok(()) => { let _ = recorded.send(Ok(())); }
                            Err(e) => {
                                error!(session = %self.writer.session_id, "taking a remote subagent's report: {e:#}");
                                let _ = recorded.send(Err(format!("could not record subagent report: {e:#}")));
                                return;
                            }
                        }
                    }
                    Some(ActorMsg::ChildReport { command_id, text, wake, task_notice, inline, recorded }) => {
                        if let Some(inline) = inline {
                            match self.journal.events_after(&self.writer.session_id, inline.after, u32::MAX).await {
                                Ok(events) if inline.recorded(&events) => {
                                    let _ = recorded.send(());
                                    continue;
                                }
                                Ok(events) if inline.background_pending(&events, |task| {
                                    !self.withdrawn.contains(task)
                                        && !self.withdrawn.contains(&inline.child)
                                        && !events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::BackgroundTask(t))
                                            if t.task_id == task && t.kind == "agent" && t.state != "started"))
                                        && self.session_tools.background.snapshot(task).is_some_and(|t| t.kind == "agent")
                                }) => {
                                    // Do not acknowledge the fallback until
                                    // the owning task has actually reported.
                                    continue;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(session = %self.writer.session_id, "checking a subagent's inline result: {e:#}");
                                    continue;
                                }
                            }
                        }
                        let input = PendingInput {
                            command_id,
                            text,
                            command: "task-notification".into(),
                            task_notice,
                            ..Default::default()
                        };
                        if let Err(e) = self.notice(input, turn_running, wake).await {
                            error!(session = %self.writer.session_id, "queueing a subagent's report: {e:#}");
                            return;
                        }
                        let _ = recorded.send(());
                    }
                    Some(ActorMsg::RecoverReport) => {
                        if let Err(e) = self.settle_delegations().await {
                            error!(session = %self.writer.session_id, "recovering a subagent's report: {e:#}");
                        }
                    }
                    Some(ActorMsg::NoticeLimits(limits)) => {
                        let previous = crate::subagent::LaunchLimits::of(&self.spec);
                        crate::subagent::LaunchLimits::apply_notice_limits(&mut self.spec, &limits);
                        self.writer.set_keep_warm_capture(&self.spec);
                        if previous == crate::subagent::LaunchLimits::of(&self.spec) {
                            continue;
                        }
                        // A settings edit is not Continue: retain all evidence
                        // and usage, and reevaluate the held batch immediately.
                        let refresh = async {
                            self.writer.append(session_event::Kind::Notice(c6_proto::Notice {
                                title: "Notice settings refreshed".into(),
                                message: serde_json::json!({
                                    "notice_quiet_seconds": limits.notice_quiet_seconds,
                                    "notice_max_wait_seconds": limits.notice_max_wait_seconds,
                                    "notice_no_progress_limit": limits.notice_no_progress_limit,
                                    "notice_failure_relaunch_limit": limits.notice_failure_relaunch_limit,
                                    "notice_tokens_per_hour": limits.notice_tokens_per_hour,
                                    "notice_loop_backoff_rounds": limits.notice_loop_backoff_rounds,
                                    "notice_loop_backoff_max_seconds": limits.notice_loop_backoff_max_seconds,
                                    "max_subagent_depth": limits.max_subagent_depth,
                                    "max_running_subagents": limits.max_running_subagents,
                                    "claude_code_trim_enabled": self.spec.claude_code_trim_enabled.unwrap_or(true),
                                    "claude_code_trim_at": self.spec.claude_code_trim_at,
                                    "keep_warm_json": limits.keep_warm_json,
                                }).to_string(),
                                priority: "low".into(),
                                quiet: true,
                            })).await?;
                            *self.launch_limits.lock().unwrap() = crate::subagent::LaunchLimits::of(&self.spec);
                            self.schedule_notices().await
                        }.await;
                        if let Err(e) = refresh {
                            error!(session = %self.writer.session_id, "refreshing notice settings: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::ReleaseNotices) => {
                        if let Err(e) = self.continue_notices(true).await {
                            error!(session = %self.writer.session_id, "continuing notice wakes: {e:#}");
                            return;
                        }
                        let held = std::mem::take(&mut self.deferred);
                        info!(session = %self.writer.session_id, notices = held.len(), "delivering held notices: you asked");
                        let mut inbox = self.inbox.lock().unwrap();
                        for (n, _) in held {
                            inbox.push(n, None);
                        }
                    }
                    Some(ActorMsg::Reloaded { noticed, pending }) => {
                        self.noticed.extend(noticed);
                        for (input, wake) in pending {
                            info!(session = %self.writer.session_id, notice = %input.command_id, "delivering a notice again after a reload");
                            if let Err(e) = self.deliver_notice(input, turn_running, wake, false).await {
                                error!(session = %self.writer.session_id, "delivering a notice again: {e:#}");
                                return;
                            }
                        }
                    }
                    Some(ActorMsg::TaskFence(ack)) => {
                        let _ = ack.send(());
                    }
                    Some(ActorMsg::Task(ev)) => {
                        let state = match ev.state {
                            c6_tools::TaskState::Started => "started",
                            c6_tools::TaskState::Finished => "finished",
                            c6_tools::TaskState::Failed => "failed",
                            c6_tools::TaskState::Lost => "lost",
                            c6_tools::TaskState::Stopped => "stopped",
                            c6_tools::TaskState::StoppedByUser => "stopped_by_user",
                        };
                        let record = BackgroundTask {
                            task_id: ev.id.clone(),
                            kind: ev.kind.as_str().into(),
                            description: ev.description.clone(),
                            state: state.into(),
                            exit_code: ev.exit_code.unwrap_or(0),
                            elapsed_ms: ev.elapsed.as_millis() as u64,
                            notify: ev.notify,
                            pgid: ev.pgid.unwrap_or(0),
                            native_type: ev.native_type.clone(),
                            foreground: ev.foreground,
                        };
                        // The agent stopped it itself: nothing to tell it. Claude
                        // Code tells its agent about its own tasks, except one
                        // lost with its process: then only we can.
                        let lost_native = ev.kind == c6_tools::TaskKind::ClaudeCode && ev.state == c6_tools::TaskState::Lost;
                        let notice = ((ev.notify || lost_native) && !matches!(ev.state, c6_tools::TaskState::Started | c6_tools::TaskState::Stopped))
                            .then(|| {
                                let cause = lost_native
                                    .then(|| spec_prompt(&self.spec, "session.task_lost_with_cli", c6_agent::TASK_LOST_WITH_CLI));
                                task_notice(&record, &ev.output, cause.as_deref())
                            });
                        // Lost because you stopped the turn (Claude Code didn't
                        // stop, so its process was): the agent hears it with
                        // your next message, and isn't woken for it.
                        let held = lost_native && self.after_stop(turn_running);
                        if let Err(e) = self.task_event(record, notice, &ev.output, turn_running && !held, !held).await {
                            error!(session = %self.writer.session_id, "recording a background task: {e:#}");
                            return;
                        }
                        if let Err(e) = self.record_remote_receipts().await {
                            error!(session = %self.writer.session_id, "recording subagent report receipts: {e:#}");
                            return;
                        }
                    }
                    Some(msg @ (ActorMsg::Resume(turn) | ActorMsg::ResumeFailed(turn))) => {
                        let failed = matches!(msg, ActorMsg::ResumeFailed(_));
                        let check = async {
                            let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
                            let pending = matches!(self.journal.claude_rewind(&self.writer.session_id).await?, Some((_, 0)));
                            anyhow::Ok(resume_superseded(&events, turn, pending))
                        }.await;
                        match check {
                            Ok(true) => continue,
                            Ok(false) => {},
                            Err(e) => { error!("checking turn continuation: {e:#}"); return; }
                        }
                        // A subagent's parent decides (its call got what it did);
                        // a held Claude Code process carries on by itself.
                        if self.spec.delegation.is_some() || (self.runs_claude_code() && self.claude.is_some()) {
                            continue;
                        }
                        let (command_id, text) = if failed {
                            (format!("resume-after-failure-{turn}"), spec_prompt(&self.spec, "session.resume_after_failure", c6_agent::RESUME_AFTER_FAILURE))
                        } else {
                            (format!("resume-after-restart-{turn}"), spec_prompt(&self.spec, "session.resume_after_restart", c6_agent::RESUME_AFTER_RESTART))
                        };
                        let input = PendingInput {
                            command_id,
                            text,
                            command: "task-notification".into(),
                            ..Default::default()
                        };
                        if let Err(e) = self.notice(input, turn_running, true).await {
                            error!(session = %self.writer.session_id, "resuming an interrupted turn: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Lost { cause, task_id, kind, description, native_type, notify, pgid, child, remote }) => {
                        // A held shell command kept running: take it up again,
                        // and its end reaches the agent as usual.
                        if kind == "shell" && self.session_tools.background.adopt(&task_id) {
                            info!(session = %self.writer.session_id, task = %task_id, "took up a background command after session reload");
                            continue;
                        }
                        // It may outlive the worker (its own process group):
                        // stop it, since nothing reads its output any more.
                        stop_orphan(pgid, &description);
                        // A subagent's log is kept: what it did so far. A
                        // remote one keeps working; its report comes later.
                        let report = if child.is_empty() {
                            String::new()
                        } else if remote {
                            let note = cause.remote_prompt(&self.spec);
                            format!("<subagent id=\"{child}\" status=\"running\"/>\n{note}")
                        } else {
                            crate::subagent::partial(&self.spec, &crate::subagent::digest(&self.journal, &child, "lost", "").await)
                        };
                        // Claude Code's own tasks died with its process: its agent
                        // hears it from us.
                        let tell = notify || kind == "claude_code";
                        let record = BackgroundTask { task_id, kind, description, native_type, state: "lost".into(), notify, ..Default::default() };
                        let notice = tell.then(|| {
                            let note = cause.prompt(&self.spec);
                            task_notice(&record, &report, (!remote).then_some(note.as_str()))
                        });
                        if let Err(e) = self.task_event(record, notice, "", turn_running, true).await {
                            error!(session = %self.writer.session_id, "recording a lost background task: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::StopTask(id)) => {
                        match self.session_tools.background.native_stop(&id) {
                            Some(task_id) => {
                                if let Err(e) = crate::claude_code::stop_task(&self.claude_stdin, &task_id).await {
                                    warn!(session = %self.writer.session_id, task = %id, "stopping a Claude Code task: {e}");
                                }
                            }
                            None => {
                                self.session_tools.background.kill_by_user(&id);
                            }
                        }
                    }
                    Some(ActorMsg::Reserve(n)) => self.session_tools.background.reserve_ids(n),
                    Some(ActorMsg::Approval(e)) => {
                        let a = c6_proto::Approval { call_id: e.call_id, state: e.state.into(), tool: e.tool, input: e.input, policy: e.policy, message: e.message };
                        if let Err(e) = self.writer.append(session_event::Kind::Approval(a)).await {
                            error!(session = %self.writer.session_id, "recording an approval: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Decide { call_id, allow }) => {
                        if self.policies.as_ref().is_some_and(|p| p.decide(&call_id, allow)) {
                            let a = c6_proto::Approval { call_id, state: if allow { "allowed" } else { "denied" }.into(), ..Default::default() };
                            if let Err(e) = self.writer.append(session_event::Kind::Approval(a)).await {
                                error!(session = %self.writer.session_id, "recording an approval: {e:#}");
                                return;
                            }
                        }
                    }
                    Some(ActorMsg::Memory(m)) => {
                        let update = c6_proto::MemoryUpdate { op: m.op, id: m.id, scope: m.scope, text: m.text };
                        if let Err(e) = self.writer.append(session_event::Kind::Memory(update)).await {
                            error!(session = %self.writer.session_id, "recording a memory change: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::SetSubagentModel { command_id, model }) => {
                        let ev = c6_proto::SubagentModelSet { model: model.clone(), command_id };
                        if let Err(e) = self.writer.append(session_event::Kind::SubagentModelSet(ev)).await {
                            error!(session = %self.writer.session_id, "recording the subagents' model: {e:#}");
                            return;
                        }
                        set_subagent_default(&mut self.spec, model);
                        *self.subagent_default.lock().unwrap() = crate::subagent::SubagentDefault::of(&self.spec);
                        info!(session = %self.writer.session_id, "subagent model set");
                    }
                    Some(ActorMsg::Notice(n)) => {
                        let notice = c6_proto::Notice { title: n.title, message: n.message, priority: n.priority, quiet: false };
                        if let Err(e) = self.writer.append(session_event::Kind::Notice(notice)).await {
                            error!(session = %self.writer.session_id, "recording a notice: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Published(p)) => {
                        let output = c6_proto::Output { name: p.name, title: p.title, mime: p.mime, data: p.data };
                        if let Err(e) = self.writer.append(session_event::Kind::Output(output)).await {
                            error!(session = %self.writer.session_id, "recording an output: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::RewindRefused { command_id, reason }) => {
                        if let Err(e) = self.rewind_failed(&command_id, &anyhow::anyhow!(reason), &[]).await {
                            error!("recording Rewind refusal: {e:#}"); return;
                        }
                    }
                    Some(ActorMsg::Rewind { command_id, to_seq, restore_files }) => {
                        if !self.rewinds.iter().any(|(id, _, _)| id == &command_id) {
                            self.rewinds.push_back((command_id.clone(), to_seq, restore_files));
                        }
                        match self.journal.is_applied(&command_id).await {
                            Ok(true) => {
                                self.rewinds.retain(|(id, _, _)| id != &command_id);
                                continue;
                            },
                            Ok(false) => {},
                            Err(e) => { error!("checking rewind receipt: {e:#}"); return; }
                        }
                        if self.runs_claude_code() || restore_files {
                            // Dispatch preflight can race a newly started child.
                            // Recheck before canceling the running turn, also
                            // for a file restore on carbon6's own loop.
                            let check = async {
                                let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
                                if self.runs_claude_code() {
                                    self.manager.rewind_preflight(&self.writer.session_id, &events, to_seq, restore_files).await
                                } else {
                                    self.manager.rewind_children_stopped(&events).await
                                }
                            }.await;
                            if let Err(e) = check {
                                if !self.runs_claude_code() {
                                    if !e.is::<StopUnconfirmed>() {
                                        let _ = self.writer.append(session_event::Kind::Error(ErrorEvent {
                                            message: format!(
                                                "Rewind could not be confirmed; no further messages were sent. Retry Rewind after resolving this: {e:#}"
                                            ),
                                        })).await;
                                        *recoverable = e.chain().any(|cause| cause.downcast_ref::<sqlx::Error>().is_some());
                                        return;
                                    }
                                    match self.rewind_failed(&command_id, &e, &[]).await {
                                        Ok(blocked) => rewind_blocked = blocked,
                                        Err(e) => { error!("recording Rewind refusal: {e:#}"); return; }
                                    }
                                    // Do not apply Claude retry grouping to
                                    // independent ordinary-loop commands.
                                    self.rewinds.retain(|(id, _, _)| id != &command_id);
                                    continue;
                                }
                                let pending = self.journal.claude_rewind(&self.writer.session_id).await;
                                let id = match &pending { Ok(Some((r, 0))) => r.command_id.as_str(), _ => &command_id };
                                let mut retries: Vec<_> = self.rewinds.iter().map(|(id, _, _)| id.clone()).filter(|queued| queued != id).collect();
                                if id != command_id && !retries.contains(&command_id) { retries.push(command_id.clone()); }
                                match self.rewind_failed(id, &e, &retries).await {
                                    Ok(blocked) => rewind_blocked = blocked,
                                    Err(e) => { error!("recording Rewind refusal: {e:#}"); return; }
                                }
                                self.rewinds.clear();
                                continue;
                            }
                        }
                        if self.runs_claude_code() {
                            if let Ok(Some((r, 0))) = self.journal.claude_rewind(&self.writer.session_id).await {
                                // A NEW command is an explicit retry. Implicit
                                // redelivery of a failed receipt never retries.
                                if !r.failure.is_empty() {
                                    if r.command_id == command_id {
                                        self.rewinds.retain(|(id, _, _)| id != &command_id);
                                        continue;
                                    }
                                    if !self.rewinds.iter().any(|(id, _, _)| id == &r.command_id) {
                                        self.rewinds.push_front((r.command_id, r.to_seq, r.restore_files));
                                    }
                                }
                            }
                            if running.is_some() {
                                // Interrupt can itself kill a CLI that fails to
                                // stop its turn. Crash recovery must already
                                // know old process/history may have been touched.
                                let transition = async {
                                    let Some((mut reset, 0)) = self.journal.claude_rewind(&self.writer.session_id).await? else {
                                        anyhow::bail!("Missing pending Rewind before interrupting Claude Code")
                                    };
                                    reset.untouched = false;
                                    reset.previous = None;
                                    self.journal.prepare_claude_rewind_owned(&self.writer.session_id, &reset, self.writer.ownership_epoch).await
                                }.await;
                                if let Err(e) = transition { error!("starting Rewind interruption: {e:#}"); return; }
                            }
                        }
                        if let Some((_, cancel)) = running.as_ref() { cancel.cancel(); }
                    }
                    Some(ActorMsg::RewindIdle(reply)) => {
                        let idle = running.is_none()
                            && self.inbox.lock().unwrap().queue.is_empty()
                            && self.deferred.is_empty()
                            && !self.session_tools.background.list().iter().any(|t| t.running)
                            && rx.is_empty();
                        let _ = reply.send(idle);
                    }
                    Some(ActorMsg::ChangePlace(command_id, place, reply)) => {
                        let result = if running.is_some() || rewind_blocked || adoption_blocked
                            || !self.inbox.lock().unwrap().queue.is_empty() || !self.deferred.is_empty()
                        {
                            Err(anyhow::anyhow!("Wait for the current turn and queued work before changing place"))
                        } else {
                            self.change_place(&command_id, place).await
                        };
                        let _ = reply.send(result.map_err(|e| format!("{e:#}")));
                    }
                    Some(ActorMsg::FreezeMove { move_id, epoch, reply }) => {
                        let result = if running.is_some() || rewind_blocked || adoption_blocked
                            || !self.inbox.lock().unwrap().queue.is_empty() || !self.deferred.is_empty()
                            || !self.rewinds.is_empty() || !rx.is_empty()
                        {
                            Err(anyhow::anyhow!("Wait for the current turn and queued work before moving workers"))
                        } else {
                            self.freeze_move(&move_id, epoch).await
                        };
                        let released = result.is_ok();
                        let _ = reply.send(result.map_err(|e| format!("{e:#}")));
                        if released { return; }
                    }
                },
                _ = tokio::time::sleep_until(launch_retry.unwrap_or_else(tokio::time::Instant::now)), if launch_retry.is_some() => {}
                result = async { (&mut cache_warm_job.as_mut().expect("checked").0).await }, if cache_warm_job.is_some() => {
                    cache_warm_job = None;
                    gate.keep_warm_next_ms.store(0, Ordering::Relaxed);
                    match result {
                        Ok(Ok(CacheWarmAction::Due { expires_ms })) => {
                            // Completion callbacks and real inputs win over a
                            // ready timer, including an event queued this tick.
                            if !rx.is_empty()
                                || self.writer.now_ms().max(0) as u64 >= expires_ms
                                || self.cache_warm_candidate().as_ref().map(|(key, _, _)| key) != cache_warm_key.as_ref()
                            {
                                cache_warm_key = None;
                                continue;
                            }
                            // A journaled claim travels with moves and survives
                            // crashes before a reply. Ambiguous in-flight calls
                            // are never sent a second time after adoption.
                            let Some(key) = cache_warm_key.as_ref() else { continue };
                            if let Err(e) = self.writer.append(session_event::Kind::Notice(c6_proto::Notice {
                                title: "Cache maintenance attempted".into(), message: key.call_ms.to_string(),
                                priority: "low".into(), quiet: true,
                            })).await {
                                error!(session = %self.writer.session_id, "claiming keep-warm request: {e:#}");
                                return;
                            }
                            let text = spec_prompt(&self.spec, "session.keep_warm", c6_agent::KEEP_WARM_NOTE);
                            let deadline = tokio::time::Instant::now()
                                + Duration::from_millis(expires_ms.saturating_sub(self.writer.now_ms().max(0) as u64));
                            if self.runs_claude_code() {
                                if let Some(process) = self.claude.as_mut()
                                    && let Err(e) = process.send_keep_warm_until(&text, &(self.writer.clone() as Arc<dyn EventSink>), deadline).await
                                {
                                    warn!(session = %self.writer.session_id, "Claude Code keep-warm skipped: {e:#}");
                                }
                            } else {
                                let journal = self.journal.clone();
                                let link = self.link.clone();
                                let session_id = self.writer.session_id.clone();
                                let on_behalf_of = self.on_behalf_of.clone();
                                let epoch = self.writer.ownership_epoch;
                                let writer = self.writer.clone();
                                cache_warm_job = Some(CacheWarmJob(tokio::spawn(async move {
                                    journal.check_ownership(&session_id, epoch).await?;
                                    let Some(snapshot) = journal.load_keep_warm_snapshot(&session_id).await? else {
                                        return Ok(CacheWarmAction::Ineligible);
                                    };
                                    if writer.now_ms().max(0) as u64 >= expires_ms {
                                        return Ok(CacheWarmAction::Ineligible);
                                    }
                                    let usage = match link.keep_warm_until(&on_behalf_of, &snapshot, &text, deadline).await {
                                        Ok(usage) => usage,
                                        Err(error) => {
                                            journal.clear_keep_warm_snapshot(&session_id, epoch).await?;
                                            return Err(error);
                                        }
                                    };
                                    // Persist before returning task completion:
                                    // a simultaneously-ready real input may
                                    // drop this job without selecting its result.
                                    writer.append(session_event::Kind::Usage(usage)).await?;
                                    writer.append(session_event::Kind::Notice(c6_proto::Notice {
                                        title: "Kept the cache warm".into(), message: String::new(), priority: "low".into(), quiet: true,
                                    })).await?;
                                    Ok(CacheWarmAction::Recorded)
                                })));
                            }
                        }
                        Ok(Ok(CacheWarmAction::Recorded)) => {}
                        Ok(Ok(CacheWarmAction::Ineligible)) => {}
                        Ok(Err(e)) => warn!(session = %self.writer.session_id, "keep-warm skipped: {e:#}"),
                        Err(e) => warn!(session = %self.writer.session_id, "keep-warm task stopped: {e}"),
                    }
                }
                _ = tokio::time::sleep_until(self.local_report_retry_at.unwrap_or_else(tokio::time::Instant::now)), if self.local_report_retry_at.is_some() => {
                    if let Err(e) = self.settle_delegations().await {
                        warn!(session = %self.writer.session_id, "retrying a subagent's report: {e:#}");
                    }
                }
                // Claude Code wrote while no turn ran: a background task's
                // progress, or a turn it starts by itself.
                line = async { self.claude.as_mut().expect("checked").next_line().await }, if !rewind_blocked && !adoption_blocked && running.is_none() && self.claude.is_some() => {
                    match line {
                        None => {
                            let unsent = self.claude.take().expect("checked").exited().await;
                            self.inbox.lock().unwrap().requeue(unsent);
                        }
                        Some(v) => {
                            let sink: Arc<dyn EventSink> = self.writer.clone();
                            match self.claude.as_mut().expect("checked").idle_line(v, &sink).await {
                                Ok(true) => {
                                    // The external CLI has already started native
                                    // work; this attaches a recorder, not a dispatch.
                                    launch_admission = None;
                                    *running = Some(self.start_turn());
                                    self.running_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                }
                                Ok(false) => {
                                    if self.claude.as_ref().is_some_and(|p| p.trim_quiet()) {
                                        // Draining the last trailing result or native-task
                                        // update can make a deferred trim eligible. This
                                        // owns the supervised slot, not a model turn.
                                        *running = Some(self.start_claude_maintenance());
                                        maintenance_running = true;
                                        self.running_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                }
                                Err(e) => {
                                    error!(session = %self.writer.session_id, "recording Claude Code's output: {e:#}");
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(wake_at.unwrap_or_else(tokio::time::Instant::now)), if running.is_none() && wake_at.is_some() => {
                    if let Some((text, task_notice)) = self.claude.as_mut().and_then(|p| p.take_unwoken()) {
                        info!(session = %self.writer.session_id, "Claude Code didn't wake for its finished tasks: telling the agent");
                        let input = PendingInput {
                            command_id: format!("task:claude-wake:{}", uuid::Uuid::now_v7()),
                            text,
                            command: "task-notification".into(),
                            task_notice: Some(task_notice),
                            ..Default::default()
                        };
                        if let Err(e) = self.notice(input, false, true).await {
                            error!(session = %self.writer.session_id, "telling the agent about its tasks: {e:#}");
                            return;
                        }
                    }
                }
                _ = tokio::time::sleep_until(retry_at.unwrap_or_else(tokio::time::Instant::now)), if retry_at.is_some() => {
                    if let Err(e) = self.release_held().await {
                        error!(session = %self.writer.session_id, "delivering merged notices: {e:#}");
                        return;
                    }
                }
                _ = tokio::time::sleep_until(adoption_retry_at.unwrap_or_else(tokio::time::Instant::now)), if adoption_retry_at.is_some() => {}
                action = async { adoption.as_mut().unwrap().action.as_mut().unwrap().await }, if waiting_for_adoption_action => {
                    adoption.as_mut().unwrap().resume(action.expect("the gate retains its sender until a user acts"), adoption_budget);
                }
                _ = self.link.wait_connected(), if waiting_to_wake && !self.link.is_connected() => {}
                // Idle long enough, with no background tasks: close it.
                _ = tokio::time::sleep_until(idle_until.unwrap_or_else(tokio::time::Instant::now)), if running.is_none() && idle_until.is_some() => {
                    info!(session = %self.writer.session_id, "closing an idle Claude Code process");
                    self.claude.take().expect("checked").close().await;
                }
                done = async { (&mut running.as_mut().unwrap().0).await }, if running.is_some() => {
                    *running = None;
                    let maintenance = std::mem::take(&mut maintenance_running);
                    self.running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                    if !maintenance {
                        self.writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: false, ..Default::default() }));
                    }
                    match done {
                        Ok(Some((conv, claude))) => {
                            self.conv = Some(conv);
                            self.claude = claude;
                            if let Err(e) = self.schedule_notices().await {
                                error!(session = %self.writer.session_id, "checking unattended progress: {e:#}");
                                return;
                            }
                            if std::mem::take(&mut self.stopping) {
                                self.stopped_until = Some(std::time::Instant::now() + crate::claude_code::STOP_QUIET);
                            }
                            // A turn Claude Code started right after your stop,
                            // stopped before it showed anything: not a turn.
                            if !maintenance && self.claude.as_mut().is_some_and(|p| p.take_unshown()) {
                                self.turns -= 1;
                            }
                            if let Err(e) = self.settle_delegations().await {
                                error!(session = %self.writer.session_id, "reading a delegation's outcome: {e:#}");
                            }
                            if let Err(e) = self.record_remote_receipts().await {
                                error!(session = %self.writer.session_id, "recording subagent report receipts: {e:#}");
                                return;
                            }
                        }
                        // The supervisor rebuilds from the journal without
                        // dropping the mailbox, inbox or tool callbacks.
                        Ok(None) => return,
                        Err(e) => {
                            error!(session = %self.writer.session_id, "turn task panicked: {e}");
                            *recoverable = false;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Tells delegations whose input a turn took (no longer queued) how
    /// that turn ended: the last turn in the log. A subagent's turn that no
    /// call waited on goes to its parent as a notice.
    async fn settle_delegations(&mut self) -> anyhow::Result<()> {
        let Some(d) = self.spec.delegation.clone() else { return Ok(()) };
        if self.running_flag.load(std::sync::atomic::Ordering::SeqCst)
            || self
                .session_tools
                .background
                .list()
                .iter()
                .any(|task| task.running && crate::subagent::counted_background(task.notify, task.kind.as_str()))
        {
            // Live tasks can precede their durable start callback. The turn
            // end or terminal task event will recheck locally, without polling.
            self.local_report_retry_at = None;
            return Ok(());
        }
        let sid = self.writer.session_id.clone();
        // Recheck a handoff even if this actor never runs another turn, and
        // retry any journal operation below that failed transiently.
        self.local_report_retry_at = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1));
        let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        let pending = pending_local_reports(&sid, &events);
        if pending.is_empty() {
            self.local_report_retry_at = None;
            return Ok(());
        }
        let mut added = false;
        for call in pending.iter().flat_map(|(_, _, inputs)| inputs.iter().map(|u| &u.delegated_by)).filter(|id| !id.is_empty()) {
            added |= self.local_report_parent.calls.insert(call.clone());
        }
        if added {
            // A newly relevant call can predate the cursor (e.g. inputs
            // queued together). Rebuild evidence in bounded pages once.
            self.local_report_parent.after = 0;
            self.local_report_parent.events.clear();
        }
        if !self.local_report_parent.update(&self.journal, &d.parent_session_id, &sid).await? {
            return Ok(());
        }
        let parent = &self.local_report_parent.events;
        let parent_handle = self.manager.actors.lock().unwrap().get(&d.parent_session_id).cloned();
        let mut remaining = pending.len();
        for (command_id, end, inputs) in pending {
            let (settled, still): (Vec<_>, Vec<_>) =
                std::mem::take(&mut self.waiting).into_iter().partition(|(id, _)| inputs.iter().any(|u| &u.command_id == id));
            self.waiting = still;
            let outcome = local_report_outcome(&events[..=end]);
            let status = crate::subagent::status_of(outcome.reason, &outcome.report);
            let calls: Vec<_> = inputs.iter().filter_map(|u| LocalReportCall::of(u)).collect();
            let after = InlineChildReport::check_after(&calls, parent, self.local_report_parent.after);
            let inline = InlineChildReport { child: sid.clone(), status, calls, after };
            // Sending, rather than testing is_closed, decides ownership. A
            // waiter can disappear between those operations.
            let mut told = inline.recorded(parent)
                || parent.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::NoticeQueued(n)) if n.command_id == command_id));
            let mut handed = false;
            for (_, tx) in settled {
                handed |= tx.send(Outcome { reason: outcome.reason, report: outcome.report.clone(), error: outcome.error.clone() }).is_ok();
            }
            if !told && let Some(ack) = self.local_report_acks.get_mut(&command_id) {
                match ack.try_recv() {
                    Ok(()) => told = true,
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => continue,
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
                }
            }
            if !told {
                if handed || parent_handle.as_ref().is_some_and(|h| inline.still_running(h, &self.local_report_parent)) {
                    continue;
                }
                let wake = inputs
                    .iter()
                    .any(|u| u.wake || !u.delegated_by.is_empty() || u.command == "session-message" || u.command == "task-notification");
                let said = inputs
                    .iter()
                    .filter(|u| u.delegated_by.is_empty() && u.command != "task-notification")
                    .map(|u| {
                        crate::subagent::report_input(
                            &sid,
                            &u.command,
                            &u.text,
                            &u.from_session_id,
                            &u.from_title,
                            u.api_origin.as_ref(),
                            u.human_origin,
                        )
                    })
                    .collect::<Vec<_>>();
                let digest = if outcome.reason == TurnEndReason::Completed && !outcome.report.trim().is_empty() {
                    String::new()
                } else {
                    let status = crate::subagent::status_of(outcome.reason, &outcome.report);
                    crate::subagent::digest(&self.journal, &sid, status, "").await
                };
                let body = outcome.with_failure_cause(
                    &self.spec,
                    crate::subagent::report_body(&self.spec, &sid, outcome.reason, &outcome.report, &digest),
                );
                let text = crate::subagent::unasked_report(&sid, &said, &body);
                let task_notice = Some(crate::subagent::task_report_notice(&self.started.title, &sid, outcome.reason, &outcome.report));
                let (recorded, ack) = tokio::sync::oneshot::channel();
                let inline = (!inline.calls.is_empty()).then_some(inline);
                let msg = ActorMsg::ChildReport { command_id: command_id.clone(), text, wake, task_notice, inline, recorded };
                self.local_report_acks.insert(command_id, ack);
                let manager = self.manager.clone();
                let parent_id = d.parent_session_id.clone();
                tokio::spawn(async move {
                    match manager.actor_for(&parent_id).await {
                        Ok(Some(parent)) => {
                            let _ = parent.send(msg);
                        }
                        Ok(None) => warn!(session = %parent_id, "parent not found; subagent report stays pending"),
                        Err(e) => warn!(session = %parent_id, "subagent report stays pending: {e:#}"),
                    }
                });
                continue;
            }
            self.writer
                .append(session_event::Kind::SubagentReported(c6_proto::SubagentReported {
                    command_id: command_id.clone(),
                    child_session_id: sid.clone(),
                    status: crate::subagent::status_of(outcome.reason, &outcome.report).into(),
                    ..Default::default()
                }))
                .await?;
            self.local_report_acks.remove(&command_id);
            remaining -= 1;
        }
        if remaining == 0 {
            self.local_report_retry_at = None;
        }
        Ok(())
    }

    async fn record_remote_receipts(&mut self) -> anyhow::Result<()> {
        if self.remote_receipts.is_empty() {
            return Ok(());
        }
        let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        let mut next = 0;
        while let Some(receipt) = self.remote_receipts.get(next).cloned() {
            if report_recorded(&events, &receipt.call_id, &receipt.child_session_id, &receipt.status) {
                self.writer.append(session_event::Kind::SubagentReported(receipt)).await?;
                self.remote_receipts.remove(next);
            } else {
                next += 1;
            }
        }
        Ok(())
    }

    /// A remote subagent's turn ended (docs/remote-subagents.md): the call
    /// waiting on it gets the outcome. Without one (you messaged the
    /// subagent, or this worker restarted while a call waited) it comes as a
    /// notice, which wakes the session if a call had waited. The log then
    /// says it came, so the head stops sending it.
    async fn remote_report(&mut self, command_id: String, ended: c6_proto::DelegationEnded, busy: bool) -> anyhow::Result<()> {
        if self.withdrawn.contains(&ended.child_session_id) {
            self.writer
                .append(session_event::Kind::SubagentReported(c6_proto::SubagentReported {
                    command_id,
                    child_session_id: ended.child_session_id,
                    call_id: ended.call_id,
                    status: "withdrawn".into(),
                }))
                .await?;
            return Ok(());
        }
        if self.remote_receipts.iter().any(|r| r.command_id == command_id) {
            // Reconnected before the waiting tool recorded its result.
            // The source command remains pending; do not tell it twice.
            return Ok(());
        }
        if ended.task_notice.as_ref().is_some_and(|n| n.kind == "recovery" && n.status == "waiting") {
            // A resource observation is not the child's turn outcome. Do not
            // resolve its waiter, manufacture a completion envelope, or let
            // an empty call id match a previous successful child report.
            let reported = c6_proto::SubagentReported {
                command_id: command_id.clone(),
                call_id: String::new(),
                child_session_id: ended.child_session_id,
                status: "waiting".into(),
            };
            self.notice(
                PendingInput {
                    command_id: format!("subagent:{command_id}"),
                    text: ended.digest,
                    command: "task-notification".into(),
                    task_notice: ended.task_notice,
                    ..Default::default()
                },
                busy,
                ended.wake,
            )
            .await?;
            self.writer.append(session_event::Kind::SubagentReported(reported)).await?;
            return Ok(());
        }
        let reason = TurnEndReason::try_from(ended.reason).unwrap_or(TurnEndReason::Unspecified);
        let reported = c6_proto::SubagentReported {
            command_id,
            call_id: ended.call_id.clone(),
            child_session_id: ended.child_session_id.clone(),
            status: crate::subagent::status_of(reason, &ended.report).into(),
        };
        if reported.status == "lost"
            && let Some(incident) = crate::notice_guard::liveness_report_id(&reported.command_id, &reported.child_session_id)
            && let Some(notice) = ended.task_notice.as_ref().filter(|n| n.status == "lost" && n.session_id == reported.child_session_id)
        {
            // Persist the structured outcome before releasing a waiter. The
            // background runner only returns a string error and would otherwise
            // replace this with a generic failure. The terminal task event still
            // records its lifecycle, but cannot deliver this incident again.
            let input = PendingInput {
                command_id: format!("subagent:{incident}"),
                text: ended.digest.clone(),
                command: "task-notification".into(),
                task_notice: Some(notice.clone()),
                ..Default::default()
            };
            self.notice(input, busy, true).await?;
            self.writer.append(session_event::Kind::SubagentReported(reported)).await?;
            if !ended.call_id.is_empty()
                && let Some(waiter) = self.manager.take_remote_wait(&self.writer.session_id, &ended.call_id)
            {
                let _ = waiter.send(ended);
            }
            return Ok(());
        }
        let waiter = if ended.call_id.is_empty() { None } else { self.manager.take_remote_wait(&self.writer.session_id, &ended.call_id) };
        let unanswered = match waiter {
            Some(tx) => tx.send(ended).err(),
            None => Some(ended),
        };
        if let Some(ended) = unanswered {
            let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
            if report_recorded(&events, &reported.call_id, &reported.child_session_id, &reported.status) {
                self.writer.append(session_event::Kind::SubagentReported(reported)).await?;
                return Ok(());
            }
            let sid = &ended.child_session_id;
            let body = crate::subagent::report_body(&self.spec, sid, reason, &ended.report, &ended.digest);
            let (text, wake) = if ended.call_id.is_empty() {
                (crate::subagent::unasked_report(sid, &ended.said, &body), ended.wake)
            } else {
                (format!("Subagent {sid} ended the turn your call {} waited on:\n{body}", ended.call_id), true)
            };
            // Its id is the head's command's: a report sent again after a
            // restart is not told twice.
            let input = PendingInput {
                command_id: format!("subagent:{}", reported.command_id),
                text,
                command: "task-notification".into(),
                task_notice: ended.task_notice,
                ..Default::default()
            };
            self.notice(input, busy, wake).await?;
        } else {
            self.remote_receipts.push(reported);
            return Ok(());
        }
        self.writer.append(session_event::Kind::SubagentReported(reported)).await?;
        Ok(())
    }

    /// A switch away from Claude Code is next while Claude Code's own
    /// background tasks run: it waits for them, since they would stop with
    /// its process (docs/background.md §8).
    fn handoff_waits(&self) -> bool {
        let leaving =
            self.inbox.lock().unwrap().queue.front().and_then(|(_, m)| m.as_ref()).is_some_and(|m| m.connection_kind != "claude_code");
        leaving && self.runs_claude_code() && self.claude.as_ref().is_some_and(|p| p.tasks_running())
    }

    /// How long its idle Claude Code process stays up with no tasks.
    fn claude_idle(&self) -> std::time::Duration {
        match self.spec.claude_code_idle_minutes {
            0 => crate::claude_code::IDLE,
            m => std::time::Duration::from_secs(u64::from(m) * 60),
        }
    }

    /// A durable acknowledgement resets the guard after an explicit Continue.
    async fn continue_notices(&mut self, explicit: bool) -> anyhow::Result<()> {
        let paused = self.notice_pause.take().is_some();
        let warning = self.notice_warning.take().is_some();
        if paused || (explicit && (warning || !self.deferred.is_empty())) {
            self.writer
                .append(session_event::Kind::Notice(c6_proto::Notice {
                    title: crate::notice_guard::CONTINUED_TITLE.into(),
                    message: serde_json::json!({
                        "notice_quiet_seconds": self.spec.notice_quiet_seconds,
                        "notice_max_wait_seconds": self.spec.notice_max_wait_seconds,
                        "notice_no_progress_limit": self.spec.notice_no_progress_limit,
                        "notice_failure_relaunch_limit": self.spec.notice_failure_relaunch_limit,
                        "notice_tokens_per_hour": self.spec.notice_tokens_per_hour,
                        "notice_loop_backoff_rounds": self.spec.notice_loop_backoff_rounds,
                        "notice_loop_backoff_max_seconds": self.spec.notice_loop_backoff_max_seconds,
                        "max_subagent_depth": self.spec.max_subagent_depth,
                        "max_running_subagents": self.spec.max_running_subagents,
                        "claude_code_trim_enabled": self.spec.claude_code_trim_enabled.unwrap_or(true),
                        "claude_code_trim_at": self.spec.claude_code_trim_at,
                    })
                    .to_string(),
                    priority: "low".into(),
                    quiet: true,
                }))
                .await?;
        }
        self.notice_arrivals.clear();
        self.notice_deadline = None;
        self.notice_ready.clear();
        Ok(())
    }

    fn queued_notice(input: &PendingInput, wake: bool, reason: String, deliver_at_ms: i64) -> c6_proto::NoticeQueued {
        c6_proto::NoticeQueued {
            command_id: input.command_id.clone(),
            text: input.text.clone(),
            command: input.command.clone(),
            wake,
            prompt: input.prompt.clone(),
            from_session_id: input.from_session_id.clone(),
            from_title: input.from_title.clone(),
            hops: input.hops,
            task_notice: input.task_notice.clone(),
            deliver_at_ms,
            reason,
        }
    }

    /// Rebuild decisions from durable facts, including group membership and real
    /// usage. No counter of wakes limits a productive fan-out.
    async fn schedule_notices(&mut self) -> anyhow::Result<()> {
        self.notice_deadline = None;
        self.notice_ready.clear();
        self.notice_warning = None;
        if self.deferred.is_empty() {
            return Ok(());
        }
        let mut raw = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        let now_ms = c6_core::now_ms();
        use crate::notice_guard::{Guard, Limits};
        let limits = Limits {
            tokens_per_hour: self.spec.notice_tokens_per_hour,
            evidence_observations: self.spec.notice_no_progress_limit,
            failure_observations: self.spec.notice_failure_relaunch_limit,
            rounds: self.spec.notice_loop_backoff_rounds,
            max_seconds: self.spec.notice_loop_backoff_max_seconds,
        };
        let events = c6_agent::effective_events(&raw);
        let mut groups: HashMap<String, Vec<&c6_proto::SubagentSpawned>> = HashMap::new();
        let mut children = HashMap::new();
        let mut grouped_runs = HashMap::new();
        let mut grouped_tasks = HashMap::new();
        let mut session_reports = HashMap::new();
        let mut finished = HashSet::new();
        let mut failed = HashSet::new();
        let mut previous = HashMap::new();
        let mut group_calls: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut pending_group_calls = HashMap::new();
        let mut launched_tasks = HashMap::new();
        let mut linked_tasks = HashSet::new();
        let mut rejected_calls = HashSet::new();
        for e in &events {
            match &e.kind {
                Some(session_event::Kind::Assistant(a)) => {
                    for block in &a.blocks {
                        let Some(c6_proto::content_block::Block::ToolUse(t)) = &block.block else { continue };
                        pending_group_calls.remove(&t.id);
                        if (c6_tools::Task::is_task(&t.name) || t.name == "mcp__carbon6__Task")
                            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&t.input_json)
                            && let Some(group) = v["group"].as_str().map(str::trim).filter(|g| !g.is_empty())
                        {
                            let description = v["description"].as_str().unwrap_or_default();
                            group_calls.entry(group.to_owned()).or_default().insert(t.id.clone(), description.to_owned());
                            rejected_calls.remove(&t.id);
                            pending_group_calls.insert(t.id.clone(), (group.to_owned(), v["wake"].as_str().unwrap_or("all").to_owned()));
                        }
                    }
                }
                Some(session_event::Kind::ToolResult(t)) if t.is_error => {
                    rejected_calls.insert(t.call_id.clone());
                }
                Some(session_event::Kind::ToolResult(t)) => {
                    if t.output.starts_with("Started the subagent in the background as ")
                        && let Some(task) = background_report_task(&t.output)
                        && let Some(group) = pending_group_calls.remove(&t.call_id)
                    {
                        launched_tasks.insert(task, (group.0, group.1, t.call_id.as_str()));
                    }
                }
                Some(session_event::Kind::SubagentSpawned(s)) => {
                    if !s.background_task_id.is_empty() {
                        linked_tasks.insert(s.background_task_id.as_str());
                    }
                    if s.group.is_empty() {
                        children.remove(&s.child_session_id);
                    } else {
                        children.insert(s.child_session_id.clone(), s);
                        // A continuation changes where new session-only reports
                        // belong, not the membership of a previous launch.
                        grouped_runs.insert(grouped_run_key(s), s);
                        if !s.background_task_id.is_empty() {
                            grouped_tasks.insert(s.background_task_id.as_str(), s);
                        }
                    }
                }
                Some(session_event::Kind::NoticeQueued(n)) => {
                    // Attribute a session-only report where it first arrived.
                    // Later scheduling snapshots must not move it to a new run.
                    if !n.command_id.starts_with("task:")
                        && !n.task_notice.as_ref().is_some_and(|t| t.kind == "recovery")
                        && !previous.contains_key(&n.command_id)
                        && let Some(s) = n.task_notice.as_ref().and_then(|t| children.get(&t.session_id))
                    {
                        session_reports.insert(n.command_id.as_str(), *s);
                    }
                    previous.insert(n.command_id.clone(), n.clone());
                }
                _ => {}
            }
        }
        // Tool background completions name the task, while direct child and
        // remote reports name the session. Both belong to the same fan-in.
        let mut orphan_outcomes = HashMap::new();
        for n in previous.values() {
            if n.task_notice.as_ref().is_some_and(|t| t.kind == "recovery") {
                continue; // observations do not complete fan-in members
            }
            let failure = n.task_notice.as_ref().is_some_and(|t| matches!(t.status.as_str(), "failed" | "lost" | "error"));
            if let Some((task, state)) = task_completion(&n.command_id) {
                let failure = failure || matches!(state, "failed" | "lost");
                if let Some(s) = grouped_tasks.get(task) {
                    finished.insert(grouped_run_key(s));
                    if failure {
                        failed.insert(grouped_run_key(s));
                    }
                } else if !linked_tasks.contains(task) && launched_tasks.contains_key(task) {
                    // A detached runner can fail before it has a child to link.
                    *orphan_outcomes.entry(task).or_insert(false) |= failure;
                }
            } else if let Some(s) = session_reports.get(n.command_id.as_str()) {
                finished.insert(grouped_run_key(s));
                if failure {
                    failed.insert(grouped_run_key(s));
                }
            }
        }
        let mut orphan_counts = HashMap::new();
        let mut finished_orphan_calls = HashSet::new();
        for (task, failure) in orphan_outcomes {
            let (group, _, call) = &launched_tasks[task];
            let (done, failed) = orphan_counts.entry(group.as_str()).or_insert((0, 0));
            finished_orphan_calls.insert((group.as_str(), *call));
            *done += 1;
            *failed += usize::from(failure);
        }
        for s in grouped_runs.values() {
            groups.entry(s.group.clone()).or_default().push(*s);
        }
        let now = tokio::time::Instant::now();
        let quiet = std::time::Duration::from_secs(u64::from(if self.spec.notice_quiet_seconds == 0 {
            c6_api::DEFAULT_NOTICE_QUIET_SECONDS
        } else {
            self.spec.notice_quiet_seconds
        }));
        let max_wait = std::time::Duration::from_secs(u64::from(if self.spec.notice_max_wait_seconds == 0 {
            c6_api::DEFAULT_NOTICE_MAX_WAIT_SECONDS
        } else {
            self.spec.notice_max_wait_seconds
        }));
        let member_of = |input: &PendingInput| {
            if let Some((task, _)) = task_completion(&input.command_id) {
                if let Some(s) = grouped_tasks.get(task) {
                    return Some((s.group.as_str(), s.wake.as_str()));
                }
                let (group, wake, _) = launched_tasks.get(task).filter(|_| !linked_tasks.contains(task))?;
                Some((group.as_str(), wake.as_str()))
            } else {
                session_reports.get(input.command_id.as_str()).map(|s| (s.group.as_str(), s.wake.as_str()))
            }
        };
        // A group barrier or a non-waking watcher is not the first arrival
        // of an unrelated merge window, however long it has been waiting.
        let arrivals: Vec<_> = self
            .deferred
            .iter()
            .filter(|(input, wake)| *wake && member_of(input).is_none())
            .filter_map(|(input, _)| self.notice_arrivals.get(&input.command_id).copied())
            .collect();
        let merge_at =
            (arrivals.iter().max().copied().unwrap_or(now) + quiet).min(arrivals.iter().min().copied().unwrap_or(now) + max_wait);
        let mut candidates = Vec::new();
        let mut eligible = HashSet::new();
        let count = self.deferred.len();
        let mut represented = HashSet::new();
        for (input, wake) in &mut self.deferred {
            let member = member_of(input);
            // One representative carries each barrier's live count and ETA.
            // Rewriting every report on every arrival would copy O(N²) report
            // bytes into the journal during the large fan-outs this supports.
            let key = if !*wake {
                input.command_id.clone()
            } else {
                member.map_or_else(|| "merge".to_owned(), |(group, _)| format!("group:{group}"))
            };
            let representative = represented.insert(key);
            let (ready, immediate, waiting) = if let Some((group, wake)) = member {
                let members = groups.get(group).map_or(&[][..], Vec::as_slice);
                let calls = group_calls.get(group);
                let (orphan_done, orphan_failed) = orphan_counts.get(group).copied().unwrap_or_default();
                // Task acknowledges background launch before its detached
                // runner necessarily links the child. Count logged calls too,
                // so an already acknowledged sibling cannot be missed.
                let total = calls
                    .map_or(0, |calls| calls.keys().filter(|id| !rejected_calls.contains(*id)).count())
                    .max(members.len() + orphan_done);
                let done = members.iter().filter(|s| finished.contains(&grouped_run_key(s))).count() + orphan_done;
                let failures = members.iter().filter(|s| failed.contains(&grouped_run_key(s))).count() + orphan_failed;
                let target = match wake {
                    "any" => 1,
                    w if w.starts_with("count:") => w[6..].parse::<usize>().unwrap_or(total),
                    _ => total,
                };
                let failure = input.task_notice.as_ref().is_some_and(|t| matches!(t.status.as_str(), "failed" | "lost" | "error"));
                // A count may exceed the number of launches (e.g. a call was
                // rejected). Once all known members have settled, no more
                // reports can arrive to satisfy the original threshold.
                let settled_short = total > 0 && done == total && done < target;
                let ready = failure || failures > 0 || done >= target || settled_short;
                if ready {
                    let mut summary = format!("group {group}: {done} of {total} finished, {failures} failed");
                    if settled_short {
                        summary.push_str(&format!("; wake threshold of {target} not met, but all members finished"));
                    }
                    let body = if input.text.starts_with(&format!("group {group}:")) {
                        input.text.split_once("\n\n").map_or(input.text.as_str(), |(_, body)| body)
                    } else {
                        &input.text
                    };
                    input.text = format!("{summary}\n\n{body}");
                }
                let waiting = if ready {
                    "collecting reports for delivery".into()
                } else if representative {
                    let linked_calls: HashSet<_> = members.iter().map(|s| s.call_id.as_str()).collect();
                    let mut descriptions: Vec<_> = members
                        .iter()
                        .filter(|s| !finished.contains(&grouped_run_key(s)))
                        .map(|s| s.description.as_str())
                        .chain(calls.into_iter().flatten().filter_map(|(id, description)| {
                            (!linked_calls.contains(id.as_str())
                                && !finished_orphan_calls.contains(&(group, id.as_str()))
                                && !rejected_calls.contains(id))
                            .then_some(description.as_str())
                        }))
                        .map(str::trim)
                        .filter(|description| !description.is_empty())
                        .collect();
                    descriptions.sort_unstable();
                    let pending = if descriptions.is_empty() {
                        String::new()
                    } else {
                        let suffix = if descriptions.len() > 3 { ", …" } else { "" };
                        descriptions.truncate(3);
                        format!(" ({}{suffix})", descriptions.join(", "))
                    };
                    let remaining = target.min(total).saturating_sub(done);
                    let threshold = if target > total { format!("; wake threshold is {target}") } else { String::new() };
                    format!("group {group}: {done} of {total} finished; waiting for {remaining}{pending}{threshold}")
                } else {
                    format!("waiting for group {group}")
                };
                (ready, true, waiting)
            } else {
                let failure = input.task_notice.as_ref().is_some_and(|t| matches!(t.status.as_str(), "failed" | "lost" | "error"));
                (true, failure, format!("merging {count} reports"))
            };
            let at = (*wake && ready).then_some(if immediate { now } else { merge_at });
            if at.is_some() {
                eligible.insert(input.command_id.clone());
            }
            let reason = if !*wake {
                "waiting for your next message".into()
            } else if representative || member.is_some() {
                waiting
            } else {
                "merging reports".into()
            };
            candidates.push((Self::queued_notice(input, *wake, reason, 0), at, representative));
        }

        // A fresh report behind an unfinished group cannot be delivered with
        // this batch and therefore cannot clear a loop hold on the batch.
        let mut guard = crate::notice_guard::evaluate(&raw, now_ms, limits, &eligible);
        let repetition_hold = matches!(&guard, Guard::Warn(_) | Guard::Backoff { .. })
            || matches!(&guard, Guard::Pause(reason) if !reason.starts_with("paused: spending limit"));
        if repetition_hold && self.started.ssh_target.is_empty() {
            // Shells and delegated children can change files without an Edit
            // tool event. Before intervening, check the current contents,
            // not merely the checkpoint from before its last completed turn.
            match self.checkpoints.fingerprint().await {
                Ok(tree) => {
                    let previous = raw.iter().rev().find_map(|event| match &event.kind {
                        Some(session_event::Kind::Notice(n)) if n.quiet && n.title == crate::notice_guard::WORKSPACE_TREE_TITLE => {
                            Some(n.message.as_str())
                        }
                        Some(session_event::Kind::Checkpoint(c)) if c.error.is_empty() && !c.tree_id.is_empty() => Some(c.tree_id.as_str()),
                        _ => None,
                    });
                    if previous != Some(tree.as_str()) {
                        self.writer
                            .append(session_event::Kind::Notice(c6_proto::Notice {
                                title: crate::notice_guard::WORKSPACE_TREE_TITLE.into(),
                                message: tree,
                                priority: "low".into(),
                                quiet: true,
                            }))
                            .await?;
                        raw = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
                        guard = crate::notice_guard::evaluate(&raw, now_ms, limits, &eligible);
                    }
                }
                Err(error) => warn!(session = %self.writer.session_id, %error, "checking file progress before a repetition hold"),
            }
        }
        let (pause, backoff) = match guard {
            Guard::Proceed => (None, None),
            Guard::Pause(reason) => (Some(reason), None),
            Guard::Warn(evidence) => {
                self.notice_warning = Some(evidence);
                (None, None)
            }
            Guard::Backoff { until_ms, evidence } => {
                self.notice_warning = Some(evidence);
                (None, Some(until_ms))
            }
        };
        if let Some(reason) = &pause
            && self.notice_pause.as_ref() != Some(reason)
        {
            self.writer
                .append(session_event::Kind::Notice(c6_proto::Notice {
                    title: format!("{} paused", self.started.title),
                    message: if reason.starts_with("paused: spending limit") {
                        format!("{reason}. Continue with Deliver now or your next message; change the cap in Agents → Limits.")
                    } else {
                        format!("{reason}. Continue with Deliver now or your next message.")
                    },
                    priority: "normal".into(),
                    quiet: false,
                }))
                .await?;
        }
        self.notice_pause = pause;

        // Scheduling snapshots supersede earlier ones by command_id on
        // replay; only a User event is a delivery receipt.
        for (mut updated, at, representative) in candidates {
            if let Some(reason) = &self.notice_pause {
                updated.reason.clone_from(reason);
            } else if let Some(at) = at {
                self.notice_ready.insert(updated.command_id.clone());
                let at = backoff.map_or(at, |until| {
                    let delay = until.saturating_sub(now_ms).clamp(0, limits.max_delay_ms());
                    at.max(now + std::time::Duration::from_millis(delay as u64))
                });
                self.notice_deadline = Some(self.notice_deadline.map_or(at, |old| old.min(at)));
                if representative {
                    updated.deliver_at_ms = now_ms + at.saturating_duration_since(now).as_millis() as i64;
                }
                if backoff.is_some_and(|until| until > now_ms)
                    && let Some(evidence) = &self.notice_warning
                {
                    updated.reason = format!("collecting reports; slowing automatic delivery after {evidence}");
                }
            }
            if previous.get(&updated.command_id) != Some(&updated) {
                self.writer.append(session_event::Kind::NoticeQueued(updated)).await?;
            }
        }
        Ok(())
    }

    async fn release_held(&mut self) -> anyhow::Result<()> {
        if !self.link.is_connected() || self.notice_pause.is_some() {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if self.notice_deadline.is_none_or(|at| at > now) {
            return Ok(());
        }
        // Only release eligible members: unrelated groups keep their barrier.
        let mut waiting = Vec::new();
        let mut warning = self.notice_warning.take();
        for (mut n, wake) in std::mem::take(&mut self.deferred) {
            if self.notice_ready.contains(&n.command_id) || !wake {
                if wake && let Some(evidence) = warning.take() {
                    add_notice_loop_warning(&self.spec, &mut n, &evidence);
                }
                self.notice_arrivals.remove(&n.command_id);
                self.inbox.lock().unwrap().push(n, None);
            } else {
                waiting.push((n, wake));
            }
        }
        self.deferred = waiting;
        self.notice_deadline = None;
        self.notice_ready.clear();
        Ok(())
    }

    /// A notice for the agent (docs/background.md §5). It is recorded first
    /// (`NoticeQueued`), so a reload delivers it if no turn took it, then
    /// delivered (`deliver_notice`). One already recorded is ignored.
    async fn notice(&mut self, input: PendingInput, busy: bool, wake: bool) -> anyhow::Result<()> {
        if self.withdrawn_input(&input) {
            return Ok(());
        }
        if !self.noticed.insert(input.command_id.clone()) {
            return Ok(());
        }
        // The first durable receipt precedes scheduling. Do not briefly mark
        // an automatic input as needing the user while its snapshot catches up.
        let reason = if wake || busy { "collecting reports for delivery" } else { "waiting for your next message" };
        let queued = Self::queued_notice(&input, wake, reason.into(), 0);
        if let Err(e) = self.writer.append(session_event::Kind::NoticeQueued(queued)).await {
            self.noticed.remove(&input.command_id);
            // The head does not redeliver tool callbacks. Keep this input
            // until the supervisor can record it, even though its first
            // durable queue marker could not be written.
            if !self.unrecorded_notices.iter().any(|(n, _)| n.command_id == input.command_id) {
                self.unrecorded_notices.push((input, wake));
            }
            return Err(e);
        }
        self.deliver_notice(input, busy, wake, true).await
    }

    /// Delivers a notice: it joins a running turn at its next step; in an
    /// idle session it joins a merge/group window if `wake`, else it waits
    /// for the next explicit input. Guards explain every pause to the user.
    async fn deliver_notice(&mut self, input: PendingInput, busy: bool, wake: bool, _announce: bool) -> anyhow::Result<()> {
        if self.withdrawn_input(&input) {
            return Ok(());
        }
        // What the agent does next follows from this message's chain.
        if input.hops > 0 {
            self.session_tools.set_message_hops(input.hops);
        }
        if busy || !self.inbox.lock().unwrap().queue.is_empty() {
            let mut inbox = self.inbox.lock().unwrap();
            // A delegated or other independently started turn can accept new
            // input, but does not get to flush a guarded backlog as a side effect.
            if self.notice_pause.is_none() && self.notice_warning.is_none() {
                self.notice_arrivals.clear();
                self.notice_deadline = None;
                self.notice_ready.clear();
                for (held, _) in std::mem::take(&mut self.deferred) {
                    inbox.push(held, None);
                }
            }
            inbox.push(input, None);
            return Ok(());
        }
        self.notice_arrivals.entry(input.command_id.clone()).or_insert_with(tokio::time::Instant::now);
        if !self.deferred.iter().any(|(n, _)| n.command_id == input.command_id) {
            self.deferred.push((input, wake));
        }
        self.schedule_notices().await
    }

    /// Logs a background task's start or end and, for an end the agent
    /// should hear about, queues the notice (`notice`). Its id names the
    /// task and how it ended, so the same end is never told twice.
    async fn task_event(
        &mut self,
        record: BackgroundTask,
        notice: Option<String>,
        output: &str,
        busy: bool,
        wake: bool,
    ) -> anyhow::Result<()> {
        self.pending_task_event = Some(PendingTaskEvent { record, notice, output: output.into(), wake, logged: false });
        self.write_pending_task_event(busy).await
    }

    async fn write_pending_task_event(&mut self, busy: bool) -> anyhow::Result<()> {
        let Some(pending) = self.pending_task_event.as_ref() else { return Ok(()) };
        let record = pending.record.clone();
        let completed = record.state != "started" && crate::subagent::counted_background(record.notify, &record.kind);
        let withdrawn = self.withdrawn.contains(&record.task_id);
        let covered = if record.state != "started" && record.kind == "agent" && pending.notice.is_some() {
            // Also check the durable receipt on recovery, when the original
            // waiter may be gone and the callback is a synthetic task loss.
            let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
            crate::notice_guard::liveness_covered_tasks(&events).contains(record.task_id.as_str())
        } else {
            false
        };
        let command_id = format!("task:{}:{}", record.task_id, record.state);
        let task_notice = (!withdrawn && !covered).then(|| background_task_notice(&record, &pending.output));
        let notice = pending.notice.clone();
        let wake = pending.wake;
        if !pending.logged {
            self.writer.append(session_event::Kind::BackgroundTask(record)).await?;
            self.pending_task_event.as_mut().expect("retained callback").logged = true;
        }
        if !withdrawn
            && !covered
            && let Some(text) = notice
        {
            let input = PendingInput { command_id, text, command: "task-notification".into(), task_notice, ..Default::default() };
            self.notice(input, busy, wake).await?;
        }
        self.pending_task_event = None;
        if completed && !busy {
            self.settle_delegations().await?;
        }
        Ok(())
    }

    /// You stopped its Claude Code turn just now, or are stopping the one
    /// `running`: what Claude Code loses meanwhile is the stop's doing.
    fn after_stop(&self, running: bool) -> bool {
        (running && self.stopping) || self.stopped_until.is_some_and(|t| std::time::Instant::now() < t)
    }

    /// Prepare a local workspace, commit the event, then install it in this
    /// actor before anything else can start a turn.
    async fn freeze_move(&mut self, move_id: &str, epoch: u64) -> anyhow::Result<()> {
        let sid = self.writer.session_id.clone();
        anyhow::ensure!(
            self.spec.delegation.is_none() && self.spec.depth == 0 && self.started.ssh_target.is_empty(),
            "only top-level local sessions can move between workers"
        );
        anyhow::ensure!(
            self.spec.mcp_servers_json.trim().is_empty() || self.spec.mcp_servers_json.trim() == "[]",
            "moving sessions with configured MCP servers is not supported"
        );
        anyhow::ensure!(
            !self.session_tools.background.list().iter().any(|t| t.running),
            "stop or wait for running background tasks before moving workers"
        );
        let events = self.journal.events_after(&sid, 0, u32::MAX).await?;
        self.manager.rewind_preflight(&sid, &events, 0, true).await?;
        // Native Claude remains alive between turns. Releasing a session must
        // stop that process (including its keeper/descendants), not just drop
        // the actor. This does not reset or fork its native conversation.
        if let Some(mut process) = self.claude.take() {
            anyhow::ensure!(process.take_unsent().is_empty(), "native Claude still owns unsent inputs");
            process.stop_for_rewind().await?;
        }
        crate::claude_code::stop_held_for_rewind(&self.manager.claude_dir().join("held"), &sid, &self.journal).await?;
        crate::claude_code::confirm_stopped(&self.journal, &sid, std::time::Duration::from_secs(3)).await?;
        self.journal.block_ownership(&sid, epoch, move_id).await?;
        Ok(())
    }

    async fn change_place(&mut self, command_id: &str, request: c6_proto::ChangePlace) -> anyhow::Result<()> {
        let _build_use = self.journal.build_folder_lifecycle.clone().read_owned().await;
        let sid = self.writer.session_id.clone();
        anyhow::ensure!(
            self.spec.delegation.is_none()
                && self.spec.depth == 0
                && self.started.ssh_target.is_empty()
                && !self.runs_claude_code()
                && self.claude.is_none(),
            "Changing place is only supported for top-level local sessions in carbon6's own loop"
        );
        anyhow::ensure!(
            self.spec.mcp_servers_json.trim().is_empty() || self.spec.mcp_servers_json.trim() == "[]",
            "Changing place with configured MCP servers is not supported yet"
        );
        let mut events = self.journal.events_after(&sid, 0, u32::MAX).await?;
        if events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::PlaceChanged(p)) if p.command_id == command_id)) {
            return Ok(()); // The command crossed a reconnect after committing.
        }
        let current =
            events.iter().rev().find(|e| matches!(e.kind, Some(session_event::Kind::Started(_) | session_event::Kind::PlaceChanged(_))));
        anyhow::ensure!(
            current.is_some_and(|e| e.seq == request.expected_place_seq),
            "This session's place changed; refresh and try again"
        );
        anyhow::ensure!(
            !self.session_tools.background.list().iter().any(|task| task.running),
            "Stop or wait for running tasks before changing place"
        );
        self.manager.rewind_preflight(&sid, &events, 0, true).await?;
        // Do not start arbitrary external hooks during preparation. Apply the
        // same policy and tool configuration in the new directory after commit.
        let place_worker = events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::PlaceChanged(p)) => {
                    p.to.as_ref().filter(|p| !p.worker_id.is_empty()).map(|p| p.worker_id.clone())
                }
                _ => None,
            })
            .unwrap_or_default();
        let from = c6_proto::SessionPlace {
            worker_id: place_worker.clone(),
            cwd: self.started.cwd.clone(),
            repo: self.started.repo.clone(),
            repo_root: self.started.repo_root.clone(),
            worktree_branch: self.started.worktree_branch.clone(),
        };
        let scratch = self.manager.state_dir.join("scratch").join(&sid);
        let is_scratch = scratch.canonicalize().is_ok_and(|p| p == Path::new(&from.cwd)) && from.worktree_branch.is_empty();
        let mut entered = None;
        let mut to = match request.action.as_str() {
            "target" => {
                anyhow::ensure!(!request.copy_scratch || is_scratch, "Folder: only a scratch workspace can be copied");
                let cwd = crate::expand_home(&request.cwd);
                anyhow::ensure!(cwd.is_absolute(), "Folder: choose an absolute folder on this worker");
                let cwd = cwd.canonicalize().context("Folder: choose an existing folder on this worker")?;
                anyhow::ensure!(cwd.is_dir(), "Folder: choose a directory");
                if let Some(spec) = &request.worktree_spec {
                    let made = crate::worktree::enter(&self.manager.worktrees, &sid, &cwd, spec)
                        .await
                        .map_err(|e| anyhow::anyhow!("Worktree: {e}"))?;
                    let to = c6_proto::SessionPlace {
                        cwd: made.made.cwd.display().to_string(),
                        repo_root: made.made.root.clone(),
                        worktree_branch: made.made.branch.clone(),
                        ..Default::default()
                    };
                    entered = Some(made);
                    to
                } else {
                    let (repo_root, worktree_branch) = crate::worktree::existing_place(&cwd).await.map_err(anyhow::Error::msg)?;
                    c6_proto::SessionPlace { cwd: cwd.display().to_string(), repo_root, worktree_branch, ..Default::default() }
                }
            }
            "" | "folder" => {
                anyhow::ensure!(from.worktree_branch.is_empty(), "Leave the worktree before choosing another folder");
                anyhow::ensure!(!request.copy_scratch || is_scratch, "Only a scratch workspace can be copied on a folder move");
                c6_proto::SessionPlace { cwd: request.cwd.clone(), ..Default::default() }
            }
            "worktree" => {
                anyhow::ensure!(from.worktree_branch.is_empty(), "This session already uses a worktree; leave it first");
                let made = crate::worktree::enter(
                    &self.manager.worktrees,
                    &sid,
                    Path::new(&from.cwd),
                    &request.worktree_spec.clone().unwrap_or_default(),
                )
                .await
                .map_err(anyhow::Error::msg)?;
                let to = c6_proto::SessionPlace {
                    worker_id: place_worker.clone(),
                    cwd: made.made.cwd.display().to_string(),
                    repo: from.repo.clone(),
                    repo_root: made.made.root.clone(),
                    worktree_branch: made.made.branch.clone(),
                };
                entered = Some(made);
                to
            }
            "leave_worktree" => {
                anyhow::ensure!(!from.worktree_branch.is_empty() && !from.repo_root.is_empty(), "This session is not in a worktree");
                let cwd = crate::worktree::leave(Path::new(&from.cwd), Path::new(&from.repo_root), &from.worktree_branch, request.merge)
                    .await
                    .map_err(anyhow::Error::msg)?;
                c6_proto::SessionPlace { cwd: cwd.display().to_string(), ..Default::default() }
            }
            "back" => {
                anyhow::ensure!(request.previous_place_seq < request.expected_place_seq, "Choose an earlier place");
                events
                    .iter()
                    .find(|e| e.seq == request.previous_place_seq)
                    .and_then(|e| match &e.kind {
                        Some(session_event::Kind::Started(s)) => Some(c6_proto::SessionPlace {
                            worker_id: place_worker.clone(),
                            cwd: s.cwd.clone(),
                            repo: s.repo.clone(),
                            repo_root: s.repo_root.clone(),
                            worktree_branch: s.worktree_branch.clone(),
                        }),
                        Some(session_event::Kind::PlaceChanged(p)) => p.to.clone(),
                        _ => None,
                    })
                    .ok_or_else(|| anyhow::anyhow!("That place is not in this session's history"))?
            }
            _ => anyhow::bail!("This worker does not support that placement action"),
        };
        to.worker_id = place_worker;
        let prepared = async {
            let cwd = crate::expand_home(&to.cwd);
            anyhow::ensure!(cwd.is_absolute(), "Choose an absolute folder on this worker");
            let cwd = cwd.canonicalize()?;
            anyhow::ensure!(cwd.is_dir() && cwd != Path::new(&from.cwd), "Choose a different existing folder");
            to.cwd = cwd.to_str().ok_or_else(|| anyhow::anyhow!("The destination path is not valid UTF-8"))?.to_owned();
            if request.action == "back" && !to.worktree_branch.is_empty() {
                crate::worktree::validate_recorded_worktree(&cwd, Path::new(&to.repo_root), &to.worktree_branch)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            let copied = if request.copy_scratch {
                let source = PathBuf::from(&from.cwd);
                let destination = cwd.clone();
                Some(
                    tokio::task::spawn_blocking(move || crate::place_files::PreparedCopy::prepare(&source, &destination))
                        .await?
                        .map_err(anyhow::Error::msg)?,
                )
            } else {
                None
            };
            anyhow::Ok((cwd, copied))
        }
        .await;
        let (cwd, copied) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(entered) = &entered {
                    crate::worktree::discard(&entered.made).await;
                }
                return Err(error);
            }
        };
        to.repo = repo_slug(&cwd).await;
        let labels = session_labels(&self.spec.labels, &to.repo, &to.cwd, &self.started.home, &spawn_model(&self.spec), "");
        let date = time::OffsetDateTime::now_utc().date().to_string();
        let base_prompt = build_system_prompt(&self.spec.base_prompt, &cwd, &date);
        let mut preparing = self.spec.clone();
        preparing.hooks_json.clear();
        let built = own_runtime(&self.link, &self.on_behalf_of, &preparing, &cwd, &labels, base_prompt, "resume").await;
        if !built.errors.is_empty() {
            if let Some(entered) = &entered {
                crate::worktree::discard(&entered.made).await;
            }
            anyhow::bail!("Could not prepare session tools: {}", built.errors.join("; "));
        }
        let notice = request.notice_template.replace("{{from}}", &from.cwd).replace("{{to}}", &to.cwd);
        let place = c6_proto::PlaceChanged {
            ownership_epoch: self.writer.ownership_epoch,
            native_resume_id: String::new(),
            command_id: command_id.to_owned(),
            from: Some(from),
            to: Some(to),
            system_prompt: built.prompt,
            tools: built.tools.iter().map(|t| t.spec().name).collect(),
            tool_profile: built.profile.as_str().to_owned(),
            notice,
            copied_paths: copied.as_ref().map(|p| p.paths().to_vec()).unwrap_or_default(),
            uncommitted_files: entered.as_ref().map_or(0, |e| e.changed_paths.len() as u32),
            setup_log: entered.as_ref().map(|e| e.setup_log.clone()).unwrap_or_default(),
        };
        let kind = session_event::Kind::PlaceChanged(place.clone());
        let seq = match self.writer.append(kind.clone()).await {
            Ok(seq) => seq,
            Err(e) => {
                if let Some(entered) = &entered {
                    crate::worktree::discard(&entered.made).await;
                }
                return Err(e);
            }
        };
        if let Some(copied) = copied {
            copied.commit();
        }
        apply_place(&mut self.started, &place);
        self.session_tools.relocate(cwd.clone());
        self.checkpoints = Arc::new(Checkpoints::new(&self.manager.state_dir, &sid, &cwd));
        self.hooks = crate::hooks::HookRunner::new(&sid, &cwd, &self.spec.hooks_json, &labels).map(Arc::new);
        let (approval_log, mut approvals) = mpsc::unbounded_channel();
        self.policies = crate::policy::Policies::new(&self.spec.policies_json, &labels, approval_log);
        if let Some(policies) = &self.policies {
            policies.set_judge(self.session_tools.head());
        }
        if let Some(actor) = self.manager.live(&sid) {
            tokio::spawn(async move {
                while let Some(approval) = approvals.recv().await {
                    if actor.send(ActorMsg::Approval(approval)).is_err() {
                        break;
                    }
                }
            });
        }
        self.session_tools.set_hooks(own_loop_hooks(&self.policies, &self.hooks));
        self.tools = built.tools;
        events.push(SessionEvent { seq, ts_ms: c6_core::now_ms(), kind: Some(kind) });
        self.conv = Some(conversation_from_log(&mut self.spec, &events));
        Ok(())
    }

    /// Rewinds the conversation to before event `to_seq`, restoring files
    /// to the checkpoint taken before it if asked to.
    async fn rewind(&mut self, command_id: String, to_seq: u64, restore_files: bool) -> anyhow::Result<()> {
        let sid = self.writer.session_id.clone();
        let events = self.journal.events_after(&sid, 0, u32::MAX).await?;
        if self.runs_claude_code() {
            self.reset_claude(&events, &command_id, to_seq, restore_files).await?;
        }
        let mut restore_error = String::new();
        let mut restored = false;
        if events.iter().any(|e| e.seq >= to_seq && matches!(e.kind, Some(session_event::Kind::PlaceChanged(_)))) {
            restore_error = "The session changed place after that message; files stayed where they are".into();
        } else if restore_files {
            // Check at the shared restore boundary, not only during Claude
            // Code reset: an ancestor on carbon6's loop can also have a
            // descendant CLI writing this tree. Raw history includes retained
            // and previously withdrawn children. Conversation-only Rewind
            // keeps its existing runtime-specific admission behavior.
            self.manager.rewind_children_stopped(&events).await?;
            let effective = c6_agent::effective_events(&events);
            let commit = effective
                .iter()
                .filter(|e| e.seq < to_seq)
                .rev()
                .take_while(|e| !matches!(e.kind, Some(session_event::Kind::PlaceChanged(_))))
                .find_map(|e| match &e.kind {
                    Some(session_event::Kind::Checkpoint(c)) if !c.commit.is_empty() => Some(c.commit.clone()),
                    _ => None,
                });
            match commit {
                None => restore_error = "no checkpoint was taken before that message".into(),
                Some(c) => match self.checkpoints.restore(&c).await {
                    Ok(()) => restored = true,
                    Err(e) => restore_error = format!("{e:#}"),
                },
            }
        }
        self.writer.append(session_event::Kind::Rewound(Rewound { command_id, to_seq, restored_files: restored, restore_error })).await?;
        info!(session = %sid, to_seq, restored, "rewound");
        let events = self.journal.events_after(&sid, 0, u32::MAX).await?;
        self.withdrawn = withdrawn_sources(&events);
        if let Some((r, _)) = self.journal.claude_rewind(&sid).await? {
            self.withdrawn.extend(r.withdrawn);
        }
        let withdrawn = &self.withdrawn;
        self.deferred.retain(|(i, _)| !withdrawn_input(withdrawn, i));
        self.inbox.lock().unwrap().queue.retain(|(i, _)| !withdrawn_input(withdrawn, i));
        self.conv = Some(conversation_from_log(&mut self.spec, &events));
        Ok(())
    }

    fn withdrawn_input(&self, input: &PendingInput) -> bool {
        withdrawn_input(&self.withdrawn, input)
    }

    /// Keep the actor and inbox alive. Only a possibly destructive failure
    /// blocks model input; its durable failure requires an explicit retry.
    async fn rewind_failed(&self, command_id: &str, error: &anyhow::Error, retries: &[String]) -> anyhow::Result<bool> {
        let reset = self.journal.claude_rewind(&self.writer.session_id).await?;
        let untouched = !matches!(&reset, Some((r, 0)) if !r.untouched);
        let message = if untouched {
            format!("Rewind was not applied; the existing conversation is unchanged and queued messages will continue: {error:#}")
        } else {
            format!(
                "Rewind could not be confirmed after reset may have started; this session is blocked and queued messages will NOT be sent. Explicitly retry Rewind with the same target and file-restore choice after resolving this: {error:#}"
            )
        };
        let mut next = self.writer.next_seq.lock().await;
        self.link.flush_ephemeral();
        let event = SessionEvent {
            seq: *next,
            ts_ms: c6_core::now_ms(),
            kind: Some(session_event::Kind::Error(ErrorEvent { message: message.clone() })),
        };
        let (blocked, rejected) = self
            .journal
            .fail_claude_rewind_owned(&self.writer.session_id, command_id, &event, retries, self.writer.ownership_epoch)
            .await?;
        *next += 1;
        self.writer.logged.store(*next, std::sync::atomic::Ordering::SeqCst);
        for id in rejected {
            self.link.reject_owned(&self.writer.session_id, &id, &message, self.writer.ownership_epoch);
        }
        Ok(blocked)
    }

    async fn reset_claude(&mut self, events: &[SessionEvent], command_id: &str, to_seq: u64, restore_files: bool) -> anyhow::Result<()> {
        let _build_use = self.journal.build_folder_lifecycle.clone().read_owned().await;
        use crate::claude_code::{Launch, Observer, Process, Reset};
        let sid = &self.writer.session_id;
        let previous = self.journal.claude_rewind(sid).await?;
        let mut reset = match previous {
            Some((r, 0)) => r,
            previous => Reset {
                command_id: command_id.into(),
                to_seq,
                restore_files,
                session_id: uuid::Uuid::now_v7().to_string(),
                fresh: true,
                withdrawn: self.withdrawn.iter().cloned().collect(),
                untouched: true,
                previous: previous.map(Box::new),
                failure: String::new(),
                retries: Vec::new(),
            },
        };
        // Intent is durable before touching the old process. A crash or a
        // failed initialization cannot let the next actor resume old history.
        self.journal.prepare_claude_rewind_owned(sid, &reset, self.writer.ownership_epoch).await?;
        self.manager.rewind_preflight(sid, events, to_seq, restore_files).await?;
        // This write is the irreversible boundary, including on crash. Never
        // clear a legacy intent (whose phase is unknown) as if untouched.
        reset.untouched = false;
        reset.previous = None;
        reset.failure.clear();
        self.journal.prepare_claude_rewind_owned(sid, &reset, self.writer.ownership_epoch).await?;
        self.session_tools.background.cancel_shell_contexts();
        let effective = c6_agent::effective_events(events);
        let known_kept: HashSet<_> = effective
            .iter()
            .filter(|e| e.seq < to_seq)
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::BackgroundTask(t)) if t.state == "started" => Some(t.task_id.as_str()),
                Some(session_event::Kind::SubagentSpawned(s)) => Some(s.child_session_id.as_str()),
                _ => None,
            })
            .collect();
        // Pending intent atomically closes shell admission for this tree.
        // Reservations precede process creation, independent of task sinks.
        // Capture source IDs before any fallible cleanup and retain them in
        // ownership rows even after absence is confirmed or this actor exits.
        reset
            .withdrawn
            .extend(self.journal.shell_writer_sources(sid).await?.into_iter().filter(|source| !known_kept.contains(source.as_str())));
        reset.withdrawn.sort();
        reset.withdrawn.dedup();
        self.journal.prepare_claude_rewind_owned(sid, &reset, self.writer.ownership_epoch).await?;
        if let Some(mut p) = self.claude.take() {
            // InputDelivered means written to the CLI, not a confirmed User
            // receipt. A stopped turn can leave these owned by Process rather
            // than Inbox. Preserve them before consuming/dropping that process,
            // even when cleanup fails. A destructive failure keeps the inbox
            // blocked but cancellable; a successful reset sends them afterward.
            self.inbox.lock().unwrap().requeue(p.take_unsent());
            p.stop_for_rewind().await?;
        }
        // A keeper may have outlived the actor/worker, including a cancelled
        // stop. It is cleanup-owned here, never attached as the active branch.
        crate::claude_code::stop_held_for_rewind(&self.manager.claude_dir().join("held"), sid, &self.journal).await?;
        // A prior actor/worker or failed initialization may have lost its
        // leader while descendants survived. Missing Process is not proof.
        crate::claude_code::confirm_stopped(&self.journal, sid, std::time::Duration::from_secs(3)).await?;
        self.journal.confirm_shell_writers_stopped(sid).await?;
        // Registry insertion precedes asynchronous ActorMsg::Task logging.
        // Snapshot after stopping the old process so queued starts/ends and
        // stop-generated Lost events cannot evade source tracking.
        for task in self.session_tools.background.list() {
            if task.kind == c6_tools::TaskKind::ClaudeCode || !known_kept.contains(task.id.as_str()) {
                reset.withdrawn.push(task.id);
            }
        }
        reset.withdrawn.extend(
            events
                .iter()
                .filter(|e| e.seq >= to_seq)
                .filter_map(|e| match &e.kind {
                    Some(session_event::Kind::SubagentSpawned(s)) => Some(s.child_session_id.clone()),
                    Some(session_event::Kind::BackgroundTask(t)) if t.state == "started" => Some(t.task_id.clone()),
                    Some(session_event::Kind::NoticeQueued(n)) => Some(n.command_id.clone()),
                    _ => None,
                })
                .filter(|source| !source.is_empty()),
        );
        reset.withdrawn.sort();
        reset.withdrawn.dedup();
        self.journal.prepare_claude_rewind_owned(sid, &reset, self.writer.ownership_epoch).await?;
        // carbon6 tasks may be remote, where cancellation cannot be confirmed
        // while disconnected. Do not pretend their work stopped. Native CLI
        // tasks and mirrored subagents have already stopped with the process.
        // Check again after stop for races with work admitted before intent.
        self.manager.rewind_preflight(sid, events, to_seq, restore_files).await?;
        self.manager.rewind_children_quiescent(&rewind_affected_events(events, to_seq, restore_files), true).await?;
        let registry = self.session_tools.background.list();
        let mut durable_tasks = HashMap::new();
        for e in if restore_files { events } else { &effective } {
            if let Some(session_event::Kind::BackgroundTask(t)) = &e.kind {
                durable_tasks.insert(&t.task_id, t);
            }
        }
        for task in durable_tasks.values().filter(|t| t.state == "started" && t.kind != "claude_code") {
            if registry.iter().any(|t| t.id == task.task_id && !t.running) {
                continue;
            }
            if task.kind == "agent"
                && events.iter().any(|e| {
                    matches!(&e.kind,
                Some(session_event::Kind::SubagentSpawned(s)) if s.background_task_id == task.task_id)
                })
            {
                continue;
            }
            // Recovery normally handles orphan shells through ActorMsg::Lost,
            // but a pending reset runs before that mailbox. Stop/check here so
            // file restore cannot race a recovered writer. Unknown identity is
            // a refusal, never permission to kill another process or continue.
            stop_orphan(task.pgid, &task.description);
            anyhow::ensure!(
                task.pgid > 0 && nix::sys::signal::killpg(nix::unistd::Pid::from_raw(task.pgid), None) == Err(nix::errno::Errno::ESRCH),
                "Background task {} has not confirmed that it stopped; stop it before retrying Rewind",
                task.task_id
            );
        }
        let effective = c6_agent::effective_events(events);
        let kept: Vec<_> = effective.iter().filter(|e| e.seq < to_seq).collect();
        let mut cutoff = None;
        if let Some(e) = kept.iter().rev().find(|e| matches!(e.kind, Some(session_event::Kind::TurnEnded(_))))
            && let Some(session_event::Kind::TurnEnded(t)) = &e.kind
            && t.reason == TurnEndReason::Completed as i32
            && !kept.iter().any(|later| {
                later.seq > e.seq
                    && matches!(
                        later.kind,
                        Some(
                            session_event::Kind::User(_)
                                | session_event::Kind::Assistant(_)
                                | session_event::Kind::ToolResult(_)
                                | session_event::Kind::Compaction(_)
                        )
                    )
            })
            && let Some(boundary) = self.journal.claude_turn(sid, e.seq).await?
            && !boundary.cutoff_uuid.is_empty()
            && uuid::Uuid::parse_str(&boundary.cutoff_uuid).is_ok()
            && uuid::Uuid::parse_str(&boundary.session_id).is_ok()
        {
            cutoff = Some(boundary);
        }
        let bin = crate::claude_code::find_claude().ok_or_else(|| anyhow::anyhow!("Claude Code is not installed on this worker"))?;
        let model = self.spec.model.as_ref().cloned().unwrap_or_default();
        let tools = self.claude_code_subagent_tools();
        let core = claude_code_core(&self.spec);
        let names = tools
            .iter()
            .map(|t| {
                let n = t.spec().name;
                if core.iter().any(|c| c6_tools::tool_allowed(std::slice::from_ref(c), &n)) { format!("{n}*") } else { n }
            })
            .collect();
        let launch = Launch {
            conversation: reset.session_id.clone(),
            effort: (!model.effort.is_empty()).then_some(model.effort),
            settings: Some(claude_code_settings(&self.spec, self.hooks.as_ref().map(|h| h.claude_settings()).as_deref())),
            system_prompt: (!tools.is_empty()).then(|| self.claude_code_system_prompt(&tools)),
            tools: names,
            worker: c6_core::build().into(),
            compile_cache_env_fingerprint: crate::claude_code::compile_cache_env_fingerprint(&self.session_tools.command_defaults()),
            native: claude_code_native(&self.spec),
            extra_args: Vec::new(),
        };
        let bridge = if tools.is_empty() { None } else { self.manager.bridge().await };
        let register = || {
            bridge.as_ref().map(|b| {
                b.register(crate::mcp_bridge::Entry {
                    session_id: sid.clone(),
                    session: self.session_tools.clone(),
                    tools: tools.clone(),
                    cancel: Mutex::default(),
                    journal: self.journal.clone(),
                    matched: Mutex::default(),
                    policies: self.policies.clone(),
                    instructions: CLAUDE_CODE_INTRO.into(),
                    core: core.clone(),
                })
            })
        };
        let observer = Arc::new(Observer {
            tools: self.session_tools.clone(),
            journal: self.journal.clone(),
            link: self.link.clone(),
            session_id: sid.clone(),
            ownership_epoch: self.writer.ownership_epoch,
            spec: self.spec.clone(),
            started: self.started.clone(),
        });
        let mut attempts = Vec::new();
        if let Some(cutoff) = cutoff {
            let mut fork = launch.clone();
            fork.conversation = cutoff.session_id;
            // The cutoff is inclusive. Choose a fresh fork ID, then adopt the
            // session_id from stream-json on every subsequent turn.
            fork.extra_args.extend([
                "--resume-session-at".into(),
                cutoff.cutoff_uuid,
                "--fork-session".into(),
                "--session-id".into(),
                reset.session_id.clone(),
            ]);
            attempts.push((fork, true));
        }
        attempts.push((launch, false));
        let mut last_error = String::new();
        for (mut launch, resume) in attempts {
            reset.fresh = !resume;
            if !resume {
                reset.session_id = uuid::Uuid::now_v7().to_string();
                launch.conversation = reset.session_id.clone();
            }
            self.journal.prepare_claude_rewind_owned(sid, &reset, self.writer.ownership_epoch).await?;
            // Until ready confirms the actual fork ID this runtime must not
            // survive a worker restart or be ordinarily adopted. Startup
            // rejects an interrupted destructive attempt; the next quiet
            // turn promotes a confirmed branch to held transport.
            let mut p = match Process::spawn(
                &bin,
                launch,
                resume,
                &self.session_tools.cwd(),
                &model.model,
                self.claude_stdin.clone(),
                register(),
                Some(observer.clone()),
            ) {
                Ok(p) => p,
                Err(e) => {
                    last_error = e.to_string();
                    continue;
                }
            };
            p.record_in(&self.manager.claude_dir()).await?;
            match p.ready(&reset.session_id).await {
                Ok(confirmed_id) => {
                    reset.session_id = confirmed_id;
                    self.journal.prepare_claude_rewind_owned(sid, &reset, self.writer.ownership_epoch).await?;
                    self.claude = Some(p);
                    return Ok(());
                }
                Err(e) => {
                    last_error = format!("{e:#}");
                    p.stop_for_rewind().await?;
                    // Do not let fresh fallback conceal a failed fork's
                    // surviving descendants, including across a restart.
                    crate::claude_code::confirm_stopped(&self.journal, sid, std::time::Duration::from_secs(3)).await?;
                }
            }
        }
        anyhow::bail!("Claude Code could not initialize the kept conversation: {last_error}")
    }

    /// Runs the session on `model` from the next turn on. Earlier reasoning
    /// is dropped from the history: it is bound to the model that wrote it.
    /// A switch between Claude Code and carbon6's own loop hands the
    /// session over instead (`hand_off`).
    async fn switch_model(&mut self, model: ModelSpec) -> anyhow::Result<()> {
        let into_claude_code = model.connection_kind == "claude_code";
        if into_claude_code != self.runs_claude_code() {
            return self.hand_off(model).await;
        }
        let current = self.spec.model.clone().unwrap_or_default();
        if (&current.connection_id, &current.model, &current.effort) == (&model.connection_id, &model.model, &model.effort) {
            return Ok(()); // e.g. redelivered after the switch was recorded
        }
        let context_window = self.link.context_window(&self.on_behalf_of, &model).await;
        self.writer
            .append(session_event::Kind::ModelSwitched(ModelSwitched { model: Some(model.clone()), context_window, handoff: None }))
            .await?;
        info!(session = %self.writer.session_id, model = %model.model, "model switched");
        // An effort change keeps the model, and with it the reasoning.
        let same_model = (&current.connection_id, &current.model) == (&model.connection_id, &model.model);
        self.spec.model = Some(model);
        self.writer.set_keep_warm_capture(&self.spec);
        if !same_model && let Some(conv) = &mut self.conv {
            conv.history = c6_agent::portable_messages(std::mem::take(&mut conv.history));
        }
        Ok(())
    }

    /// After a restart: the Claude Code process the previous worker held for
    /// this session, taken up where it was (docs/restarts.md §3). What it
    /// writes from here on comes in as it would have (a turn it carries on
    /// shows as one it started by itself).
    async fn take_up_held(&mut self) -> anyhow::Result<()> {
        let _build_use = self.journal.build_folder_lifecycle.clone().read_owned().await;
        if !self.runs_claude_code() || self.claude.is_some() {
            return Ok(());
        }
        let held = self.manager.claude_dir().join("held");
        let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        let conversation = journal_claude_conversation(&self.journal, &self.writer.session_id, &self.spec, &events).await?.id;
        let Some((record, token)) = crate::claude_code::Process::held_record(&held, &conversation)? else {
            self.manager.stop_unmapped_claude(&self.writer.session_id, &events).await?;
            crate::claude_code::confirm_stopped(&self.journal, &self.writer.session_id, std::time::Duration::ZERO).await?;
            return Ok(());
        };
        let bridge_tools = self.claude_code_subagent_tools();
        let registration = match token {
            Some(token) if !bridge_tools.is_empty() => self.manager.bridge().await.map(|b| {
                b.register_as(
                    token,
                    crate::mcp_bridge::Entry {
                        session_id: self.writer.session_id.clone(),
                        session: self.session_tools.clone(),
                        tools: bridge_tools,
                        cancel: Mutex::default(),
                        journal: self.journal.clone(),
                        matched: Mutex::default(),
                        policies: self.policies.clone(),
                        instructions: CLAUDE_CODE_INTRO.into(),
                        core: claude_code_core(&self.spec),
                    },
                )
            }),
            _ => None,
        };
        let observer = Arc::new(crate::claude_code::Observer {
            tools: self.session_tools.clone(),
            journal: self.journal.clone(),
            link: self.link.clone(),
            session_id: self.writer.session_id.clone(),
            ownership_epoch: self.writer.ownership_epoch,
            spec: self.spec.clone(),
            started: self.started.clone(),
        });
        let model = self.spec.model.as_ref().map(|m| m.model.clone()).unwrap_or_default();
        self.claude = crate::claude_code::Process::attach(
            &held,
            &conversation,
            &self.session_tools.cwd(),
            &model,
            self.claude_stdin.clone(),
            registration,
            Some(observer),
        )
        .await;
        anyhow::ensure!(
            self.claude.is_some() || !record.join("meta.json").try_exists()?,
            "Cannot confirm ownership of the held Claude Code runtime"
        );
        Ok(())
    }

    fn runs_claude_code(&self) -> bool {
        self.started.tool_profile == crate::claude_code::PROFILE
    }

    /// Hands the session over between Claude Code and carbon6's own loop,
    /// at a turn boundary: the destination's tool profile, tools and system
    /// prompt are built afresh and recorded with the switch (so a reload
    /// runs on it), and the conversation so far reaches it as a transcript
    /// (`conversation_from_log`, `claude_conversation`), never as native
    /// state. Claude Code runs only top-level sessions on the worker itself.
    async fn hand_off(&mut self, model: ModelSpec) -> anyhow::Result<()> {
        let _build_use = self.journal.build_folder_lifecycle.clone().read_owned().await;
        let sid = self.writer.session_id.clone();
        // Leaving Claude Code: its process goes, with its background tasks.
        if let Some(mut p) = self.claude.take() {
            self.inbox.lock().unwrap().requeue(p.take_unsent());
            p.stop().await;
        }
        let into_claude_code = model.connection_kind == "claude_code";
        if into_claude_code {
            let refusal = if self.spec.delegation.is_some() {
                Some("subagents run in carbon6's own loop")
            } else if !self.started.ssh_target.is_empty() {
                Some("Claude Code runs on the worker itself, not over SSH")
            } else {
                None
            };
            if let Some(why) = refusal {
                warn!(session = %sid, "switch to Claude Code refused: {why}");
                let current = self.spec.model.as_ref().map(|m| m.model.clone()).unwrap_or_default();
                let message = format!("This session cannot switch to Claude Code ({why}); it stays on {current}.");
                self.writer.append(session_event::Kind::Error(ErrorEvent { message })).await?;
                return Ok(());
            }
        }
        let (handoff, tools, setup_errors) = if into_claude_code {
            // A session imported from Claude Code that carbon6 never ran
            // goes on in Claude Code's own conversation (docs/imports.md).
            let events = self.journal.events_after(&sid, 0, u32::MAX).await?;
            let native = continues_claude_import(&self.started, &events);
            let handoff = c6_proto::Handoff {
                tool_profile: crate::claude_code::PROFILE.to_owned(),
                tools: Vec::new(),
                system_prompt: crate::claude_code::SYSTEM_PROMPT.to_owned(),
                // Else a conversation of its own: Claude Code has none of
                // what happened here, and may have an older one under the
                // session's id.
                claude_session_id: if native { sid.clone() } else { uuid::Uuid::now_v7().to_string() },
                native,
            };
            (handoff, Vec::new(), Vec::new())
        } else {
            // As a new session on that model would have it (Claude Code
            // sessions always work in a local folder).
            let mut spec = self.spec.clone();
            spec.model = Some(model.clone());
            let cwd = PathBuf::from(&self.started.cwd);
            let labels = session_labels(&spec.labels, &self.started.repo, &self.started.cwd, &self.started.home, &model.model, "");
            let date = time::OffsetDateTime::now_utc().date().to_string();
            let base = build_system_prompt(&spec.base_prompt, &cwd, &date);
            let built = own_runtime(&self.link, &self.on_behalf_of, &spec, &cwd, &labels, base, "resume").await;
            let handoff = c6_proto::Handoff {
                tool_profile: built.profile.as_str().to_owned(),
                tools: visible_tool_names(&spec, &built.tools),
                system_prompt: built.prompt,
                claude_session_id: String::new(),
                native: false,
            };
            (handoff, built.tools, built.errors)
        };
        let context_window = self.link.context_window(&self.on_behalf_of, &model).await;
        self.writer
            .append(session_event::Kind::ModelSwitched(ModelSwitched {
                model: Some(model.clone()),
                context_window,
                handoff: Some(handoff.clone()),
            }))
            .await?;
        info!(session = %sid, model = %model.model, runtime = %handoff.tool_profile, "handed over");
        self.spec.model = Some(model);
        self.writer.set_keep_warm_capture(&self.spec);
        apply_handoff(&mut self.started, &handoff);
        self.tools = tools;
        self.claude_code.store(into_claude_code, std::sync::atomic::Ordering::SeqCst);
        // Subagents keep their parent's hooks; a top-level session's are
        // carbon6's to run only in its own loop.
        if self.spec.delegation.is_none() {
            self.session_tools.set_hooks(if into_claude_code { None } else { own_loop_hooks(&self.policies, &self.hooks) });
        }
        // The switch is committed. Install its runtime before another write
        // can fail and make reload see a new model with the old tools/hooks.
        for message in setup_errors {
            self.writer.append(session_event::Kind::Error(ErrorEvent { message })).await?;
        }
        // The conversation from here: the transcript, then what follows.
        let events = self.journal.events_after(&sid, 0, u32::MAX).await?;
        self.conv = Some(conversation_from_log(&mut self.spec, &events));
        Ok(())
    }

    /// The tools a Claude Code session gets over MCP: carbon6's `Task` and
    /// `SendMessage`, which can run in the background (their notices go to
    /// the session's process, docs/background.md §4), `TaskOutput` and
    /// `TaskStop` for those (none of these without a connection or template
    /// to run on), carbon6's documents (`doc`), and the session-message
    /// tools its tool list allows.
    fn claude_code_subagent_tools(&self) -> Vec<Arc<dyn Tool>> {
        let model = self.spec.model.as_ref().map(|m| m.model.as_str()).unwrap_or_default();
        let labels =
            session_labels(&self.spec.labels, &self.started.repo, &self.started.cwd, &self.started.home, model, &self.started.ssh_target);
        // The ones chosen for Claude Code sessions (Settings → Agents; by
        // default the subagent tools, documents and session messages), of
        // those this session has. A routing rule's tool list narrows them,
        // except the subagent tools and documents, which it never did.
        let choice = claude_code_choice(&self.spec);
        let chosen = |name: &str| choice.as_ref().is_none_or(|c| is_chosen(c, name));
        // A subagent's named agent narrows all of them.
        let agent = crate::subagent::agent_tools(&self.spec).unwrap_or_default();
        let chosen = |name: &str| chosen(name) && c6_tools::tool_allowed(&agent, name);
        let narrowed = |name: &str| matches!(name, "doc") || c6_tools::tool_allowed(&self.spec.tools, name);
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            // carbon6's documents, as mcp__carbon6__doc (docs/docs.md §9).
            Arc::new(c6_tools::records::Doc::new(false)),
        ];
        tools.extend(claude_code_extras(&self.spec, &labels, c6_tools::browser::find_chrome()));
        tools.retain(|t| {
            let name = t.spec().name;
            chosen(&name) && narrowed(&name)
        });
        let depth = crate::subagent::depth_of(&self.spec);
        let connections = task_connections(&self.spec);
        let templates = task_templates(&self.spec);
        if (connections.is_empty() && templates.is_empty()) || !crate::subagent::may_delegate(&self.spec, depth) {
            return tools;
        }
        let agents = session_agents(&self.spec, &labels);
        let mut all: Vec<Arc<dyn Tool>> = vec![
            Arc::new(
                c6_tools::Task::with_agents(true, agents)
                    .with_group_guidance(spec_prompt(&self.spec, "session.subagent_group", ""))
                    .with_connections(connections)
                    .with_templates(&templates_intro(&self.spec), templates)
                    .stopped_with("mcp__carbon6__TaskStop"),
            ),
            Arc::new(c6_tools::SendMessage::claude()),
            Arc::new(c6_tools::BashOutput::carbon6()),
            Arc::new(c6_tools::KillShell::carbon6()),
        ];
        all.retain(|t| chosen(&t.spec().name));
        all.extend(tools);
        all
    }

    /// What a Claude Code session is told about carbon6's tools it gets,
    /// added to its system prompt: the `session.claude_code` prompt, then
    /// carbon6's own guidance for those that need it (memory, task cards,
    /// the memex), which carbon6's system prompt carries in its own loop.
    fn claude_code_system_prompt(&self, tools: &[Arc<dyn Tool>]) -> String {
        let model = self.spec.model.as_ref().map(|m| m.model.as_str()).unwrap_or_default();
        let labels =
            session_labels(&self.spec.labels, &self.started.repo, &self.started.cwd, &self.started.home, model, &self.started.ssh_target);
        claude_code_system_prompt(&self.spec, &labels, tools)
    }

    async fn interrupt_queued_launch(&mut self) -> anyhow::Result<()> {
        // An explicit local delegation may already have a waiter even
        // though its admission expired before dispatch. Settle it without
        // dropping or publishing a head command it never had.
        {
            let mut inbox = self.inbox.lock().unwrap();
            let mut waiting = std::mem::take(&mut self.waiting);
            for (id, done) in waiting.drain(..) {
                if matches!(inbox.remove(&id), QueueOp::Cancelled) {
                    let _ = done.send(Outcome { reason: TurnEndReason::Interrupted, report: String::new(), error: String::new() });
                } else {
                    self.waiting.push((id, done));
                }
            }
        }
        // Head-created inputs have UUID command IDs. Leave internal inputs
        // (e.g. a background report) in the inbox for the next turn.
        let ids: Vec<_> = self
            .inbox
            .lock()
            .unwrap()
            .queue
            .iter()
            .filter(|(input, _)| uuid::Uuid::parse_str(&input.command_id).is_ok())
            .map(|(input, _)| input.command_id.clone())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let reason = "subagent was stopped while queued for worker headroom";
        let session = &self.writer.session_id;
        let epoch = self.writer.ownership_epoch;
        self.journal.reject_queued_launches(session, epoch, &ids, reason).await?;
        let mut inbox = self.inbox.lock().unwrap();
        for id in &ids {
            inbox.remove(id);
            self.link.reject_owned(session, id, reason, epoch);
        }
        Ok(())
    }

    /// Quiet maintenance owns the same supervised slot as a turn, so cleanup
    /// can wait without blocking the mailbox or allowing adoption/rewind over
    /// its process group. It never takes input or changes the turn counter.
    fn start_claude_maintenance(&mut self) -> (TurnTask, CancellationToken) {
        let conv = self.conv.take().expect("no turn is running");
        let process = self.claude.take().expect("an idle Claude Code process");
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let writer = self.writer.clone();
        let manager = self.manager.clone();
        let session_tools = self.session_tools.clone();
        let bridge_tools = self.claude_code_subagent_tools();
        let bridge_core = claude_code_core(&self.spec);
        let policies = self.policies.clone();
        let observer = Arc::new(crate::claude_code::Observer {
            tools: self.session_tools.clone(),
            journal: self.journal.clone(),
            link: self.link.clone(),
            session_id: writer.session_id.clone(),
            ownership_epoch: writer.ownership_epoch,
            spec: self.spec.clone(),
            started: self.started.clone(),
        });
        let stdin = self.claude_stdin.clone();
        let model = self.spec.model.clone().unwrap_or_default().model;
        let task = tokio::spawn(async move {
            let bridge = if bridge_tools.is_empty() { None } else { manager.bridge().await };
            let register = || {
                bridge.as_ref().map(|b| {
                    b.register(crate::mcp_bridge::Entry {
                        session_id: writer.session_id.clone(),
                        session: session_tools.clone(),
                        tools: bridge_tools.clone(),
                        cancel: Mutex::default(),
                        journal: observer.journal.clone(),
                        matched: Mutex::default(),
                        policies: policies.clone(),
                        instructions: CLAUDE_CODE_INTRO.into(),
                        core: bridge_core.clone(),
                    })
                })
            };
            let sink: Arc<dyn EventSink> = writer.clone();
            match maintain_claude_context(
                &writer.session_id,
                process,
                &session_tools.cwd(),
                &model,
                &stdin,
                &register,
                observer.clone(),
                &manager.claude_dir(),
                &sink,
                &c,
            )
            .await
            {
                Ok(process) => Some((conv, process)),
                Err(e) => {
                    error!(session = %writer.session_id, "Claude Code maintenance failed: {e:#}");
                    None
                }
            }
        });
        (task, cancel)
    }

    fn start_turn(&mut self) -> (TurnTask, CancellationToken) {
        self.disk_admitted = true;
        self.writer.set_activity(c6_proto::SessionActivityKind::Unspecified);
        self.turns += 1;
        let turn = self.turns;
        let cancel = CancellationToken::new();
        self.session_tools.background.shell_context(&cancel);
        let model = self.spec.model.clone().unwrap_or_default();
        let provider = Provider::from_kind(&model.connection_kind).unwrap_or(Provider::OpenAiChat);
        let info = model_info(provider, &model.model);
        let mut cfg = TurnConfig {
            model: model.model.clone(),
            system_prompt: self.started.system_prompt.clone(),
            max_output_tokens: (model.max_output_tokens > 0).then_some(model.max_output_tokens),
            cache_key: self.writer.session_id.clone(),
            effort: Some(model.effort.clone())
                .filter(|e| !e.is_empty())
                .or_else(|| (model.connection_kind != "claude_code").then_some(info.default_effort).flatten().map(str::to_owned)),
            context_window: info.context_window,
            web_search: hosted_web_search(provider, &self.started),
            compact: compact_prompts(&self.spec),
            compact_at: model.compact_at_percent,
            trim_at: trim_at(&self.spec, info.context_window),
        };
        let mut conv = self.conv.take().expect("no turn is running");
        let sub = crate::subagent::Parent {
            session_id: self.writer.session_id.clone(),
            spec: self.spec.clone(),
            started: self.started.clone(),
            log: self.writer.clone(),
            manager: self.manager.clone(),
            default: self.subagent_default.clone(),
            limits: self.launch_limits.clone(),
        };
        let (writer, link, tools, session_tools, inbox, c) =
            (self.writer.clone(), self.link.clone(), self.tools.clone(), self.session_tools.clone(), self.inbox.clone(), cancel.clone());
        writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true, ..Default::default() }));

        let claude_code = self.runs_claude_code();
        let spec_for_claude = claude_code.then(|| self.spec.clone());
        let log = self.journal.clone();
        let checkpoints = self.checkpoints.clone();
        let remote_session = !self.started.ssh_target.is_empty();
        // Subagents cannot be rewound: their parent's checkpoints cover the
        // same folder.
        let subagent = self.spec.delegation.is_some();
        let on_behalf_of = self.on_behalf_of.clone();
        let secret_names: Vec<String> = {
            let labels = session_labels(
                &self.spec.labels,
                &self.started.repo,
                &self.started.cwd,
                &self.started.home,
                &model.model,
                &self.started.ssh_target,
            );
            applicable_secrets(&self.spec, &labels).into_iter().map(|(n, _)| n).collect()
        };
        let claude_settings =
            claude_code.then(|| claude_code_settings(&self.spec, self.hooks.as_ref().map(|h| h.claude_settings()).as_deref()));
        // Claude Code's own tools it keeps (none of its others).
        let claude_native = claude_code_native(&self.spec);
        // carbon6's subagents, offered to Claude Code over MCP.
        let bridge_tools = if claude_code { self.claude_code_subagent_tools() } else { Vec::new() };
        let bridge_core = claude_code_core(&self.spec);
        let bridge_prompt = (claude_code && !bridge_tools.is_empty()).then(|| self.claude_code_system_prompt(&bridge_tools));
        // What the process was started with: other tools, or others up
        // front, need a new one.
        let bridge_names: Vec<String> = bridge_tools
            .iter()
            .map(|t| {
                let n = t.spec().name;
                if bridge_core.iter().any(|c| c6_tools::tool_allowed(std::slice::from_ref(c), &n)) { format!("{n}*") } else { n }
            })
            .collect();
        let (manager, journal) = (self.manager.clone(), self.journal.clone());
        // Claude Code's own subagents are mirrored into sessions of their own.
        let observer = Arc::new(crate::claude_code::Observer {
            tools: self.session_tools.clone(),
            journal: self.journal.clone(),
            link: self.link.clone(),
            session_id: self.writer.session_id.clone(),
            ownership_epoch: self.writer.ownership_epoch,
            spec: self.spec.clone(),
            started: self.started.clone(),
        });
        let (mut claude, claude_stdin, claude_dir) = (self.claude.take(), self.claude_stdin.clone(), self.manager.claude_dir());
        // Claude Code's calls of carbon6's tools are held to the session's policies.
        let policies = self.policies.clone();
        let trim_spec = self.spec.clone();
        let task_fence = (subagent || self.spec.depth > 0).then(|| self.task_fence.clone());
        let task = tokio::spawn(async move {
            let _build_use = journal.build_folder_lifecycle.clone().read_owned().await;
            let sink: Arc<dyn EventSink> = Arc::new(TurnSink { writer: writer.clone(), inbox: inbox.clone(), task_fence });
            if !claude_code {
                cfg.context_window = link.context_window(&on_behalf_of, &model).await;
                // As the window the connection says, not the model's default.
                cfg.trim_at = trim_at(&trim_spec, cfg.context_window);
            }
            // Secrets for this turn's commands (values only in memory).
            if !remote_session {
                session_tools.set_command_env(crate::compile_cache::session_environment(manager.state_dir.clone()).await);
            }
            if !secret_names.is_empty() {
                let (values, errors) = fetch_secrets(&link, &on_behalf_of, &secret_names).await;
                for e in errors {
                    warn!(session = %writer.session_id, "secret unavailable: {e}");
                }
                session_tools.set_secrets(values);
            }
            // The files as they are before this turn's messages, for
            // rewinding to before them. A failed snapshot does not stop the
            // turn; the rewind then cannot restore files.
            if !subagent {
                let checkpoint = match if remote_session {
                    Err(anyhow::anyhow!("files on an SSH target are not checkpointed"))
                } else {
                    checkpoints.snapshot_with_tree().await
                } {
                    Ok((commit, tree_id)) => Checkpoint { commit, tree_id, ..Default::default() },
                    Err(e) => {
                        warn!(session = %writer.session_id, "no checkpoint: {e:#}");
                        Checkpoint { error: format!("{e:#}"), ..Default::default() }
                    }
                };
                if let Err(e) = sink.append(session_event::Kind::Checkpoint(checkpoint)).await {
                    error!(session = %writer.session_id, "turn failed: {e:#}");
                    return None;
                }
            }
            let result = if claude_code {
                let bridge = if bridge_tools.is_empty() { None } else { manager.bridge().await };
                if bridge.is_some() {
                    session_tools.set_subagents(Some(Arc::new(crate::subagent::Runner::new(sub, &[]))));
                }
                // A new process gets its own registration, for as long as it runs.
                let register = || {
                    bridge.as_ref().map(|b| {
                        b.register(crate::mcp_bridge::Entry {
                            session_id: writer.session_id.clone(),
                            session: session_tools.clone(),
                            tools: bridge_tools.clone(),
                            cancel: Mutex::default(),
                            journal: journal.clone(),
                            matched: Mutex::default(),
                            policies: policies.clone(),
                            instructions: CLAUDE_CODE_INTRO.into(),
                            core: bridge_core.clone(),
                        })
                    })
                };
                // Which of Claude Code's conversations this turn continues
                // (and what a new one is told first), as the log has it.
                let (conversation, trim) = match async {
                    let events = log.events_after(&writer.session_id, 0, u32::MAX).await?;
                    let spec = spec_for_claude.as_ref().expect("a Claude Code turn");
                    let conversation = journal_claude_conversation(&log, &writer.session_id, spec, &events).await?;
                    Ok::<_, anyhow::Error>((conversation, ClaudeTrim::from_events(spec, &events)))
                }
                .await
                {
                    Ok(conversation) => conversation,
                    Err(e) => {
                        error!(session = %writer.session_id, "turn failed: {e:#}");
                        return None;
                    }
                };
                let launch = crate::claude_code::Launch {
                    conversation: conversation.id.clone(),
                    effort: cfg.effort.clone(),
                    settings: claude_settings,
                    system_prompt: bridge_prompt,
                    tools: bridge_names,
                    worker: c6_core::build().to_owned(),
                    compile_cache_env_fingerprint: crate::claude_code::compile_cache_env_fingerprint(&session_tools.command_defaults()),
                    extra_args: Vec::new(),
                    native: claude_native,
                };
                let process = claude.take();
                let result = claude_turn(
                    &writer.session_id,
                    turn,
                    process,
                    &conversation,
                    trim,
                    launch,
                    &session_tools.cwd(),
                    &cfg.model,
                    &claude_stdin,
                    &register,
                    observer,
                    &claude_dir,
                    &inbox,
                    &sink,
                    &c,
                )
                .await;
                session_tools.set_subagents(None);
                result.map(|p| claude = p)
            } else {
                run_session_turn(
                    turn,
                    &cfg,
                    &on_behalf_of,
                    &model.connection_id,
                    &link,
                    &tools,
                    &session_tools,
                    &sink,
                    &writer,
                    &mut conv,
                    &inbox,
                    &c,
                    sub,
                )
                .await
            };
            match result {
                Ok(()) => Some((conv, claude)),
                Err(e) => {
                    // Only durability writes fail here.
                    error!(session = %writer.session_id, "turn failed: {e:#}");
                    None
                }
            }
        });
        (task, cancel)
    }
}

/// The latest reported call usage and resolved model window. Older CLIs that
/// omit per-call usage provide a conservative turn-total fallback instead.
/// Maintenance never changes the event log used by the UI.
struct ClaudeTrim {
    tokens: u64,
    stub: String,
}

impl ClaudeTrim {
    fn from_events(spec: &CreateSession, events: &[SessionEvent]) -> Option<Self> {
        if spec.claude_code_trim_enabled == Some(false) {
            return None;
        }
        let effective = c6_agent::effective_events(events);
        let usage = effective
            .iter()
            .rev()
            .take_while(|e| {
                !matches!(&e.kind, Some(session_event::Kind::Compaction(_)))
                    && !matches!(&e.kind, Some(session_event::Kind::ModelSwitched(m)) if m.handoff.is_some())
            })
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::Usage(u)) => Some(u),
                _ => None,
            })?;
        let tokens = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens + usage.output_tokens;
        let threshold = match spec.claude_code_trim_at {
            0 if usage.context_window > 0 => SUBAGENT_TRIM_AT.min(usage.context_window / 2),
            0 => SUBAGENT_TRIM_AT,
            n => n,
        };
        (tokens >= threshold).then(|| Self { tokens, stub: spec_prompt(spec, "session.trimmed_output", c6_agent::TRIMMED_OUTPUT) })
    }

    /// Preparation is read-only. Check quiet again afterwards: the reader may
    /// have queued native output while we examined a large transcript.
    fn candidate(&self, process: &crate::claude_code::Process, cwd: &Path) -> Option<PathBuf> {
        if !process.trim_quiet() {
            return None;
        }
        let path = crate::claude_code::transcript_path(cwd, &process.launch().conversation)?;
        match crate::claude_trim::prepare(&path, &self.stub) {
            Ok(Some(_)) if process.trim_quiet() => Some(path),
            Ok(_) => None,
            Err(e) => {
                warn!("deferring Claude Code context trim: {e:#}");
                None
            }
        }
    }

    /// Only after confirmed whole-group absence. Shutdown may flush native
    /// metadata, so re-read rather than publishing the pre-shutdown snapshot.
    async fn apply(&self, path: &Path, sink: &Arc<dyn EventSink>) -> anyhow::Result<()> {
        match crate::claude_trim::prepare(path, &self.stub).and_then(|candidate| {
            let Some(candidate) = candidate else { return Ok(None) };
            let after = self.tokens.saturating_mul(candidate.after as u64) / candidate.before.max(1) as u64;
            let count = candidate.count;
            candidate.apply()?;
            Ok(Some((count, after)))
        }) {
            Ok(Some((count, after))) => {
                sink.append(session_event::Kind::Notice(c6_proto::Notice {
                    title: format!("Trimmed {count} old tool outputs from Claude Code's context"),
                    message: format!(
                        "{}k → {}k tokens (estimated; full outputs remain in carbon6's log).",
                        self.tokens / 1000,
                        after / 1000
                    ),
                    priority: "low".into(),
                    quiet: true,
                }))
                .await?;
            }
            Ok(None) => {}
            Err(e) => warn!("deferring Claude Code context trim after shutdown: {e:#}"),
        }
        Ok(())
    }
}

/// Context maintenance at a quiet boundary, including one reached while the
/// actor drains idle output. This does not run a model turn or take any input.
#[allow(clippy::too_many_arguments)]
async fn maintain_claude_context(
    session: &str,
    process: crate::claude_code::Process,
    cwd: &Path,
    model: &str,
    stdin: &crate::claude_code::Stdin,
    register: &(dyn Fn() -> Option<crate::mcp_bridge::Registration> + Send + Sync),
    observer: Arc<crate::claude_code::Observer>,
    claude_dir: &Path,
    sink: &Arc<dyn EventSink>,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<crate::claude_code::Process>> {
    let journal = observer.journal.clone();
    // Idle-boundary maintenance can respawn outside a turn. Keep cleanup
    // excluded across the gap between retiring the old process and its launch.
    let _build_use = journal.build_folder_lifecycle.clone().read_owned().await;
    // Settings can change while a turn or native background task runs. Resolve
    // the policy from the durable current spec, not the preceding turn's copy.
    let trim = journal.load_session(session).await?.and_then(|(spec, events)| ClaudeTrim::from_events(&spec, &events));
    let Some(trim) = trim else { return Ok(Some(process)) };
    let Some(path) = trim.candidate(&process, cwd) else { return Ok(Some(process)) };
    if cancel.is_cancelled() {
        return Ok(Some(process));
    }
    let launch = process.launch().clone();
    if let Err(error) = process.close_for_replacement().await {
        if !error.is::<crate::claude_code::ReplacementBlocked>() {
            return Err(error);
        }
        sink.append(session_event::Kind::Error(ErrorEvent {
            message: format!("{error}; queued inputs remain available to take back while cleanup waits."),
        }))
        .await?;
        // The actor can cancel this slot to admit Rewind/Stop. Leave the
        // durable retirement/group records intact: cancellation is never
        // permission to edit under a live group or start another process.
        tokio::select! {
            result = crate::claude_code::wait_for_replacement(&journal, session) => result?,
            _ = cancel.cancelled() => return Ok(None),
        }
    }
    // A Rewind/Stop admitted during retirement must win before publication or
    // respawn. Keep the same ownership fence through the replacement launch.
    let Some(_admission) = crate::claude_code::replacement_admission(&journal, session, cancel).await? else {
        return Ok(None);
    };
    // Retirement can wait while realm settings change. Honor a disable, a
    // raised threshold, or a new stub before touching the stopped transcript.
    let trim = journal.load_session(session).await?.and_then(|(spec, events)| ClaudeTrim::from_events(&spec, &events));
    if cancel.is_cancelled() {
        return Ok(None);
    }
    if let Some(trim) = trim {
        trim.apply(&path, sink).await?;
    }
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let Some(bin) = crate::claude_code::find_claude() else { return Ok(None) };
    match crate::claude_code::Process::spawn_held(
        &claude_dir.join("held"),
        session,
        &bin,
        launch,
        true,
        cwd,
        model,
        stdin.clone(),
        register(),
        Some(observer),
    ) {
        Ok(process) => Ok(Some(process)),
        Err(e) => {
            warn!(session, "Claude Code context maintenance deferred resume: {e:#}");
            Ok(None)
        }
    }
}

/// Runs a Claude Code turn on the session's process (docs/background.md
/// §4), starting one if there is none, or if it was started with other
/// settings and has no background tasks to lose. Gives the process back
/// while it is up; inputs a process that died never took go back to the
/// inbox.
#[allow(clippy::too_many_arguments)]
async fn claude_turn(
    session: &str,
    turn: u64,
    mut process: Option<crate::claude_code::Process>,
    conversation: &crate::claude_code::ClaudeConversation,
    trim: Option<ClaudeTrim>,
    launch: crate::claude_code::Launch,
    cwd: &Path,
    model: &str,
    stdin: &crate::claude_code::Stdin,
    register: &(dyn Fn() -> Option<crate::mcp_bridge::Registration> + Send + Sync),
    observer: Arc<crate::claude_code::Observer>,
    // Where its process is recorded while it runs.
    claude_dir: &Path,
    inbox: &Arc<Mutex<Inbox>>,
    sink: &Arc<dyn EventSink>,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<crate::claude_code::Process>> {
    use crate::claude_code::Process;
    let journal = observer.journal.clone();
    let retirement = async |error: anyhow::Error| -> anyhow::Result<()> {
        if !error.is::<crate::claude_code::ReplacementBlocked>() {
            return Err(error);
        }
        sink.append(session_event::Kind::Error(ErrorEvent {
            message: format!("{error}; queued inputs remain available to take back while cleanup waits."),
        }))
        .await?;
        // Holding this future's running slot prevents idle adoption/wakes of
        // the retiring runtime. Keep original inputs, promotion and preamble
        // in this same continuation, without replaying any User receipts.
        crate::claude_code::wait_for_replacement(&journal, session).await?;
        Ok(())
    };
    // Started with other settings (carbon6's tools, guidance, hooks, effort,
    // worker): closed and started again, resuming its conversation, so a
    // running session takes them up. Only a process with nothing under way:
    // no tasks (a restart would end them; the next quiet turn does it), no
    // inputs it has not taken, no turn it has begun.
    let mut restarted = false;
    let mut promoted = false;
    let mut admission = None;
    // Read only while the runtime is held; writing is forbidden until its
    // entire process group is confirmed absent. Missing/unknown transcripts
    // simply defer maintenance, never the user's next turn.
    let trim_path = process.as_ref().zip(trim.as_ref()).and_then(|(p, trim)| trim.candidate(p, cwd));
    let worth_trimming = trim_path.is_some();
    if let Some(p) =
        process.take_if(|p| p.replacement_ready(&launch) || (worth_trimming && p.idle_until(std::time::Duration::ZERO).is_some()))
    {
        // A reset initializes non-held until its branch is confirmed. Promote
        // that quiet process to restart-surviving transport on its next turn;
        // this alone is not a settings/worker change to announce.
        restarted = p.launch() != &launch;
        promoted = !p.is_held();
        if let Err(e) = p.close_for_replacement().await {
            retirement(e).await?;
            admission = crate::claude_code::replacement_admission(&journal, session, cancel).await?;
            if admission.is_none() {
                return Ok(None);
            }
        }
        if worth_trimming && let Some((path, trim)) = trim_path.as_ref().zip(trim.as_ref()) {
            trim.apply(path, sink).await?;
        }
    }
    let inputs = inbox.lock().unwrap().drain();
    if process.is_none() && inputs.is_empty() {
        return Ok(None);
    }
    let more = inbox.lock().unwrap().more.clone();
    let feed = || inbox.lock().unwrap().joining();
    let failed = async |message: String| -> anyhow::Result<Option<Process>> {
        sink.start_turn(turn, &inputs).await?;
        sink.append(session_event::Kind::Error(ErrorEvent { message })).await?;
        sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
        Ok(None)
    };
    let (mut p, bin) = match process {
        Some(p) => (p, None),
        None => {
            let Some(bin) = crate::claude_code::find_claude() else {
                return failed("Claude Code (`claude`) is not installed on this worker, or not on its PATH. Install it and sign in with `claude auth login`, or set C6_CLAUDE_BIN.".into()).await;
            };
            // Held: it goes on if this worker restarts, and the next one
            // takes it up (docs/restarts.md §3).
            match Process::spawn_held(
                &claude_dir.join("held"),
                session,
                &bin,
                launch.clone(),
                conversation.resume || promoted,
                cwd,
                model,
                stdin.clone(),
                register(),
                Some(observer.clone()),
            ) {
                Ok(p) => (p, Some(bin)),
                Err(e) => return failed(format!("failed to start Claude Code: {e}")).await,
            }
        }
    };
    p.set_turn_cancel(cancel);
    if restarted && bin.is_some() {
        sink.append(session_event::Kind::Notice(c6_proto::Notice {
            title: "Claude Code started again".into(),
            message: "to apply carbon6's settings as they are now (its tools, guidance or hooks); the conversation continues.".into(),
            priority: "low".into(),
            quiet: true,
        }))
        .await?;
    }
    // A new conversation gets the preamble first (also when a resume finds
    // none: the first turn may have failed before Claude Code saved it).
    let preamble = (p.needs_preamble() || (promoted && !conversation.resume)).then_some(conversation.preamble.as_deref()).flatten();
    let mut end = p.turn_admitted(turn, inputs.clone(), false, preamble, model, sink, &feed, &more, cancel, admission).await?;
    if end.no_conversation && journal.ownership(session).await?.0 != 0 {
        // A transferred conversation was positively copied before adoption.
        // Falling back to --session-id would silently discard its native
        // history if Claude rejects that transcript on the destination.
        sink.append(session_event::Kind::Error(ErrorEvent {
            message: "Claude Code could not resume the transferred conversation; it was not replaced with a new conversation.".into(),
        }))
        .await?;
        sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
        return Ok(None);
    }
    if end.no_conversation
        && let Some(bin) = &bin
    {
        // The initial inputs already have User/TurnStarted receipts and are
        // replayed below with opened=true. Only unrecorded joining inputs
        // return to the inbox, before fallible cleanup can consume Process.
        inbox.lock().unwrap().requeue(p.take_unsent());
        let admission = if let Err(e) = p.close_for_replacement().await {
            retirement(e).await?;
            let admission = crate::claude_code::replacement_admission(&journal, session, cancel).await?;
            if admission.is_none() {
                sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Interrupted as i32 })).await?;
                return Ok(None);
            }
            admission
        } else {
            None
        };
        p = match Process::spawn_held(
            &claude_dir.join("held"),
            session,
            bin,
            launch,
            false,
            cwd,
            model,
            stdin.clone(),
            register(),
            Some(observer.clone()),
        ) {
            Ok(p) => p,
            Err(e) => {
                sink.append(session_event::Kind::Error(ErrorEvent { message: format!("failed to start Claude Code: {e}") })).await?;
                sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
                return Ok(None);
            }
        };
        p.set_turn_cancel(cancel);
        end = p.turn_admitted(turn, inputs, true, conversation.preamble.as_deref(), model, sink, &feed, &more, cancel, admission).await?;
        if end.no_conversation {
            sink.append(session_event::Kind::Error(ErrorEvent {
                message: "Claude Code could not find or create this conversation.".into(),
            }))
            .await?;
            sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
        }
    }
    if end.alive {
        // A wait on carbon6-held shells or subagents starts now, not with
        // the next input. They survive this CLI's restart; its native tasks
        // do not, and candidate() refuses those. No model call is made here.
        return maintain_claude_context(session, p, cwd, model, stdin, register, observer, claude_dir, sink, cancel).await;
    }
    inbox.lock().unwrap().requeue(p.take_unsent());
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn run_session_turn(
    turn: u64,
    cfg: &TurnConfig,
    // Whose model calls these are (a subagent's root session).
    session_id: &str,
    connection_id: &str,
    link: &Arc<HeadLink>,
    tools: &[Arc<dyn Tool>],
    session_tools: &Arc<SessionTools>,
    sink: &Arc<dyn EventSink>,
    writer: &Arc<Writer>,
    conv: &mut Conversation,
    inbox: &Mutex<Inbox>,
    cancel: &CancellationToken,
    sub: crate::subagent::Parent,
) -> anyhow::Result<()> {
    let first = inbox.lock().unwrap().drain();
    let compact = first.first().filter(|i| i.is_compact()).map(|i| i.text.trim_start_matches("/compact").trim().to_owned());
    let mut blocks = Vec::new();
    for input in first {
        sink.append(input.event()).await?;
        if !input.is_compact() {
            blocks.extend(input.blocks());
        }
    }
    match conv.history.last_mut() {
        Some(m) if m.role == Role::User => m.blocks.extend(blocks),
        _ if !blocks.is_empty() => conv.history.push(Message { role: Role::User, blocks }),
        _ => {}
    }

    // Credentials are checked up front (a missing sign-in says so at once),
    // then fetched for every call, so a long turn outlives its token.
    let checked = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })).await?;
            sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Interrupted as i32 })).await?;
            return Ok(());
        }
        checked = link.model_credential(session_id, connection_id) => checked,
    };
    let client: Arc<dyn c6_llm::ModelClient> =
        match checked.and_then(|g| Ok(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers)?)) {
            Ok(_) => Arc::new(crate::link::LinkedClient::new(link.clone(), session_id, connection_id).track_activity(writer)),
            Err(e) => {
                sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })).await?;
                sink.append(session_event::Kind::Error(ErrorEvent { message: format!("cannot reach the model: {e:#}") })).await?;
                sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
                return Ok(());
            }
        };
    if let Some(focus) = compact {
        c6_agent::compact_turn(turn, cfg, client.as_ref(), tools, sink, conv, &focus, cancel).await?;
        return Ok(());
    }
    let mut pending = || inbox.lock().unwrap().drain();
    let visible = delegation_tools_for_turn(&sub.spec, tools);
    session_tools.set_subagents(Some(Arc::new(crate::subagent::Runner::new(sub, tools))));
    let result = run_turn(turn, cfg, client.as_ref(), &visible, session_tools, sink, conv, &mut pending, cancel).await;
    session_tools.set_subagents(None);
    result?;
    Ok(())
}

/// The tools a session has: its profile's tools, a skill loader when it
/// has skills, and its MCP servers' tools.
pub(crate) struct BuiltTools {
    pub tools: Vec<Arc<dyn Tool>>,
    pub skills: Vec<Skill>,
    pub errors: Vec<String>,
}

/// What carbon6's own loop runs a session with: the tool profile of its
/// model, the tools in it, and the system prompt (the environment section
/// in `system_prompt`, then SessionStart hook context, memory, task-card
/// guidance and the skills index). `errors`: setup problems to show.
pub(crate) struct OwnRuntime {
    pub profile: ToolProfile,
    pub tools: Vec<Arc<dyn Tool>>,
    pub prompt: String,
    pub errors: Vec<String>,
}

/// Builds `OwnRuntime` for a session working in `local`, at creation
/// (`source`: "startup") or when it is handed over from Claude Code
/// ("resume"). `session_id` is whose secrets these are.
pub(crate) async fn own_runtime(
    link: &HeadLink,
    session_id: &str,
    spec: &CreateSession,
    local: &Path,
    labels: &[String],
    system_prompt: String,
    source: &str,
) -> OwnRuntime {
    let model = spec.model.clone().unwrap_or_default();
    let provider = Provider::from_kind(&model.connection_kind).unwrap_or(Provider::OpenAiChat);
    let profile = model_info(provider, &model.model).tool_profile;
    // Secrets go into MCP servers' configs (and, per turn, commands).
    let names: Vec<String> = applicable_secrets(spec, labels).into_iter().map(|(n, _)| n).collect();
    let (secret_values, _) = fetch_secrets(link, session_id, &names).await;
    let built = build_tools(spec, local, profile, labels, &secret_values).await;
    let mut prompt = prompt_for_tools(spec, &system_prompt, &built.tools, profile);
    if let Some(h) = crate::hooks::HookRunner::new(session_id, local, &spec.hooks_json, labels)
        && let Some(context) = h.run("SessionStart", None, serde_json::json!({"source": source})).await.context
    {
        prompt.push_str(&format!("\n\n# Session start context (from a SessionStart hook)\n\n{context}"));
    }
    prompt.push_str(&context_sections(spec, labels, &built.tools));
    // When to propose task cards (docs/task-cards.md): an editable prompt.
    if has_tool(&built.tools, "task_propose") && has_tool(&built.tools, "task_withdraw") {
        let guidance = spec_prompt(spec, "session.task_cards", "");
        if !guidance.trim().is_empty() {
            prompt.push_str(TASK_CARDS_HEADING);
            prompt.push_str(guidance.trim());
        }
    }
    prompt.push_str(&carbon6_guidance(spec, &built.tools));
    if !built.skills.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&Skill::index(&built.skills, SkillTool::tool_name(profile == ToolProfile::Claude)));
    }
    OwnRuntime { profile, tools: built.tools, prompt, errors: built.errors }
}

fn yes() -> bool {
    true
}

/// The memory section of a new session's system prompt: the notes whose
/// scope matches its labels (most specific first, newest first, within a
/// budget), and how to keep them current.
pub(crate) fn memory_section(spec: &CreateSession, labels: &[String], tools: &[Arc<dyn Tool>]) -> String {
    const BUDGET: usize = 8000;
    #[derive(serde::Deserialize)]
    struct Note {
        id: String,
        #[serde(default)]
        scope: String,
        text: String,
        #[serde(default)]
        updated_at: i64,
    }
    let mut notes: Vec<Note> = serde_json::from_str::<Vec<Note>>(&spec.memories_json)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.scope.is_empty() || c6_api::labels::label_matches(&n.scope, labels))
        .collect();
    notes.sort_by_key(|n| (n.scope.is_empty(), std::cmp::Reverse(n.updated_at)));
    let shown: Vec<String> = c6_api::labels::display_labels(labels);
    let remember = has_tool(tools, "remember");
    let forget = has_tool(tools, "forget");
    let mut s = if remember && forget {
        format!(
            "\n\n# Memory\n\nNotes kept across sessions. They may be out of date: trust what you observe, and keep them current. When you learn \
         something durable (how a machine or project is set up, where things are, the user's preferences), save it with the remember \
         tool, scoped to one of this session's labels ({}) or to every session; correct a note by passing its id as replaces, and \
         remove wrong ones with forget.\n",
            shown.join(", ")
        )
    } else {
        let mut s = "\n\n# Memory\n\nNotes kept across sessions. They may be out of date: trust what you observe.".to_owned();
        if remember {
            s.push_str(&format!(
                " When you learn something durable, save it with the remember tool, scoped to one of this session's labels ({}) or to every session; correct a note by passing its id as replaces.",
                shown.join(", ")
            ));
        }
        if forget {
            s.push_str(" Remove wrong notes with forget.");
        }
        s.push('\n');
        s
    };
    if notes.is_empty() {
        s.push_str("\n(No notes yet.)\n");
    }
    let mut used = 0;
    for n in notes {
        let line = format!("- [{}] ({}) {}\n", n.id, if n.scope.is_empty() { "all" } else { &n.scope }, n.text.replace('\n', " "));
        used += line.len();
        if used > BUDGET {
            s.push_str("- (more notes not shown)\n");
            break;
        }
        s.push_str(&line);
    }
    s
}

/// The head's secrets that apply to a session with `labels`: (name,
/// description).
pub(crate) fn applicable_secrets(spec: &CreateSession, labels: &[String]) -> Vec<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Info {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        applies_to: Vec<String>,
    }
    serde_json::from_str::<Vec<Info>>(&spec.secrets_json)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| c6_api::labels::applies(&s.applies_to, labels))
        .map(|s| (s.name, s.description))
        .collect()
}

/// The values of the secrets that apply, from the head (cached on the
/// worker for a few minutes, in memory only).
/// Each is asked for on behalf of `session_id`, so the head can check the
/// secret belongs to the session's realm.
async fn fetch_secrets(link: &HeadLink, session_id: &str, names: &[String]) -> (Vec<(String, String)>, Vec<String>) {
    let mut values = Vec::new();
    let mut errors = Vec::new();
    for name in names {
        match link.session_credential(session_id, &format!("secret:{name}@{session_id}")).await {
            Ok(g) => values.push((name.clone(), g.bearer)),
            Err(e) => errors.push(format!("{name}: {e:#}")),
        }
    }
    (values, errors)
}

fn spawn_model(spec: &CreateSession) -> String {
    spec.model.as_ref().map(|m| m.model.clone()).unwrap_or_default()
}

/// Of named settings, those that apply to a session with `labels`: for each
/// name, the most specific match.
fn applicable<T>(items: Vec<T>, name: impl Fn(&T) -> &str, applies_to: impl Fn(&T) -> &[String], labels: &[String]) -> Vec<T> {
    let keep: Vec<bool> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let same: Vec<(usize, &T)> = items.iter().enumerate().filter(|(_, x)| name(x) == name(it)).collect();
            c6_api::labels::best(same.iter(), |(_, x)| applies_to(x), labels).is_some_and(|(j, _)| *j == i)
        })
        .collect();
    items.into_iter().zip(keep).filter_map(|(it, k)| k.then_some(it)).collect()
}

/// The named agents (from the head's settings) that apply to a session.
pub(crate) fn session_agents(spec: &CreateSession, labels: &[String]) -> Vec<c6_tools::AgentDef> {
    #[derive(serde::Deserialize)]
    struct HeadAgent {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        tools: String,
        #[serde(default)]
        model: String,
        prompt: String,
        #[serde(default = "yes")]
        enabled: bool,
        #[serde(default)]
        applies_to: Vec<String>,
    }
    let all: Vec<HeadAgent> = serde_json::from_str::<Vec<HeadAgent>>(&spec.agents_json)
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.enabled && !a.prompt.trim().is_empty())
        .collect();
    applicable(all, |a| &a.name, |a| &a.applies_to, labels)
        .into_iter()
        .map(|a| c6_tools::AgentDef {
            name: a.name,
            description: a.description,
            tools: a.tools.split(',').map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect(),
            model: a.model,
            prompt: a.prompt,
        })
        .collect()
}

/// The connections a session's subagents may run on (from the head).
pub(crate) fn subagent_connections(spec: &CreateSession) -> Vec<c6_api::SubagentConnection> {
    serde_json::from_str(&spec.subagent_connections_json).unwrap_or_default()
}

/// Those connections, as `Task` lists them.
pub(crate) fn task_connections(spec: &CreateSession) -> Vec<c6_tools::ConnectionInfo> {
    subagent_connections(spec)
        .into_iter()
        .map(|c| c6_tools::ConnectionInfo { name: c.name, kind: c.kind, default_model: c.default_model })
        .collect()
}

/// The templates a session's agent may start as remote subagents (from
/// the head), as `Task` lists them.
pub(crate) fn task_templates(spec: &CreateSession) -> Vec<c6_tools::TemplateInfo> {
    serde_json::from_str::<Vec<c6_api::SubagentTemplate>>(&spec.subagent_templates_json)
        .unwrap_or_default()
        .into_iter()
        .map(|t| c6_tools::TemplateInfo { name: t.name, description: t.description, place: t.place, model: t.model, targets: t.targets })
        .collect()
}

/// The line introducing them (an editable prompt).
fn templates_intro(spec: &CreateSession) -> String {
    spec_prompt(spec, "session.subagent_templates", c6_agent::SUBAGENT_TEMPLATES)
}

/// Built-in tools, before MCP discovery and the session's tool list.
fn builtin_tools(
    spec: &CreateSession,
    profile: ToolProfile,
    labels: &[String],
    chrome: Option<PathBuf>,
) -> (Vec<Arc<dyn Tool>>, Vec<Skill>) {
    let mut tools = c6_tools::tools_for(profile);
    // Deliverables for the Outputs panel; memory across sessions.
    tools.push(Arc::new(c6_tools::Publish::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::Remember::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::Forget::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::Notify::new(profile == ToolProfile::Claude)));
    // Questions for the user, for later (never stop to ask).
    tools.push(Arc::new(c6_tools::knowledge::TodoAdd::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::knowledge::TodoResolve::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::knowledge::DecisionRequest::new(profile == ToolProfile::Claude)));
    // Work for another session, proposed as a card (docs/task-cards.md).
    tools.push(Arc::new(c6_tools::knowledge::TaskPropose::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::knowledge::TaskWithdraw::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::knowledge::GrantRequest::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::knowledge::CommonsPublish::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::kits::KitRun::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::kits::KitShell::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::templates::TemplateStart::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::apps::AppCall::new(profile == ToolProfile::Claude)));
    // The work the session belongs to (docs/work.md §4).
    tools.push(Arc::new(c6_tools::records::Work::new(profile == ToolProfile::Claude)));
    // Documents written with the user (docs/docs.md §9).
    tools.push(Arc::new(c6_tools::records::Doc::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::Carbon6::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::SeriesRecord::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::SeriesQuery::new(profile == ToolProfile::Claude)));
    // Collections: typed tables in the session's realm.
    tools.push(Arc::new(c6_tools::records::RecordsQuery::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::RecordsGet::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::RecordsPut::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::RecordsDelete::new(profile == ToolProfile::Claude)));
    // carbon6's API reference, for writing code that works with carbon6.
    tools.push(Arc::new(c6_tools::api_reference::ApiReference::new(profile == ToolProfile::Claude)));
    // Messages to other sessions in the realm (docs/background.md §6).
    tools.push(Arc::new(c6_tools::messages::MessageSession::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::messages::ListSessions::new(profile == ToolProfile::Claude)));
    if spec.memex_tools {
        tools.push(Arc::new(c6_tools::memex::MemexSearch::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::memex::MemexGet::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::memex::MemexNote::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::memex::MemexTrail::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::memex::Claims::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::memex::ClaimAdd::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::memex::ClaimRetract::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::memex::ThingCreate::new(profile == ToolProfile::Claude)));
        // Environment knowledge.
        tools.push(Arc::new(c6_tools::knowledge::KnowledgeRecord::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::knowledge::KnowledgeVerify::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::knowledge::KnowledgeList::new(profile == ToolProfile::Claude)));
        tools.push(Arc::new(c6_tools::knowledge::KnowledgeSet::new(profile == ToolProfile::Claude)));
    }
    let claude = profile == ToolProfile::Claude;
    if spec.memex_mail {
        tools.push(Arc::new(c6_tools::memex::MailThreads::new(claude)));
        tools.push(Arc::new(c6_tools::memex::MailThread::new(claude)));
        tools.push(Arc::new(c6_tools::memex::MailDraft::new(claude)));
        tools.push(Arc::new(c6_tools::memex::MailSend::new(claude)));
    }
    if spec.memex_mail || spec.memex_chat {
        tools.push(Arc::new(c6_tools::memex::Requests::new(claude)));
        tools.push(Arc::new(c6_tools::memex::Draft::new(claude)));
    }
    if spec.memex_chat {
        tools.push(Arc::new(c6_tools::memex::ChatMessages::new(claude)));
        tools.push(Arc::new(c6_tools::memex::ChatThread::new(claude)));
        tools.push(Arc::new(c6_tools::memex::ChatPost::new(claude)));
    }
    if spec.memex_calendar {
        tools.push(Arc::new(c6_tools::memex::CalendarEvents::new(claude)));
        tools.push(Arc::new(c6_tools::memex::CalendarCreate::new(claude)));
        tools.push(Arc::new(c6_tools::memex::CalendarRsvp::new(claude)));
    }
    // A browser, where this machine has one (it runs here, also for SSH
    // sessions).
    if let Some(chrome) = chrome {
        tools.push(Arc::new(c6_tools::browser::BrowserTool::new(profile == ToolProfile::Claude, chrome)));
    }
    // Named agents (from the head's settings) become Task's subagent types,
    // and it offers the connections subagents may run on.
    // Templates it may start as remote subagents (docs/remote-subagents.md).
    let agents = session_agents(spec, labels);
    let connections = task_connections(spec);
    let templates = task_templates(spec);
    if let Some(i) = tools.iter().position(|t| c6_tools::Task::is_task(&t.spec().name)) {
        tools[i] = Arc::new(
            c6_tools::Task::with_agents(profile == ToolProfile::Claude, agents)
                .with_group_guidance(spec_prompt(spec, "session.subagent_group", ""))
                .with_connections(connections)
                .with_templates(&templates_intro(spec), templates),
        );
    }

    #[derive(serde::Deserialize)]
    struct HeadSkill {
        name: String,
        #[serde(default)]
        description: String,
        body: String,
        #[serde(default = "yes")]
        enabled: bool,
        #[serde(default)]
        applies_to: Vec<String>,
    }
    let all: Vec<HeadSkill> =
        serde_json::from_str::<Vec<HeadSkill>>(&spec.skills_json).unwrap_or_default().into_iter().filter(|s| s.enabled).collect();
    let mut skills: Vec<Skill> = applicable(all, |s| &s.name, |s| &s.applies_to, labels)
        .into_iter()
        .map(|s| Skill { name: s.name, description: s.description, body: s.body, dir: None })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    if !skills.is_empty() {
        tools.push(Arc::new(SkillTool::new(profile == ToolProfile::Claude, Arc::new(skills.clone()))));
    }

    // Web search through the head's provider: for models whose provider
    // has none of its own (or every model, if so set).
    let search: c6_tools::SearchConfig = serde_json::from_str(&spec.web_search_json).unwrap_or_default();
    let own_search = spec.model.as_ref().and_then(|m| Provider::from_kind(&m.connection_kind)).is_some_and(|p| p != Provider::OpenAiChat);
    if !search.provider.is_empty() && (search.always || !own_search) {
        tools.push(Arc::new(c6_tools::WebSearch::new(profile == ToolProfile::Claude, search)));
    }

    (tools, skills)
}

/// carbon6's tools a Claude Code session gets over its MCP bridge unless
/// Settings → Agents says otherwise (docs/background.md §4): its subagents,
/// documents and session messages.
pub const CLAUDE_CODE_DEFAULT_TOOLS: &[&str] = &["Task", "SendMessage", "TaskOutput", "TaskStop", "doc", "MessageSession", "ListSessions"];

/// carbon6's tools loaded into Claude Code's prompt up front unless
/// Settings → Agents says otherwise; the rest wait behind its tool search:
/// subagents, documents, the work it serves, memory and task cards.
pub const CLAUDE_CODE_CORE_TOOLS: &[&str] =
    &["Task", "SendMessage", "doc", "Work", "Carbon6", "Remember", "Forget", "TaskPropose", "TaskWithdraw"];

/// The tools chosen for the session's Claude Code: `None` for every one the
/// session has (automatic), else those named (from an older head that
/// sends no choice: the defaults).
fn claude_code_choice(spec: &CreateSession) -> Option<Vec<String>> {
    match &spec.claude_code {
        Some(c) if c.automatic => None,
        Some(c) => Some(c.tools.clone()),
        None if spec.claude_code_tools_chosen => Some(spec.claude_code_tools.clone()),
        None => Some(CLAUDE_CODE_DEFAULT_TOOLS.iter().map(|t| (*t).to_owned()).collect()),
    }
}

/// Claude Code's own tools a session keeps unless Settings → Agents says
/// all (docs/background.md §4, Claude Code's own tools): its coding tools,
/// and its tool search for carbon6's tools not loaded up front. The rest
/// (its subagents, web, schedules, artifacts…) are off; carbon6's come
/// over MCP instead.
pub const CLAUDE_CODE_NATIVE_TOOLS: &[&str] = &["Bash", "Read", "Write", "Edit", "ToolSearch"];

/// The Claude Code tools the session's process gets: `None` for all of
/// them (the escape hatch), else the coding tools a routing rule's tool
/// list allows (its tool search always: it only finds carbon6's tools).
pub(crate) fn claude_code_native(spec: &CreateSession) -> Option<Vec<String>> {
    // A subagent's named agent keeps it to its list (and so to these).
    let agent = crate::subagent::agent_tools(spec);
    if agent.is_none() && spec.claude_code.as_ref().is_some_and(|c| c.native_all) {
        return None;
    }
    let agent = agent.unwrap_or_default();
    Some(
        CLAUDE_CODE_NATIVE_TOOLS
            .iter()
            .filter(|t| **t == "ToolSearch" || (c6_tools::tool_allowed(&spec.tools, t) && c6_tools::tool_allowed(&agent, t)))
            .map(|t| (*t).to_owned())
            .collect(),
    )
}

/// The settings carbon6 passes to Claude Code (`--settings`): the extra
/// settings of Settings → Agents (by default no Claude co-author lines),
/// without the keys carbon6 sets itself, then the head's hooks (`hooks`,
/// settings JSON). The lockdown adds its guard and plugin switches.
pub(crate) fn claude_code_settings(spec: &CreateSession, hooks: Option<&str>) -> String {
    let raw = spec.claude_code.as_ref().map(|c| c.settings_json.as_str()).filter(|s| !s.trim().is_empty());
    let mut v: serde_json::Value = serde_json::from_str(raw.unwrap_or(c6_api::CLAUDE_CODE_SETTINGS_DEFAULT))
        .ok()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::from_str(c6_api::CLAUDE_CODE_SETTINGS_DEFAULT).unwrap_or_default());
    if let Some(obj) = v.as_object_mut() {
        obj.retain(|k, _| !c6_api::CLAUDE_CODE_SETTINGS_PROTECTED.contains(&k.as_str()));
        if let Some(h) = hooks.and_then(|h| serde_json::from_str::<serde_json::Value>(h).ok()).and_then(|h| h.get("hooks").cloned()) {
            obj.insert("hooks".into(), h);
        }
    }
    v.to_string()
}

/// Those of them loaded up front (`_meta` anthropic/alwaysLoad).
fn claude_code_core(spec: &CreateSession) -> Vec<String> {
    match &spec.claude_code {
        Some(c) if c.core_chosen => c.core.clone(),
        _ => CLAUDE_CODE_CORE_TOOLS.iter().map(|t| (*t).to_owned()).collect(),
    }
}

/// `name` is one of `chosen` (in any profile's naming; none chosen: none).
fn is_chosen(chosen: &[String], name: &str) -> bool {
    chosen.iter().any(|c| c6_tools::tool_allowed(std::slice::from_ref(c), name))
}

/// carbon6's tools that may be offered to Claude Code beyond its subagents
/// and documents: carbon6's own, without those Claude Code keeps its own
/// of (files, shell, plan, skills, its own background tasks), in Claude
/// Code's naming. Its own web tools are off unless it keeps all its tools,
/// so carbon6's `WebFetch` comes instead, and `WebSearch` where a search
/// provider is set up whose key (Brave, Tavily) the session has.
fn claude_code_extras(spec: &CreateSession, labels: &[String], chrome: Option<PathBuf>) -> Vec<Arc<dyn Tool>> {
    let own: HashSet<String> = c6_tools::tools_for(ToolProfile::Claude).iter().map(|t| t.spec().name).collect();
    let web = claude_code_native(spec).is_some();
    let search = || {
        let cfg: c6_tools::SearchConfig = serde_json::from_str(&spec.web_search_json).unwrap_or_default();
        !matches!(cfg.provider.as_str(), "brave" | "tavily") || applicable_secrets(spec, labels).iter().any(|(n, _)| *n == cfg.secret)
    };
    let (tools, _) = builtin_tools(spec, ToolProfile::Claude, labels, chrome);
    tools
        .into_iter()
        .filter(|t| match t.spec().name.as_str() {
            "WebFetch" => web,
            "WebSearch" => web && search(),
            "Skill" | "Doc" => false,
            name => !own.contains(name),
        })
        .collect()
}

/// Every carbon6 tool a Claude Code session may be given (for Settings →
/// Agents), with its description's first sentence, the defaults first.
pub fn claude_code_tool_names() -> Vec<(String, String)> {
    let spec = CreateSession { memex_tools: true, memex_mail: true, memex_chat: true, memex_calendar: true, ..Default::default() };
    let first = |d: &str| d.split(". ").next().unwrap_or(d).trim_end_matches('.').to_owned() + ".";
    let mut out: Vec<(String, String)> = vec![
        ("Task".into(), "Starts carbon6 subagents, on any connection and model the session may use.".into()),
        ("SendMessage".into(), "Continues or steers one of its carbon6 subagents.".into()),
        ("TaskOutput".into(), "Shows what a background carbon6 subagent has done so far.".into()),
        ("TaskStop".into(), "Stops a background carbon6 subagent.".into()),
    ];
    let doc: Arc<dyn Tool> = Arc::new(c6_tools::records::Doc::new(false));
    for t in std::iter::once(doc).chain(claude_code_extras(&spec, &[], Some(PathBuf::new()))) {
        let s = t.spec();
        out.push((s.name, first(&s.description)));
    }
    // The defaults first, in their order.
    out.sort_by_key(|(n, _)| CLAUDE_CODE_DEFAULT_TOOLS.iter().position(|d| d == n).unwrap_or(usize::MAX));
    out
}

/// What carbon6's MCP server says about itself (its `instructions`).
pub const CLAUDE_CODE_INTRO: &str = "carbon6 runs these tools: subagents in its own loop, on any connection and model the session may use; \
     documents, messages to other sessions and the rest in carbon6, for this session.";

/// What a Claude Code session is told about carbon6 and its tools, added
/// to its system prompt: the `session.claude_code` prompt, then the
/// guidance carbon6's loop gives with them.
fn claude_code_system_prompt(spec: &CreateSession, labels: &[String], tools: &[Arc<dyn Tool>]) -> String {
    let note = spec_prompt(spec, "session.claude_code", c6_agent::CLAUDE_CODE_NOTE);
    let guidance = claude_code_guidance(spec, labels, tools);
    format!("{}\n\n{}", note.trim(), guidance.trim())
}

/// carbon6's guidance for the tools a Claude Code session gets, which its
/// own system prompt carries in carbon6's loop: memory (with the notes),
/// task cards, the memex brief.
fn claude_code_guidance(spec: &CreateSession, labels: &[String], tools: &[Arc<dyn Tool>]) -> String {
    let mut out = CLAUDE_CODE_INTRO.to_owned();
    if has_tool(tools, "remember") || has_tool(tools, "forget") {
        out.push_str(&memory_section(spec, labels, tools));
    }
    if has_tool(tools, "task_propose") && has_tool(tools, "task_withdraw") {
        let guidance = spec_prompt(spec, "session.task_cards", "");
        if !guidance.trim().is_empty() {
            out.push_str(TASK_CARDS_HEADING);
            out.push_str(guidance.trim());
        }
    }
    out.push_str(&carbon6_guidance(spec, tools));
    if (has_tool(tools, "memex_search") || has_tool(tools, "memex_get")) && !spec.memex_brief.trim().is_empty() {
        out.push_str("\n\n# Memex\n\n");
        out.push_str(spec.memex_brief.trim());
    }
    out
}

/// Every built-in name in a profile, including tools needing optional
/// services. Uses the runtime's assembly without starting those services.
pub fn builtin_tool_names(profile: ToolProfile) -> Vec<String> {
    let spec = CreateSession {
        memex_tools: true,
        memex_mail: true,
        memex_chat: true,
        memex_calendar: true,
        skills_json: r#"[{"name":"catalog","body":""}]"#.into(),
        web_search_json: r#"{"provider":"catalog","always":true}"#.into(),
        ..Default::default()
    };
    let (tools, _) = builtin_tools(&spec, profile, &[], Some(PathBuf::new()));
    let mut names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
    names.sort();
    names
}

pub(crate) async fn build_tools(
    spec: &CreateSession,
    cwd: &std::path::Path,
    profile: ToolProfile,
    labels: &[String],
    secrets: &[(String, String)],
) -> BuiltTools {
    let (mut tools, mut skills) = builtin_tools(spec, profile, labels, c6_tools::browser::find_chrome());
    let mut errors = Vec::new();
    // MCP servers that apply here, with ${SECRET} references filled in.
    let mut servers: Vec<c6_mcp::ServerConfig> = Vec::new();
    for s in serde_json::from_str::<Vec<c6_mcp::ServerConfig>>(&spec.mcp_servers_json).unwrap_or_default() {
        if !c6_api::labels::applies(&s.applies_to, labels) {
            continue;
        }
        match s.with_secrets(secrets) {
            Ok(s) => servers.push(s),
            Err(missing) => errors.push(format!(
                "MCP server {} not started: it needs the secret{} {} (add {} under Settings → Secrets, applying to this session)",
                s.name,
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", "),
                if missing.len() == 1 { "it" } else { "them" }
            )),
        }
    }
    if !servers.is_empty() {
        let (set, errs) = c6_mcp::McpSet::connect(&servers, cwd).await;
        errors.extend(errs.into_iter().map(|e| format!("MCP server unavailable: {e}")));
        tools.extend(set.tools().await);
    }
    tools.retain(|t| c6_tools::tool_allowed(&spec.tools, &t.spec().name));
    if !tools.iter().any(|t| t.spec().name == SkillTool::tool_name(profile == ToolProfile::Claude)) {
        skills.clear();
    }
    BuiltTools { tools, skills, errors }
}

fn delegation_tools_for_turn(spec: &CreateSession, tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    let allowed = crate::subagent::may_delegate(spec, crate::subagent::depth_of(spec));
    tools.iter().filter(|t| allowed || !crate::subagent::delegation_tool(&t.spec().name)).cloned().collect()
}

pub(crate) fn visible_tool_names(spec: &CreateSession, tools: &[Arc<dyn Tool>]) -> Vec<String> {
    let allowed = crate::subagent::may_delegate(spec, crate::subagent::depth_of(spec));
    tools.iter().map(|t| t.spec().name).filter(|name| allowed || !crate::subagent::delegation_tool(name)).collect()
}

/// The single writer of one session's log.
pub(crate) struct Writer {
    pub session_id: String,
    pub journal: Arc<Journal>,
    pub link: Arc<HeadLink>,
    pub next_seq: tokio::sync::Mutex<u64>,
    pub(crate) ownership_epoch: u64,
    /// `next_seq` as of the last append, read without waiting on one: what
    /// streamed text is stamped with.
    logged: std::sync::atomic::AtomicU64,
    activity: Arc<std::sync::atomic::AtomicI32>,
    /// Outstanding call IDs; true means a subagent dependency, not other work.
    outstanding_tools: Mutex<HashMap<String, bool>>,
    disk_probe: Option<(Arc<crate::disk::Monitor>, PathBuf)>,
    resource_failure: std::sync::atomic::AtomicBool,
    cache_observation: Mutex<CacheObservation>,
    cache_capture: Mutex<(bool, u64)>,
    cache_clock: (std::sync::atomic::AtomicI64, tokio::time::Instant),
}

/// Rebuilt from the same usage/turn journal that drives accounting. Background
/// liveness is deliberately not stored here: the existing registry owns it.
#[derive(Clone, Default)]
struct CacheObservation {
    usage: Option<(i64, c6_proto::Usage)>,
    // Conservative lower bounds for opaque CLI request starts. Tool results
    // and input precede a call; its completed assistant/usage do not.
    call_boundary_ms: Option<i64>,
    call_started_ms: Option<i64>,
    last_call_started_ms: Option<i64>,
    last_attempt_at_ms: Option<i64>,
    wait_started_ms: Option<i64>,
    pings_sent: u64,
    last_attempt_call_ms: Option<i64>,
}

impl CacheObservation {
    fn observe(&mut self, event: &SessionEvent) {
        match &event.kind {
            Some(session_event::Kind::Usage(usage)) => {
                if usage.keep_warm {
                    self.pings_sent = self.pings_sent.saturating_add(1);
                    self.last_call_started_ms = self.last_attempt_at_ms;
                } else {
                    self.last_call_started_ms = self.call_started_ms.take().or(self.call_boundary_ms);
                    self.call_boundary_ms = Some(event.ts_ms);
                }
                self.usage = Some((event.ts_ms, usage.clone()));
            }
            Some(session_event::Kind::Assistant(_)) => {
                if self.call_started_ms.is_none() {
                    self.call_started_ms = self.call_boundary_ms;
                }
            }
            Some(session_event::Kind::ToolResult(_)) => self.call_boundary_ms = Some(event.ts_ms),
            Some(session_event::Kind::TurnEnded(end)) => {
                self.wait_started_ms = (end.reason == TurnEndReason::Completed as i32).then_some(event.ts_ms);
                self.pings_sent = 0;
            }
            Some(session_event::Kind::User(_) | session_event::Kind::TurnStarted(_) | session_event::Kind::Rewound(_)) => {
                self.wait_started_ms = None;
                self.pings_sent = 0;
                self.call_boundary_ms = Some(event.ts_ms);
                self.call_started_ms = None;
            }
            Some(session_event::Kind::Notice(notice)) if notice.quiet && notice.title == "Cache maintenance attempted" => {
                self.last_attempt_call_ms = notice.message.parse().ok();
                self.last_attempt_at_ms = Some(event.ts_ms);
            }
            Some(session_event::Kind::Notice(notice)) if notice.quiet && notice.title == "Cache maintenance failed" => {
                self.wait_started_ms = None;
            }
            Some(session_event::Kind::ModelSwitched(_) | session_event::Kind::PlaceChanged(_) | session_event::Kind::Compaction(_)) => {
                // A new account, runtime or rewritten prompt must earn its
                // own completed-call evidence before maintenance can start.
                *self = Self::default();
            }
            Some(session_event::Kind::Notice(notice))
                if notice.quiet && notice.title.starts_with("Trimmed ") && notice.title.ends_with("from Claude Code's context") =>
            {
                *self = Self::default();
            }
            _ => {}
        }
    }
}

impl Writer {
    /// A writer whose next event is `next_seq`.
    pub fn new(session_id: String, journal: Arc<Journal>, link: Arc<HeadLink>, next_seq: u64) -> Self {
        Writer {
            session_id,
            journal,
            link,
            next_seq: tokio::sync::Mutex::new(next_seq),
            ownership_epoch: 0,
            logged: std::sync::atomic::AtomicU64::new(next_seq),
            activity: Default::default(),
            outstanding_tools: Default::default(),
            disk_probe: None,
            resource_failure: Default::default(),
            cache_observation: Default::default(),
            cache_capture: Mutex::new((true, 50_000)),
            cache_clock: (std::sync::atomic::AtomicI64::new(c6_core::now_ms()), tokio::time::Instant::now()),
        }
    }

    /// Wall-time journal timestamps advanced by Tokio's monotonic clock. This
    /// makes cache timers injectable without changing the process wall clock.
    pub(crate) fn now_ms(&self) -> i64 {
        // Monotonic clocks may pause during machine sleep. Re-anchor forward
        // so resuming an old timer cannot dispatch a now-cold request.
        self.advance_clock_to(c6_core::now_ms());
        self.cache_clock.0.load(Ordering::Relaxed).saturating_add(self.cache_clock.1.elapsed().as_millis().min(i64::MAX as u128) as i64)
    }

    fn advance_clock_to(&self, observed_ms: i64) {
        let elapsed = self.cache_clock.1.elapsed().as_millis().min(i64::MAX as u128) as i64;
        self.cache_clock.0.fetch_max(observed_ms.saturating_sub(elapsed), Ordering::Relaxed);
    }

    fn set_keep_warm_capture(&self, spec: &CreateSession) {
        let settings =
            spec.keep_warm_json.as_deref().and_then(|json| serde_json::from_str::<c6_api::KeepWarmSettings>(json).ok()).unwrap_or_default();
        let excluded = spec.model.as_ref().is_some_and(|model| settings.disabled_connections.contains(&model.connection_id));
        *self.cache_capture.lock().unwrap() = (settings.enabled && !excluded, settings.threshold_tokens);
    }

    pub(crate) fn keep_warm_capture(&self) -> (bool, u64) {
        *self.cache_capture.lock().unwrap()
    }

    pub(crate) fn set_activity(&self, kind: c6_proto::SessionActivityKind) {
        let mut tools = self.outstanding_tools.lock().unwrap();
        tools.clear();
        self.activity.store(kind as i32, std::sync::atomic::Ordering::Relaxed);
    }

    async fn journal_result<T>(&self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        if let Err(error) = &result {
            let space = if let Some((disk, path)) = &self.disk_probe { disk.fresh_space(path).await } else { None };
            self.resource_failure.store(crate::disk::resource_error(error, space.as_ref()), Ordering::Release);
        }
        result
    }

    fn observe_activity(&self, event: &session_event::Kind) {
        use c6_proto::{SessionActivityKind, content_block};
        let mut tools = self.outstanding_tools.lock().unwrap();
        match event {
            session_event::Kind::Assistant(m) => {
                if m.stop_reason != "interrupted" {
                    tools.extend(m.blocks.iter().filter_map(|b| match &b.block {
                        Some(content_block::Block::ToolUse(call)) => {
                            let name =
                                call.name.rsplit("__").next().unwrap_or(&call.name).rsplit(['.', ':', '/']).next().unwrap_or(&call.name);
                            Some((call.id.clone(), crate::subagent::delegation_tool(name)))
                        }
                        _ => None,
                    }));
                }
            }
            session_event::Kind::ToolResult(r) if tools.remove(&r.call_id).is_some() => {}
            session_event::Kind::TurnEnded(_) => tools.clear(),
            _ => return,
        }
        let kind = if tools.is_empty() {
            SessionActivityKind::Unspecified
        } else if tools.values().all(|delegation| *delegation) {
            SessionActivityKind::Subagent
        } else {
            SessionActivityKind::Tool
        };
        self.activity.store(kind as i32, std::sync::atomic::Ordering::Relaxed);
    }
}

#[async_trait]
impl EventSink for Writer {
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
        let mut next = self.next_seq.lock().await;
        // Streamed text first: it must not arrive after what it streamed.
        self.link.flush_ephemeral();
        let ev = SessionEvent { seq: *next, ts_ms: self.now_ms(), kind: Some(kind) };
        self.journal_result(self.journal.check_ownership(&self.session_id, self.ownership_epoch).await).await?;
        let count = if matches!(&ev.kind, Some(session_event::Kind::Rewound(_))) {
            self.journal_result(self.journal.append_rewind_owned(&self.session_id, &ev, self.ownership_epoch).await).await?
        } else {
            self.journal_result(self.journal.append_owned(&self.session_id, &ev, self.ownership_epoch).await).await?;
            1
        };
        *next += count;
        self.cache_observation.lock().unwrap().observe(&ev);
        self.logged.store(*next, std::sync::atomic::Ordering::SeqCst);
        Ok(ev.seq)
    }

    async fn start_turn(&self, turn: u64, inputs: &[PendingInput]) -> anyhow::Result<()> {
        let mut next = self.next_seq.lock().await;
        self.journal_result(self.journal.check_ownership(&self.session_id, self.ownership_epoch).await).await?;
        self.link.flush_ephemeral();
        let kinds = inputs
            .iter()
            .map(PendingInput::event)
            .chain(std::iter::once(session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })));
        let events: Vec<_> = kinds
            .enumerate()
            .map(|(offset, kind)| SessionEvent { seq: *next + offset as u64, ts_ms: self.now_ms(), kind: Some(kind) })
            .collect();
        self.journal_result(self.journal.append_turn_owned(&self.session_id, &events, self.ownership_epoch).await).await?;
        for event in &events {
            self.cache_observation.lock().unwrap().observe(event);
        }
        *next += events.len() as u64;
        self.logged.store(*next, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn ephemeral(&self, mut kind: ephemeral_event::Kind) {
        // Where the log is: its next reply has this text, so a viewer that
        // got that reply first (catching up) knows to drop it.
        if let ephemeral_event::Kind::Text(d) | ephemeral_event::Kind::Thinking(d) = &mut kind {
            d.seq = self.logged.load(std::sync::atomic::Ordering::SeqCst);
        }
        self.link.ephemeral(EphemeralEvent {
            session_id: self.session_id.clone(),
            kind: Some(kind),
            ownership_epoch: self.ownership_epoch,
        });
    }
}

/// Stops a background command left running by a previous worker process,
/// if process group `pgid` still runs that command (pids get reused).
fn stop_orphan(pgid: i32, command: &str) {
    if pgid <= 0 || command.trim().is_empty() {
        return;
    }
    let args = std::process::Command::new("ps").args(["-o", "args=", "-p", &pgid.to_string()]).output();
    let Ok(out) = args else { return };
    let line = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && line.contains(command.trim()) {
        warn!(pgid, %command, "stopping a background command left by the previous worker");
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::Signal::SIGTERM);
    }
}

/// What the agent is told when a background task ends.
pub(crate) fn duration_text(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn background_task_notice(t: &BackgroundTask, output: &str) -> c6_proto::TaskNotice {
    let command = t.kind == "shell" || t.native_type == "local_bash";
    let kind = if command {
        "command"
    } else if t.native_type == "local_agent" {
        "subagent"
    } else {
        "task"
    };
    let what = if command { "Command" } else { "Task" };
    let status = t.state.replace('_', " ");
    let markdown = if !output.trim().is_empty() {
        if command { format!("```text\n{output}\n```") } else { output.to_owned() }
    } else if t.state == "lost" {
        format!("{what} lost: its process stopped before it reported an outcome. Check its work before running it again.")
    } else if t.state == "finished" && command {
        format!("{what} exited with code {}.", t.exit_code)
    } else {
        format!("{what} {status}.")
    };
    c6_proto::TaskNotice {
        kind: kind.into(),
        title: t.description.clone(),
        status: if t.state == "finished" && t.exit_code != 0 { "failed".into() } else { t.state.clone() },
        markdown,
        ..Default::default()
    }
}

fn task_notice(t: &BackgroundTask, output: &str, cause: Option<&str>) -> String {
    // One of Claude Code's own tasks, lost with its process (its id is the
    // CLI's; Claude Code itself can no longer say what became of it).
    if t.kind == "claude_code" {
        let cause = cause.unwrap_or_default();
        return format!("<background-task kind=\"claude-code\" status=\"lost\">\nTask: {}\n{cause}\n</background-task>", t.description);
    }
    let elapsed = duration_text(t.elapsed_ms / 1000);
    let agent = t.kind == "agent";
    let status = match t.state.as_str() {
        "finished" if agent && t.exit_code == 0 => "finished".to_owned(),
        "finished" if agent => "failed".to_owned(),
        "finished" => format!("exited with code {}", t.exit_code),
        "stopped_by_user" => "stopped by the user".to_owned(),
        other => other.to_owned(),
    };
    let mut s =
        format!("<background-task id=\"{}\" kind=\"{}\" status=\"{status}\"", t.task_id, if agent { "subagent" } else { "command" });
    if t.state != "lost" {
        s.push_str(&format!(" elapsed=\"{elapsed}\""));
    }
    s.push_str(">\n");
    s.push_str(&format!("{}: {}\n", if agent { "Task" } else { "Command" }, t.description));
    if let Some(cause) = cause {
        s.push_str(cause.trim_end());
        s.push('\n');
    }
    if !output.trim().is_empty() {
        s.push_str(&format!("{}:\n{}\n", if agent { "Report" } else { "Last output" }, output.trim_end()));
    }
    s.push_str("</background-task>");
    if !agent && t.state != "lost" {
        s.push_str(&format!("\n(Full output: the output tool with id {}.)", t.task_id));
    }
    s
}

/// Calls to the head on a session's behalf: the head checks the session is
/// ours and applies its scope.
struct SessionHead {
    link: Arc<HeadLink>,
    session_id: String,
}

#[async_trait::async_trait]
impl c6_tools::HeadCaller for SessionHead {
    async fn call(&self, method: &str, mut params: serde_json::Value) -> Result<serde_json::Value, String> {
        if !params.is_object() {
            params = serde_json::json!({});
        }
        params["session_id"] = serde_json::json!(self.session_id);
        self.link.call(method, params).await.map_err(|e| format!("{e:#}"))
    }
}

/// Removes sessions' browser profiles unused for `age` (sign-ins and
/// caches of sessions long done).
fn prune_browser_profiles(dir: &Path, age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d > age);
        if old && let Err(err) = std::fs::remove_dir_all(e.path()) {
            warn!(path = %e.path().display(), "removing an old browser profile: {err}");
        }
    }
}

/// Where a session's prompt says when to propose task cards; a subagent,
/// which has no such tools, leaves the section out.
pub(crate) const TASK_CARDS_HEADING: &str = "\n\n# Task cards\n\n";

/// Where the `carbon6` tool's guidance starts in the system prompt.
const CARBON6_HEADING: &str = "\n\n# Doing things in carbon6\n\n";

/// The `carbon6` tool's guidance (an editable prompt), if it has the tool.
fn carbon6_guidance(spec: &CreateSession, tools: &[Arc<dyn Tool>]) -> String {
    let guidance = spec_prompt(spec, "session.carbon6", "");
    if has_tool(tools, "carbon6") && !guidance.trim().is_empty() { format!("{CARBON6_HEADING}{}", guidance.trim()) } else { String::new() }
}

/// The system prompt's sections from the head's context: your notes, the
/// memex brief and the secrets that apply.
pub(crate) fn context_sections(spec: &CreateSession, labels: &[String], tools: &[Arc<dyn Tool>]) -> String {
    let mut prompt = String::new();
    // Your notes, by their scope; then what the knowledge store
    // knows about this session's things (it leaves the notes out).
    prompt.push_str(&memory_section(spec, labels, tools));
    if !spec.memex_brief.trim().is_empty() {
        prompt.push_str("\n\n# Memex\n\n");
        let mut brief = spec.memex_brief.trim().to_owned();
        if !spec.tools.is_empty() {
            for (names, sentence) in [
                (
                    &["knowledge_record", "knowledge_verify", "todo_add"][..],
                    " Record what you learn with knowledge_record and re-checks with knowledge_verify; never stop to ask the user about it (todo_add raises a question for later).",
                ),
                (
                    &["memex_get", "memex_search"][..],
                    "Items are cited by [id]; memex_get fetches one in full and memex_search finds more. ",
                ),
            ] {
                if !names.iter().all(|name| has_tool(tools, name)) {
                    brief = brief.replace(sentence, "");
                }
            }
        }
        prompt.push_str(&brief);
    }
    let secrets = applicable_secrets(spec, labels);
    let shell = has_tool(tools, "bash");
    if !secrets.is_empty() && (shell || has_tool(tools, "browser")) {
        if shell {
            prompt.push_str(
                "\n\n# Secrets\n\nThese environment variables hold secrets for your shell commands. Use them by name (for example \"$NAME\"), never \
             print or echo them, and do not write them into files; their values are hidden from you and replaced by [secret:NAME] in output.\n",
            );
        } else {
            prompt.push_str(
                "\n\n# Secrets\n\nThese secrets are available to the browser. To type one, use the browser's type action with its secret field \
                 set to the name. Values stay hidden from you and are redacted from tool output.\n",
            );
        }
        for (name, description) in &secrets {
            prompt.push_str(&format!("- {name}{}\n", if description.is_empty() { String::new() } else { format!(": {description}") }));
        }
    }
    prompt
}

pub(crate) fn has_tool(tools: &[Arc<dyn Tool>], name: &str) -> bool {
    tools.iter().any(|t| c6_tools::tool_allowed(&[name.to_owned()], &t.spec().name))
}

/// Older records have no denial, so keep their hosted search.
fn hosted_web_search(provider: Provider, started: &SessionStarted) -> bool {
    provider != Provider::OpenAiChat && !started.hosted_search_off
}

/// Keep unrestricted sessions' prompt bytes, including older profile
/// wording. A tool list only removes the stock tool-dependent sentences.
pub(crate) fn prompt_for_tools(spec: &CreateSession, prompt: &str, tools: &[Arc<dyn Tool>], profile: ToolProfile) -> String {
    if spec.tools.is_empty() {
        return prompt.to_owned();
    }
    c6_agent::base_prompt_for_tools(prompt, &tools.iter().map(|t| t.spec().name).collect::<Vec<_>>(), profile)
}

/// The heading of the system prompt's skills index.
pub(crate) const SKILLS_HEADING: &str = "\n\n# Skills\n\n";

/// `prompt` without its skills index (another tool profile names the
/// skill tool differently).
pub(crate) fn without_skills(prompt: &str) -> String {
    let Some(start) = prompt.find(SKILLS_HEADING) else { return prompt.to_owned() };
    let rest = &prompt[start + SKILLS_HEADING.len()..];
    let end = rest.find("\n\n# ").map(|i| start + SKILLS_HEADING.len() + i).unwrap_or(prompt.len());
    format!("{}{}", &prompt[..start], &prompt[end..])
}

/// `prompt` without its task-cards section.
pub(crate) fn without_task_cards(prompt: &str) -> String {
    let Some(start) = prompt.find(TASK_CARDS_HEADING) else { return prompt.to_owned() };
    let rest = &prompt[start + TASK_CARDS_HEADING.len()..];
    let end = rest.find("\n\n# ").map(|i| start + TASK_CARDS_HEADING.len() + i).unwrap_or(prompt.len());
    format!("{}{}", &prompt[..start], &prompt[end..])
}

/// A prompt from the session's spec, as edited, or its default.
pub(crate) fn spec_prompt(spec: &CreateSession, name: &str, default: &str) -> String {
    serde_json::from_str::<serde_json::Value>(&spec.prompts_json)
        .ok()
        .and_then(|v| v[name].as_str().map(str::to_owned))
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// Add the advisory to the model input only, not the user's displayed report.
fn add_notice_loop_warning(spec: &CreateSession, input: &mut PendingInput, evidence: &str) {
    let warning = spec_prompt(spec, "session.notice_loop_warning", c6_agent::NOTICE_LOOP_WARNING).replace("{{evidence}}", evidence);
    if input.prompt.is_empty() {
        input.prompt.clone_from(&input.text);
    }
    input.prompt.push_str("\n\n");
    input.prompt.push_str(&warning);
}

/// What the model is told when its conversation is compacted.
pub(crate) fn compact_prompts(spec: &CreateSession) -> c6_agent::CompactPrompts {
    c6_agent::CompactPrompts {
        why_full: spec_prompt(spec, "session.compact.full", c6_agent::COMPACT_WHY),
        why_asked: spec_prompt(spec, "session.compact.asked", c6_agent::COMPACT_WHY_ASKED),
        instructions: spec_prompt(spec, "session.compact", c6_agent::COMPACT_PROMPT),
        trimmed: spec_prompt(spec, "session.trimmed_output", c6_agent::TRIMMED_OUTPUT),
    }
}

/// Where a subagent's old tool output starts to be trimmed: 200k tokens,
/// or half a smaller window (docs/replay-eval.md). Not your own sessions.
fn trim_at(spec: &CreateSession, window: u64) -> u64 {
    if spec.delegation.is_none() || window == 0 {
        return 0;
    }
    SUBAGENT_TRIM_AT.min(window / 2)
}

const SUBAGENT_TRIM_AT: u64 = 200_000;

#[cfg(test)]
mod claude_trim_tests {
    use super::*;

    fn call(tokens: u64, window: u64) -> SessionEvent {
        SessionEvent {
            kind: Some(session_event::Kind::Usage(c6_proto::Usage {
                input_tokens: 1,
                cache_read_tokens: tokens - 1,
                context_window: window,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[test]
    fn native_trim_defaults_cover_top_level_and_subagents_and_use_the_actual_window() {
        let mut spec = CreateSession::default();
        assert!(ClaudeTrim::from_events(&spec, &[]).is_none());
        assert!(ClaudeTrim::from_events(&spec, &[call(199_999, 1_000_000)]).is_none());
        assert_eq!(ClaudeTrim::from_events(&spec, &[call(200_000, 1_000_000)]).unwrap().tokens, 200_000);
        assert!(ClaudeTrim::from_events(&spec, &[call(99_999, 200_000)]).is_none());
        assert!(ClaudeTrim::from_events(&spec, &[call(100_000, 200_000)]).is_some());
        assert!(ClaudeTrim::from_events(&spec, &[call(200_000, 0)]).is_some());
        spec.delegation = Some(Default::default());
        assert!(ClaudeTrim::from_events(&spec, &[call(200_000, 1_000_000)]).is_some());
        spec.claude_code_trim_enabled = Some(false);
        assert!(ClaudeTrim::from_events(&spec, &[call(900_000, 1_000_000)]).is_none());
    }

    #[test]
    fn native_trim_uses_latest_call_and_the_editable_prompt_and_explicit_threshold() {
        let spec = CreateSession {
            claude_code_trim_at: 300_000,
            prompts_json: r#"{"session.trimmed_output":"Custom {{call}} stub"}"#.into(),
            ..Default::default()
        };
        assert!(ClaudeTrim::from_events(&spec, &[call(900_000, 1_000_000), call(250_000, 1_000_000)]).is_none());
        let trim = ClaudeTrim::from_events(&spec, &[call(350_000, 1_000_000)]).unwrap();
        assert_eq!(trim.stub, "Custom {{call}} stub");
        let compact = SessionEvent { kind: Some(session_event::Kind::Compaction(Default::default())), ..Default::default() };
        assert!(ClaudeTrim::from_events(&spec, &[call(900_000, 1_000_000), compact]).is_none());
    }
}

#[cfg(test)]
mod route_tool_tests {
    use super::*;
    use prost::Message;

    fn spec() -> CreateSession {
        CreateSession {
            base_prompt: c6_agent::BASE_PROMPT.into(),
            memex_tools: true,
            skills_json: r#"[{"name":"one","description":"A skill","body":"Instructions"}]"#.into(),
            memories_json: r#"[{"id":"n1","text":"A useful note"}]"#.into(),
            secrets_json: r#"[{"name":"TOKEN","description":"A secret"}]"#.into(),
            memex_brief: "Known facts.".into(),
            prompts_json: r#"{"session.task_cards":"Propose useful work."}"#.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn hosted_web_search_obeys_the_saved_denial() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let manager = SessionManager::new(journal.clone(), Arc::new(HeadLink::default()), dir.path().join("worktrees"));
        for kind in ["anthropic", "openai", "openai_compat"] {
            let provider = Provider::from_kind(kind).unwrap();
            for (i, names) in [vec![], vec!["Read"], vec!["web_search"], vec!["WebSearch"]].into_iter().enumerate() {
                let spec = CreateSession {
                    model: Some(ModelSpec { connection_kind: kind.into(), model: "claude-sonnet-5-5".into(), ..Default::default() }),
                    cwd: dir.path().display().to_string(),
                    tools: names.into_iter().map(str::to_owned).collect(),
                    ..Default::default()
                };
                let id = format!("{kind}-{i}");
                manager.create(&id, &id, spec).await.unwrap();
                let (_, events) = journal.load_session(&id).await.unwrap().unwrap();
                let Some(session_event::Kind::Started(started)) = &events[0].kind else { panic!("no SessionStarted") };
                assert_eq!(started.hosted_search_off, i == 1);
                assert_eq!(hosted_web_search(provider, started), kind != "openai_compat" && i != 1);
                assert!(
                    !started.tools.iter().any(|t| c6_tools::tool_allowed(std::slice::from_ref(t), "web_search")),
                    "no function search configured"
                );
            }
        }
    }

    #[test]
    fn secrets_guidance_supports_browser_without_changing_shell_bytes() {
        let spec = spec();
        let browser: Arc<dyn Tool> = Arc::new(c6_tools::browser::BrowserTool::new(false, PathBuf::new()));
        let shell: Arc<dyn Tool> = Arc::new(c6_tools::Bash);
        let shell_prompt = context_sections(&spec, &[], std::slice::from_ref(&shell));
        assert!(shell_prompt.contains("These environment variables hold secrets for your shell commands."));
        assert_eq!(context_sections(&spec, &[], &[shell, browser.clone()]), shell_prompt);
        let prompt = context_sections(&spec, &[], &[browser]);
        assert!(prompt.contains("# Secrets") && prompt.contains("TOKEN: A secret"));
        assert!(prompt.contains("browser's type action with its secret field set to the name"));
        assert!(!prompt.contains("shell commands") && !prompt.contains("environment variables"));
        assert!(!context_sections(&spec, &[], &[]).contains("# Secrets"));
    }

    #[tokio::test]
    async fn tool_lists_filter_generic_claude_and_codex_tools() {
        let dir = tempfile::tempdir().unwrap();
        let spec = CreateSession {
            tools: ["Read", "Bash", "Edit", "TodoWrite", "Skill", "MemexSearch", "Task", "SendMessage", "BashOutput", "KillShell"]
                .map(str::to_owned)
                .to_vec(),
            ..spec()
        };
        for (profile, expected) in [
            (
                ToolProfile::Generic,
                vec![
                    "bash",
                    "bash_output",
                    "edit",
                    "kill_shell",
                    "load_skill",
                    "memex_search",
                    "read",
                    "send_message",
                    "task",
                    "update_plan",
                ],
            ),
            (
                ToolProfile::Claude,
                vec!["Bash", "BashOutput", "Edit", "KillShell", "MemexSearch", "Read", "SendMessage", "Skill", "Task", "TodoWrite"],
            ),
            (
                ToolProfile::Codex,
                vec![
                    "apply_patch",
                    "bash_output",
                    "kill_shell",
                    "load_skill",
                    "memex_search",
                    "send_message",
                    "shell",
                    "task",
                    "update_plan",
                ],
            ),
        ] {
            let built = build_tools(&spec, dir.path(), profile, &[], &[]).await;
            let mut names: Vec<String> = built.tools.iter().map(|t| t.spec().name).collect();
            names.sort();
            assert_eq!(names, expected, "{profile:?}");
            assert_eq!(built.skills.len(), 1);
            assert!(built.errors.is_empty(), "{:?}", built.errors);
        }
        let unavailable = CreateSession { tools: vec!["mcp__missing__tool".into()], ..spec };
        let built = build_tools(&unavailable, dir.path(), ToolProfile::Generic, &[], &[]).await;
        assert!(built.tools.is_empty() && built.skills.is_empty(), "an unavailable list must not fall back to all tools");
    }

    #[tokio::test]
    async fn codex_subagents_offer_agents_connections_and_templates_and_obey_depth_and_tool_limits() {
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let spec = CreateSession {
            model: Some(ModelSpec { connection_kind: "chatgpt".into(), model: "gpt-6.1-sol".into(), ..Default::default() }),
            agents_json: json!([{"name": "finder", "description": "Find code", "prompt": "Look around."}]).to_string(),
            subagent_connections_json: json!([{"id": "gpt", "name": "GPT", "kind": "chatgpt", "default_model": "gpt-6.1-sol"}]).to_string(),
            subagent_templates_json: json!([{"name": "review", "place": "other-worker:/repo", "targets": ["other-worker"]}]).to_string(),
            prompts_json: json!({"session.subagent_templates": "Available remote reviewers:"}).to_string(),
            max_subagent_depth: 2,
            ..Default::default()
        };
        for depth in [0, 1, 2] {
            let spec = CreateSession { depth, ..spec.clone() };
            let built = own_runtime(&HeadLink::default(), "s", &spec, dir.path(), &[], String::new(), "startup").await;
            assert_eq!(built.profile, ToolProfile::Codex);
            assert!(built.errors.is_empty(), "{:?}", built.errors);
            let visible = delegation_tools_for_turn(&spec, &built.tools);
            let task = visible.iter().find(|t| c6_tools::Task::is_task(&t.spec().name));
            assert_eq!(task.is_some(), depth < 2);
            assert_eq!(visible.iter().any(|t| c6_tools::SendMessage::is_send(&t.spec().name)), depth < 2);
            for name in ["bash_output", "kill_shell"] {
                assert!(built.tools.iter().any(|t| t.spec().name == name), "{name} stays available at depth {depth}");
            }
            if let Some(task) = task {
                let task = task.spec();
                assert_eq!(task.name, "task");
                let props = &task.parameters["properties"];
                assert_eq!(props["subagent_type"]["enum"], json!(["general-purpose", "finder"]));
                assert_eq!(props["connection"]["enum"], json!(["GPT"]));
                assert_eq!(props["template"]["enum"], json!(["review"]));
                assert_eq!(props["on"]["type"], "string");
                assert_eq!(props["run_in_background"]["type"], "boolean");
                assert!(props["run_in_background"]["description"].as_str().unwrap().contains("kill_shell"));
                for text in ["gpt-6.1-sol", "Available remote reviewers:", "other-worker:/repo"] {
                    assert!(task.description.contains(text), "{text}: {}", task.description);
                }
            }
        }
        let limited = CreateSession { delegation: Some(c6_proto::Delegation { depth: 2, ..Default::default() }), ..spec.clone() };
        let built = build_tools(&limited, dir.path(), ToolProfile::Codex, &[], &[]).await;
        assert!(
            !delegation_tools_for_turn(&limited, &built.tools)
                .iter()
                .any(|t| c6_tools::Task::is_task(&t.spec().name) || c6_tools::SendMessage::is_send(&t.spec().name))
        );
        let restricted = CreateSession { tools: vec!["BashOutput".into(), "KillShell".into()], ..spec };
        let built = build_tools(&restricted, dir.path(), ToolProfile::Codex, &[], &[]).await;
        assert_eq!(built.tools.iter().map(|t| t.spec().name).collect::<Vec<_>>(), ["bash_output", "kill_shell"]);
    }

    #[tokio::test]
    async fn tool_lists_filter_discovered_mcp_tools_by_full_name() {
        use axum::response::IntoResponse;
        use serde_json::{Value, json};

        async fn handle(axum::Json(message): axum::Json<Value>) -> axum::response::Response {
            let Some(id) = message.get("id") else { return axum::http::StatusCode::ACCEPTED.into_response() };
            let result = match message["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    json!({"protocolVersion": message["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}})
                }
                "tools/list" => {
                    let tools = ["echo", "e_cho", "other"]
                        .map(|name| json!({"name": name, "description": "A test tool", "inputSchema": {"type": "object"}}));
                    json!({"tools": tools})
                }
                _ => json!({}),
            };
            axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/mcp", axum::routing::post(handle));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        for (name, count) in [("mcp__Mail__echo", 1), ("mcp__mail__echo", 0)] {
            let spec = CreateSession {
                tools: vec![name.into()],
                mcp_servers_json: json!([{"name": "Mail", "transport": "http", "url": format!("http://{addr}/mcp")}]).to_string(),
                ..Default::default()
            };
            let built = build_tools(&spec, dir.path(), ToolProfile::Generic, &[], &[]).await;
            assert!(built.errors.is_empty(), "{:?}", built.errors);
            assert_eq!(built.tools.len(), count);
            if count == 1 {
                assert_eq!(built.tools[0].spec().name, name);
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn tool_lists_keep_unrestricted_prompt_bytes_and_omit_unavailable_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec();
        let system = build_system_prompt(&spec.base_prompt, dir.path(), "2026-10-30");
        let expected = format!(
            "{system}\n\n# Memory\n\nNotes kept across sessions. They may be out of date: trust what you observe, and keep them current. When you learn \
             something durable (how a machine or project is set up, where things are, the user's preferences), save it with the remember \
             tool, scoped to one of this session's labels () or to every session; correct a note by passing its id as replaces, and \
             remove wrong ones with forget.\n- [n1] (all) A useful note\n\n\n# Memex\n\nKnown facts.\n\n# Secrets\n\n\
             These environment variables hold secrets for your shell commands. Use them by name (for example \"$NAME\"), never \
             print or echo them, and do not write them into files; their values are hidden from you and replaced by [secret:NAME] in output.\n\
             - TOKEN: A secret\n{TASK_CARDS_HEADING}Propose useful work.\n\n# Skills\n\n\
             These skills hold instructions for specific kinds of work. When one applies, load it with the load_skill tool before starting.\n\
             - one: A skill\n"
        );
        let link = HeadLink::default();
        let built = own_runtime(&link, "s", &spec, dir.path(), &[], system.clone(), "startup").await;
        assert_eq!(built.prompt, expected);
        let all = CreateSession { tools: builtin_tool_names(ToolProfile::Generic), ..spec.clone() };
        let built = own_runtime(&link, "s", &all, dir.path(), &[], system.clone(), "startup").await;
        assert_eq!(built.prompt, expected, "keeping the tools preserves every prompt byte");

        let limited = CreateSession { tools: vec!["read".into()], ..spec };
        let built = own_runtime(&link, "s", &limited, dir.path(), &[], system, "startup").await;
        for absent in ["You have tools to", "- Use the dedicated tools", "remember", "forget", "# Secrets", "# Skills", "# Task cards"] {
            assert!(!built.prompt.contains(absent), "{absent}: {}", built.prompt);
        }
        assert!(built.prompt.contains("[n1] (all) A useful note") && built.prompt.contains("Known facts."));
    }

    #[tokio::test]
    async fn memory_guidance_keeps_only_the_available_memory_tools() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["remember", "forget"] {
            let spec = CreateSession { tools: vec![name.into()], ..spec() };
            let built = build_tools(&spec, dir.path(), ToolProfile::Generic, &[], &[]).await;
            let prompt = context_sections(&spec, &[], &built.tools);
            assert_eq!(prompt.contains("remember"), name == "remember");
            assert_eq!(prompt.contains("forget"), name == "forget");
            assert!(prompt.contains("A useful note"));
        }
    }

    #[tokio::test]
    async fn tool_lists_filter_tool_guidance_in_a_real_memex_brief() {
        let dir = tempfile::tempdir().unwrap();
        let head = c6_head::start(c6_head::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        c6_head::memex::call_in(
            &head.state,
            "home",
            "memex.knowledge.record",
            serde_json::json!({
                "claim": "CI runs on Argo", "about": ["ci:argo"], "status": "inferred"
            }),
        )
        .await
        .unwrap();
        let labels = vec!["ci:argo".into()];
        let brief = c6_testkit::eventually("a brief with the fact", async || {
            let brief = c6_head::memex::brief(&head.state, "home", &labels, "").await;
            brief.contains("CI runs on Argo").then_some(brief)
        })
        .await;
        assert!(brief.contains(c6_api::prompts::BRIEF_KNOWLEDGE) && brief.contains("memex_get fetches one in full"));
        let mut spec = CreateSession { memex_brief: brief.clone(), ..spec() };
        for names in [
            vec![],
            vec!["Read"],
            vec!["MemexGet"],
            vec!["MemexSearch"],
            vec!["MemexGet", "MemexSearch"],
            vec!["KnowledgeRecord", "KnowledgeVerify"],
            vec!["KnowledgeRecord", "KnowledgeVerify", "TodoAdd"],
            vec!["MemexGet", "MemexSearch", "KnowledgeRecord", "KnowledgeVerify", "TodoAdd"],
        ] {
            spec.tools = names.iter().map(|name| (*name).to_owned()).collect();
            let built = build_tools(&spec, dir.path(), ToolProfile::Generic, &labels, &[]).await;
            let prompt = context_sections(&spec, &labels, &built.tools);
            let lookup = names.is_empty() || (names.contains(&"MemexGet") && names.contains(&"MemexSearch"));
            let record = names.is_empty() || ["KnowledgeRecord", "KnowledgeVerify", "TodoAdd"].iter().all(|name| names.contains(name));
            for name in ["memex_get", "memex_search"] {
                assert_eq!(prompt.contains(name), lookup, "{names:?}: {prompt}");
            }
            for name in ["knowledge_record", "knowledge_verify", "todo_add"] {
                assert_eq!(prompt.contains(name), record, "{names:?}: {prompt}");
            }
            assert!(prompt.contains("CI runs on Argo"));
            if lookup && record {
                assert!(prompt.contains(&brief), "keeping the tools preserves the brief's bytes");
            }
        }
        spec.tools = vec!["Read".into()];
        spec.memex_brief = format!("Custom instructions: use memex_get if needed.\n\n{brief}");
        let built = build_tools(&spec, dir.path(), ToolProfile::Generic, &labels, &[]).await;
        let prompt = context_sections(&spec, &labels, &built.tools);
        assert!(prompt.contains("Custom instructions: use memex_get if needed."));
        assert_eq!(prompt.matches("memex_get").count(), 1);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tool_lists_omit_stock_guidance_for_partly_available_tool_groups() {
        let dir = tempfile::tempdir().unwrap();
        let spec = CreateSession { memex_brief: format!("{}\n\nA useful fact.", c6_api::prompts::BRIEF_KNOWLEDGE), ..spec() };
        let base = format!("{}\n\n{}", spec.base_prompt, c6_head::prompts::SIDE_CHAT_NOTE);
        let system = build_system_prompt(&base, dir.path(), "2026-10-30");
        let link = HeadLink::default();
        for tool in ["read", "task_propose", "task_withdraw"] {
            let limited = CreateSession { tools: vec![tool.into()], ..spec.clone() };
            let built = own_runtime(&link, "s", &limited, dir.path(), &[], system.clone(), "startup").await;
            for absent in ["knowledge_record", "knowledge_verify", "todo_add", "run read-only commands", "# Task cards"] {
                assert!(!built.prompt.contains(absent), "{absent}: {}", built.prompt);
            }
            assert!(built.prompt.contains("A useful fact.") && built.prompt.contains("do not change files unless the user asks you to."));
        }
        let all = CreateSession { tools: builtin_tool_names(ToolProfile::Generic), ..spec };
        let built = own_runtime(&link, "s", &all, dir.path(), &[], system.clone(), "startup").await;
        assert!(built.prompt.starts_with(&system) && built.prompt.contains(c6_api::prompts::BRIEF_KNOWLEDGE));
        assert!(built.prompt.contains("# Task cards"));
    }

    // Representative pre-28 fields, with no tool-list field. Prost uses
    // the same unknown-field decoding for gRPC and the journal's blobs.
    #[derive(Clone, PartialEq, prost::Message)]
    struct OlderSpec {
        #[prost(message, optional, tag = "1")]
        model: Option<ModelSpec>,
        #[prost(string, tag = "2")]
        cwd: String,
        #[prost(string, tag = "3")]
        base_prompt: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct OlderRestore {
        #[prost(message, optional, tag = "1")]
        spec: Option<OlderSpec>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct OlderCommand {
        #[prost(message, optional, tag = "10")]
        create: Option<OlderSpec>,
        #[prost(message, optional, tag = "13")]
        restore: Option<OlderRestore>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct OlderHeadMsg {
        #[prost(message, optional, tag = "4")]
        command: Option<OlderCommand>,
    }

    #[tokio::test]
    async fn older_workers_ignore_tool_lists_on_create_restore_and_journal_load() {
        let dir = tempfile::tempdir().unwrap();
        let spec = CreateSession { cwd: dir.path().display().to_string(), tools: vec!["read".into()], ..spec() };
        for kind in [
            session_command::Kind::Create(spec.clone()),
            session_command::Kind::Restore(c6_proto::Restore { spec: Some(spec.clone()), events: Vec::new() }),
        ] {
            let wire = c6_proto::HeadMsg {
                msg: Some(c6_proto::head_msg::Msg::Command(SessionCommand { kind: Some(kind), ..Default::default() })),
            }
            .encode_to_vec();
            let old = OlderHeadMsg::decode(wire.as_slice()).unwrap().command.unwrap();
            let old = old.create.or_else(|| old.restore.unwrap().spec).unwrap();
            assert_eq!(old.cwd, spec.cwd);
            let decoded = CreateSession::decode(old.encode_to_vec().as_slice()).unwrap();
            assert!(decoded.tools.is_empty(), "the older schema discards field 28");
            let built = build_tools(&decoded, dir.path(), ToolProfile::Generic, &[], &[]).await;
            assert!(has_tool(&built.tools, "write") && has_tool(&built.tools, "task"), "an older worker still sends all tools");
        }

        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let started = SessionEvent { seq: 1, kind: Some(session_event::Kind::Started(SessionStarted::default())), ..Default::default() };
        journal.create_session("new", &spec, &started).await.unwrap();
        journal.import("restored", &spec, &[started]).await.unwrap();
        let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("journal.db").display())).await.unwrap();
        for id in ["new", "restored"] {
            let blob: Vec<u8> = sqlx::query_scalar("SELECT spec FROM sessions WHERE id = ?").bind(id).fetch_one(&db).await.unwrap();
            assert_eq!(CreateSession::decode(blob.as_slice()).unwrap().tools, spec.tools);
            let old = OlderSpec::decode(blob.as_slice()).unwrap();
            assert_eq!(old.cwd, spec.cwd);
            assert!(CreateSession::decode(old.encode_to_vec().as_slice()).unwrap().tools.is_empty());
        }
        db.close().await;
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn subagents_leave_out_the_task_cards_section() {
        let p = format!("Base.{TASK_CARDS_HEADING}Propose cards.\n\n# Skills\n\nOne.");
        assert_eq!(without_task_cards(&p), "Base.\n\n# Skills\n\nOne.");
        assert_eq!(without_task_cards(&format!("Base.{TASK_CARDS_HEADING}Last.")), "Base.");
        assert_eq!(without_task_cards("Base."), "Base.");
    }

    #[test]
    fn normalizes_remotes() {
        for url in [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app",
            "https://user@GitHub.com/acme/app.git/",
            "ssh://git@github.com:22/acme/app.git",
        ] {
            assert_eq!(normalize_remote(url), "github.com/acme/app", "{url}");
        }
        assert_eq!(normalize_remote("/srv/git/app.git"), "/srv/git/app");
    }

    #[tokio::test]
    async fn finds_the_repo_remote() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(repo_slug(d).await, "", "not a repository");
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(d).args(args).output().unwrap();
        git(&["init", "-q"]);
        assert_eq!(repo_slug(d).await, "", "no remote");
        git(&["remote", "add", "upstream", "git@example.com:team/tool.git"]);
        assert_eq!(repo_slug(d).await, "example.com/team/tool");
        git(&["remote", "add", "origin", "https://github.com/acme/app.git"]);
        assert_eq!(repo_slug(d).await, "github.com/acme/app", "origin wins");
    }

    #[tokio::test]
    async fn stops_orphaned_background_commands() {
        use std::os::unix::process::CommandExt;
        struct Group(std::process::Child);
        impl Drop for Group {
            fn drop(&mut self) {
                // Killing only the shell leaves its sleep child holding the
                // test's stdout/stderr open. Clean up even on assertion failure.
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.0.id() as i32), nix::sys::signal::Signal::SIGKILL);
                let _ = self.0.wait();
            }
        }
        let spawn = |cmd: &str| Group(std::process::Command::new("/bin/sh").args(["-c", cmd]).process_group(0).spawn().unwrap());
        let mut orphan = spawn("sleep 77; echo orphan-test");
        let mut other = spawn("sleep 78; echo other-test");
        let pid = |c: &Group| c.0.id() as i32;
        // A different command under that pid is left alone.
        stop_orphan(pid(&other), "   ");
        stop_orphan(pid(&other), "sleep 77; echo orphan-test");
        stop_orphan(pid(&orphan), "sleep 77; echo orphan-test");
        c6_testkit::eventually("the orphan to stop", async || orphan.0.try_wait().unwrap()).await;
        assert!(other.0.try_wait().unwrap().is_none(), "another command was stopped");
    }
}

/// Switching between Claude Code and carbon6's own loop (docs/architecture.md,
/// "Model switching"): what each runtime gets from the log.
#[cfg(test)]
mod claude_code_tools_tests {
    use super::*;

    /// Automatic gives every tool the session has; a list chosen by hand
    /// (or by an older head) gives those; the core set defaults to the
    /// tools agents use most, and every core default is a tool on offer.
    #[test]
    fn the_choice_is_automatic_unless_made_by_hand() {
        let auto = CreateSession {
            claude_code: Some(c6_proto::ClaudeCodeToolChoice { automatic: true, ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(claude_code_choice(&auto), None);
        let custom = CreateSession {
            claude_code: Some(c6_proto::ClaudeCodeToolChoice { tools: vec!["doc".into()], ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(claude_code_choice(&custom), Some(vec!["doc".into()]));
        let older = CreateSession { claude_code_tools: vec!["Task".into()], claude_code_tools_chosen: true, ..Default::default() };
        assert_eq!(claude_code_choice(&older), Some(vec!["Task".into()]));
        let oldest = CreateSession::default();
        assert_eq!(claude_code_choice(&oldest).unwrap().len(), CLAUDE_CODE_DEFAULT_TOOLS.len());
        assert_eq!(claude_code_core(&auto), CLAUDE_CODE_CORE_TOOLS.iter().map(|t| t.to_string()).collect::<Vec<_>>());
        let none_up_front = CreateSession {
            claude_code: Some(c6_proto::ClaudeCodeToolChoice { automatic: true, core_chosen: true, ..Default::default() }),
            ..Default::default()
        };
        assert!(claude_code_core(&none_up_front).is_empty());
        let names: Vec<String> = claude_code_tool_names().into_iter().map(|(n, _)| n).collect();
        for c in CLAUDE_CODE_CORE_TOOLS {
            assert!(names.iter().any(|n| n == c), "{c} is a tool Claude Code can get");
        }
    }

    /// What goes into Claude Code's system prompt: the editable note, then
    /// carbon6's guidance for the tools it got (task cards here).
    #[test]
    fn the_system_prompt_carries_the_note_and_the_guidance() {
        let spec = CreateSession {
            prompts_json: r#"{"session.task_cards":"Propose useful work.","session.claude_code":"You are in carbon6."}"#.into(),
            ..Default::default()
        };
        let tools: Vec<Arc<dyn Tool>> = claude_code_extras(&spec, &[], None)
            .into_iter()
            .filter(|t| matches!(t.spec().name.as_str(), "TaskPropose" | "TaskWithdraw"))
            .collect();
        let p = claude_code_system_prompt(&spec, &[], &tools);
        assert!(p.starts_with("You are in carbon6.\n\n"), "{p}");
        assert!(p.contains("Propose useful work."), "{p}");
        let default = claude_code_system_prompt(&CreateSession::default(), &[], &[]);
        assert!(default.starts_with(c6_agent::CLAUDE_CODE_NOTE));
    }

    /// Claude Code is offered carbon6's own tools, not copies of those it
    /// keeps, with the defaults first; carbon6's web fetch, as its own is off.
    #[test]
    fn claude_code_is_offered_carbon6s_own_tools() {
        let names: Vec<String> = claude_code_tool_names().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            &names[..CLAUDE_CODE_DEFAULT_TOOLS.len()],
            ["Task", "SendMessage", "TaskOutput", "TaskStop", "doc", "MessageSession", "ListSessions"]
        );
        for offered in
            ["PublishFile", "Remember", "Forget", "TodoAdd", "TaskPropose", "TaskWithdraw", "MemexSearch", "MailSend", "WebFetch"]
        {
            assert!(names.iter().any(|n| n == offered), "{offered} in {names:?}");
        }
        for own in ["Bash", "Read", "Write", "Edit", "Grep", "Glob", "TodoWrite", "Skill", "BashOutput", "KillShell", "Doc"] {
            assert!(!names.iter().any(|n| n == own), "{own} is Claude Code's own");
        }
    }

    /// carbon6's settings for Claude Code: the extra settings (by default,
    /// no co-author lines), never the keys carbon6 sets, then the head's hooks.
    #[test]
    fn claude_code_settings_take_the_extras_but_not_carbon6s_keys() {
        let settings = |extra: &str, hooks: Option<&str>| -> serde_json::Value {
            let spec = CreateSession {
                claude_code: Some(c6_proto::ClaudeCodeToolChoice { settings_json: extra.into(), ..Default::default() }),
                ..Default::default()
            };
            serde_json::from_str(&claude_code_settings(&spec, hooks)).unwrap()
        };
        let default = serde_json::json!({"includeCoAuthoredBy": false, "gitAttribution": false});
        assert_eq!(settings("", None), default);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&claude_code_settings(&CreateSession::default(), None)).unwrap(), default);
        assert_eq!(settings("not json", None), default, "unreadable: the default");
        assert_eq!(settings("{}", None), serde_json::json!({}), "set to nothing: nothing");
        let hooks = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"head"}]}]}}"#;
        let v = settings(
            r#"{"effortLevel":"high","hooks":{"PreToolUse":[]},"env":{"ENABLE_CLAUDEAI_MCP_SERVERS":"true"},"enabledPlugins":{"x@y":true},"disableAllHooks":true}"#,
            Some(hooks),
        );
        assert_eq!(v["effortLevel"], "high", "passes through");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "head", "the head's hooks, not the setting's");
        assert!(v["hooks"].get("PreToolUse").is_none() && v.get("env").is_none() && v.get("enabledPlugins").is_none(), "{v}");
        assert!(v.get("disableAllHooks").is_none(), "{v}");
    }

    /// Claude Code keeps only its coding tools, those a routing rule's
    /// list allows (its tool search always), unless Settings says all.
    #[test]
    fn claude_code_keeps_its_coding_tools_as_the_route_allows() {
        let native = |tools: &[&str], all: bool| {
            claude_code_native(&CreateSession {
                tools: tools.iter().map(|t| (*t).to_owned()).collect(),
                claude_code: Some(c6_proto::ClaudeCodeToolChoice { automatic: true, native_all: all, ..Default::default() }),
                ..Default::default()
            })
        };
        assert_eq!(native(&[], false).unwrap(), CLAUDE_CODE_NATIVE_TOOLS);
        assert_eq!(claude_code_native(&CreateSession::default()).unwrap(), CLAUDE_CODE_NATIVE_TOOLS, "an older head: locked down");
        assert_eq!(native(&["read_file", "web_fetch"], false).unwrap(), ["Read", "ToolSearch"], "no shell: no Bash");
        assert_eq!(native(&["shell", "apply_patch"], false).unwrap(), ["Bash", "Edit", "ToolSearch"]);
        assert_eq!(native(&[], true), None);
        assert_eq!(native(&["read_file"], true), None, "all is all");
        // A subagent's named agent keeps it to its list, even with all.
        let reader = CreateSession {
            delegation: Some(c6_proto::Delegation { agent_json: r#"{"name":"r","tools":["Read","Grep"]}"#.into(), ..Default::default() }),
            claude_code: Some(c6_proto::ClaudeCodeToolChoice { automatic: true, native_all: true, ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(claude_code_native(&reader).unwrap(), ["Read", "ToolSearch"]);
    }

    /// With Claude Code's web tools off, carbon6's come over MCP: fetch
    /// always, search where a provider is set up and the session has its
    /// key; with all its tools, neither.
    #[test]
    fn carbon6s_web_tools_replace_claude_codes() {
        let names = |spec: &CreateSession| -> Vec<String> {
            claude_code_extras(spec, &[], None).iter().map(|t| t.spec().name).filter(|n| n.starts_with("Web")).collect()
        };
        let brave = r#"{"provider":"brave","secret":"BRAVE_KEY"}"#;
        let spec = |search: &str, secrets: &str, all: bool| CreateSession {
            web_search_json: search.into(),
            secrets_json: secrets.into(),
            claude_code: Some(c6_proto::ClaudeCodeToolChoice { automatic: true, native_all: all, ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(names(&spec("", "", false)), ["WebFetch"]);
        assert_eq!(names(&spec(brave, "", false)), ["WebFetch"], "no key: no search");
        assert_eq!(names(&spec(brave, r#"[{"name":"BRAVE_KEY"}]"#, false)), ["WebFetch", "WebSearch"]);
        assert_eq!(names(&spec(r#"{"provider":"searxng","url":"http://s"}"#, "", false)), ["WebFetch", "WebSearch"], "no key needed");
        assert!(names(&spec(brave, r#"[{"name":"BRAVE_KEY"}]"#, true)).is_empty(), "Claude Code has its own");
    }

    /// The guidance carbon6's loop gives in its system prompt goes with the
    /// tools that need it, and only with them.
    #[test]
    fn guidance_follows_the_tools() {
        let spec = CreateSession {
            memories_json: r#"[{"id":"n1","scope":"","text":"The build runs on thalassa.","updated_at":1}]"#.into(),
            prompts_json: r#"{"session.task_cards":"Propose useful work."}"#.into(),
            ..Default::default()
        };
        let pick = |names: &[&str]| -> Vec<Arc<dyn Tool>> {
            let chosen: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
            claude_code_extras(&spec, &[], None).into_iter().filter(|t| is_chosen(&chosen, &t.spec().name)).collect()
        };
        let none = claude_code_guidance(&spec, &[], &pick(&[]));
        assert!(!none.contains("# Memory") && !none.contains("# Task cards"), "{none}");
        let all = claude_code_guidance(&spec, &[], &pick(&["remember", "forget", "task_propose", "task_withdraw"]));
        assert!(
            all.contains("# Memory") && all.contains("The build runs on thalassa.") && all.contains("save it with the remember"),
            "{all}"
        );
        assert!(all.contains("# Task cards\n\nPropose useful work."), "{all}");
    }
}

#[cfg(test)]
mod notice_tests {
    use prost::Message;

    use super::*;

    fn delivered_local_report_inputs<'a>(child: &str, events: &'a [SessionEvent]) -> Vec<&'a c6_proto::UserMessage> {
        let pending = events
            .iter()
            .filter_map(|event| match &event.kind {
                Some(session_event::Kind::User(input)) => Some(input.command_id.clone()),
                _ => None,
            })
            .collect();
        super::delivered_local_report_inputs(child, events, &pending)
    }

    #[test]
    fn background_liveness_distinguishes_agent_waits_from_commands() {
        use c6_tools::{TaskInfo, TaskKind};
        let task = |kind, running, notify| TaskInfo { id: String::new(), kind, description: String::new(), running, notify };
        assert_eq!(background_liveness_counts(&[task(TaskKind::Agent, true, true)]), (1, 0));
        assert_eq!(background_liveness_counts(&[task(TaskKind::Shell, true, true)]), (1, 1));
        assert_eq!(
            background_liveness_counts(&[
                task(TaskKind::Agent, true, true),
                task(TaskKind::Shell, true, true),
                task(TaskKind::Shell, true, false),
                task(TaskKind::Shell, false, true),
                task(TaskKind::ClaudeCode, true, false),
            ]),
            (3, 2)
        );
    }

    #[test]
    fn subagent_liveness_reports_remain_compatible_with_older_heads() {
        use c6_proto::{SessionActivityKind as Activity, SessionActorState as State};
        for state in [State::Running, State::Unresponsive] {
            assert_eq!(liveness_activity(state, Activity::Subagent as i32), (Activity::Tool as i32, true));
            assert_eq!(liveness_activity(state, Activity::Tool as i32), (Activity::Tool as i32, false));
            assert_eq!(liveness_activity(state, Activity::Model as i32), (Activity::Model as i32, false));
        }
        assert_eq!(liveness_activity(State::Idle, Activity::Subagent as i32), (Activity::Unspecified as i32, false));
    }

    #[test]
    fn local_report_recovery_keeps_earlier_turns_and_automatic_resumptions() {
        let mut events = vec![
            ev(1, 1, session_event::Kind::Started(Default::default())),
            ev(2, 2, session_event::Kind::SubagentReported(local_report_boundary("child"))),
            ev(
                3,
                3,
                session_event::Kind::User(c6_proto::UserMessage {
                    command_id: "steer".into(),
                    delegated_by: "call".into(),
                    ..Default::default()
                }),
            ),
            ev(4, 4, session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: TurnEndReason::Error as i32 })),
            ev(
                5,
                5,
                session_event::Kind::User(c6_proto::UserMessage {
                    command_id: "task:bg1:finished".into(),
                    command: "task-notification".into(),
                    ..Default::default()
                }),
            ),
            ev(6, 6, session_event::Kind::TurnEnded(TurnEnded { turn: 2, reason: TurnEndReason::Completed as i32 })),
        ];
        let pending = pending_local_reports("child", &events);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].2[0].command_id, "steer");
        assert_eq!(pending[1].2[0].command, "task-notification");
        let id = pending[1].0.clone();
        events.push(ev(7, 7, session_event::Kind::SubagentReported(c6_proto::SubagentReported { command_id: id, ..Default::default() })));
        let pending = pending_local_reports("child", &events);
        assert_eq!(pending.len(), 1, "a later receipt must not erase an earlier pending turn");
        assert_eq!(pending[0].0, "subagent:local:child:4");
        assert!(pending_local_reports("legacy-child", &events).is_empty(), "no replay of pre-tracking history");
    }

    #[test]
    fn delivered_local_report_inputs_keep_independent_windows_separate() {
        use session_event::Kind;
        let input = |id: &str| {
            Kind::User(c6_proto::UserMessage { command_id: id.into(), delegated_by: "reused-call".into(), ..Default::default() })
        };
        let mut events: Vec<_> = [
            Kind::SubagentReported(local_report_boundary("child")),
            input("earlier"),
            Kind::TurnEnded(Default::default()),
            input("later"),
            Kind::TurnEnded(Default::default()),
            Kind::SubagentReported(c6_proto::SubagentReported {
                command_id: "subagent:local:child:5".into(),
                child_session_id: "child".into(),
                status: "completed".into(),
                ..Default::default()
            }),
            input("next"),
            Kind::TurnEnded(Default::default()),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| ev(i as u64 + 1, 0, kind))
        .collect();
        let ids = |events: &[SessionEvent]| {
            delivered_local_report_inputs("child", events).iter().map(|u| u.command_id.clone()).collect::<Vec<_>>()
        };
        assert_eq!(ids(&events), ["later"]);
        assert!(pending_local_reports("child", &events).iter().any(|(id, _, _)| id == "subagent:local:child:3"));
        events.push(ev(
            9,
            0,
            Kind::SubagentReported(c6_proto::SubagentReported {
                command_id: "subagent:local:child:3".into(),
                child_session_id: "child".into(),
                status: "completed".into(),
                ..Default::default()
            }),
        ));
        assert_eq!(ids(&events), ["later", "earlier"]);
        let pending = HashSet::from(["earlier".into()]);
        assert_eq!(
            super::delivered_local_report_inputs("child", &events, &pending).iter().map(|u| u.command_id.as_str()).collect::<Vec<_>>(),
            ["earlier"]
        );
        let pending = HashSet::from(["next".into()]);
        assert!(super::delivered_local_report_inputs("child", &events, &pending).is_empty());
        assert!(super::delivered_local_report_inputs("child", &events, &HashSet::new()).is_empty());
    }

    #[test]
    fn delivered_local_report_inputs_include_counted_multi_turn_work() {
        use session_event::Kind;
        let input =
            |id: &str| Kind::User(c6_proto::UserMessage { command_id: id.into(), delegated_by: "call".into(), ..Default::default() });
        let task = |state: &str| {
            Kind::BackgroundTask(BackgroundTask { task_id: "bg".into(), notify: true, state: state.into(), ..Default::default() })
        };
        let events: Vec<_> = [
            Kind::SubagentReported(local_report_boundary("child")),
            input("original"),
            Kind::TurnStarted(Default::default()),
            Kind::TurnEnded(Default::default()),
            task("started"), // Durable start can follow the first turn end.
            input("continuation"),
            Kind::TurnStarted(Default::default()),
            Kind::TurnEnded(Default::default()),
            task("finished"), // The receipt's end sequence is not the interval's last event.
            Kind::SubagentReported(c6_proto::SubagentReported {
                command_id: "subagent:local:child:8".into(),
                child_session_id: "child".into(),
                status: "completed".into(),
                ..Default::default()
            }),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| ev(i as u64 + 1, 0, kind))
        .collect();
        assert!(delivered_local_report_inputs("child", &events[..9]).is_empty());
        assert_eq!(
            delivered_local_report_inputs("child", &events).iter().map(|u| u.command_id.as_str()).collect::<Vec<_>>(),
            ["original", "continuation"]
        );
    }

    #[test]
    fn delivered_local_report_inputs_exclude_invalid_receipts_and_unconsumed_inputs() {
        use session_event::Kind;
        let input = |id: &str, delegated_by: &str, blocked: &str| {
            Kind::User(c6_proto::UserMessage {
                command_id: id.into(),
                delegated_by: delegated_by.into(),
                blocked: blocked.into(),
                ..Default::default()
            })
        };
        let base: Vec<_> = [
            Kind::SubagentReported(local_report_boundary("child")),
            input("delegated", "call", ""),
            input("blocked", "call", "blocked"),
            input("ordinary", "", ""),
            Kind::TurnEnded(Default::default()),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| ev(i as u64 + 1, 0, kind))
        .collect();
        for (command_id, child_session_id, status) in [
            ("subagent:local:child:tracking", "child", "tracking"),
            ("subagent:local:other:5", "child", "completed"),
            ("subagent:local:child:5", "other", "completed"),
            ("subagent:local:child:5", "child", "waiting"),
            ("subagent:local:child:6", "child", "completed"),
            ("subagent:local:child:99", "child", "completed"),
            ("subagent:local:child:2", "child", "completed"),
        ] {
            let mut events = base.clone();
            events.push(ev(
                6,
                0,
                Kind::SubagentReported(c6_proto::SubagentReported {
                    command_id: command_id.into(),
                    child_session_id: child_session_id.into(),
                    status: status.into(),
                    ..Default::default()
                }),
            ));
            assert!(
                delivered_local_report_inputs("child", &events).is_empty(),
                "invalid receipt {command_id} / {child_session_id} / {status}"
            );
        }
        let mut events = base;
        events.push(ev(
            6,
            0,
            Kind::SubagentReported(c6_proto::SubagentReported {
                command_id: "subagent:local:child:5".into(),
                child_session_id: "child".into(),
                status: "completed".into(),
                ..Default::default()
            }),
        ));
        assert_eq!(
            delivered_local_report_inputs("child", &events).iter().map(|u| u.command_id.as_str()).collect::<Vec<_>>(),
            ["delegated"]
        );
        assert!(delivered_local_report_inputs("legacy-child", &events).is_empty());
    }

    #[test]
    fn a_waiter_needs_a_durable_result_not_a_lost_or_background_placeholder() {
        let result = |text: &str| {
            ev(
                1,
                1,
                session_event::Kind::ToolResult(c6_proto::ToolResult { call_id: "call".into(), output: text.into(), ..Default::default() }),
            )
        };
        assert!(!report_recorded(&[], "call", "child", "completed"));
        assert!(!report_recorded(&[result("<subagent id=\"child\" status=\"lost\"/>")], "call", "child", "completed"));
        assert!(report_recorded(&[result("The full report.\n[subagent session child]")], "call", "child", "completed"));
        let mut events =
            vec![result("Continuing the subagent in the background as bg2. Its report will arrive as a notice when it finishes.")];
        assert!(!report_recorded(&events, "call", "child", "completed"));
        events.push(ev(
            2,
            2,
            session_event::Kind::NoticeQueued(c6_proto::NoticeQueued { command_id: "task:bg2:lost".into(), ..Default::default() }),
        ));
        assert!(!report_recorded(&events, "call", "child", "completed"));
        assert!(!report_recorded(&events, "call", "child", "lost"), "a notice without an owner is not a receipt");
        events.push(ev(
            3,
            3,
            session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned {
                call_id: "call".into(),
                child_session_id: "child".into(),
                background_task_id: "bg2".into(),
                ..Default::default()
            }),
        ));
        assert!(!report_recorded(&events, "call", "child", "completed"));
        assert!(report_recorded(&events, "call", "child", "lost"));
        events.push(ev(
            4,
            4,
            session_event::Kind::NoticeQueued(c6_proto::NoticeQueued { command_id: "task:bg2:finished".into(), ..Default::default() }),
        ));
        assert!(report_recorded(&events, "call", "child", "completed"));
        events[0] = result("Continuing the subagent in the background as bg2. This is prose, not a placeholder.");
        assert!(!report_recorded(&events, "call", "child", "completed"));
    }

    #[test]
    fn failed_background_handoff_waits_for_its_exact_notice_without_a_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Arc::new(SessionTools::new(dir.path().into()));
        let (task_tx, mut task_rx) = mpsc::unbounded_channel();
        tools.background.set_sink(Some(task_tx));
        let (older_task, _) = tools.background.start_agent("earlier run", true);
        let (task, _) = tools.background.start_agent("failing run", true);
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = Handle {
            tx,
            recovery: Arc::default(),
            tools: tools.clone(),
            running: Arc::default(),
            claude_code: Arc::default(),
            inbox: Arc::default(),
        };
        let link = |seq, task: &str| {
            ev(
                seq,
                seq as i64,
                session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned {
                    call_id: "call".into(),
                    child_session_id: "child".into(),
                    background_task_id: task.into(),
                    ..Default::default()
                }),
            )
        };
        let notice = |seq, task: &str, state: &str| {
            ev(
                seq,
                seq as i64,
                session_event::Kind::NoticeQueued(c6_proto::NoticeQueued {
                    command_id: format!("task:{task}:{state}"),
                    ..Default::default()
                }),
            )
        };
        let call = LocalReportCall::of(&c6_proto::UserMessage {
            command_id: format!("delegation:unique:{task}"),
            delegated_by: "call".into(),
            ..Default::default()
        })
        .unwrap();
        let inline = InlineChildReport { child: "child".into(), calls: vec![call], status: "failed", after: 0 };
        let mut parent = LocalReportParent {
            events: vec![link(1, &older_task), notice(2, &older_task, "finished"), link(3, &task)],
            ..Default::default()
        };
        assert!(!inline.recorded(&parent.events), "a reused call id and the same child do not make the old run a receipt");

        tools.background.finish_agent(&task, Err("failed".into()));
        let _ = task_rx.try_recv().unwrap(); // older start
        let _ = task_rx.try_recv().unwrap(); // current start
        let completed = task_rx.try_recv().unwrap(); // do not hand it to the actor yet
        assert_eq!((completed.id, completed.exit_code), (task.clone(), Some(1)));
        assert!(!tools.background.snapshot(&task).unwrap().running);
        assert!(inline.still_running(&handle, &parent), "a finished snapshot still has an undelivered callback");
        drop(rx);
        assert!(!inline.still_running(&handle, &parent), "a dead actor cannot deliver its retained callback");

        parent.events.push(notice(4, &task, "lost"));
        assert!(!inline.recorded(&parent.events), "a loss placeholder did not deliver this failure");
        parent.events.push(notice(5, &task, "finished")); // nonzero exit, so this is the failure notice
        assert!(inline.recorded(&parent.events), "link and notice suffice before the CLI result arrives");
        parent.events.push(ev(
            6,
            6,
            session_event::Kind::ToolResult(c6_proto::ToolResult {
                call_id: "call".into(),
                output: "started".into(),
                ..Default::default()
            }),
        ));
        assert!(inline.recorded(&parent.events), "MCP can log a wrapped result instead of the launch placeholder");
        parent.events.remove(2);
        assert!(!inline.recorded(&parent.events), "the notice cannot acknowledge a child without its exact link");
    }

    #[test]
    fn linked_receipts_survive_call_id_reuse_without_acknowledging_new_calls() {
        let invoke = |seq, name: &str| {
            ev(
                seq,
                seq as i64,
                session_event::Kind::Assistant(c6_proto::AssistantMessage {
                    blocks: vec![c6_proto::ContentBlock {
                        block: Some(c6_proto::content_block::Block::ToolUse(c6_proto::ToolUse {
                            id: "call".into(),
                            name: name.into(),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            )
        };
        let spawn = |seq, child: &str, task: &str| {
            ev(
                seq,
                seq as i64,
                session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned {
                    call_id: "call".into(),
                    child_session_id: child.into(),
                    background_task_id: task.into(),
                    ..Default::default()
                }),
            )
        };
        let result = |seq, output: &str| {
            ev(
                seq,
                seq as i64,
                session_event::Kind::ToolResult(c6_proto::ToolResult {
                    call_id: "call".into(),
                    output: output.into(),
                    ..Default::default()
                }),
            )
        };
        let recorded = |events: &[SessionEvent], child| report_recorded(events, "call", child, "completed");
        let mut events = vec![invoke(1, "task"), spawn(2, "a", ""), result(3, "A done\n[subagent session a]"), invoke(4, "Task")];
        assert!(recorded(&events, "a"), "the next Task need not have linked yet");
        events.push(spawn(5, "b", ""));
        assert!(!recorded(&events, "b"));
        events.push(result(6, "B done\n[subagent session b]"));
        assert!(recorded(&events, "a") && recorded(&events, "b"));
        assert_eq!(InlineChildReport::check_after(&[LocalReportCall { id: "call".into(), task: None }], &events, 6), 0);

        events.push(invoke(7, "send_message"));
        assert!(!recorded(&events, "a"), "an unlinked continuation could be a new run of A");
        events.push(result(8, "A continuation done\n[subagent session a]"));
        assert!(recorded(&events, "a"));
        events.push(ev(9, 9, session_event::Kind::Rewound(Rewound { to_seq: 4, ..Default::default() })));
        assert!(recorded(&events, "a") && !recorded(&events, "b"));
        events.push(ev(10, 10, session_event::Kind::Rewound(Rewound { to_seq: 1, ..Default::default() })));
        assert!(!recorded(&events, "a"));

        let started = |task: &str| {
            format!(
                "Started the subagent in the background as {task}. Its report will arrive as a notice when it finishes; continue with other work meanwhile."
            )
        };
        let mut events = vec![
            invoke(1, "task"),
            spawn(2, "a", "bg1"),
            result(3, &started("bg1")),
            invoke(4, "task"),
            spawn(5, "b", "bg2"),
            result(6, &started("bg2")),
        ];
        let notice = |seq, command_id: &str| {
            ev(
                seq,
                seq as i64,
                session_event::Kind::NoticeQueued(c6_proto::NoticeQueued { command_id: command_id.into(), ..Default::default() }),
            )
        };
        events.extend([notice(7, "task:bg2:finished"), notice(8, "task:bg1:lost")]);
        assert!(!recorded(&events, "a") && recorded(&events, "b"));
        events.push(notice(9, "task:bg1:finished"));
        assert!(recorded(&events, "a"));
        events.extend([invoke(10, "task"), result(11, &started("bg3"))]);
        assert!(recorded(&events, "a") && recorded(&events, "b"), "the next Task's missing link cannot hide either receipt");

        // B's background future can link after a foreground continuation
        // of A has started. The delayed link does not belong to A's call.
        let placeholder = "Continuing the subagent in the background as bg2. Its report will arrive as a notice when it finishes.";
        let mut delayed = vec![invoke(1, "send_message"), result(2, placeholder), spawn(4, "b", "bg2"), notice(5, "task:bg2:finished")];
        assert!(!recorded(&delayed, "a") && recorded(&delayed, "b"));
        delayed.insert(2, invoke(3, "send_message"));
        assert!(!recorded(&delayed, "a"), "B's late link and notice cannot settle A's unresolved call");
        delayed.push(result(6, &format!("{placeholder}\n\n[subagent session a]")));
        assert!(recorded(&delayed, "a"), "a real report can begin with the exact placeholder text");
        assert!(recorded(&delayed, "b"), "the older placeholder finds its link across invocation boundaries");

        let quoted = [invoke(1, "send_message"), result(2, "An earlier result said:\n[subagent session a]\n\n[subagent session b]")];
        assert!(!recorded(&quoted, "a") && recorded(&quoted, "b"));

        let mut footer_reuse = vec![
            invoke(1, "task"),
            spawn(2, "a", ""),
            result(3, "A done\n[subagent session a]"),
            invoke(4, "send_message"),
            result(5, "Looks like B\n[subagent session b]"),
        ];
        assert!(recorded(&footer_reuse, "a"), "a successful send to B does not hide A's receipt");
        if let Some(session_event::Kind::ToolResult(r)) = &mut footer_reuse[4].kind {
            r.is_error = true;
        }
        assert!(!recorded(&footer_reuse, "a"), "an error cannot use its output footer to identify a sibling");
        footer_reuse[3] = invoke(4, "bash");
        footer_reuse[4] = result(5, "Looks like B\n[subagent session b]");
        assert!(!recorded(&footer_reuse, "a"), "an unrelated tool cannot use its output footer to identify a sibling");
    }

    #[test]
    fn new_inputs_persist_server_origins_without_rewriting_the_message() {
        for (api_origin, human_origin, from_session_id) in [
            (Some(c6_proto::ApiMessageOrigin { token_name: "build-bot".into() }), false, ""),
            (Some(c6_proto::ApiMessageOrigin::default()), false, ""),
            (Some(c6_proto::ApiMessageOrigin { token_name: "mixed".into() }), true, ""),
            (None, true, ""),
            (None, false, "other-session"),
            (None, false, ""),
        ] {
            let input = PendingInput {
                command_id: "command".into(),
                text: "Original message".into(),
                prompt: "Expanded command".into(),
                api_origin: api_origin.clone(),
                human_origin,
                from_session_id: from_session_id.into(),
                ..Default::default()
            };
            let session_event::Kind::User(message) = input.event() else { panic!("a UserMessage") };
            let message = c6_proto::UserMessage::decode(message.encode_to_vec().as_slice()).unwrap();
            assert_eq!(message.api_origin, api_origin);
            assert_eq!(message.human_origin, human_origin);
            assert_eq!(message.from_session_id, from_session_id);
            assert!(message.origin_recorded, "new unknown inputs must differ from legacy records");
            assert_eq!(message.text, input.text);
            assert_eq!(message.prompt, input.prompt);
        }
        let old = c6_proto::UserMessage::decode(&b"\x12\x07Status?"[..]).unwrap();
        assert!(!old.origin_recorded && !old.human_origin && old.api_origin.is_none(), "historical records stay legacy");
    }

    #[test]
    fn queued_metadata_roundtrips_through_reload_and_the_delivered_user_event() {
        let metadata = crate::subagent::task_report_notice("Review", "child", TurnEndReason::Completed, "# Actual report");
        let queued = c6_proto::NoticeQueued {
            command_id: "subagent:report".into(),
            text: "Original raw attribution <subagent>".into(),
            command: "task-notification".into(),
            task_notice: Some(metadata.clone()),
            ..Default::default()
        };
        let decoded = c6_proto::NoticeQueued::decode(queued.encode_to_vec().as_slice()).unwrap();
        let (_, _, pending) = notices_of(&[ev(1, 1, session_event::Kind::NoticeQueued(decoded))]);
        let (input, _) = &pending[0];
        assert_eq!(input.task_notice, Some(metadata.clone()));
        assert_eq!(input.text, queued.text);
        let session_event::Kind::User(message) = input.event() else { panic!("a UserMessage") };
        let decoded = c6_proto::UserMessage::decode(message.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.task_notice, Some(metadata));
        assert_eq!(decoded.text, queued.text);
        let (_, _, old) = notices_of(&[ev(1, 1, self::queued("old", false))]);
        assert!(old[0].0.task_notice.is_none(), "old logs remain neutral");
    }

    #[test]
    fn background_metadata_uses_the_record_and_output_not_the_raw_notice() {
        let record = BackgroundTask {
            task_id: "raw-id".into(),
            kind: "shell".into(),
            description: "Run checks".into(),
            state: "finished".into(),
            exit_code: 1,
            ..Default::default()
        };
        let notice = background_task_notice(&record, "warning: one failed");
        assert_eq!((notice.kind.as_str(), notice.title.as_str(), notice.status.as_str()), ("command", "Run checks", "failed"));
        assert!(notice.session_id.is_empty());
        assert_eq!(notice.markdown, "```text\nwarning: one failed\n```");
        assert_eq!(background_task_notice(&record, "").markdown, "Command exited with code 1.");
        let mut lost = BackgroundTask { kind: "claude_code".into(), state: "lost".into(), ..record };
        let notice = background_task_notice(&lost, "");
        assert_eq!(notice.kind, "task");
        assert!(notice.markdown.starts_with("Task lost:"));
        assert!(!notice.markdown.contains('<'));
        lost.native_type = "local_bash".into();
        assert_eq!(background_task_notice(&lost, "").kind, "command");
        lost.native_type = "local_agent".into();
        assert_eq!(background_task_notice(&lost, "").kind, "subagent");
    }

    struct Head(String);

    #[async_trait]
    impl crate::link::LocalHead for Head {
        async fn call(&self, _method: &str, _params: serde_json::Value) -> Result<serde_json::Value, String> {
            Err("not needed by this test".into())
        }

        async fn credential(&self, connection_id: &str) -> c6_proto::CredentialGrant {
            c6_proto::CredentialGrant {
                connection_id: connection_id.into(),
                kind: "openai_compat".into(),
                base_url: self.0.clone(),
                ..Default::default()
            }
        }
    }

    fn ev(seq: u64, ts_ms: i64, kind: session_event::Kind) -> SessionEvent {
        SessionEvent { seq, ts_ms, kind: Some(kind) }
    }

    fn queued(id: &str, wake: bool) -> session_event::Kind {
        session_event::Kind::NoticeQueued(c6_proto::NoticeQueued {
            command_id: id.into(),
            text: format!("about {id}"),
            command: "task-notification".into(),
            wake,
            ..Default::default()
        })
    }

    fn user(id: &str) -> session_event::Kind {
        session_event::Kind::User(c6_proto::UserMessage { command_id: id.into(), ..Default::default() })
    }

    fn turn() -> session_event::Kind {
        session_event::Kind::TurnStarted(TurnStarted::default())
    }

    /// A reload delivers the notices no turn took and retains notice provenance.
    #[test]
    fn a_log_says_which_notices_are_pending_and_which_turns_they_started() {
        let events = [
            ev(1, 100, queued("task:bg1:finished", true)),
            ev(2, 110, user("task:bg1:finished")),
            ev(3, 120, turn()),
            ev(4, 200, queued("subagent:s", false)),
            ev(5, 210, user("you-1")),
            ev(6, 220, turn()),
            ev(7, 300, queued("task:bg2:finished", true)),
        ];
        let (auto_turns, mut noticed, pending) = notices_of(&events);
        assert_eq!(auto_turns, [120], "only the turn the notice alone started");
        noticed.sort();
        assert_eq!(noticed, ["subagent:s", "task:bg1:finished", "task:bg2:finished"]);
        let pending: Vec<(&str, &str, bool)> = pending.iter().map(|(i, w)| (i.command_id.as_str(), i.text.as_str(), *w)).collect();
        assert_eq!(pending, [("subagent:s", "about subagent:s", false), ("task:bg2:finished", "about task:bg2:finished", true)]);
    }

    #[test]
    fn notice_replay_keeps_first_queue_order_and_latest_payload() {
        let update = |id: &str, text: &str| {
            session_event::Kind::NoticeQueued(c6_proto::NoticeQueued {
                command_id: id.into(),
                text: text.into(),
                wake: true,
                ..Default::default()
            })
        };
        let events = [
            ev(1, 100, queued("a", false)),
            ev(2, 200, queued("b", false)),
            ev(3, 300, queued("already-taken", false)),
            ev(4, 400, update("b", "latest b")),
            ev(5, 500, user("already-taken")),
            ev(6, 600, update("a", "latest a")),
            ev(7, 700, update("already-taken", "never replayed")),
        ];
        let (_, _, pending) = notices_of(&events);
        let pending: Vec<_> = pending.iter().map(|(input, wake)| (input.command_id.as_str(), input.text.as_str(), *wake)).collect();
        assert_eq!(pending, [("a", "latest a", true), ("b", "latest b", true)]);
    }

    const QUIET: std::time::Duration = std::time::Duration::from_secs(3);
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
    const SECOND: std::time::Duration = std::time::Duration::from_secs(1);
    // Tokio rounds timer deadlines up to the next clock tick.
    const TICK: std::time::Duration = std::time::Duration::from_millis(1);

    async fn idle_session() -> (tempfile::TempDir, SessionManager, c6_testkit::MockLlm) {
        let dir = tempfile::tempdir().unwrap();
        let mock = c6_testkit::MockLlm::start().await;
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let model =
            ModelSpec { connection_id: "mock".into(), connection_kind: "openai_compat".into(), model: "m".into(), ..Default::default() };
        let cwd = dir.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let spec = CreateSession { model: Some(model.clone()), cwd: cwd.display().to_string(), ..Default::default() };
        let events = [ev(
            1,
            c6_core::now_ms(),
            session_event::Kind::Started(SessionStarted {
                model: Some(model),
                cwd: spec.cwd.clone(),
                tool_profile: "generic".into(),
                ..Default::default()
            }),
        )];
        journal.import("held", &spec, &events).await.unwrap();
        let manager =
            SessionManager::new(journal, Arc::new(HeadLink::local(Arc::new(Head(mock.base_url())))), dir.path().join("worktrees"));
        (dir, manager, mock)
    }

    #[tokio::test]
    async fn settings_refresh_does_not_reject_or_recreate_a_missing_cache() {
        let (_dir, manager, _mock) = idle_session().await;
        manager
            .handle(SessionCommand {
                command_id: "ephemeral-settings".into(),
                session_id: "trimmed".into(),
                kind: Some(session_command::Kind::UpdateNoticeLimits(c6_proto::NoticeLimits::default())),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!manager.journal.has_session("trimmed").await.unwrap());
        assert!(manager.live("trimmed").is_none());
    }

    #[tokio::test]
    async fn unchanged_settings_refresh_does_not_extend_the_journal() {
        let (_dir, manager, _mock) = idle_session().await;
        let actor = manager.actor("held").await.unwrap().unwrap();
        actor.send(ActorMsg::NoticeLimits(c6_proto::NoticeLimits::default())).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        actor.send(ActorMsg::TaskFence(tx)).unwrap();
        c6_testkit::within("the unchanged settings to be handled", rx).await.unwrap();
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert!(!events.iter().any(|e| matches!(&e.kind,
            Some(session_event::Kind::Notice(n)) if n.title == "Notice settings refreshed")));
    }

    // External I/O (SQLite and the mock server) must progress without
    // Tokio automatically jumping to a merge deadline while we await it.
    fn pause_clock() -> tokio_util::task::AbortOnDropHandle<()> {
        tokio::time::pause();
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }

    async fn logged(manager: &SessionManager, predicate: impl Fn(&[SessionEvent]) -> bool) -> Vec<SessionEvent> {
        // Use real time only as a watchdog: the Tokio clock may be paused.
        let deadline = std::time::Instant::now() + c6_testkit::wait::deadline();
        loop {
            let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
            if predicate(&events) {
                return events;
            }
            assert!(std::time::Instant::now() < deadline, "notice actor did not record its work");
            tokio::task::yield_now().await;
        }
    }

    async fn enqueue_reports(manager: &SessionManager, reports: std::ops::Range<usize>, wake: bool) {
        let tx = manager.actor("held").await.unwrap().unwrap();
        for number in reports.clone() {
            let text = format!("report #{number}.");
            tx.send(ActorMsg::ChildReport {
                command_id: format!("subagent:{text}"),
                recorded: tokio::sync::oneshot::channel().0,
                task_notice: Some(crate::subagent::task_report_notice("Review", "child", TurnEndReason::Completed, &text)),
                text,
                wake,
                inline: None,
            })
            .unwrap();
        }
        barrier(manager, &format!("queued-{}-{}", reports.start, reports.end)).await;
    }

    fn notice_turn_running(manager: &SessionManager) -> bool {
        manager.actors.lock().unwrap().get("held").is_some_and(|actor| actor.running.load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn assert_waiting(manager: &SessionManager, mock: &c6_testkit::MockLlm, count: usize) {
        assert_waiting_after(manager, mock, count, 0).await;
    }

    async fn assert_waiting_after(manager: &SessionManager, mock: &c6_testkit::MockLlm, count: usize, after: u64) {
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert!(!notice_turn_running(manager), "the actor must not start a turn while notices are held");
        assert_eq!(notices_of(&events).2.len(), count, "each report must remain durably pending while waiting");
        assert!(
            !events
                .iter()
                .filter(|e| e.seq > after)
                .any(|e| matches!(&e.kind, Some(session_event::Kind::User(_) | session_event::Kind::TurnStarted(_))))
        );
        assert!(mock.requests().is_empty(), "merging or held notices must not start a model request");
    }

    async fn completed_batch(manager: &SessionManager, mock: &c6_testkit::MockLlm) -> (Vec<SessionEvent>, String) {
        completed_batch_after(manager, mock, 0).await
    }

    async fn completed_batch_after(manager: &SessionManager, mock: &c6_testkit::MockLlm, after: u64) -> (Vec<SessionEvent>, String) {
        // Require the actor to admit the turn on the simulated clock, not
        // the pre-turn checkpoint to finish on it. A contended checkpoint
        // sleeps on Tokio time before TurnStarted can be recorded.
        logged(manager, |events| {
            notice_turn_running(manager)
                || events.iter().any(|e| e.seq > after && matches!(&e.kind, Some(session_event::Kind::TurnStarted(_))))
        })
        .await;
        tokio::time::resume();
        completed_batch_in_real_time_after(manager, mock, after).await
    }

    async fn completed_batch_in_real_time_after(
        manager: &SessionManager,
        mock: &c6_testkit::MockLlm,
        after: u64,
    ) -> (Vec<SessionEvent>, String) {
        let events =
            logged(manager, |events| events.iter().any(|e| e.seq > after && matches!(&e.kind, Some(session_event::Kind::TurnEnded(_)))))
                .await;
        assert!(events.iter().any(|e| e.seq > after && matches!(&e.kind, Some(session_event::Kind::TurnStarted(_)))));
        let ends: Vec<_> = events
            .iter()
            .filter(|e| e.seq > after)
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::TurnEnded(t)) => Some(t.reason),
                _ => None,
            })
            .collect();
        assert_eq!(ends, [TurnEndReason::Completed as i32], "the batch should complete in one turn");
        assert!(notices_of(&events).2.is_empty(), "no delivered report may remain pending in the journal");
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "one turn takes the whole held batch");
        (events, requests[0].to_string())
    }

    async fn delivered(manager: &SessionManager, mock: &c6_testkit::MockLlm, count: usize) {
        let (events, request) = completed_batch(manager, mock).await;
        assert_delivered(&events, &request, count);
    }

    async fn release_held_reports(manager: &SessionManager, mock: &c6_testkit::MockLlm, actor: &Handle, count: usize) {
        // The recovery clock assertions are done. Explicit release does not
        // test a merge timer, and the pre-turn checkpoint can need real time
        // to retry a process scan while other tests spawn or reap children.
        tokio::time::resume();
        mock.push(c6_testkit::Reply::Text("Saw the complete batch.".into()));
        actor.tx.send(ActorMsg::ReleaseNotices).unwrap();
        let (events, request) = completed_batch_in_real_time_after(manager, mock, 0).await;
        assert_delivered(&events, &request, count);
    }

    fn assert_delivered(events: &[SessionEvent], request: &str, count: usize) {
        let delivered: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::User(u)) if u.text.starts_with("report ") => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(delivered.len(), count);
        for number in 1..=count {
            let text = format!("report #{number}.");
            assert!(request.contains(&text), "missing {text} in {request}");
            let matching: Vec<_> = delivered.iter().filter(|u| u.text == text).collect();
            assert_eq!(matching.len(), 1, "each queued report must be recorded as consumed exactly once");
            let metadata = matching[0].task_notice.as_ref().expect("the delivered report retains its metadata");
            assert_eq!(metadata.title, "Review");
            assert_eq!(metadata.session_id, "child");
            assert_eq!(metadata.markdown, text);
        }
    }

    #[tokio::test]
    async fn recovery_deadline_does_not_cancel_an_in_flight_attempt() {
        let _clock = pause_clock();
        let now = tokio::time::Instant::now();
        let (finish, attempt) = tokio::sync::oneshot::channel::<()>();
        let (escalate, mut escalated) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(recovery_at_deadline(now + SECOND, attempt, move || escalate.send(()).unwrap()));
        tokio::time::advance(SECOND - TICK).await;
        assert_eq!(escalated.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty));
        tokio::time::advance(2 * TICK).await;
        escalated.await.unwrap();
        assert!(!join.is_finished());
        finish.send(()).unwrap();
        join.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn held_adoption_stops_automatic_retries_and_reuses_the_incident_until_success() {
        async fn fail() -> anyhow::Result<()> {
            anyhow::bail!("metadata unreadable");
        }
        async fn no_attempt() -> anyhow::Result<()> {
            panic!("unexpected adoption attempt");
        }
        let (_dir, manager, _mock) = idle_session().await;
        manager.actor("held").await.unwrap().unwrap();
        let _clock = pause_clock();
        let gate = manager.actors.lock().unwrap().get("held").unwrap().recovery.clone();
        let writer = Writer::new("held".into(), manager.journal.clone(), manager.link.clone(), 2);
        let mut adoption = HeldAdoptionRecovery::new(gate.clone(), SECOND, (5 * SECOND, 300 * SECOND));
        assert!(!adoption.check(&writer, fail()).await);
        assert!(gate.adopting.load(Ordering::Acquire));
        assert!(!adoption.check(&writer, no_attempt()).await, "mailbox activity cannot bypass backoff");
        tokio::time::advance(SECOND).await;
        assert!(!adoption.check(&writer, no_attempt()).await, "deadline must not start one more automatic attempt");
        let incident = gate.request.lock().unwrap().as_ref().unwrap().incident.clone();
        assert!(adoption.reason.contains("metadata unreadable"));

        tokio::time::advance(10 * SECOND).await;
        assert!(!adoption.check(&writer, no_attempt()).await, "paused adoption must wait for user action");
        manager.recovery_action("held", gate.epoch, &incident, "retry").unwrap();
        let action = adoption.action.as_mut().unwrap().await.unwrap();
        adoption.resume(action, SECOND);
        assert!(!adoption.check(&writer, fail()).await);
        assert_eq!(gate.request.lock().unwrap().as_ref().unwrap().incident, incident);
        assert!(!adoption.check(&writer, no_attempt()).await, "Retry only authorizes a single attempt");

        manager.recovery_action("held", gate.epoch, &incident, "restart").unwrap();
        let action = adoption.action.as_mut().unwrap().await.unwrap();
        adoption.resume(action, SECOND);
        assert!(!adoption.check(&writer, fail()).await);
        tokio::time::advance(Duration::from_millis(100) + TICK).await;
        assert!(adoption.check(&writer, async { Ok(()) }).await, "Restart allows bounded automatic retries");
        drop(adoption);
        assert!(!gate.adopting.load(Ordering::Acquire));
        assert!(gate.request.lock().unwrap().is_none());
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(&e.kind, Some(session_event::Kind::Error(e)) if e.message.contains("metadata unreadable")))
                .count(),
            1,
            "the initial failure is durable, but retries do not write another error"
        );
    }

    #[tokio::test]
    async fn held_adoption_waits_for_disk_without_retiring_or_spending_the_ownership_budget() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        for corruption_after_wait in [false, true] {
            let (_dir, manager, _mock) = idle_session().await;
            let free = Arc::new(AtomicU64::new(0));
            let sampled = free.clone();
            let disk = Arc::new(crate::disk::Monitor::immediate(move |_| {
                Some(crate::disk::Space { mount: "/test/state".into(), free: sampled.load(Ordering::SeqCst), total: 1 << 40 })
            }));
            let mut writer = Writer::new("held".into(), manager.journal.clone(), manager.link.clone(), 2);
            writer.disk_probe = Some((disk, manager.state_dir.clone()));
            let gate = Arc::new(RecoveryGate::default());
            let fault = Arc::new(AtomicBool::new(true));
            manager.journal.inject_recovery_error(if corruption_after_wait { 4874 } else { 13 }, fault.clone());
            let _clock = pause_clock();
            let mut adoption = HeldAdoptionRecovery::new(gate.clone(), SECOND, (5 * SECOND, 300 * SECOND));
            assert!(!adoption.check(&writer, manager.journal.recover_connection()).await);
            tokio::time::advance(240 * SECOND).await;
            assert!(!adoption.check(&writer, manager.journal.recover_connection()).await);
            assert!(gate.request.lock().unwrap().is_none());
            assert!(gate.disk_wait.lock().unwrap().as_ref().unwrap().contains("/test/state: 0 bytes free"));
            let actual_retry = adoption.disk_retry_at;
            tokio::time::advance(SECOND).await;
            assert!(!adoption.check(&writer, async { panic!("sampling must not bypass the attempt backoff") }).await);
            assert_eq!(adoption.disk_retry_at, actual_retry);
            if corruption_after_wait {
                manager.journal.inject_recovery_error(11, fault);
                tokio::time::advance(actual_retry.unwrap() - tokio::time::Instant::now()).await;
                assert!(!adoption.check(&writer, manager.journal.recover_connection()).await);
                assert!(gate.disk_wait.lock().unwrap().is_none());
                assert!(gate.request.lock().unwrap().is_none(), "corruption gets its own bounded ownership retry budget");
                tokio::time::advance(SECOND).await;
                assert!(!adoption.check(&writer, async { panic!("expired corruption budget must not retry") }).await);
                assert!(gate.request.lock().unwrap().is_some());
            } else {
                fault.store(false, Ordering::SeqCst);
                free.store(crate::disk::RESUME_BYTES, Ordering::SeqCst);
                tokio::time::advance(SECOND).await;
                assert!(adoption.check(&writer, manager.journal.recover_connection()).await, "space return bypasses the backoff");
            }
            drop(adoption);
            assert!(gate.disk_wait.lock().unwrap().is_none());
            assert!(gate.request.lock().unwrap().is_none());
            assert!(!gate.adopting.load(Ordering::Acquire));
            tokio::time::resume();
        }
    }

    fn enqueue_test_report(actor: &Handle, number: usize) -> tokio::sync::oneshot::Receiver<()> {
        let (recorded, ack) = tokio::sync::oneshot::channel();
        let text = format!("report #{number}.");
        actor
            .tx
            .send(ActorMsg::ChildReport {
                command_id: format!("subagent:{text}"),
                task_notice: Some(crate::subagent::task_report_notice("Review", "child", TurnEndReason::Completed, &text)),
                text,
                wake: false,
                inline: None,
                recorded,
            })
            .unwrap();
        ack
    }

    #[tokio::test]
    async fn shmsize_recovery_preserves_held_failed_and_later_notices() {
        let (_dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        enqueue_reports(&manager, 1..2, false).await;
        manager.journal.inject_append_shmsize("report #2.").await;
        let actor = manager.actors.lock().unwrap().get("held").unwrap().clone();
        assert!(enqueue_test_report(&actor, 2).await.is_err(), "the real SHMSIZE error stopped the first append");
        let queued_in_mailbox = enqueue_test_report(&actor, 3);
        // Allow the first automatic attempt, which must replace the poisoned
        // connection before reconstructing the backlog in this same actor.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100) + TICK).await;
        barrier(&manager, "after-shmsize-recovery").await;
        queued_in_mailbox.await.unwrap();
        assert!(actor.recovery.request.lock().unwrap().is_none(), "one fresh connection made escalation unnecessary");
        assert_waiting(&manager, &mock, 3).await;
        release_held_reports(&manager, &mock, &actor, 3).await;
    }

    #[tokio::test]
    async fn bounded_recovery_keeps_held_and_mailbox_notices_until_retry() {
        let (dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        enqueue_reports(&manager, 1..2, false).await;
        let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("journal.db").display())).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER persistent_failure BEFORE INSERT ON events WHEN NEW.session_id = 'held' \
                     BEGIN SELECT RAISE(ABORT, 'persistent journal fault'); END",
        )
        .execute(&db)
        .await
        .unwrap();
        let actor = manager.actors.lock().unwrap().get("held").unwrap().clone();
        assert!(enqueue_test_report(&actor, 2).await.is_err(), "the notice's first append failed");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(u64::from(c6_api::DEFAULT_SESSION_RECOVERY_SECONDS) - 1)).await;
        assert!(actor.recovery.request.lock().unwrap().is_none());
        tokio::time::advance(SECOND + TICK).await;

        let deadline = std::time::Instant::now() + c6_testkit::wait::deadline();
        let incident = loop {
            if let Some(request) = actor.recovery.request.lock().unwrap().as_ref().filter(|r| r.ready.is_some()) {
                break request.incident.clone();
            }
            assert!(std::time::Instant::now() < deadline, "recovery did not escalate at its deadline");
            tokio::task::yield_now().await;
        };
        let mut queued_in_mailbox = enqueue_test_report(&actor, 3);
        sqlx::query("DROP TRIGGER persistent_failure").execute(&db).await.unwrap();
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(queued_in_mailbox.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty));
        assert!(manager.recovery_action("held", 1, &incident, "retry").is_err(), "misrouted epoch must not reach the actor");
        assert!(manager.recovery_action("held", 0, "stale", "retry").is_err(), "stale controls must not restart a different incident");
        manager.recovery_action("held", 0, &incident, "retry").unwrap();
        barrier(&manager, "after-manual-retry").await;
        queued_in_mailbox.try_recv().unwrap();
        assert!(actor.recovery.request.lock().unwrap().is_none());
        assert_waiting(&manager, &mock, 3).await;
        release_held_reports(&manager, &mock, &actor, 3).await;
    }

    #[tokio::test]
    async fn disk_full_outlives_recovery_deadline_and_resumes_without_action() {
        disk_full_recovery(false, false).await;
    }

    #[tokio::test]
    async fn disk_full_resumes_an_interrupted_turn_after_the_old_deadline() {
        disk_full_recovery(true, false).await;
    }

    #[tokio::test]
    async fn depth_positive_disk_recovery_resumes_below_new_work_floor_but_preserves_load_admission() {
        disk_full_recovery(true, true).await;
    }

    async fn disk_full_recovery(running: bool, child: bool) {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        struct WaitingHead(String, mpsc::UnboundedSender<serde_json::Value>);
        #[async_trait]
        impl crate::link::LocalHead for WaitingHead {
            async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
                if method == "session.recovery_status" {
                    self.1.send(params).unwrap();
                }
                Ok(serde_json::Value::Null)
            }
            async fn credential(&self, connection: &str) -> c6_proto::CredentialGrant {
                crate::link::LocalHead::credential(&Head(self.0.clone()), connection).await
            }
        }
        let (dir, mut manager, mock) = idle_session().await;
        let admission_stage = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if child {
            manager.launch_pressure = Arc::new(crate::resource_pressure::Gate::immediate({
                let stage = admission_stage.clone();
                move || crate::ResourceMeasurement {
                    disk_nearly_full: stage.load(Ordering::SeqCst) > 0,
                    load: Some(if stage.load(Ordering::SeqCst) == 1 { 2.0 } else { 0.0 }),
                    ..Default::default()
                }
            }));
        }
        let (sent, mut statuses) = mpsc::unbounded_channel();
        if running {
            // Even a budget shorter than the first SQLite checkout must not
            // install manual recovery after an already classified FULL/I/O.
            let (mut spec, _) = manager.journal.load_session("held").await.unwrap().unwrap();
            spec.session_recovery_seconds = 1;
            if child {
                // Template/remote children use depth without a local
                // delegation and therefore resume interrupted turns.
                spec.depth = 1;
            }
            let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("journal.db").display())).await.unwrap();
            sqlx::query("UPDATE sessions SET spec = ? WHERE id = 'held'").bind(spec.encode_to_vec()).execute(&db).await.unwrap();
            db.close().await;
        }
        manager.link = Arc::new(HeadLink::local(Arc::new(WaitingHead(mock.base_url(), sent))));
        let free = Arc::new(AtomicU64::new(0));
        let sampled = free.clone();
        manager.disk = Arc::new(crate::disk::Monitor::immediate(move |_| {
            Some(crate::disk::Space { mount: "/test/state".into(), free: sampled.load(Ordering::SeqCst), total: 1 << 40 })
        }));
        let _clock = pause_clock();
        enqueue_reports(&manager, 1..2, false).await;
        let _held_turn = if running {
            let gate = mock.push_gated(c6_testkit::Reply::Text("Original turn still in flight.".into()));
            mock.push(c6_testkit::Reply::Text("Recovered turn completed automatically.".into()));
            manager
                .handle(SessionCommand {
                    session_id: "held".into(),
                    command_id: "start-before-full".into(),
                    kind: Some(session_command::Kind::Input(c6_proto::UserInput {
                        text: "Continue this work through disk pressure.".into(),
                        human_origin: true,
                        ..Default::default()
                    })),
                    ..Default::default()
                })
                .await
                .unwrap();
            mock.request_headers(0).await;
            Some(gate)
        } else {
            None
        };
        let full = Arc::new(AtomicBool::new(true));
        admission_stage.store(1, Ordering::SeqCst);
        manager.journal.inject_recovery_error(13, full.clone());
        manager.journal.inject_append_shmsize("report #2.").await;
        let actor = manager.actors.lock().unwrap().get("held").unwrap().clone();
        assert!(enqueue_test_report(&actor, 2).await.is_err());
        // Recovery crosses SQLite's worker thread. Advancing virtual time on
        // each pending poll can exhaust its pool timeout before real I/O runs
        // under parallel load. Await the status in real time, then freeze only
        // for the recovery-deadline proof below.
        tokio::time::resume();
        let waiting = c6_testkit::within("the disk-wait status", statuses.recv()).await.unwrap();
        tokio::time::pause();
        assert_eq!(waiting["automatic"], true);
        assert!(waiting["reason"].as_str().unwrap().contains("/test/state: 0 bytes free"));
        tokio::time::advance(Duration::from_secs(240)).await;
        tokio::task::yield_now().await;
        assert!(actor.recovery.request.lock().unwrap().is_none(), "disk full must not enter the manual lane after 180 seconds");
        let evidence = manager.liveness_report();
        assert_eq!(evidence.sessions[0].actor_state, c6_proto::SessionActorState::WaitingForDisk as i32);
        // A queued callback survives the wait. Free space wakes recovery on
        // the next sampler tick, without spending the capped backoff delay.
        let mut later = enqueue_test_report(&actor, 3);
        full.store(false, Ordering::SeqCst);
        let recovered_free = crate::disk::RESUME_BYTES + 1024;
        assert!((recovered_free as f64) < (1u64 << 40) as f64 * 0.05);
        free.store(recovered_free, Ordering::SeqCst);
        tokio::time::advance(SECOND + TICK).await;
        // The recovery barrier also queries SQLite: let sampler/recovery
        // retries use real time instead of racing its I/O with virtual ticks.
        tokio::time::resume();
        barrier(&manager, "disk-space-returned").await;
        tokio::time::pause();
        later.try_recv().unwrap();
        assert!(actor.recovery.disk_wait.lock().unwrap().is_none());
        assert!(actor.recovery.request.lock().unwrap().is_none());
        if running {
            // ResumeFailed is queued behind the callbacks retained above;
            // let it install the normal notice merge timer before advancing.
            barrier(&manager, "automatic-resume-queued").await;
            tokio::time::advance(QUIET + TICK).await;
            // Resource timing is proved above. Process scans, SQLite and HTTP
            // must now complete without manually advancing their retry and
            // acquire deadlines while they await external I/O.
            tokio::time::resume();
            if child {
                // Recovery is complete but continuation must still wait for
                // high load. Disk remains below 5% throughout this proof.
                let watchdog = std::time::Instant::now() + c6_testkit::wait::deadline();
                loop {
                    let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
                    if events.iter().any(|e| {
                        matches!(&e.kind,
                        Some(session_event::Kind::Notice(n)) if n.message == crate::resource_pressure::Pressure::Load.message())
                    }) {
                        break;
                    }
                    assert!(std::time::Instant::now() < watchdog, "recovered child did not reach memory/load-only admission");
                    tokio::task::yield_now().await;
                }
                assert_eq!(mock.requests().len(), 1, "high load must still hold the continuation");
                admission_stage.store(2, Ordering::SeqCst);
                assert_eq!(
                    manager.launch_pressure.try_start(),
                    Err(crate::resource_pressure::Pressure::Disk),
                    "new work still waits below 5%"
                );
            }
            let events = logged(&manager, |events| {
                events.iter().any(|e| {
                    matches!(&e.kind,
                Some(session_event::Kind::TurnEnded(t)) if t.reason == TurnEndReason::Completed as i32)
                })
            })
            .await;
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(&e.kind,
                Some(session_event::Kind::User(u)) if u.command_id == "start-before-full"))
                    .count(),
                1
            );
            assert!(mock.requests().len() >= 2, "the interrupted turn must continue without another user input");
        } else {
            assert_waiting(&manager, &mock, 3).await;
            release_held_reports(&manager, &mock, &actor, 3).await;
        }
    }

    #[tokio::test]
    async fn corruption_on_a_full_filesystem_still_offers_retry_and_restart() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for (code, action) in [(11, "retry"), (26, "restart")] {
            let (_dir, mut manager, _mock) = idle_session().await;
            let full = crate::disk::Space { mount: "/test".into(), free: 0, total: 1 << 40 };
            manager.disk = Arc::new(crate::disk::Monitor::immediate(move |_| Some(full.clone())));
            let clock = pause_clock();
            enqueue_reports(&manager, 1..2, false).await;
            let fault = Arc::new(AtomicBool::new(true));
            manager.journal.inject_recovery_error(code, fault.clone());
            let error = manager.journal.recover_connection().await.unwrap_err();
            assert!(!crate::disk::resource_error(&error, manager.disk.space(Path::new("/test")).as_ref()));
            manager.journal.inject_append_error("report #2.", code).await;
            let actor = manager.actors.lock().unwrap().get("held").unwrap().clone();
            assert!(enqueue_test_report(&actor, 2).await.is_err());
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(u64::from(c6_api::DEFAULT_SESSION_RECOVERY_SECONDS) + 1)).await;
            let watchdog = std::time::Instant::now() + c6_testkit::wait::deadline();
            let incident = loop {
                if let Some(r) = actor.recovery.request.lock().unwrap().as_ref().filter(|r| r.ready.is_some()) {
                    break r.incident.clone();
                }
                assert!(std::time::Instant::now() < watchdog, "corruption did not stop at the recovery bound");
                tokio::task::yield_now().await;
            };
            assert!(actor.recovery.disk_wait.lock().unwrap().is_none());
            fault.store(false, Ordering::SeqCst);
            manager.recovery_action("held", 0, &incident, action).unwrap();
            let recovered = barrier(&manager, "corruption-explicit-action");
            tokio::pin!(recovered);
            let watchdog = std::time::Instant::now() + c6_testkit::wait::deadline();
            loop {
                if futures::poll!(&mut recovered).is_ready() {
                    break;
                }
                assert!(std::time::Instant::now() < watchdog, "manual recovery did not resume");
                tokio::time::advance(TICK).await;
                tokio::task::yield_now().await;
            }
            assert!(actor.recovery.request.lock().unwrap().is_none());
            drop(clock);
            tokio::time::resume();
        }
    }

    #[tokio::test]
    async fn recovery_control_rejects_requests_until_ready_and_while_busy() {
        let (_dir, manager, _mock) = idle_session().await;
        manager.actor("held").await.unwrap().unwrap();
        let actor = manager.actors.lock().unwrap().get("held").unwrap().clone();
        let writer = Writer::new("held".into(), manager.journal.clone(), Arc::new(HeadLink::default()), 2);
        let pause = RecoveryPause::new(actor.recovery.clone(), &writer, "waiting for a safe stop");
        let retry = || manager.recovery_action("held", 0, &pause.id, "retry");
        assert!(retry().unwrap_err().to_string().contains("still in progress"));
        let action = pause.wait_action();
        tokio::pin!(action);
        assert!(futures::poll!(&mut action).is_pending());
        retry().unwrap();
        assert!(retry().unwrap_err().to_string().contains("still in progress"));
        assert_eq!(action.await, RecoveryAction::Retry);
    }

    #[tokio::test]
    async fn parent_disk_wait_notice_is_not_a_subagent_completion() {
        let (_dir, manager, _mock) = idle_session().await;
        let _clock = pause_clock();
        let text = "Subagent child is waiting for disk space on /test: 0 bytes free. It continues automatically.";
        manager
            .handle(SessionCommand {
                command_id: "disk-observation".into(),
                session_id: "held".into(),
                kind: Some(session_command::Kind::DelegationEnded(c6_proto::DelegationEnded {
                    child_session_id: "child".into(),
                    reason: TurnEndReason::Completed as i32,
                    report: text.into(),
                    digest: text.into(),
                    wake: false,
                    task_notice: Some(c6_proto::TaskNotice {
                        kind: "recovery".into(),
                        title: "Child".into(),
                        session_id: "child".into(),
                        status: "waiting".into(),
                        markdown: text.into(),
                    }),
                    ..Default::default()
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        barrier(&manager, "waiting-not-completed").await;
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert!(events.iter().any(|e| matches!(&e.kind,
            Some(session_event::Kind::SubagentReported(r)) if r.status == "waiting" && r.call_id.is_empty())));
        let notice = events
            .iter()
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::NoticeQueued(n)) if n.command_id == "subagent:disk-observation" => Some(n),
                _ => None,
            })
            .unwrap();
        assert_eq!(notice.text, text);
        assert!(!notice.text.contains("status=\"completed\""));
    }

    #[tokio::test]
    async fn recovery_report_retries_unacknowledged_warning_before_clear() {
        struct ReportHead(mpsc::UnboundedSender<serde_json::Value>, std::sync::atomic::AtomicBool);
        #[async_trait]
        impl crate::link::LocalHead for ReportHead {
            async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
                assert_eq!(method, "session.recovery_status");
                self.0.send(params["reason"].clone()).unwrap();
                if self.1.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    Err("head could not enqueue the parent's report".into())
                } else {
                    Ok(serde_json::Value::Null)
                }
            }
            async fn credential(&self, _: &str) -> c6_proto::CredentialGrant {
                unreachable!()
            }
        }
        let _clock = pause_clock();
        let (sent, mut calls) = mpsc::unbounded_channel();
        let link = Arc::new(HeadLink::local(Arc::new(ReportHead(sent, true.into()))));
        let reason = "worker journal unavailable".to_owned();
        let (changes, state) = tokio::sync::watch::channel(Some(reason.clone()));
        let task = tokio::spawn(report_recovery(link, "child".into(), 0, "incident".into(), reason.clone(), state, false));
        assert_eq!(calls.recv().await.unwrap(), reason);
        changes.send_replace(None);
        assert_eq!(calls.recv().await.unwrap(), reason);
        assert_eq!(calls.recv().await.unwrap(), serde_json::Value::Null);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_report_clear_deadline_cancels_blocked_outbound_send() {
        let link = Arc::new(HeadLink::default());
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(c6_proto::WorkerMsg::default()).unwrap();
        link.connected(tx);
        let reason = "worker journal unavailable".to_owned();
        let (changes, state) = tokio::sync::watch::channel(Some(reason.clone()));
        let mut report = Box::pin(report_recovery(link.clone(), "child".into(), 0, "incident".into(), reason, state, false));
        assert!(futures::poll!(report.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(futures::poll!(report.as_mut()).is_pending(), "no cleanup deadline while active");
        changes.send_replace(None);
        assert!(futures::poll!(report.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(futures::poll!(report.as_mut()).is_pending(), "keep trying within the cleanup budget");
        tokio::time::advance(SECOND + TICK).await;
        tokio::time::timeout(SECOND, report).await.expect("reporter must stop despite the full outbound queue");
        assert!(rx.try_recv().unwrap().msg.is_none());
        assert!(rx.try_recv().is_err(), "cancelling the send must not enqueue the warning later");
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_report_clear_deadline_includes_pending_warning_and_local_clear() {
        type PendingCall = (serde_json::Value, tokio::sync::oneshot::Receiver<()>);
        struct ReportHead {
            calls: mpsc::UnboundedSender<PendingCall>,
            ack_warning: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        }
        #[async_trait]
        impl crate::link::LocalHead for ReportHead {
            async fn call(&self, _: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
                let (in_flight, dropped) = tokio::sync::oneshot::channel();
                self.calls.send((params["reason"].clone(), dropped)).unwrap();
                let _in_flight = in_flight;
                let ack_warning = self.ack_warning.lock().unwrap().take();
                if let Some(ack) = ack_warning {
                    ack.await.unwrap();
                    Ok(serde_json::Value::Null)
                } else {
                    std::future::pending().await
                }
            }
            async fn credential(&self, _: &str) -> c6_proto::CredentialGrant {
                unreachable!()
            }
        }
        let (sent, mut calls) = mpsc::unbounded_channel();
        let (ack, ack_warning) = tokio::sync::oneshot::channel();
        let link = Arc::new(HeadLink::local(Arc::new(ReportHead { calls: sent, ack_warning: Mutex::new(Some(ack_warning)) })));
        let reason = "worker journal unavailable".to_owned();
        let (changes, state) = tokio::sync::watch::channel(Some(reason.clone()));
        let mut report = Box::pin(report_recovery(link, "child".into(), 0, "incident".into(), reason.clone(), state, false));
        assert!(futures::poll!(report.as_mut()).is_pending());
        let (warning, mut warning_dropped) = calls.try_recv().unwrap();
        assert_eq!(warning, reason);
        changes.send_replace(None);
        assert!(futures::poll!(report.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(50)).await;
        assert!(futures::poll!(report.as_mut()).is_pending());
        assert_eq!(warning_dropped.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty));
        assert!(calls.try_recv().is_err(), "a pending warning must not be displaced by a clear");
        ack.send(()).unwrap();
        assert!(futures::poll!(report.as_mut()).is_pending());
        let (clear, mut clear_dropped) = calls.try_recv().unwrap();
        assert!(clear.is_null());
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(futures::poll!(report.as_mut()).is_pending());
        assert_eq!(clear_dropped.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty));
        tokio::time::advance(SECOND + TICK).await;
        tokio::time::timeout(SECOND, report).await.expect("the clear has only the rest of the original budget");
        assert_eq!(clear_dropped.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed));
    }

    const GROUP_SIZE: usize = 5;

    fn audit_spawn(number: usize, wake: &str) -> session_event::Kind {
        session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned {
            call_id: format!("call-{number}"),
            child_session_id: format!("audit-child-{number}"),
            background_task_id: format!("audit-task-{number}"),
            description: format!("Audit part {number}"),
            group: "audit".into(),
            wake: wake.into(),
            ..Default::default()
        })
    }

    async fn seed_audit_group(manager: &SessionManager, wake: &str) {
        // Seed launches before starting the actor, just as replay sees them.
        let now = c6_core::now_ms();
        for number in 1..=GROUP_SIZE {
            manager.journal.append("held", &ev(number as u64 + 1, now, audit_spawn(number, wake))).await.unwrap();
        }
    }

    async fn enqueue_audit_reports(manager: &SessionManager, reports: std::ops::RangeInclusive<usize>, reason: TurnEndReason) {
        let tx = manager.actor("held").await.unwrap().unwrap();
        for number in reports.clone() {
            let child = format!("audit-child-{number}");
            let text = format!("audit report #{number}.");
            tx.send(ActorMsg::ChildReport {
                command_id: format!("subagent:{child}"),
                recorded: tokio::sync::oneshot::channel().0,
                task_notice: Some(crate::subagent::task_report_notice("Audit", &child, reason, &text)),
                text,
                wake: true,
                inline: None,
            })
            .unwrap();
        }
        barrier(manager, &format!("queued-audit-{}-{}", reports.start(), reports.end())).await;
    }

    async fn audit_delivered(manager: &SessionManager, mock: &c6_testkit::MockLlm, count: usize, failures: usize) {
        // A ready group should turn without the ordinary 3s merge delay.
        tokio::time::advance(TICK).await;
        let (events, request) = completed_batch(manager, mock).await;
        assert_audit_batch(&events, &request, count, GROUP_SIZE, failures);
    }

    fn assert_audit_batch(events: &[SessionEvent], request: &str, count: usize, total: usize, failures: usize) {
        let summary = format!("group audit: {count} of {total} finished, {failures} failed");
        assert!(request.contains(&summary), "missing {summary} in {request}");
        let mut children: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::User(u)) => u.task_notice.as_ref().map(|n| n.session_id.clone()),
                _ => None,
            })
            .collect();
        children.sort();
        assert_eq!(children, (1..=count).map(|n| format!("audit-child-{n}")).collect::<Vec<_>>());
        for number in 1..=count {
            assert!(request.contains(&format!("audit report #{number}.")), "missing audit report #{number} in {request}");
        }
    }

    async fn audit_threshold(wake: &str, target: usize) {
        let (_dir, manager, mock) = idle_session().await;
        seed_audit_group(&manager, wake).await;
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw the audit reports.".into()));
        if target > 1 {
            enqueue_audit_reports(&manager, 1..=target - 1, TurnEndReason::Completed).await;
            assert_waiting(&manager, &mock, target - 1).await;
            tokio::time::advance(MAX_WAIT + TICK).await;
            barrier(&manager, "group-still-waiting").await;
            assert_waiting(&manager, &mock, target - 1).await;
            if target == GROUP_SIZE {
                let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
                assert!(events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::NoticeQueued(n))
                    if n.reason.contains("4 of 5 finished; waiting for 1 (Audit part 5)"))));
            }
        }
        enqueue_audit_reports(&manager, target..=target, TurnEndReason::Completed).await;
        audit_delivered(&manager, &mock, target, 0).await;
    }

    #[tokio::test]
    async fn audit_group_all_wakes_once_for_the_fifth_report() {
        audit_threshold("all", GROUP_SIZE).await;
    }

    #[tokio::test]
    async fn audit_group_snapshots_and_duplicate_callbacks_deliver_three_reports_once() {
        use session_event::Kind;

        let (_dir, manager, mock) = idle_session().await;
        append_seed(&manager.journal, 1, (1..=3).map(|n| audit_spawn(n, "all"))).await;
        let clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw the three audit reports.".into()));
        enqueue_audit_reports(&manager, 1..=2, TurnEndReason::Completed).await;
        enqueue_audit_reports(&manager, 1..=1, TurnEndReason::Completed).await;
        tokio::time::advance(MAX_WAIT + TICK).await;
        barrier(&manager, "two-of-three-are-still-held").await;
        assert_waiting(&manager, &mock, 2).await;

        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        let reasons: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(Kind::NoticeQueued(n)) if n.command_id == "subagent:audit-child-1" => Some(n.reason.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            [
                "collecting reports for delivery",
                "group audit: 1 of 3 finished; waiting for 2 (Audit part 2, Audit part 3)",
                "group audit: 2 of 3 finished; waiting for 1 (Audit part 3)",
            ],
            "queue snapshots record progress, not a second receipt of the same callback"
        );
        assert!(!c6_api::NoticeHoldReason::parse(reasons[2]).needs_you());

        enqueue_audit_reports(&manager, 3..=3, TurnEndReason::Completed).await;
        tokio::time::advance(TICK).await;
        let (_, request) = completed_batch(&manager, &mock).await;
        drop(clock);
        let _clock = pause_clock();
        enqueue_audit_reports(&manager, 1..=3, TurnEndReason::Completed).await;
        tokio::time::advance(MAX_WAIT + TICK).await;
        barrier(&manager, "duplicates-after-delivery").await;
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert_audit_batch(&events, &request, 3, 3, 0);
        assert!(notices_of(&events).2.is_empty(), "delivered snapshots must not replay as pending");
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, Some(Kind::TurnStarted(_)))).count(), 1);
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn audit_group_count_wakes_at_its_configured_threshold() {
        audit_threshold("count:3", 3).await;
    }

    #[tokio::test]
    async fn audit_group_count_releases_when_all_members_settle_below_threshold() {
        let (_dir, manager, mock) = idle_session().await;
        append_seed(&manager.journal, 1, (1..=3).map(|n| audit_spawn(n, "count:5"))).await;
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw all available audit reports.".into()));
        enqueue_audit_reports(&manager, 1..=2, TurnEndReason::Completed).await;
        tokio::time::advance(MAX_WAIT + TICK).await;
        barrier(&manager, "count-still-has-an-unsettled-member").await;
        assert_waiting(&manager, &mock, 2).await;

        enqueue_audit_reports(&manager, 3..=3, TurnEndReason::Completed).await;
        tokio::time::advance(TICK).await;
        let (events, request) = completed_batch(&manager, &mock).await;
        assert_audit_batch(&events, &request, 3, 3, 0);
        assert!(request.contains("wake threshold of 5 not met, but all members finished"), "{request}");
    }

    #[tokio::test]
    async fn audit_group_any_wakes_for_the_first_report() {
        audit_threshold("any", 1).await;
    }

    #[tokio::test]
    async fn audit_group_failure_wakes_immediately_with_four_children_unfinished() {
        audit_group_adverse_outcome_wakes(TurnEndReason::Error).await;
    }

    #[tokio::test]
    async fn audit_group_loss_wakes_immediately_with_four_children_unfinished() {
        audit_group_adverse_outcome_wakes(TurnEndReason::WorkerRestarted).await;
    }

    async fn audit_group_adverse_outcome_wakes(reason: TurnEndReason) {
        let (_dir, manager, mock) = idle_session().await;
        seed_audit_group(&manager, "all").await;
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw the audit failure.".into()));
        enqueue_audit_reports(&manager, 1..=1, reason).await;
        audit_delivered(&manager, &mock, 1, 1).await;
    }

    fn task_calls(turn: u64, tasks: &[(&str, &str)], group: Option<&str>) -> session_event::Kind {
        let blocks = tasks
            .iter()
            .map(|&(id, description)| {
                let mut input = serde_json::json!({ "description": description, "prompt": description, "run_in_background": true });
                if let Some(group) = group {
                    input["group"] = group.into();
                    input["wake"] = "all".into();
                }
                c6_proto::ContentBlock {
                    block: Some(c6_proto::content_block::Block::ToolUse(c6_proto::ToolUse {
                        id: id.into(),
                        name: "Task".into(),
                        input_json: input.to_string(),
                    })),
                    ..Default::default()
                }
            })
            .collect();
        session_event::Kind::Assistant(c6_proto::AssistantMessage { turn, blocks, ..Default::default() })
    }

    fn task_ack(call: &str) -> session_event::Kind {
        session_event::Kind::ToolResult(c6_proto::ToolResult {
            call_id: call.into(),
            tool: "Task".into(),
            output: "Started in the background".into(),
            ..Default::default()
        })
    }

    fn started_turn(turn: u64) -> session_event::Kind {
        session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })
    }

    fn ended_turn(turn: u64) -> session_event::Kind {
        session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Completed as i32 })
    }

    async fn append_seed(journal: &Journal, after: u64, kinds: impl IntoIterator<Item = session_event::Kind>) -> u64 {
        let now = c6_core::now_ms();
        let mut seq = after;
        for kind in kinds {
            seq += 1;
            journal.append("held", &ev(seq, now, kind)).await.unwrap();
        }
        seq
    }

    #[tokio::test]
    async fn audit_group_waits_for_a_logged_task_call_whose_child_is_not_linked_yet() {
        let (dir, manager, mock) = idle_session().await;
        let initial = append_seed(
            &manager.journal,
            1,
            [
                user("audit-request"),
                started_turn(1),
                task_calls(1, &[("call-1", "Audit part 1"), ("call-2", "Audit part 2")], Some("audit")),
                task_ack("call-1"),
                task_ack("call-2"),
                audit_spawn(1, "all"),
                ended_turn(1),
            ],
        )
        .await;

        let _clock = pause_clock();
        enqueue_audit_reports(&manager, 1..=1, TurnEndReason::Completed).await;
        tokio::time::advance(MAX_WAIT + TICK).await;
        barrier(&manager, "unlinked-sibling-still-pending").await;
        assert_waiting_after(&manager, &mock, 1, initial).await;
        let (spec, events) = manager.journal.load_session("held").await.unwrap().unwrap();
        let reason = events.iter().rev().find_map(|e| match &e.kind {
            Some(session_event::Kind::NoticeQueued(n)) if n.command_id == "subagent:audit-child-1" => Some(n.reason.as_str()),
            _ => None,
        });
        assert_eq!(reason, Some("group audit: 1 of 2 finished; waiting for 1 (Audit part 2)"));

        // Replay the same pending report in a separate journal so the live
        // actor's sequence counter never races a direct test append.
        let journal = Arc::new(Journal::open(&dir.path().join("replay.db")).await.unwrap());
        journal.import("held", &spec, &events).await.unwrap();
        append_seed(&journal, events.last().unwrap().seq, [audit_spawn(2, "all")]).await;
        let replay = SessionManager::new(journal, manager.link.clone(), dir.path().join("replay/worktrees"));
        mock.push(c6_testkit::Reply::Text("Saw both audit reports.".into()));
        enqueue_audit_reports(&replay, 2..=2, TurnEndReason::Completed).await;
        tokio::time::advance(TICK).await;
        let (events, request) = completed_batch_after(&replay, &mock, initial).await;
        assert_audit_batch(&events, &request, 2, 2, 0);
    }

    async fn audit_group_releases_on_a_failure_before_the_child_is_linked(tool: &str) {
        use session_event::Kind;
        let (_dir, manager, mock) = idle_session().await;
        let Kind::Assistant(mut calls) = task_calls(1, &[("call-1", "linked"), ("call-2", "orphan")], Some(" audit \t")) else {
            unreachable!()
        };
        for block in &mut calls.blocks {
            let Some(c6_proto::content_block::Block::ToolUse(use_)) = &mut block.block else { unreachable!() };
            use_.name = tool.into();
        }
        let launch = |call: &str, task: &str| {
            Kind::ToolResult(c6_proto::ToolResult {
                call_id: call.into(),
                tool: tool.into(),
                output: format!(
                    "Started the subagent in the background as {task}. Its report will arrive as a notice when it finishes; continue with other work meanwhile."
                ),
                ..Default::default()
            })
        };
        let Kind::SubagentSpawned(mut linked) = audit_spawn(1, "all") else { unreachable!() };
        linked.background_task_id = "bg1".into();
        let initial = append_seed(
            &manager.journal,
            1,
            [
                user("audit-request"),
                started_turn(1),
                Kind::Assistant(calls),
                launch("call-1", "bg1"),
                launch("call-2", "bg2"),
                Kind::SubagentSpawned(linked),
                ended_turn(1),
            ],
        )
        .await;
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw both outcomes.".into()));
        let actor = manager.actor("held").await.unwrap().unwrap();
        let finish = |task: &str, code, output: &str| {
            ActorMsg::Task(c6_tools::TaskEvent {
                id: task.into(),
                kind: c6_tools::TaskKind::Agent,
                description: task.into(),
                state: c6_tools::TaskState::Finished,
                notify: true,
                exit_code: Some(code),
                elapsed: TICK,
                output: output.into(),
                pgid: None,
                native_type: String::new(),
                foreground: false,
            })
        };
        actor.send(finish("bg1", 0, "linked success")).unwrap();
        barrier(&manager, "linked-sibling-only").await;
        tokio::time::advance(MAX_WAIT + TICK).await;
        barrier(&manager, "linked-sibling-still-held").await;
        assert_waiting_after(&manager, &mock, 1, initial).await;

        actor.send(finish("bg2", 1, "could not resolve model")).unwrap();
        barrier(&manager, "orphan-failure-arrived").await;
        tokio::time::advance(TICK).await;
        let (events, request) = completed_batch_after(&manager, &mock, initial).await;
        assert!(request.contains("group audit: 2 of 2 finished, 1 failed"), "{request}");
        assert!(request.contains("linked success") && request.contains("could not resolve model"), "{request}");
        let mut delivered: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(Kind::User(u)) if u.command == "task-notification" => Some(u.command_id.as_str()),
                _ => None,
            })
            .collect();
        delivered.sort_unstable();
        assert_eq!(delivered, ["task:bg1:finished", "task:bg2:finished"]);
    }

    #[tokio::test]
    async fn audit_group_counts_an_unlinked_native_task_failure() {
        audit_group_releases_on_a_failure_before_the_child_is_linked("task").await;
    }

    #[tokio::test]
    async fn audit_group_counts_an_unlinked_mcp_task_failure() {
        audit_group_releases_on_a_failure_before_the_child_is_linked("mcp__carbon6__Task").await;
    }

    async fn audit_group_keeps_a_completed_member_after_its_continuation(session_only: bool) {
        use session_event::Kind;
        let (_dir, manager, mock) = idle_session().await;
        let Kind::SubagentSpawned(mut a) = audit_spawn(1, "all") else { unreachable!() };
        a.background_task_id = "bg1".into();
        let mut continuation = a.clone();
        continuation.call_id = "continue-a".into();
        continuation.background_task_id = "bg3".into();
        continuation.group.clear();
        continuation.wake.clear();
        let (old_id, continued_id) =
            if session_only { ("subagent:original-a", "subagent:continuation-a") } else { ("task:bg1:finished", "task:bg3:finished") };
        let old = c6_proto::NoticeQueued {
            command_id: old_id.into(),
            command: "task-notification".into(),
            text: "original success".into(),
            wake: true,
            task_notice: Some(c6_proto::TaskNotice {
                session_id: if session_only { a.child_session_id.clone() } else { String::new() },
                status: "completed".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let continuation_notice = Kind::NoticeQueued(c6_proto::NoticeQueued {
            command_id: continued_id.into(),
            command: "task-notification".into(),
            text: "continuation failed".into(),
            // Hold for the next input so the final B notice and this report
            // can be checked together, including their different summaries.
            wake: false,
            task_notice: Some(c6_proto::TaskNotice {
                session_id: a.child_session_id.clone(),
                status: "failed".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let initial = append_seed(
            &manager.journal,
            1,
            [
                user("audit-request"),
                started_turn(1),
                task_calls(1, &[("call-1", "A"), ("call-2", "B")], Some("audit")),
                task_ack("call-1"),
                task_ack("call-2"),
                Kind::SubagentSpawned(a),
                audit_spawn(2, "all"),
                ended_turn(1),
                Kind::NoticeQueued(old.clone()),
                user("direct-input"),
                user(old_id),
                started_turn(2),
                Kind::SubagentSpawned(continuation),
                ended_turn(2),
                // Replaying a later scheduling snapshot cannot change where the
                // original session-only report belonged when it first arrived.
                Kind::NoticeQueued(c6_proto::NoticeQueued { reason: "later snapshot".into(), ..old }),
                continuation_notice,
            ],
        )
        .await;

        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw the final audit report.".into()));
        barrier(&manager, "continuation-notice-held").await;
        assert_waiting_after(&manager, &mock, 1, initial).await;
        enqueue_audit_reports(&manager, 2..=2, TurnEndReason::Completed).await;
        tokio::time::advance(TICK).await;
        let (events, request) = completed_batch_after(&manager, &mock, initial).await;
        assert!(request.contains("group audit: 2 of 2 finished, 0 failed"), "{request}");
        let delivered: Vec<_> = events
            .iter()
            .filter(|e| e.seq > initial)
            .filter_map(|e| match &e.kind {
                Some(Kind::User(u)) if u.command == "task-notification" => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(delivered.len(), 2);
        assert!(delivered.iter().any(|u| u.command_id == continued_id && u.text == "continuation failed"));
        assert!(delivered.iter().any(|u| u.command_id == "subagent:audit-child-2" && u.text.starts_with("group audit:")));
    }

    #[tokio::test]
    async fn audit_group_preserves_task_id_completion_across_a_continuation() {
        audit_group_keeps_a_completed_member_after_its_continuation(false).await;
    }

    #[tokio::test]
    async fn audit_group_preserves_session_only_completion_across_a_continuation() {
        audit_group_keeps_a_completed_member_after_its_continuation(true).await;
    }

    #[tokio::test]
    async fn repeated_failure_notifies_and_holds_until_limits_are_raised() {
        repeated_failure_hold(false).await;
    }

    #[tokio::test]
    async fn a_file_changed_on_the_deciding_wake_prevents_a_repetition_hold() {
        repeated_failure_hold(true).await;
    }

    async fn repeated_failure_hold(file_changed: bool) {
        use session_event::Kind;

        const DESCRIPTION: &str = "Run the same failing task";
        const PAUSED: &str =
            "paused: still repeating the same failure after relaunching a task (7 observations; source: Run the same failing task)";
        let (_dir, manager, mock) = idle_session().await;
        let (spec, _) = manager.journal.load_session("held").await.unwrap().unwrap();
        let checkpoints = Checkpoints::new(&manager.state_dir, "held", Path::new(&spec.cwd));
        let baseline = checkpoints.fingerprint().await.unwrap();
        let mut history = vec![Kind::Checkpoint(Checkpoint { tree_id: baseline, ..Default::default() }), user("direct-request")];
        for launch in 0..=8 {
            if launch != 0 {
                let command_id = format!("task:retry-{}:failed", launch - 1);
                let task_notice = Some(c6_proto::TaskNotice {
                    title: DESCRIPTION.into(),
                    status: "failed".into(),
                    markdown: "Same failure.".into(),
                    ..Default::default()
                });
                let text = format!("Failure wrapper from task {}.", launch - 1);
                history.push(Kind::NoticeQueued(c6_proto::NoticeQueued {
                    command_id: command_id.clone(),
                    task_notice: task_notice.clone(),
                    wake: true,
                    text: text.clone(),
                    ..Default::default()
                }));
                history.push(Kind::User(c6_proto::UserMessage { command_id, task_notice, text, ..Default::default() }));
            }
            let turn = launch + 1;
            let call = format!("retry-call-{launch}");
            let task = BackgroundTask {
                task_id: format!("retry-{launch}"),
                description: DESCRIPTION.into(),
                state: "started".into(),
                ..Default::default()
            };
            history.extend([
                started_turn(turn),
                task_calls(turn, &[(&call, DESCRIPTION)], None),
                task_ack(&call),
                Kind::BackgroundTask(task.clone()),
                ended_turn(turn),
                Kind::BackgroundTask(BackgroundTask { state: "failed".into(), exit_code: 1, ..task }),
            ]);
        }
        let initial = append_seed(&manager.journal, 1, history).await;
        if file_changed {
            // Model a shell write on the last completed wake: no Edit tool
            // event or subsequent start-of-turn checkpoint describes it.
            std::fs::write(Path::new(&spec.cwd).join("new-result"), "a genuinely changed file").unwrap();
            mock.push(c6_testkit::Reply::Text("The file change is new progress.".into()));
        }
        // Failed reports are eligible immediately. Keep real time for the
        // deciding workspace probe: this test checks the hold and its release,
        // not a merge or backoff deadline.
        let actor = manager.actor("held").await.unwrap().unwrap();
        actor
            .send(ActorMsg::ChildReport {
                command_id: "loop-trigger".into(),
                text: "report #1.".into(),
                task_notice: Some(c6_proto::TaskNotice {
                    title: DESCRIPTION.into(),
                    status: "failed".into(),
                    markdown: "Same failure.".into(),
                    ..Default::default()
                }),
                wake: true,
                recorded: tokio::sync::oneshot::channel().0,
                inline: None,
            })
            .unwrap();
        barrier(&manager, "still-paused").await;
        if file_changed {
            let (events, request) = completed_batch_in_real_time_after(&manager, &mock, initial).await;
            assert!(request.contains("report #1."));
            assert!(!events.iter().any(|event| matches!(&event.kind,
                Some(Kind::NoticeQueued(n)) if n.reason.starts_with("paused:")
            )));
            assert!(events.iter().any(|event| matches!(&event.kind,
                Some(Kind::Notice(n)) if n.quiet && n.title == crate::notice_guard::WORKSPACE_TREE_TITLE
            )));
            return;
        }
        assert_waiting_after(&manager, &mock, 1, initial).await;

        let events = manager.journal.events_after("held", initial, u32::MAX).await.unwrap();
        let latest = events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Some(Kind::NoticeQueued(n)) => Some(n),
                _ => None,
            })
            .unwrap();
        assert_eq!(latest.reason, PAUSED);
        assert!(c6_api::NoticeHoldReason::parse(&latest.reason).needs_you());
        assert_eq!(latest.deliver_at_ms, 0, "paused notices have no automatic delivery deadline");
        assert_eq!(
            events.iter().filter(|e| matches!(&e.kind, Some(Kind::Notice(n)) if !n.quiet && n.message.starts_with(PAUSED))).count(),
            1
        );

        mock.push(c6_testkit::Reply::Text("Delivered after the settings refresh.".into()));
        let tx = manager.actor("held").await.unwrap().unwrap();
        let limits = c6_proto::NoticeLimits {
            notice_quiet_seconds: 7,
            notice_max_wait_seconds: 41,
            notice_no_progress_limit: 8,
            notice_failure_relaunch_limit: 20,
            notice_tokens_per_hour: 7_000_000,
            notice_loop_backoff_rounds: 4,
            notice_loop_backoff_max_seconds: 200,
            max_subagent_depth: Some(4),
            max_running_subagents: Some(0),
            ..Default::default()
        };
        tx.send(ActorMsg::NoticeLimits(limits.clone())).unwrap();
        barrier(&manager, "limits-refreshed").await;
        let (events, request) = completed_batch_in_real_time_after(&manager, &mock, initial).await;
        assert!(request.contains("report #1."), "raising the threshold should deliver the held failure without Continue");
        let marker =
            events.iter().find(|e| matches!(&e.kind, Some(Kind::Notice(n)) if n.quiet && n.title == "Notice settings refreshed")).unwrap();
        let delivery = events.iter().find(|e| matches!(&e.kind, Some(Kind::User(u)) if u.text == "report #1.")).unwrap();
        assert!(marker.seq < delivery.seq, "the refreshed settings must be durable before automatic delivery");
        assert!(
            !events.iter().any(|e| matches!(&e.kind, Some(Kind::Notice(n)) if n.quiet && n.title == crate::notice_guard::CONTINUED_TITLE)),
            "refreshing limits must not masquerade as Continue or reset the guard"
        );

        let (spec, _) = manager.journal.load_session("held").await.unwrap().unwrap();
        assert_eq!(spec.notice_quiet_seconds, limits.notice_quiet_seconds);
        assert_eq!(spec.notice_max_wait_seconds, limits.notice_max_wait_seconds);
        assert_eq!(spec.notice_no_progress_limit, limits.notice_no_progress_limit);
        assert_eq!(spec.notice_failure_relaunch_limit, limits.notice_failure_relaunch_limit);
        assert_eq!(spec.notice_tokens_per_hour, limits.notice_tokens_per_hour);
        assert_eq!(spec.notice_loop_backoff_rounds, limits.notice_loop_backoff_rounds);
        assert_eq!(spec.notice_loop_backoff_max_seconds, limits.notice_loop_backoff_max_seconds);
        assert_eq!(spec.max_subagent_depth, 4);
        assert_eq!(spec.max_running_subagents, 0);
    }

    #[tokio::test]
    async fn loop_backoff_defers_automatic_delivery_and_warns_the_model() {
        use session_event::Kind;

        let (_dir, manager, mock) = idle_session().await;
        let make_notice = || c6_proto::TaskNotice {
            title: "same source".into(),
            status: "completed".into(),
            markdown: "The result is unchanged.".into(),
            ..Default::default()
        };
        let (spec, _) = manager.journal.load_session("held").await.unwrap().unwrap();
        let checkpoints = Checkpoints::new(&manager.state_dir, "held", Path::new(&spec.cwd));
        let mut history = vec![Kind::Checkpoint(Checkpoint { tree_id: checkpoints.fingerprint().await.unwrap(), ..Default::default() })];
        // The default warns on five matching observations. A sixth begins
        // the first backoff, regardless of how many notices the turn took.
        for turn in 1..=7 {
            let command_id = format!("duplicate-{turn}");
            history.extend([
                Kind::NoticeQueued(c6_proto::NoticeQueued {
                    command_id: command_id.clone(),
                    wake: true,
                    task_notice: Some(make_notice()),
                    ..Default::default()
                }),
                Kind::User(c6_proto::UserMessage { command_id, task_notice: Some(make_notice()), ..Default::default() }),
                started_turn(turn),
                ended_turn(turn),
            ]);
        }
        let initial = append_seed(&manager.journal, 1, history).await;
        // Wait for the deciding fingerprint on real time; a process scan can
        // need a retry while parallel tests exec Git. Freeze only once the
        // actor has recorded the backoff whose clock behavior we assert.
        let tx = manager.actor("held").await.unwrap().unwrap();
        tx.send(ActorMsg::ChildReport {
            command_id: "pending-duplicate".into(),
            text: "Report ready.".into(),
            task_notice: Some(make_notice()),
            wake: true,
            recorded: tokio::sync::oneshot::channel().0,
            inline: None,
        })
        .unwrap();
        let evidence = "near-identical reports from the same source (6 observations; source: same source)";
        let reason = format!("collecting reports; slowing automatic delivery after {evidence}");
        logged(&manager, |events| {
            events.iter().any(|e| matches!(&e.kind, Some(Kind::NoticeQueued(n)) if n.reason == reason && n.deliver_at_ms > 0))
        })
        .await;

        let _clock = pause_clock();
        tokio::time::advance(MAX_WAIT + TICK).await;
        assert_waiting_after(&manager, &mock, 1, initial).await;

        mock.push(c6_testkit::Reply::Text("No further action.".into()));
        tokio::time::advance(MAX_WAIT + TICK).await;
        // The deadline has elapsed. The next scheduling pass fingerprints
        // again, so let external I/O retries run while waiting for delivery.
        tokio::time::resume();
        let (events, request) = completed_batch_in_real_time_after(&manager, &mock, initial).await;
        assert!(request.contains(evidence), "the next model input should explain the warning");
        assert!(events.iter().any(|e| matches!(&e.kind, Some(Kind::User(u)) if u.text == "Report ready." && u.prompt.contains(evidence))));
    }

    #[test]
    fn notice_loop_warning_is_editable_without_changing_report_text() {
        let mut input = PendingInput { text: "Display report".into(), prompt: "Expanded report".into(), ..Default::default() };
        let spec = CreateSession {
            prompts_json: serde_json::json!({"session.notice_loop_warning": "Loop advisory: {{evidence}}"}).to_string(),
            ..Default::default()
        };
        add_notice_loop_warning(&spec, &mut input, "7 repeated results");
        assert_eq!(input.text, "Display report");
        assert_eq!(input.prompt, "Expanded report\n\nLoop advisory: 7 repeated results");
    }

    #[tokio::test]
    async fn a_burst_of_twenty_reports_starts_one_turn_after_the_quiet_window() {
        let (_dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw the batch.".into()));
        enqueue_reports(&manager, 1..21, true).await;
        assert_waiting(&manager, &mock, 20).await;
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        let mut latest = HashMap::new();
        let mut snapshots = 0;
        for event in &events {
            if let Some(session_event::Kind::NoticeQueued(n)) = &event.kind {
                snapshots += 1;
                latest.insert(&n.command_id, n);
            }
        }
        assert_eq!(latest.len(), 20);
        assert!(snapshots <= 4 * latest.len(), "live counts must not rewrite every full report on every arrival: {snapshots}");
        assert!(latest.values().all(|n| n.reason.starts_with("merging")), "the UI shows why every report is waiting");
        tokio::time::advance(QUIET - SECOND).await;
        barrier(&manager, "before-quiet").await;
        assert_waiting(&manager, &mock, 20).await;
        tokio::time::advance(SECOND + TICK).await;
        delivered(&manager, &mock, 20).await;
    }

    #[tokio::test]
    async fn a_new_report_restarts_the_quiet_window() {
        let (_dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw both reports.".into()));
        enqueue_reports(&manager, 1..2, true).await;
        tokio::time::advance(QUIET - SECOND).await;
        enqueue_reports(&manager, 2..3, true).await;
        tokio::time::advance(QUIET - SECOND).await;
        barrier(&manager, "past-original-deadline").await;
        assert_waiting(&manager, &mock, 2).await;
        tokio::time::advance(SECOND + TICK).await;
        delivered(&manager, &mock, 2).await;
    }

    #[tokio::test]
    async fn sustained_arrivals_cannot_push_delivery_past_the_maximum_wait() {
        let (_dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw the full batch.".into()));
        let interval = QUIET - SECOND;
        let count = ((MAX_WAIT - SECOND).as_secs() / interval.as_secs()) as usize + 1;
        for number in 1..=count {
            if number > 1 {
                tokio::time::advance(interval).await;
            }
            enqueue_reports(&manager, number..number + 1, true).await;
            assert_waiting(&manager, &mock, number).await;
        }
        // The last report arrived at t=28, so quiet alone would wait until t=31.
        tokio::time::advance(SECOND).await;
        barrier(&manager, "before-max-wait").await;
        assert_waiting(&manager, &mock, count).await;
        tokio::time::advance(SECOND + TICK).await;
        delivered(&manager, &mock, count).await;
    }

    #[tokio::test]
    async fn deliver_now_bypasses_the_merge_window_without_delivering_twice() {
        let (_dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        enqueue_reports(&manager, 1..3, true).await;
        assert_waiting(&manager, &mock, 2).await;
        mock.push(c6_testkit::Reply::Text("Saw them now.".into()));
        manager.actor("held").await.unwrap().unwrap().send(ActorMsg::ReleaseNotices).unwrap();
        delivered(&manager, &mock, 2).await;
        tokio::time::pause();
        tokio::time::advance(MAX_WAIT).await;
        barrier(&manager, "after-old-timer").await;
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::TurnStarted(_)))).count(), 1);
        assert_eq!(mock.requests().len(), 1, "the old timer doesn't deliver them twice");
    }

    #[tokio::test]
    async fn non_waking_notices_still_wait_for_explicit_input() {
        let (_dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        enqueue_reports(&manager, 1..3, false).await;
        tokio::time::advance(MAX_WAIT).await;
        barrier(&manager, "after-merge-windows").await;
        assert_waiting(&manager, &mock, 2).await;
        mock.push(c6_testkit::Reply::Text("Saw them when asked.".into()));
        manager.actor("held").await.unwrap().unwrap().send(ActorMsg::ReleaseNotices).unwrap();
        delivered(&manager, &mock, 2).await;
    }

    #[tokio::test]
    async fn non_waking_notices_join_the_next_waking_batch() {
        let (_dir, manager, mock) = idle_session().await;
        let _clock = pause_clock();
        enqueue_reports(&manager, 1..2, false).await;
        tokio::time::advance(MAX_WAIT).await;
        barrier(&manager, "non-waking-does-not-start-a-turn").await;
        assert_waiting(&manager, &mock, 1).await;
        mock.push(c6_testkit::Reply::Text("Saw both notices.".into()));
        enqueue_reports(&manager, 2..3, true).await;
        tokio::time::advance(QUIET + TICK).await;
        delivered(&manager, &mock, 2).await;
    }

    #[tokio::test]
    async fn recovery_reschedules_pending_notices_without_an_open_task() {
        let (_dir, manager, mock) = idle_session().await;
        seed_held(&manager).await;
        assert!(manager.journal.sessions_with_open_tasks().await.unwrap().is_empty());
        let _clock = pause_clock();
        mock.push(c6_testkit::Reply::Text("Saw them after restart.".into()));
        manager.recover().await.unwrap();
        assert_eq!(manager.active(), ["held"], "startup, not a new command, restores its actor");
        barrier(&manager, "reloaded").await;
        assert_waiting(&manager, &mock, 2).await;
        tokio::time::advance(QUIET + TICK).await;
        delivered(&manager, &mock, 2).await;
    }

    #[tokio::test]
    async fn recovery_leaves_old_held_notices_for_explicit_input() {
        let (dir, manager, mock) = idle_session().await;
        seed_held(&manager).await;
        // The log's events are recent: it is last_activity, not event
        // timestamps, that bounds the SQL scan at startup.
        let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("journal.db").display())).await.unwrap();
        sqlx::query("UPDATE sessions SET last_activity = ? WHERE id = 'held'")
            .bind(c6_core::now_ms() - NOTICE_RECOVERY_WINDOW_MS - 1000)
            .execute(&db)
            .await
            .unwrap();
        db.close().await;
        let _clock = pause_clock();
        manager.recover().await.unwrap();
        assert!(manager.active().is_empty(), "startup must not restore an old session's actor");
        tokio::time::advance(MAX_WAIT).await;
        assert_waiting(&manager, &mock, 2).await;

        // Deliver now still loads the actor and delivers the batch.
        mock.push(c6_testkit::Reply::Text("Saw the old notices when asked.".into()));
        manager.actor("held").await.unwrap().unwrap().send(ActorMsg::ReleaseNotices).unwrap();
        delivered(&manager, &mock, 2).await;
    }

    async fn seed_held(manager: &SessionManager) {
        // Seed held notices without starting an actor, exactly as a fresh
        // worker finds them on disk after its predecessor exited.
        let now = c6_core::now_ms();
        for number in 1..=2 {
            let text = format!("report #{number}.");
            let notice = c6_proto::NoticeQueued {
                command_id: format!("held{number}"),
                wake: true,
                command: "task-notification".into(),
                task_notice: Some(crate::subagent::task_report_notice("Review", "child", TurnEndReason::Completed, &text)),
                text,
                ..Default::default()
            };
            let seq = 2 * number;
            let first = ev(seq, now, session_event::Kind::NoticeQueued(notice.clone()));
            let latest =
                ev(seq + 1, now, session_event::Kind::NoticeQueued(c6_proto::NoticeQueued { reason: "merging reports".into(), ..notice }));
            manager.journal.append("held", &first).await.unwrap();
            manager.journal.append("held", &latest).await.unwrap();
        }
    }

    async fn barrier(manager: &SessionManager, id: &str) {
        // A recorded message after an enqueue or reload confirms that the
        // actor has handled that message before the test advances the clock.
        let tx = manager.actor("held").await.unwrap().unwrap();
        tx.send(ActorMsg::SetSubagentModel { command_id: id.into(), model: None }).unwrap();
        logged(manager, |events| {
            events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::SubagentModelSet(m)) if m.command_id == id))
        })
        .await;
    }

    #[tokio::test]
    async fn overdue_notices_wait_for_the_head_after_restart() {
        let (_dir, mut manager, mock) = idle_session().await;
        seed_held(&manager).await;
        let head = manager.link.clone();
        manager.link = Arc::new(HeadLink::default());
        let _clock = pause_clock();
        manager.recover().await.unwrap();
        barrier(&manager, "reloaded").await;
        tokio::time::advance(MAX_WAIT).await;
        barrier(&manager, "still-offline").await;
        assert_waiting(&manager, &mock, 2).await;

        mock.push(c6_testkit::Reply::Text("Saw them once connected.".into()));
        let (tx, mut rx) = mpsc::channel::<c6_proto::WorkerMsg>(32);
        let link = manager.link.clone();
        link.connected(tx);
        let _head = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Some(c6_proto::worker_msg::Msg::CredentialRequest(req)) = msg.msg {
                    let mut grant = head.session_credential("held", &req.connection_id).await.unwrap();
                    grant.request_id = req.request_id;
                    link.on_grant(grant);
                }
            }
        }));
        delivered(&manager, &mock, 2).await;
    }
}

#[cfg(test)]
mod handoff_tests {
    use c6_proto::{AssistantMessage, ContentBlock, Handoff, TurnStarted, Usage, UserMessage, content_block};

    use super::*;

    fn ev(seq: u64, kind: session_event::Kind) -> SessionEvent {
        SessionEvent { seq, ts_ms: 0, kind: Some(kind) }
    }
    fn model(connection: &str, name: &str, kind: &str) -> ModelSpec {
        ModelSpec { connection_id: connection.into(), model: name.into(), connection_kind: kind.into(), ..Default::default() }
    }
    fn user(seq: u64, t: &str) -> SessionEvent {
        ev(seq, session_event::Kind::User(UserMessage { text: t.into(), human_origin: true, ..Default::default() }))
    }

    #[test]
    fn rewind_accepts_legacy_human_and_unknown_messages_without_nonhuman_provenance() {
        for (origin_recorded, human_origin) in [(false, false), (false, true), (true, false), (true, true)] {
            let message = UserMessage { origin_recorded, human_origin, ..Default::default() };
            assert!(is_rewindable_message(&ev(1, session_event::Kind::User(message.clone()))));
            for nonhuman in [
                UserMessage { api_origin: Some(Default::default()), ..message.clone() },
                UserMessage { from_session_id: "other-session".into(), ..message.clone() },
                UserMessage { delegated_by: "call".into(), ..message.clone() },
                UserMessage { command: "task-notification".into(), ..message.clone() },
                UserMessage { command: "stop-hook".into(), ..message.clone() },
            ] {
                assert!(!is_rewindable_message(&ev(1, session_event::Kind::User(nonhuman))), "nonhuman provenance wins");
            }
        }
        assert!(!is_rewindable_message(&ev(1, session_event::Kind::Assistant(Default::default()))));
        assert!(!is_rewindable_message(&SessionEvent::default()));
    }

    #[tokio::test]
    async fn claude_rewind_mapping_survives_reload_and_missing_metadata_never_resumes_old_history() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let spec = CreateSession::default();
        let started = ev(1, session_event::Kind::Started(SessionStarted::default()));
        journal.create_session("s", &spec, &started).await.unwrap();
        let mut events = vec![
            started,
            user(2, "APPLE"),
            user(3, "BANANA"),
            ev(4, session_event::Kind::Rewound(Rewound { command_id: "r".into(), to_seq: 3, ..Default::default() })),
        ];
        let old = journal_claude_conversation(&journal, "s", &spec, &events).await.unwrap();
        assert!(!old.resume);
        assert_ne!(old.id, "s");
        let preamble = old.preamble.unwrap();
        assert!(preamble.contains("APPLE") && !preamble.contains("BANANA"), "{preamble}");

        let reset = crate::claude_code::Reset {
            command_id: "r".into(),
            to_seq: 3,
            restore_files: false,
            session_id: "fork".into(),
            fresh: false,
            withdrawn: Vec::new(),
            ..Default::default()
        };
        journal.prepare_claude_rewind("s", &reset).await.unwrap();
        assert!(journal_claude_conversation(&journal, "s", &spec, &events).await.is_err(), "pending resets must fail closed");
        for e in &events[1..] {
            journal.append("s", e).await.unwrap();
        }
        drop(journal);
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let fork = journal_claude_conversation(&journal, "s", &spec, &events).await.unwrap();
        assert!(fork.resume);
        assert_eq!(fork.id, "fork");
        journal
            .record_claude_turn(
                "s",
                5,
                &crate::claude_code::Boundary {
                    session_id: "returned-fork-id".into(),
                    input_uuids: vec!["top-level-user".into()],
                    cutoff_uuid: "final-assistant".into(),
                },
            )
            .await
            .unwrap();
        events.push(ev(5, session_event::Kind::TurnEnded(TurnEnded { turn: 2, reason: TurnEndReason::Completed as i32 })));
        let next = journal_claude_conversation(&journal, "s", &spec, &events).await.unwrap();
        assert_eq!(next.id, "returned-fork-id");
        assert!(next.resume);
        assert_eq!(journal.claude_turn("s", 5).await.unwrap().unwrap().input_uuids, ["top-level-user"]);
    }

    #[tokio::test]
    async fn claude_move_pins_the_rewound_branch_without_source_journal_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let spec = CreateSession::default();
        let started = ev(1, session_event::Kind::Started(SessionStarted::default()));
        journal.create_session("s", &spec, &started).await.unwrap();
        let mut events = vec![
            started,
            user(2, "kept"),
            user(3, "withdrawn"),
            ev(4, session_event::Kind::Rewound(Rewound { command_id: "r".into(), to_seq: 3, ..Default::default() })),
            ev(
                5,
                session_event::Kind::PlaceChanged(c6_proto::PlaceChanged { native_resume_id: "moved-fork".into(), ..Default::default() }),
            ),
        ];
        let moved = journal_claude_conversation(&journal, "s", &spec, &events).await.unwrap();
        assert_eq!(moved.id, "moved-fork");
        assert!(moved.resume);

        events.push(user(6, "new input"));
        events.push(ev(7, session_event::Kind::Rewound(Rewound { command_id: "r2".into(), to_seq: 6, ..Default::default() })));
        let later = journal_claude_conversation(&journal, "s", &spec, &events).await.unwrap();
        assert_ne!(later.id, moved.id);
        assert!(!later.resume, "an unmapped rewind after the move must not resume the transferred branch");
    }

    #[tokio::test]
    async fn fresh_reset_applies_prior_rewinds_before_a_historical_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let spec = CreateSession::default();
        let started = ev(1, session_event::Kind::Started(SessionStarted::default()));
        journal.create_session("s", &spec, &started).await.unwrap();
        let events = vec![
            started,
            user(2, "APPLE"),
            user(3, "PRIVATE_WITHDRAWN"),
            user(4, "HISTORICAL_TARGET"),
            ev(5, session_event::Kind::Rewound(Rewound { command_id: "r1".into(), to_seq: 3, ..Default::default() })),
            ev(6, session_event::Kind::Rewound(Rewound { command_id: "r2".into(), to_seq: 4, ..Default::default() })),
        ];
        journal
            .prepare_claude_rewind(
                "s",
                &crate::claude_code::Reset {
                    command_id: "r2".into(),
                    to_seq: 4,
                    restore_files: false,
                    session_id: "fresh".into(),
                    fresh: true,
                    withdrawn: Vec::new(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for event in &events[1..] {
            journal.append("s", event).await.unwrap();
        }
        let context = journal_claude_conversation(&journal, "s", &spec, &events).await.unwrap();
        assert!(!context.resume);
        let preamble = context.preamble.unwrap();
        assert!(preamble.contains("APPLE") && !preamble.contains("PRIVATE") && !preamble.contains("HISTORICAL_TARGET"), "{preamble}");
    }

    #[test]
    fn late_notices_from_withdrawn_tasks_and_children_are_not_new_context() {
        let events = vec![
            ev(
                1,
                session_event::Kind::BackgroundTask(BackgroundTask {
                    task_id: "kept".into(),
                    state: "started".into(),
                    ..Default::default()
                }),
            ),
            user(2, "withdraw this turn"),
            ev(
                3,
                session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned { child_session_id: "child".into(), ..Default::default() }),
            ),
            ev(
                4,
                session_event::Kind::BackgroundTask(BackgroundTask {
                    task_id: "bg1".into(),
                    state: "started".into(),
                    ..Default::default()
                }),
            ),
            ev(5, session_event::Kind::Rewound(Rewound { to_seq: 2, ..Default::default() })),
        ];
        let sources = withdrawn_sources(&events);
        assert_eq!(sources, HashSet::from(["child".into(), "bg1".into()]));
        assert!(withdrawn_input(&sources, &PendingInput { from_session_id: "child".into(), ..Default::default() }));
        assert!(withdrawn_input(&sources, &PendingInput { command_id: "task:bg1:finished".into(), ..Default::default() }));
        assert!(withdrawn_input(
            &sources,
            &PendingInput {
                task_notice: Some(c6_proto::TaskNotice { session_id: "child".into(), ..Default::default() }),
                ..Default::default()
            }
        ));
        assert!(!withdrawn_input(&sources, &PendingInput { command_id: "task:kept:finished".into(), ..Default::default() }));
    }

    #[tokio::test]
    async fn rewind_withdraws_held_native_wake_by_full_command_id_and_persists_it() {
        let wake = "task:claude-wake:0199-not-a-task-id";
        let events = vec![
            ev(1, session_event::Kind::Started(SessionStarted::default())),
            user(2, "withdraw the task"),
            ev(
                3,
                session_event::Kind::NoticeQueued(c6_proto::NoticeQueued {
                    command_id: wake.into(),
                    text: "withdrawn native task output".into(),
                    task_notice: Some(c6_proto::TaskNotice { kind: "task".into(), ..Default::default() }),
                    ..Default::default()
                }),
            ),
            ev(4, session_event::Kind::Rewound(Rewound { command_id: "rewind".into(), to_seq: 2, ..Default::default() })),
        ];
        let sources = withdrawn_sources(&events);
        assert!(sources.contains(wake), "the synthetic wake has no task/session provenance; retain its full command ID");
        let input = PendingInput { command_id: wake.into(), text: "withdrawn native task output".into(), ..Default::default() };
        assert!(withdrawn_input(&sources, &input));
        let mut held = vec![(input.clone(), true)];
        held.retain(|(input, _)| !withdrawn_input(&sources, input));
        assert!(held.is_empty(), "the next user input must not release the withdrawn wake");
        assert!(!withdrawn_input(&sources, &PendingInput { command_id: "task:claude-wake:new".into(), ..Default::default() }));

        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let reset = crate::claude_code::Reset {
            command_id: "rewind".into(),
            to_seq: 2,
            restore_files: false,
            session_id: "fresh".into(),
            fresh: true,
            withdrawn: sources.into_iter().collect(),
            ..Default::default()
        };
        journal.prepare_claude_rewind("s", &reset).await.unwrap();
        drop(journal);
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let (reset, _) = journal.claude_rewind("s").await.unwrap().unwrap();
        assert!(withdrawn_input(&reset.withdrawn.into_iter().collect(), &input), "worker restart must retain suppression");
    }

    fn rewind_child_events() -> Vec<SessionEvent> {
        vec![
            ev(1, session_event::Kind::Started(SessionStarted::default())),
            ev(2, session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })),
            ev(3, session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: TurnEndReason::Completed as i32 })),
        ]
    }

    fn rewind_child_link(id: &str) -> SessionEvent {
        ev(4, session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned { child_session_id: id.into(), ..Default::default() }))
    }

    async fn rewind_test_manager() -> (tempfile::TempDir, SessionManager) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let manager = SessionManager::new(journal, Arc::new(HeadLink::default()), dir.path().join("worktrees"));
        (dir, manager)
    }

    #[tokio::test]
    async fn same_worker_placement_commits_once_and_reloads_the_current_place() {
        let (dir, manager) = rewind_test_manager().await;
        let scratch = dir.path().join("scratch/s");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(scratch.join("keep.txt"), "stays here").unwrap();
        let started = SessionStarted { cwd: scratch.canonicalize().unwrap().display().to_string(), ..Default::default() };
        let spec = CreateSession::default();
        manager.journal.create_session("s", &spec, &ev(1, session_event::Kind::Started(started))).await.unwrap();
        let request = |id: &str, path: &Path| c6_proto::SessionCommand {
            command_id: id.into(),
            session_id: "s".into(),
            ownership_epoch: 0,
            kind: Some(session_command::Kind::ChangePlace(c6_proto::ChangePlace {
                expected_place_seq: 1,
                cwd: path.display().to_string(),
                notice_template: "Test prompt: {{from}} to {{to}}".into(),
                copy_scratch: false,
                ..Default::default()
            })),
        };
        let before = manager.load("s").await.unwrap().unwrap();
        assert!(manager.handle(request("bad", &dir.path().join("missing"))).await.is_err());
        assert_eq!(before.tools.cwd(), scratch.canonicalize().unwrap());
        for _ in 0..2 {
            manager.handle(request("good", &destination)).await.unwrap();
        }
        assert!(manager.handle(request("stale", &destination)).await.unwrap_err().contains("refresh"));
        let events = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
        let [_, change] = events.as_slice() else { panic!("expected one placement event") };
        let Some(session_event::Kind::PlaceChanged(p)) = &change.kind else { panic!("expected placement") };
        assert!(p.notice.starts_with("Test prompt: "));
        assert_eq!(before.tools.cwd(), destination.canonicalize().unwrap());
        assert!(scratch.join("keep.txt").is_file());
        assert!(!destination.join("keep.txt").exists());
        let reloaded = SessionManager::new(manager.journal.clone(), Arc::new(HeadLink::default()), dir.path().join("worktrees"));
        assert_eq!(reloaded.load("s").await.unwrap().unwrap().tools.cwd(), destination.canonicalize().unwrap());
    }

    async fn rewind_actor_barrier(manager: &SessionManager, id: &str) {
        manager.actor("s").await.unwrap().unwrap().send(ActorMsg::SetSubagentModel { command_id: id.into(), model: None }).unwrap();
        c6_testkit::eventually("rewind actor mailbox barrier", async || {
            manager
                .journal
                .events_after("s", 0, u32::MAX)
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(&e.kind, Some(session_event::Kind::SubagentModelSet(m)) if m.command_id == id))
                .then_some(())
        })
        .await;
    }

    /// No global CLI override: an accidentally replaced/adopted wrong process
    /// must fail this fixture, rather than quietly spawning another fake.
    fn rewind_held_fixture(dir: &Path) -> (CreateSession, SessionStarted, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let spec = CreateSession {
            model: Some(model("cc", "opus", "claude_code")),
            claude_code: Some(c6_proto::ClaudeCodeToolChoice { automatic: false, tools: vec![], ..Default::default() }),
            ..Default::default()
        };
        let started = SessionStarted {
            cwd: dir.display().to_string(),
            home: dir.display().to_string(),
            tool_profile: crate::claude_code::PROFILE.into(),
            model: spec.model.clone(),
            ..Default::default()
        };
        let bin = dir.join("claude-rewind-held");
        std::fs::write(&bin, r#"#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
conversation = args[args.index('--resume') + 1] if '--resume' in args else args[args.index('--session-id') + 1]
def trace(v):
    with open('held-trace.jsonl', 'a') as f:
        f.write(json.dumps(dict(conversation=conversation, **v)) + '\n')
def emit(v):
    print(json.dumps(v), flush=True)
trace(dict(kind='spawn'))
for line in sys.stdin:
    v = json.loads(line)
    trace(dict(kind='input', value=v))
    if v['type'] == 'control_request':
        emit(dict(type='control_response', response=dict(subtype='success', request_id=v['request_id'])))
    elif v['type'] == 'user':
        emit(dict(type='user', isReplay=True, uuid=v['uuid'], message=v['message'], parent_tool_use_id=None))
        emit(dict(type='assistant', message=dict(content=[dict(type='text', text=conversation)], stop_reason='end_turn'), parent_tool_use_id=None))
        emit(dict(type='result', subtype='success', is_error=False, usage={}))
"#).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (spec, started, bin)
    }

    /// Leaves a held runtime behind, as a worker that went away would, once
    /// it can be proven ours (a worker waits for `ready` before anything
    /// could stop it; under load a stop right after the spawn could not).
    async fn leave_rewind_held(manager: &SessionManager, spec: &CreateSession, bin: &Path, cwd: &Path, conversation: &str) {
        let launch = crate::claude_code::Launch {
            conversation: conversation.into(),
            settings: Some(claude_code_settings(spec, None)),
            worker: c6_core::build().to_owned(),
            native: claude_code_native(spec),
            ..Default::default()
        };
        drop(
            crate::claude_code::Process::spawn_held(
                &manager.claude_dir().join("held"),
                "s",
                bin,
                launch,
                true,
                cwd,
                "opus",
                Arc::default(),
                None,
                None,
            )
            .unwrap(),
        );
        let held = manager.claude_dir().join("held");
        c6_testkit::eventually("the held runtime's CLI or keeper in its final image", async || {
            crate::claude_code::held_record_owned(&held, conversation).then_some(())
        })
        .await;
    }

    fn held_trace(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("held-trace.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn metadata_loss_cleanup_serializes_pending_publication_and_retry_authorization() {
        if !c6_testkit::have("python3") {
            return;
        }
        for publication_first in [true, false] {
            let (dir, manager) = rewind_test_manager().await;
            let held = manager.claude_dir().join("held");
            let _cleanup = crate::claude_code::tests::HeldFixtureCleanup(held.clone());
            let (spec, started, bin) = rewind_held_fixture(dir.path());
            let events = [
                ev(1, session_event::Kind::Started(started)),
                user(2, "kept"),
                user(3, "removed"),
                ev(4, session_event::Kind::Rewound(Rewound { command_id: "completed".into(), to_seq: 3, ..Default::default() })),
            ];
            manager.journal.import("s", &spec, &events).await.unwrap();
            leave_rewind_held(&manager, &spec, &bin, dir.path(), "s").await;
            c6_testkit::eventually("the unselected held fixture to start", async || {
                held_trace(dir.path()).iter().any(|v| v["kind"] == "spawn").then_some(())
            })
            .await;
            let meta_path = held.join("s/meta.json");
            let meta_before = std::fs::read(&meta_path).unwrap();
            let meta: serde_json::Value = serde_json::from_slice(&meta_before).unwrap();
            let pg = nix::unistd::Pid::from_raw(meta["pgid"].as_i64().unwrap() as i32);
            assert!(nix::sys::signal::killpg(pg, None).is_ok());
            // Exactly the stale selection that previously authorized cleanup:
            // public Rewound, missing mapping, and no selected held record.
            let selected = journal_claude_conversation(&manager.journal, "s", &spec, &events).await.unwrap();
            assert!(!selected.resume);
            assert!(crate::claude_code::Process::held_record(&held, &selected.id).unwrap().is_none());
            let reset = crate::claude_code::Reset {
                command_id: "new-pending".into(),
                to_seq: 2,
                session_id: "never-initialized".into(),
                untouched: true,
                ..Default::default()
            };
            let ownership = manager.journal.claude_rewind_ownership.lock().await;
            let publish = async {
                manager.journal.prepare_claude_rewind("s", &reset).await?;
                if !publication_first {
                    assert_eq!(
                        nix::sys::signal::killpg(pg, None),
                        Err(nix::errno::Errno::ESRCH),
                        "a new untouched intent must not publish until destructive cleanup confirms absence"
                    );
                }
                Ok::<(), anyhow::Error>(())
            };
            let authorize = manager.journal.authorize_claude_rewind_retry("s", "new-pending", "retry");
            let cleanup = manager.stop_unmapped_claude("s", &events);
            tokio::pin!(publish, authorize, cleanup);
            // Poll once while the gate is held to enqueue these real paths in
            // a deterministic FIFO order, without sleeps or a global hook.
            std::future::poll_fn(|cx| {
                if !publication_first {
                    assert!(std::future::Future::poll(cleanup.as_mut(), cx).is_pending());
                }
                assert!(std::future::Future::poll(publish.as_mut(), cx).is_pending());
                assert!(std::future::Future::poll(authorize.as_mut(), cx).is_pending());
                if publication_first {
                    assert!(std::future::Future::poll(cleanup.as_mut(), cx).is_pending());
                }
                std::task::Poll::Ready(())
            })
            .await;
            assert!(manager.journal.claude_rewind("s").await.unwrap().is_none());
            assert_eq!(std::fs::read(&meta_path).unwrap(), meta_before);
            drop(ownership);
            let (published, authorized, cleaned) = tokio::join!(publish, authorize, cleanup);
            published.unwrap();
            assert!(authorized.unwrap());
            if publication_first {
                assert!(cleaned.unwrap_err().to_string().contains("not confirmed"));
                assert!(nix::sys::signal::killpg(pg, None).is_ok(), "untouched pending intent must preserve the owned runtime");
                assert_eq!(std::fs::read(&meta_path).unwrap(), meta_before, "no destructive cleanup beneath an untouched intent");
                assert!(manager.journal.claude_processes("s").await.unwrap().is_empty(), "cleanup must not even track/signal the group");
            } else {
                cleaned.unwrap();
                assert_eq!(nix::sys::signal::killpg(pg, None), Err(nix::errno::Errno::ESRCH));
                assert!(!meta_path.exists(), "cleanup must finish before pending publication can proceed");
            }
            let (pending, applied) = manager.journal.claude_rewind("s").await.unwrap().unwrap();
            assert_eq!(applied, 0);
            let mut expected = reset.clone();
            expected.retries.push("retry".into());
            assert_eq!(serde_json::to_value(pending).unwrap(), serde_json::to_value(expected).unwrap());
            assert_eq!(manager.journal.events_after("s", 0, u32::MAX).await.unwrap().len(), events.len());
            assert!(held_trace(dir.path()).iter().all(|v| v["kind"] == "spawn"), "neither ordering may initialize/send/retry");
            crate::claude_code::stop_held_for_rewind(&held, "s", &manager.journal).await.unwrap();
        }
    }

    #[tokio::test]
    async fn confirmed_rewind_restart_adopts_only_the_fork_and_deduplicates_input() {
        if !c6_testkit::have("python3") {
            return;
        }
        let (dir, manager) = rewind_test_manager().await;
        let _cleanup = crate::claude_code::tests::HeldFixtureCleanup(manager.claude_dir().join("held"));
        let (spec, started, bin) = rewind_held_fixture(dir.path());
        let events = [
            ev(1, session_event::Kind::Started(started)),
            user(2, "kept"),
            user(3, "removed"),
            ev(4, session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })),
            ev(5, session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: TurnEndReason::WorkerRestarted as i32 })),
        ];
        manager.journal.import("s", &spec, &events).await.unwrap();
        let fork = uuid::Uuid::now_v7().to_string();
        manager
            .journal
            .prepare_claude_rewind(
                "s",
                &crate::claude_code::Reset { command_id: "confirmed".into(), to_seq: 3, session_id: fork.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        manager
            .journal
            .append("s", &ev(6, session_event::Kind::Rewound(Rewound { command_id: "confirmed".into(), to_seq: 3, ..Default::default() })))
            .await
            .unwrap();
        leave_rewind_held(&manager, &spec, &bin, dir.path(), "s").await;
        leave_rewind_held(&manager, &spec, &bin, dir.path(), &fork).await;
        let held = manager.claude_dir().join("held");
        let (fork_record, _) = crate::claude_code::Process::held_record(&held, &fork).unwrap().unwrap();
        // Two valid records claiming the selected fork must fail closed.
        let conflicting = held.join("conflicting-record");
        std::fs::create_dir(&conflicting).unwrap();
        std::fs::copy(fork_record.join("meta.json"), conflicting.join("meta.json")).unwrap();
        let reopened = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let manager = SessionManager::new(reopened, Arc::default(), dir.path().join("worktrees"));
        manager.recover().await.unwrap();
        rewind_actor_barrier(&manager, "adoption-paused").await;
        let paused = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
        assert!(
            paused.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Error(e)) if e.message.contains("Multiple held runtimes")))
        );
        let evidence = manager.liveness_report().sessions.into_iter().find(|s| s.session_id == "s").unwrap();
        assert_eq!(evidence.actor_state, c6_proto::SessionActorState::Recovering as i32);
        assert!(evidence.reason.contains("ownership of the held Claude Code runtime"));
        assert!(!manager.actor("s").await.unwrap().unwrap().is_closed());
        manager.actor("s").await.unwrap().unwrap().send(ActorMsg::Resume(1)).unwrap();
        rewind_actor_barrier(&manager, "stale-resume").await;
        let command = || SessionCommand {
            session_id: "s".into(),
            command_id: "after-rewind".into(),
            ownership_epoch: 0,
            kind: Some(session_command::Kind::Input(c6_proto::UserInput {
                text: "display-only".into(),
                prompt: "model-only".into(),
                human_origin: true,
                ..Default::default()
            })),
        };
        manager.handle(command()).await.unwrap();
        rewind_actor_barrier(&manager, "queued-while-adoption-paused").await;
        assert!(conflicting.exists());
        assert!(held_trace(dir.path()).iter().all(|v| v["kind"] == "spawn"));
        std::fs::remove_dir_all(&conflicting).unwrap();
        // No command or mailbox barrier after removing the obstacle. The
        // adoption-only retry wake must itself release the pre-existing queue.
        c6_testkit::eventually("adoption retry wakes and completes the already queued input", async || {
            let raw = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
            let input = raw.iter().find(|e| matches!(&e.kind, Some(session_event::Kind::User(u)) if u.command_id == "after-rewind"))?;
            raw.iter().any(|e| e.seq > input.seq && matches!(e.kind, Some(session_event::Kind::TurnEnded(_)))).then_some(())
        })
        .await;
        manager.handle(command()).await.unwrap();
        rewind_actor_barrier(&manager, "redelivered").await;
        let raw = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
        assert_eq!(
            raw.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::User(u)) if u.command_id == "after-rewind")).count(),
            1
        );
        assert!(
            !raw.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::NoticeQueued(n)) if n.command_id == "resume-after-restart-1"))
        );
        let trace = held_trace(dir.path());
        let inputs = trace.iter().filter(|v| v["kind"] == "input").collect::<Vec<_>>();
        assert_eq!(inputs.len(), 1, "no implicit Resume, initialize, or duplicate input");
        assert_eq!(inputs[0]["conversation"].as_str(), Some(fork.as_str()));
        assert!(inputs[0]["value"]["message"]["content"].to_string().contains("model-only"), "PendingInput.prompt reaches the CLI");
        assert!(!inputs[0]["value"]["message"]["content"].to_string().contains("display-only"));
        assert_eq!(trace.iter().filter(|v| v["kind"] == "spawn" && v["conversation"].as_str() == Some(fork.as_str())).count(), 1);
        // Only fixture-owned records are in this private held root.
        crate::claude_code::stop_held_for_rewind(&manager.claude_dir().join("held"), "s", &manager.journal).await.unwrap();
    }

    #[tokio::test]
    async fn failed_pending_rewind_restart_cleans_keepers_without_retry_and_keeps_inbox() {
        if !c6_testkit::have("python3") {
            return;
        }
        let (dir, manager) = rewind_test_manager().await;
        let _cleanup = crate::claude_code::tests::HeldFixtureCleanup(manager.claude_dir().join("held"));
        let (spec, started, bin) = rewind_held_fixture(dir.path());
        manager
            .journal
            .import(
                "s",
                &spec,
                &[
                    ev(1, session_event::Kind::Started(started)),
                    user(2, "target"),
                    ev(3, session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })),
                ],
            )
            .await
            .unwrap();
        let reset = crate::claude_code::Reset {
            command_id: "failed".into(),
            to_seq: 2,
            session_id: "reset-attempt".into(),
            failure: "explicit new retry required".into(),
            untouched: false,
            ..Default::default()
        };
        manager.journal.prepare_claude_rewind("s", &reset).await.unwrap();
        leave_rewind_held(&manager, &spec, &bin, dir.path(), "s").await;
        leave_rewind_held(&manager, &spec, &bin, dir.path(), "reset-attempt").await;
        let reopened = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let manager = SessionManager::new(reopened, Arc::default(), dir.path().join("worktrees"));
        manager.recover().await.unwrap();
        let handle = manager.load("s").await.unwrap().unwrap();
        rewind_actor_barrier(&manager, "failed-recovered").await;
        handle.tx.send(ActorMsg::Resume(1)).unwrap();
        manager
            .handle(SessionCommand {
                session_id: "s".into(),
                command_id: "queued".into(),
                ownership_epoch: 0,
                kind: Some(session_command::Kind::Input(c6_proto::UserInput {
                    text: "must wait".into(),
                    human_origin: true,
                    ..Default::default()
                })),
            })
            .await
            .unwrap();
        let rejection = manager
            .handle(SessionCommand {
                session_id: "s".into(),
                command_id: "failed".into(),
                ownership_epoch: 0,
                kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq: 2, restore_files: false })),
            })
            .await
            .unwrap_err();
        assert!(rejection.contains("explicit") || rejection.contains("retry"), "{rejection}");
        rewind_actor_barrier(&manager, "failed-redelivered").await;
        assert!(!handle.tx.is_closed());
        assert!(matches!(manager.queue_op("s", "queued", None), QueueOp::Cancelled));
        assert!(matches!(manager.queue_op("s", "queued", None), QueueOp::Absent));
        let (pending, applied) = manager.journal.claude_rewind("s").await.unwrap().unwrap();
        assert_eq!(applied, 0);
        assert_eq!(serde_json::to_value(pending).unwrap(), serde_json::to_value(reset).unwrap());
        let raw = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
        assert!(!raw.iter().any(|e| matches!(e.kind, Some(session_event::Kind::Rewound(_) | session_event::Kind::NoticeQueued(_)))));
        assert!(!raw.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::User(u)) if u.command_id == "queued")));
        assert!(held_trace(dir.path()).iter().all(|v| v["kind"] == "spawn"), "cleanup must never initialize/send/retry");
        assert!(crate::claude_code::Process::held_sessions(&manager.claude_dir().join("held")).is_empty());
        assert!(manager.journal.claude_processes("s").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rewind_preflight_failure_restores_prior_branch_and_receipt_across_reload() {
        let (dir, manager) = rewind_test_manager().await;
        let events = [ev(1, session_event::Kind::Started(SessionStarted::default())), user(2, "kept"), user(3, "already withdrawn")];
        manager.journal.import("s", &CreateSession::default(), &events).await.unwrap();
        let confirmed = crate::claude_code::Reset {
            command_id: "confirmed".into(),
            to_seq: 3,
            session_id: "confirmed-native-branch".into(),
            fresh: false,
            withdrawn: vec!["prior-withdrawn-child".into()],
            ..Default::default()
        };
        manager.journal.prepare_claude_rewind("s", &confirmed).await.unwrap();
        manager
            .journal
            .append("s", &ev(4, session_event::Kind::Rewound(Rewound { command_id: "confirmed".into(), to_seq: 3, ..Default::default() })))
            .await
            .unwrap();
        let before = manager.journal.claude_rewind("s").await.unwrap().unwrap();
        let pending = crate::claude_code::Reset {
            command_id: "refused".into(),
            to_seq: 2,
            session_id: "never-started-branch".into(),
            untouched: true,
            previous: Some(Box::new(before.clone())),
            ..Default::default()
        };
        manager.journal.prepare_claude_rewind("s", &pending).await.unwrap();
        let failure =
            ev(5, session_event::Kind::Error(ErrorEvent { message: "preflight refused; existing conversation unchanged".into() }));
        assert!(!manager.journal.fail_claude_rewind("s", "refused", &failure, &[]).await.unwrap().0);
        let reopened = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        assert_eq!(
            serde_json::to_value(reopened.claude_rewind("s").await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert!(reopened.is_applied("refused").await.unwrap());
        assert_eq!(
            reopened.command_rejection("refused").await.unwrap().as_deref(),
            Some("preflight refused; existing conversation unchanged")
        );
        let events = reopened.events_after("s", 0, u32::MAX).await.unwrap();
        let conversation = journal_claude_conversation(&reopened, "s", &CreateSession::default(), &events).await.unwrap();
        assert_eq!(conversation.id, "confirmed-native-branch");
        assert!(conversation.resume);
        assert!(conversation.preamble.unwrap().contains("kept"));
        assert!(!events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Rewound(r)) if r.command_id == "refused")));
    }

    #[tokio::test]
    async fn rewind_legacy_intent_is_never_abandoned_without_untouched_proof() {
        let (dir, manager) = rewind_test_manager().await;
        manager
            .journal
            .import(
                "s",
                &CreateSession::default(),
                &[ev(1, session_event::Kind::Started(SessionStarted::default())), user(2, "existing history")],
            )
            .await
            .unwrap();
        let legacy: crate::claude_code::Reset = serde_json::from_value(serde_json::json!({
            "command_id": "legacy", "to_seq": 2, "restore_files": false, "session_id": "unknown-phase", "fresh": true
        }))
        .unwrap();
        assert!(!legacy.untouched, "missing phase is potentially destructive");
        manager.journal.prepare_claude_rewind("s", &legacy).await.unwrap();
        let failure = ev(3, session_event::Kind::Error(ErrorEvent { message: "blocked; explicitly retry Rewind".into() }));
        assert!(manager.journal.authorize_claude_rewind_retry("s", "legacy", "failed-explicit-retry").await.unwrap());
        // A stale actor snapshot must not erase the manager's authorization.
        manager.journal.prepare_claude_rewind("s", &legacy).await.unwrap();
        assert_eq!(manager.journal.claude_rewind("s").await.unwrap().unwrap().0.retries, ["failed-explicit-retry"]);
        assert!(manager.journal.fail_claude_rewind("s", "legacy", &failure, &[]).await.unwrap().0);
        let reopened = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let (pending, applied) = reopened.claude_rewind("s").await.unwrap().unwrap();
        assert_eq!(applied, 0);
        assert_eq!(pending.failure, "blocked; explicitly retry Rewind");
        assert!(pending.retries.is_empty(), "failed recovery consumes its durable authorizations");
        assert!(
            reopened
                .authorize_claude_rewind_retry("s", "legacy", "failed-explicit-retry")
                .await
                .unwrap_err()
                .to_string()
                .contains("explicitly retry")
        );
        assert!(
            reopened.claude_rewind("s").await.unwrap().unwrap().0.retries.is_empty(),
            "redelivery cannot resurrect a consumed authorization"
        );
        assert!(!reopened.is_applied("legacy").await.unwrap());
        assert!(reopened.is_applied("failed-explicit-retry").await.unwrap());
        assert_eq!(reopened.command_rejection("failed-explicit-retry").await.unwrap().as_deref(), Some("blocked; explicitly retry Rewind"));
        let events = reopened.events_after("s", 0, u32::MAX).await.unwrap();
        assert!(journal_claude_conversation(&reopened, "s", &CreateSession::default(), &events).await.is_err());
    }

    #[tokio::test]
    async fn rewind_retained_children_only_need_quiescence_for_file_restore() {
        let (_dir, manager) = rewind_test_manager().await;
        let mut remote = rewind_child_link("remote");
        if let Some(session_event::Kind::SubagentSpawned(s)) = &mut remote.kind {
            s.remote = Some(Default::default());
        }
        let events = [remote, user(5, "target")];
        manager.rewind_children_stopped(&rewind_affected_events(&events, 5, false)).await.unwrap();
        assert!(
            manager
                .rewind_children_stopped(&rewind_affected_events(&events, 5, true))
                .await
                .unwrap_err()
                .to_string()
                .contains("Remote subagent")
        );
        assert!(
            manager
                .rewind_children_stopped(&rewind_affected_events(&events, 3, false))
                .await
                .unwrap_err()
                .to_string()
                .contains("Remote subagent")
        );
    }

    #[tokio::test]
    async fn rewind_file_restore_checks_previously_withdrawn_children_that_resumed() {
        let (dir, manager) = rewind_test_manager().await;
        let mut child = rewind_child_link("resumed-child");
        child.seq = 3;
        let events = [
            ev(1, session_event::Kind::Started(SessionStarted::default())),
            user(2, "previous target"),
            child,
            ev(4, session_event::Kind::Rewound(Rewound { command_id: "earlier-rewind".into(), to_seq: 2, ..Default::default() })),
            user(5, "new target"),
        ];
        assert!(!c6_agent::effective_events(&events).iter().any(|e| matches!(e.kind, Some(session_event::Kind::SubagentSpawned(_)))));
        manager.journal.import("resumed-child", &CreateSession::default(), &rewind_child_events()).await.unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        manager.actors.lock().unwrap().insert(
            "resumed-child".into(),
            Handle {
                tx,
                recovery: Arc::default(),
                tools: Arc::new(SessionTools::new(dir.path().into())),
                running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                claude_code: Arc::default(),
                inbox: Arc::default(),
            },
        );
        manager.rewind_preflight("root", &events, 5, false).await.unwrap();
        let error = manager.rewind_preflight("root", &events, 5, true).await.unwrap_err().to_string();
        assert!(error.contains("resumed-child") && error.contains("still working"), "{error}");
    }

    #[tokio::test]
    async fn shell_writer_unlogged_start_blocks_failed_rewind_reload_and_late_notice() {
        if !c6_testkit::have("python3") {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let (state, manager) = rewind_test_manager().await;
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("owned.txt");
        std::fs::write(&file, "checkpoint").unwrap();
        let checkpoint = Checkpoints::new(state.path(), "s", work.path()).snapshot().await.unwrap();
        std::fs::write(&file, "unrestored").unwrap();
        let events = [
            ev(
                1,
                session_event::Kind::Started(SessionStarted {
                    tool_profile: crate::claude_code::PROFILE.into(),
                    cwd: work.path().display().to_string(),
                    ..Default::default()
                }),
            ),
            ev(2, session_event::Kind::Checkpoint(Checkpoint { commit: checkpoint, ..Default::default() })),
            user(3, "withdraw this"),
        ];
        manager.journal.import("s", &CreateSession::default(), &events).await.unwrap();
        let old = manager.load("s").await.unwrap().unwrap();
        // Force the forwarding/mailbox gap: retain Started outside the actor.
        let (tx, mut notices) = mpsc::unbounded_channel();
        old.tools.background.set_sink(Some(tx));
        let old_context = CancellationToken::new();
        old.tools.background.shell_context(&old_context);
        let command = "exec python3 -c 'import pathlib,time\np=pathlib.Path(\"writer-progress\")\nwhile True:\n p.write_text(str(time.monotonic_ns()))\n time.sleep(0.01)'";
        let task = old.tools.background.spawn(command, work.path(), true, &old_context).await.unwrap();
        let started = c6_testkit::within("unforwarded shell start", notices.recv()).await.unwrap();
        assert_eq!(started.state, c6_tools::TaskState::Started);
        let progress = work.path().join("writer-progress");
        let tick = c6_testkit::eventually("shell to write its progress", async || {
            std::fs::read_to_string(&progress).ok().filter(|text| !text.is_empty())
        })
        .await;
        let rewind = || SessionCommand {
            session_id: "s".into(),
            command_id: "rewind-after-stop".into(),
            ownership_epoch: 0,
            kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq: 3, restore_files: true })),
        };
        let mut refused = rewind();
        refused.command_id = "refused".into();
        assert!(manager.handle(refused).await.unwrap_err().contains("Background shell"));
        c6_testkit::eventually("the up-front refusal to be recorded", async || manager.journal.command_rejection("refused").await.unwrap())
            .await;
        assert!(!old.tx.is_closed(), "preflight refusal must keep the existing actor alive");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unrestored");
        let raw = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
        assert!(!raw.iter().any(|e| matches!(e.kind, Some(session_event::Kind::BackgroundTask(_) | session_event::Kind::Rewound(_)))));
        assert!(raw.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Error(e)) if e.message.contains("Background shell"))));
        assert!(manager.journal.claude_rewind("s").await.unwrap().is_none(), "preflight must not leave an intent");

        // A reopened journal and empty manager have neither a task registry
        // nor raw Started event. The durable writer still refuses preflight.
        let reopened = Arc::new(Journal::open(&state.path().join("journal.db")).await.unwrap());
        let reloaded = SessionManager::new(reopened, Arc::new(HeadLink::default()), state.path().join("worktrees"));
        assert!(reloaded.rewind_preflight("s", &raw, 3, true).await.unwrap_err().to_string().contains("Background shell"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unrestored");
        assert!(!manager.journal.is_applied("rewind-after-stop").await.unwrap());
        assert!(!old_context.is_cancelled(), "an up-front refusal must not cancel the existing turn's callbacks");
        c6_testkit::eventually("writer to keep writing after both failed attempts", async || {
            std::fs::read_to_string(&progress).ok().filter(|text| !text.is_empty() && text != &tick)
        })
        .await;

        old.tools.background.kill_by_user(&task).unwrap();
        let done = c6_testkit::within("unforwarded shell completion", notices.recv()).await.unwrap();
        c6_testkit::eventually("owned shell group to disappear", async || manager.journal.confirm_shell_writers_stopped("s").await.ok())
            .await;
        assert!(manager.journal.shell_writer_sources("s").await.unwrap().contains(&task), "ESRCH must not erase source identity");

        // Only now is a fake no-model initialize allowed, followed by restore.
        let bin = state.path().join("claude-shell-rewind");
        std::fs::write(
            &bin,
            r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    v = json.loads(line)
    assert v['type'] == 'control_request' and v['request']['subtype'] == 'initialize'
    print(json.dumps({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': v['request_id']}}), flush=True)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        struct RestoreBin(Option<PathBuf>);
        impl Drop for RestoreBin {
            fn drop(&mut self) {
                let _ = crate::claude_code::replace_test_bin(self.0.take());
            }
        }
        let _bin = RestoreBin(crate::claude_code::replace_test_bin(Some(bin)));
        manager.handle(rewind()).await.unwrap();
        c6_testkit::eventually("rewind after confirmed shell absence", async || {
            manager.journal.is_applied("rewind-after-stop").await.unwrap().then_some(())
        })
        .await;
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "checkpoint");
        assert!(!progress.exists(), "only a confirmed stopped writer may have its new files restored away");
        let fresh = manager.load("s").await.unwrap().unwrap();
        fresh.tx.send(ActorMsg::Task(done)).unwrap();
        c6_testkit::eventually("late task event to be recorded without a notice", async || {
            let raw = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
            raw.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::BackgroundTask(t)) if t.task_id == task)).then_some(())
        })
        .await;
        let raw = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
        assert!(!raw.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::NoticeQueued(n)) if n.command_id.contains(&task))));
        assert!(
            old.tools.background.spawn("touch stale-callback", work.path(), false, &old_context).await.is_err(),
            "old ToolCtx must stay canceled after marker"
        );
        assert!(!work.path().join("stale-callback").exists());
        let next = fresh.tools.background.spawn("true", work.path(), false, &CancellationToken::new()).await.unwrap();
        assert_ne!(next, task, "reload must reserve IDs from unlogged ownership rows");
        if let Some(pgid) = started.pgid {
            assert_eq!(nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None), Err(nix::errno::Errno::ESRCH));
        }
    }

    #[tokio::test]
    async fn shell_writer_leader_exit_and_unknown_remote_are_not_stop_proof() {
        let (dir, manager) = rewind_test_manager().await;
        let owner = manager.journal.shell_owner("s", "s");
        let reservation = owner.reserve("bg7").await.unwrap();
        let mut leader = tokio::process::Command::new("sleep").arg("300").process_group(0).kill_on_drop(true).spawn().unwrap();
        let pgid = leader.id().unwrap() as i32;
        let mut member = tokio::process::Command::new("sleep").arg("300").process_group(pgid).kill_on_drop(true).spawn().unwrap();
        owner.launched(&reservation, Some(pgid)).await.unwrap();
        leader.kill().await.unwrap();
        assert!(manager.journal.confirm_shell_writers_stopped("s").await.is_err());
        let reopened = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        assert!(reopened.confirm_shell_writers_stopped("s").await.is_err());
        assert!(member.try_wait().unwrap().is_none(), "persisted group checks must not signal survivors");
        member.kill().await.unwrap();
        reopened.confirm_shell_writers_stopped("s").await.unwrap();
        assert_eq!(reopened.shell_writer_sources("s").await.unwrap(), ["bg7"]);
        let remote = owner.reserve("bg8").await.unwrap();
        owner.launched(&remote, None).await.unwrap();
        assert!(reopened.confirm_shell_writers_stopped("s").await.unwrap_err().to_string().contains("unknown writer identity"));
    }

    #[tokio::test]
    async fn shell_writer_pending_root_closes_descendant_admission_and_unknown_inflight_blocks_reset() {
        let (_dir, manager) = rewind_test_manager().await;
        let child = manager.journal.shell_owner("child", "root");
        let before = child.reserve("bg1").await.unwrap();
        let reset = crate::claude_code::Reset {
            command_id: "r".into(),
            to_seq: 2,
            restore_files: true,
            session_id: "native".into(),
            fresh: true,
            withdrawn: Vec::new(),
            ..Default::default()
        };
        manager.journal.prepare_claude_rewind("root", &reset).await.unwrap();
        assert!(child.reserve("bg2").await.is_err(), "root intent must close descendant admission atomically");
        assert!(manager.journal.confirm_shell_writers_stopped("root").await.is_err(), "pre-intent unknown launch is a durable barrier");
        assert_eq!(manager.journal.shell_writer_sources("root").await.unwrap(), ["child"]);
        child.not_launched(&before).await.unwrap();
        manager.journal.confirm_shell_writers_stopped("root").await.unwrap();
    }

    #[tokio::test]
    async fn rewind_explicit_retry_preserves_pending_receipt_and_rejects_changed_target_or_files() {
        let (dir, manager) = rewind_test_manager().await;
        let events = [
            ev(1, session_event::Kind::Started(SessionStarted { tool_profile: crate::claude_code::PROFILE.into(), ..Default::default() })),
            user(2, "retry here"),
            user(3, "another target"),
        ];
        manager.journal.import("s", &CreateSession::default(), &events).await.unwrap();
        let mut pending = crate::claude_code::Reset {
            command_id: "old".into(),
            to_seq: 2,
            restore_files: false,
            session_id: "pending-native-id".into(),
            fresh: true,
            withdrawn: vec!["withdrawn".into()],
            ..Default::default()
        };
        manager.journal.prepare_claude_rewind("s", &pending).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.actors.lock().unwrap().insert(
            "s".into(),
            Handle {
                tx,
                recovery: Arc::default(),
                tools: Arc::new(SessionTools::new(dir.path().into())),
                running: Arc::default(),
                claude_code: Arc::default(),
                inbox: Arc::default(),
            },
        );
        let command = |id: &str, to_seq, restore_files| SessionCommand {
            session_id: "s".into(),
            command_id: id.into(),
            ownership_epoch: 0,
            kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq, restore_files })),
        };
        for (id, to_seq, files) in [("changed-target", 3, false), ("changed-files", 2, true)] {
            assert!(manager.handle(command(id, to_seq, files)).await.unwrap_err().contains("awaiting confirmation"));
        }
        assert!(rx.try_recv().is_err());
        manager.handle(command("retry", 2, false)).await.unwrap();
        assert!(matches!(rx.recv().await, Some(ActorMsg::Rewind { command_id, to_seq: 2, restore_files: false }) if command_id == "retry"));
        let (durable, applied) = manager.journal.claude_rewind("s").await.unwrap().unwrap();
        pending.retries.push("retry".into());
        assert_eq!(serde_json::to_value(durable).unwrap(), serde_json::to_value(&pending).unwrap());
        assert_eq!(applied, 0, "dispatch must not overwrite or acknowledge the old receipt");
        assert!(!manager.journal.is_applied("old").await.unwrap());
        assert!(!manager.journal.is_applied("retry").await.unwrap());

        // Another retry can arrive while initialization owns the actor. All
        // three receipts must commit before input, without another native reset.
        manager.handle(command("overlap", 2, false)).await.unwrap();
        assert!(matches!(rx.recv().await, Some(ActorMsg::Rewind { command_id, .. }) if command_id == "overlap"));
        let writer = Writer::new("s".into(), manager.journal.clone(), manager.link.clone(), 4);
        let confirmation = Rewound { command_id: "old".into(), to_seq: 2, restore_error: "no checkpoint".into(), ..Default::default() };
        assert_eq!(writer.append(session_event::Kind::Rewound(confirmation.clone())).await.unwrap(), 4);
        assert_eq!(*writer.next_seq.lock().await, 7);
        assert_eq!(writer.logged.load(std::sync::atomic::Ordering::SeqCst), 7);
        let events = manager.journal.events_after("s", 0, u32::MAX).await.unwrap();
        for (event, id) in events[3..].iter().zip(["old", "retry", "overlap"]) {
            let Some(session_event::Kind::Rewound(rewound)) = &event.kind else { panic!("expected a retry confirmation") };
            assert_eq!(rewound, &Rewound { command_id: id.into(), ..confirmation.clone() });
        }
        let (confirmed, applied) = manager.journal.claude_rewind("s").await.unwrap().unwrap();
        assert_eq!(applied, 6, "the native branch must cover the last retry receipt");
        assert!(confirmed.retries.is_empty());
        let conversation = journal_claude_conversation(&manager.journal, "s", &CreateSession::default(), &events).await.unwrap();
        assert_eq!(conversation.id, "pending-native-id", "retry markers must not discard the confirmed branch");
        assert_eq!(
            writer
                .append(session_event::Kind::User(c6_proto::UserMessage { text: "future input".into(), ..Default::default() }))
                .await
                .unwrap(),
            7
        );
        for id in ["old", "retry", "overlap"] {
            assert!(manager.journal.is_applied(id).await.unwrap());
            manager.handle(command(id, 2, false)).await.unwrap();
        }
        assert!(rx.try_recv().is_err(), "confirmed receipts must not reenter the actor mailbox");
        assert_eq!(manager.journal.events_after("s", 0, u32::MAX).await.unwrap().len(), 7);
    }

    #[tokio::test]
    async fn rewind_checks_idle_child_registry_and_inbox_without_model_calls() {
        let (dir, manager) = rewind_test_manager().await;
        manager.journal.import("child", &CreateSession::default(), &rewind_child_events()).await.unwrap();
        let tools = Arc::new(SessionTools::new(dir.path().into()));
        let (task, _) = tools.background.start_agent("child background work", false);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = Handle {
            tx,
            recovery: Arc::default(),
            tools: tools.clone(),
            running: Arc::default(),
            claude_code: Arc::default(),
            inbox: Arc::default(),
        };
        manager.actors.lock().unwrap().insert("child".into(), handle.clone());
        let links = [rewind_child_link("child")];
        let error = manager.rewind_children_stopped(&links).await.unwrap_err().to_string();
        assert!(error.contains("child") && error.contains("still working"), "{error}");

        tools.background.finish_agent(&task, Ok("done".into()));
        handle.inbox.lock().unwrap().push(PendingInput { command_id: "queued".into(), ..Default::default() }, None);
        assert!(manager.rewind_children_stopped(&links).await.is_err(), "queued child input must block even between model turns");
        handle.inbox.lock().unwrap().queue.clear();
        let reply = tokio::spawn(async move {
            let Some(ActorMsg::RewindIdle(reply)) = rx.recv().await else { panic!("expected a child mailbox barrier") };
            reply.send(true).unwrap();
        });
        manager.rewind_children_stopped(&links).await.unwrap();
        reply.await.unwrap();
    }

    #[tokio::test]
    async fn rewind_child_mailbox_refusal_overrides_idle_cached_flags() {
        let (dir, manager) = rewind_test_manager().await;
        manager.journal.import("child", &CreateSession::default(), &rewind_child_events()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.actors.lock().unwrap().insert(
            "child".into(),
            Handle {
                tx,
                recovery: Arc::default(),
                tools: Arc::new(SessionTools::new(dir.path().into())),
                running: Arc::default(),
                claude_code: Arc::default(),
                inbox: Arc::default(),
            },
        );
        let reply = tokio::spawn(async move {
            let Some(ActorMsg::RewindIdle(reply)) = rx.recv().await else { panic!("expected a child mailbox barrier") };
            reply.send(false).unwrap();
        });
        let error = manager.rewind_children_stopped(&[rewind_child_link("child")]).await.unwrap_err().to_string();
        assert!(error.contains("pending work"), "{error}");
        reply.await.unwrap();
    }

    #[tokio::test]
    async fn rewind_checks_descendant_raw_tasks_even_if_rewound_and_no_actor_survived() {
        let (_dir, manager) = rewind_test_manager().await;
        let mut child = rewind_child_events();
        child.push(rewind_child_link("grandchild"));
        manager.journal.import("child", &CreateSession::default(), &child).await.unwrap();
        let mut grandchild = rewind_child_events();
        grandchild.push(ev(
            4,
            session_event::Kind::BackgroundTask(BackgroundTask {
                task_id: "bg1".into(),
                kind: "shell".into(),
                state: "started".into(),
                ..Default::default()
            }),
        ));
        grandchild.push(ev(5, session_event::Kind::Rewound(Rewound { to_seq: 4, ..Default::default() })));
        manager.journal.import("grandchild", &CreateSession::default(), &grandchild).await.unwrap();
        let links = [rewind_child_link("child")];
        let error = manager.rewind_children_stopped(&links).await.unwrap_err().to_string();
        assert!(error.contains("grandchild") && error.contains("unclosed background task"), "{error}");
        manager
            .journal
            .append(
                "grandchild",
                &ev(
                    6,
                    session_event::Kind::BackgroundTask(BackgroundTask {
                        task_id: "bg1".into(),
                        kind: "shell".into(),
                        state: "finished".into(),
                        ..Default::default()
                    }),
                ),
            )
            .await
            .unwrap();
        manager.rewind_children_stopped(&links).await.unwrap();
    }

    async fn rewind_file_restore_dispatch_refuses_held_descendant(claude_ancestor: bool) {
        if !c6_testkit::have("python3") {
            return;
        }
        let (state, manager) = rewind_test_manager().await;
        let work = tempfile::tempdir().unwrap();
        let _cleanup = crate::claude_code::tests::HeldFixtureCleanup(manager.claude_dir().join("held"));
        let (child_spec, child_started, bin) = rewind_held_fixture(work.path());
        let mut child = rewind_child_events();
        child[0] = ev(1, session_event::Kind::Started(child_started.clone()));
        manager.journal.import("s", &child_spec, &child).await.unwrap();
        let spec = if claude_ancestor { child_spec.clone() } else { CreateSession::default() };
        let started = if claude_ancestor {
            child_started
        } else {
            SessionStarted { cwd: work.path().display().to_string(), tool_profile: "generic".into(), ..Default::default() }
        };
        let file = work.path().join("owned.txt");
        std::fs::write(&file, "checkpoint").unwrap();
        let checkpoint = Checkpoints::new(state.path(), "root", work.path()).snapshot().await.unwrap();
        std::fs::write(&file, "unrestored").unwrap();
        let mut link = rewind_child_link("s");
        link.seq = 3;
        let events = [
            ev(1, session_event::Kind::Started(started)),
            ev(2, session_event::Kind::Checkpoint(Checkpoint { commit: checkpoint, ..Default::default() })),
            link,
            user(4, "target"),
        ];
        manager.journal.import("root", &spec, &events).await.unwrap();
        leave_rewind_held(&manager, &child_spec, &bin, work.path(), "unselected-child-fork").await;
        let (record, _) =
            crate::claude_code::Process::held_record(&manager.claude_dir().join("held"), "unselected-child-fork").unwrap().unwrap();
        let metadata = std::fs::read(record.join("meta.json")).unwrap();
        let command = |id: &str, restore_files| SessionCommand {
            session_id: "root".into(),
            command_id: id.into(),
            kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq: 4, restore_files })),
            ownership_epoch: 0,
        };
        let result = manager.handle(command("restore-refused", true)).await;
        if claude_ancestor {
            assert!(result.unwrap_err().contains("Subagent s"), "Claude Code dispatch must refuse before reset admission");
        } else {
            result.unwrap(); // carbon6's actor checks before cancellation and execution.
        }
        let rejection = c6_testkit::eventually("production file-restore refusal", async || {
            manager.journal.command_rejection("restore-refused").await.unwrap()
        })
        .await;
        assert!(rejection.contains("Subagent s") && rejection.contains("live or unconfirmed Claude Code runtime"), "{rejection}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unrestored");
        assert_eq!(std::fs::read(record.join("meta.json")).unwrap(), metadata);
        assert!(!record.join("code").exists(), "refusing ancestor Rewind must not stop the child CLI");
        assert!(manager.journal.claude_rewind("root").await.unwrap().is_none(), "refusal must not create a Claude reset intent");
        let raw = manager.journal.events_after("root", 0, u32::MAX).await.unwrap();
        assert!(!raw.iter().any(|event| matches!(event.kind, Some(session_event::Kind::Rewound(_)))));
        let root = manager.actors.lock().unwrap().get("root").unwrap().clone();
        assert!(!root.tx.is_closed(), "an expected guard refusal must preserve the actor");
        root.tx.send(ActorMsg::SetSubagentModel { command_id: "after-refusal".into(), model: None }).unwrap();
        c6_testkit::eventually("actor remains usable after restore refusal", async || {
            manager.journal.events_after("root", 0, u32::MAX).await.unwrap().iter().any(
                |event| matches!(&event.kind, Some(session_event::Kind::SubagentModelSet(model)) if model.command_id == "after-refusal"),
            ).then_some(())
        })
        .await;

        if claude_ancestor {
            // Exercise real dispatch admission for the retained-conversation
            // exemption, but capture the message instead of starting a real
            // CLI reset. Refusal above used the real actor and its journal.
            let admission = SessionManager::new(manager.journal.clone(), Arc::default(), state.path().join("worktrees"));
            let (tx, mut rx) = mpsc::unbounded_channel();
            admission.actors.lock().unwrap().insert("root".into(), Handle { tx, ..root });
            admission.handle(command("conversation-only", false)).await.unwrap();
            assert!(matches!(rx.recv().await,
                Some(ActorMsg::Rewind { command_id, to_seq: 4, restore_files: false }) if command_id == "conversation-only"));
        } else {
            // The ordinary-loop conversation-only path actually executes,
            // despite the retained child's still-live held CLI.
            manager.handle(command("conversation-only", false)).await.unwrap();
            c6_testkit::eventually("ordinary retained-child conversation rewind", async || {
                manager
                    .journal
                    .events_after("root", 0, u32::MAX)
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| {
                        matches!(&event.kind, Some(session_event::Kind::Rewound(rewound))
                        if rewound.command_id == "conversation-only" && !rewound.restored_files)
                    })
                    .then_some(())
            })
            .await;
            assert!(manager.journal.claude_rewind("root").await.unwrap().is_none(), "ordinary Rewind must not acquire a CLI reset intent");
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unrestored");
        assert!(!record.join("code").exists());
        crate::claude_code::stop_held_for_rewind(&manager.claude_dir().join("held"), "s", &manager.journal).await.unwrap();
    }

    #[tokio::test]
    async fn rewind_file_restore_dispatch_claude_ancestor_refuses_live_held_descendant() {
        rewind_file_restore_dispatch_refuses_held_descendant(true).await;
    }

    #[tokio::test]
    async fn rewind_file_restore_dispatch_carbon6_ancestor_refuses_live_held_descendant() {
        rewind_file_restore_dispatch_refuses_held_descendant(false).await;
    }

    #[tokio::test]
    async fn rewind_file_restore_dispatch_late_descendant_refusal_preserves_actor_and_queued_input() {
        let (state, manager) = rewind_test_manager().await;
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("owned.txt");
        std::fs::write(&file, "checkpoint").unwrap();
        let checkpoint = Checkpoints::new(state.path(), "root", work.path()).snapshot().await.unwrap();
        std::fs::write(&file, "unrestored").unwrap();
        let mut link = rewind_child_link("child");
        link.seq = 3;
        let events = [
            ev(
                1,
                session_event::Kind::Started(SessionStarted {
                    cwd: work.path().display().to_string(),
                    tool_profile: "generic".into(),
                    ..Default::default()
                }),
            ),
            ev(2, session_event::Kind::Checkpoint(Checkpoint { commit: checkpoint, ..Default::default() })),
            link,
            user(4, "target"),
        ];
        manager.journal.import("root", &CreateSession::default(), &events).await.unwrap();
        let mut child = rewind_child_events();
        child[0] =
            ev(1, session_event::Kind::Started(SessionStarted { tool_profile: crate::claude_code::PROFILE.into(), ..Default::default() }));
        manager.journal.import("child", &CreateSession::default(), &child).await.unwrap();
        let mut cli = tokio::process::Command::new("sleep").arg("300").process_group(0).kill_on_drop(true).spawn().unwrap();
        let pgid = cli.id().unwrap() as i32;
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.actors.lock().unwrap().insert(
            "child".into(),
            Handle {
                tx,
                recovery: Arc::default(),
                tools: Arc::new(SessionTools::new(work.path().into())),
                running: Arc::default(),
                claude_code: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                inbox: Arc::default(),
            },
        );
        manager
            .handle(SessionCommand {
                session_id: "root".into(),
                command_id: "late-restore".into(),
                ownership_epoch: 0,
                kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq: 4, restore_files: true })),
            })
            .await
            .unwrap();
        // The actor has already checked runtime ownership, but has not yet
        // cancelled/queued the rewind. Publish a writer during its mailbox
        // barrier; the execution-time guard must detect this new ownership.
        let Some(ActorMsg::RewindIdle(idle)) = c6_testkit::within("first child admission barrier", rx.recv()).await else {
            panic!("expected a child mailbox barrier")
        };
        manager.journal.track_claude_process("child", pgid).await.unwrap();
        manager
            .handle(SessionCommand {
                session_id: "root".into(),
                command_id: "queued-input".into(),
                ownership_epoch: 0,
                kind: Some(session_command::Kind::Input(c6_proto::UserInput {
                    text: "preserve this queued input".into(),
                    human_origin: true,
                    ..Default::default()
                })),
            })
            .await
            .unwrap();
        idle.send(true).unwrap();
        let rejection = c6_testkit::eventually("execution-time descendant refusal", async || {
            manager.journal.command_rejection("late-restore").await.unwrap()
        })
        .await;
        assert!(rejection.contains("live or unconfirmed Claude Code runtime") && rejection.contains("Subagent child"), "{rejection}");
        c6_testkit::eventually("queued input survives expected execution refusal", async || {
            manager
                .journal
                .events_after("root", 0, u32::MAX)
                .await
                .unwrap()
                .iter()
                .any(|event| matches!(&event.kind, Some(session_event::Kind::User(input)) if input.command_id == "queued-input"))
                .then_some(())
        })
        .await;
        let root = manager.actors.lock().unwrap().get("root").unwrap().clone();
        assert!(!root.tx.is_closed(), "late guard refusal must not terminate the actor");
        root.tx.send(ActorMsg::SetSubagentModel { command_id: "still-usable".into(), model: None }).unwrap();
        c6_testkit::eventually("actor handles commands after late refusal", async || {
            manager
                .journal
                .events_after("root", 0, u32::MAX)
                .await
                .unwrap()
                .iter()
                .any(
                    |event| matches!(&event.kind, Some(session_event::Kind::SubagentModelSet(model)) if model.command_id == "still-usable"),
                )
                .then_some(())
        })
        .await;
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unrestored");
        assert!(manager.journal.claude_rewind("root").await.unwrap().is_none());
        let raw = manager.journal.events_after("root", 0, u32::MAX).await.unwrap();
        assert!(!raw.iter().any(|event| matches!(event.kind, Some(session_event::Kind::Rewound(_)))));
        assert!(cli.try_wait().unwrap().is_none(), "refusal must not stop the child process");
        assert_eq!(manager.journal.claude_processes("child").await.unwrap(), [pgid]);
        cli.kill().await.unwrap();
    }

    #[tokio::test]
    async fn rewind_file_restore_dispatch_refusal_preserves_already_queued_conversation_rewind() {
        let (state, manager) = rewind_test_manager().await;
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("owned.txt");
        std::fs::write(&file, "checkpoint").unwrap();
        let checkpoint = Checkpoints::new(state.path(), "root", work.path()).snapshot().await.unwrap();
        std::fs::write(&file, "unrestored").unwrap();
        let mut link = rewind_child_link("child");
        link.seq = 3;
        let events = [
            ev(
                1,
                session_event::Kind::Started(SessionStarted {
                    cwd: work.path().display().to_string(),
                    tool_profile: "generic".into(),
                    ..Default::default()
                }),
            ),
            ev(2, session_event::Kind::Checkpoint(Checkpoint { commit: checkpoint, ..Default::default() })),
            link,
            user(4, "target"),
        ];
        manager.journal.import("root", &CreateSession::default(), &events).await.unwrap();
        manager.journal.import("child", &CreateSession::default(), &rewind_child_events()).await.unwrap();
        let (outbound, mut messages) = mpsc::channel(128);
        manager.link.connected(outbound);
        manager
            .handle(SessionCommand {
                session_id: "root".into(),
                command_id: "running-input".into(),
                ownership_epoch: 0,
                kind: Some(session_command::Kind::Input(c6_proto::UserInput {
                    text: "wait for credentials".into(),
                    human_origin: true,
                    ..Default::default()
                })),
            })
            .await
            .unwrap();
        // Hold the real turn in credential acquisition. Cancellation does
        // not complete that acquisition, so Rewinds really remain queued.
        c6_testkit::within("turn waiting for credentials", async {
            loop {
                let message = messages.recv().await.unwrap();
                if matches!(message.msg, Some(c6_proto::worker_msg::Msg::CredentialRequest(_))) {
                    break;
                }
            }
        })
        .await;
        let command = |id: &str, restore_files| SessionCommand {
            session_id: "root".into(),
            command_id: id.into(),
            kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq: 4, restore_files })),
            ownership_epoch: 0,
        };
        manager.handle(command("queued-conversation", false)).await.unwrap();
        let root = manager.actors.lock().unwrap().get("root").unwrap().clone();
        root.tx.send(ActorMsg::SetSubagentModel { command_id: "conversation-is-queued".into(), model: None }).unwrap();
        c6_testkit::eventually("conversation Rewind already processed into the actor queue", async || {
            manager.journal.events_after("root", 0, u32::MAX).await.unwrap().iter().any(
                |event| matches!(&event.kind, Some(session_event::Kind::SubagentModelSet(model)) if model.command_id == "conversation-is-queued"),
            ).then_some(())
        }).await;
        assert!(root.running.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!manager.journal.is_applied("queued-conversation").await.unwrap());
        let mut cli = tokio::process::Command::new("sleep").arg("300").process_group(0).kill_on_drop(true).spawn().unwrap();
        manager.journal.track_claude_process("child", cli.id().unwrap() as i32).await.unwrap();
        manager.handle(command("refused-file-restore", true)).await.unwrap();
        let rejection = c6_testkit::eventually("file restore alone receives rejection", async || {
            manager.journal.command_rejection("refused-file-restore").await.unwrap()
        })
        .await;
        assert!(rejection.contains("Subagent child"), "{rejection}");
        assert!(manager.journal.command_rejection("queued-conversation").await.unwrap().is_none());
        assert!(
            !manager.journal.is_applied("queued-conversation").await.unwrap(),
            "the older conversation command is still queued, not rejected"
        );
        manager.link.disconnected(); // Release the cancelled turn without model/network access.
        c6_testkit::eventually("previously queued conversation Rewind executes after restore refusal", async || {
            manager.journal.events_after("root", 0, u32::MAX).await.unwrap().iter().any(
                |event| matches!(&event.kind, Some(session_event::Kind::Rewound(rewound)) if rewound.command_id == "queued-conversation"),
            ).then_some(())
        })
        .await;
        assert!(manager.journal.command_rejection("queued-conversation").await.unwrap().is_none());
        assert!(manager.journal.claude_rewind("root").await.unwrap().is_none());
        assert!(!root.tx.is_closed());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unrestored");
        assert!(cli.try_wait().unwrap().is_none());
        cli.kill().await.unwrap();
    }

    #[tokio::test]
    async fn rewind_file_restore_dispatch_internal_descendant_decode_failure_remains_terminal() {
        let (state, manager) = rewind_test_manager().await;
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("owned.txt");
        std::fs::write(&file, "checkpoint").unwrap();
        let checkpoint = Checkpoints::new(state.path(), "root", work.path()).snapshot().await.unwrap();
        std::fs::write(&file, "unrestored").unwrap();
        let mut link = rewind_child_link("child");
        link.seq = 3;
        let events = [
            ev(
                1,
                session_event::Kind::Started(SessionStarted {
                    cwd: work.path().display().to_string(),
                    tool_profile: "generic".into(),
                    ..Default::default()
                }),
            ),
            ev(2, session_event::Kind::Checkpoint(Checkpoint { commit: checkpoint, ..Default::default() })),
            link,
            user(4, "target"),
        ];
        manager.journal.import("root", &CreateSession::default(), &events).await.unwrap();
        manager.journal.import("child", &CreateSession::default(), &rewind_child_events()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.actors.lock().unwrap().insert(
            "child".into(),
            Handle {
                tx,
                recovery: Arc::default(),
                tools: Arc::new(SessionTools::new(work.path().into())),
                running: Arc::default(),
                claude_code: Arc::default(),
                inbox: Arc::default(),
            },
        );
        manager
            .handle(SessionCommand {
                session_id: "root".into(),
                command_id: "decode-file-restore".into(),
                ownership_epoch: 0,
                kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq: 4, restore_files: true })),
            })
            .await
            .unwrap();
        let Some(ActorMsg::RewindIdle(admission)) = c6_testkit::within("valid admission child barrier", rx.recv()).await else {
            panic!("expected admission child barrier")
        };
        admission.send(true).unwrap();
        let Some(ActorMsg::RewindIdle(execution)) = c6_testkit::within("execution child barrier", rx.recv()).await else {
            panic!("expected execution child barrier")
        };
        // Admission read a valid child history. Corrupt only its journal
        // event now, so the final restore guard encounters a genuine decode
        // failure after its mailbox barrier, not a protective refusal.
        let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", state.path().join("journal.db").display())).await.unwrap();
        sqlx::query("UPDATE events SET data = ? WHERE session_id = 'child' AND seq = 1").bind(vec![0xffu8]).execute(&db).await.unwrap();
        db.close().await;
        execution.send(true).unwrap();
        let root = manager.actors.lock().unwrap().get("root").unwrap().clone();
        c6_testkit::eventually("internal guard failure retains terminal actor behavior", async || root.tx.is_closed().then_some(())).await;
        let raw = manager.journal.events_after("root", 0, u32::MAX).await.unwrap();
        assert!(raw.iter().any(|event| matches!(&event.kind, Some(session_event::Kind::Error(error))
            if error.message.contains("no further messages were sent") && error.message.contains("decode"))));
        assert!(!raw.iter().any(|event| matches!(event.kind, Some(session_event::Kind::Rewound(_)))));
        assert!(
            manager.journal.command_rejection("decode-file-restore").await.unwrap().is_none(),
            "internal failure must not become a recoverable refusal receipt"
        );
        assert!(manager.journal.claude_rewind("root").await.unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "unrestored");
        let error = manager.journal.events_after("child", 0, u32::MAX).await.unwrap_err();
        assert!(error.is::<prost::DecodeError>() && !error.is::<StopUnconfirmed>(), "{error:#}");
    }

    // Unit-level guard coverage below calls rewind_preflight directly. The
    // rewind_file_restore_dispatch_* tests above cover production routing.
    #[tokio::test]
    async fn rewind_completed_child_with_live_claude_group_blocks_file_restore_but_not_retained_conversation() {
        let (dir, manager) = rewind_test_manager().await;
        manager.journal.import("child", &CreateSession::default(), &rewind_child_events()).await.unwrap();
        let mut cli = tokio::process::Command::new("sleep").arg("300").process_group(0).kill_on_drop(true).spawn().unwrap();
        let pgid = cli.id().unwrap() as i32;
        manager.journal.track_claude_process("child", pgid).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.actors.lock().unwrap().insert(
            "child".into(),
            Handle {
                tx,
                recovery: Arc::default(),
                tools: Arc::new(SessionTools::new(dir.path().into())),
                running: Arc::default(),
                claude_code: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                inbox: Arc::default(),
            },
        );
        let events = [rewind_child_link("child"), user(5, "target")];
        manager.rewind_preflight("root", &events, 5, false).await.unwrap();
        for (to_seq, restore_files) in [(5, true), (3, false)] {
            let error = manager.rewind_preflight("root", &events, to_seq, restore_files).await.unwrap_err().to_string();
            assert!(error.contains("child") && error.contains("live or unconfirmed Claude Code runtime"), "{error}");
            assert!(cli.try_wait().unwrap().is_none(), "preflight must not stop a descendant's CLI");
            assert_eq!(manager.journal.claude_processes("child").await.unwrap(), [pgid]);
        }
        assert!(rx.try_recv().is_err(), "an idle mailbox is not proof that the CLI stopped");
        cli.kill().await.unwrap();
        let reply = tokio::spawn(async move {
            let Some(ActorMsg::RewindIdle(reply)) = rx.recv().await else { panic!("expected a child mailbox barrier") };
            reply.send(true).unwrap();
        });
        manager.rewind_preflight("root", &events, 5, true).await.unwrap();
        reply.await.unwrap();
        assert!(manager.journal.claude_processes("child").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rewind_nested_completed_child_with_unselected_held_claude_runtime_fails_closed() {
        if !c6_testkit::have("python3") {
            return;
        }
        let (dir, manager) = rewind_test_manager().await;
        let _cleanup = crate::claude_code::tests::HeldFixtureCleanup(manager.claude_dir().join("held"));
        let (spec, started, bin) = rewind_held_fixture(dir.path());
        let mut child = rewind_child_events();
        child.push(rewind_child_link("s"));
        manager.journal.import("child", &CreateSession::default(), &child).await.unwrap();
        let mut grandchild = rewind_child_events();
        grandchild[0] = ev(1, session_event::Kind::Started(started));
        manager.journal.import("s", &spec, &grandchild).await.unwrap();
        // This runtime belongs to the descendant, but is not its selected
        // conversation. Looking up only that conversation would miss it.
        let conversation = journal_claude_conversation(&manager.journal, "s", &spec, &grandchild).await.unwrap();
        assert_eq!(conversation.id, "s");
        leave_rewind_held(&manager, &spec, &bin, dir.path(), "unselected-child-fork").await;
        assert!(manager.journal.claude_processes("s").await.unwrap().is_empty(), "no actor/observer owns this held runtime");
        let (record, _) =
            crate::claude_code::Process::held_record(&manager.claude_dir().join("held"), "unselected-child-fork").unwrap().unwrap();
        let before = std::fs::read(record.join("meta.json")).unwrap();
        let events = [rewind_child_link("child"), user(5, "target")];
        manager.rewind_preflight("root", &events, 5, false).await.unwrap();
        for (to_seq, restore_files) in [(5, true), (3, false)] {
            let error = format!("{:#}", manager.rewind_preflight("root", &events, to_seq, restore_files).await.unwrap_err());
            assert!(error.contains("Subagent s") && error.contains("Held Claude Code process group"), "{error}");
            assert_eq!(std::fs::read(record.join("meta.json")).unwrap(), before, "preflight must retain held ownership");
            assert!(!record.join("code").exists(), "preflight must not stop the held runtime");
        }
        crate::claude_code::stop_held_for_rewind(&manager.claude_dir().join("held"), "s", &manager.journal).await.unwrap();
        manager.rewind_preflight("root", &events, 5, true).await.unwrap();
    }

    #[tokio::test]
    async fn rewind_ignores_unmapped_stubs_but_preserves_known_descendant_process_barriers() {
        let (_dir, manager) = rewind_test_manager().await;
        manager.journal.import("child", &CreateSession::default(), &rewind_child_events()).await.unwrap();
        let events = [rewind_child_link("child"), user(5, "target")];
        let record = manager.claude_dir().join("held/unknown-runtime");
        std::fs::create_dir_all(&record).unwrap();
        // An unowned incomplete launch must not poison every other session.
        // Skipping it never clears the durable barrier of a known child.
        manager.rewind_preflight("root", &events, 5, true).await.unwrap();
        let meta = serde_json::json!({
            "session": "child", "conversation": "old-fork", "pgid": 0,
            "keeper": 0, "output_root": null, "token": null,
            "launch": crate::claude_code::Launch::default(),
        });
        let mut empty_session = meta.clone();
        empty_session["session"] = serde_json::json!("");
        let mut missing_session = meta.clone();
        missing_session.as_object_mut().unwrap().remove("session");
        for bytes in [b"not json".to_vec(), serde_json::to_vec(&missing_session).unwrap(), serde_json::to_vec(&empty_session).unwrap()] {
            std::fs::write(record.join("meta.json"), &bytes).unwrap();
            manager.rewind_preflight("root", &events, 5, false).await.unwrap();
            manager.rewind_preflight("root", &events, 5, true).await.unwrap();
            let pgid = nix::unistd::getpgrp().as_raw();
            manager.journal.track_claude_process("child", pgid).await.unwrap();
            let error = manager.rewind_preflight("root", &events, 5, true).await.unwrap_err().to_string();
            assert!(error.contains("child") && error.contains("unconfirmed Claude Code runtime"), "{error}");
            assert_eq!(std::fs::read(record.join("meta.json")).unwrap(), bytes);
            assert_eq!(manager.journal.claude_processes("child").await.unwrap(), vec![pgid]);
            manager.journal.untrack_claude_process("child", pgid).await.unwrap();
        }
        // A record that identifies this child but cannot prove group absence
        // remains fail-closed, even without a separately tracked PGID.
        std::fs::write(record.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        manager.rewind_preflight("root", &events, 5, false).await.unwrap();
        let error = manager.rewind_preflight("root", &events, 5, true).await.unwrap_err().to_string();
        assert!(error.contains("child") && error.contains("unconfirmed Claude Code runtime"), "{error}");
        assert!(manager.journal.claude_processes("child").await.unwrap().is_empty());
        let error = manager.rewind_children_stopped(&[rewind_child_link("")]).await.unwrap_err().to_string();
        assert!(error.contains("identity is unknown"), "{error}");
    }

    #[tokio::test]
    async fn rewind_unknown_child_turn_notices_and_remote_descendants_fail_closed() {
        let (_dir, manager) = rewind_test_manager().await;
        let links = [rewind_child_link("child")];
        assert!(manager.rewind_children_stopped(&links).await.is_err(), "missing history is not confirmation");
        let mut events = rewind_child_events();
        events.truncate(2);
        manager.journal.import("child", &CreateSession::default(), &events).await.unwrap();
        let error = manager.rewind_children_stopped(&links).await.unwrap_err().to_string();
        assert!(error.contains("terminal turn boundary"), "{error}");
        manager
            .journal
            .append("child", &ev(3, session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: TurnEndReason::WorkerRestarted as i32 })))
            .await
            .unwrap();
        assert!(manager.rewind_children_stopped(&links).await.is_err(), "a recovery marker is not stop confirmation");
        manager
            .journal
            .append("child", &ev(4, session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: TurnEndReason::Completed as i32 })))
            .await
            .unwrap();
        manager
            .journal
            .append(
                "child",
                &ev(
                    5,
                    session_event::Kind::NoticeQueued(c6_proto::NoticeQueued { command_id: "pending-wake".into(), ..Default::default() }),
                ),
            )
            .await
            .unwrap();
        assert!(manager.rewind_children_stopped(&links).await.unwrap_err().to_string().contains("pending notices"));
        manager
            .journal
            .append(
                "child",
                &ev(6, session_event::Kind::User(c6_proto::UserMessage { command_id: "pending-wake".into(), ..Default::default() })),
            )
            .await
            .unwrap();
        manager
            .journal
            .append("child", &ev(7, session_event::Kind::TurnEnded(TurnEnded { turn: 2, reason: TurnEndReason::Completed as i32 })))
            .await
            .unwrap();
        manager
            .journal
            .append(
                "child",
                &ev(
                    8,
                    session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned {
                        child_session_id: "remote-grandchild".into(),
                        remote: Some(Default::default()),
                        ..Default::default()
                    }),
                ),
            )
            .await
            .unwrap();
        let error = manager.rewind_children_stopped(&links).await.unwrap_err().to_string();
        assert!(error.contains("remote-grandchild") && error.contains("Remote subagent"), "{error}");
    }

    #[test]
    fn rewind_lost_child_process_with_unknown_identity_is_not_confirmed_stopped() {
        let mut events = rewind_child_events();
        for (seq, state) in [(4, "started"), (5, "lost")] {
            events.push(ev(
                seq,
                session_event::Kind::BackgroundTask(BackgroundTask {
                    task_id: "bg1".into(),
                    kind: "shell".into(),
                    state: state.into(),
                    ..Default::default()
                }),
            ));
        }
        let error = rewind_child_history_stopped("child", &events, &[]).unwrap_err().to_string();
        assert!(error.contains("without confirming its process stopped"), "{error}");
    }

    fn assistant(seq: u64, blocks: Vec<content_block::Block>) -> SessionEvent {
        let blocks = blocks.into_iter().map(|b| ContentBlock { block: Some(b), ..Default::default() }).collect();
        ev(seq, session_event::Kind::Assistant(AssistantMessage { turn: 1, blocks, stop_reason: String::new() }))
    }
    fn switched(seq: u64, m: ModelSpec, handoff: Option<Handoff>) -> SessionEvent {
        ev(seq, session_event::Kind::ModelSwitched(ModelSwitched { model: Some(m), context_window: 0, handoff }))
    }
    fn into_claude(id: &str) -> Handoff {
        Handoff {
            tool_profile: crate::claude_code::PROFILE.into(),
            system_prompt: crate::claude_code::SYSTEM_PROMPT.into(),
            claude_session_id: id.into(),
            ..Default::default()
        }
    }
    fn into_own() -> Handoff {
        Handoff { tool_profile: "generic".into(), tools: vec!["bash".into()], system_prompt: "carbon6 prompt".into(), ..Default::default() }
    }
    fn texts(history: &[Message]) -> Vec<String> {
        history
            .iter()
            .flat_map(|m| &m.blocks)
            .map(|b| match b {
                Block::Text(t) => t.clone(),
                Block::ToolUse { name, .. } => format!("tool_use:{name}"),
                Block::ToolResult { content, .. } => format!("tool_result:{content}"),
                Block::SignedThinking { text, .. } => format!("thinking:{text}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// A session on carbon6's loop, handed to Claude Code, then back.
    fn log() -> Vec<SessionEvent> {
        let own = model("m", "gpt-x", "openai_compat");
        vec![
            ev(
                1,
                session_event::Kind::Started(SessionStarted {
                    model: Some(own.clone()),
                    tool_profile: "generic".into(),
                    ..Default::default()
                }),
            ),
            user(2, "fix the cache"),
            assistant(
                3,
                vec![
                    content_block::Block::SignedThinking(c6_proto::SignedThinking { text: "private plan".into(), signature: "S".into() }),
                    content_block::Block::ToolUse(c6_proto::ToolUse {
                        id: "call_1".into(),
                        name: "bash".into(),
                        input_json: "{\"command\":\"ls\"}".into(),
                    }),
                ],
            ),
            ev(
                4,
                session_event::Kind::ToolResult(c6_proto::ToolResult {
                    call_id: "call_1".into(),
                    tool: "bash".into(),
                    output: "cache.rs".into(),
                    ..Default::default()
                }),
            ),
            assistant(5, vec![content_block::Block::Text("Found cache.rs.".into())]),
            ev(6, session_event::Kind::Usage(Usage { input_tokens: 90_000, ..Default::default() })),
            switched(7, model("cc", "opus", "claude_code"), Some(into_claude("conv-1"))),
            user(8, "continue in claude"),
            ev(9, session_event::Kind::TurnStarted(TurnStarted { turn: 2, ..Default::default() })),
            assistant(
                10,
                vec![content_block::Block::ToolUse(c6_proto::ToolUse {
                    id: "toolu_1".into(),
                    name: "Edit".into(),
                    input_json: "{}".into(),
                })],
            ),
            ev(
                11,
                session_event::Kind::ToolResult(c6_proto::ToolResult {
                    call_id: "toolu_1".into(),
                    tool: "Edit".into(),
                    output: "edited".into(),
                    ..Default::default()
                }),
            ),
            assistant(12, vec![content_block::Block::Text("Claude fixed it.".into())]),
        ]
    }

    #[test]
    fn claude_code_gets_a_conversation_of_its_own_with_the_transcript() {
        let events = log();
        let spec = CreateSession::default();
        // Its first turn after the handoff: a new conversation, told the story so far.
        let first = claude_conversation("session-1", &spec, &events[..8]);
        assert_eq!((first.id.as_str(), first.resume), ("conv-1", false));
        let p = first.preamble.unwrap();
        assert!(p.starts_with("<carbon6-handoff from=\"carbon6's own agent loop (gpt-x)\">"), "{p}");
        assert!(p.contains("fix the cache") && p.contains("[tool call bash: {\"command\":\"ls\"}]") && p.contains("cache.rs"), "{p}");
        assert!(p.contains("Found cache.rs.") && p.ends_with("The next message follows:"), "{p}");
        assert!(!p.contains("private plan") && !p.contains("continue in claude"), "{p}");
        // Later turns resume it.
        let later = claude_conversation("session-1", &spec, &events);
        assert_eq!((later.id.as_str(), later.resume), ("conv-1", true));
        // A session created on Claude Code keeps its own id, as before.
        let cc = vec![ev(1, session_event::Kind::Started(SessionStarted { tool_profile: "claude_code".into(), ..Default::default() }))];
        let c = claude_conversation("session-1", &spec, &cc);
        assert_eq!((c.id.as_str(), c.resume, c.preamble), ("session-1", false, None));
        let mut cc = cc;
        cc.push(ev(2, session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })));
        assert!(claude_conversation("session-1", &spec, &cc).resume);
    }

    #[test]
    fn back_in_carbon6s_loop_the_past_is_a_transcript_not_history() {
        let mut events = log();
        events.push(switched(13, model("m", "gpt-x", "openai_compat"), Some(into_own())));
        events.push(user(14, "now on gpt again"));
        let mut spec = CreateSession::default();
        let conv = conversation_from_log(&mut spec, &events);
        assert_eq!(spec.model.as_ref().map(|m| m.connection_kind.as_str()), Some("openai_compat"));
        let t = texts(&conv.history);
        // One user message: the marked transcript, then what you said since.
        assert_eq!(conv.history.len(), 1, "{t:?}");
        assert!(t[0].starts_with("<carbon6-handoff from=\"Claude Code (opus)\">"), "{}", t[0]);
        for seen in ["fix the cache", "Found cache.rs.", "continue in claude", "[tool call Edit: {}]", "edited", "Claude fixed it."] {
            assert!(t[0].contains(seen), "{seen} missing: {}", t[0]);
        }
        assert_eq!(t[1], "now on gpt again");
        // No native blocks from before: Claude Code's tool calls, the
        // first run's reasoning and calls are text only.
        assert!(!t.iter().any(|b| b.starts_with("tool_use:") || b.starts_with("tool_result:") || b.starts_with("thinking:")), "{t:?}");
        // The other runtime's context size is not this one's.
        assert_eq!(conv.context_tokens, 0);
        // The runtime a reload resumes on: the last handoff's.
        let started = runtime_of(SessionStarted { tool_profile: "generic".into(), ..Default::default() }, &events[..12]);
        assert_eq!((started.tool_profile.as_str(), started.system_prompt.as_str()), ("claude_code", crate::claude_code::SYSTEM_PROMPT));
        let started = runtime_of(started, &events);
        assert_eq!(
            (started.tool_profile.as_str(), started.tools.as_slice(), started.system_prompt.as_str()),
            ("generic", &["bash".to_owned()][..], "carbon6 prompt")
        );
    }

    #[test]
    fn handoffs_and_model_switches_keep_the_hosted_search_denial() {
        let mut events = log();
        events.push(switched(13, model("m", "gpt-x", "openai_compat"), Some(into_own())));
        events.push(switched(14, model("n", "claude-sonnet-5-5", "anthropic"), None));
        for off in [false, true] {
            let mut started = SessionStarted { hosted_search_off: off, ..Default::default() };
            apply_handoff(&mut started, &into_claude("conv-1"));
            assert_eq!(started.hosted_search_off, off);
            let started = runtime_of(started, &events);
            assert_eq!(started.hosted_search_off, off);
        }
    }

    #[test]
    fn an_imported_claude_code_session_goes_on_in_its_own_conversation() {
        let started = SessionStarted { command_id: "import-claude_code".into(), ..Default::default() };
        let imported = vec![
            ev(1, session_event::Kind::Started(started.clone())),
            ev(
                2,
                session_event::Kind::User(UserMessage {
                    command_id: "import-2".into(),
                    text: "fix the build".into(),
                    ..Default::default()
                }),
            ),
            ev(3, session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })),
            assistant(4, vec![content_block::Block::Text("Fixed.".into())]),
        ];
        assert!(continues_claude_import(&started, &imported));
        // Not once carbon6 has run it, nor for Codex's.
        let mut ran = imported.clone();
        ran.push(ev(5, session_event::Kind::Checkpoint(c6_proto::Checkpoint::default())));
        assert!(!continues_claude_import(&started, &ran));
        let codex = SessionStarted { command_id: "import-codex".into(), ..Default::default() };
        assert!(!continues_claude_import(&codex, &imported));
        // Its handoff resumes Claude Code's conversation by the session's
        // id, with the transcript kept for a Claude Code that lost it.
        let mut events = imported;
        let handoff = Handoff { native: true, ..into_claude("session-1") };
        events.push(switched(5, model("cc", "opus", "claude_code"), Some(handoff)));
        let c = claude_conversation("session-1", &CreateSession::default(), &events);
        assert_eq!((c.id.as_str(), c.resume), ("session-1", true));
        assert!(c.preamble.unwrap().contains("fix the build"));
    }

    #[test]
    fn a_switch_within_a_runtime_keeps_the_history_native() {
        let events = log()[..6].to_vec();
        let mut events = events;
        events.push(switched(7, model("m", "gpt-y", "openai_compat"), None));
        events.push(user(8, "on gpt-y"));
        let mut spec = CreateSession::default();
        let conv = conversation_from_log(&mut spec, &events);
        let t = texts(&conv.history);
        assert!(!t.iter().any(|b| b.contains("<carbon6-handoff")), "{t:?}");
        // Tool calls stay native; the other model's signed reasoning goes.
        assert!(t.contains(&"tool_use:bash".to_owned()) && t.contains(&"tool_result:cache.rs".to_owned()), "{t:?}");
        assert!(!t.iter().any(|b| b.starts_with("thinking:")), "{t:?}");
        assert_eq!(conv.context_tokens, 90_000);
        assert_eq!(runtime_of(SessionStarted { tool_profile: "generic".into(), ..Default::default() }, &events).tool_profile, "generic");
    }
}

#[cfg(test)]
mod writer_tests {
    use c6_proto::{AssistantMessage, TextDelta, WorkerMsg, worker_msg};
    use sqlx::Connection;

    use super::*;

    #[tokio::test]
    async fn keep_warm_clock_reanchors_after_sleep_and_remains_injectable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        tokio::time::pause();
        let writer = Writer::new("s".into(), journal, Arc::new(HeadLink::default()), 1);
        // Model a monotonic clock that did not count the suspended interval.
        writer.cache_clock.0.store(1, Ordering::Relaxed);
        let wall = c6_core::now_ms();
        let before = writer.now_ms();
        assert!(before >= wall, "wall time must expire a timer after machine sleep");
        tokio::time::advance(Duration::from_secs(3601)).await;
        assert!(writer.now_ms() >= before + 3_601_000, "paused-clock tests still advance freshness");
    }

    #[test]
    fn keep_warm_journal_rebuild_preserves_wait_cap_and_attempt_claim() {
        let event = |ts_ms, kind| SessionEvent { ts_ms, kind: Some(kind), ..Default::default() };
        let events = [
            event(1_000, session_event::Kind::Usage(c6_proto::Usage::default())),
            event(2_000, session_event::Kind::TurnEnded(TurnEnded { reason: TurnEndReason::Completed as i32, ..Default::default() })),
            event(
                3_000,
                session_event::Kind::Notice(c6_proto::Notice {
                    title: "Cache maintenance attempted".into(),
                    message: "1000".into(),
                    quiet: true,
                    ..Default::default()
                }),
            ),
            event(3_301_000, session_event::Kind::Usage(c6_proto::Usage { keep_warm: true, ..Default::default() })),
        ];
        let mut rebuilt = CacheObservation::default();
        for event in &events {
            rebuilt.observe(event);
        }
        assert_eq!(rebuilt.wait_started_ms, Some(2_000));
        assert_eq!(rebuilt.last_attempt_call_ms, Some(1_000));
        assert_eq!(rebuilt.pings_sent, 1);
        assert_eq!(rebuilt.usage.as_ref().unwrap().0, 3_301_000);
        assert_eq!(rebuilt.last_call_started_ms, Some(3_000), "generation time does not extend the cache lifetime");
        rebuilt.observe(&event(3_302_000, session_event::Kind::User(Default::default())));
        assert_eq!(rebuilt.wait_started_ms, None);
        assert_eq!(rebuilt.pings_sent, 0);
        rebuilt.observe(&event(
            3_303_000,
            session_event::Kind::TurnEnded(TurnEnded { reason: TurnEndReason::Completed as i32, ..Default::default() }),
        ));
        rebuilt.observe(&event(
            3_304_000,
            session_event::Kind::Notice(c6_proto::Notice { title: "Cache maintenance failed".into(), quiet: true, ..Default::default() }),
        ));
        assert_eq!(rebuilt.wait_started_ms, None, "a failed CLI ping cannot resume the warm loop");
    }

    #[test]
    fn keep_warm_opaque_calls_use_a_pre_generation_bound() {
        let mut observation = CacheObservation::default();
        let mut observe = |ts_ms, kind| observation.observe(&SessionEvent { ts_ms, kind: Some(kind), ..Default::default() });
        observe(1_000, session_event::Kind::TurnStarted(Default::default()));
        observe(3_601_000, session_event::Kind::Assistant(Default::default()));
        observe(3_602_000, session_event::Kind::Usage(Default::default()));
        assert_eq!(observation.last_call_started_ms, Some(1_000), "a long answer cannot move a ping past cache expiry");
        observation.observe(&SessionEvent {
            ts_ms: 3_603_000,
            kind: Some(session_event::Kind::ToolResult(Default::default())),
            ..Default::default()
        });
        observation.observe(&SessionEvent {
            ts_ms: 3_604_000,
            kind: Some(session_event::Kind::Assistant(Default::default())),
            ..Default::default()
        });
        observation.observe(&SessionEvent {
            ts_ms: 3_605_000,
            kind: Some(session_event::Kind::Usage(Default::default())),
            ..Default::default()
        });
        assert_eq!(observation.last_call_started_ms, Some(3_603_000));
        for kind in [
            session_event::Kind::ModelSwitched(Default::default()),
            session_event::Kind::PlaceChanged(Default::default()),
            session_event::Kind::Compaction(Default::default()),
            session_event::Kind::Notice(c6_proto::Notice {
                title: "Trimmed 2 old tool outputs from Claude Code's context".into(),
                quiet: true,
                ..Default::default()
            }),
        ] {
            let mut invalidated = observation.clone();
            invalidated.observe(&SessionEvent { kind: Some(kind), ..Default::default() });
            assert!(invalidated.usage.is_none(), "rewritten or switched prefixes cannot reuse previous cache evidence");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn mailbox_probe_times_out_without_queuing_a_duplicate_and_recovers_on_ack() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut probe = ActorProbe::default();
        let now = tokio::time::Instant::now;
        assert!(!probe.timed_out(&tx, now()));
        let ActorMsg::HealthProbe(reply) = rx.try_recv().unwrap() else { panic!("expected a health probe") };

        tokio::time::advance(ACTOR_PROBE_TIMEOUT - Duration::from_secs(1)).await;
        assert!(!probe.timed_out(&tx, now()));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(probe.timed_out(&tx, now()));
        assert!(rx.try_recv().is_err());

        reply.send(()).unwrap();
        assert!(!probe.timed_out(&tx, now()));
        assert!(matches!(rx.try_recv(), Ok(ActorMsg::HealthProbe(_))));
    }

    #[tokio::test]
    async fn activity_stays_tool_until_all_calls_finish() {
        use c6_proto::{ContentBlock, SessionActivityKind, ToolResult, ToolUse, content_block};
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let writer = Writer::new("s".into(), journal, Arc::new(HeadLink::default()), 1);
        let activity = || SessionActivityKind::try_from(writer.activity.load(Ordering::Relaxed)).unwrap();
        let result = |id: &str| session_event::Kind::ToolResult(ToolResult { call_id: id.into(), ..Default::default() });
        writer.set_activity(SessionActivityKind::Model);
        assert_eq!(activity(), SessionActivityKind::Model);
        writer.observe_activity(&session_event::Kind::Assistant(AssistantMessage {
            blocks: ["first", "second"]
                .into_iter()
                .map(|id| ContentBlock {
                    block: Some(content_block::Block::ToolUse(ToolUse { id: id.into(), ..Default::default() })),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }));
        assert_eq!(activity(), SessionActivityKind::Tool);
        writer.observe_activity(&result("second"));
        writer.observe_activity(&result("second"));
        assert_eq!(activity(), SessionActivityKind::Tool);
        writer.observe_activity(&result("first"));
        assert_eq!(activity(), SessionActivityKind::Unspecified);
        writer.set_activity(SessionActivityKind::Model);
        writer.observe_activity(&result("unrelated"));
        assert_eq!(activity(), SessionActivityKind::Model);
        for name in ["task", "Task", "send_message", "SendMessage", "mcp__carbon6__Task", "tools.send_message", "tools::SendMessage"] {
            writer.observe_activity(&session_event::Kind::Assistant(AssistantMessage {
                blocks: vec![ContentBlock {
                    block: Some(content_block::Block::ToolUse(ToolUse { id: "delegate".into(), name: name.into(), ..Default::default() })),
                    ..Default::default()
                }],
                ..Default::default()
            }));
            assert_eq!(activity(), SessionActivityKind::Subagent, "{name}");
            writer.observe_activity(&session_event::Kind::Assistant(AssistantMessage {
                blocks: vec![ContentBlock {
                    block: Some(content_block::Block::ToolUse(ToolUse { id: "command".into(), name: "bash".into(), ..Default::default() })),
                    ..Default::default()
                }],
                ..Default::default()
            }));
            assert_eq!(activity(), SessionActivityKind::Tool, "mixed outstanding calls");
            writer.observe_activity(&result("command"));
            assert_eq!(activity(), SessionActivityKind::Subagent);
            writer.observe_activity(&result("delegate"));
            assert_eq!(activity(), SessionActivityKind::Unspecified);
        }
    }

    #[tokio::test]
    async fn delegated_turn_end_waits_for_background_start_to_be_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let started = SessionEvent { seq: 1, kind: Some(session_event::Kind::Started(SessionStarted::default())), ..Default::default() };
        journal.create_session("s", &CreateSession::default(), &started).await.unwrap();
        let writer = Arc::new(Writer::new("s".into(), journal.clone(), Arc::new(HeadLink::default()), 2));
        let (task_tx, task_rx) = mpsc::unbounded_channel();
        let (fence, fence_rx) = mpsc::unbounded_channel();
        let (actor_tx, mut actor_rx) = mpsc::unbounded_channel();
        let background = c6_tools::Background::default();
        background.set_sink(Some(task_tx));
        let (task_id, _) = background.start_agent("nested work", true);
        let sink = TurnSink { writer: writer.clone(), inbox: Arc::default(), task_fence: Some(fence) };
        sink.start_turn(1, &[]).await.unwrap();
        let end = tokio::spawn(async move {
            sink.append(session_event::Kind::TurnEnded(TurnEnded { turn: 1, reason: TurnEndReason::Completed as i32 })).await
        });
        let forwarder = tokio::spawn(forward_tasks(task_rx, fence_rx, actor_tx));
        let Some(ActorMsg::Task(task)) = actor_rx.recv().await else { panic!("start must precede the fence") };
        assert_eq!(task.id, task_id);
        let Some(ActorMsg::TaskFence(ack)) = actor_rx.recv().await else { panic!("expected the turn's fence") };
        assert!(!end.is_finished());
        let task_seq = writer
            .append(session_event::Kind::BackgroundTask(BackgroundTask {
                task_id,
                state: "started".into(),
                notify: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        ack.send(()).unwrap();
        assert!(end.await.unwrap().unwrap() > task_seq);
        forwarder.await.unwrap();
    }

    #[tokio::test]
    async fn initial_turn_failure_keeps_the_whole_unsent_batch() {
        for fail_boundary in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("journal.db");
            let journal = Arc::new(Journal::open(&path).await.unwrap());
            let started =
                SessionEvent { seq: 1, kind: Some(session_event::Kind::Started(SessionStarted::default())), ..Default::default() };
            journal.create_session("s", &CreateSession::default(), &started).await.unwrap();
            let writer = Arc::new(Writer::new("s".into(), journal.clone(), Arc::new(HeadLink::default()), 2));
            let inputs: Vec<_> = ["first", "second"]
                .into_iter()
                .map(|id| PendingInput { command_id: id.into(), text: id.into(), ..Default::default() })
                .collect();
            let inbox = Arc::new(Mutex::new(Inbox::default()));
            for input in &inputs {
                inbox.lock().unwrap().push(input.clone(), None);
            }
            assert_eq!(inbox.lock().unwrap().drain(), inputs);
            let sink = TurnSink { writer: writer.clone(), inbox: inbox.clone(), task_fence: None };
            let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
            let mut fault = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
            let trigger = if fail_boundary {
                "CREATE TRIGGER admission_fault BEFORE UPDATE OF open_turn ON sessions
                 WHEN NEW.id = 's' AND NEW.open_turn IS NOT NULL
                 BEGIN SELECT RAISE(ABORT, 'test TurnStarted failure'); END"
            } else {
                "CREATE TRIGGER admission_fault BEFORE INSERT ON applied_commands
                 WHEN NEW.command_id = 'second'
                 BEGIN SELECT RAISE(ABORT, 'test second User failure'); END"
            };
            sqlx::query(trigger).execute(&mut fault).await.unwrap();

            assert!(sink.start_turn(1, &inputs).await.is_err());
            assert_eq!(journal.events_after("s", 0, u32::MAX).await.unwrap(), [started]);
            assert!(journal.open_turns().await.unwrap().is_empty());
            for input in &inputs {
                assert!(!journal.is_applied(&input.command_id).await.unwrap());
            }
            assert_eq!(*writer.next_seq.lock().await, 2);
            assert_eq!(writer.logged.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert_eq!(inbox.lock().unwrap().in_flight.iter().cloned().collect::<Vec<_>>(), inputs);

            sqlx::query("DROP TRIGGER admission_fault").execute(&mut fault).await.unwrap();
            {
                let mut inbox = inbox.lock().unwrap();
                inbox.recover(&HashSet::new(), &HashSet::new(), false);
                assert_eq!(inbox.drain(), inputs, "retry the complete batch in its original order");
            }
            sink.start_turn(1, &inputs).await.unwrap();
            let events = journal.events_after("s", 1, u32::MAX).await.unwrap();
            assert_eq!(events.iter().map(|event| event.seq).collect::<Vec<_>>(), [2, 3, 4]);
            assert_eq!(
                events.iter().map(|event| event.kind.clone().unwrap()).collect::<Vec<_>>(),
                [inputs[0].event(), inputs[1].event(), session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() }),]
            );
            for input in &inputs {
                assert!(journal.is_applied(&input.command_id).await.unwrap());
            }
            assert_eq!(*writer.next_seq.lock().await, 5);
            assert_eq!(writer.logged.load(std::sync::atomic::Ordering::SeqCst), 5);
            assert!(inbox.lock().unwrap().in_flight.is_empty());
        }
    }

    /// Streamed text says where the log was: a viewer that catches up gets
    /// the reply from the log before the text it streamed, and drops that
    /// text by this instead of drawing the reply twice.
    #[tokio::test]
    async fn streamed_text_says_where_the_log_is() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let started = SessionEvent { seq: 1, ts_ms: 0, kind: Some(session_event::Kind::Started(SessionStarted::default())) };
        journal.create_session("s", &CreateSession::default(), &started).await.unwrap();
        let link = Arc::new(HeadLink::default());
        let (tx, mut rx) = mpsc::channel(8);
        link.connected(tx);
        let writer = Writer::new("s".into(), journal, link.clone(), 2);
        let text = |t: &str| ephemeral_event::Kind::Text(TextDelta { turn: 1, text: t.into(), ..Default::default() });
        writer.ephemeral(text("Hel"));
        writer.ephemeral(text("lo"));
        let reply = AssistantMessage { turn: 1, ..Default::default() };
        assert_eq!(writer.append(session_event::Kind::Assistant(reply)).await.unwrap(), 2);
        writer.ephemeral(text("Next"));
        link.flush_ephemeral();
        let mut sent = Vec::new();
        while let Ok(WorkerMsg { msg: Some(worker_msg::Msg::Ephemeral(ev)) }) = rx.try_recv() {
            let Some(ephemeral_event::Kind::Text(d)) = ev.kind else { panic!("not text") };
            sent.push((d.text, d.seq));
        }
        assert_eq!(sent, [("Hello".to_owned(), 2), ("Next".to_owned(), 3)]);
    }
}

#[cfg(test)]
mod inbox_tests {
    use super::*;
    use sqlx::Connection;

    fn input(id: &str, text: &str) -> PendingInput {
        PendingInput { command_id: id.into(), text: text.into(), ..Default::default() }
    }

    /// Editing and canceling a queued input: the swap keeps the entry's
    /// model switch and its place in line; a redelivery of either id is
    /// still ignored.
    #[test]
    fn replace_and_remove() {
        let mut inbox = Inbox::default();
        inbox.push(input("a", "first"), None);
        inbox.push(input("b", "second"), Some(ModelSpec::default()));
        inbox.push(input("c", "third"), None);

        // The swap keeps the entry's model switch and its place in line.
        assert!(matches!(inbox.replace("b", input("b", "second, edited")), QueueOp::Updated));
        let b = inbox.queue.iter().find(|(i, _)| i.command_id == "b").unwrap();
        assert_eq!(b.0.text, "second, edited");
        assert!(b.1.is_some(), "the model switch survives the edit");
        assert_eq!(inbox.queue.iter().map(|(i, _)| i.command_id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);

        assert!(matches!(inbox.replace("zz", input("zz", "no")), QueueOp::Absent));
        assert!(matches!(inbox.remove("zz"), QueueOp::Absent));

        // The edited id was seen: a redelivery of the old payload does not
        // push it back in.
        inbox.push(input("b", "second, the old text"), None);
        assert_eq!(inbox.queue.iter().filter(|(i, _)| i.command_id == "b").count(), 1);

        inbox.push(input("d", "fourth"), None);
        assert!(matches!(inbox.remove("d"), QueueOp::Cancelled));
        // And its redelivery is ignored too.
        inbox.push(input("d", "fourth"), None);
        assert!(inbox.queue.iter().all(|(i, _)| i.command_id != "d"));
    }

    #[test]
    fn recovery_keeps_order_switches_and_deduplication() {
        let mut inbox = Inbox::default();
        inbox.push(input("taken", "first"), None);
        inbox.push(input("retry", "second"), None);
        inbox.push(PendingInput { from_session_id: "old-child".into(), ..input("withdrawn", "third") }, None);
        let model = ModelSpec { model: "next".into(), ..Default::default() };
        inbox.push(input("switch", "fourth"), Some(model.clone()));
        assert_eq!(inbox.drain().len(), 3);
        assert_eq!(inbox.next_switch(), Some(("switch".into(), model.clone())));
        inbox.push(input("cancel", "fifth"), None);
        assert!(matches!(inbox.remove("cancel"), QueueOp::Cancelled));

        inbox.recover(&HashSet::from(["taken"]), &HashSet::from(["old-child".into()]), false);
        for id in ["taken", "retry", "withdrawn", "switch", "cancel"] {
            inbox.push(input(id, "duplicate"), None);
        }
        assert_eq!(inbox.drain(), vec![input("retry", "second")]);
        assert_eq!(inbox.next_switch(), Some(("switch".into(), model)));
        assert!(inbox.drain().is_empty(), "the model write has not succeeded yet");
        inbox.finish_switch("switch");
        assert_eq!(inbox.drain(), vec![input("switch", "fourth")]);
    }

    async fn delegated_append_failure(fail_turn_start: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Arc::new(Journal::open(&path).await.unwrap());
        let manager = SessionManager::new(journal.clone(), Arc::new(HeadLink::default()), dir.path().join("worktrees"));
        let cwd = dir.path().display().to_string();
        let spec = CreateSession { cwd: cwd.clone(), ..Default::default() };
        let started = SessionEvent {
            seq: 1,
            kind: Some(session_event::Kind::Started(SessionStarted { cwd, tool_profile: "generic".into(), ..Default::default() })),
            ..Default::default()
        };
        journal.create_session("parent", &spec, &started).await.unwrap();
        let child_spec = CreateSession {
            delegation: Some(c6_proto::Delegation {
                parent_session_id: "parent".into(),
                root_session_id: "parent".into(),
                ..Default::default()
            }),
            ..spec
        };
        journal.create_session("child", &child_spec, &started).await.unwrap();
        let child = manager.load("child").await.unwrap().unwrap();
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).busy_timeout(std::time::Duration::from_secs(5));
        let mut fault = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
        let trigger = if fail_turn_start {
            "CREATE TRIGGER input_fault BEFORE UPDATE OF open_turn ON sessions
                WHEN NEW.id = 'child' AND NEW.open_turn IS NOT NULL BEGIN SELECT RAISE(ABORT, 'test TurnStarted failure'); END"
        } else {
            "CREATE TRIGGER input_fault BEFORE INSERT ON applied_commands
                WHEN NEW.command_id = 'delegated-input' BEGIN SELECT RAISE(ABORT, 'test User append failure'); END"
        };
        sqlx::query(trigger).execute(&mut fault).await.unwrap();
        let (done, outcome) = tokio::sync::oneshot::channel();
        let delegated = PendingInput { delegated_by: "parent-call".into(), ..input("delegated-input", "do not redeliver me") };
        child.tx.send(ActorMsg::Delegate(Box::new(delegated), Some(done), None)).unwrap();

        c6_testkit::eventually("delegate ran into the failed append", async || {
            let attempted = if fail_turn_start {
                journal.is_applied("delegated-input").await.unwrap()
            } else {
                child.inbox.lock().unwrap().in_flight.iter().any(|i| i.command_id == "delegated-input")
            };
            (attempted && !child.running.load(std::sync::atomic::Ordering::SeqCst)).then_some(())
        })
        .await;
        sqlx::query("DROP TRIGGER input_fault").execute(&mut fault).await.unwrap();

        let outcome = c6_testkit::within("the retained delegation to receive its outcome", outcome).await.unwrap();
        assert_eq!(outcome.reason, TurnEndReason::Error);
        if fail_turn_start {
            assert_eq!(outcome.error, JOURNAL_FAILURE_DIAGNOSTIC);
        } else {
            // The only failure after retry is the intentional lack of a head.
            assert!(outcome.error.starts_with("cannot reach the model:"), "{}", outcome.error);
        }
        let events = journal.events_after("child", 0, u32::MAX).await.unwrap();
        assert_eq!(
            events.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::User(u)) if u.command_id == "delegated-input")).count(),
            1
        );
        assert!(!child.tx.is_closed(), "the original mailbox remained live");
    }

    #[tokio::test]
    async fn replays_delegated_input_if_the_user_append_failed() {
        delegated_append_failure(false).await;
    }

    #[tokio::test]
    async fn settles_delegated_input_if_turn_start_failed_after_the_user_append() {
        delegated_append_failure(true).await;
    }
}

#[cfg(test)]
mod rewind_queue_tests {
    use super::*;

    #[test]
    fn claude_recovery_requires_durable_retry_and_keeps_original_first() {
        let original = ("a".to_owned(), 7, true);
        let retained = VecDeque::from([original.clone()]);
        let failed = crate::claude_code::Reset {
            command_id: original.0.clone(),
            to_seq: original.1,
            restore_files: original.2,
            failure: "explicit retry required".into(),
            ..Default::default()
        };
        assert!(recover_rewind_queue(true, retained.clone(), Some((failed.clone(), 0))).is_empty());
        let authorized = crate::claude_code::Reset { retries: vec!["b".into()], ..failed };
        assert_eq!(recover_rewind_queue(true, retained, Some((authorized, 0))), VecDeque::from([original, ("b".into(), 7, true)]));
    }

    #[test]
    fn native_recovery_keeps_retained_rewinds_in_order() {
        let retained = VecDeque::from([("a".into(), 7, true), ("b".into(), 3, false)]);
        assert_eq!(recover_rewind_queue(false, retained.clone(), None), retained);
    }
}
