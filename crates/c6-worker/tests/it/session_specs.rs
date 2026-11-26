//! A journal is a conversation, not a permanent copy of the head's settings.
//! Refresh settings on attach and before input, but never inside a turn.

use std::sync::{Arc, Weak};

use c6_api::{Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_proto::{CreateSession, SessionCommand, SessionEvent, session_command, session_event};
use c6_testkit::{MockLlm, Reply, eventually, within};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::{CancellationToken, DropGuard};

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    connection: String,
    state: tempfile::TempDir,
    project: tempfile::TempDir,
    _head_dir: tempfile::TempDir,
}

// A separate runtime is important: stopping only conn::run leaves session
// actors alive and would not exercise replay from the old journal on restart.
struct Running {
    worker: Weak<Worker>,
    stop: DropGuard,
    thread: std::thread::JoinHandle<anyhow::Result<()>>,
}

impl Running {
    fn worker(&self) -> Arc<Worker> {
        self.worker.upgrade().expect("the worker runtime is alive")
    }

    async fn stop(self) {
        let Self { stop, thread, .. } = self;
        drop(stop);
        within("the spec test worker runtime to stop", tokio::task::spawn_blocking(move || thread.join().unwrap())).await.unwrap().unwrap();
    }
}

impl Env {
    async fn new() -> Self {
        let head_dir = tempfile::tempdir().unwrap();
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
        let mock = MockLlm::start().await;
        let connection: c6_api::Connection = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "main", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let state = tempfile::tempdir().unwrap();
        let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
        let env =
            Self { head, api, http, mock, connection: connection.id, state, project: tempfile::tempdir().unwrap(), _head_dir: head_dir };
        // Only conversation requests belong in this mock's transcript.
        env.put("settings/suggestions", json!({"enabled": false, "catchup": false, "fold": false, "names": false})).await;
        env.put("settings/work", json!({"guess": false, "notes": false, "quiet_days": 3, "nudge": false, "needs_hours": 4})).await;
        env
    }

    async fn worker(&self) -> Running {
        let dir = self.state.path().to_owned();
        let stop = CancellationToken::new();
        let guard = stop.clone().drop_guard();
        let (ready, attached) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let worker = crate::open_worker(&dir).await;
                {
                    let mut overrides = worker.overrides.lock().unwrap();
                    overrides.import_home = Some(dir.join("empty-home"));
                    overrides.kit_jobs = Some(false);
                }
                let weak = Arc::downgrade(&worker);
                let (tx, mut rx) = watch::channel(ConnState::Connecting);
                let task = tokio::spawn(conn::run(worker, tx, stop));
                within("the spec test worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
                let _ = ready.send(weak);
                task.await?
            })
        });
        let worker = within("the spec test worker attachment signal", attached).await.unwrap();
        Running { worker, stop: guard, thread }
    }

    async fn put(&self, path: &str, body: Value) {
        let r = self.http.put(format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        let status = r.status();
        assert!(status.is_success(), "PUT {path}: {status}: {}", r.text().await.unwrap());
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "POST {path}: {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn start(&self, running: &Running, prompt: &str) -> Session {
        serde_json::from_value(
            self.post(
                "sessions",
                json!({"worker_id": running.worker().identity.meta.worker_id.0, "cwd": self.project.path(),
                    "connection_id": self.connection, "model": "original-model", "prompt": prompt}),
            )
            .await,
        )
        .unwrap()
    }

    async fn say(&self, id: &str, text: &str) {
        self.post(&format!("sessions/{id}/messages"), json!({"text": text})).await;
    }

    async fn ended(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("the spec test turn to end", async || {
            let ev: Vec<Event> = self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.ok()?.json().await.ok()?;
            let detail: Value = self.http.get(format!("{}/sessions/{id}", self.api)).send().await.ok()?.json().await.ok()?;
            (ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns
                && detail["session"]["status"] == "idle"
                && detail["queued"].as_array()?.is_empty())
            .then_some(ev)
        })
        .await
    }

    async fn prompt(&self, body: &str) {
        self.put("settings/prompts/session.base", json!({"body": body})).await;
    }

    async fn skills(&self, skills: Value) {
        self.put("settings/skills", skills).await;
    }
}

async fn journal(running: &Running, id: &str) -> (CreateSession, Vec<SessionEvent>) {
    running.worker().journal.load_session(id).await.unwrap().expect("session in the worker journal")
}

fn refreshes(events: &[SessionEvent]) -> Vec<&SessionEvent> {
    events.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::SpecRefreshed(_)))).collect()
}

