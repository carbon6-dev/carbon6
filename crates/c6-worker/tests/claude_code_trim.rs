//! Quiet-boundary maintenance of Claude Code's own transcript, not a rebuilt
//! carbon6 projection. One standalone test owns both CLI environment variables.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use c6_api::{Event, EventKind, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually, within};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// The fake writes a real-shaped native JSONL and separately streams the full
/// outputs. Its trace and resumed snapshots are independent of carbon6's log.
/// There are no timing gates: result/TurnEnded and the next input are barriers.
const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, os, re, socket, sys, urllib.request, uuid
from pathlib import Path
if '--version' in sys.argv:
    print('fake Claude trim CLI'); sys.exit(0)
args = sys.argv[1:]
def arg(name):
    return args[args.index(name) + 1] if name in args else None
config = json.loads(Path('fixture.json').read_text()) if Path('fixture.json').exists() else {}
resume = arg('--resume')
cutoff, chosen = arg('--resume-session-at'), arg('--session-id')
# The stream-reported native ID, not carbon6's proposed --session-id, wins.
sid = chosen if cutoff else (resume or str(uuid.uuid4()))
assert sid
root = Path(os.environ['CLAUDE_CONFIG_DIR']) / 'projects' / re.sub('[^A-Za-z0-9]', '-', os.getcwd())
root.mkdir(parents=True, exist_ok=True)
native = root / (sid + '.jsonl')
def trace(**value):
    with open('trace.jsonl', 'a') as f:
        f.write(json.dumps(value) + '\n')
def emit(value):
    print(json.dumps(dict(session_id=sid, parent_tool_use_id=None, **value)), flush=True)
def emit_together(values):
    # A single file write makes every trailing byte visible before carbon6
    # can observe the first authoritative idle edge. Never a timing sleep.
    payload = ''.join(json.dumps(dict(session_id=sid, parent_tool_use_id=None, **value)) + '\n'
                      for value in values).encode()
    sys.stdout.flush()
    assert os.write(sys.stdout.fileno(), payload) == len(payload)
def mcp(method, params, request_id):
    servers = json.loads(arg('--mcp-config'))
    url = servers['mcpServers']['carbon6']['url']
    data = json.dumps(dict(jsonrpc='2.0', id=request_id, method=method, params=params)).encode()
    request = urllib.request.Request(url, data=data,
        headers={'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream'})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request) as response:
        return json.load(response)
def message(kind, content, uid=None, **extra):
    return dict(type=kind, uuid=uid or str(uuid.uuid4()), sessionId=sid,
                parentUuid=None, isSidechain=False,
                message=dict(role=kind, content=content), **extra)
def write(records):
    with native.open('a') as f:
        for record in records:
            f.write(json.dumps(record) + '\n')
if cutoff:
    assert resume and chosen and chosen != resume and '--fork-session' in args
    source = root / (resume + '.jsonl')
    rows = source.read_bytes().splitlines(keepends=True)
    end = next(i for i, row in enumerate(rows) if json.loads(row).get('uuid') == cutoff)
    assert json.loads(rows[end])['type'] == 'assistant'
    native.write_bytes(b''.join(rows[:end + 1]))
    Path('forked.jsonl').write_bytes(native.read_bytes())
elif resume:
    assert native.exists(), native
    Path('resumed.jsonl').write_bytes(native.read_bytes())
