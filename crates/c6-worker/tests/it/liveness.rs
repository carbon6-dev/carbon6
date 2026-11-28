//! Liveness fault injection. Gates establish the fault; explicit observation
//! times, not elapsed wall time, cross the liveness thresholds.

use super::{Env, run_worker};
use c6_api::{DEFAULT_LIVENESS_MODEL_SILENCE_SECONDS, DEFAULT_LIVENESS_TREE_SILENCE_SECONDS, DEFAULT_LIVENESS_WORKER_GRACE_SECONDS};
use c6_api::{DEFAULT_SESSION_RECOVERY_SECONDS, EventKind, SessionStatus, TurnEndReason};
use c6_proto::{
    Hello, Ping, SessionActivityKind, SessionActorState, SessionCursor, SessionLiveness, SessionLivenessReport, WorkerMsg, head_msg,
    worker_hub_client::WorkerHubClient, worker_msg,
};
use c6_testkit::{Reply, eventually, eventually_or, within};
use c6_worker::{ConnState, Worker, conn};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::{CancellationToken, DropGuard};
use tonic::Streaming;
use tonic::transport::Endpoint;

struct Node {
    worker: Arc<Worker>,
    stop: CancellationToken,
    _stop_on_drop: DropGuard,
    dir: tempfile::TempDir,
}

impl Node {
    async fn start(env: &Env, name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        env.enroll_named(dir.path(), Some(name)).await;
        let (worker, stop) = run_worker(dir.path()).await;
        Self { worker, _stop_on_drop: stop.clone().drop_guard(), stop, dir }
    }

    async fn disconnect(&self, env: &Env) {
        self.stop.cancel();
        eventually("the head to observe the worker detached", async || {
            (head(env).registry.get(self.worker.identity.meta.worker_id).is_none() && !self.worker.link.is_connected()).then_some(())
        })
        .await;
    }

