//! The carbon6 assistant (docs/assistant.md): an agent in the head that can
//! do whatever the UI does, through the operation catalog.
//!
//! Its conversations are ordinary sessions on a worker that is the head
//! itself (role `head`): commands reach it through the same queue and
//! registry as a real worker's, and its events go through the same ingest,
//! so storage, live streaming, approvals, notifications and the inbox all
//! work as for any session. It needs no worker, so it works before any is
//! attached.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, context_tokens_from_events, history_from_events, run_turn};
use c6_api as api;
use c6_core::{WorkerId, now_ms};
use c6_llm::{Message, Provider, Role, ToolSpec, model_info};
use c6_proto::{
    ErrorEvent, HeadMsg, ModelSwitched, SessionActivity, SessionCommand, SessionEvent, SessionEventBatch, SessionStarted, TurnEndReason,
    TurnEnded, TurnStarted, ephemeral_event, head_msg, session_command, session_event,
};
use c6_tools::{Concurrency, SessionTools, Tool, ToolCtx, ToolOutput};
use prost::Message as _;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::catalog::{self, Effect, Input, Op, Via};
use crate::{HeadState, bad_request};

pub const SETTING: &str = "assistant";
/// The label every assistant conversation has.
pub const LABEL: &str = "assistant";
const NAME: &str = "head";
/// How long a question or an approval waits for you.
const WAIT: Duration = Duration::from_secs(3600);
/// Per model request, not per turn: tools and approvals have their own
/// lifetimes. Includes transport retries and ignores SSE keepalive bytes.
const MODEL_LIMITS: c6_llm::limit::Limits = c6_llm::limit::Limits { total: Duration::from_secs(600), idle: Duration::from_secs(300) };

/// Marks a request the head makes to itself (the assistant's calls): it is
/// you, already signed in, limited to the realm the conversation works in
/// (root, or none: every realm).
#[derive(Clone, Default)]
pub struct InternalCall {
    pub realm: Option<String>,
    /// The console's assistant, acting in `realm` with your approval.
    pub console: bool,
}

#[derive(Default)]
pub struct Assistant {
    head_id: OnceLock<WorkerId>,
    router: OnceLock<axum::Router>,
    actors: Mutex<HashMap<String, mpsc::UnboundedSender<SessionCommand>>>,
}

impl Assistant {
    pub fn head_id(&self) -> Option<WorkerId> {
        self.head_id.get().copied()
    }

    /// The head's own API, for requests it makes to itself.
    pub fn router(&self) -> Option<axum::Router> {
        self.router.get().cloned()
    }
}

/// Starts the head's own worker connection (after the router is built).
pub async fn start(st: Arc<HeadState>, router: axum::Router) -> anyhow::Result<()> {
    let _ = st.assistant.router.set(router);
    let id = head_worker(&st).await?;
    let _ = st.assistant.head_id.set(id);
    if let Err(e) = recover(&st, id).await {
        warn!("recovering assistant turns after head restart: {e:#}");
    }
    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<Result<HeadMsg, tonic::Status>>(256);
        let guard = st.registry.attach(id, now_ms(), tx, vec!["assistant".into()], "head");
        let mut delivered: i64 = 0;
        guard.deliver.notify_one();
        loop {
            tokio::select! {
                _ = guard.cancel.cancelled() => break,
                _ = guard.deliver.notified() => {
                    loop {
                        let rows = match st.store.pending_commands(id, delivered).await {
                            Ok(r) => r,
                            Err(e) => {
                                warn!("assistant: reading commands: {e:#}");
                                break;
                            }
                        };
                        if rows.is_empty() {
                            break;
                        }
                        for row in rows {
                            match SessionCommand::decode(row.data.as_slice()) {
                                Ok(cmd) => dispatch(&st, cmd),
                                Err(e) => warn!("assistant: a bad command: {e}"),
                            }
                            delivered = row.seq;
                        }
                    }
                }
                m = rx.recv() => match m {
                    Some(Ok(HeadMsg { msg: Some(head_msg::Msg::Command(cmd)) })) => dispatch(&st, cmd),
                    // Files, git and terminals are on workers.
                    Some(Ok(HeadMsg { msg: Some(head_msg::Msg::RpcRequest(req)) })) => guard.rpc_response(c6_proto::RpcResponse {
                        id: req.id,
                        result_json: Vec::new(),
                        error: "the assistant's conversations have no files, git or terminals (they are on workers)".into(),
                    }),
                    Some(_) => {}
                    None => break,
                },
            }
        }
    });
    Ok(())
}

/// Unlike a worker, the head has no session journal or surviving actor to
/// finish a turn after restart. Close its abandoned turns before commands
/// can start new actors. Never replay their model requests or tools.
async fn recover(st: &Arc<HeadState>, head_id: WorkerId) -> anyhow::Result<()> {
    let sessions: Vec<(String, i64)> =
        sqlx::query_as("SELECT id, last_seq FROM sessions WHERE worker_id = ? AND status = 'running' ORDER BY id")
            .bind(head_id.to_string())
            .fetch_all(&st.store.pool)
            .await?;
    for (session_id, last_seq) in sessions {
        let result = async {
            let events = st.store.events_after(&session_id, 0, i64::MAX).await?;
            let turn = events.iter().rev().find_map(|e| match &e.kind {
                Some(session_event::Kind::TurnStarted(t)) => Some(t.turn),
                _ => None,
            });
            let Some(turn) = turn else {
                warn!(session = %session_id, "running assistant has no turn_started event; cannot recover");
                return anyhow::Ok(());
            };
            warn!(session = %session_id, turn, "closing assistant turn interrupted by head restart");
            let sink = Sink { st: st.clone(), session_id: session_id.clone(), head_id, seq: tokio::sync::Mutex::new(last_seq as u64) };
            let mut seen = std::collections::HashSet::new();
            for event in events.iter().rev() {
                let settled = match &event.kind {
                    Some(session_event::Kind::TurnStarted(_)) => break,
                    Some(session_event::Kind::Approval(a)) if seen.insert(("approval", a.call_id.clone())) && a.state == "requested" => {
                        let mut a = a.clone();
                        a.state = "expired".into();
                        Some(session_event::Kind::Approval(a))
                    }
                    Some(session_event::Kind::Question(q)) if seen.insert(("question", q.call_id.clone())) && q.state == "asked" => {
                        let mut q = q.clone();
                        q.state = "cancelled".into();
                        Some(session_event::Kind::Question(q))
                    }
                    _ => None,
                };
                if let Some(event) = settled {
                    sink.append(event).await?;
                }
            }
            sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::WorkerRestarted as i32 })).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!(session = %session_id, "recovering assistant turn after head restart: {e:#}");
        }
    }
    Ok(())
}

/// The head's own worker row (made once).
async fn head_worker(st: &HeadState) -> anyhow::Result<WorkerId> {
    if let Some(id) =
        sqlx::query_scalar::<_, String>("SELECT id FROM workers WHERE role = 'head' LIMIT 1").fetch_optional(&st.store.pool).await?
    {
        return Ok(id.parse()?);
    }
    let id = WorkerId(uuid::Uuid::now_v7());
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "head".into());
    st.store
        .insert_worker(
            &crate::store::NewWorker {
                id,
                name: NAME,
                hostname: &host,
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
                version: env!("CARGO_PKG_VERSION"),
                cert_serial: "-",
            },
            "",
            now_ms(),
        )
        .await?;
    st.store.set_worker_role(id, "head").await?;
    Ok(id)
}

fn dispatch(st: &Arc<HeadState>, cmd: SessionCommand) {
    let sid = cmd.session_id.clone();
    let mut actors = st.assistant.actors.lock().unwrap();
    if let Some(tx) = actors.get(&sid)
        && tx.send(cmd.clone()).is_ok()
    {
        return;
    }
    // Only creating or messaging starts a conversation's actor.
    if !matches!(cmd.kind, Some(session_command::Kind::Create(_) | session_command::Kind::Input(_))) {
        return;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = tx.send(cmd);
    actors.insert(sid.clone(), tx);
    let st = st.clone();
    tokio::spawn(async move {
        if let Err(e) = Actor::run(st.clone(), sid.clone(), rx).await {
            warn!(session = %sid, "assistant: {e:#}");
        }
        st.assistant.actors.lock().unwrap().remove(&sid);
    });
}

/// What a question or an approval waits for.
enum Reply {
    Approve(bool),
    Answer {
        values: serde_json::Map<String, Value>,
        cancelled: bool,
    },
    /// A subagent the conversation started (`c6_delegate`) ended its turn.
    Delegation(c6_proto::DelegationEnded),
}

type Waiters = Arc<Mutex<HashMap<String, oneshot::Sender<Reply>>>>;

/// Writes a conversation's events through the head's ingest.
struct Sink {
    st: Arc<HeadState>,
    session_id: String,
    head_id: WorkerId,
    seq: tokio::sync::Mutex<u64>,
}

#[async_trait]
impl EventSink for Sink {
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
        let mut seq = self.seq.lock().await;
        *seq += 1;
        let ev = SessionEvent { seq: *seq, ts_ms: now_ms(), kind: Some(kind) };
        crate::sessions::ingest(&self.st, self.head_id, SessionEventBatch { session_id: self.session_id.clone(), events: vec![ev] })
            .await?;
        Ok(*seq)
    }

    fn ephemeral(&self, kind: ephemeral_event::Kind) {
        if let Some(msg) = crate::convert::ephemeral(&kind) {
            self.st.hub.publish(&self.session_id, msg);
        }
    }
}

struct Actor {
    st: Arc<HeadState>,
    session_id: String,
    sink: Arc<Sink>,
    model: c6_proto::ModelSpec,
    system_prompt: String,
    started: bool,
    applied: std::collections::HashSet<String>,
    conv: Option<Conversation>,
    turns: u64,
    inbox: Arc<Mutex<Vec<PendingInput>>>,
    env: Arc<Env>,
    /// What the model is told when the conversation is compacted.
    compact: c6_agent::CompactPrompts,
    /// Its Claude Code process, on a Claude Code connection (docs/ask.md §3).
    claude: Arc<tokio::sync::Mutex<c6_worker::hosted::HostedClaude>>,
}

