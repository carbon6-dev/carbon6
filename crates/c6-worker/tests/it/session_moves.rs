//! Cross-worker placement with isolated workers and workspaces.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Weak};

use c6_api::{ContentBlock, Event, EventKind, Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_core::WorkerId;
use c6_head::HeadConfig;
use c6_proto::{SessionEvent, SessionEventBatch, TurnStarted, UserMessage, session_event};
use c6_testkit::{MockLlm, Reply, eventually, eventually_or, within};
use c6_worker::{ConnState, Extension, Worker, conn, enroll, rpc};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::{CancellationToken, DropGuard};
use uuid::Uuid;

use crate::HeadListeners;

struct TestWorker {
    id: Uuid,
    state: PathBuf,
    worker: Weak<Worker>,
    process: Option<WorkerProcess>,
}

struct WorkerProcess {
    stop: DropGuard,
    thread: std::thread::JoinHandle<anyhow::Result<()>>,
}

impl TestWorker {
    async fn stop(&mut self) {
        let WorkerProcess { stop, thread } = self.process.take().unwrap();
        drop(stop);
        within("the fixture worker runtime to stop", tokio::task::spawn_blocking(move || thread.join().unwrap())).await.unwrap().unwrap();
    }

    async fn wait_disconnected(&self, head: &c6_head::Head) {
        eventually("the head to unregister the disconnected worker", async || {
            head.state.registry.get(WorkerId(self.id)).is_none().then_some(())
        })
        .await;
    }

    async fn restart(&mut self) {
        assert!(self.process.is_none());
        let replacement = Env::attach(self.state.clone(), None).await;
        assert_eq!(replacement.id, self.id, "reopen the same worker state and identity");
        *self = replacement;
    }
}

/// Test-only RPC interceptor. A move reaches an observable durable phase
/// before issuing this method; do not let the real side effect run yet.
struct MoveGate {
    method: &'static str,
    entered: Mutex<Option<oneshot::Sender<Value>>>,
    release: CancellationToken,
    after_release: GateAction,
}

enum GateAction {
    FailAll,
    FailFirst,
    Dispatch,
}

impl MoveGate {
    fn new(method: &'static str, after_release: GateAction) -> (Arc<Self>, oneshot::Receiver<Value>, DropGuard) {
        let (entered, reached) = oneshot::channel();
        let release = CancellationToken::new();
        (
            Arc::new(Self { method, entered: Mutex::new(Some(entered)), release: release.clone(), after_release }),
            reached,
            release.drop_guard(),
        )
    }
}

#[async_trait::async_trait]
impl Extension for MoveGate {
    fn capabilities(&self) -> Vec<String> {
        Vec::new()
    }

    async fn handle(&self, method: &str, params: Value) -> Option<anyhow::Result<Value>> {
        if method != self.method {
            return None;
        }
        let first = self.entered.lock().unwrap().take();
        let first_call = first.is_some();
        if let Some(entered) = first {
            let _ = entered.send(params);
        }
        self.release.cancelled().await;
        match self.after_release {
            GateAction::FailAll => Some(Err(anyhow::anyhow!("fixture disconnected at {method}"))),
            GateAction::FailFirst if first_call => Some(Err(anyhow::anyhow!("fixture disconnected at {method}"))),
            GateAction::FailFirst | GateAction::Dispatch => None,
        }
    }
}

struct HeldReply {
    params: Value,
    result: Result<Value, String>,
}

/// Run the real operation once, then hold only its response until one side stops.
struct LostReplyGate {
    method: &'static str,
    worker: Weak<Worker>,
    first: Mutex<Option<oneshot::Sender<HeldReply>>>,
    release: CancellationToken,
}

impl LostReplyGate {
    fn install(method: &'static str, worker: &TestWorker) -> (oneshot::Receiver<HeldReply>, DropGuard) {
        let (first, reached) = oneshot::channel();
        let release = CancellationToken::new();
        worker.worker.upgrade().unwrap().set_extension(Arc::new(Self {
            method,
            worker: worker.worker.clone(),
            first: Mutex::new(Some(first)),
            release: release.clone(),
        }));
        (reached, release.drop_guard())
    }
}

#[async_trait::async_trait]
impl Extension for LostReplyGate {
    fn capabilities(&self) -> Vec<String> {
        Vec::new()
    }

    async fn handle(&self, method: &str, params: Value) -> Option<anyhow::Result<Value>> {
        if method != self.method {
            return None;
        }
        // Take before dispatch: the inner call and any later replay go straight to the real handler.
        let first = self.first.lock().unwrap().take()?;
        let worker = self.worker.upgrade().expect("the RPC's worker is alive");
        let result = rpc::handle(&worker, method, params.clone()).await;
        let _ = first.send(HeldReply { params, result: result.as_ref().cloned().map_err(|error| format!("{error:#}")) });
        self.release.cancelled().await;
        Some(result)
    }
}

struct Env {
    source: TestWorker,
    target: TestWorker,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    connection: String,
    head: Option<c6_head::Head>,
    head_cfg: HeadConfig,
    listeners: HeadListeners,
    root: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
        Self::with_extensions(None, None).await
    }

    async fn with_extensions(source_ext: Option<Arc<dyn Extension>>, target_ext: Option<Arc<dyn Extension>>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let head_cfg = HeadConfig {
            data_dir: path.join("head"),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        };
        let listeners = HeadListeners::new();
        let head = listeners.start(head_cfg.clone()).await;
        let source = Self::worker(&head, path.join("source"), "move-source", source_ext).await;
        let target = Self::worker(&head, path.join("target"), "move-target", target_ext).await;
        let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
        let mock = MockLlm::start().await;
        let http = reqwest::Client::new();
        let connection: c6_api::Connection = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "move-test", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        Self { source, target, api, http, mock, connection: connection.id, head: Some(head), head_cfg, listeners, root }
    }

    fn head(&self) -> &c6_head::Head {
        self.head.as_ref().unwrap()
    }

    async fn restart_head(&mut self, release: DropGuard) {
        let head = self.head.take().unwrap();
        head.shutdown_token().cancel();
        drop(release);
        within("the old head to shut down", head.shutdown()).await.unwrap();
        self.reopen_head().await;
    }

    async fn reopen_head(&mut self) {
        let head = self.listeners.start(self.head_cfg.clone()).await;
        eventually("both existing workers to reconnect to the new head", async || {
            [self.source.id, self.target.id].iter().all(|id| head.state.registry.get(WorkerId(*id)).is_some()).then_some(())
        })
        .await;
        self.head = Some(head);
    }

    async fn worker(head: &c6_head::Head, state: PathBuf, name: &str, extension: Option<Arc<dyn Extension>>) -> TestWorker {
        let e = c6_head::api::new_enrollment(&head.state, Some(name)).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(&state).unwrap();
        Self::attach(state, extension).await
    }

    async fn attach(state: PathBuf, extension: Option<Arc<dyn Extension>>) -> TestWorker {
        let stop = CancellationToken::new();
        let guard = stop.clone().drop_guard();
        let (ready, attached) = oneshot::channel();
        let dir = state.clone();
        // Dropping only conn::run would leave the old turn actor alive. Give
        // the fixture worker its own runtime, just like a separate process.
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let worker = crate::open_worker(&dir).await;
                if let Some(extension) = extension {
                    worker.set_extension(extension);
                }
                {
                    let mut overrides = worker.overrides.lock().unwrap();
                    overrides.import_home = Some(dir.join("empty-home"));
                    overrides.kit_jobs = Some(false);
                }
                let id = worker.identity.meta.worker_id.0;
                let weak = Arc::downgrade(&worker);
                let (tx, mut rx) = watch::channel(ConnState::Connecting);
                let task = tokio::spawn(conn::run(worker, tx, stop));
                within("the move test worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
                let _ = ready.send((id, weak));
                task.await?
            })
        });
        let (id, worker) = within("the move test worker's attachment signal", attached).await.unwrap();
        TestWorker { id, state, worker, process: Some(WorkerProcess { stop: guard, thread }) }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.http.get(format!("{}/{path}", self.api)).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, String) {
        let r = self.http.request(Method::POST, format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        (r.status(), r.text().await.unwrap())
    }

    async fn start(&self, worker: &TestWorker, cwd: Option<&Path>, prompt: &str) -> Session {
        let mut body = json!({"worker_id": worker.id, "connection_id": self.connection, "model": "m", "prompt": prompt});
        if let Some(cwd) = cwd {
            body["cwd"] = json!(cwd);
        }
        let (status, text) = self.post("sessions", body).await;
        assert!(status.is_success(), "create session: {status}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn message(&self, id: &str, prompt: &str) {
        let (status, text) = self.post(&format!("sessions/{id}/messages"), json!({"text": prompt})).await;
        assert!(status.is_success(), "send message: {status}: {text}");
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.get(&format!("sessions/{id}/events")).await
    }

    async fn detail(&self, id: &str) -> SessionDetail {
        self.get(&format!("sessions/{id}")).await
    }

    async fn placements(&self, id: &str) -> Vec<Value> {
        self.get(&format!("sessions/{id}/placement")).await
    }

    async fn restrict_target_to_other_realm(&self) {
        for (method, path, body) in [
            (Method::POST, "realms".to_owned(), json!({"id": "other", "name": "Other"})),
            (Method::PUT, format!("workers/{}/realms", self.target.id), json!({"realms": ["other"]})),
        ] {
            let response =
                self.http.request(method, format!("{}/{path}", self.api)).header("x-c6-realm", "admin").json(&body).send().await.unwrap();
            let status = response.status();
            assert!(status.is_success(), "{path}: {status}: {}", response.text().await.unwrap());
        }
    }

    async fn assert_source_unchanged(&self, id: &str, before: &[Value], epoch: i64) {
        let row = self.head().state.store.session(id).await.unwrap().unwrap();
        assert_eq!(row.worker_id, self.source.id.to_string());
        assert_eq!(row.ownership_epoch, epoch);
        assert_eq!(row.cwd, before[0]["cwd"].as_str().unwrap());
        assert_eq!(self.placements(id).await, before);
        assert!(!self.events(id).await.iter().any(|e| matches!(e.kind, EventKind::PlaceChanged { .. })));
    }

    async fn assert_move_failed(&self, command: &str, reason: &str) {
        let (state, error) = eventually("the move to finish rolling back", async || self.move_outcome(command).await).await;
        assert_eq!(state, "failed", "move {command}: {error:?}");
        assert!(error.as_deref().is_some_and(|error| error.contains(reason)), "move {command}: {error:?}");
    }

    async fn move_outcome(&self, command: &str) -> Option<(String, Option<String>)> {
        let (state, error) = self.move_state(command).await;
        matches!(state.as_str(), "done" | "failed").then_some((state, error))
    }

    async fn move_state(&self, command: &str) -> (String, Option<String>) {
        sqlx::query_as("SELECT state, error FROM session_moves WHERE id = ?")
            .bind(command)
            .fetch_one(self.head().state.store.pool())
            .await
            .unwrap()
    }

    async fn move_progress(&self, command: &str) -> Value {
        let progress: String = sqlx::query_scalar("SELECT result_json FROM session_moves WHERE id = ?")
            .bind(command)
            .fetch_one(self.head().state.store.pool())
            .await
            .unwrap();
        serde_json::from_str(&progress).unwrap()
    }

    async fn ended(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually_or(
            "the session's turns to end and the queue to clear",
            async || {
                let events = self.events(id).await;
                let detail = self.detail(id).await;
                let ended: Vec<_> = events
                    .iter()
                    .filter_map(|e| match &e.kind {
                        EventKind::TurnEnded { reason, .. } => Some(*reason),
                        _ => None,
                    })
                    .collect();
                if ended.len() < turns || detail.session.status != SessionStatus::Idle || !detail.queued.is_empty() {
                    return None;
                }
                assert_eq!(ended, vec![TurnEndReason::Completed; turns]);
                Some(events)
            },
            || self.mock.log(),
        )
        .await
    }

    async fn request_move(&self, id: &str, body: Value) -> String {
        let (status, text) = self.post(&format!("sessions/{id}/placement"), body).await;
        assert_eq!(status, StatusCode::ACCEPTED, "placement: {text}");
        let receipt: Value = serde_json::from_str(&text).unwrap();
        receipt_id(&receipt).to_owned()
    }

    async fn moved(&self, id: &str, body: Value, target: &TestWorker) -> (Session, Event) {
        let command = self.request_move(id, body).await;
        self.wait_moved(id, &command, target).await
    }

    async fn wait_moved(&self, id: &str, command: &str, target: &TestWorker) -> (Session, Event) {
        eventually("the move event, route and placement history to agree", async || {
            // Adoption changes the head's route and emits PlaceChanged before
            // activation opens command delivery on the destination.
            let (state, error) = self.move_outcome(command).await?;
            assert_eq!(state, "done", "move {command} was refused: {error:?}");
            let detail = self.detail(id).await;
            if detail.session.worker_id != target.id || detail.session.status != SessionStatus::Idle || !detail.queued.is_empty() {
                return None;
            }
            let events = self.events(id).await;
            let mut moves =
                events.into_iter().filter(|e| matches!(&e.kind, EventKind::PlaceChanged { command_id, .. } if command_id == command));
            let event = moves.next()?;
            assert!(moves.next().is_none(), "duplicate completion for move {command}");
            let EventKind::PlaceChanged { to, .. } = &event.kind else { unreachable!() };
            let places = self.placements(id).await;
            (detail.session.id == id
                && detail.session.cwd == to.cwd
                && detail.session.last_seq >= event.seq
                && places[0]["seq"] == event.seq
                && places[0]["cwd"] == to.cwd)
                .then_some((detail.session, event))
        })
        .await
    }

    async fn assert_input_fenced(&self, id: &str) {
        let (status, text) = self.post(&format!("sessions/{id}/messages"), json!({"text": "input while placement is fenced"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "input must be refused while the move is unresolved: {text}");
        assert!(self.detail(id).await.queued.is_empty(), "fenced input must not be accepted into the queue");
    }

    async fn assert_stale_append_fenced(&self, id: &str, old_worker: Uuid, old_epoch: u64) {
        let detail = self.detail(id).await;
        let last = detail.session.last_seq;
        let event = |seq, kind| SessionEvent { seq, kind: Some(kind), ..Default::default() };
        let ack = c6_head::sessions::ingest(
            &self.head().state,
            WorkerId(old_worker),
            SessionEventBatch {
                session_id: id.into(),
                ownership_epoch: old_epoch,
                events: vec![
                    event(
                        last + 1,
                        session_event::Kind::User(UserMessage {
                            text: "input from an old worker incarnation".into(),
                            ..Default::default()
                        }),
                    ),
                    event(last + 2, session_event::Kind::TurnStarted(TurnStarted { turn: 2, ..Default::default() })),
                ],
            },
        )
        .await
        .unwrap();
        assert_eq!(ack, None, "an old incarnation must not be acknowledged at the current high-water mark");
        assert_eq!(self.head().state.store.last_seq(id).await.unwrap(), last);
        let after = self.detail(id).await;
        assert_eq!(after.session.status, detail.session.status, "the stale batch must not start a turn");
        assert_eq!(after.session.worker_id, detail.session.worker_id);
    }

    fn script(&self, prompt: &str, command: &str, reply: &str) {
        self.mock.push_about(prompt, Reply::ToolCalls(vec![("bash".into(), json!({"command": command}).to_string())]));
        self.mock.push_about(prompt, Reply::Text(reply.into()));
    }
}

fn receipt_id(receipt: &Value) -> &str {
    receipt["command_id"].as_str().or_else(|| receipt["move_id"].as_str()).unwrap_or_else(|| panic!("no move id: {receipt}"))
}

fn worker_move(expected_place_seq: &Value, worker: &TestWorker) -> Value {
    json!({"kind": "worker", "expected_place_seq": expected_place_seq, "worker": worker.id, "clone": false})
}

fn assert_tool_output(events: &[Event], after: u64, expected: &str) {
    let result = events.iter().rev().find_map(|e| match &e.kind {
        EventKind::ToolResult { tool, output, is_error, .. } if e.seq > after && tool == "bash" => Some((output, is_error)),
        _ => None,
    });
    let (output, is_error) = result.expect("the turn ran its Bash tool");
    assert!(!is_error, "{output}");
    assert_eq!(output.trim(), expected);
}

#[tokio::test]
async fn scratch_moves_back_with_current_files_and_retains_originals() {
    let env = Env::new().await;
    let first = "Prepare the first move note in scratch";
    let reply = "The scratch note is ready to travel.";
    env.script(first, "mkdir notes && printf original > notes/draft.txt && cat notes/draft.txt", reply);
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "original");
    let source = env.source.state.join("scratch").join(id);
    let initial = env.placements(id).await;
    assert_eq!(initial[0]["cwd"], json!(source));

    let (on_target, moved) = env.moved(id, worker_move(&initial[0]["seq"], &env.target), &env.target).await;
    let target = Path::new(&on_target.cwd);
    assert!(target.starts_with(&env.target.state), "{}", target.display());
    assert_ne!(target, source);
    let EventKind::PlaceChanged { from, copied_paths, .. } = &moved.kind else { unreachable!() };
    assert_eq!(Path::new(&from.cwd), source);
    assert_eq!(copied_paths, &["notes/draft.txt"]);
    assert_eq!(std::fs::read_to_string(target.join("notes/draft.txt")).unwrap(), "original");
    assert_eq!(std::fs::read_to_string(source.join("notes/draft.txt")).unwrap(), "original");

    let second = "Read the transferred note, then update only the target copy";
    env.script(second, "cat notes/draft.txt && printf target > notes/draft.txt", "I updated the transferred note.");
    env.message(id, second).await;
    assert_tool_output(&env.ended(id, 2).await, moved.seq, "original");
    let history = env
        .mock
        .requests()
        .iter()
        .rev()
        .map(|request| request["messages"].to_string())
        .find(|messages| messages.contains(second) && messages.contains(first) && messages.contains(reply))
        .expect("the model had the opening user and assistant messages on the new worker");
    assert!(history.contains(&on_target.cwd), "the model was told about the new place: {history}");
    assert_eq!(std::fs::read_to_string(target.join("notes/draft.txt")).unwrap(), "target");
    assert_eq!(std::fs::read_to_string(source.join("notes/draft.txt")).unwrap(), "original");

    let moved_places = env.placements(id).await;
    let (back, back_event) = env
        .moved(id, json!({"kind": "back", "expected_place_seq": moved_places[0]["seq"], "place_seq": initial[0]["seq"]}), &env.source)
        .await;
    // Move back changes the worker, not the file versions: a new workspace
    // carries the current edits without overwriting either retained snapshot.
    let returned = Path::new(&back.cwd);
    assert!(returned.starts_with(&env.source.state));
    assert_ne!(returned, source);
    assert_eq!(std::fs::read_to_string(returned.join("notes/draft.txt")).unwrap(), "target");
    let places = env.placements(id).await;
    assert_eq!(places.len(), 3);
    assert_eq!(places[1]["seq"], moved.seq);
    assert_eq!(places[2]["seq"], initial[0]["seq"]);
    // We are back on the same UUID; only the incarnation fence distinguishes
    // this event batch from a legitimate append by the current owner.
    env.assert_stale_append_fenced(id, env.source.id, 0).await;

    let third = "Read the current note in the new snapshot on the original worker";
    env.script(third, "cat notes/draft.txt && printf returned > after-back.txt", "I read the current version of the note.");
    env.message(id, third).await;
    let events = env.ended(id, 3).await;
    assert_tool_output(&events, back_event.seq, "target");
    assert_eq!(events.iter().filter(|e| matches!(e.kind, EventKind::PlaceChanged { .. })).count(), 2);
    assert_eq!(std::fs::read_to_string(returned.join("after-back.txt")).unwrap(), "returned");
    assert!(!source.join("after-back.txt").exists());
    assert_eq!(std::fs::read_to_string(source.join("notes/draft.txt")).unwrap(), "original");
    assert!(!target.join("after-back.txt").exists());
    assert_eq!(std::fs::read_to_string(target.join("notes/draft.txt")).unwrap(), "target");
}

#[tokio::test]
async fn destination_outside_the_session_realm_is_refused_before_admission() {
    let env = Env::new().await;
    let prompt = "Check the source before trying a destination outside the realm";
    env.script(prompt, "printf source", "The source works.");
    let started = env.start(&env.source, None, prompt).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "source");
    let before = env.placements(id).await;
    env.restrict_target_to_other_realm().await;

    let (status, text) = env.post(&format!("sessions/{id}/placement"), worker_move(&before[0]["seq"], &env.target)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(text.contains("realm may not use the destination worker"), "{text}");
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_moves WHERE session_id = ?")
        .bind(id)
        .fetch_one(env.head().state.store.pool())
        .await
        .unwrap();
    assert_eq!(receipts, 0, "a forbidden request must not reserve an ownership epoch");
    env.assert_source_unchanged(id, &before, 0).await;
}

#[tokio::test]
async fn destination_realm_revoked_during_prepare_prevents_adoption() {
    let (gate, reached, release) = MoveGate::new("move.prepare", GateAction::Dispatch);
    let env = Env::with_extensions(None, Some(gate)).await;
    let first = "Write a scratch file before destination access is revoked";
    let reply = "The original scratch file is ready.";
    env.script(first, "printf original > retained.txt && cat retained.txt", reply);
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    let opening = env.ended(id, 1).await;
    assert_tool_output(&opening, 0, "original");
    let before = env.placements(id).await;

    let command = env.request_move(id, worker_move(&before[0]["seq"], &env.target)).await;
    let params = within("the destination prepare to pass the pre-RPC realm check", reached).await.unwrap();
    assert_eq!(params["move_id"], command);
    assert_eq!(env.move_state(&command).await.0, "transferred");
    env.assert_source_unchanged(id, &before, 0).await;
    env.restrict_target_to_other_realm().await;
    drop(release);

    env.assert_move_failed(&command, "destination worker is no longer available to this session's realm").await;
    let event: Option<String> = sqlx::query_scalar("SELECT json_extract(result_json, '$.event') FROM session_moves WHERE id = ?")
        .bind(&command)
        .fetch_one(env.head().state.store.pool())
        .await
        .unwrap();
    assert!(event.is_some_and(|event| !event.is_empty()), "the worker must have prepared successfully before the CAS refused");
    env.assert_source_unchanged(id, &before, 0).await;

    let next = "Read the original scratch file after realm rollback";
    env.script(next, "cat retained.txt", "The source kept its scratch file.");
    env.message(id, next).await;
    assert_tool_output(&env.ended(id, 2).await, opening.last().unwrap().seq, "original");
    assert!(env.mock.requests().iter().any(|request| {
        let history = request["messages"].to_string();
        history.contains(first) && history.contains(reply) && history.contains(next)
    }));
}

#[tokio::test]
async fn active_background_task_prevents_cross_worker_move_until_stopped() {
    let env = Env::new().await;
    let prompt = "Start a background reader that waits for an explicit stop";
    env.mock.push_about(
        prompt,
        Reply::ToolCalls(vec![(
            "bash".into(),
            json!({
                "command": "mkfifo move-test.fifo && exec cat move-test.fifo",
                "run_in_background": true,
                "notify": false,
            })
            .to_string(),
        )]),
    );
    env.mock.push_about(prompt, Reply::Text("The background reader is running.".into()));
    let started = env.start(&env.source, None, prompt).await;
    let id = &started.id;
    env.ended(id, 1).await;
    let task = format!("sessions/{id}/tasks/bg1");
    let before = env.placements(id).await;
    let fifo = Path::new(before[0]["cwd"].as_str().unwrap()).join("move-test.fifo");
    eventually("the background task to create its FIFO and report running", async || {
        (fifo.exists()
            && env.events(id).await.iter().any(
                |event| matches!(&event.kind, EventKind::BackgroundTask { task_id, state, .. } if task_id == "bg1" && state == "started"),
            )
            && env.get::<Value>(&task).await["running"] == true)
            .then_some(())
    })
    .await;

    let request = worker_move(&before[0]["seq"], &env.target);
    let command = env.request_move(id, request.clone()).await;
    let (state, reason) = eventually("the move to refuse the running background task", async || env.move_outcome(&command).await).await;
    let still_running = env.get::<Value>(&task).await["running"] == true;
    let (status, text) = env.post(&format!("{task}/stop"), Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    // Wait for the stop event at the head too, so its append cannot race the retry's admission watermark.
    eventually("the background task's user stop to reach the head", async || {
        (env.get::<Value>(&task).await["running"] == false
            && env.events(id).await.iter().any(|event| {
                matches!(&event.kind, EventKind::BackgroundTask { task_id, state, .. } if task_id == "bg1" && state == "stopped_by_user")
            }))
        .then_some(())
    })
    .await;
    std::fs::remove_file(fifo).unwrap();

    assert_eq!(state, "failed", "{reason:?}");
    assert!(reason.as_deref().is_some_and(|reason| reason.contains("running background tasks")), "{reason:?}");
    assert!(still_running, "a refused move must not stop the task");
    env.assert_source_unchanged(id, &before, 0).await;
    env.moved(id, request, &env.target).await;
}

#[tokio::test]
async fn retry_after_prepare_refusal_reserves_a_fresh_ownership_epoch() {
    let (gate, reached, release) = MoveGate::new("move.prepare", GateAction::FailFirst);
    let env = Env::with_extensions(None, Some(gate)).await;
    let first = "Write a scratch file before retrying a refused prepare";
    env.script(first, "printf retry > retained.txt && cat retained.txt", "The retry fixture is ready.");
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "retry");
    let before = env.placements(id).await;
    let request = worker_move(&before[0]["seq"], &env.target);

    let refused = env.request_move(id, request.clone()).await;
    let params = within("the first destination prepare to be intercepted", reached).await.unwrap();
    assert_eq!(params["move_id"], refused);
    assert_eq!(params["source_epoch"], 0);
    assert_eq!(params["target_epoch"], 1);
    assert_eq!(env.move_state(&refused).await.0, "transferred");
    drop(release);
    env.assert_move_failed(&refused, "fixture disconnected at move.prepare").await;
    env.assert_source_unchanged(id, &before, 0).await;

    let retry = env.request_move(id, request).await;
    assert_ne!(retry, refused);
    let (_, moved) = env.wait_moved(id, &retry, &env.target).await;
    let epochs: (i64, i64) = sqlx::query_as("SELECT source_epoch, target_epoch FROM session_moves WHERE id = ?")
        .bind(&retry)
        .fetch_one(env.head().state.store.pool())
        .await
        .unwrap();
    assert_eq!(epochs, (0, 2));
    assert_eq!(env.head().state.store.session(id).await.unwrap().unwrap().ownership_epoch, epochs.1);
    env.assert_stale_append_fenced(id, env.target.id, params["target_epoch"].as_u64().unwrap()).await;

    let next = "Read the transferred scratch file after the successful retry";
    env.script(next, "cat retained.txt", "The retried move kept the scratch file.");
    env.message(id, next).await;
    let events = env.ended(id, 2).await;
    assert_tool_output(&events, moved.seq, "retry");
    assert_eq!(events.iter().filter(|e| matches!(e.kind, EventKind::PlaceChanged { .. })).count(), 1);
}

#[tokio::test]
async fn disconnecting_the_source_during_transfer_rolls_back_on_reattach() {
    let (gate, reached, release) = MoveGate::new("move.read", GateAction::FailAll);
    let mut env = Env::with_extensions(Some(gate), None).await;
    let first = "Prepare scratch before the source disconnects";
    env.script(first, "printf source > recovery.txt && cat recovery.txt", "The source file is ready.");
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "source");
    let places = env.placements(id).await;
    let command = env.request_move(id, worker_move(&places[0]["seq"], &env.target)).await;
    let params = within("the source chunk read to be intercepted", reached).await.unwrap();
    assert_eq!(params["move_id"], command);
    assert_eq!(params["offset"], 0);
    assert_eq!(env.move_state(&command).await.0, "released");
    env.assert_input_fenced(id).await;

    env.source.stop().await;
    drop(release);
    env.source.wait_disconnected(env.head()).await;
    eventually("rollback to wait for the disconnected source", async || {
        let (state, why) = env.move_state(&command).await;
        assert!(matches!(state.as_str(), "released" | "rolling_back"), "unexpected move state {state}: {why:?}");
        (state == "rolling_back").then_some(())
    })
    .await;
    assert_eq!(env.detail(id).await.session.worker_id, env.source.id);
    assert_eq!(env.placements(id).await, places);
    env.assert_input_fenced(id).await;

    env.source.restart().await;
    let (state, reason) = eventually("the reattached source to acknowledge rollback", async || env.move_outcome(&command).await).await;
    assert_eq!(state, "failed");
    assert!(reason.as_deref().is_some_and(|reason| reason.contains("disconnect")), "{reason:?}");
    assert_eq!(env.placements(id).await, places);
    let source = env.detail(id).await.session;
    assert_eq!(source.worker_id, env.source.id);
    let next = "Read the source file after rollback";
    env.script(next, "cat recovery.txt", "The original source is usable.");
    env.message(id, next).await;
    let events = env.ended(id, 2).await;
    assert_tool_output(&events, source.last_seq, "source");
    assert!(!events.iter().any(|e| matches!(e.kind, EventKind::PlaceChanged { .. })));
}

#[tokio::test]
async fn restarting_the_target_after_adoption_retries_activation_without_switching_back() {
    let (gate, reached, release) = MoveGate::new("move.activate", GateAction::FailAll);
    let mut env = Env::with_extensions(None, Some(gate)).await;
    let first = "Prepare scratch for an activation interrupted by a restart";
    let reply = "This conversation and note must survive the restart.";
    env.script(first, "printf adopted > recovery.txt && cat recovery.txt", reply);
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "adopted");
    let places = env.placements(id).await;
    let command = env.request_move(id, worker_move(&places[0]["seq"], &env.target)).await;
    let params = within("the first destination activation to be intercepted", reached).await.unwrap();
    assert_eq!(params["move_id"], command);
    assert_eq!(env.move_state(&command).await.0, "adopted");
    let adopted = env.detail(id).await.session;
    assert_eq!(adopted.worker_id, env.target.id);
    assert_eq!(env.placements(id).await[0]["seq"], adopted.last_seq);
    env.assert_input_fenced(id).await;
    env.assert_stale_append_fenced(id, env.source.id, 0).await;

    env.target.stop().await;
    drop(release);
    env.target.wait_disconnected(env.head()).await;
    assert_eq!(env.move_state(&command).await.0, "adopted");
    assert_eq!(env.detail(id).await.session.worker_id, env.target.id, "ownership must not roll back after adoption");
    env.assert_input_fenced(id).await;

    env.target.restart().await;
    let (target, moved) = env.wait_moved(id, &command, &env.target).await;
    assert_eq!(moved.seq, adopted.last_seq, "activation retries the adopted move rather than appending another move");
    assert_eq!(target.cwd, adopted.cwd);
    let next = "Read the adopted scratch file after target restart";
    env.script(next, "cat recovery.txt && printf destination > resumed.txt", "The restarted destination is usable.");
    env.message(id, next).await;
    let events = env.ended(id, 2).await;
    assert_tool_output(&events, moved.seq, "adopted");
    assert_eq!(events.iter().filter(|e| matches!(e.kind, EventKind::PlaceChanged { .. })).count(), 1);
    assert_eq!(std::fs::read_to_string(Path::new(&target.cwd).join("resumed.txt")).unwrap(), "destination");
    assert!(!env.source.state.join("scratch").join(id).join("resumed.txt").exists());
    assert!(env.mock.requests().iter().any(|request| {
        let history = request["messages"].to_string();
        history.contains(first) && history.contains(reply) && history.contains(next)
    }));
}

