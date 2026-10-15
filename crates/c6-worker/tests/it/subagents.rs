//! Subagents as sessions (docs/subagents.md): the parent's log links to the
//! child, the child runs as its own actor, and an interrupt or a worker
//! restart leaves what it did in its log.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use c6_api::{Event, EventKind, Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    conn_id: String,
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
        let mock = MockLlm::start().await;
        let conn: Value = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "m", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Self { head, api, http, mock, conn_id: conn["id"].as_str().unwrap().to_owned(), _head_dir: head_dir }
    }

    async fn enroll(&self, state: &Path) {
        let e = c6_head::api::new_enrollment(&self.head.state, None).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
    }

    async fn create(&self, worker: &Worker, cwd: &Path, prompt: &str) -> String {
        let s: Value = self
            .http
            .post(format!("{}/sessions", self.api))
            .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": cwd.display().to_string(), "connection_id": self.conn_id, "model": "m", "prompt": prompt}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        s["id"].as_str().unwrap().to_owned()
    }

    async fn detail(&self, id: &str) -> Option<SessionDetail> {
        self.http.get(format!("{}/sessions/{id}", self.api)).send().await.ok()?.json().await.ok()
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn children(&self, id: &str) -> Vec<Session> {
        self.http.get(format!("{}/sessions/{id}/children", self.api)).send().await.unwrap().json().await.unwrap()
    }
}

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = Worker::open(state, Identity::load(state).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(15), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (w, stop)
}

async fn until<T>(what: &str, secs: u64, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn turn_end(events: &[Event]) -> Option<TurnEndReason> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::TurnEnded { reason, .. } => Some(*reason),
        _ => None,
    })
}

/// How the last turn ended, if it ended after `seq`: other events after
/// `seq` (a subagent's progress) don't make an earlier turn's end count.
fn turn_end_after(events: &[Event], seq: u64) -> Option<TurnEndReason> {
    events.iter().rev().take_while(|e| e.seq > seq).find_map(|e| match &e.kind {
        EventKind::TurnEnded { reason, .. } => Some(*reason),
        _ => None,
    })
}

/// The parent's log names the child, the child's first message is the
/// task (the subagent note goes to the model only), and stopping the parent
/// stops the child, whose finished tool calls stay in its log.
#[tokio::test]
async fn a_subagent_is_a_session_and_stops_with_its_parent() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"dig","prompt":"Look for the needle."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo found-half"}"#.into())])); // the subagent
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into())); // the subagent, stuck
    let id = env.create(&worker, project.path(), "go").await;

    // The parent's log links to the child while it runs.
    let child = until("subagent spawned", 15, async || {
        env.events(&id).await.into_iter().find_map(|e| match e.kind {
            EventKind::SubagentSpawned { call_id, child_session_id, description, .. } => Some((call_id, child_session_id, description)),
            _ => None,
        })
    })
    .await;
    assert_eq!(child.2, "dig");
    let child_id = child.1;
    until("the subagent's command ran", 15, async || {
        env.events(&child_id)
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("found-half")))
            .then_some(())
    })
    .await;
    let children = env.children(&id).await;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, child_id);
    assert_eq!(children[0].parent_call_id.as_deref(), Some(child.0.as_str()));

    let events = env.events(&child_id).await;
    let (text, prompt, delegated_by) = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::User { text, prompt, delegated_by, .. } => Some((text.clone(), prompt.clone(), delegated_by.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(text, "Look for the needle.");
    assert!(prompt.starts_with("Look for the needle.") && prompt.contains("subagent"), "{prompt}");
    assert_eq!(delegated_by, child.0);
    // Subagents are not rewound: no checkpoints of their own.
    assert!(!events.iter().any(|e| matches!(e.kind, EventKind::Checkpoint { .. })));

    // Stopping the parent stops the child; both turns end as interrupted.
    let r = env.http.post(format!("{}/sessions/{id}/interrupt", env.api)).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let child_events = until("the subagent stopped", 15, async || {
        let ev = env.events(&child_id).await;
        (turn_end(&ev) == Some(TurnEndReason::Interrupted)).then_some(ev)
    })
    .await;
    assert!(child_events.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("found-half"))));
    let parent = until("the parent stopped", 15, async || {
        let ev = env.events(&id).await;
        (turn_end(&ev) == Some(TurnEndReason::Interrupted)).then_some(ev)
    })
    .await;
    let task_result = parent
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, is_error, .. } if tool == "task" => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap();
    assert!(task_result.1, "{task_result:?}");
    // What the parent gets: the subagent's id and what it did so far.
    let out = &task_result.0;
    assert!(out.contains(&format!("<subagent id=\"{child_id}\" status=\"interrupted\"")), "{out}");
    assert!(out.contains("tool_calls=\"1\"") && out.contains("turns=\"1\""), "{out}");
    assert!(out.contains("send it a message"), "the partial prompt follows: {out}");
    let d = env.detail(&child_id).await.unwrap();
    assert_eq!(d.session.status, SessionStatus::Idle);
    assert_eq!(d.session.end_reason, Some(TurnEndReason::Interrupted));
    stop.cancel();
}

