//! Understanding (tier 2): a model you choose reads items and email threads
//! and says what they are about (topics), sums them up, and for mail and
//! sessions who owes what by when (commitments); for papers, their claims
//! and findings. All of it is inferred (marked so, with the model and
//! version), made only from what was observed (never from other
//! inferences), and redone when you change the model. Newest first, within
//! a daily budget of model calls.

use c6_api as api;
use c6_llm::Block;
use futures_util::TryStreamExt;
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

const READABLE: &str = "m.spam = 0 AND m.trash = 0 AND m.list = 0 AND m.draft = 0 AND NOT (lower(m.from_addr) LIKE '%noreply%' \
    OR lower(m.from_addr) LIKE '%no-reply%' OR lower(m.from_addr) LIKE '%donotreply%' OR lower(m.from_addr) LIKE '%do-not-reply%' \
    OR lower(m.from_addr) LIKE 'notification%' OR lower(m.from_addr) LIKE 'mailer-daemon%')";

struct Reading {
    target: String,
    kind: String,
}

async fn thread_items(m: &Memex, account: &str, thread: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT item_id FROM mail WHERE account = ? AND thread_id = ? ORDER BY date")
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?)
}

/// What to read next, unread work before failed reads.
async fn next(m: &Memex, version: &str) -> anyhow::Result<Option<Reading>> {
    let at_once = m.tasks().at_once;
    // Email: by thread, the most recent first (and again when a thread has
    // news since it was read); not lists or spam.
    let sql = format!(
        "SELECT m.account, m.thread_id, m.date AS d FROM mail m JOIN items i ON i.id = m.item_id \
         WHERE {READABLE} AND NOT EXISTS (SELECT 1 FROM enrich e \
         WHERE e.target = 'thread:' || m.account || ':' || m.thread_id AND e.version = ? AND e.done_at >= i.added_at) \
         ORDER BY m.date DESC"
    );
    let mut rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str())).bind(version).fetch(&m.store.pool);
    // Stop the date walk after enough distinct threads, not messages.
    let mut seen = std::collections::HashSet::new();
    let mut threads = Vec::new();
    while let Some(row) = rows.try_next().await? {
        if seen.insert((row.get::<String, _>("account"), row.get::<String, _>("thread_id"))) {
            threads.push(row);
            if threads.len() >= window(at_once, 40) as usize {
                break;
            }
        }
    }
    drop(rows);
    // The newest one no other step is reading.
    let thread = threads.into_iter().find(|t| {
        let key = format!("thread:{}:{}", t.get::<String, _>("account"), t.get::<String, _>("thread_id"));
        !m.claimed(&format!("read:{key}"))
    });
    let item = sqlx::query(
        "SELECT id, kind, created_at FROM items i WHERE kind IN ('session', 'document', 'paper', 'web') \
         AND NOT EXISTS (SELECT 1 FROM enrich e WHERE e.target = 'item:' || i.id AND e.version = ?) ORDER BY created_at DESC LIMIT ?",
    )
    .bind(version)
    .bind(window(at_once, 10))
    .fetch_all(&m.store.pool)
    .await?;
    let item = item.into_iter().find(|i| !m.claimed(&format!("read:item:{}", i.get::<String, _>("id"))));
    // Whichever is newer.
    let thread_at = thread.as_ref().map(|r| r.get::<i64, _>("d")).unwrap_or(i64::MIN);
    let item_at = item.as_ref().map(|r| r.get::<i64, _>("created_at")).unwrap_or(i64::MIN);
    if let Some(t) = thread.filter(|_| thread_at >= item_at) {
        let (account, thread_id): (String, String) = (t.get("account"), t.get("thread_id"));
        return Ok(Some(Reading { target: format!("thread:{account}:{thread_id}"), kind: "thread".into() }));
    }
    if let Some(r) = item {
        return Ok(Some(Reading { target: format!("item:{}", r.get::<String, _>("id")), kind: r.get("kind") }));
    }
    retry(m, version).await
}

