//! Attachments and @-mentions reach the model: images as image parts, PDFs
//! as native documents or extracted text, and other files as text or paths.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use c6_api::{Connection, Event, EventKind, QueuedInput, Session, SessionDetail};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use prost::Message;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn start_worker(head_dir: &Path, state: &Path) -> (c6_head::Head, String, uuid::Uuid, CancellationToken) {
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
    let worker = Worker::open(state, Identity::load(state).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    (head, api, worker.identity.meta.worker_id.0, stop)
}

#[tokio::test]
async fn attachments_and_mentions_reach_the_model() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (head, api, wid, stop) = start_worker(head_dir.path(), state.path()).await;
    let http = reqwest::Client::new();
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
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(),
                      "connection_id": conn.id, "model": "m", "prompt": "hi"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!s.on_head, "a worker runs it");
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
    assert!(img.headers()["content-security-policy"].to_str().unwrap().starts_with("sandbox"), "opened alone, an image cannot act as you");
    assert_eq!(img.bytes().await.unwrap().as_ref(), png);

    // Attachments can come with a new session's first message too.
    mock.push(Reply::Text("ok".into()));
    let n = mock.requests().len();
    http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": p.display().to_string(),
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

    stop.cancel();
    head.shutdown().await.unwrap();
}

// A real one-page PDF with a searchable Helvetica text stream and xref table.
fn pdf(text: &str) -> Vec<u8> {
    let stream = format!("BT /F1 12 Tf 40 150 Td ({text}) Tj ET\n");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()),
    ];
    let mut bytes = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        writeln!(bytes, "{} 0 obj\n{object}\nendobj", i + 1).unwrap();
    }
    let xref = bytes.len();
    writeln!(bytes, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(bytes, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(bytes, "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF", objects.len() + 1).unwrap();
    bytes
}

async fn post(http: &reqwest::Client, url: &str, body: Value) -> String {
    let r = http.post(url).json(&body).send().await.unwrap();
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "POST {url}: {status} {text}");
    text
}

fn upload(name: &str, mime: &str, bytes: &[u8]) -> Value {
    json!({"name": name, "mime": mime, "data": B64.encode(bytes)})
}

fn last_user_content(request: &Value) -> Option<&Value> {
    request["messages"].as_array()?.iter().rev().find(|m| m["role"] == "user")?.get("content")
}

async fn capture(mock: &MockLlm, marker: &str) -> String {
    eventually("the attachment turn to reach the model", async || {
        mock.requests().iter().filter_map(last_user_content).map(Value::to_string).find(|content| content.contains(marker))
    })
    .await
}

fn stored_attachment(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_file() && std::fs::read(path).unwrap() == bytes)
        .unwrap_or_else(|| panic!("{name} was not stored byte-for-byte in {}", dir.display()))
        .canonicalize()
        .unwrap()
}

fn assert_stored_and_mentioned(dir: &Path, name: &str, bytes: &[u8], content: &str) {
    let entry = stored_attachment(dir, name, bytes);
    assert!(content.contains(name), "missing attachment name {name}: {content}");
    assert!(content.contains(entry.to_str().unwrap()), "missing stored path {}: {content}", entry.display());
}

