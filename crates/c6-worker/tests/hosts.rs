//! The host inventory: sessions on a host by name get its target and
//! labels; templates run on every host with given labels; checks go
//! through the worker (here a stand-in ssh; hosts named down* refuse).

use std::sync::{Arc, Mutex};

use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn fake_ssh(dir: &std::path::Path) -> std::path::PathBuf {
    let script = "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -o|-p|-i|-l|-F) shift 2 ;; -*) shift ;; *) break ;; esac; done\ncase \"$1\" in *down*) echo \"ssh: connect to host $1 port 22: Connection refused\" >&2; exit 255 ;; esac\nshift\nexport HOME=\"$C6_FAKE_HOME\"; cd \"$HOME\"\nexec sh -c \"$*\"\n";
    let path = dir.join("ssh");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A stand-in for Slack and webhooks: records (path, body).
async fn receiver() -> (String, Arc<Mutex<Vec<(String, serde_json::Value)>>>) {
    let got: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let (g, root) = (got.clone(), got.clone());
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
                let root = root.clone();
                async move {
                    root.lock().unwrap().push(("ntfy".into(), v));
                    "ok"
                }
            }),
        )
        .route(
            "/{name}",
            axum::routing::post(
                move |axum::extract::Path(name): axum::extract::Path<String>, axum::Json(v): axum::Json<serde_json::Value>| {
                    let g = g.clone();
                    async move {
                        g.lock().unwrap().push((name, v));
                        "ok"
                    }
                },
            ),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

#[tokio::test]
async fn hosts_by_name_and_label() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (bin, remote) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let home_dir = remote.path().canonicalize().unwrap();
    // SAFETY: the only test in this binary.
    unsafe {
        std::env::set_var("C6_SSH_BIN", fake_ssh(bin.path()));
        std::env::set_var("C6_FAKE_HOME", &home_dir);
    }
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

    let (hooks, got) = receiver().await;
    let put = |path: &str, v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put("hosts", json!([{"name": "a b", "target": "x"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put("hosts", json!([{"name": "a", "target": "-oProxyCommand=x"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put("hosts", json!([{"name": "a", "target": "x"}, {"name": "a", "target": "y"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(put("hosts", json!([{"name": "a", "target": "x", "labels": ["host:a"]}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert!(
        put(
            "hosts",
            json!([
                {"name": "web1", "target": "deploy@web1", "labels": ["role:web", "env:prod"], "check_every": "15"},
                {"name": "web2", "target": "deploy@web2", "labels": ["role:web", "env:prod"]},
                {"name": "db1", "target": "admin@db1", "labels": ["role:db", "env:prod"]},
                {"name": "down1", "target": "x@down1", "labels": ["role:web", "env:staging"]},
                {"name": "old", "target": "old", "enabled": false}
            ])
        )
        .await
        .is_success()
    );
    assert!(
        put("channels", json!([{"name": "hook", "kind": "webhook", "url": format!("{hooks}/hook"), "events": ["host"]}]))
            .await
            .is_success()
    );

    // Checks, through the worker.
    let check = |name: &str| {
        let (http, url) = (http.clone(), format!("{api}/hosts/{name}/check"));
        async move { http.post(url).send().await.unwrap().json::<c6_api::HostCheck>().await.unwrap() }
    };
    let up = check("web1").await;
    assert!(up.ok && !up.message.is_empty(), "{up:?}");
    let down = check("down1").await;
    assert!(!down.ok && down.message.contains("Connection refused"), "{down:?}");
    let note = eventually("a host notification", async || got.lock().unwrap().first().map(|(_, v)| v.clone())).await;
    assert_eq!(note["title"], "down1 is unreachable");
    assert!(note["labels"].as_array().unwrap().contains(&json!("env:staging")));
    let list: Vec<c6_api::HostStatus> = http.get(format!("{api}/hosts")).send().await.unwrap().json().await.unwrap();
    assert_eq!(list.len(), 5);
    assert!(list.iter().find(|s| s.host.name == "web1").unwrap().check.as_ref().unwrap().ok);
    assert!(list.iter().find(|s| s.host.name == "db1").unwrap().check.is_none());

    // A session by host name: its target and labels.
    let wid = worker.identity.meta.worker_id.0;
    for _ in 0..8 {
        mock.push(Reply::Text("ok".into()));
    }
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "ssh": "web1", "labels": ["mine"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s.ssh.as_deref(), Some("deploy@web1"));
    for l in ["mine", "role:web", "env:prod"] {
        assert!(s.labels.iter().any(|x| x == l), "{l} in {:?}", s.labels);
    }
    assert!(s.auto_labels.iter().any(|x| x == "host:web1"), "{:?}", s.auto_labels);

    // Templates by label: web1 and web2 (not down1, on staging).
    assert!(put("templates", json!([
        {"name": "uptime", "hosts": ["role:web", "env:prod"], "prompt": "Check uptime on {{target}}", "connection": "m", "model": "m"},
        {"name": "all", "hosts": ["*"], "prompt": "hi", "connection": "m", "model": "m"},
        {"name": "none", "hosts": ["role:mainframe"], "prompt": "hi", "connection": "m", "model": "m"}
    ]))
    .await
    .is_success());
    let run = |name: &str| {
        let (http, url) = (http.clone(), format!("{api}/templates/{name}/run"));
        async move { http.post(url).json(&json!({})).send().await.unwrap() }
    };
    let r: c6_api::TemplateRun = run("uptime").await.json().await.unwrap();
    let mut titles: Vec<String> = r.sessions.iter().map(|s| s.title.clone()).collect();
    titles.sort();
    assert_eq!(titles, ["uptime · web1", "uptime · web2"]);
    assert!(r.sessions.iter().all(|s| s.labels.iter().any(|l| l == "env:prod") && s.labels.iter().any(|l| l == "template:uptime")));
    let r: c6_api::TemplateRun = run("all").await.json().await.unwrap();
    assert_eq!(r.sessions.len(), 4, "every enabled host");
    assert_eq!(run("none").await.status(), reqwest::StatusCode::BAD_REQUEST);

    stop.cancel();
    head.shutdown().await.unwrap();
}
