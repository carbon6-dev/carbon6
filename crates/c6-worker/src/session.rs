//! Session runtime: one actor task per active session, each the single writer
//! of its event log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, build_system_prompt, history_from_events, run_turn};
use c6_llm::{Block, Message, Provider, Role, ToolProfile, model_info};
use c6_proto::{
    CreateSession, EphemeralEvent, ErrorEvent, SessionActivity, SessionCommand, SessionEvent, SessionStarted, TurnEndReason, TurnEnded,
    TurnStarted, UserMessage, ephemeral_event, session_command, session_event,
};
use c6_tools::{SessionTools, Tool};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::journal::Journal;
use crate::link::HeadLink;

pub struct SessionManager {
    journal: Arc<Journal>,
    link: Arc<HeadLink>,
    actors: Mutex<HashMap<String, mpsc::UnboundedSender<ActorMsg>>>,
}

enum ActorMsg {
    Input(PendingInput),
    Interrupt,
}

impl SessionManager {
    pub fn new(journal: Arc<Journal>, link: Arc<HeadLink>) -> Self {
        Self { journal, link, actors: Mutex::default() }
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
                let _ = actor.send(ActorMsg::Input(PendingInput { command_id: cmd.command_id, text: input.text }));
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
        let cwd = expand_home(&spec.cwd);
        if !cwd.is_dir() {
            return Err(format!("working directory {} does not exist on this worker", cwd.display()));
        }
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let date = time::OffsetDateTime::now_utc().date().to_string();
        let system_prompt = build_system_prompt(&spec.base_prompt, &cwd, &date);
        let model = spec.model.clone().unwrap_or_default();
        let provider = Provider::from_kind(&model.connection_kind).unwrap_or(Provider::OpenAiChat);
        let profile = model_info(provider, &model.model).tool_profile;
        let started = SessionStarted {
            command_id: command_id.to_owned(),
            model: spec.model.clone(),
            cwd: cwd.display().to_string(),
            system_prompt,
            tools: c6_tools::tools_for(profile).iter().map(|t| t.spec().name).collect(),
            tool_profile: profile.as_str().into(),
        };
        let ev = SessionEvent { seq: 1, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Started(started.clone())) };
        self.journal.create_session(session_id, &spec, &ev).await.map_err(|e| format!("worker error: {e:#}"))?;
        info!(session = %session_id, cwd = %cwd.display(), profile = profile.as_str(), "session created");
        self.spawn(session_id, spec, started, Conversation { history: Vec::new(), context_tokens: 0 }, 2, 0);
        Ok(())
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
        let next_seq = events.last().map(|e| e.seq + 1).unwrap_or(1);
        let turns = events.iter().filter(|e| matches!(e.kind, Some(session_event::Kind::TurnStarted(_)))).count() as u64;
        let history = history_from_events(&events);
        // The last request's size since the last compaction, for the next
        // compaction decision.
        let context_tokens = events
            .iter()
            .rev()
            .take_while(|e| !matches!(e.kind, Some(session_event::Kind::Compaction(_))))
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::Usage(u)) => Some(u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens),
                _ => None,
            })
            .unwrap_or(0);
        Ok(Some(self.spawn(session_id, spec, started, Conversation { history, context_tokens }, next_seq, turns)))
    }

    fn spawn(
        &self,
        session_id: &str,
        spec: CreateSession,
        started: SessionStarted,
        conv: Conversation,
        next_seq: u64,
        turns: u64,
    ) -> mpsc::UnboundedSender<ActorMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = Arc::new(Writer {
            session_id: session_id.to_owned(),
            journal: self.journal.clone(),
            link: self.link.clone(),
            next_seq: tokio::sync::Mutex::new(next_seq),
        });
        let actor = Actor {
            spec,
            session_tools: Arc::new(SessionTools::new(PathBuf::from(&started.cwd))),
            tools: c6_tools::tools_for(ToolProfile::parse(&started.tool_profile)),
            started,
            conv: Some(conv),
            turns,
            writer,
            link: self.link.clone(),
            inbox: Arc::default(),
        };
        tokio::spawn(actor.run(rx));
        self.actors.lock().unwrap().insert(session_id.to_owned(), tx.clone());
        tx
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            dirs::home_dir().map(|h| h.join(rest.trim_start_matches('/'))).unwrap_or_else(|| PathBuf::from(path))
        }
        _ => PathBuf::from(path),
    }
}

