//! Projects (docs/projects.md): long-running efforts made of work items,
//! plus what outlives them (documents, links, phases). Projects are
//! records of the system collection `projects` in their realm, each change
//! a version in the record's history; a work item names its project in
//! its own fields (`project`, `phase`), so a piece of work is in one
//! project at most. Sessions belong to a project only through work.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::post;
use axum::{Extension, Json, Router};
use c6_api as api;
use c6_api::projects::{
    LiveDoc, ProjectDetach, ProjectFields, ProjectItem, ProjectMerge, ProjectNew, ProjectPatch, ProjectWork, ProjectWorkIn,
};
use c6_core::now_ms;
use serde_json::Value;
use sqlx::Row;

use crate::api::ApiResult;
use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};

/// The system collection.
pub const PROJECTS: &str = "projects";
const MAX_TITLE: usize = 200;
const MAX_PURPOSE: usize = 4000;

// --- Records ----------------------------------------------------------------

pub(crate) struct Stored {
    pub id: String,
    pub realm: String,
    pub version: i64,
    pub fields: ProjectFields,
    pub labels: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn stored(r: &sqlx::sqlite::SqliteRow) -> Stored {
    Stored {
        id: r.get("id"),
        realm: r.get("realm"),
        version: r.get("version"),
        fields: serde_json::from_str(&r.get::<String, _>("fields")).unwrap_or_default(),
        labels: serde_json::from_str(&r.get::<String, _>("labels")).unwrap_or_default(),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

pub(crate) async fn load(st: &HeadState, id: &str) -> anyhow::Result<Option<Stored>> {
    Ok(sqlx::query("SELECT * FROM records WHERE collection = ? AND id = ?")
        .bind(PROJECTS)
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .as_ref()
        .map(stored))
}

async fn visible(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<Stored> {
    match load(st, id).await? {
        Some(r) if caller.sees(&r.realm) => Ok(r),
        _ => Err(not_found(format!("no project {id}"))),
    }
}

/// Active and paused projects of a realm, or closed ones (None: all realms).
pub(crate) async fn rows(st: &HeadState, realm: Option<&str>, closed: bool) -> anyhow::Result<Vec<Stored>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT * FROM records WHERE collection = ? AND (? IS NULL OR realm = ?) AND json_extract(fields, '$.status') {} ('active', 'paused') \
         ORDER BY updated_at DESC LIMIT 1000",
        if closed { "NOT IN" } else { "IN" }
    )))
    .bind(PROJECTS)
    .bind(realm)
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows.iter().map(stored).collect())
}

/// A project's title, if it's in `realm`.
pub async fn title_of(st: &HeadState, realm: &str, id: &str) -> Option<String> {
    load(st, id).await.ok().flatten().filter(|r| r.realm == realm).map(|r| r.fields.title)
}

