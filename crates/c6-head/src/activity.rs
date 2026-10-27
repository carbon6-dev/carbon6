//! The activity panel (docs/activity.md): model calls going on and just
//! done, sessions running, and what each connector's sync is doing. Read
//! from memory and from what syncs already store; nothing is recorded for
//! it, and a restart forgets it.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::{Value, json};
use sqlx::Row;

use crate::HeadState;

/// A connector's sync round: one scheduled run of an app's kit tool, which
/// runs the kit again while it says there's more (an import under way).
#[derive(Clone, Debug, Default, serde::Serialize)]
struct Round {
    started_at: i64,
    ended_at: Option<i64>,
    /// Kit runs so far in this round.
    runs: u32,
    items: u64,
    records: u64,
    errors: u64,
    error: String,
}

/// Each kit tool's round going on, or its last: by realm/app/kit.tool.
static ROUNDS: LazyLock<Mutex<HashMap<String, Round>>> = LazyLock::new(Default::default);

pub(crate) fn round_started(key: &str) {
    ROUNDS.lock().unwrap().insert(key.to_owned(), Round { started_at: c6_core::now_ms(), ..Default::default() });
}

/// A kit run of the round is done: what it brought in.
pub(crate) fn round_ran(key: &str, result: &Value) {
    if let Some(r) = ROUNDS.lock().unwrap().get_mut(key) {
        let n = |k: &str| result["ingested"][k].as_u64().unwrap_or(0);
        r.runs += 1;
        r.items += n("items");
        r.records += n("records");
        r.errors += result["ingested"]["errors"].as_array().map_or(0, |e| e.len() as u64);
    }
}

pub(crate) fn round_ended(key: &str, error: Option<String>) {
    if let Some(r) = ROUNDS.lock().unwrap().get_mut(key) {
        r.ended_at = Some(c6_core::now_ms());
        r.error = error.unwrap_or_default();
    }
}

/// A call as the list shows it: its prompt, thinking and answer only as
/// their last line (the whole of them is `call`).
fn listed(c: c6_llm::activity::Call, names: &HashMap<String, String>) -> Value {
    let last = |s: &str| {
        s.trim_end().rsplit('\n').next().unwrap_or("").chars().rev().take(160).collect::<Vec<_>>().into_iter().rev().collect::<String>()
    };
    let mut v = json!(c);
    v["connection_name"] = json!(names.get(&c.connection).cloned().unwrap_or_default());
    v["latest"] = json!(if c.text.is_empty() { last(&c.thinking) } else { last(&c.text) });
    // Only how long each is; the call itself has them.
    if let Some(o) = v.as_object_mut() {
        for (k, text) in [("prompt", &c.prompt), ("thinking", &c.thinking), ("text", &c.text)] {
            o.remove(k);
            o.insert(format!("{k}_chars"), json!(text.chars().count()));
        }
    }
    v
}

/// One call, whole: its prompt, thinking and answer so far.
pub async fn call(st: &HeadState, id: u64) -> anyhow::Result<Option<Value>> {
    let names: HashMap<String, String> = st.store.connections().await?.into_iter().map(|c| (c.id, c.name)).collect();
    Ok(c6_llm::activity::calls().into_iter().find(|c| c.id == id).map(|c| {
        let mut v = json!(c);
        v["connection_name"] = json!(names.get(&c.connection).cloned().unwrap_or_default());
        v
    }))
}

/// What's going on, in numbers (for the top bar).
pub async fn counts(st: &HeadState) -> anyhow::Result<Value> {
    let calls = c6_llm::activity::calls().iter().filter(|c| c.ended_at.is_none()).count();
    let syncing = ROUNDS.lock().unwrap().values().filter(|r| r.ended_at.is_none()).count();
    let sessions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE status = 'running' AND archived = 0").fetch_one(&st.store.pool).await?;
    Ok(json!({"calls": calls, "syncing": syncing, "sessions": sessions}))
}

