//! Ask as your everyday chat (docs/ask.md): a conversation takes the model
//! you pick and your mail, chat and calendar only when you say so; what you
//! @-mention comes with your message; work on a machine goes to a template's
//! subagent, and the call waits for its report; and on a Claude Code
//! connection the head runs `claude` itself, with only Ask's tools.

use std::path::Path;
use std::sync::Arc;

use c6_api::{Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually_or};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    _dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
        Self::with_claude(None).await
    }

    async fn with_claude(claude_bin: Option<std::path::PathBuf>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let head = c6_head::start(HeadConfig {
            claude_bin,
            data_dir: dir.path().to_owned(),
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
        let mock = MockLlm::start().await;
        let r = http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()})).send().await.unwrap();
        assert!(r.status().is_success());
        Self { head, api, http, mock, _dir: dir }
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        let ok = r.status().is_success();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(ok, "{path}: {v}");
        v
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.unwrap().json().await.unwrap()
    }

    /// Waits for `id`'s `n`th turn to end.
    async fn turns(&self, id: &str, n: usize) -> Vec<Event> {
        eventually_or(
            "the conversation's turn to end",
            async || {
                let events = self.events(id).await;
                (events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() >= n).then_some(events)
            },
            || self.mock.log(),
        )
        .await
    }
}

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = crate::open_worker(state).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (w, stop)
}

fn result_of(events: &[Event], tool: &str) -> Option<(String, bool)> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::ToolResult { tool: t, output, is_error, .. } if t == tool => Some((output.clone(), *is_error)),
        _ => None,
    })
}

/// New chat takes its model and the personal switch; the switch can be
/// turned off later; an @-mentioned project comes after your words, read
/// with your access, marked as yours.
#[tokio::test]
async fn a_chat_takes_its_model_your_switch_and_what_you_mention() {
    let env = Env::new().await;
    let p = env.post("/projects", json!({"title": "Realms for the team", "purpose": "Three people use realms daily", "plan": false})).await;
    let pid = p["id"].as_str().unwrap().to_owned();
    env.mock.push(Reply::Text("Hello.".into()));
    let s: Session = serde_json::from_value(
        env.post("/assistant", json!({"prompt": "hi", "connection": "m", "model": "mock-2", "personal": true, "realm": "home"})).await,
    )
    .unwrap();
    assert_eq!(s.model, "mock-2");
    assert!(s.labels.iter().any(|l| l == c6_api::ASSISTANT_PERSONAL), "{:?}", s.labels);
    let row = env.head.state.store.session(&s.id).await.unwrap().unwrap();
    let scope = c6_head::memex::scope_for(&env.head.state, &row.all_labels()).await;
    assert!(["email", "chat", "event"].iter().all(|k| scope.iter().any(|x| x == k)), "{scope:?}");
    env.turns(&s.id, 1).await;
    assert_eq!(env.mock.requests()[0]["model"], "mock-2");

    // Off again: back to the default scope.
    let labels: Vec<String> = s.labels.iter().filter(|l| *l != c6_api::ASSISTANT_PERSONAL).cloned().collect();
    let r = env.http.put(format!("{}/sessions/{}/labels", env.api, s.id)).json(&json!({"labels": labels})).send().await.unwrap();
    assert!(r.status().is_success());
    let row = env.head.state.store.session(&s.id).await.unwrap().unwrap();
    assert!(!c6_head::memex::scope_for(&env.head.state, &row.all_labels()).await.iter().any(|k| k == "email"));

    // @ offers the project; mentioning it brings it in.
    let found: Vec<c6_api::Mentionable> =
        env.http.get(format!("{}/assistant/mentionables?q=realms&realm=home", env.api)).send().await.unwrap().json().await.unwrap();
    assert!(found.iter().any(|m| m.reference == format!("project:{pid}") && m.kind == "project"), "{found:?}");
    env.mock.push(Reply::Text("It's about realms.".into()));
    let text = format!("What is [@Realms for the team](c6:project:{pid}) for?");
    let r = env.http.post(format!("{}/sessions/{}/messages", env.api, s.id)).json(&json!({"text": text})).send().await.unwrap();
    assert!(r.status().is_success());
    env.turns(&s.id, 2).await;
    let sent = env.mock.requests()[1]["messages"].to_string();
    assert!(
        sent.contains(&format!(
            "<mentioned ref=\\\"project:{pid}\\\" kind=\\\"project\\\" title=\\\"Realms for the team\\\" by=\\\"you\\\">"
        )),
        "{sent}"
    );
    assert!(sent.contains("Three people use realms daily"), "{sent}");
    // The transcript keeps your words as you wrote them.
    let events = env.events(&s.id).await;
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::User { text: t, .. } if t == &text)));
}

