//! Extraction (docs/knowledge.md §4): the knowledge store never parses
//! what comes from outside. Mail, documents, photos and videos are queued
//! here (`extractions`), sent in batches to the c6-extract kit on a worker
//! (through the head's `Extractor`), and what comes back is kept: text,
//! metadata, thumbnails, faces, keyframes. Files in a watched folder stay
//! on their worker and are read there; everything else is a blob here.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
#[cfg(feature = "in-process-extract")]
use serde_json::json;
use sqlx::Row;
use tracing::{info, warn};

use crate::Memex;

/// A file for a job: bytes the store holds, or a file on a worker
/// (`worker` empty: the worker on the head's machine).
pub enum File {
    Bytes(Vec<u8>),
    At { worker: String, path: String },
}

pub struct Job {
    pub id: String,
    /// Its name (the extension says what it is).
    pub name: String,
    pub file: File,
    /// `attachment`: which.
    pub index: Option<u64>,
    /// `video`: its sound too.
    pub sound: bool,
}

/// One kit run: a tool and its jobs, all from one place.
pub struct Request {
    pub tool: String,
    pub jobs: Vec<Job>,
    /// `faces`: the detector and embedder models.
    pub models: Option<(Vec<u8>, Vec<u8>)>,
}

#[derive(Default)]
pub struct Response {
    /// Each job's result (or `{"error"}`), by id.
    pub results: serde_json::Map<String, Value>,
    /// The files jobs made, as `<id>/<name>`.
    pub files: BTreeMap<String, Vec<u8>>,
}

/// What reads files for the store: the c6-extract kit on workers (the
/// head's), or the same code in this process (tests).
#[async_trait::async_trait]
pub trait Extractor: Send + Sync {
    async fn extract(&self, req: Request) -> anyhow::Result<Response>;
    /// The files under a folder on a worker (`~` is its home): the
    /// folder's full path, and each file's (path, size, modified ms),
    /// hidden ones left out.
    async fn walk(&self, worker: &str, root: &str) -> anyhow::Result<(String, Vec<(String, u64, i64)>)>;
    /// A file on a worker, up to `max` bytes.
    async fn read(&self, worker: &str, path: &str, max: u64) -> anyhow::Result<Vec<u8>>;
}

static OVERRIDE: OnceLock<Arc<dyn Extractor>> = OnceLock::new();

/// Uses this extractor for every store in this process (tests).
pub fn set_override(e: Arc<dyn Extractor>) {
    let _ = OVERRIDE.set(e);
}

pub(crate) fn overridden() -> Option<Arc<dyn Extractor>> {
    OVERRIDE.get().cloned()
}

/// Where a file is: `head:<hash>` (a blob here), `worker:<worker>:<path>`,
/// or a bare path (on the head's machine, as folders were first kept).
pub fn file_at(m: &Memex, location: &str) -> anyhow::Result<File> {
    if let Some(hash) = location.strip_prefix("head:") {
        return Ok(File::Bytes(std::fs::read(m.blob_path(hash))?));
    }
    Ok(match location.strip_prefix("worker:").and_then(|r| r.split_once(':')) {
        Some((w, p)) => File::At { worker: w.to_owned(), path: p.to_owned() },
        None => File::At { worker: String::new(), path: location.to_owned() },
    })
}

/// The worker a location is on ("" for the head's machine; None: a blob).
fn worker_of(location: &str) -> Option<String> {
    if location.starts_with("head:") {
        return None;
    }
    Some(location.strip_prefix("worker:").and_then(|r| r.split_once(':')).map(|(w, _)| w.to_owned()).unwrap_or_default())
}

/// A folder file's location: its path on the head's machine, else on its worker.
pub fn location(worker: &str, path: &str) -> String {
    if worker.is_empty() { path.to_owned() } else { format!("worker:{worker}:{path}") }
}

/// The path part of a location.
pub fn path_of(location: &str) -> &str {
    location.strip_prefix("worker:").and_then(|r| r.split_once(':')).map(|(_, p)| p).unwrap_or(location)
}

/// Asks for a file to be read. `key` says what for (one job each: asking
/// again replaces the waiting one).
pub async fn queue(m: &Memex, key: &str, tool: &str, input: &str, name: &str, params: Value) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO extractions (key, tool, input, name, params, created_at) VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET tool = excluded.tool, input = excluded.input, name = excluded.name, params = excluded.params, \
         attempts = 0, next_at = 0, error = NULL",
    )
    .bind(key)
    .bind(tool)
    .bind(input)
    .bind(name)
    .bind(params.to_string())
    .bind(c6_core::now_ms())
    .execute(&m.store.pool)
    .await?;
    m.extract_wake.notify_one();
    Ok(())
}

