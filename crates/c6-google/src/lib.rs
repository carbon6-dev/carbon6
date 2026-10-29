//! The google kit (docs/knowledge.md §6): what the gmail, google-calendar
//! and google-contacts apps run. It talks to Google with the access tokens
//! the head gives it (`C6_ACCOUNTS`) and never stores anything itself: it
//! reads where each sync is (`state/sync_state.json`, the app's records)
//! and an operation's input (`input.json`), and writes what changed as an
//! ingest batch (`out/ingest.json`) the head keeps as the app's, with an
//! operation's answer in `out/result.json`.
//!
//! What it writes is typed for the knowledge store's mail, calendar and
//! contacts stores, which any mail, calendar or contacts app feeds the same
//! way: messages as `message/rfc822` (their Gmail labels as attributes),
//! calendars, events (Google's own JSON) and contacts (the People API's).

pub mod calendar;
pub mod contacts;
pub mod mail;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

/// An account the app connected: its id (the head's), address and token.
#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub token: String,
}

/// Where Google is (tests point it elsewhere), and who.
pub struct Env {
    pub accounts: String,
    pub api: String,
    pub people: String,
}

impl Env {
    pub fn from_env() -> Self {
        let get = |k: &str, d: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_owned());
        Self {
            accounts: get("C6_ACCOUNTS", "[]"),
            api: get("C6_GOOGLE_API_URL", "https://www.googleapis.com"),
            people: get("C6_GOOGLE_PEOPLE_URL", "https://people.googleapis.com"),
        }
    }
}

/// A Google API error, with its status (404 and 410 mean "start over").
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub rate_limit: bool,
    /// Retry-After, as an absolute time in milliseconds.
    pub retry_at: Option<i64>,
}