async fn resume_move_after_head_restart(method: &'static str, phase: &str, target_rpc: bool, already_adopted: bool) {
    let (gate, reached, release) = MoveGate::new(method, GateAction::FailFirst);
    let mut env = if target_rpc { Env::with_extensions(None, Some(gate)).await } else { Env::with_extensions(Some(gate), None).await };
    let first = "Write a note before the head restarts during the move";
    env.script(first, "printf durable > durable.txt && cat durable.txt", "The note is ready to move.");
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "durable");
    let places = env.placements(id).await;
    let command = env.request_move(id, worker_move(&places[0]["seq"], &env.target)).await;
    let params = within(method, reached).await.unwrap();
    assert_eq!(params["move_id"], command);
    assert_eq!(env.move_state(&command).await.0, phase);
    let at_restart = env.detail(id).await.session;
    assert_eq!(at_restart.worker_id, if already_adopted { env.target.id } else { env.source.id });
    let placements = env.placements(id).await;
    assert_eq!(placements.len(), if already_adopted { 2 } else { 1 });
    assert_eq!(placements.last(), places.last());
    env.assert_input_fenced(id).await;

    env.restart_head(release).await;
    let (target, moved) = env.wait_moved(id, &command, &env.target).await;
    if already_adopted {
        assert_eq!(moved.seq, at_restart.last_seq);
        assert_eq!(target.cwd, at_restart.cwd);
    }
    let next = "Read the note after the replacement head completes the move";
    env.script(next, "cat durable.txt", "The note survived recovery.");
    env.message(id, next).await;
    let events = env.ended(id, 2).await;
    assert_tool_output(&events, moved.seq, "durable");
    assert_eq!(events.iter().filter(|e| matches!(e.kind, EventKind::PlaceChanged { .. })).count(), 1);
}