/// The newest failed target that still exists and is readable.
async fn retry(m: &Memex, version: &str) -> anyhow::Result<Option<Reading>> {
    let targets: Vec<String> = sqlx::query_scalar("SELECT target FROM enrich WHERE version = ? AND error IS NOT NULL AND tries < 3")
        .bind(version)
        .fetch_all(&m.store.pool)
        .await?;
    let sql =
        format!("SELECT MAX(m.date) FROM mail m JOIN items i ON i.id = m.item_id WHERE m.account = ? AND m.thread_id = ? AND {READABLE}");
    let mut newest: Option<(String, String, i64)> = None;
    for target in targets {
        if m.claimed(&format!("read:{target}")) {
            continue;
        }
        let found: Option<(String, i64)> = if let Some(id) = target.strip_prefix("item:") {
            sqlx::query_as("SELECT kind, created_at FROM items WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?
        } else if let Some((account, thread)) = target.strip_prefix("thread:").and_then(|t| t.rsplit_once(':')) {
            let at: Option<i64> =
                sqlx::query_scalar(sqlx::AssertSqlSafe(sql.as_str())).bind(account).bind(thread).fetch_one(&m.store.pool).await?;
            at.map(|at| ("thread".into(), at))
        } else {
            None
        };
        if let Some((kind, at)) = found
            && newest.as_ref().is_none_or(|n| at > n.2)
        {
            newest = Some((target, kind, at));
        }
    }
    let Some((target, kind, _)) = newest else { return Ok(None) };
    Ok(Some(Reading { target, kind }))
}

impl Reading {
    /// The current items, once this target is claimed.
    async fn items(&self, m: &Memex) -> anyhow::Result<Vec<String>> {
        if let Some(id) = self.target.strip_prefix("item:") {
            Ok(vec![id.to_owned()])
        } else {
            let (account, thread) = self.target.strip_prefix("thread:").unwrap().rsplit_once(':').unwrap();
            thread_items(m, account, thread).await
        }
    }
}

/// After claiming: a fresh read, a retry, or nothing if another lane finished it.
async fn retrying(m: &Memex, target: &str, version: &str, items: &[String]) -> anyhow::Result<Option<bool>> {
    let previous: Option<(Option<String>, i64)> = sqlx::query_as(
        "SELECT error, tries FROM enrich WHERE target = ? AND version = ? AND done_at >= \
         (SELECT COALESCE(MAX(added_at), 0) FROM items WHERE id IN (SELECT value FROM json_each(?)))",
    )
    .bind(target)
    .bind(version)
    .bind(serde_json::to_string(items)?)
    .fetch_optional(&m.store.pool)
    .await?;
    Ok(match previous {
        None => Some(false),
        Some((Some(_), tries)) if tries < 3 => Some(true),
        _ => None,
    })
}

/// How long the budget's day lasts. A day starts with its first call and
/// lasts 24 hours (not to a midnight: a run, or a test, never sees the
/// count reset under it). Its row in `model_calls` is `p:<start ms>`.
const BUDGET_DAY: i64 = 24 * 3600 * 1000;

/// The current day's row, if one began less than 24 hours ago.
async fn budget_day(m: &Memex) -> anyhow::Result<Option<String>> {
    let latest: Option<String> =
        sqlx::query_scalar("SELECT day FROM model_calls WHERE day LIKE 'p:%' ORDER BY CAST(substr(day, 3) AS INTEGER) DESC LIMIT 1")
            .fetch_optional(&m.store.pool)
            .await?;
    let now = c6_core::now_ms();
    Ok(latest.filter(|k| k[2..].parse::<i64>().is_ok_and(|start| now - start < BUDGET_DAY)))
}

/// Calls made in the current day.
async fn calls_used(m: &Memex) -> anyhow::Result<i64> {
    Ok(match budget_day(m).await? {
        Some(day) => sqlx::query_scalar("SELECT calls FROM model_calls WHERE day = ?").bind(day).fetch_one(&m.store.pool).await?,
        None => 0,
    })
}

/// Calls left in the current day (24 hours from its first call).
pub(crate) async fn budget_left(m: &Memex, per_day: u32) -> anyhow::Result<u32> {
    Ok(per_day.saturating_sub(calls_used(m).await? as u32))
}

/// Whether a model call may be made now: the connection isn't resting
/// after failing for now (llm.rs), and there is budget left today.
pub(crate) async fn can_call(m: &Memex, task: &Task, per_day: u32) -> anyhow::Result<bool> {
    Ok(!llm::resting(m, &task.connection_id) && budget_left(m, per_day).await? > 0)
}

/// Gives a call that failed for now back to the day's budget.
pub(crate) async fn refund_call(m: &Memex) -> anyhow::Result<()> {
    if let Some(day) = budget_day(m).await? {
        sqlx::query("UPDATE model_calls SET calls = MAX(calls - 1, 0) WHERE day = ?").bind(day).execute(&m.store.pool).await?;
    }
    Ok(())
}

/// Counts a call in the current day, or starts a day with it.
pub(crate) async fn count_call(m: &Memex) -> anyhow::Result<()> {
    let now = c6_core::now_ms();
    // One statement each: two calls starting a day at once make one day.
    let started = sqlx::query(
        "INSERT INTO model_calls (day, calls) SELECT ?, 1 WHERE NOT EXISTS \
         (SELECT 1 FROM model_calls WHERE day LIKE 'p:%' AND CAST(substr(day, 3) AS INTEGER) > ?)",
    )
    .bind(format!("p:{now}"))
    .bind(now - BUDGET_DAY)
    .execute(&m.store.pool)
    .await?
    .rows_affected();
    if started == 0
        && let Some(day) = budget_day(m).await?
    {
        sqlx::query("UPDATE model_calls SET calls = calls + 1 WHERE day = ?").bind(day).execute(&m.store.pool).await?;
    }
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
    let Some(reading) = next(m, &version).await? else { return Ok(false) };
    let Some(_claim) = m.claim(&format!("read:{}", reading.target)) else { return Ok(false) };
    let items = reading.items(m).await?;
    let Reading { target, kind } = reading;
    let Some(retry) = retrying(m, &target, &version, &items).await? else { return Ok(false) };
    count_call(m).await?;
    let result = read(m, &task, &kind, &items).await;
    if let Err(e) = &result
        && llm::waits(m, e).await?
    {
        warn!(%target, "understanding waits: {e:#}");
        return Ok(false);
    }
    done(m, &target, &version, &items, retry, result).await?;
    Ok(true)
}

/// Records the newest content read (or why it couldn't be read).
async fn done(m: &Memex, target: &str, version: &str, items: &[String], retry: bool, result: anyhow::Result<()>) -> anyhow::Result<()> {
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    if let Some(e) = &error {
        warn!(%target, "understanding: {e}");
    }
    sqlx::query(
        "INSERT INTO enrich (target, version, done_at, error, tries) VALUES (?, ?, \
         (SELECT COALESCE(MAX(added_at), 0) FROM items WHERE id IN (SELECT value FROM json_each(?))), ?, ?) \
         ON CONFLICT(target) DO UPDATE SET version = excluded.version, done_at = excluded.done_at, error = excluded.error, \
         tries = CASE WHEN excluded.error IS NULL THEN 0 WHEN ? AND enrich.version = excluded.version THEN enrich.tries + 1 ELSE 1 END",
    )
    .bind(target)
    .bind(version)
    .bind(serde_json::to_string(items)?)
    .bind(&error)
    .bind(i64::from(error.is_some()))
    .bind(retry)
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
    let items = thread_items(m, account, thread).await?;
    let target = format!("thread:{account}:{thread}");
    let version = version(task);
    let Some(retry) = retrying(m, &target, &version, &items).await? else { return Ok(()) };
    count_call(m).await?;
    let result = read(m, task, "thread", &items).await;
    if let Err(e) = &result
        && llm::waits(m, e).await?
    {
        return result;
    }
    done(m, &target, &version, &items, retry, result).await
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
    let (last, today) = crate::commitments::dates(m, items, c6_core::now_ms()).await?;
    let prompt = format!("{}\n\n{text}", api::prompts::fill(&ask, &[("owner", &owner.join(", ")), ("last", &last), ("today", &today)]));
    let system = m.prompt("memex.understand.system", SYSTEM);
    let title: String = sqlx::query_scalar("SELECT title FROM items WHERE id = ?")
        .bind(items.first())
        .fetch_optional(&m.store.pool)
        .await?
        .unwrap_or_default();
    let answer =
        llm::ask(m, task, "understanding", &format!("{kind}: {title}"), &system, vec![Block::Text(prompt)], llm::Output::Json(3000))
            .await?;
    let v = llm::json_of(&answer)?;
    apply(m, task, kind, items, &v).await
}

/// Records what the model said, replacing what this pipeline said before.
pub(crate) async fn apply(m: &Memex, task: &Task, kind: &str, items: &[String], v: &Value) -> anyhow::Result<()> {
    let summary = v["summary"].as_str().unwrap_or("").trim().to_owned();
    anyhow::ensure!(!summary.is_empty(), "the model's answer has no summary");
    let made_by = version(task);
    for id in items {
        m.store.clear_links(id, &made_by).await?;
        // Earlier versions' inferences go too.
        sqlx::query("DELETE FROM links WHERE src_item = ? AND made_by LIKE 'understand-%' AND made_by != ?")
            .bind(id)
            .bind(&made_by)
            .execute(&m.store.pool)
            .await?;
    }
    let last = items.last().cloned().unwrap_or_default();
    for id in items {
        sqlx::query("UPDATE items SET summary = ? WHERE id = ?").bind(&summary).bind(id).execute(&m.store.pool).await?;
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
    let commitments = if matches!(kind, "thread" | "session") { &v["commitments"] } else { &json!([]) };
    crate::commitments::save(m, items, commitments, &made_by, c6_core::now_ms()).await?;
    Ok(())
}

/// The most reading steps side by side (Settings → Memex: calls at once).
pub const MAX_AT_ONCE: u32 = 64;

/// Candidates for every lane, with room to pass over claimed work.
pub(crate) fn window(at_once: u32, floor: u32) -> i64 {
    i64::from(floor.max(at_once.clamp(1, MAX_AT_ONCE) + 8))
}

/// Reads in the background while there is work and budget: as many lanes
/// as calls at once. With more than one, the first lane only sorts new mail
/// and drafts, so new mail never waits behind the walk.
pub fn spawn(m: std::sync::Arc<Memex>) {
    tokio::spawn(lanes(m, run));
}

async fn lanes<F>(m: std::sync::Arc<Memex>, start: impl Fn(std::sync::Arc<Memex>, u32) -> F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut running = tokio::task::JoinSet::new();
    let mut ids = [None; MAX_AT_ONCE as usize];
    let mut resting = [false; MAX_AT_ONCE as usize];
    loop {
        let wake = m.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        for lane in 0..m.tasks().at_once.clamp(1, MAX_AT_ONCE) {
            if ids[lane as usize].is_none() && !resting[lane as usize] {
                ids[lane as usize] = Some(running.spawn(start(m.clone(), lane)).id());
            }
        }
        tokio::select! {
            _ = wake => resting.fill(false),
            Some(result) = running.join_next_with_id() => {
                let failed = result.is_err();
                let id = match result {
                    Ok((id, ())) => id,
                    Err(e) => {
                        warn!("reading lane: {e}");
                        e.id()
                    }
                };
                // Only a joined task frees its index, even if the setting rose again.
                let lane = ids.iter().position(|slot| *slot == Some(id)).unwrap();
                ids[lane] = None;
                resting[lane] = failed;
            }
        }
    }
}

async fn run(m: std::sync::Arc<Memex>, lane: u32) {
    let mut done = 0u64;
    let mut suggested: Option<std::time::Instant> = None;
    loop {
        let wake = m.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let at_once = m.tasks().at_once.clamp(1, MAX_AT_ONCE);
        if lane >= at_once {
            return;
        }
        if lane == 0 && suggested.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(600)) {
            if let Err(e) = crate::commitments::maintain(&m, c6_core::now_ms()).await {
                warn!("maintaining commitments: {e:#}");
            }
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
        if let Err(e) = crate::drafts::sweep_answered(&m).await {
            warn!("calendar requests: {e:#}");
        }
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
                _ = wake => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            }
        }
    }
}

