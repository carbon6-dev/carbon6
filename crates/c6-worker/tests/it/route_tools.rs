//! A routing rule's tool list is fixed for the session and inherited by
//! side chats and subagents, including after a worker reload or restore.

use std::sync::{Arc, Mutex};

use c6_api::{Connection, Event, EventKind, Route, Session};
use c6_head::HeadConfig;
use c6_proto::{CreateSession, Delegation, ModelSpec, SessionCommand, SessionEvent, SessionStarted, session_command, session_event};
use c6_testkit::{MockLlm, Reply, eventually, within};
use c6_worker::{ConnState, Worker, conn, enroll};
use prost::Message;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    connection: Connection,
    state: tempfile::TempDir,
    project: tempfile::TempDir,
    _head_dir: tempfile::TempDir,
}

struct Running {
    worker: Arc<Worker>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Running {
    async fn stop(self) {
        self.stop.cancel();
        within("the worker connection to stop", self.task).await.unwrap().unwrap();
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
        let connection = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "main", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let state = tempfile::tempdir().unwrap();
        let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
        Self { head, api, http, mock, connection, state, project: tempfile::tempdir().unwrap(), _head_dir: head_dir }
    }

    async fn worker(&self) -> Running {
        let worker = crate::open_worker(self.state.path()).await;
        let (tx, mut rx) = watch::channel(ConnState::Connecting);
        let stop = CancellationToken::new();
        let task = tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
        within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
        Running { worker, stop, task }
    }

    async fn routes(&self, tools: Value) -> reqwest::Response {
        self.http
            .put(format!("{}/settings/routes", self.api))
            .json(&json!([{"name": "mail locally", "applies_to": ["mail"], "connection": "main", "model": "m", "tools": tools}]))
            .send()
            .await
            .unwrap()
    }

    async fn post<T: serde::de::DeserializeOwned>(&self, path: &str, body: Value) -> T {
        let r = self.http.post(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.text().await.unwrap());
        r.json().await.unwrap()
    }

    async fn create(&self, running: &Running) -> Session {
        self.post(
            "/sessions",
            json!({"worker_id": running.worker.identity.meta.worker_id.0, "cwd": self.project.path().display().to_string(),
                "labels": ["mail"], "prompt": "hello"}),
        )
        .await
    }

    async fn after_turns(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("the turn to end", async || {
            let events: Vec<Event> = self.http.get(format!("{}/sessions/{id}/events", self.api)).send().await.ok()?.json().await.ok()?;
            (events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() >= turns).then_some(events)
        })
        .await
    }

    async fn say(&self, id: &str, turns: usize) {
        self.mock.push(Reply::Text("ok".into()));
        let _: Value = self.post(&format!("/sessions/{id}/messages"), json!({"text": "again"})).await;
        self.after_turns(id, turns).await;
    }
}

fn tool_names(request: &Value) -> Vec<String> {
    let mut names: Vec<String> =
        request["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().into()).collect();
    names.sort();
    names
}

async fn anthropic(env: &Env) -> (Connection, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            seen.lock().unwrap().push(body);
            async {
                let events = [
                    json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":"seen"}}),
                    json!({"type":"content_block_stop", "index":0}),
                    json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}}),
                    json!({"type":"message_stop"}),
                ];
                ([("content-type", "text/event-stream")], events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connection = env.post("/connections", json!({"name":"native", "kind":"anthropic", "base_url":base_url, "api_key":"test"})).await;
    (connection, requests, server)
}

fn has_hosted_search(request: &Value) -> bool {
    request["tools"].as_array().is_some_and(|tools| tools.iter().any(|t| t["type"].as_str().is_some_and(|s| s.starts_with("web_search_"))))
}

#[tokio::test]
async fn hosted_search_follows_lists_across_providers_and_reload() {
    let env = Env::new().await;
    let (_, requests, server) = anthropic(&env).await;
    let running = env.worker().await;
    assert!(env.routes(json!([])).await.status().is_success());
    let r = env
        .http
        .put(format!("{}/settings/agents", env.api))
        .json(&json!([{"name":"delegator", "description":"Delegate searches", "tools":"Read, Task", "prompt":"Delegate a search."}]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let task = json!({"description":"search", "prompt":"Look.", "connection":"native", "model":"claude-sonnet-5-5"});
    env.mock.push(Reply::ToolCalls(vec![("task".into(), task.to_string())]));
    env.mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description":"delegate", "prompt":"Look.", "subagent_type":"delegator"}).to_string(),
    )]));
    env.mock.push(Reply::ToolCalls(vec![("task".into(), task.to_string())]));
    env.mock.push(Reply::Text("delegator report".into()));
    env.mock.push(Reply::Text("done".into()));
    let root = env.create(&running).await;
    let events = env.after_turns(&root.id, 1).await;
    let captured = requests.lock().unwrap().clone();
    assert_eq!(captured.len(), 2);
    assert!(has_hosted_search(&captured[0]), "an unrestricted Chat Completions parent allows hosted search");
    assert!(!has_hosted_search(&captured[1]), "a named parent's list denies it to its child");
    let first = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id),
            _ => None,
        })
        .unwrap();
    let (_, first_events) = running.worker.journal.load_session(first).await.unwrap().unwrap();
    let Some(session_event::Kind::Started(started)) = &first_events[0].kind else { panic!("no SessionStarted") };
    assert!(!started.hosted_search_off);
    assert!(!started.tools.contains(&"web_search".into()), "hosted search is not a function-tool name");
    let parent = events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id),
            _ => None,
        })
        .unwrap();
    let child = env
        .after_turns(parent, 1)
        .await
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id),
            _ => None,
        })
        .unwrap();
    let (_, events) = running.worker.journal.load_session(&child).await.unwrap().unwrap();
    let Some(session_event::Kind::Started(started)) = &events[0].kind else { panic!("no SessionStarted") };
    assert!(started.hosted_search_off);
    assert_eq!(started.tools, ["Read", "Task"]);
    running.stop().await;
    let running = env.worker().await;
    let _: Value = env.post(&format!("/sessions/{child}/messages"), json!({"text":"again"})).await;
    env.after_turns(&child, 2).await;
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert!(!has_hosted_search(&requests.lock().unwrap()[2]), "the denial survives a reload");
    running.stop().await;
    server.abort();
    env.head.shutdown().await.unwrap();
}

