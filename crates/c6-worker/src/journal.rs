//! Local durable journal of session events (SQLite, WAL).
//!
//! Every durable event is committed here before it is sent to the head, so a
//! disconnect or crash never loses one. It is a cache: the head's copy is
//! authoritative once acknowledged.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use c6_proto::{CreateSession, SessionEvent, session_event};
use prost::Message;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};
use tokio::sync::Notify;

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
        // One write statement serializes admission with prepare_claude_rewind.
        // A launch either owns a durable unknown barrier before reset closes
        // admission, or cannot create a child at all (including descendants).
        let inserted = sqlx::query(
            "INSERT INTO shell_writers (writer_id, session_id, root_session_id, task_id)
             SELECT ?, ?, ?, ? WHERE NOT EXISTS (
                 SELECT 1 FROM claude_rewinds WHERE session_id IN (?, ?) AND applied_seq = 0
             )",
        )
        .bind(&id)
        .bind(&self.session_id)
        .bind(&self.root_session_id)
        .bind(task_id)
        .bind(&self.session_id)
        .bind(&self.root_session_id)
        .execute(&self.journal.pool)
        .await
        .map_err(std::io::Error::other)?;
        if inserted.rows_affected() != 1 {
            return Err(std::io::Error::other("Shell launch refused while Rewind awaits confirmation"));
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
                anyhow::bail!("Background shell {task} has unknown writer identity (incomplete launch or SSH); cannot confirm it stopped");
            };
            anyhow::ensure!(
                nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None) == Err(nix::errno::Errno::ESRCH),
                "Background shell {task} process group {pgid} has not confirmed that it stopped"
            );
            sqlx::query("UPDATE shell_writers SET absent = 1 WHERE writer_id = ?")
                .bind(row.get::<String, _>("writer_id"))
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn track_claude_process(&self, session_id: &str, pgid: i32) -> anyhow::Result<()> {
        anyhow::ensure!(pgid > 0, "Claude Code process group is unknown");
        sqlx::query("INSERT OR IGNORE INTO claude_processes (session_id, pgid) VALUES (?, ?)")
            .bind(session_id)
            .bind(pgid)
            .execute(&self.pool)
            .await?;
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
                let pgid = i32::try_from(pgid)?;
                anyhow::ensure!(pgid > 0, "Claude Code process group is unknown");
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
        sqlx::query("INSERT OR REPLACE INTO claude_turns (session_id, seq, data) VALUES (?, ?, ?)")
            .bind(session_id)
            .bind(seq as i64)
            .bind(serde_json::to_string(boundary)?)
            .execute(&self.pool)
            .await?;
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
        let _ownership = self.claude_rewind_ownership.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
        let _ownership = self.claude_rewind_ownership.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
        let Some(session_event::Kind::Rewound(rewound)) = &event.kind else { anyhow::bail!("Expected a rewind confirmation") };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
            // FULL: an event must survive power loss once appended.
            .synchronous(SqliteSynchronous::Full);
        let pool =
            SqlitePoolOptions::new().max_connections(4).connect_with(opts).await.with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations").run(&pool).await.context("journal migrations")?;
        Ok(Self { pool, claude_rewind_ownership: tokio::sync::Mutex::new(()), appended: Notify::new() })
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
        let mut tx = self.pool.begin().await?;
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
        let mut tx = self.pool.begin().await?;
        append_in(&mut tx, session_id, event).await?;
        tx.commit().await?;
        self.appended.notify_one();
        Ok(())
    }

    pub async fn load_session(&self, session_id: &str) -> anyhow::Result<Option<(CreateSession, Vec<SessionEvent>)>> {
        let Some(row) = sqlx::query("SELECT spec FROM sessions WHERE id = ?").bind(session_id).fetch_optional(&self.pool).await? else {
            return Ok(None);
        };
        let spec = CreateSession::decode(row.get::<Vec<u8>, _>("spec").as_slice())?;
        let events = self.events_after(session_id, 0, u32::MAX).await?;
        Ok(Some((spec, events)))
    }

    pub async fn events_after(&self, session_id: &str, after: u64, limit: u32) -> anyhow::Result<Vec<SessionEvent>> {
        let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? AND seq > ? ORDER BY seq LIMIT ?")
            .bind(session_id)
            .bind(after as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|r| Ok(SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?)).collect()
    }

    /// Sessions with events the head has not acknowledged.
    pub async fn unacked(&self) -> anyhow::Result<Vec<Cursor>> {
        let rows = sqlx::query("SELECT id, last_seq, acked_seq FROM sessions WHERE last_seq > acked_seq").fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| Cursor {
                session_id: r.get("id"),
                last_seq: r.get::<i64, _>("last_seq") as u64,
                acked_seq: r.get::<i64, _>("acked_seq") as u64,
            })
            .collect())
    }

    pub async fn set_acked(&self, session_id: &str, seq: u64) -> anyhow::Result<()> {
        sqlx::query("UPDATE sessions SET acked_seq = MAX(acked_seq, MIN(?, last_seq)) WHERE id = ?")
            .bind(seq as i64)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Imports a session from the head's log (see `Restore`). Everything is
    /// already acknowledged.
    pub async fn import(&self, session_id: &str, spec: &CreateSession, events: &[SessionEvent]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
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
        for ev in events {
            append_in(&mut tx, session_id, ev).await?;
        }
        let last = events.last().map(|e| e.seq).unwrap_or(0) as i64;
        sqlx::query("UPDATE sessions SET acked_seq = ? WHERE id = ?").bind(last).bind(session_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drops sessions that are fully synced, not mid-turn, and idle since
    /// `before_ms` (except `keep`). The head can restore them on demand.
    pub async fn trim(&self, before_ms: i64, keep: &[String]) -> anyhow::Result<usize> {
        let rows = sqlx::query("SELECT id FROM sessions WHERE acked_seq = last_seq AND open_turn IS NULL AND last_activity < ?")
            .bind(before_ms)
            .fetch_all(&self.pool)
            .await?;
        let mut n = 0;
        for r in rows {
            let id: String = r.get("id");
            if keep.contains(&id) {
                continue;
            }
            let mut tx = self.pool.begin().await?;
            sqlx::query("DELETE FROM events WHERE session_id = ?").bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM applied_commands WHERE session_id = ?").bind(&id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM sessions WHERE id = ?").bind(&id).execute(&mut *tx).await?;
            tx.commit().await?;
            n += 1;
        }
        Ok(n)
    }

    /// Removes events and applied commands whose session row is gone (an
    /// earlier build could trim a live session). Without this, the head's
    /// redelivered input would be taken as already applied.
    pub async fn drop_orphans(&self) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let events =
            sqlx::query("DELETE FROM events WHERE session_id NOT IN (SELECT id FROM sessions)").execute(&mut *tx).await?.rows_affected();
        sqlx::query("DELETE FROM applied_commands WHERE session_id NOT IN (SELECT id FROM sessions)").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(events)
    }

    /// Sessions left mid-turn by a crash: (session id, last seq, open turn).
    /// Sessions with background tasks that never ended.
    pub async fn sessions_with_open_tasks(&self) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM sessions WHERE open_tasks > 0").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Cached sessions active since the cutoff, whose notice logs may
    /// need recovery. Filter here so older sessions' logs aren't read.
    pub async fn recent_session_ids(&self, since_ms: i64) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM sessions WHERE last_activity >= ?").bind(since_ms).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    pub async fn open_turns(&self) -> anyhow::Result<Vec<(String, u64, u64)>> {
        let rows = sqlx::query("SELECT id, last_seq, open_turn FROM sessions WHERE open_turn IS NOT NULL").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| (r.get("id"), r.get::<i64, _>("last_seq") as u64, r.get::<i64, _>("open_turn") as u64)).collect())
    }
}

async fn append_in(tx: &mut sqlx::SqliteConnection, session_id: &str, event: &SessionEvent) -> anyhow::Result<()> {
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
