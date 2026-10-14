//! Views (docs/os.md §4.5): configured per realm (root's are everyone's),
//! checked when saved, and their actions on a record set its fields and
//! start a template about it.

use std::time::Duration;

use c6_api::{Connection, View};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
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
async fn views_per_realm_and_their_actions() {
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
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };

    // Checked when saved.
    for bad in [
        json!([{"name": "x y", "kind": "table", "source": {"collection": "deals"}}]),
        json!([{"name": "x", "kind": "spreadsheet"}]),
        json!([{"name": "x", "kind": "table", "source": {}}]),
        json!([{"name": "x", "kind": "board", "source": {"collection": "deals"}}]),
        json!([{"name": "x", "kind": "chart", "source": {"collection": "deals"}}]),
        json!([{"name": "x", "kind": "page"}, {"name": "x", "kind": "page"}]),
        json!([{"name": "x", "kind": "table", "source": {"collection": "deals"}, "actions": [{"label": "Do"}]}]),
    ] {
        assert_eq!(send(M::PUT, "settings/views", bad.clone()).await.status(), StatusCode::BAD_REQUEST, "{bad}");
    }

    // A board in a realm of its own, and a page in root (everyone's).
    ok(send(M::POST, "realms", json!({"id": "crm"})).await).await;
    let schema = json!({"type": "object", "required": ["name", "stage"], "properties": {
        "name": {"type": "string"}, "stage": {"type": "string", "enum": ["new", "talking", "won"]}}});
    ok(send(M::POST, "collections?realm=crm", json!({"name": "deals", "schema": schema})).await).await;
    let deal = ok(send(M::POST, "collections/deals/records?realm=crm", json!({"fields": {"name": "Acme", "stage": "new"}})).await).await;
    ok(send(
        M::PUT,
        "settings/templates",
        json!([{"name": "follow-up", "prompt": "Follow up on this deal:\n{{input}}", "connection": conn.id, "model": "m"}]),
    )
    .await)
    .await;
    ok(send(
        M::PUT,
        "settings/views?realm=crm",
        json!([{"name": "pipeline", "title": "Pipeline", "kind": "board", "group_by": "stage", "source": {"collection": "deals"},
                "actions": [{"label": "Talk", "set": {"stage": "talking"}, "start": "follow-up"}, {"label": "Won", "set": {"stage": "won"}}]}]),
    )
    .await)
    .await;
    ok(send(M::PUT, "settings/views?realm=root", json!([{"name": "today", "kind": "page", "body": "# Today"}])).await).await;
    let crm: Vec<View> = send(M::GET, "views?realm=crm", json!({})).await.json().await.unwrap();
    assert_eq!(crm.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), vec!["today", "pipeline"]);
    let home: Vec<View> = send(M::GET, "views", json!({})).await.json().await.unwrap();
    assert_eq!(home.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), vec!["today"], "crm's board stays in crm");

    // An action: sets the stage and starts the template about the record.
    mock.push(Reply::Text("Drafted.".into()));
    let id = deal["id"].as_str().unwrap();
    assert_eq!(
        send(M::POST, "views/pipeline/act?realm=crm", json!({"action": "Lose", "record": id})).await.status(),
        StatusCode::BAD_REQUEST
    );
    let out = ok(send(M::POST, "views/pipeline/act?realm=crm", json!({"action": "Talk", "record": id})).await).await;
    assert_eq!(out["record"]["fields"]["stage"], "talking");
    let s = &out["run"]["sessions"][0];
    assert_eq!(s["realm"], "crm");
    assert!(s["labels"].as_array().unwrap().contains(&json!(format!("record:{id}"))), "{s}");
    eventually("the prompt with the record", async || {
        mock.requests()
            .iter()
            .any(|r| r["messages"].to_string().contains("Follow up on this deal") && r["messages"].to_string().contains("Acme"))
            .then_some(())
    })
    .await;
    // A realm's token works its own views.
    let t: Value = send(M::POST, "auth/tokens", json!({"name": "c", "realm": "crm"})).await.json().await.unwrap();
    let r = http
        .post(format!("{api}/views/pipeline/act"))
        .bearer_auth(t["token"].as_str().unwrap())
        .json(&json!({"action": "Won", "record": id}))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["record"]["fields"]["stage"], "won", "{v}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
