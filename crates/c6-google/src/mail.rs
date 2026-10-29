//! Gmail: history import (newest first, resumable within a page), then
//! incremental sync from the history id; label changes, sending and drafts.
//! Messages go to the knowledge store as `message/rfc822`, with Gmail's
//! ids and labels as attributes; a label change is the same item without
//! its bytes.

use base64::Engine;
use serde_json::{Value, json};

use crate::{Account, Run, enc, status_of};

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
/// Message ids listed per import page.
const IMPORT_PAGE: u32 = 100;
/// The messages' bytes a run brings at most (the rest wait for the next).
const BUDGET: usize = 24 * 1024 * 1024;
/// All files in out/, below MAX_JOB_OUTPUT (44 MiB) in
/// crates/c6-worker/src/rpc.rs. The kit builds without the worker crate.
pub const MAX_OUTPUT: usize = 43 * 1024 * 1024;
/// MAIL_LIMIT in crates/c6-memex/src/ingest.rs. The kit builds alone.
pub const MAIL_LIMIT: usize = 25 * 1024 * 1024;
/// The check of kept labels an account has had (`checked`): before google
/// 1.2, a label change the memex got while the message still waited to be
/// read was lost (mail moved to spam stayed in the inbox), so the last 30
/// days' labels are read again, once.
const CHECK: i64 = 1;
/// Messages whose labels a run reads at most, while checking.
const CHECK_RUN: usize = 250;

fn b64url_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    // Gmail uses URL-safe base64, sometimes padded.
    Ok(B64URL.decode(s.trim_end_matches('=').as_bytes())?)
}