impl Actor {
    async fn run(st: Arc<HeadState>, session_id: String, mut rx: mpsc::UnboundedReceiver<SessionCommand>) -> anyhow::Result<()> {
        let head_id = st.assistant.head_id().ok_or_else(|| anyhow::anyhow!("the assistant is not started"))?;
        let row = st.store.session(&session_id).await?.ok_or_else(|| anyhow::anyhow!("no session {session_id}"))?;
        // What the log says so far (after a restart of the head).
        let events = st.store.events_after(&session_id, 0, i64::MAX).await?;
        let mut model = c6_proto::ModelSpec { connection_id: row.connection_id.clone(), model: row.model.clone(), ..Default::default() };
        let mut system_prompt = String::new();
        let mut started = false;
        let mut applied = std::collections::HashSet::new();
        let mut turns = 0;
        for e in &events {
            match &e.kind {
                Some(session_event::Kind::Started(s)) => {
                    started = true;
                    system_prompt = s.system_prompt.clone();
                    if let Some(m) = &s.model {
                        model = m.clone();
                    }
                    applied.insert(s.command_id.clone());
                }
                Some(session_event::Kind::ModelSwitched(m)) => {
                    if let Some(m) = &m.model {
                        model = m.clone();
                    }
                }
                Some(session_event::Kind::User(u)) => {
                    applied.insert(u.command_id.clone());
                }
                Some(session_event::Kind::TurnStarted(_)) => turns += 1,
                _ => {}
            }
        }
        let last_seq = events.last().map(|e| e.seq).unwrap_or(0).max(row.last_seq as u64);
        let sink = Arc::new(Sink { st: st.clone(), session_id: session_id.clone(), head_id, seq: tokio::sync::Mutex::new(last_seq) });
        let env = Arc::new(Env {
            st: st.clone(),
            session_id: session_id.clone(),
            sink: sink.clone(),
            waiters: Waiters::default(),
            vault: Mutex::default(),
            labels: row.all_labels(),
            realm: row.realm.clone(),
        });
        let conv = Conversation { history: history_from_events(&events), context_tokens: context_tokens_from_events(&events) };
        let mut actor = Actor {
            compact: crate::prompts::compact(&st, &row.realm).await,
            st,
            session_id,
            sink,
            model,
            system_prompt,
            started,
            applied,
            conv: Some(conv),
            turns,
            inbox: Arc::default(),
            env,
            claude: Arc::default(),
        };
        let mut running: Option<(tokio::task::JoinHandle<Option<Conversation>>, CancellationToken)> = None;
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    actor.handle(cmd, &mut running).await?;
                }
                done = async { (&mut running.as_mut().unwrap().0).await }, if running.is_some() => {
                    running = None;
                    actor.sink.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: false, ..Default::default() }));
                    actor.conv = Some(match done {
                        Ok(Some(c)) => c,
                        _ => {
                            let events = actor.st.store.events_after(&actor.session_id, 0, i64::MAX).await?;
                            Conversation { history: history_from_events(&events), context_tokens: context_tokens_from_events(&events) }
                        },
                    });
                    if !actor.inbox.lock().unwrap().is_empty() {
                        running = Some(actor.start_turn());
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        cmd: SessionCommand,
        running: &mut Option<(tokio::task::JoinHandle<Option<Conversation>>, CancellationToken)>,
    ) -> anyhow::Result<()> {
        match cmd.kind {
            Some(session_command::Kind::Create(spec)) => {
                if self.started {
                    return Ok(());
                }
                self.model = spec.model.clone().unwrap_or_default();
                self.system_prompt = spec.base_prompt.clone();
                let context_window = crate::pools::context_window(&self.st, &self.model, &self.env.realm).await;
                self.sink
                    .append(session_event::Kind::Started(SessionStarted {
                        command_id: cmd.command_id.clone(),
                        model: Some(self.model.clone()),
                        cwd: String::new(),
                        system_prompt: self.system_prompt.clone(),
                        tools: self.env.tools(Vec::new()).iter().map(|t| t.spec().name).collect(),
                        tool_profile: "assistant".into(),
                        context_window,
                        title: String::new(),
                        ..Default::default()
                    }))
                    .await?;
                self.started = true;
                self.applied.insert(cmd.command_id);
            }
            Some(session_command::Kind::Input(input)) => {
                if !self.applied.insert(cmd.command_id.clone()) {
                    return Ok(());
                }
                if let Some(m) = input.model.filter(|m| !m.model.is_empty()) {
                    let context_window = crate::pools::context_window(&self.st, &m, &self.env.realm).await;
                    self.sink
                        .append(session_event::Kind::ModelSwitched(ModelSwitched { model: Some(m.clone()), context_window, handoff: None }))
                        .await?;
                    if let Some(conv) = &mut self.conv {
                        conv.history = c6_agent::portable_messages(std::mem::take(&mut conv.history));
                    }
                    self.model = m;
                }
                // What you @-mentioned, read with your access (docs/ask.md §4).
                let prompt = with_mentions(&self.st, &self.env.realm, &input.text).await;
                // Send now: the running turn stops, and this starts the next.
                if input.send_now
                    && let Some((_, cancel)) = running
                {
                    cancel.cancel();
                }
                self.inbox.lock().unwrap().push(PendingInput {
                    command_id: cmd.command_id,
                    text: input.text,
                    attachments: input.attachments,
                    prompt,
                    ..Default::default()
                });
                if running.is_none() {
                    *running = Some(self.start_turn());
                }
            }
            Some(session_command::Kind::Interrupt(_)) => {
                if let Some((_, cancel)) = running {
                    cancel.cancel();
                }
            }
            Some(session_command::Kind::Approve(a)) => {
                if let Some(w) = self.env.waiters.lock().unwrap().remove(&a.call_id) {
                    let _ = w.send(Reply::Approve(a.allow));
                }
            }
            Some(session_command::Kind::Answer(a)) => {
                if let Some(w) = self.env.waiters.lock().unwrap().remove(&a.call_id) {
                    let values = serde_json::from_str(&a.values_json).unwrap_or_default();
                    let _ = w.send(Reply::Answer { values, cancelled: a.cancelled });
                }
            }
            // A subagent it started ended a turn: the call waiting on it
            // gets the report; with nobody waiting (the head restarted), the
            // report starts a turn so it isn't lost (docs/ask.md §2).
            Some(session_command::Kind::DelegationEnded(d)) => {
                if !self.applied.insert(cmd.command_id.clone()) {
                    return Ok(());
                }
                let waiter = self.env.waiters.lock().unwrap().remove(&d.call_id);
                match waiter {
                    Some(w) => {
                        let _ = w.send(Reply::Delegation(d));
                    }
                    None if !d.call_id.is_empty() => {
                        self.inbox.lock().unwrap().push(PendingInput {
                            command_id: cmd.command_id,
                            text: report_text(&d),
                            command: "task-notification".into(),
                            ..Default::default()
                        });
                        if running.is_none() {
                            *running = Some(self.start_turn());
                        }
                    }
                    None => {}
                }
            }
            Some(_) => {
                // Rewinding, restoring and background tasks are for worker
                // sessions.
                let reject = c6_proto::CommandRejected {
                    command_id: cmd.command_id,
                    session_id: cmd.session_id,
                    reason: "assistant conversations cannot do that".into(),
                };
                crate::sessions::on_rejected(&self.st, self.sink.head_id, reject).await?;
            }
            None => {}
        }
        Ok(())
    }

    fn start_turn(&mut self) -> (tokio::task::JoinHandle<Option<Conversation>>, CancellationToken) {
        self.turns += 1;
        let turn = self.turns;
        let cancel = CancellationToken::new();
        let info = info_of(&self.model);
        let mut cfg = TurnConfig {
            model: self.model.model.clone(),
            system_prompt: self.system_prompt.clone(),
            max_output_tokens: None,
            cache_key: self.session_id.clone(),
            effort: Some(self.model.effort.clone()).filter(|e| !e.is_empty()).or_else(|| info.default_effort.map(str::to_owned)),
            context_window: info.context_window,
            web_search: false,
            compact: self.compact.clone(),
            compact_at: self.model.compact_at_percent,
        };
        let mut conv = self.conv.take().unwrap_or(Conversation { history: Vec::new(), context_tokens: 0 });
        let (st, sink, inbox, env, c, connection_id) =
            (self.st.clone(), self.sink.clone(), self.inbox.clone(), self.env.clone(), cancel.clone(), self.model.connection_id.clone());
        sink.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true, ..Default::default() }));
        let session_id = self.session_id.clone();
        let model = self.model.clone();
        if model.connection_kind == "claude_code" {
            let (claude, system_prompt) = (self.claude.clone(), self.system_prompt.clone());
            let task = tokio::spawn(async move {
                let result = claude_turn(&st, &env, &sink, &inbox, &claude, turn, &model, &system_prompt, &c).await;
                if let Err(e) = result {
                    warn!(session = %session_id, "assistant turn on Claude Code failed: {e:#}");
                }
                // Its conversation is rebuilt from the log.
                None
            });
            return (task, cancel);
        }
        let task = tokio::spawn(async move {
            cfg.context_window = crate::pools::context_window(&st, &model, &env.realm).await;
            let dyn_sink: Arc<dyn EventSink> = sink.clone();
            let result = async {
                let first: Vec<PendingInput> = std::mem::take(&mut *inbox.lock().unwrap());
                let mut blocks = Vec::new();
                for input in first {
                    dyn_sink.append(input.event()).await?;
                    blocks.extend(input.blocks());
                }
                match conv.history.last_mut() {
                    Some(m) if m.role == Role::User => m.blocks.extend(blocks),
                    _ if !blocks.is_empty() => conv.history.push(Message { role: Role::User, blocks }),
                    _ => {}
                }
                let title = st.store.session(&session_id).await.ok().flatten().map(|r| r.title).filter(|t| !t.is_empty());
                let origin = c6_llm::activity::Origin {
                    what: "assistant".into(),
                    about: title.unwrap_or_else(|| session_id.clone()),
                    connection: connection_id.clone(),
                    realm: env.realm.clone(),
                    via: String::new(),
                };
                // Through the pool, when its connection is one.
                let client = crate::pools::client(&st, &connection_id, &env.realm, origin).await.map_err(|e| format!("{e:#}"));
                let client = match client {
                    Ok(c) => c,
                    Err(e) => {
                        dyn_sink.append(session_event::Kind::TurnStarted(TurnStarted { turn, ..Default::default() })).await?;
                        dyn_sink.append(session_event::Kind::Error(ErrorEvent { message: format!("cannot reach the model: {e}") })).await?;
                        dyn_sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
                        return anyhow::Ok(());
                    }
                };
                let client = c6_llm::limit::bounded(client, MODEL_LIMITS);
                // Where it may delegate: the templates agents may start.
                let templates = crate::templates::for_subagents(&st, &env.realm, "").await;
                let tools = env.tools(templates);
                let session_tools = Arc::new(SessionTools::new(std::env::temp_dir()));
                session_tools.set_head(Some(Arc::new(HeadCalls { env: env.clone() })));
                let mut pending = || std::mem::take(&mut *inbox.lock().unwrap());
                run_turn(turn, &cfg, client.as_ref(), &tools, &session_tools, &dyn_sink, &mut conv, &mut pending, &c).await?;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => Some(conv),
                Err(e) => {
                    warn!(session = %sink.session_id, "assistant turn failed: {e:#}");
                    None
                }
            }
        });
        (task, cancel)
    }
}

