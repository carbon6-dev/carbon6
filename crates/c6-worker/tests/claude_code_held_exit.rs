//! Held wrappers must be observed and reaped even when no exit-code file is written.
//! Own binary: C6_CLAUDE_BIN is process-global; no real CLI or live services.
use c6_api::{Event, EventKind, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if let Ok(bytes) = std::fs::read(entry.path().join("meta.json"))
                && let Ok(meta) = serde_json::from_slice::<Value>(&bytes)
                && let Some(pg) = meta["pgid"].as_i64()
            {
                let _ = killpg(Pid::from_raw(pg as i32), Signal::SIGKILL);
            }
        }
    }
}

#[tokio::test]
async fn held_exit_fails_the_running_turn_and_reaps_the_wrapper() {
    let bins = tempfile::tempdir().unwrap();
    let claude = bins.path().join("claude");
    std::fs::write(&claude, r#"#!/bin/sh
[ "$1" = --version ] && exit 0
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  printf '%s\n' '{"type":"system","subtype":"init"}'
  uuid=$(printf '%s' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
  printf '{"type":"user","isReplay":true,"uuid":"%s"}\n' "$uuid"
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Working."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
  case "$line" in *IDLE*) printf '%s\n' '{"type":"result","subtype":"success","usage":{}}';; esac
  read -r gate < gate
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Final output before exit."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
  exit 17
done
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary has one test and owns its stand-in CLI.
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
    let held = state.path().join("claude/held");
    let _cleanup = Cleanup(held.clone());
    for mode in ["cli_exit", "running_signal", "idle_signal"] {
        let idle = mode == "idle_signal";
        let wrapper_signal = mode != "cli_exit";
        let project = tempfile::tempdir().unwrap();
        nix::unistd::mkfifo(&project.path().join("gate"), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
        let s: Value = http
            .post(format!("{api}/sessions"))
            .json(&json!({"worker_id":worker.identity.meta.worker_id.0,
                "cwd":project.path().display().to_string(), "connection_id":cc["id"], "model":"default", "prompt":if idle { "IDLE" } else { "Work" }}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = s["id"].as_str().unwrap();
        let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
        eventually("a visible running turn", async || {
            events()
                .await
                .iter()
                .any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Working.")))
                .then_some(())
        })
        .await;
        let (dir, pid) = eventually("this session's held wrapper", async || {
            for entry in std::fs::read_dir(&held).ok()?.flatten() {
                let meta: Value = serde_json::from_slice(&std::fs::read(entry.path().join("meta.json")).ok()?).ok()?;
                if meta["session"] == id {
                    return Some((entry.path(), Pid::from_raw(meta["pgid"].as_i64()? as i32)));
                }
            }
            None
        })
        .await;
        if idle {
            eventually("the first turn to complete before killing its idle held wrapper", async || {
                (events().await.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. }))
                    && worker.sessions.holding().iter().all(|(s, what)| s != id || what != "a turn"))
                .then_some(())
            })
            .await;
        }
        if wrapper_signal {
            // Kill only the wrapper: it cannot run its `echo $? > code`.
            kill(pid, Signal::SIGTERM).unwrap();
        } else {
            let gate = project.path().join("gate");
            c6_testkit::within("release the CLI's exit gate", tokio::task::spawn_blocking(move || std::fs::write(gate, "exit\n").unwrap()))
                .await
                .unwrap();
        }
        if idle {
            eventually("the idle wrapper exit to be observed and reaped", async || {
                matches!(kill(pid, None), Err(nix::errno::Errno::ESRCH)).then_some(())
            })
            .await;
            // Reaping is the completion barrier for the actor's idle exited()
            // path, not a pause hoping that no phantom event will appear.
            let ev = events().await;
            assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count(), 1);
            assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count(), 1);
            assert!(!ev.iter().any(|e| matches!(e.kind, EventKind::Error { .. })), "idle death must not invent a failed turn: {ev:?}");
            assert!(!dir.join("code").exists(), "idle SIGTERM must also bypass the code file");
            continue;
        }
        let ev = eventually("held exit visibly fails the running turn", async || {
            let ev = events().await;
            ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Error, .. })).then_some(ev)
        })
        .await;
        assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count(), 1);
        assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("Claude Code exited") && message.contains("without a result"))));
        if !wrapper_signal {
            assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Final output before exit."))), "drain final output before reporting exit");
            assert!(
                ev.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("17"))),
                "retain the actual exit status"
            );
        }
        eventually("the wrapper is reaped, not left a zombie", async || {
            matches!(kill(pid, None), Err(nix::errno::Errno::ESRCH)).then_some(())
        })
        .await;
        if wrapper_signal {
            assert!(!dir.join("code").exists(), "SIGTERM must exercise exit without the wrapper's code file");
        }
        eventually("failed turn releases its running hold", async || {
            worker.sessions.holding().iter().all(|(s, what)| s != id || what != "a turn").then_some(())
        })
        .await;
    }
    stop.cancel();
    head.shutdown().await.unwrap();
}
