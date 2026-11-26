//! Best-effort machine observations, independent of durable event delivery.
#[path = "build_folders.rs"]
pub(crate) mod build_folders;

use std::sync::Arc;
use std::time::Duration;

use c6_api::machines::MachineSample;
use c6_proto::{WorkerMsg, worker_msg};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::Worker;

pub(crate) type SampleFn = Box<dyn FnMut(&[(u32, String)]) -> MachineSample + Send>;

pub(crate) fn sampler() -> SampleFn {
    // Initialize on the blocking pool rather than on the worker's run loop.
    let mut sampler = None;
    Box::new(move |roots| sampler.get_or_insert_with(c6_core::machines::Sampler::new).sample_with_roots(roots))
}

fn interval(worker: &Worker) -> Duration {
    let configured = worker.machine_sample_secs.load(std::sync::atomic::Ordering::Relaxed);
    worker
        .overrides
        .lock()
        .unwrap()
        .machine_every
        .unwrap_or_else(|| {
            Duration::from_secs(
                (if configured > 0 {
                    configured
                } else {
                    std::env::var("C6_MACHINE_SAMPLE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(15)
                })
                .clamp(1, 86_400),
            )
        })
        .max(Duration::from_millis(10))
}

pub(crate) async fn run(worker: Arc<Worker>, tx: mpsc::Sender<WorkerMsg>, stop: CancellationToken) {
    let mut tick = tokio::time::interval(interval(&worker));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_cleanup = None;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        // This permit belongs to the worker, not the connection. A probe
        // abandoned during reconnect still owns it until its OS work exits.
        let Ok(permit) = worker.machine_sampling.clone().try_acquire_owned() else { continue };
        let roots = match tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            roots = worker.journal.machine_process_roots() => roots,
        } {
            Ok(roots) => roots,
            Err(error) => {
                tracing::warn!(%error, "reading machine attribution roots");
                Vec::new()
            }
        };
        if stop.is_cancelled() {
            return;
        }
        let sampler = worker.machine_sampler.clone();
        // Hold the permit inside the closure, including while queued on the
        // blocking pool. Cancelling this async wait cannot abort a blocking
        // syscall, but reconnects cannot enqueue more probes behind it.
        let sample = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            (sampler.lock().unwrap())(&roots)
        });
        let sample = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            sample = sample => sample,
        };
        if let Ok(sample) = sample {
            send_sample(&tx, &sample);
            if last_cleanup.is_none_or(|last: tokio::time::Instant| last.elapsed() >= Duration::from_secs(300)) {
                last_cleanup = Some(tokio::time::Instant::now());
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => return,
                    result = maintain_build_folders(&worker, &sample) => {
                        if let Err(error) = result { tracing::debug!(%error, "build folder maintenance skipped"); }
                    }
                }
            }
        }
    }
}

fn send_sample(tx: &mpsc::Sender<WorkerMsg>, sample: &MachineSample) {
    if let Ok(sample_json) = serde_json::to_vec(sample) {
        let _ = tx.try_send(WorkerMsg { msg: Some(worker_msg::Msg::MachineSample(c6_proto::MachineSample { sample_json })) });
    }
}

pub(crate) async fn disk_scan(worker: &Worker) -> anyhow::Result<serde_json::Value> {
    // Reject excess requests rather than spawning an unbounded blocking queue.
    // The permit lives in the closure even if its async caller is cancelled.
    let permit = worker.disk_scan.clone().try_acquire_owned().map_err(|_| anyhow::anyhow!("disk investigation already running"))?;
    let mut roots = worker.sessions.launch_pressure.disk_paths();
    roots.insert(0, worker.state_dir.clone());
    let (references, attribution_partial) = build_context(worker).await?;
    let state = worker.state_dir.clone();
    let scan = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let home = dirs::home_dir();
        let temp = std::env::temp_dir();
        let mut scan = investigate(&roots, home.as_deref(), &temp);
        let references = build_folders::prepare_references(&state, references);
        let (roots, root_partial) = investigation_roots(&roots, home.as_deref(), &temp);
        let builds = build_folders::discover(&roots, &references, &[], build_folders::Limits::default());
        scan.entries += builds.entries;
        scan.partial |= builds.partial || root_partial || attribution_partial;
        scan.build_folders = builds.folders;
        scan
    })
    .await?;
    Ok(serde_json::to_value(scan)?)
}

