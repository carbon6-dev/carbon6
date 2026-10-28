//! Chat (docs/slack.md, docs/chat.md): messages of the shared kind `chat`,
//! whichever app brings them (Slack now), read by conversation for the
//! Chat page. Each item's `meta` says where it is: `workspace`, `channel`,
//! `ts`, and `thread_ts` for a reply. Beside them: the conversations and
//! their read state, the people, and your organization of them.

use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;

/// `meta` fields, as SQL.
fn f(field: &str) -> String {
    format!("json_extract(meta, '$.{field}')")
}

/// A ts as ms (as items' `created_at` has it), in SQL.
fn ms(ts: &str) -> String {
    format!("CAST(CAST({ts} AS REAL) * 1000 AS INTEGER)")
}

/// Not a thread's reply (a first message, or one with no thread).
fn top() -> String {
    format!("({t} IS NULL OR {t} = {ts})", t = f("thread_ts"), ts = f("ts"))
}

/// Not yours.
fn theirs() -> String {
    format!("COALESCE({}, 0) = 0", f("from_me"))
}

const COLUMNS: &str = "id, title, body, url, created_at, meta";

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

/// A chat message came in: its conversation is known, and when it last
/// had one.
pub async fn seen(m: &Memex, meta: &Value, at: i64) -> anyhow::Result<()> {
    let s = |k: &str| meta[k].as_str().unwrap_or_default().to_owned();
    let (w, c) = (s("workspace"), s("channel"));
    if w.is_empty() || c.is_empty() {
        return Ok(());
    }
    let kind = Some(s("channel_kind")).filter(|k| !k.is_empty()).unwrap_or_else(|| "public".into());
    sqlx::query(
        "INSERT INTO chat_conversations (workspace, channel, workspace_name, name, kind, last_at) VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT (workspace, channel) DO UPDATE SET \
           workspace_name = CASE WHEN excluded.workspace_name = '' THEN workspace_name ELSE excluded.workspace_name END, \
           name = CASE WHEN excluded.name = '' THEN name ELSE excluded.name END, \
           last_at = MAX(last_at, excluded.last_at)",
    )
    .bind(&w)
    .bind(&c)
    .bind(s("workspace_name"))
    .bind(s("channel_name"))
    .bind(kind)
    .bind(at)
    .execute(&m.store.pool)
    .await?;
    Ok(())
}

/// What a chat app says about its conversations and people (the head's
/// ingest of its kit's `chat`): kept, and a conversation's read state
/// follows the service's when that changed.
pub async fn put(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let mut n = 0;
    for c in p["conversations"].as_array().into_iter().flatten() {
        let s = |k: &str| c[k].as_str().unwrap_or_default().to_owned();
        let (w, ch) = (s("workspace"), s("channel"));
        if w.is_empty() || ch.is_empty() {
            continue;
        }
        sqlx::query("INSERT OR IGNORE INTO chat_conversations (workspace, channel) VALUES (?, ?)")
            .bind(&w)
            .bind(&ch)
            .execute(&m.store.pool)
            .await?;
        for (k, col) in [
            ("workspace_name", "workspace_name"),
            ("name", "name"),
            ("kind", "kind"),
            ("topic", "topic"),
            ("purpose", "purpose"),
            ("user", "user"),
        ] {
            if let Some(v) = c[k].as_str() {
                let sql = format!("UPDATE chat_conversations SET {col} = ? WHERE workspace = ? AND channel = ?");
                sqlx::query(sqlx::AssertSqlSafe(sql)).bind(v).bind(&w).bind(&ch).execute(&m.store.pool).await?;
            }
        }
        if let Some(v) = c["members"].as_i64() {
            sqlx::query("UPDATE chat_conversations SET members = ? WHERE workspace = ? AND channel = ?")
                .bind(v)
                .bind(&w)
                .bind(&ch)
                .execute(&m.store.pool)
                .await?;
        }
        if let Some(v) = c["archived"].as_bool() {
            sqlx::query("UPDATE chat_conversations SET archived = ? WHERE workspace = ? AND channel = ?")
                .bind(v)
                .bind(&w)
                .bind(&ch)
                .execute(&m.store.pool)
                .await?;
        }
        // Read elsewhere (or marked unread there): that's where you are.
        if let Some(v) = c["last_read"].as_str().filter(|v| !v.is_empty()) {
            sqlx::query(
                "UPDATE chat_conversations SET read_ts = ?1, slack_read = ?1 WHERE workspace = ?2 AND channel = ?3 AND slack_read IS NOT ?1",
            )
            .bind(v)
            .bind(&w)
            .bind(&ch)
            .execute(&m.store.pool)
            .await?;
        }
        n += 1;
    }
    for u in p["people"].as_array().into_iter().flatten() {
        let s = |k: &str| u[k].as_str().unwrap_or_default().to_owned();
        if s("workspace").is_empty() || s("id").is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT INTO chat_people (workspace, id, name, email, avatar, title, status, bot) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (workspace, id) DO UPDATE SET name = excluded.name, email = excluded.email, avatar = excluded.avatar, \
             title = excluded.title, status = excluded.status, bot = excluded.bot",
        )
        .bind(s("workspace"))
        .bind(s("id"))
        .bind(Some(s("name")).filter(|x| !x.is_empty()).unwrap_or_else(|| s("id")))
        .bind(s("email"))
        .bind(s("avatar"))
        .bind(s("title"))
        .bind(s("status"))
        .bind(u["bot"].as_bool().unwrap_or(false))
        .execute(&m.store.pool)
        .await?;
        n += 1;
    }
    Ok(json!({"kept": n}))
}

