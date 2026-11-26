//! Local durable journal of session events (SQLite, WAL).
//!
//! Every durable event is committed here before it is sent to the head, so a
//! disconnect or crash never loses one. It is a cache: the head's copy is
//! authoritative once acknowledged.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use c6_proto::{CreateSession, SessionEvent, session_event};
use futures::TryStreamExt;
use prost::Message;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, Row, Sqlite, Transaction};
use tokio::sync::Notify;

/// A known live writer or missing stop/identity confirmation, not a storage
/// or decoding failure. Callers may refuse an operation without stopping its
/// actor only for this classified ownership barrier.
#[derive(Debug)]
pub(crate) struct StopUnconfirmed(pub(crate) String);

impl std::fmt::Display for StopUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StopUnconfirmed {}

pub struct Journal {
    pool: SqlitePool,
    /// Cleanup owns the write side; session setup/turns and shell launch
    /// reservations own the read side. Recheck process barriers under write.
    pub(crate) build_folder_lifecycle: Arc<tokio::sync::RwLock<()>>,
    #[cfg(test)]
    recovery_error: std::sync::Mutex<Option<(i32, Arc<std::sync::atomic::AtomicBool>)>>,
    /// Serialize reset publication/retry authorization with exceptional
    /// metadata-loss cleanup. Acquire before any SQLite transaction; cleanup
    /// holds no database transaction while awaiting process-group absence.
    /// The worker's manager and actors share this Journal through one Arc.
    pub(crate) claude_rewind_ownership: tokio::sync::Mutex<()>,
    /// Signalled after every append. Single consumer (the uploader), so
    /// `notify_one` keeps a permit when it is busy and nothing is missed.
    pub appended: Notify,
}

/// The last request after account-specific replay preparation. Contains no
/// credential grant: pings must obtain a fresh grant for this exact target.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct KeepWarmSnapshot {
    pub request: c6_llm::ModelRequest,
    /// Complete provider output, kept separately so the cached input remains
    /// byte-for-byte reproducible. Older/incomplete snapshots cannot be pinged.
    #[serde(default)]
    pub response: Option<c6_llm::Message>,
    pub connection_id: String,
    pub kind: String,
    pub base_url: String,
    pub last_call_ms: i64,
    pub usage: c6_llm::Usage,
}

struct ShellOwner {
    journal: Arc<Journal>,
    session_id: String,
    root_session_id: String,
}

#[derive(Default)]
struct BuildFolderPlace {
    checkpoint_seq: i64,
    known: bool,
    backfill_blocked: bool,
    cwd: String,
    repository: String,
    worker_id: String,
    local: bool,
}

impl BuildFolderPlace {
    fn from_spec(spec: &CreateSession) -> Self {
        Self {
            known: !spec.cwd.is_empty() || !spec.ssh_target.is_empty(),
            cwd: spec.cwd.clone(),
            local: spec.ssh_target.is_empty(),
            ..Self::default()
        }
    }

    fn apply(&mut self, event: &SessionEvent) {
        match &event.kind {
            Some(session_event::Kind::Started(started)) => {
                self.cwd.clone_from(&started.cwd);
                self.repository.clone_from(&started.repo_root);
                self.worker_id.clear();
                self.local = started.ssh_target.is_empty();
                self.known = !self.local || !self.cwd.is_empty();
            }
            Some(session_event::Kind::PlaceChanged(place)) => {
                if let Some(to) = &place.to {
                    self.cwd.clone_from(&to.cwd);
                    self.repository.clone_from(&to.repo_root);
                    self.worker_id.clone_from(&to.worker_id);
                    self.local = true;
                    self.known = !self.cwd.is_empty();
                } else {
                    self.known = false;
                }
            }
            _ => {}
        }
        self.checkpoint_seq = event.seq as i64;
    }
}

async fn build_folder_place_in(tx: &mut sqlx::SqliteConnection, session_id: &str) -> anyhow::Result<Option<BuildFolderPlace>> {
    Ok(sqlx::query(
        "SELECT checkpoint_seq, known, backfill_blocked, cwd, repository, worker_id, local FROM build_folder_places WHERE session_id = ?",
    )
    .bind(session_id)
    .fetch_optional(tx)
    .await?
    .map(|row| BuildFolderPlace {
        checkpoint_seq: row.get("checkpoint_seq"),
        known: row.get("known"),
        backfill_blocked: row.get("backfill_blocked"),
        cwd: row.get("cwd"),
        repository: row.get("repository"),
        worker_id: row.get("worker_id"),
        local: row.get("local"),
    }))
}

async fn save_build_folder_place_in(tx: &mut sqlx::SqliteConnection, session_id: &str, place: &BuildFolderPlace) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO build_folder_places (session_id, checkpoint_seq, known, backfill_blocked, cwd, repository, worker_id, local) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET checkpoint_seq = excluded.checkpoint_seq, known = excluded.known, backfill_blocked = excluded.backfill_blocked, cwd = excluded.cwd, repository = excluded.repository, worker_id = excluded.worker_id, local = excluded.local")
        .bind(session_id).bind(place.checkpoint_seq).bind(place.known).bind(place.backfill_blocked).bind(&place.cwd).bind(&place.repository)
        .bind(&place.worker_id).bind(place.local).execute(&mut *tx).await?;
    // Retain unarchived references too: a session can be trimmed before the
    // first head archive refresh and still share an archived session's output.
    sqlx::query(
        "INSERT OR IGNORE INTO build_folder_archives (session_id, last_used_ms) SELECT ?, last_activity FROM sessions WHERE id = ?",
    )
    .bind(session_id)
    .bind(session_id)
    .execute(tx)
    .await?;
    Ok(())
}

#[async_trait::async_trait]
impl c6_tools::ShellOwner for ShellOwner {
    async fn reserve(&self, task_id: &str) -> std::io::Result<String> {
        let _build_use = self.journal.build_folder_lifecycle.read().await;
        let id = uuid::Uuid::now_v7().to_string();
        // One write statement serializes admission with rewind preparation
        // and freeze. A launch either owns a durable unknown barrier first,
        // or cannot create a child at all (including descendants).
        let inserted = sqlx::query(
            "INSERT INTO shell_writers (writer_id, session_id, root_session_id, task_id)
             SELECT ?, ?, ?, ? WHERE NOT EXISTS (
                 SELECT 1 FROM claude_rewinds WHERE session_id IN (?, ?) AND applied_seq = 0
             ) AND NOT EXISTS (
                 SELECT 1 FROM session_ownership WHERE session_id IN (?, ?) AND blocked != 0
             )",
        )
        .bind(&id)
        .bind(&self.session_id)
        .bind(&self.root_session_id)
        .bind(task_id)
        .bind(&self.session_id)
        .bind(&self.root_session_id)
        .bind(&self.session_id)
        .bind(&self.root_session_id)
        .execute(&self.journal.pool)
        .await
        .map_err(std::io::Error::other)?;
        if inserted.rows_affected() != 1 {
            return Err(std::io::Error::other("Shell launch refused while Rewind awaits confirmation or session ownership is blocked"));
        }
        Ok(id)
    }

    async fn launched(&self, reservation: &str, local_pgid: Option<i32>) -> std::io::Result<()> {
        let updated = sqlx::query("UPDATE shell_writers SET pgid = ? WHERE writer_id = ? AND session_id = ?")
            .bind(local_pgid)
            .bind(reservation)
            .bind(&self.session_id)
            .execute(&self.journal.pool)
            .await
            .map_err(std::io::Error::other)?;
        if updated.rows_affected() != 1 {
            return Err(std::io::Error::other("Shell writer ownership was lost"));
        }
        Ok(())
    }

