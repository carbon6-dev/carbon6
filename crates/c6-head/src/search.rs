//! Full-text search over sessions (SQLite FTS5, trigram tokens).
//!
//! Every durable event becomes zero or more rows in the `search` table, in
//! the same transaction that stores the event, so the index never drifts
//! from the log. The index is derived data: when `search.version` differs
//! from [`VERSION`] (first start, or a format change), a background task
//! rebuilds it in bounded, resumable batches.

use std::collections::HashMap;

use c6_api as api;
use c6_proto::{SessionEvent, content_block, session_event};
use prost::Message;
use sqlx::{Row, SqliteConnection};

use crate::bad_request;
use crate::store::Store;

/// Bump when what gets indexed changes; the head then re-indexes.
const VERSION: &str = "2";
const VERSION_KEY: &str = "search.version";
/// Text indexed per entry. Tool output is most of the text in a session.
const MAX_TEXT: usize = 32 * 1024;
/// Candidate matches read per query, before grouping by session.
const CANDIDATES: i64 = 400;
const HITS_PER_SESSION: usize = 3;

/// The searchable entries of an event: (part, kind, text).
fn entries(ev: &SessionEvent) -> Vec<(u32, &'static str, String)> {
    use session_event::Kind;
    let mut out = Vec::new();
    match &ev.kind {
        Some(Kind::User(u)) => {
            // Put names before contents so a large excerpt does not hide
            // the names of other attachments within the entry's size cap.
            let mut text = u.text.clone();
            for a in &u.attachments {
                if text.len() >= MAX_TEXT {
                    break;
                }
                text.push_str("\n\n");
                let end = a.name.floor_char_boundary(MAX_TEXT.saturating_sub(text.len()));
                text.extend(a.name[..end].chars().map(|c| if c.is_control() { ' ' } else { c }));
            }
            for a in &u.attachments {
                if text.len() >= MAX_TEXT {
                    break;
                }
                if let Some(body) = readable_attachment(a) {
                    text.push_str("\n\n");
                    let end = body.floor_char_boundary(MAX_TEXT.saturating_sub(text.len()));
                    text.push_str(&body[..end]);
                }
            }
            // Another session's agent wrote it, not you.
            out.push((0, if u.from_session_id.is_empty() { "user" } else { "session_message" }, text))
        }
        Some(Kind::Assistant(a)) => {
            for (i, b) in a.blocks.iter().enumerate() {
                let (kind, text) = match &b.block {
                    Some(content_block::Block::Text(t)) => ("assistant", t.clone()),
                    Some(content_block::Block::Narration(t)) => ("assistant", format!("Summarized update: {t}")),
                    Some(content_block::Block::Thinking(t)) => ("thinking", t.clone()),
                    Some(content_block::Block::SignedThinking(t)) => ("thinking", t.text.clone()),
                    Some(content_block::Block::Reasoning(r)) => ("thinking", r.summary.clone()),
                    Some(content_block::Block::ToolUse(u)) => ("tool_call", tool_call_text(&u.name, &u.input_json)),
                    _ => continue,
                };
                out.push((i as u32, kind, text));
            }
        }
        Some(Kind::ToolResult(r)) => out.push((0, "tool_result", r.output.clone())),
        Some(Kind::Error(e)) => out.push((0, "error", e.message.clone())),
        Some(Kind::Compaction(c)) => out.push((0, "summary", c.summary.clone())),
        _ => {}
    }
    out.retain(|(_, _, t)| !t.trim().is_empty());
    for (_, _, t) in &mut out {
        truncate(t, MAX_TEXT);
    }
    out
}

/// Follow the agent's text fallback: no raster images, raw documents, invalid
/// UTF-8 or binary controls. Worker-extracted text is a separate attachment.
fn readable_attachment(a: &c6_proto::Attachment) -> Option<&str> {
    let mime = a.mime.split(';').next().unwrap_or_default().trim();
    if [
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "application/pdf",
        "text/html",
        "application/xhtml+xml",
        "application/rtf",
        "text/rtf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "application/vnd.oasis.opendocument.text",
    ]
    .iter()
    .any(|t| mime.eq_ignore_ascii_case(t))
        || a.name.rsplit_once('.').is_some_and(|(_, ext)| {
            ["pdf", "docx", "pptx", "xlsx", "odt", "html", "htm", "xhtml", "rtf"].iter().any(|t| ext.eq_ignore_ascii_case(t))
        })
        || a.data[..a.data.len().min(1024)].windows(5).any(|w| w == b"%PDF-")
    {
        return None;
    }
    let text = std::str::from_utf8(&a.data).ok()?;
    (!text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0c'))).then_some(text)
}

/// A tool call as text: its name and the string values of its arguments
/// (commands, paths, patches) unescaped, so they match as typed.
fn tool_call_text(name: &str, input_json: &str) -> String {
    fn strings(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
            serde_json::Value::Object(o) => o.values().for_each(|v| strings(v, out)),
            other => out.push(other.to_string()),
        }
    }
    let mut parts = vec![name.to_owned()];
    match serde_json::from_str::<serde_json::Value>(input_json) {
        Ok(v) => strings(&v, &mut parts),
        Err(_) => parts.push(input_json.to_owned()),
    }
    parts.join("\n")
}

