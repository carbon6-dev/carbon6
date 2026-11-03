//! Recently viewed (`POST /api/v1/sessions/{id}/viewed`, `GET
//! /api/v1/sessions/recent`): the sessions you viewed, newest first, kept
//! per realm on the head so every device shows the same; a realm's caller
//! sees and writes only its own realm's, and archived sessions drop out.

use c6_api::{Connection, RecentView, Session};
use c6_head::HeadConfig;
use c6_testkit::MockLlm;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn viewed_sessions_come_first_in_their_realm() {
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
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    // Seeding with root's token; looking as your browser in a realm's page.
    let root = head.state.store.create_api_token("t", None).await.unwrap().token.unwrap();
    let link = head.state.store.create_login_link().await.unwrap();
    let signed_in = http.post(format!("{api}/auth/link")).json(&json!({"token": link})).send().await.unwrap();
    let cookie = signed_in.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned();
    let call = |realm: &str, method: M, path: &str, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}")).header("cookie", &cookie).header("x-c6-realm", realm);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send();
        async move {
            let r = r.await.unwrap();
            let s = r.status();
            (s, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let send = |realm: &'static str, method: M, path: String, body: Option<Value>| {
        let r = call(realm, method, &path, body);
        async move {
            let (s, v) = r.await;
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
    let recent = |realm: &'static str| {
        let r = send(realm, M::GET, "sessions/recent".into(), None);
        async move { serde_json::from_value::<Vec<RecentView>>(r.await).unwrap().into_iter().map(|v| v.session_id).collect::<Vec<_>>() }
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
    let start = |realm: &'static str, title: &str| {
        let body =
            json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m", "title": title});
        let r = send(realm, M::POST, "sessions".into(), Some(body));
        async move { serde_json::from_value::<Session>(r.await).unwrap().id }
    };
    let (a, b, c) = (start("home", "A").await, start("home", "B").await, start("lab", "C").await);
    assert!(recent("home").await.is_empty());

    // 1. Viewing two sessions puts them first, the latest first.
    send("home", M::POST, format!("sessions/{a}/viewed"), None).await;
    send("home", M::POST, format!("sessions/{b}/viewed"), None).await;
    assert_eq!(recent("home").await, [b.clone(), a.clone()]);

    // 2. Another realm keeps its own: lab's page can't view home's
    // session, and sees only what was viewed in lab; nor does home see lab's.
    assert!(recent("lab").await.is_empty());
    assert_eq!(call("lab", M::POST, &format!("sessions/{a}/viewed"), None).await.0, StatusCode::NOT_FOUND);
    send("lab", M::POST, format!("sessions/{c}/viewed"), None).await;
    assert_eq!(recent("lab").await, std::slice::from_ref(&c));
    assert_eq!(recent("home").await, [b.clone(), a.clone()]);
    // A token limited to lab too: it can't ask for home's.
    let lab = head.state.store.create_api_token("lab", Some("lab")).await.unwrap().token.unwrap();
    let mine: Vec<RecentView> = http.get(format!("{api}/sessions/recent")).bearer_auth(&lab).send().await.unwrap().json().await.unwrap();
    assert_eq!(mine.iter().map(|v| v.session_id.as_str()).collect::<Vec<_>>(), [c.as_str()]);
    let other = http.get(format!("{api}/sessions/recent?realm=home")).bearer_auth(&lab).send().await.unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);

    // 3. An archived session drops out, and viewing it again doesn't
    // bring it back.
    send("home", M::PATCH, format!("sessions/{b}"), Some(json!({"archived": true}))).await;
    assert_eq!(recent("home").await, std::slice::from_ref(&a));
    send("home", M::POST, format!("sessions/{b}/viewed"), None).await;
    assert_eq!(recent("home").await, [a]);
    stop.cancel();
}
