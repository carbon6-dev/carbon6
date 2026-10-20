//! Grants (docs/os.md §3.7): a realm reaches another's collection only
//! through a grant, for exactly what it allows (read, some records, add,
//! a copy once); every use is logged; revoking ends it; an agent's request
//! for one is a decision whose answer it hears.

use c6_api::{Connection, Decision, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

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
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
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
    // The answer starts a turn of its own once the asking turn has ended;
    // answered sooner, it would reach the model inside that turn instead.
    eventually("the asking turn's end", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && mock.requests().len() >= 2).then_some(())
    })
    .await;
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

/// Commons (docs/os.md §3.3): every realm reads it unless turned off;
/// realms publish into it only with an append grant, and each record
/// waits for your review; sessions and tokens never work in it directly.
#[tokio::test]
async fn commons_is_shared_and_reviewed() {
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

    // Commons is there, read by every realm; nothing runs in it.
    let realms: Value = send(M::GET, "realms", json!({})).await.json().await.unwrap();
    let ids: Vec<&str> = realms.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["root", "home", "commons"]);
    assert!(realms[1]["reads_commons"].as_bool().unwrap());
    assert_eq!(send(M::DELETE, "realms/commons", json!({})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(send(M::POST, "auth/tokens", json!({"name": "c", "realm": "commons"})).await.status(), StatusCode::BAD_REQUEST);
    let r = send(
        M::POST,
        "sessions",
        json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "x", "realm": "commons"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = send(
        M::POST,
        "grants",
        json!({"from_realm": "home", "to_realm": "commons", "kind": "collection", "mode": "read", "scope": {"collection": "x"}}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // You keep a reference collection there; a realm's token reads it.
    let schema =
        json!({"type": "object", "required": ["host", "role"], "properties": {"host": {"type": "string"}, "role": {"type": "string"}}});
    assert!(send(M::POST, "collections?realm=commons", json!({"name": "machines", "schema": schema})).await.status().is_success());
    assert!(
        send(M::POST, "collections/machines/records?realm=commons", json!({"fields": {"host": "db1", "role": "postgres"}}))
            .await
            .status()
            .is_success()
    );
    assert!(send(M::POST, "realms", json!({"id": "factory"})).await.status().is_success());
    let t: Value = send(M::POST, "auth/tokens", json!({"name": "f", "realm": "factory"})).await.json().await.unwrap();
    let tok = t["token"].as_str().unwrap().to_owned();
    let as_f = |method: M, path: &str, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}")).bearer_auth(&tok);
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
    };
    let page: Value = as_f(M::GET, "collections/machines/records?realm=commons", None).await.unwrap().json().await.unwrap();
    assert_eq!(page["total"], 1, "{page}");
    // Turned off for the realm: gone.
    let r = send(M::PATCH, "realms/factory", json!({"reads_commons": false})).await;
    assert!(r.status().is_success());
    assert_eq!(as_f(M::GET, "collections/machines/records?realm=commons", None).await.unwrap().status(), StatusCode::FORBIDDEN);
    send(M::PATCH, "realms/factory", json!({"reads_commons": true})).await;

    // Publishing needs an append grant; then each record is a proposal.
    let new = json!({"fields": {"host": "ci1", "role": "buildkite agent"}});
    assert_eq!(
        as_f(M::POST, "collections/machines/records?realm=commons", Some(new.clone())).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let r = send(M::POST, "grants", json!({"from_realm": "commons", "to_realm": "factory", "kind": "collection", "mode": "append", "scope": {"collection": "machines"}})).await;
    assert!(r.status().is_success());
    let r = as_f(M::POST, "collections/machines/records?realm=commons", Some(json!({"fields": {"host": "x", "extra": 1}}))).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "checked against the schema up front");
    let r = as_f(M::POST, "collections/machines/records?realm=commons", Some(new)).await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let proposed: Value = r.json().await.unwrap();
    let page: Value = send(M::GET, "collections/machines/records?realm=commons", json!({})).await.json().await.unwrap();
    assert_eq!(page["total"], 1, "nothing until you review it");
    let d: Decision = send(M::GET, &format!("decisions/{}", proposed["decision"]), json!({})).await.json().await.unwrap();
    assert!(d.title.contains("from realm factory") && d.document.contains("buildkite agent"), "{d:?}");
    assert_eq!(d.realm, "commons");
    assert!(send(M::POST, &format!("decisions/{}/answer", d.id), json!({"option": "Publish"})).await.status().is_success());
    let page: Value =
        send(M::POST, "collections/machines/query?realm=commons", json!({"where": {"host": "ci1"}})).await.json().await.unwrap();
    let rec = &page["records"][0];
    assert!(rec["created_by"].as_str().unwrap().contains("published from realm factory, reviewed by you"), "{rec}");
    assert!(rec["labels"].as_array().unwrap().contains(&json!("from:factory")), "{rec}");

    // An agent proposes one too; rejected, nothing is published.
    mock.push(Reply::ToolCalls(vec![(
        "commons_publish".into(),
        json!({"collection": "machines", "fields": {"host": "web9", "role": "nginx"}}).to_string(),
    )]));
    mock.push(Reply::Text("Proposed.".into()));
    send(
        M::POST,
        "sessions",
        json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "share hosts", "realm": "factory"}),
    )
    .await;
    let d = eventually("the agent's proposal", async || {
        let open: Vec<Decision> = http.get(format!("{api}/decisions?realm=commons")).send().await.ok()?.json().await.ok()?;
        open.into_iter().find(|d| d.document.contains("web9"))
    })
    .await;
    assert!(d.source.starts_with("session:"), "{d:?}");
    assert!(send(M::POST, &format!("decisions/{}/answer", d.id), json!({"option": "Reject"})).await.status().is_success());
    let page: Value = send(M::GET, "collections/machines/records?realm=commons", json!({})).await.json().await.unwrap();
    assert_eq!(page["total"], 2);

    stop.cancel();
    head.shutdown().await.unwrap();
}
