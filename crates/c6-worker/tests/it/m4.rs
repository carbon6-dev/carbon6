//! M4: subagents, MCP, skills and OTLP export through a real head and worker.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use c6_api::{Event, EventKind, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const FAKE_MCP: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    meth = m["method"]
    if meth == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}})
    elif meth == "tools/list":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"tools": [{"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}]}})
    elif meth == "tools/call":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"content": [{"type": "text", "text": "echo: " + m["params"]["arguments"]["text"]}]}})
    else:
        send({"jsonrpc": "2.0", "id": m["id"], "result": {}})
"#;

#[tokio::test]
async fn subagents_mcp_skills_and_otlp() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    // A collector that records OTLP trace exports.
    let exports: Arc<Mutex<Vec<ExportTraceServiceRequest>>> = Arc::default();
    let rec = exports.clone();
    let app = axum::Router::new().route(
        "/v1/traces",
        axum::routing::post(move |body: Bytes| {
            let rec = rec.clone();
            async move {
                rec.lock().unwrap().push(ExportTraceServiceRequest::decode(body).unwrap());
                "ok"
            }
        }),
    );
    let collector = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let collector_addr = collector.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(collector, app).await.unwrap() });

    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let cfg = HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        otlp_endpoint: Some(format!("http://{collector_addr}")),
        auth: false,
        ..Default::default()
    };
    let head = c6_head::start(cfg).await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(worker_dir.path()).unwrap();
    let worker = Worker::open(worker_dir.path(), Identity::load(worker_dir.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();

    // Head configuration: one MCP server, one skill.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("fake_mcp.py"), FAKE_MCP).unwrap();
    let r = http
        .put(format!("{api}/settings/mcp-servers"))
        .json(&json!([{"name": "fake", "transport": "stdio", "command": "python3", "args": ["fake_mcp.py"]}]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = http
        .put(format!("{api}/settings/skills"))
        .json(&json!([{"name": "greet", "description": "How to greet", "body": "Say bonjour."}]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let mock = MockLlm::start().await;
    mock.push(Reply::ToolCalls(vec![
        ("mcp__fake__echo".into(), r#"{"text":"hi"}"#.into()),
        ("task".into(), r#"{"description":"look around","prompt":"Report the word ok."}"#.into()),
    ]));
    mock.push(Reply::Text("child report: ok".into())); // the subagent's turn
    mock.push(Reply::Text("All done.".into()));

    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sess: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
            "connection_id": conn["id"], "model": "m", "prompt": "go"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", sess.id)).send().await.unwrap().json().await.unwrap();
        if d.session.status == SessionStatus::Idle && d.queued.is_empty() && d.session.last_seq > 5 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "session did not finish: {:?}", d.session);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let events: Vec<Event> = http.get(format!("{api}/sessions/{}/events", sess.id)).send().await.unwrap().json().await.unwrap();

    // Skills and MCP tools are part of the session.
    let EventKind::Started { system_prompt, tools, .. } = &events[0].kind else { panic!() };
    assert!(system_prompt.contains("- greet: How to greet"), "skills index in prompt");
    assert!(tools.iter().any(|t| t == "load_skill") && tools.iter().any(|t| t == "mcp__fake__echo"), "{tools:?}");

    let results: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, .. } => Some((tool.as_str(), output.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(results[0], ("mcp__fake__echo", "echo: hi"));
    assert_eq!(results[1].0, "task");
    assert!(results[1].1.starts_with("child report: ok"), "{:?}", results[1]);

    // The subagent ran as a linked child session, hidden from the main list.
    let children: Vec<Session> = http.get(format!("{api}/sessions/{}/children", sess.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].title, "look around");
    assert!(children[0].parent_call_id.as_deref().unwrap().contains("task"));
    let listed: Vec<Session> = http.get(format!("{api}/sessions")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed.len(), 1);

    // OTLP: one export per finished turn (parent + child), with GenAI spans.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while exports.lock().unwrap().len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no OTLP exports");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let all = exports.lock().unwrap().clone();
    let parent = all
        .iter()
        .flat_map(|r| &r.resource_spans[0].scope_spans[0].spans)
        .filter(|s| s.attributes.iter().any(|a| a.key == "gen_ai.conversation.id" && format!("{:?}", a.value).contains(&sess.id)))
        .collect::<Vec<_>>();
    let names: Vec<&str> = parent.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"invoke_agent carbon6"), "{names:?}");
    assert_eq!(names.iter().filter(|n| n.starts_with("chat ")).count(), 2, "{names:?}");
    assert!(names.contains(&"execute_tool mcp__fake__echo") && names.contains(&"execute_tool task"), "{names:?}");
    let chat = parent.iter().find(|s| s.name.starts_with("chat ")).unwrap();
    let input = chat.attributes.iter().find(|a| a.key == "gen_ai.usage.input_tokens").unwrap();
    assert!(format!("{:?}", input.value).contains("IntValue(100)"), "{input:?}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
