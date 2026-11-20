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
use prost::Message;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, Sqlite, Transaction};
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
    /// Serialize reset publication/retry authorization with exceptional
    /// metadata-loss cleanup. Acquire before any SQLite transaction; cleanup
    /// holds no database transaction while awaiting process-group absence.
    /// The worker's manager and actors share this Journal through one Arc.
    pub(crate) claude_rewind_ownership: tokio::sync::Mutex<()>,
    /// Signalled after every append. Single consumer (the uploader), so
    /// `notify_one` keeps a permit when it is busy and nothing is missed.
    pub appended: Notify,
}

struct ShellOwner {
    journal: Arc<Journal>,
    session_id: String,
    root_session_id: String,
}

#[async_trait::async_trait]
impl c6_tools::ShellOwner for ShellOwner {
    async fn reserve(&self, task_id: &str) -> std::io::Result<String> {
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
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations").run(&pool).await.context("journal migrations")?;
        Ok(Self { pool, claude_rewind_ownership: tokio::sync::Mutex::new(()), appended: Notify::new() })
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

    pub(crate) async fn known_move_folders(&self) -> anyhow::Result<Vec<std::path::PathBuf>> {
        let specs: Vec<Vec<u8>> = sqlx::query_scalar("SELECT spec FROM sessions LIMIT 4096").fetch_all(&self.pool).await?;
        Ok(specs
            .into_iter()
            .filter_map(|data| CreateSession::decode(data.as_slice()).ok())
            .filter(|spec| !spec.cwd.is_empty())
            .map(|spec| std::path::PathBuf::from(spec.cwd))
            .collect())
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
        let mut tx = self.begin_write().await?;
        check_ownership_in(&mut tx, session_id, 0).await?;
        import_in(&mut tx, session_id, spec, events).await?;
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
        import_in(&mut tx, session_id, spec, events).await?;
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
            sqlx::query("DELETE FROM events WHERE session_id = ?").bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM applied_commands WHERE session_id = ?").bind(&id).execute(&mut *tx).await?;
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
    let Some(row) = sqlx::query("SELECT spec FROM sessions WHERE id = ?").bind(session_id).fetch_optional(&mut *tx).await? else {
        return Ok(None);
    };
    let spec = CreateSession::decode(row.get::<Vec<u8>, _>("spec").as_slice())?;
    let events = events_after_in(tx, session_id, 0, u32::MAX).await?;
    Ok(Some((spec, events)))
}

async fn import_in(tx: &mut sqlx::SqliteConnection, session_id: &str, spec: &CreateSession, events: &[SessionEvent]) -> anyhow::Result<()> {
    // Leftovers of a session whose row was lost (rows written by an
    // earlier build that trimmed live sessions) would collide.
    sqlx::query("DELETE FROM events WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM applied_commands WHERE session_id = ?").bind(session_id).execute(&mut *tx).await?;
    let now = c6_core::now_ms();
    sqlx::query("INSERT INTO sessions (id, spec, created_at, last_activity) VALUES (?, ?, ?, ?)")
        .bind(session_id)
        .bind(spec.encode_to_vec())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    // An adoption may replace a blocked incarnation. Import is the only
    // writer allowed to bypass append_in's barrier; its result stays blocked.
    for event in events {
        write_event_in(tx, session_id, event).await?;
    }
    let last = events.last().map(|e| e.seq).unwrap_or(0) as i64;
    sqlx::query("UPDATE sessions SET acked_seq = ? WHERE id = ?").bind(last).bind(session_id).execute(tx).await?;
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
    let open_turn: Option<Option<i64>> = match &event.kind {
        Some(session_event::Kind::TurnStarted(t)) => Some(Some(t.turn as i64)),
        Some(session_event::Kind::TurnEnded(_)) => Some(None),
        _ => None,
    };
    let now = c6_core::now_ms();
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
mod ownership_tests {
    use super::*;

    fn event(seq: u64) -> SessionEvent {
        SessionEvent { seq, ts_ms: 0, kind: Some(session_event::Kind::Started(c6_proto::SessionStarted::default())) }
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
