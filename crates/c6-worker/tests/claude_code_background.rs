//! A Claude Code session's carbon6 subagent runs in the background
//! (docs/background.md §4): the call returns at once, the turn ends, and
//! the subagent's report reaches the same Claude Code process as a notice,
//! which starts a turn. A binary of its own: it sets C6_CLAUDE_BIN.

#[path = "support/held_worker.rs"]
mod held_worker;

use std::path::Path;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::enroll;
use serde_json::{Value, json};

/// A `claude` that stays up. Its first message: it starts carbon6's `Task`
/// in the background over MCP (with curl) and ends its turn. Every message
/// after that (the notice): it records it and answers.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
echo $$ >> spawns.txt
url=""
while [ $# -gt 0 ]; do [ "$1" = "--mcp-config" ] && url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/'); shift; done
post() { curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
read -r line
echo '{"type":"system","subtype":"init"}'
printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$line")"
post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' > /dev/null
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_b1","name":"mcp__carbon6__Task","input":{"description":"look","prompt":"Look around.","connection":"api","run_in_background":true}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
post '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"Task","arguments":{"description":"look","prompt":"Look around.","connection":"api","run_in_background":true}}}' > call.json
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_b1","content":"started"}]},"parent_tool_use_id":null}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Started it."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Started it.","usage":{"input_tokens":1,"output_tokens":1}}'
while read -r line; do
  printf '%s\n' "$line" >> notices.txt
  echo '{"type":"system","subtype":"init"}'
  printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$line")"
  echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Got the report."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
  echo '{"type":"result","subtype":"success","is_error":false,"result":"Got the report.","usage":{"input_tokens":1,"output_tokens":1}}'
done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn a_background_carbon6_subagent_reports_to_claude_code() {
    if !c6_testkit::have("curl") {
        return;
    }
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
    let mock = MockLlm::start().await;
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

    let add =
        async |body: Value| -> Value { http.post(format!("{api}/connections")).json(&body).send().await.unwrap().json().await.unwrap() };
    let cc = add(json!({"kind": "claude_code", "name": "cc", "base_url": ""})).await;
    add(json!({"name": "api", "base_url": mock.base_url(), "default_model": "m2"})).await;
    mock.push(Reply::Text("api says hi".into()));

    let project = tempfile::tempdir().unwrap();
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.id, "cwd": project.path().display().to_string(),
            "connection_id": cc["id"], "model": "opus", "prompt": "delegate in the background"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    // Two turns: the one that started it, and the one its report started.
    let events = eventually("the report's turn to end", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        let events: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
        let ended = events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
        (d.session.status == SessionStatus::Idle && ended == 2).then_some(events)
    })
    .await;

    let call: Value = serde_json::from_str(&std::fs::read_to_string(project.path().join("call.json")).unwrap()).unwrap();
    let started = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(started.starts_with("Started the subagent in the background as bg"), "the call returns at once: {call}");
    // The report went to the same process, as a message it took.
    let notices = std::fs::read_to_string(project.path().join("notices.txt")).unwrap();
    let notice: Value = serde_json::from_str(notices.lines().next().unwrap()).unwrap();
    let text = notice["message"]["content"].as_str().unwrap();
    assert!(notice.get("origin").is_none(), "a notice is not a person's message: {notice}");
    assert!(text.contains("<background-task") && text.contains("api says hi"), "{text}");
    assert_eq!(std::fs::read_to_string(project.path().join("spawns.txt")).unwrap().lines().count(), 1, "one process");
    let woke = events.iter().find_map(|e| match &e.kind {
        EventKind::User { command, text, .. } if command == "task-notification" => Some(text.clone()),
        _ => None,
    });
    assert!(woke.is_some_and(|t| t.contains("api says hi")), "the notice is the second turn's message");
    worker.stop();
    head.shutdown().await.unwrap();
}