#[tokio::test]
async fn head_restart_before_transfer_retries_the_same_move() {
    resume_move_after_head_restart("move.read", "released", false, false).await;
}

#[tokio::test]
async fn head_restart_before_adoption_retries_the_same_move() {
    resume_move_after_head_restart("move.prepare", "transferred", true, false).await;
}

#[tokio::test]
async fn head_restart_after_adoption_retries_activation_of_the_same_move() {
    resume_move_after_head_restart("move.activate", "adopted", true, true).await;
}

enum LostReply {
    Freeze,
    Prepare,
    Activate,
}

async fn replay_move_after_losing_successful_reply(lost: LostReply) {
    let mut env = Env::new().await;
    let (method, phase, rpc_worker) = match lost {
        LostReply::Freeze => ("move.freeze", "requested", &env.source),
        LostReply::Prepare => ("move.prepare", "transferred", &env.target),
        LostReply::Activate => ("move.activate", "adopted", &env.target),
    };
    let (reached, release) = LostReplyGate::install(method, rpc_worker);
    let first = "Write a note before recovering a lost move reply";
    env.script(first, "printf retained > replay.txt && cat replay.txt", "The replay fixture is ready.");
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    let opening = env.ended(id, 1).await;
    assert_tool_output(&opening, 0, "retained");
    let before = env.placements(id).await;
    let command = env.request_move(id, worker_move(&before[0]["seq"], &env.target)).await;

    let held = within(method, reached).await.unwrap();
    let reply = held.result.unwrap_or_else(|error| panic!("{method} did not execute successfully: {error}"));
    assert_eq!(held.params["move_id"], command);
    assert_eq!(held.params["source_epoch"], 0);
    assert_eq!(held.params["target_epoch"], 1);
    assert_eq!(env.move_state(&command).await.0, phase);
    let progress = env.move_progress(&command).await;
    let worker_fence = rpc_worker.worker.upgrade().unwrap().journal.ownership(id).await.unwrap();
    let (receipt, adopted_place) = match lost {
        LostReply::Freeze => {
            assert_eq!(worker_fence, (0, true, command.clone()));
            assert_eq!(reply["last_seq"], opening.last().unwrap().seq);
            assert_eq!(progress["freeze_started"], true);
            assert!(progress["last_seq"].is_null(), "the head has not seen the freeze watermark");
            env.assert_source_unchanged(id, &before, 0).await;
            (None, None)
        }
        LostReply::Prepare => {
            assert_eq!(worker_fence, (1, true, command.clone()));
            assert!(progress["event"].is_null(), "the head has not seen the prepared event");
            let receipt = reply["event"].as_str().filter(|event| !event.is_empty()).unwrap().to_owned();
            env.assert_source_unchanged(id, &before, 0).await;
            (Some(receipt), None)
        }
        LostReply::Activate => {
            assert_eq!(worker_fence, (1, false, command.clone()));
            let placements = env.placements(id).await;
            assert_eq!(placements.len(), 2);
            assert_eq!(placements.last(), before.last());
            assert_eq!(env.detail(id).await.session.worker_id, env.target.id);
            (Some(progress["event"].as_str().unwrap().to_owned()), Some(placements))
        }
    };
    env.assert_input_fenced(id).await;

    // Keep the successful response in the worker until the old head's waiter is gone.
    let head = env.head.take().unwrap();
    within("the head to stop without receiving the successful reply", head.shutdown()).await.unwrap();
    drop(release);
    env.reopen_head().await;

    let (target, moved) = env.wait_moved(id, &command, &env.target).await;
    let after = env.placements(id).await;
    assert_eq!(after.len(), 2, "replay must publish exactly one new place");
    assert_eq!(after.last(), before.last());
    if let Some(adopted_place) = adopted_place {
        assert_eq!(after, adopted_place, "activation replay must not change an already-adopted place");
    }
    if let Some(receipt) = receipt {
        assert_eq!(env.move_progress(&command).await["event"], receipt, "replay must use the original placement event");
    }
    let owner = env.head().state.store.session(id).await.unwrap().unwrap();
    assert_eq!((owner.worker_id, owner.ownership_epoch), (env.target.id.to_string(), 1));
    assert_eq!(std::fs::read_to_string(Path::new(&target.cwd).join("replay.txt")).unwrap(), "retained");

    let next = "Read the note after replaying the lost reply";
    env.script(next, "cat replay.txt", "The transferred file survived replay.");
    env.message(id, next).await;
    let events = env.ended(id, 2).await;
    assert_tool_output(&events, moved.seq, "retained");
    assert_eq!(events.iter().filter(|e| matches!(e.kind, EventKind::PlaceChanged { .. })).count(), 1);
}

