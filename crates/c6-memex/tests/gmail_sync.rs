//! Gmail's output and saved positions, against invented mail on loopback.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use base64::Engine;
use serde_json::{Value, json};

const MIB: usize = 1024 * 1024;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Default)]
struct Mailbox {
    messages: BTreeMap<String, usize>,
    history: Vec<Value>,
    /// A failing path suffix, status, body and Retry-After.
    failure: Option<(String, u16, Value, String)>,
    calls: Vec<(String, String, String)>,
}

type Google = Arc<Mutex<HashMap<String, Mailbox>>>;

fn payload(id: &str) -> Value {
    json!({"headers": [
        {"name": "From", "value": "sender@example.test"},
        {"name": "To", "value": "reader@example.test"},
        {"name": "Subject", "value": format!("Invented {id}")},
        {"name": "Message-ID", "value": format!("<{id}@example.test>")},
        {"name": "References", "value": "<parent@example.test>"}],
        "parts": [{"mimeType": "text/plain", "body": {"data": B64.encode("A small invented body.")}},
            {"filename": "invented.bin", "body": {"attachmentId": "attachment", "size": 49 * MIB}}]})
}

async fn api(State(g): State<Google>, Path(path): Path<String>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let token = headers["authorization"].to_str().unwrap().strip_prefix("Bearer ").unwrap();
    let mut g = g.lock().unwrap();
    let b = g.get_mut(token).unwrap();
    let format = q.get("format").cloned().unwrap_or_default();
    b.calls.push((path.clone(), format.clone(), q.get("startHistoryId").cloned().unwrap_or_default()));
    if let Some((suffix, status, body, retry)) = &b.failure
        && path.ends_with(suffix)
    {
        if *status == 0 {
            let broken =
                futures_util::stream::once(async { Err::<axum::body::Bytes, _>(std::io::Error::other("invented broken connection")) });
            return Response::new(axum::body::Body::from_stream(broken));
        }
        return (StatusCode::from_u16(*status).unwrap(), [("retry-after", retry.clone())], Json(body.clone())).into_response();
    }
    let value = match path.as_str() {
        "profile" => json!({"historyId": b.history.last().map_or("1", |h| h["id"].as_str().unwrap())}),
        "labels" => json!({"labels": [{"id": "INBOX", "name": "INBOX", "type": "system"}]}),
        "history" => {
            let start = q["startHistoryId"].parse::<u64>().unwrap();
            let records: Vec<_> = b.history.iter().filter(|h| h["id"].as_str().unwrap().parse::<u64>().unwrap() > start).collect();
            let offset = q.get("pageToken").map_or(0, |p| p.parse::<usize>().unwrap());
            let end = (offset + 100).min(records.len());
            json!({"history": records[offset..end], "historyId": b.history.last().map_or("1", |h| h["id"].as_str().unwrap()),
                "nextPageToken": (end < records.len()).then(|| end.to_string())})
        }
        "messages" => {
            // The label check is empty unless a test supplies check_queue.
            let ids: Vec<_> = if q.contains_key("q") { Vec::new() } else { b.messages.keys().collect() };
            let offset = q.get("pageToken").map_or(0, |p| p.parse::<usize>().unwrap());
            let end = (offset + 100).min(ids.len());
            json!({"messages": ids[offset..end].iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
                "nextPageToken": (end < ids.len()).then(|| end.to_string())})
        }
        _ => {
            let id = path.strip_prefix("messages/").unwrap();
            let Some(size) = b.messages.get(id) else { return StatusCode::NOT_FOUND.into_response() };
            let mut value = json!({"id": id, "threadId": format!("t-{id}"), "labelIds": ["INBOX"], "internalDate": "1793188800000"});
            if format == "full" {
                value["payload"] = payload(id);
            } else if format == "raw" {
                let mut raw = format!("From: sender@example.test\r\nTo: reader@example.test\r\nSubject: Invented {id}\r\nMessage-ID: <{id}@example.test>\r\nContent-Type: text/plain\r\n\r\n").into_bytes();
                raw.resize((*size).max(raw.len()), b'x');
                value["raw"] = json!(B64.encode(raw));
            }
            value
        }
    };
    Json(value).into_response()
}

