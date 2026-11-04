//! A database's log, read for backups (docs/backups.md §3), and the
//! segments it becomes.
//!
//! SQLite's WAL file is a header and frames, each a page with its number,
//! salts that name the log's current cycle, and a running checksum. The
//! valid frames up to the last commit are what the log holds. A segment is
//! those frames; replaying segments in order onto a base copy rebuilds the
//! database (https://www.sqlite.org/fileformat2.html#walformat).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

const HEADER: usize = 32;
const FRAME_HEADER: usize = 24;

/// How far the log has been read: in which cycle (salts), how many
/// frames, and the running checksum after them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub salt1: u32,
    pub salt2: u32,
    pub frames: u64,
    pub ck1: u32,
    pub ck2: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub pgno: u32,
    /// For a commit's last frame, the database's size in pages after it.
    pub commit: u32,
    pub page: Vec<u8>,
}

/// What a read found.
#[derive(Debug, Default)]
pub struct Scan {
    /// The log's page size (0: no log).
    pub page_size: u32,
    /// The log's cycle is not the cursor's: it started over.
    pub restarted: bool,
    /// Committed frames after the cursor.
    pub frames: Vec<Frame>,
    pub cursor: Cursor,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn checksum(big_endian: bool, data: &[u8], s: &mut (u32, u32)) {
    for w in data.chunks_exact(8) {
        let (a, b) = if big_endian {
            (be32(&w[..4]), be32(&w[4..]))
        } else {
            (u32::from_le_bytes([w[0], w[1], w[2], w[3]]), u32::from_le_bytes([w[4], w[5], w[6], w[7]]))
        };
        s.0 = s.0.wrapping_add(a).wrapping_add(s.1);
        s.1 = s.1.wrapping_add(b).wrapping_add(s.0);
    }
}

/// Reads the committed frames of the log at `wal` after `from`. A log in
/// another cycle is read from its start (`restarted`).
pub fn scan(wal: &Path, from: &Cursor) -> anyhow::Result<Scan> {
    let mut f = match std::fs::File::open(wal) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Scan { restarted: from.frames > 0, cursor: Cursor::default(), ..Default::default() });
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", wal.display())),
    };
    let mut h = [0u8; HEADER];
    if f.read_exact(&mut h).is_err() {
        return Ok(Scan { restarted: from.frames > 0, ..Default::default() });
    }
    let magic = be32(&h[0..4]);
    if magic != 0x377f0682 && magic != 0x377f0683 {
        return Ok(Scan { restarted: from.frames > 0, ..Default::default() });
    }
    let big_endian = magic & 1 == 1;
    let page_size = be32(&h[8..12]);
    let (salt1, salt2) = (be32(&h[16..20]), be32(&h[20..24]));
    let mut s = (0u32, 0u32);
    checksum(big_endian, &h[..24], &mut s);
    if s != (be32(&h[24..28]), be32(&h[28..32])) {
        // A header that doesn't check out is an empty log.
        return Ok(Scan { restarted: from.frames > 0, ..Default::default() });
    }
    let same = from.frames > 0 && (from.salt1, from.salt2) == (salt1, salt2);
    let start = if same { from.frames } else { 0 };
    let mut sum = if same { (from.ck1, from.ck2) } else { s };
    let frame_len = FRAME_HEADER + page_size as usize;
    f.seek(SeekFrom::Start(HEADER as u64 + start * frame_len as u64))?;
    let mut r = std::io::BufReader::with_capacity(1 << 20, f);
    let mut buf = vec![0u8; frame_len];
    let (mut frames, mut pending) = (Vec::new(), Vec::new());
    let mut cursor = if same { *from } else { Cursor { salt1, salt2, frames: 0, ck1: s.0, ck2: s.1 } };
    let mut n = start;
    while r.read_exact(&mut buf).is_ok() {
        if (be32(&buf[8..12]), be32(&buf[12..16])) != (salt1, salt2) {
            break;
        }
        let mut next = sum;
        checksum(big_endian, &buf[..8], &mut next);
        checksum(big_endian, &buf[FRAME_HEADER..], &mut next);
        if next != (be32(&buf[16..20]), be32(&buf[20..24])) {
            break;
        }
        sum = next;
        n += 1;
        let commit = be32(&buf[4..8]);
        pending.push(Frame { pgno: be32(&buf[..4]), commit, page: buf[FRAME_HEADER..].to_vec() });
        if commit != 0 {
            frames.append(&mut pending);
            cursor = Cursor { salt1, salt2, frames: n, ck1: sum.0, ck2: sum.1 };
        }
    }
    Ok(Scan { page_size, restarted: !same && from.frames > 0, frames, cursor })
}

const SEGMENT: &[u8; 6] = b"C6SEG1";