trace(kind='launch', args=args, sid=sid, resume=resume, cutoff=cutoff, native=str(native))
seeded = resume is not None
for line in sys.stdin:
    value = json.loads(line)
    if value['type'] == 'control_request':
        emit(dict(type='control_response', response=dict(subtype='success', request_id=value['request_id'])))
        continue
    assert value['type'] == 'user', value
    text = value['message']['content']
    trace(kind='input', sid=sid, text=text)
    emit(dict(type='system', subtype='init'))
    emit(dict(type='user', uuid=value['uuid'], isReplay=True,
              message=dict(role='user', content=text)))
    if text.endswith('FINISH_NATIVE'):
        emit(dict(type='system', subtype='task_notification', task_id='native-task', run_id='run-1',
                  status='completed', summary='Native work is finished.'))
    if not seeded:
        # A compacted prefix must survive byte-for-byte, even though its tool
        # output would otherwise be eligible. Only the latest live suffix counts.
        prefix = [message('assistant', [dict(type='tool_use', id='prefix-call', name='Bash', input=dict(command='old'))]),
                  message('user', [dict(type='tool_result', tool_use_id='prefix-call', content='COMPACTED_PREFIX_' + 'P' * 30000)]),
                  dict(type='system', subtype='compact_boundary', uuid='compact-boundary',
                       compactMetadata=dict(trigger='auto', preTokens=700000, opaque='preserve-me'))]
        instruction = message('user', 'USER_INSTRUCTIONS_KEEP_ME', uid=value['uuid'])
        records = prefix + [instruction]
        for i in range(22):
            call = dict(type='tool_use', id='call-' + str(i), name='Bash', input=dict(command='read fixture-' + str(i)))
            # First five after an instruction and latest eight are protected.
            # In the low-saving case those protected outputs dominate context.
            size = (450 if 5 <= i < 14 else 30000) if config.get('small_saving') else (32000 if 5 <= i < 14 else 700)
            output = 'FULL_OUTPUT_' + str(i) + '_' + 'x' * size
            assistant = message('assistant', [call], uid='assistant-' + str(i))
            result = message('user', [dict(type='tool_result', tool_use_id=call['id'], content=output, is_error=(i == 7))],
                             uid='result-' + str(i), toolUseResult=dict(stdout=output, stderr='', interrupted=False))
            records += [assistant, result]
        write(records)
        Path('original.jsonl').write_bytes(native.read_bytes())
        for record in records[4:]:
            # The native sidecar is not part of stream-json's result shape.
            event = dict(type=record['type'], uuid=record['uuid'], message=record['message'])
            if event['type'] == 'assistant':
                event['message'] = dict(event['message'], id=record['uuid'], stop_reason='tool_use')
            emit(event)
        if config.get('background'):
            emit(dict(type='system', subtype='task_started', task_id='native-task', run_id='run-1',
                      description='Still doing native work', task_type='local_bash', is_backgrounded=True))
        if config.get('idle_native'):
            emit(dict(type='system', subtype='task_started', task_id='owned-native', run_id='owned-run',
                      description='A subagent owns this native task', task_type='local_bash',
                      is_backgrounded=True, owned_by_subagent=True))
        if config.get('carbon_held'):
            mcp('initialize', dict(protocolVersion='2025-06-18', capabilities={},
                clientInfo=dict(name='trim-fixture', version='0')), 1)
            task = dict(description='Carbon-held trim probe', prompt='CARBON_HELD_TRIM',
                        connection='trim-api', model='m', run_in_background=True)
            emit(dict(type='assistant', message=dict(content=[dict(type='tool_use', id='carbon-held-call',
                name='mcp__carbon6__Task', input=task)], stop_reason='tool_use')))
            response = mcp('tools/call', dict(name='Task', arguments=task), 2)
            assert not response.get('error') and not response.get('result', {}).get('isError'), response
            Path('carbon-task.json').write_text(json.dumps(response))
            emit(dict(type='user', message=dict(content=[dict(type='tool_result', tool_use_id='carbon-held-call',
                content=response['result']['content'][0]['text'])])))
        seeded = True
    answer = message('assistant', [dict(type='text', text='finished ' + text)])
    answer['message'].update(id=answer['uuid'], model='claude-opus-5-5', stop_reason='end_turn',
                             usage=dict(input_tokens=config.get('tokens', 600000), output_tokens=1))
    write([answer])
    emit(dict(type='assistant', uuid=answer['uuid'], message=answer['message']))
    result = dict(type='result', subtype='success', is_error=False,
                  usage=dict(input_tokens=config.get('tokens', 600000), output_tokens=1))
    if config.get('trailing_result'):
        # Flush the actual assistant call's usage before the idle edge, so
        # unread result bytes (not absent usage) block immediate maintenance.
        emit(dict(type='user', message=dict(role='user', content=[])))
        emit_together([dict(type='system', subtype='session_state_changed', state='idle'), result])
    else:
        emit(result)
    if config.get('idle_native') and not resume:
        # The test releases this fixture-only socket AFTER observing carbon6
        # idle with a running native task. No user input drives completion.
        with socket.socket(socket.AF_UNIX) as gate:
            gate.connect('native-gate.sock')
            gate.sendall(b'ready\n')
            assert gate.recv(2) == b'go'
        if config['idle_native'] == 'notification':
            terminal = dict(type='system', subtype='task_notification', task_id='owned-native', run_id='owned-run',
                            status='completed', summary='Owned native task finished without waking the parent.')
        else:
            terminal = dict(type='system', subtype='task_updated', task_id='owned-native', run_id='owned-run',
                            patch=dict(status='completed'))
        emit_together([terminal, dict(type='system', subtype='session_state_changed', state='idle')])
