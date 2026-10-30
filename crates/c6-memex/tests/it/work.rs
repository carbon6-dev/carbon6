//! Work and the memex (docs/work.md §3): a request attached to work makes
//! it need you while you owe the reply, and moves it when it changes.

use c6_head::HeadConfig;
use c6_head::realms::Caller;
use serde_json::{Value, json};

#[tokio::test]
async fn a_request_you_owe_is_work_that_needs_you() {
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
    let st = head.state.clone();
    let call = |method: &'static str, params: Value| {
        let st = st.clone();
        async move { c6_head::memex::call_in(&st, "home", method, params).await.unwrap() }
    };
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    let found = c6_memex::requests::Found {
        target: "chat:T1:D1",
        kind: "chat",
        category: "needs_you",
        reason: "asks you for the numbers",
        asks: "the Q3 numbers",
        due: None,
        person: "ana@acme.com",
        person_name: "Ana",
        title: "Send the Q3 numbers",
        at: c6_core::now_ms(),
        urgent: false,
    };
    c6_memex::requests::follow(&m, &found, None).await.unwrap();
    let id = call("memex.requests.list", json!({})).await[0]["id"].as_i64().expect("a request");
    // Read in a batch, as work lists them; one that's gone is left out.
    let some = call("memex.requests.some", json!({"ids": [id, 9999]})).await;
    assert_eq!(some.as_array().map(Vec::len), Some(1), "{some}");
    assert_eq!((some[0]["person_name"].as_str(), some[0]["status"].as_str()), (Some("Ana"), Some("open")));

    let you = Caller::in_realm("home");
    let new = c6_api::work::WorkNew {
        title: "Q3 numbers for Ana".into(),
        attach: vec![c6_api::work::WorkRef { reference: format!("request:{id}"), title: String::new() }],
        ..Default::default()
    };
    let w = c6_head::work::create(&head.state, &you, &new, "you").await.unwrap();
    assert_eq!(w.state, "needs_you", "{}", w.why);
    assert_eq!(w.why, "You owe a reply: Ana: the Q3 numbers");
    assert_eq!(c6_head::work::of(&head.state, &format!("request:{id}"), &you).await.unwrap()[0].id, w.id);

    // Answered: it moved, and nothing is owed.
    let before = w.fields.moved_at;
    call("memex.requests.set", json!({"id": id, "status": "done"})).await;
    let w = c6_head::work::get(&head.state, &w.id, &you).await.unwrap();
    assert_eq!(w.live[0].state, "done");
    assert_eq!(w.state, "open", "{}", w.why);
    assert!(w.fields.moved_at >= before);
    // Snoozed in the Brief: waiting.
    call("memex.requests.set", json!({"id": id, "status": "snoozed", "until": c6_core::now_ms() + 3_600_000})).await;
    assert_eq!(c6_head::work::get(&head.state, &w.id, &you).await.unwrap().state, "waiting");
}
