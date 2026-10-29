//! Understanding (tier 2): a model you choose reads items and email threads
//! and says what they are about (topics), sums them up, and for mail and
//! sessions who owes what by when (commitments); for papers, their claims
//! and findings. All of it is inferred (marked so, with the model and
//! version), made only from what was observed (never from other
//! inferences), and redone when you change the model. Newest first, within
//! a daily budget of model calls.

use c6_api as api;
use c6_llm::Block;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::Memex;
use crate::llm::{self, Task};
use crate::store::clip;

const VERSION: &str = "understand-v1";
/// What a thread or item may contribute to one call.
const MAX_INPUT: usize = 14_000;

/// The default system prompt of reading tasks (`memex.understand.system`).
const SYSTEM: &str = api::prompts::UNDERSTAND_SYSTEM;

fn version(task: &Task) -> String {
    format!("{VERSION}@{}", task.model)
}

/// What to read next: (target, kind, item ids it covers).
async fn next(m: &Memex, version: &str) -> anyhow::Result<Option<(String, String, Vec<String>)>> {
    // Email: by thread, the most recent first (and again when a thread has
    // news since it was read); not lists or spam.
    let thread = sqlx::query(
        "SELECT m.account, m.thread_id, m.date AS d FROM mail m JOIN items i ON i.id = m.item_id \
         WHERE m.spam = 0 AND m.trash = 0 AND m.list = 0 AND m.draft = 0 AND NOT (lower(m.from_addr) LIKE '%noreply%' \
         OR lower(m.from_addr) LIKE '%no-reply%' OR lower(m.from_addr) LIKE '%donotreply%' OR lower(m.from_addr) LIKE '%do-not-reply%' \
         OR lower(m.from_addr) LIKE 'notification%' OR lower(m.from_addr) LIKE 'mailer-daemon%') AND NOT EXISTS (SELECT 1 FROM enrich e \
         WHERE e.target = 'thread:' || m.account || ':' || m.thread_id AND e.version = ? AND e.done_at >= i.added_at) \
         ORDER BY m.date DESC LIMIT 40",
    )
    .bind(version)
    .fetch_all(&m.store.pool)
    .await?;
    // The newest one no other step is reading.
    let thread = thread.into_iter().find(|t| {
        let key = format!("thread:{}:{}", t.get::<String, _>("account"), t.get::<String, _>("thread_id"));
        !m.claimed(&format!("read:{key}"))
    });
    let item = sqlx::query(
        "SELECT id, kind, created_at FROM items i WHERE kind IN ('session', 'document', 'paper', 'web') \
         AND NOT EXISTS (SELECT 1 FROM enrich e WHERE e.target = 'item:' || i.id AND e.version = ?) ORDER BY created_at DESC LIMIT 1",
    )
    .bind(version)
    .fetch_optional(&m.store.pool)
    .await?;
    // Whichever is newer.
    let thread_at = thread.as_ref().map(|r| r.get::<i64, _>("d")).unwrap_or(i64::MIN);
    let item_at = item.as_ref().map(|r| r.get::<i64, _>("created_at")).unwrap_or(i64::MIN);
    if let Some(t) = thread.filter(|_| thread_at >= item_at) {
        let (account, thread_id): (String, String) = (t.get("account"), t.get("thread_id"));
        let items: Vec<String> = sqlx::query_scalar("SELECT item_id FROM mail WHERE account = ? AND thread_id = ? ORDER BY date")
            .bind(&account)
            .bind(&thread_id)
            .fetch_all(&m.store.pool)
            .await?;
        return Ok(Some((format!("thread:{account}:{thread_id}"), "thread".into(), items)));
    }
    Ok(item.map(|r| (format!("item:{}", r.get::<String, _>("id")), r.get::<String, _>("kind"), vec![r.get("id")])))
}

/// Calls left today.
pub(crate) async fn budget_left(m: &Memex, per_day: u32) -> anyhow::Result<u32> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let used: i64 =
        sqlx::query_scalar("SELECT calls FROM model_calls WHERE day = ?").bind(&today).fetch_optional(&m.store.pool).await?.unwrap_or(0);
    Ok(per_day.saturating_sub(used as u32))
}

