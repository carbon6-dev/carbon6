//! Observational machine telemetry: a bounded mailbox and a separate database.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use c6_api::machines::*;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tokio::sync::mpsc;

use crate::{HeadState, bad_request};

const MAILBOX_CAPACITY: usize = 128;
pub(crate) const MAX_SAMPLE_BYTES: usize = 128 * 1024;
const MAX_QUERY_ROWS: usize = 16_384;
const MAX_QUERY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MachineConfig {
    pub sample_secs: u64,
    pub raw_retention_secs: u64,
    pub five_minute_retention_secs: u64,
    pub hourly_retention_secs: u64,
    pub disk_warn_percent: f64,
    pub disk_hold_percent: f64,
    pub memory_warn_percent: f64,
    pub memory_warn_samples: usize,
    pub maintenance_rows: usize,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            sample_secs: 15,
            raw_retention_secs: 24 * 3600,
            five_minute_retention_secs: 7 * 24 * 3600,
            hourly_retention_secs: 90 * 24 * 3600,
            disk_warn_percent: 10.0,
            disk_hold_percent: 5.0,
            memory_warn_percent: 5.0,
            memory_warn_samples: 3,
            maintenance_rows: 256,
        }
    }
}

#[derive(Default)]
struct Latest {
    sample: Option<MachineSample>,
    warnings: Vec<String>,
    low_memory: usize,
    oom: Option<u64>,
    active: HashSet<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedLatest {
    #[serde(flatten)]
    sample: MachineSample,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    low_memory: usize,
    #[serde(default)]
    oom: Option<u64>,
}

pub struct Machines {
    pool: SqlitePool,
    tx: mpsc::Sender<(String, MachineSample)>,
    rx: Mutex<Option<mpsc::Receiver<(String, MachineSample)>>>,
    latest: Mutex<HashMap<String, Latest>>,
    pub(crate) config: MachineConfig,
    started: Instant,
    pub(crate) disk_scan: Arc<tokio::sync::Semaphore>,
}

impl Machines {
    pub async fn open(path: &std::path::Path, config: MachineConfig) -> anyhow::Result<Self> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        anyhow::ensure!(
            config.raw_retention_secs > 0
                && config.raw_retention_secs <= config.five_minute_retention_secs
                && config.five_minute_retention_secs <= config.hourly_retention_secs,
            "machine retention must be positive and ordered raw <= five minute <= hourly"
        );
        anyhow::ensure!(
            (0.0..=100.0).contains(&config.disk_warn_percent)
                && (0.0..=100.0).contains(&config.disk_hold_percent)
                && (0.0..=100.0).contains(&config.memory_warn_percent),
            "machine warning percentages must be between 0 and 100"
        );
        anyhow::ensure!((1..=4096).contains(&config.maintenance_rows), "machine maintenance_rows must be between 1 and 4096");
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(options).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS machine_samples (machine TEXT NOT NULL, tier INTEGER NOT NULL, bucket INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(machine,tier,bucket))")
            .execute(&pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS machine_samples_tier_age ON machine_samples(tier,bucket)").execute(&pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS machine_samples_machine_age ON machine_samples(machine,bucket)").execute(&pool).await?;
        // Last observations survive a head restart, including offline workers.
        sqlx::query("CREATE TABLE IF NOT EXISTS machine_latest (machine TEXT PRIMARY KEY, data TEXT NOT NULL)").execute(&pool).await?;
        let mut latest = HashMap::new();
        for row in sqlx::query("SELECT machine,data FROM machine_latest").fetch_all(&pool).await? {
            if let Ok(saved) = serde_json::from_str::<SavedLatest>(&row.get::<String, _>("data")) {
                latest.insert(
                    row.get("machine"),
                    Latest {
                        oom: saved.oom.or(saved.sample.oom_kills),
                        sample: Some(saved.sample),
                        active: saved.warnings.iter().cloned().collect(),
                        warnings: saved.warnings,
                        low_memory: saved.low_memory,
                    },
                );
            }
        }
        let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
        Ok(Self {
            pool,
            tx,
            rx: Mutex::new(Some(rx)),
            latest: Mutex::new(latest),
            config,
            started: Instant::now(),
            disk_scan: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    /// Never await telemetry on the worker-link ingest/acknowledgement path.
    pub fn record(&self, machine: String, sample: MachineSample) {
        self.record_at(machine, sample, c6_core::now_ms());
    }

    fn record_at(&self, machine: String, mut sample: MachineSample, received_ms: i64) {
        // Telemetry is not replayed history. Reject ancient/far-future payloads,
        // and use head receipt time so a worker clock correction cannot freeze
        // latest/history. The small mailbox and payload caps also bound memory.
        if sample.at_ms.abs_diff(received_ms) > 5 * 60 * 1000
            || sample.sessions.len() > 512
            || sample.disks.len() > 128
            || sample.pressure.len() > 32
            || sample.sessions.iter().any(|s| s.session_id.len() > 128)
            || sample.disks.iter().any(|d| d.mount.len() > 1024 || d.filesystem.len() > 128)
            || sample.pressure.iter().any(|(k, v)| k.len() > 128 || v.len() > 4096)
            || !sample.cpu_percent.is_finite()
            || sample.load.iter().any(|n| !n.is_finite())
            || sample.sessions.iter().any(|s| !s.cpu_percent.is_finite())
        {
            return;
        }
        sample.at_ms = received_ms;
        if !matches!(serde_json::to_vec(&sample), Ok(json) if json.len() <= MAX_SAMPLE_BYTES) {
            return;
        }
        if self.tx.try_send((machine, sample)).is_err() {
            tracing::debug!("machine telemetry queue full or closed; dropping observation");
        }
    }

    fn observe(&self, machine: &str, sample: &MachineSample) -> Vec<String> {
        let mut all = self.latest.lock().unwrap();
        let state = all.entry(machine.to_owned()).or_default();
        if state.sample.as_ref().is_some_and(|old| old.at_ms == sample.at_ms) {
            return Vec::new();
        }
        if state.sample.as_ref().is_some_and(|old| {
            sample.at_ms < old.at_ms
                || sample.at_ms.saturating_sub(old.at_ms) > self.config.sample_secs.max(1).saturating_mul(2000).min(i64::MAX as u64) as i64
        }) {
            // An offline gap is not evidence of sustained memory pressure.
            state.low_memory = 0;
        }
        let mut warnings = Vec::new();
        for disk in &sample.disks {
            if disk.total > 0 && (disk.available as f64 / disk.total as f64) * 100.0 < self.config.disk_warn_percent {
                warnings.push(format!("Low disk space on {}", disk.mount));
            }
        }
        if sample.memory_total > 0
            && (sample.memory_available as f64 / sample.memory_total as f64) * 100.0 < self.config.memory_warn_percent
        {
            state.low_memory = state.low_memory.saturating_add(1);
        } else {
            state.low_memory = 0;
        }
        if state.low_memory >= self.config.memory_warn_samples.max(1) {
            warnings.push("Sustained low available memory".into());
        }
        let new_oom = matches!((state.oom, sample.oom_kills), (Some(old), Some(new)) if new > old);
        if new_oom {
            warnings.push("New OOM kill".into());
        }
        state.oom = sample.oom_kills.or(state.oom);
        let active: HashSet<_> = warnings.iter().cloned().collect();
        let newly_active =
            warnings.iter().filter(|w| !state.active.contains(*w) || (new_oom && w.as_str() == "New OOM kill")).cloned().collect();
        state.active = active;
        state.warnings = warnings;
        state.sample = Some(sample.clone());
        newly_active
    }

    pub async fn samples(&self, machine: &str, q: MachineSamplesQuery, now: i64) -> anyhow::Result<Vec<MachineSample>> {
        let to = q.to_ms.unwrap_or(now);
        let default_range = self.config.hourly_retention_secs.min(24 * 3600) as i64 * 1000;
        let from = q.from_ms.unwrap_or(to.saturating_sub(default_range));
        let max_range = self.config.hourly_retention_secs.min(366 * 24 * 3600).saturating_mul(1000) as i64;
        if from > to || to.saturating_sub(from) > max_range || q.limit == Some(0) {
            return Err(bad_request("invalid machine sample range or limit"));
        }
        let limit = q.limit.unwrap_or(480).min(2000);
        let mut rows = sqlx::query("SELECT data FROM machine_samples WHERE machine=? AND bucket>=? AND bucket<=? ORDER BY bucket LIMIT ?")
            .bind(machine)
            .bind(from.saturating_sub(3600 * 1000))
            .bind(to)
            .bind((MAX_QUERY_ROWS + 1) as i64)
            .fetch(&self.pool);
        let mut points = Vec::new();
        let mut bytes = 0;
        let mut count = 0;
        while let Some(row) = rows.try_next().await? {
            count += 1;
            let data: String = row.get("data");
            bytes += data.len();
            if count > MAX_QUERY_ROWS || bytes > MAX_QUERY_BYTES {
                return Err(bad_request("machine history exceeds the input budget; request a narrower time range"));
            }
            let p: MachineSample = serde_json::from_str(&data)?;
            if p.at_ms >= from && p.at_ms <= to {
                points.push(p);
            }
        }
        drop(rows);
        points.sort_by_key(|p| p.at_ms);
        // Bound the response without silently discarding the newest history.
        if points.len() <= limit {
            return Ok(points);
        }
        let width = to.saturating_sub(from).saturating_add(1).div_euclid(limit as i64).saturating_add(1);
        let mut buckets = BTreeMap::new();
        for point in points {
            let key = point.at_ms.saturating_sub(from).div_euclid(width);
            buckets.entry(key).and_modify(|old| merge_peak(old, &point)).or_insert(point);
        }
        Ok(buckets.into_values().collect())
    }

    async fn write_batch(&self, batch: &[(String, MachineSample)]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for (machine, sample) in batch {
            let bucket = sample.at_ms.div_euclid(15_000).saturating_mul(15_000);
            let old: Option<String> = sqlx::query_scalar("SELECT data FROM machine_samples WHERE machine=? AND tier=0 AND bucket=?")
                .bind(machine)
                .bind(bucket)
                .fetch_optional(&mut *tx)
                .await?;
            let mut point = sample.clone();
            if let Some(old) = old {
                let mut old = serde_json::from_str(&old)?;
                merge_peak(&mut old, sample);
                point = old;
            }
            sqlx::query("INSERT OR REPLACE INTO machine_samples VALUES (?,0,?,?)")
                .bind(machine)
                .bind(bucket)
                .bind(serde_json::to_string(&point)?)
                .execute(&mut *tx)
                .await?;
            // Observation order is checked in memory; persist the actual latest.
            let data = self
                .latest
                .lock()
                .unwrap()
                .get(machine)
                .and_then(|s| {
                    s.sample.as_ref().map(|sample| SavedLatest {
                        sample: sample.clone(),
                        warnings: s.warnings.clone(),
                        low_memory: s.low_memory,
                        oom: s.oom,
                    })
                })
                .map(|saved| serde_json::to_string(&saved))
                .transpose()?;
            if let Some(data) = data {
                sqlx::query("INSERT OR REPLACE INTO machine_latest VALUES (?,?)").bind(machine).bind(data).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn compact(&self, now: i64) -> anyhow::Result<usize> {
        let millis = |secs: u64| secs.saturating_mul(1000).min(i64::MAX as u64) as i64;
        let raw = now.saturating_sub(millis(self.config.raw_retention_secs));
        let five = now.saturating_sub(millis(self.config.five_minute_retention_secs));
        let hourly = now.saturating_sub(millis(self.config.hourly_retention_secs));
        let mut tx = self.pool.begin().await?;
        let mut remaining = self.config.maintenance_rows;
        let expired = sqlx::query("DELETE FROM machine_samples WHERE rowid IN (SELECT rowid FROM machine_samples WHERE tier IN (0,1,2) AND bucket < ? AND json_extract(data,'$.at_ms') < ? ORDER BY tier,bucket LIMIT ?)")
            .bind(hourly)
            .bind(hourly)
            .bind(remaining as i64)
            .execute(&mut *tx)
            .await?.rows_affected() as usize;
        remaining = remaining.saturating_sub(expired);
        // Move only old tiers. Existing destination points join the same
        // deterministic bucket, retaining resource peaks and low-space warnings.
        for (source, cutoff, target, width) in [(0_i64, raw, 1_i64, 300_000_i64), (1, five, 2, 3_600_000)] {
            if remaining == 0 {
                break;
            }
            let rows = sqlx::query("SELECT machine,bucket,data FROM machine_samples WHERE tier=? AND bucket<? AND json_extract(data,'$.at_ms')<? ORDER BY bucket LIMIT ?")
                .bind(source)
                .bind(cutoff)
                .bind(cutoff)
                .bind(remaining as i64)
                .fetch_all(&mut *tx)
                .await?;
            for row in rows {
                remaining -= 1;
                let machine: String = row.get("machine");
                let bucket: i64 = row.get("bucket");
                let next_bucket = bucket.div_euclid(width).saturating_mul(width);
                let mut point: MachineSample = serde_json::from_str(&row.get::<String, _>("data"))?;
                let existing: Option<String> =
                    sqlx::query_scalar("SELECT data FROM machine_samples WHERE machine=? AND tier=? AND bucket=?")
                        .bind(&machine)
                        .bind(target)
                        .bind(next_bucket)
                        .fetch_optional(&mut *tx)
                        .await?;
                if let Some(existing) = existing {
                    merge_peak(&mut point, &serde_json::from_str(&existing)?);
                }
                sqlx::query("INSERT OR REPLACE INTO machine_samples VALUES (?,?,?,?)")
                    .bind(&machine)
                    .bind(target)
                    .bind(next_bucket)
                    .bind(serde_json::to_string(&point)?)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM machine_samples WHERE machine=? AND tier=? AND bucket=?")
                    .bind(&machine)
                    .bind(source)
                    .bind(bucket)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(self.config.maintenance_rows - remaining)
    }
}

/// A bucket is a peak envelope, not an average or a fabricated simultaneous
/// observation. Its timestamp is the latest contributing observation.
fn merge_peak(old: &mut MachineSample, new: &MachineSample) {
    let newer = new.at_ms >= old.at_ms;
    old.at_ms = old.at_ms.max(new.at_ms);
    old.cpu_percent = old.cpu_percent.max(new.cpu_percent);
    old.cpu_count = old.cpu_count.max(new.cpu_count);
    for (a, b) in old.load.iter_mut().zip(new.load) {
        *a = a.max(b);
    }
    old.memory_used = old.memory_used.max(new.memory_used);
    if old.memory_total == 0 {
        old.memory_available = new.memory_available;
    } else if new.memory_total > 0 {
        old.memory_available = old.memory_available.min(new.memory_available);
    }
    old.memory_total = old.memory_total.max(new.memory_total);
    old.swap_used = old.swap_used.max(new.swap_used);
    old.swap_total = old.swap_total.max(new.swap_total);
    old.oom_kills = old.oom_kills.max(new.oom_kills);
    if newer {
        old.pressure = new.pressure.clone();
    }
    while old.pressure.len() > 32 {
        old.pressure.pop_last();
    }
    for disk in &new.disks {
        if let Some(prior) = old.disks.iter_mut().find(|d| d.mount == disk.mount) {
            prior.available = prior.available.min(disk.available);
            prior.used = prior.used.max(disk.used);
            prior.total = prior.total.max(disk.total);
            prior.inodes_used = prior.inodes_used.max(disk.inodes_used);
        } else {
            old.disks.push(disk.clone());
        }
    }
    old.disks.sort_by(|a, b| {
        let free = |d: &MachineDisk| if d.total == 0 { 1.0 } else { d.available as f64 / d.total as f64 };
        free(a).total_cmp(&free(b)).then(a.mount.cmp(&b.mount))
    });
    old.disks.truncate(128);
    for session in &new.sessions {
        if let Some(prior) = old.sessions.iter_mut().find(|s| s.session_id == session.session_id) {
            prior.cpu_percent = prior.cpu_percent.max(session.cpu_percent);
            prior.rss = prior.rss.max(session.rss);
        } else {
            old.sessions.push(session.clone());
        }
    }
    old.sessions.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent).then(b.rss.cmp(&a.rss)));
    old.sessions.truncate(512);
}

pub fn spawn(st: &Arc<HeadState>) {
    let Some(rx) = st.machines.rx.lock().unwrap().take() else { return };
    let mut sampler = None;
    spawn_tasks(st, st.shutdown.clone(), Some(Box::new(move || sampler.get_or_insert_with(c6_core::machines::Sampler::new).sample())), rx);
}

type SampleFn = Box<dyn FnMut() -> MachineSample + Send>;

// The lease owner is generic so lifecycle tests can isolate telemetry's
// reference from unrelated head background tasks without mocking the loops.
trait HeadRef: Send + Sync {
    fn head(&self) -> &HeadState;
}

impl HeadRef for HeadState {
    fn head(&self) -> &HeadState {
        self
    }
}

fn spawn_tasks<S: HeadRef + 'static>(
    st: &Arc<S>,
    shutdown: tokio_util::sync::CancellationToken,
    sampler: Option<SampleFn>,
    mut rx: mpsc::Receiver<(String, MachineSample)>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let weak = Arc::downgrade(st);
    let stop = shutdown.clone();
    let writer = tokio::spawn(async move {
        loop {
            let item = tokio::select! {
                _ = stop.cancelled() => break,
                item = rx.recv() => item,
            };
            let Some(item) = item else { break };
            let Some(lease) = weak.upgrade() else { break };
            let st = lease.head();
            let mut batch = vec![item];
            while batch.len() < 128 {
                match rx.try_recv() {
                    Ok(item) => batch.push(item),
                    Err(_) => break,
                }
            }
            for (id, sample) in &batch {
                let warnings = st.machines.observe(id, sample);
                if !warnings.is_empty() {
                    let name = if id == "head" {
                        "Head".to_owned()
                    } else if let Ok(worker) = id.parse::<c6_core::WorkerId>() {
                        st.store
                            .worker(worker)
                            .await
                            .ok()
                            .flatten()
                            .map(|worker| worker.name)
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| id.clone())
                    } else {
                        id.clone()
                    };
                    st.notifier.send(crate::notify::Note {
                        event: "machine",
                        title: format!("Machine {name} needs attention"),
                        message: warnings.join("; "),
                        priority: "high",
                        path: Some("/machines".into()),
                        ..Default::default()
                    });
                }
            }
            if let Err(e) = st.machines.write_batch(&batch).await {
                tracing::warn!("storing machine telemetry: {e:#}");
            }
        }
    });
    let weak = Arc::downgrade(st);
    let stop = shutdown.clone();
    let maintenance = tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! { _ = stop.cancelled() => break, _ = timer.tick() => {} }
            let Some(st) = weak.upgrade() else { break };
            if let Err(e) = st.head().machines.compact(c6_core::now_ms()).await {
                tracing::warn!("compacting machine telemetry: {e:#}");
            }
        }
    });
    let mut tasks = vec![writer, maintenance];
    let Some(mut sampler) = sampler else { return tasks };
    let weak = Arc::downgrade(st);
    let every = Duration::from_secs(st.head().machines.config.sample_secs.max(1));
    tasks.push(tokio::spawn(async move {
        let mut timer = tokio::time::interval(every);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = shutdown.cancelled() => break, _ = timer.tick() => {} }
            if weak.strong_count() == 0 {
                break;
            }
            // Initialization and probing run without a HeadState lease. Even
            // a blocked OS probe must not keep the head and its stores alive.
            match tokio::task::spawn_blocking(move || {
                let sample = sampler();
                (sampler, sample)
            })
            .await
            {
                Ok((next, sample)) => {
                    sampler = next;
                    if shutdown.is_cancelled() {
                        break;
                    }
                    let Some(st) = weak.upgrade() else { break };
                    st.head().machines.record("head".into(), sample);
                }
                Err(e) => {
                    tracing::warn!("sampling head machine: {e}");
                    break;
                }
            }
        }
    }));
    tasks
}

