//! Work: what you've left open (docs/work.md). A work item is something
//! you mean to finish, with the sessions, requests, conversations,
//! decisions and outside things attached to it. Items are records of the
//! system collection `work` in their realm, each with its history; their
//! state is worked out from what's attached when they're read.
//!
//! Agents do three things, each with an editable prompt: guess which work
//! a session you start belongs to (`work.attach`), keep each item's "where
//! was I" note and offer "looks done" (`work.summary`), and group what's
//! open into work once (`work.propose`). Only you close work.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use c6_api as api;
use c6_api::work::{
    Attached, LiveAttached, Todo, TodoNew, TodoPatch, TodoStart, Told, WorkFields, WorkItem, WorkNote, WorkOffer, WorkProposal, WorkRef,
    WorkSettings,
};
use c6_core::now_ms;
use c6_llm::{Accumulator, Block, Message, ModelRequest, Role};
use futures::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::warn;

use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};

/// The system collection.
pub const WORK: &str = "work";
/// The per-realm setting (`api::work::WorkSettings`).
pub const SETTING: &str = "work";
/// The routing label a rule names to pick the model for work's agent steps.
pub const ROUTE_LABEL: &str = "task:work";
const MAX_TITLE: usize = 200;
const MAX_ATTACHED: usize = 200;
const MAX_TODOS: usize = 50;
const MAX_TODO_TEXT: usize = 300;
const MAX_TODO_PROMPT: usize = 20_000;
/// What a note's model call reads at most (about 8,000 tokens).
const MAX_INPUT_CHARS: usize = 32_000;
const DAY: i64 = 24 * 3600 * 1000;

pub async fn settings(st: &HeadState, realm: &str) -> WorkSettings {
    st.store.setting_for(SETTING, realm).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

// --- Records ----------------------------------------------------------------

struct Stored {
    id: String,
    realm: String,
    version: i64,
    fields: WorkFields,
    labels: Vec<String>,
    created_at: i64,
    updated_at: i64,
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

async fn load(st: &HeadState, id: &str) -> anyhow::Result<Option<Stored>> {
    Ok(sqlx::query("SELECT * FROM records WHERE collection = ? AND id = ?")
        .bind(WORK)
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .as_ref()
        .map(stored))
}

/// A work item the caller may see.
async fn visible(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<Stored> {
    match load(st, id).await? {
        Some(r) if caller.sees(&r.realm) => Ok(r),
        _ => Err(not_found(format!("no work item {id}"))),
    }
}

/// Open items (and snoozed ones), or closed ones, of a realm (None: all).
async fn rows(st: &HeadState, realm: Option<&str>, closed: bool) -> anyhow::Result<Vec<Stored>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT * FROM records WHERE collection = ? AND (? IS NULL OR realm = ?) AND json_extract(fields, '$.status') {} 'open' \
         ORDER BY updated_at DESC LIMIT {}",
        if closed { "!=" } else { "=" },
        if closed { 200 } else { 2000 }
    )))
    .bind(WORK)
    .bind(realm)
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows.iter().map(stored).collect())
}

/// Puts a work item in a project of its realm (with a phase), or takes it
/// out (`project` ""). A work item is in one project at most.
pub async fn set_project(st: &HeadState, id: &str, realm: &str, project: &str, phase: &str, by: &str) -> anyhow::Result<()> {
    let w = load(st, id).await?.ok_or_else(|| not_found(format!("no work item {id}")))?;
    if w.realm != realm {
        return Err(bad_request(format!("that work is in realm {}, the project in {realm}", w.realm)));
    }
    let before = w.fields.project.clone();
    modify(st, id, by, None, |f| {
        if f.project == project && f.phase == phase && f.project_guess == (!project.is_empty() && by.starts_with("agent:")) {
            return Ok(false);
        }
        f.project = project.to_owned();
        f.phase = if project.is_empty() { String::new() } else { phase.to_owned() };
        f.project_guess = !project.is_empty() && by.starts_with("agent:");
        Ok(true)
    })
    .await?;
    // Both projects moved: their notes are brought up to date.
    let now = now_ms();
    for p in [before.as_str(), project] {
        if !p.is_empty() {
            crate::projects::touch(st, p, now).await?;
            crate::projects::refresh(st, p);
        }
    }
    Ok(())
}

/// The connection and model of a work item's latest session.
pub(crate) async fn latest_model(st: &HeadState, id: &str) -> Option<(String, String)> {
    let r = load(st, id).await.ok()??;
    let mut latest: Option<crate::store::SessionRow> = None;
    for a in &r.fields.attached {
        if let Some(sid) = a.reference.strip_prefix("session:")
            && let Ok(Some(s)) = st.store.session(sid).await
            && latest.as_ref().is_none_or(|l| l.updated_at < s.updated_at)
        {
            latest = Some(s);
        }
    }
    latest.map(|s| (s.connection_id, s.model)).filter(|(c, _)| !c.is_empty())
}

/// A work item's next step, from its note.
pub(crate) async fn next_step(st: &HeadState, id: &str) -> Option<String> {
    load(st, id).await.ok()??.fields.note.map(|n| n.next).filter(|n| !n.is_empty())
}

/// The project a work item is in.
pub async fn project_of(st: &HeadState, id: &str) -> Option<String> {
    load(st, id).await.ok().flatten().map(|r| r.fields.project).filter(|p| !p.is_empty())
}

/// The phase of its project a work item is in.
pub async fn phase_of(st: &HeadState, id: &str) -> Option<String> {
    load(st, id).await.ok().flatten().map(|r| r.fields.phase).filter(|p| !p.is_empty())
}

/// A work item's title.
pub async fn title_of(st: &HeadState, id: &str) -> Option<String> {
    load(st, id).await.ok().flatten().map(|r| r.fields.title)
}

/// The ids of the work items in a project (open and closed).
pub async fn in_project(st: &HeadState, project: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM records WHERE collection = ? AND json_extract(fields, '$.project') = ? ORDER BY updated_at DESC")
        .bind(WORK)
        .bind(project)
        .fetch_all(&st.store.pool)
        .await?)
}

/// The work items in a project, as they are now.
pub async fn items_in_project(st: &HeadState, project: &str) -> anyhow::Result<Vec<WorkItem>> {
    let rows = sqlx::query(
        "SELECT * FROM records WHERE collection = ? AND json_extract(fields, '$.project') = ? ORDER BY updated_at DESC LIMIT 1000",
    )
    .bind(WORK)
    .bind(project)
    .fetch_all(&st.store.pool)
    .await?;
    view(st, rows.iter().map(stored).collect()).await
}

/// Whether a work item holds a ref.
pub async fn holds(st: &HeadState, id: &str, reference: &str) -> bool {
    load(st, id).await.ok().flatten().is_some_and(|r| r.fields.attached.iter().any(|a| a.reference == reference))
}

/// The refs a work item holds.
pub async fn refs(st: &HeadState, id: &str) -> Vec<String> {
    load(st, id).await.ok().flatten().map(|r| r.fields.attached.into_iter().map(|a| a.reference).collect()).unwrap_or_default()
}

/// Attaches a ref to a work item as `by` (a session: what it made).
pub async fn attach_by(st: &HeadState, id: &str, r: &WorkRef, by: &str) -> anyhow::Result<()> {
    modify(st, id, by, None, |f| attach_to(f, r, by)).await?;
    refresh(st, id);
    Ok(())
}

/// The open items something is attached to.
pub async fn holding(st: &HeadState, reference: &str) -> anyhow::Result<Vec<String>> {
    holding_in(st, reference, None).await
}

/// The open work each session serves, for the live list: the item that
/// moved last, when it is in several.
pub async fn session_tags(st: &HeadState, realm: Option<&str>) -> anyhow::Result<HashMap<String, api::work::WorkTag>> {
    let rows = sqlx::query(
        "SELECT substr(json_extract(a.value, '$.ref'), 9) AS session, r.id, coalesce(json_extract(r.fields, '$.title'), '') AS title, \
         coalesce(p.id, '') AS project, coalesce(json_extract(p.fields, '$.title'), '') AS project_title \
         FROM records r JOIN json_each(r.fields, '$.attached') a \
         LEFT JOIN records p ON p.collection = ? AND p.realm = r.realm AND p.id = json_extract(r.fields, '$.project') \
         WHERE r.collection = ? AND json_extract(r.fields, '$.status') = 'open' \
         AND (? IS NULL OR r.realm = ?) AND json_extract(a.value, '$.ref') LIKE 'session:%' ORDER BY r.updated_at",
    )
    .bind(crate::projects::PROJECTS)
    .bind(WORK)
    .bind(realm)
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    // Later rows (moved last) win.
    Ok(rows
        .iter()
        .map(|r| {
            let tag = api::work::WorkTag {
                id: r.get("id"),
                title: r.get("title"),
                project: r.get("project"),
                project_title: r.get("project_title"),
            };
            (r.get("session"), tag)
        })
        .collect())
}

