//! Connection settings and detected windows reach the worker's compaction loop.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    routing::{get, post},
};
use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn head(dir: &std::path::Path) -> (c6_head::Head, String) {
    let h = c6_head::start(HeadConfig {
        data_dir: dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", h.ui_addrs[0].port());
    (h, api)
}

#[tokio::test]
async fn connection_context_window_round_trips_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let (head, api) = head(dir.path()).await;
    let http = reqwest::Client::new();
    let send = async |method, path: &str, body: Value| http.request(method, format!("{api}/{path}")).json(&body).send().await.unwrap();
    for invalid in [1, 1_023, 100_000_001, u64::MAX] {
        let r =
            send(Method::POST, "connections", json!({"name": "bad", "base_url": "http://127.0.0.1:9/v1", "context_window": invalid})).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(r.text().await.unwrap().contains("context window must be between"));
    }
    let c: Connection =
        send(Method::POST, "connections", json!({"name": "local", "base_url": "http://127.0.0.1:9/v1", "context_window": 16384}))
            .await
            .json()
            .await
            .unwrap();
    assert_eq!(c.context_window, Some(16_384));
    let path = format!("connections/{}", c.id);
    let list: Vec<Connection> = http.get(format!("{api}/connections")).send().await.unwrap().json().await.unwrap();
    assert_eq!(list[0].context_window, Some(16_384));
    for invalid in [1_023, 100_000_001] {
        let r = send(Method::PATCH, &path, json!({"context_window": invalid})).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(r.text().await.unwrap().contains("context window must be between"));
    }
    for n in [1_024, 100_000_000, 32_768] {
        let c: Connection = send(Method::PATCH, &path, json!({"context_window": n})).await.json().await.unwrap();
        assert_eq!(c.context_window, Some(n));
        let grant = c6_head::sessions::credential_for(
            &head.state,
            None,
            "head",
            c6_proto::CredentialRequest { connection_id: c.id, ..Default::default() },
        )
        .await;
        assert!(grant.error.is_empty());
        assert_eq!(grant.context_window, n);
    }
    let c: Connection = send(Method::PATCH, &path, json!({"name": "renamed"})).await.json().await.unwrap();
    assert_eq!(c.context_window, Some(32_768), "omitted leaves the setting alone");
    let c: Connection = send(Method::PATCH, &path, json!({"context_window": 0})).await.json().await.unwrap();
    assert_eq!(c.context_window, None);
    let list: Vec<Connection> = http.get(format!("{api}/connections")).send().await.unwrap().json().await.unwrap();
    assert_eq!(list[0].context_window, None);
    head.shutdown().await.unwrap();
}

async fn worker(head: &c6_head::Head, dir: &std::path::Path) -> (Arc<Worker>, CancellationToken) {
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir).unwrap();
    let worker = Worker::open(dir, Identity::load(dir).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (worker, stop)
}

async fn ended(http: &reqwest::Client, api: &str, id: &str, turns: usize) -> Vec<Event> {
    eventually("the turn to end", async || {
        let events: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap();
        (events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns).then_some(events)
    })
    .await
}

#[tokio::test]
async fn an_old_connection_row_is_automatic_and_keeps_the_builtin_window() {
    let (dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (head, api) = head(dir.path()).await;
    let mock = MockLlm::start().await;
    sqlx::query("INSERT INTO connections (id, name, kind, base_url, created_at) VALUES ('old', 'old', 'openai_compat', ?, 0)")
        .bind(mock.base_url())
        .execute(head.state.store.pool())
        .await
        .unwrap();
    let http = reqwest::Client::new();
    let list: Vec<Connection> = http.get(format!("{api}/connections")).send().await.unwrap().json().await.unwrap();
    assert_eq!(list[0].context_window, None);
    assert_eq!(head.state.store.connection("old").await.unwrap().unwrap().context_window, None);
    let grant = c6_head::sessions::credential_for(
        &head.state,
        None,
        "head",
        c6_proto::CredentialRequest { connection_id: "old".into(), ..Default::default() },
    )
    .await;
    assert_eq!(grant.context_window, 0);
    let (worker, stop) = worker(&head, state.path()).await;
    mock.push(Reply::Text("ok".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({
            "worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(), "connection_id": "old", "model": "unknown", "prompt": "hi"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events = ended(&http, &api, &s.id, 1).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::Started { context_window: 128_000, .. })));
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnStarted { context_window: 128_000, .. })));
    stop.cancel();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn detected_ollama_window_is_recorded_and_triggers_compaction() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    let app = Router::new()
        .route("/api/ps", get(|| async { Json(json!({"models": [{"name": "qwen3.8:27b", "context_length": 16384}]})) }))
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let requests = seen.clone();
                async move {
                    let mut requests = requests.lock().unwrap();
                    let n = requests.len();
                    requests.push(body);
                    let text = if n == 1 { "SUMMARY" } else { "ok" };
                    let reply = json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
                    let usage =
                        json!({"choices": [], "usage": {"prompt_tokens": if n == 0 { 14000 } else { 100 }, "completion_tokens": 100}});
                    format!("data: {reply}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (head, api) = head(dir.path()).await;
    let http = reqwest::Client::new();
    let c: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "ollama", "base_url": base}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (worker, stop) = worker(&head, state.path()).await;
    let s: Session = http.post(format!("{api}/sessions")).json(&json!({
        "worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(), "connection_id": c.id, "model": "qwen3.8:27b", "prompt": "hi"
    })).send().await.unwrap().json().await.unwrap();
    let events = ended(&http, &api, &s.id, 1).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::Started { context_window: 16_384, .. })));
    assert!(!events.iter().any(|e| matches!(e.kind, EventKind::Compaction { .. })));
    assert!(
        http.post(format!("{api}/sessions/{}/messages", s.id))
            .json(&json!({"text": "continue"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let events = ended(&http, &api, &s.id, 2).await;
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Compaction { summary, tokens_before: 14_100 } if summary == "SUMMARY")));
    let windows: Vec<u64> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::TurnStarted { context_window, .. } => Some(context_window),
            _ => None,
        })
        .collect();
    assert_eq!(windows, [16_384, 16_384]);
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["messages"].to_string().contains("context window is nearly full"));
    assert!(requests[2]["messages"].to_string().contains("SUMMARY"));
    stop.cancel();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_reach_assistant_turns_subagents_and_model_switches() {
    let (dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (head, api) = head(dir.path()).await;
    let http = reqwest::Client::new();
    let (main, other) = (MockLlm::start().await, MockLlm::start().await);
    let add = async |name: &str, mock: &MockLlm, window| -> Connection {
        http.post(format!("{api}/connections"))
            .json(&json!({"name": name, "base_url": mock.base_url(), "context_window": window}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    let main_c = add("main", &main, 16_384).await;
    let other_c = add("other", &other, 16_384).await;
    let r = http
        .put(format!("{api}/settings/assistant"))
        .json(&json!({"model": {"connection": main_c.id, "model": "mock-1"}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    main.push(Reply::Text("ready".into()));
    let assistant: Session =
        http.post(format!("{api}/assistant")).json(&json!({"prompt": "hi"})).send().await.unwrap().json().await.unwrap();
    let events = ended(&http, &api, &assistant.id, 1).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::Started { context_window: 16_384, .. })));
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnStarted { context_window: 16_384, .. })));
    let r = http.patch(format!("{api}/connections/{}", main_c.id)).json(&json!({"context_window": 32768})).send().await.unwrap();
    assert!(r.status().is_success());
    main.push(Reply::Text("updated".into()));
    let r = http.post(format!("{api}/sessions/{}/messages", assistant.id)).json(&json!({"text": "again"})).send().await.unwrap();
    assert!(r.status().is_success());
    let events = ended(&http, &api, &assistant.id, 2).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnStarted { turn: 2, context_window: 32_768 })));

    let (worker, stop) = worker(&head, state.path()).await;
    main.push(Reply::ToolCalls(vec![(
        "task".into(),
        json!({"description": "look", "prompt": "Look.", "connection": "other", "model": "mock-1"}).to_string(),
    )]));
    other.push(Reply::Text("seen".into()));
    main.push(Reply::Text("done".into()));
    let parent: Session = http.post(format!("{api}/sessions")).json(&json!({
        "worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(), "connection_id": main_c.id, "model": "mock-1", "prompt": "delegate"
    })).send().await.unwrap().json().await.unwrap();
    let events = ended(&http, &api, &parent.id, 1).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::Started { context_window: 32_768, .. })));
    let child = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id),
            _ => None,
        })
        .unwrap();
    let events = ended(&http, &api, child, 1).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::Started { context_window: 16_384, .. })));
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnStarted { context_window: 16_384, .. })));
    other.push(Reply::Text("switched".into()));
    let r = http
        .post(format!("{api}/sessions/{}/messages", parent.id))
        .json(&json!({"text": "switch", "connection_id": other_c.id, "model": "mock-1"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let events = ended(&http, &api, &parent.id, 2).await;
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::ModelSwitched { context_window: 16_384, .. })));
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnStarted { turn: 2, context_window: 16_384 })));
    stop.cancel();
    head.shutdown().await.unwrap();
}
