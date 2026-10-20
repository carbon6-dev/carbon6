//! Policies: on env:prod, rm -rf is refused and other commands wait for
//! the user's approval (allow runs it, deny tells the agent).

use c6_api::{Connection, Event, EventKind, Session, SessionDetail};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn policies_deny_and_ask() {
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

    let put = |v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/policies"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put(json!([{"name": "x", "action": "maybe"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put(json!([{"name": "x", "action": "ask", "pattern": "(unclosed"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert!(put(json!([
        {"name": "No rm -rf on prod", "applies_to": ["env:prod"], "tools": "Bash", "pattern": "rm\\s+-rf", "action": "deny", "message": "delete specific files instead"},
        {"name": "Ask on prod", "applies_to": ["env:prod"], "tools": "Bash|Write|Edit", "action": "ask"}
    ]))
    .await
    .is_success());

    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![
        ("bash".into(), r#"{"command":"rm -rf /tmp/whatever"}"#.into()),
        ("bash".into(), r#"{"command":"echo restarted-nginx"}"#.into()),
    ]));
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo dropped-the-db"}"#.into())]));
    mock.push(Reply::Text("Done.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "fix prod", "labels": ["env:prod"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let requested = |n: usize| {
        let (http, api, id) = (http.clone(), api.clone(), s.id.clone());
        async move {
            eventually("an approval request", async || {
                let evs: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                let reqs: Vec<(String, String)> = evs
                    .iter()
                    .filter_map(|e| match &e.kind {
                        EventKind::Approval { call_id, state, input, .. } if state == "requested" => Some((call_id.clone(), input.clone())),
                        _ => None,
                    })
                    .collect();
                (reqs.len() >= n).then(|| reqs[n - 1].clone())
            })
            .await
        }
    };
    // The restart waits; the session says so.
    let (call, input) = requested(1).await;
    assert!(input.contains("restarted-nginx"), "{input}");
    let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.unwrap().json().await.unwrap();
    assert!(d.session.awaiting_approval);
    assert_eq!(mock.requests().len(), 1, "nothing goes on while it waits");
    let r = http.post(format!("{api}/sessions/{}/approvals/{call}", s.id)).json(&json!({"allow": true})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT);
    eventually("the next step", async || (mock.requests().len() == 2).then_some(())).await;
    let second = mock.requests()[1]["messages"].to_string();
    assert!(second.contains("Denied by policy \\\"No rm -rf on prod\\\": delete specific files instead"), "{second}");
    assert!(second.contains("restarted-nginx\\n"), "allowed, it ran: {second}");

    // Denied: the agent is told.
    let (call, _) = requested(2).await;
    http.post(format!("{api}/sessions/{}/approvals/{call}", s.id)).json(&json!({"allow": false})).send().await.unwrap();
    eventually("the last step", async || (mock.requests().len() == 3).then_some(())).await;
    let third = mock.requests()[2]["messages"].to_string();
    assert!(third.contains("Not run: you denied it") && !third.contains("dropped-the-db\\n"), "{third}");
    // The denial reaches the head's log with the worker's next sync.
    let states = eventually("the approvals in the log", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        let states: Vec<String> = evs
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Approval { state, .. } => Some(state.clone()),
                _ => None,
            })
            .collect();
        (states.len() >= 4).then_some(states)
    })
    .await;
    assert_eq!(states, ["requested", "allowed", "requested", "denied"]);

    stop.cancel();
    head.shutdown().await.unwrap();
}
