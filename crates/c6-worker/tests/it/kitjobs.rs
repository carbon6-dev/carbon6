//! Kit jobs (docs/knowledge.md §6.2): the head runs an app's kit tool on a
//! worker with a container runtime, with no session; what it writes to
//! out/ingest.json becomes the app's data; a kit that may reach only some
//! hosts reaches those and no others. Needs Docker or Podman and the
//! network (it builds images), so it runs only with C6_TEST_KITS=1.

use base64::Engine;
use c6_api::Install;
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const RUN: &str = r#"#!/bin/sh
set -e
case "$1" in
  emit)
    cat > /work/out/ingest.json <<EOF
{"records": [{"collection": "seen", "fields": {"name": "$2"}}],
 "things": [{"kind": "person", "name": "$2", "key": "person:$2"}],
 "claims": [{"from": "thing:person:$2", "label": "knows", "to": "thing:person:$2"}]}
EOF
    echo "emitted $2";;
  fetch)
    wget -q -T 15 -O /work/out/page.html "https://$2/";;
esac
"#;

#[tokio::test]
async fn kit_jobs_run_for_apps() {
    if std::env::var("C6_TEST_KITS").is_err() || c6_tools::kits::runtime().await.is_none() {
        eprintln!("set C6_TEST_KITS=1 (and have Docker or Podman) to run");
        return;
    }
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
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
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
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);

    // A kit that may reach only example.com, and an app that uses it.
    ok(send(
        M::POST,
        "packages",
        json!({"manifest": {"package": "toy", "version": "1.0.0",
            "permissions": {"network": "hosts", "hosts": ["example.com"], "timeout_s": 60},
            "kit": {"build": {"files": ["run.sh"]}, "tools": [
                {"name": "emit", "command": ["sh", "/run.sh", "emit", "{name}"], "args": {"name": "text"}},
                {"name": "fetch", "command": ["sh", "/run.sh", "fetch", "{host}"], "args": {"host": "text"}}]}},
            "files": {"Dockerfile": b64("FROM alpine:3.20\nCOPY run.sh /run.sh\n"), "run.sh": b64(RUN)}}),
    )
    .await)
    .await;
    ok(send(
        M::POST,
        "packages",
        json!({"manifest": {"package": "toyapp", "version": "1.0.0", "requires": {"toy": "^1"},
            "contents": {"collections": [{"name": "seen", "schema": {"type": "object", "properties": {"name": {"type": "string"}}}}]},
            "operations": [
                {"name": "emit", "input": {"type": "object", "properties": {"name": {"type": "string"}}}, "kit": {"kit": "toy", "tool": "emit", "args": {"name": "$.name"}}},
                {"name": "fetch", "input": {"type": "object", "properties": {"host": {"type": "string"}}}, "kit": {"kit": "toy", "tool": "fetch", "args": {"host": "$.host"}}}]},
            "files": {}}),
    )
    .await)
    .await;
    let plan = ok(send(M::POST, "installs/plan", json!({"package": "toyapp"})).await).await;
    assert!(plan["plan"].as_str().unwrap().contains("Reach only example.com"), "{plan}");
    let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "toyapp"})).await).await).unwrap();
    ok(send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await).await;

    // An operation's kit job: what it writes in out/ingest.json is the app's.
    let r = ok(send(M::POST, "apps/home/toyapp/operations/emit", json!({"input": {"name": "ada"}})).await).await;
    assert_eq!(r["ingested"]["records"], 1, "{r}");
    assert_eq!(r["ingested"]["claims"], 1, "{r}");
    let seen = ok(send(M::POST, "collections/seen/query", json!({})).await).await;
    assert_eq!(seen["records"][0]["fields"]["name"], "ada");
    assert!(seen["records"][0]["created_by"].as_str().unwrap().starts_with("app:toyapp kit:toy.emit"), "{seen}");
    let claims = ok(send(M::GET, "memex/claims?about=thing:person:ada", json!({})).await).await;
    assert_eq!(claims[0]["owner"], "app:toyapp", "{claims}");

    // It reaches the hosts it may, and no others.
    let r = ok(send(M::POST, "apps/home/toyapp/operations/fetch", json!({"input": {"host": "example.com"}})).await).await;
    assert!(r["files"].to_string().contains("page.html"), "{r}");
    let r = send(M::POST, "apps/home/toyapp/operations/fetch", json!({"input": {"host": "www.google.com"}})).await;
    assert_eq!(r.status(), 400, "{}", r.text().await.unwrap());

    // A trigger's kit action, with no agent.
    ok(send(M::POST, "collections", json!({"name": "inbox", "schema": {"type": "object", "properties": {"who": {"type": "string"}}}}))
        .await)
    .await;
    ok(send(
        M::POST,
        "triggers",
        json!({"name": "seen it", "trigger": "record", "event_params": {"collection": "inbox", "ops": ["created"]},
        "action": "kit", "action_params": {"instance": "toyapp", "kit": "toy", "tool": "emit", "args": {"name": "$.record.fields.who"}}}),
    )
    .await)
    .await;
    ok(send(M::POST, "collections/inbox/records", json!({"fields": {"who": "grace"}})).await).await;
    eventually("the trigger's kit job", async || {
        let v = ok(send(M::POST, "collections/seen/query", json!({"where": {"name": "grace"}})).await).await;
        (v["total"] == 1).then_some(())
    })
    .await;

    stop.cancel();
    head.shutdown().await.unwrap();
}
