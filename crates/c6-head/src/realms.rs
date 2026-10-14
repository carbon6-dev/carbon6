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
    let rows = sqlx::query("SELECT * FROM realms ORDER BY CASE id WHEN 'root' THEN 0 WHEN 'home' THEN 1 ELSE 2 END, created_at, id")
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
        ["sessions"] | ["search"] | ["decisions"] | ["decisions", "count"] | ["memories"] | ["automations"] => Ok(()),
        ["sessions", id, ..] => owned("sessions", "session", (*id).to_owned()).await,
        ["decisions", id, ..] => owned("decisions", "decision", (*id).to_owned()).await,
        ["memories", id, ..] => owned("memories", "note", (*id).to_owned()).await,
        ["automations", id, ..] => owned("automations", "automation", (*id).to_owned()).await,
        // Starting a template's sessions (in the caller's realm).
        ["templates", _, "run"] if method == Method::POST => Ok(()),
        // Collections: always addressed within the caller's realm.
        ["collections", ..] => Ok(()),
        // The memex: the head puts the caller's realm on every call.
        ["memex", ..] | ["mail", "attachment"] => Ok(()),
        _ => Err((StatusCode::FORBIDDEN, format!("this token is limited to realm {realm}; that is carbon6's administration"))),
    }
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
