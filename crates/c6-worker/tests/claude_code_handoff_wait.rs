//! A switch away from Claude Code waits while Claude Code's own background
//! tasks run (they would stop with its process), and goes ahead once they
//! end: here, once you stop the task (docs/background.md §8). A binary of
//! its own: it sets C6_CLAUDE_BIN.

use std::path::Path;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that stays up: its first turn starts a background task that
/// runs until it is asked to stop it.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
while read -r line; do
  case "$line" in
    *'"stop_task"'*)
      echo '{"type":"system","subtype":"task_notification","task_id":"bgnative","status":"stopped","summary":"Stopped"}'
      continue;;
    *'"control_request"'*) continue;;
  esac
  echo '{"type":"system","subtype":"init"}'
  printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$line")"
  echo '{"type":"system","subtype":"task_started","task_id":"bgnative","description":"Long build","task_type":"local_bash"}'
  echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Building in the background."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
  echo '{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":1,"output_tokens":1}}'
done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn a_switch_away_from_claude_code_waits_for_its_tasks() {
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let add =
        async |body: Value| -> Value { http.post(format!("{api}/connections")).json(&body).send().await.unwrap().json().await.unwrap() };
    let cc = add(json!({"kind": "claude_code", "name": "cc", "base_url": ""})).await;
    let own = add(json!({"name": "m", "base_url": mock.base_url()})).await;
    let events =
        async |id: &str| -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
    let idle_with = async |id: &str, turns: usize| {
        eventually("idle", async || {
            let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
            let evs = events(id).await;
            let ended = evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (d.session.status == SessionStatus::Idle && ended == turns).then_some(evs)
        })
        .await
    };

    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
            "connection_id": cc["id"], "model": "opus", "prompt": "start the build"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    let evs = idle_with(&id, 1).await;
    let task = evs
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::BackgroundTask { task_id, state, .. } if state == "started" => Some(task_id.clone()),
            _ => None,
        })
        .expect("Claude Code's task is recorded");

    // Switch to carbon6's own loop: it waits, and says so.
    mock.push(Reply::Text("Carried on in carbon6.".into()));
    http.post(format!("{api}/sessions/{id}/messages"))
        .json(&json!({"text": "carry on here", "connection_id": own["id"], "model": "m"}))
        .send()
        .await
        .unwrap();
    eventually("the switch to wait", async || {
        events(&id)
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("waits for its background tasks")))
            .then_some(())
    })
    .await;
    assert!(mock.requests().is_empty(), "no turn on the new runtime yet");
    assert!(!events(&id).await.iter().any(|e| matches!(&e.kind, EventKind::ModelSwitched { handoff: Some(_), .. })), "not switched yet");

    // You stop the task: the switch goes ahead, then your message.
    let r = http.post(format!("{api}/sessions/{id}/tasks/{task}/stop")).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let evs = idle_with(&id, 2).await;
    assert!(evs.iter().any(|e| matches!(&e.kind, EventKind::ModelSwitched { handoff: Some(_), .. })), "switched");
    let stopped = evs
        .iter()
        .any(|e| matches!(&e.kind, EventKind::BackgroundTask { task_id, state, .. } if *task_id == task && state == "stopped_by_user"));
    assert!(stopped, "the task reads as stopped by you");
    assert_eq!(mock.requests().len(), 1);
    assert!(mock.requests()[0]["messages"].to_string().contains("carry on here"));
    stop.cancel();
    head.shutdown().await.unwrap();
}
