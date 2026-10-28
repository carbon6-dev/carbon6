//! Full-text search over sessions (SQLite FTS5, trigram tokens).
//!
//! Every durable event becomes zero or more rows in the `search` table, in
//! the same transaction that stores the event, so the index never drifts
//! from the log. The index is derived data: when `search.version` differs
//! from [`VERSION`] (first start, or a format change), a background task
//! rebuilds it session by session.

use std::collections::HashMap;

use c6_api as api;
use c6_proto::{SessionEvent, content_block, session_event};
use prost::Message;
use sqlx::{Row, SqliteConnection};

use crate::bad_request;
use crate::store::Store;

/// Bump when what gets indexed changes; the head then re-indexes.
const VERSION: &str = "1";
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
            // Text attachments (pasted logs, mentioned files) are searchable too.
            let mut text = u.text.clone();
            for a in u.attachments.iter().filter(|a| !a.mime.starts_with("image/")) {
                text.push_str(&format!("\n\n{}\n{}", a.name, String::from_utf8_lossy(&a.data)));
            }
            out.push((0, "user", text))
        }
        Some(Kind::Assistant(a)) => {
            for (i, b) in a.blocks.iter().enumerate() {
                let (kind, text) = match &b.block {
                    Some(content_block::Block::Text(t)) => ("assistant", t.clone()),
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
        sqlx::query("INSERT INTO search (text, session_id, seq, part, kind) VALUES (?, ?, ?, ?, ?)")
            .bind(text)
            .bind(session_id)
            .bind(ev.seq as i64)
            .bind(part as i64)
            .bind(kind)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Rebuilds the index if it is missing or from another version. Each
/// session is re-indexed in one transaction (delete, then insert all), so
/// events ingested meanwhile are neither lost nor doubled.
pub async fn backfill(store: &Store) -> anyhow::Result<()> {
    if store.setting(VERSION_KEY).await?.as_deref() == Some(VERSION) {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let ids: Vec<String> = sqlx::query("SELECT id FROM sessions").fetch_all(&store.pool).await?.iter().map(|r| r.get("id")).collect();
    let mut events = 0usize;
    for id in &ids {
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM search WHERE session_id = ?").bind(id).execute(&mut *tx).await?;
        let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? ORDER BY seq").bind(id).fetch_all(&mut *tx).await?;
        for r in &rows {
            let ev = SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?;
            index_event(&mut tx, id, &ev).await?;
        }
        events += rows.len();
        tx.commit().await?;
    }
    store.set_setting(VERSION_KEY, VERSION).await?;
    tracing::info!(sessions = ids.len(), events, elapsed = ?started.elapsed(), "search index built");
    Ok(())
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
                spawned_at.insert(sp.child_session_id.clone(), e.seq as i64);
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
            let m = api::SessionMatch { anchor: anchor(&sid, seq as u64, &r.get::<Vec<u8>, _>("data")), open, kind: r.get("kind") };
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
pub async fn search(store: &Store, req: &api::SearchQuery) -> anyhow::Result<api::SearchResponse> {
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
        results.push(api::SearchResult { session: crate::convert::session(&row), worker_name, hits, total_hits, open_in });
    }
    Ok(api::SearchResponse { results, terms, ignored, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

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
