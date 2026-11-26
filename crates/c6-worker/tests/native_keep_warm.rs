//! Exercise the native actor's maintenance loop with real provider requests.
//! A FIFO, rather than a sleep, keeps the shell task alive; only Tokio's
//! injected clock advances the cache deadlines.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use c6_proto::{
    CreateSession, CredentialGrant, ModelSpec, SessionCommand, SessionEvent, SessionStarted, UserInput, session_command, session_event,
};
use c6_testkit::{MockLlm, Reply};
use c6_worker::{
    journal::Journal,
    link::{HeadLink, LocalHead},
    session::SessionManager,
};
use serde_json::{Value, json};

const SID: &str = "warm";
const INTERVAL: Duration = Duration::from_secs(55 * 60);

struct Head {
    url: String,
    lookups: AtomicUsize,
}

#[async_trait]
impl LocalHead for Head {
    async fn call(&self, method: &str, _: Value) -> Result<Value, String> {
        if method == "session.cache_lifetime" {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"lifetime_seconds":3600,"cache_observed":true,"read_to_rewrite_ratio":0.001}))
        } else {
            Err(format!("not needed: {method}"))
        }
    }

    async fn credential(&self, connection_id: &str) -> CredentialGrant {
        CredentialGrant {
            connection_id: connection_id.into(),
            kind: "openai_compat".into(),
            base_url: self.url.clone(),
            // This fixture tests maintenance, not endpoint discovery. An
            // explicit window also keeps independent paused runtimes out of
            // the process-global compatible-endpoint discovery client/cache.
            context_window: 1_000_000,
            ..Default::default()
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    journal: Arc<Journal>,
    manager: SessionManager,
    mock: MockLlm,
    head: Arc<Head>,
    _provider: Option<Provider>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A held shell intentionally survives runtime shutdown. Clean it up
        // even when a failed assertion prevents releasing the FIFO normally.
        if let Ok(tasks) = std::fs::read_dir(self.dir.path().join("held").join(SID)) {
            for task in tasks.flatten() {
                let metadata =
                    std::fs::read(task.path().join("meta.json")).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                if let Some(pgid) = metadata.and_then(|value| value["pgid"].as_i64()).filter(|pgid| *pgid > 0) {
                    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid as i32), nix::sys::signal::Signal::SIGTERM);
                }
            }
        }
    }
}