/// What Claude Code is told about the conversation's MCP server; its
/// system prompt carries Ask's instructions.
const CLAUDE_INSTRUCTIONS: &str = "carbon6's operations for this conversation: c6_discover, c6_describe, c6_call, c6_ask, c6_delegate.";

/// A turn on Claude Code, run on the head's machine with Ask's tools only.
#[allow(clippy::too_many_arguments)]
async fn claude_turn(
    st: &Arc<HeadState>,
    env: &Arc<Env>,
    sink: &Arc<Sink>,
    inbox: &Arc<Mutex<Vec<PendingInput>>>,
    claude: &Arc<tokio::sync::Mutex<c6_worker::hosted::HostedClaude>>,
    turn: u64,
    model: &c6_proto::ModelSpec,
    system_prompt: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let inputs: Vec<PendingInput> = std::mem::take(&mut *inbox.lock().unwrap());
    let templates = crate::templates::for_subagents(st, &env.realm, "").await;
    let tools = env.tools(templates);
    let session_tools = Arc::new(SessionTools::new(std::env::temp_dir()));
    session_tools.set_head(Some(Arc::new(HeadCalls { env: env.clone() })));
    // A folder of its own, empty: no project's instructions are read.
    let cwd = st.data_dir.join("assistant").join(&env.session_id);
    std::fs::create_dir_all(&cwd)?;
    // Earlier turns (perhaps on another model), for a new Claude Code
    // conversation.
    let preamble = if turn > 1 {
        let events = st.store.events_after(&env.session_id, 0, i64::MAX).await?;
        Some(c6_agent::handoff_context(&events, "carbon6's assistant", c6_agent::HANDOFF_BUDGET))
    } else {
        None
    };
    let dyn_sink: Arc<dyn EventSink> = sink.clone();
    let log: Arc<dyn c6_worker::hosted::EventLog> = Arc::new(StoreLog { st: st.clone() });
    let mut c = claude.lock().await;
    let result = c
        .turn(c6_worker::hosted::HostedTurn {
            bin: st.claude_bin(),
            turn,
            inputs,
            session_id: &env.session_id,
            resume: turn > 1,
            preamble,
            model: &model.model,
            effort: Some(model.effort.clone()).filter(|e| !e.is_empty()),
            system_prompt: system_prompt.to_owned(),
            tools,
            instructions: CLAUDE_INSTRUCTIONS.into(),
            log,
            session_tools,
            cwd: &cwd,
            sink: &dyn_sink,
            cancel,
        })
        .await;
    drop(c);
    // Closed once idle with no turn since, as a worker's Claude Code is
    // (the next turn starts it again, resuming).
    let (idle, closer) = (c6_worker::claude_code::IDLE, claude.clone());
    tokio::spawn(async move {
        tokio::time::sleep(idle).await;
        let mut c = closer.lock().await;
        if c.last_turn() == turn {
            c.close_if_idle().await;
        }
    });
    result
}

/// The head's copy of a conversation's log, for the MCP bridge to match
/// Claude Code's tool calls.
struct StoreLog {
    st: Arc<HeadState>,
}

#[async_trait]
impl c6_worker::hosted::EventLog for StoreLog {
    async fn events(&self, session_id: &str) -> Vec<c6_proto::SessionEvent> {
        self.st.store.events_after(session_id, 0, i64::MAX).await.unwrap_or_default()
    }
}

fn info_of(m: &c6_proto::ModelSpec) -> c6_llm::ModelInfo {
    model_info(Provider::from_kind(&m.connection_kind).unwrap_or(Provider::OpenAiChat), &m.model)
}

/// What the assistant's tools share.
struct Env {
    st: Arc<HeadState>,
    session_id: String,
    sink: Arc<Sink>,
    waiters: Waiters,
    /// Secret answers, by placeholder: the model sees the placeholder,
    /// operations get the value.
    vault: Mutex<HashMap<String, String>>,
    labels: Vec<String>,
    /// The realm the conversation works in.
    realm: String,
}

impl Env {
    fn tools(self: &Arc<Self>, templates: Vec<api::SubagentTemplate>) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Discover),
            Arc::new(Describe),
            Arc::new(Call { env: self.clone() }),
            Arc::new(Ask { env: self.clone() }),
            Arc::new(Delegate { env: self.clone(), templates }),
        ];
        if crate::memex::ready(&self.st) {
            tools.push(Arc::new(c6_tools::memex::MemexSearch::new(false)));
            tools.push(Arc::new(c6_tools::memex::MemexGet::new(false)));
        }
        tools
    }

    /// Its labels as they are now (they can change while it runs).
    async fn labels_now(&self) -> Vec<String> {
        self.st.store.session(&self.session_id).await.ok().flatten().map(|r| r.all_labels()).unwrap_or_else(|| self.labels.clone())
    }

    /// Starts listening for your answer to `call_id`. Called before the
    /// question or approval is written, so an answer given the moment it
    /// shows (a quick click, a notification's link) is not missed.
    fn listen(&self, call_id: &str) -> oneshot::Receiver<Reply> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(call_id.to_owned(), tx);
        rx
    }

    /// Waits for the answer `listen` started on (or the turn's end).
    async fn wait(&self, call_id: &str, rx: oneshot::Receiver<Reply>, cancel: &CancellationToken) -> Option<Reply> {
        let r = tokio::select! {
            r = rx => r.ok(),
            _ = cancel.cancelled() => None,
            _ = tokio::time::sleep(WAIT) => None,
        };
        self.waiters.lock().unwrap().remove(call_id);
        r
    }

    /// Puts secret answers back where the model wrote their placeholders.
    fn unseal(&self, v: &mut Value) {
        let vault = self.vault.lock().unwrap();
        if vault.is_empty() {
            return;
        }
        fn walk(v: &mut Value, vault: &HashMap<String, String>) {
            match v {
                Value::String(s) => {
                    for (k, secret) in vault {
                        if s.contains(k.as_str()) {
                            *s = s.replace(k.as_str(), secret);
                        }
                    }
                }
                Value::Array(a) => a.iter_mut().for_each(|x| walk(x, vault)),
                Value::Object(o) => o.values_mut().for_each(|x| walk(x, vault)),
                _ => {}
            }
        }
        walk(v, &vault);
    }
}

/// The memex tools' way to the head: as this conversation (its scope).
struct HeadCalls {
    env: Arc<Env>,
}

#[async_trait]
impl c6_tools::HeadCaller for HeadCalls {
    async fn call(&self, method: &str, mut params: Value) -> Result<Value, String> {
        if !params.is_object() {
            params = json!({});
        }
        params["session_id"] = json!(self.env.session_id);
        crate::fabric::handle(&self.env.st, self.env.sink.head_id, method, params).await.map_err(|e| format!("{e:#}"))
    }
}

struct Discover;

#[async_trait]
impl Tool for Discover {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "c6_discover".into(),
            description: "Finds carbon6 operations (everything the UI can do) and documentation. With no query: the areas of carbon6 \
                          and how many operations each has; with an area: its operations; with a query: the best-matching operations \
                          and documentation sections. Use it before acting, to find the operation to call."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "what you want to do or know, in a few words"},
                "area": {"type": "string", "description": "rank this area first", "enum": catalog::areas().iter().map(|(a, _)| *a).collect::<Vec<_>>()}
            }, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, _ctx: &ToolCtx, input: Value) -> ToolOutput {
        ToolOutput::limited(
            catalog::discover_text(input["query"].as_str().unwrap_or(""), input["area"].as_str().filter(|a| !a.is_empty())),
            false,
        )
    }
}

struct Describe;

