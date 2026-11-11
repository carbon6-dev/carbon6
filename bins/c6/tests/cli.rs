//! The `c6` CLI against a real head (sign-in on, so through an API token)
//! and worker, with a mock model.

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
        // CI sets this independently of HOME; don't share another test's login.
        .env_remove("XDG_CONFIG_HOME")
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
async fn list_settings_target_the_requested_realm() {
    let (data, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
        data_dir: data.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: true,
        ..Default::default()
    })
    .await
    .unwrap();
    c6_head::realms::create(&head.state, &c6_api::RealmIn { id: "c6".into(), ..Default::default() }, "test").await.unwrap();
    let url = format!("http://127.0.0.1:{}", head.ui_addrs[0].port());
    let token = head.state.store.create_api_token("test", None).await.unwrap().token.unwrap();
    let http = reqwest::Client::new();
    let read = async |list: &str, realm: &str| -> Value {
        let r = http.get(format!("{url}/api/v1/settings/{list}")).query(&[("realm", realm)]).bearer_auth(&token).send().await.unwrap();
        assert!(r.status().is_success());
        r.json().await.unwrap()
    };
    let login = c6(home.path(), &["login", &url, "--token", &token]).await;
    assert_eq!(login.code, 0, "{}", login.stderr);
    assert!(login.stderr.contains(&format!("saved to {}", home.path().display())), "{}", login.stderr);

    for (list, default_realm, entry) in [
        ("templates", "home", json!({"name": "unrelated", "prompt": "Keep me"})),
        ("policies", "root", json!({"name": "unrelated", "action": "ask"})),
    ] {
        let get = format!("settings.{list}.get");
        let put = format!("settings.{list}.put");
        for op in [&get, &put] {
            let out = c6(home.path(), &["ops", op]).await;
            assert_eq!(out.code, 0, "{}", out.stderr);
            let description: Value = serde_json::from_str(&out.stdout).unwrap();
            assert!(description["input"]["properties"]["realm"].is_object(), "{description}");
        }
        // No realm still means the endpoint's historical default.
        let seed = format!("body={}", json!([entry]));
        let out = c6(home.path(), &["op", &put, &seed]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        let (before_home, before_root) = (read(list, "home").await, read(list, "root").await);
        let out = c6(home.path(), &["op", &get]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        let default: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(default, if default_realm == "home" { before_home.clone() } else { before_root.clone() });
        assert!(default.as_array().unwrap().iter().any(|v| v["name"] == "unrelated"));

        // Establish an unrelated c6 entry, read it through the catalog,
        // and keep it when replacing the list from a file.
        let out = c6(home.path(), &["op", &put, "realm=c6", &seed]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        let out = c6(home.path(), &["op", &get, "realm=c6"]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        let mut wanted: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(wanted, read(list, "c6").await);
        let added = if list == "templates" {
            json!([{"name": "implement", "prompt": "Implement"}, {"name": "design", "prompt": "Design"}, {"name": "coordinator", "prompt": "Coordinate"}])
        } else {
            json!([{"name": "Implementers don't push", "action": "deny", "tools": "bash", "pattern": "git push"}])
        };
        wanted.as_array_mut().unwrap().extend(added.as_array().unwrap().iter().cloned());
        let expected = if list == "templates" {
            serde_json::to_value(serde_json::from_value::<Vec<c6_api::Template>>(wanted.clone()).unwrap()).unwrap()
        } else {
            serde_json::to_value(serde_json::from_value::<Vec<c6_api::Policy>>(wanted.clone()).unwrap()).unwrap()
        };
        let file = home.path().join(format!("{list}.json"));
        std::fs::write(&file, wanted.to_string()).unwrap();
        let body = format!("body=@{}", file.display());
        let out = c6(home.path(), &["op", &put, "realm=c6", &body]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        let out = c6(home.path(), &["op", &get, r#"{"realm":"c6"}"#]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        let actual: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(actual, read(list, "c6").await);
        assert_eq!(actual, expected);
        for op in [&get, &put] {
            let out = c6(home.path(), &["op", op, "realm=nowhere", &body]).await;
            assert_ne!(out.code, 0);
            assert!(out.stderr.contains("no realm"), "{}", out.stderr);
        }
        assert_eq!(read(list, "c6").await, actual);
        assert_eq!(read(list, "home").await, before_home);
        assert_eq!(read(list, "root").await, before_root);

        // Explicit targeting does not grant a scoped token new powers:
        // direct settings requests stay in c6; catalog calls remain admin-only.
        let scoped = head.state.store.create_api_token("scoped", Some("c6")).await.unwrap().token.unwrap();
        for realm in ["c6", "home", "root"] {
            for method in [reqwest::Method::GET, reqwest::Method::PUT] {
                let r = http
                    .request(method, format!("{url}/api/v1/settings/{list}"))
                    .query(&[("realm", realm)])
                    .bearer_auth(&scoped)
                    .json(&actual)
                    .send()
                    .await
                    .unwrap();
                if realm == "c6" {
                    assert!(r.status().is_success(), "{}", r.status());
                } else {
                    assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN);
                }
            }
            for op in [&get, &put] {
                let r = http
                    .post(format!("{url}/api/v1/catalog/call"))
                    .bearer_auth(&scoped)
                    .json(&json!({"op": op, "input": {"realm": realm, "body": actual}}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN);
            }
        }
        let scoped_home = tempfile::tempdir().unwrap();
        let out = c6(scoped_home.path(), &["login", &url, "--token", &scoped]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        for op in [&get, &put] {
            let out = c6(scoped_home.path(), &["op", op, "realm=home", &body]).await;
            assert_ne!(out.code, 0);
        }
        assert_eq!(read(list, "c6").await, actual);
        assert_eq!(read(list, "home").await, before_home);
        assert_eq!(read(list, "root").await, before_root);
    }
    head.shutdown().await.unwrap();
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
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
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