async fn history(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    version: i64,
    op: &str,
    fields: &Value,
    by: &str,
    at: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO record_history (record_id, version, op, fields, changed_by, changed_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(id)
        .bind(version)
        .bind(op)
        .bind(fields.to_string())
        .bind(by)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

fn check_title(t: &str) -> anyhow::Result<String> {
    let t = t.trim();
    if t.is_empty() || t.chars().count() > MAX_TITLE {
        return Err(bad_request(format!("a project needs a title (up to {MAX_TITLE} characters)")));
    }
    Ok(t.to_owned())
}

fn check_purpose(p: &str) -> anyhow::Result<String> {
    let p = p.trim();
    if p.chars().count() > MAX_PURPOSE {
        return Err(bad_request(format!("a purpose is up to {MAX_PURPOSE} characters")));
    }
    Ok(p.to_owned())
}

async fn insert(st: &HeadState, realm: &str, fields: &ProjectFields, labels: &[String], by: &str) -> anyhow::Result<String> {
    let id = uuid::Uuid::now_v7().to_string();
    let now = now_ms();
    let value = serde_json::to_value(fields)?;
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        "INSERT INTO records (id, realm, collection, fields, labels, version, created_by, updated_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(realm)
    .bind(PROJECTS)
    .bind(value.to_string())
    .bind(serde_json::to_string(labels)?)
    .bind(by)
    .bind(by)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    history(&mut tx, &id, 1, "create", &value, by, now).await?;
    tx.commit().await?;
    st.registry.bump();
    Ok(id)
}

/// Changes a project in one transaction, as a new version; `change`
/// returns false for nothing to write.
pub(crate) async fn modify(
    st: &HeadState,
    id: &str,
    by: &str,
    labels: Option<Vec<String>>,
    change: impl FnOnce(&mut ProjectFields) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let r = sqlx::query("SELECT * FROM records WHERE collection = ? AND id = ?").bind(PROJECTS).bind(id).fetch_optional(&mut *tx).await?;
    let Some(r) = r.as_ref().map(stored) else { return Err(not_found(format!("no project {id}"))) };
    let mut f = r.fields.clone();
    if !change(&mut f)? && labels.is_none() {
        return Ok(());
    }
    let now = now_ms();
    let value = serde_json::to_value(&f)?;
    let labels = match labels {
        Some(l) => serde_json::to_string(&crate::sessions::check_labels(&l)?)?,
        None => serde_json::to_string(&r.labels)?,
    };
    sqlx::query("UPDATE records SET fields = ?, labels = ?, version = ?, updated_by = ?, updated_at = ? WHERE id = ?")
        .bind(value.to_string())
        .bind(labels)
        .bind(r.version + 1)
        .bind(by)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    history(&mut tx, id, r.version + 1, "update", &value, by, now).await?;
    tx.commit().await?;
    st.registry.bump();
    Ok(())
}

/// Something in it moved, without a new version.
pub(crate) async fn touch(st: &HeadState, id: &str, at: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE records SET fields = json_set(fields, '$.moved_at', ?) WHERE collection = ? AND id = ? \
         AND coalesce(json_extract(fields, '$.moved_at'), 0) < ?",
    )
    .bind(at)
    .bind(PROJECTS)
    .bind(id)
    .bind(at)
    .execute(&st.store.pool)
    .await?;
    Ok(())
}

// --- What you do ----------------------------------------------------------

pub async fn create(st: &HeadState, caller: &Caller, n: &ProjectNew, by: &str) -> anyhow::Result<ProjectItem> {
    let realm = caller.realm_for_new(None)?;
    if !crate::memex::has_memex(&realm) {
        return Err(bad_request("projects live in a realm: root keeps none"));
    }
    let f = ProjectFields {
        title: check_title(&n.title)?,
        purpose: check_purpose(&n.purpose)?,
        status: "active".into(),
        moved_at: now_ms(),
        ..Default::default()
    };
    let labels = crate::sessions::check_labels(&n.labels)?;
    let mut f = f;
    // Its plan: a doc like any other, from the editable `project.plan`.
    if n.plan {
        let template = crate::prompts::get(st, "project.plan", &realm).await;
        let purpose = if f.purpose.is_empty() { "What it's for, and what success means." } else { &f.purpose };
        let markdown = api::prompts::fill(&template, &[("title", &f.title), ("purpose", purpose)]);
        let d = api::docs::DocNew { title: format!("{}: plan", f.title), markdown, ..Default::default() };
        let (doc, _) = crate::docs::create(st, &realm, &d, by).await?;
        f.documents.push(api::projects::ProjectDoc { doc: doc.summary.id, role: "plan".into(), by: by.to_owned(), at: now_ms() });
    }
    let id = insert(st, &realm, &f, &labels, by).await?;
    for w in &n.work {
        add_work(st, &id, &ProjectWorkIn { work: w.clone(), phase: String::new() }, caller, by).await?;
    }
    get(st, &id, caller).await
}

pub async fn get(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<ProjectItem> {
    let r = visible(st, id, caller).await?;
    Ok(view(st, vec![r]).await?.remove(0))
}

/// Active and paused projects (or closed ones), what moved last first.
pub async fn list(st: &HeadState, caller: &Caller, closed: bool) -> anyhow::Result<Vec<ProjectItem>> {
    let realm = caller.listing_realm().map(|r| if r == crate::realms::ROOT { crate::realms::HOME } else { r });
    let mut items = view(st, rows(st, realm, closed).await?).await?;
    let rank = |s: &str| api::projects::STATES.iter().position(|x| *x == s).unwrap_or(99);
    items.sort_by(|a, b| rank(&a.state).cmp(&rank(&b.state)).then(b.fields.moved_at.cmp(&a.fields.moved_at)));
    Ok(items)
}

pub async fn patch(st: &HeadState, id: &str, p: &ProjectPatch, caller: &Caller) -> anyhow::Result<ProjectItem> {
    visible(st, id, caller).await?;
    let title = p.title.as_deref().map(check_title).transpose()?;
    let purpose = p.purpose.as_deref().map(check_purpose).transpose()?;
    if let Some(s) = &p.status
        && !matches!(s.as_str(), "active" | "paused" | "done" | "dropped")
    {
        return Err(bad_request("a project is active, paused, done or dropped"));
    }
    modify(st, id, "you", p.labels.clone(), |f| {
        if let Some(t) = title {
            f.title = t;
        }
        if let Some(pp) = purpose {
            f.purpose = pp;
        }
        if let Some(s) = &p.status {
            if s != &f.status {
                let closing = matches!(s.as_str(), "done" | "dropped");
                f.closed_at = closing.then(now_ms);
                f.moved_at = now_ms();
            }
            f.status = s.clone();
            f.offer = None;
        }
        if p.dismiss_offer {
            f.offer = None;
        }
        Ok(true)
    })
    .await?;
    get(st, id, caller).await
}

/// A work item moves into the project (out of any other), in a phase.
pub async fn add_work(st: &HeadState, id: &str, w: &ProjectWorkIn, caller: &Caller, by: &str) -> anyhow::Result<ProjectItem> {
    let p = visible(st, id, caller).await?;
    let phase = w.phase.trim();
    if !phase.is_empty() && !p.fields.phases.iter().any(|x| x.id == phase) {
        return Err(bad_request(format!("the project has no phase {phase}")));
    }
    crate::work::set_project(st, &w.work, &p.realm, id, phase, by).await?;
    get(st, id, caller).await
}

/// Takes a work item, a document or a link out of the project.
pub async fn detach(st: &HeadState, id: &str, d: &ProjectDetach, caller: &Caller) -> anyhow::Result<ProjectItem> {
    let p = visible(st, id, caller).await?;
    if let Some(w) = d.work.as_deref() {
        if crate::work::project_of(st, w).await.as_deref() != Some(id) {
            return Err(bad_request(format!("work {w} is not in this project")));
        }
        crate::work::set_project(st, w, &p.realm, "", "", "you").await?;
    }
    if d.doc.is_some() || d.link.is_some() {
        modify(st, id, "you", None, |f| {
            let before = f.documents.len() + f.links.len();
            if let Some(doc) = d.doc.as_deref() {
                let doc = doc.trim_start_matches("doc:");
                f.documents.retain(|x| x.doc != doc);
            }
            if let Some(l) = d.link.as_deref() {
                f.links.retain(|x| x.reference != l.trim());
            }
            Ok(f.documents.len() + f.links.len() != before)
        })
        .await?;
    }
    get(st, id, caller).await
}

/// `from` joins `id`: its work, documents and links; `from` is gone (its
/// history stays).
pub async fn merge(st: &HeadState, id: &str, m: &ProjectMerge, caller: &Caller) -> anyhow::Result<ProjectItem> {
    if id == m.from {
        return Err(bad_request("a project is already itself"));
    }
    let into = visible(st, id, caller).await?;
    let other = visible(st, &m.from, caller).await?;
    if into.realm != other.realm {
        return Err(bad_request("projects merge only within a realm"));
    }
    for w in crate::work::in_project(st, &m.from).await? {
        crate::work::set_project(st, &w, &into.realm, id, "", "you").await?;
    }
    modify(st, id, "you", None, |f| {
        for d in &other.fields.documents {
            if !f.documents.iter().any(|x| x.doc == d.doc) {
                // At most one plan: the one it had wins.
                let role = if d.role == "plan" && f.documents.iter().any(|x| x.role == "plan") { String::new() } else { d.role.clone() };
                f.documents.push(api::projects::ProjectDoc { role, ..d.clone() });
            }
        }
        for l in &other.fields.links {
            if !f.links.iter().any(|x| x.reference == l.reference) {
                f.links.push(l.clone());
            }
        }
        if f.purpose.is_empty() {
            f.purpose = other.fields.purpose.clone();
        }
        f.moved_at = f.moved_at.max(other.fields.moved_at);
        Ok(true)
    })
    .await?;
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM records WHERE collection = ? AND id = ?").bind(PROJECTS).bind(&m.from).execute(&mut *tx).await?;
    history(&mut tx, &m.from, other.version + 1, "delete", &serde_json::to_value(&other.fields)?, "you", now_ms()).await?;
    tx.commit().await?;
    st.registry.bump();
    get(st, id, caller).await
}

// --- As it is now ---------------------------------------------------------

/// Each project with its work and documents as they are now, and its state.
pub(crate) async fn view(st: &HeadState, rows: Vec<Stored>) -> anyhow::Result<Vec<ProjectItem>> {
    let mut out = Vec::new();
    for r in rows {
        let items = crate::work::items_in_project(st, &r.id).await?;
        let mut work: Vec<ProjectWork> = items
            .into_iter()
            .map(|w| ProjectWork {
                moved_at: w.live.iter().map(|l| l.at).max().unwrap_or(0).max(w.fields.moved_at),
                id: w.id,
                title: w.fields.title,
                state: w.state,
                why: w.why,
                phase: w.fields.phase,
                closed_at: w.fields.closed_at,
            })
            .collect();
        let closed = |s: &str| matches!(s, "done" | "dropped");
        work.sort_by(|a, b| closed(&a.state).cmp(&closed(&b.state)).then(b.moved_at.cmp(&a.moved_at)));
        let mut docs = Vec::new();
        for d in &r.fields.documents {
            let (title, kind, at) =
                crate::docs::title_of(st, &r.realm, &d.doc).await.unwrap_or_else(|| ("(gone)".into(), String::new(), 0));
            let answered = if kind.is_empty() { 0 } else { crate::docs::comments::answered(st, &r.realm, &d.doc).await as u32 };
            docs.push(LiveDoc { doc: d.doc.clone(), role: d.role.clone(), title, kind, updated_at: at, answered });
        }
        docs.sort_by_key(|d| d.role != "plan");
        let (state, why) = state_of(&r.fields, &work, &docs);
        let late = late(&r.fields, &today());
        out.push(ProjectItem {
            id: r.id,
            realm: r.realm,
            version: r.version,
            fields: r.fields,
            state,
            why,
            late,
            work,
            docs,
            labels: r.labels,
            created_at: r.created_at,
            updated_at: r.updated_at,
        });
    }
    Ok(out)
}

/// Its state and why (docs/projects.md §4), the first that holds: what
/// you set (paused, done, dropped); needs you (a "looks done", or work
/// that needs you); moving (work moving or waiting on others); stalled (no
/// work open: nothing advances it); gone quiet (its open work has all gone
/// quiet); on track.
pub(crate) fn state_of(f: &ProjectFields, work: &[ProjectWork], docs: &[LiveDoc]) -> (String, String) {
    let r = |state: &str, why: String| (state.to_owned(), why);
    if f.status != "active" {
        return r(&f.status, String::new());
    }
    if let Some(o) = &f.offer {
        return r("needs_you", format!("Looks done: {}", o.evidence));
    }
    if let Some((p, o)) = f.phases.iter().find_map(|p| p.offer.as_ref().map(|o| (p, o))) {
        return r("needs_you", format!("{} looks done: {}", p.title, o.evidence));
    }
    if let Some(d) = docs.iter().find(|d| d.answered > 0) {
        return r("needs_you", format!("An agent answered a comment on {}", d.title));
    }
    let open: Vec<&ProjectWork> = work.iter().filter(|w| !matches!(w.state.as_str(), "done" | "dropped")).collect();
    if let Some(w) = open.iter().find(|w| w.state == "needs_you") {
        return r("needs_you", format!("{}: {}", w.title, w.why));
    }
    if let Some(w) = open.iter().find(|w| matches!(w.state.as_str(), "moving" | "waiting")) {
        return r("moving", if w.why.is_empty() { format!("{} is moving", w.title) } else { format!("{}: {}", w.title, w.why) });
    }
    if open.is_empty() {
        let why = if work.is_empty() { "No work in it yet" } else { "All its work is closed, and nothing new is open" };
        return r("stalled", why.to_owned());
    }
    if open.iter().all(|w| w.state == "quiet") {
        return r("quiet", "Its open work has all gone quiet".to_owned());
    }
    r("on_track", if open.len() == 1 { "1 piece of work open".to_owned() } else { format!("{} pieces of work open", open.len()) })
}

/// Today, in the head's time zone, as YYYY-MM-DD.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Whether the current phase's target date has passed.
pub(crate) fn late(f: &ProjectFields, today: &str) -> bool {
    f.status == "active" && f.phases.iter().any(|p| p.state == "current" && !p.target.is_empty() && p.target.as_str() < today)
}

// --- Documents and links -----------------------------------------------------

/// A document joins the project with a role, or changes role. One plan.
pub async fn add_doc(st: &HeadState, id: &str, d: &api::projects::ProjectDocIn, caller: &Caller, by: &str) -> anyhow::Result<ProjectItem> {
    let p = visible(st, id, caller).await?;
    add_doc_in(st, &p, d, by).await?;
    get(st, id, caller).await
}

pub(crate) async fn add_doc_in(st: &HeadState, p: &Stored, d: &api::projects::ProjectDocIn, by: &str) -> anyhow::Result<()> {
    let doc = d.doc.trim().trim_start_matches("doc:").to_owned();
    let role = d.role.trim().to_owned();
    if !role.is_empty() && !api::projects::ROLES.contains(&role.as_str()) {
        return Err(bad_request(format!("a role is one of {}, or none", api::projects::ROLES.join(", "))));
    }
    if crate::docs::title_of(st, &p.realm, &doc).await.is_none() {
        return Err(bad_request(format!("no document {doc} in realm {}", p.realm)));
    }
    modify(st, &p.id, by, None, |f| {
        if role == "plan" {
            for x in f.documents.iter_mut().filter(|x| x.role == "plan" && x.doc != doc) {
                x.role = String::new();
            }
        }
        match f.documents.iter_mut().find(|x| x.doc == doc) {
            Some(x) if x.role == role => return Ok(false),
            Some(x) => x.role = role.clone(),
            None => f.documents.push(api::projects::ProjectDoc { doc: doc.clone(), role: role.clone(), by: by.to_owned(), at: now_ms() }),
        }
        f.moved_at = now_ms();
        Ok(true)
    })
    .await?;
    refresh(st, &p.id);
    Ok(())
}

/// A link: a record (`record:<collection>:<id>`), a decision, or a URL.
pub async fn add_link(
    st: &HeadState,
    id: &str,
    l: &api::projects::ProjectLinkIn,
    caller: &Caller,
    by: &str,
) -> anyhow::Result<ProjectItem> {
    let p = visible(st, id, caller).await?;
    add_link_in(st, &p, l, by).await?;
    get(st, id, caller).await
}

pub(crate) async fn add_link_in(st: &HeadState, p: &Stored, l: &api::projects::ProjectLinkIn, by: &str) -> anyhow::Result<()> {
    let r = l.reference.trim().to_owned();
    let ok =
        ["record:", "decision:", "session:", "http://", "https://"].iter().any(|x| r.strip_prefix(x).is_some_and(|rest| !rest.is_empty()))
            && r.len() <= 2000
            && !r.contains(char::is_whitespace);
    if !ok {
        return Err(bad_request(format!("{r:?}: a link is record:<collection>:<id>, decision:<id>, session:<id> or a URL")));
    }
    let title: String = l.title.trim().chars().take(300).collect();
    modify(st, &p.id, by, None, |f| {
        match f.links.iter_mut().find(|x| x.reference == r) {
            Some(x) if title.is_empty() || x.title == title => return Ok(false),
            Some(x) => x.title = title.clone(),
            None => {
                f.links.push(api::projects::ProjectLink { reference: r.clone(), title: title.clone(), by: by.to_owned(), at: now_ms() })
            }
        }
        Ok(true)
    })
    .await
}

/// The active projects holding a document (for its changes).
pub async fn holding_doc(st: &HeadState, realm: &str, doc: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT r.id FROM records r WHERE r.collection = ? AND r.realm = ? \
         AND EXISTS (SELECT 1 FROM json_each(r.fields, '$.documents') d WHERE json_extract(d.value, '$.doc') = ?)",
    )
    .bind(PROJECTS)
    .bind(realm)
    .bind(doc)
    .fetch_all(&st.store.pool)
    .await
    .unwrap_or_default()
}

/// Whether a project holds a document.
pub async fn holds_doc(st: &HeadState, id: &str, doc: &str) -> bool {
    load(st, id).await.ok().flatten().is_some_and(|p| p.fields.documents.iter().any(|d| d.doc == doc))
}

// --- Where we are -------------------------------------------------------------

/// Projects whose note is being written, and those to write again after.
static WRITING: std::sync::LazyLock<std::sync::Mutex<(std::collections::HashSet<String>, std::collections::HashSet<String>)>> =
    std::sync::LazyLock::new(Default::default);

/// The project moved (work in it opened, closed or moved in or out; a
/// document of it changed): its note is brought up to date in the
/// background, one at a time per project.
pub fn refresh(st: &HeadState, id: &str) {
    {
        let mut w = WRITING.lock().unwrap();
        if w.0.contains(id) {
            w.1.insert(id.to_owned());
            return;
        }
        w.0.insert(id.to_owned());
    }
    let Some(arc) = st.arc() else { return };
    let id = id.to_owned();
    tokio::spawn(async move {
        loop {
            if let Err(e) = note(&arc, &id).await {
                tracing::warn!(project = %id, "writing its note: {e:#}");
            }
            let mut w = WRITING.lock().unwrap();
            if !w.1.remove(&id) {
                w.0.remove(&id);
                return;
            }
        }
    });
}

/// Whether a project's note is being written (for tests).
pub fn writing() -> bool {
    !WRITING.lock().unwrap().0.is_empty()
}

/// The routing label a rule names for projects' agent steps (else work's).
pub const ROUTE_LABEL: &str = "task:projects";

/// Writes "where we are", and an offer when it looks done, by `project.summary`.
async fn note(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let Some(r) = load(st, id).await? else { return Ok(()) };
    let _ = touch(st, id, now_ms()).await;
    if r.fields.status != "active" || !crate::work::settings(st, &r.realm).await.notes {
        return Ok(());
    }
    let item = view(st, vec![r]).await?.remove(0);
    if item.work.is_empty() {
        return Ok(());
    }
    let mut fallback = None;
    for w in &item.work {
        if let Some(m) = crate::work::latest_model(st, &w.id).await {
            fallback = Some(m);
            break;
        }
    }
    let model = match crate::work::model_for_route(st, &item.realm, ROUTE_LABEL, None).await? {
        Some(m) => m,
        None => match crate::work::model_for_route(st, &item.realm, crate::work::ROUTE_LABEL, fallback).await? {
            Some(m) => m,
            None => return Ok(()),
        },
    };
    let phases = if item.fields.phases.is_empty() {
        "(none)".to_owned()
    } else {
        item.fields
            .phases
            .iter()
            .map(|p| {
                format!(
                    "- {} ({}{}): {}",
                    p.title,
                    if p.state.is_empty() { "upcoming" } else { &p.state },
                    if p.target.is_empty() { String::new() } else { format!(", target {}", p.target) },
                    p.done_when
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut work = String::new();
    for w in &item.work {
        let next = crate::work::next_step(st, &w.id).await.map(|n| format!("; next: {n}")).unwrap_or_default();
        let phase = item.fields.phases.iter().find(|p| p.id == w.phase).map(|p| format!(" [{}]", p.title)).unwrap_or_default();
        work.push_str(&format!("- {} ({}){phase}: {}{next}\n", w.title, w.state, w.why));
    }
    let docs = if item.docs.is_empty() {
        "(none)".to_owned()
    } else {
        item.docs
            .iter()
            .map(|d| format!("- {} ({})", d.title, if d.role.is_empty() { "document" } else { &d.role }))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let previous = item.fields.note.as_ref().map(|n| n.text.clone()).unwrap_or_else(|| "(none yet)".into());
    let template = crate::prompts::get(st, "project.summary", &item.realm).await;
    let prompt = api::prompts::fill(
        &template,
        &[
            ("title", &item.fields.title),
            ("purpose", if item.fields.purpose.is_empty() { "(not said)" } else { &item.fields.purpose }),
            ("phases", &phases),
            ("note", &previous),
            ("work", &work),
            ("docs", &docs),
        ],
    );
    let v = crate::work::ask_as(st, &item.realm, model, "projects", "where we are", &item.fields.title, prompt).await?;
    let text: String = v["note"].as_str().unwrap_or("").trim().chars().take(2000).collect();
    let evidence: String = v["evidence"].as_str().unwrap_or("").trim().chars().take(500).collect();
    let all_closed = item.work.iter().all(|w| matches!(w.state.as_str(), "done" | "dropped"));
    let offer = (v["finished"] == true && !evidence.is_empty() && all_closed).then(|| api::work::WorkOffer { evidence, at: now_ms() });
    modify(st, id, "agent:project.summary", None, |f| {
        if f.status != "active" || text.is_empty() {
            return Ok(false);
        }
        f.note = Some(api::projects::ProjectNote { text, at: now_ms() });
        // A "looks done" you dismissed isn't offered again for the same evidence.
        if let Some(o) = offer
            && f.offer.as_ref().is_none_or(|x| x.evidence != o.evidence)
        {
            f.offer = Some(o);
        }
        Ok(true)
    })
    .await
}

// --- Nudges -----------------------------------------------------------------

/// Tells you when a project stalls or its current phase goes late, once per
/// state until it moves (where its realm's work settings allow nudges).
pub async fn sweep(st: &HeadState) -> anyhow::Result<usize> {
    let items = view(st, rows(st, None, false).await?).await?;
    let mut told = 0;
    for p in items {
        if p.fields.status != "active" || !crate::work::settings(st, &p.realm).await.nudge {
            continue;
        }
        let state = if p.state == "stalled" {
            "stalled"
        } else if p.late {
            "late"
        } else {
            continue;
        };
        let since = p.fields.moved_at;
        if p.fields.told.as_ref().is_some_and(|t| t.state == state && t.at >= since) {
            continue;
        }
        let title = match state {
            "stalled" => format!("Stalled: {}", p.fields.title),
            _ => format!("Late: {}", p.fields.title),
        };
        let phase = p.fields.phases.iter().find(|x| x.state == "current");
        let message = match (state, phase) {
            ("late", Some(ph)) => format!("{} was due {}", ph.title, ph.target),
            _ => p.why.clone(),
        };
        st.notifier.send(crate::notify::Note {
            event: "work",
            title,
            message,
            priority: "normal",
            labels: p.labels.clone(),
            path: Some(format!("/r/{}/projects/{}", p.realm, p.id)),
            ..Default::default()
        });
        sqlx::query("UPDATE records SET fields = json_set(fields, '$.told', json(?)) WHERE collection = ? AND id = ?")
            .bind(serde_json::to_string(&api::work::Told { state: state.to_owned(), at: now_ms() })?)
            .bind(PROJECTS)
            .bind(&p.id)
            .execute(&st.store.pool)
            .await?;
        told += 1;
    }
    Ok(told)
}

// --- HTTP -------------------------------------------------------------------

pub fn routes() -> Router<Arc<HeadState>> {
    Router::new()
        .route("/api/v1/projects", axum::routing::get(list_projects).post(add_project))
        .route("/api/v1/projects/{id}", axum::routing::get(get_project).patch(patch_project))
        .route("/api/v1/projects/{id}/work", post(work_project))
        .route("/api/v1/projects/{id}/detach", post(detach_project))
        .route("/api/v1/projects/{id}/merge", post(merge_project))
        .route("/api/v1/projects/{id}/docs", post(doc_project))
        .route("/api/v1/projects/{id}/links", post(link_project))
        .route("/api/v1/projects/{id}/refresh", post(refresh_project))
}

async fn doc_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(d): Json<api::projects::ProjectDocIn>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(add_doc(&st, &id, &d, &caller, &crate::docs::by_of(&caller)).await?))
}

async fn link_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(l): Json<api::projects::ProjectLinkIn>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(add_link(&st, &id, &l, &caller, &crate::docs::by_of(&caller)).await?))
}

async fn refresh_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    visible(&st, &id, &caller).await?;
    refresh(&st, &id);
    Ok(axum::http::StatusCode::ACCEPTED)
}

/// `GET /api/v1/projects`.
#[derive(Clone, Debug, Default, schemars::JsonSchema, serde::Deserialize)]
pub(crate) struct ProjectQuery {
    /// Closed projects (done or dropped) instead of active and paused.
    #[serde(default)]
    closed: bool,
}

async fn list_projects(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<ProjectQuery>,
) -> ApiResult<Json<Vec<ProjectItem>>> {
    Ok(Json(list(&st, &caller, q.closed).await?))
}

async fn add_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Json(n): Json<ProjectNew>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(create(&st, &caller, &n, &crate::docs::by_of(&caller)).await?))
}

