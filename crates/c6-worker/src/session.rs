//! Session runtime: one actor task per active session, each the single writer
//! of its event log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, build_system_prompt, history_from_events, run_turn};
use c6_llm::{Message, Provider, Role, ToolProfile, model_info};
use c6_proto::{
    Checkpoint, CreateSession, EphemeralEvent, ErrorEvent, ModelSpec, ModelSwitched, Rewound, SessionActivity, SessionCommand,
    SessionEvent, SessionStarted, TurnEndReason, TurnEnded, TurnStarted, ephemeral_event, session_command, session_event,
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

enum ActorMsg {
    /// Rewind the conversation (after interrupting a running turn).
    Rewind {
        command_id: String,
        to_seq: u64,
        restore_files: bool,
    },
    /// An input, possibly switching the session's model from it on.
    Input(PendingInput, Option<ModelSpec>),
    Interrupt,
}

impl SessionManager {
    pub fn new(journal: Arc<Journal>, link: Arc<HeadLink>, worktrees: PathBuf) -> Self {
        let state_dir = worktrees.parent().map(Path::to_path_buf).unwrap_or_else(|| worktrees.clone());
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
                let pending =
                    PendingInput { command_id: cmd.command_id, text: input.text, attachments: input.attachments, ..Default::default() };
                let _ = actor.send(ActorMsg::Input(pending, input.model));
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
            None => Err("unsupported command".into()),
        }
    }

    async fn create(&self, session_id: &str, command_id: &str, spec: CreateSession) -> Result<(), String> {
        let cwd = crate::expand_home(&spec.cwd);
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
        let system_prompt = build_system_prompt(&spec.base_prompt, &cwd, &date);
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
            let built = build_tools(&spec, &cwd, profile).await;
            setup_errors = built.errors;
            let mut prompt = system_prompt;
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
        Ok(Some(self.spawn(session_id, spec, started, conv, next_seq, turns, None).await))
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
        let tools = match tools {
            Some(t) => t,
            None if started.tool_profile == crate::claude_code::PROFILE => Vec::new(),
            None => build_tools(&spec, std::path::Path::new(&started.cwd), ToolProfile::parse(&started.tool_profile)).await.tools,
        };
        let started_cwd = started.cwd.clone();
        let actor = Actor {
            spec,
            session_tools: Arc::new(SessionTools::new(PathBuf::from(&started.cwd))),
            tools,
            journal: self.journal.clone(),
            started,
            conv: Some(conv),
            turns,
            writer,
            link: self.link.clone(),
            inbox: Arc::default(),
            checkpoints: Arc::new(Checkpoints::new(&self.state_dir, session_id, Path::new(&started_cwd))),
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
    /// `None` while a turn task holds it.
    conv: Option<Conversation>,
    turns: u64,
    writer: Arc<Writer>,
    link: Arc<HeadLink>,
    journal: Arc<Journal>,
    tools: Vec<Arc<dyn Tool>>,
    inbox: Arc<Mutex<Inbox>>,
    checkpoints: Arc<Checkpoints>,
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
                    Some(ActorMsg::Input(mut input, model)) => {
                        // Expand a slash command and attach the files it
                        // mentions, as they are now.
                        let cwd = Path::new(&self.started.cwd);
                        crate::commands::expand(cwd, &mut input).await;
                        crate::mentions::resolve(cwd, &mut input);
                        self.inbox.lock().unwrap().push(input, model)
                    }
                    Some(ActorMsg::Interrupt) => {
                        if let Some((_, cancel)) = &running { cancel.cancel(); }
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
        };
        let mut conv = self.conv.take().expect("no turn is running");
        let sub = crate::subagent::Parent {
            session_id: self.writer.session_id.clone(),
            spec: self.spec.clone(),
            started: self.started.clone(),
            journal: self.journal.clone(),
            link: self.link.clone(),
        };
        let (writer, link, tools, session_tools, inbox, c) =
            (self.writer.clone(), self.link.clone(), self.tools.clone(), self.session_tools.clone(), self.inbox.clone(), cancel.clone());
        writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true }));

        let claude_code = self.started.tool_profile == crate::claude_code::PROFILE;
        let seed = if claude_code { self.spec.seed.clone() } else { Vec::new() };
        let checkpoints = self.checkpoints.clone();
        let task = tokio::spawn(async move {
            let sink: Arc<dyn EventSink> = writer.clone();
            // The files as they are before this turn's messages, for
            // rewinding to before them. A failed snapshot does not stop the
            // turn; the rewind then cannot restore files.
            let checkpoint = match checkpoints.snapshot().await {
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

pub(crate) async fn build_tools(spec: &CreateSession, cwd: &std::path::Path, profile: ToolProfile) -> BuiltTools {
    let mut tools = c6_tools::tools_for(profile);
    let mut errors = Vec::new();
    // Named agents (.claude/agents) become Task's subagent types.
    let agents = c6_tools::AgentDef::discover(cwd);
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
    }
    let mut skills: Vec<Skill> = serde_json::from_str::<Vec<HeadSkill>>(&spec.skills_json)
        .unwrap_or_default()
        .into_iter()
        .map(|s| Skill { name: s.name, description: s.description, body: s.body, dir: None })
        .collect();
    // Repo skills override head skills of the same name.
    for s in Skill::discover(cwd) {
        skills.retain(|k| k.name != s.name);
        skills.push(s);
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    if !skills.is_empty() {
        tools.push(Arc::new(SkillTool::new(profile == ToolProfile::Claude, Arc::new(skills.clone()))));
    }

    let servers: Vec<c6_mcp::ServerConfig> = serde_json::from_str(&spec.mcp_servers_json).unwrap_or_default();
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
