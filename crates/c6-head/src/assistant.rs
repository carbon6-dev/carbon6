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
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, history_from_events, run_turn};
use c6_api as api;
use c6_core::{WorkerId, now_ms};
use c6_llm::{Message, Provider, Role, ToolSpec, model_info};
use c6_proto::{
    CredentialRequest, ErrorEvent, HeadMsg, ModelSwitched, SessionActivity, SessionCommand, SessionEvent, SessionEventBatch,
    SessionStarted, TurnEndReason, TurnEnded, TurnStarted, ephemeral_event, head_msg, session_command, session_event,
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

/// Marks a request the head makes to itself (the assistant's calls): it is
/// you, already signed in.
#[derive(Clone, Copy)]
pub struct InternalCall;

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
}

/// Starts the head's own worker connection (after the router is built).
pub async fn start(st: Arc<HeadState>, router: axum::Router) -> anyhow::Result<()> {
    let _ = st.assistant.router.set(router);
    let id = head_worker(&st).await?;
    let _ = st.assistant.head_id.set(id);
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
    Answer { values: serde_json::Map<String, Value>, cancelled: bool },
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
        self.st.hub.publish(&self.session_id, crate::convert::ephemeral(&kind));
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
        });
        let conv = Conversation { history: history_from_events(&c6_agent::effective_events(&events)), context_tokens: 0 };
        let mut actor = Actor {
            compact: crate::prompts::compact(&st, crate::realms::ROOT).await,
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
                    actor.sink.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: false }));
                    actor.conv = Some(match done {
                        Ok(Some(c)) => c,
                        _ => Conversation { history: history_from_events(&c6_agent::effective_events(
                            &actor.st.store.events_after(&actor.session_id, 0, i64::MAX).await?)), context_tokens: 0 },
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
                let info = info_of(&self.model);
                self.sink
                    .append(session_event::Kind::Started(SessionStarted {
                        command_id: cmd.command_id.clone(),
                        model: Some(self.model.clone()),
                        cwd: String::new(),
                        system_prompt: self.system_prompt.clone(),
                        tools: self.env.tools().iter().map(|t| t.spec().name).collect(),
                        tool_profile: "assistant".into(),
                        context_window: info.context_window,
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
                    let context_window = info_of(&m).context_window;
                    self.sink.append(session_event::Kind::ModelSwitched(ModelSwitched { model: Some(m.clone()), context_window })).await?;
                    if let Some(conv) = &mut self.conv {
                        conv.history = c6_agent::portable_messages(std::mem::take(&mut conv.history));
                    }
                    self.model = m;
                }
                self.inbox.lock().unwrap().push(PendingInput {
                    command_id: cmd.command_id,
                    text: input.text,
                    attachments: input.attachments,
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
        let provider = Provider::from_kind(&self.model.connection_kind).unwrap_or(Provider::OpenAiChat);
        let cfg = TurnConfig {
            model: self.model.model.clone(),
            system_prompt: self.system_prompt.clone(),
            max_output_tokens: None,
            cache_key: self.session_id.clone(),
            effort: Some(self.model.effort.clone()).filter(|e| !e.is_empty()).or_else(|| info.default_effort.map(str::to_owned)),
            context_window: info.context_window,
            web_search: false,
            compact: self.compact.clone(),
        };
        let _ = provider;
        let mut conv = self.conv.take().unwrap_or(Conversation { history: Vec::new(), context_tokens: 0 });
        let (st, sink, inbox, env, c, connection_id) =
            (self.st.clone(), self.sink.clone(), self.inbox.clone(), self.env.clone(), cancel.clone(), self.model.connection_id.clone());
        sink.ephemeral(ephemeral_event::Kind::Activity(SessionActivity { running: true }));
        let task = tokio::spawn(async move {
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
                let grant = crate::sessions::credential(
                    &st,
                    sink.head_id,
                    CredentialRequest { connection_id: connection_id.clone(), ..Default::default() },
                )
                .await;
                let client = if grant.error.is_empty() {
                    c6_llm::client_for(&grant.kind, &grant.base_url, &grant.bearer, grant.headers).map_err(|e| e.to_string())
                } else {
                    Err(grant.error)
                };
                let client = match client {
                    Ok(c) => c,
                    Err(e) => {
                        dyn_sink.append(session_event::Kind::TurnStarted(TurnStarted { turn })).await?;
                        dyn_sink.append(session_event::Kind::Error(ErrorEvent { message: format!("cannot reach the model: {e}") })).await?;
                        dyn_sink.append(session_event::Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Error as i32 })).await?;
                        return anyhow::Ok(());
                    }
                };
                let tools = env.tools();
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
}

impl Env {
    fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> =
            vec![Arc::new(Discover), Arc::new(Describe), Arc::new(Call { env: self.clone() }), Arc::new(Ask { env: self.clone() })];
        if self.st.registry.memex().is_some() {
            tools.push(Arc::new(c6_tools::memex::MemexSearch::new(false)));
            tools.push(Arc::new(c6_tools::memex::MemexGet::new(false)));
        }
        tools
    }

    /// Waits for your answer to `call_id` (or the turn's end).
    async fn wait(&self, call_id: &str, cancel: &CancellationToken) -> Option<Reply> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(call_id.to_owned(), tx);
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
        let mut ask: Option<(String, String)> = None;
        if op.effect == Effect::Destructive {
            ask = Some(("assistant: destructive operations".into(), format!("{}: {}", op.name, op.summary)));
        } else if op.replaces.is_some() {
            match removed_entries(st, op, &args).await {
                Ok(gone) if !gone.is_empty() => {
                    ask = Some(("assistant: removes entries".into(), format!("{} removes {}", op.name, gone.join(", "))));
                }
                Ok(_) => {}
                Err(e) => return ToolOutput::err(format!("could not read the current list: {e:#}")),
            }
        }
        match policy_for(st, &self.env.labels, &shown).await {
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
            let approval = |state: &str| {
                session_event::Kind::Approval(c6_proto::Approval {
                    call_id: ctx.call_id.clone(),
                    state: state.into(),
                    tool: "c6_call".into(),
                    input: crate::util_clip(&format!("{name} {shown}"), 4000),
                    policy: policy.clone(),
                    message: message.clone(),
                })
            };
            if let Err(e) = self.env.sink.append(approval("requested")).await {
                return ToolOutput::err(format!("could not ask for approval: {e:#}"));
            }
            let reply = self.env.wait(&ctx.call_id, &ctx.cancel).await;
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
        let result = execute(st, op, args, "assistant").await;
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
        "workers.enrollments.create" => {
            "On the new machine: c6w enroll <token> then c6w run (a worker), or c6m enroll <token> then c6m run (the memex). One use, valid for an hour."
        }
        "auth.tokens.create" => "For the c6 CLI or a script (Authorization: Bearer <token>). Copy it now: it is not shown again.",
        "auth.codes.create" => "Enter it on the other device's sign-in page. Valid for a few minutes.",
        "triggers.rotate" => "The old URL no longer works.",
        _ => "Copy it now: it is not shown again.",
    }
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
async fn removed_entries(st: &HeadState, op: &Op, args: &Value) -> anyhow::Result<Vec<String>> {
    let Some(get) = op.replaces.and_then(catalog::find) else { return Ok(Vec::new()) };
    let current = execute(st, get, json!({}), "assistant").await?;
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
pub async fn execute(st: &HeadState, op: &Op, mut args: Value, made_by: &str) -> anyhow::Result<Value> {
    if !args.is_object() {
        args = json!({});
    }
    match &op.via {
        Via::Memex { method } => {
            args["made_by"] = json!(made_by);
            // As you, on your home knowledge.
            Ok(crate::memex::call_in(st, crate::realms::HOME, method, args).await?)
        }
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
            req.extensions_mut().insert(InternalCall);
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
                match choices(&self.env.st, from, f["choices_from"]["input"].clone()).await {
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
        if let Err(e) = self.env.sink.append(question("asked", &Value::Null)).await {
            return ToolOutput::err(format!("could not ask: {e:#}"));
        }
        match self.env.wait(&ctx.call_id, &ctx.cancel).await {
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
async fn choices(st: &HeadState, op: &str, input: Value) -> anyhow::Result<Vec<api::Choice>> {
    let op = catalog::find(op).ok_or_else(|| anyhow::anyhow!("no operation {op}"))?;
    anyhow::ensure!(op.effect == Effect::Read, "{} is not a read operation", op.name);
    let v = execute(st, op, input, "assistant").await?;
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
vision, transcription) in settings.memex, yourself in settings.assistant. A connection's available models: connections.models.
- Be brief. After acting, say what you did and anything the user should do next (e.g. run a command on another machine).
- You cannot run commands or read files yourself. For work on a machine, start a session on a worker (sessions.create) and \
tell the user; for something recurring, create an automation or workflow.

Areas of carbon6:
{{areas}}
{{context}}Today is {{today}}.";

/// The assistant's instructions, from its prompt.
pub async fn prompt(st: &HeadState, context: &str) -> String {
    let areas: String = catalog::areas().iter().map(|(a, d)| format!("- {a}: {d}\n")).collect();
    let context = if context.trim().is_empty() { String::new() } else { format!("The user is looking at: {}\n\n", context.trim()) };
    let today = chrono::Utc::now().format("%A %-d %B %Y").to_string();
    let template = crate::prompts::get(st, "assistant.base", crate::realms::ROOT).await;
    c6_api::prompts::fill(&template, &[("areas", &areas), ("context", &context), ("today", &today)])
}

/// The model for a new conversation: the setting, else the routes for the
/// label `assistant`, else the default.
pub async fn model(st: &HeadState) -> anyhow::Result<(String, String)> {
    let s: api::AssistantSettings = serde_json::from_str(&st.store.setting(SETTING).await?.unwrap_or_default()).unwrap_or_default();
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
        if c.kind == "claude_code" {
            return Err(bad_request("the assistant runs in the head and calls models directly; choose an API connection"));
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
