//! Session runtime: one actor task per active session, each the single writer
//! of its event log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
use crate::journal::Journal;
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
    /// Delegations running per top-level session (for its limit).
    working: Arc<Mutex<HashMap<String, usize>>>,
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
        let mut c = tokio::process::Command::new("git");
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
                match tokio::process::Command::new("git").arg("-C").arg(cwd).args(["remote", "get-url", &first]).output().await {
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

pub(crate) enum ActorMsg {
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
    /// Send now for an input already with the agent (its command id).
    SendNow(String),
    /// A background task started or ended.
    Task(c6_tools::TaskEvent),
    /// A background task that was running when the worker restarted.
    Lost {
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
    Delegate(Box<PendingInput>, Option<tokio::sync::oneshot::Sender<Outcome>>),
    /// What a subagent did in a turn no call waited on, for its parent.
    ChildReport {
        command_id: String,
        text: String,
        wake: bool,
        task_notice: Option<c6_proto::TaskNotice>,
        recorded: tokio::sync::oneshot::Sender<()>,
    },
    /// Replay a local report whose turn ended before delivery completed.
    RecoverReport,
    /// Context for the next input, never a reason to start a turn.
    /// How a remote subagent's turn ended (from the head, as a command).
    RemoteReport(String, Box<c6_proto::DelegationEnded>, tokio::sync::oneshot::Sender<Result<(), String>>),
    /// Deliver the held notices now, whatever the hourly limit (you asked).
    ReleaseNotices,
    /// Its turn `.0` was cut short by a worker restart: it goes on
    /// (docs/restarts.md §4).
    Resume(u64),
    /// From the log, on reload (docs/background.md §5): when notices
    /// started turns in the last hour, the ids of every notice recorded,
    /// and those no turn took, to deliver again.
    Reloaded {
        auto_turns_ms: Vec<i64>,
        noticed: Vec<String>,
        pending: Vec<(PendingInput, bool)>,
    },
}

/// Notices may start this many turns per hour in an idle session unless
/// the subagent-limits setting says otherwise; beyond that they wait until
/// the hour allows, or for your next message.
const AUTO_TURNS_PER_HOUR: usize = c6_api::DEFAULT_AUTO_TURNS_PER_HOUR as usize;
const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);
/// Only recently active sessions re-arm held notices at worker startup:
/// don't scan dormant logs or wake days-old conversations on upgrade.
const NOTICE_RECOVERY_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Why a rewind may not go back past a handoff (the head says the same).
const HANDOFF_REWIND: &str = "can only rewind to a message after the last switch between Claude Code and carbon6's agent: \
                              the other runtime's conversation cannot be taken back";

impl SessionManager {
    pub fn new(journal: Arc<Journal>, link: Arc<HeadLink>, worktrees: PathBuf) -> Self {
        let state_dir = worktrees.parent().map(Path::to_path_buf).unwrap_or_else(|| worktrees.clone());
        prune_browser_profiles(&state_dir.join("browser"), std::time::Duration::from_secs(30 * 24 * 3600));
        Self {
            journal,
            link,
            worktrees,
            state_dir,
            actors: Arc::default(),
            working: Arc::default(),
            bridge: Arc::default(),
            remote_waits: Arc::default(),
        }
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

    /// Session ids with a live actor.
    pub fn active(&self) -> Vec<String> {
        self.actors.lock().unwrap().iter().filter(|(_, a)| !a.tx.is_closed()).map(|(id, _)| id.clone()).collect()
    }

    pub(crate) fn journal(&self) -> &Arc<Journal> {
        &self.journal
    }

    /// Counts a delegation in `root`'s tree, if fewer than `max` run.
    pub(crate) fn working(&self, root: &str, max: usize) -> Option<Working> {
        let mut c = self.working.lock().unwrap();
        let n = c.entry(root.to_owned()).or_default();
        if *n >= max {
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

    /// Whether the session's actor is running a turn.
    pub(crate) fn is_running(&self, session_id: &str) -> bool {
        self.actors
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|a| !a.tx.is_closed() && a.running.load(std::sync::atomic::Ordering::SeqCst))
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

    /// Closes turns left open by a crash or kill.
    pub async fn recover(&self) -> anyhow::Result<()> {
        // Claude Code processes the previous worker left go first: their
        // sessions resume their conversations in processes of their own.
        crate::claude_code::sweep_orphans(&self.claude_dir()).await;
        let mut cut_short = Vec::new();
        for (session_id, last_seq, turn) in self.journal.open_turns().await? {
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
                self.journal.append(&session_id, &ev).await?;
            }
            let ev = SessionEvent {
                seq: seq + 1,
                ts_ms: c6_core::now_ms(),
                kind: Some(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::WorkerRestarted as i32 })),
            };
            self.journal.append(&session_id, &ev).await?;
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
            match self.actor(&session_id).await {
                Ok(Some(tx)) => {
                    let _ = tx.send(ActorMsg::Resume(turn));
                }
                Ok(None) => {}
                Err(e) => warn!(session = %session_id, "resuming its turn: {e:#}"),
            }
        }
        // Claude Code processes the previous worker held went on: their
        // sessions take them up (docs/restarts.md §3).
        for session_id in crate::claude_code::Process::held_sessions(&self.claude_dir().join("held")) {
            if let Err(e) = self.actor(&session_id).await {
                warn!(session = %session_id, "taking up its Claude Code process: {e:#}");
            }
        }
        // Background tasks died with the previous process: resuming those
        // sessions reports them as lost.
        for session_id in self.journal.sessions_with_open_tasks().await? {
            if let Err(e) = self.actor(&session_id).await {
                warn!(session = %session_id, "resuming for lost background tasks: {e:#}");
            }
        }
        // An idle session with only held notices has no open task to load
        // it above. Restore its actor too, so its hourly timer is armed
        // without waiting for another command from the head. Only scan
        // recently active sessions; dormant notices wait for explicit use.
        for session_id in self.journal.recent_session_ids(c6_core::now_ms() - NOTICE_RECOVERY_WINDOW_MS).await? {
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
                let outcome = last_turn_outcome(&events[..=*end]);
                let status = crate::subagent::status_of(outcome.reason, &outcome.report);
                !inputs.iter().any(|u| report_recorded(&parent, &u.delegated_by, &session_id, status))
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
                    output: crate::subagent::partial(&spec, &digest),
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
        match cmd.kind {
            Some(session_command::Kind::Create(spec)) => {
                if self.journal.has_session(&cmd.session_id).await.map_err(internal)? {
                    return Ok(()); // redelivered
                }
                self.create(&cmd.session_id, &cmd.command_id, spec).await
            }
            Some(session_command::Kind::Input(input)) => {
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
                                .authorize_claude_rewind_retry(&cmd.session_id, &pending.command_id, &cmd.command_id)
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
                                .prepare_claude_rewind(
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
            Some(session_command::Kind::ReleaseNotices(_)) => {
                // Loading it delivers what it can; then the rest, now.
                let actor = self.actor(&cmd.session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
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
            None => Err("unsupported command".into()),
        }
    }

    async fn create(&self, session_id: &str, command_id: &str, spec: CreateSession) -> Result<(), String> {
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
        spec: CreateSession,
        place: Place,
        labels: Vec<String>,
        system_prompt: String,
    ) -> Result<(), String> {
        let Place { cwd, local, home, repo, worktree_branch, repo_root, ssh_target, system, setup_log } = place;
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
            let names = built.tools.iter().map(|t| t.spec().name).collect();
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
        self.spawn(session_id, spec, started, conv, next_seq, 0, Some(session_tools)).await.map_err(|e| format!("worker error: {e:#}"))?;
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
                        "Remote subagent {} cannot confirm quiescence; stop confirmation is required before rewinding",
                        child.child_session_id
                    );
                    anyhow::ensure!(!child.child_session_id.is_empty(), "A subagent's identity is unknown; cannot confirm it stopped");
                    if seen.insert(child.child_session_id.clone()) {
                        pending.push(child.child_session_id.clone());
                    }
                }
            }
            let Some(id) = pending.pop() else { return Ok(()) };
            self.journal.confirm_shell_writers_stopped(&id).await?;
            let handle = self.actors.lock().unwrap().get(&id).filter(|h| !h.tx.is_closed()).cloned();
            let registry = if let Some(handle) = handle {
                anyhow::ensure!(
                    !handle.running.load(std::sync::atomic::Ordering::SeqCst)
                        && handle.inbox.lock().unwrap().queue.is_empty()
                        && !handle.tools.background.list().iter().any(|t| t.running),
                    "Subagent {id} is still working; stop or wait for it before retrying Rewind"
                );
                // A mailbox barrier also covers inputs/notices not yet moved
                // into Inbox and notices held on the actor's hourly timer.
                let (reply, idle) = tokio::sync::oneshot::channel();
                handle.tx.send(ActorMsg::RewindIdle(reply)).map_err(|_| anyhow::anyhow!("Subagent {id} stopped before confirming idle"))?;
                let idle = tokio::time::timeout(std::time::Duration::from_secs(5), idle)
                    .await
                    .map_err(|_| anyhow::anyhow!("Subagent {id} did not confirm idle"))?
                    .map_err(|_| anyhow::anyhow!("Subagent {id} stopped before confirming idle"))?;
                anyhow::ensure!(idle, "Subagent {id} has pending work; stop or wait for it before retrying Rewind");
                if seal_contexts {
                    handle.tools.background.cancel_shell_contexts();
                }
                handle.tools.background.list()
            } else {
                Vec::new()
            };
            let Some((_, events)) = self.journal.load_session(&id).await? else {
                anyhow::bail!("Subagent {id} has no local history; cannot confirm it stopped");
            };
            rewind_child_history_stopped(&id, &events, &registry)?;
            descendants = events;
        }
    }

    /// The session's live actor and tools, starting it from the journal if
    /// needed.
    async fn load(&self, session_id: &str) -> anyhow::Result<Option<Handle>> {
        if let Some(a) = self.actors.lock().unwrap().get(session_id).filter(|a| !a.tx.is_closed()) {
            return Ok(Some(a.clone()));
        }
        let Some((spec, mut events)) = self.journal.load_session(session_id).await? else { return Ok(None) };
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
            self.journal.append(session_id, &event).await?;
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
            self.journal.append(session_id, &event).await?;
            events.push(event);
            next_seq += 1;
        }
        let turns = events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_)))).count() as u64;
        let mut spec = spec;
        let local_child = spec.delegation.is_some();
        let conv = conversation_from_log(&mut spec, &events);
        let handle = self.spawn(session_id, spec, started, conv, next_seq, turns, None).await?;
        let tx = handle.tx.clone();
        // Background tasks still running when the worker stopped are gone.
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
        let (auto_turns_ms, noticed, pending) = notices_of(&events);
        let _ = tx.send(ActorMsg::Reloaded { auto_turns_ms, noticed, pending });
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
        Ok(self.spawn(session_id, spec, started, conv, 3, 0, Some(tools)).await?.tx)
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
    ) -> anyhow::Result<Handle> {
        let (tx, rx) = mpsc::unbounded_channel();
        let delegation = spec.delegation.clone();
        // A subagent's secrets and head calls go by its tree's top-level
        // session (its row always exists on the head).
        let on_behalf_of = delegation.as_ref().map_or(session_id, |d| d.root_session_id.as_str()).to_owned();
        let writer = Arc::new(Writer::new(session_id.to_owned(), self.journal.clone(), self.link.clone(), next_seq));
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
                // A subagent keeps the tools it started with.
                match &delegation {
                    Some(_) => built.into_iter().filter(|t| started.tools.contains(&t.spec().name)).collect(),
                    None => built,
                }
            }
        };
        let started_cwd = started.cwd.clone();
        // The head's hooks. Claude Code sessions get them as Claude Code
        // settings and run them itself.
        let labels = session_labels(&spec.labels, &started.repo, &started.cwd, &started.home, &spawn_model(&spec), &started.ssh_target);
        let ssh = remote.then(|| Arc::new(c6_tools::remote::SshHost::new(&started.ssh_target, &started.home)));
        let session_tools = Arc::new(SessionTools::with_ssh(PathBuf::from(&started.cwd), ssh));
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
        // subagent of one runs the head's hooks and policies itself, but
        // what it publishes, remembers and notifies is still its parent's.
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
        if parent.is_none() || parent_claude_code {
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
        {
            // Background tasks report to the actor.
            let (task_tx, mut task_rx) = mpsc::unbounded_channel();
            session_tools.background.set_sink(Some(task_tx));
            let actor_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = task_rx.recv().await {
                    if actor_tx.send(ActorMsg::Task(ev)).is_err() {
                        break;
                    }
                }
            });
        }
        let spec_default = spec.clone();
        let claude_code = Arc::new(std::sync::atomic::AtomicBool::new(started.tool_profile == crate::claude_code::PROFILE));
        let actor = Actor {
            spec,
            session_tools,
            hooks,
            policies,
            tools,
            journal: self.journal.clone(),
            started,
            conv: Some(conv),
            turns,
            writer,
            link: self.link.clone(),
            inbox: Arc::new(Mutex::new(Inbox::default())),
            // Files on an SSH target are not checkpointed.
            checkpoints: Arc::new(Checkpoints::new(&self.state_dir, session_id, if remote { &local } else { Path::new(&started_cwd) })),
            auto_turns: VecDeque::new(),
            deferred: Vec::new(),
            noticed: HashSet::new(),
            on_behalf_of,
            waiting: Vec::new(),
            handed_reports: HashSet::new(),
            remote_receipts: Vec::new(),
            manager: self.clone(),
            subagent_default: Arc::new(Mutex::new(crate::subagent::SubagentDefault::of(&spec_default))),
            running_flag: Arc::default(),
            claude_code: claude_code.clone(),
            claude: None,
            withdrawn: HashSet::new(),
            claude_stdin: Arc::default(),
            handoff_announced: false,
            stopping: false,
            stopped_until: None,
        };
        let handle =
            Handle { tx, tools: actor.session_tools.clone(), running: actor.running_flag.clone(), claude_code, inbox: actor.inbox.clone() };
        tokio::spawn(actor.run(rx));
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

