//! Switching a session between Claude Code and carbon6's own loop
//! (docs/architecture.md, "Model switching"): at a turn boundary the
//! session is handed over, the destination gets the conversation as a
//! marked transcript, and reloads keep the destination. A stand-in
//! `claude` records how it was run. A binary of its own: it sets
//! C6_CLAUDE_BIN.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that records its arguments and prompt in the session folder,
/// then edits a file and says which model it ran.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
prompt=$(cat)
printf '%s\n' "$*" >> claude-calls.log
printf '%s\n=====\n' "$prompt" >> claude-prompts.log
model=default; prev=""
for a in "$@"; do [ "$prev" = "--model" ] && model="$a"; prev="$a"; done
cat <<EOF
{"type":"assistant","message":{"content":[{"type":"text","text":"Checking."},{"type":"tool_use","id":"toolu_cc1","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use"},"parent_tool_use_id":null}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_cc1","content":"cc-tool-output"}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"content":[{"type":"text","text":"claude says hi on $model"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":5,"output_tokens":20},"modelUsage":{"claude-opus-5-5":{}}}
EOF
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let worker = Worker::open(state, Identity::load(state).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (worker, stop)
}

struct Env {
    api: String,
    http: reqwest::Client,
}

impl Env {
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.http.post(format!("{}{path}", self.api)).json(&body).send().await.unwrap()
    }

    async fn ok<T: serde::de::DeserializeOwned>(&self, path: &str, body: Value) -> T {
        let r = self.post(path, body).await;
        assert!(r.status().is_success(), "{path}: {}", r.text().await.unwrap());
        r.json().await.unwrap()
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.http.get(format!("{}{path}", self.api)).send().await.unwrap().json().await.unwrap()
    }

    /// Waits until the session is idle with `turns` finished turns.
    async fn idle(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("turn to finish", async || {
            let d: SessionDetail = self.get(&format!("/sessions/{id}")).await;
            if d.session.status != SessionStatus::Idle || !d.queued.is_empty() {
                return None;
            }
            let evs: Vec<Event> = self.get(&format!("/sessions/{id}/events")).await;
            (evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns).then_some(evs)
        })
        .await
    }
}

/// The handoffs in a log: (model, runtime).
fn handoffs(evs: &[Event]) -> Vec<(String, String)> {
    evs.iter()
        .filter_map(|e| match &e.kind {
            EventKind::ModelSwitched { model, handoff: Some(h), .. } => Some((model.clone(), h.tool_profile.clone())),
            _ => None,
        })
        .collect()
}

/// The `claude` runs so far: each one's arguments, and what it was told.
fn claude_runs(project: &Path) -> (Vec<String>, Vec<String>) {
    let read = |f: &str| std::fs::read_to_string(project.join(f)).unwrap_or_default();
    let calls = read("claude-calls.log").lines().map(str::to_owned).collect();
    let prompts = read("claude-prompts.log").split("\n=====\n").filter(|p| !p.is_empty()).map(str::to_owned).collect();
    (calls, prompts)
}

/// The argument after `flag` in a recorded run.
fn arg<'a>(call: &'a str, flag: &str) -> Option<&'a str> {
    let words: Vec<&str> = call.split(' ').collect();
    words.iter().position(|w| *w == flag).and_then(|i| words.get(i + 1).copied())
}

