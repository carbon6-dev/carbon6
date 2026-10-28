//! Task cards (docs/task-cards.md): an agent proposes work for a separate
//! session; the card shows in its session and in Decisions, and "Start it"
//! starts that session where the first one worked, with the card's prompt.

use c6_api::{Connection, Decision, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn agents_propose_tasks_you_start_with_one_click() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let wid = worker.identity.meta.worker_id.0;
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let start = |prompt: &'static str| {
        let (http, api, conn, cwd) = (http.clone(), api.clone(), conn.id.clone(), project.path().to_owned());
        async move {
            http.post(format!("{api}/sessions"))
                .json(&json!({"worker_id": wid, "cwd": cwd, "connection_id": conn, "model": "m", "prompt": prompt}))
                .send()
                .await
                .unwrap()
                .json::<Session>()
                .await
                .unwrap()
        }
    };
    let ended = |id: String| async move {
        eventually("the turn to end", async || {
            let ev: Vec<Event> = serde_json::from_value(get(format!("sessions/{id}/events")).await).ok()?;
            ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { .. })).then_some(ev)
        })
        .await
    };

    // 1. While fixing a test, the agent proposes a cleanup (twice: one card).
    let card = json!({"title": "Remove the dead retry module", "why": "src/retry.rs is never called since the client retries itself.",
                      "prompt": "Delete src/retry.rs and its tests; nothing calls it (grep retry::). Run the tests."})
    .to_string();
    mock.push(Reply::ToolCalls(vec![("task_propose".into(), card.clone())]));
    mock.push(Reply::ToolCalls(vec![("task_propose".into(), card)]));
    mock.push(Reply::Text("Fixed the test; I proposed removing the dead retry module separately.".into()));
    let s1 = start("fix the flaky test").await;
    let events = ended(s1.id.clone()).await;
    let EventKind::Started { tools, system_prompt, .. } = &events[0].kind else { panic!("{:?}", events[0]) };
    assert!(tools.iter().any(|t| t == "task_propose") && tools.iter().any(|t| t == "task_withdraw"), "{tools:?}");
    assert!(system_prompt.contains("# Task cards") && system_prompt.contains("dead code"), "the guidance is in its instructions");
    let replies = mock.requests()[2]["messages"].to_string();
    assert!(replies.contains("task card #1 proposed") && replies.contains("already open"), "{replies}");

    let cards: Vec<Decision> = serde_json::from_value(get(format!("sessions/{}/decisions", s1.id)).await).unwrap();
    assert_eq!(cards.len(), 1, "{cards:?}");
    let c = &cards[0];
    assert_eq!((c.kind.as_str(), c.priority.as_str(), c.group.as_str(), c.waiting, c.count), ("task", "low", "Suggested tasks", false, 2));
    assert_eq!(c.title, "Remove the dead retry module");
    assert!(c.document.contains("never called"));
    let listed: Vec<Decision> = serde_json::from_value(get("decisions".into()).await).unwrap();
    assert!(listed.iter().any(|d| d.id == c.id), "in Decisions too");

    // 2. Start it: a new session does it, where the first one worked.
    mock.push(Reply::Text("Removed it; the tests pass.".into()));
    let r = http.post(format!("{api}/decisions/{}/answer", c.id)).json(&json!({"option": "Start it"})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let done: Decision = serde_json::from_value(get(format!("decisions/{}", c.id)).await).unwrap();
    assert_eq!((done.state.as_str(), done.sessions.len()), ("answered", 1), "{done:?}");
    let task = done.sessions[0].clone();
    ended(task.clone()).await;
    let detail = get(format!("sessions/{task}")).await;
    let s = &detail["session"];
    assert_eq!(s["title"], "Remove the dead retry module", "{s}");
    assert_eq!(s["cwd"].as_str().map(std::path::Path::new), Some(project.path().canonicalize().unwrap().as_path()), "{s}");
    let labels = s["labels"].to_string();
    assert!(labels.contains("task-card") && labels.contains(&format!("from:{}", s1.id)), "{labels}");
    let asked = mock.requests().last().unwrap()["messages"].to_string();
    assert!(asked.contains("Delete src/retry.rs and its tests"), "its prompt is the card's: {asked}");

    // 3. Done after all: the agent withdraws its card.
    mock.push(Reply::ToolCalls(vec![(
        "task_propose".into(),
        json!({"title": "Document the cache flags", "why": "README lacks them.", "prompt": "Add the cache flags to README."}).to_string(),
    )]));
    mock.push(Reply::ToolCalls(vec![("task_withdraw".into(), json!({"id": 2, "why": "I documented them myself."}).to_string())]));
    mock.push(Reply::Text("Documented them.".into()));
    let s2 = start("tidy the README").await;
    ended(s2.id.clone()).await;
    let gone: Decision = serde_json::from_value(get("decisions/2".into()).await).unwrap();
    assert_eq!((gone.title.as_str(), gone.state.as_str()), ("Document the cache flags", "withdrawn"), "{gone:?}");
    let left: Vec<Decision> = serde_json::from_value(get(format!("sessions/{}/decisions", s2.id)).await).unwrap();
    assert!(left.is_empty(), "{left:?}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
