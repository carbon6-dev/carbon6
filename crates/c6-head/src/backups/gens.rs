//! Generations (docs/backups.md §3, §5): a base copy of a database at one
//! destination, and the segments sent on top of it.
//!
//! A base copy pauses the database's checkpoints, so its file doesn't
//! change while it is read: writes go only to the log, whose frames become
//! the segments after it. The file is read in chunks, each named by a
//! keyed hash of its content, and only chunks the destination doesn't have
//! are sent.

use std::collections::BTreeSet;
use std::io::Read;

use anyhow::Context;
use c6_api::{BackupDestination, BackupSettings};
use c6_core::now_ms;
use serde::{Deserialize, Serialize};
use x25519_dalek::PublicKey;

use super::crypt::{self, HeadKey};
use super::dest::Objects;
use super::{capture, wal};
use crate::HeadState;

/// A base copy's chunk size.
pub const CHUNK: usize = 4 << 20;
/// A base copy gives up (to try again later, keeping the chunks it sent)
/// when the database's log grows past this meanwhile.
const MAX_LOG_DURING_BASE: u64 = 1 << 30;

/// How long to wait after a base copy failed partway.
const BASE_RETRY_MS: i64 = 5 * 60_000;

/// A destination's generation of one database.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DestGen {
    pub id: String,
    pub base_at: i64,
    /// The database's epoch it belongs to.
    pub epoch: u64,
    /// Segments after this one build on it.
    pub start_seq: u64,
    /// Sent up to this one.
    pub sent_seq: u64,
    /// Its base copy is all there.
    pub complete: bool,
    /// It fell too far behind (or broke): it takes no more segments.
    #[serde(default)]
    pub lost: bool,
    /// When it was lost (its changes have waited since).
    #[serde(default)]
    pub lost_at: Option<i64>,
    /// When a base copy last failed here, to wait before the next.
    #[serde(default)]
    pub failed_at: Option<i64>,
}

/// A generation's manifest (sealed to your key): what a restore needs.
#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub database: String,
    pub generation: String,
    pub base_at: i64,
    pub start_seq: u64,
    pub page_size: u32,
    pub size: u64,
    pub chunk_size: usize,
    pub chunks: Vec<String>,
}

/// What the head reads back itself (sealed with its own key): the chunks a
/// generation uses, for pruning, and each object's ciphertext hash, for
/// checks without your key.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    pub chunks: Vec<String>,
    /// Object name → SHA-256 of what was stored.
    pub hashes: std::collections::BTreeMap<String, String>,
}

pub fn db_key(key: &HeadKey, rel: &str) -> String {
    key.name("db", rel.as_bytes())
}

fn sha(b: &[u8]) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b))
}

/// Whether `rel` needs a new base copy at this destination.
fn needs_base(st: &HeadState, s: &BackupSettings, d: &BackupDestination, rel: &str, force: bool) -> bool {
    let (g, epoch) = st.backups.with(st, |state| {
        (state.dests.get(&d.name).and_then(|x| x.gens.get(rel)).cloned(), state.dbs.get(rel).map(|x| x.epoch).unwrap_or(0))
    });
    let Some(g) = g else { return true };
    // After a base copy failed partway, wait a while before reading the
    // database again (one that couldn't reach the destination read nothing).
    if g.failed_at.is_some_and(|t| now_ms() - t < BASE_RETRY_MS) && !force {
        return false;
    }
    let why = if force {
        "asked"
    } else if !g.complete {
        "unfinished"
    } else if g.lost {
        "fell behind"
    } else if g.epoch != epoch {
        "new epoch"
    } else if now_ms() - g.base_at > s.generation_days as i64 * 86_400_000 {
        "due"
    } else {
        return false;
    };
    tracing::info!(database = rel, destination = %d.name, why, "a new base copy");
    true
}

