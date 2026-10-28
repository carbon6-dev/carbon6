//! The mail store behind the mail client (threads, views, labels): kept
//! from what mail apps sync (their accounts' messages, with the mailbox's
//! labels) and what is imported; changes, drafts and sending go through
//! the app that owns the account (docs/knowledge.md §6), sending held a few
//! seconds so it can be undone (or until a time you choose).

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::Memex;
use crate::people;
use crate::store::clip;

/// How long a sent message waits, so it can be undone.
pub const UNDO_MS: i64 = 10_000;

/// A mirrored message, read (an app's sync gave it): the item (its text
/// searchable), the mail row, its labels, and links to the people it is
/// from and to; told about if it came to the inbox while live.
pub async fn store_message(m: &Memex, job: &crate::extract::Done<'_>, parsed: &Parsed) -> anyhow::Result<String> {
    let p = job.params;
    let a = &p["attributes"];
    let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let (source, external_id) = (s(&p["source"]), s(&p["external_id"]));
    let (account, message) = (s(&a["account"]), s(&a["message"]));
    let thread = a["thread"].as_str().filter(|t| !t.is_empty()).map(str::to_owned).unwrap_or_else(|| message.clone());
    let labels: Vec<String> = serde_json::from_value(a["labels"].clone()).unwrap_or_default();
    let date = a["received"].as_i64().unwrap_or_else(c6_core::now_ms);
    let owner = m.accounts().into_iter().find(|x| x.id == account).map(|x| x.email).unwrap_or_else(|| account.clone());
    let rfc_id = parsed.message_id.clone();
    // Imported earlier and now synced: the item becomes the synced one and
    // keeps its id, so links and notes made meanwhile stay.
    if !rfc_id.is_empty() {
        let twin: Option<String> = sqlx::query_scalar("SELECT item_id FROM mail WHERE rfc_id = ? AND account LIKE 'import:%' LIMIT 1")
            .bind(&rfc_id)
            .fetch_optional(&m.store.pool)
            .await?;
        if let Some(id) = twin {
            let mut tx = m.store.pool.begin_with("BEGIN IMMEDIATE").await?;
            sqlx::query("UPDATE items SET source = ?, external_id = ? WHERE id = ?")
                .bind(&source)
                .bind(&external_id)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE mail SET account = ?, gmail_id = ?, thread_id = ? WHERE item_id = ?")
                .bind(&account)
                .bind(&message)
                .bind(&thread)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
    }
    let to = Stored {
        account: &account,
        external_id: &external_id,
        thread_id: &thread,
        source: &source,
        rfc_id: &rfc_id,
        origin: "mirrored",
        owner: &owner,
        message: &message,
    };
    let blob = job.input.strip_prefix("head:").unwrap_or_default();
    let item = store(m, parsed, blob, &to, &labels, date).await?;
    if a["tell"].as_bool() == Some(true) {
        let r = sqlx::query("SELECT from_addr, from_name, to_json, subject, snippet, thread_id FROM mail WHERE item_id = ?")
            .bind(&item)
            .fetch_one(&m.store.pool)
            .await?;
        let to: Vec<(String, String)> = serde_json::from_str(&r.get::<String, _>("to_json")).unwrap_or_default();
        let data = json!({
            "account": owner, "item_id": item, "thread_id": r.get::<String, _>("thread_id"),
            "from": format!("{} <{}>", r.get::<String, _>("from_name"), r.get::<String, _>("from_addr")),
            "to": to.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(", "),
            "subject": r.get::<String, _>("subject"), "snippet": r.get::<String, _>("snippet"),
        });
        m.emit("mail.received", data).await;
    }
    Ok(item)
}

/// A message as c6-extract read it.
#[derive(Default)]
pub struct Parsed {
    pub message_id: String,
    pub from: (String, String),
    pub to: Vec<(String, String)>,
    pub cc: Vec<(String, String)>,
    pub subject: String,
    pub text: String,
    pub html: Option<Vec<u8>>,
    pub list: bool,
    pub attachments: Vec<Value>,
}

impl Parsed {
    pub fn of(v: &Value, files: &std::collections::BTreeMap<String, Vec<u8>>) -> Self {
        let pairs = |k: &str| -> Vec<(String, String)> { serde_json::from_value(v[k].clone()).unwrap_or_default() };
        Self {
            message_id: v["message_id"].as_str().unwrap_or_default().to_owned(),
            from: serde_json::from_value(v["from"].clone()).unwrap_or_default(),
            to: pairs("to"),
            cc: pairs("cc"),
            subject: v["subject"].as_str().unwrap_or_default().trim().to_owned(),
            text: v["text"].as_str().unwrap_or_default().to_owned(),
            html: files.get("html").cloned(),
            list: v["list"].as_bool().unwrap_or(false),
            attachments: v["attachments"].as_array().cloned().unwrap_or_default(),
        }
    }
}

/// Mail kept before messages were read once (its text, HTML and
/// attachments' names): read again, a few at a time.
pub async fn backfill(m: &Memex) -> anyhow::Result<usize> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT item_id, blob FROM mail WHERE parsed = 0 AND ('reparse:' || item_id) NOT IN (SELECT key FROM extractions) LIMIT 100",
    )
    .fetch_all(&m.store.pool)
    .await?;
    for (item, blob) in &rows {
        crate::extract::queue(m, &format!("reparse:{item}"), "mail", &format!("head:{blob}"), "", json!({"kind": "reparse", "item": item}))
            .await?;
    }
    Ok(rows.len())
}

