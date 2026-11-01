//! A message you send while the agent works is with the agent, not just
//! queued: the queue says so until the agent takes it at its next step
//! (docs/background.md §4).

use c6_api::{SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn a_message_sent_mid_turn_is_with_the_agent() {
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
    let detail =
        async |id: &str| -> Option<SessionDetail> { http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok() };

    // A turn that holds after its first words.
    let gate = mock.push_gated(Reply::Text("Working on the first.".into()));
    mock.push(Reply::Text("Both done.".into()));
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(), "connection_id": conn["id"], "model": "m", "prompt": "first"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    eventually("the turn to be running", async || (mock.requests().len() == 1).then_some(())).await;

    // Sent now: the agent has it, and reads it at its next step.
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "second"})).send().await.unwrap();
    let queued = eventually("the message to be with the agent", async || {
        let d = detail(&id).await?;
        (d.queued.len() == 1 && d.queued[0].with_agent).then_some(d.queued)
    })
    .await;
    assert_eq!(queued[0].text, "second");

    // The step ends: the agent takes it, and the queue empties.
    gate.notify_one();
    eventually("the session to be idle with nothing queued", async || {
        let d = detail(&id).await?;
        (d.session.status == SessionStatus::Idle && d.queued.is_empty()).then_some(())
    })
    .await;
    assert_eq!(mock.requests().len(), 2);
    assert!(mock.requests()[1]["messages"].to_string().contains("second"), "it joined the turn");
    stop.cancel();
}
