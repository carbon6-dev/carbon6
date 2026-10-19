//! Decisions (docs/os.md §4.2): a tool call held for approval opens one,
//! answered either in Decisions (the call runs) or in its session (the
//! decision follows); a workflow's approve step is one; an agent asks with
//! decision_request without stopping and gets the answer as a message, also
//! answerable from a notification's link; answers are checked; a deadline
//! takes the default answer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api::{Connection, Decision, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

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

/// A stand-in for a webhook channel: records what it receives.
async fn receiver() -> (String, Arc<Mutex<Vec<Value>>>) {
    let got: Arc<Mutex<Vec<Value>>> = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
            let g = g.clone();
            async move {
                g.lock().unwrap().push(v);
                "ok"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

#[tokio::test]
async fn decisions_answer_what_waits() {
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
    let base = format!("http://127.0.0.1:{}", head.ui_addrs[0].port());
    let api = format!("{base}/api/v1");
    let http = reqwest::Client::new();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
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
    let (hook, got) = receiver().await;
    put("settings/public_url", json!({"url": base})).await;
    put("settings/channels", json!([{"name": "phone", "kind": "webhook", "url": hook}])).await;
    put("settings/policies", json!([{"name": "ask on deploy", "tools": "Bash", "pattern": "deploy", "action": "ask"}])).await;
    let open = || {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/decisions")).send().await.unwrap().json::<Vec<Decision>>().await.unwrap() }
    };
    let answer = |id: i64, body: Value| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.post(format!("{api}/decisions/{id}/answer")).json(&body).send().await.unwrap() }
    };
    let events = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json::<Vec<Event>>().await.unwrap() }
    };
    let start = |prompt: &'static str| {
        let (http, api, conn) = (http.clone(), api.clone(), conn.id.clone());
        async move {
            http.post(format!("{api}/sessions"))
                .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn, "model": "m", "prompt": prompt}))
                .send()
                .await
                .unwrap()
                .json::<Session>()
                .await
                .unwrap()
        }
    };

    // 1. A held tool call is a decision; answered here, the call runs.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo deploy-ran"}"#.into())]));
    mock.push(Reply::Text("Deployed.".into()));
    let s1 = start("deploy it").await;
    let d = eventually("the approval decision", async || open().await.into_iter().find(|d| d.kind == "approve")).await;
    assert!(d.waiting && d.title == "Allow bash?" && d.document.contains("echo deploy-ran"), "{d:?}");
    // Not among the session's own decisions: its log shows the approval.
    let of_s1: Vec<Decision> = http.get(format!("{api}/sessions/{}/decisions", s1.id)).send().await.unwrap().json().await.unwrap();
    assert!(of_s1.is_empty(), "{of_s1:?}");
    assert_eq!(d.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), vec!["Allow", "Deny"]);
    assert_eq!(d.default_answer.as_deref(), Some("Deny"));
    let r = answer(d.id, json!({"option": "Maybe"})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = http.patch(format!("{api}/decisions/{}", d.id)).json(&json!({"state": "dismissed"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "something waits on it");
    assert!(answer(d.id, json!({"option": "Allow"})).await.status().is_success());
    eventually("the call ran", async || {
        events(s1.id.clone())
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("deploy-ran")))
            .then_some(())
    })
    .await;
    let done: Decision = http.get(format!("{api}/decisions/{}", d.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        (done.state.as_str(), done.answered_via.as_str(), done.answer.unwrap()["option"].as_str()),
        ("answered", "carbon6", Some("Allow"))
    );

    // 2. Answered in its session instead: the decision follows.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo deploy-two"}"#.into())]));
    mock.push(Reply::Text("ok".into()));
    let s2 = start("deploy again").await;
    let d2 = eventually("the second approval", async || open().await.into_iter().find(|d| d.kind == "approve")).await;
    let call = d2.context["call_id"].as_str().unwrap().to_owned();
    let r = http.post(format!("{api}/sessions/{}/approvals/{call}", s2.id)).json(&json!({"allow": false})).send().await.unwrap();
    assert!(r.status().is_success());
    let settled = eventually("the decision settled", async || {
        let d: Decision = http.get(format!("{api}/decisions/{}", d2.id)).send().await.ok()?.json().await.ok()?;
        (d.state == "answered").then_some(d)
    })
    .await;
    assert_eq!((settled.answered_via.as_str(), settled.answer.unwrap()["option"].as_str()), ("the session", Some("Deny")));

    // 3. An agent asks without stopping; the answer comes back as a message.
    mock.push(Reply::ToolCalls(vec![(
        "decision_request".into(),
        json!({"title": "Which region for staging?", "document": "I need a region; using **us-east** meanwhile.",
               "options": [{"label": "us-east", "does": "keep it"}, {"label": "eu-west", "does": "move it"}], "priority": "high"})
        .to_string(),
    )]));
    mock.push(Reply::Text("Asked; carrying on with us-east.".into()));
    mock.push(Reply::Text("Moving staging to eu-west.".into()));
    let s3 = start("set up staging").await;
    let d3 = eventually("the agent's question", async || open().await.into_iter().find(|d| d.title == "Which region for staging?")).await;
    assert!(d3.waiting && d3.kind == "choose" && d3.source == format!("session:{}", s3.id), "{d3:?}");
    // The session shows it, to answer there.
    let of_s3: Vec<Decision> = http.get(format!("{api}/sessions/{}/decisions", s3.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(of_s3.iter().map(|d| d.id).collect::<Vec<_>>(), vec![d3.id]);
    // The agent heard at once that it was asked (its next request).
    eventually("the tool's reply", async || {
        mock.requests().iter().any(|r| r["messages"].to_string().contains(&format!("decision #{} asked; carry on", d3.id))).then_some(())
    })
    .await;
    // The phone got it, with a link that answers it.
    let note = eventually("the notification", async || got.lock().unwrap().iter().find(|n| n["event"] == "decision").cloned()).await;
    let link = note["approve_url"].as_str().expect("a link to answer").to_owned();
    let page = http.get(&link).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Which region for staging?") && page.contains("eu-west"), "{page}");
    let r = http.post(&link).form(&[("option", "eu-west")]).send().await.unwrap().text().await.unwrap();
    assert!(r.contains("Answered: eu-west"), "{r}");
    eventually("the answer in the session", async || {
        mock.requests()
            .iter()
            .any(|r| r["messages"].to_string().contains(&format!("Your question was answered (decision #{}", d3.id)))
            .then_some(())
    })
    .await;
    let asked: Decision = http.get(format!("{api}/decisions/{}", d3.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!((asked.state.as_str(), asked.answered_via.as_str()), ("answered", "a notification link"));
    assert!(http.post(&link).form(&[("option", "us-east")]).send().await.unwrap().text().await.unwrap().contains("already answered"));

    // 4. Forms are checked; a deadline takes the default answer.
    let f: Decision = http
        .post(format!("{api}/decisions"))
        .json(&json!({"kind": "provide", "title": "Staging URL?", "form": [{"name": "url", "label": "URL", "required": true}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(f.id, json!({})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(answer(f.id, json!({"values": {"url": "x", "other": 1}})).await.status(), StatusCode::BAD_REQUEST);
    assert!(answer(f.id, json!({"values": {"url": "https://staging.example"}})).await.status().is_success());
    let late: Decision = http
        .post(format!("{api}/decisions"))
        .json(&json!({"title": "Keep the old cluster?", "options": [{"label": "keep"}, {"label": "drop"}], "deadline": 1, "default_answer": "keep"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    c6_head::decisions::sweep(&head.state).await;
    let late: Decision = http.get(format!("{api}/decisions/{}", late.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!((late.state.as_str(), late.answer.unwrap()["option"].as_str()), ("expired", Some("keep")));
    let r = http
        .post(format!("{api}/decisions"))
        .json(&json!({"title": "x", "options": [{"label": "a"}], "default_answer": "b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "the default must be an option");

    stop.cancel();
    head.shutdown().await.unwrap();
}
