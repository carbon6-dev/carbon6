//! Secrets: stored on the head (never shown), given to the commands of
//! the sessions they apply to as environment variables, and redacted from
//! what the model sees.

use c6_api::Connection;
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn secrets_reach_commands_but_not_the_model() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let p = project.path();

    let put = |name: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/secrets/{name}"));
        async move { http.put(url).json(&body).send().await.unwrap().status() }
    };
    assert_eq!(put("bad-name", json!({"value": "x"})).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put("NEW_ONE", json!({"description": "no value"})).await, reqwest::StatusCode::BAD_REQUEST, "a new secret needs a value");
    assert!(put("DEPLOY_KEY", json!({"value": "dk-9f8e7d6c5b", "description": "Deploys to staging"})).await.is_success());
    assert!(put("PY_ONLY", json!({"value": "py-secret-777", "applies_to": ["python"]})).await.is_success());
    // Updating the description keeps the value.
    assert!(put("DEPLOY_KEY", json!({"description": "Deploys to staging (rotated monthly)"})).await.is_success());
    let list: Value = http.get(format!("{api}/settings/secrets")).send().await.unwrap().json().await.unwrap();
    assert!(!list.to_string().contains("dk-9f8e7d6c5b"), "values are never listed: {list}");
    assert_eq!(list[0]["description"], "Deploys to staging (rotated monthly)");

    // An MCP server that needs a secret this session lacks is not started,
    // and the session says why.
    let r = http
        .put(format!("{api}/settings/mcp-servers"))
        .json(&json!([{"name": "needy", "transport": "stdio", "command": "true", "env": {"TOKEN": "${MISSING_TOKEN}"}}]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    mock.push(Reply::ToolCalls(vec![(
        "bash".into(),
        r#"{"command":"test ${#DEPLOY_KEY} -eq 13 && echo has-key; echo leak=$DEPLOY_KEY; echo py=${PY_ONLY:-unset}"}"#.into(),
    )]));
    mock.push(Reply::Text("done".into()));
    let wid = worker.identity.meta.worker_id.0;
    http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m", "prompt": "deploy"}))
        .send()
        .await
        .unwrap();
    eventually("two requests", async || (mock.requests().len() == 2).then_some(())).await;
    let reqs = mock.requests();
    let system = reqs[0]["messages"][0]["content"].as_str().unwrap().to_owned();
    assert!(system.contains("- DEPLOY_KEY: Deploys to staging (rotated monthly)") && !system.contains("PY_ONLY"), "{system}");
    let all = serde_json::to_string(&reqs).unwrap();
    assert!(!all.contains("dk-9f8e7d6c5b"), "the value never reaches the model");
    let second = reqs[1]["messages"].to_string();
    assert!(second.contains("has-key") && second.contains("leak=[secret:DEPLOY_KEY]") && second.contains("py=unset"), "{second}");

    let sessions: Vec<Value> = http.get(format!("{api}/sessions")).send().await.unwrap().json().await.unwrap();
    let id = sessions[0]["id"].as_str().unwrap();
    let evs: Value = http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap();
    assert!(evs.to_string().contains("MCP server needy not started: it needs the secret MISSING_TOKEN"), "{evs}");

    // Each realm its own: work's DEPLOY_KEY is not home's, and root's are
    // every realm's that has none of its own by that name.
    assert!(http.post(format!("{api}/realms")).json(&json!({"id": "work"})).send().await.unwrap().status().is_success());
    let put_in = |realm: &str, name: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/secrets/{name}?realm={realm}"));
        async move { http.put(url).json(&body).send().await.unwrap().status() }
    };
    assert!(put_in("work", "DEPLOY_KEY", json!({"value": "dk-work-0000", "description": "Work deploys"})).await.is_success());
    assert!(put_in("root", "SHARED_TOKEN", json!({"value": "shared-1", "description": "Everyone's"})).await.is_success());
    let work: Vec<c6_api::SecretInfo> = http.get(format!("{api}/settings/secrets?realm=work")).send().await.unwrap().json().await.unwrap();
    let names: Vec<(&str, &str)> = work.iter().map(|s| (s.name.as_str(), s.realm.as_str())).collect();
    assert_eq!(names, [("DEPLOY_KEY", "work"), ("SHARED_TOKEN", "root")], "its own, and root's");
    mock.push(Reply::ToolCalls(vec![(
        "bash".into(),
        r#"{"command":"test ${#DEPLOY_KEY} -eq 12 && echo work-key; echo shared=${SHARED_TOKEN:+yes}"}"#.into(),
    )]));
    mock.push(Reply::Text("done".into()));
    let before = mock.requests().len();
    http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m", "prompt": "deploy", "realm": "work"}))
        .send()
        .await
        .unwrap();
    eventually("the work session's turn", async || (mock.requests().len() == before + 2).then_some(())).await;
    let reqs = mock.requests();
    let out = reqs[before + 1]["messages"].to_string();
    assert!(out.contains("work-key") && out.contains("shared=yes"), "{out}");
    assert!(reqs[before]["messages"][0]["content"].as_str().unwrap().contains("- DEPLOY_KEY: Work deploys"));
    // Home's is still home's.
    let home: Vec<c6_api::SecretInfo> = http.get(format!("{api}/settings/secrets?realm=home")).send().await.unwrap().json().await.unwrap();
    assert!(home.iter().any(|s| s.name == "DEPLOY_KEY" && s.realm == "home" && s.description.starts_with("Deploys to staging")));

    stop.cancel();
    head.shutdown().await.unwrap();
}
