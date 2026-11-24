//! Agents act like you (docs/agent-actions.md): a session's agent runs the
//! catalog's operations in its realm, as `session:<id>`. Reading and making
//! things is free; changing what it (or its family: root, side chats,
//! subagents) made is free; changing anything else needs your OK once per
//! thing; deleting what isn't its own needs your OK each time; and
//! changing the session's place always needs your OK for that exact move;
//! and administration is yours.

use c6_core::now_ms;
use serde_json::{Value, json};

use crate::HeadState;
use crate::catalog::{self, Effect, Op, Via};

const CHANGE_PLACE: &str = "sessions.place.change";
const PLACE_APPROVAL_MS: i64 = 60 * 60 * 1000;
const PLACE_SUBMISSION_MS: i64 = 5 * 60 * 1000;
const PLACE_SUBMISSION_INTERRUPTED: &str =
    "The approved placement submission was interrupted. No move was queued; ask for a new approval to retry.";

tokio::task_local! {
    /// Who an internal call writes as, when an agent makes it
    /// (`session:<id>`): what it makes records it as the maker.
    pub static ACTOR: String;
    /// The exact approval currently submitting a placement command. The
    /// store links it to the command in the same transaction as enqueue.
    static PLACEMENT_APPROVAL: i64;
}

/// Who the current call writes as: the agent making it, else `default`.
pub fn by(default: &str) -> String {
    ACTOR.try_with(Clone::clone).unwrap_or_else(|_| default.to_owned())
}

/// The session a `created_by` names (`session:<id>`, maybe with a grant
/// after it), else empty.
pub fn made_by(created_by: &str) -> String {
    created_by.split_whitespace().next().and_then(|m| m.strip_prefix("session:")).unwrap_or_default().to_owned()
}

/// The agent making the current call, if one is.
pub fn actor() -> Option<String> {
    ACTOR.try_with(Clone::clone).ok()
}

/// What an operation is to an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Read,
    /// Makes something new (inside anything): free.
    Make,
    /// Changes something that exists: free on its own, else your OK once.
    Change,
    /// Deletes or otherwise can't be undone: free on its own, else your OK each time.
    Delete,
    /// Acts outside carbon6 in your name (sends mail, posts in Slack, answers
    /// an invitation, changes your calendar or mailbox): your OK each time.
    Outside,
    /// Yours alone (administration, your attention, approvals, secrets).
    Never,
}

/// Areas that are administration (and their reads, which may show more
/// than the realm's own things), or that have their own, opt-in tools.
const NEVER_AREAS: &[&str] = &["account", "assistant", "notifications", "settings", "connections", "fleet"];

/// The item kind a session's knowledge scope must include for an operation
/// on your mail, calendar, chat or photos (as for the memex tools).
fn kind_needed(op: &Op) -> Option<&'static str> {
    match op.name.split('.').next().unwrap_or("") {
        "mail" | "contacts" => Some("email"),
        "calendar" => Some("event"),
        "chat" => Some("chat"),
        "photos" => Some("photo"),
        _ => None,
    }
}

/// Whether an operation reaches the knowledge store (and so runs within
/// the session's scope).
fn knowledge_op(op: &Op) -> bool {
    matches!(op.via, Via::Memex { .. }) || ["memex.", "knowledge.", "claims.", "things."].iter().any(|p| op.name.starts_with(p))
}

/// In your name, outside carbon6: your OK each time. The first ones only
/// while Settings → Memex says sending needs your approval (as the memex
/// tools' built-in policy).
const SENDS: &[&str] =
    &["mail.send", "mail.requests.send", "calendar.create", "calendar.rsvp", "chat.post", "chat.edit", "chat.delete", "chat.react"];
const OUTSIDE: &[&str] = &["mail.modify", "mail.cancel", "calendar.update", "calendar.delete", "contacts.create", "contacts.create_many"];

/// Reads in a never-area that show nothing an agent shouldn't see.
const READ_OK: &[&str] =
    &["machines.list", "machines.samples", "connections.list", "connections.quotas", "connections.models", "workers.list", "commands.list"];

/// Yours alone, wherever they are.
const NEVER: &[&str] = &[
    // Watchers are yours to set and run (docs/watchers.md).
    "watchers.put",
    "watchers.reset",
    "watchers.run",
    // Which workers run a local Codex for plan limits, and which program.
    "workers.codex_limits.put",
    // Approvals and answers to your decisions.
    "sessions.approvals.decide",
    "workflows.steps.decide",
    "decisions.answer",
    "decisions.session",
    // Card discussions are the user's conversation: they may revise a
    // proposal or ask its source session, not an arbitrary agent write tool.
    "decisions.discuss",
    "decisions.discussion_session",
    // Starting task cards together, or keeping them as to-dos: answers too.
    "decisions.task_batch",
    "work.todos.from_cards",
    // Your attention: what you've seen.
    "sessions.seen",
    "sessions.viewed",
    "sessions.suggestion.seen",
    "work.seen",
    "attention.seen",
    // How things behave, and how your screens look.
    "work.settings.put",
    "docs.settings.put",
    "decks.themes.put",
    "settings.memex.put",
    "imports.settings.put",
    "sessions.view.set",
    "sessions.grouping.put",
    "sessions.facets.put",
    "sessions.suggestions.settings.put",
    "sessions.subagent_model.set",
    "marks.set",
    // Your Google account for documents; your files into the memex.
    "docs.google_connect",
    "docs.google_remove",
    "memex.upload",
    "memex.scan",
    "memex.merge",
    "google.sync",
    // Your accounts' set-up, and what you've read, saved or chosen to see.
    "calendar.select",
    "mail.triage.seen",
    "chat.unread",
    "chat.mark_read",
    "chat.thread_read",
    "chat.prefs",
    "chat.sections",
    "chat.save",
    // Starting and steering sessions has its own tools (task,
    // template_start, message_session) with their own limits.
    "sessions.create",
    "sessions.send",
    "sessions.side_chat",
    "sessions.queue.edit",
    "sessions.queue.now",
    "sessions.queue.pull",
    "sessions.queue.cancel",
    "sessions.queue.dismiss",
    "sessions.notices.deliver",
    "sessions.interrupt",
    "sessions.resume",
    "sessions.rewind",
    "sessions.tasks.stop",
    // Apps and packages: installing is administration.
    "packages.add",
    "packages.delete",
    "installs.add",
    "installs.upgrade",
    "installs.remove",
    "installs.set_settings",
    "apps.install",
    "apps.upgrade",
    "apps.remove",
    "apps.accounts.connect",
    "apps.accounts.remove",
];

/// Makes something new, even inside something that isn't its own.
const MAKE: &[&str] = &[
    "work.add",
    "work.todos.add",
    "projects.add",
    "projects.propose",
    "docs.create",
    "docs.import",
    "docs.comment",
    "docs.reply",
    "docs.export",
    // The batch-specific gate checks every selected id and asks once;
    // there is no single path target for the generic Change gate.
    "docs.archive_many",
    "decks.bring_in",
    "collections.create",
    "records.create",
    "series.create",
    "series.record",
    "triggers.create",
    "templates.run",
    "workflows.run",
    "decisions.add",
    "memories.create",
    "memex.notes.create",
    "memex.trails.append",
    "memex.ingest",
    "memex.summarize",
    "memex.relations.write",
    "memex.commitment.set",
    "memex.review.decide",
    "knowledge.record",
    "knowledge.set",
    "claims.add",
    "things.create",
    "views.act",
    "apps.operations.call",
    // Drafts and your requests' and triage's state stay in carbon6 (and
    // your drafts folder) until you send.
    "mail.draft",
    "mail.drafts.set",
    "mail.drafts.link",
    "mail.drafts.clear",
    "mail.requests.draft",
    "mail.requests.set",
    "mail.requests.merge",
    "mail.triage.set",
    "mail.triage.mark",
    "mail.triage.unmark",
    // Puts something new on a work item or project (what it moves there is
    // checked on its own).
    "work.attach",
    "projects.docs",
    "projects.links",
    "projects.sessions.add",
];

/// An operation's class for a call: bringing new work into a project is
/// making it; bringing existing work is changing that work.
fn class_of(op: &Op, args: &Value) -> Class {
    if op.name == "projects.work" && args["body"]["work"].as_str().is_none_or(|w| w.trim().is_empty()) {
        return Class::Make;
    }
    class(op)
}