    async fn not_launched(&self, reservation: &str) -> std::io::Result<()> {
        sqlx::query("DELETE FROM shell_writers WHERE writer_id = ? AND session_id = ? AND pgid IS NULL")
            .bind(reservation)
            .bind(&self.session_id)
            .execute(&self.journal.pool)
            .await
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// Upload position of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub session_id: String,
    pub last_seq: u64,
    pub acked_seq: u64,
    pub epoch: u64,
}

impl Journal {
    /// Called before credential resolution / request dispatch, so a failed
    /// foreground call cannot leave an older prefix eligible for a warm ping.
    pub async fn clear_keep_warm_snapshot(&self, session_id: &str, epoch: u64) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        sqlx::query("DELETE FROM keep_warm_snapshots WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn save_keep_warm_snapshot(&self, session_id: &str, snapshot: &KeepWarmSnapshot, epoch: u64) -> anyhow::Result<()> {
        let data = serde_json::to_string(snapshot)?;
        let generation = uuid::Uuid::now_v7().to_string();
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        sqlx::query(
            "INSERT INTO keep_warm_snapshots (session_id, call_ms, connection_id, generation, ready, data) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET call_ms = excluded.call_ms, connection_id = excluded.connection_id,
                 generation = excluded.generation, ready = excluded.ready, data = excluded.data",
        )
        .bind(session_id)
        .bind(snapshot.last_call_ms)
        .bind(&snapshot.connection_id)
        .bind(generation)
        .bind(snapshot.response.is_some())
        .bind(data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn load_keep_warm_snapshot(&self, session_id: &str) -> anyhow::Result<Option<KeepWarmSnapshot>> {
        let data: Option<String> = sqlx::query_scalar("SELECT data FROM keep_warm_snapshots WHERE session_id = ? AND data IS NOT NULL")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        data.map(|data| serde_json::from_str(&data).map_err(Into::into)).transpose()
    }

    /// Conservative cache age without loading or parsing a large request.
    /// Unfinished, too-small and legacy response-less prefixes are excluded.
    pub async fn keep_warm_call_ms(&self, session_id: &str) -> anyhow::Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT call_ms FROM keep_warm_snapshots WHERE session_id = ? AND ready = 1 AND data IS NOT NULL")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Usage and completed output belong only to the request that generated
    /// them, never to a newer foreground request replacing this snapshot.
    pub async fn update_keep_warm_usage(&self, session_id: &str, snapshot: &KeepWarmSnapshot, epoch: u64) -> anyhow::Result<()> {
        let data = serde_json::to_string(snapshot)?;
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        sqlx::query("UPDATE keep_warm_snapshots SET data = ?, ready = ? WHERE session_id = ? AND call_ms = ? AND connection_id = ? AND data IS NOT NULL")
            .bind(data)
            .bind(snapshot.response.is_some())
            .bind(session_id)
            .bind(snapshot.last_call_ms)
            .bind(&snapshot.connection_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn invalidate_keep_warm_request(&self, session_id: &str, snapshot: &KeepWarmSnapshot, epoch: u64) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        sqlx::query("DELETE FROM keep_warm_snapshots WHERE session_id = ? AND call_ms = ? AND connection_id = ?")
            .bind(session_id)
            .bind(snapshot.last_call_ms)
            .bind(&snapshot.connection_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A small reservation fences superseded streams even when they share an
    /// injected timestamp. No request JSON is serialized or written here.
    pub async fn reserve_keep_warm_snapshot(
        &self,
        session_id: &str,
        connection_id: &str,
        call_ms: i64,
        epoch: u64,
    ) -> anyhow::Result<String> {
        let generation = uuid::Uuid::now_v7().to_string();
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        sqlx::query(
            "INSERT INTO keep_warm_snapshots (session_id, call_ms, connection_id, generation, data) VALUES (?, ?, ?, ?, NULL)
            ON CONFLICT(session_id) DO UPDATE SET call_ms = excluded.call_ms, connection_id = excluded.connection_id,
                generation = excluded.generation, ready = 0, data = NULL",
        )
        .bind(session_id)
        .bind(call_ms)
        .bind(connection_id)
        .bind(&generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(generation)
    }

    /// Commit a measured, complete response only if this stream still owns
    /// the reservation. Serialize the large request before the write lock.
    pub async fn complete_keep_warm_snapshot(
        &self,
        session_id: &str,
        generation: &str,
        snapshot: &KeepWarmSnapshot,
        epoch: u64,
    ) -> anyhow::Result<bool> {
        let data = serde_json::to_string(snapshot)?;
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        let updated = sqlx::query("UPDATE keep_warm_snapshots SET data = ?, ready = ? WHERE session_id = ? AND generation = ?")
            .bind(data)
            .bind(snapshot.response.is_some())
            .bind(session_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(updated)
    }

    pub async fn invalidate_keep_warm_generation(&self, session_id: &str, generation: &str, epoch: u64) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        sqlx::query("DELETE FROM keep_warm_snapshots WHERE session_id = ? AND generation = ?")
            .bind(session_id)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub fn shell_owner(self: &Arc<Self>, session_id: &str, root_session_id: &str) -> Arc<dyn c6_tools::ShellOwner> {
        Arc::new(ShellOwner { journal: self.clone(), session_id: session_id.into(), root_session_id: root_session_id.into() })
    }

    /// Include unlogged children by owner ID, and own tasks by task ID. Rows
    /// survive ESRCH so a failed reset cannot lose late-notice provenance.
    pub async fn shell_writer_sources(&self, session_id: &str) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT CASE WHEN session_id = ? THEN task_id ELSE session_id END
             FROM shell_writers WHERE session_id = ? OR root_session_id = ?",
        )
        .bind(session_id)
        .bind(session_id)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Never signal a persisted group. Leader completion and SSH proxy exit
    /// are not proof. Only ESRCH retires local writer ownership, not its ID.
    pub async fn confirm_shell_writers_stopped(&self, session_id: &str) -> anyhow::Result<()> {
        let rows = sqlx::query(
            "SELECT writer_id, task_id, pgid FROM shell_writers
             WHERE (session_id = ? OR root_session_id = ?) AND absent = 0",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            let task: String = row.get("task_id");
            let pgid: Option<i32> = row.try_get("pgid")?;
            let Some(pgid) = pgid else {
                anyhow::bail!(StopUnconfirmed(format!(
                    "Background shell {task} has unknown writer identity (incomplete launch or SSH); cannot confirm it stopped"
                )));
            };
            anyhow::ensure!(
                nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None) == Err(nix::errno::Errno::ESRCH),
                StopUnconfirmed(format!("Background shell {task} process group {pgid} has not confirmed that it stopped"))
            );
            sqlx::query("UPDATE shell_writers SET absent = 1 WHERE writer_id = ?")
                .bind(row.get::<String, _>("writer_id"))
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Record a legacy held-process cleanup barrier, even if its incarnation
    /// is no longer active. Live actors must use track_claude_process_owned.
    pub async fn track_claude_process(&self, session_id: &str, pgid: i32) -> anyhow::Result<()> {
        track_claude_process_in(&mut *self.pool.acquire().await?, session_id, pgid).await
    }

    pub async fn track_claude_process_owned(&self, session_id: &str, pgid: i32, epoch: u64) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        track_claude_process_in(&mut tx, session_id, pgid).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn claude_processes(&self, session_id: &str) -> anyhow::Result<Vec<i32>> {
        let groups = sqlx::query_scalar::<_, i64>("SELECT pgid FROM claude_processes WHERE session_id = ? ORDER BY pgid")
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;
        groups
            .into_iter()
            .map(|pgid| {
                let pgid = i32::try_from(pgid)
                    .map_err(|error| anyhow::Error::new(error).context(StopUnconfirmed("Claude Code process group is unknown".into())))?;
                anyhow::ensure!(pgid > 0, StopUnconfirmed("Claude Code process group is unknown".into()));
                Ok(pgid)
            })
            .collect()
    }

    /// Both live shell writers and retained Claude runtimes belong to their
    /// session even between turns. The sampler deduplicates overlapping trees.
    pub(crate) async fn machine_process_roots(&self) -> anyhow::Result<Vec<(u32, String)>> {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT pgid, session_id FROM shell_writers WHERE pgid > 0 AND absent = 0 UNION SELECT pgid, session_id FROM claude_processes WHERE pgid > 0",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(|(pid, session)| u32::try_from(pid).ok().map(|pid| (pid, session))).collect())
    }

    /// Advance legacy projections by at most 256 events per investigation,
    /// in pages of 64. Each page and its checkpoint commit together. Appends
    /// cannot race a backfill page because both hold the SQLite writer lock.
    async fn backfill_build_folder_places(&self) -> anyhow::Result<()> {
        const BUDGET: i64 = 256;
        const PAGE: i64 = 64;
        let mut tx = self.begin_write().await?;
        let sessions = sqlx::query("SELECT s.id, s.last_seq, CASE WHEN length(s.spec) <= 65536 THEN s.spec END AS spec FROM sessions s LEFT JOIN build_folder_places p ON p.session_id = s.id WHERE p.session_id IS NULL OR (p.checkpoint_seq < s.last_seq AND p.backfill_blocked = 0) ORDER BY COALESCE(p.checkpoint_seq, 0), s.id LIMIT 256")
            .fetch_all(&mut *tx).await?;
        let mut remaining = BUDGET;
        for row in sessions {
            if remaining == 0 {
                break;
            }
            let session: String = row.get("id");
            let mut place = match build_folder_place_in(&mut tx, &session).await? {
                Some(place) => place,
                None => match row.get::<Option<Vec<u8>>, _>("spec").and_then(|data| CreateSession::decode(data.as_slice()).ok()) {
                    Some(spec) => BuildFolderPlace::from_spec(&spec),
                    None => {
                        let place = BuildFolderPlace { backfill_blocked: true, ..Default::default() };
                        save_build_folder_place_in(&mut tx, &session, &place).await?;
                        continue;
                    }
                },
            };
            let events: Vec<(i64, Option<Vec<u8>>)> = sqlx::query_as("SELECT seq, CASE WHEN length(data) <= 65536 THEN data END FROM events WHERE session_id = ? AND seq > ? ORDER BY seq LIMIT ?")
                .bind(&session).bind(place.checkpoint_seq).bind(PAGE.min(remaining)).fetch_all(&mut *tx).await?;
            remaining -= events.len() as i64;
            if events.is_empty() && place.checkpoint_seq < row.get::<i64, _>("last_seq") {
                place.known = false;
                place.backfill_blocked = true;
            }
            for (seq, data) in events {
                // A missing, unreadable or oversized receipt may contain a
                // move. Never jump its checkpoint or infer a current path.
                if seq != place.checkpoint_seq + 1 {
                    place.known = false;
                    place.backfill_blocked = true;
                    break;
                }
                let Some(data) = data else {
                    place.known = false;
                    place.backfill_blocked = true;
                    break;
                };
                let Ok(event) = SessionEvent::decode(data.as_slice()) else {
                    place.known = false;
                    place.backfill_blocked = true;
                    break;
                };
                if event.seq as i64 != seq {
                    place.known = false;
                    place.backfill_blocked = true;
                    break;
                }
                place.apply(&event);
            }
            save_build_folder_place_in(&mut tx, &session, &place).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Bounded placement input for build-folder inventory. Normal sessions
    /// use the append-maintained projection, independent of history length.
    /// Legacy rows remain partial until paginated backfill proves placement.
    pub(crate) async fn build_folder_places(&self, worker: &str) -> anyhow::Result<(Vec<crate::machines::build_folders::Reference>, bool)> {
        use crate::machines::build_folders::Reference;
        const LIMIT: i64 = 256;
        self.backfill_build_folder_places().await?;
        let rows = sqlx::query("SELECT s.id, s.last_seq, s.last_activity, a.archived_at_ms, p.checkpoint_seq, p.known, p.cwd, p.repository, p.worker_id, p.local FROM sessions s LEFT JOIN build_folder_archives a ON a.session_id = s.id LEFT JOIN build_folder_places p ON p.session_id = s.id ORDER BY s.last_activity DESC LIMIT ?")
            .bind(LIMIT + 1).fetch_all(&self.pool).await?;
        let mut partial = rows.len() > LIMIT as usize;
        let mut places = Vec::new();
        for row in rows.into_iter().take(LIMIT as usize) {
            let session: String = row.get("id");
            if row.get::<Option<i64>, _>("checkpoint_seq") != Some(row.get("last_seq")) || row.get::<Option<bool>, _>("known") != Some(true)
            {
                partial = true;
                continue;
            }
            let owner: String = row.get("worker_id");
            if !row.get::<bool, _>("local") || (!owner.is_empty() && owner != worker) {
                continue;
            }
            let cwd: String = row.get("cwd");
            if cwd.is_empty() {
                partial = true;
                continue;
            }
            // Canonicalization and ownership checks belong on the blocking
            // pool, together with the actual filesystem investigation.
            places.push(Reference {
                path: cwd.into(),
                scope: true,
                session_id: session,
                repository: row.get("repository"),
                carbon6_owned: false,
                active: false,
                archived_at_ms: row.get("archived_at_ms"),
                last_used_ms: Some(row.get("last_activity")),
            });
        }
        let retained = sqlx::query("SELECT a.session_id, a.archived_at_ms, a.last_used_ms, p.known, p.cwd, p.repository, p.worker_id, p.local FROM build_folder_archives a LEFT JOIN build_folder_places p ON p.session_id = a.session_id WHERE (p.known = 0 OR p.local = 1 OR a.cwd != '' OR p.session_id IS NULL) AND NOT EXISTS (SELECT 1 FROM sessions WHERE id = a.session_id) LIMIT 257")
            .fetch_all(&self.pool).await?;
        partial |= retained.len() > LIMIT as usize;
        for row in retained.into_iter().take(LIMIT as usize) {
            if row.get::<Option<bool>, _>("known") != Some(true) {
                partial = true;
                continue;
            }
            let owner: String = row.get("worker_id");
            if !row.get::<bool, _>("local") || (!owner.is_empty() && owner != worker) {
                continue;
            }
            places.push(Reference {
                path: std::path::PathBuf::from(row.get::<String, _>("cwd")),
                scope: true,
                session_id: row.get("session_id"),
                repository: row.get("repository"),
                carbon6_owned: false,
                active: false,
                archived_at_ms: row.get("archived_at_ms"),
                last_used_ms: row.get("last_used_ms"),
            });
        }
        Ok((places, partial))
    }

    pub(crate) async fn set_build_folder_archives(&self, worker: &str, states: &[(String, Option<i64>)]) -> anyhow::Result<()> {
        let (places, _) = self.build_folder_places(worker).await?;
        let mut tx = self.begin_write().await?;
        for (session, archived) in states {
            sqlx::query("INSERT INTO build_folder_archives (session_id, archived_at_ms) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET archived_at_ms = excluded.archived_at_ms")
                .bind(session).bind(archived).execute(&mut *tx).await?;
            if let Some(place) = places.iter().find(|place| &place.session_id == session) {
                sqlx::query("UPDATE build_folder_archives SET cwd = ?, repository = ?, last_used_ms = ? WHERE session_id = ?")
                    .bind(place.path.to_string_lossy().as_ref())
                    .bind(&place.repository)
                    .bind(place.last_used_ms)
                    .bind(session)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn build_folder_active_sessions(&self) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT session_id FROM shell_writers WHERE absent = 0 UNION SELECT session_id FROM claude_processes UNION SELECT id FROM sessions WHERE open_turn IS NOT NULL OR open_tasks > 0")
            .fetch_all(&self.pool).await?)
    }

    /// Only callers that observed ESRCH may retire a cleanup barrier.
    pub async fn untrack_claude_process(&self, session_id: &str, pgid: i32) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM claude_processes WHERE session_id = ? AND pgid = ?")
            .bind(session_id)
            .bind(pgid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn claude_turn(&self, session_id: &str, seq: u64) -> anyhow::Result<Option<crate::claude_code::Boundary>> {
        let data: Option<String> = sqlx::query_scalar("SELECT data FROM claude_turns WHERE session_id = ? AND seq = ?")
            .bind(session_id)
            .bind(seq as i64)
            .fetch_optional(&self.pool)
            .await?;
        data.map(|s| serde_json::from_str(&s).map_err(Into::into)).transpose()
    }

    pub async fn record_claude_turn(&self, session_id: &str, seq: u64, boundary: &crate::claude_code::Boundary) -> anyhow::Result<()> {
        self.record_claude_turn_owned(session_id, seq, boundary, 0).await
    }

    pub async fn record_claude_turn_owned(
        &self,
        session_id: &str,
        seq: u64,
        boundary: &crate::claude_code::Boundary,
        epoch: u64,
    ) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        sqlx::query("INSERT OR REPLACE INTO claude_turns (session_id, seq, data) VALUES (?, ?, ?)")
            .bind(session_id)
            .bind(seq as i64)
            .bind(serde_json::to_string(boundary)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn claude_rewind(&self, session_id: &str) -> anyhow::Result<Option<(crate::claude_code::Reset, u64)>> {
        let row = sqlx::query("SELECT data, applied_seq FROM claude_rewinds WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| Ok((serde_json::from_str(&r.get::<String, _>("data"))?, r.get::<i64, _>("applied_seq") as u64))).transpose()
    }

    pub async fn prepare_claude_rewind(&self, session_id: &str, reset: &crate::claude_code::Reset) -> anyhow::Result<()> {
        self.prepare_claude_rewind_owned(session_id, reset, 0).await
    }

    pub async fn prepare_claude_rewind_owned(&self, session_id: &str, reset: &crate::claude_code::Reset, epoch: u64) -> anyhow::Result<()> {
        let _ownership = self.claude_rewind_ownership.lock().await;
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        let mut reset = reset.clone();
        // An actor can be initializing while the manager admits an explicit
        // retry. Its older snapshot must not erase that durable authorization.
        if let Some(data) =
            sqlx::query_scalar::<_, String>("SELECT data FROM claude_rewinds WHERE session_id = ? AND command_id = ? AND applied_seq = 0")
                .bind(session_id)
                .bind(&reset.command_id)
                .fetch_optional(&mut *tx)
                .await?
        {
            let current: crate::claude_code::Reset = serde_json::from_str(&data)?;
            for id in current.retries {
                if !reset.retries.contains(&id) {
                    reset.retries.push(id);
                }
            }
        }
        sqlx::query("INSERT OR REPLACE INTO claude_rewinds (session_id, command_id, data) VALUES (?, ?, ?)")
            .bind(session_id)
            .bind(&reset.command_id)
            .bind(serde_json::to_string(&reset)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// False means the actor confirmed the old reset concurrently; dispatch
    /// must reread its mapping and prepare a new intent instead.
    pub async fn authorize_claude_rewind_retry(&self, session_id: &str, original: &str, retry: &str) -> anyhow::Result<bool> {
        self.authorize_claude_rewind_retry_owned(session_id, original, retry, 0).await
    }

    pub async fn authorize_claude_rewind_retry_owned(
        &self,
        session_id: &str,
        original: &str,
        retry: &str,
        epoch: u64,
    ) -> anyhow::Result<bool> {
        let _ownership = self.claude_rewind_ownership.lock().await;
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        // Dispatch's receipt check can race a failure committing on the
        // actor. Never resurrect an authorization already consumed by it.
        if let Some(rejection) = sqlx::query_scalar::<_, Option<String>>("SELECT rejection FROM applied_commands WHERE command_id = ?")
            .bind(retry)
            .fetch_optional(&mut *tx)
            .await?
        {
            anyhow::bail!("{}", rejection.unwrap_or_else(|| "Rewind is already confirmed".into()));
        }
        let Some(data) =
            sqlx::query_scalar::<_, String>("SELECT data FROM claude_rewinds WHERE session_id = ? AND command_id = ? AND applied_seq = 0")
                .bind(session_id)
                .bind(original)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(false);
        };
        let mut reset: crate::claude_code::Reset = serde_json::from_str(&data)?;
        if retry != original && !reset.retries.iter().any(|id| id == retry) {
            reset.retries.push(retry.into());
            sqlx::query("UPDATE claude_rewinds SET data = ? WHERE session_id = ?")
                .bind(serde_json::to_string(&reset)?)
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// The error and recovery decision are atomic. A crash cannot leave a
    /// reported preflight failure waiting to retry, or lose the prior branch.
    pub async fn fail_claude_rewind(
        &self,
        session_id: &str,
        command_id: &str,
        event: &SessionEvent,
        retries: &[String],
    ) -> anyhow::Result<(bool, Vec<String>)> {
        self.fail_claude_rewind_owned(session_id, command_id, event, retries, 0).await
    }

    pub async fn fail_claude_rewind_owned(
        &self,
        session_id: &str,
        command_id: &str,
        event: &SessionEvent,
        retries: &[String],
        epoch: u64,
    ) -> anyhow::Result<(bool, Vec<String>)> {
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        let row = sqlx::query("SELECT data FROM claude_rewinds WHERE session_id = ? AND command_id = ? AND applied_seq = 0")
            .bind(session_id)
            .bind(command_id)
            .fetch_optional(&mut *tx)
            .await?;
        let pending = row.is_some();
        let mut reset: crate::claude_code::Reset = match row {
            Some(row) => serde_json::from_str(&row.get::<String, _>("data"))?,
            None => crate::claude_code::Reset { untouched: true, ..Default::default() },
        };
        let Some(session_event::Kind::Error(error)) = &event.kind else { anyhow::bail!("Reset failure requires an error event") };
        let mut rejected = std::mem::take(&mut reset.retries);
        for retry in retries {
            if !rejected.contains(retry) {
                rejected.push(retry.clone());
            }
        }
        let blocked = !reset.untouched;
        if blocked {
            reset.failure = error.message.clone();
            sqlx::query("UPDATE claude_rewinds SET data = ? WHERE session_id = ?")
                .bind(serde_json::to_string(&reset)?)
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
        } else if pending {
            sqlx::query("DELETE FROM claude_rewinds WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
        }
        if !blocked && let Some(previous) = reset.previous {
            let (reset, applied) = *previous;
            anyhow::ensure!(applied != 0, "Cannot restore an unconfirmed branch");
            sqlx::query("INSERT INTO claude_rewinds (session_id, command_id, data, applied_seq) VALUES (?, ?, ?, ?)")
                .bind(session_id)
                .bind(&reset.command_id)
                .bind(serde_json::to_string(&reset)?)
                .bind(applied as i64)
                .execute(&mut *tx)
                .await?;
        }
        // An explicit retry has a different head command ID. If recovery of
        // the original fails again, retire that retry too; its redelivery is
        // NOT another user action. Keep the original destructive barrier.
        if !blocked {
            rejected.push(command_id.into());
        }
        for id in &rejected {
            sqlx::query("INSERT OR IGNORE INTO applied_commands (command_id, session_id, rejection) VALUES (?, ?, ?)")
                .bind(id)
                .bind(session_id)
                .bind(&error.message)
                .execute(&mut *tx)
                .await?;
        }
        append_in(&mut tx, session_id, event).await?;
        tx.commit().await?;
        self.appended.notify_one();
        Ok((blocked, rejected))
    }

    pub async fn command_rejection(&self, command_id: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT rejection FROM applied_commands WHERE command_id = ?")
            .bind(command_id)
            .fetch_optional(&self.pool)
            .await?
            .flatten())
    }

    pub(crate) async fn reject_queued_launches(&self, session: &str, epoch: u64, ids: &[String], reason: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        check_ownership_in(&mut tx, session, epoch).await?;
        for id in ids {
            sqlx::query("INSERT OR IGNORE INTO applied_commands (command_id, session_id, rejection) VALUES (?, ?, ?)")
                .bind(id)
                .bind(session)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Confirm one reset and every retry authorized before its confirmation
    /// atomically. A retry waiting in the actor's mailbox must not reset again
    /// over resumed input, or lose its durable ownership to another retry.
    /// Returns the number of consecutive events appended under the writer lock.
    pub async fn append_rewind(&self, session_id: &str, event: &SessionEvent) -> anyhow::Result<u64> {
        self.append_rewind_owned(session_id, event, 0).await
    }

    pub async fn append_rewind_owned(&self, session_id: &str, event: &SessionEvent, epoch: u64) -> anyhow::Result<u64> {
        let Some(session_event::Kind::Rewound(rewound)) = &event.kind else { anyhow::bail!("Expected a rewind confirmation") };
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        let data =
            sqlx::query_scalar::<_, String>("SELECT data FROM claude_rewinds WHERE session_id = ? AND command_id = ? AND applied_seq = 0")
                .bind(session_id)
                .bind(&rewound.command_id)
                .fetch_optional(&mut *tx)
                .await?;
        let reset = data.map(|data| serde_json::from_str::<crate::claude_code::Reset>(&data)).transpose()?;
        append_in(&mut tx, session_id, event).await?;
        let mut count = 1;
        if let Some(mut reset) = reset {
            anyhow::ensure!(reset.to_seq == rewound.to_seq, "Rewind confirmation changed its target");
            for command_id in std::mem::take(&mut reset.retries) {
                if sqlx::query_scalar::<_, i64>("SELECT 1 FROM applied_commands WHERE command_id = ?")
                    .bind(&command_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .is_some()
                {
                    continue;
                }
                let confirmation = SessionEvent {
                    seq: event.seq + count,
                    ts_ms: event.ts_ms,
                    kind: Some(session_event::Kind::Rewound(c6_proto::Rewound { command_id, ..rewound.clone() })),
                };
                append_in(&mut tx, session_id, &confirmation).await?;
                count += 1;
            }
            // All receipt markers describe this branch. Its highwater must
            // cover the final one, or reload would discard the native mapping.
            sqlx::query("UPDATE claude_rewinds SET data = ?, applied_seq = ? WHERE session_id = ? AND command_id = ?")
                .bind(serde_json::to_string(&reset)?)
                .bind((event.seq + count - 1) as i64)
                .bind(session_id)
                .bind(&rewound.command_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.appended.notify_one();
        Ok(count)
    }

    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let opts = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // An external connection can still hold the writer lock. This
            // wait applies to both transactions and standalone writes.
            .busy_timeout(Duration::from_secs(30))
            // FULL: an event must survive power loss once appended.
            .synchronous(SqliteSynchronous::Full);
        // SQLite has only one writer. Queue local operations in the pool
        // rather than racing for the WAL write lock on separate connections.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(60))
            .before_acquire(|conn, _| {
                Box::pin(async move {
                    // Drain the worker queue, including rollback-on-drop,
                    // before inspecting SQLx's transaction depth. SQLite can
                    // implicitly roll back on FULL/IOERR; SQLx then fails its
                    // own ROLLBACK and retains a stale nonzero depth. Never
                    // lend that connection to the next reader or writer.
                    conn.ping().await?;
                    Ok(!conn.is_in_transaction())
                })
            })
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations").run(&pool).await.context("journal migrations")?;
        Ok(Self {
            pool,
            build_folder_lifecycle: Arc::new(tokio::sync::RwLock::new(())),
            #[cfg(test)]
            recovery_error: std::sync::Mutex::new(None),
            claude_rewind_ownership: tokio::sync::Mutex::new(()),
            appended: Notify::new(),
        })
    }

    /// Discard the journal's single connection before recovery reads or writes.
    /// SQLx holds the pool permit through close; the next checkout opens a
    /// fresh connection with the same options. No interrupted SQL is replayed:
    /// callers must reload durable state if a write's outcome is uncertain.
    pub(crate) async fn recover_connection(&self) -> anyhow::Result<()> {
        #[cfg(test)]
        self.check_injected_recovery_error().await?;
        self.pool
            .acquire()
            .await
            .context("checking out journal connection for recovery")?
            .close()
            .await
            .context("closing journal connection for recovery")
    }

    async fn begin_write(&self) -> sqlx::Result<Transaction<'_, Sqlite>> {
        // Wait for the writer lock before any reads; a deferred read
        // snapshot can fail to upgrade without honoring the busy timeout.
        self.pool.begin_with("BEGIN IMMEDIATE").await
    }

    /// Missing rows denote legacy epoch zero, with no move in progress.
    pub async fn ownership(&self, session_id: &str) -> anyhow::Result<(u64, bool, String)> {
        ownership_in(&mut *self.pool.acquire().await?, session_id).await
    }

    pub async fn check_ownership(&self, session_id: &str, epoch: u64) -> anyhow::Result<()> {
        check_ownership_in(&mut *self.pool.acquire().await?, session_id, epoch).await
    }

    /// Freeze the current incarnation. The transaction serializes the
    /// open-turn and pending-rewind checks with append and rewind preparation.
    pub async fn block_ownership(&self, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!move_id.is_empty(), "missing move ID for session {session_id}");
        let mut tx = self.begin_write().await?;
        let current = ownership_in(&mut tx, session_id).await?;
        ensure_epoch(session_id, current.0, epoch)?;
        anyhow::ensure!(
            !move_aborted_in(&mut tx, session_id, epoch, move_id).await?,
            "move {move_id} was aborted for session {session_id}"
        );
        if current.1 {
            anyhow::ensure!(current.2 == move_id, "session {session_id} is blocked by another move");
            return Ok(());
        }
        anyhow::ensure!(current.2 != move_id, "move {move_id} already released session {session_id}");
        let open_turn: Option<i64> = sqlx::query_scalar("SELECT open_turn FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?
            .with_context(|| format!("session {session_id} is not in the journal"))?;
        anyhow::ensure!(open_turn.is_none(), "session {session_id} has an open turn");
        ensure_no_pending_rewind(&mut tx, session_id).await?;
        let writer = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM shell_writers WHERE (session_id = ? OR root_session_id = ?) AND absent = 0 LIMIT 1",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?;
        anyhow::ensure!(writer.is_none(), "session {session_id} has an unconfirmed shell writer");
        let process = sqlx::query_scalar::<_, i64>("SELECT 1 FROM claude_processes WHERE session_id = ? LIMIT 1")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
        anyhow::ensure!(process.is_none(), "session {session_id} has an unconfirmed Claude process");
        block_in(&mut tx, session_id, epoch, move_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn unblock_ownership(&self, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
        self.abort_source_move(session_id, epoch, move_id).await
    }

    /// Cheap preflight for source freeze, before it stops any local writers.
    /// block_ownership checks again inside the transaction that sets the fence.
    pub async fn move_aborted(&self, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<bool> {
        move_aborted_in(&mut *self.pool.acquire().await?, session_id, epoch, move_id).await
    }

    /// Record the decision even if the source has not received the freeze
    /// yet. A delayed freeze can never set its fence after this commits.
    pub async fn abort_source_move(&self, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!move_id.is_empty(), "missing move ID for session {session_id}");
        let mut tx = self.begin_write().await?;
        let current = ownership_in(&mut tx, session_id).await?;
        ensure_epoch(session_id, current.0, epoch)?;
        anyhow::ensure!(!current.1 || current.2 == move_id, "session {session_id} is blocked by another move");
        abort_move_in(&mut tx, session_id, epoch, move_id).await?;
        if current.1 {
            sqlx::query("UPDATE session_ownership SET blocked = 0 WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Invalidate a target preparation even if it has not imported yet.
    /// Existing prepared history remains blocked and may be replaced only
    /// at a higher epoch. Never invalidate an already-active adoption.
    pub async fn abort_target_move(&self, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!move_id.is_empty(), "missing move ID for session {session_id}");
        let mut tx = self.begin_write().await?;
        let current = ownership_in(&mut tx, session_id).await?;
        anyhow::ensure!(
            current.0 != epoch || current.1 || current.2 != move_id,
            "move {move_id} is already active for session {session_id}"
        );
        abort_move_in(&mut tx, session_id, epoch, move_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn activate_ownership(&self, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!move_id.is_empty(), "missing move ID for session {session_id}");
        let mut tx = self.begin_write().await?;
        let current = ownership_in(&mut tx, session_id).await?;
        ensure_epoch(session_id, current.0, epoch)?;
        anyhow::ensure!(current.2 == move_id, "move ID mismatch for session {session_id}");
        anyhow::ensure!(
            !move_aborted_in(&mut tx, session_id, epoch, move_id).await?,
            "move {move_id} was aborted for session {session_id}"
        );
        if current.1 {
            sqlx::query("UPDATE session_ownership SET blocked = 0 WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn is_applied(&self, command_id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("SELECT 1 FROM applied_commands WHERE command_id = ?").bind(command_id).fetch_optional(&self.pool).await?.is_some())
    }

    pub async fn has_session(&self, session_id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("SELECT 1 FROM sessions WHERE id = ?").bind(session_id).fetch_optional(&self.pool).await?.is_some())
    }

    /// Local children own receipts here; template children's obligations
    /// belong to the head instead (they have no local Delegation).
    pub async fn local_subagent_ids(&self) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query("SELECT id, spec FROM sessions").fetch_all(&self.pool).await?;
        let mut ids = Vec::new();
        for row in rows {
            let spec = CreateSession::decode(row.get::<Vec<u8>, _>("spec").as_slice())?;
            if spec.delegation.is_some() {
                ids.push(row.get("id"));
            }
        }
        Ok(ids)
    }

    /// Records a new session with its first event (`SessionStarted`, seq 1).
    pub async fn create_session(&self, session_id: &str, spec: &CreateSession, started: &SessionEvent) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, 0).await?;
        let now = c6_core::now_ms();
        sqlx::query("INSERT INTO sessions (id, spec, created_at, last_activity) VALUES (?, ?, ?, ?)")
            .bind(session_id)
            .bind(spec.encode_to_vec())
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        save_build_folder_place_in(&mut tx, session_id, &BuildFolderPlace::from_spec(spec)).await?;
        append_in(&mut tx, session_id, started).await?;
        tx.commit().await?;
        self.appended.notify_one();
        Ok(())
    }

    /// Appends an event. The caller (the session's single writer) assigns
    /// consecutive seqs.
    pub async fn append(&self, session_id: &str, event: &SessionEvent) -> anyhow::Result<()> {
        self.append_owned(session_id, event, 0).await
    }

    pub async fn append_owned(&self, session_id: &str, event: &SessionEvent, epoch: u64) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await.with_context(|| format!("starting append of seq {} to {session_id}", event.seq))?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        append_in(&mut tx, session_id, event).await?;
        tx.commit().await?;
        self.appended.notify_one();
        Ok(())
    }

    /// Admit initial Claude inputs and their turn together, before any stdin
    /// writes. A failed boundary must not leave receipts for unsent inputs.
    pub(crate) async fn append_turn_owned(&self, session_id: &str, events: &[SessionEvent], epoch: u64) -> anyhow::Result<()> {
        let (started, inputs) = events.split_last().context("missing turn boundary")?;
        anyhow::ensure!(
            matches!(started.kind, Some(session_event::Kind::TurnStarted(_)))
                && inputs.iter().all(|event| matches!(event.kind, Some(session_event::Kind::User(_)))),
            "expected initial User events followed by TurnStarted"
        );
        let mut tx = self.begin_write().await.with_context(|| format!("starting turn append to {session_id}"))?;
        check_ownership_in(&mut tx, session_id, epoch).await?;
        for event in events {
            append_in(&mut tx, session_id, event).await?;
        }
        tx.commit().await?;
        self.appended.notify_one();
        Ok(())
    }

    pub async fn load_session(&self, session_id: &str) -> anyhow::Result<Option<(CreateSession, Vec<SessionEvent>)>> {
        let mut tx = self.pool.begin().await?;
        let loaded = load_session_in(&mut tx, session_id).await?;
        tx.commit().await?;
        Ok(loaded)
    }

    /// Actor startup must bind its writer to the same snapshot as the spec
    /// and history. A blocked incarnation is an error, not a cache miss.
    pub async fn load_session_owned(&self, session_id: &str) -> anyhow::Result<Option<(CreateSession, Vec<SessionEvent>, u64)>> {
        let mut tx = self.pool.begin().await?;
        let current = ownership_in(&mut tx, session_id).await?;
        ensure_unblocked(session_id, &current)?;
        let loaded = load_session_in(&mut tx, session_id).await?;
        tx.commit().await?;
        Ok(loaded.map(|(spec, events)| (spec, events, current.0)))
    }

    pub(crate) async fn known_move_folders(&self, local_worker: &str) -> anyhow::Result<Vec<std::path::PathBuf>> {
        let mut folders = std::collections::BTreeSet::new();
        {
            let mut specs = sqlx::query_scalar::<_, Vec<u8>>("SELECT spec FROM sessions").fetch(&self.pool);
            while let Some(data) = specs.try_next().await? {
                if let Ok(spec) = CreateSession::decode(data.as_slice())
                    && spec.ssh_target.is_empty()
                    && !spec.cwd.is_empty()
                {
                    folders.insert(std::path::PathBuf::from(spec.cwd));
                }
            }
        }
        // CreateSession.cwd can be empty (scratch), unexpanded, or refer to the
        // checkout before worktree creation. Started and placement receipts are
        // the actual worker-local paths, including retained previous places.
        // Imported histories retain places on other workers. Infer the initial
        // owner from the first tagged placement; preceding Started/legacy
        // placements belong to that owner, not to whichever worker imported
        // the history. Histories with no tagged placement are local legacy.
        let mut session = String::new();
        let mut initial_owner = None::<String>;
        let mut pending = std::collections::BTreeSet::new();
        let mut events = sqlx::query("SELECT session_id, data FROM events ORDER BY session_id, seq").fetch(&self.pool);
        while let Some(row) = events.try_next().await? {
            let sid: String = row.get("session_id");
            if sid != session {
                if initial_owner.is_none() {
                    folders.append(&mut pending);
                }
                pending.clear();
                initial_owner = None;
                session = sid;
            }
            let data: Vec<u8> = row.get("data");
            let Ok(event) = SessionEvent::decode(data.as_slice()) else { continue };
            match event.kind {
                Some(session_event::Kind::Started(s)) if s.ssh_target.is_empty() => {
                    pending.extend([s.cwd, s.repo_root].into_iter().filter(|p| !p.is_empty()).map(std::path::PathBuf::from));
                }
                Some(session_event::Kind::PlaceChanged(p)) => {
                    if initial_owner.is_none() {
                        initial_owner = p
                            .from
                            .as_ref()
                            .filter(|p| !p.worker_id.is_empty())
                            .or_else(|| p.to.as_ref().filter(|p| !p.worker_id.is_empty()))
                            .map(|p| p.worker_id.clone());
                        if initial_owner.as_deref() == Some(local_worker) {
                            folders.append(&mut pending);
                        }
                        if initial_owner.is_some() {
                            pending.clear();
                        }
                    }
                    for place in [p.from, p.to].into_iter().flatten() {
                        let paths = [place.cwd, place.repo_root].into_iter().filter(|p| !p.is_empty()).map(std::path::PathBuf::from);
                        if place.worker_id == local_worker || (place.worker_id.is_empty() && initial_owner.as_deref() == Some(local_worker))
                        {
                            folders.extend(paths);
                        } else if place.worker_id.is_empty() && initial_owner.is_none() {
                            pending.extend(paths);
                        }
                    }
                }
                _ => {}
            }
        }
        if initial_owner.is_none() {
            folders.append(&mut pending);
        }
        Ok(folders.into_iter().collect())
    }

    pub(crate) async fn check_move_history_size(&self, session: &str, limit: u64) -> anyhow::Result<()> {
        let bytes: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(length(data)), 0) FROM events WHERE session_id = ?")
            .bind(session)
            .fetch_one(&self.pool)
            .await?;
        anyhow::ensure!(u64::try_from(bytes).is_ok_and(|n| n < limit), "session history exceeds the move size limit");
        Ok(())
    }

    pub async fn events_after(&self, session_id: &str, after: u64, limit: u32) -> anyhow::Result<Vec<SessionEvent>> {
        events_after_in(&mut *self.pool.acquire().await?, session_id, after, limit).await
    }

    /// Like events_after, but rejects a stale uploader cursor. A frozen
    /// source must still upload its last events and receive their acks.
    pub async fn events_after_owned(&self, session_id: &str, after: u64, limit: u32, epoch: u64) -> anyhow::Result<Vec<SessionEvent>> {
        let mut tx = self.pool.begin().await?;
        ensure_epoch(session_id, ownership_in(&mut tx, session_id).await?.0, epoch)?;
        let events = events_after_in(&mut tx, session_id, after, limit).await?;
        tx.commit().await?;
        Ok(events)
    }

    /// Sessions with events the head has not acknowledged.
    pub async fn unacked(&self) -> anyhow::Result<Vec<Cursor>> {
        let rows = sqlx::query(
            "SELECT id, last_seq, acked_seq, COALESCE(epoch, 0) AS epoch
             FROM sessions LEFT JOIN session_ownership ON session_id = id WHERE last_seq > acked_seq",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Cursor {
                session_id: r.get("id"),
                last_seq: r.get::<i64, _>("last_seq") as u64,
                acked_seq: r.get::<i64, _>("acked_seq") as u64,
                epoch: r.get::<i64, _>("epoch") as u64,
            })
            .collect())
    }

    pub async fn set_acked(&self, session_id: &str, seq: u64) -> anyhow::Result<()> {
        self.set_acked_owned(session_id, seq, 0).await
    }

    pub async fn set_acked_owned(&self, session_id: &str, seq: u64, epoch: u64) -> anyhow::Result<()> {
        let mut tx = self.begin_write().await?;
        ensure_epoch(session_id, ownership_in(&mut tx, session_id).await?.0, epoch)?;
        sqlx::query("UPDATE sessions SET acked_seq = MAX(acked_seq, MIN(?, last_seq)) WHERE id = ?")
            .bind(i64::try_from(seq).context("acknowledgment sequence exceeds SQLite INTEGER range")?)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Imports a session from the head's log (see `Restore`). Everything is
    /// already acknowledged.
    pub async fn import(&self, session_id: &str, spec: &CreateSession, events: &[SessionEvent]) -> anyhow::Result<()> {
        let warm = spec.keep_warm_snapshot_json.as_deref().map(serde_json::from_str::<KeepWarmSnapshot>).transpose()?;
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, 0).await?;
        import_in(&mut tx, session_id, spec, events, warm.as_ref()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Adopt acknowledged history in a single transaction. The caller must
    /// ensure there is no running actor (including an initializing actor).
    pub async fn import_owned(
        &self,
        session_id: &str,
        spec: &CreateSession,
        events: &[SessionEvent],
        epoch: u64,
        move_id: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!move_id.is_empty(), "missing move ID for session {session_id}");
        let warm = spec.keep_warm_snapshot_json.as_deref().map(serde_json::from_str::<KeepWarmSnapshot>).transpose()?;
        let mut tx = self.begin_write().await?;
        anyhow::ensure!(
            !move_aborted_in(&mut tx, session_id, epoch, move_id).await?,
            "move {move_id} was aborted for session {session_id}"
        );
        let current = ownership_in(&mut tx, session_id).await?;
        if epoch == current.0 && move_id == current.2 {
            // Duplicate adoption must not re-block an activated incarnation
            // or overwrite any events appended after its activation.
            return Ok(());
        }
        anyhow::ensure!(epoch > current.0, "adoption epoch {epoch} must exceed local epoch {} for session {session_id}", current.0);
        let existing =
            sqlx::query_scalar::<_, i64>("SELECT 1 FROM sessions WHERE id = ?").bind(session_id).fetch_optional(&mut *tx).await?;
        anyhow::ensure!(current.1 || existing.is_none(), "cannot replace unblocked session {session_id}");
        ensure_no_pending_rewind(&mut tx, session_id).await?;
        sqlx::query("DELETE FROM sessions WHERE id = ?").bind(session_id).execute(&mut *tx).await?;
        // Native Claude mappings do not describe the imported incarnation.
        // Process and shell ownership receipts are stop evidence, not history;
        // never discard them just because the journal is replaced.
        sqlx::query("DELETE FROM claude_turns WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM claude_rewinds WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
        import_in(&mut tx, session_id, spec, events, warm.as_ref()).await?;
        block_in(&mut tx, session_id, epoch, move_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drops sessions that are fully synced, not mid-turn, and idle since
    /// `before_ms` (except `keep`). The head can restore them on demand.
    pub async fn trim(&self, before_ms: i64, keep: &[String]) -> anyhow::Result<usize> {
        let mut tx = self.begin_write().await?;
        let rows = sqlx::query(
            "SELECT id FROM sessions WHERE acked_seq = last_seq AND open_turn IS NULL AND last_activity < ?
             AND NOT EXISTS (SELECT 1 FROM session_ownership WHERE session_id = id)",
        )
        .bind(before_ms)
        .fetch_all(&mut *tx)
        .await?;
        let mut n = 0;
        for r in rows {
            let id: String = r.get("id");
            if keep.contains(&id) {
                continue;
            }
            // Never turn a partially backfilled legacy projection into a
            // trusted retained placement by deleting its remaining history.
            sqlx::query("UPDATE build_folder_places SET known = 0 WHERE session_id = ? AND checkpoint_seq != (SELECT last_seq FROM sessions WHERE id = ?)")
                .bind(&id).bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM events WHERE session_id = ?").bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM applied_commands WHERE session_id = ?").bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM keep_warm_snapshots WHERE session_id = ?").bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM sessions WHERE id = ?").bind(&id).execute(&mut *tx).await?;
            n += 1;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// Removes events and applied commands whose session row is gone (an
    /// earlier build could trim a live session). Without this, the head's
    /// redelivered input would be taken as already applied.
    pub async fn drop_orphans(&self) -> anyhow::Result<u64> {
        let mut tx = self.begin_write().await?;
        let events =
            sqlx::query("DELETE FROM events WHERE session_id NOT IN (SELECT id FROM sessions)").execute(&mut *tx).await?.rows_affected();
        sqlx::query("DELETE FROM applied_commands WHERE session_id NOT IN (SELECT id FROM sessions)").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM keep_warm_snapshots WHERE session_id NOT IN (SELECT id FROM sessions)").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(events)
    }

    /// Sessions with background tasks that never ended.
    pub async fn sessions_with_open_tasks(&self) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM sessions WHERE open_tasks > 0
             AND NOT EXISTS (SELECT 1 FROM session_ownership WHERE session_id = id AND blocked != 0)",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Cached sessions active since the cutoff, whose notice logs may
    /// need recovery. Filter here so older sessions' logs aren't read.
    pub async fn recent_session_ids(&self, since_ms: i64) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM sessions WHERE last_activity >= ?").bind(since_ms).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Cached sessions eligible for notice recovery. Unlike attachment
    /// retention, recovery must not attempt to write to blocked history.
    pub async fn recent_unblocked_session_ids(&self, since_ms: i64) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM sessions WHERE last_activity >= ?
             AND NOT EXISTS (SELECT 1 FROM session_ownership WHERE session_id = id AND blocked != 0)",
        )
        .bind(since_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Sessions left mid-turn by a crash: (session id, last seq, open turn).
    pub async fn open_turns(&self) -> anyhow::Result<Vec<(String, u64, u64)>> {
        let rows = sqlx::query(
            "SELECT id, last_seq, open_turn FROM sessions WHERE open_turn IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM session_ownership WHERE session_id = id AND blocked != 0)",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("id"), r.get::<i64, _>("last_seq") as u64, r.get::<i64, _>("open_turn") as u64)).collect())
    }
}

async fn ownership_in(tx: &mut sqlx::SqliteConnection, session_id: &str) -> anyhow::Result<(u64, bool, String)> {
    let row = sqlx::query("SELECT epoch, blocked, move_id FROM session_ownership WHERE session_id = ?")
        .bind(session_id)
        .fetch_optional(tx)
        .await?;
    match row {
        Some(row) => Ok((u64::try_from(row.get::<i64, _>("epoch"))?, row.get::<i64, _>("blocked") != 0, row.get("move_id"))),
        None => Ok((0, false, String::new())),
    }
}

fn ensure_epoch(session_id: &str, current: u64, expected: u64) -> anyhow::Result<()> {
    anyhow::ensure!(current == expected, "ownership epoch mismatch for session {session_id}: local {current}, requested {expected}");
    Ok(())
}

fn ensure_unblocked(session_id: &str, current: &(u64, bool, String)) -> anyhow::Result<()> {
    anyhow::ensure!(!current.1, "ownership is blocked for session {session_id} by move {}", current.2);
    Ok(())
}

async fn check_ownership_in(tx: &mut sqlx::SqliteConnection, session_id: &str, epoch: u64) -> anyhow::Result<()> {
    let current = ownership_in(tx, session_id).await?;
    ensure_epoch(session_id, current.0, epoch)?;
    ensure_unblocked(session_id, &current)
}

async fn move_aborted_in(tx: &mut sqlx::SqliteConnection, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<bool> {
    let row = sqlx::query_scalar::<_, i64>("SELECT 1 FROM aborted_session_moves WHERE move_id = ? AND session_id = ? AND epoch = ?")
        .bind(move_id)
        .bind(session_id)
        .bind(i64::try_from(epoch).context("ownership epoch exceeds SQLite INTEGER range")?)
        .fetch_optional(tx)
        .await?;
    Ok(row.is_some())
}

async fn track_claude_process_in(tx: &mut sqlx::SqliteConnection, session_id: &str, pgid: i32) -> anyhow::Result<()> {
    anyhow::ensure!(pgid > 0, "Claude Code process group is unknown");
    sqlx::query("INSERT OR IGNORE INTO claude_processes (session_id, pgid) VALUES (?, ?)").bind(session_id).bind(pgid).execute(tx).await?;
    Ok(())
}

async fn abort_move_in(tx: &mut sqlx::SqliteConnection, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT OR IGNORE INTO aborted_session_moves (move_id, session_id, epoch) VALUES (?, ?, ?)")
        .bind(move_id)
        .bind(session_id)
        .bind(i64::try_from(epoch).context("ownership epoch exceeds SQLite INTEGER range")?)
        .execute(tx)
        .await?;
    Ok(())
}

async fn ensure_no_pending_rewind(tx: &mut sqlx::SqliteConnection, session_id: &str) -> anyhow::Result<()> {
    let pending = sqlx::query_scalar::<_, i64>("SELECT 1 FROM claude_rewinds WHERE session_id = ? AND applied_seq = 0")
        .bind(session_id)
        .fetch_optional(tx)
        .await?;
    anyhow::ensure!(pending.is_none(), "session {session_id} has a pending rewind");
    Ok(())
}

async fn block_in(tx: &mut sqlx::SqliteConnection, session_id: &str, epoch: u64, move_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO session_ownership (session_id, epoch, blocked, move_id) VALUES (?, ?, 1, ?)
         ON CONFLICT(session_id) DO UPDATE SET epoch = excluded.epoch, blocked = 1, move_id = excluded.move_id",
    )
    .bind(session_id)
    .bind(i64::try_from(epoch).context("ownership epoch exceeds SQLite INTEGER range")?)
    .bind(move_id)
    .execute(tx)
    .await?;
    Ok(())
}

async fn events_after_in(tx: &mut sqlx::SqliteConnection, session_id: &str, after: u64, limit: u32) -> anyhow::Result<Vec<SessionEvent>> {
    let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? AND seq > ? ORDER BY seq LIMIT ?")
        .bind(session_id)
        .bind(after as i64)
        .bind(i64::from(limit))
        .fetch_all(tx)
        .await?;
    rows.iter().map(|r| Ok(SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?)).collect()
}

async fn load_session_in(tx: &mut sqlx::SqliteConnection, session_id: &str) -> anyhow::Result<Option<(CreateSession, Vec<SessionEvent>)>> {
    let Some(row) =
        sqlx::query("SELECT spec, imported_through FROM sessions WHERE id = ?").bind(session_id).fetch_optional(&mut *tx).await?
    else {
        return Ok(None);
    };
    let mut spec = CreateSession::decode(row.get::<Vec<u8>, _>("spec").as_slice())?;
    spec.keep_warm_snapshot_json = sqlx::query_scalar("SELECT data FROM keep_warm_snapshots WHERE session_id = ? AND data IS NOT NULL")
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?;
    let imported_through = row.get::<i64, _>("imported_through") as u64;
    let events = events_after_in(tx, session_id, 0, u32::MAX).await?;
    // A restored spec already reflects the head's current settings. Only
    // snapshots written after import can supersede it; the full log remains
    // available to replay notice guard acknowledgements and other history.
    for event in events.iter().filter(|event| event.seq > imported_through) {
        apply_notice_settings_snapshot(&mut spec, event);
    }
    Ok(Some((spec, events)))
}

pub(crate) fn apply_notice_settings_snapshot(spec: &mut CreateSession, event: &SessionEvent) {
    #[derive(serde::Deserialize)]
    struct Snapshot {
        #[serde(flatten)]
        notice: c6_api::SubagentLimits,
        max_subagent_depth: Option<u32>,
        max_running_subagents: Option<u32>,
        claude_code_trim_enabled: Option<bool>,
        claude_code_trim_at: Option<u64>,
        keep_warm_json: Option<String>,
    }
    let Some(session_event::Kind::Notice(notice)) = &event.kind else { return };
    if !notice.quiet || (notice.title != crate::notice_guard::CONTINUED_TITLE && notice.title != "Notice settings refreshed") {
        return;
    }
    let Ok(snapshot) = serde_json::from_str::<Snapshot>(&notice.message) else { return };
    if let Some(settings) = snapshot.keep_warm_json {
        spec.keep_warm_json = Some(settings);
    }
    let limits = snapshot.notice;
    if let Some(depth) = snapshot.max_subagent_depth {
        spec.max_subagent_depth = depth;
    }
    if let Some(running) = snapshot.max_running_subagents {
        spec.max_running_subagents = running;
    }
    // Old notice snapshots predate native trimming: absence must not undo a
    // current realm override when a session is restored from older history.
    if let Some(enabled) = snapshot.claude_code_trim_enabled {
        spec.claude_code_trim_enabled = Some(enabled);
    }
    if let Some(threshold) = snapshot.claude_code_trim_at {
        spec.claude_code_trim_at = threshold;
    }
    spec.notice_quiet_seconds = limits.notice_quiet_seconds;
    spec.notice_max_wait_seconds = limits.notice_max_wait_seconds;
    spec.notice_no_progress_limit = limits.notice_no_progress_limit;
    spec.notice_failure_relaunch_limit = limits.notice_failure_relaunch_limit;
    spec.notice_tokens_per_hour = limits.notice_tokens_per_hour;
    spec.notice_loop_backoff_rounds = limits.notice_loop_backoff_rounds;
    spec.notice_loop_backoff_max_seconds = limits.notice_loop_backoff_max_seconds;
}

async fn import_in(
    tx: &mut sqlx::SqliteConnection,
    session_id: &str,
    spec: &CreateSession,
    events: &[SessionEvent],
    warm: Option<&KeepWarmSnapshot>,
) -> anyhow::Result<()> {
    // Leftovers of a session whose row was lost (rows written by an
    // earlier build that trimmed live sessions) would collide.
    sqlx::query("DELETE FROM events WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM applied_commands WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM keep_warm_snapshots WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
    if let Some(snapshot) = warm {
        // Validated before taking the write lock. Reuse its serialized transfer
        // copy rather than reserializing the full request inside the transaction.
        sqlx::query(
            "INSERT INTO keep_warm_snapshots (session_id, call_ms, connection_id, generation, ready, data) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(snapshot.last_call_ms)
        .bind(&snapshot.connection_id)
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(snapshot.response.is_some())
        .bind(spec.keep_warm_snapshot_json.as_deref())
        .execute(&mut *tx)
        .await?;
    }
    let now = c6_core::now_ms();
    sqlx::query("INSERT INTO sessions (id, spec, created_at, last_activity) VALUES (?, ?, ?, ?)")
        .bind(session_id)
        .bind(spec.encode_to_vec())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    save_build_folder_place_in(tx, session_id, &BuildFolderPlace::from_spec(spec)).await?;
    // An adoption may replace a blocked incarnation. Import is the only
    // writer allowed to bypass append_in's barrier; its result stays blocked.
    for event in events {
        write_event_in(tx, session_id, event).await?;
    }
    let last = events.last().map(|e| e.seq).unwrap_or(0) as i64;
    sqlx::query("UPDATE sessions SET acked_seq = ?, imported_through = ? WHERE id = ?")
        .bind(last)
        .bind(last)
        .bind(session_id)
        .execute(tx)
        .await?;
    Ok(())
}

async fn append_in(tx: &mut sqlx::SqliteConnection, session_id: &str, event: &SessionEvent) -> anyhow::Result<()> {
    ensure_unblocked(session_id, &ownership_in(tx, session_id).await?)?;
    write_event_in(tx, session_id, event).await
}

async fn write_event_in(tx: &mut sqlx::SqliteConnection, session_id: &str, event: &SessionEvent) -> anyhow::Result<()> {
    if let Some(session_event::Kind::Rewound(r)) = &event.kind {
        sqlx::query("UPDATE claude_rewinds SET applied_seq = ? WHERE session_id = ? AND command_id = ?")
            .bind(event.seq as i64)
            .bind(session_id)
            .bind(&r.command_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO events (session_id, seq, data) VALUES (?, ?, ?)")
        .bind(session_id)
        .bind(event.seq as i64)
        .bind(event.encode_to_vec())
        .execute(&mut *tx)
        .await
        .with_context(|| format!("appending seq {} to {session_id}", event.seq))?;
    if let Some(mut place) = build_folder_place_in(tx, session_id).await?
        && !place.backfill_blocked
        && place.checkpoint_seq + 1 == event.seq as i64
    {
        place.apply(event);
        save_build_folder_place_in(tx, session_id, &place).await?;
    }
    let open_turn: Option<Option<i64>> = match &event.kind {
        Some(session_event::Kind::TurnStarted(t)) => Some(Some(t.turn as i64)),
        Some(session_event::Kind::TurnEnded(_)) => Some(None),
        _ => None,
    };
    let now = c6_core::now_ms();
    sqlx::query("UPDATE build_folder_archives SET last_used_ms = ? WHERE session_id = ?")
        .bind(now)
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    let updated = match open_turn {
        Some(turn) => {
            sqlx::query("UPDATE sessions SET last_seq = ?, open_turn = ?, last_activity = ? WHERE id = ?")
                .bind(event.seq as i64)
                .bind(turn)
                .bind(now)
                .bind(session_id)
                .execute(&mut *tx)
                .await?
        }
        None => {
            sqlx::query("UPDATE sessions SET last_seq = ?, last_activity = ? WHERE id = ?")
                .bind(event.seq as i64)
                .bind(now)
                .bind(session_id)
                .execute(&mut *tx)
                .await?
        }
    };
    // Without its row the event would never be uploaded.
    anyhow::ensure!(updated.rows_affected() == 1, "session {session_id} is not in the journal");
    if let Some(session_event::Kind::BackgroundTask(t)) = &event.kind {
        let delta = if t.state == "started" { 1 } else { -1 };
        sqlx::query("UPDATE sessions SET open_tasks = MAX(open_tasks + ?, 0) WHERE id = ?")
            .bind(delta)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
    }
    let command_id = match &event.kind {
        Some(session_event::Kind::User(u)) => Some(&u.command_id),
        Some(session_event::Kind::Started(s)) => Some(&s.command_id),
        Some(session_event::Kind::Rewound(r)) => Some(&r.command_id),
        Some(session_event::Kind::PlaceChanged(p)) => Some(&p.command_id),
        Some(session_event::Kind::SubagentModelSet(m)) => Some(&m.command_id),
        Some(session_event::Kind::SubagentReported(r)) => Some(&r.command_id),
        _ => None,
    };
    if let Some(id) = command_id.filter(|id| !id.is_empty()) {
        sqlx::query("INSERT OR IGNORE INTO applied_commands (command_id, session_id) VALUES (?, ?)")
            .bind(id)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
impl Journal {
    /// Fail recovery probes until the caller clears the flag.
    /// Unlike the connection-local SHMSIZE trigger, this survives reconnects.
    pub(crate) fn inject_recovery_error(&self, code: i32, flag: Arc<std::sync::atomic::AtomicBool>) {
        *self.recovery_error.lock().unwrap() = Some((code, flag));
    }

    async fn check_injected_recovery_error(&self) -> sqlx::Result<()> {
        let fault = self.recovery_error.lock().unwrap().clone();
        if let Some((code, _)) = fault.filter(|(_, flag)| flag.load(std::sync::atomic::Ordering::SeqCst)) {
            let mut conn = self.pool.acquire().await?;
            Self::install_error_function(&mut conn, c"inject_recovery_error", code).await?;
            sqlx::query("SELECT inject_recovery_error()").execute(&mut *conn).await?;
        }
        Ok(())
    }

    async fn install_error_function(conn: &mut sqlx::SqliteConnection, name: &std::ffi::CStr, code: i32) -> sqlx::Result<()> {
        use std::ffi::{c_char, c_int, c_void};

        // Inject a real extended SQLite result through a connection-local
        // trigger, without manipulating the filesystem or the global VFS.
        unsafe extern "C" {
            fn sqlite3_create_function_v2(
                db: *mut c_void,
                name: *const c_char,
                argc: c_int,
                flags: c_int,
                data: *mut c_void,
                func: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void)>,
                step: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void)>,
                finalizer: Option<unsafe extern "C" fn(*mut c_void)>,
                destroy: Option<unsafe extern "C" fn(*mut c_void)>,
            ) -> c_int;
            fn sqlite3_result_error_code(ctx: *mut c_void, code: c_int);
            fn sqlite3_user_data(ctx: *mut c_void) -> *mut c_void;
        }
        const SQLITE_UTF8: c_int = 1;
        unsafe extern "C" fn error(ctx: *mut c_void, _: c_int, _: *mut *mut c_void) {
            // SAFETY: SQLite owns the boxed code until it destroys the function.
            unsafe { sqlite3_result_error_code(ctx, *sqlite3_user_data(ctx).cast::<c_int>()) };
        }
        unsafe extern "C" fn destroy(data: *mut c_void) {
            // SAFETY: SQLite calls this exactly once for the boxed user data,
            // including when registration fails or the connection closes.
            unsafe { drop(Box::from_raw(data.cast::<c_int>())) };
        }

        let mut handle = conn.lock_handle().await?;
        // SAFETY: SQLx pauses its worker while this handle is locked;
        // SQLite takes ownership of the boxed code through destroy.
        let status = unsafe {
            sqlite3_create_function_v2(
                handle.as_raw_handle().as_ptr().cast(),
                name.as_ptr(),
                0,
                SQLITE_UTF8,
                Box::into_raw(Box::new(code)).cast(),
                Some(error),
                None,
                None,
                Some(destroy),
            )
        };
        assert_eq!(status, 0);
        Ok(())
    }

    /// Matching appends fail with 4874 until this particular SQLite connection closes.
    pub(crate) async fn inject_append_shmsize(&self, marker: &str) {
        self.inject_append_error(marker, 4874).await;
    }

    pub(crate) async fn inject_append_error(&self, marker: &str, code: i32) {
        assert!(!marker.is_empty());
        let mut conn = self.pool.acquire().await.unwrap();
        Self::install_error_function(&mut conn, c"inject_shmsize", code).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TEMP TRIGGER poison_append BEFORE INSERT ON main.events
             WHEN instr(NEW.data, X'{}') > 0 BEGIN SELECT inject_shmsize(); END",
            hex::encode(marker)
        )))
        .execute(&mut *conn)
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod ownership_tests {
    #[tokio::test]
    async fn keep_warm_snapshots_fence_usage_failures_and_move_ownership() {
        use super::{Journal, KeepWarmSnapshot};
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        journal.import("s", &c6_proto::CreateSession::default(), &[]).await.unwrap();
        let mut snapshot = KeepWarmSnapshot {
            response: None,
            request: c6_llm::ModelRequest {
                model: "m".into(),
                system: "prefix".into(),
                messages: vec![c6_llm::Message::user_text("hello")],
                tools: vec![],
                max_output_tokens: None,
                cache_key: Some("s:a".into()),
                effort: None,
                web_search: false,
                json: false,
            },
            connection_id: "a".into(),
            kind: "chatgpt".into(),
            base_url: "http://localhost".into(),
            last_call_ms: 100,
            usage: c6_llm::Usage::default(),
        };
        journal.save_keep_warm_snapshot("s", &snapshot, 0).await.unwrap();
        assert!(journal.keep_warm_call_ms("s").await.unwrap().is_none(), "legacy/incomplete snapshots have no eligible call clock");
        let old = snapshot.clone();
        snapshot.last_call_ms = 200;
        journal.save_keep_warm_snapshot("s", &snapshot, 0).await.unwrap();
        let mut late_usage = old.clone();
        late_usage.usage.input_tokens = 99_999;
        journal.update_keep_warm_usage("s", &late_usage, 0).await.unwrap();
        journal.invalidate_keep_warm_request("s", &old, 0).await.unwrap();
        let current = journal.load_keep_warm_snapshot("s").await.unwrap().unwrap();
        assert_eq!(current.last_call_ms, 200);
        assert_eq!(current.usage.input_tokens, 0);
        journal.clear_keep_warm_snapshot("s", 0).await.unwrap();
        journal.update_keep_warm_usage("s", &snapshot, 0).await.unwrap();
        assert!(journal.load_keep_warm_snapshot("s").await.unwrap().is_none());
        let generation_a = journal.reserve_keep_warm_snapshot("s", &snapshot.connection_id, snapshot.last_call_ms, 0).await.unwrap();
        let generation_b = journal.reserve_keep_warm_snapshot("s", &snapshot.connection_id, snapshot.last_call_ms, 0).await.unwrap();
        assert_ne!(generation_a, generation_b, "same-clock streams still have distinct reservations");
        let data: Option<String> =
            sqlx::query_scalar("SELECT data FROM keep_warm_snapshots WHERE session_id = 's'").fetch_one(&journal.pool).await.unwrap();
        assert!(data.is_none(), "reserving a request never writes its full JSON");
        assert!(journal.load_keep_warm_snapshot("s").await.unwrap().is_none());
        assert!(journal.keep_warm_call_ms("s").await.unwrap().is_none());
        assert!(!journal.complete_keep_warm_snapshot("s", &generation_a, &snapshot, 0).await.unwrap());
        journal.invalidate_keep_warm_generation("s", &generation_a, 0).await.unwrap();
        snapshot.response = Some(c6_llm::Message { role: c6_llm::Role::Assistant, blocks: vec![c6_llm::Block::Text("done".into())] });
        assert!(journal.complete_keep_warm_snapshot("s", &generation_b, &snapshot, 0).await.unwrap());
        assert_eq!(journal.keep_warm_call_ms("s").await.unwrap(), Some(snapshot.last_call_ms));
        journal.clear_keep_warm_snapshot("s", 0).await.unwrap();
        assert!(journal.keep_warm_call_ms("s").await.unwrap().is_none());
        assert!(
            !journal.complete_keep_warm_snapshot("s", &generation_b, &snapshot, 0).await.unwrap(),
            "cleared reservations cannot be resurrected"
        );
        journal.save_keep_warm_snapshot("s", &snapshot, 0).await.unwrap();
        journal.block_ownership("s", 0, "move").await.unwrap();
        assert!(journal.save_keep_warm_snapshot("s", &snapshot, 0).await.is_err());
        assert!(journal.clear_keep_warm_snapshot("s", 0).await.is_err());
        let (spec, _) = journal.load_session("s").await.unwrap().unwrap();
        journal.import_owned("s", &spec, &[], 2, "move").await.unwrap();
        journal.activate_ownership("s", 2, "move").await.unwrap();
        assert_eq!(
            journal.keep_warm_call_ms("s").await.unwrap(),
            Some(snapshot.last_call_ms),
            "move imports retain the conservative request clock"
        );
        assert!(journal.update_keep_warm_usage("s", &snapshot, 0).await.is_err());
        assert!(journal.clear_keep_warm_snapshot("s", 0).await.is_err());
        journal.clear_keep_warm_snapshot("s", 2).await.unwrap();
        // Loading a spec must not resurrect its older embedded transfer copy.
        assert!(journal.load_session("s").await.unwrap().unwrap().0.keep_warm_snapshot_json.is_none());
    }

    #[tokio::test]
    async fn build_folder_projection_preserves_old_moves_and_backfills_legacy_pages_after_reopen() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).await.unwrap();
        for (id, cwd) in [("archived", "/worker/worktrees/repo/archived"), ("active", "/old-workspace")] {
            let started = SessionEvent {
                seq: 1,
                kind: Some(session_event::Kind::Started(c6_proto::SessionStarted { cwd: cwd.into(), ..Default::default() })),
                ..Default::default()
            };
            journal.create_session(id, &CreateSession::default(), &started).await.unwrap();
        }
        journal.set_build_folder_archives("worker", &[("archived".into(), Some(1)), ("active".into(), None)]).await.unwrap();
        journal
            .append(
                "active",
                &SessionEvent {
                    seq: 2,
                    kind: Some(session_event::Kind::PlaceChanged(c6_proto::PlaceChanged {
                        to: Some(c6_proto::SessionPlace { cwd: "/worker/worktrees/repo/archived".into(), ..Default::default() }),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(!partial);
        assert_eq!(places.iter().find(|p| p.session_id == "active").unwrap().path, std::path::Path::new("/worker/worktrees/repo/archived"));
        for seq in 3..=330 {
            journal.append("active", &SessionEvent { seq, ..Default::default() }).await.unwrap();
        }
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(!partial, "new sessions must not lose attribution as conversations grow");
        assert_eq!(places.iter().find(|p| p.session_id == "active").unwrap().path, std::path::Path::new("/worker/worktrees/repo/archived"));
        sqlx::query("DELETE FROM build_folder_places WHERE session_id = 'active'").execute(&journal.pool).await.unwrap();
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(partial, "an incomplete legacy checkpoint must disable cleanup");
        assert!(!places.iter().any(|p| p.session_id == "active"));
        let checkpoint: i64 = sqlx::query_scalar("SELECT checkpoint_seq FROM build_folder_places WHERE session_id = 'active'")
            .fetch_one(&journal.pool)
            .await
            .unwrap();
        assert_eq!(checkpoint, 64, "one investigation must only read a bounded page");
        journal.pool.close().await;
        let journal = Journal::open(&path).await.unwrap();
        for expected in [128, 192, 256, 320] {
            assert!(journal.build_folder_places("worker").await.unwrap().1);
            let checkpoint: i64 = sqlx::query_scalar("SELECT checkpoint_seq FROM build_folder_places WHERE session_id = 'active'")
                .fetch_one(&journal.pool)
                .await
                .unwrap();
            assert_eq!(checkpoint, expected, "backfill must resume instead of rereading recent history");
        }
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(!partial);
        assert_eq!(places.iter().find(|p| p.session_id == "active").unwrap().path, std::path::Path::new("/worker/worktrees/repo/archived"));
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn build_folder_legacy_gap_and_incomplete_trim_never_trust_stale_placement() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        journal
            .create_session(
                "legacy",
                &CreateSession::default(),
                &SessionEvent {
                    seq: 1,
                    kind: Some(session_event::Kind::Started(c6_proto::SessionStarted {
                        cwd: "/old-workspace".into(),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        journal.set_build_folder_archives("worker", &[("legacy".into(), Some(1))]).await.unwrap();
        journal.append("legacy", &SessionEvent { seq: 3, ..Default::default() }).await.unwrap();
        sqlx::query("DELETE FROM build_folder_places WHERE session_id = 'legacy'").execute(&journal.pool).await.unwrap();
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(partial);
        assert!(places.is_empty());
        journal.set_acked("legacy", 3).await.unwrap();
        assert_eq!(journal.trim(i64::MAX, &[]).await.unwrap(), 1);
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(partial, "trimming must not promote an incomplete legacy snapshot");
        assert!(places.is_empty());
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn build_folder_import_projects_current_worker_and_replaces_old_spec_placement() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let spec = CreateSession { cwd: "/spec-workspace".into(), ssh_target: "old-ssh-host".into(), ..Default::default() };
        let mut events = vec![
            SessionEvent {
                seq: 1,
                kind: Some(session_event::Kind::Started(c6_proto::SessionStarted {
                    cwd: "/initial-workspace".into(),
                    ssh_target: "old-ssh-host".into(),
                    ..Default::default()
                })),
                ..Default::default()
            },
            SessionEvent {
                seq: 2,
                kind: Some(session_event::Kind::PlaceChanged(c6_proto::PlaceChanged {
                    to: Some(c6_proto::SessionPlace {
                        cwd: "/current-workspace".into(),
                        repo_root: "/repo".into(),
                        worker_id: "worker".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
                ..Default::default()
            },
        ];
        events.extend((3..=130).map(|seq| SessionEvent { seq, ..Default::default() }));
        journal.import("restored", &spec, &events).await.unwrap();
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(!partial);
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].path, std::path::Path::new("/current-workspace"));
        assert_eq!(places[0].repository, "/repo");
        let (places, partial) = journal.build_folder_places("other-worker").await.unwrap();
        assert!(!partial);
        assert!(places.is_empty(), "foreign placements are not local just because Started was imported");
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn build_folder_unarchived_reference_survives_trim_before_head_refresh() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        journal
            .create_session(
                "unarchived",
                &CreateSession::default(),
                &SessionEvent {
                    seq: 1,
                    kind: Some(session_event::Kind::Started(c6_proto::SessionStarted {
                        cwd: "/shared-workspace".into(),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        journal.set_acked("unarchived", 1).await.unwrap();
        assert_eq!(journal.trim(i64::MAX, &[]).await.unwrap(), 1);
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(!partial);
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].path, std::path::Path::new("/shared-workspace"));
        assert_eq!(places[0].archived_at_ms, None, "missing head archive state must protect shared output");
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn build_folder_archive_placement_survives_trim_and_unarchive() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).await.unwrap();
        let started = SessionEvent {
            seq: 1,
            ts_ms: 10,
            kind: Some(session_event::Kind::Started(c6_proto::SessionStarted {
                cwd: "/worker/worktrees/repo/session".into(),
                repo_root: "/repo".into(),
                ..Default::default()
            })),
        };
        journal.create_session("archived", &CreateSession::default(), &started).await.unwrap();
        journal.set_build_folder_archives("worker", &[("archived".into(), Some(123))]).await.unwrap();
        journal.set_acked("archived", 1).await.unwrap();
        assert_eq!(journal.trim(i64::MAX, &[]).await.unwrap(), 1);
        let (places, partial) = journal.build_folder_places("worker").await.unwrap();
        assert!(!partial);
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].path, std::path::Path::new("/worker/worktrees/repo/session"));
        assert_eq!(places[0].archived_at_ms, Some(123));
        journal.pool.close().await;
        let journal = Journal::open(&path).await.unwrap();
        assert_eq!(journal.build_folder_places("worker").await.unwrap().0[0].archived_at_ms, Some(123));
        journal.set_build_folder_archives("worker", &[("archived".into(), None)]).await.unwrap();
        let places = journal.build_folder_places("worker").await.unwrap().0;
        assert_eq!(places[0].archived_at_ms, None);
        assert_eq!(places[0].path, std::path::Path::new("/worker/worktrees/repo/session"));
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn build_folder_active_sessions_include_unknown_launches_and_held_roots() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let owner = journal.shell_owner("shell-session", "shell-session");
        let reserved = owner.reserve("task").await.unwrap();
        journal.track_claude_process("held-session", 123).await.unwrap();
        let mut active = journal.build_folder_active_sessions().await.unwrap();
        active.sort();
        assert_eq!(active, ["held-session", "shell-session"]);
        owner.not_launched(&reserved).await.unwrap();
        journal.untrack_claude_process("held-session", 123).await.unwrap();
        assert!(journal.build_folder_active_sessions().await.unwrap().is_empty());
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn machine_roots_include_shells_and_retained_claude_once() {
        let dir = tempfile::tempdir().unwrap();
        let journal = super::Journal::open(&dir.path().join("journal.db")).await.unwrap();
        journal.track_claude_process("held-session", 123).await.unwrap();
        journal.track_claude_process("held-session", 123).await.unwrap();
        // The ownership table is authoritative even between actor turns.
        sqlx::query("INSERT INTO shell_writers (writer_id, session_id, root_session_id, task_id, pgid) VALUES ('writer', 'shell-session', 'shell-session', 'task', 456)")
            .execute(&journal.pool).await.unwrap();
        sqlx::query("INSERT INTO shell_writers (writer_id, session_id, root_session_id, task_id, pgid, absent) VALUES ('retired', 'old-session', 'old-session', 'old-task', 789, 1)")
            .execute(&journal.pool).await.unwrap();
        let mut roots = journal.machine_process_roots().await.unwrap();
        roots.sort();
        assert_eq!(roots, vec![(123, "held-session".into()), (456, "shell-session".into())]);
    }
    use super::*;

    fn event(seq: u64) -> SessionEvent {
        SessionEvent { seq, ts_ms: 0, kind: Some(session_event::Kind::Started(c6_proto::SessionStarted::default())) }
    }

    #[tokio::test]
    async fn next_ack_and_append_after_shmsize_do_not_replay_failed_append() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        journal.create_session("s", &CreateSession::default(), &event(1)).await.unwrap();
        journal.inject_append_shmsize("uncertain").await;

        let failed = SessionEvent {
            kind: Some(session_event::Kind::User(c6_proto::UserMessage { command_id: "uncertain".into(), ..Default::default() })),
            ..event(2)
        };
        let err = journal.append("s", &failed).await.unwrap_err();
        let sqlx = err.downcast_ref::<sqlx::Error>().expect("SQLite error");
        assert_eq!(sqlx.as_database_error().and_then(|err| err.code()).as_deref(), Some("4874"));
        // SQLite rolled back automatically. The next checkout must discard
        // SQLx's stale depth, without waiting for actor recovery to run.
        journal.set_acked("s", 1).await.unwrap();
        assert!(journal.unacked().await.unwrap().is_empty());
        assert_eq!(journal.events_after("s", 0, 10).await.unwrap(), [event(1)]);
        assert!(!journal.is_applied("uncertain").await.unwrap());
        // The replacement still matches the fault trigger but is a new
        // caller decision, not a replay of the earlier write.
        let replacement = SessionEvent {
            kind: Some(session_event::Kind::User(c6_proto::UserMessage {
                command_id: "replacement".into(),
                text: "supersedes the uncertain append".into(),
                ..Default::default()
            })),
            ..event(2)
        };
        journal.append("s", &replacement).await.unwrap();
        assert_eq!(journal.unacked().await.unwrap()[0].acked_seq, 1);

        // Also exercise append as the very next operation after a failed
        // write, rather than allowing an ack/read to repair the connection.
        journal.inject_append_shmsize("uncertain").await;
        let failed = SessionEvent { seq: 3, ..failed };
        let err = journal.append("s", &failed).await.unwrap_err();
        let err = err.downcast_ref::<sqlx::Error>().unwrap();
        assert_eq!(err.as_database_error().and_then(|err| err.code()).as_deref(), Some("4874"));
        let next = SessionEvent { seq: 3, ..replacement.clone() };
        journal.append("s", &next).await.unwrap();
        journal.recover_connection().await.unwrap();
        assert_eq!(journal.events_after("s", 0, 10).await.unwrap(), [event(1), replacement, next]);
        assert!(!journal.is_applied("uncertain").await.unwrap());
    }

    #[tokio::test]
    async fn injected_recovery_errors_persist_until_cleared() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        for code in [11, 26, 13, 4874] {
            let flag = Arc::new(AtomicBool::new(true));
            journal.inject_recovery_error(code, flag.clone());
            for _ in 0..2 {
                let err = journal.recover_connection().await.unwrap_err();
                let err = err.downcast_ref::<sqlx::Error>().unwrap();
                assert_eq!(err.as_database_error().and_then(|err| err.code()).as_deref(), Some(code.to_string().as_str()));
            }
            flag.store(false, Ordering::SeqCst);
            journal.recover_connection().await.unwrap();
        }
    }

    #[tokio::test]
    async fn running_session_recovers_notice_after_shmsize() {
        use c6_proto::{DelegationEnded, ModelSpec, SessionCommand, TurnEndReason, UserInput, session_command};

        struct MockHead(String);

        #[async_trait::async_trait]
        impl crate::link::LocalHead for MockHead {
            async fn call(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value, String> {
                Err("not needed by this test".into())
            }

            async fn credential(&self, connection_id: &str) -> c6_proto::CredentialGrant {
                c6_proto::CredentialGrant {
                    connection_id: connection_id.into(),
                    kind: "openai_compat".into(),
                    base_url: self.0.clone(),
                    ..Default::default()
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mock = c6_testkit::MockLlm::start().await;
        let gate = mock.push_gated(c6_testkit::Reply::Text("The first turn stays in flight.".into()));
        for _ in 0..2 {
            mock.push(c6_testkit::Reply::Text("Recovery completed.".into()));
        }
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let model =
            ModelSpec { connection_id: "mock".into(), connection_kind: "openai_compat".into(), model: "m".into(), ..Default::default() };
        let cwd = dir.path().display().to_string();
        let spec = CreateSession { model: Some(model.clone()), cwd: cwd.clone(), ..Default::default() };
        let started = c6_proto::SessionStarted { model: Some(model), cwd, tool_profile: "generic".into(), ..Default::default() };
        journal.import("s", &spec, &[SessionEvent { kind: Some(session_event::Kind::Started(started)), ..event(1) }]).await.unwrap();
        let link = crate::link::HeadLink::local(Arc::new(MockHead(mock.base_url())));
        let manager = crate::session::SessionManager::new(journal.clone(), Arc::new(link), dir.path().join("worktrees"));
        let command =
            |id: &str, kind| SessionCommand { session_id: "s".into(), command_id: id.into(), ownership_epoch: 0, kind: Some(kind) };
        manager
            .handle(command(
                "running-input",
                session_command::Kind::Input(UserInput { text: "Start the gated turn.".into(), human_origin: true, ..Default::default() }),
            ))
            .await
            .unwrap();
        mock.request_headers(0).await;

        let marker = "retained-notice-after-shmsize";
        journal.inject_append_shmsize(marker).await;
        let failure = manager
            .handle(command(
                "report",
                session_command::Kind::DelegationEnded(DelegationEnded {
                    child_session_id: "test-child".into(),
                    report: marker.into(),
                    wake: true,
                    reason: TurnEndReason::Completed as i32,
                    ..Default::default()
                }),
            ))
            .await
            .unwrap_err();
        assert!(failure.contains("4874"), "{failure}");
        let is_notice = |event: &SessionEvent| matches!(&event.kind, Some(session_event::Kind::User(user)) if user.command_id == "subagent:report" && user.text.contains(marker));
        let events = c6_testkit::eventually("the faulted notice to reach the model and complete", async || {
            let events = journal.events_after("s", 0, u32::MAX).await.ok()?;
            let index = events.iter().position(is_notice)?;
            let completed = events[index + 1..]
                .iter()
                .any(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(end)) if end.reason == TurnEndReason::Completed as i32));
            (completed && mock.requests().iter().any(|request| request.to_string().contains(marker))).then_some(events)
        })
        .await;
        assert_eq!(events.iter().filter(|e| is_notice(e)).count(), 1);
        gate.notify_one();
    }

    #[tokio::test]
    async fn epoch_replacement_fences_stale_writers_cursors_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let spec = CreateSession::default();
        journal.create_session("s", &spec, &event(1)).await.unwrap();
        assert!(journal.import_owned("s", &spec, &[event(1)], 1, "return").await.is_err());
        journal.block_ownership("s", 0, "away").await.unwrap();
        assert_eq!(journal.unacked().await.unwrap().len(), 1);
        assert_eq!(journal.events_after_owned("s", 0, 10, 0).await.unwrap(), [event(1)]);
        journal.set_acked_owned("s", 1, 0).await.unwrap();
        assert_eq!(journal.trim(i64::MAX, &[]).await.unwrap(), 0);
        assert!(journal.import_owned("s", &spec, &[event(1), event(1)], 1, "return").await.is_err());
        assert_eq!(journal.ownership("s").await.unwrap(), (0, true, "away".into()));
        assert_eq!(journal.events_after_owned("s", 0, 10, 0).await.unwrap(), [event(1)]);
        journal.import_owned("s", &spec, &[event(1), event(2)], 1, "return").await.unwrap();
        assert!(journal.append_owned("s", &event(3), 1).await.is_err());
        journal.activate_ownership("s", 1, "return").await.unwrap();
        assert!(journal.append_owned("s", &event(3), 0).await.is_err());
        journal.append_owned("s", &event(3), 1).await.unwrap();
        assert!(journal.set_acked_owned("s", 3, 0).await.is_err());
        assert!(journal.events_after_owned("s", 0, 10, 0).await.is_err());
        assert_eq!(journal.unacked().await.unwrap(), [Cursor { session_id: "s".into(), epoch: 1, last_seq: 3, acked_seq: 2 }]);
    }

    #[tokio::test]
    async fn block_survives_restart_until_exact_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Arc::new(Journal::open(&path).await.unwrap());
        journal.create_session("root", &CreateSession::default(), &event(1)).await.unwrap();
        let shell = journal.shell_owner("child", "root");
        let reservation = shell.reserve("task").await.unwrap();
        assert!(journal.block_ownership("root", 0, "move").await.unwrap_err().to_string().contains("unconfirmed shell writer"));
        shell.not_launched(&reservation).await.unwrap();
        journal.block_ownership("root", 0, "move").await.unwrap();
        journal.pool.close().await;
        let journal = Arc::new(Journal::open(&path).await.unwrap());
        assert_eq!(journal.ownership("root").await.unwrap(), (0, true, "move".into()));
        assert!(journal.check_ownership("root", 0).await.is_err());
        assert!(journal.append("root", &event(2)).await.is_err());
        assert!(journal.shell_owner("child", "root").reserve("task").await.is_err());
        assert_eq!(journal.recent_session_ids(i64::MIN).await.unwrap(), ["root"]);
        assert!(journal.recent_unblocked_session_ids(i64::MIN).await.unwrap().is_empty());
        assert!(journal.unblock_ownership("root", 0, "other").await.is_err());
        journal.block_ownership("root", 0, "move").await.unwrap();
        journal.unblock_ownership("root", 0, "move").await.unwrap();
        journal.unblock_ownership("root", 0, "move").await.unwrap();
        assert!(journal.move_aborted("root", 0, "move").await.unwrap());
        journal.append("root", &event(2)).await.unwrap();
    }

    #[tokio::test]
    async fn abort_before_freeze_survives_restart_and_later_moves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).await.unwrap();
        journal.create_session("s", &CreateSession::default(), &event(1)).await.unwrap();
        journal.abort_source_move("s", 0, "late").await.unwrap();
        journal.pool.close().await;

        let journal = Journal::open(&path).await.unwrap();
        assert!(journal.move_aborted("s", 0, "late").await.unwrap());
        assert!(journal.block_ownership("s", 0, "late").await.is_err());
        journal.check_ownership("s", 0).await.unwrap();
        journal.block_ownership("s", 0, "next").await.unwrap();
        assert!(journal.abort_source_move("s", 0, "wrong").await.is_err());
        journal.abort_source_move("s", 0, "next").await.unwrap();
        assert!(journal.block_ownership("s", 0, "late").await.is_err());
        journal.check_ownership("s", 0).await.unwrap();
        journal.append("s", &event(2)).await.unwrap();
    }

    #[tokio::test]
    async fn aborted_target_refuses_late_prepare_and_activation_but_allows_new_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let spec = CreateSession::default();
        let journal = Journal::open(&path).await.unwrap();
        journal.abort_target_move("s", 1, "late").await.unwrap();
        journal.pool.close().await;

        let journal = Journal::open(&path).await.unwrap();
        assert!(journal.import_owned("s", &spec, &[event(1)], 1, "late").await.is_err());
        journal.import_owned("s", &spec, &[event(1)], 2, "prepared").await.unwrap();
        journal.abort_target_move("s", 2, "prepared").await.unwrap();
        assert!(journal.import_owned("s", &spec, &[event(1)], 2, "prepared").await.is_err());
        assert!(journal.activate_ownership("s", 2, "prepared").await.is_err());
        assert_eq!(journal.ownership("s").await.unwrap(), (2, true, "prepared".into()));
        journal.import_owned("s", &spec, &[event(1)], 3, "next").await.unwrap();
        journal.activate_ownership("s", 3, "next").await.unwrap();
        assert!(journal.abort_target_move("s", 3, "next").await.is_err());
        assert!(journal.abort_source_move("s", 2, "prepared").await.is_err());
        journal.check_ownership("s", 3).await.unwrap();
    }

    #[tokio::test]
    async fn stale_native_writes_cannot_change_new_incarnation() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let boundary = crate::claude_code::Boundary::default();
        let reset = crate::claude_code::Reset::default();
        journal.import_owned("s", &CreateSession::default(), &[event(1)], 3, "move").await.unwrap();
        assert!(journal.track_claude_process_owned("s", 42, 3).await.is_err());
        journal.activate_ownership("s", 3, "move").await.unwrap();
        assert!(journal.track_claude_process_owned("s", 42, 0).await.is_err());
        assert!(journal.record_claude_turn_owned("s", 1, &boundary, 0).await.is_err());
        assert!(journal.prepare_claude_rewind_owned("s", &reset, 0).await.is_err());
        assert!(journal.authorize_claude_rewind_retry_owned("s", "", "retry", 0).await.is_err());
        assert!(journal.claude_processes("s").await.unwrap().is_empty());
        assert!(journal.claude_turn("s", 1).await.unwrap().is_none());
        assert!(journal.claude_rewind("s").await.unwrap().is_none());
        journal.record_claude_turn_owned("s", 1, &boundary, 3).await.unwrap();
        journal.prepare_claude_rewind_owned("s", &reset, 3).await.unwrap();
        assert!(journal.authorize_claude_rewind_retry_owned("s", "", "retry", 3).await.unwrap());
    }

    #[tokio::test]
    async fn adoption_replay_preserves_history_and_activation() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let spec = CreateSession::default();
        assert_eq!(journal.ownership("s").await.unwrap(), (0, false, String::new()));
        journal.import_owned("s", &spec, &[event(1)], 4, "move").await.unwrap();
        journal.import_owned("s", &spec, &[event(1)], 4, "move").await.unwrap();
        assert_eq!(journal.ownership("s").await.unwrap(), (4, true, "move".into()));
        assert!(journal.load_session_owned("s").await.is_err());
        assert_eq!(journal.load_session("s").await.unwrap().unwrap().1, [event(1)]);
        assert!(journal.activate_ownership("s", 3, "move").await.is_err());
        journal.activate_ownership("s", 4, "move").await.unwrap();
        journal.activate_ownership("s", 4, "move").await.unwrap();
        journal.append_owned("s", &event(2), 4).await.unwrap();
        journal.import_owned("s", &spec, &[event(1)], 4, "move").await.unwrap();
        assert!(journal.import_owned("s", &spec, &[event(1)], 4, "other").await.is_err());
        assert_eq!(journal.ownership("s").await.unwrap(), (4, false, "move".into()));
        assert_eq!(journal.events_after_owned("s", 0, 10, 4).await.unwrap(), [event(1), event(2)]);
        assert_eq!(journal.load_session_owned("s").await.unwrap().unwrap(), (spec, vec![event(1), event(2)], 4));
    }

    #[test]
    fn native_trim_settings_survive_snapshots_without_resetting_legacy_overrides() {
        let mut spec = CreateSession { claude_code_trim_enabled: Some(false), claude_code_trim_at: 450_000, ..Default::default() };
        let snapshot = |message: serde_json::Value| SessionEvent {
            kind: Some(session_event::Kind::Notice(c6_proto::Notice {
                title: "Notice settings refreshed".into(),
                message: message.to_string(),
                quiet: true,
                ..Default::default()
            })),
            ..Default::default()
        };
        apply_notice_settings_snapshot(&mut spec, &snapshot(serde_json::json!({"notice_quiet_seconds": 9})));
        assert_eq!((spec.claude_code_trim_enabled, spec.claude_code_trim_at), (Some(false), 450_000));
        apply_notice_settings_snapshot(
            &mut spec,
            &snapshot(serde_json::json!({
                "claude_code_trim_enabled": true, "claude_code_trim_at": 0,
            })),
        );
        assert_eq!((spec.claude_code_trim_enabled, spec.claude_code_trim_at), (Some(true), 0));
    }

    #[tokio::test]
    async fn keep_warm_settings_refresh_survives_restore_and_legacy_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).await.unwrap();
        let original = r#"{"enabled":true}"#;
        let changed = r#"{"enabled":false,"disabled_connections":["local"]}"#;
        let spec = CreateSession { keep_warm_json: Some(original.into()), ..Default::default() };
        journal.create_session("s", &spec, &event(1)).await.unwrap();
        let snapshot = |seq, settings: Option<&str>| SessionEvent {
            seq,
            ts_ms: seq as i64,
            kind: Some(session_event::Kind::Notice(c6_proto::Notice {
                title: "Notice settings refreshed".into(),
                message: serde_json::json!({"keep_warm_json": settings}).to_string(),
                quiet: true,
                ..Default::default()
            })),
        };
        journal.append("s", &snapshot(2, Some(changed))).await.unwrap();
        // Older heads' notices must not reset a policy selected by a newer head.
        journal.append("s", &snapshot(3, None)).await.unwrap();
        drop(journal);
        let journal = Journal::open(&path).await.unwrap();
        let (mut loaded, _) = journal.load_session("s").await.unwrap().unwrap();
        assert_eq!(loaded.keep_warm_json.as_deref(), Some(changed));
        let mut legacy = snapshot(4, None);
        let Some(session_event::Kind::Notice(notice)) = &mut legacy.kind else { unreachable!() };
        notice.message = "{}".into();
        apply_notice_settings_snapshot(&mut loaded, &legacy);
        assert_eq!(loaded.keep_warm_json.as_deref(), Some(changed));
    }

    #[tokio::test]
    async fn imported_notice_limits_win_over_history_but_not_new_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path).await.unwrap();
        let refresh = "Notice settings refreshed";
        let continued = crate::notice_guard::CONTINUED_TITLE;
        let snapshot = |seq, title: &str, quiet_seconds, depth, count| SessionEvent {
            kind: Some(session_event::Kind::Notice(c6_proto::Notice {
                title: title.into(),
                quiet: true,
                message: serde_json::json!({
                    "notice_quiet_seconds": quiet_seconds,
                    "max_subagent_depth": depth,
                    "max_running_subagents": count,
                })
                .to_string(),
                ..Default::default()
            })),
            ..event(seq)
        };
        let history = [event(1), snapshot(2, refresh, 3, 4, 5), snapshot(3, continued, 6, 7, 8)];
        let spec = CreateSession { notice_quiet_seconds: 19, max_subagent_depth: 2, max_running_subagents: 0, ..Default::default() };
        // Older move exporters send the original spec, not the effective
        // settings. The destination folds its snapshots before importing.
        let mut exported = CreateSession::default();
        for event in &history {
            apply_notice_settings_snapshot(&mut exported, event);
        }
        journal.import_owned("old-export", &exported, &history, 2, "old-move").await.unwrap();
        journal.activate_ownership("old-export", 2, "old-move").await.unwrap();
        let (loaded, _, _) = journal.load_session_owned("old-export").await.unwrap().unwrap();
        assert_eq!((loaded.notice_quiet_seconds, loaded.max_subagent_depth, loaded.max_running_subagents), (6, 7, 8));
        journal.import("restored", &spec, &history).await.unwrap();
        journal.import_owned("moved", &spec, &history, 4, "move").await.unwrap();
        journal.activate_ownership("moved", 4, "move").await.unwrap();
        assert!(journal.unacked().await.unwrap().is_empty());
        drop(journal);

        let journal = Journal::open(&path).await.unwrap();
        for (id, epoch) in [("restored", 0), ("moved", 4)] {
            assert_eq!(journal.load_session_owned(id).await.unwrap().unwrap(), (spec.clone(), history.to_vec(), epoch));
            let later = snapshot(4, refresh, 11, 3, 0);
            journal.append_owned(id, &later, epoch).await.unwrap();
            if epoch != 0 {
                // Retrying adoption must preserve the current settings and event tail.
                journal.import_owned(id, &spec, &history, epoch, "move").await.unwrap();
            }
            journal.set_acked_owned(id, later.seq, epoch).await.unwrap();
            let (loaded, events) = journal.load_session(id).await.unwrap().unwrap();
            assert_eq!((loaded.notice_quiet_seconds, loaded.max_subagent_depth, loaded.max_running_subagents), (11, 3, 0));
            assert_eq!(events, [history.to_vec(), vec![later]].concat());
        }
    }

    #[tokio::test]
    async fn known_move_folders_includes_resolved_scratch_and_later_worktree_placements() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let scratch = dir.path().join("scratch");
        let checkout = dir.path().join("falcon");
        let linked = dir.path().join("worktrees/falcon/topic/src");
        let started = SessionEvent {
            kind: Some(session_event::Kind::Started(c6_proto::SessionStarted { cwd: scratch.display().to_string(), ..Default::default() })),
            ..event(1)
        };
        // A scratch session's creation spec deliberately has no cwd. Moving
        // it later updates placement events, not that original saved spec.
        journal.create_session("s", &CreateSession::default(), &started).await.unwrap();
        let place = SessionEvent {
            kind: Some(session_event::Kind::PlaceChanged(c6_proto::PlaceChanged {
                command_id: "place".into(),
                from: Some(c6_proto::SessionPlace { cwd: scratch.display().to_string(), ..Default::default() }),
                to: Some(c6_proto::SessionPlace {
                    cwd: linked.display().to_string(),
                    repo_root: checkout.display().to_string(),
                    worktree_branch: "topic".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..event(2)
        };
        journal.append("s", &place).await.unwrap();
        let folders = journal.known_move_folders("local-worker").await.unwrap();
        for expected in [&scratch, &checkout, &linked] {
            assert!(folders.contains(expected), "missing {} in {folders:?}", expected.display());
        }
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn known_move_folders_excludes_foreign_started_and_legacy_places_in_imported_history() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let started = SessionEvent {
            kind: Some(session_event::Kind::Started(c6_proto::SessionStarted { cwd: "/foreign/scratch".into(), ..Default::default() })),
            ..event(1)
        };
        let legacy = SessionEvent {
            kind: Some(session_event::Kind::PlaceChanged(c6_proto::PlaceChanged {
                from: Some(c6_proto::SessionPlace { cwd: "/foreign/scratch".into(), ..Default::default() }),
                to: Some(c6_proto::SessionPlace {
                    cwd: "/foreign/checkout".into(),
                    repo_root: "/foreign/primary".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..event(2)
        };
        let moved = SessionEvent {
            kind: Some(session_event::Kind::PlaceChanged(c6_proto::PlaceChanged {
                from: Some(c6_proto::SessionPlace { cwd: "/foreign/checkout".into(), worker_id: "foreign".into(), ..Default::default() }),
                to: Some(c6_proto::SessionPlace {
                    cwd: "/local/linked".into(),
                    repo_root: "/local/primary".into(),
                    worker_id: "local".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..event(3)
        };
        let spec = CreateSession { cwd: "/local/linked".into(), ..Default::default() };
        journal.import("s", &spec, &[started, legacy, moved]).await.unwrap();
        let folders = journal.known_move_folders("local").await.unwrap();
        assert_eq!(folders, [std::path::PathBuf::from("/local/linked"), std::path::PathBuf::from("/local/primary")]);
        journal.pool.close().await;
    }

    #[tokio::test]
    async fn block_refuses_open_turn_and_pending_rewind() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
        journal.create_session("s", &CreateSession::default(), &event(1)).await.unwrap();
        let started = SessionEvent { kind: Some(session_event::Kind::TurnStarted(c6_proto::TurnStarted::default())), ..event(2) };
        journal.append("s", &started).await.unwrap();
        assert!(journal.block_ownership("s", 0, "move").await.unwrap_err().to_string().contains("open turn"));
        let ended = SessionEvent { kind: Some(session_event::Kind::TurnEnded(c6_proto::TurnEnded::default())), ..event(3) };
        journal.append("s", &ended).await.unwrap();
        journal.prepare_claude_rewind("s", &crate::claude_code::Reset::default()).await.unwrap();
        assert!(journal.block_ownership("s", 0, "move").await.unwrap_err().to_string().contains("pending rewind"));
        journal.check_ownership("s", 0).await.unwrap();
    }
}
