//! Reading the databases' logs (docs/backups.md §3). The head checkpoints
//! its databases itself: each pass, for each database, it takes the write
//! lock for a moment, reads the frames committed since last time into a
//! segment in the local spool (when backups are on), checkpoints, and
//! lets go. Nothing is lost between reading and checkpointing, because no
//! one can write meanwhile.

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

/// The two connections a database's pass uses.
pub struct Conns {
    lock: SqliteConnection,
    ckpt: SqliteConnection,
}

async fn connect(path: &Path) -> anyhow::Result<SqliteConnection> {
    let opts = SqliteConnectOptions::from_str("sqlite:")?.filename(path).busy_timeout(Duration::from_secs(30));
    let mut c = SqliteConnection::connect_with(&opts).await.with_context(|| format!("opening {}", path.display()))?;
    c6_core::wal::configure(&mut c).await?;
    Ok(c)
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
    sqlx::query("BEGIN IMMEDIATE").execute(&mut conns.lock).await?;
    let r = async {
        let mut ds = st.backups.with(st, |s| s.dbs.get(rel).cloned().unwrap_or_default());
        let mark = file_mark(&path);
        let scan = wal::scan(&wal_path, &ds.cursor)?;
        let upgraded = newest_upgrade(&mut conns.lock).await;
        // Changed outside the head (another program, a restored copy), or
        // upgraded: what follows can't build on earlier base copies.
        let outside = (ds.file.is_some() && ds.file != mark) || (scan.restarted && !ds.restart_ok);
        if outside || upgraded > ds.upgraded_at {
            tracing::info!(
                database = rel,
                file_changed = ds.file.is_some() && ds.file != mark,
                log_restarted = scan.restarted && !ds.restart_ok,
                upgraded = upgraded > ds.upgraded_at,
                "its line of changes starts again (new base copies follow)"
            );
            ds.epoch += 1;
            ds.upgraded_at = upgraded;
            let _ = std::fs::remove_dir_all(super::state_dir(st).join("spool").join(rel.replace('/', "_")));
        } else if spool && !scan.frames.is_empty() {
            let dir = spool_dir(st, rel, ds.epoch);
            std::fs::create_dir_all(&dir)?;
            let seq = ds.seq + 1;
            let file = dir.join(format!("{seq:012}-{:013}.seg", c6_core::now_ms()));
            let tmp = file.with_extension("tmp");
            std::fs::write(&tmp, wal::encode(scan.page_size, &scan.frames))?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &file)?;
            ds.seq = seq;
        }
        ds.cursor = scan.cursor;
        let start = if base {
            // Its whole log, from the start of this cycle: replayed onto the
            // file as it is now (no checkpoint changes it until `resume`).
            let all = wal::scan(&wal_path, &wal::Cursor::default())?;
            *st.backups.paused.lock().unwrap_or_else(|e| e.into_inner()).entry(rel.to_owned()).or_default() += 1;
            Some(BaseStart { page_size: all.page_size, wal0: all.frames, start_seq: ds.seq, epoch: ds.epoch })
        } else {
            None
        };
        let paused = st.backups.paused.lock().unwrap_or_else(|e| e.into_inner()).get(rel).is_some_and(|n| *n > 0);
        if !paused && ds.cursor.frames > 0 {
            let (_, log, done): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)").fetch_one(&mut conns.ckpt).await?;
            ds.restart_ok = log >= 0 && log == done;
        }
        if !paused {
            ds.file = file_mark(&path);
        }
        st.backups.with(st, |s| s.dbs.insert(rel.to_owned(), ds));
        anyhow::Ok(start)
    }
    .await;
    let _ = sqlx::query("ROLLBACK").execute(&mut conns.lock).await;
    r
}

/// A pass over every database: reads their logs and checkpoints them.
pub async fn pass(st: &HeadState, spool: bool) -> anyhow::Result<()> {
    let mut conns = st.backups.capture.lock().await;
    for rel in databases(&st.data_dir) {
        if !conns.contains_key(&rel) {
            let path = st.data_dir.join(&rel);
            conns.insert(rel.clone(), Conns { lock: connect(&path).await?, ckpt: connect(&path).await? });
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
        conns.insert(rel.to_owned(), Conns { lock: connect(&path).await?, ckpt: connect(&path).await? });
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
