//! Who each person and organization is to you (docs/relationships.md §2.3):
//! a card the model writes from the history (counted from the mail) and the
//! summaries of the threads with them, newest first. A card is grown, not
//! rewritten: an update reads the card so far and the threads since.

use c6_api as api;
use c6_llm::Block;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::warn;

use crate::Memex;
use crate::llm::{self, Task};
use crate::relations::{self, domain, shared};
use crate::store::clip;

/// How many threads a card reads at once.
const THREADS: i64 = 12;
/// How many unread threads sorting may read on the spot to write a card.
pub const READ_ON_THE_SPOT: usize = 5;

/// A card as stored.
#[derive(Clone, Debug)]
pub struct Card {
    pub key: String,
    pub name: String,
    pub card: Value,
    pub covered_to: Option<i64>,
    pub threads: i64,
}

pub async fn get(m: &Memex, key: &str) -> anyhow::Result<Option<Card>> {
    let r =
        sqlx::query("SELECT key, name, card, covered_to, threads FROM cards WHERE key = ?").bind(key).fetch_optional(&m.store.pool).await?;
    Ok(r.map(|r| Card {
        key: r.get("key"),
        name: r.get("name"),
        card: serde_json::from_str(&r.get::<String, _>("card")).unwrap_or_default(),
        covered_to: r.get("covered_to"),
        threads: r.get("threads"),
    }))
}

/// A card as a prompt reads it.
pub fn text(c: &Card) -> String {
    let f = |k: &str| c.card[k].as_str().unwrap_or("").trim().to_owned();
    let mut out = format!("{} — {}", c.name, f("relationship"));
    if !f("role").is_empty() {
        out.push_str(&format!(" ({})", f("role")));
    }
    out.push('.');
    for (label, k) in [("", "story"), ("Open: ", "open"), ("The owner: ", "how")] {
        if !f(k).is_empty() {
            out.push_str(&format!(" {label}{}", f(k)));
        }
    }
    out
}

/// Whether an address has the history a card is for: you wrote to them,
/// or more than one thread of theirs that isn't bulk mail (a list or a
/// bulk-sending address you never wrote to has none).
pub fn worth_a_card(addr: &str, c: &relations::Counts) -> bool {
    if relations::automated(addr) {
        return false;
    }
    if c.sent_to > 0 {
        return true;
    }
    let bulk = relations::sending_subdomain(addr) || c.bulk * 2 >= c.received.max(1);
    c.threads > 1 && !bulk
}

/// The card's key for an address (person) and its domain (organization;
/// none at a shared provider).
pub fn keys(addr: &str) -> (String, Option<String>) {
    let addr = addr.to_lowercase();
    let d = domain(&addr);
    (format!("person:{addr}"), (!d.is_empty() && !shared(&d)).then(|| format!("org:{d}")))
}

/// Their threads, newest first: (account, thread, latest date, subject).
async fn threads(m: &Memex, key: &str, since: Option<i64>, limit: i64) -> anyhow::Result<Vec<(String, String, i64, String)>> {
    let (who, org) = match key.split_once(':') {
        Some(("org", d)) => (d.to_owned(), true),
        Some((_, a)) => (a.to_owned(), false),
        None => return Ok(Vec::new()),
    };
    let from = if org { "lower(from_addr) LIKE '%@' || ?1" } else { "lower(from_addr) = ?1" };
    let to = if org { "'%@' || ?1 || '\"%'" } else { "'%\"' || ?1 || '\"%'" };
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT account, thread_id, MAX(date) AS at, MAX(subject) AS subject FROM mail WHERE draft = 0 AND trash = 0 AND spam = 0 \
         AND ({from} OR lower(to_json) LIKE {to} OR lower(cc_json) LIKE {to}) GROUP BY account, thread_id \
         HAVING ?2 IS NULL OR MAX(date) > ?2 ORDER BY at DESC LIMIT ?3"
    )))
    .bind(&who)
    .bind(since)
    .bind(limit)
    .fetch_all(&m.store.pool)
    .await?;
    Ok(rows.iter().map(|r| (r.get("account"), r.get("thread_id"), r.get("at"), r.get("subject"))).collect())
}

