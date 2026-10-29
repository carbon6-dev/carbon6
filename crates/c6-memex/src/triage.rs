//! Triage (docs/brief.md §3–4): each recent mail thread and chat
//! conversation sorted for the brief: what needs you, a real first contact,
//! FYI, newsletters and notifications, noise, and cold pitches.
//!
//! Cold pitches go first and mostly without a model: someone you've written
//! to (or anyone at their company, or a contact) is never one; a stranger's
//! mail carrying a sales-sequence tool's marks, or a sequence's follow-up
//! in a thread you never answered, is. The model sorts the rest, with your
//! corrections as examples; senders you mark are settled for good.

use c6_api as api;
use c6_llm::Block;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::warn;

use crate::Memex;
use crate::llm;
use crate::relations::{SHARED_DOMAINS, domain};
use crate::store::clip;

const VERSION: &str = "triage-v1";
const DAY: i64 = 86_400_000;
/// Only what's recent is sorted: the brief is about now (and so is Mail's
/// "Needs reply").
pub const WINDOW: i64 = 14 * DAY;
/// What one call reads of a conversation.
const MAX_INPUT: usize = 10_000;
pub const CATEGORIES: [&str; 8] = ["needs_you", "first_contact", "fyi", "newsletter", "notification", "pitch", "noise", "yours"];

/// A conversation waiting to be sorted.
struct Target {
    key: String,
    kind: &'static str,
}

/// What a sorting step did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sorting {
    /// Sorted a conversation.
    Worked,
    /// Nothing to sort.
    Idle,
    /// Conversations wait for a model call it couldn't make (no model, or
    /// no budget left as it looked): nothing else may take a call first.
    Waiting,
}

/// Sorts one more conversation, if there is one to sort and (when it needs
/// a model) a model and budget.
pub async fn step(m: &Memex) -> anyhow::Result<Sorting> {
    // The history each conversation is read with.
    if let Err(e) = crate::relations::refresh(m).await {
        warn!("counting correspondents: {e:#}");
    }
    let model = m.understanding();
    let can_ask = match &model {
        Some((task, per_day)) => crate::understand::can_call(m, task, *per_day).await?,
        None => false,
    };
    let mut waits = false;
    for t in waiting(m).await? {
        // Another step is sorting it.
        let Some(_claim) = m.claim(&format!("sort:{}", t.key)) else { continue };
        let done = match t.kind {
            "mail" => mail(m, &t.key, can_ask.then(|| model.as_ref().map(|(task, _)| task)).flatten()).await,
            _ => chat(m, &t.key, can_ask.then(|| model.as_ref().map(|(task, _)| task)).flatten()).await,
        };
        match done {
            Ok(true) => return Ok(Sorting::Worked),
            Ok(false) => {
                waits |= !can_ask;
                continue;
            }
            Err(e) if llm::waits(m, &e).await? => {
                warn!(target = %t.key, "triage waits: {e:#}");
                return Ok(Sorting::Waiting);
            }
            Err(e) => {
                warn!(target = %t.key, "triage: {e:#}");
                // Not again until it has news.
                let s = Sorted::new("fyi", &clip(&format!("Couldn't be sorted: {e:#}"), 140), "error");
                put(m, &t.key, t.kind, &s, "", "", "", c6_core::now_ms()).await?;
                return Ok(Sorting::Worked);
            }
        }
    }
    Ok(if waits { Sorting::Waiting } else { Sorting::Idle })
}

/// Recent conversations with news since they were sorted, newest first.
async fn waiting(m: &Memex) -> anyhow::Result<Vec<Target>> {
    let since = c6_core::now_ms() - WINDOW;
    let mut out: Vec<(i64, Target)> = Vec::new();
    let threads = sqlx::query(
        "SELECT x.account, x.thread_id, x.at FROM (SELECT m.account, m.thread_id, MAX(m.date) AS at, MAX(i.added_at) AS added, \
         SUM(m.inbox) AS inbox, SUM(m.spam) AS spam FROM mail m JOIN items i ON i.id = m.item_id WHERE m.date > ? AND m.trash = 0 \
         AND m.draft = 0 GROUP BY m.account, m.thread_id) x LEFT JOIN triage t ON t.target = 'thread:' || x.account || ':' || x.thread_id \
         WHERE x.inbox > 0 AND x.spam = 0 AND (t.done_at IS NULL OR t.done_at < x.added \
         OR (?2 AND t.by IN ('rule', 'signals', 'sequence') AND t.category != 'yours')) ORDER BY x.at DESC LIMIT 20",
    )
    .bind(since)
    // What rules sorted while there was no model, the model sorts again.
    .bind(m.understanding().is_some())
    .fetch_all(&m.store.pool)
    .await?;
    for r in threads {
        let key = format!("thread:{}:{}", r.get::<String, _>("account"), r.get::<String, _>("thread_id"));
        out.push((r.get("at"), Target { key, kind: "mail" }));
    }
    for (at, key) in chat_waiting(m, since).await? {
        out.push((at, Target { key, kind: "chat" }));
    }
    out.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    Ok(out.into_iter().map(|(_, t)| t).take(20).collect())
}