pub async fn inventory(st: &HeadState, caller: &crate::realms::Caller) -> anyhow::Result<Vec<Machine>> {
    let workers = crate::api::workers_snapshot(st).await?;
    let visible: Vec<_> =
        workers.into_iter().filter(|w| caller.realm.as_deref().is_none_or(|r| crate::realms::serves(&w.realms, r))).collect();
    let observations: HashMap<_, _> =
        st.machines.latest.lock().unwrap().iter().map(|(id, s)| (id.clone(), (s.sample.clone(), s.warnings.clone()))).collect();
    let observation = |id: &str| observations.get(id).cloned().unwrap_or_default();
    let (last_sample, warnings) = observation("head");
    let disks = last_sample.as_ref().map(|sample| sample.disks.clone()).unwrap_or_default();
    let (databases, data_disk, update) = if caller.realm.is_none() {
        let data = st.data_dir.clone();
        let knowledge = crate::knowledge::dir(st);
        let (databases, data_disk) =
            tokio::task::spawn_blocking(move || (database_sizes(&data, &knowledge), disk_for_path(&data, &disks))).await?;
        (databases, data_disk, serde_json::to_value(crate::updates::status(st).await?)?)
    } else {
        (Vec::new(), None, serde_json::Value::Null)
    };
    let mut result = vec![Machine {
        id: "head".into(),
        name: "Head".into(),
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        role: "head".into(),
        version: Some(st.updates.version.clone()),
        online: true,
        sample_secs: st.machines.config.sample_secs.max(1),
        last_sample,
        warnings,
        worker: None,
        head: caller.realm.is_none().then(|| HeadMachineInfo {
            uptime_secs: st.machines.started.elapsed().as_secs(),
            data_dir: st.data_dir.display().to_string(),
            data_disk,
            databases,
            connected_workers: visible.iter().filter(|w| w.status == c6_api::WorkerStatus::Online).count(),
            update,
        }),
    }];
    for worker in visible {
        let id = worker.id.to_string();
        let (last_sample, warnings) = observation(&id);
        result.push(Machine {
            id,
            name: worker.name.clone(),
            hostname: worker.hostname.clone(),
            role: "worker".into(),
            version: worker.version.clone(),
            online: worker.status == c6_api::WorkerStatus::Online,
            sample_secs: st.machines.config.sample_secs.max(1),
            last_sample,
            warnings,
            worker: Some(worker),
            head: None,
        });
    }
    if caller.realm.is_some() {
        let allowed = visible_sessions(st, caller).await?;
        for machine in &mut result {
            if let Some(sample) = &mut machine.last_sample {
                sample.sessions.retain(|s| allowed.contains(&s.session_id));
            }
        }
    }
    Ok(result)
}

