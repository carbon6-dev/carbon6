//! Side chats: branch from a point in a session with its context, continue,
//! nest, and survive the worker losing its journal.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = crate::open_worker(state).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
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

/// Exercise the head's API with controlled stored activity, independently of
/// model timing: operational status must never be rewritten for display.
#[tokio::test]
async fn side_chat_activity_counts_are_realm_safe_and_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let env = Env { api: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()), http: reqwest::Client::new() };
    let pool = head.state.store.pool();
    for (id, parent, root, realm, status, fork, call) in [
        ("main", None, None, "home", "idle", None, None),
        ("other", None, None, "home", "idle", None, None),
        ("realm-root", None, None, "root", "idle", None, None),
        ("side", Some("main"), Some("main"), "home", "running", Some(1), None),
        ("nested", Some("side"), Some("main"), "home", "running", Some(2), None),
        ("sibling", Some("main"), Some("main"), "home", "running", Some(1), None),
        ("starting", Some("main"), Some("main"), "home", "starting", Some(1), None),
        ("failed", Some("main"), Some("main"), "home", "failed", Some(1), None),
        ("idle", Some("main"), Some("main"), "home", "idle", Some(1), None),
        ("agent", Some("side"), Some("main"), "home", "running", None, Some("call")),
        ("realm-side", Some("realm-root"), Some("realm-root"), "root", "running", Some(1), None),
        // Even malformed cross-realm references cannot affect home counts.
        ("cross-side", Some("main"), Some("main"), "root", "running", Some(1), None),
        ("cross-agent", Some("main"), Some("main"), "root", "running", None, Some("call")),
    ] {
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, \
             parent_id, root_id, realm, fork_seq, parent_call_id) VALUES (?, ?, ?, '/tmp', 'c', 'm', ?, 0, 0, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("00000000-0000-0000-0000-000000000000")
        .bind(id)
        .bind(status)
        .bind(parent)
        .bind(root)
        .bind(realm)
        .bind(fork)
        .bind(call)
        .execute(pool)
        .await
        .unwrap();
    }
    let list: Vec<Session> = env.get("/sessions").await;
    let main = list.iter().find(|s| s.id == "main").unwrap();
    assert_eq!(main.status, SessionStatus::Idle);
    assert_eq!(main.display_status(), SessionStatus::Running);
    assert_eq!((main.side_chats_running, main.subagents_running), (3, 1));
    let detail: SessionDetail = env.get("/sessions/main").await;
    assert_eq!(&detail.session, main, "detail and list share counts and retain raw status");
    let stored = head.state.store.session("main").await.unwrap().unwrap();
    assert_eq!(stored.status, "idle");
    for (id, sides) in [("other", 0), ("realm-root", 1), ("side", 1), ("sibling", 0), ("nested", 0)] {
        let detail: SessionDetail = env.get(&format!("/sessions/{id}")).await;
        assert_eq!(detail.session.side_chats_running, sides, "{id}");
        assert_eq!(detail.session.subagents_running, 0, "subagents keep root-only semantics: {id}");
    }
    let scoped = c6_head::api::sessions_snapshot(&head.state, &c6_head::realms::Caller::in_realm("home")).await.unwrap();
    assert!(scoped.iter().all(|s| s.realm == "home"));
    assert_eq!(scoped.iter().find(|s| s.id == "main").unwrap(), main, "realm-scoped feed snapshot matches list");

    // Completed side chats no longer count; the root's still-active
    // subagent keeps overall display activity until it too completes.
    sqlx::query("UPDATE sessions SET status = 'idle' WHERE fork_seq IS NOT NULL AND realm = 'home'").execute(pool).await.unwrap();
    let detail: SessionDetail = env.get("/sessions/main").await;
    assert_eq!((detail.session.side_chats_running, detail.session.subagents_running), (0, 1));
    assert_eq!(detail.session.display_status(), SessionStatus::Running);
    sqlx::query("UPDATE sessions SET status = 'idle' WHERE id = 'agent'").execute(pool).await.unwrap();
    let detail: SessionDetail = env.get("/sessions/main").await;
    assert_eq!(detail.session.status, SessionStatus::Idle);
    assert_eq!(detail.session.display_status(), SessionStatus::Idle);
    assert_eq!((detail.session.side_chats_running, detail.session.subagents_running), (0, 0));
    let list: Vec<Session> = env.get("/sessions").await;
    assert_eq!(list.iter().find(|s| s.id == "main").unwrap(), &detail.session);
    head.shutdown().await.unwrap();
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
    let side_gate = mock.push_gated(Reply::Text("side answer".into()));
    let side: Session = env
        .post(
            &format!("/sessions/{}/side-chats", main.id),
            json!({"anchor_seq": anchor, "connection_id": conn.id, "model": "side-model", "prompt": "why?"}),
        )
        .await;
    assert_eq!((side.parent_id.as_deref(), side.fork_seq), (Some(main.id.as_str()), Some(anchor)));
    let active = eventually("side chat activity on its idle main", async || {
        let d: SessionDetail = env.get(&format!("/sessions/{}", main.id)).await;
        (d.session.side_chats_running == 1).then_some(d.session)
    })
    .await;
    assert_eq!(active.status, SessionStatus::Idle);
    assert_eq!(active.display_status(), SessionStatus::Running);
    assert_eq!(active.subagents_running, 0);
    let top: Vec<Session> = env.get("/sessions").await;
    assert_eq!(top.iter().find(|s| s.id == main.id).unwrap(), &active);
    side_gate.notify_one();
    let side_evs = env.idle(&side.id, 1).await;
    let d: SessionDetail = env.get(&format!("/sessions/{}", main.id)).await;
    assert_eq!(d.session.side_chats_running, 0);
    assert_eq!(d.session.display_status(), SessionStatus::Idle);
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
    let nested_gate = mock.push_gated(Reply::Text("nested answer".into()));
    let nested: Session = env
        .post(
            &format!("/sessions/{}/side-chats", side.id),
            json!({"anchor_seq": first_assistant(&side_evs), "connection_id": conn.id, "model": "side-model", "prompt": "deeper?"}),
        )
        .await;
    eventually("nested side chat activity on each ancestor", async || {
        let main: SessionDetail = env.get(&format!("/sessions/{}", main.id)).await;
        let side: SessionDetail = env.get(&format!("/sessions/{}", side.id)).await;
        (main.session.side_chats_running == 1 && side.session.side_chats_running == 1).then(|| {
            for s in [&main.session, &side.session] {
                assert_eq!(s.status, SessionStatus::Idle);
                assert_eq!(s.display_status(), SessionStatus::Running);
            }
        })
    })
    .await;
    nested_gate.notify_one();
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
    // The switch comes before the message (a checkpoint may sit between).
    let next = evs[switched + 1..].iter().find(|e| !matches!(e.kind, EventKind::Checkpoint { .. })).unwrap();
    assert!(matches!(&next.kind, EventKind::User { text, .. } if text == "now on another model"), "{next:?}");
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
