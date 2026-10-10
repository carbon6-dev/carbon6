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
