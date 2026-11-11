//! Single-message withdrawal by command id. These workers are scripted
//! registry connections, not processes; no model or external service runs.
//! A separate native worker uses only a loopback MockLlm. A durable
//! UserMessage is already worker-owned, even with stale pending metadata;
//! only unrecorded, inbox-owned queued inputs can be taken back.

use c6_core::{WorkerId, now_ms};
use c6_head::store::{NewCommand, NewWorker, SessionRow};
use c6_head::{Head, HeadConfig};
use c6_proto::{RpcRequest, RpcResponse, SessionCommand, SessionEvent, UserInput, UserMessage, head_msg, session_command, session_event};
use c6_testkit::within;
use prost::Message;
use reqwest::StatusCode;
use serde_json::{Value, json};

struct Fixture {
    head: Head,
    dir: tempfile::TempDir,
    worker: WorkerId,
    http: reqwest::Client,
}

fn config(dir: &std::path::Path) -> HeadConfig {
    HeadConfig {
        data_dir: dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    }
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let head = c6_head::start(config(dir.path())).await.unwrap();
        let worker = WorkerId::new();
        head.state
            .store
            .insert_worker(
                &NewWorker {
                    id: worker,
                    name: "withdrawal-fake",
                    hostname: "withdrawal-fake",
                    os: "linux",
                    arch: "x86_64",
                    version: "0",
                    cert_serial: "withdrawal-fake",
                },
                "",
                0,
            )
            .await
            .unwrap();
        Self { head, dir, worker, http: reqwest::Client::new() }
    }

    async fn restart(self) -> Self {
        let Self { head, dir, worker, http } = self;
        head.shutdown().await.unwrap();
        let head = c6_head::start(config(dir.path())).await.unwrap();
        Self { head, dir, worker, http }
    }

    fn api(&self) -> String {
        format!("http://127.0.0.1:{}/api/v1", self.head.ui_addrs[0].port())
    }

    async fn session(&self, id: &str) {
        let row = SessionRow {
            id: id.into(),
            worker_id: self.worker.to_string(),
            title: "withdrawal fixture".into(),
            cwd: String::new(),
            connection_id: String::new(),
            model: "unused".into(),
            status: "idle".into(),
            error: None,
            effort: None,
            parent_id: None,
            parent_call_id: None,
            fork_seq: None,
            created_at: now_ms(),
            updated_at: now_ms(),
            last_seq: 0,
            archived: 0,
            labels: "[]".into(),
            worker_name: "withdrawal-fake".into(),
            repo: String::new(),
            folder: String::new(),
            home: String::new(),
            ssh_target: String::new(),
            awaiting: 0,
            subagent_model: String::new(),
            subagent_connection_id: String::new(),
            subagent_effort: String::new(),
            tools: "[]".into(),
            realm: "home".into(),
            agent: String::new(),
            end_reason: String::new(),
            root_id: None,
            observed: false,
            instance: String::new(),
            colour: String::new(),
            remote_template: String::new(),
        };
        self.head.state.store.create_session(&row, &[], false).await.unwrap();
        self.record(id, session_event::Kind::Started(Default::default())).await;
    }

    async fn record(&self, session: &str, kind: session_event::Kind) -> u64 {
        let seq = self.head.state.store.session(session).await.unwrap().unwrap().last_seq as u64 + 1;
        self.head
            .state
            .store
            .ingest(self.worker, session, &[SessionEvent { seq, ts_ms: now_ms(), kind: Some(kind) }])
            .await
            .unwrap()
            .unwrap();
        seq
    }

    async fn queue(&self, session: &str, command: &str) {
        let data = SessionCommand {
            command_id: command.into(),
            session_id: session.into(),
            kind: Some(session_command::Kind::Input(UserInput { text: "same text".into(), ..Default::default() })),
        }
        .encode_to_vec();
        self.head
            .state
            .store
            .enqueue_command(&NewCommand { id: command, session_id: session, worker_id: self.worker, data, text: Some("same text") })
            .await
            .unwrap();
    }

    async fn take_back(&self, session: &str, command: &str) -> reqwest::Response {
        self.http.post(format!("{}/sessions/{session}/messages/{command}/take-back", self.api())).send().await.unwrap()
    }

    async fn queued(&self, session: &str) -> Vec<String> {
        self.head.state.store.queued_inputs(session).await.unwrap().into_iter().map(|c| c.id).collect()
    }

    async fn events(&self, session: &str) -> Vec<SessionEvent> {
        self.head.state.store.events_after(session, 0, i64::MAX).await.unwrap()
    }

    async fn command_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM commands").fetch_one(self.head.state.store.pool()).await.unwrap()
    }
}

