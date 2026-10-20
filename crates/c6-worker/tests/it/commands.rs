//! Slash commands: a custom command (from the head's settings) expands for the model, /compact
//! summarizes on request, and a session exports as Markdown.

use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn slash_commands_compact_and_export() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let p = project.path();

    // Commands live in the head's settings; files are not read.
    std::fs::create_dir_all(p.join(".claude/commands")).unwrap();
    std::fs::write(p.join(".claude/commands/fromfile.md"), "never used").unwrap();
    let r = http.put(format!("{api}/settings/commands")).json(&json!([{"name": "bad name", "body": "x"}])).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    let r = http
        .put(format!("{api}/settings/commands"))
        .json(&json!([
            {"name": "greet", "description": "Say hello", "argument_hint": "<name>", "body": "Say hello to $ARGUMENTS in French."},
            {"name": "off", "body": "disabled", "enabled": false}
        ]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let wid = worker.identity.meta.worker_id.0;

    // The composer's list: built-ins and the enabled custom ones.
    let list: serde_json::Value = http.get(format!("{api}/commands")).send().await.unwrap().json().await.unwrap();
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["clear", "compact", "export", "greet", "init", "review"]);
    assert_eq!(list[3]["argument_hint"], "<name>");

    mock.push(Reply::Text("Bonjour, Ada !".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m", "prompt": "/greet Ada"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("first turn", async || (mock.requests().len() == 1).then_some(())).await;
    let first = mock.requests()[0]["messages"].to_string();
    assert!(first.contains("Say hello to Ada in French.") && !first.contains("/greet"), "{first}");

    let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{}/events", s.id)).send().await.unwrap().json().await.unwrap() };
    let ended = async |n: usize| {
        eventually("turn end", async || {
            let evs = events().await;
            (evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() >= n).then_some(evs)
        })
        .await
    };
    let evs = ended(1).await;
    assert!(
        evs.iter()
            .any(|e| matches!(&e.kind, EventKind::User { command, prompt, .. } if command == "greet" && prompt.contains("Ada in French")))
    );

    // /compact with a focus: one request, which asks for the summary.
    mock.push(Reply::Text("SUMMARY: greeted Ada.".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "/compact the greeting"})).send().await.unwrap();
    let evs = ended(2).await;
    let req = mock.requests()[1]["messages"].to_string();
    assert!(req.contains("The user asked to compact") && req.contains("focus on: the greeting"), "{req}");
    assert!(evs.iter().any(|e| matches!(&e.kind, EventKind::Compaction { summary, .. } if summary == "SUMMARY: greeted Ada.")));

    // The next turn starts from the summary.
    mock.push(Reply::Text("ok".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "thanks"})).send().await.unwrap();
    ended(3).await;
    let req = mock.requests()[2]["messages"].to_string();
    assert!(req.contains("SUMMARY: greeted Ada.") && req.contains("thanks"), "{req}");
    assert!(!req.contains("Bonjour") && !req.contains("/compact"), "{req}");

    // Export: the conversation as Markdown.
    let r = http.get(format!("{api}/sessions/{}/export", s.id)).send().await.unwrap();
    assert!(r.headers()["content-disposition"].to_str().unwrap().contains(".md"));
    let md = r.text().await.unwrap();
    assert!(md.contains("## You\n\n/greet Ada") && md.contains("Expanded /greet") && md.contains("Bonjour, Ada !"), "{md}");
    assert!(md.contains("SUMMARY: greeted Ada.") && md.contains("thanks"), "{md}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
