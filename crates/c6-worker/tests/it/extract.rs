//! Reading files (docs/knowledge.md §4): the head never parses them; the
//! c6-extract kit does, on a worker, once you approve it. A watched
//! folder's files are read where they are (mounted read-only); mail and
//! uploads go with the job. Needs Docker or Podman and the network (it
//! builds the kit), so it runs only with C6_TEST_KITS=1.

use std::time::Duration;

use base64::Engine;
use c6_head::HeadConfig;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Building the kit's image takes up to half an hour, so each wait says how long.
// test-lint: allow (opt-in kit build)
async fn eventually<T>(what: &str, wait: Duration, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn the_kit_reads_files_on_workers() {
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
    let e = c6_head::api::new_enrollment(&head.state, Some("box")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let send = |method: reqwest::Method, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.request(method, &url).json(&body).send().await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{url}: {s}: {v}");
            v
        }
    };
    let rpc = |method: &str, params: Value| send(reqwest::Method::POST, "memex/rpc", json!({"method": method, "params": params}));
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);

    // A folder on the worker "box": a page and a photo.
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("plan.html"), "<h1>Launch plan</h1><p>Ship the <b>analytical engine</b> on Friday.</p>").unwrap();
    let img = image::RgbImage::from_fn(64, 48, |x, _| image::Rgb([(x * 4) as u8, 90, 160]));
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90).encode_image(&img).unwrap();
    std::fs::write(folder.path().join("harbour.jpg"), &jpg).unwrap();
    send(reqwest::Method::PUT, "settings/memex_folders", json!([{"path": folder.path().display().to_string(), "worker": "box"}])).await;

    // Reading waits for you: the kit is offered for approval.
    let decision = eventually("the kit offered", Duration::from_secs(60), async || {
        let ds = send(reqwest::Method::GET, "decisions", json!({})).await;
        ds.as_array()?.iter().find(|d| d["title"].as_str().unwrap_or("").contains("c6-extract")).cloned()
    })
    .await;
    assert!(decision["document"].as_str().unwrap().contains("No network"), "{decision}");
    send(reqwest::Method::POST, &format!("decisions/{}/answer", decision["id"]), json!({"option": "Install"})).await;

    // The worker builds the kit and reads the folder where it is.
    eventually("the folder read", Duration::from_secs(1800), async || {
        let s = send(reqwest::Method::GET, "memex/status", json!({})).await;
        (s["items"]["document"] == 1 && s["media"] == 1).then_some(())
    })
    .await;
    let hits = send(reqwest::Method::GET, "memex/search?q=analytical%20engine", json!({})).await;
    assert!(hits.to_string().contains("Launch plan"), "{hits}");
    let photos = rpc("memex.media.list", json!({})).await;
    let photo = &photos[0];
    assert_eq!((photo["title"].as_str(), photo["kind"].as_str()), (Some("harbour.jpg"), Some("photo")), "{photos}");
    let got = rpc("memex.media.get", json!({"id": photo["id"]})).await;
    assert_eq!((got["item"]["meta"]["width"].as_i64(), got["item"]["meta"]["height"].as_i64()), (Some(64), Some(48)), "{got}");
    let thumb = rpc("memex.media.thumb", json!({"id": photo["id"]})).await;
    let thumb = base64::engine::general_purpose::STANDARD.decode(thumb["data"].as_str().unwrap()).unwrap();
    assert_eq!(&thumb[..2], &[0xFF, 0xD8]);
    // The document's original comes from its worker.
    let doc = hits.as_array().unwrap().iter().find(|h| h["title"] == "plan.html").unwrap();
    let original = http.get(format!("{api}/memex/items/{}/content", doc["id"].as_str().unwrap())).send().await.unwrap();
    assert!(original.text().await.unwrap().contains("<b>analytical engine</b>"));

    // Mail goes with the job: read, threaded, its attachment read when asked.
    let mail = "From: Ada <ada@example.com>\r\nTo: me@example.com\r\nSubject: Engines\r\nMessage-ID: <m1@example.com>\r\n\
Date: Tue, 3 Mar 2009 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n\
Content-Type: text/html\r\n\r\n<p>Shall we meet about the <i>engine</i>?</p>\r\n--b\r\nContent-Type: text/plain; name=notes.txt\r\n\
Content-Disposition: attachment; filename=notes.txt\r\n\r\nGears and cards.\r\n--b--\r\n";
    let r = rpc(
        "memex.ingest",
        json!({"items": [{"type": "message/rfc822", "data": b64(mail.as_bytes()), "source": {"kind": "mbox", "id": "a"}, "external_id": "1"}]}),
    )
    .await;
    assert_eq!(r["results"][0]["status"], "created", "{r}");
    let threads = rpc("memex.mail.threads", json!({"view": "all"})).await;
    let t = &threads[0];
    let msgs = rpc("memex.mail.thread", json!({"account": t["account"], "thread_id": t["thread_id"]})).await;
    assert!(msgs[0]["html"].as_str().unwrap().contains("<i>engine</i>"), "{msgs}");
    assert_eq!(msgs[0]["attachments"][0]["name"], "notes.txt", "{msgs}");
    let a = rpc("memex.mail.attachment", json!({"item_id": msgs[0]["item_id"], "index": 0})).await;
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(a["data"].as_str().unwrap()).unwrap(), b"Gears and cards.");

    stop.cancel();
    head.shutdown().await.unwrap();
}
