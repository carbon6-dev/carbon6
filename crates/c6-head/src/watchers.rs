//! Watchers (docs/watchers.md): standing agents with their own agenda that
//! follow sessions and act on what they see as things happen.
//!
//! A watcher is dormant between wakes. The head notes the events it cares
//! about; a wake reads what changed since its position, and calls a model
//! only when something matched what wakes it. Each wake that calls a model
//! is a turn of the watcher's own session, which you can open, read and
//! write to like any other. Its position and notes live in
//! `watcher_state`, so compaction and restarts lose nothing. What it may do
//! (a note to the watched agent, a mark, a memory note, a decision, a task
//! card, a ledger line, a document) is code; what it looks for is its
//! agenda, an editable prompt; its definition is a setting.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use c6_api as api;
use c6_api::watchers::{Finding, SessionWatch, Watcher, WatcherDef, preinstalled, severity_rank};
use c6_core::now_ms;
use c6_llm::{Accumulator, Block, Message, ModelRequest, Role};
use c6_proto::session_event::Kind;
use c6_proto::{SessionEvent, SessionEventBatch, TurnEndReason, content_block};
use futures::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::store::SessionRow;
use crate::{HeadState, bad_request, not_found};

/// Your watcher definitions (edited shipped ones and your own).
pub const SETTING: &str = "watchers";
/// Every watcher session has this label, and `watcher:<name>`.
pub const LABEL: &str = "watcher";
/// Events read back from a watched session's position, at most.
const WINDOW: i64 = 400;
/// A digest's length, at most (the newest is kept).
const DIGEST_CHARS: usize = 12_000;
/// A diff's, for review.
const DIFF_CHARS: usize = 12_000;
/// What a watcher's notes may grow to.
const MEMORY_CHARS: usize = 4_000;

/// What runs now in this head: which watcher sessions are watchers', and
/// which wakes are under way (with what came in meanwhile).
#[derive(Default)]
pub struct Runtime {
    sessions: Mutex<HashSet<String>>,
    running: Mutex<HashMap<String, Vec<String>>>,
    /// Wakes under way or queued, for tests to wait on.
    busy: std::sync::atomic::AtomicUsize,
}

/// Whether any wake is under way (tests wait for none).
pub fn busy(st: &HeadState) -> bool {
    st.watchers.busy.load(std::sync::atomic::Ordering::SeqCst) > 0
}

// --- Definitions -------------------------------------------------------------

async fn stored(st: &HeadState) -> Vec<WatcherDef> {
    st.store.setting(SETTING).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// Every watcher: the shipped ones (yours where you changed them), then
/// your own.
pub async fn defs(st: &HeadState) -> Vec<WatcherDef> {
    let mine = stored(st).await;
    let as_shipped = st.watchers_as_shipped;
    let mut out: Vec<WatcherDef> = preinstalled()
        .into_iter()
        .map(|mut d| match mine.iter().find(|m| m.name == d.name) {
            Some(m) => m.clone(),
            None => {
                d.enabled &= as_shipped;
                d
            }
        })
        .collect();
    let shipped: HashSet<String> = out.iter().map(|d| d.name.clone()).collect();
    out.extend(mine.into_iter().filter(|m| !shipped.contains(&m.name)));
    out
}

pub async fn def(st: &HeadState, name: &str) -> Option<WatcherDef> {
    defs(st).await.into_iter().find(|d| d.name == name)
}

/// The watchers with how they're doing.
pub async fn list(st: &HeadState) -> anyhow::Result<Vec<Watcher>> {
    let mine = stored(st).await;
    let mut shipped = preinstalled();
    for d in &mut shipped {
        d.enabled &= st.watchers_as_shipped;
    }
    let day = today();
    let mut out = Vec::new();
    for d in defs(st).await {
        let stats = sqlx::query(
            "SELECT COUNT(*) AS n, COALESCE(SUM(wakes), 0) AS w, COALESCE(SUM(skips), 0) AS s FROM watcher_state WHERE watcher = ?",
        )
        .bind(&d.name)
        .fetch_one(&st.store.pool)
        .await?;
        let tokens: i64 = sqlx::query_scalar("SELECT tokens FROM watcher_spend WHERE watcher = ? AND day = ?")
            .bind(&d.name)
            .bind(&day)
            .fetch_optional(&st.store.pool)
            .await?
            .unwrap_or(0);
        let findings: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM watcher_findings WHERE watcher = ?").bind(&d.name).fetch_one(&st.store.pool).await?;
        let original = shipped.iter().find(|s| s.name == d.name);
        let edited = mine.iter().any(|m| m.name == d.name);
        out.push(Watcher {
            preinstalled: original.is_some(),
            edited: edited && original.is_some(),
            newer_default: edited && original.is_some_and(|o| *o != d),
            woke: stats.get("w"),
            skipped: stats.get("s"),
            tokens_today: tokens,
            sessions: stats.get("n"),
            findings,
            def: d,
        });
    }
    Ok(out)
}

fn check(d: &WatcherDef) -> anyhow::Result<()> {
    if d.name.is_empty() || !d.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(bad_request("a watcher's name is letters, digits and -"));
    }
    if d.agenda.trim().is_empty() {
        return Err(bad_request("a watcher needs an agenda (a prompt's name)"));
    }
    if !matches!(d.scope.as_str(), "session" | "all") {
        return Err(bad_request("scope is session or all"));
    }
    if !api::watchers::SEVERITIES.contains(&d.note_severity.as_str()) {
        return Err(bad_request("note_severity is info, important or urgent"));
    }
    if !d.pattern.is_empty() && regex::Regex::new(&d.pattern).is_err() {
        return Err(bad_request("the pattern is not a valid regular expression"));
    }
    Ok(())
}

/// Saves a watcher: yours from now on (a shipped one you change keeps your
/// version through updates).
pub async fn save(st: &HeadState, d: WatcherDef) -> anyhow::Result<WatcherDef> {
    check(&d)?;
    let mut mine = stored(st).await;
    mine.retain(|m| m.name != d.name);
    mine.push(d.clone());
    st.store.set_setting(SETTING, &serde_json::to_string(&mine)?).await?;
    Ok(d)
}

/// Takes back your version: a shipped watcher returns to its default, one
/// of your own is gone.
pub async fn reset(st: &HeadState, name: &str) -> anyhow::Result<()> {
    let mut mine = stored(st).await;
    let before = mine.len();
    mine.retain(|m| m.name != name);
    if before == mine.len() {
        return Err(not_found("you haven't changed that watcher"));
    }
    st.store.set_setting(SETTING, &serde_json::to_string(&mine)?).await
}

fn today() -> String {
    let secs = now_ms() / 1000;
    format!("{}", secs / 86_400)
}