/// Whether a model call may be made now: the connection isn't resting
/// after failing for now (llm.rs), and there is budget left today.
pub(crate) async fn can_call(m: &Memex, task: &Task, per_day: u32) -> anyhow::Result<bool> {
    Ok(!llm::resting(m, &task.connection_id) && budget_left(m, per_day).await? > 0)
}

/// Gives a call that failed for now back to today's budget.
pub(crate) async fn refund_call(m: &Memex) -> anyhow::Result<()> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    sqlx::query("UPDATE model_calls SET calls = MAX(calls - 1, 0) WHERE day = ?").bind(today).execute(&m.store.pool).await?;
    Ok(())
}

pub(crate) async fn count_call(m: &Memex) -> anyhow::Result<()> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    sqlx::query("INSERT INTO model_calls (day, calls) VALUES (?, 1) ON CONFLICT(day) DO UPDATE SET calls = calls + 1")
        .bind(today)
        .execute(&m.store.pool)
        .await?;
    Ok(())
}

/// Reads one more thing, if there is a model, budget and work. Whether it
/// did.
pub async fn step(m: &Memex) -> anyhow::Result<bool> {
    let Some((task, per_day)) = m.understanding() else { return Ok(false) };
    if !can_call(m, &task, per_day).await? {
        return Ok(false);
    }
    let version = version(&task);
    let Some((target, kind, items)) = next(m, &version).await? else { return Ok(false) };
    let Some(_claim) = m.claim(&format!("read:{target}")) else { return Ok(false) };
    count_call(m).await?;
    let result = read(m, &task, &kind, &items).await;
    if let Err(e) = &result
        && llm::waits(m, e).await?
    {
        warn!(%target, "understanding waits: {e:#}");
        return Ok(false);
    }
    done(m, &target, &version, result).await?;
    Ok(true)
}

/// Records that a target was read (or why it couldn't be).
async fn done(m: &Memex, target: &str, version: &str, result: anyhow::Result<()>) -> anyhow::Result<()> {
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    if let Some(e) = &error {
        warn!(%target, "understanding: {e}");
    }
    sqlx::query("INSERT OR REPLACE INTO enrich (target, version, done_at, error) VALUES (?, ?, ?, ?)")
        .bind(target)
        .bind(version)
        .bind(c6_core::now_ms())
        .bind(&error)
        .execute(&m.store.pool)
        .await?;
    // Its people's cards have news.
    if error.is_none()
        && let Some((account, thread)) = target.strip_prefix("thread:").and_then(|t| t.rsplit_once(':'))
        && let Err(e) = crate::cards::noted(m, account, thread).await
    {
        warn!(%target, "noting for cards: {e:#}");
    }
    Ok(())
}

/// Whether a mail thread was read by this model since its latest message.
pub(crate) async fn read_already(m: &Memex, task: &Task, account: &str, thread: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM enrich e WHERE e.target = ?1 AND e.version = ?2 AND e.done_at >= \
         (SELECT COALESCE(MAX(i.added_at), 0) FROM mail m JOIN items i ON i.id = m.item_id WHERE m.account = ?3 AND m.thread_id = ?4))",
    )
    .bind(format!("thread:{account}:{thread}"))
    .bind(version(task))
    .bind(account)
    .bind(thread)
    .fetch_one(&m.store.pool)
    .await?)
}

/// Reads one mail thread now (one model call), as the walk would.
pub(crate) async fn read_thread(m: &Memex, task: &Task, account: &str, thread: &str) -> anyhow::Result<()> {
    // Being read by another step: it will be read.
    let Some(_claim) = m.claim(&format!("read:thread:{account}:{thread}")) else { return Ok(()) };
    let items: Vec<String> = sqlx::query_scalar("SELECT item_id FROM mail WHERE account = ? AND thread_id = ? ORDER BY date")
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?;
    count_call(m).await?;
    let result = read(m, task, "thread", &items).await;
    if let Err(e) = &result
        && llm::waits(m, e).await?
    {
        return result;
    }
    done(m, &format!("thread:{account}:{thread}"), &version(task), result).await
}

