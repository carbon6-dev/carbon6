//! Follow-through (docs/follow-through.md): a project kit reacts to a push
//! (checks for a PR on the session's worker, offers "open a PR", tells the
//! agent, starts a CI tracker probed on a named worker), the tracker follows
//! the run's phases (every change kept), the agent reads it and offers next
//! steps, notes reach the model with your next message, and a failure
//! starts an investigation with the logs.

use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Followups, Session, SessionDetail, SessionStatus};
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
async fn push_tracks_ci_and_investigates_failures() {
    let mock = MockLlm::start().await;
    let (head_dir, state, ci) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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

    // The kit: nothing here is built into carbon6.
    let ci_dir = ci.path().display().to_string();
    let kit = json!([{
        "name": "app",
        "applies_to": ["team:core"],
        "reactions": [{
            "name": "after push",
            "when": {"tool": "bash", "input": "git push|echo pushed", "output": "pushed (?P<sha>[0-9a-f]+) to (?P<branch>\\S+)"},
            "check": {"command": format!("cat {ci_dir}/pr.json 2>/dev/null || echo '{{}}'")},
            "then": [
                {"kind": "suggest", "text": "Open a PR for {{vars.branch}}", "if": [{"var": "check.json.number", "exists": false}]},
                {"kind": "track", "tracker": "ci"},
                {"kind": "tell", "text": "CI is being followed for {{vars.sha}} on {{vars.branch}}."}
            ]
        }],
        "trackers": [{
            "name": "ci",
            "title": "CI for {{vars.sha}}",
            "probe": {"command": format!("cat {ci_dir}/run-{{{{vars.sha}}}}.json 2>/dev/null || echo '{{}}'"), "on": "worker", "worker": "build-1"},
            "phases": [
                {"phase": "succeeded", "if": [{"var": "probe.json.status", "equals": "Succeeded"}]},
                {"phase": "failed", "if": [{"var": "probe.json.status", "equals": "Failed"}]},
                {"phase": "running", "if": [{"var": "probe.json.status", "equals": "Running"}]},
                {"phase": "waiting"}
            ],
            "line": "{{probe.json.status}} {{probe.json.done}}/{{probe.json.total}}",
            "url": "https://argo.internal/workflows/ci/{{probe.json.name}}",
            "interval_s": 1,
            "logs": {"command": "echo 'test_parse failed: expected 3, got 4'", "on": "worker", "worker": "build-1"}
        }]
    }]);
    let bad = http
        .put(format!("{api}/settings/kits"))
        .json(&json!([{"name": "x", "reactions": [{"name": "r", "when": {"tool": "("}}]}]))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let put = http.put(format!("{api}/settings/kits")).json(&kit).send().await.unwrap();
    assert!(put.status().is_success(), "{}", put.text().await.unwrap());

    // A dry run, before anything happens: the probe works, nothing to see yet.
    let dry: Value = http
        .post(format!("{api}/kits/dry-run"))
        .json(&json!({"name": "app", "worker": "build-1", "vars": {"sha": "abc123", "branch": "feature-x"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((dry["trackers"][0]["phase"].as_str(), dry["trackers"][0]["line"].as_str()), (Some("waiting"), Some("waiting")), "{dry}");
    assert_eq!(dry["ok"], true, "{dry}");
    assert_eq!(dry["reactions"][0]["would"][0], "suggest Open a PR for feature-x", "{dry}");

    // The agent pushes (and offers a next step of its own).
    mock.push(Reply::ToolCalls(vec![
        ("bash".into(), r#"{"command":"echo pushed abc123 to feature-x"}"#.into()),
        ("suggest".into(), r#"{"text":"Run the e2e tests"}"#.into()),
    ]));
    mock.push(Reply::Text("Pushed.".into()));
    let wid = worker.identity.meta.worker_id.0;
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "push my work", "labels": ["team:core"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let followups = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/sessions/{id}/followups")).send().await.unwrap().json::<Followups>().await.unwrap() }
    };
    let f = eventually("a next step and a tracker", async || {
        let f = followups(s.id.clone()).await;
        (f.suggestions.len() == 3 && !f.trackers.is_empty()).then_some(f)
    })
    .await;
    let texts: Vec<(&str, &str)> = f.suggestions.iter().map(|x| (x.kind.as_str(), x.text.as_str())).collect();
    assert!(texts.contains(&("next", "Open a PR for feature-x")), "{texts:?}");
    assert!(texts.contains(&("next", "Run the e2e tests")), "{texts:?}");
    assert!(texts.contains(&("note", "CI is being followed for abc123 on feature-x.")), "{texts:?}");
    let t = &f.trackers[0];
    assert_eq!((t.title.as_str(), t.phase.as_str()), ("CI for abc123", "waiting"));
    let events: Vec<Event> = http.get(format!("{api}/sessions/{}/events?after=0", s.id)).send().await.unwrap().json().await.unwrap();
    let push_seq = events.iter().find(|e| matches!(&e.kind, EventKind::ToolResult { tool, .. } if tool == "bash")).unwrap().seq;
    assert_eq!(t.anchor_seq, push_seq, "anchored at the push");

    // CI picks it up.
    std::fs::write(ci.path().join("run-abc123.json"), r#"{"status":"Running","done":2,"total":5,"name":"ci-7f2"}"#).unwrap();
    let t = eventually("the run", async || {
        let f = followups(s.id.clone()).await;
        f.trackers.into_iter().find(|t| t.phase == "running")
    })
    .await;
    assert_eq!(t.line, "Running 2/5");
    assert_eq!(t.url, "https://argo.internal/workflows/ci/ci-7f2");

    // You take the PR step: it is your message, with the note for the model;
    // the agent checks on CI.
    eventually("the first turn's end", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle).then_some(())
    })
    .await;
    mock.push(Reply::ToolCalls(vec![("followups".into(), "{}".into())]));
    mock.push(Reply::Text("CI is running; opening the PR.".into()));
    let pr = f.suggestions.iter().find(|x| x.text == "Open a PR for feature-x").unwrap();
    let r = http.post(format!("{api}/sessions/{}/suggestions/{}/take", s.id, pr.id)).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let asked = eventually("the agent's read of its trackers", async || {
        let reqs = mock.requests();
        (reqs.len() >= 4).then(|| reqs[3].to_string())
    })
    .await;
    assert!(asked.contains("Tracker CI for abc123 (app): running"), "{asked}");
    let sent = mock.requests()[2].to_string();
    assert!(sent.contains("Open a PR for feature-x") && sent.contains("carbon6-followups"), "{sent}");
    assert!(sent.contains("CI is being followed for abc123"), "the note reached the model: {sent}");
    assert!(sent.contains("Run the e2e tests"), "open next steps are listed: {sent}");
    let f = followups(s.id.clone()).await;
    assert!(f.suggestions.iter().any(|x| x.text == "Open a PR for feature-x" && x.state == "taken"));
    assert!(f.suggestions.iter().any(|x| x.kind == "note" && x.state == "delivered"));

    // CI fails: the tracker ends, and an investigation starts with the logs.
    eventually("the second turn's end", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle).then_some(())
    })
    .await;
    mock.push(Reply::Text("test_parse expects 3 but the parser now returns 4; fix on branch fix/feature-x-abc123.".into()));
    std::fs::write(ci.path().join("run-abc123.json"), r#"{"status":"Failed","done":3,"total":5,"name":"ci-7f2"}"#).unwrap();
    let t = eventually("the failure and its investigation", async || {
        let f = followups(s.id.clone()).await;
        f.trackers.into_iter().find(|t| t.phase == "failed" && t.investigation.is_some())
    })
    .await;
    assert!(t.ended_at.is_some());
    let side = t.investigation.clone().unwrap();
    let side_s: SessionDetail = http.get(format!("{api}/sessions/{side}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(side_s.session.parent_id.as_deref(), Some(s.id.as_str()));
    let investigation = eventually("the investigation's question", async || {
        mock.requests().into_iter().map(|r| r.to_string()).find(|r| r.contains("test_parse failed: expected 3, got 4"))
    })
    .await;
    assert!(investigation.contains("fix/<branch>-<short sha>") && investigation.contains("Do not push"), "{investigation}");
    // Every change it saw is kept.
    let full: Value = http.get(format!("{api}/trackers/{}", t.id)).send().await.unwrap().json().await.unwrap();
    let phases: Vec<&str> = full["observations"].as_array().unwrap().iter().map(|o| o["phase"].as_str().unwrap()).collect();
    assert_eq!(phases, vec!["waiting", "running", "failed"], "{full}");
    // The end is a note for the agent's next turn.
    let f = followups(s.id.clone()).await;
    assert!(
        f.suggestions.iter().any(|x| x.kind == "note" && x.text.starts_with("Tracker ended — CI for abc123 failed")),
        "{:?}",
        f.suggestions
    );

    // Kits only apply to their labels: a session without team:core is left alone.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo pushed def456 to other"}"#.into())]));
    mock.push(Reply::Text("Pushed.".into()));
    let other: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "push"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("the other session to finish", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", other.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let f = followups(other.id.clone()).await;
    assert!(f.suggestions.is_empty() && f.trackers.is_empty(), "{f:?}");
    stop.cancel();
}