/// Keeps what reading an older message again found.
pub async fn reparsed(m: &Memex, job: &crate::extract::Done<'_>, parsed: &Parsed) -> anyhow::Result<String> {
    let item = job.params["item"].as_str().unwrap_or_default().to_owned();
    let (text_blob, html_blob) = parts_of(m, parsed).await?;
    sqlx::query("UPDATE mail SET text_blob = ?, html_blob = ?, parts = ?, attachments = ?, parsed = 1 WHERE item_id = ?")
        .bind(&text_blob)
        .bind(&html_blob)
        .bind(json!(parsed.attachments).to_string())
        .bind(parsed.attachments.len() as i64)
        .bind(&item)
        .execute(&m.store.pool)
        .await?;
    Ok(item)
}

async fn parts_of(m: &Memex, parsed: &Parsed) -> anyhow::Result<(String, Option<String>)> {
    let text = m.put_blob(parsed.text.as_bytes()).await?;
    let html = match &parsed.html {
        Some(h) => Some(m.put_blob(h).await?),
        None => None,
    };
    Ok((text, html))
}

/// Where a message is kept: its mail account (an app's account id, or
/// `import:…` for imported mail) and thread.
pub struct Stored<'a> {
    pub account: &'a str,
    pub external_id: &'a str,
    pub thread_id: &'a str,
    /// The item's source.
    pub source: &'a str,
    pub rfc_id: &'a str,
    /// mirrored (synced both ways) or imported (here only).
    pub origin: &'a str,
    /// The account's address, or the source for imported mail.
    pub owner: &'a str,
    /// The mailbox's id for it (empty for imported mail).
    pub message: &'a str,
}