fn truncate(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Indexes `ev` (called in the transaction that stores it).
pub(crate) async fn index_event(conn: &mut SqliteConnection, session_id: &str, ev: &SessionEvent) -> anyhow::Result<()> {
    for (part, kind, text) in entries(ev) {
        let result = sqlx::query("INSERT INTO search (text, session_id, seq, part, kind) VALUES (?, ?, ?, ?, ?)")
            .bind(text)
            .bind(session_id)
            .bind(ev.seq as i64)
            .bind(part as i64)
            .bind(kind)
            .execute(&mut *conn)
            .await?;
        sqlx::query("INSERT INTO search_entries (rowid, session_id, seq) VALUES (?, ?, ?)")
            .bind(result.last_insert_rowid())
            .bind(session_id)
            .bind(ev.seq as i64)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

const BACKFILL_BATCH: i64 = 200;

/// Clears only the old generation's rowids, then replaces bounded event
/// ranges atomically with their cursor. Live ingest uses the same rowid
/// bookkeeping, so replacing a range never duplicates or loses its events.
/// Every commit returns its connection before yielding to foreground writes.
pub async fn backfill(store: &Store) -> anyhow::Result<()> {
    backfill_between_chunks(store, |_| std::future::ready(())).await
}

// The boundary callback also lets tests stop at a committed cursor, rather
// than racing a timer against the background task on a shared machine.
async fn backfill_between_chunks<F, Fut>(store: &Store, mut between: F) -> anyhow::Result<()>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if store.setting(VERSION_KEY).await?.as_deref() == Some(VERSION) {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("INSERT OR IGNORE INTO search_rebuild (version, old_rowid) VALUES (?, COALESCE((SELECT rowid FROM search ORDER BY rowid DESC LIMIT 1), 0))")
        .bind(VERSION)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut purged = 0usize;
    let mut logged = std::time::Instant::now();
    loop {
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (cutoff, cursor, cleared): (i64, i64, bool) =
            sqlx::query_as("SELECT old_rowid, cursor, cleared FROM search_rebuild WHERE version = ?")
                .bind(VERSION)
                .fetch_one(&mut *tx)
                .await?;
        if cleared {
            tx.commit().await?;
            break;
        }
        // Delete by rowid: filtering on FTS's unindexed session_id would
        // scan hundreds of thousands of rows while holding the write lock.
        let rows: Vec<i64> = sqlx::query_scalar("SELECT rowid FROM search WHERE rowid > ? AND rowid <= ? ORDER BY rowid LIMIT ?")
            .bind(cursor)
            .bind(cutoff)
            .bind(BACKFILL_BATCH)
            .fetch_all(&mut *tx)
            .await?;
        for rowid in &rows {
            sqlx::query("DELETE FROM search WHERE rowid = ?").bind(rowid).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM search_entries WHERE rowid = ?").bind(rowid).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE search_rebuild SET cursor = ?, cleared = ? WHERE version = ?")
            .bind(rows.last().copied().unwrap_or(cursor))
            .bind(rows.len() < BACKFILL_BATCH as usize)
            .bind(VERSION)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        purged += rows.len();
        if logged.elapsed().as_secs() >= 5 {
            let rate = purged as f64 / started.elapsed().as_secs_f64();
            tracing::info!(
                sessions = 0,
                events = purged,
                rows_per_second = rate,
                eta_seconds = (cutoff - rows.last().copied().unwrap_or(cursor)).max(0) as f64 / rate.max(1.0),
                "search index clearing old generation"
            );
            logged = std::time::Instant::now();
        }
        tokio::task::yield_now().await;
        between(0).await;
    }
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM sessions ORDER BY id").fetch_all(&store.pool).await?;
    let mut events = 0usize;
    let mut logged = std::time::Instant::now();
    for (sessions, id) in ids.iter().enumerate() {
        while let Some(n) = backfill_chunk(store, id).await? {
            events += n;
            if logged.elapsed().as_secs() >= 5 {
                let rate = events as f64 / started.elapsed().as_secs_f64();
                let remaining: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM events e LEFT JOIN search_backfill b ON b.version = ? AND b.session_id = e.session_id \
                     WHERE e.seq > COALESCE(b.seq, 0) AND COALESCE(b.done, 0) = 0 \
                     AND (b.session_id IS NULL OR e.seq <= b.through)",
                )
                .bind(VERSION)
                .fetch_one(&store.pool)
                .await?;
                tracing::info!(
                    sessions,
                    total_sessions = ids.len(),
                    events,
                    events_per_second = rate,
                    eta_seconds = remaining as f64 / rate.max(1.0),
                    "search index rebuilding"
                );
                logged = std::time::Instant::now();
            }
            tokio::task::yield_now().await;
            between(events).await;
        }
    }
    store.set_setting(VERSION_KEY, VERSION).await?;
    tracing::info!(sessions = ids.len(), events, elapsed = ?started.elapsed(), "search index built");
    Ok(())
}

/// `None` means this session was already complete (including before restart).
async fn backfill_chunk(store: &Store, id: &str) -> anyhow::Result<Option<usize>> {
    let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        "INSERT OR IGNORE INTO search_backfill (version, session_id, through) \
                 VALUES (?, ?, COALESCE((SELECT seq FROM events WHERE session_id = ? ORDER BY seq DESC LIMIT 1), 0))",
    )
    .bind(VERSION)
    .bind(id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let (seq, through, done): (i64, i64, bool) =
        sqlx::query_as("SELECT seq, through, done FROM search_backfill WHERE version = ? AND session_id = ?")
            .bind(VERSION)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if done {
        tx.commit().await?;
        return Ok(None);
    }
    let rows = sqlx::query("SELECT seq, data FROM events WHERE session_id = ? AND seq > ? AND seq <= ? ORDER BY seq LIMIT ?")
        .bind(id)
        .bind(seq)
        .bind(through)
        .bind(BACKFILL_BATCH)
        .fetch_all(&mut *tx)
        .await?;
    let end = rows.last().map_or(seq, |r| r.get::<i64, _>("seq"));
    sqlx::query("DELETE FROM search WHERE rowid IN (SELECT rowid FROM search_entries WHERE session_id = ? AND seq > ? AND seq <= ?)")
        .bind(id)
        .bind(seq)
        .bind(end)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM search_entries WHERE session_id = ? AND seq > ? AND seq <= ?")
        .bind(id)
        .bind(seq)
        .bind(end)
        .execute(&mut *tx)
        .await?;
    for row in &rows {
        match SessionEvent::decode(row.get::<Vec<u8>, _>("data").as_slice()) {
            Ok(ev) => index_event(&mut tx, id, &ev).await?,
            Err(error) => {
                // One damaged log record must not leave every subsequent
                // session unsearchable after clearing an old generation.
                tracing::warn!(session = id, seq = row.get::<i64, _>("seq"), %error, "cannot index damaged event");
            }
        }
    }
    sqlx::query("UPDATE search_backfill SET seq = ?, done = ? WHERE version = ? AND session_id = ?")
        .bind(end)
        .bind(rows.len() < BACKFILL_BATCH as usize)
        .bind(VERSION)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(rows.len()))
}

/// The user's query as an FTS5 expression: whitespace-separated terms and
/// "quoted phrases" must all match; a leading `-` excludes. Each is matched
/// as a substring (trigram), so it needs at least 3 characters.
/// Also returns the searched terms and the ones ignored as too short.
fn fts_query(q: &str) -> anyhow::Result<(String, Vec<String>, Vec<String>)> {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    let mut ignored = Vec::new();
    let mut chars = q.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let negate = c == '-';
        if negate {
            chars.next();
        }
        let term: String = if chars.peek() == Some(&'"') {
            chars.next();
            chars.by_ref().take_while(|c| *c != '"').collect()
        } else {
            let mut t = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                t.push(c);
                chars.next();
            }
            t
        };
        let term = term.trim().to_owned();
        if term.chars().count() < 3 {
            if !term.is_empty() {
                ignored.push(if negate { format!("-{term}") } else { term });
            }
            continue;
        }
        if negate { exclude.push(term) } else { include.push(term) }
    }
    if include.is_empty() {
        return Err(bad_request(if ignored.is_empty() {
            "Search for at least one word of 3 or more characters".to_owned()
        } else {
            format!(
                "Every word was shorter than 3 characters ({}): use longer words, or a \"quoted phrase\" that includes them",
                ignored.join(", ")
            )
        }));
    }
    let quote = |t: &String| format!("\"{}\"", t.replace('"', "\"\""));
    let mut expr = include.iter().map(quote).collect::<Vec<_>>().join(" AND ");
    for t in &exclude {
        expr = format!("({expr}) NOT {}", quote(t));
    }
    Ok((expr, include, ignored))
}

