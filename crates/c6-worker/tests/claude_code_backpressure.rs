//! Stop must remain effective while a held CLI no longer reads its FIFO.
//! Own binary: C6_CLAUDE_BIN is process-global. No real CLI or live services.
#[path = "support/held_worker.rs"]
mod held_worker;

use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use c6_api::{Event, EventKind, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::enroll;
use nix::{sys::signal::killpg, unistd::Pid};
use serde_json::{Value, json};

/// A timed-out tokio::fs FIFO write cannot be cancelled by dropping its
/// future: its blocking task retains an O_RDWR descriptor (and thus a reader).
/// Drain independently BEFORE dropping the fixture's runtime, including on
/// assertion failure. Killing only the CLI/keeper is not sufficient cleanup.
struct DrainBeforeWorkerStop {
    worker: Option<held_worker::HeldWorker>,
    held: PathBuf,
}

impl Drop for DrainBeforeWorkerStop {
    fn drop(&mut self) {
        let held = self.held.clone();
        let drain = std::thread::spawn(move || {
            // This fixture owns one FIFO. Open without waiting for a writer,
            // then drain in blocking mode until worker shutdown/held cleanup
            // closes every writer. EOF is the completion signal, not a timer.
            for entry in std::fs::read_dir(&held).into_iter().flatten().flatten() {
                if let Ok(mut reader) =
                    std::fs::OpenOptions::new().read(true).custom_flags(nix::libc::O_NONBLOCK).open(entry.path().join("in"))
                {
                    // SAFETY: reader owns this valid descriptor; F_GETFL and
                    // F_SETFL query/update only its file status flags.
                    unsafe {
                        let flags = nix::libc::fcntl(reader.as_raw_fd(), nix::libc::F_GETFL);
                        assert!(flags >= 0, "reading cleanup FIFO flags: {}", std::io::Error::last_os_error());
                        assert_eq!(
                            nix::libc::fcntl(reader.as_raw_fd(), nix::libc::F_SETFL, flags & !nix::libc::O_NONBLOCK),
                            0,
                            "setting blocking cleanup FIFO: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                    std::io::copy(&mut reader, &mut std::io::sink()).expect("draining fixture FIFO through shutdown");
                }
            }
        });
        // HeldWorker stops all actor/turn tasks before removing held records.
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
        drain.join().expect("FIFO cleanup thread panicked");
    }
}

#[tokio::test]
async fn interrupt_ends_a_turn_blocked_writing_to_a_nonreading_held_cli() {
    let bins = tempfile::tempdir().unwrap();
    let claude = bins.path().join("claude");
    std::fs::write(&claude, r#"#!/bin/sh
[ "$1" = --version ] && exit 0
read -r line
printf '%s\n' '{"type":"system","subtype":"init"}'
uuid=$(printf '%s' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
printf '{"type":"user","isReplay":true,"uuid":"%s","parent_tool_use_id":null}\n' "$uuid"
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Ready, no longer reading stdin."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
printf '%s\n' '{"type":"result","subtype":"success","usage":{}}'
# The wrapper and CLI remain alive; no missing code file or stale output
# event is needed. Never consume the second input (or its interrupt).
read -r gate < hold
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this test binary owns the process-global override.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;

    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    nix::unistd::mkfifo(&project.path().join("hold"), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
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
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = held_worker::HeldWorker::start(state.path()).await;
    let worker_id = worker.id.clone();
    let cleanup = DrainBeforeWorkerStop { worker: Some(worker), held: state.path().join("claude/held") };
    let cc: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind":"claude_code", "name":"cc", "base_url":""}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let session: Value = http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id":worker_id, "cwd":project.path().display().to_string(), "connection_id":cc["id"], "model":"default", "prompt":"Start, then stop reading."}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let id = session["id"].as_str().unwrap();
    let events = async || -> Vec<Event> {
        http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    };
    eventually("the held CLI's first turn to complete normally", async || {
        events().await.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. })).then_some(())
    })
    .await;

    let (fifo, pg) = eventually("this session's held FIFO and process group", async || {
        for entry in std::fs::read_dir(&cleanup.held).ok()?.flatten() {
            let meta: Value = serde_json::from_slice(&std::fs::read(entry.path().join("meta.json")).ok()?).ok()?;
            if meta["session"] == id {
                return Some((entry.path().join("in"), Pid::from_raw(i32::try_from(meta["pgid"].as_i64()?).unwrap())));
            }
        }
        None
    })
    .await;
    // An independent read-only, nonblocking descriptor permits observing the
    // queued bytes without draining them or changing the worker's descriptor.
    let observer = std::fs::OpenOptions::new().read(true).custom_flags(nix::libc::O_NONBLOCK).open(&fifo).unwrap();

    // A report and an API input use the same initial send/write_line path.
    // Use a large API input here to isolate transport/cancellation from task
    // report truncation, subagent recovery and automatic-turn rate limits.
    let large = format!("BACKPRESSURE_REPORT:{}", "x".repeat(512 * 1024));
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text":large})).send().await.unwrap().error_for_status().unwrap();
    eventually("the large input to start its turn before its FIFO write completes", async || {
        let ev = events().await;
        let started = ev.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count();
        let ended = ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
        let recorded = ev.iter().any(|e| matches!(&e.kind, EventKind::User { text, .. } if text.starts_with("BACKPRESSURE_REPORT:")));
        (started == 2 && ended == 1 && recorded).then_some(())
    })
    .await;
    eventually("the oversized input to be partially queued in the nonreading FIFO", async || {
        let mut queued: nix::libc::c_int = 0;
        // SAFETY: observer is a live FIFO descriptor and FIONREAD writes one
        // c_int to this valid buffer on both Darwin and Linux. It reads no data.
        let result = unsafe { nix::libc::ioctl(observer.as_raw_fd(), nix::libc::FIONREAD, &mut queued) };
        assert_eq!(result, 0, "FIONREAD failed: {}", std::io::Error::last_os_error());
        // The CLI never reads the second input. Nonzero queued bytes prove
        // that writing has begun; fewer than the payload prove it is partial.
        (queued > 0 && queued < 512 * 1024).then_some(())
    })
    .await;
    http.post(format!("{api}/sessions/{id}/interrupt")).send().await.unwrap().error_for_status().unwrap();

    // A non-reading CLI must still be forcibly stopped even while the initial
    // send is backpressured. The shared test deadline only detects a hang.
    eventually("Stop to end the turn whose initial held FIFO write is backpressured", async || {
        events().await.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { turn: 2, reason: TurnEndReason::Interrupted })).then_some(())
    })
    .await;
    eventually("production Stop retires the partially written runtime's whole group before fixture cleanup", async || {
        matches!(killpg(pg, None), Err(nix::errno::Errno::ESRCH)).then_some(())
    })
    .await;
    drop(observer);
    // Cleanup also runs on an eventually panic: a failing test must itself
    // terminate rather than wait forever for Tokio's blocking FIFO writer.
    drop(cleanup);
    head.shutdown().await.unwrap();
}
