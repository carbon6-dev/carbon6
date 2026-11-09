//! A Claude Code process outlives its worker's restart (docs/restarts.md
//! §3): the worker holds it apart from itself (its input a named pipe, its
//! output a file), so a turn under way when the worker goes on goes on, and
//! the next worker takes the process up and shows what it wrote, without
//! starting Claude Code again.
//! A binary of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;
use std::sync::Arc;

use c6_api::{Event, EventKind};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that stays up between turns and records each start. A message
/// "LONG <file>" says "Working.", waits for that file, then says "Done.".
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
[ "$1" = --version ] && exit 0
echo start >> {spawns}
uuid_of() {{ printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }}
say() {{ printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"%s"}}],"stop_reason":"end_turn"}},"parent_tool_use_id":null}}\n' "$1"; }}
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  printf '%s\n' '{{"type":"system","subtype":"init"}}'
  printf '{{"type":"user","isReplay":true,"uuid":"%s","message":{{"role":"user","content":"echo"}},"parent_tool_use_id":null}}\n' "$(uuid_of "$line")"
  gate=$(printf '%s' "$line" | sed -n 's/.*LONG \([^"\\]*\).*/\1/p')
  if [ -n "$gate" ]; then
    say "Working."
    while [ ! -f "$gate" ]; do sleep 0.05; done
    say "Done."
  else
    say "Reply."
  fi
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"usage":{{"input_tokens":1,"output_tokens":1}}}}'
done
"#,
        spawns = dir.join("spawns.txt").display()
    );
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A worker attached to the head, with what stops it.
async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = loop {
        match Worker::open(state, Identity::load(state).unwrap()).await {
            Ok(w) => break w,
            // The previous one hasn't let go of the state yet.
            Err(e) if format!("{e}").contains("already running") => tokio::task::yield_now().await,
            Err(e) => panic!("opening the worker: {e:#}"),
        }
    };
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (w, stop)
}

fn says(events: &[Event], text: &str) -> bool {
    events.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains(text)))
}

#[tokio::test]
async fn a_claude_code_turn_goes_on_through_a_worker_restart() {
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
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
    // The first worker in a runtime of its own, ended below as a worker's
    // process ends.
    let first_rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let st = state.path().to_owned();
    let (first, stop_first) = first_rt.spawn(async move { run_worker(&st).await }).await.unwrap();
    let cc: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind": "claude_code", "name": "cc", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gate = project.path().join("gate");
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": first.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
                       "connection_id": cc["id"], "model": "default", "prompt": format!("LONG {}", gate.display())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
    eventually("Claude Code at work", async || says(&events().await, "Working.").then_some(())).await;

    // The worker goes mid-turn; Claude Code doesn't.
    stop_first.cancel();
    drop(first);
    first_rt.shutdown_background();
    let (second, stop_second) = run_worker(state.path()).await;

    // Its turn finishes under the new worker, which shows what it said.
    std::fs::write(&gate, "").unwrap();
    eventually("the rest of its turn, shown", async || says(&events().await, "Done.").then_some(())).await;
    let starts = std::fs::read_to_string(bins.path().join("spawns.txt")).unwrap();
    assert_eq!(starts.lines().count(), 1, "the same process, not started again: {starts}");

    // And it carries on talking to the new worker.
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "and now?"})).send().await.unwrap();
    eventually("a reply through the new worker", async || {
        let ev = events().await;
        (ev.iter().filter(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Reply."))).count()
            >= 1)
            .then_some(())
    })
    .await;
    let starts = std::fs::read_to_string(bins.path().join("spawns.txt")).unwrap();
    assert_eq!(starts.lines().count(), 1, "still the same process: {starts}");
    drop(second);
    stop_second.cancel();
    // The process ends with its folder.
    let held = state.path().join("claude").join("held");
    for d in std::fs::read_dir(&held).into_iter().flatten().flatten() {
        let meta: Value = serde_json::from_slice(&std::fs::read(d.path().join("meta.json")).unwrap()).unwrap();
        let _ =
            nix::sys::signal::killpg(nix::unistd::Pid::from_raw(meta["pgid"].as_i64().unwrap() as i32), nix::sys::signal::Signal::SIGKILL);
    }
    head.shutdown().await.unwrap();
}
