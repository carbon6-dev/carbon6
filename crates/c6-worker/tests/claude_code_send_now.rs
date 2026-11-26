//! Send now in a Claude Code session (docs/background.md §4): the message
//! goes from the API through the head and the worker to the Claude Code
//! process, labelled as automation without a human marker, followed by an
//! interrupt that names it with `send_now`, so Claude Code delivers it
//! without stopping its background agents. Both ways: a message sent with
//! Send now, and ⏵ Send now on a message already with the agent. A binary
//! of its own: it sets C6_CLAUDE_BIN.

#[path = "support/held_worker.rs"]
mod held_worker;

use std::path::Path;
use std::{io::Write, os::unix::fs::OpenOptionsExt};

use c6_api::{Event, EventKind, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::enroll;
use serde_json::{Value, json};

async fn release_gate(dir: &Path, name: &str) {
    // A blocking FIFO open cannot be cancelled by an async deadline. If the
    // fixture never reaches this reader, fail boundedly instead of hanging.
    eventually("the CLI to read its placement gate", async || {
        let mut gate = std::fs::OpenOptions::new().write(true).custom_flags(nix::libc::O_NONBLOCK).open(dir.join(name)).ok()?;
        gate.write_all(b"continue\n").ok()
    })
    .await;
}

/// A `claude` that stays up and records what it reads (`stdin.txt`). A
/// message starts a turn that waits on a foreground command; a message
/// written meanwhile waits for a Send now, which it answers as the turn's
/// message says: "delivering" (the command goes to the background and the
/// message joins the turn) or "stopped" (only the turn stops; the message
/// starts the next).
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
req_of() { printf '%s' "$1" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p'; }
echo_of() { printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$1"; }
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}\n' "$1"; }
answer() { printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"still_queued":[],"send_now":"%s"}}}\n' "$1" "$2"; }
init() { printf '%s\n' '{"type":"system","subtype":"init"}'; }
done_ok() { printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":1,"output_tokens":1}}'; }
mode=""
waiting=""
n=0
while read -r line; do
  printf '%s\n' "$line" >> stdin.txt
  case "$line" in
    *'"interrupt"'*)
      answer "$(req_of "$line")" "$mode"
      if [ "$mode" = delivering ]; then
        printf '{"type":"system","subtype":"task_updated","task_id":"fg%s","patch":{"is_backgrounded":true}}\n' "$n"
        printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_fg%s","content":"Moved to the background."}]},"parent_tool_use_id":null}\n' "$n"
        echo_of "$waiting"
        say "Got it."
        done_ok
      else
        printf '{"type":"system","subtype":"task_notification","task_id":"fg%s","status":"stopped"}\n' "$n"
        printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"usage":{"input_tokens":1,"output_tokens":1}}'
        init
        echo_of "$waiting"
        say "Took it."
        done_ok
      fi
      mode=""
      continue;;
    *'"control_request"'*) continue;;
  esac
  if [ -n "$mode" ]; then
    waiting=$(uuid_of "$line")
    case "$mode" in
      placement-*)
        read -r release < placement.gate
        printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_fg%s","content":"Done."}]},"parent_tool_use_id":null}\n' "$n"
        if [ "$mode" = placement-echo ]; then
          echo_of "$waiting"
        fi
        say "Mid-turn reply."
        read -r release < finish.gate
        done_ok
        mode="";;
    esac
    continue
  fi
  case "$line" in
    *placement-no-echo*) mode=placement-no-echo;;
    *placement-echo*) mode=placement-echo;;
    *delivering*) mode=delivering;;
    *) mode=stopped;;
  esac
  n=$((n+1))
  init
  echo_of "$(uuid_of "$line")"
  printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_fg%s","name":"Bash","input":{"command":"make"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}\n' "$n"
  printf '{"type":"system","subtype":"task_started","task_id":"fg%s","tool_use_id":"toolu_fg%s","description":"Long build","task_type":"local_bash","is_backgrounded":false}\n' "$n" "$n"