fn labels_of(msg: &Value) -> Vec<String> {
    msg["labelIds"].as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

/// The item's id: account and Gmail id (Gmail's ids are per mailbox).
fn external(a: &Account, id: &str) -> String {
    format!("{}:{id}", a.id)
}

/// Whether a message fits the share, or takes a run of its own.
fn fits(so_far: usize, size: usize) -> bool {
    so_far == 0 || size <= BUDGET.saturating_sub(so_far)
}

/// One step, kept together with the position it advances.
struct Part {
    state: Value,
    items: Vec<Value>,
    deleted: Vec<Value>,
    file: Option<(String, Vec<u8>)>,
    errors: Vec<String>,
}

impl Part {
    fn new(st: &Value) -> Self {
        Self { state: st.clone(), items: Vec::new(), deleted: Vec::new(), file: None, errors: Vec::new() }
    }

    fn bytes(&self) -> usize {
        self.items.iter().chain(&self.deleted).map(|v| v.to_string().len() + 1).sum::<usize>()
            + self.errors.iter().map(|e| json!(e).to_string().len() + 1).sum::<usize>()
            + self.file.as_ref().map_or(0, |(_, b)| b.len())
    }

    fn append(self, r: &mut Run) {
        r.items.extend(self.items);
        r.deleted.extend(self.deleted);
        r.errors.extend(self.errors);
        if let Some((name, bytes)) = self.file {
            r.files.insert(name, bytes);
        }
    }
}

/// Counts JSON too, including all accounts' records. The reserve covers
/// each account's final error, back-off and scalar bookkeeping fields.
struct Space {
    bytes: usize,
    messages: usize,
    stopped: bool,
}

impl Space {
    fn records(r: &Run, st: &Value) -> usize {
        r.records
            .values()
            .map(|s| {
                let s = if s["key"] == st["key"] { st } else { s };
                json!({"collection": r.collection, "key": "key", "fields": s}).to_string().len() + 8192
            })
            .sum()
    }

    fn alone(r: &Run, p: &Part) -> usize {
        128 + Self::records(r, &p.state) + p.bytes()
    }

    fn keep(&mut self, r: &mut Run, st: &mut Value, p: Part) -> bool {
        let bytes = p.bytes();
        if self.bytes + Self::records(r, &p.state) + bytes > MAX_OUTPUT {
            return false;
        }
        self.bytes += bytes;
        self.messages += p.file.as_ref().map_or(0, |(_, b)| b.len());
        *st = p.state.clone();
        r.records.insert(st["key"].as_str().unwrap_or_default().to_owned(), st.clone());
        p.append(r);
        true
    }
}

/// Fetches once; only a message too big to return needs its MIME tree.
async fn fetch(r: &Run, a: &Account, st: &Value, id: &str, tell: bool) -> anyhow::Result<Part> {
    let file = format!("msg/{}.eml", enc(&external(a, id)));
    // History and the import page can name the same message this run.
    if r.files.contains_key(&file) {
        return Ok(Part::new(st));
    }
    let v = match r.get(a, &format!("/gmail/v1/users/me/messages/{}", enc(id)), &[("format", "raw".into())]).await {
        Ok(v) => v,
        Err(e) if status_of(&e) == Some(404) => return Ok(Part::new(st)),
        Err(e) => return Err(e),
    };
    let raw = b64url_decode(v["raw"].as_str().unwrap_or_default())?;
    let size = raw.len();
    let date: Option<i64> = v["internalDate"].as_str().and_then(|d| d.parse().ok());
    let mut p = Part::new(st);
    p.items.push(json!({
        "type": "message/rfc822", "file": file, "external_id": external(a, id),
        "attributes": {"account": a.id, "message": id, "thread": v["threadId"].as_str().unwrap_or(id), "labels": labels_of(&v),
                       "received": date, "tell": tell},
    }));
    p.file = Some((file.clone(), raw));
    if size > MAIL_LIMIT || Space::alone(r, &p) > MAX_OUTPUT {
        // Release the raw copy before reading the smaller MIME tree.
        p.file = None;
        drop(v);
        let full = r.get(a, &format!("/gmail/v1/users/me/messages/{}", enc(id)), &[("format", "full".into())]).await?;
        p.file = Some((file, stand_in(&full["payload"], size)));
        p.errors.push(format!("{}: message {id}: too big to keep whole ({size} bytes); kept a stand-in; open it in Gmail", a.email));
    }
    Ok(p)
}

/// Headers, bounded inline text and an attachment list, never attachments.
fn stand_in(payload: &Value, size: usize) -> Vec<u8> {
    fn bounded(s: &str, n: usize) -> &str {
        let mut end = s.len().min(n);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
    fn parts(p: &Value, text: &mut String, files: &mut String) {
        if !p["filename"].as_str().unwrap_or_default().is_empty() || p["body"]["attachmentId"].is_string() {
            if files.len() < 16 * 1024 {
                let name = p["filename"].as_str().filter(|s| !s.is_empty()).unwrap_or("unnamed attachment").replace(['\r', '\n'], " ");
                files.push_str(&format!("- {} ({} bytes)\r\n", bounded(&name, 512), p["body"]["size"].as_u64().unwrap_or(0)));
            }
        } else if matches!(p["mimeType"].as_str(), Some("text/plain" | "text/html"))
            && text.len() < 128 * 1024
            && let Some(data) = p["body"]["data"].as_str()
        {
            // A base64 prefix decodes independently at a multiple of four.
            let n = data.len().min(4 * (128 * 1024 / 3));
            let data = if n < data.len() { &data[..n / 4 * 4] } else { data };
            if let Ok(bytes) = b64url_decode(data) {
                let decoded = String::from_utf8_lossy(&bytes);
                text.push_str(bounded(&decoded, 128 * 1024 - text.len()));
                text.push_str("\r\n");
            }
        }
        for child in p["parts"].as_array().into_iter().flatten() {
            parts(child, text, files);
        }
    }
    let mut headers = String::new();
    let identity = ["from", "to", "cc", "date", "subject", "message-id", "in-reply-to", "references"];
    let mut original: Vec<_> = payload["headers"].as_array().into_iter().flatten().collect();
    original.sort_by_key(|h| !identity.iter().any(|name| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name))));
    for h in original {
        let name = h["name"].as_str().unwrap_or_default();
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || name.to_ascii_lowercase().starts_with("content-")
            || name.eq_ignore_ascii_case("mime-version")
        {
            continue;
        }
        let value = h["value"].as_str().unwrap_or_default().replace(['\r', '\n'], " ");
        let line = format!("{name}: {}\r\n", bounded(&value, 2048));
        if headers.len() + line.len() <= 32 * 1024 {
            headers.push_str(&line);
        }
    }
    let (mut text, mut files) = (String::new(), String::new());
    parts(payload, &mut text, &mut files);
    format!("{headers}MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\
        [This message was too big for carbon6 to keep whole ({size} bytes). This is a bounded copy of its text. Attachments remain in Gmail; open it in Gmail.]\r\n\
        {files}\r\n{text}").into_bytes()
}