#[async_trait]
impl Tool for Describe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "c6_describe".into(),
            description: "An operation in full: its input schema (path parameters, then `body` for a JSON body, or query parameters), \
                          its effect (read, write, destructive, secret), which inputs take values another operation lists (choices), \
                          and whether it replaces a whole list. Read it before calling an operation you have not used."
                .into(),
            parameters: json!({"type": "object", "properties": {"op": {"type": "string"}}, "required": ["op"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, _ctx: &ToolCtx, input: Value) -> ToolOutput {
        let name = input["op"].as_str().unwrap_or("");
        match catalog::find(name) {
            Some(op) => ToolOutput::limited(serde_json::to_string_pretty(&catalog::describe(op)).unwrap_or_default(), false),
            None => ToolOutput::err(format!("no operation {name:?}; use c6_discover to find one")),
        }
    }
}

struct Call {
    env: Arc<Env>,
}

#[async_trait]
impl Tool for Call {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "c6_call".into(),
            description: "Runs a carbon6 operation, as the user. `input` follows the operation's schema (c6_describe). Destructive \
                          operations (deleting, revoking, sending, replacing a list with fewer entries) wait for the user's approval. \
                          Knowledge (mail, chat, calendars, contacts, photos) is within this conversation's memex scope, as for \
                          memex_search. \
                          Secrets an operation returns (tokens, codes) are shown to the user directly; you get a note that they were \
                          shown, never the value."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "op": {"type": "string"},
                "input": {"type": "object", "description": "path parameters, then body or query parameters"}
            }, "required": ["op"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let name = input["op"].as_str().unwrap_or("").to_owned();
        let Some(op) = catalog::find(&name) else { return ToolOutput::err(format!("no operation {name:?}; use c6_discover")) };
        let mut args = if input["input"].is_object() { input["input"].clone() } else { json!({}) };
        let shown = serde_json::to_string(&args).unwrap_or_default();
        self.env.unseal(&mut args);
        // Should you be asked first?
        let st = &self.env.st;
        // What it reads of the memex (mail, chat, calendars, contacts…) is
        // this conversation's scope, as for its memex tools.
        // As they are now: the conversation's switch for your mail, chat and
        // calendar can change between calls.
        let labels = self.env.labels_now().await;
        let scope = crate::memex::scope_for(st, &labels).await;
        let mut ask: Option<(String, String)> = None;
        // What the approval shows (else the operation and its input).
        let mut asked: Option<String> = None;
        // From root, what goes into a realm (a session, a message, a
        // trigger…) goes as that realm's, only once you approve the realm
        // and exactly what goes there.
        let mut elsewhere: Option<IntoRealm> = None;
        if let Some(why) = yours_to_approve(st, &name, &args).await {
            return ToolOutput::err(why);
        }
        if self.env.realm == crate::realms::ROOT {
            let unsealed = serde_json::to_string(&args).unwrap_or_default() != shown;
            match into_realm(st, op, &mut args, unsealed).await {
                Ok(Some(i)) => {
                    ask = Some((INTO_REALM.into(), i.said.clone()));
                    asked = Some(i.shown.clone());
                    elsewhere = Some(i);
                }
                Ok(None) => {}
                Err(e) => return ToolOutput::err(e),
            }
        }
        if elsewhere.is_some() {
            // Asked for above.
        } else if op.effect == Effect::Destructive {
            ask = Some(("assistant: destructive operations".into(), format!("{}: {}", op.name, op.summary)));
        } else if op.replaces.is_some() {
            match crate::memex::SCOPE.scope(scope.clone(), removed_entries(st, op, &args, &self.env.realm)).await {
                Ok(gone) if !gone.is_empty() => {
                    ask = Some(("assistant: removes entries".into(), format!("{} removes {}", op.name, gone.join(", "))));
                }
                Ok(_) => {}
                Err(e) => return ToolOutput::err(format!("could not read the current list: {e:#}")),
            }
        }
        match policy_for(st, &labels, &shown).await {
            Some(p) if p.action == "deny" => {
                return ToolOutput::err(format!(
                    "denied by policy {}{}",
                    p.name,
                    if p.message.is_empty() { String::new() } else { format!(": {}", p.message) }
                ));
            }
            Some(p) if ask.is_none() => ask = Some((p.name.clone(), p.message.clone())),
            _ => {}
        }
        if let Some((policy, message)) = ask {
            let input = asked.unwrap_or_else(|| crate::util_clip(&format!("{name} {shown}"), 4000));
            let approval = |state: &str| {
                session_event::Kind::Approval(c6_proto::Approval {
                    call_id: ctx.call_id.clone(),
                    state: state.into(),
                    tool: "c6_call".into(),
                    input: input.clone(),
                    policy: policy.clone(),
                    message: message.clone(),
                })
            };
            let rx = self.env.listen(&ctx.call_id);
            if let Err(e) = self.env.sink.append(approval("requested")).await {
                self.env.waiters.lock().unwrap().remove(&ctx.call_id);
                return ToolOutput::err(format!("could not ask for approval: {e:#}"));
            }
            let reply = self.env.wait(&ctx.call_id, rx, &ctx.cancel).await;
            let allowed = matches!(reply, Some(Reply::Approve(true)));
            let state = match reply {
                Some(Reply::Approve(true)) => "allowed",
                Some(_) => "denied",
                None => "expired",
            };
            let _ = self.env.sink.append(approval(state)).await;
            if !allowed {
                return ToolOutput::err(format!("the user did not approve {name} ({state}); do not try another way to do it"));
            }
        }
        let result = match &elsewhere {
            Some(i) => i.run(st, op, args, &self.env.session_id).await,
            None => crate::memex::SCOPE.scope(scope, execute(st, op, args, "assistant", Some(&self.env.realm))).await,
        };
        match result {
            Ok(mut v) => {
                // Secrets go to the user, not the model.
                let mut shown_secrets = Vec::new();
                for field in op.secret {
                    if let Some(value) = v.get(*field).and_then(|x| x.as_str()).map(str::to_owned) {
                        self.env.sink.ephemeral(ephemeral_event::Kind::Secret(c6_proto::SecretCard {
                            call_id: ctx.call_id.clone(),
                            title: secret_title(op, field),
                            value,
                            note: secret_note(op).into(),
                        }));
                        v[*field] = json!("[shown to the user]");
                        shown_secrets.push(*field);
                    }
                }
                let mut text = serde_json::to_string_pretty(&v).unwrap_or_default();
                if !shown_secrets.is_empty() {
                    text.push_str(&format!(
                        "\n\nThe {} was shown to the user in a card (once, not kept in this conversation). You do not have it; do not ask for it.",
                        shown_secrets.join(" and ")
                    ));
                }
                ToolOutput::limited(text, false)
            }
            Err(e) => ToolOutput::err(format!("{e:#}")),
        }
    }
}

fn secret_title(op: &Op, field: &str) -> String {
    match op.name {
        "workers.enrollments.create" => "Enrollment token".into(),
        "auth.tokens.create" => "API token".into(),
        "auth.codes.create" => "One-time sign-in code".into(),
        "triggers.rotate" => "Webhook URL".into(),
        _ => format!("{} ({field})", op.name),
    }
}

fn secret_note(op: &Op) -> &'static str {
    match op.name {
        "workers.enrollments.create" => "On the new machine: c6w enroll <token> then c6w run. One use, valid for an hour.",
        "auth.tokens.create" => "For the c6 CLI or a script (Authorization: Bearer <token>). Copy it now: it is not shown again.",
        "auth.codes.create" => "Enter it on the other device's sign-in page. Valid for a few minutes.",
        "triggers.rotate" => "The old URL no longer works.",
        _ => "Copy it now: it is not shown again.",
    }
}

/// The label of a session the assistant started from root (the console).
pub const STARTED_FROM_CONSOLE: &str = "started-by:console";
/// Who wrote what the assistant in root put in a realm (documents,
/// records, knowledge), and what started a workflow run there.
pub const BY_CONSOLE: &str = "console";
/// Whom a message the assistant in root sent a realm's session is from, as
/// the session reads it.
pub const FROM_CONSOLE: &str = "Ask in the console";
/// The most the assistant may put in another realm from root at once (a
/// first message, a message, an operation's input): what you approve is
/// shown in full, also in the decision.
const PROMPT_SHOWN: usize = 4000;
/// The approval's policy for whatever the assistant in root does in a realm.
const INTO_REALM: &str = "assistant: into another realm";
const SECRET_STAYS: &str = "a secret you were given cannot go into another realm";
/// Operations that put a file in a realm: the approval cannot show it.
const FILES: &[&str] = &["docs.import", "docs.add_file", "memex.upload", "memex.ingest"];

/// Something the assistant in root would do in a realm, once you approve it.
struct IntoRealm {
    realm: String,
    /// What and where, in words.
    said: String,
    /// What goes there, exactly as it goes.
    shown: String,
    /// A message for this session, from the console (else the operation
    /// runs as the realm).
    message: Option<(String, String)>,
}

impl IntoRealm {
    /// Does it as that realm's API token would, with none of root's, marked
    /// as the console's; a message goes as one from this conversation.
    async fn run(&self, st: &HeadState, op: &Op, args: Value, conversation: &str) -> anyhow::Result<Value> {
        if let Some((to, text)) = &self.message {
            return crate::sessions::message_from_console(st, to, text, conversation).await;
        }
        let v = execute_as(st, op, args, BY_CONSOLE, Some(&self.realm), true).await?;
        // A side chat says where it came from, as a session does.
        if op.name == "sessions.side_chat"
            && let Some(id) = v["id"].as_str()
            && let Some(row) = st.store.session(id).await?
        {
            let mut labels = row.user_labels();
            labels.push(STARTED_FROM_CONSOLE.to_owned());
            return Ok(serde_json::to_value(crate::sessions::set_labels(st, id, &labels).await?)?);
        }
        Ok(v)
    }
}

