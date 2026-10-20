//! Automations: a webhook (its token the only credential) and a schedule
//! start a template; runs are labelled automation:<name>.

use c6_api::Connection;
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn automations_start_templates() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let token = head.state.store.create_api_token("t", None).await.unwrap().token.unwrap();
    let mut auth = reqwest::header::HeaderMap::new();
    auth.insert("authorization", format!("Bearer {token}").parse().unwrap());
    let http = reqwest::Client::builder().default_headers(auth).build().unwrap();
    let anon = reqwest::Client::new();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let _: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let r = http
        .put(format!("{api}/settings/templates"))
        .json(&json!([{"name": "triage", "model": "m", "prompt": "Triage this alert: {{input}}"}]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let create = |v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/automations"));
        async move { http.post(url).json(&v).send().await.unwrap() }
    };
    assert_eq!(create(json!({"name": "x", "template": "nope", "trigger": "webhook"})).await.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        create(json!({"name": "x", "template": "triage", "trigger": "schedule", "cron": "every day"})).await.status(),
        reqwest::StatusCode::BAD_REQUEST
    );

    // A schedule: validated, with its next time; runnable by hand.
    let sched: serde_json::Value =
        create(json!({"name": "Morning triage", "template": "triage", "trigger": "schedule", "cron": "0 8 * * mon-fri", "input": "the overnight alerts"}))
            .await
            .json()
            .await
            .unwrap();
    assert!(sched["next_run_at"].as_i64().is_some() && sched.get("webhook_url").is_none(), "{sched}");

    // A webhook: its URL is shown once and works without signing in.
    let hook: serde_json::Value = create(json!({"name": "pager", "template": "triage", "trigger": "webhook"})).await.json().await.unwrap();
    let path = hook["webhook_url"].as_str().unwrap().to_owned();
    let listed: serde_json::Value = http.get(format!("{api}/automations")).send().await.unwrap().json().await.unwrap();
    assert!(!listed.to_string().contains(path.rsplit('/').next().unwrap()), "the token is not listed");
    let base = api.trim_end_matches("/api/v1");
    let wrong = anon.post(format!("{base}{}x", &path[..path.len() - 1])).body("{}").send().await.unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        anon.get(format!("{api}/automations")).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "the rest needs sign-in"
    );

    mock.push(Reply::Text("Looks like disk pressure.".into()));
    let fired = anon.post(format!("{base}{path}")).body(r#"{"alert":"disk 95%","host":"web1"}"#).send().await.unwrap();
    assert_eq!(fired.status(), reqwest::StatusCode::ACCEPTED);
    let run: serde_json::Value = fired.json().await.unwrap();
    let labels = run["sessions"][0]["labels"].to_string();
    assert!(labels.contains("automation:pager") && labels.contains("template:triage"), "{labels}");
    eventually("the webhook's session ran", async || (mock.requests().len() == 1).then_some(())).await;
    let msg = mock.requests()[0]["messages"].to_string();
    assert!(msg.contains("Triage this alert:") && msg.contains("\\\"alert\\\": \\\"disk 95%\\\""), "pretty JSON input: {msg}");

    // Run the schedule now; a switched-off webhook stops answering.
    mock.push(Reply::Text("All quiet.".into()));
    let id = sched["id"].as_str().unwrap();
    let r: serde_json::Value = http.post(format!("{api}/automations/{id}/run")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["sessions"].as_array().unwrap().len(), 1);
    eventually("the scheduled run", async || (mock.requests().len() == 2).then_some(())).await;
    assert!(mock.requests()[1]["messages"].to_string().contains("the overnight alerts"));
    let listed: serde_json::Value = http.get(format!("{api}/automations")).send().await.unwrap().json().await.unwrap();
    let s = listed.as_array().unwrap().iter().find(|a| a["id"] == sched["id"]).unwrap();
    assert!(s["last_run_at"].as_i64().is_some() && s["last_run_id"] == r["run_id"]);
    let hid = hook["id"].as_str().unwrap();
    http.patch(format!("{api}/automations/{hid}")).json(&json!({"enabled": false})).send().await.unwrap();
    assert_eq!(anon.post(format!("{base}{path}")).body("x").send().await.unwrap().status(), reqwest::StatusCode::NOT_FOUND);

    stop.cancel();
    head.shutdown().await.unwrap();
}
