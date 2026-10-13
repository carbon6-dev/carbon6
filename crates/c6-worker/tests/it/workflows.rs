//! Workflows: steps pass output along, run only when an earlier step's
//! output says so (or changed since the last run), wait for approval (by
//! link or API), and can be started by an automation.

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
async fn workflows_chain_conditions_and_approvals() {
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
    let _ = &conn;
    let put = |path: &str, v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert!(
        put(
            "templates",
            json!([
                {"name": "check", "prompt": "Check the disks on {{input}}. End with a RESULT: line.", "connection": "m", "model": "m"},
                {"name": "fix", "prompt": "Clean up, given this report: {{input}}", "connection": "m", "model": "m"}
            ])
        )
        .await
        .is_success()
    );
    let base = api.trim_end_matches("/api/v1").to_owned();
    assert!(put("public_url", json!({"url": base})).await.is_success());
    assert!(
        put("channels", json!([{"name": "hook", "kind": "webhook", "url": format!("{hooks}/hook"), "events": ["notice", "approval"]}]))
            .await
            .is_success()
    );

    // Checked on save.
    let bad = |steps: serde_json::Value| put("workflows", json!([{"name": "w", "steps": steps}]));
    assert_eq!(bad(json!([{"kind": "run", "template": "nope"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(bad(json!([{"kind": "notify", "title": "x", "when": {"step": "later", "changed": true}}, {"id": "later", "kind": "run", "template": "check"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        put("workflows", json!([{"name": "check", "steps": [{"kind": "notify", "title": "x"}]}])).await,
        reqwest::StatusCode::BAD_REQUEST
    );
    assert!(put("workflows", json!([{"name": "disk", "steps": [
        {"id": "check", "kind": "run", "template": "check", "input": "{{input}}"},
        {"id": "tell", "kind": "notify", "title": "Disk report", "message": "{{steps.check.output}}", "when": {"step": "check", "changed": true}},
        {"id": "ok", "kind": "approve", "title": "Clean up?", "message": "{{steps.check.output}}", "when": {"step": "check", "contains": "FULL"}},
        {"id": "fix", "kind": "run", "template": "fix", "input": "{{steps.check.output}}", "when": {"step": "check", "contains": "FULL"}}
    ]}]))
    .await
    .is_success());

    let run = |input: &str| {
        let (http, url, body) = (http.clone(), format!("{api}/workflows/disk/run"), json!({"input": input}));
        async move { http.post(url).json(&body).send().await.unwrap().json::<c6_api::WorkflowRun>().await.unwrap() }
    };
    let status = |id: String, want: &'static [&'static str]| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually(want[0], async || {
                let r: c6_api::WorkflowRun = http.get(format!("{api}/workflows/runs/{id}")).send().await.ok()?.json().await.ok()?;
                want.contains(&r.status.as_str()).then_some(r)
            })
            .await
        }
    };
    let steps = |r: &c6_api::WorkflowRun| r.steps.iter().map(|s| format!("{}={}", s.id, s.status)).collect::<Vec<_>>().join(" ");
    let hook = |event: &'static str| {
        let got = got.clone();
        move || got.lock().unwrap().iter().filter(|(_, v)| v["event"] == event).map(|(_, v)| v.clone()).collect::<Vec<_>>()
    };

    // 1: full. Told (nothing to compare with), asked, approved by link, fixed.
    mock.push(Reply::Text("Disk at 95% on web1.\nRESULT: FULL".into()));
    mock.push(Reply::Text("Cleaned /var/log.".into()));
    let r1 = run("web1").await;
    let w = status(r1.id.clone(), &["waiting"]).await;
    assert_eq!(steps(&w), "check=ok tell=ok ok=waiting fix=pending");
    assert!(w.steps[0].output.contains("Disk at 95%") && w.steps[0].sessions.len() == 1);
    let approvals = hook("approval");
    let link = eventually("the approval", async || approvals().first().and_then(|v| v["approve_url"].as_str().map(str::to_owned))).await;
    let page = http.get(&link).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Go ahead with disk?") && page.contains("Disk at 95%"), "{page}");
    let answer = http.post(&link).header("origin", &base).form(&[("allow", "1")]).send().await.unwrap().text().await.unwrap();
    assert!(answer.contains("Sent"), "{answer}");
    let done = status(r1.id.clone(), &["ok", "failed"]).await;
    assert_eq!(steps(&done), "check=ok tell=ok ok=ok fix=ok", "{done:?}");
    assert!(
        mock.requests()[1]["messages"].to_string().contains("Clean up, given this report: Disk at 95% on web1."),
        "the output passed on"
    );
    assert_eq!(hook("notice")().len(), 1);
    assert!(
        http.post(&link)
            .header("origin", &base)
            .form(&[("allow", "1")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("already answered")
    );

    // 2: still full, worded differently: no report (same RESULT); denied.
    mock.push(Reply::Text("Still 95% here.\nRESULT: FULL".into()));
    let r2 = run("web1").await;
    let w = status(r2.id.clone(), &["waiting"]).await;
    assert_eq!(steps(&w), "check=ok tell=skipped ok=waiting fix=pending");
    let r = http.post(format!("{api}/workflows/runs/{}/steps/ok/decide", r2.id)).json(&json!({"allow": false})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(status(r2.id.clone(), &["denied"]).await.status, "denied");

    // 3: fine now: reported (changed), nothing to approve or fix.
    mock.push(Reply::Text("Disk at 40%.\nRESULT: OK".into()));
    let r3 = run("web1").await;
    let done = status(r3.id.clone(), &["ok", "failed"]).await;
    assert_eq!(steps(&done), "check=ok tell=ok ok=skipped fix=skipped");
    assert_eq!(hook("notice")().len(), 2);

    // An automation can start it; its run follows the workflow's.
    let a: c6_api::Automation = http
        .post(format!("{api}/automations"))
        .json(&json!({"name": "disk-hourly", "template": "disk", "trigger": "schedule", "cron": "0 * * * *", "input": "web1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    mock.push(Reply::Text("Disk at 41%.\nRESULT: OK".into()));
    let started: c6_api::TemplateRun = http.post(format!("{api}/automations/{}/run", a.id)).send().await.unwrap().json().await.unwrap();
    let wr = status(started.run_id.clone(), &["ok", "failed"]).await;
    assert_eq!((wr.trigger.as_str(), steps(&wr).as_str()), ("manual", "check=ok tell=skipped ok=skipped fix=skipped"));
    let runs = eventually("the automation run", async || {
        let r: Vec<c6_api::AutomationRun> = http.get(format!("{api}/automations/{}/runs", a.id)).send().await.ok()?.json().await.ok()?;
        r.first().filter(|r| r.outcome != "running").cloned()
    })
    .await;
    assert_eq!(runs.outcome, "ok");
    let all: Vec<c6_api::WorkflowRun> = http.get(format!("{api}/workflows/runs?workflow=disk")).send().await.unwrap().json().await.unwrap();
    assert_eq!(all.len(), 4);

    stop.cancel();
    head.shutdown().await.unwrap();
}
