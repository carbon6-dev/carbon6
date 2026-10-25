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
    // The console's decisions are root's own (installs, grants), not every
    // realm's; and what waits there shows in the realm menu.
    seed(M::POST, "decisions?realm=root", json!({"title": "Install the slack app?"})).await;
    let console: Vec<Value> = send(M::GET, "decisions".into(), Some("admin"), None).await.unwrap().json().await.unwrap();
    assert_eq!(console.iter().map(|d| d["title"].as_str().unwrap()).collect::<Vec<_>>(), ["Install the slack app?"]);
    let count: Value = send(M::GET, "decisions/count".into(), Some("admin"), None).await.unwrap().json().await.unwrap();
    assert_eq!(count["open"], 1);
    let places: Vec<Value> = send(M::GET, "elsewhere".into(), Some("home"), None).await.unwrap().json().await.unwrap();
    assert_eq!(places.iter().find(|p| p["id"] == "root").map(|p| p["waiting"].clone()), Some(json!(1)), "{places:?}");
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
    // A busy realm (more recent sessions than a list holds) doesn't crowd
    // a quieter one out of its own list.
    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", head_dir.path().join("c6h.db").display())).await.unwrap();
    let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('sessions')").fetch_all(&db).await.unwrap();
    let picked: Vec<String> = cols
        .iter()
        .map(|c| match c.as_str() {
            "id" => "'busy-' || n.i".to_owned(),
            "realm" => "'lab'".to_owned(),
            "updated_at" => "updated_at + 1000 + n.i".to_owned(),
            other => other.to_owned(),
        })
        .collect();
    let copy = format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 250) INSERT INTO sessions ({}) SELECT {} FROM sessions, n WHERE sessions.id = ?",
        cols.join(", "),
        picked.join(", ")
    );
    sqlx::query(sqlx::AssertSqlSafe(copy)).bind(&s.id).execute(&db).await.unwrap();
    head.state.registry.bump();
    assert_eq!(live("home").await, 1, "home's session still listed");
    let home: Vec<Session> = send(M::GET, "sessions".into(), Some("home"), None).await.unwrap().json().await.unwrap();
    assert_eq!(home.len(), 1);
    sqlx::query("DELETE FROM sessions WHERE id LIKE 'busy-%'").execute(&db).await.unwrap();
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
    assert_eq!((waiting("home"), waiting("lab"), waiting("root")), (Some(1), Some(0), Some(1)), "{elsewhere:?}");
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

    // 7. A realm's own settings, root's where it has none: the console sets
    // root's, a realm's page its own, and "use root's" goes back.
    let get = |path: &'static str, place: &'static str| {
        let r = send(M::GET, path.into(), Some(place), None);
        async move { r.await.unwrap().json::<Value>().await.unwrap() }
    };
    let put = |path: &'static str, place: &'static str, body: Value| {
        let r = send(M::PUT, path.into(), Some(place), Some(body));
        async move { assert!(r.await.unwrap().status().is_success(), "{path} in {place}") }
    };
    put("settings/suggestions", "admin", json!({"enabled": false})).await;
    put("settings/suggestions", "home", json!({"enabled": true})).await;
    assert_eq!(get("settings/suggestions", "lab").await["enabled"], false, "lab follows root's");
    assert_eq!(get("settings/suggestions", "home").await["enabled"], true, "home has its own");
    assert_eq!(
        (get("settings/own/suggestions", "home").await["own"].clone(), get("settings/own/suggestions", "lab").await["own"].clone()),
        (json!(true), json!(false))
    );
    // Sessions read it for their realm.
    let used = |realm: &'static str| head.state.store.setting_for("session_suggestions", realm);
    assert!(used("home").await.unwrap().unwrap().contains("true"));
    assert!(used("lab").await.unwrap().unwrap().contains("false"));
    assert!(send(M::DELETE, "settings/own/suggestions".into(), Some("home"), None).await.unwrap().status().is_success());
    assert_eq!(get("settings/suggestions", "home").await["enabled"], false, "back to root's");
    // Prompts: lab's own text, home still root's.
    put("settings/prompts/session.base", "lab", json!({"body": "LAB BASE"})).await;
    let prompt = |place: &'static str| async move {
        get("settings/prompts", place).await.as_array().unwrap().iter().find(|p| p["name"] == "session.base").cloned().unwrap()
    };
    let (lab, home_p) = (prompt("lab").await, prompt("home").await);
    assert_eq!((lab["body"].as_str(), lab["edited"].as_bool()), (Some("LAB BASE"), Some(true)));
    assert_eq!(home_p["edited"], false);
    assert_ne!(home_p["body"], "LAB BASE");
    // Mail and calendar are a realm's: its page installs the Google apps
    // and keeps its own OAuth client; a realm without one uses root's.
    put("google/client", "admin", json!({"client_id": "root-client", "client_secret": "s1"})).await;
    let lab_client = get("google/client", "lab").await;
    assert_eq!((lab_client["client_id"].as_str(), lab_client["from_root"].as_bool()), (Some("root-client"), Some(true)), "{lab_client}");
    put("google/client", "home", json!({"client_id": "home-client", "client_secret": "s2"})).await;
    let home_client = get("google/client", "home").await;
    assert_eq!((home_client["client_id"].as_str(), home_client["from_root"].as_bool()), (Some("home-client"), Some(false)));
    assert_eq!(get("google/client", "lab").await["client_id"], "root-client", "lab still uses root's");
    let r = send(M::POST, "google/install".into(), Some("lab"), None).await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    // Root's list entries reach a realm (shown to it read-only).
    put("settings/skills?realm=root", "admin", json!([{"name": "root-skill", "description": "everyone's", "body": "x"}])).await;
    let used: Vec<Value> = send(M::GET, "realms/lab/effective/skills".into(), Some("lab"), None).await.unwrap().json().await.unwrap();
    assert!(used.iter().any(|e| e["from"] == "root" && e["entry"]["name"] == "root-skill"), "{used:?}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
