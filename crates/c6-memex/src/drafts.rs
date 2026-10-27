//! Drafts (docs/brief.md §6): for every open request that needs you, a
//! reply waits before you open it. The model reads the request's
//! conversations, what else you and that person have said lately, what's
//! open between you, when you're free, and how you write; it answers where
//! the request's substance is, and leaves what only you can decide as
//! choices (`[[one | other]]`). Drafted again when a conversation moves on.
//! Sending is yours: from the brief, through the mail or Slack app (mail
//! with its undo window).

use c6_api as api;
use c6_llm::Block;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::warn;

use crate::Memex;
use crate::llm;
use crate::store::clip;

const MAX_CONTEXT: usize = 16_000;

/// Writes one more draft, if a request needs one and there's a model and
/// budget. Whether it did.
pub async fn step(m: &Memex) -> anyhow::Result<bool> {
    let Some((task, per_day)) = m.understanding() else { return Ok(false) };
    if crate::understand::budget_left(m, per_day).await? == 0 {
        return Ok(false);
    }
    // Needs you, open, never drafted or drafted before its news; the soonest
    // due first, then the newest.
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT r.id FROM requests r WHERE r.category = 'needs_you' AND (r.status = 'open' OR (r.status = 'snoozed' AND r.until <= ?1)) \
         AND (r.drafted_at IS NULL OR r.drafted_at < (SELECT MAX(noted_at) FROM request_sources s WHERE s.request_id = r.id)) \
         ORDER BY r.due IS NULL, r.due, r.updated_at DESC LIMIT 1",
    )
    .bind(c6_core::now_ms())
    .fetch_optional(&m.store.pool)
    .await?;
    let Some(id) = id else { return Ok(false) };
    if let Err(e) = draft(m, &task, id).await {
        warn!(id, "drafting: {e:#}");
        // Not again until the conversation has news.
        sqlx::query("UPDATE requests SET draft = ?, drafted_at = ? WHERE id = ?")
            .bind(json!({"error": format!("{e:#}")}).to_string())
            .bind(c6_core::now_ms())
            .bind(id)
            .execute(&m.store.pool)
            .await?;
    }
    Ok(true)
}

/// Drafts (again) a request's reply now.
pub async fn draft(m: &Memex, task: &llm::Task, id: i64) -> anyhow::Result<api::RequestDraft> {
    let r = crate::requests::get(m, id).await?;
    let context = context(m, &r).await?;
    let template = m.prompt("memex.requests.draft", api::prompts::REQUEST_DRAFT);
    let request = format!(
        "{} (from {} <{}>{})",
        r.title,
        r.person_name,
        r.person,
        r.due.and_then(chrono::DateTime::from_timestamp_millis).map(|d| format!(", by {}", d.format("%Y-%m-%d"))).unwrap_or_default()
    );
    let prompt = api::prompts::fill(&template, &[("request", &request), ("context", &clip(&context, MAX_CONTEXT))]);
    let system = m.prompt("memex.understand.system", api::prompts::UNDERSTAND_SYSTEM);
    crate::understand::count_call(m).await?;
    let v = llm::json_of(&llm::ask(m, task, "draft", &r.title, &system, vec![Block::Text(prompt)], 1500).await?)?;
    let text = v["text"].as_str().unwrap_or("").trim().to_owned();
    anyhow::ensure!(!text.is_empty(), "the model wrote no draft");
    // Where to answer: one of its conversations (the latest, if the model
    // named none of them).
    let target = v["target"]
        .as_str()
        .filter(|t| r.sources.iter().any(|s| s.target == *t))
        .map(str::to_owned)
        .or_else(|| r.sources.last().map(|s| s.target.clone()))
        .unwrap_or_default();
    let kind = if target.starts_with("chat:") { "chat" } else { "mail" };
    let d = api::RequestDraft {
        target,
        kind: kind.into(),
        text,
        note: v["note"].as_str().unwrap_or("").trim().to_owned(),
        error: None,
        at: c6_core::now_ms(),
    };
    sqlx::query("UPDATE requests SET draft = ?, drafted_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&d)?)
        .bind(d.at)
        .bind(id)
        .execute(&m.store.pool)
        .await?;
    Ok(d)
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
}

/// What the model reads to draft: the conversations, recent exchanges with
/// the person, what's open between you, when you're free, your voice.
async fn context(m: &Memex, r: &api::Request) -> anyhow::Result<String> {
    let mut out = String::from("## The request's conversations (answer in one of them, by its target)\n");
    for s in &r.sources {
        out.push_str(&format!("\n### target: {} ({})\n", s.target, s.kind));
        out.push_str(&conversation(m, &s.target).await?);
    }
    let person = r.person.to_lowercase();
    if person.contains('@') {
        // Who they are to the owner.
        let cards = crate::cards::of(m, &person).await?;
        if !cards.is_empty() {
            out.push_str("\n## Who they are to you\n");
            for c in &cards {
                out.push_str(&format!("- {}\n", crate::cards::text(c)));
            }
        }
        // Other threads with them lately.
        let threads: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT subject, snippet, MAX(date) FROM mail WHERE (from_addr = ?1 OR to_json LIKE ?2) AND trash = 0 AND spam = 0 \
             GROUP BY account, thread_id ORDER BY MAX(date) DESC LIMIT 6",
        )
        .bind(&person)
        .bind(format!("%\"{person}\"%"))
        .fetch_all(&m.store.pool)
        .await?;
        if !threads.is_empty() {
            out.push_str("\n## Other recent mail with them\n");
            for (subject, snippet, at) in threads {
                out.push_str(&format!("- {} ({}): {}\n", subject, date(at), clip(&snippet, 200)));
            }
        }
    }
    let open: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT who, what, due FROM commitments WHERE status = 'open' AND (lower(who) = ?1 OR item_id IN \
         (SELECT item_id FROM mail WHERE from_addr = ?1 OR to_json LIKE ?2)) ORDER BY due IS NULL, due LIMIT 10",
    )
    .bind(&person)
    .bind(format!("%\"{person}\"%"))
    .fetch_all(&m.store.pool)
    .await?;
    if !open.is_empty() {
        out.push_str("\n## Open between you\n");
        for (who, what, due) in open {
            let who = if who == "me" { "the owner".to_owned() } else { who };
            out.push_str(&format!("- {who}: {what}{}\n", due.map(|d| format!(" (due {})", date(d))).unwrap_or_default()));
        }
    }
    out.push_str(&free(m).await?);
    out.push_str(&voice(m, &person).await?);
    Ok(out)
}

