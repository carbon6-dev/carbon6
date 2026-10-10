//! Side chats: branch from a point in a session with its context, continue,
//! nest, and survive the worker losing its journal.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = Worker::open(state, Identity::load(state).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (w, stop)
}

struct Env {
    api: String,
    http: reqwest::Client,
}

impl Env {
    async fn post<T: serde::de::DeserializeOwned>(&self, path: &str, body: Value) -> T {
        let r = self.http.post(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.text().await.unwrap());
        r.json().await.unwrap()
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.http.get(format!("{}{path}", self.api)).send().await.unwrap().json().await.unwrap()
    }

    /// Waits until the session is idle with `turns` finished turns.
    async fn idle(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("turn to finish", async || {
            let d: SessionDetail = self.get(&format!("/sessions/{id}")).await;
            if d.session.status != SessionStatus::Idle || !d.queued.is_empty() {
                return None;
            }
            let evs: Vec<Event> = self.get(&format!("/sessions/{id}/events")).await;
            (evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns).then_some(evs)
        })
        .await
    }
}

fn first_assistant(evs: &[Event]) -> u64 {
    evs.iter().find(|e| matches!(e.kind, EventKind::Assistant { .. })).unwrap().seq
}

/// The conversation text of the last model request.
fn last_request(mock: &MockLlm) -> String {
    mock.requests().last().unwrap()["messages"].to_string()
}

