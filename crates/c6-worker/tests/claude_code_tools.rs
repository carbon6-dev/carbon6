//! Which of carbon6's tools a Claude Code session gets over carbon6's MCP
//! server is a setting (docs/background.md §4): chosen in any profile's
//! naming, with carbon6's guidance for them in the server's instructions.
//! A binary of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;

use c6_api::{SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that records what carbon6's MCP server offers it.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
url=""
while [ $# -gt 0 ]; do [ "$1" = "--mcp-config" ] && url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/'); shift; done
post() { curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }
read -r line
echo '{"type":"system","subtype":"init"}'
post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' > init.json
post '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' > tools.json
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Looked."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Looked.","usage":{"input_tokens":1,"output_tokens":1}}'
while read -r line; do :; done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn claude_code_gets_the_carbon6_tools_you_choose() {
    if !c6_testkit::have("curl") {
        return;
    }
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

    // The defaults, and what may be chosen.
    let setting: Value = http.get(format!("{api}/settings/claude-code-tools")).send().await.unwrap().json().await.unwrap();
    assert_eq!(setting["tools"], json!(["Task", "SendMessage", "TaskOutput", "TaskStop", "doc", "MessageSession", "ListSessions"]));
    let available: Vec<&str> = setting["available"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(available.contains(&"PublishFile") && !available.contains(&"Bash"), "{available:?}");
    // Not carbon6's to offer: Claude Code has its own.
    let r = http.put(format!("{api}/settings/claude-code-tools")).json(&json!({"tools": ["doc", "Bash"]})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("Bash"));
    // Chosen in either naming.
    let r = http
        .put(format!("{api}/settings/claude-code-tools"))
        .json(&json!({"tools": ["doc", "publish_file", "remember", "Forget"]}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    // A note for the memory guidance to carry.
    let r = http.post(format!("{api}/memories")).json(&json!({"text": "The build runs on thalassa."})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());

    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
            "connection_id": cc["id"], "model": "opus", "prompt": "look around"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    eventually("the turn to end", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.session.end_reason.is_some()).then_some(())
    })
    .await;
    let read = |f: &str| -> Value { serde_json::from_str(&std::fs::read_to_string(project.path().join(f)).unwrap()).unwrap() };
    let tools = read("tools.json");
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["doc", "PublishFile", "Remember", "Forget"], "only those chosen (no connection for subagents here)");
    let instructions = read("init.json")["result"]["instructions"].as_str().unwrap().to_owned();
    assert!(instructions.contains("# Memory") && instructions.contains("The build runs on thalassa."), "{instructions}");
    stop.cancel();
    head.shutdown().await.unwrap();
}
