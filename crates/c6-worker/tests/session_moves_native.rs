//! One standalone test owns the process-wide Claude executable and config
//! directory. No model or real Claude project is accessed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use c6_api::{ContentBlock, Event, EventKind, Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::{eventually, within};
use c6_worker::{ConnState, Extension, Identity, Worker, conn, enroll};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};
use uuid::Uuid;

fn fake_claude(dir: &Path) -> PathBuf {
    let bin = dir.join("claude");
    std::fs::write(
        &bin,
        r#"#!/usr/bin/env python3
import json, os, sys, uuid
from pathlib import Path

if '--version' in sys.argv:
    print('fake Claude move CLI')
    sys.exit(0)

args = sys.argv[1:]
resume = args[args.index('--resume') + 1] if '--resume' in args else None
# Like the real CLI, the stream can report a native ID different from the
# initial ID supplied by carbon6. Moving must retain the reported ID.
sid = resume or str(uuid.uuid4())
config = Path(os.environ['CLAUDE_CONFIG_DIR'])
cwd = os.getcwd()
project = ''.join(c if c.isascii() and c.isalnum() else '-' for c in cwd)
transcript = config / 'projects' / project / (sid + '.jsonl')

def append(path, value):
    with path.open('a') as stream:
        stream.write(json.dumps(value) + '\n')

def emit(value):
    print(json.dumps(dict(session_id=sid, **value)), flush=True)

append(config / 'launches.jsonl', dict(cwd=cwd, sid=sid, resume=resume, args=args))
if resume and not transcript.is_file():
    print('No conversation found with session ID', file=sys.stderr)
    sys.exit(1)
transcript.parent.mkdir(parents=True, exist_ok=True)
for line in sys.stdin:
    event = json.loads(line)
    if event['type'] == 'control_request':
        emit(dict(type='control_response', response=dict(subtype='success', request_id=event['request_id'])))
        continue
    assert event['type'] == 'user', event
    text = event['message']['content']
    assert isinstance(text, str), event
    if 'OPENING_NATIVE_MOVE' in text:
        # Never emit the private marker into the Carbon6 transcript. The
        # continuation can produce its answer only from the copied JSONL.
        answer = 'Native move prepared.'
        private_marker = 'private-only-in-native-jsonl'
    else:
        assert 'CONTINUE_NATIVE_MOVE' in text, text
        assert resume, 'continuation started a new conversation'
        previous = [json.loads(row) for row in transcript.read_text().splitlines()]
        assert any(row.get('fixture_marker') == 'private-only-in-native-jsonl' for row in previous), previous
        answer = 'Resumed with the private native history.'
        private_marker = None
    user = dict(type='user', uuid=event['uuid'], message=dict(role='user', content=text), parent_tool_use_id=None)
    assistant = dict(type='assistant', uuid=str(uuid.uuid4()), parent_tool_use_id=None,
                     message=dict(id='msg-' + str(uuid.uuid4()), content=[dict(type='text', text=answer)], stop_reason='end_turn'))
    append(transcript, dict(sessionId=sid, cwd=cwd, fixture_marker=private_marker, **user))
    append(transcript, dict(sessionId=sid, cwd=cwd, **assistant))
    emit(dict(type='system', subtype='init'))
    emit(dict(isReplay=True, **user))
    emit(assistant)
    emit(dict(type='result', uuid=str(uuid.uuid4()), subtype='success', is_error=False,
              usage=dict(input_tokens=1, output_tokens=1)))
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Held Claude processes deliberately survive worker drop. Use production's
/// ownership checks to stop only processes belonging to these fixture states.
struct HeldCleanup(Vec<PathBuf>);

impl Drop for HeldCleanup {
    fn drop(&mut self) {
        let states = std::mem::take(&mut self.0);
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            runtime.block_on(async {
                for state in states {
                    let held = state.join("claude/held");
                    let Ok(entries) = std::fs::read_dir(&held) else { continue };
                    let sessions: std::collections::HashSet<_> = entries
                        .flatten()
                        .filter_map(|entry| {
                            let meta: Value = serde_json::from_slice(&std::fs::read(entry.path().join("meta.json")).ok()?).ok()?;
                            Some(meta["session"].as_str()?.to_owned())
                        })
                        .collect();
                    let Ok(journal) = c6_worker::journal::Journal::open(&state.join("journal.db")).await else { continue };
                    for session in sessions {
                        let _ = c6_worker::claude_code::stop_held_for_rewind(&held, &session, &journal).await;
                    }
                }
            });
        })
        .join();
    }
}

struct RemoveCliAtPrepare {
    armed: AtomicBool,
    bin: PathBuf,
    parked: PathBuf,
}

#[async_trait::async_trait]
impl Extension for RemoveCliAtPrepare {
    fn capabilities(&self) -> Vec<String> {
        vec![]
    }

