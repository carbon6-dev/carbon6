//! Where subagents run (docs/subagents.md §14): a Task call, the session's
//! default (picked in the session, or a routing rule's) or a named agent
//! may put a subagent on another connection and model than its parent's;
//! carbon6 runs it in its own loop either way.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c6_api::{Event, EventKind, Session, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    /// The session's connection ("main") and another ("other").
    main: MockLlm,
    other: MockLlm,
    main_id: String,
    other_id: String,
    _head_dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
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
        let (main, other) = (MockLlm::start().await, MockLlm::start().await);
        let add = async |name: &str, mock: &MockLlm| -> String {
            let c: Value = http
                .post(format!("{api}/connections"))
                .json(&json!({"name": name, "base_url": mock.base_url()}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            c["id"].as_str().unwrap().to_owned()
        };
        let main_id = add("main", &main).await;
        let other_id = add("other", &other).await;
        Self { head, api, http, main, other, main_id, other_id, _head_dir: head_dir }
    }

    async fn enroll(&self, state: &Path) {
        let e = c6_head::api::new_enrollment(&self.head.state, None).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
    }

    async fn create(&self, worker: &Worker, cwd: &Path, prompt: &str) -> String {
        let s: Value = self
            .http
            .post(format!("{}/sessions", self.api))
            .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": cwd.display().to_string(), "connection_id": self.main_id, "model": "m", "prompt": prompt}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        s["id"].as_str().unwrap().to_owned()
    }

    async fn session(&self, id: &str) -> Session {
        let d: Value = self.http.get(format!("{}/sessions/{id}", self.api)).send().await.unwrap().json().await.unwrap();
        serde_json::from_value(d["session"].clone()).unwrap()
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn children(&self, id: &str) -> Vec<Session> {
        self.http.get(format!("{}/sessions/{id}/children", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn set_subagent_model(&self, id: &str, body: Value) -> reqwest::Response {
        self.http.put(format!("{}/sessions/{id}/subagent-model", self.api)).json(&body).send().await.unwrap()
    }

    async fn say(&self, id: &str, text: &str) {
        let r = self.http.post(format!("{}/sessions/{id}/messages", self.api)).json(&json!({"text": text})).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }

    /// The session's events once its turns have ended `turns` times.
    async fn after_turns(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("the turn to end", async || {
            let ev = self.events(id).await;
            let ended = ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (ended >= turns).then_some(ev)
        })
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

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_owned()).collect()
}

fn task_results(events: &[Event]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, is_error, .. } if tool == "task" => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// Asked in the call: the subagent runs on the other connection and model,
/// with the tools of its model's family (not its parent's), and its row,
/// the parent's link to it and its cost say so.
#[tokio::test]
async fn a_task_call_runs_its_subagent_on_another_connection() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.main.push(Reply::ToolCalls(vec![(
        "task".into(),
        r#"{"description":"look","prompt":"Look around.","connection":"other","model":"claude-sonnet-5-5","effort":"high"}"#.into(),
    )]));
    env.other.push(Reply::Text("seen it all".into()));
    env.main.push(Reply::Text("done".into()));
    let id = env.create(&worker, project.path(), "go").await;
    let ev = env.after_turns(&id, 1).await;
    let results = task_results(&ev);
    assert!(results[0].0.starts_with("seen it all") && !results[0].1, "{results:?}");

    // The parent offered both connections, by name.
    let parent_req = &env.main.requests()[0];
    let task = parent_req["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "task").unwrap();
    assert_eq!(task["function"]["parameters"]["properties"]["connection"]["enum"], json!(["main", "other"]));
    assert!(task["function"]["description"].as_str().unwrap().contains("- other (openai_compat)"));
    // The subagent ran on the other connection, with Claude's tools.
    let reqs = env.other.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["model"], "claude-sonnet-5-5");
    let (parent_tools, child_tools) = (tool_names(parent_req), tool_names(&reqs[0]));
    assert!(parent_tools.contains(&"bash".to_owned()) && !parent_tools.contains(&"Bash".to_owned()), "{parent_tools:?}");
    assert!(child_tools.contains(&"Bash".to_owned()) && child_tools.contains(&"Task".to_owned()), "{child_tools:?}");
    assert_eq!(env.main.requests().len(), 2, "the parent's two calls only");

    let child = env.children(&id).await.remove(0);
    assert_eq!((child.connection_id.as_str(), child.model.as_str()), (env.other_id.as_str(), "claude-sonnet-5-5"));
    assert_eq!(child.effort.as_deref(), Some("high"));
    let spawned = ev
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { connection_id, model, effort, .. } => Some((connection_id.clone(), model.clone(), effort.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(spawned, (env.other_id.clone(), "claude-sonnet-5-5".into(), "high".into()));
    match &env.events(&child.id).await[0].kind {
        EventKind::Started { tools, .. } => assert!(tools.contains(&"Read".to_owned()), "{tools:?}"),
        k => panic!("{k:?}"),
    }
    // Continuing it keeps it there.
    env.main.push(Reply::ToolCalls(vec![("send_message".into(), json!({"to": child.id, "message": "And upstairs?"}).to_string())]));
    env.other.push(Reply::Text("upstairs too".into()));
    env.main.push(Reply::Text("ok".into()));
    env.say(&id, "more").await;
    env.after_turns(&id, 2).await;
    assert_eq!(env.other.requests().len(), 2);
    assert_eq!(env.other.requests()[1]["model"], "claude-sonnet-5-5");
    stop.cancel();
}

/// A connection the call names that is not there is refused, with the
/// ones that are; the parent carries on.
#[tokio::test]
async fn an_unknown_connection_is_refused() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.main.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"look","prompt":"Look.","connection":"nope"}"#.into())]));
    env.main.push(Reply::Text("fine".into()));
    let id = env.create(&worker, project.path(), "go").await;
    let ev = env.after_turns(&id, 1).await;
    let (out, is_error) = task_results(&ev).remove(0);
    assert!(is_error && out.contains("no connection \"nope\"") && out.contains("main, other"), "{out}");
    assert!(env.children(&id).await.is_empty());
    assert!(env.other.requests().is_empty());
    stop.cancel();
}

/// Picked in the session: the default applies to the next Task call, is in
/// the log and on the row, survives a worker restart, and goes back to the
/// session's own model when cleared.
#[tokio::test]
async fn the_session_default_for_subagents_can_be_changed() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.main.push(Reply::Text("ready".into()));
    let id = env.create(&worker, project.path(), "hello").await;
    env.after_turns(&id, 1).await;

    // Refused: a connection that is not there, a bad effort.
    assert_eq!(env.set_subagent_model(&id, json!({"connection_id": "nope", "model": "x"})).await.status(), 400);
    assert_eq!(env.set_subagent_model(&id, json!({"model": "x", "effort": "huge"})).await.status(), 400);
    let r = env.set_subagent_model(&id, json!({"connection_id": env.other_id, "model": "small", "effort": "low"})).await;
    assert!(r.status().is_success());
    let s: Session = r.json().await.unwrap();
    assert_eq!(
        (s.subagent_connection_id.as_str(), s.subagent_model.as_str(), s.subagent_effort.as_str()),
        (env.other_id.as_str(), "small", "low")
    );
    eventually("the worker to record it", async || {
        env.events(&id).await.iter().find_map(|e| match &e.kind {
            EventKind::SubagentModelSet { model, .. } if model == "small" => Some(()),
            _ => None,
        })
    })
    .await;

    env.main.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"a","prompt":"A."}"#.into())]));
    env.other.push(Reply::Text("from small".into()));
    env.main.push(Reply::Text("ok".into()));
    env.say(&id, "delegate").await;
    env.after_turns(&id, 2).await;
    assert_eq!(env.other.requests().last().unwrap()["model"], "small");
    let child = env.children(&id).await.remove(0);
    assert_eq!((child.connection_id.as_str(), child.effort.as_deref()), (env.other_id.as_str(), Some("low")));

    // After a restart the actor reloads it from the log.
    stop.cancel();
    drop(worker);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_worker, stop) = run_worker(state.path()).await;
    env.main.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"b","prompt":"B."}"#.into())]));
    env.other.push(Reply::Text("still small".into()));
    env.main.push(Reply::Text("ok".into()));
    env.say(&id, "again").await;
    env.after_turns(&id, 3).await;
    assert_eq!(env.other.requests().len(), 2, "on the other connection again");

    // Cleared: the session's own.
    assert!(env.set_subagent_model(&id, json!({"model": ""})).await.status().is_success());
    assert!(env.session(&id).await.subagent_model.is_empty());
    env.main.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"c","prompt":"C."}"#.into())]));
    env.main.push(Reply::Text("from m".into()));
    env.main.push(Reply::Text("ok".into()));
    env.say(&id, "once more").await;
    let ev = env.after_turns(&id, 4).await;
    assert_eq!(task_results(&ev).last().unwrap().0.lines().next(), Some("from m"));
    assert_eq!(env.other.requests().len(), 2);
    assert_eq!(turn_end(&ev), Some(TurnEndReason::Completed));
    stop.cancel();
}

