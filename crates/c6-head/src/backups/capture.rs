//! Reading the databases' logs (docs/backups.md §3). The head checkpoints
//! its databases itself. A brief writer gate aligns a read snapshot with
//! SQLite's committed WAL end. The read snapshot limits the checkpoint to
//! frames we captured; copying and syncing them does not hold the writer.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

use super::wal;
use crate::HeadState;

/// How far a database's log was read, and what makes its changes one
/// unbroken line.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DbState {
    pub cursor: wal::Cursor,
    /// The last checkpoint folded the whole log in, so the log may start
    /// over (new salts) without anything being missed.
    #[serde(default)]
    pub restart_ok: bool,
    /// The last segment spooled.
    #[serde(default)]
    pub seq: u64,
    /// The database file as the last checkpoint left it: only checkpoints
    /// change it, so anything else did if it differs.
    #[serde(default)]
    pub file: Option<(u64, i128)>,
    /// The newest upgrade seen in its `upgrades` table.
    #[serde(default)]
    pub upgraded_at: i64,
    /// Bumped whenever the line of changes breaks (the database changed
    /// outside the head, or was upgraded): base copies from an older epoch
    /// can't take later segments.
    #[serde(default)]
    pub epoch: u64,
}

/// The connections a database's pass uses.
pub struct Conns {
    lock: SqliteConnection,
    read: SqliteConnection,
    ckpt: SqliteConnection,
}

async fn connect(path: &Path) -> anyhow::Result<SqliteConnection> {
    let opts = SqliteConnectOptions::from_str("sqlite:")?.filename(path).busy_timeout(Duration::from_secs(30));
    let mut c = SqliteConnection::connect_with(&opts).await.with_context(|| format!("opening {}", path.display()))?;
    c6_core::wal::configure(&mut c).await?;
    Ok(c)
}

impl Conns {
    async fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self { lock: connect(path).await?, read: connect(path).await?, ckpt: connect(path).await? })
    }

    async fn pin<'a>(
        lock: &mut SqliteConnection,
        read: &'a mut SqliteConnection,
        ckpt: &mut SqliteConnection,
    ) -> anyhow::Result<(sqlx::Transaction<'a, sqlx::Sqlite>, i64, i64)> {
        // BEGIN alone does not establish a read snapshot. Start a real read
        // before sampling mxFrame, with no commits possible in between.
        let gate = lock.begin_with("BEGIN IMMEDIATE").await?;
        let mut reader = read.begin().await?;
        sqlx::query("SELECT 1 FROM main.sqlite_schema LIMIT 1").fetch_optional(&mut *reader).await?;
        // NOOP gets the committed end from the WAL index without doing any
        // checkpoint I/O. Never infer it from on-disk checksums alone: a
        // writer may have written a commit frame without publishing it yet.
        let (busy, log, backfilled): (i64, i64, i64) = sqlx::query_as("PRAGMA main.wal_checkpoint(NOOP)").fetch_one(ckpt).await?;
        gate.rollback().await?;
        anyhow::ensure!(
            busy == 0 && ((log, backfilled) == (-1, -1) || (backfilled >= 0 && backfilled <= log)),
            "could not sample the committed WAL end"
        );
        Ok((reader, log, backfilled))
    }
}

/// The head's databases, by path in its folder (those that exist).
pub fn databases(data_dir: &Path) -> Vec<String> {
    let mut out = vec!["c6h.db".to_owned(), "knowledge/memex.db".to_owned()];
    if let Ok(rd) = std::fs::read_dir(data_dir.join("knowledge").join("realms")) {
        let mut realms: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        realms.sort();
        out.extend(realms.into_iter().map(|r| format!("knowledge/realms/{r}/memex.db")));
    }
    out.retain(|r| data_dir.join(r).is_file());
    out
}

/// Where a database's unsent segments wait, per epoch.
pub fn spool_dir(st: &HeadState, rel: &str, epoch: u64) -> PathBuf {
    super::state_dir(st).join("spool").join(rel.replace('/', "_")).join(epoch.to_string())
}

/// A spooled segment's sequence number and time, from its file name.
pub fn spooled(dir: &Path) -> Vec<(u64, i64, PathBuf)> {
    let mut out: Vec<(u64, i64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (seq, ms) = name.strip_suffix(".seg")?.split_once('-')?;
            Some((seq.parse().ok()?, ms.parse().ok()?, e.path()))
        })
        .collect();
    out.sort();
    out
}

fn file_mark(path: &Path) -> Option<(u64, i128)> {
    let m = std::fs::metadata(path).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as i128;
    Some((m.len(), t))
}