/// A message's labels changed: the same item, without its bytes.
fn label_item(a: &Account, id: &str, labels: Vec<String>) -> Value {
    json!({"type": "message/rfc822", "external_id": external(a, id), "attributes": {"account": a.id, "message": id, "labels": labels}})
}

fn relabel(r: &mut Run, a: &Account, id: &str, labels: Vec<String>) {
    r.items.push(label_item(a, id, labels));
}

fn backoff(count: u64) -> i64 {
    (60_000i64 * (1i64 << count.min(5))).min(30 * 60_000)
}

fn failure(a: &Account, st: &mut Value, e: &anyhow::Error, now: i64) -> String {
    let message = if let Some(e) = e.downcast_ref::<crate::ApiError>().filter(|e| e.rate_limit) {
        let count = st["retry_count"].as_u64().unwrap_or(0);
        let until = now.saturating_add(backoff(count)).max(e.retry_at.unwrap_or(0));
        st["retry_at"] = json!(until);
        st["retry_count"] = json!(count.saturating_add(1));
        format!(
            "{}: waiting for Google's quota until {}",
            a.email,
            chrono::DateTime::from_timestamp_millis(until).map(|d| d.to_rfc3339()).unwrap_or_else(|| until.to_string())
        )
    } else {
        format!("{}: mail: {e:#}", a.email)
    };
    message.chars().take(500).collect()
}

fn transient(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::ApiError>().is_some_and(|e| e.rate_limit || e.status == 408 || e.status >= 500)
        || e.downcast_ref::<reqwest::Error>().is_some()
}

fn waiting(st: &Value) -> bool {
    st["phase"] == "import"
        || st["checked"].as_i64() != Some(CHECK)
        || st["history_more"] == true
        || st["fetch_queue"].as_array().is_some_and(|q| !q.is_empty())
}

