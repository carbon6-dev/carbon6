//! Notifications: channels get the events they asked for, in sessions with
//! their labels; an approval link answers a held call without signing in.

use std::sync::{Arc, Mutex};

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A stand-in for Slack and webhooks: records (path, body).
async fn receiver() -> (String, Arc<Mutex<Vec<(String, serde_json::Value)>>>) {
    let got: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let (g, root) = (got.clone(), got.clone());
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
                let root = root.clone();
                async move {
                    root.lock().unwrap().push(("ntfy".into(), v));
                    "ok"
                }
            }),
        )
        .route(
            "/{name}",
            axum::routing::post(
                move |axum::extract::Path(name): axum::extract::Path<String>, axum::Json(v): axum::Json<serde_json::Value>| {
                    let g = g.clone();
                    async move {
                        g.lock().unwrap().push((name, v));
                        "ok"
                    }
                },
            ),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

#[tokio::test]
async fn channels_and_approval_links() {
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

    let (hooks, got) = receiver().await;
    let put = |path: &str, v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put("channels", json!([{"name": "x", "kind": "pager"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put("channels", json!([{"name": "x", "kind": "ntfy", "url": "https://ntfy.sh/"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        put("channels", json!([{"name": "x", "kind": "webhook", "url": "http://h/x", "events": ["sometimes"]}])).await,
        reqwest::StatusCode::BAD_REQUEST
    );
    let base = api.trim_end_matches("/api/v1").to_owned();
    assert!(put("public_url", json!({"url": base})).await.is_success());
    assert!(put("channels", json!([
        {"name": "hook", "kind": "webhook", "url": format!("{hooks}/hook")},
        {"name": "slack", "kind": "slack", "url": format!("{hooks}/slack"), "events": ["finished", "notice"], "applies_to": ["ops"]},
        {"name": "phone", "kind": "ntfy", "url": format!("{hooks}/my-topic"), "events": ["approval"]},
        {"name": "off", "kind": "webhook", "url": format!("{hooks}/off"), "enabled": false}
    ]))
    .await
    .is_success());
    assert!(put("policies", json!([{"name": "Ask on ops", "applies_to": ["ops"], "tools": "Bash", "action": "ask"}])).await.is_success());

    // A test message.
    let r = http.post(format!("{api}/settings/channels/test")).json(&json!({"name": "hook"})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(got.lock().unwrap().last().unwrap().1["title"], "carbon6 test");
    got.lock().unwrap().clear();

    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![(
        "notify".into(),
        r#"{"title":"Disk at 92% on web1","message":"/var is nearly full","priority":"high"}"#.into(),
    )]));
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo cleaned-up"}"#.into())]));
    mock.push(Reply::Text("Cleaned /var/log.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "check disks", "labels": ["ops"], "title": "Disk check"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sent = |path: &'static str, n: usize| {
        let got = got.clone();
        async move {
            eventually(path, async || {
                let g: Vec<serde_json::Value> = got.lock().unwrap().iter().filter(|(p, _)| p == path).map(|(_, v)| v.clone()).collect();
                (g.len() >= n).then_some(g)
            })
            .await
        }
    };
    // The approval comes to the webhook (a default event), not to Slack.
    let hook = sent("hook", 2).await;
    assert_eq!(
        (hook[0]["event"].as_str(), hook[0]["title"].as_str(), hook[0]["priority"].as_str()),
        (Some("notice"), Some("Disk at 92% on web1"), Some("high"))
    );
    assert_eq!(hook[1]["event"], "approval");
    assert_eq!(hook[1]["session_url"], format!("{base}/sessions/{}", s.id));
    assert!(hook[1]["labels"].as_array().unwrap().contains(&json!("ops")));
    let link = hook[1]["approve_url"].as_str().unwrap().to_owned();
    assert!(link.starts_with(&format!("{base}/approve/")), "{link}");

    let ntfy = sent("ntfy", 1).await;
    assert_eq!((ntfy[0]["topic"].as_str(), ntfy[0]["priority"].as_i64()), (Some("my-topic"), Some(5)));
    assert_eq!(ntfy[0]["actions"][0]["url"], link.as_str());

    // The page works without signing in; a forged link does not.
    let page = http.get(&link).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Approve bash?") && page.contains("echo cleaned-up") && page.contains("Ask on ops"), "{page}");
    let forged = format!("{}x", &link[..link.len() - 1]);
    assert!(http.get(&forged).send().await.unwrap().text().await.unwrap().contains("not valid"));
    // As a browser posts the page's form (with its Origin).
    let answer = http.post(&link).header("origin", &base).form(&[("allow", "1")]).send().await.unwrap().text().await.unwrap();
    assert!(answer.contains("Sent"), "{answer}");

    // Slack: the notice, then the finished turn with its reply.
    let slack = sent("slack", 2).await;
    assert!(slack[0]["text"].as_str().unwrap().starts_with("*Disk at 92% on web1*"), "{slack:?}");
    let done = slack[1]["text"].as_str().unwrap();
    assert!(
        done.contains("Done: Disk check")
            && done.contains("Cleaned /var/log.")
            && done.contains(&format!("/sessions/{}|Open session", s.id)),
        "{done}"
    );
    assert!(mock.requests()[2]["messages"].to_string().contains("cleaned-up"), "allowed, it ran");
    // Answered: the link says so.
    assert!(http.post(&link).form(&[("allow", "0")]).send().await.unwrap().text().await.unwrap().contains("already answered"));
    assert!(!got.lock().unwrap().iter().any(|(p, _)| p == "off"));

    stop.cancel();
    head.shutdown().await.unwrap();
}
