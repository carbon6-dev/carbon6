//! What the realm explorer reads (docs/explorer.md): counts of a realm's
//! knowledge by kind, its latest items, and lists of things, items and
//! facts.

use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;

/// Items by kind (with the latest), things by kind, facts by status,
/// trails, and the latest items.
pub async fn inventory(m: &Memex, allowed: &[String]) -> anyhow::Result<Value> {
    let kinds = serde_json::to_string(allowed)?;
    let pool = &m.store.pool;
    let items = sqlx::query(
        "SELECT kind, count(*) AS n, max(added_at) AS last FROM items WHERE kind IN (SELECT value FROM json_each(?)) GROUP BY kind ORDER BY n DESC",
    )
    .bind(&kinds)
    .fetch_all(pool)
    .await?;
    let things = sqlx::query("SELECT kind, count(*) AS n, max(created_at) AS last FROM things GROUP BY kind ORDER BY n DESC")
        .fetch_all(pool)
        .await?;
    let facts = sqlx::query(
        "SELECT status, count(*) AS n, max(updated_at) AS last FROM facts WHERE valid_to IS NULL GROUP BY status ORDER BY n DESC",
    )
    .fetch_all(pool)
    .await?;
    let trails: i64 = sqlx::query_scalar("SELECT count(*) FROM trails").fetch_one(pool).await?;
    let recent = sqlx::query(
        "SELECT id, kind, title, created_at, added_at FROM items WHERE kind IN (SELECT value FROM json_each(?)) ORDER BY added_at DESC LIMIT 30",
    )
    .bind(&kinds)
    .fetch_all(pool)
    .await?;
    let group = |rows: &[sqlx::sqlite::SqliteRow], col: &str| -> Vec<Value> {
        rows.iter()
            .map(|r| json!({"kind": r.get::<String, _>(col), "count": r.get::<i64, _>("n"), "last": r.get::<Option<i64>, _>("last")}))
            .collect()
    };
    Ok(json!({
        "items": group(&items, "kind"),
        "things": group(&things, "kind"),
        "facts": group(&facts, "status"),
        "trails": trails,
        "recent": recent.iter().map(|r| json!({
            "id": r.get::<String, _>("id"), "kind": r.get::<String, _>("kind"), "title": r.get::<String, _>("title"),
            "created_at": r.get::<i64, _>("created_at"), "added_at": r.get::<i64, _>("added_at"),
        })).collect::<Vec<_>>(),
    }))
}

/// A page of things (by kind), items (by kind) or facts (by status), newest
/// first, optionally matching `q`: [{id, kind, title, sub, at}].
pub async fn list(m: &Memex, params: &Value, allowed: &[String]) -> anyhow::Result<Value> {
    let kind = params["kind"].as_str().unwrap_or("");
    let q = format!("%{}%", params["q"].as_str().unwrap_or("").replace(['%', '_'], ""));
    let limit = params["limit"].as_i64().unwrap_or(100).clamp(1, 500);
    let offset = params["offset"].as_i64().unwrap_or(0).max(0);
    let pool = &m.store.pool;
    let out: Vec<Value> = match params["what"].as_str().unwrap_or("items") {
        "things" => sqlx::query(
            "SELECT t.id, t.kind, t.name, t.key, t.created_at, (SELECT count(*) FROM links l WHERE l.dst_thing = t.id) AS n FROM things t \
             WHERE (?1 = '' OR t.kind = ?1) AND (t.name LIKE ?2 OR coalesce(t.key, '') LIKE ?2) ORDER BY n DESC, t.name LIMIT ?3 OFFSET ?4",
        )
        .bind(kind)
        .bind(&q)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            json!({"id": r.get::<String, _>("id"), "kind": r.get::<String, _>("kind"), "title": r.get::<String, _>("name"),
                   "sub": format!("{} · {} links", r.get::<Option<String>, _>("key").unwrap_or_default(), r.get::<i64, _>("n")),
                   "at": r.get::<i64, _>("created_at")})
        })
        .collect(),
        "facts" => sqlx::query(
            "SELECT id, claim, status, about, updated_at FROM facts WHERE valid_to IS NULL AND (?1 = '' OR status = ?1) AND claim LIKE ?2 \
             ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4",
        )
        .bind(kind)
        .bind(&q)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            let about: Vec<String> = serde_json::from_str(&r.get::<String, _>("about")).unwrap_or_default();
            json!({"id": r.get::<String, _>("id"), "kind": r.get::<String, _>("status"), "title": r.get::<String, _>("claim"),
                   "sub": about.join(", "), "at": r.get::<i64, _>("updated_at")})
        })
        .collect(),
        _ => sqlx::query(
            "SELECT id, kind, title, source, created_at FROM items WHERE kind IN (SELECT value FROM json_each(?1)) AND (?2 = '' OR kind = ?2) \
             AND title LIKE ?3 ORDER BY created_at DESC LIMIT ?4 OFFSET ?5",
        )
        .bind(serde_json::to_string(allowed)?)
        .bind(kind)
        .bind(&q)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            json!({"id": r.get::<String, _>("id"), "kind": r.get::<String, _>("kind"), "title": r.get::<String, _>("title"),
                   "sub": r.get::<String, _>("source"), "at": r.get::<i64, _>("created_at")})
        })
        .collect(),
    };
    Ok(json!(out))
}

/// The items mentioning a thing, and the facts about it.
pub async fn about_thing(m: &Memex, id: &str, allowed: &[String]) -> anyhow::Result<Value> {
    let Some(t) = m.store.thing(id).await? else { anyhow::bail!("no thing {id:?}") };
    let items = m.store.about(Some(&t.id), allowed, 50).await?;
    let pool = &m.store.pool;
    let facts = sqlx::query(
        "SELECT id, claim, status FROM facts WHERE valid_to IS NULL AND EXISTS (SELECT 1 FROM json_each(about) WHERE value = ?) ORDER BY updated_at DESC LIMIT 50",
    )
    .bind(&t.label)
    .fetch_all(pool)
    .await?;
    let related = m.store.related(&t.id, allowed, 20).await?;
    Ok(json!({
        "thing": t,
        "items": items,
        "facts": facts.iter().map(|r| json!({"id": r.get::<String, _>("id"), "claim": r.get::<String, _>("claim"), "status": r.get::<String, _>("status")})).collect::<Vec<_>>(),
        "related": related.into_iter().map(|(t, n)| json!({"thing": t, "shared": n})).collect::<Vec<_>>(),
    }))
}
