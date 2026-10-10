//! Durable head state (SQLite via sqlx).

use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use c6_core::WorkerId;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::{FromRow, Row};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

#[derive(Clone, Debug, FromRow)]
pub struct WorkerRow {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: Option<String>,
    pub cert_serial: String,
    pub enrolled_at: i64,
    pub last_seen: Option<i64>,
    pub revoked_at: Option<i64>,
}

impl WorkerRow {
    pub fn worker_id(&self) -> Option<WorkerId> {
        self.id.parse().ok()
    }
}

pub struct NewWorker<'a> {
    pub id: WorkerId,
    pub name: &'a str,
    pub hostname: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
    pub version: &'a str,
    pub cert_serial: &'a str,
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool =
            SqlitePoolOptions::new().max_connections(8).connect_with(opts).await.with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations").run(&pool).await.context("running migrations")?;
        Ok(Self { pool })
    }

    pub async fn create_enrollment(&self, secret_hash: &str, name: Option<&str>, now: i64, expires_at: i64) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO enrollments (secret_hash, name, created_at, expires_at) VALUES (?, ?, ?, ?)")
            .bind(secret_hash)
            .bind(name)
            .bind(now)
            .bind(expires_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Atomically marks an enrollment used. Returns its requested name
    /// (`Some(None)` if it had none) or `None` if it is unknown, used or expired.
    pub async fn consume_enrollment(&self, secret_hash: &str, now: i64) -> anyhow::Result<Option<Option<String>>> {
        let row =
            sqlx::query("UPDATE enrollments SET used_at = ? WHERE secret_hash = ? AND used_at IS NULL AND expires_at > ? RETURNING name")
                .bind(now)
                .bind(secret_hash)
                .bind(now)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.get::<Option<String>, _>("name")))
    }

    pub async fn insert_worker(&self, w: &NewWorker<'_>, secret_hash: &str, now: i64) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO workers (id, name, hostname, os, arch, version, cert_serial, enrolled_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(w.id.to_string())
        .bind(w.name)
        .bind(w.hostname)
        .bind(w.os)
        .bind(w.arch)
        .bind(w.version)
        .bind(w.cert_serial)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE enrollments SET worker_id = ? WHERE secret_hash = ?")
            .bind(w.id.to_string())
            .bind(secret_hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn worker(&self, id: WorkerId) -> anyhow::Result<Option<WorkerRow>> {
        Ok(sqlx::query_as("SELECT * FROM workers WHERE id = ?").bind(id.to_string()).fetch_optional(&self.pool).await?)
    }

    pub async fn workers(&self) -> anyhow::Result<Vec<WorkerRow>> {
        Ok(sqlx::query_as("SELECT * FROM workers ORDER BY name, id").fetch_all(&self.pool).await?)
    }

    /// Records facts from a worker's `Hello`.
    pub async fn worker_attached(&self, id: WorkerId, hostname: &str, os: &str, arch: &str, version: &str, now: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE workers SET hostname = ?, os = ?, arch = ?, version = ?, last_seen = ? WHERE id = ?")
            .bind(hostname)
            .bind(os)
            .bind(arch)
            .bind(version)
            .bind(now)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn touch_worker(&self, id: WorkerId, now: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE workers SET last_seen = ? WHERE id = ?").bind(now).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }

    /// Returns false if the worker does not exist.
    pub async fn revoke_worker(&self, id: WorkerId, now: i64) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE workers SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
            .bind(now)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn delete_worker(&self, id: WorkerId) -> anyhow::Result<bool> {
        let r = sqlx::query("DELETE FROM workers WHERE id = ?").bind(id.to_string()).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }
}

// --- Connections --------------------------------------------------------------

#[derive(Clone, Debug, FromRow)]
pub struct ConnectionRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: Option<Vec<u8>>,
    pub default_model: Option<String>,
    pub created_at: i64,
}

impl Store {
    pub async fn insert_connection(&self, c: &ConnectionRow) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO connections (id, name, kind, base_url, api_key, default_model, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&c.id)
            .bind(&c.name)
            .bind(&c.kind)
            .bind(&c.base_url)
            .bind(&c.api_key)
            .bind(&c.default_model)
            .bind(c.created_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn connections(&self) -> anyhow::Result<Vec<ConnectionRow>> {
        Ok(sqlx::query_as("SELECT * FROM connections ORDER BY name").fetch_all(&self.pool).await?)
    }

    pub async fn connection(&self, id: &str) -> anyhow::Result<Option<ConnectionRow>> {
        Ok(sqlx::query_as("SELECT * FROM connections WHERE id = ?").bind(id).fetch_optional(&self.pool).await?)
    }

    pub async fn set_connection_secret(&self, id: &str, secret: &[u8]) -> anyhow::Result<()> {
        sqlx::query("UPDATE connections SET api_key = ? WHERE id = ?").bind(secret).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn delete_connection(&self, id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("DELETE FROM connections WHERE id = ?").bind(id).execute(&self.pool).await?.rows_affected() > 0)
    }
}

// --- Sessions and events ------------------------------------------------------

#[derive(Clone, Debug, FromRow)]
pub struct SessionRow {
    pub id: String,
    pub worker_id: String,
    pub title: String,
    pub cwd: String,
    pub connection_id: String,
    pub model: String,
    pub status: String,
    pub error: Option<String>,
    pub effort: Option<String>,
    pub parent_id: Option<String>,
    pub parent_call_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seq: i64,
    pub archived: i64,
}

#[derive(Clone, Debug, FromRow)]
pub struct CommandRow {
    pub seq: i64,
    pub id: String,
    pub session_id: String,
    pub worker_id: String,
    pub data: Vec<u8>,
    pub text: Option<String>,
    pub created_at: i64,
    pub state: String,
    pub error: Option<String>,
}

/// What an event batch changed.
pub struct Ingested {
    pub last_seq: u64,
    pub events: Vec<c6_proto::SessionEvent>,
    /// Commands the worker has now applied.
    pub done_commands: Vec<String>,
}

impl Store {
    /// Creates a session and enqueues its commands atomically.
    pub async fn create_session(&self, s: &SessionRow, commands: &[NewCommand<'_>]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, effort) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&s.id)
        .bind(&s.worker_id)
        .bind(&s.title)
        .bind(&s.cwd)
        .bind(&s.connection_id)
        .bind(&s.model)
        .bind(&s.status)
        .bind(s.created_at)
        .bind(s.updated_at)
        .bind(&s.effort)
        .execute(&mut *tx)
        .await?;
        for c in commands {
            insert_command(&mut tx, c).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn session(&self, id: &str) -> anyhow::Result<Option<SessionRow>> {
        Ok(sqlx::query_as("SELECT * FROM sessions WHERE id = ?").bind(id).fetch_optional(&self.pool).await?)
    }

    pub async fn sessions(&self, limit: i64) -> anyhow::Result<Vec<SessionRow>> {
        Ok(sqlx::query_as("SELECT * FROM sessions WHERE archived = 0 AND parent_id IS NULL ORDER BY updated_at DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn child_sessions(&self, parent_id: &str) -> anyhow::Result<Vec<SessionRow>> {
        Ok(sqlx::query_as("SELECT * FROM sessions WHERE parent_id = ? ORDER BY created_at").bind(parent_id).fetch_all(&self.pool).await?)
    }

    pub async fn setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query("SELECT value FROM settings WHERE key = ?").bind(key).fetch_optional(&self.pool).await?.map(|r| r.get("value")))
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_archived(&self, id: &str, archived: bool) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE sessions SET archived = ? WHERE id = ?").bind(archived as i64).bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_title(&self, id: &str, title: &str) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE sessions SET title = ? WHERE id = ?").bind(title).bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }

    /// Last durable seq for each of `ids` that belongs to `worker_id`.
    pub async fn session_positions(&self, worker_id: WorkerId, ids: &[String]) -> anyhow::Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(row) = sqlx::query("SELECT last_seq FROM sessions WHERE id = ? AND worker_id = ?")
                .bind(id)
                .bind(worker_id.to_string())
                .fetch_optional(&self.pool)
                .await?
            {
                out.push((id.clone(), row.get::<i64, _>("last_seq") as u64));
            }
        }
        Ok(out)
    }

    /// Stores the next contiguous events of a worker's session; duplicates
    /// are skipped and a gap stops the batch (the worker resends from the
    /// ack). Returns `None` if the session is unknown or not this worker's.
    pub async fn ingest(
        &self,
        worker_id: WorkerId,
        session_id: &str,
        events: &[c6_proto::SessionEvent],
    ) -> anyhow::Result<Option<Ingested>> {
        use c6_proto::session_event::Kind;
        use prost::Message;

        let mut tx = self.pool.begin().await?;
        let mut row = sqlx::query("SELECT last_seq, status FROM sessions WHERE id = ? AND worker_id = ?")
            .bind(session_id)
            .bind(worker_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if row.is_none() && create_subagent_session(&mut tx, worker_id, session_id, events).await? {
            row = sqlx::query("SELECT last_seq, status FROM sessions WHERE id = ?").bind(session_id).fetch_optional(&mut *tx).await?;
        }
        let Some(row) = row else {
            return Ok(None);
        };
        let mut last = row.get::<i64, _>("last_seq") as u64;
        let mut status: String = row.get("status");
        let mut stored = Vec::new();
        let mut done = Vec::new();
        for ev in events {
            if ev.seq <= last {
                continue;
            }
            if ev.seq != last + 1 {
                break;
            }
            sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES (?, ?, ?, ?)")
                .bind(session_id)
                .bind(ev.seq as i64)
                .bind(ev.ts_ms)
                .bind(ev.encode_to_vec())
                .execute(&mut *tx)
                .await?;
            last = ev.seq;
            match &ev.kind {
                Some(Kind::Started(s)) => {
                    status = "idle".into();
                    done.push(s.command_id.clone());
                }
                Some(Kind::User(u)) => done.push(u.command_id.clone()),
                Some(Kind::TurnStarted(_)) => status = "running".into(),
                Some(Kind::TurnEnded(_)) => status = "idle".into(),
                _ => {}
            }
            stored.push(ev.clone());
        }
        if !stored.is_empty() {
            sqlx::query("UPDATE sessions SET last_seq = ?, status = ?, updated_at = ? WHERE id = ?")
                .bind(last as i64)
                .bind(&status)
                .bind(c6_core::now_ms())
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            for id in &done {
                sqlx::query("UPDATE commands SET state = 'done' WHERE id = ?").bind(id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(Some(Ingested { last_seq: last, events: stored, done_commands: done }))
    }

    /// The events of the turn that ends at `end_seq`, from its TurnStarted.
    pub async fn turn_events(&self, session_id: &str, end_seq: u64) -> anyhow::Result<Vec<c6_proto::SessionEvent>> {
        use prost::Message;
        let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? AND seq <= ? ORDER BY seq DESC LIMIT 5000")
            .bind(session_id)
            .bind(end_seq as i64)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let ev = c6_proto::SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?;
            let start = matches!(ev.kind, Some(c6_proto::session_event::Kind::TurnStarted(_)));
            out.push(ev);
            if start {
                break;
            }
        }
        out.reverse();
        Ok(out)
    }

    pub async fn events_after(&self, session_id: &str, after: u64, limit: i64) -> anyhow::Result<Vec<c6_proto::SessionEvent>> {
        use prost::Message;
        let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? AND seq > ? ORDER BY seq LIMIT ?")
            .bind(session_id)
            .bind(after as i64)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|r| Ok(c6_proto::SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?)).collect()
    }
}

/// Subagent sessions are created by the worker. Accept one when its first
/// event names a parent session that belongs to the same worker.
async fn create_subagent_session(
    tx: &mut sqlx::SqliteConnection,
    worker_id: WorkerId,
    session_id: &str,
    events: &[c6_proto::SessionEvent],
) -> anyhow::Result<bool> {
    let Some(c6_proto::session_event::Kind::Started(s)) = events.first().filter(|e| e.seq == 1).and_then(|e| e.kind.as_ref()) else {
        return Ok(false);
    };
    if s.parent_session_id.is_empty() {
        return Ok(false);
    }
    let parent = sqlx::query("SELECT connection_id, model, effort FROM sessions WHERE id = ? AND worker_id = ?")
        .bind(&s.parent_session_id)
        .bind(worker_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(parent) = parent else { return Ok(false) };
    let now = c6_core::now_ms();
    sqlx::query(
        "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, effort, parent_id, parent_call_id) \
         VALUES (?, ?, ?, ?, ?, ?, 'starting', ?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(worker_id.to_string())
    .bind(if s.title.is_empty() { "Subagent" } else { &s.title })
    .bind(&s.cwd)
    .bind(parent.get::<String, _>("connection_id"))
    .bind(parent.get::<String, _>("model"))
    .bind(now)
    .bind(now)
    .bind(parent.get::<Option<String>, _>("effort"))
    .bind(&s.parent_session_id)
    .bind(&s.parent_call_id)
    .execute(&mut *tx)
    .await?;
    Ok(true)
}

// --- Commands -----------------------------------------------------------------

pub struct NewCommand<'a> {
    pub id: &'a str,
    pub session_id: &'a str,
    pub worker_id: WorkerId,
    pub data: Vec<u8>,
    pub text: Option<&'a str>,
}

async fn insert_command(tx: &mut sqlx::SqliteConnection, c: &NewCommand<'_>) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO commands (id, session_id, worker_id, data, text, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(c.id)
        .bind(c.session_id)
        .bind(c.worker_id.to_string())
        .bind(&c.data)
        .bind(c.text)
        .bind(c6_core::now_ms())
        .execute(&mut *tx)
        .await?;
    Ok(())
}

impl Store {
    pub async fn enqueue_command(&self, c: &NewCommand<'_>) -> anyhow::Result<()> {
        let mut conn = self.pool.acquire().await?;
        insert_command(&mut conn, c).await
    }

    /// Pending commands for a worker after `after_seq`, in delivery order.
    pub async fn pending_commands(&self, worker_id: WorkerId, after_seq: i64) -> anyhow::Result<Vec<CommandRow>> {
        Ok(sqlx::query_as("SELECT * FROM commands WHERE worker_id = ? AND state = 'pending' AND seq > ? ORDER BY seq LIMIT 500")
            .bind(worker_id.to_string())
            .bind(after_seq)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Inputs not yet applied (or rejected) for a session, oldest first.
    pub async fn queued_inputs(&self, session_id: &str) -> anyhow::Result<Vec<CommandRow>> {
        Ok(sqlx::query_as("SELECT * FROM commands WHERE session_id = ? AND state != 'done' AND text IS NOT NULL ORDER BY seq")
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Marks a pending command rejected. A rejected `Create` fails the
    /// session and everything queued behind it. Returns the session id.
    pub async fn reject_command(&self, worker_id: WorkerId, id: &str, reason: &str) -> anyhow::Result<Option<String>> {
        let mut tx = self.pool.begin().await?;
        let Some(row) =
            sqlx::query("UPDATE commands SET state = 'rejected', error = ? WHERE id = ? AND worker_id = ? AND state = 'pending' RETURNING session_id, text")
                .bind(reason)
                .bind(id)
                .bind(worker_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(None);
        };
        let session_id: String = row.get("session_id");
        if row.get::<Option<String>, _>("text").is_none() {
            sqlx::query("UPDATE sessions SET status = 'failed', error = ?, updated_at = ? WHERE id = ?")
                .bind(reason)
                .bind(c6_core::now_ms())
                .bind(&session_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE commands SET state = 'rejected', error = 'session failed to start' WHERE session_id = ? AND state = 'pending'",
            )
            .bind(&session_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Some(session_id))
    }

    /// Clears rejected inputs from a session's queue (the user dismissed them).
    pub async fn dismiss_rejected(&self, session_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE commands SET state = 'done' WHERE session_id = ? AND state = 'rejected'")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
