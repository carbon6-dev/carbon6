//! Claude Code sessions with a stand-in `claude`: its own subagents show
//! as read-only sessions under the session (docs/subagents.md §8). A binary
//! of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;
use std::time::Duration;

use c6_api::{Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` whose turn runs one subagent.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
cat > /dev/null
cat <<'JSON'
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_9","name":"Task","input":{"description":"look around","prompt":"List files."}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}
{"type":"user","message":{"content":"List files."},"parent_tool_use_id":"toolu_9"}
{"type":"assistant","message":{"content":[{"type":"text","text":"a.txt"}],"stop_reason":"end_turn"},"parent_tool_use_id":"toolu_9"}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_9","content":"a.txt"}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"content":[{"type":"text","text":"There is a.txt."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"There is a.txt.","usage":{"input_tokens":5,"output_tokens":20},"modelUsage":{"claude-opus-5-5":{}}}
JSON
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn until<T>(what: &str, secs: u64, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn claude_codes_subagents_are_shown_read_only() {
    let bins = tempfile::tempdir().unwrap();
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", fake_claude(bins.path())) };
    let head_dir = tempfile::tempdir().unwrap();
    let cfg = HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    };
    let head = c6_head::start(cfg).await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let state = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(15), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();

    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind": "claude_code", "name": "cc", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let project = tempfile::tempdir().unwrap();
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
            "connection_id": conn["id"], "model": "opus", "prompt": "what is here?"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    until("turn done", 20, async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.session.end_reason.is_some()).then_some(())
    })
    .await;
    let children = until("the mirrored subagent", 20, async || {
        let c: Vec<Session> = http.get(format!("{api}/sessions/{id}/children")).send().await.ok()?.json().await.ok()?;
        c.first().is_some_and(|c| c.end_reason.is_some()).then_some(c)
    })
    .await;
    let child = &children[0];
    assert!(child.observed);
    assert_eq!(child.title, "look around");
    assert_eq!(child.parent_call_id.as_deref(), Some("toolu_9"));
    assert_eq!(child.end_reason, Some(TurnEndReason::Completed));
    let r = http.post(format!("{api}/sessions/{}/messages", child.id)).json(&json!({"text": "more"})).send().await.unwrap();
    assert_eq!(r.status(), 400, "Claude Code runs its own subagents");
    let r = http.post(format!("{api}/sessions/{}/resume", child.id)).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    stop.cancel();
    head.shutdown().await.unwrap();
}
