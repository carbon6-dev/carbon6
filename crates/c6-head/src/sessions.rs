//! Session lifecycle on the head: creation, input, sync from workers, and
//! live fan-out to UI subscribers.

use std::collections::HashMap;
use std::sync::Mutex;

use c6_api as api;
use c6_core::{WorkerId, now_ms};
use c6_proto::{
    CommandRejected, CreateSession, CredentialGrant, CredentialRequest, EphemeralEvent, ModelSpec, SessionCommand, SessionEvent,
    SessionEventBatch, UserInput, session_command, session_event,
};
use prost::Message;
use tokio::sync::broadcast;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::store::{NewCommand, SessionRow};
use crate::{HeadState, bad_request, convert, not_found};

/// Settings keys, included in every new session's configuration.
pub const MCP_SERVERS: &str = "mcp_servers";
pub const SKILLS: &str = "skills";

/// Per-session broadcast channels for stream subscribers.
#[derive(Default)]
pub struct LiveHub {
    channels: Mutex<HashMap<String, broadcast::Sender<api::StreamMsg>>>,
    /// Session → owning worker, to vet ephemeral events cheaply.
    owners: Mutex<HashMap<String, WorkerId>>,
    /// Last restore per session, to rate-limit them.
    restores: Mutex<HashMap<String, std::time::Instant>>,
}

impl LiveHub {
    fn may_restore(&self, session_id: &str) -> bool {
        let mut r = self.restores.lock().unwrap();
        let now = std::time::Instant::now();
        match r.get(session_id) {
            Some(t) if now.duration_since(*t) < std::time::Duration::from_secs(60) => false,
            _ => {
                r.insert(session_id.to_owned(), now);
                true
            }
        }
    }

    pub fn subscribe(&self, session_id: &str) -> broadcast::Receiver<api::StreamMsg> {
        self.channels.lock().unwrap().entry(session_id.to_owned()).or_insert_with(|| broadcast::channel(1024).0).subscribe()
    }

    fn publish(&self, session_id: &str, msg: api::StreamMsg) {
        let mut channels = self.channels.lock().unwrap();
        if let Some(tx) = channels.get(session_id)
            && tx.send(msg).is_err()
        {
            channels.remove(session_id); // no subscribers left
        }
    }
}

pub async fn create(st: &HeadState, req: api::CreateSession) -> anyhow::Result<api::Session> {
    let worker_id = WorkerId(req.worker_id);
    let worker = st.store.worker(worker_id).await?.ok_or_else(|| bad_request("no such worker"))?;
    if worker.revoked_at.is_some() {
        return Err(bad_request("worker is revoked"));
    }
    let effort = check_effort(req.effort.clone())?;
    if req.cwd.trim().is_empty() || req.model.trim().is_empty() {
        return Err(bad_request("cwd and model are required"));
    }
    let now = now_ms();
    let prompt = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let title = req.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(prompt));
    let row = SessionRow {
        id: Uuid::now_v7().to_string(),
        worker_id: worker_id.to_string(),
        title,
        cwd: req.cwd.trim().to_owned(),
        connection_id: req.connection_id.clone(),
        model: req.model.trim().to_owned(),
        status: "starting".into(),
        error: None,
        effort,
        parent_id: None,
        parent_call_id: None,
        fork_seq: None,
        created_at: now,
        updated_at: now,
        last_seq: 0,
        archived: 0,
    };
    let spec = session_spec(st, &row, req.worktree, Vec::new()).await?;
    insert(st, row, spec, prompt).await
}

