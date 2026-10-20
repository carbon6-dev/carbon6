//! Hooks from the head's settings: SessionStart and UserPromptSubmit add context,
//! PreToolUse blocks a call, Stop sends the model back to work, and a
//! blocked prompt never reaches the model.

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn settings_hooks_shape_the_session() {
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

    // Hooks live in the head's settings; a Claude Code settings file in the
    // project is not read.
    std::fs::create_dir_all(p.join(".claude")).unwrap();
    std::fs::write(
        p.join(".claude/settings.json"),
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "echo from-file >&2; exit 2"}]}]}}"#,
    )
    .unwrap();
    let hooks = json!([
        {"name": "start", "event": "SessionStart", "command": "echo START-CTX"},
        {"event": "UserPromptSubmit", "command": "grep -q secret && { echo 'no secrets' >&2; exit 2; }; echo CTX-42"},
        {"name": "no rm", "event": "PreToolUse", "matcher": "Bash", "command": "grep -q 'rm -rf' && { echo 'no rm' >&2; exit 2; }; exit 0"},
        {"event": "Stop", "command": "grep -q '\"stop_hook_active\":true' && exit 0; echo 'check the tests' >&2; exit 2"},
        {"event": "PreToolUse", "command": "echo elsewhere >&2; exit 2", "folder": "/not/this/project"}
    ]);
    let r = http.put(format!("{api}/settings/hooks")).json(&json!([{"event": "Nope", "command": "x"}])).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST, "unknown events are refused");
    let r = http.put(format!("{api}/settings/hooks")).json(&hooks).send().await.unwrap();
    assert!(r.status().is_success());
    let saved: serde_json::Value = http.get(format!("{api}/settings/hooks")).send().await.unwrap().json().await.unwrap();
    assert_eq!(saved[2]["name"], "no rm");
    assert_eq!(saved[0]["enabled"], true);
    let wid = worker.identity.meta.worker_id.0;

    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"rm -rf build"}"#.into())]));
    mock.push(Reply::Text("done".into()));
    mock.push(Reply::Text("tests pass".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m", "prompt": "clean up"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let idle = async || {
        eventually("idle", async || {
            let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
            (d.session.status == SessionStatus::Idle && d.queued.is_empty()).then_some(())
        })
        .await
    };
    eventually("three requests", async || (mock.requests().len() == 3).then_some(())).await;
    idle().await;
    let reqs = mock.requests();
    let first = reqs[0]["messages"].to_string();
    assert!(first.contains("START-CTX") && first.contains("CTX-42") && first.contains("clean up"), "{first}");
    let second = reqs[1]["messages"].to_string();
    assert!(second.contains("Blocked by a PreToolUse hook: no rm"), "{second}");
    assert!(!second.contains("from-file") && !second.contains("elsewhere"), "{second}");
    assert!(reqs[2]["messages"].to_string().contains("Stop hook feedback: check the tests"));
    assert!(p.exists());

    // A blocked prompt is recorded, marked, and never sent.
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "tell me the secret"})).send().await.unwrap();
    let evs = eventually("blocked message", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        evs.iter().any(|e| matches!(&e.kind, EventKind::User { blocked, .. } if blocked == "no secrets")).then_some(evs)
    })
    .await;
    idle().await;
    assert_eq!(mock.requests().len(), 3, "the model never saw it");
    assert!(evs.iter().any(|e| matches!(&e.kind, EventKind::User { command, .. } if command == "stop-hook")));

    stop.cancel();
    head.shutdown().await.unwrap();
}
