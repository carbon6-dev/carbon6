//! Views (docs/os.md §4.5): configured per realm (root's are everyone's),
//! checked when saved, and their actions on a record set its fields and
//! start a template about it.

use c6_api::{Connection, View};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

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
        "settings/templates?realm=crm",
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

/// Items of a shared kind (docs/knowledge.md §6.2, #6 and #7): root's
/// vocabulary gives each kind its shape, an app's items are checked against
/// it, and a messages view an app brings shows its items, its buttons
/// calling the app's operations.
#[tokio::test]
async fn item_views_and_shared_kinds() {
    let head_dir = tempfile::tempdir().unwrap();
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

    // The shared kinds, with their shapes and cards.
    let vocab = ok(send(M::GET, "settings/vocabulary", json!({})).await).await;
    let email = vocab.as_array().unwrap().iter().find(|e| e["name"] == "email" && e["kind"] == "kind").unwrap().clone();
    assert_eq!(email["schema"]["required"], json!(["thread_id"]), "{email}");
    assert!(email["card"]["invariants"].to_string().contains("thread_id"), "{email}");
    // An items view shows a kind its kind is for.
    let bad = json!([{"name": "x", "kind": "messages", "source": {"items": "photo"}}]);
    assert_eq!(send(M::PUT, "settings/views", bad).await.status(), StatusCode::BAD_REQUEST);

    // A mail app: its messages view, and an operation its Archive button calls.
    let pkg = json!({"package": "mailbox", "version": "1.0.0",
        "contents": {
            "collections": [{"name": "archived", "schema": {"type": "object", "properties": {"item": {"type": "string"}}}}],
            "views": [{"name": "inbox", "kind": "messages", "source": {"items": "email"},
                       "actions": [{"label": "Archive", "operation": "archive", "input": {"item": "$.id"}}]}]},
        "operations": [{"name": "archive", "input": {"type": "object", "properties": {"item": {"type": "string"}}},
                        "write": {"collection": "archived", "fields": {"item": "$.item"}}}]});
    ok(send(M::POST, "packages", json!({"manifest": pkg})).await).await;
    let i: c6_api::Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "mailbox"})).await).await).unwrap();
    ok(send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await).await;

    // Its items (as its kit's out/ingest.json would give them); one without
    // a thread is not an email.
    let mail = |id: &str, thread: Option<&str>, at: i64, subject: &str| {
        let mut meta = json!({"from": "ada@example.com"});
        if let Some(t) = thread {
            meta["thread_id"] = json!(t);
        }
        json!({"kind": "email", "external_id": id, "title": subject, "text": format!("From: ada@example.com\n{subject}"), "created_at": at, "meta": meta})
    };
    let done = c6_head::kitjobs::ingest(
        &head.state,
        "home",
        "mailbox",
        "kit:test.sync",
        &json!({"items": [mail("m1", Some("t1"), 1_000, "Engines"), mail("m2", Some("t1"), 2_000, "Re: Engines"), mail("m3", None, 3_000, "Nope")]}),
    )
    .await
    .unwrap();
    assert_eq!(done["items"], 2, "{done}");
    assert!(done["errors"].to_string().contains("thread_id"), "{done}");

    let items: Vec<c6_api::ViewItem> = serde_json::from_value(ok(send(M::GET, "views/inbox/items", json!({})).await).await).unwrap();
    assert_eq!(items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["Re: Engines", "Engines"]);
    assert!(items.iter().all(|i| i.source == "app:mailbox" && i.meta["thread_id"] == "t1"));
    let older: Vec<c6_api::ViewItem> =
        serde_json::from_value(ok(send(M::GET, "views/inbox/items?before=2000", json!({})).await).await).unwrap();
    assert_eq!(older.len(), 1);

    // Archive calls the app's operation with the item.
    let out = ok(send(M::POST, "views/inbox/act", json!({"action": "Archive", "item": items[0].id})).await).await;
    assert_eq!(out["item"], items[0].id.as_str(), "{out}");
    let archived = ok(send(M::POST, "collections/archived/query", json!({})).await).await;
    assert_eq!(archived["records"][0]["fields"]["item"], items[0].id.as_str(), "{archived}");
    assert!(archived["records"][0]["created_by"].as_str().unwrap().starts_with("app:mailbox"), "{archived}");

    head.shutdown().await.unwrap();
}
