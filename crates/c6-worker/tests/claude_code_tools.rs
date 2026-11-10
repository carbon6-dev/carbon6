//! Which of carbon6's tools a Claude Code session gets over carbon6's MCP
//! server is a setting (docs/background.md §4): automatic unless chosen by
//! hand (in any profile's naming), some loaded up front, with carbon6's
//! guidance for them in Claude Code's system prompt. A running session
//! takes a change up at its next quiet turn: its process starts again,
//! resuming the conversation, never while a background task runs. Claude
//! Code keeps only its coding tools unless the setting says all.
//! A binary of its own: it sets C6_CLAUDE_BIN.

#[path = "support/held_worker.rs"]
mod held_worker;

use std::path::Path;

use c6_api::{Event, EventKind};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::enroll;
use serde_json::{Value, json};

/// A `claude` that stays up between turns and records each start (its
/// arguments), its system prompt, and what carbon6's MCP server offers it.
/// "BGSTART" starts a background task, "BGEND" ends it.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
printf '%s\n' "$(printf '%s' "$*" | tr '\n' ' ')" >> spawns.txt
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mcp-config) url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/');;
    --append-system-prompt) printf '%s' "$2" > system.txt;;
    --append-system-prompt-file) cat "$2" > system.txt;;
  esac
  shift
done
post() { curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }
post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' > init.json
post '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' > tools.json
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
echo_of() { printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$1")"; }
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}\n' "$1"; }
done_ok() { printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":1,"output_tokens":1}}'; }
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  printf '%s\n' '{"type":"system","subtype":"init"}'
  echo_of "$line"
  case "$line" in
    *BGSTART*)
      printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"bg1","description":"Build","task_type":"local_bash"}'
      say "Started."; done_ok;;
    *BGEND*)
      printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"bg1","status":"completed","summary":"Built"}'
      say "Finished."; done_ok;;
    *) say "Reply."; done_ok;;
  esac
