//! Memory: notes reach sessions whose labels match their scope; agents
//! add, correct and remove them; the next session sees the change.

use std::time::Duration;

use c6_api::Connection;
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
async fn memory_carries_across_sessions() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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

    let note = |v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/memories"));
        async move { http.post(url).json(&v).send().await.unwrap() }
    };
    assert_eq!(note(json!({"text": "", "scope": ""})).await.status(), reqwest::StatusCode::BAD_REQUEST);
    note(json!({"text": "The user likes short answers.", "scope": ""})).await;
    note(json!({"text": "Acme's staging is at staging.acme.test.", "scope": "client:acme"})).await;
    let py: serde_json::Value = note(json!({"text": "Use uv for Python.", "scope": "python"})).await.json().await.unwrap();
    let py_id = py["id"].as_str().unwrap().to_owned();

    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![
        ("remember".into(), r#"{"text":"Acme's database listens on port 5433.","scope":"client:acme"}"#.into()),
        ("forget".into(), format!(r#"{{"id":"{py_id}"}}"#)),
    ]));
    mock.push(Reply::Text("Noted.".into()));
    let start = |prompt: &str| {
        let (http, url) = (http.clone(), format!("{api}/sessions"));
        let body =
            json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": prompt, "labels": ["client:acme"]});
        async move { http.post(url).json(&body).send().await.unwrap() }
    };
    start("set up").await;
    eventually("first session", async || (mock.requests().len() == 2).then_some(())).await;
    let system = mock.requests()[0]["messages"][0]["content"].as_str().unwrap().to_owned();
    assert!(system.contains("# Memory") && system.contains("(client:acme) Acme's staging is at staging.acme.test."), "{system}");
    assert!(system.contains("(all) The user likes short answers.") && !system.contains("Use uv"), "{system}");

    // The agent's changes reach the head.
    let notes: serde_json::Value = eventually("notes updated", async || {
        let n: serde_json::Value = http.get(format!("{api}/memories")).send().await.ok()?.json().await.ok()?;
        (n.to_string().contains("port 5433") && !n.to_string().contains("Use uv")).then_some(n)
    })
    .await;
    let new = notes.as_array().unwrap().iter().find(|n| n["text"].as_str().unwrap().contains("5433")).unwrap();
    assert_eq!(new["scope"], "client:acme");
    assert!(new["session_id"].is_string(), "remembers who wrote it");

    // The next session starts with it.
    mock.push(Reply::Text("ok".into()));
    start("next").await;
    eventually("second session", async || (mock.requests().len() == 3).then_some(())).await;
    let system = mock.requests()[2]["messages"][0]["content"].as_str().unwrap().to_owned();
    assert!(system.contains("Acme's database listens on port 5433."), "{system}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
