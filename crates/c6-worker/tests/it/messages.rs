//! Messages between sessions (docs/background.md §6): an agent finds a
//! session of its realm on another worker and sends it a message; that
//! session's agent wakes with it, as a message from that session; a reply
//! beyond the chain limit is refused. Side chats are listed under their
//! sessions.

use std::path::Path;
use std::sync::Arc;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn run_worker(head: &c6_head::Head, state: &Path) -> (Arc<Worker>, CancellationToken) {
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
    let w = crate::open_worker(state).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (w, stop)
}

#[tokio::test]
async fn a_session_messages_another_on_another_worker() {
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
    let (state_a, state_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (worker_a, stop_a) = run_worker(&head, state_a.path()).await;
    let (worker_b, stop_b) = run_worker(&head, state_b.path()).await;
    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let defaults: c6_api::SubagentLimits = http.get(format!("{api}/settings/subagent-limits")).send().await.unwrap().json().await.unwrap();
    assert_eq!(defaults.max_message_hops, 4);
    assert_eq!(c6_api::SubagentLimits::default().max_message_hops, 4);
    let missing_fields: c6_api::SubagentLimits = serde_json::from_value(json!({})).unwrap();
    assert_eq!(missing_fields.max_message_hops, 4, "older settings keep the same default");
    // A chain of one: a reply to a session message is refused.
    let limits = json!({"max_depth": 3, "max_running": 16, "auto_turns_per_hour": 20, "claude_code_idle_minutes": 15, "max_message_hops": 1, "messages_per_hour": 30});
    assert!(http.put(format!("{api}/settings/subagent-limits")).json(&limits).send().await.unwrap().status().is_success());
    let events =
        async |id: &str| -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
    let idle = async |id: &str, turns: usize| {
        eventually("idle", async || {
            let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
            let ended = events(id).await.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (d.session.status == SessionStatus::Idle && d.queued.is_empty() && ended == turns).then_some(())
        })
        .await
    };
    let create = async |worker: &Worker, dir: &Path, title: &str, prompt: Option<&str>| -> String {
        let s: Value = http
            .post(format!("{api}/sessions"))
            .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": dir.display().to_string(), "connection_id": conn["id"], "model": "m", "title": title, "prompt": prompt}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        s["id"].as_str().unwrap().to_owned()
    };
    let tool_results = |evs: &[Event]| -> Vec<String> {
        evs.iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    };

    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    mock.push_about("Bob: be ready", Reply::Text("Ready.".into()));
    let b = create(&worker_b, dir_b.path(), "Bob", Some("Bob: be ready")).await;
    idle(&b, 1).await;
    // A side chat of B, which A sees under B.
    mock.push_about("Bob's side question", Reply::Text("Side answer.".into()));
    let anchor = events(&b).await.last().unwrap().seq;
    let side: Value = http
        .post(format!("{api}/sessions/{b}/side-chats"))
        .json(&json!({"anchor_seq": anchor, "connection_id": conn["id"], "model": "m", "prompt": "Bob's side question"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let side = side["id"].as_str().unwrap().to_owned();
    idle(&side, 1).await;
    let a = create(&worker_a, dir_a.path(), "Alice", None).await;

    // A finds B and asks it something.
    mock.push_about("ask the other session", Reply::ToolCalls(vec![("list_sessions".into(), "{}".into())]));
    // Once it has listed B (B's id is in the list).
    mock.push_about(
        &b,
        Reply::ToolCalls(vec![("message_session".into(), json!({"to": b, "message": "What is your status?"}).to_string())]),
    );
    mock.push_about("Sent to", Reply::Text("Asked it.".into()));
    // B wakes with it and tries to answer: one message too many.
    mock.push_about(
        "<session-message",
        Reply::ToolCalls(vec![("message_session".into(), json!({"to": a, "message": "All good."}).to_string())]),
    );
    mock.push_about("not sent", Reply::Text("Could not reply.".into()));
    http.post(format!("{api}/sessions/{a}/messages")).json(&json!({"text": "ask the other session"})).send().await.unwrap();
    idle(&a, 1).await;
    idle(&b, 2).await;

    let a_events = events(&a).await;
    let listed = &tool_results(&a_events)[0];
    assert!(listed.contains(&b) && listed.contains("\"Bob\""), "{listed}");
    assert!(!listed.contains(&a), "not itself: {listed}");
    let side_line = listed.lines().find(|l| l.contains(&side)).unwrap_or_else(|| panic!("the side chat is listed: {listed}"));
    assert!(side_line.starts_with("  - ") && side_line.contains(r#"side chat of "Bob""#), "{listed}");
    let lines: Vec<&str> = listed.lines().collect();
    let at = |id: &str| lines.iter().position(|l| l.contains(id)).unwrap();
    assert_eq!(at(&side), at(&b) + 1, "under B: {listed}");
    let b_events = events(&b).await;
    let got = b_events.iter().find_map(|e| match &e.kind {
        EventKind::User { text, command, from_session_id, from_title, .. } if command == "session-message" => {
            Some((text.clone(), from_session_id.clone(), from_title.clone()))
        }
        _ => None,
    });
    assert_eq!(got, Some(("What is your status?".into(), a.clone(), "Alice".into())));
    let woke = mock.requests().iter().map(|r| r["messages"].to_string()).find(|m| m.contains("<session-message")).unwrap();
    assert!(woke.contains(&format!(r#"from=\"{a}\""#)) && woke.contains("not from the user"), "{woke}");
    let refused = &tool_results(&b_events)[0];
    assert!(refused.contains("not sent") && refused.contains("the limit is 1"), "{refused}");
    assert!(
        !a_events.iter().any(|e| matches!(&e.kind, EventKind::User { command, .. } if command == "session-message")),
        "nothing reached A"
    );
    // B's transcript attributes it to A, not to you.
    let md = http.get(format!("{api}/sessions/{b}/export")).send().await.unwrap().text().await.unwrap();
    assert!(md.contains(r#"## ✉ From "Alice""#) && !md.contains("## You\n\nWhat is your status?"), "{md}");

    // Your direct message resets B's count: its next send is hop 1,
    // rather than a refused hop 2 in the previous chain.
    mock.push_about(
        "try again from me",
        Reply::ToolCalls(vec![("message_session".into(), json!({"to": a, "message": "A fresh chain."}).to_string())]),
    );
    mock.push_about("Sent to", Reply::Text("Sent this time.".into()));
    mock.push_about("<session-message", Reply::Text("Received the fresh chain.".into()));
    let r = http.post(format!("{api}/sessions/{b}/messages")).json(&json!({"text": "try again from me"})).send().await.unwrap();
    assert!(r.status().is_success());
    idle(&b, 3).await;
    idle(&a, 2).await;
    assert!(
        head.state
            .store
            .events_after(&a, 0, 1_000)
            .await
            .unwrap()
            .iter()
            .any(|e| { matches!(&e.kind, Some(c6_proto::session_event::Kind::User(u)) if u.text == "A fresh chain." && u.hops == 1) })
    );
    stop_a.cancel();
    stop_b.cancel();
}