/// A worker that restarts while a subagent runs closes both turns; the
/// child's log, as far as it got, is on the head like the worker's.
#[tokio::test]
async fn a_worker_restart_keeps_the_subagents_log() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (first, stop_first) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"dig","prompt":"Dig."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo partial"}"#.into())]));
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into()));
    let id = env.create(&first, project.path(), "go").await;
    let child_id = until("subagent working", 15, async || {
        let c = env.children(&id).await.into_iter().next()?;
        let ev = env.events(&c.id).await;
        ev.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("partial"))).then_some(c.id)
    })
    .await;

    stop_first.cancel();
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (second, stop_second) = run_worker(state.path()).await;
    for sid in [&id, &child_id] {
        let ev = until("turn closed", 15, async || {
            let ev = env.events(sid).await;
            (turn_end(&ev) == Some(TurnEndReason::WorkerRestarted)).then_some(ev)
        })
        .await;
        let (_, journal) = second.journal.load_session(sid).await.unwrap().unwrap();
        assert_eq!(ev.len(), journal.len(), "head and worker agree on {sid}");
    }
    let ev = env.events(&child_id).await;
    assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("partial"))));
    // The parent's call got the digest, not a generic placeholder.
    let parent = env.events(&id).await;
    let out = parent
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, .. } if tool == "task" => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(out.contains(&format!("<subagent id=\"{child_id}\" status=\"lost\"")), "{out}");
    stop_second.cancel();
}

/// A subagent that edited files, then failed: its digest names the files it
/// changed (its own successful edits only) and its last error.
#[tokio::test]
async fn a_failed_subagent_reports_its_changes() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("notes.txt");

    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"write","prompt":"Write notes."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("write".into(), json!({"path": file.display().to_string(), "content": "hi"}).to_string())]));
    env.mock.push(Reply::Status(400)); // the subagent's model refuses (not retried)
    env.mock.push(Reply::Text("The subagent failed; noted.".into())); // the parent
    let id = env.create(&worker, project.path(), "go").await;
    let parent = until("parent done", 30, async || {
        let ev = env.events(&id).await;
        (turn_end(&ev) == Some(TurnEndReason::Completed)).then_some(ev)
    })
    .await;
    let out = parent
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, .. } if tool == "task" => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(out.contains("status=\"failed\""), "{out}");
    assert!(out.contains(&format!("Changed: {}", file.display())), "{out}");
    assert!(out.contains("Errors: "), "{out}");
    let child = &env.children(&id).await[0];
    assert_eq!(child.end_reason, Some(TurnEndReason::Error));
    stop.cancel();
}

/// A background subagent lost to a restart: the parent hears about it with
/// what it did, not that its output is gone.
#[tokio::test]
async fn a_lost_background_subagent_is_reported_with_its_work() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (first, stop_first) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    // The parent sleeps after starting it, so the subagent asks the model
    // first.
    env.mock.push(Reply::ToolCalls(vec![
        ("task".into(), r#"{"description":"dig","prompt":"Dig.","run_in_background":true}"#.into()),
        ("bash".into(), r#"{"command":"sleep 1"}"#.into()),
    ]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo partial"}"#.into())])); // the subagent
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into()));
    env.mock.push(Reply::Text("Started it.".into())); // the parent, while the subagent works
    let id = env.create(&first, project.path(), "go").await;
    let child_id = until("subagent working", 15, async || {
        let c = env.children(&id).await.into_iter().next()?;
        let ev = env.events(&c.id).await;
        ev.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("partial"))).then_some(c.id)
    })
    .await;
    until("the parent is idle", 15, async || env.detail(&id).await.filter(|d| d.session.status == SessionStatus::Idle)).await;

    stop_first.cancel();
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    env.mock.push(Reply::Text("I will continue it.".into())); // the parent, told
    let (_second, stop_second) = run_worker(state.path()).await;
    let notice = until("the parent is told", 15, async || {
        env.events(&id).await.into_iter().find_map(|e| match e.kind {
            EventKind::User { command, text, .. } if command == "task-notification" && text.contains("lost") => Some(text),
            _ => None,
        })
    })
    .await;
    assert!(notice.contains(&format!("<subagent id=\"{child_id}\" status=\"lost\"")), "{notice}");
    assert!(!notice.contains("output is gone"), "{notice}");
    stop_second.cancel();
}

