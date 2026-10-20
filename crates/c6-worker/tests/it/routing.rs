//! Model routing: sessions started without a model get one from the
//! first routing rule matching their labels (with its connection, effort
//! and subagent model); otherwise the latest model on the connection.

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn routes_pick_connection_and_model() {
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
    let add = |name: &str| {
        let (http, url, body) = (http.clone(), format!("{api}/connections"), json!({"name": name, "base_url": mock.base_url()}));
        async move { http.post(url).json(&body).send().await.unwrap().json::<Connection>().await.unwrap() }
    };
    let (main, cheap) = (add("main").await, add("cheap").await);

    let put = |v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/routes"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put(json!([{"name": "x", "connection": "nope", "model": "m"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put(json!([{"name": "x", "model": "m", "effort": "huge"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put(json!([{"name": "x", "model": ""}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert!(
        put(json!([
            {"name": "off", "model": "never", "enabled": false},
            {"name": "ops", "applies_to": ["ops"], "connection": "cheap", "model": "small", "effort": "low", "subagent_model": "tiny"},
            {"name": "default", "model": "big"}
        ]))
        .await
        .is_success()
    );

    let wid = worker.identity.meta.worker_id.0;
    let create = |body: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/sessions"));
        let mut body = body;
        body["worker_id"] = json!(wid);
        body["cwd"] = json!("");
        async move { http.post(url).json(&body).send().await.unwrap() }
    };

    // Labels ops: the ops rule, on its connection; subagents on tiny.
    mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"look","prompt":"Look around."}"#.into())]));
    mock.push(Reply::Text("nothing here".into())); // the subagent
    mock.push(Reply::Text("done".into()));
    let s: Session = create(json!({"prompt": "check disks", "labels": ["ops"]})).await.json().await.unwrap();
    assert_eq!((s.connection_id.as_str(), s.model.as_str()), (cheap.id.as_str(), "small"));
    assert_eq!(s.effort.as_deref(), Some("low"));
    eventually("all requests", async || (mock.requests().len() == 3).then_some(())).await;
    let models: Vec<String> = mock.requests().iter().map(|r| r["model"].as_str().unwrap_or_default().to_owned()).collect();
    assert_eq!(models, ["small", "tiny", "small"]);
    let child = eventually("the subagent session", async || {
        let c: Vec<Session> = http.get(format!("{api}/sessions/{}/children", s.id)).send().await.ok()?.json().await.ok()?;
        c.into_iter().next()
    })
    .await;
    assert_eq!(child.model, "tiny", "the subagent session shows its model");

    // No labels: the catch-all rule. A model given: as is.
    let s: Session = create(json!({})).await.json().await.unwrap();
    assert_eq!(s.model, "big");
    let s: Session = create(json!({"connection_id": main.id, "model": "mine", "labels": ["ops"]})).await.json().await.unwrap();
    assert_eq!((s.connection_id.as_str(), s.model.as_str()), (main.id.as_str(), "mine"));
    // A connection given: only rules for it (or for any) apply.
    let s: Session = create(json!({"connection_id": main.id, "labels": ["ops"]})).await.json().await.unwrap();
    assert_eq!((s.connection_id.as_str(), s.model.as_str()), (main.id.as_str(), "big"));

    // No rules: the latest model on the connection, else an error.
    assert!(put(json!([])).await.is_success());
    let s: Session = create(json!({"connection_id": main.id})).await.json().await.unwrap();
    assert_eq!(s.model, "big");
    let s: Session = create(json!({})).await.json().await.unwrap();
    assert_eq!((s.connection_id.as_str(), s.model.as_str()), (main.id.as_str(), "big"), "your latest connection");
    let none = add("fresh").await;
    assert_eq!(create(json!({"connection_id": none.id})).await.status(), reqwest::StatusCode::BAD_REQUEST);

    stop.cancel();
    head.shutdown().await.unwrap();
}
