//! Memex output fits a known running window, including through a pool.

use std::sync::Arc;

use c6_api::Connection;
use c6_head::HeadConfig;
use c6_llm::Block;
use c6_memex::{
    Memex,
    llm::{self, Output, Task},
};
use c6_testkit::{MockLlm, Reply};
use serde_json::{Value, json};

struct Env {
    head: c6_head::Head,
    http: reqwest::Client,
    api: String,
    connection: Connection,
    memex: Arc<Memex>,
    task: Task,
    _dir: tempfile::TempDir,
}

impl Env {
    async fn new(base_url: &str, window: Option<u64>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let head = c6_head::start(HeadConfig {
            data_dir: dir.path().to_owned(),
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
        let connection: Connection = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "local", "base_url": base_url, "context_window": window}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let memex = c6_head::knowledge::get(&head.state).unwrap();
        let task = Task { connection_id: connection.id.clone(), model: "mock-1".into(), effort: Some("low".into()) };
        let env = Self { head, http, api, connection, memex, task, _dir: dir };
        env.set_task(&env.connection.id).await;
        env
    }

    async fn set_task(&self, connection: &str) {
        let r = self
            .http
            .put(format!("{}/settings/memex", self.api))
            .json(&json!({"understanding": {"connection": connection, "model": "mock-1", "effort": "low"}, "calls_per_day": 1}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        c6_testkit::eventually("the memex task settings", async || {
            self.memex
                .tasks()
                .understanding
                .filter(|t| t.connection_id == connection && t.model == "mock-1" && t.effort.as_deref() == Some("low"))
        })
        .await;
    }

    async fn ask(&self, system: &str, text: &str, output: Output) -> anyhow::Result<String> {
        llm::ask(&self.memex, &self.task, "test", "window", system, vec![Block::Text(text.into())], output).await
    }

    async fn window(&self, n: u64) {
        let r = self
            .http
            .patch(format!("{}/connections/{}", self.api, self.connection.id))
            .json(&json!({"context_window": n}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }
}

#[tokio::test]
async fn memex_clamps_output_and_rejects_a_prompt_without_answer_room() {
    let mock = MockLlm::start().await;
    let env = Env::new(&mock.base_url(), Some(16_384)).await;
    mock.push(Reply::Text("{}".into()));
    env.ask(&"s".repeat(3_000), &"u".repeat(21_000), Output::Json(3_000)).await.unwrap();
    // 24,000 characters / 3 + 256 for the template leaves 8,128 tokens.
    assert_eq!(mock.requests()[0]["max_tokens"], 8_128);
    let error = env.ask(&"s".repeat(3_000), &"u".repeat(40_000), Output::Json(3_000)).await.unwrap_err();
    assert!(error.to_string().contains("leaves no room for a 3000-token answer"), "{error}");
    assert!(error.to_string().contains("16384-token context window"), "{error}");
    assert!(!llm::transient(&error));
    // Text has the same useful-answer requirement, without extra thinking.
    let error = env.ask("", &"u".repeat(48_000), Output::Text(1_000)).await.unwrap_err();
    assert!(error.to_string().contains("1000-token answer"), "{error}");
    assert_eq!(mock.requests().len(), 1, "oversized prompts are never sent");
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unknown_window_keeps_the_memex_output_limit() {
    let mock = MockLlm::start().await;
    let env = Env::new(&mock.base_url(), None).await;
    mock.push(Reply::Text("{}".into()));
    env.ask("sys", &"u".repeat(400_000), Output::Json(3_000)).await.unwrap();
    assert_eq!(mock.requests()[0]["max_tokens"], 11_192);
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_memex_detects_the_running_ollama_window() {
    let mock = MockLlm::start().await;
    let chat = format!("{}/chat/completions", mock.base_url());
    let app = axum::Router::new()
        .route(
            "/api/ps",
            axum::routing::get(|| async { axum::Json(json!({"models": [{"name": "mock-1:latest", "context_length": 16384}]})) }),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let chat = chat.clone();
                async move { reqwest::Client::new().post(chat).json(&body).send().await.unwrap().text().await.unwrap() }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let env = Env::new(&base, None).await;
    mock.push(Reply::Text("{}".into()));
    env.ask(&"s".repeat(3_000), &"u".repeat(21_000), Output::Json(3_000)).await.unwrap();
    assert_eq!(mock.requests()[0]["max_tokens"], 8_128);
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_changes_reach_the_heads_cached_grants() {
    let mock = MockLlm::start().await;
    let env = Env::new(&mock.base_url(), Some(16_384)).await;
    let (system, user) = ("s".repeat(3_000), "u".repeat(21_000));
    for (window, expected) in [(16_384, 8_128), (32_768, 11_192), (16_384, 8_128), (0, 11_192)] {
        env.window(window).await;
        mock.push(Reply::Text("{}".into()));
        env.ask(&system, &user, Output::Json(3_000)).await.unwrap();
        assert_eq!(mock.requests().last().unwrap()["max_tokens"], expected);
    }
    let r = env.http.delete(format!("{}/connections/{}", env.api, env.connection.id)).send().await.unwrap();
    assert!(r.status().is_success());
    let error = env.ask(&system, &user, Output::Json(3_000)).await.unwrap_err();
    assert!(error.to_string().contains("no such connection"), "{error}");
    assert_eq!(mock.requests().len(), 4);
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_memex_pool_uses_its_smallest_member_window() {
    let (large, small) = (MockLlm::start().await, MockLlm::start().await);
    let mut env = Env::new(&large.base_url(), Some(32_768)).await;
    let small: Connection = env
        .http
        .post(format!("{}/connections", env.api))
        .json(&json!({"name": "small", "base_url": small.base_url(), "context_window": 16384}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // A smaller server that only serves another model must not set this
    // call's budget. Its model list is fetched just as for pool scheduling.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let app =
        axum::Router::new().route("/v1/models", axum::routing::get(|| async { axum::Json(json!({"data": [{"id": "other-model"}]})) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let other: Connection = env
        .http
        .post(format!("{}/connections", env.api))
        .json(&json!({"name": "other", "base_url": base, "context_window": 4096}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pool: Connection = env
        .http
        .post(format!("{}/connections", env.api))
        .json(&json!({"kind": "pool", "name": "pool", "members": [
            {"connection_id": env.connection.id, "slots": 1}, {"connection_id": small.id, "slots": 1},
            {"connection_id": other.id, "slots": 1}
        ]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    env.set_task(&pool.id).await;
    env.task.connection_id = pool.id;
    large.push(Reply::Text("{}".into()));
    env.ask(&"s".repeat(3_000), &"u".repeat(21_000), Output::Json(3_000)).await.unwrap();
    assert_eq!(large.requests()[0]["max_tokens"], 8_128, "even a call sent to the larger member fits both");
    env.head.shutdown().await.unwrap();
}