/// Chat conversations that may need you, with news: DMs and group DMs, and
/// in channels, threads you're in or are mentioned in and messages that
/// mention you. (Only messages that say whose they are: older ones don't.)
async fn chat_waiting(m: &Memex, since: i64) -> anyhow::Result<Vec<(i64, String)>> {
    let rows = sqlx::query(
        "SELECT meta, created_at, added_at FROM items WHERE kind = 'chat' AND created_at > ? ORDER BY created_at DESC LIMIT 2000",
    )
    .bind(since)
    .fetch_all(&m.store.pool)
    .await?;
    // Each conversation's latest time and news, and whether it's yours to look at.
    let mut seen: std::collections::BTreeMap<String, (i64, i64, bool)> = Default::default();
    for r in &rows {
        let meta: Value = serde_json::from_str(&r.get::<String, _>("meta")).unwrap_or_default();
        if meta["from_me"].is_null() {
            continue;
        }
        let Some(key) = chat_key(&meta) else { continue };
        let direct = matches!(meta["channel_kind"].as_str(), Some("dm" | "group"));
        let e = seen.entry(key).or_insert((r.get("created_at"), 0, false));
        e.1 = e.1.max(r.get("added_at"));
        e.2 |= direct || meta["mentions_me"] == true || meta["from_me"] == true;
    }
    let mut out = Vec::new();
    for (key, (at, added, mine)) in seen {
        if !mine {
            continue;
        }
        let done: Option<i64> =
            sqlx::query_scalar("SELECT done_at FROM triage WHERE target = ?").bind(&key).fetch_optional(&m.store.pool).await?;
        if done.is_none_or(|d| d < added) {
            out.push((at, key));
        }
    }
    Ok(out)
}

/// Which conversation a chat message is part of: its thread; for a DM or
/// group's other messages, the conversation; else the message itself.
fn chat_key(meta: &Value) -> Option<String> {
    let (w, c) = (meta["workspace"].as_str()?, meta["channel"].as_str()?);
    Some(match (meta["thread_ts"].as_str(), meta["channel_kind"].as_str()) {
        (Some(t), _) => format!("chat:{w}:{c}:{t}"),
        (None, Some("dm" | "group")) => format!("chat:{w}:{c}"),
        (None, _) => format!("chat:{w}:{c}:{}", meta["ts"].as_str()?),
    })
}

/// How a conversation was sorted (and, when it needs you, what's asked).
struct Sorted {
    category: String,
    reason: String,
    by: &'static str,
    asks: String,
    due: Option<i64>,
    urgent: bool,
    invitation: bool,
}

impl Sorted {
    fn new(category: &str, reason: &str, by: &'static str) -> Self {
        Self { category: category.into(), reason: reason.into(), by, asks: String::new(), due: None, urgent: false, invitation: false }
    }
}

/// Keeps how it was sorted, and its request (docs/brief.md §5).
#[allow(clippy::too_many_arguments)]
async fn settle(
    m: &Memex,
    key: &str,
    kind: &str,
    s: &Sorted,
    sender: &str,
    sender_name: &str,
    title: &str,
    at: i64,
    seen_mail: &[String],
    task: Option<&crate::llm::Task>,
) -> anyhow::Result<bool> {
    let f = crate::requests::Found {
        target: key,
        kind,
        category: &s.category,
        reason: &s.reason,
        asks: &s.asks,
        due: s.due,
        person: sender,
        person_name: sender_name,
        title,
        at,
        seen_mail,
        urgent: s.urgent,
        invitation: s.invitation,
    };
    // Its request first: a same-request check that fails for now leaves
    // it unsorted, to wait.
    crate::requests::follow(m, &f, task).await?;
    put(m, key, kind, s, sender, sender_name, title, at).await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn put(m: &Memex, key: &str, kind: &str, s: &Sorted, sender: &str, sender_name: &str, title: &str, at: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO triage (target, kind, category, reason, by, sender, sender_name, title, at, done_at, version) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(target) DO UPDATE SET category = excluded.category, reason = excluded.reason, \
         by = excluded.by, sender = excluded.sender, sender_name = excluded.sender_name, title = excluded.title, at = excluded.at, \
         done_at = excluded.done_at, version = excluded.version",
    )
    .bind(key)
    .bind(kind)
    .bind(&s.category)
    .bind(&s.reason)
    .bind(s.by)
    .bind(sender)
    .bind(sender_name)
    .bind(title)
    .bind(at)
    .bind(c6_core::now_ms())
    .bind(VERSION)
    .execute(&m.store.pool)
    .await?;
    Ok(())
}

