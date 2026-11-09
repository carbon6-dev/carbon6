//! M5 chaos: head restarts mid-turn, worker restarts mid-turn, and a worker
//! that lost its journal.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn cfg(dir: &Path, ui: u16, worker: u16) -> HeadConfig {
    HeadConfig {
        data_dir: dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: ui,
        worker_port: worker,
        auth: false,
        ..Default::default()
    }
}

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = crate::open_worker(state).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (w, stop)
}

struct Env {
    head: Option<c6_head::Head>,
    head_dir: tempfile::TempDir,
    api: String,
    ports: (u16, u16),
    http: reqwest::Client,
    mock: MockLlm,
    conn_id: String,
}

impl Env {
    async fn new() -> Self {
        let head_dir = tempfile::tempdir().unwrap();
        // Ports it gets back when it restarts (its worker knows them).
        let head = c6_head::start(cfg(head_dir.path(), c6_testkit::port(), c6_testkit::port())).await.unwrap();
        let ports = (head.ui_addrs[0].port(), head.worker_addrs[0].port());
        let api = format!("http://127.0.0.1:{}/api/v1", ports.0);
        let http = reqwest::Client::new();
        let mock = MockLlm::start().await;
        let conn: Value = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "m", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Self { head: Some(head), head_dir, api, ports, http, mock, conn_id: conn["id"].as_str().unwrap().to_owned() }
    }

    async fn enroll(&self, state: &Path) {
        let e = c6_head::api::new_enrollment(&self.head.as_ref().unwrap().state, None).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
    }

    async fn create(&self, worker: &Worker, cwd: &Path, prompt: &str) -> String {
        let s: Value = self
            .http
            .post(format!("{}/sessions", self.api))
            .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": cwd.display().to_string(), "connection_id": self.conn_id, "model": "m", "prompt": prompt}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        s["id"].as_str().unwrap().to_owned()
    }

    async fn detail(&self, id: &str) -> Option<SessionDetail> {
        self.http.get(format!("{}/sessions/{id}", self.api)).send().await.ok()?.json().await.ok()
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn idle_after(&self, id: &str, min_seq: u64) -> SessionDetail {
        eventually("session idle", async || {
            self.detail(id)
                .await
                .filter(|d| d.session.status == SessionStatus::Idle && d.queued.is_empty() && d.session.last_seq >= min_seq)
        })
        .await
    }
}

#[tokio::test]
async fn head_restarts_during_a_long_turn() {
    let mut env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    const STEPS: usize = 15;
    let mut restarts = Vec::new();
    for i in 0..STEPS {
        let reply = Reply::ToolCalls(vec![("bash".into(), format!(r#"{{"command":"echo step-{i}"}}"#))]);
        if i % 5 == 0 {
            restarts.push((i, env.mock.push_gated(reply)));
        } else {
            env.mock.push(reply);
        }
    }
    env.mock.push(Reply::Text("finished".into()));
    let id = env.create(&worker, project.path(), "do the steps").await;

    for (step, gate) in restarts {
        // Restart at known points in a live turn, not a delay that can
        // expire during setup or after all the steps on a loaded machine.
        c6_testkit::eventually_or("the next gated step", async || (env.mock.requests().len() > step).then_some(()), || env.mock.log())
            .await;
        env.head.take().unwrap().shutdown().await.unwrap();
        gate.notify_one();
        // Keep the offline-progress coverage: this tool result must be
        // journaled with no head, before a new attachment can upload it.
        let result = eventually("the next step to finish offline", async || {
            let (_, events) = worker.journal.load_session(&id).await.unwrap()?;
            let result = events
                .iter()
                .filter_map(|e| match &e.kind {
                    Some(c6_proto::session_event::Kind::ToolResult(r)) => Some(r.clone()),
                    _ => None,
                })
                .nth(step);
            assert!(
                result.is_some() || !events.iter().any(|e| matches!(e.kind, Some(c6_proto::session_event::Kind::TurnEnded(_)))),
                "the turn ended before step {step}: {events:?}"
            );
            result
        })
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, format!("step-{step}\n"));
        env.head = Some(crate::restart_head(cfg(env.head_dir.path(), env.ports.0, env.ports.1)).await);
        eventually("the worker to reattach", async || {
            (env.head.as_ref().unwrap().state.registry.get(worker.identity.meta.worker_id).is_some() && worker.link.is_connected())
                .then_some(())
        })
        .await;
    }
    // Wait for a terminal event, not the expected successful event count:
    // an early error should fail the assertions, not turn into a timeout.
    let ended = eventually("the restarted turn to end", async || {
        env.detail(&id).await.filter(|d| d.session.status == SessionStatus::Idle && d.session.end_reason.is_some())
    })
    .await;
    assert_eq!(ended.session.end_reason, Some(TurnEndReason::Completed), "{ended:?}");
    let (_, journal) = worker.journal.load_session(&id).await.unwrap().unwrap();
    let head_log = env.head.as_ref().unwrap().state.store.events_after(&id, 0, 100_000).await.unwrap();
    assert_eq!(head_log, journal, "head and worker agree byte for byte");
    let outputs: Vec<String> = env
        .events(&id)
        .await
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, (0..STEPS).map(|i| format!("step-{i}\n")).collect::<Vec<_>>());
    stop.cancel();
}

#[tokio::test]
async fn worker_restart_mid_turn_then_continue() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (first, stop_first) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into()));
    let id = env.create(&first, project.path(), "start").await;
    eventually("turn running", async || env.detail(&id).await.filter(|d| d.session.status == SessionStatus::Running)).await;