/// The text a model reads for these items.
async fn input(m: &Memex, kind: &str, items: &[String]) -> anyhow::Result<String> {
    let mut out = String::new();
    for id in items {
        let r = sqlx::query("SELECT title, body, created_at FROM items WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?;
        let Some(r) = r else { continue };
        let date =
            chrono::DateTime::from_timestamp_millis(r.get("created_at")).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
        let per = if kind == "thread" { MAX_INPUT / items.len().max(1) } else { MAX_INPUT };
        out.push_str(&format!("--- {} ({date})\n{}\n\n", r.get::<String, _>("title"), clip(&r.get::<String, _>("body"), per.max(1500))));
    }
    Ok(clip(&out, MAX_INPUT))
}

async fn read(m: &Memex, task: &Task, kind: &str, items: &[String]) -> anyhow::Result<()> {
    let text = input(m, kind, items).await?;
    let ask = match kind {
        "thread" => m.prompt("memex.understand.thread", api::prompts::UNDERSTAND_THREAD),
        "paper" => m.prompt("memex.understand.paper", api::prompts::UNDERSTAND_PAPER),
        "session" => m.prompt("memex.understand.session", api::prompts::UNDERSTAND_SESSION),
        _ => m.prompt("memex.understand.other", api::prompts::UNDERSTAND_OTHER),
    };
    let owner: Vec<String> = m.accounts().iter().map(|a| a.email.clone()).collect();
    let prompt = format!("{}\n\n{text}", api::prompts::fill(&ask, &[("owner", &owner.join(", "))]));
    let system = m.prompt("memex.understand.system", SYSTEM);
    let title: String = sqlx::query_scalar("SELECT title FROM items WHERE id = ?")
        .bind(items.first())
        .fetch_optional(&m.store.pool)
        .await?
        .unwrap_or_default();
    let answer = llm::ask(m, task, "understanding", &format!("{kind}: {title}"), &system, vec![Block::Text(prompt)], 3000).await?;
    let v = llm::json_of(&answer)?;
    apply(m, task, kind, items, &v).await
}

/// Records what the model said, replacing what this pipeline said before.
async fn apply(m: &Memex, task: &Task, kind: &str, items: &[String], v: &Value) -> anyhow::Result<()> {
    let made_by = version(task);
    let summary = v["summary"].as_str().unwrap_or("").trim().to_owned();
    for id in items {
        m.store.clear_links(id, &made_by).await?;
        // Earlier versions' inferences go too.
        sqlx::query("DELETE FROM links WHERE src_item = ? AND made_by LIKE 'understand-%' AND made_by != ?")
            .bind(id)
            .bind(&made_by)
            .execute(&m.store.pool)
            .await?;
        // What you have marked done or dropped stays.
        sqlx::query("DELETE FROM commitments WHERE item_id = ? AND made_by LIKE 'understand-%' AND status = 'open'")
            .bind(id)
            .execute(&m.store.pool)
            .await?;
    }
    let last = items.last().cloned().unwrap_or_default();
    if !summary.is_empty() {
        for id in items {
            sqlx::query("UPDATE items SET summary = ? WHERE id = ?").bind(&summary).bind(id).execute(&m.store.pool).await?;
        }
    }
    for t in v["topics"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).take(8) {
        let name = t.trim();
        if name.is_empty() || name.len() > 60 {
            continue;
        }
        let slug: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let thing = m.store.ensure_thing(&format!("topic:{slug}"), "topic", name).await?;
        for id in items {
            m.store.link_inferred(id, "about", &thing, 0.7, &made_by, None).await?;
        }
    }
    for c in v["claims"].as_array().into_iter().flatten() {
        let text = c["text"].as_str().unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        let key = format!("claim:{}", &c6_core::sha256_hex(text.to_lowercase().as_bytes())[..16]);
        let thing = m.store.ensure_thing(&key, "claim", text).await?;
        let rel = match c["type"].as_str() {
            Some("method") => "uses_method",
            Some("limitation") => "has_limitation",
            _ => "claims",
        };
        m.store.link_inferred(&last, rel, &thing, 0.7, &made_by, Some(text)).await?;
    }
    if matches!(kind, "thread" | "session") {
        for c in v["commitments"].as_array().into_iter().flatten() {
            let what = c["what"].as_str().unwrap_or("").trim();
            if what.is_empty() {
                continue;
            }
            let known: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM commitments WHERE item_id IN (SELECT value FROM json_each(?)) AND lower(what) = lower(?)",
            )
            .bind(serde_json::to_string(items)?)
            .bind(what)
            .fetch_optional(&m.store.pool)
            .await?;
            if known.is_some() {
                continue;
            }
            let due = c["due"]
                .as_str()
                .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .and_then(|d| d.and_hms_opt(17, 0, 0))
                .map(|d| d.and_utc().timestamp_millis());
            sqlx::query("INSERT INTO commitments (item_id, who, what, due, made_by, created_at) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(&last)
                .bind(c["who"].as_str().unwrap_or("me").trim().to_lowercase())
                .bind(what)
                .bind(due)
                .bind(&made_by)
                .bind(c6_core::now_ms())
                .execute(&m.store.pool)
                .await?;
        }
    }
    Ok(())
}

/// The most reading steps side by side (Settings → Memex: calls at once).
pub const MAX_AT_ONCE: u32 = 8;

/// Reads in the background while there is work and budget: as many lanes
/// as calls at once. With more than one, the first lane only sorts new mail
/// and drafts, so new mail never waits behind the walk.
pub fn spawn(m: std::sync::Arc<Memex>) {
    for lane in 0..MAX_AT_ONCE {
        let m = m.clone();
        tokio::spawn(async move { run(m, lane).await });
    }
}

async fn run(m: std::sync::Arc<Memex>, lane: u32) {
    let mut done = 0u64;
    let mut suggested: Option<std::time::Instant> = None;
    loop {
        let at_once = m.tasks().at_once.max(1);
        if lane >= at_once {
            m.wake.notified().await;
            continue;
        }
        if lane == 0 && suggested.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(600)) {
            if let Err(e) = crate::review::suggest(&m.store).await {
                warn!("suggesting merges: {e:#}");
            }
            suggested = Some(std::time::Instant::now());
        }
        let fast_only = at_once > 1 && lane == 0;
        // Sorting what's new for the brief comes before anything else that
        // calls the model: while conversations wait for a call it couldn't
        // make, the rest waits too (a budget raised mid-pass goes to them).
        // The rest also calls only in a pass that began with a model and
        // budget: one raised after sorting looked (with nothing waiting yet)
        // is the next pass's, so sorting, then drafts, see it first.
        let could = match m.drafting() {
            Some((_, per_day)) => budget_left(&m, per_day).await.is_ok_and(|n| n > 0),
            None => false,
        };
        let sorting = if m.link.is_connected() {
            crate::triage::step(&m).await.unwrap_or_else(|e| {
                warn!("triage: {e:#}");
                crate::triage::Sorting::Idle
            })
        } else {
            crate::triage::Sorting::Waiting
        };
        let worked = match sorting {
            crate::triage::Sorting::Worked => true,
            crate::triage::Sorting::Waiting => false,
            crate::triage::Sorting::Idle if !could => false,
            crate::triage::Sorting::Idle => {
                crate::drafts::step(&m).await.unwrap_or_else(|e| {
                    warn!("drafting: {e:#}");
                    false
                }) || (!fast_only
                    && (crate::cards::step(&m).await.unwrap_or_else(|e| {
                        warn!("cards: {e:#}");
                        false
                    }) || step(&m).await.unwrap_or_else(|e| {
                        warn!("understanding: {e:#}");
                        false
                    }) || crate::knowledge::page_step(&m).await.unwrap_or_else(|e| {
                        warn!("system pages: {e:#}");
                        false
                    })))
            }
        };
        if worked {
            done += 1;
            if done.is_multiple_of(100) {
                info!(done, lane, "understanding: read");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        } else {
            tokio::select! {
                _ = m.wake.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            }
        }
    }
}

/// How far understanding has got, for the status.
pub async fn status(m: &Memex, s: &mut api::MemexStatus) -> anyhow::Result<()> {
    let t = m.tasks();
    s.calls_per_day = t.per_day as u64;
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    s.calls_today = sqlx::query_scalar::<_, i64>("SELECT calls FROM model_calls WHERE day = ?")
        .bind(today)
        .fetch_optional(&m.store.pool)
        .await?
        .unwrap_or(0) as u64;
    if let Some(u) = &t.understanding {
        s.understood =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM enrich WHERE version = ?").bind(version(u)).fetch_one(&m.store.pool).await?
                as u64;
    }
    s.review = crate::review::open_count(&m.store).await?;
    Ok(())
}

/// Commitments read from what the caller may see (`allowed`: those of
/// items gone with everything).
pub async fn commitments(m: &Memex, status: &str, allowed: &[String]) -> anyhow::Result<Vec<api::Commitment>> {
    let everything = api::MEMEX_KINDS.iter().all(|k| allowed.iter().any(|a| a == k));
    let rows = sqlx::query(
        "SELECT c.*, i.title AS title, i.kind AS kind, (SELECT 'thread:' || ml.account || ':' || ml.thread_id FROM mail ml WHERE ml.item_id = c.item_id) AS target \
         FROM commitments c LEFT JOIN items i ON i.id = c.item_id WHERE c.status = ? \
         ORDER BY c.due IS NULL, c.due, c.created_at DESC LIMIT 200",
    )
    .bind(status)
    .fetch_all(&m.store.pool)
    .await?;
    Ok(rows
        .iter()
        .filter(|r| r.get::<Option<String>, _>("kind").map_or(everything, |k| allowed.contains(&k)))
        .map(|r| api::Commitment {
            id: r.get("id"),
            item_id: r.get("item_id"),
            item_title: r.get::<Option<String>, _>("title").unwrap_or_default(),
            who: r.get("who"),
            what: r.get("what"),
            due: r.get("due"),
            status: r.get("status"),
            target: r.get::<Option<String>, _>("target").unwrap_or_default(),
        })
        .collect())
}

pub async fn set_commitment(m: &Memex, id: i64, status: &str) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(status, "open" | "done" | "dropped"), "a commitment is open, done or dropped");
    sqlx::query("UPDATE commitments SET status = ? WHERE id = ?").bind(status).bind(id).execute(&m.store.pool).await?;
    Ok(())
}

