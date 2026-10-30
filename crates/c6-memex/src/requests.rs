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
    /// Incoming mail item ids from the rows triage read; empty for chat.
    pub seen_mail: &'a [String],
    /// Urgent by your rule: you're told at once.
    pub urgent: bool,
    pub invitation: bool,
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
    if let Some(id) = mine.first()
        && source(m, *id, f).await?
    {
        sqlx::query("UPDATE requests SET title = ?, due = COALESCE(?, due), reason = ?, category = ?, updated_at = ? WHERE id = ? AND status IN ('open', 'snoozed')")
            .bind(&title)
            .bind(f.due)
            .bind(f.reason)
            .bind(f.category)
            .bind(now)
            .bind(id)
            .execute(&m.store.pool)
            .await?;
        if crate::drafts::close_answered(m, *id).await? {
            return Ok(());
        }
        changed(m, *id, "open").await;
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
            let v =
                llm::json_of(&llm::ask(m, task, "same request?", &new, &system, vec![Block::Text(ask)], llm::Output::Json(200)).await?)?;
            match (v["same"] == true, v["sure"] == true) {
                (true, true) if source(m, *id, f).await? => {
                    sqlx::query(
                        "UPDATE requests SET due = COALESCE(due, ?), updated_at = ? WHERE id = ? AND status IN ('open', 'snoozed')",
                    )
                    .bind(f.due)
                    .bind(now)
                    .bind(id)
                    .execute(&m.store.pool)
                    .await?;
                    if crate::drafts::close_answered(m, *id).await? {
                        return Ok(());
                    }
                    changed(m, *id, "open").await;
                    return urgent(m, *id, f).await;
                }
                (true, false) => maybe = Some(*id),
                _ => {}
            }
        }
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO requests (person, person_name, title, due, category, reason, created_at, updated_at, invitation) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(f.person)
    .bind(f.person_name)
    .bind(&title)
    .bind(f.due)
    .bind(f.category)
    .bind(f.reason)
    .bind(now)
    .bind(now)
    .bind(f.invitation)
    .fetch_one(&m.store.pool)
    .await?;
    source(m, id, f).await?;
    if crate::drafts::close_answered(m, id).await? {
        return Ok(());
    }
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