/// A side chat on `parent_id`, branching after event `anchor_seq`.
pub async fn create_side_chat(st: &HeadState, parent_id: &str, req: api::CreateSideChat) -> anyhow::Result<api::Session> {
    let parent = st.store.session(parent_id).await?.ok_or_else(|| not_found("no such session"))?;
    if parent.parent_call_id.is_some() {
        return Err(bad_request("subagent sessions cannot have side chats"));
    }
    if parent.status == "failed" {
        return Err(bad_request("the session failed to start"));
    }
    let prompt = req.prompt.trim();
    if prompt.is_empty() || req.model.trim().is_empty() {
        return Err(bad_request("a question and a model are required"));
    }
    let effort = check_effort(req.effort.clone())?;
    // The parent's real directory (its worktree, if it has one).
    let started = st.store.events_after(parent_id, 0, 1).await?;
    let cwd = match started.first().and_then(|e| e.kind.as_ref()) {
        Some(session_event::Kind::Started(s)) => s.cwd.clone(),
        _ => return Err(bad_request("the session has not started yet")),
    };
    let now = now_ms();
    let row = SessionRow {
        id: Uuid::now_v7().to_string(),
        worker_id: parent.worker_id.clone(),
        title: default_title(Some(prompt)),
        cwd,
        connection_id: req.connection_id.clone(),
        model: req.model.trim().to_owned(),
        status: "starting".into(),
        error: None,
        effort,
        parent_id: Some(parent.id.clone()),
        parent_call_id: None,
        fork_seq: Some(req.anchor_seq as i64),
        created_at: now,
        updated_at: now,
        last_seq: 0,
        archived: 0,
    };
    let seed = fork_seed(st, &parent, req.anchor_seq).await?;
    let spec = session_spec(st, &row, false, seed).await?;
    insert(st, row, spec, Some(prompt)).await
}

fn check_effort(effort: Option<String>) -> anyhow::Result<Option<String>> {
    const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    let effort = effort.filter(|e| !e.is_empty());
    if let Some(e) = &effort
        && !EFFORTS.contains(&e.as_str())
    {
        return Err(bad_request(format!("effort must be one of {}", EFFORTS.join(", "))));
    }
    Ok(effort)
}

/// Added to the base prompt of side chats.
const SIDE_CHAT_NOTE: &str = "This is a side chat: the user branched off the conversation above to ask about it. \
The main session continues on its own and does not see this chat. Answer the user's questions here; \
look at files and run read-only commands as needed, but do not change files unless the user asks you to.";

/// The worker-side spec of a session, from its row. `seed` is the branched-
/// from conversation of a side chat.
async fn session_spec(st: &HeadState, row: &SessionRow, worktree: bool, seed: Vec<SessionEvent>) -> anyhow::Result<CreateSession> {
    let connection = st.store.connection(&row.connection_id).await?.ok_or_else(|| bad_request("no such connection"))?;
    let base_prompt =
        if row.fork_seq.is_some() { format!("{}\n\n{SIDE_CHAT_NOTE}", c6_agent::BASE_PROMPT) } else { c6_agent::BASE_PROMPT.into() };
    Ok(CreateSession {
        model: Some(ModelSpec {
            connection_id: row.connection_id.clone(),
            model: row.model.clone(),
            max_output_tokens: 0,
            effort: row.effort.clone().unwrap_or_default(),
            connection_kind: connection.kind.clone(),
        }),
        cwd: row.cwd.clone(),
        base_prompt,
        worktree,
        mcp_servers_json: st.store.setting(MCP_SERVERS).await?.unwrap_or_else(|| "[]".into()),
        skills_json: st.store.setting(SKILLS).await?.unwrap_or_else(|| "[]".into()),
        seed,
    })
}

/// The conversation a side chat branching from `session` after `upto` sees:
/// the session's own seed (its ancestors), then its events up to `upto`,
/// plus the results of tool calls made by then, so every call is answered.
async fn fork_seed(st: &HeadState, session: &SessionRow, upto: u64) -> anyhow::Result<Vec<SessionEvent>> {
    let mut seed = match (&session.parent_id, session.fork_seq) {
        (Some(p), Some(f)) => {
            let parent = st.store.session(p).await?.ok_or_else(|| not_found("a parent session is missing"))?;
            Box::pin(fork_seed(st, &parent, f as u64)).await?
        }
        _ => Vec::new(),
    };
    let mut calls = std::collections::HashSet::new();
    for ev in st.store.events_after(&session.id, 0, i64::MAX).await? {
        match &ev.kind {
            Some(session_event::Kind::Assistant(a)) if ev.seq <= upto => {
                for b in &a.blocks {
                    if let Some(c6_proto::content_block::Block::ToolUse(u)) = &b.block {
                        calls.insert(u.id.clone());
                    }
                }
                seed.push(ev);
            }
            Some(session_event::Kind::ToolResult(r)) if ev.seq <= upto || calls.contains(&r.call_id) => seed.push(ev),
            _ if ev.seq <= upto => seed.push(ev),
            _ => {}
        }
    }
    Ok(seed)
}