    // "Crash": the old process stops talking; a new one opens the same state
    // (once the old one has released the state directory lock).
    stop_first.cancel();
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (second, stop_second) = run_worker(state.path()).await;
    env.idle_after(&id, 4).await;
    let events = env.events(&id).await;
    assert!(
        matches!(events.last().unwrap().kind, EventKind::TurnEnded { reason: TurnEndReason::WorkerRestarted, .. }),
        "{:?}",
        events.last()
    );

    // The session keeps working on the restarted worker.
    env.mock.push(Reply::Text("back again".into()));
    env.http.post(format!("{}/sessions/{id}/messages", env.api)).json(&json!({"text": "continue"})).send().await.unwrap();
    let last = events.last().unwrap().seq;
    env.idle_after(&id, last + 4).await;
    let texts: Vec<String> = env
        .events(&id)
        .await
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Assistant { blocks, .. } => Some(format!("{blocks:?}")),
            _ => None,
        })
        .collect();
    assert!(texts.last().unwrap().contains("back again"));
    drop(second);
    stop_second.cancel();
}

#[tokio::test]
async fn lost_journal_is_restored_from_the_head() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    env.mock.push(Reply::Text("first answer".into()));
    let id = env.create(&worker, project.path(), "remember the word pineapple").await;
    env.idle_after(&id, 6).await;
    stop.cancel();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Same identity, empty state: the journal is gone.
    let fresh = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(c6_worker::identity::identity_dir(fresh.path())).unwrap();
    for f in std::fs::read_dir(c6_worker::identity::identity_dir(state.path())).unwrap().flatten() {
        std::fs::copy(f.path(), c6_worker::identity::identity_dir(fresh.path()).join(f.file_name())).unwrap();
    }
    let (_reborn, stop2) = run_worker(fresh.path()).await;

    env.mock.push(Reply::Text("it was pineapple".into()));
    let before = env.mock.requests().len();
    env.http.post(format!("{}/sessions/{id}/messages", env.api)).json(&json!({"text": "what was the word?"})).send().await.unwrap();
    env.idle_after(&id, 10).await;
    let req = &env.mock.requests()[before];
    let msgs = req["messages"].as_array().unwrap();
    let all: String = msgs.iter().map(|m| m["content"].to_string()).collect();
    assert!(all.contains("remember the word pineapple") && all.contains("first answer"), "history restored: {all}");
    stop2.cancel();
}

/// A background command outlives a worker restart: the restarted worker
/// takes it up, follows its output, and its end (exit code and all of its
/// output) reaches the agent as usual (docs/restarts.md §3).
#[tokio::test]
async fn a_background_command_outlives_a_worker_restart() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (first, stop_first) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let gate = project.path().join("gate");
    // Its output ("early-line", "late-line") differs from its text.
    let command =
        format!("printf 'early-%s\\n' line; while [ ! -f {} ]; do sleep 0.05; done; printf 'late-%s\\n' line; exit 3", gate.display());
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), json!({"command": command, "run_in_background": true}).to_string())]));
    env.mock.push(Reply::Text("started it".into()));
    let id = env.create(&first, project.path(), "run the long job").await;
    let detail = env.idle_after(&id, 4).await;
    let started = eventually("the command's first output", async || {
        let held = state.path().join("held").join(&id);
        let task = std::fs::read_dir(&held).ok()?.flatten().next()?.path();
        std::fs::read_to_string(task.join("out")).ok().filter(|o| o.contains("early-line")).map(|_| task)
    })
    .await;

    // The worker goes; the command doesn't.
    stop_first.cancel();
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let meta: Value = serde_json::from_slice(&std::fs::read(started.join("meta.json")).unwrap()).unwrap();
    let pgid = nix::unistd::Pid::from_raw(meta["pgid"].as_i64().unwrap() as i32);
    assert!(nix::sys::signal::kill(pgid, None).is_ok(), "still running without its worker");
    env.mock.push(Reply::Text("the job finished".into()));
    let (second, stop_second) = run_worker(state.path()).await;

    // It finishes under the new worker, and its end wakes the agent.
    std::fs::write(&gate, "").unwrap();
    let after = detail.session.last_seq;
    let code = eventually("the command's end, reported", async || {
        env.events(&id).await.into_iter().find_map(|e| match e.kind {
            EventKind::BackgroundTask { state, exit_code, .. } if e.seq > after && state == "finished" => Some(exit_code),
            _ => None,
        })
    })
    .await;
    assert_eq!(code, 3, "its exit code");
    // The agent is told, with all of its output, from both workers' time.
    eventually("the agent told of its end", async || {
        env.mock
            .requests()
            .iter()
            .any(|r| {
                let m = r["messages"].to_string();
                m.contains("early-line") && m.contains("late-line")
            })
            .then_some(())
    })
    .await;
    assert!(
        !env.events(&id).await.iter().any(|e| matches!(&e.kind, EventKind::BackgroundTask { state, .. } if state == "lost")),
        "not lost"
    );
    eventually("its files cleared", async || (!started.exists()).then_some(())).await;
    drop(second);
    stop_second.cancel();
}
