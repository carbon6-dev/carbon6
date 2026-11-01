//! carbon6's policies hold Claude Code's calls of carbon6's tools over the
//! MCP bridge, as they hold carbon6's own loop's: an ask rule waits for your
//! approval, a deny rule refuses (docs/background.md §6). Claude Code
//! doesn't know carbon6's policies; the bridge applies them. A binary of its
//! own: it sets C6_CLAUDE_BIN.

use std::path::Path;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that calls ListSessions, then doc, over MCP (with curl),
/// streaming each call first as Claude Code does, and records the answers.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
url=""
while [ $# -gt 0 ]; do [ "$1" = "--mcp-config" ] && url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/'); shift; done
post() { curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }
read -r line
echo '{"type":"system","subtype":"init"}'
post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' > /dev/null
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_l1","name":"mcp__carbon6__ListSessions","input":{}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
post '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ListSessions","arguments":{}}}' > list.json
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_l1","content":"listed"}]},"parent_tool_use_id":null}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_d1","name":"mcp__carbon6__doc","input":{"action":"list"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
post '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"doc","arguments":{"action":"list"}}}' > doc.json
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_d1","content":"refused"}]},"parent_tool_use_id":null}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Done.","usage":{"input_tokens":1,"output_tokens":1}}'
while read -r line; do :; done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn policies_hold_claude_codes_calls_of_carbon6s_tools() {
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
    // One rule by the tool's own name, one by the name Claude Code knows it by.
    let policies = json!([
        {"name": "Ask before listing", "tools": "ListSessions", "action": "ask"},
        {"name": "No documents here", "tools": "mcp__carbon6__doc", "action": "deny", "message": "not in this test"}
    ]);
    assert!(http.put(format!("{api}/settings/policies")).json(&policies).send().await.unwrap().status().is_success());

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
    let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };

    // The listing waits for your approval, at the call it belongs to.
    let call = eventually("the approval request", async || {
        events().await.iter().find_map(|e| match &e.kind {
            EventKind::Approval { call_id, state, policy, .. } if state == "requested" && policy == "Ask before listing" => {
                Some(call_id.clone())
            }
            _ => None,
        })
    })
    .await;
    assert_eq!(call, "toolu_l1");
    // (The shell has made the file it writes the answer to; no answer yet.)
    let answered = std::fs::metadata(project.path().join("list.json")).is_ok_and(|m| m.len() > 0);
    assert!(!answered, "nothing ran while it waits");
    let r = http.post(format!("{api}/sessions/{id}/approvals/{call}")).json(&json!({"allow": true})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    eventually("the turn to end", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.session.end_reason.is_some()).then_some(())
    })
    .await;

    let read = |f: &str| -> Value { serde_json::from_str(&std::fs::read_to_string(project.path().join(f)).unwrap()).unwrap() };
    let list = read("list.json");
    assert_eq!(list["result"]["isError"], false, "approved, it ran: {list}");
    let doc = read("doc.json");
    assert_eq!(doc["result"]["isError"], true, "{doc}");
    let refusal = doc["result"]["content"][0]["text"].as_str().unwrap();
    assert!(refusal.contains("Denied by policy \"No documents here\"") && refusal.contains("not in this test"), "{refusal}");
    stop.cancel();
    head.shutdown().await.unwrap();
}
