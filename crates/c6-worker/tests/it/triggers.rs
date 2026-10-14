//! Triggers (docs/os.md §4.3): records, decisions, sessions and webhooks
//! fire them; their filters pick events by JSONPath; their actions write
//! records, ask, notify or start templates with the event's values; a chain
//! of them is limited in depth and each in runs an hour; a schedule's
//! event carries the open decisions, for digests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api::{Automation, AutomationRun, Connection, Decision};
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
async fn triggers_react_to_events() {
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
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let (hook, got) = receiver().await;
    ok(send(M::PUT, "settings/channels", json!([{"name": "phone", "kind": "webhook", "url": hook}])).await).await;
    let collection = |name: &'static str, props: Value| async move {
        let required: Vec<&String> = props.as_object().unwrap().keys().collect();
        let schema = json!({"type": "object", "required": required, "properties": props});
        let r = send(M::POST, "collections", json!({"name": name, "schema": schema})).await;
        assert!(r.status().is_success(), "{name}");
    };
    collection("deals", json!({"name": {"type": "string"}, "stage": {"type": "string"}})).await;
    collection("invoices", json!({"client": {"type": "string"}, "note": {"type": "string"}})).await;
    let records = |name: &'static str| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            http.get(format!("{api}/collections/{name}/records")).send().await.unwrap().json::<Value>().await.unwrap()["records"].clone()
        }
    };
    let trigger = |body: Value| async move {
        let r = send(M::POST, "triggers", body).await;
        let s = r.status();
        let v: Value = r.json().await.unwrap();
        assert!(s.is_success(), "{v}");
        serde_json::from_value::<Automation>(v).unwrap()
    };

    // Checked: events, actions and filters.
    for bad in [
        json!({"name": "x", "trigger": "record", "event_params": {"collection": "nowhere"}, "action": "notify", "action_params": {"title": "t"}}),
        json!({"name": "x", "trigger": "record", "event_params": {"collection": "deals", "ops": ["exploded"]}, "action": "notify", "action_params": {"title": "t"}}),
        json!({"name": "x", "trigger": "decision", "action": "write", "action_params": {"collection": "invoices"}}),
        json!({"name": "x", "trigger": "decision", "action": "dance"}),
        json!({"name": "x", "trigger": "decision", "filter": {"stage": "won"}, "action": "notify", "action_params": {"title": "t"}}),
    ] {
        assert_eq!(send(M::POST, "triggers", bad.clone()).await.status(), StatusCode::BAD_REQUEST, "{bad}");
    }

    // 1. A deal won → an invoice record → a decision to send it → answered
    //    "Send" → a notification. Each step is a trigger, mapped from the event.
    let won = trigger(json!({
        "name": "won deals", "trigger": "record", "event_params": {"collection": "deals", "ops": ["updated"]},
        "filter": {"$.record.fields.stage": "won", "$.changed": {"$contains": "stage"}},
        "action": "write", "action_params": {"collection": "invoices", "fields": {"client": "$.record.fields.name", "note": "won {{$.record.fields.name}} from {{$.before.stage}}"}}
    }))
    .await;
    let ask = trigger(json!({
        "name": "new invoices", "trigger": "record", "event_params": {"collection": "invoices", "ops": ["created"]},
        "action": "ask", "action_params": {"title": "Send the invoice to {{$.record.fields.client}}?", "document": "{{$.record.fields.note}}",
                                            "options": [{"label": "Send"}, {"label": "Hold"}]}
    }))
    .await;
    trigger(json!({
        "name": "sent", "trigger": "decision", "event_params": {"ops": ["answered"]}, "filter": {"$.option": "Send"},
        "action": "notify", "action_params": {"title": "Sending: {{$.decision.title}}", "message": "answered via {{$.decision.answered_via}}"}
    }))
    .await;
    let deal = ok(send(M::POST, "collections/deals/records", json!({"fields": {"name": "Acme", "stage": "talking"}})).await).await;
    assert_eq!(records("invoices").await.as_array().unwrap().len(), 0, "created, not updated");
    let id = deal["id"].as_str().unwrap();
    ok(send(M::PUT, &format!("collections/deals/records/{id}"), json!({"fields": {"name": "Acme", "stage": "won"}, "version": 1})).await)
        .await;
    let inv = records("invoices").await;
    assert_eq!(inv[0]["fields"], json!({"client": "Acme", "note": "won Acme from talking"}), "{inv}");
    assert_eq!(inv[0]["created_by"], "trigger:won-deals");
    let d = eventually("the decision", async || {
        let open: Vec<Decision> = http.get(format!("{api}/decisions")).send().await.ok()?.json().await.ok()?;
        open.into_iter().find(|d| d.title == "Send the invoice to Acme?")
    })
    .await;
    assert_eq!((d.kind.as_str(), d.source.as_str(), d.document.as_str()), ("choose", "trigger:new-invoices", "won Acme from talking"));
    // The ask's run says which trigger led to it.
    let runs: Vec<AutomationRun> = send(M::GET, &format!("triggers/{}/runs", ask.id), json!({})).await.json().await.unwrap();
    assert_eq!((runs[0].outcome.as_str(), runs[0].trigger.as_str(), runs[0].cause.clone()), ("ok", "record", vec![won.id.clone()]));
    ok(send(M::POST, &format!("decisions/{}/answer", d.id), json!({"option": "Send"})).await).await;
    let note = eventually("the notification", async || got.lock().unwrap().iter().find(|n| n["event"] == "trigger").cloned()).await;
    assert_eq!(note["title"], "Sending: Send the invoice to Acme?", "{note}");
    assert_eq!(note["message"], "answered via carbon6", "{note}");

    // 2. A webhook writes a record directly, when its filter passes.
    collection("payments", json!({"amount": {"type": "number"}, "customer": {"type": "string"}})).await;
    let hook_t = trigger(json!({
        "name": "payments in", "trigger": "webhook", "filter": {"$.body.amount": {"$gt": 0}},
        "action": "write", "action_params": {"collection": "payments", "fields": {"amount": "$.body.amount", "customer": "$.body.customer.email"}}
    }))
    .await;
    let url = format!("{base}{}", hook_t.webhook_url.unwrap());
    assert!(http.post(&url).json(&json!({"amount": 12.5, "customer": {"email": "a@b.c"}})).send().await.unwrap().status().is_success());
    assert!(http.post(&url).json(&json!({"amount": 0, "customer": {"email": "x@y.z"}})).send().await.unwrap().status().is_success());
    let pays = records("payments").await;
    assert_eq!(pays.as_array().unwrap().len(), 1, "{pays}");
    assert_eq!(pays[0]["fields"], json!({"amount": 12.5, "customer": "a@b.c"}));

    // 3. Loop protection: a trigger that feeds itself stops at the depth
    //    limit, and says so; one over its hourly limit is skipped.
    collection("echo", json!({"n": {"type": "number"}})).await;
    let echo = trigger(json!({
        "name": "echo", "trigger": "record", "event_params": {"collection": "echo", "ops": ["created"]},
        "action": "write", "action_params": {"collection": "echo", "fields": {"n": "$.record.fields.n"}}
    }))
    .await;
    ok(send(M::POST, "collections/echo/records", json!({"fields": {"n": 1}})).await).await;
    assert_eq!(records("echo").await.as_array().unwrap().len(), 1 + c6_head::triggers::MAX_DEPTH);
    let runs: Vec<AutomationRun> = send(M::GET, &format!("triggers/{}/runs", echo.id), json!({})).await.json().await.unwrap();
    let stopped = runs.iter().find(|r| r.outcome == "stopped").expect("a stopped run");
    assert!(stopped.error.as_deref().unwrap().contains("loop protection: a chain of 5"), "{stopped:?}");
    assert_eq!(stopped.cause.len(), c6_head::triggers::MAX_DEPTH);
    assert!(got.lock().unwrap().iter().any(|n| n["title"] == "Trigger echo stopped"));
    ok(send(M::PATCH, &format!("triggers/{}", hook_t.id), json!({"per_hour": 2})).await).await;
    for n in [1, 2] {
        http.post(&url).json(&json!({"amount": n, "customer": {"email": "r@s.t"}})).send().await.unwrap();
    }
    assert_eq!(records("payments").await.as_array().unwrap().len(), 2, "one more fits its two an hour");
    let runs: Vec<AutomationRun> = send(M::GET, &format!("triggers/{}/runs", hook_t.id), json!({})).await.json().await.unwrap();
    assert!(runs[0].error.as_deref().unwrap().contains("times in the last hour"), "{:?}", runs[0]);

    // 4. Sessions: a finished turn writes a log record.
    collection("log", json!({"what": {"type": "string"}})).await;
    trigger(json!({
        "name": "log sessions", "trigger": "session", "event_params": {"ops": ["finished"]},
        "action": "write", "action_params": {"collection": "log", "fields": {"what": "{{$.op}}: {{$.session.id}}"}}
    }))
    .await;
    mock.push(Reply::Text("Done.".into()));
    let s: Value = send(M::POST, "sessions", json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "hi"}))
        .await
        .json()
        .await
        .unwrap();
    let sid = s["id"].as_str().unwrap().to_owned();
    eventually("the session's log record", async || {
        records("log").await.as_array()?.iter().any(|r| r["fields"]["what"] == format!("finished: {sid}")).then_some(())
    })
    .await;

    // 5. A digest: a schedule notifies of open decisions, only when there
    //    are some (run here directly rather than waiting for the clock).
    let digest = trigger(json!({
        "name": "digest", "trigger": "schedule", "cron": "0 8 * * *", "filter": {"$.open_decisions.count": {"$gt": 0}},
        "action": "notify", "action_params": {"title": "{{$.open_decisions.count}} open", "message": "{{$.open_decisions.text}}"}
    }))
    .await;
    ok(send(M::POST, "decisions", json!({"title": "Renew the domain?", "priority": "high"})).await).await;
    c6_head::triggers::scheduled(&head.state, &digest).await;
    let note = eventually("the digest", async || {
        got.lock().unwrap().iter().find(|n| n["title"].as_str().is_some_and(|t| t.ends_with(" open"))).cloned()
    })
    .await;
    assert!(note["message"].as_str().unwrap().contains("Renew the domain? (high)"), "{note}");

    // The old routes still answer.
    let old: Vec<Automation> = send(M::GET, "automations", json!({})).await.json().await.unwrap();
    assert_eq!(old.len(), 7);

    stop.cancel();
    head.shutdown().await.unwrap();
}
