//! Web search through the head's provider setting (here a stand-in
//! SearXNG), offered to models without a search of their own.

use c6_api::Connection;
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn web_search_through_the_configured_provider() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let searx = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let searx_url = format!("http://{}", searx.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut s, _)) = searx.accept().await {
            let mut buf = vec![0u8; 8192];
            let _ = s.read(&mut buf).await;
            let body = r#"{"results":[{"title":"carbon6 docs","url":"https://example.org/c6","content":"The command center."}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    let put = |v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/web-search"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put(json!({"provider": "brave"})).await, reqwest::StatusCode::BAD_REQUEST, "needs a key secret");
    assert_eq!(put(json!({"provider": "brave", "secret": "NOPE"})).await, reqwest::StatusCode::BAD_REQUEST, "an existing secret");
    assert!(put(json!({"provider": "searxng", "url": searx_url})).await.is_success());

    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![("web_search".into(), r#"{"query":"carbon6 command center"}"#.into())]));
    mock.push(Reply::Text("Found it.".into()));
    http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "look it up"}))
        .send()
        .await
        .unwrap();
    eventually("two requests", async || (mock.requests().len() == 2).then_some(())).await;
    let reqs = mock.requests();
    let tools: Vec<&str> = reqs[0]["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert!(tools.contains(&"web_search"), "{tools:?}");
    let second = reqs[1]["messages"].to_string();
    assert!(second.contains("1. carbon6 docs") && second.contains("https://example.org/c6"), "{second}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
