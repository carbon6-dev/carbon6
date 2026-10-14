//! Realms (docs/os.md §3): every object belongs to exactly one. You, signed
//! in (or with a root API token), reach every realm; an API token scoped to
//! a realm reaches only that realm's data, and none of carbon6's
//! administration. The head is where this is decided.

use axum::http::{Method, StatusCode};
use c6_api as api;
use sqlx::Row;

use crate::{HeadState, bad_request, forbidden};

/// carbon6's own administration, and the defaults realms inherit.
pub const ROOT: &str = "root";
/// Everything that existed before realms, and the default.
pub const HOME: &str = "home";
/// Knowledge meant for every realm, published into with your review.
pub const COMMONS: &str = "commons";

/// Who is calling the API.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caller {
    /// `None`: every realm (you, or a root token).
    pub realm: Option<String>,
}

impl Caller {
    pub fn root() -> Self {
        Self { realm: None }
    }

    pub fn in_realm(realm: &str) -> Self {
        Self { realm: Some(realm.to_owned()) }
    }

    /// The realm whose memex knowledge the caller works with: its own, or
    /// (for you) home.
    pub fn memex_realm(&self) -> &str {
        self.realm.as_deref().unwrap_or(HOME)
    }

    /// Whether the caller may see what `realm` owns.
    pub fn sees(&self, realm: &str) -> bool {
        self.realm.as_deref().is_none_or(|r| r == realm)
    }

    /// The realm a new object goes in: the one asked for (which a scoped
    /// caller cannot leave), else the caller's, else home.
    pub fn realm_for_new(&self, asked: Option<&str>) -> anyhow::Result<String> {
        let asked = asked.map(str::trim).filter(|r| !r.is_empty());
        match (&self.realm, asked) {
            (Some(mine), Some(a)) if a != mine => Err(forbidden(format!("this token is limited to realm {mine}"))),
            (Some(mine), _) => Ok(mine.clone()),
            (None, Some(a)) => Ok(a.to_owned()),
            (None, None) => Ok(HOME.to_owned()),
        }
    }
}

pub async fn list(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<api::Realm>> {
    let rows = sqlx::query(
        "SELECT * FROM realms ORDER BY CASE id WHEN 'root' THEN 0 WHEN 'home' THEN 1 WHEN 'commons' THEN 2 ELSE 3 END, created_at, id",
    )
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| api::Realm {
            id: r.get("id"),
            name: r.get("name"),
            description: r.get("description"),
            created_at: r.get("created_at"),
            created_by: r.get("created_by"),
            app: r.get("app"),
            reads_commons: r.get::<i64, _>("reads_commons") != 0,
        })
        .filter(|r| caller.sees(&r.id))
        .collect())
}

pub async fn exists(st: &HeadState, realm: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("SELECT 1 FROM realms WHERE id = ?").bind(realm).fetch_optional(&st.store.pool).await?.is_some())
}

/// A realm sessions can run in: one that exists, and not root (root's
/// only conversations are the assistant's administration).
pub async fn check_for_sessions(st: &HeadState, realm: &str) -> anyhow::Result<()> {
    if realm == ROOT {
        return Err(bad_request("sessions do not run in root; it is carbon6's administration (ask the assistant instead)"));
    }
    if realm == COMMONS {
        return Err(bad_request("sessions do not run in commons; realms publish into it, and you review what they publish"));
    }
    if !exists(st, realm).await? {
        return Err(bad_request(format!("no realm {realm:?}")));
    }
    Ok(())
}

/// Read-only shared infrastructure a realm's token needs to start
/// sessions: which workers, connections and models there are.
fn shared_read(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/workers" | "/api/v1/connections" | "/api/v1/commands" | "/api/v1/catalog" | "/api/v1/openapi.json" | "/api/v1/realms"
    ) || (path.starts_with("/api/v1/connections/") && path.ends_with("/models"))
}

