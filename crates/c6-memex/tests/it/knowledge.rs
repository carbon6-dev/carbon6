//! Environment knowledge and action items (docs/follow-through.md §6–7):
//! agents record what they learn with evidence (observed facts need it),
//! the same claim again adds evidence, a contradiction disputes the old
//! fact and raises an action item, later sessions start with the knowledge
//! in their brief (with status), you settle facts (confirm, correct), and an
//! action item can start a session that resolves it.

use std::time::Duration;

use c6_api::{Connection, Decision, Fact, Session, SessionDetail, SessionStatus};
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

async fn node(head: &c6_head::Head, _memex: bool, stop: &CancellationToken) -> (std::sync::Arc<Worker>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, Some("build-1")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (w, dir)
}

#[tokio::test]
async fn knowledge_and_action_items() {
    crate::extract_here();
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
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
    let stop = CancellationToken::new();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (w, _wd) = node(&head, false, &stop).await;
    let wid = w.identity.meta.worker_id.0;
    let rpc = |method: &str, params: Value| {
        let (http, url, method) = (http.clone(), format!("{api}/memex/rpc"), method.to_owned());
        async move {
            let r = http.post(url).json(&json!({"method": method, "params": params})).send().await.unwrap();
            let ok = r.status().is_success();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(ok, "{method}: {v}");
            v
        }
    };
    let session = |prompt: &str| {
        let (http, api) = (http.clone(), api.clone());
        let body =
            json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": prompt, "labels": ["project:app"]});
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
    let results = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events?after=0")).send().await.unwrap().json().await.unwrap();
            ev.into_iter()
                .filter(|e| e["type"] == "tool_result")
                .map(|e| (e["output"].as_str().unwrap_or("").to_owned(), e["is_error"] == true))
                .collect::<Vec<_>>()
        }
    };

    // A session learns about CI: one observed fact (with evidence), one
    // without evidence (refused), one inferred.
    mock.push(Reply::ToolCalls(vec![
        (
            "knowledge_record".into(),
            json!({"claim": "CI runs as Argo Workflows in namespace ci", "about": ["ci:argo", "project:app"], "volatility": "stable",
                   "evidence": {"command": "argo list -n ci", "excerpt": "ci-7f2  Succeeded  2m", "where": "build-1"},
                   "check": {"command": "argo list -n ci --limit 1", "on": "worker", "worker": "build-1"}})
            .to_string(),
        ),
        ("knowledge_record".into(), json!({"claim": "CI is fast", "about": ["ci:argo"]}).to_string()),
        (
            "knowledge_record".into(),
            json!({"claim": "Runs carry the commit as label git-sha", "about": ["ci:argo"], "status": "inferred", "volatility": "stable",
                   "labels": ["project:app"], "conditions": "for pushes to branches"})
            .to_string(),
        ),
        // Not knowing something is an action item, not a stop.
        ("todo_add".into(), json!({"kind": "provide", "title": "Argo token for build-1", "detail": "Assumed the worker's kubeconfig meanwhile", "context": {"things": ["ci:argo"], "worker": "build-1"}, "group": "argo-token"}).to_string()),
    ]));
    mock.push(Reply::Text("Noted what I learned.".into()));
    let s1 = session("figure out our CI").await;
    idle(s1.id.clone()).await;
    let r = results(s1.id.clone()).await;
    assert!(r[0].0.starts_with("recorded fact") && !r[0].1, "{r:?}");
    assert!(r[1].1 && r[1].0.contains("needs evidence"), "observed facts need evidence: {r:?}");
    assert!(r[2].0.contains("(inferred)"), "{r:?}");
    assert!(r[3].0.starts_with("action item #"), "{r:?}");
    let facts: Vec<Fact> = serde_json::from_value(rpc("memex.knowledge.list", json!({"about": ["ci:argo"]})).await).unwrap();
    assert_eq!(facts.len(), 2);
    let argo = facts.iter().find(|f| f.claim.starts_with("CI runs as Argo")).unwrap().clone();
    assert_eq!((argo.status.as_str(), argo.evidence.len(), argo.made_by.starts_with("session:")), ("observed", 1, true));

    // The next session starts knowing it, with how each fact is known.
    mock.push(Reply::ToolCalls(vec![(
        "knowledge_record".into(),
        json!({"claim": "CI runs as Argo Workflows in namespace ci.", "about": ["project:app", "ci:argo"], "evidence": {"command": "kubectl get wf -n ci", "excerpt": "ci-8a1"}}).to_string(),
    )]));
    mock.push(Reply::Text("Same CI.".into()));
    let s2 = session("what is our CI?").await;
    idle(s2.id.clone()).await;
    let first = mock.requests().iter().map(|r| r.to_string()).find(|r| r.contains("what is our CI?")).unwrap();
    assert!(first.contains("## Environment knowledge"), "{first}");
    assert!(first.contains("[observed · today] CI runs as Argo Workflows in namespace ci"), "{first}");
    assert!(first.contains("[inferred · today · only project:app · for pushes to branches] Runs carry the commit"), "{first}");
    // The same claim again: more evidence, not a duplicate.
    let facts: Vec<Fact> = serde_json::from_value(rpc("memex.knowledge.list", json!({"about": ["ci:argo"]})).await).unwrap();
    assert_eq!(facts.len(), 2);
    assert_eq!(facts.iter().find(|f| f.id == argo.id).unwrap().evidence.len(), 2);
    // Facts are found by search too.
    let hits: Vec<Value> = http
        .get(format!("{api}/memex/search"))
        .query(&[("q", "argo namespace"), ("kinds", "fact")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!hits.is_empty(), "facts are searchable");

    // A contradiction: the old fact is disputed and you get an action item.
    mock.push(Reply::ToolCalls(vec![(
        "knowledge_record".into(),
        json!({"claim": "CI runs as Argo Workflows in namespace ci-v2", "about": ["ci:argo", "project:app"],
               "evidence": {"command": "argo list -n ci", "excerpt": "No workflows found"}, "contradicts": argo.id})
        .to_string(),
    )]));
    mock.push(Reply::Text("The namespace moved.".into()));
    let s3 = session("check CI").await;
    idle(s3.id.clone()).await;
    let old: Vec<Fact> = serde_json::from_value(rpc("memex.knowledge.get", json!({"ids": [argo.id]})).await).unwrap();
    assert_eq!(old[0].status, "disputed");
    let todos: Vec<Decision> = eventually("the action items", async || {
        let t: Vec<Decision> = http.get(format!("{api}/decisions")).send().await.ok()?.json().await.ok()?;
        (t.len() == 2).then_some(t)
    })
    .await;
    let dispute = todos.iter().find(|t| t.kind == "dispute").expect("a dispute item");
    assert!(dispute.title.contains("namespace ci") && dispute.context["facts"].as_array().unwrap().len() == 2, "{dispute:?}");
    let token = todos.iter().find(|t| t.kind == "provide").unwrap();
    assert_eq!(token.context["worker"], "build-1");
    assert_eq!(token.source, format!("session:{}", s1.id));
    let count: Value = http.get(format!("{api}/decisions/count")).send().await.unwrap().json().await.unwrap();
    assert_eq!(count["open"], 2);

    // You settle it: the new one is right (confirmed), the old one retired.
    let new_id = dispute.context["facts"][1].as_str().unwrap().to_owned();
    rpc("memex.knowledge.set", json!({"id": new_id, "status": "reviewed"})).await;
    rpc("memex.knowledge.set", json!({"id": argo.id, "status": "retired"})).await;
    let r = http
        .post(format!("{api}/decisions/{}/answer", dispute.id))
        .json(&json!({"note": "the namespace moved to ci-v2"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let facts: Vec<Fact> = serde_json::from_value(rpc("memex.knowledge.get", json!({"ids": [argo.id, new_id]})).await).unwrap();
    assert_eq!((facts[0].status.as_str(), facts[0].valid_to.is_some()), ("retired", true));
    assert_eq!(facts[1].status, "reviewed");
    // Retired facts leave briefs; reviewed ones lead.
    mock.push(Reply::Text("ok".into()));
    let s4 = session("and now?").await;
    idle(s4.id.clone()).await;
    let brief = mock.requests().iter().map(|r| r.to_string()).find(|r| r.contains("and now?")).unwrap();
    assert!(brief.contains("[reviewed · today] CI runs as Argo Workflows in namespace ci-v2"), "{brief}");
    assert!(!brief.contains("namespace ci [fact"), "the retired fact is gone: {brief}");

    // An action item starts a session that settles it.
    mock.push(Reply::ToolCalls(vec![(
        "todo_resolve".into(),
        json!({"id": token.id, "resolution": "the user added ARGO_TOKEN"}).to_string(),
    )]));
    mock.push(Reply::Text("Settled.".into()));
    let work: Session = http.post(format!("{api}/decisions/{}/session", token.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(work.labels, vec!["decision".to_owned()]);
    idle(work.id.clone()).await;
    let asked = mock.requests().iter().map(|r| r.to_string()).find(|r| r.contains(&format!("Action item #{}", token.id))).unwrap();
    assert!(asked.contains("Argo token for build-1") && asked.contains("todo_resolve"), "{asked}");
    let t: Decision = http.get(format!("{api}/decisions/{}", token.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!((t.state.as_str(), t.note.as_str()), ("answered", "the user added ARGO_TOKEN"));
    assert_eq!(t.sessions, vec![work.id.clone()]);
    let count: Value = http.get(format!("{api}/decisions/count")).send().await.unwrap().json().await.unwrap();
    assert_eq!(count["open"], 0);
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
