//! Durable cross-worker placement. A single head sweeper advances these
//! receipts; workers key every side effect by move id and ownership epoch.
//!
//! The RPCs run over the registry. Release freezes the source only after
//! destination preflight; prepare may install a destination actor but it
//! must remain blocked. Only the head commits ownership. After that commit
//! the head will only activate the destination, never thaw the source.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use c6_core::{WorkerId, now_ms};
use c6_proto::{SessionEvent, session_event};
use futures::StreamExt as _;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
use uuid::Uuid;

use crate::{HeadState, conflict, store::Store};

const CHUNK: u64 = 256 * 1024;
const MAX_BYTES: u64 = 200_000_000;
const MAX_META: usize = 1024 * 1024;
const ATTACH_GRACE: Duration = Duration::from_secs(25);

#[derive(Debug, FromRow, Serialize)]
pub(crate) struct MoveStatus {
    pub id: String,
    pub session_id: String,
    pub source_worker_id: String,
    pub target_worker_id: String,
    pub source_epoch: i64,
    pub target_epoch: i64,
    pub state: String,
    #[serde(skip)]
    request_json: String,
    #[serde(skip)]
    result_json: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Progress {
    descriptor: Option<Value>,
    plan: Option<Value>,
    freeze_started: bool,
    last_seq: Option<i64>,
    manifest: Option<Value>,
    offset: u64,
    event: Option<String>,
    target_clean: bool,
    source_clean: bool,
}

pub(crate) async fn status(st: &HeadState, id: &str) -> anyhow::Result<Option<MoveStatus>> {
    Ok(sqlx::query_as("SELECT * FROM session_moves WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?)
}

/// Admit only at the exact placement the user saw. The event scan is outside
/// the write transaction; the admission predicate rechecks its watermark so
/// a concurrent placement or command cannot slip through.
pub(crate) async fn start(
    st: &HeadState,
    session_id: &str,
    source: WorkerId,
    target: WorkerId,
    expected_place_seq: u64,
    request: Value,
) -> anyhow::Result<String> {
    if source == target {
        return Err(crate::bad_request("choose another worker"));
    }
    let request = request.to_string();
    if request.len() > 64 * 1024 {
        return Err(crate::bad_request("move request is too large"));
    }
    let last_seq = st.store.last_seq(session_id).await?;
    if latest_place(&st.store, session_id, last_seq).await? != Some(expected_place_seq) {
        return Err(conflict("the session's place changed; refresh and try again"));
    }
    let id = Uuid::now_v7().to_string();
    let now = now_ms();
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let inserted = sqlx::query(
        "WITH previous AS (SELECT COALESCE(MAX(target_epoch), 0) AS attempted FROM session_moves WHERE session_id = ?5) \
         INSERT INTO session_moves (id, session_id, source_worker_id, target_worker_id, source_epoch, target_epoch, state, request_json, created_at, updated_at) \
         SELECT ?1, s.id, s.worker_id, ?2, s.ownership_epoch, MAX(s.ownership_epoch, previous.attempted) + 1, 'requested', ?3, ?4, ?4 \
         FROM sessions s CROSS JOIN previous \
         WHERE s.id = ?5 AND s.worker_id = ?6 AND s.last_seq = ?7 AND s.status = 'idle' AND s.archived = 0 AND s.observed = 0 \
           AND s.parent_id IS NULL AND s.root_id IS NULL AND s.parent_call_id IS NULL AND s.remote_template = '' AND s.ssh_target = '' \
           AND MAX(s.ownership_epoch, previous.attempted) < 9223372036854775807 \
           AND NOT EXISTS (SELECT 1 FROM commands WHERE session_id = s.id AND state IN ('pending', 'held')) \
           AND NOT EXISTS (SELECT 1 FROM session_moves WHERE session_id = s.id AND state NOT IN ('done', 'failed'))",
    )
    .bind(&id)
    .bind(target.to_string())
    .bind(request)
    .bind(now)
    .bind(session_id)
    .bind(source.to_string())
    .bind(i64::try_from(last_seq)?)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(conflict("the session must still be idle and unarchived, with no pending commands or move; refresh and try again"));
    }
    crate::agent_ops::link_placement_command(&mut tx, session_id, &id).await?;
    tx.commit().await?;
    st.registry.bump();
    st.attention.changed();
    Ok(id)
}

async fn latest_place(store: &Store, id: &str, at: u64) -> anyhow::Result<Option<u64>> {
    let mut through = i64::try_from(at)?;
    while through > 0 {
        let page = sqlx::query_scalar::<_, Vec<u8>>("SELECT data FROM events WHERE session_id = ? AND seq <= ? ORDER BY seq DESC LIMIT 32")
            .bind(id)
            .bind(through)
            .fetch_all(&store.pool)
            .await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let ev = SessionEvent::decode(row.as_slice())?;
            if matches!(ev.kind, Some(session_event::Kind::Started(_) | session_event::Kind::PlaceChanged(_))) {
                return Ok(Some(ev.seq));
            }
            through = i64::try_from(ev.seq)? - 1;
        }
    }
    Ok(None)
}

/// Use the same check on the live policy before dispatching data and again
/// under the immediate transaction that changes ownership. Abort/retire do
/// not carry session data and must remain possible after a policy change.
async fn check_worker_realms(db: &mut sqlx::SqliteConnection, row: &MoveStatus) -> anyhow::Result<()> {
    let (realm, source_realms, target_realms): (String, String, String) = sqlx::query_as(
        "SELECT s.realm, source.realms, target.realms FROM sessions s \
         JOIN workers source ON source.id = ?2 AND source.revoked_at IS NULL AND source.role NOT IN ('head', 'memex') \
         JOIN workers target ON target.id = ?3 AND target.revoked_at IS NULL AND target.role NOT IN ('head', 'memex') WHERE s.id = ?1",
    )
    .bind(&row.session_id)
    .bind(&row.source_worker_id)
    .bind(&row.target_worker_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| refuse("the session or one of its workers is no longer available"))?;
    for (side, json) in [("source", source_realms), ("destination", target_realms)] {
        let available = serde_json::from_str::<Vec<String>>(&json).is_ok_and(|realms| crate::realms::serves(&realms, &realm));
        if !available {
            return Err(refuse(format!("the {side} worker is no longer available to this session's realm")));
        }
    }
    Ok(())
}

pub(crate) async fn run_sweeper(st: Arc<HeadState>) -> anyhow::Result<()> {
    let boot = Boot { wall_ms: now_ms(), since: Instant::now() };
    // Target abort/source export retirement can wait for a worker that
    // may never return. Never spend active move slots on this cleanup.
    tokio::join!(run_lane(&st, boot, false), run_lane(&st, boot, true));
    Ok(())
}

#[derive(Clone, Copy)]
struct Boot {
    wall_ms: i64,
    since: Instant,
}

impl Boot {
    fn may_wait_for_attach(self, created_at: i64, seen_worker: bool) -> bool {
        created_at < self.wall_ms && !seen_worker && self.since.elapsed() < ATTACH_GRACE
    }
}

async fn run_lane(st: &HeadState, boot: Boot, cleanup: bool) {
    let mut seen_workers = HashSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(if cleanup { 2_000 } else { 500 }));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = st.shutdown.cancelled() => return,
            result = async {
                tick.tick().await;
                sweep(st, boot, &mut seen_workers, cleanup).await
            } => if let Err(error) = result {
                tracing::warn!(%error, "scanning session moves");
            },
        }
    }
}

