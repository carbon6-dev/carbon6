use c6_api::{Event, EventKind, Session, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually_or, within};
use c6_worker::{ConnState, conn, enroll};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const PLAIN: &str = "WIDGET-PLAIN";
const LIST: &str = "WIDGET-LIST";
const LAUNCH: &str = "WIDGET-LAUNCH";
const CHILD: &str = "WIDGET-CHILD-PROCEDURE";
const STATUS: &str = "WIDGET-STATUS";
const POLICY_V1: &str = "Keep the widget interface stable.";
const POLICY_V2: &str = "Keep widget adapters separate.";

async fn write(http: &Client, api: &str, method: Method, path: &str, body: Value) -> Value {
    let r = http.request(method.clone(), format!("{api}/{path}")).json(&body).send().await.unwrap();
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "{method} {path}: {status}: {text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

async fn completed_turn(http: &Client, api: &str, id: &str, mark: &str, mock: &MockLlm) -> Vec<Event> {
    eventually_or(
        mark,
        async || {
            let events: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
            let start = events.iter().position(|e| matches!(&e.kind, EventKind::User { text, .. } if text.contains(mark)))?;
            let rest = &events[start..];
            let end = rest.iter().position(|e| matches!(&e.kind, EventKind::TurnEnded { .. }))?;
            assert!(
                matches!(&rest[end].kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. }),
                "{mark}: {:?}\n{}",
                rest[end],
                mock.log()
            );
            Some(rest[..=end].to_vec())
        },
        || mock.log(),
    )
    .await
}

fn tool_reply(input: Value) -> Reply {
    Reply::ToolCalls(vec![("capability".into(), input.to_string())])
}

fn tool_result(events: &[Event]) -> Value {
    let (output, is_error) = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult { tool, output, is_error, .. } if tool == "capability" => Some((output, *is_error)),
            _ => None,
        })
        .expect("capability tool result");
    assert!(!is_error, "{output}");
    serde_json::from_str(output).unwrap()
}

fn system_for(mock: &MockLlm, mark: &str) -> String {
    let request = mock.requests().into_iter().find(|r| r["messages"].to_string().contains(mark)).expect("marked model request");
    request["messages"][0]["content"].as_str().expect("system prompt").to_owned()
}

fn contract_edit(expected_version: u64, rule: &str) -> Value {
    json!({"expected_version": expected_version, "contract": {
        "architecture_rules": [{"value": rule}],
        "capabilities": [{"value": {
            "name": "inspect-widget", "description": "Inspect a widget",
            "location": {"kind": "template", "name": "widget-procedure"},
            "inputs": {"type": "object", "properties": {"part": {"type": "string"}}, "required": ["part"], "additionalProperties": false},
            "result_schema": {"type": "object", "properties": {"observation": {"type": "string"}}, "required": ["observation"], "additionalProperties": false}
        }}]
    }})
}

