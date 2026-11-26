//! Disk exhaustion is a resource wait, not evidence that a session was lost.
//! Sampling follows the worker's existing Unix platform support. Unavailable
//! filesystem samples are unknown, not evidence of exhaustion. OS filesystem
//! calls cannot be interrupted safely: at most two may remain stuck, while
//! connection handling and asynchronous recovery waits stay cancellable.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Available space for the worker's uid (not root's reserved blocks).
#[derive(Clone, Debug)]
pub(crate) struct Space {
    pub mount: PathBuf,
    pub free: u64,
    pub total: u64,
}

pub(crate) const RESUME_BYTES: u64 = 16 * 1024 * 1024;
type Sampler = dyn Fn(&Path) -> Option<Space> + Send + Sync;
const SAMPLE_TTL: Duration = Duration::from_secs(1);
const MAX_PATHS: usize = 64;

#[derive(Default)]
struct Check {
    at: Option<tokio::time::Instant>,
    space: Option<Space>,
    in_flight: bool,
}

pub(crate) struct Monitor {
    sample: Arc<Sampler>,
    warned: Mutex<HashSet<PathBuf>>,
    warning: Mutex<(u32, u64)>,
    cache: Arc<Mutex<HashMap<PathBuf, Check>>>,
    slots: Arc<tokio::sync::Semaphore>,
    changed: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    immediate: bool,
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new(sample)
    }
}

impl Monitor {
    pub(crate) fn new(sample: impl Fn(&Path) -> Option<Space> + Send + Sync + 'static) -> Self {
        Self {
            sample: Arc::new(sample),
            warned: Mutex::default(),
            warning: Mutex::new((10, 20 * 1024 * 1024 * 1024)),
            cache: Arc::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
            changed: Arc::default(),
            #[cfg(test)]
            immediate: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn immediate(sample: impl Fn(&Path) -> Option<Space> + Send + Sync + 'static) -> Self {
        Self { immediate: true, ..Self::new(sample) }
    }

    /// Nonblocking, best-effort snapshot. A probe owns its slot even after
    /// callers drop: Unix filesystem syscalls cannot safely be interrupted.
    /// A stalled mount consumes one of two slots, never an ACK/recovery thread.
    /// Both live probes and historical path metadata are strictly bounded.
    pub(crate) fn space(&self, path: &Path) -> Option<Space> {
        #[cfg(test)]
        if self.immediate {
            return (self.sample)(path);
        }
        let now = tokio::time::Instant::now();
        let mut cache = self.cache.lock().unwrap();
        let prior = cache.get(path).and_then(|check| check.space.clone());
        if cache.get(path).is_some_and(|check| check.in_flight || check.at.is_some_and(|at| now.duration_since(at) < SAMPLE_TTL)) {
            return prior;
        }
        let Ok(permit) = self.slots.clone().try_acquire_owned() else { return prior };
        if !cache.contains_key(path) && cache.len() >= MAX_PATHS {
            let oldest = cache.iter().filter(|(_, check)| !check.in_flight).min_by_key(|(_, check)| check.at).map(|(path, _)| path.clone());
            let Some(oldest) = oldest else { return prior };
            cache.remove(&oldest);
        }
        cache.entry(path.to_owned()).or_default().in_flight = true;
        drop(cache);
        let (cache, sample, changed, path) = (self.cache.clone(), self.sample.clone(), self.changed.clone(), path.to_owned());
        tokio::task::spawn_blocking(move || {
            // No metadata, canonicalize, statvfs, or injected sampler call may
            // run under a cache/warning lock or on an async runtime thread.
            let space = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sample(&path))).ok().flatten();
            cache.lock().unwrap().insert(path, Check { at: Some(tokio::time::Instant::now()), space, in_flight: false });
            drop(permit);
            changed.notify_waiters();
        });
        prior
    }

