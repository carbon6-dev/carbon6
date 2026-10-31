//! Remote subagents (docs/remote-subagents.md): a session on one worker
//! starts a template's session on another as its subagent. The head holds
//! the link: the parent's call gets the subagent's report, SendMessage
//! continues it, stopping the parent stops it, your messages to it reach
//! the parent, and a parent whose worker restarted still hears its report.

use std::path::Path;
use std::sync::Arc;

use c6_api::{Event, EventKind, Session, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually, eventually_or};
use c6_worker::{ConnState, Worker, conn, enroll};
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

    /// A worker named `name`, attached.
    async fn worker(&self, name: &str, state: &Path) -> (Arc<Worker>, CancellationToken) {
        let e = c6_head::api::new_enrollment(&self.head.state, Some(name)).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
        run_worker(state).await
    }

    async fn templates(&self, v: Value) {
        let r = self.http.put(format!("{}/settings/templates", self.api)).json(&v).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
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
        s["id"].as_str().unwrap_or_else(|| panic!("{s}")).to_owned()
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn children(&self, id: &str) -> Vec<Session> {
        self.http.get(format!("{}/sessions/{id}/children", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn message(&self, id: &str, text: &str, wake: bool) {
        let r =
            self.http.post(format!("{}/sessions/{id}/messages", self.api)).json(&json!({"text": text, "wake": wake})).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }

    /// Waits for `id`'s turn after event `seq` to end, and says how.
    async fn turn_ended_after(&self, id: &str, seq: u64) -> TurnEndReason {
        eventually_or(
            "the turn to end",
            async || {
                self.events(id).await.iter().rev().take_while(|e| e.seq > seq).find_map(|e| match &e.kind {
                    EventKind::TurnEnded { reason, .. } => Some(*reason),
                    _ => None,
                })
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

/// The output of the call `tool` made, if it has one.
fn result_of(events: &[Event], tool: &str) -> Option<(String, bool)> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::ToolResult { tool: t, output, is_error, .. } if t == tool => Some((output.clone(), *is_error)),
        _ => None,
    })
}

fn last_seq(events: &[Event]) -> u64 {
    events.last().map_or(0, |e| e.seq)
}

/// A template agents may start runs as a session of its own on the worker
/// it names, in its folder, with its prompt before the task; the parent's
/// call gets its report, and SendMessage continues it through the head.
/// One agents may not start is refused, and not offered.
#[tokio::test]
async fn a_template_runs_a_subagent_on_another_worker() {
    let env = Env::new().await;
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (alpha, stop_a) = env.worker("alpha", sa.path()).await;
    let (beta, stop_b) = env.worker("beta", sb.path()).await;
    let (here, there) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let there_path = there.path().canonicalize().unwrap().display().to_string();
    env.templates(json!([
        {"name": "over-there", "description": "Work in the other project", "workers": ["beta"], "cwd": there_path,
         "connection": "m", "model": "m", "agents": true, "prompt": "Be brief.\n\n{{input}}"},
        {"name": "private", "workers": ["beta"], "cwd": there_path, "connection": "m", "model": "m", "prompt": "x"}
    ]))
    .await;

    // The parent asks for the template; the subagent runs on beta.
    env.mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description": "look there", "prompt": "Where are you?", "template": "over-there"}).to_string(),
    )]));
    env.mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"pwd"}"#.into())]));
    env.mock.push(Reply::Text("I am in the other project.".into()));
    env.mock.push(Reply::Text("Done.".into()));
    let id = env.create(&alpha, here.path(), "go").await;
    assert_eq!(env.turn_ended_after(&id, 0).await, TurnEndReason::Completed);

    // Task offered the template agents may start, not the other.
    let first = &env.mock.requests()[0];
    let task = first["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "task").unwrap();
    assert_eq!(task["function"]["parameters"]["properties"]["template"]["enum"], json!(["over-there"]));
    assert!(
        task["function"]["description"]
            .as_str()
            .unwrap()
            .contains(&format!("- over-there (runs on beta:{there_path}, m · m): Work in the other project"))
    );

    let events = env.events(&id).await;
    let (call_id, child_id, remote) = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { call_id, child_session_id, remote, .. } => {
                Some((call_id.clone(), child_session_id.clone(), remote.clone()))
            }
            _ => None,
        })
        .unwrap();
    let remote = remote.expect("a remote subagent");
    assert_eq!((remote.template.as_str(), remote.worker_name.as_str()), ("over-there", "beta"));
    let (out, is_error) = result_of(&events, "task").unwrap();
    assert!(!is_error && out.contains("I am in the other project.") && out.contains(&format!("[subagent session {child_id}]")), "{out}");

    // The child: on beta, in the template's folder, linked to the call.
    let child = env.children(&id).await.into_iter().find(|c| c.id == child_id).unwrap();
    assert_eq!(child.worker_id, beta.identity.meta.worker_id.0);
    assert_eq!(child.parent_call_id.as_deref(), Some(call_id.as_str()));
    assert!(child.labels.contains(&"template:over-there".to_owned()) && child.labels.contains(&format!("started-by:{id}")));
    let child_events = env.events(&child_id).await;
    let first_message = child_events.iter().find_map(|e| match &e.kind {
        EventKind::User { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(first_message.as_deref(), Some("Be brief.\n\nWhere are you?"));
    let (pwd, _) = result_of(&child_events, "bash").unwrap();
    assert!(pwd.contains(&there_path), "{pwd}");
    // The report reached the parent's log: nothing left to deliver.
    let alpha_id = alpha.identity.meta.worker_id;
    eventually("the report command done", async || {
        env.head.state.store.pending_commands(alpha_id, 0).await.unwrap().is_empty().then_some(())
    })
    .await;

    // SendMessage continues it, with its conversation, on beta.
    let seq = last_seq(&env.events(&id).await);
    env.mock.push(Reply::ToolCalls(vec![("send_message".into(), json!({"to": child_id, "message": "Once more."}).to_string())]));
    env.mock.push(Reply::Text("Still in the other project.".into()));
    env.mock.push(Reply::Text("Ok.".into()));
    env.message(&id, "ask it again", false).await;
    assert_eq!(env.turn_ended_after(&id, seq).await, TurnEndReason::Completed);
    let (out, _) = result_of(&env.events(&id).await, "send_message").unwrap();
    assert!(out.contains("Still in the other project."), "{out}");
    let delegated = env.events(&child_id).await.into_iter().find_map(|e| match e.kind {
        EventKind::User { text, delegated_by, .. } if text == "Once more." => Some(delegated_by),
        _ => None,
    });
    assert!(delegated.is_some_and(|d| !d.is_empty()), "the head says which call sent it");

    // A template agents may not start is refused.
    let seq = last_seq(&env.events(&id).await);
    env.mock
        .push(Reply::ToolCalls(vec![("task".into(), json!({"description": "sneak", "prompt": "x", "template": "private"}).to_string())]));
    env.mock.push(Reply::Text("Refused.".into()));
    env.message(&id, "try the private one", false).await;
    env.turn_ended_after(&id, seq).await;
    let (out, is_error) = result_of(&env.events(&id).await, "task").unwrap();
    assert!(is_error && out.contains("not one agents may start") && out.contains("over-there"), "{out}");
    stop_a.cancel();
    stop_b.cancel();
}

/// Stopping the parent's turn stops the remote subagent's, and the call
/// gets what it did. Your message to the subagent afterwards reaches the
/// parent as a notice (here waking it).
#[tokio::test]
async fn stopping_and_messaging_a_remote_subagent() {
    let env = Env::new().await;
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (alpha, stop_a) = env.worker("alpha", sa.path()).await;
    let (_beta, stop_b) = env.worker("beta", sb.path()).await;
    let (here, there) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    env.templates(json!([{"name": "far", "workers": ["beta"], "cwd": there.path().display().to_string(), "connection": "m", "model": "m", "agents": true}]))
        .await;

    env.mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description": "slow", "prompt": "Take your time.", "template": "far"}).to_string(),
    )]));
    let _gate = env.mock.push_gated(Reply::Text("never finishes".into()));
    let id = env.create(&alpha, here.path(), "go").await;
    let child_id = eventually("the subagent working", async || {
        let c = env.children(&id).await.into_iter().next()?;
        (c.status == c6_api::SessionStatus::Running).then_some(c.id)
    })
    .await;

    let r = env.http.post(format!("{}/sessions/{id}/interrupt", env.api)).send().await.unwrap();
    assert!(r.status().is_success());
    assert_eq!(env.turn_ended_after(&id, 0).await, TurnEndReason::Interrupted);
    assert_eq!(env.turn_ended_after(&child_id, 0).await, TurnEndReason::Interrupted);
    let (out, is_error) = result_of(&env.events(&id).await, "task").unwrap();
    assert!(is_error && out.contains(&format!("<subagent id=\"{child_id}\" status=\"interrupted\" turns=")), "stopped by this call: {out}");

    // You message the subagent and tick wake: the parent hears and acts.
    let seq = last_seq(&env.events(&id).await);
    env.mock.push(Reply::Text("Fixed by hand.".into()));
    env.mock.push(Reply::Text("Noted.".into()));
    env.message(&child_id, "Just fix it.", true).await;
    assert_eq!(env.turn_ended_after(&id, seq).await, TurnEndReason::Completed);
    let notice = env.events(&id).await.into_iter().find_map(|e| match e.kind {
        EventKind::User { text, command, .. } if command == "task-notification" => Some(text),
        _ => None,
    });
    let notice = notice.expect("a notice");
    assert!(
        notice.contains(&format!("The user messaged subagent {child_id} directly: \"Just fix it.\"")) && notice.contains("Fixed by hand."),
        "{notice}"
    );
    stop_a.cancel();
    stop_b.cancel();
}

