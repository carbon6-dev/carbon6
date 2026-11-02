//! What needs you (`GET /api/v1/attention`): a tool call waiting for
//! approval and an open decision wait on you until answered; a session's
//! ended turn is worth a look until you've seen it; another realm's
//! decision is only a count there, never its title; and each change pings
//! the live feed.

use c6_api::{Attention, Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::Method as M;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn what_needs_you_comes_and_goes() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let port = head.ui_addrs[0].port();
    let api = format!("http://127.0.0.1:{port}/api/v1");
    let http = reqwest::Client::new();
    // Seeding with root's token; looking as your browser in home.
    let root = head.state.store.create_api_token("t", None).await.unwrap().token.unwrap();
    let link = head.state.store.create_login_link().await.unwrap();
    let signed_in = http.post(format!("{api}/auth/link")).json(&json!({"token": link})).send().await.unwrap();
    let cookie = signed_in.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned();
    let send = |method: M, path: &str, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}")).header("cookie", &cookie).header("x-c6-realm", "home");
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send();
        async move {
            let r = r.await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{s}: {v}");
            v
        }
    };
    let seed = |method: M, path: &str, body: Value| {
        let r = http.request(method, format!("{api}/{path}")).bearer_auth(&root).json(&body).send();
        async move {
            let r = r.await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{s}: {v}");
            v
        }
    };
    let attention = || {
        let r = send(M::GET, "attention", None);
        async move { serde_json::from_value::<Attention>(r.await).unwrap() }
    };

    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    seed(M::POST, "realms", json!({"id": "lab", "name": "Lab"})).await;
    let conn: Connection =
        serde_json::from_value(seed(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).unwrap();
    seed(M::PUT, "settings/policies", json!([{"name": "ask on deploy", "tools": "Bash", "pattern": "deploy", "action": "ask"}])).await;
    let a = attention().await;
    assert!(a.waiting.is_empty() && a.worth.is_empty() && a.elsewhere.is_empty(), "{a:?}");

    // The live feed, read as home's page does: how many pings came.
    let (pinged, pings) = watch::channel(0usize);
    {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let mut req = format!("ws://127.0.0.1:{port}/api/v1/live?c6_realm=home").into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        // Its first snapshot says it's listening.
        loop {
            let Message::Text(t) = c6_testkit::within("the first snapshot", ws.next()).await.unwrap().unwrap() else { continue };
            if serde_json::from_str::<Value>(&t).unwrap()["type"] == "sessions" {
                break;
            }
        }
        tokio::spawn(async move {
            while let Some(Ok(m)) = ws.next().await {
                if let Message::Text(t) = m
                    && serde_json::from_str::<Value>(&t).is_ok_and(|v| v["type"] == "attention")
                {
                    pinged.send_modify(|n| *n += 1);
                }
            }
        });
    }
    // How many came so far; then waiting for one more than that.
    let so_far = || *pings.borrow();
    let pinged_since = |seen: usize| {
        let mut pings = pings.clone();
        async move {
            c6_testkit::within("a ping on the live feed", pings.wait_for(|n| *n > seen)).await.unwrap();
        }
    };

    // 1. A tool call held for approval waits on you, with its session and
    // call; while it waits, the session isn't also worth a look.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo deploy-ran"}"#.into())]));
    mock.push(Reply::Text("Deployed.".into()));
    let s: Session = serde_json::from_value(
        send(
            M::POST,
            "sessions",
            Some(json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "deploy it", "title": "Deploy"})),
        )
        .await,
    )
    .unwrap();
    let approval =
        eventually("the approval to wait on you", async || attention().await.waiting.into_iter().find(|i| i.kind == "approval")).await;
    assert_eq!((approval.session_id.as_deref(), approval.detail.as_str()), (Some(s.id.as_str()), "Deploy"), "{approval:?}");
    assert!(approval.call_id.as_deref().is_some_and(|c| !c.is_empty()) && approval.key.starts_with("decision:"), "{approval:?}");
    pinged_since(0).await;
    assert!(attention().await.worth.is_empty());
    send(M::POST, &format!("decisions/{}/answer", approval.decision_id.unwrap()), Some(json!({"option": "Allow"}))).await;

    // 2. Its turn ended: worth a look, until you've seen it.
    let key = format!("session:{}", s.id);
    let turn = eventually("the ended turn to be worth a look", async || attention().await.worth.into_iter().find(|i| i.key == key)).await;
    assert_eq!((turn.kind.as_str(), turn.detail.as_str(), turn.session_id.as_deref()), ("turn_ended", "Deploy", Some(s.id.as_str())));
    let a = attention().await;
    assert!(a.waiting.is_empty(), "the approval left once answered: {a:?}");
    let n = so_far();
    send(M::POST, "attention/seen", Some(json!({"keys": [key]}))).await;
    assert!(attention().await.worth.is_empty());
    pinged_since(n).await;

    // 3. A decision opened here waits on you until it's answered.
    let n = so_far();
    let d = send(M::POST, "decisions", Some(json!({"title": "Ship on Friday?", "options": [{"label": "Yes"}, {"label": "No"}]}))).await;
    let waiting = attention().await.waiting;
    assert_eq!(waiting.iter().map(|i| (i.kind.as_str(), i.title.as_str())).collect::<Vec<_>>(), [("decision", "Ship on Friday?")]);
    pinged_since(n).await;
    let n = so_far();
    send(M::POST, &format!("decisions/{}/answer", d["id"]), Some(json!({"option": "Yes"}))).await;
    assert!(attention().await.waiting.is_empty());
    pinged_since(n).await;

    // 4. Another realm's decision: only a count, from here.
    let n = so_far();
    seed(M::POST, "decisions?realm=lab", json!({"title": "Lab's secret plan"})).await;
    let a = attention().await;
    assert!(a.waiting.is_empty(), "{a:?}");
    assert_eq!(a.elsewhere.iter().map(|e| (e.id.as_str(), e.name.as_str(), e.waiting)).collect::<Vec<_>>(), [("lab", "Lab", 1)]);
    assert!(!serde_json::to_string(&a).unwrap().contains("secret"), "{a:?}");
    pinged_since(n).await;
    // In lab it waits in full.
    let lab: Value = http
        .get(format!("{api}/attention"))
        .header("cookie", &cookie)
        .header("x-c6-realm", "lab")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(lab["waiting"][0]["title"], "Lab's secret plan", "{lab}");
    stop.cancel();
}
