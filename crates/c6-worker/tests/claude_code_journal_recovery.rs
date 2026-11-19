//! This binary owns C6_CLAUDE_BIN. A held CLI reaches idle after its reply
//! fails to journal; recovery must not adopt that idle process and stall.

use std::os::unix::fs::PermissionsExt;

use c6_api::{EventKind, TurnEndReason};
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct StopFakeCli(std::path::PathBuf);

impl Drop for StopFakeCli {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if let Ok(data) = std::fs::read(entry.path().join("meta.json"))
                && let Ok(meta) = serde_json::from_slice::<Value>(&data)
                && let Some(pgid) = meta["pgid"].as_i64().filter(|p| *p > 1)
            {
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid as i32), nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
}

#[tokio::test]
async fn a_held_cli_with_a_failed_append_is_retired_and_resumed_without_input() {
    let temp = tempfile::tempdir().unwrap();
    let gate = temp.path().join("reply");
    assert!(tokio::process::Command::new("mkfifo").arg(&gate).status().await.unwrap().success());
    let starts = temp.path().join("starts");
    let cli = temp.path().join("claude");
    std::fs::write(
        &cli,
        format!(
            r#"#!/bin/sh
[ "$1" = --version ] && exit 0
first=0
[ -f '{starts}' ] || first=1
echo start >> '{starts}'
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  uuid=$(printf '%s' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
  printf '{{"type":"user","isReplay":true,"uuid":"%s","message":{{"role":"user","content":"echo"}}}}\n' "$uuid"
  if [ "$first" = 1 ]; then
    read -r ignored < '{gate}'
    printf '%s\n' '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"native-call","name":"Task","input":{{"description":"Native child","prompt":"Look around"}}}}],"stop_reason":"tool_use"}}}}'
    printf '%s\n' '{{"type":"assistant","parent_tool_use_id":"native-call","message":{{"content":[{{"type":"text","text":"Native child was still running"}}],"stop_reason":""}}}}'
    text='fail-this-held-journal-append'
  else
    text='Reply from explicit recovery'
  fi
  printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"%s"}}],"stop_reason":"end_turn"}}}}\n' "$text"
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"usage":{{"input_tokens":1,"output_tokens":1}}}}'
done
"#,
            starts = starts.display(),
            gate = gate.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary has one test and sets the override before starting the worker.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &cli) };
    c6_testkit::runnable(&cli, &["--version"]).await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let held = state.path().join("claude").join("held");
    let _cleanup = StopFakeCli(held.clone());
    let head = c6_head::start(c6_head::HeadConfig {
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
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let connection: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind": "claude_code", "name": "cc", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let session: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(),
                      "connection_id": connection["id"], "model": "default", "prompt": "Finish the work"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let sid = session["id"].as_str().unwrap();
    let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", state.path().join("journal.db").display())).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_held_reply BEFORE INSERT ON events
         WHEN instr(NEW.data, CAST('fail-this-held-journal-append' AS BLOB)) > 0
         BEGIN SELECT RAISE(FAIL, 'injected held reply failure'); END",
    )
    .execute(&db)
    .await
    .unwrap();
    eventually("the held Claude Code CLI to start", async || {
        (starts.exists() && std::fs::read_dir(&held).into_iter().flatten().flatten().any(|entry| entry.path().join("meta.json").exists()))
            .then_some(())
    })
    .await;
    let mut release = tokio::process::Command::new("sh")
        .args(["-c", "printf 'reply\\n' > \"$1\"", "release-gate"])
        .arg(gate)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(c6_testkit::within("the fake CLI to read its reply gate", release.wait()).await.unwrap().success());
    let events = eventually("explicit recovery without another message", async || {
        let events: Vec<c6_api::Event> = http.get(format!("{api}/sessions/{sid}/events")).send().await.unwrap().json().await.unwrap();
        events.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Reply from explicit recovery"))).then_some(events)
    }).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Error, .. })));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id.starts_with("resume-after-failure-")))
            .count(),
        1
    );
    assert_eq!(std::fs::read_to_string(starts).unwrap().lines().count(), 2);
    let child = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id),
            _ => None,
        })
        .expect("the fake CLI's native child was mirrored");
    eventually("the retired CLI's native child to end with the journal failure cause", async || {
        let child_events: Vec<c6_api::Event> =
            http.get(format!("{api}/sessions/{child}/events")).send().await.unwrap().json().await.unwrap();
        (child_events.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Error, .. }))
            && child_events.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("journal write failed"))))
        .then_some(())
    })
    .await;
    stop.cancel();
    head.shutdown().await.unwrap();
}