pub async fn queued(m: &Memex, key: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM extractions WHERE key = ?").bind(key).fetch_one(&m.store.pool).await? > 0)
}

/// What a finished job kept: the item, and whether it was new
/// (`created`) or one already here (`duplicate`).
#[derive(Clone, Debug)]
pub struct Kept {
    pub id: String,
    pub status: &'static str,
}

impl From<String> for Kept {
    fn from(id: String) -> Self {
        Self { id, status: "created" }
    }
}

/// Waits (up to `wait`) for a job to be done: its outcome (what it kept,
/// or why not), or None while it waits.
pub async fn outcome(m: &Memex, key: &str, wait: Duration) -> anyhow::Result<Option<Result<Kept, String>>> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let done = m.outcomes.lock().unwrap().get(key).cloned();
        if let Some(o) = done {
            return Ok(Some(o));
        }
        if !queued(m, key).await? || tokio::time::Instant::now() >= deadline {
            return Ok(m.outcomes.lock().unwrap().get(key).cloned());
        }
        tokio::select! {
            _ = m.extracted.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

fn remember(m: &Memex, key: &str, o: Result<Kept, String>) {
    let mut out = m.outcomes.lock().unwrap();
    if out.len() > 2000 {
        out.clear();
    }
    out.insert(key.to_owned(), o);
}

/// The waiting jobs and the last trouble, for the status.
pub async fn status(m: &Memex) -> anyhow::Result<(i64, Option<String>)> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM extractions").fetch_one(&m.store.pool).await?;
    let e: Option<String> = sqlx::query_scalar("SELECT error FROM extractions WHERE error IS NOT NULL ORDER BY id LIMIT 1")
        .fetch_optional(&m.store.pool)
        .await?
        .flatten();
    Ok((n, e))
}

/// Jobs per kit run, and the bytes of the store's own files sent in one.
const BATCH: usize = 24;
const BATCH_BYTES: u64 = 24 * 1024 * 1024;

