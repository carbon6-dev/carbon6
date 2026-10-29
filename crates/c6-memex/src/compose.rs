//! Reply boxes and the sessions working on them (docs/conversations.md §3):
//! the context a session starts with, and the drafts it writes back.
//!
//! A box is named by a key: `request:<id>` (the Brief's draft: its text is
//! the request's), or a conversation (`thread:<account>:<thread>`,
//! `chat:<workspace>:<channel>[:<thread ts>]`), or `compose:<id>` (a new
//! message). The `drafts` table keeps the others' text, and every box's
//! session.

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;
use crate::store::clip;

/// The most people whose cards a context names.
const PEOPLE: usize = 4;

fn request_of(key: &str) -> Option<i64> {
    key.strip_prefix("request:")?.parse().ok()
}

/// The first message of a session on a box: the `memex.compose.session`
/// prompt with the box's context.
pub async fn context(m: &Memex, key: &str, draft: &str, to: &[String]) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut people: Vec<String> = to.iter().map(|a| a.trim().to_lowercase()).filter(|a| a.contains('@')).collect();
    match request_of(key) {
        Some(id) => {
            let r = crate::requests::get(m, id).await?;
            // Use the draft's cleaned request line once, under the reply box's heading.
            let context = crate::drafts::context(m, &r).await?;
            let (request, context) = crate::drafts::task_context(&context);
            out.push_str(&format!("## What they need\n{request}. {}\n{context}", crate::drafts::clean(&r.reason)));
            people.clear();
        }
        None if key.starts_with("thread:") || key.starts_with("chat:") => {
            out.push_str(&crate::drafts::conversation_context(m, key).await?);
            people.extend(participants(m, key).await?);
        }
        None => {}
    }
    people.dedup();
    let mut cards = String::new();
    for a in people.iter().take(PEOPLE) {
        for c in crate::cards::of(m, a).await? {
            cards.push_str(&format!("- {}\n", crate::cards::text(&c)));
        }
    }
    if !cards.is_empty() {
        out.push_str(&format!("\n## Who they are to me\n{}", crate::drafts::clean(&cards)));
    }
    let draft = if draft.trim().is_empty() { "(none yet)".to_owned() } else { clip(draft, 8000) };
    out.push_str(&format!("\n## My draft so far\n{draft}\n"));
    let template = m.prompt("memex.compose.session", api::prompts::COMPOSE_SESSION);
    Ok(api::prompts::fill(&template, &[("context", out.trim()), ("for", key)]))
}

/// Who wrote in a conversation, besides you (their addresses).
async fn participants(m: &Memex, key: &str) -> anyhow::Result<Vec<String>> {
    if let Some(rest) = key.strip_prefix("thread:") {
        let (account, thread) = rest.rsplit_once(':').unwrap_or_default();
        return Ok(sqlx::query_scalar(
            "SELECT lower(from_addr) FROM mail WHERE account = ? AND thread_id = ? AND sent = 0 AND draft = 0 GROUP BY lower(from_addr) \
             ORDER BY MAX(date) DESC",
        )
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?);
    }
    let parts: Vec<&str> = key.strip_prefix("chat:").unwrap_or_default().splitn(3, ':').collect();
    let (ws, ch) = (parts.first().copied().unwrap_or_default(), parts.get(1).copied().unwrap_or_default());
    Ok(sqlx::query_scalar(
        "SELECT lower(json_extract(meta, '$.user_email')) AS a FROM items WHERE kind = 'chat' AND json_extract(meta, '$.workspace') = ? \
         AND json_extract(meta, '$.channel') = ? AND COALESCE(json_extract(meta, '$.from_me'), 0) = 0 AND a IS NOT NULL AND a != '' \
         GROUP BY a ORDER BY MAX(created_at) DESC LIMIT 10",
    )
    .bind(ws)
    .bind(ch)
    .fetch_all(&m.store.pool)
    .await?)
}

/// A box's draft: its text, who wrote it last and when, and its session.
pub async fn get(m: &Memex, key: &str) -> anyhow::Result<Value> {
    let row = sqlx::query("SELECT text, session, by, at FROM drafts WHERE key = ?").bind(key).fetch_optional(&m.store.pool).await?;
    let s = |k: &str| row.as_ref().map(|r| r.get::<String, _>(k)).unwrap_or_default();
    let session = s("session");
    if let Some(id) = request_of(key) {
        let d = crate::requests::get(m, id).await?.draft.unwrap_or_default();
        return Ok(json!({"key": key, "text": d.text, "by": d.by, "at": d.at, "session": session, "target": d.target}));
    }
    Ok(json!({"key": key, "text": s("text"), "by": s("by"), "at": row.as_ref().map_or(0, |r| r.get::<i64, _>("at")), "session": session}))
}

/// Replaces a box's draft: a request's, or the box's own.
pub async fn set(m: &Memex, key: &str, text: &str, by: &str) -> anyhow::Result<Value> {
    let at = c6_core::now_ms();
    if let Some(id) = request_of(key) {
        let r = crate::requests::get(m, id).await?;
        let mut d = r.draft.clone().unwrap_or_default();
        if d.target.is_empty() {
            // Not drafted yet: in its latest conversation.
            d.target = r.sources.last().map(|s| s.target.clone()).unwrap_or_default();
            d.kind = if d.target.starts_with("chat:") { "chat" } else { "mail" }.into();
        }
        (d.text, d.by, d.at, d.error) = (text.to_owned(), by.to_owned(), at, None);
        sqlx::query("UPDATE requests SET draft = ?, drafted_at = ? WHERE id = ?")
            .bind(serde_json::to_string(&d)?)
            .bind(at)
            .bind(id)
            .execute(&m.store.pool)
            .await?;
    }
    let own = if request_of(key).is_some() { "" } else { text };
    sqlx::query(
        "INSERT INTO drafts (key, text, by, at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (key) DO UPDATE SET text = excluded.text, by = excluded.by, at = excluded.at",
    )
    .bind(key)
    .bind(own)
    .bind(by)
    .bind(at)
    .execute(&m.store.pool)
    .await?;
    get(m, key).await
}

/// The session working on a box.
pub async fn link(m: &Memex, key: &str, session: &str) -> anyhow::Result<Value> {
    sqlx::query("INSERT INTO drafts (key, session, at) VALUES (?1, ?2, 0) ON CONFLICT (key) DO UPDATE SET session = excluded.session")
        .bind(key)
        .bind(session)
        .execute(&m.store.pool)
        .await?;
    get(m, key).await
}

pub async fn method(m: &Memex, method: &str, params: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
    let key = s("for");
    anyhow::ensure!(!key.is_empty(), "for (the reply box) is required");
    Ok(match method {
        "memex.compose.context" => {
            let to: Vec<String> = params["to"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_owned)).collect();
            json!({"prompt": context(m, &key, &s("draft"), &to).await?})
        }
        "memex.drafts.get" => get(m, &key).await?,
        "memex.drafts.set" => {
            let by = params["made_by"].as_str().filter(|b| b.starts_with("session:")).unwrap_or("you");
            set(m, &key, &s("text"), by).await?
        }
        "memex.drafts.link" => link(m, &key, &s("session")).await?,
        "memex.drafts.clear" => {
            sqlx::query("DELETE FROM drafts WHERE key = ?").bind(&key).execute(&m.store.pool).await?;
            json!({})
        }
        other => anyhow::bail!("the memex has no method {other}"),
    })
}
