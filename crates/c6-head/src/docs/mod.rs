//! Documents (docs/docs.md): docs and decks, first-class objects of each
//! realm, kept in the realm's knowledge database. A document is tabs of
//! blocks with stable ids; every change, yours or an agent's, is a batch
//! of operations applied atomically, guarded by the version and hash it
//! read, and kept as a version with who made it. Every 50 versions a
//! snapshot of the whole document is kept, so any version opens quickly.
//!
//! Each change goes out on the live feed at once; once a document's
//! changes settle (nothing new for a few seconds), `doc.changed` fires for
//! triggers, with who wrote it.

pub mod archive;
pub mod bring;
pub mod comments;
pub mod export;
pub mod files;
pub mod google;
pub mod http;
pub mod markdown;
pub mod model;
pub mod session;

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use c6_api::docs::{
    self as d, Doc, DocApplied, DocBatch, DocChange, DocNew, DocPatch, DocQuery, DocRead, DocRestore, DocSummary, DocTab, DocVersion,
};
use c6_core::now_ms;
use serde_json::{Map, Value, json};
use sqlx::Row as _;
use tracing::warn;

use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};
use model::{Meta, Row, State, Tab};

/// A snapshot every this many versions.
const SNAPSHOT_EVERY: i64 = 50;
/// A document's changes have settled after this long without one.
const SETTLE_MS: u64 = 3000;

type WriteLocks = HashMap<(String, String), std::sync::Weak<tokio::sync::Mutex<()>>>;

/// Documents' changes as they happen, and those not yet settled.
pub struct Docs {
    pub live: tokio::sync::broadcast::Sender<DocChange>,
    settling: Mutex<HashMap<(String, String), Settling>>,
    /// Covers mutations and mirror enqueueing, including external exports.
    writes: Mutex<WriteLocks>,
}

impl Default for Docs {
    fn default() -> Self {
        Self { live: tokio::sync::broadcast::channel(256).0, settling: Default::default(), writes: Default::default() }
    }
}

struct Settling {
    authors: BTreeSet<String>,
    from_rev: i64,
    rev: i64,
    last: std::time::Instant,
}

/// Who a caller writes as: you (in the browser, or with the console's
/// token), the console's assistant (with your approval), else the realm's
/// token.
pub fn by_of(caller: &Caller) -> String {
    if let Some(a) = crate::agent_ops::actor() {
        return a;
    }
    match (&caller.realm, caller.browser) {
        _ if caller.console => crate::assistant::BY_CONSOLE.into(),
        (Some(r), false) => format!("token:{r}"),
        _ => "you".into(),
    }
}

/// The realm a request is for: `?realm=` (you), the caller's, else home.
pub async fn realm_for(st: &HeadState, caller: &Caller, asked: Option<&str>) -> anyhow::Result<String> {
    let realm = caller.realm_for_new(asked)?;
    if realm == crate::realms::ROOT {
        return Err(bad_request("documents live in a realm: root keeps none"));
    }
    if !crate::realms::exists(st, &realm).await? {
        return Err(bad_request(format!("no realm {realm:?}")));
    }
    Ok(realm)
}

/// The realm's knowledge database.
pub(crate) async fn pool(st: &HeadState, realm: &str) -> anyhow::Result<sqlx::SqlitePool> {
    if !crate::memex::has_memex(realm) {
        return Err(bad_request("documents live in a realm: root keeps none"));
    }
    let m = crate::knowledge::get(st).ok_or_else(|| anyhow::anyhow!("the knowledge store is not open"))?;
    Ok(m.of_realm(realm).await?.store.pool.clone())
}

// --- Loading and storing --------------------------------------------------------

struct Head {
    state: State,
    rev: i64,
    labels: Vec<String>,
    created_by: String,
    created_at: i64,
    updated_by: String,
    updated_at: i64,
    archived_at: Option<i64>,
}

/// A document as stored, and its version.
pub(crate) async fn load_head(conn: &mut sqlx::SqliteConnection, id: &str) -> anyhow::Result<(State, i64)> {
    let h = load(conn, id).await?;
    Ok((h.state, h.rev))
}