async fn holding_in(st: &HeadState, reference: &str, realm: Option<&str>) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT r.id FROM records r WHERE r.collection = ? AND json_extract(r.fields, '$.status') = 'open' \
         AND (? IS NULL OR r.realm = ?) \
         AND EXISTS (SELECT 1 FROM json_each(r.fields, '$.attached') a WHERE json_extract(a.value, '$.ref') = ?) ORDER BY r.updated_at DESC",
    )
    .bind(WORK)
    .bind(realm)
    .bind(realm)
    .bind(reference)
    .fetch_all(&st.store.pool)
    .await?)
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
        return Err(bad_request(format!("work needs a title (up to {MAX_TITLE} characters)")));
    }
    Ok(t.to_owned())
}

/// A ref names what it points at: `session:`, `request:`, `thread:`,
/// `chat:`, `decision:`, or an http(s) URL.
fn check_ref(r: &str) -> anyhow::Result<String> {
    let r = r.trim();
    let ok = ["session:", "request:", "thread:", "chat:", "decision:", "doc:", "http://", "https://"]
        .iter()
        .any(|p| r.strip_prefix(p).is_some_and(|rest| !rest.is_empty()))
        && r.len() <= 2000
        && !r.contains(char::is_whitespace);
    if !ok {
        return Err(bad_request(format!(
            "{r:?}: attach session:<id>, request:<id>, thread:<account>:<thread>, chat:<workspace>:<channel>, decision:<id>, doc:<id> or a URL"
        )));
    }
    Ok(r.to_owned())
}

/// Creates a work item in `realm`.
async fn insert(st: &HeadState, realm: &str, fields: &WorkFields, labels: &[String], by: &str) -> anyhow::Result<String> {
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
    .bind(WORK)
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

/// Changes a work item: reads it, applies `change`, and writes a new
/// version with its history, in one transaction. `change` returns false
/// for nothing to write.
async fn modify(
    st: &HeadState,
    id: &str,
    by: &str,
    labels: Option<Vec<String>>,
    change: impl FnOnce(&mut WorkFields) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let r = sqlx::query("SELECT * FROM records WHERE collection = ? AND id = ?").bind(WORK).bind(id).fetch_optional(&mut *tx).await?;
    let Some(r) = r.as_ref().map(stored) else { return Err(not_found(format!("no work item {id}"))) };
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

/// Something attached moved: when, without a new version (it's not a
/// change you or an agent made).
async fn touch(st: &HeadState, id: &str, at: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE records SET fields = json_set(fields, '$.moved_at', ?) WHERE collection = ? AND id = ? \
         AND coalesce(json_extract(fields, '$.moved_at'), 0) < ?",
    )
    .bind(at)
    .bind(WORK)
    .bind(id)
    .bind(at)
    .execute(&st.store.pool)
    .await?;
    Ok(())
}

fn attach_to(f: &mut WorkFields, r: &WorkRef, by: &str) -> anyhow::Result<bool> {
    let reference = check_ref(&r.reference)?;
    if f.attached.iter().any(|a| a.reference == reference) {
        if let Some(a) = f.attached.iter_mut().find(|a| a.reference == reference && a.title.is_empty() && !r.title.is_empty()) {
            a.title = r.title.trim().to_owned();
            return Ok(true);
        }
        return Ok(false);
    }
    if f.attached.len() >= MAX_ATTACHED {
        return Err(bad_request(format!("a work item holds at most {MAX_ATTACHED} things")));
    }
    let now = now_ms();
    f.attached.push(Attached {
        reference,
        title: r.title.trim().chars().take(300).collect(),
        by: by.to_owned(),
        at: now,
        ..Default::default()
    });
    f.moved_at = f.moved_at.max(now);
    Ok(true)
}

// --- What you do ----------------------------------------------------------

pub async fn create(st: &HeadState, caller: &Caller, w: &api::work::WorkNew, by: &str) -> anyhow::Result<WorkItem> {
    let realm = caller.realm_for_new(None)?;
    let mut f = WorkFields {
        title: check_title(&w.title)?,
        done_when: w.done_when.trim().to_owned(),
        status: "open".into(),
        moved_at: now_ms(),
        ..Default::default()
    };
    for r in &w.attach {
        attach_to(&mut f, r, by)?;
    }
    let labels = crate::sessions::check_labels(&w.labels)?;
    let id = insert(st, &realm, &f, &labels, by).await?;
    refresh(st, &id);
    guess_project(st, &id);
    get(st, &id, caller).await
}

/// New work made for a project (planning, an accepted proposal): no guess.
pub(crate) async fn create_in_project(
    st: &HeadState,
    caller: &Caller,
    w: &api::work::WorkNew,
    project: &str,
    phase: &str,
    by: &str,
) -> anyhow::Result<WorkItem> {
    let realm = caller.realm_for_new(None)?;
    let f = WorkFields {
        title: check_title(&w.title)?,
        done_when: w.done_when.trim().to_owned(),
        status: "open".into(),
        moved_at: now_ms(),
        ..Default::default()
    };
    let id = insert(st, &realm, &f, &crate::sessions::check_labels(&w.labels)?, by).await?;
    set_project(st, &id, &realm, project, phase, by).await?;
    refresh(st, &id);
    get(st, &id, caller).await
}

pub async fn get(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<WorkItem> {
    let r = visible(st, id, caller).await?;
    let mut items = view(st, vec![r]).await?;
    Ok(items.remove(0))
}

/// Open work (with snoozed), or closed work, the way it's listed: by state,
/// then what moved last.
pub async fn list(st: &HeadState, caller: &Caller, closed: bool) -> anyhow::Result<Vec<WorkItem>> {
    let mut items = view(st, rows(st, caller.listing_realm(), closed).await?).await?;
    let rank = |s: &str| api::work::STATES.iter().position(|x| *x == s).unwrap_or(99);
    items.sort_by(|a, b| rank(&a.state).cmp(&rank(&b.state)).then(moved(b).cmp(&moved(a))));
    Ok(items)
}

/// The open work something belongs to.
pub async fn of(st: &HeadState, reference: &str, caller: &Caller) -> anyhow::Result<Vec<WorkItem>> {
    let mut found = Vec::new();
    for id in holding(st, reference).await? {
        if let Some(r) = load(st, &id).await?.filter(|r| caller.sees(&r.realm)) {
            found.push(r);
        }
    }
    view(st, found).await
}

pub async fn patch(st: &HeadState, id: &str, p: &api::work::WorkPatch, caller: &Caller) -> anyhow::Result<WorkItem> {
    visible(st, id, caller).await?;
    let title = p.title.as_deref().map(check_title).transpose()?;
    if let Some(s) = &p.status
        && !matches!(s.as_str(), "open" | "done" | "dropped")
    {
        return Err(bad_request("work is open, done or dropped"));
    }
    // Into or out of a project, and its phase there.
    if p.project.is_some() || p.phase.is_some() {
        let row = visible(st, id, caller).await?;
        let project = p.project.clone().unwrap_or(row.fields.project.clone());
        let phase = p.phase.clone().unwrap_or(row.fields.phase.clone());
        if !project.is_empty() {
            let pr = crate::projects::load(st, &project).await?.filter(|x| x.realm == row.realm);
            let pr = pr.ok_or_else(|| bad_request(format!("no project {project} in realm {}", row.realm)))?;
            if !phase.is_empty() && !pr.fields.phases.iter().any(|x| x.id == phase) {
                return Err(bad_request(format!("the project has no phase {phase}")));
            }
        }
        set_project(st, id, &row.realm, &project, &phase, "you").await?;
    }
    modify(st, id, "you", p.labels.clone(), |f| {
        if let Some(t) = title {
            f.title = t;
        }
        if let Some(d) = &p.done_when {
            f.done_when = d.trim().to_owned();
        }
        if let Some(s) = &p.status {
            if s != &f.status {
                f.closed_at = (s != "open").then(now_ms);
                if s == "open" {
                    f.moved_at = now_ms();
                }
            }
            f.status = s.clone();
            f.offer = None;
        }
        if let Some(until) = p.snooze_until {
            f.snoozed_until = (until > now_ms()).then_some(until);
        }
        if p.dismiss_offer {
            f.offer = None;
        }
        Ok(true)
    })
    .await?;
    // Work opening or closing moves its project.
    if p.status.is_some()
        && let Some(project) = project_of(st, id).await
    {
        crate::projects::refresh(st, &project);
    }
    get(st, id, caller).await
}

pub async fn attach(st: &HeadState, id: &str, r: &WorkRef, caller: &Caller, by: &str) -> anyhow::Result<WorkItem> {
    let row = visible(st, id, caller).await?;
    if let Some(session) = r.reference.strip_prefix("session:")
        && let Some(s) = st.store.session(session).await?
        && s.realm != row.realm
    {
        return Err(bad_request(format!("that session is in realm {}, the work in {}", s.realm, row.realm)));
    }
    modify(st, id, by, None, |f| attach_to(f, r, by)).await?;
    refresh(st, id);
    get(st, id, caller).await
}

pub async fn detach(st: &HeadState, id: &str, r: &WorkRef, caller: &Caller) -> anyhow::Result<WorkItem> {
    visible(st, id, caller).await?;
    modify(st, id, "you", None, |f| {
        let before = f.attached.len();
        f.attached.retain(|a| a.reference != r.reference.trim());
        Ok(f.attached.len() != before)
    })
    .await?;
    get(st, id, caller).await
}

/// `from` joins `id`: what's attached to it, and its done-when if `id` has
/// none; `from` is gone (its history stays).
pub async fn merge(st: &HeadState, id: &str, from: &str, caller: &Caller) -> anyhow::Result<WorkItem> {
    if id == from {
        return Err(bad_request("work is already itself"));
    }
    let into = visible(st, id, caller).await?;
    let other = visible(st, from, caller).await?;
    if into.realm != other.realm {
        return Err(bad_request("work merges only within a realm"));
    }
    modify(st, id, "you", None, |f| {
        let new: Vec<Attached> =
            other.fields.attached.iter().filter(|a| !f.attached.iter().any(|x| x.reference == a.reference)).cloned().collect();
        f.attached.extend(new);
        f.todos.extend(other.fields.todos.iter().cloned());
        if f.done_when.is_empty() {
            f.done_when = other.fields.done_when.clone();
        }
        f.moved_at = f.moved_at.max(other.fields.moved_at);
        Ok(true)
    })
    .await?;
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM records WHERE collection = ? AND id = ?").bind(WORK).bind(from).execute(&mut *tx).await?;
    history(&mut tx, from, other.version + 1, "delete", &serde_json::to_value(&other.fields)?, "you", now_ms()).await?;
    tx.commit().await?;
    refresh(st, id);
    get(st, id, caller).await
}

// --- To-dos -------------------------------------------------------------------

fn new_todo(text: &str, prompt: &str, by: &str) -> anyhow::Result<Todo> {
    Ok(Todo {
        id: uuid::Uuid::now_v7().simple().to_string()[20..].to_owned(),
        text: check_todo_text(text)?,
        prompt: check_todo_prompt(prompt)?,
        by: by.to_owned(),
        at: now_ms(),
        ..Default::default()
    })
}

fn check_todo_text(t: &str) -> anyhow::Result<String> {
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() || t.chars().count() > MAX_TODO_TEXT {
        return Err(bad_request(format!("a to-do is a line of up to {MAX_TODO_TEXT} characters")));
    }
    Ok(t)
}

fn check_todo_prompt(p: &str) -> anyhow::Result<String> {
    let p = p.trim();
    if p.len() > MAX_TODO_PROMPT {
        return Err(bad_request(format!("a to-do's prompt is at most {MAX_TODO_PROMPT} bytes")));
    }
    Ok(p.to_owned())
}

/// Adds a to-do; one with the same text still open is that one.
fn add_todo_to(f: &mut WorkFields, t: Todo) -> anyhow::Result<bool> {
    if f.todos.iter().any(|x| x.done_at.is_none() && x.text.eq_ignore_ascii_case(&t.text)) {
        return Ok(false);
    }
    if f.todos.len() >= MAX_TODOS {
        return Err(bad_request(format!("a piece of work holds at most {MAX_TODOS} to-dos")));
    }
    f.todos.push(t);
    f.moved_at = now_ms();
    Ok(true)
}

fn todo_mut<'a>(f: &'a mut WorkFields, todo: &str) -> anyhow::Result<&'a mut Todo> {
    f.todos.iter_mut().find(|t| t.id == todo).ok_or_else(|| not_found(format!("no to-do {todo} in this work")))
}

