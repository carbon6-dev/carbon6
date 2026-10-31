//! A Claude Code session writes carbon6 documents (docs/docs.md §9): each
//! turn's MCP server offers `doc` (so Claude Code calls
//! `mcp__carbon6__doc`), even where no subagents are offered. The `claude`
//! here is a stand-in that speaks the MCP protocol with curl, as Claude Code
//! does; the real CLI isn't run. A binary of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;

use c6_api::{SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that lists the MCP server's tools, then creates a document
/// with a pending section and fills it, as `mcp__carbon6__doc` calls.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let create = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "doc", "arguments": {
        "action": "create", "title": "Design notes", "markdown": "# Design notes\n\n<?c6 block a?>",
        "blocks": {"a": {"type": "pending", "intent": "Why: the problem"}}}}});
    let outline = json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "doc", "arguments": {"action": "list"}}});
    std::fs::write(dir.join("create.json"), create.to_string()).unwrap();
    std::fs::write(dir.join("list.json"), outline.to_string()).unwrap();
    let script = format!(
        r#"#!/bin/sh
cat > /dev/null
url=""
while [ $# -gt 0 ]; do [ "$1" = "--mcp-config" ] && url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/'); shift; done
if [ -z "$url" ]; then
  echo '{{"type":"result","subtype":"success","is_error":false,"result":"no mcp","usage":{{"input_tokens":1,"output_tokens":1}}}}'
  exit 0
fi
post() {{ curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }}
post '{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"fake","version":"0"}}}}}}' > init.json
post '{{"jsonrpc":"2.0","method":"notifications/initialized"}}' > /dev/null
post '{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}' > tools.json
echo '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_d1","name":"mcp__carbon6__doc","input":{{"action":"create"}}}}],"stop_reason":"tool_use"}},"parent_tool_use_id":null}}'
post "$(cat {dir}/create.json)" > create.out
echo '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_d1","content":"ok"}}]}},"parent_tool_use_id":null}}'
post "$(cat {dir}/list.json)" > list.out
echo '{{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{{"input_tokens":1,"output_tokens":1}}}}'
"#,
        dir = dir.display()
    );
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn claude_code_writes_documents_through_mcp() {
    if !c6_testkit::have("curl") {
        return;
    }
    let bins = tempfile::tempdir().unwrap();
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", fake_claude(bins.path())) };
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

    // Only Claude Code: no other connection, so no subagents are offered.
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
            "connection_id": cc["id"], "model": "opus", "prompt": "write the design notes", "work": "new"}))
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

    let read = |f: &str| -> Value { serde_json::from_str(&std::fs::read_to_string(project.path().join(f)).unwrap()).unwrap() };
    let tools = read("tools.json");
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["doc"]);
    assert!(tools["result"]["tools"][0]["inputSchema"]["properties"]["action"]["enum"].to_string().contains("create"));
    let created = read("create.out");
    assert_eq!(created["result"]["isError"], false, "{created}");
    let made: Value = serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let doc_id = made["doc"]["id"].as_str().unwrap();
    // It's carbon6's document, written by this session, in its work.
    let doc: Value = http.get(format!("{api}/docs/{doc_id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!((doc["title"].as_str(), doc["created_by"].as_str()), (Some("Design notes"), Some(format!("session:{id}").as_str())));
    assert_eq!(doc["pending"], 1);
    let listed = read("list.out");
    let mine: Value = serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mine["yours"][0]["id"], doc_id, "{mine}");
    let work: Value =
        http.get(format!("{api}/work/of")).query(&[("ref", format!("session:{id}"))]).send().await.unwrap().json().await.unwrap();
    assert!(work[0]["attached"].to_string().contains(&format!("doc:{doc_id}")), "{work}");
    stop.cancel();
    head.shutdown().await.unwrap();
}
