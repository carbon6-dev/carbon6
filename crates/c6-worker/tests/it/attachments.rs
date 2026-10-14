//! Attachments and @-mentions reach the model: images as image parts, files
//! and mentioned paths as text.

use std::time::Duration;

use base64::Engine;
use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
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
async fn attachments_and_mentions_reach_the_model() {
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

    std::fs::write(p.join("notes.md"), "the secret word is papaya").unwrap();
    let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let b64 = base64::engine::general_purpose::STANDARD;

    mock.push(Reply::Text("hello".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": p.display().to_string(),
                      "connection_id": conn.id, "model": "m", "prompt": "hi"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("first turn", async || (mock.requests().len() == 1).then_some(())).await;
    mock.push(Reply::Text("seen".into()));
    let r = http
        .post(format!("{api}/sessions/{}/messages", s.id))
        .json(&json!({"text": "what is in @notes.md and this picture?",
                      "attachments": [{"name": "shot.png", "mime": "image/png", "data": b64.encode(png)},
                                      {"name": "log.txt", "mime": "text/plain", "data": b64.encode("line one")}]}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    eventually("second request", async || (mock.requests().len() == 2).then_some(())).await;
    let last = mock.requests().last().unwrap()["messages"].to_string();
    assert!(last.contains("data:image/png;base64,"), "{last}");
    assert!(last.contains("papaya"), "the mentioned file is attached: {last}");
    assert!(last.contains("line one"), "{last}");

    // The transcript lists them, and each can be fetched back.
    let user = eventually("user event", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        evs.into_iter().find(|e| matches!(&e.kind, EventKind::User { text, .. } if text.contains("picture")))
    })
    .await;
    let EventKind::User { attachments, .. } = &user.kind else { unreachable!() };
    let names: Vec<&str> = attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["shot.png", "log.txt", "notes.md"]);
    let img = http.get(format!("{api}/sessions/{}/attachments/{}/0", s.id, user.seq)).send().await.unwrap();
    assert_eq!(img.headers()["content-type"], "image/png");
    assert_eq!(img.bytes().await.unwrap().as_ref(), png);

    // Attachments can come with a new session's first message too.
    mock.push(Reply::Text("ok".into()));
    let n = mock.requests().len();
    http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": p.display().to_string(),
                      "connection_id": conn.id, "model": "m", "prompt": "look",
                      "attachments": [{"name": "first.png", "mime": "image/png", "data": b64.encode(png)}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    eventually("first message", async || (mock.requests().len() > n).then_some(())).await;
    assert!(mock.requests()[n]["messages"].to_string().contains("data:image/png;base64,"));

    // A tool's image reaches the model too: reading a PNG shows it, and the
    // transcript serves what the model saw.
    let full_png = [png.as_slice(), &[0u8; 24]].concat();
    std::fs::write(p.join("slide-1.png"), &full_png).unwrap();
    mock.push(Reply::ToolCalls(vec![("read".into(), json!({"path": "slide-1.png"}).to_string())]));
    mock.push(Reply::Text("The slide looks fine.".into()));
    let n = mock.requests().len();
    http.post(format!("{api}/sessions/{}/messages", s.id)).json(&json!({"text": "check the slide"})).send().await.unwrap();
    eventually("the request after the read", async || (mock.requests().len() >= n + 2).then_some(())).await;
    let after = mock.requests()[n + 1]["messages"].to_string();
    assert!(after.contains("[image from read]") && after.contains(&b64.encode(&full_png)), "{after}");
    let result = eventually("the tool result", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        evs.into_iter().find(|e| matches!(&e.kind, EventKind::ToolResult { images, .. } if *images == 1))
    })
    .await;
    let img = http.get(format!("{api}/sessions/{}/attachments/{}/0", s.id, result.seq)).send().await.unwrap();
    assert_eq!(img.headers()["content-type"], "image/png");
    assert_eq!(img.bytes().await.unwrap().as_ref(), full_png.as_slice());

    // An unsupported type is refused.
    let r = http
        .post(format!("{api}/sessions/{}/messages", s.id))
        .json(&json!({"text": "", "attachments": [{"name": "x.bin", "mime": "application/octet-stream", "data": b64.encode([0xffu8, 0xfe, 0])}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    stop.cancel();
    head.shutdown().await.unwrap();
}
