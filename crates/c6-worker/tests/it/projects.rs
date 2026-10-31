//! Projects (docs/projects.md): work moves in and out of a project (a work
//! item is in one at most), each work item shows its project, and projects
//! merge and close.

use c6_head::HeadConfig;
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};

#[tokio::test]
async fn work_moves_in_and_out_of_projects() {
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
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.request(method, &url).json(&body).send().await.unwrap();
            let s = r.status();
            (s, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let ok = |(s, v): (StatusCode, Value)| {
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(&url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let work = |title: &str| json!({"title": title});
    let a = ok(send(M::POST, "work".into(), work("Draft the realm guide")).await)["id"].as_str().unwrap().to_owned();
    let b = ok(send(M::POST, "work".into(), work("Invite the team")).await)["id"].as_str().unwrap().to_owned();
    let c = ok(send(M::POST, "work".into(), work("Fix the login page")).await)["id"].as_str().unwrap().to_owned();

    // A project, with work brought in at once.
    let p =
        ok(send(M::POST, "projects".into(), json!({"title": "Realms for the team", "purpose": "The team uses realms daily", "work": [a]}))
            .await);
    let pid = p["id"].as_str().unwrap().to_owned();
    assert_eq!((p["status"].as_str(), p["work"][0]["id"].as_str()), (Some("active"), Some(a.as_str())));
    // Work joins from the project, and from the work item.
    ok(send(M::POST, format!("projects/{pid}/work"), json!({"work": b})).await);
    ok(send(M::PATCH, format!("work/{c}"), json!({"project": pid})).await);
    let p = get(format!("projects/{pid}")).await;
    assert_eq!(p["work"].as_array().unwrap().len(), 3);
    assert_eq!(p["why"], "3 pieces of work open");
    // Each work item shows its project.
    let w = get(format!("work/{a}")).await;
    assert_eq!((w["project"].as_str(), w["project_title"].as_str()), (Some(pid.as_str()), Some("Realms for the team")));
    // A work item is in one project at most: moving it moves it out.
    let q = ok(send(M::POST, "projects".into(), json!({"title": "Login revamp"})).await);
    let qid = q["id"].as_str().unwrap().to_owned();
    ok(send(M::POST, format!("projects/{qid}/work"), json!({"work": c})).await);
    assert_eq!(get(format!("projects/{pid}")).await["work"].as_array().unwrap().len(), 2);
    assert_eq!(get(format!("work/{c}")).await["project_title"], "Login revamp");
    // Out of a project: from the project, or the work item.
    ok(send(M::POST, format!("projects/{pid}/detach"), json!({"work": b})).await);
    assert!(get(format!("work/{b}")).await.get("project").is_none());
    ok(send(M::PATCH, format!("work/{c}"), json!({"project": ""})).await);
    assert_eq!(get(format!("projects/{qid}")).await["work"].as_array().unwrap().len(), 0);
    // A phase the project doesn't have, or a project that isn't there.
    assert_eq!(send(M::POST, format!("projects/{pid}/work"), json!({"work": b, "phase": "nope"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(send(M::PATCH, format!("work/{b}"), json!({"project": "nope"})).await.0, StatusCode::BAD_REQUEST);

    // Merge: the other's work comes along; it's gone.
    ok(send(M::POST, format!("projects/{qid}/work"), json!({"work": c})).await);
    let m = ok(send(M::POST, format!("projects/{pid}/merge"), json!({"from": qid})).await);
    assert_eq!(m["work"].as_array().unwrap().len(), 2);
    assert_eq!(get(format!("work/{c}")).await["project"], pid.as_str());
    assert_eq!(send(M::GET, format!("projects/{qid}"), Value::Null).await.0, StatusCode::NOT_FOUND);
    // Closed: out of the list, behind closed=true; its history is kept.
    ok(send(M::PATCH, format!("projects/{pid}"), json!({"status": "done"})).await);
    assert_eq!(get("projects".into()).await.as_array().unwrap().len(), 0);
    let closed = get("projects?closed=true".into()).await;
    assert_eq!((closed[0]["state"].as_str(), closed[0]["closed_at"].is_i64()), (Some("done"), true));
    let h = get(format!("collections/projects/records/{pid}/history")).await;
    assert!(h.as_array().is_some_and(|v| v.len() >= 3), "{h}");
}

/// A project's plan, its documents with roles, "where we are" written by a
/// model, its state from its work (stalled once nothing is open), and a
/// session in its work writing its documents.
#[tokio::test]
async fn projects_keep_their_plan_documents_and_where_they_are() {
    use c6_testkit::{MockLlm, Reply, eventually};
    use c6_worker::{ConnState, Identity, Worker, conn, enroll};
    const WHERE: &str = "where we are\\\" note on a long-running project";
    const WORK_NOTE: &str = "Keep the user's note on a piece of work";
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
    let (tx, mut rx) = tokio::sync::watch::channel(ConnState::Connecting);
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.request(method, &url).json(&body).send().await.unwrap();
            let s = r.status();
            (s, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let ok = |(s, v): (StatusCode, Value)| {
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(&url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let conn = ok(send(M::POST, "connections".into(), json!({"name": "m", "base_url": mock.base_url()})).await);
    // Projects' notes go to a small model; work's notes aren't asked for.
    ok(send(M::PUT, "settings/work".into(), json!({"guess": false, "notes": true, "quiet_days": 3, "nudge": true, "needs_hours": 4}))
        .await);
    ok(send(
        M::PUT,
        "settings/routes".into(),
        json!([{"name": "projects", "applies_to": ["task:projects"], "connection": conn["id"], "model": "small", "enabled": true}]),
    )
    .await);
    for _ in 0..8 {
        mock.push_about(WHERE, Reply::Text(json!({"note": "Phase 1: the guide is being written.", "finished": false}).to_string()));
        mock.push_about(WORK_NOTE, Reply::Text(json!({"done": "", "left": "", "next": "Write it", "finished": false}).to_string()));
    }

    // 1. A project starts with its plan, from `project.plan`.
    let p = ok(send(M::POST, "projects".into(), json!({"title": "Realms for the team", "purpose": "The team uses realms daily"})).await);
    let pid = p["id"].as_str().unwrap().to_owned();
    assert_eq!((p["docs"][0]["role"].as_str(), p["docs"][0]["title"].as_str()), (Some("plan"), Some("Realms for the team: plan")));
    let plan = p["docs"][0]["doc"].as_str().unwrap().to_owned();
    let md = get(format!("docs/{plan}/read?view=markdown")).await;
    assert!(md["tabs"][0]["markdown"].as_str().unwrap().contains("## Purpose\n\nThe team uses realms daily"), "{md}");
    assert_eq!((p["state"].as_str(), p["why"].as_str()), (Some("stalled"), Some("No work in it yet")));
    // One plan at most: a new plan leaves the old one without a role.
    let other = ok(send(M::POST, "docs".into(), json!({"title": "Plan v2"})).await);
    let p = ok(send(M::POST, format!("projects/{pid}/docs"), json!({"doc": other["id"], "role": "plan"})).await);
    assert_eq!(p["docs"][0]["title"], "Plan v2", "the plan first");
    assert_eq!(p["docs"][1]["role"], Value::Null, "{p}");
    ok(send(M::POST, format!("projects/{pid}/docs"), json!({"doc": plan, "role": "plan"})).await);
    assert_eq!(send(M::POST, format!("projects/{pid}/docs"), json!({"doc": plan, "role": "boss"})).await.0, StatusCode::BAD_REQUEST);
    ok(send(M::POST, format!("projects/{pid}/links"), json!({"ref": "https://example.com/rfc", "title": "The RFC"})).await);

    // 2. A session in work of the project edits its plan without asking, and
    // what it makes lands on the project.
    let edit = json!({"action": "edit", "id": plan, "ops": [{"op": "replace", "id": md["tabs"][0]["markdown"].as_str().map(|_| get(format!("docs/{plan}/read?view=search&q=First%20phase"))).unwrap().await["hits"][0]["id"], "find": "First phase", "with": "Home realm"}]});
    let create = json!({"action": "create", "title": "Realm guide", "markdown": "# Realm guide"});
    mock.push_about("You are carbon6", Reply::ToolCalls(vec![("doc".into(), edit.to_string()), ("doc".into(), create.to_string())]));
    mock.push_about("You are carbon6", Reply::Text("Done.".into()));
    let w = ok(send(M::POST, "work".into(), json!({"title": "Write the realm guide"})).await);
    let wid = w["id"].as_str().unwrap().to_owned();
    ok(send(M::PATCH, format!("work/{wid}"), json!({"project": pid})).await);
    let s = ok(send(
        M::POST,
        "sessions".into(),
        json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn["id"], "model": "m",
               "prompt": "update the plan", "work": wid}),
    )
    .await);
    let sid = s["id"].as_str().unwrap().to_owned();
    eventually("the plan edited and the guide made", async || {
        let p = get(format!("projects/{pid}")).await;
        let plan_md = get(format!("docs/{plan}/read?view=markdown")).await;
        let edited = plan_md["tabs"][0]["markdown"].as_str()?.contains("1. Home realm");
        let guide = p["docs"].as_array()?.iter().any(|d| d["title"] == "Realm guide");
        (edited && guide).then_some(())
    })
    .await;
    assert!(get("decisions".into()).await.as_array().unwrap().iter().all(|d| !d["title"].as_str().unwrap_or("").contains("edit")));

    // 3. Where we are: written by the routed model when the project moves.
    let p = eventually("its note", async || {
        let p = get(format!("projects/{pid}")).await;
        (p["note"]["text"] == "Phase 1: the guide is being written." && !c6_head::projects::writing()).then_some(p)
    })
    .await;
    let asked: Vec<Value> = mock.requests().into_iter().filter(|r| r["messages"].to_string().contains(WHERE)).collect();
    assert!(asked.iter().all(|r| r["model"] == "small"));
    eventually("a note that read the new document", async || {
        mock.requests()
            .iter()
            .any(|r| r["messages"].to_string().contains(WHERE) && r["messages"].to_string().contains("Realm guide"))
            .then_some(())
    })
    .await;
    assert_ne!(p["state"], "stalled", "{p}");
    // The work tool shows the project.
    let got = c6_head::work::for_session(&head.state, &sid, &json!({"action": "get"})).await.unwrap();
    assert_eq!(got["work"][0]["project"]["title"], "Realms for the team", "{got}");
    assert!(got["work"][0]["project"]["documents"].to_string().contains("\"role\":\"plan\""), "{got}");

    // 4. Its last work closes: stalled, and you're told once.
    ok(send(M::PATCH, format!("work/{wid}"), json!({"status": "done"})).await);
    let p = get(format!("projects/{pid}")).await;
    assert_eq!((p["state"].as_str(), p["why"].as_str()), (Some("stalled"), Some("All its work is closed, and nothing new is open")));
    assert_eq!(c6_head::projects::sweep(&head.state).await.unwrap(), 1);
    assert_eq!(c6_head::projects::sweep(&head.state).await.unwrap(), 0, "once per state");
    assert_eq!(get(format!("projects/{pid}")).await["told"]["state"], "stalled");
    stop.cancel();
}