fn user_messages(events: &[Event]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::User { text, command, delegated_by, .. } => Some((text.clone(), command.clone(), delegated_by.clone())),
            _ => None,
        })
        .collect()
}

/// The text of every message in a model request.
fn request_text(req: &Value) -> String {
    req["messages"].to_string()
}

/// Starts a session whose subagent runs a command and is then stopped with
/// its parent; returns (parent, subagent).
async fn interrupted_subagent(env: &Env, worker: &Worker, project: &Path) -> (String, String) {
    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"dig","prompt":"Look for the needle."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo found-half"}"#.into())]));
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into()));
    let id = env.create(worker, project, "go").await;
    let child_id = until("subagent working", 15, async || {
        let c = env.children(&id).await.into_iter().next()?;
        let ev = env.events(&c.id).await;
        ev.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("found-half"))).then_some(c.id)
    })
    .await;
    env.http.post(format!("{}/sessions/{id}/interrupt", env.api)).send().await.unwrap();
    for sid in [&id, &child_id] {
        until("stopped", 15, async || (turn_end(&env.events(sid).await) == Some(TurnEndReason::Interrupted)).then_some(())).await;
    }
    until("idle", 15, async || env.detail(&id).await.filter(|d| d.session.status == SessionStatus::Idle && d.queued.is_empty())).await;
    (id, child_id)
}

/// The parent continues a stopped subagent with SendMessage: same session,
/// its earlier work in the conversation, the report back as the result.
#[tokio::test]
async fn the_parent_continues_a_stopped_subagent() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let (id, child_id) = interrupted_subagent(&env, &worker, project.path()).await;

    env.mock.push(Reply::ToolCalls(vec![("send_message".into(), json!({"to": child_id, "message": "Carry on."}).to_string())]));
    env.mock.push(Reply::Text("The needle is in the haystack.".into())); // the subagent
    env.mock.push(Reply::Text("Found it.".into())); // the parent
    let before = env.mock.requests().len();
    env.http.post(format!("{}/sessions/{id}/messages", env.api)).json(&json!({"text": "continue it"})).send().await.unwrap();
    let parent = until("parent done", 15, async || {
        let ev = env.events(&id).await;
        (turn_end(&ev) == Some(TurnEndReason::Completed)).then_some(ev)
    })
    .await;
    let (call_id, out) = parent
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { call_id, tool, output, .. } if tool == "send_message" => Some((call_id.clone(), output.clone())),
            _ => None,
        })
        .unwrap();
    assert!(out.starts_with("The needle is in the haystack."), "{out}");
    assert_eq!(env.children(&id).await.len(), 1, "the same subagent");
    let msgs = user_messages(&env.events(&child_id).await);
    assert_eq!(msgs.last().unwrap(), &("Carry on.".to_owned(), String::new(), call_id));
    // The subagent saw what it had done before.
    let reqs = env.mock.requests();
    let child_req = reqs[before..].iter().find(|r| request_text(r).contains("Carry on.")).unwrap();
    assert!(request_text(child_req).contains("found-half"));
    stop.cancel();
}