#[tokio::test]
async fn head_replays_freeze_with_a_lost_successful_reply() {
    replay_move_after_losing_successful_reply(LostReply::Freeze).await;
}

#[tokio::test]
async fn head_replays_prepare_with_a_lost_successful_reply() {
    replay_move_after_losing_successful_reply(LostReply::Prepare).await;
}

#[tokio::test]
async fn head_replays_activate_with_a_lost_successful_reply() {
    replay_move_after_losing_successful_reply(LostReply::Activate).await;
}

enum PreAdoptionRestart {
    SourceAfterFreeze,
    TargetAfterPrepare,
}

async fn worker_restart_after_losing_successful_reply(restart: PreAdoptionRestart) {
    let mut env = Env::new().await;
    let (method, phase, worker) = match restart {
        PreAdoptionRestart::SourceAfterFreeze => ("move.freeze", "requested", &env.source),
        PreAdoptionRestart::TargetAfterPrepare => ("move.prepare", "transferred", &env.target),
    };
    let (reached, release) = LostReplyGate::install(method, worker);
    let first = "Write a note before restarting a worker with a move reply held";
    env.script(first, "printf retained > rollback.txt && cat rollback.txt", "The source note is ready.");
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    let opening = env.ended(id, 1).await;
    assert_tool_output(&opening, 0, "retained");
    let last = opening.last().unwrap().seq;
    let before = env.placements(id).await;
    let command = env.request_move(id, worker_move(&before[0]["seq"], &env.target)).await;

    let held = within(method, reached).await.unwrap();
    let reply = held.result.unwrap_or_else(|error| panic!("{method} did not execute successfully: {error}"));
    assert_eq!(held.params["move_id"], command);
    assert_eq!(held.params["source_epoch"], 0);
    assert_eq!(held.params["target_epoch"], 1);
    assert_eq!(env.move_state(&command).await.0, phase);
    let progress = env.move_progress(&command).await;
    assert_eq!(progress["freeze_started"], true);
    assert!(progress["event"].is_null());
    let source_restarts = matches!(restart, PreAdoptionRestart::SourceAfterFreeze);
    let prepared_events = {
        let worker = worker.worker.upgrade().unwrap();
        let fence = worker.journal.ownership(id).await.unwrap();
        if source_restarts {
            assert_eq!(fence, (0, true, command.clone()));
            assert_eq!(reply["last_seq"], last);
            assert!(progress["last_seq"].is_null(), "the head has not received the source watermark");
            None
        } else {
            assert_eq!(fence, (1, true, command.clone()));
            assert!(reply["event"].as_str().is_some_and(|event| !event.is_empty()));
            let events = worker.journal.events_after(id, last, 2).await.unwrap();
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0].kind, Some(session_event::Kind::PlaceChanged(place)) if place.command_id == command));
            Some(events)
        }
    };
    env.assert_source_unchanged(id, &before, 0).await;
    env.assert_input_fenced(id).await;

    // Runtime shutdown drops the response task before the reply is released. The head keeps running.
    if source_restarts {
        env.source.stop().await;
        drop(release);
        env.source.wait_disconnected(env.head()).await;
        eventually("rollback to wait for the source's abort acknowledgement", async || {
            (env.move_state(&command).await.0 == "rolling_back").then_some(())
        })
        .await;
        env.assert_input_fenced(id).await;
        env.source.restart().await;
        env.assert_move_failed(&command, "disconnect").await;
        let source = env.source.worker.upgrade().unwrap();
        assert_eq!(source.journal.ownership(id).await.unwrap(), (0, false, command.clone()));
        assert!(env.move_progress(&command).await["last_seq"].is_null());
    } else {
        env.target.stop().await;
        drop(release);
        env.target.wait_disconnected(env.head()).await;
        env.assert_move_failed(&command, "disconnect").await;
        assert_eq!(env.move_progress(&command).await["target_clean"], false);
        env.assert_source_unchanged(id, &before, 0).await;
        env.target.restart().await;
        eventually("the restarted target to acknowledge abort cleanup", async || {
            (env.move_progress(&command).await["target_clean"] == true).then_some(())
        })
        .await;
        let target = env.target.worker.upgrade().unwrap();
        assert_eq!(target.journal.ownership(id).await.unwrap(), (1, true, command.clone()));
        assert_eq!(target.journal.events_after(id, last, 2).await.unwrap(), prepared_events.unwrap());
        env.assert_stale_append_fenced(id, env.target.id, 1).await;
    }

    assert!(env.move_progress(&command).await["event"].is_null(), "rollback must not record a prepared placement at the head");
    env.assert_source_unchanged(id, &before, 0).await;
    let next = "Read the original source note after the worker restart and rollback";
    env.script(next, "cat rollback.txt", "The original note survived rollback.");
    env.message(id, next).await;
    assert_tool_output(&env.ended(id, 2).await, last, "retained");
    env.assert_source_unchanged(id, &before, 0).await;
}

