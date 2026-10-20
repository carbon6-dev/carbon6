//! M3: files, git, terminals and worktrees through the head.

use std::path::Path;
use std::time::Duration;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

struct Setup {
    head: c6_head::Head,
    worker: std::sync::Arc<Worker>,
    api: String,
    http: reqwest::Client,
    stop: CancellationToken,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn setup() -> Setup {
    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let cfg = HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    };
    let head = c6_head::start(cfg).await.unwrap();
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(worker_dir.path()).unwrap();
    let worker = Worker::open(worker_dir.path(), Identity::load(worker_dir.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    Setup { head, worker, api, http: reqwest::Client::new(), stop, _dirs: (head_dir, worker_dir) }
}

fn sh(dir: &Path, cmd: &str) {
    assert!(std::process::Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap().success(), "{cmd}");
}

#[tokio::test]
async fn files_git_and_terminal() {
    let s = setup().await;
    let wid = s.worker.identity.meta.worker_id;
    let repo = tempfile::tempdir().unwrap();
    sh(
        repo.path(),
        "git init -q -b main && git config user.email t@t && git config user.name t && echo one > a.txt && git add . && git commit -qm init && echo two >> a.txt",
    );
    let path = repo.path().display().to_string();

    let list: Value =
        s.http.get(format!("{}/workers/{wid}/fs/list", s.api)).query(&[("path", &path)]).send().await.unwrap().json().await.unwrap();
    assert_eq!(list["entries"][0]["name"], "a.txt");
    let read: Value = s
        .http
        .get(format!("{}/workers/{wid}/fs/read", s.api))
        .query(&[("path", format!("{path}/a.txt"))])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["content"], "one\ntwo\n");
    let diff: Value =
        s.http.get(format!("{}/workers/{wid}/git/diff", s.api)).query(&[("path", &path)]).send().await.unwrap().json().await.unwrap();
    assert!(diff["diff"].as_str().unwrap().contains("+two"));
    let missing = s.http.get(format!("{}/workers/{wid}/fs/read", s.api)).query(&[("path", "/no/such/file")]).send().await.unwrap();
    assert_eq!(missing.status(), 400);

    // Terminal: open, attach over WebSocket, type, see output, exit.
    let term: Value = s
        .http
        .post(format!("{}/workers/{wid}/terminals", s.api))
        .json(&json!({"cwd": path, "cols": 80, "rows": 24}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = term["id"].as_str().unwrap().to_owned();
    let ws_url = format!("{}/workers/{wid}/terminals/{tid}/attach", s.api.replace("http://", "ws://"));
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    ws.send(Message::Text(json!({"type": "resize", "cols": 100, "rows": 30}).to_string().into())).await.unwrap();
    ws.send(Message::Binary(b"stty size; echo c6-$((40+2)); exit\n".to_vec().into())).await.unwrap();
    let mut seen = String::new();
    let mut exit = None;
    // Everything the shell says until it exits (the exit is a text frame).
    while exit.is_none() {
        match c6_testkit::within("the terminal's output", ws.next()).await {
            Some(Ok(Message::Binary(b))) => seen.push_str(&String::from_utf8_lossy(&b)),
            Some(Ok(Message::Text(t))) => exit = Some(t.to_string()),
            _ => break,
        }
    }
    assert!(seen.contains("c6-42"), "{seen}");
    assert!(seen.contains("30 100"), "resize reached the pty: {seen}");
    assert!(exit.unwrap().contains("\"exit\""));
    let terms: Value = s.http.get(format!("{}/workers/{wid}/terminals", s.api)).send().await.unwrap().json().await.unwrap();
    assert!(!terms[0]["exited"].is_null());

    s.stop.cancel();
    s.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_in_a_worktree() {
    let s = setup().await;
    let mock = MockLlm::start().await;
    mock.push(Reply::ToolCalls(vec![(
        "bash".into(),
        r#"{"command":"git branch --show-current > where.txt; pwd -P >> where.txt"}"#.into(),
    )]));
    mock.push(Reply::Text("done".into()));
    let repo = tempfile::tempdir().unwrap();
    sh(
        repo.path(),
        "git init -q -b main && git config user.email t@t && git config user.name t && mkdir sub && echo x > sub/f && git add . && git commit -qm init",
    );

    let conn: Value = s
        .http
        .post(format!("{}/connections", s.api))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sess: Value = s
        .http
        .post(format!("{}/sessions", s.api))
        .json(&json!({"worker_id": s.worker.identity.meta.worker_id.0, "cwd": repo.path().join("sub").display().to_string(),
            "connection_id": conn["id"], "model": "m", "prompt": "where am I", "worktree": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = sess["id"].as_str().unwrap();
    let deadline = tokio::time::Instant::now() + c6_testkit::wait::deadline();
    loop {
        let d: SessionDetail = s.http.get(format!("{}/sessions/{id}", s.api)).send().await.unwrap().json().await.unwrap();
        if d.session.status == SessionStatus::Idle && d.queued.is_empty() && d.session.last_seq > 3 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "session did not finish");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let events: Vec<Event> = s.http.get(format!("{}/sessions/{id}/events", s.api)).send().await.unwrap().json().await.unwrap();
    let EventKind::Started { cwd, worktree_branch, .. } = &events[0].kind else { panic!() };
    assert!(worktree_branch.starts_with("c6/"), "{worktree_branch}");
    assert!(cwd.ends_with("/sub") && cwd.contains("/worktrees/"), "{cwd}");
    let where_ = std::fs::read_to_string(Path::new(cwd).join("where.txt")).unwrap();
    assert!(where_.starts_with(worktree_branch.as_str()), "{where_}");
    // The original checkout is untouched.
    assert!(!repo.path().join("sub/where.txt").exists());

    s.stop.cancel();
    s.head.shutdown().await.unwrap();
}