/// Ticks a to-do off (or not), saying who and why.
fn tick(t: &mut Todo, done: bool, by: &str, evidence: &str) -> bool {
    if t.done_at.is_some() == done {
        return false;
    }
    t.done_at = done.then(now_ms);
    t.done_by = if done { by.to_owned() } else { String::new() };
    t.evidence = if done { evidence.trim().chars().take(500).collect() } else { String::new() };
    true
}

pub async fn todo_add(st: &HeadState, id: &str, n: &TodoNew, caller: &Caller) -> anyhow::Result<WorkItem> {
    visible(st, id, caller).await?;
    let t = new_todo(&n.text, &n.prompt, "you")?;
    modify(st, id, "you", None, |f| add_todo_to(f, t)).await?;
    get(st, id, caller).await
}

pub async fn todo_edit(st: &HeadState, id: &str, todo: &str, p: &TodoPatch, caller: &Caller) -> anyhow::Result<WorkItem> {
    visible(st, id, caller).await?;
    let text = p.text.as_deref().map(check_todo_text).transpose()?;
    let prompt = p.prompt.as_deref().map(check_todo_prompt).transpose()?;
    let mut ticked = false;
    modify(st, id, "you", None, |f| {
        let t = todo_mut(f, todo)?;
        if let Some(text) = text {
            t.text = text;
        }
        if let Some(prompt) = prompt {
            t.prompt = prompt;
        }
        if let Some(done) = p.done {
            ticked = tick(t, done, "you", "");
        }
        f.moved_at = now_ms();
        Ok(true)
    })
    .await?;
    if ticked {
        refresh(st, id);
    }
    get(st, id, caller).await
}

pub async fn todo_remove(st: &HeadState, id: &str, todo: &str, caller: &Caller) -> anyhow::Result<WorkItem> {
    visible(st, id, caller).await?;
    modify(st, id, "you", None, |f| {
        todo_mut(f, todo)?;
        f.todos.retain(|t| t.id != todo);
        Ok(true)
    })
    .await?;
    get(st, id, caller).await
}

// --- State ------------------------------------------------------------------

fn moved(w: &WorkItem) -> i64 {
    w.live.iter().map(|l| l.at).max().unwrap_or(0).max(w.fields.moved_at)
}

/// Each item as it is now: what's attached, its state and why.
async fn view(st: &HeadState, rows: Vec<Stored>) -> anyhow::Result<Vec<WorkItem>> {
    view_at(st, rows, now_ms()).await
}

