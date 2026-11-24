//! Best-effort machine observations, independent of durable event delivery.
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
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let roots = match worker.journal.machine_process_roots().await {
            Ok(roots) => roots,
            Err(error) => {
                tracing::warn!(%error, "reading machine attribution roots");
                Vec::new()
            }
        };
        let sampler = worker.machine_sampler.clone();
        // Only one in flight. Do not await outbound capacity or retain history
        // on workers: event ACKs and commands always get priority.
        let sample = tokio::task::spawn_blocking(move || (sampler.lock().unwrap())(&roots)).await;
        if stop.is_cancelled() {
            return;
        }
        if let Ok(sample) = sample {
            send_sample(&tx, &sample);
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
    let scan = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        investigate(&roots, dirs::home_dir().as_deref(), &std::env::temp_dir())
    })
    .await?;
    Ok(serde_json::to_value(scan)?)
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
