//! Claims (docs/apps.md §9), in the head's knowledge graph: anyone adds
//! their own (you, an app's session, the realm's agents), each owned and
//! only its owner retracts it; readers believe by the realm's policies
//! (yours first by default); identity is same_as claims; an app's labels
//! map onto the shared vocabulary; removing an app retracts its claims; a
//! fact is retired only by its owner.

use c6_api::{Claim, Connection, Install};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn claims_are_owned_and_readers_choose() {
    crate::extract_here();
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
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let conn: Connection =
        serde_json::from_value(ok(send(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).await).unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let wid = worker.identity.meta.worker_id.0;

    // Things anyone may make, and an email to make claims about.
    for (name, key) in [("Jane Doe", "person:jane@acme.com"), ("Jim Roe", "person:jim@acme.com"), ("J. Doe", "person:jdoe@home.net")] {
        ok(send(M::POST, "memex/things", json!({"kind": "person", "name": name, "key": key})).await).await;
    }
    let ingested = ok(send(
        M::POST,
        "memex/rpc",
        json!({"method": "memex.ingest", "params": {"items": [{"type": "text/markdown",
        "text": "# Q3 plan\n\nThe Q3 plan, as discussed.", "source": {"kind": "test"}, "external_id": "q3"}]}}),
    )
    .await)
    .await;
    let item = ingested["results"][0]["id"].as_str().map(str::to_owned).unwrap_or_else(|| panic!("{ingested}"));
    let about = format!("item:{item}");
    let claims = |q: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let v: Value = http.get(format!("{api}/memex/claims?{q}")).send().await.unwrap().json().await.unwrap();
            serde_json::from_value::<Vec<Claim>>(v.clone()).unwrap_or_else(|e| panic!("{e}: {v}"))
        }
    };

    // An app whose sessions make claims, with a label of its own.
    let crm = json!({"package": "crm", "version": "1.0.0", "settings": {"type": "object", "properties": {}},
        "vocabulary": [{"name": "wrote", "kind": "label", "means": "the CRM's guess at who wrote it", "maps_to": "sent_by"}],
        "contents": {"templates": [{"name": "guess", "prompt": "Who wrote {{input}}?", "connection": conn.id, "model": "m"}]}});
    ok(send(M::POST, "packages", json!({"manifest": crm})).await).await;
    let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "crm"})).await).await).unwrap();
    ok(send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await).await;

    // The app claims Jim wrote it (its own label); the realm's agent claims Jane.
    mock.push(Reply::ToolCalls(vec![(
        "claim_add".into(),
        json!({"from": about, "label": "wrote", "to": "thing:person:jim@acme.com", "confidence": 0.6}).to_string(),
    )]));
    mock.push(Reply::Text("Jim, probably.".into()));
    ok(send(M::POST, "templates/guess/run", json!({"input": "the Q3 plan"})).await).await;
    let n = eventually("the app's claim", async || Some(claims(format!("about={about}&policy=all")).await).filter(|c| !c.is_empty())).await;
    assert_eq!((n[0].owner.as_str(), n[0].label.as_str()), ("app:crm", "wrote"));
    mock.push(Reply::ToolCalls(vec![(
        "claim_add".into(),
        json!({"from": about, "label": "sent_by", "to": "thing:person:jane@acme.com", "evidence": [item]}).to_string(),
    )]));
    mock.push(Reply::Text("Jane.".into()));
    ok(send(M::POST, "sessions", json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "who wrote it?"}))
        .await)
    .await;
    eventually("the realm's claim", async || (claims(format!("about={about}&policy=all")).await.len() == 2).then_some(())).await;

    // A label finds owners' labels mapped to it, with what each means; with
    // no policy, both stand.
    let sent = claims(format!("about={about}&label=sent_by")).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    let wrote = sent.iter().find(|c| c.label == "wrote").unwrap();
    assert!(wrote.means.contains("the CRM's guess") && wrote.means.contains("(as sent_by)"), "{wrote:?}");
    let realm_claim = sent.iter().find(|c| c.owner == "realm").unwrap().clone();
    assert_eq!(realm_claim.evidence, vec![item.clone()]);
    assert!(realm_claim.made_by.starts_with("session:"));

    // Your correction outranks both by default.
    let mine: Claim = serde_json::from_value(
        ok(send(M::POST, "memex/claims", json!({"from": about, "label": "sent_by", "to": "thing:person:jdoe@home.net"})).await).await,
    )
    .unwrap();
    assert_eq!(mine.owner, "you");
    let believed = claims(format!("about={about}&label=sent_by")).await;
    assert_eq!(believed.iter().map(|c| c.to.as_str()).collect::<Vec<_>>(), vec!["thing:person:jdoe@home.net"], "{believed:?}");
    let all = claims(format!("about={about}&label=sent_by&policy=all")).await;
    assert_eq!(all.iter().filter(|c| c.believed).count(), 1);
    // A realm's policy: the app's word first.
    ok(send(M::PUT, "settings/claim_policies", json!([{"label": "sent_by", "believe": ["realm", "you"]}])).await).await;
    let believed = claims(format!("about={about}&label=sent_by")).await;
    assert_eq!(believed.iter().map(|c| c.owner.as_str()).collect::<Vec<_>>(), vec!["realm"]);
    assert_eq!(send(M::PUT, "settings/claim_policies", json!([{"label": "x", "believe": []}])).await.status(), 400);

    // Only the owner retracts: you can't take the realm's claim away.
    let r = send(M::DELETE, &format!("memex/claims/{}", realm_claim.id), json!({})).await;
    assert!(r.text().await.unwrap().contains("only its owner retracts it"));
    ok(send(M::DELETE, &format!("memex/claims/{}", mine.id), json!({})).await).await;

    // Identity: Jane and J. Doe are the same, by your claim.
    ok(send(
        M::POST,
        "memex/claims",
        json!({"from": "thing:person:jane@acme.com", "label": "same_as", "to": "thing:person:jdoe@home.net"}),
    )
    .await)
    .await;
    ok(send(M::POST, "memex/claims", json!({"from": about, "label": "about", "to": "thing:person:jdoe@home.net"})).await).await;
    let jane = claims("about=thing:person:jane@acme.com&same_as=true".into()).await;
    assert!(jane.iter().any(|c| c.label == "about" && c.to == "thing:person:jdoe@home.net"), "{jane:?}");
    let alone = claims("about=thing:person:jane@acme.com".into()).await;
    assert!(!alone.iter().any(|c| c.label == "about"), "{alone:?}");

    // The shared vocabulary, in root, starts with carbon6's labels and kinds.
    let vocab = ok(send(M::GET, "settings/vocabulary", json!({})).await).await;
    assert!(vocab.as_array().unwrap().iter().any(|v| v["name"] == "same_as" && v["kind"] == "label"), "{vocab}");

    // A fact is retired only by its owner: the app records one; the realm's
    // agent can't retire it.
    mock.push(Reply::ToolCalls(vec![(
        "knowledge_record".into(),
        json!({"claim": "Acme bills quarterly", "about": ["org:acme"], "status": "inferred"}).to_string(),
    )]));
    mock.push(Reply::Text("noted".into()));
    ok(send(M::POST, "templates/guess/run", json!({"input": "x"})).await).await;
    let fact = eventually("the app's fact", async || {
        let v = ok(send(M::POST, "memex/rpc", json!({"method": "memex.knowledge.list", "params": {"about": ["org:acme"]}})).await).await;
        v.as_array()?.iter().find(|f| f["claim"] == "Acme bills quarterly").cloned()
    })
    .await;
    let fid = fact["id"].as_str().unwrap().to_owned();
    mock.push(Reply::ToolCalls(vec![("knowledge_set".into(), json!({"id": fid, "status": "retired"}).to_string())]));
    mock.push(Reply::Text("tried".into()));
    let before = mock.requests().len();
    ok(send(M::POST, "sessions", json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "retire it"}))
        .await)
    .await;
    eventually("the refusal", async || {
        (mock.requests().len() > before + 1 && mock.requests().iter().any(|r| r["messages"].to_string().contains("is app:crm's")))
            .then_some(())
    })
    .await;

    // Removing the app retracts its claims.
    let rm: Install = serde_json::from_value(ok(send(M::DELETE, &format!("installs/{}", i.id), json!({})).await).await).unwrap();
    ok(send(M::POST, &format!("decisions/{}/answer", rm.decision.unwrap()), json!({"option": "Remove"})).await).await;
    let left = claims(format!("about={about}&policy=all")).await;
    assert!(left.iter().all(|c| c.owner != "app:crm"), "{left:?}");

    stop.cancel();
    head.shutdown().await.unwrap();
}