/// How far understanding has got, for the status.
pub async fn status(m: &Memex, s: &mut api::MemexStatus) -> anyhow::Result<()> {
    let t = m.tasks();
    s.calls_per_day = t.per_day as u64;
    s.calls_today = calls_used(m).await? as u64;
    if let Some(u) = &t.understanding {
        s.understood = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM enrich WHERE version = ? AND error IS NULL")
            .bind(version(u))
            .fetch_one(&m.store.pool)
            .await? as u64;
        s.understanding_failed =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM enrich WHERE version = ? AND error IS NOT NULL AND tries >= 3")
                .bind(version(u))
                .fetch_one(&m.store.pool)
                .await? as u64;
    }
    s.review = crate::review::open_count(&m.store).await?;
    Ok(())
}

/// Commitments read from what the caller may see (`allowed`: those of
/// items gone with everything).
pub async fn commitments(m: &Memex, status: &str, allowed: &[String]) -> anyhow::Result<Vec<api::Commitment>> {
    crate::commitments::status(status)?;
    let everything = api::MEMEX_KINDS.iter().all(|k| allowed.iter().any(|a| a == k));
    let rows = sqlx::query(
        "SELECT c.*, i.title AS title, i.kind AS kind, (SELECT 'thread:' || ml.account || ':' || ml.thread_id FROM mail ml WHERE ml.item_id = c.item_id) AS target \
         FROM commitments c LEFT JOIN items i ON i.id = c.item_id WHERE c.status = ? \
         ORDER BY CASE WHEN c.status = 'open' THEN c.due IS NULL END, CASE WHEN c.status = 'open' THEN c.due END, c.said_at DESC, c.id DESC LIMIT 200",
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
            said_at: r.get("said_at"),
            status: r.get("status"),
            target: r.get::<Option<String>, _>("target").unwrap_or_default(),
        })
        .collect())
}