/// Stores a new session with its Create command (and first input) and
/// tells the worker.
async fn insert(st: &HeadState, row: SessionRow, spec: CreateSession, prompt: Option<&str>) -> anyhow::Result<api::Session> {
    let worker_id: WorkerId = row.worker_id.parse()?;
    let session_id = row.id.clone();
    let create_id = Uuid::now_v7().to_string();
    let create =
        SessionCommand { command_id: create_id.clone(), session_id: session_id.clone(), kind: Some(session_command::Kind::Create(spec)) };
    let mut commands = vec![NewCommand { id: &create_id, session_id: &session_id, worker_id, data: create.encode_to_vec(), text: None }];
    let input_id = Uuid::now_v7().to_string();
    if let Some(text) = prompt {
        commands.push(NewCommand {
            id: &input_id,
            session_id: &session_id,
            worker_id,
            data: input_command(&input_id, &session_id, text),
            text: Some(text),
        });
    }
    st.store.create_session(&row, &commands).await?;
    st.hub.owners.lock().unwrap().insert(session_id.clone(), worker_id);
    st.registry.deliver(worker_id);
    st.registry.bump();
    Ok(convert::session(&row))
}

fn default_title(prompt: Option<&str>) -> String {
    let Some(p) = prompt else { return "New session".into() };
    let line = p.lines().next().unwrap_or(p).trim();
    if line.chars().count() <= 60 { line.to_owned() } else { format!("{}…", line.chars().take(59).collect::<String>()) }
}

fn input_command(command_id: &str, session_id: &str, text: &str) -> Vec<u8> {
    switch_command(command_id, session_id, text, None)
}

/// An input that also switches the session to `model`.
fn switch_command(command_id: &str, session_id: &str, text: &str, model: Option<ModelSpec>) -> Vec<u8> {
    SessionCommand {
        command_id: command_id.to_owned(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::Input(UserInput { text: text.to_owned(), model })),
    }
    .encode_to_vec()
}

pub async fn send_input(st: &HeadState, session_id: &str, req: &api::SendMessage) -> anyhow::Result<api::QueuedInput> {
    let text = req.text.trim();
    if text.is_empty() {
        return Err(bad_request("message is empty"));
    }
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    if session.parent_call_id.is_some() {
        return Err(bad_request("subagent sessions run once and cannot take messages"));
    }
    if session.status == "failed" {
        return Err(bad_request(format!("session failed to start: {}", session.error.unwrap_or_default())));
    }
    let model = match (&req.connection_id, &req.model) {
        (Some(c), Some(m)) if !m.trim().is_empty() => Some(model_switch(st, &session, c, m.trim(), req.effort.clone()).await?),
        (None, None) => None,
        _ => return Err(bad_request("a model switch needs both a connection and a model")),
    };
    let worker_id: WorkerId = session.worker_id.parse()?;
    let id = Uuid::now_v7().to_string();
    let data = switch_command(&id, session_id, text, model);
    st.store.enqueue_command(&NewCommand { id: &id, session_id, worker_id, data, text: Some(text) }).await?;
    publish_queue(st, session_id).await;
    st.registry.deliver(worker_id);
    Ok(api::QueuedInput { command_id: id, text: text.to_owned(), created_at: now_ms(), rejected: None })
}

/// The spec for switching `session` to another model. Claude Code runs a
/// different agent loop, so a session cannot switch into or out of it.
async fn model_switch(
    st: &HeadState,
    session: &SessionRow,
    connection_id: &str,
    model: &str,
    effort: Option<String>,
) -> anyhow::Result<ModelSpec> {
    let effort = check_effort(effort)?;
    let to = st.store.connection(connection_id).await?.ok_or_else(|| bad_request("no such connection"))?;
    let from = st.store.connection(&session.connection_id).await?.map(|c| c.kind).unwrap_or_default();
    if (to.kind == "claude_code") != (from == "claude_code") {
        return Err(bad_request("a session cannot switch between Claude Code and other connections"));
    }
    Ok(ModelSpec {
        connection_id: connection_id.to_owned(),
        model: model.to_owned(),
        max_output_tokens: 0,
        effort: effort.unwrap_or_default(),
        connection_kind: to.kind,
    })
}

/// Interrupts are not queued: they only make sense while the worker is
/// connected and running the turn.
pub async fn interrupt(st: &HeadState, session_id: &str) -> anyhow::Result<bool> {
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    let cmd = SessionCommand {
        command_id: Uuid::now_v7().to_string(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::Interrupt(Default::default())),
    };
    Ok(st.registry.send_command(session.worker_id.parse()?, cmd))
}