async fn build_context(worker: &Worker) -> anyhow::Result<(Vec<build_folders::Reference>, bool)> {
    use std::collections::BTreeSet;
    let (mut references, partial) =
        tokio::time::timeout(Duration::from_secs(5), worker.journal.build_folder_places(&worker.identity.meta.worker_id.to_string()))
            .await??;
    let active: BTreeSet<_> = worker
        .journal
        .build_folder_active_sessions()
        .await?
        .into_iter()
        .chain(
            worker
                .sessions
                .liveness_report()
                .sessions
                .into_iter()
                .filter(|s| {
                    s.actor_state != c6_proto::SessionActorState::Idle as i32
                        && s.actor_state != c6_proto::SessionActorState::Stopped as i32
                        || s.running_background_tasks > 0
                        || s.running_background_processes > 0
                })
                .map(|s| s.session_id),
        )
        .collect();
    // A terminal may cd after opening. Conservatively protect every build
    // folder while any terminal is alive, rather than trusting its initial cwd.
    let terminal = worker.terminals.list().iter().any(|t| t.exited.is_none());
    let unknown_active = active.iter().any(|id| !references.iter().any(|r| &r.session_id == id));
    for reference in &mut references {
        reference.active = terminal || active.contains(&reference.session_id);
    }
    Ok((references, partial || terminal || unknown_active))
}

pub(crate) async fn build_folder_archives(worker: &Worker, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct State {
        session_id: String,
        archived_at_ms: Option<i64>,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        sessions: Vec<State>,
    }
    let request: Request = serde_json::from_value(params)?;
    anyhow::ensure!(request.sessions.len() <= 256, "too many archive states");
    let _build_use = worker.journal.build_folder_lifecycle.read().await;
    let states: Vec<_> = request.sessions.into_iter().map(|s| (s.session_id, s.archived_at_ms)).collect();
    worker.journal.set_build_folder_archives(&worker.identity.meta.worker_id.to_string(), &states).await?;
    Ok(serde_json::json!({"updated": states.len()}))
}

pub(crate) async fn remove_build_folders(worker: &Worker, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        path: std::path::PathBuf,
        #[serde(default)]
        incremental_only: bool,
        #[serde(default)]
        confirmed: bool,
    }
    let request: Request = serde_json::from_value(params)?;
    anyhow::ensure!(request.confirmed, "explicit confirmation is required to remove build folders");
    // Never await a queued writer: turns can reserve shell writers while
    // already holding a reader. try_write also makes manual refusal immediate.
    let lifecycle = worker
        .journal
        .build_folder_lifecycle
        .clone()
        .try_write_owned()
        .map_err(|_| anyhow::anyhow!("build folders are in use; try again when sessions are idle"))?;
    let permit = worker.disk_scan.clone().try_acquire_owned().map_err(|_| anyhow::anyhow!("disk investigation already running"))?;
    let (references, partial) = build_context(worker).await?;
    anyhow::ensure!(!partial, "cannot establish complete active build-folder attribution");
    let state = worker.state_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let (_permit, _lifecycle) = (permit, lifecycle);
        anyhow::ensure!(
            request.path.canonicalize().is_ok_and(|path| path == request.path),
            "use the exact canonical path returned by disk investigation"
        );
        let references = build_folders::prepare_references(&state, references);
        // Only a recognized target/custom Rust directory or its incremental
        // child is removable. Never treat an arbitrary requested path as owned.
        anyhow::ensure!(
            !references.iter().any(|r| r.scope && r.path == request.path),
            "a session workspace is not a removable build folder"
        );
        let scan_root = if request.incremental_only {
            build_folders::incremental_scan_root(&build_folders::LocalFs, &request.path)
        } else {
            request.path.clone()
        };
        let scan = build_folders::discover(&[scan_root], &references, &[], build_folders::Limits::default());
        anyhow::ensure!(
            scan.folders.iter().any(|f| std::path::Path::new(&f.path) == request.path),
            "requested path is not a recognized build folder"
        );
        let selected: Vec<_> = if request.incremental_only {
            scan.folders.into_iter().filter(|f| f.incremental && std::path::Path::new(&f.path).starts_with(&request.path)).collect()
        } else {
            scan.folders.into_iter().filter(|f| std::path::Path::new(&f.path) == request.path).collect()
        };
        anyhow::ensure!(!selected.is_empty(), "no recognized build folder at the requested path");
        anyhow::ensure!(!selected.iter().any(|f| f.active), "build folder is active");
        anyhow::ensure!(!selected.iter().any(|f| f.partial), "build folder scan is incomplete; removal is not safe");
        let mut planned = Vec::new();
        for folder in selected {
            let exact = build_folders::CleanupRequest::Confirmed { path: folder.path.clone().into(), confirmed: request.confirmed };
            planned.extend(build_folders::plan(&[folder], &exact, c6_core::now_ms(), build_folders::DEFAULT_ARCHIVED_GRACE));
        }
        Ok::<_, anyhow::Error>(build_folders::cleanup_with(&planned, false, |_| false, build_folders::remove_local))
    })
    .await??;
    Ok(serde_json::to_value(result)?)
}