/// Your mark on a sender (by address, else by domain): pitch or known.
async fn mark_of(m: &Memex, addr: &str) -> anyhow::Result<Option<String>> {
    let addr = addr.to_lowercase();
    Ok(sqlx::query_scalar("SELECT mark FROM sender_marks WHERE pattern IN (?, ?) ORDER BY pattern = ? DESC LIMIT 1")
        .bind(&addr)
        .bind(format!("@{}", domain(&addr)))
        .bind(&addr)
        .fetch_optional(&m.store.pool)
        .await?)
}

/// Whether you know them: you've written to them (or, at a company's own
/// domain, to anyone there), they're a contact or were at your meetings, or
/// they're at your own domain.
async fn known(m: &Memex, addr: &str, mine: &[String]) -> anyhow::Result<bool> {
    let addr = addr.to_lowercase();
    let d = domain(&addr);
    if mine.iter().any(|a| domain(a) == d && !SHARED_DOMAINS.contains(&d.as_str())) {
        return Ok(true);
    }
    let quoted = format!("%\"{addr}\"%");
    let written: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mail WHERE sent = 1 AND (to_json LIKE ?1 OR cc_json LIKE ?1))")
        .bind(&quoted)
        .fetch_one(&m.store.pool)
        .await?;
    if written {
        return Ok(true);
    }
    if !d.is_empty() && !SHARED_DOMAINS.contains(&d.as_str()) {
        let at = format!("%@{d}\"%");
        let company: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mail WHERE sent = 1 AND (to_json LIKE ?1 OR cc_json LIKE ?1))")
                .bind(&at)
                .fetch_one(&m.store.pool)
                .await?;
        if company {
            return Ok(true);
        }
    }
    let contact: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM contacts WHERE lower(emails) LIKE ?1) OR EXISTS (SELECT 1 FROM events WHERE lower(attendees) LIKE ?2)",
    )
    .bind(&quoted)
    .bind(format!("%{addr}%"))
    .fetch_one(&m.store.pool)
    .await?;
    Ok(contact)
}

/// The sequence marks (`memex.triage.signals`): marks and follow-up phrases.
fn signals(m: &Memex) -> (Vec<String>, Vec<String>) {
    let text = m.prompt("memex.triage.signals", api::prompts::TRIAGE_SIGNALS);
    let (mut marks, mut phrases) = (Vec::new(), Vec::new());
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        match l.strip_prefix("phrase:") {
            Some(p) => phrases.push(p.trim().to_lowercase()),
            None => marks.push(l.to_lowercase()),
        }
    }
    (marks, phrases)
}

