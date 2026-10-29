//! Reply boxes and sessions (docs/conversations.md §3): the context a
//! session starts with, and the drafts it writes back into a box.

use c6_head::HeadConfig;
use serde_json::{Value, json};

#[tokio::test]
async fn a_session_writes_the_draft_you_send() {
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
    // A DM from Ana, asking for something.
    let ts = format!("{}.000100", c6_core::now_ms() / 1000 - 60);
    call(
        "memex.ingest",
        json!({"kind": "chat", "source": "app:slack", "external_id": format!("T1/D1/{ts}"), "title": "@Ana · Ana",
               "text": "Ana: can you send the Q3 numbers by Friday?", "created_at": c6_core::now_ms() - 60_000,
               "meta": {"service": "slack", "workspace": "T1", "channel": "D1", "channel_name": "@Ana", "channel_kind": "dm", "ts": ts,
                        "user": "U1", "user_name": "Ana", "user_email": "ana@acme.com", "from_me": false}}),
    )
    .await;

    // A conversation's box: the messages, then the draft so far, and the
    // box the draft tool writes to.
    let prompt =
        call("memex.compose.context", json!({"for": "chat:T1:D1", "draft": "Sure, on it."})).await["prompt"].as_str().unwrap().to_owned();
    assert!(prompt.contains("can you send the Q3 numbers by Friday?"), "{prompt}");
    assert!(prompt.contains("## My draft so far\nSure, on it."), "{prompt}");
    assert!(prompt.contains("draft tool (for: chat:T1:D1)"), "{prompt}");

    // A session replaces it; the box reads who wrote it and when.
    let set = call("memex.drafts.set", json!({"for": "chat:T1:D1", "text": "Yes: Friday morning.", "made_by": "session:s1"})).await;
    assert_eq!((set["text"].as_str(), set["by"].as_str()), (Some("Yes: Friday morning."), Some("session:s1")));
    let linked = call("memex.drafts.link", json!({"for": "chat:T1:D1", "session": "s1"})).await;
    assert_eq!((linked["session"].as_str(), linked["text"].as_str()), (Some("s1"), Some("Yes: Friday morning.")));
    // Your own edits are yours.
    let mine = call("memex.drafts.set", json!({"for": "chat:T1:D1", "text": "Friday."})).await;
    assert_eq!(mine["by"], "you");
    call("memex.drafts.clear", json!({"for": "chat:T1:D1"})).await;
    let gone = call("memex.drafts.get", json!({"for": "chat:T1:D1"})).await;
    assert_eq!((gone["text"].as_str(), gone["session"].as_str()), (Some(""), Some("")));

    // The Brief's box is the request's draft.
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
    let requests = call("memex.requests.list", json!({})).await;
    let id = requests[0]["id"].as_i64().expect("a request");
    let key = format!("request:{id}");
    let prompt = call("memex.compose.context", json!({"for": key})).await["prompt"].as_str().unwrap().to_owned();
    assert!(prompt.contains("## What they need") && prompt.contains("can you send the Q3 numbers"), "{prompt}");
    call("memex.drafts.set", json!({"for": key, "text": "Sending them Friday.", "made_by": "session:s2"})).await;
    let r = call("memex.requests.get", json!({"id": id})).await;
    assert_eq!((r["draft"]["text"].as_str(), r["draft"]["by"].as_str()), (Some("Sending them Friday."), Some("session:s2")));
    assert_eq!(r["draft"]["target"], "chat:T1:D1", "in its conversation");
    let got = call("memex.drafts.get", json!({"for": key})).await;
    assert_eq!((got["text"].as_str(), got["target"].as_str()), (Some("Sending them Friday."), Some("chat:T1:D1")));
    head.shutdown().await.unwrap();
}