/// Stores a read message: the item (searchable text, the people as
/// links) and its row in the mail store. `blob`: the raw message's.
pub async fn store(m: &Memex, msg: &Parsed, blob: &str, at: &Stored<'_>, labels: &[String], date: i64) -> anyhow::Result<String> {
    let from = msg.from.clone();
    let (to, cc) = (msg.to.clone(), msg.cc.clone());
    let subject = msg.subject.clone();
    let body = msg.text.clone();
    let attachments = &msg.attachments;
    let list = msg.list || from.0.contains("noreply") || from.0.contains("no-reply");
    let (text_blob, html_blob) = parts_of(m, msg).await?;
    let spam = labels.iter().any(|l| l == "SPAM");
    let who = |v: &[(String, String)]| {
        v.iter().map(|(a, n)| if n.is_empty() { a.clone() } else { format!("{n} <{a}>") }).collect::<Vec<_>>().join(", ")
    };
    let item = api::MemexItemIn {
        kind: if spam { "spam" } else { "email" }.into(),
        source: at.source.into(),
        external_id: at.external_id.into(),
        title: if subject.is_empty() { "(no subject)".into() } else { subject.clone() },
        text: format!(
            "From: {}\nTo: {}\n{}\n{}",
            who(std::slice::from_ref(&from)),
            who(&to),
            if cc.is_empty() { String::new() } else { format!("Cc: {}\n", who(&cc)) },
            clip(&body, 100_000)
        ),
        created_at: Some(date),
        meta: json!({"account": at.owner, "thread_id": at.thread_id, "from": from.0, "attachments": attachments.len(), "origin": at.origin}),
        ..Default::default()
    };
    let item_id = m.store.put(&item, &item.source).await?;
    let has = |l: &str| labels.iter().any(|x| x == l) as i64;
    sqlx::query(
        "INSERT INTO mail (item_id, account, gmail_id, thread_id, rfc_id, date, from_addr, from_name, to_json, cc_json, subject, snippet, labels, \
         unread, starred, inbox, spam, trash, sent, draft, list, attachments, blob, text_blob, html_blob, parts, parsed) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1) \
         ON CONFLICT(item_id) DO UPDATE SET labels = excluded.labels, unread = excluded.unread, starred = excluded.starred, inbox = excluded.inbox, \
         spam = excluded.spam, trash = excluded.trash, sent = excluded.sent, draft = excluded.draft, text_blob = excluded.text_blob, \
         html_blob = excluded.html_blob, parts = excluded.parts, parsed = 1",
    )
    .bind(&item_id)
    .bind(at.account)
    .bind(if at.message.is_empty() { at.external_id } else { at.message })
    .bind(at.thread_id)
    .bind(at.rfc_id)
    .bind(date)
    .bind(&from.0)
    .bind(&from.1)
    .bind(json!(to).to_string())
    .bind(json!(cc).to_string())
    .bind(&subject)
    .bind(clip(&body.split_whitespace().collect::<Vec<_>>().join(" "), 200))
    .bind(json!(labels).to_string())
    .bind(has("UNREAD"))
    .bind(has("STARRED"))
    .bind(has("INBOX"))
    .bind(has("SPAM"))
    .bind(has("TRASH"))
    .bind(has("SENT"))
    .bind(has("DRAFT"))
    .bind(list as i64)
    .bind(attachments.len() as i64)
    .bind(blob)
    .bind(&text_blob)
    .bind(&html_blob)
    .bind(json!(attachments).to_string())
    .execute(&m.store.pool)
    .await?;
    set_labels(m, &item_id, labels).await?;
    // The people, as observed links.
    m.store.clear_links(&item_id, "mail").await?;
    if !from.0.is_empty() {
        let p = people::person(&m.store, &from.0, &from.1).await?;
        m.store.link(&item_id, "from", &p, "observed", "mail", None).await?;
    }
    for (rel, list) in [("to", &to), ("cc", &cc)] {
        for (a, n) in list.iter() {
            let p = people::person(&m.store, a, n).await?;
            m.store.link(&item_id, rel, &p, "observed", "mail", None).await?;
        }
    }
    Ok(item_id)
}

pub async fn set_labels(m: &Memex, item: &str, labels: &[String]) -> anyhow::Result<()> {
    let has = |l: &str| labels.iter().any(|x| x == l) as i64;
    sqlx::query(
        "UPDATE mail SET labels = ?, unread = ?, starred = ?, inbox = ?, spam = ?, trash = ?, sent = ?, draft = ? WHERE item_id = ?",
    )
    .bind(json!(labels).to_string())
    .bind(has("UNREAD"))
    .bind(has("STARRED"))
    .bind(has("INBOX"))
    .bind(has("SPAM"))
    .bind(has("TRASH"))
    .bind(has("SENT"))
    .bind(has("DRAFT"))
    .bind(item)
    .execute(&m.store.pool)
    .await?;
    sqlx::query("DELETE FROM mail_labels WHERE item_id = ?").bind(item).execute(&m.store.pool).await?;
    for l in labels {
        sqlx::query("INSERT OR IGNORE INTO mail_labels (item_id, label) VALUES (?, ?)").bind(item).bind(l).execute(&m.store.pool).await?;
    }
    // Spam stays out of sessions' reach.
    sqlx::query("UPDATE items SET kind = CASE WHEN ? THEN 'spam' ELSE 'email' END WHERE id = ? AND kind IN ('email', 'spam')")
        .bind(has("SPAM") == 1)
        .bind(item)
        .execute(&m.store.pool)
        .await?;
    Ok(())
}