/// Your corrections, as the model's examples.
async fn examples(m: &Memex, kind: &str) -> anyhow::Result<String> {
    let rows = sqlx::query("SELECT sender, title, category, note FROM triage_examples WHERE kind = ? ORDER BY id DESC LIMIT 20")
        .bind(kind)
        .fetch_all(&m.store.pool)
        .await?;
    if rows.is_empty() {
        return Ok("(none yet)".into());
    }
    Ok(rows
        .iter()
        .map(|r| {
            let note: String = r.get("note");
            format!(
                "- \"{}\" from {}: {}{}",
                r.get::<String, _>("title"),
                r.get::<String, _>("sender"),
                r.get::<String, _>("category"),
                if note.is_empty() { String::new() } else { format!(" ({note})") }
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Asks the model to sort a conversation with `prompt`.
async fn ask(
    m: &Memex,
    task: &crate::llm::Task,
    about: &str,
    prompt: &str,
    kind: &str,
    text: &str,
    allowed: &[&str],
) -> anyhow::Result<Sorted> {
    let owner: Vec<String> = m.accounts().iter().map(|a| a.email.clone()).collect();
    let owner = if owner.is_empty() { "the owner".to_owned() } else { owner.join(", ") };
    let rule = m.prompt("memex.triage.urgent", api::prompts::TRIAGE_URGENT);
    let ask = api::prompts::fill(prompt, &[("owner", &owner), ("examples", &examples(m, kind).await?), ("urgent", &rule)]);
    let system = m.prompt("memex.understand.system", api::prompts::UNDERSTAND_SYSTEM);
    crate::understand::count_call(m).await?;
    let answer = llm::ask(m, task, "sorting", about, &system, vec![Block::Text(format!("{ask}\n\n{text}"))], 400).await?;
    let v = llm::json_of(&answer)?;
    let category = v["category"].as_str().unwrap_or("").trim().to_lowercase();
    let category = if allowed.contains(&category.as_str()) { category } else { "fyi".to_owned() };
    let due = v["due"]
        .as_str()
        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(17, 0, 0))
        .map(|d| d.and_utc().timestamp_millis());
    let invitation = category == "needs_you" && v["invitation"] == true;
    Ok(Sorted {
        category,
        reason: clip(v["reason"].as_str().unwrap_or("").trim(), 140),
        by: "model",
        asks: v["asks"].as_str().unwrap_or("").trim().to_owned(),
        due,
        urgent: v["urgent"] == true,
        invitation,
    })
}

/// A mail thread. Whether it was sorted (false: it needs a model there
/// isn't, or isn't budget for, now).
async fn mail(m: &Memex, key: &str, task: Option<&crate::llm::Task>) -> anyhow::Result<bool> {
    let (account, thread) = key.strip_prefix("thread:").and_then(|k| k.rsplit_once(':')).ok_or_else(|| anyhow::anyhow!("not a thread"))?;
    let rows = sqlx::query(
        "SELECT m.item_id, m.from_addr, m.from_name, m.subject, m.date, m.sent, m.list, m.text_blob, m.html_blob, i.body FROM mail m \
         JOIN items i ON i.id = m.item_id WHERE m.account = ? AND m.thread_id = ? AND m.draft = 0 AND m.trash = 0 ORDER BY m.date",
    )
    .bind(account)
    .bind(thread)
    .fetch_all(&m.store.pool)
    .await?;
    let Some(last) = rows.last() else { return Ok(false) };
    let me: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
    let is_mine =
        |r: &sqlx::sqlite::SqliteRow| r.get::<i64, _>("sent") == 1 || me.contains(&r.get::<String, _>("from_addr").to_lowercase());
    let at: i64 = last.get("date");
    let subject: String = last.get("subject");
    if is_mine(last) {
        return settle(m, key, "mail", &Sorted::new("yours", "You wrote last", "rule"), "", "", &subject, at, &[], task).await;
    }
    let (sender, sender_name): (String, String) = (last.get::<String, _>("from_addr").to_lowercase(), last.get("from_name"));
    let (who, name, title) = (sender.as_str(), sender_name.as_str(), subject.as_str());
    let done = move |s: Sorted| async move { settle(m, key, "mail", &s, who, name, title, at, &[], task).await };
    // Your marks settle it; so does a sender you marked known, as not a pitch.
    let mark = mark_of(m, &sender).await?;
    if mark.as_deref() == Some("pitch") {
        return done(Sorted::new("pitch", "A sender you marked as a pitch", "mark")).await;
    }
    let answered = rows.iter().any(is_mine);
    let knows = mark.as_deref() == Some("known") || answered || known(m, &sender, &me).await?;
    let list = last.get::<i64, _>("list") == 1;
    let automated = crate::relations::automated(&sender);
    // What the rules see, for the model as hints (and, with no model, the
    // verdict): a sequence tool's marks, a sequence's follow-up.
    let mut hints = Vec::new();
    if list {
        hints.push("The latest message has mailing-list headers (it was bulk-sent).".to_owned());
    }
    if !knows {
        let (marks, phrases) = signals(m);
        for r in rows.iter().filter(|r| !is_mine(r)) {
            let mut seen = r.get::<String, _>("body").to_lowercase();
            for blob in [r.get::<Option<String>, _>("html_blob"), r.get::<Option<String>, _>("text_blob")].into_iter().flatten() {
                if let Ok(b) = tokio::fs::read(m.blob_path(&blob)).await {
                    seen.push_str(&String::from_utf8_lossy(&b).to_lowercase());
                }
            }
            if let Some(found) = marks.iter().find(|x| seen.contains(x.as_str())) {
                hints.push(format!("Its links or text carry a sales-sequence tool's mark ({found})."));
                if task.is_none() && m.understanding().is_none() {
                    return done(Sorted::new("pitch", &format!("Sent through a sales-sequence tool ({found})"), "signals")).await;
                }
                break;
            }
        }
        let latest = last.get::<String, _>("body").to_lowercase();
        if let Some(p) = phrases.iter().find(|p| latest.contains(p.as_str())) {
            hints.push(format!("The latest message reads like a sequence's follow-up (\"{p}\")."));
            if task.is_none() && m.understanding().is_none() {
                return done(Sorted::new("pitch", &format!("A sequence's follow-up (\"{p}\") from a stranger"), "sequence")).await;
            }
        }
    }
    let Some(task) = task else {
        // No model at all: the rules sort what they can (the model sorts it
        // again once there is one). A model without budget: it waits.
        if m.understanding().is_some() {
            return Ok(false);
        }
        return match (list, automated) {
            (_, true) => done(Sorted::new("notification", "Automated", "rule")).await,
            (true, false) => done(Sorted::new("newsletter", "A mailing list", "rule")).await,
            _ => Ok(false),
        };
    };
    // What the thread says, after the history with the sender.
    let history = crate::relations::history(m, &sender, &sender_name).await?;
    let mut text = format!("History with the sender (counted from the owner's mail):\n{history}\n");
    // Who they are to the owner, from earlier mail (written now if missing).
    let per_day = m.understanding().map(|(_, n)| n).unwrap_or(0);
    let cards = crate::cards::for_sorting(m, task, per_day, &sender, &sender_name).await.unwrap_or_else(|e| {
        warn!(%sender, "cards: {e:#}");
        Vec::new()
    });
    if !cards.is_empty() {
        let lines: Vec<String> = cards.iter().map(crate::cards::text).collect();
        text.push_str(&format!("Who they are to the owner (from earlier mail):\n- {}\n", lines.join("\n- ")));
    }
    if !hints.is_empty() {
        text.push_str(&format!("What the rules noticed:\n- {}\n", hints.join("\n- ")));
    }
    text.push_str(&format!("\nSubject: {subject}\n"));
    let mut seen_mail = Vec::new();
    for r in rows.iter().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
        let date =
            chrono::DateTime::from_timestamp_millis(r.get("date")).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
        let who = if is_mine(r) {
            "the owner".to_owned()
        } else {
            format!("{} <{}>", r.get::<String, _>("from_name"), r.get::<String, _>("from_addr"))
        };
        let body: String = r.get("body");
        let shown = clip(&body, 2500);
        text.push_str(&format!("--- From {who} ({date})\n{shown}\n"));
        // Certify only whole messages that also survive the final prompt limit.
        if r.get::<i64, _>("sent") == 0 && shown == body.trim() && text.trim().chars().count() <= MAX_INPUT {
            seen_mail.push(r.get::<String, _>("item_id"));
        }
    }
    let text = clip(&text, MAX_INPUT);
    seen_mail.sort();
    let sorted = if knows {
        let prompt = m.prompt("memex.triage", api::prompts::TRIAGE);
        // Someone you marked as known isn't pitching; anyone else may be.
        let allowed: &[&str] = if mark.as_deref() == Some("known") {
            &["needs_you", "fyi", "newsletter", "notification", "noise"]
        } else {
            &["needs_you", "fyi", "newsletter", "notification", "noise", "pitch"]
        };
        ask(m, task, &subject, &prompt, "mail", &text, allowed).await?
    } else {
        let prompt = m.prompt("memex.triage.pitch", api::prompts::TRIAGE_PITCH);
        let allowed = ["pitch", "first_contact", "fyi", "newsletter", "notification", "noise"];
        ask(m, task, &subject, &prompt, "mail", &text, &allowed).await?
    };
    settle(m, key, "mail", &sorted, who, name, title, at, &seen_mail, Some(task)).await
}

/// A chat conversation (people in your workspace are known to you).
async fn chat(m: &Memex, key: &str, task: Option<&crate::llm::Task>) -> anyhow::Result<bool> {
    let parts: Vec<&str> = key.strip_prefix("chat:").unwrap_or_default().splitn(3, ':').collect();
    let (w, c) = (parts.first().copied().unwrap_or_default(), parts.get(1).copied().unwrap_or_default());
    let f = |k: &str| format!("json_extract(meta, '$.{k}')");
    let sql = match parts.get(2) {
        // A thread (its first message and replies), or one message.
        Some(_) => format!(
            "SELECT title, body, meta, created_at FROM items WHERE kind = 'chat' AND {} = ?1 AND {} = ?2 AND ({} = ?3 OR {} = ?3) \
             ORDER BY created_at DESC LIMIT 15",
            f("workspace"),
            f("channel"),
            f("thread_ts"),
            f("ts")
        ),
        // A DM's or group's messages outside threads.
        None => format!(
            "SELECT title, body, meta, created_at FROM items WHERE kind = 'chat' AND {} = ?1 AND {} = ?2 AND {} IS NULL \
             ORDER BY created_at DESC LIMIT 15",
            f("workspace"),
            f("channel"),
            f("thread_ts")
        ),
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(w)
        .bind(c)
        .bind(parts.get(2).copied().unwrap_or_default())
        .fetch_all(&m.store.pool)
        .await?;
    let Some(last) = rows.first() else { return Ok(false) };
    let meta = |r: &sqlx::sqlite::SqliteRow| serde_json::from_str::<Value>(&r.get::<String, _>("meta")).unwrap_or_default();
    let lm = meta(last);
    let at: i64 = last.get("created_at");
    let name = lm["channel_name"].as_str().unwrap_or_default().to_owned();
    if lm["from_me"] == true {
        return settle(m, key, "chat", &Sorted::new("yours", "You wrote last", "rule"), "", "", &name, at, &[], task).await;
    }
    // Who, as mail knows them when Slack says their address.
    let sender = lm["user_email"].as_str().or(lm["user"].as_str()).unwrap_or_default().to_owned();
    let sender_name = lm["user_name"].as_str().unwrap_or_default().to_owned();
    let Some(task) = task else { return Ok(false) };
    let mut text = format!("Slack conversation {name} ({})\n", lm["channel_kind"].as_str().unwrap_or(""));
    for r in rows.iter().rev() {
        let mm = meta(r);
        let date = chrono::DateTime::from_timestamp_millis(r.get("created_at"))
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let who = if mm["from_me"] == true { "the owner".to_owned() } else { mm["user_name"].as_str().unwrap_or("someone").to_owned() };
        let body: String = r.get("body");
        let said = body.split_once(": ").map_or(body.as_str(), |(_, t)| t);
        text.push_str(&format!(
            "--- {who} ({date}){}: {}\n",
            if mm["mentions_me"] == true { ", mentioning the owner" } else { "" },
            clip(said, 1500)
        ));
    }
    let prompt = m.prompt("memex.triage", api::prompts::TRIAGE);
    let allowed = ["needs_you", "fyi", "notification", "noise"];
    let s = ask(m, task, &name, &prompt, "chat", &clip(&text, MAX_INPUT), &allowed).await?;
    settle(m, key, "chat", &s, &sender, &sender_name, &name, at, &[], Some(task)).await
}

/// Sorted conversations, newest first: of a category, or all.
/// Recent conversations as sorted; with `unseen`, only those with news since
/// you said you'd seen them.
pub async fn list(m: &Memex, category: Option<&str>, kind: Option<&str>, unseen: bool, limit: u32) -> anyhow::Result<Vec<api::Triage>> {
    let rows = sqlx::query(
        "SELECT * FROM triage WHERE (?1 IS NULL OR category = ?1) AND (?2 IS NULL OR kind = ?2) AND at > ?3 \
         AND (?5 = 0 OR seen_at IS NULL OR seen_at < at) ORDER BY at DESC LIMIT ?4",
    )
    .bind(category)
    .bind(kind)
    .bind(c6_core::now_ms() - WINDOW)
    .bind(limit.clamp(1, 500) as i64)
    .bind(unseen)
    .fetch_all(&m.store.pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

fn row(r: &sqlx::sqlite::SqliteRow) -> api::Triage {
    api::Triage {
        target: r.get("target"),
        kind: r.get("kind"),
        category: r.get("category"),
        reason: r.get("reason"),
        by: r.get("by"),
        sender: r.get("sender"),
        sender_name: r.get("sender_name"),
        title: r.get("title"),
        at: r.get("at"),
        known: None,
        request: None,
    }
}

/// How each category counts now (the brief's numbers).
pub async fn counts(m: &Memex) -> anyhow::Result<Value> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT category, COUNT(*) FROM triage WHERE at > ? GROUP BY category")
        .bind(c6_core::now_ms() - WINDOW)
        .fetch_all(&m.store.pool)
        .await?;
    Ok(Value::Object(rows.into_iter().map(|(k, n)| (k, json!(n))).collect()))
}

/// You've seen it: it leaves the brief's FYI until something new comes.
pub async fn seen(m: &Memex, target: &str) -> anyhow::Result<()> {
    let r =
        sqlx::query("UPDATE triage SET seen_at = ? WHERE target = ?").bind(c6_core::now_ms()).bind(target).execute(&m.store.pool).await?;
    anyhow::ensure!(r.rows_affected() == 1, "no conversation {target} is sorted");
    Ok(())
}

/// Whether sorting can go on: a model chosen, budget left today, and how
/// many conversations wait (up to 20); and how many recent ones were sorted,
/// by kind (mail, chat) and how many of those by the model.
pub async fn status(m: &Memex) -> anyhow::Result<Value> {
    let (model, budget) = match m.understanding() {
        Some((_, per_day)) => (true, crate::understand::budget_left(m, per_day).await?),
        None => (false, 0),
    };
    let rows: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT kind, COUNT(*), COALESCE(SUM([by] = 'model'), 0) FROM triage WHERE at > ? GROUP BY kind")
            .bind(c6_core::now_ms() - WINDOW)
            .fetch_all(&m.store.pool)
            .await?;
    let sorted: serde_json::Map<String, Value> =
        rows.into_iter().map(|(k, n, by_model)| (k, json!({"n": n, "by_model": by_model}))).collect();
    Ok(json!({"model": model, "budget_left": budget, "waiting": waiting(m).await?.len(), "sorted": sorted, "days": WINDOW / DAY}))
}

/// A thread's sorting, for the mail list.
pub async fn of_thread(m: &Memex, account: &str, thread: &str) -> anyhow::Result<Option<api::Triage>> {
    with_request(m, &format!("thread:{account}:{thread}")).await
}

/// How a conversation was sorted, with the open request it is part of.
async fn with_request(m: &Memex, target: &str) -> anyhow::Result<Option<api::Triage>> {
    let r = sqlx::query("SELECT * FROM triage WHERE target = ?").bind(target).fetch_optional(&m.store.pool).await?;
    let Some(r) = r else { return Ok(None) };
    let mut t = row(&r);
    t.request = crate::requests::open_of(m, target).await?;
    Ok(Some(t))
}

async fn example(m: &Memex, t: &api::Triage, category: &str, note: &str) -> anyhow::Result<()> {
    let who = if t.sender_name.is_empty() { t.sender.clone() } else { format!("{} <{}>", t.sender_name, t.sender) };
    sqlx::query("INSERT INTO triage_examples (kind, sender, title, category, note, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&t.kind)
        .bind(who)
        .bind(&t.title)
        .bind(category)
        .bind(note)
        .bind(c6_core::now_ms())
        .execute(&m.store.pool)
        .await?;
    Ok(())
}

async fn get(m: &Memex, target: &str) -> anyhow::Result<api::Triage> {
    let r = sqlx::query("SELECT * FROM triage WHERE target = ?").bind(target).fetch_optional(&m.store.pool).await?;
    let r = r.ok_or_else(|| anyhow::anyhow!("nothing sorted as {target:?}"))?;
    Ok(row(&r))
}

/// You sort a conversation yourself (with a note the model learns from).
pub async fn set(m: &Memex, target: &str, category: &str, note: &str) -> anyhow::Result<api::Triage> {
    anyhow::ensure!(CATEGORIES.contains(&category), "a category is one of {}", CATEGORIES.join(", "));
    let t = get(m, target).await?;
    sqlx::query("UPDATE triage SET category = ?, by = 'you', reason = ? WHERE target = ?")
        .bind(category)
        .bind(if note.is_empty() { "You sorted it" } else { note })
        .bind(target)
        .execute(&m.store.pool)
        .await?;
    example(m, &t, category, note).await?;
    let t = get(m, target).await?;
    crate::requests::sorted_by_you(m, &t).await?;
    Ok(t)
}

/// You mark a conversation's sender (or their whole domain): a pitch, so
/// everything they send is; or known, so nothing they send is.
pub async fn mark(m: &Memex, target: &str, mark: &str, whole_domain: bool) -> anyhow::Result<api::Triage> {
    anyhow::ensure!(matches!(mark, "pitch" | "known"), "a mark is pitch or known");
    let t = get(m, target).await?;
    anyhow::ensure!(t.kind == "mail" && t.sender.contains('@'), "only a mail sender can be marked");
    let pattern = if whole_domain { format!("@{}", domain(&t.sender)) } else { t.sender.clone() };
    sqlx::query(
        "INSERT INTO sender_marks (pattern, mark, created_at) VALUES (?, ?, ?) ON CONFLICT(pattern) DO UPDATE SET mark = excluded.mark",
    )
    .bind(&pattern)
    .bind(mark)
    .bind(c6_core::now_ms())
    .execute(&m.store.pool)
    .await?;
    let like = if whole_domain { format!("%{pattern}") } else { pattern.clone() };
    if mark == "pitch" {
        sqlx::query("UPDATE triage SET category = 'pitch', by = 'mark', reason = 'A sender you marked as a pitch' WHERE kind = 'mail' AND sender LIKE ?")
            .bind(&like)
            .execute(&m.store.pool)
            .await?;
        sqlx::query("UPDATE requests SET status = 'dropped', closed_by = 'pitch', updated_at = ? WHERE person LIKE ? AND status IN ('open', 'snoozed')")
            .bind(c6_core::now_ms())
            .bind(&like)
            .execute(&m.store.pool)
            .await?;
        example(m, &t, "pitch", "").await?;
    } else {
        // Sorted again, as someone you know.
        sqlx::query("DELETE FROM triage WHERE kind = 'mail' AND sender LIKE ? AND category IN ('pitch', 'first_contact')")
            .bind(&like)
            .execute(&m.store.pool)
            .await?;
        example(m, &t, "not a pitch", "a sender you know").await?;
    }
    m.wake.notify_waiters();
    Ok(get(m, target).await.unwrap_or(t))
}

/// The senders you marked.
pub async fn marks(m: &Memex) -> anyhow::Result<Value> {
    let rows: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT pattern, mark, created_at FROM sender_marks ORDER BY created_at DESC").fetch_all(&m.store.pool).await?;
    Ok(json!(rows.into_iter().map(|(p, k, at)| json!({"pattern": p, "mark": k, "created_at": at})).collect::<Vec<_>>()))
}

/// Takes a mark back; what it sorted is sorted again.
pub async fn unmark(m: &Memex, pattern: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM sender_marks WHERE pattern = ?").bind(pattern).execute(&m.store.pool).await?;
    let like = if pattern.starts_with('@') { format!("%{pattern}") } else { pattern.to_owned() };
    sqlx::query("DELETE FROM triage WHERE kind = 'mail' AND sender LIKE ? AND by = 'mark'").bind(like).execute(&m.store.pool).await?;
    m.wake.notify_waiters();
    Ok(())
}

pub async fn method(m: &Memex, method: &str, params: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
    let opt = |k: &str| params[k].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    Ok(match method {
        "memex.triage.list" => {
            let limit = params["limit"].as_u64().unwrap_or(100) as u32;
            json!(list(m, opt("category").as_deref(), opt("kind").as_deref(), params["unseen"] == true, limit).await?)
        }
        "memex.triage.seen" => {
            seen(m, &s("target")).await?;
            json!({})
        }
        "memex.triage.counts" => counts(m).await?,
        "memex.triage.status" => status(m).await?,
        "memex.triage.get" => {
            let mut sorting = with_request(m, &s("target")).await?;
            if let Some(t) = &mut sorting
                && t.kind == "mail"
            {
                let mine: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
                t.known = Some(mark_of(m, &t.sender).await?.as_deref() == Some("known") || known(m, &t.sender, &mine).await?);
            }
            json!(sorting)
        }
        "memex.triage.set" => json!(set(m, &s("target"), &s("category"), &s("note")).await?),
        "memex.triage.mark" => json!(mark(m, &s("target"), &s("mark"), params["domain"] == true).await?),
        "memex.triage.marks" => marks(m).await?,
        "memex.triage.unmark" => {
            unmark(m, &s("pattern")).await?;
            json!({})
        }
        other => anyhow::bail!("the memex has no method {other}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sorting_carries_whether_the_sender_is_known_now() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        m.accounts.lock().unwrap().push(crate::people::Account { email: "me@mine.example".into(), ..Default::default() });
        let sorted = Sorted::new("fyi", "An update", "model");
        let senders = [
            ("ana@company.example", true),
            ("bo@company.example", true),
            ("friend@gmail.com", true),
            ("stranger@gmail.com", false),
            ("colleague@mine.example", true),
            ("contact@example.net", true),
            ("guest@example.net", true),
            ("unknown@example.org", false),
        ];
        for (sender, _) in senders {
            put(&m, &format!("thread:acct:{sender}"), "mail", &sorted, sender, "", "Update", c6_core::now_ms()).await.unwrap();
        }
        assert_eq!(method(&m, "memex.triage.get", &json!({"target": "thread:acct:ana@company.example"})).await.unwrap()["known"], false);
        let sent = crate::mail::Parsed {
            message_id: "sent@mine.example".into(),
            from: ("me@mine.example".into(), "Me".into()),
            to: vec![("ana@company.example".into(), "Ana".into()), ("friend@gmail.com".into(), "Friend".into())],
            text: "Here are the notes.".into(),
            ..Default::default()
        };
        let at = crate::mail::Stored {
            account: "acct",
            external_id: "sent",
            thread_id: "sent",
            source: "test-mail",
            rfc_id: &sent.message_id,
            origin: "mirrored",
            owner: "me@mine.example",
            message: "sent",
        };
        crate::mail::store(&m, &sent, "", &at, &["SENT".into()], c6_core::now_ms()).await.unwrap();
        crate::contacts::store(
            &m,
            "acct",
            &json!({"resourceName": "people/contact", "emailAddresses": [{"value": "contact@example.net"}]}),
        )
        .await
        .unwrap();
        crate::calendar::store_event(
            &m,
            "test-calendar",
            "acct",
            "me@mine.example",
            "primary",
            &json!({
                "id": "meeting", "attendees": [{"email": "guest@example.net"}],
                "start": {"dateTime": "2026-01-02T10:00:00Z"}, "end": {"dateTime": "2026-01-02T11:00:00Z"},
            }),
        )
        .await
        .unwrap();
        let all = list(&m, None, Some("mail"), false, 100).await.unwrap();
        for (sender, expected) in senders {
            let t: api::Triage =
                serde_json::from_value(method(&m, "memex.triage.get", &json!({"target": format!("thread:acct:{sender}")})).await.unwrap())
                    .unwrap();
            let thread = of_thread(&m, "acct", sender).await.unwrap().unwrap();
            assert_eq!(thread.known, None, "{sender}");
            assert!(serde_json::to_value(&thread).unwrap().get("known").is_none(), "{sender}");
            assert_eq!(t.known, Some(expected), "{sender}");
            assert_eq!(serde_json::to_value(&t).unwrap()["known"], expected, "{sender}");
            let listed = all.iter().find(|t| t.sender == sender).unwrap();
            assert_eq!(listed.known, None, "{sender}");
            assert!(serde_json::to_value(listed).unwrap().get("known").is_none(), "{sender}");
        }
        sqlx::query("INSERT INTO sender_marks (pattern, mark, created_at) VALUES (?, 'known', ?)")
            .bind("unknown@example.org")
            .bind(c6_core::now_ms())
            .execute(&m.store.pool)
            .await
            .unwrap();
        assert_eq!(method(&m, "memex.triage.get", &json!({"target": "thread:acct:unknown@example.org"})).await.unwrap()["known"], true);
    }
}
