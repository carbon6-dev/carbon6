//! The slack kit (docs/slack.md): what the slack app runs. It talks to
//! Slack's Web API with the user token the head gives it (`C6_ACCOUNTS`)
//! and keeps nothing itself: it reads where each workspace's sync is
//! (`state/slack_sync.json`, the app's records) and an operation's input
//! (`input.json`), and writes what changed as an ingest batch
//! (`out/ingest.json`) the head keeps as the app's, with an operation's
//! answer in `out/result.json`.
//!
//! What it writes: each message as a `chat` item (about its channel, its
//! author and who it mentions), each person and channel as a thing, and
//! shared documents as typed items the extract kit reads.

pub mod sync;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

/// A workspace the app connected: the head's id for it, the workspace's
/// id, its name, and your token there.
#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub team: String,
    pub name: String,
    pub token: String,
}

/// Where Slack is (tests point it elsewhere), who, and the pace.
pub struct Env {
    pub accounts: String,
    pub api: String,
    /// Requests a run makes per workspace at most.
    pub budget: u32,
    /// How often an active conversation's last week is read again (ms).
    pub reread_every: i64,
}

impl Env {
    pub fn from_env() -> Self {
        let get = |k: &str, d: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_owned());
        Self {
            accounts: get("C6_ACCOUNTS", "[]"),
            api: get("C6_SLACK_API_URL", "https://slack.com/api"),
            budget: BUDGET,
            reread_every: REREAD_EVERY,
        }
    }
}

/// Slack asked to slow down: try again after this many seconds.
#[derive(Debug)]
pub struct Limited(pub u64);

impl std::fmt::Display for Limited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slack is rate-limiting: try again in {}s", self.0)
    }
}

impl std::error::Error for Limited {}

/// Slack answered `ok: false`.
#[derive(Debug)]
pub struct SlackError(pub String);

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slack: {}", self.0)
    }
}

impl std::error::Error for SlackError {}

/// The Slack error a call failed with, if it did.
pub fn slack_error(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<SlackError>().map(|s| s.0.as_str())
}

/// Requests a run may make for each workspace: within Slack's limit for
/// history (about 50 a minute), since a run comes every two minutes.
pub const BUDGET: u32 = 40;
/// An active conversation's last week is read again this often.
pub const REREAD_EVERY: i64 = 60 * 60_000;
/// What a run downloads of shared files at most.
pub const FILE_BYTES: u64 = 24 * 1024 * 1024;
/// A file bigger than this keeps its name and link only.
pub const MAX_FILE: u64 = 40 * 1024 * 1024;

/// A run: Slack, the workspaces, where each sync is, and what it found.
pub struct Run {
    http: reqwest::Client,
    pub api: String,
    pub accounts: Vec<Account>,
    /// The app's collection where its syncs are, and its records by key.
    pub collection: String,
    pub state: BTreeMap<String, Value>,
    pub input: Value,
    pub items: Vec<Value>,
    pub things: Vec<Value>,
    pub deleted: Vec<Value>,
    /// Sync records to keep, by key.
    pub records: BTreeMap<String, Value>,
    pub files: BTreeMap<String, Vec<u8>>,
    /// Run again soon (an import is under way).
    pub again: bool,
    pub result: Value,
    pub errors: Vec<String>,
    /// Requests left for the workspace being synced, and each one's share.
    pub budget: u32,
    pub per_workspace: u32,
    pub reread_every: i64,
    pub file_bytes: u64,
}

impl Run {
    pub fn new(env: &Env, collection: &str, state: &Value, input: Value) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).timeout(Duration::from_secs(120)).build()?;
        let list: Vec<Value> = serde_json::from_str(&env.accounts)?;
        let accounts = list
            .iter()
            .filter(|a| a["provider"].as_str().is_none_or(|p| p == "slack"))
            .map(|a| Account {
                id: a["id"].as_str().unwrap_or_default().to_owned(),
                team: a["account"].as_str().unwrap_or_default().to_owned(),
                name: a["name"].as_str().unwrap_or_default().to_owned(),
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
            accounts,
            collection: collection.to_owned(),
            state,
            input,
            items: Vec::new(),
            things: Vec::new(),
            deleted: Vec::new(),
            records: BTreeMap::new(),
            files: BTreeMap::new(),
            again: false,
            result: Value::Null,
            errors: Vec::new(),
            budget: env.budget,
            per_workspace: env.budget,
            reread_every: env.reread_every,
            file_bytes: 0,
        })
    }

    /// The workspace an operation names (by the head's id or the
    /// workspace's).
    pub fn account(&self, which: &str) -> anyhow::Result<Account> {
        self.accounts
            .iter()
            .find(|a| a.id == which || a.team == which)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no Slack workspace {which:?} is connected"))
    }

    /// A Web API method. A 429 is `Limited`; `ok: false` is `SlackError`.
    pub async fn call(&mut self, a: &Account, method: &str, query: &[(&str, String)], body: Option<&Value>) -> anyhow::Result<Value> {
        self.budget = self.budget.saturating_sub(1);
        let url = format!("{}/{method}", self.api);
        let req = match body {
            Some(b) => self.http.post(&url).bearer_auth(&a.token).json(b),
            None => self.http.get(&url).bearer_auth(&a.token).query(query),
        };
        let resp = req.send().await?;
        if resp.status().as_u16() == 429 {
            let wait = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse().ok()).unwrap_or(60);
            return Err(Limited(wait).into());
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text)
            .map_err(|_| anyhow::anyhow!("Slack {method}: {status} {}", text.chars().take(200).collect::<String>()))?;
        if v["ok"] != true {
            return Err(SlackError(v["error"].as_str().unwrap_or("unknown error").to_owned()).into());
        }
        Ok(v)
    }

    /// A file's bytes (a shared file's private download address).
    pub async fn download(&mut self, a: &Account, url: &str) -> anyhow::Result<Vec<u8>> {
        self.budget = self.budget.saturating_sub(1);
        let resp = self.http.get(url).bearer_auth(&a.token).send().await?;
        if resp.status().as_u16() == 429 {
            let wait = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse().ok()).unwrap_or(60);
            return Err(Limited(wait).into());
        }
        anyhow::ensure!(resp.status().is_success(), "downloading a file: {}", resp.status());
        Ok(resp.bytes().await?.to_vec())
    }

    /// A sync record (new: empty but its key).
    pub fn record(&self, key: &str) -> Value {
        self.state.get(key).cloned().unwrap_or_else(|| json!({"key": key}))
    }

    /// Keeps a sync record.
    pub fn save(&mut self, key: &str, mut s: Value) {
        s["key"] = json!(key);
        s["updated_at"] = json!(now_ms());
        self.state.insert(key.to_owned(), s.clone());
        self.records.insert(key.to_owned(), s);
    }

    /// The batch for the head.
    pub fn batch(&self) -> Value {
        let records: Vec<Value> =
            self.records.values().map(|f| json!({"collection": self.collection, "key": "key", "fields": f})).collect();
        json!({"things": self.things, "items": self.items, "deleted": self.deleted, "records": records, "again": self.again, "errors": self.errors})
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
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
        "sync" => sync::sync(&mut r).await,
        "post" => sync::post(&mut r).await,
        "react" => sync::react(&mut r).await,
        "mark-read" => sync::mark_read(&mut r).await,
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