/// You message a subagent directly: its report reaches the idle parent
/// with the parent's next input, or at once with wake.
#[tokio::test]
async fn your_message_to_a_subagent_reports_back() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let (id, child_id) = interrupted_subagent(&env, &worker, project.path()).await;
    let parent_seq = env.detail(&id).await.unwrap().session.last_seq;

    // Without wake: the parent stays idle.
    env.mock.push(Reply::Text("Half the haystack is searched.".into()));
    let r = env
        .http
        .post(format!("{}/sessions/{child_id}/messages", env.api))
        .json(&json!({"text": "How far did you get?"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    until("subagent answered", 15, async || (turn_end(&env.events(&child_id).await) == Some(TurnEndReason::Completed)).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.detail(&id).await.unwrap().session.last_seq, parent_seq, "nothing ran in the parent");

    // Its next input brings the report.
    env.mock.push(Reply::Text("Noted.".into()));
    let before = env.mock.requests().len();
    env.http.post(format!("{}/sessions/{id}/messages", env.api)).json(&json!({"text": "status?"})).send().await.unwrap();
    until("parent answered", 15, async || {
        let ev = env.events(&id).await;
        (turn_end_after(&ev, parent_seq) == Some(TurnEndReason::Completed)).then_some(())
    })
    .await;
    let req = request_text(&env.mock.requests()[before]);
    assert!(req.contains("How far did you get?") && req.contains("Half the haystack is searched."), "{req}");

    // With wake: the parent starts a turn by itself.
    env.mock.push(Reply::Text("All of it now.".into())); // the subagent
    env.mock.push(Reply::Text("Great.".into())); // the parent, woken
    let seq = env.detail(&id).await.unwrap().session.last_seq;
    env.http
        .post(format!("{}/sessions/{child_id}/messages", env.api))
        .json(&json!({"text": "Search the rest.", "wake": true}))
        .send()
        .await
        .unwrap();
    let ev = until("parent woken", 15, async || {
        let ev = env.events(&id).await;
        (turn_end_after(&ev, seq) == Some(TurnEndReason::Completed)).then_some(ev)
    })
    .await;
    let notice = user_messages(&ev).into_iter().rev().find(|(_, c, _)| c == "task-notification").unwrap();
    assert!(notice.0.contains("All of it now."), "{notice:?}");
    let msgs = user_messages(&env.events(&child_id).await);
    assert_eq!(msgs.last().unwrap().0, "Search the rest.");
    stop.cancel();
}

/// Resume sends the resume prompt to a stopped subagent; a finished one is
/// not resumed.
#[tokio::test]
async fn resume_continues_a_stopped_subagent() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let (id, child_id) = interrupted_subagent(&env, &worker, project.path()).await;

    let r = env.http.post(format!("{}/sessions/{id}/resume", env.api)).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), 400, "only subagents are resumed");
    env.mock.push(Reply::Text("Found it after all.".into()));
    let r = env.http.post(format!("{}/sessions/{child_id}/resume", env.api)).json(&json!({})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    until("resumed", 15, async || (turn_end(&env.events(&child_id).await) == Some(TurnEndReason::Completed)).then_some(())).await;
    let (text, command, _) = user_messages(&env.events(&child_id).await).pop().unwrap();
    assert_eq!(command, "resume");
    assert!(text.contains("stopped partway"), "{text}");
    let r = env.http.post(format!("{}/sessions/{child_id}/resume", env.api)).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), 400, "its last turn finished");

    // A side chat on the subagent sees its conversation.
    let anchor = env.events(&child_id).await.last().unwrap().seq;
    env.mock.push(Reply::Text("It searched with echo.".into()));
    let before = env.mock.requests().len();
    let side: Value = env
        .http
        .post(format!("{}/sessions/{child_id}/side-chats", env.api))
        .json(&json!({"anchor_seq": anchor, "prompt": "How did it search?", "connection_id": env.conn_id, "model": "m"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let side_id = side["id"].as_str().unwrap().to_owned();
    until("side chat answered", 15, async || (turn_end(&env.events(&side_id).await) == Some(TurnEndReason::Completed)).then_some(())).await;
    let req = request_text(&env.mock.requests()[before]);
    assert!(req.contains("found-half") && req.contains("How did it search?"), "{req}");
    stop.cancel();
}

/// The parent steers a subagent running in the background: the message
/// joins the subagent's turn, and its report comes as the task's notice.
#[tokio::test]
async fn the_parent_steers_a_running_subagent() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.mock.push(Reply::ToolCalls(vec![
        ("task".into(), r#"{"description":"dig","prompt":"Dig.","run_in_background":true}"#.into()),
        ("bash".into(), r#"{"command":"sleep 1"}"#.into()),
    ]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"sleep 3"}"#.into())])); // the subagent
    env.mock.push(Reply::ToolCalls(vec![("send_message".into(), r#"{"to":"bg1","message":"Also check the shed."}"#.into())]));
    env.mock.push(Reply::Text("Waiting for it.".into()));
    env.mock.push(Reply::Text("Dug, and the shed is empty.".into())); // the subagent, steered
    env.mock.push(Reply::Text("Good.".into())); // the parent, told
    let id = env.create(&worker, project.path(), "go").await;
    let ev = until("parent told", 20, async || {
        let ev = env.events(&id).await;
        user_messages(&ev).iter().any(|(t, c, _)| c == "task-notification" && t.contains("shed is empty")).then_some(ev)
    })
    .await;
    let sent = ev
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, .. } if tool == "send_message" => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(sent.starts_with("Sent:"), "{sent}");
    let child_id = env.children(&id).await[0].id.clone();
    let msgs = user_messages(&env.events(&child_id).await);
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert_eq!(msgs[1].0, "Also check the shed.");
    // One turn: the message joined it.
    let turns = env.events(&child_id).await.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count();
    assert_eq!(turns, 1);
    stop.cancel();
}

/// A worker that lost its journal gets a subagent back from the head when
/// you message it (without its parent there, with its own hooks), and the
/// subagent keeps its conversation and tools.
#[tokio::test]
async fn a_lost_subagent_is_restored_from_the_head() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let (_id, child_id) = interrupted_subagent(&env, &worker, project.path()).await;
    let tools = match &env.events(&child_id).await[0].kind {
        EventKind::Started { tools, .. } => tools.clone(),
        _ => panic!(),
    };
    assert!(tools.iter().any(|t| t == "task"), "it may start its own (depth 1 of 3): {tools:?}");
    stop.cancel();
    drop(worker);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Same identity, empty state: the journal is gone.
    let fresh = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(c6_worker::identity::identity_dir(fresh.path())).unwrap();
    for f in std::fs::read_dir(c6_worker::identity::identity_dir(state.path())).unwrap().flatten() {
        std::fs::copy(f.path(), c6_worker::identity::identity_dir(fresh.path()).join(f.file_name())).unwrap();
    }
    let (_reborn, stop2) = run_worker(fresh.path()).await;
    env.mock.push(Reply::Text("Picking up where I was.".into()));
    let before = env.mock.requests().len();
    env.http.post(format!("{}/sessions/{child_id}/messages", env.api)).json(&json!({"text": "Go on."})).send().await.unwrap();
    until("restored subagent answered", 20, async || {
        let ev = env.events(&child_id).await;
        (turn_end(&ev) == Some(TurnEndReason::Completed)).then_some(())
    })
    .await;
    let req = &env.mock.requests()[before];
    assert!(request_text(req).contains("found-half"), "history restored");
    let offered: Vec<String> =
        req["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_owned()).collect();
    let mut a = offered.clone();
    a.sort();
    let mut b = tools.clone();
    b.sort();
    assert_eq!(a, b, "the same tools");
    stop2.cancel();
}

/// Subagents start subagents: reports go up one level at a time, the tree
/// is listed and costed as one, and stopping the top stops every level.
#[tokio::test]
async fn subagents_nest() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();

    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"middle","prompt":"Delegate the digging."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"leaf","prompt":"Dig."}"#.into())])); // middle
    env.mock.push(Reply::Text("leaf: dug".into())); // leaf
    env.mock.push(Reply::Text("middle: the leaf dug".into())); // middle
    env.mock.push(Reply::Text("top: done".into())); // parent
    let id = env.create(&worker, project.path(), "go").await;
    let ev = until("parent done", 20, async || {
        let ev = env.events(&id).await;
        (turn_end(&ev) == Some(TurnEndReason::Completed)).then_some(ev)
    })
    .await;
    let out = ev
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, .. } if tool == "task" => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(out.starts_with("middle: the leaf dug"), "{out}");
    let middle = env.children(&id).await;
    assert_eq!(middle.len(), 1);
    let leaf = env.children(&middle[0].id).await;
    assert_eq!(leaf.len(), 1);
    assert_eq!(leaf[0].parent_id.as_deref(), Some(middle[0].id.as_str()));
    let tree: Vec<Session> = env.http.get(format!("{}/sessions/{id}/children?tree=1", env.api)).send().await.unwrap().json().await.unwrap();
    assert_eq!(tree.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec![middle[0].id.clone(), leaf[0].id.clone()]);
    match &env.events(&leaf[0].id).await[0].kind {
        EventKind::Started { tools, .. } => assert!(tools.iter().any(|t| t == "task"), "depth 2 of 3 may delegate: {tools:?}"),
        _ => panic!(),
    }
    // Five model calls in the tree, counted for the top session.
    let (_, calls) = c6_head::usage::cost_of(&env.head.state, std::slice::from_ref(&id)).await.unwrap();
    assert_eq!(calls, 5);
    // Search opens the leaf in the top session, through the threads; find
    // in the top session reaches it, after the call that started it.
    let found: c6_api::SearchResponse =
        env.http.get(format!("{}/search?q=leaf%3A%20dug", env.api)).send().await.unwrap().json().await.unwrap();
    let hit = found.results.iter().find(|r| r.session.id == leaf[0].id).expect("the leaf's match");
    let open_in = hit.open_in.as_ref().expect("opened in the top session");
    assert_eq!(open_in.session_id, id);
    assert_eq!(open_in.side_chats, vec![middle[0].id.clone(), leaf[0].id.clone()]);
    let m: c6_api::SessionMatches =
        env.http.get(format!("{}/sessions/{id}/matches?q=dug", env.api)).send().await.unwrap().json().await.unwrap();
    let opens: Vec<Vec<String>> = m.matches.iter().map(|x| x.open.clone()).collect();
    assert!(opens.contains(&vec![middle[0].id.clone(), leaf[0].id.clone()]), "{opens:?}");
    assert_eq!(opens.last().unwrap(), &Vec::<String>::new(), "the parent's own report comes last: {:?}", m.matches);

    // A stop runs down the tree.
    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"middle","prompt":"Again."}"#.into())]));
    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"leaf","prompt":"Again."}"#.into())]));
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into()));
    env.http.post(format!("{}/sessions/{id}/messages", env.api)).json(&json!({"text": "again"})).send().await.unwrap();
    let ids = until("three levels working", 15, async || {
        let tree: Vec<Session> = env.http.get(format!("{}/sessions/{id}/children?tree=1", env.api)).send().await.ok()?.json().await.ok()?;
        (tree.len() == 4
            && tree.iter().all(|s| s.status == SessionStatus::Running || s.end_reason.is_some())
            && tree[3].status == SessionStatus::Running)
            .then(|| tree.iter().skip(2).map(|s| s.id.clone()).collect::<Vec<_>>())
    })
    .await;
    env.http.post(format!("{}/sessions/{id}/interrupt", env.api)).send().await.unwrap();
    for sid in ids.iter().chain([&id]) {
        until("stopped", 15, async || (turn_end(&env.events(sid).await) == Some(TurnEndReason::Interrupted)).then_some(())).await;
    }
    stop.cancel();
}