// MockLlm deliberately spaces SSE frames using Tokio timers. Give it its own
// clock, otherwise pausing the worker would pause the provider's response too.
struct Provider {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Provider {
    fn start() -> (MockLlm, Self) {
        let (ready, receive) = std::sync::mpsc::sync_channel(1);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                ready.send(MockLlm::start().await).unwrap();
                let _ = stopped.await;
            });
        });
        (receive.recv().unwrap(), Self { stop: Some(stop), thread: Some(thread) })
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Fixture {
    async fn new(threshold: u64, cap: u32) -> Self {
        let (mock, provider) = Provider::start();
        let mut fixture = Self::with_mock(mock, threshold, cap).await;
        fixture._provider = Some(provider);
        fixture
    }

    async fn with_mock(mock: MockLlm, threshold: u64, cap: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let model =
            ModelSpec { connection_id: "mock".into(), connection_kind: "openai_compat".into(), model: "m".into(), ..Default::default() };
        let settings = c6_api::KeepWarmSettings { enabled: true, threshold_tokens: threshold, cap_seconds: cap, ..Default::default() };
        let spec = CreateSession {
            model: Some(model.clone()),
            cwd: dir.path().display().to_string(),
            keep_warm_json: Some(serde_json::to_string(&settings).unwrap()),
            ..Default::default()
        };
        journal
            .import(
                SID,
                &spec,
                &[SessionEvent {
                    seq: 1,
                    ts_ms: c6_core::now_ms(),
                    kind: Some(session_event::Kind::Started(SessionStarted {
                        model: Some(model),
                        cwd: spec.cwd.clone(),
                        tool_profile: "generic".into(),
                        ..Default::default()
                    })),
                }],
            )
            .await
            .unwrap();
        let head = Arc::new(Head { url: mock.base_url(), lookups: AtomicUsize::new(0) });
        let manager = SessionManager::new_with_resource_sampler(
            journal.clone(),
            Arc::new(HeadLink::local(head.clone())),
            dir.path().join("worktrees"),
            c6_worker::ResourceMeasurement::default,
        );
        Self { dir, journal, manager, mock, head, _provider: None }
    }

    async fn input(&self) {
        self.input_text("start", "Start the work.").await;
    }

    async fn input_text(&self, command_id: &str, text: &str) {
        let turns = self.events().await.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::TurnStarted(_)))).count();
        self.manager
            .handle(SessionCommand {
                session_id: SID.into(),
                command_id: command_id.into(),
                kind: Some(session_command::Kind::Input(UserInput { text: text.into(), ..Default::default() })),
                ..Default::default()
            })
            .await
            .unwrap();
        self.wait_for_turn_start(turns).await;
    }

    async fn events(&self) -> Vec<SessionEvent> {
        self.journal.events_after(SID, 0, u32::MAX).await.unwrap()
    }

    async fn wait(&self, label: &str, predicate: impl Fn(&[SessionEvent]) -> bool) {
        let deadline = Instant::now() + c6_testkit::wait::deadline();
        loop {
            if predicate(&self.events().await) {
                return;
            }
            assert!(Instant::now() < deadline, "{label}; model requests: {}", self.mock.requests().len());
            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_turn_start(&self, previous: usize) {
        let deadline = Instant::now() + c6_testkit::wait::deadline();
        loop {
            if self.events().await.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::TurnStarted(_)))).count() > previous {
                return;
            }
            assert!(Instant::now() < deadline, "foreground admission; model requests: {}", self.mock.requests().len());
            // Even a constant injected resource sampler runs on a blocking
            // thread. Its first probe can return Sampling, which requires a
            // retry timer. Drive that timer only until a turn starts, not
            // while waiting for a provider or a maintenance deadline.
            tokio::time::advance(Duration::from_millis(25)).await;
            tokio::task::yield_now().await;
        }
    }

    async fn start_background(&self) {
        let fifo = self.dir.path().join("release");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        self.mock.push(Reply::ToolCalls(vec![(
            "bash".into(),
            json!({"command": format!("cat '{}'", fifo.display()), "run_in_background":true}).to_string(),
        )]));
        self.mock.push(Reply::Text("The work is running.".into()));
        self.input().await;
        self.wait("the foreground turn to end", |events| events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(_)))))
            .await;
    }

    async fn finish_background(&self) {
        let turns = self.events().await.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(_)))).count();
        self.mock.push(Reply::Text("The work finished.".into()));
        let fifo = self.dir.path().join("release");
        tokio::task::spawn_blocking(move || std::fs::write(fifo, "done\n")).await.unwrap().unwrap();
        let deadline = Instant::now() + c6_testkit::wait::deadline();
        loop {
            if self.events().await.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::BackgroundTask(t)) if t.state == "finished"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "the released background task did not finish");
            // The held-shell monitor also uses Tokio timers, including after
            // adoption. Drive those timers, not a real-time sleep.
            tokio::time::advance(Duration::from_millis(25)).await;
            tokio::task::yield_now().await;
        }
        self.settle().await;
        tokio::time::advance(Duration::from_secs(u64::from(c6_api::DEFAULT_NOTICE_MAX_WAIT_SECONDS) + 1)).await;
        self.wait_for_turn_start(turns).await;
        self.wait("completion notice turn", |events| {
            events.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(_)))).count() == turns + 1
        })
        .await;
    }

    async fn armed(&self, count: usize) {
        let deadline = Instant::now() + c6_testkit::wait::deadline();
        while self.head.lookups.load(Ordering::SeqCst) < count {
            assert!(Instant::now() < deadline, "cache timer was not armed");
            tokio::task::yield_now().await;
        }
        self.settle().await;
    }

    async fn settle(&self) {
        // Keep the scheduler runnable so paused time cannot auto-advance over
        // the deadline while SQLite and the mock server perform real I/O.
        for _ in 0..100 {
            let _ = self.events().await;
            tokio::task::yield_now().await;
        }
    }
}

fn pause_clock() -> tokio_util::task::AbortOnDropHandle<()> {
    tokio::time::pause();
    tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    }))
}

fn warm_count(events: &[SessionEvent]) -> usize {
    events.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::Usage(u)) if u.keep_warm)).count()
}