/// As they are at `now`.
async fn view_at(st: &HeadState, rows: Vec<Stored>, now: i64) -> anyhow::Result<Vec<WorkItem>> {
    // The memex's requests, per realm, in one call each.
    let mut requests: HashMap<(String, i64), Value> = HashMap::new();
    let mut wanted: HashMap<String, Vec<i64>> = HashMap::new();
    for r in &rows {
        for a in &r.fields.attached {
            if let Some(id) = a.reference.strip_prefix("request:").and_then(|i| i.parse::<i64>().ok()) {
                wanted.entry(r.realm.clone()).or_default().push(id);
            }
        }
    }
    for (realm, ids) in wanted {
        if !crate::memex::has_memex(&realm) {
            continue;
        }
        match crate::memex::call_in(st, &realm, "memex.requests.some", json!({"ids": ids})).await {
            Ok(Value::Array(found)) => {
                for q in found {
                    if let Some(id) = q["id"].as_i64() {
                        requests.insert((realm.clone(), id), q);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => warn!(%realm, "reading requests for work: {e:#}"),
        }
    }
    let mut settings_of: HashMap<String, WorkSettings> = HashMap::new();
    let mut out = Vec::new();
    for r in rows {
        let mut live = Vec::new();
        for a in &r.fields.attached {
            live.push(live_of(st, &r.realm, a, &requests).await?);
        }
        if !settings_of.contains_key(&r.realm) {
            settings_of.insert(r.realm.clone(), settings(st, &r.realm).await);
        }
        let (state, why) = state_of(&r.fields, &live, now, &settings_of[&r.realm]);
        let project_title = match r.fields.project.as_str() {
            "" => String::new(),
            p => crate::projects::title_of(st, &r.realm, p).await.unwrap_or_default(),
        };
        out.push(WorkItem {
            project_title,
            id: r.id,
            realm: r.realm,
            version: r.version,
            fields: r.fields,
            state,
            why,
            live,
            labels: r.labels,
            created_at: r.created_at,
            updated_at: r.updated_at,
        });
    }
    Ok(out)
}

/// One carbon6 thing as it is now (a ref chip in a document): a session,
/// request, decision, document, work item or project of the realm, or a
/// URL as it is.
pub async fn describe(st: &HeadState, realm: &str, reference: &str) -> anyhow::Result<LiveAttached> {
    let mut requests = HashMap::new();
    if let Some(id) = reference.strip_prefix("request:").and_then(|i| i.parse::<i64>().ok())
        && crate::memex::has_memex(realm)
        && let Ok(Value::Array(found)) = crate::memex::call_in(st, realm, "memex.requests.some", json!({"ids": [id]})).await
    {
        for q in found {
            if let Some(id) = q["id"].as_i64() {
                requests.insert((realm.to_owned(), id), q);
            }
        }
    }
    if let Some(id) = reference.strip_prefix("work:") {
        let mut l = LiveAttached { reference: reference.to_owned(), title: reference.to_owned(), state: "gone".into(), at: 0 };
        if let Some(r) = load(st, id).await?.filter(|r| r.realm == realm) {
            let item = view(st, vec![r]).await?.remove(0);
            (l.title, l.state, l.at) = (item.fields.title, item.state, item.fields.moved_at);
        }
        return Ok(l);
    }
    if let Some(id) = reference.strip_prefix("project:") {
        let mut l = LiveAttached { reference: reference.to_owned(), title: reference.to_owned(), state: "gone".into(), at: 0 };
        if let Some(r) = crate::projects::load(st, id).await?.filter(|r| r.realm == realm) {
            let item = crate::projects::view(st, vec![r]).await?.remove(0);
            (l.title, l.state, l.at) = (item.fields.title, item.state, item.fields.moved_at);
        }
        return Ok(l);
    }
    let a = Attached { reference: reference.to_owned(), ..Default::default() };
    live_of(st, realm, &a, &requests).await
}

async fn live_of(st: &HeadState, realm: &str, a: &Attached, requests: &HashMap<(String, i64), Value>) -> anyhow::Result<LiveAttached> {
    let mut l = LiveAttached { reference: a.reference.clone(), title: a.title.clone(), state: a.status.clone(), at: 0 };
    if let Some(id) = a.reference.strip_prefix("session:") {
        match st.store.session(id).await? {
            Some(s) => {
                l.title = if s.title.trim().is_empty() { "Untitled session".into() } else { s.title.clone() };
                l.at = s.updated_at;
                l.state = if s.archived != 0 {
                    "archived".into()
                } else if s.awaiting > 0 || asks(st, id).await? {
                    "needs_you".into()
                } else if matches!(s.status.as_str(), "running" | "starting") {
                    "running".into()
                } else if s.status == "failed" || s.end_reason == "error" {
                    "failed".into()
                } else {
                    "idle".into()
                };
            }
            None => l.state = "gone".into(),
        }
    } else if let Some(id) = a.reference.strip_prefix("request:").and_then(|i| i.parse::<i64>().ok()) {
        match requests.get(&(realm.to_owned(), id)) {
            Some(q) => {
                let who = q["person_name"].as_str().filter(|n| !n.is_empty()).or(q["person"].as_str()).unwrap_or("");
                let title = q["title"].as_str().unwrap_or("");
                l.title = if who.is_empty() { title.to_owned() } else { format!("{who}: {title}") };
                l.state = q["status"].as_str().unwrap_or("").to_owned();
                l.at = q["updated_at"].as_i64().unwrap_or(0);
            }
            None if crate::memex::has_memex(realm) => l.state = "gone".into(),
            None => {}
        }
    } else if let Some(id) = a.reference.strip_prefix("decision:").and_then(|i| i.parse::<i64>().ok())
        && let Some(d) =
            sqlx::query("SELECT title, state, updated_at FROM decisions WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?
    {
        l.title = d.get("title");
        l.state = d.get("state");
        l.at = d.get("updated_at");
    }
    if let Some(id) = a.reference.strip_prefix("doc:") {
        match crate::docs::title_of(st, realm, id).await {
            Some((title, _, at)) => {
                // An agent answered a comment on it: your move.
                if crate::docs::comments::answered(st, realm, id).await > 0 {
                    l.state = "needs_you".into();
                }
                l.title = title;
                l.at = at;
            }
            None => l.state = "gone".into(),
        }
    }
    if l.title.is_empty() {
        l.title = a.reference.clone();
    }
    Ok(l)
}

/// Whether something in a session waits on you: a question, a decision.
async fn asks(st: &HeadState, session_id: &str) -> anyhow::Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM decisions WHERE state = 'open' AND kind != 'task' \
         AND (json_extract(context, '$.session') = ? OR EXISTS (SELECT 1 FROM json_each(decisions.sessions) WHERE value = ?))",
    )
    .bind(session_id)
    .bind(session_id)
    .fetch_one(&st.store.pool)
    .await?;
    Ok(n > 0)
}

/// Its state and why, first that holds: closed; snoozed; something asks
/// you (looks done, a question, a failure, a reply you owe); a session
/// runs; others are on it; nothing moved for a while (gone quiet); an
/// agent finished its turn (your move); else open.
fn state_of(f: &WorkFields, live: &[LiveAttached], now: i64, s: &WorkSettings) -> (String, String) {
    let r = |state: &str, why: String| (state.to_owned(), why);
    if f.status != "open" {
        return r(&f.status, String::new());
    }
    if let Some(until) = f.snoozed_until.filter(|u| *u > now) {
        return r("snoozed", format!("Snoozed until {}", when(until)));
    }
    if let Some(o) = &f.offer {
        return r("needs_you", format!("Looks done: {}", o.evidence));
    }
    let find = |states: &[&str]| live.iter().find(|l| states.contains(&l.state.as_str()));
    if let Some(l) = live.iter().find(|l| l.state == "needs_you") {
        return r("needs_you", format!("{} waits for your answer", l.title));
    }
    if let Some(l) = live.iter().find(|l| l.state == "failed") {
        return r("needs_you", format!("{} failed", l.title));
    }
    if let Some(l) = live.iter().find(|l| l.reference.starts_with("request:") && l.state == "open") {
        return r("needs_you", format!("You owe a reply: {}", l.title));
    }
    if let Some(l) = live.iter().find(|l| l.reference.starts_with("decision:") && l.state == "open") {
        return r("needs_you", format!("Decide: {}", l.title));
    }
    if let Some(l) = live.iter().find(|l| l.reference.starts_with("session:") && l.state == "running") {
        return r("moving", format!("{} is running", l.title));
    }
    if let Some(l) = find(&["waiting", "running"]).filter(|l| !l.reference.starts_with("session:")) {
        return r("waiting", format!("Waiting on {}", l.title));
    }
    if let Some(l) = live.iter().find(|l| l.reference.starts_with("request:") && l.state == "snoozed") {
        return r("waiting", format!("{} is snoozed in the Brief", l.title));
    }
    let last = live.iter().map(|l| l.at).max().unwrap_or(0).max(f.moved_at);
    let quiet = i64::from(s.quiet_days.max(1)) * DAY;
    if now - last > quiet {
        let days = (now - last) / DAY;
        return r("quiet", format!("Nothing has moved for {days} days"));
    }
    if let Some(l) = live.iter().filter(|l| l.reference.starts_with("session:") && l.state == "idle").max_by_key(|l| l.at) {
        return r("needs_you", format!("{} finished its turn: your move", l.title));
    }
    let open: Vec<&Todo> = f.todos.iter().filter(|t| t.done_at.is_none()).collect();
    if let Some(next) = open.first() {
        let left = if open.len() == 1 { "1 to-do left".to_owned() } else { format!("{} of {} to-dos left", open.len(), f.todos.len()) };
        return r("open", format!("{left}: {}", next.text));
    }
    r("open", if f.attached.is_empty() { "Nothing started yet".into() } else { String::new() })
}

// --- Events -----------------------------------------------------------------

/// Something attached moved (a session's turn ended, a request changed, a
/// decision opened or was answered): its work's note is brought up to date.
pub async fn moved_ref(st: &HeadState, reference: &str) {
    moved_ref_in(st, reference, None).await;
}

async fn moved_ref_in(st: &HeadState, reference: &str, realm: Option<&str>) {
    let ids = match holding_in(st, reference, realm).await {
        Ok(ids) => ids,
        Err(e) => {
            warn!(%reference, "finding its work: {e:#}");
            return;
        }
    };
    let now = now_ms();
    for id in ids {
        if let Err(e) = touch(st, &id, now).await {
            warn!(work = %id, "noting that it moved: {e:#}");
        }
        refresh(st, &id);
    }
    if !reference.is_empty() {
        st.registry.bump();
    }
}

/// A session's turn ended or it asks for something (from the triggers'
/// hook). A subagent's or side chat's moves its parent's work.
pub async fn on_session(st: &HeadState, session_id: &str) {
    let target = match st.store.session(session_id).await {
        Ok(Some(s)) => s.root_id.clone().or(s.parent_id.clone()).unwrap_or(s.id),
        _ => return,
    };
    moved_ref(st, &format!("session:{target}")).await;
}

pub async fn on_decision(st: &HeadState, d: &api::Decision) {
    moved_ref(st, &format!("decision:{}", d.id)).await;
    if let Some(s) = d.context["session"].as_str() {
        on_session(st, s).await;
    }
}

/// The memex's events: a request opened, changed or got a new message.
pub async fn on_memex_event(st: &HeadState, realm: &str, kind: &str, data: &Value) {
    if kind.starts_with("request.") {
        let mut ids: Vec<i64> = data["request_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
        ids.extend(data["request_id"].as_i64());
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            // Request ids (including merge redirects) are local to a realm.
            moved_ref_in(st, &format!("request:{id}"), Some(realm)).await;
        }
    }
}

/// A session you started (through the API, not a template, trigger or
/// subagent): attached to the work you chose (`work`: an item's id, "new"
/// or "none"), else to the work that already holds what it was started
/// from (`refs`), else, when asked (`work`: "guess", the new-session
/// form's default), to an agent's guess. Without "guess" no model is
/// called: API and CLI sessions cost what they did. Started on one of an
/// item's to-dos (`todo`), the to-do names it.
pub async fn on_new_session(st: &HeadState, s: &api::Session, work: Option<&str>, refs: &[WorkRef], prompt: &str, todo: Option<&str>) {
    let session = WorkRef { reference: format!("session:{}", s.id), title: s.title.clone() };
    let mut all = vec![session];
    for r in refs {
        if check_ref(&r.reference).is_ok() {
            all.push(r.clone());
        }
    }
    let asked = work.map(str::trim).unwrap_or("");
    let result = match asked {
        "none" => Ok(()),
        "new" => {
            let title = first_line(if s.title.trim().is_empty() { prompt } else { &s.title });
            new_with(st, &s.realm, &title, "", &all, "you").await.map(|_| ())
        }
        "" | "guess" => {
            let mut held = None;
            for r in refs {
                if let Ok(ids) = holding(st, &r.reference).await
                    && let Some(id) = ids.into_iter().next()
                {
                    held = Some(id);
                    break;
                }
            }
            match held {
                Some(id) => add_all(st, &id, &all, "you").await,
                None if asked == "guess" && settings(st, &s.realm).await.guess => {
                    if let Some(arc) = st.arc() {
                        let (s, prompt) = (s.clone(), prompt.to_owned());
                        tokio::spawn(async move {
                            if let Err(e) = guess(&arc, &s, &all, &prompt).await {
                                warn!(session = %s.id, "guessing its work: {e:#}");
                            }
                        });
                    }
                    Ok(())
                }
                None => Ok(()),
            }
        }
        id => match load(st, id).await {
            Ok(Some(r)) if r.realm == s.realm => match add_all(st, id, &all, "you").await {
                Ok(()) => match todo.map(str::trim).filter(|t| !t.is_empty()) {
                    Some(t) => {
                        modify(st, id, "you", None, |f| {
                            todo_mut(f, t)?.session = s.id.clone();
                            Ok(true)
                        })
                        .await
                    }
                    None => Ok(()),
                },
                Err(e) => Err(e),
            },
            _ => Err(bad_request(format!("no work item {id} in realm {}", s.realm))),
        },
    };
    if let Err(e) = result {
        warn!(session = %s.id, "attaching it to work: {e:#}");
    }
}

/// A task card kept as a to-do (docs/task-cards.md §4.1): added to the
/// work its session belongs to (`into_session_work`), else to new work
/// named after it. Returns the work item's id.
pub async fn keep_todo(st: &HeadState, d: &api::Decision, into_session_work: bool) -> anyhow::Result<String> {
    let origin = d.context["session"].as_str().unwrap_or("").to_owned();
    let mut target = None;
    if into_session_work && !origin.is_empty() {
        let root = match st.store.session(&origin).await? {
            Some(s) => s.root_id.clone().or(s.parent_id.clone()).unwrap_or(s.id),
            None => origin.clone(),
        };
        for id in holding(st, &format!("session:{root}")).await? {
            if load(st, &id).await?.is_some_and(|r| r.realm == d.realm) {
                target = Some(id);
                break;
            }
        }
    }
    let mut todo = new_todo(&d.title, d.context["prompt"].as_str().unwrap_or(""), "you")?;
    todo.from = format!("decision:{}", d.id);
    todo.origin = origin;
    let id = match target {
        Some(id) => {
            modify(st, &id, "you", None, |f| add_todo_to(f, todo)).await?;
            id
        }
        None => {
            let mut f = WorkFields { title: check_title(&d.title)?, status: "open".into(), ..Default::default() };
            add_todo_to(&mut f, todo)?;
            insert(st, &d.realm, &f, &[], "you").await?
        }
    };
    Ok(id)
}

/// A time as you read it, in the head's time zone.
fn when(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.format("%a %-d %b %H:%M").to_string()).unwrap_or_default()
}

fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("New work");
    let mut t: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        t.push('…');
    }
    t
}

