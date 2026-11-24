//! The Machines inventory and history use the real authenticated worker link,
//! but resource values come from a deterministic sampler, not this test host.

use c6_api::machines::{Machine, MachineDisk, MachineSample, MachineSessionUsage};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, conn, enroll};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn worker_samples_reach_machine_inventory_and_history() {
    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
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
    let (note_tx, mut note_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let webhook = format!("http://{}", listener.local_addr().unwrap());
    let receiver = axum::Router::new().route(
        "/",
        axum::routing::post(move |axum::Json(note): axum::Json<serde_json::Value>| {
            let note_tx = note_tx.clone();
            async move {
                note_tx.send(note).unwrap();
                "ok"
            }
        }),
    );
    let receiver = tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });
    http.put(format!("{api}/settings/channels"))
        .json(&serde_json::json!([{"name": "machine-test", "kind": "webhook", "url": webhook}]))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let enrollment = c6_head::api::new_enrollment(&head.state, Some("telemetry-worker")).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(worker_dir.path()).unwrap();
    let worker = crate::open_worker(worker_dir.path()).await;
    let worker_id = worker.identity.meta.worker_id.to_string();
    let sample = MachineSample {
        at_ms: c6_core::now_ms(),
        cpu_percent: 73.5,
        cpu_count: 8,
        load: [2.0, 1.0, 0.5],
        memory_total: 16000,
        memory_used: 12000,
        memory_available: 4000,
        sessions: vec![MachineSessionUsage { session_id: uuid::Uuid::now_v7().to_string(), cpu_percent: 42.0, rss: 1234 }],
        disks: vec![MachineDisk {
            mount: "/test-state".into(),
            filesystem: "ext4".into(),
            total: 1000,
            used: 910,
            available: 90,
            inodes_used: Some(12),
        }],
        ..Default::default()
    };
    let fake = sample.clone();
    worker.set_machine_sampler(move || fake.clone());
    let matches_payload = |received: &MachineSample| {
        let mut received = received.clone();
        received.at_ms = sample.at_ms;
        received == sample
    };
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    let task = tokio::spawn(conn::run(worker, tx, stop.clone()));
    c6_testkit::within("the telemetry worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();

    eventually("the worker sample to arrive in the head inventory", || async {
        let response = http.get(format!("{api}/machines")).send().await.unwrap().error_for_status().unwrap();
        let machines: Vec<Machine> = response.json().await.unwrap();
        assert_eq!(machines[0].id, "head");
        assert_eq!(machines[0].role, "head");
        assert!(machines[0].online);
        assert!(machines[0].head.is_some());
        machines
            .iter()
            .find(|m| m.id == worker_id)
            .is_some_and(|m| {
                m.online && m.last_sample.as_ref().is_some_and(matches_payload) && m.warnings.iter().any(|w| w.contains("/test-state"))
            })
            .then_some(())
    })
    .await;

    let note = c6_testkit::within("the default-enabled machine warning notification", async {
        loop {
            let note = note_rx.recv().await.expect("notification receiver closed");
            if note["message"].as_str().is_some_and(|m| m.contains("/test-state")) {
                break note;
            }
        }
    })
    .await;
    assert_eq!(note["event"], "machine");
    assert!(note["message"].as_str().unwrap().contains("/test-state"));

    eventually("the background writer to persist the worker sample", || async {
        let response = http
            .get(format!("{api}/machines/{worker_id}/samples"))
            .query(&[("from_ms", sample.at_ms)])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let history: Vec<MachineSample> = response.json().await.unwrap();
        history.iter().any(matches_payload).then_some(())
    })
    .await;

    let home_token: serde_json::Value = http
        .post(format!("{api}/auth/tokens"))
        .json(&serde_json::json!({"name": "machine-test-home", "realm": "home"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = home_token["token"].as_str().unwrap();
    let scoped: Vec<Machine> =
        http.get(format!("{api}/machines")).bearer_auth(token).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let visible_worker = scoped.iter().find(|m| m.id == worker_id).unwrap();
    assert!(visible_worker.last_sample.as_ref().unwrap().sessions.is_empty(), "a shared machine must not leak other realms' sessions");
    let scoped_history: Vec<MachineSample> = http
        .get(format!("{api}/machines/{worker_id}/samples"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!scoped_history.is_empty());
    assert!(scoped_history.iter().all(|s| s.sessions.is_empty()));

    http.post(format!("{api}/realms"))
        .json(&serde_json::json!({"id": "machine-other", "name": "Other machines"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    http.put(format!("{api}/workers/{worker_id}/realms"))
        .json(&serde_json::json!({"realms": ["machine-other"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let hidden: Vec<Machine> =
        http.get(format!("{api}/machines")).bearer_auth(token).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert!(hidden.iter().all(|m| m.id != worker_id));
    let denied = http.get(format!("{api}/machines/{worker_id}/samples")).bearer_auth(token).send().await.unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::NOT_FOUND);

    stop.cancel();
    c6_testkit::within("the telemetry worker to disconnect", task).await.unwrap().unwrap();
    head.shutdown().await.unwrap();
    receiver.abort();
}