async fn get_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(get(&st, &id, &caller).await?))
}

async fn patch_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(p): Json<ProjectPatch>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(patch(&st, &id, &p, &caller).await?))
}

async fn work_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(w): Json<ProjectWorkIn>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(add_work(&st, &id, &w, &caller, &crate::docs::by_of(&caller)).await?))
}

async fn detach_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(d): Json<ProjectDetach>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(detach(&st, &id, &d, &caller).await?))
}

async fn merge_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(m): Json<ProjectMerge>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(merge(&st, &id, &m, &caller).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(title: &str, state: &str) -> ProjectWork {
        ProjectWork { id: title.into(), title: title.into(), state: state.into(), why: format!("{title} why"), ..Default::default() }
    }

    #[test]
    fn its_state_comes_from_its_work() {
        let f = ProjectFields { title: "p".into(), status: "active".into(), ..Default::default() };
        let st = |work: &[ProjectWork]| state_of(&f, work, &[]).0;
        assert_eq!(st(&[]), "stalled");
        assert_eq!(state_of(&f, &[w("a", "done")], &[]), ("stalled".into(), "All its work is closed, and nothing new is open".into()));
        assert_eq!(st(&[w("a", "open")]), "on_track");
        assert_eq!(st(&[w("a", "quiet"), w("b", "quiet"), w("c", "done")]), "quiet");
        assert_eq!(st(&[w("a", "quiet"), w("b", "waiting")]), "moving");
        assert_eq!(state_of(&f, &[w("a", "moving"), w("b", "needs_you")], &[]), ("needs_you".into(), "b: b why".into()));
        let offered = ProjectFields { offer: Some(api::work::WorkOffer { evidence: "shipped".into(), at: 1 }), ..f.clone() };
        assert_eq!(state_of(&offered, &[], &[]).0, "needs_you");
        let answered = LiveDoc { doc: "d".into(), title: "Plan".into(), kind: "doc".into(), answered: 1, ..Default::default() };
        assert_eq!(state_of(&f, &[w("a", "open")], &[answered]), ("needs_you".into(), "An agent answered a comment on Plan".into()));
        let paused = ProjectFields { status: "paused".into(), ..f.clone() };
        assert_eq!(state_of(&paused, &[], &[]).0, "paused");
        // Late: the current phase's target has passed.
        let phases = vec![
            api::projects::Phase {
                id: "1".into(),
                title: "One".into(),
                state: "done".into(),
                target: "2026-01-01".into(),
                ..Default::default()
            },
            api::projects::Phase {
                id: "2".into(),
                title: "Two".into(),
                state: "current".into(),
                target: "2026-10-01".into(),
                ..Default::default()
            },
        ];
        let phased = ProjectFields { phases, ..f };
        assert!(late(&phased, "2026-10-02") && !late(&phased, "2026-10-01"));
    }
}