async fn source(m: &Memex, id: i64, f: &Found<'_>) -> anyhow::Result<bool> {
    let mut seen_mail = if f.kind == "mail" { f.seen_mail.to_vec() } else { Vec::new() };
    seen_mail.sort();
    let mut tx = m.store.pool.begin().await?;
    let added = sqlx::query(
        "INSERT INTO request_sources (request_id, target, kind, at, noted_at, seen_mail) SELECT ?1, ?2, ?3, ?4, ?5, ?6 \
         WHERE EXISTS (SELECT 1 FROM requests WHERE id = ?1 AND status IN ('open', 'snoozed')) \
         ON CONFLICT DO UPDATE SET at = excluded.at, noted_at = excluded.noted_at, seen_mail = excluded.seen_mail",
    )
    .bind(id)
    .bind(f.target)
    .bind(f.kind)
    .bind(f.at)
    .bind(c6_core::now_ms())
    .bind(serde_json::to_string(&seen_mail)?)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if added != 0 {
        sqlx::query("UPDATE requests SET invitation = invitation AND ? WHERE id = ?").bind(f.invitation).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(added != 0)
}

pub(crate) async fn close(m: &Memex, id: i64, status: &str, by: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE requests SET status = ?, closed_by = ?, until = NULL, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(by)
        .bind(c6_core::now_ms())
        .bind(id)
        .execute(&m.store.pool)
        .await?;
    unpair(m, id).await?;
    changed(m, id, status).await;
    Ok(())
}

pub(crate) struct CalendarAnswer {
    pub account: String,
    pub calendar: String,
    pub event: String,
    pub status: String,
    pub updated: i64,
}

// Already closed by Calendar still needs the same source and event checks;
// rechecking it leaves its original closing time alone.
pub(crate) async fn close_calendar(
    m: &Memex,
    id: i64,
    reason: &str,
    sources: &[(String, i64, i64, String)],
    event: &CalendarAnswer,
) -> anyhow::Result<bool> {
    let moved = calendar_transition(m, id, reason, sources, event, c6_core::now_ms()).await?;
    if let Some(moved) = moved {
        unpair(m, id).await?;
        // A recheck of one Calendar closed before isn't news.
        if moved {
            changed(m, id, "done").await;
        }
    }
    Ok(moved.is_some())
}

async fn calendar_transition(
    m: &Memex,
    id: i64,
    reason: &str,
    sources: &[(String, i64, i64, String)],
    event: &CalendarAnswer,
    now: i64,
) -> anyhow::Result<Option<bool>> {
    let Some(threads): Option<Vec<_>> = sources
        .iter()
        .map(|(target, _, _, _)| {
            let (account, thread) = target.strip_prefix("thread:")?.rsplit_once(':')?;
            Some((target, account, thread))
        })
        .collect()
    else {
        return Ok(None);
    };
    if threads.is_empty() {
        return Ok(None);
    }
    let mut tx = m.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let was_open: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM requests WHERE id = ? AND status IN ('open', 'snoozed'))")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let closed = sqlx::query(
        "UPDATE requests SET status = 'done', closed_by = 'calendar', reason = ?2, until = NULL, updated_at = CASE WHEN status = 'done' THEN updated_at ELSE ?3 END \
         WHERE id = ?1 AND category = 'needs_you' AND invitation = 1 \
         AND (status IN ('open', 'snoozed') OR (status = 'done' AND closed_by = 'calendar')) \
         AND ?4 = (SELECT json_group_array(json_array(target, at, noted_at, seen_mail)) FROM \
             (SELECT target, at, noted_at, seen_mail FROM request_sources WHERE request_id = ?1 ORDER BY target)) \
         AND NOT EXISTS (SELECT 1 FROM request_sources s JOIN json_each(?10) t ON s.target = json_extract(t.value, '$[0]') \
             WHERE s.request_id = ?1 AND (s.seen_mail = '[]' OR s.seen_mail != (SELECT json_group_array(item_id) FROM \
                 (SELECT item_id FROM mail WHERE account = json_extract(t.value, '$[1]') AND thread_id = json_extract(t.value, '$[2]') \
                  AND sent = 0 AND draft = 0 AND trash = 0 ORDER BY item_id)))) \
         AND EXISTS (SELECT 1 FROM events WHERE account = ?5 AND calendar_id = ?6 AND event_id = ?7 AND my_status = ?8 AND updated = ?9)",
    )
    .bind(id)
    .bind(reason)
    .bind(now)
    .bind(serde_json::to_string(sources)?)
    .bind(&event.account)
    .bind(&event.calendar)
    .bind(&event.event)
    .bind(&event.status)
    .bind(event.updated)
    .bind(serde_json::to_string(&threads)?)
    .execute(&mut *tx)
    .await?
    .rows_affected() != 0;
    tx.commit().await?;
    Ok(closed.then_some(was_open))
}

async fn unpair(m: &Memex, id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM request_maybe WHERE a = ?1 OR b = ?1").bind(id).execute(&m.store.pool).await?;
    Ok(())
}

/// For triggers and the work it belongs to (docs/work.md §3): it moved.
async fn changed(m: &Memex, id: i64, status: &str) {
    // Work may still hold any of the old ids: notify those references too,
    // without rewriting its attachments or their history.
    let ids: Vec<i64> = sqlx::query_scalar(
        "WITH RECURSIVE refs(id) AS (SELECT ? UNION SELECT r.from_id FROM request_redirects r JOIN refs ON r.into_id = refs.id) \
         SELECT id FROM refs",
    )
    .bind(id)
    .fetch_all(&m.store.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(%id, "reading merged request references: {e:#}");
        vec![id]
    });
    m.emit("request.changed", json!({"request_id": id, "request_ids": ids, "status": status})).await;
}

