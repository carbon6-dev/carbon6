//! Stop in a Claude Code session (docs/background.md §4, Stopping): what
//! ends because you stopped the agent doesn't wake it. A turn Claude Code
//! starts by itself right after the stop is stopped too: no turn shows, only
//! the notice it put into its conversation, word for word from its
//! transcript, and a quiet line; a task lost because Claude Code didn't stop (so its process was) is told
//! with your next message; a task that ends on its own still wakes the
//! agent. From the API through the head and the worker to the Claude Code
//! process. A binary of its own: it sets C6_CLAUDE_BIN and CLAUDE_CONFIG_DIR.

use std::path::Path;

use c6_api::{Event, EventKind, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that stays up and records what it reads (`stdin.txt`).
/// - "hold": a build and an agent go to the background, the agent finishes
///   (Claude Code queues its notification), and a command holds the turn.
///   Stopped, it stops the command and, as 2.1.311 does, starts a turn by
///   itself for what it queued, whose text it writes only to its transcript
///   (`$CLAUDE_CONFIG_DIR/projects/<cwd>/<conversation>.jsonl`). Stopped
///   again, it says "[Request interrupted by user]" and waits; then the build
///   ends on its own and it wakes for that.
/// - "stubborn": a server goes to the background and a command holds the
///   turn; it never answers a stop, so carbon6 stops the process.
/// - A notice (`<background-task …>`) opens a turn the message after it
///   joins; anything else is answered.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
echo_of() { printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$1")"; }
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}\n' "$1"; }
init() { printf '%s\n' '{"type":"system","subtype":"init"}'; }
done_ok() { printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":1,"output_tokens":1}}'; open=""; }
stopped() { printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"usage":{"input_tokens":1,"output_tokens":1}}'; open=""; }
started() { printf '{"type":"system","subtype":"task_started","task_id":"%s","description":"%s","task_type":"%s","is_backgrounded":%s}\n' "$1" "$2" "$3" "$4"; }
said() { printf '{"type":"user","uuid":"%s","message":{"role":"user","content":[{"type":"text","text":"%s"}]},"parent_tool_use_id":null}\n' "$1" "$2"; }
sid=""
prev=""
for a in "$@"; do
  if [ "$prev" = --session-id ] || [ "$prev" = --resume ]; then sid="$a"; fi
  prev="$a"
done
transcript() { t="$CLAUDE_CONFIG_DIR/projects/$(pwd | sed 's/[^A-Za-z0-9]/-/g')"; mkdir -p "$t"; printf '%s\n' "$1" >> "$t/$sid.jsonl"; }
mode=""
open=""
while read -r line; do
  printf '%s\n' "$line" >> stdin.txt
  case "$line" in
    *'"interrupt"'*)
      case "$mode" in
        hold)
          printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"fg1","status":"stopped","summary":"Wait"}'
          said u-stop "[Request interrupted by user for tool use]"
          stopped
          transcript '{"type":"user","uuid":"u-stop","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}'
          transcript '{"type":"user","uuid":"n-ag1","isSidechain":false,"origin":{"kind":"task-notification","producer":"session-task"},"turnOrigin":"task_notification","message":{"role":"user","content":"<task-notification>\n<task-id>ag1</task-id>\n<status>completed</status>\n<summary>Agent \"Review\" is waiting — on its own work</summary>\n<result>Half done: 3 of 7 files.</result>\n</task-notification>"}}'
          init
          open=1
          mode=flushed;;
        flushed)
          said u-stop2 "[Request interrupted by user]"
          transcript '{"type":"user","uuid":"u-stop2","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}'
          stopped
          sleep 0.3
          printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"bg1","status":"completed","summary":"Build finished"}'
          init
          say "The build passed."
          done_ok
          mode="";;
      esac
      continue;;
    *'"control_request"'*) continue;;
  esac
  [ -z "$open" ] && init
  open=1
  echo_of "$line"
  case "$line" in
    *background-task*) ;;
    *hold*)
      mode=hold
      started bg1 Build local_bash true
      started ag1 Review local_agent true
      printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"ag1","status":"completed","summary":"Reviewed"}'
      printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_fg1","name":"Bash","input":{"command":"sleep 90"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
      started fg1 Wait local_bash false;;
    *stubborn*)
      mode=stubborn
      started srv Server local_bash true
      printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_fg2","name":"Bash","input":{"command":"sleep 90"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
      started fg2 Wait local_bash false;;
    *)
      say "Next."
      done_ok;;
  esac
done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The notice the stand-in put into its conversation.
const QUEUED: &str = "<task-notification>\n<task-id>ag1</task-id>\n<status>completed</status>\n<summary>Agent \"Review\" is waiting — on its own work</summary>\n<result>Half done: 3 of 7 files.</result>\n</task-notification>";

