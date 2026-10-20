//! Each realm's knowledge is its own (docs/os.md §3): another realm's
//! memex is a database of its own under home's, so search, notes, facts
//! and briefs never cross; mail and the like stay home's; a deleted
//! realm's knowledge is dropped.

use c6_head::HeadConfig;
use c6_testkit::eventually;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn realms_have_their_own_knowledge() {
    crate::extract_here();
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
    let dir = c6_head::knowledge::dir(&head.state);
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
    // The explorer reads lab's knowledge: counts, lists, a thing and its facts.
    let sum: c6_api::RealmSummary = http.get(format!("{api}/realms/lab/summary")).send().await.unwrap().json().await.unwrap();
    assert!(sum.knowledge, "{sum:?}");
    assert!(sum.entries.iter().any(|e| e.section == "items:note" && e.count >= 1), "{:?}", sum.entries);
    assert!(sum.entries.iter().any(|e| e.section.starts_with("facts:") && e.count >= 1), "{:?}", sum.entries);
    let things: Vec<c6_api::ExRef> = http.get(format!("{api}/realms/lab/list?section=things:")).send().await.unwrap().json().await.unwrap();
    assert!(!things.is_empty());
    let t: c6_api::ExObject =
        http.get(format!("{api}/realms/lab/object?kind=thing&id=ci:buildkite")).send().await.unwrap().json().await.unwrap();
    let facts = t.connected.iter().find(|g| g.label == "Facts about it").expect("facts");
    assert_eq!(facts.items[0].title, "Lab CI runs on Buildkite");
    let notes: Vec<c6_api::ExRef> =
        http.get(format!("{api}/realms/lab/list?section=items:note")).send().await.unwrap().json().await.unwrap();
    let item: c6_api::ExObject =
        http.get(format!("{api}/realms/lab/object?kind=item&id={}", notes[0].id)).send().await.unwrap().json().await.unwrap();
    assert!(item.body.contains("eu-west"), "{item:?}");
    // Search reaches the knowledge: items and the facts.
    let found: Vec<c6_api::ExGroup> = http.get(format!("{api}/realms/lab/search?q=staging")).send().await.unwrap().json().await.unwrap();
    let k = found.iter().find(|g| g.label == "Knowledge").expect("knowledge hits");
    assert!(k.items.iter().any(|i| i.sub.contains("eu-west")), "{k:?}");
    let found: Vec<c6_api::ExGroup> = http.get(format!("{api}/realms/lab/search?q=Buildkite")).send().await.unwrap().json().await.unwrap();
    assert!(found.iter().any(|g| g.label == "Facts"), "{found:?}");
    // And a thing's two-hop neighborhood: its fact, and what that fact is about.
    let n: serde_json::Value =
        http.get(format!("{api}/realms/lab/neighborhood?kind=thing&id=ci:buildkite")).send().await.unwrap().json().await.unwrap();
    assert!(n["nodes"].as_array().unwrap().iter().any(|x| x["ref"]["kind"] == "fact"), "{n}");

    // Home's mail and photos link out to their own pages.
    let home: c6_api::RealmSummary = http.get(format!("{api}/realms/home/summary")).send().await.unwrap().json().await.unwrap();
    assert!(home.knowledge);

    // Its own database, under home's.
    assert!(dir.join("realms/lab/memex.db").exists());
    // Photos and folders are home's (mail, calendars and contacts are each
    // realm's own).
    let refused = call("lab", "memex.media.list", json!({})).await.unwrap_err();
    assert!(call("lab", "memex.mail.threads", json!({})).await.is_ok());
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

    // A grant lets lab read home's notes: its searches include them, from
    // home; each use is logged; revoked, they're gone again.
    let g: Value = http
        .post(format!("{api}/grants"))
        .json(&json!({"from_realm": "home", "to_realm": "lab", "kind": "memex", "mode": "read", "scope": {"kinds": ["note"]}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lab_search = || {
        let (http, api, tok) = (http.clone(), api.clone(), tok["token"].as_str().unwrap().to_owned());
        async move { http.get(format!("{api}/memex/search?q=staging")).bearer_auth(tok).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let found = lab_search().await;
    let hits = found.as_array().unwrap();
    let from_home = hits.iter().find(|h| h["snippet"].as_str().unwrap_or("").contains("us-east")).expect("home's note, granted");
    assert_eq!(from_home["realm"], "home");
    assert!(hits.iter().any(|h| h["snippet"].as_str().unwrap_or("").contains("eu-west")), "{found}");
    let item: Value = http
        .get(format!("{api}/memex/get?id={}", from_home["id"].as_str().unwrap()))
        .bearer_auth(tok["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(item["realm"], "home", "{item}");
    let uses: Value = http.get(format!("{api}/grants/{}/uses", g["id"].as_str().unwrap())).send().await.unwrap().json().await.unwrap();
    assert_eq!(uses.as_array().unwrap().len(), 2, "{uses}");
    assert!(http.delete(format!("{api}/grants/{}", g["id"].as_str().unwrap())).send().await.unwrap().status().is_success());
    assert!(!lab_search().await.to_string().contains("us-east"));

    // Commons: lab publishes its fact, you review it, and another realm
    // finds it in search and briefs.
    let facts = call("lab", "memex.knowledge.list", json!({})).await.unwrap();
    let fact = facts[0]["id"].as_str().unwrap().to_owned();
    let refused = c6_head::commons::propose_fact(st, "lab", "you", &fact).await.unwrap_err();
    assert!(format!("{refused:#}").contains("append grant"), "{refused:#}");
    let r = http
        .post(format!("{api}/grants"))
        .json(&json!({"from_realm": "commons", "to_realm": "lab", "kind": "memex", "mode": "append"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let d = c6_head::commons::propose_fact(st, "lab", "session:abc", &fact).await.unwrap();
    assert!(d.document.contains("Lab CI runs on Buildkite"), "{d:?}");
    let r = http.post(format!("{api}/decisions/{}/answer", d.id)).json(&json!({"option": "Publish"})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let shared = call("commons", "memex.knowledge.list", json!({})).await.unwrap();
    assert_eq!(
        (shared[0]["claim"].as_str(), shared[0]["status"].as_str()),
        (Some("Lab CI runs on Buildkite"), Some("reviewed")),
        "{shared}"
    );
    let home_brief = c6_head::memex::brief(st, "home", &["ci:buildkite".to_owned()], "fix CI").await;
    assert!(home_brief.contains("From commons") && home_brief.contains("Buildkite"), "{home_brief}");
    // Not for a realm that doesn't read commons.
    http.patch(format!("{api}/realms/home")).json(&json!({"reads_commons": false})).send().await.unwrap();
    assert!(!c6_head::memex::brief(st, "home", &["ci:buildkite".to_owned()], "fix CI").await.contains("Buildkite"));

    // Deleting the realm drops its knowledge.
    let r = http.delete(format!("{api}/realms/lab")).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    eventually("lab's memex dropped", async || (!dir.join("realms/lab").exists()).then_some(())).await;
    stop.cancel();
}
