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
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::store::{NewCommand, SessionRow};
use crate::{HeadState, bad_request, convert, not_found};

/// Settings keys, included in every new session's configuration.
pub const MCP_SERVERS: &str = "mcp_servers";
pub const SKILLS: &str = "skills";
pub const HOOKS: &str = "hooks";
pub const COMMANDS: &str = "commands";
pub const AGENTS: &str = "agents";
pub const WEB_SEARCH: &str = "web_search";
pub const POLICIES: &str = "policies";
pub const ROUTES: &str = "routes";

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

    pub(crate) fn publish(&self, session_id: &str, msg: api::StreamMsg) {
        let mut channels = self.channels.lock().unwrap();
        if let Some(tx) = channels.get(session_id)
            && tx.send(msg).is_err()
        {
            channels.remove(session_id); // no subscribers left
        }
    }
}

pub async fn create(st: &HeadState, req: api::CreateSession) -> anyhow::Result<api::Session> {
    create_as(st, req, "").await
}

/// A session acting as an app instance in its realm (docs/apps.md §5.3),
/// or as the realm (`instance` empty). Only the head starts sessions as an
/// instance: from its templates, triggers and operations.
pub async fn create_as(st: &HeadState, req: api::CreateSession, instance: &str) -> anyhow::Result<api::Session> {
    let worker_id = WorkerId(req.worker_id);
    let worker = st.store.worker(worker_id).await?.ok_or_else(|| bad_request("no such worker"))?;
    if worker.revoked_at.is_some() {
        return Err(bad_request("worker is revoked"));
    }
    if worker.role == "head" {
        return Err(bad_request("that is the head itself; ask the assistant instead (POST /api/v1/assistant)"));
    }
    if worker.role == "memex" {
        return Err(bad_request(format!("{} is the memex, which runs no sessions; pick a worker", worker.name)));
    }
    let mut effort = check_effort(req.effort.clone())?;
    // No folder: the worker gives the session a scratch workspace.
    if req.cwd.trim().is_empty() && req.worktree && req.ssh.is_none() {
        return Err(bad_request("a git worktree needs a folder"));
    }
    let mut ssh = req.ssh.as_deref().map(str::trim).unwrap_or("").to_owned();
    // A host from the inventory (by name or target): its target and labels.
    let mut req_labels = req.labels.clone();
    if !ssh.is_empty()
        && let Some(h) = crate::hosts::lookup(st, &ssh).await?
    {
        ssh = h.target.trim().to_owned();
        req_labels.extend(h.labels.iter().cloned());
    }
    if !ssh.is_empty() {
        if ssh.starts_with('-') || ssh.contains(char::is_whitespace) {
            return Err(bad_request("an SSH target is user@host or a host alias"));
        }
        if req.worktree {
            return Err(bad_request("git worktrees are for folders on the worker, not SSH targets"));
        }
    }
    let labels = check_labels(&req_labels)?;
    let realm = req.realm.clone().filter(|r| !r.trim().is_empty()).unwrap_or_else(|| crate::realms::HOME.to_owned());
    crate::realms::check_for_sessions(st, &realm).await?;
    if !instance.is_empty() && crate::instances::get(&st.store.pool, &realm, instance).await?.is_none() {
        return Err(bad_request(format!("no app {instance:?} is installed in realm {realm}")));
    }
    if !crate::realms::available(&worker.realms, &realm) {
        return Err(bad_request(format!("{} is not available to realm {realm}", worker.name)));
    }
    let cwd = req.cwd.trim().to_owned();
    let (connection_id, model, subagent_model, subagent_connection_id, subagent_effort) = {
        let mut route_labels = labels.clone();
        route_labels.extend(api::labels::auto_labels(&worker.name, "", if cwd.starts_with('/') { &cwd } else { "" }, "", "", &ssh));
        if let Some(why) = crate::usage::blocked(st, &route_labels, &realm).await? {
            return Err(bad_request(format!("not started: {why}")));
        }
        let routed = route(st, &req.connection_id, &req.model, &route_labels, &realm).await?;
        if effort.is_none() {
            effort = check_effort(Some(routed.effort))?;
        }
        (routed.connection, routed.model, routed.subagent_model, routed.subagent_connection, routed.subagent_effort)
    };
    if let Some(c) = st.store.connection(&connection_id).await?
        && !crate::realms::available(&c.realms, &realm)
    {
        return Err(bad_request(format!("the connection {} is not available to realm {realm}", c.name)));
    }
    let now = now_ms();
    let prompt = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let files = attachments(&req.attachments)?;
    if prompt.is_none() && !files.is_empty() {
        return Err(bad_request("attachments need a prompt"));
    }
    let title = req.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(prompt));
    let row = SessionRow {
        id: Uuid::now_v7().to_string(),
        worker_id: worker_id.to_string(),
        title,
        cwd,
        connection_id,
        model,
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
        labels: serde_json::to_string(&labels)?,
        worker_name: worker.name.clone(),
        repo: String::new(),
        folder: String::new(),
        home: String::new(),
        ssh_target: ssh.clone(),
        awaiting: 0,
        subagent_model,
        subagent_connection_id,
        subagent_effort,
        realm,
        agent: String::new(),
        end_reason: String::new(),
        root_id: None,
        observed: false,
        instance: instance.to_owned(),
        colour: String::new(),
    };
    let mut spec = session_spec(st, &row, req.worktree, Vec::new()).await?;
    // Context from the memex, for what is known at creation.
    let mut brief_labels = labels.clone();
    brief_labels.extend(api::labels::auto_labels(
        &worker.name,
        "",
        if row.cwd.starts_with('/') { &row.cwd } else { "" },
        "",
        "",
        &row.ssh_target,
    ));
    spec.memex_brief = crate::memex::brief(st, &row.realm, &brief_labels, prompt.unwrap_or("")).await;
    insert(st, row, spec, prompt.map(|p| (p, files))).await
}