/// A snippet around the first match in `highlighted` (text with matches
/// between \u{1} and \u{2}): about 80 characters before it and 200 after.
fn snippet(highlighted: &str) -> Vec<api::SnippetPart> {
    let chars: Vec<char> = highlighted.chars().collect();
    let first = chars.iter().position(|c| *c == '\u{1}').unwrap_or(0);
    let mut start = first.saturating_sub(80);
    let mut end = (first + 200).min(chars.len());
    // Start and end at word boundaries when one is near.
    if start > 0
        && let Some(p) = chars[start..first].iter().position(|c| c.is_whitespace())
    {
        start += p + 1;
    }
    if end < chars.len()
        && let Some(p) = chars[first..end].iter().rposition(|c| c.is_whitespace())
    {
        end = (first + p).max(first + 1);
    }
    // A match cut by the window still counts as one.
    let mut hit = chars[..start].iter().filter(|c| **c == '\u{1}').count() > chars[..start].iter().filter(|c| **c == '\u{2}').count();
    let mut parts: Vec<api::SnippetPart> = Vec::new();
    let mut cur = String::new();
    if start > 0 {
        cur.push('…');
    }
    let flush = |parts: &mut Vec<api::SnippetPart>, cur: &mut String, hit: bool| {
        if !cur.is_empty() {
            // Collapse runs of whitespace (code and logs have many).
            let text = cur.split_whitespace().collect::<Vec<_>>().join(" ");
            let text = if cur.starts_with(char::is_whitespace) && !parts.is_empty() { format!(" {text}") } else { text };
            let text = if cur.ends_with(char::is_whitespace) { format!("{text} ") } else { text };
            parts.push(api::SnippetPart { text, hit });
            cur.clear();
        }
    };
    for &c in &chars[start..end] {
        match c {
            '\u{1}' => {
                flush(&mut parts, &mut cur, hit);
                hit = true;
            }
            '\u{2}' => {
                flush(&mut parts, &mut cur, hit);
                hit = false;
            }
            c => cur.push(c),
        }
    }
    if end < chars.len() {
        cur.push('…');
    }
    flush(&mut parts, &mut cur, hit);
    parts
}

