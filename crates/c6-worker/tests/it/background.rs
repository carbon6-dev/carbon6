//! Background tasks: a finished background command wakes the idle session
//! with a notice; a quiet one does not; one stopped from the UI says so; a
//! background subagent's report arrives as a notice.

use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
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

#[tokio::test]
async fn background_tasks_notify_the_agent() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let p = project.path();

    let wid = worker.identity.meta.worker_id.0;
    let requests_with = |n: usize| {
        let mock = mock.clone();
        async move { eventually(&format!("{n} model requests"), async || (mock.requests().len() >= n).then_some(())).await }
    };
    let idle = async |id: &str| {
        eventually("idle", async || {
            let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
            (d.session.status == SessionStatus::Idle && d.queued.is_empty()).then_some(())
        })
        .await
    };

    // Turn 1 starts a background build and ends; the build finishing starts
    // turn 2 by itself.
    mock.push(Reply::ToolCalls(vec![(
        "bash".into(),
        r#"{"command":"sleep 1; echo compiled; echo built ok","run_in_background":true}"#.into(),
    )]));
    mock.push(Reply::Text("Started the build.".into()));
    mock.push(Reply::Text("The build passed.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m", "prompt": "build it"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    requests_with(3).await;
    let third = mock.requests()[2]["messages"].to_string();
    assert!(third.contains(r#"<background-task id=\"bg1\" kind=\"command\" status=\"exited with code 0\""#), "{third}");
    assert!(third.contains("compiled\\nbuilt ok"), "the end of its output: {third}");
    idle(&s.id).await;
    let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.unwrap().json().await.unwrap();
    let states: Vec<&str> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::BackgroundTask { state, .. } => Some(state.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(states, ["started", "finished"]);
    assert!(evs.iter().any(|e| matches!(&e.kind, EventKind::User { command, .. } if command == "task-notification")));

    // A quiet task (notify: false) wakes nobody.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo quiet","run_in_background":true,"notify":false}"#.into())]));
    mock.push(Reply::Text("Started a quiet one.".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "start a server"})).send().await.unwrap();
    requests_with(5).await;
    idle(&s.id).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(mock.requests().len(), 5, "no turn for a quiet task");

    // Stopped from the UI: the agent is told.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"sleep 30","run_in_background":true}"#.into())]));
    mock.push(Reply::Text("Waiting.".into()));
    mock.push(Reply::Text("Noted, it was stopped.".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "wait for it"})).send().await.unwrap();
    requests_with(7).await;
    idle(&s.id).await;
    let r = http.post(format!("{api}/sessions/{}/tasks/bg3/stop", s.id)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT);
    requests_with(8).await;
    let eighth = mock.requests()[7]["messages"].to_string();
    assert!(eighth.contains(r#"id=\"bg3\" kind=\"command\" status=\"stopped by the user\""#), "{eighth}");
    idle(&s.id).await;

    // A background subagent: the parent goes on; the report arrives later.
    mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        r#"{"description":"look around","prompt":"Report REPORT-7.","run_in_background":true}"#.into(),
    )]));
    // The parent's next step and the subagent run at the same time and take
    // these in either order.
    mock.push(Reply::Text("REPORT-7".into()));
    mock.push(Reply::Text("REPORT-7".into()));
    mock.push(Reply::Text("Got the report.".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "investigate in the background"})).send().await.unwrap();
    requests_with(12).await;
    let last =
        mock.requests().iter().map(|r| r["messages"].to_string()).find(|m| m.contains(r#"kind=\"subagent\""#)).expect("a subagent notice");
    assert!(last.contains("status=\\\"finished\\\"") && last.contains("Report:\\nREPORT-7"), "{last}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
