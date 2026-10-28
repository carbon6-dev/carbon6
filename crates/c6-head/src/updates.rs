//! Updating carbon6 from signed GitHub releases (docs/updates.md). The head
//! looks for releases, verifies their manifest, asks you (or not, by the
//! setting), updates itself, and once it runs the new version tells each
//! worker to update.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use c6_api as api;
use c6_core::update::{self as up, Manifest, Method, OnStart, Pending, Restart};
use c6_core::{WorkerId, now_ms};
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::{HeadState, bad_request, not_found};

/// The setting.
pub const KEY: &str = "updates";
const ETAG: &str = "updates.etag";
const ETAG_EDGE: &str = "updates.etag.edge";
/// A worker that hasn't come back on the new version by then has failed.
const RESTART_TIMEOUT_MS: i64 = 10 * 60 * 1000;

/// What the head knows of updating, beyond its database.
pub struct Updater {
    /// The repository on GitHub (`https://github.com/carbon6-dev/carbon6`).
    pub source: String,
    /// Release keys (hex); empty: the ones built in.
    pub keys: Vec<String>,
    /// The binary to update, when not this process's (tests).
    pub exe: Option<PathBuf>,
    /// The version this head runs (tests may say another).
    pub version: String,
    pub every: Duration,
    /// An update asked the process to exit with the restart code.
    pub restart: AtomicBool,
    pub checked_at: AtomicI64,
    pub check_error: Mutex<String>,
    pub wake: tokio::sync::Notify,
    on_start: Mutex<Option<OnStart>>,
}

impl Updater {
    pub fn new(cfg: &crate::HeadConfig) -> Self {
        Self {
            source: cfg.update_source.clone(),
            keys: cfg.update_keys.clone(),
            exe: cfg.update_exe.clone(),
            version: cfg.update_as_version.clone().unwrap_or_else(|| c6_core::build().to_owned()),
            every: Duration::from_secs(cfg.update_check_secs.max(1)),
            restart: AtomicBool::new(false),
            checked_at: AtomicI64::new(0),
            check_error: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            on_start: Mutex::default(),
        }
    }

    pub fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::SeqCst)
    }

    fn keys(&self) -> Vec<String> {
        if self.keys.is_empty() { up::RELEASE_KEYS.iter().map(|k| k.to_string()).collect() } else { self.keys.clone() }
    }

    fn exe_method(&self) -> (PathBuf, Method) {
        match &self.exe {
            Some(exe) => (exe.clone(), up::method(exe, false)),
            None => {
                let exe = std::env::current_exe().and_then(|e| e.canonicalize()).unwrap_or_default();
                let m = up::method(&exe, cfg!(debug_assertions));
                (exe, m)
            }
        }
    }
}

/// Error from `start`: this version was rolled back; exit with the
/// restart code so the previous one runs.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RolledBack(pub String);

fn ver(st: &HeadState) -> &str {
    &st.updates.version
}

fn dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("updates")
}

/// First thing in `start`, before the database opens: an update in
/// progress counts this start, and rolls back after too many. The old
/// version, back after a rollback, puts the database back as it was.
pub fn before_open(u: &Updater, data_dir: &std::path::Path) -> anyhow::Result<()> {
    let (exe, method) = u.exe_method();
    let s = up::on_start(&dir(data_dir), &u.version, &method, "c6h", &exe);
    match &s {
        OnStart::RolledBack(p) => return Err(RolledBack(p.detail.clone()).into()),
        OnStart::WasRolledBack(p) if !p.backup.is_empty() => {
            let db = data_dir.join("c6h.db");
            std::fs::copy(&p.backup, &db).map_err(|e| anyhow::anyhow!("putting back {}: {e}", p.backup))?;
            for x in ["c6h.db-wal", "c6h.db-shm"] {
                let _ = std::fs::remove_file(data_dir.join(x));
            }
            warn!(backup = %p.backup, "{}; its database is back too", p.detail);
        }
        _ => {}
    }
    *u.on_start.lock().unwrap() = Some(s);
    Ok(())
}

