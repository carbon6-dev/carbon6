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
        (p["note"]["text"] == "Phase 1: the guide is being written." && !c6_head::projects::writing(&head.state).await).then_some(p)
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

/// Phases (docs/projects.md §7, §8): they change only when you say so; an
/// agent offers "phase looks done" once the phase's work is closed; Plan
/// the next phase makes planning work whose session proposes the phase's
/// work for you to accept; and sessions in project work are told the
/// project's context.
#[tokio::test]
async fn phases_move_on_when_you_say_and_sessions_know_their_project() {
    use c6_testkit::{MockLlm, Reply, eventually};
    use c6_worker::{ConnState, Identity, Worker, conn, enroll};
    const WHERE: &str = "where we are\\\" note on a long-running project";
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
    let settings = |context: bool| json!({"guess": false, "notes": true, "quiet_days": 3, "nudge": true, "needs_hours": 4, "project_context": context});
    ok(send(M::PUT, "settings/work".into(), settings(true)).await);
    ok(send(
        M::PUT,
        "settings/routes".into(),
        json!([{"name": "projects", "applies_to": ["task:projects", "task:work"], "connection": conn["id"], "model": "small", "enabled": true}]),
    )
    .await);
    for _ in 0..30 {
        mock.push_about(
            WHERE,
            Reply::Text(
                json!({"note": ["The guide is out.", "Next: the pilot"], "finished": false, "phase_done": true, "phase_evidence": "The guide was published"})
                    .to_string(),
            ),
        );
        mock.push_about(
            "Keep the user's note on a piece of work",
            Reply::Text(json!({"done": "", "left": "", "next": "", "finished": false}).to_string()),
        );
    }
    let start = |prompt: &str, work: &str| {
        mock.push_about_all(&["You are carbon6", prompt], Reply::Text("ok".into()));
        json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn["id"], "model": "m", "prompt": prompt, "work": work})
    };

    // 1. Phases: the first added is current; you order and date them.
    let p = ok(send(M::POST, "projects".into(), json!({"title": "Realms for the team", "purpose": "The team uses realms daily"})).await);
    let pid = p["id"].as_str().unwrap().to_owned();
    let plan_doc = p["docs"][0]["doc"].as_str().unwrap().to_owned();
    let phase = |p: &Value, i: usize| p["phases"][i].clone();
    let p = ok(send(
        M::POST,
        format!("projects/{pid}/phases"),
        json!({"action": "add", "title": "Write the guide", "done_when": "The guide is published"}),
    )
    .await);
    let p1 = phase(&p, 0)["id"].as_str().unwrap().to_owned();
    assert_eq!(phase(&p, 0)["state"], "current");
    let r = send(M::POST, format!("projects/{pid}/phases"), json!({"action": "add", "title": "Pilot", "target": "Dec 1"})).await;
    assert_eq!(r.0, StatusCode::BAD_REQUEST, "a target is a date");
    let p = ok(send(M::POST, format!("projects/{pid}/phases"), json!({"action": "add", "title": "Pilot", "target": "2026-12-01"})).await);
    let p2 = phase(&p, 1)["id"].as_str().unwrap().to_owned();
    assert_eq!(phase(&p, 1)["state"], "upcoming");
    let p = ok(send(M::POST, format!("projects/{pid}/phases"), json!({"action": "move", "phase": p2, "to": 0})).await);
    assert_eq!(phase(&p, 0)["id"], p2.as_str());
    ok(send(M::POST, format!("projects/{pid}/phases"), json!({"action": "move", "phase": p2, "to": 1})).await);

    // 2. Work in the first phase; a session on it is told the project's context.
    let a = ok(send(M::POST, "work".into(), json!({"title": "Draft the guide"})).await)["id"].as_str().unwrap().to_owned();
    assert_eq!(send(M::POST, format!("projects/{pid}/work"), json!({"work": a, "phase": "nope"})).await.0, StatusCode::BAD_REQUEST);
    ok(send(M::POST, format!("projects/{pid}/work"), json!({"work": a, "phase": p1})).await);
    let s1 = ok(send(M::POST, "sessions".into(), start("Write the first draft", &a)).await)["id"].as_str().unwrap().to_owned();
    let first = eventually("the session's first turn", async || {
        mock.requests()
            .into_iter()
            .map(|r| r["messages"].to_string())
            .find(|m| m.contains("Write the first draft") && m.contains("You are carbon6"))
    })
    .await;
    assert!(first.contains("is part of the project Realms for the team"), "{first}");
    assert!(
        first.contains("Current phase: Write the guide (done when: The guide is published)") && first.contains(&format!("doc:{plan_doc}")),
        "{first}"
    );

    // 3. Its work closes: the phase looks done. You say not yet, and it isn't offered again for the same evidence.
    ok(send(M::PATCH, format!("work/{a}"), json!({"status": "done"})).await);
    let p = eventually("phase looks done", async || {
        let p = get(format!("projects/{pid}")).await;
        (p["phases"][0]["offer"]["evidence"] == "The guide was published").then_some(p)
    })
    .await;
    assert_eq!((p["state"].as_str(), p["why"].as_str()), (Some("needs_you"), Some("Write the guide looks done: The guide was published")));
    assert_eq!(p["note"]["text"], "The guide is out.\nNext: the pilot", "a note given as lines");
    let p = ok(send(M::POST, format!("projects/{pid}/phases"), json!({"action": "dismiss", "phase": p1})).await);
    assert!(p["phases"][0]["offer"].is_null() && p["phases"][0]["dismissed"] == "The guide was published", "{p}");
    eventually("no note being written", async || (!c6_head::projects::writing(&head.state).await).then_some(())).await;
    let asked = |m: &MockLlm| m.requests().iter().filter(|r| r["messages"].to_string().contains(WHERE)).count();
    let before = asked(&mock);
    ok(send(M::POST, format!("projects/{pid}/refresh"), json!({})).await);
    eventually("the note written again", async || (asked(&mock) > before && !c6_head::projects::writing(&head.state).await).then_some(()))
        .await;
    assert!(get(format!("projects/{pid}")).await["phases"][0]["offer"].is_null(), "not offered again");

    // 4. You move on: the phase is done, the next current.
    let p = ok(send(M::POST, format!("projects/{pid}/phases"), json!({"action": "advance"})).await);
    assert_eq!((phase(&p, 0)["state"].as_str(), phase(&p, 1)["state"].as_str()), (Some("done"), Some("current")));

    // 5. Plan the next phase: planning work in it, and a session's first message.
    let plan = ok(send(M::POST, format!("projects/{pid}/plan"), json!({})).await);
    let pw = plan["work"].as_str().unwrap().to_owned();
    let text = plan["text"].as_str().unwrap().to_owned();
    assert!(
        text.contains("Plan the phase \"Pilot\"") && text.contains(&format!("doc:{plan_doc}")) && text.contains("Draft the guide"),
        "{text}"
    );
    assert_eq!(plan["seed"], s1.as_str(), "starts like the project's latest session");
    let w = get(format!("work/{pw}")).await;
    assert_eq!(
        (w["title"].as_str(), w["project"].as_str(), w["phase"].as_str()),
        (Some("Plan Pilot"), Some(pid.as_str()), Some(p2.as_str()))
    );
    // Its session proposes the phase's work; you accept one and dismiss one.
    let s2 = ok(send(M::POST, "sessions".into(), start("Plan the pilot", &pw)).await)["id"].as_str().unwrap().to_owned();
    let propose = |t: &str| json!({"action": "propose", "title": t, "done_when": "the team says so"});
    let got = c6_head::work::for_session(&head.state, &s2, &propose("Invite the pilot team")).await.unwrap();
    assert_eq!(got["did"], "proposed");
    c6_head::work::for_session(&head.state, &s2, &propose("Write the FAQ")).await.unwrap();
    assert!(c6_head::work::for_session(&head.state, &s2, &propose("Write the FAQ")).await.is_err(), "once");
    let p = get(format!("projects/{pid}")).await;
    let proposed = p["proposed"].as_array().unwrap().clone();
    assert_eq!(
        (proposed.len(), proposed[0]["phase"].as_str(), proposed[0]["by"].as_str()),
        (2, Some(p2.as_str()), Some(format!("session:{s2}").as_str()))
    );
    ok(send(M::POST, format!("projects/{pid}/proposed"), json!({"proposal": proposed[0]["id"], "accept": true})).await);
    let p = ok(send(M::POST, format!("projects/{pid}/proposed"), json!({"proposal": proposed[1]["id"]})).await);
    assert!(p["proposed"].is_null(), "{p}");
    let invited = p["work"].as_array().unwrap().iter().find(|w| w["title"] == "Invite the pilot team").cloned().unwrap();
    assert_eq!(invited["phase"], p2.as_str());
    assert!(!p["work"].to_string().contains("Write the FAQ"));

    // 6. Bring the plan up to date after the phase that's done.
    let up = ok(send(M::POST, format!("projects/{pid}/plan"), json!({"update": true})).await);
    let t = up["text"].as_str().unwrap();
    assert!(t.contains("The phase \"Write the guide\"") && t.contains("the next is Pilot") && t.contains("Draft the guide"), "{t}");
    assert_eq!(get(format!("work/{}", up["work"].as_str().unwrap())).await["title"], "Update the plan after Write the guide");

    // 7. A phase removed: its work stays in the project, in none.
    ok(send(M::POST, format!("projects/{pid}/phases"), json!({"action": "remove", "phase": p2})).await);
    let w = get(format!("work/{}", invited["id"].as_str().unwrap())).await;
    assert_eq!((w["project"].as_str(), w["phase"].as_str()), (Some(pid.as_str()), None));

    // 8. A realm that turns the context off: sessions get only what they're given.
    ok(send(M::PUT, "settings/work".into(), settings(false)).await);
    ok(send(M::POST, "sessions".into(), start("Send the invites", invited["id"].as_str().unwrap())).await);
    let first = eventually("the third session's first turn", async || {
        mock.requests()
            .into_iter()
            .filter_map(|r| r["messages"].as_array()?.last()?["content"].as_str().map(str::to_owned))
            .find(|m| m.starts_with("Send the invites"))
    })
    .await;
    assert_eq!(first, "Send the invites", "only what it was given");
    stop.cancel();
}