impl ApiError {
    pub(crate) fn response(status: u16, text: &str, retry_after: Option<&str>, now: i64) -> Self {
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let e = &v["error"];
        let message: String = e["message"].as_str().unwrap_or(text).chars().take(300).collect();
        let reason = e["errors"].as_array().into_iter().flatten().any(|e| {
            matches!(e["reason"].as_str(), Some("rateLimitExceeded" | "userRateLimitExceeded" | "quotaExceeded" | "dailyLimitExceeded"))
        }) || e["details"].as_array().into_iter().flatten().any(|e| e["reason"] == "RATE_LIMIT_EXCEEDED")
            || e["status"] == "RESOURCE_EXHAUSTED";
        let rate_limit = status == 429
            || (status == 403
                && (reason
                    || ["Quota exceeded", "Rate Limit Exceeded", "User-rate limit exceeded"].iter().any(|s| message.starts_with(s))));
        let retry_at = retry_after.and_then(|s| {
            s.parse::<i64>()
                .ok()
                .map(|seconds| now.saturating_add(seconds.max(0).saturating_mul(1000)))
                .or_else(|| chrono::DateTime::parse_from_rfc2822(s).ok().map(|d| d.timestamp_millis()))
        });
        Self { status, message, rate_limit, retry_at }
    }
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

/// A run: Google, the accounts, where each sync is, and what it found.
pub struct Run {
    http: reqwest::Client,
    pub api: String,
    pub people: String,
    pub accounts: Vec<Account>,
    /// The app's collection where its syncs are, and its records by key.
    pub collection: String,
    pub state: BTreeMap<String, Value>,
    pub input: Value,
    pub items: Vec<Value>,
    pub deleted: Vec<Value>,
    /// sync_state records to keep, by key.
    pub records: BTreeMap<String, Value>,
    pub files: BTreeMap<String, Vec<u8>>,
    /// Run again soon (an import is under way).
    pub again: bool,
    pub result: Value,
    pub errors: Vec<String>,
}

impl Run {
    pub fn new(env: &Env, collection: &str, state: &Value, input: Value) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).timeout(Duration::from_secs(120)).build()?;
        let list: Vec<Value> = serde_json::from_str(&env.accounts)?;
        let accounts = list
            .iter()
            .filter(|a| a["provider"].as_str().is_none_or(|p| p == "google"))
            .map(|a| Account {
                id: a["id"].as_str().unwrap_or_default().to_owned(),
                email: a["account"].as_str().unwrap_or_default().to_owned(),
                token: a["access_token"].as_str().unwrap_or_default().to_owned(),
            })
            .collect();
        let state = state
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let f = r.get("fields").unwrap_or(r);
                Some((f["key"].as_str()?.to_owned(), f.clone()))
            })
            .collect();
        Ok(Self {
            http,
            api: env.api.trim_end_matches('/').to_owned(),
            people: env.people.trim_end_matches('/').to_owned(),
            accounts,
            collection: collection.to_owned(),
            state,
            input,
            items: Vec::new(),
            deleted: Vec::new(),
            records: BTreeMap::new(),
            files: BTreeMap::new(),
            again: false,
            result: Value::Null,
            errors: Vec::new(),
        })
    }

    /// The account an operation names (by id or address).
    pub fn account(&self, which: &str) -> anyhow::Result<Account> {
        self.accounts
            .iter()
            .find(|a| a.id == which || a.email.eq_ignore_ascii_case(which))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no connected account {which:?}"))
    }

    /// Calls a Google API: `path` under the main API host, or a full URL.
    pub async fn call(
        &self,
        a: &Account,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let url = if path.starts_with("http") { path.to_owned() } else { format!("{}{path}", self.api) };
        let mut req = self.http.request(method, &url).bearer_auth(&a.token).query(query);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ApiError::response(status.as_u16(), &text, retry_after.as_deref(), chrono::Utc::now().timestamp_millis()).into());
        }
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    pub async fn get(&self, a: &Account, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> {
        self.call(a, reqwest::Method::GET, path, query, None).await
    }

    /// Where a sync is (a new one: importing).
    pub fn sync(&self, key: &str) -> Value {
        self.state.get(key).cloned().unwrap_or_else(|| json!({"key": key, "phase": "import", "done": 0}))
    }

    /// Keeps where a sync is (and its trouble, if any).
    pub fn save(&mut self, key: &str, mut s: Value, error: Option<String>) {
        s["key"] = json!(key);
        s["error"] = json!(error);
        s["updated_at"] = json!(chrono::Utc::now().timestamp_millis());
        self.state.insert(key.to_owned(), s.clone());
        self.records.insert(key.to_owned(), s);
    }

    /// A typed JSON item for the knowledge store.
    pub fn json_item(&mut self, mime: &str, external_id: &str, body: Value) {
        self.items.push(json!({"type": mime, "external_id": external_id, "text": body.to_string()}));
    }

    /// The batch for the head.
    pub fn batch(&self) -> Value {
        let records: Vec<Value> =
            self.records.values().map(|f| json!({"collection": self.collection, "key": "key", "fields": f})).collect();
        json!({"items": self.items, "deleted": self.deleted, "records": records, "again": self.again, "errors": self.errors})
    }
}

/// Percent-encodes a path segment.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Runs a tool in `work` (its `state/<collection>.json`, where the app
/// keeps its syncs, and `input.json`), writing `out/`.
pub async fn run(tool: &str, work: &Path, env: &Env) -> anyhow::Result<()> {
    let read = |p: &Path| -> Value { std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null) };
    let state_file = std::fs::read_dir(work.join("state"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "json"));
    let collection = state_file.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let state = state_file.as_deref().map(read).unwrap_or(Value::Null);
    let mut r = Run::new(env, &collection, &state, read(&work.join("input.json")))?;
    let outcome = match tool {
        "mail-sync" => mail::sync(&mut r).await,
        "mail-modify" => mail::modify(&mut r).await,
        "mail-send" => mail::send(&mut r).await,
        "mail-draft" => mail::draft(&mut r).await,
        "calendar-sync" => calendar::sync(&mut r).await,
        "calendar-write" => calendar::write(&mut r).await,
        "contacts-sync" => contacts::sync(&mut r).await,
        "contacts-write" => contacts::write(&mut r).await,
        other => Err(anyhow::anyhow!("no tool {other:?}")),
    };
    let out = work.join("out");
    std::fs::create_dir_all(&out)?;
    for (path, data) in &r.files {
        let p = out.join(path);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, data)?;
    }
    std::fs::write(out.join("ingest.json"), r.batch().to_string())?;
    if !r.result.is_null() {
        std::fs::write(out.join("result.json"), r.result.to_string())?;
    }
    outcome
}
