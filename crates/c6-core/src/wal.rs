//! The head checkpoints its databases itself (docs/backups.md §3).
//!
//! Every connection to a database under a folder set with [`manual_under`]
//! turns off SQLite's automatic checkpoints and the checkpoint a closing
//! connection runs, so the log keeps every change until the head has read
//! it (for backups) and checkpoints it. Its log is capped in size when it
//! starts over.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sqlx::sqlite::{SqliteConnection, SqlitePoolOptions};

static MANUAL: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Databases under `dir` are checkpointed by the head, not by SQLite.
pub fn manual_under(dir: &Path) {
    let mut m = MANUAL.lock().unwrap_or_else(|e| e.into_inner());
    if !m.iter().any(|d| d == dir) {
        m.push(dir.to_owned());
    }
}

/// Whether the database at `path` is checkpointed by the head.
pub fn is_manual(path: &Path) -> bool {
    MANUAL.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|d| path.starts_with(d))
}

/// `pool` with every connection set up for the head's checkpoints.
pub fn manual_pool(pool: SqlitePoolOptions) -> SqlitePoolOptions {
    pool.after_connect(|conn, _| Box::pin(async move { configure(conn).await }))
}

/// Sets up one connection: no automatic checkpoints, none on close, and a
/// log no bigger than 64 MB once it starts over.
pub async fn configure(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("PRAGMA wal_autocheckpoint = 0").execute(&mut *conn).await?;
    sqlx::query("PRAGMA journal_size_limit = 67108864").execute(&mut *conn).await?;
    let mut handle = conn.lock_handle().await?;
    let db = handle.as_raw_handle().as_ptr();
    // SAFETY: `db` is this connection's open handle, held locked for the
    // call; the option takes an int and an optional int pointer for the
    // value it was set to.
    let rc = unsafe {
        libsqlite3_sys::sqlite3_db_config(
            db,
            libsqlite3_sys::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            1 as std::ffi::c_int,
            std::ptr::null_mut::<std::ffi::c_int>(),
        )
    };
    if rc != libsqlite3_sys::SQLITE_OK {
        return Err(sqlx::Error::Configuration(format!("turning off the checkpoint on close failed ({rc})").into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

    use super::*;

    /// A manual database keeps its log through its last connection closing,
    /// and only an explicit checkpoint folds it in.
    #[tokio::test]
    async fn the_log_stays_until_checkpointed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        manual_under(dir.path());
        assert!(is_manual(&path) && !is_manual(Path::new("/elsewhere/a.db")));
        let opts =
            SqliteConnectOptions::from_str("sqlite:").unwrap().filename(&path).create_if_missing(true).journal_mode(SqliteJournalMode::Wal);
        let pool = manual_pool(SqlitePoolOptions::new().max_connections(2)).connect_with(opts.clone()).await.unwrap();
        sqlx::query("CREATE TABLE t (x TEXT)").execute(&pool).await.unwrap();
        for i in 0..2000 {
            sqlx::query("INSERT INTO t VALUES (?)").bind(format!("row {i} {}", "x".repeat(100))).execute(&pool).await.unwrap();
        }
        pool.close().await;
        let wal = dir.path().join("a.db-wal");
        let len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(len > 100_000, "the log is still there after closing ({len} bytes)");
        let pool = manual_pool(SqlitePoolOptions::new().max_connections(1)).connect_with(opts).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM t").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 2000);
        let (busy, log, done): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)").fetch_one(&pool).await.unwrap();
        assert_eq!((busy, log), (0, done), "an explicit checkpoint folds it all in");
        pool.close().await;
    }
}
