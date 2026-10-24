//! Realms in the UI (docs/realms-ux.md): a browser's request says which
//! realm's page it comes from, and the head keeps it there. Nothing of
//! another realm comes back from any realm list; administration is the
//! console's; a page that doesn't say where it is gets nothing.

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_head::catalog::{NOT_OPERATIONS, Scope, Via, ops, scope};
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// What every object seeded in home carries, so a leak shows.
const MARK: &str = "homemark";

#[tokio::test]
async fn realm_pages_stay_in_their_realm() {
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
    let base = format!("http://127.0.0.1:{}", head.ui_addrs[0].port());
    let api = format!("{base}/api/v1");
    let http = reqwest::Client::new();
    // You in the console, by a root token (seeding), and a signed-in browser.
    let root = head.state.store.create_api_token("t", None).await.unwrap().token.unwrap();
    let link = head.state.store.create_login_link().await.unwrap();
    let signed_in = http.post(format!("{api}/auth/link")).json(&json!({"token": link})).send().await.unwrap();
    assert!(signed_in.status().is_success());
    let cookie = signed_in.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned();
    let send = |method: M, path: String, place: Option<&str>, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}")).header("cookie", &cookie);
        if let Some(p) = place {
            r = r.header("x-c6-realm", p);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
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

    // Home has one of everything, marked; lab is empty.
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    seed(M::POST, "realms", json!({"id": "lab"})).await;
    let conn: Connection =
        serde_json::from_value(seed(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).unwrap();
    mock.push(Reply::Text("Done.".into()));
    let s: Session = serde_json::from_value(
        seed(
            M::POST,
            "sessions",
            json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": format!("{MARK} session"), "title": format!("{MARK} session")}),
        )
        .await,
    )
    .unwrap();
    eventually("the session's turn", async || (!mock.requests().is_empty()).then_some(())).await;
    let decision = seed(M::POST, "decisions", json!({"title": format!("{MARK} decision")})).await;
    seed(M::POST, "memories", json!({"text": format!("{MARK} note")})).await;
    seed(M::POST, "series", json!({"name": MARK})).await;
    seed(M::PUT, "settings/secrets/HOMEMARK_KEY", json!({"value": "v", "description": MARK})).await;
    seed(M::PUT, "settings/skills?realm=home", json!([{"name": MARK, "description": MARK, "body": "x"}])).await;
    let trigger =
        seed(M::POST, "triggers", json!({"name": MARK, "trigger": "webhook", "action": "notify", "action_params": {"title": MARK}})).await;
    seed(M::POST, "collections", json!({"name": MARK, "schema": {"type": "object", "properties": {"what": {"type": "string"}}}})).await;

    // 1. Every realm list, from lab's page: none of home's.
    let lists: Vec<&str> = ops()
        .iter()
        .filter(|o| scope(o.name) == Some(Scope::Realm))
        .filter_map(|o| match o.via {
            Via::Http { method: "GET", path } if !path.contains('{') => Some(path),
            _ => None,
        })
        .chain(NOT_OPERATIONS.iter().filter(|(m, p, _, s)| *m == "GET" && *s == Scope::Realm && !p.contains('{')).map(|(_, p, ..)| *p))
        .collect();
    assert!(lists.len() > 15, "{lists:?}");
    for path in &lists {
        let r = send(M::GET, path.trim_start_matches("/api/v1/").to_owned(), Some("lab"), None).await.unwrap();
        let status = r.status();
        let body = r.text().await.unwrap().to_lowercase();
        assert!(!body.contains(MARK), "{path} ({status}) shows home's: {body}");
    }
    // The same lists from home's page do show them (the check above means
    // something).
    let home: Vec<Session> = send(M::GET, "sessions".into(), Some("home"), None).await.unwrap().json().await.unwrap();
    assert_eq!(home.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), [s.id.as_str()]);
    let skills = send(M::GET, "settings/skills".into(), Some("home"), None).await.unwrap().text().await.unwrap();
    assert!(skills.contains(MARK), "{skills}");

    // 2. Home's objects, asked for by id from lab: missing.
    for path in [
        format!("sessions/{}", s.id),
        format!("sessions/{}/events", s.id),
        format!("decisions/{}", decision["id"]),
        format!("triggers/{}/runs", trigger["id"].as_str().unwrap()),
        format!("collections/{MARK}"),
    ] {
        let r = send(M::GET, path.clone(), Some("lab"), None).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let r = send(M::POST, format!("sessions/{}/messages", s.id), Some("lab"), Some(json!({"text": "hi"}))).await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // Nor by naming home in the query.
    let r = send(M::GET, "settings/skills?realm=home".into(), Some("lab"), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // 3. Administration is the console's: every admin operation refuses a
    // realm's page, before it does anything.
    let admin: Vec<(M, String)> = ops()
        .iter()
        .filter(|o| scope(o.name) == Some(Scope::Admin))
        .filter_map(|o| match o.via {
            Via::Http { method, path } => Some((M::from_bytes(method.as_bytes()).unwrap(), path.to_owned())),
            _ => None,
        })
        .chain(
            NOT_OPERATIONS
                .iter()
                .filter(|(_, p, _, s)| *s == Scope::Admin && p.starts_with("/api/"))
                .map(|(m, p, ..)| (M::from_bytes(m.as_bytes()).unwrap(), (*p).to_owned())),
        )
        .collect();
    assert!(admin.len() > 40, "{}", admin.len());
    let fill = regex::Regex::new(r"\{\*?[a-z_]+\}").unwrap();
    for (method, path) in &admin {
        let path = fill.replace_all(path.trim_start_matches("/api/v1/"), "x").into_owned();
        let r = send(method.clone(), path.clone(), Some("lab"), Some(json!({}))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    // From the console they work.
    assert!(send(M::GET, "settings/prices".into(), Some("admin"), None).await.unwrap().status().is_success());
    let all: Vec<Session> = send(M::GET, "sessions".into(), Some("admin"), None).await.unwrap().json().await.unwrap();
    assert_eq!(all.len(), 1);

    // 4. What's live and what's counted is the realm's: the live feed, the
    // analytics, how the list is grouped, the workers it may use.
    let live = |place: &'static str| {
        let (url, cookie) = (format!("ws://127.0.0.1:{}/api/v1/live?c6_realm={place}", head.ui_addrs[0].port()), cookie.clone());
        async move {
            use futures::StreamExt;
            use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
            let mut req = url.into_client_request().unwrap();
            req.headers_mut().insert("cookie", cookie.parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            loop {
                let Message::Text(t) = c6_testkit::within("a live message", ws.next()).await.unwrap().unwrap() else { continue };
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "sessions" {
                    return v["sessions"].as_array().unwrap().len();
                }
            }
        }
    };
    assert_eq!((live("lab").await, live("home").await), (0, 1));
    let turns = |place: &'static str| {
        let r = send(M::GET, "analytics?days=1".into(), Some(place), None);
        async move { r.await.unwrap().json::<Value>().await.unwrap()["total"]["turns"].as_u64().unwrap() }
    };
    assert_eq!(turns("lab").await, 0);
    assert!(turns("home").await >= 1);
    assert!(
        send(M::PUT, "settings/session_grouping".into(), Some("home"), Some(json!({"levels": ["repo"]})))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let lab_grouping: Value = send(M::GET, "settings/session_grouping".into(), Some("lab"), None).await.unwrap().json().await.unwrap();
    assert_eq!(lab_grouping, json!({"levels": []}));
    let wid = worker.identity.meta.worker_id.0.to_string();
    seed(M::PUT, &format!("workers/{wid}/realms"), json!({"realms": ["home"]})).await;
    let lab_workers: Vec<Value> = send(M::GET, "workers".into(), Some("lab"), None).await.unwrap().json().await.unwrap();
    assert!(lab_workers.is_empty(), "{lab_workers:?}");
    let r = send(M::GET, format!("workers/{wid}/fs/list?path=/"), Some("lab"), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(send(M::GET, format!("workers/{wid}/fs/list?path=/"), Some("home"), None).await.unwrap().status().is_success());

    // 5. Other realms: a count each, no more; and an old link finds its realm.
    let elsewhere: Vec<Value> = send(M::GET, "elsewhere".into(), Some("lab"), None).await.unwrap().json().await.unwrap();
    let waiting = |id: &str| elsewhere.iter().find(|r| r["id"] == id).map(|r| r["waiting"].as_u64().unwrap());
    assert_eq!((waiting("home"), waiting("lab"), waiting("root")), (Some(1), Some(0), None), "{elsewhere:?}");
    assert!(!serde_json::to_string(&elsewhere).unwrap().contains(MARK));
    let at: Value = send(M::GET, format!("locate/sessions/{}", s.id), Some("lab"), None).await.unwrap().json().await.unwrap();
    assert_eq!(at["realm"], "home");

    // 6. A page that doesn't say where it is gets only this browser's own
    // business; nor can it claim root, or a realm that isn't there.
    assert_eq!(send(M::GET, "sessions".into(), None, None).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert!(send(M::GET, "version".into(), None, None).await.unwrap().status().is_success());
    assert!(send(M::GET, "auth/status".into(), None, None).await.unwrap().status().is_success());
    for bad in ["root", "nowhere"] {
        assert_eq!(send(M::GET, "sessions".into(), Some(bad), None).await.unwrap().status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    // What can't send headers says it in the query.
    let r = http.get(format!("{api}/sessions?c6_realm=lab")).header("cookie", &cookie).send().await.unwrap();
    assert_eq!(r.json::<Vec<Session>>().await.unwrap().len(), 0);
    // No realm can take the console's name.
    let r = http.post(format!("{api}/realms")).bearer_auth(&root).json(&json!({"id": "admin"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    stop.cancel();
    head.shutdown().await.unwrap();
}