#[tokio::test]
async fn what_ends_because_you_stopped_the_agent_does_not_wake_it() {
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // Where Claude Code (the stand-in) keeps its transcripts.
    let config = tempfile::tempdir().unwrap();
    // SAFETY: this binary has one test; nothing else reads the variables.
    unsafe {
        std::env::set_var("C6_CLAUDE_BIN", &claude);
        std::env::set_var("CLAUDE_CONFIG_DIR", config.path());
    }
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
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
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
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
            "connection_id": cc["id"], "model": "opus", "prompt": "hold on"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    let events = async || -> Option<Vec<Event>> { http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok() };
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
    let interrupts = || read().iter().filter(|l| l["request"]["subtype"] == "interrupt").count();
    // Once a foreground command holds the turn.
    let holding = async |n: usize| {
        eventually("the turn to wait on its command", async || {
            let evs = events().await?;
            let held =
                evs.iter().filter(|e| matches!(&e.kind, EventKind::BackgroundTask { state, foreground: true, .. } if state == "started"));
            (held.count() >= n).then_some(())
        })
        .await
    };
    let interrupt = async || {
        let r = http.post(format!("{api}/sessions/{id}/interrupt")).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.status());
    };

    // You stop it. The turn Claude Code starts by itself at once is stopped
    // too: no turn shows, only the notice, as it reached the model, and a
    // quiet line; the build that then ends on its own wakes it.
    holding(1).await;
    interrupt().await;
    let evs = eventually("the build's turn to end", async || {
        let evs = events().await?;
        (ends(&evs).len() == 2).then_some(evs)
    })
    .await;
    assert_eq!(ends(&evs), [TurnEndReason::Interrupted, TurnEndReason::Completed]);
    assert_eq!(interrupts(), 2, "yours, then carbon6's for the turn Claude Code started by itself");
    let users: Vec<(String, String)> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::User { text, command, .. } => Some((text.clone(), command.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        [
            ("hold on".to_owned(), String::new()),
            (QUEUED.to_owned(), "task-notification".to_owned()),
            ("Build finished".to_owned(), "task-notification".to_owned())
        ],
        "what Claude Code queued before the stop is shown word for word, and gets no turn"
    );
    let told = evs.iter().position(|e| matches!(&e.kind, EventKind::User { text, .. } if text == QUEUED)).unwrap();
    assert!(
        matches!(&evs[told + 1].kind, EventKind::Notice { title, .. } if title == "Stopped before replying"),
        "then the quiet line: {:?}",
        evs[told + 1].kind
    );
    let turns: Vec<u64> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::TurnStarted { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns, [1, 2], "the stopped turn took no number");

    // Claude Code doesn't stop when asked, so its process is stopped: its
    // server is lost, and the agent hears it with your next message.
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "stubborn"})).send().await.unwrap();
    holding(2).await;
    interrupt().await;
    eventually("the server to be lost and told", async || {
        let evs = events().await?;
        let lost = evs
            .iter()
            .any(|e| matches!(&e.kind, EventKind::BackgroundTask { state, description, .. } if state == "lost" && description == "Server"));
        let told = evs.iter().any(|e| matches!(&e.kind, EventKind::NoticeQueued { text, .. } if text.contains("Server")));
        (ends(&evs).len() == 3 && lost && told).then_some(())
    })
    .await;
    http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": "what now"})).send().await.unwrap();
    let evs = eventually("your message's turn to end", async || {
        let evs = events().await?;
        (ends(&evs).len() == 4).then_some(evs)
    })
    .await;
    assert_eq!(ends(&evs)[2..], [TurnEndReason::Interrupted, TurnEndReason::Completed]);
    // Your message began the turn, with the notices before it: none woke the agent.
    let last = evs.iter().rposition(|e| matches!(e.kind, EventKind::TurnStarted { .. })).unwrap();
    let before = evs[..last].iter().rposition(|e| matches!(e.kind, EventKind::TurnEnded { .. })).unwrap();
    let opened: Vec<&str> = evs[before..last]
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(opened.last(), Some(&"what now"), "{opened:?}");
    assert!(opened.iter().any(|t| t.contains("Server") && t.contains("lost")), "{opened:?}");
    let lines = read();
    let yours =
        lines.iter().position(|l| l["message"]["content"] == "Message sent by an automation, not typed by the user:\nwhat now").unwrap();
    assert!(lines[yours].get("origin").is_none(), "automation has no human marker: {:?}", lines[yours]);
    assert!(lines[..yours].iter().rev().take_while(|l| l["type"] == "user").any(|l| l.to_string().contains("Server")), "{lines:?}");
    stop.cancel();
    head.shutdown().await.unwrap();
}