fn user(command: &str) -> session_event::Kind {
    session_event::Kind::User(UserMessage { command_id: command.into(), text: "same text".into(), ..Default::default() })
}

type Inbox = tokio::sync::mpsc::Receiver<Result<c6_proto::HeadMsg, tonic::Status>>;

fn attach<'a>(f: &'a Fixture, capabilities: &[&str]) -> (c6_head::registry::ConnGuard<'a>, Inbox) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    (f.head.state.registry.attach(f.worker, now_ms(), tx, capabilities.iter().map(|s| (*s).into()).collect(), "worker"), rx)
}

async fn cancellation(rx: &mut Inbox, session: &str, command: &str) -> RpcRequest {
    let msg = within("one cancellation request", rx.recv()).await.unwrap().unwrap();
    let Some(head_msg::Msg::RpcRequest(req)) = msg.msg else { panic!("only a queue RPC, never rewind or resend") };
    assert_eq!(req.method, "session.queue_op");
    assert_eq!(
        serde_json::from_slice::<Value>(&req.params_json).unwrap(),
        json!({"session_id": session, "command_id": command, "op": "cancel"})
    );
    req
}

fn answer(guard: &c6_head::registry::ConnGuard<'_>, req: RpcRequest, state: &str) {
    guard.rpc_response(RpcResponse {
        id: req.id,
        result_json: serde_json::to_vec(&json!({"state": state})).unwrap(),
        error: String::new(),
    });
}