/// Where an operation the assistant in root asks you for finds its realm.
/// These are what carry text or data into a realm's agents or data, or
/// start something there (docs/assistant.md §Realms); administration and
/// housekeeping are root's own.
#[derive(Clone, Copy)]
enum Reach {
    /// The session path parameter `id` names.
    Session,
    /// The object path parameter `id` names: a row of `table`, or a record
    /// of `collection`.
    Object { table: &'static str, collection: &'static str, noun: &'static str },
    /// A template's: the realm named (body.realm), else the one that has it.
    Template,
    /// The realm path parameter `realm` names.
    PathRealm,
    /// The realm named (body.realm, else realm), else home: where root's
    /// new things, documents and knowledge go.
    Named,
}

fn reach(op: &str) -> Option<Reach> {
    use Reach::*;
    let record = |collection: &'static str, noun: &'static str| Object { table: "records", collection, noun };
    Some(match op {
        "sessions.send" | "sessions.side_chat" | "sessions.queue.edit" => Session,
        "templates.run" => Template,
        "apps.operations.call" => PathRealm,
        "triggers.update" | "triggers.run" => Object { table: "triggers", collection: "", noun: "trigger" },
        "decisions.answer" | "decisions.session" => Object { table: "decisions", collection: "", noun: "decision" },
        "memories.update" => record(crate::lists::MEMORY, "memory note"),
        "work.edit" | "work.attach" | "work.todos.add" | "work.todos.edit" => record(crate::work::WORK, "work item"),
        "projects.edit" | "projects.work" | "projects.links" | "projects.phases" => record(crate::projects::PROJECTS, "project"),
        "workflows.run"
        | "triggers.create"
        | "decisions.add"
        | "memories.create"
        | "work.add"
        | "projects.add"
        | "docs.create"
        | "docs.edit"
        | "docs.rename"
        | "docs.comment"
        | "docs.reply"
        | "collections.create"
        | "collections.update"
        | "records.create"
        | "records.put"
        | "series.create"
        | "series.update"
        | "series.record"
        | "views.act"
        | "memex.notes.create"
        | "memex.trails.append"
        | "memex.trails.update"
        | "memex.relations.write"
        | "claims.add"
        | "things.create"
        | "knowledge.record"
        | "knowledge.set"
        | "mail.drafts.set" => Named,
        _ => return None,
    })
}

/// What the assistant in root would do in a realm, for you to approve, or
/// why it cannot; `None`: nothing goes into a realm (or it goes into root
/// itself). What it carries is shown in full, so nothing that could hide
/// more goes: no files or attachments, no secret you gave it, nothing over
/// PROMPT_SHOWN characters. `unsealed`: a secret was put in its input.
/// Allowing or denying what waits on your approval (a tool call, a
/// workflow step, an approve decision) is yours: Ask never answers it, in
/// any realm, so a policy that holds a call for you can't be passed by an
/// agent. It points you to the card instead.
async fn yours_to_approve(st: &HeadState, name: &str, args: &Value) -> Option<String> {
    let approval = match name {
        "sessions.approvals.decide" | "workflows.steps.decide" => true,
        "decisions.answer" => match args["id"].as_i64().or_else(|| args["id"].as_str().and_then(|i| i.parse().ok())) {
            Some(id) => crate::decisions::get(st, id).await.ok().flatten().is_some_and(|d| d.kind == "approve"),
            None => false,
        },
        _ => false,
    };
    approval.then(|| {
        "approvals are the user's to answer, not yours: don't allow or deny it. Tell them it's waiting, and that they answer it in its approval card (or in Decisions)".to_owned()
    })
}

async fn into_realm(st: &HeadState, op: &Op, args: &mut Value, unsealed: bool) -> Result<Option<IntoRealm>, String> {
    if op.name == "sessions.create" {
        if unsealed {
            return Err(SECRET_STAYS.into());
        }
        return start_in_realm(st, args).await.map(Some);
    }
    if FILES.contains(&op.name) {
        return Err(format!(
            "from root, {} cannot put a file in a realm: the user approves in full what goes there, and a file cannot be shown. \
             They can add it in that realm itself",
            op.name
        ));
    }
    let Some(reach) = reach(op.name) else { return Ok(None) };
    let param = |p: &str| match &args[p] {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let named = [&args["body"]["realm"], &args["realm"]]
        .into_iter()
        .find_map(|r| r.as_str().map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned));
    let text = |k: &str| args["body"][k].as_str().map(str::trim).unwrap_or("").to_owned();
    let (name, id) = (param("name"), param("id"));
    // Its realm, and what it acts on.
    let (realm, what) = match reach {
        Reach::Session => {
            let row = st.store.session(&id).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no session {id:?}"))?;
            if op.name == "sessions.send" && row.realm != crate::realms::ROOT {
                crate::sessions::takes_messages(&row).map_err(|e| format!("{e:#}"))?;
            }
            (row.realm, format!("the session “{}”", if row.title.is_empty() { &id } else { &row.title }))
        }
        Reach::Object { table, collection, noun } => {
            let (realm, title) = object(st, table, collection, &id).await.ok_or_else(|| format!("no {noun} {id:?}"))?;
            (realm, format!("the {noun} “{}”", if title.is_empty() { id.clone() } else { crate::util_clip(&title, 80) }))
        }
        Reach::Template => {
            let realm = match named {
                Some(r) => r,
                None => crate::templates::realm_of(st, &name).await.map_err(|e| format!("{e:#}"))?,
            };
            (realm, format!("the template “{name}”"))
        }
        Reach::PathRealm => (param("realm"), String::new()),
        Reach::Named => (named.unwrap_or_else(|| crate::realms::HOME.to_owned()), String::new()),
    };
    if realm == crate::realms::ROOT {
        return Ok(None);
    }
    if realm.is_empty() || !crate::realms::exists(st, &realm).await.map_err(|e| format!("{e:#}"))? {
        return Err(format!("no realm {realm:?}: name one from realms.list"));
    }
    if unsealed {
        return Err(SECRET_STAYS.into());
    }
    let place = realm_name(st, &realm).await;
    let or_none = |s: String, none: &str| if s.is_empty() { none.to_owned() } else { s };
    let (said, shown, message) = match op.name {
        "sessions.send" => {
            let req: api::SendMessage =
                serde_json::from_value(args["body"].clone()).map_err(|e| format!("sessions.send needs a body (c6_describe it): {e}"))?;
            if !req.attachments.is_empty() || req.connection_id.is_some() || req.model.is_some() || req.effort.is_some() {
                return Err("from root a message carries its text only: no attachments or model switch".into());
            }
            let body = req.text.trim().to_owned();
            if body.is_empty() {
                return Err("the message is empty".into());
            }
            let said = format!(
                "Send {what} in {place} a message. Its agent gets it as a message from “{FROM_CONSOLE}” (not from you), under \
                 {place}'s own rules. The message:"
            );
            (said, body.clone(), Some((id.clone(), body)))
        }
        "sessions.side_chat" => {
            let model = or_none(text("model"), "its model");
            let said = format!(
                "Start a side chat off {what} in {place}, with {model}, labelled {STARTED_FROM_CONSOLE}, under {place}'s own rules. \
                 Its first message:"
            );
            (said, text("prompt"), None)
        }
        "sessions.queue.edit" => (format!("Change a message waiting to go to {what} in {place} to read:"), text("text"), None),
        "templates.run" => {
            let input = text("input");
            args["body"]["realm"] = json!(realm);
            let said = format!(
                "Start {what} in {place}: its sessions run under {place}'s own rules, labelled {STARTED_FROM_CONSOLE}. {}",
                if input.is_empty() { "It has no input." } else { "Its input:" }
            );
            (said, or_none(input, "(no input)"), None)
        }
        "workflows.run" => {
            let input = text("input");
            let said = format!(
                "Run the workflow “{name}” in {place}, under {place}'s own rules, started by the console. {}",
                if input.is_empty() { "It has no input." } else { "Its input:" }
            );
            (said, or_none(input, "(no input)"), None)
        }
        _ => {
            // The memex takes no realm: it is the realm it runs as.
            if matches!(op.via, Via::Memex { .. })
                && let Some(o) = args.as_object_mut()
            {
                o.remove("realm");
            }
            let on = if what.is_empty() { String::new() } else { format!(" on {what}") };
            let said = format!(
                "{}{on} in {place}: {} It runs as {place}'s, under its rules and with nothing of root's, written by the console. \
                 What goes there, exactly:",
                op.name, op.summary
            );
            (said, format!("{} {}", op.name, serde_json::to_string_pretty(&*args).unwrap_or_default()), None)
        }
    };
    let long = shown.chars().count();
    if long > PROMPT_SHOWN {
        return Err(format!(
            "that is too long to approve in full ({long} characters; at most {PROMPT_SHOWN}): make it shorter, or do it in parts"
        ));
    }
    Ok(Some(IntoRealm { realm, said, shown, message }))
}

/// The realm and title of a trigger, a decision, or a record of a
/// collection (a work item, project or memory note), if there is one.
async fn object(st: &HeadState, table: &str, collection: &str, id: &str) -> Option<(String, String)> {
    let query = match table {
        "triggers" => sqlx::query("SELECT realm, name AS title FROM triggers WHERE id = ?"),
        "decisions" => sqlx::query("SELECT realm, title FROM decisions WHERE id = ?"),
        _ => sqlx::query(
            "SELECT realm, COALESCE(json_extract(fields, '$.title'), json_extract(fields, '$.text'), '') AS title FROM records \
             WHERE collection = ? AND id = ?",
        )
        .bind(collection.to_owned()),
    };
    use sqlx::Row;
    let row = query.bind(id.to_owned()).fetch_optional(&st.store.pool).await.ok().flatten()?;
    Some((row.try_get("realm").ok()?, row.try_get::<String, _>("title").unwrap_or_default()))
}

/// A realm as you read it: its name, with its id when that differs.
async fn realm_name(st: &HeadState, realm: &str) -> String {
    crate::realms::list(st, &crate::realms::Caller::root())
        .await
        .ok()
        .and_then(|rs| rs.into_iter().find(|r| r.id == realm))
        .map_or_else(|| realm.to_owned(), |r| if r.name == realm { realm.to_owned() } else { format!("{} ({realm})", r.name) })
}

/// What starting a session from root asks you, or why it cannot: the
/// realm it runs in, where and how, and its first message, in full. The
/// first message is the one way root's knowledge reaches the realm, so
/// nothing else that could carry it is passed (attachments, work, to-dos).
/// Labels the call `started-by:console`.
async fn start_in_realm(st: &HeadState, args: &mut Value) -> Result<IntoRealm, String> {
    let req: api::CreateSession =
        serde_json::from_value(args["body"].clone()).map_err(|e| format!("sessions.create needs a body (c6_describe it): {e}"))?;
    let realm = req.realm.as_deref().map(str::trim).unwrap_or("").to_owned();
    if realm.is_empty() || realm == crate::realms::ROOT {
        return Err("sessions do not run in root: name the realm to start it in (body.realm, from realms.list), and the user \
                    approves it there"
            .into());
    }
    crate::realms::check_for_sessions(st, &realm).await.map_err(|e| format!("{e:#}"))?;
    let other_work = req.work.as_deref().map(str::trim).is_some_and(|w| !matches!(w, "" | "new" | "none" | "guess"));
    if !req.attachments.is_empty() || !req.work_refs.is_empty() || req.todo.is_some() || other_work {
        return Err("from root a session starts with its first message only: no attachments, work item, to-do or references".into());
    }
    let prompt = req.prompt.as_deref().map(str::trim).unwrap_or("").to_owned();
    if prompt.chars().count() > PROMPT_SHOWN {
        return Err(format!("keep the first message under {PROMPT_SHOWN} characters: the user approves it in full"));
    }
    let worker = st.store.worker(WorkerId(req.worker_id)).await.map_err(|e| format!("{e:#}"))?.ok_or("no such worker")?;
    if !crate::realms::available(&worker.realms, &realm) {
        return Err(format!("{} is not available to realm {realm}", worker.name));
    }
    let name = realm_name(st, &realm).await;
    let mut said = format!("Start a session in {name} on {}", worker.name);
    if let Some(ssh) = req.ssh.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        said.push_str(&format!(" over SSH to {ssh}"));
    }
    if !req.cwd.trim().is_empty() {
        said.push_str(&format!(", in {}{}", req.cwd.trim(), if req.worktree { " (a new git worktree)" } else { "" }));
    }
    if !req.model.trim().is_empty() || !req.connection_id.trim().is_empty() {
        let connection = match st.store.connection(req.connection_id.trim()).await {
            Ok(Some(c)) => c.name,
            _ => req.connection_id.trim().to_owned(),
        };
        let on = if connection.is_empty() { String::new() } else { format!(" on {connection}") };
        said.push_str(&format!(", with {}{on}", Some(req.model.trim()).filter(|m| !m.is_empty()).unwrap_or("its default model")));
    }
    if let Some(e) = req.effort.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        said.push_str(&format!(", effort {e}"));
    }
    if let Some(t) = req.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        said.push_str(&format!(", titled “{t}”"));
    }
    if !req.labels.is_empty() {
        said.push_str(&format!(", labelled {}", req.labels.join(", ")));
    }
    said.push_str(&format!(
        ". It runs under {name}'s own rules, labelled {STARTED_FROM_CONSOLE}. {}",
        if prompt.is_empty() { "It has no first message." } else { "Its first message:" }
    ));
    let mut labels = req.labels.clone();
    labels.push(STARTED_FROM_CONSOLE.to_owned());
    args["body"]["labels"] = json!(labels);
    args["body"]["realm"] = json!(realm);
    Ok(IntoRealm { realm, said, shown: if prompt.is_empty() { "(no first message)".into() } else { prompt }, message: None })
}

/// The first of your policies (ask or deny) that applies to this call.
async fn policy_for(st: &HeadState, labels: &[String], input: &str) -> Option<api::Policy> {
    let policies: Vec<api::Policy> =
        serde_json::from_str(&st.store.setting(crate::sessions::POLICIES).await.ok().flatten().unwrap_or_default()).unwrap_or_default();
    policies.into_iter().find(|p| {
        p.enabled
            && api::labels::applies(&p.applies_to, labels)
            && (p.tools.is_empty() || regex::Regex::new(&p.tools).is_ok_and(|r| r.is_match("c6_call")))
            && (p.pattern.is_empty() || regex::Regex::new(&p.pattern).is_ok_and(|r| r.is_match(input)))
    })
}

/// Entries a list replacement would remove (by name or id).
async fn removed_entries(st: &HeadState, op: &Op, args: &Value, realm: &str) -> anyhow::Result<Vec<String>> {
    let Some(get) = op.replaces.and_then(catalog::find) else { return Ok(Vec::new()) };
    let current = execute(st, get, json!({}), "assistant", Some(realm)).await?;
    let new = args["body"].as_array().cloned().unwrap_or_default();
    let key = |v: &Value| -> String {
        for k in ["name", "id", "path", "key"] {
            if let Some(s) = v[k].as_str() {
                return s.to_owned();
            }
        }
        v.to_string()
    };
    let kept: Vec<String> = new.iter().map(key).collect();
    Ok(current.as_array().into_iter().flatten().map(key).filter(|k| !kept.contains(k)).collect())
}

