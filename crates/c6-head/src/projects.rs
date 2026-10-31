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
        add_work(st, &id, &ProjectWorkIn { work: w.clone(), ..Default::default() }, caller, by).await?;
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
    if w.work.trim().is_empty() {
        let n = api::work::WorkNew { title: w.title.clone(), ..Default::default() };
        crate::work::create_in_project(st, caller, &n, id, phase, by).await?;
    } else {
        crate::work::set_project(st, &w.work, &p.realm, id, phase, by).await?;
    }
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

/// Finds projects in your work, by `project.propose`: groups of open and
/// recently closed work in no project (labels that already group them
/// count), for you to accept or skip.
pub async fn find(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<api::projects::ProjectProposal>> {
    let realm = caller.realm_for_new(None)?;
    let since = now_ms() - 30 * 24 * 3600 * 1000;
    let mut items = crate::work::list(st, caller, false).await?;
    items.extend(crate::work::list(st, caller, true).await?.into_iter().filter(|w| w.fields.closed_at.is_some_and(|c| c > since)));
    items.retain(|w| w.realm == realm && w.fields.project.is_empty());
    if items.len() < 3 {
        return Ok(Vec::new());
    }
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for w in &items {
        for l in &w.labels {
            *counts.entry(l.as_str()).or_default() += 1;
        }
    }
    let labels: Vec<String> = counts.iter().filter(|(_, n)| **n >= 2).map(|(l, n)| format!("- {l} ({n} pieces)")).collect();
    let work: Vec<String> = items
        .iter()
        .take(150)
        .map(|w| format!("{}: {} [{}] {}", w.id, w.fields.title, api::work::state_label(&w.state), w.labels.join(", ")))
        .collect();
    let have: Vec<String> = rows(st, Some(&realm), false)
        .await?
        .iter()
        .map(|p| format!("- {}: {}", p.fields.title, p.fields.purpose.replace('\n', " ")))
        .collect();
    let mut fallback = None;
    for w in &items {
        if let Some(m) = crate::work::latest_model(st, &w.id).await {
            fallback = Some(m);
            break;
        }
    }
    let model = match crate::work::model_for_route(st, &realm, ROUTE_LABEL, None).await? {
        Some(m) => m,
        None => crate::work::model_for_route(st, &realm, crate::work::ROUTE_LABEL, fallback)
            .await?
            .ok_or_else(|| bad_request("no model to ask: start a session first, or add a routing rule for task:projects"))?,
    };
    let template = crate::prompts::get(st, "project.propose", &realm).await;
    let none = |v: &[String]| if v.is_empty() { "(none)".to_owned() } else { v.join("\n") };
    let prompt = api::prompts::fill(&template, &[("work", &work.join("\n")), ("projects", &none(&have)), ("labels", &none(&labels))]);
    let v = crate::work::ask_as(st, &realm, model, "projects", "projects in what's open", "what's open", prompt)
        .await
        .map_err(|e| bad_request(format!("the model couldn't find projects: {e:#}")))?;
    let titles: std::collections::HashMap<&str, &str> = items.iter().map(|w| (w.id.as_str(), w.fields.title.as_str())).collect();
    let mut used = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in v["projects"].as_array().into_iter().flatten().take(5) {
        let title: String = p["title"].as_str().unwrap_or("").trim().chars().take(MAX_TITLE).collect();
        let work: Vec<(String, String)> = p["work"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|id| titles.contains_key(id) && used.insert(id.to_string()))
            .map(|id| (id.to_owned(), titles[id].to_owned()))
            .collect();
        if !title.is_empty() && work.len() >= 2 {
            out.push(api::projects::ProjectProposal {
                title,
                purpose: p["purpose"].as_str().unwrap_or("").trim().chars().take(MAX_PURPOSE).collect(),
                work,
            });
        }
    }
    Ok(out)
}

/// A realm's active projects.
pub(crate) async fn active_in(st: &HeadState, realm: &str) -> anyhow::Result<Vec<Stored>> {
    Ok(rows(st, Some(realm), false).await?.into_iter().filter(|r| r.fields.status == "active").collect())
}

/// Today, in the head's time zone, as YYYY-MM-DD.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Whether the current phase's target date has passed.
pub(crate) fn late(f: &ProjectFields, today: &str) -> bool {
    f.status == "active" && f.phases.iter().any(|p| p.state == "current" && !p.target.is_empty() && p.target.as_str() < today)
}

// --- Phases (docs/projects.md §7) --------------------------------------------

fn check_target(t: &str) -> anyhow::Result<String> {
    let t = t.trim();
    if !t.is_empty() && chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").is_err() {
        return Err(bad_request("a target date is YYYY-MM-DD"));
    }
    Ok(t.to_owned())
}

