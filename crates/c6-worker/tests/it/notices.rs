//! Notices that are never lost (docs/background.md §5): a background task's
//! end held by the hourly limit is recorded, survives a worker restart, is
//! shown as waiting, and reaches the agent once when you deliver it.

use std::path::Path;
use std::sync::Arc;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
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

#[tokio::test]
async fn a_held_notice_survives_a_worker_restart() {
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
    let (worker, stop) = run_worker(state.path()).await;
    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // One automatic turn an hour.
    let limits = json!({"max_depth": 3, "max_running": 16, "auto_turns_per_hour": 1, "claude_code_idle_minutes": 15});
    assert!(http.put(format!("{api}/settings/subagent-limits")).json(&limits).send().await.unwrap().status().is_success());

    let events =
        async |id: &str| -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
    let idle = async |id: &str, turns: usize| {
        eventually("idle", async || {
            let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
            let ended = events(id).await.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (d.session.status == SessionStatus::Idle && d.queued.is_empty() && ended == turns).then_some(())
        })
        .await
    };
    // Two background commands that end when the test says.
    let p = project.path();
    let wait_for = |f: &str| format!("until [ -f {f} ]; do sleep 0.05; done; echo {f} done");
    mock.push(Reply::ToolCalls(vec![
        ("bash".into(), json!({"command": wait_for("go1"), "run_in_background": true}).to_string()),
        ("bash".into(), json!({"command": wait_for("go2"), "run_in_background": true}).to_string()),
    ]));
    mock.push(Reply::Text("Started both.".into()));
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": p.display().to_string(), "connection_id": conn["id"], "model": "m", "prompt": "start two"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    idle(&id, 1).await;

    // The first end wakes the agent: the hour's one automatic turn.
    mock.push(Reply::Text("Saw one.".into()));
    std::fs::write(p.join("go1"), "").unwrap();
    idle(&id, 2).await;
    // The second is held, and the log says so.
    std::fs::write(p.join("go2"), "").unwrap();
    eventually("the held notice to be announced", async || {
        events(&id)
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("Background task bg2 ended")))
            .then_some(())
    })
    .await;
    assert_eq!(mock.requests().len(), 3, "the limit holds it: no turn");

    // The worker restarts; the held notice is not in memory any more.
    stop.cancel();
    drop(worker);
    let (worker, stop) = run_worker(state.path()).await;
    // It is still waiting, as its events say (what the UI's pill shows).
    let waiting = |evs: &[Event]| {
        evs.iter().filter(|e| matches!(&e.kind, EventKind::NoticeQueued { command_id, .. } if command_id == "task:bg2:finished")).count()
            == 1
            && !evs.iter().any(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id == "task:bg2:finished"))
    };
    assert!(waiting(&events(&id).await));
    // Deliver now: a turn takes it, whatever the limit.
    mock.push(Reply::Text("bg2 ended too.".into()));
    let r = http.post(format!("{api}/sessions/{id}/notices/deliver")).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT);
    idle(&id, 3).await;
    let last = mock.requests().last().unwrap()["messages"].to_string();
    assert!(last.contains(r#"id=\"bg2\""#), "{last}");
    let after = events(&id).await;
    assert!(!waiting(&after));
    let told = after.iter().filter(|e| matches!(&e.kind, EventKind::User { text, .. } if text.contains(r#"id="bg2""#))).count();
    assert_eq!(told, 1, "told once");
    // Your next message doesn't bring it again.
    mock.push(Reply::Text("Nothing else.".into()));
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "anything else?"})).send().await.unwrap();
    idle(&id, 4).await;
    assert!(!mock.requests().last().unwrap()["messages"].to_string().ends_with(r#"id=\"bg2\""#));
    let told = events(&id).await.iter().filter(|e| matches!(&e.kind, EventKind::User { text, .. } if text.contains(r#"id="bg2""#))).count();
    assert_eq!(told, 1, "still told once");
    drop(worker);
    stop.cancel();
}
