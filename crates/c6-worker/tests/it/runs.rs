//! Automation runs: each run's outcome comes from its sessions' first
//! turns; failures notify, and a streak switches the automation off.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api::Connection;
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
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
async fn runs_outcomes_and_streaks() {
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
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
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
    assert!(put("templates", json!([{"name": "t", "prompt": "check things", "connection": "m", "model": "m"}])).await.is_success());
    assert!(
        put("channels", json!([{"name": "hook", "kind": "webhook", "url": format!("{hooks}/hook"), "events": ["automation"]}]))
            .await
            .is_success()
    );
    let a: c6_api::Automation = http
        .post(format!("{api}/automations"))
        .json(&json!({"name": "nightly", "template": "t", "trigger": "schedule", "cron": "0 0 1 1 *", "disable_after": 2}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(a.disable_after, 2);
    let _ = &conn;

    let runs = || {
        let (http, url) = (http.clone(), format!("{api}/automations/{}/runs", a.id));
        async move { http.get(url).send().await.unwrap().json::<Vec<c6_api::AutomationRun>>().await.unwrap() }
    };
    let run_once = |reply: Reply, n: usize| {
        let (http, url, mock) = (http.clone(), format!("{api}/automations/{}/run", a.id), mock.clone());
        async move {
            mock.push(reply);
            assert!(http.post(url).send().await.unwrap().status().is_success());
            eventually("the run to finish", async || {
                let r = runs().await;
                (r.len() == n && r[0].outcome != "running").then(|| r[0].clone())
            })
            .await
        }
    };

    let ok = run_once(Reply::Text("All good.".into()), 1).await;
    assert_eq!((ok.outcome.as_str(), ok.trigger.as_str(), ok.calls), ("ok", "manual", 1));
    assert_eq!(ok.sessions.len(), 1);
    assert_eq!((ok.sessions[0].outcome.as_str(), ok.sessions[0].title.as_str()), ("ok", "t"));
    assert!(ok.finished_at.is_some());

    let bad = run_once(Reply::Status(400), 2).await;
    assert_eq!((bad.outcome.as_str(), bad.sessions[0].outcome.as_str()), ("failed", "failed"));
    let titles = || got.lock().unwrap().iter().map(|(_, v)| v["title"].as_str().unwrap_or_default().to_owned()).collect::<Vec<_>>();
    eventually("the failure notification", async || titles().contains(&"Automation nightly failed".to_owned()).then_some(())).await;
    let list: Vec<c6_api::Automation> = http.get(format!("{api}/automations")).send().await.unwrap().json().await.unwrap();
    assert!(list[0].enabled, "one failure is not a streak");

    run_once(Reply::Status(400), 3).await;
    eventually("switched off", async || titles().contains(&"Automation nightly switched off".to_owned()).then_some(())).await;
    let list: Vec<c6_api::Automation> = http.get(format!("{api}/automations")).send().await.unwrap().json().await.unwrap();
    assert!(!list[0].enabled);
    assert_eq!(list[0].recent, ["failed", "failed", "ok"]);

    stop.cancel();
    head.shutdown().await.unwrap();
}
