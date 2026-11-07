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
import json, socket, sys, uuid
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
log('launch', args=args, sid=sid, resume=resume, cutoff=cutoff)
if cutoff and Path('unsupported-cutoff').exists():
    print('unknown option --resume-session-at', file=sys.stderr); sys.exit(2)
history = load(resume) if resume else []
if cutoff:
    assert '--fork-session' in args and chosen and chosen != resume
    history = history[:next(i + 1 for i, e in enumerate(history) if e['uuid'] == cutoff)]
    assert history[-1]['role'] == 'assistant'
initialized = False
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
            emit(dict(type='control_response', response=dict(
                subtype='error' if failed else 'success', request_id=v['request_id'],
                error='fixture refuses reset' if failed else None)))
        continue
    assert v['type'] == 'user', v
    # The normal stream's session_id is authoritative, not carbon6's initial ID.
    if not resume and not initialized and not history:
        sid = str(uuid.uuid4())
    text = v['message']['content']
    assert isinstance(text, str)
    log('prompt', sid=sid, value=v, history=history)
    emit(dict(type='system', subtype='init'))
    emit(dict(type='user', uuid=v['uuid'], isReplay=True,
              message=dict(role='user', content=text), parent_tool_use_id=None))
    history.append(dict(uuid=v['uuid'], role='user', text=text))
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
    stop: oneshot::Sender<()>,
    thread: std::thread::JoinHandle<()>,
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
            // Process::drop kills each fake CLI's process group.
        });
        let id = within("the worker's ready signal", attached).await.unwrap();
        Self { stop, thread, id }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        within("the worker runtime to stop", tokio::task::spawn_blocking(move || self.thread.join().unwrap())).await.unwrap();
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

    async fn idle(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("the completed turn and idle session", async || {
            let d: SessionDetail = self.http.get(format!("{}/sessions/{id}", self.url)).send().await.ok()?.json().await.ok()?;
            let events = self.events(id).await;
            let errors: Vec<_> = events.iter().filter(|e| matches!(e.kind, EventKind::Error { .. })).collect();
            assert!(errors.is_empty(), "unexpected session error: {errors:?}");
            let ended = events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (ended == turns && d.session.status == SessionStatus::Idle && d.queued.is_empty()).then_some(events)
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
    let to_seq = user_seq(&events, "DROP_PRIVATE");
    (id, to_seq)
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
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = WorkerProcess::start(state.path()).await;
    let mut api = Api {
        http: reqwest::Client::new(),
        url: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()),
        worker: worker.id.clone(),
        cc: Value::Null,
    };
    api.cc = api.post("/connections", json!({"kind": "claude_code", "name": "fake rewind", "base_url": ""})).await;

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
    assert_eq!(launches.len(), 2, "A should use the supported native cutoff, not fresh fallback");
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
    assert_eq!(records(first.path(), "launch").len(), 2);
    assert!(records(first.path(), "launch")[1]["resume"].is_null());
    clean_context(&records(first.path(), "prompt").pop().unwrap());

    // A real worker-runtime restart must use the reset conversation, never
    // --resume the abandoned pre-rewind ID (both native A and fallback B).
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
    for (dir, id, turns) in [(a.path(), a_id.as_str(), 4), (b.path(), b_id.as_str(), 4)] {
        let previous_sid = records(dir, "prompt").last().unwrap()["sid"].clone();
        api.say(id, "AFTER_RESTART").await;
        api.idle(id, turns).await;
        let launches = records(dir, "launch");
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
    worker.stop().await;
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", state.path().join("journal.db").display())).await.unwrap();
    for sql in ["DELETE FROM claude_turns WHERE session_id = ?", "DELETE FROM claude_rewinds WHERE session_id = ?"] {
        sqlx::query(sql).bind(&a_id).execute(&pool).await.unwrap();
    }
    pool.close().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&a_id, "AFTER_METADATA_LOSS").await;
    api.idle(&a_id, 5).await;
    let fresh = records(a.path(), "launch").pop().unwrap();
    assert!(fresh["resume"].is_null(), "missing chain metadata requires conservative fresh fallback: {fresh}");
    let after = records(a.path(), "prompt").pop().unwrap();
    clean_context(&after);
    assert_eq!(after["history"], json!([]));
    assert!(after["value"]["message"]["content"].as_str().unwrap().contains("KEEP_TOOL"));

    // Both reset attempts fail: Error, no Rewound, and no later input is
    // delivered, including when the pending reset is recovered on restart.
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
    eventually("the failed reset error", async || (errors().await >= 1).then_some(())).await;
    api.say(&failed_id, "MUST_NOT_REACH_FAILED_RESET").await;
    eventually("a later input to retry and refuse the pending reset", async || (errors().await >= 2).then_some(())).await;
    assert_eq!(rewinds(&api.events(&failed_id).await), 0);
    assert_eq!(records(failed.path(), "prompt").len(), 2);
    let (reset, applied) = journal.claude_rewind(&failed_id).await.unwrap().unwrap();
    assert_eq!(reset.to_seq, failed_to);
    assert_eq!(applied, 0, "failure leaves a durable unconfirmed intent");
    worker.stop().await;
    let worker = WorkerProcess::start(state.path()).await;
    api.say(&failed_id, "MUST_NOT_LEAK_AFTER_RESTART").await;
    eventually("the recovered reset to fail closed", async || (errors().await >= 3).then_some(())).await;
    assert_eq!(rewinds(&api.events(&failed_id).await), 0);
    assert_eq!(records(failed.path(), "prompt").len(), 2);

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

    worker.stop().await;
    head.shutdown().await.unwrap();
    for prompt in fork_prompts {
        clean_context(&prompt);
    }
}
