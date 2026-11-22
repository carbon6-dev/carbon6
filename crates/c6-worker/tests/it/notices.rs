//! Durable notice batching, spending, and recovery (docs/notices.md).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::{Head, HeadConfig};
use c6_testkit::{MockLlm, Reply, eventually, eventually_or};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let w = crate::open_worker(state).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (w, stop)
}

struct Env {
    head: Head,
    mock: MockLlm,
    http: reqwest::Client,
    api: String,
    connection: Value,
    _head_dir: tempfile::TempDir,
    state: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Env {
    async fn new(extra_limits: Value) -> Self {
        let mock = MockLlm::start().await;
        mock.real_usage();
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
        let connection = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "m", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut limits = json!({"max_depth": 3, "max_running": 16, "notice_quiet_seconds": 1});
        limits.as_object_mut().unwrap().extend(extra_limits.as_object().unwrap().clone());
        let r = http.put(format!("{api}/settings/subagent-limits")).json(&limits).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT, "{}", r.text().await.unwrap());
        Self { head, mock, http, api, connection, _head_dir: head_dir, state, project }
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn idle(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually_or(
            "session to finish its turns and go idle",
            async || {
                let d: SessionDetail = self.http.get(format!("{}/sessions/{id}", self.api)).send().await.ok()?.json().await.ok()?;
                let events = self.events(id).await;
                let ended = events
                    .iter()
                    .filter_map(|e| match &e.kind {
                        EventKind::TurnEnded { reason, .. } => Some(reason),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert!(ended.len() <= turns, "unexpected turn: {ended:?}");
                if d.session.status == SessionStatus::Idle && d.queued.is_empty() && ended.len() == turns {
                    assert!(ended.iter().all(|reason| **reason == TurnEndReason::Completed), "{ended:?}");
                    Some(events)
                } else {
                    None
                }
            },
            || self.mock.log(),
        )
        .await
    }

    async fn create(&self, worker: &Worker, prompt: &str) -> String {
        let s: Value = self
            .http
            .post(format!("{}/sessions", self.api))
            .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": self.project.path().display().to_string(),
            "connection_id": self.connection["id"], "model": "m", "prompt": prompt}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        s["id"].as_str().unwrap().to_owned()
    }
}

fn taken(events: &[Event], id: &str) -> usize {
    events.iter().filter(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id == id)).count()
}

fn waiting_reason<'a>(events: &'a [Event], id: &str) -> Option<&'a str> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::NoticeQueued { command_id, reason, .. } if command_id == id => Some(reason.as_str()),
        _ => None,
    })
}

fn tokens(events: &[Event]) -> u64 {
    events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, .. } => {
                Some(input_tokens + output_tokens + cache_read_tokens + cache_write_tokens)
            }
            _ => None,
        })
        .sum()
}

fn refers_to_spending(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("spend") || text.contains("budget") || text.contains("token")
}

fn shell_tasks(files: &[&str]) -> Reply {
    Reply::ToolCalls(
        files
            .iter()
            .map(|f| {
                (
                    "bash".into(),
                    json!({"command": format!("until [ -f {f} ]; do sleep 0.05; done; echo {f} done"),
            "run_in_background": true})
                    .to_string(),
                )
            })
            .collect(),
    )
}