#[tokio::test]
async fn pdf_and_binary_attachments_reach_openai_compat() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (head, api, wid, stop) = start_worker(head_dir.path(), state.path()).await;
    let http = reqwest::Client::new();
    let conn: Connection =
        serde_json::from_str(&post(&http, &format!("{api}/connections"), json!({"name": "compat", "base_url": mock.base_url()})).await)
            .unwrap();

    let first_text = "First PDF contains a violet narwhal";
    let first_pdf = pdf(first_text);
    let first_binary = [0xff, 0xfe, 0, 1];
    let prompt = "Consider the first document and opaque file";
    mock.push_about(prompt, Reply::Text("Got them".into()));
    let s: Session = serde_json::from_str(
        &post(
            &http,
            &format!("{api}/sessions"),
            json!({"worker_id": wid, "cwd": project.path(), "connection_id": conn.id, "model": "m", "prompt": prompt,
                   "attachments": [upload("first.pdf", "application/pdf", &first_pdf),
                                   upload("first.bin", "application/octet-stream", &first_binary)]}),
        )
        .await,
    )
    .unwrap();
    assert!(!s.on_head);
    let dir = state.path().join("attachments").join(&s.id);
    let check = |content: &str, name: &str, bytes: &[u8], text: &str, binary_name: &str, binary: &[u8]| {
        assert_stored_and_mentioned(&dir, name, bytes, content);
        assert!(content.contains(text), "missing extracted PDF text: {content}");
        assert!(!content.contains(&B64.encode(bytes)), "OpenAI-compatible chat should not receive native PDF data: {content}");
        assert_stored_and_mentioned(&dir, binary_name, binary, content);
        assert!(!content.contains(&B64.encode(binary)), "binary data should not be inlined: {content}");
    };
    check(&capture(&mock, prompt).await, "first.pdf", &first_pdf, first_text, "first.bin", &first_binary);

    let later_text = "Second PDF contains an orange pangolin";
    let later_pdf = pdf(later_text);
    let broken_pdf = b"%PDF-1.4\ninvalid";
    let later_binary = [0xff, 0xfe, 0, 2, 3];
    let followup = "Now consider the second document and opaque file";
    mock.push_about(followup, Reply::Text("Got these too".into()));
    post(
        &http,
        &format!("{api}/sessions/{}/messages", s.id),
        json!({"text": followup, "attachments": [upload("later.pdf", "application/pdf", &later_pdf),
                                                upload("broken.pdf", "application/pdf", broken_pdf),
                                                upload("later.bin", "application/octet-stream", &later_binary)]}),
    )
    .await;
    let captured = capture(&mock, followup).await;
    check(&captured, "later.pdf", &later_pdf, later_text, "later.bin", &later_binary);
    assert_stored_and_mentioned(&dir, "broken.pdf", broken_pdf, &captured);
    assert!(captured.to_ascii_lowercase().contains("text extraction failed"), "missing PDF extraction failure note: {captured}");

    stop.cancel();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_attachment_edit_replaces_bytes_and_notes() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (head, api, wid, stop) = start_worker(head_dir.path(), state.path()).await;
    let http = reqwest::Client::new();
    let conn: Connection =
        serde_json::from_str(&post(&http, &format!("{api}/connections"), json!({"name": "compat", "base_url": mock.base_url()})).await)
            .unwrap();
    let first = "Hold this model turn open";
    let gate = mock.push_gated_about(first, Reply::Text("Waiting.".into()));
    let s: Session = serde_json::from_str(
        &post(
            &http,
            &format!("{api}/sessions"),
            json!({"worker_id": wid, "cwd": project.path(), "connection_id": conn.id, "model": "m", "prompt": first}),
        )
        .await,
    )
    .unwrap();
    capture(&mock, first).await;

    // This tests queue replacement, not the external PDF converter's launch
    // time inside the RPC's two-second preparation budget. HTML still needs
    // extraction and generated notes, but is parsed in-process. Separate PDF
    // tests above exercise the platform converter and original-byte storage.
    let old_text = "Stale attachment contains a turquoise wombat";
    let old_document = format!("<html><body><p>{old_text}</p></body></html>").into_bytes();
    let old_binary = [0xff, 0, 1];
    let (document_name, bin_name) = ("queued.html", "queued.bin");
    let url = format!("{api}/sessions/{}", s.id);
    let queued: QueuedInput = serde_json::from_str(
        &post(
            &http,
            &format!("{url}/messages"),
            json!({"text": "Original queued prompt", "attachments": [upload(document_name, "text/html", &old_document),
                                                                      upload(bin_name, "application/octet-stream", &old_binary)]}),
        )
        .await,
    )
    .unwrap();
    eventually("the queued input to be with the worker", async || {
        let detail: SessionDetail = http.get(&url).send().await.ok()?.json().await.ok()?;
        detail.queued.iter().any(|input| input.command_id == queued.command_id && input.with_agent).then_some(())
    })
    .await;

    // Public text edits preserve uploads. Stage the old bytes through that
    // route, then send the worker's full replacement RPC (the public API
    // does not let callers change the uploads of a queued message).
    let intermediate = "Intermediate queued prompt";
    let r = http.put(format!("{url}/queue/{}", queued.command_id)).json(&json!({"text": intermediate})).send().await.unwrap();
    assert!(r.status().is_success(), "queue edit failed: {}", r.text().await.unwrap());
    let dir = state.path().join("attachments").join(&s.id);
    let old_document_path = stored_attachment(&dir, document_name, &old_document);
    let old_bin_path = stored_attachment(&dir, bin_name, &old_binary);

    let updated = "Use the replacement queued files";
    let new_text = "Replacement attachment contains a golden axolotl";
    let new_document = format!("<html><body><p>{new_text}</p></body></html>").into_bytes();
    let new_binary = [0xfe, 0, 2, 3];
    let input = c6_proto::UserInput {
        text: updated.into(),
        attachments: vec![
            c6_proto::Attachment { name: document_name.into(), mime: "text/html".into(), data: new_document.clone() },
            c6_proto::Attachment { name: bin_name.into(), mime: "application/octet-stream".into(), data: new_binary.to_vec() },
        ],
        ..Default::default()
    };
    let result = head
        .state
        .registry
        .call(
            c6_core::WorkerId(wid),
            "session.queue_op",
            json!({"session_id": s.id, "command_id": queued.command_id, "op": "update", "data": B64.encode(input.encode_to_vec())}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "updated");

    mock.push_about(updated, Reply::Text("Replacement seen.".into()));
    gate.notify_one();
    let content = capture(&mock, updated).await;
    assert_stored_and_mentioned(&dir, document_name, &new_document, &content);
    assert_stored_and_mentioned(&dir, bin_name, &new_binary, &content);
    assert!(content.contains(new_text), "missing replacement document text: {content}");
    assert_eq!(content.matches("Worker-local path:").count(), 2, "expected one note per replacement file: {content}");
    for stale in [old_text, intermediate, "Original queued prompt", old_document_path.to_str().unwrap(), old_bin_path.to_str().unwrap()] {
        assert!(!content.contains(stale), "stale queued data {stale:?}: {content}");
    }
    assert_eq!(std::fs::read(old_document_path).unwrap(), old_document, "editing may not overwrite a path already handed out");
    assert_eq!(std::fs::read(old_bin_path).unwrap(), old_binary, "editing may not overwrite a path already handed out");

    stop.cancel();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn pdf_attachment_reaches_anthropic_as_a_document() {
    // MockLlm only implements chat completions; capture the Messages API
    // request locally so this exercises the worker as well as serialization.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| async move {
            let _ = tx.send(body);
            ([("content-type", "text/event-stream")], "data: {\"type\":\"message_stop\"}\n\n")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (head, api, wid, stop) = start_worker(head_dir.path(), state.path()).await;
    let http = reqwest::Client::new();
    let conn: Connection = serde_json::from_str(
        &post(
            &http,
            &format!("{api}/connections"),
            json!({"name": "messages", "kind": "anthropic", "base_url": base_url, "api_key": "test-key"}),
        )
        .await,
    )
    .unwrap();
    let bytes = pdf("Native PDF fixture");
    let s: Session = serde_json::from_str(
        &post(
            &http,
            &format!("{api}/sessions"),
            json!({"worker_id": wid, "cwd": project.path(), "connection_id": conn.id, "model": "claude-sonnet-4-6",
                   "prompt": "Inspect this native PDF", "attachments": [upload("native.pdf", "application/pdf", &bytes)]}),
        )
        .await,
    )
    .unwrap();
    assert!(!s.on_head);
    let request = c6_testkit::within("the Anthropic attachment turn", async {
        while let Some(request) = rx.recv().await {
            if last_user_content(&request).is_some_and(|content| content.to_string().contains("Inspect this native PDF")) {
                return request;
            }
        }
        panic!("the Anthropic capture server stopped before receiving the attachment turn");
    })
    .await;
    let content = last_user_content(&request).unwrap();
    let parts = content.as_array().unwrap_or_else(|| panic!("expected Anthropic content blocks: {content}"));
    let document = parts.iter().find(|part| part["type"] == "document").unwrap_or_else(|| panic!("missing document: {content}"));
    assert_eq!(document["source"], json!({"type": "base64", "media_type": "application/pdf", "data": B64.encode(&bytes)}));
    let dir = state.path().join("attachments").join(&s.id);
    assert_stored_and_mentioned(&dir, "native.pdf", &bytes, &content.to_string());

    stop.cancel();
    head.shutdown().await.unwrap();
    server.abort();
}