/// Whether a realm-scoped caller may make this request at all, and if it
/// names an object, whether that object is in the caller's realm.
/// Everything not listed is carbon6's administration.
pub async fn admit(st: &HeadState, caller: &Caller, method: &Method, path: &str) -> Result<(), (StatusCode, String)> {
    let Some(realm) = caller.realm.as_deref() else { return Ok(()) };
    if method == Method::GET && shared_read(path) {
        return Ok(());
    }
    let segments: Vec<&str> = path.trim_start_matches("/api/v1/").split('/').collect();
    let owned = |table: &'static str, noun: &'static str, id: String| async move {
        // Memory notes are records of the memory collection.
        let sql = if table == "memories" {
            "SELECT realm FROM records WHERE collection = 'memory' AND id = ?".to_owned()
        } else {
            format!("SELECT realm FROM {table} WHERE id = ?")
        };
        let found = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&st.store.pool)
            .await
            .ok()
            .flatten()
            .map(|r| r.get::<String, _>("realm"));
        match found {
            Some(r) if r == realm => Ok(()),
            // Another realm's object is as good as missing.
            _ => Err((StatusCode::NOT_FOUND, format!("no such {noun}"))),
        }
    };
    match segments.as_slice() {
        // Collections whose lists and creations are filtered by realm.
        ["sessions"] | ["search"] | ["decisions"] | ["decisions", "count"] | ["memories"] | ["automations"] | ["triggers"] => Ok(()),
        ["sessions", id, ..] => owned("sessions", "session", (*id).to_owned()).await,
        ["decisions", id, ..] => owned("decisions", "decision", (*id).to_owned()).await,
        ["memories", id, ..] => owned("memories", "note", (*id).to_owned()).await,
        ["automations" | "triggers", id, ..] => owned("triggers", "trigger", (*id).to_owned()).await,
        // Starting a template's sessions (in the caller's realm).
        ["templates", _, "run"] if method == Method::POST => Ok(()),
        // Collections: within the caller's realm, or another's through a
        // grant (checked by each call).
        ["collections", ..] => Ok(()),
        // The memex: the head puts the caller's realm on every call.
        ["memex", ..] | ["mail", "attachment"] => Ok(()),
        _ => Err((StatusCode::FORBIDDEN, format!("this token is limited to realm {realm}; that is carbon6's administration"))),
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !id.starts_with('-')
}

/// A new realm, with its own system collections (it inherits root's).
pub async fn create(st: &HeadState, r: &api::RealmIn, by: &str) -> anyhow::Result<api::Realm> {
    let id = r.id.trim();
    if !valid_id(id) {
        return Err(bad_request("a realm's id is lowercase letters, digits and - (up to 32)"));
    }
    let name = if r.name.trim().is_empty() { id } else { r.name.trim() };
    let done = sqlx::query("INSERT OR IGNORE INTO realms (id, name, description, created_at, created_by) VALUES (?, ?, ?, ?, ?)")
        .bind(id)
        .bind(name)
        .bind(r.description.trim())
        .bind(c6_core::now_ms())
        .bind(by)
        .execute(&st.store.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(bad_request(format!("there is already a realm {id:?}")));
    }
    crate::lists::ensure(&st.store.pool).await?;
    st.registry.bump();
    list(st, &Caller::root()).await?.into_iter().find(|x| x.id == id).ok_or_else(|| anyhow::anyhow!("realm gone"))
}

pub async fn update(st: &HeadState, id: &str, r: &api::RealmIn) -> anyhow::Result<api::Realm> {
    let current = list(st, &Caller::root()).await?.into_iter().find(|x| x.id == id).ok_or_else(|| crate::not_found("no such realm"))?;
    sqlx::query("UPDATE realms SET name = ?, description = ?, reads_commons = ? WHERE id = ?")
        .bind(if r.name.trim().is_empty() { current.name.as_str() } else { r.name.trim() })
        .bind(r.description.trim())
        .bind(r.reads_commons.unwrap_or(current.reads_commons))
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    list(st, &Caller::root()).await?.into_iter().find(|x| x.id == id).ok_or_else(|| anyhow::anyhow!("realm gone"))
}

