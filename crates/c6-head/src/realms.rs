//! Realms (docs/os.md §3) organise data and settings; they don't isolate
//! agents. Signed-in users and root API tokens reach every realm. The head
//! enforces realm-scoped API requests and refuses administrative operations
//! to realm tokens. Those checks do not isolate worker files or delegation.

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
    /// Your browser (a realm's page or the console), not a token: it may
    /// learn what waits in other realms (docs/realms-ux.md §6).
    pub browser: bool,
    /// A verified browser sign-in, not just a page header with sign-in off.
    /// Only authentication middleware sets this; agent calls never do.
    pub authenticated_browser: bool,
    /// The console's assistant, acting in this realm with your approval
    /// (docs/assistant.md §Realms): what it writes is the console's.
    pub console: bool,
    /// An API token's display name, never its secret. Some("") means an
    /// API caller with no known token name (sign-in off).
    pub api_token_name: Option<String>,
}

impl Caller {
    pub fn root() -> Self {
        Self { realm: None, browser: false, authenticated_browser: false, console: false, api_token_name: None }
    }

    pub fn in_realm(realm: &str) -> Self {
        Self { realm: Some(realm.to_owned()), browser: false, authenticated_browser: false, console: false, api_token_name: None }
    }

    pub fn api_origin(&self) -> Option<c6_proto::ApiMessageOrigin> {
        self.api_token_name.as_ref().map(|name| c6_proto::ApiMessageOrigin { token_name: name.clone() })
    }

    /// The page path represents the person when sign-in is off. With sign-in
    /// on, it also needs a verified cookie. Tokens never become browser input.
    pub fn human_origin(&self, auth_required: bool) -> bool {
        self.browser && (!auth_required || self.authenticated_browser) && self.api_token_name.is_none()
    }

    /// Whose decisions and the like a caller lists: its realm's; the
    /// console's are root's own (carbon6's administration: installs,
    /// grants); a root token's, every realm's.
    pub fn listing_realm(&self) -> Option<&str> {
        match (&self.realm, self.browser) {
            (Some(r), _) => Some(r),
            (None, true) => Some(ROOT),
            (None, false) => None,
        }
    }

