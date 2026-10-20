//! Rewind: checkpoints before each turn, rewinding the conversation (and
//! files), then continuing from there (edit and resend).

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn rewind_restores_conversation_and_files() {
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

    let events = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json::<Vec<Event>>().await.unwrap() }
    };
    let idle_after = |id: String, turns: usize| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("turn to finish", async || {
                let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                let evs: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                let ended = evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
                (d.session.status == SessionStatus::Idle && d.queued.iter().all(|q| q.rejected.is_some()) && ended >= turns).then_some(evs)
            })
            .await
        }
    };

    // Turn 1 writes v1; turn 2 writes v2 and a new file.
    mock.push(Reply::ToolCalls(vec![("write".into(), r#"{"path":"notes.txt","content":"v1"}"#.into())]));
    mock.push(Reply::Text("done one".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": p.display().to_string(),
                      "connection_id": conn.id, "model": "m", "prompt": "write version one"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    idle_after(s.id.clone(), 1).await;
    mock.push(Reply::ToolCalls(vec![
        ("write".into(), r#"{"path":"notes.txt","content":"v2"}"#.into()),
        ("write".into(), r#"{"path":"extra.txt","content":"new"}"#.into()),
    ]));
    mock.push(Reply::Text("done two".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "write version two"})).send().await.unwrap();
    let evs = idle_after(s.id.clone(), 2).await;
    assert_eq!(std::fs::read_to_string(p.join("notes.txt")).unwrap(), "v2");
    assert!(
        evs.iter().filter(|e| matches!(e.kind, EventKind::Checkpoint { ref error } if error.is_empty())).count() == 2,
        "a checkpoint per turn"
    );
    let second = evs.iter().find(|e| matches!(&e.kind, EventKind::User { text, .. } if text == "write version two")).unwrap().seq;

    // Rewinding to an event that is not one of your messages is refused.
    let r = http.post(format!("{api}/sessions/{}/rewind", s.id)).json(&json!({"to_seq": second + 1})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    // Rewind to before the second message, restoring files; then send an
    // edited message (edit and resend).
    let r =
        http.post(format!("{api}/sessions/{}/rewind", s.id)).json(&json!({"to_seq": second, "restore_files": true})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);
    mock.push(Reply::Text("done three".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "write version three"})).send().await.unwrap();
    let evs = idle_after(s.id.clone(), 3).await;
    let rewound = evs.iter().find_map(|e| match &e.kind {
        EventKind::Rewound { to_seq, restored_files, restore_error } => Some((*to_seq, *restored_files, restore_error.clone())),
        _ => None,
    });
    assert_eq!(rewound, Some((second, true, String::new())));
    assert_eq!(std::fs::read_to_string(p.join("notes.txt")).unwrap(), "v1", "files are back to before the second message");
    assert!(!p.join("extra.txt").exists(), "a file created after it is removed");
    // The model no longer sees the rewound turn.
    let last: Value = mock.requests().last().unwrap().clone();
    let msgs = last["messages"].to_string();
    assert!(msgs.contains("write version one") && msgs.contains("done one") && msgs.contains("write version three"), "{msgs}");
    assert!(!msgs.contains("write version two") && !msgs.contains("done two"), "{msgs}");
    // The rewound events stay in the log, for the record.
    assert!(events(s.id.clone()).await.iter().any(|e| matches!(&e.kind, EventKind::User { text, .. } if text == "write version two")));

    stop.cancel();
    head.shutdown().await.unwrap();
}
