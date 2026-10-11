//! Google APIs for the memex: requests with short-lived access tokens from
//! the head (`google:<account>` credentials), and where each sync is.

use std::sync::Arc;
use std::time::Duration;

use c6_worker::link::HeadLink;
use serde_json::Value;
use sqlx::Row;

use crate::store::Store;

/// A Google account the memex syncs (from the head's config).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Account {
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub name: String,
}

/// A Google API error, with its status (404 and 410 mean "start over").
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Google: {} {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<ApiError>().map(|a| a.status)
}

pub struct Google {
    link: Arc<HeadLink>,
    http: reqwest::Client,
    /// The People API's address (it is not on the main API host).
    pub people: std::sync::Mutex<String>,
}

impl Google {
    pub fn new(link: Arc<HeadLink>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("HTTP client");
        Self { link, http, people: std::sync::Mutex::new("https://people.googleapis.com".into()) }
    }

    /// Calls a Google API: `path` under the main API host, or a full URL.
    pub async fn call(
        &self,
        account: &str,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let grant = self.link.credential(&format!("google:{account}")).await?;
        let url = if path.starts_with("http") { path.to_owned() } else { format!("{}{path}", grant.base_url.trim_end_matches('/')) };
        let mut req = self.http.request(method, &url).bearer_auth(&grant.bearer).query(query);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let message = v["error"]["message"].as_str().unwrap_or(&text).chars().take(300).collect();
            return Err(ApiError { status: status.as_u16(), message }.into());
        }
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    pub async fn get(&self, account: &str, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> {
        self.call(account, reqwest::Method::GET, path, query, None).await
    }

    pub fn people_url(&self, path: &str) -> String {
        format!("{}{path}", self.people.lock().unwrap().trim_end_matches('/'))
    }
}

/// A sync's place.
#[derive(Clone, Debug, Default)]
pub struct SyncState {
    pub cursor: Option<String>,
    pub page_token: Option<String>,
    pub phase: String,
    pub done: i64,
}

pub async fn state(store: &Store, account: &str, kind: &str) -> anyhow::Result<SyncState> {
    let r =
        sqlx::query("SELECT * FROM sync_state WHERE account = ? AND kind = ?").bind(account).bind(kind).fetch_optional(&store.pool).await?;
    Ok(match r {
        Some(r) => SyncState { cursor: r.get("cursor"), page_token: r.get("page_token"), phase: r.get("phase"), done: r.get("done") },
        None => SyncState { phase: "import".into(), ..Default::default() },
    })
}

pub async fn save_state(store: &Store, account: &str, kind: &str, s: &SyncState, error: Option<&str>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO sync_state (account, kind, cursor, page_token, phase, done, error, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(account, kind) DO UPDATE SET cursor = excluded.cursor, page_token = excluded.page_token, phase = excluded.phase, \
         done = excluded.done, error = excluded.error, updated_at = excluded.updated_at",
    )
    .bind(account)
    .bind(kind)
    .bind(&s.cursor)
    .bind(&s.page_token)
    .bind(&s.phase)
    .bind(s.done)
    .bind(error)
    .bind(c6_core::now_ms())
    .execute(&store.pool)
    .await?;
    Ok(())
}

pub async fn set_error(store: &Store, account: &str, kind: &str, error: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE sync_state SET error = ?, updated_at = ? WHERE account = ? AND kind = ?")
        .bind(error)
        .bind(c6_core::now_ms())
        .bind(account)
        .bind(kind)
        .execute(&store.pool)
        .await?;
    Ok(())
}

/// Each sync's state, for the UI.
pub async fn states(store: &Store) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query("SELECT * FROM sync_state ORDER BY account, kind").fetch_all(&store.pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "account": r.get::<String, _>("account"), "kind": r.get::<String, _>("kind"), "phase": r.get::<String, _>("phase"),
                "done": r.get::<i64, _>("done"), "error": r.get::<Option<String>, _>("error"), "updated_at": r.get::<i64, _>("updated_at"),
            })
        })
        .collect())
}

/// Free-mail domains: an address there says nothing about an organization.
const FREE_MAIL: [&str; 14] = [
    "gmail.com",
    "googlemail.com",
    "yahoo.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "fastmail.com",
    "gmx.com",
];

/// The person with this address (made on first sight), and their
/// organization when the domain names one.
pub async fn person(store: &Store, email: &str, name: &str) -> anyhow::Result<String> {
    let email = email.trim().to_lowercase();
    let key = format!("person:{email}");
    let id = match store.thing_by_key(&key).await? {
        Some(id) => {
            if !name.trim().is_empty() {
                store.rename_if_unnamed(&id, name.trim(), &email).await?;
            }
            id
        }
        None => store.ensure_thing(&key, "person", if name.trim().is_empty() { &email } else { name.trim() }).await?,
    };
    if let Some(domain) = email.rsplit_once('@').map(|(_, d)| d.to_owned())
        && !FREE_MAIL.contains(&domain.as_str())
        && !domain.is_empty()
    {
        let org = store.ensure_thing(&format!("org:{domain}"), "org", &domain).await?;
        store.link_things(&id, "member_of", &org, "observed", "mail").await?;
    }
    Ok(id)
}