/// A conversation with the assistant (it runs in the head).
pub async fn create_assistant(st: &HeadState, req: api::AskAssistant) -> anyhow::Result<api::Session> {
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err(bad_request("ask something"));
    }
    let head_id = st.assistant.head_id().ok_or_else(|| bad_request("the assistant is starting; try again"))?;
    // The realm it works in: the one you are looking at, else root (all
    // realms, with administration).
    let realm = req.realm.as_deref().map(str::trim).filter(|r| !r.is_empty()).unwrap_or(crate::realms::ROOT).to_owned();
    let (connection_id, model) = crate::assistant::model(st, &realm).await?;
    let connection = st.store.connection(&connection_id).await?.ok_or_else(|| bad_request("no such connection"))?;
    if connection.kind == "claude_code" {
        return Err(bad_request(
            "the assistant calls models directly: choose an API connection for it (Settings → Assistant), not Claude Code",
        ));
    }
    if !crate::realms::exists(st, &realm).await? || realm == crate::realms::COMMONS {
        return Err(bad_request(format!("no realm {realm:?} for the assistant")));
    }
    let labels = vec![crate::assistant::LABEL.to_owned()];
    if let Some(why) = crate::usage::blocked(st, &labels, &realm).await? {
        return Err(bad_request(format!("not started: {why}")));
    }
    let now = now_ms();
    let row = SessionRow {
        id: Uuid::now_v7().to_string(),
        worker_id: head_id.to_string(),
        title: default_title(Some(prompt)),
        cwd: String::new(),
        connection_id: connection_id.clone(),
        model: model.clone(),
        status: "starting".into(),
        error: None,
        effort: None,
        parent_id: None,
        parent_call_id: None,
        fork_seq: None,
        created_at: now,
        updated_at: now,
        last_seq: 0,
        archived: 0,
        labels: serde_json::to_string(&labels)?,
        worker_name: "head".into(),
        repo: String::new(),
        folder: String::new(),
        home: String::new(),
        ssh_target: String::new(),
        awaiting: 0,
        subagent_model: String::new(),
        subagent_connection_id: String::new(),
        subagent_effort: String::new(),
        realm: realm.clone(),
        agent: String::new(),
        end_reason: String::new(),
        root_id: None,
        observed: false,
        instance: String::new(),
        colour: String::new(),
    };
    let spec = CreateSession {
        model: Some(ModelSpec { connection_id, model, max_output_tokens: 0, effort: String::new(), connection_kind: connection.kind }),
        base_prompt: crate::assistant::prompt(st, &req.context, &realm).await,
        labels,
        ..Default::default()
    };
    let session = insert(st, row, spec, Some((prompt, Vec::new()))).await?;
    crate::assistant::started(&session.id);
    Ok(session)
}

/// The connection and model a new session with these labels gets.
pub async fn route_for(st: &HeadState, connection: &str, model: &str, labels: &[String]) -> anyhow::Result<(String, String)> {
    let r = route(st, connection, model, labels, crate::realms::ROOT).await?;
    Ok((r.connection, r.model))
}

/// A side chat on `parent_id`, branching after event `anchor_seq`.
pub async fn create_side_chat(st: &HeadState, parent_id: &str, req: api::CreateSideChat) -> anyhow::Result<api::Session> {
    let parent = st.store.session(parent_id).await?.ok_or_else(|| not_found("no such session"))?;
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
        // A side chat has its parent's labels.
        labels: parent.labels.clone(),
        worker_name: parent.worker_name.clone(),
        repo: parent.repo.clone(),
        folder: parent.folder.clone(),
        home: parent.home.clone(),
        ssh_target: parent.ssh_target.clone(),
        awaiting: 0,
        subagent_model: parent.subagent_model.clone(),
        subagent_connection_id: parent.subagent_connection_id.clone(),
        subagent_effort: parent.subagent_effort.clone(),
        // A side chat is in its parent's realm.
        realm: parent.realm.clone(),
        agent: String::new(),
        end_reason: String::new(),
        root_id: Some(parent.root_id.clone().unwrap_or_else(|| parent.id.clone())),
        observed: false,
        // And acts as what its parent acts as.
        instance: parent.instance.clone(),
        // A side chat has its parent's colour.
        colour: parent.colour.clone(),
    };
    let seed = fork_seed(st, &parent, req.anchor_seq).await?;
    let spec = session_spec(st, &row, false, seed).await?;
    insert(st, row, spec, Some((prompt, Vec::new()))).await
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

/// The worker-side spec of a session, from its row. `seed` is the branched-
/// from conversation of a side chat.
/// Your policies, after the built-in one: agents' mail and invitations
/// wait for your approval (unless you turned that off).
async fn with_builtin_policies(st: &HeadState, yours: String) -> String {
    let on = crate::memex::settings(st).await.ok().and_then(|s| s.send_needs_approval).unwrap_or(true);
    let mut list: Vec<serde_json::Value> = serde_json::from_str(&yours).unwrap_or_default();
    if on {
        list.insert(
            0,
            serde_json::json!({
                "name": "Sending mail, invitations and chat messages needs your approval",
                "tools": "^(mail_send|MailSend|calendar_create|CalendarCreate|calendar_rsvp|CalendarRsvp|chat_post|ChatPost|chat_edit|ChatEdit|chat_delete|ChatDelete)$",
                "action": "ask",
                "message": "An agent wants to send mail, answer or send an invitation, or post in Slack in your name.",
            }),
        );
    }
    serde_json::to_string(&list).unwrap_or(yours)
}

async fn session_spec(st: &HeadState, row: &SessionRow, worktree: bool, seed: Vec<SessionEvent>) -> anyhow::Result<CreateSession> {
    let memex = crate::memex::ready(st);
    let scope = crate::memex::scope_for(st, &row.all_labels()).await;
    let connection = st.store.connection(&row.connection_id).await?.ok_or_else(|| bad_request("no such connection"))?;
    let connections = st.store.connections().await?;
    let subagent_connections = subagent_connections(&connections, &row.realm);
    let limits = subagent_limits(st, Some(&row.realm)).await?;
    // carbon6's prompts as the session's realm has them (docs/os.md §4.6).
    let base = crate::prompts::get(st, "session.base", &row.realm).await;
    let base_prompt =
        if row.fork_seq.is_some() { format!("{base}\n\n{}", crate::prompts::get(st, "session.side_chat", &row.realm).await) } else { base };
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
        mcp_servers_json: st.store.setting_in(MCP_SERVERS, &row.realm, &row.instance).await?.unwrap_or_else(|| "[]".into()),
        skills_json: st.store.setting_in(SKILLS, &row.realm, &row.instance).await?.unwrap_or_else(|| "[]".into()),
        hooks_json: st.store.setting_in(HOOKS, &row.realm, &row.instance).await?.unwrap_or_else(|| "[]".into()),
        agents_json: st.store.setting_in(AGENTS, &row.realm, &row.instance).await?.unwrap_or_else(|| "[]".into()),
        // Its realm's secrets, and root's it has none of.
        secrets_json: serde_json::to_string(&st.store.secrets_for(&row.realm).await?)?,
        labels: row.head_labels(),
        ssh_target: row.ssh_target.clone(),
        web_search_json: st.store.setting_for(WEB_SEARCH, &row.realm).await?.unwrap_or_default(),
        memories_json: crate::memory::for_sessions_of(st, &row.realm, &row.instance).await?,
        policies_json: with_builtin_policies(st, st.store.setting_in(POLICIES, &row.realm, &row.instance).await?.unwrap_or_default()).await,
        subagent_model: row.subagent_model.clone(),
        subagent_target: subagent_target(row, &connections),
        subagent_connections_json: serde_json::to_string(&subagent_connections)?,
        memex_brief: String::new(),
        memex_tools: memex,
        memex_mail: memex && scope.iter().any(|k| k == "email"),
        memex_calendar: memex && scope.iter().any(|k| k == "event"),
        memex_chat: memex && scope.iter().any(|k| k == "chat"),
        prompts_json: crate::prompts::for_worker(st, &row.realm).await,
        seed,
        delegation: None,
        max_subagent_depth: limits.max_depth,
        max_running_subagents: limits.max_running,
    })
}