async fn load(conn: &mut sqlx::SqliteConnection, id: &str) -> anyhow::Result<Head> {
    let r = sqlx::query("SELECT * FROM docs WHERE id = ? AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| not_found(format!("no document {id}")))?;
    let mut state = State::new(Meta { kind: r.get("kind"), title: r.get("title"), theme: r.get("theme"), size: r.get("size") });
    state.clock = r.get("clock");
    for t in sqlx::query("SELECT id, title, pos FROM doc_tabs WHERE doc_id = ?").bind(id).fetch_all(&mut *conn).await? {
        state.tabs.push(Tab { id: t.get("id"), title: t.get("title"), pos: t.get("pos") });
    }
    for b in sqlx::query("SELECT * FROM doc_blocks WHERE doc_id = ?").bind(id).fetch_all(&mut *conn).await? {
        let row = Row {
            id: b.get("id"),
            tab: b.get("tab"),
            parent: b.get("parent"),
            region: b.get("region"),
            pos: b.get("pos"),
            kind: b.get("kind"),
            text: b.get("text"),
            attrs: serde_json::from_str(&b.get::<String, _>("attrs")).unwrap_or(json!({})),
            by: b.get("by"),
            rev: b.get("rev"),
        };
        state.rows.insert(row.id.clone(), row);
    }
    Ok(Head {
        state,
        rev: r.get("rev"),
        labels: serde_json::from_str(&r.get::<String, _>("labels")).unwrap_or_default(),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        updated_by: r.get("updated_by"),
        updated_at: r.get("updated_at"),
        archived_at: r.get("archived_at"),
    })
}

/// Writes what changed from `before` to `after` as version `rev`.
#[allow(clippy::too_many_arguments)]
async fn store(
    conn: &mut sqlx::SqliteConnection,
    id: &str,
    before: &State,
    after: &State,
    rev: i64,
    ops: &Value,
    by: &str,
    turn: Option<i64>,
    answering: &str,
) -> anyhow::Result<Value> {
    let changes = model::changes(before, after);
    let now = now_ms();
    for (bid, v) in changes["blocks"].as_object().into_iter().flatten() {
        match after.rows.get(bid) {
            Some(r) => {
                sqlx::query(
                    "INSERT OR REPLACE INTO doc_blocks (doc_id, id, tab, parent, region, pos, kind, text, attrs, by, rev) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(id)
                .bind(&r.id)
                .bind(&r.tab)
                .bind(&r.parent)
                .bind(&r.region)
                .bind(r.pos)
                .bind(&r.kind)
                .bind(&r.text)
                .bind(if r.attrs.is_null() { "{}".to_owned() } else { r.attrs.to_string() })
                .bind(&r.by)
                .bind(r.rev)
                .execute(&mut *conn)
                .await?;
            }
            None => {
                debug_assert!(v.is_null());
                sqlx::query("DELETE FROM doc_blocks WHERE doc_id = ? AND id = ?").bind(id).bind(bid).execute(&mut *conn).await?;
            }
        }
    }
    for (tid, _) in changes["tabs"].as_object().into_iter().flatten() {
        match after.tab(tid) {
            Some(t) => {
                sqlx::query("INSERT OR REPLACE INTO doc_tabs (doc_id, id, title, pos) VALUES (?, ?, ?, ?)")
                    .bind(id)
                    .bind(&t.id)
                    .bind(&t.title)
                    .bind(t.pos)
                    .execute(&mut *conn)
                    .await?;
            }
            None => {
                sqlx::query("DELETE FROM doc_tabs WHERE doc_id = ? AND id = ?").bind(id).bind(tid).execute(&mut *conn).await?;
            }
        }
    }
    sqlx::query("UPDATE docs SET title = ?, theme = ?, size = ?, rev = ?, clock = ?, updated_by = ?, updated_at = ? WHERE id = ?")
        .bind(&after.meta.title)
        .bind(&after.meta.theme)
        .bind(&after.meta.size)
        .bind(rev)
        .bind(after.clock)
        .bind(by)
        .bind(now)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO doc_ops (doc_id, rev, by, turn, at, ops, changes, answering) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id)
        .bind(rev)
        .bind(by)
        .bind(turn)
        .bind(now)
        .bind(ops.to_string())
        .bind(changes.to_string())
        .bind(answering)
        .execute(&mut *conn)
        .await?;
    if rev == 1 || rev % SNAPSHOT_EVERY == 0 {
        sqlx::query("INSERT OR REPLACE INTO doc_snapshots (doc_id, rev, state) VALUES (?, ?, ?)")
            .bind(id)
            .bind(rev)
            .bind(serde_json::to_string(after)?)
            .execute(&mut *conn)
            .await?;
    }
    Ok(changes)
}

/// The document as it was at version `rev`: the snapshot before it, then
/// the versions after the snapshot.
async fn state_at(pool: &sqlx::SqlitePool, id: &str, rev: i64) -> anyhow::Result<State> {
    let snap = sqlx::query("SELECT rev, state FROM doc_snapshots WHERE doc_id = ? AND rev <= ? ORDER BY rev DESC LIMIT 1")
        .bind(id)
        .bind(rev)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| not_found(format!("document {id} has no version {rev}")))?;
    let mut state: State = serde_json::from_str(&snap.get::<String, _>("state"))?;
    let from: i64 = snap.get("rev");
    for c in sqlx::query("SELECT changes FROM doc_ops WHERE doc_id = ? AND rev > ? AND rev <= ? ORDER BY rev")
        .bind(id)
        .bind(from)
        .bind(rev)
        .fetch_all(pool)
        .await?
    {
        model::replay(&mut state, &serde_json::from_str(&c.get::<String, _>("changes"))?);
    }
    Ok(state)
}

fn summary_of(realm: &str, id: &str, h: &Head) -> DocSummary {
    let pending: Vec<&Row> = h.state.rows.values().filter(|r| r.kind == "pending").collect();
    let mut writers: Vec<String> = pending.iter().filter_map(|r| r.attrs["by"].as_str().map(str::to_owned)).collect();
    writers.sort();
    writers.dedup();
    DocSummary {
        id: id.to_owned(),
        realm: realm.to_owned(),
        kind: h.state.meta.kind.clone(),
        title: h.state.meta.title.clone(),
        theme: h.state.meta.theme.clone(),
        size: h.state.meta.size.clone(),
        labels: h.labels.clone(),
        rev: h.rev,
        created_by: h.created_by.clone(),
        created_at: h.created_at,
        updated_by: h.updated_by.clone(),
        updated_at: h.updated_at,
        archived_at: h.archived_at,
        pending: pending.len() as u32,
        writers,
    }
}

fn doc_of(realm: &str, id: &str, h: &Head) -> Doc {
    Doc {
        summary: summary_of(realm, id, h),
        tabs: h
            .state
            .tabs_in_order()
            .into_iter()
            .map(|t| DocTab { id: t.id.clone(), title: t.title.clone(), blocks: h.state.blocks_of(&t.id) })
            .collect(),
    }
}

// --- What you (and agents) do ---------------------------------------------------

