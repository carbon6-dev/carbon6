//! Remote subagents (docs/remote-subagents.md): sessions the head starts
//! from a template for another session's `Task` call, perhaps on another
//! worker. The head holds the link: it sends the subagent its parent's
//! messages, and the parent how each of the subagent's turns ended.

use std::collections::HashMap;

use c6_api as api;
use c6_core::WorkerId;
use c6_proto::{DelegationEnded, SessionCommand, TurnEndReason, content_block, session_command, session_event};
use prost::Message;
use serde_json::{Value, json};
use tracing::warn;
use uuid::Uuid;

use crate::store::{NewCommand, SessionRow};
use crate::{HeadState, bad_request};

/// `session.subagent_<what>` from the worker of `parent`, its parent.
pub async fn for_session(st: &HeadState, parent: &SessionRow, what: &str, params: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| params[k].as_str().unwrap_or_default().trim().to_owned();
    match what {
        "start" => {
            let (template, on, prompt, description, call_id) = (s("template"), s("on"), s("prompt"), s("description"), s("call_id"));
            if call_id.is_empty() || prompt.is_empty() {
                return Err(bad_request("call_id and prompt are required"));
            }
            let req = crate::templates::SubagentStart {
                template: &template,
                on: &on,
                prompt: &prompt,
                description: if description.is_empty() { &template } else { &description },
                call_id: &call_id,
                depth: params["depth"].as_u64().unwrap_or(0) as u32,
            };
            let (session, remote) = crate::templates::start_subagent(st, parent, &req).await?;
            Ok(json!({"child_session_id": session.id, "remote": remote, "connection_id": session.connection_id, "model": session.model,
                      "effort": session.effort.unwrap_or_default()}))
        }
        "message" => {
            let child = own_child(st, parent, &s("to")).await?;
            let (call_id, text) = (s("call_id"), s("message"));
            if text.is_empty() {
                return Err(bad_request("message is required"));
            }
            let req = api::SendMessage { text, ..Default::default() };
            let id = Uuid::now_v7().to_string();
            // Running: the message joins its turn, whose report goes where
            // it would. Idle: the call waits on the turn it starts.
            let running = child.status == "running";
            if !running {
                st.store.add_remote_delegation(&id, &child.id, &parent.id, &call_id).await?;
            }
            if let Err(e) = crate::sessions::send_input_as(st, &child.id, &req, id.clone(), &call_id).await {
                st.store.delegation_reported(&id).await?;
                return Err(e);
            }
            Ok(json!({"running": running}))
        }
        "interrupt" => {
            let child = own_child(st, parent, &s("to")).await?;
            Ok(json!({"sent": crate::sessions::interrupt(st, &child.id).await?}))
        }
        "state" => {
            let child = own_child(st, parent, &s("to")).await?;
            Ok(json!({"running": child.status == "running", "status": child.status, "end_reason": child.end_reason}))
        }
        "peek" => {
            let child = own_child(st, parent, &s("to")).await?;
            let status = if child.status == "running" { "running".to_owned() } else { state_of(&child) };
            Ok(json!({"text": digest(st, &child.id, &status).await?}))
        }
        _ => Err(bad_request(format!("unknown method session.subagent_{what}"))),
    }
}

/// One of `parent`'s remote subagents, by session id.
async fn own_child(st: &HeadState, parent: &SessionRow, to: &str) -> anyhow::Result<SessionRow> {
    match st.store.session(to).await? {
        Some(c) if c.parent_id.as_deref() == Some(parent.id.as_str()) && !c.remote_template.is_empty() => Ok(c),
        _ => Err(bad_request(format!("no remote subagent {to:?} was started by this session"))),
    }
}

/// A subagent's state as its digest names it.
fn state_of(s: &SessionRow) -> String {
    if s.status == "running" {
        return "running".into();
    }
    match s.end_reason.as_str() {
        "" => "not started",
        "completed" => "completed",
        "interrupted" => "interrupted",
        "worker_restarted" => "lost",
        _ => "failed",
    }
    .into()
}

/// What a subagent did, from the head's copy of its log.
async fn digest(st: &HeadState, child_id: &str, status: &str) -> anyhow::Result<String> {
    let events = st.store.events_after(child_id, 0, i64::MAX).await?;
    let mut states = HashMap::new();
    for id in c6_worker::spawned_children(&events) {
        if let Some(s) = st.store.session(&id).await? {
            states.insert(id, state_of(&s));
        }
    }
    Ok(c6_worker::digest_text(&events, child_id, status, "", &states))
}