/// Unread messages of a conversation `c` (an alias), and those of them
/// that need you (mention you, or in a DM or group), as SQL. `+c.workspace`:
/// without the column's affinity, items_chat is searched (not every chat
/// item walked for each conversation).
fn counts() -> (String, String) {
    let base = format!(
        "SELECT COUNT(*) FROM items i WHERE i.kind = 'chat' AND {w} = +c.workspace AND {ch} = +c.channel \
         AND c.read_ts IS NOT NULL AND i.created_at > {read} AND {top} AND {theirs}",
        w = "json_extract(i.meta, '$.workspace')",
        ch = "json_extract(i.meta, '$.channel')",
        read = ms("c.read_ts"),
        top = top().replace("meta", "i.meta"),
        theirs = theirs().replace("meta", "i.meta"),
    );
    let mentions = format!("{base} AND (c.kind IN ('dm', 'group') OR json_extract(i.meta, '$.mentions_me') = 1)");
    (format!("({base})"), format!("({mentions})"))
}

/// The conversations: each workspace's channels, DMs and groups, the most
/// recently active first, with what's unread, your prefs, and a DM's
/// person.
pub async fn channels(m: &Memex) -> anyhow::Result<Value> {
    let (unread, mentions) = counts();
    let sql = format!(
        "SELECT c.*, {unread} AS unread, {mentions} AS mentions, \
           COALESCE(p.starred, 0) AS starred, COALESCE(p.muted, 0) AS muted, COALESCE(p.hidden, 0) AS hidden, COALESCE(p.section, '') AS section, \
           u.name AS user_name, u.avatar AS user_avatar, u.status AS user_status, u.bot AS user_bot \
         FROM chat_conversations c \
         LEFT JOIN chat_prefs p ON p.workspace = c.workspace AND p.channel = c.channel \
         LEFT JOIN chat_people u ON u.workspace = c.workspace AND u.id = c.user \
         ORDER BY c.last_at DESC"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(&m.store.pool).await?;
    Ok(json!(rows.iter().map(conversation).collect::<Vec<_>>()))
}

fn conversation(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "workspace": r.get::<String, _>("workspace"),
        "workspace_name": r.get::<String, _>("workspace_name"),
        "channel": r.get::<String, _>("channel"),
        "channel_name": r.get::<String, _>("name"),
        "channel_kind": r.get::<String, _>("kind"),
        "topic": r.get::<String, _>("topic"),
        "purpose": r.get::<String, _>("purpose"),
        "members": r.get::<Option<i64>, _>("members"),
        "archived": r.get::<bool, _>("archived"),
        "last_at": r.get::<i64, _>("last_at"),
        "read_ts": r.get::<Option<String>, _>("read_ts"),
        "unread": r.get::<i64, _>("unread"),
        "mentions": r.get::<i64, _>("mentions"),
        "starred": r.get::<bool, _>("starred"),
        "muted": r.get::<bool, _>("muted"),
        "hidden": r.get::<bool, _>("hidden"),
        "section": r.get::<String, _>("section"),
        "user": r.get::<Option<String>, _>("user"),
        "user_name": r.get::<Option<String>, _>("user_name"),
        "user_avatar": r.get::<Option<String>, _>("user_avatar"),
        "user_status": r.get::<Option<String>, _>("user_status"),
        "user_bot": r.get::<Option<bool>, _>("user_bot").unwrap_or(false),
    })
}

