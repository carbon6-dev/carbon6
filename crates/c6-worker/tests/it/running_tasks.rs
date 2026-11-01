//! A session's background tasks keep it showing as running after its turn
//! ends; a server (notify: false) is listed but doesn't (docs/background.md
//! §4).

use c6_api::{Session, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn background_tasks_keep_a_session_running() {
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
    let worker = crate::open_worker(state.path()).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(c6_worker::conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let p = project.path();
    let wait_for = |f: &str| format!("until [ -f {f} ]; do sleep 0.05; done; echo {f}");
    mock.push(Reply::ToolCalls(vec![
        ("bash".into(), json!({"command": wait_for("build-done"), "run_in_background": true}).to_string()),
        ("bash".into(), json!({"command": wait_for("server-done"), "run_in_background": true, "notify": false}).to_string()),
    ]));
    mock.push(Reply::Text("Started the build and the server.".into()));
    mock.push(Reply::Text("The build finished.".into()));
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": p.display().to_string(), "connection_id": conn["id"], "model": "m", "prompt": "build it"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    let row = async || -> Option<Session> {
        let list: Vec<Session> = http.get(format!("{api}/sessions")).send().await.ok()?.json().await.ok()?;
        list.into_iter().find(|s| s.id == id)
    };

    // Its turn is over, but the build runs: it shows as running.
    let r = eventually("the turn to end with both running", async || {
        row().await.filter(|r| r.status == SessionStatus::Idle && r.tasks_running == 1 && r.servers_running == 1)
    })
    .await;
    assert_eq!(r.display_status(), SessionStatus::Running);

    // The build ends (and wakes the agent); the server alone doesn't keep it running.
    std::fs::write(p.join("build-done"), "").unwrap();
    let r = eventually("only the server left", async || {
        row().await.filter(|r| r.status == SessionStatus::Idle && r.tasks_running == 0 && r.servers_running == 1)
    })
    .await;
    assert_eq!(r.display_status(), SessionStatus::Idle);
    std::fs::write(p.join("server-done"), "").unwrap();
    eventually("nothing running", async || row().await.filter(|r| r.tasks_running == 0 && r.servers_running == 0)).await;
    stop.cancel();
}
