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
            // Every input owns a durable report obligation, including a
            // steer. The running snapshot can already be stale, and the
            // background task that owned the previous turn may have ended.
            // A steer has no waiter: its report must arrive as a waking notice.
            let running = child.status == "running";
            st.store.add_remote_delegation(&id, &child.id, &parent.id, if running { "" } else { &call_id }).await?;
            if let Err(e) = crate::sessions::send_input_as(st, &child.id, &req, id.clone(), &call_id, None, false).await {
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
        delivery_failed(st, session_id, &end_seq.to_string(), &e).await;
    }
}

async fn delivery_failed(st: &HeadState, session_id: &str, report: &str, error: &anyhow::Error) {
    if let Some(child) = st.store.session(session_id).await.ok().flatten() {
        let id = format!("subagent:delivery-failed:{session_id}:{report}");
        let text = format!(
            "Subagent report delivery failed: report from {session_id} to parent {} remains pending.",
            child.parent_id.as_deref().unwrap_or("unknown")
        );
        let failure = async {
            let data = SessionCommand {
                command_id: id.clone(),
                session_id: session_id.into(),
                kind: Some(session_command::Kind::Input(c6_proto::UserInput { text: text.clone(), ..Default::default() })),
            }
            .encode_to_vec();
            st.store
                .subagent_report_failed(
                    &NewCommand { id: &id, session_id, worker_id: child.worker_id.parse()?, data, text: Some(&text) },
                    &format!("{error:#}"),
                )
                .await?;
            crate::sessions::publish_queue(st, session_id).await;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = failure {
            warn!(session = %session_id, "recording report delivery failure: {error:#}");
        }
    }
    st.notifier.send(crate::notify::Note {
        event: "failed",
        title: "Subagent report delivery failed".into(),
        message: format!(
            "Report from subagent {session_id} could not reach its parent: {error:#}. Its delivery obligation is retained for retry."
        ),
        priority: "high",
        session_id: Some(session_id.to_owned()),
        ..Default::default()
    });
}

async fn clear_delivery_failure(st: &HeadState, child: &str, report: &str) {
    let id = format!("subagent:delivery-failed:{child}:{report}");
    match sqlx::query("UPDATE commands SET state = 'done' WHERE id = ? AND state = 'rejected'").bind(id).execute(st.store.pool()).await {
        Ok(done) if done.rows_affected() > 0 => crate::sessions::publish_queue(st, child).await,
        Ok(_) => {}
        Err(e) => warn!(session = %child, "clearing an earlier report delivery failure: {e:#}"),
    }
}

/// Also used inside ingest's transaction: even an automatic turn with no
/// parent call owns a report, durably, before the relay hook runs.
pub(crate) fn turn_report_id(child: &str, seq: u64) -> String {
    format!("subagent:turn:{child}:{seq}")
}

/// Ingest commits the child's events before running its hooks. If the
/// head stopped between those operations, replay outstanding obligations
/// from that durable log rather than waiting for another child turn.
pub async fn recover(st: &HeadState) -> anyhow::Result<()> {
    let children: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT child_id FROM remote_delegations WHERE state = 'waiting'").fetch_all(st.store.pool()).await?;
    for child in children {
        let pending = st.store.waiting_delegations(&child).await?;
        let events = st.store.events_after(&child, 0, i64::MAX).await?;
        let mut taken = false;
        for event in events {
            match event.kind {
                Some(session_event::Kind::User(u)) if pending.iter().any(|(id, _)| id == &u.command_id) => taken = true,
                Some(session_event::Kind::TurnEnded(t))
                    if taken || pending.iter().any(|(id, _)| id == &turn_report_id(&child, event.seq)) =>
                {
                    turn_ended(st, &child, event.seq, t.reason).await;
                    taken = false;
                }
                _ => {}
            }
        }
        // Rejections are committed before their hook too.
        let rejected_inputs: Vec<(String, String)> = sqlx::query_as("SELECT d.command_id, coalesce(c.error, 'input rejected') FROM remote_delegations d JOIN commands c ON c.id = d.command_id WHERE d.child_id = ? AND d.state = 'waiting' AND c.state = 'rejected'")
            .bind(&child).fetch_all(st.store.pool()).await?;
        for (id, error) in rejected_inputs {
            rejected(st, &child, &id, &error).await;
        }
    }
    Ok(())
}

async fn relay(st: &HeadState, child_id: &str, end_seq: u64, reason: i32) -> anyhow::Result<()> {
    let Some(child) = st.store.session(child_id).await? else { return Ok(()) };
    let Some(parent_id) = child.parent_id.clone().filter(|_| !child.remote_template.is_empty()) else { return Ok(()) };
    let events = st.store.since_last_turn(child_id, end_seq).await?;
    let mut taken = Vec::new();
    let mut said = Vec::new();
    let mut delegated = false;
    let mut automatic = false;
    let mut wake = false;
    let mut report = String::new();
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::User(u)) => {
                taken.push(u.command_id.clone());
                delegated |= !u.delegated_by.is_empty();
                automatic |= u.command == "task-notification";
                // Yours (not its parent's, nor its own tasks' notices).
                if u.delegated_by.is_empty() && u.command != "task-notification" {
                    wake |= u.wake;
                    said.push(c6_worker::report_input(
                        child_id,
                        &u.command,
                        &u.text,
                        &u.from_session_id,
                        &u.from_title,
                        u.api_origin.as_ref(),
                        u.human_origin,
                    ));
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
    taken.push(turn_report_id(child_id, end_seq));
    let settled: Vec<(String, String)> =
        st.store.waiting_delegations(child_id).await?.into_iter().filter(|(cmd, _)| taken.contains(cmd)).collect();
    if taken.len() == 1 && settled.is_empty() {
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
        task_notice: Some(c6_worker::task_report_notice(&child.title, child_id, reason, &report)),
    };
    let (steers, calls): (Vec<_>, Vec<_>) = settled.into_iter().partition(|(_, call)| call.is_empty());
    let mut steers: Vec<String> = steers.into_iter().map(|(cmd, _)| cmd).collect();
    if calls.is_empty() {
        let id = format!("subagent:{child_id}:{end_seq}:notice");
        let wake = wake || delegated || automatic || said.is_empty();
        send(st, &parent_id, &id, &steers, ended("", said, wake)).await?;
    }
    for (command_id, call_id) in calls {
        let id = format!("subagent:{child_id}:{end_seq}:{call_id}");
        // A turn's normal waiter also covers steers it took. Steers still
        // queued for the next turn retain their own obligations.
        let mut inputs = std::mem::take(&mut steers);
        inputs.push(command_id);
        send(st, &parent_id, &id, &inputs, ended(&call_id, Vec::new(), false)).await?;
    }
    clear_delivery_failure(st, child_id, &end_seq.to_string()).await;
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
            if failed || cmd == command_id {
                let digest = format!(
                    "<subagent id=\"{session_id}\" status=\"failed\" turns=\"0\" tool_calls=\"0\" elapsed=\"0s\">\nErrors: 1 (last: {why})\n</subagent>"
                );
                let ended = DelegationEnded {
                    wake: call_id.is_empty(),
                    call_id,
                    child_session_id: session_id.to_owned(),
                    reason: TurnEndReason::Error as i32,
                    digest,
                    task_notice: Some(c6_worker::task_report_notice(&child.title, session_id, TurnEndReason::Error, why)),
                    ..Default::default()
                };
                let id = format!("subagent:rejected:{cmd}");
                send(st, &parent_id, &id, &[cmd], ended).await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        warn!(session = %session_id, "telling a remote subagent's parent of a rejected command: {e:#}");
        delivery_failed(st, session_id, &format!("rejected:{command_id}"), &e).await;
    } else {
        clear_delivery_failure(st, session_id, &format!("rejected:{command_id}")).await;
    }
}

/// Queues `ended` for the parent: delivered when its worker is connected,
/// and again until the parent's log says it got it.
async fn send(st: &HeadState, parent_id: &str, id: &str, inputs: &[String], ended: DelegationEnded) -> anyhow::Result<()> {
    let parent = st.store.session(parent_id).await?.ok_or_else(|| anyhow::anyhow!("parent session {parent_id} is missing"))?;
    let worker_id: WorkerId = parent.worker_id.parse()?;
    let cmd = SessionCommand {
        command_id: id.to_owned(),
        session_id: parent_id.to_owned(),
        kind: Some(session_command::Kind::DelegationEnded(ended)),
    };
    st.store
        .enqueue_subagent_report(&NewCommand { id, session_id: parent_id, worker_id, data: cmd.encode_to_vec(), text: None }, inputs)
        .await?;
    st.registry.deliver(worker_id);
    Ok(())
}
