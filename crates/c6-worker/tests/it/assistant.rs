//! The assistant (docs/assistant.md): it runs in the head with no worker
//! attached, finds operations, asks for what it needs with a form, keeps
//! secrets out of the model's sight (they go to you as a card), and waits
//! for your approval before destructive operations. The catalog also
//! serves the CLI.

use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Session, StreamMsg};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use serde_json::{Value, json};

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
async fn assistant_discovers_asks_acts_and_asks_first() {
    let mock = MockLlm::start().await;
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
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let old: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "old", "base_url": "http://127.0.0.1:9/v1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The assistant's model.
    let put =
        http.put(format!("{api}/settings/assistant")).json(&json!({"model": {"connection": "m", "model": "mock-1"}})).send().await.unwrap();
    assert!(put.status().is_success());
    let bad =
        http.put(format!("{api}/settings/assistant")).json(&json!({"model": {"connection": "nope", "model": "x"}})).send().await.unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // The head's own worker is not a worker you see or start sessions on.
    let workers: Vec<Value> = http.get(format!("{api}/workers")).send().await.unwrap().json().await.unwrap();
    assert!(workers.is_empty(), "{workers:?}");
    let head_id = head.state.assistant.head_id().unwrap();
    let r = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": head_id.0, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    // "Create an enrollment token for a new worker": it finds the
    // operation, asks for the name, makes the token, and the token goes to
    // you, not to the model.
    mock.push(Reply::ToolCalls(vec![("c6_discover".into(), r#"{"query":"enrollment token for a new worker"}"#.into())]));
    mock.push(Reply::ToolCalls(vec![(
        "c6_ask".into(),
        r#"{"title":"New worker","fields":[{"name":"name","label":"Worker name","type":"text","required":true},{"name":"connection","type":"choice","choices_from":{"op":"connections.list"}}]}"#.into(),
    )]));
    mock.push(Reply::ToolCalls(vec![(
        "c6_call".into(),
        r#"{"op":"workers.enrollments.create","input":{"body":{"name":"build-1"}}}"#.into(),
    )]));
    mock.push(Reply::Text("Done: the token is in the card above. On the new machine run c6w enroll <token>.".into()));
    let s: Session = http
        .post(format!("{api}/assistant"))
        .json(&json!({"prompt": "create an enrollment token for a new worker", "context": "Workers"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s.labels, vec!["assistant".to_owned()]);
    let mut live = head.state.hub.subscribe(&s.id);
    let events = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/sessions/{id}/events?after=0")).send().await.unwrap().json::<Vec<Event>>().await.unwrap() }
    };
    // The form, with your connections as choices.
    let (call_id, fields) = eventually("a question", async || {
        events(s.id.clone()).await.into_iter().find_map(|e| match e.kind {
            EventKind::Question { call_id, fields, state, .. } if state == "asked" => Some((call_id, fields)),
            _ => None,
        })
    })
    .await;
    assert_eq!(fields[0].name, "name");
    assert_eq!(fields[1].r#type, "choice");
    let names: Vec<&str> = fields[1].choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(names, vec!["m", "old"]);
    let r = http
        .post(format!("{api}/sessions/{}/answers/{call_id}", s.id))
        .json(&json!({"values": {"name": "build-1", "connection": conn.id}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    // The secret card, live.
    let card = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(StreamMsg::Secret { title, value, note, .. }) = live.recv().await {
                return (title, value, note);
            }
        }
    })
    .await
    .expect("a secret card");
    assert_eq!(card.0, "Enrollment token");
    assert!(card.1.starts_with("c6e1."), "{}", card.1);
    assert!(card.2.contains("c6w enroll"));
    let done = eventually("the turn's end", async || {
        let ev = events(s.id.clone()).await;
        ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { .. })).then_some(ev)
    })
    .await;
    let log = serde_json::to_string(&done).unwrap();
    assert!(!log.contains(&card.1), "the token is not in the log");
    assert!(log.contains("[shown to the user]"));
    assert!(log.contains("\"name\":\"build-1\""), "the answer is logged");
    // The model saw the discovery result and the answer, not the token.
    let requests = mock.requests();
    let sent = serde_json::to_string(&requests).unwrap();
    assert!(!sent.contains(&card.1));
    assert!(requests[1].to_string().contains("workers.enrollments.create"), "discovery found it");
    assert!(requests[0].to_string().contains("The user is looking at: Workers"));

    // A destructive operation waits for your approval.
    mock.push(Reply::ToolCalls(vec![("c6_call".into(), format!(r#"{{"op":"connections.delete","input":{{"id":"{}"}}}}"#, old.id))]));
    mock.push(Reply::Text("Deleted the old connection.".into()));
    let r =
        http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "delete the old connection"})).send().await.unwrap();
    assert!(r.status().is_success());
    let call_id = eventually("an approval request", async || {
        events(s.id.clone()).await.into_iter().find_map(|e| match e.kind {
            EventKind::Approval { call_id, state, tool, input, .. } if state == "requested" => {
                assert_eq!(tool, "c6_call");
                assert!(input.contains("connections.delete"));
                Some(call_id)
            }
            _ => None,
        })
    })
    .await;
    let still: Vec<Connection> = http.get(format!("{api}/connections")).send().await.unwrap().json().await.unwrap();
    assert_eq!(still.len(), 2, "nothing deleted before you approve");
    let r = http.post(format!("{api}/sessions/{}/approvals/{call_id}", s.id)).json(&json!({"allow": true})).send().await.unwrap();
    assert!(r.status().is_success());
    eventually("the connection deleted", async || {
        let c: Vec<Connection> = http.get(format!("{api}/connections")).send().await.ok()?.json().await.ok()?;
        (c.len() == 1).then_some(())
    })
    .await;

    // The catalog, for the CLI: destructive operations need confirming.
    let catalog: Value = http.get(format!("{api}/catalog")).send().await.unwrap().json().await.unwrap();
    assert!(catalog["operations"].as_array().unwrap().len() > 100);
    let r =
        http.post(format!("{api}/catalog/call")).json(&json!({"op": "connections.delete", "input": {"id": conn.id}})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
    let r = http.post(format!("{api}/catalog/call")).json(&json!({"op": "connections.list"})).send().await.unwrap();
    let list: Value = r.json().await.unwrap();
    assert_eq!(list[0]["name"], "m");
}
