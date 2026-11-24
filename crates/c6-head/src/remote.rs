//! Remote subagents (docs/remote-subagents.md): sessions the head starts
//! from a template for another session's `Task` call, perhaps on another
//! worker. The head holds the link: it sends the subagent its parent's
//! messages, and the parent how each of the subagent's turns ended.

use std::collections::{BTreeMap, HashMap};

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
            let waiting = child.status == "idle" && has_counted_background(st, &child.id).await?;
            // `running` describes the foreground turn, not whether the child
            // has more work. SendMessage uses it to choose steer vs new turn.
            let status = if waiting { "waiting" } else { child.status.as_str() };
            Ok(json!({"running": child.status == "running", "status": status, "end_reason": child.end_reason}))
        }
        "peek" => {
            let child = own_child(st, parent, &s("to")).await?;
            let status = state_of(st, &child).await?;
            Ok(json!({"text": digest(st, &child.id, status).await?}))
        }
        _ => Err(bad_request(format!("unknown method session.subagent_{what}"))),
    }
}

pub(crate) const LOCAL_WAIT_PREFIX: &str = "subagent:local-wait:delegation:";

/// One of `parent`'s remote subagents, by session id.
async fn own_child(st: &HeadState, parent: &SessionRow, to: &str) -> anyhow::Result<SessionRow> {
    match st.store.session(to).await? {
        Some(c) if c.parent_id.as_deref() == Some(parent.id.as_str()) && !c.remote_template.is_empty() => Ok(c),
        _ => Err(bad_request(format!("no remote subagent {to:?} was started by this session"))),
    }
}

/// A subagent's state as its digest names it.
async fn state_of(st: &HeadState, s: &SessionRow) -> anyhow::Result<&'static str> {
    if s.status == "running" {
        return Ok("running");
    }
    if s.status == "idle" && has_counted_background(st, &s.id).await? {
        return Ok("waiting");
    }
    Ok(match s.end_reason.as_str() {
        "" => "not started",
        "completed" => "completed",
        "interrupted" => "interrupted",
        "worker_restarted" => "lost",
        _ => "failed",
    })
}

async fn has_counted_background(st: &HeadState, session_id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM running_tasks WHERE session_id = ? AND counted = 1)")
        .bind(session_id)
        .fetch_one(st.store.pool())
        .await?)
}