pub async fn create(st: &HeadState, realm: &str, n: &DocNew, by: &str) -> anyhow::Result<(Doc, DocApplied)> {
    let kind = if n.kind.trim().is_empty() { "doc".to_owned() } else { n.kind.trim().to_owned() };
    if !d::KINDS.contains(&kind.as_str()) {
        return Err(bad_request("a document is a doc or a deck"));
    }
    let title = model::check_title(&n.title)?;
    let labels = crate::sessions::check_labels(&n.labels)?;
    let size = match n.size.trim() {
        "" if kind == "deck" => "16:9".to_owned(),
        "" => String::new(),
        s @ ("16:9" | "4:3") if kind == "deck" => s.to_owned(),
        _ => return Err(bad_request("a deck is 16:9 or 4:3; a doc has no size")),
    };
    if kind == "doc" && !n.theme.trim().is_empty() {
        return Err(bad_request("a theme is a deck's"));
    }
    let theme = if kind == "deck" && n.theme.trim().is_empty() { "Paper".to_owned() } else { n.theme.trim().to_owned() };
    let mut state = State::new(Meta { kind: kind.clone(), title: title.clone(), theme, size });
    let empty = state.clone();
    let tab_title = if n.tab.trim().is_empty() { title.clone() } else { model::check_title(&n.tab)? };
    state.first_tab(&tab_title, by);
    let applied = if n.markdown.trim().is_empty() {
        model::Applied::default()
    } else {
        let op = d::DocOp::Insert {
            markdown: n.markdown.clone(),
            blocks: n.blocks.clone(),
            before: None,
            after: None,
            section: None,
            slide: None,
            region: None,
            tab: None,
        };
        state.apply(&[op], by, 1)?
    };
    let id = uuid::Uuid::now_v7().to_string();
    let pool = pool(st, realm).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let now = now_ms();
    sqlx::query(
        "INSERT INTO docs (id, kind, title, theme, size, labels, rev, clock, created_by, created_at, updated_by, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 1, 0, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&kind)
    .bind(&title)
    .bind(&state.meta.theme)
    .bind(&state.meta.size)
    .bind(serde_json::to_string(&labels)?)
    .bind(by)
    .bind(now)
    .bind(by)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    store(&mut tx, &id, &empty, &state, 1, &json!([{"op": "create", "kind": kind, "title": title}]), by, None, "").await?;
    let h = load(&mut tx, &id).await?;
    tx.commit().await?;
    let doc = doc_of(realm, &id, &h);
    let hashes = h.state.rows.values().map(|r| (r.id.clone(), json!(h.state.hash(r)))).collect();
    let applied = DocApplied { id: id.clone(), rev: 1, ids: applied.ids, keys: applied.keys, hashes };
    changed(st, realm, &id, 1, by, "created");
    let event = json!({"type": "doc", "op": "created", "doc": event_doc(&doc.summary), "by": by});
    crate::triggers::dispatch(st, realm, "doc", "created", event).await;
    Ok((doc, applied))
}

fn event_doc(s: &DocSummary) -> Value {
    json!({"id": s.id, "kind": s.kind, "title": s.title, "realm": s.realm, "rev": s.rev, "labels": s.labels})
}

pub async fn list(st: &HeadState, realm: &str, q: &DocQuery) -> anyhow::Result<Vec<DocSummary>> {
    let pool = pool(st, realm).await?;
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let rows = sqlx::query(
        "SELECT id FROM docs WHERE deleted_at IS NULL AND (?1 IS NULL OR kind = ?1) \
         AND (?2 IS NULL OR EXISTS (SELECT 1 FROM json_each(docs.labels) WHERE value = ?2)) \
         AND (?3 IS NULL OR EXISTS (SELECT 1 FROM doc_ops o WHERE o.doc_id = docs.id AND (o.by = ?3 OR (?3 = 'session' AND o.by LIKE 'session:%')))) \
         AND (?4 IS NULL OR title LIKE '%' || ?4 || '%') AND (?6 OR archived_at IS NULL) ORDER BY updated_at DESC LIMIT ?5",
    )
    .bind(q.kind.as_deref().filter(|k| !k.is_empty()))
    .bind(q.label.as_deref().filter(|k| !k.is_empty()))
    .bind(q.by.as_deref().filter(|k| !k.is_empty()))
    .bind(q.q.as_deref().map(str::trim).filter(|k| !k.is_empty()))
    .bind(limit)
    .bind(q.archived)
    .fetch_all(&pool)
    .await?;
    let mut out = Vec::new();
    let mut conn = pool.acquire().await?;
    for r in rows {
        let id: String = r.get("id");
        let h = load(&mut conn, &id).await?;
        // An archive may have committed since the ids were selected.
        if q.archived || h.archived_at.is_none() {
            out.push(summary_of(realm, &id, &h));
        }
    }
    Ok(out)
}

pub async fn get(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<Doc> {
    let pool = pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    let h = load(&mut conn, id).await?;
    Ok(doc_of(realm, id, &h))
}

/// Serializes writes and mirror enqueueing for a document. Weak entries do
/// not keep locks alive after their last user finishes.
async fn write_lock(st: &HeadState, realm: &str, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut writes = st.docs.writes.lock().unwrap();
        writes.retain(|_, lock| lock.strong_count() != 0);
        let entry = writes.entry((realm.to_owned(), id.to_owned())).or_default();
        let lock = entry.upgrade().unwrap_or_else(|| std::sync::Arc::new(tokio::sync::Mutex::new(())));
        *entry = std::sync::Arc::downgrade(&lock);
        lock
    };
    lock.lock_owned().await
}

fn check_writable(archived_at: Option<i64>) -> anyhow::Result<()> {
    if archived_at.is_some() {
        return Err(crate::conflict("This document is archived. Unarchive it first to make changes."));
    }
    Ok(())
}

/// Refuses writes to archived documents; reads and existing links still work.
pub async fn ensure_writable(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<()> {
    let pool = pool(st, realm).await?;
    let archived_at: Option<i64> = sqlx::query_scalar("SELECT archived_at FROM docs WHERE id = ? AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&pool)
        .await?
        .ok_or_else(|| not_found(format!("no document {id}")))?;
    check_writable(archived_at)
}

/// Holds the document's write lock until the entire mutation (including
/// external side effects) finishes. Versioned mutations also check in their
/// transaction, so another database writer cannot slip past the archive flag.
async fn writable(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<tokio::sync::OwnedMutexGuard<()>> {
    let guard = write_lock(st, realm, id).await;
    ensure_writable(st, realm, id).await?;
    Ok(guard)
}

/// An archive timestamp for refs without loading the document's content.
pub async fn archived_at(st: &HeadState, realm: &str, id: &str) -> Option<i64> {
    let pool = pool(st, realm).await.ok()?;
    sqlx::query_scalar("SELECT archived_at FROM docs WHERE id = ? AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

/// A document's title, kind and when it last changed (for refs).
pub async fn title_of(st: &HeadState, realm: &str, id: &str) -> Option<(String, String, i64)> {
    let pool = pool(st, realm).await.ok()?;
    sqlx::query("SELECT title, kind, updated_at FROM docs WHERE id = ? AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
        .map(|r| (r.get("title"), r.get("kind"), r.get("updated_at")))
}

pub async fn patch(st: &HeadState, realm: &str, id: &str, p: &DocPatch, by: &str) -> anyhow::Result<Doc> {
    Ok(patch_inner(st, realm, id, p, by).await?.0)
}

/// Changes archive state without changing document content or history.
/// Returns whether the state actually changed (repeated calls are harmless).
pub async fn set_archived(st: &HeadState, realm: &str, id: &str, archived: bool) -> anyhow::Result<bool> {
    let by = crate::agent_ops::actor().unwrap_or_else(|| "you".to_owned());
    Ok(patch_inner(st, realm, id, &DocPatch { archived: Some(archived), ..Default::default() }, &by).await?.1)
}

async fn patch_inner(st: &HeadState, realm: &str, id: &str, p: &DocPatch, by: &str) -> anyhow::Result<(Doc, bool)> {
    let _guard = write_lock(st, realm, id).await;
    let pool = pool(st, realm).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let h = load(&mut tx, id).await?;
    if p.archived.is_none() || p.title.is_some() || p.labels.is_some() {
        check_writable(h.archived_at)?;
    }
    let mut after = h.state.clone();
    let mut changed_rev = None;
    if let Some(t) = &p.title {
        let op = d::DocOp::Set {
            title: Some(t.clone()),
            theme: None,
            size: None,
            tab: None,
            tab_title: None,
            tabs: None,
            block: None,
            chip: None,
            value: None,
        };
        let rev = h.rev + 1;
        after.apply(std::slice::from_ref(&op), by, rev)?;
        if !model::is_empty(&model::changes(&h.state, &after)) {
            store(&mut tx, id, &h.state, &after, rev, &json!([op]), by, None, "").await?;
            changed_rev = Some(rev);
        }
    }
    if let Some(l) = &p.labels {
        let labels = crate::sessions::check_labels(l)?;
        sqlx::query("UPDATE docs SET labels = ? WHERE id = ? AND deleted_at IS NULL AND archived_at IS NULL")
            .bind(serde_json::to_string(&labels)?)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let archive_changed = p.archived.is_some_and(|a| a != h.archived_at.is_some());
    if archive_changed {
        sqlx::query("UPDATE docs SET archived_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(if p.archived == Some(true) { Some(now_ms()) } else { None })
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let doc = doc_of(realm, id, &load(&mut tx, id).await?);
    tx.commit().await?;
    if let Some(rev) = changed_rev {
        changed(st, realm, id, rev, by, "changed");
    } else if p.labels.is_some() || archive_changed {
        changed(st, realm, id, 0, by, "changed");
    }
    if archive_changed {
        st.attention.changed();
        if doc.summary.archived_at.is_some() {
            unmirror(st, realm, &doc).await;
        } else {
            mirror_locked(st, realm, &doc).await;
        }
        crate::work::moved_ref(st, &format!("doc:{id}")).await;
        for p in crate::projects::holding_doc(st, realm, id).await {
            crate::projects::refresh(st, &p);
        }
    }
    Ok((doc, archive_changed))
}

/// Deletes a document (its history goes with it from view).
pub async fn delete(st: &HeadState, realm: &str, id: &str, by: &str) -> anyhow::Result<()> {
    let doc = get(st, realm, id).await?;
    let pool = pool(st, realm).await?;
    let n = sqlx::query("UPDATE docs SET deleted_at = ?, updated_by = ? WHERE id = ? AND deleted_at IS NULL")
        .bind(now_ms())
        .bind(by)
        .bind(id)
        .execute(&pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(not_found(format!("no document {id}")));
    }
    changed(st, realm, id, 0, by, "deleted");
    unmirror(st, realm, &doc).await;
    Ok(())
}

/// Applies a batch of operations as `by` (for a session, in `turn`).
pub async fn apply(st: &HeadState, realm: &str, id: &str, batch: &DocBatch, by: &str, turn: Option<i64>) -> anyhow::Result<DocApplied> {
    let _guard = writable(st, realm, id).await?;
    if batch.ops.is_empty() {
        return Err(bad_request("a batch needs at least one operation"));
    }
    let pool = pool(st, realm).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let h = load(&mut tx, id).await?;
    check_writable(h.archived_at)?;
    let rev = h.rev + 1;
    let mut after = h.state.clone();
    let applied = after.apply(&batch.ops, by, rev)?;
    let ops = serde_json::to_value(&batch.ops)?;
    let changes = model::changes(&h.state, &after);
    if model::is_empty(&changes) {
        return Ok(DocApplied { id: id.to_owned(), rev: h.rev, ids: applied.ids, keys: applied.keys, hashes: Map::new() });
    }
    let changes = store(&mut tx, id, &h.state, &after, rev, &ops, by, turn, batch.answering.trim()).await?;
    tx.commit().await?;
    let hashes = touched(&after, &changes);
    changed(st, realm, id, rev, by, "changed");
    Ok(DocApplied { id: id.to_owned(), rev, ids: applied.ids, keys: applied.keys, hashes })
}

/// The new hashes of what changed (and the slides holding it).
fn touched(state: &State, changes: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for bid in changes["blocks"].as_object().into_iter().flatten().map(|(k, _)| k) {
        if let Some(r) = state.rows.get(bid) {
            out.insert(bid.clone(), json!(state.hash(r)));
            if let Some(p) = state.rows.get(&r.parent) {
                out.insert(p.id.clone(), json!(state.hash(p)));
            }
        }
    }
    out
}

/// A version, or one block or slide of it, back as a new change.
pub async fn restore(st: &HeadState, realm: &str, id: &str, r: &DocRestore, by: &str) -> anyhow::Result<DocApplied> {
    let _guard = writable(st, realm, id).await?;
    let pool = pool(st, realm).await?;
    let old = state_at(&pool, id, r.rev).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let h = load(&mut tx, id).await?;
    check_writable(h.archived_at)?;
    if r.rev > h.rev {
        return Err(bad_request(format!("document {id} has no version {}", r.rev)));
    }
    let rev = h.rev + 1;
    let mut after = h.state.clone();
    let stamp = |row: &Row| Row { by: by.to_owned(), rev, ..row.clone() };
    match &r.block {
        Some(b) => {
            let was = old.rows.get(b).ok_or_else(|| not_found(format!("block {b} isn't in version {}", r.rev)))?;
            let mut row = stamp(was);
            if after.tab(&row.tab).is_none() {
                row.tab = after.tabs_in_order().first().map(|t| t.id.clone()).unwrap_or_default();
            }
            if !row.parent.is_empty() && !after.rows.contains_key(&row.parent) {
                return Err(bad_request(format!("block {b} was on a slide that is gone: restore the slide")));
            }
            after.rows.retain(|_, x| x.parent != *b);
            for kid in old.rows.values().filter(|x| x.parent == *b) {
                after.rows.insert(kid.id.clone(), Row { tab: row.tab.clone(), ..stamp(kid) });
            }
            after.rows.insert(b.clone(), row);
        }
        None => {
            let clock = after.clock;
            after = old.clone();
            after.clock = clock;
            for row in after.rows.values_mut() {
                if h.state.rows.get(&row.id).is_none_or(|now| now.text != row.text || now.attrs != row.attrs || now.kind != row.kind) {
                    row.by = by.to_owned();
                    row.rev = rev;
                }
            }
        }
    }
    let changes = model::changes(&h.state, &after);
    if model::is_empty(&changes) {
        return Ok(DocApplied { id: id.to_owned(), rev: h.rev, ..Default::default() });
    }
    let ops = json!([{"op": "restore", "rev": r.rev, "block": r.block}]);
    let changes = store(&mut tx, id, &h.state, &after, rev, &ops, by, None, "").await?;
    tx.commit().await?;
    changed(st, realm, id, rev, by, "changed");
    Ok(DocApplied { id: id.to_owned(), rev, hashes: touched(&after, &changes), ..Default::default() })
}

// --- Reading ------------------------------------------------------------------

fn outline_of(state: &State, rev: i64, tab: Option<&str>) -> Value {
    let tabs: Vec<Value> = state
        .tabs_in_order()
        .into_iter()
        .filter(|t| tab.is_none_or(|x| x == t.id))
        .map(|t| {
            let blocks: Vec<Value> = state
                .siblings(&t.id, "", "")
                .into_iter()
                .map(|r| {
                    let mut b = json!({"id": r.id, "kind": r.kind, "hash": state.hash(r), "by": r.by});
                    match r.kind.as_str() {
                        "heading" => {
                            b["level"] = json!(markdown::level(&r.text));
                            b["text"] = json!(markdown::first_words("heading", &r.text, 120));
                        }
                        "pending" => {
                            b["intent"] = r.attrs["intent"].clone();
                            b["writer"] = r.attrs["by"].clone();
                        }
                        "slide" => {
                            let kids = state.children(r);
                            let title = kids.iter().find(|k| k.region == "title").or(kids.first());
                            b["layout"] = r.attrs["layout"].clone();
                            b["title"] = json!(title.map(|k| markdown::first_words(&k.kind, &k.text, 80)).unwrap_or_default());
                            b["blocks"] = json!(
                                kids.iter()
                                    .map(|k| json!({"id": k.id, "kind": k.kind, "region": k.region, "hash": state.hash(k),
                                                    "words": markdown::first_words(&k.kind, &k.text, 60)}))
                                    .collect::<Vec<_>>()
                            );
                            if r.attrs["hidden"] == true {
                                b["hidden"] = json!(true);
                            }
                            b["notes"] = json!(r.attrs["notes"].as_str().is_some_and(|n| !n.is_empty()));
                        }
                        "embed" => b["type"] = r.attrs["type"].clone(),
                        _ => b["words"] = json!(markdown::first_words(&r.kind, &r.text, 60)),
                    }
                    b
                })
                .collect();
            json!({"id": t.id, "title": t.title, "blocks": blocks})
        })
        .collect();
    json!({"rev": rev, "kind": state.meta.kind, "title": state.meta.title, "theme": state.meta.theme, "tabs": tabs})
}

fn block_view(state: &State, r: &Row) -> Value {
    let mut blocks = Map::new();
    let md = state.markdown_of(r, &mut blocks);
    json!({"id": r.id, "kind": r.kind, "hash": state.hash(r), "rev": r.rev, "by": r.by, "tab": r.tab,
           "slide": (!r.parent.is_empty()).then_some(&r.parent), "region": (!r.region.is_empty()).then_some(&r.region),
           "markdown": md, "blocks": blocks})
}

/// Small reads (docs/docs.md §7): an outline, one block or slide, a search
/// inside the document, what changed since a version, or the Markdown.
pub async fn read(st: &HeadState, realm: &str, id: &str, q: &DocRead) -> anyhow::Result<Value> {
    let pool = pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    let h = load(&mut conn, id).await?;
    drop(conn);
    let (state, rev) = match q.at {
        Some(at) if at != h.rev => {
            if at < 1 || at > h.rev {
                return Err(bad_request(format!("document {id} has versions 1 to {}", h.rev)));
            }
            (state_at(&pool, id, at).await?, at)
        }
        _ => (h.state.clone(), h.rev),
    };
    let tab = match q.tab.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => Some(state.tab(t).map(|t| t.id.clone()).ok_or_else(|| not_found(format!("no tab {t}")))?),
        None => None,
    };
    match q.view.as_str() {
        "" | "outline" => Ok(outline_of(&state, rev, tab.as_deref())),
        "block" | "slide" => {
            let bid = q.id.as_deref().ok_or_else(|| bad_request(format!("view={} needs id", q.view)))?;
            let r = state.row(bid)?;
            if q.view == "slide" && r.kind != "slide" && r.kind != "pending" {
                return Err(bad_request(format!("block {bid} is not a slide")));
            }
            let mut v = block_view(&state, r);
            v["rev_doc"] = json!(rev);
            Ok(v)
        }
        "search" => {
            let words = q.q.as_deref().map(str::trim).filter(|w| !w.is_empty()).ok_or_else(|| bad_request("view=search needs q"))?;
            let lower = words.to_lowercase();
            let mut hits = Vec::new();
            for r in state.rows.values() {
                let text = if r.kind == "slide" { r.attrs["notes"].as_str().unwrap_or("").to_owned() } else { r.text.clone() };
                if let Some(i) = text.to_lowercase().find(&lower) {
                    let start = text[..i].char_indices().rev().nth(40).map(|(j, _)| j).unwrap_or(0);
                    let snippet: String = text[start..].chars().take(120).collect();
                    hits.push(json!({"id": r.id, "kind": r.kind, "tab": r.tab, "hash": state.hash(r),
                                     "slide": (!r.parent.is_empty()).then_some(&r.parent), "snippet": snippet}));
                }
            }
            Ok(json!({"rev": rev, "q": words, "hits": hits}))
        }
        "since" => {
            let since = q.rev.ok_or_else(|| bad_request("view=since needs rev"))?;
            let rows = sqlx::query("SELECT changes FROM doc_ops WHERE doc_id = ? AND rev > ? ORDER BY rev")
                .bind(id)
                .bind(since)
                .fetch_all(&pool)
                .await?;
            let mut ids = BTreeSet::new();
            let mut tabs = BTreeSet::new();
            let mut meta = false;
            for r in rows {
                let c: Value = serde_json::from_str(&r.get::<String, _>("changes"))?;
                ids.extend(c["blocks"].as_object().into_iter().flatten().map(|(k, _)| k.clone()));
                tabs.extend(c["tabs"].as_object().into_iter().flatten().map(|(k, _)| k.clone()));
                meta |= c.get("meta").is_some();
            }
            let changed: Vec<Value> = ids.iter().filter_map(|b| state.rows.get(b)).map(|r| block_view(&state, r)).collect();
            let removed: Vec<&String> = ids.iter().filter(|b| !state.rows.contains_key(*b)).collect();
            Ok(json!({"rev": rev, "since": since, "changed": changed, "removed": removed, "tabs": tabs, "title_changed": meta,
                      "title": state.meta.title}))
        }
        "markdown" => {
            let mut blocks = Map::new();
            let tabs: Vec<Value> = state
                .tabs_in_order()
                .into_iter()
                .filter(|t| tab.as_deref().is_none_or(|x| x == t.id))
                .map(|t| json!({"id": t.id, "title": t.title, "markdown": state.markdown(&t.id, &mut blocks)}))
                .collect();
            Ok(json!({"rev": rev, "kind": state.meta.kind, "title": state.meta.title, "tabs": tabs, "blocks": blocks}))
        }
        "full" => {
            let h = Head { state, rev, ..h };
            Ok(serde_json::to_value(doc_of(realm, id, &h))?)
        }
        other => Err(bad_request(format!("{other:?}: view is outline, block, slide, search, since, markdown or full"))),
    }
}

/// A document's versions, newest first.
pub async fn history(st: &HeadState, realm: &str, id: &str, limit: u32) -> anyhow::Result<Vec<DocVersion>> {
    let pool = pool(st, realm).await?;
    get(st, realm, id).await?;
    let rows = sqlx::query("SELECT * FROM doc_ops WHERE doc_id = ? ORDER BY rev DESC LIMIT ?")
        .bind(id)
        .bind(limit.clamp(1, 2000))
        .fetch_all(&pool)
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let ops: Value = serde_json::from_str(&r.get::<String, _>("ops")).unwrap_or(json!([]));
        let changes: Value = serde_json::from_str(&r.get::<String, _>("changes")).unwrap_or(json!({}));
        let rev: i64 = r.get("rev");
        let (mut added, mut changed, mut removed) = (Vec::new(), Vec::new(), Vec::new());
        for (b, v) in changes["blocks"].as_object().into_iter().flatten() {
            if v.is_null() {
                removed.push(b.clone());
            } else if changes["added"].as_array().is_some_and(|a| a.iter().any(|x| x == b)) {
                added.push(b.clone());
            } else {
                changed.push(b.clone());
            }
        }
        let first_op = ops[0]["op"].as_str().unwrap_or("");
        let summary = match first_op {
            "create" => "created".to_owned(),
            "restore" => format!("restored version {}", ops[0]["rev"]),
            _ => {
                let mut parts = Vec::new();
                if changes.get("meta").is_some() {
                    parts.push("renamed".to_owned());
                }
                for (n, what) in [(added.len(), "added"), (changed.len(), "changed"), (removed.len(), "removed")] {
                    if n > 0 {
                        parts.push(format!("{n} {} {what}", if n == 1 { "block" } else { "blocks" }));
                    }
                }
                if parts.is_empty() { "tabs changed".to_owned() } else { parts.join(", ") }
            }
        };
        out.push(DocVersion {
            rev,
            by: r.get("by"),
            turn: r.get("turn"),
            at: r.get("at"),
            ops: ops.as_array().map(Vec::len).unwrap_or(0) as u32,
            added,
            changed,
            removed,
            answering: r.get("answering"),
            summary,
        });
    }
    Ok(out)
}

// --- Events -------------------------------------------------------------------

/// A change went out: on the live feed now, and once it settles, as
/// `doc.changed` for triggers.
fn changed(st: &HeadState, realm: &str, id: &str, rev: i64, by: &str, what: &str) {
    let _ = st.docs.live.send(DocChange { id: id.to_owned(), realm: realm.to_owned(), rev, by: by.to_owned(), what: what.to_owned() });
    // A new document is found in search at once; later changes once they settle.
    if what == "created"
        && let Some(arc) = st.arc()
    {
        let (realm, id) = (realm.to_owned(), id.to_owned());
        tokio::spawn(async move {
            if let Ok(doc) = get(&arc, &realm, &id).await {
                mirror(&arc, &realm, &doc).await;
            }
        });
    }
    if what != "changed" || rev == 0 {
        return;
    }
    let key = (realm.to_owned(), id.to_owned());
    let start = {
        let mut s = st.docs.settling.lock().unwrap();
        match s.get_mut(&key) {
            Some(x) => {
                x.authors.insert(by.to_owned());
                x.rev = rev;
                x.last = std::time::Instant::now();
                false
            }
            None => {
                s.insert(key.clone(), Settling { authors: [by.to_owned()].into(), from_rev: rev, rev, last: std::time::Instant::now() });
                true
            }
        }
    };
    if !start {
        return;
    }
    let Some(arc) = st.arc() else { return };
    tokio::spawn(async move {
        let wait = std::time::Duration::from_millis(SETTLE_MS);
        loop {
            let left = {
                let s = arc.docs.settling.lock().unwrap();
                match s.get(&key) {
                    Some(x) => wait.saturating_sub(x.last.elapsed()),
                    None => return,
                }
            };
            if left.is_zero() {
                break;
            }
            tokio::select! {
                _ = arc.shutdown.cancelled() => return,
                _ = tokio::time::sleep(left) => {}
            }
        }
        let Some(x) = arc.docs.settling.lock().unwrap().remove(&key) else { return };
        settled(&arc, &key.0, &key.1, x).await;
    });
}

async fn settled(st: &HeadState, realm: &str, id: &str, x: Settling) {
    let doc = match get(st, realm, id).await {
        Ok(d) => d,
        Err(e) => {
            warn!(doc = %id, "a settled document: {e:#}");
            return;
        }
    };
    let event = json!({"type": "doc", "op": "changed", "doc": event_doc(&doc.summary), "authors": x.authors,
                       "from_rev": x.from_rev, "rev": x.rev});
    crate::triggers::dispatch(st, realm, "doc", "changed", event).await;
    mirror(st, realm, &doc).await;
    // Its projects moved (their plan, spec or notes changed).
    for p in crate::projects::holding_doc(st, realm, id).await {
        crate::projects::refresh(st, &p);
    }
}

/// Where a document's tabs are mirrored in the memex.
pub const MIRROR_SOURCE: &str = "docs";

/// Mirrors a document in the realm's memex (docs/docs.md §9), so search,
/// the Brief and agents find it: each tab an item of kind doc or deck,
/// rewritten when a change settles, not on every op.
async fn mirror(st: &HeadState, realm: &str, doc: &Doc) {
    let _guard = write_lock(st, realm, &doc.summary.id).await;
    // A settling task may have loaded the document before it was archived.
    // Re-read under the same lock archive transitions hold through enqueueing.
    if let Ok(current) = get(st, realm, &doc.summary.id).await {
        mirror_locked(st, realm, &current).await;
    }
}

async fn mirror_locked(st: &HeadState, realm: &str, doc: &Doc) {
    if doc.summary.archived_at.is_some() {
        return;
    }
    if !crate::memex::has_memex(realm) {
        return;
    }
    let state = match pool(st, realm).await {
        Ok(p) => match p.acquire().await {
            Ok(mut c) => match load_head(&mut c, &doc.summary.id).await {
                Ok((s, _)) => s,
                Err(e) => return warn!(doc = %doc.summary.id, "mirroring: {e:#}"),
            },
            Err(e) => return warn!(doc = %doc.summary.id, "mirroring: {e:#}"),
        },
        Err(e) => return warn!(doc = %doc.summary.id, "mirroring: {e:#}"),
    };
    let several = doc.tabs.len() > 1;
    for t in &doc.tabs {
        let item = c6_api::MemexItemIn {
            kind: doc.summary.kind.clone(),
            source: MIRROR_SOURCE.into(),
            external_id: format!("{}#{}", doc.summary.id, t.id),
            title: if several { format!("{} — {}", doc.summary.title, t.title) } else { doc.summary.title.clone() },
            text: export::tab_text(&state, &t.id),
            created_at: Some(doc.summary.created_at),
            labels: doc.summary.labels.clone(),
            meta: json!({"doc": doc.summary.id, "tab": t.id, "rev": doc.summary.rev, "ref": format!("doc:{}", doc.summary.id)}),
            ..Default::default()
        };
        crate::memex::enqueue(st, "memex.ingest", crate::memex::in_realm(json!(item), realm)).await;
    }
}

/// Takes a document's tabs out of the memex.
async fn unmirror(st: &HeadState, realm: &str, doc: &Doc) {
    if !crate::memex::has_memex(realm) {
        return;
    }
    let mut external_ids: BTreeSet<String> = doc.tabs.iter().map(|t| format!("{}#{}", doc.summary.id, t.id)).collect();
    // Include tabs removed after their mirror was queued. Read the outbox
    // before the items: a delivery in between then appears in the second
    // query, rather than falling through the gap between the two stores.
    if let Ok(ids) = sqlx::query_scalar::<_, String>(
        "SELECT json_extract(params, '$.external_id') FROM memex_outbox WHERE method = 'memex.ingest' \
         AND json_extract(params, '$.realm') = ? AND json_extract(params, '$.source') = ? \
         AND json_extract(params, '$.meta.doc') = ?",
    )
    .bind(realm)
    .bind(MIRROR_SOURCE)
    .bind(&doc.summary.id)
    .fetch_all(&st.store.pool)
    .await
    {
        external_ids.extend(ids);
    }
    // Include mirrors for tabs removed since the previous settled version.
    if let Ok(pool) = pool(st, realm).await
        && let Ok(ids) =
            sqlx::query_scalar::<_, String>("SELECT external_id FROM items WHERE source = ? AND json_extract(meta, '$.doc') = ?")
                .bind(MIRROR_SOURCE)
                .bind(&doc.summary.id)
                .fetch_all(&pool)
                .await
    {
        external_ids.extend(ids);
    }
    for external_id in external_ids {
        let params = json!({"source": MIRROR_SOURCE, "external_id": external_id});
        crate::memex::enqueue(st, "memex.delete", crate::memex::in_realm(params, realm)).await;
    }
}

/// Whether some document's changes haven't settled yet (for tests).
pub fn settling(st: &HeadState) -> bool {
    !st.docs.settling.lock().unwrap().is_empty()
}

/// Whether the caller may see a live change.
pub fn visible_to(caller: &Caller, c: &DocChange) -> bool {
    caller.sees(&c.realm)
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    use base64::Engine as _;
    use d::{Anchor, CommentEdit, CommentNew, DocFileIn, GoogleExportIn, ThreadNew};

    async fn head(dir: &std::path::Path) -> crate::Head {
        crate::start(crate::HeadConfig {
            data_dir: dir.to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    fn refused<T: std::fmt::Debug>(result: anyhow::Result<T>) {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("archived") && error.contains("Unarchive it first"), "{error}");
    }

    #[tokio::test]
    async fn archive_preserves_reads_refuses_writes_and_unarchives_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let head = head(dir.path()).await;
        let st = &head.state;
        let (doc, _) = create(
            st,
            "home",
            &DocNew { title: "Living plan".into(), markdown: "# Plan\n\nKeep this.".into(), ..Default::default() },
            "you",
        )
        .await
        .unwrap();
        let id = &doc.summary.id;
        let thread = comments::create(
            st,
            "home",
            id,
            &ThreadNew { anchor: Anchor::default(), text: "Please review".into(), to: String::new() },
            "you",
        )
        .await
        .unwrap();
        let answer =
            comments::reply(st, "home", id, &thread.id, &CommentNew { text: "Reviewed".into(), to: String::new() }, "session:agent")
                .await
                .unwrap();
        assert_eq!(comments::answered(st, "home", id).await, 1);
        assert_eq!(comments::answered_in(st, "home", 20).await.unwrap().len(), 1);
        let upload = DocFileIn {
            name: "note.txt".into(),
            mime: "text/plain".into(),
            content: base64::engine::general_purpose::STANDARD.encode(b"attached"),
        };
        let file = files::add(st, "home", id, &upload, "you").await.unwrap();
        let export = export::export(st, "home", id, "md", "you").await.unwrap();
        let p = pool(st, "home").await.unwrap();
        sqlx::query("INSERT INTO doc_google (doc_id, file_id, url, account, rev, modified, by, at) VALUES (?, 'google-id', 'https://example.test/file', 'me', 1, 'v1', 'you', 1)")
            .bind(id).execute(&p).await.unwrap();
        let before_google = google::of_doc(st, "home", id).await.unwrap().file;
        let before_history = history(st, "home", id, 100).await.unwrap();
        let memex = crate::knowledge::get(st).unwrap().of_realm("home").await.unwrap();
        let external_id = format!("{id}#{}", doc.tabs[0].id);
        c6_testkit::eventually("the document mirrored", async || memex.store.id_of(MIRROR_SOURCE, &external_id).await.unwrap()).await;
        let old_tab = format!("{id}#removed-tab");
        crate::memex::enqueue(
            st,
            "memex.ingest",
            crate::memex::in_realm(
                json!({"kind": "doc", "source": MIRROR_SOURCE, "external_id": old_tab,
                "title": "Old tab", "text": "Former content", "meta": {"doc": id}}),
                "home",
            ),
        )
        .await;
        let mut changes = st.docs.live.subscribe();

        assert!(set_archived(st, "home", id, true).await.unwrap());
        let event = changes.recv().await.unwrap();
        assert_eq!((&event.id, event.what.as_str()), (id, "changed"));
        let archived = get(st, "home", id).await.unwrap();
        assert!(archived.summary.archived_at.is_some());
        assert_eq!(archived_at(st, "home", id).await, archived.summary.archived_at);
        assert!(!set_archived(st, "home", id, true).await.unwrap());
        assert_eq!(get(st, "home", id).await.unwrap(), archived, "archive is idempotent");
        assert_eq!(archived.tabs, doc.tabs);
        assert_eq!(archived.summary.rev, doc.summary.rev);
        assert_eq!(history(st, "home", id, 100).await.unwrap(), before_history);
        assert!(title_of(st, "home", id).await.is_some());
        assert!(list(st, "home", &DocQuery::default()).await.unwrap().is_empty());
        assert!(list(st, "home", &DocQuery { q: Some("Living".into()), ..Default::default() }).await.unwrap().is_empty());
        assert_eq!(list(st, "home", &DocQuery { archived: true, ..Default::default() }).await.unwrap(), vec![archived.summary]);
        assert_eq!(comments::list(st, "home", id).await.unwrap(), vec![answer]);
        assert_eq!(comments::answered(st, "home", id).await, 0);
        assert!(comments::answered_in(st, "home", 20).await.unwrap().is_empty());
        assert_eq!(files::get(st, "home", id, &file.blob).await.unwrap().2, b"attached");
        assert!(!export::file(st, "home", id, &export.id).await.unwrap().2.is_empty());
        assert_eq!(export::list(st, "home", id).await.unwrap(), vec![export]);
        assert_eq!(google::of_doc(st, "home", id).await.unwrap().file, before_google);
        assert!(memex.store.keyword("Keep", &["doc".into()], &[], 10).await.unwrap().is_empty());
        c6_testkit::eventually("the archived document unmirrored", async || {
            let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM memex_outbox").fetch_one(&st.store.pool).await.unwrap();
            (pending == 0
                && memex.store.id_of(MIRROR_SOURCE, &external_id).await.unwrap().is_none()
                && memex.store.id_of(MIRROR_SOURCE, &old_tab).await.unwrap().is_none())
            .then_some(())
        })
        .await;
        assert!(read(st, "home", id, &DocRead { view: "markdown".into(), ..Default::default() }).await.is_ok());

        refused(ensure_writable(st, "home", id).await);
        refused(patch(st, "home", id, &DocPatch::default(), "you").await);
        refused(patch(st, "home", id, &DocPatch { title: Some("No".into()), ..Default::default() }, "you").await);
        refused(patch(st, "home", id, &DocPatch { labels: Some(vec!["topic:no".into()]), ..Default::default() }, "you").await);
        refused(
            patch(st, "home", id, &DocPatch { archived: Some(false), title: Some("No bypass".into()), ..Default::default() }, "you").await,
        );
        refused(apply(st, "home", id, &DocBatch::default(), "you", None).await);
        refused(restore(st, "home", id, &DocRestore { rev: 1, block: None }, "you").await);
        refused(comments::create(st, "home", id, &ThreadNew { text: "No".into(), ..Default::default() }, "you").await);
        refused(comments::reply(st, "home", id, &thread.id, &CommentNew { text: "No".into(), ..Default::default() }, "you").await);
        refused(comments::edit(st, "home", id, &thread.id, &thread.comments[0].id, &CommentEdit { text: "No".into() }, "you").await);
        refused(comments::delete(st, "home", id, &thread.id, &thread.comments[0].id, "you").await);
        refused(comments::resolve(st, "home", id, &thread.id, "you").await);
        refused(comments::send_many(st, "home", id, &[], "you").await);
        refused(files::add(st, "home", id, &upload, "you").await);
        refused(export::export(st, "home", id, "md", "you").await);
        refused(export::keep(st, "home", &doc, "md", b"stale render".to_vec(), "you").await);
        refused(google::export(st, "home", id, &GoogleExportIn::default(), "you").await);
        assert!(archived_at(st, "home", id).await.is_some(), "mixed unarchive/rename did not sneak through");

        assert!(set_archived(st, "home", id, false).await.unwrap());
        assert!(!set_archived(st, "home", id, false).await.unwrap());
        assert_eq!(get(st, "home", id).await.unwrap(), doc);
        c6_testkit::eventually("the unarchived document mirrored again", async || {
            memex.store.id_of(MIRROR_SOURCE, &external_id).await.unwrap()
        })
        .await;
        assert_eq!(comments::answered(st, "home", id).await, 1);
        patch(st, "home", id, &DocPatch { labels: Some(vec!["topic:live".into()]), ..Default::default() }, "you").await.unwrap();
        // Deleting an archived document remains the existing delete operation.
        set_archived(st, "home", id, true).await.unwrap();
        delete(st, "home", id, "you").await.unwrap();
        assert!(get(st, "home", id).await.is_err());
        assert!(list(st, "home", &DocQuery { archived: true, ..Default::default() }).await.unwrap().is_empty());
        assert_eq!(archived_at(st, "home", id).await, None);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn archive_waits_for_writers_and_stale_mirrors_cannot_reinsert_it() {
        let dir = tempfile::tempdir().unwrap();
        let head = head(dir.path()).await;
        let st = head.state.clone();
        let (doc, _) = create(&st, "home", &DocNew { title: "Locked".into(), markdown: "Original".into(), ..Default::default() }, "you")
            .await
            .unwrap();
        let id = &doc.summary.id;
        let guard = writable(&st, "home", id).await.unwrap();
        let transition = set_archived(&st, "home", id, true);
        tokio::pin!(transition);
        assert!(futures::poll!(&mut transition).is_pending());
        assert_eq!(archived_at(&st, "home", id).await, None);
        drop(guard);
        assert!(c6_testkit::within("archive to finish after the writer releases its lock", transition).await.unwrap());
        // Mirrors may have captured this old, active summary before archive.
        // Calling with it must still consult current state under the write lock.
        // AUTOINCREMENT persists even if the outbox is drained meanwhile.
        let before: i64 =
            sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'memex_outbox'").fetch_one(&st.store.pool).await.unwrap();
        mirror(&st, "home", &doc).await;
        let after: i64 =
            sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'memex_outbox'").fetch_one(&st.store.pool).await.unwrap();
        assert_eq!(after, before, "a stale mirror must enqueue no ingestion after archive");
        refused(writable(&st, "home", id).await);
        set_archived(&st, "home", id, false).await.unwrap();
        // Title + invalid labels is one atomic patch, not a partial rename.
        assert!(
            patch(
                &st,
                "home",
                id,
                &DocPatch { title: Some("Partial".into()), labels: Some(vec!["not a label".into()]), ..Default::default() },
                "you"
            )
            .await
            .is_err()
        );
        assert_eq!(get(&st, "home", id).await.unwrap().summary.title, "Locked");
        head.shutdown().await.unwrap();
    }
}
