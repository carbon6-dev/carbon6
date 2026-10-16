//! Series (docs/os.md §4.4): numbers over time, in a realm. Points come
//! from triggers (record a value), agents (`series_record`), the API and
//! carbon6 itself (host checks); a query sums, averages or counts them over
//! a window, by dimension or by hour or day; triggers watch series for
//! thresholds.
//!
//! Model usage is offered as series too (`model_spend`, `model_tokens`,
//! `model_calls`), computed from the usage ledger with today's prices.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use c6_core::now_ms;
use serde_json::{Map, Value, json};
use sqlx::Row;
use tracing::warn;

use crate::{HeadState, bad_request, not_found};

/// Series computed from the usage ledger: (name, unit, description).
pub const USAGE: [(&str, &str, &str); 3] = [
    ("model_spend", "USD", "What model calls cost, at the prices in Settings (unpriced models count 0)."),
    ("model_tokens", "tokens", "Tokens of model calls: input, output and cache."),
    ("model_calls", "calls", "Model calls."),
];

/// At most this many points are read for one query.
const MAX_POINTS: i64 = 200_000;

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') && !n.starts_with('_')
}

fn series_of(r: &sqlx::sqlite::SqliteRow) -> api::Series {
    api::Series {
        realm: r.get("realm"),
        name: r.get("name"),
        unit: r.get("unit"),
        description: r.get("description"),
        dimensions: serde_json::from_str(&r.get::<String, _>("dimensions")).unwrap_or_default(),
        retention_days: r.get::<i64, _>("retention_days").max(0) as u32,
        system: r.get::<i64, _>("system") != 0,
        computed: false,
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        points: r.try_get("n").unwrap_or(0),
        last: None,
        owner: r.try_get("owner").unwrap_or_default(),
    }
}

fn usage_series(realm: &str, (name, unit, description): (&str, &str, &str)) -> api::Series {
    api::Series {
        realm: realm.to_owned(),
        name: name.to_owned(),
        unit: unit.to_owned(),
        description: description.to_owned(),
        dimensions: vec!["model".into(), "connection".into()],
        retention_days: 0,
        system: true,
        computed: true,
        created_by: "system".into(),
        created_at: 0,
        points: 0,
        last: None,
        owner: String::new(),
    }
}

