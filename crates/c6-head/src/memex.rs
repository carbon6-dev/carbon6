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

/// Calls the memex now. Err if none is connected.
pub async fn call(st: &HeadState, method: &str, params: Value) -> anyhow::Result<Value> {
    let id = st.registry.memex().ok_or_else(|| bad_request("no memex is connected (Settings → Memex)"))?;
    st.registry.call(id, method, params, CALL_TIMEOUT).await.map_err(|e| {
        // What the memex refuses is about the request; losing it is not.
        if e.contains("not connected") || e.contains("disconnected") || e.contains("in time") {
            anyhow::anyhow!("memex: {e}")
        } else {
            bad_request(format!("memex: {e}"))
        }
    })
}

/// Tells the memex its settings changed (if one is connected).
pub async fn reload(st: &HeadState) {
    if st.registry.memex().is_some() {
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
            if st.registry.memex().is_none() {
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
    for m in crate::memory::list(st).await? {
        enqueue(st, "memex.ingest", json!(memory_item(&m.id, &m.scope, &m.text))).await;
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

pub async fn check_settings(st: &HeadState, s: &api::MemexSettings) -> anyhow::Result<()> {
    if let Some(m) = &s.embedding {
        let connections = st.store.connections().await?;
        let c = connections
            .iter()
            .find(|c| c.id == m.connection || c.name == m.connection)
            .ok_or_else(|| bad_request(format!("no connection {:?}", m.connection)))?;
        if c.kind == "anthropic" || c.kind == "claude_code" {
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
        if what == "transcription" && c.kind == "anthropic" {
            return Err(bad_request("transcription: choose an OpenAI or OpenAI-compatible connection (one with /audio/transcriptions)"));
        }
        if m.model.trim().is_empty() {
            return Err(bad_request(format!("choose a model for {what}")));
        }
    }
    if s.calls_per_day == Some(0) {
        return Err(bad_request("calls a day: at least 1 (or leave the model unset to turn it off)"));
    }
    Ok(())
}

pub fn check_folders(folders: &[api::MemexFolder]) -> anyhow::Result<()> {
    for f in folders {
        if !f.path.starts_with('/') && !f.path.starts_with('~') {
            return Err(bad_request(format!("{:?}: a folder is a full path on the memex's machine", f.path)));
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

/// What the memex asks for at start: its models, with each connection's id
/// (it fetches credentials for them like a worker does).
pub async fn config(st: &HeadState) -> anyhow::Result<Value> {
    let s = settings(st).await?;
    let connections = st.store.connections().await?;
    let task = |m: &Option<api::MemexModel>| {
        m.as_ref().and_then(|m| {
            connections
                .iter()
                .find(|c| c.id == m.connection || c.name == m.connection)
                .map(|c| json!({"connection_id": c.id, "kind": c.kind, "model": m.model}))
        })
    };
    let embedding = task(&s.embedding);
    let folders: Vec<api::MemexFolder> = serde_json::from_str(&st.store.setting(FOLDERS).await?.unwrap_or_default()).unwrap_or_default();
    Ok(json!({
        "embedding": embedding,
        "folders": folders,
        "google": crate::google::for_memex(st).await,
        "google_people": crate::google::endpoints().people,
        "contacts_auto": s.contacts_auto.unwrap_or(false),
        "understanding": task(&s.understanding),
        "vision": task(&s.vision),
        "transcription": task(&s.transcription),
        "calls_per_day": s.calls_per_day.unwrap_or(500),
    }))
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
pub async fn brief(st: &HeadState, labels: &[String], text: &str) -> String {
    let Some(id) = st.registry.memex() else { return String::new() };
    let chars = settings(st).await.ok().and_then(|s| s.brief_chars).unwrap_or(4000);
    if chars == 0 {
        return String::new();
    }
    let scope = scope_for(st, labels).await;
    let params = json!({"labels": labels, "text": text, "chars": chars, "scope": scope});
    match st.registry.call(id, "memex.brief", params, BRIEF_TIMEOUT).await {
        Ok(v) => serde_json::from_value::<api::MemexBrief>(v).map(|b| b.text).unwrap_or_default(),
        Err(e) => {
            debug!("memex brief: {e}");
            String::new()
        }
    }
}

pub async fn status(st: &HeadState) -> anyhow::Result<api::MemexStatus> {
    let outbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memex_outbox").fetch_one(&st.store.pool).await?;
    let node = st.registry.memex();
    let mut s = match node {
        Some(_) => match call(st, "memex.status", json!({})).await {
            Ok(v) => serde_json::from_value(v).unwrap_or_default(),
            Err(e) => api::MemexStatus { last_error: Some(format!("{e:#}")), ..Default::default() },
        },
        None => api::MemexStatus::default(),
    };
    s.connected = node.is_some();
    s.node = match node {
        Some(id) => st.store.worker(id).await?.map(|w| w.name),
        None => None,
    };
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
                enqueue(st, "memex.ingest", json!(item)).await;
            }
            Some(Kind::Memory(m)) if m.op == "forget" => {
                enqueue(st, "memex.delete", json!({"source": "memory", "external_id": m.id})).await;
            }
            Some(Kind::Memory(m)) => {
                let mut item = memory_item(&m.id, &m.scope, &m.text);
                item.meta = json!({"session_id": session_id});
                enqueue(st, "memex.ingest", json!(item)).await;
            }
            Some(Kind::ToolResult(r)) if is_web_tool(&r.tool) && !r.is_error => {
                if let Some(item) = web_item(st, &session, ev, r).await {
                    enqueue(st, "memex.ingest", json!(item)).await;
                }
            }
            Some(Kind::TurnEnded(_)) if session.parent_call_id.is_none() => {
                if let Some(item) = digest(st, &session, labels.clone()).await {
                    enqueue(st, "memex.ingest", json!(item)).await;
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