struct Fixture {
    google: Google,
    env: c6_google::Env,
    server: tokio::task::JoinHandle<()>,
    state: Vec<Value>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(mailboxes: Vec<(&str, Mailbox)>, state: Vec<Value>) -> Self {
        let accounts: Vec<_> =
            mailboxes.iter().map(|(id, _)| json!({"id": id, "account": format!("{id}@example.test"), "access_token": id})).collect();
        let google = Arc::new(Mutex::new(mailboxes.into_iter().map(|(id, b)| (id.to_owned(), b)).collect()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/gmail/v1/users/me/{*path}", get(api)).with_state(google.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { google, env: c6_google::Env { accounts: json!(accounts).to_string(), api: url.clone(), people: url }, server, state }
    }

    async fn run(&mut self) -> (Value, BTreeMap<String, Vec<u8>>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("state/gmail_sync.json"), json!(self.state).to_string()).unwrap();
        c6_google::run("mail-sync", dir.path(), &self.env).await.unwrap();
        let out = dir.path().join("out");
        let mut paths = vec![out.clone()];
        let mut bytes = 0;
        let mut files = BTreeMap::new();
        while let Some(dir) = paths.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    paths.push(path);
                } else {
                    let data = std::fs::read(&path).unwrap();
                    bytes += data.len();
                    files.insert(path.strip_prefix(&out).unwrap().to_string_lossy().into_owned(), data);
                }
            }
        }
        assert!(bytes <= c6_google::mail::MAX_OUTPUT, "all out/ bytes: {bytes}");
        assert!((bytes as u64) < c6_worker::rpc::MAX_JOB_OUTPUT, "worker ceiling: {bytes}");
        let batch: Value = serde_json::from_slice(&files["ingest.json"]).unwrap();
        for item in batch["items"].as_array().unwrap() {
            if item["type"] == "message/rfc822"
                && let Some(file) = item["file"].as_str()
            {
                assert!(files[file].len() as u64 <= c6_memex::ingest::MAIL_LIMIT, "message exceeds the memex limit: {file}");
            }
        }
        for rec in batch["records"].as_array().unwrap() {
            if let Some(old) = self.state.iter_mut().find(|s| s["key"] == rec["fields"]["key"]) {
                *old = rec["fields"].clone();
            } else {
                self.state.push(rec["fields"].clone());
            }
        }
        (batch, files)
    }

    fn state(&self, id: &str) -> &Value {
        self.state.iter().find(|s| s["key"] == format!("mail:{id}")).unwrap()
    }

    fn edit(&mut self, id: &str) -> &mut Value {
        self.state.iter_mut().find(|s| s["key"] == format!("mail:{id}")).unwrap()
    }
}

fn state(id: &str, queue: &[&str]) -> Value {
    json!({"key": format!("mail:{id}"), "phase": "import", "cursor": "1", "queue": queue, "done": 0, "checked": 1})
}

fn mailbox(messages: &[(&str, usize)]) -> Mailbox {
    Mailbox { messages: messages.iter().map(|(id, size)| ((*id).into(), *size)).collect(), ..Default::default() }
}