async fn spent(st: &HeadState, watcher: &str) -> i64 {
    sqlx::query_scalar("SELECT tokens FROM watcher_spend WHERE watcher = ? AND day = ?")
        .bind(watcher)
        .bind(today())
        .fetch_optional(&st.store.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

async fn spend(st: &HeadState, watcher: &str, tokens: i64) {
    let _ = sqlx::query(
        "INSERT INTO watcher_spend (watcher, day, tokens) VALUES (?, ?, ?) ON CONFLICT(watcher, day) DO UPDATE SET tokens = tokens + excluded.tokens",
    )
    .bind(watcher)
    .bind(today())
    .bind(tokens)
    .execute(&st.store.pool)
    .await;
}

// --- State and sessions ------------------------------------------------------

#[derive(Clone, Debug)]
struct State {
    key: String,
    realm: String,
    session_id: String,
    position: i64,
    memory: String,
    turn: i64,
    ended: bool,
}

async fn load(st: &HeadState, watcher: &str, key: &str) -> anyhow::Result<Option<State>> {
    let r = sqlx::query("SELECT * FROM watcher_state WHERE watcher = ? AND key = ?")
        .bind(watcher)
        .bind(key)
        .fetch_optional(&st.store.pool)
        .await?;
    Ok(r.map(|r| State {
        key: r.get("key"),
        realm: r.get("realm"),
        session_id: r.get("session_id"),
        position: r.get("position"),
        memory: r.get("memory"),
        turn: r.get("turn"),
        ended: r.get::<Option<i64>, _>("ended_at").is_some(),
    }))
}

/// Whether `id` is a watcher's session.
pub fn owns(st: &HeadState, id: &str) -> bool {
    st.watchers.sessions.lock().unwrap().contains(id)
}

pub fn is_watcher(row: &SessionRow) -> bool {
    row.all_labels().iter().any(|l| l == LABEL)
}

/// The connection and model a watcher uses: a routing rule for
/// `task:watch:<name>`, else the watched session's (if the head can ask
/// it), else the assistant's.
async fn model_for(st: &HeadState, d: &WatcherDef, realm: &str, watched: Option<&SessionRow>) -> anyhow::Result<(String, String)> {
    let mut fallback = None;
    if let Some(w) = watched
        && crate::pools::cannot_answer(st, &w.connection_id).await.is_none()
        && !w.connection_id.is_empty()
    {
        fallback = Some((w.connection_id.clone(), w.model.clone()));
    }
    if fallback.is_none() {
        fallback = crate::assistant::model(st, realm).await.ok().filter(|(c, _)| !c.is_empty());
    }
    // Else what a new session would get (your latest session's, else the
    // first connection's).
    if fallback.is_none() {
        fallback = crate::sessions::route_for(st, "", "", &[format!("task:watch:{}", d.name)]).await.ok().filter(|(c, _)| !c.is_empty());
    }
    crate::work::model_for_route(st, realm, &format!("task:watch:{}", d.name), fallback)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no model to ask: add a routing rule for task:watch:{} (Settings → Model routing)", d.name))
}

/// The watcher's state for `key`, making its session the first time.
async fn state_for(st: &HeadState, d: &WatcherDef, key: &str, realm: &str, watched: Option<&SessionRow>) -> anyhow::Result<State> {
    if let Some(s) = load(st, &d.name, key).await? {
        return Ok(s);
    }
    let head_id = st.assistant.head_id().ok_or_else(|| anyhow::anyhow!("the head is starting"))?;
    let (connection_id, model) = model_for(st, d, realm, watched).await.unwrap_or_default();
    let now = now_ms();
    let title = match watched {
        Some(w) => format!("{} · {}", if d.title.is_empty() { &d.name } else { &d.title }, w.title),
        None => {
            if d.title.is_empty() {
                d.name.clone()
            } else {
                d.title.clone()
            }
        }
    };
    let labels = vec![LABEL.to_owned(), format!("{LABEL}:{}", d.name)];
    let row = SessionRow {
        id: uuid::Uuid::now_v7().to_string(),
        worker_id: head_id.to_string(),
        title: title.clone(),
        cwd: String::new(),
        connection_id: connection_id.clone(),
        model: model.clone(),
        status: "starting".into(),
        error: None,
        effort: None,
        // Shown off the watched session, like a side chat.
        parent_id: watched.map(|w| w.id.clone()),
        parent_call_id: None,
        fork_seq: watched.map(|_| 0),
        created_at: now,
        updated_at: now,
        last_seq: 0,
        archived: 0,
        labels: serde_json::to_string(&labels)?,
        worker_name: "head".into(),
        repo: String::new(),
        folder: String::new(),
        home: String::new(),
        ssh_target: String::new(),
        awaiting: 0,
        subagent_model: String::new(),
        subagent_connection_id: String::new(),
        subagent_effort: String::new(),
        tools: "[]".into(),
        realm: realm.to_owned(),
        agent: String::new(),
        end_reason: String::new(),
        root_id: None,
        observed: false,
        instance: String::new(),
        colour: String::new(),
        remote_template: String::new(),
    };
    st.store.create_session(&row, &[], false).await?;
    st.watchers.sessions.lock().unwrap().insert(row.id.clone());
    let started = c6_proto::SessionStarted {
        model: Some(c6_proto::ModelSpec { connection_id, model, ..Default::default() }),
        title,
        system: format!("Watcher {} (docs/watchers.md)", d.name),
        parent_session_id: watched.map(|w| w.id.clone()).unwrap_or_default(),
        ..Default::default()
    };
    append(st, &row.id, vec![Kind::Started(started)]).await?;
    // A per-session watcher starts reading where the session is now, not
    // from its beginning: what happened before it came isn't its news.
    let position = match watched {
        Some(w) if d.scope == "session" => (w.last_seq - WINDOW / 4).max(0),
        _ => 0,
    };
    sqlx::query("INSERT OR IGNORE INTO watcher_state (watcher, key, realm, session_id, position, last_wake_at) VALUES (?, ?, ?, ?, ?, 0)")
        .bind(&d.name)
        .bind(key)
        .bind(realm)
        .bind(&row.id)
        .bind(position)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    load(st, &d.name, key).await?.ok_or_else(|| anyhow::anyhow!("the watcher's state is gone"))
}

/// Adds events to a watcher's session.
async fn append(st: &HeadState, session_id: &str, kinds: Vec<Kind>) -> anyhow::Result<u64> {
    let head = st.assistant.head_id().ok_or_else(|| anyhow::anyhow!("the head is starting"))?;
    let row = st.store.session(session_id).await?.ok_or_else(|| anyhow::anyhow!("the watcher's session is gone"))?;
    let mut seq = row.last_seq as u64;
    let events = kinds
        .into_iter()
        .map(|k| {
            seq += 1;
            SessionEvent { seq, ts_ms: now_ms(), kind: Some(k) }
        })
        .collect();
    crate::sessions::ingest(st, head, SessionEventBatch { session_id: session_id.to_owned(), events }).await?;
    Ok(seq)
}

// --- What wakes a watcher ----------------------------------------------------

/// Words that, at the start of your message, usually redirect the agent.
fn corrects(text: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)^\s*(no\b|nope|don'?t|do not|stop|wrong|not that|that's not|thats not|instead|actually|undo|revert|why did you|you (should|shouldn'?t|forgot|missed))|\b(instead of|i said|i asked|not what i|make sure|always|never)\b",
        )
        .unwrap()
    });
    RE.is_match(text)
}

const EDIT_TOOLS: [&str; 10] =
    ["edit", "write", "multiedit", "multi_edit", "apply_patch", "notebookedit", "str_replace", "create_file", "replace", "patch"];

