//! Each realm's knowledge is its own (docs/os.md §3): another realm's
//! memex is a database of its own under home's, so search, notes, facts
//! and briefs never cross; mail and the like stay home's; a deleted
//! realm's knowledge is dropped.

use std::time::Duration;

use c6_head::HeadConfig;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn realms_have_their_own_knowledge() {
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
    let stop = CancellationToken::new();
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, Some("memex")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let m = c6_memex::Memex::open(&dir.path().join("memex"), w.link.clone()).await.unwrap();
    m.spawn_jobs();
    w.set_extension(m);
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    assert!(http.post(format!("{api}/realms")).json(&json!({"id": "lab"})).send().await.unwrap().status().is_success());
    let st = &head.state;

    // A note and a fact in each realm.
    let call =
        |realm: &'static str, method: &'static str, params: Value| async move { c6_head::memex::call_in(st, realm, method, params).await };
    call("home", "memex.note", json!({"text": "Home: the staging cluster is in us-east.", "labels": ["env:staging"]})).await.unwrap();
    call("lab", "memex.note", json!({"text": "Lab: the staging cluster is in eu-west.", "labels": ["env:staging"]})).await.unwrap();
    call("lab", "memex.knowledge.record", json!({"claim": "Lab CI runs on Buildkite", "about": ["ci:buildkite"], "status": "inferred"}))
        .await
        .unwrap();
    let search =
        |realm: &'static str| async move { call(realm, "memex.search", json!({"query": "staging cluster"})).await.unwrap().to_string() };
    let home = eventually("home's note", async || {
        let s = search("home").await;
        s.contains("us-east").then_some(s)
    })
    .await;
    assert!(!home.contains("eu-west"), "home sees only its own: {home}");
    let lab = search("lab").await;
    assert!(lab.contains("eu-west") && !lab.contains("us-east"), "lab sees only its own: {lab}");
    // Facts and things are per realm too.
    let home_facts = call("home", "memex.knowledge.list", json!({})).await.unwrap().to_string();
    assert!(!home_facts.contains("Buildkite"), "{home_facts}");
    let lab_brief = c6_head::memex::brief(st, "lab", &["ci:buildkite".to_owned()], "fix CI").await;
    assert!(lab_brief.contains("Lab CI runs on Buildkite"), "{lab_brief}");
    assert!(!c6_head::memex::brief(st, "home", &["ci:buildkite".to_owned()], "fix CI").await.contains("Buildkite"));
    // Its own database, under home's.
    assert!(dir.path().join("memex/realms/lab/memex.db").exists());
    // Mail and the like are home's.
    let refused = call("lab", "memex.mail.threads", json!({})).await.unwrap_err();
    assert!(format!("{refused:#}").contains("home's"), "{refused:#}");
    // A token for lab reaches lab's knowledge through the API.
    let tok: Value =
        http.post(format!("{api}/auth/tokens")).json(&json!({"name": "t", "realm": "lab"})).send().await.unwrap().json().await.unwrap();
    let found: Value = http
        .get(format!("{api}/memex/search?q=staging"))
        .bearer_auth(tok["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(found.to_string().contains("eu-west") && !found.to_string().contains("us-east"), "{found}");

    // Deleting the realm drops its knowledge.
    let r = http.delete(format!("{api}/realms/lab")).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    eventually("lab's memex dropped", async || (!dir.path().join("memex/realms/lab").exists()).then_some(())).await;
    stop.cancel();
}