async fn error(response: reqwest::Response, status: StatusCode) -> String {
    assert_eq!(response.status(), status);
    response.json::<Value>().await.unwrap()["error"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn confirmed_take_back_uses_durable_identity_and_cancels_only_that_message() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "earlier").await;
    f.queue("s", "target").await;
    f.queue("s", "later").await;
    let before = f.events("s").await;
    let f = f.restart().await;
    assert_eq!(f.events("s").await, before);
    assert_eq!(f.queued("s").await, ["earlier", "target", "later"], "command identity survives a head restart");
    let (guard, mut rx) = attach(&f, &["queue_ops"]);
    let (response, ()) = within("take-back confirmation", async {
        tokio::join!(f.take_back("s", "target"), async {
            answer(&guard, cancellation(&mut rx, "s", "target").await, "cancelled");
        })
    })
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.bytes().await.unwrap().is_empty());
    assert_eq!(f.queued("s").await, ["earlier", "later"], "same-text neighbours are not canceled");
    assert_eq!(f.events("s").await, before, "no rewind or transcript rewrite");
    assert_eq!(f.command_count().await, 3, "no rewind or automatic resend is enqueued");
    assert_eq!(error(f.take_back("s", "target").await, StatusCode::CONFLICT).await, "The agent has this message");
    assert!(rx.try_recv().is_err(), "no later input is canceled, even on retry");
    drop(guard);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn take_back_refuses_taken_in_flight_and_unconfirmed_worker_ownership() {
    for state in ["in_flight", "delivered", "pending"] {
        let f = Fixture::new().await;
        f.session("s").await;
        f.queue("s", "target").await;
        f.queue("s", "later").await;
        let before = f.events("s").await;
        let (guard, mut rx) = attach(&f, &["queue_ops"]);
        let (response, ()) = within("worker ownership answer", async {
            tokio::join!(f.take_back("s", "target"), async {
                answer(&guard, cancellation(&mut rx, "s", "target").await, state);
            })
        })
        .await;
        assert_eq!(error(response, StatusCode::CONFLICT).await, "The agent has this message");
        assert_eq!(f.queued("s").await, ["target", "later"]);
        assert_eq!(f.events("s").await, before);
        assert_eq!(f.command_count().await, 2);
        assert!(rx.try_recv().is_err());
        drop(guard);
        f.head.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn cancel_and_pull_refuse_pending_without_retiring_target_or_asking_about_later_input() {
    for pull in [false, true] {
        let f = Fixture::new().await;
        f.session("s").await;
        f.queue("s", "target").await;
        f.queue("s", "later").await;
        let before = f.events("s").await;
        let (guard, mut rx) = attach(&f, &["queue_ops"]);
        let request = if pull {
            f.http.post(format!("{}/sessions/s/queue/target/pull", f.api()))
        } else {
            f.http.delete(format!("{}/sessions/s/queue/target", f.api()))
        };
        let response = within("pending queue cancellation", async {
            let response = request.send();
            tokio::pin!(response);
            let req = tokio::select! {
                req = cancellation(&mut rx, "s", "target") => req,
                response = &mut response => panic!("request ended before worker confirmation: {response:?}"),
            };
            answer(&guard, req, "pending");
            // A tail pull must stop at its unconfirmed anchor. Fail
            // immediately if it asks to cancel a later message.
            tokio::select! {
                response = &mut response => response.unwrap(),
                msg = rx.recv() => panic!("no later cancellation RPC is allowed: {msg:?}"),
            }
        })
        .await;
        assert_eq!(error(response, StatusCode::CONFLICT).await, "The agent has this message");
        assert_eq!(f.queued("s").await, ["target", "later"]);
        assert_eq!(f.events("s").await, before);
        assert_eq!(f.command_count().await, 2);
        assert!(rx.try_recv().is_err(), "neither operation cancels any later input");
        drop(guard);
        f.head.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn take_back_does_not_claim_success_when_delivery_wins_the_confirmation_race() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "target").await;
    f.queue("s", "later").await;
    let (guard, mut rx) = attach(&f, &["queue_ops"]);
    let (response, ()) = within("delivery versus cancellation", async {
        tokio::join!(f.take_back("s", "target"), async {
            let req = cancellation(&mut rx, "s", "target").await;
            // A durable delivery arrives after the initial pending lookup
            // but before the cancellation reply. No sleeps or scheduling
            // assumptions are needed to exercise the head's final guard.
            f.record("s", user("target")).await;
            // Even stale pending metadata cannot override the durable
            // User event or the session highwater advancing during RPC.
            sqlx::query("UPDATE commands SET state = 'pending' WHERE id = 'target'").execute(f.head.state.store.pool()).await.unwrap();
            answer(&guard, req, "cancelled");
        })
    })
    .await;
    assert_eq!(error(response, StatusCode::CONFLICT).await, "The agent has this message");
    assert_eq!(f.queued("s").await, ["target", "later"], "refusal does not mutate even stale pending metadata");
    assert_eq!(f.command_count().await, 2);
    assert!(f.events("s").await.iter().all(|e| !matches!(e.kind, Some(session_event::Kind::Rewound(_)))));
    assert!(rx.try_recv().is_err());
    drop(guard);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn take_back_refuses_offline_old_and_disconnected_workers_without_retiring_input() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "target").await;
    f.queue("s", "later").await;
    let before = f.events("s").await;
    assert!(error(f.take_back("s", "target").await, StatusCode::SERVICE_UNAVAILABLE).await.contains("worker is not connected"));
    {
        let (_guard, mut rx) = attach(&f, &[]);
        assert!(error(f.take_back("s", "target").await, StatusCode::CONFLICT).await.contains("update it"));
        assert!(rx.try_recv().is_err(), "an old worker is never asked an unsupported method");
    }
    {
        let (guard, mut rx) = attach(&f, &["queue_ops"]);
        let (response, ()) = within("disconnect during cancellation", async {
            tokio::join!(f.take_back("s", "target"), async {
                cancellation(&mut rx, "s", "target").await;
                drop(guard);
            })
        })
        .await;
        assert!(error(response, StatusCode::SERVICE_UNAVAILABLE).await.contains("the worker could not be asked"));
    }
    assert_eq!(f.queued("s").await, ["target", "later"]);
    assert_eq!(f.events("s").await, before);
    assert_eq!(f.command_count().await, 2);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn take_back_accepts_cancellation_across_replacement_without_a_cross_restart_delivery_guarantee() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "target").await;
    f.queue("s", "later").await;
    let before = f.events("s").await;
    let (guard, mut rx) = attach(&f, &["queue_ops"]);
    let mut replacement = None;
    let mut selected = None;
    let (response, ()) = within("worker replacement during cancellation", async {
        tokio::join!(f.take_back("s", "target"), async {
            let req = cancellation(&mut rx, "s", "target").await;
            // Script the inherited restart gap: a replacement delivery
            // loop selects the row before retirement. Inbox.seen only
            // protects the process that actually answered cancellation.
            replacement = Some(attach(&f, &["queue_ops"]));
            selected = f.head.state.store.pending_commands(f.worker, 0).await.unwrap().into_iter().find(|row| row.id == "target");
            assert!(selected.is_some(), "replacement selected target while it was still pending");
            answer(&guard, req, "cancelled");
        })
    })
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "no connection-generation fence is part of take-back");
    assert!(response.bytes().await.unwrap().is_empty());
    assert_eq!(f.queued("s").await, ["later"]);
    assert_eq!(f.events("s").await, before);
    assert_eq!(f.command_count().await, 2);
    assert!(rx.try_recv().is_err());
    let (new_guard, mut new_rx) = replacement.unwrap();
    assert!(new_rx.try_recv().is_err(), "no cancellation or resend is guessed for the replacement worker");
    // No real replacement actor runs in this fixture: ingest the User
    // from that preselected snapshot explicitly, after the successful
    // response. Retiring the head row cannot retract the snapshot or
    // carry the old process's seen set into a replacement process.
    let command = SessionCommand::decode(selected.unwrap().data.as_slice()).unwrap();
    assert_eq!(command.session_id, "s");
    assert!(matches!(command.kind, Some(session_command::Kind::Input(_))));
    let seq = f.record("s", user(&command.command_id)).await;
    assert!(f.events("s").await.iter().any(|e| e.seq == seq && e.kind == Some(user("target"))));
    assert_eq!(f.queued("s").await, ["later"], "the inherited delivery gap never cancels later input");
    assert_eq!(f.command_count().await, 2, "no retry, rewind or replacement is enqueued");
    drop(new_guard);
    drop(guard);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn take_back_validates_same_session_and_input_command_identity() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.session("other").await;
    f.queue("other", "foreign").await;
    f.queue("s", "later").await;
    f.head
        .state
        .store
        .enqueue_command(&NewCommand {
            id: "not-an-input",
            session_id: "s",
            worker_id: f.worker,
            data: SessionCommand {
                command_id: "not-an-input".into(),
                session_id: "s".into(),
                kind: Some(session_command::Kind::Interrupt(Default::default())),
            }
            .encode_to_vec(),
            text: None,
        })
        .await
        .unwrap();
    let (guard, mut rx) = attach(&f, &["queue_ops"]);
    for invalid in ["foreign", "not-an-input", "unknown", "1"] {
        assert_eq!(error(f.take_back("s", invalid).await, StatusCode::CONFLICT).await, "The agent has this message");
    }
    assert_eq!(error(f.take_back("missing", "foreign").await, StatusCode::NOT_FOUND).await, "no such session");
    assert_eq!(f.queued("s").await, ["later"]);
    assert_eq!(f.queued("other").await, ["foreign"]);
    assert!(rx.try_recv().is_err(), "invalid identity never reaches a worker");
    drop(guard);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn normal_transcript_ingestion_is_already_taken_even_with_later_pending_input() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "taken").await;
    f.record("s", user("taken")).await;
    f.queue("s", "later").await;
    let before = f.events("s").await;
    let (guard, mut rx) = attach(&f, &["queue_ops"]);
    assert_eq!(error(f.take_back("s", "taken").await, StatusCode::CONFLICT).await, "The agent has this message");
    assert_eq!(f.queued("s").await, ["later"]);
    assert_eq!(f.events("s").await, before);
    assert_eq!(f.command_count().await, 2);
    assert!(rx.try_recv().is_err(), "head already knows this command is taken");
    drop(guard);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn recorded_input_is_taken_even_with_stale_pending_metadata_or_a_blocking_hook() {
    for blocked in [false, true] {
        let f = Fixture::new().await;
        f.session("s").await;
        // The floor is captured at enqueue, before the worker can append
        // this command's User event. Re-queueing an already logged User
        // would fabricate a chronology the bounded scan need not cover.
        f.queue("s", "taken").await;
        f.record(
            "s",
            session_event::Kind::User(UserMessage {
                command_id: "taken".into(),
                text: "same text".into(),
                blocked: if blocked { "a hook blocked this input".into() } else { String::new() },
                ..Default::default()
            }),
        )
        .await;
        // Intentionally contradictory metadata tests refusal, never
        // success: a persisted User event is already worker-owned.
        sqlx::query("UPDATE commands SET state = 'pending' WHERE id = 'taken'").execute(f.head.state.store.pool()).await.unwrap();
        f.queue("s", "later").await;
        let f = f.restart().await;
        sqlx::query("DELETE FROM activity WHERE session_id = 's'").execute(f.head.state.store.pool()).await.unwrap();
        let before = f.events("s").await;
        let (guard, mut rx) = attach(&f, &["queue_ops"]);
        assert_eq!(error(f.take_back("s", "taken").await, StatusCode::CONFLICT).await, "The agent has this message");
        assert_eq!(f.queued("s").await, ["taken", "later"]);
        assert_eq!(f.events("s").await, before);
        assert_eq!(f.command_count().await, 2);
        assert!(rx.try_recv().is_err(), "durable ownership is checked before any worker RPC, without an analytics index");
        drop(guard);
        f.head.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn take_back_refuses_a_lost_pending_retirement_race() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "target").await;
    f.queue("s", "later").await;
    let before = f.events("s").await;
    let (guard, mut rx) = attach(&f, &["queue_ops"]);
    let (response, ()) = within("another queue operation wins retirement", async {
        tokio::join!(f.take_back("s", "target"), async {
            let req = cancellation(&mut rx, "s", "target").await;
            assert!(f.head.state.store.cancel_pending_command("target").await.unwrap());
            answer(&guard, req, "cancelled");
        })
    })
    .await;
    assert_eq!(error(response, StatusCode::CONFLICT).await, "The agent has this message");
    assert_eq!(f.queued("s").await, ["later"]);
    assert_eq!(f.events("s").await, before);
    assert_eq!(f.command_count().await, 2);
    assert!(rx.try_recv().is_err());
    drop(guard);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn take_back_refuses_the_head_assistant_without_claiming_inbox_cancellation() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "target").await;
    f.queue("s", "later").await;
    let head_worker = f.head.state.assistant.head_id().unwrap();
    sqlx::query("UPDATE sessions SET worker_id = ? WHERE id = 's'")
        .bind(head_worker.to_string())
        .execute(f.head.state.store.pool())
        .await
        .unwrap();
    let before = f.events("s").await;
    assert_eq!(
        error(f.take_back("s", "target").await, StatusCode::CONFLICT).await,
        "this session's worker cannot confirm message withdrawal"
    );
    assert_eq!(f.queued("s").await, ["target", "later"]);
    assert_eq!(f.events("s").await, before);
    assert_eq!(f.command_count().await, 2);
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn take_back_never_retires_input_on_a_bad_worker_answer() {
    for state in ["updated", "", "unknown"] {
        let f = Fixture::new().await;
        f.session("s").await;
        f.queue("s", "target").await;
        let (guard, mut rx) = attach(&f, &["queue_ops"]);
        let (response, ()) = within("bad cancellation answer", async {
            tokio::join!(f.take_back("s", "target"), async {
                answer(&guard, cancellation(&mut rx, "s", "target").await, state);
            })
        })
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(f.queued("s").await, ["target"]);
        assert_eq!(f.command_count().await, 1);
        assert!(rx.try_recv().is_err());
        drop(guard);
        f.head.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn take_back_observes_token_realm_scope_and_catalog_exposes_no_body() {
    let f = Fixture::new().await;
    f.session("s").await;
    f.queue("s", "target").await;
    let token = f.head.state.store.create_api_token("other-realm", Some("lab")).await.unwrap().token.unwrap();
    let response = f.http.post(format!("{}/sessions/s/messages/target/take-back", f.api())).bearer_auth(&token).send().await.unwrap();
    assert_eq!(error(response, StatusCode::NOT_FOUND).await, "no such session");
    assert_eq!(f.queued("s").await, ["target"]);
    let spec: Value = f.http.get(format!("{}/openapi.json", f.api())).send().await.unwrap().json().await.unwrap();
    let op = &spec["paths"]["/api/v1/sessions/{id}/messages/{command_id}/take-back"]["post"];
    assert_eq!(op["operationId"], "sessions.messages.take_back");
    assert!(op["requestBody"].is_null());
    let params = op["parameters"].as_array().unwrap();
    assert!(params.iter().any(|p| p["name"] == "command_id" && p["in"] == "path" && p["required"] == true));
    f.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_queued_cancellation_and_ordinary_send_keep_later_messages_without_rewind() {
    use c6_api::{QueuedInput, SessionDetail, SessionStatus};
    use c6_testkit::{MockLlm, Reply, eventually};
    use c6_worker::{ConnState, enroll};
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    let f = Fixture::new().await;
    let mock = MockLlm::start().await;
    let (state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let enrollment = c6_head::api::new_enrollment(&f.head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = crate::open_worker(state.path()).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    let connection_task = tokio::spawn(c6_worker::conn::run(worker.clone(), tx, stop.clone()));
    within("the native worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let connection: Value = f
        .http
        .post(format!("{}/connections", f.api()))
        .json(&json!({"name": "withdrawal mock", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let gate = mock.push_gated(Reply::Text("Holding the first turn.".into()));
    mock.push(Reply::Text("Later input and correction received.".into()));
    let session: Value = f
        .http
        .post(format!("{}/sessions", f.api()))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(),
            "connection_id": connection["id"], "model": "mock", "prompt": "start held turn"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap();
    let detail = async || -> SessionDetail {
        f.http.get(format!("{}/sessions/{id}", f.api())).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    };
    let send = async |text: &str| -> QueuedInput {
        f.http
            .post(format!("{}/sessions/{id}/messages", f.api()))
            .json(&json!({"text": text}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    eventually("the first mock request to hold", async || (mock.requests().len() == 1).then_some(())).await;
    let original = send("withdraw-me-original-phrase").await;
    let later = send("keep-this-later-phrase").await;
    eventually("both inputs to reach the native inbox", async || {
        let d = detail().await;
        (d.queued.len() == 2 && d.queued.iter().all(|q| q.with_agent)).then_some(())
    })
    .await;

    // A natural durable transcript User is already taken, even though
    // these other commands are still cancellable in the worker's inbox.
    let first = f
        .events(id)
        .await
        .into_iter()
        .find_map(|e| match e.kind {
            Some(session_event::Kind::User(u)) => Some(u.command_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(error(f.take_back(id, &first).await, StatusCode::CONFLICT).await, "The agent has this message");
    let response = f.take_back(id, &original.command_id).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(detail().await.queued.iter().map(|q| &q.command_id).collect::<Vec<_>>(), [&later.command_id]);

    // This is a separate, explicit ordinary Send, not an automatic
    // resend or a rewind performed by either cancellation endpoint.
    let replacement = send("ordinary-send-replacement-phrase").await;
    assert_ne!(replacement.command_id, original.command_id);
    eventually("the ordinary Send to join the preserved later input", async || {
        let d = detail().await;
        (d.queued.len() == 2 && d.queued.iter().all(|q| q.with_agent)).then_some(())
    })
    .await;
    gate.notify_one();
    eventually("the turn to finish with no queued input", async || {
        let d = detail().await;
        (d.session.status == SessionStatus::Idle && d.queued.is_empty()).then_some(())
    })
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let model_input = serde_json::to_string(&requests).unwrap();
    assert!(!model_input.contains("withdraw-me-original-phrase"));
    assert!(model_input.contains("keep-this-later-phrase"));
    assert!(model_input.contains("ordinary-send-replacement-phrase"));
    let events = f.events(id).await;
    assert!(events.iter().all(|e| !matches!(e.kind, Some(session_event::Kind::Rewound(_)))));
    let inputs: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Some(session_event::Kind::User(u)) => Some(u.command_id.as_str()),
            _ => None,
        })
        .collect();
    assert!(!inputs.contains(&original.command_id.as_str()));
    assert!(inputs.contains(&later.command_id.as_str()));
    assert!(inputs.contains(&replacement.command_id.as_str()));
    stop.cancel();
    within("the worker connection to close", connection_task).await.unwrap().unwrap();
    f.head.shutdown().await.unwrap();
}