#[tokio::test]
async fn side_chats_branch_continue_nest_and_restore() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let env = Env { api: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()), http: reqwest::Client::new() };
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let (worker, stop) = run_worker(state.path()).await;
    let conn: Connection = env.post("/connections", json!({"name": "m", "base_url": mock.base_url()})).await;

    // A main session with two turns.
    mock.push(Reply::Text("main answer one".into()));
    let main: Session = env
        .post(
            "/sessions",
            json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
                   "connection_id": conn.id, "model": "main-model", "prompt": "first question"}),
        )
        .await;
    let evs = env.idle(&main.id, 1).await;
    let anchor = first_assistant(&evs);
    mock.push(Reply::Text("main answer two".into()));
    let _: Value = env.post(&format!("/sessions/{}/messages", main.id), json!({"text": "second question"})).await;
    env.idle(&main.id, 2).await;

    // A side chat on the first answer, with another model: it sees the
    // conversation up to there and nothing after.
    mock.push(Reply::Text("side answer".into()));
    let side: Session = env
        .post(
            &format!("/sessions/{}/side-chats", main.id),
            json!({"anchor_seq": anchor, "connection_id": conn.id, "model": "side-model", "prompt": "why?"}),
        )
        .await;
    assert_eq!((side.parent_id.as_deref(), side.fork_seq), (Some(main.id.as_str()), Some(anchor)));
    let side_evs = env.idle(&side.id, 1).await;
    let req = &mock.requests().last().unwrap().clone();
    assert_eq!(req["model"], "side-model");
    let msgs = req["messages"].to_string();
    for s in ["first question", "main answer one", "why?"] {
        assert!(msgs.contains(s), "{s} missing from {msgs}");
    }
    assert!(!msgs.contains("second question") && !msgs.contains("main answer two"), "{msgs}");
    assert!(msgs.contains("This is a side chat"), "side-chat note in the system prompt");

    // It takes follow-ups.
    mock.push(Reply::Text("side answer two".into()));
    let _: Value = env.post(&format!("/sessions/{}/messages", side.id), json!({"text": "and then?"})).await;
    env.idle(&side.id, 2).await;
    let msgs = last_request(&mock);
    assert!(msgs.contains("first question") && msgs.contains("side answer") && msgs.contains("and then?"), "{msgs}");

    // Nested: a side chat of the side chat.
    mock.push(Reply::Text("nested answer".into()));
    let nested: Session = env
        .post(
            &format!("/sessions/{}/side-chats", side.id),
            json!({"anchor_seq": first_assistant(&side_evs), "connection_id": conn.id, "model": "side-model", "prompt": "deeper?"}),
        )
        .await;
    env.idle(&nested.id, 1).await;
    let msgs = last_request(&mock);
    for s in ["first question", "main answer one", "why?", "side answer", "deeper?"] {
        assert!(msgs.contains(s), "{s} missing from {msgs}");
    }
    assert!(!msgs.contains("and then?"), "branched before the follow-up: {msgs}");

    // Switch the side chat's model mid-conversation: recorded in the log,
    // used from that message on, and kept for later messages.
    mock.push(Reply::Text("switched answer".into()));
    let _: Value = env
        .post(
            &format!("/sessions/{}/messages", side.id),
            json!({"text": "now on another model", "connection_id": conn.id, "model": "other-model"}),
        )
        .await;
    let evs = env.idle(&side.id, 3).await;
    let req = mock.requests().last().unwrap().clone();
    assert_eq!(req["model"], "other-model");
    assert!(req["messages"].to_string().contains("side answer two"), "history carries over");
    let switched = evs.iter().position(|e| matches!(&e.kind, EventKind::ModelSwitched { model, .. } if model == "other-model")).unwrap();
    assert!(matches!(&evs[switched + 1].kind, EventKind::User { text, .. } if text == "now on another model"), "{:?}", evs[switched + 1]);
    let d: SessionDetail = env.get(&format!("/sessions/{}", side.id)).await;
    assert_eq!(d.session.model, "other-model");
    mock.push(Reply::Text("still switched".into()));
    let _: Value = env.post(&format!("/sessions/{}/messages", side.id), json!({"text": "and next?"})).await;
    env.idle(&side.id, 4).await;
    assert_eq!(mock.requests().last().unwrap()["model"], "other-model");

    // An effort-only switch keeps the model.
    mock.push(Reply::Text("thought harder".into()));
    let _: Value = env
        .post(
            &format!("/sessions/{}/messages", side.id),
            json!({"text": "think harder", "connection_id": conn.id, "model": "other-model", "effort": "high"}),
        )
        .await;
    let evs = env.idle(&side.id, 5).await;
    // (Chat Completions sends effort only to OpenAI itself, so check the log.)
    assert_eq!(mock.requests().last().unwrap()["model"], "other-model");
    assert!(evs.iter().any(|e| matches!(&e.kind, EventKind::ModelSwitched { effort, .. } if effort == "high")));
    let d: SessionDetail = env.get(&format!("/sessions/{}", side.id)).await;
    assert_eq!(d.session.effort.as_deref(), Some("high"));

    // Side chats are children, not top-level sessions.
    let children: Vec<Session> = env.get(&format!("/sessions/{}/children", main.id)).await;
    assert_eq!(children.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), [side.id.as_str()]);
    let top: Vec<Session> = env.get("/sessions").await;
    assert!(top.iter().all(|s| s.parent_id.is_none()));

    // The worker loses its journal: the side chat is restored with its
    // branched-from context.
    stop.cancel();
    drop(worker);
    tokio::time::sleep(Duration::from_millis(200)).await;
    for f in ["journal.db", "journal.db-wal", "journal.db-shm"] {
        let _ = std::fs::remove_file(state.path().join(f));
    }
    let (_worker, stop) = run_worker(state.path()).await;
    mock.push(Reply::Text("after restore".into()));
    let _: Value = env.post(&format!("/sessions/{}/messages", side.id), json!({"text": "still there?"})).await;
    env.idle(&side.id, 6).await;
    let msgs = last_request(&mock);
    for s in ["first question", "main answer one", "why?", "side answer two", "switched answer", "still there?"] {
        assert!(msgs.contains(s), "{s} missing after restore: {msgs}");
    }
    assert_eq!(mock.requests().last().unwrap()["model"], "other-model", "the switch survives a restore");
    stop.cancel();
    head.shutdown().await.unwrap();
}