/// Once the database is open: the backup this version's upgrade made goes
/// with the update in progress, for a rollback.
pub async fn after_open(st: &HeadState) {
    let Some(OnStart::Trying(mut p)) = st.updates.on_start.lock().unwrap().clone() else { return };
    if !p.backup.is_empty() {
        return;
    }
    if let Some(b) = st.store.upgrades().await.ok().into_iter().flatten().find(|r| r.at >= p.started_at).and_then(|r| r.backup) {
        p.backup = b;
        let _ = p.save(&dir(&st.data_dir));
    }
}

/// The head is up: an update in progress is done (or was rolled back), and
/// the loops start.
pub async fn started(st: &std::sync::Arc<HeadState>) {
    let s = st.updates.on_start.lock().unwrap().take();
    let (_, method) = st.updates.exe_method();
    match s {
        Some(OnStart::Trying(_)) => {
            if let Some(p) = up::healthy(&dir(&st.data_dir)) {
                info!(from = %p.from, to = %p.to, "updated");
                let _ = machine(st, "head", &p.to, "done", method.name(), "").await;
            }
        }
        Some(OnStart::WasRolledBack(p)) => {
            up::Pending::clear(&dir(&st.data_dir));
            let _ = machine(st, "head", &p.to, "rolled_back", method.name(), &p.detail).await;
            let _ = set_state(st, &p.to, "failed", &p.detail).await;
            if let Ok(mut s) = settings(st).await
                && !s.skip.contains(&p.to)
            {
                s.skip.push(p.to.clone());
                let _ = st.store.set_setting(KEY, &serde_json::to_string(&s).unwrap_or_default()).await;
            }
            let d = api::DecisionIn {
                kind: "confirm".into(),
                title: format!("carbon6 {} didn't start, and {} is back", p.to, p.from),
                document: format!("{}\n\nThe head will not update to {} again (Settings → System → Updates).", p.detail, p.to),
                source: "updates".into(),
                ..Default::default()
            };
            let _ = crate::decisions::open(st, &d, crate::realms::ROOT, "").await;
        }
        _ => {}
    }
    let st2 = st.clone();
    tokio::spawn(async move { look_loop(st2).await });
    let st2 = st.clone();
    tokio::spawn(async move { drive_loop(st2).await });
}