    /// On an I/O failure, do not classify using a pre-failure healthy sample.
    /// Wait asynchronously for a shared fresh probe, with a bounded deadline
    /// if the filesystem is unresponsive. Dropping this wait is always safe.
    pub(crate) async fn fresh_space(&self, path: &Path) -> Option<Space> {
        #[cfg(test)]
        if self.immediate {
            return self.space(path);
        }
        let requested = tokio::time::Instant::now();
        if let Some(check) = self.cache.lock().unwrap().get_mut(path) {
            check.at = None;
        }
        let until = requested + Duration::from_secs(2);
        loop {
            let changed = self.changed.notified();
            let space = self.space(path);
            if let Some(check) = self.cache.lock().unwrap().get(path).filter(|check| check.at.is_some_and(|at| at >= requested)) {
                return check.space.clone();
            }
            if tokio::time::Instant::now() >= until {
                return space;
            }
            tokio::select! {
                _ = changed => {},
                _ = tokio::time::sleep_until(until.min(tokio::time::Instant::now() + Duration::from_millis(10))) => {},
            }
        }
    }

    pub(crate) fn waiting_reason(&self, path: &Path) -> String {
        self.space(path).map_or_else(
            || {
                format!(
                    "Waiting for disk space on {}: free space unavailable. Recovery continues automatically; no action needed.",
                    path.display()
                )
            },
            |space| space.waiting_reason(),
        )
    }

    pub(crate) fn configure(&self, spec: &c6_proto::CreateSession) {
        *self.warning.lock().unwrap() =
            (spec.disk_warning_percent.unwrap_or(10).min(100), spec.disk_warning_bytes.unwrap_or(20 * 1024 * 1024 * 1024));
    }

    pub(crate) fn retry_warning(&self, mount: &Path) {
        self.warned.lock().unwrap().remove(mount);
    }

    /// Edge-triggered per filesystem; a healthy sample rearms the warning.
    pub(crate) fn warnings(&self, paths: &[PathBuf]) -> Vec<(Space, bool)> {
        let (percent, bytes) = *self.warning.lock().unwrap();
        // Cold/oldest paths get the bounded slots first. Otherwise the first
        // two stale paths in each ping could monopolize both slots forever,
        // leaving (for example) a separately mounted tmp directory unprobed.
        let paths = {
            let cache = self.cache.lock().unwrap();
            let mut paths: Vec<_> = paths.iter().collect();
            paths.sort_by_key(|path| cache.get(*path).and_then(|check| check.at));
            paths
        };
        let mut out = Vec::new();
        for path in paths {
            if let Some(space) = self.space(path) {
                let threshold = (space.total / 100 * u64::from(percent)).max(bytes);
                let mut warned = self.warned.lock().unwrap();
                if space.free < threshold {
                    // Retire an old edge if more than the bounded history of
                    // distinct mounts has been observed. An evicted mount may
                    // warn again; warning metadata must not grow forever.
                    if !warned.contains(&space.mount)
                        && warned.len() >= MAX_PATHS
                        && let Some(oldest) = warned.iter().next().cloned()
                    {
                        warned.remove(&oldest);
                    }
                    if warned.insert(space.mount.clone()) {
                        out.push((space, true));
                    }
                } else if warned.remove(&space.mount) {
                    out.push((space, false));
                }
            }
        }
        out
    }
}

impl Space {
    pub(crate) fn waiting_reason(&self) -> String {
        format!(
            "Waiting for disk space on {}: {} bytes free. Recovery continues automatically; no action needed.",
            self.mount.display(),
            self.free
        )
    }
}

/// Preserve real SQLite codes through anyhow's context chain. A write I/O
/// failure alone is not evidence of exhaustion; it also needs a full sample.
pub(crate) fn resource_error(error: &anyhow::Error, space: Option<&Space>) -> bool {
    let full = space.is_some_and(|s| s.free < RESUME_BYTES);
    let text = format!("{error:#}").to_ascii_lowercase();
    for cause in error.chain() {
        // SQLx can spend its acquire budget trying to reopen a connection
        // after FULL/IOERR, returning a timeout instead of the first SQLite
        // code. Do not turn that loss of detail into a terminal recovery.
        if full && matches!(cause.downcast_ref::<sqlx::Error>(), Some(sqlx::Error::PoolTimedOut | sqlx::Error::Io(_))) {
            return true;
        }
        if let Some(sqlx::Error::Database(db)) = cause.downcast_ref::<sqlx::Error>() {
            let code = db.code().and_then(|c| c.parse::<i32>().ok()).unwrap_or_default();
            if matches!(code & 255, 11 | 26) {
                return false;
            }
            if code & 255 == 13 || (code & 255 == 10 && full) {
                return true;
            }
        }
        if cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
            e.raw_os_error() == Some(nix::libc::ENOSPC)
                || (full && (e.raw_os_error() == Some(nix::libc::EIO) || e.kind() == std::io::ErrorKind::WriteZero))
        }) {
            return true;
        }
    }
    if text.contains("integrity check") || text.contains("database disk image is malformed") || text.contains("not a database") {
        return false;
    }
    text.contains("database or disk is full")
        || text.contains("no space left on device")
        || text.contains("out of diskspace")
        || text.contains("enospc")
}