async fn sweep(st: &HeadState, boot: Boot, seen_workers: &mut HashSet<WorkerId>, cleanup: bool) -> anyhow::Result<()> {
    let query = if cleanup {
        "SELECT * FROM session_moves WHERE state IN ('done', 'failed') AND \
         (NOT COALESCE(json_extract(result_json, '$.source_clean'), 0) OR \
          (state = 'failed' AND NOT COALESCE(json_extract(result_json, '$.target_clean'), 0))) ORDER BY updated_at"
    } else {
        "SELECT * FROM session_moves WHERE state NOT IN ('done', 'failed') ORDER BY updated_at"
    };
    let moves: Vec<MoveStatus> = sqlx::query_as(query).fetch_all(&st.store.pool).await?;
    if !cleanup {
        for worker in moves.iter().flat_map(|job| [&job.source_worker_id, &job.target_worker_id]).filter_map(|id| id.parse().ok()) {
            if st.registry.get(worker).is_some() {
                seen_workers.insert(worker);
            }
        }
    }
    let seen = &*seen_workers;
    futures::stream::iter(moves)
        .for_each_concurrent(if cleanup { 4 } else { 8 }, |job| async move {
            if !st.shutdown.is_cancelled()
                && let Err(error) = advance(st, &job, boot, seen).await
            {
                tracing::warn!(move_id = %job.id, state = %job.state, %error, "advancing session move");
            }
        })
        .await;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Refused(String);

fn refuse(reason: impl Into<String>) -> anyhow::Error {
    Refused(reason.into()).into()
}

struct Move<'a> {
    st: &'a HeadState,
    row: &'a MoveStatus,
    source: WorkerId,
    target: WorkerId,
    request: Value,
    progress: Progress,
}

