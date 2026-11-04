//! Restoring a data folder from a destination (docs/backups.md §6), to any
//! point in time it still keeps, and checking that it would.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, bail};
use sqlx::Connection;

use super::crypt::{self, HeadKey};
use super::dest::{Listed, Objects};
use super::gens::{Index, Manifest};
use super::{BlobEntry, wal};

/// What a restore put back.
#[derive(Debug, Default)]
pub struct Restored {
    /// Per database: its path, the base copy's time, segments replayed, and
    /// what `PRAGMA integrity_check` said.
    pub databases: Vec<(String, i64, usize, String)>,
    pub blobs: usize,
    /// Where the config file went, if it was kept.
    pub config: Option<std::path::PathBuf>,
    /// The time of the newest change put back.
    pub up_to: i64,
}

impl Restored {
    pub fn ok(&self) -> bool {
        !self.databases.is_empty() && self.databases.iter().all(|d| d.3 == "ok")
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for (db, base, segs, check) in &self.databases {
            out.push_str(&format!("{db}: base copy of {}, {segs} changes after it, integrity {check}\n", when(*base)));
        }
        out.push_str(&format!("{} blobs\n", self.blobs));
        if let Some(c) = &self.config {
            out.push_str(&format!("config file: {} (check its data_dir)\n", c.display()));
        }
        out.push_str(&format!("as of {}\n", when(self.up_to)));
        out
    }
}

fn when(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string()).unwrap_or_else(|| ms.to_string())
}

/// The head's key and yours, from a destination.
async fn keys(o: &dyn Objects, private: &str) -> anyhow::Result<(x25519_dalek::StaticSecret, HeadKey)> {
    let key = crypt::private_key(private)?;
    let recipient = o.get("keys/recipient").await?.context("no backups there (no keys/recipient)")?;
    if String::from_utf8_lossy(&recipient).trim() != crypt::public_of(private)? {
        bail!("these backups are encrypted to another key ({})", String::from_utf8_lossy(&recipient).trim());
    }
    let head = crypt::open(&key, &o.get("keys/head").await?.context("no keys/head there")?)?;
    let head: [u8; 32] = head.try_into().map_err(|_| anyhow::anyhow!("keys/head is not a key"))?;
    Ok((key, HeadKey(head)))
}

/// Time in a name's last part (`…/<seq>-<ms>`, `files/<ms>`, `bloblog/<ms>-…`).
fn name_ms(name: &str) -> Option<i64> {
    let last = name.rsplit('/').next()?;
    let seg = name.starts_with("seg/") || name.contains("/seg/");
    let ms = match last.split_once('-') {
        Some((_, b)) if seg => b,
        Some((a, _)) => a,
        None => last,
    };
    ms.parse().ok()
}

fn by_gen(all: &[Listed]) -> BTreeMap<(String, String), Vec<String>> {
    let mut out: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for l in all {
        let parts: Vec<&str> = l.name.splitn(4, '/').collect();
        if parts.len() == 4 && parts[0] == "db" {
            out.entry((parts[1].to_owned(), parts[2].to_owned())).or_default().push(parts[3].to_owned());
        }
    }
    out
}

