//! Self-configuration (docs/follow-through.md §5): the first session in a
//! repo with no kit sets it up in a side chat (once per repo); a session
//! writes a kit through the catalog, which is dry-run and switched on (with
//! an action item to review it); sessions cannot remove kits, touch secrets
//! or do anything destructive; a tracker whose probe keeps failing starts a
//! repair.

use std::time::Duration;

use c6_api::{Connection, Followups, Kit, Session, SessionDetail, SessionStatus, Todo};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn sessions_configure_follow_through() {
    let mock = MockLlm::start().await;
    let (head_dir, state, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, Some("build-1")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let wid = worker.identity.meta.worker_id.0;
    // A repository (its remote names it: repo:github.com/you/app).
    let git =
        |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(repo.path()).output().unwrap().status.success());
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "https://github.com/you/app.git"]);
    let start = |prompt: &str, cwd: String| {
        let (http, api) = (http.clone(), api.clone());
        let body = json!({"worker_id": wid, "cwd": cwd, "connection_id": conn.id, "model": "m", "prompt": prompt});
        async move { http.post(format!("{api}/sessions")).json(&body).send().await.unwrap().json::<Session>().await.unwrap() }
    };
    let idle = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("the turn's end", async || {
                let d: SessionDetail = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                (d.session.status == SessionStatus::Idle && turn_ended(&http, &api, &d.session.id).await).then_some(())
            })
            .await
        }
    };
    let children = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/sessions/{id}/children")).send().await.unwrap().json::<Vec<Session>>().await.unwrap() }
    };
    let cwd = repo.path().canonicalize().unwrap().display().to_string();

    // The playbooks start as carbon6's own; yours replace them. A more
    // specific one (for this org) wins over the general one.
    let mut books: Vec<Value> = http.get(format!("{api}/settings/playbooks")).send().await.unwrap().json().await.unwrap();
    assert_eq!(books.len(), 4, "{books:?}");
    assert!(books[0]["prompt"].as_str().unwrap().contains("Set up follow-through for {{key}}"));
    books.push(json!({"name": "Our repos", "trigger": "first_session", "applies_to": ["repo:github.com/you"],
        "prompt": "Set up {{repo}} our way: CI is Argo, reached from build-1."}));
    let bad = http
        .put(format!("{api}/settings/playbooks"))
        .json(&json!([{"name": "x", "trigger": "whenever", "prompt": "p"}]))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let r = http.put(format!("{api}/settings/playbooks")).json(&books).send().await.unwrap();
    assert!(r.status().is_success());

    // The first session in the repo: set up in a side chat, by itself.
    mock.push(Reply::Text("ok".into()));
    mock.push(Reply::Text("Set up: nothing found yet.".into()));
    let s1 = start("hello", cwd.clone()).await;
    let side = eventually("the setup side chat", async || children(s1.id.clone()).await.into_iter().next()).await;
    assert_eq!(side.parent_id.as_deref(), Some(s1.id.as_str()));
    idle(s1.id.clone()).await;
    idle(side.id.clone()).await;
    let asked = mock
        .requests()
        .iter()
        .map(|r| r.to_string())
        .find(|r| r.contains("Set up github.com/you/app our way: CI is Argo, reached from build-1."))
        .expect("your playbook, filled in");
    assert!(!asked.contains("Set up follow-through for"), "the more specific playbook won: {asked}");
    let runs: Value = http.get(format!("{api}/settings/playbook_runs")).send().await.unwrap().json().await.unwrap();
    assert_eq!(runs["Our repos"], json!(["repo:github.com/you/app"]));
    // Once per repo.
    mock.push(Reply::Text("ok".into()));
    let s2 = start("again", cwd.clone()).await;
    idle(s2.id.clone()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(children(s2.id.clone()).await.is_empty(), "set up only once");

    // A session writes a kit: dry-run, then active, with an item to review it.
    let kit = json!([{"name": "app-ci", "applies_to": ["team:none"], "reactions": [], "trackers": [{
        "name": "ci", "title": "CI", "probe": {"command": "echo '{\"phase\":\"waiting\"}'"}, "interval_s": 1
    }]}]);
    mock.push(Reply::ToolCalls(vec![
        ("c6_discover".into(), r#"{"query":"project kits"}"#.into()),
        ("c6_call".into(), json!({"op": "settings.kits.put", "input": {"body": kit}}).to_string()),
    ]));
    mock.push(Reply::ToolCalls(vec![
        ("c6_call".into(), json!({"op": "settings.kits.put", "input": {"body": []}}).to_string()),
        ("c6_call".into(), json!({"op": "settings.secrets.put", "input": {"name": "X", "body": {"value": "v"}}}).to_string()),
        ("c6_call".into(), json!({"op": "connections.delete", "input": {"id": conn.id}}).to_string()),
        ("c6_call".into(), json!({"op": "workers.enrollments.create", "input": {"body": {}}}).to_string()),
        ("c6_call".into(), json!({"op": "workers.list"}).to_string()),
    ]));
    mock.push(Reply::Text("Kit saved.".into()));
    let s3 = start("set up CI tracking", String::new()).await;
    idle(s3.id.clone()).await;
    let ev: Vec<Value> = http.get(format!("{api}/sessions/{}/events?after=0", s3.id)).send().await.unwrap().json().await.unwrap();
    let results: Vec<(String, bool)> = ev
        .iter()
        .filter(|e| e["type"] == "tool_result")
        .map(|e| (e["output"].as_str().unwrap().to_owned(), e["is_error"] == true))
        .collect();
    assert!(results[0].0.contains("settings.kits"), "discovery: {}", results[0].0);
    assert!(!results[1].1 && results[1].0.contains("\"active\": true"), "saved and active: {}", results[1].0);
    assert!(results[2].1 && results[2].0.contains("do not remove kits"), "{:?}", results[2]);
    assert!(results[3].1 && results[3].0.contains("only the user"), "{:?}", results[3]);
    assert!(results[4].1 && results[4].0.contains("destructive"), "{:?}", results[4]);
    assert!(results[5].1 && results[5].0.contains("secret"), "{:?}", results[5]);
    assert!(!results[6].1 && results[6].0.contains("build-1"), "reads are fine: {:?}", results[6]);
    let kits: Vec<Kit> = http.get(format!("{api}/settings/kits")).send().await.unwrap().json().await.unwrap();
    assert_eq!((kits.len(), kits[0].state.as_str()), (1, "active"));
    let todos: Vec<Todo> = http.get(format!("{api}/todos")).send().await.unwrap().json().await.unwrap();
    assert!(todos.iter().any(|t| t.kind == "kit" && t.title == "Review kit app-ci" && t.priority == "low"), "{todos:?}");
    let conns: Vec<Connection> = http.get(format!("{api}/connections")).send().await.unwrap().json().await.unwrap();
    assert_eq!(conns.len(), 1, "the connection is still there");

    // A tracker whose probe keeps failing starts a repair.
    let broken = json!([{"name": "deploys", "applies_to": ["team:core"], "reactions": [{
        "name": "after deploy", "when": {"tool": "bash", "input": "deploy"}, "then": [{"kind": "track", "tracker": "rollout"}]
    }], "trackers": [{"name": "rollout", "title": "Rollout", "probe": {"command": "echo 'cluster unreachable' >&2; exit 3"}, "interval_s": 1}]}]);
    let r = http.put(format!("{api}/settings/kits")).json(&broken).send().await.unwrap();
    assert!(r.status().is_success());
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo deploy"}"#.into())]));
    mock.push(Reply::Text("Deployed.".into()));
    mock.push(Reply::Text("The cluster's address changed; fixing the kit.".into()));
    let s4: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "deploy it", "labels": ["team:core"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t = eventually("a repair", async || {
        let f: Followups = http.get(format!("{api}/sessions/{}/followups", s4.id)).send().await.ok()?.json().await.ok()?;
        f.trackers.into_iter().find(|t| t.repair.is_some())
    })
    .await;
    assert!(t.error.contains("cluster unreachable"), "{t:?}");
    let repair = eventually("the repair's question", async || {
        mock.requests().into_iter().map(|r| r.to_string()).find(|r| r.contains("may be broken") && r.contains("failed 3 times in a row"))
    })
    .await;
    assert!(repair.contains("settings.kits.put"), "{repair}");
    stop.cancel();
}

/// The session's last event ends a turn (a new session is idle for a moment
/// before its first turn starts).
async fn turn_ended(http: &reqwest::Client, api: &str, id: &str) -> bool {
    let ev: Vec<serde_json::Value> = match http.get(format!("{api}/sessions/{id}/events?after=0")).send().await {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => return false,
    };
    ev.last().is_some_and(|e| e["type"] == "turn_ended")
}