/// Deletes a realm that owns nothing yet (removing an app's realm with its
/// data is the app's uninstall).
pub async fn delete(st: &HeadState, id: &str) -> anyhow::Result<()> {
    if id == ROOT || id == HOME || id == COMMONS {
        return Err(bad_request(format!("{id} is carbon6's own and stays")));
    }
    if !exists(st, id).await? {
        return Err(crate::not_found("no such realm"));
    }
    let mut owned = Vec::new();
    for (table, what, cond) in [
        ("sessions", "sessions", ""),
        ("records", "records", " AND collection NOT IN (SELECT name FROM collections WHERE realm = records.realm AND system = 1)"),
        ("collections", "collections", " AND system = 0"),
        ("decisions", "decisions", ""),
        ("triggers", "triggers", ""),
        ("secrets", "secrets", ""),
    ] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table} WHERE realm = ?{cond}")))
            .bind(id)
            .fetch_one(&st.store.pool)
            .await?;
        if n > 0 {
            owned.push(format!("{n} {what}"));
        }
    }
    if !owned.is_empty() {
        return Err(bad_request(format!("realm {id} still owns {}", owned.join(", "))));
    }
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM records WHERE realm = ?").bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM collections WHERE realm = ?").bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM api_tokens WHERE realm = ?").bind(id).execute(&mut *tx).await?;
    // Its grants end (their log stays).
    sqlx::query("UPDATE grants SET state = 'revoked', revoked_at = ?1 WHERE (from_realm = ?2 OR to_realm = ?2) AND state IN ('active', 'requested')")
        .bind(c6_core::now_ms())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM realms WHERE id = ?").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    // Its knowledge in the memex goes too (when the memex is next there).
    crate::memex::enqueue(st, "memex.drop_realm", serde_json::json!({"realm": id})).await;
    st.registry.bump();
    Ok(())
}

/// Sets which realms a worker or connection is available to (empty: all).
pub async fn set_availability(st: &HeadState, table: &'static str, id: &str, realms: &[String]) -> anyhow::Result<()> {
    for r in realms {
        if !exists(st, r).await? || r == ROOT || r == COMMONS {
            return Err(bad_request(format!("no realm {r:?} that sessions run in")));
        }
    }
    let done = sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {table} SET realms = ? WHERE id = ?")))
        .bind(serde_json::to_string(realms)?)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(crate::not_found("no such worker or connection"));
    }
    st.registry.bump();
    Ok(())
}

/// Whether a worker or connection with these realms serves `realm`.
pub fn available(realms_json: &str, realm: &str) -> bool {
    let realms: Vec<String> = serde_json::from_str(realms_json).unwrap_or_default();
    realms.is_empty() || realms.iter().any(|r| r == realm)
}

/// The realm a session runs in.
pub async fn of_session(st: &HeadState, session_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query("SELECT realm FROM sessions WHERE id = ?")
        .bind(session_id)
        .fetch_optional(&st.store.pool)
        .await?
        .map(|r| r.get("realm")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_objects_go_in_the_callers_realm() {
        assert_eq!(Caller::root().realm_for_new(None).unwrap(), HOME);
        assert_eq!(Caller::root().realm_for_new(Some("lab")).unwrap(), "lab");
        assert_eq!(Caller::in_realm("lab").realm_for_new(None).unwrap(), "lab");
        assert_eq!(Caller::in_realm("lab").realm_for_new(Some(" lab ")).unwrap(), "lab");
        assert!(Caller::in_realm("lab").realm_for_new(Some("home")).is_err());
        assert!(Caller::root().sees("anything") && Caller::in_realm("lab").sees("lab") && !Caller::in_realm("lab").sees("home"));
    }
}