#[tokio::test]
async fn source_restart_after_successful_freeze_with_lost_reply_rolls_back() {
    worker_restart_after_losing_successful_reply(PreAdoptionRestart::SourceAfterFreeze).await;
}

#[tokio::test]
async fn target_restart_after_successful_prepare_with_lost_reply_stays_fenced() {
    worker_restart_after_losing_successful_reply(PreAdoptionRestart::TargetAfterPrepare).await;
}

#[tokio::test]
async fn rollback_of_an_unanswered_freeze_recovers_an_evicted_source_cache() {
    let (gate, reached, release) = MoveGate::new("move.freeze", GateAction::FailAll);
    let mut env = Env::with_extensions(Some(gate), None).await;
    let first = "Write a note before evicting the source journal";
    let reply = "The source note is ready for later restoration.";
    env.script(first, "printf retained > restore.txt && cat restore.txt", reply);
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    let opening = env.ended(id, 1).await;
    assert_tool_output(&opening, 0, "retained");
    let before = env.placements(id).await;
    let command = env.request_move(id, worker_move(&before[0]["seq"], &env.target)).await;

    let freeze = within("the first freeze to be held before worker dispatch", reached).await.unwrap();
    assert_eq!(freeze["move_id"], command);
    assert_eq!(freeze["source_epoch"], 0);
    assert_eq!(env.move_state(&command).await.0, "requested");
    let progress = env.move_progress(&command).await;
    assert_eq!(progress["freeze_started"], true);
    assert!(progress["last_seq"].is_null());
    {
        let worker = env.source.worker.upgrade().unwrap();
        assert_eq!(worker.journal.ownership(id).await.unwrap(), (0, false, String::new()));
        assert!(!worker.journal.move_aborted(id, 0, &command).await.unwrap());
    }

    env.source.stop().await;
    drop(release);
    env.source.wait_disconnected(env.head()).await;
    eventually("rollback to wait for the source worker", async || (env.move_state(&command).await.0 == "rolling_back").then_some(())).await;
    env.assert_input_fenced(id).await;

    let last = opening.last().unwrap().seq;
    assert_eq!(env.head().state.store.last_seq(id).await.unwrap(), last);
    {
        let journal = c6_worker::journal::Journal::open(&env.source.state.join("journal.db")).await.unwrap();
        assert_eq!(journal.events_after(id, 0, u32::MAX).await.unwrap().last().unwrap().seq, last);
        journal.set_acked(id, last).await.unwrap();
        assert_eq!(journal.trim(i64::MAX, &[]).await.unwrap(), 1);
        assert!(!journal.has_session(id).await.unwrap());
    }

    env.source.restart().await;
    env.assert_move_failed(&command, "disconnect").await;
    env.assert_source_unchanged(id, &before, 0).await;
    let worker = env.source.worker.upgrade().unwrap();
    assert!(!worker.journal.has_session(id).await.unwrap(), "abort must complete even without local history");
    assert!(worker.journal.move_aborted(id, 0, &command).await.unwrap(), "abort must durably record this exact attempt");
    assert!(!worker.journal.move_aborted(id, 0, &Uuid::nil().to_string()).await.unwrap());
    worker.journal.check_ownership(id, 0).await.unwrap();

    let next = "Read the original source note and recover the conversation from the head";
    env.script(next, "cat restore.txt", "The restored session kept the note.");
    env.message(id, next).await;
    assert_tool_output(&env.ended(id, 2).await, last, "retained");
    assert!(worker.journal.has_session(id).await.unwrap());
    assert!(env.mock.requests().iter().any(|request| {
        let history = request["messages"].to_string();
        history.contains(first) && history.contains(reply) && history.contains(next)
    }));

    // Resend the exact held RPC after rehydration, when it could otherwise fence the now-cached source.
    let error = rpc::handle(&worker, "move.freeze", freeze).await.unwrap_err();
    assert!(format!("{error:#}").contains("this move was aborted"), "{error:#}");
    worker.journal.check_ownership(id, 0).await.unwrap();
    env.assert_source_unchanged(id, &before, 0).await;
}

