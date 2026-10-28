//! Requests (docs/brief.md §5): what people need from you, one per ask,
//! from whichever conversations it came in. Triage says a conversation
//! needs you (or is a real first contact) and what's asked; the request it
//! belongs to is the one already open from that conversation, else one of
//! the same person's open requests when the model says it's the same ask
//! (unsure: both, marked as possibly the same), else a new one. A request
//! closes when you answer in any of its conversations, or one of them is
//! settled without you.

use c6_api as api;
use c6_llm::Block;
use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;
use crate::llm::{self, Task};
use crate::store::clip;

/// What triage found in a conversation, for its request.
pub struct Found<'a> {
    pub target: &'a str,
    pub kind: &'a str,
    pub category: &'a str,
    pub reason: &'a str,
    pub asks: &'a str,
    pub due: Option<i64>,
    pub person: &'a str,
    pub person_name: &'a str,
    pub title: &'a str,
    pub at: i64,
    /// Urgent by your rule: you're told at once.
    pub urgent: bool,
}

/// Open, or snoozed until a time that has passed.
const OPEN: &str = "(status = 'open' OR (status = 'snoozed' AND until <= ?))";

/// The open request (or one snoozed until a time that has passed) a
/// conversation is part of, if any: the latest.
pub async fn open_of(m: &Memex, target: &str) -> anyhow::Result<Option<i64>> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT r.id FROM requests r JOIN request_sources s ON s.request_id = r.id WHERE s.target = ? AND {OPEN} ORDER BY r.updated_at DESC LIMIT 1"
    )))
    .bind(target)
    .bind(c6_core::now_ms())
    .fetch_optional(&m.store.pool)
    .await?)
}

/// Keeps a conversation's request up to date with how it was just sorted.
pub async fn follow(m: &Memex, f: &Found<'_>, task: Option<&Task>) -> anyhow::Result<()> {
    let now = c6_core::now_ms();
    let mine: Vec<i64> = sqlx::query_scalar(
        "SELECT r.id FROM requests r JOIN request_sources s ON s.request_id = r.id WHERE s.target = ? AND r.status IN ('open', 'snoozed')",
    )
    .bind(f.target)
    .fetch_all(&m.store.pool)
    .await?;
    if !matches!(f.category, "needs_you" | "first_contact") {
        // Answered, or settled without you.
        let (status, by) = match f.category {
            "yours" => ("done", "replied"),
            "pitch" => ("dropped", "pitch"),
            _ => ("done", "settled"),
        };
        for id in mine {
            close(m, id, status, by).await?;
        }
        return Ok(());
    }
    let title = if f.asks.trim().is_empty() { format!("Reply about \"{}\"", f.title) } else { clip(f.asks.trim(), 200) };
    if let Some(id) = mine.first() {
        sqlx::query("UPDATE requests SET title = ?, due = COALESCE(?, due), reason = ?, category = ?, updated_at = ? WHERE id = ?")
            .bind(&title)
            .bind(f.due)
            .bind(f.reason)
            .bind(f.category)
            .bind(now)
            .bind(id)
            .execute(&m.store.pool)
            .await?;
        source(m, *id, f).await?;
        return urgent(m, *id, f).await;
    }
    // The same person's other open requests: is this one of them?
    let theirs: Vec<(i64, String)> = if f.person.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, title FROM requests WHERE person = ? AND {OPEN} ORDER BY updated_at DESC LIMIT 3"
        )))
        .bind(f.person)
        .bind(now)
        .fetch_all(&m.store.pool)
        .await?
    };
    let mut maybe = None;
    if let (Some((id, open)), Some(task)) = (theirs.first(), task) {
        let (_, per_day) = m.understanding().unwrap_or((task.clone(), 0));
        if crate::understand::can_call(m, task, per_day).await? {
            let new = format!("{title} (in \"{}\", {})", f.title, f.kind);
            let ask = api::prompts::fill(&m.prompt("memex.requests.same", api::prompts::REQUEST_SAME), &[("open", open), ("new", &new)]);
            let system = m.prompt("memex.understand.system", api::prompts::UNDERSTAND_SYSTEM);
            crate::understand::count_call(m).await?;
            let v = llm::json_of(&llm::ask(m, task, "same request?", &new, &system, vec![Block::Text(ask)], 200).await?)?;
            match (v["same"] == true, v["sure"] == true) {
                (true, true) => {
                    sqlx::query("UPDATE requests SET due = COALESCE(due, ?), updated_at = ? WHERE id = ?")
                        .bind(f.due)
                        .bind(now)
                        .bind(id)
                        .execute(&m.store.pool)
                        .await?;
                    source(m, *id, f).await?;
                    return urgent(m, *id, f).await;
                }
                (true, false) => maybe = Some(*id),
                _ => {}
            }
        }
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO requests (person, person_name, title, due, category, reason, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(f.person)
    .bind(f.person_name)
    .bind(&title)
    .bind(f.due)
    .bind(f.category)
    .bind(f.reason)
    .bind(now)
    .bind(now)
    .fetch_one(&m.store.pool)
    .await?;
    source(m, id, f).await?;
    if let Some(other) = maybe {
        sqlx::query("INSERT OR IGNORE INTO request_maybe (a, b) VALUES (?, ?)").bind(other).bind(id).execute(&m.store.pool).await?;
    }
    // For triggers (request.opened).
    let from = if f.person_name.is_empty() { f.person.to_owned() } else { format!("{} <{}>", f.person_name, f.person) };
    m.emit(
        "request.opened",
        json!({"request_id": id, "from": from, "subject": title, "reason": f.reason, "category": f.category, "due": f.due, "sources": [f.target]}),
    )
    .await;
    urgent(m, id, f).await
}

/// An urgent request tells you at once (once), if the realm's Brief setting
/// allows; the rest wait for the brief.
async fn urgent(m: &Memex, id: i64, f: &Found<'_>) -> anyhow::Result<()> {
    if !f.urgent || f.category != "needs_you" || !m.brief_setting.lock().unwrap().urgent {
        return Ok(());
    }
    let told = sqlx::query("UPDATE requests SET notified_at = ? WHERE id = ? AND notified_at IS NULL")
        .bind(c6_core::now_ms())
        .bind(id)
        .execute(&m.store.pool)
        .await?
        .rows_affected();
    if told == 0 {
        return Ok(());
    }
    let r = get(m, id).await?;
    let who = if r.person_name.is_empty() { r.person.clone() } else { r.person_name.clone() };
    crate::briefs::notify(m, &format!("{who}: {}", r.title), &r.reason, "high").await;
    Ok(())
}

async fn source(m: &Memex, id: i64, f: &Found<'_>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO request_sources (request_id, target, kind, at, noted_at) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT DO UPDATE SET at = excluded.at, noted_at = excluded.noted_at",
    )
    .bind(id)
    .bind(f.target)
    .bind(f.kind)
    .bind(f.at)
    .bind(c6_core::now_ms())
    .execute(&m.store.pool)
    .await?;
    Ok(())
}

