//! Best-effort admission from the worker's current local resources. This
//! cannot predict the resources a model, tool, or an SSH host will need later.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;
const START_WINDOW: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Default)]
pub struct Measurement {
    // Available bytes and capacity, for the most constrained observed domain.
    pub memory: Option<(u64, u64)>,
    pub memory_pressure: bool,
    // One-minute system load divided by the system's online CPUs.
    pub load: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pressure {
    Memory,
    Load,
}

impl Pressure {
    pub(crate) fn message(self) -> &'static str {
        match self {
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
    sample: Option<(Instant, Measurement)>,
    starts: VecDeque<Instant>,
}

pub(crate) struct Gate {
    state: Mutex<State>,
    measure: Box<dyn Fn() -> Measurement + Send + Sync>,
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
        Self::new(system)
    }
}

impl Gate {
    pub(crate) fn new(measure: impl Fn() -> Measurement + Send + Sync + 'static) -> Self {
        Self { state: Mutex::default(), measure: Box::new(measure) }
    }

    /// Budget briefly against starts that may not be visible in the sample
    /// yet. Never retain a reservation for a whole (possibly nested) turn.
    pub(crate) fn try_start(&self) -> Result<(), Pressure> {
        let now = Instant::now();
        let mut s = self.state.lock().unwrap();
        let m = match s.sample {
            Some((at, m)) if now.saturating_duration_since(at) < RETRY => m,
            _ => {
                let m = (self.measure)();
                s.sample = Some((now, m));
                m
            }
        };
        while s.starts.front().is_some_and(|at| now.saturating_duration_since(*at) >= START_WINDOW) {
            s.starts.pop_front();
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

    #[tokio::test(start_paused = true)]
    async fn local_wait_queues_under_memory_and_load_and_starts_when_clear() {
        let stage = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Gate::new({
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
        let gate = Gate::new(|| Measurement { memory_pressure: true, ..Default::default() });
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
        let gate = Gate::new(|| Measurement { memory: Some((600 * MIB, 4_096 * MIB)), ..Default::default() });
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
        manager.launch_pressure = Arc::new(Gate::new({
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