/// Live work across all accounts, then imports, least recently served first.
/// A message deferred for space gets first turn before either pass.
pub async fn sync(r: &mut Run) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let mut space = Space { bytes: 128, messages: 0, stopped: false };
    let mut accounts = Vec::new();
    for a in r.accounts.clone() {
        let key = format!("mail:{}", a.id);
        let mut st = r.sync(&key);
        st["key"] = json!(key);
        // Records not visited this run stay on the head. Even an unusually
        // large set of accounts must not make the output too large.
        r.records.insert(key.clone(), st.clone());
        if Space::records(r, &st) + space.bytes > MAX_OUTPUT {
            r.records.remove(&key);
            r.again = true;
            continue;
        }
        let error = (st["retry_at"].as_i64().unwrap_or(0) > now).then(|| st["error"].as_str().unwrap_or_default().to_owned());
        accounts.push((a, st, error));
    }
    let mut turn = accounts.iter().filter_map(|(_, s, _)| s["import_turn"].as_u64()).max().unwrap_or(0);
    // Only a rejected fetch sets `first`, and stops further fetches, so
    // normally there is just one. Keep its queue head across restarts.
    for (a, st, error) in &mut accounts {
        if error.is_none()
            && let Some(queue) = st["first"].as_str().map(str::to_owned)
        {
            if let Err(e) = drain(r, a, st, &mut space, &queue, true).await {
                *error = Some(failure(a, st, &e, chrono::Utc::now().timestamp_millis()));
            }
            r.records.insert(st["key"].as_str().unwrap().to_owned(), st.clone());
        }
    }
    for (a, st, error) in &mut accounts {
        // A deferred import can fail before this pass. Only quota forbids
        // this account's live calls; an import failure must not stop them.
        if error.is_none() || st["retry_at"].as_i64().unwrap_or(0) <= now {
            if let Err(e) = one(r, a, st, &mut space).await {
                *error = Some(failure(a, st, &e, chrono::Utc::now().timestamp_millis()));
            }
            r.records.insert(st["key"].as_str().unwrap().to_owned(), st.clone());
        }
    }
    accounts.sort_by_key(|(_, st, _)| st["import_turn"].as_u64().unwrap_or(0));
    for (a, st, error) in &mut accounts {
        // After expired history, take a fresh cursor before listing again.
        if error.is_none() && !space.stopped && space.messages < BUDGET && st["phase"] == "import" && st["cursor"].is_string() {
            turn = turn.saturating_add(1);
            st["import_turn"] = json!(turn);
            if let Err(e) = import(r, a, st, &mut space).await {
                *error = Some(failure(a, st, &e, chrono::Utc::now().timestamp_millis()));
            }
        }
        if error.is_none()
            && st["phase"] == "live"
            && st["checked"].as_i64() != Some(CHECK)
            && let Err(e) = check(r, a, st, &mut space).await
        {
            *error = Some(failure(a, st, &e, chrono::Utc::now().timestamp_millis()));
        }
        if error.is_none() {
            st["retry_at"] = Value::Null;
            st["retry_count"] = json!(0);
            r.again |= waiting(st);
        } else if st["retry_at"].as_i64().unwrap_or(0) <= now || st["error"] != json!(error) {
            r.errors.push(error.clone().unwrap());
        }
        r.save(&format!("mail:{}", a.id), st.clone(), error.clone());
    }
    // Admission counts every file and JSON entry; this also guards future
    // changes to the batch envelope or the bookkeeping reserve.
    debug_assert!(r.files.values().map(Vec::len).sum::<usize>() + r.batch().to_string().len() <= MAX_OUTPUT);
    Ok(())
}

async fn one(r: &mut Run, a: &Account, st: &mut Value, space: &mut Space) -> anyhow::Result<()> {
    if st["cursor"].as_str().is_none() {
        let profile = r.get(a, "/gmail/v1/users/me/profile", &[]).await?;
        let mut p = Part::new(st);
        p.state["cursor"] = json!(history_id(&profile["historyId"]));
        p.state["phase"] = json!("import");
        let v = r.get(a, "/gmail/v1/users/me/labels", &[]).await?;
        let list: Vec<Value> = v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| json!({"id": l["id"], "name": l["name"], "type": l["type"].as_str().unwrap_or("user")}))
            .collect();
        p.items.push(json!({"type": "application/vnd.c6.mail-labels+json", "external_id": format!("labels:{}", a.id),
            "text": json!({"account": a.id, "labels": list}).to_string()}));
        if !space.keep(r, st, p) {
            return Ok(());
        }
    }
    history(r, a, st, space).await?;
    drain(r, a, st, space, "fetch_queue", false).await
}

fn history_id(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned).or_else(|| v.as_u64().map(|n| n.to_string()))
}