async fn new_with(st: &HeadState, realm: &str, title: &str, done_when: &str, refs: &[WorkRef], by: &str) -> anyhow::Result<String> {
    let mut f = WorkFields {
        title: check_title(title)?,
        done_when: done_when.trim().to_owned(),
        status: "open".into(),
        moved_at: now_ms(),
        ..Default::default()
    };
    for r in refs {
        attach_to(&mut f, r, by)?;
    }
    let id = insert(st, realm, &f, &[], by).await?;
    refresh(st, &id);
    guess_project(st, &id);
    Ok(id)
}

async fn add_all(st: &HeadState, id: &str, refs: &[WorkRef], by: &str) -> anyhow::Result<()> {
    modify(st, id, by, None, |f| {
        let mut changed = false;
        for r in refs {
            changed |= attach_to(f, r, by)?;
        }
        Ok(changed)
    })
    .await?;
    refresh(st, id);
    Ok(())
}

// --- Agents' steps ----------------------------------------------------------

/// The connection and model for work in a realm: a routing rule for
/// `task:work` (the realm's, then root's), else `fallback` (a session's).
async fn model_for(st: &HeadState, realm: &str, fallback: Option<(String, String)>) -> anyhow::Result<Option<(String, String)>> {
    model_for_route(st, realm, ROUTE_LABEL, fallback).await
}

/// The connection and model a routing rule for `label` picks (the realm's,
/// then root's), else `fallback`.
pub(crate) async fn model_for_route(
    st: &HeadState,
    realm: &str,
    label: &str,
    fallback: Option<(String, String)>,
) -> anyhow::Result<Option<(String, String)>> {
    let route_label = label;
    let connections = st.store.connections().await?;
    let labels = vec![route_label.to_owned()];
    for r in [realm, crate::realms::ROOT] {
        let routes: Vec<api::Route> =
            serde_json::from_str(&st.store.setting_for(crate::sessions::ROUTES, r).await?.unwrap_or_default()).unwrap_or_default();
        for route in routes.iter().filter(|x| x.enabled && x.applies_to.iter().any(|l| l == route_label)) {
            if !api::labels::applies(&route.applies_to, &labels) {
                continue;
            }
            let want = route.connection.trim();
            let connection = if want.is_empty() {
                fallback.as_ref().map(|f| f.0.clone())
            } else {
                connections.iter().find(|c| c.id == want || c.name == want).map(|c| c.id.clone())
            };
            if let Some(c) = connection {
                let model = route.model.trim();
                let model = if model.is_empty() { fallback.as_ref().map(|f| f.1.clone()).unwrap_or_default() } else { model.to_owned() };
                return Ok(Some((c, model)));
            }
        }
    }
    Ok(fallback.filter(|(c, _)| !c.is_empty()))
}

/// Asks the model, for JSON. Its tokens count in the realm's usage, as
/// `work:<about>`.
async fn ask(st: &HeadState, realm: &str, model: (String, String), what: &str, about: &str, prompt: String) -> anyhow::Result<Value> {
    ask_as(st, realm, model, "work", what, about, prompt).await
}