fn tool_uses(e: &SessionEvent) -> Vec<&c6_proto::ToolUse> {
    match &e.kind {
        Some(Kind::Assistant(a)) => a
            .blocks
            .iter()
            .filter_map(|b| match &b.block {
                Some(content_block::Block::ToolUse(t)) => Some(t),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a shell tool's output ends with a non-zero exit code.
fn failed_exit(output: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\[exit code [1-9][0-9]*\]\s*$").unwrap());
    RE.is_match(output)
}

/// What in these events (since a watcher's position) may wake it.
fn reasons(events: &[SessionEvent]) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut add = |r: &'static str| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    let mut calls: HashMap<String, (String, String)> = HashMap::new();
    let mut same: HashMap<String, u32> = HashMap::new();
    let mut errors_by_tool: HashMap<String, u32> = HashMap::new();
    let mut last_results: Vec<bool> = Vec::new();
    for e in events {
        for t in tool_uses(e) {
            calls.insert(t.id.clone(), (t.name.clone(), t.input_json.clone()));
            let n = same.entry(format!("{}|{}", t.name, t.input_json)).or_default();
            *n += 1;
            if *n >= 3 {
                add("loop");
            }
        }
        match &e.kind {
            Some(Kind::TurnEnded(t)) => {
                add("turn_end");
                if t.reason == TurnEndReason::Interrupted as i32 {
                    add("correction");
                }
            }
            Some(Kind::User(u)) if u.from_session_id.is_empty() && u.delegated_by.is_empty() && corrects(&u.text) => add("correction"),
            Some(Kind::Rewound(_)) => add("correction"),
            Some(Kind::Approval(a)) if a.state == "denied" => add("correction"),
            Some(Kind::ToolResult(r)) => {
                // A shell command that exits non-zero failed too, though
                // the tool call didn't.
                let failed = r.is_error || failed_exit(&r.output);
                last_results.push(failed);
                if failed {
                    *errors_by_tool.entry(r.tool.clone()).or_default() += 1;
                    continue;
                }
                let (name, input) = calls.get(&r.call_id).cloned().unwrap_or_else(|| (r.tool.clone(), String::new()));
                let lower = name.to_lowercase();
                if input.contains("git commit") || input.contains("\\\"commit\\\"") {
                    add("commit");
                }
                if EDIT_TOOLS.iter().any(|t| lower == *t || lower.ends_with(&format!("__{t}"))) {
                    add("edits");
                }
            }
            _ => {}
        }
    }
    let tail_errors = last_results.iter().rev().take_while(|e| **e).count();
    if tail_errors >= 3 || errors_by_tool.values().any(|n| *n >= 3) {
        add("tool_failures");
    }
    out
}

/// The events since a watcher's position, as the model reads them.
fn digest(events: &[SessionEvent]) -> String {
    let clip = |s: &str, n: usize| -> String {
        let s = s.trim();
        if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_owned() }
    };
    let mut lines: Vec<String> = Vec::new();
    for e in events {
        match &e.kind {
            Some(Kind::User(u)) if !u.from_session_id.is_empty() => {
                lines.push(format!("[{}] message from \"{}\": {}", e.seq, u.from_title, clip(&u.text, 600)))
            }
            Some(Kind::User(u)) => lines.push(format!("[{}] user: {}", e.seq, clip(&u.text, 800))),
            Some(Kind::Assistant(a)) => {
                for b in &a.blocks {
                    match &b.block {
                        Some(content_block::Block::Text(t)) if !t.trim().is_empty() => {
                            lines.push(format!("[{}] agent: {}", e.seq, clip(t, 600)))
                        }
                        Some(content_block::Block::ToolUse(t)) => {
                            lines.push(format!("[{}] agent calls {} {}", e.seq, t.name, clip(&t.input_json, 400)))
                        }
                        _ => {}
                    }
                }
            }
            Some(Kind::ToolResult(r)) if r.is_error || failed_exit(&r.output) => {
                lines.push(format!("[{}]   ✗ {} failed: {}", e.seq, r.tool, clip(&r.output, 400)))
            }
            Some(Kind::ToolResult(r)) => lines.push(format!("[{}]   ✓ {}: {}", e.seq, r.tool, clip(&r.output, 160))),
            Some(Kind::Approval(a)) if a.state != "requested" => lines.push(format!("[{}] approval of {}: {}", e.seq, a.tool, a.state)),
            Some(Kind::Rewound(_)) => lines.push(format!("[{}] the user rewound the conversation", e.seq)),
            Some(Kind::TurnEnded(t)) if t.reason == TurnEndReason::Interrupted as i32 => {
                lines.push(format!("[{}] the user interrupted the turn", e.seq))
            }
            Some(Kind::TurnEnded(t)) if t.reason == TurnEndReason::Error as i32 => {
                lines.push(format!("[{}] the turn ended with an error", e.seq))
            }
            Some(Kind::TurnEnded(_)) => lines.push(format!("[{}] (turn ended)", e.seq)),
            Some(Kind::Error(x)) => lines.push(format!("[{}] error: {}", e.seq, clip(&x.message, 400))),
            _ => {}
        }
    }
    let mut out = String::new();
    for l in lines.iter().rev() {
        if out.len() + l.len() > DIGEST_CHARS {
            out.insert_str(0, "(earlier events left out)\n");
            break;
        }
        out.insert_str(0, &format!("{l}\n"));
    }
    out
}

/// After new events in a session: wake the watchers that follow it, when
/// something they wake on may have happened.
pub async fn on_events(st: &HeadState, session_id: &str, events: &[SessionEvent]) {
    // carbon6's own tools failing, for the improvement watcher.
    for e in events {
        if let Some(Kind::ToolResult(r)) = &e.kind
            && r.is_error
            && carbon6_tool(&r.tool)
        {
            signal(st, "tool", &format!("{}: {}", r.tool, r.output.lines().next().unwrap_or(""))).await;
        }
    }
    let boundary = events
        .iter()
        .any(|e| matches!(e.kind, Some(Kind::TurnEnded(_) | Kind::User(_) | Kind::ToolResult(_) | Kind::Rewound(_) | Kind::Approval(_))));
    if !boundary || owns(st, session_id) {
        return;
    }
    let Ok(Some(row)) = st.store.session(session_id).await else { return };
    // Its own sessions, and subagents (their parent's watchers follow the
    // parent).
    if is_watcher(&row) || row.parent_call_id.is_some() || row.archived != 0 {
        return;
    }
    let labels = row.all_labels();
    for d in defs(st).await {
        if d.enabled && d.scope == "session" && api::labels::applies(&d.applies_to, &labels) && !d.wakes.is_empty() {
            poke(st, &d.name, &row.id, Vec::new(), None);
        }
    }
}

/// A session that ended (archived): its watchers take a last turn if they
/// wake on that, and end with it.
pub async fn session_ended(st: &HeadState, session_id: &str) {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT watcher, session_id FROM watcher_state WHERE key = ? AND ended_at IS NULL")
        .bind(session_id)
        .fetch_all(&st.store.pool)
        .await
        .unwrap_or_default();
    for (watcher, own) in rows {
        // A last turn for those that wake on it; the rest just end.
        match def(st, &watcher).await {
            Some(d) if d.enabled && d.wakes.iter().any(|w| w == "session_end") => {
                poke(st, &watcher, session_id, vec!["session_end".into()], None)
            }
            _ => end(st, &watcher, session_id, &own).await,
        }
    }
    // A watcher session you archived: it stops following.
    if owns(st, session_id) {
        let _ = sqlx::query("UPDATE watcher_state SET ended_at = ? WHERE session_id = ? AND ended_at IS NULL")
            .bind(now_ms())
            .bind(session_id)
            .execute(&st.store.pool)
            .await;
    }
}

/// Asks for a wake of `watcher` on `key` (a session, or `*`): one at a
/// time per watcher and key; what comes in while one runs is looked at
/// after it.
fn poke(st: &HeadState, watcher: &str, key: &str, forced: Vec<String>, message: Option<(String, String)>) {
    let Some(arc) = st.arc() else { return };
    let id = format!("{watcher}\u{1}{key}");
    {
        let mut running = st.watchers.running.lock().unwrap();
        if let Some(queued) = running.get_mut(&id) {
            queued.extend(forced);
            queued.push(String::new());
            if let Some((cmd, text)) = message {
                queued.push(format!("\u{2}{cmd}\u{3}{text}"));
            }
            return;
        }
        running.insert(id.clone(), Vec::new());
    }
    st.watchers.busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (watcher, key) = (watcher.to_owned(), key.to_owned());
    tokio::spawn(async move {
        let (mut forced, mut message) = (forced, message);
        loop {
            if let Err(e) = wake(&arc, &watcher, &key, &forced, message.as_ref()).await {
                warn!(watcher = %watcher, key = %key, "watcher: {e:#}");
            }
            let mut running = arc.watchers.running.lock().unwrap();
            let queued = running.get_mut(&id).map(std::mem::take).unwrap_or_default();
            if queued.is_empty() {
                running.remove(&id);
                break;
            }
            message = queued
                .iter()
                .rev()
                .find_map(|q| q.strip_prefix('\u{2}').and_then(|r| r.split_once('\u{3}')).map(|(c, t)| (c.to_owned(), t.to_owned())));
            forced = queued.into_iter().filter(|q| !q.is_empty() && !q.starts_with('\u{2}')).collect();
        }
        arc.watchers.busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    });
}

async fn skip(st: &HeadState, watcher: &str, key: &str) {
    let _ = sqlx::query("UPDATE watcher_state SET skips = skips + 1 WHERE watcher = ? AND key = ?")
        .bind(watcher)
        .bind(key)
        .execute(&st.store.pool)
        .await;
}

/// One wake: read what's new; if it holds something this watcher wakes on
/// (or it was asked to, or you wrote to it), one turn of its session.
async fn wake(st: &HeadState, name: &str, key: &str, forced: &[String], message: Option<&(String, String)>) -> anyhow::Result<()> {
    let Some(d) = def(st, name).await else { return Ok(()) };
    if !d.enabled && message.is_none() && forced.is_empty() {
        return Ok(());
    }
    let watched = if key == "*" { None } else { st.store.session(key).await? };
    if key != "*" && watched.is_none() {
        return Ok(());
    }
    let realm = match &watched {
        Some(w) => w.realm.clone(),
        None => crate::realms::ROOT.to_owned(),
    };
    // A watcher that hasn't anything to say keeps no session for it.
    let existing = load(st, name, key).await?;
    if existing.as_ref().is_some_and(|s| s.ended) && message.is_none() {
        return Ok(());
    }
    let mut events = Vec::new();
    let mut matched: Vec<String> = forced.iter().filter(|f| !f.is_empty()).cloned().collect();
    let position =
        existing.as_ref().map(|s| s.position).unwrap_or_else(|| watched.as_ref().map_or(0, |w| (w.last_seq - WINDOW / 4).max(0)));
    if let Some(w) = &watched {
        let from = position.max(w.last_seq - WINDOW);
        events = st.store.events_after(&w.id, from.max(0) as u64, WINDOW).await?;
        for r in reasons(&events) {
            if d.wakes.iter().any(|x| x == r) && !matched.iter().any(|m| m == r) {
                matched.push(r.to_owned());
            }
        }
    }
    if matched.is_empty() && message.is_none() {
        if existing.is_some() {
            skip(st, name, key).await;
        }
        return Ok(());
    }
    if d.daily_tokens > 0 && spent(st, name).await >= d.daily_tokens as i64 && message.is_none() {
        let s = state_for(st, &d, key, &realm, watched.as_ref()).await?;
        skip(st, name, &s.key).await;
        info!(watcher = %name, "over its daily budget: skipped");
        return Ok(());
    }
    // What the agenda reads.
    let mut values: Vec<(&str, String)> = Vec::new();
    let reason = if matched.is_empty() { "you wrote to it".to_owned() } else { matched.join(", ") };
    values.push(("reason", reason.clone()));
    let mut input_shown = String::new();
    match d.name.as_str() {
        "improve" => {
            let groups = signal_groups(st, d.threshold.max(1)).await?;
            if groups.is_empty() && message.is_none() {
                if existing.is_some() {
                    skip(st, name, key).await;
                }
                return Ok(());
            }
            let text = groups.iter().map(|(k, n, sample)| format!("- {n}× {k}\n  e.g. {sample}")).collect::<Vec<_>>().join("\n");
            input_shown = text.clone();
            values.push(("signals", text));
        }
        "retro-weekly" => {
            let text = week_findings(st).await?;
            // A week with nothing found writes nothing (unless you asked).
            if text.is_empty() && message.is_none() && !forced.iter().any(|f| f == "now") {
                return Ok(());
            }
            input_shown = text.clone();
            values.push(("findings", if text.is_empty() { "(nothing this week)".into() } else { text }));
            values.push(("notes", notes_text(st, &realm, None).await));
        }
        _ => {}
    }
    let s = state_for(st, &d, key, &realm, watched.as_ref()).await?;
    let mut diff = String::new();
    if let Some(w) = &watched {
        let dig = digest(&events);
        input_shown = dig.clone();
        values.push(("digest", if dig.is_empty() { "(nothing new)".into() } else { dig }));
        values.push(("title", w.title.clone()));
        values.push((
            "labels",
            w.user_labels().into_iter().chain(w.all_labels()).collect::<HashSet<_>>().into_iter().collect::<Vec<_>>().join(", "),
        ));
        values.push(("goal", goal(st, &w.id).await));
        values.push(("notes", notes_text(st, &w.realm, Some(&w.all_labels())).await));
        let standards = crate::memory::enforced_by(st, &w.realm, &d.name, &w.all_labels()).await?;
        values.push((
            "standards",
            if standards.is_empty() {
                "(none)".into()
            } else {
                standards.iter().map(|m| format!("- {}", m.text)).collect::<Vec<_>>().join("\n")
            },
        ));
        if d.outputs.iter().any(|o| o == "ledger") {
            values.push(("ledger", ledger_text(st, w).await));
        }
        if matched.iter().any(|m| m == "commit" || m == "edits") {
            diff = changes(st, w).await;
            values.push(("diff", if diff.is_empty() { "(none, or not readable)".into() } else { diff.clone() }));
        }
    }
    values.push(("memory", if s.memory.is_empty() { "(none yet)".into() } else { s.memory.clone() }));
    let agenda = crate::prompts::get(st, &d.agenda, &realm).await;
    let pairs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut prompt = api::prompts::fill(&agenda, &pairs);
    if let Some((_, text)) = message {
        prompt.push_str(&format!("\n\nThe user wrote to you: {text}"));
    }
    prompt.push_str("\n\n");
    prompt.push_str(&crate::prompts::get(st, "watch.answer", &realm).await);

    // The turn, in its session.
    let turn = (s.turn + 1) as u64;
    let shown_input = match message {
        Some((_, text)) => text.clone(),
        None => {
            let mut t = format!("Woke on: {reason}");
            if !input_shown.is_empty() {
                t.push_str(&format!("\n\n{}", clip_end(&input_shown, 6000)));
            }
            if !diff.is_empty() {
                t.push_str(&format!("\n\nChanges:\n{}", clip_end(&diff, 3000)));
            }
            t
        }
    };
    let command_id = message.map(|(c, _)| c.clone()).unwrap_or_default();
    append(
        st,
        &s.session_id,
        vec![
            Kind::TurnStarted(c6_proto::TurnStarted { turn, ..Default::default() }),
            Kind::User(c6_proto::UserMessage { command_id, text: shown_input, ..Default::default() }),
        ],
    )
    .await?;
    let (connection, model) = match model_for(st, &d, &realm, watched.as_ref()).await {
        Ok(m) => m,
        Err(e) => {
            end_turn(st, &s, turn, Some(format!("{e:#}"))).await?;
            return Err(e);
        }
    };
    let answer = ask(st, &realm, &d.name, &connection, &model, prompt).await;
    let (v, usage) = match answer {
        Ok(x) => x,
        Err(e) => {
            end_turn(st, &s, turn, Some(format!("{e:#}"))).await?;
            return Err(e);
        }
    };
    let tokens = (usage.input_tokens + usage.output_tokens) as i64;
    spend(st, &d.name, tokens).await;
    let findings: Vec<Value> = v["findings"].as_array().cloned().unwrap_or_default();
    let reply = v["reply"].as_str().unwrap_or("").trim().to_owned();
    let mut said = String::new();
    if !reply.is_empty() {
        said.push_str(&reply);
        said.push_str("\n\n");
    }
    if findings.is_empty() {
        said.push_str("Nothing to report.");
    } else {
        for f in &findings {
            said.push_str(&format!(
                "- **{}** ({}): {}\n",
                f["kind"].as_str().unwrap_or("finding"),
                f["severity"].as_str().unwrap_or("info"),
                f["text"].as_str().or(f["note"].as_str()).unwrap_or("")
            ));
        }
    }
    let seq = append(
        st,
        &s.session_id,
        vec![
            Kind::Assistant(c6_proto::AssistantMessage {
                turn,
                blocks: vec![c6_proto::ContentBlock { block: Some(content_block::Block::Text(said)) }],
                stop_reason: "end_turn".into(),
            }),
            Kind::Usage(c6_proto::Usage {
                model: model.clone(),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                context_window: 0,
            }),
        ],
    )
    .await?;
    let mut notices = Vec::new();
    for f in &findings {
        match act(st, &d, &s, watched.as_ref(), seq as i64, f).await {
            Ok(Some(n)) => notices.push(n),
            Ok(None) => {}
            Err(e) => notices.push(format!("couldn't act on a finding: {e:#}")),
        }
    }
    let mut kinds: Vec<Kind> = notices
        .into_iter()
        .map(|n| Kind::Notice(c6_proto::Notice { title: d.title.clone(), message: n, priority: "low".into(), quiet: true }))
        .collect();
    kinds.push(Kind::TurnEnded(c6_proto::TurnEnded { turn, reason: TurnEndReason::Completed as i32 }));
    append(st, &s.session_id, kinds).await?;
    let memory: String = v["memory"].as_str().map(|m| m.trim().chars().take(MEMORY_CHARS).collect()).unwrap_or(s.memory.clone());
    let new_position = watched.as_ref().map_or(s.position, |w| events.last().map_or(w.last_seq, |e| e.seq as i64));
    sqlx::query(
        "UPDATE watcher_state SET position = ?, memory = ?, wakes = wakes + 1, turn = ?, last_wake_at = ? WHERE watcher = ? AND key = ?",
    )
    .bind(new_position)
    .bind(&memory)
    .bind(turn as i64)
    .bind(now_ms())
    .bind(name)
    .bind(key)
    .execute(&st.store.pool)
    .await?;
    if matched.iter().any(|m| m == "session_end") {
        end(st, name, key, &s.session_id).await;
    }
    if d.name == "improve" {
        sqlx::query("UPDATE watch_signals SET reported = count WHERE count >= ?")
            .bind(d.threshold.max(1) as i64)
            .execute(&st.store.pool)
            .await?;
    }
    Ok(())
}

fn clip_end(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut cut = s.len() - n;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &s[cut..])
}

async fn end_turn(st: &HeadState, s: &State, turn: u64, error: Option<String>) -> anyhow::Result<()> {
    let mut kinds = Vec::new();
    if let Some(message) = error {
        kinds.push(Kind::Error(c6_proto::ErrorEvent { message }));
    }
    kinds.push(Kind::TurnEnded(c6_proto::TurnEnded { turn, reason: TurnEndReason::Error as i32 }));
    append(st, &s.session_id, kinds).await?;
    sqlx::query("UPDATE watcher_state SET turn = ? WHERE session_id = ?")
        .bind(turn as i64)
        .bind(&s.session_id)
        .execute(&st.store.pool)
        .await?;
    Ok(())
}

/// A per-session watcher ends with its session: archived with it.
async fn end(st: &HeadState, name: &str, key: &str, session_id: &str) {
    let _ = sqlx::query("UPDATE watcher_state SET ended_at = ? WHERE watcher = ? AND key = ?")
        .bind(now_ms())
        .bind(name)
        .bind(key)
        .execute(&st.store.pool)
        .await;
    let _ = sqlx::query("UPDATE sessions SET archived = 1 WHERE id = ?").bind(session_id).execute(&st.store.pool).await;
    st.registry.bump();
}

/// The work a session serves, as its goal.
async fn goal(st: &HeadState, session_id: &str) -> String {
    let ids = crate::work::holding(st, &format!("session:{session_id}")).await.unwrap_or_default();
    let mut out = Vec::new();
    for id in ids {
        if let Some((title, done_when)) = crate::work::title_and_goal(st, &id).await {
            out.push(if done_when.is_empty() { title } else { format!("{title} (done when: {done_when})") });
        }
    }
    if out.is_empty() { "(not said)".into() } else { out.join("; ") }
}

/// The memory notes sessions here get, with who wrote each (for lessons not
/// to repeat them, and the weekly roll-up to find stale ones).
async fn notes_text(st: &HeadState, realm: &str, labels: Option<&[String]>) -> String {
    let rows = sqlx::query(
        "SELECT id, fields, created_by FROM records WHERE collection = 'memory' AND realm = ? ORDER BY updated_at DESC LIMIT 200",
    )
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for r in rows {
        let f: Value = serde_json::from_str(&r.get::<String, _>("fields")).unwrap_or(Value::Null);
        let scope = f["scope"].as_str().unwrap_or("");
        if let Some(l) = labels
            && !scope.is_empty()
            && !api::labels::label_matches(scope, l)
        {
            continue;
        }
        out.push(format!(
            "- [{}] ({}; by {}) {}",
            r.get::<String, _>("id"),
            if scope.is_empty() { "every session" } else { scope },
            r.get::<String, _>("created_by"),
            f["text"].as_str().unwrap_or("")
        ));
    }
    if out.is_empty() { "(none)".into() } else { out.join("\n") }
}

/// A session's uncommitted changes, from its worker (empty when it can't
/// be read).
async fn changes(st: &HeadState, w: &SessionRow) -> String {
    let Ok(worker) = w.worker_id.parse::<c6_core::WorkerId>() else { return String::new() };
    if w.cwd.is_empty() || st.registry.get(worker).is_none() {
        return String::new();
    }
    match st.registry.call(worker, "git.diff", json!({"path": w.cwd}), std::time::Duration::from_secs(20)).await {
        Ok(v) => v["diff"].as_str().map(|d| clip_end(d, DIFF_CHARS)).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

// --- Acting on findings ------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn record(
    st: &HeadState,
    d: &WatcherDef,
    s: &State,
    watched: Option<&SessionRow>,
    seq: i64,
    f: &Value,
    kind: &str,
    done: &str,
    reference: &str,
) -> anyhow::Result<i64> {
    let text = f["text"].as_str().or(f["note"].as_str()).unwrap_or("").trim().chars().take(2000).collect::<String>();
    let severity = f["severity"].as_str().filter(|x| api::watchers::SEVERITIES.contains(x)).unwrap_or("info");
    let id = sqlx::query(
        "INSERT INTO watcher_findings (watcher, realm, watched, session_id, seq, severity, kind, text, done, ref, at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&d.name)
    .bind(&s.realm)
    .bind(watched.map(|w| w.id.as_str()).unwrap_or(""))
    .bind(&s.session_id)
    .bind(seq)
    .bind(severity)
    .bind(kind)
    .bind(&text)
    .bind(done)
    .bind(reference)
    .bind(now_ms())
    .execute(&st.store.pool)
    .await?
    .last_insert_rowid();
    st.registry.bump();
    Ok(id)
}

async fn set_done(st: &HeadState, id: i64, done: &str, reference: &str) {
    let _ = sqlx::query("UPDATE watcher_findings SET done = ?, ref = ? WHERE id = ?")
        .bind(done)
        .bind(reference)
        .bind(id)
        .execute(&st.store.pool)
        .await;
}

/// Notes this watcher sent this session in the last hour.
async fn sent_lately(st: &HeadState, watcher: &str, watched: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM watcher_findings WHERE watcher = ? AND watched = ? AND done = 'sent' AND at > ?")
        .bind(watcher)
        .bind(watched)
        .bind(now_ms() - 3_600_000)
        .fetch_one(&st.store.pool)
        .await
        .unwrap_or(0)
}

/// Does what a finding asks, as far as the watcher may. What happened, for
/// its transcript.
async fn act(
    st: &HeadState,
    d: &WatcherDef,
    s: &State,
    watched: Option<&SessionRow>,
    seq: i64,
    f: &Value,
) -> anyhow::Result<Option<String>> {
    let kind = f["kind"].as_str().unwrap_or("note").to_owned();
    let may = |o: &str| d.outputs.iter().any(|x| x == o);
    let severity = f["severity"].as_str().unwrap_or("info");
    let text = f["text"].as_str().unwrap_or("").trim().to_owned();
    match kind.as_str() {
        "note" | "issue" => {
            let note = f["note"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or(&text).trim().to_owned();
            let Some(w) = watched else {
                record(st, d, s, None, seq, f, &kind, "noted", "").await?;
                return Ok(None);
            };
            let id = record(st, d, s, Some(w), seq, f, &kind, "marked", "").await?;
            let wants = may("note") && !d.to_you && severity_rank(severity) >= severity_rank(&d.note_severity);
            if !wants {
                return Ok(Some(if may("mark") { format!("Marked the session: {note}") } else { format!("Noted: {note}") }));
            }
            if sent_lately(st, &d.name, &w.id).await >= d.notes_per_hour as i64 {
                set_done(st, id, "over limit", "").await;
                return Ok(Some(format!("Not sent (already {} notes this hour); marked: {note}", d.notes_per_hour)));
            }
            let from_title = format!("{} (watcher)", if d.title.is_empty() { &d.name } else { &d.title });
            match crate::sessions::message_from_watcher(st, &w.id, &note, &s.session_id, &from_title).await {
                Ok(_) => {
                    set_done(st, id, "sent", &format!("session:{}", w.id)).await;
                    Ok(Some(format!("Told the agent: {note}")))
                }
                Err(e) => Ok(Some(format!("Couldn't tell the agent ({e:#}); marked: {note}"))),
            }
        }
        "lesson" if may("lesson") => {
            let note = f["note"].as_str().unwrap_or("").trim().to_owned();
            if note.is_empty() {
                return Ok(None);
            }
            let scope = f["scope"].as_str().unwrap_or("").trim().to_owned();
            // Scoped to one repo or one worker: applied, and in the weekly
            // roll-up to undo; wider ones ask you.
            let narrow = (scope.starts_with("repo:") || scope.starts_with("worker:")) && !scope.contains(char::is_whitespace);
            let f2 = json!({"severity": severity, "text": format!("{note} ({})", if scope.is_empty() { "every session".into() } else { scope.clone() })});
            if narrow {
                let m = crate::memory::put_as(
                    st,
                    api::Memory { scope: scope.clone(), text: note.clone(), ..Default::default() },
                    &s.realm,
                    &format!("watcher:{}", d.name),
                )
                .await?;
                record(st, d, s, watched, seq, &f2, "lesson", "applied", &format!("memory:{}", m.id)).await?;
                return Ok(Some(format!("Remembered for {scope}: {note}")));
            }
            let id = record(st, d, s, watched, seq, &f2, "lesson", "asked", "").await?;
            let dec = ask_you(
                st,
                s,
                id,
                format!("Remember this for {}?", if scope.is_empty() { "every session" } else { &scope }),
                format!("{}\n\n> {note}\n\nA lesson from {}. Allow adds it as a memory note sessions with that label get.", text, d.title),
                vec![("Allow", "Adds the memory note."), ("Not this", "Nothing changes.")],
                json!({"what": "lesson", "scope": scope, "note": note}),
            )
            .await?;
            set_done(st, id, "asked", &format!("decision:{dec}")).await;
            Ok(Some(format!("Asked you about a memory note (decision {dec}): {note}")))
        }
        "directive" if may("directive") => {
            let directive = f["directive"].as_str().unwrap_or(&text).trim().to_owned();
            let scope = f["scope"].as_str().unwrap_or("").trim().to_owned();
            let id = record(st, d, s, watched, seq, f, "directive", "asked", "").await?;
            let dec = ask_you(
                st,
                s,
                id,
                format!("How far should this go: \"{}\"?", directive.chars().take(120).collect::<String>()),
                format!(
                    "You asked for this in one place. It may hold more widely (docs/watchers.md §7).\n\n> {directive}\n\n\
                     - **Here only:** nothing more.\n- **Audit now:** a task card for a session that checks {} for it.\n\
                     - **From now on:** a standard for {}: a memory note the review watcher checks every change against{}.",
                    if scope.is_empty() { "the codebase".into() } else { scope.clone() },
                    if scope.is_empty() { "every session".into() } else { scope.clone() },
                    if f["checkable"].as_bool() == Some(true) {
                        ", and a task card to add a mechanical check (a lint rule, test or CI step)"
                    } else {
                        ""
                    }
                ),
                vec![
                    ("Here only", "Nothing more."),
                    ("Audit now", "A task card to check for it."),
                    ("From now on", "A standard the review watcher enforces."),
                ],
                json!({"what": "directive", "directive": directive, "scope": scope, "checkable": f["checkable"].as_bool().unwrap_or(false),
                       "audit": f["audit"].as_str().unwrap_or(""), "watched": watched.map(|w| w.id.clone())}),
            )
            .await?;
            set_done(st, id, "asked", &format!("decision:{dec}")).await;
            Ok(Some(format!("Asked you how far to take a directive (decision {dec})")))
        }
        "card" if may("card") => {
            let (title, prompt) = (f["title"].as_str().unwrap_or("").trim(), f["prompt"].as_str().unwrap_or("").trim());
            if title.is_empty() || prompt.is_empty() {
                return Ok(None);
            }
            let source = match watched {
                Some(w) => w.id.clone(),
                None => card_source(st, &s.realm).await.unwrap_or_else(|| s.session_id.clone()),
            };
            let params = json!({"title": title, "why": f["why"].as_str().unwrap_or(&text), "prompt": prompt});
            let v = crate::decisions::task_card(st, &source, "task_propose", &params).await?;
            record(
                st,
                d,
                s,
                watched,
                seq,
                &json!({"severity": severity, "text": title}),
                "card",
                "carded",
                &format!("decision:{}", v["id"]),
            )
            .await?;
            Ok(Some(format!("Proposed a task card: {title}")))
        }
        "change" if may("change") => {
            let what = f["what"].as_str().unwrap_or("setting").to_owned();
            let change = f["change"].as_str().unwrap_or(&text).trim().to_owned();
            let id =
                record(st, d, s, watched, seq, &json!({"severity": severity, "text": format!("{what}: {change}")}), "change", "asked", "")
                    .await?;
            let retire = (what == "memory").then(|| f["id"].as_str().map(str::to_owned)).flatten();
            let dec = ask_you(
                st,
                s,
                id,
                match &retire {
                    Some(_) => "Retire this memory note?".into(),
                    None => format!("Change a {what}?"),
                },
                format!("{text}\n\n> {change}\n\nProposed by {}.{}", d.title, match &retire {
                    Some(_) => " Allow deletes the note.",
                    None => " Allow records that you agree; make the change in Settings (watchers don't edit prompts, policies or routing rules themselves).",
                }),
                vec![("Allow", "Agree."), ("Not this", "Nothing changes.")],
                json!({"what": "change", "kind": what, "change": change, "retire": retire}),
            )
            .await?;
            set_done(st, id, "asked", &format!("decision:{dec}")).await;
            Ok(Some(format!("Asked you about a change (decision {dec})")))
        }
        "ledger" if may("ledger") => {
            let entry = f["entry"].as_str().unwrap_or(&text).trim().to_owned();
            if entry.is_empty() {
                return Ok(None);
            }
            let Some(w) = watched else { return Ok(None) };
            let doc = ledger_doc(st, w).await?;
            let batch: api::docs::DocBatch = serde_json::from_value(json!({"ops": [{"op": "insert", "markdown": format!("- {entry}")}]}))?;
            crate::docs::apply(st, &w.realm, &doc, &batch, &format!("watcher:{}", d.name), None).await?;
            record(st, d, s, watched, seq, &json!({"severity": "info", "text": entry}), "ledger", "ledger", &format!("doc:{doc}")).await?;
            Ok(Some(format!("Added to the decisions ledger: {entry}")))
        }
        "doc" if may("doc") => {
            let markdown = f["doc"].as_str().unwrap_or("").trim().to_owned();
            if markdown.is_empty() {
                return Ok(None);
            }
            let title = f["title"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} — {}", d.title, chrono::Utc::now().format("%Y-%m-%d")));
            let n = api::docs::DocNew { kind: "doc".into(), title: title.clone(), markdown, ..Default::default() };
            // Root keeps no documents: a watcher over everything writes in home.
            let realm = if s.realm == crate::realms::ROOT { crate::realms::HOME } else { s.realm.as_str() };
            let (doc, _) = crate::docs::create(st, realm, &n, &format!("watcher:{}", d.name)).await?;
            record(st, d, s, watched, seq, &json!({"severity": "info", "text": title}), "doc", "doc", &format!("doc:{}", doc.summary.id))
                .await?;
            Ok(Some(format!("Wrote \"{title}\"")))
        }
        "risky" => {
            record(st, d, s, watched, seq, f, "risky", "asked", "").await?;
            Ok(None)
        }
        _ => {
            record(st, d, s, watched, seq, f, &kind, "noted", "").await?;
            Ok(None)
        }
    }
}

/// The session a card about carbon6 starts from: the newest one in a
/// carbon6 checkout, if any.
async fn card_source(st: &HeadState, realm: &str) -> Option<String> {
    let rows = st.store.sessions(Some(realm), 200).await.ok()?;
    rows.into_iter()
        .find(|r| !is_watcher(r) && r.all_labels().iter().any(|l| l.starts_with("repo:") && l.ends_with("/carbon6")))
        .map(|r| r.id)
}

async fn ask_you(
    st: &HeadState,
    s: &State,
    finding: i64,
    title: String,
    document: String,
    options: Vec<(&str, &str)>,
    context: Value,
) -> anyhow::Result<i64> {
    let mut context = context;
    context["finding"] = json!(finding);
    context["watcher_session"] = json!(s.session_id);
    let d = api::DecisionIn {
        kind: "approve".into(),
        title,
        document,
        context,
        source: format!("session:{}", s.session_id),
        group: "Watchers".into(),
        options: options.into_iter().map(|(l, does)| api::DecisionOption { label: l.into(), does: does.into() }).collect(),
        ..Default::default()
    };
    Ok(crate::decisions::open(st, &d, &s.realm, &format!("watch:{finding}:{}", uuid::Uuid::now_v7())).await?.id)
}

/// Your answer to a watcher's card (`watch:…`).
pub async fn decided(st: &HeadState, d: &api::Decision, option: Option<&str>) -> anyhow::Result<()> {
    let c = &d.context;
    let finding = c["finding"].as_i64().unwrap_or(0);
    let realm = d.realm.clone();
    let mut done = format!("you: {}", option.unwrap_or("dismissed"));
    match (c["what"].as_str(), option) {
        (Some("lesson"), Some("Allow")) => {
            let m = crate::memory::put_as(
                st,
                api::Memory {
                    scope: c["scope"].as_str().unwrap_or("").into(),
                    text: c["note"].as_str().unwrap_or("").into(),
                    ..Default::default()
                },
                &realm,
                "you",
            )
            .await?;
            done = "applied".into();
            set_done(st, finding, &done, &format!("memory:{}", m.id)).await;
            return Ok(());
        }
        (Some("directive"), Some("Audit now")) => {
            let directive = c["directive"].as_str().unwrap_or("");
            let audit = c["audit"].as_str().filter(|a| !a.trim().is_empty()).map(str::to_owned).unwrap_or_else(|| {
                format!("Check the codebase for this quality, consistently, and report or fix what falls short: {directive}")
            });
            if let Some(source) = c["watched"].as_str() {
                crate::decisions::task_card(st, source, "task_propose", &json!({"title": format!("Audit: {}", directive.chars().take(80).collect::<String>()), "why": "You asked for this in one place; it may hold more widely.", "prompt": audit})).await?;
            }
            done = "carded".into();
        }
        (Some("directive"), Some("From now on")) => {
            let directive = c["directive"].as_str().unwrap_or("").to_owned();
            let scope = c["scope"].as_str().unwrap_or("").to_owned();
            let m = crate::memory::put_as(
                st,
                api::Memory { scope: scope.clone(), text: directive.clone(), enforced_by: "review".into(), ..Default::default() },
                &realm,
                "you",
            )
            .await?;
            if c["checkable"].as_bool() == Some(true)
                && let Some(source) = c["watched"].as_str()
            {
                crate::decisions::task_card(
                    st,
                    source,
                    "task_propose",
                    &json!({"title": format!("Add a check for: {}", directive.chars().take(80).collect::<String>()),
                            "why": "A standard you set; a mechanical check is cheaper and surer than review.",
                            "prompt": format!("Add a mechanical check (a lint rule, a test or a CI step) that enforces this standard across the codebase, and fix what it finds: {directive}")}),
                )
                .await?;
            }
            done = "standard".into();
            set_done(st, finding, &done, &format!("memory:{}", m.id)).await;
            return Ok(());
        }
        (Some("change"), Some("Allow")) => {
            if let Some(id) = c["retire"].as_str() {
                let _ = crate::memory::delete(st, id).await;
                done = "retired".into();
            } else {
                done = "agreed".into();
            }
        }
        _ => {}
    }
    set_done(st, finding, &done, "").await;
    Ok(())
}

// --- The decisions ledger ------------------------------------------------------

/// The ledger document for a session's project (else its realm).
async fn ledger_doc(st: &HeadState, w: &SessionRow) -> anyhow::Result<String> {
    let project = project_of(st, &w.id).await;
    let key = format!("ledger:{}", project.clone().unwrap_or_else(|| w.realm.clone()));
    if let Some(id) = sqlx::query_scalar::<_, String>("SELECT memory FROM watcher_state WHERE watcher = 'consistency' AND key = ?")
        .bind(&key)
        .fetch_optional(&st.store.pool)
        .await?
        && crate::docs::title_of(st, &w.realm, &id).await.is_some()
    {
        return Ok(id);
    }
    let title = match &project {
        Some(p) => {
            format!("Decisions ledger · {}", crate::projects::load(st, p).await.ok().flatten().map(|r| r.fields.title).unwrap_or_default())
        }
        None => "Decisions ledger".into(),
    };
    let n = api::docs::DocNew {
        kind: "doc".into(),
        title,
        markdown: "Decisions sessions made (names, APIs, conventions, choices), kept by the consistency watcher (docs/watchers.md §5).\n"
            .into(),
        ..Default::default()
    };
    let (doc, _) = crate::docs::create(st, &w.realm, &n, "watcher:consistency").await?;
    if let Some(p) = &project
        && let Ok(Some(row)) = crate::projects::load(st, p).await
    {
        let _ = crate::projects::add_doc_in(
            st,
            &row,
            &api::projects::ProjectDocIn { doc: doc.summary.id.clone(), role: "notes".into() },
            "watcher:consistency",
        )
        .await;
    }
    sqlx::query("INSERT OR REPLACE INTO watcher_state (watcher, key, realm, session_id, memory) VALUES ('consistency', ?, ?, '', ?)")
        .bind(&key)
        .bind(&w.realm)
        .bind(&doc.summary.id)
        .execute(&st.store.pool)
        .await?;
    Ok(doc.summary.id)
}

async fn project_of(st: &HeadState, session_id: &str) -> Option<String> {
    for w in crate::work::holding(st, &format!("session:{session_id}")).await.unwrap_or_default() {
        if let Some(p) = crate::work::project_of(st, &w).await {
            return Some(p);
        }
    }
    crate::projects::of_session(st, session_id).await
}

async fn ledger_text(st: &HeadState, w: &SessionRow) -> String {
    let Ok(doc) = ledger_doc(st, w).await else { return "(none yet)".into() };
    let q: api::docs::DocRead = serde_json::from_value(json!({"view": "markdown"})).unwrap_or_default();
    match crate::docs::read(st, &w.realm, &doc, &q).await {
        Ok(v) => clip_end(v["markdown"].as_str().unwrap_or(""), 8000),
        Err(_) => "(none yet)".into(),
    }
}

// --- The weekly roll-up ------------------------------------------------------

async fn week_findings(st: &HeadState) -> anyhow::Result<String> {
    let rows = sqlx::query(
        "SELECT watcher, severity, kind, text, done FROM watcher_findings WHERE at > ? AND watcher != 'retro-weekly' ORDER BY at",
    )
    .bind(now_ms() - 7 * 86_400_000)
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            format!(
                "- {} · {} {} ({}): {}",
                r.get::<String, _>("watcher"),
                r.get::<String, _>("kind"),
                r.get::<String, _>("severity"),
                r.get::<String, _>("done"),
                r.get::<String, _>("text")
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

// --- carbon6's own failures --------------------------------------------------

const CARBON6_TOOLS: [&str; 12] = [
    "carbon6",
    "Carbon6",
    "work",
    "Work",
    "doc",
    "app_call",
    "AppCall",
    "template_start",
    "TemplateStart",
    "message_session",
    "MessageSession",
    "Carbon6Api",
];

fn carbon6_tool(name: &str) -> bool {
    name.starts_with("mcp__carbon6__") || CARBON6_TOOLS.contains(&name)
}

/// A failure's key: its source and message with ids, numbers and quoted
/// values taken out, so the same failure counts as one.
fn signal_key(source: &str, message: &str) -> String {
    static IDS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"[0-9a-fA-F]{8}-[0-9a-fA-F-]{27,}|\b[0-9a-f]{12,}\b|"[^"]{0,80}"|\d+"#).unwrap());
    let m: String = message.trim().chars().take(240).collect();
    format!("{source}: {}", IDS.replace_all(&m, "#"))
}

/// One of carbon6's failures: a warning or error in its log, a crash the
/// web UI reported, a call to its tools that failed.
pub async fn signal(st: &HeadState, source: &str, message: &str) {
    if message.trim().is_empty() {
        return;
    }
    let key = signal_key(source, message);
    let now = now_ms();
    let sample: String = message.trim().chars().take(600).collect();
    let _ = sqlx::query(
        "INSERT INTO watch_signals (key, source, sample, count, first_at, last_at) VALUES (?, ?, ?, 1, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET count = count + 1, last_at = excluded.last_at, sample = excluded.sample",
    )
    .bind(&key)
    .bind(source)
    .bind(&sample)
    .bind(now)
    .bind(now)
    .execute(&st.store.pool)
    .await;
}

/// Log lines the head's tracing layer caught, until a head takes them in.
static LOGGED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// A tracing layer that keeps the head's warnings and errors for the
/// improvement watcher (none of it calls a model).
pub struct SignalLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SignalLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let level = *event.metadata().level();
        if level > tracing::Level::WARN || !event.metadata().target().starts_with("c6") {
            return;
        }
        struct Msg(String);
        impl tracing::field::Visit for Msg {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                } else if self.0.len() < 400 {
                    self.0.push_str(&format!(" {}={value:?}", field.name()));
                }
            }
        }
        let mut m = Msg(String::new());
        event.record(&mut m);
        let mut logged = LOGGED.lock().unwrap();
        if logged.len() < 2000 {
            logged.push((event.metadata().target().to_owned(), m.0));
        }
    }
}

async fn take_logged(st: &HeadState) {
    let lines = std::mem::take(&mut *LOGGED.lock().unwrap());
    for (target, message) in lines {
        signal(st, "log", &format!("{target}: {message}")).await;
    }
}

/// Failures that happened at least `threshold` times and grew since they
/// were last given to the watcher: (key, count, a sample).
async fn signal_groups(st: &HeadState, threshold: u32) -> anyhow::Result<Vec<(String, i64, String)>> {
    take_logged(st).await;
    let rows =
        sqlx::query("SELECT key, count, sample FROM watch_signals WHERE count >= ? AND count > reported ORDER BY count DESC LIMIT 30")
            .bind(threshold as i64)
            .fetch_all(&st.store.pool)
            .await?;
    Ok(rows.iter().map(|r| (r.get("key"), r.get("count"), r.get("sample"))).collect())
}

// --- The guard -----------------------------------------------------------------

/// Whether a tool call a guard policy matched is risky (docs/watchers.md
/// §5.6): `Some(why)` if so. Without an enabled guard or a model, nothing
/// is judged risky here: the policy's pattern already matched, and the
/// worker asks you when the head can't answer.
pub async fn guard(st: &HeadState, session: &SessionRow, tool: &str, input: &str) -> anyhow::Result<Option<String>> {
    let Some(d) = def(st, "guard").await.filter(|d| d.enabled) else { return Ok(None) };
    if d.daily_tokens > 0 && spent(st, "guard").await >= d.daily_tokens as i64 {
        return Ok(Some("the guard is over its daily budget, so it asks you".into()));
    }
    let s = state_for(st, &d, "*", crate::realms::ROOT, None).await?;
    let agenda = crate::prompts::get(st, &d.agenda, &session.realm).await;
    let labels = session.all_labels().join(", ");
    let input: String = input.chars().take(4000).collect();
    let mut prompt = api::prompts::fill(
        &agenda,
        &[
            ("title", &session.title),
            ("labels", &labels),
            ("tool", tool),
            ("input", &input),
            ("memory", if s.memory.is_empty() { "(none)" } else { &s.memory }),
        ],
    );
    prompt.push_str("\n\n");
    prompt.push_str(&crate::prompts::get(st, "watch.answer", &session.realm).await);
    let turn = (s.turn + 1) as u64;
    append(
        st,
        &s.session_id,
        vec![
            Kind::TurnStarted(c6_proto::TurnStarted { turn, ..Default::default() }),
            Kind::User(c6_proto::UserMessage {
                text: format!("\"{}\" wants to run {tool}:\n{input}", session.title),
                ..Default::default()
            }),
        ],
    )
    .await?;
    sqlx::query("UPDATE watcher_state SET turn = ?, wakes = wakes + 1, last_wake_at = ? WHERE session_id = ?")
        .bind(turn as i64)
        .bind(now_ms())
        .bind(&s.session_id)
        .execute(&st.store.pool)
        .await?;
    let (connection, model) = model_for(st, &d, &session.realm, Some(session)).await?;
    let (v, usage) = match ask(st, &session.realm, "guard", &connection, &model, prompt).await {
        Ok(x) => x,
        Err(e) => {
            end_turn(st, &s, turn, Some(format!("{e:#}"))).await?;
            return Err(e);
        }
    };
    spend(st, "guard", (usage.input_tokens + usage.output_tokens) as i64).await;
    let risky = v["findings"].as_array().into_iter().flatten().find(|f| f["kind"] == "risky" || f["severity"] != "info").cloned();
    let said = match &risky {
        Some(f) => format!("Risky: {}", f["text"].as_str().unwrap_or("")),
        None => "Not risky: it runs.".into(),
    };
    let seq = append(
        st,
        &s.session_id,
        vec![
            Kind::Assistant(c6_proto::AssistantMessage {
                turn,
                blocks: vec![c6_proto::ContentBlock { block: Some(content_block::Block::Text(said)) }],
                stop_reason: "end_turn".into(),
            }),
            Kind::Usage(c6_proto::Usage {
                model,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                ..Default::default()
            }),
            Kind::TurnEnded(c6_proto::TurnEnded { turn, reason: TurnEndReason::Completed as i32 }),
        ],
    )
    .await?;
    if let Some(f) = &risky {
        record(st, &d, &s, Some(session), seq as i64, f, "risky", "held", "").await?;
    }
    Ok(risky.map(|f| f["text"].as_str().unwrap_or("the guard judged it risky").to_owned()))
}

// --- The model -------------------------------------------------------------------

async fn ask(
    st: &HeadState,
    realm: &str,
    watcher: &str,
    connection: &str,
    model: &str,
    prompt: String,
) -> anyhow::Result<(Value, c6_llm::Usage)> {
    let origin = c6_llm::activity::Origin {
        what: format!("watcher {watcher}"),
        about: watcher.to_owned(),
        connection: connection.to_owned(),
        realm: realm.to_owned(),
        via: String::new(),
    };
    let client = crate::pools::client(st, connection, realm, origin).await?;
    let req = ModelRequest {
        model: model.to_owned(),
        system: String::new(),
        messages: vec![Message { role: Role::User, blocks: vec![Block::Text(prompt)] }],
        tools: Vec::new(),
        // Room for a thinking model's reasoning before its answer.
        max_output_tokens: Some(8192),
        cache_key: None,
        effort: Some("low".into()),
        web_search: false,
        json: true,
    };
    let mut stream = client.stream(&req).await?;
    let mut acc = Accumulator::default();
    while let Some(ev) = stream.next().await {
        acc.push(&ev?);
    }
    let usage = acc.usage;
    let msg = acc.finish(true);
    let text: String = msg.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect();
    let v = json_of(&text).ok_or_else(|| anyhow::anyhow!("{model} gave no JSON: {}", text.chars().take(200).collect::<String>()))?;
    Ok((v, usage))
}

fn json_of(text: &str) -> Option<Value> {
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    (start < end).then(|| serde_json::from_str(&text[start..=end]).ok()).flatten()
}

// --- Commands to a watcher's session -------------------------------------------

/// A command for one of the watchers' sessions (you wrote to it): a turn
/// with your message.
pub fn command(st: &HeadState, cmd: c6_proto::SessionCommand) {
    let Some(arc) = st.arc() else { return };
    tokio::spawn(async move {
        let Some(c6_proto::session_command::Kind::Input(input)) = cmd.kind else { return };
        let row: Option<(String, String)> = sqlx::query_as("SELECT watcher, key FROM watcher_state WHERE session_id = ?")
            .bind(&cmd.session_id)
            .fetch_optional(&arc.store.pool)
            .await
            .ok()
            .flatten();
        let Some((watcher, key)) = row else { return };
        poke(&arc, &watcher, &key, Vec::new(), Some((cmd.command_id.clone(), input.text.clone())));
    });
}

// --- API helpers -----------------------------------------------------------------

/// Wakes a watcher now: for a session (a per-session watcher), or its
/// whole scope. Returns once the wake is done.
pub async fn run_now(st: &HeadState, name: &str, session: Option<&str>) -> anyhow::Result<()> {
    let d = def(st, name).await.ok_or_else(|| not_found("no such watcher"))?;
    let key = match (d.scope.as_str(), session) {
        ("session", Some(s)) => s.to_owned(),
        ("session", None) => return Err(bad_request("this watcher follows sessions: say which")),
        _ => "*".to_owned(),
    };
    // What went wrong is the answer: you asked for this wake.
    wake(st, name, &key, &["now".to_owned()], None).await.map_err(|e| match e.downcast_ref::<crate::UserError>() {
        Some(_) => e,
        None => bad_request(format!("{e:#}")),
    })
}

pub async fn findings(st: &HeadState, q: &api::watchers::FindingsQuery, realm: Option<&str>) -> anyhow::Result<Vec<Finding>> {
    let rows = sqlx::query(
        "SELECT * FROM watcher_findings WHERE (? IS NULL OR watcher = ?) AND (? IS NULL OR watched = ?) AND (? IS NULL OR realm = ?) \
         ORDER BY id DESC LIMIT ?",
    )
    .bind(&q.watcher)
    .bind(&q.watcher)
    .bind(&q.session)
    .bind(&q.session)
    .bind(realm)
    .bind(realm)
    .bind(q.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows.iter().map(finding).collect())
}

fn finding(r: &sqlx::sqlite::SqliteRow) -> Finding {
    Finding {
        id: r.get("id"),
        watcher: r.get("watcher"),
        watched: r.get("watched"),
        session_id: r.get("session_id"),
        seq: r.get("seq"),
        severity: r.get("severity"),
        kind: r.get("kind"),
        text: r.get("text"),
        done: r.get("done"),
        reference: r.get("ref"),
        at: r.get("at"),
    }
}

/// Who watches a session, and what they found.
pub async fn of_session(st: &HeadState, session_id: &str) -> anyhow::Result<SessionWatch> {
    let watchers: Vec<(String, String)> = sqlx::query_as("SELECT watcher, session_id FROM watcher_state WHERE key = ? ORDER BY watcher")
        .bind(session_id)
        .fetch_all(&st.store.pool)
        .await?;
    let rows = sqlx::query("SELECT * FROM watcher_findings WHERE watched = ? ORDER BY id DESC LIMIT 50")
        .bind(session_id)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(SessionWatch { watchers, findings: rows.iter().map(finding).collect() })
}

// --- Start, and the schedule ------------------------------------------------------

/// Takes up where watchers were: knows their sessions again, wakes those
/// whose sessions moved while the head was down, and runs the scheduled
/// ones when due.
pub fn spawn(st: std::sync::Arc<HeadState>) {
    tokio::spawn(async move {
        let rows: Vec<(String, String, String, i64)> =
            sqlx::query_as("SELECT watcher, key, session_id, position FROM watcher_state WHERE ended_at IS NULL AND session_id != ''")
                .fetch_all(&st.store.pool)
                .await
                .unwrap_or_default();
        for (_, _, sid, _) in &rows {
            st.watchers.sessions.lock().unwrap().insert(sid.clone());
        }
        for (watcher, key, _, position) in rows {
            if key == "*" {
                continue;
            }
            if let Ok(Some(w)) = st.store.session(&key).await
                && w.last_seq > position
            {
                poke(&st, &watcher, &key, Vec::new(), None);
            }
        }
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            take_logged(&st).await;
            for d in defs(&st).await {
                if !d.enabled || d.scope != "all" {
                    continue;
                }
                let period = if d.wakes.iter().any(|w| w == "hourly") {
                    3_600_000
                } else if d.wakes.iter().any(|w| w == "daily") {
                    86_400_000
                } else if d.wakes.iter().any(|w| w == "weekly") {
                    7 * 86_400_000
                } else {
                    continue;
                };
                let mut last: i64 = sqlx::query_scalar("SELECT last_wake_at FROM watcher_state WHERE watcher = ? AND key = '*'")
                    .bind(&d.name)
                    .fetch_optional(&st.store.pool)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                // Never woken: a period from when this head first had it
                // on, not at once (kept, so restarts don't put it off).
                if last == 0 {
                    let key = format!("watchers.first_on.{}", d.name);
                    last = match st.store.setting(&key).await.ok().flatten().and_then(|v| v.parse().ok()) {
                        Some(t) => t,
                        None => {
                            let _ = st.store.set_setting(&key, &now_ms().to_string()).await;
                            now_ms()
                        }
                    };
                }
                if now_ms() - last < period {
                    continue;
                }
                let reason = d.wakes.iter().find(|w| matches!(w.as_str(), "hourly" | "daily" | "weekly")).cloned().unwrap_or_default();
                poke(&st, &d.name, "*", vec![reason], None);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(output: &str, is_error: bool) -> SessionEvent {
        SessionEvent {
            seq: 1,
            ts_ms: 0,
            kind: Some(Kind::ToolResult(c6_proto::ToolResult {
                tool: "bash".into(),
                output: output.into(),
                is_error,
                ..Default::default()
            })),
        }
    }

    #[test]
    fn failed_commands_count_as_failures() {
        let three = vec![result("No such file\n[exit code 1]", false); 3];
        assert!(reasons(&three).contains(&"tool_failures"));
        let fine = vec![result("ok\n[exit code 0]", false); 3];
        assert!(!reasons(&fine).contains(&"tool_failures"));
    }

    #[test]
    fn corrections_are_recognized() {
        assert!(corrects("no, use pnpm"));
        assert!(corrects("Don't do that"));
        assert!(corrects("please make sure memory is accounted for"));
        assert!(!corrects("thanks, looks good"));
    }
}