"#;

fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    std::fs::write(&path, FAKE_CLAUDE).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Reap only fixture-owned held groups, including when an assertion panics.
/// Use production's ownership check, rather than killing a stale numeric pgid.
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

async fn run_worker(state: &Path) -> (Arc<Worker>, CancellationToken) {
    let worker =
        Worker::open_with_resource_sampler(state, Identity::load(state).unwrap(), c6_worker::ResourceMeasurement::default).await.unwrap();
    {
        let mut overrides = worker.overrides.lock().unwrap();
        overrides.import_home = Some(state.join("empty-home"));
        overrides.kit_jobs = Some(false);
    }
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    within("the trim worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (worker, stop)
}

struct Api {
    http: reqwest::Client,
    url: String,
}

impl Api {
    async fn post(&self, path: &str, body: Value) -> Value {
        let response = self.http.post(format!("{}{path}", self.url)).json(&body).send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "POST {path}: {status}: {text}");
        if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap() }
    }

    async fn page_post(&self, path: &str, body: Value) {
        // Sign-in is off, but this is still the UI's explicit page context,
        // not an automation token claiming human_origin in its JSON body.
        let response = self.http.post(format!("{}{path}", self.url)).header("x-c6-realm", "home").json(&body).send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "page POST {path}: {status}: {text}");
    }

    async fn limits(&self, enabled: bool, at: u64) {
        // JSON rather than a Rust struct: exercise the public settings wire
        // format and do not depend on unrelated optional limits fields.
        let response = self
            .http
            .put(format!("{}/settings/subagent-limits", self.url))
            .json(&json!({"claude_code_trim_enabled": enabled, "claude_code_trim_at": at, "claude_code_idle_minutes": 60}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "set trim limits: {status}: {text}");
        let value: Value = self
            .http
            .get(format!("{}/settings/subagent-limits", self.url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["claude_code_trim_enabled"], enabled);
        assert_eq!(value["claude_code_trim_at"], at);
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.http.get(format!("{}/sessions/{id}/events", self.url)).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    }

    async fn detail(&self, id: &str) -> SessionDetail {
        self.http.get(format!("{}/sessions/{id}", self.url)).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    }

    async fn idle(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("the trim fixture turn to finish and clear its input", async || {
            let detail: SessionDetail = self.http.get(format!("{}/sessions/{id}", self.url)).send().await.ok()?.json().await.ok()?;
            let events = self.events(id).await;
            let errors: Vec<_> = events.iter().filter(|e| matches!(e.kind, EventKind::Error { .. })).collect();
            assert!(errors.is_empty(), "unexpected trim session errors: {errors:?}");
            (events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns
                && detail.session.status == SessionStatus::Idle
                && detail.queued.is_empty())
            .then_some(events)
        })
        .await
    }
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn launches(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["kind"] == "launch")
        .collect()
}

async fn wait_launches(dir: &Path, count: usize) -> Vec<Value> {
    eventually("the quiet-boundary CLI replacement to launch and snapshot its native input", async || {
        let starts = launches(dir);
        assert!(starts.len() <= count, "unexpected extra CLI launch: {starts:?}");
        (starts.len() == count).then_some(starts)
    })
    .await
}

fn trims(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::Notice { title, .. } if title.starts_with("Trimmed ") && title.contains("Claude Code")))
        .count()
}

