//! Rewind must reset Claude's native conversation before acknowledging it.
//! One standalone test owns C6_CLAUDE_BIN; the fake runs no model and keeps
//! its own native history, independently of carbon6's transcript projection.

use std::path::{Path, PathBuf};
use std::time::Duration;

use c6_api::{Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{eventually, within};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    std::fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json, os, socket, sys, uuid
from pathlib import Path
if '--version' in sys.argv:
    print('fake Claude rewind CLI'); sys.exit(0)
args = sys.argv[1:]
def arg(name):
    return args[args.index(name) + 1] if name in args else None
def log(kind, **fields):
    with open('trace.jsonl', 'a') as f:
        f.write(json.dumps(dict(kind=kind, **fields)) + '\n')
def emit(v):
    print(json.dumps(dict(session_id=sid, **v)), flush=True)
def history_file(s):
    return Path('.fake-history-' + s + '.json')
def load(s):
    return json.loads(history_file(s).read_text())
def save():
    history_file(sid).write_text(json.dumps(history))
resume, chosen, cutoff = arg('--resume'), arg('--session-id'), arg('--resume-session-at')
sid = chosen or resume
log('launch', args=args, sid=sid, resume=resume, cutoff=cutoff, pgid=os.getpgrp())
if cutoff and Path('unsupported-cutoff').exists():
    print('unknown option --resume-session-at', file=sys.stderr); sys.exit(2)
if resume and not history_file(resume).exists():
    if Path('missing-resume.sock').exists():
        with socket.socket(socket.AF_UNIX) as gate:
            gate.connect('missing-resume.sock')
            gate.sendall(b'missing\n')
            assert gate.recv(2) == b'ok'
    print('No conversation found with session ID', file=sys.stderr); sys.exit(1)
history = load(resume) if resume else []
if cutoff:
    assert '--fork-session' in args and chosen and chosen != resume
    history = history[:next(i + 1 for i, e in enumerate(history) if e['uuid'] == cutoff)]
    assert history[-1]['role'] == 'assistant'
initialized = False
holding, held_inputs = False, []
for line in sys.stdin:
    v = json.loads(line)
    log('stdin', sid=sid, value=v)
    if v['type'] == 'control_request':
        if v['request']['subtype'] == 'initialize':
            initialized = True
            if Path('init.sock').exists():
                with socket.socket(socket.AF_UNIX) as gate:
                    gate.connect('init.sock')
                    gate.sendall(b'initializing\n')
                    assert gate.recv(2) == b'ok'
            failed = Path('fail-reset').exists()
            if not failed:
                # Initialization confirms a real persisted branch. Quiet
                # promotion closes this process and resumes it before input.
                save()
            emit(dict(type='control_response', response=dict(
                subtype='error' if failed else 'success', request_id=v['request_id'],
                error='fixture refuses reset' if failed else None)))
        elif v['request']['subtype'] == 'interrupt' and holding:
            log('interrupted-held-turn', sid=sid, queued=held_inputs)
            holding = False
            emit(dict(type='result', uuid=str(uuid.uuid4()),
                      subtype='error_during_execution', is_error=True, usage={}))
        continue
    assert v['type'] == 'user', v
    # The normal stream's session_id is authoritative, not carbon6's initial ID.
    if not resume and not initialized and not history:
        sid = str(uuid.uuid4())
    text = v['message']['content']
    assert isinstance(text, str)
    if holding:
        # Written to the CLI, but not acknowledged/taken by its agent. Keep
        # these in its native queue until this process is stopped by Rewind.
        held_inputs.append(v)
        log('held-input', sid=sid, value=v)
        continue
    log('prompt', sid=sid, value=v, history=history)
    emit(dict(type='system', subtype='init'))
    emit(dict(type='user', uuid=v['uuid'], isReplay=True,
              message=dict(role='user', content=text), parent_tool_use_id=None))
    history.append(dict(uuid=v['uuid'], role='user', text=text))
    if text == 'HOLD_TURN':
        holding = True
        save()
        log('holding-turn', sid=sid, value=v)
        continue
    if 'KEEP_TOOL' in text:
        tool_uuid = str(uuid.uuid4())
        emit(dict(type='assistant', uuid=tool_uuid, parent_tool_use_id=None,
                  message=dict(id='api-message-tool-NOT-CUTOFF', content=[dict(
                      type='tool_use', id='tool1', name='Bash', input=dict(command='printf kept-tool-output'))], stop_reason='tool_use')))
        history.append(dict(uuid=tool_uuid, role='tool', text='kept-tool-call'))
        emit(dict(type='user', uuid=str(uuid.uuid4()), parent_tool_use_id=None,
                  message=dict(content=[dict(type='tool_result', tool_use_id='tool1', content='kept-tool-output')])))
    answer = 'PRIVATE_WITHDRAWN_ANSWER' if 'DROP_PRIVATE' in text else 'kept-final-answer'
    final_uuid, result_uuid = str(uuid.uuid4()), str(uuid.uuid4())
    emit(dict(type='assistant', uuid=final_uuid, parent_tool_use_id=None,
              message=dict(id='api-message-final-NOT-CUTOFF', content=[dict(type='text', text=answer)], stop_reason='end_turn')))
    history.append(dict(uuid=final_uuid, role='assistant', text=answer))
    # A child assistant is never the main-chain boundary either.
    emit(dict(type='assistant', uuid=str(uuid.uuid4()), parent_tool_use_id='child1',
              message=dict(content=[dict(type='text', text='child-only-answer')], stop_reason='end_turn')))
    save()
    log('answer', sid=sid, input_uuid=v['uuid'], final_uuid=final_uuid, result_uuid=result_uuid)
    emit(dict(type='result', uuid=result_uuid, subtype='success', is_error=False,
              usage=dict(input_tokens=1, output_tokens=1)))
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A separate runtime lets a restart drop every worker actor/process, not
/// merely reconnect the same in-memory actor. The head remains alive.
struct WorkerProcess {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    id: String,
}

impl WorkerProcess {
    async fn start(state: &Path) -> Self {
        let state = state.to_owned();
        let (stop, stopped) = oneshot::channel();
        let (ready, attached) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let worker = Worker::open(&state, Identity::load(&state).unwrap()).await.unwrap();
                let id = worker.identity.meta.worker_id.to_string();
                let (tx, mut rx) = watch::channel(ConnState::Connecting);
                tokio::spawn(conn::run(worker, tx, CancellationToken::new()));
                within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
                ready.send(id).unwrap();
                let _ = stopped.await;
            });
            // Runtime drop cancels the connection and all its actors, and
            // Non-held initialization dies; ordinary held CLIs survive for
            // the next worker to adopt, just as after a real worker death.
        });
        let id = within("the worker's ready signal", attached).await.unwrap();
        Self { stop: Some(stop), thread: Some(thread), id }
    }

    async fn stop(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        let thread = self.thread.take().unwrap();
        within("the worker runtime to stop", tokio::task::spawn_blocking(move || thread.join().unwrap())).await.unwrap();
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Created before any workers, so panic unwinding stops their runtimes first.
/// Cleanup uses the same fresh kernel ownership checks as production; saved
/// fixture PIDs alone must never authorize a signal after they are reused.
struct HeldFixtureCleanup(PathBuf);

impl Drop for HeldFixtureCleanup {
    fn drop(&mut self) {
        let state = self.0.clone();
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async {
                let held = state.join("claude/held");
                let Ok(entries) = std::fs::read_dir(&held) else { return };
                let sessions = entries
                    .flatten()
                    .filter_map(|entry| {
                        let meta: Value = serde_json::from_slice(&std::fs::read(entry.path().join("meta.json")).ok()?).ok()?;
                        Some(meta["session"].as_str()?.to_owned())
                    })
                    .collect::<std::collections::HashSet<_>>();
                let Ok(journal) = c6_worker::journal::Journal::open(&state.join("journal.db")).await else { return };
                for session in sessions {
                    let _ = c6_worker::claude_code::stop_held_for_rewind(&held, &session, &journal).await;
                }
            });
        })
        .join();
    }
}

struct Api {
    http: reqwest::Client,
    url: String,
    worker: String,
    cc: Value,
}

