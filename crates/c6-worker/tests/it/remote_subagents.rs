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

/// Continuing in the background joins the new task to the existing child,
/// including when another child has the same description. A later continuation
/// may address the child by that new task id, not just its original session id.
#[tokio::test]
async fn a_background_continuation_links_its_existing_local_child() {
    background_continuation_identity(false).await;
}

#[tokio::test]
async fn a_background_continuation_links_its_existing_remote_child() {
    background_continuation_identity(true).await;
}

async fn background_continuation_identity(remote: bool) {
    let env = Env::new().await;
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (alpha, stop_a) = env.worker("alpha", sa.path()).await;
    let (_beta, stop_b) = env.worker("beta", sb.path()).await;
    let (here, there) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    env.templates(json!([{"name": "far", "workers": ["beta"], "cwd": there.path().display().to_string(), "connection": "m", "model": "m", "agents": true}]))
        .await;
    let mut task = json!({"description": "same description", "prompt": "identity-child"});
    if remote {
        task["template"] = json!("far");
    }
    env.mock.push(Reply::ToolCalls(vec![("task".into(), task.to_string()), ("task".into(), task.to_string())]));
    env.mock.push(Reply::Text("First child done.".into()));
    env.mock.push(Reply::Text("Second child done.".into()));
    env.mock.push(Reply::Text("Both children done.".into()));
    let id = env.create(&alpha, here.path(), "identity-parent create both existing children").await;
    assert_eq!(env.turn_ended_after(&id, 0).await, TurnEndReason::Completed);
    // A short opening defers naming until a later turn. Let this opening's
    // unscripted naming request finish before adding scoped conversation
    // replies, which also match the opening quoted in a naming request.
    eventually("initial automatic naming finished", async || {
        env.head.state.store.setting(&format!("session_naming:{id}")).await.unwrap().is_none().then_some(())
    })
    .await;
    let originals: Vec<EventKind> =
        env.events(&id).await.into_iter().filter_map(|e| matches!(e.kind, EventKind::SubagentSpawned { .. }).then_some(e.kind)).collect();
    assert_eq!(originals.len(), 2);
    let child_id = match &originals[0] {
        EventKind::SubagentSpawned { child_session_id, description, remote: target, .. } => {
            assert_eq!(description, "same description");
            assert_eq!(target.is_some(), remote);
            child_session_id.clone()
        }
        _ => unreachable!(),
    };
    let sibling_id = match &originals[1] {
        EventKind::SubagentSpawned { child_session_id, description, .. } => {
            assert_eq!(description, "same description");
            assert_ne!(child_session_id, &child_id);
            child_session_id.clone()
        }
        _ => unreachable!(),
    };
    let sibling_events = env.events(&sibling_id).await;
    let mut to = child_id.clone();
    for run in 1..=2 {
        let seq = last_seq(&env.events(&id).await);
        let child_seq = last_seq(&env.events(&child_id).await);
        let message = format!("identity-continuation-{run}");
        env.mock.push_about(
            "identity-parent",
            Reply::ToolCalls(vec![("send_message".into(), json!({"to": to, "message": message, "run_in_background": true}).to_string())]),
        );
        env.mock.push_about("identity-parent", Reply::Text("Waiting for the continuation.".into()));
        let gate = env.mock.push_gated(Reply::Text(format!("Continuation {run} done.")));
        env.message(&id, &format!("continue identity {run}"), false).await;
        assert_eq!(env.turn_ended_after(&id, seq).await, TurnEndReason::Completed);
        let parent = eventually_or(
            "the continuation task's durable identity link",
            async || {
                let events = env.events(&id).await;
                let linked = events.iter().any(|e| e.seq > seq && matches!(e.kind, EventKind::SubagentSpawned { .. }));
                let started = events.iter().any(|e| {
                    e.seq > seq
                        && matches!(&e.kind,
                    EventKind::BackgroundTask { kind, state, .. } if kind == "agent" && state == "started")
                });
                (linked && started).then_some(events)
            },
            || env.mock.log(),
        )
        .await;
        let call_id = parent
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolResult { call_id, tool, is_error, output, .. } if e.seq > seq && tool == "send_message" => {
                    assert!(!is_error && output.starts_with("Continuing the subagent in the background as "), "{output}");
                    Some(call_id.clone())
                }
                _ => None,
            })
            .unwrap();
        let task_id = parent
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::BackgroundTask { task_id, kind, state, .. } if e.seq > seq && kind == "agent" && state == "started" => {
                    Some(task_id.clone())
                }
                _ => None,
            })
            .unwrap();
        assert_ne!(task_id, to, "each run has a new task identity");
        let mut expected = originals[0].clone();
        if let EventKind::SubagentSpawned { call_id: call, background_task_id, .. } = &mut expected {
            *call = call_id.clone();
            *background_task_id = task_id.clone();
        }
        let links: Vec<_> = parent.iter().filter(|e| matches!(e.kind, EventKind::SubagentSpawned { .. })).collect();
        assert_eq!(links.len(), 2 + run);
        assert_eq!(links[0].kind, originals[0], "the original link is unchanged");
        assert_eq!(links[1].kind, originals[1], "the same-named sibling is unchanged");
        assert_eq!(links.last().unwrap().kind, expected, "only call/task change; child, description, agent, model and remote stay intact");
        let journal = alpha.journal.events_after(&id, 0, u32::MAX).await.unwrap();
        assert!(journal.iter().any(|e| matches!(&e.kind,
            Some(c6_proto::session_event::Kind::SubagentSpawned(sp))
                if sp.child_session_id == child_id && sp.call_id == call_id && sp.background_task_id == task_id)));
        assert_eq!(c6_worker::spawned_children(&journal).len(), 2, "run links do not duplicate children");
        eventually("the same child takes the continuation", async || {
            env.events(&child_id).await.into_iter().find(|e| {
                e.seq > child_seq
                    && matches!(&e.kind,
                EventKind::User { text, delegated_by, .. } if text == &message && delegated_by == &call_id)
            })
        })
        .await;
        assert_eq!(env.events(&sibling_id).await, sibling_events, "names do not route the continuation");
        assert_eq!(env.children(&id).await.len(), 2, "no duplicate session is created");
        let parent_seq = last_seq(&parent);
        env.mock.push_about("identity-parent", Reply::Text("Received the continuation report.".into()));
        gate.notify_one();
        assert_eq!(env.turn_ended_after(&child_id, child_seq).await, TurnEndReason::Completed);
        assert_eq!(env.turn_ended_after(&id, parent_seq).await, TurnEndReason::Completed);
        to = task_id;
    }

    // Matching descriptions never authorize a send, even when both match.
    let seq = last_seq(&env.events(&id).await);
    env.mock.push_about(
        "identity-parent",
        Reply::ToolCalls(vec![(
            "send_message".into(),
            json!({"to": "same description", "message": "Do not route by name.", "run_in_background": true}).to_string(),
        )]),
    );
    env.mock.push_about("identity-parent", Reply::Text("A durable id is required.".into()));
    env.message(&id, "try an ambiguous name", false).await;
    assert_eq!(env.turn_ended_after(&id, seq).await, TurnEndReason::Completed);
    let events = env.events(&id).await;
    let (out, error) = result_of(&events, "send_message").unwrap();
    assert!(error && out.contains("no subagent"), "{out}");
    assert!(!events.iter().any(|e| e.seq > seq && matches!(e.kind, EventKind::SubagentSpawned { .. })));

    if remote {
        // The state check still succeeds for an archived child, but sending
        // fails. Its failed background task must not fabricate a run link.
        let child_events = env.events(&child_id).await;
        let r = env.http.patch(format!("{}/sessions/{child_id}", env.api)).json(&json!({"archived": true})).send().await.unwrap();
        assert!(r.status().is_success());
        let seq = last_seq(&events);
        env.mock.push_about(
            "identity-parent",
            Reply::ToolCalls(vec![(
                "send_message".into(),
                json!({"to": to, "message": "Do not enqueue this.", "run_in_background": true}).to_string(),
            )]),
        );
        env.mock.push_about("identity-parent", Reply::Text("Waiting for the refused continuation.".into()));
        env.mock.push_about("identity-parent", Reply::Text("The child must be unarchived first.".into()));
        env.message(&id, "try the archived child", false).await;
        let refused = eventually("the refused continuation task finished", async || {
            let events = env.events(&id).await;
            let failed = events.iter().any(|e| {
                e.seq > seq
                    && matches!(&e.kind,
                EventKind::BackgroundTask { state, exit_code, .. } if state == "finished" && *exit_code != 0)
            });
            (failed && turn_finished(&events)).then_some(events)
        })
        .await;
        assert!(!refused.iter().any(|e| e.seq > seq && matches!(e.kind, EventKind::SubagentSpawned { .. })));
        assert_eq!(env.events(&child_id).await, child_events, "the refused send never resumes the child");
    }
    stop_a.cancel();
    stop_b.cancel();
}

