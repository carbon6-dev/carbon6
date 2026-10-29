//! A requested draft belongs to the memex after its RPC returns. The
//! caller can watch it, ask twice, or leave; a failure keeps the old reply.

use c6_api::{Request, RequestDraft};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually, within};
use serde_json::json;

#[tokio::test]
async fn requested_drafts_finish_after_the_rpc_and_keep_the_old_text_on_failure() {
    let mock = MockLlm::start().await;
    let dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: dir.path().to_owned(),
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
    let response =
        http.post(format!("{api}/connections")).json(&json!({"name": "draft-model", "base_url": mock.base_url()})).send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.text().await.unwrap());
    let settings =
        async |budget| {
            let response = http.put(format!("{api}/settings/memex"))
            .json(&json!({"understanding": {"connection": "draft-model", "model": "mock-1"}, "calls_per_day": budget, "calls_at_once": 2}))
            .send().await.unwrap();
            assert!(response.status().is_success(), "{}", response.text().await.unwrap());
        };
    // First contacts are drafted only when asked. There is no background
    // work until its own phase below.
    settings(1).await;
    let m = c6_head::knowledge::get(&head.state).unwrap();
    eventually("the drafting model configured", async || m.understanding().map(|_| ())).await;
    let old = RequestDraft {
        text: "Here is my previous reply.".into(),
        target: "thread:acct:notes".into(),
        kind: "mail".into(),
        at: 7,
        ..Default::default()
    };
    sqlx::query("INSERT INTO requests (id, person, title, category, created_at, updated_at, draft) VALUES (1, 'pat@example.test', 'Please review the notes', 'first_contact', 1, 1, ?)")
        .bind(serde_json::to_string(&old).unwrap()).execute(&m.store.pool).await.unwrap();
    sqlx::query("INSERT INTO request_sources (request_id, target, kind, at, noted_at) VALUES (1, 'thread:acct:notes', 'mail', 1, 1)")
        .execute(&m.store.pool)
        .await
        .unwrap();
    let rpc = async |method: &str, id: i64| -> Request {
        let response = http.post(format!("{api}/memex/rpc")).json(&json!({"method": method, "params": {"id": id}})).send().await.unwrap();
        assert!(response.status().is_success(), "{method}: {}", response.text().await.unwrap());
        response.json().await.unwrap()
    };
    let gate = mock.push_gated(Reply::Text(json!({"text": "The revised notes look good."}).to_string()));
    let pending = within("draft RPC returns before the model finishes", rpc("memex.requests.draft", 1)).await;
    assert!(pending.drafting);
    assert_eq!(pending.draft, Some(old.clone()));
    eventually("the model has the first draft", async || (mock.requests().len() == 1).then_some(())).await;
    assert!(rpc("memex.requests.get", 1).await.drafting);
    assert!(rpc("memex.requests.draft", 1).await.drafting);
    // In-memory claims cannot become a stale mark in a newly opened memex.
    let reopened = c6_memex::Memex::open(&dir.path().join("knowledge"), Default::default()).await.unwrap();
    assert!(!c6_memex::requests::get(&reopened, 1).await.unwrap().drafting);
    reopened.store.pool.close().await;
    drop(reopened);
    gate.notify_one();
    let done = eventually("the draft stored after its caller returned", async || {
        let r = rpc("memex.requests.get", 1).await;
        (!r.drafting).then_some(r)
    })
    .await;
    let good = done.draft.unwrap();
    assert_eq!(good.text, "The revised notes look good.");
    assert!(good.error.is_none());
    assert_eq!(mock.requests().len(), 1, "the repeated RPC did not start another call");

    let gate = mock.push_gated(Reply::Text("not a JSON draft".into()));
    assert!(rpc("memex.requests.draft", 1).await.drafting);
    eventually("the model has the second draft", async || (mock.requests().len() == 2).then_some(())).await;
    assert_eq!(rpc("memex.requests.get", 1).await.draft.unwrap().text, good.text);
    gate.notify_one();
    let failed = eventually("the failure stored and the claim released", async || {
        let r = rpc("memex.requests.get", 1).await;
        (!r.drafting).then_some(r.draft.unwrap())
    })
    .await;
    assert!(failed.error.as_deref().is_some_and(|e| e.contains("no JSON")));
    assert_eq!(RequestDraft { error: None, ..failed }, good);

    // Automatic drafting shares the claim and still uses the same budget.
    sqlx::query("INSERT INTO requests (id, person, title, category, created_at, updated_at) VALUES (2, 'lee@example.test', 'Confirm the agenda', 'needs_you', 2, 2)")
        .execute(&m.store.pool).await.unwrap();
    let gate = mock.push_gated(Reply::Text(json!({"text": "The agenda works for me."}).to_string()));
    settings(3).await;
    eventually("the automatic draft started", async || {
        let r = rpc("memex.requests.get", 2).await;
        (r.drafting && mock.requests().len() == 3).then_some(())
    })
    .await;
    assert!(rpc("memex.requests.draft", 2).await.drafting);
    gate.notify_one();
    let automatic = eventually("the automatic draft stored", async || {
        let r = rpc("memex.requests.get", 2).await;
        (!r.drafting && r.draft.is_some()).then_some(r)
    })
    .await;
    assert_eq!(automatic.draft.unwrap().text, "The agenda works for me.");
    assert_eq!(mock.requests().len(), 3);
    let calls: i64 = sqlx::query_scalar("SELECT SUM(calls) FROM model_calls").fetch_one(&m.store.pool).await.unwrap();
    assert_eq!(calls, 3);
    assert_eq!(m.tasks().at_once, 2);

    // Realm deletion closes this same pool. A detached draft must stop
    // too, without waiting for the model to finish its response.
    let gate = mock.push_gated(Reply::Text(json!({"text": "This reply must not finish."}).to_string()));
    assert!(rpc("memex.requests.draft", 1).await.drafting);
    eventually("the closing draft reaches the model", async || (mock.requests().len() == 4).then_some(())).await;
    let call = c6_llm::activity::calls()
        .into_iter()
        .filter(|c| c.what == "draft" && c.about == "Please review the notes")
        .max_by_key(|c| c.id)
        .unwrap();
    assert!(call.ended_at.is_none());
    m.store.pool.close().await;
    eventually("closing the store stops the draft", async || {
        c6_llm::activity::calls().into_iter().find(|c| c.id == call.id && c.state == "stopped")
    })
    .await;
    gate.notify_one();
    head.shutdown().await.unwrap();
}