/// Rebuilds a data folder in `to` (which must not hold a database) from
/// `o`, as of `at` (ms; `None`: the newest).
pub async fn restore(o: &dyn Objects, private: &str, at: Option<i64>, to: &Path) -> anyhow::Result<Restored> {
    anyhow::ensure!(!to.join("c6h.db").exists(), "{} already has a c6h.db: restore into an empty folder", to.display());
    let at = at.unwrap_or(i64::MAX);
    let (key, _head) = keys(o, private).await?;
    c6_core::create_private_dir(to)?;
    let all = o.list("").await?;
    let mut done = Restored::default();
    // Per database: the newest base copy no later than `at`.
    let mut chosen: BTreeMap<String, (Manifest, String, Vec<String>)> = BTreeMap::new();
    for ((dbk, id), parts) in by_gen(&all) {
        if !parts.iter().any(|p| p == "manifest") {
            continue;
        }
        let m: Manifest =
            serde_json::from_slice(&crypt::open(&key, &o.get(&format!("db/{dbk}/{id}/manifest")).await?.context("manifest")?)?)?;
        if m.base_at > at {
            continue;
        }
        if chosen.get(&m.database).is_some_and(|(c, _, _)| c.base_at >= m.base_at) {
            continue;
        }
        let mut segs: Vec<String> = parts.into_iter().filter(|p| p.starts_with("seg/")).collect();
        segs.sort();
        chosen.insert(m.database.clone(), (m, dbk, segs));
    }
    anyhow::ensure!(!chosen.is_empty(), "no base copy there from before {}", when(at));
    for (rel, (m, dbk, segs)) in &chosen {
        anyhow::ensure!(!rel.contains("..") && !rel.starts_with('/'), "a bad database path {rel:?}");
        let path = to.join(rel);
        std::fs::create_dir_all(path.parent().unwrap_or(to))?;
        let mut f = std::fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?;
        {
            use std::io::Write;
            for c in &m.chunks {
                let b = o.get(&format!("chunks/{c}")).await?.with_context(|| format!("{rel}: chunk {c} is missing"))?;
                f.write_all(&crypt::open(&key, &b)?)?;
            }
            f.set_len(m.size)?;
        }
        let mut up_to = m.base_at;
        let wal0 = crypt::open(&key, &o.get(&format!("db/{dbk}/{}/wal0", m.generation)).await?.context("wal0")?)?;
        let (ps, frames) = wal::decode(&wal0)?;
        wal::apply(&mut f, ps, &frames)?;
        let mut n = 0;
        for s in segs {
            let ms = name_ms(s).unwrap_or(0);
            if ms > at {
                break;
            }
            let b = o.get(&format!("db/{dbk}/{}/{s}", m.generation)).await?.with_context(|| format!("{rel}: {s} is missing"))?;
            let (ps, frames) = wal::decode(&crypt::open(&key, &b)?)?;
            wal::apply(&mut f, ps, &frames)?;
            n += 1;
            up_to = up_to.max(ms);
        }
        f.sync_all()?;
        drop(f);
        done.up_to = done.up_to.max(up_to);
        let check = integrity(&path).await.unwrap_or_else(|e| format!("{e:#}"));
        done.databases.push((rel.clone(), m.base_at, n, check));
    }
    // The CA, the master key and the config: the newest from before `at`
    // (or the first, if there is none that early).
    let files: Vec<&Listed> = all.iter().filter(|l| l.name.starts_with("files/")).collect();
    let pick = files.iter().rev().find(|l| name_ms(&l.name).is_some_and(|t| t <= at)).or(files.first());
    if let Some(l) = pick {
        use base64::Engine as _;
        let v: serde_json::Value = serde_json::from_slice(&crypt::open(&key, &o.get(&l.name).await?.context("files")?)?)?;
        for (rel, b) in v["files"].as_object().into_iter().flatten() {
            anyhow::ensure!(!rel.contains("..") && !rel.starts_with('/'), "a bad file path {rel:?}");
            let bytes = base64::engine::general_purpose::STANDARD.decode(b.as_str().unwrap_or_default())?;
            let path = to.join(rel);
            std::fs::create_dir_all(path.parent().unwrap_or(to))?;
            write_private(&path, &bytes)?;
        }
        if let (Some(name), Some(text)) = (v["config"]["name"].as_str(), v["config"]["text"].as_str()) {
            let path = to.join(format!("{name}.restored"));
            std::fs::write(&path, text)?;
            done.config = Some(path);
        }
    }
    // Blobs named in logs from before `at`.
    let blobs_dir = to.join("knowledge").join("blobs");
    for l in all.iter().filter(|l| l.name.starts_with("bloblog/") && name_ms(&l.name).is_some_and(|t| t <= at)) {
        let entries: Vec<BlobEntry> = serde_json::from_slice(&crypt::open(&key, &o.get(&l.name).await?.context("blob log")?)?)?;
        for e in entries {
            anyhow::ensure!(!e.path.contains("..") && !e.path.starts_with('/'), "a bad blob path");
            let path = blobs_dir.join(&e.path);
            if path.exists() {
                continue;
            }
            let b = o.get(&format!("blobs/{}", e.name)).await?.with_context(|| format!("blob {} is missing", e.path))?;
            std::fs::create_dir_all(path.parent().unwrap_or(&blobs_dir))?;
            std::fs::write(&path, crypt::open(&key, &b)?)?;
            done.blobs += 1;
        }
    }
    Ok(done)
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

async fn integrity(path: &Path) -> anyhow::Result<String> {
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite:")?.filename(path);
    let mut c = sqlx::SqliteConnection::connect_with(&opts).await?;
    let v: String = sqlx::query_scalar("PRAGMA integrity_check").fetch_one(&mut c).await?;
    // Leaves the file as a plain database: its log folded in.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&mut c).await?;
    c.close().await?;
    Ok(v)
}

/// Checks a destination without your key: each database's newest base
/// copy has every object its index names, as stored.
pub async fn verify(o: &dyn Objects, head: &HeadKey) -> anyhow::Result<String> {
    let all = o.list("").await?;
    let names: std::collections::BTreeSet<&str> = all.iter().map(|l| l.name.as_str()).collect();
    let mut newest: BTreeMap<String, String> = BTreeMap::new();
    for ((dbk, id), parts) in by_gen(&all) {
        if parts.iter().any(|p| p == "manifest") && parts.iter().any(|p| p == "index") {
            newest.insert(dbk, id);
        }
    }
    anyhow::ensure!(!newest.is_empty(), "no complete base copy there");
    let mut checked = 0;
    for (dbk, id) in &newest {
        let b = o.get(&format!("db/{dbk}/{id}/index")).await?.context("index")?;
        let i: Index = serde_json::from_slice(&head.open_index(&b)?)?;
        for c in &i.chunks {
            anyhow::ensure!(names.contains(format!("chunks/{c}").as_str()), "chunk {c} is missing");
        }
        // Every object is there; the manifest, the log and a sample of chunks
        // are read back and hashed (not every chunk: that would read the
        // whole backup back each time).
        let sample: Vec<(&String, &String)> =
            i.hashes.iter().filter(|(n, _)| !n.starts_with("chunks/") || rand::random::<u8>() < 16).collect();
        for name in i.hashes.keys() {
            anyhow::ensure!(names.contains(name.as_str()), "{name} is missing");
        }
        for (name, hash) in sample {
            let b = o.get(name).await?.with_context(|| format!("{name} is missing"))?;
            anyhow::ensure!(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&b)) == *hash, "{name} was changed");
            checked += 1;
        }
    }
    Ok(format!("{} databases' newest base copies are all there ({checked} objects checked by hash)", newest.len()))
}