/// A conversation's messages, newest first, without thread replies (each
/// reply's parent says how many it has): older than `before`, or `around`
/// a message (half on each side, it included).
pub async fn messages(
    m: &Memex,
    workspace: &str,
    channel: &str,
    before: Option<i64>,
    around: Option<&str>,
    limit: u32,
) -> anyhow::Result<Value> {
    let limit = limit.clamp(1, 200) as i64;
    let page = |cond: &str, order: &str| {
        format!(
            "SELECT {COLUMNS} FROM items WHERE kind = 'chat' AND {w} = ? AND {c} = ? AND {top} AND {cond} ORDER BY created_at {order} LIMIT ?",
            w = f("workspace"),
            c = f("channel"),
            top = top(),
        )
    };
    let Some(ts) = around else {
        let rows = sqlx::query(sqlx::AssertSqlSafe(page("(?1 IS NULL OR created_at < ?1)".replace("?1", "?").as_str(), "DESC")))
            .bind(workspace)
            .bind(channel)
            .bind(before)
            .bind(before)
            .bind(limit)
            .fetch_all(&m.store.pool)
            .await?;
        return Ok(json!(rows.iter().map(message).collect::<Vec<_>>()));
    };
    // The message's own time: a reply's is its thread's first message.
    let at = (ts.parse::<f64>().unwrap_or(0.0) * 1000.0) as i64;
    let older = sqlx::query(sqlx::AssertSqlSafe(page("created_at <= ?", "DESC")))
        .bind(workspace)
        .bind(channel)
        .bind(at)
        .bind(limit / 2 + 1)
        .fetch_all(&m.store.pool)
        .await?;
    let newer = sqlx::query(sqlx::AssertSqlSafe(page("created_at > ?", "ASC")))
        .bind(workspace)
        .bind(channel)
        .bind(at)
        .bind(limit / 2)
        .fetch_all(&m.store.pool)
        .await?;
    let mut out: Vec<Value> = newer.iter().rev().map(message).collect();
    out.extend(older.iter().map(message));
    Ok(json!(out))
}