/// A turn of `session_id` ended (at `end_seq`). For a remote subagent, its
/// parent hears how: the calls that waited on the inputs the turn took get
/// its outcome; a turn no call waited on (you messaged the subagent) comes
/// to the parent as a notice.
pub async fn turn_ended(st: &HeadState, session_id: &str, end_seq: u64, reason: i32) {
    if let Err(e) = relay(st, session_id, end_seq, reason).await {
        warn!(session = %session_id, "relaying a remote subagent's turn to its parent: {e:#}");
    }
}

async fn relay(st: &HeadState, child_id: &str, end_seq: u64, reason: i32) -> anyhow::Result<()> {
    let Some(child) = st.store.session(child_id).await? else { return Ok(()) };
    let Some(parent_id) = child.parent_id.clone().filter(|_| !child.remote_template.is_empty()) else { return Ok(()) };
    let events = st.store.since_last_turn(child_id, end_seq).await?;
    let mut taken = Vec::new();
    let mut said = Vec::new();
    let mut yours = false;
    let mut wake = false;
    let mut report = String::new();
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::User(u)) => {
                taken.push(u.command_id.clone());
                // Yours (not its parent's, nor its own tasks' notices).
                if u.delegated_by.is_empty() && u.command != "task-notification" {
                    yours = true;
                    wake |= u.wake;
                    if u.command != "resume" {
                        said.push(u.text.clone());
                    }
                }
            }
            Some(session_event::Kind::Assistant(a)) => {
                let text: Vec<&str> = a
                    .blocks
                    .iter()
                    .filter_map(|b| match &b.block {
                        Some(content_block::Block::Text(t)) if !t.trim().is_empty() => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    report = text.join("\n");
                }
            }
            _ => {}
        }
    }
    let settled: Vec<(String, String)> =
        st.store.waiting_delegations(child_id).await?.into_iter().filter(|(cmd, _)| taken.contains(cmd)).collect();
    if settled.is_empty() && !yours {
        return Ok(());
    }
    let reason = TurnEndReason::try_from(reason).unwrap_or(TurnEndReason::Unspecified);
    let digest = if reason == TurnEndReason::Completed && !report.trim().is_empty() {
        String::new()
    } else {
        digest(st, child_id, c6_worker::status_of(reason, &report)).await?
    };
    let ended = |call_id: &str, said: Vec<String>, wake: bool| DelegationEnded {
        call_id: call_id.to_owned(),
        child_session_id: child_id.to_owned(),
        reason: reason as i32,
        report: report.clone(),
        digest: digest.clone(),
        said,
        wake,
    };
    if settled.is_empty() {
        send(st, &parent_id, ended("", said, wake)).await?;
    }
    for (command_id, call_id) in settled {
        if st.store.delegation_reported(&command_id).await? {
            send(st, &parent_id, ended(&call_id, Vec::new(), false)).await?;
        }
    }
    Ok(())
}

/// A command for a remote subagent was rejected: calls waiting on it (or,
/// when the subagent failed to start, on any of its inputs) hear that.
pub async fn rejected(st: &HeadState, session_id: &str, command_id: &str, why: &str) {
    let result: anyhow::Result<()> = async {
        let Some(child) = st.store.session(session_id).await? else { return Ok(()) };
        let Some(parent_id) = child.parent_id.clone().filter(|_| !child.remote_template.is_empty()) else { return Ok(()) };
        let failed = child.status == "failed";
        for (cmd, call_id) in st.store.waiting_delegations(session_id).await? {
            if (failed || cmd == command_id) && st.store.delegation_reported(&cmd).await? {
                let digest = format!(
                    "<subagent id=\"{session_id}\" status=\"failed\" turns=\"0\" tool_calls=\"0\" elapsed=\"0s\">\nErrors: 1 (last: {why})\n</subagent>"
                );
                let ended = DelegationEnded {
                    call_id,
                    child_session_id: session_id.to_owned(),
                    reason: TurnEndReason::Error as i32,
                    digest,
                    ..Default::default()
                };
                send(st, &parent_id, ended).await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        warn!(session = %session_id, "telling a remote subagent's parent of a rejected command: {e:#}");
    }
}

/// Queues `ended` for the parent: delivered when its worker is connected,
/// and again until the parent's log says it got it.
async fn send(st: &HeadState, parent_id: &str, ended: DelegationEnded) -> anyhow::Result<()> {
    let Some(parent) = st.store.session(parent_id).await? else { return Ok(()) };
    let worker_id: WorkerId = parent.worker_id.parse()?;
    let id = Uuid::now_v7().to_string();
    let cmd = SessionCommand {
        command_id: id.clone(),
        session_id: parent_id.to_owned(),
        kind: Some(session_command::Kind::DelegationEnded(ended)),
    };
    st.store.enqueue_command(&NewCommand { id: &id, session_id: parent_id, worker_id, data: cmd.encode_to_vec(), text: None }).await?;
    st.registry.deliver(worker_id);
    Ok(())
}