/// Takes a base copy of `rel` for one destination.
async fn base_copy(st: &HeadState, d: &BackupDestination, o: &dyn Objects, to: &PublicKey, key: &HeadKey, rel: &str) -> anyhow::Result<()> {
    let have: BTreeSet<String> =
        o.list("chunks/").await?.into_iter().filter_map(|l| l.name.strip_prefix("chunks/").map(str::to_owned)).collect();
    let start = capture::begin_base(st, rel).await?;
    let base_at = now_ms();
    let id = format!("{base_at:013}-{:04x}", rand::random::<u16>());
    st.backups.with(st, |state| {
        let ds = state.dests.entry(d.name.clone()).or_default();
        let failed_at = ds.gens.get(rel).and_then(|g| g.failed_at);
        ds.gens.insert(
            rel.to_owned(),
            DestGen {
                id: id.clone(),
                base_at,
                epoch: start.epoch,
                start_seq: start.start_seq,
                sent_seq: start.start_seq,
                complete: false,
                lost: false,
                lost_at: None,
                failed_at,
            },
        );
    });
    let r = async {
        let path = st.data_dir.join(rel);
        let dbk = db_key(key, rel);
        let mut f = std::fs::File::open(&path).with_context(|| format!("reading {}", path.display()))?;
        let size = f.metadata()?.len();
        let mut index = Index::default();
        let mut chunks = Vec::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let mut n = 0;
            while n < CHUNK {
                let k = f.read(&mut buf[n..])?;
                if k == 0 {
                    break;
                }
                n += k;
            }
            if n == 0 {
                break;
            }
            let name = key.name("chunk", &buf[..n]);
            if !have.contains(&name) && !index.hashes.contains_key(&format!("chunks/{name}")) {
                let sealed = crypt::seal(to, &buf[..n]);
                index.hashes.insert(format!("chunks/{name}"), sha(&sealed));
                st.backups.sent(&d.name, sealed.len());
                o.put(&format!("chunks/{name}"), sealed).await?;
            }
            chunks.push(name);
            anyhow::ensure!(
                capture::log_size(st, rel) < MAX_LOG_DURING_BASE,
                "its log grew past {} MB during the base copy; trying again later",
                MAX_LOG_DURING_BASE >> 20
            );
            if n < CHUNK {
                break;
            }
        }
        index.chunks = chunks.clone();
        let wal0 = crypt::seal(to, &wal::encode(start.page_size.max(1), &start.wal0));
        index.hashes.insert(format!("db/{dbk}/{id}/wal0"), sha(&wal0));
        o.put(&format!("db/{dbk}/{id}/wal0"), wal0).await?;
        let page_size = if start.page_size > 0 { start.page_size } else { page_size_of(&path)? };
        let m = Manifest {
            database: rel.to_owned(),
            generation: id.clone(),
            base_at,
            start_seq: start.start_seq,
            page_size,
            size,
            chunk_size: CHUNK,
            chunks,
        };
        let manifest = crypt::seal(to, &serde_json::to_vec(&m)?);
        index.hashes.insert(format!("db/{dbk}/{id}/manifest"), sha(&manifest));
        o.put(&format!("db/{dbk}/{id}/index"), key.seal_index(&serde_json::to_vec(&index)?)).await?;
        // Written last: a generation with a manifest is complete.
        o.put(&format!("db/{dbk}/{id}/manifest"), manifest).await?;
        anyhow::Ok(())
    }
    .await;
    capture::resume(st, rel);
    st.backups.with(st, |state| {
        if let Some(g) = state.dests.get_mut(&d.name).and_then(|x| x.gens.get_mut(rel)) {
            match &r {
                Ok(()) => {
                    g.complete = true;
                    g.failed_at = None;
                }
                Err(_) => g.failed_at = Some(now_ms()),
            }
        }
    });
    r
}

/// A database file's page size, from its header.
fn page_size_of(path: &std::path::Path) -> anyhow::Result<u32> {
    let mut h = [0u8; 100];
    std::fs::File::open(path)?.read_exact(&mut h)?;
    let v = u16::from_be_bytes([h[16], h[17]]) as u32;
    Ok(if v == 1 { 65536 } else { v })
}