#[tokio::test]
async fn project_contract_refresh_and_capability_run_follow_the_worker_turns() {
    let (head_dir, worker_dir, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let http = Client::new();
    let browser = Client::builder()
        .default_headers(
            [(reqwest::header::HeaderName::from_static("x-c6-realm"), reqwest::header::HeaderValue::from_static("home"))]
                .into_iter()
                .collect(),
        )
        .build()
        .unwrap();
    let mock = MockLlm::start().await;
    let connection = write(&http, &api, Method::POST, "connections", json!({"name": "widget-model", "base_url": mock.base_url()})).await;
    write(&http, &api, Method::PUT, "settings/suggestions", json!({"enabled": false, "catchup": false, "fold": false, "names": false}))
        .await;
    write(
        &http,
        &api,
        Method::PUT,
        "settings/work",
        json!({"guess": false, "notes": false, "quiet_days": 3, "nudge": false, "needs_hours": 4}),
    )
    .await;

    let enrollment = c6_head::api::new_enrollment(&head.state, Some("worker-a")).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(worker_dir.path()).unwrap();
    let worker = crate::open_worker(worker_dir.path()).await;
    {
        let mut overrides = worker.overrides.lock().unwrap();
        overrides.import_home = Some(worker_dir.path().join("empty-home"));
        overrides.kit_jobs = Some(false);
    }
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    let _stop_on_exit = stop.clone().drop_guard();
    let connection_task = tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    within("the contract test worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    write(
        &http,
        &api,
        Method::PUT,
        "settings/templates",
        json!([{
            "name": "widget-procedure", "workers": ["worker-a"], "cwd": folder.path(),
            "connection": "widget-model", "model": "widget-model", "agents": true, "prompt": format!("{CHILD}\n\n{{{{input}}}}")
        }]),
    )
    .await;

    mock.push_about(PLAIN, Reply::Text("Plain conversation completed.".into()));
    let session: Session = serde_json::from_value(
        write(
            &http,
            &api,
            Method::POST,
            "sessions",
            json!({
                "worker_id": worker.identity.meta.worker_id.0, "cwd": folder.path(), "connection_id": connection["id"],
                "model": "widget-model", "prompt": PLAIN
            }),
        )
        .await,
    )
    .unwrap();
    completed_turn(&http, &api, &session.id, PLAIN, &mock).await;
    let plain = system_for(&mock, PLAIN);
    assert!(!plain.is_empty() && !plain.contains("Project contract") && !plain.contains(POLICY_V1));

    let project = write(&http, &api, Method::POST, "projects", json!({"title": "Widget", "plan": false})).await;
    let pid = project["id"].as_str().unwrap();
    write(&http, &api, Method::POST, &format!("projects/{pid}/sessions"), json!({"session": session.id})).await;
    let contract_path = format!("projects/{pid}/contract");
    let messages_path = format!("sessions/{}/messages", session.id);
    let edit = write(&browser, &api, Method::PUT, &contract_path, contract_edit(0, POLICY_V1)).await;
    assert_eq!(edit["version"], 1);

    mock.push_about(LIST, tool_reply(json!({"action": "list"})));
    mock.push_about(LIST, Reply::Text("Listed the widget capability.".into()));
    write(&http, &api, Method::POST, &messages_path, json!({"text": LIST})).await;
    let listed = tool_result(&completed_turn(&http, &api, &session.id, LIST, &mock).await);
    assert_eq!(listed["version"], 1);
    assert_eq!(listed["capabilities"][0]["value"]["name"], "inspect-widget");
    let v1 = system_for(&mock, LIST);
    assert!(v1.contains(POLICY_V1) && !v1.contains(POLICY_V2));

    let edit = write(&browser, &api, Method::PUT, &contract_path, contract_edit(1, POLICY_V2)).await;
    assert_eq!(edit["version"], 2);
    // The idle actor must persist the new prompt before we send another input.
    eventually_or(
        "idle contract refresh",
        async || {
            let (_, events) = worker.journal.load_session(&session.id).await.ok()??;
            events
                .iter()
                .any(|e| match &e.kind {
                    Some(c6_proto::session_event::Kind::SpecRefreshed(refresh)) => {
                        refresh.system_prompt.contains(POLICY_V2) && !refresh.system_prompt.contains(POLICY_V1)
                    }
                    _ => false,
                })
                .then_some(())
        },
        || mock.log(),
    )
    .await;

    mock.push_about(LAUNCH, tool_reply(json!({"name": "inspect-widget", "input": {"part": "widget-panel"}})));
    mock.push_about(LAUNCH, Reply::Text("The widget inspection was queued.".into()));
    let terminal = json!({"status": "succeeded", "details": {"observation": "widget-panel-ready"}});
    let child_gate = mock.push_gated_about(CHILD, Reply::Text(terminal.to_string()));
    write(&http, &api, Method::POST, &messages_path, json!({"text": LAUNCH})).await;
    let started = tool_result(&completed_turn(&http, &api, &session.id, LAUNCH, &mock).await);
    assert_eq!(started["status"], "queued");
    let run_id = started["details"]["run_id"].as_str().unwrap();
    let child_id = started["details"]["sessions"][0].as_str().unwrap();
    let v2 = system_for(&mock, LAUNCH);
    assert!(v2.contains(POLICY_V2) && !v2.contains(POLICY_V1));

    // Match the explicit query before the optional waking report if the
    // parent happens to receive both in the same turn.
    mock.push_about(STATUS, tool_reply(json!({"run_id": run_id})));
    mock.push_about(STATUS, Reply::Text("Read the structured widget result.".into()));
    mock.push_about_all(&["Subagent ", "widget-panel-ready"], Reply::Text("Received the widget report.".into()));
    child_gate.notify_one();
    completed_turn(&http, &api, child_id, CHILD, &mock).await;
    let child_system = system_for(&mock, CHILD);
    assert!(child_system.contains(POLICY_V2) && !child_system.contains(POLICY_V1));
    let child_request = mock.requests().into_iter().find(|r| r["messages"].to_string().contains(CHILD)).unwrap();
    assert!(child_request["messages"].to_string().contains("widget-panel"));
    let children: Vec<Session> =
        http.get(format!("{api}/sessions/{}/children", session.id)).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert!(children.iter().any(|child| child.id == child_id && child.parent_call_id.is_some()));

    write(&http, &api, Method::POST, &messages_path, json!({"text": STATUS})).await;
    let observed = tool_result(&completed_turn(&http, &api, &session.id, STATUS, &mock).await);
    assert_eq!(observed, terminal);

    stop.cancel();
    within("the contract test worker to stop", connection_task).await.unwrap().unwrap();
}