/// Runs an operation as you: through the head's own handlers (HTTP ops) or
/// the memex (memex ops).
/// Runs an operation as you, within `realm` when given (root or none:
/// every realm, with administration).
pub async fn execute(st: &HeadState, op: &Op, args: Value, made_by: &str, realm: Option<&str>) -> anyhow::Result<Value> {
    execute_as(st, op, args, made_by, realm, false).await
}

/// `execute`; `console`: as the console's assistant, in `realm` with your
/// approval (what it writes is the console's).
async fn execute_as(st: &HeadState, op: &Op, mut args: Value, made_by: &str, realm: Option<&str>, console: bool) -> anyhow::Result<Value> {
    if !args.is_object() {
        args = json!({});
    }
    let realm = realm.filter(|r| *r != crate::realms::ROOT);
    match &op.via {
        // As you, on the realm's knowledge (home's, from root), as the API
        // calls it.
        Via::Memex { method } => crate::memex::call_as(st, realm.unwrap_or(crate::realms::HOME), method, args, made_by).await,
        Via::Http { method, path } => {
            let router = st.assistant.router.get().ok_or_else(|| anyhow::anyhow!("the head is starting"))?.clone();
            let mut uri = (*path).to_owned();
            let params = catalog::path_params(path);
            for p in &params {
                let v = match &args[*p] {
                    Value::String(s) if !s.is_empty() => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => return Err(bad_request(format!("{} needs {p}", op.name))),
                };
                uri = uri.replace(&format!("{{{p}}}"), &encode(&v));
            }
            let mut body = axum::body::Body::empty();
            match op.input {
                Input::Body(_) => {
                    let b = args.get("body").cloned().unwrap_or(Value::Null);
                    if !b.is_null() {
                        body = axum::body::Body::from(serde_json::to_vec(&b)?);
                    }
                }
                Input::Query(_) => {
                    let mut q = Vec::new();
                    if let Some(o) = args.as_object() {
                        for (k, v) in o {
                            if params.contains(&k.as_str()) || v.is_null() {
                                continue;
                            }
                            let s = match v {
                                Value::String(s) => s.clone(),
                                Value::Array(a) => a
                                    .iter()
                                    .map(|x| x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string()))
                                    .collect::<Vec<_>>()
                                    .join(","),
                                other => other.to_string(),
                            };
                            q.push(format!("{}={}", encode(k), encode(&s)));
                        }
                    }
                    if !q.is_empty() {
                        uri = format!("{uri}?{}", q.join("&"));
                    }
                }
                _ => {}
            }
            let mut req = axum::http::Request::builder()
                .method(*method)
                .uri(&uri)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(body)?;
            req.extensions_mut().insert(InternalCall { realm: realm.map(str::to_owned), console });
            use tower::ServiceExt;
            let resp = router.oneshot(req).await?;
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024).await?;
            let v: Value = if bytes.is_empty() {
                json!({"ok": true})
            } else {
                serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)))
            };
            if !status.is_success() {
                let msg = v["error"].as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
                return Err(if status.is_client_error() { bad_request(msg) } else { anyhow::anyhow!("{status}: {msg}") });
            }
            Ok(v)
        }
    }
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

struct Ask {
    env: Arc<Env>,
}

#[async_trait]
impl Tool for Ask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "c6_ask".into(),
            description: "Asks the user for inputs with a form, and waits for the answers. Use it whenever an operation needs a value \
                          you do not have or that is ambiguous; never guess. Each field: name, label, type (text, longtext, number, \
                          boolean, secret, choice), required, default, description, and for a choice either `choices` \
                          ([{value, label}]) or `choices_from` (an operation that lists them, with its input). A secret's answer \
                          comes back as a placeholder: pass it as is to c6_call, which puts the value in."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "title": {"type": "string", "description": "what you are asking for, e.g. New worker"},
                "fields": {"type": "array", "items": {"type": "object", "properties": {
                    "name": {"type": "string"},
                    "label": {"type": "string"},
                    "type": {"type": "string", "enum": ["text", "longtext", "number", "boolean", "secret", "choice"]},
                    "required": {"type": "boolean"},
                    "default": {},
                    "description": {"type": "string"},
                    "choices": {"type": "array", "items": {"type": "object", "properties": {"value": {"type": "string"}, "label": {"type": "string"}}, "required": ["value"]}},
                    "choices_from": {"type": "object", "properties": {"op": {"type": "string"}, "input": {"type": "object"}}, "required": ["op"]}
                }, "required": ["name"]}}
            }, "required": ["title", "fields"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let title = input["title"].as_str().unwrap_or("A few details").to_owned();
        let mut fields: Vec<api::QuestionField> = Vec::new();
        for f in input["fields"].as_array().into_iter().flatten() {
            let mut field: api::QuestionField = match serde_json::from_value(f.clone()) {
                Ok(x) => x,
                Err(e) => return ToolOutput::err(format!("a field is not valid: {e}")),
            };
            if field.label.is_empty() {
                field.label = field.name.replace('_', " ");
            }
            if let Some(from) = f["choices_from"]["op"].as_str() {
                match choices(&self.env.st, from, f["choices_from"]["input"].clone(), &self.env.realm).await {
                    Ok(c) => {
                        field.choices = c;
                        field.r#type = "choice".into();
                    }
                    Err(e) => return ToolOutput::err(format!("choices for {}: {e:#}", field.name)),
                }
            }
            fields.push(field);
        }
        if fields.is_empty() {
            return ToolOutput::err("ask for at least one field");
        }
        let question = |state: &str, answer: &Value| {
            session_event::Kind::Question(c6_proto::Question {
                call_id: ctx.call_id.clone(),
                title: title.clone(),
                fields_json: serde_json::to_string(&fields).unwrap_or_default(),
                state: state.into(),
                answer_json: if answer.is_null() { String::new() } else { answer.to_string() },
            })
        };
        let rx = self.env.listen(&ctx.call_id);
        if let Err(e) = self.env.sink.append(question("asked", &Value::Null)).await {
            self.env.waiters.lock().unwrap().remove(&ctx.call_id);
            return ToolOutput::err(format!("could not ask: {e:#}"));
        }
        match self.env.wait(&ctx.call_id, rx, &ctx.cancel).await {
            Some(Reply::Answer { values, cancelled: false }) => {
                let mut for_model = serde_json::Map::new();
                let mut for_log = serde_json::Map::new();
                for f in &fields {
                    let v = values.get(&f.name).cloned().unwrap_or(Value::Null);
                    if f.r#type == "secret" && v.as_str().is_some_and(|s| !s.is_empty()) {
                        let mut vault = self.env.vault.lock().unwrap();
                        let placeholder = format!("{{{{secret:{}:{}}}}}", f.name, vault.len() + 1);
                        vault.insert(placeholder.clone(), v.as_str().unwrap_or_default().to_owned());
                        for_model.insert(f.name.clone(), json!(placeholder));
                        for_log.insert(f.name.clone(), json!("[secret]"));
                    } else {
                        for_model.insert(f.name.clone(), v.clone());
                        for_log.insert(f.name.clone(), v);
                    }
                }
                let _ = self.env.sink.append(question("answered", &Value::Object(for_log))).await;
                ToolOutput::ok(serde_json::to_string_pretty(&Value::Object(for_model)).unwrap_or_default())
            }
            _ => {
                let _ = self.env.sink.append(question("cancelled", &Value::Null)).await;
                ToolOutput::err("the user cancelled the form; ask what they want instead")
            }
        }
    }
}

