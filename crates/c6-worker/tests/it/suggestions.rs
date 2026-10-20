//! Suggested next messages (docs/suggestions.md): once a turn finishes,
//! the model a `task:suggest` routing rule picks proposes what you'd type
//! next, at most once per turn; none after an error, while something waits
//! on you, or when they're off.

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn finished_turns_suggest_the_next_message() {
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
    let wid = worker.identity.meta.worker_id.0;
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let put = |path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { assert!(http.put(&url).json(&body).send().await.unwrap().status().is_success(), "{url}") }
    };
    // Suggestions go to a small model.
    put(
        "settings/routes",
        json!([{"name": "suggest", "applies_to": ["task:suggest"], "connection": conn.id, "model": "small", "enabled": true}]),
    )
    .await;
    let suggestion = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let r = http.get(format!("{api}/sessions/{id}/suggestion")).send().await.unwrap();
            if r.status() == reqwest::StatusCode::NO_CONTENT { None } else { Some(r.json::<Value>().await.unwrap()) }
        }
    };
    let idle_after = |id: String, turns: usize| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("the turn to end", async || {
                let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                let ended = ev.iter().filter(|e| e["type"] == "turn_ended").count();
                (d["session"]["status"] == "idle" && ended >= turns).then_some(())
            })
            .await
        }
    };
    let say = |id: String, text: &'static str| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            assert!(
                http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": text})).send().await.unwrap().status().is_success()
            )
        }
    };

    // 1. A finished turn: the small model suggests, once.
    mock.push(Reply::Text("Fixed the off-by-one in the pager.".into()));
    mock.push(Reply::Text("\"Run the tests again\"".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "fix the pager bug"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    idle_after(s.id.clone(), 1).await;
    let got = suggestion(s.id.clone()).await.expect("a suggestion");
    assert_eq!(got["text"], "Run the tests again", "{got}");
    let asked = mock.requests().last().unwrap().clone();
    assert_eq!(asked["model"], "small", "the routing rule's model");
    let text = asked["messages"].to_string();
    assert!(text.contains("fix the pager bug") && text.contains("Fixed the off-by-one"), "{text}");
    let calls = mock.requests().len();
    assert_eq!(suggestion(s.id.clone()).await, Some(got.clone()), "cached");
    assert_eq!(mock.requests().len(), calls, "no second call for the same turn");
    let seen = http
        .post(format!("{api}/sessions/{}/suggestion/seen", s.id))
        .json(&json!({"for_seq": got["for_seq"], "accepted": true}))
        .send()
        .await
        .unwrap();
    assert!(seen.status().is_success());
    // Its tokens count toward the session.
    let report: Value = http.get(format!("{api}/usage?days=1")).send().await.unwrap().json().await.unwrap();
    let small = report["by_model"].as_array().unwrap().iter().find(|r| r["key"] == "small").cloned().unwrap_or_default();
    assert_eq!(small["calls"], 1, "{report}");

    // 2. Turned off: none, and no call.
    mock.push(Reply::Text("Done.".into()));
    say(s.id.clone(), "now the footer").await;
    idle_after(s.id.clone(), 2).await;
    put("settings/suggestions", json!({"enabled": false})).await;
    let calls = mock.requests().len();
    assert_eq!(suggestion(s.id.clone()).await, None);
    assert_eq!(mock.requests().len(), calls);
    put("settings/suggestions", json!({"enabled": true})).await;

    // 3. A turn that failed: what next is yours to decide.
    mock.push(Reply::Status(400));
    say(s.id.clone(), "and the header").await;
    idle_after(s.id.clone(), 3).await;
    let calls = mock.requests().len();
    assert_eq!(suggestion(s.id.clone()).await, None);
    assert_eq!(mock.requests().len(), calls);

    // 4. Something waits on you in the session: the answer goes there.
    mock.push(Reply::Text("Which header?".into()));
    say(s.id.clone(), "try again").await;
    idle_after(s.id.clone(), 4).await;
    let r = http
        .post(format!("{api}/decisions"))
        .json(&json!({"title": "Which header?", "options": [{"label": "top"}, {"label": "side"}], "context": {"session": s.id}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let calls = mock.requests().len();
    assert_eq!(suggestion(s.id.clone()).await, None);
    assert_eq!(mock.requests().len(), calls);

    stop.cancel();
    head.shutdown().await.unwrap();
}
