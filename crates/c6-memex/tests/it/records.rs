//! Records of collections are mirrored into the memex (docs/os.md §4.1):
//! search finds them, and deleting one removes it.

use std::time::Duration;

use c6_head::HeadConfig;
use serde_json::{Value, json};
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
    // The knowledge store is the head's own (docs/knowledge.md).

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

/// The memex's prompts are editable (docs/os.md §4.6): an edited one
/// reaches the memex, here the guidance briefs give before facts.
#[tokio::test]
async fn edited_prompts_reach_the_memex() {
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
    // The knowledge store is the head's own (docs/knowledge.md).

    let r = http
        .post(format!("{api}/memex/rpc"))
        .json(&json!({"method": "memex.knowledge.record", "params": {"claim": "CI runs on Argo", "about": ["ci:argo"], "status": "inferred"}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let labels = vec!["ci:argo".to_owned()];
    let brief = || c6_head::memex::brief(&head.state, "home", &labels, "fix the build");
    let first = eventually("a brief with the fact", async || {
        let b = brief().await;
        b.contains("CI runs on Argo").then_some(b)
    })
    .await;
    assert!(first.contains("What earlier sessions learned"), "the default guidance: {first}");
    let r = http
        .put(format!("{api}/settings/prompts/memex.brief.knowledge"))
        .json(&json!({"body": "CUSTOM-GUIDANCE: trust nothing."}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    eventually("the edited guidance in briefs", async || brief().await.contains("CUSTOM-GUIDANCE: trust nothing.").then_some(())).await;
    stop.cancel();
}
