//! A Claude Code child of carbon6's own loop owns the policy gate on its
//! MCP bridge, and gets hooks filtered against its own labels. Its held
//! calls must reach the parent's waiting list and the bell. A binary of
//! its own: it sets C6_CLAUDE_BIN.

#[path = "support/held_worker.rs"]
mod held_worker;

use std::path::Path;
use std::time::Duration;

use c6_api::{Attention, AttentionItem, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::enroll;
use serde_json::{Value, json};

const ASK_COMMAND: &str = "printf asked > child-ask-effect";
const DENY_COMMAND: &str = "printf forbidden > child-deny-effect";

/// Calls carbon6's Task over MCP, asking an API subagent to run a command.
/// Native Bash is Claude Code's own tool, not carbon6's policy bridge; the
/// marker commands here run only if this MCP dispatch actually gets through.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mcp-config) url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/');;
    --settings) printf '%s' "$2" > child-settings.json;;
  esac
  shift
done
post() { curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }
read -r line
echo '{"type":"system","subtype":"init"}'
post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' > /dev/null
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_ask","name":"mcp__carbon6__Task","input":{"description":"asked command","prompt":"Run exactly this command: printf asked > child-ask-effect","connection":"commands","model":"m"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
post '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"Task","arguments":{"description":"asked command","prompt":"Run exactly this command: printf asked > child-ask-effect","connection":"commands","model":"m"}}}' > ask.json
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_ask","content":"answered"}]},"parent_tool_use_id":null}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_deny","name":"mcp__carbon6__Task","input":{"description":"forbidden command","prompt":"Run exactly this command: printf forbidden > child-deny-effect","connection":"commands","model":"m"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}'
post '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"Task","arguments":{"description":"forbidden command","prompt":"Run exactly this command: printf forbidden > child-deny-effect","connection":"commands","model":"m"}}}' > deny.json
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_deny","content":"refused"}]},"parent_tool_use_id":null}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Child checks complete."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Child checks complete.","usage":{"input_tokens":1,"output_tokens":1}}'
while read -r line; do :; done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn events(http: &reqwest::Client, api: &str, id: &str) -> Vec<Event> {
    http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
}