/// An operation's class for agents.
pub fn class(op: &Op) -> Class {
    // How carbon6 behaves (settings and lists) is yours to change.
    if op.effect != Effect::Read && (op.name.starts_with("settings.") || op.name.contains(".settings.")) {
        return Class::Never;
    }
    if op.effect == Effect::Secret || NEVER.contains(&op.name) {
        return Class::Never;
    }
    if op.area == "system" {
        return if op.effect == Effect::Read { Class::Read } else { Class::Never };
    }
    if NEVER_AREAS.contains(&op.area) {
        return if READ_OK.contains(&op.name) { Class::Read } else { Class::Never };
    }
    // Connecting Google and Slack is yours; their status is a read.
    if op.name.starts_with("google.") || op.name.starts_with("slack.") {
        return if op.effect == Effect::Read { Class::Read } else { Class::Never };
    }
    if SENDS.contains(&op.name) || OUTSIDE.contains(&op.name) {
        return Class::Outside;
    }
    if MAKE.contains(&op.name) {
        return Class::Make;
    }
    match op.effect {
        Effect::Read => Class::Read,
        Effect::Write => Class::Change,
        Effect::Destructive => Class::Delete,
        Effect::Secret => Class::Never,
    }
}

fn arg(args: &Value, k: &str) -> String {
    match (&args[k], &args["body"][k]) {
        (Value::String(s), _) | (_, Value::String(s)) => s.trim().to_owned(),
        (Value::Number(n), _) | (_, Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// What a change or delete acts on (`work:<id>`, `project:<id>`, …), from
/// its path parameters. `None`: nothing to own (it can't be checked).
pub fn thing(op: &Op, args: &Value) -> Option<String> {
    let (id, name) = (arg(args, "id"), arg(args, "name"));
    let pre = op.name.split('.').next().unwrap_or("");
    let t = match pre {
        "work" if !id.is_empty() => format!("work:{id}"),
        "projects" if !id.is_empty() => format!("project:{id}"),
        "docs" if !id.is_empty() => format!("doc:{id}"),
        "collections" if !name.is_empty() => format!("collection:{name}"),
        "records" if !name.is_empty() && !id.is_empty() => format!("record:{name}/{id}"),
        "series" if !name.is_empty() => format!("series:{name}"),
        "triggers" if !id.is_empty() => format!("trigger:{id}"),
        "memories" if !id.is_empty() => format!("memory:{id}"),
        "decisions" if !id.is_empty() => format!("decision:{id}"),
        "sessions" if !id.is_empty() => format!("session:{id}"),
        "memex" if op.name.starts_with("memex.trails.") && !id.is_empty() => format!("trail:{id}"),
        "memex" if !id.is_empty() => format!("memex:{id}"),
        "claims" if !id.is_empty() => format!("claim:{id}"),
        "memex" | "knowledge" => return None,
        _ => return None,
    };
    Some(t)
}

/// Something else a call moves, beside what it acts on: an existing work
/// item brought into a project, a session attached to work or a project.
fn moved(op: &Op, args: &Value) -> Vec<String> {
    let body = &args["body"];
    let s = |k: &str| body[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
    let mut out = Vec::new();
    match op.name {
        "projects.work" => out.extend(s("work").map(|w| format!("work:{w}"))),
        "projects.sessions.add" => out.extend(s("session").map(|x| format!("session:{x}"))),
        "work.attach" | "work.merge" => {
            if let Some(r) = s("ref").or_else(|| s("reference")).filter(|r| r.starts_with("session:")) {
                out.push(r);
            }
            if let Some(w) = s("from").or_else(|| s("other")) {
                out.push(format!("work:{w}"));
            }
        }
        "projects.merge" => out.extend(s("from").or_else(|| s("other")).map(|p| format!("project:{p}"))),
        _ => {}
    }
    out
}

/// The session a session works for: its root (side chats and subagents are
/// one family with it).
async fn root_of(st: &HeadState, session: &str) -> String {
    match st.store.session(session).await.ok().flatten() {
        Some(s) => family_root(&s).to_owned(),
        None => session.to_owned(),
    }
}

fn family_root(s: &crate::store::SessionRow) -> &str {
    s.root_id.as_deref().or(s.parent_id.as_deref()).unwrap_or(&s.id)
}

/// Who made a thing, as recorded (`session:<id>`, `you`, …).
async fn maker(st: &HeadState, realm: &str, thing: &str) -> Option<String> {
    let (kind, id) = thing.split_once(':')?;
    let pool = &st.store.pool;
    let one = |sql: &'static str, a: String, b: Option<String>| async move {
        let mut q = sqlx::query_scalar::<_, String>(sql).bind(a);
        if let Some(b) = b {
            q = q.bind(b);
        }
        q.fetch_optional(pool).await.ok().flatten()
    };
    match kind {
        "work" | "project" | "memory" => one("SELECT created_by FROM records WHERE id = ?", id.to_owned(), None).await,
        "record" => {
            let (c, r) = id.split_once('/')?;
            one("SELECT created_by FROM records WHERE collection = ? AND id = ?", c.to_owned(), Some(r.to_owned())).await
        }
        "collection" => one("SELECT created_by FROM collections WHERE name = ? AND realm = ?", id.to_owned(), Some(realm.to_owned())).await,
        "series" => one("SELECT created_by FROM series WHERE name = ? AND realm = ?", id.to_owned(), Some(realm.to_owned())).await,
        "trigger" => one("SELECT owner FROM triggers WHERE id = ?", id.to_owned(), None).await,
        "decision" => one("SELECT source FROM decisions WHERE CAST(id AS TEXT) = ?", id.to_owned(), None).await,
        "session" => Some(format!("session:{id}")),
        "doc" => {
            let pool = crate::docs::pool(st, realm).await.ok()?;
            sqlx::query_scalar("SELECT created_by FROM docs WHERE id = ?").bind(id).fetch_optional(&pool).await.ok().flatten()
        }
        _ => None,
    }
}

/// Whether `thing` is the session family's own: it made it, or it's the
/// work or project the family belongs to.
async fn own(st: &HeadState, realm: &str, root: &str, thing: &str) -> bool {
    if let Some(m) = maker(st, realm, thing).await
        && let Some(s) = m.split_whitespace().next().and_then(|m| m.strip_prefix("session:"))
        && root_of(st, s).await == root
    {
        return true;
    }
    let work = crate::work::holding(st, &format!("session:{root}")).await.unwrap_or_default();
    if let Some(w) = thing.strip_prefix("work:") {
        return work.iter().any(|x| x == w);
    }
    if let Some(p) = thing.strip_prefix("project:") {
        for w in &work {
            if crate::work::project_of(st, w).await.as_deref() == Some(p) {
                return true;
            }
        }
        return crate::projects::of_session(st, root).await.as_deref() == Some(p);
    }
    false
}

/// Whether you let the session family change `thing` (an earlier Allow).
async fn allowed(st: &HeadState, realm: &str, root: &str, thing: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_writers WHERE realm = ? AND thing = ? AND writer = ?")
        .bind(realm)
        .bind(thing)
        .bind(format!("session:{root}"))
        .fetch_one(&st.store.pool)
        .await
        .is_ok_and(|n| n > 0)
}

/// Lets the session family change `thing` from now on.
pub async fn grant(st: &HeadState, realm: &str, thing: &str, root: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT OR IGNORE INTO agent_writers (realm, thing, writer, granted_at) VALUES (?, ?, ?, ?)")
        .bind(realm)
        .bind(thing)
        .bind(format!("session:{root}"))
        .bind(now_ms())
        .execute(&st.store.pool)
        .await?;
    Ok(())
}

/// A thing as you'd recognize it: its title where it has one.
async fn label(st: &HeadState, realm: &str, thing: &str) -> String {
    let (kind, id) = thing.split_once(':').unwrap_or(("", thing));
    let title = match kind {
        "work" | "project" => sqlx::query_scalar::<_, String>("SELECT json_extract(fields, '$.title') FROM records WHERE id = ?")
            .bind(id)
            .fetch_optional(&st.store.pool)
            .await
            .ok()
            .flatten(),
        "session" => st.store.session(id).await.ok().flatten().map(|s| s.title).filter(|t| !t.trim().is_empty()),
        "account" => {
            return match id {
                "event" => "your calendar".into(),
                "chat" => "your Slack".into(),
                "photo" => "your photos".into(),
                _ => "your mail".into(),
            };
        }
        "doc" => crate::docs::title_of(st, realm, id).await.map(|t| t.0),
        _ => None,
    };
    let what = match kind {
        "work" => "work item",
        "project" => "project",
        "doc" => "document",
        "record" => "record",
        "collection" => "collection",
        "series" => "series",
        "trigger" => "trigger",
        "memory" => "memory note",
        "decision" => "decision",
        "session" => "session",
        "trail" => "trail",
        "claim" => "claim",
        _ => "item",
    };
    match title {
        Some(t) => format!("the {what} \"{t}\""),
        None => format!("the {what} {id}"),
    }
}

/// `session.op`: what the `carbon6` tool asks (find, or call an operation).
pub async fn for_session(st: &HeadState, session: &str, p: &Value) -> anyhow::Result<Value> {
    let realm = crate::realms::of_session(st, session).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
    let root = root_of(st, session).await;
    let name = p["op"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return Ok(json!({"text": find_text(p["find"].as_str().unwrap_or(""))}));
    }
    let op = catalog::find(name).ok_or_else(|| crate::bad_request(format!("no operation {name:?}; find it with find")))?;
    let args = if p["input"].is_object() { p["input"].clone() } else { json!({}) };
    if p["describe"].as_bool() == Some(true) {
        return Ok(json!({"text": serde_json::to_string_pretty(&catalog::describe(op))?}));
    }
    call(st, op, args, &realm, &root, session).await
}

/// Runs operation `name` for `session` by the rule (the narrow tools'
/// way into it).
pub async fn call_for(st: &HeadState, session: &str, name: &str, args: Value) -> anyhow::Result<Value> {
    let realm = crate::realms::of_session(st, session).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
    let root = root_of(st, session).await;
    let op = catalog::find(name).ok_or_else(|| anyhow::anyhow!("no operation {name}"))?;
    call(st, op, args, &realm, &root, session).await
}

/// The `work` tool's `create`, and its actions on a work item other than
/// the session's own (`work`: its id): through the same rule.
pub async fn work_tool(st: &HeadState, session: &str, p: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| p[k].as_str().map(str::trim).unwrap_or("").to_owned();
    let action = p["action"].as_str().unwrap_or("get");
    if action == "create" {
        let title = s("title");
        if title.is_empty() {
            return Err(crate::bad_request("create takes the work's title (and done_when, and a project by name or id)"));
        }
        let project = s("project");
        if project.is_empty() {
            return call_for(st, session, "work.add", json!({"body": {"title": title, "done_when": s("done_when")}})).await;
        }
        let realm = crate::realms::of_session(st, session).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
        let id = project_id(st, &realm, &project).await?;
        let made = call_for(st, session, "projects.work", json!({"id": id, "body": {"title": title, "done_when": s("done_when")}})).await?;
        // The work item it made (newest of that title in the project).
        let work: Option<String> = sqlx::query_scalar(
            "SELECT id FROM records WHERE collection = 'work' AND realm = ? AND json_extract(fields, '$.title') = ? \
             AND json_extract(fields, '$.project') = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&realm)
        .bind(&title)
        .bind(&id)
        .fetch_optional(&st.store.pool)
        .await?;
        return Ok(match work {
            Some(w) => json!({"did": "created", "work": w, "project": id, "note": "Add to-dos to it with action todo and work: its id."}),
            None => made,
        });
    }
    let work = s("work");
    let id = s("id");
    match action {
        "get" => call_for(st, session, "work.get", json!({"id": work})).await,
        "todo" => call_for(st, session, "work.todos.add", json!({"id": work, "body": {"text": s("text"), "prompt": s("prompt")}})).await,
        "todo_done" => call_for(st, session, "work.todos.edit", json!({"id": work, "todo": id, "body": {"done": true}})).await,
        // The to-do's words or prompt; the work item's history keeps the
        // earlier ones.
        "todo_edit" => {
            let mut body = json!({});
            for k in ["text", "prompt"] {
                if p[k].is_string() {
                    body[k] = json!(s(k));
                }
            }
            if id.is_empty() || body.as_object().is_none_or(|b| b.is_empty()) {
                return Err(crate::bad_request("todo_edit takes the to-do's id, and its new text and/or prompt"));
            }
            call_for(st, session, "work.todos.edit", json!({"id": work, "todo": id, "body": body})).await
        }
        // Takes it off without saying it was done (one that's no longer wanted).
        "todo_remove" => {
            if id.is_empty() {
                return Err(crate::bad_request("todo_remove takes the to-do's id"));
            }
            call_for(st, session, "work.todos.remove", json!({"id": work, "todo": id})).await
        }
        "attach" => call_for(st, session, "work.attach", json!({"id": work, "body": {"ref": s("ref"), "title": s("title")}})).await,
        "edit" => {
            let mut body = json!({});
            for k in ["title", "done_when", "status"] {
                if !s(k).is_empty() {
                    body[k] = json!(s(k));
                }
            }
            call_for(st, session, "work.edit", json!({"id": work, "body": body})).await
        }
        other => Err(crate::bad_request(format!(
            "with work, the actions are get, todo, todo_done, todo_edit, todo_remove, attach and edit, not {other:?}"
        ))),
    }
}

/// A project of the realm, by id or (case aside) title.
async fn project_id(st: &HeadState, realm: &str, project: &str) -> anyhow::Result<String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, json_extract(fields, '$.title') FROM records WHERE collection = ? AND realm = ? \
         AND json_extract(fields, '$.status') NOT IN ('done', 'dropped')",
    )
    .bind(crate::projects::PROJECTS)
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    if let Some((id, _)) = rows.iter().find(|(id, _)| id == project) {
        return Ok(id.clone());
    }
    let want = project.to_lowercase();
    let hits: Vec<&(String, String)> = rows.iter().filter(|(_, t)| t.to_lowercase() == want).collect();
    let hits = if hits.is_empty() { rows.iter().filter(|(_, t)| t.to_lowercase().contains(&want)).collect() } else { hits };
    match hits.as_slice() {
        [one] => Ok(one.0.clone()),
        [] => Err(crate::bad_request(format!(
            "no open project {project:?}; the realm's are: {}",
            rows.iter().map(|(_, t)| format!("{t:?}")).collect::<Vec<_>>().join(", ")
        ))),
        many => Err(crate::bad_request(format!(
            "{project:?} could be {}; give its id",
            many.iter().map(|(id, t)| format!("{t:?} ({id})")).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// Runs `op` for a session family after the rule says it may.
async fn call(st: &HeadState, op: &'static Op, mut args: Value, realm: &str, root: &str, session: &str) -> anyhow::Result<Value> {
    let mut class = class_of(op, &args);
    if class == Class::Never {
        return Err(crate::forbidden(format!("{} is the user's alone; tell them what you'd like done", op.name)));
    }
    if knowledge_op(op) || kind_needed(op).is_some() {
        if !crate::memex::ready(st) {
            return Err(crate::forbidden("the knowledge store isn't open on this head"));
        }
        if let Some(k) = kind_needed(op)
            && !scope_of(st, root).await?.iter().any(|s| s == k)
        {
            return Err(crate::forbidden(format!(
                "{} needs this session's knowledge scope to include {k}, and it doesn't: the user decides that in Settings → Memex \
                 (scopes by label)",
                op.name
            )));
        }
    }
    if class == Class::Outside && SENDS.contains(&op.name) && !sending_needs_approval(st).await {
        class = Class::Make;
    }
    if class == Class::Outside {
        let account = format!("account:{}", kind_needed(op).unwrap_or("email"));
        let d = ask(st, op, &args, realm, root, session, &[account], true).await?;
        return Ok(json!({
            "asked": d,
            "text": format!(
                "Not yet: {} acts in the user's name outside carbon6, so they're asked each time (decision {d}). Nothing happened yet. \
                 You'll get a message when they answer; if they allow it, it's done for you.",
                op.name
            ),
        }));
    }
    // Placement always needs a fresh, exact approval, even for the root and
    // even if an unrelated session edit previously granted writing rights.
    if op.name == CHANGE_PLACE {
        return ask_placement(st, &args, realm, root, session).await;
    }
    // An operation that names another realm stays in this one.
    if let Some(r) = [&args["realm"], &args["body"]["realm"]].into_iter().find_map(|v| v.as_str()).filter(|r| !r.is_empty() && *r != realm)
    {
        return Err(crate::forbidden(format!("this session works in realm {realm}, not {r}")));
    }
    let pure_archive = op.name == "docs.rename"
        && args["body"]["archived"].is_boolean()
        && args["body"]["title"].is_null()
        && args["body"]["labels"].is_null();
    if op.effect != Effect::Read
        && op.name != "docs.delete"
        && !pure_archive
        && let Some(thing) = thing(op, &args)
        && let Some(id) = thing.strip_prefix("doc:")
    {
        // Read-only is a state refusal, not an invitation to ask for
        // permission. The core rechecks under its write lock as well.
        crate::docs::ensure_writable(st, realm, id).await?;
    }
    // Cleanup is one fixed, per-call selection, never a persistent grant.
    // In a mixed list even its own documents wait for that one answer.
    if op.name == "docs.archive_many" {
        let mut input: c6_api::docs_archive::ArchiveDocsIn = serde_json::from_value(args["body"].clone())
            .map_err(|e| crate::bad_request(format!("archive_many takes ids and reason under body: {e}")))?;
        input.ids = crate::docs::archive::selection(&input.ids)?;
        input.reason = input.reason.trim().to_owned();
        if input.reason.is_empty() {
            return Err(crate::bad_request("archive_many takes a reason for the user"));
        }
        args = json!({"body": input});
        let mut others = Vec::new();
        // Check every selected id, not just today's live candidates: an
        // archived document can be unarchived while this call is running.
        // A stale entry must not become an unapproved change in that race.
        for id in &input.ids {
            let thing = format!("doc:{id}");
            if !own(st, realm, root, &thing).await {
                others.push(thing);
            }
        }
        if !others.is_empty() {
            let d = ask(st, op, &args, realm, root, session, &others, true).await?;
            return Ok(json!({"asked": d, "text": format!(
                "Not yet: the user is asked to archive this exact document selection (decision {d}). Nothing changed. \
                 Allow archives it once; Not now does nothing. No future permission is granted. You'll hear the result when they answer."
            )}));
        }
        return run(st, op, args, realm, root).await;
    }
    // Archive/unarchive changes another person's document even when it
    // belongs to this session's work/project. Only ownership or explicit
    // per-document consent exempts it, not may_write's work/realm policy.
    let archiving = op.name == "docs.rename" && args["body"]["archived"].is_boolean();
    let mut needs: Vec<String> = Vec::new();
    if matches!(class, Class::Change | Class::Delete) && op.name != "projects.work" {
        match thing(op, &args) {
            Some(t) => needs.push(t),
            None => {
                let want = match &op.via {
                    Via::Http { path, .. } => catalog::path_params(path).join(", "),
                    Via::Memex { .. } => String::new(),
                };
                return Err(crate::bad_request(format!(
                    "{} needs {} at the top of input (its path parameters); the body goes under \"body\"",
                    op.name,
                    if want.is_empty() { "what it acts on".to_owned() } else { want }
                )));
            }
        }
    }
    needs.extend(moved(op, &args));
    let mut not_own = Vec::new();
    for t in needs {
        if t == format!("session:{root}") || own(st, realm, root, &t).await {
            continue;
        }
        if let Some(doc) = t.strip_prefix("doc:")
            && class != Class::Delete
            && !archiving
            && crate::docs::session::may_write(st, realm, doc, root).await?.is_ok()
        {
            continue;
        }
        if archiving && let Some(doc) = t.strip_prefix("doc:") {
            let pool = crate::docs::pool(st, realm).await?;
            let granted: i64 = sqlx::query_scalar("SELECT count(*) FROM doc_writers WHERE doc_id = ? AND writer = ?")
                .bind(doc)
                .bind(format!("session:{root}"))
                .fetch_one(&pool)
                .await?;
            if granted > 0 {
                continue;
            }
        }
        if class != Class::Delete && allowed(st, realm, root, &t).await {
            continue;
        }
        not_own.push(t);
    }
    if !not_own.is_empty() {
        let d = ask(st, op, &args, realm, root, session, &not_own, class == Class::Delete).await?;
        return Ok(json!({
            "asked": d,
            "text": format!(
                "Not yet: {} isn't this session's, so the user is asked first (decision {d}). Nothing changed. You'll get a message when they answer{}.",
                label(st, realm, &not_own[0]).await,
                if class == Class::Delete { "; if they allow it, it's done for you" } else { "; then call again" }
            ),
        }));
    }
    run(st, op, args, realm, root).await
}

async fn run(st: &HeadState, op: &'static Op, args: Value, realm: &str, root: &str) -> anyhow::Result<Value> {
    // Every knowledge call made for it sees what the session's scope allows,
    // however it reaches the memex.
    if knowledge_op(op) || kind_needed(op).is_some() {
        let scope = scope_of(st, root).await?;
        return Box::pin(crate::memex::SCOPE.scope(scope, run_unscoped(st, op, args, realm, root))).await;
    }
    run_unscoped(st, op, args, realm, root).await
}

/// The item kinds the session family's knowledge calls may see.
async fn scope_of(st: &HeadState, root: &str) -> anyhow::Result<Vec<String>> {
    match st.store.session(root).await? {
        Some(s) => crate::fabric::session_scope(st, &s).await,
        None => Ok(c6_api::MEMEX_DEFAULT_SCOPE.iter().map(|k| k.to_string()).collect()),
    }
}

async fn sending_needs_approval(st: &HeadState) -> bool {
    crate::memex::settings(st).await.ok().and_then(|s| s.send_needs_approval).unwrap_or(true)
}

async fn run_unscoped(st: &HeadState, op: &'static Op, args: Value, realm: &str, root: &str) -> anyhow::Result<Value> {
    if matches!(op.via, Via::Memex { .. }) {
        return crate::memex::call_as(
            st,
            realm,
            match op.via {
                Via::Memex { method } => method,
                _ => unreachable!(),
            },
            args,
            &format!("session:{root}"),
        )
        .await;
    }
    ACTOR.scope(format!("session:{root}"), crate::assistant::execute(st, op, args, &format!("session:{root}"), Some(realm))).await
}

/// Resolve both rows instead of using the fallback root or realm of an
/// unknown session. Siblings and unrelated sessions are not valid targets.
/// Return the requester's title for the card, even when targeting its root.
async fn placement_family(st: &HeadState, realm: &str, root: &str, session: &str, target: &str) -> anyhow::Result<String> {
    let refusal = || crate::forbidden("an agent can only ask to change place for itself or its root session in the same realm");
    if target != session && target != root {
        return Err(refusal());
    }
    let requester = st.store.session(session).await?.ok_or_else(refusal)?;
    if requester.realm != realm || family_root(&requester) != root {
        return Err(refusal());
    }
    if target != session {
        let target = st.store.session(target).await?.ok_or_else(refusal)?;
        if target.realm != realm || family_root(&target) != root {
            return Err(refusal());
        }
    }
    Ok(requester.title)
}

fn placement_input(args: &Value) -> anyhow::Result<Value> {
    let id =
        args["id"].as_str().filter(|id| !id.is_empty()).ok_or_else(|| crate::bad_request("changing place needs id at the top of input"))?;
    let request: crate::sessions::PlacementRequest = serde_json::from_value(args["body"].clone())
        .map_err(|e| crate::bad_request(format!("changing place needs a placement request under body: {e}")))?;
    Ok(json!({"id": id, "body": request}))
}

async fn ask_placement(st: &HeadState, args: &Value, realm: &str, root: &str, session: &str) -> anyhow::Result<Value> {
    // The only agent-specific key is presentation metadata, not input to
    // the backend. Reject other keys instead of silently dropping them.
    if args.as_object().is_some_and(|o| o.keys().any(|k| !matches!(k.as_str(), "id" | "body" | "reason"))) {
        return Err(crate::bad_request("changing place takes id, body and optionally reason at the top of input"));
    }
    let reason = match &args["reason"] {
        Value::Null => String::new(),
        Value::String(reason) => reason.split_whitespace().collect::<Vec<_>>().join(" "),
        _ => return Err(crate::bad_request("the reason for changing place must be text")),
    };
    let input = placement_input(args)?;
    let target = input["id"].as_str().unwrap_or_default();
    let asker = placement_family(st, realm, root, session, target).await?;
    let source_seq =
        input["body"]["expected_place_seq"].as_u64().ok_or_else(|| crate::bad_request("changing place needs expected_place_seq"))?;
    // The public placement view is newest first and survives rewind. Never
    // silently replace a stale sequence with the target's current place.
    let places = crate::sessions::placements(st, target).await?;
    if serde_json::to_value(places.first())?["seq"].as_u64() != Some(source_seq) {
        return Err(crate::conflict("the session's place changed or isn't known; read its current placement and ask again"));
    }
    // Decision titles are capped at 500 bytes, not 500 Unicode characters.
    let who = crate::util_clip(if asker.trim().is_empty() { session } else { asker.trim() }, 60);
    let title = if reason.is_empty() {
        format!("{who} asks to change place")
    } else {
        format!("{who} asks to change place: {}", crate::util_clip(&reason, 56))
    };
    let d = c6_api::DecisionIn {
        kind: "approve".into(),
        title,
        document: format!(
            "The session asks to change the place of `{}` from placement sequence {source_seq}.\n\n```json\n{}\n```\n\n\
             Allow submits this exact request once. It may still be refused if the session or its place changed. Not now or expiry changes nothing.",
            target,
            serde_json::to_string_pretty(&input)?
        ),
        context: json!({"session": session, "root": root, "op": CHANGE_PLACE, "input": input, "source_place_seq": source_seq,
            "reason": reason, "once": true, "realm": realm}),
        source: format!("session:{session}"),
        options: vec![
            c6_api::DecisionOption { label: "Allow".into(), ..Default::default() },
            c6_api::DecisionOption { label: "Not now".into(), ..Default::default() },
        ],
        deadline: Some(now_ms() + PLACE_APPROVAL_MS),
        default_answer: Some("Not now".into()),
        ..Default::default()
    };
    let reference = format!("agentop:{realm}:{}", uuid::Uuid::now_v7());
    let id = crate::decisions::open(st, &d, realm, &reference).await?.id;
    Ok(json!({"asked": id, "text": format!(
        "Not yet: the user is asked to allow this exact change of place (decision {id}). Nothing changed. \
         Allow submits it once if the session can safely move; no ongoing permission is granted. You'll hear the result when they answer."
    )}))
}

/// Opens an approval card for what isn't the session's. A change, allowed,
/// lets it change those things from then on; a delete is done on Allow.
#[allow(clippy::too_many_arguments)]
async fn ask(
    st: &HeadState,
    op: &Op,
    args: &Value,
    realm: &str,
    root: &str,
    session: &str,
    things: &[String],
    once: bool,
) -> anyhow::Result<i64> {
    let who = st.store.session(root).await?.map(|s| s.title).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "A session".into());
    let what = label(st, realm, &things[0]).await;
    let archive_docs = if op.name == "docs.archive_many" {
        let ids: Vec<String> = serde_json::from_value(args["body"]["ids"].clone())?;
        let family = crate::docs::archive::family(st, root).await?;
        let live = crate::docs::archive::candidates(st, realm, &ids, &family).await?;
        let mut shown = Vec::new();
        for id in ids {
            if let Some(doc) = live.iter().find(|d| d.id == id) {
                shown.push(doc.clone());
            } else {
                // Show every frozen id, even one that went stale while
                // preparing the card. Nothing unseen can later be run.
                let (title, kind, _) = crate::docs::title_of(st, realm, &id).await.unwrap_or((id.clone(), "doc".into(), 0));
                shown.push(c6_api::docs_archive::ArchiveDocCandidate {
                    id,
                    title,
                    kind,
                    reasons: vec!["Gone or already archived; will be skipped if still unavailable".into()],
                });
            }
        }
        shown
    } else {
        Vec::new()
    };
    let (title, document) = if op.name == "docs.archive_many" {
        let mut document = format!(
            "The session wants to archive **{} documents**.\n\n{}\n\n",
            archive_docs.len(),
            args["body"]["reason"].as_str().unwrap_or("")
        );
        for doc in &archive_docs {
            document.push_str(&format!("- [{}](doc:{})", doc.title.replace(['[', ']'], ""), doc.id));
            if !doc.reasons.is_empty() {
                document.push_str(&format!(" — {}", doc.reasons.join("; ")));
            }
            document.push('\n');
        }
        document.push_str(
            "\nAllow archives only the listed ids once. Gone or already archived documents are skipped and reported. \
                           Not now changes nothing. No ongoing permission is granted; no other document is included.",
        );
        (format!("Let {who} archive {} documents?", archive_docs.len()), document)
    } else if things[0].starts_with("account:") {
        (
            format!("Let {who} {}?", lower_first(op.summary.trim_end_matches('.'))),
            format!(
                "The session wants to run **{}** ({}) with {what}, in your name.\n\n```json\n{}\n```\n\nAllow runs it once, as shown.",
                op.name,
                op.summary,
                serde_json::to_string_pretty(args).unwrap_or_default()
            ),
        )
    } else if once {
        (
            format!("Let {who} do this to {what}: {}?", lower_first(op.summary.trim_end_matches('.'))),
            format!(
                "The session wants to run **{}** ({}) on {what}, which isn't its own.\n\n```json\n{}\n```\n\nAllow runs it once, as shown.",
                op.name,
                op.summary,
                serde_json::to_string_pretty(args).unwrap_or_default()
            ),
        )
    } else {
        let mut all = Vec::new();
        for t in things {
            all.push(label(st, realm, t).await);
        }
        (
            format!("Let {who} change {what}?"),
            format!(
                "The session wants to run **{}** ({}) on {}, which isn't its own. Allow lets this session (with its side chats and \
                 subagents) change {} from now on; other sessions still ask.\n\n```json\n{}\n```",
                op.name,
                op.summary,
                all.join(", "),
                if all.len() > 1 { "them" } else { "it" },
                serde_json::to_string_pretty(args).unwrap_or_default()
            ),
        )
    };
    let mut d = c6_api::DecisionIn {
        kind: "approve".into(),
        title,
        document,
        context: json!({"session": session, "root": root, "op": op.name, "input": args, "things": things, "once": once, "realm": realm}),
        source: format!("session:{session}"),
        options: vec![
            c6_api::DecisionOption { label: "Allow".into(), ..Default::default() },
            c6_api::DecisionOption { label: "Deny".into(), ..Default::default() },
        ],
        ..Default::default()
    };
    if op.name == "docs.archive_many" {
        d.context["archive_docs"] = json!(archive_docs);
        d.context["reason"] = args["body"]["reason"].clone();
        d.options[1].label = "Not now".into();
    }
    let reference =
        if once { format!("agentop:{realm}:{}", uuid::Uuid::now_v7()) } else { format!("agentop:{realm}:{root}/{}", things.join(",")) };
    Ok(crate::decisions::open(st, &d, realm, &reference).await?.id)
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

/// Called inside the placement enqueue transaction. A human-initiated
/// command has no approval in scope and no requester notice obligation.
pub(crate) async fn link_placement_command(tx: &mut sqlx::SqliteConnection, target: &str, command_id: &str) -> anyhow::Result<()> {
    let Ok(id) = PLACEMENT_APPROVAL.try_with(|id| *id) else {
        anyhow::ensure!(actor().is_none(), "an agent placement needs an exact approval before enqueue");
        return Ok(());
    };
    let linked = sqlx::query(
        "UPDATE decisions SET context = json_set(context, '$.placement_command_id', ?1) WHERE id = ?2 \
         AND json_extract(context, '$.op') = ?3 AND json_extract(context, '$.input.id') = ?4 \
         AND realm = (SELECT realm FROM sessions WHERE id = ?4) \
         AND json_extract(context, '$.placement_consumed') = 1 AND json_extract(context, '$.placement_command_id') IS NULL \
         AND json_extract(context, '$.placement_result') IS NULL",
    )
    .bind(command_id)
    .bind(id)
    .bind(CHANGE_PLACE)
    .bind(target)
    .execute(tx)
    .await?;
    anyhow::ensure!(linked.rows_affected() == 1, "placement command does not match an unused approval submission");
    Ok(())
}

/// Persist the correlated outcome in the same transaction as its worker
/// event or rejection. An unrelated, unapproved, or replayed command cannot
/// change the outcome or create a notice.
pub(crate) async fn finish_placement_command(
    tx: &mut sqlx::SqliteConnection,
    target: &str,
    command_id: &str,
    outcome: Result<(u64, &str), &str>,
) -> anyhow::Result<()> {
    if command_id.is_empty() {
        return Ok(());
    }
    let result = match outcome {
        Ok((seq, cwd)) => format!("Placement completed (command {command_id}, placement sequence {seq}): now in {cwd:?}."),
        Err(reason) => format!("The worker refused the placement change (command {command_id}): {}", crate::util_clip(reason, 1000)),
    };
    sqlx::query(
        "UPDATE decisions SET context = json_set(context, '$.placement_result', ?) \
         WHERE ref LIKE 'agentop:%' AND json_extract(context, '$.op') = ? \
         AND json_extract(context, '$.placement_command_id') = ? AND json_extract(context, '$.input.id') = ? \
         AND json_extract(context, '$.placement_result') IS NULL",
    )
    .bind(result)
    .bind(CHANGE_PLACE)
    .bind(command_id)
    .bind(target)
    .execute(tx)
    .await?;
    Ok(())
}

/// The decision context is the outbox. A stable, unique input command id
/// bridges a crash between queuing that input and marking the outbox sent.
pub(crate) async fn sweep_placement_notices(st: &HeadState) -> anyhow::Result<()> {
    // The head may have stopped after claiming Allow but before enqueue.
    // This update and the transactional link exclude each other: if it
    // wins, the result fences off even a still-running submission.
    sqlx::query(
        "UPDATE decisions SET context = json_set(context, '$.placement_result', ?) \
         WHERE ref LIKE 'agentop:%' AND json_extract(context, '$.op') = ? \
         AND json_extract(context, '$.placement_consumed') = 1 \
         AND COALESCE(json_extract(context, '$.placement_consumed_at'), updated_at) <= ? \
         AND json_extract(context, '$.placement_command_id') IS NULL AND json_extract(context, '$.placement_result') IS NULL",
    )
    .bind(PLACE_SUBMISSION_INTERRUPTED)
    .bind(CHANGE_PLACE)
    .bind(now_ms() - PLACE_SUBMISSION_MS)
    .execute(&st.store.pool)
    .await?;

    let pending: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT d.id, s.id, json_extract(d.context, '$.placement_result') FROM decisions d \
         JOIN sessions s ON s.id = json_extract(d.context, '$.session') AND s.realm = d.realm \
         WHERE d.ref LIKE 'agentop:%' AND json_extract(d.context, '$.op') = ? \
         AND json_extract(d.context, '$.placement_result') IS NOT NULL \
         AND json_extract(d.context, '$.placement_notified') IS NULL \
         AND s.archived = 0 AND s.observed = 0 AND s.status NOT IN ('failed', 'stopped') ORDER BY d.id",
    )
    .bind(CHANGE_PLACE)
    .fetch_all(&st.store.pool)
    .await?;
    for (id, session, result) in pending {
        let notice = format!("placement-decision:{id}");
        let queued = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM commands WHERE id = ? AND session_id = ?)")
            .bind(&notice)
            .bind(&session)
            .fetch_one(&st.store.pool)
            .await?;
        if !queued
            && let Err(e) = crate::sessions::send_input_as(
                st,
                &session,
                &c6_api::SendMessage { text: format!("Placement decision #{id}: {result}"), ..Default::default() },
                notice,
                "",
                None,
                false,
            )
            .await
        {
            tracing::debug!(decision = id, "could not queue the placement notice; will retry: {e:#}");
            continue;
        }
        sqlx::query("UPDATE decisions SET context = json_set(context, '$.placement_notified', json('true')) WHERE id = ?")
            .bind(id)
            .execute(&st.store.pool)
            .await?;
    }
    Ok(())
}

async fn decide_placement(st: &HeadState, d: &c6_api::Decision, allow: bool) -> anyhow::Result<()> {
    let c = &d.context;
    let (realm, root, session) =
        (c["realm"].as_str().unwrap_or_default(), c["root"].as_str().unwrap_or_default(), c["session"].as_str().unwrap_or_default());
    if d.realm != realm || !c["once"].as_bool().unwrap_or(false) {
        return Err(crate::bad_request("this isn't an exact placement approval"));
    }

    // Claim the card durably before any attempt. A refused attempt is not
    // reusable, either; ask for a new approval rather than retrying silently.
    let now = now_ms();
    let claimed: Option<i64> = sqlx::query_scalar(
        "UPDATE decisions SET context = CASE WHEN ?1 THEN json_set(context, '$.placement_consumed', json('true'), '$.placement_consumed_at', ?2) \
         ELSE json_set(context, '$.placement_consumed', json('true'), '$.placement_consumed_at', ?2, '$.placement_result', \
           CASE WHEN deadline IS NULL OR deadline <= ?2 THEN 'The approval expired. No change was requested.' \
                ELSE 'The user chose Not now. No change was requested.' END) END \
         WHERE id = ?3 AND realm = ?4 AND ref LIKE 'agentop:%' AND state IN ('open', 'snoozed') \
           AND json_extract(context, '$.op') = ?5 AND json_extract(context, '$.placement_consumed') IS NULL \
           AND (NOT ?1 OR deadline > ?2) \
         RETURNING id",
    )
    .bind(allow)
    .bind(now)
    .bind(d.id)
    .bind(realm)
    .bind(CHANGE_PLACE)
    .fetch_optional(&st.store.pool)
    .await?;
    if claimed.is_none() {
        return Err(crate::bad_request("this placement approval is no longer available; ask for a new exact request"));
    }
    if !allow {
        return Ok(());
    }

    let submitted = async {
        let input = placement_input(&c["input"])?;
        if input != c["input"] || input["body"]["expected_place_seq"] != c["source_place_seq"] {
            return Err(crate::bad_request("the frozen placement request no longer matches its approval; ask again"));
        }
        placement_family(st, realm, root, session, input["id"].as_str().unwrap_or_default()).await?;
        let op = catalog::find(CHANGE_PLACE).ok_or_else(|| crate::bad_request("the operation to change place is gone"))?;
        // The same handler used for a user-requested move enforces the
        // safe boundary and checks the approved source sequence again.
        PLACEMENT_APPROVAL.scope(d.id, run(st, op, input, realm, root)).await
    }
    .await;
    let result = match submitted {
        // Do not send input to the session yet: it can start another turn
        // and interfere with the approved safe-boundary command.
        Ok(_) => return Ok(()),
        Err(e) => format!("The user allowed the move, but the request was refused: {e:#}"),
    };
    // Preserve a previously queued command if the response failed after
    // enqueue. Its worker outcome still owns the notice in that case.
    sqlx::query(
        "UPDATE decisions SET context = json_set(context, '$.placement_result', ?) \
         WHERE id = ? AND json_extract(context, '$.placement_command_id') IS NULL AND json_extract(context, '$.placement_result') IS NULL",
    )
    .bind(&result)
    .bind(d.id)
    .execute(&st.store.pool)
    .await?;
    Ok(())
}

/// Your answer to an approval card (`agentop:…`).
pub async fn decided(st: &HeadState, d: &c6_api::Decision, option: Option<&str>) -> anyhow::Result<()> {
    let c = &d.context;
    let (realm, root, session) =
        (c["realm"].as_str().unwrap_or_default(), c["root"].as_str().unwrap_or_default(), c["session"].as_str().unwrap_or_default());
    let allow = option == Some("Allow");
    let op_name = c["op"].as_str().unwrap_or_default();
    if op_name == CHANGE_PLACE {
        return decide_placement(st, d, allow).await;
    }
    if op_name == "docs.archive_many" {
        if allow {
            let ids: Vec<String> = serde_json::from_value(c["input"]["body"]["ids"].clone())?;
            let shown: Vec<&str> = c["archive_docs"].as_array().into_iter().flatten().filter_map(|d| d["id"].as_str()).collect();
            if crate::docs::archive::selection(&ids)? != ids || shown != ids.iter().map(String::as_str).collect::<Vec<_>>() {
                return Err(crate::bad_request(
                    "this cleanup approval's ids are not canonical or do not match its preview; ask for a new exact selection",
                ));
            }
        }
        // One answer consumes this exact selection, including Not now.
        // A conditional write also prevents two concurrent answers from
        // replaying it after both readers saw the decision still open.
        let claimed = sqlx::query(
            "UPDATE decisions SET context = json_set(context, '$.archive_consumed', json('true')) \
             WHERE id = ? AND json_extract(context, '$.archive_consumed') IS NULL",
        )
        .bind(d.id)
        .execute(&st.store.pool)
        .await?;
        if claimed.rows_affected() == 0 {
            return Err(crate::bad_request("this document cleanup approval was already used; ask for a new exact selection"));
        }
    }
    let mut text = format!("The user {} decision #{} ({}).", if allow { "allowed" } else { "denied" }, d.id, d.title);
    if allow {
        if c["once"].as_bool() == Some(true) {
            let op = catalog::find(op_name).ok_or_else(|| crate::bad_request("that operation is gone"))?;
            match run(st, op, c["input"].clone(), realm, root).await {
                Ok(v) => {
                    if op_name == "docs.archive_many" {
                        // Keep the complete skipped-id report on the card
                        // as well as in the session notification.
                        // The approved input and candidate list never change.
                        sqlx::query("UPDATE decisions SET context = json_set(context, '$.archive_result', json(?)) WHERE id = ?")
                            .bind(v.to_string())
                            .bind(d.id)
                            .execute(&st.store.pool)
                            .await?;
                    }
                    let result = if op_name == "docs.archive_many" { v.to_string() } else { crate::util_clip(&v.to_string(), 1500) };
                    text.push_str(&format!(" {op_name} ran: {result}"));
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    if op_name == "docs.archive_many" {
                        // An archived requester cannot receive the notice;
                        // keep failures readable on its decision as well.
                        sqlx::query("UPDATE decisions SET context = json_set(context, '$.archive_error', ?) WHERE id = ?")
                            .bind(&error)
                            .bind(d.id)
                            .execute(&st.store.pool)
                            .await?;
                    }
                    text.push_str(&format!(" {op_name} failed: {error}"));
                }
            }
        } else {
            for t in c["things"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                grant(st, realm, t, root).await?;
            }
            text.push_str(&format!(" You may now run {op_name} on it; call it again."));
        }
    }
    tell_session(st, session, d.id, text).await;
    Ok(())
}

async fn tell_session(st: &HeadState, session: &str, decision: i64, text: String) {
    if !session.is_empty()
        && let Err(e) = crate::sessions::send_input(st, session, &c6_api::SendMessage { text, ..Default::default() }).await
    {
        tracing::warn!(decision, "telling the session: {e:#}");
    }
}

/// Operations matching words, with how an agent may use each.
fn find_text(query: &str) -> String {
    let v = catalog::discover(query, None);
    let mut out = String::new();
    if let Some(areas) = v["areas"].as_array() {
        out.push_str("Areas (operations); find with words to see operations:\n");
        for a in areas {
            out.push_str(&format!("- {}: {}\n", a["area"].as_str().unwrap_or(""), a["what"].as_str().unwrap_or("")));
        }
        return out;
    }
    // Only what an agent may use: the user's own operations would crowd out
    // the ones it can call.
    let found: Vec<&Op> = catalog::ranked(query, None).into_iter().filter(|o| class(o) != Class::Never).take(10).collect();
    if found.is_empty() {
        return format!("No operations match {query:?}; try other words.");
    }
    for (i, op) in found.iter().enumerate() {
        let how = match (op.name, class(op)) {
            (CHANGE_PLACE, _) => "changes place: asks the user each time for the exact request, even for this session",
            (_, Class::Read) => "reads",
            (_, Class::Make) => "makes something: free",
            (_, Class::Change) => "changes: free on what this session made, else the user is asked once",
            (_, Class::Delete) => "deletes: free on what this session made, else the user is asked each time",
            (_, Class::Outside) => "acts in the user's name outside carbon6: they're asked each time",
            (_, Class::Never) => "the user's alone",
        };
        if i == 0 {
            out.push_str(&format!("{} ({how}):\n{}\n\n", op.name, serde_json::to_string(&catalog::describe(op)).unwrap_or_default()));
        } else {
            out.push_str(&format!("- {}: {} ({how})\n", op.name, op.summary));
        }
    }
    out.push_str("\nCall one with op and input (path parameters at the top, the body under \"body\"); describe: true shows one in full.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every change or delete an agent may make names what it acts on, so
    /// the rule can be applied: a new operation without that fails here.
    #[test]
    fn every_change_names_what_it_acts_on() {
        let mut missing = Vec::new();
        for op in catalog::ops() {
            if !matches!(class(op), Class::Change | Class::Delete) {
                continue;
            }
            let Via::Http { path, .. } = &op.via else {
                // Memex operations name no thing: they're makes or reads.
                missing.push(op.name);
                continue;
            };
            let mut args = json!({});
            for p in catalog::path_params(path) {
                args[p] = json!("x");
            }
            if thing(op, &args).is_none() {
                missing.push(op.name);
            }
        }
        assert!(missing.is_empty(), "classify these for agents (agent_ops.rs): {missing:?}");
    }

    #[test]
    fn settings_list_replacements_remain_the_users_alone() {
        for op in catalog::ops().iter().filter(|op| op.name.starts_with("settings.") && op.replaces.is_some()) {
            assert_eq!(class(op), Class::Never, "{}", op.name);
        }
    }

    #[test]
    fn classes() {
        let c = |n: &str| class(catalog::find(n).unwrap());
        assert_eq!(c("work.add"), Class::Make);
        assert_eq!(c("projects.add"), Class::Make);
        assert_eq!(c("projects.work"), Class::Change);
        assert_eq!(c("docs.archive_many"), Class::Make);
        assert_eq!(c(CHANGE_PLACE), Class::Change);
        assert_eq!(c("work.edit"), Class::Change);
        assert_eq!(c("work.todos.remove"), Class::Delete);
        assert_eq!(c("work.list"), Class::Read);
        assert_eq!(c("decisions.answer"), Class::Never);
        assert_eq!(c("decisions.discussion"), Class::Read);
        assert_eq!(c("decisions.discuss"), Class::Never);
        assert_eq!(c("decisions.discussion_session"), Class::Never);
        assert_eq!(c("connections.create"), Class::Never);
        assert_eq!(c("auth.tokens.list"), Class::Never);
        assert_eq!(c("realms.create"), Class::Never);
        assert_eq!(c("realms.list"), Class::Read);
        assert_eq!(c("settings.prompts.put"), Class::Never);
        // Your mail, calendar and chat: by the session's scope; in your
        // name outside carbon6, each time; your set-up and what you've read, never.
        assert_eq!(c("mail.threads"), Class::Read);
        assert_eq!(c("mail.draft"), Class::Make);
        assert_eq!(c("mail.send"), Class::Outside);
        assert_eq!(c("calendar.create"), Class::Outside);
        assert_eq!(c("chat.post"), Class::Outside);
        assert_eq!(c("mail.modify"), Class::Outside);
        assert_eq!(c("photos.list"), Class::Read);
        assert_eq!(c("google.status"), Class::Read);
        assert_eq!(c("google.connect"), Class::Never);
        assert_eq!(c("calendar.select"), Class::Never);
        assert_eq!(c("chat.mark_read"), Class::Never);
        assert_eq!(kind_needed(catalog::find("mail.threads").unwrap()), Some("email"));
        assert_eq!(kind_needed(catalog::find("calendar.events").unwrap()), Some("event"));
        assert_eq!(kind_needed(catalog::find("chat.messages").unwrap()), Some("chat"));
        assert_eq!(kind_needed(catalog::find("work.add").unwrap()), None);
    }

    #[tokio::test]
    async fn place_requires_fresh_exact_consent_even_with_a_grant() {
        use c6_proto::session_event::Kind;
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let st = &head.state;
        let worker: c6_core::WorkerId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        for (id, root, realm) in [
            ("root", None, "home"),
            ("asking-child", Some("root"), "home"),
            ("sibling", Some("root"), "home"),
            ("foreign-child", Some("root"), "other-realm"),
        ] {
            sqlx::query(
                "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, root_id, realm) \
                         VALUES (?, '00000000-0000-0000-0000-000000000001', ?, '/old', '', '', 'idle', 0, 0, ?, ?)",
            )
            .bind(id)
            .bind(id)
            .bind(root)
            .bind(realm)
            .execute(&st.store.pool)
            .await
            .unwrap();
        }
        let start = c6_proto::SessionEvent {
            seq: 1,
            ts_ms: 1,
            kind: Some(Kind::Started(c6_proto::SessionStarted { cwd: "/old".into(), ..Default::default() })),
        };
        st.store.ingest(worker, "root", &[start]).await.unwrap().unwrap();
        sqlx::query("INSERT INTO workers (id, name, hostname, os, arch, cert_serial, enrolled_at) VALUES (?, 'test', '', '', '', '', 0)")
            .bind(worker.to_string())
            .execute(&st.store.pool)
            .await
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let _guard = st.registry.attach(worker, now_ms(), tx, vec!["same_worker_placement_v1".into()], "worker").await;

        let op = catalog::find(CHANGE_PLACE).unwrap();
        let args =
            json!({"id": "root", "body": {"kind": "folder", "expected_place_seq": 1, "cwd": "/new"}, "reason": "Investigate the patch"});
        assert!(call(st, op, json!({"id": "sibling", "body": args["body"]}), "home", "root", "asking-child").await.is_err());
        assert!(call(st, op, args.clone(), "other-realm", "root", "asking-child").await.is_err());
        assert!(call_for(st, "foreign-child", CHANGE_PLACE, args.clone()).await.is_err());
        let mut stale = args.clone();
        stale["body"]["expected_place_seq"] = json!(99);
        assert!(call_for(st, "root", CHANGE_PLACE, stale).await.is_err());
        grant(st, "home", "session:root", "root").await.unwrap();

        let ask = |session| call_for(st, session, CHANGE_PLACE, args.clone());
        let read = |id| crate::decisions::get(st, id);
        let allow = |id| async move {
            crate::decisions::answer(st, id, &c6_api::DecisionAnswer { option: Some("Allow".into()), ..Default::default() }, "test")
                .await
                .unwrap()
        };
        let self_id = ask("root").await.unwrap()["asked"].as_i64().unwrap();
        let denied_id = ask("asking-child").await.unwrap()["asked"].as_i64().unwrap();
        let denied = read(denied_id).await.unwrap().unwrap();
        assert_eq!(denied.title, "asking-child asks to change place: Investigate the patch");
        assert_eq!(
            denied.context["input"],
            json!({"id": "root", "body": {
                "kind": "folder", "expected_place_seq": 1, "cwd": "/new", "copy_scratch": false
            }})
        );
        assert_eq!(denied.context["source_place_seq"], 1);
        assert!(denied.context["placement_consumed"].is_null());
        crate::decisions::answer(st, denied_id, &c6_api::DecisionAnswer { option: Some("Not now".into()), ..Default::default() }, "test")
            .await
            .unwrap();
        assert!(decided(st, &denied, Some("Allow")).await.is_err(), "a denial cannot be replayed as an allowance");

        let expired_id = ask("asking-child").await.unwrap()["asked"].as_i64().unwrap();
        let before_expiry = read(expired_id).await.unwrap().unwrap();
        sqlx::query("UPDATE decisions SET deadline = 1 WHERE id = ?").bind(expired_id).execute(&st.store.pool).await.unwrap();
        // Even a stale reader who saw the card before it expired cannot submit the move.
        assert!(decided(st, &before_expiry, Some("Allow")).await.is_err());
        crate::decisions::sweep(st).await;
        let expired = read(expired_id).await.unwrap().unwrap();
        assert_eq!(expired.state, "expired");
        assert!(expired.context["placement_result"].as_str().unwrap().contains("expired"));

        for completed in [false, true] {
            let id = ask("asking-child").await.unwrap()["asked"].as_i64().unwrap();
            let before = read(id).await.unwrap().unwrap();
            let d = allow(id).await;
            let command_id = d.context["placement_command_id"].as_str().unwrap();
            assert_eq!(d.context["placement_consumed"], true);
            assert!(d.context["placement_consumed_at"].as_i64().is_some());
            assert!(d.context["placement_result"].is_null(), "enqueue alone does not mean the move happened");
            assert!(decided(st, &before, Some("Allow")).await.is_err());
            let commands = st.store.pending_commands(worker, 0).await.unwrap();
            let mut moves = commands.iter().filter_map(|c| {
                let cmd = c6_proto::decode_stored_command(&c.data).unwrap();
                match cmd.kind {
                    Some(c6_proto::session_command::Kind::ChangePlace(p)) => Some((cmd.command_id, cmd.session_id, p)),
                    _ => None,
                }
            });
            let (queued_id, target, request) = moves.next().unwrap();
            assert!(moves.next().is_none());
            assert_eq!((queued_id.as_str(), target.as_str()), (command_id, "root"));
            assert_eq!((request.expected_place_seq, request.cwd.as_str(), request.copy_scratch), (1, "/new", false));

            if completed {
                let changed = c6_proto::SessionEvent {
                    seq: 2,
                    ts_ms: 2,
                    kind: Some(Kind::PlaceChanged(c6_proto::PlaceChanged {
                        command_id: command_id.into(),
                        to: Some(c6_proto::SessionPlace { cwd: "/new".into(), ..Default::default() }),
                        ..Default::default()
                    })),
                };
                st.store.ingest(worker, "root", std::slice::from_ref(&changed)).await.unwrap().unwrap();
                st.store.ingest(worker, "root", &[changed]).await.unwrap().unwrap();
            } else {
                st.store.reject_command(worker, command_id, "move blocked").await.unwrap().unwrap();
                assert!(st.store.reject_command(worker, command_id, "replay").await.unwrap().is_none());
            }
            // The committed worker outcome survives independently of any
            // post-commit delivery hook. A resumed sweep delivers it once.
            let settled = read(id).await.unwrap().unwrap();
            let result = settled.context["placement_result"].as_str().unwrap();
            assert!(result.contains(command_id));
            assert!(result.contains(if completed { "Placement completed" } else { "move blocked" }));
            sweep_placement_notices(st).await.unwrap();
            sqlx::query("UPDATE decisions SET context = json_remove(context, '$.placement_notified') WHERE id = ?")
                .bind(id)
                .execute(&st.store.pool)
                .await
                .unwrap();
            sweep_placement_notices(st).await.unwrap();
            let notices: Vec<(String, String)> = sqlx::query_as("SELECT session_id, text FROM commands WHERE id = ?")
                .bind(format!("placement-decision:{id}"))
                .fetch_all(&st.store.pool)
                .await
                .unwrap();
            assert_eq!(notices, vec![("asking-child".into(), format!("Placement decision #{id}: {result}"))]);
        }

        // Simulate a head that committed the Allow claim, but stopped
        // before it could link a placement command or record a refusal.
        let mut current = args.clone();
        current["body"]["expected_place_seq"] = json!(2);
        let interrupted_id = call_for(st, "asking-child", CHANGE_PLACE, current).await.unwrap()["asked"].as_i64().unwrap();
        sqlx::query(
            "UPDATE decisions SET context = json_set(context, '$.placement_consumed', json('true'), '$.placement_consumed_at', ?) WHERE id = ?",
        )
        .bind(now_ms() - PLACE_SUBMISSION_MS - 1)
        .bind(interrupted_id)
        .execute(&st.store.pool)
        .await
        .unwrap();
        sweep_placement_notices(st).await.unwrap();
        sweep_placement_notices(st).await.unwrap();
        let interrupted = read(interrupted_id).await.unwrap().unwrap();
        assert_eq!(interrupted.context["placement_result"], PLACE_SUBMISSION_INTERRUPTED);
        assert!(interrupted.context["placement_command_id"].is_null());
        let notices: Vec<(String, String)> = sqlx::query_as("SELECT session_id, text FROM commands WHERE id = ?")
            .bind(format!("placement-decision:{interrupted_id}"))
            .fetch_all(&st.store.pool)
            .await
            .unwrap();
        assert_eq!(notices, vec![("asking-child".into(), format!("Placement decision #{interrupted_id}: {PLACE_SUBMISSION_INTERRUPTED}"))]);
        let late = crate::store::NewCommand { id: "late-placement", session_id: "root", worker_id: worker, data: Vec::new(), text: None };
        assert!(PLACEMENT_APPROVAL.scope(interrupted_id, st.store.enqueue_placement_command(&late)).await.is_err());
        assert!(!st.store.pending_commands(worker, 0).await.unwrap().iter().any(|c| c.id == late.id));

        // Our own older card is not rebased onto the new source. The backend
        // rejects it before enqueue, and this refusal also consumes approval.
        let stale = allow(self_id).await;
        assert!(stale.context["placement_command_id"].is_null());
        assert!(stale.context["placement_result"].as_str().unwrap().contains("place changed"));
    }

    /// Asked in your words, find puts the operation you mean first.
    #[test]
    fn find_understands_everyday_words() {
        let first = |q: &str| catalog::ranked(q, None).into_iter().find(|o| class(o) != Class::Never).map(|o| o.name).unwrap_or_default();
        assert_eq!(first("remove a to-do"), "work.todos.remove");
        assert_eq!(first("delete a todo from a work item"), "work.todos.remove");
        assert_eq!(first("add a to-do to work"), "work.todos.add");
        assert_eq!(first("tick off a to-do"), "work.todos.edit");
        assert_eq!(first("create a project"), "projects.add");
        assert_eq!(first("rename a project"), "projects.edit");
        let top = |q: &str, n: usize| -> Vec<&str> {
            catalog::ranked(q, None).into_iter().filter(|o| class(o) != Class::Never).take(n).map(|o| o.name).collect()
        };
        assert!(top("create a new work in the project", 3).contains(&"projects.work"), "{:?}", top("create a new work in the project", 3));
        assert_eq!(first("send an email"), "mail.send");
        // What an agent can't use isn't offered.
        assert!(!find_text("create an API token").contains("auth.tokens.create"));
    }
}