/// Threads in a view, newest first: inbox, needs_reply, waiting, starred,
/// sent, drafts, archive, spam, trash, all, or label:<id>.
pub async fn threads(
    m: &Memex,
    view: &str,
    account: Option<&str>,
    before: Option<i64>,
    limit: u32,
) -> anyhow::Result<Vec<api::MailThread>> {
    let me: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
    let now = c6_core::now_ms();
    let month = now - 30 * 86_400_000;
    // What needs a reply is as recent as what is sorted, so the two agree.
    let recent = now - crate::triage::WINDOW;
    let model = m.understanding().is_some();
    // The view's condition on a thread's messages.
    // As sorted for the brief (docs/brief.md): triage:<category>. What
    // needs you (or is a first contact) is what waits on you there: in the
    // inbox, its request open (not done, dropped, or snoozed till later).
    let sorted = view.strip_prefix("triage:").map(str::to_owned);
    let (cond, label): (&str, Option<String>) = match view {
        "inbox" => ("m.inbox = 1 AND m.trash = 0 AND m.spam = 0", None),
        "starred" => ("m.starred = 1 AND m.trash = 0", None),
        "sent" => ("m.sent = 1 AND m.trash = 0", None),
        "drafts" => ("m.draft = 1 AND m.trash = 0", None),
        "archive" => ("m.inbox = 0 AND m.trash = 0 AND m.spam = 0 AND m.sent = 0 AND m.draft = 0", None),
        "spam" => ("m.spam = 1", None),
        "trash" => ("m.trash = 1", None),
        "all" => ("m.trash = 0 AND m.spam = 0", None),
        // The latest message is from someone else, to you directly, in the
        // inbox, not a list, as recent as what is sorted; and as sorted
        // (`needs_reply`).
        "needs_reply" => ("m.inbox = 1 AND m.trash = 0 AND m.spam = 0 AND m.list = 0", None),
        // The latest message is yours, sent over two days ago, this month.
        "waiting" => ("m.trash = 0 AND m.spam = 0", None),
        v if sorted.is_some() && v.starts_with("triage:") => ("m.trash = 0 AND m.spam = 0", None),
        v => match v.strip_prefix("label:") {
            Some(l) => ("m.trash = 0", Some(l.to_owned())),
            None => anyhow::bail!("no mail view {v:?}"),
        },
    };
    let sql = format!(
        "SELECT m.account AS account, m.thread_id AS thread_id, MAX(m.date) AS date, COUNT(*) AS n, MAX(m.unread) AS unread, MAX(m.starred) AS starred, \
         MAX(m.attachments) AS attachments FROM mail m WHERE {cond} AND (?1 IS NULL OR m.account = ?1) \
         AND (?4 IS NULL OR m.item_id IN (SELECT item_id FROM mail_labels WHERE label = ?4)) \
         AND (?5 IS NULL OR EXISTS (SELECT 1 FROM triage t WHERE t.target = 'thread:' || m.account || ':' || m.thread_id AND t.category = ?5 \
             AND (?5 NOT IN ('needs_you', 'first_contact') OR (m.inbox = 1 AND EXISTS (SELECT 1 FROM request_sources s JOIN requests r \
             ON r.id = s.request_id WHERE s.target = t.target AND (r.status = 'open' OR (r.status = 'snoozed' AND r.until <= ?6))))))) \
         GROUP BY m.account, m.thread_id HAVING (?2 IS NULL OR MAX(m.date) < ?2) AND (?7 IS NULL OR MAX(m.date) > ?7) \
         ORDER BY date DESC LIMIT ?3"
    );
    // The views of recent mail read no further back.
    let since = match view {
        "needs_reply" => Some(recent),
        "waiting" => Some(month),
        _ => None,
    };
    // Over-fetch for the views that look at each thread's latest message.
    let fetch = if matches!(view, "needs_reply" | "waiting") { limit * 5 } else { limit };
    let mut out = Vec::new();
    // Pages go by a thread's latest date (`before`, the last one shown);
    // the filtered views read on until they fill one, or there's no more.
    let mut cursor = before;
    'pages: loop {
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(account)
            .bind(cursor)
            .bind(fetch as i64)
            .bind(&label)
            .bind(&sorted)
            .bind(now)
            .bind(since)
            .fetch_all(&m.store.pool)
            .await?;
        let more = rows.len() as u32 == fetch && fetch > limit;
        for r in rows {
            cursor = Some(r.get("date"));
            let (acct, thread): (String, String) = (r.get("account"), r.get("thread_id"));
            let msgs = sqlx::query("SELECT * FROM mail WHERE account = ? AND thread_id = ? ORDER BY date")
                .bind(&acct)
                .bind(&thread)
                .fetch_all(&m.store.pool)
                .await?;
            let Some(last) = msgs.last() else { continue };
            let last_from: String = last.get("from_addr");
            let mine = me.contains(&last_from);
            let date: i64 = r.get("date");
            let triage = crate::triage::of_thread(m, &acct, &thread).await?;
            let keep = match view {
                "needs_reply" => {
                    let to: Vec<(String, String)> = serde_json::from_str(&last.get::<String, _>("to_json")).unwrap_or_default();
                    !mine
                        && date > recent
                        && to.iter().any(|(a, _)| me.contains(a))
                        && needs_reply(triage.as_ref(), date, &last_from, model)
                }
                // Yours, unanswered: sent to someone else.
                "waiting" => {
                    let to: Vec<(String, String)> = serde_json::from_str(&last.get::<String, _>("to_json")).unwrap_or_default();
                    mine && date < now - 2 * 86_400_000 && date > month && to.iter().any(|(a, _)| !me.contains(a))
                }
                _ => true,
            };
            if !keep {
                continue;
            }
            let mut people: Vec<String> = Vec::new();
            for x in &msgs {
                let n: String = x.get("from_name");
                let a: String = x.get("from_addr");
                let label = if me.contains(&a) {
                    "me".to_owned()
                } else if n.is_empty() {
                    a
                } else {
                    n
                };
                if !people.contains(&label) {
                    people.push(label);
                }
            }
            out.push(api::MailThread {
                triage,
                account: acct,
                thread_id: thread,
                subject: last.get::<String, _>("subject"),
                snippet: last.get("snippet"),
                date,
                count: r.get::<i64, _>("n") as u32,
                unread: r.get::<i64, _>("unread") != 0,
                starred: r.get::<i64, _>("starred") != 0,
                attachments: r.get::<i64, _>("attachments") != 0,
                people,
                labels: serde_json::from_str(&last.get::<String, _>("labels")).unwrap_or_default(),
            });
            if out.len() as u32 >= limit {
                break 'pages;
            }
        }
        if !more {
            break;
        }
    }
    Ok(out)
}