/// What a subagent did, from the head's copy of its log.
async fn digest(st: &HeadState, child_id: &str, status: &str) -> anyhow::Result<String> {
    let events = st.store.events_after(child_id, 0, i64::MAX).await?;
    let mut states = HashMap::new();
    for id in c6_worker::spawned_children(&events) {
        if let Some(s) = st.store.session(&id).await? {
            states.insert(id, state_of(st, &s).await?.to_owned());
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
                ownership_epoch: 0,
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

fn turn_report_seq(child: &str, id: &str) -> Option<u64> {
    let (prefix, seq) = id.strip_prefix("subagent:turn:")?.rsplit_once(':')?;
    (prefix == child).then_some(())?;
    seq.parse::<u64>().ok().filter(|&seq| seq != 0)
}

/// A background-only completion has no TurnEnded of its own. Revisit the
/// latest deferred turn unless the child is working on a new turn.
pub(crate) async fn background_ended(st: &HeadState, session_id: &str) {
    if let Err(e) = flush_pending_turn(st, session_id).await {
        warn!(session = %session_id, "checking a remote subagent's deferred report: {e:#}");
    }
}

async fn flush_pending_turn(st: &HeadState, child: &str) -> anyhow::Result<()> {
    let Some(session) = st.store.session(child).await? else { return Ok(()) };
    if session.parent_id.is_none() || session.remote_template.is_empty() || session.status == "running" {
        return Ok(());
    }
    let pending = st.store.waiting_delegations(child).await?;
    if let Some(seq) = pending.iter().filter_map(|(id, _)| turn_report_seq(child, id)).max() {
        let events = st.store.events_after(child, seq - 1, 1).await?;
        if let Some(event) = events.first().filter(|event| event.seq == seq)
            && let Some(session_event::Kind::TurnEnded(t)) = &event.kind
        {
            turn_ended(st, child, seq, t.reason).await;
        }
    } else if !pending.is_empty() {
        // Older logs can have an input obligation but no per-turn marker.
        let mut taken = false;
        for event in st.store.events_after(child, 0, i64::MAX).await? {
            match event.kind {
                Some(session_event::Kind::User(u)) if pending.iter().any(|(id, _)| id == &u.command_id) => taken = true,
                Some(session_event::Kind::TurnEnded(t)) if taken => {
                    turn_ended(st, child, event.seq, t.reason).await;
                    taken = false;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Ingest commits the child's events before running its hooks. If the
/// head stopped between those operations, replay outstanding obligations
/// from that durable log rather than waiting for another child turn.
pub async fn recover(st: &HeadState) -> anyhow::Result<()> {
    let children: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT child_id FROM remote_delegations WHERE state = 'waiting'").fetch_all(st.store.pool()).await?;
    for child in children {
        flush_pending_turn(st, &child).await?;
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
    if child.status == "running" || has_counted_background(st, child_id).await? {
        // Keep the turn marker and the inputs it took. An ended foreground
        // turn is not the end of a child with outstanding background work.
        return Ok(());
    }
    let pending = st.store.waiting_delegations(child_id).await?;
    let mut earlier: Vec<_> = pending.iter().filter_map(|(id, _)| turn_report_seq(child_id, id)).collect();
    if earlier.iter().any(|&seq| seq > end_seq) {
        // Ingest commits an entire batch before running its hooks. Only the
        // newest end should flush this group if several landed together.
        return Ok(());
    }
    earlier.retain(|&seq| seq < end_seq);
    earlier.sort_unstable();
    let mut events = Vec::new();
    for seq in earlier.iter().copied().chain(std::iter::once(end_seq)) {
        events.extend(st.store.since_last_turn(child_id, seq).await?);
    }
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
            Some(session_event::Kind::TurnEnded(_)) if e.seq != end_seq => report.clear(),
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
    taken.extend(earlier.iter().map(|seq| turn_report_id(child_id, *seq)));
    let settled: Vec<(String, String)> = pending.into_iter().filter(|(cmd, _)| taken.contains(cmd)).collect();
    if settled.is_empty() {
        return Ok(());
    }
    let mut reason = TurnEndReason::try_from(reason).unwrap_or(TurnEndReason::Unspecified);
    // If the last background task finished without another foreground turn,
    // don't tell the parent only what the child said before that task ended.
    let mut task_outcomes = Vec::new();
    if child.last_seq as u64 > end_seq {
        for e in st.store.events_after(child_id, end_seq, i64::MAX).await? {
            if let Some(session_event::Kind::BackgroundTask(t)) = e.kind
                && t.state != "started"
                && (t.notify || t.kind == "claude_code")
            {
                if reason == TurnEndReason::Completed {
                    reason = match t.state.as_str() {
                        "lost" => TurnEndReason::WorkerRestarted,
                        "stopped" | "stopped_by_user" => TurnEndReason::Interrupted,
                        "failed" => TurnEndReason::Error,
                        _ if t.exit_code != 0 => TurnEndReason::Error,
                        _ => reason,
                    };
                }
                task_outcomes.push(format!("{} ({}): {}", t.description, t.task_id, t.state));
            }
        }
    }
    let task_outcomes = (!task_outcomes.is_empty()).then(|| format!("Background work ended:\n{}", task_outcomes.join("\n")));
    if reason == TurnEndReason::Completed
        && let Some(tasks) = &task_outcomes
    {
        report = if report.is_empty() { tasks.clone() } else { format!("{report}\n\n{tasks}") };
    }
    let digest = if reason == TurnEndReason::Completed && !report.trim().is_empty() {
        String::new()
    } else {
        let mut progress = digest(st, child_id, c6_worker::status_of(reason, &report)).await?;
        if let Some(tasks) = task_outcomes {
            if !report.is_empty() {
                report.push_str("\n\n");
            }
            report.push_str(&tasks);
            progress.push_str(&format!("\n{tasks}"));
        }
        progress
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
    let (steers, calls) = group_obligations(settled);
    if calls.is_empty() {
        let id = format!("subagent:{child_id}:{end_seq}:notice");
        let wake = wake || delegated || automatic || said.is_empty();
        send(st, &parent_id, &id, &steers, ended("", said, wake)).await?;
    }
    let last_call = calls.keys().next_back().cloned();
    let mut steers = steers;
    for (call_id, mut inputs) in calls {
        let id = format!("subagent:{child_id}:{end_seq}:{call_id}");
        // A turn's normal waiter also covers steers it took. Steers still
        // queued for the next turn retain their own obligations.
        // Retire the turn markers with the last waiter, so a crash sending
        // the preceding reports leaves the turns discoverable on startup.
        if Some(&call_id) == last_call.as_ref() {
            inputs.append(&mut steers);
        }
        send(st, &parent_id, &id, &inputs, ended(&call_id, Vec::new(), false)).await?;
    }
    clear_delivery_failure(st, child_id, &end_seq.to_string()).await;
    for seq in earlier {
        clear_delivery_failure(st, child_id, &seq.to_string()).await;
    }
    Ok(())
}

fn group_obligations(pending: Vec<(String, String)>) -> (Vec<String>, BTreeMap<String, Vec<String>>) {
    let mut steers = Vec::new();
    let mut calls = BTreeMap::<String, Vec<String>>::new();
    for (cmd, call) in pending {
        if call.is_empty() {
            steers.push(cmd);
        } else {
            calls.entry(call).or_default().push(cmd);
        }
    }
    (steers, calls)
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

/// Queue a recovery warning as a distinct report, not a completion of an
/// earlier invocation. The child may still finish after the owner retries.
pub(crate) async fn recovery_notice(
    st: &HeadState,
    child: &SessionRow,
    parent: &str,
    status: &c6_api::SessionRecovery,
) -> anyhow::Result<()> {
    let prompt = if status.automatic { "session.subagent_recovery_waiting" } else { "session.subagent_recovery_stuck" };
    let text = crate::prompts::get(st, prompt, &child.realm).await.replace("{{child}}", &child.id).replace("{{reason}}", &status.reason);
    send(
        st,
        parent,
        &format!("recovery:{}:{}", child.id, status.incident),
        &[],
        DelegationEnded {
            child_session_id: child.id.clone(),
            // As with liveness observations, a resource wait is not a failed
            // relaunch or a delegation outcome. Keep the real call pending.
            reason: if status.automatic { TurnEndReason::Completed } else { TurnEndReason::Error } as i32,
            task_notice: Some(if status.automatic {
                c6_proto::TaskNotice {
                    kind: "recovery".into(),
                    title: child.title.clone(),
                    session_id: child.id.clone(),
                    status: "waiting".into(),
                    markdown: text.clone(),
                }
            } else {
                c6_worker::task_report_notice(&child.title, &child.id, TurnEndReason::Error, &text)
            }),
            report: if status.automatic { text.clone() } else { String::new() },
            said: if status.automatic { vec!["Recovery notice (no subagent completion):".into()] } else { Vec::new() },
            digest: text,
            wake: true,
            ..Default::default()
        },
    )
    .await
}

/// Lose the dependency as well as notifying the parent. Remote tool waiters
/// take this as an error result; otherwise the parent's durable notice path
/// wakes it (including when a local worker never recorded its own result).
pub(crate) async fn liveness_notice(st: &HeadState, child: &SessionRow, incident: &str, cause: &str) -> anyhow::Result<()> {
    let (Some(parent), Some(original_call)) = (child.parent_id.as_deref(), child.parent_call_id.as_deref()) else {
        return Ok(());
    };
    let prefix = format!("liveness:lost:{}:{incident}:", child.id);
    // Include only obligations that existed at the observation. In particular,
    // retrying an old report must not settle a newly queued continuation.
    let pending = sqlx::query_as(
        "SELECT command_id, call_id FROM remote_delegations WHERE child_id = ?1 AND state = 'waiting' \
         AND created_at <= COALESCE((SELECT observed_at FROM liveness_incidents WHERE id = ?2), ?3) AND parent_id = ?4",
    )
    .bind(&child.id)
    .bind(incident)
    .bind(c6_core::now_ms())
    .bind(parent)
    .fetch_all(st.store.pool())
    .await?;
    let (mut steers, mut calls) = group_obligations(pending);
    if child.remote_template.is_empty() {
        let events = st.store.events_after(&child.id, 0, i64::MAX).await?;
        let tracked: Vec<String> = sqlx::query_scalar(
            "SELECT command_id FROM remote_delegations WHERE child_id = ? AND parent_id = ? AND command_id LIKE 'subagent:local-wait:%'",
        )
        .bind(&child.id)
        .bind(parent)
        .fetch_all(st.store.pool())
        .await?;
        let tracked: std::collections::HashSet<_> = tracked.into_iter().collect();
        // Include foreground and legacy local waiters as well. Registered
        // background waits are authoritative even before the child's User.
        for call in local_pending_calls(&events, &child.id, child.last_seq as u64, |u| {
            tracked.contains(&format!("subagent:local-wait:{}", u.command_id))
        }) {
            calls.entry(call).or_default();
        }
    }
    if calls.is_empty() {
        // The last command in a multi-waiter report retires all the remaining
        // obligations. Do not send an extra fallback on replay after commit.
        let sent: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM commands WHERE substr(id, 1, length(?)) = ?)")
            .bind(&prefix)
            .bind(&prefix)
            .fetch_one(st.store.pool())
            .await?;
        if sent && steers.is_empty() {
            return Ok(());
        }
        let call = if child.remote_template.is_empty() && child.last_seq <= 1 { original_call.to_owned() } else { String::new() };
        calls.insert(call, Vec::new());
    }
    let progress = digest(st, &child.id, "lost").await?;
    let template = crate::prompts::get(st, "session.liveness_lost", &child.realm).await;
    let text = api::prompts::fill(&template, &[("child", &child.id), ("reason", cause), ("progress", &progress)]);
    let reason = TurnEndReason::WorkerRestarted; // The existing wire classification for “lost”; the prompt carries the actual cause.
    let last_call = calls.keys().next_back().cloned();
    for (call_id, mut inputs) in calls {
        if Some(&call_id) == last_call.as_ref() {
            inputs.append(&mut steers);
        }
        let id = format!("{prefix}{call_id}");
        send(
            st,
            parent,
            &id,
            &inputs,
            DelegationEnded {
                call_id,
                child_session_id: child.id.clone(),
                reason: reason as i32,
                task_notice: Some(c6_worker::task_report_notice(&child.title, &child.id, reason, &text)),
                digest: text.clone(),
                wake: true,
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

fn local_pending_calls(
    events: &[c6_proto::SessionEvent],
    child: &str,
    through: u64,
    tracked: impl Fn(&c6_proto::UserMessage) -> bool,
) -> std::collections::BTreeSet<String> {
    let prefix = format!("subagent:local:{child}:");
    let reported = events
        .iter()
        .filter(|e| e.seq <= through)
        .filter_map(|e| match &e.kind {
            Some(session_event::Kind::SubagentReported(r)) => r.command_id.strip_prefix(&prefix)?.parse::<u64>().ok(),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    events
        .iter()
        .filter(|e| e.seq > reported && e.seq <= through)
        .filter_map(|e| match &e.kind {
            Some(session_event::Kind::User(u)) if !u.delegated_by.is_empty() && u.blocked.is_empty() && !tracked(u) => {
                Some(u.delegated_by.clone())
            }
            _ => None,
        })
        .collect()
}

/// A wakeable, ordinary notice, not a failed child or a completed dependency.
/// Use a reserved synthetic source (not a real child or the parent itself)
/// so the notice never satisfies a Task group or competes for a tool waiter.
pub(crate) async fn silence_notice(st: &HeadState, parent: &SessionRow, incident: &str, pending: &str) -> anyhow::Result<()> {
    let template = crate::prompts::get(st, "session.liveness_silence", &parent.realm).await;
    let text = api::prompts::fill(&template, &[("pending", pending)]);
    let id = format!("liveness:silence:{}:{incident}", parent.id);
    send(
        st,
        &parent.id,
        &id,
        &[],
        DelegationEnded {
            child_session_id: id.clone(),
            // DelegationEnded has no neutral wire reason. Use a success
            // envelope so the receipt is not counted as a failed relaunch.
            reason: TurnEndReason::Completed as i32,
            task_notice: Some(c6_proto::TaskNotice {
                kind: "liveness".into(),
                title: "Pending subagents".into(),
                status: "waiting".into(),
                markdown: text.clone(),
                ..Default::default()
            }),
            report: text,
            said: vec!["Liveness notice (no subagent completion):".into()],
            wake: true,
            ..Default::default()
        },
    )
    .await
}

/// Queues `ended` for the parent: delivered when its worker is connected,
/// and again until the parent's log says it got it.
async fn send(st: &HeadState, parent_id: &str, id: &str, inputs: &[String], ended: DelegationEnded) -> anyhow::Result<()> {
    let parent = st.store.session(parent_id).await?.ok_or_else(|| anyhow::anyhow!("parent session {parent_id} is missing"))?;
    let worker_id: WorkerId = parent.worker_id.parse()?;
    let cmd = SessionCommand {
        command_id: id.to_owned(),
        session_id: parent_id.to_owned(),
        ownership_epoch: 0,
        kind: Some(session_command::Kind::DelegationEnded(ended)),
    };
    st.store
        .enqueue_subagent_report(&NewCommand { id, session_id: parent_id, worker_id, data: cmd.encode_to_vec(), text: None }, inputs)
        .await?;
    st.registry.deliver(worker_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_loss_includes_idle_background_waiters_but_not_settled_or_new_inputs() {
        use c6_proto::{SessionEvent, SubagentReported, UserMessage};
        let events = [
            session_event::Kind::User(UserMessage { delegated_by: "settled".into(), ..Default::default() }),
            session_event::Kind::TurnEnded(Default::default()),
            session_event::Kind::SubagentReported(SubagentReported { command_id: "subagent:local:child:2".into(), ..Default::default() }),
            session_event::Kind::User(UserMessage { delegated_by: "waiting-a".into(), ..Default::default() }),
            session_event::Kind::TurnEnded(Default::default()),
            session_event::Kind::User(UserMessage { delegated_by: "waiting-b".into(), ..Default::default() }),
            session_event::Kind::User(UserMessage { delegated_by: "new-input".into(), ..Default::default() }),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SessionEvent { seq: i as u64 + 1, kind: Some(kind), ..Default::default() })
        .collect::<Vec<_>>();
        assert_eq!(local_pending_calls(&events, "child", 6, |_| false).into_iter().collect::<Vec<_>>(), ["waiting-a", "waiting-b"]);
    }

    #[test]
    fn deferred_markers_identify_the_child_and_turn() {
        assert_eq!(turn_report_seq("child", &turn_report_id("child", 42)), Some(42));
        assert_eq!(turn_report_seq("other", &turn_report_id("child", 42)), None);
        assert_eq!(turn_report_seq("child", "subagent:turn:child:0"), None);
        assert_eq!(turn_report_seq("child", "subagent:input:child:42"), None);
    }

    #[test]
    fn one_report_can_retire_multiple_obligations_for_the_same_waiter() {
        let (steers, calls) = group_obligations(
            [("turn-1", ""), ("input-1", "call-a"), ("input-2", "call-b"), ("input-3", "call-a"), ("turn-2", "")]
                .into_iter()
                .map(|(input, call)| (input.into(), call.into()))
                .collect(),
        );
        assert_eq!(steers, ["turn-1", "turn-2"]);
        assert_eq!(calls["call-a"], ["input-1", "input-3"]);
        assert_eq!(calls["call-b"], ["input-2"]);
    }
}