/// Runs waiting jobs in the background.
pub fn spawn(m: Arc<Memex>) {
    tokio::spawn(async move {
        loop {
            let worked = match step(&m).await {
                Ok(w) => w,
                Err(e) => {
                    warn!("extraction: {e:#}");
                    false
                }
            };
            if !worked {
                tokio::select! {
                    _ = m.extract_wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    });
}

/// One kit run, of the oldest waiting job's kind and place. Whether there was one.
async fn step(m: &Memex) -> anyhow::Result<bool> {
    let Some(ex) = m.extractor() else { return Ok(false) };
    let now = c6_core::now_ms();
    let Some(first) =
        sqlx::query("SELECT * FROM extractions WHERE next_at <= ? ORDER BY id LIMIT 1").bind(now).fetch_optional(&m.store.pool).await?
    else {
        return Ok(false);
    };
    let tool: String = first.get("tool");
    let place = worker_of(&first.get::<String, _>("input"));
    let rows = sqlx::query("SELECT * FROM extractions WHERE next_at <= ? AND tool = ? ORDER BY id LIMIT 200")
        .bind(now)
        .bind(&tool)
        .fetch_all(&m.store.pool)
        .await?;
    let mut batch = Vec::new();
    let mut jobs = Vec::new();
    let mut bytes = 0u64;
    for r in rows {
        let input: String = r.get("input");
        if worker_of(&input) != place {
            continue;
        }
        let file = match file_at(m, &input) {
            Ok(f) => f,
            Err(e) => {
                finish(m, &r, Err(format!("its file: {e:#}")), &BTreeMap::new()).await?;
                continue;
            }
        };
        if let File::Bytes(b) = &file {
            if !jobs.is_empty() && bytes + b.len() as u64 > BATCH_BYTES {
                break;
            }
            bytes += b.len() as u64;
        }
        let params: Value = serde_json::from_str(&r.get::<String, _>("params")).unwrap_or_default();
        jobs.push(Job {
            id: r.get::<i64, _>("id").to_string(),
            name: r.get("name"),
            file,
            index: params["index"].as_u64(),
            sound: params["sound"].as_bool().unwrap_or(false),
        });
        batch.push(r);
        if jobs.len() >= BATCH {
            break;
        }
    }
    if jobs.is_empty() {
        return Ok(true);
    }
    let models = if tool == "faces" {
        match m.face_models().await {
            Ok(Some(x)) => Some(x),
            Ok(None) => {
                // No models any more: nothing to look for.
                for r in &batch {
                    sqlx::query("DELETE FROM extractions WHERE id = ?").bind(r.get::<i64, _>("id")).execute(&m.store.pool).await?;
                }
                return Ok(true);
            }
            Err(e) => {
                put_off(m, &batch, &format!("face models: {e:#}")).await?;
                return Ok(false);
            }
        }
    } else {
        None
    };
    let n = jobs.len();
    match ex.extract(Request { tool: tool.clone(), jobs, models }).await {
        Ok(resp) => {
            for r in &batch {
                let id = r.get::<i64, _>("id").to_string();
                let result = match resp.results.get(&id) {
                    Some(v) if v.get("error").is_none() => Ok(v.clone()),
                    Some(v) => Err(v["error"].as_str().unwrap_or("could not read it").to_owned()),
                    None => Err("the kit gave nothing for it".to_owned()),
                };
                let prefix = format!("{id}/");
                let files: BTreeMap<String, Vec<u8>> =
                    resp.files.iter().filter_map(|(k, v)| k.strip_prefix(&prefix).map(|n| (n.to_owned(), v.clone()))).collect();
                finish(m, r, result, &files).await?;
            }
            m.extracted.notify_waiters();
            info!(%tool, jobs = n, "extracted");
            Ok(true)
        }
        Err(e) => {
            put_off(m, &batch, &format!("{e:#}")).await?;
            Ok(false)
        }
    }
}

/// Tries again later: a minute, doubling to an hour.
async fn put_off(m: &Memex, batch: &[sqlx::sqlite::SqliteRow], why: &str) -> anyhow::Result<()> {
    for r in batch {
        let attempts: i64 = r.get("attempts");
        let wait = (60_000i64 << attempts.min(6)).min(3_600_000);
        sqlx::query("UPDATE extractions SET attempts = attempts + 1, next_at = ?, error = ? WHERE id = ?")
            .bind(c6_core::now_ms() + wait)
            .bind(why)
            .bind(r.get::<i64, _>("id"))
            .execute(&m.store.pool)
            .await?;
    }
    *m.last_error.lock().unwrap() = Some(format!("extraction: {why}"));
    Ok(())
}

/// Keeps what a job found (or notes why not), and forgets the job.
async fn finish(
    m: &Memex,
    r: &sqlx::sqlite::SqliteRow,
    result: Result<Value, String>,
    files: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    let (key, tool, input, name): (String, String, String, String) = (r.get("key"), r.get("tool"), r.get("input"), r.get("name"));
    let params: Value = serde_json::from_str(&r.get::<String, _>("params")).unwrap_or_default();
    let job = Done { key: &key, input: &input, name: &name, params: &params, files };
    let kept = match result {
        Ok(v) => match tool.as_str() {
            "document" => m.document_from(&job, &v).await.map(Kept::from),
            "mail" => crate::ingest::mail_from(m, &job, &v).await,
            "media" => crate::media::add_from(m, &job, &v).await.map(Kept::from),
            "faces" => crate::faces::from(m, &job, &v).await.map(Kept::from),
            "video" => crate::media::video_from(m, &job, &v).await.map(Kept::from),
            other => Err(anyhow::anyhow!("no tool {other:?}")),
        }
        .map_err(|e| format!("{e:#}")),
        Err(e) => Err(e),
    };
    if let Err(e) = &kept {
        info!(%key, "not read: {e}");
        failed(m, &tool, &job, e).await?;
    }
    remember(m, &key, kept);
    sqlx::query("DELETE FROM extractions WHERE id = ?").bind(r.get::<i64, _>("id")).execute(&m.store.pool).await?;
    Ok(())
}

/// Where a job that could not be read says so.
async fn failed(m: &Memex, tool: &str, job: &Done<'_>, why: &str) -> anyhow::Result<()> {
    match tool {
        "document" | "media" if job.params["folder"].as_bool() == Some(true) => {
            let (mtime, size) = (job.params["mtime"].as_i64().unwrap_or(0), job.params["size"].as_i64().unwrap_or(0));
            m.store.set_file(job.input, mtime, size, None, Some(why)).await?;
        }
        "faces" | "video" => {
            let sql = if tool == "faces" {
                "UPDATE media SET faces_done = ?, error = ? WHERE item_id = ?"
            } else {
                "UPDATE media SET heard = ?, error = ? WHERE item_id = ?"
            };
            sqlx::query(sql)
                .bind(job.params["version"].as_str().unwrap_or_default())
                .bind(format!("{tool}: {why}"))
                .bind(job.params["item"].as_str().unwrap_or_default())
                .execute(&m.store.pool)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

/// A finished job: what it was for, and the files it made.
pub struct Done<'a> {
    pub key: &'a str,
    pub input: &'a str,
    pub name: &'a str,
    pub params: &'a Value,
    pub files: &'a BTreeMap<String, Vec<u8>>,
}

/// Reads one thing now (an attachment, say), not through the queue.
pub async fn now(m: &Memex, tool: &str, input: &str, name: &str, index: Option<u64>) -> anyhow::Result<(Value, BTreeMap<String, Vec<u8>>)> {
    let ex = m.extractor().ok_or_else(|| anyhow::anyhow!("nothing here reads files"))?;
    let job = Job { id: "0".into(), name: name.to_owned(), file: file_at(m, input)?, index, sound: false };
    let mut resp = ex.extract(Request { tool: tool.to_owned(), jobs: vec![job], models: None }).await?;
    let v = resp.results.remove("0").ok_or_else(|| anyhow::anyhow!("the kit gave nothing"))?;
    if let Some(e) = v.get("error") {
        anyhow::bail!("{}", e.as_str().unwrap_or("could not read it"));
    }
    let files = resp.files.into_iter().filter_map(|(k, v)| k.strip_prefix("0/").map(|n| (n.to_owned(), v))).collect();
    Ok((v, files))
}

#[cfg(feature = "in-process-extract")]
pub use in_process::InProcess;

/// c6-extract in this process, for tests: what the kit would do, on this
/// machine's files.
#[cfg(feature = "in-process-extract")]
mod in_process {
    use super::*;

    #[derive(Default)]
    pub struct InProcess {
        pub faces: Option<Arc<dyn c6_extract::faces::FaceModel>>,
    }

    #[async_trait::async_trait]
    impl Extractor for InProcess {
        async fn extract(&self, req: Request) -> anyhow::Result<Response> {
            let dir = std::env::temp_dir().join(format!("c6x-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(dir.join("in"))?;
            let mut jobs = Vec::new();
            for j in &req.jobs {
                let path = match &j.file {
                    File::Bytes(b) => {
                        let p = dir.join("in").join(&j.id);
                        std::fs::write(&p, b)?;
                        p.display().to_string()
                    }
                    File::At { path, .. } => path.clone(),
                };
                jobs.push(json!({"id": j.id, "name": j.name, "path": path, "index": j.index, "sound": j.sound}));
            }
            let request = json!({"tool": req.tool, "jobs": jobs});
            let (out, faces) = (dir.join("out"), self.faces.clone());
            let result = tokio::task::spawn_blocking(move || c6_extract::run(&request, &out, faces.as_deref())).await?;
            let mut resp = Response { results: result["results"].as_object().cloned().unwrap_or_default(), files: BTreeMap::new() };
            let mut stack = vec![dir.join("out")];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    if e.file_type()?.is_dir() {
                        stack.push(e.path());
                    } else {
                        let rel = e.path().strip_prefix(dir.join("out"))?.to_string_lossy().into_owned();
                        resp.files.insert(rel, std::fs::read(e.path())?);
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            if let Some(e) = result["error"].as_str() {
                anyhow::bail!("{e}");
            }
            Ok(resp)
        }

        async fn walk(&self, _worker: &str, root: &str) -> anyhow::Result<(String, Vec<(String, u64, i64)>)> {
            let root = c6_worker::expand_home(root).display().to_string();
            Ok((root.clone(), walk_here(&root)?))
        }

        async fn read(&self, _worker: &str, path: &str, max: u64) -> anyhow::Result<Vec<u8>> {
            anyhow::ensure!(std::fs::metadata(path)?.len() <= max, "larger than {} MB", max >> 20);
            Ok(std::fs::read(path)?)
        }
    }

    fn walk_here(root: &str) -> anyhow::Result<Vec<(String, u64, i64)>> {
        let root = std::path::PathBuf::from(root);
        anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if e.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(e.path());
                    continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                out.push((e.path().display().to_string(), meta.len(), mtime));
            }
        }
        Ok(out)
    }
}
