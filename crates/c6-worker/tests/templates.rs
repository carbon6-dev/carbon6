//! Templates: a saved way to start sessions; a fleet run starts one per
//! target (here two SSH hosts through a stand-in ssh), sharing a run label.

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

/// Drops ssh's options and target, then runs the command line with sh in
/// a fake home, as sshd would on the target.
fn fake_ssh(dir: &std::path::Path) -> std::path::PathBuf {
    let script = "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -o|-p|-i|-l|-F) shift 2 ;; -*) shift ;; *) break ;; esac; done\nshift\nexport HOME=\"$C6_FAKE_HOME\"; cd \"$HOME\"\nexec sh -c \"$*\"\n";
    let path = dir.join("ssh");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn templates_start_sessions_and_fleet_runs() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (bin, remote) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let home_dir = remote.path().canonicalize().unwrap();
    std::fs::create_dir_all(home_dir.join("app")).unwrap();
    // SAFETY: the only test in this binary.
    unsafe {
        std::env::set_var("C6_SSH_BIN", fake_ssh(bin.path()));
        std::env::set_var("C6_FAKE_HOME", &home_dir);
    }
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
    let _: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let put = |v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/templates"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put(json!([{"name": "bad name", "prompt": "x"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put(json!([{"name": "x", "prompt": ""}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert!(
        put(json!([
            {"name": "disk-check", "ssh": ["admin@web1", "admin@web2"], "connection": "m", "model": "m",
             "labels": ["ops"], "prompt": "Check disk space on {{target}}. Note: {{input}}"},
            {"name": "solo", "model": "m", "prompt": "Hello {{target}}"}
        ]))
        .await
        .is_success()
    );
    let r = http.post(format!("{api}/templates/nope/run")).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);

    for _ in 0..2 {
        mock.push(Reply::Text("Disk is fine.".into()));
    }
    let run: serde_json::Value = http
        .post(format!("{api}/templates/disk-check/run"))
        .json(&json!({"input": "after the upgrade"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let sessions = run["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{run}");
    let titles: Vec<&str> = sessions.iter().map(|s| s["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["disk-check · admin@web1", "disk-check · admin@web2"]);
    for s in sessions {
        let labels: Vec<&str> = s["labels"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        assert_eq!(labels, ["ops", "template:disk-check", &format!("run:{run_id}")]);
        assert!(s["ssh"].as_str().unwrap().starts_with("admin@web"));
    }
    eventually("both ran", async || (mock.requests().len() == 2).then_some(())).await;
    let prompts: Vec<String> = mock.requests().iter().map(|r| r["messages"].to_string()).collect();
    assert!(prompts.iter().any(|p| p.contains("Check disk space on admin@web1. Note: after the upgrade")), "{prompts:?}");
    assert!(prompts.iter().any(|p| p.contains("Check disk space on admin@web2.")));

    // One worker, no targets: a single session there.
    mock.push(Reply::Text("hi".into()));
    let run: serde_json::Value = http.post(format!("{api}/templates/solo/run")).send().await.unwrap().json().await.unwrap();
    assert_eq!(run["sessions"].as_array().unwrap().len(), 1);
    eventually("solo ran", async || (mock.requests().len() == 3).then_some(())).await;
    let wname = run["sessions"][0]["auto_labels"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|l| l.as_str().unwrap().strip_prefix("worker:").map(str::to_owned))
        .unwrap();
    assert!(mock.requests()[2]["messages"].to_string().contains(&format!("Hello {wname}")));

    stop.cancel();
    head.shutdown().await.unwrap();
}