async fn summary(m: &Memex, account: &str, thread: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT i.summary FROM mail m JOIN items i ON i.id = m.item_id WHERE m.account = ? AND m.thread_id = ? AND i.summary IS NOT NULL \
         AND i.summary != '' ORDER BY m.date DESC LIMIT 1",
    )
    .bind(account)
    .bind(thread)
    .fetch_optional(&m.store.pool)
    .await?)
}

fn day(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// The model calls writing (or growing) a card would take now: unread
/// threads to read first (at most `read`), and the card itself. None when
/// there's nothing new to write it from.
pub async fn cost(m: &Memex, task: &Task, key: &str, read: usize) -> anyhow::Result<Option<usize>> {
    let since = get(m, key).await?.and_then(|c| c.covered_to);
    let ts = threads(m, key, since, THREADS).await?;
    if ts.is_empty() {
        return Ok(None);
    }
    let mut unread = 0;
    for (account, thread, _, _) in &ts {
        if !crate::understand::read_already(m, task, account, thread).await? {
            unread += 1;
        }
    }
    Ok(Some(unread.min(read) + 1))
}

/// Writes or grows a card: reads up to `read` of their unread threads first
/// (newest first; the rest are listed by subject), then asks the model.
/// `only`: these threads (the walk's news), else their newest since the card.
pub async fn write(m: &Memex, task: &Task, key: &str, name: &str, read: usize) -> anyhow::Result<Option<Card>> {
    let before = get(m, key).await?;
    let ts = threads(m, key, before.as_ref().and_then(|c| c.covered_to), THREADS).await?;
    grow(m, task, key, name, read, before, ts).await
}

async fn grow(
    m: &Memex,
    task: &Task,
    key: &str,
    name: &str,
    read: usize,
    before: Option<Card>,
    ts: Vec<(String, String, i64, String)>,
) -> anyhow::Result<Option<Card>> {
    if ts.is_empty() {
        return Ok(before);
    }
    let mut left = read;
    let mut lines = Vec::new();
    for (account, thread, at, subject) in &ts {
        if left > 0 && !crate::understand::read_already(m, task, account, thread).await? {
            left -= 1;
            if let Err(e) = crate::understand::read_thread(m, task, account, thread).await {
                warn!(%thread, "reading for a card: {e:#}");
            }
        }
        let s = summary(m, account, thread).await?.unwrap_or_else(|| "(not read yet)".into());
        lines.push(format!("- {} · {}: {}", day(*at), clip(subject, 120), clip(&s, 400)));
    }
    let (who, org) = match key.split_once(':') {
        Some(("org", d)) => (d.to_owned(), true),
        Some((_, a)) => (a.to_owned(), false),
        None => anyhow::bail!("not a card key: {key}"),
    };
    let history = if org {
        let (c, people) = relations::live(m, &who, true).await?;
        format!(
            "- {people} people at {who} wrote {} messages in {} threads; the owner wrote to {who} {} times and answered in {} of their threads.",
            c.received, c.threads, c.sent_to, c.answered
        )
    } else {
        relations::history(m, &who, name).await?
    };
    let previous = before
        .as_ref()
        .map(|c| format!("The card so far (from threads up to {}):\n{}\n\n", c.covered_to.map(day).unwrap_or_default(), c.card));
    let template = m.prompt("memex.relations.card", api::prompts::RELATION_CARD);
    let label = if org {
        format!("{who} (an organization)")
    } else if name.is_empty() {
        who.clone()
    } else {
        format!("{name} <{who}>")
    };
    let prompt = api::prompts::fill(
        &template,
        &[("who", &label), ("history", &history), ("threads", &lines.join("\n")), ("previous", previous.as_deref().unwrap_or(""))],
    );
    let system = m.prompt("memex.understand.system", api::prompts::UNDERSTAND_SYSTEM);
    crate::understand::count_call(m).await?;
    let answer = llm::ask(m, task, "card", &label, &system, vec![Block::Text(prompt)], 800).await?;
    let v = llm::json_of(&answer)?;
    let card = json!({
        "relationship": v["relationship"].as_str().unwrap_or("other").trim().to_lowercase(),
        "role": v["role"].as_str().unwrap_or("").trim(),
        "story": v["story"].as_str().unwrap_or("").trim(),
        "open": v["open"].as_str().unwrap_or("").trim(),
        "how": v["how"].as_str().unwrap_or("").trim(),
    });
    let newest = ts.iter().map(|t| t.2).max();
    let oldest = ts.iter().map(|t| t.2).min();
    let display = if org || name.is_empty() { label.clone() } else { format!("{name} <{who}>") };
    sqlx::query(
        "INSERT INTO cards (key, name, card, covered_from, covered_to, threads, model, updated_at, dirty) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0) \
         ON CONFLICT(key) DO UPDATE SET name = excluded.name, card = excluded.card, covered_from = MIN(COALESCE(covered_from, excluded.covered_from), excluded.covered_from), \
         covered_to = MAX(COALESCE(covered_to, 0), excluded.covered_to), threads = threads + excluded.threads, model = excluded.model, \
         updated_at = excluded.updated_at, dirty = 0",
    )
    .bind(key)
    .bind(&display)
    .bind(card.to_string())
    .bind(oldest)
    .bind(newest)
    .bind(ts.len() as i64)
    .bind(&task.model)
    .bind(c6_core::now_ms())
    .execute(&m.store.pool)
    .await?;
    // Taken in: the walk's news of these threads.
    for (account, thread, _, _) in &ts {
        sqlx::query("DELETE FROM card_news WHERE key = ? AND account = ? AND thread_id = ?")
            .bind(key)
            .bind(account)
            .bind(thread)
            .execute(&m.store.pool)
            .await?;
    }
    get(m, key).await
}

/// A thread was read: its people with history (you wrote to them, or more
/// than one thread), and their organizations you write to, have news for
/// their cards.
pub async fn noted(m: &Memex, account: &str, thread: &str) -> anyhow::Result<()> {
    let me: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
    let rows = sqlx::query(
        "SELECT from_addr, from_name, sent, to_json, cc_json, date FROM mail WHERE account = ? AND thread_id = ? AND draft = 0",
    )
    .bind(account)
    .bind(thread)
    .fetch_all(&m.store.pool)
    .await?;
    let Some(at) = rows.iter().map(|r| r.get::<i64, _>("date")).max() else { return Ok(()) };
    let mut people: std::collections::BTreeMap<String, String> = Default::default();
    for r in &rows {
        let from = r.get::<String, _>("from_addr").to_lowercase();
        if r.get::<i64, _>("sent") == 1 || me.contains(&from) {
            for col in ["to_json", "cc_json"] {
                let to: Vec<(String, String)> = serde_json::from_str(&r.get::<String, _>(col)).unwrap_or_default();
                for (a, n) in to {
                    people.entry(a.to_lowercase()).or_insert(n);
                }
            }
        } else {
            people.insert(from, r.get("from_name"));
        }
    }
    for (addr, name) in people {
        if me.contains(&addr) || relations::automated(&addr) || !addr.contains('@') {
            continue;
        }
        let (person, org) = keys(&addr);
        // Counted now (the index may be a minute behind).
        let (c, _) = relations::live(m, &addr, false).await?;
        let mut news = Vec::new();
        if worth_a_card(&addr, &c) {
            news.push((person, name.clone()));
        }
        if let Some(org) = org
            && relations::live(m, &domain(&addr), true).await?.0.sent_to > 0
        {
            news.push((org, String::new()));
        }
        for (key, name) in news {
            sqlx::query("INSERT INTO card_news (key, name, account, thread_id, at) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING")
                .bind(&key)
                .bind(&name)
                .bind(account)
                .bind(thread)
                .bind(at)
                .execute(&m.store.pool)
                .await?;
        }
    }
    Ok(())
}

/// Removes the cards of people who aren't worth one (written before the
/// rule left out bulk senders), and their news. Once. How many went.
pub async fn prune(m: &Memex) -> anyhow::Result<usize> {
    let done: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memex_once WHERE name = 'prune-bulk-cards')").fetch_one(&m.store.pool).await?;
    if done {
        return Ok(0);
    }
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT key FROM cards WHERE key LIKE 'person:%' UNION SELECT key FROM card_news WHERE key LIKE 'person:%'")
            .fetch_all(&m.store.pool)
            .await?;
    let mut gone = 0;
    for key in keys {
        let addr = key.trim_start_matches("person:");
        let (c, _) = relations::live(m, addr, false).await?;
        if !worth_a_card(addr, &c) {
            sqlx::query("DELETE FROM cards WHERE key = ?").bind(&key).execute(&m.store.pool).await?;
            sqlx::query("DELETE FROM card_news WHERE key = ?").bind(&key).execute(&m.store.pool).await?;
            gone += 1;
        }
    }
    sqlx::query("INSERT OR IGNORE INTO memex_once (name, at) VALUES ('prune-bulk-cards', ?)")
        .bind(c6_core::now_ms())
        .execute(&m.store.pool)
        .await?;
    if gone > 0 {
        tracing::info!(gone, "cards of bulk senders removed");
    }
    Ok(gone)
}

