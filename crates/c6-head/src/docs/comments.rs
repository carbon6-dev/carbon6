//! Comments (docs/docs.md §8): threads anchored to quoted words, a block or
//! slide, or a whole tab; one can be sent to an agent instead of just left.
//! It goes to the session that last wrote that part if that session is
//! idle, else to a new session started from the document's work or project
//! (its latest session), told what to do by the editable prompt
//! `docs.comment`. The agent answers in the thread, and links the edits it
//! made for it (`answering`). A thread an agent answered needs you until
//! you reply or resolve it; only you resolve a thread you wrote.

use c6_api::docs::{Anchor, CommentNew, DocComment, DocThread, ThreadNew};
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row as _;

use crate::{HeadState, bad_request, forbidden, not_found};

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

/// A document's threads as they are now (open first), with whether each is
/// detached and needs you.
pub async fn list(st: &HeadState, realm: &str, doc: &str) -> anyhow::Result<Vec<DocThread>> {
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
        let comments: Vec<DocComment> = sqlx::query("SELECT * FROM doc_comments WHERE thread_id = ? ORDER BY at, id")
            .bind(&id)
            .fetch_all(&pool)
            .await?
            .iter()
            .map(|c| DocComment { id: c.get("id"), by: c.get("by"), text: c.get("text"), at: c.get("at") })
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
        out.push(DocThread {
            id,
            doc: doc.to_owned(),
            anchor,
            state: thread_state,
            detached,
            about: r.get("about"),
            to_agent: r.get::<i64, _>("to_agent") != 0,
            agent: r.get("agent"),
            comments,
            edits,
            needs_you,
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
        send(st, realm, doc, &id, by).await?;
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
        send(st, realm, doc, thread, by).await?;
    }
    one(st, realm, doc, thread).await
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
    let title = super::title_of(st, realm, doc).await.map(|t| t.0).unwrap_or_default();
    let event = json!({"type": "doc", "op": "commented", "doc": {"id": doc, "title": title, "realm": realm}, "thread": thread, "by": by, "text": text});
    crate::triggers::dispatch(st, realm, "doc", "commented", event).await;
    moved(st, realm, doc).await;
}

async fn moved(st: &HeadState, realm: &str, doc: &str) {
    crate::work::moved_ref(st, &format!("doc:{doc}")).await;
    for p in crate::projects::holding_doc(st, realm, doc).await {
        let _ = crate::projects::touch(st, &p, now_ms()).await;
    }
}

/// A session to ask, and whether to start it: the one that last wrote the
/// thread's part if it's idle, else the latest session of the document's
/// work or project (a new session like it, in that work).
async fn pick(st: &HeadState, realm: &str, doc: &str, anchor: &Anchor) -> anyhow::Result<(String, bool, Option<String>)> {
    let pool = super::pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    let (state, _) = super::load_head(&mut conn, doc).await?;
    drop(conn);
    // Who wrote that part last (the slide's or block's, else the document's).
    let wrote =
        state.rows.get(&anchor.block).map(|r| r.by.clone()).or_else(|| state.rows.values().max_by_key(|r| r.rev).map(|r| r.by.clone()));
    if let Some(s) = wrote.as_deref().and_then(|b| b.strip_prefix("session:"))
        && let Some(row) = st.store.session(s).await?
        && row.archived == 0
        && matches!(row.status.as_str(), "idle")
        && row.awaiting == 0
    {
        return Ok((row.id, false, None));
    }
    // The document's work (and its project's work): their latest session.
    let reference = format!("doc:{doc}");
    let mut work: Vec<String> = crate::work::holding(st, &reference).await?;
    for p in crate::projects::holding_doc(st, realm, doc).await {
        work.extend(crate::work::in_project(st, &p).await?);
    }
    let mut best: Option<(crate::store::SessionRow, String)> = None;
    for w in &work {
        for r in crate::work::refs(st, w).await {
            if let Some(sid) = r.strip_prefix("session:")
                && let Some(s) = st.store.session(sid).await?
                && best.as_ref().is_none_or(|(b, _)| b.updated_at < s.updated_at)
            {
                best = Some((s, w.clone()));
            }
        }
    }
    match best {
        Some((s, w)) => Ok((s.id, true, Some(w))),
        None => Err(bad_request(
            "no session to send it to: the document's work and project have none yet. Start a session on it (or put it in work), then send the thread",
        )),
    }
}

/// Sends a thread to an agent, by `docs.comment`.
async fn send(st: &HeadState, realm: &str, doc: &str, thread: &str, _by: &str) -> anyhow::Result<()> {
    let t = one(st, realm, doc, thread).await?;
    let (session, start, work) = pick(st, realm, doc, &t.anchor).await?;
    let title = super::title_of(st, realm, doc).await.map(|t| t.0).unwrap_or_default();
    let comments: String = t.comments.iter().map(|c| format!("- {}: {}\n", who(&c.by), c.text)).collect();
    let template = crate::prompts::get(st, "docs.comment", realm).await;
    let anchor = if t.anchor.block.is_empty() { t.about.clone() } else { format!("{} (block {})", t.about, t.anchor.block) };
    let text = c6_api::prompts::fill(
        &template,
        &[("title", &title), ("doc", doc), ("thread", thread), ("anchor", &anchor), ("comments", comments.trim_end())],
    );
    let sent_to = if start {
        let seed = st.store.session(&session).await?.ok_or_else(|| not_found("its session is gone"))?;
        let labels: Vec<String> = serde_json::from_str(&seed.labels).unwrap_or_default();
        let first = crate::projects::with_context(st, work.as_deref(), &text).await.unwrap_or_else(|| text.clone());
        let req: c6_api::CreateSession = serde_json::from_value(json!({
            "worker_id": seed.worker_id, "cwd": seed.cwd, "connection_id": seed.connection_id, "model": seed.model,
            "title": format!("Comment on {title}"), "prompt": first, "labels": labels, "realm": realm,
        }))?;
        let s = crate::sessions::create(st, req).await?;
        crate::work::on_new_session(st, &s, work.as_deref(), &[], &text, None).await;
        s.id
    } else {
        let m: c6_api::SendMessage = serde_json::from_value(json!({"text": text}))?;
        crate::sessions::send_input(st, &session, &m).await?;
        session
    };
    let pool = super::pool(st, realm).await?;
    sqlx::query("UPDATE doc_threads SET to_agent = 1, agent = ? WHERE id = ?").bind(&sent_to).bind(thread).execute(&pool).await?;
    Ok(())
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
