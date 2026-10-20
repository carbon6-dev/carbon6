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

    async fn restart_head(&mut self) {
        self.head.take().unwrap().shutdown().await.unwrap();
        self.head = Some(crate::restart_head(cfg(self.head_dir.path(), self.ports.0, self.ports.1)).await);
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
    for i in 0..STEPS {
        env.mock.push(Reply::ToolCalls(vec![("bash".into(), format!(r#"{{"command":"sleep 0.1; echo step-{i}"}}"#))]));
    }
    env.mock.push(Reply::Text("finished".into()));
    let id = env.create(&worker, project.path(), "do the steps").await;

    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        env.restart_head().await;
    }
    // Started + user + turn_started + STEPS×(assistant, usage, tool_result) + assistant + usage + turn_ended
    let min = (3 + STEPS * 3 + 3) as u64;
    env.idle_after(&id, min).await;
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