#[tokio::test]
async fn moved_session_recovers_an_interrupted_turn_after_the_new_owner_restarts() {
    let mut env = Env::new().await;
    let first = "Write a note to read after the moved worker recovers";
    let opening = "Recovery starts after this completed opening.";
    env.script(first, "printf survives > recovery.txt && cat recovery.txt", opening);
    let started = env.start(&env.source, None, first).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "survives");
    let places = env.placements(id).await;
    let (_, moved) = env.moved(id, worker_move(&places[0]["seq"], &env.target), &env.target).await;
    let epoch = env.head().state.store.session(id).await.unwrap().unwrap().ownership_epoch;
    assert!(epoch > 0, "exercise worker recovery at a moved incarnation");

    let interrupted = "A continuation to interrupt on the positive-epoch owner";
    let old_reply = env.mock.push_gated_about(interrupted, Reply::Text("The interrupted response must not end the turn.".into()));
    env.message(id, interrupted).await;
    let turn = eventually("the destination's continued turn to be durably running at the model barrier", async || {
        let detail = env.detail(id).await;
        let called = env.mock.requests().iter().any(|request| request["messages"].to_string().contains(interrupted));
        if !called || detail.session.status != SessionStatus::Running {
            return None;
        }
        assert_eq!(detail.session.worker_id, env.target.id);
        env.events(id).await.iter().find_map(|e| match &e.kind {
            EventKind::TurnStarted { turn, .. } if e.seq > moved.seq => Some(*turn),
            _ => None,
        })
    })
    .await;

    let resume_notice = "worker restarted while you were working";
    let resumed_reply = "The recovered turn read the moved file.";
    env.script(resume_notice, "cat recovery.txt", resumed_reply);
    env.target.stop().await;
    old_reply.notify_one();
    env.target.wait_disconnected(env.head()).await;
    env.target.restart().await;

    let (closed_seq, events) = eventually_or(
        "the interrupted turn to close and resume on the restarted destination",
        async || {
            let detail = env.detail(id).await;
            let events = env.events(id).await;
            let closed = events
                .iter()
                .find(|e| matches!(&e.kind, EventKind::TurnEnded { turn: n, reason: TurnEndReason::WorkerRestarted, .. } if *n == turn))?;
            let finished = events
                .iter()
                .any(|e| e.seq > closed.seq && matches!(&e.kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. }));
            (finished && detail.session.status == SessionStatus::Idle && detail.session.worker_id == env.target.id)
                .then_some((closed.seq, events))
        },
        || env.mock.log(),
    )
    .await;
    assert_tool_output(&events, closed_seq, "survives");
    assert_eq!(env.head().state.store.session(id).await.unwrap().unwrap().ownership_epoch, epoch);
    assert!(events.iter().any(|e| e.seq > closed_seq
        && matches!(&e.kind, EventKind::Assistant { blocks, .. }
        if blocks.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == resumed_reply)))));
    assert_eq!(events.iter().filter(|e| matches!(e.kind, EventKind::PlaceChanged { .. })).count(), 1);
    assert!(env.mock.requests().iter().any(|request| {
        let history = request["messages"].to_string();
        history.contains(first) && history.contains(opening) && history.contains(interrupted) && history.contains(resume_notice)
    }));
}

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Move Test",
            "-c",
            "user.email=move@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} in {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn repositories(env: &Env) -> (PathBuf, PathBuf, PathBuf) {
    let root = env.root.path().canonicalize().unwrap();
    let origin = root.join("origin");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("tracked.txt"), "base\n").unwrap();
    git(&origin, &["add", "tracked.txt"]);
    git(&origin, &["commit", "-qm", "Base"]);
    let source = env.source.state.join("checkout");
    let target = env.target.state.join("checkout");
    for checkout in [&source, &target] {
        git(&root, &["clone", "-q", "--no-local", origin.to_str().unwrap(), checkout.to_str().unwrap()]);
        // All objects come from the local fixture. Matching deliberately uses
        // a network-shaped identity, not a local path or a reachable host.
        git(checkout, &["remote", "set-url", "origin", "https://example.invalid/move-fixture/checkout.git"]);
        git(checkout, &["remote", "set-url", "--push", "origin", origin.to_str().unwrap()]);
    }
    git(&target, &["remote", "set-url", "origin", "git@EXAMPLE.invalid:move-fixture/checkout"]);
    (origin, source, target)
}