/// A kind's schema that gained fields since your vocabulary was seeded
/// gets them (items with them are refused without), your edits kept.
#[tokio::test]
async fn a_kinds_new_fields_reach_a_seeded_vocabulary() {
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
    let pool = head.state.store.pool();
    // As an earlier version left it: chat without from_me, and your edit.
    let mut vocab = c6_head::lists::read(pool, "vocabulary", "root").await.unwrap();
    let chat = vocab.iter_mut().find(|e| e["name"] == "chat" && e["kind"] == "kind").unwrap();
    chat["schema"]["properties"].as_object_mut().unwrap().remove("from_me");
    chat["schema"]["properties"]["user_name"]["description"] = json!("Who, as you put it");
    c6_head::lists::write(pool, "vocabulary", "root", &vocab, "you").await.unwrap();
    sqlx::query("UPDATE settings SET value = '3' WHERE key = 'vocabulary_seeded'").execute(pool).await.unwrap();
    let meta = json!({"workspace": "T1", "channel": "C1", "ts": "1.0", "from_me": true});
    let schema = c6_head::claims::kind_schema(&head.state, "chat").await.unwrap();
    assert!(c6_head::schema::validate(&schema, &meta).is_err(), "refused before");

    c6_head::claims::seed(pool).await.unwrap();
    let schema = c6_head::claims::kind_schema(&head.state, "chat").await.unwrap();
    assert!(c6_head::schema::validate(&schema, &meta).is_ok(), "{schema}");
    assert_eq!(schema["properties"]["user_name"]["description"], "Who, as you put it", "your edit kept");
}
