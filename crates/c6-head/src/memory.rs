//! Memory: durable notes, each scoped by a label (or none: every session).
//! Sessions get the notes for their labels in their system prompt, and
//! agents keep them current with remember/forget (as MemoryUpdate events,
//! applied here when ingested). Notes are records of the system collection
//! `memory` in their realm (docs/os.md §12.2), each with its history.

use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row;

use crate::lists::MEMORY;
use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};

/// Notes given to a new session at most (the newest).
const MAX_NOTES: i64 = 500;
pub const MAX_TEXT: usize = 2000;

fn row(r: &sqlx::sqlite::SqliteRow) -> api::Memory {
    let fields: Value = serde_json::from_str(&r.get::<String, _>("fields")).unwrap_or(Value::Null);
    api::Memory {
        id: r.get("id"),
        scope: fields["scope"].as_str().unwrap_or("").to_owned(),
        text: fields["text"].as_str().unwrap_or("").to_owned(),
        session_id: fields["session_id"].as_str().map(str::to_owned),
        updated_at: r.get("updated_at"),
    }
}

/// The notes the caller may see.
pub async fn list(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<api::Memory>> {
    let rows = sqlx::query(
        "SELECT * FROM records WHERE collection = ? AND (? IS NULL OR realm = ?) \
         ORDER BY json_extract(fields, '$.scope'), updated_at DESC",
    )
    .bind(MEMORY)
    .bind(&caller.realm)
    .bind(&caller.realm)
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// The newest notes of `realm`, for a new session's spec there.
pub async fn for_sessions(st: &HeadState, realm: &str) -> anyhow::Result<String> {
    let rows = sqlx::query("SELECT * FROM records WHERE collection = ? AND realm = ? ORDER BY updated_at DESC LIMIT ?")
        .bind(MEMORY)
        .bind(realm)
        .bind(MAX_NOTES)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(serde_json::to_string(&rows.iter().map(row).collect::<Vec<_>>())?)
}

/// The notes for a new session: its realm's, and, for an app instance's
/// session, the notes its package ships (docs/apps.md §5.3).
pub async fn for_sessions_of(st: &HeadState, realm: &str, instance: &str) -> anyhow::Result<String> {
    let mut notes: Vec<api::Memory> = serde_json::from_str(&for_sessions(st, realm).await?)?;
    if !instance.is_empty()
        && let Some(i) = crate::instances::get(&st.store.pool, realm, instance).await?
    {
        for (n, m) in i.package.contents.memory.iter().enumerate() {
            let m = crate::instances::fill_settings(m, &i.settings);
            // Apps installed before instances copied their notes into the realm.
            if notes.iter().any(|x| Some(x.text.as_str()) == m["text"].as_str()) {
                continue;
            }
            notes.push(api::Memory {
                id: format!("app:{instance}:{n}"),
                scope: m["scope"].as_str().unwrap_or("").to_owned(),
                text: m["text"].as_str().unwrap_or("").to_owned(),
                session_id: None,
                updated_at: 0,
            });
        }
    }
    Ok(serde_json::to_string(&notes)?)
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
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    upsert(&mut tx, &m.id, &m.scope, &m.text, None, realm, "you").await?;
    tx.commit().await?;
    let realm = realm_of(st, &m.id).await?.unwrap_or_else(|| realm.to_owned());
    if crate::memex::has_memex(&realm) {
        let item = json!(crate::memex::memory_item(&m.id, &m.scope, &m.text));
        crate::memex::enqueue(st, "memex.ingest", crate::memex::in_realm(item, &realm)).await;
    }
    Ok(list(st, &Caller::root()).await?.into_iter().find(|x| x.id == m.id).unwrap_or(m))
}

async fn realm_of(st: &HeadState, id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT realm FROM records WHERE collection = ? AND id = ?")
        .bind(MEMORY)
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?)
}

pub async fn delete(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let realm = realm_of(st, id).await?.ok_or_else(|| not_found("no such note"))?;
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    remove(&mut tx, id, "you").await?;
    tx.commit().await?;
    if crate::memex::has_memex(&realm) {
        let params = json!({"source": "memory", "external_id": id});
        crate::memex::enqueue(st, "memex.delete", crate::memex::in_realm(params, &realm)).await;
    }
    Ok(())
}

/// Creates the note, or changes its scope and text (keeping who wrote it
/// first, and its realm); either way a version in its history.
async fn upsert(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    scope: &str,
    text: &str,
    session: Option<&str>,
    realm: &str,
    by: &str,
) -> anyhow::Result<()> {
    let now = now_ms();
    let current: Option<(String, i64)> = sqlx::query_as("SELECT fields, version FROM records WHERE collection = ? AND id = ?")
        .bind(MEMORY)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let (fields, version) = match current {
        Some((old, version)) => {
            let old: Value = serde_json::from_str(&old).unwrap_or(Value::Null);
            let session = session.map(str::to_owned).or_else(|| old["session_id"].as_str().map(str::to_owned));
            let fields = json!({"scope": scope, "text": text, "session_id": session});
            sqlx::query("UPDATE records SET fields = ?, version = ?, updated_by = ?, updated_at = ? WHERE id = ?")
                .bind(fields.to_string())
                .bind(version + 1)
                .bind(by)
                .bind(now)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            (fields, version + 1)
        }
        None => {
            let fields = json!({"scope": scope, "text": text, "session_id": session});
            sqlx::query(
                "INSERT INTO records (id, realm, collection, fields, version, created_by, updated_by, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(realm)
            .bind(MEMORY)
            .bind(fields.to_string())
            .bind(by)
            .bind(by)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            (fields, 1)
        }
    };
    history(tx, id, version, if version == 1 { "create" } else { "update" }, &fields, by, now).await
}

async fn remove(tx: &mut sqlx::SqliteConnection, id: &str, by: &str) -> anyhow::Result<()> {
    let current: Option<(String, i64)> = sqlx::query_as("SELECT fields, version FROM records WHERE collection = ? AND id = ?")
        .bind(MEMORY)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some((fields, version)) = current {
        sqlx::query("DELETE FROM records WHERE id = ?").bind(id).execute(&mut *tx).await?;
        let fields: Value = serde_json::from_str(&fields).unwrap_or(Value::Null);
        history(tx, id, version + 1, "delete", &fields, by, now_ms()).await?;
    }
    Ok(())
}

async fn history(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    version: i64,
    op: &str,
    fields: &Value,
    by: &str,
    at: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO record_history (record_id, version, op, fields, changed_by, changed_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(id)
        .bind(version)
        .bind(op)
        .bind(fields.to_string())
        .bind(by)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Applies an agent's MemoryUpdate (while its event is stored).
pub async fn apply(tx: &mut sqlx::SqliteConnection, session_id: &str, u: &c6_proto::MemoryUpdate) -> anyhow::Result<()> {
    let by = format!("session:{session_id}");
    match u.op.as_str() {
        "remember" if !u.id.is_empty() && !u.text.trim().is_empty() => {
            let text: String = u.text.trim().chars().take(MAX_TEXT).collect();
            // A session's notes belong to its realm.
            let realm: String = sqlx::query_scalar("SELECT realm FROM sessions WHERE id = ?")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or_else(|| crate::realms::HOME.to_owned());
            upsert(tx, &u.id, u.scope.trim(), &text, Some(session_id), &realm, &by).await
        }
        "forget" => remove(tx, &u.id, &by).await,
        _ => Ok(()),
    }
}