pub(crate) fn retry_delays(spec: &c6_proto::CreateSession) -> (Duration, Duration) {
    let first =
        Duration::from_secs(u64::from(if spec.disk_recovery_initial_seconds == 0 { 5 } else { spec.disk_recovery_initial_seconds }));
    let max = Duration::from_secs(u64::from(if spec.disk_recovery_max_seconds == 0 { 300 } else { spec.disk_recovery_max_seconds }));
    (first.min(max), max)
}

/// Check once a second, independently of exponential retry delay. Crossing
/// the small headroom threshold wakes recovery on this tick, not in 5 minutes.
pub(crate) async fn wait(monitor: &Monitor, path: &Path, delay: Duration, mut was_full: bool, mut update: impl FnMut(&Space)) {
    let until = tokio::time::Instant::now() + delay;
    loop {
        let changed = monitor.changed.notified();
        if let Some(space) = monitor.space(path) {
            update(&space);
            if was_full && space.free >= RESUME_BYTES {
                return;
            }
            was_full = space.free < RESUME_BYTES;
        }
        if tokio::time::Instant::now() >= until {
            return;
        }
        tokio::select! {
            _ = changed => {},
            _ = tokio::time::sleep_until(until.min(tokio::time::Instant::now() + Duration::from_secs(1))) => {},
        }
    }
}