/// The parent's worker restarts while its call waits: the call gets a
/// result saying the subagent runs on, and its report, which comes while
/// that worker is away, arrives once it is back, waking the parent.
#[tokio::test]
async fn a_parent_restart_still_gets_the_report() {
    let env = Env::new().await;
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (alpha, stop_a) = env.worker("alpha", sa.path()).await;
    let (_beta, stop_b) = env.worker("beta", sb.path()).await;
    let (here, there) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    env.templates(json!([{"name": "far", "workers": ["beta"], "cwd": there.path().display().to_string(), "connection": "m", "model": "m", "agents": true}]))
        .await;

    env.mock
        .push(Reply::ToolCalls(vec![("task".into(), json!({"description": "long", "prompt": "Count.", "template": "far"}).to_string())]));
    let gate = env.mock.push_gated(Reply::Text("Counted to ten.".into()));
    env.mock.push(Reply::Text("Got the count.".into()));
    let id = env.create(&alpha, here.path(), "go").await;
    let child_id = eventually("the subagent working", async || {
        let c = env.children(&id).await.into_iter().next()?;
        (c.status == c6_api::SessionStatus::Running).then_some(c.id)
    })
    .await;

    // alpha goes away; the subagent finishes meanwhile.
    let alpha_id = alpha.identity.meta.worker_id;
    stop_a.cancel();
    drop(alpha);
    eventually("alpha offline", async || {
        let w = env.head.state.registry.get(alpha_id);
        w.is_none().then_some(())
    })
    .await;
    gate.notify_one();
    assert_eq!(env.turn_ended_after(&child_id, 0).await, TurnEndReason::Completed);
    eventually("the report waiting for alpha", async || {
        (!env.head.state.store.pending_commands(alpha_id, 0).await.unwrap().is_empty()).then_some(())
    })
    .await;

    // Back: the waiting call is closed, and the report wakes the parent.
    let (_alpha, stop_a) = run_worker(sa.path()).await;
    let notice = eventually_or(
        "the report as a notice",
        async || {
            env.events(&id).await.into_iter().find_map(|e| match e.kind {
                EventKind::User { text, command, .. } if command == "task-notification" => Some(text),
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    assert!(notice.contains("Counted to ten."), "{notice}");
    let events = env.events(&id).await;
    let (out, _) = result_of(&events, "task").unwrap();
    assert!(out.contains(&format!("<subagent id=\"{child_id}\" status=\"running\" on=\"beta\"/>")), "{out}");
    let seq = events.iter().find(|e| matches!(&e.kind, EventKind::User { command, .. } if command == "task-notification")).unwrap().seq;
    assert_eq!(env.turn_ended_after(&id, seq).await, TurnEndReason::Completed);
    eventually("the report command done", async || {
        env.head.state.store.pending_commands(alpha_id, 0).await.unwrap().is_empty().then_some(())
    })
    .await;
    stop_a.cancel();
    stop_b.cancel();
}
