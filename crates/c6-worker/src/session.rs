//! Session runtime: one actor task per active session, each the single writer
//! of its event log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, build_system_prompt, history_from_events, run_turn};
use c6_llm::{Message, Provider, Role, ToolProfile, model_info};
use c6_proto::{
    BackgroundTask, Checkpoint, CreateSession, EphemeralEvent, ErrorEvent, ModelSpec, ModelSwitched, Rewound, SessionActivity,
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
    link: Arc<HeadLink>,
    worktrees: PathBuf,
    /// The worker's state directory (checkpoint repositories live here).
    state_dir: PathBuf,
    actors: Arc<Mutex<HashMap<String, Handle>>>,
    /// Delegations running per top-level session (for its limit).
    working: Arc<Mutex<HashMap<String, usize>>>,
    /// The MCP server that gives Claude Code sessions carbon6's subagents
    /// (started on first use; `None` if it could not start).
    bridge: Arc<tokio::sync::OnceCell<Option<Arc<crate::mcp_bridge::Bridge>>>>,
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
    /// It runs Claude Code, whose tools carbon6 does not run (so its
    /// subagents do not share them).
    claude_code: bool,
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
    /// Rewind the conversation (after interrupting a running turn).
    Rewind {
        command_id: String,
        to_seq: u64,
        restore_files: bool,
    },
    /// An input, possibly switching the session's model from it on, and
    /// what is needed to pick the variant of a custom command it invokes.
    Input(Box<PendingInput>, Option<ModelSpec>, Box<CommandChoice>),
    Interrupt,
    /// A background task started or ended.
    Task(c6_tools::TaskEvent),
    /// A background task that was running when the worker restarted.
    Lost {
        task_id: String,
        kind: String,
        description: String,
        notify: bool,
        pgid: i32,
        /// A subagent's session.
        child: String,
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
        text: String,
        wake: bool,
    },
}

/// Background-task notices may start this many turns per hour in an idle
/// session; beyond that they wait for your next message.
const AUTO_TURNS_PER_HOUR: usize = 20;

impl SessionManager {
    pub fn new(journal: Arc<Journal>, link: Arc<HeadLink>, worktrees: PathBuf) -> Self {
        let state_dir = worktrees.parent().map(Path::to_path_buf).unwrap_or_else(|| worktrees.clone());
        prune_browser_profiles(&state_dir.join("browser"), std::time::Duration::from_secs(30 * 24 * 3600));
        Self { journal, link, worktrees, state_dir, actors: Arc::default(), working: Arc::default(), bridge: Arc::default() }
    }

