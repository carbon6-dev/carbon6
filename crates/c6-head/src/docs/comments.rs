//! Comments (docs/docs.md §8): threads anchored to quoted words, a block or
//! slide, or a whole tab; one can be sent to an agent instead of just left.
//! It goes, as a message (queued if the session is busy), to the session
//! that already answered the thread, else the one that last wrote that
//! part, else the latest session of the document's work or project; never
//! to a new session. It's told what to do by the editable prompt
//! `docs.comment`. The agent answers in the thread, and links the edits it
//! made for it (`answering`). A thread an agent answered needs you until
//! you reply or resolve it; only you resolve a thread you wrote.

use c6_api::docs::{Anchor, CommentEdit, CommentNew, DocComment, DocThread, ThreadNew};
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row as _;

use crate::{HeadState, bad_request, conflict, forbidden, not_found};

const MAX_TEXT: usize = 20_000;

fn check_text(t: &str) -> anyhow::Result<String> {
    let t = t.trim();
    if t.is_empty() || t.len() > MAX_TEXT {
        return Err(bad_request(format!("a comment is 1 to {MAX_TEXT} characters")));
    }
    Ok(t.to_owned())
}

/// Who wrote a comment, for the agent reading the thread.
fn who(by: &str) -> String {
    match by {
        "you" => "the user".into(),
        b if b.starts_with("session:") => format!("an agent ({b})"),
        b => b.to_owned(),
    }
}

/// Whether a writer is an agent (its answer is one you look at).
fn agent(by: &str) -> bool {
    by.starts_with("session:")
}

async fn thread_row(pool: &sqlx::SqlitePool, doc: &str, thread: &str) -> anyhow::Result<sqlx::sqlite::SqliteRow> {
    sqlx::query("SELECT * FROM doc_threads WHERE id = ? AND doc_id = ?")
        .bind(thread)
        .bind(doc)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| not_found(format!("no thread {thread} on this document")))
}

/// Which comment of each thread last went to an agent (thread id to comment
/// id), kept per realm as a setting: `comments_sent@<realm>`.
fn sent_key(realm: &str) -> String {
    format!("comments_sent@{realm}")
}