/// Inputs waiting for the next model-request boundary.
#[derive(Default)]
struct Inbox {
    queue: VecDeque<PendingInput>,
    /// Every command id ever queued here, so redeliveries are ignored even
    /// while an input is being recorded.
    seen: HashSet<String>,
}

impl Inbox {
    fn push(&mut self, input: PendingInput) {
        if self.seen.insert(input.command_id.clone()) {
            self.queue.push_back(input);
        }
    }

    fn drain(&mut self) -> Vec<PendingInput> {
        self.queue.drain(..).collect()
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
    tools: Vec<Arc<dyn Tool>>,
    inbox: Arc<Mutex<Inbox>>,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ActorMsg>) {
        let mut running: Option<(tokio::task::JoinHandle<Conversation>, CancellationToken)> = None;
        loop {
            if running.is_none() && !self.inbox.lock().unwrap().queue.is_empty() {
                running = Some(self.start_turn());
            }
            tokio::select! {
                msg = rx.recv() => match msg {
                    None => return,
                    Some(ActorMsg::Input(input)) => self.inbox.lock().unwrap().push(input),
                    Some(ActorMsg::Interrupt) => {
                        if let Some((_, cancel)) = &running { cancel.cancel(); }
                    }
                },
                done = async { (&mut running.as_mut().unwrap().0).await }, if running.is_some() => {
                    running = None;
                    match done {
                        Ok(conv) => self.conv = Some(conv),
                        Err(e) => {
                            // Rebuild from the log on the next turn.
                            error!(session = %self.writer.session_id, "turn task panicked: {e}");
                            return;
                        }
                    }
                    self.writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: false }));
                }
            }
        }
    }

    fn start_turn(&mut self) -> (tokio::task::JoinHandle<Conversation>, CancellationToken) {
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
            effort: Some(model.effort.clone()).filter(|e| !e.is_empty()).or_else(|| info.default_effort.map(str::to_owned)),
            context_window: info.context_window,
        };
        let mut conv = self.conv.take().expect("no turn is running");
        let (writer, link, tools, session_tools, inbox, c) =
            (self.writer.clone(), self.link.clone(), self.tools.clone(), self.session_tools.clone(), self.inbox.clone(), cancel.clone());
        writer.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true }));

        let task = tokio::spawn(async move {
            let sink: Arc<dyn EventSink> = writer.clone();
            if let Err(e) =
                run_session_turn(turn, &cfg, &model.connection_id, &link, &tools, &session_tools, &sink, &mut conv, &inbox, &c).await
            {
                // Only journal writes fail here. The in-memory history may now
                // differ from the log; a worker restart rebuilds it from the log.
                error!(session = %writer.session_id, "turn failed: {e:#}");
            }
            conv
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
) -> anyhow::Result<()> {
    let first = inbox.lock().unwrap().drain();
    let mut blocks = Vec::new();
    for input in first {
        sink.append(session_event::Kind::User(UserMessage { command_id: input.command_id, text: input.text.clone() })).await?;
        blocks.push(Block::Text(input.text));
    }
    match conv.history.last_mut() {
        Some(m) if m.role == Role::User => m.blocks.extend(blocks),
        _ if !blocks.is_empty() => conv.history.push(Message { role: Role::User, blocks }),
        _ => {}
    }

    let client =
        match link.credential(connection_id).await.and_then(|g| Ok(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers)?)) {
            Ok(c) => c,
            Err(e) => {
                sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;
                sink.append(session_event::Kind::Error(ErrorEvent { message: format!("cannot reach the model: {e:#}") })).await?;
                sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
                return Ok(());
            }
        };
    let mut pending = || inbox.lock().unwrap().drain();
    run_turn(turn, cfg, client.as_ref(), tools, session_tools, sink, conv, &mut pending, cancel).await?;
    Ok(())
}

/// The single writer of one session's log.
struct Writer {
    session_id: String,
    journal: Arc<Journal>,
    link: Arc<HeadLink>,
    next_seq: tokio::sync::Mutex<u64>,
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
