//! Sessions without a folder work in a scratch workspace of their own.

use c6_api::{Connection, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn sessions_without_a_folder_get_a_scratch_workspace() {
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

    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"pwd; echo made > notes.txt; ls"}"#.into())]));
    mock.push(Reply::Text("done".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "make a note"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let d: SessionDetail = eventually("idle", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && mock.requests().len() == 2).then_some(d)
    })
    .await;
    let scratch = state.path().join("scratch").join(&s.id);
    assert!(scratch.join("notes.txt").exists(), "the command ran in the scratch workspace");
    assert!(d.session.cwd.ends_with(&format!("scratch/{}", s.id)), "the head learns the folder: {}", d.session.cwd);
    let out = mock.requests()[1]["messages"].to_string();
    assert!(out.contains(&format!("scratch/{}", s.id)) && out.contains("notes.txt"), "{out}");
    // A worktree needs a folder.
    let r = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "worktree": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    stop.cancel();
    head.shutdown().await.unwrap();
}