async fn newest_upgrade(c: &mut SqliteConnection) -> i64 {
    sqlx::query_scalar::<_, Option<i64>>("SELECT max(at) FROM upgrades").fetch_one(&mut *c).await.ok().flatten().unwrap_or(0)
}

/// A base copy's start, read under the lock (see `gens`): its log so far,
/// and where in the line of segments it stands.
pub struct BaseStart {
    pub page_size: u32,
    pub wal0: Vec<wal::Frame>,
    pub start_seq: u64,
    pub epoch: u64,
}

/// One database's pass. `spool`: backups are on. `base`: also start a
/// base copy (its checkpoints then wait until `resume`).
async fn one(st: &HeadState, conns: &mut Conns, rel: &str, spool: bool, base: bool) -> anyhow::Result<Option<BaseStart>> {
    let path = st.data_dir.join(rel);
    let wal_path = PathBuf::from(format!("{}-wal", path.display()));
    let (mut reader, log, backfilled) = Conns::pin(&mut conns.lock, &mut conns.read, &mut conns.ckpt).await?;
    let upgraded = newest_upgrade(&mut reader).await;
    let mut ds = st.backups.with(st, |s| s.dbs.get(rel).cloned().unwrap_or_default());
    let spool_root = super::state_dir(st).join("spool").join(rel.replace('/', "_"));
    let database = rel.to_owned();
    let uncopied = log > backfilled;
    let copy = move || {
        let mark = file_mark(&path);
        let file_changed = ds.file.is_some() && ds.file != mark;
        let upgraded_db = upgraded > ds.upgraded_at;
        let from = if file_changed || upgraded_db { wal::Cursor::default() } else { ds.cursor };
        // If already checkpointed, SQLite may let a writer restart the
        // WAL even with our reader active. There is nothing new to copy
        // from it: take a base directly from the stable DB file, if asked.
        let scan = uncopied.then(|| wal::scan_committed(&wal_path, &from, log as u64)).transpose()?;
        let restarted = scan.as_ref().map_or(log <= 0 && ds.cursor.frames > 0, |s| s.restarted);
        let unexpected_checkpoint = !uncopied && log > 0 && ds.file.is_some() && ds.cursor.frames != log as u64;
        // Changed outside the head (another program, a restored copy), or
        // upgraded: what follows can't build on earlier base copies.
        let outside = file_changed || (restarted && !ds.restart_ok) || unexpected_checkpoint;
        if outside || upgraded_db {
            tracing::info!(
                database = %database,
                file_changed,
                log_restarted = restarted && !ds.restart_ok,
                upgraded = upgraded > ds.upgraded_at,
                "its line of changes starts again (new base copies follow)"
            );
            ds.epoch += 1;
            ds.upgraded_at = upgraded;
            let _ = std::fs::remove_dir_all(&spool_root);
        } else if let Some(scan) = scan.as_ref().filter(|s| spool && !s.frames.is_empty()) {
            let dir = spool_root.join(ds.epoch.to_string());
            std::fs::create_dir_all(&dir)?;
            let seq = ds.seq + 1;
            let file = dir.join(format!("{seq:012}-{:013}.seg", c6_core::now_ms()));
            let tmp = file.with_extension("tmp");
            std::fs::write(&tmp, wal::encode(scan.page_size, &scan.frames))?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &file)?;
            std::fs::File::open(&dir)?.sync_all()?;
            ds.seq = seq;
        }
        if let Some(scan) = scan {
            ds.cursor = scan.cursor;
        } else if outside || upgraded_db || log <= 0 {
            ds.cursor = wal::Cursor::default();
        }
        ds.file = mark;
        ds.restart_ok = !uncopied;
        let start = if base {
            // The capture mutex still excludes all head checkpoints.
            let all = if uncopied { wal::scan_committed(&wal_path, &wal::Cursor::default(), log as u64)? } else { wal::Scan::default() };
            Some(BaseStart { page_size: all.page_size, wal0: all.frames, start_seq: ds.seq, epoch: ds.epoch })
        } else {
            None
        };
        anyhow::Ok((ds, start))
    };
    // Don't detach the disk work from the capture mutex or read pin: if
    // interrupted, it must not keep writing a segment for the old pass.
    let (mut ds, start) = match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(copy),
        _ => copy(),
    }?;

    let paused = st.backups.paused.lock().unwrap_or_else(|e| e.into_inner()).contains_key(rel);
    if uncopied && !paused && !base {
        // A reader whose snapshot contains WAL frames pins a read mark at
        // or before `log`. PASSIVE does not take SQLite's writer gate and
        // cannot checkpoint past that mark while the reader is alive.
        let (_, latest, done): (i64, i64, i64) = sqlx::query_as("PRAGMA main.wal_checkpoint(PASSIVE)").fetch_one(&mut conns.ckpt).await?;
        anyhow::ensure!(done <= log, "checkpoint exceeded the captured WAL end");
        ds.restart_ok = latest >= 0 && latest == done;
        ds.file = file_mark(&st.data_dir.join(rel));
    }
    reader.rollback().await?;
    if base {
        *st.backups.paused.lock().unwrap_or_else(|e| e.into_inner()).entry(rel.to_owned()).or_default() += 1;
    }
    st.backups.with(st, |s| s.dbs.insert(rel.to_owned(), ds));
    Ok(start)
}