fn messages(batch: &Value) -> Vec<String> {
    batch["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["file"].is_string())
        .map(|i| i["external_id"].as_str().unwrap().to_owned())
        .collect()
}

fn added(record: u64, id: &str) -> Value {
    json!({"id": record.to_string(), "messagesAdded": [{"message": {"id": id, "labelIds": ["INBOX"]}}]})
}

/// The kit's independent build still stays below the worker's envelope.
#[test]
fn the_kit_ceiling_is_below_the_workers() {
    assert_eq!(c6_worker::rpc::MAX_JOB_OUTPUT, 44 * MIB as u64);
    assert!(c6_google::mail::MAX_OUTPUT as u64 <= c6_worker::rpc::MAX_JOB_OUTPUT - MIB as u64);
}

/// Whole messages emitted by the independent kit must fit the memex.
#[test]
fn the_kit_message_limit_matches_the_memex() {
    assert_eq!(c6_google::mail::MAIL_LIMIT as u64, c6_memex::ingest::MAIL_LIMIT);
}

/// The memex boundary, even below the job ceiling, decides what stays whole.
#[tokio::test]
async fn messages_above_the_memex_limit_become_stand_ins() {
    for size in [25 * MIB, 25 * MIB + 4, 30 * MIB] {
        let mut f = Fixture::new(vec![("a", mailbox(&[("large", size), ("next", 1024)]))], vec![state("a", &["large", "next"])]).await;
        let (batch, files) = f.run().await;
        let item = &batch["items"][0];
        let bytes = &files[item["file"].as_str().unwrap()];
        if size == 25 * MIB {
            assert_eq!(bytes.len(), size);
            assert_eq!(messages(&batch), ["a:large"]);
            assert!(batch["errors"].as_array().unwrap().is_empty());
            assert!(f.google.lock().unwrap()["a"].calls.iter().all(|(_, format, _)| format != "full"));
            assert_eq!(messages(&f.run().await.0), ["a:next"]);
        } else {
            assert_eq!(messages(&batch), ["a:large", "a:next"]);
            assert!(bytes.len() < 192 * 1024);
            let text = std::str::from_utf8(bytes).unwrap();
            for s in ["Message-ID: <large@example.test>", "Subject: Invented large", "A small invented body.", "open it in Gmail"] {
                assert!(text.contains(s), "missing {s}");
            }
            assert!(text.contains(&format!("{size} bytes")));
            let errors = batch["errors"].as_array().unwrap();
            assert_eq!(errors.len(), 1);
            assert!(errors[0].as_str().unwrap().contains("a@example.test: message large"));
            assert_eq!(f.google.lock().unwrap()["a"].calls.iter().filter(|(_, format, _)| format == "full").count(), 1);
        }
        assert_eq!(f.state("a")["phase"], "live");
        assert_eq!(f.state("a")["done"], 2);
    }
}

/// A replacement import snapshots its cursor before listing, so mail between runs survives.
#[tokio::test]
async fn expired_history_reimports_mail_arriving_between_runs() {
    let mut b = mailbox(&[("old", 1024)]);
    b.failure = Some(("history".into(), 404, json!({"error": {"message": "Expired history"}}), String::new()));
    let mut st = state("a", &[]);
    st["phase"] = json!("live");
    st["done"] = json!(90_437);
    let mut f = Fixture::new(vec![("a", b)], vec![st]).await;
    let (first, _) = f.run().await;
    let reset = f.state("a").clone();
    let calls = f.google.lock().unwrap()["a"].calls.clone();
    {
        let mut g = f.google.lock().unwrap();
        let b = g.get_mut("a").unwrap();
        b.failure = None;
        b.messages.insert("new".into(), 1024);
        b.history.push(added(2, "new"));
        b.calls.clear();
    }
    let (second, _) = f.run().await;
    assert_eq!(messages(&second), ["a:new", "a:old"]);
    assert!(messages(&first).is_empty());
    assert_eq!(reset["cursor"], Value::Null);
    assert_eq!(reset["done"], 0);
    assert_eq!(first["again"], true);
    assert!(calls.iter().all(|(path, _, _)| path != "messages"));
    assert_eq!(f.state("a")["cursor"], "2");
    assert_eq!(f.state("a")["phase"], "live");
    assert_eq!(f.state("a")["done"], 2);
    let g = f.google.lock().unwrap();
    let calls = &g["a"].calls;
    assert!(
        calls.iter().position(|(path, _, _)| path == "profile").unwrap()
            < calls.iter().position(|(path, _, _)| path == "messages").unwrap()
    );
}

/// Repeated runs keep every id, in order, including a deferred large head.
#[tokio::test]
async fn imports_fit_and_a_big_message_takes_its_own_run() {
    let ids = ["a", "b", "c", "d", "e", "f"];
    let b = mailbox(&[("a", 12 * MIB), ("b", 12 * MIB), ("c", 2 * MIB), ("d", 24 * MIB + MIB / 2), ("e", 12 * MIB), ("f", 12 * MIB)]);
    let mut f = Fixture::new(vec![("a", b)], vec![state("a", &ids)]).await;
    let mut kept = Vec::new();
    let mut batches = Vec::new();
    loop {
        let (batch, _) = f.run().await;
        let got = messages(&batch);
        assert!(!got.is_empty(), "an import makes progress");
        kept.extend(got.clone());
        batches.push(got);
        if batch["again"] == false {
            break;
        }
    }
    assert_eq!(kept, ids.map(|id| format!("a:{id}")));
    assert!(batches.contains(&vec!["a:d".to_owned()]));
    assert_eq!(f.state("a")["phase"], "live");
    assert_eq!(f.state("a")["done"], 6);
    assert!(f.google.lock().unwrap()["a"].calls.iter().all(|(_, format, _)| format != "full"));
}

/// Gmail keeps the attachment; carbon6 gets a bounded, identifiable message.
#[tokio::test]
async fn a_message_over_the_ceiling_becomes_a_stand_in() {
    let mut b = mailbox(&[("huge", 50 * MIB), ("next", 1024)]);
    // The same message is new in history and waiting in the import page.
    b.history = vec![added(2, "huge")];
    let mut f = Fixture::new(vec![("a", b)], vec![state("a", &["huge", "next"])]).await;
    let (batch, files) = f.run().await;
    assert_eq!(messages(&batch), ["a:huge", "a:next"]);
    let item = &batch["items"][0];
    let bytes = &files[item["file"].as_str().unwrap()];
    assert!(bytes.len() < 192 * 1024);
    let text = std::str::from_utf8(bytes).unwrap();
    for s in ["Message-ID: <huge@example.test>", "Subject: Invented huge", "A small invented body.", "invented.bin", "open it in Gmail"] {
        assert!(text.contains(s), "missing {s}");
    }
    assert_eq!(
        item["attributes"],
        json!({"account": "a", "message": "huge", "thread": "t-huge", "labels": ["INBOX"], "received": 1793188800000i64, "tell": false})
    );
    let errors = batch["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].as_str().unwrap().contains("a@example.test: message huge"));
    assert!(!errors[0].as_str().unwrap().contains("Invented huge"));
    assert_eq!(f.state("a")["phase"], "live");
    let g = f.google.lock().unwrap();
    for format in ["raw", "full"] {
        assert_eq!(g["a"].calls.iter().filter(|(path, f, _)| path == "messages/huge" && f == format).count(), 1);
    }
}

/// New mail is queued before import; labels and deletions respect the queue.
#[tokio::test]
async fn a_live_burst_advances_the_cursor_and_arrives_before_import() {
    let mut b = mailbox(&[("new1", 16 * MIB), ("new2", 16 * MIB), ("new3", 16 * MIB), ("gone", 1), ("old", 1)]);
    b.history = vec![
        added(2, "new1"),
        added(3, "new2"),
        added(4, "new3"),
        added(5, "gone"),
        json!({"id": "6", "labelsAdded": [{"message": {"id": "new2", "labelIds": ["INBOX"]}}]}),
        json!({"id": "7", "messagesDeleted": [{"message": {"id": "gone"}}]}),
    ];
    let mut f = Fixture::new(vec![("a", b)], vec![state("a", &["old"])]).await;
    let (first, _) = f.run().await;
    assert_eq!(messages(&first), ["a:new1"]);
    assert_eq!(f.state("a")["cursor"], "7");
    assert_eq!(f.state("a")["fetch_queue"], json!([{"id": "new2", "tell": false}, {"id": "new3", "tell": false}]));
    assert_eq!(first["items"].as_array().unwrap().len(), 1, "no relabel of an unknown message");
    assert_eq!(first["deleted"], json!([{"external_id": "a:gone"}]));
    let mut kept = messages(&first);
    while f.state("a")["phase"] != "live" {
        let (batch, _) = f.run().await;
        assert!(!messages(&batch).is_empty());
        kept.extend(messages(&batch));
    }
    assert_eq!(kept, ["a:new1", "a:new2", "a:new3", "a:old"]);
}

/// A postponed import message wins over new mail arriving on every run.
#[tokio::test]
async fn a_deferred_head_gets_first_turn_before_new_mail() {
    let mut f = Fixture::new(
        vec![("a", mailbox(&[("small", MIB), ("big", 24 * MIB + MIB / 2), ("last", 1)]))],
        vec![state("a", &["small", "big", "last"])],
    )
    .await;
    assert_eq!(messages(&f.run().await.0), ["a:small"]);
    assert_eq!(f.state("a")["first"], "queue");
    {
        let mut g = f.google.lock().unwrap();
        let b = g.get_mut("a").unwrap();
        b.messages.insert("new".into(), MIB);
        b.history.push(added(2, "new"));
    }
    assert_eq!(messages(&f.run().await.0), ["a:big"]);
    assert_eq!(messages(&f.run().await.0), ["a:new", "a:last"]);
}

/// Metadata alone can fill out/: resume at the last complete history id.
#[tokio::test]
async fn mass_label_changes_resume_without_crossing_the_ceiling() {
    let mut b = Mailbox::default();
    let labels: Vec<_> = (0..14).map(|n| format!("{n:03}{}", "x".repeat(290))).collect();
    for n in 2..12_002 {
        b.history.push(json!({"id": n.to_string(), "labelsRemoved": [{"message": {"id": format!("m{n}"), "labelIds": labels}}]}));
    }
    let mut s = state("a", &[]);
    s["phase"] = json!("live");
    let mut f = Fixture::new(vec![("a", b)], vec![s]).await;
    let (batch, _) = f.run().await;
    let n = batch["items"].as_array().unwrap().len();
    assert!(n > 0 && n < 12_000);
    assert_eq!(f.state("a")["cursor"], (n + 1).to_string());
    assert_eq!(batch["again"], true);
    let (batch, _) = f.run().await;
    assert_eq!(batch["items"].as_array().unwrap().len() + n, 12_000);
    assert_eq!(f.state("a")["cursor"], "12001");
    assert_eq!(batch["again"], false);
}

/// Even one large history record can span runs without advancing too early.
#[tokio::test]
async fn a_partial_history_record_keeps_its_cursor_and_offset() {
    let mut b = Mailbox::default();
    let labels: Vec<_> = (0..14).map(|n| format!("{n:03}{}", "x".repeat(290))).collect();
    b.history.push(json!({"id": "2", "labelsAdded": (0..12_000).map(|n| json!({"message": {"id": format!("m{n}"), "labelIds": labels}})).collect::<Vec<_>>()}));
    let mut s = state("a", &[]);
    s["phase"] = json!("live");
    let mut f = Fixture::new(vec![("a", b)], vec![s]).await;
    let (batch, _) = f.run().await;
    let n = batch["items"].as_array().unwrap().len();
    assert_eq!(f.state("a")["cursor"], "1");
    assert_eq!(f.state("a")["history_record"], "2");
    assert_eq!(f.state("a")["history_offset"], n);
    let (batch, _) = f.run().await;
    assert_eq!(batch["items"].as_array().unwrap().len() + n, 12_000);
    assert_eq!(f.state("a")["cursor"], "2");
    assert_eq!(batch["again"], false);
}

/// Quota preserves the failing head, sleeps across runs, and then resumes.
#[tokio::test]
async fn quota_keeps_the_import_page_and_makes_no_calls_until_retry() {
    let mut b = mailbox(&[("m1", 1000), ("m2", 1000), ("m3", 1000)]);
    b.failure = Some((
        "messages/m2".into(),
        403,
        json!({"error": {"message": "Quota exceeded for quota metric 'Total Query Cost' and limit 'Units per minute per user'",
        "errors": [{"reason": "rateLimitExceeded"}]}}),
        "3600".into(),
    ));
    let mut f = Fixture::new(vec![("a", b)], vec![state("a", &["m1", "m2", "m3"])]).await;
    f.edit("a").as_object_mut().unwrap().remove("queue");
    let before = c6_core::now_ms();
    let (batch, _) = f.run().await;
    assert_eq!(messages(&batch), ["a:m1"]);
    assert_eq!(f.state("a")["queue"], json!(["m2", "m3"]));
    assert_eq!(batch["again"], false);
    assert_eq!(f.state("a")["retry_count"], 1);
    assert!(f.state("a")["retry_at"].as_i64().unwrap() >= before + 3_600_000);
    let calls = f.google.lock().unwrap()["a"].calls.len();
    // Put time under the test's control; no wall-clock race or wait.
    f.edit("a")["retry_at"] = json!(i64::MAX);
    assert_eq!(f.run().await.0["again"], false);
    assert_eq!(f.google.lock().unwrap()["a"].calls.len(), calls);
    f.google.lock().unwrap().get_mut("a").unwrap().failure = None;
    f.edit("a")["retry_at"] = json!(0);
    let (batch, _) = f.run().await;
    assert_eq!(messages(&batch), ["a:m2", "a:m3"]);
    assert_eq!(f.state("a")["retry_count"], 0);
    assert!(f.state("a")["retry_at"].is_null());
    assert!(f.state("a")["error"].is_null());
}

/// An unavailable fetch is retried; a permanent permission failure is named.
#[tokio::test]
async fn transient_import_failures_keep_the_head_and_permission_errors_do_not() {
    for status in [0, 408, 500, 503, 403] {
        let mut b = mailbox(&[("one", 1), ("two", 1)]);
        b.failure = Some((
            "messages/one".into(),
            status,
            json!({"error": {"message": "Insufficient Permission", "errors": [{"reason": "forbidden"}]}}),
            String::new(),
        ));
        let mut f = Fixture::new(vec![("a", b)], vec![state("a", &["one", "two"])]).await;
        let (batch, _) = f.run().await;
        if status == 403 {
            assert_eq!(messages(&batch), ["a:two"]);
            assert!(batch["errors"][0].as_str().unwrap().contains("message one not imported"));
        } else {
            assert!(messages(&batch).is_empty());
            assert_eq!(f.state("a")["queue"], json!(["one", "two"]));
            f.google.lock().unwrap().get_mut("a").unwrap().failure = None;
            assert_eq!(messages(&f.run().await.0), ["a:one", "a:two"]);
        }
    }
}

/// The older label repair observes the same quota wait and saved head.
#[tokio::test]
async fn the_label_check_backs_off_without_losing_its_place() {
    let mut b = mailbox(&[("one", 1)]);
    b.failure = Some(("messages/one".into(), 429, json!({"error": {"message": "quota"}}), "120".into()));
    let mut s = state("a", &[]);
    s["phase"] = json!("live");
    s["checked"] = json!(0);
    s["check_queue"] = json!(["one"]);
    let mut f = Fixture::new(vec![("a", b)], vec![s]).await;
    assert_eq!(f.run().await.0["again"], false);
    assert_eq!(f.state("a")["check_queue"], json!(["one"]));
    assert_eq!(f.state("a")["retry_count"], 1);
    let calls = f.google.lock().unwrap()["a"].calls.len();
    f.edit("a")["retry_at"] = json!(i64::MAX);
    f.run().await;
    assert_eq!(f.google.lock().unwrap()["a"].calls.len(), calls);
    f.edit("a")["retry_at"] = json!(0);
    f.google.lock().unwrap().get_mut("a").unwrap().failure = None;
    let (batch, _) = f.run().await;
    assert_eq!(batch["items"][0]["external_id"], "a:one");
    assert!(batch["items"][0]["file"].is_null());
    assert_eq!(f.state("a")["checked"], 1);
}

/// Live work across accounts wins; imports take turns while both have mail.
#[tokio::test]
async fn accounts_get_live_mail_first_and_take_turns_importing() {
    let a = mailbox(&[("a1", 16 * MIB), ("a2", 16 * MIB), ("a3", 16 * MIB)]);
    let mut b = mailbox(&[("new", 1024), ("b1", MIB)]);
    b.history = vec![added(2, "new")];
    let mut f = Fixture::new(vec![("a", a), ("b", b)], vec![state("a", &["a1", "a2", "a3"]), state("b", &["b1"])]).await;
    assert_eq!(messages(&f.run().await.0), ["b:new", "a:a1"]);
    // a2 was deferred, so it goes first. b's import then gets its turn.
    assert_eq!(messages(&f.run().await.0), ["a:a2", "b:b1"]);
    assert_eq!(f.state("b")["phase"], "live");
    assert_eq!(f.state("a")["queue"], json!(["a3"]));
}

/// A broken import holds up neither live mail nor another account.
#[tokio::test]
async fn import_trouble_does_not_stop_live_work_or_other_accounts() {
    let mut a = mailbox(&[("old", 1), ("new", 1)]);
    a.failure = Some(("messages/old".into(), 503, json!({"error": {"message": "unavailable"}}), String::new()));
    a.history = vec![added(2, "new")];
    let b = mailbox(&[("other", 1)]);
    let mut f = Fixture::new(vec![("a", a), ("b", b)], vec![state("a", &["old"]), state("b", &["other"])]).await;
    assert_eq!(messages(&f.run().await.0), ["a:new", "b:other"]);
    assert_eq!(f.state("a")["queue"], json!(["old"]));
    {
        let mut g = f.google.lock().unwrap();
        let a = g.get_mut("a").unwrap();
        a.messages.insert("next".into(), 1);
        a.history.push(added(3, "next"));
    }
    assert_eq!(messages(&f.run().await.0), ["a:next"]);
}

/// A deferred import's transient failure still allows its account's live pass.
#[tokio::test]
async fn trouble_in_a_deferred_import_still_allows_live_mail() {
    let mut b = mailbox(&[("old", 24 * MIB + MIB / 2), ("new", 1)]);
    b.failure = Some(("messages/old".into(), 503, json!({"error": {"message": "unavailable"}}), String::new()));
    b.history = vec![added(2, "new")];
    let mut s = state("a", &["old"]);
    s["first"] = json!("queue");
    let mut f = Fixture::new(vec![("a", b)], vec![s]).await;
    let (batch, _) = f.run().await;
    assert_eq!(messages(&batch), ["a:new"]);
    assert_eq!(f.state("a")["queue"], json!(["old"]));
    assert_eq!(f.state("a")["first"], "queue");
    assert_eq!(f.google.lock().unwrap()["a"].calls.iter().filter(|(p, _, _)| p == "messages/old").count(), 1);
}

/// A queue saved by a previous run gets current labels, deletions and tell.
#[tokio::test]
async fn waiting_new_messages_get_current_labels_and_keep_their_tell() {
    let mut b = mailbox(&[("waiting", 1)]);
    b.history = vec![
        json!({"id": "2", "labelsRemoved": [{"message": {"id": "waiting", "labelIds": ["OLD"]}}]}),
        json!({"id": "3", "messagesDeleted": [{"message": {"id": "gone"}}]}),
    ];
    let mut s = state("a", &[]);
    s["phase"] = json!("live");
    s["fetch_queue"] = json!([{"id": "waiting", "tell": true}, {"id": "gone", "tell": true}]);
    let mut f = Fixture::new(vec![("a", b)], vec![s]).await;
    let (batch, _) = f.run().await;
    assert_eq!(messages(&batch), ["a:waiting"]);
    assert_eq!(batch["items"].as_array().unwrap().len(), 1);
    assert_eq!(batch["items"][0]["attributes"]["labels"], json!(["INBOX"]));
    assert_eq!(batch["items"][0]["attributes"]["tell"], true);
    assert_eq!(batch["deleted"], json!([{"external_id": "a:gone"}]));
    assert!(f.state("a")["fetch_queue"].as_array().unwrap().is_empty());
    assert!(f.google.lock().unwrap()["a"].calls.iter().all(|(p, _, _)| p != "messages/gone"));
}

/// The waiting record is bounded too; a history record resumes after draining.
#[tokio::test]
async fn a_large_history_addition_drains_its_bounded_queue_and_resumes() {
    let mut b = Mailbox::default();
    let ids: Vec<_> = (0..1001).map(|n| format!("m{n:04}")).collect();
    for id in &ids {
        b.messages.insert(id.clone(), 1);
    }
    b.history = vec![
        json!({"id": "2", "messagesAdded": ids.iter().map(|id| json!({"message": {"id": id, "labelIds": ["INBOX"]}})).collect::<Vec<_>>()}),
    ];
    let mut s = state("a", &[]);
    s["phase"] = json!("live");
    let mut f = Fixture::new(vec![("a", b)], vec![s]).await;
    let (batch, _) = f.run().await;
    let mut kept = messages(&batch);
    assert_eq!(kept.len(), 1000);
    assert_eq!(f.state("a")["cursor"], "1");
    assert_eq!(f.state("a")["history_offset"], 1000);
    assert_eq!(batch["again"], true);
    let (batch, _) = f.run().await;
    kept.extend(messages(&batch));
    assert_eq!(kept, ids.iter().map(|id| format!("a:{id}")).collect::<Vec<_>>());
    assert_eq!(f.state("a")["cursor"], "2");
    assert_eq!(batch["again"], false);
}

/// Another account's immediate follow-up never overrides a quota wait.
#[tokio::test]
async fn another_accounts_again_does_not_bypass_a_quota_wait() {
    let a = mailbox(&[("old", 1)]);
    let b = mailbox(&[("b1", 16 * MIB), ("b2", 16 * MIB)]);
    let mut s = state("a", &["old"]);
    s["retry_at"] = json!(i64::MAX);
    s["retry_count"] = json!(2);
    s["error"] = json!("a@example.test: waiting for Google's quota");
    let mut f = Fixture::new(vec![("a", a), ("b", b)], vec![s, state("b", &["b1", "b2"])]).await;
    let (batch, _) = f.run().await;
    assert_eq!(messages(&batch), ["b:b1"]);
    assert_eq!(batch["again"], true);
    let (batch, _) = f.run().await;
    assert_eq!(messages(&batch), ["b:b2"]);
    assert_eq!(batch["again"], false);
    assert!(f.google.lock().unwrap()["a"].calls.is_empty());
    assert_eq!(f.state("a")["queue"], json!(["old"]));
    assert_eq!(f.state("a")["retry_count"], 2);
}