    /// The MCP server for Claude Code sessions' subagents, started on first
    /// use.
    async fn bridge(&self) -> Option<Arc<crate::mcp_bridge::Bridge>> {
        self.bridge
            .get_or_init(|| async {
                crate::mcp_bridge::Bridge::start()
                    .await
                    .map(Arc::new)
                    .inspect_err(|e| warn!("the MCP bridge for Claude Code did not start: {e:#}"))
                    .ok()
            })
            .await
            .clone()
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

    /// Closes turns left open by a crash or kill.
    pub async fn recover(&self) -> anyhow::Result<()> {
        for (session_id, last_seq, turn) in self.journal.open_turns().await? {
            warn!(session = %session_id, turn, "closing turn interrupted by worker restart");
            // A subagent the turn was waiting on: its call gets what it did.
            let mut seq = last_seq;
            for result in self.lost_delegations(&session_id).await? {
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
        // Background tasks died with the previous process: resuming those
        // sessions reports them as lost.
        for session_id in self.journal.sessions_with_open_tasks().await? {
            if let Err(e) = self.actor(&session_id).await {
                warn!(session = %session_id, "resuming for lost background tasks: {e:#}");
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
                    ..Default::default()
                };
                let choice = CommandChoice { candidates_json: input.command_candidates_json, labels: input.labels };
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
                if self.journal.is_applied(&cmd.command_id).await.map_err(internal)? {
                    return Ok(());
                }
                // Only to one of the session's own messages.
                let events = self.journal.events_after(&cmd.session_id, 0, u32::MAX).await.map_err(internal)?;
                let is_user = events.iter().any(|e| e.seq == r.to_seq && matches!(e.kind, Some(session_event::Kind::User(_))));
                if !is_user {
                    return Err("can only rewind to before one of your messages".into());
                }
                let actor = self.actor(&cmd.session_id).await.map_err(internal)?.ok_or("unknown session on this worker")?;
                let _ = actor.send(ActorMsg::Rewind { command_id: cmd.command_id, to_seq: r.to_seq, restore_files: r.restore_files });
                Ok(())
            }
            Some(session_command::Kind::Interrupt(_)) => {
                if let Some(a) = self.live(&cmd.session_id) {
                    let _ = a.send(ActorMsg::Interrupt);
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
        let (cwd, worktree_branch, repo_root) = if spec.worktree {
            let (path, branch, root) = self.create_worktree(session_id, &cwd).await?;
            (path, branch, root)
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
        let Place { cwd, local, home, repo, worktree_branch, repo_root, ssh_target, system } = place;
        let model = spec.model.clone().unwrap_or_default();
        let mut setup_errors = Vec::new();
        let mut session_tools = Vec::new();
        let provider = Provider::from_kind(&model.connection_kind).unwrap_or(Provider::OpenAiChat);
        let context_window = model_info(provider, &model.model).context_window;
        let (profile, tools, system_prompt) = if model.connection_kind == "claude_code" {
            // Claude Code brings its own prompt (incl. CLAUDE.md) and tools.
            (crate::claude_code::PROFILE.to_owned(), Vec::new(), "(Claude Code's own system prompt)".to_owned())
        } else {
            let profile = model_info(provider, &model.model).tool_profile;
            // Secrets go into MCP servers' configs (and, per turn, commands).
            let names: Vec<String> = applicable_secrets(&spec, &labels).into_iter().map(|(n, _)| n).collect();
            let (secret_values, _) = fetch_secrets(&self.link, session_id, &names).await;
            let built = build_tools(&spec, &local, profile, &labels, &secret_values).await;
            setup_errors = built.errors;
            let mut prompt = system_prompt;
            if let Some(h) = crate::hooks::HookRunner::new(session_id, &local, &spec.hooks_json, &labels)
                && let Some(context) = h.run("SessionStart", None, serde_json::json!({"source": "startup"})).await.context
            {
                prompt.push_str(&format!("\n\n# Session start context (from a SessionStart hook)\n\n{context}"));
            }
            prompt.push_str(&context_sections(&spec, &labels));
            // When to propose task cards (docs/task-cards.md): an editable prompt.
            if built.tools.iter().any(|t| c6_tools::knowledge::is_task_card(&t.spec().name)) {
                let guidance = spec_prompt(&spec, "session.task_cards", "");
                if !guidance.trim().is_empty() {
                    prompt.push_str(TASK_CARDS_HEADING);
                    prompt.push_str(guidance.trim());
                }
            }
            if !built.skills.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&Skill::index(&built.skills, SkillTool::tool_name(profile == ToolProfile::Claude)));
            }
            let names = built.tools.iter().map(|t| t.spec().name).collect();
            session_tools = built.tools;
            (profile.as_str().to_owned(), names, prompt)
        };
        let started = SessionStarted {
            command_id: command_id.to_owned(),
            model: spec.model.clone(),
            cwd: cwd.display().to_string(),
            system_prompt,
            tools,
            tool_profile: profile.clone(),
            worktree_branch,
            repo_root,
            context_window,
            repo,
            home,
            ssh_target,
            system,
            ..Default::default()
        };
        let ev = SessionEvent { seq: 1, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Started(started.clone())) };
        self.journal.create_session(session_id, &spec, &ev).await.map_err(|e| format!("worker error: {e:#}"))?;
        // Non-fatal setup problems (an MCP server that would not start) are
        // shown in the session.
        let mut next_seq = 2;
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

    /// `git worktree add` on a new `c6/<id>` branch from HEAD. Returns the
    /// session directory (same relative position as `cwd` in the repo), the
    /// branch, and the repo root.
    async fn create_worktree(&self, session_id: &str, cwd: &std::path::Path) -> Result<(PathBuf, String, String), String> {
        let root = cwd
            .ancestors()
            .find(|d| d.join(".git").exists())
            .ok_or_else(|| format!("{} is not inside a git repository, so a worktree cannot be created", cwd.display()))?
            .to_owned();
        let repo = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
        let short: String =
            session_id.chars().filter(|c| c.is_ascii_hexdigit()).rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
        let branch = format!("c6/{short}");
        let path = self.worktrees.join(&repo).join(&short);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("creating {}: {e}", self.worktrees.display()))?;
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["worktree", "add", "-b", &branch])
            .arg(&path)
            .arg("HEAD")
            .output()
            .await
            .map_err(|e| format!("running git: {e}"))?;
        if !out.status.success() {
            return Err(format!("git worktree add failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        let rel = cwd.strip_prefix(&root).unwrap_or(std::path::Path::new(""));
        info!(session = %session_id, %branch, path = %path.display(), "worktree created");
        Ok((path.join(rel), branch, root.display().to_string()))
    }

    /// The session's actor, starting it from the journal if needed.
    async fn actor(&self, session_id: &str) -> anyhow::Result<Option<mpsc::UnboundedSender<ActorMsg>>> {
        Ok(self.load(session_id).await?.map(|h| h.tx))
    }

    /// The session's live actor and tools, starting it from the journal if
    /// needed.
    async fn load(&self, session_id: &str) -> anyhow::Result<Option<Handle>> {
        if let Some(a) = self.actors.lock().unwrap().get(session_id).filter(|a| !a.tx.is_closed()) {
            return Ok(Some(a.clone()));
        }
        let Some((spec, events)) = self.journal.load_session(session_id).await? else { return Ok(None) };
        let started = events.iter().find_map(|e| match &e.kind {
            Some(session_event::Kind::Started(s)) => Some(s.clone()),
            _ => None,
        });
        let Some(started) = started else { anyhow::bail!("session {session_id} has no SessionStarted event") };
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
            self.journal.append(session_id, &SessionEvent { seq: next_seq, ts_ms: c6_core::now_ms(), kind: Some(kind) }).await?;
            next_seq += 1;
        }
        let turns = events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_)))).count() as u64;
        let mut spec = spec;
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
        for t in open {
            let child = events
                .iter()
                .find_map(|e| match &e.kind {
                    Some(session_event::Kind::SubagentSpawned(sp)) if sp.background_task_id == t.task_id => {
                        Some(sp.child_session_id.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let _ = tx.send(ActorMsg::Lost {
                task_id: t.task_id,
                kind: t.kind,
                description: t.description,
                notify: t.notify,
                pgid: t.pgid,
                child,
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
        let conv = Conversation { history: Vec::new(), context_tokens: 0 };
        Ok(self.spawn(session_id, spec, started, conv, 2, 0, Some(tools)).await?.tx)
    }

    /// The shared tool state of a subagent's parent, starting the parent's
    /// actor if needed (after a restart, a subagent may be loaded first).
    /// None for a Claude Code parent: carbon6 runs none of its tools, so
    /// its subagents work with hooks, policies and sinks of their own.
    fn parent_tools<'a>(&'a self, parent_id: &'a str) -> BoxFuture<'a, anyhow::Result<Option<Arc<SessionTools>>>> {
        Box::pin(async move { Ok(self.load(parent_id).await?.filter(|h| !h.claude_code).map(|h| h.tools)) })
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
        let writer = Arc::new(Writer {
            session_id: session_id.to_owned(),
            journal: self.journal.clone(),
            link: self.link.clone(),
            next_seq: tokio::sync::Mutex::new(next_seq),
        });
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
        session_tools.browser.set_profile(self.state_dir.join("browser").join(session_id));
        let mut hooks = None;
        let mut policies = None;
        // A subagent works with its parent's tool hooks and policies (held
        // calls wait in the parent), publisher, memory, notices and way to
        // the head. Without its parent here (it was trimmed), its own.
        let parent = match &delegation {
            Some(d) => self.parent_tools(&d.parent_session_id).await.unwrap_or_else(|e| {
                warn!(session = %session_id, parent = %d.parent_session_id, "a subagent without its parent: {e:#}");
                None
            }),
            None => None,
        };
        if let Some(parent) = &parent {
            session_tools.set_hooks(parent.hooks().map(|h| Arc::new(crate::subagent::SubagentHooks(h)) as Arc<dyn c6_tools::Hooks>));
            session_tools.set_publisher(parent.publisher());
            session_tools.set_memory(parent.memory());
            session_tools.set_notices(parent.notices());
            session_tools.set_head(parent.head());
        } else {
            hooks = crate::hooks::HookRunner::new(session_id, &local, &spec.hooks_json, &labels).map(Arc::new);
            session_tools.set_head(Some(Arc::new(SessionHead { link: self.link.clone(), session_id: on_behalf_of.clone() })));
        }
        if started.tool_profile != crate::claude_code::PROFILE && parent.is_none() {
            // Policies first (a denial wins), then your hooks.
            let (ap_tx, mut ap_rx) = mpsc::unbounded_channel();
            policies = crate::policy::Policies::new(&spec.policies_json, &labels, ap_tx);
            let actor_tx = tx.clone();
            tokio::spawn(async move {
                while let Some(e) = ap_rx.recv().await {
                    if actor_tx.send(ActorMsg::Approval(e)).is_err() {
                        break;
                    }
                }
            });
            let mut chain: Vec<Arc<dyn c6_tools::Hooks>> = Vec::new();
            if let Some(p) = &policies {
                chain.push(p.clone());
            }
            if let Some(h) = &hooks {
                chain.push(h.clone());
            }
            session_tools.set_hooks(match chain.len() {
                0 => None,
                1 => chain.pop(),
                _ => Some(Arc::new(crate::policy::Chain(chain))),
            });
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
        if started.tool_profile != crate::claude_code::PROFILE {
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
            inbox: Arc::default(),
            // Files on an SSH target are not checkpointed.
            checkpoints: Arc::new(Checkpoints::new(&self.state_dir, session_id, if remote { &local } else { Path::new(&started_cwd) })),
            auto_turns: VecDeque::new(),
            deferred: Vec::new(),
            on_behalf_of,
            waiting: Vec::new(),
            manager: self.clone(),
            subagent_default: Arc::new(Mutex::new(crate::subagent::SubagentDefault::of(&spec_default))),
            running_flag: Arc::default(),
            report: None,
        };
        let handle = Handle {
            tx,
            tools: actor.session_tools.clone(),
            running: actor.running_flag.clone(),
            claude_code: actor.started.tool_profile == crate::claude_code::PROFILE,
        };
        tokio::spawn(actor.run(rx));
        self.actors.lock().unwrap().insert(session_id.to_owned(), handle.clone());
        Ok(handle)
    }
}

/// The conversation a session's log adds up to (its seed first, rewinds
/// applied), and the model it runs on now (its last switch, set in
/// `spec`). Reasoning from before the current model's run is dropped:
/// another model wrote it.
fn conversation_from_log(spec: &mut CreateSession, events: &[SessionEvent]) -> Conversation {
    let all = [seed_events(spec), events.to_vec()].concat();
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
    // Start of the current model's run: just after the last event naming
    // another model, at the next event naming this one.
    let other = all.iter().rposition(|e| model_of(e).is_some_and(|m| Some(key(&m)) != current));
    let history = match other {
        Some(i) => {
            let run = all[i + 1..].iter().position(|e| model_of(e).is_some()).map_or(all.len(), |p| i + 1 + p);
            history_from_events(&[c6_agent::portable_events(&all[..run]), all[run..].to_vec()].concat())
        }
        None => history_from_events(&all),
    };
    // The last request's size since the last compaction, for the next
    // compaction decision.
    let context_tokens = c6_agent::effective_events(events)
        .iter()
        .rev()
        .take_while(|e| !matches!(e.kind, Some(session_event::Kind::Compaction(_))))
        .find_map(|e| match &e.kind {
            Some(session_event::Kind::Usage(u)) => Some(u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens),
            _ => None,
        })
        .unwrap_or(0);
    Conversation { history, context_tokens }
}

/// Sets a spec's default for its subagents (`None`: the session's own
/// model).
fn set_subagent_default(spec: &mut CreateSession, model: Option<ModelSpec>) {
    spec.subagent_target = model;
    spec.subagent_model.clear();
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

/// Inputs waiting for the next model-request boundary.
#[derive(Default)]
struct Inbox {
    /// Inputs with the model switch each asks for.
    queue: VecDeque<(PendingInput, Option<ModelSpec>)>,
    /// Every command id ever queued here, so redeliveries are ignored even
    /// while an input is being recorded.
    seen: HashSet<String>,
}

impl Inbox {
    fn push(&mut self, input: PendingInput, model: Option<ModelSpec>) {
        if self.seen.insert(input.command_id.clone()) {
            self.queue.push_back((input, model));
        }
    }

    /// The model switch the next input asks for, which applies before it.
    fn take_switch(&mut self) -> Option<ModelSpec> {
        self.queue.front_mut().and_then(|(_, m)| m.take())
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
    auto_turns: VecDeque<std::time::Instant>,
    /// Notices held back by that limit, delivered with your next message.
    deferred: Vec<PendingInput>,
    /// The session secrets and head calls are asked for on behalf of: its
    /// own, or a subagent's top-level session.
    on_behalf_of: String,
    /// Delegations waiting for their turn to end: the input's command id.
    waiting: Vec<(String, tokio::sync::oneshot::Sender<Outcome>)>,
    /// To start subagents.
    manager: SessionManager,
    /// The default for its subagents (as in `spec`), shared with a running
    /// turn's subagent runner.
    subagent_default: crate::subagent::SharedDefault,
    /// Shared with the manager: a turn is running.
    running_flag: Arc<std::sync::atomic::AtomicBool>,
    /// A subagent's inputs since its last report that no call waits on
    /// (yours, notices, messages joining a turn): what they said, and
    /// whether the report should wake the parent.
    report: Option<(Vec<String>, bool)>,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ActorMsg>) {
        let mut running: Option<(tokio::task::JoinHandle<Option<Conversation>>, CancellationToken)> = None;
        // A rewind that waits for the running turn to stop.
        let mut rewind: Option<(String, u64, bool)> = None;
        loop {
            if running.is_none()
                && let Some((command_id, to_seq, restore)) = rewind.take()
                && let Err(e) = self.rewind(command_id, to_seq, restore).await
            {
                error!(session = %self.writer.session_id, "rewinding: {e:#}");
                return;
            }
            if running.is_none() && !self.inbox.lock().unwrap().queue.is_empty() {
                let switch = self.inbox.lock().unwrap().take_switch();
                if let Some(model) = switch
                    && let Err(e) = self.switch_model(model).await
                {
                    error!(session = %self.writer.session_id, "recording a model switch: {e:#}");
                    return;
                }
                running = Some(self.start_turn());
                self.running_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            tokio::select! {
                msg = rx.recv() => match msg {
                    None => return,
                    Some(ActorMsg::Input(mut input, model, choice)) => {
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
                        if self.spec.delegation.is_some() && !input.is_compact() {
                            let r = self.report.get_or_insert_with(Default::default);
                            r.0.push(input.text.clone());
                            r.1 |= input.wake;
                        }
                        let held: Vec<PendingInput> = std::mem::take(&mut self.deferred);
                        let mut inbox = self.inbox.lock().unwrap();
                        for n in held {
                            inbox.push(n, None);
                        }
                        inbox.push(*input, model)
                    }
                    Some(ActorMsg::Interrupt) => {
                        if let Some((_, cancel)) = &running { cancel.cancel(); }
                    }
                    Some(ActorMsg::Delegate(input, done)) => {
                        match done {
                            Some(done) => self.waiting.push((input.command_id.clone(), done)),
                            None => self.report.get_or_insert_with(Default::default).0.push(String::new()),
                        }
                        // Notices held for the next input come with it.
                        let held: Vec<PendingInput> = std::mem::take(&mut self.deferred);
                        let mut inbox = self.inbox.lock().unwrap();
                        for n in held {
                            inbox.push(n, None);
                        }
                        inbox.push(*input, None);
                    }
                    Some(ActorMsg::ChildReport { text, wake }) => {
                        let input = PendingInput {
                            command_id: format!("subagent:{}", uuid::Uuid::now_v7()),
                            text,
                            command: "task-notification".into(),
                            ..Default::default()
                        };
                        if let Err(e) = self.queue_notice(input, running.is_some(), wake).await {
                            error!(session = %self.writer.session_id, "queueing a subagent's report: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Task(ev)) => {
                        let state = match ev.state {
                            c6_tools::TaskState::Started => "started",
                            c6_tools::TaskState::Finished => "finished",
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
                        };
                        // The agent stopped it itself: nothing to tell it.
                        let notice = (ev.notify && !matches!(ev.state, c6_tools::TaskState::Started | c6_tools::TaskState::Stopped))
                            .then(|| task_notice(&record, &ev.output));
                        if let Err(e) = self.task_event(record, notice, running.is_some()).await {
                            error!(session = %self.writer.session_id, "recording a background task: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::Lost { task_id, kind, description, notify, pgid, child }) => {
                        // It may outlive the worker (its own process group):
                        // stop it, since nothing reads its output any more.
                        stop_orphan(pgid, &description);
                        // A subagent's log is kept: what it did so far.
                        let report = if child.is_empty() {
                            String::new()
                        } else {
                            crate::subagent::partial(&self.spec, &crate::subagent::digest(&self.journal, &child, "lost", "").await)
                        };
                        let record = BackgroundTask { task_id, kind, description, state: "lost".into(), notify, ..Default::default() };
                        let notice = notify.then(|| task_notice(&record, &report));
                        if let Err(e) = self.task_event(record, notice, running.is_some()).await {
                            error!(session = %self.writer.session_id, "recording a lost background task: {e:#}");
                            return;
                        }
                    }
                    Some(ActorMsg::StopTask(id)) => {
                        self.session_tools.background.kill_by_user(&id);
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
                        let notice = c6_proto::Notice { title: n.title, message: n.message, priority: n.priority };
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
                    Some(ActorMsg::Rewind { command_id, to_seq, restore_files }) => {
                        if let Some((_, cancel)) = &running { cancel.cancel(); }
                        rewind = Some((command_id, to_seq, restore_files));
                    }
                },
                done = async { (&mut running.as_mut().unwrap().0).await }, if running.is_some() => {
                    running = None;
                    self.running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                    self.writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: false, ..Default::default() }));
                    match done {
                        Ok(Some(conv)) => {
                            self.conv = Some(conv);
                            if let Err(e) = self.settle_delegations().await {
                                error!(session = %self.writer.session_id, "reading a delegation's outcome: {e:#}");
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
        let settled: Vec<_> = {
            let inbox = self.inbox.lock().unwrap();
            let (done, still): (Vec<_>, Vec<_>) =
                std::mem::take(&mut self.waiting).into_iter().partition(|(id, _)| !inbox.queue.iter().any(|(i, _)| &i.command_id == id));
            self.waiting = still;
            done
        };
        // A call still waiting gets this turn's report: nothing to tell.
        let told = settled.iter().any(|(_, tx)| !tx.is_closed());
        if settled.is_empty() && self.report.is_none() {
            return Ok(());
        }
        let events = self.journal.events_after(&self.writer.session_id, 0, u32::MAX).await?;
        let outcome = last_turn_outcome(&events);
        let report = self.report.take();
        if !told && let Some((said, wake)) = report {
            let sid = self.writer.session_id.clone();
            let body = if outcome.reason == TurnEndReason::Completed && !outcome.report.trim().is_empty() {
                format!("<subagent id=\"{sid}\" status=\"completed\">\nReport:\n{}\n</subagent>", outcome.report.trim_end())
            } else {
                let status = crate::subagent::status_of(outcome.reason, &outcome.report);
                crate::subagent::partial(&self.spec, &crate::subagent::digest(&self.journal, &sid, status, "").await)
            };
            let said: Vec<String> = said.into_iter().filter(|s| !s.trim().is_empty()).collect();
            let text = if said.is_empty() {
                format!("Subagent {sid} worked on its own (no call of yours waited on it):\n{body}")
            } else {
                format!("The user messaged subagent {sid} directly: \"{}\"\n{body}", said.join("\" / \""))
            };
            match self.manager.actor_for(&d.parent_session_id).await {
                Ok(Some(parent)) => {
                    let _ = parent.send(ActorMsg::ChildReport { text, wake });
                }
                Ok(None) => warn!(session = %sid, "a subagent's report has no parent on this worker"),
                Err(e) => warn!(session = %sid, "a subagent's report could not reach its parent: {e:#}"),
            }
        }
        for (_, tx) in settled {
            let _ = tx.send(Outcome { reason: outcome.reason, report: outcome.report.clone(), error: outcome.error.clone() });
        }
        Ok(())
    }

    /// Queues a notice for the agent: it joins a running turn at its next
    /// step; in an idle session it starts a turn if `wake` (within the
    /// hourly limit), else it waits for the next input.
    async fn queue_notice(&mut self, input: PendingInput, busy: bool, wake: bool) -> anyhow::Result<()> {
        if busy || !self.inbox.lock().unwrap().queue.is_empty() {
            self.inbox.lock().unwrap().push(input, None);
            return Ok(());
        }
        if !wake {
            self.deferred.push(input);
            return Ok(());
        }
        let now = std::time::Instant::now();
        while self.auto_turns.front().is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(3600)) {
            self.auto_turns.pop_front();
        }
        if self.auto_turns.len() < AUTO_TURNS_PER_HOUR {
            self.auto_turns.push_back(now);
            self.inbox.lock().unwrap().push(input, None);
            return Ok(());
        }
        warn!(session = %self.writer.session_id, "notice held: too many automatic turns this hour");
        self.deferred.push(input);
        self.writer
            .append(session_event::Kind::Error(ErrorEvent {
                message: format!(
                    "A notice for the agent will come with your next message: notices already started {AUTO_TURNS_PER_HOUR} turns in the last hour."
                ),
            }))
            .await?;
        Ok(())
    }

    /// Logs a background task's start or end and, for an end the agent
    /// should hear about, queues the notice: it reaches a running turn at
    /// its next step, and starts a turn in an idle session (within the
    /// hourly limit; beyond it the notice waits for your next message).
    async fn task_event(&mut self, record: BackgroundTask, notice: Option<String>, busy: bool) -> anyhow::Result<()> {
        let task_id = record.task_id.clone();
        self.writer.append(session_event::Kind::BackgroundTask(record)).await?;
        let Some(text) = notice else { return Ok(()) };
        let input = PendingInput {
            command_id: format!("task:{}", uuid::Uuid::now_v7()),
            text,
            command: "task-notification".into(),
            ..Default::default()
        };
        let now = std::time::Instant::now();
        while self.auto_turns.front().is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(3600)) {
            self.auto_turns.pop_front();
        }
        let held = {
            let mut inbox = self.inbox.lock().unwrap();
            if busy || !inbox.queue.is_empty() {
                inbox.push(input, None);
                None
            } else if self.auto_turns.len() < AUTO_TURNS_PER_HOUR {
                self.auto_turns.push_back(now);
                inbox.push(input, None);
                None
            } else {
                Some(input)
            }
        };
        let Some(input) = held else { return Ok(()) };
        warn!(session = %self.writer.session_id, task = %task_id, "background-task notice held: too many automatic turns this hour");
        self.deferred.push(input);
        self.writer
            .append(session_event::Kind::Error(ErrorEvent {
                message: format!(
                    "Background task {task_id} ended. The agent will hear about it with your next message: notices already started {AUTO_TURNS_PER_HOUR} turns in the last hour."
                ),
            }))
            .await?;
        Ok(())
    }

    /// Rewinds the conversation to before event `to_seq`, restoring files
    /// to the checkpoint taken before it if asked to.
    async fn rewind(&mut self, command_id: String, to_seq: u64, restore_files: bool) -> anyhow::Result<()> {
        let sid = self.writer.session_id.clone();
        let events = self.journal.events_after(&sid, 0, u32::MAX).await?;
        let mut restore_error = String::new();
        let mut restored = false;
        if restore_files {
            let commit = events.iter().filter(|e| e.seq < to_seq).rev().find_map(|e| match &e.kind {
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
        self.conv = Some(conversation_from_log(&mut self.spec, &events));
        Ok(())
    }

    /// Runs the session on `model` from the next turn on. Earlier reasoning
    /// is dropped from the history: it is bound to the model that wrote it.
    async fn switch_model(&mut self, model: ModelSpec) -> anyhow::Result<()> {
        let current = self.spec.model.clone().unwrap_or_default();
        if (&current.connection_id, &current.model, &current.effort) == (&model.connection_id, &model.model, &model.effort) {
            return Ok(()); // e.g. redelivered after the switch was recorded
        }
        let provider = Provider::from_kind(&model.connection_kind).unwrap_or(Provider::OpenAiChat);
        let context_window = model_info(provider, &model.model).context_window;
        self.writer.append(session_event::Kind::ModelSwitched(ModelSwitched { model: Some(model.clone()), context_window })).await?;
        info!(session = %self.writer.session_id, model = %model.model, "model switched");
        // An effort change keeps the model, and with it the reasoning.
        let same_model = (&current.connection_id, &current.model) == (&model.connection_id, &model.model);
        self.spec.model = Some(model);
        if !same_model && let Some(conv) = &mut self.conv {
            conv.history = c6_agent::portable_messages(std::mem::take(&mut conv.history));
        }
        Ok(())
    }

    /// The tools a Claude Code session gets over MCP: carbon6's `Task` and
    /// `SendMessage` (no background runs: nothing could deliver their
    /// notices to Claude Code). None without a connection to run on.
    fn claude_code_subagent_tools(&self) -> Vec<Arc<dyn Tool>> {
        let depth = self.spec.delegation.as_ref().map_or(0, |d| d.depth);
        let connections = task_connections(&self.spec);
        if connections.is_empty() || !crate::subagent::may_delegate(&self.spec, depth) {
            return Vec::new();
        }
        let model = self.spec.model.as_ref().map(|m| m.model.as_str()).unwrap_or_default();
        let labels =
            session_labels(&self.spec.labels, &self.started.repo, &self.started.cwd, &self.started.home, model, &self.started.ssh_target);
        let agents = session_agents(&self.spec, &labels);
        vec![
            Arc::new(c6_tools::Task::with_agents(true, agents).with_connections(connections).foreground_only()),
            Arc::new(c6_tools::SendMessage::claude().foreground_only()),
        ]
    }

    fn start_turn(&mut self) -> (tokio::task::JoinHandle<Option<Conversation>>, CancellationToken) {
        self.turns += 1;
        let turn = self.turns;
        let cancel = CancellationToken::new();
        let model = self.spec.model.clone().unwrap_or_default();
        let provider = Provider::from_kind(&model.connection_kind).unwrap_or(Provider::OpenAiChat);
        let info = model_info(provider, &model.model);
        let cfg = TurnConfig {
            model: model.model.clone(),
            system_prompt: self.started.system_prompt.clone(),
            max_output_tokens: (model.max_output_tokens > 0).then_some(model.max_output_tokens),
            cache_key: self.writer.session_id.clone(),
            effort: Some(model.effort.clone())
                .filter(|e| !e.is_empty())
                .or_else(|| (model.connection_kind != "claude_code").then_some(info.default_effort).flatten().map(str::to_owned)),
            context_window: info.context_window,
            web_search: provider != Provider::OpenAiChat,
            compact: compact_prompts(&self.spec),
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

        let claude_code = self.started.tool_profile == crate::claude_code::PROFILE;
        let seed = if claude_code { self.spec.seed.clone() } else { Vec::new() };
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
        let claude_settings = self.hooks.as_ref().filter(|_| claude_code).map(|h| h.claude_settings());
        // carbon6's subagents, offered to Claude Code over MCP.
        let bridge_tools = if claude_code { self.claude_code_subagent_tools() } else { Vec::new() };
        let (manager, journal) = (self.manager.clone(), self.journal.clone());
        // Claude Code's own subagents are mirrored into sessions of their own.
        let observer = crate::claude_code::Observer {
            journal: self.journal.clone(),
            link: self.link.clone(),
            session_id: self.writer.session_id.clone(),
            spec: self.spec.clone(),
            started: self.started.clone(),
        };
        let task = tokio::spawn(async move {
            let sink: Arc<dyn EventSink> = writer.clone();
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
                let registration = match (bridge_tools.is_empty(), manager.bridge().await) {
                    (false, Some(bridge)) => {
                        session_tools.set_subagents(Some(Arc::new(crate::subagent::Runner::new(sub, &[]))));
                        Some(bridge.register(crate::mcp_bridge::Entry {
                            session_id: writer.session_id.clone(),
                            session: session_tools.clone(),
                            tools: bridge_tools,
                            cancel: c.clone(),
                            journal,
                            matched: Mutex::default(),
                        }))
                    }
                    _ => None,
                };
                let mcp_config = registration.as_ref().map(|r| r.mcp_config());
                let inputs = inbox.lock().unwrap().drain();
                // Claude Code keeps its own history: a side chat's context
                // goes in as text with the first question.
                let preamble = (turn == 1 && !seed.is_empty()).then(|| {
                    format!(
                        "<conversation-so-far>\n{}\n</conversation-so-far>\n\nThe user branched off the conversation above to ask:",
                        c6_agent::transcript_text(&seed)
                    )
                });
                let result = crate::claude_code::run_turn(
                    turn,
                    &writer.session_id,
                    &session_tools.cwd(),
                    &cfg.model,
                    cfg.effort.as_deref(),
                    inputs,
                    preamble.as_deref(),
                    claude_settings.as_deref(),
                    mcp_config.as_deref(),
                    &sink,
                    Some(&observer),
                    &c,
                )
                .await
                .map(drop);
                drop(registration);
                session_tools.set_subagents(None);
                result
            } else {
                run_session_turn(turn, &cfg, &model.connection_id, &link, &tools, &session_tools, &sink, &mut conv, &inbox, &c, sub).await
            };
            match result {
                Ok(()) => Some(conv),
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

#[allow(clippy::too_many_arguments)]
async fn run_session_turn(
    turn: u64,
    cfg: &TurnConfig,
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
    let client: Arc<dyn c6_llm::ModelClient> =
        match link.credential(connection_id).await.and_then(|g| Ok(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers)?)) {
            Ok(_) => Arc::new(crate::link::LinkedClient::new(link.clone(), connection_id)),
            Err(e) => {
                sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;
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

fn yes() -> bool {
    true
}

/// The memory section of a new session's system prompt: the notes whose
/// scope matches its labels (most specific first, newest first, within a
/// budget), and how to keep them current.
pub(crate) fn memory_section(spec: &CreateSession, labels: &[String]) -> String {
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
    let mut s = format!(
        "\n\n# Memory\n\nNotes kept across sessions. They may be out of date: trust what you observe, and keep them current. When you learn \
         something durable (how a machine or project is set up, where things are, the user's preferences), save it with the remember \
         tool, scoped to one of this session's labels ({}) or to every session; correct a note by passing its id as replaces, and \
         remove wrong ones with forget.\n",
        shown.join(", ")
    );
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
        match link.credential(&format!("secret:{name}@{session_id}")).await {
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

pub(crate) async fn build_tools(
    spec: &CreateSession,
    cwd: &std::path::Path,
    profile: ToolProfile,
    labels: &[String],
    secrets: &[(String, String)],
) -> BuiltTools {
    let mut tools = c6_tools::tools_for(profile);
    let mut errors = Vec::new();
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
    tools.push(Arc::new(c6_tools::records::SeriesRecord::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::SeriesQuery::new(profile == ToolProfile::Claude)));
    // Collections: typed tables in the session's realm.
    tools.push(Arc::new(c6_tools::records::RecordsQuery::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::RecordsGet::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::RecordsPut::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::records::RecordsDelete::new(profile == ToolProfile::Claude)));
    // carbon6's API reference, for writing code that works with carbon6.
    tools.push(Arc::new(c6_tools::api_reference::ApiReference::new(profile == ToolProfile::Claude)));
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
    if let Some(chrome) = c6_tools::browser::find_chrome() {
        tools.push(Arc::new(c6_tools::browser::BrowserTool::new(profile == ToolProfile::Claude, chrome)));
    }
    // Named agents (from the head's settings) become Task's subagent types,
    // and it offers the connections subagents may run on.
    let agents = session_agents(spec, labels);
    let connections = task_connections(spec);
    if (!agents.is_empty() || !connections.is_empty())
        && let Some(i) = tools.iter().position(|t| c6_tools::Task::is_task(&t.spec().name))
    {
        tools[i] = Arc::new(c6_tools::Task::with_agents(profile == ToolProfile::Claude, agents).with_connections(connections));
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
    BuiltTools { tools, skills, errors }
}

/// The single writer of one session's log.
pub(crate) struct Writer {
    pub session_id: String,
    pub journal: Arc<Journal>,
    pub link: Arc<HeadLink>,
    pub next_seq: tokio::sync::Mutex<u64>,
}

#[async_trait]
impl EventSink for Writer {
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
        let mut next = self.next_seq.lock().await;
        let ev = SessionEvent { seq: *next, ts_ms: c6_core::now_ms(), kind: Some(kind) };
        self.journal.append(&self.session_id, &ev).await?;
        *next += 1;
        Ok(ev.seq)
    }

    fn ephemeral(&self, kind: ephemeral_event::Kind) {
        self.link.ephemeral(EphemeralEvent { session_id: self.session_id.clone(), kind: Some(kind) });
    }
}

/// Stops a background command left running by a previous worker process,
/// if process group `pgid` still runs that command (pids get reused).
fn stop_orphan(pgid: i32, command: &str) {
    if pgid <= 0 {
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

fn task_notice(t: &BackgroundTask, output: &str) -> String {
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

/// The system prompt's sections from the head's context: your notes, the
/// memex brief and the secrets that apply.
pub(crate) fn context_sections(spec: &CreateSession, labels: &[String]) -> String {
    let mut prompt = String::new();
    // Your notes, by their scope; then what the knowledge store
    // knows about this session's things (it leaves the notes out).
    prompt.push_str(&memory_section(spec, labels));
    if !spec.memex_brief.trim().is_empty() {
        prompt.push_str("\n\n# Memex\n\n");
        prompt.push_str(spec.memex_brief.trim());
    }
    let secrets = applicable_secrets(spec, labels);
    if !secrets.is_empty() {
        prompt.push_str(
            "\n\n# Secrets\n\nThese environment variables hold secrets for your shell commands. Use them by name (for example \"$NAME\"), never \
             print or echo them, and do not write them into files; their values are hidden from you and replaced by [secret:NAME] in output.\n",
        );
        for (name, description) in &secrets {
            prompt.push_str(&format!("- {name}{}\n", if description.is_empty() { String::new() } else { format!(": {description}") }));
        }
    }
    prompt
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

    #[test]
    fn stops_orphaned_background_commands() {
        use std::os::unix::process::CommandExt;
        let spawn = |cmd: &str| std::process::Command::new("/bin/sh").args(["-c", cmd]).process_group(0).spawn().unwrap();
        let mut orphan = spawn("sleep 77; echo orphan-test");
        let mut other = spawn("sleep 78; echo other-test");
        let pid = |c: &std::process::Child| c.id() as i32;
        // A different command under that pid is left alone.
        stop_orphan(pid(&other), "sleep 77; echo orphan-test");
        stop_orphan(pid(&orphan), "sleep 77; echo orphan-test");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while orphan.try_wait().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "the orphan was not stopped");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(other.try_wait().unwrap().is_none(), "another command was stopped");
        other.kill().unwrap();
        let _ = other.wait();
    }
}
