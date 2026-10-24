//! Updating this worker when the head says a release is out
//! (docs/updates.md §5): it checks the release's signature itself, waits
//! for its sessions to go idle (or the deadline), installs the new version
//! the way this one was installed, and exits so the service manager starts
//! it. The new version, once attached, says it's done; one that never gets
//! there is rolled back.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_core::now_ms;
use c6_core::update::{self as up, Method, OnStart, Pending, Restart};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::Worker;

#[derive(Default)]
pub struct Updating {
    /// An update asked the process to exit with the restart code.
    pub restart: AtomicBool,
    busy: AtomicBool,
    stop: Mutex<Option<CancellationToken>>,
    on_start: Mutex<Option<OnStart>>,
}

impl Updating {
    pub fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::SeqCst)
    }
}

/// From `conn::run`: this version was rolled back; exit with the restart
/// code so the previous one runs.
#[derive(Debug)]
pub struct RolledBack(pub String);

impl std::fmt::Display for RolledBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RolledBack {}

fn dir(worker: &Worker) -> PathBuf {
    worker.state_dir.join("updates")
}

/// The binary to replace and how it was installed. Debug builds (tests)
/// may name another binary on the worker.
fn exe_method(w: &Worker) -> (PathBuf, Method) {
    if cfg!(debug_assertions)
        && let Some(exe) = w.overrides.lock().unwrap().update_exe.clone()
    {
        let m = up::method(&exe, false);
        return (exe, m);
    }
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).unwrap_or_default();
    let m = up::method(&exe, cfg!(debug_assertions));
    (exe, m)
}

/// The version this worker runs (debug builds may say another, for tests).
fn version(w: &Worker) -> String {
    if cfg!(debug_assertions)
        && let Some(v) = w.overrides.lock().unwrap().update_as_version.clone()
    {
        return v;
    }
    c6_core::build().to_owned()
}

/// Release keys: the built-in ones (debug builds may name others).
fn keys(w: &Worker) -> Vec<String> {
    if cfg!(debug_assertions)
        && let Some(k) = w.overrides.lock().unwrap().update_keys.clone()
    {
        return k;
    }
    up::RELEASE_KEYS.iter().map(|k| k.to_string()).collect()
}

/// First thing in `conn::run`: an update in progress counts this start.
pub fn on_start(worker: &Worker, stop: &CancellationToken) -> anyhow::Result<()> {
    *worker.update.stop.lock().unwrap() = Some(stop.clone());
    let (exe, method) = exe_method(worker);
    let s = up::on_start(&dir(worker), &version(worker), &method, "c6w", &exe);
    if let OnStart::RolledBack(p) = &s {
        return Err(RolledBack(p.detail.clone()).into());
    }
    *worker.update.on_start.lock().unwrap() = Some(s);
    Ok(())
}

/// Attached to the head: the update that brought this version is done (or
/// was rolled back), and the head hears it.
pub fn attached(worker: &Arc<Worker>) {
    let s = worker.update.on_start.lock().unwrap().take();
    let (_, method) = exe_method(worker);
    let report = match s {
        Some(OnStart::Trying(_)) => up::healthy(&dir(worker)).map(|p| json!({"version": p.to, "state": "done", "method": method.name()})),
        Some(OnStart::WasRolledBack(p)) => {
            Pending::clear(&dir(worker));
            Some(json!({"version": p.to, "state": "rolled_back", "method": method.name(), "detail": p.detail}))
        }
        _ => None,
    };
    if let Some(r) = report {
        let w = worker.clone();
        tokio::spawn(async move {
            if let Err(e) = w.link.call("update.status", r).await {
                warn!("telling the head how the update went: {e:#}");
            }
        });
    }
}