/// One change to a project's phases. A phase's state changes only here,
/// when you say so.
pub async fn phases(st: &HeadState, id: &str, c: &api::projects::PhaseIn, caller: &Caller, by: &str) -> anyhow::Result<ProjectItem> {
    let p = visible(st, id, caller).await?;
    let title = c.title.as_deref().map(str::trim).map(|t| {
        if t.is_empty() || t.chars().count() > MAX_TITLE {
            Err(bad_request(format!("a phase needs a title (up to {MAX_TITLE} characters)")))
        } else {
            Ok(t.to_owned())
        }
    });
    let title = title.transpose()?;
    let target = c.target.as_deref().map(check_target).transpose()?;
    let done_when = c.done_when.as_deref().map(|d| d.trim().chars().take(MAX_PURPOSE).collect::<String>());
    let which = c.phase.trim().to_owned();
    let find = |f: &ProjectFields| -> anyhow::Result<usize> {
        f.phases.iter().position(|x| x.id == which).ok_or_else(|| not_found(format!("the project has no phase {which:?}")))
    };
    let mut emptied = false;
    match c.action.as_str() {
        "add" => {
            let title = title.ok_or_else(|| bad_request("add takes the phase's title"))?;
            modify(st, id, by, None, |f| {
                let first = !f.phases.iter().any(|x| x.state == "current");
                f.phases.push(api::projects::Phase {
                    id: format!("ph{}", &uuid::Uuid::now_v7().simple().to_string()[24..]),
                    title,
                    done_when: done_when.unwrap_or_default(),
                    target: target.unwrap_or_default(),
                    state: if first && f.phases.is_empty() { "current" } else { "upcoming" }.into(),
                    ..Default::default()
                });
                Ok(true)
            })
            .await?;
        }
        "edit" => {
            modify(st, id, by, None, |f| {
                let i = find(f)?;
                let ph = &mut f.phases[i];
                if let Some(t) = title {
                    ph.title = t;
                }
                if let Some(d) = done_when {
                    ph.done_when = d;
                }
                if let Some(t) = target {
                    ph.target = t;
                }
                Ok(true)
            })
            .await?;
        }
        "remove" => {
            modify(st, id, by, None, |f| {
                let i = find(f)?;
                f.phases.remove(i);
                Ok(true)
            })
            .await?;
            emptied = true;
        }
        "move" => {
            let to = c.to.ok_or_else(|| bad_request("move takes to: the phase's new place, from 0"))?;
            modify(st, id, by, None, |f| {
                let i = find(f)?;
                let ph = f.phases.remove(i);
                f.phases.insert(to.min(f.phases.len()), ph);
                Ok(true)
            })
            .await?;
        }
        "start" => {
            modify(st, id, by, None, |f| {
                let i = find(f)?;
                for (k, ph) in f.phases.iter_mut().enumerate() {
                    if k == i {
                        ph.state = "current".into();
                    } else if ph.state == "current" {
                        ph.state = "upcoming".into();
                    }
                    ph.offer = None;
                }
                f.moved_at = now_ms();
                Ok(true)
            })
            .await?;
        }
        "advance" => {
            modify(st, id, by, None, |f| {
                let i = f.phases.iter().position(|x| x.state == "current").ok_or_else(|| bad_request("no phase is current"))?;
                f.phases[i].state = "done".into();
                if let Some(o) = f.phases[i].offer.take() {
                    f.phases[i].evidence = o.evidence;
                }
                if let Some(next) = f.phases.iter_mut().skip(i + 1).find(|x| x.state != "done") {
                    next.state = "current".into();
                }
                f.moved_at = now_ms();
                Ok(true)
            })
            .await?;
        }
        "dismiss" => {
            modify(st, id, by, None, |f| {
                let i = find(f)?;
                let ph = &mut f.phases[i];
                let Some(o) = ph.offer.take() else { return Ok(false) };
                ph.dismissed = o.evidence;
                Ok(true)
            })
            .await?;
        }
        other => {
            return Err(bad_request(format!("a phase's actions are add, edit, remove, move, start, advance and dismiss, not {other:?}")));
        }
    }
    // Work in a phase that's gone stays in the project, in none.
    if emptied {
        for w in crate::work::items_in_project(st, id).await? {
            if w.fields.phase == which {
                crate::work::set_project(st, &w.id, &p.realm, id, "", by).await?;
            }
        }
    }
    refresh(st, id);
    get(st, id, caller).await
}