/// How work joins projects (docs/projects.md §5): a new work item gets an
/// agent's project guess (shown as a guess until you keep it), only when
/// the realm has active projects; and "Find work in what's open" also
/// finds projects in your work.
#[tokio::test]
async fn new_work_finds_its_project_and_projects_are_found_in_work() {
    use c6_testkit::{MockLlm, Reply, eventually};
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
    ok(send(
        M::PUT,
        "settings/routes".into(),
        json!([{"name": "projects", "applies_to": ["task:projects"], "connection": conn["id"], "model": "small", "enabled": true}]),
    )
    .await);
    const GUESS: &str = "decide which project it belongs to";
    let asked = |m: &MockLlm| m.requests().iter().filter(|r| r["messages"].to_string().contains(GUESS)).count();

    // No active project: no guess asked.
    let w0 = ok(send(M::POST, "work".into(), json!({"title": "Tidy the garage"})).await);
    eventually("no guess under way", async || (!c6_head::work::guessing(&head.state).await).then_some(())).await;
    assert_eq!(asked(&mock), 0);
    assert_eq!(get(format!("work/{}", w0["id"].as_str().unwrap())).await["project"], Value::Null);

    // With one, a new work item gets the agent's guess, marked as one.
    let p = ok(send(
        M::POST,
        "projects".into(),
        json!({"title": "Realms for the team", "purpose": "The team uses realms daily", "plan": false}),
    )
    .await);
    let pid = p["id"].as_str().unwrap().to_owned();
    mock.push_about(GUESS, Reply::Text(json!({"project": pid}).to_string()));
    let w = ok(send(M::POST, "work".into(), json!({"title": "Write the realm guide"})).await);
    let wid = w["id"].as_str().unwrap().to_owned();
    let w = eventually("its project guessed", async || {
        let w = get(format!("work/{wid}")).await;
        (w["project"] == pid.as_str()).then_some(w)
    })
    .await;
    assert_eq!(w["project_guess"], true);
    let asked_text = mock.requests().iter().map(|r| r["messages"].to_string()).find(|m| m.contains(GUESS)).unwrap();
    assert!(asked_text.contains("Write the realm guide") && asked_text.contains(&format!("{pid}: Realms for the team")), "{asked_text}");
    // You keep it: no longer a guess.
    let w = ok(send(M::PATCH, format!("work/{wid}"), json!({"project": pid})).await);
    assert!(w["project_guess"].is_null(), "{w}");
    // Made in a project: no guess at all.
    let before = asked(&mock);
    ok(send(M::POST, format!("projects/{pid}/work"), json!({"title": "Invite the team"})).await);
    eventually("no guess under way", async || (!c6_head::work::guessing(&head.state).await).then_some(())).await;
    assert_eq!(asked(&mock), before);
    // A guess naming no project of the realm changes nothing.
    mock.push_about(GUESS, Reply::Text(json!({"project": "nope"}).to_string()));
    let w2 = ok(send(M::POST, "work".into(), json!({"title": "Book the dentist"})).await);
    eventually("the guess answered", async || (!c6_head::work::guessing(&head.state).await).then_some(())).await;
    assert_eq!(get(format!("work/{}", w2["id"].as_str().unwrap())).await["project"], Value::Null);

    // Projects found in work: only real work in no project, two or more.
    let mut ids = Vec::new();
    for t in ["Migrate the billing tables", "Backfill billing history", "Switch billing reads"] {
        ids.push(ok(send(M::POST, "work".into(), json!({"title": t, "labels": ["billing"]})).await)["id"].as_str().unwrap().to_owned());
    }
    eventually("no guess under way", async || (!c6_head::work::guessing(&head.state).await).then_some(())).await;
    const FIND: &str = "Find those groups";
    mock.push_about(
        FIND,
        Reply::Text(
            json!({"projects": [
                {"title": "Billing migration", "purpose": "Billing runs on the new tables", "work": [ids[0], ids[1], ids[2], wid, "nope"]},
                {"title": "Lonely", "purpose": "x", "work": [w0["id"]]}
            ]})
            .to_string(),
        ),
    );
    let found = ok(send(M::POST, "projects/propose".into(), json!({})).await);
    let asked_text = mock.requests().iter().map(|r| r["messages"].to_string()).find(|m| m.contains(FIND)).unwrap();
    assert!(asked_text.contains("billing (3 pieces)") && !asked_text.contains("Write the realm guide"), "{asked_text}");
    assert_eq!(found.as_array().unwrap().len(), 1, "one piece isn't a project: {found}");
    assert_eq!(found[0]["title"], "Billing migration");
    let names: Vec<&str> = found[0]["work"].as_array().unwrap().iter().map(|w| w[0].as_str().unwrap()).collect();
    assert_eq!(names, [ids[0].as_str(), ids[1].as_str(), ids[2].as_str()], "only work in no project");
    // You accept it: the project is made with its work.
    let made =
        ok(send(M::POST, "projects".into(), json!({"title": found[0]["title"], "purpose": found[0]["purpose"], "work": names})).await);
    assert_eq!(made["work"].as_array().unwrap().len(), 3);
}
