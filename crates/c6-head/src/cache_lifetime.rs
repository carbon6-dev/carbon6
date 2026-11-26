//! Cache expiry evidence from consecutive calls, never from unrelated sessions.
//! A cache miss alone is not expiry: edits, compaction and provider eviction
//! also invalidate prefixes. Only substantial reads of a comparable context
//! establish a lower bound; rewrites above that bound can bracket it.

use std::collections::BTreeMap;

use c6_api::cache::{CacheLifetime, ModelCacheLifetime};
use serde::{Deserialize, Serialize};

use crate::{HeadState, bad_request};

const INITIALIZED: &str = "cache_lifetimes_enumerated";
const ENUM_CURSOR: &str = "cache_lifetimes_session_cursor";
const PAGE: i64 = 128;

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
struct Call {
    session_id: String,
    seq: i64,
    ts: i64,
    realm: String,
    connection_id: String,
    model: String,
    input: i64,
    cache_read: i64,
    cache_write: i64,
    started_at: i64,
    boundary_after: i64,
    eligible: bool,
    reliable: bool,
}

impl Call {
    fn context(&self) -> u64 {
        self.input.max(0) as u64 + self.cache_read.max(0) as u64 + self.cache_write.max(0) as u64
    }

    fn key(&self) -> (String, String, String) {
        (self.realm.clone(), self.connection_id.clone(), self.model.clone())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Evidence {
    observed: bool,
    longest_read: u64,
    read_pairs: u64,
    /// Five longest successful gaps: require repeated survival, not one long
    /// generation or another session briefly refreshing a shared prefix.
    read_samples: Vec<u64>,
    shortest_rewrite: Option<u64>,
    rewrite_pairs: u64,
}

impl Evidence {
    fn observe(&mut self, previous: Option<&Call>, next: &Call) {
        self.observed |= next.cache_read > 0 || next.cache_write > 0;
        let Some(previous) = previous else { return };
        if !previous.reliable
            || !next.reliable
            || previous.session_id != next.session_id
            || previous.key() != next.key()
            || next.seq <= previous.seq
        {
            return;
        }
        // Count only the idle interval, not the next call's generation time.
        // A long answer can otherwise make a five-minute cache look hours old.
        let read_gap = next.started_at.saturating_sub(previous.ts) / 1000;
        let rewrite_gap = next.ts.saturating_sub(previous.started_at) / 1000;
        // Zero/negative time, tiny prefixes, huge gaps, and context shrinkage
        // do not distinguish a live prefix from a different cached prompt.
        let size = previous.context();
        if size < 1024 || next.context() < size * 4 / 5 || next.context() > size * 5 / 4 {
            return;
        }
        if next.cache_read.max(0) as u64 >= size * 4 / 5 && (1..=86_400).contains(&read_gap) {
            let gap = read_gap as u64;
            self.read_pairs += 1;
            self.longest_read = self.longest_read.max(gap);
            self.read_samples.push(gap);
            self.read_samples.sort_unstable_by(|a, b| b.cmp(a));
            self.read_samples.truncate(5);
            // A proven read defeats an earlier miss (prefix changes are common).
            if self.shortest_rewrite.is_some_and(|miss| miss <= self.longest_read) {
                self.shortest_rewrite = None;
                self.rewrite_pairs = 0;
            }
        } else if next.cache_read.max(0) as u64 <= size / 10
            && next.cache_write.max(0) as u64 >= size * 4 / 5
            && (1..=86_400).contains(&rewrite_gap)
            && rewrite_gap as u64 > self.longest_read
        {
            let gap = rewrite_gap as u64;
            // Input alone is NOT a rewrite: OpenAI's uncached input can be
            // a different prompt. Only explicit creation tokens are evidence.
            self.shortest_rewrite = Some(self.shortest_rewrite.map_or(gap, |old| old.min(gap)));
            self.rewrite_pairs += 1;
        }
    }

    fn estimate(&self, kind: &str) -> CacheLifetime {
        let default = match kind {
            "anthropic" => Some(300), // current ephemeral cache_control (no TTL)
            "claude_code" => Some(3600),
            "openai" | "chatgpt" => Some(600),
            _ => None,
        };
        // Don't turn the many 2-second reads in an active turn into a 2-second
        // TTL. Need repeated reads and either a longer-than-default survival,
        // or repeated rewrites bracketing the read to within 20%.
        let supported = self.read_samples.get(4).copied().unwrap_or(0);
        let bracketed = self.rewrite_pairs >= 2 && self.shortest_rewrite.is_some_and(|upper| supported >= upper * 4 / 5);
        let learned = supported >= 60 && (default.is_none_or(|d| supported >= d) || bracketed);
        CacheLifetime {
            lifetime_seconds: if learned { Some(supported) } else { default },
            learned,
            cache_observed: self.observed,
            read_to_rewrite_ratio: None,
        }
    }
}

const CALLS: &str = "SELECT u.session_id, u.seq, u.ts, u.realm, u.connection_id, u.model, u.input, u.cache_read, u.cache_write, \
    0 AS started_at, 0 AS boundary_after, 0 AS reliable, \
    CASE WHEN i.session_id IS NOT NULL AND u.seq <= i.imported_seq THEN 0 \
         WHEN c.kind IN ('alias', 'pool') THEN 0 ELSE 1 END AS eligible \
    FROM usage u LEFT JOIN session_imports i ON i.session_id = u.session_id LEFT JOIN connections c ON c.id = u.connection_id";

#[derive(Default, Serialize, Deserialize)]
struct Timing {
    boundary: i64,
    assistants: u64,
    started: i64,
    completed: i64,
}

impl Timing {
    fn event(&mut self, kind: Option<u32>, ts: i64) {
        match kind {
            Some(11 | 12 | 14 | 18 | 19 | 32) => self.boundary = ts,
            Some(13) => {
                self.assistants += 1;
                self.started = self.boundary;
                self.completed = ts;
            }
            _ => {}
        }
    }

    fn apply(&self, call: &mut Call) {
        call.started_at = self.started;
        call.ts = self.completed;
        call.boundary_after = self.boundary;
        // Multiple assistant messages can be a turn total, not one model call.
        call.reliable = call.eligible && self.assistants == 1 && self.started > 0 && self.completed >= self.started;
    }
}

/// Prost puts the envelope's kind tag before its (possibly huge) payload.
/// Unfamiliar/truncated headers are ignored rather than used as evidence.
fn event_kind(mut header: &[u8]) -> Option<u32> {
    use prost::encoding::{DecodeContext, decode_key, skip_field};
    while !header.is_empty() {
        let (tag, wire) = decode_key(&mut header).ok()?;
        if tag >= 10 {
            return Some(tag);
        }
        skip_field(wire, tag, &mut header, DecodeContext::default()).ok()?;
    }
    None
}

async fn save_evidence(tx: &mut sqlx::SqliteConnection, realm: &str, connection: &str, model: &str, e: &Evidence) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO cache_lifetime_estimates (realm, connection_id, model, evidence) VALUES (?, ?, ?, ?)")
        .bind(realm)
        .bind(connection)
        .bind(model)
        .bind(serde_json::to_string(e)?)
        .execute(tx)
        .await?;
    Ok(())
}

async fn save_previous(tx: &mut sqlx::SqliteConnection, session: &str, call: &Call) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO cache_lifetime_previous (session_id, call) VALUES (?, ?)")
        .bind(session)
        .bind(serde_json::to_string(call)?)
        .execute(tx)
        .await?;
    Ok(())
}