    async fn handle(&self, method: &str, _: Value) -> Option<anyhow::Result<Value>> {
        if method == "move.prepare"
            && self.armed.swap(false, Ordering::SeqCst)
            && let Err(error) = std::fs::rename(&self.bin, &self.parked)
        {
            return Some(Err(error.into()));
        }
        // Let the real move handler check runtime availability.
        None
    }
}

async fn attach(head: &c6_head::Head, state: &Path, name: &str, extension: Option<Arc<dyn Extension>>) -> (Uuid, DropGuard) {
    let enrollment = c6_head::api::new_enrollment(&head.state, Some(name)).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state).unwrap();
    let worker = Worker::open(state, Identity::load(state).unwrap()).await.unwrap();
    if let Some(extension) = extension {
        worker.set_extension(extension);
    }
    {
        let mut overrides = worker.overrides.lock().unwrap();
        overrides.import_home = Some(state.join("empty-home"));
        overrides.kit_jobs = Some(false);
    }
    let id = worker.identity.meta.worker_id.0;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker, tx, stop.clone()));
    within("the native move worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (id, stop.drop_guard())
}

struct Api {
    http: reqwest::Client,
    base: String,
}

impl Api {
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.http.get(format!("{}{path}", self.base)).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value, status: StatusCode) -> Value {
        let mut request = self.http.request(Method::POST, format!("{}{path}", self.base)).json(&body);
        if path == "/connections" {
            request = request.header("x-c6-realm", "admin");
        }
        let response = request.send().await.unwrap();
        let actual = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(actual, status, "POST {path}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn idle(&self, path: &str, turns: usize) -> (Session, Vec<Event>) {
        eventually("the Claude turn to complete and its queue to clear", async || {
            let detail: SessionDetail = self.get(path).await;
            let events: Vec<Event> = self.get(&format!("{path}/events")).await;
            let reasons: Vec<_> = events
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::TurnEnded { reason, .. } => Some(*reason),
                    _ => None,
                })
                .collect();
            if reasons.len() < turns || detail.session.status != SessionStatus::Idle || !detail.queued.is_empty() {
                return None;
            }
            assert_eq!(reasons, vec![TurnEndReason::Completed; turns], "{events:?}");
            Some((detail.session, events))
        })
        .await
    }

    async fn move_to(&self, placement: &str, target: Uuid) -> String {
        let places: Value = self.get(placement).await;
        let receipt = self
            .post(
                placement,
                json!({"kind": "worker", "expected_place_seq": places[0]["seq"], "worker": target, "clone": false}),
                StatusCode::ACCEPTED,
            )
            .await;
        receipt["command_id"].as_str().or_else(|| receipt["move_id"].as_str()).expect("placement receipt").to_owned()
    }
}

fn mapped_transcript(config: &Path, cwd: &str, native: &str) -> PathBuf {
    let project: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    config.join("projects").join(project).join(format!("{native}.jsonl"))
}

