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

pub struct SessionManager {
    journal: Arc<Journal>,
    link: Arc<HeadLink>,
    worktrees: PathBuf,
    /// The worker's state directory (checkpoint repositories live here).
    state_dir: PathBuf,
    actors: Mutex<HashMap<String, mpsc::UnboundedSender<ActorMsg>>>,
}

struct CommandChoice {
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

enum ActorMsg {
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
    },
    /// Stop a background task (the user asked).
    StopTask(String),
    /// Task ids up to bg{n} were used before a restart.
    Reserve(u64),
    /// The agent published a file.
    Published(c6_tools::Published),
    /// The agent changed a memory note.
    Memory(c6_tools::MemoryChange),
    /// The agent sent you a notification.
    Notice(c6_tools::Notice),
    /// A tool call held for approval, or what became of it.
    Approval(crate::policy::ApprovalEvent),
    /// Your answer for a held tool call.
    Decide {
        call_id: String,
        allow: bool,
    },
}

/// Background-task notices may start this many turns per hour in an idle
/// session; beyond that they wait for your next message.
const AUTO_TURNS_PER_HOUR: usize = 20;

impl SessionManager {
    pub fn new(journal: Arc<Journal>, link: Arc<HeadLink>, worktrees: PathBuf) -> Self {
        let state_dir = worktrees.parent().map(Path::to_path_buf).unwrap_or_else(|| worktrees.clone());
        prune_browser_profiles(&state_dir.join("browser"), std::time::Duration::from_secs(30 * 24 * 3600));
        Self { journal, link, worktrees, state_dir, actors: Mutex::default() }
    }

    /// Session ids with a live actor.
    pub fn active(&self) -> Vec<String> {
        self.actors.lock().unwrap().iter().filter(|(_, a)| !a.is_closed()).map(|(id, _)| id.clone()).collect()
    }

