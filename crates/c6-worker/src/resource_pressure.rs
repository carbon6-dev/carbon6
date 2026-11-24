//! Best-effort admission from the worker's current local resources. This
//! cannot predict the resources a model, tool, or an SSH host will need later.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;
const START_WINDOW: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Default)]
pub struct Measurement {
    /// True when a relevant writable partition is below its admission floor.
    pub disk_nearly_full: bool,
    // Available bytes and capacity, for the most constrained observed domain.
    pub memory: Option<(u64, u64)>,
    pub memory_pressure: bool,
    // One-minute system load divided by the system's online CPUs.
    pub load: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pressure {
    Sampling,
    Disk,
    Memory,
    Load,
}

impl Pressure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Sampling => "Queued: checking worker resources. Starts when the check completes.",
            Self::Disk => "queued: disk nearly full. Starts when space is available.",
            Self::Memory => "Queued: worker busy (memory pressure). Starts when the worker has headroom.",
            Self::Load => "Queued: worker busy (high system load). Starts when the worker has headroom.",
        }
    }

    pub(crate) async fn announce(self, writer: &crate::session::Writer) {
        use c6_agent::EventSink;
        use c6_proto::{Notice, session_event};
        let notice = Notice { title: "Subagent queued".into(), message: self.message().into(), priority: "low".into(), quiet: true };
        if let Err(error) = writer.append(session_event::Kind::Notice(notice)).await {
            tracing::warn!(session = %writer.session_id, %error, "recording subagent admission delay");
        }
    }
}

#[derive(Default)]
struct State {
    starts: VecDeque<Instant>,
}

#[derive(Default)]
struct Cached {
    sample: Option<(Instant, u64, Measurement)>,
    in_flight: bool,
}

struct Probe {
    cache: Mutex<Cached>,
    measure: Arc<dyn Fn() -> Measurement + Send + Sync>,
    // None is only used by deterministic unit tests. All production probes,
    // including the injected worker sampler, run on the blocking pool.
    pool: Option<Arc<tokio::sync::Semaphore>>,
}

impl Probe {
    fn snapshot(self: &Arc<Self>, generation: u64) -> Option<Measurement> {
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap();
        let sample = cache.sample.filter(|(_, version, _)| *version == generation);
        if sample.is_some_and(|(at, _, _)| now.saturating_duration_since(at) < RETRY) || cache.in_flight {
            return sample.map(|(_, _, m)| m);
        }
        if let Some(pool) = &self.pool {
            let Ok(permit) = pool.clone().try_acquire_owned() else { return sample.map(|(_, _, m)| m) };
            cache.in_flight = true;
            drop(cache);
            let probe = self.clone();
            tokio::task::spawn_blocking(move || {
                // No cache/accounting/path lock is held across OS calls. A
                // hung filesystem consumes one bounded slot, not a waiter or
                // runtime thread. Global and scoped pools are separate.
                let measured = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (probe.measure)()));
                let mut cache = probe.cache.lock().unwrap();
                cache.in_flight = false;
                if let Ok(m) = measured {
                    cache.sample = Some((Instant::now(), generation, m));
                }
                drop(permit);
            });
            sample.map(|(_, _, m)| m)
        } else {
            drop(cache);
            let m = (self.measure)();
            self.cache.lock().unwrap().sample = Some((now, generation, m));
            Some(m)
        }
    }
}

struct Scope {
    probe: Arc<Probe>,
}

type DiskMeasure = dyn Fn(&Path, f64) -> bool + Send + Sync;

struct Shared {
    state: Mutex<State>,
    global: Arc<Probe>,
    disk_percent: Arc<std::sync::atomic::AtomicU64>,
    paths: Vec<PathBuf>,
    scopes: Mutex<BTreeMap<PathBuf, Weak<Scope>>>,
    scope_pool: Arc<tokio::sync::Semaphore>,
    disk_measure: Option<Arc<DiskMeasure>>,
}

