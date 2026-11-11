//! A message you send while the agent works is with the agent, not just
//! queued: the queue says so until the agent takes it at its next step
//! (docs/background.md §4).

use c6_api::{Event, EventKind, Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn a_message_sent_mid_turn_is_with_the_agent() {
    mid_turn_input(false).await;
}

#[tokio::test]
async fn api_queue_edits_replace_the_original_browser_origin() {
    mid_turn_input(true).await;
}

async fn mid_turn_input(edit_via_api: bool) {
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
    let worker = crate::open_worker(state.path()).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(c6_worker::conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let detail =
        async |id: &str| -> Option<SessionDetail> { http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok() };

    // A turn that holds after its first words.
    let gate = mock.push_gated(Reply::Text("Working on the first.".into()));
    mock.push(Reply::Text("Both done.".into()));
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(), "connection_id": conn["id"], "model": "m", "prompt": "first"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    eventually("the turn to be running", async || (mock.requests().len() == 1).then_some(())).await;

    // Sent now: the agent has it, and reads it at its next step.
    let login = head.state.store.create_login_link().await.unwrap();
    let signed_in = http.post(format!("{api}/auth/link")).json(&json!({"token": login})).send().await.unwrap();
    let cookie = signed_in.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap();
    let r = http
        .post(format!("{api}/sessions/{id}/messages"))
        .header(reqwest::header::COOKIE, cookie)
        .header("x-c6-realm", "home")
        .json(&json!({"text": "second"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let queued = eventually("the message to be with the agent", async || {
        let d = detail(&id).await?;
        (d.queued.len() == 1 && d.queued[0].with_agent).then_some(d.queued)
    })
    .await;
    assert_eq!(queued[0].text, "second");
    let expected = if edit_via_api {
        let token = head.state.store.create_api_token("queue-editor", None).await.unwrap().token.unwrap();
        let r = http
            .post(format!("{api}/catalog/call"))
            .bearer_auth(&token)
            .json(&json!({"op": "sessions.queue.edit", "input": {"id": id, "command_id": queued[0].command_id,
                "body": {"text": "Don't do that: edited by API", "human_origin": true}}}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        "Don't do that: edited by API"
    } else {
        "second"
    };

    // The step ends: the agent takes it, and the queue empties.
    gate.notify_one();
    eventually("the session to be idle with nothing queued", async || {
        let d = detail(&id).await?;
        (d.session.status == SessionStatus::Idle && d.queued.is_empty()).then_some(())
    })
    .await;
    assert_eq!(mock.requests().len(), 2);
    assert!(mock.requests()[1]["messages"].to_string().contains(expected), "it joined the turn");
    let events = worker.journal.events_after(&id, 0, u32::MAX).await.unwrap();
    let input = events
        .iter()
        .find_map(|e| match &e.kind {
            Some(c6_proto::session_event::Kind::User(u)) if u.text == expected => Some(u),
            _ => None,
        })
        .expect("the consumed input is logged");
    assert_eq!(input.human_origin, !edit_via_api);
    assert_eq!(input.api_origin.as_ref().map(|o| o.token_name.as_str()), edit_via_api.then_some("queue-editor"));
    stop.cancel();
}

#[tokio::test]
async fn archived_sessions_stay_readable_but_reject_new_messages_until_unarchived() {
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
    let worker = crate::open_worker(state.path()).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(c6_worker::conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let detail = async |id: &str| -> SessionDetail {
        http.get(format!("{api}/sessions/{id}")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    };
    let events = async |id: &str| -> Vec<Event> {
        http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    };
    let listed = async || -> Vec<Session> {
        http.get(format!("{api}/sessions")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    };

    mock.push(Reply::Text("First answer.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(), "connection_id": conn["id"], "model": "m", "prompt": "first"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!s.archived);
    eventually("the first turn to finish", async || {
        let d = detail(&s.id).await;
        (d.session.status == SessionStatus::Idle && d.session.end_reason == Some(TurnEndReason::Completed) && d.queued.is_empty())
            .then_some(())
    })
    .await;
    assert!(listed().await.iter().any(|row| row.id == s.id && !row.archived));

    http.patch(format!("{api}/sessions/{}", s.id)).json(&json!({"archived": true})).send().await.unwrap().error_for_status().unwrap();
    let archived = detail(&s.id).await;
    assert!(archived.session.archived, "archived sessions are still fetched by id");
    assert!(archived.queued.is_empty());
    assert!(!listed().await.iter().any(|row| row.id == s.id));
    let before = events(&s.id).await;
    let requests = mock.requests().len();
    let commands: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM commands WHERE session_id = ?")
        .bind(&s.id)
        .fetch_one(head.state.store.pool())
        .await
        .unwrap();

    // Send now must not interrupt or queue anything either.
    for send_now in [false, true] {
        let r = http
            .post(format!("{api}/sessions/{}/messages", s.id))
            .json(&json!({"text": "must not be sent", "send_now": send_now}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
        let error: c6_api::ApiError = r.json().await.unwrap();
        assert!(error.error.contains("archived") && error.error.contains("unarchive"), "{}", error.error);
    }
    let after = detail(&s.id).await;
    assert!(after.session.archived);
    assert_eq!(after.session.status, archived.session.status);
    assert_eq!(after.session.last_seq, archived.session.last_seq);
    assert!(after.queued.is_empty());
    assert_eq!(events(&s.id).await, before);
    assert_eq!(mock.requests().len(), requests);
    let after_commands: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM commands WHERE session_id = ?")
        .bind(&s.id)
        .fetch_one(head.state.store.pool())
        .await
        .unwrap();
    assert_eq!(after_commands, commands, "rejected sends insert no commands");

    http.patch(format!("{api}/sessions/{}", s.id)).json(&json!({"archived": false})).send().await.unwrap().error_for_status().unwrap();
    assert!(!detail(&s.id).await.session.archived);
    assert!(listed().await.iter().any(|row| row.id == s.id && !row.archived));
    mock.push(Reply::Text("After unarchive.".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id))
        .json(&json!({"text": "after unarchive"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    eventually("the unarchived session's new turn to finish", async || {
        let d = detail(&s.id).await;
        let ends = events(&s.id).await.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
        (d.session.status == SessionStatus::Idle
            && d.session.end_reason == Some(TurnEndReason::Completed)
            && !d.session.archived
            && d.queued.is_empty()
            && ends == 2)
            .then_some(())
    })
    .await;
    assert_eq!(mock.requests().len(), requests + 1);
    assert!(mock.requests().last().unwrap()["messages"].to_string().contains("after unarchive"));
    assert!(events(&s.id).await.iter().any(|e| matches!(&e.kind, EventKind::User { text, .. } if text == "after unarchive")));
    stop.cancel();
}

/// Send now in carbon6's own loop: the step stops (the turn ends
/// interrupted, without taking the message), and the message starts the
/// next turn, as Stop and then Send did before.
#[tokio::test]
async fn send_now_stops_the_step_and_starts_the_next_turn() {
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
    let worker = crate::open_worker(state.path()).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(c6_worker::conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // A step that holds.
    let gate = mock.push_gated(Reply::Text("Working on the first.".into()));
    mock.push(Reply::Text("On the second now.".into()));
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(), "connection_id": conn["id"], "model": "m", "prompt": "first"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    eventually("the turn to be running", async || (mock.requests().len() == 1).then_some(())).await;

    let r = http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "second", "send_now": true})).send().await.unwrap();
    assert!(r.status().is_success());
    let (ends, events) = eventually("both turns to end", async || {
        let events: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
        let ends: Vec<TurnEndReason> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::TurnEnded { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect();
        (ends.len() == 2).then_some((ends, events))
    })
    .await;
    gate.notify_one();
    assert_eq!(ends, [TurnEndReason::Interrupted, TurnEndReason::Completed]);
    let messages: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::User { text, .. } => Some(text.as_str()),
            EventKind::TurnStarted { .. } => Some("|"),
            _ => None,
        })
        .collect();
    assert_eq!(messages, ["first", "|", "second", "|"], "the message is the next turn's");
    assert_eq!(mock.requests().len(), 2);
    assert!(mock.requests()[1]["messages"].to_string().contains("second"));
    stop.cancel();
}