async fn maintain_build_folders(worker: &Worker, _sample: &MachineSample) -> anyhow::Result<()> {
    // Archive state belongs to the head, not the worker event stream. Refresh
    // before lifecycle exclusion so a slow head cannot block session starts.
    // A disconnected worker must not trust an old archive after an unarchive.
    if !worker.link.is_connected() {
        return Ok(());
    }
    let (places, partial) = worker.journal.build_folder_places(&worker.identity.meta.worker_id.to_string()).await?;
    anyhow::ensure!(!partial, "build folder archive attribution is partial");
    let ids: Vec<_> = places.into_iter().map(|p| p.session_id).collect();
    let context =
        tokio::time::timeout(Duration::from_secs(3), worker.link.call("build_folders.context", serde_json::json!({"session_ids": ids})))
            .await??;
    let states = context["sessions"].as_array().ok_or_else(|| anyhow::anyhow!("invalid build folder archive context"))?;
    for batch in states.chunks(256) {
        build_folder_archives(worker, serde_json::json!({"sessions": batch})).await?;
    }
    let lifecycle =
        worker.journal.build_folder_lifecycle.clone().try_write_owned().map_err(|_| anyhow::anyhow!("session lifecycle is active"))?;
    let permit = worker.disk_scan.clone().try_acquire_owned().map_err(|_| anyhow::anyhow!("disk scan is busy"))?;
    let (references, partial) = build_context(worker).await?;
    anyhow::ensure!(!partial, "build folder attribution is partial");
    let state = worker.state_dir.clone();
    let warning_percent = worker.disk_warning_percent();
    let result = tokio::task::spawn_blocking(move || {
        let (_permit, _lifecycle) = (permit, lifecycle);
        let grace = crate::compile_cache::load(&state).map(|c| Duration::from_secs(u64::from(c.archived_grace_days) * 86_400))?;
        let references = build_folders::prepare_references(&state, references);
        let scan = build_folders::discover(&[], &references, &[], build_folders::Limits::default());
        let (planned, pressure) = maintenance_plan(scan.folders, warning_percent, c6_core::now_ms(), grace, crate::disk::sample);
        let notice_sessions: Vec<_> = planned.iter().flat_map(|f| f.session_ids.clone()).collect();
        let result = build_folders::cleanup_with(&planned, pressure, |_| false, build_folders::remove_local);
        Ok::<_, anyhow::Error>((result, notice_sessions))
    })
    .await??;
    if let Some(notice) = result.0.notice {
        tracing::warn!(%notice, "automatic build folder cleanup");
        if worker.link.call("build_folders.cleaned", serde_json::json!({"message": notice})).await.is_err() {
            worker.sessions.build_cleanup_notice(&result.1, notice).await?;
        }
    }
    for error in result.0.errors {
        tracing::warn!(%error, "automatic build folder cleanup failed");
    }
    Ok(())
}