impl Api {
    async fn post(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}{path}", self.url)).json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{path}: {status}: {text}");
        if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap() }
    }

    async fn create(&self, dir: &Path, prompt: &str) -> String {
        let s = self
            .post(
                "/sessions",
                json!({"worker_id": self.worker, "cwd": dir, "connection_id": self.cc["id"],
                    "model": "opus", "prompt": prompt}),
            )
            .await;
        s["id"].as_str().unwrap().to_owned()
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.url)).send().await.unwrap().json().await.unwrap()
    }

    async fn detail(&self, id: &str) -> SessionDetail {
        self.http.get(format!("{}/sessions/{id}", self.url)).send().await.unwrap().json().await.unwrap()
    }

    async fn queued(&self, id: &str, text: &str) {
        eventually("the blocked message to remain accepted in the queue", async || {
            let detail = self.detail(id).await;
            detail.queued.iter().any(|q| q.text == text && q.rejected.is_none()).then_some(())
        })
        .await;
    }

    async fn barrier(&self, id: &str) {
        let count = |events: &[Event]| events.iter().filter(|e| matches!(e.kind, EventKind::SubagentModelSet { .. })).count();
        let before = count(&self.events(id).await);
        self.http
            .put(format!("{}/sessions/{id}/subagent-model", self.url))
            .json(&json!({"model": ""}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        eventually("the blocked actor to process its mailbox barrier", async || {
            (count(&self.events(id).await) == before + 1).then_some(())
        })
        .await;
    }

    async fn cancel(&self, id: &str, command_id: &str) {
        let response = self.http.delete(format!("{}/sessions/{id}/queue/{command_id}", self.url)).send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "cancel queued input {command_id}: {status}: {body}");
    }

    async fn idle(&self, id: &str, turns: usize) -> Vec<Event> {
        self.idle_allowing_errors(id, turns, 0).await
    }

    async fn idle_allowing_errors(&self, id: &str, turns: usize, expected_errors: usize) -> Vec<Event> {
        eventually("the completed turn and idle session", async || {
            let d: SessionDetail = self.http.get(format!("{}/sessions/{id}", self.url)).send().await.ok()?.json().await.ok()?;
            let events = self.events(id).await;
            let errors: Vec<_> = events.iter().filter(|e| matches!(e.kind, EventKind::Error { .. })).collect();
            assert!(errors.len() <= expected_errors, "unexpected session error: {errors:?}");
            let ended = events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (ended == turns && errors.len() == expected_errors && d.session.status == SessionStatus::Idle && d.queued.is_empty())
                .then_some(events)
        })
        .await
    }

    async fn say(&self, id: &str, text: &str) {
        self.post(&format!("/sessions/{id}/messages"), json!({"text": text})).await;
    }

    async fn rewind(&self, id: &str, to_seq: u64) {
        self.post(&format!("/sessions/{id}/rewind"), json!({"to_seq": to_seq, "restore_files": false})).await;
    }

    async fn rewound(&self, id: &str, n: usize) -> Vec<Event> {
        eventually("the confirmed rewind marker", async || {
            let events = self.events(id).await;
            (rewinds(&events) == n).then_some(events)
        })
        .await
    }
}

fn trace(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("trace.jsonl")).unwrap_or_default().lines().map(|s| serde_json::from_str(s).unwrap()).collect()
}

fn records(dir: &Path, kind: &str) -> Vec<Value> {
    trace(dir).into_iter().filter(|v| v["kind"] == kind).collect()
}

fn initializations(dir: &Path) -> usize {
    records(dir, "stdin").iter().filter(|v| v["value"]["request"]["subtype"] == "initialize").count()
}

fn rewinds(events: &[Event]) -> usize {
    events.iter().filter(|e| matches!(e.kind, EventKind::Rewound { .. })).count()
}

fn user_seq(events: &[Event], text: &str) -> u64 {
    events.iter().find(|e| matches!(&e.kind, EventKind::User { text: t, .. } if t == text)).unwrap().seq
}

fn clean_context(prompt: &Value) {
    let context = json!({"history": prompt["history"], "input": prompt["value"]["message"]["content"]}).to_string();
    assert!(!context.contains("DROP_PRIVATE") && !context.contains("PRIVATE_WITHDRAWN_ANSWER"), "withdrawn context leaked: {context}");
}

async fn seed(api: &Api, dir: &Path) -> (String, u64) {
    let id = api.create(dir, "KEEP_TOOL").await;
    api.idle(&id, 1).await;
    api.say(&id, "DROP_PRIVATE").await;
    let events = api.idle(&id, 2).await;
    for event in &events {
        if let EventKind::User { origin, .. } = &event.kind {
            assert_eq!(origin.as_deref(), Some(&c6_api::MessageOrigin::Human), "auth-off browser inputs remain the person's messages");
        }
    }
    let to_seq = user_seq(&events, "DROP_PRIVATE");
    (id, to_seq)
}