fn refresh_notices(events: &[SessionEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(&e.kind, Some(session_event::Kind::Notice(n)) if n.title == "Session configuration refreshed"))
        .count()
}

fn system(request: &Value) -> &str {
    request["messages"][0]["content"].as_str().expect("the system prompt")
}

fn tools(request: &Value) -> Vec<&str> {
    request["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn task_lists_exactly_the_realms_named_connections_after_legacy_attach() {
    let env = Env::new().await;
    // Subscription connections normally come from OAuth. This catalog-only
    // fixture needs no login or provider calls, just an existing account row.
    let mut account = env.head.state.store.connection(&env.connection).await.unwrap().unwrap();
    account.id = uuid::Uuid::now_v7().to_string();
    account.name = "ChatGPT Account".into();
    account.kind = "chatgpt".into();
    account.default_model = Some("gpt-6.2-sol".into());
    env.head.state.store.insert_connection(&account).await.unwrap();
    let target = json!({"id": account.id});
    let alias = env.post("connections", json!({"name": "ChatGPT Subscription", "kind": "alias", "alias_target_id": target["id"]})).await;
    let cc = env.post("connections", json!({"name": "Claude Code", "kind": "claude_code", "default_model": "opus"})).await;
    env.post("realms", json!({"id": "elsewhere"})).await;
    let forbidden = env.post("connections", json!({"name": "Forbidden", "base_url": env.mock.base_url()})).await;
    env.put(&format!("connections/{}/realms", forbidden["id"].as_str().unwrap()), json!({"realms": ["elsewhere"]})).await;
    env.post("connections", json!({"name": "Head pool", "kind": "pool", "members": [{"connection_id": env.connection, "slots": 1}]})).await;
    let running = env.worker().await;
    env.mock.push(Reply::Text("first".into()));
    let s = env.start(&running, "start").await;
    env.ended(&s.id, 1).await;
    env.put(&format!("sessions/{}/subagent-model", s.id), json!({"connection_id": alias["id"], "model": "gpt-6.2-sol"})).await;
    eventually("the selected alias to reach the journal", async || {
        let (spec, _) = journal(&running, &s.id).await;
        (spec.subagent_target.as_ref()?.connection_id == alias["id"].as_str().unwrap()).then_some(())
    })
    .await;
    eventually("the alias default to be acknowledged before editing the stopped journal", async || {
        running.worker().journal.unacked().await.unwrap().is_empty().then_some(())
    })
    .await;
    running.stop().await;

    // Exercise the live regression: an old journal has head-authorized model
    // and alias-default events, but lacks the realm's connection catalog.
    let local = c6_worker::journal::Journal::open(&env.state.path().join("journal.db")).await.unwrap();
    let (mut old, events) = local.load_session(&s.id).await.unwrap().unwrap();
    old.subagent_connections_json.clear();
    assert_eq!(local.trim(i64::MAX, &[]).await.unwrap(), 1, "only the acknowledged idle fixture is replaced");
    local.import(&s.id, &old, &events).await.unwrap();
    drop(local);
    let running = env.worker().await;
    eventually("attach to restore the named realm catalog", async || {
        let (spec, _) = journal(&running, &s.id).await;
        (!spec.subagent_connections_json.is_empty()).then_some(())
    })
    .await;
    env.mock.push(Reply::Text("second".into()));
    env.say(&s.id, "list the current tools").await;
    env.ended(&s.id, 2).await;
    let (spec, _) = journal(&running, &s.id).await;
    let catalog: Vec<c6_api::SubagentConnection> = serde_json::from_str(&spec.subagent_connections_json).unwrap();
    let mut expected = vec![
        (target["id"].as_str().unwrap(), "ChatGPT Account"),
        (alias["id"].as_str().unwrap(), "ChatGPT Subscription"),
        (env.connection.as_str(), "main"),
    ];
    // Exactly the pre-existing native-worker eligibility rule. The guarded
    // integration run has no CLI; installed/fake-CLI runs may advertise it.
    let live = env.head.state.registry.get(running.worker().identity.meta.worker_id).unwrap();
    if live.capabilities.iter().any(|c| c == "claude_code_subagents_v1") {
        expected.push((cc["id"].as_str().unwrap(), "Claude Code"));
    }
    let actual: std::collections::BTreeMap<_, _> = catalog.iter().map(|c| (c.id.as_str(), c.name.as_str())).collect();
    assert_eq!(actual, expected.into_iter().collect(), "no pool, forbidden connection or fabricated model row");
    let requests = env.mock.requests();
    let task = requests[1]["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "task").unwrap();
    let names: Vec<_> = catalog.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(task["function"]["parameters"]["properties"]["connection"]["enum"], json!(names));
    let description = task["function"]["description"].as_str().unwrap();
    for c in &catalog {
        assert!(description.contains(&format!("- {} ({}", c.name, c.kind)), "{description}");
        assert!(!description.contains(&c.id), "raw connection IDs must not replace names: {description}");
    }
    assert!(description.contains("ChatGPT Subscription (chatgpt"), "the alias keeps its name, not its target's");
    running.stop().await;
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn attaching_refreshes_an_old_journal_without_recreating_the_conversation() {
    let env = Env::new().await;
    env.prompt("SPEC-OLD: original instructions.").await;
    let running = env.worker().await;
    env.mock.push(Reply::Text("SPEC-REMEMBER: the answer is forty-two.".into()));
    let s = env.start(&running, "SPEC-FIRST: remember our answer").await;
    env.ended(&s.id, 1).await;
    let (old, before) = journal(&running, &s.id).await;
    assert!(refreshes(&before).is_empty(), "creation already uses current settings");
    assert!(system(&env.mock.requests()[0]).contains("SPEC-OLD"));
    assert!(!tools(&env.mock.requests()[0]).contains(&"load_skill"));
    eventually("the idle journal to be fully acknowledged before restart", async || {
        running.worker().journal.unacked().await.unwrap().is_empty().then_some(())
    })
    .await;
    running.stop().await;

    // No worker is connected while these settings change. Its existing
    // journal's SessionStarted and saved CreateSession still have SPEC-OLD.
    env.prompt("SPEC-NEW: updated instructions.").await;
    env.skills(json!([{"name": "fresh-skill", "description": "SPEC-FRESH-SKILL", "body": "new skill body"}])).await;
    let running = env.worker().await;
    eventually("attach to refresh the idle journal before any input", async || {
        let (spec, events) = journal(&running, &s.id).await;
        (spec.base_prompt.contains("SPEC-NEW") && refreshes(&events).len() == 1).then_some(())
    })
    .await;
    let (fresh, events) = journal(&running, &s.id).await;
    assert_eq!(fresh.model, old.model, "refresh is not model switching");
    assert_eq!(fresh.cwd, old.cwd);
    assert_eq!(fresh.worktree, old.worktree);
    assert_eq!(fresh.ssh_target, old.ssh_target);
    assert_eq!(fresh.labels, old.labels);
    assert_eq!(&events[..before.len()], before.as_slice(), "the original conversation is not rewritten");
    let Some(session_event::Kind::SpecRefreshed(snapshot)) = &refreshes(&events)[0].kind else { unreachable!() };
    assert!(snapshot.system_prompt.contains("SPEC-NEW") && !snapshot.system_prompt.contains("SPEC-OLD"));
    assert!(snapshot.tools.iter().any(|n| n == "load_skill"));
    assert_eq!(env.mock.requests().len(), 1, "refresh itself must not run a turn");

    env.mock.push(Reply::Text("I remember forty-two.".into()));
    env.say(&s.id, "SPEC-SECOND: what was our answer?").await;
    env.ended(&s.id, 2).await;
    let requests = env.mock.requests();
    let req = &requests[1];
    assert_eq!(req["model"], "original-model");
    assert!(system(req).contains("SPEC-NEW") && !system(req).contains("SPEC-OLD"));
    assert!(system(req).contains("SPEC-FRESH-SKILL"));
    assert!(tools(req).contains(&"load_skill"));
    assert!(req["messages"].to_string().contains("SPEC-REMEMBER"), "the old assistant reply remains in context");
    assert!(req["messages"].to_string().contains("SPEC-FIRST"));
    assert_eq!(refreshes(&journal(&running, &s.id).await.1).len(), 1, "unchanged next-turn snapshot is a no-op");
    running.stop().await;

    // Replaying SpecRefreshed must not resurrect SessionStarted's old prompt
    // or append another user-visible configuration line on the next attach.
    let running = env.worker().await;
    env.mock.push(Reply::Text("Still current.".into()));
    env.say(&s.id, "SPEC-THIRD: check after another restart").await;
    env.ended(&s.id, 3).await;
    assert!(system(&env.mock.requests()[2]).contains("SPEC-NEW"));
    let (_, events) = journal(&running, &s.id).await;
    assert_eq!(refreshes(&events).len(), 1);
    assert_eq!(refresh_notices(&events), 1, "unchanged attaches and inputs must not duplicate the timeline line");
    running.stop().await;
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_edits_take_effect_on_the_next_turn_not_a_tool_continuation() {
    let env = Env::new().await;
    env.prompt("SPEC-BEFORE: complete this turn with the original configuration.").await;
    let running = env.worker().await;
    let gate = env.mock.push_gated(Reply::ToolCalls(vec![("bash".into(), json!({"command": "printf SPEC-TOOL-OUTPUT"}).to_string())]));
    env.mock.push(Reply::Text("The original turn is finished.".into()));
    let s = env.start(&running, "SPEC-BUSY: run a command").await;
    env.mock.request_headers(0).await;
    let (old, _) = journal(&running, &s.id).await;
    env.prompt("SPEC-AFTER: use these instructions for the next turn.").await;

    // Inject the same wire command an attaching head sends, while a model
    // response is gated. This deliberately covers the worker's deferral too,
    // rather than relying on whether the head buffers a busy session's input.
    let mut fresh = old.derived();
    fresh.base_prompt = "SPEC-AFTER: use these instructions for the next turn.".into();
    running
        .worker()
        .sessions
        .handle(SessionCommand {
            command_id: uuid::Uuid::now_v7().to_string(),
            session_id: s.id.clone(),
            kind: Some(session_command::Kind::RefreshSpec(fresh)),
            ..Default::default()
        })
        .await
        .unwrap();
    // This recorded actor message follows RefreshSpec in the same inbox.
    // Wait for it before releasing the model: a busy actor has definitely
    // observed the refresh, so a broken eager refresh cannot pass by racing
    // the end of the turn. Resetting the already-implicit child model is a
    // harmless way to obtain an existing durable actor acknowledgement.
    let barrier = uuid::Uuid::now_v7().to_string();
    running
        .worker()
        .sessions
        .handle(SessionCommand {
            command_id: barrier.clone(),
            session_id: s.id.clone(),
            kind: Some(session_command::Kind::SetSubagentModel(c6_proto::SetSubagentModel::default())),
            ..Default::default()
        })
        .await
        .unwrap();
    eventually("the busy actor to observe the refresh", async || {
        let (_, events) = journal(&running, &s.id).await;
        events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::SubagentModelSet(m)) if m.command_id == barrier)).then_some(())
    })
    .await;
    assert!(refreshes(&journal(&running, &s.id).await.1).is_empty(), "a busy actor must defer the snapshot");
    gate.notify_one();
    env.ended(&s.id, 1).await;
    let requests = env.mock.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(system(request).contains("SPEC-BEFORE") && !system(request).contains("SPEC-AFTER"));
    }
    assert!(requests[1]["messages"].to_string().contains("SPEC-TOOL-OUTPUT"));
    env.mock.push(Reply::Text("The new turn uses the new settings.".into()));
    env.say(&s.id, "SPEC-NEXT: now use the new settings").await;
    env.ended(&s.id, 2).await;
    assert!(system(&env.mock.requests()[2]).contains("SPEC-AFTER"));
    let (_, events) = journal(&running, &s.id).await;
    let first_end = events.iter().find(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(_)))).unwrap().seq;
    let changes = refreshes(&events);
    assert_eq!(changes.len(), 1, "head's matching pre-input refresh is a no-op");
    assert!(changes[0].seq > first_end, "configuration is installed only after the original turn ends");
    running.stop().await;
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn revoking_catalog_entries_removes_both_their_advertisement_and_execution() {
    let env = Env::new().await;
    env.skills(json!([{"name": "revoked-skill", "description": "SPEC-REVOKED-SKILL", "body": "SPEC-SECRET-BODY"}])).await;
    env.put("settings/agents", json!([{"name": "revoked-agent", "description": "SPEC-REVOKED-AGENT", "prompt": "SPEC-REVOKED-CHILD"}]))
        .await;
    let running = env.worker().await;
    env.mock.push(Reply::Text("I can see both catalog entries.".into()));
    let s = env.start(&running, "SPEC-CATALOG: inspect the available catalog").await;
    env.ended(&s.id, 1).await;
    let first = &env.mock.requests()[0];
    assert!(system(first).contains("SPEC-REVOKED-SKILL"));
    assert!(first["tools"].to_string().contains("SPEC-REVOKED-AGENT"));
    assert!(tools(first).contains(&"load_skill"));

    env.skills(json!([])).await;
    env.put("settings/agents", json!([])).await;
    // A model can still hallucinate a removed tool/agent name from history.
    // Neither its old tool implementation nor an old Task catalog may run.
    env.mock.push(Reply::ToolCalls(vec![
        ("load_skill".into(), json!({"name": "revoked-skill"}).to_string()),
        ("task".into(), json!({"description": "try revoked agent", "prompt": "do not run", "subagent_type": "revoked-agent"}).to_string()),
    ]));
    env.mock.push(Reply::Text("Both revoked entries are unavailable.".into()));
    env.say(&s.id, "SPEC-REVOKE: try the old catalog entries").await;
    let events = env.ended(&s.id, 2).await;
    let requests = env.mock.requests();
    assert_eq!(requests.len(), 3, "no revoked agent was allowed to call a model");
    let next = &requests[1];
    assert!(!system(next).contains("SPEC-REVOKED-SKILL"));
    assert!(!next["tools"].to_string().contains("SPEC-REVOKED-AGENT"));
    assert!(!tools(next).contains(&"load_skill"));
    let results: Vec<&Value> = requests[2]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(results.len(), 2, "both stale calls get results");
    assert!(results.iter().any(|r| r.to_string().contains("Unknown tool `load_skill`")), "removed skill tool must be refused: {results:?}");
    assert!(!results.iter().any(|r| r.to_string().contains("SPEC-SECRET-BODY")), "revoked skill contents must not leak");
    assert!(results.iter().any(|r| r.to_string().contains("no agent named")), "the stale named agent must be refused: {results:?}");
    assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::SubagentSpawned { .. })));
    running.stop().await;
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_worker_without_the_capability_receives_neither_attach_nor_pre_input_refreshes() {
    use c6_proto::{Hello, Ping, SessionCursor, WorkerMsg, head_msg, worker_hub_client::WorkerHubClient, worker_msg};

    let env = Env::new().await;
    env.prompt("SPEC-LEGACY: the old worker's instructions.").await;
    let running = env.worker().await;
    env.mock.push(Reply::Text("The old conversation is idle.".into()));
    let s = env.start(&running, "SPEC-LEGACY-FIRST").await;
    env.ended(&s.id, 1).await;
    let (_, events) = journal(&running, &s.id).await;
    running.stop().await;
    env.prompt("SPEC-LEGACY-EDIT: an unsupported worker must not receive the new wire variant.").await;

    // Reattach the same enrolled identity and cursor as an older binary,
    // advertising no session_spec_refresh_v1 support. Keep the real mTLS
    // transport and head delivery lane, but inspect the wire ourselves.
    let identity = c6_worker::Identity::load(env.state.path()).unwrap();
    let channel = tonic::transport::Endpoint::from_shared(identity.meta.head_url.clone())
        .unwrap()
        .tls_config(identity.tls_config())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tx.send(WorkerMsg {
        msg: Some(worker_msg::Msg::Hello(Hello {
            protocol_version: c6_core::PROTOCOL_VERSION,
            capabilities: Vec::new(),
            sessions: vec![SessionCursor { session_id: s.id.clone(), seq: events.last().unwrap().seq, ownership_epoch: 0 }],
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let mut inbound = WorkerHubClient::new(channel).attach(tokio_stream::wrappers::ReceiverStream::new(rx)).await.unwrap().into_inner();
    let welcome = within("the old worker's Welcome", inbound.message()).await.unwrap().unwrap();
    let Some(head_msg::Msg::Welcome(welcome)) = welcome.msg else { panic!("not a Welcome: {welcome:?}") };
    assert!(!welcome.session_spec_refresh, "an older worker does not participate in the attach barrier");

    // A Pong is an ordering barrier, not a wall-clock assertion that no
    // configuration commands happened to arrive during a short sleep.
    tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Ping(Ping { nonce: 17, ..Default::default() })) }).await.unwrap();
    within("the old worker's attach barrier", async {
        loop {
            let msg = inbound.message().await.unwrap().expect("old worker disconnected");
            match msg.msg {
                Some(head_msg::Msg::Pong(p)) => {
                    assert_eq!(p.nonce, 17);
                    break;
                }
                Some(head_msg::Msg::Command(c)) => panic!("an old worker received an unsolicited attach command: {c:?}"),
                _ => {}
            }
        }
    })
    .await;
    env.say(&s.id, "SPEC-LEGACY-NEXT").await;
    within("the old worker's ordinary input", async {
        loop {
            let msg = inbound.message().await.unwrap().expect("old worker disconnected");
            if let Some(head_msg::Msg::Command(c)) = msg.msg {
                assert_eq!(c.session_id, s.id);
                let Some(session_command::Kind::Input(input)) = c.kind else { panic!("unsupported command to an old worker: {c:?}") };
                assert_eq!(input.text, "SPEC-LEGACY-NEXT");
                break;
            }
        }
    })
    .await;
    drop(inbound);
    drop(tx);
    env.head.shutdown().await.unwrap();
}
