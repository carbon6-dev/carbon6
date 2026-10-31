//! Labels: settings scoped by "applies to" reach only the sessions whose
//! labels (yours, and the automatic repo:/folder:/worker:/model:) match;
//! command variants follow label changes; the config endpoint explains.

use c6_api::{Connection, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn labels_scope_settings() {
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
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let p = project.path();

    let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(p).args(args).output().unwrap();
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    let put = |what: &str, v: Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{what}"));
        async move {
            let r = http.put(url).json(&v).send().await.unwrap();
            assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        }
    };
    put(
        "agents",
        json!([
            {"name": "acme-helper", "description": "Knows Acme", "prompt": "You help with Acme.", "applies_to": ["repo:github.com/acme"]},
            {"name": "py-helper", "description": "Knows Python", "prompt": "You help with Python.", "applies_to": ["python"]}
        ]),
    )
    .await;
    put(
        "skills",
        json!([
            {"name": "acme-style", "description": "Acme's code style", "body": "Tabs.", "applies_to": ["client:acme"]},
            {"name": "elsewhere", "description": "Not here", "body": "x", "applies_to": ["folder:/nowhere"]}
        ]),
    )
    .await;
    put(
        "commands",
        json!([
            {"name": "greet", "body": "Hello generic $ARGUMENTS"},
            {"name": "greet", "body": "Hello acme $ARGUMENTS", "applies_to": ["client:acme"]}
        ]),
    )
    .await;
    let wid = worker.identity.meta.worker_id.0;

    // Labels must be yours to set.
    let r = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m", "labels": ["repo:x"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    mock.push(Reply::Text("hi".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m",
                      "prompt": "/greet Ada", "labels": ["client:acme"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("first request", async || (mock.requests().len() == 1).then_some(())).await;
    let req = mock.requests()[0].clone();
    let text = req["messages"].to_string();
    assert!(text.contains("Hello acme Ada") && !text.contains("generic"), "{text}");
    assert!(text.contains("acme-style") && !text.contains("elsewhere"), "only matching skills: {text}");
    let task =
        req["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "task").unwrap()["function"]["description"].to_string();
    assert!(task.contains("acme-helper") && !task.contains("py-helper"), "{task}");

    // The session shows its labels, automatic ones included.
    let d: SessionDetail = eventually("started", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle).then_some(d)
    })
    .await;
    assert_eq!(d.session.labels, ["client:acme"]);
    assert!(d.session.auto_labels.contains(&"repo:github.com/acme/app".to_owned()), "{:?}", d.session.auto_labels);
    assert!(d.session.auto_labels.contains(&"model:m".to_owned()) && d.session.auto_labels.iter().any(|l| l.starts_with("worker:")));

    // What applies, and why not.
    let cfg: Value = http.get(format!("{api}/sessions/{}/config", s.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["agents"][0]["skipped"], Value::Null);
    assert_eq!(cfg["agents"][1]["skipped"], "needs python");
    assert_eq!(cfg["commands"][0]["skipped"], "replaced by a more specific one");
    assert_eq!(cfg["commands"][1]["skipped"], Value::Null);

    // Commands follow label changes at once.
    let r = http.put(format!("{api}/sessions/{}/labels", s.id)).json(&json!({"labels": []})).send().await.unwrap();
    assert!(r.status().is_success());
    mock.push(Reply::Text("hello".into()));
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "/greet Bob"})).send().await.unwrap();
    eventually("second request", async || (mock.requests().len() == 2).then_some(())).await;
    let text = mock.requests()[1]["messages"].to_string();
    assert!(text.contains("Hello generic Bob"), "{text}");

    // Search narrowed by labels: automatic ones (prefixes too) and yours.
    let found = async |labels: &str| -> usize {
        let r: Value =
            http.get(format!("{api}/search")).query(&[("q", "greet"), ("labels", labels)]).send().await.unwrap().json().await.unwrap();
        r["results"].as_array().unwrap().len()
    };
    assert_eq!(found("").await, 1);
    assert_eq!(found("repo:github.com/acme").await, 1, "an org-wide repo prefix");
    assert_eq!(found("python").await, 0);
    assert_eq!(found("client:acme").await, 0, "the label was removed above");

    stop.cancel();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_grouping_is_kept() {
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
    let url = format!("http://127.0.0.1:{}/api/v1/settings/session_grouping", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let get = async || http.get(&url).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(get().await, json!({"levels": []}), "a flat list at first");
    let r = http.put(&url).json(&json!({"levels": ["repo:", " client"]})).send().await.unwrap();
    assert!(r.status().is_success());
    assert_eq!(get().await, json!({"levels": ["repo", "client"]}), "keys, outermost first");
    for bad in [json!(["repo", "repo"]), json!([""]), json!(["a b"])] {
        let r = http.put(&url).json(&json!({ "levels": bad })).send().await.unwrap();
        assert_eq!(r.status(), 400, "{bad}");
    }
    assert_eq!(get().await, json!({"levels": ["repo", "client"]}));
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn sessions_get_colours_you_can_change() {
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
    let start = async || -> Session {
        http.post(format!("{api}/sessions"))
            .json(&json!({"worker_id": wid, "cwd": project.path().display().to_string(), "connection_id": conn.id, "model": "m"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    // Each new session takes the colour its realm's recent ones use least.
    let (a, b, c) = (start().await, start().await, start().await);
    assert_eq!([a.colour.as_str(), b.colour.as_str(), c.colour.as_str()], ["sky", "emerald", "amber"]);

    let set = async |id: &str, colour: &str| {
        http.put(format!("{api}/sessions/{id}/colour")).json(&json!({ "colour": colour })).send().await.unwrap()
    };
    let r = set(&b.id, "rose").await;
    assert!(r.status().is_success());
    assert_eq!(r.json::<Session>().await.unwrap().colour, "rose");
    let listed: Vec<Session> = http.get(format!("{api}/sessions")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed.iter().find(|s| s.id == b.id).unwrap().colour, "rose", "the list shows it");
    // Emerald is free again: the next session takes it.
    assert_eq!(start().await.colour, "emerald");
    assert_eq!(set(&a.id, "beige").await.status(), 400, "only the palette's colours");
    assert_eq!(set(&a.id, "sky/plaid").await.status(), 400, "and its patterns");
    assert_eq!(set("no-such-session", "teal").await.status(), 404);
    // A colour can come in a pattern.
    assert_eq!(set(&c.id, "navy/dots").await.json::<Session>().await.unwrap().colour, "navy/dots");

    // Its worker and its work have marks too, the same everywhere.
    let marks = async || http.get(format!("{api}/marks")).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(marks().await[format!("worker:{wid}")], "sky", "a worker gets the first mark no other worker has");
    let r = http.put(format!("{api}/settings/work")).json(&json!({"guess": false, "notes": false})).send().await.unwrap();
    assert!(r.status().is_success());
    let work: Value = http
        .post(format!("{api}/work"))
        .json(&json!({"title": "Ship marks", "attach": [{"ref": format!("session:{}", a.id)}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let work_id = work["id"].as_str().unwrap().to_owned();
    let other: Value =
        http.post(format!("{api}/work")).json(&json!({"title": "Something else"})).send().await.unwrap().json().await.unwrap();
    let m = marks().await;
    assert_eq!((&m[format!("work:{work_id}")], &m[format!("work:{}", other["id"].as_str().unwrap())]), (&json!("sky"), &json!("emerald")));
    // A project brings the work in: the session's tag names it, and it has a mark of its own.
    let project: Value = http
        .post(format!("{api}/projects"))
        .json(&json!({"title": "Visuals", "work": [work_id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_owned();
    assert_eq!(marks().await[format!("project:{project_id}")], "sky");
    let listed: Vec<Session> = http.get(format!("{api}/sessions")).send().await.unwrap().json().await.unwrap();
    let tag = listed.iter().find(|s| s.id == a.id).unwrap().work.clone().expect("the list says which work it serves");
    assert_eq!((tag.id.as_str(), tag.title.as_str()), (work_id.as_str(), "Ship marks"));
    assert_eq!((tag.project.as_str(), tag.project_title.as_str()), (project_id.as_str(), "Visuals"));
    assert_eq!(listed.iter().find(|s| s.id == b.id).unwrap().work, None);

    let mark =
        async |key: &str, mark: &str| http.put(format!("{api}/marks")).json(&json!({ "key": key, "mark": mark })).send().await.unwrap();
    assert!(mark(&format!("worker:{wid}"), "olive/vstripes").await.status().is_success());
    assert!(mark("repo:carbon6", "maroon").await.status().is_success());
    let m = marks().await;
    assert_eq!((&m[format!("worker:{wid}")], &m["repo:carbon6"]), (&json!("olive/vstripes"), &json!("maroon")));
    assert!(mark("repo:carbon6", "").await.status().is_success());
    assert_eq!(marks().await.get("repo:carbon6"), None, "taken away: back to the one worked out from its name");
    assert_eq!(mark("repo:carbon6", "beige").await.status(), 400);
    assert_eq!(mark("colour:red", "red").await.status(), 400, "only aspects' values have marks");
    assert_eq!(mark("work:no-such-work", "red").await.status(), 404);
    assert_eq!(mark("project:no-such-project", "red").await.status(), 404);
    assert!(mark(&format!("project:{project_id}"), "teal/dots").await.status().is_success());
    assert_eq!(marks().await[format!("project:{project_id}")], "teal/dots");

    stop.cancel();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn which_aspects_a_session_header_shows_is_kept() {
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
    let url = format!("http://127.0.0.1:{}/api/v1/settings/session_facets", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let get = async || http.get(&url).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(get().await, json!({"shown": ["worker", "work", "project", "realm"]}), "the default");
    let r = http.put(&url).json(&json!({"shown": ["model", "worker", "model"]})).send().await.unwrap();
    assert!(r.status().is_success());
    assert_eq!(get().await, json!({"shown": ["worker", "model"]}), "in the aspects' order, each once");
    let r = http.put(&url).json(&json!({"shown": ["colour"]})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    head.shutdown().await.unwrap();
}
