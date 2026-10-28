//! Syncing a workspace (docs/slack.md §5), and the operations that write
//! to it (§6).
//!
//! Each workspace's records: `team:<id>` (its name and address, its people,
//! files waiting, and a rate limit's wait) and `chan:<id>:<channel>` per
//! conversation (the newest message read, how far back the import is, the
//! last week's messages and threads, for the re-read, and threads whose
//! replies are still to read).

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::{Account, Limited, MAX_FILE, Run, now_ms, slack_error};

const DAY: i64 = 86_400_000;
const WEEK: i64 = 7 * DAY;
/// A conversation with nothing in the last day is due again this long
/// after it was last looked at; one with something is due every run.
const IDLE_EVERY: i64 = 5 * 60_000;
/// Documents the memex reads (the rest keep their name and link).
const READABLE: [&str; 6] = [
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "text/markdown",
    "text/plain",
    "text/html",
    "text/x-rst",
];
/// What the records were made by: a conversation from an earlier one is
/// read again whole (2: the names of people from other organizations, and
/// every thread's replies; 3: which messages are yours, and which mention
/// you).
const REV: i64 = 3;
/// Messages that say nothing (joins, leaves, topic changes).
const NOISE: [&str; 6] = ["channel_join", "channel_leave", "group_join", "group_leave", "channel_purpose", "channel_topic"];

/// A message's time, in ms.
pub fn ts_ms(ts: &str) -> i64 {
    ts.parse::<f64>().map(|s| (s * 1000.0) as i64).unwrap_or(0)
}

fn ms_ts(ms: i64) -> String {
    format!("{}.000000", ms / 1000)
}

/// Who a Slack user is, as the memex knows people: by email when there is
/// one (so they are the person mail knows), else by their Slack id.
fn person_key(team: &str, user: &str, users: &Map<String, Value>) -> String {
    match users.get(user).and_then(|u| u["e"].as_str()).filter(|e| !e.is_empty()) {
        Some(email) => format!("person:{}", email.to_lowercase()),
        None => format!("slack-user:{team}/{user}"),
    }
}