async fn register_checkout(env: &Env, checkout: &Path) {
    // Publish a known place on the target, rather than relying on the two
    // workers sharing a physical filesystem in the test.
    let prompt = "Register the target repository checkout";
    env.mock.push_about(prompt, Reply::Text("This checkout is known.".into()));
    let seed = env.start(&env.target, Some(checkout), prompt).await;
    env.ended(&seed.id, 1).await;
}

#[tokio::test]
async fn repo_move_transfers_unpushed_commit_and_preserves_index_and_worktree() {
    let env = Env::new().await;
    let (origin, source, target) = repositories(&env);
    register_checkout(&env, &target).await;
    git(&source, &["switch", "-qc", "move-test-topic"]);
    std::fs::write(source.join("committed.txt"), "local commit\n").unwrap();
    git(&source, &["add", "committed.txt"]);
    git(&source, &["commit", "-qm", "An unpushed commit"]);
    let commit = git(&source, &["rev-parse", "HEAD"]);
    let prompt = "Prepare both staged and unstaged changes before moving the repository";
    env.script(prompt,
        "printf 'staged\n' > tracked.txt && git add tracked.txt && printf 'unstaged\n' > tracked.txt && printf 'untracked\n' > draft.txt && cat tracked.txt",
        "The index and worktree have different changes.");
    let started = env.start(&env.source, Some(&source), prompt).await;
    let id = &started.id;
    assert_tool_output(&env.ended(id, 1).await, 0, "unstaged");

    let before = env.placements(id).await;
    let (after, moved) = env.moved(id, worker_move(&before[0]["seq"], &env.target), &env.target).await;
    let EventKind::PlaceChanged { uncommitted_files, .. } = &moved.kind else { unreachable!() };
    assert_eq!(*uncommitted_files, 2, "the staged/unstaged path counts once, plus the untracked file");
    let destination = Path::new(&after.cwd);
    assert_ne!(destination, source, "must not reuse the source checkout");
    for checkout in [source.as_path(), destination] {
        assert_eq!(git(checkout, &["rev-parse", "HEAD"]), commit);
        assert_eq!(git(checkout, &["branch", "--show-current"]).trim(), "move-test-topic");
        assert_eq!(git(checkout, &["status", "--porcelain=v1", "--untracked-files=all"]), "MM tracked.txt\n?? draft.txt\n");
        assert_eq!(git(checkout, &["show", ":tracked.txt"]), "staged\n");
        assert_eq!(std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(), "unstaged\n");
        assert_eq!(std::fs::read_to_string(checkout.join("draft.txt")).unwrap(), "untracked\n");
        assert_eq!(std::fs::read_to_string(checkout.join("committed.txt")).unwrap(), "local commit\n");
    }
    assert!(git(&origin, &["for-each-ref", "refs/heads/move-test-topic"]).is_empty(), "move must not push to origin");
    if destination != target {
        assert_eq!(git(&target, &["branch", "--show-current"]).trim(), "main");
        assert!(git(&target, &["status", "--porcelain=v1"]).is_empty(), "the target's original checkout changed");
    }

    let next = "Read the local commit and the untracked change in the moved repository";
    env.script(next, "cat committed.txt draft.txt", "Both files survived the move.");
    env.message(id, next).await;
    assert_tool_output(&env.ended(id, 2).await, moved.seq, "local commit\nuntracked");
}