/// A destination given on the command line: a folder, or
/// `s3://<bucket>/<prefix>` (keys from AWS_ACCESS_KEY_ID and
/// AWS_SECRET_ACCESS_KEY).
pub fn source(from: &str, endpoint: Option<&str>, region: Option<&str>) -> anyhow::Result<Box<dyn Objects>> {
    if let Some(rest) = from.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        let region = region.unwrap_or("us-east-1").to_owned();
        let endpoint = endpoint.map(str::to_owned).unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
        let env = |k: &str| std::env::var(k).with_context(|| format!("set {k} to read from S3"));
        let mut prefix = prefix.to_owned();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        c6_llm::install_crypto_provider();
        return Ok(Box::new(super::dest::S3 {
            endpoint,
            bucket: bucket.to_owned(),
            region,
            prefix,
            access_key: env("AWS_ACCESS_KEY_ID")?,
            secret_key: env("AWS_SECRET_ACCESS_KEY")?,
            http: reqwest::Client::new(),
        }));
    }
    let path = Path::new(from.strip_prefix("folder:").unwrap_or(from));
    anyhow::ensure!(path.is_dir(), "{} is not a folder (or s3://bucket/prefix)", path.display());
    Ok(Box::new(super::dest::Folder::new(path)))
}

/// A time on the command line: RFC 3339 (2026-11-04T09:30:00Z), a date
/// and time in UTC (2026-11-04 09:30), or milliseconds since 1970.
pub fn parse_time(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return Ok(t.and_utc().timestamp_millis());
        }
    }
    bail!("{s:?} is not a time (2026-11-04T09:30:00Z, 2026-11-04 09:30 in UTC, or ms since 1970)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_names() {
        assert_eq!(parse_time("1700000000000").unwrap(), 1_700_000_000_000);
        assert_eq!(parse_time("2023-11-14T22:13:20Z").unwrap(), 1_700_000_000_000);
        assert_eq!(parse_time("2023-11-14 22:13:20").unwrap(), 1_700_000_000_000);
        assert!(parse_time("yesterday").is_err());
        assert_eq!(name_ms("db/k/g/seg/000000000004-0001700000000123"), Some(1_700_000_000_123));
        assert_eq!(name_ms("files/0001700000000000"), Some(1_700_000_000_000));
        assert_eq!(name_ms("bloblog/0001700000000000-0a0b0c0d"), Some(1_700_000_000_000));
    }
}