#[derive(Clone)]
pub(crate) struct Gate {
    shared: Arc<Shared>,
    scope: Option<Arc<Scope>>,
}

/// A charged start can be handed from Runner to its child once. Delayed
/// setup cannot redeem it after the gate's startup accounting has expired.
pub(crate) struct Admission(Instant);

impl Admission {
    pub(crate) fn redeem(self) -> bool {
        self.0.elapsed() < START_WINDOW
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::for_state(&std::env::temp_dir())
    }
}

impl Gate {
    pub(crate) fn for_state(state_dir: &std::path::Path) -> Self {
        let paths = vec![
            state_dir.to_owned(),
            state_dir.join("worktrees"),
            state_dir.join("kitjobs"),
            std::env::temp_dir(),
            dirs::home_dir().unwrap_or_else(|| state_dir.to_owned()),
        ];
        let disk_paths = paths.clone();
        let disk_percent = Arc::new(std::sync::atomic::AtomicU64::new(disk_hold_percent().to_bits()));
        let threshold = disk_percent.clone();
        let mut gate = Self::new(move || {
            let mut m = system();
            m.disk_nearly_full = disk_nearly_full(&paths, f64::from_bits(threshold.load(std::sync::atomic::Ordering::Relaxed)));
            m
        });
        let shared = Arc::get_mut(&mut gate.shared).unwrap();
        shared.disk_percent = disk_percent;
        shared.paths = disk_paths;
        shared.disk_measure = Some(Arc::new(|path, percent| disk_nearly_full(&[path.to_owned()], percent)));
        gate
    }

    /// Original folders and externally mounted worktrees need admission on
    /// their own filesystem, not just the partition containing the journal.
    /// A view checks only this cwd plus the mandatory global paths. Views of
    /// the same cwd share a probe; their last drop retires its registration,
    /// even if an OS call is still blocked. Never aggregate cwd checks into
    /// global admission: an unrelated session must not inherit this disk.
    pub(crate) fn scoped(&self, path: &Path) -> Self {
        let Some(measure) = self.shared.disk_measure.clone().filter(|_| path.is_absolute()) else { return self.clone() };
        let mut scopes = self.shared.scopes.lock().unwrap();
        scopes.retain(|_, scope| scope.strong_count() > 0);
        let scope = scopes.get(path).and_then(Weak::upgrade).unwrap_or_else(|| {
            let path = path.to_owned();
            let key = path.clone();
            let percent = self.shared.disk_percent.clone();
            let scope = Arc::new(Scope {
                probe: Arc::new(Probe {
                    cache: Mutex::default(),
                    measure: Arc::new(move || Measurement {
                        disk_nearly_full: measure(&path, f64::from_bits(percent.load(std::sync::atomic::Ordering::Relaxed))),
                        ..Default::default()
                    }),
                    pool: Some(self.shared.scope_pool.clone()),
                }),
            });
            scopes.insert(key, Arc::downgrade(&scope));
            scope
        });
        Self { shared: self.shared.clone(), scope: Some(scope) }
    }

    pub(crate) fn disk_paths(&self) -> Vec<std::path::PathBuf> {
        let mut scopes = self.shared.scopes.lock().unwrap();
        scopes.retain(|_, scope| scope.strong_count() > 0);
        self.shared.paths.iter().cloned().chain(scopes.keys().cloned()).collect()
    }