#[tokio::test]
async fn native_keep_warm_repeats_with_identical_prefix_until_work_finishes() {
    let f = Fixture::new(1, 43_200).await;
    let _clock = pause_clock();
    f.start_background().await;
    f.armed(1).await;
    let original = f.mock.requests()[1].clone();
    let baseline = f.events().await;
    tokio::time::advance(INTERVAL - Duration::from_secs(1)).await;
    f.settle().await;
    assert_eq!(f.mock.requests().len(), 2, "maintenance must not run before its deadline");
    for n in 1..=3 {
        f.mock.push(Reply::Text(".".into()));
        let step = if n == 1 { Duration::from_secs(1) } else { INTERVAL };
        tokio::time::advance(step + Duration::from_millis(1)).await;
        f.wait("keep-warm accounting", |events| warm_count(events) == n).await;
        f.armed(n + 1).await;
        let warm = f.mock.requests()[n + 1].clone();
        assert_eq!(warm["max_tokens"], 1);
        let prefix = original["messages"].as_array().unwrap();
        let messages = warm["messages"].as_array().unwrap();
        assert_eq!(
            serde_json::to_vec(&messages[..prefix.len()]).unwrap(),
            serde_json::to_vec(prefix).unwrap(),
            "cached prefix bytes must not change"
        );
        assert_eq!(messages.len(), prefix.len() + 2, "maintenance must not accumulate turns");
        assert_eq!(warm["tools"], original["tools"]);
        assert_eq!(warm["model"], original["model"]);
        assert!(messages[prefix.len()].to_string().contains("The work is running."));
    }
    let events = f.events().await;
    for kind in ["user", "turn"] {
        let count = |events: &[SessionEvent]| {
            events
                .iter()
                .filter(|e| {
                    matches!(
                        (&e.kind, kind),
                        (Some(session_event::Kind::User(_)), "user") | (Some(session_event::Kind::TurnStarted(_)), "turn")
                    )
                })
                .count()
        };
        assert_eq!(count(&events), count(&baseline), "maintenance must not start a {kind}");
    }
    f.finish_background().await;
    tokio::time::advance(2 * INTERVAL).await;
    f.settle().await;
    assert_eq!(f.mock.requests().len(), 6, "completed work must cancel maintenance (only its completion notice gets a turn)");
}

#[tokio::test]
async fn native_keep_warm_never_runs_while_waiting_for_the_user() {
    let f = Fixture::new(1, 43_200).await;
    let _clock = pause_clock();
    f.mock.push(Reply::Text("Your turn.".into()));
    f.input().await;
    f.wait("turn completion", |events| events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(_))))).await;
    tokio::time::advance(4 * INTERVAL).await;
    f.settle().await;
    assert_eq!(f.mock.requests().len(), 1);
    assert_eq!(warm_count(&f.events().await), 0);
}

#[tokio::test]
async fn native_keep_warm_skips_a_deadline_resumed_after_cache_expiry() {
    let f = Fixture::new(1, 43_200).await;
    let _clock = pause_clock();
    f.start_background().await;
    f.armed(1).await;

    // Resume the actor once, after both its 55-minute deadline and its
    // provider's entire hour-long cache lifetime have elapsed. Dispatching the
    // already-ready timer must not turn maintenance into a cold rewrite.
    tokio::time::advance(Duration::from_secs(3_601)).await;
    f.settle().await;
    assert_eq!(f.mock.requests().len(), 2, "a delayed timer must never send an expired prefix");
    let events = f.events().await;
    assert_eq!(warm_count(&events), 0);
    assert!(!events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Notice(n)) if n.title == "Cache maintenance attempted")));
    f.finish_background().await;
}

#[tokio::test]
async fn native_keep_warm_cancels_an_inflight_ping_when_real_input_arrives() {
    let f = Fixture::new(1, 43_200).await;
    let _clock = pause_clock();
    f.start_background().await;
    f.armed(1).await;
    let release = f.mock.push_gated(Reply::Text(".".into()));
    tokio::time::advance(INTERVAL + Duration::from_millis(1)).await;
    f.wait("the provider to accept the maintenance request", |_| f.mock.requests().len() == 3).await;

    // The provider has accepted the maintenance request, but its response is
    // gated. A real input must proceed without waiting for that response.
    f.mock.push(Reply::Text("I received your update.".into()));
    f.input_text("update", "Here is an update while the work runs.").await;
    f.wait("the real input to finish while maintenance is held", |events| {
        events.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(_)))).count() == 2
    })
    .await;
    assert_eq!(f.mock.requests().len(), 4);
    release.notify_one();
    f.settle().await;
    assert_eq!(warm_count(&f.events().await), 0, "a canceled response must not later invent completed maintenance");
    f.finish_background().await;
}

