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
pub async fn write(m: &Memex, task: &Task, key: &str, name: &str, read: usize) -> anyhow::Result<Option<Card>> {
    let before = get(m, key).await?;
    let ts = threads(m, key, before.as_ref().and_then(|c| c.covered_to), THREADS).await?;
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
    get(m, key).await
}

/// The cards a sorting of mail from `addr` reads: the person's, and their
/// organization's. Written on the spot when missing and there's history
/// (and budget for it); as they are otherwise.
pub async fn for_sorting(m: &Memex, task: &Task, per_day: u32, addr: &str, name: &str) -> anyhow::Result<Vec<Card>> {
    let (person, org) = keys(addr);
    let (c, _) = relations::live(m, addr, false).await?;
    let mut out = Vec::new();
    // Someone with history: you wrote to them, or more than one thread.
    let history = c.sent_to > 0 || c.threads > 1;
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
        if wanted
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
        other => anyhow::bail!("the memex has no method {other}"),
    })
}