/// Work on a machine: c6_delegate offers the templates agents may start,
/// runs one as the conversation's subagent on its worker, and the call
/// waits for the subagent's report, which Ask then answers from.
#[tokio::test]
async fn ask_delegates_to_a_template_and_waits_for_the_report() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&env.head.state, Some("rust-dev")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let (_w, stop) = run_worker(state.path()).await;
    let repo = tempfile::tempdir().unwrap();
    let r = env
        .http
        .put(format!("{}/settings/templates", env.api))
        .json(&json!([{"name": "tests-on-rust-dev", "description": "Run things in the repo", "workers": ["rust-dev"],
                        "cwd": repo.path().canonicalize().unwrap().display().to_string(), "connection": "m", "model": "m", "agents": true}]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    env.mock.push(Reply::ToolCalls(vec![(
        "c6_delegate".into(),
        json!({"template": "tests-on-rust-dev", "prompt": "Run the tests and say what fails.", "description": "run the tests"}).to_string(),
    )]));
    env.mock.push(Reply::Text("2 tests fail: parse_dates and merge_ranges.".into()));
    env.mock.push(Reply::Text("Two tests fail: parse_dates and merge_ranges.".into()));
    let s: Session = serde_json::from_value(
        env.post("/assistant", json!({"prompt": "run the tests on rust-dev", "connection": "m", "model": "m", "realm": "home"})).await,
    )
    .unwrap();
    let events = env.turns(&s.id, 1).await;

    // It was offered with where it runs.
    let tools = env.mock.requests()[0]["tools"].to_string();
    assert!(tools.contains("c6_delegate") && tools.contains("tests-on-rust-dev: Run things in the repo (on rust-dev:"), "{tools}");
    // A child of the conversation, on the worker; the call got its report.
    let child = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a subagent: {:?}", result_of(&events, "c6_delegate")));
    let row = env.head.state.store.session(&child).await.unwrap().unwrap();
    assert_eq!(row.parent_id.as_deref(), Some(s.id.as_str()));
    assert_eq!(row.worker_name, "rust-dev");
    let (out, is_error) = result_of(&events, "c6_delegate").expect("the delegation's result");
    assert!(!is_error && out.contains("2 tests fail: parse_dates and merge_ranges.") && out.contains("status=\"completed\""), "{out}");
    assert!(
        events.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Two tests fail")))
    );
    stop.cancel();
}

/// A stand-in `claude` that records how it was started, lists the MCP
/// server's tools, and answers its first message by calling c6_call over
/// the bridge, as Claude Code would.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("claude");
    let script = format!(
        r#"#!/bin/sh
cd "{dir}"
[ "$1" = --version ] && exit 0
printf '%s\n' "$@" > args.txt
url=""
sys=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mcp-config) url=$(printf '%s' "$2" | sed 's/.*"url":"\([^"]*\)".*/\1/');;
    --append-system-prompt-file) sys="$2";;
  esac
  shift
done
[ -n "$sys" ] && cp "$sys" system.txt
post() {{ curl -s -X POST -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$1" "$url"; }}
post '{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"fake","version":"0"}}}}}}' > init.json
post '{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}' > tools.json
while read -r line; do
  uuid=$(printf '%s' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
  printf '%s\n' '{{"type":"system","subtype":"init"}}'
  printf '{{"type":"user","isReplay":true,"uuid":"%s","message":{{"role":"user","content":"echo"}},"parent_tool_use_id":null}}\n' "$uuid"
  printf '%s\n' '{{"type":"assistant","message":{{"id":"m1","content":[{{"type":"tool_use","id":"toolu_v","name":"mcp__carbon6__c6_call","input":{{"op":"system.version"}}}}],"stop_reason":"tool_use"}},"parent_tool_use_id":null}}'
  out=$(post '{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"c6_call","arguments":{{"op":"system.version"}}}}}}')
  printf '%s' "$out" > call.json
  text=$(printf '%s' "$out" | sed 's/.*"text":"\(.*\)","type".*/\1/; s/.*"text":"\([^"]*\)".*/\1/' | head -c 300 | tr -d '\\"')
  printf '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_v","content":"%s"}}]}},"parent_tool_use_id":null}}\n' "$text"
  printf '%s\n' '{{"type":"assistant","message":{{"id":"m2","content":[{{"type":"text","text":"Read the version."}}],"stop_reason":"end_turn"}},"parent_tool_use_id":null}}'
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"usage":{{}}}}'
done
"#,
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// On a Claude Code connection the head runs `claude` itself: none of
/// Claude Code's own tools, no MCP server but carbon6's, Ask's instructions
/// in its system prompt, and Ask's tools (all loaded up front) doing what
/// they do in Ask's own loop: here c6_call reads carbon6's version.
#[tokio::test]
async fn ask_runs_on_claude_code_with_only_its_own_tools() {
    let bin_dir = tempfile::tempdir().unwrap();
    let env = Env::with_claude(Some(fake_claude(bin_dir.path()))).await;
    let claude = env.http.get(format!("{}/assistant/claude", env.api)).send().await.unwrap().json::<c6_api::AskClaude>().await.unwrap();
    assert!(claude.available);
    let c: Value = env.post("/connections", json!({"name": "cc", "kind": "claude_code"})).await;
    let s: Session = serde_json::from_value(
        env.post("/assistant", json!({"prompt": "which version is this?", "connection": c["id"], "model": "haiku"})).await,
    )
    .unwrap();
    let events = env.turns(&s.id, 1).await;
    let args = std::fs::read_to_string(bin_dir.path().join("args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let at = |flag: &str| args.iter().position(|a| *a == flag);
    assert_eq!(at("--tools").map(|i| args[i + 1]), Some(""), "none of Claude Code's own tools: {args:?}");
    assert!(at("--strict-mcp-config").is_some() && at("--disable-slash-commands").is_some(), "{args:?}");
    assert!(std::fs::read_to_string(bin_dir.path().join("system.txt")).unwrap().contains("You are the carbon6 assistant"));
    let tools: Value = serde_json::from_str(&std::fs::read_to_string(bin_dir.path().join("tools.json")).unwrap()).unwrap();
    let listed = tools["result"]["tools"].as_array().unwrap();
    for name in ["c6_discover", "c6_describe", "c6_call", "c6_ask", "c6_delegate"] {
        let t = listed.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name} in {tools}"));
        assert_eq!(t["_meta"]["anthropic/alwaysLoad"], true, "{name} up front");
    }
    let call = std::fs::read_to_string(bin_dir.path().join("call.json")).unwrap();
    assert!(call.contains(env!("CARGO_PKG_VERSION")) && call.contains("\"isError\":false"), "{call}");
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Read the version.")))
    );
    assert!(env.mock.requests().is_empty(), "no API model was called");
}
