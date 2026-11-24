//! Deterministic evidence and bounded waits (docs/liveness.md). No model calls.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_core::{WorkerId, now_ms};
use c6_proto::{HeldProcessState, SessionActivityKind, SessionActorState, SessionLivenessReport};
use serde_json::{Value, json};
use sqlx::Row;

use crate::HeadState;
use crate::registry::ConnectionStamp;
use crate::store::SessionRow;

#[derive(Clone)]
struct Evidence {
    worker: WorkerId,
    connection: u64,
    received: i64,
    generation: u64,
    changed: i64,
    report: c6_proto::SessionLiveness,
}

impl Evidence {
    fn fresh_after(&self, progress: Option<Progress>) -> bool {
        progress.is_none_or(|p| self.generation > p.generation)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Progress {
    at: i64,
    generation: u64,
}

#[derive(Default)]
pub(crate) struct Runtime {
    evidence: Mutex<HashMap<String, Evidence>>,
    disconnected: Mutex<HashMap<WorkerId, (ConnectionStamp, i64)>>,
    /// Sweep is also a public test hook; serialize it with scheduled sweeps.
    sweep: tokio::sync::Mutex<()>,
    changed: tokio::sync::Notify,
    progress: Mutex<HashMap<String, Progress>>,
    // Allocated under the liveness commit lock; timestamps only measure elapsed time.
    generation: AtomicU64,
}

pub(crate) async fn progress(st: &HeadState, session: &str) {
    let _fence = st.registry.liveness_commit.lock().await;
    let generation = st.liveness.generation.fetch_add(1, Ordering::Relaxed);
    st.liveness.progress.lock().unwrap().insert(session.to_owned(), Progress { at: now_ms(), generation });
}

/// Called on the control lane, never behind worker event persistence. The
/// narrow loss commit fence orders a newer report against a declaration.
pub(crate) async fn report(st: &HeadState, worker: WorkerId, connection: u64, report: SessionLivenessReport) {
    let _fence = st.registry.liveness_commit.lock().await;
    if st.registry.get(worker).is_none_or(|c| c.conn_id != connection) {
        return;
    }
    let now = now_ms();
    let generation = st.liveness.generation.fetch_add(1, Ordering::Relaxed);
    let mut book = st.liveness.evidence.lock().unwrap();
    for r in report.sessions {
        let changed = book
            .get(&r.session_id)
            .filter(|old| {
                old.worker == worker
                    && old.connection == connection
                    && old.report.ownership_epoch == r.ownership_epoch
                    && old.report.activity_kind == r.activity_kind
                    && (old.report.actor_state == r.actor_state
                        || matches!(
                            SessionActorState::try_from(old.report.actor_state),
                            Ok(SessionActorState::Running | SessionActorState::Unresponsive)
                        ) && matches!(
                            SessionActorState::try_from(r.actor_state),
                            Ok(SessionActorState::Running | SessionActorState::Unresponsive)
                        ))
            })
            .map_or(now, |old| old.changed);
        book.insert(r.session_id.clone(), Evidence { worker, connection, received: now, generation, changed, report: r });
    }
    drop(book);
    st.liveness.changed.notify_one();
}

pub(crate) fn spawn(st: Arc<HeadState>) {
    tokio::spawn(async move {
        let mut workers = st.registry.subscribe();
        loop {
            let seconds = crate::sessions::subagent_limits(&st, None).await.unwrap_or_default().liveness_sweep_seconds.max(1);
            tokio::select! {
                _ = st.shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(seconds.into())) => {},
                _ = st.liveness.changed.notified() => {},
                _ = workers.changed() => {},
            }
            if let Err(error) = sweep_at(&st, now_ms()).await {
                tracing::warn!(%error, "liveness sweep failed; retrying on next sweep");
            }
        }
    });
}

fn elapsed(now: i64, then: i64) -> i64 {
    now.saturating_sub(then).max(0)
}

/// Explicit observation time makes grace and silence tests deterministic.
/// A declaration never manufactures a sequence in the worker's journal.
pub async fn sweep_at(st: &HeadState, now: i64) -> anyhow::Result<()> {
    let _sweep = st.liveness.sweep.lock().await;
    let sessions: Vec<SessionRow> =
        sqlx::query_as("SELECT * FROM sessions WHERE archived = 0 AND observed = 0").fetch_all(st.store.pool()).await?;
    let evidence = st.liveness.evidence.lock().unwrap().clone();
    let progress = st.liveness.progress.lock().unwrap().clone();
    let pending: HashSet<String> = sqlx::query_scalar("SELECT DISTINCT child_id FROM remote_delegations WHERE state = 'waiting'")
        .fetch_all(st.store.pool())
        .await?
        .into_iter()
        .collect();
    let tasks: HashSet<String> = sqlx::query_scalar("SELECT DISTINCT session_id FROM running_tasks WHERE counted = 1")
        .fetch_all(st.store.pool())
        .await?
        .into_iter()
        .collect();
    // Continued work resolves an earlier observation, never an unrelated
    // metadata edit or another heartbeat from the same dead incarnation.
    let resolved = sqlx::query(
        "UPDATE liveness_incidents SET resolved_at = ? WHERE resolved_at IS NULL AND EXISTS \
         (SELECT 1 FROM sessions s WHERE s.id = session_id AND (s.last_seq > liveness_incidents.last_seq \
          OR (liveness_incidents.worker_id IS NOT NULL AND (s.worker_id <> liveness_incidents.worker_id \
              OR s.ownership_epoch <> liveness_incidents.ownership_epoch))))",
    )
    .bind(now)
    .execute(st.store.pool())
    .await?;
    if resolved.rows_affected() != 0 {
        st.registry.bump();
        st.attention.changed();
    }
    let mut limits = HashMap::new();
    for s in &sessions {
        if let std::collections::hash_map::Entry::Vacant(entry) = limits.entry(s.realm.clone()) {
            entry.insert(crate::sessions::subagent_limits(st, Some(&s.realm)).await?);
        }
        let waiting = s.status == "running" || s.notices_proceeding || pending.contains(&s.id) || tasks.contains(&s.id);
        if !waiting {
            continue;
        }
        let l = &limits[&s.realm];
        let worker: WorkerId = s.worker_id.parse()?;
        let stamp = st.registry.connection_stamp(worker);
        let mut checked_evidence = None;
        let cause = if let Some(connection) = stamp.live {
            st.liveness.disconnected.lock().unwrap().remove(&worker);
            // Reports from superseded connections/owners cannot certify work.
            let e = evidence
                .get(&s.id)
                .filter(|e| e.worker == worker && e.connection == connection && e.report.ownership_epoch as i64 == s.ownership_epoch);
            if let Some(e) = e {
                checked_evidence = Some(e);
                let r = &e.report;
                let last: Option<i64> =
                    sqlx::query_scalar("SELECT MAX(ts) FROM events WHERE session_id = ?").bind(&s.id).fetch_one(st.store.pool()).await?;
                let silent = elapsed(now, last.unwrap_or(s.created_at).max(e.changed).max(progress.get(&s.id).map_or(0, |p| p.at)));
                let actor = SessionActorState::try_from(r.actor_state).unwrap_or(SessionActorState::Unspecified);
                let current = e.fresh_after(progress.get(&s.id).copied());
                match actor {
                    // Resource recovery may wait indefinitely. A previously
                    // exited held process is not evidence that this actor died.
                    SessionActorState::WaitingForDisk => None,
                    SessionActorState::Stopped if current => Some(format!("session actor stopped: {}", r.reason)),
                    _ if current && r.held_process == HeldProcessState::Exited as i32 => Some("held process exited".into()),
                    SessionActorState::Unresponsive if current => Some(format!("session actor unresponsive: {}", r.reason)),
                    SessionActorState::Recovering if current && elapsed(now, e.changed) >= i64::from(l.session_recovery_seconds) * 1000 => {
                        Some(format!("session recovering beyond {} seconds: {}", l.session_recovery_seconds, r.reason))
                    }
                    SessionActorState::Idle if current && s.status == "running" && r.running_background_tasks == 0 => {
                        // Event persistence and the control lane can race. A
                        // second sweep must confirm this is not a turn end
                        // merely in flight on the ordered persistence lane.
                        (elapsed(now, e.changed) >= i64::from(l.liveness_sweep_seconds) * 1000)
                            .then(|| "turn marked running but its actor is idle with no live work".into())
                    }
                    SessionActorState::Running
                        if current
                            && r.activity_kind != SessionActivityKind::Tool as i32
                            && silent >= i64::from(l.liveness_model_silence_seconds) * 1000 =>
                    {
                        Some(format!("no model progress for {} seconds; last activity at {}", silent / 1000, last.unwrap_or(e.changed)))
                    }
                    _ => None,
                }
            } else {
                // Missing evidence is not actor death. An upgraded worker
                // that cannot report still needs a bounded visible warning.
                None
            }
        } else {
            let since = {
                let mut disconnected = st.liveness.disconnected.lock().unwrap();
                let entry = disconnected.entry(worker).or_insert((stamp, now));
                if entry.0 != stamp {
                    *entry = (stamp, now);
                }
                entry.1
            };
            (elapsed(now, since) >= i64::from(l.liveness_worker_grace_seconds) * 1000)
                .then(|| format!("worker disconnected for {} seconds", elapsed(now, since) / 1000))
        };
        if let Some(cause) = cause {
            let fence = LossFence { worker, stamp, evidence: checked_evidence, progress: progress.get(&s.id).copied() };
            record_loss(st, s, fence, &cause, now).await?;
        }
    }
    for parent in &sessions {
        let mut tree: HashSet<&str> = HashSet::from([parent.id.as_str()]);
        // Include descendants at every depth, including idle intermediates.
        loop {
            let old = tree.len();
            for s in &sessions {
                if s.parent_id.as_deref().is_some_and(|p| tree.contains(p)) {
                    tree.insert(&s.id);
                }
            }
            if tree.len() == old {
                break;
            }
        }
        let descendants: Vec<&SessionRow> = sessions
            .iter()
            .filter(|s| {
                s.id != parent.id
                    && tree.contains(s.id.as_str())
                    && s.parent_call_id.is_some()
                    && (pending.contains(&s.id) || s.status == "running" || s.notices_proceeding || tasks.contains(&s.id))
            })
            .collect();
        if descendants.is_empty() {
            continue;
        }
        let last = sessions
            .iter()
            .filter(|s| tree.contains(s.id.as_str()))
            .map(|s| s.updated_at.max(progress.get(&s.id).map_or(0, |p| p.at)))
            .max()
            .unwrap_or(parent.updated_at);
        let window = i64::from(limits[&parent.realm].liveness_tree_silence_seconds) * 1000;
        if elapsed(now, last) < window {
            continue;
        }
        let pending =
            descendants.iter().map(|s| format!("{} ({}) pending since {}", s.id, s.title, s.updated_at)).collect::<Vec<_>>().join("\n");
        record_silence(st, parent, &pending, now).await?;
    }
    // Retry durable incidents whose parent could not be queued last time.
    let undelivered = sqlx::query(
        "SELECT i.id, i.session_id, i.kind, i.cause FROM liveness_incidents i JOIN sessions s ON s.id = i.session_id \
         WHERE i.delivered = 0 AND i.resolved_at IS NULL AND (i.kind <> 'lost' OR \
           (i.last_seq = s.last_seq AND (i.worker_id IS NULL OR (i.worker_id = s.worker_id AND i.ownership_epoch = s.ownership_epoch))))",
    )
    .fetch_all(st.store.pool())
    .await?;
    for row in undelivered {
        let Some(s) = sessions.iter().find(|s| s.id == row.get::<String, _>("session_id")) else { continue };
        deliver(st, s, &row.get::<String, _>("id"), &row.get::<String, _>("kind"), &row.get::<String, _>("cause")).await?;
    }
    Ok(())
}

async fn record_silence(st: &HeadState, s: &SessionRow, cause: &str, now: i64) -> anyhow::Result<()> {
    let id = uuid::Uuid::now_v7().to_string();
    let result = sqlx::query(
        "INSERT INTO liveness_incidents (id, session_id, kind, cause, observed_at, last_seq) \
         SELECT ?, ?, 'silence', ?, ?, ? WHERE NOT EXISTS \
         (SELECT 1 FROM liveness_incidents WHERE session_id = ? AND kind = 'silence' AND resolved_at IS NULL)",
    )
    .bind(&id)
    .bind(&s.id)
    .bind(cause)
    .bind(now)
    .bind(s.last_seq)
    .bind(&s.id)
    .execute(st.store.pool())
    .await?;
    if result.rows_affected() > 0 {
        st.registry.bump();
        st.attention.changed();
    }
    Ok(())
}

struct LossFence<'a> {
    worker: WorkerId,
    stamp: ConnectionStamp,
    evidence: Option<&'a Evidence>,
    progress: Option<Progress>,
}

