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

pub mod bring;
pub mod comments;
pub mod export;
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

/// Documents' changes as they happen, and those not yet settled.
pub struct Docs {
    pub live: tokio::sync::broadcast::Sender<DocChange>,
    settling: Mutex<HashMap<(String, String), Settling>>,
}

impl Default for Docs {
    fn default() -> Self {
        Self { live: tokio::sync::broadcast::channel(256).0, settling: Default::default() }
    }
}

struct Settling {
    authors: BTreeSet<String>,
    from_rev: i64,
    rev: i64,
    last: std::time::Instant,
}

/// Who a caller writes as: you (in the browser, or with the console's
/// token), else the realm's token.
pub fn by_of(caller: &Caller) -> String {
    match (&caller.realm, caller.browser) {
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
async fn pool(st: &HeadState, realm: &str) -> anyhow::Result<sqlx::SqlitePool> {
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
         AND (?4 IS NULL OR title LIKE '%' || ?4 || '%') ORDER BY updated_at DESC LIMIT ?5",
    )
    .bind(q.kind.as_deref().filter(|k| !k.is_empty()))
    .bind(q.label.as_deref().filter(|k| !k.is_empty()))
    .bind(q.by.as_deref().filter(|k| !k.is_empty()))
    .bind(q.q.as_deref().map(str::trim).filter(|k| !k.is_empty()))
    .bind(limit)
    .fetch_all(&pool)
    .await?;
    let mut out = Vec::new();
    let mut conn = pool.acquire().await?;
    for r in rows {
        let id: String = r.get("id");
        let h = load(&mut conn, &id).await?;
        out.push(summary_of(realm, &id, &h));
    }
    Ok(out)
}

pub async fn get(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<Doc> {
    let pool = pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    let h = load(&mut conn, id).await?;
    Ok(doc_of(realm, id, &h))
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
        apply(st, realm, id, &DocBatch { ops: vec![op], answering: String::new() }, by, None).await?;
    }
    if let Some(l) = &p.labels {
        let labels = crate::sessions::check_labels(l)?;
        let pool = pool(st, realm).await?;
        let n = sqlx::query("UPDATE docs SET labels = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(serde_json::to_string(&labels)?)
            .bind(id)
            .execute(&pool)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(not_found(format!("no document {id}")));
        }
        changed(st, realm, id, 0, by, "changed");
    }
    get(st, realm, id).await
}

/// Deletes a document (its history goes with it from view).
pub async fn delete(st: &HeadState, realm: &str, id: &str, by: &str) -> anyhow::Result<()> {
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
    Ok(())
}

/// Applies a batch of operations as `by` (for a session, in `turn`).
pub async fn apply(st: &HeadState, realm: &str, id: &str, batch: &DocBatch, by: &str, turn: Option<i64>) -> anyhow::Result<DocApplied> {
    if batch.ops.is_empty() {
        return Err(bad_request("a batch needs at least one operation"));
    }
    let pool = pool(st, realm).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let h = load(&mut tx, id).await?;
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
    let pool = pool(st, realm).await?;
    let old = state_at(&pool, id, r.rev).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let h = load(&mut tx, id).await?;
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
    // Its projects moved (their plan, spec or notes changed).
    for p in crate::projects::holding_doc(st, realm, id).await {
        crate::projects::refresh(st, &p);
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