/// Folds the walk's news into one card: the person or organization most
/// recently active. Whether it did.
pub async fn step(m: &Memex) -> anyhow::Result<bool> {
    if let Err(e) = prune(m).await {
        warn!("removing bulk senders' cards: {e:#}");
    }
    let Some((task, per_day)) = m.understanding() else { return Ok(false) };
    if !crate::understand::can_call(m, &task, per_day).await? {
        return Ok(false);
    }
    let next: Vec<(String, String)> = sqlx::query_as("SELECT key, MAX(name) FROM card_news GROUP BY key ORDER BY MAX(at) DESC LIMIT ?")
        .bind(crate::understand::window(m.tasks().at_once, 10))
        .fetch_all(&m.store.pool)
        .await?;
    // The first no other step is writing.
    let Some((key, name, _claim)) = next.into_iter().find_map(|(k, n)| m.claim(&format!("card:{k}")).map(|c| (k, n, c))) else {
        return Ok(false);
    };
    let rows = sqlx::query(
        "SELECT n.account, n.thread_id, n.at, (SELECT MAX(subject) FROM mail m WHERE m.account = n.account AND m.thread_id = n.thread_id) AS subject \
         FROM card_news n WHERE n.key = ? ORDER BY n.at DESC LIMIT ?",
    )
    .bind(&key)
    .bind(THREADS)
    .fetch_all(&m.store.pool)
    .await?;
    let ts: Vec<(String, String, i64, String)> = rows
        .iter()
        .map(|r| (r.get("account"), r.get("thread_id"), r.get("at"), r.get::<Option<String>, _>("subject").unwrap_or_default()))
        .collect();
    let before = get(m, &key).await?;
    if let Err(e) = grow(m, &task, &key, &name, 0, before, ts.clone()).await {
        if llm::waits(m, &e).await? {
            warn!(%key, "growing a card waits: {e:#}");
            return Ok(false);
        }
        warn!(%key, "growing a card: {e:#}");
        // Not again for these threads: the next news tries again.
        for (account, thread, _, _) in &ts {
            sqlx::query("DELETE FROM card_news WHERE key = ? AND account = ? AND thread_id = ?")
                .bind(&key)
                .bind(account)
                .bind(thread)
                .execute(&m.store.pool)
                .await?;
        }
    }
    Ok(true)
}