/// The runtime a session runs on as of its log: its `Started`, with the
/// tool profile, tools and system prompt of its last handoff.
fn runtime_of(mut started: SessionStarted, events: &[SessionEvent]) -> SessionStarted {
    if let Some(h) = events.iter().rev().find_map(handoff_of) {
        apply_handoff(&mut started, h);
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
fn claude_conversation(session_id: &str, spec: &CreateSession, events: &[SessionEvent]) -> crate::claude_code::ClaudeConversation {
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
/// Missing local chain metadata is safe: rewinds then use kept-history seeding.
async fn journal_claude_conversation(
    journal: &Journal,
    session_id: &str,
    spec: &CreateSession,
    events: &[SessionEvent],
) -> anyhow::Result<crate::claude_code::ClaudeConversation> {
    let mut conversation = claude_conversation(session_id, spec, events);
    let handoff_seq = events.iter().rev().find(|e| handoff_of(e).is_some()).map_or(0, |e| e.seq);
    let mut since = handoff_seq;
    let rewound_seq = events.iter().rev().find(|e| matches!(e.kind, Some(session_event::Kind::Rewound(_)))).map_or(0, |e| e.seq);
    let mut reset_found = false;
    if let Some((reset, applied)) = journal.claude_rewind(session_id).await? {
        anyhow::ensure!(applied != 0, "Claude Code rewind is not confirmed; retry Rewind before sending more input");
        if applied > handoff_seq && applied >= rewound_seq {
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
    if rewound_seq > handoff_seq && !reset_found {
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
    anyhow::ensure!(!registry.iter().any(|t| t.running), "Subagent {id} has running background tasks");
    let last_work = events.iter().rev().find_map(|event| match &event.kind {
        Some(kind @ (session_event::Kind::User(_) | session_event::Kind::TurnStarted(_) | session_event::Kind::TurnEnded(_))) => Some(kind),
        _ => None,
    });
    anyhow::ensure!(
        matches!(last_work, Some(session_event::Kind::TurnEnded(end))
            if matches!(TurnEndReason::try_from(end.reason),
                Ok(TurnEndReason::Completed | TurnEndReason::Interrupted | TurnEndReason::Error))),
        "Subagent {id} has no confirmed terminal turn boundary"
    );
    anyhow::ensure!(notices_of(events).2.is_empty(), "Subagent {id} has pending notices");
    let mut tasks = HashMap::new();
    for event in events {
        if let Some(session_event::Kind::BackgroundTask(task)) = &event.kind {
            tasks.insert(&task.task_id, task);
        }
    }
    for task in tasks.values() {
        anyhow::ensure!(
            matches!(task.state.as_str(), "finished" | "failed" | "lost" | "stopped" | "stopped_by_user"),
            "Subagent {id} has an unclosed background task {}",
            task.task_id
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
                "Subagent {id} lost background task {} without confirming its process stopped",
                task.task_id
            );
        } else if task.state == "lost" {
            anyhow::ensure!(
                events.iter().any(|event| matches!(&event.kind,
                    Some(session_event::Kind::SubagentSpawned(child)) if child.background_task_id == task.task_id)),
                "Subagent {id} lost background task {} without a known child",
                task.task_id
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
    let mut noticed = HashSet::new();
    let mut taken = HashSet::new();
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::NoticeQueued(n)) => {
                noticed.insert(n.command_id.clone());
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
                if !inputs.is_empty() && inputs.iter().all(|i| noticed.contains(*i)) {
                    auto_turns.push(e.ts_ms);
                }
                inputs.clear();
            }
            _ => {}
        }
    }
    let pending = events
        .iter()
        .filter_map(|e| match &e.kind {
            Some(session_event::Kind::NoticeQueued(n)) if !taken.contains(&n.command_id) => Some((
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
            )),
            _ => None,
        })
        .collect();
    (auto_turns, noticed.into_iter().collect(), pending)
}

/// Sets a spec's default for its subagents (`None`: the session's own
/// model).
fn set_subagent_default(spec: &mut CreateSession, model: Option<ModelSpec>) {
    spec.subagent_target = model;
    spec.subagent_model.clear();
}

/// A local turn's report belongs to its consumed inputs, not to an
/// in-memory background task or a queued input for a later turn. The
/// receipt is appended only after its waiter or parent's journal took it.
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
    let mut pending = Vec::new();
    let mut start = boundary + 1;
    for (end, event) in events.iter().enumerate().skip(start) {
        if !matches!(event.kind, Some(session_event::Kind::TurnEnded(_))) {
            continue;
        }
        let id = format!("subagent:local:{sid}:{}", event.seq);
        let inputs: Vec<_> = events[start..end]
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::User(u)) if u.blocked.is_empty() && u.command != "compact" => Some(u),
                _ => None,
            })
            .collect();
        if !received.contains(id.as_str()) && !inputs.is_empty() {
            pending.push((id, end, inputs));
        }
        start = end + 1;
    }
    pending
}

/// A successful channel send is not durable delivery. On recovery, look
/// for the call's real result or its background notice before retiring a
/// report. A placeholder saying the child is still running/lost does not
/// acknowledge a later completed report.
fn report_recorded(events: &[SessionEvent], call: &str, child: &str, status: &str) -> bool {
    if call.is_empty() {
        return false;
    }
    // Some providers (and the scripted model) reuse ids across turns.
    // A previous invocation's result must not acknowledge the latest one.
    let start = events
        .iter()
        .rposition(|e| match &e.kind {
            Some(session_event::Kind::Assistant(a)) => a.blocks.iter().any(|b| {
                matches!(&b.block,
            Some(c6_proto::content_block::Block::ToolUse(u)) if u.id == call)
            }),
            _ => false,
        })
        .unwrap_or(0);
    let Some(result) = events[start..].iter().rev().find_map(|e| match &e.kind {
        Some(session_event::Kind::ToolResult(r)) if r.call_id == call => Some(r),
        _ => None,
    }) else {
        return false;
    };
    if result.output.starts_with("Started the subagent in the background as ")
        || result.output.starts_with("Continuing the subagent in the background as ")
    {
        let Some(task) = result.output.split(" as ").nth(1).and_then(|s| s.split('.').next()) else {
            return false;
        };
        return events.iter().any(|e| {
            matches!(&e.kind,
            Some(session_event::Kind::NoticeQueued(n)) if n.command_id.starts_with(&format!("task:{task}:"))
                && (status == "lost" || !n.command_id.ends_with(":lost")))
        });
    }
    result.output.contains(&format!("[subagent session {child}]"))
        || (result.output.contains(&format!("<subagent id=\"{child}\" status=\"{status}\"")))
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
            self.queue.push_front((input, None));
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

    /// The model switch the next input asks for, which applies before it.
    fn take_switch(&mut self) -> Option<ModelSpec> {
        self.queue.front_mut().and_then(|(_, m)| m.take())
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
        if self.queue.front().is_some_and(|(i, _)| i.is_compact()) {
            return self.queue.pop_front().map(|(i, _)| i).into_iter().collect();
        }
        let n = self.queue.iter().position(|(i, m)| m.is_some() || i.is_compact()).unwrap_or(self.queue.len());
        self.queue.drain(..n).map(|(i, _)| i).collect()
    }
}

struct Actor {
    spec: CreateSession,
    started: SessionStarted,
    session_tools: Arc<SessionTools>,
    hooks: Option<Arc<crate::hooks::HookRunner>>,
    policies: Option<Arc<crate::policy::Policies>>,
    /// `None` while a turn task holds it.
    conv: Option<Conversation>,
    turns: u64,
    writer: Arc<Writer>,
    link: Arc<HeadLink>,
    journal: Arc<Journal>,
    tools: Vec<Arc<dyn Tool>>,
    inbox: Arc<Mutex<Inbox>>,
    checkpoints: Arc<Checkpoints>,
    /// When background-task notices started turns (for the hourly limit).
    auto_turns: VecDeque<tokio::time::Instant>,
    /// Notices held for your next message, and whether each would start a
    /// turn (those held by the hourly limit go once the hour allows).
    deferred: Vec<(PendingInput, bool)>,
    /// Every notice recorded in the log (`NoticeQueued`), by command id.
    noticed: HashSet<String>,
    /// The session secrets and head calls are asked for on behalf of: its
    /// own, or a subagent's top-level session.
    on_behalf_of: String,
    /// Delegations waiting for their turn to end: the input's command id.
    waiting: Vec<(String, tokio::sync::oneshot::Sender<Outcome>)>,
    /// Waiters already given a report in this process. Their source log
    /// stays pending until the parent records the result durably.
    handed_reports: HashSet<String>,
    remote_receipts: Vec<c6_proto::SubagentReported>,
    /// To start subagents.
    manager: SessionManager,
    /// The default for its subagents (as in `spec`), shared with a running
    /// turn's subagent runner.
    subagent_default: crate::subagent::SharedDefault,
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

struct CancelShellContexts(Arc<SessionTools>);

impl Drop for CancelShellContexts {
    fn drop(&mut self) {
        // An unrelated actor failure can also strand an in-flight tool call.
        // Reload creates fresh tools; the old callbacks must stay canceled.
        self.0.background.cancel_shell_contexts();
    }
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ActorMsg>) {
        // Locals drop before arguments: seal old callbacks before rx closes
        // and another caller can reload the actor, including on task abort.
        let _shell_contexts = CancelShellContexts(self.session_tools.clone());
        match self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await {
            Ok(events) => self.withdrawn = withdrawn_sources(&events),
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
        let mut running: Option<(TurnTask, CancellationToken)> = None;
        // Rewinds wait for the running turn to stop. Do not let a retry
        // replace the original receipt while cancellation is still pending.
        let mut rewind_blocked = matches!(&reset, Some((r, 0)) if !r.failure.is_empty());
        let mut rewinds = match reset {
            Some((r, 0)) if r.failure.is_empty() || !r.retries.is_empty() => {
                let mut pending = VecDeque::from([(r.command_id, r.to_seq, r.restore_files)]);
                pending.extend(r.retries.into_iter().map(|id| (id, r.to_seq, r.restore_files)));
                pending
            }
            _ => VecDeque::new(),
        };
        loop {
            while running.is_none()
                && let Some((command_id, to_seq, restore)) = rewinds.pop_front()
            {
                // Recovery and head redelivery may queue the same receipt.
                // Never replay a confirmed rewind over subsequent input.
                match self.journal.is_applied(&command_id).await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        error!("checking rewind receipt: {e:#}");
                        return;
                    }
                }
                if let Err(e) = self.rewind(command_id.clone(), to_seq, restore).await {
                    error!(session = %self.writer.session_id, "rewinding: {e:#}");
                    if !self.runs_claude_code() {
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
                        return;
                    }
                    let retries: Vec<_> = rewinds.iter().map(|(id, _, _)| id.clone()).filter(|id| id != &command_id).collect();
                    match self.rewind_failed(&command_id, &e, &retries).await {
                        Ok(blocked) => rewind_blocked = blocked,
                        Err(e) => {
                            error!("recording Rewind failure: {e:#}");
                            return;
                        }
                    }
                    rewinds.clear();
                    break;
                }
                rewind_blocked = false;
            }
            if !rewind_blocked
                && running.is_none()
                && let Err(e) = self.take_up_held().await
            {
                // Do not overwrite an unidentifiable held runtime or drop
                // its actor/inbox. Explicit rewind/stop can settle ownership.
                rewind_blocked = true;
                if let Err(e) = self
                    .writer
                    .append(session_event::Kind::Error(ErrorEvent {
                        message: format!("Claude Code could not be safely taken up; queued messages were not sent: {e:#}"),
                    }))
                    .await
                {
                    error!("recording Claude Code adoption failure: {e:#}");
                    return;
                }
            }
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
            if !rewind_blocked && running.is_none() && !handoff_waits && !self.inbox.lock().unwrap().queue.is_empty() {
                let switch = self.inbox.lock().unwrap().take_switch();
                if switch.is_some() {
                    self.handoff_announced = false;
                }
                if let Some(model) = switch
                    && let Err(e) = self.switch_model(model).await
                {
                    error!(session = %self.writer.session_id, "recording a model switch: {e:#}");
                    return;
                }
                running = Some(self.start_turn());
                self.running_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let idle_until = self.claude.as_ref().filter(|_| running.is_none()).and_then(|p| p.idle_until(self.claude_idle()));
            // Claude Code's tasks that ended while it waited, if it doesn't
            // start a turn for them by itself.
            let wake_at = self.claude.as_ref().filter(|_| !rewind_blocked && running.is_none()).and_then(|p| p.wake_due());
            // Notices the hourly limit held: once the hour allows.
            let waiting_to_wake = !rewind_blocked && running.is_none() && self.deferred.iter().any(|(_, wake)| *wake);
            let retry_at = (waiting_to_wake && self.link.is_connected()).then(|| self.next_auto_turn());
            tokio::select! {
                msg = rx.recv() => match msg {
                    None => return,
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
                        if let Err(e) = self.notice(input, running.is_some(), true).await {
                            error!(session = %self.writer.session_id, "queueing a session message: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Input(mut input, model, choice)) => {
                        // Your message: what the agent does from now on is not
                        // a session message's chain.
                        self.session_tools.set_message_hops(0);
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
                        let held = std::mem::take(&mut self.deferred);
                        let command_id = input.command_id.clone();
                        // Send now in carbon6's own loop: the turn stops (it takes
                        // no input once stopping), and this starts the next. A
                        // Claude Code turn delivers it at once when it writes it
                        // (`Process::turn`).
                        let stops = input.send_now && !self.runs_claude_code();
                        if stops && let Some((_, cancel)) = &running {
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
                    Some(ActorMsg::Interrupt) => {
                        if let Some((_, cancel)) = &running {
                            cancel.cancel();
                            self.stopping |= self.runs_claude_code();
                        }
                    }
                    Some(ActorMsg::SendNow(command_id)) => {
                        if running.is_some() && self.runs_claude_code() {
                            // The running turn asks its process to deliver it now.
                            self.inbox.lock().unwrap().send_now(command_id);
                        } else if let Some((_, cancel)) = &running {
                            // carbon6's own loop: the turn stops, and the agent
                            // takes it as its next turn.
                            cancel.cancel();
                        }
                    }
                    Some(ActorMsg::Delegate(input, done)) => {
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
                        match self.remote_report(command_id, *ended, running.is_some()).await {
                            Ok(()) => { let _ = recorded.send(Ok(())); }
                            Err(e) => {
                                error!(session = %self.writer.session_id, "taking a remote subagent's report: {e:#}");
                                let _ = recorded.send(Err(format!("could not record subagent report: {e:#}")));
                                return;
                            }
                        }
                    }
                    Some(ActorMsg::ChildReport { command_id, text, wake, task_notice, recorded }) => {
                        let input = PendingInput {
                            command_id,
                            text,
                            command: "task-notification".into(),
                            task_notice,
                            ..Default::default()
                        };
                        if let Err(e) = self.notice(input, running.is_some(), wake).await {
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
                    Some(ActorMsg::ReleaseNotices) => {
                        let held = std::mem::take(&mut self.deferred);
                        info!(session = %self.writer.session_id, notices = held.len(), "delivering held notices: you asked");
                        let mut inbox = self.inbox.lock().unwrap();
                        for (n, _) in held {
                            inbox.push(n, None);
                        }
                    }
                    Some(ActorMsg::Reloaded { auto_turns_ms, noticed, pending }) => {
                        let (now, now_ms) = (tokio::time::Instant::now(), c6_core::now_ms());
                        self.auto_turns = auto_turns_ms
                            .into_iter()
                            .filter_map(|t| now.checked_sub(std::time::Duration::from_millis(now_ms.saturating_sub(t).max(0) as u64)))
                            .collect();
                        self.noticed.extend(noticed);
                        for (input, wake) in pending {
                            info!(session = %self.writer.session_id, notice = %input.command_id, "delivering a notice again after a reload");
                            if let Err(e) = self.deliver_notice(input, running.is_some(), wake, false).await {
                                error!(session = %self.writer.session_id, "delivering a notice again: {e:#}");
                                return;
                            }
                        }
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
                            .then(|| task_notice(&record, &ev.output));
                        // Lost because you stopped the turn (Claude Code didn't
                        // stop, so its process was): the agent hears it with
                        // your next message, and isn't woken for it.
                        let held = lost_native && self.after_stop(running.is_some());
                        if let Err(e) = self.task_event(record, notice, &ev.output, running.is_some() && !held, !held).await {
                            error!(session = %self.writer.session_id, "recording a background task: {e:#}");
                            return;
                        }
                        if let Err(e) = self.record_remote_receipts().await {
                            error!(session = %self.writer.session_id, "recording subagent report receipts: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Resume(turn)) => {
                        let check = async {
                            let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
                            let pending = matches!(self.journal.claude_rewind(&self.writer.session_id).await?, Some((_, 0)));
                            anyhow::Ok(resume_superseded(&events, turn, pending))
                        }.await;
                        match check {
                            Ok(true) => continue,
                            Ok(false) => {},
                            Err(e) => { error!("checking restart continuation: {e:#}"); return; }
                        }
                        // A subagent's parent decides (its call got what it did);
                        // a held Claude Code process carries on by itself.
                        if self.spec.delegation.is_some() || (self.runs_claude_code() && self.claude.is_some()) {
                            continue;
                        }
                        let text = spec_prompt(&self.spec, "session.resume_after_restart", c6_agent::RESUME_AFTER_RESTART);
                        let input = PendingInput {
                            command_id: format!("resume-after-restart-{turn}"),
                            text,
                            command: "task-notification".into(),
                            ..Default::default()
                        };
                        if let Err(e) = self.notice(input, running.is_some(), true).await {
                            error!(session = %self.writer.session_id, "resuming after a restart: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Lost { task_id, kind, description, native_type, notify, pgid, child, remote }) => {
                        // A held shell command kept running: take it up again,
                        // and its end reaches the agent as usual.
                        if kind == "shell" && self.session_tools.background.adopt(&task_id) {
                            info!(session = %self.writer.session_id, task = %task_id, "took up a background command after the restart");
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
                            let note = spec_prompt(&self.spec, "session.subagent_remote_reattach", c6_agent::SUBAGENT_REMOTE_REATTACH);
                            format!("<subagent id=\"{child}\" status=\"running\"/>\n{note}")
                        } else {
                            crate::subagent::partial(&self.spec, &crate::subagent::digest(&self.journal, &child, "lost", "").await)
                        };
                        // Claude Code's own tasks died with its process: its agent
                        // hears it from us.
                        let tell = notify || kind == "claude_code";
                        let record = BackgroundTask { task_id, kind, description, native_type, state: "lost".into(), notify, ..Default::default() };
                        let notice = tell.then(|| task_notice(&record, &report));
                        if let Err(e) = self.task_event(record, notice, "", running.is_some(), true).await {
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
                        match self.journal.is_applied(&command_id).await {
                            Ok(true) => continue,
                            Ok(false) => {},
                            Err(e) => { error!("checking rewind receipt: {e:#}"); return; }
                        }
                        if self.runs_claude_code() {
                            // Dispatch preflight can race a newly started child.
                            // Recheck before canceling the running turn as well.
                            let check = async {
                                let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
                                self.manager.rewind_preflight(&self.writer.session_id, &events, to_seq, restore_files).await
                            }.await;
                            if let Err(e) = check {
                                let pending = self.journal.claude_rewind(&self.writer.session_id).await;
                                let id = match &pending { Ok(Some((r, 0))) => r.command_id.as_str(), _ => &command_id };
                                let mut retries: Vec<_> = rewinds.iter().map(|(id, _, _)| id.clone()).filter(|queued| queued != id).collect();
                                if id != command_id && !retries.contains(&command_id) { retries.push(command_id.clone()); }
                                match self.rewind_failed(id, &e, &retries).await {
                                    Ok(blocked) => rewind_blocked = blocked,
                                    Err(e) => { error!("recording Rewind refusal: {e:#}"); return; }
                                }
                                rewinds.clear();
                                continue;
                            }
                            if let Ok(Some((r, 0))) = self.journal.claude_rewind(&self.writer.session_id).await {
                                // A NEW command is an explicit retry. Implicit
                                // redelivery of a failed receipt never retries.
                                if !r.failure.is_empty() {
                                    if r.command_id == command_id { continue; }
                                    rewinds.push_back((r.command_id, r.to_seq, r.restore_files));
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
                                    self.journal.prepare_claude_rewind(&self.writer.session_id, &reset).await
                                }.await;
                                if let Err(e) = transition { error!("starting Rewind interruption: {e:#}"); return; }
                            }
                        }
                        if let Some((_, cancel)) = &running { cancel.cancel(); }
                        rewinds.push_back((command_id, to_seq, restore_files));
                    }
                    Some(ActorMsg::RewindIdle(reply)) => {
                        let idle = running.is_none()
                            && self.inbox.lock().unwrap().queue.is_empty()
                            && self.deferred.is_empty()
                            && !self.session_tools.background.list().iter().any(|t| t.running)
                            && rx.is_empty();
                        let _ = reply.send(idle);
                    }
                },
                // Claude Code wrote while no turn ran: a background task's
                // progress, or a turn it starts by itself.
                line = async { self.claude.as_mut().expect("checked").next_line().await }, if !rewind_blocked && running.is_none() && self.claude.is_some() => {
                    match line {
                        None => {
                            let unsent = self.claude.take().expect("checked").exited().await;
                            self.inbox.lock().unwrap().requeue(unsent);
                        }
                        Some(v) => {
                            let sink: Arc<dyn EventSink> = self.writer.clone();
                            match self.claude.as_mut().expect("checked").idle_line(v, &sink).await {
                                Ok(true) => {
                                    running = Some(self.start_turn());
                                    self.running_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                }
                                Ok(false) => {}
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
                    self.release_held();
                }
                _ = self.link.wait_connected(), if waiting_to_wake && retry_at.is_none() => {}
                // Idle long enough, with no background tasks: close it.
                _ = tokio::time::sleep_until(idle_until.unwrap_or_else(tokio::time::Instant::now)), if running.is_none() && idle_until.is_some() => {
                    info!(session = %self.writer.session_id, "closing an idle Claude Code process");
                    self.claude.take().expect("checked").close().await;
                }
                done = async { (&mut running.as_mut().unwrap().0).await }, if running.is_some() => {
                    running = None;
                    self.running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                    self.writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: false, ..Default::default() }));
                    match done {
                        Ok(Some((conv, claude))) => {
                            self.conv = Some(conv);
                            self.claude = claude;
                            if std::mem::take(&mut self.stopping) {
                                self.stopped_until = Some(std::time::Instant::now() + crate::claude_code::STOP_QUIET);
                            }
                            // A turn Claude Code started right after your stop,
                            // stopped before it showed anything: not a turn.
                            if self.claude.as_mut().is_some_and(|p| p.take_unshown()) {
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
                        // The log and the in-memory state may disagree, and
                        // queued inputs may be lost: stop, and let the next
                        // command (or the head's redelivery) rebuild the
                        // session from the log.
                        Ok(None) => return,
                        Err(e) => {
                            error!(session = %self.writer.session_id, "turn task panicked: {e}");
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
        let sid = self.writer.session_id.clone();
        let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        for (command_id, end, inputs) in pending_local_reports(&sid, &events) {
            if self.handed_reports.contains(&command_id) {
                continue;
            }
            let (settled, still): (Vec<_>, Vec<_>) =
                std::mem::take(&mut self.waiting).into_iter().partition(|(id, _)| inputs.iter().any(|u| &u.command_id == id));
            self.waiting = still;
            let outcome = last_turn_outcome(&events[..=end]);
            let status = crate::subagent::status_of(outcome.reason, &outcome.report);
            let parent = self.journal.events_after(&d.parent_session_id, 0, u32::MAX).await?;
            // Sending, rather than testing is_closed, decides ownership. A
            // waiter can disappear between those operations.
            let told = inputs.iter().any(|u| report_recorded(&parent, &u.delegated_by, &sid, status));
            let mut handed = false;
            for (_, tx) in settled {
                handed |= tx.send(Outcome { reason: outcome.reason, report: outcome.report.clone(), error: outcome.error.clone() }).is_ok();
            }
            if handed {
                self.handed_reports.insert(command_id);
                continue;
            }
            if !told {
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
                let body = crate::subagent::report_body(&self.spec, &sid, outcome.reason, &outcome.report, &digest);
                let text = crate::subagent::unasked_report(&sid, &said, &body);
                let delivered = async {
                    let parent = self
                        .manager
                        .actor_for(&d.parent_session_id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("parent {} is not on this worker", d.parent_session_id))?;
                    let task_notice = Some(crate::subagent::task_report_notice(&self.started.title, &sid, outcome.reason, &outcome.report));
                    let (recorded, ack) = tokio::sync::oneshot::channel();
                    parent
                        .send(ActorMsg::ChildReport { command_id: command_id.clone(), text, wake, task_notice, recorded })
                        .map_err(|_| anyhow::anyhow!("parent stopped before taking the report"))?;
                    ack.await.map_err(|_| anyhow::anyhow!("parent stopped before recording the report"))
                }
                .await;
                if let Err(e) = delivered {
                    self.writer
                        .append(session_event::Kind::Notice(Notice {
                            title: "Subagent report delivery failed".into(),
                            message: format!(
                                "The report could not reach parent {}: {e:#}. It remains pending in this session's log.",
                                d.parent_session_id
                            ),
                            priority: "high".into(),
                            ..Default::default()
                        }))
                        .await?;
                    return Err(e);
                }
            }
            self.writer
                .append(session_event::Kind::SubagentReported(c6_proto::SubagentReported {
                    command_id,
                    child_session_id: sid.clone(),
                    status: crate::subagent::status_of(outcome.reason, &outcome.report).into(),
                    ..Default::default()
                }))
                .await?;
        }
        Ok(())
    }

    async fn record_remote_receipts(&mut self) -> anyhow::Result<()> {
        if self.remote_receipts.is_empty() {
            return Ok(());
        }
        let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        let pending = std::mem::take(&mut self.remote_receipts);
        for receipt in pending {
            if report_recorded(&events, &receipt.call_id, &receipt.child_session_id, &receipt.status) {
                self.writer.append(session_event::Kind::SubagentReported(receipt)).await?;
            } else {
                self.remote_receipts.push(receipt);
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
        let reason = TurnEndReason::try_from(ended.reason).unwrap_or(TurnEndReason::Unspecified);
        let reported = c6_proto::SubagentReported {
            command_id,
            call_id: ended.call_id.clone(),
            child_session_id: ended.child_session_id.clone(),
            status: crate::subagent::status_of(reason, &ended.report).into(),
        };
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

    /// The most turns notices may start per hour.
    fn auto_turn_limit(&self) -> usize {
        match self.spec.auto_turns_per_hour {
            0 => AUTO_TURNS_PER_HOUR,
            n => n as usize,
        }
    }

    /// How long its idle Claude Code process stays up with no tasks.
    fn claude_idle(&self) -> std::time::Duration {
        match self.spec.claude_code_idle_minutes {
            0 => crate::claude_code::IDLE,
            m => std::time::Duration::from_secs(u64::from(m) * 60),
        }
    }

    /// Takes a turn for notices if the hour allows one.
    fn take_auto_turn(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        while self.auto_turns.front().is_some_and(|t| now.duration_since(*t) >= HOUR) {
            self.auto_turns.pop_front();
        }
        if self.auto_turns.len() >= self.auto_turn_limit() {
            return false;
        }
        self.auto_turns.push_back(now);
        true
    }

    /// The first slot with fewer than N turns in the preceding hour. A
    /// manual Deliver now can leave more than N counted turns after load.
    fn next_auto_turn(&self) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        self.auto_turns.len().checked_sub(self.auto_turn_limit()).map_or(now, |i| (self.auto_turns[i] + HOUR).max(now))
    }

    /// The hour allows another automatic turn: the held notices start one
    /// together.
    fn release_held(&mut self) {
        if !self.link.is_connected() || !self.deferred.iter().any(|(_, wake)| *wake) || !self.take_auto_turn() {
            return;
        }
        for (n, _) in std::mem::take(&mut self.deferred) {
            self.inbox.lock().unwrap().push(n, None);
        }
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
        let queued = c6_proto::NoticeQueued {
            command_id: input.command_id.clone(),
            text: input.text.clone(),
            command: input.command.clone(),
            wake,
            prompt: input.prompt.clone(),
            from_session_id: input.from_session_id.clone(),
            from_title: input.from_title.clone(),
            hops: input.hops,
            task_notice: input.task_notice.clone(),
            deliver_at_ms: if wake && !busy && self.inbox.lock().unwrap().queue.is_empty() {
                let delay = self.next_auto_turn().saturating_duration_since(tokio::time::Instant::now());
                if delay.is_zero() { 0 } else { c6_core::now_ms() + delay.as_millis() as i64 }
            } else {
                0
            },
        };
        self.writer.append(session_event::Kind::NoticeQueued(queued)).await?;
        self.deliver_notice(input, busy, wake, true).await
    }

    /// Delivers a notice: it joins a running turn at its next step; in an
    /// idle session it starts a turn if `wake` (within the hourly limit;
    /// beyond it, once the hour allows), else it waits for the next input.
    /// `announce`: say so in the log when the limit holds it.
    async fn deliver_notice(&mut self, input: PendingInput, busy: bool, wake: bool, announce: bool) -> anyhow::Result<()> {
        if self.withdrawn_input(&input) {
            return Ok(());
        }
        // What the agent does next follows from this message's chain.
        if input.hops > 0 {
            self.session_tools.set_message_hops(input.hops);
        }
        if busy || !self.inbox.lock().unwrap().queue.is_empty() {
            let mut inbox = self.inbox.lock().unwrap();
            for (held, _) in std::mem::take(&mut self.deferred) {
                inbox.push(held, None);
            }
            inbox.push(input, None);
            return Ok(());
        }
        if !wake {
            self.deferred.push((input, false));
            return Ok(());
        }
        if !self.link.is_connected() {
            self.deferred.push((input, true));
            return Ok(());
        }
        if self.take_auto_turn() {
            let mut inbox = self.inbox.lock().unwrap();
            for (held, _) in std::mem::take(&mut self.deferred) {
                inbox.push(held, None);
            }
            inbox.push(input, None);
            return Ok(());
        }
        warn!(session = %self.writer.session_id, notice = %input.command_id, "notice held: too many automatic turns this hour");
        let limit = self.auto_turn_limit();
        let what = match input.command_id.strip_prefix("task:").and_then(|t| t.split(':').next()) {
            Some(task) => format!("Background task {task} ended."),
            None => "A notice for the agent is waiting.".to_owned(),
        };
        self.deferred.push((input, true));
        if !announce {
            return Ok(());
        }
        self.writer
            .append(session_event::Kind::Error(ErrorEvent {
                message: format!(
                    "{what} Notices already started {limit} turns in the last hour, so the agent hears about it once the hour allows, or with your next message."
                ),
            }))
            .await?;
        Ok(())
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
        let withdrawn = self.withdrawn.contains(&record.task_id);
        let command_id = format!("task:{}:{}", record.task_id, record.state);
        let task_notice = (!withdrawn).then(|| background_task_notice(&record, output));
        self.writer.append(session_event::Kind::BackgroundTask(record)).await?;
        if withdrawn {
            return Ok(());
        }
        let Some(text) = notice else { return Ok(()) };
        let input = PendingInput { command_id, text, command: "task-notification".into(), task_notice, ..Default::default() };
        self.notice(input, busy, wake).await
    }

    /// You stopped its Claude Code turn just now, or are stopping the one
    /// `running`: what Claude Code loses meanwhile is the stop's doing.
    fn after_stop(&self, running: bool) -> bool {
        (running && self.stopping) || self.stopped_until.is_some_and(|t| std::time::Instant::now() < t)
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
        if restore_files {
            let effective = c6_agent::effective_events(&events);
            let commit = effective.iter().filter(|e| e.seq < to_seq).rev().find_map(|e| match &e.kind {
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
        let (blocked, rejected) = self.journal.fail_claude_rewind(&self.writer.session_id, command_id, &event, retries).await?;
        *next += 1;
        self.writer.logged.store(*next, std::sync::atomic::Ordering::SeqCst);
        for id in rejected {
            self.link.reject_command(&self.writer.session_id, &id, &message);
        }
        Ok(blocked)
    }

    async fn reset_claude(&mut self, events: &[SessionEvent], command_id: &str, to_seq: u64, restore_files: bool) -> anyhow::Result<()> {
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
        self.journal.prepare_claude_rewind(sid, &reset).await?;
        self.manager.rewind_preflight(sid, events, to_seq, restore_files).await?;
        // This write is the irreversible boundary, including on crash. Never
        // clear a legacy intent (whose phase is unknown) as if untouched.
        reset.untouched = false;
        reset.previous = None;
        reset.failure.clear();
        self.journal.prepare_claude_rewind(sid, &reset).await?;
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
        self.journal.prepare_claude_rewind(sid, &reset).await?;
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
        self.journal.prepare_claude_rewind(sid, &reset).await?;
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
            spec: self.spec.clone(),
            started: self.started.clone(),
        });
        let mut attempts = Vec::new();
        if let Some(cutoff) = cutoff {
            let mut fork = launch.clone();
            fork.conversation = cutoff.session_id;
            // The cutoff is inclusive. Choose a fresh fork ID, then adopt the
            // session_id from stream-json on every subsequent turn.
            fork.extra_args = vec![
                "--resume-session-at".into(),
                cutoff.cutoff_uuid,
                "--fork-session".into(),
                "--session-id".into(),
                reset.session_id.clone(),
            ];
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
            self.journal.prepare_claude_rewind(sid, &reset).await?;
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
            match p.ready(&reset.session_id).await {
                Ok(confirmed_id) => {
                    reset.session_id = confirmed_id;
                    self.journal.prepare_claude_rewind(sid, &reset).await?;
                    p.record_in(&self.manager.claude_dir());
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
        if !self.runs_claude_code() || self.claude.is_some() {
            return Ok(());
        }
        let held = self.manager.claude_dir().join("held");
        let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        let conversation = journal_claude_conversation(&self.journal, &self.writer.session_id, &self.spec, &events).await?.id;
        let Some((record, token)) = crate::claude_code::Process::held_record(&held, &conversation)? else { return Ok(()) };
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
        let sid = self.writer.session_id.clone();
        // Leaving Claude Code: its process goes, with its background tasks.
        if let Some(p) = self.claude.take() {
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
                tools: built.tools.iter().map(|t| t.spec().name).collect(),
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
        for message in setup_errors {
            self.writer.append(session_event::Kind::Error(ErrorEvent { message })).await?;
        }
        info!(session = %sid, model = %model.model, runtime = %handoff.tool_profile, "handed over");
        self.spec.model = Some(model);
        apply_handoff(&mut self.started, &handoff);
        self.tools = tools;
        self.claude_code.store(into_claude_code, std::sync::atomic::Ordering::SeqCst);
        // Subagents keep their parent's hooks; a top-level session's are
        // carbon6's to run only in its own loop.
        if self.spec.delegation.is_none() {
            self.session_tools.set_hooks(if into_claude_code { None } else { own_loop_hooks(&self.policies, &self.hooks) });
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

    fn start_turn(&mut self) -> (TurnTask, CancellationToken) {
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
        };
        let mut conv = self.conv.take().expect("no turn is running");
        let sub = crate::subagent::Parent {
            session_id: self.writer.session_id.clone(),
            spec: self.spec.clone(),
            started: self.started.clone(),
            log: self.writer.clone(),
            manager: self.manager.clone(),
            default: self.subagent_default.clone(),
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
            spec: self.spec.clone(),
            started: self.started.clone(),
        });
        let (mut claude, claude_stdin, claude_dir) = (self.claude.take(), self.claude_stdin.clone(), self.manager.claude_dir());
        // Claude Code's calls of carbon6's tools are held to the session's policies.
        let policies = self.policies.clone();
        let task = tokio::spawn(async move {
            let sink: Arc<dyn EventSink> = writer.clone();
            if !claude_code {
                cfg.context_window = link.context_window(&on_behalf_of, &model).await;
            }
            // Secrets for this turn's commands (values only in memory).
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
                    checkpoints.snapshot().await
                } {
                    Ok(commit) => Checkpoint { commit, error: String::new() },
                    Err(e) => {
                        warn!(session = %writer.session_id, "no checkpoint: {e:#}");
                        Checkpoint { commit: String::new(), error: format!("{e:#}") }
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
                let conversation = match async {
                    let events = log.events_after(&writer.session_id, 0, u32::MAX).await?;
                    journal_claude_conversation(&log, &writer.session_id, spec_for_claude.as_ref().expect("a Claude Code turn"), &events)
                        .await
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
                    extra_args: Vec::new(),
                    native: claude_native,
                };
                let process = claude.take();
                let result = claude_turn(
                    &writer.session_id,
                    turn,
                    process,
                    &conversation,
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
                    // Only journal writes fail here.
                    error!(session = %writer.session_id, "turn failed: {e:#}");
                    None
                }
            }
        });
        (task, cancel)
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
    // Started with other settings (carbon6's tools, guidance, hooks, effort,
    // worker): closed and started again, resuming its conversation, so a
    // running session takes them up. Only a process with nothing under way:
    // no tasks (a restart would end them; the next quiet turn does it), no
    // inputs it has not taken, no turn it has begun.
    let mut restarted = false;
    let mut promoted = false;
    if let Some(p) = process.take_if(|p| (p.launch() != &launch || !p.is_held()) && p.idle_until(std::time::Duration::ZERO).is_some()) {
        // A reset initializes non-held until its branch is confirmed. Promote
        // that quiet process to restart-surviving transport on its next turn;
        // this alone is not a settings/worker change to announce.
        restarted = p.launch() != &launch;
        promoted = !p.is_held();
        p.close().await;
    }
    let inputs = inbox.lock().unwrap().drain();
    if process.is_none() && inputs.is_empty() {
        return Ok(None);
    }
    let more = inbox.lock().unwrap().more.clone();
    let feed = || inbox.lock().unwrap().joining();
    let failed = async |message: String| -> anyhow::Result<Option<Process>> {
        for input in &inputs {
            sink.append(input.event()).await?;
        }
        sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })).await?;
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
    let mut end = p.turn(turn, inputs.clone(), false, preamble, model, sink, &feed, &more, cancel).await?;
    if end.no_conversation
        && let Some(bin) = &bin
    {
        drop(p);
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
            Some(observer),
        ) {
            Ok(p) => p,
            Err(e) => {
                sink.append(session_event::Kind::Error(ErrorEvent { message: format!("failed to start Claude Code: {e}") })).await?;
                sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
                return Ok(None);
            }
        };
        p.set_turn_cancel(cancel);
        end = p.turn(turn, inputs, true, conversation.preamble.as_deref(), model, sink, &feed, &more, cancel).await?;
        if end.no_conversation {
            sink.append(session_event::Kind::Error(ErrorEvent {
                message: "Claude Code could not find or create this conversation.".into(),
            }))
            .await?;
            sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
        }
    }
    if end.alive {
        return Ok(Some(p));
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
    let checked = link.session_credential(session_id, connection_id).await;
    let client: Arc<dyn c6_llm::ModelClient> =
        match checked.and_then(|g| Ok(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers)?)) {
            Ok(_) => Arc::new(crate::link::LinkedClient::new(link.clone(), session_id, connection_id)),
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
    session_tools.set_subagents(Some(Arc::new(crate::subagent::Runner::new(sub, tools))));
    let result = run_turn(turn, cfg, client.as_ref(), tools, session_tools, sink, conv, &mut pending, cancel).await;
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
    if (!agents.is_empty() || !connections.is_empty() || !templates.is_empty())
        && let Some(i) = tools.iter().position(|t| c6_tools::Task::is_task(&t.spec().name))
    {
        tools[i] = Arc::new(
            c6_tools::Task::with_agents(profile == ToolProfile::Claude, agents)
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
    // At the depth limit (a remote subagent deep in its tree), no more.
    if !crate::subagent::may_delegate(spec, crate::subagent::depth_of(spec)) {
        tools.retain(|t| !c6_tools::Task::is_task(&t.spec().name) && !c6_tools::SendMessage::is_send(&t.spec().name));
    }
    if !tools.iter().any(|t| t.spec().name == SkillTool::tool_name(profile == ToolProfile::Claude)) {
        skills.clear();
    }
    BuiltTools { tools, skills, errors }
}

/// The single writer of one session's log.
pub(crate) struct Writer {
    pub session_id: String,
    pub journal: Arc<Journal>,
    pub link: Arc<HeadLink>,
    pub next_seq: tokio::sync::Mutex<u64>,
    /// `next_seq` as of the last append, read without waiting on one: what
    /// streamed text is stamped with.
    logged: std::sync::atomic::AtomicU64,
}

impl Writer {
    /// A writer whose next event is `next_seq`.
    pub fn new(session_id: String, journal: Arc<Journal>, link: Arc<HeadLink>, next_seq: u64) -> Self {
        Writer {
            session_id,
            journal,
            link,
            next_seq: tokio::sync::Mutex::new(next_seq),
            logged: std::sync::atomic::AtomicU64::new(next_seq),
        }
    }
}

#[async_trait]
impl EventSink for Writer {
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
        let mut next = self.next_seq.lock().await;
        // Streamed text first: it must not arrive after what it streamed.
        self.link.flush_ephemeral();
        let ev = SessionEvent { seq: *next, ts_ms: c6_core::now_ms(), kind: Some(kind) };
        let count = if matches!(&ev.kind, Some(session_event::Kind::Rewound(_))) {
            self.journal.append_rewind(&self.session_id, &ev).await?
        } else {
            self.journal.append(&self.session_id, &ev).await?;
            1
        };
        *next += count;
        self.logged.store(*next, std::sync::atomic::Ordering::SeqCst);
        Ok(ev.seq)
    }

    fn ephemeral(&self, mut kind: ephemeral_event::Kind) {
        // Where the log is: its next reply has this text, so a viewer that
        // got that reply first (catching up) knows to drop it.
        if let ephemeral_event::Kind::Text(d) | ephemeral_event::Kind::Thinking(d) = &mut kind {
            d.seq = self.logged.load(std::sync::atomic::Ordering::SeqCst);
        }
        self.link.ephemeral(EphemeralEvent { session_id: self.session_id.clone(), kind: Some(kind) });
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

fn task_notice(t: &BackgroundTask, output: &str) -> String {
    // One of Claude Code's own tasks, lost with its process (its id is the
    // CLI's; Claude Code itself can no longer say what became of it).
    if t.kind == "claude_code" {
        return format!(
            "<background-task kind=\"claude-code\" status=\"lost\">\nTask: {}\nIts Claude Code process stopped before it finished \
             (the worker restarted, or the CLI exited), and it stopped with it. Files it changed stay as they are; check them, and redo \
             the task if it is still needed.\n</background-task>",
            t.description
        );
    }
    let elapsed = duration_text(t.elapsed_ms / 1000);
    let agent = t.kind == "agent";
    let status = match t.state.as_str() {
        "finished" if agent && t.exit_code == 0 => "finished".to_owned(),
        "finished" if agent => "failed".to_owned(),
        "finished" => format!("exited with code {}", t.exit_code),
        "stopped_by_user" => "stopped by the user".to_owned(),
        "lost" if agent => "lost: the worker restarted while it was running".to_owned(),
        "lost" => "lost: the worker restarted while it was running; it has been stopped and its output is gone".to_owned(),
        other => other.to_owned(),
    };
    let mut s =
        format!("<background-task id=\"{}\" kind=\"{}\" status=\"{status}\"", t.task_id, if agent { "subagent" } else { "command" });
    if t.state != "lost" {
        s.push_str(&format!(" elapsed=\"{elapsed}\""));
    }
    s.push_str(">\n");
    s.push_str(&format!("{}: {}\n", if agent { "Task" } else { "Command" }, t.description));
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

/// What the model is told when its conversation is compacted.
pub(crate) fn compact_prompts(spec: &CreateSession) -> c6_agent::CompactPrompts {
    c6_agent::CompactPrompts {
        why_full: spec_prompt(spec, "session.compact.full", c6_agent::COMPACT_WHY),
        why_asked: spec_prompt(spec, "session.compact.asked", c6_agent::COMPACT_WHY_ASKED),
        instructions: spec_prompt(spec, "session.compact", c6_agent::COMPACT_PROMPT),
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
            let task = built.tools.iter().find(|t| c6_tools::Task::is_task(&t.spec().name));
            assert_eq!(task.is_some(), depth < 2);
            assert_eq!(built.tools.iter().any(|t| c6_tools::SendMessage::is_send(&t.spec().name)), depth < 2);
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
        assert!(!built.tools.iter().any(|t| c6_tools::Task::is_task(&t.spec().name) || c6_tools::SendMessage::is_send(&t.spec().name)));
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
        assert!(report_recorded(&events, "call", "child", "lost"));
        events.push(ev(
            3,
            3,
            session_event::Kind::NoticeQueued(c6_proto::NoticeQueued { command_id: "task:bg2:finished".into(), ..Default::default() }),
        ));
        assert!(report_recorded(&events, "call", "child", "completed"));
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

    /// A reload delivers the notices no turn took, and knows which turns
    /// notices started (they count toward the hourly limit).
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

    /// A log at the default limit, with no open task or turn: the case
    /// startup recovery used to leave without an actor (and thus a timer).
    async fn at_limit() -> (tempfile::TempDir, SessionManager, c6_testkit::MockLlm) {
        let dir = tempfile::tempdir().unwrap();
        let mock = c6_testkit::MockLlm::start().await;
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let model =
            ModelSpec { connection_id: "mock".into(), connection_kind: "openai_compat".into(), model: "m".into(), ..Default::default() };
        let spec = CreateSession { model: Some(model.clone()), cwd: dir.path().display().to_string(), ..Default::default() };
        let now = c6_core::now_ms();
        let mut events = vec![ev(
            1,
            now,
            session_event::Kind::Started(SessionStarted {
                model: Some(model),
                cwd: spec.cwd.clone(),
                tool_profile: "generic".into(),
                ..Default::default()
            }),
        )];
        for i in 0..AUTO_TURNS_PER_HOUR {
            for kind in [
                queued(&format!("n{i}"), true),
                user(&format!("n{i}")),
                turn(),
                session_event::Kind::TurnEnded(TurnEnded { turn: i as u64 + 1, reason: TurnEndReason::Completed as i32 }),
            ] {
                events.push(ev(events.len() as u64 + 1, now, kind));
            }
        }
        journal.import("held", &spec, &events).await.unwrap();
        let manager =
            SessionManager::new(journal, Arc::new(HeadLink::local(Arc::new(Head(mock.base_url())))), dir.path().join("worktrees"));
        (dir, manager, mock)
    }

    // External I/O (SQLite and the mock server) must progress without
    // Tokio automatically jumping to the hourly timer while we await it.
    fn pause_clock() -> tokio_util::task::AbortOnDropHandle<()> {
        tokio::time::pause();
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }

    async fn logged(manager: &SessionManager, predicate: impl Fn(&[SessionEvent]) -> bool) -> Vec<SessionEvent> {
        c6_testkit::within("the notice actor to record its work", async {
            loop {
                let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
                if predicate(&events) {
                    return events;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
    }

    async fn hold(manager: &SessionManager) -> mpsc::UnboundedSender<ActorMsg> {
        let tx = manager.actor("held").await.unwrap().unwrap();
        for text in ["report 21", "report 22"] {
            tx.send(ActorMsg::ChildReport {
                command_id: format!("subagent:{text}"),
                recorded: tokio::sync::oneshot::channel().0,
                text: text.into(),
                wake: true,
                task_notice: Some(crate::subagent::task_report_notice("Review", "child", TurnEndReason::Completed, text)),
            })
            .unwrap();
        }
        let events =
            logged(manager, |events| events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::Error(_)))).count() == 2).await;
        let held: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::NoticeQueued(n)) if n.text.starts_with("report ") => Some(n),
                _ => None,
            })
            .collect();
        assert_eq!(held.len(), 2);
        assert!(held.iter().all(|n| n.task_notice.as_ref().is_some_and(|m| m.title == "Review" && m.session_id == "child")));
        assert!(held.iter().all(|n| n.deliver_at_ms > c6_core::now_ms()), "the UI gets the next free slot");
        tx
    }

    async fn delivered(manager: &SessionManager, mock: &c6_testkit::MockLlm) {
        // Only the hourly wait is simulated. The mock streams chunks on
        // short timers, and the model's I/O must run on a live clock.
        tokio::time::resume();
        let events =
            logged(manager, |events| events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::TurnEnded(_)))).count() == 21)
                .await;
        assert_eq!(mock.requests().len(), 1, "one turn takes the whole held batch");
        let request = mock.requests()[0].to_string();
        assert!(request.contains("report 21") && request.contains("report 22"), "{request}");
        assert!(notices_of(&events).2.is_empty());
        let delivered: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                Some(session_event::Kind::User(u)) if u.text.starts_with("report ") => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(delivered.len(), 2);
        assert!(delivered.iter().all(|u| u.task_notice.as_ref().is_some_and(|m| m.markdown == u.text)));
    }

    #[tokio::test]
    async fn the_twenty_first_notice_delivers_itself_when_the_hour_frees_up() {
        let (_dir, manager, mock) = at_limit().await;
        let _clock = pause_clock();
        let _tx = hold(&manager).await;
        assert!(mock.requests().is_empty(), "the 21st notice is held");
        tokio::time::advance(HOUR - std::time::Duration::from_secs(1)).await;
        assert!(mock.requests().is_empty(), "not before a counted turn expires");
        mock.push(c6_testkit::Reply::Text("Saw the batch.".into()));
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        delivered(&manager, &mock).await;
    }

    #[tokio::test]
    async fn deliver_now_bypasses_the_hourly_timer() {
        let (_dir, manager, mock) = at_limit().await;
        let _clock = pause_clock();
        let tx = hold(&manager).await;
        mock.push(c6_testkit::Reply::Text("Saw them now.".into()));
        tx.send(ActorMsg::ReleaseNotices).unwrap();
        delivered(&manager, &mock).await;
        tokio::time::pause();
        tokio::time::advance(HOUR).await;
        barrier(&manager, "after-old-timer").await;
        assert_eq!(mock.requests().len(), 1, "the old timer doesn't deliver them twice");
    }

    #[tokio::test]
    async fn non_waking_notices_still_wait_for_explicit_input() {
        let (_dir, manager, mock) = at_limit().await;
        let _clock = pause_clock();
        let tx = manager.actor("held").await.unwrap().unwrap();
        for text in ["report 21", "report 22"] {
            tx.send(ActorMsg::ChildReport {
                command_id: format!("subagent:{text}"),
                recorded: tokio::sync::oneshot::channel().0,
                text: text.into(),
                wake: false,
                task_notice: Some(crate::subagent::task_report_notice("Review", "child", TurnEndReason::Completed, text)),
            })
            .unwrap();
        }
        barrier(&manager, "held-without-wake").await;
        tokio::time::advance(HOUR).await;
        barrier(&manager, "hour-free").await;
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert_eq!(notices_of(&events).2.len(), 2);
        assert!(mock.requests().is_empty(), "a free slot doesn't grant permission to wake");
        mock.push(c6_testkit::Reply::Text("Saw them when asked.".into()));
        tx.send(ActorMsg::ReleaseNotices).unwrap();
        delivered(&manager, &mock).await;
    }

    #[tokio::test]
    async fn recovery_reschedules_pending_notices_without_an_open_task() {
        let (_dir, manager, mock) = at_limit().await;
        seed_held(&manager).await;
        assert!(manager.journal.sessions_with_open_tasks().await.unwrap().is_empty());
        let _clock = pause_clock();
        manager.recover().await.unwrap();
        assert_eq!(manager.active(), ["held"], "startup, not a new command, restores its actor");
        barrier(&manager, "reloaded").await;
        assert!(mock.requests().is_empty(), "restart must not reset the limit");
        mock.push(c6_testkit::Reply::Text("Saw them after restart.".into()));
        tokio::time::advance(HOUR).await;
        delivered(&manager, &mock).await;
    }

    #[tokio::test]
    async fn recovery_leaves_old_held_notices_for_explicit_input() {
        let (dir, manager, mock) = at_limit().await;
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
        tokio::time::advance(HOUR).await;
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert_eq!(notices_of(&events).2.len(), 2, "old held notices remain pending");
        assert!(mock.requests().is_empty(), "old notices must not cause an upgrade burst");

        // Deliver now still loads the actor and delivers the batch.
        mock.push(c6_testkit::Reply::Text("Saw the old notices when asked.".into()));
        manager.actor("held").await.unwrap().unwrap().send(ActorMsg::ReleaseNotices).unwrap();
        delivered(&manager, &mock).await;
    }

    async fn seed_held(manager: &SessionManager) {
        // Seed held notices without starting an actor, exactly as a fresh
        // worker finds them on disk after its predecessor exited.
        for (i, text) in ["report 21", "report 22"].into_iter().enumerate() {
            let notice = c6_proto::NoticeQueued {
                command_id: format!("held{i}"),
                text: text.into(),
                wake: true,
                command: "task-notification".into(),
                task_notice: Some(crate::subagent::task_report_notice("Review", "child", TurnEndReason::Completed, text)),
                ..Default::default()
            };
            manager.journal.append("held", &ev(82 + i as u64, c6_core::now_ms(), session_event::Kind::NoticeQueued(notice))).await.unwrap();
        }
    }

    async fn barrier(manager: &SessionManager, id: &str) {
        // A recorded message after Reloaded is a barrier: the actor has
        // reconstructed its counted turns and armed its timer.
        let tx = manager.actor("held").await.unwrap().unwrap();
        tx.send(ActorMsg::SetSubagentModel { command_id: id.into(), model: None }).unwrap();
        logged(manager, |events| {
            events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::SubagentModelSet(m)) if m.command_id == id))
        })
        .await;
    }

    #[tokio::test]
    async fn overdue_notices_wait_for_the_head_after_restart() {
        let (_dir, mut manager, mock) = at_limit().await;
        seed_held(&manager).await;
        let head = manager.link.clone();
        manager.link = Arc::new(HeadLink::default());
        let _clock = pause_clock();
        manager.recover().await.unwrap();
        barrier(&manager, "reloaded").await;
        tokio::time::advance(HOUR).await;
        barrier(&manager, "still-offline").await;
        let events = manager.journal.events_after("held", 0, u32::MAX).await.unwrap();
        assert_eq!(notices_of(&events).2.len(), 2, "offline delivery must not consume the held batch");
        assert!(mock.requests().is_empty());

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
        delivered(&manager, &mock).await;
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

    fn leave_rewind_held(manager: &SessionManager, spec: &CreateSession, bin: &Path, cwd: &Path, conversation: &str) {
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
    }

    fn held_trace(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("held-trace.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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
        leave_rewind_held(&manager, &spec, &bin, dir.path(), "s");
        leave_rewind_held(&manager, &spec, &bin, dir.path(), &fork);
        let reopened = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let manager = SessionManager::new(reopened, Arc::default(), dir.path().join("worktrees"));
        manager.recover().await.unwrap();
        rewind_actor_barrier(&manager, "adopted").await;
        manager.actor("s").await.unwrap().unwrap().send(ActorMsg::Resume(1)).unwrap();
        rewind_actor_barrier(&manager, "stale-resume").await;
        let command = || SessionCommand {
            session_id: "s".into(),
            command_id: "after-rewind".into(),
            kind: Some(session_command::Kind::Input(c6_proto::UserInput {
                text: "display-only".into(),
                prompt: "model-only".into(),
                human_origin: true,
                ..Default::default()
            })),
        };
        manager.handle(command()).await.unwrap();
        c6_testkit::eventually("adopted fork completes a real input", async || {
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
        leave_rewind_held(&manager, &spec, &bin, dir.path(), "s");
        leave_rewind_held(&manager, &spec, &bin, dir.path(), "reset-attempt");
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
            ev(2, session_event::Kind::Checkpoint(Checkpoint { commit: checkpoint, error: String::new() })),
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
                tools: Arc::new(SessionTools::new(dir.path().into())),
                running: Arc::default(),
                claude_code: Arc::default(),
                inbox: Arc::default(),
            },
        );
        let command = |id: &str, to_seq, restore_files| SessionCommand {
            session_id: "s".into(),
            command_id: id.into(),
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
        let handle = Handle { tx, tools: tools.clone(), running: Arc::default(), claude_code: Arc::default(), inbox: Arc::default() };
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

    use super::*;

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
}
