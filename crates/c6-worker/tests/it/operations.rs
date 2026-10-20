//! Operations (docs/apps.md §7): the way anyone but an app changes what it
//! owns. A write runs at once, as the app, checked against its input's
//! schema and the record's state; a template starts the app's session.
//! You call them through the API, agents with app_call (another realm
//! through a grant), triggers with the operation action, and views through
//! their actions.

use c6_api::{Connection, Install};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn operations_change_what_apps_own() {
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
    let conn: Connection =
        serde_json::from_value(ok(send(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).await).unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let wid = worker.identity.meta.worker_id.0;

    // Checked at install: an operation writes what the package owns, and
    // starts one of its templates.
    let bad = ok(send(
        M::POST,
        "apps/plan",
        json!({"package": "x", "version": "1.0.0", "contents": {"collections": [{"name": "a", "schema": {"type": "object"}}]},
        "operations": [{"name": "w", "write": {"collection": "b", "fields": {}}}, {"name": "t", "template": "nope"}]}),
    )
    .await)
    .await;
    let problems = bad["problems"].to_string();
    assert!(problems.contains("writes b, which the package does not own") && problems.contains("starts nope"), "{problems}");

    let crm = json!({
        "package": "crm", "version": "1.0.0",
        "contents": {
            "collections": [{"name": "relationships", "schema": {"type": "object", "required": ["name"], "properties": {
                "name": {"type": "string"}, "weeks": {"type": "integer"}, "state": {"enum": ["active", "paused"]}}}}],
            "records": {"relationships": [{"name": "Ada", "weeks": 6, "state": "active"}]},
            "templates": [{"name": "merge", "prompt": "Merge these: {{input}}", "connection": conn.id, "model": "m"}],
            "views": [{"name": "people", "kind": "table", "source": {"collection": "relationships"}}]
        },
        "operations": [
            {"name": "add", "description": "Someone to stay in touch with", "input": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}},
             "write": {"collection": "relationships", "fields": {"name": "$.name", "weeks": 6, "state": "active"}}},
            {"name": "set_cadence", "input": {"type": "object", "required": ["relationship", "weeks"], "properties": {
                "relationship": {"type": "string"}, "weeks": {"type": "integer", "minimum": 1, "maximum": 52}}},
             "write": {"collection": "relationships", "id": "$.relationship", "fields": {"weeks": "$.weeks"}, "when": {"state": "active"}}},
            {"name": "drop", "input": {"type": "object", "required": ["relationship"], "properties": {"relationship": {"type": "string"}}},
             "write": {"collection": "relationships", "id": "$.relationship", "delete": true}},
            {"name": "merge_people", "input": {"type": "object", "properties": {"keep": {"type": "string"}, "merge": {"type": "string"}}}, "template": "merge"}
        ]
    });
    ok(send(M::POST, "packages", json!({"manifest": crm})).await).await;
    let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "crm"})).await).await).unwrap();
    ok(send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await).await;
    let ops = ok(send(M::GET, "apps/home/crm/operations", json!({})).await).await;
    assert_eq!(ops.as_array().unwrap().len(), 4);
    let q = |name: &'static str| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let v: Value = http
                .post(format!("{api}/collections/relationships/query"))
                .json(&json!({"where": {"name": name}}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["records"][0].clone()
        }
    };

    // You: a write, checked against the input's schema and the record's state.
    let added = ok(send(M::POST, "apps/home/crm/operations/add", json!({"input": {"name": "Grace"}})).await).await;
    assert_eq!(added["record"]["fields"], json!({"name": "Grace", "weeks": 6, "state": "active"}));
    assert_eq!(added["record"]["created_by"], "app:crm op:add you");
    let ada = q("Ada").await;
    let r = send(M::POST, "apps/home/crm/operations/set_cadence", json!({"input": {"relationship": ada["id"], "weeks": 0}})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("the input does not match"));
    let set =
        ok(send(M::POST, "apps/home/crm/operations/set_cadence", json!({"input": {"relationship": ada["id"], "weeks": 4}})).await).await;
    assert_eq!(set["record"]["fields"], json!({"name": "Ada", "weeks": 4, "state": "active"}), "only its fields change");
    // Not in a state it applies to.
    ok(send(M::POST, "collections/relationships/records?force=true", json!({"fields": {"name": "Linus", "state": "paused"}})).await).await;
    let d = ok(send(M::GET, "decisions", json!({})).await).await;
    let force = d.as_array().unwrap().iter().find(|d| d["title"].as_str().unwrap().starts_with("Force")).unwrap()["id"].as_i64().unwrap();
    ok(send(M::POST, &format!("decisions/{force}/answer"), json!({"option": "Write it"})).await).await;
    let linus = q("Linus").await;
    let r = send(M::POST, "apps/home/crm/operations/set_cadence", json!({"input": {"relationship": linus["id"], "weeks": 2}})).await;
    assert!(r.text().await.unwrap().contains("not in a state this operation applies to"));
    ok(send(M::POST, "apps/home/crm/operations/drop", json!({"input": {"relationship": linus["id"]}})).await).await;
    assert!(q("Linus").await.is_null());

    // A view's action calls the owner's operation, the record mapped in.
    let mut views = ok(send(M::GET, "settings/views", json!({})).await).await;
    views.as_array_mut().unwrap().push(json!({"name": "mine", "kind": "table", "source": {"collection": "relationships"},
        "actions": [{"label": "Monthly", "operation": "set_cadence", "input": {"relationship": "$.id", "weeks": 4}}]}));
    ok(send(M::PUT, "settings/views", views).await).await;
    let grace = q("Grace").await;
    let v = ok(send(M::POST, "views/mine/act", json!({"action": "Monthly", "record": grace["id"]})).await).await;
    assert_eq!(v["record"]["fields"]["weeks"], 4, "{v}");

    // A trigger's operation action.
    ok(send(M::POST, "collections", json!({"name": "inbox", "schema": {"type": "object", "properties": {"who": {"type": "string"}}}}))
        .await)
    .await;
    ok(send(
        M::POST,
        "triggers",
        json!({"name": "new person", "trigger": "record", "event_params": {"collection": "inbox", "ops": ["created"]},
        "action": "operation", "action_params": {"instance": "crm", "operation": "add", "input": {"name": "$.record.fields.who"}}}),
    )
    .await)
    .await;
    ok(send(M::POST, "collections/inbox/records", json!({"fields": {"who": "Katherine"}})).await).await;
    let k = eventually("the trigger's operation", async || Some(q("Katherine").await).filter(|r| !r.is_null())).await;
    assert!(k["created_by"].as_str().unwrap().starts_with("app:crm op:add trigger:"), "acts as the app, for the trigger: {k}");
    let r = send(
        M::POST,
        "triggers",
        json!({"name": "bad", "trigger": "record", "event_params": {"collection": "inbox", "ops": ["created"]},
        "action": "operation", "action_params": {"instance": "crm", "operation": "nope"}}),
    )
    .await;
    assert!(r.text().await.unwrap().contains("has no operation"));

    // An agent: lists what there is to call, then calls; a template
    // operation starts the app's session.
    mock.push(Reply::ToolCalls(vec![
        ("app_call".into(), "{}".into()),
        ("app_call".into(), json!({"instance": "crm", "operation": "add", "input": {"name": "Barbara"}}).to_string()),
        ("app_call".into(), json!({"instance": "crm", "operation": "merge_people", "input": {"keep": "a", "merge": "b"}}).to_string()),
    ]));
    mock.push(Reply::Text("done".into()));
    mock.push(Reply::Text("merged".into()));
    let s = ok(send(
        M::POST,
        "sessions",
        json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "add Barbara"}),
    )
    .await)
    .await;
    let sid = s["id"].as_str().unwrap().to_owned();
    eventually("the agent's calls", async || (mock.requests().len() >= 2).then_some(())).await;
    let seen = mock.requests()[1]["messages"].to_string();
    assert!(seen.contains("set_cadence") && seen.contains("merge_people"), "it lists the operations: {seen}");
    let b = q("Barbara").await;
    assert_eq!(b["created_by"], format!("app:crm op:add session:{sid}"));
    let merged = eventually("the merge session", async || {
        let all: Vec<c6_api::Session> = http.get(format!("{api}/sessions")).send().await.ok()?.json().await.ok()?;
        all.into_iter().find(|s| s.labels.contains(&"op:crm.merge_people".to_owned()))
    })
    .await;
    assert_eq!(merged.instance, "crm");

    // Another realm calls only through a grant.
    ok(send(M::POST, "realms", json!({"id": "lab"})).await).await;
    mock.push(Reply::ToolCalls(vec![(
        "app_call".into(),
        json!({"realm": "home", "instance": "crm", "operation": "add", "input": {"name": "Eve"}}).to_string(),
    )]));
    mock.push(Reply::Text("no".into()));
    ok(send(
        M::POST,
        "sessions",
        json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "add Eve", "realm": "lab"}),
    )
    .await)
    .await;
    let n = mock.requests().len();
    eventually("the lab call", async || (mock.requests().len() > n).then_some(())).await;
    eventually("the refusal", async || {
        mock.requests().iter().any(|r| r["messages"].to_string().contains("may not call home's crm.add")).then_some(())
    })
    .await;
    ok(send(
        M::POST,
        "grants",
        json!({"from_realm": "home", "to_realm": "lab", "kind": "operations", "mode": "call", "scope": {"methods": ["op:crm.add"]}}),
    )
    .await)
    .await;
    mock.push(Reply::ToolCalls(vec![(
        "app_call".into(),
        json!({"realm": "home", "instance": "crm", "operation": "add", "input": {"name": "Eve"}}).to_string(),
    )]));
    mock.push(Reply::Text("yes".into()));
    ok(send(
        M::POST,
        "sessions",
        json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "add Eve", "realm": "lab"}),
    )
    .await)
    .await;
    eventually("Eve, through the grant", async || Some(q("Eve").await).filter(|r| !r.is_null())).await;

    stop.cancel();
    head.shutdown().await.unwrap();
}