/// How far the walk and the cards are (for the Activity page).
pub async fn progress(m: &Memex) -> anyhow::Result<Value> {
    let r = sqlx::query(
        "SELECT (SELECT COUNT(*) FROM cards) AS cards, (SELECT COUNT(DISTINCT key) FROM card_news) AS waiting, \
         (SELECT COUNT(*) FROM enrich WHERE target LIKE 'thread:%' AND error IS NULL) AS read, \
         (SELECT COUNT(*) FROM correspondents) AS correspondents",
    )
    .fetch_one(&m.store.pool)
    .await?;
    let (threads, oldest): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(DISTINCT m.account || ':' || m.thread_id), MIN(CASE WHEN e.target IS NOT NULL THEN m.date END) FROM mail m \
         LEFT JOIN enrich e ON e.target = 'thread:' || m.account || ':' || m.thread_id AND e.error IS NULL \
         WHERE m.spam = 0 AND m.trash = 0 AND m.list = 0 AND m.draft = 0",
    )
    .fetch_one(&m.store.pool)
    .await?;
    Ok(json!({"cards": r.get::<i64, _>("cards"), "cards_waiting": r.get::<i64, _>("waiting"), "threads_read": r.get::<i64, _>("read"),
              "threads": threads, "oldest_read": oldest, "correspondents": r.get::<i64, _>("correspondents")}))
}

