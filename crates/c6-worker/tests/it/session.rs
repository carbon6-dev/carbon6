//! M1 acceptance: a coding task runs end to end on a worker and survives a
//! head outage mid-turn.

use std::path::Path;

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn head_config(data_dir: &Path, worker_port: u16, ui_port: u16) -> HeadConfig {
    HeadConfig {
        data_dir: data_dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port,
        worker_port,
        auth: false,
        ..Default::default()
    }
}

#[tokio::test]
async fn task_runs_and_survives_head_outage() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "Always be brief.").unwrap();

    let head = c6_head::start(head_config(head_dir.path(), 0, 0)).await.unwrap();
    let (worker_port, ui_port) = (head.worker_addrs[0].port(), head.ui_addrs[0].port());
    let api = format!("http://127.0.0.1:{ui_port}/api/v1");
    let http = reqwest::Client::new();

    let enrollment = c6_head::api::new_enrollment(&head.state, Some("w1")).await.unwrap();
    let identity = enroll::enroll(&enrollment.token).await.unwrap();
    identity.save(worker_dir.path()).unwrap();
    let worker = Worker::open(worker_dir.path(), Identity::load(worker_dir.path()).unwrap()).await.unwrap();
    let worker_id = worker.identity.meta.worker_id;
    let (state_tx, mut state_rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), state_tx, stop.clone()));
    c6_testkit::within("the worker to attach", state_rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();

    // Connection and session through the HTTP API.
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "mock", "base_url": mock.base_url(), "api_key": "sk-test"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(conn.has_api_key);

    mock.push(Reply::ToolCalls(vec![("write".into(), r#"{"path":"hello.txt","content":"hello\n"}"#.into())]));
    mock.push(Reply::Text("Created hello.txt.".into()));
    let session: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({
            "worker_id": worker_id.0, "cwd": project.path().display().to_string(),
            "connection_id": conn.id, "model": "mock-1", "prompt": "Create hello.txt"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session.title, "Create hello.txt");

    let detail = |id: String| {
        let http = http.clone();
        let api = api.clone();
        async move { http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json::<SessionDetail>().await.ok() }
    };
    let events = |id: String| {
        let http = http.clone();
        let api = api.clone();
        async move { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json::<Vec<Event>>().await.unwrap() }
    };

    eventually("first turn to finish", async || {
        detail(session.id.clone())
            .await
            .filter(|d| d.session.status == SessionStatus::Idle && d.session.last_seq >= 9 && d.queued.is_empty())
    })
    .await;
    assert_eq!(std::fs::read_to_string(project.path().join("hello.txt")).unwrap(), "hello\n");

    let evs = events(session.id.clone()).await;
    let kinds: Vec<&str> = evs.iter().map(kind).collect();
    assert_eq!(
        kinds,
        ["started", "checkpoint", "user", "turn_started", "assistant", "usage", "tool_result", "assistant", "usage", "turn_ended"]
    );
    let EventKind::Started { system_prompt, .. } = &evs[0].kind else { panic!() };
    assert!(system_prompt.contains("Always be brief."), "AGENTS.md is in the prompt");
    assert_eq!(mock.requests()[0]["model"], "mock-1");

    // Second turn: the head goes away mid-stream; the worker finishes offline.
    let gate = mock.push_gated(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo offline > offline.txt"}"#.into())]));
    mock.push(Reply::Text("Wrote offline.txt.".into()));
    http.post(format!("{api}/sessions/{}/messages", session.id)).json(&json!({"text": "Now write offline.txt"})).send().await.unwrap();
    eventually("second turn to start", async || detail(session.id.clone()).await.filter(|d| d.session.status == SessionStatus::Running))
        .await;

    head.shutdown().await.unwrap();
    gate.notify_one();
    eventually("offline turn to finish on the worker", async || {
        let (_, evs) = worker.journal.load_session(&session.id).await.unwrap()?;
        matches!(evs.last()?.kind, Some(c6_proto::session_event::Kind::TurnEnded(_))).then_some(())
    })
    .await;
    assert!(project.path().join("offline.txt").exists());
    let (_, journal) = worker.journal.load_session(&session.id).await.unwrap().unwrap();

    // The head comes back on the same ports and catches up.
    let head = c6_head::start(head_config(head_dir.path(), worker_port, ui_port)).await.unwrap();
    let last = journal.last().unwrap().seq;
    eventually("head to catch up", async || {
        detail(session.id.clone()).await.filter(|d| d.session.last_seq == last && d.session.status == SessionStatus::Idle)
    })
    .await;

    let synced = head.state.store.events_after(&session.id, 0, 10_000).await.unwrap();
    assert_eq!(synced, journal, "head log equals the worker journal");
    // The stream replays the log, then says it has (what follows is live).
    let url = format!("ws://127.0.0.1:{}/api/v1/sessions/{}/stream?after=0", ui_port, session.id);
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let mut replayed = Vec::new();
    loop {
        use futures::StreamExt;
        let msg = c6_testkit::within("the next stream message", ws.next()).await.unwrap().unwrap();
        let tokio_tungstenite::tungstenite::Message::Text(text) = msg else { continue };
        match serde_json::from_str::<c6_api::StreamMsg>(&text).unwrap() {
            c6_api::StreamMsg::Event { event } => replayed.push(event.seq),
            c6_api::StreamMsg::Synced => break,
            _ => {}
        }
    }
    assert_eq!(replayed, synced.iter().map(|e| e.seq).collect::<Vec<_>>());
    eventually("worker to see the acks", async || worker.journal.unacked().await.unwrap().is_empty().then_some(())).await;

    stop.cancel();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_cwd_fails_the_session() {
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(head_config(head_dir.path(), 0, 0)).await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();

    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(worker_dir.path()).unwrap();
    let worker = Worker::open(worker_dir.path(), Identity::load(worker_dir.path()).unwrap()).await.unwrap();
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), watch::channel(ConnState::Connecting).0, stop.clone()));

    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "/definitely/not/here", "connection_id": conn.id, "model": "m", "prompt": "hi"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let detail = eventually("rejection", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", session.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Failed).then_some(d)
    })
    .await;
    assert!(detail.session.error.as_deref().unwrap().contains("does not exist"), "{:?}", detail.session);
    assert_eq!(detail.queued[0].rejected.as_deref(), Some("session failed to start"));
    let r = http.post(format!("{api}/sessions/{}/messages", session.id)).json(&json!({"text": "again"})).send().await.unwrap();
    assert_eq!(r.status(), 400);

    stop.cancel();
    head.shutdown().await.unwrap();
}

fn kind(e: &Event) -> &'static str {
    match e.kind {
        EventKind::Started { .. } => "started",
        EventKind::User { .. } => "user",
        EventKind::TurnStarted { .. } => "turn_started",
        EventKind::Assistant { .. } => "assistant",
        EventKind::ToolResult { .. } => "tool_result",
        EventKind::Usage { .. } => "usage",
        EventKind::TurnEnded { .. } => "turn_ended",
        EventKind::Error { .. } => "error",
        EventKind::Compaction { .. } => "compaction",
        EventKind::ModelSwitched { .. } => "model_switched",
        EventKind::Checkpoint { .. } => "checkpoint",
        EventKind::Rewound { .. } => "rewound",
        EventKind::BackgroundTask { .. } => "background_task",
        EventKind::Output { .. } => "output",
        EventKind::Memory { .. } => "memory",
        EventKind::Approval { .. } => "approval",
        EventKind::Notice { .. } => "notice",
        EventKind::Question { .. } => "question",
        EventKind::SubagentSpawned { .. } => "subagent_spawned",
        EventKind::SubagentModelSet { .. } => "subagent_model_set",
    }
}
