//! Data that explains itself (docs/apps.md §8, §12): a collection's data
//! card (filled with its app's version and operations), shown to an agent
//! the first time it reads the collection; internal collections marked;
//! every record's provenance and what it was derived from; and a breaking
//! change to what an app publishes needs a new major version, its upgrade
//! naming what in the realm depends on it.

use c6_api::{Collection, Connection, Decision, Install};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn cards_provenance_and_contracts() {
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
    let decide = |id: i64, option: &'static str| async move {
        let r = send(M::POST, &format!("decisions/{id}/answer"), json!({"option": option})).await;
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
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

    let v1 = json!({
        "package": "crm", "version": "1.0.0",
        "contents": {
            "collections": [
                {"name": "relationships", "description": "People to stay in touch with",
                 "schema": {"type": "object", "required": ["name"], "properties": {
                     "name": {"type": "string", "description": "Their name"}, "weeks": {"type": "integer", "description": "Weeks between check-ins"}}},
                 "card": {"sources": "Mail with people in the CRM, and what you add", "invariants": ["weeks is at least 1"], "freshness": "on each mail"}},
                {"name": "scores", "schema": {"type": "object", "properties": {"score": {"type": "number"}}}, "card": {"stability": "internal"}}
            ],
            "records": {"scores": [{"score": 1.5}]}
        },
        "operations": [{"name": "add", "input": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}, "email": {"type": "string"}}},
                        "write": {"collection": "relationships", "fields": {"name": "$.name", "weeks": 6}, "derived_from": ["item:{{$.email}}"]}}]
    });
    ok(send(M::POST, "packages", json!({"manifest": v1})).await).await;
    let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "crm"})).await).await).unwrap();
    decide(i.decision.unwrap(), "Install").await;

    // The card, with what the head knows.
    let c: Collection = serde_json::from_value(ok(send(M::GET, "collections/relationships", json!({})).await).await).unwrap();
    assert_eq!((c.owner.as_str(), c.card.version.as_str(), c.card.operations.clone()), ("crm", "1.0.0", vec!["add".to_owned()]));
    assert_eq!(c.card.invariants, vec!["weeks is at least 1".to_owned()]);
    // Internal: readable, and marked.
    let page = ok(send(M::GET, "collections/scores/records", json!({})).await).await;
    assert_eq!(page["total"], 1);
    assert!(page["note"].as_str().unwrap().contains("internal to crm: may change without notice"), "{page}");
    // A realm's own collection: you describe it.
    ok(send(M::POST, "collections", json!({"name": "leads", "schema": {"type": "object", "properties": {"who": {"type": "string"}}}}))
        .await)
    .await;
    let c = ok(send(M::PATCH, "collections/leads", json!({"card": {"sources": "Typed in at events", "freshness": "weekly"}})).await).await;
    assert_eq!(c["card"]["sources"], "Typed in at events");
    assert_eq!(send(M::PATCH, "collections/leads", json!({"card": {"stability": "secret"}})).await.status(), 400);

    // Provenance: an operation's record says which app version wrote it, and
    // from what.
    let r = ok(send(M::POST, "apps/home/crm/operations/add", json!({"input": {"name": "Ada", "email": "e123"}})).await).await;
    assert_eq!(r["record"]["derived_from"], json!(["item:e123"]));
    assert_eq!(r["record"]["provenance"]["app"], "crm");
    assert_eq!(r["record"]["provenance"]["version"], "1.0.0");
    assert_eq!(r["record"]["provenance"]["operation"], "crm.add");

    // An agent: the card comes the first time it reads a collection; an
    // internal one carries the note; its own record names its evidence.
    mock.push(Reply::ToolCalls(vec![
        ("records_query".into(), r#"{"collection":"relationships"}"#.into()),
        ("records_query".into(), r#"{"collection":"relationships"}"#.into()),
        ("records_query".into(), r#"{"collection":"scores"}"#.into()),
        (
            "records_put".into(),
            r#"{"collection":"leads","fields":{"who":"Grace"},"derived_from":["item:mail-9","record:relationships/x"]}"#.into(),
        ),
        ("records_put".into(), r#"{"collection":"leads","fields":{"who":"Bad"},"derived_from":["mail-9"]}"#.into()),
    ]));
    mock.push(Reply::Text("done".into()));
    let s =
        ok(send(M::POST, "sessions", json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "look"})).await)
            .await;
    eventually("the agent's calls", async || (mock.requests().len() >= 2).then_some(())).await;
    let seen = mock.requests()[1]["messages"].to_string();
    assert_eq!(seen.matches("What stays true: weeks is at least 1").count(), 1, "the card once: {seen}");
    assert!(seen.contains("Where it comes from: Mail with people in the CRM"), "{seen}");
    assert!(seen.contains("Weeks between check-ins"), "field meanings: {seen}");
    assert!(seen.contains("internal to crm: may change without notice"), "{seen}");
    assert!(seen.contains("derived_from names items"), "a bad reference is refused: {seen}");
    let leads = ok(send(M::POST, "collections/leads/query", json!({})).await).await;
    let grace = &leads["records"][0];
    assert_eq!(grace["derived_from"], json!(["item:mail-9", "record:relationships/x"]));
    assert_eq!(grace["provenance"]["session"], s["id"]);

    // What publishes is a contract: narrowing it in a minor version is
    // refused; in a new major version it goes ahead, naming what uses it.
    let mut views = ok(send(M::GET, "settings/views", json!({})).await).await;
    views.as_array_mut().unwrap().push(json!({"name": "people", "kind": "table", "source": {"collection": "relationships"}}));
    ok(send(M::PUT, "settings/views", views).await).await;
    let mut v11 = v1.clone();
    v11["version"] = json!("1.1.0");
    v11["contents"]["collections"][0]["schema"]["properties"].as_object_mut().unwrap().remove("weeks");
    ok(send(M::POST, "packages", json!({"manifest": v11})).await).await;
    let r = send(M::POST, &format!("installs/{}/upgrade", i.id), json!({"version": "1.1.0"})).await;
    let text = r.text().await.unwrap();
    assert!(text.contains("field weeks is gone") && text.contains("needs a new major version"), "{text}");
    // An internal collection may change in a minor version; a new optional field is fine.
    let mut v12 = v1.clone();
    v12["version"] = json!("1.2.0");
    v12["contents"]["collections"][1]["schema"]["properties"] = json!({"score": {"type": "number"}, "why": {"type": "string"}});
    v12["contents"]["collections"][1]["schema"]["required"] = json!(["score"]);
    v12["contents"]["collections"][0]["schema"]["properties"]["notes"] = json!({"type": "string"});
    ok(send(M::POST, "packages", json!({"manifest": v12})).await).await;
    let up: Install =
        serde_json::from_value(ok(send(M::POST, &format!("installs/{}/upgrade", i.id), json!({"version": "1.2.0"})).await).await).unwrap();
    let d: Decision = send(M::GET, &format!("decisions/{}", up.decision.unwrap()), json!({})).await.json().await.unwrap();
    assert!(d.document.contains("What in home uses what it publishes: view people"), "{}", d.document);
    decide(d.id, "Upgrade").await;
    let mut v2 = v11.clone();
    v2["version"] = json!("2.0.0");
    ok(send(M::POST, "packages", json!({"manifest": v2})).await).await;
    let r = send(M::POST, &format!("installs/{}/upgrade", i.id), json!({"version": "2.0.0"})).await;
    let text = r.text().await.unwrap();
    assert!(!text.contains("needs a new major version"), "{text}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