/// Frames as a segment's bytes.
pub fn encode(page_size: u32, frames: &[Frame]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + frames.len() * (8 + page_size as usize));
    out.extend_from_slice(SEGMENT);
    out.extend_from_slice(&page_size.to_be_bytes());
    out.extend_from_slice(&(frames.len() as u32).to_be_bytes());
    for f in frames {
        out.extend_from_slice(&f.pgno.to_be_bytes());
        out.extend_from_slice(&f.commit.to_be_bytes());
        out.extend_from_slice(&f.page);
    }
    out
}

pub fn decode(b: &[u8]) -> anyhow::Result<(u32, Vec<Frame>)> {
    if b.len() < 14 || &b[..6] != SEGMENT {
        bail!("not a log segment");
    }
    let page_size = be32(&b[6..10]);
    let n = be32(&b[10..14]) as usize;
    let mut frames = Vec::with_capacity(n);
    let mut at = 14;
    for _ in 0..n {
        let end = at + 8 + page_size as usize;
        anyhow::ensure!(end <= b.len(), "a log segment is cut short");
        frames.push(Frame { pgno: be32(&b[at..]), commit: be32(&b[at + 4..]), page: b[at + 8..end].to_vec() });
        at = end;
    }
    Ok((page_size, frames))
}

/// Writes frames into a database file (no log of its own): each page in
/// its place, and at each commit the file cut to the database's size.
pub fn apply(db: &mut std::fs::File, page_size: u32, frames: &[Frame]) -> anyhow::Result<()> {
    for f in frames {
        anyhow::ensure!(f.pgno >= 1 && f.page.len() == page_size as usize, "a frame doesn't fit");
        db.seek(SeekFrom::Start((f.pgno as u64 - 1) * page_size as u64))?;
        db.write_all(&f.page)?;
        if f.commit != 0 {
            db.set_len(f.commit as u64 * page_size as u64)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

    use super::*;

    async fn pool(path: &Path) -> SqlitePool {
        let opts =
            SqliteConnectOptions::from_str("sqlite:").unwrap().filename(path).create_if_missing(true).journal_mode(SqliteJournalMode::Wal);
        c6_core::wal::manual_pool(SqlitePoolOptions::new().max_connections(1)).connect_with(opts).await.unwrap()
    }

    async fn rows(path: &Path) -> Vec<String> {
        let p = pool(path).await;
        let v = sqlx::query_scalar("SELECT x FROM t ORDER BY rowid").fetch_all(&p).await.unwrap();
        p.close().await;
        v
    }

    /// Reading the log after each change and replaying it onto a copy
    /// taken at a checkpoint rebuilds the database, across the log
    /// starting over.
    #[tokio::test]
    async fn replayed_frames_rebuild_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let (path, wal) = (dir.path().join("a.db"), dir.path().join("a.db-wal"));
        let p = pool(&path).await;
        sqlx::query("CREATE TABLE t (x TEXT)").execute(&p).await.unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&p).await.unwrap();
        // The base copy: the database file once checkpointed.
        let copy = dir.path().join("copy.db");
        std::fs::copy(&path, &copy).unwrap();
        let mut cursor = Cursor::default();
        let mut segments = Vec::new();
        for round in 0..3 {
            for i in 0..300 {
                sqlx::query("INSERT INTO t VALUES (?)").bind(format!("{round}-{i}-{}", "y".repeat(200))).execute(&p).await.unwrap();
            }
            sqlx::query("DELETE FROM t WHERE rowid % 7 = 0").execute(&p).await.unwrap();
            let s = scan(&wal, &cursor).unwrap();
            assert!(!s.frames.is_empty());
            assert_eq!(s.restarted, round > 0, "after a full checkpoint the log starts over");
            segments.push(encode(s.page_size, &s.frames));
            cursor = s.cursor;
            // Nothing new: nothing read.
            assert!(scan(&wal, &cursor).unwrap().frames.is_empty());
            let (_, log, done): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)").fetch_one(&p).await.unwrap();
            assert_eq!(log, done);
        }
        let want: Vec<String> = sqlx::query_scalar("SELECT x FROM t ORDER BY rowid").fetch_all(&p).await.unwrap();
        p.close().await;
        let mut f = std::fs::OpenOptions::new().write(true).open(&copy).unwrap();
        for seg in &segments {
            let (ps, frames) = decode(seg).unwrap();
            apply(&mut f, ps, &frames).unwrap();
        }
        drop(f);
        assert_eq!(rows(&copy).await, want);
    }

    #[test]
    fn a_missing_or_foreign_log_reads_as_started_over() {
        let dir = tempfile::tempdir().unwrap();
        let from = Cursor { salt1: 1, salt2: 2, frames: 5, ck1: 0, ck2: 0 };
        let s = scan(&dir.path().join("none-wal"), &from).unwrap();
        assert!(s.restarted && s.frames.is_empty());
        std::fs::write(dir.path().join("junk-wal"), b"not a log at all, just some bytes").unwrap();
        assert!(scan(&dir.path().join("junk-wal"), &from).unwrap().restarted);
        assert!(!scan(&dir.path().join("junk-wal"), &Cursor::default()).unwrap().restarted);
    }
}
