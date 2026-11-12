//! Follow-ups: what you mean to ask or tell a session once it's done,
//! noted while it works and held until you send them (docs/capture.md §4).
//! They never reach the agent by themselves.

use std::collections::HashMap;

use c6_api::work::{Followup, FollowupNew, Quote};
use c6_core::now_ms;
use sqlx::Row;
use tracing::warn;

use crate::{HeadState, bad_request, not_found};

/// The most a session holds at once.
const MAX_FOLLOWUPS: i64 = 50;
/// The most of one follow-up's text.
const MAX_TEXT: usize = 4_000;

fn check_text(t: &str) -> anyhow::Result<String> {
    let t = t.trim();
    if t.is_empty() || t.chars().count() > MAX_TEXT {
        return Err(bad_request(format!("a follow-up is up to {MAX_TEXT} characters")));
    }
    Ok(t.to_owned())
}

fn row(r: &sqlx::sqlite::SqliteRow) -> Followup {
    Followup {
        id: r.get("id"),
        session: r.get("session_id"),
        text: r.get("text"),
        quote: serde_json::from_str::<Quote>(&r.get::<String, _>("quote")).ok(),
        at: r.get("created_at"),
    }
}

pub async fn list(st: &HeadState, session: &str) -> anyhow::Result<Vec<Followup>> {
    let rows =
        sqlx::query("SELECT * FROM followups WHERE session_id = ? ORDER BY created_at, id").bind(session).fetch_all(&st.store.pool).await?;
    Ok(rows.iter().map(row).collect())
}

/// How many each session holds, for the session list.
pub async fn counts(st: &HeadState) -> anyhow::Result<HashMap<String, u32>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT session_id, COUNT(*) FROM followups GROUP BY session_id").fetch_all(&st.store.pool).await?;
    Ok(rows.into_iter().map(|(s, n)| (s, n as u32)).collect())
}

pub async fn add(st: &HeadState, session: &str, n: &FollowupNew) -> anyhow::Result<Followup> {
    st.store.session(session).await?.ok_or_else(|| not_found("no such session"))?;
    let text = check_text(&n.text)?;
    let quote = crate::work::check_quote(n.quote.as_ref())?;
    let held: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM followups WHERE session_id = ?").bind(session).fetch_one(&st.store.pool).await?;
    if held >= MAX_FOLLOWUPS {
        return Err(bad_request(format!("a session holds at most {MAX_FOLLOWUPS} follow-ups: send or remove some")));
    }
    let f = Followup { id: uuid::Uuid::now_v7().to_string(), session: session.to_owned(), text, quote, at: now_ms() };
    sqlx::query("INSERT INTO followups (id, session_id, text, quote, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&f.id)
        .bind(session)
        .bind(&f.text)
        .bind(f.quote.as_ref().map(serde_json::to_string).transpose()?.unwrap_or_default())
        .bind(f.at)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(f)
}

pub async fn edit(st: &HeadState, session: &str, id: &str, text: &str) -> anyhow::Result<Followup> {
    let text = check_text(text)?;
    let done = sqlx::query("UPDATE followups SET text = ? WHERE id = ? AND session_id = ?")
        .bind(&text)
        .bind(id)
        .bind(session)
        .execute(&st.store.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(not_found("no such follow-up"));
    }
    st.registry.bump();
    get(st, session, id).await
}

async fn get(st: &HeadState, session: &str, id: &str) -> anyhow::Result<Followup> {
    let r = sqlx::query("SELECT * FROM followups WHERE id = ? AND session_id = ?")
        .bind(id)
        .bind(session)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found("no such follow-up"))?;
    Ok(row(&r))
}

pub async fn remove(st: &HeadState, session: &str, id: &str) -> anyhow::Result<()> {
    let done = sqlx::query("DELETE FROM followups WHERE id = ? AND session_id = ?").bind(id).bind(session).execute(&st.store.pool).await?;
    if done.rows_affected() == 0 {
        return Err(not_found("no such follow-up"));
    }
    st.registry.bump();
    Ok(())
}

/// One follow-up as you'd type it: what prompted it, quoted, then yours.
fn as_message(f: &Followup) -> String {
    match &f.quote {
        Some(q) if !q.text.is_empty() => format!("{}\n\n{}", crate::work::quoted(&q.text), f.text),
        _ => f.text.clone(),
    }
}

/// Sends follow-ups (`ids`, else all) to the session as one message of
/// yours, numbered when several, and lets them go once it's taken.
pub async fn send(st: &HeadState, session: &str, ids: &[String]) -> anyhow::Result<c6_api::QueuedInput> {
    let all = list(st, session).await?;
    let chosen: Vec<&Followup> = all.iter().filter(|f| ids.is_empty() || ids.contains(&f.id)).collect();
    if chosen.is_empty() || (!ids.is_empty() && chosen.len() != ids.len()) {
        return Err(bad_request("no such follow-ups to send"));
    }
    let text = match chosen.as_slice() {
        [one] => as_message(one),
        many => many.iter().enumerate().map(|(i, f)| format!("{}. {}", i + 1, as_message(f))).collect::<Vec<_>>().join("\n\n"),
    };
    let queued = crate::sessions::send_input(st, session, &c6_api::SendMessage { text, ..Default::default() }).await?;
    for f in &chosen {
        sqlx::query("DELETE FROM followups WHERE id = ?").bind(&f.id).execute(&st.store.pool).await?;
    }
    st.registry.bump();
    Ok(queued)
}

/// Moves a follow-up to the session's work as a to-do.
pub async fn to_todo(st: &HeadState, session: &str, id: &str) -> anyhow::Result<(String, String)> {
    let f = get(st, session, id).await?;
    let added =
        crate::work::session_todo(st, session, &first_line(&f.text), &f.text, f.quote.as_ref(), &crate::agent_ops::by("you")).await?;
    sqlx::query("DELETE FROM followups WHERE id = ?").bind(id).execute(&st.store.pool).await?;
    st.registry.bump();
    Ok(added)
}

/// A session archived with follow-ups not sent: they become to-dos on its
/// work, so none is lost.
pub async fn on_archived(st: &HeadState, session: &str) {
    let Ok(held) = list(st, session).await else { return };
    for f in held {
        if let Err(e) = to_todo(st, session, &f.id).await {
            warn!(%session, followup = %f.id, "keeping a follow-up of an archived session: {e:#}");
        }
    }
}

/// A to-do's line from a follow-up's text.
fn first_line(t: &str) -> String {
    let line = t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(t);
    let mut out: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        out.push('…');
    }
    out
}