pub async fn settings(st: &HeadState) -> anyhow::Result<api::UpdateSettings> {
    Ok(st.store.setting(KEY).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

pub async fn set_settings(st: &HeadState, s: &api::UpdateSettings) -> anyhow::Result<()> {
    if !matches!(s.mode.as_str(), "off" | "notify" | "approve" | "auto") {
        return Err(bad_request("mode is off, notify, approve or auto"));
    }
    if !matches!(s.channel.as_str(), "stable" | "edge") {
        return Err(bad_request("channel is stable or edge"));
    }
    if !matches!(s.auto_for.as_str(), "patch" | "minor") {
        return Err(bad_request("auto_for is patch or minor"));
    }
    if !s.window.trim().is_empty() && window(&s.window).is_none() {
        return Err(bad_request("a window is like 02:00-05:00"));
    }
    st.store.set_setting(KEY, &serde_json::to_string(s)?).await?;
    st.updates.wake.notify_one();
    Ok(())
}

/// "02:00-05:00" as minutes of the day.
fn window(w: &str) -> Option<(u32, u32)> {
    let m = |t: &str| {
        let (h, m) = t.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let (a, b) = w.split_once(['-', '–'])?;
    Some((m(a)?, m(b)?))
}

fn in_window(w: &str) -> bool {
    let Some((a, b)) = window(w) else { return true };
    use chrono::Timelike;
    let now = chrono::Local::now();
    let t = now.hour() * 60 + now.minute();
    if a <= b { t >= a && t < b } else { t >= a || t < b }
}

async fn look_loop(st: std::sync::Arc<HeadState>) {
    loop {
        if settings(&st).await.map(|s| s.mode != "off").unwrap_or(false)
            && let Err(e) = check(&st).await
        {
            warn!("looking for a carbon6 release: {e:#}");
        }
        tokio::select! {
            _ = st.shutdown.cancelled() => return,
            _ = tokio::time::sleep(st.updates.every) => {}
        }
    }
}

/// Looks at GitHub for a newer release; returns its version when there is one.
/// Only release files are downloaded, never GitHub's API, whose 60 requests
/// an hour a household shares.
pub async fn check(st: &HeadState) -> anyhow::Result<Option<String>> {
    let r = look(st).await;
    st.updates.checked_at.store(now_ms(), Ordering::SeqCst);
    *st.updates.check_error.lock().unwrap() = r.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default();
    r
}

/// The newest release (and, on the edge channel, the newest build of
/// main) newer than this one, if any.
async fn look(st: &HeadState) -> anyhow::Result<Option<String>> {
    let stable = look_at(st, false).await?;
    if settings(st).await?.channel != "edge" {
        return Ok(stable);
    }
    let edge = look_at(st, true).await?;
    Ok(match (stable, edge) {
        (Some(a), Some(b)) => Some(if up::newer(&b, &a) { b } else { a }),
        (a, b) => a.or(b),
    })
}

/// One of GitHub's releases: the latest (`edge`: false), or the rolling
/// `edge` pre-release that CI replaces for every green commit on main. Its
/// `release.json` is downloaded from the release's files (with the last
/// ETag, so an unchanged one is a 304), and its signed manifest kept (and,
/// by the setting, asked about) when it is newer than this build.
async fn look_at(st: &HeadState, edge: bool) -> anyhow::Result<Option<String>> {
    let u = &st.updates;
    let repo = u.source.trim_end_matches('/');
    // `latest/download` redirects to the latest release's files: never a
    // draft or a pre-release.
    let (files, etag_key) =
        if edge { (format!("{repo}/releases/download/edge/"), ETAG_EDGE) } else { (format!("{repo}/releases/latest/download/"), ETAG) };
    let mut req = st.http.get(format!("{files}release.json")).header("User-Agent", format!("carbon6/{}", c6_core::build()));
    if let Some(etag) = st.store.setting(etag_key).await? {
        req = req.header("If-None-Match", etag);
    }
    let resp = req.send().await?;
    // The edge release is gone while CI replaces it.
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED || (edge && resp.status() == reqwest::StatusCode::NOT_FOUND) {
        return Ok(None);
    }
    let what = if edge { "the edge build" } else { "the latest release" };
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("{what} has no release.json: it won't update to it");
    }
    if !resp.status().is_success() {
        anyhow::bail!("GitHub said {} for {what}'s release.json", resp.status());
    }
    let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let manifest = resp.bytes().await?.to_vec();
    let remember = async || -> anyhow::Result<()> {
        if let Some(e) = &etag {
            st.store.set_setting(etag_key, e).await?;
        }
        Ok(())
    };
    let sig =
        fetch(st, &format!("{files}release.json.sig")).await.map_err(|e| anyhow::anyhow!("{what} has no signed release.json: {e:#}"))?;
    let m = up::verify(&manifest, &sig, &u.keys()).map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
    if !edge && up::is_edge(&m.version) {
        anyhow::bail!("the latest release's release.json names an edge build ({})", m.version);
    }
    if edge && !up::is_edge(&m.version) {
        anyhow::bail!("the edge build's release.json names a release ({})", m.version);
    }
    let version = m.version.clone();
    if !up::newer(&version, ver(st)) {
        remember().await?;
        return Ok(None);
    }
    if !known(st, &version).await? {
        // Its files: a release's under its own tag (`latest` moves on).
        let base = if edge { files } else { format!("{repo}/releases/download/v{version}/") };
        let now = now_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO updates (version, manifest, signature, base_url, found_at, state, updated_at) VALUES (?, ?, ?, ?, ?, 'available', ?)",
        )
        .bind(&version)
        .bind(String::from_utf8_lossy(&manifest).into_owned())
        .bind(hex::encode(&sig))
        .bind(&base)
        .bind(now)
        .bind(now)
        .execute(&st.store.pool)
        .await?;
        info!(%version, edge, "found a carbon6 build to update to");
        consider(st, &version).await?;
    }
    remember().await?;
    Ok(Some(version))
}

async fn fetch(st: &HeadState, url: &str) -> anyhow::Result<Vec<u8>> {
    let r = st.http.get(url).header("User-Agent", format!("carbon6/{}", c6_core::build())).timeout(Duration::from_secs(600)).send().await?;
    if !r.status().is_success() {
        anyhow::bail!("{url}: {}", r.status());
    }
    Ok(r.bytes().await?.to_vec())
}

async fn known(st: &HeadState, version: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("SELECT 1 FROM updates WHERE version = ?").bind(version).fetch_optional(&st.store.pool).await?.is_some())
}