done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn claude_code_gets_carbon6s_tools_and_takes_up_changes() {
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

    // Automatic by default, with the defaults up front, and what may be chosen.
    let setting: Value = http.get(format!("{api}/settings/claude-code-tools")).send().await.unwrap().json().await.unwrap();
    assert_eq!(setting["automatic"], true);
    assert_eq!(setting["native_all"], false, "Claude Code keeps only its coding tools by default");
    assert_eq!(setting["settings"], json!({"includeCoAuthoredBy": false, "gitAttribution": false}), "no co-author lines by default");
    // Extra settings must be an object, without the keys carbon6 sets.
    for (bad, says) in [
        (json!({"automatic": true, "tools": [], "settings": [1]}), "JSON object"),
        (json!({"automatic": true, "tools": [], "settings": {"hooks": {}, "env": {}}}), "hooks, env"),
    ] {
        let r = http.put(format!("{api}/settings/claude-code-tools")).json(&bad).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(r.text().await.unwrap().contains(says));
    }
    assert_eq!(
        setting["core"],
        json!(["Task", "SendMessage", "doc", "Work", "Carbon6", "Remember", "Forget", "TaskPropose", "TaskWithdraw"])
    );
    let available: Vec<&str> = setting["available"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(available.contains(&"PublishFile") && !available.contains(&"Bash"), "{available:?}");
    // Not carbon6's to offer: Claude Code has its own (also not up front).
    for bad in [json!({"tools": ["doc", "Bash"]}), json!({"automatic": true, "tools": [], "core": ["Bash"]})] {
        let r = http.put(format!("{api}/settings/claude-code-tools")).json(&bad).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(r.text().await.unwrap().contains("Bash"));
    }
    // Chosen by hand, in either naming, two of them up front.
    let choose = async |body: Value| {
        let r = http.put(format!("{api}/settings/claude-code-tools")).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.status());
    };
    choose(json!({"tools": ["doc", "publish_file", "remember", "Forget"], "core": ["doc", "remember"]})).await;
    // A note for the memory guidance to carry.
    let r = http.post(format!("{api}/memories")).json(&json!({"text": "The build runs on thalassa."})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());

    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.id, "cwd": project.path().display().to_string(),
            "connection_id": cc["id"], "model": "opus", "prompt": "look around"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = s["id"].as_str().unwrap().to_owned();
    let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
    let turns_ended = async |n: usize| {
        eventually("the turn to end", async || {
            let evs: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
            (evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() >= n).then_some(())
        })
        .await
    };
    let send = async |text: &str| {
        let r = http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text": text})).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.status());
    };
    let file = |f: &str| std::fs::read_to_string(project.path().join(f)).unwrap();
    let spawns = || file("spawns.txt").lines().count();
    let offered = || -> Vec<(String, bool)> {
        let tools: Value = serde_json::from_str(&file("tools.json")).unwrap();
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| (t["name"].as_str().unwrap().to_owned(), t["_meta"]["anthropic/alwaysLoad"] == true))
            .collect()
    };
    let restarts = async || {
        events().await.iter().filter(|e| matches!(&e.kind, EventKind::Notice { title, .. } if title == "Claude Code started again")).count()
    };
    turns_ended(1).await;

    let up_front = |names: &[&str]| -> Vec<(String, bool)> {
        ["doc", "PublishFile", "Remember", "Forget"].iter().map(|n| (n.to_string(), names.contains(n))).collect()
    };
    assert_eq!(offered(), up_front(&["doc", "Remember"]), "only those chosen (no connection for subagents here), two up front");
    let system = file("system.txt");
    assert!(system.starts_with(c6_agent::CLAUDE_CODE_NOTE), "{system}");
    assert!(
        system.contains("# Memory") && system.contains("The build runs on thalassa."),
        "the guidance is in its system prompt: {system}"
    );
    assert_eq!(spawns(), 1, "{}", file("spawns.txt"));
    let first = file("spawns.txt").lines().next().unwrap().to_owned();
    for flag in ["--tools Bash,Read,Write,Edit,ToolSearch ", "--strict-mcp-config", "--setting-sources ", "--disable-slash-commands"] {
        assert!(first.contains(flag), "only its coding tools: {flag} in {first}");
    }
    assert!(first.contains("claude-guard Bash Read Write Edit ToolSearch"), "the guard hook: {first}");
    assert!(first.contains(r#""gitAttribution":false"#) && first.contains(r#""includeCoAuthoredBy":false"#), "the default extras: {first}");

    // Nothing changed: the same process.
    send("again").await;
    turns_ended(2).await;
    assert_eq!(spawns(), 1);

    // A background task runs: a change waits for it.
    send("BGSTART").await;
    turns_ended(3).await;
    choose(json!({"tools": ["doc", "publish_file", "remember", "Forget"], "core": ["doc"]})).await;
    send("hello").await;
    turns_ended(4).await;
    assert_eq!(spawns(), 1, "not while its task runs");
    send("BGEND").await;
    turns_ended(5).await;
    assert_eq!(spawns(), 1);

    // Quiet now: started again with the change, the conversation resumed.
    send("after").await;
    turns_ended(6).await;
    assert_eq!(spawns(), 2);
    assert!(file("spawns.txt").lines().nth(1).unwrap().contains("--resume"), "the conversation continues");
    assert_eq!(offered(), up_front(&["doc"]));
    assert_eq!(restarts().await, 1, "said in the transcript");

    // Automatic: every carbon6 tool the session has.
    choose(json!({"automatic": true, "tools": []})).await;
    send("auto").await;
    turns_ended(7).await;
    assert_eq!(spawns(), 3);
    let names: Vec<String> = offered().into_iter().map(|(n, _)| n).collect();
    assert!(names.len() > 4 && names.contains(&"TaskPropose".to_owned()) && names.contains(&"Work".to_owned()), "{names:?}");
    assert!(offered().iter().any(|(n, up)| n == "TaskPropose" && *up), "the defaults up front");
    assert_eq!(restarts().await, 2);

    // The escape hatch: all of Claude Code's own tools, as without carbon6.
    // The extra settings apply there too.
    choose(json!({"automatic": true, "tools": [], "native_all": true, "settings": {"effortLevel": "high"}})).await;
    send("all").await;
    turns_ended(8).await;
    assert_eq!(spawns(), 4);
    let last = file("spawns.txt").lines().last().unwrap().to_owned();
    for flag in ["--tools", "--strict-mcp-config", "--setting-sources", "claude-guard", "includeCoAuthoredBy"] {
        assert!(!last.contains(flag), "{flag} in {last}");
    }
    assert!(last.contains(r#"--settings {"effortLevel":"high"}"#), "{last}");
    worker.stop();
    head.shutdown().await.unwrap();
}