#[tokio::test]
async fn native_keep_warm_rebuilds_request_from_reopened_journal() {
    let f = Fixture::new(1, 43_200).await;
    let _clock = pause_clock();
    f.start_background().await;
    f.armed(1).await;
    let original = f.mock.requests()[1].clone();

    // Re-open the database and construct a new link: no in-memory conversation
    // or provider client is available to reconstruct this maintenance request.
    let reopened = Journal::open(&f.dir.path().join("journal.db")).await.unwrap();
    let snapshot = reopened.load_keep_warm_snapshot(SID).await.unwrap().unwrap();
    let link = HeadLink::local(f.head.clone());
    f.mock.push(Reply::Text(".".into()));
    let usage = link.keep_warm(SID, &snapshot, c6_agent::KEEP_WARM_NOTE).await.unwrap();
    assert!(usage.keep_warm);
    assert_eq!(usage.resolved_connection_id, "mock");
    let request = f.mock.requests()[2].clone();
    let prefix = original["messages"].as_array().unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(serde_json::to_vec(&messages[..prefix.len()]).unwrap(), serde_json::to_vec(prefix).unwrap());
    assert_eq!(messages.len(), prefix.len() + 2);
    assert_eq!(request["tools"], original["tools"]);
    assert_eq!(request["max_tokens"], 1);
    assert_eq!(warm_count(&f.events().await), 0, "the direct replay must not invent journal events");
    f.finish_background().await;
}

#[test]
fn native_keep_warm_rebuilds_actor_timer_after_runtime_restart() {
    // Keep the provider outside the worker's runtime so destroying the latter
    // models a restart without changing the recorded provider configuration.
    let provider = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mock = provider.block_on(MockLlm::start());
    let runtime = || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let first = runtime();
    let mut f = first.block_on(async {
        let f = Fixture::with_mock(mock, 1, 43_200).await;
        let _clock = pause_clock();
        f.start_background().await;
        f.armed(1).await;
        f.mock.push(Reply::Text(".".into()));
        tokio::time::advance(INTERVAL + Duration::from_millis(1)).await;
        f.wait("pre-restart ping", |events| warm_count(events) == 1).await;
        f.armed(2).await;
        f
    });
    drop(first);

    let second = runtime();
    second.block_on(async {
        let _clock = pause_clock();
        f.journal = Arc::new(Journal::open(&f.dir.path().join("journal.db")).await.unwrap());
        f.manager = SessionManager::new_with_resource_sampler(
            f.journal.clone(),
            Arc::new(HeadLink::local(f.head.clone())),
            f.dir.path().join("worktrees"),
            c6_worker::ResourceMeasurement::default,
        );
        f.manager.recover().await.unwrap();
        f.armed(3).await;
        assert_eq!(f.mock.requests().len(), 3, "restart must not immediately repeat the already-accounted ping");
        tokio::time::advance(INTERVAL - Duration::from_secs(1)).await;
        f.settle().await;
        assert_eq!(f.mock.requests().len(), 3, "the rebuilt timer must retain the last ping's deadline");
        f.mock.push(Reply::Text(".".into()));
        tokio::time::advance(Duration::from_secs(1) + Duration::from_millis(1)).await;
        f.wait("post-restart ping", |events| warm_count(events) == 2).await;
        let requests = f.mock.requests();
        assert_eq!(requests[2], requests[3], "restart must rebuild the identical maintenance request");
        f.finish_background().await;
    });
}

#[tokio::test]
async fn native_keep_warm_respects_context_threshold_and_wait_cap() {
    for (threshold, cap, expected) in [(50_000, 43_200, 0), (1, 3_400, 1)] {
        let f = Fixture::new(threshold, cap).await;
        let _clock = pause_clock();
        f.start_background().await;
        if expected == 1 {
            f.armed(1).await;
            f.mock.push(Reply::Text(".".into()));
            tokio::time::advance(INTERVAL + Duration::from_millis(1)).await;
            f.wait("first maintenance request", |events| warm_count(events) == 1).await;
            f.armed(2).await;
        }
        tokio::time::advance(4 * INTERVAL).await;
        f.settle().await;
        assert_eq!(warm_count(&f.events().await), expected);
        assert_eq!(f.mock.requests().len(), expected + 2);
        f.finish_background().await;
        tokio::time::resume();
    }
}