#[allow(clippy::unnecessary_cast)] // statvfs block counters are u32 on Darwin, u64 on Linux.
pub(crate) fn sample(path: &Path) -> Option<Space> {
    use std::ffi::CString;
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let existing = path.ancestors().find(|p| p.exists())?.canonicalize().ok()?;
    let name = CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut fs = std::mem::MaybeUninit::<nix::libc::statvfs>::uninit();
    // SAFETY: name is NUL terminated; fs points to a writable statvfs.
    if unsafe { nix::libc::statvfs(name.as_ptr(), fs.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and initialized fs.
    let fs = unsafe { fs.assume_init() };
    let dev = existing.metadata().ok()?.dev();
    let mount = existing.ancestors().take_while(|p| p.metadata().is_ok_and(|m| m.dev() == dev)).last()?.to_path_buf();
    #[cfg(target_os = "macos")]
    let mount = {
        let mut info = std::mem::MaybeUninit::<nix::libc::statfs>::uninit();
        // SAFETY: name is NUL terminated, info is writable. Darwin provides
        // the real APFS mount rather than a firmlink's apparent ancestor.
        if unsafe { nix::libc::statfs(name.as_ptr(), info.as_mut_ptr()) } == 0 {
            // SAFETY: successful statfs initializes info and NUL terminates f_mntonname.
            let info = unsafe { info.assume_init() };
            let bytes = unsafe { std::ffi::CStr::from_ptr(info.f_mntonname.as_ptr()) }.to_bytes();
            PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
        } else {
            mount
        }
    };
    Some(Space {
        mount,
        free: (fs.f_bavail as u64).saturating_mul(fs.f_frsize as u64),
        total: (fs.f_blocks as u64).saturating_mul(fs.f_frsize as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[tokio::test]
    async fn warning_batches_prioritize_cold_and_oldest_paths_before_refreshing_recent_paths() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let monitor = Monitor::new(move |_| {
            let _ = blocked.lock().unwrap().recv();
            None
        });
        let paths: Vec<_> = ["/recent", "/oldest", "/cold"].into_iter().map(PathBuf::from).collect();
        let now = tokio::time::Instant::now();
        for (path, age) in [(paths[0].clone(), 2), (paths[1].clone(), 3)] {
            monitor.cache.lock().unwrap().insert(
                path.clone(),
                Check {
                    at: Some(now - Duration::from_secs(age)),
                    space: Some(Space { mount: path, free: 0, total: 1 << 40 }),
                    in_flight: false,
                },
            );
        }
        monitor.warnings(&paths);
        let cache = monitor.cache.lock().unwrap();
        assert!(cache[&paths[2]].in_flight, "a cold path must not starve behind stale cached paths");
        assert!(cache[&paths[1]].in_flight, "the oldest sample gets the remaining slot");
        assert!(!cache[&paths[0]].in_flight);
        drop(cache);
        drop(release);
    }

    #[tokio::test]
    async fn blocked_probes_are_bounded_without_holding_locks_or_recovery_waiters() {
        let calls = Arc::new(AtomicU64::new(0));
        let (entered, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let monitor = Arc::new(Monitor::new({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.send(()).unwrap();
                let _ = blocked.lock().unwrap().recv();
                None
            }
        }));
        for n in 0..256 {
            assert!(monitor.space(Path::new(&format!("/mount/{n}"))).is_none());
        }
        starts.recv().await.unwrap();
        starts.recv().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(monitor.cache.lock().unwrap().len(), 2);
        assert_eq!(monitor.slots.available_permits(), 0);
        assert!(monitor.warned.try_lock().is_ok());
        assert!(monitor.warnings(&[PathBuf::from("/mount/0")]).is_empty());
        monitor.retry_warning(Path::new("/mount/0"));
        monitor.configure(&c6_proto::CreateSession::default());
        let recovery = tokio::spawn({
            let monitor = monitor.clone();
            async move { wait(&monitor, Path::new("/mount/0"), Duration::from_secs(300), true, |_| {}).await }
        });
        let fresh = tokio::spawn({
            let monitor = monitor.clone();
            async move { monitor.fresh_space(Path::new("/mount/0")).await }
        });
        tokio::task::yield_now().await;
        recovery.abort();
        fresh.abort();
        assert!(recovery.await.unwrap_err().is_cancelled());
        assert!(fresh.await.unwrap_err().is_cancelled());
        assert_eq!(calls.load(Ordering::SeqCst), 2, "waiters never create extra probes");
        drop(release);
    }

    #[tokio::test]
    async fn fresh_error_samples_and_historical_path_metadata_are_bounded() {
        let free = Arc::new(AtomicU64::new(1 << 30));
        let monitor = Monitor::new({
            let free = free.clone();
            move |path| Some(Space { mount: path.to_owned(), free: free.load(Ordering::SeqCst), total: 1 << 40 })
        });
        assert_eq!(monitor.fresh_space(Path::new("/state")).await.unwrap().free, 1 << 30);
        free.store(0, Ordering::SeqCst);
        assert_eq!(
            monitor.fresh_space(Path::new("/state")).await.unwrap().free,
            0,
            "I/O classification never uses a pre-error healthy value"
        );
        for n in 0..(MAX_PATHS * 2) {
            let path = PathBuf::from(format!("/history/{n}"));
            assert!(monitor.fresh_space(&path).await.is_some());
            monitor.warnings(&[path]);
            assert!(monitor.cache.lock().unwrap().len() <= MAX_PATHS);
            assert!(monitor.warned.lock().unwrap().len() <= MAX_PATHS);
        }
    }

    #[tokio::test]
    async fn sqlite_codes_distinguish_resource_exhaustion_from_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap();
        let full = Space { mount: "/test".into(), free: 0, total: 1 << 40 };
        let healthy = Space { free: RESUME_BYTES, ..full.clone() };
        for (code, exhausted, available) in
            [(11, false, false), (26, false, false), (13, true, true), (4874, true, false), (778, true, false)]
        {
            journal.inject_recovery_error(code, Arc::new(std::sync::atomic::AtomicBool::new(true)));
            let error = journal.recover_connection().await.unwrap_err();
            assert_eq!(resource_error(&error, Some(&full)), exhausted, "code {code} on full filesystem");
            assert_eq!(resource_error(&error, Some(&healthy)), available, "code {code} on healthy filesystem");
            if code == 13 {
                assert!(
                    resource_error(&error.context("integrity check could not run"), Some(&full)),
                    "a check blocked by FULL is not corruption"
                );
            }
        }
    }

    #[test]
    fn resource_and_corruption_diagnostics_are_distinct() {
        let full = Space { mount: "/test".into(), free: 0, total: 1 << 40 };
        for text in ["database or disk is full", "ENOSPC", "No space left on device", "Out of diskspace"] {
            assert!(resource_error(&anyhow::anyhow!(text), Some(&full)), "{text}");
        }
        for text in ["database disk image is malformed", "file is not a database", "failed integrity check", "unrelated error"] {
            assert!(!resource_error(&anyhow::anyhow!(text), Some(&full)), "{text}");
        }
        let timeout = anyhow::Error::new(sqlx::Error::PoolTimedOut);
        assert!(resource_error(&timeout, Some(&full)));
        assert!(!resource_error(&timeout, None));
        let write = anyhow::Error::new(std::io::Error::from_raw_os_error(nix::libc::EIO));
        assert!(resource_error(&write, Some(&full)));
        assert!(!resource_error(&write, None));
    }

    #[test]
    fn warning_can_be_disabled_without_changing_legacy_defaults() {
        let monitor = Monitor::immediate(|_| Some(Space { mount: "/test".into(), free: 0, total: 1 << 40 }));
        let paths = [PathBuf::from("/test")];
        assert_eq!(monitor.warnings(&paths).len(), 1);
        monitor.configure(&c6_proto::CreateSession { disk_warning_percent: Some(0), disk_warning_bytes: Some(0), ..Default::default() });
        assert_eq!(monitor.warnings(&paths).len(), 1, "disabling clears existing warning");
        assert!(monitor.warnings(&paths).is_empty());
    }

    #[test]
    fn failed_active_warning_rearms_without_waiting_for_space() {
        let monitor = Monitor::immediate(|_| Some(Space { mount: "/test".into(), free: 0, total: 1 << 40 }));
        let paths = [PathBuf::from("/state")];
        let (space, active) = monitor.warnings(&paths).pop().unwrap();
        assert!(active);
        assert!(monitor.warnings(&paths).is_empty());
        monitor.retry_warning(&space.mount);
        assert_eq!(monitor.warnings(&paths).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn free_space_return_wakes_on_tick_not_backoff() {
        let free = Arc::new(AtomicU64::new(0));
        let value = free.clone();
        let monitor =
            Monitor::immediate(move |_| Some(Space { mount: "/test".into(), free: value.load(Ordering::SeqCst), total: 1 << 40 }));
        let task = tokio::spawn(async move { wait(&monitor, Path::new("/test"), Duration::from_secs(300), true, |_| {}).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(200)).await;
        assert!(!task.is_finished());
        free.store(RESUME_BYTES, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(1)).await;
        task.await.unwrap();
    }

    #[test]
    fn warning_fires_once_and_rearms_after_clear() {
        let threshold = 20 * 1024 * 1024 * 1024;
        let free = Arc::new(AtomicU64::new(threshold));
        let value = free.clone();
        let monitor = Monitor::immediate(move |_| {
            Some(Space { mount: "/test".into(), free: value.load(Ordering::SeqCst), total: 100 * 1024 * 1024 * 1024 })
        });
        let paths = [PathBuf::from("/state"), PathBuf::from("/tmp")];
        assert!(monitor.warnings(&paths).is_empty());
        free.store(threshold - 1, Ordering::SeqCst);
        assert_eq!(monitor.warnings(&paths).len(), 1);
        assert!(monitor.warnings(&paths).is_empty());
        free.store(threshold, Ordering::SeqCst);
        let cleared = monitor.warnings(&paths);
        assert_eq!(cleared.len(), 1);
        assert!(!cleared[0].1);
        free.store(0, Ordering::SeqCst);
        assert_eq!(monitor.warnings(&paths).len(), 1);
    }
}
