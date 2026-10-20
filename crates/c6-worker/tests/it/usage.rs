//! Usage and budgets: model calls are priced and rolled up; a blocking
//! budget, once used up, tells you and refuses new work with its labels.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A stand-in for Slack and webhooks: records (path, body).
async fn receiver() -> (String, Arc<Mutex<Vec<(String, serde_json::Value)>>>) {
    let got: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let (g, root) = (got.clone(), got.clone());
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
                let root = root.clone();
                async move {
                    root.lock().unwrap().push(("ntfy".into(), v));
                    "ok"
                }
            }),
        )
        .route(
            "/{name}",
            axum::routing::post(
                move |axum::extract::Path(name): axum::extract::Path<String>, axum::Json(v): axum::Json<serde_json::Value>| {
                    let g = g.clone();
                    async move {
                        g.lock().unwrap().push((name, v));
                        "ok"
                    }
                },
            ),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

#[tokio::test]
async fn usage_and_budgets() {
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

    let (hooks, got) = receiver().await;
    let put = |path: &str, v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put("prices", json!([{"model": "m", "input": -1, "output": 1}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        put("budgets", json!([{"name": "x", "period": "year", "limit": 1, "action": "block"}])).await,
        reqwest::StatusCode::BAD_REQUEST
    );
    assert_eq!(put("budgets", json!([{"name": "x", "period": "day", "limit": "", "action": "block"}])).await.as_u16() / 100, 4);
    // Strings, as the settings editor saves them. Each mock call: 20 input,
    // 80 cached, 10 output tokens = $0.048.
    assert!(
        put("prices", json!([{"model": "m", "input": "1000", "output": 2000, "cache_read": "100", "cache_write": ""}])).await.is_success()
    );
    assert!(
        put(
            "budgets",
            json!([
                {"name": "Ops per day", "applies_to": ["ops"], "period": "day", "limit": "0.1", "action": "block"},
                {"name": "Monthly", "period": "month", "limit": 1000, "action": "notify"}
            ])
        )
        .await
        .is_success()
    );
    assert!(
        put("channels", json!([{"name": "hook", "kind": "webhook", "url": format!("{hooks}/hook"), "events": ["budget"]}]))
            .await
            .is_success()
    );

    let wid = worker.identity.meta.worker_id.0;
    let create = |labels: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/sessions"));
        let body = json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "go", "labels": labels});
        async move { http.post(url).json(&body).send().await.unwrap() }
    };
    let calls = |n: usize| {
        let mock = &mock;
        async move { eventually("model calls", async || (mock.requests().len() >= n).then_some(())).await }
    };
    let idle = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("idle", async || {
                let d: serde_json::Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                (d["session"]["status"] == "idle").then_some(())
            })
            .await
        }
    };
    let say = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "again"})).send().await.unwrap() }
    };
    let report = |q: &str| {
        let (http, url) = (http.clone(), format!("{api}/usage?{q}"));
        async move { http.get(url).send().await.unwrap().json::<c6_api::UsageReport>().await.unwrap() }
    };

    mock.push(Reply::Text("one".into()));
    let s: Session = create(json!(["ops"])).await.json().await.unwrap();
    calls(1).await;
    idle(s.id.clone()).await;
    // The call is recorded with the turn's events (it may trail the idle state).
    let deadline = tokio::time::Instant::now() + c6_testkit::wait::deadline();
    let r = loop {
        let r = report("days=1").await;
        if r.total.calls == 1 || tokio::time::Instant::now() > deadline {
            break r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(r.total.calls, 1);
    assert!((r.total.cost - 0.048).abs() < 1e-9, "{}", r.total.cost);
    assert_eq!((r.total.input, r.total.cache_read, r.total.output), (20, 80, 10));
    assert!(r.by_label.iter().any(|l| l.key == "ops"));
    assert_eq!(r.by_model[0].key, "m");
    assert_eq!(r.top_sessions[0].key, s.id);
    assert!(r.unpriced_models.is_empty());
    let b = r.budgets.iter().find(|b| b.name == "Ops per day").unwrap();
    assert!(!b.blocking && (b.spent - 0.048).abs() < 1e-9);

    // Two more calls: $0.144, past the $0.10 limit.
    for n in [2, 3] {
        mock.push(Reply::Text("more".into()));
        assert!(say(s.id.clone()).await.status().is_success());
        calls(n).await;
        idle(s.id.clone()).await;
    }
    let note = eventually("the budget notification", async || got.lock().unwrap().first().map(|(_, v)| v.clone())).await;
    assert_eq!(note["event"], "budget");
    assert_eq!(note["title"], "Budget Ops per day used up");
    assert!(note["message"].as_str().unwrap().contains("$0.14 of $0.10"), "{note}");

    // New work with ops is refused; other sessions go on.
    let r = say(s.id.clone()).await;
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    let e: serde_json::Value = r.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("budget \"Ops per day\" is used up"), "{e}");
    assert_eq!(create(json!(["ops", "x"])).await.status(), reqwest::StatusCode::BAD_REQUEST);
    mock.push(Reply::Text("fine".into()));
    let other = create(json!(["dev"])).await;
    assert!(other.status().is_success());
    calls(4).await;
    let r = report("days=1&labels=ops").await;
    assert_eq!(r.total.calls, 3, "the filter");
    assert!(r.budgets.iter().any(|b| b.name == "Ops per day" && b.blocking));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(got.lock().unwrap().len(), 1, "once per period");

    stop.cancel();
    head.shutdown().await.unwrap();
}