/// The connections subagents in `realm` may run on: those the realm may use,
/// but not Claude Code (a subagent runs in carbon6's own loop).
fn subagent_connections(connections: &[crate::store::ConnectionRow], realm: &str) -> Vec<api::SubagentConnection> {
    connections
        .iter()
        .filter(|c| c.kind != "claude_code" && crate::realms::available(&c.realms, realm))
        .map(|c| api::SubagentConnection {
            id: c.id.clone(),
            name: c.name.clone(),
            kind: c.kind.clone(),
            default_model: c.default_model.clone().unwrap_or_default(),
        })
        .collect()
}

/// The default for a session's subagents, from its row: `None` when they
/// run on the session's own model.
fn subagent_target(row: &SessionRow, connections: &[crate::store::ConnectionRow]) -> Option<ModelSpec> {
    let model = row.subagent_model.trim();
    let connection_id = row.subagent_connection_id.trim();
    if model.is_empty() && connection_id.is_empty() {
        return None;
    }
    let connection_id = if connection_id.is_empty() { row.connection_id.as_str() } else { connection_id };
    let c = connections.iter().find(|c| c.id == connection_id)?;
    let model = match model {
        "" if c.id == row.connection_id => row.model.clone(),
        "" => c.default_model.clone().unwrap_or_default(),
        m => m.to_owned(),
    };
    Some(ModelSpec {
        connection_id: c.id.clone(),
        model,
        max_output_tokens: 0,
        effort: row.subagent_effort.trim().to_owned(),
        connection_kind: c.kind.clone(),
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
async fn insert(
    st: &HeadState,
    row: SessionRow,
    spec: CreateSession,
    first: Option<(&str, Vec<c6_proto::Attachment>)>,
) -> anyhow::Result<api::Session> {
    let worker_id: WorkerId = row.worker_id.parse()?;
    let session_id = row.id.clone();
    let labels = row.head_labels();
    let realm = row.realm.clone();
    let create_id = Uuid::now_v7().to_string();
    let create =
        SessionCommand { command_id: create_id.clone(), session_id: session_id.clone(), kind: Some(session_command::Kind::Create(spec)) };
    let mut commands = vec![NewCommand { id: &create_id, session_id: &session_id, worker_id, data: create.encode_to_vec(), text: None }];
    let input_id = Uuid::now_v7().to_string();
    if let Some((text, attachments)) = first {
        commands.push(NewCommand {
            id: &input_id,
            session_id: &session_id,
            worker_id,
            data: input_command(
                &input_id,
                &session_id,
                with_command(st, &realm, labels.clone(), UserInput { text: text.to_owned(), attachments, ..Default::default() }).await?,
            ),
            text: Some(text),
        });
    }
    let colour = st.store.create_session(&row, &commands).await?;
    st.hub.owners.lock().unwrap().insert(session_id.clone(), worker_id);
    st.registry.deliver(worker_id);
    st.registry.bump();
    Ok(convert::session(&SessionRow { colour, ..row }))
}

fn default_title(prompt: Option<&str>) -> String {
    let Some(p) = prompt else { return "New session".into() };
    let line = p.lines().next().unwrap_or(p).trim();
    if line.chars().count() <= 60 { line.to_owned() } else { format!("{}…", line.chars().take(59).collect::<String>()) }
}

/// An input (which may also switch the model or carry attachments).
fn input_command(command_id: &str, session_id: &str, input: UserInput) -> Vec<u8> {
    SessionCommand { command_id: command_id.to_owned(), session_id: session_id.to_owned(), kind: Some(session_command::Kind::Input(input)) }
        .encode_to_vec()
}

const MAX_ATTACHMENT: usize = 10 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 20 * 1024 * 1024;

/// Uploaded attachments, checked: images of the kinds models take, or
/// UTF-8 text, within size limits.
fn attachments(uploads: &[api::AttachmentUpload]) -> anyhow::Result<Vec<c6_proto::Attachment>> {
    use base64::Engine;
    let mut out = Vec::new();
    let mut total = 0;
    for u in uploads {
        let data = base64::engine::general_purpose::STANDARD
            .decode(u.data.trim())
            .map_err(|_| bad_request(format!("{}: not valid base64", u.name)))?;
        if data.len() > MAX_ATTACHMENT {
            return Err(bad_request(format!("{} is larger than 10 MB", u.name)));
        }
        total += data.len();
        let mime = u.mime.to_ascii_lowercase();
        let image = matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp");
        if !image && (mime.starts_with("image/") || std::str::from_utf8(&data).is_err()) {
            return Err(bad_request(format!("{}: only PNG, JPEG, GIF and WebP images, or text files, can be attached", u.name)));
        }
        out.push(c6_proto::Attachment { name: u.name.clone(), mime: if image { mime } else { "text/plain".into() }, data });
    }
    if total > MAX_ATTACHMENTS {
        return Err(bad_request("attachments total more than 20 MB"));
    }
    Ok(out)
}

/// The enabled custom slash commands `realm` uses.
pub async fn custom_commands(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::CommandDef>> {
    let raw = st.store.setting_for(COMMANDS, realm).await?.unwrap_or_else(|| "[]".into());
    Ok(serde_json::from_str::<Vec<api::CommandDef>>(&raw).unwrap_or_default().into_iter().filter(|c| c.enabled).collect())
}

/// Attaches the variants of the custom command the text invokes, if any,
/// and the session's labels, so the worker can pick the variant for the
/// session: commands apply as they are when the message is sent.
async fn with_command(st: &HeadState, realm: &str, labels: Vec<String>, mut input: UserInput) -> anyhow::Result<UserInput> {
    input.labels = labels;
    // @memex:<label or id>: what the memex knows about it, attached.
    for key in crate::memex::mentions(&input.text) {
        if input.attachments.iter().any(|a| a.name == format!("@memex:{key}")) {
            continue;
        }
        match crate::memex::card(st, &key).await {
            Ok(text) => input.attachments.push(c6_proto::Attachment {
                name: format!("@memex:{key}"),
                mime: "text/markdown".into(),
                data: text.into_bytes(),
            }),
            Err(e) => debug!(%key, "memex mention: {e:#}"),
        }
    }
    let Some(rest) = input.text.strip_prefix('/') else { return Ok(input) };
    let name = rest.split(char::is_whitespace).next().unwrap_or("");
    if !api::valid_command_name(name) {
        return Ok(input);
    }
    let variants: Vec<serde_json::Value> = custom_commands(st, realm)
        .await?
        .into_iter()
        .filter(|c| c.name == name)
        .map(|c| serde_json::json!({"applies_to": c.applies_to, "body": c.body}))
        .collect();
    if !variants.is_empty() {
        input.command = name.to_owned();
        input.command_candidates_json = serde_json::to_string(&variants)?;
    }
    Ok(input)
}

/// An "applies to" list: labels, deduplicated (automatic keys allowed).
pub fn check_selector(labels: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for l in labels.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        if l.contains(char::is_whitespace) || l.contains(',') {
            return Err(bad_request(format!("{l:?} is not a label")));
        }
        if !out.iter().any(|x| x == l) {
            out.push(l.to_owned());
        }
    }
    Ok(out)
}

/// Your labels, checked and deduplicated.
pub fn check_labels(labels: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for l in labels.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        c6_api::labels::check_user_label(l).map_err(bad_request)?;
        if !out.iter().any(|x| x == l) {
            out.push(l.to_owned());
        }
    }
    Ok(out)
}

/// Sets a session's colour, one of `SESSION_COLOURS`.
pub async fn set_colour(st: &HeadState, session_id: &str, colour: &str) -> anyhow::Result<api::Session> {
    if !api::SESSION_COLOURS.contains(&colour) {
        return Err(bad_request(format!("{colour:?} is not a session colour (one of {})", api::SESSION_COLOURS.join(", "))));
    }
    if !st.store.set_session_colour(session_id, colour).await? {
        return Err(not_found("no such session"));
    }
    st.registry.bump();
    let row = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    Ok(crate::convert::session(&row))
}

/// Sets the default for a session's subagents from now on: its row (for
/// the UI and a restored spec) and, through a queued command, the worker.
pub async fn set_subagent_model(st: &HeadState, session_id: &str, req: &api::SetSubagentModel) -> anyhow::Result<api::Session> {
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    if session.observed {
        return Err(bad_request("this is Claude Code's own subagent: it is shown here, and Claude Code runs it"));
    }
    let model = req.model.trim();
    let spec = if model.is_empty() {
        None
    } else {
        let effort = check_effort(Some(req.effort.trim().to_owned()))?.unwrap_or_default();
        let connection_id = if req.connection_id.trim().is_empty() { session.connection_id.as_str() } else { req.connection_id.trim() };
        let c = st.store.connection(connection_id).await?.ok_or_else(|| bad_request("no such connection"))?;
        if c.kind == "claude_code" {
            return Err(bad_request("subagents run in carbon6's own loop, not on Claude Code: pick an API connection for them"));
        }
        if !crate::realms::available(&c.realms, &session.realm) {
            return Err(bad_request(format!("the connection {} is not available to realm {}", c.name, session.realm)));
        }
        Some(ModelSpec { connection_id: c.id, model: model.to_owned(), max_output_tokens: 0, effort, connection_kind: c.kind })
    };
    let (c, m, e) = spec.as_ref().map(|s| (s.connection_id.as_str(), s.model.as_str(), s.effort.as_str())).unwrap_or_default();
    st.store.set_subagent_model(session_id, c, m, e).await?;
    let worker_id: WorkerId = session.worker_id.parse()?;
    let id = Uuid::now_v7().to_string();
    let data = SessionCommand {
        command_id: id.clone(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::SetSubagentModel(c6_proto::SetSubagentModel { model: spec })),
    }
    .encode_to_vec();
    st.store.enqueue_command(&NewCommand { id: &id, session_id, worker_id, data, text: None }).await?;
    st.registry.deliver(worker_id);
    st.registry.bump();
    let row = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    Ok(crate::convert::session(&row))
}

/// Replaces the labels you attached to a session (and its side chats keep
/// theirs).
pub async fn set_labels(st: &HeadState, session_id: &str, labels: &[String]) -> anyhow::Result<api::Session> {
    let labels = check_labels(labels)?;
    if !st.store.set_session_labels(session_id, &labels).await? {
        return Err(not_found("no such session"));
    }
    st.registry.bump();
    let row = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    Ok(crate::convert::session(&row))
}

/// What applies to a session now: the head's current settings matched
/// against its labels.
pub async fn effective_config(st: &HeadState, session_id: &str) -> anyhow::Result<api::EffectiveConfig> {
    use c6_api::labels::{applies, best};
    let row = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    let labels = row.all_labels();
    let realm = row.realm.as_str();
    let setting = |key: &'static str| async move {
        anyhow::Ok(
            serde_json::from_str::<serde_json::Value>(&st.store.setting_for(key, realm).await?.unwrap_or_else(|| "[]".into()))
                .unwrap_or_default(),
        )
    };
    // (name, enabled, applies_to) of each item, in order.
    type Item = (String, bool, Vec<String>);
    fn items(v: &serde_json::Value, name_key: &str) -> Vec<(String, bool, Vec<String>)> {
        v.as_array()
            .into_iter()
            .flatten()
            .map(|i| {
                let mut applies_to: Vec<String> =
                    i["applies_to"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect();
                if let Some(f) = i["folder"].as_str().filter(|f| !f.trim().is_empty()) {
                    applies_to.push(format!("folder:{}", f.trim()));
                }
                let name = i[name_key].as_str().unwrap_or("").to_owned();
                (name, i["enabled"].as_bool().unwrap_or(true), applies_to)
            })
            .collect()
    }
    let judge = |list: Vec<(String, bool, Vec<String>)>, named: bool| -> Vec<api::Applied> {
        let indexed: Vec<(usize, &Item)> = list.iter().enumerate().collect();
        list.iter()
            .enumerate()
            .map(|(i, (name, enabled, applies_to))| {
                let skipped = if !enabled {
                    Some("off".to_owned())
                } else if !applies(applies_to, &labels) {
                    let missing: Vec<&str> =
                        applies_to.iter().filter(|s| !c6_api::labels::label_matches(s, &labels)).map(String::as_str).collect();
                    Some(format!("needs {}", missing.join(", ")))
                } else if named
                    && best(indexed.iter().filter(|(_, (n, e, _))| n == name && *e), |x| &x.1.2, &labels).is_some_and(|(j, _)| *j != i)
                {
                    Some("replaced by a more specific one".to_owned())
                } else {
                    None
                };
                api::Applied { name: name.clone(), applies_to: applies_to.clone(), skipped }
            })
            .collect()
    };
    let hooks = items(&setting(HOOKS).await?, "event")
        .into_iter()
        .zip(setting(HOOKS).await?.as_array().cloned().unwrap_or_default())
        .map(|((event, enabled, applies_to), h)| {
            let name = h["name"].as_str().filter(|n| !n.is_empty()).map(|n| format!("{n} ({event})")).unwrap_or(event);
            (name, enabled, applies_to)
        })
        .collect();
    Ok(api::EffectiveConfig {
        labels: c6_api::labels::display_labels(&labels),
        commands: judge(items(&setting(COMMANDS).await?, "name"), true),
        agents: judge(items(&setting(AGENTS).await?, "name"), true),
        skills: judge(items(&setting(SKILLS).await?, "name"), true),
        hooks: judge(hooks, false),
    })
}

pub async fn send_input(st: &HeadState, session_id: &str, req: &api::SendMessage) -> anyhow::Result<api::QueuedInput> {
    let text = req.text.trim();
    let files = attachments(&req.attachments)?;
    if text.is_empty() && files.is_empty() {
        return Err(bad_request("message is empty"));
    }
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    if session.observed {
        return Err(bad_request("this is Claude Code's own subagent: it is shown here, and Claude Code runs it"));
    }
    if let Some(why) = crate::usage::blocked(st, &session.all_labels(), &session.realm).await? {
        return Err(bad_request(format!("not sent: {why}")));
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
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    let shown = if names.is_empty() { text.to_owned() } else { format!("{text} [{}]", names.join(", ")).trim().to_owned() };
    // To a subagent: whether its report should wake its parent.
    let wake = req.wake && session.parent_call_id.is_some();
    let input = with_command(
        st,
        &session.realm,
        session.head_labels(),
        UserInput { text: text.to_owned(), model, attachments: files, wake, ..Default::default() },
    )
    .await?;
    let data = input_command(&id, session_id, input);
    st.store.enqueue_command(&NewCommand { id: &id, session_id, worker_id, data, text: Some(&shown) }).await?;
    publish_queue(st, session_id).await;
    st.registry.deliver(worker_id);
    Ok(api::QueuedInput { command_id: id, text: shown, created_at: now_ms(), rejected: None })
}

/// The subagent-limits setting.
pub const SUBAGENT_LIMITS: &str = "subagent_limits";

/// The limits in `realm` (its own, else root's), or in the realm of the
/// Settings request (`None`).
pub async fn subagent_limits(st: &HeadState, realm: Option<&str>) -> anyhow::Result<api::SubagentLimits> {
    let raw = match realm {
        Some(r) => st.store.setting_for(SUBAGENT_LIMITS, r).await?,
        None => st.store.setting(SUBAGENT_LIMITS).await?,
    };
    Ok(raw.and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default())
}

/// Continues a subagent whose last turn did not finish, with the realm's
/// resume prompt.
pub async fn resume(st: &HeadState, session_id: &str, req: &api::ResumeRequest) -> anyhow::Result<api::QueuedInput> {
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    if session.parent_call_id.is_none() {
        return Err(bad_request("only subagents are resumed; send the session a message instead"));
    }
    if session.observed {
        return Err(bad_request("this is Claude Code's own subagent: Claude Code continues it"));
    }
    if !matches!(session.end_reason.as_str(), "interrupted" | "error" | "worker_restarted") || session.status == "running" {
        return Err(bad_request("its last turn finished; send it a message instead"));
    }
    if let Some(why) = crate::usage::blocked(st, &session.all_labels(), &session.realm).await? {
        return Err(bad_request(format!("not sent: {why}")));
    }
    let text = crate::prompts::get(st, "session.subagent_resume", &session.realm).await;
    let worker_id: WorkerId = session.worker_id.parse()?;
    let id = Uuid::now_v7().to_string();
    let input =
        UserInput { text: text.clone(), command: "resume".into(), wake: req.wake, labels: session.head_labels(), ..Default::default() };
    let data = input_command(&id, session_id, input);
    st.store.enqueue_command(&NewCommand { id: &id, session_id, worker_id, data, text: Some(&text) }).await?;
    publish_queue(st, session_id).await;
    st.registry.deliver(worker_id);
    Ok(api::QueuedInput { command_id: id, text, created_at: now_ms(), rejected: None })
}

/// Queues a rewind (applied after a running turn is interrupted). Commands
/// keep their order, so a message sent right after lands on the rewound
/// conversation.
pub async fn rewind(st: &HeadState, session_id: &str, req: &api::RewindRequest) -> anyhow::Result<()> {
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    if session.parent_call_id.is_some() {
        return Err(bad_request("subagent sessions cannot be rewound"));
    }
    let target = st.store.events_after(session_id, req.to_seq.saturating_sub(1), 1).await?;
    if !matches!(target.first().and_then(|e| e.kind.as_ref()), Some(session_event::Kind::User(_))) || target[0].seq != req.to_seq {
        return Err(bad_request("can only rewind to before one of your messages"));
    }
    let worker_id: WorkerId = session.worker_id.parse()?;
    let id = Uuid::now_v7().to_string();
    let data = SessionCommand {
        command_id: id.clone(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::Rewind(c6_proto::Rewind { to_seq: req.to_seq, restore_files: req.restore_files })),
    }
    .encode_to_vec();
    st.store.enqueue_command(&NewCommand { id: &id, session_id, worker_id, data, text: None }).await?;
    st.registry.deliver(worker_id);
    Ok(())
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
    // An imported session has no connection yet: it may take any.
    if !session.connection_id.is_empty() && (to.kind == "claude_code") != (from == "claude_code") {
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

/// Your answer to a tool call waiting for approval. `false`: its worker is
/// not connected.
pub async fn decide(st: &HeadState, session_id: &str, call_id: &str, allow: bool) -> anyhow::Result<bool> {
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    let cmd = SessionCommand {
        command_id: Uuid::now_v7().to_string(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::Approve(c6_proto::Approve { call_id: call_id.to_owned(), allow })),
    };
    Ok(st.registry.send_command(session.worker_id.parse()?, cmd))
}

/// Answers the assistant's question `call_id` in a conversation. Whether
/// it reached the conversation.
pub async fn answer_question(
    st: &HeadState,
    session_id: &str,
    call_id: &str,
    values: serde_json::Map<String, serde_json::Value>,
    cancelled: bool,
) -> anyhow::Result<bool> {
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    let cmd = SessionCommand {
        command_id: Uuid::now_v7().to_string(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::Answer(c6_proto::Answer {
            call_id: call_id.to_owned(),
            values_json: serde_json::Value::Object(values).to_string(),
            cancelled,
        })),
    };
    Ok(st.registry.send_command(session.worker_id.parse()?, cmd))
}

/// Where a new session runs: the connection, model, effort and subagent
/// model.
#[derive(Debug, Default)]
struct Routed {
    connection: String,
    model: String,
    effort: String,
    subagent_model: String,
    /// The subagents' connection (an id; empty: the session's) and effort.
    subagent_connection: String,
    subagent_effort: String,
}

/// Fills in what a new session left out. A model given: as is (on the
/// given connection, or the first). Otherwise the first enabled routing
/// rule matching `labels` on that connection (any, if none was given);
/// then the connection of your latest session (if none was given) and
/// its default model, or the model of your latest session on it.
/// The connection and model for a new session in `realm` with these
/// labels: the realm's routing rules first, then root's.
async fn route(st: &HeadState, connection: &str, model: &str, labels: &[String], realm: &str) -> anyhow::Result<Routed> {
    let connections = st.store.connections().await?;
    let connection = connection.trim();
    let model = model.trim();
    if !connection.is_empty() && !connections.iter().any(|c| c.id == connection) {
        return Err(bad_request("no such connection"));
    }
    let first = || connections.first().map(|c| c.id.clone()).ok_or_else(|| bad_request("add a connection first (Settings → Connections)"));
    if !model.is_empty() {
        let connection = if connection.is_empty() { first()? } else { connection.to_owned() };
        return Ok(Routed { connection, model: model.to_owned(), ..Default::default() });
    }
    let routes: Vec<api::Route> = serde_json::from_str(&st.store.setting_for(ROUTES, realm).await?.unwrap_or_default()).unwrap_or_default();
    // A rule's connection, by name or id: None when it names one that is gone.
    let rule_connection = |r: &api::Route| -> Option<String> {
        let want = r.connection.trim();
        if want.is_empty() {
            return Some(String::new());
        }
        connections.iter().find(|c| c.id == want || c.name == want).map(|c| c.id.clone())
    };
    let hit = routes.into_iter().filter(|r| r.enabled && api::labels::applies(&r.applies_to, labels)).find_map(|r| {
        let rc = rule_connection(&r)?;
        (connection.is_empty() || rc.is_empty() || rc == connection).then_some((rc, r))
    });
    if let Some((rc, r)) = hit {
        let connection = match (connection, rc.as_str()) {
            ("", "") => match st.store.latest_model("").await? {
                Some((latest, _)) => latest,
                None => first()?,
            },
            ("", rc) => rc.to_owned(),
            (c, _) => c.to_owned(),
        };
        // A subagent connection that is gone: the session's.
        let subagent_connection = connections
            .iter()
            .find(|c| {
                !r.subagent_connection.trim().is_empty() && (c.id == r.subagent_connection.trim() || c.name == r.subagent_connection.trim())
            })
            .map(|c| c.id.clone())
            .unwrap_or_default();
        return Ok(Routed {
            connection,
            model: r.model.trim().to_owned(),
            effort: r.effort,
            subagent_model: r.subagent_model.trim().to_owned(),
            subagent_connection,
            subagent_effort: r.subagent_effort.trim().to_owned(),
        });
    }
    let connection = match (connection, st.store.latest_model("").await?) {
        ("", Some((latest, _))) => latest,
        ("", None) => first()?,
        (c, _) => c.to_owned(),
    };
    let row = connections.iter().find(|c| c.id == connection);
    if let Some(m) = row.and_then(|c| c.default_model.clone()).filter(|m| !m.trim().is_empty()) {
        return Ok(Routed { connection, model: m.trim().to_owned(), ..Default::default() });
    }
    if let Some((_, m)) = st.store.latest_model(&connection).await? {
        return Ok(Routed { connection, model: m, ..Default::default() });
    }
    Err(bad_request("pick a model: there is no routing rule for this session and the connection has no default model"))
}

/// Checks routing rules before they are saved.
pub async fn check_routes(st: &HeadState, routes: &[api::Route]) -> anyhow::Result<()> {
    let connections = st.store.connections().await?;
    for r in routes {
        if r.name.trim().is_empty() {
            return Err(bad_request("a routing rule needs a name"));
        }
        if r.model.trim().is_empty() {
            return Err(bad_request(format!("routing rule {:?}: a model is required", r.name)));
        }
        let want = r.connection.trim();
        if !want.is_empty() && !connections.iter().any(|c| c.id == want || c.name == want) {
            return Err(bad_request(format!("routing rule {:?}: no connection {want:?}", r.name)));
        }
        check_effort(Some(r.effort.clone())).map_err(|e| bad_request(format!("routing rule {:?}: {e}", r.name)))?;
        let sub = r.subagent_connection.trim();
        if !sub.is_empty() {
            match connections.iter().find(|c| c.id == sub || c.name == sub) {
                None => return Err(bad_request(format!("routing rule {:?}: no connection {sub:?} for subagents", r.name))),
                Some(c) if c.kind == "claude_code" => {
                    return Err(bad_request(format!(
                        "routing rule {:?}: subagents run in carbon6's own loop, not on Claude Code; pick an API connection for them",
                        r.name
                    )));
                }
                Some(_) => {}
            }
            if r.subagent_model.trim().is_empty() {
                return Err(bad_request(format!("routing rule {:?}: a subagent connection needs a subagent model", r.name)));
            }
        }
        check_effort(Some(r.subagent_effort.clone())).map_err(|e| bad_request(format!("routing rule {:?}: subagent {e}", r.name)))?;
        check_selector(&r.applies_to)?;
    }
    Ok(())
}

/// Checks policies before they are saved.
pub fn check_policies(policies: &[api::Policy]) -> anyhow::Result<()> {
    for p in policies {
        if p.name.trim().is_empty() {
            return Err(bad_request("a policy needs a name"));
        }
        if !matches!(p.action.as_str(), "ask" | "deny") {
            return Err(bad_request(format!("policy {:?}: the action is ask or deny", p.name)));
        }
        for (what, re) in [("tools", &p.tools), ("pattern", &p.pattern)] {
            if !re.trim().is_empty()
                && let Err(e) = regex::Regex::new(re)
            {
                return Err(bad_request(format!("policy {:?}: {what} is not a valid regular expression: {e}", p.name)));
            }
        }
    }
    Ok(())
}

/// Stops a session's background task. `false`: its worker is not connected.
pub async fn stop_task(st: &HeadState, session_id: &str, task_id: &str) -> anyhow::Result<bool> {
    let session = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    let cmd = SessionCommand {
        command_id: Uuid::now_v7().to_string(),
        session_id: session_id.to_owned(),
        kind: Some(session_command::Kind::StopTask(c6_proto::StopTask { task_id: task_id.to_owned() })),
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
    notify_events(st, &batch.session_id, &ing.events).await;
    crate::memex::on_events(st, &batch.session_id, &ing.events).await;
    crate::decisions::on_events(st, &batch.session_id, &ing.events).await;
    // How a run's session went: its first turn's end.
    for ev in &ing.events {
        if let Some(c6_proto::session_event::Kind::TurnEnded(t)) = &ev.kind {
            let outcome = match c6_proto::TurnEndReason::try_from(t.reason) {
                Ok(c6_proto::TurnEndReason::Completed) => "ok",
                Ok(c6_proto::TurnEndReason::Error) => "failed",
                _ => "stopped",
            };
            crate::triggers::session_ended(st, &batch.session_id, outcome).await;
            match outcome {
                "ok" => crate::triggers::on_session(st, &batch.session_id, "finished", "").await,
                "failed" => crate::triggers::on_session(st, &batch.session_id, "failed", "").await,
                _ => {}
            }
        }
    }
    if ing.events.iter().any(|e| matches!(e.kind, Some(c6_proto::session_event::Kind::Usage(_)))) {
        crate::usage::after_calls(st, &batch.session_id).await;
    }
    let ended: Vec<u64> =
        ing.events.iter().filter(|e| matches!(e.kind, Some(c6_proto::session_event::Kind::TurnEnded(_)))).map(|e| e.seq).collect();
    if st.otlp.is_some() && !ended.is_empty() {
        export_turns(st, worker_id, &batch.session_id, ended).await;
    }
    Ok(Some(ing.last_seq))
}

/// Queues notifications for new events: approvals, notices, outputs, and
/// turns that finished or failed (in your sessions, not subagents').
async fn notify_events(st: &HeadState, session_id: &str, events: &[SessionEvent]) {
    use c6_proto::session_event::Kind;
    use c6_proto::{ContentBlock, TurnEndReason, content_block};
    let interesting = events.iter().any(|e| {
        matches!(&e.kind, Some(Kind::Approval(a)) if a.state == "requested")
            || matches!(e.kind, Some(Kind::Notice(_) | Kind::Output(_) | Kind::TurnEnded(_)))
    });
    if !interesting {
        return;
    }
    let Ok(Some(session)) = st.store.session(session_id).await else { return };
    let labels = session.all_labels();
    let base = crate::notify::Note { session_id: Some(session_id.to_owned()), labels, priority: "normal", ..Default::default() };
    let title = |what: &str| format!("{what}: {}", session.title);
    let text_of = |blocks: &[ContentBlock]| -> String {
        blocks.iter().filter_map(|b| if let Some(content_block::Block::Text(t)) = &b.block { Some(t.as_str()) } else { None }).collect()
    };
    for ev in events {
        let note = match &ev.kind {
            Some(Kind::Approval(a)) if a.state == "requested" => crate::notify::Note {
                event: "approval",
                title: title("Approval needed"),
                message: format!(
                    "{}{}: {}",
                    if a.message.is_empty() { String::new() } else { format!("{}\n", a.message) },
                    a.tool,
                    a.input.chars().take(600).collect::<String>()
                ),
                priority: "high",
                call_id: Some(a.call_id.clone()),
                ..base.clone()
            },
            Some(Kind::Notice(n)) => crate::notify::Note {
                event: "notice",
                title: n.title.clone(),
                message: n.message.clone(),
                priority: match n.priority.as_str() {
                    "low" => "low",
                    "high" => "high",
                    _ => "normal",
                },
                ..base.clone()
            },
            Some(Kind::Output(o)) => crate::notify::Note {
                event: "output",
                title: title("Published"),
                message: if o.title.is_empty() { o.name.clone() } else { format!("{} ({})", o.title, o.name) },
                ..base.clone()
            },
            Some(Kind::TurnEnded(t)) if session.parent_call_id.is_none() => {
                // What the turn ended with: its error, or the last reply.
                let turn = st.store.turn_events(session_id, ev.seq).await.unwrap_or_default();
                let before = turn.as_slice();
                match TurnEndReason::try_from(t.reason) {
                    Ok(TurnEndReason::Error) => crate::notify::Note {
                        event: "failed",
                        title: title("Failed"),
                        message: before
                            .iter()
                            .rev()
                            .find_map(|e| if let Some(Kind::Error(x)) = &e.kind { Some(x.message.clone()) } else { None })
                            .unwrap_or_default(),
                        priority: "high",
                        ..base.clone()
                    },
                    Ok(TurnEndReason::Completed) => {
                        let reply = before
                            .iter()
                            .rev()
                            .find_map(|e| match &e.kind {
                                Some(Kind::Assistant(m)) => Some(text_of(&m.blocks)).filter(|t| !t.trim().is_empty()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        let mut message: String = reply.trim().chars().take(500).collect();
                        if reply.trim().chars().count() > 500 {
                            message.push('…');
                        }
                        crate::notify::Note { event: "finished", title: title("Done"), message, ..base.clone() }
                    }
                    _ => continue,
                }
            }
            _ => continue,
        };
        st.notifier.send(note);
    }
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
        if st.store.session(&session_id).await?.is_some_and(|s| s.status == "failed") {
            crate::triggers::session_ended(st, &session_id, "failed").await;
        }
    }
    Ok(())
}

/// Queues a `Restore` and a fresh copy of the rejected input, in that order.
async fn restore(st: &HeadState, worker_id: WorkerId, session_id: &str, command_id: &str) -> anyhow::Result<bool> {
    let Some(session) = st.store.session(session_id).await? else { return Ok(false) };
    if session.status == "failed" || session.observed || session.worker_id != worker_id.to_string() {
        return Ok(false);
    }
    let Some((text, data)) = st.store.pending_input(command_id).await? else { return Ok(false) };
    // The same input (model switch, attachments and all), under a new id.
    let input = match SessionCommand::decode(data.as_slice()).ok().and_then(|c| c.kind) {
        Some(session_command::Kind::Input(i)) => i,
        _ => UserInput { text: text.clone(), ..Default::default() },
    };
    let events = st.store.events_after(session_id, 0, i64::MAX).await?;
    if events.is_empty() {
        return Ok(false);
    }
    // An imported session has no connection until its first message here
    // picks one (docs/imports.md).
    let mut session = session;
    if session.connection_id.is_empty() {
        let Some(m) = &input.model else { return Ok(false) };
        session.connection_id = m.connection_id.clone();
        session.model = m.model.clone();
    }
    let seed = match (&session.parent_id, session.fork_seq) {
        (Some(p), Some(f)) => match st.store.session(p).await? {
            Some(parent) => fork_seed(st, &parent, f as u64).await?,
            None => return Ok(false),
        },
        _ => Vec::new(),
    };
    let mut spec = session_spec(st, &session, false, seed).await?;
    // A subagent's spec says whose it is; its tools and prompt come from
    // its log.
    if let (Some(parent), Some(call)) = (&session.parent_id, &session.parent_call_id) {
        let depth = match events.first().and_then(|e| e.kind.as_ref()) {
            Some(session_event::Kind::Started(s)) => s.depth.max(1),
            _ => 1,
        };
        spec.delegation = Some(c6_proto::Delegation {
            parent_session_id: parent.clone(),
            parent_call_id: call.clone(),
            root_session_id: session.root_id.clone().unwrap_or_else(|| parent.clone()),
            agent_json: String::new(),
            depth,
            observed: session.observed,
        });
    }
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
            &NewCommand { id: &input_id, session_id, worker_id, data: input_command(&input_id, session_id, input), text: Some(&text) },
        )
        .await?;
    warn!(session = %session_id, "worker no longer had this session; restoring it from the head's log");
    publish_queue(st, session_id).await;
    st.registry.deliver(worker_id);
    Ok(true)
}

/// A secret's value for a worker, as a credential: `name@session` asks on
/// behalf of one of the worker's sessions, and the secret must be in that
/// session's realm; a bare name (an older worker) gets only home's. A
/// secret scoped to `worker:` labels is only given to those workers.
async fn secret_credential(
    st: &HeadState,
    worker_id: WorkerId,
    worker_name: &str,
    name: &str,
    mut grant: CredentialGrant,
) -> CredentialGrant {
    let (name, realm) = match name.split_once('@') {
        Some((name, session_id)) => match st.store.session(session_id).await {
            Ok(Some(s)) if s.worker_id == worker_id.to_string() => (name, s.realm),
            _ => {
                grant.error = "no such session on this worker".into();
                return grant;
            }
        },
        None => (name, crate::realms::HOME.to_owned()),
    };
    // The session's realm's, else root's.
    let (blob, applies_to) = match st.store.secret(&realm, name).await {
        Ok(Some((blob, applies_to, _))) => (blob, applies_to),
        Ok(None) => {
            grant.error = format!("no secret named {name} in realm {realm} (or root)");
            return grant;
        }
        Err(e) => {
            warn!("loading secret: {e:#}");
            grant.error = "internal error".into();
            return grant;
        }
    };
    let worker_scopes: Vec<&String> = applies_to.iter().filter(|l| l.starts_with("worker:")).collect();
    if !worker_scopes.is_empty() && !worker_scopes.iter().any(|l| l.as_str() == format!("worker:{worker_name}")) {
        grant.error = format!("secret {name} is not for this worker");
        return grant;
    }
    match st.secrets.decrypt(&blob) {
        Ok(v) => {
            grant.kind = "secret".into();
            grant.bearer = v;
            // Short-lived on the worker, so changes reach running sessions.
            grant.expires_at_ms = now_ms() + 5 * 60 * 1000;
        }
        Err(e) => {
            warn!(secret = %name, "decrypting: {e:#}");
            grant.error = "secret unavailable".into();
        }
    }
    grant
}

pub async fn credential(st: &HeadState, worker_id: WorkerId, req: CredentialRequest) -> CredentialGrant {
    let worker_name = st.store.worker(worker_id).await.ok().flatten().map(|w| w.name).unwrap_or_default();
    credential_for(st, Some(worker_id), &worker_name, req).await
}

/// Credentials for a node (`worker_id`: a fabric node; none: the head's
/// own knowledge store, docs/knowledge.md).
pub async fn credential_for(st: &HeadState, worker_id: Option<WorkerId>, worker_name: &str, req: CredentialRequest) -> CredentialGrant {
    let mut grant = CredentialGrant { request_id: req.request_id, connection_id: req.connection_id.clone(), ..Default::default() };
    if let Some(name) = req.connection_id.strip_prefix("secret:") {
        let Some(worker_id) = worker_id else {
            grant.error = "secrets are for workers' sessions".into();
            return grant;
        };
        return secret_credential(st, worker_id, worker_name, name, grant).await;
    }
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
    // A worker gets a connection only if some realm it serves may use it
    // (a subagent may pick its connection on the worker).
    if let Some(worker_id) = worker_id
        && !worker_may_use(st, worker_id, &conn.realms).await
    {
        grant.error = format!("the connection {} is not available to any realm this worker serves", conn.name);
        return grant;
    }
    grant.kind = conn.kind.clone();
    grant.base_url = conn.base_url.clone();
    if conn.kind == "chatgpt" {
        match chatgpt_tokens_now(st, &conn.id, req.refresh).await {
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

/// Whether a worker serves a realm a connection kept to `connection_realms`
/// (JSON; empty: every realm) is available to.
async fn worker_may_use(st: &HeadState, worker_id: WorkerId, connection_realms: &str) -> bool {
    let kept: Vec<String> = serde_json::from_str(connection_realms).unwrap_or_default();
    if kept.is_empty() {
        return true;
    }
    match st.store.worker(worker_id).await {
        Ok(Some(w)) => {
            let serves: Vec<String> = serde_json::from_str(&w.realms).unwrap_or_default();
            serves.is_empty() || serves.iter().any(|r| kept.contains(r))
        }
        // The head's own nodes and unknown ones: as before.
        _ => true,
    }
}

/// Current ChatGPT tokens for a connection, refreshed here (never on
/// workers) when close to expiry.
pub async fn chatgpt_tokens(st: &HeadState, connection_id: &str) -> anyhow::Result<crate::oauth::ChatGptTokens> {
    chatgpt_tokens_now(st, connection_id, false).await
}

/// When ChatGPT refused a token before its time (`refused`), it is
/// refreshed anyway, at most once a minute per connection (the workers
/// that all got the same refusal share the one refresh).
async fn chatgpt_tokens_now(st: &HeadState, connection_id: &str, refused: bool) -> anyhow::Result<crate::oauth::ChatGptTokens> {
    static FORCED: std::sync::LazyLock<std::sync::Mutex<HashMap<String, i64>>> = std::sync::LazyLock::new(Default::default);
    let lock = st.oauth.lock_for(connection_id);
    let _guard = lock.lock().await;
    // Read under the lock so a refresh that just finished is seen.
    let row = st.store.connection(connection_id).await?.ok_or_else(|| not_found("no such connection"))?;
    let blob = row.api_key.ok_or_else(|| bad_request("connection has no stored sign-in"))?;
    let tokens: crate::oauth::ChatGptTokens = serde_json::from_str(&st.secrets.decrypt(&blob)?)?;
    let force = refused && {
        let mut f = FORCED.lock().unwrap();
        let now = now_ms();
        let recent = f.get(connection_id).is_some_and(|t| now - t < 60_000);
        if !recent {
            f.insert(connection_id.to_owned(), now);
        }
        !recent
    };
    if !force && !crate::oauth::OAuth::needs_refresh(&tokens) {
        return Ok(tokens);
    }
    if force {
        info!(connection = %connection_id, "ChatGPT refused its token early; refreshing it");
    }
    let refreshed = st.oauth.refresh(&st.http, tokens).await?;
    st.store.set_connection_secret(connection_id, &st.secrets.encrypt(&serde_json::to_string(&refreshed)?)?).await?;
    Ok(refreshed)
}
