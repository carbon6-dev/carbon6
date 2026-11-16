//! A real local child report must reach the same held parent after adoption.
#[path = "support/held_worker.rs"]
mod held_worker;

use c6_api::{Event, EventKind};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::enroll;
use serde_json::{Value, json};

#[tokio::test]
async fn a_local_child_report_reaches_an_adopted_idle_claude_parent() {
    if !c6_testkit::have("python3") {
        return;
    }
    let bins = tempfile::tempdir().unwrap();
    let claude = bins.path().join("claude");
    std::fs::write(&claude, r#"#!/usr/bin/env python3
import json, os, sys, urllib.request
from pathlib import Path
if '--version' in sys.argv: sys.exit(0)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
args = sys.argv[1:]
sid = args[args.index('--session-id') + 1] if '--session-id' in args else args[args.index('--resume') + 1]
with open('launches.jsonl', 'a') as f: f.write(json.dumps(dict(sid=sid, pid=os.getpid())) + '\n')
def emit(**v): print(json.dumps(dict(session_id=sid, **v)), flush=True)
def answer(text):
    emit(type='assistant', parent_tool_use_id=None, message=dict(content=[dict(type='text', text=text)], stop_reason='end_turn'))
def gate(name):
    with open(name) as f: assert f.readline() == 'go\n'
def post(method, params, id=1):
    config = json.loads(args[args.index('--mcp-config') + 1])
    url = config['mcpServers']['carbon6']['url']
    data = json.dumps(dict(jsonrpc='2.0', id=id, method=method, params=params)).encode()
    req = urllib.request.Request(url, data=data, headers={'Content-Type':'application/json', 'Accept':'application/json, text/event-stream'})
    with opener.open(req) as r: return json.load(r)
for line in sys.stdin:
    v = json.loads(line)
    if v['type'] != 'user': continue
    with open('inputs.jsonl', 'a') as f: f.write(json.dumps(dict(sid=sid, input=v)) + '\n')
    text = v['message']['content']
    emit(type='system', subtype='init')
    emit(type='user', uuid=v['uuid'], isReplay=True, parent_tool_use_id=None)
    if text.endswith('PARENT'):
        post('initialize', dict(protocolVersion='2025-06-18', capabilities={}, clientInfo=dict(name='fake', version='0')))
        emit(type='assistant', parent_tool_use_id=None, message=dict(content=[dict(type='tool_use', id='task1', name='mcp__carbon6__Task', input=dict(description='Scout', prompt='CHILD_INITIAL', connection='cc'))], stop_reason='tool_use'))
        result = post('tools/call', dict(name='Task', arguments=dict(description='Scout', prompt='CHILD_INITIAL', connection='cc')), 2)
        Path('task-result.json').write_text(json.dumps(result))
        # Task's returned session annotation is the real local child's id.
        import re
        child = re.search(r'\[subagent session ([^\]]+)\]', result['result']['content'][0]['text']).group(1)
        emit(type='user', parent_tool_use_id=None, message=dict(content=[dict(type='tool_result', tool_use_id='task1', content='ready')]))
        emit(type='assistant', parent_tool_use_id=None, message=dict(content=[dict(type='tool_use', id='send1', name='mcp__carbon6__SendMessage', input=dict(to=child, message='CHILD_HOLD'))], stop_reason='tool_use'))
        answer('Parent waiting for restart.')
        # The worker restart destroys the foreground waiter, not the held child.
        # Its eventual report must use subagent:local, not a background handle.
        try: post('tools/call', dict(name='SendMessage', arguments=dict(to=child, message='CHILD_HOLD')), 3)
        except Exception: pass
        emit(type='user', parent_tool_use_id=None, message=dict(content=[dict(type='tool_result', tool_use_id='send1', content='worker restarted')]))
        gate('parent-gate')
        answer('Parent resumed and done.')
    elif text.endswith('CHILD_HOLD'):
        answer('Child waiting.')
        Path('child-waiting').touch()
        gate('child-gate')
        answer('Child final report.')
    elif text.endswith('CHILD_INITIAL'): answer('Child ready.')
    else: answer('Parent received report.')
    emit(type='result', subtype='success', usage={})
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    // This binary owns the process-global override.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for name in ["parent-gate", "child-gate"] {
        nix::unistd::mkfifo(&project.path().join(name), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
    }
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
    let first = held_worker::HeldWorker::start(state.path()).await;
    let cc: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind":"claude_code", "name":"cc", "base_url":""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let s: Value = http.post(format!("{api}/sessions")).json(&json!({"worker_id":first.id, "cwd":project.path().display().to_string(), "connection_id":cc["id"], "model":"default", "prompt":"PARENT"})).send().await.unwrap().json().await.unwrap();
    let id = s["id"].as_str().unwrap();
    let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
    let says = |events: &[Event], text: &str| {
        events.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains(text)))
    };
    eventually("parent ready for restart with its child running", async || {
        (says(&events().await, "Parent waiting for restart.") && project.path().join("child-waiting").exists()).then_some(())
    })
    .await;
    let second = first.restart().await;
    let release = async |name: &str| {
        let path = project.path().join(name);
        tokio::task::spawn_blocking(move || std::fs::write(path, "go\n").unwrap()).await.unwrap();
    };
    release("parent-gate").await;
    eventually("adopted parent's turn finished", async || {
        let ev = events().await;
        let detail: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        (says(&ev, "Parent resumed and done.") && detail["session"]["status"] == "idle").then_some(())
    })
    .await;
    release("child-gate").await;
    eventually("local child report sent to the parent's held process and answered", async || {
        let ev = events().await;
        let detail: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
        (says(&ev, "Parent received report.") && detail["session"]["status"] == "idle").then_some(())
    })
    .await;
    let ev = events().await;
    assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id.starts_with("subagent:local:"))));
    let launches: Vec<Value> = std::fs::read_to_string(project.path().join("launches.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(launches.len(), 2, "the held parent and child, not replacements: {launches:?}");
    second.stop();
    head.shutdown().await.unwrap();
}