/// Inject only while this fixture's worker is stopped. No real remote worker
/// or child runs: its unconfirmable identity is enough to exercise preflight.
async fn remote_child(journal: &c6_worker::journal::Journal, id: &str, child: &str) {
    let (_, events) = journal.load_session(id).await.unwrap().unwrap();
    journal
        .append(
            id,
            &c6_proto::SessionEvent {
                seq: events.last().unwrap().seq + 1,
                ts_ms: c6_core::now_ms(),
                kind: Some(c6_proto::session_event::Kind::SubagentSpawned(c6_proto::SubagentSpawned {
                    child_session_id: child.into(),
                    background_task_id: format!("remote-task-{child}"),
                    remote: Some(Default::default()),
                    ..Default::default()
                })),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn claude_rewind_resets_native_history_before_acknowledging_and_fails_closed() {
    if !c6_testkit::have("python3") {
        return;
    }
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    let config = tempfile::tempdir().unwrap();
    // SAFETY: this standalone binary has one test, set before starting tasks.
    unsafe {
        std::env::set_var("C6_CLAUDE_BIN", &claude);
        std::env::set_var("CLAUDE_CONFIG_DIR", config.path());
    }
    c6_testkit::runnable(&claude, &["--version"]).await;
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
    let state = tempfile::tempdir().unwrap();
    let _held_cleanup = HeldFixtureCleanup(state.path().to_owned());
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = WorkerProcess::start(state.path()).await;
    let mut api = Api {
        // The product's page path, not an unattributed HTTP automation client.
        http: reqwest::Client::builder()
            .default_headers([(reqwest::header::HeaderName::from_static("x-c6-realm"), "home".parse().unwrap())].into_iter().collect())
            .build()
            .unwrap(),
        url: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()),
        worker: worker.id.clone(),
        cc: Value::Null,
    };
    // Connection setup is the console's page; session traffic below is home.
    api.cc = api
        .http
        .post(format!("{}/connections", api.url))
        .header("x-c6-realm", "admin")
        .json(&json!({"kind": "claude_code", "name": "fake rewind", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    // A token still wins over page context with sign-in off: its initial
    // message is an API card, and the head must refuse to queue its rewind.
    let token_dir = tempfile::tempdir().unwrap();
    let token = head.state.store.create_api_token("rewind-controller", None).await.unwrap().token.unwrap();
    let created: Value = api
        .http
        .post(format!("{}/sessions", api.url))
        .bearer_auth(&token)
        .json(&json!({"worker_id": api.worker, "cwd": token_dir.path(), "connection_id": api.cc["id"],
            "model": "opus", "prompt": "API_INPUT", "human_origin": true, "origin": {"kind": "human"}}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token_id = created["id"].as_str().unwrap();
    let token_events = api.idle(token_id, 1).await;
    assert!(token_events.iter().any(|e| matches!(&e.kind, EventKind::User { origin, .. }
        if origin.as_deref() == Some(&c6_api::MessageOrigin::Api { token_name: "rewind-controller".into() }))));
    let response = api
        .http
        .post(format!("{}/sessions/{token_id}/rewind", api.url))
        .json(&json!({"to_seq": user_seq(&token_events, "API_INPUT")}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("can only rewind to before one of your messages"));
    assert_eq!(rewinds(&api.events(token_id).await), 0);

    // Unknown is not a human claim, but must not lock the person out of
    // their session. Exercise the worker's actual native reset as well.
    let unknown_dir = tempfile::tempdir().unwrap();
    let created: Value = reqwest::Client::new()
        .post(format!("{}/sessions", api.url))
        .json(&json!({"worker_id": api.worker, "cwd": unknown_dir.path(), "connection_id": api.cc["id"],
            "model": "opus", "prompt": "UNKNOWN_INPUT", "human_origin": true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let unknown_id = created["id"].as_str().unwrap();
    let unknown_events = api.idle(unknown_id, 1).await;
    assert!(unknown_events.iter().any(|e| matches!(&e.kind, EventKind::User { origin, .. }
        if origin.as_deref() == Some(&c6_api::MessageOrigin::Unknown))));
    api.rewind(unknown_id, user_seq(&unknown_events, "UNKNOWN_INPUT")).await;
    api.rewound(unknown_id, 1).await;

    // A: a completed tool turn cuts at its FINAL top-level assistant UUID.
    let a = tempfile::tempdir().unwrap();
    let (a_id, a_to) = seed(&api, a.path()).await;
    let answers = records(a.path(), "answer");
    let kept = &answers[0];
    assert_ne!(kept["sid"], a_id, "the fake must exercise stream-json session_id adoption");
    let journal = c6_worker::journal::Journal::open(&state.path().join("journal.db")).await.unwrap();
    let events = api.events(&a_id).await;
    let kept_end = events.iter().find(|e| matches!(e.kind, EventKind::TurnEnded { .. })).unwrap().seq;
    let boundary = journal.claude_turn(&a_id, kept_end).await.unwrap().unwrap();
    assert_eq!(boundary.session_id, kept["sid"].as_str().unwrap());
    assert_eq!(boundary.input_uuids, [kept["input_uuid"].as_str().unwrap()]);
    assert_eq!(boundary.cutoff_uuid, kept["final_uuid"].as_str().unwrap());
    assert_ne!(boundary.cutoff_uuid, kept["result_uuid"].as_str().unwrap());

    let gate = UnixListener::bind(a.path().join("init.sock")).unwrap();
    api.rewind(&a_id, a_to).await;
    let (mut pending, _) = within("the no-model initialize request", gate.accept()).await.unwrap();
    let mut signal = [0; 13];
    within("the fake's initialization barrier", pending.read_exact(&mut signal)).await.unwrap();
    assert_eq!(&signal, b"initializing\n");
    api.say(&a_id, "AFTER_A").await;
    // Nothing-happens assertion: readiness has arrived but is not acknowledged.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rewinds(&api.events(&a_id).await), 0, "Rewound must wait for initialize success");
    assert_eq!(records(a.path(), "prompt").len(), 2, "no user prompt before initialize success");
    pending.write_all(b"ok").await.unwrap();
    drop(gate);
    std::fs::remove_file(a.path().join("init.sock")).unwrap();
    let events = api.idle(&a_id, 3).await;
    assert_eq!(rewinds(&events), 1);
    let confirmed = events.iter().find(|e| matches!(e.kind, EventKind::Rewound { .. })).unwrap().seq;
    assert!(user_seq(&events, "AFTER_A") > confirmed, "queued input comes after the confirmed reset");
    let launches = records(a.path(), "launch");
    assert_eq!(launches.len(), 3, "old process, supported native cutoff, then quiet held promotion");
    let fork = &launches[1];
    assert_eq!(fork["resume"], kept["sid"]);
    assert_eq!(fork["cutoff"], kept["final_uuid"]);
    let args = fork["args"].as_array().unwrap();
    assert!(args.iter().any(|a| a == "--fork-session"));
    assert_ne!(fork["sid"], kept["sid"]);
    let after = records(a.path(), "prompt").pop().unwrap();
    assert_eq!(after["sid"], fork["sid"], "the fake honors the chosen --session-id");
    clean_context(&after);
    assert!(after["history"].to_string().contains("KEEP_TOOL"));
    // The raw audit log is retained; only effective/model context is pruned.
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::User { text, .. } if text == "DROP_PRIVATE")));

    // B: an older CLI exits on the hidden flag, then initializes a fresh
    // conversation and receives exactly the kept history as a handoff.
    let b = tempfile::tempdir().unwrap();
    let (b_id, b_to) = seed(&api, b.path()).await;
    std::fs::write(b.path().join("unsupported-cutoff"), "").unwrap();
    api.rewind(&b_id, b_to).await;
    api.rewound(&b_id, 1).await;
    assert_eq!(records(b.path(), "prompt").len(), 2, "reset itself runs no model");
    let launches = records(b.path(), "launch");
    assert_eq!(launches.len(), 3, "old process, unsupported A, then fresh B");
    assert!(launches[1]["cutoff"].is_string());
    assert!(launches[2]["resume"].is_null() && launches[2]["cutoff"].is_null());
    assert_ne!(launches[1]["sid"], launches[2]["sid"], "fallback gets its own fresh ID");
    api.say(&b_id, "AFTER_B").await;
    api.idle(&b_id, 3).await;
    let after = records(b.path(), "prompt").pop().unwrap();
    clean_context(&after);
    let input = after["value"]["message"]["content"].as_str().unwrap();
    assert!(input.contains("<carbon6-handoff") && input.contains("KEEP_TOOL") && input.contains("kept-final-answer"), "{input}");
    assert!(input.contains("kept-tool-output") && input.contains("AFTER_B"), "{input}");
    assert_eq!(after["history"], json!([]), "fresh B has no abandoned native state");

    // Before the first user message there is no assistant cutoff: fresh only.
    let first = tempfile::tempdir().unwrap();
    let first_id = api.create(first.path(), "DROP_PRIVATE").await;
    let events = api.idle(&first_id, 1).await;
    api.rewind(&first_id, user_seq(&events, "DROP_PRIVATE")).await;
    api.rewound(&first_id, 1).await;
    api.say(&first_id, "FIRST_REPLACEMENT").await;
    api.idle(&first_id, 2).await;
    assert_eq!(records(first.path(), "launch").len(), 3, "old process, fresh reset, then quiet held promotion");
    assert!(records(first.path(), "launch")[1]["resume"].is_null());
    clean_context(&records(first.path(), "prompt").pop().unwrap());

    // A confirmed fresh initialization need not have saved a native
    // transcript before its first prompt. Quiet promotion then encounters a
    // real held resume failure: the exited CLI leaves its keeper alive.
    // Inline fallback must close that entire group, not merely drop Process,
    // and must preserve late, unrecorded inputs without duplicating receipts.
    let missing = tempfile::tempdir().unwrap();
    let missing_id = api.create(missing.path(), "DROP_PRIVATE").await;
    let events = api.idle(&missing_id, 1).await;
    api.rewind(&missing_id, user_seq(&events, "DROP_PRIVATE")).await;
    api.rewound(&missing_id, 1).await;
    let (fresh, applied) = journal.claude_rewind(&missing_id).await.unwrap().unwrap();
    assert!(fresh.fresh && applied != 0);
    std::fs::remove_file(missing.path().join(format!(".fake-history-{}.json", fresh.session_id))).unwrap();
    let gate = UnixListener::bind(missing.path().join("missing-resume.sock")).unwrap();
    api.say(&missing_id, "MISSING_HISTORY_REPLACEMENT").await;
    let (mut pending, _) = within("held resume to discover its missing native transcript", gate.accept()).await.unwrap();
    let mut signal = [0; 8];
    within("missing-resume gate readiness", pending.read_exact(&mut signal)).await.unwrap();
    assert_eq!(&signal, b"missing\n");
    let launches = records(missing.path(), "launch");
    let resumed = launches.last().unwrap();
    assert_eq!(resumed["resume"].as_str(), Some(fresh.session_id.as_str()));
    let pg = nix::unistd::Pid::from_raw(resumed["pgid"].as_i64().unwrap() as i32);
    let held_record: Value =
        serde_json::from_slice(&std::fs::read(state.path().join("claude/held").join(&fresh.session_id).join("meta.json")).unwrap())
            .unwrap();
    assert_eq!(held_record["pgid"].as_i64(), Some(i64::from(pg.as_raw())));
    assert!(nix::sys::signal::killpg(pg, None).is_ok(), "this regression requires an actually held runtime");
    api.say(&missing_id, "JOIN_DURING_MISSING_RESUME").await;
    let joining = eventually("a late input to be written but unconfirmed before the resume failure", async || {
        api.detail(&missing_id).await.queued.into_iter().find(|q| q.text == "JOIN_DURING_MISSING_RESUME" && q.with_agent)
    })
    .await;
    assert!(!api.events(&missing_id).await.iter().any(|e| matches!(&e.kind,
        EventKind::User { command_id, .. } if command_id == &joining.command_id)));
    pending.write_all(b"ok").await.unwrap();
    drop(pending);
    drop(gate);
    std::fs::remove_file(missing.path().join("missing-resume.sock")).unwrap();
    let recovered = eventually("fresh fallback to take the original and late input exactly once", async || {
        let events = api.events(&missing_id).await;
        assert!(!events.iter().any(|e| matches!(e.kind, EventKind::Error { .. })), "fallback must close the held keeper before spawning");
        for text in ["MISSING_HISTORY_REPLACEMENT", "JOIN_DURING_MISSING_RESUME"] {
            let count = events.iter().filter(|e| matches!(&e.kind, EventKind::User { text: t, .. } if t == text)).count();
            assert!(count <= 1, "{text} must not acquire duplicate User receipts");
            if count == 0 {
                return None;
            }
        }
        let detail = api.detail(&missing_id).await;
        (detail.queued.is_empty() && detail.session.status == SessionStatus::Idle).then_some(events)
    })
    .await;
    assert_eq!(rewinds(&recovered), 1, "native transcript fallback must not replay Rewind");
    assert_eq!(nix::sys::signal::killpg(pg, None), Err(nix::errno::Errno::ESRCH), "the failed resume's keeper must be gone");
    let prompts = records(missing.path(), "prompt");
    for text in ["MISSING_HISTORY_REPLACEMENT", "JOIN_DURING_MISSING_RESUME"] {
        assert_eq!(prompts.iter().filter(|p| p["value"]["message"]["content"].as_str().is_some_and(|t| t.contains(text))).count(), 1);
    }
    for prompt in &prompts[1..] {
        clean_context(prompt);
    }

    // A real worker-runtime restart must use the reset conversation, never
    // --resume the abandoned pre-rewind ID (both native A and fallback B).
    let held_launches = [records(a.path(), "launch").len(), records(b.path(), "launch").len()];
    worker.stop().await;
    let worker_id = api.worker.parse().unwrap();
    eventually("the stopped worker to detach", async || head.state.registry.get(worker_id).is_none().then_some(())).await;
    let rejected_rewind = async || {
        let response = api.http.post(format!("{}/sessions/{a_id}/rewind", api.url)).json(&json!({"to_seq": a_to})).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(response.text().await.unwrap().contains("updated worker"));
        assert_eq!(rewinds(&api.events(&a_id).await), 1, "unsupported workers must not enqueue a rewind");
    };
    rejected_rewind().await;
    // A legacy worker can run Claude Code but cannot confirm native-memory
    // reset. Advertising only the old capability must not bypass the gate.
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let legacy = head.state.registry.attach(worker_id, 0, tx, vec!["claude_code".into()], "worker");
    rejected_rewind().await;
    drop(legacy);
    let worker = WorkerProcess::start(state.path()).await;
    let mut fork_prompts = Vec::new();
    for ((dir, id, turns), launches_before) in [(a.path(), a_id.as_str(), 4), (b.path(), b_id.as_str(), 4)].into_iter().zip(held_launches) {
        let previous_sid = records(dir, "prompt").last().unwrap()["sid"].clone();
        api.say(id, "AFTER_RESTART").await;
        api.idle(id, turns).await;
        let launches = records(dir, "launch");
        assert_eq!(launches.len(), launches_before, "restart must adopt the confirmed held branch, not spawn/resume abandoned history");
        assert_eq!(launches.last().unwrap()["resume"], previous_sid);
        clean_context(&records(dir, "prompt").pop().unwrap());
        // Side chats seed from the effective transcript, not the raw audit log.
        let events = api.events(id).await;
        let anchor = events.iter().rev().find(|e| matches!(e.kind, EventKind::Assistant { .. })).unwrap().seq;
        let side: Session = serde_json::from_value(
            api.post(
                &format!("/sessions/{id}/side-chats"),
                json!({"anchor_seq": anchor, "connection_id": api.cc["id"], "model": "opus", "prompt": "FORK_AFTER_REWIND"}),
            )
            .await,
        )
        .unwrap();
        api.idle(&side.id, 1).await;
        let fork_prompt = records(dir, "prompt").pop().unwrap();
        assert!(fork_prompt["value"]["message"]["content"].as_str().unwrap().contains("KEEP_TOOL"));
        fork_prompts.push(fork_prompt);
    }

    // Restored public events may outlive the local chain metadata. A
    // historical Rewound must never be treated as permission to resume the
    // original session ID: use a fresh kept-history handoff instead.
    let unselected = std::fs::read_dir(state.path().join("claude/held"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta: Value = serde_json::from_slice(&std::fs::read(entry.path().join("meta.json")).ok()?).ok()?;
            (meta["session"].as_str() == Some(a_id.as_str())).then(|| nix::unistd::Pid::from_raw(meta["pgid"].as_i64().unwrap() as i32))
        })
        .collect::<Vec<_>>();
    assert!(!unselected.is_empty(), "metadata-loss reconstruction must face an actually surviving held runtime");
    let rewinds_before_loss = rewinds(&api.events(&a_id).await);
    worker.stop().await;
    assert!(
        unselected.iter().all(|pg| nix::sys::signal::killpg(*pg, None).is_ok()),
        "held runtimes survive the worker, not just the actor"
    );
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", state.path().join("journal.db").display())).await.unwrap();
    for sql in ["DELETE FROM claude_turns WHERE session_id = ?", "DELETE FROM claude_rewinds WHERE session_id = ?"] {
        sqlx::query(sql).bind(&a_id).execute(&pool).await.unwrap();
    }
    pool.close().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&a_id, "AFTER_METADATA_LOSS").await;
    let reconstructed = api.idle(&a_id, 5).await;
    assert_eq!(rewinds(&reconstructed), rewinds_before_loss, "reconstruction must not replay the completed Rewind");
    assert!(
        unselected.iter().all(|pg| nix::sys::signal::killpg(*pg, None) == Err(nix::errno::Errno::ESRCH)),
        "unselected runtimes must be stopped before fresh kept-history input"
    );
    let fresh = records(a.path(), "launch").pop().unwrap();
    assert!(fresh["resume"].is_null(), "missing chain metadata requires conservative fresh fallback: {fresh}");
    let after = records(a.path(), "prompt").pop().unwrap();
    clean_context(&after);
    assert_eq!(after["history"], json!([]));
    assert!(after["value"]["message"]["content"].as_str().unwrap().contains("KEEP_TOOL"));

    // Crash after destructive reset began, while the fake is waiting for
    // initialization acknowledgment. The socket is the barrier: no timer
    // guesses whether the reset reached this point before runtime shutdown.
    let crashed = tempfile::tempdir().unwrap();
    let (crashed_id, crashed_to) = seed(&api, crashed.path()).await;
    let held_root = state.path().join("claude/held");
    assert!(
        std::fs::read_dir(&held_root).unwrap().filter_map(Result::ok).any(|entry| {
            std::fs::read(entry.path().join("meta.json"))
                .ok()
                .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
                .is_some_and(|meta| meta["session"].as_str() == Some(crashed_id.as_str()))
        }),
        "the interrupted-initialize fixture must actually start held"
    );
    let gate = UnixListener::bind(crashed.path().join("init.sock")).unwrap();
    api.rewind(&crashed_id, crashed_to).await;
    let (mut pending, _) = within("the reset to initialize before worker shutdown", gate.accept()).await.unwrap();
    let mut signal = [0; 13];
    within("the crash fixture's initialization barrier", pending.read_exact(&mut signal)).await.unwrap();
    assert_eq!(&signal, b"initializing\n");
    let (reset, applied) = journal.claude_rewind(&crashed_id).await.unwrap().unwrap();
    assert!(!reset.untouched, "reset must be durably destructive before initializing the replacement CLI");
    assert!(reset.failure.is_empty(), "this fixture crashes during reset, not after a reported failure");
    assert_eq!(applied, 0);
    assert!(!held_root.join(&reset.session_id).exists(), "unconfirmed initialization must not acquire a keeper");
    assert_eq!(initializations(crashed.path()), 1);
    api.say(&crashed_id, "AFTER_CRASH_DURING_INITIALIZE").await;
    api.queued(&crashed_id, "AFTER_CRASH_DURING_INITIALIZE").await;
    assert_eq!(records(crashed.path(), "prompt").len(), 2, "queued input must not reach the unconfirmed CLI");
    assert_eq!(rewinds(&api.events(&crashed_id).await), 0);

    worker.stop().await;
    let (durable, applied) = journal.claude_rewind(&crashed_id).await.unwrap().unwrap();
    assert_eq!(durable.command_id, reset.command_id);
    assert!(!durable.untouched, "runtime loss must not make a destructive reset cancelable");
    assert!(durable.failure.is_empty());
    assert_eq!(applied, 0);
    assert_eq!(records(crashed.path(), "prompt").len(), 2);
    std::fs::remove_file(crashed.path().join("init.sock")).unwrap();
    drop(gate);
    drop(pending);

    let worker = WorkerProcess::start(state.path()).await;
    api.barrier(&crashed_id).await;
    let (blocked, applied) = journal.claude_rewind(&crashed_id).await.unwrap().unwrap();
    assert_eq!(applied, 0);
    assert_eq!(blocked.command_id, reset.command_id);
    assert!(blocked.failure.contains("Worker restarted before Claude Code confirmed Rewind"));
    assert!(blocked.retries.is_empty());
    assert_eq!(initializations(crashed.path()), 1, "restart must not automatically retry an interrupted initialization");
    assert_eq!(records(crashed.path(), "prompt").len(), 2, "queued input must stay blocked after restart");
    let blocked_events = api.events(&crashed_id).await;
    assert_eq!(rewinds(&blocked_events), 0);
    assert!(!blocked_events.iter().any(|e| matches!(&e.kind,
        EventKind::NoticeQueued { command_id, .. } if command_id.starts_with("resume-after-restart-"))));
    assert!(!journal.is_applied(&reset.command_id).await.unwrap());
    api.rewind(&crashed_id, crashed_to).await;
    let events = api.idle_allowing_errors(&crashed_id, 3, 1).await;
    assert_eq!(rewinds(&events), 2, "a new explicit retry confirms both the original and retry receipts");
    let confirmed = events.iter().find(|e| matches!(e.kind, EventKind::Rewound { .. })).unwrap().seq;
    assert!(user_seq(&events, "AFTER_CRASH_DURING_INITIALIZE") > confirmed, "queued input must follow the recovered reset");
    assert!(journal.is_applied(&reset.command_id).await.unwrap());
    assert_eq!(initializations(crashed.path()), 2, "only the explicit retry may initialize a replacement");
    let prompts = records(crashed.path(), "prompt");
    assert_eq!(prompts.len(), 3);
    let after = prompts.last().unwrap();
    clean_context(after);
    assert!(after["history"].to_string().contains("KEEP_TOOL"));

    // A retained remote child is not affected by a conversation-only
    // rewind. A child in the withdrawn suffix, however, must refuse it
    // before loading/stopping the actor or initializing another CLI.
    let retained_remote = tempfile::tempdir().unwrap();
    let retained_id = api.create(retained_remote.path(), "KEEP_TOOL").await;
    api.idle(&retained_id, 1).await;
    let affected_remote = tempfile::tempdir().unwrap();
    let (affected_id, affected_to) = seed(&api, affected_remote.path()).await;
    let affected_trace = trace(affected_remote.path());
    let affected_native_id = records(affected_remote.path(), "prompt").last().unwrap()["sid"].clone();
    worker.stop().await;
    remote_child(&journal, &retained_id, "retained-remote-child").await;
    remote_child(&journal, &affected_id, "affected-remote-child").await;
    let worker = WorkerProcess::start(state.path()).await;

    api.say(&retained_id, "DROP_PRIVATE").await;
    let events = api.idle(&retained_id, 2).await;
    api.rewind(&retained_id, user_seq(&events, "DROP_PRIVATE")).await;
    api.rewound(&retained_id, 1).await;
    api.say(&retained_id, "AFTER_RETAINED_REMOTE_REWIND").await;
    let retained_events = api.idle(&retained_id, 3).await;
    clean_context(records(retained_remote.path(), "prompt").last().unwrap());

    // File restoration affects even retained children. Refusing it must
    // preserve the native mapping from the earlier confirmed rewind.
    let retained_mapping = journal.claude_rewind(&retained_id).await.unwrap().unwrap();
    let retained_trace = trace(retained_remote.path());
    api.post(
        &format!("/sessions/{retained_id}/rewind"),
        json!({"to_seq": user_seq(&retained_events, "AFTER_RETAINED_REMOTE_REWIND"), "restore_files": true}),
    )
    .await;
    eventually("file restore to refuse the retained remote writer", async || {
        let rejected: Option<String> = sqlx::query_scalar(
            "SELECT error FROM commands WHERE session_id = ? AND state = 'rejected' AND text IS NULL ORDER BY seq DESC LIMIT 1",
        )
        .bind(&retained_id)
        .fetch_optional(head.state.store.pool())
        .await
        .unwrap();
        rejected.filter(|reason| reason.contains("retained-remote-child"))
    })
    .await;
    assert_ne!(api.detail(&retained_id).await.session.status, SessionStatus::Failed, "a rejected Rewind is not a failed Create");
    api.idle_allowing_errors(&retained_id, 3, 1).await;
    assert_eq!(trace(retained_remote.path()), retained_trace, "refused file restore must not initialize another reset");
    assert_eq!(
        serde_json::to_value(journal.claude_rewind(&retained_id).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(retained_mapping).unwrap(),
        "preflight refusal must preserve the previous confirmed native mapping"
    );
    api.say(&retained_id, "AFTER_RESTORE_PREFLIGHT_REFUSAL").await;
    let retained_events = api.idle_allowing_errors(&retained_id, 4, 1).await;
    let after = records(retained_remote.path(), "prompt").pop().unwrap();
    clean_context(&after);
    assert!(after["history"].to_string().contains("AFTER_RETAINED_REMOTE_REWIND"));

    api.rewind(&affected_id, affected_to).await;
    let (rejected_id, reason): (String, String) = eventually("the affected remote child to reject preflight", async || {
        sqlx::query_as(
            "SELECT id, error FROM commands WHERE session_id = ? AND state = 'rejected' AND text IS NULL ORDER BY seq DESC LIMIT 1",
        )
        .bind(&affected_id)
        .fetch_optional(head.state.store.pool())
        .await
        .unwrap()
    })
    .await;
    assert!(reason.contains("affected-remote-child"), "{reason}");
    assert_ne!(api.detail(&affected_id).await.session.status, SessionStatus::Failed, "a rejected Rewind is not a failed Create");
    assert_eq!(trace(affected_remote.path()), affected_trace, "preflight must not initialize or send a prompt");
    assert!(journal.claude_rewind(&affected_id).await.unwrap().is_none(), "preflight must leave no pending reset");
    let refused_events = api.idle_allowing_errors(&affected_id, 2, 1).await;
    assert_eq!(rewinds(&refused_events), 0);
    assert!(refused_events.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("affected-remote-child"))));

    api.say(&affected_id, "AFTER_REMOTE_PREFLIGHT_REFUSAL").await;
    api.idle_allowing_errors(&affected_id, 3, 1).await;
    let after = records(affected_remote.path(), "prompt").pop().unwrap();
    assert_eq!(after["sid"], affected_native_id);
    assert!(after["history"].to_string().contains("DROP_PRIVATE"), "the refused rewind must retain the target input");
    assert!(after["history"].to_string().contains("PRIVATE_WITHDRAWN_ANSWER"), "the refused rewind must retain the answer");

    // Simulate a crash after dispatch prepared an untouched reset, with a
    // previously confirmed native mapping to restore. This bypasses manager
    // preflight and exercises actual durable intent rollback on actor load.
    let prepared_to = user_seq(&retained_events, "AFTER_RESTORE_PREFLIGHT_REFUSAL");
    let prepared_id = format!("prepared-rewind-{retained_id}");
    let previous_mapping = journal.claude_rewind(&retained_id).await.unwrap().unwrap();
    let prepared_initializations = initializations(retained_remote.path());
    worker.stop().await;
    remote_child(&journal, &retained_id, "prepared-remote-child").await;
    journal
        .prepare_claude_rewind(
            &retained_id,
            &c6_worker::claude_code::Reset {
                command_id: prepared_id.clone(),
                to_seq: prepared_to,
                session_id: uuid::Uuid::now_v7().to_string(),
                fresh: true,
                withdrawn: previous_mapping.0.withdrawn.clone(),
                untouched: true,
                previous: Some(Box::new(previous_mapping.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&retained_id, "AFTER_PREPARED_PREFLIGHT_ROLLBACK").await;
    let prepared_events = api.idle_allowing_errors(&retained_id, 5, 2).await;
    assert_eq!(rewinds(&prepared_events), 1, "abandoning untouched intent must not append another Rewound");
    assert_eq!(
        serde_json::to_value(journal.claude_rewind(&retained_id).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(&previous_mapping).unwrap(),
        "prepared-intent rollback must restore the exact previous native mapping"
    );
    assert!(journal.is_applied(&prepared_id).await.unwrap(), "safe rollback must persist a terminal receipt");
    assert!(journal.command_rejection(&prepared_id).await.unwrap().unwrap().contains("prepared-remote-child"));
    assert_eq!(initializations(retained_remote.path()), prepared_initializations, "prepared preflight must not initialize a CLI reset");
    let after = records(retained_remote.path(), "prompt").pop().unwrap();
    clean_context(&after);
    assert!(after["history"].to_string().contains("AFTER_RESTORE_PREFLIGHT_REFUSAL"), "canceled intent must retain its target message");

    api.say(&affected_id, "AFTER_REMOTE_PREFLIGHT_RESTART").await;
    let events = api.idle_allowing_errors(&affected_id, 4, 1).await;
    assert_eq!(rewinds(&events), 0, "rejected preflight must not return as a pending reset after restart");
    let prompts = records(affected_remote.path(), "prompt");
    let after = prompts.last().unwrap();
    assert_eq!(after["sid"], affected_native_id);
    let history = after["history"].to_string();
    assert!(history.contains("DROP_PRIVATE") && history.contains("PRIVATE_WITHDRAWN_ANSWER"));
    assert!(history.contains("AFTER_REMOTE_PREFLIGHT_REFUSAL"));
    assert!(
        records(affected_remote.path(), "stdin").iter().all(|r| r["value"]["type"] != "control_request"),
        "preflight refusal and restart must never initialize a reset"
    );
    let rejected_state: String =
        sqlx::query_scalar("SELECT state FROM commands WHERE id = ?").bind(&rejected_id).fetch_one(head.state.store.pool()).await.unwrap();
    assert_eq!(rejected_state, "rejected", "rejected preflight must not be replayed as a successful rewind");

    worker.stop().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&retained_id, "AFTER_PREPARED_ROLLBACK_RESTART").await;
    let events = api.idle_allowing_errors(&retained_id, 6, 2).await;
    assert_eq!(rewinds(&events), 1, "recovered rollback must not replay the refused reset");
    assert_eq!(initializations(retained_remote.path()), prepared_initializations);
    let after = records(retained_remote.path(), "prompt").pop().unwrap();
    clean_context(&after);
    let history = after["history"].to_string();
    assert!(history.contains("AFTER_RESTORE_PREFLIGHT_REFUSAL") && history.contains("AFTER_PREPARED_PREFLIGHT_ROLLBACK"));
    assert!(journal.command_rejection(&prepared_id).await.unwrap().unwrap().contains("prepared-remote-child"));

    // Both initialization strategies fail once. New messages and worker
    // restarts must retain the failure, not initialize again automatically.
    let failed = tempfile::tempdir().unwrap();
    let (failed_id, failed_to) = seed(&api, failed.path()).await;
    std::fs::write(failed.path().join("fail-reset"), "").unwrap();
    api.rewind(&failed_id, failed_to).await;
    let errors = async || {
        let events = api.events(&failed_id).await;
        events
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("Rewind could not be confirmed")))
            .count()
    };
    eventually("the failed reset error", async || (errors().await == 1).then_some(())).await;
    let failed_trace = trace(failed.path());
    api.say(&failed_id, "MUST_NOT_REACH_FAILED_RESET").await;
    api.queued(&failed_id, "MUST_NOT_REACH_FAILED_RESET").await;
    let sentinel = tempfile::tempdir().unwrap();
    let sentinel_id = api.create(sentinel.path(), "CHECK_BLOCKED_RESET").await;
    api.idle(&sentinel_id, 1).await;
    api.barrier(&failed_id).await;
    assert_eq!(errors().await, 1, "ordinary input must not retry the failed reset");
    assert_eq!(trace(failed.path()), failed_trace, "blocked input must send neither initialize nor a model prompt");
    assert_eq!(rewinds(&api.events(&failed_id).await), 0);
    assert_eq!(records(failed.path(), "prompt").len(), 2);
    let (reset, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
    assert_eq!(reset.to_seq, failed_to);
    assert_eq!(applied, 0, "failure leaves a durable unconfirmed intent");
    worker.stop().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&failed_id, "MUST_NOT_LEAK_AFTER_RESTART").await;
    api.queued(&failed_id, "MUST_NOT_LEAK_AFTER_RESTART").await;
    // A completed unrelated turn proves the restarted worker is attached
    // and processing commands; queue acceptance alone only proves head work.
    api.say(&sentinel_id, "CHECK_BLOCKED_RESET_AFTER_RESTART").await;
    api.idle(&sentinel_id, 2).await;
    api.barrier(&failed_id).await;
    assert_eq!(errors().await, 1, "restart and redelivery must not retry the failed reset");
    assert_eq!(trace(failed.path()), failed_trace, "restart must not initialize the failed reset again");
    assert_eq!(rewinds(&api.events(&failed_id).await), 0);
    assert_eq!(records(failed.path(), "prompt").len(), 2);

    // A genuine retry B may fail while recovering the original intent A.
    // Retire B durably without clearing A's destructive barrier: otherwise
    // head redelivery mistakes B for another new, explicit retry of A.
    let failed_initializations = initializations(failed.path());
    api.rewind(&failed_id, failed_to).await;
    let rejected_retry: String = sqlx::query_scalar("SELECT id FROM commands WHERE session_id = ? ORDER BY seq DESC LIMIT 1")
        .bind(&failed_id)
        .fetch_one(head.state.store.pool())
        .await
        .unwrap();
    assert_ne!(rejected_retry, reset.command_id);
    eventually("one failed explicit retry to report its recovery failure", async || (errors().await == 2).then_some(())).await;
    eventually("the failed retry to have durable worker and head rejections", async || {
        let rejection = journal.command_rejection(&rejected_retry).await.unwrap()?;
        assert!(!rejection.is_empty());
        let state: String = sqlx::query_scalar("SELECT state FROM commands WHERE id = ?")
            .bind(&rejected_retry)
            .fetch_one(head.state.store.pool())
            .await
            .unwrap();
        (state == "rejected").then_some(())
    })
    .await;
    assert!(journal.is_applied(&rejected_retry).await.unwrap(), "failed retry B needs a terminal receipt");
    let (failed_again, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
    assert_eq!(failed_again.command_id, reset.command_id, "failed retry B must retain original intent A");
    assert_eq!(applied, 0);
    assert!(!failed_again.untouched && !failed_again.failure.is_empty(), "A must retain its destructive failure barrier");
    assert!(!journal.is_applied(&reset.command_id).await.unwrap(), "A must not be acknowledged before reset succeeds");
    assert_eq!(initializations(failed.path()), failed_initializations + 2, "one failed recovery tries fork and fresh once each");
    let rejected_trace = trace(failed.path());
    worker.stop().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&failed_id, "AFTER_FAILED_EXPLICIT_RETRY_RESTART").await;
    api.queued(&failed_id, "AFTER_FAILED_EXPLICIT_RETRY_RESTART").await;
    api.say(&sentinel_id, "CHECK_FAILED_EXPLICIT_RETRY_RESTART").await;
    api.idle(&sentinel_id, 3).await;
    api.barrier(&failed_id).await;
    assert_eq!(errors().await, 2, "redelivery of rejected B must not retry A again");
    assert_eq!(trace(failed.path()), rejected_trace, "restart and new input must not initialize after failed retry B");
    assert_eq!(rewinds(&api.events(&failed_id).await), 0);
    let (still_blocked, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(still_blocked).unwrap(), serde_json::to_value(failed_again).unwrap());
    assert_eq!(applied, 0);
    assert!(journal.command_rejection(&rejected_retry).await.unwrap().is_some(), "B's rejection must survive restart");

    // Another explicit retry can crash while recovering the same A. The
    // non-held initialization must die with its worker; even durable retry
    // receipts from before that crash cannot authorize another initialize.
    let before_crashed_retry = initializations(failed.path());
    let gate = UnixListener::bind(failed.path().join("init.sock")).unwrap();
    api.rewind(&failed_id, failed_to).await;
    let (mut pending, _) = within("the crash-during-retry initialization barrier", gate.accept()).await.unwrap();
    let mut signal = [0; 13];
    within("the explicit retry to reach initialization before crashing", pending.read_exact(&mut signal)).await.unwrap();
    assert_eq!(&signal, b"initializing\n");
    let crashed_retry: String = sqlx::query_scalar("SELECT id FROM commands WHERE session_id = ? ORDER BY seq DESC LIMIT 1")
        .bind(&failed_id)
        .fetch_one(head.state.store.pool())
        .await
        .unwrap();
    assert_ne!(crashed_retry, reset.command_id);
    assert_ne!(crashed_retry, rejected_retry);
    let (initializing, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
    assert_eq!(initializing.command_id, reset.command_id);
    assert_eq!(applied, 0);
    assert!(!initializing.untouched && initializing.failure.is_empty());
    assert_eq!(initializing.retries, std::slice::from_ref(&crashed_retry), "the explicit retry must survive an interrupted recovery of A");
    api.rewind(&failed_id, failed_to).await;
    let crashed_companion = eventually("a second retry to be authorized during the gated recovery before crashing", async || {
        let (pending, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
        assert_eq!(pending.command_id, reset.command_id);
        assert_eq!(applied, 0);
        (pending.retries.len() == 2 && pending.retries.contains(&crashed_retry))
            .then(|| pending.retries.into_iter().find(|id| id != &crashed_retry).unwrap())
    })
    .await;
    assert_eq!(errors().await, 2, "initialization is still gated, not failed yet");
    worker.stop().await;
    std::fs::remove_file(failed.path().join("init.sock")).unwrap();
    drop(gate);
    drop(pending);
    assert!(failed.path().join("fail-reset").exists());
    let worker = WorkerProcess::start(state.path()).await;
    eventually("interrupted initialization to acquire a durable fail-closed receipt", async || (errors().await == 3).then_some(())).await;
    for rejected in [&crashed_retry, &crashed_companion] {
        eventually("interrupted initialization to reject both old retry authorizations durably", async || {
            journal.command_rejection(rejected).await.unwrap()?;
            let state: String = sqlx::query_scalar("SELECT state FROM commands WHERE id = ?")
                .bind(rejected)
                .fetch_one(head.state.store.pool())
                .await
                .unwrap();
            (state == "rejected").then_some(())
        })
        .await;
    }
    api.barrier(&failed_id).await;
    assert_eq!(errors().await, 3, "the interrupted attempt must consume its old retry authorizations");
    assert_eq!(initializations(failed.path()), before_crashed_retry + 1, "one gated initialization, never an automatic fork/fresh retry");
    let (crash_failed, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
    assert_eq!(crash_failed.command_id, reset.command_id);
    assert_eq!(applied, 0);
    assert!(!crash_failed.untouched && !crash_failed.failure.is_empty());
    assert!(crash_failed.retries.is_empty(), "failed recovery must consume all durable retry authorizations");
    assert!(journal.is_applied(&crashed_retry).await.unwrap());
    assert!(journal.is_applied(&crashed_companion).await.unwrap());
    assert!(!journal.is_applied(&reset.command_id).await.unwrap());
    let crash_failed_trace = trace(failed.path());
    worker.stop().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&failed_id, "AFTER_CRASHED_RETRY_RESTART").await;
    api.queued(&failed_id, "AFTER_CRASHED_RETRY_RESTART").await;
    api.say(&sentinel_id, "CHECK_CRASHED_RETRY_RESTART").await;
    api.idle(&sentinel_id, 4).await;
    api.barrier(&failed_id).await;
    assert_eq!(errors().await, 3);
    assert_eq!(trace(failed.path()), crash_failed_trace, "the rejected crashed retry must not initialize on another restart or input");
    assert_eq!(rewinds(&api.events(&failed_id).await), 0);
    assert_eq!(records(failed.path(), "prompt").len(), 2);
    assert!(journal.command_rejection(&crashed_retry).await.unwrap().is_some(), "the crashed retry's rejection must survive restart");
    assert!(journal.command_rejection(&crashed_companion).await.unwrap().is_some(), "the companion retry's rejection must survive restart");

    let queued_texts = [
        "MUST_NOT_REACH_FAILED_RESET",
        "MUST_NOT_LEAK_AFTER_RESTART",
        "AFTER_FAILED_EXPLICIT_RETRY_RESTART",
        "AFTER_CRASHED_RETRY_RESTART",
    ];
    // These inputs must already belong to Inbox before recovery begins.
    // Inputs sent while initialization is gated would instead remain in rx,
    // behind C, and miss the actor-exit race this regression must exercise.
    api.barrier(&failed_id).await;
    let queued = api.detail(&failed_id).await.queued;
    assert_eq!(queued.len(), queued_texts.len());
    assert!(queued.iter().all(|q| queued_texts.contains(&q.text.as_str()) && q.rejected.is_none()));

    // B authorizes recovery of failed A; C is admitted while A initializes
    // for B. All three receipts must be confirmed before releasing Inbox.
    // Confirming A then separately resetting B used to lose durable C, start
    // an Inbox turn, and exit when C found no pending intent to interrupt.
    let gate = UnixListener::bind(failed.path().join("init.sock")).unwrap();
    std::fs::remove_file(failed.path().join("fail-reset")).unwrap();
    let before_overlapping_retries = initializations(failed.path());
    api.rewind(&failed_id, failed_to).await;
    let (mut pending, _) = within("B recovery to wait at initialization before admitting C", gate.accept()).await.unwrap();
    let mut signal = [0; 13];
    within("B recovery's initialization barrier", pending.read_exact(&mut signal)).await.unwrap();
    assert_eq!(&signal, b"initializing\n");
    let (recovering, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
    assert_eq!(recovering.command_id, reset.command_id);
    assert_eq!(applied, 0);
    assert_eq!(recovering.retries.len(), 1);
    let retry_b = recovering.retries[0].clone();
    api.rewind(&failed_id, failed_to).await;
    let retry_c = eventually("C to be durably authorized while B recovery is still gated", async || {
        let (pending, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
        assert_eq!(pending.command_id, reset.command_id);
        assert_eq!(applied, 0);
        (pending.retries.len() == 2 && pending.retries.contains(&retry_b))
            .then(|| pending.retries.into_iter().find(|id| id != &retry_b).unwrap())
    })
    .await;
    // An unrelated actor's barrier also proves the serialized manager has
    // finished dispatching C into this actor's rx, not merely committed it.
    api.barrier(&sentinel_id).await;
    let overlap_ids = [reset.command_id.clone(), retry_b, retry_c];
    for id in &overlap_ids {
        assert!(!journal.is_applied(id).await.unwrap(), "receipt {id} must remain pending while initialization is gated");
    }
    assert_eq!(records(failed.path(), "prompt").len(), 2, "the pre-existing Inbox must remain blocked during initialization");
    assert_eq!(rewinds(&api.events(&failed_id).await), 0);
    std::fs::remove_file(failed.path().join("init.sock")).unwrap();
    drop(gate);
    pending.write_all(b"ok").await.unwrap();
    api.rewound(&failed_id, 3).await;
    for id in &overlap_ids {
        assert!(journal.is_applied(id).await.unwrap(), "each overlapping rewind receipt must be durably confirmed");
        assert!(journal.command_rejection(id).await.unwrap().is_none());
        eventually("the head to retire every overlapping rewind receipt", async || {
            let state: String =
                sqlx::query_scalar("SELECT state FROM commands WHERE id = ?").bind(id).fetch_one(head.state.store.pool()).await.unwrap();
            (state == "done").then_some(())
        })
        .await;
    }
    let raw = journal.events_after(&failed_id, 0, u32::MAX).await.unwrap();
    let mut confirmed_ids: Vec<_> = raw
        .iter()
        .filter_map(|event| match &event.kind {
            Some(c6_proto::session_event::Kind::Rewound(r)) => Some(r.command_id.clone()),
            _ => None,
        })
        .collect();
    confirmed_ids.sort();
    let mut expected_ids = overlap_ids.to_vec();
    expected_ids.sort();
    assert_eq!(confirmed_ids, expected_ids, "A, B, and C each need their own confirmation event");
    let recovered = eventually("all pre-existing Inbox inputs to be delivered exactly once after A, B, and C confirmations", async || {
        let events = api.events(&failed_id).await;
        let prompts = records(failed.path(), "prompt");
        for text in queued_texts {
            let users = events.iter().filter(|e| matches!(&e.kind, EventKind::User { text: t, .. } if t == text)).count();
            let taken = prompts.iter().filter(|p| p["value"]["message"]["content"].as_str().is_some_and(|t| t.contains(text))).count();
            assert!(users <= 1 && taken <= 1, "queued input {text} was delivered more than once");
            if users != 1 || taken != 1 {
                return None;
            }
        }
        let detail = api.detail(&failed_id).await;
        (detail.queued.is_empty() && detail.session.status == SessionStatus::Idle).then_some(events)
    })
    .await;
    let confirmed = recovered.iter().rev().find(|e| matches!(e.kind, EventKind::Rewound { .. })).unwrap().seq;
    for text in queued_texts {
        assert!(user_seq(&recovered, text) > confirmed, "queued input must follow every overlapping retry confirmation");
    }
    assert_eq!(errors().await, 3, "successful overlapping retries must not repeat any rejected retry's failed attempt");
    assert_eq!(initializations(failed.path()), before_overlapping_retries + 1, "A, B, and C must share one successful recovery");
    assert_eq!(records(failed.path(), "prompt").len(), 6);
    for prompt in &records(failed.path(), "prompt")[2..] {
        clean_context(prompt);
    }
    api.barrier(&failed_id).await;
    assert_eq!(errors().await, 3, "the actor must remain live after handling C and draining its Inbox");
    for rejected in [&rejected_retry, &crashed_retry, &crashed_companion] {
        let rejected_state: String =
            sqlx::query_scalar("SELECT state FROM commands WHERE id = ?").bind(rejected).fetch_one(head.state.store.pool()).await.unwrap();
        assert_eq!(rejected_state, "rejected", "successful retry C must not acknowledge any rejected retry");
    }

    // A separate failed reset has no later inputs queued at the head: the
    // only recovery trigger here must be the explicit Rewind request.
    let retry = tempfile::tempdir().unwrap();
    let (retry_id, retry_to) = seed(&api, retry.path()).await;
    std::fs::write(retry.path().join("fail-reset"), "").unwrap();
    api.rewind(&retry_id, retry_to).await;
    eventually("the explicit-retry fixture to fail its first reset", async || {
        api.events(&retry_id)
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("Rewind could not be confirmed")))
            .then_some(())
    })
    .await;
    let (reset, applied) = journal.claude_rewind(&retry_id).await.unwrap().unwrap();
    assert_eq!(applied, 0);

    // Retry is an explicit HTTP Rewind with a NEW command ID, not an input
    // or another restart. Preserve and confirm the old receipt first, then
    // confirm the new one: neither may remain queued over future input.
    let gate = UnixListener::bind(retry.path().join("init.sock")).unwrap();
    std::fs::remove_file(retry.path().join("fail-reset")).unwrap();
    api.rewind(&retry_id, retry_to).await;
    let (mut pending, _) = within("explicit Rewind retry to initialize", gate.accept()).await.unwrap();
    let mut signal = [0; 13];
    within("the retry initialization barrier", pending.read_exact(&mut signal)).await.unwrap();
    assert_eq!(&signal, b"initializing\n");
    let rewind_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM commands WHERE session_id = ? AND state = 'pending' AND text IS NULL ORDER BY seq")
            .bind(&retry_id)
            .fetch_all(head.state.store.pool())
            .await
            .unwrap();
    assert_eq!(rewind_ids.len(), 2, "both old and explicit-retry Rewind receipts must still be pending");
    assert_eq!(rewind_ids[0], reset.command_id);
    assert_ne!(rewind_ids[0], rewind_ids[1]);
    let (pending_reset, applied) = journal.claude_rewind(&retry_id).await.unwrap().unwrap();
    assert_eq!(pending_reset.command_id, reset.command_id, "retry must not overwrite the unconfirmed old intent");
    assert_eq!(applied, 0);
    assert_eq!(rewinds(&api.events(&retry_id).await), 0);
    assert_eq!(records(retry.path(), "prompt").len(), 2, "retry initialization runs no model");
    std::fs::remove_file(retry.path().join("init.sock")).unwrap();
    drop(gate);
    pending.write_all(b"ok").await.unwrap();
    api.rewound(&retry_id, 2).await;
    for id in &rewind_ids {
        assert!(journal.is_applied(id).await.unwrap(), "worker must retain receipt {id}");
        eventually("the head to retire each Rewind receipt", async || {
            let state: String =
                sqlx::query_scalar("SELECT state FROM commands WHERE id = ?").bind(id).fetch_one(head.state.store.pool()).await.unwrap();
            (state == "done").then_some(())
        })
        .await;
    }
    assert_eq!(records(retry.path(), "prompt").len(), 2, "both confirmations run no model");
    api.say(&retry_id, "AFTER_EXPLICIT_RETRY").await;
    let events = eventually("input after explicit retry to finish", async || {
        let events = api.events(&retry_id).await;
        (events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == 3).then_some(events)
    })
    .await;
    let last_rewind = events.iter().rev().find(|e| matches!(e.kind, EventKind::Rewound { .. })).unwrap().seq;
    assert!(user_seq(&events, "AFTER_EXPLICIT_RETRY") > last_rewind);
    let prompts = records(retry.path(), "prompt");
    assert_eq!(prompts.len(), 3);
    clean_context(prompts.last().unwrap());
    assert!(prompts.last().unwrap()["value"]["message"]["content"].as_str().unwrap().contains("AFTER_EXPLICIT_RETRY"));

    // Head redelivery after another worker restart must not replay either
    // confirmed rewind over the successful post-retry input.
    worker.stop().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&retry_id, "AFTER_RETRY_RESTART").await;
    eventually("post-retry input after worker restart", async || {
        (api.events(&retry_id).await.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == 4).then_some(())
    })
    .await;
    assert_eq!(rewinds(&api.events(&retry_id).await), 2);
    let prompts = records(retry.path(), "prompt");
    assert_eq!(prompts.len(), 4);
    let last = prompts.last().unwrap();
    clean_context(last);
    assert!(last["history"].to_string().contains("AFTER_EXPLICIT_RETRY"), "confirmed receipts must not rewind future input");

    // InputDelivered only means written to the native process, not taken by
    // its agent. Rewind must recover these unconfirmed inputs before stopping
    // that process, including when a failed reset's actor is later lost.
    let mut worker = worker;
    let mut delivered_dirs = Vec::new();
    for restart in [false, true] {
        let held = tempfile::tempdir().unwrap();
        let (held_id, held_to) = seed(&api, held.path()).await;
        api.say(&held_id, "HOLD_TURN").await;
        eventually("the fake to hold the active turn before its result", async || {
            (!records(held.path(), "holding-turn").is_empty()).then_some(())
        })
        .await;
        let texts = ["DELIVERED_FIRST", "DELIVERED_CANCEL_ME", "DELIVERED_THIRD", "DELIVERED_FOURTH"];
        for text in texts {
            api.say(&held_id, text).await;
        }
        let delivered = eventually("all four inputs to be written but not taken by the native agent", async || {
            let detail = api.detail(&held_id).await;
            let queued: Vec<_> = detail.queued.into_iter().filter(|q| texts.contains(&q.text.as_str())).collect();
            (queued.len() == texts.len()
                && queued.iter().all(|q| q.with_agent && q.rejected.is_none())
                && records(held.path(), "held-input").len() == texts.len())
            .then_some(queued)
        })
        .await;
        assert_eq!(delivered.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(), texts);
        let raw = journal.events_after(&held_id, 0, u32::MAX).await.unwrap();
        for input in &delivered {
            assert!(raw.iter().any(|e| matches!(&e.kind,
                Some(c6_proto::session_event::Kind::InputDelivered(d)) if d.command_id == input.command_id)));
            assert!(!raw.iter().any(|e| matches!(&e.kind,
                Some(c6_proto::session_event::Kind::User(u)) if u.command_id == input.command_id)));
        }

        std::fs::write(held.path().join("fail-reset"), "").unwrap();
        api.rewind(&held_id, held_to).await;
        eventually("the held turn's reset to fail after interrupting its original input", async || {
            let events = api.events(&held_id).await;
            events
                .iter()
                .any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("Rewind could not be confirmed")))
                .then_some(())
        })
        .await;
        assert_eq!(records(held.path(), "interrupted-held-turn").len(), 1, "Rewind must interrupt the held turn without taking its queue");
        assert_eq!(records(held.path(), "prompt").len(), 3, "none of the four delivered inputs may be taken before reset confirmation");
        let failed_trace = trace(held.path());
        if restart {
            // This is the persisted shape of an already-stuck queue: head
            // rows say with_agent, but the worker has only InputDelivered,
            // no User receipt, and its previous actor/process has gone.
            worker.stop().await;
            worker = WorkerProcess::start(state.path()).await;
        }
        let sentinel = tempfile::tempdir().unwrap();
        let sentinel_id = api.create(sentinel.path(), "DELIVERED_INPUT_RECOVERY_SENTINEL").await;
        api.idle(&sentinel_id, 1).await;
        assert_eq!(trace(held.path()), failed_trace, "neither actor reload nor queue replay may initialize a failed reset");
        assert_eq!(rewinds(&api.events(&held_id).await), 0);

        // Cancel the middle input, not a suffix. A stale InputDelivered
        // marker must not make the recovered inbox answer permanent 409.
        api.cancel(&held_id, &delivered[1].command_id).await;
        eventually("cancellation to remove only the selected recovered input", async || {
            let detail = api.detail(&held_id).await;
            (detail.queued.len() == 3
                && !detail.queued.iter().any(|q| q.command_id == delivered[1].command_id)
                && [0, 2, 3].iter().all(|&i| detail.queued.iter().any(|q| q.command_id == delivered[i].command_id && q.rejected.is_none())))
            .then_some(())
        })
        .await;
        let blocked = api.events(&held_id).await;
        assert!(!blocked.iter().any(|e| matches!(&e.kind, EventKind::User { command_id, .. }
            if delivered.iter().any(|q| q.command_id == *command_id))));

        std::fs::remove_file(held.path().join("fail-reset")).unwrap();
        api.rewind(&held_id, held_to).await;
        api.rewound(&held_id, 2).await;
        let events = eventually("each remaining recovered input to be taken exactly once and leave the queue", async || {
            let events = api.events(&held_id).await;
            let errors = events.iter().filter(|e| matches!(e.kind, EventKind::Error { .. })).count();
            assert_eq!(errors, 1, "queue recovery must not repeatedly retry the failed reset");
            let prompts = records(held.path(), "prompt");
            for i in [0, 2, 3] {
                let users = events
                    .iter()
                    .filter(|e| {
                        matches!(&e.kind, EventKind::User { command_id, .. }
                    if command_id == &delivered[i].command_id)
                    })
                    .count();
                let taken =
                    prompts.iter().filter(|p| p["value"]["message"]["content"].as_str().is_some_and(|t| t.contains(texts[i]))).count();
                assert!(users <= 1 && taken <= 1, "recovered input {} was delivered more than once", texts[i]);
                if users != 1 || taken != 1 {
                    return None;
                }
            }
            let detail = api.detail(&held_id).await;
            (detail.queued.is_empty() && detail.session.status == SessionStatus::Idle).then_some(events)
        })
        .await;
        let confirmed = events.iter().rev().find(|e| matches!(e.kind, EventKind::Rewound { .. })).unwrap().seq;
        for i in [0, 2, 3] {
            assert!(user_seq(&events, texts[i]) > confirmed, "recovered input must follow both reset confirmations");
        }
        assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id == &delivered[1].command_id)));
        let prompts = records(held.path(), "prompt");
        assert_eq!(prompts.len(), 6);
        for prompt in &prompts[3..] {
            clean_context(prompt);
            assert!(!prompt["value"]["message"]["content"].as_str().unwrap().contains("DELIVERED_CANCEL_ME"));
            assert!(!prompt["history"].to_string().contains("DELIVERED_CANCEL_ME"));
        }
        delivered_dirs.extend([held, sentinel]);
    }

    worker.stop().await;
    head.shutdown().await.unwrap();
    for prompt in fork_prompts {
        clean_context(&prompt);
    }
}