#[tokio::test]
async fn sessions_hand_over_between_claude_code_and_carbon6s_loop() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable
    // while it changes.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
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
    let env = Env { api: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()), http: reqwest::Client::new() };
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let (worker, stop) = run_worker(state.path()).await;
    let own: Connection = env.ok("/connections", json!({"name": "m", "base_url": mock.base_url()})).await;
    let cc: Connection = env.ok("/connections", json!({"kind": "claude_code", "name": "cc", "base_url": ""})).await;
    let say = |text: &str, switch: Option<(&str, &str)>| {
        let mut body = json!({"text": text});
        if let Some((c, m)) = switch {
            body["connection_id"] = json!(c);
            body["model"] = json!(m);
        }
        body
    };

    // 1. A session on carbon6's loop: a tool call, then an answer.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo own-tool-output"}"#.into())]));
    mock.push(Reply::Text("own answer one".into()));
    let s: Session = env
        .ok(
            "/sessions",
            json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
                   "connection_id": own.id, "model": "gpt-x", "prompt": "find the bug"}),
        )
        .await;
    let sid = s.id.clone();
    env.idle(&sid, 1).await;

    // 2. Ordinary → Claude Code: a new Claude Code conversation (not the
    // session's id), started with the marked transcript, then the message.
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("continue in claude", Some((&cc.id, "opus")))).await;
    let evs = env.idle(&sid, 2).await;
    assert_eq!(handoffs(&evs), [("opus".to_owned(), "claude_code".to_owned())]);
    let switched = evs.iter().position(|e| matches!(&e.kind, EventKind::ModelSwitched { handoff: Some(_), .. })).unwrap();
    let next = evs[switched + 1..].iter().find(|e| !matches!(e.kind, EventKind::Checkpoint { .. })).unwrap();
    assert!(
        matches!(&next.kind, EventKind::User { text, .. } if text == "continue in claude"),
        "the handoff precedes the message: {next:?}"
    );
    let (calls, prompts) = claude_runs(project.path());
    assert_eq!(calls.len(), 1, "{calls:?}");
    let conversation = arg(&calls[0], "--session-id").expect("a new conversation").to_owned();
    assert!(conversation != sid && arg(&calls[0], "--resume").is_none(), "{calls:?}");
    assert_eq!(arg(&calls[0], "--model"), Some("opus"));
    let p = &prompts[0];
    assert!(p.starts_with("<carbon6-handoff from=\"carbon6's own agent loop (gpt-x)\">"), "{p}");
    for seen in ["find the bug", "[tool call bash:", "own-tool-output", "own answer one", "</carbon6-handoff>"] {
        assert!(p.contains(seen), "{seen} missing from {p}");
    }
    assert!(p.ends_with("The user now says:\n\ncontinue in claude"), "{p}");
    assert!(mock.requests().len() == 2, "Claude Code ran the turn, not the model");
    let d: SessionDetail = env.get(&format!("/sessions/{sid}")).await;
    assert_eq!((d.session.id.as_str(), d.session.connection_id.as_str(), d.session.model.as_str()), (sid.as_str(), cc.id.as_str(), "opus"));

    // 3. The next turn resumes that conversation, with no transcript.
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("and the tests?", None)).await;
    env.idle(&sid, 3).await;
    let (calls, prompts) = claude_runs(project.path());
    assert_eq!(arg(&calls[1], "--resume"), Some(conversation.as_str()), "{calls:?}");
    assert_eq!(prompts[1], "and the tests?");

    // 4. The worker restarts (journal kept): still Claude Code, same
    // conversation.
    stop.cancel();
    drop(worker);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (worker, stop) = run_worker(state.path()).await;
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("still there?", None)).await;
    env.idle(&sid, 4).await;
    let (calls, _) = claude_runs(project.path());
    assert_eq!(arg(&calls[2], "--resume"), Some(conversation.as_str()), "reloaded on Claude Code: {calls:?}");
    assert_eq!(mock.requests().len(), 2);

    // 5. A switch within Claude Code keeps its conversation.
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("quicker now", Some((&cc.id, "sonnet")))).await;
    let evs = env.idle(&sid, 5).await;
    assert_eq!(handoffs(&evs).len(), 1, "no handoff within a runtime");
    assert!(evs.iter().any(|e| matches!(&e.kind, EventKind::ModelSwitched { model, handoff: None, .. } if model == "sonnet")));
    let (calls, _) = claude_runs(project.path());
    assert_eq!((arg(&calls[3], "--resume"), arg(&calls[3], "--model")), (Some(conversation.as_str()), Some("sonnet")), "{calls:?}");

    // 6. No rewinding to before the handoff: Claude Code's conversation
    // would not follow.
    let first_user = evs.iter().find(|e| matches!(e.kind, EventKind::User { .. })).unwrap().seq;
    let r = env.post(&format!("/sessions/{sid}/rewind"), json!({"to_seq": first_user, "restore_files": false})).await;
    assert_eq!(r.status(), 400, "{}", r.text().await.unwrap());

    // 7. Claude Code → ordinary: carbon6's loop runs again with its own
    // prompt and tools; Claude Code's turns are a transcript, not history.
    mock.push(Reply::Text("own answer two".into()));
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("back to gpt", Some((&own.id, "gpt-x")))).await;
    let evs = env.idle(&sid, 6).await;
    let h = handoffs(&evs);
    assert_eq!(h.last().unwrap(), &("gpt-x".to_owned(), "generic".to_owned()));
    let handoff = evs
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::ModelSwitched { handoff: Some(h), .. } => Some(h.clone()),
            _ => None,
        })
        .unwrap();
    assert!(handoff.tools.iter().any(|t| t == "bash") && !handoff.system_prompt.contains("Claude Code's own"), "{handoff:?}");
    let req = mock.requests().last().unwrap().clone();
    assert_eq!(req["model"], "gpt-x");
    let tools: Vec<&str> = req["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"bash"), "{tools:?}");
    let msgs = req["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs.len(), 2, "the system prompt, then one user message: {msgs:?}");
    let user = msgs[1].to_string();
    assert!(user.contains("<carbon6-handoff from=\\\"Claude Code (sonnet)\\\">"), "{user}");
    for seen in [
        "find the bug",
        "own answer one",
        "continue in claude",
        "[tool call Bash: {\\\"command\\\":\\\"ls\\\"}]",
        "cc-tool-output",
        "claude says hi on opus",
        "back to gpt",
    ] {
        assert!(user.contains(seen), "{seen} missing from {user}");
    }
    assert!(!req["messages"].to_string().contains("\"tool_calls\""), "no native tool calls from before the handoff");
    let (calls, _) = claude_runs(project.path());
    assert_eq!(calls.len(), 4, "Claude Code did not run");

    // 8. The worker loses its journal: restored from the head, the session
    // stays on carbon6's loop with the transcript.
    stop.cancel();
    drop(worker);
    tokio::time::sleep(Duration::from_millis(300)).await;
    for f in ["journal.db", "journal.db-wal", "journal.db-shm"] {
        let _ = std::fs::remove_file(state.path().join(f));
    }
    let (worker, stop) = run_worker(state.path()).await;
    mock.push(Reply::Text("own answer three".into()));
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("after the restore", None)).await;
    env.idle(&sid, 7).await;
    let req = mock.requests().last().unwrap().clone();
    let msgs = req["messages"].to_string();
    assert!(msgs.contains("carbon6-handoff") && msgs.contains("claude says hi on opus") && msgs.contains("own answer two"), "{msgs}");
    assert!(msgs.contains("after the restore"));
    assert_eq!(claude_runs(project.path()).0.len(), 4);

    // 9. Into Claude Code when it has gone from the worker: the turn fails
    // and says why; switching back works.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", bins.path().join("gone")) };
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("try claude", Some((&cc.id, "opus")))).await;
    let evs = env.idle(&sid, 8).await;
    assert!(
        evs.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("not installed"))),
        "the failure is in the log"
    );
    assert_eq!(handoffs(&evs).len(), 3);
    mock.push(Reply::Text("own answer four".into()));
    env.ok::<Value>(&format!("/sessions/{sid}/messages"), say("never mind", Some((&own.id, "gpt-x")))).await;
    let evs = env.idle(&sid, 9).await;
    assert_eq!(handoffs(&evs).len(), 4);
    let msgs = mock.requests().last().unwrap()["messages"].to_string();
    assert!(msgs.contains("try claude") && msgs.contains("never mind") && msgs.contains("own answer three"), "{msgs}");
    stop.cancel();
    drop(worker);
    head.shutdown().await.unwrap();
}
