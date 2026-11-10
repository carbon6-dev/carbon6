//! A subagent on a Claude Code connection runs as a Claude Code session of
//! its own (on the Claude Code plan, not an API connection), and its report
//! reaches its parent's Task call as any subagent's does. A named agent's
//! tool list keeps it to those tools there too.
//! A binary of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;

use c6_api::{Event, EventKind};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that answers every message "Found it on Claude Code." and
/// records each start's arguments, a line each.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
[ "$1" = --version ] && exit 0
echo "$@" >> {spawns}
uuid_of() {{ printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }}
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  printf '%s\n' '{{"type":"system","subtype":"init"}}'
  printf '{{"type":"user","isReplay":true,"uuid":"%s","message":{{"role":"user","content":"echo"}},"parent_tool_use_id":null}}\n' "$(uuid_of "$line")"
  printf '%s\n' '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Found it on Claude Code."}}],"stop_reason":"end_turn"}},"parent_tool_use_id":null}}'
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"usage":{{"input_tokens":1,"output_tokens":1}}}}'
done
"#,
        spawns = dir.join("spawns.txt").display()
    );
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn a_subagent_on_claude_code_runs_as_a_claude_code_session() {
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
    let mock = MockLlm::start().await;
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
    assert!(head.state.registry.get(worker.identity.meta.worker_id).unwrap().capabilities.iter().any(|c| c == "claude_code_subagents_v1"));
    let post = async |path: &str, body: Value| -> Value {
        http.post(format!("{api}{path}")).json(&body).send().await.unwrap().json().await.unwrap()
    };
    let m = post("/connections", json!({"name": "m", "base_url": mock.base_url()})).await;
    let cc = post("/connections", json!({"kind": "claude_code", "name": "Claude Code", "base_url": ""})).await;

    let r = http
        .put(format!("{api}/settings/agents"))
        .json(&json!([{"name": "reader", "description": "Reads", "tools": "Read", "prompt": "You are READER."}]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    // The parent, in carbon6's own loop, starts a subagent on Claude Code.
    mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description": "look", "prompt": "Find the bug.", "connection": "Claude Code", "model": "opus",
               "subagent_type": "reader"})
        .to_string(),
    )]));
    mock.push(Reply::Text("The subagent found it.".into()));
    let s = post(
        "/sessions",
        json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
               "connection_id": m["id"], "model": "m", "prompt": "find the bug"}),
    )
    .await;
    let id = s["id"].as_str().unwrap().to_owned();
    let events =
        async |sid: &str| -> Vec<Event> { http.get(format!("{api}/sessions/{sid}/events")).send().await.unwrap().json().await.unwrap() };
    let result = eventually("the subagent's report", async || {
        events(&id).await.into_iter().find_map(|e| match e.kind {
            EventKind::ToolResult { tool, output, is_error, .. } if tool == "task" => Some((output, is_error)),
            _ => None,
        })
    })
    .await;
    assert!(!result.1 && result.0.contains("Found it on Claude Code."), "{result:?}");

    // The child is a Claude Code session, run by `claude`.
    let children: Vec<Value> = http.get(format!("{api}/sessions/{id}/children")).send().await.unwrap().json().await.unwrap();
    let child = children[0]["id"].as_str().unwrap().to_owned();
    let started = events(&child).await.into_iter().find_map(|e| match e.kind {
        EventKind::Started { connection_id, system_prompt, .. } => Some((connection_id, system_prompt)),
        _ => None,
    });
    let (on, prompt) = started.unwrap();
    assert_eq!(on, cc["id"].as_str().unwrap(), "on the Claude Code connection");
    assert!(prompt.contains("Claude Code's own system prompt"), "a Claude Code session of its own: {prompt}");
    // Run by claude, with only the tools its agent may use: of Claude
    // Code's, Read (and its tool search), and none of carbon6's (no Task,
    // no documents).
    let spawns = std::fs::read_to_string(bins.path().join("spawns.txt")).unwrap();
    let args = spawns.lines().next().expect("run by claude");
    assert!(args.contains("--tools Read,ToolSearch "), "{args}");
    assert!(!args.contains("--mcp-config"), "none of carbon6's tools, so no bridge: {args}");
    stop.cancel();
    head.shutdown().await.unwrap();
}
