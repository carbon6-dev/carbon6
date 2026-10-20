//! Named agents (from the head's settings): Task's subagent_type runs one with its
//! own instructions and only the tools it lists.

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn named_agents_run_with_their_prompt_and_tools() {
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

    let r = http
        .put(format!("{api}/settings/agents"))
        .json(&json!([
            {"name": "finder", "description": "Finds things in the code", "tools": "Read, Grep", "prompt": "You are FINDER. Search, never edit."},
            {"name": "sleepy", "description": "Disabled", "prompt": "zzz", "enabled": false}
        ]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let wid = worker.identity.meta.worker_id.0;

    mock.push(Reply::ToolCalls(vec![(
        "task".into(),
        r#"{"description":"find main","prompt":"Where is main?","subagent_type":"finder"}"#.into(),
    )]));
    mock.push(Reply::Text("main is in src/main.rs".into())); // the agent
    mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"x","prompt":"y","subagent_type":"nobody"}"#.into())]));
    mock.push(Reply::Text("done".into()));
    let _: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(), "connection_id": conn.id, "model": "m", "prompt": "find main"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("all requests", async || (mock.requests().len() == 4).then_some(())).await;
    let reqs = mock.requests();
    // The parent's Task tool offers the agent.
    let task = reqs[0]["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "task").unwrap().clone();
    let offered = task["function"]["description"].as_str().unwrap();
    assert!(offered.contains("- finder: Finds things in the code") && !offered.contains("sleepy"), "{offered}");
    // The agent: its own system prompt, and only the tools it lists.
    let child = &reqs[1];
    let system = child["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are FINDER.") && !system.contains("autonomous software engineering agent"), "{system}");
    let mut tools: Vec<&str> = child["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    tools.sort();
    assert_eq!(tools, ["grep", "read"]);
    // Its report reaches the parent; an unknown agent is an error result.
    let results = reqs[2]["messages"].to_string();
    assert!(results.contains("main is in src/main.rs"), "{results}");
    let last = reqs[3]["messages"].to_string();
    assert!(last.contains("no agent named \\\"nobody\\\""), "{last}");

    stop.cancel();
    head.shutdown().await.unwrap();
}
