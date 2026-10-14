//! Grants (docs/os.md §3.7): a realm reaches another's collection only
//! through a grant, for exactly what it allows (read, some records, add,
//! a copy once); every use is logged; revoking ends it; an agent's request
//! for one is a decision whose answer it hears.

use std::time::Duration;

use c6_api::{Connection, Decision, Session};
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
async fn grants_let_data_cross_realms() {
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
    let wid = worker.identity.meta.worker_id.0;
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
    let json_of = async |r: reqwest::Response| -> (StatusCode, Value) {
        let s = r.status();
        (s, r.json().await.unwrap_or(Value::Null))
    };

    // Two realms: crm keeps clients; factory would like to use them.
    for id in ["crm", "factory"] {
        assert!(send(M::POST, "realms", json!({"id": id})).await.status().is_success());
    }
    let schema =
        json!({"type": "object", "required": ["name", "tier"], "properties": {"name": {"type": "string"}, "tier": {"type": "string"}}});
    assert!(send(M::POST, "collections?realm=crm", json!({"name": "clients", "schema": schema})).await.status().is_success());
    for (name, tier) in [("Acme", "gold"), ("Globex", "gold"), ("Initech", "basic")] {
        let r = send(M::POST, "collections/clients/records?realm=crm", json!({"fields": {"name": name, "tier": tier}})).await;
        assert!(r.status().is_success());
    }
    let t: Value = send(M::POST, "auth/tokens", json!({"name": "f", "realm": "factory"})).await.json().await.unwrap();
    let factory = t["token"].as_str().unwrap().to_owned();
    let as_f = |method: M, path: &str, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}")).bearer_auth(&factory);
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
    };

    // Without a grant: nothing crosses; and only you manage grants.
    let r = as_f(M::GET, "collections/clients/records?realm=crm", None).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(as_f(M::GET, "grants", None).await.unwrap().status(), StatusCode::FORBIDDEN);

    // Grants are checked.
    for bad in [
        json!({"from_realm": "crm", "to_realm": "crm", "kind": "collection", "mode": "read", "scope": {"collection": "clients"}}),
        json!({"from_realm": "root", "to_realm": "factory", "kind": "collection", "mode": "read", "scope": {"collection": "clients"}}),
        json!({"from_realm": "crm", "to_realm": "factory", "kind": "collection", "mode": "call", "scope": {"collection": "clients"}}),
        json!({"from_realm": "crm", "to_realm": "factory", "kind": "collection", "mode": "read", "scope": {}}),
        json!({"from_realm": "crm", "to_realm": "factory", "kind": "operations", "mode": "call", "scope": {}}),
    ] {
        assert_eq!(send(M::POST, "grants", bad.clone()).await.status(), StatusCode::BAD_REQUEST, "{bad}");
    }

    // A read grant on the gold clients only.
    let (s, read) = json_of(
        send(
            M::POST,
            "grants",
            json!({"from_realm": "crm", "to_realm": "factory", "kind": "collection", "mode": "read",
                   "scope": {"collection": "clients", "where": {"tier": "gold"}}, "note": "pitch upsells"}),
        )
        .await,
    )
    .await;
    assert!(s.is_success(), "{read}");
    assert_eq!(read["state"], "active");
    let page: Value = as_f(M::GET, "collections/clients/records?realm=crm", None).await.unwrap().json().await.unwrap();
    let mut names: Vec<&str> = page["records"].as_array().unwrap().iter().map(|r| r["fields"]["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, vec!["Acme", "Globex"]);
    // A query can't widen it.
    let page: Value = as_f(M::POST, "collections/clients/query?realm=crm", Some(json!({"where": {"tier": "basic"}})))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["records"].as_array().unwrap().len(), 0, "{page}");
    let all: Value = send(M::GET, "collections/clients/records?realm=crm", json!({})).await.json().await.unwrap();
    let initech =
        all["records"].as_array().unwrap().iter().find(|r| r["fields"]["name"] == "Initech").unwrap()["id"].as_str().unwrap().to_owned();
    let r = as_f(M::GET, &format!("collections/clients/records/{initech}?realm=crm"), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // Reading isn't adding.
    let r =
        as_f(M::POST, "collections/clients/records?realm=crm", Some(json!({"fields": {"name": "Hooli", "tier": "gold"}}))).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // An append grant: factory adds leads; the record says how it came.
    let append: Value = send(
        M::POST,
        "grants",
        json!({"from_realm": "crm", "to_realm": "factory", "kind": "collection", "mode": "append", "scope": {"collection": "clients"}}),
    )
    .await
    .json()
    .await
    .unwrap();
    let r =
        as_f(M::POST, "collections/clients/records?realm=crm", Some(json!({"fields": {"name": "Hooli", "tier": "basic"}}))).await.unwrap();
    let (s, hooli) = json_of(r).await;
    assert!(s.is_success(), "{hooli}");
    assert_eq!(
        (hooli["realm"].as_str(), hooli["created_by"].as_str()),
        (Some("crm"), Some(&*format!("token:factory grant:{}", append["id"].as_str().unwrap())))
    );
    // Changing others' records needs more than append.
    let r = as_f(M::DELETE, &format!("collections/clients/records/{}?realm=crm", hooli["id"].as_str().unwrap()), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // A snapshot: a copy now, each record saying where it came from.
    let snap: Value = send(
        M::POST,
        "grants",
        json!({"from_realm": "crm", "to_realm": "factory", "kind": "collection", "mode": "snapshot",
               "scope": {"collection": "clients", "where": {"tier": "basic"}}}),
    )
    .await
    .json()
    .await
    .unwrap();
    let copies: Value = as_f(M::GET, "collections/clients/records", None).await.unwrap().json().await.unwrap();
    let copies = copies["records"].as_array().unwrap();
    assert_eq!(copies.len(), 2, "Initech and Hooli: {copies:?}");
    let c = &copies[0];
    assert_eq!(c["realm"], "factory");
    assert!(c["labels"].as_array().unwrap().contains(&json!("from:crm")), "{c}");
    assert!(c["labels"].as_array().unwrap().contains(&json!(format!("grant:{}", snap["id"].as_str().unwrap()))), "{c}");
    assert!(c["links"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().starts_with("record:crm/")), "{c}");

    // Uses are logged; revoking ends the grant.
    let g: Value = send(M::GET, &format!("grants/{}", read["id"].as_str().unwrap()), json!({})).await.json().await.unwrap();
    assert!(g["uses"].as_i64().unwrap() >= 3, "{g}");
    let uses: Value = send(M::GET, &format!("grants/{}/uses", read["id"].as_str().unwrap()), json!({})).await.json().await.unwrap();
    assert_eq!(uses[0]["by"], "token:factory");
    let listed: Value = send(M::GET, "grants?realm=crm", json!({})).await.json().await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 3);
    assert_eq!(send(M::DELETE, &format!("grants/{}", read["id"].as_str().unwrap()), json!({})).await.status(), StatusCode::NO_CONTENT);
    let r = as_f(M::GET, "collections/clients/records?realm=crm", None).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // An agent in factory asks; you allow it as a decision; it hears the
    // answer and reads through the grant.
    mock.push(Reply::ToolCalls(vec![(
        "grant_request".into(),
        json!({"from_realm": "crm", "kind": "collection", "mode": "read", "scope": {"collection": "clients"}, "why": "to find upsells"})
            .to_string(),
    )]));
    mock.push(Reply::Text("Asked for the clients.".into()));
    mock.push(Reply::ToolCalls(vec![("records_query".into(), json!({"collection": "clients", "realm": "crm"}).to_string())]));
    mock.push(Reply::Text("Read them.".into()));
    let s: Session = send(
        M::POST,
        "sessions",
        json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "find upsells", "realm": "factory"}),
    )
    .await
    .json()
    .await
    .unwrap();
    let d = eventually("the grant decision", async || {
        let open: Vec<Decision> = http.get(format!("{api}/decisions?realm=factory")).send().await.ok()?.json().await.ok()?;
        open.into_iter().find(|d| d.title.contains("collection clients"))
    })
    .await;
    assert!(d.document.contains("to find upsells") && d.source == format!("session:{}", s.id), "{d:?}");
    let pending: Value = send(M::GET, "grants?realm=factory", json!({})).await.json().await.unwrap();
    assert!(pending.as_array().unwrap().iter().any(|g| g["state"] == "requested"), "{pending}");
    let r = send(M::POST, &format!("decisions/{}/answer", d.id), json!({"option": "Allow"})).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let seen = eventually("the agent read through the grant", async || {
        let reqs = mock.requests();
        let last = reqs.last()?["messages"].to_string();
        (last.contains("Your grant request was allowed") && last.contains("Initech")).then_some(last)
    })
    .await;
    assert!(seen.contains("Acme"), "{seen}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