fn maintenance_plan(
    folders: Vec<c6_api::machines::MachineBuildFolder>,
    warning_percent: f64,
    now_ms: i64,
    grace: Duration,
    mut sample: impl FnMut(&std::path::Path) -> Option<crate::disk::Space>,
) -> (Vec<c6_api::machines::MachineBuildFolder>, bool) {
    use std::collections::BTreeMap;
    // Use statfs/statvfs's actual mount identity, not lexical longest-prefix:
    // /private/tmp is on Darwin's Data volume via an APFS firmlink.
    let mut partitions = BTreeMap::<std::path::PathBuf, (u64, Vec<_>)>::new();
    let mut ordinary = Vec::new();
    for folder in folders {
        let space = sample(std::path::Path::new(&folder.path));
        if let Some(space) = space.filter(|s| s.total > 0) {
            let threshold = (space.total as f64 * warning_percent / 100.0).ceil() as u64;
            let deficit = threshold.saturating_sub(space.free);
            if deficit > 0 {
                let partition = partitions.entry(space.mount).or_default();
                partition.0 = partition.0.max(deficit);
                partition.1.push(folder);
                continue;
            }
        }
        ordinary.push(folder);
    }
    let mut planned = build_folders::plan(&ordinary, &build_folders::CleanupRequest::Archived, now_ms, grace);
    let mut pressure = false;
    for (_, (deficit, folders)) in partitions {
        let cleanup = build_folders::plan(&folders, &build_folders::CleanupRequest::DiskPressure { bytes_to_free: deficit }, now_ms, grace);
        pressure |= !cleanup.is_empty();
        planned.extend(cleanup);
    }
    (planned, pressure)
}

fn investigation_roots(
    known: &[std::path::PathBuf],
    home: Option<&std::path::Path>,
    temp: &std::path::Path,
) -> (Vec<std::path::PathBuf>, bool) {
    use std::path::{Path, PathBuf};
    const MAX_ROOTS: usize = 64;
    fn add(roots: &mut Vec<PathBuf>, path: &Path, partial: &mut bool) {
        if roots.len() >= MAX_ROOTS {
            *partial = true;
            return;
        }
        // Resolve only these trusted starting directories, never traversal's
        // symlinks. /tmp itself is a symlink on macOS.
        let path = match path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                *partial |= error.kind() != std::io::ErrorKind::NotFound;
                return;
            }
        };
        if !path.is_dir() || roots.iter().any(|root| path.starts_with(root)) {
            return;
        }
        if roots.iter().any(|root| root.starts_with(&path)) {
            *partial = true;
            return;
        }
        roots.push(path);
    }
    fn children(roots: &mut Vec<PathBuf>, path: &Path, partial: &mut bool) {
        if path.canonicalize().is_ok_and(|path| roots.iter().any(|root| path.starts_with(root))) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            *partial = true;
            return;
        };
        let entries: Vec<_> = entries.take(129).collect();
        *partial |= entries.len() > 128;
        let mut paths = Vec::new();
        for entry in entries.into_iter().take(128) {
            match entry.and_then(|entry| Ok((entry.file_type()?, entry.path()))) {
                Ok((kind, path)) if kind.is_dir() => paths.push(path),
                Ok(_) => {}
                Err(_) => *partial = true,
            }
        }
        paths.sort();
        for path in paths {
            add(roots, &path, partial);
        }
    }
    let mut roots = Vec::new();
    let mut partial = false;
    if let Some(state) = known.first() {
        add(&mut roots, state, &mut partial);
    }
    if let Some(home) = home {
        add(&mut roots, &home.join("targets"), &mut partial);
        add(&mut roots, &home.join(".cargo"), &mut partial);
    }
    // Broad home/temp ancestors would swallow prioritized roots, so those
    // are covered by bounded child-directory enumeration below instead.
    for path in known.iter().skip(1) {
        if Some(path.as_path()) != home && path != temp {
            add(&mut roots, path, &mut partial);
        }
    }
    add(&mut roots, temp, &mut partial);
    children(&mut roots, temp, &mut partial);
    if let Some(home) = home {
        children(&mut roots, home, &mut partial);
    }
    (roots, partial)
}