/// Follow merge edges in this realm's database. UNION bounds even a corrupt
/// cycle; no terminal id then means an error, not an infinite loop.
async fn request_id<'a>(db: impl sqlx::Executor<'a, Database = sqlx::Sqlite>, id: i64) -> anyhow::Result<i64> {
    let resolved = sqlx::query_scalar(
        "WITH RECURSIVE refs(id) AS (SELECT ? UNION SELECT r.into_id FROM request_redirects r JOIN refs ON r.from_id = refs.id) \
         SELECT id FROM refs WHERE NOT EXISTS (SELECT 1 FROM request_redirects r WHERE r.from_id = refs.id)",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    resolved.ok_or_else(|| anyhow::anyhow!("request {id} has a merge redirect cycle"))
}

/// Requests by id, as work lists them: who, what, how it stands (the ones
/// that are gone are left out).
pub async fn some(m: &Memex, ids: &[i64]) -> anyhow::Result<Value> {
    let mut out = Vec::new();
    for id in ids.iter().take(500) {
        let resolved = request_id(&m.store.pool, *id).await?;
        if let Some(r) = sqlx::query("SELECT person, person_name, title, status, until, updated_at FROM requests WHERE id = ?")
            .bind(resolved)
            .fetch_optional(&m.store.pool)
            .await?
        {
            out.push(json!({
                "id": id, "person": r.get::<String, _>("person"), "person_name": r.get::<String, _>("person_name"),
                "title": r.get::<String, _>("title"), "status": r.get::<String, _>("status"), "until": r.get::<Option<i64>, _>("until"),
                "updated_at": r.get::<i64, _>("updated_at"),
            }));
        }
    }
    Ok(json!(out))
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
        drafting: m.claimed(&format!("draft:{id}")),
        notified_at: r.get("notified_at"),
    })
}

pub async fn get(m: &Memex, id: i64) -> anyhow::Result<api::Request> {
    let id = request_id(&m.store.pool, id).await?;
    // If a draft lands while this is read, poll once more for its new text.
    let drafting = m.claimed(&format!("draft:{id}"));
    let r = sqlx::query("SELECT * FROM requests WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?;
    match r {
        Some(r) => {
            let mut r = request(m, &r).await?;
            r.drafting |= drafting;
            Ok(r)
        }
        None => anyhow::bail!("no request {id}"),
    }
}

/// You settle a request: done, dropped, snoozed until a time, or open
/// again.
pub async fn set(m: &Memex, id: i64, status: &str, until: Option<i64>) -> anyhow::Result<api::Request> {
    let id = request_id(&m.store.pool, id).await?;
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
            changed(m, id, "snoozed").await;
        }
        "open" => {
            sqlx::query("UPDATE requests SET status = 'open', until = NULL, closed_by = NULL, updated_at = ? WHERE id = ?")
                .bind(c6_core::now_ms())
                .bind(id)
                .execute(&m.store.pool)
                .await?;
            changed(m, id, "open").await;
        }
        other => anyhow::bail!("a request is open, snoozed, done or dropped, not {other:?}"),
    }
    get(m, id).await
}