/// A conversation's last messages, as the model reads them.
async fn conversation(m: &Memex, target: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    if let Some(rest) = target.strip_prefix("thread:") {
        let (account, thread) = rest.rsplit_once(':').unwrap_or_default();
        let rows = sqlx::query(
            "SELECT m.from_addr, m.from_name, m.subject, m.date, m.sent, i.body FROM mail m JOIN items i ON i.id = m.item_id \
             WHERE m.account = ? AND m.thread_id = ? AND m.draft = 0 AND m.trash = 0 ORDER BY m.date DESC LIMIT 6",
        )
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?;
        for r in rows.iter().rev() {
            let who = if r.get::<i64, _>("sent") == 1 {
                "the owner".to_owned()
            } else {
                format!("{} <{}>", r.get::<String, _>("from_name"), r.get::<String, _>("from_addr"))
            };
            out.push_str(&format!(
                "--- {who}, {} — {}\n{}\n",
                date(r.get("date")),
                r.get::<String, _>("subject"),
                clip(&r.get::<String, _>("body"), 3000)
            ));
        }
        return Ok(out);
    }
    let parts: Vec<&str> = target.strip_prefix("chat:").unwrap_or_default().splitn(3, ':').collect();
    let f = |k: &str| format!("json_extract(meta, '$.{k}')");
    let sql = match parts.get(2) {
        Some(_) => format!(
            "SELECT body, meta, created_at FROM items WHERE kind = 'chat' AND {} = ?1 AND {} = ?2 AND ({} = ?3 OR {} = ?3) \
             ORDER BY created_at DESC LIMIT 20",
            f("workspace"),
            f("channel"),
            f("thread_ts"),
            f("ts")
        ),
        None => format!(
            "SELECT body, meta, created_at FROM items WHERE kind = 'chat' AND {} = ?1 AND {} = ?2 AND {} IS NULL \
             ORDER BY created_at DESC LIMIT 20",
            f("workspace"),
            f("channel"),
            f("thread_ts")
        ),
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(parts.first().copied().unwrap_or_default())
        .bind(parts.get(1).copied().unwrap_or_default())
        .bind(parts.get(2).copied().unwrap_or_default())
        .fetch_all(&m.store.pool)
        .await?;
    for r in rows.iter().rev() {
        let meta: Value = serde_json::from_str(&r.get::<String, _>("meta")).unwrap_or_default();
        let who = if meta["from_me"] == true { "the owner".to_owned() } else { meta["user_name"].as_str().unwrap_or("someone").to_owned() };
        let body: String = r.get("body");
        let said = body.split_once(": ").map_or(body.as_str(), |(_, t)| t);
        out.push_str(&format!("--- {who}, {}: {}\n", date(r.get("created_at")), clip(said, 1500)));
    }
    Ok(out)
}

/// When you're busy over the next week, in your calendar's time zone.
async fn free(m: &Memex) -> anyhow::Result<String> {
    let now = c6_core::now_ms();
    let busy = crate::calendar::busy(m, now, now + 7 * 86_400_000).await.unwrap_or_default();
    let zone: Option<String> =
        sqlx::query_scalar("SELECT time_zone FROM calendars WHERE is_primary = 1 LIMIT 1").fetch_optional(&m.store.pool).await?;
    let tz: chrono_tz::Tz = zone.as_deref().and_then(|z| z.parse().ok()).unwrap_or(chrono_tz::UTC);
    let at = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.with_timezone(&tz).format("%a %Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    };
    let mut out = format!("\n## The owner's calendar, next 7 days ({}; busy times, the rest is free)\n", tz.name());
    if busy.is_empty() {
        out.push_str("- nothing booked\n");
    }
    for (s, e) in busy.iter().take(40) {
        out.push_str(&format!("- busy {} to {}\n", at(*s), at(*e)));
    }
    Ok(out)
}

/// How you write: your last few messages to them, else to anyone.
async fn voice(m: &Memex, person: &str) -> anyhow::Result<String> {
    let mut rows: Vec<String> = sqlx::query_scalar(
        "SELECT i.body FROM mail m JOIN items i ON i.id = m.item_id WHERE m.sent = 1 AND m.to_json LIKE ? ORDER BY m.date DESC LIMIT 3",
    )
    .bind(format!("%\"{person}\"%"))
    .fetch_all(&m.store.pool)
    .await?;
    if rows.is_empty() {
        rows =
            sqlx::query_scalar("SELECT i.body FROM mail m JOIN items i ON i.id = m.item_id WHERE m.sent = 1 ORDER BY m.date DESC LIMIT 3")
                .fetch_all(&m.store.pool)
                .await?;
    }
    let chat: Vec<String> = sqlx::query_scalar(
        "SELECT body FROM items WHERE kind = 'chat' AND json_extract(meta, '$.from_me') = 1 ORDER BY created_at DESC LIMIT 5",
    )
    .fetch_all(&m.store.pool)
    .await?;
    if rows.is_empty() && chat.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::from("\n## How the owner writes (their recent messages)\n");
    for b in rows {
        // Without the headers and the quoted thread.
        let body = b.splitn(4, '\n').last().unwrap_or("");
        let mine: Vec<&str> = body.lines().take_while(|l| !l.starts_with('>') && !l.starts_with("On ")).collect();
        out.push_str(&format!("---\n{}\n", clip(mine.join("\n").trim(), 800)));
    }
    for c in chat {
        out.push_str(&format!("--- (chat) {}\n", clip(c.split_once(": ").map_or(c.as_str(), |(_, t)| t), 300)));
    }
    Ok(out)
}

/// Who and what a reply in a conversation goes to: the mail it answers
/// (and how), or the chat conversation or thread.
pub async fn reply_to(m: &Memex, target: &str) -> anyhow::Result<Value> {
    if let Some(rest) = target.strip_prefix("thread:") {
        let (account, thread) = rest.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("not a thread"))?;
        let last = sqlx::query(
            "SELECT item_id, from_addr, from_name, subject FROM mail WHERE account = ? AND thread_id = ? AND sent = 0 AND draft = 0 \
             ORDER BY date DESC LIMIT 1",
        )
        .bind(account)
        .bind(thread)
        .fetch_optional(&m.store.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("nobody else wrote in that thread"))?;
        let subject: String = last.get("subject");
        let subject = if subject.to_lowercase().starts_with("re:") { subject } else { format!("Re: {subject}") };
        return Ok(json!({
            "kind": "mail", "account": account, "to": [last.get::<String, _>("from_addr")],
            "to_name": last.get::<String, _>("from_name"), "subject": subject, "reply_to": last.get::<String, _>("item_id"),
        }));
    }
    let parts: Vec<&str> = target.strip_prefix("chat:").unwrap_or_default().splitn(3, ':').collect();
    anyhow::ensure!(parts.len() >= 2, "not a conversation");
    Ok(json!({"kind": "chat", "workspace": parts[0], "channel": parts[1], "thread_ts": parts.get(2)}))
}

/// Sends a reply you approved (and edited) in one of a request's
/// conversations; the request is done.
pub async fn send(m: &Memex, id: i64, target: &str, text: &str) -> anyhow::Result<Value> {
    let r = crate::requests::get(m, id).await?;
    anyhow::ensure!(r.sources.iter().any(|s| s.target == target), "that conversation isn't one of the request's");
    anyhow::ensure!(!text.trim().is_empty(), "nothing to send");
    anyhow::ensure!(!(text.contains("[[") && text.contains("]]")), "choose first: the draft still has choices in [[…]]");
    let to = reply_to(m, target).await?;
    let sent = if to["kind"] == "mail" {
        let d = api::MailDraft {
            account: to["account"].as_str().unwrap_or_default().to_owned(),
            to: serde_json::from_value(to["to"].clone())?,
            subject: to["subject"].as_str().unwrap_or_default().to_owned(),
            body: text.to_owned(),
            reply_to: to["reply_to"].as_str().map(str::to_owned),
            ..Default::default()
        };
        json!(crate::mail::queue(m, &d, "you").await?)
    } else {
        let mut p = json!({"workspace": to["workspace"], "channel": to["channel"], "text": text, "made_by": "you"});
        if let Some(t) = to["thread_ts"].as_str() {
            p["thread_ts"] = json!(t);
        }
        crate::chat::method(m, "memex.chat.post", &p, &["chat".to_owned()]).await?
    };
    crate::requests::set(m, id, "done", None).await?;
    sqlx::query("UPDATE requests SET closed_by = 'replied' WHERE id = ?").bind(id).execute(&m.store.pool).await?;
    Ok(sent)
}