/// A routing rule names the subagents' connection, model and effort; a
/// rule that names Claude Code for them, or a connection without a model,
/// is refused.
#[tokio::test]
async fn a_routing_rule_puts_subagents_on_another_connection() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let put = async |v: Value| env.http.put(format!("{}/settings/routes", env.api)).json(&v).send().await.unwrap().status();
    assert_eq!(put(json!([{"name": "x", "model": "m", "subagent_connection": "nope", "subagent_model": "s"}])).await, 400);
    assert_eq!(put(json!([{"name": "x", "model": "m", "subagent_connection": "other"}])).await, 400);
    assert_eq!(put(json!([{"name": "x", "model": "m", "subagent_effort": "huge"}])).await, 400);
    let cc: Value = env
        .http
        .post(format!("{}/connections", env.api))
        .json(&json!({"name": "cc", "kind": "claude_code", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cc["id"].is_string(), "{cc}");
    assert_eq!(put(json!([{"name": "x", "model": "m", "subagent_connection": "cc", "subagent_model": "opus"}])).await, 400);
    assert!(
        put(json!([{"name": "all", "connection": "main", "model": "m", "subagent_connection": "other", "subagent_model": "tiny", "subagent_effort": "medium"}]))
            .await
            .is_success()
    );

    env.main.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"a","prompt":"A."}"#.into())]));
    env.other.push(Reply::Text("tiny here".into()));
    env.main.push(Reply::Text("ok".into()));
    let s: Session = env
        .http
        .post(format!("{}/sessions", env.api))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(), "prompt": "go"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((s.subagent_connection_id.as_str(), s.subagent_model.as_str()), (env.other_id.as_str(), "tiny"));
    env.after_turns(&s.id, 1).await;
    assert_eq!(env.other.requests()[0]["model"], "tiny");
    let child = env.children(&s.id).await.remove(0);
    assert_eq!(child.effort.as_deref(), Some("medium"));
    // The subagents' list leaves Claude Code out.
    let task = env.main.requests()[0]["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "task").unwrap().clone();
    assert_eq!(task["function"]["parameters"]["properties"]["connection"]["enum"], json!(["main", "other"]));
    stop.cancel();
}

fn turn_end(events: &[Event]) -> Option<TurnEndReason> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::TurnEnded { reason, .. } => Some(*reason),
        _ => None,
    })
}