/// `update.apply` from the head.
pub async fn apply(worker: &Arc<Worker>, p: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
    let (raw, sig) = (s("manifest"), hex::decode(s("signature"))?);
    let m = up::verify(raw.as_bytes(), &sig, &keys(worker))?;
    if !up::newer(&m.version, &version(worker)) {
        anyhow::bail!("this worker runs {} already", version(worker));
    }
    if worker.update.busy.swap(true, Ordering::SeqCst) {
        return Ok(json!({"already": true}));
    }
    let (w, base, deadline) = (worker.clone(), s("base_url"), p["deadline_ms"].as_i64().unwrap_or(0));
    tokio::spawn(async move {
        if let Err(e) = run(&w, &m, (&raw, &sig), &base, deadline).await {
            warn!(to = %m.version, "updating: {e:#}");
            say(&w, &m.version, "failed", &format!("{e:#}")).await;
            w.update.busy.store(false, Ordering::SeqCst);
        }
    });
    Ok(json!({"started": true}))
}

/// Tells the head how the update goes (`update.status`).
async fn say(w: &Worker, version: &str, state: &str, detail: &str) {
    let r = json!({"version": version, "state": state, "method": exe_method(w).1.name(), "detail": detail});
    if let Err(e) = w.link.call("update.status", r).await {
        warn!("telling the head how the update goes: {e:#}");
    }
}

async fn run(w: &Arc<Worker>, m: &up::Manifest, signed: (&str, &[u8]), base: &str, deadline: i64) -> anyhow::Result<()> {
    let (exe, method) = exe_method(w);
    let v = m.version.as_str();
    if method == Method::Source {
        anyhow::bail!("built from source: update it yourself");
    }
    // Idle first: no session in the middle of a turn (or the deadline).
    let mut said = usize::MAX;
    loop {
        let busy = w.sessions.running();
        if busy == 0 || (deadline > 0 && now_ms() >= deadline) {
            break;
        }
        if busy != said {
            say(w, v, "waiting", &format!("{busy} session{} running", if busy == 1 { "" } else { "s" })).await;
            said = busy;
        }
        tokio::time::sleep(Duration::from_secs(if cfg!(debug_assertions) { 1 } else { 15 })).await;
    }
    say(w, v, "downloading", "").await;
    let asset = match &method {
        // Homebrew installs a release itself; an edge build is put in place.
        Method::Brew { .. } if !up::in_place(&method, m) => None,
        _ => {
            let (name, sum) = up::asset(m, "c6w", method.name(), std::env::consts::OS, std::env::consts::ARCH)
                .ok_or_else(|| anyhow::anyhow!("the release has no c6w for {} {}", std::env::consts::OS, std::env::consts::ARCH))?;
            let r = reqwest::Client::new()
                .get(format!("{base}{name}"))
                .header("User-Agent", format!("carbon6/{}", c6_core::VERSION))
                .timeout(Duration::from_secs(600))
                .send()
                .await?
                .error_for_status()?;
            let bytes = r.bytes().await?.to_vec();
            if up::checksum(&bytes) != sum {
                anyhow::bail!("{name} isn't the file the release signed");
            }
            Some((name, bytes))
        }
    };
    say(w, v, "installing", "").await;
    // Before installing: a .deb's postinst restarts the worker mid-install.
    Pending {
        from: version(w),
        to: m.version.clone(),
        state: "installed".into(),
        started_at: now_ms(),
        in_place: up::in_place(&method, m),
        ..Default::default()
    }
    .save(&dir(w))?;
    let (m2, raw, sig, exe2, method2) = (m.clone(), signed.0.to_owned(), signed.1.to_vec(), exe.clone(), method.clone());
    let restart = tokio::task::spawn_blocking(move || {
        up::install(&method2, "c6w", &exe2, &m2, (raw.as_bytes(), &sig), asset.as_ref().map(|(n, b)| (n.as_str(), b.as_slice())))
    })
    .await?
    .map_err(|e| {
        Pending::clear(&dir(w));
        anyhow::anyhow!(e)
    })?;
    say(w, v, "restarting", "").await;
    info!(to = %m.version, "updated; restarting");
    if restart == Restart::Exit {
        w.update.restart.store(true, Ordering::SeqCst);
        if let Some(stop) = w.update.stop.lock().unwrap().clone() {
            stop.cancel();
        }
    }
    Ok(())
}