done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn send_now_reaches_claude_code_as_a_send_now() {
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
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
    let worker = held_worker::HeldWorker::start(state.path()).await;
    let cc: Value = http
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
        .json(&json!({"worker_id": worker.id, "cwd": project.path().display().to_string(),
            "connection_id": cc["id"], "model": "opus", "prompt": "build it (delivering)"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    let events = async || -> Option<Vec<Event>> { http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok() };
    let detail = async || -> Option<SessionDetail> { http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok() };
    let ends = |evs: &[Event]| -> Vec<TurnEndReason> {
        evs.iter()
            .filter_map(|e| match &e.kind {
                EventKind::TurnEnded { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    };
    let read = || -> Vec<Value> {
        std::fs::read_to_string(project.path().join("stdin.txt")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    };
    // Once its command holds the turn.
    let holding = async |n: usize| {
        let last = std::sync::Mutex::new(Vec::new());
        c6_testkit::eventually_or(
            "the turn to wait on its command",
            async || {
                let evs = events().await?;
                *last.lock().unwrap() = evs.clone();
                (evs.iter().filter(|e| matches!(&e.kind, EventKind::BackgroundTask { state, .. } if state == "started")).count() >= n)
                    .then_some(())
            },
            || format!("events: {:#?}", last.lock().unwrap()),
        )
        .await
    };

    // A message sent with Send now: Claude Code delivers it into the turn.
    holding(1).await;
    let r = http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "PING", "send_now": true})).send().await.unwrap();
    assert!(r.status().is_success());
    let evs = eventually("the turn to end", async || {
        let evs = events().await?;
        (ends(&evs).len() == 1).then_some(evs)
    })
    .await;
    assert_eq!(ends(&evs), [TurnEndReason::Completed], "delivered into the running turn, not stopped");
    let lines = read();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[1]["message"]["content"], "Message sent by an automation, not typed by the user:\nPING");
    assert!(lines[1].get("origin").is_none(), "automation has no human marker: {:?}", lines[1]);
    assert_eq!(lines[2]["request"], json!({"subtype": "interrupt", "send_now": true, "message_uuid": lines[1]["uuid"]}));
    // The command it moved runs on as a background task.
    let moved: Vec<(String, bool)> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::BackgroundTask { state, native_type, foreground, .. } if state == "started" => {
                Some((native_type.clone(), *foreground))
            }
            _ => None,
        })
        .collect();
    assert_eq!(moved, [("local_bash".to_owned(), true), ("local_bash".to_owned(), false)]);

    // ⏵ Send now on a message already with the agent: Claude Code stops only
    // the turn, which ends interrupted; the message starts the next.
    // A genuinely changed snapshot, not just the equivalent refresh sent
    // before every input, must not block instructions to this still-running
    // native process behind the background task those instructions can end.
    http.put(format!("{api}/settings/prompts/session.base"))
        .json(&json!({"body": "Updated guidance for the next quiet native boundary."}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "build again"})).send().await.unwrap();
    holding(3).await;
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "later"})).send().await.unwrap();
    let queued = eventually("the message to be with the agent", async || {
        let d = detail().await?;
        d.queued.iter().find(|q| q.text == "later" && q.with_agent).map(|q| q.command_id.clone())
    })
    .await;
    let r = http.post(format!("{api}/sessions/{id}/queue/{queued}/now")).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT);
    let evs = eventually("its turn to end", async || {
        let evs = events().await?;
        (ends(&evs).len() == 3).then_some(evs)
    })
    .await;
    // Nothing queued: it was the turn's.
    let d = detail().await.unwrap();
    assert!(d.queued.is_empty(), "{:?}", d.queued);
    assert_ne!(d.session.status, SessionStatus::Failed);
    assert_eq!(ends(&evs), [TurnEndReason::Completed, TurnEndReason::Interrupted, TurnEndReason::Completed]);
    assert!(!evs.iter().any(|e| matches!(e.kind, EventKind::Error { .. })), "a stop for Send now is not a failure");
    let lines = read();
    let later = lines.iter().find(|l| l["message"]["content"] == "Message sent by an automation, not typed by the user:\nlater").unwrap();
    assert!(later.get("origin").is_none(), "automation has no human marker: {later}");
    assert_eq!(lines.last().unwrap()["request"], json!({"subtype": "interrupt", "send_now": true, "message_uuid": later["uuid"]}));
    let last_user = evs.iter().rev().find_map(|e| match &e.kind {
        EventKind::User { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(last_user.as_deref(), Some("later"), "the message starts the next turn");

    // Ordinary mid-turn delivery does not need Send now or the turn's result.
    // Explicit FIFO gates let us observe written-but-not-taken, then the exact
    // placement, while the same turn is still running. Without an echo even a
    // subsequent assistant message must not pretend the CLI took the input.
    for echo in [true, false] {
        let project = tempfile::tempdir().unwrap();
        for name in ["placement.gate", "finish.gate"] {
            nix::unistd::mkfifo(&project.path().join(name), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
        }
        let mode = if echo { "placement-echo" } else { "placement-no-echo" };
        let s: Value = http
            .post(format!("{api}/sessions"))
            .json(&json!({"worker_id": worker.id, "cwd": project.path().display().to_string(),
                "connection_id": cc["id"], "model": "opus", "prompt": mode}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = s["id"].as_str().unwrap();
        let events =
            async || -> Option<Vec<Event>> { http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok() };
        let detail = async || -> Option<SessionDetail> { http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok() };
        eventually("the placement turn to hold its tool", async || {
            events().await?.iter().any(|e| matches!(&e.kind, EventKind::BackgroundTask { state, .. } if state == "started")).then_some(())
        })
        .await;
        http.post(format!("{api}/sessions/{id}/messages"))
            .json(&json!({"text": "mid-turn input"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let command = eventually("the mid-turn input to be written but not taken", async || {
            detail().await?.queued.iter().find(|q| q.text == "mid-turn input" && q.with_agent).map(|q| q.command_id.clone())
        })
        .await;
        assert!(!events().await.unwrap().iter().any(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id == &command)));
        release_gate(project.path(), "placement.gate").await;
        let evs = eventually("the CLI's next reply while the turn stays open", async || {
            let evs = events().await?;
            evs.iter()
                .any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if blocks.iter().any(|b| matches!(b, c6_api::ContentBlock::Text { text, .. } if text == "Mid-turn reply."))))
                .then_some(evs)
        })
        .await;
        assert!(!evs.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { .. })), "placement is not deferred until result");
        let users: Vec<_> =
            evs.iter().filter(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id == &command)).collect();
        if echo {
            assert_eq!(users.len(), 1, "the echo places the input exactly once");
            let tool = evs.iter().find(|e| matches!(e.kind, EventKind::ToolResult { .. })).unwrap();
            let reply = evs.iter().rev().find(|e| matches!(e.kind, EventKind::Assistant { .. })).unwrap();
            assert!(tool.seq < users[0].seq && users[0].seq < reply.seq, "tool result < taken input < next reply");
            eventually("the placed input to leave the queue", async || {
                (!detail().await?.queued.iter().any(|q| q.command_id == command)).then_some(())
            })
            .await;
        } else {
            assert!(users.is_empty(), "no echo means no evidence that the CLI took the input");
            assert!(detail().await.unwrap().queued.iter().any(|q| q.command_id == command && q.with_agent));
        }
        let state: String =
            sqlx::query_scalar("SELECT state FROM commands WHERE id = ?").bind(&command).fetch_one(head.state.store.pool()).await.unwrap();
        assert_eq!(state, if echo { "done" } else { "pending" });
        release_gate(project.path(), "finish.gate").await;
        eventually("the placement turn to finish", async || {
            events().await?.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { .. })).then_some(())
        })
        .await;
    }
    worker.stop();
    head.shutdown().await.unwrap();
}