#[tokio::test]
async fn repo_move_without_a_matching_destination_leaves_source_authoritative() {
    let env = Env::new().await;
    let (_origin, source, target) = repositories(&env);
    // Same folder name and commit, but a different remote identity.
    git(&target, &["remote", "set-url", "origin", "https://example.invalid/unrelated/checkout.git"]);
    register_checkout(&env, &target).await;
    let prompt = "Write a file in the repository that has no target match";
    env.script(prompt, "printf source > local.txt && cat local.txt", "This stays on the source.");
    let started = env.start(&env.source, Some(&source), prompt).await;
    let id = &started.id;
    let events = env.ended(id, 1).await;
    let before = env.placements(id).await;
    let (status, text) = env.post(&format!("sessions/{id}/placement"), worker_move(&before[0]["seq"], &env.target)).await;
    let refusal = if status == StatusCode::ACCEPTED {
        let receipt: Value = serde_json::from_str(&text).unwrap();
        let command = receipt_id(&receipt);
        eventually("the unmatched repository move to be refused", async || {
            let detail = env.detail(id).await;
            assert_eq!(detail.session.worker_id, env.source.id, "a failed move changed the owner");
            let (state, error) = env.move_outcome(command).await?;
            assert_eq!(state, "failed");
            Some(error.expect("a failed move should explain why"))
        })
        .await
    } else {
        assert!(status.is_client_error(), "expected a refusal, got {status}: {text}");
        text
    };
    let reason = refusal.to_ascii_lowercase();
    assert!(reason.contains("repo") && reason.contains("match"), "{refusal}");
    assert_eq!(env.placements(id).await, before);
    let detail = env.detail(id).await;
    assert_eq!(detail.session.worker_id, env.source.id);
    assert_eq!(Path::new(&detail.session.cwd), source);
    assert_eq!(std::fs::read_to_string(source.join("local.txt")).unwrap(), "source");
    assert!(git(&target, &["status", "--porcelain=v1"]).is_empty(), "refusal changed the unrelated checkout");

    let next = "Read the local source file after the refused move";
    env.script(next, "cat local.txt", "The source is still usable.");
    env.message(id, next).await;
    let resumed = env.ended(id, 2).await;
    assert_tool_output(&resumed, events.last().unwrap().seq, "source");
    assert!(!resumed.iter().any(|e| matches!(e.kind, EventKind::PlaceChanged { .. })));
}