/// What the memex knows about a thing, summed up by the model and saved as
/// a note about it (inferred, with its sources).
pub async fn summarize(m: &Memex, key: &str, allowed: &[String]) -> anyhow::Result<String> {
    let (task, _) = m.understanding().ok_or_else(|| anyhow::anyhow!("choose an understanding model first (Settings → Memex)"))?;
    let thing = m.store.thing(key).await?.ok_or_else(|| anyhow::anyhow!("no thing {key:?}"))?;
    // From what the caller may see.
    let kinds: Vec<String> = allowed.iter().filter(|k| *k != "spam").cloned().collect();
    // Observed items only: not other summaries.
    let items: Vec<api::MemexHit> =
        m.store.about(Some(&thing.id), &kinds, 40).await?.into_iter().filter(|h| !h.source.starts_with("pipeline")).collect();
    anyhow::ensure!(!items.is_empty(), "nothing is known about {} yet", thing.name);
    let mut text = String::new();
    for h in &items {
        let date = chrono::DateTime::from_timestamp_millis(h.created_at).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
        text.push_str(&format!("[{}] ({}, {date}) {}: {}\n", h.id, h.kind, h.title, clip(&h.snippet, 400)));
    }
    let template = m.prompt("memex.understand.thing", api::prompts::UNDERSTAND_THING);
    let records = clip(&text, MAX_INPUT);
    let prompt = api::prompts::fill(&template, &[("name", &thing.name), ("kind", &thing.kind), ("records", &records)]);
    let system = m.prompt("memex.understand.system", SYSTEM);
    let answer = llm::ask(m, &task, "summary", &thing.name, &system, vec![Block::Text(prompt)], 4000).await?;
    let summary = llm::json_of(&answer).ok().and_then(|v| v["summary"].as_str().map(str::to_owned)).unwrap_or(answer);
    let item = api::MemexItemIn {
        kind: "note".into(),
        source: format!("pipeline:summary@{}", task.model),
        external_id: format!("summary:{}", thing.id),
        title: format!("Summary: {}", thing.name),
        text: summary.clone(),
        labels: if thing.label.is_empty() { Vec::new() } else { vec![thing.label.clone()] },
        meta: json!({"inferred": true, "sources": items.iter().map(|h| h.id.clone()).collect::<Vec<_>>()}),
        ..Default::default()
    };
    m.store.put(&item, &item.source).await?;
    Ok(summary)
}