/// A failed background task no longer owns reports. A genuinely running
/// resumed child must still report the turn that takes its parent's steer.
#[tokio::test]
async fn a_failed_background_subagent_steered_after_failure_wakes_its_parent() {
    failed_background_resume(true).await;
}

#[tokio::test]
async fn a_failed_local_background_subagent_steered_into_its_next_turn_wakes_its_parent() {
    failed_background_resume(false).await;
}

async fn failed_background_resume(remote: bool) {
    let env = Env::new().await;
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (alpha, stop_a) = env.worker("alpha", sa.path()).await;
    let (_beta, stop_b) = env.worker("beta", sb.path()).await;
    let (here, there) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    env.templates(json!([{"name": "far", "workers": ["beta"], "cwd": there.path().display().to_string(), "connection": "m", "model": "m", "agents": true}]))
        .await;
    let mut task = json!({"description": "failure probe", "prompt": "remote-failure-probe", "run_in_background": true});
    if remote {
        task["template"] = json!("far");
    }
    env.mock.push(Reply::ToolCalls(vec![("task".into(), task.to_string())]));
    env.mock.push_about("start failure probe", Reply::Text("Waiting for the failure probe.".into()));
    let failure = env.mock.push_gated(Reply::Broken("Partial report before the provider failed.".into()));
    // The transport retries a truncated response. Make its retry a
    // permanent provider error, rather than letting it consume a parent reply.
    env.mock.push(Reply::Error(400, "permanent provider failure".into()));
    let id = env.create(&alpha, here.path(), "start failure probe").await;
    assert_eq!(env.turn_ended_after(&id, 0).await, TurnEndReason::Completed);
    let initial_end = last_seq(&env.events(&id).await);
    env.mock.push_about("start failure probe", Reply::Text("Acknowledged the failed background task.".into()));
    failure.notify_one();
    let child_id = eventually("the failed background task reported", async || {
        let events = env.events(&id).await;
        let failed = events.iter().any(|e| {
            matches!(&e.kind,
            EventKind::BackgroundTask { state, exit_code, .. } if state == "finished" && *exit_code != 0)
        });
        let child = env.children(&id).await.into_iter().next()?;
        let took_notice = events.iter().any(|e| {
            e.seq > initial_end
                && matches!(&e.kind,
            EventKind::User { command, .. } if command == "task-notification")
        });
        (failed && took_notice && turn_finished(&events)).then_some(child.id)
    })
    .await;
    assert_eq!(env.turn_ended_after(&child_id, 0).await, TurnEndReason::Error);
    // Resume independently of the ended background task, as the queued
    // shell notices did in the incident. The parent's steer arrives after
    // the final model call starts, and is consumed by the *next* turn.
    let prelude = env.mock.push_gated(Reply::Text("Prelude, not the requested final report.".into()));
    env.message(&child_id, "prelude-probe", false).await;
    eventually("the child's final response streaming", async || {
        let running = env.head.state.store.session(&child_id).await.unwrap()?.status == "running";
        (running && env.mock.requests().iter().any(|r| r["messages"].to_string().contains("prelude-probe"))).then_some(())
    })
    .await;
    let seq = last_seq(&env.events(&id).await);
    env.mock.push_about(
        "resume failure probe",
        Reply::ToolCalls(vec![(
            "send_message".into(),
            json!({"to": child_id, "message": "finish after failure", "run_in_background": true}).to_string(),
        )]),
    );
    env.mock.push_about("resume failure probe", Reply::Text("The steer was sent.".into()));
    let gate = env.mock.push_gated(Reply::Text("Recovered final report.".into()));
    env.message(&id, "resume failure probe", false).await;
    // The child has started, and SendMessage returned without a new bg id.
    let parent_events = eventually("the steering parent idle", async || {
        let events = env.events(&id).await;
        let sent = result_of(&events, "send_message").is_some();
        (sent && events.iter().any(|e| e.seq > seq && matches!(e.kind, EventKind::TurnEnded { .. })) && turn_finished(&events))
            .then_some(events)
    })
    .await;
    let (sent, error) = result_of(&parent_events, "send_message").unwrap();
    assert!(!error && sent.starts_with("Sent:"), "{sent}\n{}", env.mock.log());
    assert_eq!(
        parent_events.iter().filter(|e| matches!(e.kind, EventKind::SubagentSpawned { .. })).count(),
        1,
        "steering a running child does not create a continuation run link"
    );
    if remote {
        assert_eq!(
            env.head.state.store.waiting_delegations(&child_id).await.unwrap().len(),
            1,
            "a steer owns a durable report obligation even after its background task failed"
        );
    }
    let parent_end = last_seq(&parent_events);
    env.mock.push_about("resume failure probe", Reply::Text("I received the recovered report.".into()));
    prelude.notify_one();
    gate.notify_one();
    let notice = eventually_or(
        "the final report to wake the parent",
        async || {
            env.events(&id).await.into_iter().find_map(|e| match e.kind {
                EventKind::User { text, command, .. }
                    if e.seq > parent_end && command == "task-notification" && text.contains("Recovered final report.") =>
                {
                    Some((e.seq, text))
                }
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    assert_eq!(env.turn_ended_after(&id, notice.0).await, TurnEndReason::Completed);
    assert!(env.head.state.store.waiting_delegations(&child_id).await.unwrap().is_empty());
    stop_a.cancel();
    stop_b.cancel();
}

fn turn_finished(events: &[Event]) -> bool {
    events
        .iter()
        .rev()
        .find_map(|e| match e.kind {
            EventKind::TurnStarted { .. } => Some(false),
            EventKind::TurnEnded { .. } => Some(true),
            _ => None,
        })
        .unwrap_or(false)
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

    // An archived remote child refuses the message immediately. The
    // parent hears a tool error, rather than waiting for a report from a
    // child that never received input, and the head retires its delegation.
    let child_events = env.events(&child_id).await;
    let r = env.http.patch(format!("{}/sessions/{child_id}", env.api)).json(&json!({"archived": true})).send().await.unwrap();
    assert!(r.status().is_success());
    let seq = last_seq(&env.events(&id).await);
    env.mock.push(Reply::ToolCalls(vec![("send_message".into(), json!({"to": child_id, "message": "Do not enqueue this."}).to_string())]));
    env.mock.push(Reply::Text("That child is archived; I cannot message it.".into()));
    env.message(&id, "try the archived child", false).await;
    assert_eq!(env.turn_ended_after(&id, seq).await, TurnEndReason::Completed);
    let (out, is_error) = result_of(&env.events(&id).await, "send_message").unwrap();
    assert!(is_error && out.contains("archived") && out.contains("unarchive"), "{out}");
    assert!(env.head.state.store.waiting_delegations(&child_id).await.unwrap().is_empty());
    assert!(env.head.state.store.queued_inputs(&child_id).await.unwrap().is_empty());
    assert_eq!(env.events(&child_id).await, child_events, "the refused input never reaches the child");

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
    stopping_and_messaging_remote(false).await;
}

#[tokio::test]
async fn api_message_to_a_remote_subagent_names_its_scoped_token() {
    stopping_and_messaging_remote(true).await;
}

async fn stopping_and_messaging_remote(via_api: bool) {
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
    let secret = if via_api {
        let token = env.head.state.store.create_api_token("ops-controller", Some("home")).await.unwrap().token.unwrap();
        let r = env
            .http
            .post(format!("{}/sessions/{child_id}/messages", env.api))
            .bearer_auth(&token)
            .json(&json!({"text": "Just fix it.", "wake": true}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        Some(token)
    } else {
        // Explicit browser sign-in, not an origin-less test request.
        let token = env.head.state.store.create_login_link().await.unwrap();
        let signed_in = env.http.post(format!("{}/auth/link", env.api)).json(&json!({"token": token})).send().await.unwrap();
        let cookie = signed_in.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap();
        let r = env
            .http
            .post(format!("{}/sessions/{child_id}/messages", env.api))
            .header(reqwest::header::COOKIE, cookie)
            .header("x-c6-realm", "home")
            .json(&json!({"text": "Just fix it.", "wake": true}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        None
    };
    assert_eq!(env.turn_ended_after(&id, seq).await, TurnEndReason::Completed);
    let notice = env.events(&id).await.into_iter().find_map(|e| match e.kind {
        EventKind::User { text, command, .. } if command == "task-notification" => Some(text),
        _ => None,
    });
    let notice = notice.expect("a notice");
    let label = if via_api {
        format!("Sent to subagent {child_id} via the API (token \"ops-controller\"): \"Just fix it.\"")
    } else {
        format!("The user messaged subagent {child_id} directly: \"Just fix it.\"")
    };
    assert!(notice.contains(&label) && notice.contains("Fixed by hand."), "{notice}");
    let logged = env.head.state.store.events_after(&child_id, 0, i64::MAX).await.unwrap();
    let input = logged
        .iter()
        .find_map(|e| match &e.kind {
            Some(c6_proto::session_event::Kind::User(u)) if u.text == "Just fix it." => Some(u),
            _ => None,
        })
        .unwrap();
    assert_eq!(input.human_origin, !via_api);
    assert_eq!(input.api_origin.as_ref().map(|o| o.token_name.as_str()), via_api.then_some("ops-controller"));
    if let Some(secret) = secret {
        assert!(!notice.contains(&secret), "the token secret never enters the report");
    }
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
    let producer_events = env.head.state.store.events_after(&id, 0, i64::MAX).await.unwrap();
    let report = producer_events
        .iter()
        .find_map(|e| match &e.kind {
            Some(c6_proto::session_event::Kind::User(u)) if u.command == "task-notification" => Some(u),
            _ => None,
        })
        .unwrap();
    let metadata = report.task_notice.as_ref().expect("remote report metadata survived the parent's restart");
    assert_eq!(
        (metadata.kind.as_str(), metadata.title.as_str(), metadata.session_id.as_str(), metadata.status.as_str()),
        ("subagent", "long", child_id.as_str(), "completed")
    );
    assert_eq!(metadata.markdown, "Counted to ten.");
    let queued = producer_events
        .iter()
        .find_map(|e| match &e.kind {
            Some(c6_proto::session_event::Kind::NoticeQueued(n)) => n.task_notice.as_ref(),
            _ => None,
        })
        .unwrap();
    assert_eq!(queued, metadata);
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