    pub(crate) fn configure_disk(&self, percent: Option<f64>) {
        let percent = percent.filter(|v| v.is_finite() && (0.0..=100.0).contains(v)).unwrap_or_else(disk_hold_percent);
        self.shared.disk_percent.store(percent.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) async fn wait_disk(&self, cancel: &CancellationToken) -> Result<(), String> {
        self.wait_disk_with(cancel, || {}).await
    }

    pub(crate) async fn wait_disk_with(&self, cancel: &CancellationToken, queued: impl FnOnce() + Send) -> Result<(), String> {
        let mut queued = Some(queued);
        loop {
            if cancel.is_cancelled() {
                return Err("kit job was stopped before it started".into());
            }
            match self.measurement() {
                Ok(m) if !m.disk_nearly_full => return Ok(()),
                Ok(_) => {
                    if let Some(queued) = queued.take() {
                        queued();
                    }
                }
                Err(_) => {}
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err("kit job was stopped before it started".into()),
                _ = tokio::time::sleep(RETRY) => {}
            }
        }
    }

    pub(crate) fn new(measure: impl Fn() -> Measurement + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::default(),
                global: Arc::new(Probe {
                    cache: Mutex::default(),
                    measure: Arc::new(measure),
                    pool: Some(Arc::new(tokio::sync::Semaphore::new(1))),
                }),
                disk_percent: Arc::new(std::sync::atomic::AtomicU64::new(5.0f64.to_bits())),
                paths: Vec::new(),
                scopes: Mutex::default(),
                scope_pool: Arc::new(tokio::sync::Semaphore::new(2)),
                disk_measure: None,
            }),
            scope: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn immediate(measure: impl Fn() -> Measurement + Send + Sync + 'static) -> Self {
        let mut gate = Self::new(measure);
        Arc::get_mut(&mut Arc::get_mut(&mut gate.shared).unwrap().global).unwrap().pool = None;
        gate
    }

    fn measurement(&self) -> Result<Measurement, Pressure> {
        let version = self.shared.disk_percent.load(std::sync::atomic::Ordering::Relaxed);
        let global = self.shared.global.snapshot(version);
        let scoped = self.scope.as_ref().map(|scope| scope.probe.snapshot(version));
        // A known full partition wins over a different pending probe, so the
        // caller can show the useful disk queue reason immediately.
        if global.is_some_and(|m| m.disk_nearly_full) || scoped.flatten().is_some_and(|m| m.disk_nearly_full) {
            return Ok(Measurement { disk_nearly_full: true, ..global.unwrap_or_default() });
        }
        if scoped.is_some_and(|m| m.is_none()) {
            return Err(Pressure::Sampling);
        }
        global.ok_or(Pressure::Sampling)
    }

    /// Budget briefly against starts that may not be visible in the sample
    /// yet. Never retain a reservation for a whole (possibly nested) turn.
    pub(crate) fn try_start(&self) -> Result<(), Pressure> {
        let now = Instant::now();
        let m = self.measurement()?;
        let mut s = self.shared.state.lock().unwrap();
        while s.starts.front().is_some_and(|at| now.saturating_duration_since(*at) >= START_WINDOW) {
            s.starts.pop_front();
        }
        if m.disk_nearly_full {
            return Err(Pressure::Disk);
        }
        if m.memory_pressure
            || m.memory.is_some_and(|(available, capacity)| {
                let allowance = (128 * MIB).min(capacity / 8).saturating_mul(1 + s.starts.len() as u64);
                capacity == 0 || available < floor(capacity).saturating_add(allowance)
            })
        {
            return Err(Pressure::Memory);
        }
        if m.load.is_some_and(|load| load >= 1.25) {
            return Err(Pressure::Load);
        }
        if m.memory.is_some() {
            s.starts.push_back(now);
        }
        Ok(())
    }

    /// The caller owns the queued work; dropping the future removes it.
    pub(crate) async fn wait<F, Fut>(&self, cancel: &CancellationToken, mut queued: F) -> Result<Admission, String>
    where
        F: FnMut(Pressure) -> Fut + Send,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let mut announced = false;
        loop {
            if cancel.is_cancelled() {
                return Err("subagent was stopped before it started".into());
            }
            match self.try_start() {
                Ok(()) => return Ok(Admission(Instant::now())),
                Err(Pressure::Sampling) => {}
                Err(pressure) if !announced => {
                    queued(pressure).await;
                    announced = true;
                }
                Err(_) => {}
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err("subagent was stopped before it started".into()),
                _ = tokio::time::sleep(RETRY) => {}
            }
        }
    }
}

