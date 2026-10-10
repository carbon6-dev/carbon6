//! Session lifecycle on the head: creation, input, sync from workers, and
//! live fan-out to UI subscribers.

use std::collections::HashMap;
use std::sync::Mutex;

use c6_api as api;
use c6_core::{WorkerId, now_ms};
use c6_proto::{
    CommandRejected, CreateSession, CredentialGrant, CredentialRequest, EphemeralEvent, ModelSpec, SessionCommand, SessionEventBatch,
    UserInput, session_command,
};
use prost::Message;
use tokio::sync::broadcast;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::store::{NewCommand, SessionRow};
use crate::{HeadState, bad_request, convert, not_found, prompts};

/// Per-session broadcast channels for stream subscribers.
#[derive(Default)]
pub struct LiveHub {
    channels: Mutex<HashMap<String, broadcast::Sender<api::StreamMsg>>>,
    /// Session → owning worker, to vet ephemeral events cheaply.
    owners: Mutex<HashMap<String, WorkerId>>,
}

impl LiveHub {
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
    st.store.connection(&req.connection_id).await?.ok_or_else(|| bad_request("no such connection"))?;
    if req.cwd.trim().is_empty() || req.model.trim().is_empty() {
        return Err(bad_request("cwd and model are required"));
    }

    let session_id = Uuid::now_v7().to_string();
    let now = now_ms();
    let prompt = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let title = req.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(prompt));
    let row = SessionRow {
        id: session_id.clone(),
        worker_id: worker_id.to_string(),
        title,
        cwd: req.cwd.trim().to_owned(),
        connection_id: req.connection_id.clone(),
        model: req.model.trim().to_owned(),
        status: "starting".into(),
        error: None,
        created_at: now,
        updated_at: now,
        last_seq: 0,
        archived: 0,
    };

    let create_id = Uuid::now_v7().to_string();
    let create = SessionCommand {
        command_id: create_id.clone(),
        session_id: session_id.clone(),
        kind: Some(session_command::Kind::Create(CreateSession {
            model: Some(ModelSpec { connection_id: row.connection_id.clone(), model: row.model.clone(), max_output_tokens: 0 }),
            cwd: row.cwd.clone(),
            base_prompt: prompts::BASE_PROMPT.into(),
        })),
    };
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
    SessionCommand {
        command_id: command_id.to_owned(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::Input(UserInput { text: text.to_owned() })),
    }
    .encode_to_vec()
}

pub async fn send_input(st: &HeadState, session_id: &str, text: &str) -> anyhow::Result<api::QueuedInput> {
    let text = text.trim();
    if text.is_empty() {
        return Err(bad_request("message is empty"));
    }
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    if session.status == "failed" {
        return Err(bad_request(format!("session failed to start: {}", session.error.unwrap_or_default())));
    }
    let worker_id: WorkerId = session.worker_id.parse()?;
    let id = Uuid::now_v7().to_string();
    st.store
        .enqueue_command(&NewCommand { id: &id, session_id, worker_id, data: input_command(&id, session_id, text), text: Some(text) })
        .await?;
    publish_queue(st, session_id).await;
    st.registry.deliver(worker_id);
    Ok(api::QueuedInput { command_id: id, text: text.to_owned(), created_at: now_ms(), rejected: None })
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
    Ok(Some(ing.last_seq))
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
    if let Some(session_id) = st.store.reject_command(worker_id, &r.command_id, &r.reason).await? {
        warn!(session = %session_id, reason = %r.reason, "worker rejected command");
        publish_queue(st, &session_id).await;
        st.registry.bump();
    }
    Ok(())
}

pub async fn credential(st: &HeadState, req: CredentialRequest) -> CredentialGrant {
    let mut grant = CredentialGrant { request_id: req.request_id, connection_id: req.connection_id.clone(), ..Default::default() };
    match st.store.connection(&req.connection_id).await {
        Ok(Some(c)) => {
            let bearer = match c.api_key.as_deref().map(|k| st.secrets.decrypt(k)).transpose() {
                Ok(k) => k.unwrap_or_default(),
                Err(e) => {
                    warn!(connection = %c.id, "decrypting API key: {e:#}");
                    grant.error = "credential unavailable".into();
                    return grant;
                }
            };
            grant.kind = c.kind;
            grant.base_url = c.base_url;
            grant.bearer = bearer;
        }
        Ok(None) => grant.error = "no such connection".into(),
        Err(e) => {
            warn!("loading connection: {e:#}");
            grant.error = "internal error".into();
        }
    }
    grant
}
