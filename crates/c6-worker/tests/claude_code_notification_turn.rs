//! A binary of its own: C6_CLAUDE_BIN is process-global. No real CLI or model.
use c6_api::{Event, EventKind};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn notification_turns_end_on_result_or_authoritative_idle() {
    let bins = tempfile::tempdir().unwrap();
    let claude = bins.path().join("claude");
    std::fs::write(&claude, r##"#!/bin/sh
[ "$1" = --version ] && exit 0
[ "$CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS" = 1 ] || exit 2
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  mode=$(printf '%s' "$line" | sed -n 's/.*MODE \([a-z]*\).*/\1/p')
  printf '%s\n' '{"type":"system","subtype":"init"}'
  uuid=$(printf '%s' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
  if [ "$mode" != unconfirmed ]; then
    printf '{"type":"user","isReplay":true,"uuid":"%s"}\n' "$uuid"
  fi
  case "$line" in *PING*)
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Ready."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}' '{"type":"result","subtype":"success","usage":{}}'
    continue;;
  esac
  for task in bg1 bg2 bg3; do
    printf '{"type":"system","subtype":"task_started","task_id":"%s","task_type":"local_agent","is_backgrounded":true,"description":"Scout"}\n' "$task"
  done
  printf '%s\n' '{"type":"result","subtype":"success","usage":{}}'
  read -r gate < gate
  for task in bg1 bg2 bg3; do
    printf '{"type":"system","subtype":"task_notification","task_id":"%s","status":"completed","summary":"Provider found"}\n' "$task"
  done
  if [ "$mode" = unconfirmed ]; then
    printf '%s\n' \
      '{"type":"user","origin":{"kind":"task-notification"},"message":{"content":"<background-task id=\"bg1\" kind=\"subagent\" status=\"finished\">Done.</background-task>"}}' \
      '{"type":"user","origin":{"kind":"task-notification"},"message":{"content":"<background-task id=\"bg2\" kind=\"subagent\" status=\"finished\">Done.</background-task>"}}' \
      '{"type":"user","origin":{"kind":"task-notification"},"message":{"content":"<background-task id=\"bg3\" kind=\"subagent\" status=\"finished\">Done.</background-task>"}}'
  fi
  printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"running"}'
  if [ "$mode" != silent ]; then
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"All providers found."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
  fi
  if [ "$mode" = result ]; then
    printf '%s\n' '{"type":"result","subtype":"success","usage":{}}'
  else
    # Neither compaction finishing nor an empty task snapshot ends a turn.
    printf '%s\n' '{"type":"system","subtype":"status","status":null}' '{"type":"system","subtype":"background_tasks_changed","tasks":[]}'
    read -r gate < idle
    printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"idle"}'
    if [ "$mode" = silent ]; then read -r gate < trailing; fi
    # Optional trailing result must not create a phantom third turn.
    printf '%s\n' '{"type":"result","subtype":"success","usage":{}}'
  fi
done
"##).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary has one test, with its own fake CLI.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
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
    c6_testkit::within("worker attached", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let cc: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind":"claude_code","name":"cc","base_url":""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for mode in ["result", "idle", "silent", "unconfirmed"] {
        let project = tempfile::tempdir().unwrap();
        for name in ["gate", "idle", "trailing"] {
            nix::unistd::mkfifo(&project.path().join(name), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
        }
        let s: Value = http
            .post(format!("{api}/sessions"))
            .json(&json!({
                "worker_id":worker.identity.meta.worker_id.0, "cwd":project.path().display().to_string(),
                "connection_id":cc["id"], "model":"default", "prompt":format!("MODE {mode}")
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = s["id"].as_str().unwrap();
        let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
        eventually("first turn ended, tasks still hold update", async || {
            let ev = events().await;
            (ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == 1
                && worker.sessions.holding().iter().filter(|(s, _)| s == id).count() == 3)
                .then_some(())
        })
        .await;
        let gate = project.path().join("gate");
        tokio::task::spawn_blocking(move || std::fs::write(gate, "go\n").unwrap()).await.unwrap();
        if mode != "result" {
            eventually("notification turn remains running until proven idle", async || {
                (events().await.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count()
                    == if mode == "silent" { 1 } else { 2 }
                    && worker.sessions.holding().iter().any(|(s, what)| s == id && what == "a turn"))
                .then_some(())
            })
            .await;
            let gate = project.path().join("idle");
            tokio::task::spawn_blocking(move || std::fs::write(gate, "go\n").unwrap()).await.unwrap();
        }
        eventually("notification turn ends and releases update", async || {
            let ev = events().await;
            let session: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
            (ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == 2
                && session["session"]["status"] == "idle"
                && worker.sessions.holding().iter().all(|(s, _)| s != id))
            .then_some(())
        })
        .await;
        // A subsequent real input is a stream-order barrier for the optional
        // trailing result: there must be no phantom turn in between.
        http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text":"PING"})).send().await.unwrap();
        if mode == "silent" {
            eventually("next input admitted before the previous result", async || {
                worker.sessions.holding().iter().any(|(s, what)| s == id && what == "a turn").then_some(())
            })
            .await;
            let gate = project.path().join("trailing");
            tokio::task::spawn_blocking(move || std::fs::write(gate, "go\n").unwrap()).await.unwrap();
        }
        eventually("a real next turn, without a phantom turn for the trailing result", async || {
            let ev = events().await;
            (ev.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Ready.")))
                && ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == 3
                && worker.sessions.holding().iter().all(|(s, _)| s != id))
            .then_some(())
        })
        .await;
    }
    stop.cancel();
    // Stop only this fixture's held CLIs, never a user's worker/process.
    for d in std::fs::read_dir(state.path().join("claude/held")).into_iter().flatten().flatten() {
        let meta: Value = serde_json::from_slice(&std::fs::read(d.path().join("meta.json")).unwrap()).unwrap();
        let _ =
            nix::sys::signal::killpg(nix::unistd::Pid::from_raw(meta["pgid"].as_i64().unwrap() as i32), nix::sys::signal::Signal::SIGKILL);
    }
    head.shutdown().await.unwrap();
}