pub async fn publish_queue(st: &HeadState, session_id: &str) {
    match st.store.queued_inputs(session_id).await {
        Ok(rows) => st.hub.publish(session_id, api::StreamMsg::Queue { queued: rows.iter().map(convert::queued).collect() }),
        Err(e) => warn!("loading queue: {e:#}"),
    }
}

/// Stores a batch from a worker and returns the cumulative ack, if any.
pub async fn ingest(st: &HeadState, worker_id: WorkerId, batch: SessionEventBatch) -> anyhow::Result<Option<u64>> {
    let Some(ing) = st.store.ingest(worker_id, &batch.session_id, &batch.events).await? else {
        debug!(session = %batch.session_id, worker = %worker_id, "events for unknown session ignored");
        return Ok(None);
    };
    for ev in &ing.events {
        if let Some(event) = convert::event(ev) {
            st.hub.publish(&batch.session_id, api::StreamMsg::Event { event });
        }
    }
    if !ing.done_commands.is_empty() {
        publish_queue(st, &batch.session_id).await;
    }
    if !ing.events.is_empty() {
        st.registry.bump();
    }
    let ended: Vec<u64> =
        ing.events.iter().filter(|e| matches!(e.kind, Some(c6_proto::session_event::Kind::TurnEnded(_)))).map(|e| e.seq).collect();
    if st.otlp.is_some() && !ended.is_empty() {
        export_turns(st, worker_id, &batch.session_id, ended).await;
    }
    Ok(Some(ing.last_seq))
}

async fn export_turns(st: &HeadState, worker_id: WorkerId, session_id: &str, ended: Vec<u64>) {
    let Some(otlp) = &st.otlp else { return };
    let (Ok(Some(session)), worker) = (st.store.session(session_id).await, st.store.worker(worker_id).await) else { return };
    let provider = match st.store.connection(&session.connection_id).await {
        Ok(Some(c)) => match c.kind.as_str() {
            "anthropic" | "claude_code" => "anthropic",
            "openai" | "chatgpt" | "openai_compat" => "openai",
            other => other,
        }
        .to_owned(),
        _ => "unknown".to_owned(),
    };
    let worker_name = worker.ok().flatten().map(|w| w.name).unwrap_or_default();
    for seq in ended {
        match st.store.turn_events(session_id, seq).await {
            Ok(events) => {
                if let Some(req) = crate::telemetry::turn_spans(&session, &provider, &worker_name, &events) {
                    otlp.export(req).await;
                }
            }
            Err(e) => warn!("loading turn for telemetry: {e:#}"),
        }
    }
}

pub async fn on_ephemeral(st: &HeadState, worker_id: WorkerId, ev: EphemeralEvent) {
    let owner = st.hub.owners.lock().unwrap().get(&ev.session_id).copied();
    let owner = match owner {
        Some(o) => Some(o),
        None => match st.store.session(&ev.session_id).await {
            Ok(Some(s)) => s.worker_id.parse().ok().inspect(|o| {
                st.hub.owners.lock().unwrap().insert(ev.session_id.clone(), *o);
            }),
            _ => None,
        },
    };
    if owner != Some(worker_id) {
        return;
    }
    if let Some(kind) = &ev.kind {
        st.hub.publish(&ev.session_id, convert::ephemeral(kind));
    }
}

pub async fn on_rejected(st: &HeadState, worker_id: WorkerId, r: CommandRejected) -> anyhow::Result<()> {
    // The worker lost (or trimmed) this session: send it the log, then the
    // input again. At most once a minute per session, so a worker that
    // cannot restore does not loop.
    if r.reason.contains("unknown session") && st.hub.may_restore(&r.session_id) {
        match restore(st, worker_id, &r.session_id, &r.command_id).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => warn!(session = %r.session_id, "restoring session: {e:#}"),
        }
    }
    if let Some(session_id) = st.store.reject_command(worker_id, &r.command_id, &r.reason).await? {
        warn!(session = %session_id, reason = %r.reason, "worker rejected command");
        publish_queue(st, &session_id).await;
        st.registry.bump();
    }
    Ok(())
}