/// The cards a sorting of mail from `addr` reads: the person's, and their
/// organization's. Written on the spot when missing and there's history
/// (and budget for it); as they are otherwise.
pub async fn for_sorting(m: &Memex, task: &Task, per_day: u32, addr: &str, name: &str) -> anyhow::Result<Vec<Card>> {
    let (person, org) = keys(addr);
    let (c, _) = relations::live(m, addr, false).await?;
    let mut out = Vec::new();
    // Someone with history: you wrote to them, or more than one thread.
    let history = worth_a_card(addr, &c);
    for (key, wanted) in [(person, history && !relations::automated(addr)), (org.clone().unwrap_or_default(), org.is_some())] {
        if key.is_empty() {
            continue;
        }
        let existing = get(m, &key).await?;
        let wanted = wanted
            && existing.is_none()
            && (key.starts_with("person:") || {
                let (dc, _) = relations::live(m, &domain(addr), true).await?;
                dc.sent_to > 0
            });
        // Another step writing it: sort with what there is.
        let claim = if wanted { m.claim(&format!("card:{key}")) } else { None };
        if let Some(_claim) = claim
            && let Some(calls) = cost(m, task, &key, READ_ON_THE_SPOT).await?
            // Room for the card and the sorting itself.
            && crate::understand::budget_left(m, per_day).await? as usize > calls
        {
            match write(m, task, &key, if key.starts_with("person:") { name } else { "" }, READ_ON_THE_SPOT).await {
                Ok(Some(card)) => out.push(card),
                Ok(None) => {}
                Err(e) => warn!(%key, "writing a card: {e:#}"),
            }
            continue;
        }
        out.extend(existing);
    }
    Ok(out)
}

/// The cards an address has, as written (for drafts: none are written here).
pub async fn of(m: &Memex, addr: &str) -> anyhow::Result<Vec<Card>> {
    let (person, org) = keys(addr);
    let mut out = Vec::new();
    for key in std::iter::once(person).chain(org) {
        out.extend(get(m, &key).await?);
    }
    Ok(out)
}

/// The cards of a thing in the graph (its keys: person:<address>, org:<domain>).
pub async fn of_thing(m: &Memex, id: &str) -> anyhow::Result<Vec<Card>> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM things WHERE (id = ?1 OR key = ?1) AND key IS NOT NULL UNION SELECT key FROM thing_keys WHERE thing_id = ?1 \
         OR thing_id = (SELECT id FROM things WHERE key = ?1)",
    )
    .bind(id)
    .fetch_all(&m.store.pool)
    .await?;
    let mut out = Vec::new();
    for k in keys.iter().filter(|k| k.starts_with("person:") || k.starts_with("org:")) {
        out.extend(get(m, k).await?);
    }
    Ok(out)
}

pub fn json_of(c: &Card) -> Value {
    json!({"key": c.key, "name": c.name, "card": c.card, "covered_to": c.covered_to, "threads": c.threads})
}

pub async fn method(m: &Memex, method: &str, params: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| params[k].as_str().unwrap_or_default().to_owned();
    Ok(match method {
        // A thing's cards (a person, an organization), or an address's.
        "memex.relations.cards" => {
            let cards = if s("address").is_empty() { of_thing(m, &s("id")).await? } else { of(m, &s("address")).await? };
            json!(cards.iter().map(json_of).collect::<Vec<_>>())
        }
        // Writes (or grows) a card now.
        "memex.relations.write" => {
            let (task, _) = m.understanding().ok_or_else(|| anyhow::anyhow!("choose an understanding model first (Settings → Memex)"))?;
            let key = s("key");
            anyhow::ensure!(key.starts_with("person:") || key.starts_with("org:"), "a card is person:<address> or org:<domain>");
            json!(write(m, &task, &key, &s("name"), READ_ON_THE_SPOT).await?.as_ref().map(json_of))
        }
        "memex.relations.progress" => progress(m).await?,
        other => anyhow::bail!("the memex has no method {other}"),
    })
}
