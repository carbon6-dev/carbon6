//! Realms (docs/os.md §3): a token limited to a realm reaches only that
//! realm's data and none of carbon6's administration; sessions run in a
//! realm (never root), and secrets and memory notes reach only sessions in
//! their own realm.

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn realms_keep_data_apart() {
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

    // Realms: root and home.
    let realms: Value = http.get(format!("{api}/realms")).send().await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = realms.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["root", "home", "commons"]);
    // A second realm, made directly (realms can't be created through the API yet).
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(head_dir.path().join("c6h.db")))
        .await
        .unwrap();
    sqlx::query("INSERT INTO realms (id, name, created_at, created_by) VALUES ('lab', 'Lab', 0, 'you')").execute(&db).await.unwrap();

    // Tokens: one for every realm, one limited to home.
    let token = |realm: Option<&str>| {
        let (http, api) = (http.clone(), api.clone());
        let realm = realm.map(str::to_owned);
        async move {
            let t: Value = http
                .post(format!("{api}/auth/tokens"))
                .json(&json!({"name": "t", "realm": realm}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            t["token"].as_str().unwrap().to_owned()
        }
    };
    let root = token(None).await;
    let home = token(Some("home")).await;
    for bad in ["nowhere", "root"] {
        let r = http.post(format!("{api}/auth/tokens")).json(&json!({"name": "t", "realm": bad})).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    let as_ = |tok: &str, method: reqwest::Method, path: &str, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}")).bearer_auth(tok);
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
    };
    use reqwest::Method as M;

    // The home token: shared infrastructure to read, no administration.
    assert!(as_(&home, M::GET, "workers", None).await.unwrap().status().is_success());
    assert!(as_(&home, M::GET, "connections", None).await.unwrap().status().is_success());
    let realms: Value = as_(&home, M::GET, "realms", None).await.unwrap().json().await.unwrap();
    assert_eq!(realms.as_array().unwrap().len(), 1, "it sees only its realm: {realms}");
    for (method, path) in [
        (M::POST, "workers/enrollments"),
        (M::GET, "settings/prices"),
        (M::PUT, "settings/routes"),
        (M::POST, "catalog/call"),
        (M::GET, "system/updates"),
    ] {
        let r = as_(&home, method.clone(), path, Some(json!({}))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert!(as_(&root, M::GET, "settings/skills", None).await.unwrap().status().is_success());
    // Its own usage, counted for its realm only.
    assert!(as_(&home, M::GET, "usage", None).await.unwrap().status().is_success());
    // Its own realm's settings lists are its to read and change.
    let skill = json!([{"name": "home-only", "description": "d", "body": "b"}]);
    assert!(as_(&home, M::PUT, "settings/skills", Some(skill)).await.unwrap().status().is_success());
    let lab_skills: Value = as_(&root, M::GET, "settings/skills?realm=lab", None).await.unwrap().json().await.unwrap();
    assert!(!lab_skills.to_string().contains("home-only"), "{lab_skills}");

    // Sessions: never in root, only in realms that exist; a scoped token's
    // land in its realm and cannot go elsewhere.
    let start = |tok: String, realm: Option<&str>, prompt: &str| {
        let body = json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": prompt, "realm": realm});
        let (http, api) = (http.clone(), api.clone());
        async move { http.post(format!("{api}/sessions")).bearer_auth(tok).json(&body).send().await.unwrap() }
    };
    assert_eq!(start(root.clone(), Some("root"), "x").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(start(root.clone(), Some("nowhere"), "x").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(start(home.clone(), Some("lab"), "x").await.status(), StatusCode::FORBIDDEN);

    // Secrets and notes: one each in home and in lab.
    let put = |path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { assert!(http.put(&url).json(&body).send().await.unwrap().status().is_success(), "{url}") }
    };
    put("settings/secrets/HOME_KEY", json!({"value": "home-secret-1"})).await;
    put("settings/secrets/LAB_KEY", json!({"value": "lab-secret-2"})).await;
    sqlx::query("UPDATE secrets SET realm = 'lab' WHERE name = 'LAB_KEY'").execute(&db).await.unwrap();
    as_(&home, M::POST, "memories", Some(json!({"text": "Home note: deploys on Fridays."}))).await.unwrap();
    as_(&root, M::POST, "memories", Some(json!({"text": "Lab note: experiments only."}))).await.unwrap();
    sqlx::query("UPDATE records SET realm = 'lab' WHERE collection = 'memory' AND json_extract(fields, '$.text') LIKE 'Lab note%'")
        .execute(&db)
        .await
        .unwrap();
    let notes: Value = as_(&home, M::GET, "memories", None).await.unwrap().json().await.unwrap();
    assert_eq!(notes.as_array().unwrap().len(), 1, "{notes}");

    let probe = r#"{"command":"echo home=${HOME_KEY:-unset} lab=${LAB_KEY:-unset}"}"#;
    // A home session (by the home token).
    mock.push(Reply::ToolCalls(vec![("bash".into(), probe.into())]));
    mock.push(Reply::Text("done".into()));
    let h: Session = start(home.clone(), None, "check keys").await.json().await.unwrap();
    assert_eq!(h.realm, "home");
    eventually("the home session's second call", async || (mock.requests().len() >= 2).then_some(())).await;
    // A lab session (by the root token).
    mock.push(Reply::ToolCalls(vec![("bash".into(), probe.into())]));
    mock.push(Reply::Text("done".into()));
    let l: Session = start(root.clone(), Some("lab"), "check keys").await.json().await.unwrap();
    assert_eq!(l.realm, "lab");
    eventually("the lab session's second call", async || (mock.requests().len() >= 4).then_some(())).await;
    let reqs = mock.requests();
    let (home_sys, home_out) = (reqs[0]["messages"][0]["content"].to_string(), reqs[1]["messages"].to_string());
    let (lab_sys, lab_out) = (reqs[2]["messages"][0]["content"].to_string(), reqs[3]["messages"].to_string());
    assert!(home_out.contains("home=[secret:HOME_KEY] lab=unset"), "{home_out}");
    assert!(lab_out.contains("home=unset lab=[secret:LAB_KEY]"), "{lab_out}");
    assert!(
        home_sys.contains("HOME_KEY")
            && !home_sys.contains("LAB_KEY")
            && home_sys.contains("deploys on Fridays")
            && !home_sys.contains("experiments only"),
        "{home_sys}"
    );
    assert!(
        lab_sys.contains("LAB_KEY")
            && !lab_sys.contains("HOME_KEY")
            && lab_sys.contains("experiments only")
            && !lab_sys.contains("deploys on Fridays"),
        "{lab_sys}"
    );

    // The home token sees home's sessions only; another realm's are missing.
    let mine: Vec<Session> = as_(&home, M::GET, "sessions", None).await.unwrap().json().await.unwrap();
    assert_eq!(mine.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec![h.id.as_str()]);
    assert_eq!(as_(&home, M::GET, &format!("sessions/{}", l.id), None).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_eq!(as_(&home, M::GET, &format!("sessions/{}/events", l.id), None).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert!(as_(&home, M::GET, &format!("sessions/{}/events", h.id), None).await.unwrap().status().is_success());
    let all: Vec<Session> = as_(&root, M::GET, "sessions", None).await.unwrap().json().await.unwrap();
    assert_eq!(all.len(), 2);

    // Action items stay in their realm.
    let t: Value = as_(&home, M::POST, "decisions", Some(json!({"title": "Home question"}))).await.unwrap().json().await.unwrap();
    as_(&root, M::POST, "decisions", Some(json!({"title": "Lab question"}))).await.unwrap();
    sqlx::query("UPDATE decisions SET realm = 'lab' WHERE title = 'Lab question'").execute(&db).await.unwrap();
    let todos: Value = as_(&home, M::GET, "decisions", None).await.unwrap().json().await.unwrap();
    assert_eq!(todos.as_array().unwrap().len(), 1, "{todos}");
    assert_eq!(todos[0]["id"], t["id"]);
    let count: Value = as_(&home, M::GET, "decisions/count", None).await.unwrap().json().await.unwrap();
    assert_eq!(count["open"], 1);

    stop.cancel();
    head.shutdown().await.unwrap();
}

/// Realms you create (docs/os.md §3): each has its own settings lists,
/// workers and connections can be kept to some realms, and a realm is
/// deleted only once it owns nothing.
#[tokio::test]
async fn creating_realms() {
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
    let send = |method: reqwest::Method, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    use reqwest::Method as M;

    // Create: ids are checked and unique.
    assert_eq!(send(M::POST, "realms", json!({"id": "Bad Id"})).await.status(), StatusCode::BAD_REQUEST);
    let r =
        send(M::POST, "realms", json!({"id": "factory", "name": "Service factory", "description": "Finds gaps, builds services"})).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    assert_eq!(send(M::POST, "realms", json!({"id": "factory"})).await.status(), StatusCode::BAD_REQUEST);
    let realms: Value = http.get(format!("{api}/realms")).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        realms.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["root", "home", "commons", "factory"]
    );
    // It has its own system collections.
    let cols: Value = http.get(format!("{api}/collections?realm=factory")).send().await.unwrap().json().await.unwrap();
    assert!(cols.as_array().unwrap().iter().any(|c| c["name"] == "skills" && c["system"] == true), "{cols}");

    // Settings for one realm: its own lists.
    let put =
        send(M::PUT, "settings/skills?realm=factory", json!([{"name": "rubric", "description": "FACTORY-RUBRIC", "body": "x"}])).await;
    assert!(put.status().is_success());
    send(M::PUT, "settings/skills", json!([{"name": "greet", "description": "HOME-GREET", "body": "x"}])).await;
    let get = |path: &'static str| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/{path}")).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    assert_eq!(get("settings/skills?realm=factory").await[0]["name"], "rubric");
    assert_eq!(get("settings/skills").await[0]["name"], "greet");
    assert_eq!(http.get(format!("{api}/settings/skills?realm=nowhere")).send().await.unwrap().status(), StatusCode::BAD_REQUEST);

    // A worker dedicated to the factory runs only its sessions.
    assert!(send(M::PUT, &format!("workers/{wid}/realms"), json!({"realms": ["factory"]})).await.status().is_success());
    assert_eq!(send(M::PUT, &format!("workers/{wid}/realms"), json!({"realms": ["root"]})).await.status(), StatusCode::BAD_REQUEST);
    let workers: Value = get("workers").await;
    assert_eq!(workers[0]["realms"], json!(["factory"]));
    let start = |realm: &'static str| {
        send(
            M::POST,
            "sessions",
            json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "hi", "realm": realm}),
        )
    };
    let r = start("home").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("not available to realm home"));
    mock.push(Reply::Text("ok".into()));
    let r = start("factory").await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    eventually("the factory session's call", async || (!mock.requests().is_empty()).then_some(())).await;
    let sys = mock.requests()[0]["messages"][0]["content"].as_str().unwrap().to_owned();
    assert!(sys.contains("FACTORY-RUBRIC") && !sys.contains("HOME-GREET"), "{sys}");
    // A connection kept to home: the factory's sessions cannot use it.
    send(M::PUT, &format!("workers/{wid}/realms"), json!({"realms": []})).await;
    assert!(send(M::PUT, &format!("connections/{}/realms", conn.id), json!({"realms": ["home"]})).await.status().is_success());
    let r = start("factory").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("not available to realm factory"));

    // Deleting: only an empty realm; root and home stay.
    let r = send(M::DELETE, "realms/factory", json!({})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("still owns 1 sessions"));
    send(M::POST, "realms", json!({"id": "scratch"})).await;
    assert_eq!(send(M::DELETE, "realms/scratch", json!({})).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(send(M::DELETE, "realms/home", json!({})).await.status(), StatusCode::BAD_REQUEST);
    stop.cancel();
    head.shutdown().await.unwrap();
}
