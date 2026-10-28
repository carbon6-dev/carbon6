//! A Claude Code session delegates to carbon6's subagents (docs/subagents.md
//! §11.5): each turn gets `Task` and `SendMessage` from a local MCP server,
//! and the subagent runs in carbon6's own loop on the connection asked for.
//! A binary of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;

use c6_api::{Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that speaks MCP to the server its `--mcp-config` names (with
/// curl): it lists the tools, then calls `Task` for a subagent on "api",
/// streaming the call first as Claude Code does.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
cat > /dev/null
url=""
while [ $# -gt 0 ]; do [ "$1" = "--mcp-config" ] && url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/'); shift; done
if [ -z "$url" ]; then
  echo '{"type":"result","subtype":"success","is_error":false,"result":"no mcp","usage":{"input_tokens":1,"output_tokens":1}}'
  exit 0
fi
post() { curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }
post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' > init.json
post '{"jsonrpc":"2.0","method":"notifications/initialized"}' > /dev/null
post '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' > tools.json
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_m1","name":"mcp__carbon6__Task","input":{"description":"look","prompt":"Look around.","connection":"api"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
post '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"Task","arguments":{"description":"look","prompt":"Look around.","connection":"api"}}}' > call.json
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_m1","content":"ok"}]},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":1,"output_tokens":1}}'
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn claude_code_delegates_to_carbon6_subagents() {
    if !c6_testkit::have("curl") {
        return;
    }
    let bins = tempfile::tempdir().unwrap();
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", fake_claude(bins.path())) };
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
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();

    let add =
        async |body: Value| -> Value { http.post(format!("{api}/connections")).json(&body).send().await.unwrap().json().await.unwrap() };
    let cc = add(json!({"kind": "claude_code", "name": "cc", "base_url": ""})).await;
    let api_conn = add(json!({"name": "api", "base_url": mock.base_url(), "default_model": "m2"})).await;
    mock.push(Reply::Text("api says hi".into()));

    let project = tempfile::tempdir().unwrap();
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
            "connection_id": cc["id"], "model": "opus", "prompt": "delegate"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    eventually("turn done", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.session.end_reason.is_some()).then_some(())
    })
    .await;

    let init: Value = serde_json::from_str(&std::fs::read_to_string(project.path().join("init.json")).unwrap()).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "carbon6");
    let tools: Value = serde_json::from_str(&std::fs::read_to_string(project.path().join("tools.json")).unwrap()).unwrap();
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Task", "SendMessage"]);
    let task = &tools["result"]["tools"][0]["inputSchema"]["properties"];
    assert_eq!(task["connection"]["enum"], json!(["api"]), "Claude Code is not offered to subagents");
    assert!(task.get("run_in_background").is_none(), "{task}");
    let call: Value = serde_json::from_str(&std::fs::read_to_string(project.path().join("call.json")).unwrap()).unwrap();
    assert_eq!(call["result"]["isError"], false, "{call}");
    assert!(call["result"]["content"][0]["text"].as_str().unwrap().starts_with("api says hi"), "{call}");

    // carbon6 ran it, on the API connection's default model, under the call.
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["model"], "m2");
    assert!(!reqs[0].to_string().contains("Claude Code's own system prompt"), "a carbon6 prompt of its own");
    let children: Vec<Session> = http.get(format!("{api}/sessions/{id}/children")).send().await.unwrap().json().await.unwrap();
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert!(!child.observed, "carbon6's own, not mirrored");
    assert_eq!(child.parent_call_id.as_deref(), Some("toolu_m1"));
    assert_eq!((child.connection_id.as_str(), child.model.as_str()), (api_conn["id"].as_str().unwrap(), "m2"));
    stop.cancel();
    head.shutdown().await.unwrap();
}
