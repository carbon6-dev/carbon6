//! Subagents as sessions (docs/subagents.md): the parent's log links to the
//! child, the child runs as its own actor, and an interrupt or a worker
//! restart leaves what it did in its log.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c6_api::{Event, EventKind, Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    conn_id: String,
    _head_dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
        let head_dir = tempfile::tempdir().unwrap();
        let cfg = HeadConfig {
            data_dir: head_dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        };
        let head = c6_head::start(cfg).await.unwrap();
        let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
        let http = reqwest::Client::new();
        let mock = MockLlm::start().await;
        let conn: Value =
            http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()})).send().await.unwrap().json().await.unwrap();
        Self { head, api, http, mock, conn_id: conn["id"].as_str().unwrap().to_owned(), _head_dir: head_dir }
    }

    async fn enroll(&self, state: &Path) {
        let e = c6_head::api::new_enrollment(&self.head.state, None).await.unwrap();
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

    async fn children(&self, id: &str) -> Vec<Session> {
        self.http.get(format!("{}/sessions/{id}/children", self.api)).send().await.unwrap().json().await.unwrap()
    }
}

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = Worker::open(state, Identity::load(state).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(15), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (w, stop)
}

async fn until<T>(what: &str, secs: u64, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn turn_end(events: &[Event]) -> Option<TurnEndReason> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::TurnEnded { reason, .. } => Some(*reason),
        _ => None,
    })
}

/// The parent's log names the child, the child's first message is the
/// task (the subagent note goes to the model only), and stopping the parent
/// stops the child, whose finished tool calls stay in its log.
#[tokio::test]
async fn a_subagent_is_a_session_and_stops_with_its_parent() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"dig","prompt":"Look for the needle."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo found-half"}"#.into())])); // the subagent
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into())); // the subagent, stuck
    let id = env.create(&worker, project.path(), "go").await;

    // The parent's log links to the child while it runs.
    let child = until("subagent spawned", 15, async || {
        env.events(&id).await.into_iter().find_map(|e| match e.kind {
            EventKind::SubagentSpawned { call_id, child_session_id, description, .. } => Some((call_id, child_session_id, description)),
            _ => None,
        })
    })
    .await;
    assert_eq!(child.2, "dig");
    let child_id = child.1;
    until("the subagent's command ran", 15, async || {
        env.events(&child_id).await.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("found-half"))).then_some(())
    })
    .await;
    let children = env.children(&id).await;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, child_id);
    assert_eq!(children[0].parent_call_id.as_deref(), Some(child.0.as_str()));

    let events = env.events(&child_id).await;
    let (text, prompt, delegated_by) = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::User { text, prompt, delegated_by, .. } => Some((text.clone(), prompt.clone(), delegated_by.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(text, "Look for the needle.");
    assert!(prompt.starts_with("Look for the needle.") && prompt.contains("subagent"), "{prompt}");
    assert_eq!(delegated_by, child.0);
    // Subagents are not rewound: no checkpoints of their own.
    assert!(!events.iter().any(|e| matches!(e.kind, EventKind::Checkpoint { .. })));

    // Stopping the parent stops the child; both turns end as interrupted.
    let r = env.http.post(format!("{}/sessions/{id}/interrupt", env.api)).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let child_events = until("the subagent stopped", 15, async || {
        let ev = env.events(&child_id).await;
        (turn_end(&ev) == Some(TurnEndReason::Interrupted)).then_some(ev)
    })
    .await;
    assert!(child_events.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("found-half"))));
    let parent = until("the parent stopped", 15, async || {
        let ev = env.events(&id).await;
        (turn_end(&ev) == Some(TurnEndReason::Interrupted)).then_some(ev)
    })
    .await;
    let task_result = parent
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, is_error, .. } if tool == "task" => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap();
    assert!(task_result.1, "{task_result:?}");
    let d = env.detail(&child_id).await.unwrap();
    assert_eq!(d.session.status, SessionStatus::Idle);
    stop.cancel();
}

/// A worker that restarts while a subagent runs closes both turns; the
/// child's log, as far as it got, is on the head like the worker's.
#[tokio::test]
async fn a_worker_restart_keeps_the_subagents_log() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (first, stop_first) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"dig","prompt":"Dig."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo partial"}"#.into())]));
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into()));
    let id = env.create(&first, project.path(), "go").await;
    let child_id = until("subagent working", 15, async || {
        let c = env.children(&id).await.into_iter().next()?;
        let ev = env.events(&c.id).await;
        ev.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("partial"))).then_some(c.id)
    })
    .await;

    stop_first.cancel();
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (second, stop_second) = run_worker(state.path()).await;
    for sid in [&id, &child_id] {
        let ev = until("turn closed", 15, async || {
            let ev = env.events(sid).await;
            (turn_end(&ev) == Some(TurnEndReason::WorkerRestarted)).then_some(ev)
        })
        .await;
        let (_, journal) = second.journal.load_session(sid).await.unwrap().unwrap();
        assert_eq!(ev.len(), journal.len(), "head and worker agree on {sid}");
    }
    let ev = env.events(&child_id).await;
    assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("partial"))));
    stop_second.cancel();
}