async fn advance(st: &HeadState, row: &MoveStatus, boot: Boot, seen_workers: &HashSet<WorkerId>) -> anyhow::Result<()> {
    let mut job = Move {
        st,
        row,
        source: row.source_worker_id.parse()?,
        target: row.target_worker_id.parse()?,
        request: serde_json::from_str(&row.request_json)?,
        progress: serde_json::from_str(&row.result_json)?,
    };
    match row.state.as_str() {
        "rolling_back" => job.rollback().await,
        "done" | "failed" => job.cleanup().await,
        "adopted" => job.activate().await,
        "requested" | "released" | "transferred" => {
            if st.shutdown.is_cancelled() {
                return Ok(());
            }
            let result = job.before_adoption(boot, seen_workers).await;
            if let Err(error) = &result
                && error.is::<Refused>()
            {
                return job.save("rolling_back", Some(&crate::util_clip(&error.to_string(), 1000))).await;
            }
            result
        }
        state => anyhow::bail!("unknown move state {state:?}"),
    }
}

impl Move<'_> {
    async fn save(&self, state: &str, error: Option<&str>) -> anyhow::Result<()> {
        let result =
            sqlx::query("UPDATE session_moves SET state = ?, result_json = ?, error = ?, updated_at = ? WHERE id = ? AND state = ?")
                .bind(state)
                .bind(serde_json::to_string(&self.progress)?)
                .bind(error)
                .bind(now_ms())
                .bind(&self.row.id)
                .bind(&self.row.state)
                .execute(&self.st.store.pool)
                .await?;
        ensure!(result.rows_affected() == 1, "move progress changed under the sweeper");
        Ok(())
    }

    async fn rpc(&self, worker: WorkerId, method: &str, extra: Value) -> anyhow::Result<Value> {
        let mut args = json!({
            "move_id": self.row.id, "session_id": self.row.session_id,
            "source_worker_id": self.row.source_worker_id, "target_worker_id": self.row.target_worker_id,
            "source_epoch": self.row.source_epoch, "target_epoch": self.row.target_epoch,
        });
        args.as_object_mut().expect("object").extend(extra.as_object().expect("object").clone());
        if matches!(method, "move.inspect" | "move.preflight" | "move.freeze" | "move.export" | "move.read" | "move.write" | "move.prepare")
        {
            args["request"] = self.request.clone();
            let mut conn = self.st.store.pool.acquire().await?;
            check_worker_realms(&mut conn, self.row).await?;
        }
        let timeout = if matches!(method, "move.read" | "move.write" | "move.inspect" | "move.retire") { 15 } else { 120 };
        match self.st.registry.call(worker, method, args, Duration::from_secs(timeout)).await {
            Ok(value) => Ok(value),
            // An unanswered call may have executed. Reissue the exact call,
            // not a later stage, if the connection is still up.
            Err(msg) if msg == "worker did not respond in time" => Err(anyhow!("{method}: {msg}")),
            Err(msg) if self.st.shutdown.is_cancelled() => Err(anyhow!("{method}: {msg}")),
            Err(msg) => Err(refuse(format!("{method}: {msg}"))),
        }
    }

    async fn before_adoption(&mut self, boot: Boot, seen_workers: &HashSet<WorkerId>) -> anyhow::Result<()> {
        let mut waiting_for_attach = false;
        for (worker, side) in [(self.source, "source"), (self.target, "destination")] {
            if self.st.registry.get(worker).is_none() {
                // The registry starts empty on head restart. Don't turn
                // recovery into rollback just because workers haven't dialed yet.
                if boot.may_wait_for_attach(self.row.created_at, seen_workers.contains(&worker)) {
                    waiting_for_attach = true;
                } else {
                    return Err(refuse(format!("the {side} worker disconnected before the move was adopted")));
                }
            }
        }
        if waiting_for_attach {
            return Ok(());
        }
        match self.row.state.as_str() {
            "requested" => self.release().await,
            "released" => self.transfer().await,
            "transferred" => self.prepare_and_adopt().await,
            _ => unreachable!(),
        }
    }

    async fn release(&mut self) -> anyhow::Result<()> {
        if self.progress.descriptor.is_none() {
            let descriptor = self.rpc(self.source, "move.inspect", json!({})).await?;
            self.progress.descriptor = Some(bounded_metadata(descriptor)?);
            return self.save("requested", None).await;
        }
        if self.progress.plan.is_none() {
            let plan = self.rpc(self.target, "move.preflight", json!({"descriptor": self.progress.descriptor})).await?;
            self.progress.plan = Some(bounded_metadata(plan)?);
            return self.save("requested", None).await;
        }
        if !self.progress.freeze_started {
            // A crash after freeze is sent but before its reply is saved
            // must still make rollback send an exact source abort.
            self.progress.freeze_started = true;
            return self.save("requested", None).await;
        }
        let released = self.rpc(self.source, "move.freeze", json!({"descriptor": self.progress.descriptor})).await?;
        let last_seq = released
            .get("last_seq")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0 && *n < i64::MAX)
            .ok_or_else(|| refuse("source returned an invalid freeze watermark"))?;
        self.progress.last_seq = Some(last_seq);
        self.save("released", None).await
    }

    async fn transfer(&mut self) -> anyhow::Result<()> {
        let frozen = self.progress.last_seq.context("released move has no freeze watermark")?;
        let head_seq: i64 = sqlx::query_scalar("SELECT last_seq FROM sessions WHERE id = ? AND worker_id = ? AND ownership_epoch = ?")
            .bind(&self.row.session_id)
            .bind(&self.row.source_worker_id)
            .bind(self.row.source_epoch)
            .fetch_optional(&self.st.store.pool)
            .await?
            .ok_or_else(|| refuse("source no longer owns the session"))?;
        if head_seq < frozen {
            // The ordered upload lane is independent of the RPC lane. Never
            // infer durability from the freeze reply or a fixed delay.
            return Ok(());
        }
        if head_seq != frozen {
            return Err(refuse("source appended events after freezing"));
        }
        if self.progress.manifest.is_none() {
            let manifest =
                self.rpc(self.source, "move.export", json!({"last_seq": frozen, "descriptor": self.progress.descriptor})).await?;
            let manifest = bounded_metadata(manifest)?;
            manifest_bytes(&manifest)?;
            self.progress.manifest = Some(manifest);
            return self.save("released", None).await;
        }
        let size = manifest_bytes(self.progress.manifest.as_ref().unwrap())?;
        ensure!(self.progress.offset <= size, "move cursor is beyond the manifest");
        let dir = self.staging()?;
        c6_core::create_private_dir(dir.parent().context("move staging path has no parent")?)?;
        c6_core::create_private_dir(&dir)?;
        // Bounded work per sweep; any crash before save simply replays the
        // chunk at the same offset (the destination checks overlap bytes).
        for _ in 0..8 {
            if self.progress.offset == size {
                return self.save("transferred", None).await;
            }
            if self.st.shutdown.is_cancelled() {
                return Ok(());
            }
            let offset = self.progress.offset;
            let length = CHUNK.min(size - offset);
            let output = self.rpc(self.source, "move.read", json!({"offset": offset, "length": length})).await?;
            let data = output.get("data").and_then(Value::as_str).ok_or_else(|| refuse("source omitted the transfer data"))?;
            let bytes = decode_bounded(data, length as usize)?;
            if bytes.len() as u64 != length {
                return Err(refuse("source sent a short transfer chunk"));
            }
            let mut options = tokio::fs::OpenOptions::new();
            options.create(true).truncate(false).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(dir.join("bundle")).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            file.write_all(&bytes).await?;
            file.sync_data().await?;
            self.rpc(
                self.target,
                "move.write",
                json!({
                    "offset": offset, "data": data, "manifest": self.progress.manifest, "plan": self.progress.plan,
                }),
            )
            .await?;
            self.progress.offset += length;
            self.save("released", None).await?;
        }
        Ok(())
    }

    async fn prepare_and_adopt(&mut self) -> anyhow::Result<()> {
        let frozen = self.progress.last_seq.context("transferred move has no freeze watermark")?;
        if self.progress.event.is_none() {
            let prepared = self.rpc(self.target, "move.prepare", json!({
                "last_seq": frozen, "manifest": self.progress.manifest, "plan": self.progress.plan, "descriptor": self.progress.descriptor,
            })).await?;
            let data =
                prepared.get("event").and_then(Value::as_str).ok_or_else(|| refuse("destination did not return its placement event"))?;
            self.check_event(data, frozen)?;
            self.progress.event = Some(data.to_owned());
            return self.save("transferred", None).await;
        }
        let ev = self.check_event(self.progress.event.as_deref().unwrap(), frozen)?;
        let now = now_ms();
        let mut tx = self.st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
        check_worker_realms(&mut tx, self.row).await?;
        let owner = sqlx::query(
            "UPDATE sessions SET worker_id = ?1, ownership_epoch = ?2, \
             worker_name = COALESCE((SELECT name FROM workers WHERE id = ?1), '') \
             WHERE id = ?3 AND worker_id = ?4 AND ownership_epoch = ?5 AND last_seq = ?6 AND status = 'idle' AND archived = 0 \
             AND EXISTS (SELECT 1 FROM workers WHERE id = ?1 AND revoked_at IS NULL)",
        )
        .bind(&self.row.target_worker_id)
        .bind(self.row.target_epoch)
        .bind(&self.row.session_id)
        .bind(&self.row.source_worker_id)
        .bind(self.row.source_epoch)
        .bind(frozen)
        .execute(&mut *tx)
        .await?;
        if owner.rows_affected() != 1 {
            return Err(refuse("the source session or destination changed before adoption"));
        }
        Store::append_move_place_in(&mut tx, &self.row.session_id, self.target, self.row.target_epoch, frozen, &ev).await?;
        let moved = sqlx::query("UPDATE session_moves SET state = 'adopted', updated_at = ? WHERE id = ? AND state = 'transferred'")
            .bind(now)
            .bind(&self.row.id)
            .execute(&mut *tx)
            .await?;
        ensure!(moved.rows_affected() == 1, "move adoption lost its state fence");
        tx.commit().await?;
        if let Some(event) = crate::convert::event(&ev) {
            self.st.hub.publish(&self.row.session_id, c6_api::StreamMsg::Event { event });
        }
        self.st.registry.bump();
        Ok(())
    }

    fn check_event(&self, data: &str, frozen: i64) -> anyhow::Result<SessionEvent> {
        let bytes = decode_bounded(data, MAX_META)?;
        let ev = SessionEvent::decode(bytes.as_slice()).map_err(|_| refuse("destination sent an invalid placement event"))?;
        let Some(session_event::Kind::PlaceChanged(place)) = &ev.kind else {
            return Err(refuse("destination sent a non-placement event"));
        };
        if ev.seq != (frozen + 1) as u64
            || ev.ts_ms <= 0
            || place.command_id != self.row.id
            || place.ownership_epoch != self.row.target_epoch as u64
            || !place.from.as_ref().is_some_and(|p| p.worker_id == self.row.source_worker_id)
            || !place.to.as_ref().is_some_and(|p| p.worker_id == self.row.target_worker_id && !p.cwd.is_empty())
        {
            return Err(refuse("destination returned a placement event for a different move"));
        }
        Ok(ev)
    }

    async fn rollback(&mut self) -> anyhow::Result<()> {
        if self.progress.freeze_started {
            if self.st.registry.get(self.source).is_none() {
                return Ok(());
            }
            // Remain gated until the source confirms abort on attach. A
            // timeout does not permit sending commands to a frozen actor.
            self.rpc(self.source, "move.abort", json!({"side": "source"})).await?;
        }
        self.remove_staging().await?;
        let reason = self.row.error.as_deref().unwrap_or("the move did not reach adoption");
        let mut tx = self.st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
        let failed = sqlx::query("UPDATE session_moves SET state = 'failed', updated_at = ? WHERE id = ? AND state = 'rolling_back'")
            .bind(now_ms())
            .bind(&self.row.id)
            .execute(&mut *tx)
            .await?;
        ensure!(failed.rows_affected() == 1, "move rollback lost its state fence");
        crate::agent_ops::finish_placement_command(&mut tx, &self.row.session_id, &self.row.id, Err(reason)).await?;
        tx.commit().await?;
        self.refresh_notice_limits(self.source).await;
        self.st.registry.deliver(self.source);
        self.st.registry.bump();
        self.st.attention.changed();
        Ok(())
    }

    async fn cleanup(&mut self) -> anyhow::Result<()> {
        let retire = !self.progress.source_clean && self.st.registry.get(self.source).is_some();
        let abort = self.row.state == "failed" && !self.progress.target_clean && self.st.registry.get(self.target).is_some();
        let (retired, aborted) = tokio::join!(
            async { if retire { self.rpc(self.source, "move.retire", json!({"role": "source"})).await.map(|_| ()) } else { Ok(()) } },
            async { if abort { self.rpc(self.target, "move.abort", json!({"side": "target"})).await.map(|_| ()) } else { Ok(()) } },
        );
        let changed = (retire && retired.is_ok()) || (abort && aborted.is_ok());
        if changed {
            self.progress.source_clean |= retire && retired.is_ok();
            self.progress.target_clean |= abort && aborted.is_ok();
            self.save(&self.row.state, self.row.error.as_deref()).await?;
        }
        retired.and(aborted)
    }

    async fn activate(&self) -> anyhow::Result<()> {
        if self.st.registry.get(self.target).is_none() {
            return Ok(());
        }
        // After the CAS every error (including explicit refusal) is retryable;
        // the database owner is never switched back.
        self.rpc(self.target, "move.activate", json!({})).await?;
        self.remove_staging().await?;
        self.save("done", None).await?;
        self.refresh_notice_limits(self.target).await;
        self.st.registry.deliver(self.target);
        self.st.registry.bump();
        Ok(())
    }

    async fn refresh_notice_limits(&self, worker: WorkerId) {
        // Settings edits skip gated sessions. Refresh once the owner is routable again.
        if let Err(error) = crate::sessions::refresh_notice_limits(self.st, Some(worker)).await {
            tracing::warn!(move_id = %self.row.id, %worker, "refreshing session settings after move: {error:#}");
        }
    }

    fn staging(&self) -> anyhow::Result<PathBuf> {
        let id: Uuid = self.row.id.parse()?;
        Ok(self.st.data_dir.join("session-moves").join(id.to_string()))
    }

    async fn remove_staging(&self) -> anyhow::Result<()> {
        match tokio::fs::remove_dir_all(self.staging()?).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn bounded_metadata(value: Value) -> anyhow::Result<Value> {
    if !value.is_object() || value.to_string().len() > MAX_META {
        return Err(refuse("invalid or oversized move metadata"));
    }
    Ok(value)
}

fn manifest_bytes(manifest: &Value) -> anyhow::Result<u64> {
    manifest
        .get("bytes")
        .and_then(Value::as_u64)
        .filter(|n| (1..=MAX_BYTES).contains(n))
        .ok_or_else(|| refuse("the move archive exceeds 200 MB or has an invalid size"))
}

fn decode_bounded(data: &str, limit: usize) -> anyhow::Result<Vec<u8>> {
    if data.len() > limit.div_ceil(3) * 4 {
        return Err(refuse("oversized base64 move payload"));
    }
    let bytes = STANDARD.decode(data).map_err(|_| refuse("invalid base64 move payload"))?;
    if bytes.len() > limit {
        return Err(refuse("oversized move payload"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unbounded_archives_and_chunks() {
        assert_eq!(manifest_bytes(&json!({"bytes": MAX_BYTES})).unwrap(), MAX_BYTES);
        for invalid in [json!({}), json!({"bytes": -1}), json!({"bytes": MAX_BYTES + 1})] {
            assert!(manifest_bytes(&invalid).unwrap_err().is::<Refused>());
        }

        let raw = vec![123; CHUNK as usize];
        assert_eq!(decode_bounded(&STANDARD.encode(&raw), CHUNK as usize).unwrap(), raw);
        assert!(decode_bounded(&STANDARD.encode([0, 1, 2]), 2).unwrap_err().is::<Refused>());
        assert!(decode_bounded("!!!!", 8).unwrap_err().is::<Refused>());
    }

    #[test]
    fn restart_grace_is_bounded_and_only_for_unseen_workers_on_existing_moves() {
        let fresh = Boot { wall_ms: 100, since: Instant::now() };
        assert!(fresh.may_wait_for_attach(99, false));
        assert!(!fresh.may_wait_for_attach(99, true));
        assert!(!fresh.may_wait_for_attach(101, false));
        let expired = Boot { since: fresh.since - ATTACH_GRACE, ..fresh };
        assert!(!expired.may_wait_for_attach(99, false));
    }

    #[tokio::test]
    async fn admission_rejects_stale_places_and_serializes_with_commands() {
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
        let pool = &st.store.pool;
        let source = WorkerId::new();
        let target = WorkerId::new();
        for worker in [source, target] {
            sqlx::query(
                "INSERT INTO workers (id, name, hostname, os, arch, cert_serial, enrolled_at) \
                 VALUES (?, 'worker', '', '', '', '', 1)",
            )
            .bind(worker.to_string())
            .execute(pool)
            .await
            .unwrap();
        }
        let (src_tx, _src_rx) = tokio::sync::mpsc::channel(8);
        let (dst_tx, _dst_rx) = tokio::sync::mpsc::channel(8);
        let capabilities = vec![c6_proto::SESSION_MOVE_CAPABILITY.to_owned()];
        let source_conn = st.registry.attach(source, now_ms(), src_tx, capabilities.clone(), "worker").await;
        let target_conn = st.registry.attach(target, now_ms(), dst_tx, capabilities, "worker").await;
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, last_seq) \
             VALUES ('session', ?, 'move', '/source', '', '', 'idle', 1, 1, 1)",
        )
        .bind(source.to_string())
        .execute(pool)
        .await
        .unwrap();
        let started = SessionEvent { seq: 1, ts_ms: 1, kind: Some(session_event::Kind::Started(Default::default())) };
        sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES ('session', 1, 1, ?)")
            .bind(started.encode_to_vec())
            .execute(pool)
            .await
            .unwrap();
        let command = || {
            sqlx::query("INSERT INTO commands (id, session_id, worker_id, data, created_at) VALUES ('pending', 'session', ?, X'', 1)")
                .bind(source.to_string())
                .execute(pool)
        };
        assert!(start(st, "session", source, target, 4, json!({})).await.is_err());
        command().await.unwrap();
        assert!(start(st, "session", source, target, 1, json!({})).await.is_err());
        sqlx::query("DELETE FROM commands WHERE id = 'pending'").execute(pool).await.unwrap();

        let (left, right) =
            tokio::join!(start(st, "session", source, target, 1, json!({})), start(st, "session", source, target, 1, json!({})));
        let receipt = match (left, right) {
            (Ok(id), Err(_)) | (Err(_), Ok(id)) => status(st, &id).await.unwrap().unwrap(),
            result => panic!("expected exactly one move, got {result:?}"),
        };
        assert_eq!(receipt.state, "requested");
        assert_eq!((receipt.source_epoch, receipt.target_epoch), (0, 1));
        assert!(command().await.unwrap_err().to_string().contains("session is moving"));

        // Attempt 1 may already have left an immutable generation on the
        // destination. Failure must not make that generation reusable.
        sqlx::query("UPDATE session_moves SET state = 'failed' WHERE id = ?").bind(&receipt.id).execute(pool).await.unwrap();
        let retry = start(st, "session", source, target, 1, json!({})).await.unwrap();
        let retry = status(st, &retry).await.unwrap().unwrap();
        assert_eq!((retry.source_epoch, retry.target_epoch), (0, 2));

        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        check_worker_realms(&mut tx, &retry).await.unwrap();
        for worker in [source, target] {
            sqlx::query("UPDATE workers SET realms = '[\"other\"]' WHERE id = ?").bind(worker.to_string()).execute(&mut *tx).await.unwrap();
            assert!(check_worker_realms(&mut tx, &retry).await.unwrap_err().is::<Refused>());
            sqlx::query("UPDATE workers SET realms = '[]' WHERE id = ?").bind(worker.to_string()).execute(&mut *tx).await.unwrap();
        }
        tx.rollback().await.unwrap();

        drop(source_conn);
        drop(target_conn);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restarted_head_activates_the_adopted_generation_without_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        };
        let head = crate::start(config.clone()).await.unwrap();
        let source = WorkerId::new();
        let target = WorkerId::new();
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, ownership_epoch, title, cwd, connection_id, model, status, created_at, updated_at, last_seq) \
             VALUES ('session', ?, 3, 'move', '/target', '', '', 'idle', 1, 1, 2)",
        )
        .bind(target.to_string())
        .execute(&head.state.store.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO session_moves (id, session_id, source_worker_id, target_worker_id, source_epoch, target_epoch, state, request_json, created_at, updated_at) \
             VALUES (?, 'session', ?, ?, 1, 3, 'adopted', '{}', 1, 1)",
        )
        .bind(&id)
        .bind(source.to_string())
        .bind(target.to_string())
        .execute(&head.state.store.pool)
        .await
        .unwrap();
        let placed = SessionEvent {
            seq: 2,
            ts_ms: 1,
            kind: Some(session_event::Kind::PlaceChanged(c6_proto::PlaceChanged { command_id: id.clone(), ..Default::default() })),
        };
        sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES ('session', 2, 1, ?)")
            .bind(placed.encode_to_vec())
            .execute(&head.state.store.pool)
            .await
            .unwrap();
        head.shutdown().await.unwrap();

        let head = crate::start(config).await.unwrap();
        let st = &head.state;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let capabilities = vec![c6_proto::SESSION_MOVE_CAPABILITY.to_owned(), "notice_settings_v1".to_owned()];
        let live = st.registry.attach(target, now_ms(), tx, capabilities, "worker").await;
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap().unwrap();
        let Some(c6_proto::head_msg::Msg::RpcRequest(rpc)) = message.msg else { panic!("expected activation request") };
        assert_eq!(rpc.method, "move.activate");
        let params: Value = serde_json::from_slice(&rpc.params_json).unwrap();
        assert_eq!(params["move_id"], id);
        assert_eq!(params["target_epoch"], 3);
        let configured = c6_api::SubagentLimits { notice_quiet_seconds: 7, max_running: 12, ..Default::default() };
        st.store.set_setting(crate::sessions::SUBAGENT_LIMITS, &configured.to_stored_json().unwrap()).await.unwrap();
        crate::sessions::refresh_notice_limits(st, None).await.unwrap();
        assert!(matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)), "settings must be gated during activation");

        live.rpc_response(c6_proto::RpcResponse { id: rpc.id, result_json: b"{}".to_vec(), error: String::new() });
        tokio::time::timeout(Duration::from_secs(5), live.deliver.notified()).await.unwrap();
        let message = rx.try_recv().expect("settings must be sent before waking queue delivery").unwrap();
        let Some(c6_proto::head_msg::Msg::Command(command)) = message.msg else { panic!("expected settings command") };
        assert_eq!(command.session_id, "session");
        assert_eq!(command.ownership_epoch, 3);
        let Some(c6_proto::session_command::Kind::UpdateNoticeLimits(limits)) = command.kind else { panic!("expected notice limits") };
        assert_eq!(limits.notice_quiet_seconds, configured.notice_quiet_seconds);
        assert_eq!(limits.max_running_subagents, Some(configured.max_running));
        assert_eq!(status(st, &id).await.unwrap().unwrap().state, "done");
        let (owner, epoch, seq): (String, i64, i64) =
            sqlx::query_as("SELECT worker_id, ownership_epoch, last_seq FROM sessions WHERE id = 'session'")
                .fetch_one(&st.store.pool)
                .await
                .unwrap();
        assert_eq!((owner, epoch, seq), (target.to_string(), 3, 2));
        assert_eq!(st.store.events_after("session", 0, 10).await.unwrap(), vec![placed]);

        let (source_tx, mut source_rx) = tokio::sync::mpsc::channel(8);
        let source_live =
            st.registry.attach(source, now_ms(), source_tx, vec![c6_proto::SESSION_MOVE_CAPABILITY.to_owned()], "worker").await;
        let message = tokio::time::timeout(Duration::from_secs(6), source_rx.recv()).await.unwrap().unwrap().unwrap();
        let Some(c6_proto::head_msg::Msg::RpcRequest(rpc)) = message.msg else { panic!("expected source retirement request") };
        assert_eq!(rpc.method, "move.retire");
        let params: Value = serde_json::from_slice(&rpc.params_json).unwrap();
        assert_eq!(params["move_id"], id);
        assert_eq!(params["role"], "source");
        source_live.rpc_response(c6_proto::RpcResponse { id: rpc.id, result_json: b"{}".to_vec(), error: String::new() });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let receipt = status(st, &id).await.unwrap().unwrap();
                let progress: Progress = serde_json::from_str(&receipt.result_json).unwrap();
                if progress.source_clean {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(source_live);
        drop(live);
        head.shutdown().await.unwrap();
    }
}