    /// Whether the caller may learn about realms other than its own.
    pub fn sees_others(&self) -> bool {
        self.realm.is_none() || self.browser
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
    let (generation, cached) = st.store.realm_cache.get(realm);
    if cached == Some(true) {
        return Ok(true);
    }
    let exists = sqlx::query("SELECT 1 FROM realms WHERE id = ?").bind(realm).fetch_optional(&st.store.pool).await?.is_some();
    // Missing realms aren't cached: creating a realm is visible immediately.
    if exists {
        st.store.realm_cache.insert(generation, realm.to_owned(), true);
    }
    Ok(exists)
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

/// Whether a realm's caller may make this request: by its operation's
/// scope (docs/realms-ux.md §3), and, if it names an object, whether that
/// object is in the caller's realm. Root callers may do anything.
pub async fn admit(st: &HeadState, caller: &Caller, method: &Method, path: &str) -> Result<(), (StatusCode, String)> {
    use crate::catalog::Scope;
    let Some(realm) = caller.realm.as_deref() else { return Ok(()) };
    match crate::catalog::route_scope(method.as_str(), path) {
        Scope::Device | Scope::Shared => return Ok(()),
        Scope::Admin => return Err((StatusCode::FORBIDDEN, format!("this is limited to realm {realm}; that is carbon6's administration"))),
        Scope::Realm => {}
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
        // What you viewed lately is narrowed to the caller's realm itself.
        ["sessions", "recent"] => Ok(()),
        ["sessions", id, ..] => owned("sessions", "session", (*id).to_owned()).await,
        ["decisions", id, ..] if *id != "count" => owned("decisions", "decision", (*id).to_owned()).await,
        ["memories", id, ..] => owned("memories", "note", (*id).to_owned()).await,
        ["automations" | "triggers", id, ..] => owned("triggers", "trigger", (*id).to_owned()).await,
        ["workflows", "runs", id, ..] => owned("workflow_runs", "workflow run", (*id).to_owned()).await,
        // A worker's files and terminals: only on a worker serving it.
        ["workers", id, ..] => {
            let realms: Option<String> =
                sqlx::query_scalar("SELECT realms FROM workers WHERE id = ?").bind(*id).fetch_optional(&st.store.pool).await.ok().flatten();
            match realms {
                Some(r) if available(&r, realm) => Ok(()),
                _ => Err((StatusCode::NOT_FOUND, "no such worker".into())),
            }
        }
        // Its own realm in the explorer, and its own apps.
        ["realms", id, ..] | ["apps", id, ..] if *id != realm => Err((StatusCode::NOT_FOUND, "no such realm".into())),
        // Everything else narrows to the caller's realm itself: lists,
        // collections, series, views, the memex, secrets, settings lists.
        _ => Ok(()),
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
    // The console's place (docs/realms-ux.md §3).
    if id == crate::auth::ADMIN_PLACE {
        return Err(bad_request("\"admin\" is the console's name, not a realm's"));
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
        ("series", "series", " AND system = 0"),
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
    delete_rows(st, id).await
}

/// Keep COMMIT and both invalidations together even if the requesting
/// stream is cancelled while SQLite is finishing the transaction.
async fn delete_rows(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let st = st.arc().ok_or_else(|| anyhow::anyhow!("the head stopped"))?;
    let id = id.to_owned();
    tokio::spawn(async move {
        let store = &st.store;
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM records WHERE realm = ?").bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM collections WHERE realm = ?").bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM points WHERE realm = ?").bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM series WHERE realm = ?").bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM api_tokens WHERE realm = ?").bind(&id).execute(&mut *tx).await?;
        // Its grants end (their log stays).
        sqlx::query("UPDATE grants SET state = 'revoked', revoked_at = ?1 WHERE (from_realm = ?2 OR to_realm = ?2) AND state IN ('active', 'requested')")
            .bind(c6_core::now_ms())
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM realms WHERE id = ?").bind(&id).execute(&mut *tx).await?;
        // Queue knowledge cleanup atomically: a cancelled request must not
        // leave the deleted realm's knowledge behind when its id is reused.
        sqlx::query("INSERT INTO memex_outbox (method, params, created_at) VALUES ('memex.drop_realm', ?, ?)")
            .bind(serde_json::json!({"realm": id}).to_string())
            .bind(c6_core::now_ms())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        store.auth_cache.invalidate();
        store.realm_cache.invalidate();
        st.memex_feed.wake();
        st.registry.bump();
        Ok::<_, anyhow::Error>(())
    })
    .await?
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
    serves(&realms, realm)
}

/// Whether a worker or connection kept to `realms` (none: every realm)
/// serves `realm`.
pub fn serves(realms: &[String], realm: &str) -> bool {
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

    #[tokio::test]
    async fn brief_realm_cache_deletion_revokes_tokens_and_object_checks_stay_live() {
        use futures::FutureExt;

        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            ..Default::default()
        })
        .await
        .unwrap();
        let st = &head.state;
        assert!(!exists(st, "lab").await.unwrap());
        create(st, &api::RealmIn { id: "lab".into(), name: "Lab".into(), ..Default::default() }, "you").await.unwrap();
        assert!(exists(st, "lab").await.unwrap(), "missing realms must not be negatively cached");
        assert!(exists(st, "home").await.unwrap());

        let root = st.store.create_api_token("root", None).await.unwrap().token.unwrap();
        let scoped = st.store.create_api_token("lab", Some("lab")).await.unwrap().token.unwrap();
        let url = format!("http://{}/api/v1/sessions", head.ui_addrs[0]);
        for token in [&root, &scoped] {
            assert_eq!(st.http.get(&url).bearer_auth(token).send().await.unwrap().status(), StatusCode::OK);
        }
        // Both cache key and returned value are realm-specific; no request
        // can borrow a previous request's successful ownership decision.
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, realm) \
             VALUES ('owned', 'w', 'Owned', '', 'c', 'm', 'idle', 0, 0, 'lab')",
        )
        .execute(&st.store.pool)
        .await
        .unwrap();
        let path = "/api/v1/sessions/owned";
        assert!(admit(st, &Caller::in_realm("lab"), &Method::GET, path).await.is_ok());
        assert_eq!(admit(st, &Caller::in_realm("home"), &Method::GET, path).await.unwrap_err().0, StatusCode::NOT_FOUND);
        sqlx::query("UPDATE sessions SET realm = 'home' WHERE id = 'owned'").execute(&st.store.pool).await.unwrap();
        assert_eq!(admit(st, &Caller::in_realm("lab"), &Method::GET, path).await.unwrap_err().0, StatusCode::NOT_FOUND);
        assert!(admit(st, &Caller::in_realm("home"), &Method::GET, path).await.is_ok());

        delete(st, "lab").await.unwrap();
        assert!(!exists(st, "lab").await.unwrap(), "a warm realm entry cannot survive deletion");
        assert!(exists(st, "home").await.unwrap());
        assert_eq!(st.http.get(&url).bearer_auth(&scoped).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(st.http.get(&url).bearer_auth(&root).send().await.unwrap().status(), StatusCode::OK);
        // Re-creating the same realm must not resurrect its revoked token.
        create(st, &api::RealmIn { id: "lab".into(), name: "Lab again".into(), ..Default::default() }, "you").await.unwrap();
        assert!(exists(st, "lab").await.unwrap());
        assert_eq!(st.http.get(&url).bearer_auth(&scoped).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

        let mut connections = Vec::new();
        for _ in 0..8 {
            connections.push(st.store.pool.acquire().await.unwrap());
        }
        // Don't count async pool setup against the product's two-second TTL.
        let generation = st.store.realm_cache.get("lab").0;
        st.store.realm_cache.insert(generation, "lab".into(), true);
        assert!(exists(st, "lab").now_or_never().expect("a warm realm check must not acquire a database connection").unwrap());
        for connection in &mut connections {
            let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous").fetch_one(&mut **connection).await.unwrap();
            assert_eq!(synchronous, 2, "every head pool connection must retain synchronous=FULL");
        }
        drop(connections);

        let recreated = st.store.create_api_token("recreated lab", Some("lab")).await.unwrap().token.unwrap();
        assert_eq!(st.http.get(&url).bearer_auth(&recreated).send().await.unwrap().status(), StatusCode::OK);
        // The sender may consume the outbox immediately. Audit the insert
        // rather than racing its delivery when checking durable cleanup.
        sqlx::query("CREATE TABLE cleanup_audit (params TEXT NOT NULL)").execute(&st.store.pool).await.unwrap();
        sqlx::query("CREATE TRIGGER audit_cleanup AFTER INSERT ON memex_outbox WHEN new.method = 'memex.drop_realm' BEGIN INSERT INTO cleanup_audit VALUES (new.params); END")
            .execute(&st.store.pool).await.unwrap();
        let auth_generation = st.store.auth_cache.get("").0;
        let realm_generation = st.store.realm_cache.get("").0;
        let writer = st.store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        // Cancel the await after handing off the transaction. Its writer
        // lock is gated so it cannot have committed before cancellation.
        assert!(delete_rows(st, "lab").now_or_never().is_none());
        writer.commit().await.unwrap();
        c6_testkit::eventually("cancelled realm deletion invalidates realm and auth caches", async || {
            (st.store.auth_cache.get("").0 != auth_generation && st.store.realm_cache.get("").0 != realm_generation).then_some(())
        })
        .await;
        assert!(!exists(st, "lab").await.unwrap());
        let cleanup: i64 = sqlx::query_scalar("SELECT count(*) FROM cleanup_audit WHERE json_extract(params, '$.realm') = 'lab'")
            .fetch_one(&st.store.pool)
            .await
            .unwrap();
        assert_eq!(cleanup, 1, "cancelled deletion durably queues knowledge cleanup");
        assert_eq!(st.http.get(&url).bearer_auth(&recreated).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        head.shutdown().await.unwrap();
    }

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
