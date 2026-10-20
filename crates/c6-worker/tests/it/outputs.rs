//! Outputs: the agent publishes a deliverable; each publish is a version,
//! kept in the session and served (sandboxed) for preview and download.

use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn published_outputs_are_versioned_and_served() {
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
    mock.push(Reply::ToolCalls(vec![
        ("bash".into(), r#"{"command":"printf '<h1>Deck v1</h1>' > deck.html"}"#.into()),
        ("publish_file".into(), r#"{"path":"deck.html","title":"Q3 deck"}"#.into()),
    ]));
    mock.push(Reply::ToolCalls(vec![
        ("bash".into(), r#"{"command":"printf '<h1>Deck v2</h1>' > deck.html"}"#.into()),
        ("publish_file".into(), r#"{"path":"deck.html","title":"Q3 deck"}"#.into()),
        ("publish_file".into(), r#"{"path":"missing.pdf"}"#.into()),
    ]));
    mock.push(Reply::Text("Here is the deck.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "make a deck"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outputs: Vec<(u64, String, String, u64)> = eventually("two outputs", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        let outs: Vec<_> = evs
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Output { name, title, size, .. } => Some((e.seq, name.clone(), title.clone(), *size)),
                _ => None,
            })
            .collect();
        (outs.len() == 2 && mock.requests().len() == 3).then_some(outs)
    })
    .await;
    assert_eq!((outputs[0].1.as_str(), outputs[0].2.as_str(), outputs[0].3), ("deck.html", "Q3 deck", 16));
    // Each version keeps its own content.
    let get = |seq: u64, q: &str| http.get(format!("{api}/sessions/{}/outputs/{seq}{q}", s.id)).send();
    let v1 = get(outputs[0].0, "").await.unwrap();
    assert!(v1.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    assert!(v1.headers()["content-security-policy"].to_str().unwrap().starts_with("sandbox"), "agent pages are sandboxed");
    assert_eq!(v1.text().await.unwrap(), "<h1>Deck v1</h1>");
    assert_eq!(get(outputs[1].0, "").await.unwrap().text().await.unwrap(), "<h1>Deck v2</h1>");
    let dl = get(outputs[1].0, "?download=1").await.unwrap();
    assert_eq!(dl.headers()["content-disposition"], "attachment; filename=\"deck.html\"");
    // A missing file is an error for the model, not an output.
    let third = mock.requests()[2]["messages"].to_string();
    assert!(third.contains("missing.pdf is not a file"), "{third}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