/// Two requests that are one: `from`'s conversations join `into`.
pub async fn merge(m: &Memex, into: i64, from: i64) -> anyhow::Result<api::Request> {
    let mut tx = m.store.pool.begin().await?;
    let into = request_id(&mut *tx, into).await?;
    let from = request_id(&mut *tx, from).await?;
    anyhow::ensure!(into != from, "a request is already itself");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM requests WHERE id IN (?, ?)").bind(into).bind(from).fetch_one(&mut *tx).await?;
    anyhow::ensure!(count == 2, "no such request to merge");
    sqlx::query("INSERT INTO request_redirects (from_id, into_id, merged_at) VALUES (?, ?, ?)")
        .bind(from)
        .bind(into)
        .bind(c6_core::now_ms())
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE requests SET invitation = invitation AND (SELECT invitation FROM requests WHERE id = ?) WHERE id = ?")
        .bind(from)
        .bind(into)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO request_sources (request_id, target, kind, at, noted_at, seen_mail) SELECT ?, target, kind, at, noted_at, seen_mail \
         FROM request_sources WHERE request_id = ?",
    )
    .bind(into)
    .bind(from)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM request_sources WHERE request_id = ?").bind(from).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM request_maybe WHERE a = ?1 OR b = ?1").bind(from).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM requests WHERE id = ?").bind(from).execute(&mut *tx).await?;
    sqlx::query("UPDATE requests SET updated_at = ? WHERE id = ?").bind(c6_core::now_ms()).bind(into).execute(&mut *tx).await?;
    tx.commit().await?;
    let request = get(m, into).await?;
    changed(m, into, &request.status).await;
    Ok(request)
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
        seen_mail: &[],
        urgent: false,
        invitation: false,
    };
    follow(m, &f, None).await
}