async fn record_loss(st: &HeadState, s: &SessionRow, fence: LossFence<'_>, cause: &str, now: i64) -> anyhow::Result<()> {
    // The write transaction pins the owner and durable journal position until
    // commit. Stage the incident and its projections together before taking
    // the registry's narrow commit lock; never hold it while waiting for a
    // SQLite writer or delivering a report.
    let mut tx = st.store.pool().begin_with("BEGIN IMMEDIATE").await?;
    // A same-millisecond local registration may have committed after the
    // report but not yet published its runtime generation. Wait for new evidence.
    let result = sqlx::query(
        "INSERT INTO liveness_incidents (id, session_id, kind, cause, observed_at, last_seq, worker_id, ownership_epoch) \
         SELECT ?, id, 'lost', ?, ?, last_seq, worker_id, ownership_epoch FROM sessions \
         WHERE id = ? AND worker_id = ? AND ownership_epoch = ? AND last_seq = ? AND archived = 0 AND observed = 0 \
         AND (?8 IS NULL OR NOT EXISTS (SELECT 1 FROM remote_delegations d WHERE d.child_id = sessions.id \
         AND d.parent_id = sessions.parent_id AND d.state = 'waiting' AND d.command_id LIKE 'subagent:local-wait:delegation:%' AND d.created_at >= ?8)) \
         AND NOT EXISTS (SELECT 1 FROM liveness_incidents WHERE session_id = sessions.id AND kind = 'lost' AND resolved_at IS NULL)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(cause)
    .bind(now)
    .bind(&s.id)
    .bind(&s.worker_id)
    .bind(s.ownership_epoch)
    .bind(s.last_seq)
    .bind(fence.evidence.map(|e| e.received))
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(());
    }
    // An affirmative death can close the projection without inventing a
    // worker sequence. Do not transition a goal if no running row was closed.
    let mut goal_changed = false;
    if cause.starts_with("session actor stopped:") || cause.starts_with("turn marked running") || cause == "held process exited" {
        let closed = sqlx::query(
            "UPDATE sessions SET status = 'idle', error = ?, end_reason = 'error', turn_ended_at = ? WHERE id = ? AND status = 'running'",
        )
        .bind(cause)
        .bind(now)
        .bind(&s.id)
        .execute(&mut *tx)
        .await?;
        if closed.rows_affected() != 0 {
            goal_changed =
                sqlx::query("UPDATE goals SET state = 'needs_you', why = ?, updated_at = ? WHERE session_id = ? AND state = 'active'")
                    .bind(cause)
                    .bind(now)
                    .bind(&s.id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                    != 0;
        }
    }
    let commit = st.registry.liveness_commit.lock().await;
    let evidence_current = fence.evidence.is_none_or(|selected| {
        selected.fresh_after(fence.progress)
            && st.liveness.evidence.lock().unwrap().get(&s.id).is_some_and(|latest| latest.generation == selected.generation)
    });
    if st.registry.connection_stamp(fence.worker) != fence.stamp
        || !evidence_current
        || st.liveness.progress.lock().unwrap().get(&s.id).copied() != fence.progress
    {
        drop(commit);
        tx.rollback().await?;
        return Ok(());
    }
    tx.commit().await?;
    drop(commit);
    if goal_changed {
        crate::goals::liveness_lost(st, s, cause);
    }
    st.registry.bump();
    st.attention.changed();
    Ok(())
}

async fn deliver(st: &HeadState, s: &SessionRow, id: &str, kind: &str, cause: &str) -> anyhow::Result<()> {
    if kind == "silence" {
        crate::remote::silence_notice(st, s, id, cause).await?;
    } else {
        crate::remote::liveness_notice(st, s, id, cause).await?;
        // Always inform the user as well: notice guards or an offline parent
        // must not turn the failure into another invisible wait.
        let template = crate::prompts::get(st, "session.liveness_notification", &s.realm).await;
        let message = c6_api::prompts::fill(&template, &[("child", &s.id), ("reason", cause)]);
        st.notifier.send(crate::notify::Note {
            event: "failed",
            title: format!("Session lost: {}", s.title),
            message,
            priority: "high",
            session_id: Some(s.id.clone()),
            labels: s.all_labels(),
            ..Default::default()
        });
    }
    sqlx::query("UPDATE liveness_incidents SET delivered = 1 WHERE id = ?").bind(id).execute(st.store.pool()).await?;
    Ok(())
}

pub(crate) async fn is_lost(st: &HeadState, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM active_liveness_losses WHERE session_id = ?)")
        .bind(id)
        .fetch_one(st.store.pool())
        .await?)
}

