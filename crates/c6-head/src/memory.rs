//! Memory: durable notes, each scoped by a label (or none: every session).
//! Sessions get the notes for their labels in their system prompt, and
//! agents keep them current with remember/forget (as MemoryUpdate events,
//! applied here when ingested).

use c6_api as api;
use c6_core::now_ms;
use sqlx::Row;

use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};

/// Notes given to a new session at most (the newest).
const MAX_NOTES: i64 = 500;
pub const MAX_TEXT: usize = 2000;

fn row(r: &sqlx::sqlite::SqliteRow) -> api::Memory {
    api::Memory {
        id: r.get("id"),
        scope: r.get("scope"),
        text: r.get("text"),
        session_id: r.get("session_id"),
        updated_at: r.get("updated_at"),
    }
}

/// The notes the caller may see.
pub async fn list(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<api::Memory>> {
    let rows = sqlx::query("SELECT * FROM memories WHERE (? IS NULL OR realm = ?) ORDER BY scope, updated_at DESC")
        .bind(&caller.realm)
        .bind(&caller.realm)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(rows.iter().map(row).collect())
}

/// The newest notes of `realm`, for a new session's spec there.
pub async fn for_sessions(st: &HeadState, realm: &str) -> anyhow::Result<String> {
    let rows = sqlx::query("SELECT * FROM memories WHERE realm = ? ORDER BY updated_at DESC LIMIT ?")
        .bind(realm)
        .bind(MAX_NOTES)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(serde_json::to_string(&rows.iter().map(row).collect::<Vec<_>>())?)
}

fn check(m: &api::Memory) -> anyhow::Result<()> {
    if m.text.trim().is_empty() || m.text.len() > MAX_TEXT {
        return Err(bad_request(format!("a note has 1 to {MAX_TEXT} characters")));
    }
    if m.scope.contains(char::is_whitespace) || m.scope.contains(',') {
        return Err(bad_request("a note's scope is one label"));
    }
    Ok(())
}

/// Adds or replaces a note (by id). A new note goes in `realm`; an edited
/// one stays where it is.
pub async fn put(st: &HeadState, mut m: api::Memory, realm: &str) -> anyhow::Result<api::Memory> {
    m.text = m.text.trim().to_owned();
    m.scope = m.scope.trim().to_owned();
    check(&m)?;
    if m.id.is_empty() {
        m.id = uuid::Uuid::now_v7().simple().to_string()[20..].to_owned();
    }
    upsert(&st.store.pool, &m.id, &m.scope, &m.text, None, realm).await?;
    let realm = realm_of(st, &m.id).await?.unwrap_or_else(|| realm.to_owned());
    if crate::memex::has_memex(&realm) {
        let item = serde_json::json!(crate::memex::memory_item(&m.id, &m.scope, &m.text));
        crate::memex::enqueue(st, "memex.ingest", crate::memex::in_realm(item, &realm)).await;
    }
    Ok(list(st, &Caller::root()).await?.into_iter().find(|x| x.id == m.id).unwrap_or(m))
}

async fn realm_of(st: &HeadState, id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT realm FROM memories WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?)
}

pub async fn delete(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let realm = realm_of(st, id).await?.ok_or_else(|| not_found("no such note"))?;
    sqlx::query("DELETE FROM memories WHERE id = ?").bind(id).execute(&st.store.pool).await?;
    if crate::memex::has_memex(&realm) {
        let params = serde_json::json!({"source": "memory", "external_id": id});
        crate::memex::enqueue(st, "memex.delete", crate::memex::in_realm(params, &realm)).await;
    }
    Ok(())
}

async fn upsert<'e, E: sqlx::Executor<'e, Database = sqlx::Sqlite>>(
    db: E,
    id: &str,
    scope: &str,
    text: &str,
    session: Option<&str>,
    realm: &str,
) -> anyhow::Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO memories (id, scope, text, session_id, created_at, updated_at, realm) VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET scope = excluded.scope, text = excluded.text, \
         session_id = COALESCE(excluded.session_id, memories.session_id), updated_at = excluded.updated_at",
    )
    .bind(id)
    .bind(scope)
    .bind(text)
    .bind(session)
    .bind(now)
    .bind(now)
    .bind(realm)
    .execute(db)
    .await?;
    Ok(())
}

/// Applies an agent's MemoryUpdate (while its event is stored).
pub async fn apply(tx: &mut sqlx::SqliteConnection, session_id: &str, u: &c6_proto::MemoryUpdate) -> anyhow::Result<()> {
    match u.op.as_str() {
        "remember" if !u.id.is_empty() && !u.text.trim().is_empty() => {
            let text: String = u.text.trim().chars().take(MAX_TEXT).collect();
            // A session's notes belong to its realm.
            let realm: String = sqlx::query_scalar("SELECT realm FROM sessions WHERE id = ?")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or_else(|| crate::realms::HOME.to_owned());
            upsert(&mut *tx, &u.id, u.scope.trim(), &text, Some(session_id), &realm).await
        }
        "forget" => {
            sqlx::query("DELETE FROM memories WHERE id = ?").bind(&u.id).execute(&mut *tx).await?;
            Ok(())
        }
        _ => Ok(()),
    }
}
