//! Chat (docs/slack.md): messages of the shared kind `chat`, whichever
//! app brings them (Slack now), read by conversation for the Chat page.
//! Each item's `meta` says where it is: `workspace`, `channel`, `ts`, and
//! `thread_ts` for a reply.

use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;

/// `meta` fields, as SQL.
fn f(field: &str) -> String {
    format!("json_extract(meta, '$.{field}')")
}

fn message(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "title": r.get::<String, _>("title"),
        "text": r.get::<String, _>("body"),
        "url": r.get::<Option<String>, _>("url"),
        "created_at": r.get::<i64, _>("created_at"),
        "meta": serde_json::from_str::<Value>(&r.get::<String, _>("meta")).unwrap_or_default(),
    })
}

/// The conversations: each workspace's channels, DMs and groups, the most
/// recently active first.
pub async fn channels(m: &Memex) -> anyhow::Result<Value> {
    let sql = format!(
        "SELECT {w} AS workspace, MAX({wn}) AS workspace_name, {c} AS channel, MAX({cn}) AS channel_name, MAX({ck}) AS channel_kind, \
         MAX(created_at) AS last_at, COUNT(*) AS messages FROM items WHERE kind = 'chat' GROUP BY {w}, {c} ORDER BY last_at DESC",
        w = f("workspace"),
        wn = f("workspace_name"),
        c = f("channel"),
        cn = f("channel_name"),
        ck = f("channel_kind"),
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(&m.store.pool).await?;
    Ok(json!(
        rows.iter()
            .map(|r| json!({
                "workspace": r.get::<Option<String>, _>("workspace"),
                "workspace_name": r.get::<Option<String>, _>("workspace_name"),
                "channel": r.get::<Option<String>, _>("channel"),
                "channel_name": r.get::<Option<String>, _>("channel_name"),
                "channel_kind": r.get::<Option<String>, _>("channel_kind"),
                "last_at": r.get::<i64, _>("last_at"),
                "messages": r.get::<i64, _>("messages"),
            }))
            .collect::<Vec<_>>()
    ))
}

/// A conversation's messages, newest first (`before`: older than that),
/// without thread replies: each reply's parent says how many it has.
pub async fn messages(m: &Memex, workspace: &str, channel: &str, before: Option<i64>, limit: u32) -> anyhow::Result<Value> {
    let sql = format!(
        "SELECT id, title, body, url, created_at, meta FROM items WHERE kind = 'chat' AND {w} = ? AND {c} = ? \
         AND ({t} IS NULL OR {t} = {ts}) AND (? IS NULL OR created_at < ?) ORDER BY created_at DESC LIMIT ?",
        w = f("workspace"),
        c = f("channel"),
        t = f("thread_ts"),
        ts = f("ts"),
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(workspace)
        .bind(channel)
        .bind(before)
        .bind(before)
        .bind(limit.clamp(1, 200) as i64)
        .fetch_all(&m.store.pool)
        .await?;
    Ok(json!(rows.iter().map(message).collect::<Vec<_>>()))
}

/// A thread: its first message and the replies, oldest first.
pub async fn thread(m: &Memex, workspace: &str, channel: &str, thread_ts: &str) -> anyhow::Result<Value> {
    let sql = format!(
        "SELECT id, title, body, url, created_at, meta FROM items WHERE kind = 'chat' AND {w} = ? AND {c} = ? \
         AND ({t} = ? OR {ts} = ?) ORDER BY created_at, {ts}",
        w = f("workspace"),
        c = f("channel"),
        t = f("thread_ts"),
        ts = f("ts"),
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(workspace)
        .bind(channel)
        .bind(thread_ts)
        .bind(thread_ts)
        .fetch_all(&m.store.pool)
        .await?;
    Ok(json!(rows.iter().map(message).collect::<Vec<_>>()))
}

pub async fn method(m: &Memex, method: &str, params: &Value, allowed: &[String]) -> anyhow::Result<Value> {
    anyhow::ensure!(allowed.iter().any(|k| k == "chat"), "chat is outside what this caller may see");
    let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
    match method {
        "memex.chat.channels" => channels(m).await,
        "memex.chat.messages" => {
            messages(m, &s("workspace"), &s("channel"), params["before"].as_i64(), params["limit"].as_u64().unwrap_or(50) as u32).await
        }
        "memex.chat.thread" => thread(m, &s("workspace"), &s("channel"), &s("thread_ts")).await,
        // Through the app that brings the workspace (its operations ask
        // first by policy, for agents).
        "memex.chat.post" | "memex.chat.react" | "memex.chat.mark_read" => {
            let a = m.account("chat", &s("workspace"))?;
            let by = params["made_by"].as_str().unwrap_or("you").to_owned();
            let op = method.trim_start_matches("memex.chat.");
            // The operation's own fields (callers' context stays out).
            let mut input = serde_json::Map::new();
            input.insert("account".into(), json!(a.id));
            for k in ["channel", "text", "thread_ts", "ts", "name", "remove"] {
                if !params[k].is_null() {
                    input.insert(k.into(), params[k].clone());
                }
            }
            m.app_call(&a, op, Value::Object(input), &by).await
        }
        other => anyhow::bail!("the memex has no method {other}"),
    }
}