struct Rel {
    version: String,
    manifest: Manifest,
    raw: String,
    sig: Vec<u8>,
    base: String,
    state: String,
    detail: String,
}

async fn release(st: &HeadState, version: &str) -> anyhow::Result<Option<Rel>> {
    let Some(r) = sqlx::query("SELECT * FROM updates WHERE version = ?").bind(version).fetch_optional(&st.store.pool).await? else {
        return Ok(None);
    };
    let raw: String = r.get("manifest");
    Ok(Some(Rel {
        version: r.get("version"),
        manifest: serde_json::from_str(&raw)?,
        raw,
        sig: hex::decode(r.get::<String, _>("signature"))?,
        base: r.get("base_url"),
        state: r.get("state"),
        detail: r.get("detail"),
    }))
}

async fn set_state(st: &HeadState, version: &str, state: &str, detail: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE updates SET state = ?, detail = ?, updated_at = ? WHERE version = ?")
        .bind(state)
        .bind(detail)
        .bind(now_ms())
        .bind(version)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(())
}

async fn machine(st: &HeadState, machine: &str, version: &str, state: &str, method: &str, detail: &str) -> anyhow::Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO update_machines (machine, version, state, method, detail, started_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (machine, version) DO UPDATE SET state = excluded.state, method = CASE WHEN excluded.method = '' THEN method ELSE excluded.method END, \
         detail = excluded.detail, updated_at = excluded.updated_at",
    )
    .bind(machine)
    .bind(version)
    .bind(state)
    .bind(method)
    .bind(detail)
    .bind(now)
    .bind(now)
    .execute(&st.store.pool)
    .await?;
    st.registry.bump();
    Ok(())
}

fn reference(version: &str) -> String {
    format!("update:{version}")
}

/// What the setting says to do with a release found.
async fn consider(st: &HeadState, version: &str) -> anyhow::Result<()> {
    let Some(rel) = release(st, version).await? else { return Ok(()) };
    if rel.state != "available" {
        return Ok(());
    }
    let s = settings(st).await?;
    if s.skip.iter().any(|v| v == version) {
        return set_state(st, version, "skipped", "").await;
    }
    // A newer release replaces an older one still waiting.
    let older: Vec<String> = sqlx::query_scalar("SELECT version FROM updates WHERE state = 'available' AND version != ?")
        .bind(version)
        .fetch_all(&st.store.pool)
        .await?;
    for o in older.iter().filter(|o| up::newer(version, o)) {
        crate::decisions::settle_ref(st, &reference(o), "withdrawn", json!({}), &format!("carbon6 {version}")).await;
        set_state(st, o, "skipped", &format!("{version} replaces it")).await?;
    }
    let step = up::step(ver(st), version).unwrap_or(up::Step::Major);
    let biggest = if s.auto_for == "minor" { up::Step::Minor } else { up::Step::Patch };
    match s.mode.as_str() {
        "auto" if step <= biggest && up::unattended(ver(st), &rel.manifest) => set_state(st, version, "approved", "auto").await?,
        "approve" | "auto" => ask(st, &rel).await?,
        _ => {}
    }
    st.updates.wake.notify_one();
    Ok(())
}

async fn ask(st: &HeadState, rel: &Rel) -> anyhow::Result<()> {
    let m = &rel.manifest;
    let workers = st.store.workers().await?.into_iter().filter(|w| w.revoked_at.is_none() && w.role != "memex").count();
    let mut doc = format!(
        "carbon6 **{}** is out (you run {}). Updating it updates the head first, then {workers} worker{} one after another, each once its sessions are idle (or after {} hours).\n\n",
        m.version,
        ver(st),
        if workers == 1 { "" } else { "s" },
        settings(st).await?.drain_hours
    );
    if let Some(steps) = m.manual.as_deref().filter(|s| !s.trim().is_empty()) {
        doc.push_str(&format!("**Before updating:** {steps}\n\n"));
    }
    if !m.upgrade_from.is_empty() && up::newer(&m.upgrade_from, ver(st)) {
        doc.push_str(&format!("**It updates on its own only from {}:** update by hand (README → Upgrades).\n\n", m.upgrade_from));
    }
    doc.push_str("## What's new\n\n");
    doc.push_str(&m.notes);
    let d = api::DecisionIn {
        kind: "approve".into(),
        title: format!("Update carbon6 to {}?", m.version),
        document: doc,
        source: "updates".into(),
        options: ["Update now", "Skip this version", "Not yet"]
            .iter()
            .map(|o| api::DecisionOption { label: (*o).to_owned(), does: String::new() })
            .collect(),
        context: json!({"version": m.version}),
        ..Default::default()
    };
    crate::decisions::open(st, &d, crate::realms::ROOT, &reference(&m.version)).await?;
    Ok(())
}