/// Called inside the event/usage insertion transaction. During bootstrap only
/// register durable work: never replay history under a foreground writer lock.
pub async fn record(tx: &mut sqlx::SqliteConnection, session: &str, seq: u64) -> anyhow::Result<()> {
    let enumerated: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM settings WHERE key = ?)").bind(INITIALIZED).fetch_one(&mut *tx).await?;
    // Only genuinely new sessions may bypass historical replay after bootstrap.
    let historical: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM usage WHERE session_id = ? AND seq < ?)")
        .bind(session)
        .bind(seq as i64)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("INSERT OR IGNORE INTO cache_lifetime_replay (session_id, after_seq, timing, live) VALUES (?, 0, ?, ?)")
        .bind(session)
        .bind(serde_json::to_string(&Timing::default())?)
        .bind(enumerated && !historical)
        .execute(&mut *tx)
        .await?;
    let (after, live): (i64, bool) =
        sqlx::query_as("SELECT after_seq, live FROM cache_lifetime_replay WHERE session_id = ?").bind(session).fetch_one(&mut *tx).await?;
    if live && (seq as i64) > after {
        replay_page(tx, session, Some(seq as i64)).await?;
    }
    Ok(())
}

/// At most 128 short headers and the usage rows in that exact interval. Timing,
/// evidence, previous call and cursor share a transaction, including mid-call
/// checkpoints. Foreground callers defer oversized intervals in their entirety.
async fn replay_page(tx: &mut sqlx::SqliteConnection, session: &str, through: Option<i64>) -> anyhow::Result<()> {
    let (after, saved): (i64, String) = sqlx::query_as("SELECT after_seq, timing FROM cache_lifetime_replay WHERE session_id = ?")
        .bind(session)
        .fetch_one(&mut *tx)
        .await?;
    let events: Vec<(i64, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT seq, ts, substr(data, 1, 32) FROM events WHERE session_id = ? AND seq > ? AND seq <= ? ORDER BY seq LIMIT ?",
    )
    .bind(session)
    .bind(after)
    .bind(through.unwrap_or(i64::MAX))
    .bind(PAGE + i64::from(through.is_some()))
    .fetch_all(&mut *tx)
    .await?;
    if through.is_some() && events.len() > PAGE as usize {
        sqlx::query("UPDATE cache_lifetime_replay SET live = 0 WHERE session_id = ?").bind(session).execute(&mut *tx).await?;
        return Ok(());
    }
    let end = events.last().map_or(after, |e| e.0);
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(CALLS);
    query.push(" WHERE u.session_id = ").push_bind(session).push(" AND u.seq > ").push_bind(after).push(" AND u.seq <= ").push_bind(end);
    let calls: Vec<Call> = query.build_query_as().fetch_all(&mut *tx).await?;
    let mut calls: BTreeMap<_, _> = calls.into_iter().map(|c| (c.seq, c)).collect();
    let saved_previous: Option<String> =
        sqlx::query_scalar("SELECT call FROM cache_lifetime_previous WHERE session_id = ?").bind(session).fetch_optional(&mut *tx).await?;
    let mut previous: Option<Call> = saved_previous.map(|s| serde_json::from_str(&s)).transpose()?;
    let mut timing: Timing = serde_json::from_str(&saved)?;
    for (seq, ts, header) in &events {
        let kind = event_kind(header);
        timing.event(kind, *ts);
        if kind == Some(15) {
            if let Some(mut call) = calls.remove(seq) {
                timing.apply(&mut call);
                if !call.connection_id.is_empty() && !call.model.is_empty() {
                    let data: Option<String> = sqlx::query_scalar(
                        "SELECT evidence FROM cache_lifetime_estimates WHERE realm = ? AND connection_id = ? AND model = ?",
                    )
                    .bind(&call.realm)
                    .bind(&call.connection_id)
                    .bind(&call.model)
                    .fetch_optional(&mut *tx)
                    .await?;
                    let mut evidence: Evidence = data.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
                    evidence.observe(previous.as_ref(), &call);
                    save_evidence(tx, &call.realm, &call.connection_id, &call.model, &evidence).await?;
                }
                save_previous(tx, session, &call).await?;
                previous = Some(call);
            }
            timing.assistants = 0;
        }
    }
    // Background uses BEGIN IMMEDIATE: a short page proves there are no more
    // events while ingestion is excluded. Subsequent arrivals use record().
    let live = through.is_some() || events.len() < PAGE as usize;
    sqlx::query("UPDATE cache_lifetime_replay SET after_seq = ?, timing = ?, live = ? WHERE session_id = ?")
        .bind(end)
        .bind(serde_json::to_string(&timing)?)
        .bind(live)
        .bind(session)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Enumerate outside the writer transaction, then register just one keyset page.
/// Foreground registration covers new sessions whose IDs precede this cursor.
async fn enumerate_page(pool: &sqlx::SqlitePool) -> anyhow::Result<bool> {
    let done: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM settings WHERE key = ?)").bind(INITIALIZED).fetch_one(pool).await?;
    if done {
        return Ok(false);
    }
    let saved: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(ENUM_CURSOR).fetch_optional(pool).await?;
    let sessions: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT session_id FROM usage WHERE session_id > ? ORDER BY session_id LIMIT ?")
            .bind(saved.as_deref().unwrap_or(""))
            .bind(PAGE)
            .fetch_all(pool)
            .await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(ENUM_CURSOR).fetch_optional(&mut *tx).await?;
    if current != saved {
        tx.rollback().await?;
        return Ok(true);
    }
    for session in &sessions {
        sqlx::query("INSERT OR IGNORE INTO cache_lifetime_replay (session_id, after_seq, timing, live) VALUES (?, 0, ?, 0)")
            .bind(session)
            .bind(serde_json::to_string(&Timing::default())?)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(last) = sessions.last() {
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)").bind(ENUM_CURSOR).bind(last).execute(&mut *tx).await?;
    }
    if sessions.len() < PAGE as usize {
        sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, 'true')").bind(INITIALIZED).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

async fn backfill_page(pool: &sqlx::SqlitePool) -> anyhow::Result<bool> {
    // Historical usage indexing must finish first, or an event could be passed
    // before its usage row exists. Live event+usage insertion is already atomic.
    let indexed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM settings WHERE key = 'usage_backfilled')").fetch_one(pool).await?;
    if !indexed {
        return Ok(false);
    }
    if enumerate_page(pool).await? {
        return Ok(true);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let session: Option<String> =
        sqlx::query_scalar("SELECT session_id FROM cache_lifetime_replay WHERE live = 0 ORDER BY session_id LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
    let Some(session) = session else {
        // Drop queues an asynchronous rollback. Await it so a completed page
        // has actually released its writer before foreground work resumes.
        tx.rollback().await?;
        return Ok(false);
    };
    replay_page(&mut tx, &session, None).await?;
    tx.commit().await?;
    Ok(true)
}

pub fn spawn_backfill(st: std::sync::Arc<HeadState>) {
    tokio::spawn(async move {
        while !st.shutdown.is_cancelled() {
            let busy = match backfill_page(&st.store.pool).await {
                Ok(busy) => busy,
                Err(e) => {
                    tracing::warn!("cache lifetime backfill: {e:#}");
                    false
                }
            };
            // Release the writer between pages, including when catching up a
            // formerly-live session. Retry pending work after errors/restarts.
            tokio::select! {
                _ = st.shutdown.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_millis(if busy { 10 } else { 1000 })) => {}
            }
        }
    });
}

fn priced(mut estimate: CacheLifetime, kind: &str, model: &str, prices: &[c6_api::Price]) -> CacheLifetime {
    if let Some(price) = crate::usage::price_for(prices, model) {
        let rewrite = if matches!(kind, "anthropic" | "claude_code") { price.cache_write.unwrap_or(price.input) } else { price.input };
        let ratio = price.cache_read.unwrap_or(price.input) / rewrite;
        if rewrite > 0.0 && ratio.is_finite() && ratio >= 0.0 {
            estimate.read_to_rewrite_ratio = Some(ratio);
        }
    }
    estimate
}

/// Scope both metadata and evidence to the requesting session's realm. Resolve
/// aliases before reading evidence; a pool has no fixed target and no default.
pub async fn for_session(st: &HeadState, session: &crate::store::SessionRow, params: &serde_json::Value) -> anyhow::Result<CacheLifetime> {
    let id = params["connection_id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| bad_request("connection_id is required"))?;
    let model = params["model"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| bad_request("model is required"))?;
    let resolved = st.store.resolve_connection(id).await?.ok_or_else(|| bad_request("no such connection"))?;
    crate::sessions::may_use_connection(st, session, &resolved.row).await.map_err(bad_request)?;
    // Unlike choosing a subagent model, a maintenance query must concern this
    // session's selected connection or a concrete account actually granted to
    // its tree. Mere availability as an unrelated subagent provider is not
    // evidence that it owns the saved prefix.
    if id != session.connection_id && !crate::aliases::prior_target_allowed(&st.store, session, &resolved.row).await? {
        return Err(bad_request("no prior concrete grant for this session's cache"));
    }
    let concrete = resolved.resolved_connection_id.as_deref().unwrap_or(&resolved.row.id);
    let data: Option<String> =
        sqlx::query_scalar("SELECT evidence FROM cache_lifetime_estimates WHERE realm = ? AND connection_id = ? AND model = ?")
            .bind(&session.realm)
            .bind(concrete)
            .bind(model)
            .fetch_optional(&st.store.pool)
            .await?;
    let evidence: Evidence = data.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
    Ok(priced(evidence.estimate(&resolved.row.kind), &resolved.row.kind, model, &crate::usage::prices(st).await?))
}

/// Read-only catalog view. Realm callers never see another realm's usage.
pub async fn list(st: &HeadState, id: &str, realm: Option<&str>) -> anyhow::Result<Vec<ModelCacheLifetime>> {
    let resolved = st.store.resolve_connection(id).await?.ok_or_else(|| bad_request("no such connection"))?;
    if realm.is_some_and(|r| !crate::realms::available(&resolved.row.realms, r)) {
        return Err(crate::not_found("no such connection"));
    }
    let concrete = resolved.resolved_connection_id.as_deref().unwrap_or(&resolved.row.id);
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT model, evidence FROM cache_lifetime_estimates WHERE connection_id = ? AND (? IS NULL OR realm = ?)")
            .bind(concrete)
            .bind(realm)
            .bind(realm)
            .fetch_all(&st.store.pool)
            .await?;
    let mut models = BTreeMap::<String, Evidence>::new();
    for (model, data) in rows {
        let e: Evidence = serde_json::from_str(&data)?;
        let merged = models.entry(model).or_default();
        merged.observed |= e.observed;
        merged.longest_read = merged.longest_read.max(e.longest_read);
        merged.read_pairs += e.read_pairs;
        merged.read_samples.extend(e.read_samples);
        merged.read_samples.sort_unstable_by(|a, b| b.cmp(a));
        merged.read_samples.truncate(5);
        merged.rewrite_pairs += e.rewrite_pairs;
        merged.shortest_rewrite = merged.shortest_rewrite.into_iter().chain(e.shortest_rewrite).min();
    }
    if let Some(model) = &resolved.row.default_model {
        models.entry(model.clone()).or_default();
    }
    if models.is_empty() {
        models.insert(String::new(), Evidence::default());
    }
    let prices = crate::usage::prices(st).await?;
    Ok(models
        .into_iter()
        .map(|(model, e)| ModelCacheLifetime {
            connection_id: concrete.to_owned(),
            estimate: priced(e.estimate(&resolved.row.kind), &resolved.row.kind, &model, &prices),
            model,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(seq: i64, seconds: i64, read: i64, write: i64) -> Call {
        Call {
            session_id: "s".into(),
            seq,
            ts: seconds * 1000,
            realm: "home".into(),
            connection_id: "c".into(),
            model: "m".into(),
            input: 100,
            cache_read: read,
            cache_write: write,
            started_at: seconds * 1000,
            boundary_after: seconds * 1000,
            eligible: true,
            reliable: true,
        }
    }

    #[test]
    fn provider_defaults_are_not_claimed_as_learned() {
        for (kind, seconds) in [
            ("anthropic", Some(300)),
            ("claude_code", Some(3600)),
            ("openai", Some(600)),
            ("chatgpt", Some(600)),
            ("openai_compat", None),
            ("pool", None),
        ] {
            let e = Evidence::default().estimate(kind);
            assert_eq!(e.lifetime_seconds, seconds);
            assert!(!e.learned && !e.cache_observed);
        }
    }

    #[test]
    fn read_to_rewrite_ratio_is_only_returned_for_known_nonzero_prices() {
        let prices = [c6_api::Price { model: "m*".into(), input: 10.0, output: 30.0, cache_read: Some(1.0), cache_write: Some(12.5) }];
        let e = Evidence::default().estimate("anthropic");
        assert_eq!(priced(e.clone(), "anthropic", "model", &prices).read_to_rewrite_ratio, Some(0.08));
        assert_eq!(priced(e.clone(), "openai", "model", &prices).read_to_rewrite_ratio, Some(0.1));
        assert_eq!(priced(e.clone(), "anthropic", "unknown", &prices).read_to_rewrite_ratio, None);
        let free = [c6_api::Price { input: 0.0, cache_write: Some(0.0), ..prices[0].clone() }];
        assert_eq!(priced(e, "anthropic", "model", &free).read_to_rewrite_ratio, None);
    }

    #[test]
    fn reads_extend_lifetime_but_fast_active_calls_do_not_shorten_it() {
        let mut e = Evidence::default();
        let a = call(1, 0, 0, 10_000);
        let b = call(2, 10, 10_000, 100);
        let c = call(3, 20, 10_000, 100);
        e.observe(None, &a);
        e.observe(Some(&a), &b);
        e.observe(Some(&b), &c);
        assert_eq!(e.estimate("claude_code").lifetime_seconds, Some(3600));
        assert!(!e.estimate("claude_code").learned);
        let mut previous = c;
        for i in 1..=5 {
            let next = call(3 + i, 20 + i * 3600, 10_000, 100);
            e.observe(Some(&previous), &next);
            previous = next;
        }
        assert!(e.estimate("claude_code").learned);
        assert_eq!(e.longest_read, 3600);
    }

    #[test]
    fn short_misses_are_not_expiry_and_reads_defeat_them() {
        let mut e = Evidence::default();
        let a = call(1, 0, 0, 10_000);
        let b = call(2, 10, 0, 10_000);
        let c = call(3, 300, 10_000, 100);
        e.observe(Some(&a), &b);
        e.observe(Some(&b), &c);
        assert_eq!(e.shortest_rewrite, None);
        assert_eq!(e.rewrite_pairs, 0);
    }

    #[test]
    fn comparable_reads_and_repeated_rewrites_can_learn_a_shorter_lifetime() {
        let mut e = Evidence::default();
        let calls = [
            call(1, 0, 0, 10_000),
            call(2, 280, 10_000, 100),
            call(3, 560, 10_000, 100),
            call(4, 840, 10_000, 100),
            call(5, 1120, 10_000, 100),
            call(6, 1400, 10_000, 100),
            call(7, 1710, 0, 10_000),
            call(8, 2020, 0, 10_000),
        ];
        for pair in calls.windows(2) {
            e.observe(Some(&pair[0]), &pair[1]);
        }
        assert_eq!(e.estimate("claude_code").lifetime_seconds, Some(280));
        assert!(e.estimate("claude_code").learned);
    }

    #[test]
    fn switches_shrinks_tiny_prefixes_and_uncached_input_are_ambiguous() {
        let a = call(1, 0, 0, 10_000);
        let mut b = call(2, 3700, 10_000, 100);
        for change in 0..5 {
            let mut e = Evidence::default();
            let mut a = a.clone();
            let mut b = b.clone();
            match change {
                0 => b.connection_id = "other-account".into(),
                1 => b.model = "other-model".into(),
                2 => b.session_id = "other-session".into(),
                3 => b.cache_read = 500,
                _ => {
                    a.cache_write = 500;
                    b.cache_read = 500;
                }
            }
            e.observe(Some(&a), &b);
            assert_eq!(e.longest_read, 0);
        }
        b.cache_read = 0;
        b.cache_write = 0;
        b.input = 10_000;
        let mut e = Evidence::default();
        e.observe(Some(&a), &b);
        assert_eq!(e.shortest_rewrite, None);
    }

    #[test]
    fn a_single_long_read_does_not_prove_a_long_lifetime() {
        let mut e = Evidence::default();
        let mut previous = call(1, 0, 0, 10_000);
        for i in 1..=10 {
            let next = call(i + 1, i * 10, 10_000, 100);
            e.observe(Some(&previous), &next);
            previous = next;
        }
        e.observe(Some(&previous), &call(12, 20_000, 10_000, 100));
        assert!(!e.estimate("claude_code").learned);
        assert_eq!(e.estimate("claude_code").lifetime_seconds, Some(3600));
        let mut unreliable = call(13, 40_000, 10_000, 100);
        unreliable.reliable = false;
        e.observe(Some(&previous), &unreliable);
        assert_eq!(e.longest_read, 19_900);
    }

    #[test]
    fn delayed_usage_keeps_the_original_interval_and_the_next_prompt_boundary() {
        let mut timing = Timing::default();
        timing.event(Some(11), 1000);
        timing.event(Some(13), 10_000);
        // Tool execution belongs to the following request, although Claude
        // Code has not flushed the preceding call's usage yet.
        timing.event(Some(14), 20_000);
        let mut previous = call(1, 30, 0, 10_000);
        timing.apply(&mut previous);
        assert_eq!((previous.started_at, previous.ts, previous.boundary_after), (1000, 10_000, 20_000));
        let mut timing = Timing { boundary: previous.boundary_after, ..Default::default() };
        timing.event(Some(13), 700_000); // a long generation is not idle time
        let mut next = call(2, 800, 10_000, 100);
        timing.apply(&mut next);
        let mut e = Evidence::default();
        e.observe(Some(&previous), &next);
        assert_eq!(e.longest_read, 10);
        // A turn total covering several assistant messages is not a call.
        timing.event(Some(13), 710_000);
        timing.apply(&mut next);
        assert!(!next.reliable);
    }

    #[test]
    fn header_inspection_does_not_decode_large_message_payloads() {
        use prost::Message;
        let event = c6_proto::SessionEvent {
            seq: 123456,
            ts_ms: 1790000000000,
            kind: Some(c6_proto::session_event::Kind::Assistant(c6_proto::AssistantMessage {
                blocks: vec![c6_proto::ContentBlock {
                    block: Some(c6_proto::content_block::Block::Text("x".repeat(100_000))),
                    ..Default::default()
                }],
                ..Default::default()
            })),
        };
        let bytes = event.encode_to_vec();
        assert_eq!(event_kind(&bytes[..32]), Some(13));
        assert_eq!(event_kind(&[0xff]), None);
    }

    async fn append_calls(tx: &mut sqlx::SqliteConnection, session: &str, start: i64, end: i64) {
        use prost::Message;
        for i in start..end {
            let seq = i * 3 + 3;
            let assistant_ts = (100 + i * 3600) * 1000;
            // Delayed usage flushes must never inflate the cache idle interval.
            let usage_ts = assistant_ts + i * 10_000_000;
            sqlx::query("INSERT INTO usage (session_id, seq, ts, connection_id, model, input, output, cache_read, cache_write, realm) VALUES (?, ?, ?, 'c', 'm', 100, 1, ?, ?, 'home')")
                .bind(session).bind(seq).bind(usage_ts).bind(if i == 0 { 0 } else { 10_000 })
                .bind(if i == 0 { 10_000 } else { 100 }).execute(&mut *tx).await.unwrap();
            for event in [
                c6_proto::SessionEvent {
                    seq: seq as u64 - 2,
                    ts_ms: assistant_ts,
                    kind: Some(c6_proto::session_event::Kind::User(Default::default())),
                },
                c6_proto::SessionEvent {
                    seq: seq as u64 - 1,
                    ts_ms: assistant_ts,
                    kind: Some(c6_proto::session_event::Kind::Assistant(Default::default())),
                },
                c6_proto::SessionEvent {
                    seq: seq as u64,
                    ts_ms: usage_ts,
                    kind: Some(c6_proto::session_event::Kind::Usage(Default::default())),
                },
            ] {
                sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES (?, ?, ?, ?)")
                    .bind(session)
                    .bind(event.seq as i64)
                    .bind(event.ts_ms)
                    .bind(event.encode_to_vec())
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
        }
    }

    async fn replay_evidence(pool: &sqlx::SqlitePool) -> Evidence {
        let data: String =
            sqlx::query_scalar("SELECT evidence FROM cache_lifetime_estimates WHERE connection_id = 'c'").fetch_one(pool).await.unwrap();
        serde_json::from_str(&data).unwrap()
    }

    #[tokio::test]
    async fn bounded_replay_releases_writer_catches_arrivals_and_resumes_mid_call_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("head.db");
        let store = crate::store::Store::open(&path).await.unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        append_calls(&mut tx, "s", 0, 90).await;
        record(&mut tx, "s", 270).await.unwrap();
        tx.commit().await.unwrap();
        // Never pass events until the concurrent usage indexer has finished.
        assert!(!backfill_page(&store.pool).await.unwrap());
        sqlx::query("INSERT INTO settings (key, value) VALUES ('usage_backfilled', 'true')").execute(&store.pool).await.unwrap();
        assert!(backfill_page(&store.pool).await.unwrap()); // enumeration
        assert!(backfill_page(&store.pool).await.unwrap()); // exactly 128 headers
        let (after, saved): (i64, String) = sqlx::query_as("SELECT after_seq, timing FROM cache_lifetime_replay WHERE session_id = 's'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(after, 128);
        assert_eq!(serde_json::from_str::<Timing>(&saved).unwrap().assistants, 1);
        assert_eq!(replay_evidence(&store.pool).await.read_pairs, 41);
        // A separate foreground writer commits between background pages. Its
        // pending-session usage is not learned early, and is not lost either.
        let mut writer = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        append_calls(&mut writer, "s", 90, 91).await;
        record(&mut writer, "s", 273).await.unwrap();
        writer.commit().await.unwrap();
        assert_eq!(replay_evidence(&store.pool).await.read_pairs, 41);
        store.pool.close().await;

        let store = crate::store::Store::open(&path).await.unwrap();
        while backfill_page(&store.pool).await.unwrap() {}
        // An idle page must release its writer before returning, not merely
        // queue a rollback on a pool connection's SQLite worker thread.
        {
            use sqlx::Connection as _;
            let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).busy_timeout(std::time::Duration::ZERO);
            let mut foreground = sqlx::SqliteConnection::connect_with(&options).await.unwrap();
            sqlx::query("BEGIN IMMEDIATE").execute(&mut foreground).await.unwrap();
            sqlx::query("ROLLBACK").execute(&mut foreground).await.unwrap();
            foreground.close().await.unwrap();
        }
        let e = replay_evidence(&store.pool).await;
        assert_eq!(e.read_pairs, 90); // includes the mid-call checkpoint and arrival
        assert_eq!(e.longest_read, 3600);
        assert!(e.estimate("claude_code").learned);
        // Production calls record after inserting an event and already owns
        // the writer. A duplicate-only probe must obtain the same authority,
        // rather than attempt a deferred read-to-write lock upgrade.
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        record(&mut tx, "s", 273).await.unwrap(); // duplicate is inert
        append_calls(&mut tx, "s", 91, 92).await;
        record(&mut tx, "s", 276).await.unwrap(); // bounded live increment
        tx.commit().await.unwrap();
        assert_eq!(replay_evidence(&store.pool).await.read_pairs, 91);
        assert!(!backfill_page(&store.pool).await.unwrap());

        // Too much intervening activity requeues a live session without any
        // partial foreground evidence/cursor changes.
        let mut tx = store.pool.begin().await.unwrap();
        append_calls(&mut tx, "s", 92, 136).await;
        record(&mut tx, "s", 408).await.unwrap();
        tx.commit().await.unwrap();
        let checkpoint: (i64, bool) = sqlx::query_as("SELECT after_seq, live FROM cache_lifetime_replay WHERE session_id = 's'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(checkpoint, (276, false));
        assert_eq!(replay_evidence(&store.pool).await.read_pairs, 91);
        while backfill_page(&store.pool).await.unwrap() {}
        assert_eq!(replay_evidence(&store.pool).await.read_pairs, 135);
        store.pool.close().await;
    }

    #[tokio::test]
    async fn failed_checkpoint_rolls_back_evidence_and_previous_call() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("head.db")).await.unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        append_calls(&mut tx, "s", 0, 3).await;
        record(&mut tx, "s", 9).await.unwrap();
        tx.commit().await.unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON cache_lifetime_replay BEGIN SELECT RAISE(ABORT, 'checkpoint rejected'); END",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert!(replay_page(&mut tx, "s", None).await.is_err());
        tx.rollback().await.unwrap();
        for table in ["cache_lifetime_estimates", "cache_lifetime_previous"] {
            let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT count(*) FROM ");
            query.push(table);
            let count: i64 = query.build_query_scalar().fetch_one(&store.pool).await.unwrap();
            assert_eq!(count, 0);
        }
        sqlx::query("DROP TRIGGER reject_checkpoint").execute(&store.pool).await.unwrap();
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        replay_page(&mut tx, "s", None).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(replay_evidence(&store.pool).await.read_pairs, 2);
        store.pool.close().await;
    }

    #[tokio::test]
    async fn enumeration_is_keyset_paged_and_foreground_covers_ids_before_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("head.db")).await.unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        for i in 0..129 {
            append_calls(&mut tx, &format!("s{i:03}"), 0, 1).await;
        }
        tx.commit().await.unwrap();
        assert!(enumerate_page(&store.pool).await.unwrap());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cache_lifetime_replay").fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 128);
        let mut tx = store.pool.begin().await.unwrap();
        append_calls(&mut tx, "earlier", 0, 1).await;
        record(&mut tx, "earlier", 3).await.unwrap();
        tx.commit().await.unwrap();
        assert!(enumerate_page(&store.pool).await.unwrap());
        assert!(!enumerate_page(&store.pool).await.unwrap());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cache_lifetime_replay").fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 130);
        let mut tx = store.pool.begin().await.unwrap();
        append_calls(&mut tx, "brandnew", 0, 1).await;
        record(&mut tx, "brandnew", 3).await.unwrap();
        tx.commit().await.unwrap();
        let live: bool = sqlx::query_scalar("SELECT live FROM cache_lifetime_replay WHERE session_id = 'brandnew'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(live);
        assert!(replay_evidence(&store.pool).await.observed);
        store.pool.close().await;
    }

    #[tokio::test]
    async fn alias_queries_use_the_current_actual_target_and_realm_evidence() {
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
        let worker = c6_core::WorkerId::new();
        for (id, kind, realms) in
            [("a", "alias", "[]"), ("t1", "anthropic", "[]"), ("t2", "anthropic", "[]"), ("private", "anthropic", "[\"other\"]")]
        {
            sqlx::query("INSERT INTO connections (id, name, kind, base_url, created_at, realms) VALUES (?, ?, ?, '', 0, ?)")
                .bind(id)
                .bind(id)
                .bind(kind)
                .bind(realms)
                .execute(&st.store.pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO connection_aliases (alias_id, target_id, target_kind, switched_at) VALUES ('a', 't1', 'anthropic', 0)")
            .execute(&st.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, realm) VALUES ('s', ?, '', '', 'a', 'm', 'idle', 0, 0, 'home')")
            .bind(worker.to_string()).execute(&st.store.pool).await.unwrap();
        let mut tx = st.store.pool.begin().await.unwrap();
        for (realm, target, seconds) in [("home", "t1", 420), ("other", "t1", 5000), ("home", "t2", 900)] {
            let e = Evidence { observed: true, longest_read: seconds, read_pairs: 5, read_samples: vec![seconds; 5], ..Default::default() };
            save_evidence(&mut tx, realm, target, "m", &e).await.unwrap();
        }
        tx.commit().await.unwrap();
        let session = st.store.session("s").await.unwrap().unwrap();
        let params = serde_json::json!({"connection_id": "a", "model": "m"});
        let pinned = serde_json::json!({"connection_id": "t1", "model": "m"});
        assert!(for_session(st, &session, &pinned).await.is_err(), "available subagent targets are not saved-prefix grants");
        sqlx::query("INSERT INTO connection_alias_grants (root_session_id, worker_id, alias_id, target_id, alias_name, target_name) VALUES ('s', ?, 'a', 't1', 'a', 't1')")
            .bind(worker.to_string()).execute(&st.store.pool).await.unwrap();
        assert_eq!(for_session(st, &session, &params).await.unwrap().lifetime_seconds, Some(420));
        let rows = list(st, "a", Some("home")).await.unwrap();
        assert_eq!((rows[0].connection_id.as_str(), rows[0].estimate.lifetime_seconds), ("t1", Some(420)));
        sqlx::query("UPDATE connection_aliases SET target_id = 't2' WHERE alias_id = 'a'").execute(&st.store.pool).await.unwrap();
        assert_eq!(for_session(st, &session, &params).await.unwrap().lifetime_seconds, Some(900));
        assert_eq!(for_session(st, &session, &pinned).await.unwrap().lifetime_seconds, Some(420));
        // The NEW target's realm must not affect a previously granted prefix.
        sqlx::query("UPDATE connections SET realms = '[\"other\"]' WHERE id = 't2'").execute(&st.store.pool).await.unwrap();
        assert!(for_session(st, &session, &params).await.is_err());
        assert_eq!(for_session(st, &session, &pinned).await.unwrap().lifetime_seconds, Some(420));
        let grant = crate::sessions::credential_for(
            st,
            Some(worker),
            "worker",
            c6_proto::CredentialRequest {
                connection_id: "t1".into(),
                session_id: "s".into(),
                supports_alias: true,
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        assert!(grant.error.is_empty(), "{}", grant.error);
        assert_eq!(grant.connection_id, "t1");
        assert!(!grant.is_alias);
        assert_eq!(grant.kind, "anthropic");
        let other_worker = c6_core::WorkerId::new();
        let denied = crate::sessions::credential_for(
            st,
            Some(other_worker),
            "other",
            c6_proto::CredentialRequest {
                connection_id: "t1".into(),
                session_id: "s".into(),
                supports_alias: true,
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        assert!(!denied.error.is_empty());
        let target = st.store.connection("t1").await.unwrap().unwrap();
        let mut moved = session.clone();
        moved.worker_id = other_worker.to_string();
        assert!(!crate::aliases::prior_target_allowed(&st.store, &moved, &target).await.unwrap());
        moved = session.clone();
        moved.realm = "other".into();
        assert!(!crate::aliases::prior_target_allowed(&st.store, &moved, &target).await.unwrap());
        moved = session.clone();
        moved.id = "different-root".into();
        assert!(!crate::aliases::prior_target_allowed(&st.store, &moved, &target).await.unwrap());
        sqlx::query("UPDATE connections SET realms = '[\"other\"]' WHERE id = 'a'").execute(&st.store.pool).await.unwrap();
        assert!(for_session(st, &session, &pinned).await.is_err(), "alias realm revocation revokes the saved-prefix grant");
        sqlx::query("UPDATE connections SET realms = '[]' WHERE id = 'a'").execute(&st.store.pool).await.unwrap();
        sqlx::query("UPDATE connections SET realms = '[\"other\"]' WHERE id = 't1'").execute(&st.store.pool).await.unwrap();
        assert!(for_session(st, &session, &pinned).await.is_err(), "concrete account revocation is not bypassed");
        let revoked = crate::sessions::credential_for(
            st,
            Some(worker),
            "worker",
            c6_proto::CredentialRequest {
                connection_id: "t1".into(),
                session_id: "s".into(),
                supports_alias: true,
                refresh: true,
                ..Default::default()
            },
        )
        .await;
        assert!(!revoked.error.is_empty());
        assert!(revoked.bearer.is_empty());
        sqlx::query("UPDATE connections SET realms = '[]' WHERE id = 't1'").execute(&st.store.pool).await.unwrap();
        sqlx::query("UPDATE sessions SET connection_id = 't2' WHERE id = 's'").execute(&st.store.pool).await.unwrap();
        assert!(for_session(st, &session, &pinned).await.is_err(), "an alias no longer used by the tree is not a standing permission");
        assert!(for_session(st, &session, &serde_json::json!({"connection_id": "private", "model": "m"})).await.is_err());
        assert!(list(st, "private", Some("home")).await.is_err());
    }

    #[tokio::test]
    async fn direct_keep_warm_usage_preserves_only_its_own_validated_concrete_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("head.db")).await.unwrap();
        let worker = c6_core::WorkerId::new();
        for id in ["direct", "other"] {
            sqlx::query("INSERT INTO connections (id, name, kind, base_url, created_at) VALUES (?, ?, 'anthropic', '', 0)")
                .bind(id)
                .bind(format!("canonical {id}"))
                .execute(&store.pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, realm) VALUES ('s', ?, '', '', 'direct', 'm', 'idle', 0, 0, 'home')")
            .bind(worker.to_string()).execute(&store.pool).await.unwrap();
        for (seq, keep_warm, target, accepted) in
            [(1, true, "direct", true), (2, false, "direct", false), (3, true, "other", false), (4, true, "missing", false)]
        {
            let event = c6_proto::SessionEvent {
                seq,
                ts_ms: seq as i64,
                kind: Some(c6_proto::session_event::Kind::Usage(c6_proto::Usage {
                    model: "m".into(),
                    input_tokens: 100,
                    keep_warm,
                    resolved_connection_id: target.into(),
                    resolved_connection_name: "spoofed".into(),
                    alias_connection_name: "spoofed".into(),
                    ..Default::default()
                })),
            };
            let ingested = store.ingest(worker, "s", &[event]).await.unwrap().unwrap();
            let Some(c6_proto::session_event::Kind::Usage(usage)) = &ingested.events[0].kind else { panic!("expected usage") };
            assert_eq!(usage.resolved_connection_id, if accepted { "direct" } else { "" });
            assert_eq!(usage.resolved_connection_name, if accepted { "canonical direct" } else { "" });
            assert!(usage.alias_connection_name.is_empty());
        }
        store.pool.close().await;
    }
}