// A subagent written before SessionStarted had a hosted-search denial.
#[derive(Clone, PartialEq, Message)]
struct LegacyStarted {
    #[prost(message, optional, tag = "2")]
    model: Option<ModelSpec>,
    #[prost(string, tag = "3")]
    cwd: String,
    #[prost(string, repeated, tag = "5")]
    tools: Vec<String>,
    #[prost(string, tag = "6")]
    tool_profile: String,
    #[prost(string, tag = "9")]
    parent_session_id: String,
}

#[tokio::test]
async fn legacy_subagents_keep_hosted_search_after_reload_and_restore() {
    let env = Env::new().await;
    let (native, requests, server) = anthropic(&env).await;
    let running = env.worker().await;
    assert!(env.routes(json!([])).await.status().is_success());
    env.mock.push(Reply::Text("ready".into()));
    let root = env.create(&running).await;
    env.after_turns(&root.id, 1).await;
    let (spec, _) = running.worker.journal.load_session(&root.id).await.unwrap().unwrap();
    let model = ModelSpec {
        connection_id: native.id,
        connection_kind: "anthropic".into(),
        model: "claude-sonnet-5-5".into(),
        ..Default::default()
    };
    let spec = CreateSession {
        model: Some(model.clone()),
        delegation: Some(Delegation {
            parent_session_id: root.id.clone(),
            root_session_id: root.id.clone(),
            depth: 1,
            ..Default::default()
        }),
        ..spec
    };
    let legacy = LegacyStarted {
        model: Some(model),
        cwd: spec.cwd.clone(),
        tools: vec!["Read".into()],
        tool_profile: "claude".into(),
        parent_session_id: root.id,
    };
    let started = SessionStarted::decode(legacy.encode_to_vec().as_slice()).unwrap();
    assert!(!started.hosted_search_off);
    let event = SessionEvent { seq: 1, kind: Some(session_event::Kind::Started(started)), ..Default::default() };
    running.worker.journal.create_session("legacy-child", &spec, &event).await.unwrap();
    running.stop().await;
    let running = env.worker().await;
    // Loading an older worker's journal and importing the same record via
    // Restore both offer hosted search, without a tool-name marker.
    for (i, id) in ["legacy-child", "restored-child"].into_iter().enumerate() {
        if i == 1 {
            running
                .worker
                .sessions
                .handle(SessionCommand {
                    session_id: id.into(),
                    kind: Some(session_command::Kind::Restore(c6_proto::Restore { spec: Some(spec.clone()), events: vec![event.clone()] })),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        running
            .worker
            .sessions
            .handle(SessionCommand {
                session_id: id.into(),
                command_id: format!("input-{id}"),
                kind: Some(session_command::Kind::Input(c6_proto::UserInput { text: "continue".into(), ..Default::default() })),
            })
            .await
            .unwrap();
        eventually("the legacy subagent's turn to end", async || {
            let events = running.worker.journal.events_after(id, 0, u32::MAX).await.ok()?;
            events.iter().any(|e| matches!(e.kind, Some(session_event::Kind::TurnEnded(_)))).then_some(())
        })
        .await;
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), i + 1);
        assert!(has_hosted_search(&captured[i]));
    }
    running.stop().await;
    server.abort();
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn routes_validate_tool_names_from_every_profile() {
    let env = Env::new().await;
    let r = env.routes(json!(["not_a_tool"])).await;
    assert_eq!(r.status(), 400);
    let error = r.text().await.unwrap();
    for text in ["mail locally", "not_a_tool", "valid tools:", "read", "bash", "memex_search", "mcp__<server>__<tool>"] {
        assert!(error.contains(text), "{error}");
    }
    let mut names: Vec<String> = [c6_llm::ToolProfile::Generic, c6_llm::ToolProfile::Claude, c6_llm::ToolProfile::Codex]
        .into_iter()
        .flat_map(c6_worker::session::builtin_tool_names)
        .collect();
    // The services need not exist when a rule is saved.
    for name in ["MemexSearch", "MailThreads", "CalendarEvents", "ChatMessages", "Skill", "WebSearch", "Browser", "shell", "apply_patch"] {
        assert!(names.contains(&name.to_owned()), "{name} missing from the catalog");
    }
    names.push("mcp__unconfigured__tool".into());
    names.extend(["read_file", "skill", "MultiEdit"].map(str::to_owned));
    let r = env.routes(json!(names)).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let routes: Vec<Route> = env.http.get(format!("{}/settings/routes", env.api)).send().await.unwrap().json().await.unwrap();
    assert_eq!(routes[0].tools, names);
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn route_tools_survive_rule_edits_reload_restore_and_side_chats() {
    let env = Env::new().await;
    let running = env.worker().await;
    assert!(env.routes(json!(["Read", "shell"])).await.status().is_success());
    env.mock.push(Reply::Text("ready".into()));
    let s = env.create(&running).await;
    let events = env.after_turns(&s.id, 1).await;
    assert_eq!(tool_names(&env.mock.requests()[0]), ["bash", "read"]);
    match &events[0].kind {
        EventKind::Started { tools, system_prompt, .. } => {
            assert_eq!(tools, &["read", "bash"]);
            assert!(!system_prompt.contains("You have tools to") && !system_prompt.contains("- Use the dedicated tools"));
        }
        e => panic!("{e:?}"),
    }
    let row = env.head.state.store.session(&s.id).await.unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Vec<String>>(&row.tools).unwrap(), ["Read", "shell"]);
    let (spec, _) = running.worker.journal.load_session(&s.id).await.unwrap().unwrap();
    assert_eq!(spec.tools, ["Read", "shell"]);

    assert!(env.routes(json!(["Write"])).await.status().is_success());
    env.say(&s.id, 2).await;
    assert_eq!(tool_names(env.mock.requests().last().unwrap()), ["bash", "read"]);
    let anchor = events.iter().find(|e| matches!(e.kind, EventKind::Assistant { .. })).unwrap().seq;
    env.mock.push(Reply::Text("side answer".into()));
    let side: Session = env
        .post(
            &format!("/sessions/{}/side-chats", s.id),
            json!({"anchor_seq": anchor, "connection_id": env.connection.id, "model": "side", "prompt": "why?"}),
        )
        .await;
    env.after_turns(&side.id, 1).await;
    assert_eq!(tool_names(env.mock.requests().last().unwrap()), ["bash", "read"]);
    assert_eq!(env.head.state.store.session(&side.id).await.unwrap().unwrap().tools, row.tools);

    running.stop().await;
    let running = env.worker().await;
    env.say(&s.id, 3).await;
    assert_eq!(tool_names(env.mock.requests().last().unwrap()), ["bash", "read"]);
    running.stop().await;

    // No worker copy: the head restores the original list from its row.
    let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", env.state.path().join("journal.db").display())).await.unwrap();
    sqlx::query("DELETE FROM sessions WHERE id = ?").bind(&s.id).execute(&db).await.unwrap();
    db.close().await;
    let running = env.worker().await;
    env.say(&s.id, 4).await;
    assert_eq!(tool_names(env.mock.requests().last().unwrap()), ["bash", "read"]);
    let (restored, _) = running.worker.journal.load_session(&s.id).await.unwrap().unwrap();
    assert_eq!(restored.tools, spec.tools);
    running.stop().await;
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn empty_route_tools_send_the_full_tool_set() {
    let env = Env::new().await;
    let running = env.worker().await;
    assert!(env.routes(json!([])).await.status().is_success());
    env.mock.push(Reply::Text("ready".into()));
    let s = env.create(&running).await;
    let events = env.after_turns(&s.id, 1).await;
    let names = tool_names(&env.mock.requests()[0]);
    for name in ["read", "write", "edit", "bash", "grep", "glob", "task", "remember", "forget", "task_propose", "records_query"] {
        assert!(names.contains(&name.to_owned()), "{name} missing: {names:?}");
    }
    match &events[0].kind {
        EventKind::Started { tools, system_prompt, .. } => {
            let mut tools = tools.clone();
            tools.sort();
            assert_eq!(tools, names);
            assert!(system_prompt.starts_with(c6_agent::BASE_PROMPT.trim_end()));
        }
        e => panic!("{e:?}"),
    }
    running.stop().await;
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagents_inherit_route_tools_in_both_profiles_and_narrow_for_named_agents() {
    let env = Env::new().await;
    let running = env.worker().await;
    assert!(env.routes(json!(["Read", "Bash", "Task"])).await.status().is_success());
    let r = env
        .http
        .put(format!("{}/settings/agents", env.api))
        .json(&json!([
            {"name": "finder", "description": "Find things", "tools": "Read, Edit", "prompt": "Search only."},
            {"name": "delegator", "description": "Delegate searches", "tools": "Read, Task", "prompt": "Delegate a focused search."}
        ]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    for choice in [json!({}), json!({"model": "claude-sonnet-5-5"}), json!({"model": "claude-sonnet-5-5", "subagent_type": "finder"})] {
        let mut input = json!({"description": "look", "prompt": "Look around."});
        input.as_object_mut().unwrap().extend(choice.as_object().unwrap().clone());
        env.mock.push(Reply::ToolCalls(vec![("task".into(), input.to_string())]));
        env.mock.push(Reply::Text("seen".into()));
    }
    // A named parent's narrower list also bounds a grandchild in another
    // profile; inheriting only the root's route list would restore Bash.
    env.mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description":"delegate", "prompt":"Search.", "subagent_type":"delegator"}).to_string(),
    )]));
    env.mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description":"search", "prompt":"Look.", "model":"claude-sonnet-5-5"}).to_string(),
    )]));
    env.mock.push(Reply::Text("nested report".into()));
    env.mock.push(Reply::Text("delegator report".into()));
    env.mock.push(Reply::Text("done".into()));
    let s = env.create(&running).await;
    let events = env.after_turns(&s.id, 1).await;
    let requests = env.mock.requests();
    assert_eq!(requests.len(), 11);
    assert_eq!(tool_names(&requests[0]), ["bash", "read", "task"]);
    assert_eq!(tool_names(&requests[1]), ["bash", "read", "task"]);
    assert_eq!(tool_names(&requests[3]), ["Bash", "Read", "Task"]);
    assert_eq!(tool_names(&requests[5]), ["Read"]);
    assert_eq!(tool_names(&requests[7]), ["read", "task"]);
    assert_eq!(tool_names(&requests[8]), ["Read", "Task"]);
    let children: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(children.len(), 4);
    for (id, request) in children.iter().zip([1, 3, 5, 7]) {
        let child_events = env.after_turns(id, 1).await;
        match &child_events[0].kind {
            EventKind::Started { tools, .. } => {
                let mut tools = tools.clone();
                tools.sort();
                assert_eq!(tools, tool_names(&requests[request]));
            }
            e => panic!("{e:?}"),
        }
    }
    running.stop().await;
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn named_agents_bound_cross_profile_children_without_a_route_tool_list() {
    let env = Env::new().await;
    let running = env.worker().await;
    assert!(env.routes(json!([])).await.status().is_success());
    let r = env.http.put(format!("{}/settings/agents", env.api))
        .json(&json!([{"name": "delegator", "description": "Delegate searches", "tools": "Read, Task", "prompt": "Delegate a focused search."}]))
        .send().await.unwrap();
    assert!(r.status().is_success());
    env.mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description":"delegate", "prompt":"Search.", "subagent_type":"delegator"}).to_string(),
    )]));
    env.mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description":"search", "prompt":"Look.", "model":"claude-sonnet-5-5"}).to_string(),
    )]));
    env.mock.push(Reply::Text("nested report".into()));
    env.mock.push(Reply::Text("delegator report".into()));
    env.mock.push(Reply::Text("done".into()));
    let s = env.create(&running).await;
    env.after_turns(&s.id, 1).await;
    let requests = env.mock.requests();
    assert_eq!(requests.len(), 5);
    assert!(tool_names(&requests[0]).contains(&"bash".to_owned()));
    assert_eq!(tool_names(&requests[1]), ["read", "task"]);
    assert_eq!(tool_names(&requests[2]), ["Read", "Task"]);
    running.stop().await;
    env.head.shutdown().await.unwrap();
}