/// Read-only, realm-filterable evidence and retained incident history.
pub async fn snapshot(st: &HeadState) -> anyhow::Result<Value> {
    let rows = sqlx::query(
        "SELECT i.*, s.realm, s.title FROM liveness_incidents i JOIN sessions s ON s.id = i.session_id ORDER BY observed_at DESC LIMIT 200",
    )
    .fetch_all(st.store.pool())
    .await?;
    let incidents: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String,_>("id"), "session_id": r.get::<String,_>("session_id"), "realm": r.get::<String,_>("realm"),
                "title": r.get::<String,_>("title"), "kind": r.get::<String,_>("kind"), "cause": r.get::<String,_>("cause"),
                "observed_at": r.get::<i64,_>("observed_at"), "resolved_at": r.get::<Option<i64>,_>("resolved_at"),
            })
        })
        .collect();
    let sessions: Vec<SessionRow> =
        sqlx::query_as("SELECT * FROM sessions WHERE archived = 0 AND observed = 0").fetch_all(st.store.pool()).await?;
    let book = st.liveness.evidence.lock().unwrap();
    let evidence: Vec<Value> = sessions.iter().map(|s| {
        let worker = s.worker_id.parse::<WorkerId>().ok();
        let connection = worker.and_then(|w| st.registry.get(w));
        let e = book.get(&s.id).filter(|e| Some(e.worker) == worker
            && e.report.ownership_epoch as i64 == s.ownership_epoch
            && connection.as_ref().is_some_and(|c| c.conn_id == e.connection));
        json!({
            "session_id": s.id, "realm": s.realm, "worker_id": s.worker_id, "connected": connection.is_some(),
            "turn_status": s.status, "last_activity_at": s.updated_at, "received_at": e.map(|e| e.received),
            "actor_state": e.map(|e| SessionActorState::try_from(e.report.actor_state).unwrap_or(SessionActorState::Unspecified).as_str_name())
                .unwrap_or("SESSION_ACTOR_STATE_UNSPECIFIED"),
            "reason": e.map(|e| e.report.reason.as_str()).unwrap_or("no current worker evidence"),
            "activity_kind": e.map_or(0, |e| e.report.activity_kind), "held_process": e.map_or(0, |e| e.report.held_process),
            "running_background_tasks": e.map_or(0, |e| e.report.running_background_tasks),
        })
    }).collect();
    Ok(json!({"incidents": incidents, "sessions": evidence}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disk_wait_is_not_terminal_even_after_recovery_deadline_or_held_process_exit() {
        let (_dir, head, _mock, _work) = crate::work::tests::routing_fixture().await;
        let st = &head.state;
        let sweep = st.liveness.sweep.lock().await;
        let worker = WorkerId::new();
        sqlx::query("UPDATE sessions SET worker_id = ?, status = 'running', created_at = 0, updated_at = 0 WHERE id = 'session'")
            .bind(worker.to_string())
            .execute(st.store.pool())
            .await
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let live = st.registry.attach(worker, 1, tx, vec![], "worker").await;
        report(
            st,
            worker,
            live.conn_id,
            SessionLivenessReport {
                sessions: vec![SessionLiveness {
                    session_id: "session".into(),
                    actor_state: SessionActorState::WaitingForDisk as i32,
                    held_process: HeldProcessState::Exited as i32,
                    reason: "waiting for disk space".into(),
                    ..Default::default()
                }],
            },
        )
        .await;
        drop(sweep);
        sweep_at(st, now_ms() + 7 * 86400 * 1000).await.unwrap();
        assert!(!is_lost(st, "session").await.unwrap());
        assert_eq!(st.store.session("session").await.unwrap().unwrap().status, "running");
    }
    use c6_proto::SessionLiveness;
    use prost::Message;

    #[tokio::test]
    async fn unavailable_parent_with_automatic_group_hold_needs_you_and_receives_child_loss() {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let st = &head.state;
        let parent = WorkerId::new();
        let child = WorkerId::new();
        for (id, worker, owner, call) in [("parent", parent, None, None), ("child", child, Some("parent"), Some("call"))] {
            sqlx::query("INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, parent_id, parent_call_id) \
                         VALUES (?, ?, ?, '', '', '', 'idle', 0, 0, ?, ?)")
                .bind(id).bind(worker.to_string()).bind(id).bind(owner).bind(call).execute(st.store.pool()).await.unwrap();
        }
        let now = now_ms();
        let notice = c6_proto::SessionEvent {
            seq: 1,
            ts_ms: now,
            kind: Some(c6_proto::session_event::Kind::NoticeQueued(c6_proto::NoticeQueued {
                command_id: "first-child".into(),
                wake: true,
                reason: "group audit: 1 of 2 finished; waiting for 1".into(),
                ..Default::default()
            })),
        };
        st.store.ingest(parent, "parent", &[notice]).await.unwrap();
        let wait = format!("{}call", crate::remote::LOCAL_WAIT_PREFIX);
        st.store.add_remote_delegation(&wait, "child", "parent", "call").await.unwrap();
        let work = || async { crate::work::session_live(st, "home", "parent").await.unwrap().state };
        assert_eq!(work().await, "running");
        sweep_at(st, now).await.unwrap();
        let grace = crate::sessions::subagent_limits(st, Some("home")).await.unwrap().liveness_worker_grace_seconds;
        sweep_at(st, now + (i64::from(grace) + 1) * 1000).await.unwrap();
        assert!(is_lost(st, "parent").await.unwrap(), "the notice is its only pending work");
        let parent_row = st.store.session("parent").await.unwrap().unwrap();
        let mut display = crate::convert::session(&parent_row, None);
        crate::api::project_session_health(st, std::slice::from_mut(&mut display)).await.unwrap();
        assert!(display.notices_proceeding && display.needs_you());
        assert_eq!(work().await, "needs_you");
        let attention = crate::attention::get(st, &crate::realms::Caller::in_realm("home")).await.unwrap();
        assert!(attention.waiting.iter().any(|r| r.key == "notice:parent" && r.title == "Session lost: needs you"));
        assert!(!attention.worth.iter().any(|r| r.session_id.as_deref() == Some("parent")));
        let rows = st.store.pending_commands(parent, 0).await.unwrap();
        let report = rows.iter().find(|r| r.id.starts_with("liveness:lost:child:")).unwrap();
        let command = c6_proto::decode_stored_command(&report.data).unwrap();
        let Some(c6_proto::session_command::Kind::DelegationEnded(ended)) = command.kind else { panic!("expected terminal report") };
        assert!(ended.wake);
        assert_eq!(ended.call_id, "call");
        assert_eq!(ended.task_notice.unwrap().status, "lost");
        assert!(st.store.waiting_delegations("child").await.unwrap().is_empty());
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stale_connection_evidence_and_session_generation_do_not_commit_loss() {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let st = &head.state;
        // Isolate the staged-observation checks from the scheduled sweep.
        let sweep = st.liveness.sweep.lock().await;
        let pool = st.store.pool();
        let worker = WorkerId::new();
        for id in ["parent", "child:one"] {
            sqlx::query("INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, last_seq) VALUES (?, ?, '', '', '', '', 'running', 0, 0, 4)")
                .bind(id).bind(worker.to_string()).execute(pool).await.unwrap();
        }
        sqlx::query("INSERT INTO goals (session_id, text, state, max_turns, deadline, created_at, updated_at) VALUES ('child:one', '', 'active', 3, 0, 0, 0)")
            .execute(pool).await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let first = st.registry.attach(worker, 1, tx.clone(), vec![], "worker").await;
        let original = st.store.session("child:one").await.unwrap().unwrap();
        let stamp = st.registry.connection_stamp(worker);
        let health = |state| SessionLivenessReport {
            sessions: vec![SessionLiveness { session_id: original.id.clone(), actor_state: state as i32, ..Default::default() }],
        };
        let cause = "session actor stopped: test";
        report(st, worker, first.conn_id, health(SessionActorState::Stopped)).await;
        let stopped = st.liveness.evidence.lock().unwrap()[&original.id].clone();
        let stale = || LossFence { worker, stamp, evidence: Some(&stopped), progress: None };

        // Health can improve even before there is a new durable event.
        report(st, worker, first.conn_id, health(SessionActorState::Running)).await;
        record_loss(st, &original, stale(), cause, 10).await.unwrap();

        // Keep the stopped evidence unchanged, so only the connection fence
        // can reject the same observation after a replacement attaches.
        report(st, worker, first.conn_id, health(SessionActorState::Stopped)).await;
        let stopped = st.liveness.evidence.lock().unwrap()[&original.id].clone();
        let replacement = st.registry.attach(worker, 2, tx, vec![], "worker").await;
        let stale_connection = LossFence { worker, stamp, evidence: Some(&stopped), progress: None };
        record_loss(st, &original, stale_connection, cause, 11).await.unwrap();
        let stamp = st.registry.connection_stamp(worker);
        report(st, worker, replacement.conn_id, health(SessionActorState::Stopped)).await;
        let stopped = st.liveness.evidence.lock().unwrap()[&original.id].clone();
        let current = || LossFence { worker, stamp, evidence: Some(&stopped), progress: None };

        // The connection can remain identical while a newer journal row
        // invalidates the selected session snapshot.
        sqlx::query("UPDATE sessions SET last_seq = 5 WHERE id = 'child:one'").execute(pool).await.unwrap();
        record_loss(st, &original, current(), cause, 12).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM liveness_incidents").fetch_one(pool).await.unwrap();
        assert_eq!(count, 0);
        let row = st.store.session("child:one").await.unwrap().unwrap();
        assert_eq!(row.status, "running");
        let goal: String = sqlx::query_scalar("SELECT state FROM goals WHERE session_id = 'child:one'").fetch_one(pool).await.unwrap();
        assert_eq!(goal, "active");

        // Progress and reports can share a wall-clock millisecond. Only their
        // runtime order determines whether the newer evidence is usable.
        let at = stopped.received;
        let progress = Progress { at, generation: stopped.generation + 1 };
        assert!(!stopped.fresh_after(Some(progress)));
        let later = Evidence { generation: stopped.generation + 2, ..stopped.clone() };
        assert!(later.fresh_after(Some(progress)));

        // Exercise the durable gap before registration progress is published:
        // equal milliseconds must not retire a newly registered local waiter.
        sqlx::query("UPDATE sessions SET parent_id = 'parent' WHERE id = 'child:one'").execute(pool).await.unwrap();
        let input = format!("{}call", crate::remote::LOCAL_WAIT_PREFIX);
        st.store.add_remote_delegation(&input, &row.id, "parent", "call").await.unwrap();
        sqlx::query("UPDATE remote_delegations SET created_at = ? WHERE command_id = ?").bind(at).bind(&input).execute(pool).await.unwrap();
        record_loss(st, &row, current(), cause, 13).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM liveness_incidents").fetch_one(pool).await.unwrap();
        assert_eq!(count, 0);
        sqlx::query("UPDATE remote_delegations SET created_at = ? WHERE command_id = ?")
            .bind(at - 1)
            .bind(&input)
            .execute(pool)
            .await
            .unwrap();

        // A current observation can commit, but cannot retire a waiter if
        // owner or sequence changed while its delivery was being prepared.
        record_loss(st, &row, current(), cause, 14).await.unwrap();
        let incident: String = sqlx::query_scalar("SELECT id FROM liveness_incidents").fetch_one(pool).await.unwrap();
        let id = format!("liveness:lost:child:one:{incident}:call");
        let command = crate::store::NewCommand {
            id: &id,
            session_id: "parent",
            worker_id: worker,
            data: c6_proto::SessionCommand {
                kind: Some(c6_proto::session_command::Kind::DelegationEnded(c6_proto::DelegationEnded {
                    child_session_id: row.id.clone(),
                    ..Default::default()
                })),
                ..Default::default()
            }
            .encode_to_vec(),
            text: None,
        };
        sqlx::query("UPDATE sessions SET ownership_epoch = 1 WHERE id = 'child:one'").execute(pool).await.unwrap();
        st.store.enqueue_subagent_report(&command, &[input]).await.unwrap();
        assert_eq!(st.store.waiting_delegations(&row.id).await.unwrap().len(), 1);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM commands WHERE id = ?").bind(&id).fetch_one(pool).await.unwrap();
        assert_eq!(count, 0);
        drop(replacement);
        drop(first);
        drop(sweep);
        head.shutdown().await.unwrap();
    }
}