/// Asks the model for JSON; its tokens count in the realm's usage as
/// `<area>:<what>`.
pub(crate) async fn ask_as(
    st: &HeadState,
    realm: &str,
    (connection, model): (String, String),
    area: &str,
    what: &str,
    about: &str,
    prompt: String,
) -> anyhow::Result<Value> {
    let origin = c6_llm::activity::Origin {
        what: what.into(),
        about: about.chars().take(80).collect(),
        connection: connection.clone(),
        realm: realm.to_owned(),
        via: String::new(),
    };
    let client = crate::pools::client(st, &connection, realm, origin).await?;
    let req = ModelRequest {
        model: model.clone(),
        system: String::new(),
        messages: vec![Message { role: Role::User, blocks: vec![Block::Text(prompt)] }],
        tools: Vec::new(),
        // Room for a thinking model's reasoning (more for a project's
        // note, which reads all of its work).
        max_output_tokens: Some(if area == "projects" { 8192 } else { 2048 }),
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
    sqlx::query(
        "INSERT OR REPLACE INTO usage (session_id, seq, ts, connection_id, model, input, output, cache_read, cache_write, realm) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(format!("{area}:{what}"))
    .bind(-now_ms())
    .bind(now_ms())
    .bind(&connection)
    .bind(&model)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_read_tokens as i64)
    .bind(usage.cache_write_tokens as i64)
    .bind(realm)
    .execute(&st.store.pool)
    .await?;
    let text: String = msg.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect();
    json_of(&text).ok_or_else(|| anyhow::anyhow!("{model} gave no JSON: {}", text.chars().take(200).collect::<String>()))
}

/// The JSON object in a model's answer (fenced, or with words around it).
fn json_of(text: &str) -> Option<Value> {
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    (start < end).then(|| serde_json::from_str(&text[start..=end]).ok()).flatten()
}

/// Which work a new session belongs to, by `work.attach`.
async fn guess(st: &HeadState, s: &api::Session, refs: &[WorkRef], prompt: &str) -> anyhow::Result<()> {
    let Some(model) = model_for(st, &s.realm, Some((s.connection_id.clone(), s.model.clone()))).await? else { return Ok(()) };
    let open = rows(st, Some(&s.realm), false).await?;
    let lines: Vec<String> = open
        .iter()
        .take(60)
        .map(|r| {
            let next = r.fields.note.as_ref().map(|n| n.next.as_str()).filter(|n| !n.is_empty()).unwrap_or(&r.fields.done_when);
            format!("{}: {} — {}", r.id, r.fields.title, next)
        })
        .collect();
    let mut about = format!("Title: {}\nFolder: {}\n", s.title, s.cwd);
    if refs.len() > 1 {
        about.push_str(&format!("Started from: {}\n", refs[1..].iter().map(|r| r.title.as_str()).collect::<Vec<_>>().join("; ")));
    }
    about.push_str(&format!("First message: {}", prompt.chars().take(4000).collect::<String>()));
    let template = crate::prompts::get(st, "work.attach", &s.realm).await;
    let open_text = if lines.is_empty() { "(none yet)".to_owned() } else { lines.join("\n") };
    let ask_text = api::prompts::fill(&template, &[("session", &about), ("open", &open_text)]);
    let v = ask(st, &s.realm, model, "work: which work", &s.title, ask_text).await?;
    if let Some(id) = v["work"].as_str()
        && open.iter().any(|r| r.id == id)
    {
        return add_all(st, id, refs, "guess").await;
    }
    if let Some(title) = v["new"].as_str().filter(|t| !t.trim().is_empty()) {
        new_with(st, &s.realm, title, v["done_when"].as_str().unwrap_or(""), refs, "guess").await?;
    }
    Ok(())
}

/// Guesses a new work item's project (docs/projects.md §5), in the
/// background, by `project.attach`: only when the realm has active
/// projects and lets agents pick, and only if nobody has put it in one by
/// the time the answer comes.
pub fn guess_project(st: &HeadState, id: &str) {
    let Some(arc) = st.arc() else { return };
    let id = id.to_owned();
    GUESSING.lock().unwrap().insert(id.clone());
    tokio::spawn(async move {
        if let Err(e) = project_guess(&arc, &id).await {
            warn!(work = %id, "guessing its project: {e:#}");
        }
        GUESSING.lock().unwrap().remove(&id);
    });
}

static GUESSING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Whether a project guess is under way (for tests).
pub fn guessing() -> bool {
    !GUESSING.lock().unwrap().is_empty()
}

async fn project_guess(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let Some(r) = load(st, id).await? else { return Ok(()) };
    if !r.fields.project.is_empty() || !settings(st, &r.realm).await.guess {
        return Ok(());
    }
    let projects = crate::projects::active_in(st, &r.realm).await?;
    if projects.is_empty() {
        return Ok(());
    }
    let fallback = latest_model(st, id).await;
    let model = match model_for_route(st, &r.realm, crate::projects::ROUTE_LABEL, None).await? {
        Some(m) => m,
        None => match model_for_route(st, &r.realm, ROUTE_LABEL, fallback).await? {
            Some(m) => m,
            None => return Ok(()),
        },
    };
    let mut work =
        format!("{}\nDone when: {}", r.fields.title, if r.fields.done_when.is_empty() { "(not said)" } else { &r.fields.done_when });
    let from: Vec<&str> =
        r.fields.attached.iter().map(|a| if a.title.is_empty() { a.reference.as_str() } else { a.title.as_str() }).collect();
    if !from.is_empty() {
        work.push_str(&format!("\nMade from: {}", from.join("; ")));
    }
    let lines: Vec<String> = projects
        .iter()
        .map(|p| {
            let phase = p.fields.phases.iter().find(|x| x.state == "current").map(|x| x.title.as_str()).unwrap_or("no phases");
            format!("{}: {}: {}; {phase}", p.id, p.fields.title, p.fields.purpose.replace('\n', " "))
        })
        .collect();
    let template = crate::prompts::get(st, "project.attach", &r.realm).await;
    let text = api::prompts::fill(&template, &[("work", &work), ("projects", &lines.join("\n"))]);
    let v = ask_as(st, &r.realm, model, "projects", "project guess", &r.fields.title, text).await?;
    let Some(pid) = v["project"].as_str().filter(|p| projects.iter().any(|x| x.id == *p)) else { return Ok(()) };
    // Someone put it somewhere meanwhile: theirs stands.
    if load(st, id).await?.is_some_and(|r| r.fields.project.is_empty()) {
        set_project(st, id, &r.realm, pid, "", "agent:project.attach").await?;
    }
    Ok(())
}

/// Items whose note is being written, and those to write again after.
static WRITING: LazyLock<Mutex<(HashSet<String>, HashSet<String>)>> = LazyLock::new(Default::default);

/// Brings an item's note up to date, in the background; one at a time per
/// item (a change while it's written writes it again after).
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
                warn!(work = %id, "writing its note: {e:#}");
            }
            let mut w = WRITING.lock().unwrap();
            if !w.1.remove(&id) {
                w.0.remove(&id);
                return;
            }
        }
    });
}

/// Whether a note is being written for any item (for tests).
pub fn writing() -> bool {
    !WRITING.lock().unwrap().0.is_empty()
}

/// Writes "where was I", and an offer if it looks done, by `work.summary`.
async fn note(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let Some(r) = load(st, id).await? else { return Ok(()) };
    if r.fields.status != "open" || r.fields.attached.is_empty() || !settings(st, &r.realm).await.notes {
        return Ok(());
    }
    // The model: the rule's, else that of the session that moved last.
    let mut latest: Option<crate::store::SessionRow> = None;
    for a in &r.fields.attached {
        if let Some(sid) = a.reference.strip_prefix("session:")
            && let Some(s) = st.store.session(sid).await?
            && latest.as_ref().is_none_or(|l| l.updated_at < s.updated_at)
        {
            latest = Some(s);
        }
    }
    let fallback = latest.as_ref().map(|s| (s.connection_id.clone(), s.model.clone()));
    let Some(model) = model_for(st, &r.realm, fallback).await? else { return Ok(()) };
    let item = view(st, vec![r]).await?.remove(0);
    let attached = described(st, &item).await?;
    let previous = match &item.fields.note {
        Some(n) => format!("Done: {}\nLeft: {}\nNext: {}", n.done, n.left, n.next),
        None => "(none yet)".into(),
    };
    let template = crate::prompts::get(st, "work.summary", &item.realm).await;
    let prompt = api::prompts::fill(
        &template,
        &[("title", &item.fields.title), ("done_when", &item.fields.done_when), ("note", &previous), ("attached", &attached)],
    );
    let v = ask(st, &item.realm, model, "work: where was I", &item.fields.title, prompt).await?;
    let s = |k: &str| v[k].as_str().unwrap_or("").trim().chars().take(1000).collect::<String>();
    let note = WorkNote { done: s("done"), left: s("left"), next: s("next"), at: now_ms() };
    let offer = (v["finished"] == true && !s("evidence").is_empty()).then(|| WorkOffer { evidence: s("evidence"), at: now_ms() });
    let by = "agent:work.summary";
    modify(st, id, by, None, |f| {
        if f.status != "open" {
            return Ok(false);
        }
        f.note = Some(note);
        // An offer you dismissed isn't made again for the same evidence,
        // and none is made while a to-do is left.
        if let Some(o) = offer
            && f.offer.as_ref().is_none_or(|x| x.evidence != o.evidence)
            && f.todos.iter().all(|t| t.done_at.is_some())
        {
            f.offer = Some(o);
        }
        Ok(true)
    })
    .await
}

/// What's attached, for the note: each thing as it is now, and for
/// sessions their last exchanges.
async fn described(st: &HeadState, item: &WorkItem) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut sessions: Vec<&LiveAttached> = item.live.iter().filter(|l| l.reference.starts_with("session:")).collect();
    sessions.sort_by_key(|l| -l.at);
    for l in &item.live {
        let when = if l.at > 0 { format!(", {}", when(l.at)) } else { String::new() };
        out.push_str(&format!("- {} ({}{when}): {}\n", l.reference, if l.state.is_empty() { "linked" } else { &l.state }, l.title));
        if let Some(a) = item.fields.attached.iter().find(|a| a.reference == l.reference && !a.detail.is_empty()) {
            out.push_str(&format!("  {}\n", a.detail));
        }
    }
    if !item.fields.todos.is_empty() {
        out.push_str(&format!("\nTo-dos ([x] done, [ ] left):\n{}\n", checklist(&item.fields.todos)));
    }
    // The newest sessions' last exchanges, while there's room.
    for l in sessions.iter().take(4) {
        let Some(id) = l.reference.strip_prefix("session:") else { continue };
        let Some(s) = st.store.session(id).await? else { continue };
        let ex = crate::suggestions::exchanges(st, id, s.last_seq as u64).await.unwrap_or_default();
        if ex.trim().is_empty() || out.len() + ex.len() > MAX_INPUT_CHARS {
            continue;
        }
        out.push_str(&format!("\nLately in {} ({}):\n{}\n", l.title, l.reference, ex.trim()));
    }
    Ok(out)
}