/// The subagent-limits setting: depth 1 keeps subagents from starting
/// their own, and one at a time refuses a second.
#[tokio::test]
async fn subagent_limits_apply() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    env.enroll(state.path()).await;
    let (worker, stop) = run_worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    let r = env
        .http
        .put(format!("{}/settings/subagent-limits", env.api))
        .json(&json!({"max_depth": 0, "max_running": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = env
        .http
        .put(format!("{}/settings/subagent-limits", env.api))
        .json(&json!({"max_depth": 1, "max_running": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    env.mock.push(Reply::ToolCalls(vec![
        ("task".into(), r#"{"description":"one","prompt":"One."}"#.into()),
        ("task".into(), r#"{"description":"two","prompt":"Two."}"#.into()),
    ]));
    env.mock.push(Reply::Text("one done".into())); // the one that runs
    env.mock.push(Reply::Text("Only one ran.".into()));
    let id = env.create(&worker, project.path(), "go").await;
    let ev = until("parent done", 20, async || {
        let ev = env.events(&id).await;
        (turn_end(&ev) == Some(TurnEndReason::Completed)).then_some(ev)
    })
    .await;
    let results: Vec<(String, bool)> = ev
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, is_error, .. } if tool == "task" => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|(o, e)| *e && o.contains("already working")), "{results:?}");
    assert!(results.iter().any(|(o, e)| !*e && o.starts_with("one done")), "{results:?}");
    let child = &env.children(&id).await[0];
    match &env.events(&child.id).await[0].kind {
        EventKind::Started { tools, .. } => assert!(!tools.iter().any(|t| t == "task" || t == "send_message"), "{tools:?}"),
        _ => panic!(),
    }
    stop.cancel();
}