/// A thread's address on its mailbox's web, as the account's app declares
/// it (`links`): none for imported mail, an app without one, or one that
/// isn't https.
fn link(m: &Memex, account: &str, thread: &str, message: &str, message_id: &str) -> Option<api::Link> {
    let a = m.accounts().into_iter().find(|a| a.id == account)?;
    let l = a.links?;
    if !l.thread.starts_with("https://") {
        return None;
    }
    let url = [
        ("{account}", a.email.as_str()),
        ("{thread}", thread),
        ("{message}", message),
        ("{message_id}", message_id.trim_matches(['<', '>'])),
    ]
    .iter()
    .fold(l.thread.clone(), |u, (k, v)| u.replace(k, &encode(v)));
    Some(api::Link { service: l.service, url })
}

/// `s` as part of an address: unreserved characters as they are, the rest
/// percent-encoded.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Whether a thread whose latest message (at `latest`, from `sender`) is
/// someone else's to you needs your reply: as it was sorted (it needs you,
/// or is a real first contact, and its request is open). With a model,
/// only that: what isn't sorted yet soon is, and a rule can't tell a
/// person from a support desk or a store's receipts sent straight to you.
/// With none, a sorting that saw the latest message, else anyone's but an
/// automated sender's.
fn needs_reply(sorted: Option<&api::Triage>, latest: i64, sender: &str, model: bool) -> bool {
    match sorted {
        Some(t) if model || t.at >= latest => matches!(t.category.as_str(), "needs_you" | "first_contact") && t.request.is_some(),
        _ => !model && !crate::relations::automated(sender),
    }
}

