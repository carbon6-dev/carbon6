//! Usage, cost and budgets. Each model call's tokens (from `Usage` events)
//! go into the `usage` table as they are stored; costs come from your
//! per-model prices; budgets cap what sessions with given labels spend per
//! day, week or month.

use std::collections::{BTreeMap, HashMap};

use c6_api as api;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone};
use prost::Message;
use sqlx::Row;
use tracing::{info, warn};

use crate::store::SessionRow;
use crate::{HeadState, bad_request};

/// Settings keys.
pub const PRICES: &str = "prices";
pub const BUDGETS: &str = "budgets";
/// Budgets already used up: name → period key, so each tells you once.
const BUDGET_STATE: &str = "budget_state";
const BACKFILLED: &str = "usage_backfilled";

/// Records a model call (while its event is stored).
pub async fn record(tx: &mut sqlx::SqliteConnection, session_id: &str, seq: u64, ts: i64, u: &c6_proto::Usage) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO usage (session_id, seq, ts, connection_id, model, input, output, cache_read, cache_write) \
         SELECT ?, ?, ?, connection_id, CASE WHEN ? = '' THEN model ELSE ? END, ?, ?, ?, ? FROM sessions WHERE id = ?",
    )
    .bind(session_id)
    .bind(seq as i64)
    .bind(ts)
    .bind(&u.model)
    .bind(&u.model)
    .bind(u.input_tokens as i64)
    .bind(u.output_tokens as i64)
    .bind(u.cache_read_tokens as i64)
    .bind(u.cache_write_tokens as i64)
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Fills the table from events stored before it existed (once).
pub fn spawn_backfill(st: std::sync::Arc<HeadState>) {
    tokio::spawn(async move {
        if let Err(e) = backfill(&st).await {
            warn!("filling usage from earlier events: {e:#}");
        }
    });
}

async fn backfill(st: &HeadState) -> anyhow::Result<()> {
    if st.store.setting(BACKFILLED).await?.is_some() {
        return Ok(());
    }
    let (mut after_session, mut after_seq, mut n) = (String::new(), 0i64, 0u64);
    loop {
        let rows = sqlx::query(
            "SELECT session_id, seq, ts, data FROM events WHERE (session_id, seq) > (?, ?) ORDER BY session_id, seq LIMIT 2000",
        )
        .bind(&after_session)
        .bind(after_seq)
        .fetch_all(&st.store.pool)
        .await?;
        let Some(last) = rows.last() else { break };
        (after_session, after_seq) = (last.get("session_id"), last.get("seq"));
        let mut tx = st.store.pool.begin().await?;
        for r in &rows {
            let ev = c6_proto::SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?;
            if let Some(c6_proto::session_event::Kind::Usage(u)) = &ev.kind {
                record(&mut tx, &r.get::<String, _>("session_id"), ev.seq, r.get("ts"), u).await?;
                n += 1;
            }
        }
        tx.commit().await?;
    }
    st.store.set_setting(BACKFILLED, "1").await?;
    if n > 0 {
        info!(calls = n, "usage filled from earlier events");
    }
    Ok(())
}

pub async fn prices(st: &HeadState) -> anyhow::Result<Vec<api::Price>> {
    Ok(serde_json::from_str(&st.store.setting(PRICES).await?.unwrap_or_default()).unwrap_or_default())
}

pub async fn budgets(st: &HeadState) -> anyhow::Result<Vec<api::Budget>> {
    Ok(serde_json::from_str(&st.store.setting(BUDGETS).await?.unwrap_or_default()).unwrap_or_default())
}

pub fn check_prices(prices: &[api::Price]) -> anyhow::Result<()> {
    for p in prices {
        if p.model.trim().is_empty() {
            return Err(bad_request("a price needs a model"));
        }
        let all = [Some(p.input), Some(p.output), p.cache_read, p.cache_write];
        if all.iter().flatten().any(|v| !v.is_finite() || *v < 0.0) {
            return Err(bad_request(format!("{}: prices are USD per million tokens, 0 or more", p.model)));
        }
    }
    Ok(())
}

pub fn check_budgets(budgets: &[api::Budget]) -> anyhow::Result<()> {
    for b in budgets {
        let what = |m: &str| bad_request(format!("budget {:?}: {m}", b.name));
        if b.name.trim().is_empty() {
            return Err(bad_request("a budget needs a name"));
        }
        if !matches!(b.period.as_str(), "day" | "week" | "month") {
            return Err(what("the period is day, week or month"));
        }
        if !matches!(b.action.as_str(), "notify" | "block") {
            return Err(what("the action is notify or block"));
        }
        if !b.limit.is_finite() || b.limit <= 0.0 {
            return Err(what("the limit is an amount in USD above 0"));
        }
        crate::sessions::check_selector(&b.applies_to)?;
    }
    Ok(())
}