#[tokio::test]
async fn a_claude_code_child_owns_policies_and_surfaces_approval_to_its_loop_parent() {
    if !c6_testkit::have("curl") {
        return;
    }
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
    let parent_llm = MockLlm::start().await;
    let command_llm = MockLlm::start().await;
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
    // Declared after all tempdirs: Drop joins its entire runtime and cleans
    // owned held process groups even when the pre-fix assertion panics.
    let worker = held_worker::HeldWorker::start(state.path()).await;
    let post = async |path: &str, body: Value| -> Value {
        http.post(format!("{api}{path}")).json(&body).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    };
    let main = post("/connections", json!({"name": "parent", "base_url": parent_llm.base_url()})).await;
    let cc = post("/connections", json!({"kind": "claude_code", "name": "cc", "base_url": ""})).await;
    post("/connections", json!({"name": "commands", "base_url": command_llm.base_url()})).await;
    // These rules cannot match the ordinary loop parent's model:m labels.
    // Only the child's cloned spec, filtered with model:opus, owns them.
    http.put(format!("{api}/settings/policies"))
        .json(&json!([
            {"name": "Ask before the child command", "applies_to": ["model:opus"], "tools": "Task", "pattern": ASK_COMMAND, "action": "ask"},
            {"name": "No forbidden child command", "applies_to": ["model:opus"], "tools": "mcp__carbon6__Task", "pattern": DENY_COMMAND,
             "action": "deny", "message": "not in this test"}
        ]))
        .send().await.unwrap().error_for_status().unwrap();
    http.put(format!("{api}/settings/hooks"))
        .json(&json!([
            {"name": "Child hook", "event": "PreToolUse", "matcher": "Bash", "applies_to": ["model:opus"], "command": "printf child-only-hook"},
            {"name": "Parent hook", "event": "PreToolUse", "matcher": "Bash", "applies_to": ["model:m"], "command": "printf parent-only-hook"}
        ]))
        .send().await.unwrap().error_for_status().unwrap();

    parent_llm.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description": "check child policies", "prompt": "Check the guarded child dispatches.", "connection": "cc", "model": "opus"})
            .to_string(),
    )]));
    parent_llm.push(Reply::Text("The child checks finished.".into()));
    // If the gate is missing, the fake CLI really dispatches these commands
    // and writes the markers. No positive-path sleeps or mocked policy calls.
    for command in [ASK_COMMAND, DENY_COMMAND] {
        command_llm.push(Reply::ToolCalls(vec![("bash".into(), json!({"command": command}).to_string())]));
        command_llm.push(Reply::Text("Command ran.".into()));
    }
    let parent = post(
        "/sessions",
        json!({
            "worker_id": worker.id, "cwd": project.path().display().to_string(), "connection_id": main["id"], "model": "m",
            "prompt": "Check delegated command policy ownership.", "title": "Child policy ownership"
        }),
    )
    .await;
    let parent_id = parent["id"].as_str().unwrap();
    let child: Session = eventually("the Claude Code child", async || {
        let children: Vec<Session> = http.get(format!("{api}/sessions/{parent_id}/children")).send().await.ok()?.json().await.ok()?;
        children.into_iter().find(|c| c.connection_id == cc["id"].as_str().unwrap())
    })
    .await;
    assert_eq!(child.parent_id.as_deref(), Some(parent_id));
    // The head currently derives observed from the started tool profile
    // (store::create_subagent_session), so it also marks a real Task child
    // on Claude Code observed. That flag cannot distinguish this runtime
    // from a mirrored CLI subagent; its policy gate is what we test here.

    // Fail as soon as an unguarded dispatch answers (or writes its marker),
    // rather than wasting the suite's default five-minute hang deadline.
    let call = c6_testkit::within("the Claude Code child's own policy approval", async {
        loop {
            if let Some(call) = events(&http, &api, &child.id).await.iter().find_map(|e| match &e.kind {
                EventKind::Approval { call_id, state, policy, input, .. }
                    if state == "requested" && policy == "Ask before the child command" && input.contains(ASK_COMMAND) =>
                {
                    Some(call_id.clone())
                }
                _ => None,
            }) {
                break call;
            }
            let answered = std::fs::metadata(project.path().join("ask.json")).is_ok_and(|m| m.len() > 0);
            assert!(
                !answered && !project.path().join("child-ask-effect").exists(),
                "Claude Code child dispatched the guarded command without requesting approval: marker={}, command requests={}, MCP answer={:?}",
                project.path().join("child-ask-effect").exists(),
                command_llm.requests().len(),
                std::fs::read_to_string(project.path().join("ask.json"))
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(call, "toolu_ask");
    eventually("the child to show awaiting approval", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", child.id)).send().await.ok()?.json().await.ok()?;
        d.session.awaiting_approval.then_some(())
    })
    .await;
    let is_approval = |i: &AttentionItem| {
        i.kind == "approval" && i.session_id.as_deref() == Some(child.id.as_str()) && i.call_id.as_deref() == Some(call.as_str())
    };
    eventually("the child's approval in the parent's waiting list", async || {
        let waiting: Vec<AttentionItem> = http.get(format!("{api}/sessions/{parent_id}/waiting")).send().await.ok()?.json().await.ok()?;
        waiting.iter().any(&is_approval).then_some(())
    })
    .await;
    eventually("the child's approval in the bell", async || {
        let bell: Attention = http.get(format!("{api}/attention")).send().await.ok()?.json().await.ok()?;
        bell.waiting.iter().any(&is_approval).then_some(())
    })
    .await;
    assert!(command_llm.requests().is_empty(), "held means no command subagent was dispatched");
    assert!(!std::fs::metadata(project.path().join("ask.json")).is_ok_and(|m| m.len() > 0), "no MCP answer while held");
    assert!(!project.path().join("child-ask-effect").exists());

    // The approval belongs to the child actor, not the parent's hook gate.
    http.post(format!("{api}/sessions/{}/approvals/{call}", child.id))
        .json(&json!({"allow": false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    eventually("the parent to receive the child's report", async || {
        events(&http, &api, parent_id).await.into_iter().find_map(|e| match e.kind {
            EventKind::ToolResult { tool, output, is_error, .. } if tool == "task" && output.contains("Child checks complete.") => {
                assert!(!is_error, "{output}");
                Some(())
            }
            _ => None,
        })
    })
    .await;
    eventually("the child's turn to finish", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", child.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.session.end_reason.is_some() && !d.session.awaiting_approval).then_some(())
    })
    .await;
    let read = |name: &str| -> Value { serde_json::from_str(&std::fs::read_to_string(project.path().join(name)).unwrap()).unwrap() };
    let ask = read("ask.json");
    assert_eq!(ask["result"]["isError"], true, "{ask}");
    assert!(ask["result"]["content"][0]["text"].as_str().unwrap().contains("Not run: you denied it"), "{ask}");
    let denied = read("deny.json");
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert!(
        denied["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Denied by policy \"No forbidden child command\": not in this test"),
        "{denied}"
    );
    assert!(command_llm.requests().is_empty(), "neither the denied approval nor the deny rule dispatched a command");
    for marker in ["child-ask-effect", "child-deny-effect"] {
        assert!(!project.path().join(marker).exists(), "denied command had an effect: {marker}");
    }
    let children: Vec<Session> = http.get(format!("{api}/sessions/{}/children", child.id)).send().await.unwrap().json().await.unwrap();
    assert!(children.is_empty(), "denials must prevent Task itself, not only a later command");
    let settings = read("child-settings.json").to_string();
    assert!(settings.contains("child-only-hook"), "child-scoped hooks must reach its CLI: {settings}");
    assert!(!settings.contains("parent-only-hook"), "the parent-only hook must not be copied: {settings}");
    let states: Vec<String> = events(&http, &api, &child.id)
        .await
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Approval { call_id, state, .. } if call_id == call => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(states, ["requested", "denied"]);
    let waiting: Vec<AttentionItem> = http.get(format!("{api}/sessions/{parent_id}/waiting")).send().await.unwrap().json().await.unwrap();
    assert!(!waiting.iter().any(&is_approval), "the answered child approval must clear from its parent");

    worker.stop();
    head.shutdown().await.unwrap();
}
