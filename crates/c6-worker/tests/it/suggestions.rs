//! Suggested next messages (docs/suggestions.md): once a turn finishes,
//! the model a `task:suggest` routing rule picks proposes what you'd type
//! next, at most once per turn; none after an error, while something waits
//! on you, or when they're off. A realm's own rule (set by root) picks
//! its model, never on a connection the realm may not use.

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

#[tokio::test]
async fn a_realm_s_own_rule_picks_its_suggestions_model() {
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
    let send = |method: reqwest::Method, path: String, token: Option<String>, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let mut r = http.request(method, url).json(&body);
            if let Some(t) = token {
                r = r.bearer_auth(t);
            }
            r.send().await.unwrap()
        }
    };
    use reqwest::Method as M;
    let ok = async |r: reqwest::Response| -> Value {
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    for realm in ["c6", "lab"] {
        ok(send(M::POST, "realms".into(), None, json!({"id": realm})).await).await;
    }
    let connection = async |name: &str| -> Connection {
        serde_json::from_value(
            ok(send(M::POST, "connections".into(), None, json!({"name": name, "base_url": mock.base_url()})).await).await,
        )
        .unwrap()
    };
    let m = connection("m").await;
    // A connection only lab may use.
    let lab_only = connection("lab-only").await;
    ok(send(M::PUT, format!("connections/{}/realms", lab_only.id), None, json!({"realms": ["lab"]})).await).await;

    // Root keeps suggestions off and has no rule; c6's own rules (root
    // sets them) name lab's connection first, then a small model.
    ok(send(M::PUT, "settings/suggestions".into(), None, json!({"enabled": false})).await).await;
    ok(send(
        M::PUT,
        "settings/routes?realm=c6".into(),
        None,
        json!([
            {"name": "lab's", "applies_to": ["task:suggest"], "connection": lab_only.id, "model": "lab-model", "enabled": true},
            {"name": "small", "applies_to": ["task:suggest"], "connection": m.id, "model": "small", "enabled": true},
        ]),
    )
    .await)
    .await;
    assert_eq!(ok(send(M::GET, "settings/routes".into(), None, json!(null)).await).await, json!([]), "root's own are untouched");
    // c6 may not see or change routing, but turns its suggestions on.
    let token: Value = ok(send(M::POST, "auth/tokens".into(), None, json!({"name": "c6", "realm": "c6"})).await).await;
    let c6 = token["token"].as_str().unwrap().to_owned();
    for method in [M::GET, M::PUT] {
        let r = send(method.clone(), "settings/routes".into(), Some(c6.clone()), json!([])).await;
        assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN, "{method} settings/routes as c6");
    }
    ok(send(M::PUT, "settings/suggestions".into(), Some(c6.clone()), json!({"enabled": true})).await).await;
    let root_s = ok(send(M::GET, "settings/suggestions".into(), None, json!(null)).await).await;
    assert_eq!(root_s["enabled"], false, "root's stays off: {root_s}");

    let session_in = async |realm: &str| -> Session {
        mock.push(Reply::Text("Done.".into()));
        let body = json!({"worker_id": wid, "cwd": "", "connection_id": m.id, "model": "m", "prompt": "tidy up", "realm": realm});
        let s: Session = serde_json::from_value(ok(send(M::POST, "sessions".into(), None, body).await).await).unwrap();
        eventually("the turn to end", async || {
            let d: Value = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
            let ev: Vec<Value> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
            (d["session"]["status"] == "idle" && ev.iter().any(|e| e["type"] == "turn_ended")).then_some(())
        })
        .await;
        s
    };
    let suggestion = async |id: &str| -> reqwest::Response { send(M::GET, format!("sessions/{id}/suggestion"), None, json!(null)).await };

    // 1. In c6: its own rule, passing over the connection c6 may not use.
    let s = session_in("c6").await;
    mock.push(Reply::Text("Run the tests".into()));
    let got = ok(suggestion(&s.id).await).await;
    assert_eq!(got["text"], "Run the tests", "{got}");
    assert_eq!(mock.requests().last().unwrap()["model"], "small", "c6's rule, on a connection c6 may use");

    // 2. In home: off, as root's is.
    let s = session_in("home").await;
    let calls = mock.requests().len();
    assert_eq!(suggestion(&s.id).await.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(mock.requests().len(), calls, "no call where suggestions are off");

    // 3. In lab, turned on: c6's rules don't apply; the session's own model.
    ok(send(M::PUT, "settings/suggestions?realm=lab".into(), None, json!({"enabled": true})).await).await;
    let s = session_in("lab").await;
    mock.push(Reply::Text("Ship it".into()));
    let got = ok(suggestion(&s.id).await).await;
    assert_eq!(got["text"], "Ship it", "{got}");
    assert_eq!(mock.requests().last().unwrap()["model"], "m", "lab has no rule of its own");

    // 4. Only a connection c6 may not use: the session's own model, never it.
    ok(send(
        M::PUT,
        "settings/routes?realm=c6".into(),
        None,
        json!([{"name": "lab's", "applies_to": ["task:suggest"], "connection": lab_only.id, "model": "lab-model", "enabled": true}]),
    )
    .await)
    .await;
    let s = session_in("c6").await;
    mock.push(Reply::Text("Next step".into()));
    ok(suggestion(&s.id).await).await;
    assert_eq!(mock.requests().last().unwrap()["model"], "m", "fails closed to the session's own");
    assert!(mock.requests().iter().all(|r| r["model"] != "lab-model"), "lab's connection is never used for c6");

    stop.cancel();
    head.shutdown().await.unwrap();
}