/// A thread's messages, with each one's HTML (if any) and attachments.
pub async fn thread(m: &Memex, account: &str, thread_id: &str) -> anyhow::Result<Vec<api::MailMessage>> {
    let rows = sqlx::query("SELECT * FROM mail WHERE account = ? AND thread_id = ? ORDER BY date")
        .bind(account)
        .bind(thread_id)
        .fetch_all(&m.store.pool)
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let read = |hash: Option<String>| async move {
            match hash {
                Some(h) => tokio::fs::read(m.blob_path(&h)).await.ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
                None => None,
            }
        };
        let item_id: String = r.get("item_id");
        let (text, html) = if r.get::<i64, _>("parsed") == 1 {
            (read(r.get("text_blob")).await.unwrap_or_default(), read(r.get("html_blob")).await)
        } else {
            // Kept before messages were read once: its searchable text for
            // now, and read again soon.
            crate::extract::queue(
                m,
                &format!("reparse:{item_id}"),
                "mail",
                &format!("head:{}", r.get::<String, _>("blob")),
                "",
                json!({"kind": "reparse", "item": item_id}),
            )
            .await?;
            let body: String = sqlx::query_scalar("SELECT body FROM items WHERE id = ?")
                .bind(&item_id)
                .fetch_optional(&m.store.pool)
                .await?
                .unwrap_or_default();
            (body.splitn(4, '\n').nth(3).unwrap_or(&body).to_owned(), None)
        };
        let attachments = serde_json::from_str::<Vec<Value>>(&r.get::<String, _>("parts"))
            .unwrap_or_default()
            .iter()
            .map(|a| api::MailAttachment {
                index: a["index"].as_u64().unwrap_or(0) as u32,
                name: a["name"].as_str().unwrap_or("attachment").to_owned(),
                mime: a["mime"].as_str().unwrap_or_default().to_owned(),
                size: a["size"].as_u64().unwrap_or(0),
            })
            .collect();
        out.push(api::MailMessage {
            item_id,
            gmail_id: r.get("gmail_id"),
            from: r.get("from_addr"),
            from_name: r.get("from_name"),
            to: serde_json::from_str(&r.get::<String, _>("to_json")).unwrap_or_default(),
            cc: serde_json::from_str(&r.get::<String, _>("cc_json")).unwrap_or_default(),
            date: r.get("date"),
            subject: r.get("subject"),
            text,
            html,
            attachments,
            labels: serde_json::from_str(&r.get::<String, _>("labels")).unwrap_or_default(),
            unread: r.get::<i64, _>("unread") != 0,
            link: link(m, account, thread_id, &r.get::<String, _>("gmail_id"), &r.get::<String, _>("rfc_id")),
        });
    }
    Ok(out)
}

/// A message's attachment: (name, mime, bytes), read from the message
/// when asked for.
pub async fn attachment(m: &Memex, item_id: &str, index: usize) -> anyhow::Result<(String, String, Vec<u8>)> {
    let blob: String = sqlx::query_scalar("SELECT blob FROM mail WHERE item_id = ?")
        .bind(item_id)
        .fetch_optional(&m.store.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no such message"))?;
    let (v, mut files) = crate::extract::now(m, "attachment", &format!("head:{blob}"), "", Some(index as u64)).await?;
    let data = files.remove("data").ok_or_else(|| anyhow::anyhow!("no attachment {index}"))?;
    Ok((v["name"].as_str().unwrap_or("attachment").to_owned(), v["mime"].as_str().unwrap_or("application/octet-stream").to_owned(), data))
}

/// Adds and removes labels on a thread: through the mail app that owns
/// the account (which tells the mailbox, and keeps the new labels), or
/// here only for imported mail: read and unread (UNREAD), archive
/// (−INBOX), star (STARRED), trash (TRASH).
pub async fn modify(m: &Memex, account: &str, thread_id: &str, add: &[String], remove: &[String], by: &str) -> anyhow::Result<()> {
    if !account.starts_with("import:") {
        let a = m.account("mail", account)?;
        m.app_call(&a, "modify", json!({"account": a.id, "thread": thread_id, "add": add, "remove": remove}), by).await?;
        return Ok(());
    }
    let rows = sqlx::query("SELECT item_id, labels FROM mail WHERE account = ? AND thread_id = ?")
        .bind(account)
        .bind(thread_id)
        .fetch_all(&m.store.pool)
        .await?;
    for r in rows {
        let mut labels: Vec<String> = serde_json::from_str(&r.get::<String, _>("labels")).unwrap_or_default();
        labels.retain(|l| !remove.contains(l));
        for a in add {
            if !labels.contains(a) {
                labels.push(a.clone());
            }
        }
        set_labels(m, &r.get::<String, _>("item_id"), &labels).await?;
    }
    Ok(())
}

/// A message to send (or keep as a draft), as RFC 822 in base64url, and
/// the thread it continues.
async fn compose(m: &Memex, d: &api::MailDraft) -> anyhow::Result<(String, Option<String>)> {
    let acct = m.account("mail", &d.account)?;
    let mut b = mail_builder::MessageBuilder::new()
        .from((acct.name.clone(), acct.email.clone()))
        .to(d.to.iter().map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()).collect::<Vec<_>>())
        .subject(d.subject.clone());
    if !d.cc.is_empty() {
        b = b.cc(d.cc.clone());
    }
    if !d.bcc.is_empty() {
        b = b.bcc(d.bcc.clone());
    }
    let mut thread = None;
    if let Some(reply) = &d.reply_to {
        let r = sqlx::query("SELECT thread_id, rfc_id FROM mail WHERE item_id = ?").bind(reply).fetch_optional(&m.store.pool).await?;
        if let Some(r) = r {
            let rfc: String = r.get("rfc_id");
            if !rfc.is_empty() {
                b = b.in_reply_to(rfc.clone()).references(rfc);
            }
            thread = Some(r.get("thread_id"));
        }
    }
    let html = markdown_html(&d.body);
    b = b.text_body(d.body.clone()).html_body(html);
    for a in &d.attachments {
        let data = base64::engine::general_purpose::STANDARD.decode(a.data.as_bytes())?;
        b = b.attachment(if a.mime.is_empty() { "application/octet-stream".to_owned() } else { a.mime.clone() }, a.name.clone(), data);
    }
    let raw = b.write_to_vec()?;
    use base64::Engine;
    Ok((base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw), thread))
}