/// Your answer to "Update carbon6 to …?".
pub async fn answer(st: &HeadState, version: &str, option: Option<&str>) -> anyhow::Result<()> {
    match option {
        Some("Update now") => apply(st, version).await,
        Some("Skip this version") => skip(st, version).await,
        _ => Ok(()),
    }
}

/// Updates to `version` now (whatever the setting says).
pub async fn apply(st: &HeadState, version: &str) -> anyhow::Result<()> {
    let rel = release(st, version).await?.ok_or_else(|| not_found(format!("no release {version} found")))?;
    if rel.state == "applying" {
        return Ok(());
    }
    // Trying again: what failed before is tried again.
    sqlx::query("DELETE FROM update_machines WHERE version = ? AND state IN ('failed', 'rolled_back')")
        .bind(version)
        .execute(&st.store.pool)
        .await?;
    set_state(st, version, "approved", "").await?;
    crate::decisions::settle_ref(st, &reference(version), "answered", json!({"option": "Update now"}), "updates").await;
    st.updates.wake.notify_one();
    Ok(())
}

pub async fn skip(st: &HeadState, version: &str) -> anyhow::Result<()> {
    let mut s = settings(st).await?;
    if !s.skip.iter().any(|v| v == version) {
        s.skip.push(version.to_owned());
    }
    st.store.set_setting(KEY, &serde_json::to_string(&s)?).await?;
    set_state(st, version, "skipped", "").await?;
    crate::decisions::settle_ref(st, &reference(version), "answered", json!({"option": "Skip this version"}), "updates").await;
    Ok(())
}

async fn drive_loop(st: std::sync::Arc<HeadState>) {
    loop {
        if let Err(e) = drive(&st).await {
            warn!("updating: {e:#}");
        }
        tokio::select! {
            _ = st.shutdown.cancelled() => return,
            _ = st.updates.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
        }
    }
}

/// One step of whatever update is under way.
async fn drive(st: &HeadState) -> anyhow::Result<()> {
    if st.updates.restart_requested() {
        return Ok(());
    }
    let versions: Vec<String> =
        sqlx::query_scalar("SELECT version FROM updates WHERE state IN ('approved', 'applying', 'done')").fetch_all(&st.store.pool).await?;
    let Some(target) = versions.into_iter().reduce(|a, b| if up::newer(&b, &a) { b } else { a }) else { return Ok(()) };
    let Some(rel) = release(st, &target).await? else { return Ok(()) };
    if up::newer(&target, ver(st)) {
        if rel.state == "approved" && rel.detail == "auto" && !in_window(&settings(st).await?.window) {
            return Ok(());
        }
        return update_head(st, &rel).await;
    }
    if rel.state == "approved" {
        set_state(st, &target, "applying", "").await?;
    }
    workers(st, &rel).await
}