/// A list operation's items as choices.
/// Work on a machine (docs/ask.md §2): a remote subagent from one of the
/// templates agents may start, waited on until its turn ends.
struct Delegate {
    env: Arc<Env>,
    templates: Vec<api::SubagentTemplate>,
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        let list = if self.templates.is_empty() {
            "None yet: the user adds one in Settings → Templates and ticks \"Agents may start it\" (a worker, folder and model).".to_owned()
        } else {
            self.templates
                .iter()
                .map(|t| {
                    let mut line = format!("- {}: {} (on {}", t.name, t.description, t.place);
                    if !t.model.is_empty() {
                        line.push_str(&format!("; {}", t.model));
                    }
                    if !t.targets.is_empty() {
                        line.push_str(&format!("; on is one of {}", t.targets.join(", ")));
                    }
                    line.push(')');
                    line
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        ToolSpec {
            name: "c6_delegate".into(),
            description: format!(
                "Starts a subagent on one of the user's machines and waits for its report: for what needs a machine (running commands, \
                 tests or builds, reading a repository in depth). It runs one of the templates the user lets agents start, which says \
                 where (worker, folder) and on which model; you can't pick another place. prompt is all the subagent sees: make it \
                 stand alone. Its report comes back as this call's result.\nTemplates:\n{list}"
            ),
            parameters: json!({"type": "object", "properties": {
                "template": {"type": "string", "description": "The template's name"},
                "on": {"type": "string", "description": "Which of its places, for a template with several"},
                "prompt": {"type": "string", "description": "The task, standing on its own"},
                "description": {"type": "string", "description": "A few words for its thread"}
            }, "required": ["template", "prompt"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let s = |k: &str| input[k].as_str().unwrap_or_default().trim().to_owned();
        let (template, on, prompt) = (s("template"), s("on"), s("prompt"));
        let description = Some(s("description")).filter(|d| !d.is_empty()).unwrap_or_else(|| template.clone());
        if template.is_empty() || prompt.is_empty() {
            return ToolOutput::err("template and prompt are required");
        }
        let st = &self.env.st;
        let row = match st.store.session(&self.env.session_id).await {
            Ok(Some(r)) => r,
            _ => return ToolOutput::err("this conversation is gone"),
        };
        // Listening before it starts, so a quick report isn't missed.
        let rx = self.env.listen(&ctx.call_id);
        let req = crate::templates::SubagentStart {
            template: &template,
            on: &on,
            prompt: &prompt,
            description: &description,
            call_id: &ctx.call_id,
            depth: 0,
        };
        let (child, remote) = match crate::templates::start_subagent(st, &row, &req).await {
            Ok(x) => x,
            Err(e) => {
                self.env.waiters.lock().unwrap().remove(&ctx.call_id);
                return ToolOutput::err(format!("{e:#}"));
            }
        };
        let spawned = session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned {
            call_id: ctx.call_id.clone(),
            child_session_id: child.id.clone(),
            agent: String::new(),
            description: description.clone(),
            background_task_id: String::new(),
            model: Some(c6_proto::ModelSpec {
                connection_id: child.connection_id.clone(),
                model: child.model.clone(),
                effort: child.effort.clone().unwrap_or_default(),
                ..Default::default()
            }),
            remote: Some(c6_proto::RemoteSubagent {
                template: remote.template.clone(),
                worker_id: remote.worker_id.clone(),
                worker_name: remote.worker_name.clone(),
                target: remote.target.clone(),
                cwd: remote.cwd.clone(),
            }),
        });
        if let Err(e) = self.env.sink.append(spawned).await {
            warn!(session = %self.env.session_id, "recording a delegation: {e:#}");
        }
        // Until its turn ends, or you stop the conversation (it stops too).
        let reply = tokio::select! {
            r = rx => r.ok(),
            _ = ctx.cancel.cancelled() => None,
        };
        self.env.waiters.lock().unwrap().remove(&ctx.call_id);
        match reply {
            Some(Reply::Delegation(d)) => {
                let text = report_text(&d);
                if d.reason == c6_proto::TurnEndReason::Completed as i32 { ToolOutput::ok(text) } else { ToolOutput::err(text) }
            }
            _ => {
                let _ = crate::sessions::interrupt(st, &child.id).await;
                ToolOutput::err(format!("stopped before {} reported (its session: {})", remote.worker_name, child.id))
            }
        }
    }
}

/// A subagent's report as the conversation hears it.
fn report_text(d: &c6_proto::DelegationEnded) -> String {
    let reason = c6_proto::TurnEndReason::try_from(d.reason).unwrap_or(c6_proto::TurnEndReason::Unspecified);
    let status = match reason {
        c6_proto::TurnEndReason::Completed => "completed",
        c6_proto::TurnEndReason::Interrupted => "interrupted",
        c6_proto::TurnEndReason::WorkerRestarted => "lost",
        _ => "failed",
    };
    let mut s = format!("<subagent-report session=\"{}\" status=\"{status}\">\n", d.child_session_id);
    if !d.report.trim().is_empty() {
        s.push_str(d.report.trim());
        s.push('\n');
    }
    if !d.digest.trim().is_empty() {
        s.push_str(d.digest.trim());
        s.push('\n');
    }
    s.push_str("</subagent-report>");
    s
}

/// The most of one mentioned item the model is given.
const MENTION_CHARS: usize = 8_000;

/// Your message with what it @-mentions after it (docs/ask.md §4): each
/// `c6:<ref>` link read with your access, only that item; "" when it
/// mentions nothing (the text goes as it is).
pub async fn with_mentions(st: &HeadState, realm: &str, text: &str) -> String {
    let refs = mention_refs(text);
    if refs.is_empty() {
        return String::new();
    }
    let mut out = text.to_owned();
    for r in refs {
        let (kind, title, body) = match read_mention(st, realm, &r).await {
            Ok(x) => x,
            Err(e) => (String::new(), String::new(), format!("(could not read it: {e:#})")),
        };
        let body: String =
            if body.chars().count() > MENTION_CHARS { body.chars().take(MENTION_CHARS).collect::<String>() + "\n…" } else { body };
        out.push_str(&format!(
            "\n\n<mentioned ref=\"{r}\" kind=\"{kind}\" title=\"{}\" by=\"you\">\n{}\n</mentioned>",
            title.replace('"', "'"),
            body.trim()
        ));
    }
    out
}

/// The `c6:<ref>` links in a message, each once, in order.
pub fn mention_refs(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, _) in text.match_indices("(c6:") {
        let rest = &text[i + 4..];
        let Some(end) = rest.find(')') else { continue };
        let r = rest[..end].trim();
        let known = ["item:", "project:", "doc:", "work:"].iter().any(|p| r.strip_prefix(p).is_some_and(|id| !id.is_empty()));
        if known && !r.contains(char::is_whitespace) && !out.iter().any(|o| o == r) {
            out.push(r.to_owned());
        }
    }
    out
}

/// One mentioned thing: (kind, title, text).
async fn read_mention(st: &HeadState, realm: &str, r: &str) -> anyhow::Result<(String, String, String)> {
    let (what, id) = r.split_once(':').ok_or_else(|| anyhow::anyhow!("not a reference"))?;
    let realm_arg = Some(realm).filter(|r| *r != crate::realms::ROOT);
    let run = async |op: &str, args: Value| -> anyhow::Result<Value> {
        let op = catalog::find(op).ok_or_else(|| anyhow::anyhow!("no operation {op}"))?;
        execute(st, op, args, "assistant", realm_arg).await
    };
    let str_at = |v: &Value, path: &str| v.pointer(path).and_then(Value::as_str).unwrap_or_default().to_owned();
    match what {
        // Any kind: you mentioned it, so your mail, chat and calendar too.
        "item" => {
            let all: Vec<String> = api::MEMEX_KINDS.iter().map(|k| k.to_string()).collect();
            let v = crate::memex::SCOPE.scope(all, run("memex.get", json!({"id": id}))).await?;
            let item = if v["item"].is_object() { &v["item"] } else { &v };
            let kind = str_at(item, "/kind");
            let title = [str_at(item, "/title"), str_at(item, "/subject"), str_at(item, "/name")]
                .into_iter()
                .find(|t| !t.is_empty())
                .unwrap_or_default();
            let mut body = String::new();
            for k in ["from", "author", "date", "when", "starts_at"] {
                if let Some(x) = item.get(k).filter(|x| !x.is_null()) {
                    body.push_str(&format!("{k}: {}\n", x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string())));
                }
            }
            let text = [str_at(item, "/text"), str_at(item, "/body"), str_at(item, "/content"), str_at(item, "/summary")]
                .into_iter()
                .find(|t| !t.trim().is_empty())
                .unwrap_or_else(|| serde_json::to_string_pretty(item).unwrap_or_default());
            body.push_str(&text);
            Ok((kind, title, body))
        }
        "project" => {
            let v = run("projects.get", json!({"id": id})).await?;
            let mut body = format!("Purpose: {}\n", str_at(&v, "/purpose"));
            let note = str_at(&v, "/note/text");
            if !note.is_empty() {
                body.push_str(&format!("Where it stands: {note}\n"));
            }
            for w in v["work"].as_array().into_iter().flatten() {
                body.push_str(&format!("- work {}: {} ({})\n", str_at(w, "/id"), str_at(w, "/title"), str_at(w, "/state")));
            }
            Ok(("project".into(), str_at(&v, "/title"), body))
        }
        "work" => {
            let v = run("work.get", json!({"id": id})).await?;
            let mut body = format!("Done when: {}\nState: {} {}\n", str_at(&v, "/done_when"), str_at(&v, "/state"), str_at(&v, "/why"));
            let note = str_at(&v, "/note/text");
            if !note.is_empty() {
                body.push_str(&format!("Where it stands: {note}\n"));
            }
            for t in v.pointer("/todos").and_then(Value::as_array).into_iter().flatten() {
                let done = t.get("done_at").is_some_and(|d| !d.is_null());
                body.push_str(&format!("- [{}] {}\n", if done { "x" } else { " " }, str_at(t, "/text")));
            }
            Ok(("work".into(), str_at(&v, "/title"), body))
        }
        "doc" => {
            let v = run("docs.get", json!({"id": id})).await?;
            let mut body = String::new();
            for tab in v["tabs"].as_array().into_iter().flatten() {
                for b in tab["blocks"].as_array().into_iter().flatten() {
                    let md = str_at(b, "/markdown");
                    if !md.is_empty() {
                        body.push_str(&md);
                        body.push_str("\n\n");
                    }
                }
            }
            Ok((str_at(&v, "/kind"), str_at(&v, "/title"), body))
        }
        _ => Err(anyhow::anyhow!("unknown reference")),
    }
}

/// What @ offers in an Ask conversation for `q` (docs/ask.md §4): your
/// projects, docs and work by title, and the memex's items of any kind,
/// your mail, chat and calendar included (you are the one asking).
pub async fn mentionables(st: &HeadState, realm: &str, q: &str) -> anyhow::Result<Vec<api::Mentionable>> {
    let q = q.trim();
    if q.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let needle = q.to_lowercase();
    let realm_arg = Some(realm).filter(|r| *r != crate::realms::ROOT);
    let run = async |op: &str, args: Value| -> Value {
        match catalog::find(op) {
            Some(op) => execute(st, op, args, "assistant", realm_arg).await.unwrap_or(Value::Null),
            None => Value::Null,
        }
    };
    let mut out = Vec::new();
    let title_of = |v: &Value| v.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
    for (op, what) in [("projects.list", "project"), ("work.list", "work"), ("docs.list", "doc")] {
        let v = run(op, json!({})).await;
        let list =
            v.as_array().cloned().or_else(|| v["items"].as_array().cloned()).or_else(|| v["docs"].as_array().cloned()).unwrap_or_default();
        for x in list.iter().filter(|x| title_of(x).to_lowercase().contains(&needle)).take(5) {
            let id = x["id"].as_str().unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            let kind = if what == "doc" { x["kind"].as_str().unwrap_or("doc").to_owned() } else { what.to_owned() };
            out.push(api::Mentionable { reference: format!("{what}:{id}"), title: title_of(x), kind, detail: String::new() });
        }
    }
    let all: Vec<String> = api::MEMEX_KINDS.iter().map(|k| k.to_string()).collect();
    let found = crate::memex::SCOPE.scope(all, run("memex.search", json!({"q": q, "limit": 10}))).await;
    let hits = found
        .as_array()
        .cloned()
        .or_else(|| found["hits"].as_array().cloned())
        .or_else(|| found["results"].as_array().cloned())
        .or_else(|| found["items"].as_array().cloned())
        .unwrap_or_default();
    for h in hits.iter().take(10) {
        let item = if h["item"].is_object() { &h["item"] } else { h };
        let id = item["id"].as_str().map(str::to_owned).or_else(|| item["id"].as_i64().map(|n| n.to_string())).unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let title = ["title", "subject", "name"].iter().filter_map(|k| item[*k].as_str()).find(|t| !t.is_empty()).unwrap_or("(untitled)");
        let detail = ["from", "author", "date"].iter().filter_map(|k| item[*k].as_str()).collect::<Vec<_>>().join(" · ");
        out.push(api::Mentionable {
            reference: format!("item:{id}"),
            title: title.to_owned(),
            kind: item["kind"].as_str().unwrap_or("item").to_owned(),
            detail,
        });
    }
    Ok(out)
}

async fn choices(st: &HeadState, op: &str, input: Value, realm: &str) -> anyhow::Result<Vec<api::Choice>> {
    let op = catalog::find(op).ok_or_else(|| anyhow::anyhow!("no operation {op}"))?;
    anyhow::ensure!(op.effect == Effect::Read, "{} is not a read operation", op.name);
    let v = execute(st, op, input, "assistant", Some(realm)).await?;
    let items: Vec<Value> = match v {
        Value::Array(a) => a,
        Value::Object(o) => o.into_iter().find_map(|(_, x)| x.as_array().cloned()).unwrap_or_default(),
        _ => Vec::new(),
    };
    let s = |v: &Value, keys: &[&str]| keys.iter().find_map(|k| v[*k].as_str().filter(|s| !s.is_empty()).map(str::to_owned));
    Ok(items
        .iter()
        .take(200)
        .filter_map(|x| {
            if let Some(s) = x.as_str() {
                return Some(api::Choice { value: s.to_owned(), label: s.to_owned() });
            }
            let value = s(x, &["id", "name", "email", "key", "path"])?;
            let label = s(x, &["name", "title", "email", "path"]).unwrap_or_else(|| value.clone());
            Some(api::Choice { label, value })
        })
        .collect())
}

/// The assistant's instructions (an editable prompt, `assistant.base`).
pub const PROMPT: &str = "You are the carbon6 assistant. carbon6 is the user's self-hosted command center for AI agents: a head (this \
server, with the web UI) and workers (machines that run agent sessions), plus a memex (their knowledge store, with mail, calendar, \
documents and photos). You can do anything the user can do in the UI, through operations.

How to work:
- Find operations with c6_discover; read one with c6_describe before using it; run it with c6_call. Answer questions about the \
user's setup by reading the relevant settings or lists, not from memory; quote what you found.
- When an operation needs a value you do not have, or there is a real choice to make, ask with c6_ask (use choices_from to offer \
real options: workers, connections, sessions…). Never guess names, ids or settings. Do not ask for what you can look up.
- To change a list setting (policies, routes, channels…), read it, change it, and send the whole list back.
- Destructive operations wait for the user's approval in the UI; say what you are about to do and why before calling one.
- Secrets (tokens, codes) returned by operations are shown to the user directly; you never see them. For a secret the user must \
give (an API key, a password), ask with a secret field.
- Which model does what is in settings: sessions by settings.routes (by label), the memex's tasks (embeddings, understanding, \
drafting, vision, transcription) in settings.memex, yourself in settings.assistant. A connection's available models: connections.models.
- Be brief. After acting, say what you did and anything the user should do next (e.g. run a command on another machine).
- You work in one realm: {{realm}}. In root you administer carbon6 (workers, connections, realms, grants, settings) and reach \
every realm. In any other realm you see and change only that realm's sessions, collections, decisions, triggers, views, series and \
knowledge, and administration is refused: for that, the user opens the assistant with \"All realms\". Never act in another realm; \
when this realm needs another's data, suggest a grant.
- You cannot run commands yourself. For work on a machine (running tests, builds, commands, reading a repository in depth), \
use c6_delegate: it runs one of the templates the user lets agents start and waits for its report, which you then answer from. \
With no template for the place, tell the user to add one (Settings → Templates, \"Agents may start it\"), or start a session on a \
worker (sessions.create) and tell the user. Sessions never run in root: from root, start it in the realm the work belongs to \
(body.realm; ask if that is unclear), never send the user to another realm for it. Then follow the session (sessions.get, \
sessions.events) and report. For a quick look at a file or the git state on a worker, the workers' file and git operations do. \
For something recurring, create a trigger or workflow.
- From root, whatever you put in a realm waits for the user's approval each time, showing the realm and exactly what goes there: \
starting a session, a message to a session, a side chat, a template, a workflow, triggers, decisions and answers, work, projects, \
documents, records, memory notes and knowledge. For something new, name its realm with realm in the input (or body.realm), else it \
goes to home; what exists already is in its own. \
Write only what that realm should know; files cannot go. If the user does not approve, do not try another way.
- The user may just want to talk: answer from what you know, and use operations only when the question is about their setup or \
asks for something done.
- What the user @-mentions comes after their message in <mentioned> blocks, read for this conversation only: use it, and say \
where an answer came from.

Areas of carbon6:
{{areas}}
{{context}}Today is {{today}}.";

/// The assistant's instructions, from its prompt.
pub async fn prompt(st: &HeadState, context: &str, realm: &str) -> String {
    let areas: String = catalog::areas().iter().map(|(a, d)| format!("- {a}: {d}\n")).collect();
    let context = if context.trim().is_empty() { String::new() } else { format!("The user is looking at: {}\n\n", context.trim()) };
    let today = chrono::Utc::now().format("%A %-d %B %Y").to_string();
    let r = crate::realms::list(st, &crate::realms::Caller::root()).await.unwrap_or_default().into_iter().find(|r| r.id == realm);
    let realm_text = match r {
        Some(r) if !r.description.is_empty() => format!("{} ({}: {})", r.id, r.name, r.description),
        Some(r) => format!("{} ({})", r.id, r.name),
        None => realm.to_owned(),
    };
    let template = crate::prompts::get(st, "assistant.base", realm).await;
    c6_api::prompts::fill(&template, &[("areas", &areas), ("context", &context), ("today", &today), ("realm", &realm_text)])
}

/// The model for a new conversation: the setting, else the routes for the
/// label `assistant`, else the default.
pub async fn model(st: &HeadState, realm: &str) -> anyhow::Result<(String, String)> {
    let s: api::AssistantSettings =
        serde_json::from_str(&st.store.setting_for(SETTING, realm).await?.unwrap_or_default()).unwrap_or_default();
    let (conn, model) = match &s.model {
        Some(m) => {
            let connections = st.store.connections().await?;
            let c = connections
                .iter()
                .find(|c| c.id == m.connection || c.name == m.connection)
                .ok_or_else(|| bad_request(format!("the assistant's connection {:?} is gone (Settings → Assistant)", m.connection)))?;
            (c.id.clone(), m.model.clone())
        }
        None => (String::new(), String::new()),
    };
    crate::sessions::route_for(st, &conn, &model, &[LABEL.to_owned()]).await
}

pub async fn check_settings(st: &HeadState, s: &api::AssistantSettings) -> anyhow::Result<()> {
    if let Some(m) = &s.model {
        let connections = st.store.connections().await?;
        let c = connections
            .iter()
            .find(|c| c.id == m.connection || c.name == m.connection)
            .ok_or_else(|| bad_request(format!("no connection {:?}", m.connection)))?;
        if c.kind == "claude_code" && st.claude_bin().is_none() {
            return Err(bad_request("Ask on Claude Code runs `claude` on the head's machine, and it isn't on its PATH there"));
        }
        if m.model.trim().is_empty() {
            return Err(bad_request("choose a model"));
        }
    }
    Ok(())
}

/// Logs the start of a conversation (for the head's log).
pub fn started(session_id: &str) {
    info!(session = %session_id, "assistant conversation started");
}

#[cfg(test)]
mod mention_tests {
    use super::*;

    #[test]
    fn mentions_are_carbon6_links_each_once() {
        let text = "See [@Plan](c6:doc:d1) and [@Realms](c6:project:p1), again [@Plan](c6:doc:d1), a [web](https://x.y) link, \
                    [@mail](c6:item:42), [@odd](c6:nonsense:1) and [@bad](c6:doc: spaced)";
        assert_eq!(mention_refs(text), ["doc:d1", "project:p1", "item:42"]);
        assert!(mention_refs("plain words").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the assistant in root asks you for before it goes into a realm
    /// (docs/assistant.md §Realms) is these operations, each one that
    /// exists and writes: a renamed operation must not slip out of it.
    #[test]
    fn what_goes_into_a_realm_from_root_is_asked_for() {
        let asked = [
            "sessions.send",
            "sessions.side_chat",
            "sessions.queue.edit",
            "templates.run",
            "workflows.run",
            "triggers.create",
            "triggers.update",
            "triggers.run",
            "decisions.add",
            "decisions.answer",
            "decisions.session",
            "memories.create",
            "memories.update",
            "work.add",
            "work.edit",
            "work.attach",
            "work.todos.add",
            "work.todos.edit",
            "projects.add",
            "projects.edit",
            "projects.work",
            "projects.links",
            "projects.phases",
            "docs.create",
            "docs.edit",
            "docs.rename",
            "docs.comment",
            "docs.reply",
            "collections.create",
            "collections.update",
            "records.create",
            "records.put",
            "series.create",
            "series.update",
            "series.record",
            "views.act",
            "apps.operations.call",
            "memex.notes.create",
            "memex.trails.append",
            "memex.trails.update",
            "memex.relations.write",
            "claims.add",
            "things.create",
            "knowledge.record",
            "knowledge.set",
            "mail.drafts.set",
        ];
        for name in asked.iter().chain(FILES).chain(&["sessions.create"]) {
            let op = catalog::find(name).unwrap_or_else(|| panic!("no operation {name}"));
            assert_ne!(op.effect, Effect::Read, "{name}");
        }
        let gated: Vec<&str> = catalog::ops().iter().map(|o| o.name).filter(|n| reach(n).is_some()).collect();
        assert_eq!(gated.len(), asked.len(), "{gated:?}");
        assert!(asked.iter().all(|n| gated.contains(n)), "{gated:?}");
    }

    #[tokio::test]
    async fn a_corrupt_assistant_does_not_block_startup_or_other_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let start = || {
            crate::start(crate::HeadConfig {
                data_dir: dir.path().to_owned(),
                bind: vec!["127.0.0.1".parse().unwrap()],
                ui_port: 0,
                worker_port: 0,
                auth: false,
                ..Default::default()
            })
        };
        let head = start().await.unwrap();
        let id = head.state.assistant.head_id().unwrap();
        for (session, data) in [
            ("a-corrupt", vec![0]), // Not a valid protobuf event.
            ("b-no-start", SessionEvent { seq: 1, ts_ms: now_ms(), kind: None }.encode_to_vec()),
            (
                "c-valid",
                SessionEvent {
                    seq: 1,
                    ts_ms: now_ms(),
                    kind: Some(session_event::Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() })),
                }
                .encode_to_vec(),
            ),
        ] {
            sqlx::query(
                "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, last_seq) \
                         VALUES (?, ?, 'test', '', '', '', 'running', 0, 0, 1)",
            )
            .bind(session)
            .bind(id.to_string())
            .execute(&head.state.store.pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES (?, 1, 0, ?)")
                .bind(session)
                .bind(data)
                .execute(&head.state.store.pool)
                .await
                .unwrap();
        }
        head.shutdown().await.unwrap();

        let head = start().await.expect("corrupt session recovery must not fail head startup");
        assert_eq!(head.state.store.session("a-corrupt").await.unwrap().unwrap().status, "running", "leave unreadable evidence alone");
        assert_eq!(head.state.store.session("b-no-start").await.unwrap().unwrap().status, "running");
        let recovered = head.state.store.session("c-valid").await.unwrap().unwrap();
        assert_eq!((recovered.status.as_str(), recovered.end_reason.as_str()), ("idle", "worker_restarted"));
        assert_eq!(head.state.store.events_after("c-valid", 0, i64::MAX).await.unwrap().len(), 2);
        head.shutdown().await.unwrap();
    }
}
