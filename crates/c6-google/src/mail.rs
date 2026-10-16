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

/// Fetches a message in full: an item with its bytes. Its size (0: gone).
async fn fetch(r: &mut Run, a: &Account, id: &str, tell: bool) -> anyhow::Result<usize> {
    let v = match r.get(a, &format!("/gmail/v1/users/me/messages/{}", enc(id)), &[("format", "raw".into())]).await {
        Ok(v) => v,
        Err(e) if status_of(&e) == Some(404) => return Ok(0),
        Err(e) => return Err(e),
    };
    let raw = b64url_decode(v["raw"].as_str().unwrap_or_default())?;
    let n = raw.len();
    let file = format!("msg/{}.eml", enc(&external(a, id)));
    r.files.insert(file.clone(), raw);
    let date: Option<i64> = v["internalDate"].as_str().and_then(|d| d.parse().ok());
    r.items.push(json!({
        "type": "message/rfc822", "file": file, "external_id": external(a, id),
        "attributes": {"account": a.id, "message": id, "thread": v["threadId"].as_str().unwrap_or(id), "labels": labels_of(&v),
                       "received": date, "tell": tell},
    }));
    Ok(n)
}

/// A message's labels changed: the same item, without its bytes.
fn relabel(r: &mut Run, a: &Account, id: &str, labels: Vec<String>) {
    r.items.push(
        json!({"type": "message/rfc822", "external_id": external(a, id), "attributes": {"account": a.id, "message": id, "labels": labels}}),
    );
}

/// Gmail's labels for an account (names for the user's own).
async fn labels(r: &mut Run, a: &Account) -> anyhow::Result<()> {
    let v = r.get(a, "/gmail/v1/users/me/labels", &[]).await?;
    let list: Vec<Value> = v["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| json!({"id": l["id"], "name": l["name"], "type": l["type"].as_str().unwrap_or("user")}))
        .collect();
    r.json_item("application/vnd.c6.mail-labels+json", &format!("labels:{}", a.id), json!({"account": a.id, "labels": list}));
    Ok(())
}

/// One round of every account's sync: what changed since the last one,
/// then (while importing) more of the history.
pub async fn sync(r: &mut Run) -> anyhow::Result<()> {
    let mut budget = BUDGET;
    for a in r.accounts.clone() {
        let key = format!("mail:{}", a.id);
        let mut st = r.sync(&key);
        let error = one(r, &a, &mut st, &mut budget).await.err().map(|e| format!("{e:#}"));
        if let Some(e) = &error {
            r.errors.push(format!("{}: mail: {e}", a.email));
        }
        r.again |= st["phase"] == "import" && error.is_none();
        r.save(&key, st, error);
    }
    Ok(())
}

async fn one(r: &mut Run, a: &Account, st: &mut Value, budget: &mut usize) -> anyhow::Result<()> {
    if st["cursor"].as_str().is_none() {
        let profile = r.get(a, "/gmail/v1/users/me/profile", &[]).await?;
        let cursor = profile["historyId"].as_str().map(str::to_owned).or_else(|| profile["historyId"].as_u64().map(|n| n.to_string()));
        st["cursor"] = json!(cursor);
        st["phase"] = json!("import");
        labels(r, a).await?;
    }
    // Changes since the cursor.
    let mut page: Option<String> = None;
    loop {
        let mut q = vec![("startHistoryId", st["cursor"].as_str().unwrap_or_default().to_owned())];
        for t in ["messageAdded", "messageDeleted", "labelAdded", "labelRemoved"] {
            q.push(("historyTypes", t.into()));
        }
        if let Some(p) = &page {
            q.push(("pageToken", p.clone()));
        }
        let h = match r.get(a, "/gmail/v1/users/me/history", &q).await {
            Ok(h) => h,
            Err(e) if status_of(&e) == Some(404) => {
                // Too old: import again (what is kept already stays).
                *st = json!({"phase": "import", "done": 0});
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let live = st["phase"] == "live";
        for rec in h["history"].as_array().into_iter().flatten() {
            for added in rec["messagesAdded"].as_array().into_iter().flatten() {
                let msg = &added["message"];
                let id = msg["id"].as_str().unwrap_or_default();
                // A new message in the inbox is told about once it is kept.
                let tell = live && labels_of(msg).iter().any(|l| l == "INBOX");
                let n = fetch(r, a, id, tell).await?;
                *budget = budget.saturating_sub(n);
            }
            for d in rec["messagesDeleted"].as_array().into_iter().flatten() {
                r.deleted.push(json!({"external_id": external(a, d["message"]["id"].as_str().unwrap_or_default())}));
            }
            for k in ["labelsAdded", "labelsRemoved"] {
                for c in rec[k].as_array().into_iter().flatten() {
                    let msg = &c["message"];
                    relabel(r, a, msg["id"].as_str().unwrap_or_default(), labels_of(msg));
                }
            }
        }
        if let Some(id) = h["historyId"].as_str().map(str::to_owned).or_else(|| h["historyId"].as_u64().map(|n| n.to_string())) {
            st["cursor"] = json!(id);
        }
        page = h["nextPageToken"].as_str().map(str::to_owned);
        if page.is_none() {
            break;
        }
    }
    // History import: a page at a time, newest first; what the budget
    // leaves of a page waits in `queue`.
    if st["phase"] == "import" {
        let mut queue: Vec<String> = serde_json::from_value(st["queue"].clone()).unwrap_or_default();
        if queue.is_empty() {
            let mut q = vec![("maxResults", IMPORT_PAGE.to_string()), ("includeSpamTrash", "true".into())];
            if let Some(p) = st["page_token"].as_str() {
                q.push(("pageToken", p.to_owned()));
            }
            let list = r.get(a, "/gmail/v1/users/me/messages", &q).await?;
            queue = list["messages"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect();
            st["page_token"] = json!(list["nextPageToken"].as_str());
        }
        while *budget > 0
            && let Some(id) = queue.first().cloned()
        {
            match fetch(r, a, &id, false).await {
                Ok(n) => {
                    *budget = budget.saturating_sub(n);
                    st["done"] = json!(st["done"].as_i64().unwrap_or(0) + 1);
                }
                Err(e) => r.errors.push(format!("{}: message {id} not imported: {e:#}", a.email)),
            }
            queue.remove(0);
        }
        st["queue"] = json!(queue);
        if queue.is_empty() && st["page_token"].is_null() {
            st["phase"] = json!("live");
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
    fetch(r, &a, &id, false).await?;
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
