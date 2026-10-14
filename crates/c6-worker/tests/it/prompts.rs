//! carbon6's prompts are editable records (docs/os.md §4.6): sessions start
//! with the base prompt as edited, a realm can override one, action-item
//! sessions use theirs with placeholders filled, an empty text returns a
//! prompt to its default, and a prompt nobody edited follows its default.

use std::time::Duration;

use c6_api::{Connection, PromptInfo};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
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
async fn prompts_are_editable() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let config = HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    };
    let head = c6_head::start(config.clone()).await.unwrap();
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

    // Every prompt, at its default.
    let list = || {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/settings/prompts")).send().await.unwrap().json::<Vec<PromptInfo>>().await.unwrap() }
    };
    let prompts = list().await;
    for name in
        ["session.base", "session.compact", "session.subagent", "assistant.base", "todo.session", "memex.brief.knowledge", "memex.look"]
    {
        let p = prompts.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("{name}"));
        assert!(!p.edited && p.body == p.default && !p.body.is_empty(), "{name}");
    }
    let set = |name: &'static str, body: &'static str| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            assert!(
                http.put(format!("{api}/settings/prompts/{name}")).json(&json!({"body": body})).send().await.unwrap().status().is_success()
            )
        }
    };
    let r = http.put(format!("{api}/settings/prompts/no.such")).json(&json!({"body": "x"})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);

    // The base prompt as edited; another realm's own version.
    set("session.base", "CUSTOM BASE: you are a careful agent.").await;
    set("todo.session", "Settle #{{id}}: {{title}} ({{kind}}). TODO-PROMPT").await;
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(head_dir.path().join("c6h.db")))
        .await
        .unwrap();
    sqlx::query("INSERT INTO realms (id, name, created_at, created_by) VALUES ('lab', 'Lab', 0, 'you')").execute(&db).await.unwrap();
    sqlx::query("INSERT INTO records (id, realm, collection, fields, version, created_by, updated_by, created_at, updated_at) VALUES ('p1', 'lab', 'prompts', ?, 1, 'you', 'you', 0, 0)")
        .bind(json!({"name": "session.base", "description": "", "body": "LAB BASE."}).to_string())
        .execute(&db)
        .await
        .unwrap();
    for (realm, n) in [("home", 1), ("lab", 2)] {
        mock.push(Reply::Text("ok".into()));
        http.post(format!("{api}/sessions"))
            .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "hi", "realm": realm}))
            .send()
            .await
            .unwrap();
        eventually("the session's call", async || (mock.requests().len() >= n).then_some(())).await;
    }
    let sys = |i: usize| mock.requests()[i]["messages"][0]["content"].as_str().unwrap().to_owned();
    assert!(sys(0).starts_with("CUSTOM BASE: you are a careful agent."), "{}", sys(0));
    assert!(sys(1).starts_with("LAB BASE.") && !sys(1).contains("CUSTOM BASE"), "{}", sys(1));

    // An action item's session gets its prompt, filled in.
    let t: Value = http
        .post(format!("{api}/decisions"))
        .json(&json!({"kind": "confirm", "title": "Is CI on Argo?"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    mock.push(Reply::Text("ok".into()));
    let r = http.post(format!("{api}/decisions/{}/session", t["id"])).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    eventually("the action item's session", async || (mock.requests().len() >= 3).then_some(())).await;
    let first = mock.requests()[2]["messages"].to_string();
    assert!(first.contains(&format!("Settle #{}: Is CI on Argo? (confirm). TODO-PROMPT", t["id"])), "{first}");

    // Edited, and back to the default.
    let base = list().await.into_iter().find(|p| p.name == "session.base").unwrap();
    assert!(base.edited && base.body.starts_with("CUSTOM BASE"));
    set("session.base", "").await;
    let base = list().await.into_iter().find(|p| p.name == "session.base").unwrap();
    assert!(!base.edited && base.body == base.default);

    // A prompt nobody edited follows its default (here: an old default, as
    // after an upgrade); an edited one is left alone.
    sqlx::query("UPDATE records SET fields = json_set(fields, '$.body', 'OLD DEFAULT') WHERE collection = 'prompts' AND realm = 'root' AND json_extract(fields, '$.name') = 'memex.look'")
        .execute(&db)
        .await
        .unwrap();
    stop.cancel();
    head.shutdown().await.unwrap();
    let head = c6_head::start(config).await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let prompts: Vec<PromptInfo> = http.get(format!("{api}/settings/prompts")).send().await.unwrap().json().await.unwrap();
    let look = prompts.iter().find(|p| p.name == "memex.look").unwrap();
    assert!(!look.edited && look.body == look.default, "{look:?}");
    let todo = prompts.iter().find(|p| p.name == "todo.session").unwrap();
    assert!(todo.edited && todo.body.contains("TODO-PROMPT"), "{todo:?}");
    head.shutdown().await.unwrap();
}