fn assert_native_trim(dir: &Path) {
    let original_bytes = std::fs::read(dir.join("original.jsonl")).unwrap();
    let resumed_bytes = std::fs::read(dir.join("resumed.jsonl")).unwrap();
    let prefix_end = original_bytes.iter().enumerate().filter(|(_, byte)| **byte == b'\n').nth(2).unwrap().0 + 1;
    assert!(resumed_bytes.starts_with(&original_bytes[..prefix_end]), "the compacted prefix and boundary are byte-for-byte unchanged");
    let original = records(&dir.join("original.jsonl"));
    let resumed = records(&dir.join("resumed.jsonl"));
    assert_eq!(&resumed[..4], &original[..4], "compacted prefix, boundary metadata and user instructions are untouched");
    for i in 0..22 {
        let call = 4 + i * 2;
        let result = call + 1;
        assert_eq!(resumed[call], original[call], "tool call and arguments {i} are unchanged");
        let mut expected = original[result].clone();
        if (5..14).contains(&i) {
            let stub = resumed[result]["message"]["content"][0]["content"].as_str().unwrap();
            assert!(stub.starts_with('\u{2060}'), "result {i} is a marked trim stub: {stub}");
            assert!(stub.contains(&format!("Bash read fixture-{i}")), "stub retains paired call context: {stub}");
            assert!(!stub.contains("FULL_OUTPUT_"));
            assert_eq!(resumed[result]["toolUseResult"], stub, "the native sidecar cannot retain the large output");
            expected["message"]["content"][0]["content"] = json!(stub);
            expected["toolUseResult"] = json!(stub);
        }
        assert_eq!(resumed[result], expected, "only eligible result content/sidecar {i} may change; IDs, pairings and errors stay");
    }
}

/// Exercise the actor's invocation of maintenance after idle-line handling,
/// not just Process::trim_quiet's eligibility predicate. None of these cases
/// sends another model input to get maintenance unstuck.
async fn idle_maintenance_regressions(api: &Api, worker: &Worker, connection: &Value) {
    api.limits(true, 0).await;
    for (name, fixture) in [
        ("trailing-result-after-authoritative-idle", json!({"trailing_result": true})),
        ("owned-native-notification-while-idle", json!({"idle_native": "notification"})),
        ("owned-native-update-while-idle", json!({"idle_native": "updated"})),
    ] {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("fixture.json"), serde_json::to_vec(&fixture).unwrap()).unwrap();
        let gate = fixture.get("idle_native").map(|_| UnixListener::bind(project.path().join("native-gate.sock")).unwrap());
        let session = api
            .post(
                "/sessions",
                json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(),
                       "connection_id": connection["id"], "model": "default", "prompt": name}),
            )
            .await;
        let id = session["id"].as_str().unwrap();
        let idle = api.idle(id, 1).await;
        if let Some(gate) = gate {
            let (mut signal, _) = within("the idle native task's fixture-only completion gate", gate.accept()).await.unwrap();
            let mut ready = [0; 6];
            within("the native task to reach its explicit gate", signal.read_exact(&mut ready)).await.unwrap();
            assert_eq!(&ready, b"ready\n");
            assert_eq!(launches(project.path()).len(), 1, "{name}: live native work forbids post-turn replacement");
            assert_eq!(trims(&idle), 0);
            assert!(
                idle.iter().any(|e| matches!(&e.kind, EventKind::BackgroundTask { kind, state, .. }
                if kind == "claude_code" && state == "started")),
                "{name}: the native task really is running"
            );
            signal.write_all(b"go").await.unwrap();
        }

        let starts = wait_launches(project.path(), 2).await;
        let events = api.idle(id, 1).await;
        assert_eq!(trims(&events), 1, "{name}: draining the last idle line retries maintenance without a user input");
        assert_native_trim(project.path());
        assert_eq!(starts[1]["resume"], starts[0]["sid"], "{name}: maintenance resumes the same native ID");
        assert!(!starts[1]["args"].as_array().unwrap().iter().any(|arg| arg == "--fork-session"));
        assert_eq!(
            records(&project.path().join("trace.jsonl")).iter().filter(|v| v["kind"] == "input").count(),
            1,
            "{name}: maintenance creates no keep-warm/model input"
        );
        assert_eq!(
            events.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count(),
            1,
            "{name}: no model or notification-only turn starts"
        );
        assert_eq!(
            events.iter().filter(|e| matches!(e.kind, EventKind::User { .. })).count(),
            1,
            "{name}: no user or task-notification input drives the retry"
        );
        if fixture.get("idle_native").is_some() {
            assert!(
                events.iter().any(|e| matches!(&e.kind, EventKind::BackgroundTask { kind, state, .. }
                if kind == "claude_code" && state == "finished")),
                "{name}: task completion is observed before trimming"
            );
        }
        let original = records(&project.path().join("original.jsonl"));
        for i in 5..14 {
            let full = original[5 + i * 2]["message"]["content"][0]["content"].as_str().unwrap();
            assert!(
                events.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { call_id, output, .. }
                if call_id == &format!("call-{i}") && output == full)),
                "{name}: full output {i} remains in carbon6's log"
            );
        }
        c6_worker::claude_code::stop_held_for_rewind(&worker.state_dir.join("claude/held"), id, &worker.journal).await.unwrap();
    }
}