fn database_sizes(data: &std::path::Path, knowledge: &std::path::Path) -> Vec<MachineDatabase> {
    let mut dirs = vec![data.to_path_buf(), knowledge.to_path_buf()];
    // The knowledge module owns the root path; each realm's actual databases
    // live one directory below its realms folder. Never open a realm merely
    // to report sizes, or follow arbitrary directory symlinks.
    if let Ok(entries) = std::fs::read_dir(knowledge.join("realms")) {
        dirs.extend(entries.filter_map(Result::ok).take(1000).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
    }
    let mut result = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let filename = entry.file_name().to_string_lossy().into_owned();
            if ![".db", ".sqlite", ".db-wal", ".db-shm", ".sqlite-wal", ".sqlite-shm"].iter().any(|suffix| filename.ends_with(suffix)) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                result.push(MachineDatabase { name: path.strip_prefix(data).unwrap_or(&path).display().to_string(), bytes: meta.len() });
            }
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

fn disk_for_path(path: &std::path::Path, disks: &[MachineDisk]) -> Option<MachineDisk> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // APFS firmlinks need device identity: /private may belong to
        // /System/Volumes/Data even though that is not a lexical ancestor.
        if let Ok(data) = std::fs::metadata(&canonical)
            && let Some(disk) = disks
                .iter()
                .filter(|disk| std::fs::metadata(&disk.mount).is_ok_and(|mount| mount.dev() == data.dev()))
                .max_by_key(|disk| (canonical.starts_with(&disk.mount), disk.mount.len()))
        {
            return Some(disk.clone());
        }
    }
    disks.iter().filter(|disk| canonical.starts_with(&disk.mount)).max_by_key(|disk| disk.mount.len()).cloned()
}