/// The transcript element showing an event: a tool result shows inside its
/// call's card, everything else in the event's own block.
fn anchor(session_id: &str, seq: u64, data: &[u8]) -> String {
    match SessionEvent::decode(data).ok().and_then(|e| e.kind) {
        Some(session_event::Kind::ToolResult(r)) => format!("call-{}", r.call_id),
        _ => format!("{session_id}-e{seq}"),
    }
}

/// Where a side chat or subagent is shown: the top-level session it is in,
/// with the side chats and subagent threads to expand on the way down
/// (outermost first).
async fn open_in(store: &Store, row: &crate::store::SessionRow) -> anyhow::Result<Option<api::OpenIn>> {
    let mut chain = Vec::new();
    let mut cur = row.clone();
    while let Some(parent) = cur.parent_id.clone() {
        chain.push(cur.id.clone());
        match store.session(&parent).await? {
            Some(p) => cur = p,
            None => return Ok(None),
        }
        if chain.len() > 64 {
            return Ok(None); // a cycle would be a bug; show the side chat alone
        }
    }
    if chain.is_empty() {
        return Ok(None);
    }
    chain.reverse();
    Ok(Some(api::OpenIn { session_id: cur.id, title: cur.title, side_chats: chain }))
}

/// Matches read for one session (find in session).
const SESSION_MATCHES: usize = 1000;

/// Every match of `q` in a session, its side chats and its subagents, in
/// the order the transcript shows them (a side chat right after the event
/// it branches from, a subagent's thread at the call that started it), for
/// stepping through them.
pub async fn session_matches(store: &Store, session_id: &str, q: &str) -> anyhow::Result<api::SessionMatches> {
    let (expr, terms, ignored) = fts_query(q)?;
    // The session and everything below it, with each one's place in the
    // page: its parent's key, then (branch event, 1 = after it, creation
    // time).
    let rows = sqlx::query(
        "WITH RECURSIVE tree(id) AS (SELECT ? UNION ALL \
         SELECT s.id FROM sessions s JOIN tree ON s.parent_id = tree.id) \
         SELECT s.id, s.parent_id, s.parent_call_id, s.fork_seq, s.created_at FROM sessions s JOIN tree ON s.id = tree.id",
    )
    .bind(session_id)
    .fetch_all(&store.pool)
    .await?;
    if rows.is_empty() {
        return Err(crate::not_found("no such session"));
    }
    // A subagent's place: the event in its parent that started it.
    let mut spawned_at: HashMap<String, i64> = HashMap::new();
    let mut read: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &rows {
        let (Some(parent), Some(_)) = (r.get::<Option<String>, _>("parent_id"), r.get::<Option<String>, _>("parent_call_id")) else {
            continue;
        };
        if !read.insert(parent.clone()) {
            continue;
        }
        for e in store.events_after(&parent, 0, i64::MAX).await? {
            if let Some(session_event::Kind::SubagentSpawned(sp)) = &e.kind {
                // Later links update task identities, not the child's place.
                spawned_at.entry(sp.child_session_id.clone()).or_insert(e.seq as i64);
            }
        }
    }
    let parents: HashMap<String, (Option<String>, i64, i64)> = rows
        .iter()
        .map(|r| {
            let id: String = r.get("id");
            let fork = r.get::<Option<i64>, _>("fork_seq").or_else(|| spawned_at.get(&id).copied()).unwrap_or(0);
            (id, (r.get("parent_id"), fork, r.get("created_at")))
        })
        .collect();
    // Order key and side chats to open, per session in the tree.
    fn place(id: &str, root: &str, parents: &HashMap<String, (Option<String>, i64, i64)>) -> (Vec<i64>, Vec<String>) {
        if id == root {
            return (Vec::new(), Vec::new());
        }
        let (parent, fork, created) = &parents[id];
        let (mut key, mut open) = place(parent.as_deref().unwrap_or(root), root, parents);
        key.extend([*fork, 1, *created]);
        open.push(id.to_owned());
        (key, open)
    }
    let ids: Vec<&String> = parents.keys().collect();
    let sql = format!(
        "SELECT search.session_id AS sid, search.seq AS seq, search.part AS part, search.kind AS kind, e.data AS data \
         FROM search JOIN events e ON e.session_id = search.session_id AND e.seq = search.seq \
         WHERE search MATCH ? AND search.session_id IN ({}) LIMIT ?",
        vec!["?"; ids.len()].join(", ")
    );
    // Safe: only `?` placeholders are added; every value is bound.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&expr);
    for id in &ids {
        query = query.bind(*id);
    }
    let hits = query.bind(SESSION_MATCHES as i64 + 1).fetch_all(&store.pool).await?;
    let truncated = hits.len() > SESSION_MATCHES;
    let mut matches: Vec<(Vec<i64>, api::SessionMatch)> = hits
        .iter()
        .take(SESSION_MATCHES)
        .map(|r| {
            let sid: String = r.get("sid");
            let seq = r.get::<i64, _>("seq");
            let part = r.get::<i64, _>("part");
            let (mut key, open) = place(&sid, session_id, &parents);
            key.extend([seq, 0, part]);
            // This session's event that shows it (its own, or where a side chat branches).
            let at = key.first().copied().unwrap_or(seq).max(0) as u64;
            let m = api::SessionMatch {
                anchor: anchor(&sid, seq as u64, &r.get::<Vec<u8>, _>("data")),
                at,
                seq: seq as u64,
                open,
                kind: r.get("kind"),
            };
            (key, m)
        })
        .collect();
    matches.sort_by(|a, b| a.0.cmp(&b.0));
    // Several entries of one block (or a call and its output) reveal the
    // same element: step through each element once.
    // (Per session: call ids are only unique within one.)
    let mut seen = std::collections::HashSet::new();
    let matches = matches.into_iter().map(|(_, m)| m).filter(|m| seen.insert((m.open.last().cloned(), m.anchor.clone()))).collect();
    Ok(api::SessionMatches { matches, terms, ignored, truncated })
}