async fn sent_in(st: &HeadState, realm: &str) -> std::collections::HashMap<String, String> {
    st.store.setting(&sent_key(realm)).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// Whether a thread's last comment (a person's) hasn't gone to an agent:
/// it isn't the one last sent. A thread sent before sends were recorded
/// counts as unsent if it never went to an agent, or if a person spoke after
/// the agent's answer.
fn unsent(sent: &std::collections::HashMap<String, String>, thread: &str, to_agent: bool, comments: &[DocComment]) -> bool {
    let Some(last) = comments.last() else { return false };
    match sent.get(thread) {
        Some(id) => *id != last.id,
        None => !to_agent || comments.iter().any(|c| agent(&c.by)),
    }
}

/// Where a thread's comments no agent has received yet begin: after the
/// one last sent with it. With no record of that (a thread sent before sends
/// were recorded, or resolved since), after the agent's latest answer, as
/// `unsent` counts; none, if it was sent and hasn't been answered.
fn unreceived_from(sent: &std::collections::HashMap<String, String>, thread: &str, to_agent: bool, comments: &[DocComment]) -> usize {
    match sent.get(thread).and_then(|id| comments.iter().position(|c| c.id == *id)) {
        Some(i) => i + 1,
        None if !to_agent => 0,
        None => comments.iter().rposition(|c| agent(&c.by)).map_or(comments.len(), |i| i + 1),
    }
}

/// Notes each thread's latest comment as sent; the record keeps only
/// threads still open.
async fn note_sent(st: &HeadState, realm: &str, pool: &sqlx::SqlitePool, threads: &[String]) -> anyhow::Result<()> {
    let mut sent = sent_in(st, realm).await;
    for t in threads {
        let last: Option<String> = sqlx::query_scalar("SELECT id FROM doc_comments WHERE thread_id = ? ORDER BY at DESC, id DESC LIMIT 1")
            .bind(t)
            .fetch_optional(pool)
            .await?;
        if let Some(id) = last {
            sent.insert(t.clone(), id);
        }
    }
    let open: Vec<String> = sqlx::query_scalar("SELECT id FROM doc_threads WHERE state = 'open'").fetch_all(pool).await?;
    sent.retain(|t, _| open.contains(t));
    st.store.set_setting(&sent_key(realm), &serde_json::to_string(&sent)?).await
}

/// A document's threads as they are now (open first), with whether each is
/// detached and needs you.
pub async fn list(st: &HeadState, realm: &str, doc: &str) -> anyhow::Result<Vec<DocThread>> {
    let sent = sent_in(st, realm).await;
    let pool = super::pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    let (state, _) = super::load_head(&mut conn, doc).await?;
    drop(conn);
    let rows = sqlx::query("SELECT * FROM doc_threads WHERE doc_id = ? ORDER BY state = 'resolved', created_at")
        .bind(doc)
        .fetch_all(&pool)
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let id: String = r.get("id");
        let anchor: Anchor = serde_json::from_str(&r.get::<String, _>("anchor")).unwrap_or_default();
        let mut comments: Vec<DocComment> = sqlx::query("SELECT * FROM doc_comments WHERE thread_id = ? ORDER BY at, id")
            .bind(&id)
            .fetch_all(&pool)
            .await?
            .iter()
            .map(|c| DocComment { id: c.get("id"), by: c.get("by"), text: c.get("text"), at: c.get("at"), editable: false })
            .collect();
        let edits: Vec<i64> = sqlx::query_scalar("SELECT rev FROM doc_ops WHERE doc_id = ? AND answering = ? ORDER BY rev")
            .bind(doc)
            .bind(&id)
            .fetch_all(&pool)
            .await?;
        // Detached: its block is gone, or its words aren't in it any more.
        let detached = match state.rows.get(&anchor.block) {
            _ if anchor.block.is_empty() => !anchor.tab.is_empty() && state.tab(&anchor.tab).is_none(),
            None => true,
            Some(b) => !anchor.quote.is_empty() && !b.text.contains(&anchor.quote),
        };
        let thread_state: String = r.get("state");
        let needs_you = thread_state == "open" && comments.last().is_some_and(|c| agent(&c.by));
        let to_agent = r.get::<i64, _>("to_agent") != 0;
        let unsent =
            thread_state == "open" && comments.last().is_some_and(|last| !agent(&last.by) && unsent(&sent, &id, to_agent, &comments));
        // A person's comment no agent has received, in an open thread, can
        // still change.
        if thread_state == "open" {
            let from = unreceived_from(&sent, &id, to_agent, &comments);
            for c in comments.iter_mut().skip(from) {
                c.editable = !agent(&c.by);
            }
        }
        out.push(DocThread {
            id,
            doc: doc.to_owned(),
            anchor,
            state: thread_state,
            detached,
            about: r.get("about"),
            to_agent,
            agent: r.get("agent"),
            comments,
            edits,
            needs_you,
            unsent,
            created_by: r.get("created_by"),
            created_at: r.get("created_at"),
            resolved_at: r.get("resolved_at"),
            resolved_by: r.get("resolved_by"),
        });
    }
    Ok(out)
}

/// The threads an agent answered and you haven't looked at yet, per
/// document (for work and projects: needs you).
pub async fn answered(st: &HeadState, realm: &str, doc: &str) -> usize {
    match list(st, realm, doc).await {
        Ok(t) => t.iter().filter(|t| t.needs_you).count(),
        Err(_) => 0,
    }
}

/// A thread an agent answered, waiting for your reply or for you to
/// resolve it.
#[derive(Clone, Debug)]
pub struct Answered {
    pub doc: String,
    /// The document's title.
    pub title: String,
    pub thread: String,
    /// What the thread is about (quoted words, a block, a tab).
    pub about: String,
    /// The agent's answer (the thread's latest comment): another means it
    /// answered again.
    pub comment: String,
    /// When the agent answered.
    pub at: i64,
}

/// Every thread in a realm that an agent answered last and that's still
/// open (what `list` calls needs you), newest first, in one query.
pub async fn answered_in(st: &HeadState, realm: &str, limit: i64) -> anyhow::Result<Vec<Answered>> {
    let pool = super::pool(st, realm).await?;
    let rows = sqlx::query(
        "SELECT t.id, t.doc_id, t.about, d.title, c.id AS comment, c.at FROM doc_threads t \
         JOIN docs d ON d.id = t.doc_id AND d.deleted_at IS NULL \
         JOIN doc_comments c ON c.id = (SELECT id FROM doc_comments WHERE thread_id = t.id ORDER BY at DESC, id DESC LIMIT 1) \
         WHERE t.state = 'open' AND c.by LIKE 'session:%' ORDER BY c.at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(&pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| Answered {
            doc: r.get("doc_id"),
            title: r.get("title"),
            thread: r.get("id"),
            about: r.get("about"),
            comment: r.get("comment"),
            at: r.get("at"),
        })
        .collect())
}