/// The latest session of a project's work (to start the next one like it).
async fn latest_session(st: &HeadState, id: &str) -> anyhow::Result<Option<String>> {
    let mut best: Option<crate::store::SessionRow> = None;
    for w in crate::work::in_project(st, id).await? {
        for r in crate::work::refs(st, &w).await {
            if let Some(sid) = r.strip_prefix("session:")
                && let Some(s) = st.store.session(sid).await?
                && best.as_ref().is_none_or(|b| b.updated_at < s.updated_at)
            {
                best = Some(s);
            }
        }
    }
    Ok(best.map(|s| s.id))
}

fn phase_list(f: &ProjectFields) -> String {
    if f.phases.is_empty() {
        return "(none)".into();
    }
    f.phases
        .iter()
        .map(|p| {
            format!(
                "- {} ({}{}): {}",
                p.title,
                if p.state.is_empty() { "upcoming" } else { &p.state },
                if p.target.is_empty() { String::new() } else { format!(", target {}", p.target) },
                if p.done_when.is_empty() { "(done when not said)" } else { &p.done_when }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn doc_list(docs: &[LiveDoc]) -> String {
    if docs.is_empty() {
        return "(none)".into();
    }
    docs.iter()
        .map(|d| {
            format!(
                "- {} (doc:{}, {}{})",
                d.title,
                d.doc,
                if d.kind.is_empty() { "gone" } else { &d.kind },
                if d.role.is_empty() { String::new() } else { format!(", {}", d.role) }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A work item for planning (a phase, or the plan after a phase), and the
/// first message of a session on it, which you start (and may change).
pub async fn plan(
    st: &HeadState,
    id: &str,
    q: &api::projects::ProjectPlanIn,
    caller: &Caller,
    by: &str,
) -> anyhow::Result<api::projects::ProjectStart> {
    let item = get(st, id, caller).await?;
    let f = &item.fields;
    let plan_doc =
        item.docs.iter().find(|d| d.role == "plan").map(|d| format!("{} (doc:{})", d.title, d.doc)).unwrap_or_else(|| "(none)".into());
    let work_lines = |phase: Option<&str>| -> String {
        let lines: Vec<String> = item
            .work
            .iter()
            .filter(|w| phase.is_none_or(|p| w.phase == p))
            .map(|w| {
                format!(
                    "- {} ({}){}",
                    w.title,
                    api::work::state_label(&w.state),
                    if w.why.is_empty() { String::new() } else { format!(": {}", w.why) }
                )
            })
            .collect();
        if lines.is_empty() { "(none yet)".into() } else { lines.join("\n") }
    };
    let (title, phase, text) = if q.update {
        // The phase just done: the last done before the current one.
        let current = f.phases.iter().position(|x| x.state == "current");
        let done = f.phases.iter().enumerate().rev().find(|(i, x)| x.state == "done" && current.is_none_or(|c| *i < c)).map(|(_, x)| x);
        let done = done.ok_or_else(|| bad_request("no phase is done yet"))?;
        let next = current.map(|c| f.phases[c].title.clone()).unwrap_or_else(|| "(none: it was the last)".into());
        let template = crate::prompts::get(st, "project.plan_update", &item.realm).await;
        let evidence = if done.evidence.is_empty() { "(you marked it done)".to_owned() } else { done.evidence.clone() };
        let text = api::prompts::fill(
            &template,
            &[
                ("title", &f.title),
                ("phase", &done.title),
                ("next", &next),
                ("plan", &plan_doc),
                ("evidence", &evidence),
                ("work", &work_lines(Some(&done.id))),
            ],
        );
        (format!("Update the plan after {}", done.title), String::new(), text)
    } else {
        let which = q.phase.trim();
        let ph = if which.is_empty() {
            f.phases.iter().find(|x| x.state == "upcoming").or_else(|| f.phases.iter().find(|x| x.state == "current"))
        } else {
            f.phases.iter().find(|x| x.id == which)
        };
        let ph = ph.ok_or_else(|| bad_request("which phase? the project has none to plan"))?;
        let template = crate::prompts::get(st, "project.plan_phase", &item.realm).await;
        let text = api::prompts::fill(
            &template,
            &[
                ("title", &f.title),
                ("purpose", if f.purpose.is_empty() { "(not said)" } else { &f.purpose }),
                ("phase", &ph.title),
                ("done_when", if ph.done_when.is_empty() { "(not said)" } else { &ph.done_when }),
                ("target", if ph.target.is_empty() { "(none)" } else { &ph.target }),
                ("phases", &phase_list(f)),
                ("plan", &plan_doc),
                ("docs", &doc_list(&item.docs)),
                ("work", &work_lines(None)),
            ],
        );
        (format!("Plan {}", ph.title), ph.id.clone(), text)
    };
    let w = crate::work::create_in_project(st, caller, &api::work::WorkNew { title, ..Default::default() }, id, &phase, by).await?;
    let seed = latest_session(st, id).await?;
    Ok(api::projects::ProjectStart { work: w.id, text, seed })
}

/// A session proposes a piece of work for its work's project (planning a
/// phase): yours to accept or dismiss. In the phase of the session's work,
/// else the current one.
pub async fn propose(st: &HeadState, work: &str, title: &str, done_when: &str, by: &str) -> anyhow::Result<String> {
    let title = check_title(title)?;
    let pid = crate::work::project_of(st, work)
        .await
        .ok_or_else(|| bad_request("propose is for work that is in a project; this session's isn't"))?;
    let phase = crate::work::phase_of(st, work).await.unwrap_or_default();
    let mut made = String::new();
    modify(st, &pid, by, None, |f| {
        if f.proposed.len() >= 50 {
            return Err(bad_request("the project has 50 proposals waiting: ask the user to go through them first"));
        }
        if f.proposed.iter().any(|x| x.title.eq_ignore_ascii_case(&title)) {
            return Err(bad_request(format!("{title:?} is already proposed")));
        }
        let phase = if phase.is_empty() || !f.phases.iter().any(|x| x.id == phase) {
            f.phases.iter().find(|x| x.state == "current").map(|x| x.id.clone()).unwrap_or_default()
        } else {
            phase
        };
        made = format!("pw{}", &uuid::Uuid::now_v7().simple().to_string()[24..]);
        f.proposed.push(api::projects::ProposedWork {
            id: made.clone(),
            title,
            done_when: done_when.trim().chars().take(MAX_PURPOSE).collect(),
            phase,
            by: by.to_owned(),
            at: now_ms(),
        });
        Ok(true)
    })
    .await?;
    Ok(made)
}

/// You accept a proposed piece of work (a work item in the project, in its
/// phase) or dismiss it.
pub async fn proposed(st: &HeadState, id: &str, q: &api::projects::ProposedIn, caller: &Caller, by: &str) -> anyhow::Result<ProjectItem> {
    let p = visible(st, id, caller).await?;
    let one =
        p.fields.proposed.iter().find(|x| x.id == q.proposal).cloned().ok_or_else(|| not_found(format!("no proposal {}", q.proposal)))?;
    if q.accept {
        let phase = if p.fields.phases.iter().any(|x| x.id == one.phase) { one.phase.as_str() } else { "" };
        let n = api::work::WorkNew { title: one.title.clone(), done_when: one.done_when.clone(), ..Default::default() };
        crate::work::create_in_project(st, caller, &n, id, phase, by).await?;
    }
    modify(st, id, by, None, |f| {
        f.proposed.retain(|x| x.id != one.id);
        Ok(true)
    })
    .await?;
    get(st, id, caller).await
}

/// What a session started in a work item in a project is told after its
/// first message (`project.context`), unless the realm turned it off.
pub async fn context_for(st: &HeadState, work: &str) -> Option<String> {
    let pid = crate::work::project_of(st, work).await?;
    let r = load(st, &pid).await.ok()??;
    if r.fields.status != "active" || !crate::work::settings(st, &r.realm).await.project_context {
        return None;
    }
    let work_title = crate::work::title_of(st, work).await.unwrap_or_default();
    let item = view(st, vec![r]).await.ok()?.remove(0);
    let f = &item.fields;
    let phase = f
        .phases
        .iter()
        .find(|x| x.state == "current")
        .map(|x| format!("{}{}", x.title, if x.done_when.is_empty() { String::new() } else { format!(" (done when: {})", x.done_when) }))
        .unwrap_or_else(|| "(no phases)".into());
    let template = crate::prompts::get(st, "project.context", &item.realm).await;
    Some(api::prompts::fill(
        &template,
        &[
            ("title", &f.title),
            ("purpose", if f.purpose.is_empty() { "(not said)" } else { &f.purpose }),
            ("phase", &phase),
            ("note", f.note.as_ref().map(|n| n.text.as_str()).unwrap_or("(no note yet)")),
            ("docs", &doc_list(&item.docs)),
            ("work", &work_title),
        ],
    ))
}

/// A first message with its project's context after it (a session started
/// without one gets none: it would start a turn).
pub async fn with_context(st: &HeadState, work: Option<&str>, prompt: &str) -> Option<String> {
    let w = work.map(str::trim).filter(|w| !w.is_empty() && !matches!(*w, "new" | "none" | "guess"))?;
    if prompt.trim().is_empty() {
        return None;
    }
    let c = context_for(st, w).await?;
    Some(format!("{prompt}\n\n---\n{c}"))
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

/// Whether one of this head's projects has its note being written (for tests).
pub async fn writing(st: &HeadState) -> bool {
    let ids: Vec<String> = WRITING.lock().unwrap().0.iter().cloned().collect();
    for id in ids {
        if load(st, &id).await.ok().flatten().is_some() {
            return true;
        }
    }
    false
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
    let phases = phase_list(&item.fields);
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
    // Some models give the note's lines as a list.
    let text = match &v["note"] {
        Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"),
        n => n.as_str().unwrap_or("").to_owned(),
    };
    let text: String = text.trim().chars().take(2000).collect();
    let evidence: String = v["evidence"].as_str().unwrap_or("").trim().chars().take(500).collect();
    let all_closed = item.work.iter().all(|w| matches!(w.state.as_str(), "done" | "dropped"));
    let offer = (v["finished"] == true && !evidence.is_empty() && all_closed).then(|| api::work::WorkOffer { evidence, at: now_ms() });
    // The current phase looks done: only once it has work, all closed.
    let current = item.fields.phases.iter().find(|p| p.state == "current").map(|p| p.id.clone());
    let phase_work: Vec<&ProjectWork> = item.work.iter().filter(|w| current.as_deref() == Some(w.phase.as_str())).collect();
    let phase_closed = !phase_work.is_empty() && phase_work.iter().all(|w| matches!(w.state.as_str(), "done" | "dropped"));
    let phase_evidence: String = v["phase_evidence"].as_str().unwrap_or("").trim().chars().take(500).collect();
    let phase_offer = (v["phase_done"] == true && !phase_evidence.is_empty() && phase_closed)
        .then(|| api::work::WorkOffer { evidence: phase_evidence, at: now_ms() });
    modify(st, id, "agent:project.summary", None, |f| {
        if f.status != "active" || (text.is_empty() && offer.is_none() && phase_offer.is_none()) {
            return Ok(false);
        }
        if !text.is_empty() {
            f.note = Some(api::projects::ProjectNote { text, at: now_ms() });
        }
        if let (Some(o), Some(c)) = (phase_offer, current.as_deref())
            && let Some(ph) = f.phases.iter_mut().find(|p| p.id == c && p.state == "current")
            && ph.offer.is_none()
            && ph.dismissed != o.evidence
        {
            ph.offer = Some(o);
        }
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
        .route("/api/v1/projects/{id}/phases", post(phases_project))
        .route("/api/v1/projects/{id}/plan", post(plan_project))
        .route("/api/v1/projects/{id}/proposed", post(proposed_project))
        .route("/api/v1/projects/propose", post(propose_projects))
}

async fn propose_projects(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Vec<api::projects::ProjectProposal>>> {
    Ok(Json(find(&st, &caller).await?))
}

async fn phases_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(c): Json<api::projects::PhaseIn>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(phases(&st, &id, &c, &caller, &crate::docs::by_of(&caller)).await?))
}

async fn plan_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(q): Json<api::projects::ProjectPlanIn>,
) -> ApiResult<Json<api::projects::ProjectStart>> {
    Ok(Json(plan(&st, &id, &q, &caller, &crate::docs::by_of(&caller)).await?))
}

async fn proposed_project(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(q): Json<api::projects::ProposedIn>,
) -> ApiResult<Json<ProjectItem>> {
    Ok(Json(proposed(&st, &id, &q, &caller, &crate::docs::by_of(&caller)).await?))
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