#[tokio::test]
async fn a_spending_pause_survives_restart_and_delivers_held_notices_once() {
    const BUDGET: u64 = 30_000;
    const STEPS: usize = 32;
    const HELD: [&str; 2] = ["task:bg2:finished", "task:bg3:finished"];
    let env = Env::new(json!({"notice_tokens_per_hour": BUDGET})).await;
    let (worker, stop) = run_worker(env.state.path()).await;
    let finish = |f: &str| std::fs::write(env.project.path().join(f), "").unwrap();
    env.mock.push(shell_tasks(&["go1", "go2", "go3"]));
    env.mock.push(Reply::Text("Started them.".into()));
    let id = env.create(&worker, "Start three background tasks.").await;
    let direct = env.idle(&id, 1).await;
    // A long notice turn crosses the limit. Later, the short Continue turn
    // must leave room for another wake if the old usage was acknowledged.
    for i in 0..STEPS {
        env.mock.push(Reply::ToolCalls(vec![("bash".into(), json!({"command": format!("echo step-{i}")}).to_string())]));
    }
    env.mock.push(Reply::Text("Saw the first completion.".into()));
    finish("go1");
    let first_wake = env.idle(&id, 2).await;
    assert_eq!(taken(&first_wake, "task:bg1:finished"), 1);
    let unattended_spent = tokens(&first_wake[direct.len()..]);
    assert!(unattended_spent > BUDGET, "the unattended turn spent more than the budget");

    // Further completions are durable and visibly held. The pause sends a
    // user notification, not just an update to the waiting-notices pill.
    finish("go2");
    finish("go3");
    let held = eventually("both completions to be held and the spending pause announced", async || {
        let events = env.events(&id).await;
        (HELD.iter().all(|cid| waiting_reason(&events, cid).is_some_and(refers_to_spending))
            && events.iter().any(|e| {
                matches!(&e.kind, EventKind::Notice { title, message, .. }
                if refers_to_spending(title) || refers_to_spending(message))
            }))
        .then_some(events)
    })
    .await;
    for cid in HELD {
        assert_eq!(taken(&held, cid), 0);
    }
    let calls_before_continue = env.mock.requests().len();
    assert_eq!(calls_before_continue, STEPS + 3, "the pause does not start another turn");

    // Raise the limit to the actual prior spending, not a guessed prompt size.
    // Without Continue's acknowledgement, that amount alone exhausts the new
    // budget; with it, the short continuation and next wake have ample room.
    env.http
        .put(format!("{}/settings/subagent-limits", env.api))
        .json(&json!({"notice_quiet_seconds": 1, "notice_tokens_per_hour": unattended_spent}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    stop.cancel();
    drop(worker);
    let (_worker, stop) = run_worker(env.state.path()).await;
    env.mock.push(shell_tasks(&["go4"]));
    env.mock.push(Reply::Text("Saw both held completions and started another job.".into()));
    let r = env.http.post(format!("{}/sessions/{id}/notices/deliver", env.api)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT);
    let after = env.idle(&id, 3).await;
    for cid in HELD {
        assert_eq!(taken(&after, cid), 1, "Continue consumes the recovered batch once");
    }
    assert_eq!(env.mock.requests().len(), calls_before_continue + 2);
    let requests = env.mock.requests();
    let resumed = requests[calls_before_continue]["messages"].to_string();
    assert!(resumed.contains(r#"id=\"bg2\""#) && resumed.contains(r#"id=\"bg3\""#), "{resumed}");
    assert!(tokens(&after[held.len()..]) < unattended_spent, "the Continue turn alone must leave budget for another wake");

    env.mock.push(Reply::Text("The new completion woke me.".into()));
    finish("go4");
    let next_wake = env.idle(&id, 4).await;
    assert!(
        next_wake[after.len()..].iter().any(|e| matches!(&e.kind, EventKind::User { command_id, text, .. }
        if command_id.starts_with("task:bg") && text.contains("go4 done"))),
        "Continue acknowledges the old spending"
    );

    env.mock.push(Reply::Text("Nothing else.".into()));
    env.http
        .post(format!("{}/sessions/{id}/messages", env.api))
        .json(&json!({"text": "anything else?"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let later = env.idle(&id, 5).await;
    for cid in HELD {
        assert_eq!(taken(&later, cid), 1, "no repeat on the next direct message");
    }
    stop.cancel();
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn twenty_grouped_subagents_wake_the_parent_in_one_turn() {
    grouped_subagents_wake_the_parent_in_one_turn(20).await;
}

#[tokio::test]
async fn three_grouped_subagents_proceed_without_user_attention() {
    grouped_subagents_wake_the_parent_in_one_turn(3).await;
}

async fn grouped_subagents_wake_the_parent_in_one_turn(children: usize) {
    const GROUP: &str = "scout-burst";
    let env = Env::new(json!({"max_running": children})).await;
    let (worker, stop) = run_worker(env.state.path()).await;
    let calls = (0..children)
        .map(|i| {
            (
                "task".into(),
                json!({"description": format!("Scout {i:02}"), "prompt": format!("scout-work-{i:02}"),
            "run_in_background": true, "group": GROUP, "wake": "all"})
                .to_string(),
            )
        })
        .collect();
    env.mock.push(Reply::ToolCalls(calls));
    // Match the parent follow-up first: its history also contains the prompts.
    env.mock.push_about("Started the subagent in the background as bg", Reply::Text("Scouts launched.".into()));
    let gates = (0..children)
        .map(|i| env.mock.push_gated_about(&format!("scout-work-{i:02}"), Reply::Text(format!("Scout report {i:02}."))))
        .collect::<Vec<_>>();
    let id = env.create(&worker, "Launch the scout burst.").await;
    env.idle(&id, 1).await;

    let reports = |events: &[Event]| -> BTreeSet<String> {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NoticeQueued { command_id, .. } if command_id.starts_with("subagent:") || command_id.starts_with("task:bg") => {
                    Some(command_id.clone())
                }
                _ => None,
            })
            .collect()
    };
    env.mock.push(Reply::Text("Got every scout report.".into()));
    for gate in &gates[..children - 1] {
        gate.notify_one();
    }
    let progress = format!("group {GROUP}: {} of {children} finished; waiting for 1 (Scout {:02})", children - 1, children - 1);
    let held = eventually("all but the last scout's report to be queued", async || {
        let events = env.events(&id).await;
        let reports = reports(&events);
        if reports.len() != children - 1
            || !reports.iter().any(|cid| waiting_reason(&events, cid) == Some(progress.as_str()))
            || !reports.iter().all(|cid| {
                events.iter().rev().find_map(|event| match &event.kind {
                    EventKind::NoticeQueued { command_id, hold, .. } if command_id == cid => Some(*hold),
                    _ => None,
                }) == Some(c6_api::NoticeHoldReason::Group)
            })
        {
            return None;
        }
        let detail: SessionDetail = env.http.get(format!("{}/sessions/{id}", env.api)).send().await.unwrap().json().await.unwrap();
        // The last child is gated, so this flag must come from the held
        // notices, not just the session's display status for running children.
        assert!(detail.session.notices_proceeding);
        assert!(!detail.session.needs_you());
        assert_eq!(detail.session.status, SessionStatus::Idle);
        assert_eq!(detail.session.display_status(), SessionStatus::Running);
        Some(events)
    })
    .await;
    for cid in reports(&held) {
        assert_eq!(taken(&held, &cid), 0, "a group with a pending member must not wake");
    }

    gates.last().unwrap().notify_one();
    let complete = env.idle(&id, 2).await;
    let queued = reports(&complete);
    assert_eq!(queued.len(), children);
    for cid in queued {
        assert_eq!(taken(&complete, &cid), 1, "each scout's report was delivered once");
    }
    assert_eq!(env.mock.requests().len(), children + 3, "two parent turns and one request per child");
    let last = env.mock.requests().last().unwrap()["messages"].to_string();
    assert!(last.contains(&format!("group {GROUP}: {children} of {children} finished")), "missing final group summary: {last}");
    for i in 0..children {
        assert!(last.contains(&format!("Scout report {i:02}.")), "missing scout {i}: {last}");
    }
    stop.cancel();
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_notice_joining_a_user_turn_does_not_charge_its_tokens() {
    const BUDGET: u64 = 20;
    let env = Env::new(json!({"notice_tokens_per_hour": BUDGET})).await;
    let (worker, stop) = run_worker(env.state.path()).await;
    env.mock.push(Reply::ToolCalls(["first", "second"].iter().map(|f| {
        ("bash".into(), json!({"command": format!("until [ -f {f} ]; do sleep 0.05; done; printf '%s\\n' 'finished {f}'"), "run_in_background": true}).to_string())
    }).collect()));
    env.mock.push(Reply::Text("Started two jobs.".into()));
    let id = env.create(&worker, "Start two background jobs.").await;
    let started = env.idle(&id, 1).await;

    // Give the direct turn another model boundary, and hold its first response
    // until the first completion has been durably queued into that turn.
    let gate = env.mock.push_gated(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"true"}"#.into())]));
    env.mock.push(Reply::Text("The first job finished during this turn.".into()));
    env.http
        .post(format!("{}/sessions/{id}/messages", env.api))
        .json(&json!({"text": "Check in with me."}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    eventually("the direct turn's model request", async || {
        env.mock.requests().iter().any(|r| r["messages"].to_string().contains("Check in with me.")).then_some(())
    })
    .await;
    std::fs::write(env.project.path().join("first"), "").unwrap();
    eventually("the first background report to reach the worker's journal", async || {
        waiting_reason(&env.events(&id).await, "task:bg1:finished").is_some().then_some(())
    })
    .await;
    gate.notify_one();
    let mixed = env.idle(&id, 2).await;
    assert_eq!(taken(&mixed, "task:bg1:finished"), 1);
    assert!(tokens(&mixed[started.len()..]) > BUDGET);

    // A notice-only wake is still allowed: neither direct turn is charged.
    env.mock.push(Reply::Text("The second report woke me.".into()));
    std::fs::write(env.project.path().join("second"), "").unwrap();
    let wake = env.idle(&id, 3).await;
    assert_eq!(taken(&wake, "task:bg2:finished"), 1);
    stop.cancel();
    env.head.shutdown().await.unwrap();
}