fn markdown_html(md: &str) -> String {
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(md));
    html
}

/// Saves a draft in Gmail (new, or replacing `draft_id`); its id.
pub async fn draft(m: &Memex, d: &api::MailDraft, by: &str) -> anyhow::Result<String> {
    let (raw, thread) = compose(m, d).await?;
    let a = m.account("mail", &d.account)?;
    let v = m.app_call(&a, "draft", json!({"account": a.id, "raw": raw, "thread": thread, "draft_id": d.draft_id}), by).await?;
    Ok(v["result"]["draft_id"].as_str().unwrap_or_default().to_owned())
}

/// Queues a message: sent after the undo window, or at `send_at`.
pub async fn queue(m: &Memex, d: &api::MailDraft, made_by: &str) -> anyhow::Result<api::Outgoing> {
    anyhow::ensure!(!d.to.is_empty(), "a message needs a recipient");
    let (raw, thread) = compose(m, d).await?;
    let acct = m.account("mail", &d.account)?.id;
    let now = c6_core::now_ms();
    let send_at = d.send_at.filter(|t| *t > now).unwrap_or(now + UNDO_MS);
    let id = uuid::Uuid::now_v7().simple().to_string();
    sqlx::query("INSERT INTO outgoing (id, account, raw, thread_id, send_at, status, subject, to_json, made_by, created_at) VALUES (?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?)")
        .bind(&id)
        .bind(&acct)
        .bind(&raw)
        .bind(&thread)
        .bind(send_at)
        .bind(&d.subject)
        .bind(json!(d.to).to_string())
        .bind(made_by)
        .bind(now)
        .execute(&m.store.pool)
        .await?;
    Ok(api::Outgoing { id, send_at, status: "pending".into(), subject: d.subject.clone(), to: d.to.clone(), error: None })
}

/// Cancels a queued message, if it has not gone yet.
pub async fn cancel(m: &Memex, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("UPDATE outgoing SET status = 'cancelled' WHERE id = ? AND status = 'pending'")
        .bind(id)
        .execute(&m.store.pool)
        .await?
        .rows_affected()
        > 0)
}

pub async fn outgoing(m: &Memex) -> anyhow::Result<Vec<api::Outgoing>> {
    let rows = sqlx::query("SELECT * FROM outgoing WHERE status IN ('pending', 'failed') OR created_at > ? ORDER BY send_at DESC LIMIT 50")
        .bind(c6_core::now_ms() - 86_400_000)
        .fetch_all(&m.store.pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| api::Outgoing {
            id: r.get("id"),
            send_at: r.get("send_at"),
            status: r.get("status"),
            subject: r.get("subject"),
            to: serde_json::from_str(&r.get::<String, _>("to_json")).unwrap_or_default(),
            error: r.get("error"),
        })
        .collect())
}

