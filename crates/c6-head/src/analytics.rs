//! Analytics (docs/analytics.md): who worked when, what it cost, and how
//! much it took of you. Each event that says so leaves a timing mark in the
//! `activity` table as it is stored; the report walks the marks of the
//! range per session, and adds the model calls of the `usage` table.

use std::collections::{BTreeMap, HashMap, HashSet};

use c6_api as api;
use c6_proto::session_event::Kind;
use chrono::{Duration, Local, TimeZone};
use prost::Message;
use sqlx::Row;
use tracing::{info, warn};

use crate::HeadState;
use crate::usage::Sessions;

const BACKFILLED: &str = "activity_backfilled";

/// Records an event's timing mark, if it has one (while it is stored).
pub async fn mark(tx: &mut sqlx::SqliteConnection, session_id: &str, ev: &c6_proto::SessionEvent) -> anyhow::Result<()> {
    let (kind, name, ms, failed) = match &ev.kind {
        Some(Kind::User(_)) => ("user", String::new(), 0, false),
        Some(Kind::TurnStarted(_)) => ("turn_start", String::new(), 0, false),
        Some(Kind::TurnEnded(t)) => {
            let reason = match c6_proto::TurnEndReason::try_from(t.reason) {
                Ok(c6_proto::TurnEndReason::Completed) => "completed",
                Ok(c6_proto::TurnEndReason::Interrupted) => "interrupted",
                Ok(c6_proto::TurnEndReason::WorkerRestarted) => "worker_restarted",
                _ => "error",
            };
            ("turn_end", reason.to_owned(), 0, false)
        }
        Some(Kind::Usage(u)) => ("model", u.model.clone(), 0, false),
        Some(Kind::ToolResult(r)) => ("tool", r.tool.clone(), r.duration_ms, r.is_error),
        Some(Kind::Approval(a)) => ("approval", a.state.clone(), 0, false),
        Some(Kind::Question(q)) => ("question", q.state.clone(), 0, false),
        Some(Kind::Rewound(_)) => ("rewind", String::new(), 0, false),
        _ => return Ok(()),
    };
    sqlx::query("INSERT OR IGNORE INTO activity (session_id, seq, ts, kind, name, ms, failed) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(session_id)
        .bind(ev.seq as i64)
        .bind(ev.ts_ms)
        .bind(kind)
        .bind(name)
        .bind(ms as i64)
        .bind(failed)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Marks the events stored before the table existed (once).
pub fn spawn_backfill(st: std::sync::Arc<HeadState>) {
    tokio::spawn(async move {
        if let Err(e) = backfill(&st).await {
            warn!("filling activity from earlier events: {e:#}");
        }
    });
}

async fn backfill(st: &HeadState) -> anyhow::Result<()> {
    if st.store.setting(BACKFILLED).await?.is_some() {
        return Ok(());
    }
    let (mut after_session, mut after_seq, mut n) = (String::new(), 0i64, 0u64);
    loop {
        let rows =
            sqlx::query("SELECT session_id, seq, data FROM events WHERE (session_id, seq) > (?, ?) ORDER BY session_id, seq LIMIT 2000")
                .bind(&after_session)
                .bind(after_seq)
                .fetch_all(&st.store.pool)
                .await?;
        let Some(last) = rows.last() else { break };
        (after_session, after_seq) = (last.get("session_id"), last.get("seq"));
        let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
        for r in &rows {
            let ev = c6_proto::SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?;
            mark(&mut tx, &r.get::<String, _>("session_id"), &ev).await?;
            n += 1;
        }
        tx.commit().await?;
    }
    st.store.set_setting(BACKFILLED, "1").await?;
    if n > 0 {
        info!(events = n, "activity filled from earlier events");
    }
    Ok(())
}

struct Mark {
    session_id: String,
    seq: i64,
    ts: i64,
    kind: String,
    name: String,
    ms: u64,
    failed: bool,
}

/// One row of the report, with the sessions counted once.
#[derive(Default)]
struct Acc {
    row: api::ActivityRow,
    sessions: HashSet<String>,
}

impl Acc {
    fn new(key: &str) -> Self {
        Acc { row: api::ActivityRow { key: key.to_owned(), ..Default::default() }, sessions: HashSet::new() }
    }

    fn done(mut self) -> api::ActivityRow {
        self.row.sessions = self.sessions.len() as u64;
        self.row
    }
}

fn day_of(ts: i64) -> String {
    Local.timestamp_millis_opt(ts).single().map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn percentile(v: &mut [u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// The most intervals open at once.
fn peak(spans: &[(i64, i64)]) -> u64 {
    let mut edges: Vec<(i64, i64)> = spans.iter().flat_map(|&(a, b)| [(a, 1), (b.max(a), -1)]).collect();
    // An end before a start at the same instant: back-to-back isn't overlap.
    edges.sort();
    let (mut now, mut most) = (0i64, 0i64);
    for (_, d) in edges {
        now += d;
        most = most.max(now);
    }
    most as u64
}

/// `GET /api/v1/analytics`: the last `days` days, for sessions with
/// `filter`, grouped by the label key `by`.
pub async fn report(st: &HeadState, days: u32, filter: &[String], by: &str) -> anyhow::Result<api::AnalyticsReport> {
    let days = days.clamp(1, 366);
    let since = crate::usage::period_start("day", Local::now()) - Duration::days(days as i64 - 1);
    let since_ms = since.timestamp_millis();
    let by = if by.trim().is_empty() { "repo" } else { by.trim().trim_end_matches(':') };
    let prices = crate::usage::prices(st).await?;
    let mut sessions = Sessions::default();
    // Whether a session counts, and its group.
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    let prefix = format!("{by}:");
    let mut group_of = async |sessions: &mut Sessions, id: &str| -> Option<String> {
        if let Some(g) = seen.get(id) {
            return g.clone();
        }
        let g = sessions.get(st, id).await.and_then(|s| {
            let labels = s.all_labels();
            api::labels::applies(filter, &labels).then(|| {
                api::labels::display_labels(&labels).iter().find_map(|l| l.strip_prefix(&prefix).map(str::to_owned)).unwrap_or_default()
            })
        });
        seen.insert(id.to_owned(), g.clone());
        g
    };

    let mut total = Acc::new("total");
    let mut by_day: BTreeMap<String, Acc> = BTreeMap::new();
    let mut d = since.date_naive();
    while d <= Local::now().date_naive() {
        let k = d.format("%Y-%m-%d").to_string();
        by_day.insert(k.clone(), Acc::new(&k));
        d += Duration::days(1);
    }
    let mut by_model: BTreeMap<String, Acc> = BTreeMap::new();
    let mut by_group: BTreeMap<String, Acc> = BTreeMap::new();
    let mut tools: BTreeMap<String, (api::ToolRow, Vec<u64>)> = BTreeMap::new();
    let mut spans: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    let (mut replies, mut answers) = (Vec::new(), Vec::new());
    let mut unpriced = std::collections::BTreeSet::new();

    // Adds to the total, the day and the group.
    macro_rules! each {
        ($day:expr, $group:expr, $sid:expr, |$r:ident| $body:expr) => {{
            let day = $day;
            for acc in [Some(&mut total), by_day.get_mut(&day), Some(by_group.entry($group.clone()).or_insert_with(|| Acc::new(&$group)))]
                .into_iter()
                .flatten()
            {
                acc.sessions.insert($sid.to_owned());
                let $r = &mut acc.row;
                $body;
            }
        }};
    }

    for c in crate::usage::calls_since(st, since_ms).await? {
        let Some(group) = group_of(&mut sessions, &c.session_id).await else { continue };
        let cost = c.cost(&prices);
        if cost.is_none() {
            unpriced.insert(c.model.clone());
        }
        let add = |r: &mut api::ActivityRow| {
            r.calls += 1;
            r.input += c.input;
            r.output += c.output;
            r.cache_read += c.cache_read;
            r.cost += cost.unwrap_or(0.0);
        };
        each!(day_of(c.ts), group, c.session_id, |r| add(r));
        let m = by_model.entry(c.model.clone()).or_insert_with(|| Acc::new(&c.model));
        m.sessions.insert(c.session_id.clone());
        add(&mut m.row);
    }

    // Each session's first message starts its task.
    let firsts: HashMap<String, i64> =
        sqlx::query("SELECT session_id, MIN(seq) AS seq FROM activity WHERE kind = 'user' GROUP BY session_id")
            .fetch_all(&st.store.pool)
            .await?
            .iter()
            .map(|r| (r.get("session_id"), r.get("seq")))
            .collect();
    // A day before the range too, for what was already under way.
    let marks: Vec<Mark> = sqlx::query("SELECT * FROM activity WHERE ts >= ? ORDER BY session_id, seq")
        .bind(since_ms - 86_400_000)
        .fetch_all(&st.store.pool)
        .await?
        .iter()
        .map(|r| Mark {
            session_id: r.get("session_id"),
            seq: r.get("seq"),
            ts: r.get("ts"),
            kind: r.get("kind"),
            name: r.get("name"),
            ms: r.get::<i64, _>("ms").max(0) as u64,
            failed: r.get("failed"),
        })
        .collect();
    let mut i = 0;
    while i < marks.len() {
        let sid = marks[i].session_id.clone();
        let end = marks[i..].iter().position(|m| m.session_id != sid).map_or(marks.len(), |n| i + n);
        let session = &marks[i..end];
        i = end;
        let Some(group) = group_of(&mut sessions, &sid).await else { continue };
        let (mut prev, mut turn_start, mut waiting, mut asked) = (None::<i64>, None::<i64>, None::<i64>, None::<i64>);
        // The turn's model calls (model, ms) and tool time, counted when it ends.
        let (mut calls, mut tool_ms): (Vec<(String, u64)>, u64) = (Vec::new(), 0);
        for m in session {
            let counted = m.ts >= since_ms;
            let day = day_of(m.ts);
            match m.kind.as_str() {
                "user" => {
                    if counted {
                        let first = firsts.get(&sid) == Some(&m.seq);
                        each!(day.clone(), group, sid, |r| {
                            r.messages += 1;
                            if first { r.tasks += 1 } else { r.steers += 1 }
                        });
                        if let Some(w) = waiting {
                            replies.push((m.ts - w).max(0) as u64);
                        }
                    }
                    waiting = None;
                }
                "turn_start" => {
                    turn_start = Some(m.ts);
                    (calls, tool_ms) = (Vec::new(), 0);
                }
                "turn_end" => {
                    if let Some(s) = turn_start.take()
                        && counted
                    {
                        let ms = (m.ts - s).max(0) as u64;
                        let (failed, interrupted) = (m.name == "error", m.name == "interrupted");
                        let model_ms: u64 = calls.iter().map(|c| c.1).sum();
                        for (model, ms) in calls.drain(..) {
                            let acc = by_model.entry(model.clone()).or_insert_with(|| Acc::new(&model));
                            acc.sessions.insert(sid.clone());
                            acc.row.model_ms += ms;
                        }
                        each!(day.clone(), group, sid, |r| {
                            r.turns += 1;
                            r.agent_ms += ms;
                            r.model_ms += model_ms;
                            r.tool_ms += tool_ms;
                            r.turns_failed += failed as u64;
                            r.turns_interrupted += interrupted as u64;
                            r.steers += interrupted as u64;
                        });
                        spans.entry(day_of(s)).or_default().push((s, m.ts));
                    }
                    // A finished turn waits for your reply.
                    waiting = (m.name == "completed").then_some(m.ts);
                }
                "model" => {
                    if let (Some(p), true) = (prev, turn_start.is_some()) {
                        calls.push((m.name.clone(), (m.ts - p).max(0) as u64));
                    }
                }
                "tool" if counted => {
                    tool_ms += m.ms;
                    let (row, durations) = tools
                        .entry(m.name.clone())
                        .or_insert_with(|| (api::ToolRow { key: m.name.clone(), ..Default::default() }, Vec::new()));
                    row.calls += 1;
                    row.failed += m.failed as u64;
                    row.ms += m.ms;
                    durations.push(m.ms);
                }
                "approval" | "question" => match m.name.as_str() {
                    "requested" | "asked" => asked = Some(m.ts),
                    state => {
                        if let Some(a) = asked.take()
                            && counted
                            && matches!(state, "allowed" | "denied" | "answered")
                        {
                            answers.push((m.ts - a).max(0) as u64);
                            each!(day.clone(), group, sid, |r| r.answers += 1);
                        }
                    }
                },
                "rewind" if counted => each!(day.clone(), group, sid, |r| r.steers += 1),
                _ => {}
            }
            prev = Some(m.ts);
        }
    }

    let by_day: Vec<api::ActivityRow> = by_day
        .into_values()
        .map(|a| {
            let mut r = a.done();
            r.peak_agents = spans.get(&r.key).map_or(0, |s| peak(s));
            r
        })
        .collect();
    let mut total = total.done();
    total.peak_agents = by_day.iter().map(|d| d.peak_agents).max().unwrap_or(0);
    let ranked = |m: BTreeMap<String, Acc>, n: usize| {
        let mut v: Vec<api::ActivityRow> = m.into_values().map(Acc::done).collect();
        v.sort_by(|a, b| {
            b.agent_ms.max(b.model_ms).cmp(&a.agent_ms.max(a.model_ms)).then(b.cost.total_cmp(&a.cost)).then(b.calls.cmp(&a.calls))
        });
        v.truncate(n);
        v
    };
    let mut by_tool: Vec<api::ToolRow> = tools
        .into_values()
        .map(|(mut r, mut d)| {
            r.median_ms = percentile(&mut d, 0.5);
            r
        })
        .collect();
    by_tool.sort_by(|a, b| b.ms.cmp(&a.ms).then(b.calls.cmp(&a.calls)));
    by_tool.truncate(30);
    let waits = api::Waits {
        replies: replies.len() as u64,
        reply_median_ms: percentile(&mut replies, 0.5),
        reply_p90_ms: percentile(&mut replies, 0.9),
        answers: answers.len() as u64,
        answer_median_ms: percentile(&mut answers, 0.5),
        answer_p90_ms: percentile(&mut answers, 0.9),
    };
    Ok(api::AnalyticsReport {
        since_ms,
        by: by.to_owned(),
        total,
        by_day,
        by_model: ranked(by_model, 30),
        by_tool,
        by_group: ranked(by_group, 30),
        waits,
        unpriced_models: unpriced.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_and_percentiles() {
        assert_eq!(peak(&[]), 0);
        assert_eq!(peak(&[(0, 10), (5, 15), (12, 20)]), 2);
        assert_eq!(peak(&[(0, 10), (10, 20)]), 1, "back to back");
        assert_eq!(peak(&[(0, 10), (1, 9), (2, 8)]), 3);
        assert_eq!(percentile(&mut [], 0.5), 0);
        assert_eq!(percentile(&mut [5, 1, 3], 0.5), 3);
        assert_eq!(percentile(&mut [1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.9), 9);
    }
}
