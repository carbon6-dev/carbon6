//! Records of collections are mirrored into the memex (docs/os.md §4.1):
//! search finds them, and deleting one removes it.

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
async fn records_are_searchable() {
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

    let r = http
        .post(format!("{api}/collections"))
        .json(&json!({"name": "contacts", "schema": {"type": "object", "properties": {"name": {"type": "string"}, "company": {"type": "string"}}}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let rec: Value = http
        .post(format!("{api}/collections/contacts/records"))
        .json(&json!({"fields": {"name": "Grace Hopper", "company": "Navy"}, "links": ["client:navy"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let search = |q: &'static str| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/memex/search?q={q}")).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let found = eventually("the record in the memex", async || {
        let v = search("Hopper").await;
        v.to_string().contains("Grace Hopper").then_some(v)
    })
    .await;
    assert_eq!(found[0]["kind"], "record", "{found}");
    let id = rec["id"].as_str().unwrap();
    http.delete(format!("{api}/collections/contacts/records/{id}")).send().await.unwrap();
    eventually("the record gone from the memex", async || (!search("Hopper").await.to_string().contains("Grace Hopper")).then_some(()))
        .await;
    stop.cancel();
}
