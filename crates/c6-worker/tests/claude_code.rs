//! Claude Code sessions with a stand-in `claude`: its own subagents show
//! as read-only sessions under the session (docs/subagents.md §8). A binary
//! of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;

use c6_api::{Event, EventKind, Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` whose turn runs one subagent.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
read -r line
printf '%s\n' "$line" > delivered.json
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

#[tokio::test]
async fn fresh_worktree_starts_beside_unpublished_held_session_and_shows_subagents() {
    let bins = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe {
        std::env::set_var("C6_CLAUDE_BIN", fake_claude(bins.path()));
        std::env::set_var("CLAUDE_CONFIG_DIR", config.path());
    }
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
    // A different launch reserved its directory but has not published metadata.
    // This must not prevent a brand-new conversation from starting.
    let publishing = state.path().join("claude/held/another-session-publishing");
    std::fs::create_dir_all(&publishing).unwrap();
    std::fs::write(publishing.join("out"), "").unwrap();
    std::fs::write(publishing.join("err"), "").unwrap();
    nix::unistd::mkfifo(&publishing.join("in"), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();

    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind": "claude_code", "name": "cc", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let repo = tempfile::tempdir().unwrap();
    let project = state.path().join("worktrees/falcon/fresh");
    for args in
        [vec!["init"], vec!["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]]
    {
        assert!(tokio::process::Command::new("git").args(args).current_dir(repo.path()).status().await.unwrap().success());
    }
    assert!(
        tokio::process::Command::new("git")
            .args(["worktree", "add", "-b", "fresh"])
            .arg(&project)
            .current_dir(repo.path())
            .status()
            .await
            .unwrap()
            .success()
    );
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": format!("{}/", project.display()),
            "connection_id": conn["id"], "model": "opus", "prompt": "what is here?"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    eventually("turn done", async || {
        let events: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
        assert!(!events.iter().any(|event| matches!(event.kind, EventKind::Error { .. })), "fresh-session failure: {events:?}");
        let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.session.end_reason.is_some()).then_some(())
    })
    .await;
    let delivered: Value = serde_json::from_slice(&std::fs::read(project.join("delivered.json")).unwrap()).unwrap();
    assert!(delivered.to_string().contains("what is here?"), "the first prompt must reach the CLI");
    let detail: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(detail.session.end_reason, Some(TurnEndReason::Completed));
    assert!(publishing.is_dir(), "a young launch reservation must not be removed");
    assert_eq!(std::fs::read_dir(config.path()).unwrap().count(), 0, "no pre-existing Claude transcript is needed");
    let children = eventually("the mirrored subagent", async || {
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