fn investigation_roots(data: &std::path::Path, home: Option<&std::path::Path>, temp: &std::path::Path) -> (Vec<std::path::PathBuf>, bool) {
    use std::path::{Path, PathBuf};
    fn add(roots: &mut Vec<PathBuf>, path: &Path, partial: &mut bool) {
        if roots.len() >= 64 {
            *partial = true;
            return;
        }
        // Canonicalize trusted roots only (/tmp is a macOS symlink); the
        // underlying scanner still never follows traversal symlinks.
        match path.canonicalize() {
            Ok(path) if path.is_dir() => {
                if roots.iter().any(|root| path.starts_with(root)) {
                    return;
                }
                if roots.iter().any(|root| root.starts_with(&path)) {
                    *partial = true;
                    return;
                }
                roots.push(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => *partial = true,
        }
    }
    fn children(roots: &mut Vec<PathBuf>, path: &Path, partial: &mut bool) {
        if path.canonicalize().is_ok_and(|path| roots.iter().any(|root| path.starts_with(root))) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            *partial = true;
            return;
        };
        let entries: Vec<_> = entries.take(129).collect();
        *partial |= entries.len() > 128;
        let mut paths = Vec::new();
        for entry in entries.into_iter().take(128) {
            match entry.and_then(|e| Ok((e.file_type()?, e.path()))) {
                Ok((kind, path)) if kind.is_dir() => paths.push(path),
                Ok(_) => {}
                Err(_) => *partial = true,
            }
        }
        paths.sort();
        for path in paths {
            add(roots, &path, partial);
        }
    }
    let mut roots = Vec::new();
    let mut partial = false;
    add(&mut roots, data, &mut partial);
    if let Some(home) = home {
        for path in [home.join("targets"), home.join(".cargo")] {
            add(&mut roots, &path, &mut partial);
        }
    }
    // Never let a broad home/temp ancestor erase the prioritized state root.
    add(&mut roots, temp, &mut partial);
    children(&mut roots, temp, &mut partial);
    if let Some(home) = home {
        children(&mut roots, home, &mut partial);
    }
    (roots, partial)
}

pub(crate) fn investigate_disk(data: &std::path::Path, home: Option<&std::path::Path>, temp: &std::path::Path) -> MachineDiskScan {
    let started = Instant::now();
    let (roots, partial) = investigation_roots(data, home, temp);
    scan_each(
        &roots,
        partial,
        c6_core::now_ms(),
        || started.elapsed(),
        |root, limits| c6_core::machines::scan_disk(&[root.to_path_buf()], limits),
    )
}

fn scan_each(
    roots: &[std::path::PathBuf],
    partial: bool,
    at_ms: i64,
    elapsed: impl Fn() -> Duration,
    mut scan: impl FnMut(&std::path::Path, c6_core::machines::DiskScanLimits) -> MachineDiskScan,
) -> MachineDiskScan {
    let limits = c6_core::machines::DiskScanLimits::default();
    let mut result = MachineDiskScan { at_ms, partial, ..Default::default() };
    let mut remaining = limits.max_entries;
    for (i, root) in roots.iter().enumerate() {
        let time = limits.max_duration.saturating_sub(elapsed());
        if remaining == 0 || time.is_zero() {
            result.partial = true;
            break;
        }
        let count = roots.len() - i;
        let share = c6_core::machines::DiskScanLimits {
            max_entries: (remaining / count).max(1),
            max_duration: time / count as u32,
            max_folders: limits.max_folders,
        };
        let part = scan(root, share);
        remaining = remaining.saturating_sub(part.entries);
        result.entries += part.entries;
        result.partial |= part.partial;
        result.folders.extend(part.folders);
    }
    result.folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    result.folders.truncate(limits.max_folders);
    result
}

async fn visible_sessions(st: &HeadState, caller: &crate::realms::Caller) -> anyhow::Result<HashSet<String>> {
    Ok(sqlx::query_scalar::<_, String>("SELECT id FROM sessions WHERE realm=?")
        .bind(caller.realm.as_deref())
        .fetch_all(st.store.pool())
        .await?
        .into_iter()
        .collect())
}

pub async fn filter_samples(st: &HeadState, caller: &crate::realms::Caller, samples: &mut [MachineSample]) -> anyhow::Result<()> {
    if caller.realm.is_none() {
        return Ok(());
    }
    let allowed = visible_sessions(st, caller).await?;
    for sample in samples {
        sample.sessions.retain(|s| allowed.contains(&s.session_id));
    }
    Ok(())
}

pub async fn authorize(st: &HeadState, caller: &crate::realms::Caller, id: &str) -> anyhow::Result<()> {
    if id == "head" {
        return Ok(());
    }
    let workers = crate::api::workers_snapshot(st).await?;
    if workers.iter().any(|w| w.id.to_string() == id && caller.realm.as_deref().is_none_or(|r| crate::realms::serves(&w.realms, r))) {
        Ok(())
    } else {
        Err(crate::not_found("machine not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_tasks_and_blocked_sampling_do_not_retain_head_state() {
        struct IsolatedHead(Arc<HeadState>);
        impl HeadRef for IsolatedHead {
            fn head(&self) -> &HeadState {
                &self.0
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: temp.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        // Other head tasks have independent lifetimes. Give the exact
        // production loops an isolated owner of one real HeadState reference,
        // so only telemetry can retain the lease measured below.
        let state = Arc::new(IsolatedHead(head.state.clone()));
        head.shutdown().await.unwrap();

        // Exercise the real writer/maintenance loops with sampling disabled
        // and a separate uncancelled lifetime. The fixture's shutdown token
        // must not mask a strong lease held by an idle telemetry task.
        let stop = tokio_util::sync::CancellationToken::new();
        let (_tx, rx) = mpsc::channel(1);
        let tasks = spawn_tasks(&state, stop.clone(), None, rx);
        tokio::task::yield_now().await;
        tokio::time::timeout(
            Duration::from_secs(10),
            c6_testkit::eventually("idle telemetry tasks to release their state", || async {
                (Arc::strong_count(&state) == 1).then_some(())
            }),
        )
        .await
        .expect("idle telemetry retained its head lease");
        stop.cancel();
        for task in tasks {
            c6_testkit::within("idle telemetry task to stop", task).await.unwrap();
        }

        // A probe can block in the OS: verify that even an in-flight blocking
        // callback owns no HeadState. Both synchronization points are explicit.
        let stop = tokio_util::sync::CancellationToken::new();
        let (_tx, rx) = mpsc::channel(1);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut entered = Some(entered_tx);
        let tasks = spawn_tasks(
            &state,
            stop.clone(),
            Some(Box::new(move || {
                if let Some(entered) = entered.take() {
                    let _ = entered.send(());
                }
                let _ = release_rx.recv();
                MachineSample::default()
            })),
            rx,
        );
        c6_testkit::within("the controlled sampler to block", entered_rx).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            c6_testkit::eventually("the blocked sampler to release its state", || async { (Arc::strong_count(&state) == 1).then_some(()) }),
        )
        .await
        .expect("the blocked sampler retained its head lease");
        let weak = Arc::downgrade(&state);
        drop(state);
        assert!(weak.upgrade().is_none(), "telemetry must release its owned head lease");
        release_tx.send(()).unwrap();
        stop.cancel();
        for task in tasks {
            c6_testkit::within("telemetry task to stop after head drop", task).await.unwrap();
        }
    }

    fn sample(at_ms: i64, cpu: f64, free: u64) -> MachineSample {
        MachineSample {
            at_ms,
            cpu_percent: cpu,
            cpu_count: 8,
            memory_total: 1000,
            memory_available: free,
            memory_used: 1000 - free,
            disks: vec![MachineDisk {
                mount: "/".into(),
                filesystem: "apfs".into(),
                total: 1000,
                available: free,
                used: 1000 - free,
                inodes_used: None,
            }],
            ..Default::default()
        }
    }

    async fn store() -> (tempfile::TempDir, Machines) {
        let temp = tempfile::tempdir().unwrap();
        let store = Machines::open(&temp.path().join("machines.sqlite"), Default::default()).await.unwrap();
        (temp, store)
    }

    #[tokio::test]
    async fn retention_uses_deterministic_peak_buckets_and_keeps_latest() {
        let (temp, store) = store().await;
        let day = 86_400_000;
        let now = 100 * day;
        let times = [
            (now - 91 * day, 5.0, 500),
            (now - 8 * day, 50.0, 500),
            (now - 8 * day + 15_000, 75.0, 10),
            (now - 2 * day, 90.0, 20),
            (now - 2 * day + 15_000, 10.0, 500),
            (now - 15_000, 3.0, 800),
        ];
        let batch: Vec<(String, MachineSample)> =
            times.into_iter().map(|(at, cpu, free)| ("worker".into(), sample(at, cpu, free))).collect();
        for (id, sample) in &batch {
            store.observe(id, sample);
        }
        store.write_batch(&batch).await.unwrap();
        store.compact(now).await.unwrap();
        let tiers: Vec<(i64, i64)> =
            sqlx::query_as("SELECT tier,count(*) FROM machine_samples GROUP BY tier ORDER BY tier").fetch_all(&store.pool).await.unwrap();
        assert_eq!(tiers, vec![(0, 1), (1, 1), (2, 1)]);
        let q = MachineSamplesQuery { from_ms: Some(now - 90 * day), to_ms: Some(now), limit: Some(100) };
        let points = store.samples("worker", q.clone(), now).await.unwrap();
        assert_eq!(points.len(), 3);
        assert_eq!((points[0].cpu_percent, points[0].memory_available), (75.0, 10));
        assert_eq!((points[1].cpu_percent, points[1].disks[0].available), (90.0, 20));
        store.compact(now).await.unwrap();
        assert_eq!(store.samples("worker", q, now).await.unwrap(), points);
        store.pool.close().await;
        let reopened = Machines::open(&temp.path().join("machines.sqlite"), Default::default()).await.unwrap();
        assert_eq!(reopened.latest.lock().unwrap()["worker"].sample.as_ref().unwrap().at_ms, now - 15_000);
    }

    #[tokio::test]
    async fn warnings_notify_only_new_episodes_and_each_new_oom() {
        let (_temp, store) = store().await;
        let mut p = sample(1, 1.0, 40);
        p.oom_kills = Some(10);
        assert_eq!(store.observe("worker", &p), vec!["Low disk space on /"]);
        p.at_ms += 1;
        assert!(store.observe("worker", &p).is_empty());
        p.at_ms += 1;
        assert_eq!(store.observe("worker", &p), vec!["Sustained low available memory"]);
        p.at_ms += 1;
        p.oom_kills = Some(11);
        assert_eq!(store.observe("worker", &p), vec!["New OOM kill"]);
        p.at_ms += 1;
        p.oom_kills = Some(12);
        assert_eq!(store.observe("worker", &p), vec!["New OOM kill"]);
        assert!(store.observe("worker", &p).is_empty(), "duplicate timestamps never advance episodes");
        let healthy = sample(100, 1.0, 500);
        assert!(store.observe("worker", &healthy).is_empty());
        p.at_ms = 101;
        assert_eq!(store.observe("worker", &p), vec!["Low disk space on /"]);
    }

    #[tokio::test]
    async fn warning_episodes_survive_a_head_restart() {
        let (temp, store) = store().await;
        let p = sample(1, 1.0, 40);
        assert_eq!(store.observe("worker", &p), vec!["Low disk space on /"]);
        store.write_batch(&[("worker".into(), p)]).await.unwrap();
        store.pool.close().await;
        let reopened = Machines::open(&temp.path().join("machines.sqlite"), Default::default()).await.unwrap();
        assert!(reopened.observe("worker", &sample(2, 1.0, 40)).is_empty());
        assert_eq!(reopened.latest.lock().unwrap()["worker"].warnings, vec!["Low disk space on /"]);
    }

    #[tokio::test]
    async fn query_bounds_and_output_preserve_peaks() {
        let (_temp, store) = store().await;
        let batch: Vec<(String, MachineSample)> =
            (0..100).map(|n| ("head".into(), sample(n * 15_000, n as f64, 1000 - n as u64))).collect();
        store.write_batch(&batch).await.unwrap();
        let q = MachineSamplesQuery { from_ms: Some(0), to_ms: Some(1_500_000), limit: Some(10) };
        let points = store.samples("head", q, 1_500_000).await.unwrap();
        assert!(points.len() <= 10);
        assert_eq!(points.last().unwrap().cpu_percent, 99.0);
        for q in [
            MachineSamplesQuery { from_ms: Some(10), to_ms: Some(0), limit: None },
            MachineSamplesQuery { limit: Some(0), ..Default::default() },
            MachineSamplesQuery { from_ms: Some(0), to_ms: Some(100 * 86_400_000), limit: None },
        ] {
            assert!(store.samples("head", q, 0).await.is_err());
        }
    }

    #[tokio::test]
    async fn bounded_mailbox_drops_without_waiting() {
        let (_temp, store) = store().await;
        for n in 0..2000 {
            store.record_at("head".into(), sample(n, 1.0, 500), n);
        }
        assert_eq!(store.rx.lock().unwrap().as_ref().unwrap().len(), MAILBOX_CAPACITY);
    }

    #[test]
    fn database_sizes_include_actual_knowledge_realms_and_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let knowledge = dir.path().join("knowledge");
        std::fs::create_dir_all(knowledge.join("realms/private")).unwrap();
        for name in ["c6h.db", "c6h.db-wal", "machines.sqlite", "knowledge/memex.db", "knowledge/realms/private/memex.db"] {
            std::fs::write(dir.path().join(name), "abc").unwrap();
        }
        let dbs = database_sizes(dir.path(), &knowledge);
        assert_eq!(dbs.len(), 5);
        assert!(dbs.iter().any(|d| d.name == "knowledge/realms/private/memex.db" && d.bytes == 3));
    }

    #[test]
    fn data_directory_disk_prefers_the_specific_filesystem_path() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("volume/data");
        std::fs::create_dir_all(&nested).unwrap();
        let root = dir.path().canonicalize().unwrap();
        let volume = root.join("volume");
        let disks = vec![
            MachineDisk { mount: root.display().to_string(), ..Default::default() },
            MachineDisk { mount: volume.display().to_string(), ..Default::default() },
        ];
        assert_eq!(disk_for_path(&nested, &disks).unwrap().mount, volume.display().to_string());
        assert!(disk_for_path(&nested, &[]).is_none());
    }

    #[test]
    fn catalog_and_agent_guards_distinguish_explicit_disk_investigation() {
        use crate::agent_ops::{Class, class};
        use crate::catalog::{self, Scope};
        for name in ["machines.list", "machines.samples"] {
            assert_eq!(catalog::scope(name), Some(Scope::Shared));
            assert_eq!(class(catalog::find(name).unwrap()), Class::Read);
        }
        assert_eq!(catalog::scope("machines.disk_scan"), Some(Scope::Admin));
        assert_eq!(class(catalog::find("machines.disk_scan").unwrap()), Class::Never);
        assert!(c6_api::DEFAULT_NOTIFY_EVENTS.contains(&"machine"));
    }

    #[tokio::test]
    async fn configured_retention_is_applied_and_validated() {
        let temp = tempfile::tempdir().unwrap();
        let config =
            MachineConfig { raw_retention_secs: 60, five_minute_retention_secs: 600, hourly_retention_secs: 3600, ..Default::default() };
        let store = Machines::open(&temp.path().join("configured.sqlite"), config).await.unwrap();
        let now = 10_000_000;
        let batch: Vec<(String, MachineSample)> =
            [15_000, 120_000, 900_000, 4_000_000].into_iter().map(|age| ("head".into(), sample(now - age, 1.0, 500))).collect();
        store.write_batch(&batch).await.unwrap();
        store.compact(now).await.unwrap();
        let points = store.samples("head", Default::default(), now).await.unwrap();
        assert_eq!(points.len(), 3);
        let bad = MachineConfig { raw_retention_secs: 700, five_minute_retention_secs: 600, ..Default::default() };
        assert!(Machines::open(&temp.path().join("invalid.sqlite"), bad).await.is_err());
    }

    #[tokio::test]
    async fn maintenance_backlog_is_processed_in_bounded_invocations() {
        let temp = tempfile::tempdir().unwrap();
        let config = MachineConfig { maintenance_rows: 2, ..Default::default() };
        let store = Machines::open(&temp.path().join("bounded.sqlite"), config).await.unwrap();
        let day = 86_400_000;
        let now = 100 * day;
        let batch: Vec<(String, MachineSample)> = (0..6)
            .map(|n| ("head".into(), sample(now - 91 * day + n * 15_000, 1.0, 500)))
            .chain((0..6).map(|n| ("head".into(), sample(now - 2 * day + n * 15_000, 1.0, 500))))
            .collect();
        store.write_batch(&batch).await.unwrap();
        assert_eq!(store.compact(now).await.unwrap(), 2);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM machine_samples").fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 10, "expiration must not delete the whole backlog at once");
        for _ in 0..20 {
            assert!(store.compact(now).await.unwrap() <= 2);
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM machine_samples").fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(store.compact(now).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn receipt_time_handles_worker_and_head_clock_corrections_and_caps_payloads() {
        let (_temp, store) = store().await;
        for (source, receipt) in [(10_000, 20_000), (5_000, 25_000), (250_000, 30_000)] {
            store.record_at("worker".into(), sample(source, receipt as f64, 500), receipt);
        }
        let mut rx = store.rx.lock().unwrap().take().unwrap();
        for expected in [20_000, 25_000, 30_000] {
            let (id, point) = rx.try_recv().unwrap();
            assert_eq!(point.at_ms, expected);
            store.observe(&id, &point);
        }
        store.observe("worker", &sample(15_000, 99.0, 500));
        assert_eq!(
            store.latest.lock().unwrap()["worker"].sample.as_ref().unwrap().cpu_percent,
            99.0,
            "backward receipt-clock correction must not freeze latest"
        );
        store.record_at("worker".into(), sample(0, 1.0, 500), 300_001);
        let mut too_many = sample(0, 1.0, 500);
        too_many.sessions = vec![MachineSessionUsage::default(); 513];
        store.record_at("worker".into(), too_many, 0);
        let mut too_large = sample(0, 1.0, 500);
        too_large.disks = vec![MachineDisk { mount: "m".repeat(1024), filesystem: "fs".into(), ..Default::default() }; 128];
        store.record_at("worker".into(), too_large, 0);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn history_refuses_an_excessive_input_byte_budget() {
        let (_temp, store) = store().await;
        let data = "x".repeat(MAX_QUERY_BYTES + 1);
        sqlx::query("INSERT INTO machine_samples VALUES ('head',0,0,?)").bind(data).execute(&store.pool).await.unwrap();
        let error =
            store.samples("head", MachineSamplesQuery { from_ms: Some(0), to_ms: Some(1000), limit: Some(1) }, 1000).await.unwrap_err();
        assert!(error.to_string().contains("input budget"));
    }

    #[test]
    fn investigation_preserves_prioritized_roots_and_divides_budgets() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let data = home.join(".local/share/c6h");
        let scratch = temp.path().join("scratch");
        for path in [&data, &scratch, &home.join(".cargo"), &home.join("targets"), &home.join("work")] {
            std::fs::create_dir_all(path).unwrap();
        }
        let (roots, partial) = investigation_roots(&data, Some(&home), &scratch);
        assert_eq!(roots[0], data.canonicalize().unwrap());
        assert!(partial, "the broad home ancestor containing state was intentionally skipped");
        for (i, root) in roots.iter().enumerate() {
            assert!(!roots.iter().skip(i + 1).any(|other| root.starts_with(other) || other.starts_with(root)));
        }
        let mut calls = Vec::new();
        let spent = std::cell::Cell::new(Duration::ZERO);
        let result = scan_each(
            &roots,
            partial,
            123,
            || spent.get(),
            |root, limits| {
                calls.push((root.to_owned(), limits.max_entries, limits.max_duration));
                spent.set(spent.get() + limits.max_duration);
                MachineDiskScan { entries: limits.max_entries, ..Default::default() }
            },
        );
        assert_eq!(calls.len(), roots.len());
        assert_eq!(result.entries, 100_000);
        assert!(calls.iter().all(|(_, entries, time)| *entries < 100_000 && *time < Duration::from_secs(5)));
        assert_eq!(result.at_ms, 123);
    }
}
