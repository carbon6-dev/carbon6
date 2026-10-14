//! Collections (docs/os.md §4.1): records match their schema strictly,
//! keys stay unique, updates give the version they read, every change is
//! kept, queries filter, sort and count, schema changes must fit the
//! records already there, agents use them through tools, and each realm
//! has its own.

use std::time::Duration;

use c6_api::{Connection, Record, RecordPage, Session};
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
async fn collections_and_records() {
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
    let send = |method: reqwest::Method, path: &str, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}"));
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
    };
    use reqwest::Method as M;
    async fn body(r: reqwest::Response) -> (StatusCode, Value) {
        let s = r.status();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    // A collection.
    let schema = json!({"type": "object", "required": ["name", "stage"], "properties": {
        "name": {"type": "string", "minLength": 1},
        "stage": {"enum": ["found", "approved", "build"]},
        "score": {"type": "number", "minimum": 0, "maximum": 10},
        "tags": {"type": "array", "items": {"type": "string"}}
    }});
    let (s, v) = body(send(M::POST, "collections", Some(json!({"name": "bad name", "schema": schema}))).await.unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = body(
        send(
            M::POST,
            "collections",
            Some(json!({"name": "candidates", "description": "Gaps worth a look", "schema": schema, "keys": ["name"]})),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(s.is_success(), "{v}");
    assert_eq!((v["realm"].as_str(), v["records"].as_i64()), (Some("home"), Some(0)));
    let (s, _) = body(send(M::POST, "collections", Some(json!({"name": "candidates", "schema": schema}))).await.unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "names are unique in a realm");

    // Records: strict, unique by key.
    let add =
        |fields: Value| send(M::POST, "collections/candidates/records", Some(json!({"fields": fields, "links": ["ecosystem:shopify"]})));
    let (s, a) = body(add(json!({"name": "csv sync", "stage": "found", "score": 7, "tags": ["shopify"]})).await.unwrap()).await;
    assert!(s.is_success(), "{a}");
    let a: Record = serde_json::from_value(a).unwrap();
    assert_eq!((a.version, a.created_by.as_str()), (1, "you"));
    for (bad, why) in [
        (json!({"name": "x", "stage": "found", "owner": "me"}), "owner: unknown field"),
        (json!({"name": "x", "stage": "shipped"}), "stage: must be one of"),
        (json!({"name": "x"}), "stage is required"),
        (json!({"name": "x", "stage": "found", "score": 12}), "score: 12 is outside the allowed range"),
        (json!({"name": "csv sync", "stage": "approved"}), "name \"csv sync\" is already used"),
    ] {
        let (s, v) = body(add(bad).await.unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains(why), "{why}: {v}");
    }
    let (_, b) =
        body(add(json!({"name": "pdf invoices", "stage": "approved", "score": 9, "tags": ["stripe", "pdf"]})).await.unwrap()).await;
    let (_, c) = body(add(json!({"name": "slack digest", "stage": "found", "score": 4})).await.unwrap()).await;

    // Updates give the version they read; history keeps every change.
    let url = format!("collections/candidates/records/{}", a.id);
    let (s, v) =
        body(send(M::PUT, &url, Some(json!({"fields": {"name": "csv sync", "stage": "approved", "score": 8}}))).await.unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "an update needs the version: {v}");
    let (s, v) = body(
        send(M::PUT, &url, Some(json!({"fields": {"name": "csv sync", "stage": "approved", "score": 8}, "version": 1}))).await.unwrap(),
    )
    .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["version"], 2);
    assert_eq!(v["links"], json!(["ecosystem:shopify"]), "links stay unless given");
    let (s, v) =
        body(send(M::PUT, &url, Some(json!({"fields": {"name": "csv sync", "stage": "build"}, "version": 1}))).await.unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("changed since you read it"), "{v}");
    let (_, h) = body(send(M::GET, &format!("{url}/history"), None).await.unwrap()).await;
    let ops: Vec<&str> = h.as_array().unwrap().iter().map(|c| c["op"].as_str().unwrap()).collect();
    assert_eq!(ops, vec!["create", "update"]);

    // Queries.
    let q = |query: Value| {
        let r = send(M::POST, "collections/candidates/query", Some(query));
        async move { serde_json::from_value::<RecordPage>(r.await.unwrap().json().await.unwrap()).unwrap() }
    };
    let names = |p: &RecordPage| p.records.iter().map(|r| r.fields["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(names(&q(json!({"where": {"stage": "approved"}, "sort": "name"})).await), vec!["csv sync", "pdf invoices"]);
    assert_eq!(names(&q(json!({"where": {"score": {"$gte": 8}}, "sort": "-score"})).await), vec!["pdf invoices", "csv sync"]);
    assert_eq!(names(&q(json!({"where": {"tags": {"$contains": "pdf"}}})).await), vec!["pdf invoices"]);
    assert_eq!(names(&q(json!({"where": {"stage": {"$in": ["found", "build"]}}})).await), vec!["slack digest"]);
    assert_eq!(names(&q(json!({"text": "digest"})).await), vec!["slack digest"]);
    let page = q(json!({"sort": "name", "limit": 1, "offset": 1})).await;
    assert_eq!((names(&page), page.total), (vec!["pdf invoices".to_owned()], 3));
    let groups = q(json!({"group_by": "stage"})).await.groups;
    assert_eq!(groups, vec![(json!("approved"), 2), (json!("found"), 1)]);
    let (s, _) = body(send(M::POST, "collections/candidates/query", Some(json!({"where": {"x') OR 1=1 --": 1}}))).await.unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "field names are checked");

    // Schema changes must fit the records already there.
    let mut wider = schema.clone();
    wider["properties"]["notes"] = json!({"type": "string"});
    let (s, v) = body(send(M::PATCH, "collections/candidates", Some(json!({"schema": wider}))).await.unwrap()).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["schema_version"], 2);
    let mut stricter = wider.clone();
    stricter["required"] = json!(["name", "stage", "notes"]);
    let (s, v) = body(send(M::PATCH, "collections/candidates", Some(json!({"schema": stricter}))).await.unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("rejects 3 existing record(s), so it needs a migration"), "{v}");

    // An agent adds a record and finds it, in its session's realm.
    mock.push(Reply::ToolCalls(vec![
        ("records_query".into(), "{}".into()),
        ("records_put".into(), r#"{"collection":"candidates","fields":{"name":"jira export","stage":"found","score":6}}"#.into()),
        ("records_put".into(), r#"{"collection":"candidates","fields":{"name":"jira export","stage":"nope"}}"#.into()),
    ]));
    mock.push(Reply::ToolCalls(vec![(
        "records_query".into(),
        r#"{"collection":"candidates","query":{"where":{"score":{"$lt":7}},"sort":"score"}}"#.into(),
    )]));
    mock.push(Reply::Text("done".into()));
    let session: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "track a gap"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("the agent's three calls", async || (mock.requests().len() >= 3).then_some(())).await;
    let reqs = mock.requests();
    let first = reqs[1]["messages"].to_string();
    assert!(first.contains("Gaps worth a look") && first.contains("candidates"), "lists collections: {first}");
    assert!(first.contains("stage: must be one of"), "a bad record is refused: {first}");
    let second = reqs[2]["messages"].to_string();
    assert!(second.contains("slack digest") && second.contains("jira export"), "{second}");
    let mine = q(json!({"where": {"name": "jira export"}})).await;
    assert_eq!(mine.records[0].created_by, format!("session:{}", session.id));

    // Each realm has its own collections: a lab session sees none of home's.
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(head_dir.path().join("c6h.db")))
        .await
        .unwrap();
    sqlx::query("INSERT INTO realms (id, name, created_at, created_by) VALUES ('lab', 'Lab', 0, 'you')").execute(&db).await.unwrap();
    let (s, v) = body(
        send(
            M::POST,
            "collections?realm=lab",
            Some(json!({"name": "experiments", "schema": {"type": "object", "properties": {"x": {"type": "string"}}}})),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(s.is_success(), "{v}");
    mock.push(Reply::ToolCalls(vec![
        ("records_query".into(), "{}".into()),
        ("records_query".into(), r#"{"collection":"candidates"}"#.into()),
    ]));
    mock.push(Reply::Text("done".into()));
    http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "look", "realm": "lab"}))
        .send()
        .await
        .unwrap();
    eventually("the lab session's calls", async || (mock.requests().len() >= 5).then_some(())).await;
    let lab = mock.requests()[4]["messages"].to_string();
    assert!(lab.contains("experiments") && !lab.contains("Gaps worth a look"), "{lab}");
    assert!(lab.contains("no collection \\\"candidates\\\" in realm lab"), "{lab}");

    // A home token cannot reach lab's collections.
    let tok: Value =
        http.post(format!("{api}/auth/tokens")).json(&json!({"name": "t", "realm": "home"})).send().await.unwrap().json().await.unwrap();
    let tok = tok["token"].as_str().unwrap();
    let r = http.get(format!("{api}/collections?realm=lab")).bearer_auth(tok).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let mine: Value = http.get(format!("{api}/collections")).bearer_auth(tok).send().await.unwrap().json().await.unwrap();
    let names: Vec<&str> = mine.as_array().unwrap().iter().filter(|c| c["system"] == false).map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["candidates"]);
    // carbon6's own lists are collections too, written only through Settings.
    assert!(mine.as_array().unwrap().iter().any(|c| c["name"] == "skills" && c["system"] == true));
    let r = http
        .post(format!("{api}/collections/skills/records"))
        .json(&json!({"fields": {"name": "x", "description": "y", "body": "z"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // Deleting keeps the history; dropping removes the records.
    let (s, _) = body(send(M::DELETE, &format!("collections/candidates/records/{}", b["id"].as_str().unwrap()), None).await.unwrap()).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, h) =
        body(send(M::GET, &format!("collections/candidates/records/{}/history", b["id"].as_str().unwrap()), None).await.unwrap()).await;
    assert_eq!(h.as_array().unwrap().last().unwrap()["op"], "delete");
    let (s, _) = body(send(M::DELETE, "collections/candidates", None).await.unwrap()).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = body(send(M::GET, &format!("collections/candidates/records/{}", c["id"].as_str().unwrap()), None).await.unwrap()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    stop.cancel();
    head.shutdown().await.unwrap();
}

/// carbon6's lists are system collections (docs/os.md §12.2): a realm uses
/// root's entries and its own, its own replacing root's of the same name,
/// and root's policies apply first.
#[tokio::test]
async fn realms_inherit_lists_from_root() {
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
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(head_dir.path().join("c6h.db")))
        .await
        .unwrap();
    sqlx::query("INSERT INTO realms (id, name, created_at, created_by) VALUES ('lab', 'Lab', 0, 'you')").execute(&db).await.unwrap();
    // Settings write the list where it lives: skills in home, policies in root.
    let put = |path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { assert!(http.put(&url).json(&body).send().await.unwrap().status().is_success(), "{url}") }
    };
    put("settings/skills", json!([{"name": "greet", "description": "HOME-GREET", "body": "x"}])).await;
    put("settings/policies", json!([{"name": "no rm", "tools": "Bash", "pattern": "rm -rf", "action": "deny", "message": "ROOT-DENIES"}]))
        .await;
    let realm_of: Vec<(String, String)> =
        sqlx::query_as("SELECT collection, realm FROM records WHERE collection IN ('skills', 'policies') ORDER BY collection")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(realm_of, vec![("policies".into(), "root".into()), ("skills".into(), "home".into())]);
    // Root's own skills, and lab's version of greet.
    let add = |realm: &'static str, fields: Value| {
        let db = db.clone();
        async move {
            sqlx::query("INSERT INTO records (id, realm, collection, fields, version, created_by, updated_by, created_at, updated_at) VALUES (?, ?, 'skills', ?, 1, 'you', 'you', 0, 0)")
                .bind(uuid::Uuid::now_v7().to_string())
                .bind(realm)
                .bind(fields.to_string())
                .execute(&db)
                .await
                .unwrap();
        }
    };
    add("root", json!({"name": "greet", "description": "ROOT-GREET", "body": "x"})).await;
    add("root", json!({"name": "deploy", "description": "ROOT-DEPLOY", "body": "x"})).await;
    add("lab", json!({"name": "greet", "description": "LAB-GREET", "body": "x"})).await;
    // The settings endpoint still shows home's list, as it always did.
    let skills: Value = http.get(format!("{api}/settings/skills")).send().await.unwrap().json().await.unwrap();
    assert_eq!(skills.as_array().unwrap().len(), 1);

    let deny = r#"{"command":"rm -rf /tmp/nothing-here"}"#;
    for (realm, n) in [("home", 2), ("lab", 4)] {
        mock.push(Reply::ToolCalls(vec![("bash".into(), deny.into())]));
        mock.push(Reply::Text("done".into()));
        http.post(format!("{api}/sessions"))
            .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "hi", "realm": realm}))
            .send()
            .await
            .unwrap();
        eventually("the session's two calls", async || (mock.requests().len() >= n).then_some(())).await;
    }
    let reqs = mock.requests();
    let sys = |i: usize| reqs[i]["messages"][0]["content"].as_str().unwrap().to_owned();
    let (home, lab) = (sys(0), sys(2));
    assert!(
        home.contains("HOME-GREET") && home.contains("ROOT-DEPLOY") && !home.contains("ROOT-GREET") && !home.contains("LAB-GREET"),
        "{home}"
    );
    assert!(
        lab.contains("LAB-GREET") && lab.contains("ROOT-DEPLOY") && !lab.contains("ROOT-GREET") && !lab.contains("HOME-GREET"),
        "{lab}"
    );
    for i in [1, 3] {
        assert!(reqs[i]["messages"].to_string().contains("ROOT-DENIES"), "root's policy applies in every realm: {}", reqs[i]["messages"]);
    }
    stop.cancel();
    head.shutdown().await.unwrap();
}