/// Sends a database's spooled segments that this destination's generation
/// still needs.
async fn send_segments(
    st: &HeadState,
    s: &BackupSettings,
    d: &BackupDestination,
    o: &dyn Objects,
    to: &PublicKey,
    key: &HeadKey,
    rel: &str,
) -> anyhow::Result<()> {
    let (g, epoch) = st.backups.with(st, |state| {
        (state.dests.get(&d.name).and_then(|x| x.gens.get(rel)).cloned(), state.dbs.get(rel).map(|x| x.epoch).unwrap_or(0))
    });
    let Some(g) = g.filter(|g| g.complete && !g.lost && g.epoch == epoch) else { return Ok(()) };
    let dbk = db_key(key, rel);
    let pending: Vec<(u64, i64, std::path::PathBuf)> =
        capture::spooled(&capture::spool_dir(st, rel, epoch)).into_iter().filter(|(seq, _, _)| *seq > g.sent_seq).collect();
    // Too far behind: it starts again from a new base copy, and stops
    // holding the spool.
    let bytes: u64 = pending.iter().map(|(_, _, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
    if bytes > s.spool_limit_mb * (1 << 20) {
        st.backups.with(st, |state| {
            if let Some(g) = state.dests.get_mut(&d.name).and_then(|x| x.gens.get_mut(rel)) {
                g.lost = true;
                g.lost_at.get_or_insert(now_ms());
            }
        });
        anyhow::bail!("{rel}: more than {} MB of changes waited to be sent; a new base copy follows", s.spool_limit_mb);
    }
    for (seq, ms, path) in pending {
        let plain = std::fs::read(&path)?;
        let sealed = crypt::seal(to, &plain);
        st.backups.sent(&d.name, sealed.len());
        o.put(&format!("db/{dbk}/{}/seg/{seq:012}-{ms:013}", g.id), sealed).await?;
        st.backups.with(st, |state| {
            if let Some(g) = state.dests.get_mut(&d.name).and_then(|x| x.gens.get_mut(rel)) {
                g.sent_seq = seq;
            }
        });
    }
    Ok(())
}

/// Removes spooled segments no destination still needs.
pub fn clean_spool(st: &HeadState, s: &BackupSettings) {
    let enabled: BTreeSet<&str> = s.destinations.iter().filter(|d| d.enabled).map(|d| d.name.as_str()).collect();
    let state = st.backups.with(st, |state| state.clone());
    for (rel, ds) in &state.dbs {
        let holding: Vec<u64> = state
            .dests
            .iter()
            .filter(|(n, _)| enabled.contains(n.as_str()))
            .filter_map(|(_, x)| x.gens.get(rel))
            .filter(|g| !g.lost && g.epoch == ds.epoch)
            .map(|g| g.sent_seq)
            .collect();
        let upto = holding.iter().copied().min().unwrap_or(ds.seq);
        for (seq, _, path) in capture::spooled(&capture::spool_dir(st, rel, ds.epoch)) {
            if seq <= upto {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Keeps the newest `keep` complete generations per database at a
/// destination, and the chunks and small files they need.
async fn retain(st: &HeadState, d: &BackupDestination, o: &dyn Objects, key: &HeadKey) -> anyhow::Result<()> {
    let all = o.list("").await?;
    // db/<dbk>/<gen>/…
    let mut gens: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    let mut complete: BTreeSet<(String, String)> = BTreeSet::new();
    for l in &all {
        let parts: Vec<&str> = l.name.split('/').collect();
        if parts.len() >= 4 && parts[0] == "db" {
            gens.entry(parts[1].to_owned()).or_default().insert(parts[2].to_owned());
            if parts[3] == "manifest" {
                complete.insert((parts[1].to_owned(), parts[2].to_owned()));
            }
        }
    }
    let mut drop_gens: BTreeSet<(String, String)> = BTreeSet::new();
    let mut kept: Vec<(String, String)> = Vec::new();
    // Generations still being made (no manifest yet) stay, unless an
    // older complete one... they're newer than every complete one we drop.
    let in_progress: BTreeSet<String> =
        st.backups.with(st, |state| state.dests.get(&d.name).map(|x| x.gens.values().map(|g| g.id.clone()).collect()).unwrap_or_default());
    for (dbk, ids) in &gens {
        let done: Vec<&String> = ids.iter().filter(|id| complete.contains(&(dbk.clone(), (*id).clone()))).collect();
        let keep_from = done.len().saturating_sub(d.keep.max(1) as usize);
        let newest_complete = done.last().map(|s| (*s).clone());
        for (i, id) in done.iter().enumerate() {
            if i < keep_from {
                drop_gens.insert((dbk.clone(), (*id).clone()));
            } else {
                kept.push((dbk.clone(), (*id).clone()));
            }
        }
        // Unfinished ones older than the newest complete one, and not being made.
        for id in ids.iter().filter(|id| !complete.contains(&(dbk.clone(), (*id).clone()))) {
            if newest_complete.as_ref().is_some_and(|n| id < n) && !in_progress.contains(id) {
                drop_gens.insert((dbk.clone(), id.clone()));
            }
        }
    }
    // The chunks kept generations use.
    let mut used: BTreeSet<String> = BTreeSet::new();
    for (dbk, id) in &kept {
        let Some(b) = o.get(&format!("db/{dbk}/{id}/index")).await? else { continue };
        let i: Index = serde_json::from_slice(&key.open_index(&b)?)?;
        used.extend(i.chunks);
    }
    // A base copy in progress sends chunks before its index: keep chunks
    // newer than this pass's start if any generation is being made.
    let making = st.backups.with(st, |state| state.dests.get(&d.name).is_some_and(|x| x.gens.values().any(|g| !g.complete)));
    let oldest_kept = kept.iter().map(|(_, id)| id.clone()).min();
    let (mut bytes, mut objects) = (0u64, 0u64);
    for l in &all {
        let parts: Vec<&str> = l.name.split('/').collect();
        let gone = match parts.as_slice() {
            ["db", dbk, id, ..] => drop_gens.contains(&((*dbk).to_owned(), (*id).to_owned())),
            ["chunks", name] => !making && !used.contains(*name),
            // Small files older than the oldest kept generation, but the newest.
            ["files", t] => {
                oldest_kept.as_ref().is_some_and(|o| *t < &o[..13])
                    && all.iter().rfind(|x| x.name.starts_with("files/")).is_some_and(|x| x.name != l.name)
            }
            _ => false,
        };
        if gone {
            o.delete(&l.name).await?;
        } else {
            bytes += l.size;
            objects += 1;
        }
    }
    st.backups.with(st, |state| {
        let ds = state.dests.entry(d.name.clone()).or_default();
        ds.stored_bytes = bytes;
        ds.stored_objects = objects;
    });
    Ok(())
}

/// A destination's databases: base copies where needed, then segments,
/// then pruning after any base copy.
pub async fn send(
    st: &HeadState,
    s: &BackupSettings,
    d: &BackupDestination,
    o: &dyn Objects,
    to: &PublicKey,
    key: &HeadKey,
    force: bool,
) -> anyhow::Result<()> {
    let mut copied = false;
    let mut first_error = None;
    for rel in capture::databases(&st.data_dir) {
        if needs_base(st, s, d, &rel, force) {
            match base_copy(st, d, o, to, key, &rel).await {
                Ok(()) => copied = true,
                Err(e) => {
                    first_error.get_or_insert(e.context(format!("base copy of {rel}")));
                    continue;
                }
            }
        }
        if let Err(e) = send_segments(st, s, d, o, to, key, &rel).await {
            first_error.get_or_insert(e);
        }
    }
    if copied {
        retain(st, d, o, key).await?;
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