/// Queues a `Restore` and a fresh copy of the rejected input, in that order.
async fn restore(st: &HeadState, worker_id: WorkerId, session_id: &str, command_id: &str) -> anyhow::Result<bool> {
    let Some(session) = st.store.session(session_id).await? else { return Ok(false) };
    if session.status == "failed" || session.parent_call_id.is_some() || session.worker_id != worker_id.to_string() {
        return Ok(false);
    }
    let Some((text, data)) = st.store.pending_input(command_id).await? else { return Ok(false) };
    // Keep a model switch the input asked for.
    let model = SessionCommand::decode(data.as_slice()).ok().and_then(|c| match c.kind {
        Some(session_command::Kind::Input(i)) => i.model,
        _ => None,
    });
    let events = st.store.events_after(session_id, 0, i64::MAX).await?;
    if events.is_empty() {
        return Ok(false);
    }
    let seed = match (&session.parent_id, session.fork_seq) {
        (Some(p), Some(f)) => match st.store.session(p).await? {
            Some(parent) => fork_seed(st, &parent, f as u64).await?,
            None => return Ok(false),
        },
        _ => Vec::new(),
    };
    let spec = session_spec(st, &session, false, seed).await?;
    let restore_id = Uuid::now_v7().to_string();
    let cmd = SessionCommand {
        command_id: restore_id.clone(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::Restore(c6_proto::Restore { spec: Some(spec), events })),
    };
    let input_id = Uuid::now_v7().to_string();
    st.store
        .replace_with_restore(
            command_id,
            &NewCommand { id: &restore_id, session_id, worker_id, data: cmd.encode_to_vec(), text: None },
            &NewCommand {
                id: &input_id,
                session_id,
                worker_id,
                data: switch_command(&input_id, session_id, &text, model),
                text: Some(&text),
            },
        )
        .await?;
    warn!(session = %session_id, "worker no longer had this session; restoring it from the head's log");
    publish_queue(st, session_id).await;
    st.registry.deliver(worker_id);
    Ok(true)
}

pub async fn credential(st: &HeadState, req: CredentialRequest) -> CredentialGrant {
    let mut grant = CredentialGrant { request_id: req.request_id, connection_id: req.connection_id.clone(), ..Default::default() };
    let conn = match st.store.connection(&req.connection_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            grant.error = "no such connection".into();
            return grant;
        }
        Err(e) => {
            warn!("loading connection: {e:#}");
            grant.error = "internal error".into();
            return grant;
        }
    };
    grant.kind = conn.kind.clone();
    grant.base_url = conn.base_url.clone();
    if conn.kind == "chatgpt" {
        match chatgpt_tokens(st, &conn.id).await {
            Ok(t) => {
                grant.bearer = t.access_token;
                grant.expires_at_ms = t.expires_at_ms;
            }
            Err(e) => {
                warn!(connection = %conn.id, "ChatGPT credentials: {e:#}");
                grant.error = format!("ChatGPT sign-in needs attention: {e}");
            }
        }
        return grant;
    }
    match conn.api_key.as_deref().map(|k| st.secrets.decrypt(k)).transpose() {
        Ok(k) => grant.bearer = k.unwrap_or_default(),
        Err(e) => {
            warn!(connection = %conn.id, "decrypting API key: {e:#}");
            grant.error = "credential unavailable".into();
        }
    }
    grant
}

/// Current ChatGPT tokens for a connection, refreshed here (never on
/// workers) when close to expiry.
pub async fn chatgpt_tokens(st: &HeadState, connection_id: &str) -> anyhow::Result<crate::oauth::ChatGptTokens> {
    let lock = st.oauth.lock_for(connection_id);
    let _guard = lock.lock().await;
    // Read under the lock so a refresh that just finished is seen.
    let row = st.store.connection(connection_id).await?.ok_or_else(|| not_found("no such connection"))?;
    let blob = row.api_key.ok_or_else(|| bad_request("connection has no stored sign-in"))?;
    let tokens: crate::oauth::ChatGptTokens = serde_json::from_str(&st.secrets.decrypt(&blob)?)?;
    if !crate::oauth::OAuth::needs_refresh(&tokens) {
        return Ok(tokens);
    }
    let refreshed = st.oauth.refresh(&st.http, tokens).await?;
    st.store.set_connection_secret(connection_id, &st.secrets.encrypt(&serde_json::to_string(&refreshed)?)?).await?;
    Ok(refreshed)
}