fn investigate(known: &[std::path::PathBuf], home: Option<&std::path::Path>, temp: &std::path::Path) -> c6_api::machines::MachineDiskScan {
    let started = std::time::Instant::now();
    let (roots, partial) = investigation_roots(known, home, temp);
    scan_each(
        &roots,
        partial,
        c6_core::now_ms(),
        || started.elapsed(),
        |root, limits| c6_core::machines::scan_disk(&[root.to_owned()], limits),
    )
}

fn scan_each(
    roots: &[std::path::PathBuf],
    partial: bool,
    at_ms: i64,
    elapsed: impl Fn() -> Duration,
    mut scan: impl FnMut(&std::path::Path, c6_core::machines::DiskScanLimits) -> c6_api::machines::MachineDiskScan,
) -> c6_api::machines::MachineDiskScan {
    let limits = c6_core::machines::DiskScanLimits::default();
    let mut result = c6_api::machines::MachineDiskScan { at_ms, partial, ..Default::default() };
    let mut remaining = limits.max_entries;
    for (i, root) in roots.iter().enumerate() {
        let time = limits.max_duration.saturating_sub(elapsed());
        if remaining == 0 || time.is_zero() {
            result.partial = true;
            break;
        }
        let count = roots.len() - i;
        // A huge first directory cannot spend every other root's budget.
        let part = c6_core::machines::DiskScanLimits {
            max_entries: (remaining / count).max(1),
            max_duration: time / count as u32,
            max_folders: limits.max_folders,
        };
        let scan = scan(root, part);
        remaining = remaining.saturating_sub(scan.entries);
        result.entries += scan.entries;
        result.partial |= scan.partial;
        result.folders.extend(scan.folders);
    }
    result.folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    result.folders.truncate(limits.max_folders);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_uses_actual_mount_deficits_without_cross_partition_credit() {
        use c6_api::machines::MachineBuildFolder;
        const GIB: u64 = 1 << 30;
        let folder = |path: &str, bytes, used| MachineBuildFolder {
            path: path.into(),
            bytes,
            last_used_ms: Some(used),
            archived_at_ms: Some(0),
            carbon6_owned: true,
            ..Default::default()
        };
        let folders = vec![
            folder("/private/tmp/a1/target", 60 * GIB, 1),
            folder("/Users/caches/a2/target", 60 * GIB, 2),
            folder("/Users/caches/a3/target", 60 * GIB, 3),
            folder("/Volumes/other/b1/target", 5 * GIB, 1),
            folder("/Volumes/other/b2/target", 5 * GIB, 2),
            folder("/Volumes/other/b3/target", 5 * GIB, 3),
        ];
        let (planned, pressure) = maintenance_plan(folders, 10.0, 100, Duration::ZERO, |path| {
            Some(if path.starts_with("/Volumes/other") {
                crate::disk::Space { mount: "/Volumes/other".into(), free: 0, total: 100 * GIB }
            } else {
                crate::disk::Space { mount: "/System/Volumes/Data".into(), free: 100 * GIB, total: 2 * 1024 * GIB }
            })
        });
        assert!(pressure);
        let paths: Vec<_> = planned.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["/private/tmp/a1/target", "/Users/caches/a2/target", "/Volumes/other/b1/target", "/Volumes/other/b2/target"]);
        let result = build_folders::cleanup_with(&planned, pressure, |_| false, |_| Ok(()));
        assert_eq!(result.bytes, 130 * GIB);
        assert!(result.notice.unwrap().contains("4 inactive carbon6"));
    }

    #[tokio::test]
    async fn build_folder_removal_requires_confirmation_and_excludes_launches_and_only_removes_selected_output() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().canonicalize().unwrap();
        let cwd = state.join("worktrees/repo/session");
        let target = cwd.join("target");
        std::fs::create_dir_all(target.join("debug/incremental")).unwrap();
        std::fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        std::fs::write(target.join("debug/app"), "binary").unwrap();
        std::fs::write(target.join("debug/incremental/object"), "object").unwrap();
        let identity = crate::Identity {
            meta: crate::identity::IdentityMeta {
                worker_id: c6_core::WorkerId::new(),
                worker_name: "build-cleanup".into(),
                head_url: "https://localhost:1".into(),
                tls_domain: None,
            },
            key_pem: String::new(),
            cert_pem: String::new(),
            ca_pem: String::new(),
        };
        let worker = Worker::open_with_resource_sampler(&state, identity, crate::ResourceMeasurement::default).await.unwrap();
        let started = c6_proto::SessionEvent {
            seq: 1,
            ts_ms: 1,
            kind: Some(c6_proto::session_event::Kind::Started(c6_proto::SessionStarted {
                cwd: cwd.to_string_lossy().into_owned(),
                ..Default::default()
            })),
        };
        worker.journal.create_session("s", &c6_proto::CreateSession::default(), &started).await.unwrap();
        assert!(remove_build_folders(&worker, serde_json::json!({"path": target})).await.unwrap_err().to_string().contains("confirmation"));
        let turn = worker.journal.build_folder_lifecycle.clone().read_owned().await;
        assert!(remove_build_folders(&worker, serde_json::json!({"path": target, "confirmed": true})).await.is_err());
        drop(turn);
        let owner = worker.journal.shell_owner("s", "s");
        let reservation = owner.reserve("task").await.unwrap();
        assert!(
            remove_build_folders(&worker, serde_json::json!({"path": target, "confirmed": true}))
                .await
                .unwrap_err()
                .to_string()
                .contains("active")
        );
        assert!(target.join("debug/app").exists());
        owner.not_launched(&reservation).await.unwrap();
        assert!(
            remove_build_folders(&worker, serde_json::json!({"path": state, "incremental_only": true, "confirmed": true}))
                .await
                .unwrap_err()
                .to_string()
                .contains("recognized")
        );
        let result =
            remove_build_folders(&worker, serde_json::json!({"path": target, "incremental_only": true, "confirmed": true})).await.unwrap();
        assert_eq!(result["removed"].as_array().unwrap().len(), 1);
        assert!(!target.join("debug/incremental").exists());
        assert!(target.join("debug/app").exists());
        let result = remove_build_folders(&worker, serde_json::json!({"path": target, "confirmed": true})).await.unwrap();
        assert_eq!(result["removed"].as_array().unwrap().len(), 1);
        assert!(!target.exists());
        let external = tempfile::tempdir().unwrap();
        let external_target = external.path().canonicalize().unwrap().join("custom-cache");
        let incremental = external_target.join("triple/debug/incremental");
        std::fs::create_dir_all(&incremental).unwrap();
        std::fs::write(external_target.join(".rustc_info.json"), "{}").unwrap();
        std::fs::write(external_target.join("triple/debug/app"), "keep").unwrap();
        std::fs::write(incremental.join("object"), "remove").unwrap();
        let result = remove_build_folders(&worker, serde_json::json!({"path": incremental, "incremental_only": true, "confirmed": true}))
            .await
            .unwrap();
        assert_eq!(result["removed"], serde_json::json!([incremental]));
        assert!(!incremental.exists());
        assert!(external_target.join("triple/debug/app").exists(), "explicit incremental removal must preserve external sibling output");
    }

    #[tokio::test]
    async fn reconnects_cancel_without_accumulating_blocked_sampler_probes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let identity = crate::Identity {
            meta: crate::identity::IdentityMeta {
                worker_id: c6_core::WorkerId::new(),
                worker_name: "blocked-telemetry".into(),
                head_url: "https://localhost:1".into(),
                tls_domain: None,
            },
            key_pem: String::new(),
            cert_pem: String::new(),
            ca_pem: String::new(),
        };
        let worker = Worker::open_with_resource_sampler(dir.path(), identity, crate::ResourceMeasurement::default).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered, started) = tokio::sync::oneshot::channel();
        let mut entered = Some(entered);
        // Dropping the sender releases the fake OS call, even if an assertion
        // panics: runtime teardown must never wait forever for this test probe.
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        worker.set_machine_sampler({
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::Relaxed);
                if let Some(entered) = entered.take() {
                    let _ = entered.send(());
                }
                let _ = blocked.recv();
                MachineSample::default()
            }
        });
        let (tx, mut rx) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let first = tokio::spawn(run(worker.clone(), tx.clone(), stop.clone()));
        c6_testkit::within("the first telemetry probe to enter the fake OS call", started).await.unwrap();
        stop.cancel();
        c6_testkit::within("the blocked telemetry connection to stop without releasing its probe", first).await.unwrap();

        for _ in 0..32 {
            let stop = CancellationToken::new();
            let mut reconnect = Box::pin(run(worker.clone(), tx.clone(), stop.clone()));
            assert!(futures::poll!(reconnect.as_mut()).is_pending());
            tokio::task::yield_now().await;
            assert!(futures::poll!(reconnect.as_mut()).is_pending());
            stop.cancel();
            c6_testkit::within("a reconnect to stop while the original telemetry probe is blocked", reconnect).await;
            assert_eq!(worker.machine_sampling.available_permits(), 0);
            // Counting sampler calls alone would miss extra blocking tasks
            // queued on the sampler mutex. Each such task also owns an Arc.
            assert_eq!(Arc::strong_count(&worker.machine_sampler), 2);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
        assert!(rx.try_recv().is_err(), "cancelled connections never send the abandoned result");
        drop(release);
        c6_testkit::within("the abandoned telemetry probe to release its worker permit", async {
            let permit = worker.machine_sampling.acquire().await.unwrap();
            drop(permit);
        })
        .await;
    }

    #[tokio::test]
    async fn telemetry_never_waits_on_capacity_and_keeps_sample_timestamp() {
        let (tx, mut rx) = mpsc::channel(1);
        let sample = MachineSample { at_ms: 1234, cpu_percent: 42.0, ..Default::default() };
        send_sample(&tx, &sample);
        // Saturation drops telemetry, not durable events or acks.
        send_sample(&tx, &MachineSample { at_ms: 9999, ..Default::default() });
        let message = rx.recv().await.unwrap();
        let Some(worker_msg::Msg::MachineSample(wire)) = message.msg else { panic!("expected machine sample") };
        assert_eq!(serde_json::from_slice::<MachineSample>(&wire.sample_json).unwrap(), sample);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn prioritized_scan_roots_do_not_disappear_inside_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = home.join(".local/state/c6w");
        let temp = dir.path().join("temp");
        for path in [&state, &home.join("targets"), &home.join(".cargo"), &home.join("bulk"), &temp] {
            std::fs::create_dir_all(path).unwrap();
        }
        let (roots, _) = investigation_roots(&[state.clone(), home.clone(), temp.clone()], Some(&home), &temp);
        assert_eq!(roots[0], state.canonicalize().unwrap());
        assert_eq!(roots[1], home.join("targets").canonicalize().unwrap());
        assert_eq!(roots[2], home.join(".cargo").canonicalize().unwrap());
        assert!(roots.contains(&home.join("bulk").canonicalize().unwrap()));
        assert!(!roots.contains(&home.canonicalize().unwrap()));
        assert!(!roots.iter().any(|a| roots.iter().any(|b| a != b && a.starts_with(b))));
    }

    #[test]
    fn huge_first_root_cannot_spend_target_or_cargo_budget() {
        let elapsed = std::cell::Cell::new(Duration::ZERO);
        let roots: Vec<_> = ["/state", "/targets", "/cargo"].into_iter().map(std::path::PathBuf::from).collect();
        let mut seen = Vec::new();
        let result = scan_each(
            &roots,
            false,
            1234,
            || elapsed.get(),
            |root, limits| {
                assert!(limits.max_entries > 0);
                assert!(!limits.max_duration.is_zero());
                elapsed.set(elapsed.get() + limits.max_duration);
                seen.push(root.to_owned());
                c6_api::machines::MachineDiskScan { entries: limits.max_entries, partial: true, ..Default::default() }
            },
        );
        assert_eq!(seen, roots);
        assert_eq!(result.at_ms, 1234);
        assert!(result.partial);
        assert_eq!(result.entries, 100_000);
        assert_eq!(elapsed.get(), Duration::from_secs(5));
    }
}