pub async fn method(m: &Memex, method: &str, params: &Value) -> anyhow::Result<Value> {
    let id = params["id"].as_i64().unwrap_or(0);
    let id = if id == 0 { id } else { request_id(&m.store.pool, id).await? };
    Ok(match method {
        "memex.requests.list" => {
            json!(list(m, params["status"].as_str().unwrap_or("open"), params["limit"].as_u64().unwrap_or(100) as u32).await?)
        }
        "memex.requests.get" => json!(get(m, id).await?),
        "memex.requests.some" => {
            let ids: Vec<i64> = params["ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
            some(m, &ids).await?
        }
        "memex.requests.set" => json!(set(m, id, params["status"].as_str().unwrap_or_default(), params["until"].as_i64()).await?),
        "memex.requests.merge" => json!(merge(m, params["into"].as_i64().unwrap_or(0), params["from"].as_i64().unwrap_or(0)).await?),
        "memex.requests.draft" => json!(crate::drafts::start(m, id).await?),
        "memex.requests.reply_to" => crate::drafts::reply_to(m, params["target"].as_str().unwrap_or_default()).await?,
        "memex.requests.send" => {
            crate::drafts::send(m, id, params["target"].as_str().unwrap_or_default(), params["text"].as_str().unwrap_or_default()).await?
        }
        other => anyhow::bail!("the memex has no method {other}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merge_redirects_keep_old_ids_sources_and_history_across_reopening() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        for id in 1..=3 {
            sqlx::query("INSERT INTO requests (id, person, title, category, created_at, updated_at) VALUES (?, 'pat@example.net', ?, 'needs_you', 0, 0)")
                .bind(id).bind(format!("Request {id}")).execute(&m.store.pool).await.unwrap();
            sqlx::query("INSERT INTO request_sources (request_id, target, kind, at) VALUES (?, ?, 'mail', ?)")
                .bind(id)
                .bind(format!("thread:import:test:thread-{id}"))
                .bind(id)
                .execute(&m.store.pool)
                .await
                .unwrap();
        }
        assert_eq!(merge(&m, 1, 2).await.unwrap().sources.len(), 2);
        assert_eq!(get(&m, 2).await.unwrap().id, 1);
        assert_eq!(merge(&m, 3, 2).await.unwrap().sources.len(), 3);
        for id in [1, 2, 3] {
            let r = get(&m, id).await.unwrap();
            assert_eq!(r.id, 3);
            assert_eq!(r.sources.len(), 3);
        }
        // Aliases cannot be merged back into themselves, creating a cycle.
        assert!(merge(&m, 2, 3).await.is_err());
        assert!(merge(&m, 3, 9999).await.is_err());
        assert_eq!(set(&m, 2, "done", None).await.unwrap().id, 3);
        let batch = some(&m, &[1, 2, 3, 9999]).await.unwrap();
        assert_eq!(batch.as_array().unwrap().len(), 3);
        for (row, id) in batch.as_array().unwrap().iter().zip([1, 2, 3]) {
            assert_eq!(row["id"], id, "batch lookup keeps the attached id");
            assert_eq!(row["status"], "done");
        }
        m.store.pool.close().await;
        drop(m);
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let r = get(&m, 2).await.unwrap();
        assert_eq!((r.id, r.status.as_str(), r.sources.len()), (3, "done", 3));
        let edges: Vec<(i64, i64)> =
            sqlx::query_as("SELECT from_id, into_id FROM request_redirects ORDER BY from_id").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(edges, [(1, 3), (2, 1)], "retain the original merge edges");
    }

    #[tokio::test]
    async fn saved_request_sources_remain_readable_after_leaving_the_brief() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO requests (id, person, title, category, created_at, updated_at) \
             VALUES (1, 'pat@example.net', 'Follow up', 'needs_you', 0, 0); \
             INSERT INTO request_sources (request_id, target, kind, at, noted_at) VALUES \
             (1, 'thread:me@example.net:original-thread', 'mail', 1, 1), \
             (1, 'chat:T1:C2:123.456', 'chat', 2, 2)",
        )
        .execute(&m.store.pool)
        .await
        .unwrap();
        let until = c6_core::now_ms() + 86_400_000;
        for status in ["done", "dropped", "snoozed"] {
            sqlx::query("UPDATE requests SET status = ?, until = ? WHERE id = 1")
                .bind(status)
                .bind(until)
                .execute(&m.store.pool)
                .await
                .unwrap();
            assert!(list(&m, "open", 100).await.unwrap().is_empty());
            // The Messages view uses get, not the active Brief's list.
            let request: api::Request = serde_json::from_value(method(&m, "memex.requests.get", &json!({"id": 1})).await.unwrap()).unwrap();
            assert_eq!(request.status, status);
            assert_eq!(request.sources.len(), 2);
            assert_eq!(request.sources[0].target, "thread:me@example.net:original-thread");
            assert_eq!(request.sources[1].target, "chat:T1:C2:123.456");
        }
    }

    #[tokio::test]
    async fn calendar_rechecks_do_not_repeat_the_transition() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO requests (id, person, title, category, invitation, created_at, updated_at) \
             VALUES (1, 'pat@example.net', 'RSVP', 'needs_you', 1, 0, 0); \
             INSERT INTO mail (item_id, account, gmail_id, thread_id, date, blob) VALUES ('mail', 'a', 'm', 't', 1, ''); \
             INSERT INTO request_sources (request_id, target, kind, at, noted_at, seen_mail) \
             VALUES (1, 'thread:a:t', 'mail', 1, 1, '[\"mail\"]'); \
             INSERT INTO events (account, calendar_id, event_id, status, start, end, my_status, updated) \
             VALUES ('a', 'primary', 'invite', 'confirmed', 1, 2, 'accepted', 1)",
        )
        .execute(&m.store.pool)
        .await
        .unwrap();
        let sources = vec![("thread:a:t".into(), 1, 1, r#"["mail"]"#.into())];
        let event = CalendarAnswer {
            account: "a".into(),
            calendar: "primary".into(),
            event: "invite".into(),
            status: "accepted".into(),
            updated: 1,
        };
        for status in ["open", "snoozed"] {
            sqlx::query("UPDATE requests SET status = ?, updated_at = 0 WHERE id = 1").bind(status).execute(&m.store.pool).await.unwrap();
            assert_eq!(calendar_transition(&m, 1, "You accepted it in Calendar.", &sources, &event, 10).await.unwrap(), Some(true));
            assert_eq!(calendar_transition(&m, 1, "You accepted it in Calendar.", &sources, &event, 10).await.unwrap(), Some(false));
            assert!(close_calendar(&m, 1, "You accepted it in Calendar.", &sources, &event).await.unwrap());
            let updated: i64 = sqlx::query_scalar("SELECT updated_at FROM requests WHERE id = 1").fetch_one(&m.store.pool).await.unwrap();
            assert_eq!(updated, 10);
        }
    }
}
