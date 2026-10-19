//! Importing sessions from Claude Code and Codex (docs/imports.md): a
//! worker you turn imports on for reads its session files; they become
//! sessions in the realm their folder maps to, grow as the files do, and
//! once continued in carbon6 are carbon6's.

use std::time::Duration;

use c6_api::Connection;
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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn lines(v: &[Value]) -> String {
    v.iter().map(|l| format!("{l}\n")).collect()
}

#[tokio::test]
async fn sessions_come_in_from_claude_code_and_codex() {
    let home = tempfile::tempdir().unwrap();
    let cc = home.path().join(".claude/projects/-w-app");
    std::fs::create_dir_all(&cc).unwrap();
    let cc_file = cc.join("0b1e5c6a-1111-4222-8333-944455556666.jsonl");
    std::fs::write(
        &cc_file,
        lines(&[
            json!({"type": "user", "sessionId": "0b1e5c6a-1111-4222-8333-944455556666", "cwd": "/w/app", "gitBranch": "main",
                   "timestamp": "2026-10-01T10:00:00Z", "message": {"role": "user", "content": "fix the flaky build"}}),
            json!({"type": "assistant", "timestamp": "2026-10-01T10:00:05Z", "message": {"id": "m1", "model": "claude-opus-5",
                   "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "make test"}}],
                   "usage": {"input_tokens": 10, "output_tokens": 5}}}),
            json!({"type": "user", "timestamp": "2026-10-01T10:00:09Z", "message": {"role": "user",
                   "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "1 failed"}]}}),
            json!({"type": "assistant", "timestamp": "2026-10-01T10:00:12Z", "message": {"id": "m2", "content": [{"type": "text", "text": "A race in the cache."}]}}),
        ]),
    )
    .unwrap();
    let cx = home.path().join(".codex/sessions/2026/10/01");
    std::fs::create_dir_all(&cx).unwrap();
    std::fs::write(
        cx.join("rollout-1.jsonl"),
        lines(&[
            json!({"type": "session_meta", "timestamp": "2026-10-02T09:00:00Z", "payload": {"id": "7f3a9c10-aaaa-4bbb-8ccc-1dddeeefff00", "cwd": "/home/me/notes"}}),
            json!({"type": "turn_context", "payload": {"model": "gpt-6-codex"}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "summarize the notes"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Three themes."}]}}),
            json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
        ]),
    )
    .unwrap();
    // SAFETY: before the worker starts; only workers that import read them.
    unsafe {
        std::env::set_var("C6_IMPORT_HOME", home.path());
        std::env::set_var("C6_IMPORT_EVERY_MS", "300");
        std::env::set_var("C6_IMPORT_SETTLE_MS", "0");
    }

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
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    ok(send(M::POST, "realms", json!({"id": "work"})).await).await;
    // Rules are checked: the realm exists.
    assert_eq!(send(M::PUT, "settings/session_imports", json!({"rules": [{"folder": "/w", "realm": "nope"}]})).await.status(), 400);
    let e = c6_head::api::new_enrollment(&head.state, Some("laptop")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();

    // Nothing comes in until you turn it on for the worker.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(ok(send(M::GET, "imports", json!({})).await).await, json!([]));
    ok(send(M::PUT, "settings/session_imports", json!({"workers": ["laptop"], "rules": [{"folder": "/w/**", "realm": "work"}]})).await)
        .await;

    let cc_id = "0b1e5c6a-1111-4222-8333-944455556666";
    let session = |id: &str| {
        let (http, url) = (http.clone(), format!("{api}/sessions/{id}"));
        async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap()["session"].clone() }
    };
    let s = eventually("the Claude Code session", async || {
        let s = session(cc_id).await;
        (s["id"] == cc_id).then_some(s)
    })
    .await;
    assert_eq!((s["realm"].as_str(), s["cwd"].as_str(), s["model"].as_str()), (Some("work"), Some("/w/app"), Some("claude-opus-5")), "{s}");
    assert!(s["labels"].to_string().contains("imported:claude-code"), "{s}");
    let codex = eventually("the Codex session", async || {
        let s = session("7f3a9c10-aaaa-4bbb-8ccc-1dddeeefff00").await;
        (s["realm"] == "home").then_some(s)
    })
    .await;
    assert_eq!(codex["title"], "summarize the notes");
    let events = |id: &str| {
        let (http, url) = (http.clone(), format!("{api}/sessions/{id}/events"));
        async move { http.get(url).send().await.unwrap().json::<Vec<Value>>().await.unwrap() }
    };
    let first = events(cc_id).await;
    assert!(serde_json::to_string(&first).unwrap().contains("A race in the cache."));
    assert!(first.iter().any(|e| e["type"] == "tool_result" && e["tool"] == "Bash"), "{first:?}");
    let summary = ok(send(M::GET, "imports", json!({})).await).await;
    assert_eq!(summary.as_array().unwrap().len(), 2, "{summary}");

    // The file grows (you go on in Claude Code): the session follows.
    let mut f = std::fs::OpenOptions::new().append(true).open(&cc_file).unwrap();
    use std::io::Write;
    f.write_all(
        lines(&[
            json!({"type": "user", "timestamp": "2026-10-01T11:00:00Z", "message": {"role": "user", "content": "and the other test?"}}),
        ])
        .as_bytes(),
    )
    .unwrap();
    drop(f);
    eventually("the new message", async || {
        let ev = events(cc_id).await;
        serde_json::to_string(&ev).unwrap().contains("and the other test?").then_some(())
    })
    .await;

    // Continued in carbon6 with a model you pick: the agent has the history.
    let conn: Connection =
        serde_json::from_value(ok(send(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).await).unwrap();
    mock.push(Reply::Text("Both fixed.".into()));
    ok(send(M::POST, &format!("sessions/{cc_id}/messages"), json!({"text": "fix them", "connection_id": conn.id, "model": "m"})).await)
        .await;
    eventually("the continued turn", async || (!mock.requests().is_empty()).then_some(())).await;
    let asked = mock.requests()[0]["messages"].to_string();
    assert!(asked.contains("fix the flaky build") && asked.contains("1 failed") && asked.contains("fix them"), "{asked}");
    // The whole turn: its reply, then its usage and its end.
    let ev = eventually("the reply", async || {
        let ev = events(cc_id).await;
        let done = serde_json::to_string(&ev).unwrap().contains("Both fixed.") && ev.last().is_some_and(|e| e["type"] == "turn_ended");
        done.then_some(ev)
    })
    .await;
    // Carbon6's now: the file growing again changes nothing.
    let mut f = std::fs::OpenOptions::new().append(true).open(&cc_file).unwrap();
    f.write_all(lines(&[json!({"type": "user", "message": {"role": "user", "content": "from claude code again"}})]).as_bytes()).unwrap();
    drop(f);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = events(cc_id).await;
    assert_eq!(after.len(), ev.len());
    assert!(!serde_json::to_string(&after).unwrap().contains("from claude code again"));

    stop.cancel();
    head.shutdown().await.unwrap();
}