/// A realm's series, with the usage series.
pub async fn list(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::Series>> {
    let rows = sqlx::query(
        "SELECT s.*, (SELECT count(*) FROM points p WHERE p.realm = s.realm AND p.series = s.name) AS n FROM series s WHERE realm = ? ORDER BY name",
    )
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    let mut out: Vec<api::Series> = rows.iter().map(series_of).collect();
    for s in &mut out {
        s.last = last(st, realm, &s.name).await?;
    }
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM usage WHERE realm = ?").bind(realm).fetch_one(&st.store.pool).await?;
    out.extend(USAGE.iter().map(|u| api::Series { points: calls, ..usage_series(realm, *u) }));
    Ok(out)
}

async fn last(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<Option<api::Point>> {
    Ok(sqlx::query("SELECT ts, value, dims FROM points WHERE realm = ? AND series = ? ORDER BY ts DESC LIMIT 1")
        .bind(realm)
        .bind(name)
        .fetch_optional(&st.store.pool)
        .await?
        .map(|r| api::Point {
            ts: r.get("ts"),
            value: r.get("value"),
            dims: serde_json::from_str(&r.get::<String, _>("dims")).unwrap_or_default(),
        }))
}

pub async fn get(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<api::Series> {
    if let Some(u) = USAGE.iter().find(|u| u.0 == name) {
        return Ok(usage_series(realm, *u));
    }
    let row = sqlx::query(
        "SELECT s.*, (SELECT count(*) FROM points p WHERE p.realm = s.realm AND p.series = s.name) AS n FROM series s WHERE realm = ? AND name = ?",
    )
    .bind(realm)
    .bind(name)
    .fetch_optional(&st.store.pool)
    .await?;
    let mut s = row.as_ref().map(series_of).ok_or_else(|| not_found(format!("no series {name:?} in realm {realm}")))?;
    s.last = last(st, realm, name).await?;
    Ok(s)
}

fn check_dims(dims: &[String]) -> anyhow::Result<()> {
    match dims.iter().find(|d| !valid_name(d)) {
        Some(d) => Err(bad_request(format!("{d:?}: a dimension's name is lowercase letters, digits and _"))),
        None => Ok(()),
    }
}

pub async fn create(st: &HeadState, realm: &str, s: &api::SeriesIn, by: &str) -> anyhow::Result<api::Series> {
    create_owned(st, realm, s, by, "").await
}

/// A series an app instance owns (`owner`), or the realm (`""`): docs/apps.md §6.
pub async fn create_owned(st: &HeadState, realm: &str, s: &api::SeriesIn, by: &str, owner: &str) -> anyhow::Result<api::Series> {
    let name = s.name.trim();
    if !valid_name(name) {
        return Err(bad_request("a series' name is lowercase letters, digits and _ (up to 64)"));
    }
    if USAGE.iter().any(|u| u.0 == name) {
        return Err(bad_request(format!("{name} is carbon6's own series")));
    }
    if !crate::realms::exists(st, realm).await? || realm == crate::realms::ROOT {
        return Err(bad_request(format!("no realm {realm:?} that holds data")));
    }
    let dims = s.dimensions.clone().unwrap_or_default();
    check_dims(&dims)?;
    let done = sqlx::query(
        "INSERT OR IGNORE INTO series (realm, name, unit, description, dimensions, retention_days, created_by, created_at, owner) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(realm)
    .bind(name)
    .bind(s.unit.as_deref().unwrap_or("").trim())
    .bind(s.description.as_deref().unwrap_or("").trim())
    .bind(serde_json::to_string(&dims)?)
    .bind(s.retention_days.unwrap_or(0) as i64)
    .bind(by)
    .bind(now_ms())
    .bind(owner)
    .execute(&st.store.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(bad_request(format!("realm {realm} already has a series {name}")));
    }
    st.registry.bump();
    get(st, realm, name).await
}

pub async fn update(st: &HeadState, realm: &str, name: &str, s: &api::SeriesIn) -> anyhow::Result<api::Series> {
    let cur = get(st, realm, name).await?;
    if cur.system {
        return Err(bad_request(format!("{name} is carbon6's own series")));
    }
    let dims = s.dimensions.clone().unwrap_or(cur.dimensions);
    check_dims(&dims)?;
    sqlx::query("UPDATE series SET unit = ?, description = ?, dimensions = ?, retention_days = ? WHERE realm = ? AND name = ?")
        .bind(s.unit.clone().unwrap_or(cur.unit))
        .bind(s.description.clone().unwrap_or(cur.description))
        .bind(serde_json::to_string(&dims)?)
        .bind(s.retention_days.unwrap_or(cur.retention_days) as i64)
        .bind(realm)
        .bind(name)
        .execute(&st.store.pool)
        .await?;
    get(st, realm, name).await
}

/// Deletes a series and its points.
pub async fn delete(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<()> {
    if get(st, realm, name).await?.system {
        return Err(bad_request(format!("{name} is carbon6's own series")));
    }
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM points WHERE realm = ? AND series = ?").bind(realm).bind(name).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM series WHERE realm = ? AND name = ?").bind(realm).bind(name).execute(&mut *tx).await?;
    tx.commit().await?;
    st.registry.bump();
    Ok(())
}

/// Adds a point; the series' thresholds are checked at once.
pub async fn record(st: &HeadState, realm: &str, name: &str, p: &api::Point, source: &str) -> anyhow::Result<api::Point> {
    let s = get(st, realm, name).await?;
    if s.computed {
        return Err(bad_request(format!("{name} is computed from model usage")));
    }
    if !s.owner.is_empty() {
        let (instance, forced) = crate::collections::writer_of(st, source).await;
        if !forced && instance.as_deref() != Some(s.owner.as_str()) {
            return Err(bad_request(format!("{name} belongs to the app {}: its points come from the app", s.owner)));
        }
    }
    if !p.value.is_finite() {
        return Err(bad_request("a point's value is a number"));
    }
    if let Some(d) = p.dims.keys().find(|d| !s.dimensions.contains(d)) {
        return Err(bad_request(format!("{name} has no dimension {d:?} (it has: {})", s.dimensions.join(", "))));
    }
    let point = api::Point { ts: if p.ts > 0 { p.ts } else { now_ms() }, ..p.clone() };
    sqlx::query("INSERT INTO points (realm, series, ts, value, dims, source) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(realm)
        .bind(name)
        .bind(point.ts)
        .bind(point.value)
        .bind(Value::Object(point.dims.clone()).to_string())
        .bind(source)
        .execute(&st.store.pool)
        .await?;
    Box::pin(crate::triggers::watch_series(st, Some(realm), Some(name))).await;
    Ok(point)
}

/// The points of a window: (time, value, dimensions).
async fn points(
    st: &HeadState,
    realm: &str,
    name: &str,
    from: i64,
    to: i64,
    filter: &Map<String, Value>,
) -> anyhow::Result<Vec<(i64, f64, Map<String, Value>)>> {
    let raw = if USAGE.iter().any(|u| u.0 == name) {
        crate::usage::as_points(st, realm, name, from, to).await?
    } else {
        get(st, realm, name).await?;
        sqlx::query("SELECT ts, value, dims FROM points WHERE realm = ? AND series = ? AND ts >= ? AND ts < ? ORDER BY ts LIMIT ?")
            .bind(realm)
            .bind(name)
            .bind(from)
            .bind(to)
            .bind(MAX_POINTS)
            .fetch_all(&st.store.pool)
            .await?
            .iter()
            .map(|r| (r.get("ts"), r.get("value"), serde_json::from_str(&r.get::<String, _>("dims")).unwrap_or_default()))
            .collect()
    };
    Ok(raw.into_iter().filter(|(_, _, d)| filter.iter().all(|(k, v)| d.get(k) == Some(v))).collect())
}

fn aggregate(agg: &str, values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return (agg == "count" || agg == "sum").then_some(0.0);
    }
    Some(match agg {
        "avg" => values.iter().sum::<f64>() / values.len() as f64,
        "min" => values.iter().copied().fold(f64::INFINITY, f64::min),
        "max" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        "last" => *values.last().unwrap_or(&0.0),
        "count" => values.len() as f64,
        _ => values.iter().sum(),
    })
}

/// A summary of a series over a window.
pub async fn query(st: &HeadState, realm: &str, name: &str, q: &api::SeriesQuery) -> anyhow::Result<api::SeriesResult> {
    let agg = if q.agg.is_empty() { "sum" } else { q.agg.as_str() };
    if !matches!(agg, "sum" | "avg" | "min" | "max" | "last" | "count") {
        return Err(bad_request("agg is sum, avg, min, max, last or count"));
    }
    let to = q.to.unwrap_or_else(|| now_ms() + 1);
    let from = q.from.unwrap_or_else(|| to - q.minutes.unwrap_or(1440) as i64 * 60_000);
    let pts = points(st, realm, name, from, to, &q.filter).await?;
    let values: Vec<f64> = pts.iter().map(|p| p.1).collect();
    let mut out = api::SeriesResult {
        series: name.to_owned(),
        agg: agg.to_owned(),
        from,
        to,
        value: aggregate(agg, &values),
        points: pts.len() as i64,
        ..Default::default()
    };
    if !q.group_by.is_empty() {
        let mut groups: BTreeMap<String, (Value, Vec<f64>)> = BTreeMap::new();
        for (_, v, d) in &pts {
            let key = d.get(&q.group_by).cloned().unwrap_or(Value::Null);
            groups.entry(key.to_string()).or_insert_with(|| (key, Vec::new())).1.push(*v);
        }
        out.groups = groups.into_values().filter_map(|(k, vs)| Some((k, aggregate(agg, &vs)?))).collect();
        out.groups.sort_by(|a, b| b.1.total_cmp(&a.1));
    }
    if !q.bucket.is_empty() {
        let size: i64 = match q.bucket.as_str() {
            "hour" => 3_600_000,
            "day" => 86_400_000,
            _ => return Err(bad_request("bucket is hour or day")),
        };
        let mut buckets: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
        for (ts, v, _) in &pts {
            buckets.entry(ts - ts.rem_euclid(size)).or_default().push(*v);
        }
        out.buckets = buckets.into_iter().filter_map(|(t, vs)| Some((t, aggregate(agg, &vs)?))).collect();
    }
    Ok(out)
}

/// The latest point's time (for "stopped arriving").
pub async fn latest_ts(st: &HeadState, realm: &str, name: &str, filter: &Map<String, Value>) -> anyhow::Result<Option<i64>> {
    if filter.is_empty() && !USAGE.iter().any(|u| u.0 == name) {
        return Ok(sqlx::query_scalar("SELECT max(ts) FROM points WHERE realm = ? AND series = ?")
            .bind(realm)
            .bind(name)
            .fetch_one(&st.store.pool)
            .await?);
    }
    let pts = points(st, realm, name, 0, now_ms() + 1, filter).await?;
    Ok(pts.last().map(|p| p.0))
}

/// An agent's call, in its session's realm.
pub async fn for_session(st: &HeadState, realm: &str, session_id: &str, what: &str, p: &Value) -> anyhow::Result<Value> {
    let name = p["series"].as_str().unwrap_or_default();
    match what {
        "series_record" => {
            let point = api::Point {
                ts: p["ts"].as_i64().unwrap_or(0),
                value: p["value"].as_f64().ok_or_else(|| bad_request("value is a number"))?,
                dims: p["dims"].as_object().cloned().unwrap_or_default(),
            };
            Ok(json!(record(st, realm, name, &point, &format!("session:{session_id}")).await?))
        }
        "series_query" if name.is_empty() => Ok(json!(list(st, realm).await?)),
        "series_query" => {
            let q: api::SeriesQuery = serde_json::from_value(p["query"].clone()).unwrap_or_default();
            Ok(json!(query(st, realm, name, &q).await?))
        }
        other => Err(bad_request(format!("unknown series call {other}"))),
    }
}

/// Every minute: thresholds (including "stopped arriving"), and points
/// past their series' retention.
pub fn spawn_sweeper(st: Arc<HeadState>) {
    tokio::spawn(async move {
        let mut ticks = 0u64;
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            crate::triggers::watch_series(&st, None, None).await;
            ticks += 1;
            if ticks % 60 == 1 {
                let r = sqlx::query(
                    "DELETE FROM points WHERE rowid IN (SELECT p.rowid FROM points p JOIN series s ON s.realm = p.realm AND s.name = p.series \
                     WHERE s.retention_days > 0 AND p.ts < ? - s.retention_days * 86400000)",
                )
                .bind(now_ms())
                .execute(&st.store.pool)
                .await;
                if let Err(e) = r {
                    warn!("dropping old points: {e:#}");
                }
            }
        }
    });
}
