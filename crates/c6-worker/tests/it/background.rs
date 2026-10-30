//! Background tasks: a finished background command wakes the idle session
//! with a notice; a quiet one does not; one stopped from the UI says so; a
//! background subagent's report arrives as a notice.

use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus, TaskSnapshot};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn background_tasks_notify_the_agent() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let token = head.state.store.create_api_token("background inspection", None).await.unwrap().token.unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    let http = reqwest::Client::builder().default_headers(headers).build().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    let worker_conn = tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
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
        r#"{"command":"sleep 1; echo compiled; echo built ok; echo build warning >&2","run_in_background":true}"#.into(),
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
    assert!(third.contains("compiled") && third.contains("built ok") && third.contains("build warning"), "the end of its output: {third}");
    idle(&s.id).await;
    // The worker's events reach the head on their own path from its status:
    // wait for the notice itself.
    let evs = eventually("the notice in the session's events", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        evs.iter().any(|e| matches!(&e.kind, EventKind::User { command, .. } if command == "task-notification")).then_some(evs)
    })
    .await;
    let states: Vec<&str> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::BackgroundTask { state, .. } => Some(state.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(states, ["started", "finished"]);

    // Completion above required no polling. Inspect only after the notice has
    // already woken the model, and retain stdout and stderr independently.
    let snapshot = async |session: &str, task: &str| {
        http.get(format!("{api}/sessions/{session}/tasks/{task}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<TaskSnapshot>()
            .await
            .unwrap()
    };
    let completed = snapshot(&s.id, "bg1").await;
    assert_eq!(completed.task_id, "bg1");
    assert_eq!(completed.kind, "shell");
    assert_eq!(completed.command, "sleep 1; echo compiled; echo built ok; echo build warning >&2");
    assert!(!completed.running);
    assert_eq!(completed.exit_code, Some(0));
    assert!(completed.elapsed_ms > 0);
    assert_eq!(completed.stdout.text, "compiled\nbuilt ok\n");
    assert_eq!(completed.stderr.text, "build warning\n");
    assert!(completed.stdin.text.is_empty());
    assert!(completed.stdin.note.contains("closed"), "{:?}", completed.stdin);
    for stream in [&completed.stdin, &completed.stdout, &completed.stderr] {
        assert_eq!(stream.dropped_bytes, 0);
        assert!(!stream.note.is_empty());
    }
    assert_eq!(snapshot(&s.id, "bg1").await, completed, "inspection must not consume output");
    for (session, task) in [(s.id.as_str(), "missing-task"), ("missing-session", "bg1")] {
        let r = http.get(format!("{api}/sessions/{session}/tasks/{task}")).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND, "{session}/{task}");
    }
    let anon = reqwest::Client::new();
    for session in [s.id.as_str(), "missing-session"] {
        let r = anon.get(format!("{api}/sessions/{session}/tasks/bg1")).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED, "unauthenticated inspection must not disclose session existence");
    }
    let r = http.post(format!("{api}/sessions/{}/tasks/claude:native-task/stop", s.id)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST, "native task stopping must not silently succeed");
    let error = r.text().await.unwrap();
    assert!(error.contains("Claude Code owns this native task"), "{error}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(mock.requests().len(), 3, "inspection must not wake the model");

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
    let running = snapshot(&s.id, "bg3").await;
    assert_eq!(running.task_id, "bg3");
    assert_eq!(running.kind, "shell");
    assert_eq!(running.command, "sleep 30");
    assert!(running.running);
    assert_eq!(running.exit_code, None);
    assert!(running.stdout.text.is_empty() && running.stderr.text.is_empty());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.requests().len(), 7, "inspecting a running task must not wake the model");
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

    idle(&s.id).await;
    stop.cancel();
    c6_testkit::within("the worker connection to stop", worker_conn).await.unwrap().unwrap();
    eventually("the head to see the worker offline", async || {
        head.state.registry.get(worker.identity.meta.worker_id).is_none().then_some(())
    })
    .await;
    let offline = snapshot(&s.id, "bg1").await;
    assert_eq!(offline.task_id, completed.task_id);
    assert_eq!(offline.kind, completed.kind);
    assert_eq!(offline.command, completed.command);
    assert!(!offline.running);
    assert_eq!(offline.exit_code, Some(0));
    assert!(offline.elapsed_ms > 0);
    assert!(offline.stdin.text.is_empty());
    assert_eq!(offline.stdin.dropped_bytes, 0);
    assert!(offline.stdin.note.contains("not recorded"), "{:?}", offline.stdin);
    for stream in [&offline.stdout, &offline.stderr] {
        assert!(stream.text.is_empty(), "offline snapshots must not pretend journal output is a retained stream: {stream:?}");
        assert_eq!(stream.dropped_bytes, 0);
        assert!(stream.note.to_lowercase().contains("unavailable"), "an explicit unavailable-stream note: {stream:?}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.requests().len(), 12, "offline inspection must not wake the model");
    head.shutdown().await.unwrap();
}
