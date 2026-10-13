//! Action items (docs/follow-through.md §7): a list of things for you to
//! settle when you want to (facts to confirm, disputes, kits to review,
//! questions agents could not answer), each with its context. Agents never
//! wait on them. Any item can start a session that helps you settle it.

use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row;

use crate::{HeadState, bad_request, not_found};

const KINDS: [&str; 6] = ["confirm", "dispute", "kit", "provide", "question", "follow_up"];

fn todo_of(r: &sqlx::sqlite::SqliteRow) -> api::Todo {
    api::Todo {
        id: r.get("id"),
        kind: r.get("kind"),
        title: r.get("title"),
        detail: r.get("detail"),
        context: serde_json::from_str(&r.get::<String, _>("context")).unwrap_or(json!({})),
        source: r.get("source"),
        priority: r.get("priority"),
        state: r.get("state"),
        snooze_until: r.get("snooze_until"),
        resolution: r.get("resolution"),
        sessions: serde_json::from_str(&r.get::<String, _>("sessions")).unwrap_or_default(),
        count: r.get::<i64, _>("count") as u32,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Adds an item; an open item of the same group is raised again instead.
pub async fn add(st: &HeadState, t: &api::TodoIn) -> anyhow::Result<api::Todo> {
    let title = t.title.trim();
    if title.is_empty() || title.len() > 500 {
        return Err(bad_request("an action item needs a title (up to 500 characters)"));
    }
    let kind = if KINDS.contains(&t.kind.as_str()) { t.kind.as_str() } else { "question" };
    let priority = if matches!(t.priority.as_str(), "high" | "low") { t.priority.as_str() } else { "normal" };
    let now = now_ms();
    if !t.group.is_empty()
        && let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM todos WHERE grp = ? AND state IN ('open', 'snoozed') LIMIT 1")
            .bind(&t.group)
            .fetch_optional(&st.store.pool)
            .await?
    {
        sqlx::query("UPDATE todos SET count = count + 1, updated_at = ?, detail = CASE WHEN ? = '' THEN detail ELSE ? END WHERE id = ?")
            .bind(now)
            .bind(&t.detail)
            .bind(&t.detail)
            .bind(id)
            .execute(&st.store.pool)
            .await?;
        st.registry.bump();
        return get(st, id).await?.ok_or_else(|| anyhow::anyhow!("action item gone"));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO todos (kind, title, detail, context, source, priority, grp, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(kind)
    .bind(title)
    .bind(t.detail.trim())
    .bind(if t.context.is_null() { "{}".to_owned() } else { t.context.to_string() })
    .bind(if t.source.is_empty() { "you" } else { t.source.as_str() })
    .bind(priority)
    .bind(&t.group)
    .bind(now)
    .bind(now)
    .fetch_one(&st.store.pool)
    .await?;
    st.registry.bump();
    get(st, id).await?.ok_or_else(|| anyhow::anyhow!("action item gone"))
}

pub async fn get(st: &HeadState, id: i64) -> anyhow::Result<Option<api::Todo>> {
    Ok(sqlx::query("SELECT * FROM todos WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?.as_ref().map(todo_of))
}

/// Items in a state ("open" includes snoozed ones whose time has come).
pub async fn list(st: &HeadState, state: &str) -> anyhow::Result<Vec<api::Todo>> {
    let now = now_ms();
    let rows = match state {
        "" | "open" => {
            sqlx::query(
                "SELECT * FROM todos WHERE state = 'open' OR (state = 'snoozed' AND snooze_until <= ?) \
                 ORDER BY CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END, updated_at DESC",
            )
            .bind(now)
            .fetch_all(&st.store.pool)
            .await?
        }
        s => {
            sqlx::query("SELECT * FROM todos WHERE state = ? ORDER BY updated_at DESC LIMIT 200").bind(s).fetch_all(&st.store.pool).await?
        }
    };
    Ok(rows.iter().map(todo_of).collect())
}

pub async fn open_count(st: &HeadState) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM todos WHERE state = 'open' OR (state = 'snoozed' AND snooze_until <= ?)")
        .bind(now_ms())
        .fetch_one(&st.store.pool)
        .await?)
}

/// Settles, snoozes, dismisses or reopens an item.
pub async fn edit(st: &HeadState, id: i64, e: &api::TodoEdit) -> anyhow::Result<api::Todo> {
    let mut t = get(st, id).await?.ok_or_else(|| not_found("no such action item"))?;
    if let Some(s) = &e.state {
        if !matches!(s.as_str(), "open" | "snoozed" | "done" | "dismissed") {
            return Err(bad_request("an action item is open, snoozed, done or dismissed"));
        }
        t.state = s.clone();
    }
    if let Some(r) = &e.resolution {
        t.resolution = r.trim().to_owned();
    }
    if t.state == "snoozed" {
        t.snooze_until = Some(e.snooze_until.unwrap_or_else(|| now_ms() + 86_400_000));
    }
    sqlx::query("UPDATE todos SET state = ?, resolution = ?, snooze_until = ?, updated_at = ? WHERE id = ?")
        .bind(&t.state)
        .bind(&t.resolution)
        .bind(t.snooze_until)
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(t)
}

/// Starts a session to help you settle an item: on the worker its context
/// names, else the one your latest session used; with the item's context.
pub async fn start_session(st: &HeadState, id: i64) -> anyhow::Result<api::Session> {
    let t = get(st, id).await?.ok_or_else(|| not_found("no such action item"))?;
    let workers = st.store.workers().await?;
    let online: Vec<_> = workers
        .iter()
        .filter(|w| w.revoked_at.is_none() && w.role != "memex" && w.id.parse().ok().is_some_and(|id| st.registry.get(id).is_some()))
        .collect();
    let wanted = t.context["worker"].as_str().unwrap_or("");
    let worker = online
        .iter()
        .find(|w| !wanted.is_empty() && (w.name == wanted || w.id == wanted))
        .or_else(|| online.first())
        .ok_or_else(|| bad_request("no worker is online to work on it"))?;
    let prompt = format!(
        "Help me settle this action item.\n\nAction item #{} ({}): {}\n{}\n\nContext: {}\n\nIf it names facts, read them with \
knowledge_list (by id) and their evidence. Ask me what you need, settle it with me, and record the outcome: knowledge_set for facts I \
confirm, correct or retire, knowledge_record for anything new, and todo_resolve with a one-line resolution when we are done.",
        t.id,
        t.kind,
        t.title,
        t.detail,
        serde_json::to_string_pretty(&t.context).unwrap_or_default()
    );
    let req: api::CreateSession = serde_json::from_value(json!({
        "worker_id": worker.id, "cwd": "", "connection_id": "", "model": "",
        "prompt": prompt, "title": format!("To do: {}", crate::util_clip(&t.title, 60)), "labels": ["todo"],
    }))?;
    let session = crate::sessions::create(st, req).await?;
    let mut sessions = t.sessions.clone();
    sessions.push(session.id.clone());
    sqlx::query("UPDATE todos SET sessions = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&sessions)?)
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(session)
}

/// An agent settles the item it was started for (or raises a new one).
pub async fn from_session(st: &HeadState, session_id: &str, what: &str, params: &Value) -> anyhow::Result<Value> {
    match what {
        "todo" => {
            let mut t: api::TodoIn = serde_json::from_value(params.clone()).map_err(|e| bad_request(format!("{e}")))?;
            t.source = format!("session:{session_id}");
            let mut ctx = if t.context.is_object() { t.context.clone() } else { json!({}) };
            ctx["session"] = json!(session_id);
            t.context = ctx;
            let todo = add(st, &t).await?;
            Ok(json!({"id": todo.id, "count": todo.count}))
        }
        "todo_resolve" => {
            let id = params["id"].as_i64().ok_or_else(|| bad_request("id is required"))?;
            let t = edit(
                st,
                id,
                &api::TodoEdit {
                    state: Some("done".into()),
                    resolution: params["resolution"].as_str().map(str::to_owned),
                    snooze_until: None,
                },
            )
            .await?;
            Ok(json!({"id": t.id, "state": t.state}))
        }
        other => Err(bad_request(format!("unknown session.{other}"))),
    }
}