/// The price for a model: an exact entry, else the longest matching prefix.
fn price_for<'a>(prices: &'a [api::Price], model: &str) -> Option<&'a api::Price> {
    prices.iter().find(|p| p.model == model).or_else(|| {
        prices
            .iter()
            .filter_map(|p| p.model.strip_suffix('*').filter(|prefix| model.starts_with(prefix)).map(|prefix| (prefix.len(), p)))
            .max_by_key(|(n, _)| *n)
            .map(|(_, p)| p)
    })
}

/// A model call.
struct Call {
    session_id: String,
    ts: i64,
    connection_id: String,
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Call {
    fn cost(&self, prices: &[api::Price]) -> Option<f64> {
        let p = price_for(prices, &self.model)?;
        let per = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        Some(
            per(self.input, p.input)
                + per(self.output, p.output)
                + per(self.cache_read, p.cache_read.unwrap_or(p.input))
                + per(self.cache_write, p.cache_write.unwrap_or(p.input)),
        )
    }
}

fn add(row: &mut api::UsageRow, c: &Call, cost: Option<f64>) {
    row.input += c.input;
    row.output += c.output;
    row.cache_read += c.cache_read;
    row.cache_write += c.cache_write;
    row.calls += 1;
    match cost {
        Some(x) => row.cost += x,
        None => row.unpriced_calls += 1,
    }
}

async fn calls_since(st: &HeadState, since_ms: i64) -> anyhow::Result<Vec<Call>> {
    let rows = sqlx::query("SELECT * FROM usage WHERE ts >= ? ORDER BY ts").bind(since_ms).fetch_all(&st.store.pool).await?;
    Ok(rows
        .iter()
        .map(|r| Call {
            session_id: r.get("session_id"),
            ts: r.get("ts"),
            connection_id: r.get("connection_id"),
            model: r.get("model"),
            input: r.get::<i64, _>("input") as u64,
            output: r.get::<i64, _>("output") as u64,
            cache_read: r.get::<i64, _>("cache_read") as u64,
            cache_write: r.get::<i64, _>("cache_write") as u64,
        })
        .collect())
}

/// The sessions calls were made in, loaded once each.
#[derive(Default)]
struct Sessions(HashMap<String, Option<SessionRow>>);

impl Sessions {
    async fn get(&mut self, st: &HeadState, id: &str) -> Option<&SessionRow> {
        if !self.0.contains_key(id) {
            let row = st.store.session(id).await.ok().flatten();
            self.0.insert(id.to_owned(), row);
        }
        self.0.get(id).and_then(Option::as_ref)
    }
}

/// When the current period of a budget began.
fn period_start(period: &str, now: DateTime<Local>) -> DateTime<Local> {
    let today = now.date_naive();
    let day = match period {
        "week" => today - Duration::days(today.weekday().num_days_from_monday() as i64),
        "month" => today.with_day(1).unwrap_or(today),
        _ => today,
    };
    Local.from_local_datetime(&day.and_time(NaiveTime::MIN)).earliest().unwrap_or(now)
}

fn period_end(period: &str, start: DateTime<Local>) -> DateTime<Local> {
    match period {
        "week" => start + Duration::days(7),
        "month" => {
            let d = start.date_naive();
            let next = if d.month() == 12 { d.with_year(d.year() + 1).and_then(|x| x.with_month(1)) } else { d.with_month(d.month() + 1) };
            next.and_then(|n| Local.from_local_datetime(&n.and_time(NaiveTime::MIN)).earliest()).unwrap_or(start + Duration::days(31))
        }
        _ => start + Duration::days(1),
    }
}

/// What sessions a budget covers have spent this period.
async fn spent(st: &HeadState, b: &api::Budget, prices: &[api::Price], sessions: &mut Sessions) -> anyhow::Result<f64> {
    let start = period_start(&b.period, Local::now());
    let mut total = 0.0;
    for c in calls_since(st, start.timestamp_millis()).await? {
        let Some(s) = sessions.get(st, &c.session_id).await else { continue };
        if api::labels::applies(&b.applies_to, &s.all_labels()) {
            total += c.cost(prices).unwrap_or(0.0);
        }
    }
    Ok(total)
}

/// Why new work in a session with `labels` is refused: a blocking budget
/// that applies is used up.
pub async fn blocked(st: &HeadState, labels: &[String]) -> anyhow::Result<Option<String>> {
    let budgets = budgets(st).await?;
    let applicable: Vec<&api::Budget> =
        budgets.iter().filter(|b| b.enabled && b.action == "block" && api::labels::applies(&b.applies_to, labels)).collect();
    if applicable.is_empty() {
        return Ok(None);
    }
    let prices = prices(st).await?;
    let mut sessions = Sessions::default();
    for b in applicable {
        let spent = spent(st, b, &prices, &mut sessions).await?;
        if spent >= b.limit {
            let until = period_end(&b.period, period_start(&b.period, Local::now()));
            return Ok(Some(format!(
                "budget {:?} is used up (${spent:.2} of ${:.2} this {}); it resets {}",
                b.name,
                b.limit,
                b.period,
                until.format("%a %b %-d %H:%M")
            )));
        }
    }
    Ok(None)
}

/// After a session's model calls are stored: tells you (once a period)
/// about budgets they used up, and for blocking ones stops matching turns.
pub async fn after_calls(st: &HeadState, session_id: &str) {
    if let Err(e) = enforce(st, session_id).await {
        warn!(session = %session_id, "checking budgets: {e:#}");
    }
}

async fn enforce(st: &HeadState, session_id: &str) -> anyhow::Result<()> {
    let budgets = budgets(st).await?;
    if budgets.is_empty() {
        return Ok(());
    }
    let Some(session) = st.store.session(session_id).await? else { return Ok(()) };
    let labels = session.all_labels();
    let prices = prices(st).await?;
    let mut sessions = Sessions::default();
    for b in budgets.iter().filter(|b| b.enabled && api::labels::applies(&b.applies_to, &labels)) {
        let spent = spent(st, b, &prices, &mut sessions).await?;
        if spent < b.limit {
            continue;
        }
        let start = period_start(&b.period, Local::now());
        let key = format!("{}:{}", b.period, start.format("%Y-%m-%d"));
        let mut state: HashMap<String, String> =
            serde_json::from_str(&st.store.setting(BUDGET_STATE).await?.unwrap_or_default()).unwrap_or_default();
        if state.get(&b.name) == Some(&key) {
            continue;
        }
        state.insert(b.name.clone(), key);
        st.store.set_setting(BUDGET_STATE, &serde_json::to_string(&state)?).await?;
        let mut stopped = 0;
        if b.action == "block" {
            let running: Vec<SessionRow> =
                sqlx::query_as("SELECT * FROM sessions WHERE status = 'running'").fetch_all(&st.store.pool).await?;
            for s in running.iter().filter(|s| api::labels::applies(&b.applies_to, &s.all_labels())) {
                if crate::sessions::interrupt(st, &s.id).await.unwrap_or(false) {
                    stopped += 1;
                }
            }
        }
        let until = period_end(&b.period, start).format("%a %b %-d %H:%M");
        info!(budget = %b.name, spent, limit = b.limit, stopped, "budget used up");
        st.notifier.send(crate::notify::Note {
            event: "budget",
            title: format!("Budget {} used up", b.name),
            message: if b.action == "block" {
                format!(
                    "${spent:.2} of ${:.2} this {}. Stopped {stopped} running session(s); new work is refused until {until}.",
                    b.limit, b.period
                )
            } else {
                format!("${spent:.2} of ${:.2} this {} (resets {until}).", b.limit, b.period)
            },
            priority: "high",
            session_id: Some(session_id.to_owned()),
            labels: labels.clone(),
            ..Default::default()
        });
    }
    Ok(())
}

/// Labels worth a row in the report: yours and host:, worker:, repo:.
fn report_label(l: &str) -> bool {
    match l.split_once(':') {
        Some((k, _)) => matches!(k, "host" | "worker" | "repo") || !api::labels::AUTO_KEYS.contains(&k),
        None => true,
    }
}

/// `GET /api/v1/usage`: the last `days` days, for sessions with `filter`.
pub async fn report(st: &HeadState, days: u32, filter: &[String]) -> anyhow::Result<api::UsageReport> {
    let days = days.clamp(1, 366);
    let since = period_start("day", Local::now()) - Duration::days(days as i64 - 1);
    let prices = prices(st).await?;
    let connections: HashMap<String, String> = st.store.connections().await?.into_iter().map(|c| (c.id, c.name)).collect();
    let mut sessions = Sessions::default();
    let mut total = api::UsageRow { key: "total".into(), ..Default::default() };
    let (mut by_day, mut by_model, mut by_connection, mut by_label, mut by_session) =
        (BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    let mut unpriced = std::collections::BTreeSet::new();
    for c in calls_since(st, since.timestamp_millis()).await? {
        let Some(s) = sessions.get(st, &c.session_id).await.cloned() else { continue };
        let labels = s.all_labels();
        if !api::labels::applies(filter, &labels) {
            continue;
        }
        let cost = c.cost(&prices);
        if cost.is_none() {
            unpriced.insert(c.model.clone());
        }
        add(&mut total, &c, cost);
        let day = Local.timestamp_millis_opt(c.ts).single().map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
        let row = |m: &mut BTreeMap<String, api::UsageRow>, key: String, name: String| {
            add(m.entry(key.clone()).or_insert_with(|| api::UsageRow { key, name, ..Default::default() }), &c, cost);
        };
        row(&mut by_day, day.clone(), day);
        row(&mut by_model, c.model.clone(), c.model.clone());
        row(&mut by_connection, c.connection_id.clone(), connections.get(&c.connection_id).cloned().unwrap_or_else(|| "(deleted)".into()));
        for l in labels.iter().filter(|l| report_label(l)) {
            row(&mut by_label, l.clone(), l.clone());
        }
        // Subagents count toward the session that started them.
        let (root, title) = match &s.parent_id {
            Some(p) if s.parent_call_id.is_some() => {
                let title = sessions.get(st, p).await.map(|r| r.title.clone()).unwrap_or_default();
                (p.clone(), title)
            }
            _ => (s.id.clone(), s.title.clone()),
        };
        row(&mut by_session, root, title);
    }
    let by_cost = |m: BTreeMap<String, api::UsageRow>, n: usize| {
        let mut v: Vec<api::UsageRow> = m.into_values().collect();
        v.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.calls.cmp(&a.calls)));
        v.truncate(n);
        v
    };
    let mut budget_status = Vec::new();
    let mut spent_sessions = Sessions::default();
    for b in budgets(st).await? {
        let spent = spent(st, &b, &prices, &mut spent_sessions).await?;
        budget_status.push(api::BudgetStatus {
            blocking: b.enabled && b.action == "block" && spent >= b.limit,
            name: b.name,
            period: b.period,
            limit: b.limit,
            spent,
            action: if b.enabled { b.action } else { "off".into() },
        });
    }
    Ok(api::UsageReport {
        since_ms: since.timestamp_millis(),
        total,
        by_day: by_day.into_values().collect(),
        by_model: by_cost(by_model, 50),
        by_connection: by_cost(by_connection, 50),
        by_label: by_cost(by_label, 30),
        top_sessions: by_cost(by_session, 20),
        unpriced_models: unpriced.into_iter().collect(),
        budgets: budget_status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_and_periods() {
        let p = |m: &str, i: f64| api::Price { model: m.into(), input: i, output: i * 5.0, cache_read: None, cache_write: None };
        let prices = vec![p("claude-*", 1.0), p("claude-opus-*", 5.0), p("claude-opus-5-5", 6.0)];
        assert_eq!(price_for(&prices, "claude-opus-5-5").unwrap().input, 6.0);
        assert_eq!(price_for(&prices, "claude-opus-4-8").unwrap().input, 5.0, "longest prefix");
        assert_eq!(price_for(&prices, "claude-haiku-4-5").unwrap().input, 1.0);
        assert!(price_for(&prices, "qwen3.6:27b").is_none());
        let c = Call {
            session_id: String::new(),
            ts: 0,
            connection_id: String::new(),
            model: "claude-opus-4-8".into(),
            input: 1_000_000,
            output: 100_000,
            cache_read: 2_000_000,
            cache_write: 0,
        };
        assert!((c.cost(&prices).unwrap() - (5.0 + 2.5 + 10.0)).abs() < 1e-9);

        // 2026-10-14 is a Wednesday.
        let now = Local.with_ymd_and_hms(2026, 10, 14, 15, 30, 0).unwrap();
        assert_eq!(period_start("day", now), Local.with_ymd_and_hms(2026, 10, 14, 0, 0, 0).unwrap());
        assert_eq!(period_start("week", now), Local.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap());
        assert_eq!(period_start("month", now), Local.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap());
        let dec = Local.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        assert_eq!(period_end("month", dec), Local.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
    }
}
