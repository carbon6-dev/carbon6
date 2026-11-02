//! Plan limits per connection (docs/analytics.md, "Plan limits"): Claude
//! Code reports its subscription's windows as its sessions run; a ChatGPT
//! plan's are read from its usage endpoint with the head's own sign-in.
//! Passing 80% and 95%, and running out, are notified once per window.
//! A binary of its own: it sets C6_CLAUDE_BIN.

use std::path::Path;
use std::sync::{Arc, Mutex};

use c6_api::{ConnectionQuota, EventKind};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A `claude` that reports its subscription's limits with each turn: the
/// week at 50%; "HOT" puts it at 96% with a warning; "OUT" uses up the 5h
/// window.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
[ "$1" = --version ] && exit 0
uuid_of() { printf '%s' "$1" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p'; }
echo_of() { printf '{"type":"user","isReplay":true,"uuid":"%s","message":{"role":"user","content":"echo"},"parent_tool_use_id":null}\n' "$(uuid_of "$1")"; }
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"stop_reason":"end_turn"},"parent_tool_use_id":null}\n' "$1"; }
done_ok() { printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":1,"output_tokens":1}}'; }
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  printf '%s\n' '{"type":"system","subtype":"init"}'
  echo_of "$line"
  case "$line" in
    *HOT*) printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":4102444800,"rateLimitType":"seven_day","unifiedWindows":{"seven_day":{"utilization":0.96,"resetsAt":4102444800,"status":"allowed_warning"}}}}';;
    *OUT*) printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":4102441200,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":1.0,"resetsAt":4102441200,"status":"rejected"}}}}';;
    *) printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":4102444800,"rateLimitType":"seven_day","unifiedWindows":{"seven_day":{"utilization":0.5,"resetsAt":4102444800,"status":"allowed"}}}}';;
  esac
  say "Reply."; done_ok
done
"#;
    let path = dir.join("claude");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A stand-in for ChatGPT's usage endpoint (answering `body`, recording each