/// A thread: its first message and the replies, oldest first.
pub async fn thread(m: &Memex, workspace: &str, channel: &str, thread_ts: &str) -> anyhow::Result<Value> {
    let sql = format!(
        "SELECT {COLUMNS} FROM items WHERE kind = 'chat' AND {w} = ? AND {c} = ? \
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

/// Read up to `ts` (here; the caller tells the service).
async fn set_read(m: &Memex, workspace: &str, channel: &str, ts: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO chat_conversations (workspace, channel, read_ts) VALUES (?1, ?2, ?3) \
         ON CONFLICT (workspace, channel) DO UPDATE SET read_ts = ?3",
    )
    .bind(workspace)
    .bind(channel)
    .bind(ts)
    .execute(&m.store.pool)
    .await?;
    Ok(())
}

/// Unread from `ts` on: read up to the message before it (or nothing).
async fn unread_from(m: &Memex, workspace: &str, channel: &str, ts: &str) -> anyhow::Result<String> {
    let at = (ts.parse::<f64>().unwrap_or(0.0) * 1000.0) as i64;
    let sql = format!(
        "SELECT {t} FROM items WHERE kind = 'chat' AND {w} = ? AND {c} = ? AND {top} AND created_at < ? ORDER BY created_at DESC LIMIT 1",
        t = f("ts"),
        w = f("workspace"),
        c = f("channel"),
        top = top(),
    );
    let before: Option<String> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(workspace).bind(channel).bind(at).fetch_optional(&m.store.pool).await?.flatten();
    // Slack's "nothing read" is 0000000000.000000.
    let to = before.unwrap_or_else(|| "0000000000.000000".into());
    set_read(m, workspace, channel, &to).await?;
    Ok(to)
}

/// Your organization of a conversation (only the fields given change).
async fn prefs(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
    let (w, c) = (s("workspace"), s("channel"));
    anyhow::ensure!(!w.is_empty() && !c.is_empty(), "prefs need a workspace and a channel");
    sqlx::query("INSERT OR IGNORE INTO chat_prefs (workspace, channel) VALUES (?, ?)").bind(&w).bind(&c).execute(&m.store.pool).await?;
    for k in ["starred", "muted", "hidden"] {
        if let Some(v) = p[k].as_bool() {
            let sql = format!("UPDATE chat_prefs SET {k} = ? WHERE workspace = ? AND channel = ?");
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(v).bind(&w).bind(&c).execute(&m.store.pool).await?;
        }
    }
    if let Some(v) = p["section"].as_str() {
        sqlx::query("UPDATE chat_prefs SET section = ? WHERE workspace = ? AND channel = ?")
            .bind(v.trim())
            .bind(&w)
            .bind(&c)
            .execute(&m.store.pool)
            .await?;
    }
    Ok(json!({"ok": true}))
}

/// Your sections, in order (each workspace's).
async fn sections(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let w = p["workspace"].as_str().unwrap_or_default();
    if let Some(names) = p["names"].as_array() {
        let names: Vec<String> = names.iter().filter_map(|n| n.as_str().map(|s| s.trim().to_owned())).filter(|n| !n.is_empty()).collect();
        sqlx::query(
            "INSERT INTO chat_prefs (workspace, channel, sections) VALUES (?1, '', ?2) ON CONFLICT (workspace, channel) DO UPDATE SET sections = ?2",
        )
        .bind(w)
        .bind(serde_json::to_string(&names)?)
        .execute(&m.store.pool)
        .await?;
    }
    let rows = sqlx::query("SELECT workspace, sections FROM chat_prefs WHERE channel = ''").fetch_all(&m.store.pool).await?;
    let mut out = serde_json::Map::new();
    for r in rows {
        out.insert(r.get("workspace"), serde_json::from_str(&r.get::<String, _>("sections")).unwrap_or(json!([])));
    }
    Ok(Value::Object(out))
}

/// The people, and which is you in each workspace (who your messages are
/// from).
async fn people(m: &Memex, workspace: Option<&str>) -> anyhow::Result<Value> {
    let sql = format!(
        "SELECT DISTINCT {w} AS w, {u} AS u FROM (SELECT meta FROM items WHERE kind = 'chat' AND {mine} = 1 ORDER BY created_at DESC LIMIT 200)",
        w = f("workspace"),
        u = f("user"),
        mine = f("from_me"),
    );
    let me: Vec<(Option<String>, Option<String>)> =
        sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(&m.store.pool).await?.iter().map(|r| (r.get("w"), r.get("u"))).collect();
    let rows = sqlx::query(
        "SELECT workspace, id, name, email, avatar, title, status, bot FROM chat_people WHERE ?1 IS NULL OR workspace = ?1 ORDER BY name COLLATE NOCASE",
    )
    .bind(workspace)
    .fetch_all(&m.store.pool)
    .await?;
    Ok(json!(
        rows.iter()
            .map(|r| json!({
                "workspace": r.get::<String, _>("workspace"), "id": r.get::<String, _>("id"), "name": r.get::<String, _>("name"),
                "email": r.get::<String, _>("email"), "avatar": r.get::<String, _>("avatar"), "title": r.get::<String, _>("title"),
                "status": r.get::<String, _>("status"), "bot": r.get::<bool, _>("bot"),
                "me": me.iter().any(|(w, u)| w.as_deref() == Some(&r.get::<String, _>("workspace")) && u.as_deref() == Some(&r.get::<String, _>("id"))),
            }))
            .collect::<Vec<_>>()
    ))
}

/// Every conversation with something unread (a muted one only when it
/// needs you), with its unread messages, oldest first.
async fn unreads(m: &Memex) -> anyhow::Result<Value> {
    let all = channels(m).await?;
    let mut out = Vec::new();
    for c in all.as_array().into_iter().flatten() {
        let (unread, mentions) = (c["unread"].as_i64().unwrap_or(0), c["mentions"].as_i64().unwrap_or(0));
        if unread == 0 || c["hidden"] == true || (c["muted"] == true && mentions == 0) {
            continue;
        }
        let sql = format!(
            "SELECT {COLUMNS} FROM items WHERE kind = 'chat' AND {w} = ? AND {ch} = ? AND created_at > {read} AND {top} \
             ORDER BY created_at DESC LIMIT 50",
            w = f("workspace"),
            ch = f("channel"),
            read = ms("?"),
            top = top(),
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(c["workspace"].as_str())
            .bind(c["channel"].as_str())
            .bind(c["read_ts"].as_str())
            .fetch_all(&m.store.pool)
            .await?;
        let mut c = c.clone();
        c["messages"] = json!(rows.iter().rev().map(message).collect::<Vec<_>>());
        out.push(c);
    }
    Ok(json!(out))
}

/// Threads you're in (you wrote in them, or they mention you), the most
/// recently active first: the first message, the latest reply, and the
/// replies since yours or since you looked.
async fn threads(m: &Memex, limit: u32) -> anyhow::Result<Value> {
    let since = c6_core::now_ms() - 90 * 86_400_000;
    let sql = format!(
        "SELECT DISTINCT {w} AS w, {c} AS c, {t} AS t FROM items WHERE kind = 'chat' AND {t} IS NOT NULL AND created_at > ? \
         AND ({mine} = 1 OR {mention} = 1)",
        w = f("workspace"),
        c = f("channel"),
        t = f("thread_ts"),
        mine = f("from_me"),
        mention = f("mentions_me"),
    );
    let ids = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(since).fetch_all(&m.store.pool).await?;
    let mut out = Vec::new();
    for r in ids {
        let (w, c, t): (String, String, String) = (r.get("w"), r.get("c"), r.get("t"));
        let all = thread(m, &w, &c, &t).await?;
        let list = all.as_array().cloned().unwrap_or_default();
        let replies: Vec<&Value> = list.iter().filter(|x| x["meta"]["ts"].as_str() != Some(&t)).collect();
        let Some(latest) = replies.last() else { continue };
        let read: Option<String> =
            sqlx::query_scalar("SELECT read_ts FROM chat_thread_reads WHERE workspace = ? AND channel = ? AND thread_ts = ?")
                .bind(&w)
                .bind(&c)
                .bind(&t)
                .fetch_optional(&m.store.pool)
                .await?;
        let ts_of = |x: &Value| x["meta"]["ts"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let mine = replies.iter().filter(|x| x["meta"]["from_me"] == true).map(|x| ts_of(x)).fold(0.0, f64::max);
        let seen = read.and_then(|r| r.parse::<f64>().ok()).unwrap_or(0.0).max(mine);
        let new = replies.iter().filter(|x| x["meta"]["from_me"] != true && ts_of(x) > seen).count();
        out.push(json!({
            "workspace": w, "channel": c, "thread_ts": t,
            "channel_name": latest["meta"]["channel_name"], "workspace_name": latest["meta"]["workspace_name"],
            "first": list.iter().find(|x| x["meta"]["ts"].as_str() == Some(&t)), "latest": latest,
            "replies": replies.len(), "new": new, "last_at": latest["created_at"],
        }));
    }
    out.sort_by_key(|x| std::cmp::Reverse(x["last_at"].as_i64().unwrap_or(0)));
    out.truncate(limit.clamp(1, 200) as usize);
    Ok(json!(out))
}

/// Messages that mention you, and yours that got reactions, newest first.
async fn activity(m: &Memex, limit: u32) -> anyhow::Result<Value> {
    let limit = limit.clamp(1, 200) as i64;
    let q = |cond: String| format!("SELECT {COLUMNS} FROM items WHERE kind = 'chat' AND {cond} ORDER BY created_at DESC LIMIT ?");
    let mentions = sqlx::query(sqlx::AssertSqlSafe(q(format!("{} = 1 AND {}", f("mentions_me"), theirs()))))
        .bind(limit)
        .fetch_all(&m.store.pool)
        .await?;
    let reacted = sqlx::query(sqlx::AssertSqlSafe(q(format!("{} = 1 AND {} IS NOT NULL", f("from_me"), f("reactions")))))
        .bind(limit)
        .fetch_all(&m.store.pool)
        .await?;
    let mut out: Vec<Value> = mentions
        .iter()
        .map(|r| json!({"why": "mention", "message": message(r)}))
        .chain(reacted.iter().map(|r| json!({"why": "reaction", "message": message(r)})))
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x["message"]["created_at"].as_i64().unwrap_or(0)));
    out.truncate(limit as usize);
    Ok(json!(out))
}

async fn saved(m: &Memex, done: bool) -> anyhow::Result<Value> {
    let sql = "SELECT i.id, i.title, i.body, i.url, i.created_at, i.meta, s.saved_at, s.done_at FROM chat_saved s JOIN items i ON i.id = s.item_id \
         WHERE (s.done_at IS NOT NULL) = ? ORDER BY s.saved_at DESC";
    let rows = sqlx::query(sql).bind(done).fetch_all(&m.store.pool).await?;
    Ok(json!(
        rows.iter()
            .map(|r| {
                let mut v = message(r);
                v["saved_at"] = json!(r.get::<i64, _>("saved_at"));
                v["done_at"] = json!(r.get::<Option<i64>, _>("done_at"));
                v
            })
            .collect::<Vec<_>>()
    ))
}

async fn save(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let id = p["id"].as_str().unwrap_or_default();
    anyhow::ensure!(!id.is_empty(), "save needs a message's id");
    if p["remove"] == true {
        sqlx::query("DELETE FROM chat_saved WHERE item_id = ?").bind(id).execute(&m.store.pool).await?;
        return Ok(json!({"ok": true}));
    }
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM items WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?;
    anyhow::ensure!(kind.as_deref() == Some("chat"), "no chat message {id}");
    let done = p["done"].as_bool().map(|d| d.then(c6_core::now_ms));
    sqlx::query("INSERT INTO chat_saved (item_id, saved_at) VALUES (?, ?) ON CONFLICT (item_id) DO NOTHING")
        .bind(id)
        .bind(c6_core::now_ms())
        .execute(&m.store.pool)
        .await?;
    if let Some(d) = done {
        sqlx::query("UPDATE chat_saved SET done_at = ? WHERE item_id = ?").bind(d).bind(id).execute(&m.store.pool).await?;
    }
    Ok(json!({"ok": true}))
}

/// Chat messages by words and filters: `in` (a conversation's id or
/// name), `from` (a person's id or name), `has` (link, file, reaction),
/// `before`/`after` (ms), `thread` (replies only), `workspace`.
async fn search(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| p[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
    let mut cond = vec!["i.kind = 'chat'".to_owned()];
    let mut binds: Vec<Value> = Vec::new();
    let mf = |k: &str| format!("json_extract(i.meta, '$.{k}')");
    if let Some(w) = s("workspace") {
        cond.push(format!("{} = ?", mf("workspace")));
        binds.push(json!(w));
    }
    if let Some(c) = s("in") {
        let name = c.trim_start_matches(['#', '@']).to_owned();
        cond.push(format!("({} = ? OR ltrim({}, '#@') = ? COLLATE NOCASE)", mf("channel"), mf("channel_name")));
        binds.extend([json!(c), json!(name)]);
    }
    if let Some(u) = s("from") {
        let name = u.trim_start_matches('@').to_owned();
        if name == "me" {
            cond.push(format!("{} = 1", mf("from_me")));
        } else {
            cond.push(format!("({} = ? OR {} = ? COLLATE NOCASE OR {} = ? COLLATE NOCASE)", mf("user"), mf("user_name"), mf("user_email")));
            binds.extend([json!(u), json!(name), json!(name)]);
        }
    }
    match s("has").as_deref() {
        Some("link") => cond.push("(i.body LIKE '%http://%' OR i.body LIKE '%https://%')".into()),
        Some("file") => cond.push(format!("{} IS NOT NULL", mf("files"))),
        Some("reaction") => cond.push(format!("{} IS NOT NULL", mf("reactions"))),
        _ => {}
    }
    if let Some(b) = p["before"].as_i64() {
        cond.push("i.created_at < ?".into());
        binds.push(json!(b));
    }
    if let Some(a) = p["after"].as_i64() {
        cond.push("i.created_at > ?".into());
        binds.push(json!(a));
    }
    if p["thread"] == true {
        cond.push(format!("{t} IS NOT NULL AND {t} != {ts}", t = mf("thread_ts"), ts = mf("ts")));
    }
    let words: Vec<String> = s("q")
        .unwrap_or_default()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect();
    let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, 200) as i64;
    let sql = if words.is_empty() {
        anyhow::ensure!(cond.len() > 1, "search needs words or a filter");
        format!(
            "SELECT i.id, i.title, i.body, i.url, i.created_at, i.meta FROM items i WHERE {} ORDER BY i.created_at DESC LIMIT ?",
            cond.join(" AND ")
        )
    } else {
        format!(
            "SELECT i.id, i.title, i.body, i.url, i.created_at, i.meta FROM items i WHERE {} AND i.id IN \
             (SELECT p.item_id FROM parts_fts f JOIN parts p ON p.id = f.rowid WHERE parts_fts MATCH ?) ORDER BY i.created_at DESC LIMIT ?",
            cond.join(" AND ")
        )
    };
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for b in &binds {
        q = match b {
            Value::Number(n) => q.bind(n.as_i64()),
            other => q.bind(other.as_str().unwrap_or_default().to_owned()),
        };
    }
    if !words.is_empty() {
        q = q.bind(words.join(" "));
    }
    let rows = q.bind(limit).fetch_all(&m.store.pool).await?;
    Ok(json!(rows.iter().map(message).collect::<Vec<_>>()))
}

pub async fn method(m: &Memex, method: &str, params: &Value, allowed: &[String]) -> anyhow::Result<Value> {
    anyhow::ensure!(allowed.iter().any(|k| k == "chat"), "chat is outside what this caller may see");
    let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
    let limit = |d: u64| params["limit"].as_u64().unwrap_or(d) as u32;
    match method {
        "memex.chat.channels" => channels(m).await,
        "memex.chat.messages" => {
            messages(m, &s("workspace"), &s("channel"), params["before"].as_i64(), params["around"].as_str(), limit(50)).await
        }
        "memex.chat.thread" => thread(m, &s("workspace"), &s("channel"), &s("thread_ts")).await,
        "memex.chat.put" => put(m, params).await,
        "memex.chat.prefs" => prefs(m, params).await,
        "memex.chat.sections" => sections(m, params).await,
        "memex.chat.people" => people(m, params["workspace"].as_str()).await,
        "memex.chat.unreads" => unreads(m).await,
        "memex.chat.threads" => threads(m, limit(50)).await,
        "memex.chat.activity" => activity(m, limit(50)).await,
        "memex.chat.saved" => saved(m, params["done"] == true).await,
        "memex.chat.save" => save(m, params).await,
        "memex.chat.search" => search(m, params).await,
        "memex.chat.thread_read" => {
            sqlx::query(
                "INSERT INTO chat_thread_reads (workspace, channel, thread_ts, read_ts) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (workspace, channel, thread_ts) DO UPDATE SET read_ts = ?4",
            )
            .bind(s("workspace"))
            .bind(s("channel"))
            .bind(s("thread_ts"))
            .bind(s("ts"))
            .execute(&m.store.pool)
            .await?;
            Ok(json!({"ok": true}))
        }
        // Read here, and in the service (as far as it lets us).
        "memex.chat.mark_read" | "memex.chat.unread" => {
            let ts = if method == "memex.chat.unread" {
                unread_from(m, &s("workspace"), &s("channel"), &s("ts")).await?
            } else {
                set_read(m, &s("workspace"), &s("channel"), &s("ts")).await?;
                s("ts")
            };
            let a = m.account("chat", &s("workspace"))?;
            let input = json!({"account": a.id, "channel": s("channel"), "ts": ts});
            m.app_call(&a, "mark_read", input, params["made_by"].as_str().unwrap_or("you")).await?;
            Ok(json!({"ok": true, "read_ts": ts}))
        }
        // Through the app that brings the workspace (its operations ask
        // first by policy, for agents).
        "memex.chat.post" | "memex.chat.react" | "memex.chat.edit" | "memex.chat.delete" => {
            let a = m.account("chat", &s("workspace"))?;
            let by = params["made_by"].as_str().unwrap_or("you").to_owned();
            let op = match method {
                "memex.chat.edit" => "update",
                other => other.trim_start_matches("memex.chat."),
            };
            // The operation's own fields (callers' context stays out).
            let mut input = serde_json::Map::new();
            input.insert("account".into(), json!(a.id));
            for k in ["channel", "text", "thread_ts", "ts", "name", "remove", "broadcast"] {
                if !params[k].is_null() {
                    input.insert(k.into(), params[k].clone());
                }
            }
            m.app_call(&a, op, Value::Object(input), &by).await
        }
        other => anyhow::bail!("the memex has no method {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unread_counts_search_the_chat_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("memex.db")).await.unwrap();
        let (unread, mentions) = counts();
        let sql = format!("EXPLAIN QUERY PLAN SELECT {unread}, {mentions} FROM chat_conversations c");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(&store.pool).await.unwrap();
        let plan: Vec<String> = rows.iter().map(|r| r.get("detail")).collect();
        let searches = plan.iter().filter(|d| d.starts_with("SEARCH i USING INDEX items_chat (<expr>=? AND <expr>=?")).count();
        assert_eq!(searches, 2, "{plan:#?}");
    }
}