fn launches(config: &Path) -> Vec<Value> {
    std::fs::read_to_string(config.join("launches.jsonl")).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

async fn rejected_move(head: &c6_head::Head, command: &str, stage: &str, freeze_started: bool) {
    eventually("move refusal and destination cleanup", async || {
        let (state, error, progress): (String, Option<String>, String) =
            sqlx::query_as("SELECT state, error, result_json FROM session_moves WHERE id = ?")
                .bind(command)
                .fetch_one(head.state.store.pool())
                .await
                .unwrap();
        assert_ne!(state, "done", "a destination with no Claude CLI became the owner");
        if state != "failed" {
            return None;
        }
        let error = error.unwrap();
        assert!(error.contains(stage) && error.contains("destination Claude Code CLI is unavailable"), "{error}");
        let progress: Value = serde_json::from_str(&progress).unwrap();
        assert_eq!(progress["freeze_started"], freeze_started);
        (progress["target_clean"] == true).then_some(())
    })
    .await;
}

#[tokio::test]
async fn moving_claude_copies_the_native_transcript_and_resumes_its_id() {
    if !c6_testkit::have("python3") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let bin = fake_claude(&root);
    let config = root.join("claude-config");
    std::fs::create_dir(&config).unwrap();
    // SAFETY: single test in this standalone binary, before spawning tasks.
    unsafe {
        std::env::set_var("C6_CLAUDE_BIN", &bin);
        std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    }
    c6_testkit::runnable(&bin, &["--version"]).await;
    let head = c6_head::start(HeadConfig {
        data_dir: root.join("head"),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let source = root.join("source");
    let target = root.join("target");
    let _cleanup = HeldCleanup(vec![source.clone(), target.clone()]);
    let gate = Arc::new(RemoveCliAtPrepare { armed: AtomicBool::new(false), bin: bin.clone(), parked: root.join("claude-parked") });
    let (source_id, _source_stop) = attach(&head, &source, "native-source", None).await;
    let (target_id, _target_stop) = attach(&head, &target, "native-target", Some(gate.clone())).await;
    let api = Api {
        http: reqwest::Client::builder()
            .default_headers([(reqwest::header::HeaderName::from_static("x-c6-realm"), "home".parse().unwrap())].into_iter().collect())
            .build()
            .unwrap(),
        base: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()),
    };
    let cc = api.post("/connections", json!({"kind": "claude_code", "name": "fake move", "base_url": ""}), StatusCode::OK).await;
    let session = api
        .post(
            "/sessions",
            json!({"worker_id": source_id, "connection_id": cc["id"], "model": "opus", "prompt": "OPENING_NATIVE_MOVE"}),
            StatusCode::OK,
        )
        .await;
    let id = session["id"].as_str().unwrap();
    let path = format!("/sessions/{id}");
    let (before, _) = api.idle(&path, 1).await;
    assert_eq!(before.worker_id, source_id);
    let launch = launches(&config);
    assert_eq!(launch.len(), 1, "{launch:?}");
    let native = launch[0]["sid"].as_str().unwrap();
    assert_ne!(native, id);
    let source_jsonl = mapped_transcript(&config, &before.cwd, native);
    let original = std::fs::read(&source_jsonl).unwrap();
    assert!(String::from_utf8_lossy(&original).contains("private-only-in-native-jsonl"));

    let placement = format!("{path}/placement");
    std::fs::rename(&bin, &gate.parked).unwrap();
    let command = api.move_to(&placement, target_id).await;
    rejected_move(&head, &command, "move.preflight", false).await;
    std::fs::rename(&gate.parked, &bin).unwrap();

    gate.armed.store(true, Ordering::SeqCst);
    let command = api.move_to(&placement, target_id).await;
    rejected_move(&head, &command, "move.prepare", true).await;
    std::fs::rename(&gate.parked, &bin).unwrap();
    let (rolled_back, events) = api.idle(&path, 1).await;
    assert_eq!(rolled_back.worker_id, source_id);
    assert!(!events.iter().any(|e| matches!(e.kind, EventKind::PlaceChanged { .. })));
    assert_eq!(launches(&config).len(), 1, "availability checks did not launch a conversation");

    let command = api.move_to(&placement, target_id).await;
    eventually("native move activation", async || {
        let outcome: (String, Option<String>) = sqlx::query_as("SELECT state, error FROM session_moves WHERE id = ?")
            .bind(&command)
            .fetch_one(head.state.store.pool())
            .await
            .unwrap();
        assert_ne!(outcome.0, "failed", "native move refused: {:?}", outcome.1);
        (outcome.0 == "done").then_some(())
    })
    .await;
    let (after, events) = api.idle(&path, 1).await;
    assert_eq!(after.id, id);
    assert_eq!(after.worker_id, target_id);
    assert!(Path::new(&after.cwd).starts_with(&target), "{}", after.cwd);
    let moved = events
        .iter()
        .find(|e| {
            matches!(&e.kind, EventKind::PlaceChanged { command_id, from, to, .. }
        if command_id == &command && from.cwd == before.cwd && to.cwd == after.cwd)
        })
        .expect("native move event");
    let target_jsonl = mapped_transcript(&config, &after.cwd, native);
    assert_ne!(target_jsonl, source_jsonl);
    assert_eq!(std::fs::read(&target_jsonl).unwrap(), original, "destination transcript before the resumed turn");
    assert_eq!(std::fs::read(&source_jsonl).unwrap(), original, "source transcript was retained");

    api.post(&format!("{path}/messages"), json!({"text": "CONTINUE_NATIVE_MOVE"}), StatusCode::OK).await;
    let (continued, events) = api.idle(&path, 2).await;
    assert_eq!(continued.worker_id, target_id);
    assert!(events.iter().any(|e| e.seq > moved.seq
        && matches!(&e.kind, EventKind::Assistant { blocks, .. }
        if blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "Resumed with the private native history.")))));
    let launches = launches(&config);
    let resumed = launches.iter().find(|launch| launch["cwd"] == after.cwd).expect("Claude launched in the destination");
    assert_eq!(resumed["resume"], native, "destination launched with --resume and the source's native ID");
    assert!(!resumed["args"].as_array().unwrap().iter().any(|arg| arg == "--fork-session"));
    assert_eq!(std::fs::read(&source_jsonl).unwrap(), original);
    let target_bytes = std::fs::read(&target_jsonl).unwrap();
    assert!(target_bytes.starts_with(&original));
    assert!(String::from_utf8_lossy(&target_bytes[original.len()..]).contains("CONTINUE_NATIVE_MOVE"));
}
