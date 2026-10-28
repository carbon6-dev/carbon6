//! The head's side of the memex (docs/memex.md): what carbon6 sends it
//! (session digests, outputs, web pages read, memory notes), through a
//! durable outbox; its settings and scopes; and context briefs for new
//! sessions.

use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use c6_proto::{SessionEvent, session_event};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::store::SessionRow;
use crate::{HeadState, bad_request};

/// Settings keys.
pub const SETTINGS: &str = "memex";
pub const SCOPES: &str = "memex_scopes";
pub const FOLDERS: &str = "memex_folders";
const MEMORIES_SENT: &str = "memex_memories_sent";

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const WAITING_TIMEOUT: Duration = Duration::from_secs(120);
/// A session's brief must not hold up its start for long.
const BRIEF_TIMEOUT: Duration = Duration::from_secs(4);
/// Session digests keep the latest turns within this many characters.
const DIGEST_CHARS: usize = 16_000;

/// Wakes the outbox sender.
#[derive(Default)]
pub struct Feed {
    wake: Notify,
}

impl Feed {
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// Whether `realm` keeps knowledge in the memex. Root is carbon6's
/// administration, with none.
pub fn has_memex(realm: &str) -> bool {
    realm != crate::realms::ROOT
}

/// `params` for a call on behalf of `realm` (the memex keeps each realm's
/// data apart).
pub fn in_realm(mut params: Value, realm: &str) -> Value {
    if let Some(o) = params.as_object_mut() {
        o.insert("realm".into(), json!(realm));
    }
    params
}

/// Calls the memex now, for `realm`. Err if none is connected.
pub async fn call_in(st: &HeadState, realm: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    if !has_memex(realm) {
        return Err(bad_request(format!("realm {realm} keeps no knowledge in the memex")));
    }
    call(st, method, in_realm(params, realm)).await
}

/// A memex method called by a person (`made_by`), through the API or an
/// operation: what the head answers itself (the Google apps' accounts and
/// syncs), checks (ingest's labels) or adds (the graph's reader policies)
/// first, and never as an app.
pub async fn call_as(st: &HeadState, realm: &str, method: &str, params: Value, made_by: &str) -> anyhow::Result<Value> {
    // Ingest from outside is a batch of typed items, with labels checked as
    // uploads are; the head's own generic items are not sent through here.
    if method == "memex.ingest" {
        let batch: api::MemexIngest =
            serde_json::from_value(params.clone()).map_err(|e| bad_request(format!("memex.ingest takes {{\"items\": [...]}}: {e}")))?;
        for item in &batch.items {
            crate::sessions::check_labels(&item.labels)?;
        }
    }
    // The mail client's accounts and syncs are the Google apps'.
    match method {
        "memex.google" => return crate::google::status(st, realm).await,
        "memex.google_sync" => {
            let st = st.arc().ok_or_else(|| anyhow::anyhow!("the head stopped"))?;
            crate::google::sync_now(&st, realm).await?;
            return Ok(json!({}));
        }
        _ => {}
    }
    let mut params = if params.is_object() { params } else { json!({}) };
    params["made_by"] = json!(made_by);
    // Only the head writes as an app (its kits' output).
    if let Some(o) = params.as_object_mut() {
        o.remove("as");
    }
    // The graph shows what the realm's reader policies believe (docs/apps.md §9.3).
    if method == "memex.graph" {
        params["policies"] = json!(crate::claims::policies(st, realm).await);
    }
    call_in(st, realm, method, params).await
}

/// Calls the knowledge store (in the head, docs/knowledge.md).
pub async fn call(st: &HeadState, method: &str, params: Value) -> anyhow::Result<Value> {
    // Ingest and uploads wait a while for what a worker reads (extract.rs).
    let limit = if matches!(method, "memex.ingest" | "memex.upload") { WAITING_TIMEOUT } else { CALL_TIMEOUT };
    call_within(st, method, params, limit).await
}

async fn call_within(st: &HeadState, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
    let m = crate::knowledge::get(st).ok_or_else(|| anyhow::anyhow!("the knowledge store is starting"))?;
    match tokio::time::timeout(timeout, m.handle_method(method, params)).await {
        Ok(Ok(v)) => Ok(v),
        // What it refuses is about the request.
        Ok(Err(e)) => Err(bad_request(format!("memex: {e:#}"))),
        Err(_) => Err(anyhow::anyhow!("memex: {method} took longer than {}s", timeout.as_secs())),
    }
}

/// Whether the knowledge store is open.
pub fn ready(st: &HeadState) -> bool {
    crate::knowledge::get(st).is_some()
}

/// When briefs come and what may interrupt (per realm; docs/brief.md §2.3).
pub const BRIEF: &str = "brief";

/// Tells the knowledge store its settings changed.
pub async fn reload(st: &HeadState) {
    if ready(st) {
        let _ = call(st, "memex.reload", serde_json::json!({})).await;
    }
}

/// Queues a call for the memex; it is delivered in order, when one is
/// connected, and kept until then.
pub async fn enqueue(st: &HeadState, method: &str, params: Value) {
    let r = sqlx::query("INSERT INTO memex_outbox (method, params, created_at) VALUES (?, ?, ?)")
        .bind(method)
        .bind(params.to_string())
        .bind(c6_core::now_ms())
        .execute(&st.store.pool)
        .await;
    match r {
        Ok(_) => st.memex_feed.wake(),
        Err(e) => warn!("queueing for the memex: {e:#}"),
    }
}

/// Sends the outbox whenever a memex is connected (and on its first
/// connection, your memory notes).
pub fn spawn(st: Arc<HeadState>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = st.memex_feed.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            if !ready(&st) {
                continue;
            }
            if let Err(e) = send_memories(&st).await {
                warn!("sending memory notes to the memex: {e:#}");
            }
            if let Err(e) = drain(&st).await {
                debug!("memex outbox: {e:#}");
            }
        }
    });
}