/// Searches; sessions ordered by their best match.
/// `head`: the head's own worker, for the sessions it runs.
pub async fn search(store: &Store, req: &api::SearchQuery, head: Option<c6_core::WorkerId>) -> anyhow::Result<api::SearchResponse> {
    let (expr, terms, ignored) = fts_query(&req.q)?;
    let kinds: Vec<String> =
        req.kind.as_deref().unwrap_or("").split(',').map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned).collect();
    let mut sql = String::from(
        "SELECT search.session_id AS sid, search.seq AS seq, search.part AS part, search.kind AS kind, \
         highlight(search, 0, char(1), char(2)) AS hl, e.ts AS ts, e.data AS data \
         FROM search JOIN sessions s ON s.id = search.session_id \
         JOIN events e ON e.session_id = search.session_id AND e.seq = search.seq \
         WHERE search MATCH ?",
    );
    if req.worker.is_some() {
        sql.push_str(" AND s.worker_id = ?");
    }
    if !kinds.is_empty() {
        sql.push_str(&format!(" AND search.kind IN ({})", vec!["?"; kinds.len()].join(", ")));
    }
    sql.push_str(" ORDER BY bm25(search) LIMIT ?");
    // Safe: the SQL only gains fixed clauses and `?` placeholders; every
    // value is bound.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&expr);
    if let Some(w) = &req.worker {
        query = query.bind(w.to_string());
    }
    for k in &kinds {
        query = query.bind(k);
    }
    let rows = query.bind(CANDIDATES).fetch_all(&store.pool).await.map_err(|e| match &e {
        sqlx::Error::Database(d) if d.message().contains("fts5") => bad_request(format!("cannot search for that: {}", d.message())),
        _ => e.into(),
    })?;
    let truncated = rows.len() as i64 == CANDIDATES;

    // Group by session, keeping the best-ranked order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, (Vec<api::SearchHit>, u32)> = HashMap::new();
    for r in &rows {
        let sid: String = r.get("sid");
        let (hits, total) = groups.entry(sid.clone()).or_insert_with(|| {
            order.push(sid.clone());
            (Vec::new(), 0)
        });
        *total += 1;
        if hits.len() < HITS_PER_SESSION {
            let seq = r.get::<i64, _>("seq") as u64;
            hits.push(api::SearchHit {
                anchor: anchor(&sid, seq, &r.get::<Vec<u8>, _>("data")),
                seq,
                part: r.get::<i64, _>("part") as u32,
                kind: r.get("kind"),
                ts: r.get("ts"),
                snippet: snippet(&r.get::<String, _>("hl")),
            });
        }
    }
    let limit = req.limit.unwrap_or(30).clamp(1, 100) as usize;
    let wanted = c6_api::labels::parse_labels(req.labels.as_deref().unwrap_or(""));
    let mut results = Vec::new();
    for sid in order {
        if results.len() == limit {
            break;
        }
        let Some(row) = store.session(&sid).await? else { continue };
        if !c6_api::labels::applies(&wanted, &row.all_labels()) {
            continue;
        }
        let worker_name = match store.worker(row.worker_id.parse()?).await? {
            Some(w) => w.name,
            None => String::new(),
        };
        let (hits, total_hits) = groups.remove(&sid).unwrap_or_default();
        let open_in = open_in(store, &row).await?;
        results.push(api::SearchResult { session: crate::convert::session(&row, head), worker_name, hits, total_hits, open_in });
    }
    Ok(api::SearchResponse { results, terms, ignored, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn backfill_chunks_resume_and_replace_live_events_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c6h.db");
        let store = Store::open(&path).await.unwrap();
        let worker = c6_core::WorkerId::new();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at) \
             VALUES ('large', ?, 'Large', '', 'c', 'm', 'idle', 0, 0)",
        )
        .bind(worker.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        let event = |seq| SessionEvent {
            seq,
            kind: Some(session_event::Kind::User(c6_proto::UserMessage { text: format!("needle {seq}"), ..Default::default() })),
            ..Default::default()
        };
        let input: Vec<_> = (1..=1200).map(event).collect();
        store.ingest(worker, "large", &input).await.unwrap().unwrap();
        // Simulate the completed purge phase of a version bump. Ingested
        // rows are already present: range replacement must not double them.
        sqlx::query("INSERT INTO search_rebuild (version, old_rowid, cleared) VALUES (?, 0, 1)")
            .bind(VERSION)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(backfill_chunk(&store, "large").await.unwrap(), Some(200));
        let cursor: i64 =
            sqlx::query_scalar("SELECT seq FROM search_backfill WHERE session_id = 'large'").fetch_one(&store.pool).await.unwrap();
        assert_eq!(cursor, 200, "one transaction must not rebuild a whole large session");
        let first_rowid: i64 = sqlx::query_scalar("SELECT rowid FROM search_entries WHERE session_id = 'large' AND seq = 1")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        // Foreground ingest and an API settings write between chunks. The
        // transaction has returned its pooled connection and writer lock.
        store.ingest(worker, "large", &[event(1201)]).await.unwrap().unwrap();
        store.set_setting("backfill-test", "foreground write").await.unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at) \
             VALUES ('finished', ?, 'Finished', '', 'c', 'm', 'idle', 0, 0)",
        )
        .bind(worker.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        store.ingest(worker, "finished", &[event(1)]).await.unwrap().unwrap();
        assert_eq!(backfill_chunk(&store, "finished").await.unwrap(), Some(1));
        let finished_rowid: i64 =
            sqlx::query_scalar("SELECT rowid FROM search_entries WHERE session_id = 'finished'").fetch_one(&store.pool).await.unwrap();
        store.pool.close().await;
        let store = Store::open(&path).await.unwrap();
        backfill(&store).await.unwrap();
        let finished_after: i64 =
            sqlx::query_scalar("SELECT rowid FROM search_entries WHERE session_id = 'finished'").fetch_one(&store.pool).await.unwrap();
        assert_eq!(finished_after, finished_rowid, "restart must skip completed sessions");
        let after: i64 = sqlx::query_scalar("SELECT rowid FROM search_entries WHERE session_id = 'large' AND seq = 1")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(after, first_rowid, "restart must not redo a committed range");
        let (rows, distinct): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COUNT(DISTINCT seq) FROM search WHERE session_id = 'large' AND search MATCH 'needle'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!((rows, distinct), (1201, 1201));
        let high_water: (i64, i64) =
            sqlx::query_as("SELECT seq, through FROM search_backfill WHERE session_id = 'large'").fetch_one(&store.pool).await.unwrap();
        assert_eq!(high_water, (1200, 1200), "live ingest must not extend the historical rebuild");
        assert_eq!(backfill_chunk(&store, "large").await.unwrap(), None, "finished sessions are durable too");
        assert_eq!(store.setting(VERSION_KEY).await.unwrap().as_deref(), Some(VERSION));
    }

    #[tokio::test]
    async fn version_bump_clears_legacy_rows_including_entries_that_no_longer_exist() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        // Old indexes have no search_entries bookkeeping. Include exactly
        // one full batch to exercise the empty final purge transaction.
        for seq in 1..=BACKFILL_BATCH {
            sqlx::query("INSERT INTO search (text, session_id, seq, part, kind) VALUES ('obsolete', 'deleted', ?, 0, 'user')")
                .bind(seq)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        store.set_setting(VERSION_KEY, "old").await.unwrap();
        backfill(&store).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM search").fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 0);
        assert_eq!(store.setting(VERSION_KEY).await.unwrap().as_deref(), Some(VERSION));
    }

    #[tokio::test]
    async fn background_backfill_releases_writer_for_ingest_and_api_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let worker = c6_core::WorkerId::new();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at) \
             VALUES ('large', ?, 'Large', '', 'c', 'm', 'idle', 0, 0)",
        )
        .bind(worker.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        let event = |seq| SessionEvent {
            seq,
            kind: Some(session_event::Kind::User(c6_proto::UserMessage { text: format!("needle {seq}"), ..Default::default() })),
            ..Default::default()
        };
        let input: Vec<_> = (1..=1200).map(event).collect();
        store.ingest(worker, "large", &input).await.unwrap().unwrap();
        sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES ('damaged', 1, 0, X'FF')").execute(&store.pool).await.unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at) \
             VALUES ('damaged', ?, 'Damaged', '', 'c', 'm', 'idle', 0, 0)",
        )
        .bind(worker.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        store.set_setting(VERSION_KEY, "old").await.unwrap();
        let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let background_store = store.clone();
        let task = tokio::spawn(async move {
            let mut gate = Some((paused_tx, resume_rx));
            backfill_between_chunks(&background_store, move |events| {
                let gate = if events > 1 { gate.take() } else { None };
                async move {
                    if let Some((paused, resume)) = gate {
                        paused.send(()).unwrap();
                        resume.await.unwrap();
                    }
                }
            })
            .await
            .unwrap();
        });
        c6_testkit::within("backfill's first committed event chunk", paused_rx).await.unwrap();
        // A separate foreground writer proves no pooled connection or
        // transaction survives the boundary. No timeout measures throughput.
        c6_testkit::within("ingest during backfill", store.ingest(worker, "large", &[event(1201)])).await.unwrap().unwrap();
        c6_testkit::within("API settings write during backfill", store.set_setting("backfill-test", "write")).await.unwrap();
        resume_tx.send(()).unwrap();
        c6_testkit::within("background index completion", task).await.unwrap();
        let (rows, distinct): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COUNT(DISTINCT seq) FROM search WHERE session_id = 'large' AND search MATCH 'needle'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!((rows, distinct), (1201, 1201));
    }

    #[tokio::test]
    async fn purge_does_not_revisit_live_rows_when_sqlite_reuses_a_rowid() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let worker = c6_core::WorkerId::new();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at) \
             VALUES ('live', ?, 'Live', '', 'c', 'm', 'idle', 0, 0)",
        )
        .bind(worker.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        for seq in 1..=BACKFILL_BATCH {
            sqlx::query("INSERT INTO search (text, session_id, seq, part, kind) VALUES ('obsolete', 'deleted', ?, 0, 'user')")
                .bind(seq)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        let mut live = Some(SessionEvent {
            seq: 1,
            kind: Some(session_event::Kind::User(c6_proto::UserMessage { text: "needle".into(), ..Default::default() })),
            ..Default::default()
        });
        backfill_between_chunks(&store, |_| {
            let live = live.take();
            let store = &store;
            async move {
                if let Some(ev) = live {
                    // All 200 old rows have just been removed. SQLite may
                    // reuse rowid 1; the durable purge cursor must skip it.
                    store.ingest(worker, "live", &[ev]).await.unwrap().unwrap();
                }
            }
        })
        .await
        .unwrap();
        let rows: Vec<(String, i64)> = sqlx::query_as("SELECT session_id, seq FROM search").fetch_all(&store.pool).await.unwrap();
        assert_eq!(rows, [("live".into(), 1)]);
    }

    #[tokio::test]
    async fn continuation_spawn_links_keep_the_original_child_insertion_point() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let worker = c6_core::WorkerId::new();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at) \
             VALUES ('parent', ?, 'Parent', '', 'c', 'm', 'idle', 0, 0)",
        )
        .bind(worker.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        let user = |seq| SessionEvent {
            seq,
            kind: Some(session_event::Kind::User(c6_proto::UserMessage { text: "needle".into(), ..Default::default() })),
            ..Default::default()
        };
        let spawn = |seq, sp| SessionEvent { seq, kind: Some(session_event::Kind::SubagentSpawned(sp)), ..Default::default() };
        let first = c6_proto::SubagentSpawned {
            call_id: "first-call".into(),
            child_session_id: "first".into(),
            description: "Same title".into(),
            ..Default::default()
        };
        let sibling = c6_proto::SubagentSpawned { call_id: "sibling-call".into(), child_session_id: "sibling".into(), ..first.clone() };
        assert!(
            store
                .ingest(worker, "parent", &[user(1), spawn(2, first.clone()), user(3), spawn(4, sibling), user(5)])
                .await
                .unwrap()
                .is_some()
        );
        for (id, call) in [("first", "first-call"), ("sibling", "sibling-call")] {
            let started = SessionEvent {
                seq: 1,
                kind: Some(session_event::Kind::Started(c6_proto::SessionStarted {
                    title: "Same title".into(),
                    parent_session_id: "parent".into(),
                    parent_call_id: call.into(),
                    ..Default::default()
                })),
                ..Default::default()
            };
            assert!(store.ingest(worker, id, &[started, user(2)]).await.unwrap().is_some());
        }
        let before = session_matches(&store, "parent", "needle").await.unwrap();
        let places = |m: &api::SessionMatches| {
            m.matches.iter().map(|hit| (hit.open.last().cloned().unwrap_or_else(|| "parent".into()), hit.at)).collect::<Vec<_>>()
        };
        assert_eq!(
            places(&before),
            [("parent".into(), 1), ("first".into(), 2), ("parent".into(), 3), ("sibling".into(), 4), ("parent".into(), 5)]
        );

        let continuation = c6_proto::SubagentSpawned { call_id: "continue-call".into(), background_task_id: "bg2".into(), ..first };
        assert!(store.ingest(worker, "parent", &[spawn(6, continuation)]).await.unwrap().is_some());
        let after = session_matches(&store, "parent", "needle").await.unwrap();
        assert_eq!(after, before, "a new task link must not move or duplicate the child's matches after its sibling");
        assert!(store.ingest(worker, "first", &[user(3)]).await.unwrap().is_some());
        let resumed = session_matches(&store, "parent", "needle").await.unwrap();
        assert_eq!(
            places(&resumed),
            [
                ("parent".into(), 1),
                ("first".into(), 2),
                ("first".into(), 2),
                ("parent".into(), 3),
                ("sibling".into(), 4),
                ("parent".into(), 5)
            ],
            "new child matches also remain at its original insertion, before the sibling"
        );
    }

    #[test]
    fn card_answers_are_searchable_as_user_messages() {
        let text = "Not now: task card #185 \"Cleanup\".";
        let input = SessionEvent {
            seq: 17,
            kind: Some(session_event::Kind::User(c6_proto::UserMessage {
                command_id: "test".into(),
                text: text.into(),
                command: "card-answer".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(entries(&input), [(0, "user", text.into())]);
        assert_eq!(crate::convert::event(&input).unwrap().seq, 17, "search points to the user message");
    }

    #[test]
    fn attachment_search_indexes_names_and_readable_text_but_not_raw_binary() {
        let files: &[(&str, &str, &[u8])] = &[
            ("source.rs", "text/x-rust", b"inline code\r\n\tmore code"),
            ("report.pdf (extracted text)", "text/plain", "Extracted résumé\x0cpage two".as_bytes()),
            ("typed-document", "Application/PDF; charset=UTF-8", b"declared PDF raw data"),
            ("report.PDF", "application/octet-stream", b"named PDF raw data"),
            ("no-extension", "application/octet-stream", b"%PDF-1.7\nheader PDF raw data"),
            ("artifact.bin", "application/octet-stream", b"non-UTF8 raw data\xff"),
            ("controls.bin", "text/plain", b"control raw data\x00"),
            ("photo.png", "IMAGE/PNG", b"mislabeled image data"),
            ("page.html", "text/html", b"<script>HTML raw data</script>"),
            ("letter.rtf", "application/octet-stream", b"{\\rtf1 RTF raw data}"),
        ];
        let ev = SessionEvent {
            kind: Some(session_event::Kind::User(c6_proto::UserMessage {
                text: "Please review".into(),
                attachments: files
                    .iter()
                    .map(|&(name, mime, data)| c6_proto::Attachment { name: name.into(), mime: mime.into(), data: data.to_vec() })
                    .collect(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let entries = entries(&ev);
        assert_eq!(entries.len(), 1);
        let indexed = &entries[0].2;
        for (name, _, _) in files {
            assert!(indexed.contains(name), "attachment name missing: {name}");
        }
        for text in ["Please review", "inline code", "more code", "Extracted résumé", "page two"] {
            assert!(indexed.contains(text), "readable text missing: {text}");
        }
        assert!(!indexed.contains("raw data") && !indexed.contains("mislabeled image data"), "indexed raw attachment bytes: {indexed}");
    }

    #[test]
    fn narration_is_searchable_as_a_labeled_assistant_update() {
        let input = SessionEvent {
            kind: Some(session_event::Kind::Assistant(c6_proto::AssistantMessage {
                blocks: vec![c6_proto::ContentBlock {
                    block: Some(content_block::Block::Narration("Visible summary".into())),
                    ..Default::default()
                }],
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(entries(&input), [(0, "assistant", "Summarized update: Visible summary".into())]);
    }

    #[test]
    fn query_syntax() {
        assert_eq!(fts_query("foo_bar src/").unwrap().0, r#""foo_bar" AND "src/""#);
        assert_eq!(fts_query(r#""out of memory" -oom2 x"#).unwrap().0, r#"("out of memory") NOT "oom2""#);
        let (expr, _, ignored) = fts_query(r#"say "hi" macOS 15 -ab"#).unwrap();
        assert_eq!(expr, r#""say" AND "macOS""#, "short words and phrases are dropped");
        assert_eq!(ignored, ["hi", "15", "-ab"]);
        assert!(fts_query("ab 15").unwrap_err().to_string().contains("ab, 15"));
        assert!(fts_query("ab -cdef").is_err());
        assert_eq!(fts_query(r#"a"b"c"#).unwrap().0, r#""a""b""c""#, "quotes inside a term are escaped");
    }

    #[test]
    fn snippets_window_and_mark() {
        let text = format!("{} start \u{1}needle\u{2} middle \u{1}needle\u{2} {}", "x ".repeat(100), "y ".repeat(200));
        let s = snippet(&text);
        assert!(s.first().unwrap().text.starts_with('…'));
        assert!(s.last().unwrap().text.ends_with('…'));
        assert_eq!(s.iter().filter(|p| p.hit).count(), 2);
        assert!(s.iter().filter(|p| p.hit).all(|p| p.text == "needle"));
    }

    #[test]
    fn tool_calls_index_their_arguments() {
        let t = tool_call_text("bash", r#"{"command":"cargo test -p \"c6-head\"","timeout":30}"#);
        assert_eq!(t, "bash\ncargo test -p \"c6-head\"\n30");
    }
}
