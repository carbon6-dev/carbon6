//! Decks (docs/docs.md §5): their themes are records of the system
//! collection `deck_themes` in each realm, editable in Settings → Decks;
//! Paper, Night and Signal are put there the first time a realm's themes
//! are read. A deck names its theme; carbon6 draws slides from the theme's
//! CSS custom properties over the base slide CSS (`c6_api::slides`).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::{Extension, Json, Router};
use c6_api as api;
use c6_api::slides::DeckTheme;
use c6_core::now_ms;
use sqlx::Row;

use crate::api::ApiResult;
use crate::realms::Caller;
use crate::{HeadState, bad_request};

/// The system collection.
pub const DECK_THEMES: &str = "deck_themes";
const MAX_CSS: usize = 20_000;

/// A realm's themes (the shipped ones the first time).
pub async fn themes(st: &HeadState, realm: &str) -> anyhow::Result<Vec<DeckTheme>> {
    let rows = sqlx::query("SELECT fields FROM records WHERE collection = ? AND realm = ? ORDER BY created_at, id")
        .bind(DECK_THEMES)
        .bind(realm)
        .fetch_all(&st.store.pool)
        .await?;
    if rows.is_empty() {
        let defaults = api::slides::default_themes();
        write(st, realm, &defaults, "system").await?;
        return Ok(defaults);
    }
    Ok(rows.iter().filter_map(|r| serde_json::from_str(&r.get::<String, _>("fields")).ok()).collect())
}

/// A theme's CSS by name (the first theme's when it isn't there).
pub async fn theme_css(st: &HeadState, realm: &str, name: &str) -> String {
    let all = themes(st, realm).await.unwrap_or_else(|_| api::slides::default_themes());
    all.iter().find(|t| t.name.eq_ignore_ascii_case(name)).or(all.first()).map(|t| t.css.clone()).unwrap_or_default()
}

/// Replaces a realm's themes, each change a version of its record.
pub async fn put(st: &HeadState, realm: &str, list: &[DeckTheme], by: &str) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for t in list {
        let n = t.name.trim();
        if n.is_empty() || n.len() > 60 || !names.insert(n.to_lowercase()) {
            return Err(bad_request("each theme needs a name of its own (up to 60 characters)"));
        }
        if t.css.trim().is_empty() || t.css.len() > MAX_CSS {
            return Err(bad_request(format!("theme {n}: its CSS is 1 to {MAX_CSS} characters (custom properties on :root)")));
        }
        if t.css.to_lowercase().contains("</style") || t.css.to_lowercase().contains("<script") {
            return Err(bad_request(format!("theme {n}: CSS only")));
        }
    }
    if list.is_empty() {
        return Err(bad_request("a realm keeps at least one theme"));
    }
    write(st, realm, list, by).await
}

async fn write(st: &HeadState, realm: &str, list: &[DeckTheme], by: &str) -> anyhow::Result<()> {
    let now = now_ms();
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let old: Vec<(String, String, i64)> = sqlx::query("SELECT id, fields, version FROM records WHERE collection = ? AND realm = ?")
        .bind(DECK_THEMES)
        .bind(realm)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|r| (r.get("id"), r.get("fields"), r.get("version")))
        .collect();
    let name_of = |f: &str| serde_json::from_str::<DeckTheme>(f).map(|t| t.name.to_lowercase()).unwrap_or_default();
    for t in list {
        let value = serde_json::to_value(t)?;
        match old.iter().find(|(_, f, _)| name_of(f) == t.name.trim().to_lowercase()) {
            Some((id, f, _)) if serde_json::from_str::<DeckTheme>(f).ok().as_ref() == Some(t) => {
                let _ = id;
            }
            Some((id, _, version)) => {
                sqlx::query("UPDATE records SET fields = ?, version = ?, updated_by = ?, updated_at = ? WHERE id = ?")
                    .bind(value.to_string())
                    .bind(version + 1)
                    .bind(by)
                    .bind(now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                history(&mut tx, id, version + 1, "update", &value, by, now).await?;
            }
            None => {
                let id = uuid::Uuid::now_v7().to_string();
                sqlx::query(
                    "INSERT INTO records (id, realm, collection, fields, labels, version, created_by, updated_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, '[]', 1, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(realm)
                .bind(DECK_THEMES)
                .bind(value.to_string())
                .bind(by)
                .bind(by)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                history(&mut tx, &id, 1, "create", &value, by, now).await?;
            }
        }
    }
    for (id, f, version) in &old {
        if !list.iter().any(|t| t.name.trim().to_lowercase() == name_of(f)) {
            sqlx::query("DELETE FROM records WHERE id = ?").bind(id).execute(&mut *tx).await?;
            history(&mut tx, id, version + 1, "delete", &serde_json::from_str(f).unwrap_or_default(), by, now).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn history(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    version: i64,
    op: &str,
    fields: &serde_json::Value,
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

/// What a session making a deck reads first: the `decks.*` prompts, the
/// layouts and the realm's themes.
pub async fn guide(st: &HeadState, realm: &str) -> String {
    let mut out = String::new();
    for name in ["decks.flow", "decks.storyline", "decks.design", "decks.visuals", "decks.review"] {
        out.push_str(&format!("## {name}\n\n{}\n\n", crate::prompts::get(st, name, realm).await));
    }
    out.push_str("## Themes\n\n");
    for t in themes(st, realm).await.unwrap_or_default() {
        out.push_str(&format!("- {}: {}\n", t.name, t.mood));
    }
    out
}

// --- HTTP -------------------------------------------------------------------

pub fn routes() -> Router<Arc<HeadState>> {
    Router::new()
        .route("/api/v1/decks/themes", axum::routing::get(get_themes).put(put_themes))
        .route("/api/v1/decks/bring-in", axum::routing::get(bring_status).post(bring_in))
}

async fn get_themes(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<Vec<DeckTheme>>> {
    let realm = crate::docs::realm_for(&st, &caller, r.realm.as_deref()).await?;
    Ok(Json(themes(&st, &realm).await?))
}

async fn put_themes(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
    Json(list): Json<Vec<DeckTheme>>,
) -> ApiResult<axum::http::StatusCode> {
    let realm = crate::docs::realm_for(&st, &caller, r.realm.as_deref()).await?;
    put(&st, &realm, &list, &crate::docs::by_of(&caller)).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn bring_status(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = crate::docs::realm_for(&st, &caller, r.realm.as_deref()).await?;
    Ok(Json(crate::docs::bring::status(&st, &realm).await?))
}

async fn bring_in(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(r): Query<api::InRealm>,
) -> ApiResult<Json<serde_json::Value>> {
    let realm = crate::docs::realm_for(&st, &caller, r.realm.as_deref()).await?;
    Ok(Json(crate::docs::bring::bring_in(&st, &realm, &crate::docs::by_of(&caller)).await?))
}
