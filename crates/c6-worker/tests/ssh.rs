//! SSH targets: a session on another machine through its worker (here a
//! stand-in ssh that runs the "remote" side locally in a fake home).

use std::time::Duration;

use c6_api::{Connection, Session, SessionDetail, SessionStatus};
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
async fn ssh_sessions_work_on_the_target() {
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
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo installed > tool.txt && pwd"}"#.into())]));
    mock.push(Reply::Text("Installed.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "ssh": "admin@box", "cwd": "~/app", "connection_id": conn.id, "model": "m", "prompt": "install the tool"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let d: SessionDetail = eventually("idle", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && mock.requests().len() == 2).then_some(d)
    })
    .await;
    // The command ran "there", in the session's folder.
    assert_eq!(std::fs::read_to_string(home_dir.join("app/tool.txt")).unwrap(), "installed\n");
    let reqs = mock.requests();
    let system = reqs[0]["messages"][0]["content"].as_str().unwrap().to_owned();
    assert!(
        system.contains("remote machine admin@box over SSH")
            && system.contains(&format!("Working directory there: {}", home_dir.join("app").display())),
        "{system}"
    );
    assert!(reqs[1]["messages"].to_string().contains(&home_dir.join("app").display().to_string()));
    assert_eq!(d.session.ssh.as_deref(), Some("admin@box"));
    assert!(d.session.auto_labels.contains(&"host:box".to_owned()), "{:?}", d.session.auto_labels);
    // Nothing of it landed on the worker's side.
    assert!(!state.path().join("scratch").join(&s.id).join("tool.txt").exists());

    // A folder that is not there fails the session with a clear reason.
    let bad: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "ssh": "admin@box", "cwd": "/no/such/place", "connection_id": conn.id, "model": "m"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let failed: SessionDetail = eventually("failed", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", bad.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Failed).then_some(d)
    })
    .await;
    assert!(failed.session.error.unwrap().contains("no folder /no/such/place"));

    stop.cancel();
    head.shutdown().await.unwrap();
}
