//! Disk exhaustion is a resource wait, not evidence that a session was lost.
use std::collections::HashSet;
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

pub(crate) struct Monitor {
    sample: Arc<Sampler>,
    warned: Mutex<HashSet<PathBuf>>,
    warning: Mutex<(u32, u64)>,
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new(sample)
    }
}

impl Monitor {
    pub(crate) fn new(sample: impl Fn(&Path) -> Option<Space> + Send + Sync + 'static) -> Self {
        Self { sample: Arc::new(sample), warned: Mutex::default(), warning: Mutex::new((10, 20 * 1024 * 1024 * 1024)) }
    }

    pub(crate) fn space(&self, path: &Path) -> Option<Space> {
        (self.sample)(path)
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
        let mut warned = self.warned.lock().unwrap();
        let mut out = Vec::new();
        for path in paths {
            if let Some(space) = self.space(path) {
                let threshold = (space.total / 100 * u64::from(percent)).max(bytes);
                if space.free < threshold {
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
        tokio::time::sleep_until(until.min(tokio::time::Instant::now() + Duration::from_secs(1))).await;
    }
}

#[allow(clippy::unnecessary_cast)] // statvfs block counters are u32 on Darwin, u64 on Linux.
fn sample(path: &Path) -> Option<Space> {
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
        let monitor = Monitor::new(|_| Some(Space { mount: "/test".into(), free: 0, total: 1 << 40 }));
        let paths = [PathBuf::from("/test")];
        assert_eq!(monitor.warnings(&paths).len(), 1);
        monitor.configure(&c6_proto::CreateSession { disk_warning_percent: Some(0), disk_warning_bytes: Some(0), ..Default::default() });
        assert_eq!(monitor.warnings(&paths).len(), 1, "disabling clears existing warning");
        assert!(monitor.warnings(&paths).is_empty());
    }

    #[test]
    fn failed_active_warning_rearms_without_waiting_for_space() {
        let monitor = Monitor::new(|_| Some(Space { mount: "/test".into(), free: 0, total: 1 << 40 }));
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
        let monitor = Monitor::new(move |_| Some(Space { mount: "/test".into(), free: value.load(Ordering::SeqCst), total: 1 << 40 }));
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
        let monitor = Monitor::new(move |_| {
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