/// A new thread with its first comment; sent to an agent when asked.
pub async fn create(st: &HeadState, realm: &str, doc: &str, n: &ThreadNew, by: &str) -> anyhow::Result<DocThread> {
    let text = check_text(&n.text)?;
    let pool = super::pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    let (state, _) = super::load_head(&mut conn, doc).await?;
    drop(conn);
    let mut anchor = n.anchor.clone();
    anchor.quote = anchor.quote.trim().to_owned();
    let about = if anchor.block.is_empty() {
        let tab = if anchor.tab.is_empty() {
            state.tabs_in_order().first().map(|t| t.id.clone()).unwrap_or_default()
        } else {
            anchor.tab.clone()
        };
        let t = state.tab(&tab).ok_or_else(|| not_found(format!("no tab {tab}")))?;
        anchor.tab = tab.clone();
        if !anchor.quote.is_empty() {
            return Err(bad_request("quoted words are in a block: say which (anchor.block)"));
        }
        format!("the tab {}", t.title)
    } else {
        let b = state.row(&anchor.block)?;
        anchor.tab = b.tab.clone();
        if !anchor.quote.is_empty() {
            if anchor.quote.split_whitespace().count() > 20 {
                return Err(bad_request("quote a few words (up to 20), not a passage"));
            }
            if !b.text.contains(&anchor.quote) {
                return Err(bad_request(format!("{:?} isn't in block {}", anchor.quote, b.id)));
            }
            format!("“{}”", anchor.quote)
        } else if b.kind == "slide" {
            let kids = state.children(b);
            let title =
                kids.iter().find(|k| k.region == "title").or(kids.first()).map(|k| super::markdown::first_words(&k.kind, &k.text, 80));
            format!("the slide {}", title.unwrap_or_default())
        } else {
            format!("“{}”", super::markdown::first_words(&b.kind, &b.text, 80))
        }
    };
    let id = uuid::Uuid::now_v7().to_string();
    let now = now_ms();
    sqlx::query("INSERT INTO doc_threads (id, doc_id, anchor, about, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(doc)
        .bind(serde_json::to_string(&anchor)?)
        .bind(&about)
        .bind(by)
        .bind(now)
        .execute(&pool)
        .await?;
    add_comment(&pool, &id, doc, by, &text).await?;
    commented(st, realm, doc, &id, by, &text).await;
    if n.to == "agent" {
        send(st, realm, doc, &id).await?;
    }
    one(st, realm, doc, &id).await
}

async fn add_comment(pool: &sqlx::SqlitePool, thread: &str, doc: &str, by: &str, text: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO doc_comments (id, thread_id, doc_id, by, text, at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(thread)
        .bind(doc)
        .bind(by)
        .bind(text)
        .bind(now_ms())
        .execute(pool)
        .await?;
    Ok(())
}

async fn one(st: &HeadState, realm: &str, doc: &str, thread: &str) -> anyhow::Result<DocThread> {
    list(st, realm, doc).await?.into_iter().find(|t| t.id == thread).ok_or_else(|| not_found(format!("no thread {thread}")))
}

/// A reply; sent (again) to an agent when asked. A reply reopens a
/// resolved thread.
pub async fn reply(st: &HeadState, realm: &str, doc: &str, thread: &str, c: &CommentNew, by: &str) -> anyhow::Result<DocThread> {
    let text = check_text(&c.text)?;
    let pool = super::pool(st, realm).await?;
    thread_row(&pool, doc, thread).await?;
    add_comment(&pool, thread, doc, by, &text).await?;
    sqlx::query("UPDATE doc_threads SET state = 'open', resolved_at = NULL, resolved_by = '' WHERE id = ?")
        .bind(thread)
        .execute(&pool)
        .await?;
    commented(st, realm, doc, thread, by, &text).await;
    if c.to == "agent" {
        send(st, realm, doc, thread).await?;
    }
    one(st, realm, doc, thread).await
}

/// A comment its writer may still change: theirs (a person's), in an open
/// thread, and no agent has received it (`DocComment::editable`).
async fn changeable(st: &HeadState, realm: &str, doc: &str, thread: &str, comment: &str, by: &str) -> anyhow::Result<DocThread> {
    let t = one(st, realm, doc, thread).await?;
    let c = t.comments.iter().find(|c| c.id == comment).ok_or_else(|| not_found(format!("no comment {comment} in this thread")))?;
    if agent(by) || c.by != by {
        return Err(forbidden("only who wrote a comment edits or deletes it"));
    }
    if t.state != "open" {
        return Err(conflict("the thread is resolved: reply to reopen it"));
    }
    if !c.editable {
        return Err(conflict("an agent has received this comment: reply in the thread instead"));
    }
    Ok(t)
}

/// Your comment's text, changed in place (its time stays) while no agent
/// has received it.
pub async fn edit(
    st: &HeadState,
    realm: &str,
    doc: &str,
    thread: &str,
    comment: &str,
    e: &CommentEdit,
    by: &str,
) -> anyhow::Result<DocThread> {
    let text = check_text(&e.text)?;
    changeable(st, realm, doc, thread, comment, by).await?;
    let pool = super::pool(st, realm).await?;
    sqlx::query("UPDATE doc_comments SET text = ? WHERE id = ? AND thread_id = ?")
        .bind(&text)
        .bind(comment)
        .bind(thread)
        .execute(&pool)
        .await?;
    changed(st, realm, doc, by).await;
    one(st, realm, doc, thread).await
}

/// Your comment, gone while no agent has received it; with it the thread,
/// if it was the only one.
pub async fn delete(st: &HeadState, realm: &str, doc: &str, thread: &str, comment: &str, by: &str) -> anyhow::Result<()> {
    changeable(st, realm, doc, thread, comment, by).await?;
    let pool = super::pool(st, realm).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM doc_comments WHERE id = ? AND thread_id = ?").bind(comment).bind(thread).execute(&mut *tx).await?;
    // Only if nothing else is in it (a reply may have just come).
    sqlx::query("DELETE FROM doc_threads WHERE id = ? AND NOT EXISTS (SELECT 1 FROM doc_comments WHERE thread_id = ?)")
        .bind(thread)
        .bind(thread)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    changed(st, realm, doc, by).await;
    Ok(())
}

/// Resolves a thread. An agent resolves only threads it began.
pub async fn resolve(st: &HeadState, realm: &str, doc: &str, thread: &str, by: &str) -> anyhow::Result<DocThread> {
    let pool = super::pool(st, realm).await?;
    let r = thread_row(&pool, doc, thread).await?;
    let made: String = r.get("created_by");
    if agent(by) && made != by {
        return Err(forbidden("only the user resolves a thread they wrote: reply saying what you did, and they will"));
    }
    sqlx::query("UPDATE doc_threads SET state = 'resolved', resolved_at = ?, resolved_by = ? WHERE id = ?")
        .bind(now_ms())
        .bind(by)
        .bind(thread)
        .execute(&pool)
        .await?;
    let _ = st.docs.live.send(c6_api::docs::DocChange {
        id: doc.to_owned(),
        realm: realm.to_owned(),
        rev: 0,
        by: by.to_owned(),
        what: "commented".into(),
    });
    st.attention.changed();
    let title = super::title_of(st, realm, doc).await.map(|t| t.0).unwrap_or_default();
    let event =
        json!({"type": "doc", "op": "thread_resolved", "doc": {"id": doc, "title": title, "realm": realm}, "thread": thread, "by": by});
    crate::triggers::dispatch(st, realm, "doc", "thread_resolved", event).await;
    moved(st, realm, doc).await;
    one(st, realm, doc, thread).await
}

/// A comment went in: on the live feed, as `doc.commented`, and the work
/// and projects holding the document moved.
async fn commented(st: &HeadState, realm: &str, doc: &str, thread: &str, by: &str, text: &str) {
    let _ = st.docs.live.send(c6_api::docs::DocChange {
        id: doc.to_owned(),
        realm: realm.to_owned(),
        rev: 0,
        by: by.to_owned(),
        what: "commented".into(),
    });
    // An agent's answer is worth a look; your reply settles it.
    st.attention.changed();
    let title = super::title_of(st, realm, doc).await.map(|t| t.0).unwrap_or_default();
    let event = json!({"type": "doc", "op": "commented", "doc": {"id": doc, "title": title, "realm": realm}, "thread": thread, "by": by, "text": text});
    crate::triggers::dispatch(st, realm, "doc", "commented", event).await;
    moved(st, realm, doc).await;
}

/// A comment changed or went (no trigger: nobody else has seen it): on the
/// live feed, and the work and projects holding the document moved.
async fn changed(st: &HeadState, realm: &str, doc: &str, by: &str) {
    let _ = st.docs.live.send(c6_api::docs::DocChange {
        id: doc.to_owned(),
        realm: realm.to_owned(),
        rev: 0,
        by: by.to_owned(),
        what: "commented".into(),
    });
    st.attention.changed();
    moved(st, realm, doc).await;
}

async fn moved(st: &HeadState, realm: &str, doc: &str) {
    crate::work::moved_ref(st, &format!("doc:{doc}")).await;
    for p in crate::projects::holding_doc(st, realm, doc).await {
        let _ = crate::projects::touch(st, &p, now_ms()).await;
    }
}

/// Whether a session can be sent a comment: not archived, not Claude
/// Code's own subagent (Claude Code runs it) and didn't fail to start.
/// Busy, waiting on approval or offline is fine: its input queues.
fn takes_comments(s: &crate::store::SessionRow) -> bool {
    s.archived == 0 && !s.observed && s.status != "failed"
}

/// The session `id` if it's still there and can be sent a comment.
async fn taking(st: &HeadState, id: &str) -> anyhow::Result<Option<String>> {
    Ok(st.store.session(id).await?.filter(takes_comments).map(|s| s.id))
}

/// The session a thread goes to, whatever it's doing (a busy one gets it
/// queued); never a new one. The session that already answered the thread,
/// else the one that last wrote its part (the block's or slide's writer,
/// else the document's latest), else the most recently updated session of
/// the document's work or project. Sessions that can't take it are passed
/// over; with none left it's refused.
async fn pick(st: &HeadState, realm: &str, doc: &str, t: &DocThread) -> anyhow::Result<String> {
    if !t.agent.is_empty()
        && let Some(s) = taking(st, &t.agent).await?
    {
        return Ok(s);
    }
    let pool = super::pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    let (state, _) = super::load_head(&mut conn, doc).await?;
    drop(conn);
    // Who wrote that part last (the slide's or block's), then the document.
    let wrote = [state.rows.get(&t.anchor.block), state.rows.values().max_by_key(|r| r.rev)];
    for by in wrote.into_iter().flatten().map(|r| r.by.as_str()) {
        if let Some(sid) = by.strip_prefix("session:")
            && let Some(s) = taking(st, sid).await?
        {
            return Ok(s);
        }
    }
    // The document's work (and its project's work): their latest session.
    let reference = format!("doc:{doc}");
    let mut work: Vec<String> = crate::work::holding(st, &reference).await?;
    for p in crate::projects::holding_doc(st, realm, doc).await {
        work.extend(crate::work::in_project(st, &p).await?);
    }
    let mut best: Option<crate::store::SessionRow> = None;
    for w in &work {
        for r in crate::work::refs(st, w).await {
            if let Some(sid) = r.strip_prefix("session:")
                && let Some(s) = st.store.session(sid).await?
                && takes_comments(&s)
                && best.as_ref().is_none_or(|b| b.updated_at < s.updated_at)
            {
                best = Some(s);
            }
        }
    }
    match best {
        Some(s) => Ok(s.id),
        None => Err(bad_request(
            "no session to send it to: the document's work and project have none yet. Start a session on it (or put it in work), then send the thread",
        )),
    }
}

/// Sends a thread to an agent, by `docs.comment` (see `pick`).
async fn send(st: &HeadState, realm: &str, doc: &str, thread: &str) -> anyhow::Result<()> {
    let t = one(st, realm, doc, thread).await?;
    to_agents(st, realm, doc, &[t]).await
}

/// Comment threads sent to an agent together: each goes where `pick` says
/// (never to a new session), and the threads going to one session go in
/// one message, which answers each in its thread. `ids` empty: every open
/// thread `by` began that hasn't been sent to an agent.
pub async fn send_many(st: &HeadState, realm: &str, doc: &str, ids: &[String], by: &str) -> anyhow::Result<Vec<DocThread>> {
    let all = list(st, realm, doc).await?;
    let chosen: Vec<DocThread> =
        all.into_iter().filter(|t| t.state == "open").filter(|t| if ids.is_empty() { t.unsent } else { ids.contains(&t.id) }).collect();
    if chosen.is_empty() {
        return Err(bad_request("no open comment threads to send"));
    }
    to_agents(st, realm, doc, &chosen).await?;
    let _ = st.docs.live.send(c6_api::docs::DocChange {
        id: doc.to_owned(),
        realm: realm.to_owned(),
        rev: 0,
        by: by.to_owned(),
        what: "commented".into(),
    });
    st.attention.changed();
    let threads: Vec<String> = chosen.iter().map(|t| t.id.clone()).collect();
    let now = list(st, realm, doc).await?;
    Ok(now.into_iter().filter(|t| threads.contains(&t.id)).collect())
}

/// Picks each thread's session (all before any is sent, so one with nowhere
/// to go sends none), then delivers one message per session.
async fn to_agents(st: &HeadState, realm: &str, doc: &str, threads: &[DocThread]) -> anyhow::Result<()> {
    let mut groups: Vec<(String, Vec<&DocThread>)> = Vec::new();
    for t in threads {
        let session = pick(st, realm, doc, t).await?;
        match groups.iter_mut().find(|(s, _)| *s == session) {
            Some((_, g)) => g.push(t),
            None => groups.push((session, vec![t])),
        }
    }
    let title = super::title_of(st, realm, doc).await.map(|t| t.0).unwrap_or_default();
    for (session, group) in groups {
        let mut parts = Vec::new();
        for t in &group {
            parts.push(told(st, realm, doc, &title, t).await);
        }
        let text = if parts.len() == 1 {
            parts.remove(0)
        } else {
            format!(
                "{} comments on the document \"{title}\" (doc:{doc}) come to you together. Take each in turn and answer it in its own thread, as it says.\n\n{}",
                parts.len(),
                parts.join("\n\n---\n\n")
            )
        };
        let ids: Vec<String> = group.iter().map(|t| t.id.clone()).collect();
        deliver(st, realm, &ids, &session, &text).await?;
    }
    Ok(())
}

/// What an agent is told about one thread (the prompt `docs.comment`).
async fn told(st: &HeadState, realm: &str, doc: &str, title: &str, t: &DocThread) -> String {
    let comments: String = t.comments.iter().map(|c| format!("- {}: {}\n", who(&c.by), c.text)).collect();
    let template = crate::prompts::get(st, "docs.comment", realm).await;
    let anchor = if t.anchor.block.is_empty() { t.about.clone() } else { format!("{} (block {})", t.about, t.anchor.block) };
    c6_api::prompts::fill(
        &template,
        &[("title", title), ("doc", doc), ("thread", &t.id), ("anchor", &anchor), ("comments", comments.trim_end())],
    )
}

/// Sends `text` to the session (queued if it's busy) and notes the threads
/// as with it.
async fn deliver(st: &HeadState, realm: &str, threads: &[String], session: &str, text: &str) -> anyhow::Result<()> {
    let m: c6_api::SendMessage = serde_json::from_value(json!({"text": text}))?;
    crate::sessions::send_input(st, session, &m).await?;
    let pool = super::pool(st, realm).await?;
    for thread in threads {
        sqlx::query("UPDATE doc_threads SET to_agent = 1, agent = ? WHERE id = ?").bind(session).bind(thread).execute(&pool).await?;
    }
    note_sent(st, realm, &pool, threads).await
}

/// The `doc` tool's `comment` action: create (block, quote, text), reply
/// (thread, text), list, or resolve (only threads the session began).
pub async fn for_session(st: &HeadState, realm: &str, doc: &str, p: &Value, by: &str) -> anyhow::Result<Value> {
    match p["op"].as_str().unwrap_or("list") {
        "list" => Ok(json!({"threads": list(st, realm, doc).await?})),
        "create" => {
            let n = ThreadNew {
                anchor: Anchor {
                    block: p["block"].as_str().unwrap_or("").to_owned(),
                    quote: p["quote"].as_str().unwrap_or("").to_owned(),
                    tab: p["tab"].as_str().unwrap_or("").to_owned(),
                },
                text: p["text"].as_str().unwrap_or("").to_owned(),
                to: String::new(),
            };
            Ok(json!({"thread": create(st, realm, doc, &n, by).await?}))
        }
        "reply" => {
            let thread = p["thread"].as_str().ok_or_else(|| bad_request("reply takes thread and text"))?;
            let c = CommentNew { text: p["text"].as_str().unwrap_or("").to_owned(), to: String::new() };
            Ok(json!({"thread": reply(st, realm, doc, thread, &c, by).await?}))
        }
        "resolve" => {
            let thread = p["thread"].as_str().ok_or_else(|| bad_request("resolve takes thread"))?;
            Ok(json!({"thread": resolve(st, realm, doc, thread, by).await?}))
        }
        other => Err(bad_request(format!("comment's ops are list, create, reply and resolve, not {other:?}"))),
    }
}