async fn close(m: &Memex, id: i64, status: &str, by: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE requests SET status = ?, closed_by = ?, until = NULL, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(by)
        .bind(c6_core::now_ms())
        .bind(id)
        .execute(&m.store.pool)
        .await?;
    sqlx::query("DELETE FROM request_maybe WHERE a = ?1 OR b = ?1").bind(id).execute(&m.store.pool).await?;
    Ok(())
}

/// Requests, the latest first: open ones (and snoozed ones whose time has
/// come), or of a status.
pub async fn list(m: &Memex, status: &str, limit: u32) -> anyhow::Result<Vec<api::Request>> {
    let now = c6_core::now_ms();
    let rows = if status == "open" {
        sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM requests WHERE {OPEN} ORDER BY due IS NULL, due, updated_at DESC LIMIT ?")))
            .bind(now)
            .bind(limit.clamp(1, 500) as i64)
            .fetch_all(&m.store.pool)
            .await?
    } else {
        sqlx::query("SELECT * FROM requests WHERE status = ? ORDER BY updated_at DESC LIMIT ?")
            .bind(status)
            .bind(limit.clamp(1, 500) as i64)
            .fetch_all(&m.store.pool)
            .await?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(request(m, &r).await?);
    }
    Ok(out)
}

async fn request(m: &Memex, r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<api::Request> {
    let id: i64 = r.get("id");
    let sources = sqlx::query(
        "SELECT s.target, s.kind, s.at, COALESCE(t.title, '') AS title FROM request_sources s LEFT JOIN triage t ON t.target = s.target \
         WHERE s.request_id = ? ORDER BY s.at",
    )
    .bind(id)
    .fetch_all(&m.store.pool)
    .await?
    .iter()
    .map(|s| api::RequestSource { target: s.get("target"), kind: s.get("kind"), at: s.get("at"), title: s.get("title") })
    .collect();
    let maybe: Vec<i64> = sqlx::query_scalar("SELECT CASE WHEN a = ?1 THEN b ELSE a END FROM request_maybe WHERE a = ?1 OR b = ?1")
        .bind(id)
        .fetch_all(&m.store.pool)
        .await?;
    Ok(api::Request {
        id,
        person: r.get("person"),
        person_name: r.get("person_name"),
        title: r.get("title"),
        due: r.get("due"),
        category: r.get("category"),
        status: r.get("status"),
        until: r.get("until"),
        reason: r.get("reason"),
        closed_by: r.get("closed_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        sources,
        maybe_same: maybe,
        draft: r.get::<Option<String>, _>("draft").and_then(|d| serde_json::from_str(&d).ok()),
        notified_at: r.get("notified_at"),
    })
}

pub async fn get(m: &Memex, id: i64) -> anyhow::Result<api::Request> {
    let r = sqlx::query("SELECT * FROM requests WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?;
    match r {
        Some(r) => request(m, &r).await,
        None => anyhow::bail!("no request {id}"),
    }
}

/// You settle a request: done, dropped, snoozed until a time, or open
/// again.
pub async fn set(m: &Memex, id: i64, status: &str, until: Option<i64>) -> anyhow::Result<api::Request> {
    match status {
        "done" | "dropped" => close(m, id, status, "you").await?,
        "snoozed" => {
            let until = until.ok_or_else(|| anyhow::anyhow!("snoozed until when?"))?;
            sqlx::query("UPDATE requests SET status = 'snoozed', until = ?, updated_at = ? WHERE id = ?")
                .bind(until)
                .bind(c6_core::now_ms())
                .bind(id)
                .execute(&m.store.pool)
                .await?;
        }
        "open" => {
            sqlx::query("UPDATE requests SET status = 'open', until = NULL, closed_by = NULL, updated_at = ? WHERE id = ?")
                .bind(c6_core::now_ms())
                .bind(id)
                .execute(&m.store.pool)
                .await?;
        }
        other => anyhow::bail!("a request is open, snoozed, done or dropped, not {other:?}"),
    }
    get(m, id).await
}

/// Two requests that are one: `from`'s conversations join `into`.
pub async fn merge(m: &Memex, into: i64, from: i64) -> anyhow::Result<api::Request> {
    anyhow::ensure!(into != from, "a request is already itself");
    get(m, into).await?;
    get(m, from).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO request_sources (request_id, target, kind, at, noted_at) SELECT ?, target, kind, at, noted_at \
         FROM request_sources WHERE request_id = ?",
    )
    .bind(into)
    .bind(from)
    .execute(&m.store.pool)
    .await?;
    sqlx::query("DELETE FROM request_sources WHERE request_id = ?").bind(from).execute(&m.store.pool).await?;
    sqlx::query("DELETE FROM request_maybe WHERE a = ?1 OR b = ?1").bind(from).execute(&m.store.pool).await?;
    sqlx::query("DELETE FROM requests WHERE id = ?").bind(from).execute(&m.store.pool).await?;
    sqlx::query("UPDATE requests SET updated_at = ? WHERE id = ?").bind(c6_core::now_ms()).bind(into).execute(&m.store.pool).await?;
    get(m, into).await
}

/// A conversation you sorted yourself: its request follows (no model).
pub async fn sorted_by_you(m: &Memex, t: &api::Triage) -> anyhow::Result<()> {
    let f = Found {
        target: &t.target,
        kind: &t.kind,
        category: &t.category,
        reason: &t.reason,
        asks: "",
        due: None,
        person: &t.sender,
        person_name: &t.sender_name,
        title: &t.title,
        at: t.at,
        urgent: false,
    };
    follow(m, &f, None).await
}

pub async fn method(m: &Memex, method: &str, params: &Value) -> anyhow::Result<Value> {
    let id = params["id"].as_i64().unwrap_or(0);
    Ok(match method {
        "memex.requests.list" => {
            json!(list(m, params["status"].as_str().unwrap_or("open"), params["limit"].as_u64().unwrap_or(100) as u32).await?)
        }
        "memex.requests.get" => json!(get(m, id).await?),
        "memex.requests.set" => json!(set(m, id, params["status"].as_str().unwrap_or_default(), params["until"].as_i64()).await?),
        "memex.requests.merge" => json!(merge(m, params["into"].as_i64().unwrap_or(0), params["from"].as_i64().unwrap_or(0)).await?),
        "memex.requests.draft" => {
            let (task, _) = m.understanding().ok_or_else(|| anyhow::anyhow!("choose an understanding model first (Settings → Memex)"))?;
            json!(crate::drafts::draft(m, &task, id).await?)
        }
        "memex.requests.reply_to" => crate::drafts::reply_to(m, params["target"].as_str().unwrap_or_default()).await?,
        "memex.requests.send" => {
            crate::drafts::send(m, id, params["target"].as_str().unwrap_or_default(), params["text"].as_str().unwrap_or_default()).await?
        }
        other => anyhow::bail!("the memex has no method {other}"),
    })
}
