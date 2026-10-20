//! The API reference tool: an agent writing an integration reads how to
//! call carbon6 (base URL, auth, operations) and saves an OpenAPI document.

use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn agents_read_the_api_reference() {
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
    // The public address comes first in the reference.
    let r = http.put(format!("{api}/settings/public_url")).json(&json!({"url": "https://c6.example.net/"})).send().await.unwrap();
    assert!(r.status().is_success());

    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![
        ("carbon6_api".into(), "{}".into()),
        ("carbon6_api".into(), r#"{"query":"upload a document to the memex"}"#.into()),
        ("carbon6_api".into(), r#"{"op":"sessions.create"}"#.into()),
        ("carbon6_api".into(), r#"{"op":"no.such"}"#.into()),
        ("carbon6_api".into(), r#"{"save_openapi":"spec/carbon6.json"}"#.into()),
    ]));
    mock.push(Reply::Text("I know how to call carbon6.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "write an importer"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let results: Vec<(String, bool)> = eventually("five results", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        let r: Vec<_> = evs
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolResult { tool, output, is_error, .. } if tool == "carbon6_api" => Some((output.clone(), *is_error)),
                _ => None,
            })
            .collect();
        (r.len() == 5).then_some(r)
    })
    .await;

    let (basics, found, one, missing, saved) = (&results[0].0, &results[1].0, &results[2].0, &results[3], &results[4].0);
    assert!(basics.starts_with("carbon6 HTTP API\n- Base URL: https://c6.example.net or http"), "{basics}");
    assert!(basics.contains("Authorization: Bearer c6_") && basics.contains("- knowledge ("), "{basics}");
    assert!(found.contains("Matching operations") && found.contains("/api/v1/memex/upload"), "{found}");
    assert!(one.starts_with("sessions.create [write]") && one.contains("POST /api/v1/sessions\n"), "{one}");
    assert!(missing.1 && missing.0.contains("no operation \"no.such\""), "{missing:?}");
    assert!(!results[4].1 && saved.contains("Wrote the OpenAPI 3.1 document to "), "{saved}");

    // The file is in the session's folder, and is the same document the
    // head serves.
    let path = saved.trim_start_matches("Wrote the OpenAPI 3.1 document to ").split(" (").next().unwrap();
    assert!(path.ends_with("spec/carbon6.json"), "{path}");
    let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let served: serde_json::Value = http.get(format!("{api}/openapi.json")).send().await.unwrap().json().await.unwrap();
    assert_eq!(file, served);
    assert_eq!(file["servers"][0]["url"], "https://c6.example.net");
    assert!(file["paths"]["/api/v1/sessions"]["post"]["requestBody"].is_object());
    stop.cancel();
}
