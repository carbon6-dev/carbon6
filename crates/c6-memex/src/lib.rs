//! The memex (`c6m`): carbon6's knowledge service. A node on the fabric
//! (it attaches to the head like a worker, with the role `memex`) that
//! keeps items, passages, vectors and a graph of things, and answers the
//! head's `memex.*` calls. See docs/memex.md.

pub mod calendar;
pub mod cards;
pub mod chat;
pub mod claims;
pub mod contacts;
pub mod embed;
mod explore;
pub mod extract;
pub mod faces;
pub mod ingest;
pub mod knowledge;
pub mod llm;
pub mod mail;
pub mod media;
pub mod people;
pub mod relations;
pub mod review;
pub mod store;

/// The realm everything in the memex belongs to, so far (docs/os.md §3).
pub const REALM_HOME: &str = "home";
pub mod briefs;
pub mod drafts;
pub mod requests;
pub mod triage;
pub mod understand;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api as api;
use c6_worker::link::HeadLink;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::store::{Store, clip};

/// Reciprocal-rank fusion constant: how much rank 1 beats rank 10.
const RRF_K: f64 = 60.0;
/// Candidates from each of keyword and meaning search.
const CANDIDATES: u32 = 60;

pub struct Memex {
    /// The realm whose knowledge this is (docs/os.md §3): home's memex
    /// holds every other realm's, each with its own database.
    pub realm: String,
    /// Other realms' memexes, opened when first used (home's only).
    realms: tokio::sync::Mutex<std::collections::HashMap<String, Arc<Memex>>>,
    pub store: Store,
    pub embed: embed::Embedder,
    link: Arc<HeadLink>,
    last_error: Mutex<Option<String>>,
    /// The memex's folder: its database and blobs.
    dir: std::path::PathBuf,
    /// Watched folders, from the head's settings.
    folders: Mutex<Vec<api::MemexFolder>>,
    /// The memex's prompts as edited (docs/os.md §4.6), by name.
    prompts: Mutex<std::collections::HashMap<String, String>>,
    /// When briefs come and what may interrupt (docs/brief.md §2.3).
    pub(crate) brief_setting: Mutex<api::BriefSetting>,
    /// Wakes the folder scanner (settings changed).
    rescan: tokio::sync::Notify,
    /// The accounts apps connected for mail, calendars and contacts, from
    /// the head's config.
    accounts: Mutex<Vec<people::Account>>,
    /// Add people you correspond with to your contacts (through the
    /// contacts app).
    contacts_auto: std::sync::atomic::AtomicBool,
    /// The models for understanding, vision and transcription, and model
    /// calls a day.
    tasks: Mutex<Tasks>,
    /// Wakes understanding and media work (their models changed).
    pub wake: tokio::sync::Notify,
    /// Re-reads the settings now (the head says they changed).
    reload: tokio::sync::Notify,
    /// Cities, for naming where photos were taken.
    pub places: Mutex<Option<media::Places>>,
    /// The face models' files (detector, embedder), and each person's faces.
    face_paths: Mutex<Option<(String, String)>>,
    pub(crate) face_index: Mutex<Option<faces::Index>>,
    /// Photos or videos came or went: make events again.
    pub media_changed: std::sync::atomic::AtomicBool,
    /// A folder scan is under way (events wait for it).
    pub scanning: std::sync::atomic::AtomicBool,
    /// What reads files (extract.rs): the head's kit jobs.
    extractor: std::sync::OnceLock<Arc<dyn extract::Extractor>>,
    /// Wakes the extraction queue; tells waiters jobs finished.
    extract_wake: tokio::sync::Notify,
    extracted: tokio::sync::Notify,
    /// Finished jobs' outcomes (an item, or why not), for waiters.
    outcomes: Mutex<HashMap<String, Result<extract::Kept, String>>>,
    /// Every message kept before 0.3 has been read again: no more looking.
    pub(crate) reread: std::sync::atomic::AtomicBool,
    /// What the reading loop's steps are working on now (a conversation, a
    /// request, a card, a thread), so steps side by side never take the same.
    claims: Mutex<std::collections::HashSet<String>>,
}

/// Work a reading step has taken (`Memex::claim`).
pub(crate) struct Claim<'a> {
    m: &'a Memex,
    what: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.m.claims.lock().unwrap().remove(&self.what);
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tasks {
    pub understanding: Option<llm::Task>,
    pub vision: Option<llm::Task>,
    pub transcription: Option<llm::Task>,
    pub per_day: u32,
    /// Model calls side by side (at least 1).
    pub at_once: u32,
}

fn task_of(v: &Value) -> Option<llm::Task> {
    Some(llm::Task {
        connection_id: v["connection_id"].as_str()?.to_owned(),
        model: v["model"].as_str().unwrap_or_default().to_owned(),
        effort: v["effort"].as_str().map(str::to_owned),
    })
}

/// Folder files larger than this are not read.
pub(crate) const MAX_FILE: u64 = 50 * 1024 * 1024;
/// The largest file the store holds itself (it goes to a worker whole).
pub(crate) const MAX_HELD: u64 = 40 * 1024 * 1024;
/// Documents in folders, by extension.
pub const DOC_EXTENSIONS: [&str; 9] = ["pdf", "docx", "md", "markdown", "txt", "text", "html", "htm", "rst"];

impl Memex {
    /// One of the memex's prompts, as edited, else its default.
    pub fn prompt(&self, name: &str, default: &str) -> String {
        self.prompts.lock().unwrap().get(name).filter(|p| !p.trim().is_empty()).cloned().unwrap_or_else(|| default.to_owned())
    }

    /// Home's memex (the one `c6m` runs), in `dir`.
    pub async fn open(dir: &Path, link: Arc<HeadLink>) -> anyhow::Result<Arc<Self>> {
        Self::open_in(dir, link, REALM_HOME).await
    }

    async fn open_in(dir: &Path, link: Arc<HeadLink>, realm: &str) -> anyhow::Result<Arc<Self>> {
        c6_core::create_private_dir(dir)?;
        let store = Store::open(&dir.join("memex.db")).await?;
        Ok(Arc::new(Self {
            realm: realm.to_owned(),
            realms: Default::default(),
            store,
            embed: embed::Embedder::new(link.clone()),
            link,
            last_error: Mutex::new(None),
            dir: dir.to_owned(),
            folders: Mutex::default(),
            rescan: tokio::sync::Notify::new(),
            accounts: Mutex::default(),
            contacts_auto: Default::default(),
            prompts: Default::default(),
            brief_setting: Default::default(),
            tasks: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            reload: tokio::sync::Notify::new(),
            places: Mutex::default(),
            face_paths: Mutex::default(),
            face_index: Mutex::default(),
            media_changed: Default::default(),
            scanning: Default::default(),
            extractor: Default::default(),
            extract_wake: tokio::sync::Notify::new(),
            extracted: tokio::sync::Notify::new(),
            outcomes: Default::default(),
            reread: Default::default(),
            claims: Default::default(),
        }))
    }