    // Reuse the process's journal and still-running actors, not a restart.
    async fn reconnect(&mut self) {
        self.stop = CancellationToken::new();
        self._stop_on_drop = self.stop.clone().drop_guard();
        let (tx, mut rx) = watch::channel(ConnState::Connecting);
        tokio::spawn(conn::run(self.worker.clone(), tx, self.stop.clone()));
        within("the same worker to reattach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    }
}

/// Control what an attached worker says without adding an actor-killing hook
/// to production or issuing an interrupt that would end the turn normally.
struct SyntheticPeer {
    tx: mpsc::Sender<WorkerMsg>,
    rx: Streaming<c6_proto::HeadMsg>,
}

impl SyntheticPeer {
    async fn attach(node: &Node, id: &str, seq: u64) -> Self {
        let identity = &node.worker.identity;
        let channel = Endpoint::from_shared(identity.meta.head_url.clone())
            .unwrap()
            .tls_config(identity.tls_config())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let (tx, rx) = mpsc::channel(16);
        tx.send(WorkerMsg {
            msg: Some(worker_msg::Msg::Hello(Hello {
                protocol_version: c6_core::PROTOCOL_VERSION,
                sessions: vec![SessionCursor { session_id: id.to_owned(), seq, ownership_epoch: 0 }],
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let mut rx = WorkerHubClient::new(channel).attach(tokio_stream::wrappers::ReceiverStream::new(rx)).await.unwrap().into_inner();
        assert!(matches!(within("synthetic peer Welcome", rx.message()).await.unwrap().unwrap().msg, Some(head_msg::Msg::Welcome(_))));
        Self { tx, rx }
    }

    async fn ping(&mut self, nonce: u64, liveness: Option<SessionLivenessReport>) {
        self.tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Ping(Ping { nonce, last_rtt_us: 0, liveness })) }).await.unwrap();
        within("the head to acknowledge the injected ping", async {
            loop {
                let msg = self.rx.message().await.unwrap().expect("synthetic peer disconnected");
                if let Some(head_msg::Msg::Pong(pong)) = msg.msg {
                    assert_eq!(pong.nonce, nonce);
                    break;
                }
            }
        })
        .await;
    }

    async fn actor(&mut self, nonce: u64, id: &str, epoch: u64, state: SessionActorState, reason: &str) {
        self.ping(
            nonce,
            Some(SessionLivenessReport {
                sessions: vec![SessionLiveness {
                    session_id: id.into(),
                    ownership_epoch: epoch,
                    actor_state: state as i32,
                    reason: reason.into(),
                    ..Default::default()
                }],
            }),
        )
        .await;
    }
}

fn head(env: &Env) -> &c6_head::HeadState {
    &env.head.as_ref().unwrap().state
}

fn past(now: i64, seconds: u32) -> i64 {
    now + i64::from(seconds + 1) * 1000
}

async fn sweep(env: &Env, at: i64) {
    c6_head::liveness::sweep_at(head(env), at).await.unwrap();
}

async fn incidents(env: &Env, id: &str) -> Vec<(String, String, i64)> {
    sqlx::query_as("SELECT kind, cause, last_seq FROM liveness_incidents WHERE session_id = ? ORDER BY observed_at, id")
        .bind(id)
        .fetch_all(head(env).store.pool())
        .await
        .unwrap()
}

async fn assert_no_incident(env: &Env, id: &str) {
    let rows = incidents(env, id).await;
    assert!(rows.is_empty(), "unexpected liveness incident for {id}: {rows:?}");
}

async fn evidence(env: &Env, id: &str, matches: impl Fn(&Value) -> bool) -> Value {
    eventually_or(
        "the expected actor state in the head's heartbeat evidence",
        async || {
            let snapshot = c6_head::liveness::snapshot(head(env)).await.unwrap();
            snapshot["sessions"].as_array().unwrap().iter().find(|r| r["session_id"] == id && matches(r)).cloned()
        },
        || format!("session {id}\n{}", env.mock.log()),
    )
    .await
}

async fn model_wait(env: &Env, node: &Node, cwd: &Path, prompt: &str) -> (String, Arc<Notify>) {
    let gate = env.mock.push_gated_about(&user_prompt_scope(prompt), Reply::Text("The gated model turn completed.".into()));
    let id = env.create(&node.worker, cwd, prompt).await;
    model_requested(env, &id, prompt).await;
    (id, gate)
}

// Match the end of the serialized Chat user message, not the same prompt
// quoted and escaped inside the parent's Task arguments. The content may
// have a provenance prefix, so do not require an exact opening quote.
fn user_prompt_scope(prompt: &str) -> String {
    let message = json!({"content": prompt, "role": "user"}).to_string();
    message.strip_prefix("{\"content\":\"").expect("serialized Chat user message").to_owned()
}

async fn model_requested(env: &Env, id: &str, prompt: &str) {
    eventually_or(
        "the scoped model request to reach its gate",
        async || {
            env.mock
                .requests()
                .iter()
                .any(|r| {
                    r["messages"].as_array().is_some_and(|messages| {
                        messages.iter().any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|text| text.ends_with(prompt)))
                    })
                })
                .then_some(())
        },
        || env.mock.log(),
    )
    .await;
    eventually("the head to persist the open turn", async || {
        env.events(id).await.iter().any(|e| matches!(e.kind, EventKind::TurnStarted { .. })).then_some(())
    })
    .await;
}

async fn end_after(env: &Env, id: &str, floor: u64) -> TurnEndReason {
    eventually_or(
        "a new turn end",
        async || {
            env.events(id).await.into_iter().filter(|e| e.seq > floor).find_map(|e| match e.kind {
                EventKind::TurnEnded { reason, .. } => Some(reason),
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await
}

const REMOTE_PARENT_PROMPT: &str = "PARENT-LIVENESS: Delegate the scoped work to the other worker.";

async fn remote_task(
    env: &Env,
    parent_worker: &Node,
    child_name: &str,
    child_dir: &Path,
    child_prompt: &str,
    background: bool,
) -> (String, String) {
    let response = env
        .http
        .put(format!("{}/settings/templates", env.api))
        .json(&json!([{
            "name": "liveness-child", "description": "Work on the other worker", "workers": [child_name],
            "cwd": child_dir.canonicalize().unwrap(), "connection": "m", "model": "m", "agents": true, "prompt": "{{input}}"
        }]))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.text().await.unwrap());
    let parent_prompt = user_prompt_scope(REMOTE_PARENT_PROMPT);
    env.mock.push_about(
        &parent_prompt,
        Reply::ToolCalls(vec![(
            "task".into(),
            json!({
                "description": "Run the scoped job", "prompt": child_prompt, "template": "liveness-child", "run_in_background": background
            })
            .to_string(),
        )]),
    );
    if background {
        env.mock.push_about(&parent_prompt, Reply::Text("I will wait for the report.".into()));
    }
    let parent_dir = parent_worker.dir.path().join("project");
    std::fs::create_dir(&parent_dir).unwrap();
    let parent = env.create(&parent_worker.worker, &parent_dir, REMOTE_PARENT_PROMPT).await;
    let child = eventually_or(
        "the remote child link",
        async || {
            env.events(&parent).await.into_iter().find_map(|e| match e.kind {
                EventKind::SubagentSpawned { child_session_id, remote: Some(_), .. } => Some(child_session_id),
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    if background {
        assert_eq!(end_after(env, &parent, 0).await, TurnEndReason::Completed);
        eventually("the opening's naming attempt to finish", async || {
            head(env).store.setting(&format!("session_naming:{parent}")).await.unwrap().is_none().then_some(())
        })
        .await;
    }
    (parent, child)
}

fn script_ack(env: &Env, child: &str, cause: &str) {
    // The child's id alone already appears in the Task result. Require the
    // parent's actual opening and the distinctive text of the failure notice.
    env.mock.push_about_all(
        &[&user_prompt_scope(REMOTE_PARENT_PROMPT), child, cause, "Do not wait for a report that cannot arrive."],
        Reply::Text("I received the lost-child report.".into()),
    );
}

async fn assert_parent_notified(env: &Env, parent: &str, child: &str, cause: &str) {
    let (seq, text, notice) = eventually_or(
        "the parent's durable failure notice",
        async || {
            env.events(parent).await.into_iter().find_map(|e| match e.kind {
                EventKind::User { text, command, task_notice, .. } if command == "task-notification" => Some((e.seq, text, task_notice)),
                _ => None,
            })
        },
        || format!("parent {parent}, child {child}\n{}", env.mock.log()),
    )
    .await;
    assert!(text.contains(child) && text.contains(cause) && text.contains("SendMessage"), "{text}");
    let notice = notice.expect("the loss is also a structured task notice");
    assert_eq!((notice.session_id.as_str(), notice.status.as_str()), (child, "lost"));
    assert_eq!(end_after(env, parent, seq).await, TurnEndReason::Completed, "the failure notice wakes the parent");
    assert!(head(env).store.waiting_delegations(child).await.unwrap().is_empty(), "the failed dependency no longer blocks the parent");
    let count = env
        .events(parent)
        .await
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::User { command, .. } if command == "task-notification"))
        .count();
    assert_eq!(count, 1, "the parent receives one failure notice");
}

/// Files gate the shell without making elapsed time part of any assertion.
/// Release even on panic, so the held command can never prevent teardown.
struct ShellGate {
    ready: PathBuf,
    release: PathBuf,
}

impl ShellGate {
    fn new(cwd: &Path) -> Self {
        Self { ready: cwd.join("liveness-ready"), release: cwd.join("liveness-release") }
    }

    fn wait_command(&self) -> String {
        // Keep the files in the fixture directory even if a child uses a worktree.
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let (ready, release) = (quote(&self.ready), quote(&self.release));
        format!(": > {ready}; while [ -f {ready} ] && [ ! -f {release} ]; do sleep 0.05; done")
    }

    fn script(&self, env: &Env, prompt: &str, background: bool) {
        let command = format!("{}; echo held-tool-completed", self.wait_command());
        env.mock.push_about(
            &user_prompt_scope(prompt),
            Reply::ToolCalls(vec![("bash".into(), json!({"command": command, "run_in_background": background}).to_string())]),
        );
    }

    async fn started(&self, env: &Env, cwd: &Path) {
        eventually_or(
            "a tool to write its readiness marker",
            async || self.ready.is_file().then_some(()),
            || format!("marker: {}; shell cwd: {}\n{}", self.ready.display(), cwd.display(), env.mock.log()),
        )
        .await;
    }

    fn release(&self) {
        std::fs::write(&self.release, "").unwrap();
    }
}

impl Drop for ShellGate {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, "");
    }
}

#[tokio::test]
async fn worker_disconnect_only_becomes_a_loss_after_grace() {
    let env = Env::new().await;
    let parent_worker = Node::start(&env, "stays-online").await;
    let node = Node::start(&env, "gone").await;
    let project = tempfile::tempdir().unwrap();
    let prompt = "GONE: Leave this model turn open.";
    let _gate = env.mock.push_gated_about(&user_prompt_scope(prompt), Reply::Text(String::new()));
    let (parent, id) = remote_task(&env, &parent_worker, "gone", project.path(), prompt, true).await;
    model_requested(&env, &id, prompt).await;
    node.disconnect(&env).await;
    let now = c6_core::now_ms();
    let before = env.events(&id).await;
    sweep(&env, now).await;
    assert_no_incident(&env, &id).await;
    assert!(!head(&env).store.waiting_delegations(&id).await.unwrap().is_empty());

    let expired = past(now, DEFAULT_LIVENESS_WORKER_GRACE_SECONDS);
    script_ack(&env, &id, "worker disconnected");
    sweep(&env, expired).await;
    sweep(&env, expired + 1).await;
    let rows = incidents(&env, &id).await;
    assert_eq!(rows.len(), 1, "one durable loss across repeated sweeps: {rows:?}");
    assert_eq!(rows[0].0, "lost");
    assert!(rows[0].1.contains("worker disconnected"), "{rows:?}");
    assert_eq!(rows[0].2, before.last().unwrap().seq as i64, "loss names its journal position");
    assert_eq!(env.detail(&id).await.unwrap().session.liveness_loss.as_deref(), Some(rows[0].1.as_str()));
    assert_eq!(env.events(&id).await, before, "the head must not invent worker journal events");
    assert_parent_notified(&env, &parent, &id, "worker disconnected").await;
}

#[tokio::test]
async fn worker_reconnect_in_grace_preserves_the_open_turn() {
    let env = Env::new().await;
    let mut node = Node::start(&env, "back").await;
    let project = tempfile::tempdir().unwrap();
    let (id, gate) = model_wait(&env, &node, project.path(), "BACK: Leave this model turn open.").await;
    node.disconnect(&env).await;
    let now = c6_core::now_ms();
    sweep(&env, now).await;
    assert_no_incident(&env, &id).await;

    node.reconnect().await;
    eventually("the head to register the reattachment", async || head(&env).registry.get(node.worker.identity.meta.worker_id)).await;
    sweep(&env, past(now, DEFAULT_LIVENESS_WORKER_GRACE_SECONDS)).await;
    assert_no_incident(&env, &id).await;
    gate.notify_one();
    assert_eq!(end_after(&env, &id, 0).await, TurnEndReason::Completed);
}

#[tokio::test]
async fn head_restart_preserves_a_new_subagent_and_never_replays_its_tools() {
    let mut env = Env::new().await;
    let parent_worker = Node::start(&env, "restart-parent").await;
    let node = Node::start(&env, "restart-child").await;
    let project = tempfile::tempdir().unwrap();
    let prompt = "HEAD-RESTART: Run the two scoped steps and report success.";
    let scope = user_prompt_scope(prompt);
    let first =
        env.mock.push_gated_about(&scope, Reply::ToolCalls(vec![("bash".into(), json!({"command": "echo before-reconnect"}).to_string())]));
    let second =
        env.mock.push_gated_about(&scope, Reply::ToolCalls(vec![("bash".into(), json!({"command": "echo after-reconnect"}).to_string())]));
    env.mock.push_about(&scope, Reply::Text("Both steps finished, exactly once.".into()));
    let (parent, child) = remote_task(&env, &parent_worker, "restart-child", project.path(), prompt, true).await;
    model_requested(&env, &child, prompt).await;
    env.mock.push_about(&user_prompt_scope(REMOTE_PARENT_PROMPT), Reply::Text("I received the successful report.".into()));

    // The child was only just created. Its actor and first model stream stay
    // alive while the head and all stream-local credential state disappear.
    env.head.take().unwrap().shutdown().await.unwrap();
    eventually("the old child stream to disconnect", async || (!node.worker.link.is_connected()).then_some(())).await;
    first.notify_one();
    eventually("the first tool to finish in the offline journal", async || {
        let (_, events) = node.worker.journal.load_session(&child).await.unwrap()?;
        events
            .iter()
            .any(|e| {
                matches!(
                    &e.kind, Some(c6_proto::session_event::Kind::ToolResult(r)) if r.output == "before-reconnect\n" && !r.is_error
                )
            })
            .then_some(())
    })
    .await;
    // Establish that the second request is already in flight before attaching
    // again. Reconnect invalidates its cached authorization; the third request
    // must get a fresh credential from the replacement head's durable row.
    eventually("the next model request to reach its gate offline", async || {
        env.mock
            .requests()
            .iter()
            .any(|r| {
                r["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|m| m["role"] == "tool" && m["content"].as_str().is_some_and(|s| s.contains("before-reconnect")))
                })
            })
            .then_some(())
    })
    .await;
    env.head = Some(env.listeners.start(super::cfg(env.head_dir.path())).await);
    // Inject the liveness observation 30 seconds after restart instead of
    // sleeping. link.rs separately advances the credential clock through real
    // 30-second outages; MockLlm's SSE clock stays live on this runtime.
    let observed = c6_core::now_ms() + 30_000;
    sweep(&env, observed).await;
    assert_no_incident(&env, &child).await;
    eventually("the surviving child worker to reattach", async || {
        (head(&env).registry.get(node.worker.identity.meta.worker_id).is_some() && node.worker.link.is_connected()).then_some(())
    })
    .await;
    second.notify_one();
    assert_eq!(end_after(&env, &child, 0).await, TurnEndReason::Completed);
    let notice = eventually("the parent's successful child report", async || {
        env.events(&parent).await.into_iter().find_map(|e| match e.kind {
            EventKind::User { task_notice: Some(n), .. } if n.session_id == child => Some(n),
            _ => None,
        })
    })
    .await;
    assert_eq!(notice.status, "completed");
    eventually("the background Task to finish", async || {
        env.events(&parent)
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::BackgroundTask { state, .. } if state == "finished"))
            .then_some(())
    })
    .await;
    let outputs: Vec<String> = env
        .events(&child)
        .await
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, ["before-reconnect\n", "after-reconnect\n"]);
    assert_no_incident(&env, &child).await;
    assert!(!env.events(&parent).await.iter().any(|e| matches!(
        &e.kind, EventKind::User { task_notice: Some(n), .. } if n.session_id == child && matches!(n.status.as_str(), "lost" | "failed")
    )));
}

#[tokio::test]
async fn orphan_running_needs_affirmative_actor_stop_evidence() {
    let env = Env::new().await;
    let parent_worker = Node::start(&env, "orphan-parent").await;
    let node = Node::start(&env, "orphan").await;
    let project = tempfile::tempdir().unwrap();
    let prompt = "ORPHAN: Leave this model turn open.";
    let _gate = env.mock.push_gated_about(&user_prompt_scope(prompt), Reply::Text(String::new()));
    let (parent, id) = remote_task(&env, &parent_worker, "orphan", project.path(), prompt, true).await;
    model_requested(&env, &id, prompt).await;
    evidence(&env, &id, |e| e["activity_kind"] == SessionActivityKind::Model as i32).await;
    node.disconnect(&env).await;
    let before = env.events(&id).await;
    let seq = before.last().unwrap().seq;
    let mut peer = SyntheticPeer::attach(&node, &id, seq).await;

    let stale = past(c6_core::now_ms(), DEFAULT_LIVENESS_MODEL_SILENCE_SECONDS);
    peer.ping(1, None).await;
    sweep(&env, stale).await;
    assert_no_incident(&env, &id).await;

    peer.ping(2, Some(SessionLivenessReport { sessions: Vec::new() })).await;
    sweep(&env, stale).await;
    assert_no_incident(&env, &id).await;

    // Even on the current connection, an observation made for another
    // ownership epoch is not evidence about this incarnation of the child.
    peer.actor(3, &id, 1, SessionActorState::Stopped, "wrong ownership epoch").await;
    sweep(&env, stale).await;
    assert_no_incident(&env, &id).await;
    assert_eq!(env.detail(&id).await.unwrap().session.status, SessionStatus::Running);

    script_ack(&env, &id, "injected actor exit");
    peer.actor(4, &id, 0, SessionActorState::Stopped, "injected actor exit").await;
    sweep(&env, stale).await;
    let rows = incidents(&env, &id).await;
    assert_eq!(rows.len(), 1, "explicitly stopped actor becomes a durable loss: {rows:?}");
    assert_eq!(rows[0].0, "lost");
    assert!(rows[0].1.contains("actor stopped") && rows[0].1.contains("injected actor exit"), "{rows:?}");
    assert_eq!(rows[0].2, seq as i64);
    let visible = env.detail(&id).await.unwrap().session;
    assert_eq!(visible.status, SessionStatus::Idle, "an affirmatively stopped actor no longer looks busy and remains resumable");
    assert_eq!(visible.end_reason, Some(TurnEndReason::Error));
    assert_eq!(visible.liveness_loss.as_deref(), Some(rows[0].1.as_str()));
    assert_eq!(env.events(&id).await, before, "the head must not invent worker journal events");
    assert!(head(&env).registry.get(node.worker.identity.meta.worker_id).is_some(), "only the actor went away");
    assert_parent_notified(&env, &parent, &id, "injected actor exit").await;
}

#[tokio::test]
async fn recovering_reason_changes_do_not_extend_the_recovery_bound() {
    let env = Env::new().await;
    let parent_worker = Node::start(&env, "recovery-parent").await;
    let node = Node::start(&env, "recovery-child").await;
    let project = tempfile::tempdir().unwrap();
    let prompt = "RECOVERY-BOUND: Leave this child turn open for actor recovery.";
    let _gate = env.mock.push_gated_about(&user_prompt_scope(prompt), Reply::Text(String::new()));
    let (parent, id) = remote_task(&env, &parent_worker, "recovery-child", project.path(), prompt, true).await;
    model_requested(&env, &id, prompt).await;
    node.disconnect(&env).await;
    let before = env.events(&id).await;
    let seq = before.last().unwrap().seq;

    let mut old_peer = SyntheticPeer::attach(&node, &id, seq).await;
    old_peer.actor(1, &id, 0, SessionActorState::Recovering, "superseded recovery").await;
    let old = evidence(&env, &id, |r| r["reason"] == "superseded recovery").await["received_at"].as_i64().unwrap();
    let mut peer = SyntheticPeer::attach(&node, &id, seq).await;
    peer.ping(1, None).await;
    sweep(&env, past(old, DEFAULT_SESSION_RECOVERY_SECONDS)).await;
    assert_no_incident(&env, &id).await;
    drop(old_peer);

    peer.actor(2, &id, 0, SessionActorState::Recovering, "reopening journal").await;
    let first = evidence(&env, &id, |r| r["reason"] == "reopening journal").await["received_at"].as_i64().unwrap();
    let expiry = first + i64::from(DEFAULT_SESSION_RECOVERY_SECONDS) * 1000;
    for (nonce, reason) in [(3, "repairing the open turn"), (4, "final retry still blocked")] {
        // Separate receipt timestamps even on a fast host. The sweep at
        // exactly the original deadline will then catch any timer reset.
        let received = evidence(&env, &id, |_| true).await["received_at"].as_i64().unwrap();
        eventually("the head clock to advance past the last receipt timestamp", async || (c6_core::now_ms() > received).then_some(()))
            .await;
        peer.actor(nonce, &id, 0, SessionActorState::Recovering, reason).await;
    }
    sweep(&env, expiry - 1).await;
    assert_no_incident(&env, &id).await;

    script_ack(&env, &id, "final retry still blocked");
    sweep(&env, expiry).await;
    let rows = incidents(&env, &id).await;
    assert_eq!(rows.len(), 1, "one bounded recovery loss: {rows:?}");
    assert_eq!(rows[0].0, "lost");
    assert_eq!(rows[0].2, seq as i64);
    assert!(rows[0].1.contains("session recovering beyond") && rows[0].1.contains("final retry still blocked"), "{rows:?}");
    assert_eq!(env.events(&id).await, before, "recovery loss does not invent worker journal events");
    assert_parent_notified(&env, &parent, &id, "final retry still blocked").await;
}

#[tokio::test]
async fn model_stream_that_never_answers_becomes_a_visible_loss() {
    let env = Env::new().await;
    let parent_worker = Node::start(&env, "model-parent").await;
    let _node = Node::start(&env, "silent-model").await;
    let project = tempfile::tempdir().unwrap();
    let prompt = "MODEL-SILENCE: Wait for the provider to finish this reply.";
    // The opening SSE frame has no text; no answer or finish frame follows.
    let gate = env.mock.push_gated_about(&user_prompt_scope(prompt), Reply::Text(String::new()));
    let (parent, id) = remote_task(&env, &parent_worker, "silent-model", project.path(), prompt, true).await;
    model_requested(&env, &id, prompt).await;
    evidence(&env, &id, |e| e["activity_kind"] == SessionActivityKind::Model as i32).await;
    let before = env.events(&id).await;
    let now = c6_core::now_ms();
    sweep(&env, now).await;
    assert_no_incident(&env, &id).await;

    let expired = past(now, DEFAULT_LIVENESS_MODEL_SILENCE_SECONDS);
    script_ack(&env, &id, "no model progress");
    sweep(&env, expired).await;
    let rows = incidents(&env, &id).await;
    assert_eq!(rows.len(), 1, "one stalled model loss: {rows:?}");
    assert_eq!(rows[0].0, "lost");
    assert!(rows[0].1.contains("no model progress"), "{rows:?}");
    assert_eq!(env.events(&id).await, before, "model silence is an observation, not a journal event");
    assert_parent_notified(&env, &parent, &id, "no model progress").await;
    gate.notify_one();
    assert_eq!(end_after(&env, &id, before.last().unwrap().seq).await, TurnEndReason::Completed);
    sweep(&env, expired + 1).await;
    assert_eq!(env.detail(&id).await.unwrap().session.liveness_loss, None, "new work clears the projected loss");
    let snapshot = c6_head::liveness::snapshot(head(&env)).await.unwrap();
    assert!(snapshot["incidents"].as_array().unwrap().iter().any(|i| i["session_id"] == id && i["resolved_at"].is_number()), "{snapshot}");
}

#[tokio::test]
async fn a_long_live_tool_is_not_model_silence() {
    if !c6_testkit::have("sh") {
        return;
    }
    let env = Env::new().await;
    let node = Node::start(&env, "live-tool").await;
    let project = tempfile::tempdir().unwrap();
    let gate = ShellGate::new(project.path());
    let prompt = "LIVE-TOOL: Run a long foreground shell command.";
    gate.script(&env, prompt, false);
    env.mock.push_about_all(&[prompt, "held-tool-completed"], Reply::Text("Tool done.".into()));
    let id = env.create(&node.worker, project.path(), prompt).await;
    gate.started(&env, project.path()).await;
    evidence(&env, &id, |e| e["activity_kind"] == SessionActivityKind::Tool as i32).await;

    sweep(&env, past(c6_core::now_ms(), DEFAULT_LIVENESS_MODEL_SILENCE_SECONDS)).await;
    assert_no_incident(&env, &id).await;
    assert_eq!(env.detail(&id).await.unwrap().session.status, SessionStatus::Running);
    gate.release();
    assert_eq!(end_after(&env, &id, 0).await, TurnEndReason::Completed);
}

#[tokio::test]
async fn waiting_on_live_background_work_is_not_tree_silence_or_loss() {
    if !c6_testkit::have("sh") {
        return;
    }
    let env = Env::new().await;
    let parent_worker = Node::start(&env, "parent-liveness").await;
    let _child_worker = Node::start(&env, "child-liveness").await;
    let parent_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    let gate = ShellGate::new(child_dir.path());
    let response = env
        .http
        .put(format!("{}/settings/templates", env.api))
        .json(&json!([{
            "name": "liveness-child", "description": "Holds live background work", "workers": ["child-liveness"],
            "cwd": child_dir.path().canonicalize().unwrap(), "connection": "m", "model": "m", "agents": true, "prompt": "{{input}}"
        }]))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.text().await.unwrap());
    let parent_prompt = "PARENT-LIVENESS: Delegate work and wait for it.";
    let child_prompt = "CHILD-LIVENESS: Start the gated background job.";
    env.mock.push_about(
        parent_prompt,
        Reply::ToolCalls(vec![(
            "task".into(),
            json!({"description": "Run the gated job", "prompt": child_prompt, "template": "liveness-child"}).to_string(),
        )]),
    );
    gate.script(&env, child_prompt, true);
    env.mock.push_about(&user_prompt_scope(child_prompt), Reply::Text("Background job started; waiting on it.".into()));
    let parent = env.create(&parent_worker.worker, parent_dir.path(), parent_prompt).await;
    let child = eventually("the remote child link", async || {
        env.events(&parent).await.into_iter().find_map(|e| match e.kind {
            EventKind::SubagentSpawned { child_session_id, remote: Some(_), .. } => Some(child_session_id),
            _ => None,
        })
    })
    .await;
    gate.started(&env, child_dir.path()).await;
    assert_eq!(end_after(&env, &child, 0).await, TurnEndReason::Completed);
    evidence(&env, &child, |e| {
        e["actor_state"] == SessionActorState::Idle.as_str_name()
            && e["running_background_tasks"] == 1
            && e["running_background_processes"] == 1
    })
    .await;
    let now = c6_core::now_ms();
    sweep(&env, now).await;
    assert!(
        !head(&env).store.waiting_delegations(&child).await.unwrap().is_empty(),
        "an idle child with a running task still owes its report"
    );
    assert!(!env.events(&parent).await.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { tool, .. } if tool == "task")));
    assert_no_incident(&env, &parent).await;
    assert_no_incident(&env, &child).await;

    sweep(&env, past(now, DEFAULT_LIVENESS_TREE_SILENCE_SECONDS)).await;
    assert_no_incident(&env, &parent).await;
    assert_no_incident(&env, &child).await;
    env.mock.push_about(parent_prompt, Reply::Text("The child has reported back.".into()));
    env.mock.push_about(&user_prompt_scope(child_prompt), Reply::Text("The background work has ended.".into()));
    let child_floor = env.events(&child).await.last().unwrap().seq;
    gate.release();
    let (output, is_error) = eventually_or(
        "the parent to receive the child's report after the job ends",
        async || {
            env.events(&parent).await.into_iter().find_map(|e| match e.kind {
                EventKind::ToolResult { tool, output, is_error, .. } if tool == "task" => Some((output, is_error)),
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    assert!(!is_error && output.contains(&child), "{output}");
    assert_eq!(end_after(&env, &child, child_floor).await, TurnEndReason::Completed);
    assert_eq!(end_after(&env, &parent, 0).await, TurnEndReason::Completed);
    eventually("the completed child's report obligations to be delivered", async || {
        (head(&env).store.waiting_delegations(&child).await.unwrap().is_empty()
            && head(&env).store.session(&child).await.unwrap().unwrap().status == "idle")
            .then_some(())
    })
    .await;
    sweep(&env, past(c6_core::now_ms(), DEFAULT_LIVENESS_TREE_SILENCE_SECONDS)).await;
    assert_no_incident(&env, &parent).await;
    assert_no_incident(&env, &child).await;
}

const LOCAL_PARENT_PROMPT: &str = "LOCAL-PARENT: Wait for the child and its shell job.";

struct LocalBackground {
    parent: String,
    child: String,
    call: String,
    task: String,
    child_message: String,
}

async fn local_background(env: &Env, node: &Node, project: &Path, gate: &ShellGate) -> LocalBackground {
    let child_prompt = "LOCAL-CHILD: Launch the gated shell job in background.";
    // A local Task appends its instructions to the user message, unlike the
    // remote template's verbatim input. Match the message that reaches Chat.
    let child_message = format!("{child_prompt}\n\n---\n{}", c6_agent::SUBAGENT_NOTE);
    env.mock.push_about(
        &user_prompt_scope(LOCAL_PARENT_PROMPT),
        Reply::ToolCalls(vec![(
            "task".into(),
            json!({
                "description": "Run the local gated job", "prompt": child_prompt
            })
            .to_string(),
        )]),
    );
    gate.script(env, &child_message, true);
    env.mock.push_about(&user_prompt_scope(&child_message), Reply::Text("The child launched the shell job.".into()));

    let parent = env.create(&node.worker, project, LOCAL_PARENT_PROMPT).await;
    let (call, child) = eventually_or(
        "the local child link",
        async || {
            env.events(&parent).await.into_iter().find_map(|e| match e.kind {
                EventKind::SubagentSpawned { call_id, child_session_id, remote: None, .. } => Some((call_id, child_session_id)),
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    let child_cwd =
        eventually("the head to learn the local child's cwd", async || env.detail(&child).await.map(|d| PathBuf::from(d.session.cwd)))
            .await;
    gate.started(env, &child_cwd).await;
    assert_eq!(end_after(env, &child, 0).await, TurnEndReason::Completed);
    evidence(env, &child, |e| e["actor_state"] == SessionActorState::Idle.as_str_name() && e["running_background_tasks"] == 1).await;
    let task = env
        .events(&child)
        .await
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::BackgroundTask { task_id, kind, state, notify: true, .. } if kind == "shell" && state == "started" => Some(task_id),
            _ => None,
        })
        .expect("the idle child still has a durable, counted shell job");
    assert_eq!(env.detail(&parent).await.unwrap().session.status, SessionStatus::Running);
    assert!(
        !node
            .worker
            .journal
            .events_after(&parent, 0, u32::MAX)
            .await
            .unwrap()
            .iter()
            .any(|e| { matches!(&e.kind, Some(c6_proto::session_event::Kind::ToolResult(r)) if r.call_id == call) }),
        "the local waiter must not write a result before its child finishes the job"
    );
    LocalBackground { parent, child, call, task, child_message }
}

async fn assert_shell_completed(env: &Env, child: &str, task: &str) {
    let (state, exit_code) = eventually_or(
        "the child's shell completion",
        async || {
            env.events(child).await.into_iter().find_map(|e| match e.kind {
                EventKind::BackgroundTask { task_id, state, exit_code, .. } if task_id == task && state != "started" => {
                    Some((state, exit_code))
                }
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    assert_eq!((state.as_str(), exit_code), ("finished", 0));
}

#[tokio::test]
async fn local_foreground_task_waits_for_the_childs_counted_background_job() {
    if !c6_testkit::have("sh") {
        return;
    }
    let env = Env::new().await;
    let node = Node::start(&env, "local-background").await;
    let project = tempfile::tempdir().unwrap();
    let gate = ShellGate::new(project.path());
    let LocalBackground { parent, child, call, task, child_message } = local_background(&env, &node, project.path(), &gate).await;

    env.mock.push_about_all(&[&user_prompt_scope(LOCAL_PARENT_PROMPT), &child], Reply::Text("The local child has reported back.".into()));
    env.mock.push_about(&user_prompt_scope(&child_message), Reply::Text("The local shell job has ended.".into()));
    gate.release();
    assert_shell_completed(&env, &child, &task).await;
    assert_eq!(end_after(&env, &parent, 0).await, TurnEndReason::Completed);
    let results: Vec<_> = env
        .events(&parent)
        .await
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::ToolResult { call_id, output, is_error, .. } if call_id == call => Some((output, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "one foreground result for the local child: {results:?}");
    let (output, is_error) = &results[0];
    assert!(!is_error && output.contains(&child), "{output}");
    assert!(output.contains("Background work ended:") || output.contains("The local shell job has ended."), "{output}");
}

#[tokio::test]
async fn local_child_loss_settles_the_pending_task_without_canceling_the_shell() {
    if !c6_testkit::have("sh") {
        return;
    }
    let env = Env::new().await;
    let mut node = Node::start(&env, "local-loss").await;
    let project = tempfile::tempdir().unwrap();
    let gate = ShellGate::new(project.path());
    let LocalBackground { parent, child, call, task, child_message } = local_background(&env, &node, project.path(), &gate).await;
    node.disconnect(&env).await;

    let before = env.events(&child).await;
    let seq = before.last().unwrap().seq;
    let cause = "injected local actor stop";
    let mut peer = SyntheticPeer::attach(&node, &child, seq).await;
    peer.actor(1, &child, 0, SessionActorState::Stopped, cause).await;
    sweep(&env, c6_core::now_ms()).await;
    let rows = incidents(&env, &child).await;
    assert_eq!(rows.len(), 1, "the head loses the idle child with counted work: {rows:?}");
    assert_eq!(rows[0].0, "lost");
    assert!(rows[0].1.contains(cause), "{rows:?}");
    assert_eq!(env.events(&child).await, before, "the loss does not invent child journal events");

    env.mock
        .push_about_all(&[&user_prompt_scope(LOCAL_PARENT_PROMPT), &child, cause], Reply::Text("I received the local child loss.".into()));
    drop(peer);
    node.reconnect().await;
    let notice_seq = eventually_or(
        "the parent to receive the structured loss after reconnecting",
        async || {
            env.events(&parent).await.into_iter().find_map(|e| match e.kind {
                EventKind::User { command, text, task_notice: Some(notice), .. } if command == "task-notification" => {
                    assert_eq!((notice.session_id.as_str(), notice.status.as_str()), (child.as_str(), "lost"));
                    assert!(text.contains(cause) && text.contains(&child), "{text}");
                    Some(e.seq)
                }
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    assert_eq!(end_after(&env, &parent, notice_seq).await, TurnEndReason::Completed);
    let events = env.events(&parent).await;
    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { call_id, output, is_error, .. } if call_id == &call => Some((output, *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "loss releases the original foreground waiter: {results:?}");
    assert!(results[0].1 && results[0].0.contains(cause), "{results:?}");
    assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::User { command, .. } if command == "task-notification")).count(), 1);

    // This is an observation, not permission to interrupt surviving work.
    assert!(
        !env.events(&child)
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::BackgroundTask { task_id, state, .. } if task_id == &task && state != "started"))
    );
    env.mock.push_about(&user_prompt_scope(&child_message), Reply::Text("The surviving local shell job has ended.".into()));
    gate.release();
    assert_shell_completed(&env, &child, &task).await;
}

#[tokio::test]
async fn local_send_message_loss_releases_the_waiter_before_the_child_journals_the_input() {
    if !c6_testkit::have("sh") {
        return;
    }
    let env = Env::new().await;
    let mut node = Node::start(&env, "local-send-wedge").await;
    let project = tempfile::tempdir().unwrap();
    let gate = ShellGate::new(project.path());
    let command = format!("Wait for the fixture gate: !`{}`", gate.wait_command());
    env.http
        .put(format!("{}/settings/commands", env.api))
        .json(&json!([{
            "name": "liveness-wedge", "body": command
        }]))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let child_prompt = "Prepare for a future local continuation.";
    let child_message = format!("{child_prompt}\n\n---\n{}", c6_agent::SUBAGENT_NOTE);
    let parent_prompt = "Create a child for the pre-journal wedge test.";
    env.mock.push_about(
        &user_prompt_scope(parent_prompt),
        Reply::ToolCalls(vec![(
            "task".into(),
            json!({
                "description": "Get ready for another request", "prompt": child_prompt
            })
            .to_string(),
        )]),
    );
    env.mock.push_about(&user_prompt_scope(&child_message), Reply::Text("The child is ready.".into()));
    env.mock.push_about(&user_prompt_scope(parent_prompt), Reply::Text("I will continue the child later.".into()));
    let parent = env.create(&node.worker, project.path(), parent_prompt).await;
    assert_eq!(end_after(&env, &parent, 0).await, TurnEndReason::Completed);
    let parent_events = env.events(&parent).await;
    let parent_floor = parent_events.last().unwrap().seq;
    let child = parent_events
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::SubagentSpawned { child_session_id, remote: None, .. } => Some(child_session_id),
            _ => None,
        })
        .expect("the parent has a local child");
    let child_cwd = PathBuf::from(env.detail(&child).await.unwrap().session.cwd);

    // A direct input is inside gated slash-command expansion, before the
    // actor starts a turn or consumes the next mailbox item. Local children
    // inherit tool hooks, not their own UserPromptSubmit runner.
    env.http
        .post(format!("{}/sessions/{child}/messages", env.api))
        .json(&json!({
            "text": "/liveness-wedge"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    gate.started(&env, &child_cwd).await;
    assert_eq!(env.detail(&child).await.unwrap().session.status, SessionStatus::Idle);

    let parent_resume = "Continue the idle child using SendMessage now.";
    let continuation = "The delegated input that cannot reach the child journal.";
    env.mock.push_about(
        &user_prompt_scope(parent_resume),
        Reply::ToolCalls(vec![(
            "send_message".into(),
            json!({
                "to": child, "message": continuation, "run_in_background": true
            })
            .to_string(),
        )]),
    );
    env.mock.push_about(&user_prompt_scope(parent_resume), Reply::Text("I will wait for the continuation report.".into()));
    env.http
        .post(format!("{}/sessions/{parent}/messages", env.api))
        .json(&json!({"text": parent_resume}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let call = eventually_or(
        "the parent's SendMessage call",
        async || {
            env.events(&parent).await.into_iter().filter(|e| e.seq > parent_floor).find_map(|e| match e.kind {
                EventKind::Assistant { blocks, .. } => blocks.into_iter().find_map(|b| match b {
                    c6_api::ContentBlock::ToolUse { id, name, .. } if name == "send_message" => Some(id),
                    _ => None,
                }),
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    // The background SendMessage has a local waiter even while its target
    // actor cannot consume the mailbox. Wait for the head's durable row,
    // not merely the already-journaled assistant tool call.
    let prefix = "subagent:local-wait:delegation:";
    let task = eventually_or(
        "the head to persist the local SendMessage waiter before child input",
        async || {
            head(&env).store.waiting_delegations(&child).await.unwrap().into_iter().find_map(|(id, registered_call)| {
                let (_, task) = id.strip_prefix(prefix)?.split_once(':')?;
                assert_eq!(registered_call, call, "the row belongs to the original SendMessage call");
                Some(task.to_owned())
            })
        },
        || env.mock.log(),
    )
    .await;
    assert_eq!(end_after(&env, &parent, parent_floor).await, TurnEndReason::Completed);

    let before = env.events(&child).await;
    assert!(!before.iter().any(|e| matches!(&e.kind, EventKind::User { text, .. } if text == continuation)));
    let seq = before.last().unwrap().seq;
    node.disconnect(&env).await;
    let mut peer = SyntheticPeer::attach(&node, &child, seq).await;
    let cause = "injected stop before local input journal";
    peer.actor(1, &child, 0, SessionActorState::Stopped, cause).await;
    sweep(&env, c6_core::now_ms()).await;
    let rows = incidents(&env, &child).await;
    assert_eq!(rows.len(), 1, "the unconsumed obligation makes the idle child eligible for loss: {rows:?}");
    assert!(rows[0].1.contains(cause), "{rows:?}");
    assert_eq!(env.events(&child).await, before, "the head did not fabricate the missing child input");

    env.mock
        .push_about_all(&[&user_prompt_scope(parent_resume), &child, cause], Reply::Text("The SendMessage waiter got its loss.".into()));
    drop(peer);
    node.reconnect().await;
    let notice_seq = eventually_or(
        "the loss to wake the parent",
        async || {
            env.events(&parent).await.into_iter().find_map(|e| match e.kind {
                EventKind::User { command, task_notice: Some(n), .. } if command == "task-notification" && n.status == "lost" => {
                    Some(e.seq)
                }
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    assert_eq!(end_after(&env, &parent, notice_seq).await, TurnEndReason::Completed);
    let (state, code) = eventually_or(
        "the original SendMessage background waiter to end",
        async || {
            env.events(&parent).await.into_iter().find_map(|e| match e.kind {
                EventKind::BackgroundTask { task_id, state, exit_code, .. } if task_id == task && state != "started" => {
                    Some((state, exit_code))
                }
                _ => None,
            })
        },
        || env.mock.log(),
    )
    .await;
    assert!(state == "failed" || code != 0, "loss unexpectedly ended {task} as {state}/{code}");
    let receipts = head(&env).store.events_after(&parent, parent_floor, i64::MAX).await.unwrap();
    assert!(
        receipts.iter().any(|e| matches!(&e.kind, Some(c6_proto::session_event::Kind::SubagentReported(r))
        if r.child_session_id == child && r.call_id == call && r.status == "lost")),
        "the durable receipt must settle the same SendMessage call"
    );
    assert!(head(&env).store.waiting_delegations(&child).await.unwrap().is_empty());
    let events = env.events(&parent).await;
    let losses: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::User { command, task_notice: Some(n), .. }
                if e.seq > parent_floor && command == "task-notification" && n.status == "lost" =>
            {
                Some(n.session_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(losses, [child.as_str()]);
    assert_eq!(env.events(&child).await, before, "the actor is still wedged before the delegated input");
}