/// A pass over every database: reads their logs and checkpoints them.
pub async fn pass(st: &HeadState, spool: bool) -> anyhow::Result<()> {
    // A cancelled API caller must not drop the read pin while the SQLx
    // worker is still running PASSIVE on the other connection.
    let st = st.arc().context("the head stopped")?;
    tokio::spawn(async move { pass_inner(&st, spool).await }).await?
}

async fn pass_inner(st: &HeadState, spool: bool) -> anyhow::Result<()> {
    let mut conns = st.backups.capture.lock().await;
    for rel in databases(&st.data_dir) {
        if !conns.contains_key(&rel) {
            let path = st.data_dir.join(&rel);
            conns.insert(rel.clone(), Conns::open(&path).await?);
        }
        let c = conns.get_mut(&rel).expect("just made");
        if let Err(e) = one(st, c, &rel, spool, false).await {
            tracing::warn!(database = %rel, "reading its log: {e:#}");
            conns.remove(&rel);
        }
    }
    Ok(())
}

/// Starts a base copy of `rel`: its checkpoints wait until `resume`.
pub async fn begin_base(st: &HeadState, rel: &str) -> anyhow::Result<BaseStart> {
    let mut conns = st.backups.capture.lock().await;
    if !conns.contains_key(rel) {
        let path = st.data_dir.join(rel);
        conns.insert(rel.to_owned(), Conns::open(&path).await?);
    }
    let c = conns.get_mut(rel).expect("just made");
    one(st, c, rel, true, true).await?.context("starting a base copy")
}

/// A base copy is done (or gave up): checkpoints may go on.
pub fn resume(st: &HeadState, rel: &str) {
    let mut p = st.backups.paused.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = p.get_mut(rel) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            p.remove(rel);
        }
    }
}

/// The size of a database's log now.
pub fn log_size(st: &HeadState, rel: &str) -> u64 {
    std::fs::metadata(format!("{}-wal", st.data_dir.join(rel).display())).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteJournalMode;

    #[tokio::test]
    async fn captured_wal_prefix_does_not_block_writes_or_checkpoint_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let opts = SqliteConnectOptions::from_str("sqlite:")
            .unwrap()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::ZERO);
        let mut writer = SqliteConnection::connect_with(&opts).await.unwrap();
        c6_core::wal::configure(&mut writer).await.unwrap();
        sqlx::query("CREATE TABLE t(v INTEGER)").execute(&mut writer).await.unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&mut writer).await.unwrap();
        sqlx::query("INSERT INTO t VALUES (1)").execute(&mut writer).await.unwrap();

        let mut conns = Conns::open(&path).await.unwrap();
        let (mut reader, end, checked) = Conns::pin(&mut conns.lock, &mut conns.read, &mut conns.ckpt).await.unwrap();
        assert!(end > checked);
        // The read pin remains live through capture and checkpoint, but a
        // foreground write with no busy timeout must commit immediately.
        sqlx::query("INSERT INTO t VALUES (2)").execute(&mut writer).await.unwrap();
        let wal_path = dir.path().join("test.db-wal");
        let first = wal::scan_committed(&wal_path, &wal::Cursor::default(), end as u64).unwrap();
        assert_eq!(first.frames.len(), end as usize);
        let (busy, latest, done): (i64, i64, i64) =
            sqlx::query_as("PRAGMA main.wal_checkpoint(PASSIVE)").fetch_one(&mut conns.ckpt).await.unwrap();
        assert!(latest > end);
        assert_eq!((busy, done), (0, end), "must not checkpoint the uncaptured write");
        let seen: Vec<i64> = sqlx::query_scalar("SELECT v FROM t").fetch_all(&mut *reader).await.unwrap();
        assert_eq!(seen, vec![1]);
        reader.rollback().await.unwrap();

        let (reader, next, checked) = Conns::pin(&mut conns.lock, &mut conns.read, &mut conns.ckpt).await.unwrap();
        assert_eq!((next, checked), (latest, end));
        let rest = wal::scan_committed(&wal_path, &first.cursor, next as u64).unwrap();
        assert!(!rest.restarted);
        assert_eq!(rest.frames.len() as i64, next - end);
        reader.rollback().await.unwrap();
    }
}