async fn history(r: &mut Run, a: &Account, st: &mut Value, space: &mut Space) -> anyhow::Result<()> {
    let start = st["cursor"].as_str().unwrap_or_default().to_owned();
    let mut page: Option<String> = None;
    st["history_more"] = json!(true);
    loop {
        let mut q = vec![("startHistoryId", start.clone()), ("maxResults", "100".into())];
        if let Some(p) = &page {
            q.push(("pageToken", p.clone()));
        }
        let h = match r.get(a, "/gmail/v1/users/me/history", &q).await {
            Ok(h) => h,
            Err(e) if status_of(&e) == Some(404) => {
                // Preserve queued new mail, including a deferred head.
                st["cursor"] = Value::Null;
                st["phase"] = json!("import");
                st["done"] = json!(0);
                st.as_object_mut().unwrap().remove("queue");
                st["page_token"] = Value::Null;
                st["history_record"] = Value::Null;
                st["history_offset"] = json!(0);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for rec in h["history"].as_array().into_iter().flatten() {
            let id = history_id(&rec["id"]);
            let offset = if st["history_record"] == json!(id) { st["history_offset"].as_u64().unwrap_or(0) as usize } else { 0 };
            let events = ["messagesAdded", "messagesDeleted", "labelsAdded", "labelsRemoved"]
                .into_iter()
                .flat_map(|k| rec[k].as_array().into_iter().flatten().map(move |v| (k, v)));
            for (n, (kind, event)) in events.enumerate().skip(offset) {
                let msg = &event["message"];
                let mid = msg["id"].as_str().unwrap_or_default();
                let mut p = Part::new(st);
                if !p.state["fetch_queue"].is_array() {
                    p.state["fetch_queue"] = json!([]);
                }
                let queue = p.state["fetch_queue"].as_array_mut().unwrap();
                match kind {
                    "messagesAdded" => {
                        if !queue.iter().any(|m| m["id"] == mid) {
                            // Bound the record as well as the items. Resume
                            // this record after its waiting messages are kept.
                            if queue.len() >= 1000 {
                                return Ok(());
                            }
                            queue.push(json!({"id": mid, "tell": st["phase"] == "live" && labels_of(msg).iter().any(|l| l == "INBOX")}));
                        }
                    }
                    "messagesDeleted" => {
                        queue.retain(|m| m["id"] != mid);
                        if let Some(q) = p.state["queue"].as_array_mut() {
                            q.retain(|m| m != mid);
                        }
                        p.deleted.push(json!({"external_id": external(a, mid)}));
                    }
                    _ if !queue.iter().any(|m| m["id"] == mid) => p.items.push(label_item(a, mid, labels_of(msg))),
                    _ => {}
                }
                p.state["history_record"] = json!(id);
                p.state["history_offset"] = json!(n + 1);
                if !space.keep(r, st, p) {
                    return Ok(());
                }
            }
            if let Some(id) = id {
                st["cursor"] = json!(id);
            }
            st["history_record"] = Value::Null;
            st["history_offset"] = json!(0);
        }
        page = h["nextPageToken"].as_str().map(str::to_owned);
        if page.is_none() {
            if let Some(id) = history_id(&h["historyId"]) {
                st["cursor"] = json!(id);
            }
            st["history_more"] = json!(false);
            return Ok(());
        }
    }
}

async fn drain(r: &mut Run, a: &Account, st: &mut Value, space: &mut Space, queue: &str, first: bool) -> anyhow::Result<()> {
    while !space.stopped && space.messages < BUDGET {
        let Some(head) = st[queue].as_array().and_then(|q| q.first()).cloned() else {
            if st["first"] == queue {
                st["first"] = Value::Null;
            }
            break;
        };
        let id = if queue == "queue" { head.as_str() } else { head["id"].as_str() }.unwrap_or_default();
        let mut p = match fetch(r, a, st, id, head["tell"] == true).await {
            Ok(p) => p,
            Err(e) if queue == "queue" && !transient(&e) => {
                let mut p = Part::new(st);
                p.errors.push(format!(
                    "{}: message {id} not imported: {}",
                    a.email,
                    format!("{e:#}").chars().take(500).collect::<String>()
                ));
                p
            }
            Err(e) => return Err(e),
        };
        p.state[queue].as_array_mut().unwrap().remove(0);
        if queue == "queue" && (p.errors.is_empty() || p.file.is_some()) {
            p.state["done"] = json!(st["done"].as_u64().unwrap_or(0) + 1);
        }
        if st["first"] == queue {
            p.state["first"] = Value::Null;
        }
        let size = p.file.as_ref().map_or(0, |(_, b)| b.len());
        if !fits(space.messages, size) || !space.keep(r, st, p) {
            st["first"] = json!(queue);
            space.stopped = true;
            break;
        }
        if first {
            break;
        }
    }
    Ok(())
}

async fn import(r: &mut Run, a: &Account, st: &mut Value, space: &mut Space) -> anyhow::Result<()> {
    if st["queue"].as_array().is_none_or(|q| q.is_empty()) {
        // An existing empty queue with no next page is already complete.
        if st["queue"].is_array() && st["page_token"].is_null() {
            st["phase"] = json!("live");
            return Ok(());
        }
        let mut q = vec![("maxResults", IMPORT_PAGE.to_string()), ("includeSpamTrash", "true".into())];
        if let Some(p) = st["page_token"].as_str() {
            q.push(("pageToken", p.to_owned()));
        }
        let list = r.get(a, "/gmail/v1/users/me/messages", &q).await?;
        let mut p = Part::new(st);
        p.state["queue"] = json!(list["messages"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str()).collect::<Vec<_>>());
        p.state["page_token"] = json!(list["nextPageToken"].as_str());
        if !space.keep(r, st, p) {
            return Ok(());
        }
    }
    drain(r, a, st, space, "queue", false).await?;
    if st["queue"].as_array().is_some_and(|q| q.is_empty()) && st["page_token"].is_null() {
        st["phase"] = json!("live");
    }
    Ok(())
}

/// Reads the last 30 days' labels again (`CHECK`), a part a run.
async fn check(r: &mut Run, a: &Account, st: &mut Value, space: &mut Space) -> anyhow::Result<()> {
    for _ in 0..CHECK_RUN {
        let Some(id) = st["check_queue"].as_array().and_then(|q| q.first()).and_then(Value::as_str).map(str::to_owned) else {
            if st["check_queue"].is_array() && st["check_page"].is_null() {
                st["checked"] = json!(CHECK);
                return Ok(());
            }
            let mut q = vec![("maxResults", "500".to_owned()), ("includeSpamTrash", "true".into()), ("q", "newer_than:30d".into())];
            if let Some(p) = st["check_page"].as_str() {
                q.push(("pageToken", p.to_owned()));
            }
            let list = r.get(a, "/gmail/v1/users/me/messages", &q).await?;
            let mut p = Part::new(st);
            p.state["check_queue"] = json!(list["messages"].as_array().into_iter().flatten().map(|m| m["id"].clone()).collect::<Vec<_>>());
            p.state["check_page"] = json!(list["nextPageToken"].as_str());
            if !space.keep(r, st, p) {
                return Ok(());
            }
            continue;
        };
        let mut p = Part::new(st);
        // A pending fetch brings its current labels; a bare relabel would
        // be refused by the memex until it has the message.
        if !st["fetch_queue"].as_array().into_iter().flatten().any(|m| m["id"] == id) {
            match r.get(a, &format!("/gmail/v1/users/me/messages/{}", enc(&id)), &[("format", "minimal".into())]).await {
                Ok(v) => p.items.push(label_item(a, &id, labels_of(&v))),
                Err(e) if status_of(&e) == Some(404) => {}
                Err(e) => return Err(e),
            }
        }
        p.state["check_queue"].as_array_mut().unwrap().remove(0);
        if !space.keep(r, st, p) {
            return Ok(());
        }
    }
    Ok(())
}

/// `mail-modify`: adds and removes labels on a thread (read and unread,
/// archive, star, trash); the messages' new labels are kept.
pub async fn modify(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(r.input["account"].as_str().unwrap_or_default())?;
    let thread = r.input["thread"].as_str().unwrap_or_default().to_owned();
    anyhow::ensure!(!thread.is_empty(), "which thread?");
    let body = json!({"addLabelIds": r.input["add"], "removeLabelIds": r.input["remove"]});
    let t = r.call(&a, reqwest::Method::POST, &format!("/gmail/v1/users/me/threads/{}/modify", enc(&thread)), &[], Some(&body)).await?;
    for m in t["messages"].as_array().into_iter().flatten() {
        relabel(r, &a, m["id"].as_str().unwrap_or_default(), labels_of(m));
    }
    r.result = json!({"thread": thread});
    Ok(())
}

/// `mail-send`: sends a message (RFC 822, base64url in `raw`), in a thread
/// if it continues one; the sent message is kept.
pub async fn send(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(r.input["account"].as_str().unwrap_or_default())?;
    let mut message = json!({"raw": r.input["raw"]});
    if let Some(t) = r.input["thread"].as_str().filter(|t| !t.is_empty()) {
        message["threadId"] = json!(t);
    }
    let sent = r.call(&a, reqwest::Method::POST, "/gmail/v1/users/me/messages/send", &[], Some(&message)).await?;
    let id = sent["id"].as_str().unwrap_or_default().to_owned();
    fetch(r, &a, &json!({}), &id, false).await?.append(r);
    r.result = json!({"id": id, "thread": sent["threadId"]});
    Ok(())
}

/// `mail-draft`: saves a draft (new, or replacing `draft_id`).
pub async fn draft(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(r.input["account"].as_str().unwrap_or_default())?;
    let mut message = json!({"raw": r.input["raw"]});
    if let Some(t) = r.input["thread"].as_str().filter(|t| !t.is_empty()) {
        message["threadId"] = json!(t);
    }
    let v = match r.input["draft_id"].as_str().filter(|d| !d.is_empty()) {
        Some(d) => {
            r.call(&a, reqwest::Method::PUT, &format!("/gmail/v1/users/me/drafts/{}", enc(d)), &[], Some(&json!({"message": message})))
                .await?
        }
        None => r.call(&a, reqwest::Method::POST, "/gmail/v1/users/me/drafts", &[], Some(&json!({"message": message}))).await?,
    };
    r.result = json!({"draft_id": v["id"]});
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiError;

    /// A message bigger than the share gets a run of its own.
    #[test]
    fn a_big_message_does_not_hold_up_the_queue() {
        assert!(fits(0, (BUDGET + MAIL_LIMIT) / 2));
        assert!(!fits(1, (BUDGET + MAIL_LIMIT) / 2));
        assert!(fits(BUDGET - 10, 10));
        assert!(!fits(BUDGET - 10, 11));
    }

    /// Quota reasons and Google's older messages agree; permission does not.
    #[test]
    fn google_quota_errors_are_not_permission_errors() {
        let quota = json!({"error": {"message": "Quota exceeded for quota metric 'Total Query Cost' and limit 'Units per minute per user'",
            "errors": [{"reason": "rateLimitExceeded"}]}});
        assert!(ApiError::response(403, &quota.to_string(), None, 0).rate_limit);
        assert!(ApiError::response(429, "too many requests", None, 0).rate_limit);
        for reason in ["rateLimitExceeded", "userRateLimitExceeded", "quotaExceeded", "dailyLimitExceeded"] {
            assert!(ApiError::response(403, &json!({"error": {"errors": [{"reason": reason}]}}).to_string(), None, 0).rate_limit);
        }
        for error in [json!({"details": [{"reason": "RATE_LIMIT_EXCEEDED"}]}), json!({"status": "RESOURCE_EXHAUSTED"})] {
            assert!(ApiError::response(403, &json!({"error": error}).to_string(), None, 0).rate_limit);
        }
        for message in ["Quota exceeded for user", "Rate Limit Exceeded", "User-rate limit exceeded"] {
            assert!(ApiError::response(403, &json!({"error": {"message": message}}).to_string(), None, 0).rate_limit);
        }
        let permission =
            ApiError::response(403, r#"{"error":{"message":"Insufficient Permission","errors":[{"reason":"forbidden"}]}}"#, None, 0);
        assert!(!permission.rate_limit);
        assert_eq!(status_of(&permission.into()), Some(403));
    }

    /// Back-off doubles to thirty minutes and respects either Retry-After form.
    #[test]
    fn quota_waits_double_and_honour_retry_after() {
        assert_eq!((0..8).map(backoff).collect::<Vec<_>>(), [60_000, 120_000, 240_000, 480_000, 960_000, 1_800_000, 1_800_000, 1_800_000]);
        let now = 1_000_000;
        let a = Account { id: "a".into(), email: "a@example.test".into(), token: String::new() };
        let mut st = json!({});
        let e = ApiError::response(429, "quota", Some("240"), now);
        let error = failure(&a, &mut st, &e.into(), now);
        assert_eq!(st["retry_at"], now + 240_000);
        assert_eq!(st["retry_count"], 1);
        assert!(error.contains("waiting for Google's quota until"));
        failure(&a, &mut st, &ApiError::response(429, "quota", None, now).into(), now);
        assert_eq!(st["retry_at"], now + 120_000);
        assert_eq!(
            ApiError::response(429, "quota", Some("Wed, 28 Oct 2026 12:00:00 GMT"), now).retry_at,
            Some(chrono::DateTime::parse_from_rfc3339("2026-10-28T12:00:00Z").unwrap().timestamp_millis())
        );
    }

    /// The stand-in is readable mail with its identity and bounded text.
    #[test]
    fn a_stand_in_keeps_headers_text_and_attachment_names() {
        let payload = json!({"headers": [
            {"name": "From", "value": "Sender <sender@example.test>"},
            {"name": "To", "value": "Reader <reader@example.test>"},
            {"name": "Cc", "value": "copy@example.test"},
            {"name": "Date", "value": "Wed, 28 Oct 2026 12:00:00 +0000"},
            {"name": "Subject", "value": "Invented large message"},
            {"name": "Message-ID", "value": "<large@example.test>"},
            {"name": "In-Reply-To", "value": "<parent@example.test>"},
            {"name": "References", "value": "<parent@example.test>"},
            {"name": "Content-Type", "value": "multipart/mixed; boundary=old"}],
            "parts": [{"mimeType": "text/plain", "body": {"data": B64URL.encode("Readable text.\n".repeat(30_000))}},
                {"filename": "invented.pdf", "body": {"attachmentId": "no-download", "size": 50_000_000}}]});
        let bytes = stand_in(&payload, 50_000_123);
        assert!(bytes.len() < 192 * 1024);
        let text = String::from_utf8(bytes).unwrap();
        for s in [
            "Subject: Invented large message",
            "Message-ID: <large@example.test>",
            "In-Reply-To: <parent@example.test>",
            "References: <parent@example.test>",
            "Readable text.",
            "invented.pdf (50000000 bytes)",
            "50000123 bytes",
            "open it in Gmail",
        ] {
            assert!(text.contains(s), "missing {s}");
        }
        assert!(!text.contains("multipart/mixed"));
        let hostile = json!({"headers": [{"name": "Subject", "value": "é".repeat(100_000)}],
            "parts": (0..2000).map(|_| json!({"filename": "x".repeat(2000), "body": {"size": 1}})).collect::<Vec<_>>()});
        assert!(stand_in(&hostile, 50_000_000).len() < 192 * 1024);
    }
}