async fn update_head(st: &HeadState, rel: &Rel) -> anyhow::Result<()> {
    if matches!(rel.state.as_str(), "failed") {
        return Ok(());
    }
    let (exe, method) = st.updates.exe_method();
    let v = &rel.version;
    set_state(st, v, "applying", "").await?;
    let fail = async |detail: String| -> anyhow::Result<()> {
        machine(st, "head", v, "failed", method.name(), &detail).await?;
        set_state(st, v, "failed", &format!("the head: {detail}")).await
    };
    if method == Method::Source {
        return fail("built from source: update it yourself, and the workers follow".into()).await;
    }
    // It restarted for this version, and runs the old one still: not again.
    let before: Option<String> = sqlx::query_scalar("SELECT state FROM update_machines WHERE machine = 'head' AND version = ?")
        .bind(v)
        .fetch_optional(&st.store.pool)
        .await?;
    if matches!(before.as_deref(), Some("restarting" | "installing")) {
        return fail(format!("it restarted, and still runs {}", ver(st))).await;
    }
    machine(st, "head", v, "downloading", method.name(), "").await?;
    let asset = match &method {
        // Homebrew installs a release itself; an edge build is put in place.
        Method::Brew { .. } if !up::in_place(&method, &rel.manifest) => None,
        _ => match download(st, rel, "c6h", &method).await {
            Ok(a) => Some(a),
            Err(e) => return fail(format!("{e:#}")).await,
        },
    };
    machine(st, "head", v, "installing", method.name(), "").await?;
    // Before installing: a .deb's postinst restarts the head mid-install.
    let pending = dir(&st.data_dir);
    Pending {
        from: ver(st).to_owned(),
        to: v.clone(),
        state: "installed".into(),
        started_at: now_ms(),
        in_place: up::in_place(&method, &rel.manifest),
        ..Default::default()
    }
    .save(&pending)?;
    let (m, raw, sig) = (rel.manifest.clone(), rel.raw.clone(), rel.sig.clone());
    let (exe2, method2) = (exe.clone(), method.clone());
    let done = tokio::task::spawn_blocking(move || {
        up::install(&method2, "c6h", &exe2, &m, (raw.as_bytes(), &sig), asset.as_ref().map(|(n, b)| (n.as_str(), b.as_slice())))
    })
    .await?;
    let restart = match done {
        Ok(r) => r,
        Err(e) => {
            Pending::clear(&pending);
            return fail(e).await;
        }
    };
    machine(st, "head", v, "restarting", method.name(), "").await?;
    info!(to = %v, "updated the head; restarting");
    if restart == Restart::Exit {
        st.updates.restart.store(true, Ordering::SeqCst);
        st.shutdown.cancel();
    }
    Ok(())
}

/// The file for `bin` this install method takes, downloaded and checked.
async fn download(st: &HeadState, rel: &Rel, bin: &str, method: &Method) -> anyhow::Result<(String, Vec<u8>)> {
    let (name, sum) = up::asset(&rel.manifest, bin, method.name(), std::env::consts::OS, std::env::consts::ARCH)
        .ok_or_else(|| anyhow::anyhow!("the release has no {bin} for {} {}", std::env::consts::OS, std::env::consts::ARCH))?;
    let bytes = fetch(st, &format!("{}{name}", rel.base)).await?;
    if up::checksum(&bytes) != sum {
        anyhow::bail!("{name} isn't the file the release signed");
    }
    Ok((name, bytes))
}

/// The head runs `rel`: workers on an older version update, one at a time
/// (or all at once), each when it is connected.
async fn workers(st: &HeadState, rel: &Rel) -> anyhow::Result<()> {
    let s = settings(st).await?;
    let v = &rel.version;
    let rows = sqlx::query("SELECT machine, state, updated_at FROM update_machines WHERE version = ? AND machine != 'head'")
        .bind(v)
        .fetch_all(&st.store.pool)
        .await?;
    let mut states: std::collections::HashMap<String, (String, i64)> =
        rows.iter().map(|r| (r.get("machine"), (r.get("state"), r.get("updated_at")))).collect();
    let now = now_ms();
    let mut in_flight = 0;
    let mut left = 0;
    let workers = st.store.workers().await?;
    for w in workers.iter().filter(|w| w.revoked_at.is_none() && w.role == "worker") {
        let running = w.version.clone().unwrap_or_default();
        let state = states.remove(&w.id);
        if !up::newer(v, &running) {
            if state.as_ref().is_some_and(|(s, _)| s != "done") {
                machine(st, &w.id, v, "done", "", "").await?;
            }
            continue;
        }
        match state.as_ref().map(|(s, at)| (s.as_str(), *at)) {
            Some(("failed" | "rolled_back", _)) => {}
            Some(("restarting", at)) if now - at > RESTART_TIMEOUT_MS => {
                machine(st, &w.id, v, "failed", "", "didn't come back on the new version within 10 minutes").await?;
            }
            Some(("done", _)) => {}
            Some(_) => {
                in_flight += 1;
                left += 1;
            }
            None => left += 1,
        }
    }
    for w in workers.iter().filter(|w| w.revoked_at.is_none() && w.role == "worker") {
        if in_flight > 0 && !s.parallel {
            break;
        }
        let Some(id) = w.worker_id() else { continue };
        if !up::newer(v, w.version.as_deref().unwrap_or_default()) || st.registry.get(id).is_none() {
            continue;
        }
        let started = sqlx::query("SELECT 1 FROM update_machines WHERE machine = ? AND version = ?")
            .bind(&w.id)
            .bind(v)
            .fetch_optional(&st.store.pool)
            .await?
            .is_some();
        if started {
            continue;
        }
        machine(st, &w.id, v, "waiting", "", "").await?;
        let params = json!({
            "version": v,
            "manifest": rel.raw,
            "signature": hex::encode(&rel.sig),
            "base_url": rel.base,
            "deadline_ms": now + s.drain_hours as i64 * 3600 * 1000,
        });
        if let Err(e) = st.registry.call(id, "update.apply", params, Duration::from_secs(30)).await {
            machine(st, &w.id, v, "failed", "", &e).await?;
        } else {
            in_flight += 1;
        }
    }
    if left == 0 && rel.state != "done" {
        set_state(st, v, "done", "").await?;
    }
    Ok(())
}