    /// Closes turns left open by a crash or kill.
    pub async fn recover(&self) -> anyhow::Result<()> {
        for (session_id, last_seq, turn) in self.journal.open_turns().await? {
            warn!(session = %session_id, turn, "closing turn interrupted by worker restart");
            let ev = SessionEvent {
                seq: last_seq + 1,
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
                if let Some(a) = self.actors.lock().unwrap().get(&cmd.session_id) {
                    let _ = a.send(ActorMsg::Interrupt);
                }
                Ok(())
            }
            // Questions are the head's assistant's; worker sessions ask none.
            Some(session_command::Kind::Answer(_)) => Ok(()),
            Some(session_command::Kind::Approve(a)) => {
                if let Some(actor) = self.actors.lock().unwrap().get(&cmd.session_id) {
                    let _ = actor.send(ActorMsg::Decide { call_id: a.call_id, allow: a.allow });
                }
                Ok(())
            }
            Some(session_command::Kind::StopTask(t)) => {
                // Without a live actor no task of the session is running.
                if let Some(a) = self.actors.lock().unwrap().get(&cmd.session_id) {
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
            // With a memex, its brief (which holds your notes) replaces the
            // memory section.
            if spec.memex_brief.trim().is_empty() {
                prompt.push_str(&memory_section(&spec, &labels));
            } else {
                prompt.push_str("\n\n# Memex\n\n");
                prompt.push_str(spec.memex_brief.trim());
            }
            let secrets = applicable_secrets(&spec, &labels);
            if !secrets.is_empty() {
                prompt.push_str(
                    "\n\n# Secrets\n\nThese environment variables hold secrets for your shell commands. Use them by name (for example \"$NAME\"), never \
                     print or echo them, and do not write them into files; their values are hidden from you and replaced by [secret:NAME] in output.\n",
                );
                for (name, description) in &secrets {
                    prompt.push_str(&format!(
                        "- {name}{}\n",
                        if description.is_empty() { String::new() } else { format!(": {description}") }
                    ));
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
        self.spawn(session_id, spec, started, conv, next_seq, 0, Some(session_tools)).await;
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
        if let Some(a) = self.actors.lock().unwrap().get(session_id).filter(|a| !a.is_closed()) {
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
        let tx = self.spawn(session_id, spec, started, conv, next_seq, turns, None).await;
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
            let _ =
                tx.send(ActorMsg::Lost { task_id: t.task_id, kind: t.kind, description: t.description, notify: t.notify, pgid: t.pgid });
        }
        Ok(Some(tx))
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
    ) -> mpsc::UnboundedSender<ActorMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
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
                let labels =
                    session_labels(&spec.labels, &started.repo, &started.cwd, &started.home, &spawn_model(&spec), &started.ssh_target);
                let names: Vec<String> = applicable_secrets(&spec, &labels).into_iter().map(|(n, _)| n).collect();
                let (secret_values, _) = fetch_secrets(&self.link, session_id, &names).await;
                build_tools(&spec, &local, ToolProfile::parse(&started.tool_profile), &labels, &secret_values).await.tools
            }
        };
        let started_cwd = started.cwd.clone();
        // The head's hooks. Claude Code sessions get them as Claude Code
        // settings and run them itself.
        let labels = session_labels(&spec.labels, &started.repo, &started.cwd, &started.home, &spawn_model(&spec), &started.ssh_target);
        let hooks = crate::hooks::HookRunner::new(session_id, &local, &spec.hooks_json, &labels).map(Arc::new);
        let ssh = remote.then(|| Arc::new(c6_tools::remote::SshHost::new(&started.ssh_target, &started.home)));
        let session_tools = Arc::new(SessionTools::with_ssh(PathBuf::from(&started.cwd), ssh));
        session_tools.browser.set_profile(self.state_dir.join("browser").join(session_id));
        session_tools.set_head(Some(Arc::new(SessionHead { link: self.link.clone(), session_id: session_id.to_owned() })));
        let mut policies = None;
        if started.tool_profile != crate::claude_code::PROFILE {
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
        };
        tokio::spawn(actor.run(rx));
        self.actors.lock().unwrap().insert(session_id.to_owned(), tx.clone());
        tx
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
                    Some(ActorMsg::Lost { task_id, kind, description, notify, pgid }) => {
                        // It may outlive the worker (its own process group):
                        // stop it, since nothing reads its output any more.
                        stop_orphan(pgid, &description);
                        let record = BackgroundTask { task_id, kind, description, state: "lost".into(), notify, ..Default::default() };
                        let notice = notify.then(|| task_notice(&record, ""));
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
                    self.writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: false }));
                    match done {
                        Ok(Some(conv)) => self.conv = Some(conv),
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
            journal: self.journal.clone(),
            link: self.link.clone(),
            hooks: self.session_tools.hooks(),
            secrets: self.session_tools.secrets().clone(),
            publisher: self.session_tools.publisher(),
            memory: self.session_tools.memory(),
            notices: self.session_tools.notices(),
            head: self.session_tools.head(),
        };
        let (writer, link, tools, session_tools, inbox, c) =
            (self.writer.clone(), self.link.clone(), self.tools.clone(), self.session_tools.clone(), self.inbox.clone(), cancel.clone());
        writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true }));

        let claude_code = self.started.tool_profile == crate::claude_code::PROFILE;
        let seed = if claude_code { self.spec.seed.clone() } else { Vec::new() };
        let checkpoints = self.checkpoints.clone();
        let remote_session = !self.started.ssh_target.is_empty();
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
        let task = tokio::spawn(async move {
            let sink: Arc<dyn EventSink> = writer.clone();
            // Secrets for this turn's commands (values only in memory).
            if !secret_names.is_empty() {
                let (values, errors) = fetch_secrets(&link, &writer.session_id, &secret_names).await;
                for e in errors {
                    warn!(session = %writer.session_id, "secret unavailable: {e}");
                }
                session_tools.set_secrets(values);
            }
            // The files as they are before this turn's messages, for
            // rewinding to before them. A failed snapshot does not stop the
            // turn; the rewind then cannot restore files.
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
            let result = if claude_code {
                let inputs = inbox.lock().unwrap().drain();
                // Claude Code keeps its own history: a side chat's context
                // goes in as text with the first question.
                let preamble = (turn == 1 && !seed.is_empty()).then(|| {
                    format!(
                        "<conversation-so-far>\n{}\n</conversation-so-far>\n\nThe user branched off the conversation above to ask:",
                        c6_agent::transcript_text(&seed)
                    )
                });
                crate::claude_code::run_turn(
                    turn,
                    &writer.session_id,
                    &session_tools.cwd(),
                    &cfg.model,
                    cfg.effort.as_deref(),
                    inputs,
                    preamble.as_deref(),
                    claude_settings.as_deref(),
                    &sink,
                    &c,
                )
                .await
                .map(drop)
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
    link: &HeadLink,
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

    let client: Arc<dyn c6_llm::ModelClient> =
        match link.credential(connection_id).await.and_then(|g| Ok(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers)?)) {
            Ok(c) => Arc::from(c),
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
    session_tools.set_subagents(Some(Arc::new(crate::subagent::Runner::new(sub, cfg, client.clone(), tools))));
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
    tools.push(Arc::new(c6_tools::knowledge::GrantRequest::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::knowledge::CommonsPublish::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::kits::KitRun::new(profile == ToolProfile::Claude)));
    tools.push(Arc::new(c6_tools::kits::KitShell::new(profile == ToolProfile::Claude)));
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
    // Named agents (from the head's settings) become Task's subagent types.
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
    let agents: Vec<c6_tools::AgentDef> = applicable(all, |a| &a.name, |a| &a.applies_to, labels)
        .into_iter()
        .map(|a| c6_tools::AgentDef {
            name: a.name,
            description: a.description,
            tools: a.tools.split(',').map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect(),
            model: a.model,
            prompt: a.prompt,
        })
        .collect();
    if !agents.is_empty()
        && let Some(i) = tools.iter().position(|t| c6_tools::Task::is_task(&t.spec().name))
    {
        tools[i] = Arc::new(c6_tools::Task::with_agents(profile == ToolProfile::Claude, agents));
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
fn task_notice(t: &BackgroundTask, output: &str) -> String {
    let secs = t.elapsed_ms / 1000;
    let elapsed = if secs >= 3600 {
        format!("{}h {}m", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    };
    let agent = t.kind == "agent";
    let status = match t.state.as_str() {
        "finished" if agent && t.exit_code == 0 => "finished".to_owned(),
        "finished" if agent => "failed".to_owned(),
        "finished" => format!("exited with code {}", t.exit_code),
        "stopped_by_user" => "stopped by the user".to_owned(),
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