async fn drain(st: &HeadState) -> anyhow::Result<()> {
    loop {
        let rows = sqlx::query("SELECT id, method, params FROM memex_outbox ORDER BY id LIMIT 50").fetch_all(&st.store.pool).await?;
        if rows.is_empty() {
            return Ok(());
        }
        for r in rows {
            let id: i64 = r.get("id");
            let params: Value = serde_json::from_str(&r.get::<String, _>("params")).unwrap_or(Value::Null);
            match call(st, &r.get::<String, _>("method"), params).await {
                Ok(_) => {
                    sqlx::query("DELETE FROM memex_outbox WHERE id = ?").bind(id).execute(&st.store.pool).await?;
                }
                Err(e) => {
                    let attempts: i64 = sqlx::query_scalar(
                        "UPDATE memex_outbox SET attempts = attempts + 1, last_error = ? WHERE id = ? RETURNING attempts",
                    )
                    .bind(format!("{e:#}"))
                    .bind(id)
                    .fetch_one(&st.store.pool)
                    .await?;
                    // One the memex keeps refusing would block the rest.
                    if attempts >= 5 {
                        warn!(id, "dropping a memex update it refused 5 times: {e:#}");
                        sqlx::query("DELETE FROM memex_outbox WHERE id = ?").bind(id).execute(&st.store.pool).await?;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}

/// The memory notes made before a memex was connected, sent once.
async fn send_memories(st: &HeadState) -> anyhow::Result<()> {
    if st.store.setting(MEMORIES_SENT).await?.is_some() {
        return Ok(());
    }
    let home = crate::realms::Caller::in_realm(crate::realms::HOME);
    for m in crate::memory::list(st, &home).await? {
        enqueue(st, "memex.ingest", in_realm(json!(memory_item(&m.id, &m.scope, &m.text)), crate::realms::HOME)).await;
    }
    st.store.set_setting(MEMORIES_SENT, "1").await
}

pub fn memory_item(id: &str, scope: &str, text: &str) -> api::MemexItemIn {
    api::MemexItemIn {
        kind: "note".into(),
        source: "memory".into(),
        external_id: id.into(),
        title: text.chars().take(80).collect(),
        text: text.into(),
        labels: if scope.is_empty() { Vec::new() } else { vec![scope.to_owned()] },
        ..Default::default()
    }
}

pub async fn settings(st: &HeadState) -> anyhow::Result<api::MemexSettings> {
    Ok(serde_json::from_str(&st.store.setting(SETTINGS).await?.unwrap_or_default()).unwrap_or_default())
}

pub async fn scopes(st: &HeadState) -> anyhow::Result<Vec<api::MemexScope>> {
    Ok(serde_json::from_str(&st.store.setting(SCOPES).await?.unwrap_or_default()).unwrap_or_default())
}

/// Why a member of a pool can't serve the memex's `what` model, if it can't.
fn unfit(what: &str, kind: &str) -> Option<&'static str> {
    match (what, kind) {
        ("embedding", "anthropic" | "chatgpt") => Some("has no embedding models (choose OpenAI or OpenAI-compatible members, e.g. Ollama)"),
        ("transcription", "anthropic" | "chatgpt") => Some("has no /audio/transcriptions (choose OpenAI or OpenAI-compatible members)"),
        _ => None,
    }
}

/// A pool's members, for the memex's `what` model: each must fit it.
async fn check_pool_members(st: &HeadState, what: &str, pool: &str, members: &[api::PoolMember]) -> anyhow::Result<()> {
    for m in members {
        let Some(c) = st.store.connection(&m.connection_id).await? else { continue };
        if let Some(why) = unfit(what, &c.kind) {
            return Err(bad_request(format!("{what}: {} in pool {pool} {why}", c.name)));
        }
    }
    Ok(())
}

/// A pool's new members, for what the memex uses it for.
pub async fn check_pool_use(st: &HeadState, pool_id: &str, members: &[api::PoolMember]) -> anyhow::Result<()> {
    let s = settings(st).await?;
    let Some(pool) = st.store.connection(pool_id).await? else { return Ok(()) };
    let uses =
        [("embedding", &s.embedding), ("understanding", &s.understanding), ("vision", &s.vision), ("transcription", &s.transcription)];
    for (what, m) in uses {
        if m.as_ref().is_some_and(|m| m.connection == pool.id || m.connection == pool.name) {
            check_pool_members(st, what, &pool.name, members).await?;
        }
    }
    Ok(())
}

pub async fn check_settings(st: &HeadState, s: &api::MemexSettings) -> anyhow::Result<()> {
    if let Some(m) = &s.embedding {
        let connections = st.store.connections().await?;
        let c = connections
            .iter()
            .find(|c| c.id == m.connection || c.name == m.connection)
            .ok_or_else(|| bad_request(format!("no connection {:?}", m.connection)))?;
        if c.kind == crate::pools::KIND {
            check_pool_members(st, "embedding", &c.name, &crate::pools::members(c)).await?;
        }
        if matches!(c.kind.as_str(), "anthropic" | "claude_code" | "chatgpt") {
            return Err(bad_request(format!(
                "{} has no embedding models; choose an OpenAI or OpenAI-compatible (e.g. Ollama) connection",
                c.name
            )));
        }
        if m.model.trim().is_empty() {
            return Err(bad_request("choose an embedding model"));
        }
    }
    let connections = st.store.connections().await?;
    for (what, m) in [("understanding", &s.understanding), ("vision", &s.vision), ("transcription", &s.transcription)] {
        let Some(m) = m else { continue };
        let c = connections
            .iter()
            .find(|c| c.id == m.connection || c.name == m.connection)
            .ok_or_else(|| bad_request(format!("{what}: no connection {:?}", m.connection)))?;
        if c.kind == "claude_code" {
            return Err(bad_request(format!("{what}: the memex calls models directly; choose an API connection, not {}", c.name)));
        }
        if let Some(e) = m.effort.as_deref().filter(|e| !matches!(*e, "" | "none" | "low" | "medium" | "high" | "xhigh" | "max")) {
            return Err(bad_request(format!("{what}: effort is none, low, medium, high, xhigh or max, not {e:?}")));
        }
        if what == "transcription" && matches!(c.kind.as_str(), "anthropic" | "chatgpt") {
            return Err(bad_request("transcription: choose an OpenAI or OpenAI-compatible connection (one with /audio/transcriptions)"));
        }
        if c.kind == crate::pools::KIND {
            check_pool_members(st, what, &c.name, &crate::pools::members(c)).await?;
        }
        if m.model.trim().is_empty() {
            return Err(bad_request(format!("choose a model for {what}")));
        }
    }
    if s.face_detector.as_deref().is_some_and(|p| !p.trim().is_empty()) != s.face_embedder.as_deref().is_some_and(|p| !p.trim().is_empty())
    {
        return Err(bad_request("faces need both models: a detector and an embedder"));
    }
    if s.calls_at_once.is_some_and(|n| n == 0 || n > MAX_AT_ONCE) {
        return Err(bad_request(format!("calls at once is 1 to {MAX_AT_ONCE}")));
    }
    if s.calls_per_day == Some(0) {
        return Err(bad_request("calls a day: at least 1 (or leave the model unset to turn it off)"));
    }
    Ok(())
}

pub fn check_folders(folders: &[api::MemexFolder]) -> anyhow::Result<()> {
    for f in folders {
        if !f.path.starts_with('/') && !f.path.starts_with('~') {
            return Err(bad_request(format!("{:?}: a folder is a full path on its worker", f.path)));
        }
        crate::sessions::check_labels(&f.labels)?;
    }
    Ok(())
}

pub fn check_scopes(scopes: &[api::MemexScope]) -> anyhow::Result<()> {
    for s in scopes {
        if s.name.trim().is_empty() {
            return Err(bad_request("a scope rule needs a name"));
        }
        if let Some(k) = s.kinds.iter().find(|k| !api::MEMEX_KINDS.contains(&k.as_str())) {
            return Err(bad_request(format!("scope {:?}: {k:?} is not a kind ({})", s.name, api::MEMEX_KINDS.join(", "))));
        }
        crate::sessions::check_selector(&s.applies_to)?;
    }
    Ok(())
}

/// The most model calls understanding may make side by side.
pub const MAX_AT_ONCE: u32 = 8;

/// What the memex asks for at start: its models, with each connection's id
/// (it fetches credentials for them like a worker does).
pub async fn config(st: &HeadState) -> anyhow::Result<Value> {
    let s = settings(st).await?;
    let connections = st.store.connections().await?;
    // Understanding sorts and drafts many times a day: by default it doesn't
    // reason (a thinking model would spend a minute on each); vision a little.
    let task_with = |m: &Option<api::MemexModel>, effort: &str| {
        m.as_ref().and_then(|m| {
            connections.iter().find(|c| c.id == m.connection || c.name == m.connection).map(|c| {
                let effort = m.effort.as_deref().filter(|e| !e.is_empty()).unwrap_or(effort);
                json!({"connection_id": c.id, "kind": c.kind, "model": m.model, "effort": effort})
            })
        })
    };
    let task = |m: &Option<api::MemexModel>| task_with(m, "low");
    let embedding = task(&s.embedding);
    let folders: Vec<api::MemexFolder> = serde_json::from_str(&st.store.setting(FOLDERS).await?.unwrap_or_default()).unwrap_or_default();
    Ok(json!({
        "embedding": embedding,
        "folders": folders,
        "accounts": accounts_by_realm(st).await,
        "contacts_auto": s.contacts_auto.unwrap_or(false),
        "understanding": task_with(&s.understanding, "none"),
        "prompts": crate::prompts::for_memex(st, crate::realms::HOME).await,
        "realm_prompts": crate::prompts::for_memex_realms(st).await,
        "brief": briefs(st).await,
        "vision": task(&s.vision),
        "transcription": task(&s.transcription),
        "calls_per_day": s.calls_per_day.unwrap_or(500),
        "calls_at_once": s.calls_at_once.unwrap_or(1).clamp(1, crate::memex::MAX_AT_ONCE),
        "places_file": s.places_file,
        "face_detector": s.face_detector,
        "face_embedder": s.face_embedder,
    }))
}

/// Each realm's Brief setting, by realm.
async fn briefs(st: &HeadState) -> Value {
    let realms: Vec<String> = sqlx::query_scalar("SELECT id FROM realms").fetch_all(&st.store.pool).await.unwrap_or_default();
    let mut out = serde_json::Map::new();
    for r in realms.into_iter().filter(|r| r != crate::realms::ROOT) {
        let s: api::BriefSetting =
            st.store.setting_for(BRIEF, &r).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        out.insert(r, json!(s));
    }
    Value::Object(out)
}

/// Each realm's accounts that apps connected for mail, calendars and
/// contacts (docs/knowledge.md §6): {realm: [...]}.
async fn accounts_by_realm(st: &HeadState) -> Value {
    let mut out = serde_json::Map::new();
    let realms: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT realm FROM oauth_accounts").fetch_all(&st.store.pool).await.unwrap_or_default();
    for r in realms {
        if let Ok(Value::Array(a)) = crate::providers::feeding(st, &r).await
            && !a.is_empty()
        {
            out.insert(r, Value::Array(a));
        }
    }
    Value::Object(out)
}

/// The kinds a session with `labels` may see.
pub async fn scope_for(st: &HeadState, labels: &[String]) -> Vec<String> {
    let mut kinds: Vec<String> = api::MEMEX_DEFAULT_SCOPE.iter().map(|k| k.to_string()).collect();
    for s in scopes(st).await.unwrap_or_default() {
        if s.enabled && api::labels::applies(&s.applies_to, labels) {
            for k in s.kinds {
                if !kinds.contains(&k) {
                    kinds.push(k);
                }
            }
        }
    }
    kinds
}

/// The labels worth turning into things: all but model: (which changes).
fn item_labels(session: &SessionRow) -> Vec<String> {
    session.all_labels().into_iter().filter(|l| !l.starts_with("model:")).collect()
}

/// A brief for a new session about `labels`, or "" (no memex, nothing
/// known, or it took too long).
pub async fn brief(st: &HeadState, realm: &str, labels: &[String], text: &str) -> String {
    if !ready(st) || !has_memex(realm) {
        return String::new();
    }
    let chars = settings(st).await.ok().and_then(|s| s.brief_chars).unwrap_or(4000);
    if chars == 0 {
        return String::new();
    }
    let scope = scope_for(st, labels).await;
    let one = async |realm: &str| {
        let params = json!({"labels": labels, "text": text, "chars": chars, "scope": scope, "realm": realm});
        match call_within(st, "memex.brief", params, BRIEF_TIMEOUT).await {
            Ok(v) => serde_json::from_value::<api::MemexBrief>(v).map(|b| b.text).unwrap_or_default(),
            Err(e) => {
                debug!("memex brief: {e}");
                String::new()
            }
        }
    };
    let own = one(realm).await;
    // What commons knows about the same things (docs/os.md §3.3).
    if !crate::grants::reads_commons(st, realm).await {
        return own;
    }
    match one(crate::realms::COMMONS).await {
        c if c.trim().is_empty() => own,
        c if own.trim().is_empty() => format!("From commons (shared by every realm):\n{c}"),
        c => format!("{own}\n\nFrom commons (shared by every realm):\n{c}"),
    }
}

pub async fn status(st: &HeadState) -> anyhow::Result<api::MemexStatus> {
    let outbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memex_outbox").fetch_one(&st.store.pool).await?;
    let mut s = match call(st, "memex.status", json!({})).await {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => api::MemexStatus { last_error: Some(format!("{e:#}")), ..Default::default() },
    };
    // Part of the head (docs/knowledge.md).
    s.connected = ready(st);
    s.node = Some("head".into());
    s.outbox = outbox as u64;
    Ok(s)
}

/// Sends what a batch of a session's events adds to the memex: outputs,
/// web pages read, memory changes, and (at the end of a turn) the
/// session's digest.
pub async fn on_events(st: &HeadState, session_id: &str, events: &[SessionEvent]) {
    use session_event::Kind;
    let interesting = events.iter().any(|e| {
        matches!(e.kind, Some(Kind::Output(_) | Kind::Memory(_) | Kind::TurnEnded(_)))
            || matches!(&e.kind, Some(Kind::ToolResult(r)) if is_web_tool(&r.tool) && !r.is_error)
    });
    if !interesting {
        return;
    }
    let Ok(Some(session)) = st.store.session(session_id).await else { return };
    // What a session does goes to its realm's knowledge (root keeps none).
    if !has_memex(&session.realm) {
        return;
    }
    let realm = session.realm.clone();
    let labels = item_labels(&session);
    for ev in events {
        match &ev.kind {
            Some(Kind::Output(o)) => {
                let text = if is_text(&o.mime, &o.name) {
                    String::from_utf8_lossy(&o.data).chars().take(200_000).collect()
                } else {
                    String::new()
                };
                let item = api::MemexItemIn {
                    kind: "output".into(),
                    source: "carbon6".into(),
                    external_id: format!("{session_id}:{}", o.name),
                    title: if o.title.is_empty() { o.name.clone() } else { o.title.clone() },
                    text,
                    created_at: Some(ev.ts_ms),
                    labels: labels.clone(),
                    meta: json!({"session_id": session_id, "name": o.name, "mime": o.mime, "seq": ev.seq, "size": o.data.len()}),
                    ..Default::default()
                };
                enqueue(st, "memex.ingest", in_realm(json!(item), &realm)).await;
            }
            Some(Kind::Memory(m)) if m.op == "forget" => {
                enqueue(st, "memex.delete", in_realm(json!({"source": "memory", "external_id": m.id}), &realm)).await;
            }
            Some(Kind::Memory(m)) => {
                let mut item = memory_item(&m.id, &m.scope, &m.text);
                item.meta = json!({"session_id": session_id});
                enqueue(st, "memex.ingest", in_realm(json!(item), &realm)).await;
            }
            Some(Kind::ToolResult(r)) if is_web_tool(&r.tool) && !r.is_error => {
                if let Some(item) = web_item(st, &session, ev, r).await {
                    enqueue(st, "memex.ingest", in_realm(json!(item), &realm)).await;
                }
            }
            Some(Kind::TurnEnded(_)) if session.parent_call_id.is_none() => {
                if let Some(item) = digest(st, &session, labels.clone()).await {
                    enqueue(st, "memex.ingest", in_realm(json!(item), &realm)).await;
                }
            }
            _ => {}
        }
    }
}

fn is_web_tool(name: &str) -> bool {
    matches!(name, "web_fetch" | "WebFetch" | "browser" | "Browser")
}

fn is_text(mime: &str, name: &str) -> bool {
    mime.starts_with("text/")
        || matches!(mime, "application/json" | "application/xml")
        || [".md", ".txt", ".csv", ".json", ".html", ".htm", ".xml", ".yaml", ".yml"].iter().any(|e| name.ends_with(e))
}

/// A page the agent read: from web_fetch's input, or the browser's
/// "Page: … URL: …" header.
async fn web_item(st: &HeadState, session: &SessionRow, ev: &SessionEvent, r: &c6_proto::ToolResult) -> Option<api::MemexItemIn> {
    let (title, url, text) = if r.tool.eq_ignore_ascii_case("browser") {
        // Pages, not "Clicked …" or "Typed …" notes.
        let page = r.output.find("Page: ").map(|i| &r.output[i..])?;
        let mut lines = page.lines();
        let title = lines.next()?.strip_prefix("Page: ")?.to_owned();
        let url = lines.next()?.strip_prefix("URL: ")?.trim().to_owned();
        let body: String =
            page.splitn(3, '\n').nth(2).unwrap_or("").split("\n\nInteractive elements").next().unwrap_or("").trim().to_owned();
        (title, url, body)
    } else {
        let events = st.store.turn_events(&session.id, ev.seq).await.ok()?;
        let input = events.iter().rev().find_map(|e| match &e.kind {
            Some(session_event::Kind::Assistant(m)) => m.blocks.iter().find_map(|b| match &b.block {
                Some(c6_proto::content_block::Block::ToolUse(u)) if u.id == r.call_id => Some(u.input_json.clone()),
                _ => None,
            }),
            _ => None,
        })?;
        let url = serde_json::from_str::<Value>(&input).ok()?["url"].as_str()?.to_owned();
        (url.clone(), url, r.output.clone())
    };
    if text.trim().is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    Some(api::MemexItemIn {
        kind: "web".into(),
        source: "carbon6".into(),
        external_id: url.clone(),
        title,
        text: text.chars().take(200_000).collect(),
        url: Some(url),
        created_at: Some(ev.ts_ms),
        labels: item_labels(session),
        meta: json!({"session_id": session.id, "read_by": r.tool}),
    })
}

/// A session's digest: per turn, what was asked, what tools did, and the
/// answer; the latest turns within `DIGEST_CHARS`.
async fn digest(st: &HeadState, session: &SessionRow, labels: Vec<String>) -> Option<api::MemexItemIn> {
    use c6_proto::content_block::Block;
    let events = st.store.events_after(&session.id, 0, i64::MAX).await.ok()?;
    let mut turns: Vec<String> = Vec::new();
    let mut cur: Option<(String, Vec<String>, String)> = None;
    let flush = |cur: &mut Option<(String, Vec<String>, String)>, turns: &mut Vec<String>| {
        if let Some((asked, tools, answer)) = cur.take() {
            let mut t = format!("Asked: {}", clip(&asked, 1500));
            if !tools.is_empty() {
                t.push_str(&format!("\nDid: {}", clip(&tools.join("; "), 800)));
            }
            if !answer.trim().is_empty() {
                t.push_str(&format!("\nAnswer: {}", clip(answer.trim(), 2500)));
            }
            turns.push(t);
        }
    };
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::User(u)) if u.command != "task-notification" => {
                flush(&mut cur, &mut turns);
                cur = Some((u.text.clone(), Vec::new(), String::new()));
            }
            Some(session_event::Kind::Assistant(m)) => {
                if let Some((_, tools, answer)) = cur.as_mut() {
                    let text: String =
                        m.blocks.iter().filter_map(|b| if let Some(Block::Text(t)) = &b.block { Some(t.as_str()) } else { None }).collect();
                    if !text.trim().is_empty() {
                        *answer = text;
                    }
                    for b in &m.blocks {
                        if let Some(Block::ToolUse(u)) = &b.block {
                            tools.push(tool_line(&u.name, &u.input_json));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut turns);
    if turns.is_empty() {
        return None;
    }
    // The latest turns first in what is kept, then put back in order.
    let mut kept = Vec::new();
    let mut size = 0;
    for t in turns.iter().rev() {
        if size + t.len() > DIGEST_CHARS && !kept.is_empty() {
            break;
        }
        size += t.len();
        kept.push(t.clone());
    }
    kept.reverse();
    let mut text = String::new();
    if kept.len() < turns.len() {
        text.push_str(&format!("({} earlier turns not shown)\n\n", turns.len() - kept.len()));
    }
    text.push_str(&kept.join("\n\n"));
    Some(api::MemexItemIn {
        kind: "session".into(),
        source: "carbon6".into(),
        external_id: session.id.clone(),
        title: session.title.clone(),
        text,
        created_at: Some(session.created_at),
        labels,
        meta: json!({"session_id": session.id, "turns": turns.len(), "model": session.model, "worker": session.worker_name}),
        ..Default::default()
    })
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// One line for a tool call: its name and the gist of its input.
fn tool_line(name: &str, input: &str) -> String {
    let v: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let gist = ["command", "file_path", "path", "url", "query", "pattern", "action", "title", "description"]
        .iter()
        .find_map(|k| v[*k].as_str())
        .map(|s| clip(s, 120))
        .unwrap_or_default();
    if gist.is_empty() { name.to_owned() } else { format!("{name} {gist}") }
}

/// The `@memex:<key>` mentions in a message.
pub fn mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if let Some(key) = word.strip_prefix("@memex:") {
            let key = key.trim_end_matches(['.', ',', ';', '?', '!', ')']);
            if !key.is_empty() && !out.iter().any(|k| k == key) {
                out.push(key.to_owned());
            }
        }
    }
    out
}

/// What the memex knows about a thing (or an item), as Markdown for an
/// attachment.
pub async fn card(st: &HeadState, key: &str) -> anyhow::Result<String> {
    let v = call(st, "memex.get", json!({"id": key})).await?;
    if let Some(item) = v.get("item") {
        return Ok(format!(
            "# {} ({})\n\n{}",
            item["title"].as_str().unwrap_or(""),
            item["kind"].as_str().unwrap_or(""),
            clip(item["text"].as_str().unwrap_or(""), 8000)
        ));
    }
    let t = &v["thing"];
    let mut out = format!(
        "# {} ({}: {})\n",
        t["thing"]["name"].as_str().unwrap_or(""),
        t["thing"]["kind"].as_str().unwrap_or(""),
        t["thing"]["label"].as_str().unwrap_or("")
    );
    for (title, key) in [("Notes", "notes"), ("Recent items", "items")] {
        let rows: Vec<String> = t[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .take(12)
                    .map(|h| {
                        format!(
                            "- ({}) {}: {} [{}]",
                            h["kind"].as_str().unwrap_or(""),
                            h["title"].as_str().unwrap_or(""),
                            clip(h["snippet"].as_str().unwrap_or(""), 300),
                            h["id"].as_str().unwrap_or("")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !rows.is_empty() {
            out.push_str(&format!("\n## {title}\n{}\n", rows.join("\n")));
        }
    }
    Ok(out)
}