/// request's headers) and for a notification webhook.
async fn stand_in(body: Arc<Mutex<Value>>) -> (String, Arc<Mutex<Vec<axum::http::HeaderMap>>>, Seen) {
    let asked: Arc<Mutex<Vec<axum::http::HeaderMap>>> = Arc::default();
    let notes: Seen = Arc::default();
    let (a, n) = (asked.clone(), notes.clone());
    let app = axum::Router::new()
        .route(
            "/wham/usage",
            axum::routing::get(move |h: axum::http::HeaderMap| {
                let (a, body) = (a.clone(), body.clone());
                async move {
                    a.lock().unwrap().push(h);
                    axum::Json(body.lock().unwrap().clone())
                }
            }),
        )
        .route(
            "/hook",
            axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
                let n = n.clone();
                async move {
                    n.lock().unwrap().push(("hook".into(), v));
                    "ok"
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, asked, notes)
}

fn titles(notes: &Seen) -> Vec<String> {
    notes.lock().unwrap().iter().map(|(_, v)| v["title"].as_str().unwrap_or("").to_owned()).collect()
}

#[tokio::test]
async fn plan_limits_from_claude_code_and_chatgpt() {
    let bins = tempfile::tempdir().unwrap();
    let claude = fake_claude(bins.path());
    // SAFETY: this binary has one test; nothing else reads the variable.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
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
    let usage = Arc::new(Mutex::new(json!({"rate_limit":{"limit_reached":false,
        "primary_window":{"used_percent":34.0,"limit_window_seconds":18000,"reset_after_seconds":7800},
        "secondary_window":{"used_percent":71.0,"limit_window_seconds":604800,"resets_at":4102444800i64}}})));
    let (stand, asked, notes) = stand_in(usage.clone()).await;
    let put = async |path: &str, v: Value| {
        let r = http.put(format!("{api}/settings/{path}")).json(&v).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.status());
    };
    put("channels", json!([{"name": "hook", "kind": "webhook", "url": format!("{stand}/hook"), "events": ["quota"]}])).await;
    let quotas =
        async || -> Vec<ConnectionQuota> { http.get(format!("{api}/connections/quotas")).send().await.unwrap().json().await.unwrap() };
    assert!(quotas().await.is_empty(), "nothing seen yet");

    // A ChatGPT plan, read with the head's sign-in and its workspace.
    let tokens = json!({"client_id": "c", "access_token": "tok-1", "refresh_token": "r", "expires_at_ms": i64::MAX / 2,
        "scope": "openid", "workspace": "ws-9"});
    let row = c6_head::store::ConnectionRow {
        id: "gpt".into(),
        name: "ChatGPT (me)".into(),
        kind: "chatgpt".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: Some(head.state.secrets.encrypt(&tokens.to_string()).unwrap()),
        default_model: None,
        context_window: None,
        created_at: 0,
        realms: "[]".into(),
        members: "[]".into(),
        subagent_model: None,
        compact_at: None,
    };
    head.state.store.insert_connection(&row).await.unwrap();
    head.state.store.set_setting(c6_head::quotas::CHATGPT_USAGE_URL, &format!("{stand}/wham/usage")).await.unwrap();
    c6_head::quotas::poll_chatgpt(&head.state).await;
    let h = asked.lock().unwrap().last().cloned().unwrap();
    assert_eq!(h["authorization"], "Bearer tok-1");
    assert_eq!(h["chatgpt-account-id"], "ws-9");
    let q = quotas().await;
    let gpt = q.iter().find(|q| q.connection_id == "gpt").unwrap();
    assert_eq!(gpt.source, "chatgpt");
    assert_eq!(gpt.windows.iter().map(|w| (w.name.as_str(), w.used_percent)).collect::<Vec<_>>(), [("5h", 34.0), ("week", 71.0)]);
    assert_eq!(gpt.windows[1].resets_at, Some(4_102_444_800_000));
    assert!(titles(&notes).is_empty(), "under 80%: nothing to say");

    // Past 80%, then past 95%: said once each, not again on the next read.
    usage.lock().unwrap()["rate_limit"]["secondary_window"]["used_percent"] = json!(85.0);
    c6_head::quotas::poll_chatgpt(&head.state).await;
    c6_head::quotas::poll_chatgpt(&head.state).await;
    usage.lock().unwrap()["rate_limit"]["secondary_window"]["used_percent"] = json!(97.0);
    c6_head::quotas::poll_chatgpt(&head.state).await;
    let want = ["ChatGPT (me): 85% of its week limit used", "ChatGPT (me): 97% of its week limit used"];
    eventually("the notifications", async || (titles(&notes) == want).then_some(())).await;

    // A failed read keeps the last windows and says why.
    *usage.lock().unwrap() = json!({"detail": "Unauthorized"});
    c6_head::quotas::poll_chatgpt(&head.state).await;
    let gpt = quotas().await.into_iter().find(|q| q.connection_id == "gpt").unwrap();
    assert_eq!(gpt.windows.len(), 2);
    assert!(gpt.error.contains("named no limits"), "{}", gpt.error);

    // Claude Code: its sessions' turns bring its subscription's windows.
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let cc: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind": "claude_code", "name": "cc", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cc_id = cc["id"].as_str().unwrap().to_owned();
    let s: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(), "connection_id": cc_id, "model": "opus", "prompt": "hi"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sid = s["id"].as_str().unwrap().to_owned();
    let ended = async |turns: usize| {
        eventually("the turn to end", async || {
            let events: Vec<c6_api::Event> = http.get(format!("{api}/sessions/{sid}/events")).send().await.unwrap().json().await.unwrap();
            (events.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns).then_some(())
        })
        .await
    };
    let cc_quota = async || quotas().await.into_iter().find(|q| q.connection_id == cc_id);
    ended(1).await;
    let q = eventually("Claude Code's report", async || cc_quota().await).await;
    assert_eq!(q.source, "claude_code");
    assert_eq!(
        q.windows.iter().map(|w| (w.name.as_str(), w.used_percent, w.status.as_str())).collect::<Vec<_>>(),
        [("week", 50.0, "allowed")]
    );
    // A report about the 5h window keeps the week; used up, it says so.
    let send = async |text: &str| {
        let r = http.post(format!("{api}/sessions/{sid}/messages")).json(&json!({"text": text})).send().await.unwrap();
        assert!(r.status().is_success());
    };
    send("OUT").await;
    ended(2).await;
    let q = eventually("the 5h window", async || cc_quota().await.filter(|q| q.windows.len() == 2)).await;
    assert_eq!(
        q.windows.iter().map(|w| (w.name.as_str(), w.used_percent, w.status.as_str())).collect::<Vec<_>>(),
        [("5h", 100.0, "rejected"), ("week", 50.0, "allowed")]
    );
    assert_eq!(q.tightest().unwrap().name, "5h");
    // A realm's pages see them too (the connection serves every realm).
    let seen: Vec<ConnectionQuota> =
        http.get(format!("{api}/connections/quotas")).header("X-C6-Realm", "home").send().await.unwrap().json().await.unwrap();
    assert!(seen.iter().any(|q| q.connection_id == cc_id), "{seen:?}");
    send("HOT").await;
    ended(3).await;
    eventually("the week near its limit", async || cc_quota().await.filter(|q| q.windows[1].used_percent == 96.0)).await;
    eventually("Claude Code's notifications", async || {
        let t = titles(&notes);
        (t.contains(&"cc: its 5h limit is used up".to_owned()) && t.contains(&"cc: 96% of its week limit used".to_owned())).then_some(())
    })
    .await;
    // Nothing of it in the session's own stream or log.
    let events: Vec<c6_api::Event> = http.get(format!("{api}/sessions/{sid}/events")).send().await.unwrap().json().await.unwrap();
    assert!(!format!("{events:?}").contains("rate_limit"));
    stop.cancel();
    head.shutdown().await.unwrap();
}