/// Everything the panel shows.
pub async fn get(st: &HeadState) -> anyhow::Result<Value> {
    let names: HashMap<String, String> = st.store.connections().await?.into_iter().map(|c| (c.id, c.name)).collect();
    let calls: Vec<Value> = c6_llm::activity::calls().into_iter().map(|c| listed(c, &names)).collect();
    let sessions: Vec<Value> = sqlx::query(
        "SELECT id, title, realm, model, connection_id, worker_name, updated_at FROM sessions WHERE status = 'running' AND archived = 0 \
         ORDER BY updated_at DESC LIMIT 50",
    )
    .fetch_all(&st.store.pool)
    .await?
    .iter()
    .map(|r| {
        let connection: String = r.get("connection_id");
        json!({"id": r.get::<String, _>("id"), "title": r.get::<String, _>("title"), "realm": r.get::<String, _>("realm"),
               "model": r.get::<String, _>("model"), "connection_name": names.get(&connection).cloned().unwrap_or_default(),
               "worker": r.get::<String, _>("worker_name"), "updated_at": r.get::<i64, _>("updated_at")})
    })
    .collect();
    Ok(json!({"calls": calls, "sessions": sessions, "connectors": connectors(st).await?, "memex": memex(st).await}))
}

/// Each app's scheduled syncs: their schedule, the round going on (or the
/// last), and what the sync keeps about each account.
async fn connectors(st: &HeadState) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, name, realm, owner, cron, enabled, last_run_at, last_error, action_params FROM triggers WHERE action = 'kit' ORDER BY realm, owner, name",
    )
    .fetch_all(&st.store.pool)
    .await?;
    let rounds = ROUNDS.lock().unwrap().clone();
    let mut details: HashMap<String, (Value, Vec<c6_api::SlackWorkspace>)> = HashMap::new();
    let mut out = Vec::new();
    for r in rows {
        let realm: String = r.get("realm");
        let app: String = r.get("owner");
        let p: Value = serde_json::from_str(&r.get::<String, _>("action_params")).unwrap_or_default();
        let key = format!("{realm}/{app}/{}.{}", p["kit"].as_str().unwrap_or_default(), p["tool"].as_str().unwrap_or_default());
        if !details.contains_key(&realm) {
            let google = crate::google::status(st, &realm).await.unwrap_or_default();
            let slack = crate::slack::setup(st, &realm, None).await.map(|s| s.workspaces).unwrap_or_default();
            details.insert(realm.clone(), (google, slack));
        }
        let (google, slack) = &details[&realm];
        // The accounts this app syncs, and where each is.
        let mut accounts: Vec<Value> =
            google["sync"].as_array().into_iter().flatten().filter(|s| s["app"].as_str() == Some(app.as_str())).cloned().collect();
        if p["kit"].as_str().is_some_and(|k| k.contains("slack")) {
            accounts.extend(slack.iter().map(|w| {
                json!({"account": w.name, "kind": "slack", "conversations": w.conversations, "imported": w.imported,
                       "files_waiting": w.files_waiting, "retry_after": w.retry_after, "error": w.error, "updated_at": w.synced_at})
            }));
        }
        out.push(json!({
            "id": r.get::<String, _>("id"), "name": r.get::<String, _>("name"), "realm": realm, "app": app,
            "cron": r.get::<String, _>("cron"), "enabled": r.get::<i64, _>("enabled") != 0,
            "last_run_at": r.get::<Option<i64>, _>("last_run_at"), "last_error": r.get::<Option<String>, _>("last_error"),
            "round": rounds.get(&key), "accounts": accounts,
        }));
    }
    Ok(out)
}

/// The memex's own work: folder scans, extraction, what waits to be sorted
/// for the brief, model calls left today.
async fn memex(st: &HeadState) -> Value {
    let status = crate::memex::status(st).await.ok();
    let realms: Vec<String> = sqlx::query_scalar("SELECT id FROM realms").fetch_all(&st.store.pool).await.unwrap_or_default();
    let mut sorting = Vec::new();
    // Commons is shared knowledge: no mail or chat of its own to sort.
    for realm in realms.iter().filter(|r| crate::memex::has_memex(r) && *r != crate::realms::COMMONS) {
        if let Ok(v) = crate::memex::call_in(st, realm, "memex.triage.status", json!({})).await {
            let walk = crate::memex::call_in(st, realm, "memex.relations.progress", json!({})).await.unwrap_or(Value::Null);
            sorting.push(json!({"realm": realm, "status": v, "walk": walk}));
        }
    }
    json!({"status": status, "sorting": sorting})
}