/// A person's name, as Slack's user object has it.
fn display(u: &Value) -> String {
    u["profile"]["real_name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .or(u["real_name"].as_str().filter(|n| !n.is_empty()))
        .or(u["profile"]["display_name"].as_str().filter(|n| !n.is_empty()))
        .or(u["name"].as_str())
        .unwrap_or("")
        .to_owned()
}

/// A person as the team record keeps them: name, email, avatar, title,
/// status, and whether they're an app.
fn entry(id: &str, u: &Value) -> Value {
    let p = &u["profile"];
    let status = format!("{} {}", p["status_emoji"].as_str().unwrap_or(""), p["status_text"].as_str().unwrap_or("")).trim().to_owned();
    json!({
        "n": Some(display(u)).filter(|n| !n.is_empty()).unwrap_or_else(|| id.to_owned()),
        "e": p["email"].as_str().unwrap_or(""),
        "i": p["image_48"].as_str().unwrap_or(""),
        "t": p["title"].as_str().unwrap_or(""),
        "s": status,
        "b": u["is_bot"] == true,
    })
}

/// A person for the Chat page.
fn person(team: &str, id: &str, u: &Value) -> Value {
    json!({"workspace": team, "id": id, "name": u["n"], "email": u["e"], "avatar": u["i"], "title": u["t"], "status": u["s"], "bot": u["b"]})
}

fn user_name(user: &str, users: &Map<String, Value>) -> String {
    users.get(user).and_then(|u| u["n"].as_str()).filter(|n| !n.is_empty()).unwrap_or(user).to_owned()
}

fn channel_key(team: &str, channel: &str) -> String {
    format!("channel:slack/{team}/{channel}")
}

/// Slack's markup as plain text: mentions and channels by name, links as
/// "label (address)", and its escapes undone. Also who it mentions.
pub fn plain(text: &str, users: &Map<String, Value>) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut mentions = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('>') else {
            out.push_str(&rest[i..]);
            rest = "";
            break;
        };
        let inner = &rest[i + 1..i + j];
        let (target, label) = inner.split_once('|').unwrap_or((inner, ""));
        if let Some(u) = target.strip_prefix('@') {
            mentions.push(u.to_owned());
            out.push('@');
            out.push_str(&if label.is_empty() { user_name(u, users) } else { label.to_owned() });
        } else if let Some(c) = target.strip_prefix('#') {
            out.push('#');
            out.push_str(if label.is_empty() { c } else { label });
        } else if let Some(s) = target.strip_prefix('!') {
            out.push('@');
            out.push_str(if label.is_empty() { s.split('^').next().unwrap_or(s) } else { label });
        } else if label.is_empty() || label == target {
            out.push_str(target);
        } else {
            out.push_str(&format!("{label} ({target})"));
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    (out.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&"), mentions)
}

/// A conversation as the sync keeps it.
#[derive(Clone)]
pub struct Conversation {
    pub id: String,
    pub name: String,
    pub kind: String,
    /// When Slack last changed it (ms), for ordering the import.
    pub updated: i64,
    /// What the Chat page shows of it (topic, purpose, …), from the
    /// listing.
    pub info: Value,
}

/// A message as a `chat` item, and its files.
pub fn item(a: &Account, team: &Value, c: &Conversation, m: &Value) -> Option<(Value, Vec<Value>)> {
    let ts = m["ts"].as_str()?;
    if NOISE.contains(&m["subtype"].as_str().unwrap_or("")) {
        return None;
    }
    let users = team["users"].as_object().cloned().unwrap_or_default();
    let user = m["user"].as_str().or(m["bot_id"].as_str()).unwrap_or("").to_owned();
    let who = match m["user"].as_str() {
        Some(u) => user_name(u, &users),
        None => m["username"].as_str().or(m["bot_profile"]["name"].as_str()).unwrap_or("someone").to_owned(),
    };
    let (text, mentions) = plain(m["text"].as_str().unwrap_or(""), &users);
    let files: Vec<Value> = m["files"].as_array().cloned().unwrap_or_default();
    let reactions: Map<String, Value> = m["reactions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| Some((x["name"].as_str()?.to_owned(), json!(x["count"].as_u64().unwrap_or(1)))))
        .collect();
    let thread_ts = m["thread_ts"].as_str().filter(|t| *t != ts);
    let domain = team["domain"].as_str().unwrap_or("app");
    let mut url = format!("https://{domain}.slack.com/archives/{}/p{}", c.id, ts.replace('.', ""));
    if let Some(t) = thread_ts {
        url.push_str(&format!("?thread_ts={t}&cid={}", c.id));
    }
    let mut labels = vec![channel_key(&a.team, &c.id)];
    if let Some(u) = m["user"].as_str() {
        labels.push(person_key(&a.team, u, &users));
    }
    for u in &mentions {
        let k = person_key(&a.team, u, &users);
        if !labels.contains(&k) {
            labels.push(k);
        }
    }
    let file_names: Vec<String> = files.iter().filter_map(|f| f["name"].as_str().map(str::to_owned)).collect();
    let body = match (text.trim().is_empty(), file_names.is_empty()) {
        (true, false) => format!("{who} shared {}", file_names.join(", ")),
        _ => format!("{who}: {text}"),
    };
    let mut meta = json!({
        "service": "slack", "workspace": a.team, "workspace_name": team["name"].as_str().unwrap_or(&a.name),
        "channel": c.id, "channel_name": c.name, "channel_kind": c.kind, "ts": ts, "user": user, "user_name": who,
    });
    // Who they are by address (the person mail knows), when Slack says.
    if let Some(e) = m["user"].as_str().and_then(|u| users.get(u)).and_then(|u| u["e"].as_str()).filter(|e| !e.is_empty()) {
        meta["user_email"] = json!(e.to_lowercase());
    }
    // Yours, or mentioning you (for sorting what needs you, docs/brief.md).
    if let Some(me) = team["me"].as_str().filter(|m| !m.is_empty()) {
        meta["from_me"] = json!(m["user"].as_str() == Some(me));
        meta["mentions_me"] = json!(mentions.iter().any(|u| u == me));
    }
    if let Some(t) = thread_ts {
        meta["thread_ts"] = json!(t);
    } else if m["reply_count"].as_u64().unwrap_or(0) > 0 {
        meta["thread_ts"] = json!(ts);
        meta["reply_count"] = json!(m["reply_count"]);
    }
    if !reactions.is_empty() {
        meta["reactions"] = Value::Object(reactions);
        let me = team["me"].as_str().unwrap_or("");
        let mine: Vec<&str> = m["reactions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|x| x["users"].as_array().is_some_and(|u| u.iter().any(|u| u == me)))
            .filter_map(|x| x["name"].as_str())
            .collect();
        if !mine.is_empty() {
            meta["my_reactions"] = json!(mine);
        }
    }
    if m["bot_id"].is_string() || m["subtype"] == "bot_message" {
        meta["bot"] = json!(true);
    }
    if !files.is_empty() {
        meta["files"] = json!(
            files
                .iter()
                .map(|f| json!({"id": f["id"], "name": f["name"], "mime": f["mimetype"], "size": f["size"], "url": f["permalink"]}))
                .collect::<Vec<_>>()
        );
    }
    if m["edited"].is_object() {
        meta["edited"] = json!(true);
    }
    let item = json!({
        "kind": "chat",
        "external_id": format!("{}/{}/{ts}", a.team, c.id),
        "title": format!("{} · {who}", c.name),
        "text": body,
        "url": url,
        "created_at": ts_ms(ts),
        "labels": labels,
        "meta": meta,
    });
    Some((item, files))
}

/// Syncs every connected workspace.
pub async fn sync(r: &mut Run) -> anyhow::Result<()> {
    for a in r.accounts.clone() {
        r.budget = r.per_workspace;
        r.file_bytes = 0;
        let done = workspace(r, &a).await;
        // Slack slowed a pass down: what it read is kept, and the
        // workspace waits.
        if let (Ok(()), Some(s)) = (&done, r.limited.take()) {
            let key = format!("team:{}", a.team);
            let mut team = r.record(&key);
            team["retry_after"] = json!(now_ms() + s as i64 * 1000);
            r.save(&key, team);
        }
        r.limited = None;
        if let Err(e) = done {
            let key = format!("team:{}", a.team);
            let mut team = r.record(&key);
            match e.downcast_ref::<Limited>() {
                Some(Limited(s)) => {
                    team["retry_after"] = json!(now_ms() + *s as i64 * 1000);
                    team["error"] = Value::Null;
                }
                None => {
                    team["error"] = json!(format!("{e:#}"));
                    r.errors.push(format!("{}: {e:#}", a.name));
                }
            }
            r.save(&key, team);
        }
    }
    Ok(())
}

/// Every page of a listing method (`members`/`channels` under `field`).
async fn pages(r: &mut Run, a: &Account, method: &str, query: &[(&str, String)], field: &str) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    loop {
        let mut q = query.to_vec();
        if !cursor.is_empty() {
            q.push(("cursor", cursor.clone()));
        }
        let v = r.call(a, method, &q, None).await?;
        out.extend(v[field].as_array().cloned().unwrap_or_default());
        cursor = v["response_metadata"]["next_cursor"].as_str().unwrap_or("").to_owned();
        if cursor.is_empty() {
            return Ok(out);
        }
    }
}

async fn workspace(r: &mut Run, a: &Account) -> anyhow::Result<()> {
    let tkey = format!("team:{}", a.team);
    let mut team = r.record(&tkey);
    if team["retry_after"].as_i64().unwrap_or(0) > now_ms() {
        return Ok(());
    }
    team["retry_after"] = Value::Null;
    team["error"] = Value::Null;
    // The workspace and its people, once a day.
    if team["domain"].is_null() || team["me"].is_null() || team["users_at"].as_i64().unwrap_or(0) < now_ms() - DAY {
        // Who you are here, so your messages and mentions of you are known.
        let me = r.call(a, "auth.test", &[], None).await?;
        team["me"] = me["user_id"].clone();
        let info = r.call(a, "team.info", &[], None).await?;
        team["domain"] = info["team"]["domain"].clone();
        team["name"] = info["team"]["name"].clone();
        let members = pages(r, a, "users.list", &[("limit", "200".into())], "members").await?;
        // Those looked up one by one (another organization's) stay.
        let mut users: Map<String, Value> =
            team["users"].as_object().into_iter().flatten().filter(|(_, u)| u["x"] == true).map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut listed = Map::new();
        for u in &members {
            let Some(id) = u["id"].as_str() else { continue };
            listed.insert(id.to_owned(), entry(id, u));
        }
        for (id, u) in &listed {
            r.things.push(json!({"kind": "person", "key": person_key(&a.team, id, &listed), "name": u["n"]}));
        }
        for (id, u) in users.iter().chain(&listed) {
            r.people.push(person(&a.team, id, u));
        }
        users.extend(listed);
        team["users"] = Value::Object(users);
        team["users_at"] = json!(now_ms());
        r.save(&tkey, team.clone());
    }
    // The conversations you're in.
    let listed = pages(
        r,
        a,
        "users.conversations",
        &[("types", "public_channel,private_channel,mpim,im".into()), ("exclude_archived", "true".into()), ("limit", "200".into())],
        "channels",
    )
    .await?;
    let dm_people: Vec<String> =
        listed.iter().filter(|c| c["is_im"] == true).filter_map(|c| c["user"].as_str().map(str::to_owned)).collect();
    learn_users(r, a, &mut team, &dm_people).await?;
    let users = team["users"].as_object().cloned().unwrap_or_default();
    let mut convs: Vec<Conversation> = listed
        .iter()
        .filter_map(|c| {
            let id = c["id"].as_str()?.to_owned();
            let (kind, name) = if c["is_im"] == true {
                ("dm", format!("@{}", user_name(c["user"].as_str().unwrap_or(""), &users)))
            } else if c["is_mpim"] == true {
                // mpdm-ana--bo--me-1: the people.
                let n = c["name"].as_str().unwrap_or("").trim_start_matches("mpdm-");
                let n = n.rsplit_once('-').map_or(n, |(p, _)| p);
                ("group", n.split("--").collect::<Vec<_>>().join(", "))
            } else if c["is_private"] == true {
                ("private", format!("#{}", c["name"].as_str().unwrap_or(&id)))
            } else {
                ("public", format!("#{}", c["name"].as_str().unwrap_or(&id)))
            };
            let info = json!({
                "name": name, "kind": kind, "topic": c["topic"]["value"].as_str().unwrap_or(""),
                "purpose": c["purpose"]["value"].as_str().unwrap_or(""), "members": c["num_members"].as_i64(),
                "user": c["user"].as_str(), "archived": c["is_archived"] == true,
            });
            Some(Conversation { id, name, kind: kind.into(), updated: c["updated"].as_i64().unwrap_or(0), info })
        })
        .collect();
    for c in &convs {
        let key = format!("chan:{}:{}", a.team, c.id);
        let mut s = r.record(&key);
        let mut changed = false;
        if s["name"].as_str() != Some(&c.name) {
            r.things.push(json!({"kind": "channel", "key": channel_key(&a.team, &c.id), "name": c.name}));
            s["name"] = json!(c.name);
            s["kind"] = json!(c.kind);
            changed = true;
        }
        if s["info"] != c.info {
            let mut v = c.info.clone();
            v["workspace"] = json!(a.team);
            v["workspace_name"] = team["name"].clone();
            v["channel"] = json!(c.id);
            r.conversations.push(v);
            s["info"] = c.info.clone();
            changed = true;
        }
        // Made by an earlier version: read again, whole (its files aren't
        // fetched again).
        if s["rev"].as_i64().unwrap_or(0) < REV {
            if s["latest_ts"].is_string() || s["import_done"] == true {
                s["import_done"] = json!(false);
                s["oldest_ts"] = Value::Null;
                s["redo"] = json!(true);
            }
            s["rev"] = json!(REV);
            changed = true;
        }
        if changed {
            r.save(&key, s);
        }
    }
    files_waiting(r, a, &mut team).await?;
    r.save(&tkey, team.clone());

    let now = now_ms();
    let chan = |r: &Run, c: &Conversation| r.record(&format!("chan:{}:{}", a.team, c.id));
    // 1. Keeping up: the conversations of the last day every run, the rest
    // in turn, the one looked at longest ago first.
    let mut live: Vec<Conversation> = convs.iter().filter(|c| chan(r, c)["latest_ts"].is_string()).cloned().collect();
    // While an import is under way, keeping up takes half the budget at
    // most, so the import moves on; otherwise the rest in turn leave a
    // quarter for the re-read.
    let importing = convs.iter().any(|c| chan(r, c)["import_done"] != true);
    let keep = if importing { r.per_workspace / 2 } else { 0 };
    let states: Vec<Value> = live.iter().map(|c| chan(r, c)).collect();
    for (i, hot) in turn(&states, now) {
        let floor = if hot { keep } else { keep.max(r.per_workspace / 4) };
        if r.budget < floor.max(2) {
            break;
        }
        conversation(r, a, &mut team, &live[i], Pass::New).await?;
    }
    // The re-read: the most recent first, or while importing the one
    // checked longest ago.
    if importing {
        live.sort_by_key(|c| chan(r, c)["checked_at"].as_i64().unwrap_or(0));
    } else {
        live.sort_by_key(|c| std::cmp::Reverse(chan(r, c)["last_at"].as_i64().unwrap_or(0)));
    }
    // 2. The last week again, for edits, deletions, reactions and replies.
    for c in &live {
        let s = chan(r, c);
        if s["last_at"].as_i64().unwrap_or(0) < now - WEEK || s["reread_at"].as_i64().unwrap_or(0) > now - r.reread_every {
            continue;
        }
        if r.budget < keep.max(4) {
            break;
        }
        conversation(r, a, &mut team, c, Pass::Reread).await?;
    }
    // 3. Importing: DMs and groups first, then the most recently changed.
    convs.retain(|c| chan(r, c)["import_done"] != true);
    convs.sort_by_key(|c| (!matches!(c.kind.as_str(), "dm" | "group"), std::cmp::Reverse(c.updated)));
    for c in &convs {
        while chan(r, c)["import_done"] != true {
            if r.budget < 3 {
                r.again = true;
                r.save(&tkey, team);
                return Ok(());
            }
            conversation(r, a, &mut team, c, Pass::Import).await?;
        }
    }
    // Threads still to read: run again at once.
    if convs.iter().chain(&live).any(|c| chan(r, c)["pending"].as_object().is_some_and(|p| !p.is_empty())) {
        r.again = true;
    }
    r.save(&tkey, team);
    Ok(())
}

/// The conversations keeping up looks at this run (by their sync
/// records), in order, and whether each is hot: those with a message in
/// the last day, or threads waiting, every run; the rest once they were
/// looked at `IDLE_EVERY` ago. Each group the one looked at longest ago
/// first, so a budget too small for all of them reaches every one in turn.
fn turn(states: &[Value], now: i64) -> Vec<(usize, bool)> {
    let mut due: Vec<(bool, i64, usize)> = states
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let checked = s["checked_at"].as_i64().unwrap_or(0);
            let hot = s["last_at"].as_i64().unwrap_or(0) > now - DAY || s["pending"].as_object().is_some_and(|p| !p.is_empty());
            (hot || checked <= now - IDLE_EVERY).then_some((!hot, checked, i))
        })
        .collect();
    due.sort();
    due.into_iter().map(|(cold, _, i)| (i, !cold)).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Pass {
    /// Messages after the newest read.
    New,
    /// The last week, again.
    Reread,
    /// A page further back.
    Import,
}

/// One pass over a conversation. A conversation Slack won't show (you
/// left, it's gone) is done with.
async fn conversation(r: &mut Run, a: &Account, team: &mut Value, c: &Conversation, pass: Pass) -> anyhow::Result<()> {
    let key = format!("chan:{}:{}", a.team, c.id);
    let mut s = r.record(&key);
    s["name"] = json!(c.name);
    s["kind"] = json!(c.kind);
    let now = now_ms();
    let files = !(s["redo"] == true && pass == Pass::Import);
    let mut threads: Map<String, Value> = s["threads"].as_object().cloned().unwrap_or_default();
    // Threads whose replies an earlier pass couldn't finish, first.
    let mut pending: Map<String, Value> = s["pending"].as_object().cloned().unwrap_or_default();
    for (ts, from) in pending.clone() {
        if r.budget < 2 {
            break;
        }
        if let Some(latest) = slowed(replies(r, a, team, c, &ts, from.as_str(), files).await)? {
            pending.remove(&ts);
            threads.insert(ts, json!(latest));
        }
    }
    let mut q = vec![("channel", c.id.clone()), ("limit", "200".to_owned())];
    match pass {
        Pass::New => q.push(("oldest", s["latest_ts"].as_str().unwrap_or("0").to_owned())),
        Pass::Reread => q.push(("oldest", ms_ts(now - WEEK))),
        Pass::Import => {
            if let Some(t) = s["oldest_ts"].as_str() {
                q.push(("latest", t.to_owned()));
            }
        }
    }
    let mut messages = Vec::new();
    let mut cursor = String::new();
    // Whether the pass read all it asked for (a re-read cut short by the
    // budget can't tell what was deleted).
    let complete;
    let more = loop {
        let mut q = q.clone();
        if !cursor.is_empty() {
            q.push(("cursor", cursor.clone()));
        }
        let v = match r.call(a, "conversations.history", &q, None).await {
            Ok(v) => v,
            Err(e) if matches!(slack_error(&e), Some("not_in_channel" | "channel_not_found" | "missing_scope")) => {
                s["import_done"] = json!(true);
                s["skipped"] = json!(slack_error(&e));
                r.save(&key, s);
                return Ok(());
            }
            // Slowed down before this page: the threads read are kept.
            Err(e) if e.is::<Limited>() && messages.is_empty() => {
                s["threads"] = Value::Object(threads);
                s["pending"] = Value::Object(pending);
                r.save(&key, s);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        messages.extend(v["messages"].as_array().cloned().unwrap_or_default());
        cursor = v["response_metadata"]["next_cursor"].as_str().unwrap_or("").to_owned();
        // An import takes a page a pass; the others read to the end, while
        // the budget lasts.
        if pass == Pass::Import || cursor.is_empty() || r.budget < 2 {
            complete = cursor.is_empty();
            break v["has_more"] == true;
        }
    };
    // A re-read's messages were mostly kept before, their files with them.
    let before: Vec<String> = s["recent"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let mut seen: Vec<String> = Vec::new();
    for m in &messages {
        let Some(ts) = m["ts"].as_str() else { continue };
        seen.push(ts.to_owned());
        let fetch = files && !(pass == Pass::Reread && before.iter().any(|b| b == ts));
        keep(r, a, team, c, m, fetch).await?;
        // Its thread's replies: all of a new one's, and new ones since;
        // what the budget doesn't reach waits for the next pass.
        if m["reply_count"].as_u64().unwrap_or(0) > 0 {
            let latest = m["latest_reply"].as_str().unwrap_or("").to_owned();
            let known = threads.get(ts).and_then(Value::as_str).map(str::to_owned);
            if known.as_deref() == Some(latest.as_str()) {
                continue;
            }
            let read = if r.budget > 1 { slowed(replies(r, a, team, c, ts, known.as_deref(), files).await)? } else { None };
            match read {
                Some(l) => {
                    pending.remove(ts);
                    threads.insert(ts.to_owned(), json!(l));
                }
                None => {
                    pending.entry(ts.to_owned()).or_insert(known.map_or(Value::Null, Value::from));
                }
            }
        }
    }
    let newest = messages.iter().filter_map(|m| m["ts"].as_str()).max_by(|x, y| ts_ms(x).cmp(&ts_ms(y))).map(str::to_owned);
    let oldest = messages.iter().filter_map(|m| m["ts"].as_str()).min_by(|x, y| ts_ms(x).cmp(&ts_ms(y))).map(str::to_owned);
    match pass {
        Pass::New => s["checked_at"] = json!(now),
        Pass::Reread => {
            // What was here in the last week and isn't now was deleted in
            // Slack (only when the whole week was read).
            if complete {
                let since = now - WEEK;
                let recent: Vec<String> =
                    s["recent"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
                for gone in recent.iter().filter(|t| ts_ms(t) >= since && !seen.contains(t)) {
                    r.deleted.push(json!({"external_id": format!("{}/{}/{gone}", a.team, c.id)}));
                }
                s["reread_at"] = json!(now);
            }
            s["checked_at"] = json!(now);
        }
        Pass::Import => {
            if let Some(o) = &oldest {
                s["oldest_ts"] = json!(o);
            }
            if !more {
                s["import_done"] = json!(true);
                s["redo"] = Value::Null;
            }
        }
    }
    // Where you've read to in Slack: when there's news, at the re-read,
    // and once at first.
    let news = pass == Pass::New && (!messages.is_empty() || s["last_read"].is_null());
    if (news || pass == Pass::Reread)
        && r.budget > 0
        && let Ok(v) = r.call(a, "conversations.info", &[("channel", c.id.clone())], None).await
        && let Some(lr) = v["channel"]["last_read"].as_str()
        && s["last_read"].as_str() != Some(lr)
    {
        s["last_read"] = json!(lr);
        r.conversations.push(json!({"workspace": a.team, "channel": c.id, "last_read": lr}));
    }
    // The newest message read: where keeping up starts.
    let current = s["latest_ts"].as_str().map(str::to_owned);
    s["latest_ts"] = json!(match (current, newest.clone()) {
        (Some(x), Some(y)) if ts_ms(&y) > ts_ms(&x) => y,
        (Some(x), _) => x,
        (None, y) => y.unwrap_or_else(|| "0".into()),
    });
    if let Some(n) = &newest {
        let at = ts_ms(n);
        if at > s["last_at"].as_i64().unwrap_or(0) {
            s["last_at"] = json!(at);
        }
    }
    // The last week's messages and threads, for the next re-read.
    let since = now - WEEK;
    let mut recent: Vec<String> = s["recent"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    if pass == Pass::Reread && complete {
        recent.clear();
    }
    recent.extend(seen.into_iter().filter(|t| ts_ms(t) >= since));
    recent.sort();
    recent.dedup();
    recent.retain(|t| ts_ms(t) >= since);
    threads.retain(|t, latest| ts_ms(t) >= since || ts_ms(latest.as_str().unwrap_or("0")) >= since);
    s["recent"] = json!(recent);
    s["threads"] = Value::Object(threads);
    s["pending"] = Value::Object(pending);
    r.save(&key, s);
    Ok(())
}

/// A thread's replies after `from` (all of them without), every page; its
/// latest reply, or `None` when the budget ran out first.
async fn replies(
    r: &mut Run,
    a: &Account,
    team: &mut Value,
    c: &Conversation,
    ts: &str,
    from: Option<&str>,
    files: bool,
) -> anyhow::Result<Option<String>> {
    let mut latest = from.unwrap_or(ts).to_owned();
    let mut cursor = String::new();
    loop {
        if r.budget < 1 {
            return Ok(None);
        }
        let mut q = vec![("channel", c.id.clone()), ("ts", ts.to_owned()), ("limit", "200".to_owned())];
        if let Some(f) = from {
            q.push(("oldest", f.to_owned()));
        }
        if !cursor.is_empty() {
            q.push(("cursor", cursor.clone()));
        }
        let v = match r.call(a, "conversations.replies", &q, None).await {
            Ok(v) => v,
            // Gone (deleted, or the conversation left): nothing to read.
            Err(e) if matches!(slack_error(&e), Some("thread_not_found" | "channel_not_found" | "not_in_channel")) => {
                return Ok(Some(latest));
            }
            Err(e) => return Err(e),
        };
        for x in v["messages"].as_array().into_iter().flatten().filter(|x| x["ts"].as_str() != Some(ts)) {
            if let Some(t) = x["ts"].as_str().filter(|t| ts_ms(t) > ts_ms(&latest)) {
                latest = t.to_owned();
            }
            keep(r, a, team, c, x, files).await?;
        }
        cursor = v["response_metadata"]["next_cursor"].as_str().unwrap_or("").to_owned();
        if cursor.is_empty() {
            return Ok(Some(latest));
        }
    }
}

/// A thread's replies cut short by Slack slowing down: it waits, pending.
fn slowed(read: anyhow::Result<Option<String>>) -> anyhow::Result<Option<String>> {
    match read {
        Err(e) if e.is::<Limited>() => Ok(None),
        other => other,
    }
}

/// People Slack's member list leaves out (another organization's, in a
/// shared channel or a DM), looked up one by one before their messages are
/// kept. Slack limits `users.info` on its own (more than a hundred a
/// minute), so it isn't the run's budget; a 429 stops the run, as always.
async fn learn_users(r: &mut Run, a: &Account, team: &mut Value, ids: &[String]) -> anyhow::Result<()> {
    for id in ids {
        let known = team["users"].get(id).is_some() || team["unknown"].as_array().is_some_and(|u| u.iter().any(|x| x == id.as_str()));
        if known || !id.starts_with(['U', 'W']) {
            continue;
        }
        let budget = r.budget;
        let asked = r.call(a, "users.info", &[("user", id.clone())], None).await;
        r.budget = budget;
        match asked {
            Ok(v) => {
                let mut u = entry(id, &v["user"]);
                u["x"] = json!(true);
                if !team["users"].is_object() {
                    team["users"] = json!({});
                }
                team["users"][id.as_str()] = u.clone();
                let users = team["users"].as_object().cloned().unwrap_or_default();
                r.things.push(json!({"kind": "person", "key": person_key(&a.team, id, &users), "name": u["n"]}));
                r.people.push(person(&a.team, id, &u));
            }
            Err(e) if matches!(slack_error(&e), Some("user_not_found" | "user_not_visible")) => {
                let mut unknown = team["unknown"].as_array().cloned().unwrap_or_default();
                unknown.push(json!(id));
                team["unknown"] = json!(unknown);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Keeps a message (learning who its people are first), and queues its
/// files unless they were fetched before.
async fn keep(r: &mut Run, a: &Account, team: &mut Value, c: &Conversation, m: &Value, fetch: bool) -> anyhow::Result<()> {
    let users = team["users"].as_object().cloned().unwrap_or_default();
    let mut people: Vec<String> = m["user"].as_str().map(str::to_owned).into_iter().collect();
    people.extend(plain(m["text"].as_str().unwrap_or(""), &users).1);
    learn_users(r, a, team, &people).await?;
    let Some((item, files)) = item(a, team, c, m) else { return Ok(()) };
    r.items.push(item);
    if !fetch {
        return Ok(());
    }
    let mut queue = team["files_queue"].as_array().cloned().unwrap_or_default();
    for f in files {
        let mime = f["mimetype"].as_str().unwrap_or("");
        let size = f["size"].as_u64().unwrap_or(0);
        if READABLE.contains(&mime) && size <= MAX_FILE && f["url_private_download"].is_string() {
            queue.push(json!({"id": f["id"], "name": f["name"], "mime": mime, "size": size, "url": f["url_private_download"], "created": f["created"]}));
        }
    }
    team["files_queue"] = json!(queue);
    files_waiting(r, a, team).await
}

/// Downloads the files waiting, as far as this run's share allows; they
/// become documents the extract kit reads.
async fn files_waiting(r: &mut Run, a: &Account, team: &mut Value) -> anyhow::Result<()> {
    let mut queue = team["files_queue"].as_array().cloned().unwrap_or_default();
    while let Some(f) = queue.first().cloned() {
        let size = f["size"].as_u64().unwrap_or(0);
        if r.budget < 2 || r.file_bytes + size > crate::FILE_BYTES {
            break;
        }
        let id = f["id"].as_str().unwrap_or_default().to_owned();
        match r.download(a, f["url"].as_str().unwrap_or_default()).await {
            Ok(bytes) => {
                r.file_bytes += bytes.len() as u64;
                let path = format!("files/{}/{id}", a.team);
                r.files.insert(path.clone(), bytes);
                let mut attributes = json!({"name": f["name"]});
                if let Some(t) = f["created"].as_i64() {
                    attributes["created"] = json!(t * 1000);
                }
                r.items.push(
                    json!({"type": f["mime"], "file": path, "external_id": format!("{}/file/{id}", a.team), "attributes": attributes}),
                );
            }
            Err(e) if e.downcast_ref::<Limited>().is_some() => {
                team["files_queue"] = json!(queue);
                return Err(e);
            }
            Err(e) => r.errors.push(format!("file {}: {e:#}", f["name"].as_str().unwrap_or(&id))),
        }
        queue.remove(0);
    }
    team["files_queue"] = json!(queue);
    Ok(())
}

/// A conversation as the records know it (for an operation's message).
fn known(r: &Run, a: &Account, channel: &str) -> Conversation {
    let s = r.record(&format!("chan:{}:{channel}", a.team));
    Conversation {
        id: channel.to_owned(),
        name: s["name"].as_str().unwrap_or(channel).to_owned(),
        kind: s["kind"].as_str().unwrap_or("public").to_owned(),
        updated: 0,
        info: Value::Null,
    }
}

fn input(r: &Run, k: &str) -> String {
    r.input[k].as_str().unwrap_or_default().to_owned()
}

/// `post`: a message as you, in a channel or a thread. The message comes
/// back as an item, so it shows at once.
pub async fn post(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(&input(r, "account"))?;
    let (channel, text) = (input(r, "channel"), input(r, "text"));
    anyhow::ensure!(!channel.is_empty() && !text.trim().is_empty(), "post needs a channel and some text");
    let mut body = json!({"channel": channel, "text": text});
    if let Some(t) = r.input["thread_ts"].as_str().filter(|t| !t.is_empty()) {
        body["thread_ts"] = json!(t);
        if r.input["broadcast"] == true {
            body["reply_broadcast"] = json!(true);
        }
    }
    let v = r.call(&a, "chat.postMessage", &[], Some(&body)).await?;
    let team = r.record(&format!("team:{}", a.team));
    let c = known(r, &a, &channel);
    if let Some((it, _)) = item(&a, &team, &c, &v["message"]) {
        r.items.push(it);
    }
    r.result = json!({"ok": true, "channel": channel, "ts": v["ts"]});
    Ok(())
}

/// `react`: adds (or with `remove`, takes back) a reaction.
pub async fn react(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(&input(r, "account"))?;
    let method = if r.input["remove"] == true { "reactions.remove" } else { "reactions.add" };
    let body = json!({"channel": input(r, "channel"), "timestamp": input(r, "ts"), "name": input(r, "name").trim_matches(':')});
    r.call(&a, method, &[], Some(&body)).await?;
    // As it is now, so yours show at once.
    again(r, &a).await?;
    r.result = json!({"ok": true});
    Ok(())
}

/// The message an operation names (`channel`, `ts`, a reply's
/// `thread_ts`), read again and kept.
async fn again(r: &mut Run, a: &Account) -> anyhow::Result<()> {
    let (channel, ts, thread) = (input(r, "channel"), input(r, "ts"), input(r, "thread_ts"));
    let one =
        [("channel", channel.clone()), ("oldest", ts.clone()), ("latest", ts.clone()), ("inclusive", "true".into()), ("limit", "1".into())];
    let v = if thread.is_empty() || thread == ts {
        r.call(a, "conversations.history", &one, None).await?
    } else {
        let mut q = one.to_vec();
        q.push(("ts", thread));
        r.call(a, "conversations.replies", &q, None).await?
    };
    let team = r.record(&format!("team:{}", a.team));
    let c = known(r, a, &channel);
    if let Some(m) = v["messages"].as_array().into_iter().flatten().find(|m| m["ts"].as_str() == Some(&ts))
        && let Some((it, _)) = item(a, &team, &c, m)
    {
        r.items.push(it);
    }
    Ok(())
}

/// `update`: your message's new text.
pub async fn update(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(&input(r, "account"))?;
    let (channel, ts, text) = (input(r, "channel"), input(r, "ts"), input(r, "text"));
    anyhow::ensure!(!channel.is_empty() && !ts.is_empty() && !text.trim().is_empty(), "update needs a channel, a ts and some text");
    r.call(&a, "chat.update", &[], Some(&json!({"channel": channel, "ts": ts, "text": text}))).await?;
    again(r, &a).await?;
    r.result = json!({"ok": true});
    Ok(())
}

/// `delete`: your message, gone (here too).
pub async fn delete(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(&input(r, "account"))?;
    let (channel, ts) = (input(r, "channel"), input(r, "ts"));
    anyhow::ensure!(!channel.is_empty() && !ts.is_empty(), "delete needs a channel and a ts");
    r.call(&a, "chat.delete", &[], Some(&json!({"channel": channel, "ts": ts}))).await?;
    r.deleted.push(json!({"external_id": format!("{}/{channel}/{ts}", a.team)}));
    r.result = json!({"ok": true});
    Ok(())
}

/// `mark_read`: a conversation read up to a message.
pub async fn mark_read(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(&input(r, "account"))?;
    let body = json!({"channel": input(r, "channel"), "ts": input(r, "ts")});
    r.call(&a, "conversations.mark", &[], Some(&body)).await?;
    r.result = json!({"ok": true});
    Ok(())
}

/// For tests: a map of users as the team record keeps them.
pub fn users(list: &[(&str, &str, &str)]) -> Map<String, Value> {
    list.iter().map(|(id, n, e)| (id.to_string(), json!({"n": n, "e": e}))).collect::<BTreeMap<_, _>>().into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_markup_as_text() {
        let u = users(&[("U1", "Ana Lima", "ana@acme.com"), ("U2", "Bo", "")]);
        let (t, m) = plain("hi <@U1>, see <#C9|eng> and <https://x.io|the doc> &amp; <https://y.io> <!here>", &u);
        assert_eq!(t, "hi @Ana Lima, see #eng and the doc (https://x.io) & https://y.io @here");
        assert_eq!(m, ["U1"]);
        assert_eq!(person_key("T1", "U1", &u), "person:ana@acme.com");
        assert_eq!(person_key("T1", "U2", &u), "slack-user:T1/U2");
        assert_eq!(ts_ms("1700000000.123456"), 1_700_000_000_123);
    }

    /// Keeping up with more conversations than a run can look at: the
    /// recently active ones every run, and every quiet one in turn, however
    /// long it has been quiet. (Ordering the quiet ones by their last
    /// message had the same few take every turn, so a new message in any
    /// other never arrived.)
    #[test]
    fn keeping_up_reaches_every_conversation_in_turn() {
        let mut now = 1_800_000_000_000;
        // 5 active today; 200 quiet for a month to a year, all just
        // imported.
        let mut states: Vec<Value> = (0..205)
            .map(|i| {
                let last = if i < 5 { now - 60_000 } else { now - 30 * DAY - i * DAY };
                json!({"last_at": last, "checked_at": now, "pending": {}})
            })
            .collect();
        // A run every two minutes, looking at 25 conversations.
        let mut seen = vec![0; states.len()];
        for _ in 0..30 {
            now += 2 * 60_000;
            let looked: Vec<(usize, bool)> = turn(&states, now).into_iter().take(25).collect();
            assert!((0..5).all(|i| looked.contains(&(i, true))), "the active ones every run: {looked:?}");
            for (i, _) in looked {
                states[i]["checked_at"] = json!(now);
                seen[i] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n >= 2), "every quiet one, more than once in an hour: {seen:?}");
        // Threads waiting count as active.
        states[100]["pending"] = json!({"1.0": "x"});
        assert_eq!(turn(&states, now)[..6].iter().filter(|(_, hot)| *hot).count(), 6);
    }
}
