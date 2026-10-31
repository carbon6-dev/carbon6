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
    touch(st, id, now_ms()).await?;
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
            docs.push(LiveDoc { doc: d.doc.clone(), role: d.role.clone(), title, kind, updated_at: at });
        }
        docs.sort_by_key(|d| d.role != "plan");
        let (state, why) = state_of(&r.fields, &work);
        out.push(ProjectItem {
            id: r.id,
            realm: r.realm,
            version: r.version,
            fields: r.fields,
            state,
            why,
            late: false,
            work,
            docs,
            labels: r.labels,
            created_at: r.created_at,
            updated_at: r.updated_at,
        });
    }
    Ok(out)
}

/// Its state (step 4 keeps it to what you set; docs/projects.md §4 comes
/// with step 5).
fn state_of(f: &ProjectFields, work: &[ProjectWork]) -> (String, String) {
    if f.status != "active" {
        return (f.status.clone(), String::new());
    }
    let open = work.iter().filter(|w| !matches!(w.state.as_str(), "done" | "dropped")).count();
    let why = match open {
        0 => "No open work".to_owned(),
        1 => "1 piece of work open".to_owned(),
        n => format!("{n} pieces of work open"),
    };
    ("on_track".into(), why)
}

// --- HTTP -------------------------------------------------------------------

pub fn routes() -> Router<Arc<HeadState>> {
    Router::new()
        .route("/api/v1/projects", axum::routing::get(list_projects).post(add_project))
        .route("/api/v1/projects/{id}", axum::routing::get(get_project).patch(patch_project))
        .route("/api/v1/projects/{id}/work", post(work_project))
        .route("/api/v1/projects/{id}/detach", post(detach_project))
        .route("/api/v1/projects/{id}/merge", post(merge_project))
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
