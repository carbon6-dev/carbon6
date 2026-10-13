//! Labels: settings scoped by "applies to" reach only the sessions whose
//! labels (yours, and the automatic repo:/folder:/worker:/model:) match;
//! command variants follow label changes; the config endpoint explains.

use std::time::Duration;

use c6_api::{Connection, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

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
    let p = project.path();

    // Not about follow-through: no setup side chat taking the model's replies.
    let r = http.put(format!("{api}/settings/playbooks")).json(&json!([])).send().await.unwrap();
    assert!(r.status().is_success());
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