/// `update.status`: a worker says how its update goes.
pub async fn worker_status(st: &HeadState, from: WorkerId, p: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
    let state = s("state");
    if !matches!(state.as_str(), "waiting" | "downloading" | "installing" | "restarting" | "done" | "failed" | "rolled_back") {
        return Err(bad_request(format!("{state:?} is not an update state")));
    }
    machine(st, &from.to_string(), &s("version"), &state, &s("method"), &s("detail")).await?;
    st.updates.wake.notify_one();
    Ok(json!({}))
}

/// Settings → System → Updates.
pub async fn status(st: &HeadState) -> anyhow::Result<api::Updates> {
    let rows = sqlx::query("SELECT * FROM updates ORDER BY found_at DESC LIMIT 20").fetch_all(&st.store.pool).await?;
    let mut releases: Vec<api::Release> = rows
        .iter()
        .filter_map(|r| {
            let m: Manifest = serde_json::from_str(&r.get::<String, _>("manifest")).ok()?;
            Some(api::Release {
                version: m.version,
                date: m.date,
                notes: m.notes,
                manual: m.manual,
                state: r.get("state"),
                detail: r.get("detail"),
                found_at: r.get("found_at"),
            })
        })
        .collect();
    releases.sort_by(|a, b| if up::newer(&a.version, &b.version) { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater });
    releases.truncate(6);
    let latest = |machine: String| async move {
        sqlx::query("SELECT * FROM update_machines WHERE machine = ? ORDER BY updated_at DESC LIMIT 1")
            .bind(machine)
            .fetch_optional(&st.store.pool)
            .await
    };
    let as_api = |id: String, name: String, running: String, r: Option<sqlx::sqlite::SqliteRow>| api::MachineUpdate {
        machine: id,
        name,
        running,
        version: r.as_ref().map(|r| r.get("version")).unwrap_or_default(),
        state: r.as_ref().map(|r| r.get("state")).unwrap_or_default(),
        method: r.as_ref().map(|r| r.get("method")).unwrap_or_default(),
        detail: r.as_ref().map(|r| r.get("detail")).unwrap_or_default(),
        updated_at: r.as_ref().map(|r| r.get("updated_at")).unwrap_or_default(),
    };
    let mut machines = vec![as_api("head".into(), "head".into(), ver(st).to_owned(), latest("head".into()).await?)];
    let (_, method) = st.updates.exe_method();
    if machines[0].method.is_empty() {
        machines[0].method = method.name().into();
    }
    for w in st.store.workers().await?.into_iter().filter(|w| w.revoked_at.is_none() && w.role == "worker") {
        let r = latest(w.id.clone()).await?;
        machines.push(as_api(w.id, w.name, w.version.unwrap_or_default(), r));
    }
    let checked = st.updates.checked_at.load(Ordering::SeqCst);
    Ok(api::Updates {
        version: ver(st).to_owned(),
        settings: settings(st).await?,
        releases,
        machines,
        checked_at: (checked > 0).then_some(checked),
        check_error: st.updates.check_error.lock().unwrap().clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        assert_eq!(window("02:00-05:00"), Some((120, 300)));
        assert_eq!(window("23:30–01:00"), Some((1410, 60)));
        assert_eq!(window("25:00-01:00"), None);
        assert!(in_window(""));
        assert!(in_window("00:00-23:59") || in_window("23:59-00:00"));
    }
}