pub async fn set_commitment(m: &Memex, id: i64, status: &str) -> anyhow::Result<()> {
    crate::commitments::set(m, id, status, c6_core::now_ms()).await
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
    let answer = llm::ask(m, &task, "summary", &thing.name, &system, vec![Block::Text(prompt)], llm::Output::Json(4000)).await?;
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

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[tokio::test]
    async fn a_budget_day_is_24_hours_from_its_first_call() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        // A day from before (a date, as days were counted) and one that
        // began 25 hours ago don't count.
        let old = format!("p:{}", c6_core::now_ms() - 25 * 3600 * 1000);
        for (day, calls) in [("2026-10-29", 40), (old.as_str(), 30)] {
            sqlx::query("INSERT INTO model_calls (day, calls) VALUES (?, ?)").bind(day).bind(calls).execute(&m.store.pool).await.unwrap();
        }
        assert_eq!(budget_left(&m, 5).await.unwrap(), 5);
        // The first call starts a day; the next ones count in it.
        for _ in 0..3 {
            count_call(&m).await.unwrap();
        }
        assert_eq!(budget_left(&m, 5).await.unwrap(), 2);
        refund_call(&m).await.unwrap();
        assert_eq!(budget_left(&m, 5).await.unwrap(), 3);
        let days: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_calls WHERE day LIKE 'p:%'").fetch_one(&m.store.pool).await.unwrap();
        assert_eq!(days, 2, "one new day, beside the old one");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c6_testkit::within;

    async fn read_mail_thread(m: &Memex, task: &Task) -> Vec<String> {
        let mut items = Vec::new();
        for n in 1..=2 {
            let id = m
                .store
                .put(
                    &api::MemexItemIn {
                        kind: "email".into(),
                        source: "test".into(),
                        external_id: n.to_string(),
                        title: "Workshop".into(),
                        text: format!("Message {n}."),
                        ..Default::default()
                    },
                    "test",
                )
                .await
                .unwrap();
            sqlx::query("INSERT INTO mail (item_id, account, gmail_id, thread_id, date, from_addr, blob) VALUES (?, 'a', ?, 't', ?, 'pat@example.com', '')")
                .bind(&id).bind(n.to_string()).bind(n).execute(&m.store.pool).await.unwrap();
            items.push(id);
        }
        apply(
            m,
            task,
            "thread",
            &items,
            &json!({"summary": "Plans for the workshop.", "topics": ["Workshop", "Planning"], "commitments": [{"who": "me", "what": "Send the agenda"}]}),
        )
        .await
        .unwrap();
        done(m, "thread:a:t", &version(task), &items, false, Ok(())).await.unwrap();
        let topic = m.store.ensure_thing("topic:earlier", "topic", "Earlier").await.unwrap();
        for id in &items {
            m.store.link_inferred(id, "about", &topic, 0.7, "understand-v0@test", None).await.unwrap();
        }
        items
    }

    #[tokio::test]
    async fn answers_without_summaries_keep_previous_reads_and_retry() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let task = Task { connection_id: "test".into(), model: "test".into(), effort: None };
        let version = version(&task);
        let items = read_mail_thread(&m, &task).await;
        let topics = async || -> Vec<(i64, String, String)> {
            sqlx::query_as("SELECT id, dst_thing, made_by FROM links WHERE rel = 'about' ORDER BY id")
                .fetch_all(&m.store.pool)
                .await
                .unwrap()
        };
        let commitments = async || -> Vec<(i64, String, String, String)> {
            sqlx::query_as("SELECT id, who, what, status FROM commitments ORDER BY id").fetch_all(&m.store.pool).await.unwrap()
        };
        let previous_topics = topics().await;
        let previous_commitments = commitments().await;
        assert_eq!(previous_topics.len(), 6);
        assert_eq!(previous_commitments.len(), 1);
        assert_eq!(previous_commitments[0].3, "open");

        for answer in [
            "{}",
            r#"{"summary": ""}"#,
            r#"{"summary": "   "}"#,
            "[]",
            r#"{"topics": ["x"], "commitments": [{"who": "me", "what": "Something else"}]}"#,
            r#"{"summary": null}"#,
            r#"{"summary": 7}"#,
        ] {
            // Each answer gets a fresh retry count.
            sqlx::query("DELETE FROM enrich").execute(&m.store.pool).await.unwrap();
            // read() rejects [], but apply() must also reject it.
            let v = serde_json::from_str(answer).unwrap();
            assert_eq!(
                apply(&m, &task, "thread", &items, &v).await.unwrap_err().to_string(),
                "the model's answer has no summary",
                "{answer}"
            );
            for (is_retry, tries) in [(false, 1), (true, 2), (true, 3)] {
                assert_eq!(retrying(&m, "thread:a:t", &version, &items).await.unwrap(), Some(is_retry));
                let result = async {
                    let v = llm::json_of(answer)?;
                    apply(&m, &task, "thread", &items, &v).await
                }
                .await;
                let error = result.as_ref().unwrap_err().to_string();
                done(&m, "thread:a:t", &version, &items, is_retry, result).await.unwrap();
                let recorded: (Option<String>, i64) =
                    sqlx::query_as("SELECT error, tries FROM enrich WHERE target = 'thread:a:t'").fetch_one(&m.store.pool).await.unwrap();
                assert_eq!(recorded, (Some(error), tries), "{answer}");
                let summaries: Vec<String> =
                    sqlx::query_scalar("SELECT summary FROM items ORDER BY id").fetch_all(&m.store.pool).await.unwrap();
                assert_eq!(summaries, ["Plans for the workshop.", "Plans for the workshop."], "{answer}");
                assert_eq!(topics().await, previous_topics, "{answer}");
                assert_eq!(commitments().await, previous_commitments, "{answer}");
                if tries < 3 {
                    assert_eq!(retrying(&m, "thread:a:t", &version, &items).await.unwrap(), Some(true));
                    assert_eq!(next(&m, &version).await.unwrap().unwrap().target, "thread:a:t");
                } else {
                    assert_eq!(retrying(&m, "thread:a:t", &version, &items).await.unwrap(), None);
                    assert!(next(&m, &version).await.unwrap().is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn valid_answers_replace_previous_reads_and_clear_errors() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let task = Task { connection_id: "test".into(), model: "test".into(), effort: None };
        let version = version(&task);
        let items = read_mail_thread(&m, &task).await;
        done(&m, "thread:a:t", &version, &items, false, Err(anyhow::anyhow!("earlier read failed"))).await.unwrap();
        assert_eq!(retrying(&m, "thread:a:t", &version, &items).await.unwrap(), Some(true));
        let answer =
            r#"{"summary": "  New plans.  ", "topics": ["New topic"], "commitments": [{"who": "me", "what": "Send the new agenda"}]}"#;
        let v = llm::json_of(answer).unwrap();
        let result = apply(&m, &task, "thread", &items, &v).await;
        assert!(result.is_ok(), "{result:?}");
        done(&m, "thread:a:t", &version, &items, true, result).await.unwrap();
        for id in &items {
            let summary: String =
                sqlx::query_scalar("SELECT summary FROM items WHERE id = ?").bind(id).fetch_one(&m.store.pool).await.unwrap();
            assert_eq!(summary, "New plans.");
            let topics: Vec<(String, String)> = sqlx::query_as(
                "SELECT t.key, l.made_by FROM links l JOIN things t ON t.id = l.dst_thing WHERE l.src_item = ? AND l.rel = 'about'",
            )
            .bind(id)
            .fetch_all(&m.store.pool)
            .await
            .unwrap();
            assert_eq!(topics, [("topic:new-topic".into(), version.clone())]);
        }
        let commitments: Vec<(String, String, String, String)> =
            sqlx::query_as("SELECT item_id, who, what, status FROM commitments ORDER BY id").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(commitments, [(items.last().unwrap().clone(), "me".into(), "Send the new agenda".into(), "open".into())]);
        let recorded: (Option<String>, i64) =
            sqlx::query_as("SELECT error, tries FROM enrich WHERE target = 'thread:a:t'").fetch_one(&m.store.pool).await.unwrap();
        assert_eq!(recorded, (None, 0));
        assert_eq!(retrying(&m, "thread:a:t", &version, &items).await.unwrap(), None);
        assert!(next(&m, &version).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_reads_count_content_and_model_changes() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let id = m
            .store
            .put(
                &api::MemexItemIn {
                    kind: "document".into(),
                    source: "test".into(),
                    title: "Notes".into(),
                    text: "Notes.".into(),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let target = format!("item:{id}");
        let items = vec![id];
        let tries = async || -> (i64, Option<String>) {
            sqlx::query_as("SELECT tries, error FROM enrich WHERE target = ?").bind(&target).fetch_one(&m.store.pool).await.unwrap()
        };
        for (retry, expected) in [(false, 1), (true, 2), (true, 3)] {
            assert_eq!(retrying(&m, &target, "v1", &items).await.unwrap(), Some(retry));
            done(&m, &target, "v1", &items, retry, Err(anyhow::anyhow!("no JSON"))).await.unwrap();
            assert_eq!(tries().await, (expected, Some("no JSON".into())));
        }
        assert_eq!(retrying(&m, &target, "v1", &items).await.unwrap(), None, "even a stale selection gets no fourth try");
        assert_eq!(retrying(&m, &target, "v2", &items).await.unwrap(), Some(false));
        done(&m, &target, "v2", &items, false, Err(anyhow::anyhow!("new model"))).await.unwrap();
        assert_eq!(tries().await.0, 1);
        done(&m, &target, "v2", &items, true, Err(anyhow::anyhow!("again"))).await.unwrap();
        assert_eq!(tries().await.0, 2);
        sqlx::query("UPDATE enrich SET done_at = 9").execute(&m.store.pool).await.unwrap();
        sqlx::query("UPDATE items SET added_at = 10").execute(&m.store.pool).await.unwrap();
        assert_eq!(retrying(&m, &target, "v2", &items).await.unwrap(), Some(false));
        done(&m, &target, "v2", &items, false, Err(anyhow::anyhow!("new content"))).await.unwrap();
        assert_eq!(tries().await.0, 1);
        done(&m, &target, "v2", &items, true, Ok(())).await.unwrap();
        assert_eq!(tries().await, (0, None));
        assert_eq!(retrying(&m, &target, "v2", &items).await.unwrap(), None);
        let watermark: i64 =
            sqlx::query_scalar("SELECT done_at FROM enrich WHERE target = ?").bind(&target).fetch_one(&m.store.pool).await.unwrap();
        assert_eq!(watermark, 10, "the item's content, not the completion time");
    }

    #[tokio::test]
    async fn stale_selections_read_current_items_without_resetting_tries() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let mail = async |n: i64| {
            let id = m
                .store
                .put(
                    &api::MemexItemIn {
                        kind: "email".into(),
                        source: "test".into(),
                        external_id: n.to_string(),
                        title: "Notes".into(),
                        text: format!("Message {n}."),
                        created_at: Some(n),
                        ..Default::default()
                    },
                    "test",
                )
                .await
                .unwrap();
            sqlx::query("UPDATE items SET added_at = ? WHERE id = ?").bind(n).bind(&id).execute(&m.store.pool).await.unwrap();
            sqlx::query("INSERT INTO mail (item_id, account, gmail_id, thread_id, date, from_addr, blob) VALUES (?, 'a', ?, 't', ?, 'pat@example.com', '')")
                .bind(&id).bind(n.to_string()).bind(n).execute(&m.store.pool).await.unwrap();
            id
        };
        let first = mail(1).await;
        let unread = next(&m, "test").await.unwrap().unwrap();
        done(&m, &unread.target, "test", std::slice::from_ref(&first), false, Err(anyhow::anyhow!("no JSON"))).await.unwrap();
        let failed = retry(&m, "test").await.unwrap().unwrap();
        let second = mail(2).await;

        // Another lane reads the new mail before either selection claims it.
        let claim = m.claim("read:thread:a:t").unwrap();
        let items = thread_items(&m, "a", "t").await.unwrap();
        assert_eq!(retrying(&m, &unread.target, "test", &items).await.unwrap(), Some(false));
        done(&m, &unread.target, "test", &items, false, Err(anyhow::anyhow!("no JSON"))).await.unwrap();
        drop(claim);

        for (reading, tries) in [(unread, 2), (failed, 3)] {
            let _claim = m.claim(&format!("read:{}", reading.target)).unwrap();
            let items = reading.items(&m).await.unwrap();
            assert_eq!(items, [first.clone(), second.clone()]);
            assert!(input(&m, &reading.kind, &items).await.unwrap().contains("Message 2."));
            let retry = retrying(&m, &reading.target, "test", &items).await.unwrap().unwrap();
            assert!(retry);
            done(&m, &reading.target, "test", &items, retry, Err(anyhow::anyhow!("no JSON"))).await.unwrap();
            let recorded: (i64, i64) = sqlx::query_as("SELECT done_at, tries FROM enrich WHERE target = ?")
                .bind(&reading.target)
                .fetch_one(&m.store.pool)
                .await
                .unwrap();
            assert_eq!(recorded, (2, tries));
        }
        assert!(next(&m, "test").await.unwrap().is_none(), "no fourth try for the same content");
    }

    #[tokio::test]
    async fn retries_skip_claimed_missing_and_unreadable_targets() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let mut ids = Vec::new();
        for (kind, at) in [("document", 1), ("email", 2)] {
            ids.push(
                m.store
                    .put(
                        &api::MemexItemIn {
                            kind: kind.into(),
                            source: "test".into(),
                            title: kind.into(),
                            text: "Notes.".into(),
                            created_at: Some(at),
                            ..Default::default()
                        },
                        "test",
                    )
                    .await
                    .unwrap(),
            );
        }
        sqlx::query("INSERT INTO mail (item_id, account, gmail_id, thread_id, date, from_addr, blob) VALUES (?, 'a', 'msg', 't', 2, 'pat@example.com', '')")
            .bind(&ids[1]).execute(&m.store.pool).await.unwrap();
        let item = format!("item:{}", ids[0]);
        for (target, items) in [(item.as_str(), &ids[..1]), ("thread:a:t", &ids[1..]), ("item:gone", &[]), ("thread:a:gone", &[])] {
            done(&m, target, "test", items, false, Err(anyhow::anyhow!("no JSON"))).await.unwrap();
        }
        assert_eq!(next(&m, "test").await.unwrap().unwrap().target, "thread:a:t");
        assert_eq!(retry(&m, "test").await.unwrap().unwrap().target, "thread:a:t");
        assert!(retry(&m, "other model").await.unwrap().is_none());
        let thread_claim = m.claim("read:thread:a:t").unwrap();
        assert_eq!(retry(&m, "test").await.unwrap().unwrap().target, item);
        let item_claim = m.claim(&format!("read:{item}")).unwrap();
        assert!(retry(&m, "test").await.unwrap().is_none());
        drop(thread_claim);
        for field in ["spam", "trash", "list", "draft"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE mail SET {field} = 1"))).execute(&m.store.pool).await.unwrap();
            assert!(retry(&m, "test").await.unwrap().is_none(), "{field}");
            sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE mail SET {field} = 0"))).execute(&m.store.pool).await.unwrap();
        }
        for sender in [
            "noreply@x.test",
            "no-reply@x.test",
            "donotreply@x.test",
            "do-not-reply@x.test",
            "notifications@x.test",
            "mailer-daemon@x.test",
        ] {
            sqlx::query("UPDATE mail SET from_addr = ?").bind(sender).execute(&m.store.pool).await.unwrap();
            assert!(retry(&m, "test").await.unwrap().is_none(), "{sender}");
        }
        sqlx::query("DELETE FROM mail").execute(&m.store.pool).await.unwrap();
        sqlx::query("DELETE FROM items WHERE id = ?").bind(&ids[0]).execute(&m.store.pool).await.unwrap();
        drop(item_claim);
        assert!(next(&m, "test").await.unwrap().is_none());
    }

    #[test]
    fn candidate_windows_cover_every_lane() {
        for at_once in 1..=MAX_AT_ONCE {
            for floor in [10, 40] {
                assert!(window(at_once, floor) >= i64::from(floor));
                assert!(window(at_once, floor) >= i64::from(at_once + 8));
            }
        }
        assert_eq!(window(0, 10), 10);
        assert_eq!(window(1, 40), 40);
        assert_eq!(window(u32::MAX, 10), 72);
    }

    #[tokio::test]
    async fn items_pass_over_claimed_work() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        m.set_tasks(crate::Tasks { at_once: 12, ..Default::default() });
        let mut claims = Vec::new();
        let mut last = String::new();
        for i in 0..12 {
            let id = m
                .store
                .put(
                    &api::MemexItemIn {
                        kind: "document".into(),
                        source: "test".into(),
                        external_id: i.to_string(),
                        title: format!("Document {i}"),
                        text: "Notes.".into(),
                        created_at: Some(100 - i),
                        ..Default::default()
                    },
                    "test",
                )
                .await
                .unwrap();
            last = format!("item:{id}");
            if i < 11 {
                claims.push(m.claim(&format!("read:{last}")).unwrap());
            }
        }
        assert_eq!(next(&m, "test").await.unwrap().unwrap().target, last);
    }

    #[tokio::test]
    async fn lanes_follow_live_settings_without_overlapping_indices() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        m.set_tasks(crate::Tasks { at_once: 1, ..Default::default() });
        let (events, mut recv) = tokio::sync::mpsc::unbounded_channel();
        let supervisor = tokio::spawn(lanes(m.clone(), move |m, lane| {
            let events = events.clone();
            async move {
                events.send((lane, true)).unwrap();
                run(m, lane).await;
                events.send((lane, false)).unwrap();
            }
        }));
        assert_eq!(within("lane zero starts", recv.recv()).await, Some((0, true)));
        let mut active = std::collections::HashSet::from([0]);
        for at_once in [4, 1, 4] {
            m.set_tasks(crate::Tasks { at_once, ..Default::default() });
            for _ in 0..3 {
                let (lane, started) = within("lanes follow the setting", recv.recv()).await.unwrap();
                assert_ne!(lane, 0, "lane zero stays running");
                assert_eq!(started, at_once == 4);
                if started {
                    assert!(active.insert(lane), "a lane starts only once");
                } else {
                    assert!(active.remove(&lane));
                }
            }
            assert_eq!(active, (0..at_once).collect());
        }
        supervisor.abort();
        assert!(supervisor.await.unwrap_err().is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicked_lane_waits_for_a_wake_before_restarting() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        m.set_tasks(crate::Tasks { at_once: 2, ..Default::default() });
        let starts = [const { std::cell::Cell::new(0) }; 2];
        let (events, mut recv) = tokio::sync::mpsc::unbounded_channel();
        let supervisor = lanes(m.clone(), |_, lane| {
            let count = &starts[lane as usize];
            count.set(count.get() + 1);
            let panic = lane == 1 && count.get() == 1;
            let events = events.clone();
            async move {
                if panic {
                    events.send(()).unwrap();
                    panic!("test lane");
                }
                std::future::pending::<()>().await;
            }
        });
        let supervisor = tokio::task::unconstrained(supervisor);
        tokio::pin!(supervisor);
        assert!(futures_util::poll!(supervisor.as_mut()).is_pending());
        within("lane one panics", recv.recv()).await.unwrap();
        assert!(futures_util::poll!(supervisor.as_mut()).is_pending());
        assert_eq!(starts.each_ref().map(|n| n.get()), [1, 1]);

        m.wake.notify_waiters();
        assert!(futures_util::poll!(supervisor.as_mut()).is_pending());
        assert_eq!(starts.each_ref().map(|n| n.get()), [1, 2]);
    }
}