#[tokio::test]
async fn native_context_is_trimmed_only_between_quiet_turns_and_resumed_without_forking() {
    let bins = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    let mock = MockLlm::start().await;
    let carbon_gate = mock.push_gated_about("CARBON_HELD_TRIM", Reply::Text("CARBON_WORK_SURVIVED".into()));
    // SAFETY: this standalone integration binary has exactly one test. Every
    // CLI, transcript and setting belongs to these temporary fixture folders.
    unsafe {
        std::env::set_var("C6_CLAUDE_BIN", &claude);
        std::env::set_var("CLAUDE_CONFIG_DIR", config.path());
    }
    c6_testkit::runnable(&claude, &["--version"]).await;
    let head_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let _cleanup = HeldFixtureCleanup(state.path().to_owned());
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
    let api = Api { http: reqwest::Client::new(), url: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()) };
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let (worker, stop) = run_worker(state.path()).await;
    let connection = api.post("/connections", json!({"kind": "claude_code", "name": "trim fixture", "base_url": ""})).await;
    api.post("/connections", json!({"name": "trim-api", "base_url": mock.base_url(), "default_model": "m"})).await;

    for (name, enabled, at, fixture, should_trim) in [
        ("auto", true, 0, json!({}), true),
        // Auto is min(200k, half the actual 1M window), not half alone.
        ("auto-minimum", true, 0, json!({"tokens": 250000}), true),
        ("disabled", false, 0, json!({}), false),
        ("below-auto", true, 0, json!({"tokens": 100000}), false),
        ("explicit-high", true, 700000, json!({}), false),
        ("explicit-low", true, 100000, json!({"tokens": 150000}), true),
        ("small-saving", true, 0, json!({"small_saving": true}), false),
        ("background", true, 0, json!({"background": true}), false),
        ("carbon-held", true, 0, json!({"carbon_held": true}), true),
    ] {
        api.limits(enabled, at).await;
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("fixture.json"), serde_json::to_vec(&fixture).unwrap()).unwrap();
        let session = api
            .post(
                "/sessions",
                json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(),
                       "connection_id": connection["id"], "model": "default", "prompt": name}),
            )
            .await;
        let id = session["id"].as_str().unwrap();
        api.idle(id, 1).await;
        let initial = wait_launches(project.path(), if should_trim { 2 } else { 1 }).await;
        let first = api.events(id).await;
        assert_eq!(trims(&first), usize::from(should_trim), "{name}: trim immediately when the first turn becomes quiet");
        assert!(
            first.iter().any(|e| matches!(&e.kind, EventKind::Usage { input_tokens, context_window, .. }
            if *input_tokens == fixture["tokens"].as_u64().unwrap_or(600000) && *context_window == 1000000)),
            "actual assistant usage: {name}"
        );
        assert_ne!(initial[0]["sid"], id, "the fixture reports its own native ID");
        if name == "background" {
            assert!(
                first.iter().any(|e| matches!(&e.kind, EventKind::BackgroundTask { kind, state, .. }
                if kind == "claude_code" && state == "started")),
                "the no-restart case really has native work alive"
            );
        }
        let native = PathBuf::from(initial[0]["native"].as_str().unwrap());
        let before = std::fs::read(&native).unwrap();
        assert_eq!(String::from_utf8_lossy(&before).contains('\u{2060}'), should_trim, "first quiet boundary: {name}");
        if should_trim {
            assert_native_trim(project.path());
            assert_eq!(
                records(&project.path().join("trace.jsonl")).iter().filter(|v| v["kind"] == "input").count(),
                1,
                "post-turn replacement starts no keep-warm/model input"
            );
        }
        let carbon_child = if name == "carbon-held" {
            let child = first
                .iter()
                .find_map(|e| match &e.kind {
                    EventKind::SubagentSpawned { call_id, child_session_id, .. } if call_id == "carbon-held-call" => {
                        Some(child_session_id.clone())
                    }
                    _ => None,
                })
                .expect("a real carbon6-owned Task was started over MCP");
            eventually("the carbon6-held child to reach its explicit provider gate", async || {
                mock.requests().iter().any(|request| request.to_string().contains("CARBON_HELD_TRIM")).then_some(())
            })
            .await;
            assert_eq!(
                api.detail(&child).await.session.status,
                SessionStatus::Running,
                "the provider-gated carbon6 task survives parent CLI retirement and immediate trim"
            );
            Some(child)
        } else {
            None
        };

        api.post(&format!("/sessions/{id}/messages"), json!({"text": if name == "background" { "FINISH_NATIVE" } else { "NEXT_INPUT" }}))
            .await;
        api.idle(id, 2).await;
        let trimmed_after_second = should_trim || name == "background";
        let starts = wait_launches(project.path(), if trimmed_after_second { 2 } else { 1 }).await;
        let second = api.events(id).await;
        if name == "background" {
            assert!(
                second.iter().any(|e| matches!(&e.kind, EventKind::BackgroundTask { kind, state, .. }
                if kind == "claude_code" && state == "finished")),
                "explicit native completion unlocks immediate maintenance"
            );
            assert!(
                !second.iter().any(|e| matches!(&e.kind, EventKind::BackgroundTask { kind, state, .. }
                if kind == "claude_code" && matches!(state.as_str(), "lost" | "stopped" | "stopped_by_user"))),
                "maintenance never kills Claude Code's own running work"
            );
        }
        assert_eq!(trims(&second), usize::from(trimmed_after_second), "{name}: maintenance notice");
        let after = std::fs::read(&native).unwrap();
        if trimmed_after_second {
            assert_native_trim(project.path());
            assert_eq!(starts[1]["resume"], starts[0]["sid"], "{name}: preserve the native conversation ID");
            assert_eq!(starts[1]["sid"], starts[0]["sid"]);
            let args = starts[1]["args"].as_array().unwrap();
            assert!(args.iter().any(|arg| arg == "--resume"));
            assert!(!args.iter().any(|arg| arg == "--fork-session" || arg == "--resume-session-at"));
            let original = records(&project.path().join("original.jsonl"));
            for i in 5..14 {
                let full = original[5 + i * 2]["message"]["content"][0]["content"].as_str().unwrap();
                assert!(
                    second.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { call_id, output, .. }
                    if call_id == &format!("call-{i}") && output == full)),
                    "{name}: carbon6 keeps full output {i}"
                );
            }
            // Another high-usage turn cannot trim already marked stubs again.
            if name == "auto" {
                api.page_post(&format!("/sessions/{id}/messages"), json!({"text": "THIRD_INPUT"})).await;
            } else {
                api.post(&format!("/sessions/{id}/messages"), json!({"text": "THIRD_INPUT"})).await;
            }
            let third = api.idle(id, 3).await;
            assert_eq!(launches(project.path()).len(), 2, "{name}: second maintenance is idempotent");
            assert_eq!(trims(&third), 1);
            assert!(std::fs::read(&native).unwrap().starts_with(&after), "{name}: no second rewrite of existing native records");
            if name == "auto" {
                let to_seq = third
                    .iter()
                    .find(|e| {
                        matches!(&e.kind, EventKind::User { text, origin, .. }
                        if text == "THIRD_INPUT" && origin.as_deref() == Some(&c6_api::MessageOrigin::Human))
                    })
                    .expect("the rewind target is a real UI-origin input")
                    .seq;
                let kept_end = second.iter().rev().find(|e| matches!(e.kind, EventKind::TurnEnded { .. })).unwrap().seq;
                let boundary = worker.journal.claude_turn(id, kept_end).await.unwrap().unwrap();
                assert_eq!(boundary.session_id, starts[0]["sid"].as_str().unwrap());
                let source_bytes = std::fs::read(&native).unwrap();
                let source_records = records(&native);
                let cutoff = source_records.iter().position(|v| v["uuid"] == boundary.cutoff_uuid).unwrap();
                let cutoff_bytes = source_bytes.iter().enumerate().filter(|(_, byte)| **byte == b'\n').nth(cutoff).unwrap().0 + 1;
                api.page_post(&format!("/sessions/{id}/rewind"), json!({"to_seq": to_seq, "restore_files": false})).await;
                eventually("rewind to confirm a native fork of the trimmed conversation", async || {
                    api.events(id).await.iter().any(|e| matches!(e.kind, EventKind::Rewound { .. })).then_some(())
                })
                .await;
                let rewind_launches = launches(project.path());
                assert_eq!(rewind_launches.len(), 3, "rewind initializes one no-model native fork");
                let fork = &rewind_launches[2];
                assert_eq!(fork["resume"], starts[0]["sid"], "rewind starts from the same trimmed native ID");
                assert_eq!(fork["cutoff"], boundary.cutoff_uuid);
                assert_ne!(fork["sid"], fork["resume"], "rewind deliberately selects a new branch ID");
                assert!(fork["args"].as_array().unwrap().iter().any(|arg| arg == "--fork-session"));
                let fork_bytes = std::fs::read(project.path().join("forked.jsonl")).unwrap();
                assert_eq!(fork_bytes, source_bytes[..cutoff_bytes], "fork retains the exact already-trimmed prefix through its cutoff");
                let fork_text = String::from_utf8(fork_bytes.clone()).unwrap();
                assert!(fork_text.contains('\u{2060}'), "native rewind retains the trim stubs before its cutoff");
                assert!(!fork_text.contains("THIRD_INPUT"), "the withdrawn assistant suffix is absent from the fork");
                assert_eq!(std::fs::read(&native).unwrap(), source_bytes, "rewind never mutates the trimmed source conversation");

                api.page_post(&format!("/sessions/{id}/messages"), json!({"text": "AFTER_TRIMMED_REWIND"})).await;
                let continued = api.idle(id, 4).await;
                let continued_launches = launches(project.path());
                assert_eq!(continued_launches.len(), 4, "the confirmed branch is promoted to held transport once");
                assert_eq!(continued_launches[3]["resume"], fork["sid"], "future input resumes the confirmed fork");
                assert_eq!(std::fs::read(project.path().join("resumed.jsonl")).unwrap(), fork_bytes);
                assert_eq!(std::fs::read(&native).unwrap(), source_bytes, "continuation still leaves the source untouched");
                assert_eq!(trims(&continued), 1, "rewind and held promotion do not trim marked outputs again");
                for i in 5..14 {
                    let full = original[5 + i * 2]["message"]["content"][0]["content"].as_str().unwrap();
                    assert!(
                        continued.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { call_id, output, .. }
                        if call_id == &format!("call-{i}") && output == full)),
                        "full output {i} remains in carbon6's log after native rewind"
                    );
                }
            }
            if let Some(child) = &carbon_child {
                assert_eq!(
                    api.detail(child).await.session.status,
                    SessionStatus::Running,
                    "carbon6-held work stays alive across parent replacement and further inputs"
                );
                carbon_gate.notify_one();
                let reported = api.idle(id, 4).await;
                assert!(
                    reported.iter().any(|e| matches!(&e.kind, EventKind::User { command, text, .. }
                    if command == "task-notification" && text.contains("CARBON_WORK_SURVIVED"))),
                    "the resumed parent receives the real task's eventual completion report"
                );
                assert_eq!(launches(project.path()).len(), 2, "reporting does not duplicate keep-warm or native trimming");
                assert_eq!(trims(&reported), 1);
            }
        } else {
            assert!(after.starts_with(&before), "{name}: original transcript bytes were not rewritten");
            assert!(!project.path().join("resumed.jsonl").exists(), "{name}: the held process took the next input");
        }
        // End this fixture explicitly while its cwd still exists. The drop
        // guard above is the panic path, not normal process lifecycle control.
        c6_worker::claude_code::stop_held_for_rewind(&state.path().join("claude/held"), id, &worker.journal).await.unwrap();
    }

    idle_maintenance_regressions(&api, &worker, &connection).await;

    // A managed workspace moves with the already-trimmed native JSONL, not
    // a reconstructed projection with the original enormous tool results.
    api.limits(true, 0).await;
    let target_state = tempfile::tempdir().unwrap();
    let _target_cleanup = HeldFixtureCleanup(target_state.path().to_owned());
    let enrollment = c6_head::api::new_enrollment(&head.state, Some("trim-target")).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(target_state.path()).unwrap();
    let (target, stop_target) = run_worker(target_state.path()).await;
    let session = api
        .post(
            "/sessions",
            json!({"worker_id": worker.identity.meta.worker_id.0, "connection_id": connection["id"],
                   "model": "default", "prompt": "MOVE_AFTER_TRIM"}),
        )
        .await;
    let id = session["id"].as_str().unwrap();
    api.idle(id, 1).await;
    let source_cwd = PathBuf::from(api.detail(id).await.session.cwd);
    wait_launches(&source_cwd, 2).await;
    assert_eq!(trims(&api.events(id).await), 1, "the native context is trimmed at the first quiet boundary before move");
    api.post(&format!("/sessions/{id}/messages"), json!({"text": "TRIM_BEFORE_MOVE"})).await;
    let events = api.idle(id, 2).await;
    assert_eq!(trims(&events), 1);
    assert_native_trim(&source_cwd);
    let source_launches = launches(&source_cwd);
    let native_id = source_launches[0]["sid"].as_str().unwrap();
    let source_native = PathBuf::from(source_launches[0]["native"].as_str().unwrap());
    let trimmed = std::fs::read(&source_native).unwrap();
    let placement = format!("{}/sessions/{id}/placement", api.url);
    let places: Value = api.http.get(&placement).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let response = api
        .http
        .post(&placement)
        .json(
            &json!({"kind": "worker", "expected_place_seq": places[0]["seq"], "worker": target.identity.meta.worker_id.0, "clone": false}),
        )
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "move trimmed native context: {body}");
    let receipt: Value = serde_json::from_str(&body).unwrap();
    let command = receipt["command_id"].as_str().or_else(|| receipt["move_id"].as_str()).unwrap();
    eventually("the trimmed conversation move to activate", async || {
        let (state, error): (String, Option<String>) = sqlx::query_as("SELECT state, error FROM session_moves WHERE id = ?")
            .bind(command)
            .fetch_one(head.state.store.pool())
            .await
            .unwrap();
        assert_ne!(state, "failed", "move of trimmed native JSONL failed: {error:?}");
        (state == "done").then_some(())
    })
    .await;
    api.idle(id, 2).await;
    let placed = api.detail(id).await.session;
    assert_eq!(placed.worker_id, target.identity.meta.worker_id.0);
    let target_cwd = PathBuf::from(&placed.cwd);
    assert_ne!(target_cwd, source_cwd);
    let folder: String = placed.cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let target_native = config.path().join("projects").join(folder).join(format!("{native_id}.jsonl"));
    assert_eq!(std::fs::read(&target_native).unwrap(), trimmed, "the move copies the already-trimmed native file exactly");
    assert_eq!(std::fs::read(&source_native).unwrap(), trimmed, "the source transcript is retained");
    api.post(&format!("/sessions/{id}/messages"), json!({"text": "CONTINUE_AFTER_MOVE"})).await;
    let moved_events = api.idle(id, 3).await;
    assert_eq!(trims(&moved_events), 1, "moving a trimmed conversation does not trim it again");
    let moved_launches = launches(&target_cwd);
    let resumed = moved_launches.last().unwrap();
    assert_eq!(resumed["resume"], native_id, "the destination resumes the same trimmed native ID");
    assert_eq!(resumed["sid"], native_id);
    assert!(!resumed["args"].as_array().unwrap().iter().any(|arg| arg == "--fork-session"));
    assert_eq!(std::fs::read(target_cwd.join("resumed.jsonl")).unwrap(), trimmed, "the destination CLI sees the trimmed snapshot");
    c6_worker::claude_code::stop_held_for_rewind(&target_state.path().join("claude/held"), id, &target.journal).await.unwrap();
    stop_target.cancel();
    drop(target);
    stop.cancel();
    drop(worker);
    head.shutdown().await.unwrap();
}