/// The first message of a session that picks the work up again, by
/// `work.pickup`.
pub async fn pickup(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<String> {
    let w = get(st, id, caller).await?;
    let note = match &w.fields.note {
        Some(n) => format!("Done: {}\nLeft: {}\nNext: {}", n.done, n.left, n.next),
        None => "(no note yet)".into(),
    };
    let attached: Vec<String> = w
        .live
        .iter()
        .map(|l| format!("- {} ({}{})", l.title, l.reference, if l.state.is_empty() { String::new() } else { format!(", {}", l.state) }))
        .collect();
    let mut attached = if attached.is_empty() { "(nothing yet)".to_owned() } else { attached.join("\n") };
    if !w.fields.todos.is_empty() {
        attached.push_str(&format!("\n\nTo-dos:\n{}", checklist(&w.fields.todos)));
    }
    let done_when = if w.fields.done_when.is_empty() { "(not said)" } else { &w.fields.done_when };
    let template = crate::prompts::get(st, "work.pickup", &w.realm).await;
    Ok(api::prompts::fill(&template, &[("title", &w.fields.title), ("done_when", done_when), ("note", &note), ("attached", &attached)]))
}

/// The to-dos as a checklist, for agents to read.
fn checklist(todos: &[Todo]) -> String {
    todos
        .iter()
        .map(|t| {
            let mark = if t.done_at.is_some() { "x" } else { " " };
            let session = if t.session.is_empty() { String::new() } else { format!(" (started in session:{})", t.session) };
            format!("- [{mark}] {}{session} [{}]", t.text, t.id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A to-do's text, if `work` holds it.
pub async fn todo_text(st: &HeadState, work: &str, todo: &str) -> Option<String> {
    load(st, work).await.ok().flatten()?.fields.todos.into_iter().find(|t| t.id == todo).map(|t| t.text)
}

/// A session's first message for a to-do (by `work.todo`), and the
/// session to start from: the one that raised it, else the work's latest.
pub async fn todo_start(st: &HeadState, id: &str, todo: &str, caller: &Caller) -> anyhow::Result<TodoStart> {
    let w = get(st, id, caller).await?;
    let t = w.fields.todos.iter().find(|t| t.id == todo).ok_or_else(|| not_found(format!("no to-do {todo} in this work")))?;
    let note = match &w.fields.note {
        Some(n) => format!("Done: {}\nLeft: {}\nNext: {}", n.done, n.left, n.next),
        None => "(no note yet)".into(),
    };
    let what = if t.prompt.trim().is_empty() { &t.text } else { &t.prompt };
    let done_when = if w.fields.done_when.is_empty() { "(not said)" } else { &w.fields.done_when };
    let template = crate::prompts::get(st, "work.todo", &w.realm).await;
    let text = api::prompts::fill(
        &template,
        &[("todo", what.trim()), ("id", &t.id), ("title", &w.fields.title), ("done_when", done_when), ("note", &note)],
    );
    let latest = w.live.iter().filter(|l| l.reference.starts_with("session:") && l.state != "gone").max_by_key(|l| l.at);
    let mut seed = None;
    for s in [t.origin.as_str(), latest.and_then(|l| l.reference.strip_prefix("session:")).unwrap_or("")] {
        if !s.is_empty() && st.store.session(s).await?.is_some() {
            seed = Some(s.to_owned());
            break;
        }
    }
    Ok(TodoStart { text, seed })
}

/// Groups what's open into work, by `work.propose`: top-level sessions of
/// the last two weeks and open requests that belong to no work yet.
pub async fn propose(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<WorkProposal>> {
    let realm = caller.realm_for_new(None)?;
    let since = now_ms() - 14 * DAY;
    let sessions = sqlx::query(
        "SELECT id, title, connection_id, model FROM sessions WHERE realm = ? AND parent_id IS NULL AND archived = 0 AND instance = '' \
         AND observed = 0 AND updated_at > ? ORDER BY updated_at DESC LIMIT 150",
    )
    .bind(&realm)
    .bind(since)
    .fetch_all(&st.store.pool)
    .await?;
    let held: HashSet<String> =
        rows(st, Some(&realm), false).await?.iter().flat_map(|r| r.fields.attached.iter().map(|a| a.reference.clone())).collect();
    let mut things = Vec::new();
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut fallback = None;
    for s in &sessions {
        let id: String = s.get("id");
        let r = format!("session:{id}");
        if held.contains(&r) {
            continue;
        }
        if fallback.is_none() {
            fallback = Some((s.get::<String, _>("connection_id"), s.get::<String, _>("model")));
        }
        let title: String = s.get("title");
        let first = st
            .store
            .events_after(&id, 0, 20)
            .await
            .unwrap_or_default()
            .into_iter()
            .find_map(|e| match e.kind {
                Some(c6_proto::session_event::Kind::User(u)) => Some(u.text),
                _ => None,
            })
            .unwrap_or_default();
        things.push(format!("{r} — session \"{title}\": {}", first.chars().take(300).collect::<String>().replace('\n', " ")));
        titles.insert(r, title);
    }
    if crate::memex::has_memex(&realm)
        && let Ok(Value::Array(open)) =
            crate::memex::call_in(st, &realm, "memex.requests.list", json!({"status": "open", "limit": 100})).await
    {
        for q in open {
            let r = format!("request:{}", q["id"].as_i64().unwrap_or(0));
            if held.contains(&r) {
                continue;
            }
            let title = format!("{}: {}", q["person_name"].as_str().unwrap_or(""), q["title"].as_str().unwrap_or(""));
            things.push(format!("{r} — request from {title}"));
            titles.insert(r, title);
        }
    }
    if things.is_empty() {
        return Ok(Vec::new());
    }
    let Some(model) = model_for(st, &realm, fallback).await? else {
        return Err(bad_request("no model to ask: start a session first, or add a routing rule for task:work"));
    };
    let template = crate::prompts::get(st, "work.propose", &realm).await;
    let prompt = api::prompts::fill(&template, &[("things", &things.join("\n"))]);
    // You asked: what the model said goes back to you, not "internal error".
    let v = ask(st, &realm, model, "work: what's open", "what's open", prompt)
        .await
        .map_err(|e| bad_request(format!("the model couldn't group what's open: {e:#}")))?;
    let mut used = HashSet::new();
    let mut out = Vec::new();
    for w in v["work"].as_array().into_iter().flatten() {
        let title = w["title"].as_str().unwrap_or("").trim();
        let attach: Vec<WorkRef> = w["refs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|r| titles.contains_key(*r) && used.insert(r.to_string()))
            .map(|r| WorkRef { reference: r.to_owned(), title: titles[r].clone() })
            .collect();
        if !title.is_empty() && !attach.is_empty() {
            out.push(WorkProposal {
                title: title.chars().take(MAX_TITLE).collect(),
                done_when: w["done_when"].as_str().unwrap_or("").to_owned(),
                attach,
            });
        }
    }
    Ok(out)
}

// --- Nudges -----------------------------------------------------------------

/// Tells you about work that went quiet, or has needed you for a while,
/// once per state it reaches (where its realm's setting allows).
pub async fn sweep(st: &HeadState, now: i64) -> anyhow::Result<usize> {
    let items = view_at(st, rows(st, None, false).await?, now).await?;
    let mut told = 0;
    let mut settings_of: HashMap<String, WorkSettings> = HashMap::new();
    for w in items {
        if !settings_of.contains_key(&w.realm) {
            settings_of.insert(w.realm.clone(), settings(st, &w.realm).await);
        }
        let s = &settings_of[&w.realm];
        let since = moved(&w);
        let due = match w.state.as_str() {
            "quiet" => true,
            "needs_you" => now - since > i64::from(s.needs_hours.max(1)) * 3600 * 1000,
            _ => false,
        };
        // Once per state, until it moves again.
        let already = w.fields.told.as_ref().is_some_and(|t| t.state == w.state && t.at >= since);
        if !s.nudge || !due || already {
            continue;
        }
        let title = match w.state.as_str() {
            "quiet" => format!("Gone quiet: {}", w.fields.title),
            _ => format!("Still needs you: {}", w.fields.title),
        };
        let next = w.fields.note.as_ref().map(|n| n.next.clone()).filter(|n| !n.is_empty());
        st.notifier.send(crate::notify::Note {
            event: "work",
            title,
            message: match next {
                Some(n) => format!("{}. Next: {n}", w.why),
                None => w.why.clone(),
            },
            priority: "normal",
            labels: w.labels.clone(),
            path: Some(format!("/r/{}/work/{}", w.realm, w.id)),
            ..Default::default()
        });
        let state = w.state.clone();
        sqlx::query("UPDATE records SET fields = json_set(fields, '$.told', json(?)) WHERE collection = ? AND id = ?")
            .bind(serde_json::to_string(&Told { state, at: now })?)
            .bind(WORK)
            .bind(&w.id)
            .execute(&st.store.pool)
            .await?;
        told += 1;
    }
    Ok(told)
}

pub fn spawn_sweeper(st: std::sync::Arc<HeadState>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
            }
            if let Err(e) = sweep(&st, now_ms()).await {
                warn!("nudging about work: {e:#}");
            }
            if let Err(e) = crate::projects::sweep(&st).await {
                warn!("nudging about projects: {e:#}");
            }
        }
    });
}

// --- Sessions' tool -----------------------------------------------------------

/// The `work` tool: what the session's work is, attaching what it made
/// (a pull request, a CI run, a document) with how it stands.
pub async fn for_session(st: &HeadState, session_id: &str, p: &Value) -> anyhow::Result<Value> {
    let ids = holding(st, &format!("session:{session_id}")).await?;
    if ids.is_empty() {
        return Ok(json!({"work": [], "note": "This session belongs to no work item; the user can add it to one in its header."}));
    }
    let by = format!("session:{session_id}");
    match p["action"].as_str().unwrap_or("get") {
        "get" => {}
        // What it made, for the project (docs/projects.md §8): it outlives the work.
        "attach" if p["to"] == "project" => {
            let reference = p["ref"].as_str().unwrap_or_default().trim().to_owned();
            let mut done = false;
            for id in &ids {
                let Some(project) = project_of(st, id).await else { continue };
                let Some(row) = crate::projects::load(st, &project).await? else { continue };
                if let Some(doc) = reference.strip_prefix("doc:") {
                    let role = p["role"].as_str().unwrap_or("").to_owned();
                    crate::projects::add_doc_in(st, &row, &c6_api::projects::ProjectDocIn { doc: doc.to_owned(), role }, &by).await?;
                } else {
                    let l = c6_api::projects::ProjectLinkIn {
                        reference: reference.clone(),
                        title: p["title"].as_str().unwrap_or("").to_owned(),
                    };
                    crate::projects::add_link_in(st, &row, &l, &by).await?;
                }
                done = true;
                break;
            }
            if !done {
                return Err(bad_request("this session's work is in no project: attach it to the work instead"));
            }
        }
        "attach" => {
            let reference = check_ref(p["ref"].as_str().unwrap_or_default())?;
            let status = p["status"].as_str().unwrap_or("").trim().to_owned();
            if !status.is_empty() && !matches!(status.as_str(), "running" | "waiting" | "done" | "failed") {
                return Err(bad_request("status is running, waiting, done or failed"));
            }
            let title: String = p["title"].as_str().unwrap_or("").trim().chars().take(300).collect();
            let detail: String = p["detail"].as_str().unwrap_or("").trim().chars().take(500).collect();
            for id in &ids {
                modify(st, id, &by, None, |f| {
                    let r = WorkRef { reference: reference.clone(), title: title.clone() };
                    attach_to(f, &r, &by)?;
                    if let Some(a) = f.attached.iter_mut().find(|a| a.reference == reference) {
                        if !title.is_empty() {
                            a.title = title.clone();
                        }
                        a.status = status.clone();
                        a.detail = detail.clone();
                    }
                    f.moved_at = now_ms();
                    Ok(true)
                })
                .await?;
                refresh(st, id);
            }
        }
        // A piece of work for its project, for you to accept (planning a phase).
        "propose" => {
            let title = p["title"].as_str().unwrap_or("").trim();
            if title.is_empty() {
                return Err(bad_request("propose takes the work's title, and done_when"));
            }
            let mut made = None;
            for id in &ids {
                if project_of(st, id).await.is_some() {
                    made = Some(crate::projects::propose(st, id, title, p["done_when"].as_str().unwrap_or(""), &by).await?);
                    break;
                }
            }
            let made = made.ok_or_else(|| bad_request("propose is for work that is in a project; this session's isn't"))?;
            return Ok(json!({"did": "proposed", "proposal": made, "note": "The user accepts or dismisses it on the project's page."}));
        }
        "todo" => {
            let mut t = new_todo(p["text"].as_str().unwrap_or(""), p["prompt"].as_str().unwrap_or(""), &by)?;
            t.origin = session_id.to_owned();
            // One piece of work holds it: the one updated last.
            modify(st, &ids[0], &by, None, |f| add_todo_to(f, t)).await?;
        }
        "todo_done" => {
            let todo = p["id"].as_str().unwrap_or("").trim().to_owned();
            let evidence = p["evidence"].as_str().unwrap_or("").trim().to_owned();
            if todo.is_empty() || evidence.is_empty() {
                return Err(bad_request("todo_done takes the to-do's id and the evidence that it's done"));
            }
            let mut found = None;
            for id in &ids {
                if load(st, id).await?.is_some_and(|r| r.fields.todos.iter().any(|t| t.id == todo)) {
                    found = Some(id.clone());
                    break;
                }
            }
            let id = found.ok_or_else(|| not_found(format!("no to-do {todo} in this session's work")))?;
            modify(st, &id, &by, None, |f| {
                let ticked = tick(todo_mut(f, &todo)?, true, &by, &evidence);
                if ticked {
                    f.moved_at = now_ms();
                }
                Ok(ticked)
            })
            .await?;
            refresh(st, &id);
        }
        other => return Err(bad_request(format!("work's actions are get, attach, todo, todo_done and propose, not {other:?}"))),
    }
    let mut out = Vec::new();
    for id in ids {
        if let Some(r) = load(st, &id).await? {
            let f = r.fields;
            let project = match f.project.as_str() {
                "" => Value::Null,
                pid => match crate::projects::load(st, pid).await? {
                    Some(p) => {
                        let mut docs = Vec::new();
                        for d in &p.fields.documents {
                            if let Some((title, kind, _)) = crate::docs::title_of(st, &p.realm, &d.doc).await {
                                docs.push(json!({"ref": format!("doc:{}", d.doc), "title": title, "kind": kind, "role": d.role}));
                            }
                        }
                        json!({"id": p.id, "title": p.fields.title, "purpose": p.fields.purpose,
                               "where_we_are": p.fields.note.map(|n| n.text), "documents": docs})
                    }
                    None => Value::Null,
                },
            };
            out.push(json!({
                "id": r.id, "title": f.title, "done_when": f.done_when, "note": f.note, "project": project,
                "attached": f.attached.iter().map(|a| json!({"ref": a.reference, "title": a.title, "status": a.status})).collect::<Vec<_>>(),
                "todos": f.todos.iter().map(|t| json!({"id": t.id, "text": t.text, "done": t.done_at.is_some(), "session": t.session}))
                    .collect::<Vec<_>>(),
            }));
        }
    }
    Ok(json!({"work": out}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(reference: &str, state: &str, at: i64) -> LiveAttached {
        LiveAttached { reference: reference.into(), title: reference.into(), state: state.into(), at }
    }

    #[test]
    fn its_state_comes_from_what_is_attached() {
        let s = WorkSettings::default();
        let now = 100 * DAY;
        let f = WorkFields { title: "x".into(), status: "open".into(), moved_at: now - 1000, ..Default::default() };
        let st = |l: &[LiveAttached]| state_of(&f, l, now, &s).0;
        assert_eq!(st(&[]), "open");
        assert_eq!(st(&[live("session:a", "running", now)]), "moving");
        assert_eq!(st(&[live("session:a", "idle", now)]), "needs_you");
        assert_eq!(st(&[live("session:a", "running", now), live("session:b", "needs_you", now)]), "needs_you");
        assert_eq!(st(&[live("https://pr", "waiting", 0), live("session:a", "idle", now)]), "waiting");
        assert_eq!(st(&[live("request:1", "open", now)]), "needs_you");
        assert_eq!(st(&[live("request:1", "done", now)]), "open");
        // To-dos left: open, saying how many and the next.
        let mut a = new_todo("Bump  the lockfile", "", "you").unwrap();
        let b = new_todo("Split the config", "Split src/config.rs.", "you").unwrap();
        assert_eq!(a.text, "Bump the lockfile");
        let mut listed = f.clone();
        assert!(add_todo_to(&mut listed, a.clone()).unwrap() && add_todo_to(&mut listed, b.clone()).unwrap());
        assert!(!add_todo_to(&mut listed, new_todo("bump the lockfile", "", "you").unwrap()).unwrap(), "the same open to-do");
        assert_eq!(state_of(&listed, &[], now, &s), ("open".into(), "2 of 2 to-dos left: Bump the lockfile".into()));
        assert!(tick(&mut a, true, "session:x", "cargo update ran") && !tick(&mut a, true, "you", ""));
        assert_eq!((a.done_by.as_str(), a.evidence.as_str()), ("session:x", "cargo update ran"));
        listed.todos[0] = a;
        assert_eq!(state_of(&listed, &[], now, &s), ("open".into(), "1 to-do left: Split the config".into()));
        assert_eq!(
            checklist(&listed.todos),
            format!("- [x] Bump the lockfile [{}]\n- [ ] Split the config [{}]", listed.todos[0].id, b.id)
        );
        assert!(new_todo(" ", "", "you").is_err() && new_todo(&"x".repeat(MAX_TODO_TEXT + 1), "", "you").is_err());
        // Nothing moved for longer than the setting: gone quiet, even with
        // a finished turn.
        let old = WorkFields { moved_at: now - 4 * DAY, ..f.clone() };
        assert_eq!(state_of(&old, &[live("session:a", "idle", now - 4 * DAY)], now, &s).0, "quiet");
        // But a question still asks.
        assert_eq!(state_of(&old, &[live("session:a", "needs_you", now - 4 * DAY)], now, &s).0, "needs_you");
        let offered = WorkFields { offer: Some(WorkOffer { evidence: "merged".into(), at: now }), ..f.clone() };
        assert_eq!(state_of(&offered, &[], now, &s), ("needs_you".into(), "Looks done: merged".into()));
        let snoozed = WorkFields { snoozed_until: Some(now + DAY), ..f.clone() };
        assert_eq!(state_of(&snoozed, &[live("session:a", "idle", now)], now, &s).0, "snoozed");
        let done = WorkFields { status: "done".into(), ..f };
        assert_eq!(state_of(&done, &[live("session:a", "running", now)], now, &s).0, "done");
    }

    #[test]
    fn refs_name_what_they_point_at() {
        for ok in [
            "doc:01a18f50",
            "session:abc",
            "request:12",
            "thread:me@x.com:t1",
            "chat:T1:C2:1.2",
            "decision:4",
            "https://github.com/o/r/pull/1",
        ] {
            assert!(check_ref(ok).is_ok(), "{ok}");
        }
        for bad in ["", "session:", "ftp://x", "a b", "notes"] {
            assert!(check_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn json_in_an_answer() {
        assert_eq!(json_of("```json\n{\"work\": \"a\"}\n```"), Some(json!({"work": "a"})));
        assert_eq!(json_of("no"), None);
    }
}