pub(crate) fn disk_hold_percent() -> f64 {
    std::env::var("C6_MACHINE_DISK_HOLD_PERCENT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
        .unwrap_or(5.0)
}

fn below_disk_floor(available: u64, total: u64, percent: f64) -> bool {
    total > 0 && (available as f64) < total as f64 * percent / 100.0
}

// Filesystem counter widths differ across supported Unix targets.
#[allow(clippy::unnecessary_cast)]
fn disk_nearly_full(paths: &[std::path::PathBuf], percent: f64) -> bool {
    #[cfg(unix)]
    for path in paths {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { continue };
        let mut stat = std::mem::MaybeUninit::<nix::libc::statvfs>::uninit();
        // SAFETY: path is terminated; statvfs initializes stat on success.
        if unsafe { nix::libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0 {
            let stat = unsafe { stat.assume_init() };
            if stat.f_flag & nix::libc::ST_RDONLY == 0 && below_disk_floor(stat.f_bavail as u64, stat.f_blocks as u64, percent) {
                return true;
            }
        }
    }
    false
}

#[async_trait::async_trait]
impl c6_tools::kits::Admission for Gate {
    async fn wait(&self, ctx: &c6_tools::ToolCtx) -> Result<(), String> {
        let gate = self.scoped(&ctx.session.cwd());
        let mut announced = false;
        loop {
            if ctx.cancel.is_cancelled() {
                return Err("kit job was stopped before it started".into());
            }
            // Kits only use disk admission, not the model-start memory budget.
            let full = match gate.measurement() {
                Ok(m) if !m.disk_nearly_full => return Ok(()),
                Ok(_) => true,
                Err(_) => false,
            };
            if full && !announced {
                if let Some(sink) = ctx.session.notices() {
                    let _ = sink.send(c6_tools::Notice {
                        title: "Kit queued".into(),
                        message: Pressure::Disk.message().into(),
                        priority: "low".into(),
                    });
                }
                announced = true;
            }
            tokio::select! {
                _ = ctx.cancel.cancelled() => return Err("kit job was stopped before it started".into()),
                _ = tokio::time::sleep(RETRY) => {}
            }
        }
    }
}

pub(crate) fn retry_at() -> Instant {
    Instant::now() + RETRY
}

fn floor(capacity: u64) -> u64 {
    (capacity / 10).clamp(128 * MIB, 512 * MIB).min(capacity / 4)
}

fn system() -> Measurement {
    let mut m = Measurement::default();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use nix::libc;
        let mut load = 0.0f64;
        // SAFETY: load points to one writable double; sysconf takes a constant.
        let (count, cpus) = unsafe { (libc::getloadavg(&mut load, 1), libc::sysconf(libc::_SC_NPROCESSORS_ONLN)) };
        m.load = (count == 1 && cpus > 0 && load.is_finite() && load >= 0.0).then_some(load / cpus as f64);
    }
    #[cfg(target_os = "linux")]
    {
        m.memory = std::fs::read_to_string("/proc/meminfo").ok().as_deref().and_then(meminfo);
        // A healthy host can have a constrained worker cgroup or ancestor.
        if let Ok(groups) = std::fs::read_to_string("/proc/self/cgroup")
            && let Some(path) = groups.lines().find_map(|line| line.strip_prefix("0::/"))
        {
            let root = std::path::Path::new("/sys/fs/cgroup");
            let group = root.join(path);
            for dir in group.ancestors().take_while(|d| d.starts_with(root)) {
                let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok()?.trim().parse::<u64>().ok();
                let limit = [read("memory.high"), read("memory.max")].into_iter().flatten().min();
                if let Some((limit, used)) = limit.zip(read("memory.current")) {
                    let remaining = limit.saturating_sub(used);
                    let headroom = |available, capacity| i128::from(available) - i128::from(floor(capacity));
                    if m.memory.is_none_or(|(a, c)| headroom(remaining, limit) < headroom(a, c)) {
                        m.memory = Some((remaining, limit));
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        use nix::libc;
        let mut level = 0i32;
        let mut len = std::mem::size_of_val(&level);
        // SAFETY: this read-only sysctl writes at most len bytes into level.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.memorystatus_vm_pressure_level".as_ptr(),
                (&mut level as *mut i32).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        // Darwin levels: normal=1, warning=2, critical=4.
        m.memory_pressure = result == 0 && len == std::mem::size_of_val(&level) && level >= 2;
    }
    m
}

#[cfg(target_os = "linux")]
fn meminfo(text: &str) -> Option<(u64, u64)> {
    let read = |name: &str| {
        text.lines().find_map(|line| line.strip_prefix(name))?.split_whitespace().next()?.parse::<u64>().ok()?.checked_mul(1024)
    };
    Some((read("MemAvailable:")?, read("MemTotal:")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn disk_floor_is_strict_and_handles_unknown_capacity() {
        assert!(below_disk_floor(4, 100, 5.0));
        assert!(!below_disk_floor(5, 100, 5.0));
        assert!(!below_disk_floor(0, 0, 5.0));
        assert!(!below_disk_floor(0, 100, 0.0));
    }

    #[tokio::test]
    async fn external_full_disk_is_scoped_shared_and_released() {
        let mut gate = Gate::immediate(Measurement::default);
        let shared = Arc::get_mut(&mut gate.shared).unwrap();
        shared.paths = vec![PathBuf::from("/mandatory")];
        shared.disk_measure = Some(Arc::new(|path, _| path == Path::new("/full")));
        let full = gate.scoped(Path::new("/full"));
        let same = gate.scoped(Path::new("/full"));
        assert!(Arc::ptr_eq(full.scope.as_ref().unwrap(), same.scope.as_ref().unwrap()));
        let healthy = gate.scoped(Path::new("/healthy"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while full.try_start() == Err(Pressure::Sampling) || healthy.try_start() == Err(Pressure::Sampling) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(full.try_start(), Err(Pressure::Disk));
        assert_eq!(gate.try_start(), Ok(()), "external full disks never poison global admission");
        assert_eq!(healthy.try_start(), Ok(()));
        drop(full);
        assert!(gate.disk_paths().contains(&PathBuf::from("/full")));
        drop(same);
        assert!(!gate.disk_paths().contains(&PathBuf::from("/full")));
        drop(healthy);
        assert_eq!(gate.disk_paths(), vec![PathBuf::from("/mandatory")]);
    }

    #[tokio::test]
    async fn blocked_measurement_is_shared_bounded_and_does_not_block_cancellation_or_locks() {
        let count = Arc::new(AtomicUsize::new(0));
        let (entered, started) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let gate = Arc::new(Gate::new({
            let count = count.clone();
            move || {
                count.fetch_add(1, Ordering::Relaxed);
                if let Some(entered) = entered.lock().unwrap().take() {
                    let _ = entered.send(());
                }
                let _ = blocked.lock().unwrap().recv();
                Measurement::default()
            }
        }));
        assert_eq!(gate.try_start(), Err(Pressure::Sampling));
        started.await.unwrap();
        let cancel = CancellationToken::new();
        let mut waiters = Vec::new();
        for n in 0..64 {
            assert_eq!(gate.try_start(), Err(Pressure::Sampling));
            let (gate, cancel) = (gate.clone(), cancel.clone());
            waiters.push(tokio::spawn(async move {
                if n % 2 == 0 { gate.wait_disk(&cancel).await } else { gate.wait(&cancel, |_| std::future::ready(())).await.map(|_| ()) }
            }));
        }
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(gate.shared.state.try_lock().is_ok());
        assert!(gate.shared.global.cache.try_lock().is_ok());
        gate.configure_disk(Some(4.0));
        tokio::task::yield_now().await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), async {
            for waiter in waiters {
                assert!(waiter.await.unwrap().is_err());
            }
        })
        .await
        .unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1);
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while gate.try_start() != Ok(()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 2, "a configuration change discards the old in-flight result");
    }

    #[tokio::test(start_paused = true)]
    async fn background_samples_are_cached_and_refreshed_using_the_shared_clock() {
        let full = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Gate::new({
            let (full, calls) = (full.clone(), calls.clone());
            move || {
                calls.fetch_add(1, Ordering::Relaxed);
                Measurement { disk_nearly_full: full.load(Ordering::Relaxed), ..Default::default() }
            }
        });
        assert_eq!(gate.try_start(), Err(Pressure::Sampling));
        while gate.shared.global.cache.lock().unwrap().in_flight {
            tokio::task::yield_now().await;
        }
        assert_eq!(gate.try_start(), Err(Pressure::Disk));
        full.store(false, Ordering::Relaxed);
        for _ in 0..64 {
            assert_eq!(gate.try_start(), Err(Pressure::Disk));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        tokio::time::advance(RETRY).await;
        assert_eq!(gate.try_start(), Err(Pressure::Disk), "refresh returns the last known sample without waiting");
        while gate.shared.global.cache.lock().unwrap().in_flight {
            tokio::task::yield_now().await;
        }
        assert_eq!(gate.try_start(), Ok(()));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn scoped_probes_are_bounded_and_pending_probes_do_not_keep_scopes_alive() {
        let mut gate = Gate::immediate(Measurement::default);
        let count = Arc::new(AtomicUsize::new(0));
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        Arc::get_mut(&mut gate.shared).unwrap().disk_measure = Some(Arc::new({
            let count = count.clone();
            move |_, _| {
                count.fetch_add(1, Ordering::Relaxed);
                let _ = blocked.lock().unwrap().recv();
                false
            }
        }));
        let scopes: Vec<_> = (0..64).map(|n| gate.scoped(Path::new(&format!("/external/{n}")))).collect();
        for scope in &scopes {
            assert_eq!(scope.try_start(), Err(Pressure::Sampling));
        }
        assert_eq!(gate.shared.scope_pool.available_permits(), 0);
        assert!(count.load(Ordering::Relaxed) <= 2);
        assert_eq!(gate.try_start(), Ok(()), "scoped probes cannot starve the global probe");
        drop(scopes);
        assert!(gate.disk_paths().is_empty(), "blocking tasks own caches, not active-path registrations");
        drop(release);
    }

    #[tokio::test(start_paused = true)]
    async fn disk_holds_subagents_and_kits_releases_and_cancels() {
        let full = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let gate = Arc::new(Gate::immediate({
            let full = full.clone();
            move || Measurement { disk_nearly_full: full.load(Ordering::Relaxed), ..Default::default() }
        }));
        let cancel = CancellationToken::new();
        let (queued, mut reasons) = tokio::sync::mpsc::unbounded_channel();
        let subagent = tokio::spawn({
            let (gate, cancel) = (gate.clone(), cancel.clone());
            async move {
                gate.wait(&cancel, |reason| {
                    queued.send(reason).unwrap();
                    std::future::ready(())
                })
                .await
            }
        });
        assert_eq!(reasons.recv().await, Some(Pressure::Disk));
        let kit = tokio::spawn({
            let (gate, cancel) = (gate.clone(), cancel.clone());
            async move { gate.wait_disk(&cancel).await }
        });
        tokio::task::yield_now().await;
        assert!(!kit.is_finished());
        full.store(false, Ordering::Relaxed);
        tokio::time::advance(RETRY).await;
        assert!(subagent.await.unwrap().is_ok());
        assert!(kit.await.unwrap().is_ok());
        full.store(true, Ordering::Relaxed);
        tokio::time::advance(RETRY).await;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(gate.wait_disk(&cancelled).await.is_err());
        assert!(gate.wait(&cancelled, |_| std::future::ready(())).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn session_kit_admission_announces_queue_and_is_cancellable() {
        let gate = Arc::new(Gate::immediate(|| Measurement { disk_nearly_full: true, ..Default::default() }));
        let session = Arc::new(c6_tools::SessionTools::new(std::path::PathBuf::from("/test")));
        session.set_kit_admission(gate);
        let (tx, mut notices) = tokio::sync::mpsc::unbounded_channel();
        session.set_notices(Some(tx));
        let cancel = CancellationToken::new();
        let ctx = c6_tools::ToolCtx { session, cancel: cancel.clone(), output: Arc::new(|_| {}), call_id: "kit-call".into() };
        let wait = tokio::spawn(async move { ctx.session.kit_admission().unwrap().wait(&ctx).await });
        let notice = notices.recv().await.unwrap();
        assert!(notice.message.contains("queued: disk nearly full"));
        assert!(!wait.is_finished());
        cancel.cancel();
        assert!(wait.await.unwrap().unwrap_err().contains("stopped"));
    }

    #[tokio::test(start_paused = true)]
    async fn local_wait_queues_under_memory_and_load_and_starts_when_clear() {
        let stage = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Gate::immediate({
            let stage = stage.clone();
            move || match stage.load(Ordering::Relaxed) {
                0 => Measurement { memory: Some((64 * MIB, 4_096 * MIB)), ..Default::default() },
                1 => Measurement { memory: Some((2_048 * MIB, 4_096 * MIB)), load: Some(2.0), ..Default::default() },
                _ => Measurement { memory: Some((2_048 * MIB, 4_096 * MIB)), load: Some(0.25), ..Default::default() },
            }
        }));
        let (queued, mut reason) = tokio::sync::mpsc::unbounded_channel();
        let waiting = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.wait(&CancellationToken::new(), |pressure| {
                    queued.send(pressure).unwrap();
                    std::future::ready(())
                })
                .await
            }
        });
        assert_eq!(reason.recv().await, Some(Pressure::Memory));
        assert!(!waiting.is_finished());
        stage.store(1, Ordering::Relaxed);
        tokio::time::advance(RETRY).await;
        assert_eq!(gate.try_start(), Err(Pressure::Load));
        assert!(!waiting.is_finished());
        assert!(reason.try_recv().is_err(), "one announcement per wait");
        stage.store(2, Ordering::Relaxed);
        tokio::time::advance(RETRY).await;
        assert!(waiting.await.unwrap().unwrap().redeem());
    }

    #[tokio::test]
    async fn local_wait_can_be_stopped_before_admission() {
        let gate = Gate::immediate(|| Measurement { memory_pressure: true, ..Default::default() });
        let cancel = CancellationToken::new();
        let result = gate
            .wait(&cancel, |_| {
                cancel.cancel();
                std::future::ready(())
            })
            .await;
        assert_eq!(result.err().unwrap(), "subagent was stopped before it started");
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_reserves_memory_only_during_the_start_window() {
        let gate = Gate::immediate(|| Measurement { memory: Some((600 * MIB, 4_096 * MIB)), ..Default::default() });
        let cancel = CancellationToken::new();
        let admission = gate.wait(&cancel, |_| std::future::ready(())).await.unwrap();
        assert_eq!(gate.try_start(), Err(Pressure::Memory));
        tokio::time::advance(START_WINDOW).await;
        assert!(!admission.redeem());
        assert_eq!(gate.try_start(), Ok(()));
    }

    #[tokio::test]
    async fn template_launch_stays_in_inbox_under_load_then_starts_or_can_be_stopped() {
        use crate::{journal::Journal, link::HeadLink, session::SessionManager};
        use c6_proto::{CreateSession, Interrupt, SessionCommand, UserInput, session_command, session_event};

        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let mut manager = SessionManager::new(journal.clone(), Arc::new(HeadLink::default()), dir.path().join("worktrees"));
        let overloaded = Arc::new(std::sync::atomic::AtomicBool::new(true));
        manager.launch_pressure = Arc::new(Gate::immediate({
            let overloaded = overloaded.clone();
            move || Measurement { load: Some(if overloaded.load(Ordering::Relaxed) { 2.0 } else { 0.0 }), ..Default::default() }
        }));
        let uncapped: Vec<_> = (0..65).map(|_| manager.working("uncapped", 0).unwrap()).collect();
        assert_eq!(uncapped.len(), 65);
        let capped = manager.working("capped", 1).unwrap();
        assert!(manager.working("capped", 1).is_none());
        drop(capped);
        assert!(manager.working("capped", 1).is_some());
        let command = |session: &str, id: String, kind| SessionCommand {
            session_id: session.into(),
            command_id: id,
            kind: Some(kind),
            ..Default::default()
        };
        let input = |session, id| command(session, id, session_command::Kind::Input(UserInput { text: "go".into(), ..Default::default() }));

        for child in ["kept", "stopped", "local"] {
            manager
                .handle(command(
                    child,
                    format!("create-{child}"),
                    session_command::Kind::Create(CreateSession {
                        cwd: dir.path().display().to_string(),
                        depth: if child == "local" { 0 } else { 1 },
                        delegation: (child == "local").then(|| c6_proto::Delegation {
                            parent_session_id: "parent".into(),
                            root_session_id: "parent".into(),
                            depth: 1,
                            ..Default::default()
                        }),
                        model: Some(c6_proto::ModelSpec {
                            connection_id: "test".into(),
                            connection_kind: "openai_compat".into(),
                            model: "test".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                ))
                .await
                .unwrap();
        }
        let keep_id = uuid::Uuid::now_v7().to_string();
        let stop_id = uuid::Uuid::now_v7().to_string();
        let local_id = uuid::Uuid::now_v7().to_string();
        manager.handle(input("kept", keep_id.clone())).await.unwrap();
        manager.handle(input("stopped", stop_id.clone())).await.unwrap();
        manager.handle(input("local", local_id.clone())).await.unwrap();
        for (child, id) in [("kept", &keep_id), ("stopped", &stop_id), ("local", &local_id)] {
            c6_testkit::eventually("a visible worker-busy notice", async || {
                journal
                    .events_after(child, 0, u32::MAX)
                    .await
                    .unwrap()
                    .into_iter()
                    .any(|e| {
                        matches!(e.kind,
                            Some(session_event::Kind::Notice(n)) if n.quiet && n.message == Pressure::Load.message()
                        )
                    })
                    .then_some(())
            })
            .await;
            assert!(manager.inbox_holds(child, id));
            assert!(!manager.is_running(child));
        }
        manager.handle(command("stopped", "interrupt".into(), session_command::Kind::Interrupt(Interrupt::default()))).await.unwrap();
        let reason =
            c6_testkit::eventually("the queued launch rejection", async || journal.command_rejection(&stop_id).await.unwrap()).await;
        assert_eq!(manager.handle(input("stopped", stop_id.clone())).await.unwrap_err(), reason);
        overloaded.store(false, Ordering::Relaxed);
        for (child, id) in [("kept", &keep_id), ("local", &local_id)] {
            c6_testkit::eventually("admitted worker input", async || {
                journal
                    .events_after(child, 0, u32::MAX)
                    .await
                    .unwrap()
                    .into_iter()
                    .any(|e| matches!(e.kind, Some(session_event::Kind::User(u)) if u.command_id == *id))
                    .then_some(())
            })
            .await;
        }
        assert!(!journal.events_after("stopped", 0, u32::MAX).await.unwrap().into_iter().any(|e| matches!(e.kind,
            Some(session_event::Kind::User(u)) if u.command_id == stop_id
        )));
    }
}
