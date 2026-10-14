//! The `c6` CLI against a real head (sign-in on, so through an API token)
//! and worker, with a mock model.

use std::time::Duration;

use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn c6(home: &std::path::Path, args: &[&str]) -> Out {
    let o = tokio::process::Command::new(env!("CARGO_BIN_EXE_c6"))
        .args(args)
        .env("HOME", home)
        .env_remove("C6_URL")
        .env_remove("C6_TOKEN")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

#[tokio::test]
async fn headless_cli() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project, home) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let url = format!("http://127.0.0.1:{}", head.ui_addrs[0].port());
    let token = head.state.store.create_api_token("test", None).await.unwrap().token.unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(20), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    let r = reqwest::Client::new()
        .post(format!("{url}/api/v1/connections"))
        .bearer_auth(&token)
        .json(&json!({"name": "mock", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let h = home.path();

    // A wrong token is refused at login; the right one is saved.
    let bad = c6(h, &["login", &url, "--token", "c6_nope"]).await;
    assert_eq!(bad.code, 2, "{}", bad.stderr);
    assert!(bad.stderr.contains("invalid API token"), "{}", bad.stderr);
    let ok = c6(h, &["login", &url, "--token", &token]).await;
    assert_eq!(ok.code, 0, "{}", ok.stderr);

    // run: starts a session and prints the reply.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo hi"}"#.into())]));
    mock.push(Reply::Text("All done here.".into()));
    let cwd = project.path().display().to_string();
    let run = c6(h, &["run", "-C", &cwd, "-m", "m", "--title", "cli test", "-l", "team:core,cli", "say", "hi"]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "All done here.");
    assert!(run.stderr.contains("→ bash"), "{}", run.stderr);
    let first = mock.requests()[0]["messages"].to_string();
    assert!(first.contains("say hi"), "{first}");
    let id = run.stderr.split_whitespace().nth(1).unwrap().to_owned();

    // send, by id prefix; the model defaults to the latest session's.
    mock.push(Reply::Text("Second answer.".into()));
    let send = c6(h, &["send", &id[..13], "and", "again"]).await;
    assert_eq!((send.code, send.stdout.trim()), (0, "Second answer."), "{}", send.stderr);

    // A failed turn exits 1; --json prints events.
    mock.push(Reply::Status(400));
    let fail = c6(h, &["send", &id, "--json", "break"]).await;
    assert_eq!(fail.code, 1, "{}", fail.stderr);
    let kinds: Vec<String> =
        fail.stdout.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()["type"].as_str().unwrap().to_owned()).collect();
    assert!(kinds.contains(&"error".to_owned()) && kinds.last().map(String::as_str) == Some("turn_ended"), "{kinds:?}");

    // A template, run by name with its {{input}}.
    let r = reqwest::Client::new()
        .put(format!("{url}/api/v1/settings/templates"))
        .bearer_auth(&token)
        .json(&json!([{"name": "greet", "model": "m", "cwd": cwd, "prompt": "Greet {{input}}."}]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    mock.push(Reply::Text("Hello, Ada!".into()));
    let t = c6(h, &["run", "-t", "greet", "Ada"]).await;
    assert_eq!((t.code, t.stdout.trim()), (0, "Hello, Ada!"), "{}", t.stderr);
    assert!(mock.requests().last().unwrap()["messages"].to_string().contains("Greet Ada."));

    // sessions, show, search.
    let list = c6(h, &["sessions"]).await;
    assert!(list.stdout.contains("cli test") && list.stdout.contains("[team:core, cli]"), "{}", list.stdout);
    let by_label = c6(h, &["sessions", "-l", "team:core"]).await;
    assert!(by_label.stdout.contains("cli test"), "{}", by_label.stdout);
    let none = c6(h, &["sessions", "-l", "team:other"]).await;
    assert!(none.stdout.trim().is_empty(), "{}", none.stdout);
    let show = c6(h, &["show", &id]).await;
    assert!(show.stdout.contains("## You\n\nsay hi") && show.stdout.contains("Second answer."), "{}", show.stdout);
    let found = c6(h, &["search", "Second answer"]).await;
    assert!(found.stdout.contains("cli test"), "{}", found.stdout);

    stop.cancel();
    head.shutdown().await.unwrap();
}