    /// Background work: keeps the settings fresh from the head, and embeds
    /// passages that have no vector yet.
    pub fn spawn_jobs(self: &Arc<Self>) {
        // Folders and photos are home's; every realm sends its own mail.
        if self.is_home() {
            self.spawn_scanner();
            media::spawn(self.clone());
        }
        self.spawn_mail();
        understand::spawn(self.clone());
        briefs::spawn(self.clone());
        extract::spawn(self.clone());
        let m = self.clone();
        tokio::spawn(async move {
            let mut refreshed: Option<std::time::Instant> = None;
            loop {
                if m.link.is_connected() && refreshed.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                    match m.link.call("config.get", json!({})).await {
                        Ok(c) => {
                            m.embed.set(c["embedding"]["connection_id"].as_str().map(|id| embed::Config {
                                connection_id: id.to_owned(),
                                model: c["embedding"]["model"].as_str().unwrap_or_default().to_owned(),
                            }));
                            let home = m.is_home();
                            let accounts: Vec<people::Account> =
                                serde_json::from_value(c["accounts"][&m.realm].clone()).unwrap_or_default();
                            *m.accounts.lock().unwrap() = accounts;
                            let tasks = Tasks {
                                understanding: task_of(&c["understanding"]),
                                vision: task_of(&c["vision"]),
                                transcription: task_of(&c["transcription"]),
                                per_day: c["calls_per_day"].as_u64().unwrap_or(500) as u32,
                                at_once: c["calls_at_once"].as_u64().unwrap_or(1).clamp(1, understand::MAX_AT_ONCE as u64) as u32,
                            };
                            if home {
                                let places = c["places_file"].as_str().filter(|p| !p.is_empty()).map(str::to_owned);
                                let current = m.places.lock().unwrap().as_ref().map(|p| p.file.clone());
                                if places != current {
                                    m.set_places(places).await;
                                }
                                faces::configure(
                                    &m,
                                    c["face_detector"].as_str().filter(|p| !p.is_empty()).map(str::to_owned),
                                    c["face_embedder"].as_str().filter(|p| !p.is_empty()).map(str::to_owned),
                                )
                                .await;
                            }
                            if *m.tasks.lock().unwrap() != tasks {
                                *m.tasks.lock().unwrap() = tasks;
                                m.wake.notify_waiters();
                            }
                            m.contacts_auto.store(c["contacts_auto"].as_bool().unwrap_or(false), std::sync::atomic::Ordering::Relaxed);
                            // The realm's own prompts (as it uses them), else home's.
                            let mine = match &c["realm_prompts"][&m.realm] {
                                Value::Null => c["prompts"].clone(),
                                p => p.clone(),
                            };
                            *m.prompts.lock().unwrap() = serde_json::from_value(mine).unwrap_or_default();
                            *m.brief_setting.lock().unwrap() = serde_json::from_value(c["brief"][&m.realm].clone()).unwrap_or_default();
                            let folders: Vec<api::MemexFolder> =
                                if home { serde_json::from_value(c["folders"].clone()).unwrap_or_default() } else { Vec::new() };
                            let changed = *m.folders.lock().unwrap() != folders;
                            *m.folders.lock().unwrap() = folders;
                            if changed {
                                m.rescan.notify_one();
                            }
                            refreshed = Some(std::time::Instant::now());
                        }
                        Err(e) => warn!("memex settings from the head: {e:#}"),
                    }
                }
                let wait = match m.embed_some().await {
                    Ok(0) => Duration::from_secs(2),
                    Ok(_) => Duration::from_millis(50),
                    Err(e) => {
                        warn!("embedding: {e:#}");
                        *m.last_error.lock().unwrap() = Some(format!("{e:#}"));
                        Duration::from_secs(30)
                    }
                };
                tokio::select! {
                    _ = m.reload.notified() => refreshed = None,
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        });
    }

    /// Sends mail when it is due, and (when you asked) adds people you
    /// write to to your contacts, through the apps that own the accounts.
    pub fn spawn_mail(self: &Arc<Self>) {
        let m = self.clone();
        tokio::spawn(async move {
            let mut round: u64 = 0;
            loop {
                if m.link.is_connected() {
                    if let Err(e) = mail::send_due(&m).await {
                        warn!("sending mail: {e:#}");
                    }
                    // Every ten minutes.
                    if m.contacts_auto()
                        && round.is_multiple_of(300)
                        && let Err(e) = contacts::write_back(&m).await
                    {
                        warn!("adding contacts: {e:#}");
                    }
                }
                round += 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    /// Calls an operation of the app an account belongs to (it talks to
    /// the provider, and what changed comes back through its ingest).
    pub async fn app_call(&self, account: &people::Account, operation: &str, input: Value, by: &str) -> anyhow::Result<Value> {
        anyhow::ensure!(!account.app.is_empty(), "no app owns the account {}", account.email);
        self.link.call("app.call", json!({"realm": self.realm, "app": account.app, "operation": operation, "input": input, "by": by})).await
    }

    /// Takes `what` for this step, unless another has it; given back when
    /// the claim is dropped.
    pub(crate) fn claim(&self, what: &str) -> Option<Claim<'_>> {
        self.claims.lock().unwrap().insert(what.to_owned()).then(|| Claim { m: self, what: what.to_owned() })
    }

    /// Whether another step has `what` now.
    pub(crate) fn claimed(&self, what: &str) -> bool {
        self.claims.lock().unwrap().contains(what)
    }

    pub fn tasks(&self) -> Tasks {
        self.tasks.lock().unwrap().clone()
    }

    /// Sets the model tasks (tests; normally from the head's settings).
    pub fn set_tasks(&self, t: Tasks) {
        *self.tasks.lock().unwrap() = t;
        self.wake.notify_waiters();
    }

    /// Reads files with this (the head's kit jobs).
    pub fn set_extractor(&self, e: Arc<dyn extract::Extractor>) {
        let _ = self.extractor.set(e);
        self.extract_wake.notify_one();
    }

    /// Tries waiting extraction now (here and in other realms): what reads
    /// files is ready.
    pub async fn extract_now(&self) {
        let _ = sqlx::query("UPDATE extractions SET next_at = 0").execute(&self.store.pool).await;
        self.extract_wake.notify_one();
        for m in self.realms.lock().await.values() {
            let _ = sqlx::query("UPDATE extractions SET next_at = 0").execute(&m.store.pool).await;
            m.extract_wake.notify_one();
        }
    }

    /// What reads files, if anything does yet.
    pub fn extractor(&self) -> Option<Arc<dyn extract::Extractor>> {
        extract::overridden().or_else(|| self.extractor.get().cloned())
    }

    /// The face models' files, if there are models.
    pub async fn face_models(&self) -> anyhow::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some((d, e)) = self.face_paths.lock().unwrap().clone() else { return Ok(None) };
        let read = |p: String| async move {
            let path = c6_worker::expand_home(&p);
            tokio::fs::read(&path).await.map_err(|err| anyhow::anyhow!("{}: {err}", path.display()))
        };
        Ok(Some((read(d).await?, read(e).await?)))
    }

    /// Loads a GeoNames file (None: no places), and places what is here.
    pub async fn set_places(&self, file: Option<String>) {
        let loaded = match &file {
            Some(f) => {
                let f2 = f.clone();
                match tokio::task::spawn_blocking(move || media::Places::load(&f2)).await {
                    Ok(Ok(p)) => {
                        info!(file = %f, places = p.len(), "places loaded");
                        Some(p)
                    }
                    Ok(Err(e)) => {
                        *self.last_error.lock().unwrap() = Some(format!("places: {e:#}"));
                        None
                    }
                    Err(e) => {
                        *self.last_error.lock().unwrap() = Some(format!("places: {e}"));
                        None
                    }
                }
            }
            None => None,
        };
        let any = loaded.is_some();
        *self.places.lock().unwrap() = loaded;
        if any && let Err(e) = media::place_all(self).await {
            warn!("placing photos: {e:#}");
        }
    }

    /// The understanding model and the day's budget, if there is one.
    pub fn understanding(&self) -> Option<(llm::Task, u32)> {
        let t = self.tasks();
        t.understanding.map(|u| (u, t.per_day))
    }

    pub fn accounts(&self) -> Vec<people::Account> {
        self.accounts.lock().unwrap().clone()
    }

    /// An account that feeds a store (mail, calendar, contacts), by id or
    /// address.
    pub fn account(&self, feeds: &str, id_or_email: &str) -> anyhow::Result<people::Account> {
        let accounts: Vec<people::Account> = self.accounts().into_iter().filter(|a| a.feeds.iter().any(|f| f == feeds)).collect();
        match accounts.iter().find(|a| a.id == id_or_email || a.email.eq_ignore_ascii_case(id_or_email)) {
            Some(a) => Ok(a.clone()),
            None if id_or_email.is_empty() && accounts.len() == 1 => Ok(accounts[0].clone()),
            None => anyhow::bail!("no {feeds} account {id_or_email:?} (connect one in Settings → Mail & calendar)"),
        }
    }

    /// A mail account to read or label: an app's, or an import source
    /// (`import:…`).
    pub fn mail_account(&self, id_or_email: &str) -> anyhow::Result<String> {
        if id_or_email.starts_with("import:") { Ok(id_or_email.to_owned()) } else { Ok(self.account("mail", id_or_email)?.id) }
    }

    pub fn contacts_auto(&self) -> bool {
        self.contacts_auto.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The item of a Gmail message, if it is here.
    pub async fn mail_item(&self, account: &str, gmail_id: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT item_id FROM mail WHERE account = ? AND gmail_id = ?")
            .bind(account)
            .bind(gmail_id)
            .fetch_optional(&self.store.pool)
            .await?)
    }

    /// Tells the head something happened (automations can start on it).
    pub async fn emit(&self, kind: &str, data: Value) {
        if let Err(e) = self.link.call("event", json!({"type": kind, "data": data, "realm": self.realm})).await {
            warn!(%kind, "telling the head: {e:#}");
        }
    }

    /// Scans watched folders every few minutes (and when they change).
    pub fn spawn_scanner(self: &Arc<Self>) {
        let m = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = m.rescan.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                }
                let folders: Vec<api::MemexFolder> = m.folders.lock().unwrap().clone();
                for f in folders.iter().filter(|f| f.enabled) {
                    if let Err(e) = m.scan(f).await {
                        warn!(folder = %f.path, "scanning: {e:#}");
                        *m.last_error.lock().unwrap() = Some(format!("{}: {e:#}", f.path));
                    }
                }
            }
        });
    }

    /// Reads new and changed files in a folder, and forgets removed ones.
    pub async fn scan(&self, f: &api::MemexFolder) -> anyhow::Result<usize> {
        self.scanning.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = self.scan_inner(f).await;
        self.scanning.store(false, std::sync::atomic::Ordering::Relaxed);
        self.wake.notify_waiters();
        r
    }

    async fn scan_inner(&self, f: &api::MemexFolder) -> anyhow::Result<usize> {
        let ex = self.extractor().ok_or_else(|| anyhow::anyhow!("nothing reads folders yet"))?;
        let (root, files) = ex.walk(&f.worker, &f.path).await?;
        let started = c6_core::now_ms();
        let mut n = 0;
        for (path, size, mtime) in files {
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
            let media_kind = media::kind_of(&ext);
            if media_kind.is_none() && (!DOC_EXTENSIONS.contains(&ext.as_str()) || size > MAX_FILE) {
                continue;
            }
            let at = extract::location(&f.worker, &path);
            if self.store.file_state(&at).await? == Some((mtime, size as i64)) {
                self.store.touch_file(&at).await?;
                continue;
            }
            self.store.touch_file(&at).await?;
            let key = format!("folder:{at}");
            if extract::queued(self, &key).await? {
                continue;
            }
            let params = json!({"folder": true, "worker": f.worker, "mtime": mtime, "size": size, "labels": f.labels, "kind": media_kind});
            extract::queue(self, &key, if media_kind.is_some() { "media" } else { "document" }, &at, &name, params).await?;
            n += 1;
        }
        for (path, item) in self.store.files_gone(&extract::location(&f.worker, &root), started).await? {
            if let Some(id) = item {
                self.store.delete(&id).await?;
                self.media_changed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            self.store.forget_file(&path).await?;
        }
        if n > 0 {
            info!(folder = %f.path, files = n, "folder files to read");
        }
        Ok(n)
    }

    /// Stores a file's bytes by their hash; the hash.
    pub async fn put_blob(&self, data: &[u8]) -> anyhow::Result<String> {
        let hash = c6_core::sha256_hex(data);
        let path = self.blob_path(&hash);
        if !path.exists() {
            tokio::fs::create_dir_all(path.parent().expect("blob dir")).await?;
            let tmp = path.with_extension("tmp");
            tokio::fs::write(&tmp, data).await?;
            tokio::fs::rename(&tmp, &path).await?;
        }
        Ok(hash)
    }

    pub fn blob_path(&self, hash: &str) -> std::path::PathBuf {
        self.dir.join("blobs").join(&hash[..2]).join(&hash[2..])
    }

    /// Keeps a document (or paper) the kit read: its text, and for a paper
    /// its identity, authors and citations as links. Its id.
    pub async fn document_from(&self, job: &extract::Done<'_>, v: &Value) -> anyhow::Result<String> {
        let p = job.params;
        let name = job.name;
        let text = v["text"].as_str().unwrap_or_default().to_owned();
        anyhow::ensure!(!text.trim().is_empty(), "no text in {name} (a scan? images are read in a later step)");
        let folder = p["folder"].as_bool() == Some(true);
        let (source, external_id) = if folder {
            ("folder", job.input)
        } else {
            (p["source"].as_str().unwrap_or("upload"), p["external_id"].as_str().unwrap_or_default())
        };
        let paper = v["paper"].as_object().filter(|_| v["paper"]["key"].is_string());
        let title = paper
            .and_then(|p| p["title"].as_str())
            .filter(|t| !t.is_empty())
            .or(v["title"].as_str().filter(|t| !t.is_empty()))
            .map(str::to_owned)
            .unwrap_or_else(|| name.rsplit('/').next().unwrap_or(name).to_owned());
        let kind = if paper.is_some() { "paper" } else { "document" };
        let mut meta = json!({"name": name, "size": p["size"], "mime": mime_of(name)});
        if let Some(b) = job.input.strip_prefix("head:") {
            meta["blob"] = json!(b);
        }
        if folder {
            meta["path"] = json!(extract::path_of(job.input));
            if let Some(w) = p["worker"].as_str().filter(|w| !w.is_empty()) {
                meta["worker"] = json!(w);
            }
        }
        if let Some(p) = paper {
            meta["paper"] = json!({"doi": p["doi"], "arxiv": p["arxiv"], "authors": p["authors"], "year": p["year"],
                "references": p["references"].as_array().map_or(0, Vec::len)});
        }
        let labels: Vec<String> = serde_json::from_value(p["labels"].clone()).unwrap_or_default();
        let item = api::MemexItemIn {
            kind: kind.into(),
            source: source.into(),
            external_id: external_id.into(),
            title: title.clone(),
            text,
            created_at: p["created"].as_i64().or(p["mtime"].as_i64().filter(|_| folder)),
            labels,
            meta,
            ..Default::default()
        };
        let id = self.store.put(&item, source).await?;
        self.store.clear_links(&id, "docs").await?;
        if let Some(p) = paper {
            let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
            let me = self.store.ensure_thing(&s(&p["key"]), "paper", &s(&p["title"])).await?;
            self.store.link(&id, "is", &me, "observed", "docs", None).await?;
            for a in p["authors"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let key = format!("person:name:{}", a.to_lowercase().split_whitespace().collect::<Vec<_>>().join("-"));
                let person = self.store.ensure_thing(&key, "person", a).await?;
                self.store.link(&id, "authored_by", &person, "observed", "docs", None).await?;
            }
            for r in p["references"].as_array().into_iter().flatten() {
                let cited = self.store.ensure_thing(&s(&r["key"]), "paper", &s(&r["title"])).await?;
                if cited != me {
                    self.store.link(&id, "cites", &cited, "observed", "docs", Some(&s(&r["text"]))).await?;
                }
            }
        }
        if folder {
            self.store.set_file(job.input, p["mtime"].as_i64().unwrap_or(0), p["size"].as_i64().unwrap_or(0), Some(&id), None).await?;
        }
        Ok(id)
    }

    /// Embeds a batch of passages; how many.
    async fn embed_some(&self) -> anyhow::Result<usize> {
        let space = self.embed.space();
        if space.is_empty() || !self.link.is_connected() {
            return Ok(0);
        }
        let batch = self.store.unembedded(&space, 32).await?;
        if batch.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let vectors = self.embed.embed(&texts).await?;
        for ((part, _), v) in batch.iter().zip(vectors) {
            self.store.put_vector(*part, &space, &v).await?;
        }
        *self.last_error.lock().unwrap() = None;
        Ok(batch.len())
    }

    /// The kinds a call may see: its scope (stamped by the head for
    /// sessions), else everything (you, through the UI).
    fn allowed(params: &Value) -> Vec<String> {
        match params["scope"].as_array() {
            Some(a) => a.iter().filter_map(|k| k.as_str().map(str::to_owned)).collect(),
            None => api::MEMEX_KINDS.iter().map(|k| k.to_string()).collect(),
        }
    }

    /// Forgets a deleted realm's knowledge: its database and blobs.
    async fn drop_realm(&self, realm: &str) -> anyhow::Result<Value> {
        anyhow::ensure!(self.is_home() && realm != REALM_HOME, "home's knowledge stays");
        if let Some(m) = self.realms.lock().await.remove(realm) {
            m.store.pool.close().await;
        }
        let dir = self.dir.join("realms").join(realm);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        info!(%realm, "a realm's memex dropped");
        Ok(json!({"dropped": realm}))
    }

    pub fn is_home(&self) -> bool {
        self.realm == REALM_HOME
    }

    /// Another realm's memex (home's only): its own database under
    /// `realms/<realm>/`, opened and started when first used.
    async fn realm_memex(&self, realm: &str) -> anyhow::Result<Arc<Memex>> {
        anyhow::ensure!(self.is_home(), "only home's memex keeps other realms'");
        anyhow::ensure!(
            !realm.is_empty() && realm.len() <= 32 && realm.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{realm:?} is not a realm"
        );
        let mut realms = self.realms.lock().await;
        if let Some(m) = realms.get(realm) {
            return Ok(m.clone());
        }
        let m = Self::open_in(&self.dir.join("realms").join(realm), self.link.clone(), realm).await?;
        if let Some(e) = self.extractor.get() {
            m.set_extractor(e.clone());
        }
        m.spawn_jobs();
        info!(%realm, "a realm's memex opened");
        realms.insert(realm.to_owned(), m.clone());
        Ok(m)
    }

    pub async fn handle_method(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        // Every call is for one realm (the head stamps it): another realm's
        // goes to that realm's memex, with its own database, so nothing
        // crosses. Mail, calendars, contacts, photos and folders are home's.
        let realm = params["realm"].as_str().unwrap_or(&self.realm).to_owned();
        if realm != self.realm {
            // Each realm keeps its own mail, calendars and contacts (its apps'
            // accounts'); photos and folders are home's.
            const HOME_ONLY: [&str; 4] = ["memex.media.", "memex.legacy_sync", "memex.scan", "memex.upgrades"];
            if HOME_ONLY.iter().any(|p| method.starts_with(p)) {
                anyhow::bail!("{method} is home's (photos and folders belong to home)");
            }
            if method == "memex.drop_realm" {
                return self.drop_realm(&realm).await;
            }
            let other = self.realm_memex(&realm).await?;
            return Box::pin(other.handle_method(method, params)).await;
        }
        let allowed = Self::allowed(&params);
        match method {
            "memex.ingest" => ingest::ingest(self, params).await,
            // Claims (docs/apps.md §9): the head stamps the owner.
            "memex.claims.add" => claims::add(self, &params).await,
            "memex.claims.retract" => claims::retract(self, &params).await,
            "memex.claims.retract_owner" => claims::retract_owner(self, params["owner"].as_str().unwrap_or_default()).await,
            "memex.claims.list" => claims::list(self, &params).await,
            "memex.things.create" => claims::create_thing(self, &params).await,
            "memex.types" => Ok(json!(ingest::types())),
            "memex.upgrades" => Ok(json!(self.store.upgrades().await?)),
            "memex.delete" => {
                let id = match params["id"].as_str() {
                    Some(id) => Some(id.to_owned()),
                    None => self.store.id_of(params["source"].as_str().unwrap_or(""), params["external_id"].as_str().unwrap_or("")).await?,
                };
                let deleted = match id {
                    Some(id) => {
                        // A message gone from its mailbox leaves the mail store too.
                        sqlx::query("DELETE FROM mail WHERE item_id = ?").bind(&id).execute(&self.store.pool).await?;
                        sqlx::query("DELETE FROM mail_labels WHERE item_id = ?").bind(&id).execute(&self.store.pool).await?;
                        self.store.delete(&id).await?
                    }
                    None => false,
                };
                Ok(json!({"deleted": deleted}))
            }
            "memex.search" => {
                let q: api::MemexQuery = serde_json::from_value(params.clone())?;
                Ok(json!(self.search(&q, &allowed).await?))
            }
            "memex.get" => {
                let id = params["id"].as_str().unwrap_or_default();
                if let Some(item) = self.store.item(id, &allowed).await? {
                    return Ok(json!({"item": item}));
                }
                match self.thing(id, &allowed).await? {
                    Some(t) => Ok(json!({"thing": t})),
                    None => anyhow::bail!("nothing in the memex is {id:?} (or it is outside what this session may see)"),
                }
            }
            "memex.related" => {
                let id = params["id"].as_str().unwrap_or_default();
                let thing = self.store.thing(id).await?.ok_or_else(|| anyhow::anyhow!("no thing {id:?}"))?;
                Ok(json!(self.store.related(&thing.id, &allowed, 20).await?))
            }
            "memex.timeline" => {
                let id = params["about"].as_str().unwrap_or_default();
                let thing = self.store.thing(id).await?.ok_or_else(|| anyhow::anyhow!("no thing {id:?}"))?;
                let limit = params["limit"].as_u64().unwrap_or(30).min(200) as u32;
                Ok(json!(self.store.about(Some(&thing.id), &allowed, limit).await?))
            }
            "memex.note" => {
                anyhow::ensure!(allowed.iter().any(|k| k == "note"), "notes are outside what this session may write");
                let text = params["text"].as_str().unwrap_or_default().trim().to_owned();
                anyhow::ensure!(!text.is_empty(), "a note needs text");
                let labels: Vec<String> = ["labels", "about"]
                    .iter()
                    .flat_map(|k| params[*k].as_array().cloned().unwrap_or_default())
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
                let item = api::MemexItemIn {
                    kind: "note".into(),
                    source: params["made_by"].as_str().unwrap_or("you").to_owned(),
                    external_id: String::new(),
                    title: clip(text.lines().next().unwrap_or(""), 80),
                    text,
                    labels,
                    ..Default::default()
                };
                let id = self.store.put(&item, &item.source).await?;
                Ok(json!({"id": id}))
            }
            "memex.brief" => {
                let labels: Vec<String> = params["labels"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                let chars = params["chars"].as_u64().unwrap_or(4000) as usize;
                Ok(json!(self.brief(&labels, params["text"].as_str().unwrap_or(""), chars, &allowed).await?))
            }
            "memex.upload" => {
                use base64::Engine;
                let u: api::MemexUpload = serde_json::from_value(params)?;
                let data = base64::engine::general_purpose::STANDARD.decode(u.data.as_bytes())?;
                anyhow::ensure!(data.len() as u64 <= MAX_HELD, "files up to {} MB", MAX_HELD >> 20);
                let hash = c6_core::sha256_hex(&data);
                // Through the document handler: a file already here is that item.
                let ext = u.name.rsplit('.').next().unwrap_or("").to_lowercase();
                let mut r = ingest::document(self, "upload", &hash, &u.name, &ext, &data, &u.labels, None).await?;
                if r.status == "queued" {
                    // Read by the kit: wait a while for it.
                    match extract::outcome(self, &format!("doc:upload:{hash}"), std::time::Duration::from_secs(90)).await? {
                        Some(Ok(k)) => {
                            r.id = Some(k.id);
                            r.status = k.status.into();
                        }
                        Some(Err(e)) => anyhow::bail!("{e}"),
                        None => return Ok(json!({"status": "queued", "title": u.name})),
                    }
                }
                let id = r.id.unwrap_or_default();
                let (kind, title): (String, String) = sqlx::query_as("SELECT kind, title FROM items WHERE id = ?")
                    .bind(&id)
                    .fetch_optional(&self.store.pool)
                    .await?
                    .unwrap_or_default();
                Ok(json!({"id": id, "kind": kind, "title": title, "status": r.status}))
            }
            "memex.content" => {
                use base64::Engine;
                let id = params["id"].as_str().unwrap_or_default();
                let item = self.store.item(id, &allowed).await?.ok_or_else(|| anyhow::anyhow!("no item {id:?}"))?;
                let data = match (item.meta["blob"].as_str(), item.meta["path"].as_str()) {
                    (Some(b), _) => tokio::fs::read(self.blob_path(b)).await?,
                    // In a folder on its worker: read there.
                    (None, Some(p)) => {
                        let ex = self.extractor().ok_or_else(|| anyhow::anyhow!("nothing reads folders yet"))?;
                        ex.read(item.meta["worker"].as_str().unwrap_or_default(), p, MAX_HELD).await?
                    }
                    _ => item.text.into_bytes(),
                };
                Ok(json!({
                    "name": item.meta["name"].as_str().unwrap_or(&item.title),
                    "mime": item.meta["mime"].as_str().unwrap_or("text/plain"),
                    "data": base64::engine::general_purpose::STANDARD.encode(data),
                }))
            }
            // Items of a kind, for views (docs/knowledge.md §6.2, #7).
            "memex.items.list" => {
                let kind = params["kind"].as_str().unwrap_or_default();
                anyhow::ensure!(allowed.iter().any(|k| k == kind), "{kind} is outside what this caller may see");
                let asc = params["order"] == "asc";
                let sql = if asc {
                    "SELECT id, kind, title, substr(body, 1, 4000) AS text, created_at, source, meta FROM items WHERE kind = ? \
                     AND (? = '' OR source = ?) AND created_at >= ? AND created_at < ? ORDER BY created_at ASC LIMIT ?"
                } else {
                    "SELECT id, kind, title, substr(body, 1, 4000) AS text, created_at, source, meta FROM items WHERE kind = ? \
                     AND (? = '' OR source = ?) AND created_at >= ? AND created_at < ? ORDER BY created_at DESC LIMIT ?"
                };
                let rows = sqlx::query(sql)
                    .bind(kind)
                    .bind(params["source"].as_str().unwrap_or_default())
                    .bind(params["source"].as_str().unwrap_or_default())
                    .bind(params["from"].as_i64().unwrap_or(i64::MIN))
                    .bind(params["to"].as_i64().unwrap_or(i64::MAX))
                    .bind(params["limit"].as_u64().unwrap_or(100).min(500) as i64)
                    .fetch_all(&self.store.pool)
                    .await?;
                use sqlx::Row;
                Ok(json!(
                    rows.iter()
                        .map(|r| api::ViewItem {
                            id: r.get("id"),
                            kind: r.get("kind"),
                            title: r.get("title"),
                            text: r.get("text"),
                            created_at: r.get("created_at"),
                            source: r.get("source"),
                            meta: serde_json::from_str(&r.get::<String, _>("meta")).unwrap_or_default(),
                        })
                        .collect::<Vec<_>>()
                ))
            }
            // An item's picture: a photo's thumbnail, else the blob its
            // meta names (`thumb`).
            "memex.items.thumb" => {
                use base64::Engine;
                let id = params["id"].as_str().unwrap_or_default();
                let item = self.store.item(id, &allowed).await?.ok_or_else(|| anyhow::anyhow!("no item {id:?}"))?;
                let data = match media::thumb(self, id, None).await {
                    Ok(d) => d,
                    Err(_) => {
                        let hash = item.meta["thumb"].as_str().filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()));
                        tokio::fs::read(self.blob_path(hash.ok_or_else(|| anyhow::anyhow!("no picture for {id}"))?)).await?
                    }
                };
                Ok(json!({"mime": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(data)}))
            }
            "memex.trails" => Ok(json!(self.store.trails().await?)),
            "memex.trail" => {
                let id = params["id"].as_str().unwrap_or_default();
                let mut t = self.store.trail(id).await?.ok_or_else(|| anyhow::anyhow!("no trail {id:?}"))?;
                for s in t.steps.iter_mut() {
                    (s.kind, s.title) = self.store.describe(&s.target).await?;
                }
                Ok(json!(t))
            }
            "memex.trail_append" => {
                let trail = params["trail"].as_str().unwrap_or_default().trim().to_owned();
                let target = params["target"].as_str().unwrap_or_default().trim().to_owned();
                anyhow::ensure!(
                    !trail.is_empty() && !target.is_empty(),
                    "a trail step needs a trail (name or id) and a target (item id or thing)"
                );
                anyhow::ensure!(self.store.describe(&target).await?.0 != "missing", "nothing in the memex is {target:?}");
                let made_by = params["made_by"].as_str().unwrap_or("you");
                let id = self.store.trail_append(&trail, &target, params["comment"].as_str().unwrap_or(""), made_by).await?;
                Ok(json!({"id": id}))
            }
            "memex.trail_update" => {
                self.store
                    .trail_update(params["id"].as_str().unwrap_or_default(), params["name"].as_str(), params["description"].as_str())
                    .await?;
                Ok(json!({}))
            }
            "memex.trail_remove_step" => {
                self.store.trail_remove_step(params["step"].as_i64().unwrap_or(0)).await?;
                Ok(json!({}))
            }
            "memex.trail_delete" => {
                self.store.trail_delete(params["id"].as_str().unwrap_or_default()).await?;
                Ok(json!({}))
            }
            "memex.scan" => {
                let folders: Vec<api::MemexFolder> = self.folders.lock().unwrap().clone();
                let mut n = 0;
                for f in folders.iter().filter(|f| f.enabled) {
                    n += self.scan(f).await?;
                }
                Ok(json!({"read": n}))
            }
            m if m.starts_with("memex.mail.") || m.starts_with("memex.calendar.") || m.starts_with("memex.contacts.") => {
                self.google_method(m, params, &allowed).await
            }
            m if m.starts_with("memex.chat.") => chat::method(self, m, &params, &allowed).await,
            m if m.starts_with("memex.requests.") => {
                anyhow::ensure!(allowed.iter().any(|k| k == "email" || k == "chat"), "mail and chat are outside what this caller may see");
                requests::method(self, m, &params).await
            }
            m if m.starts_with("memex.relations.") => {
                anyhow::ensure!(allowed.iter().any(|k| k == "email" || k == "chat"), "mail and chat are outside what this caller may see");
                cards::method(self, m, &params).await
            }
            m if m.starts_with("memex.triage.") => {
                anyhow::ensure!(allowed.iter().any(|k| k == "email" || k == "chat"), "mail and chat are outside what this caller may see");
                triage::method(self, m, &params).await
            }
            // Which of the mail, calendar and contacts stores this realm
            // uses: an app feeds it, or it holds something (imported mail).
            "memex.feeds" => {
                let fed = |f: &str| self.accounts().iter().any(|a| a.feeds.iter().any(|x| x == f));
                let has = |sql: &'static str| {
                    let pool = self.store.pool.clone();
                    async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(&pool).await.unwrap_or(0) > 0 }
                };
                Ok(json!({
                    "mail": fed("mail") || has("SELECT EXISTS (SELECT 1 FROM mail)").await,
                    "calendar": fed("calendar") || has("SELECT EXISTS (SELECT 1 FROM events)").await,
                    "contacts": fed("contacts") || has("SELECT EXISTS (SELECT 1 FROM contacts)").await,
                    "chat": fed("chat") || has("SELECT EXISTS (SELECT 1 FROM items WHERE kind = 'chat')").await,
                }))
            }
            // Where the memex's own Google sync was, for moving it to apps.
            "memex.legacy_sync" => Ok(json!(people::legacy_states(&self.store).await?)),
            "memex.move_google" => people::move_to_apps(self, &params["accounts"]).await.map(|n| json!({"moved": n})),
            "memex.commitments" => Ok(json!(understand::commitments(self, params["status"].as_str().unwrap_or("open")).await?)),
            "memex.commitment" => {
                understand::set_commitment(self, params["id"].as_i64().unwrap_or(0), params["status"].as_str().unwrap_or("")).await?;
                Ok(json!({}))
            }
            "memex.review" => {
                review::suggest(&self.store).await?;
                Ok(json!(review::list(&self.store, params["limit"].as_u64().unwrap_or(50) as u32).await?))
            }
            m if m.starts_with("memex.media.") => self.media_method(m, &params, &allowed).await,
            "memex.review_decide" => {
                *self.face_index.lock().unwrap() = None;
                review::decide(
                    &self.store,
                    params["id"].as_i64().unwrap_or(0),
                    params["accept"].as_bool().unwrap_or(false),
                    params["name"].as_str(),
                )
                .await?;
                Ok(json!({}))
            }
            "memex.merge" => {
                let from = self
                    .store
                    .thing(params["from"].as_str().unwrap_or_default())
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no such thing to merge"))?;
                let into = self
                    .store
                    .thing(params["into"].as_str().unwrap_or_default())
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no such thing to merge into"))?;
                review::merge(&self.store, &from.id, &into.id).await?;
                *self.face_index.lock().unwrap() = None;
                Ok(json!({}))
            }
            "memex.summarize" => Ok(json!({"summary": understand::summarize(self, params["id"].as_str().unwrap_or_default()).await?})),
            "memex.graph" => {
                let limit = params["limit"].as_u64().unwrap_or(40).min(150) as u32;
                let policies: Vec<api::ClaimPolicy> = serde_json::from_value(params["policies"].clone()).unwrap_or_default();
                Ok(json!(review::graph(&self.store, params["id"].as_str().unwrap_or_default(), &allowed, limit, &policies).await?))
            }
            "memex.explore.inventory" => explore::inventory(self, &allowed).await,
            "memex.explore.list" => explore::list(self, &params, &allowed).await,
            "memex.explore.thing" => explore::about_thing(self, params["id"].as_str().unwrap_or_default(), &allowed).await,
            "memex.months" => Ok(review::histogram(&self.store, params["id"].as_str().unwrap_or_default(), &allowed).await?),
            "memex.knowledge.record" => {
                let v = knowledge::record(self, &params).await?;
                // Whose claim it is, as the head says (docs/apps.md §9).
                if let (Some(owner), Some(id)) = (params["owner"].as_str().filter(|o| !o.is_empty()), v["id"].as_str()) {
                    sqlx::query("UPDATE facts SET owner = ? WHERE id = ?").bind(owner).bind(id).execute(&self.store.pool).await?;
                }
                Ok(v)
            }
            "memex.knowledge.verify" => knowledge::verify(self, &params).await,
            "memex.knowledge.set" => knowledge::set(self, &params).await,
            "memex.knowledge.list" => knowledge::list(self, &params).await,
            "memex.knowledge.get" => {
                let mut out = Vec::new();
                for id in params["ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                    if let Some(f) = knowledge::get(self, id).await? {
                        out.push(f);
                    }
                }
                Ok(json!(out))
            }
            "memex.knowledge.page" => knowledge::page(self, params["key"].as_str().or(params["id"].as_str()).unwrap_or_default()).await,
            "memex.reload" => {
                self.reload.notify_one();
                for m in self.realms.lock().await.values() {
                    m.reload.notify_one();
                }
                Ok(json!({}))
            }
            "memex.mention" => Ok(json!(self.store.find_things(params["q"].as_str().unwrap_or(""), 20).await?)),
            "memex.status" => {
                let mut s = self.store.status(&self.embed.space()).await?;
                s.last_error = self.last_error.lock().unwrap().clone();
                understand::status(self, &mut s).await?;
                media::status(self, &mut s).await?;
                let (waiting, trouble) = extract::status(self).await?;
                s.extracting = waiting as u64;
                if s.last_error.is_none() {
                    s.last_error = trouble.map(|t| format!("reading files: {t}"));
                }
                Ok(json!(s))
            }
            other => anyhow::bail!("the memex has no method {other}"),
        }
    }

    async fn media_method(&self, method: &str, params: &Value, allowed: &[String]) -> anyhow::Result<Value> {
        use base64::Engine;
        let id = params["id"].as_str().unwrap_or_default();
        let b64 = |data: Vec<u8>| json!({"mime": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(data)});
        match method {
            "memex.media.list" => media::list(self, params, allowed).await,
            "memex.media.get" => media::get(self, id, allowed).await,
            "memex.media.thumb" => {
                anyhow::ensure!(self.store.item(id, allowed).await?.is_some(), "no photo {id:?}");
                Ok(b64(media::thumb(self, id, params["frame"].as_u64().map(|n| n as usize)).await?))
            }
            "memex.media.face" => {
                anyhow::ensure!(allowed.iter().any(|k| k == "photo"), "photos are outside what this caller may see");
                Ok(b64(faces::picture(self, id).await?))
            }
            "memex.media.facets" => media::facets(self).await,
            other => anyhow::bail!("the memex has no method {other}"),
        }
    }

    async fn google_method(&self, method: &str, params: Value, allowed: &[String]) -> anyhow::Result<Value> {
        use base64::Engine;
        let need = |kind: &str| -> anyhow::Result<()> {
            anyhow::ensure!(allowed.iter().any(|k| k == kind), "{kind} is outside what this session may see");
            Ok(())
        };
        if method.starts_with("memex.mail.") || method.starts_with("memex.contacts.") {
            need("email")?;
        } else if method.starts_with("memex.calendar.") {
            need("event")?;
        }
        let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
        let made_by = params["made_by"].as_str().unwrap_or("you").to_owned();
        Ok(match method {
            "memex.mail.threads" => {
                let account = params["account"].as_str().filter(|a| !a.is_empty()).map(|a| self.mail_account(a)).transpose()?;
                let view = params["view"].as_str().unwrap_or("inbox");
                json!(
                    mail::threads(
                        self,
                        view,
                        account.as_deref(),
                        params["before"].as_i64(),
                        params["limit"].as_u64().unwrap_or(50).min(200) as u32
                    )
                    .await?
                )
            }
            "memex.mail.thread" => json!(mail::thread(self, &self.mail_account(&s("account"))?, &s("thread_id")).await?),
            "memex.mail.modify" => {
                let list = |k: &str| -> Vec<String> {
                    params[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
                };
                mail::modify(self, &self.mail_account(&s("account"))?, &s("thread_id"), &list("add"), &list("remove"), &made_by).await?;
                json!({})
            }
            "memex.mail.attachment" => {
                let (name, mime, data) = mail::attachment(self, &s("item_id"), params["index"].as_u64().unwrap_or(0) as usize).await?;
                json!({"name": name, "mime": mime, "data": base64::engine::general_purpose::STANDARD.encode(data)})
            }
            "memex.mail.draft" => {
                let d: api::MailDraft = serde_json::from_value(params)?;
                json!({"draft_id": mail::draft(self, &d, &made_by).await?})
            }
            "memex.mail.send" => {
                let d: api::MailDraft = serde_json::from_value(params)?;
                json!(mail::queue(self, &d, &made_by).await?)
            }
            "memex.mail.cancel" => json!({"cancelled": mail::cancel(self, &s("id")).await?}),
            "memex.mail.outgoing" => json!(mail::outgoing(self).await?),
            "memex.mail.labels" => json!(mail::label_list(self).await?),
            "memex.calendar.calendars" => json!(calendar::calendars(self).await?),
            "memex.calendar.select" => {
                calendar::select(self, &self.account("calendar", &s("account"))?.id, &s("id"), params["on"].as_bool().unwrap_or(true))
                    .await?;
                json!({})
            }
            "memex.calendar.events" => {
                json!(calendar::events(self, params["from"].as_i64().unwrap_or(0), params["to"].as_i64().unwrap_or(0)).await?)
            }
            "memex.calendar.busy" => {
                json!(calendar::busy(self, params["from"].as_i64().unwrap_or(0), params["to"].as_i64().unwrap_or(0)).await?)
            }
            "memex.calendar.create" => json!(calendar::write(self, "create", params, &made_by).await?),
            "memex.calendar.update" => {
                calendar::write(self, "update", params, &made_by).await?;
                json!({})
            }
            "memex.calendar.delete" => {
                calendar::write(self, "delete", params, &made_by).await?;
                json!({})
            }
            "memex.calendar.rsvp" => {
                calendar::write(self, "rsvp", params, &made_by).await?;
                json!({})
            }
            "memex.contacts.list" => json!(contacts::list(self, &s("q")).await?),
            "memex.contacts.candidates" => json!(contacts::candidates(self).await?),
            "memex.contacts.create" => json!(contacts::create(self, &s("account"), &s("name"), &s("email"), &made_by).await?),
            // Several at once: [{name, email}]; the addresses added.
            "memex.contacts.create_many" => {
                let people: Vec<(String, String)> = params["people"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| (p["name"].as_str().unwrap_or_default().to_owned(), p["email"].as_str().unwrap_or_default().to_owned()))
                    .collect();
                json!({"added": contacts::create_many(self, &s("account"), &people, &made_by).await?})
            }
            other => anyhow::bail!("the memex has no method {other}"),
        })
    }

    /// Keyword and meaning search, merged by reciprocal rank.
    pub async fn search(&self, q: &api::MemexQuery, allowed: &[String]) -> anyhow::Result<Vec<api::MemexHit>> {
        let kinds: Vec<String> =
            if q.kinds.is_empty() { allowed.to_vec() } else { q.kinds.iter().filter(|k| allowed.contains(k)).cloned().collect() };
        let about = self.store.things_for(&q.about).await?;
        if kinds.is_empty() || q.query.trim().is_empty() || (!q.about.is_empty() && about.len() < q.about.len()) {
            return Ok(Vec::new());
        }
        let limit = q.limit.unwrap_or(10).clamp(1, 50) as usize;
        // part → (score, item, how it matched)
        let mut parts: HashMap<i64, (f64, String, Vec<String>)> = HashMap::new();
        for (rank, f) in self.store.keyword(&q.query, &kinds, &about, CANDIDATES).await?.into_iter().enumerate() {
            let e = parts.entry(f.part_id).or_insert((0.0, f.item_id, Vec::new()));
            e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
            e.2.push("keyword".into());
        }
        let space = self.embed.space();
        if !space.is_empty() {
            match self.embed.embed(std::slice::from_ref(&q.query)).await {
                Ok(v) => {
                    let mut scored: Vec<(i64, String, f32)> = self
                        .store
                        .vectors(&space, &kinds, &about)
                        .await?
                        .into_iter()
                        .map(|(p, i, vec)| (p, i, embed::similarity(&v[0], &vec)))
                        .filter(|(_, _, s)| *s > 0.0)
                        .collect();
                    scored.sort_by(|a, b| b.2.total_cmp(&a.2));
                    for (rank, (p, i, _)) in scored.into_iter().take(CANDIDATES as usize).enumerate() {
                        let e = parts.entry(p).or_insert((0.0, i, Vec::new()));
                        e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
                        e.2.push("meaning".into());
                    }
                }
                Err(e) => warn!("embedding a query (keyword search only): {e:#}"),
            }
        }
        // Each item at its best passage.
        let mut items: HashMap<String, (f64, i64, Vec<String>)> = HashMap::new();
        for (part, (score, item, how)) in parts {
            let e = items.entry(item).or_insert((0.0, part, Vec::new()));
            if score > e.0 {
                e.0 = score;
                e.1 = part;
            }
            for h in how {
                if !e.2.contains(&h) {
                    e.2.push(h);
                }
            }
        }
        let mut ranked: Vec<(String, f64, Option<i64>, Vec<String>)> =
            items.into_iter().map(|(id, (s, p, how))| (id, s, Some(p), how)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);
        self.store.hits(&ranked, &q.query).await
    }

    async fn thing(&self, key_or_id: &str, allowed: &[String]) -> anyhow::Result<Option<api::MemexThing>> {
        let Some(t) = self.store.thing(key_or_id).await? else { return Ok(None) };
        let notes =
            if allowed.iter().any(|k| k == "note") { self.store.about(Some(&t.id), &["note".to_owned()], 20).await? } else { Vec::new() };
        let others: Vec<String> = allowed.iter().filter(|k| *k != "note").cloned().collect();
        Ok(Some(api::MemexThing {
            notes,
            items: self.store.about(Some(&t.id), &others, 20).await?,
            related: self.store.related(&t.id, allowed, 10).await?,
            thing: t,
        }))
    }

    /// Context for a session about `labels` (and its first message), within
    /// `chars`: notes about them (and notes for every session), recent
    /// sessions about them, and what else matches the message.
    pub async fn brief(&self, labels: &[String], text: &str, chars: usize, allowed: &[String]) -> anyhow::Result<api::MemexBrief> {
        let mut refs: Vec<String> = Vec::new();
        let mut sections: Vec<String> = Vec::new();
        let notes_ok = allowed.iter().any(|k| k == "note");
        // Not worker: (every session on it would match).
        let labels: Vec<String> = labels.iter().filter(|l| !l.starts_with("worker:")).cloned().collect();
        let things = self.store.things_for(&labels).await?;
        let mut names = Vec::new();
        let mut notes = Vec::new();
        let mut recent = Vec::new();
        if notes_ok {
            notes.extend(self.store.about(None, &["note".to_owned()], 15).await?);
        }
        for t in &things {
            let Some(r) = self.store.thing(t).await? else { continue };
            names.push(r.label.clone());
            if notes_ok {
                notes.extend(self.store.about(Some(t), &["note".to_owned()], 15).await?);
            }
            if allowed.iter().any(|k| k == "session") {
                recent.extend(self.store.about(Some(t), &["session".to_owned()], 5).await?);
            }
        }
        // Environment knowledge: facts about these things and the systems
        // they touch, with how each is known and where it applies.
        if allowed.iter().any(|k| k == "fact") {
            let facts = knowledge::for_brief(self, &names, &labels, 30).await.unwrap_or_default();
            if !facts.is_empty() {
                let now = c6_core::now_ms();
                let lines: Vec<String> = facts.iter().map(|f| knowledge::brief_line(f, now)).collect();
                let guidance = self.prompt("memex.brief.knowledge", api::prompts::BRIEF_KNOWLEDGE);
                sections.push(format!("## Environment knowledge\n{guidance}\n{}", lines.join("\n")));
            }
        }
        let mut seen = std::collections::HashSet::new();
        // carbon6's memory notes reach sessions on their own, by their scope.
        notes.retain(|n| n.source != "memory" && seen.insert(n.id.clone()));
        notes.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        if !notes.is_empty() {
            let lines: Vec<String> = notes
                .iter()
                .map(|n| {
                    refs.push(n.id.clone());
                    format!("- {} [{}]", clip(&n.snippet, 400), n.id)
                })
                .collect();
            sections.push(format!("## Notes\n{}", lines.join("\n")));
        }
        recent.retain(|r| seen.insert(r.id.clone()));
        recent.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        recent.truncate(6);
        if !recent.is_empty() {
            let lines: Vec<String> = recent
                .iter()
                .map(|r| {
                    refs.push(r.id.clone());
                    format!("- {} — {} [{}]", r.title, clip(&last_answer(&r.snippet), 200), r.id)
                })
                .collect();
            sections.push(format!("## Recent sessions about these\n{}", lines.join("\n")));
        }
        if !text.trim().is_empty() {
            let q = api::MemexQuery { query: text.chars().take(500).collect(), limit: Some(5), ..Default::default() };
            let hits: Vec<api::MemexHit> = self
                .search(&q, allowed)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|h| h.source != "memory" && seen.insert(h.id.clone()))
                .collect();
            if !hits.is_empty() {
                let lines: Vec<String> = hits
                    .iter()
                    .map(|h| {
                        refs.push(h.id.clone());
                        format!("- ({}) {}: {} [{}]", h.kind, h.title, clip(&h.snippet, 240), h.id)
                    })
                    .collect();
                sections.push(format!("## Possibly relevant\n{}", lines.join("\n")));
            }
        }
        if sections.is_empty() {
            return Ok(api::MemexBrief::default());
        }
        let about = if names.is_empty() { String::new() } else { format!(" about {}", names.join(", ")) };
        let mut out = format!(
            "From your memex{about}. Items are cited by [id]; memex_get fetches one in full and memex_search finds more. They may be out of date: trust what you observe.\n\n{}",
            sections.join("\n\n")
        );
        if out.chars().count() > chars {
            out = format!("{}…", out.chars().take(chars).collect::<String>());
        }
        Ok(api::MemexBrief { text: out, refs })
    }
}

/// A session digest's last answer line (what it concluded).
fn last_answer(snippet: &str) -> String {
    snippet.rsplit("Answer:").next().unwrap_or(snippet).trim().to_owned()
}

#[async_trait::async_trait]
impl c6_worker::Extension for Memex {
    fn capabilities(&self) -> Vec<String> {
        vec!["memex".into()]
    }

    async fn handle(&self, method: &str, params: Value) -> Option<anyhow::Result<Value>> {
        if !method.starts_with("memex.") {
            return None;
        }
        let r = self.handle_method(method, params).await;
        if let Err(e) = &r {
            info!(%method, "memex call failed: {e:#}");
        }
        Some(r)
    }
}

fn mime_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        _ => "text/plain",
    }
}