/// Sends what is due.
pub async fn send_due(m: &Memex) -> anyhow::Result<()> {
    let due = sqlx::query("SELECT * FROM outgoing WHERE status = 'pending' AND send_at <= ? ORDER BY send_at")
        .bind(c6_core::now_ms())
        .fetch_all(&m.store.pool)
        .await?;
    for r in due {
        let id: String = r.get("id");
        // Claimed first, so it is sent once.
        if sqlx::query("UPDATE outgoing SET status = 'sending' WHERE id = ? AND status = 'pending'")
            .bind(&id)
            .execute(&m.store.pool)
            .await?
            .rows_affected()
            == 0
        {
            continue;
        }
        let account: String = r.get("account");
        let input = json!({"account": account, "raw": r.get::<String, _>("raw"), "thread": r.get::<Option<String>, _>("thread_id")});
        let result = match m.account("mail", &account) {
            Ok(a) => m.app_call(&a, "send", input, &r.get::<String, _>("made_by")).await,
            Err(e) => Err(e),
        };
        let (status, error) = match &result {
            Ok(_) => ("sent", None),
            Err(e) => ("failed", Some(format!("{e:#}"))),
        };
        sqlx::query("UPDATE outgoing SET status = ?, error = ? WHERE id = ?")
            .bind(status)
            .bind(&error)
            .bind(&id)
            .execute(&m.store.pool)
            .await?;
        match result {
            Ok(_) => info!(subject = %r.get::<String, _>("subject"), "mail sent"),
            Err(e) => warn!("sending mail: {e:#}"),
        }
    }
    Ok(())
}

/// Gmail's labels for the mail client's sidebar: (account, id, name).
pub async fn label_list(m: &Memex) -> anyhow::Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as("SELECT account, id, name FROM label_names WHERE type = 'user' ORDER BY name").fetch_all(&m.store.pool).await?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn markdown() {
        assert_eq!(super::markdown_html("Hi **there**"), "<p>Hi <strong>there</strong></p>\n");
    }

    #[test]
    fn needs_reply_follows_the_sorting() {
        let sorted = |category: &str, at: i64| c6_api::Triage { category: category.into(), at, request: Some(1), ..Default::default() };
        for model in [true, false] {
            let needs = |t: Option<c6_api::Triage>, sender: &str| super::needs_reply(t.as_ref(), 100, sender, model);
            // Sorted: what it was sorted as, whoever sent it.
            assert!(needs(Some(sorted("needs_you", 100)), "no-reply@bank.example"));
            assert!(needs(Some(sorted("first_contact", 100)), "pat@new.example"));
            for c in ["notification", "newsletter", "noise", "pitch", "fyi", "yours"] {
                assert!(!needs(Some(sorted(c, 100)), "orders@shop.example"), "{c}");
            }
            // What needs you, its request settled in the Brief (done,
            // dropped, snoozed): not now.
            assert!(!needs(Some(c6_api::Triage { request: None, ..sorted("needs_you", 100) }), "ana@acme.com"));
        }
        // With a model, only what it sorted: as sorted, though before its
        // latest message (it's sorted again soon); not what isn't yet.
        let needs = |t: Option<c6_api::Triage>, sender: &str| super::needs_reply(t.as_ref(), 100, sender, true);
        assert!(!needs(None, "ana@acme.com"));
        assert!(!needs(Some(sorted("notification", 50)), "ana@acme.com"));
        assert!(needs(Some(sorted("needs_you", 50)), "ana@acme.com"));
        // With none, what isn't sorted (or was before its latest message):
        // a person's, not an automated sender's.
        let needs = |t: Option<c6_api::Triage>, sender: &str| super::needs_reply(t.as_ref(), 100, sender, false);
        assert!(needs(None, "ana@acme.com"));
        assert!(!needs(None, "no-reply@bank.example"));
        assert!(needs(Some(sorted("notification", 50)), "ana@acme.com"));
        assert!(!needs(Some(sorted("needs_you", 50)), "notifications@github.com"));
    }
}
