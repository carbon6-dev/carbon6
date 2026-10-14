//! Upgrading a SQLite database to this build's schema, safely
//! (docs/os.md §13).
//!
//! Before any pending migration runs, the database is backed up next to
//! itself (`backups/`). Migrations then run; if one fails, or a check finds
//! that data did not survive, the backup is restored and the error says so,
//! so the database is exactly as it was and the previous build still runs
//! on it. Every table must keep at least as many rows as it had, unless a
//! pending migration says it moves or drops that table. Each upgrade leaves
//! a report in the database's `upgrades` table.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Connection, Row};
use tracing::{info, warn};

/// A comparison between the database before and after an upgrade, run
/// right after the upgrade applies migration `version`. `before` runs with
/// the backup (the database as it was before the whole upgrade) attached
/// as `before` (write `before.table`); `after` runs on the database as
/// migration `version` left it. Each returns one value, and they must be
/// equal.
#[derive(Clone, Copy, Debug)]
pub struct Check {
    pub version: i64,
    pub name: &'static str,
    pub before: &'static str,
    pub after: &'static str,
}

/// Migration `version` moves or drops `table`: it may lose rows (or
/// vanish) in that upgrade. Everything else must keep its rows.
#[derive(Clone, Copy, Debug)]
pub struct Allow {
    pub version: i64,
    pub table: &'static str,
}

/// What a database's upgrades check and allow.
#[derive(Clone, Copy, Debug, Default)]
pub struct Plan {
    pub checks: &'static [Check],
    pub allow: &'static [Allow],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableCount {
    pub table: String,
    pub before: i64,
    /// `None`: the table is gone.
    pub after: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub before: String,
    pub after: String,
    pub ok: bool,
}

/// What an upgrade did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub at: i64,
    pub database: String,
    /// The newest migration applied before (`None`: a new database).
    pub from_version: Option<i64>,
    pub to_version: i64,
    /// (version, description) of each migration applied.
    pub applied: Vec<(i64, String)>,
    pub backup: Option<String>,
    pub tables: Vec<TableCount>,
    pub checks: Vec<CheckResult>,
}

impl Report {
    /// The report as a person reads it.
    pub fn text(&self) -> String {
        let mut out = format!("{}: ", self.database);
        match self.from_version {
            None => out.push_str(&format!("new database at version {}\n", self.to_version)),
            Some(f) if self.applied.is_empty() => out.push_str(&format!("up to date at version {f}\n")),
            Some(f) => out.push_str(&format!("version {f} → {}\n", self.to_version)),
        }
        for (v, d) in &self.applied {
            out.push_str(&format!("  applied {v:04} {d}\n"));
        }
        if let Some(b) = &self.backup {
            out.push_str(&format!("  backup: {b}\n"));
        }
        let changed: Vec<&TableCount> = self.tables.iter().filter(|t| t.after != Some(t.before)).collect();
        if !self.applied.is_empty() {
            out.push_str(&format!("  tables: {} unchanged", self.tables.len() - changed.len()));
            out.push_str(if changed.is_empty() { "\n" } else { ", changed:\n" });
            for t in changed {
                match t.after {
                    Some(a) => out.push_str(&format!("    {}: {} → {a} rows\n", t.table, t.before)),
                    None => out.push_str(&format!("    {}: {} rows, moved or dropped\n", t.table, t.before)),
                }
            }
        }
        let short = |s: &str| if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s.to_owned() };
        for c in &self.checks {
            out.push_str(&format!(
                "  check {}: {} ({} before, {} after)\n",
                c.name,
                if c.ok { "ok" } else { "FAILED" },
                short(&c.before),
                short(&c.after)
            ));
        }
        out
    }
}

/// Opens the database at `path`, upgrading it to `migrator`'s newest
/// migration (see the module docs). The report is `None` when nothing
/// needed doing.
pub async fn open(
    path: &Path,
    opts: SqliteConnectOptions,
    pool_opts: SqlitePoolOptions,
    migrator: &Migrator,
    plan: &Plan,
) -> anyhow::Result<(SqlitePool, Option<Report>)> {
    let existed = path.exists();
    let pool = pool_opts.clone().connect_with(opts.clone()).await.with_context(|| format!("opening {}", path.display()))?;
    let applied = applied_versions(&pool).await?;
    let pending: Vec<(i64, String)> = migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration() && !applied.contains(&m.version))
        .map(|m| (m.version, m.description.to_string()))
        .collect();
    if pending.is_empty() {
        return Ok((pool, None));
    }
    let to_version = pending.iter().map(|(v, _)| *v).max().unwrap_or_default();
    let database = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    // A new database: nothing to keep.
    if !existed || applied.is_empty() {
        migrator.run(&pool).await.with_context(|| format!("creating {}", path.display()))?;
        let report = Report {
            at: crate::now_ms(),
            database,
            from_version: None,
            to_version,
            applied: pending,
            backup: None,
            tables: Vec::new(),
            checks: Vec::new(),
        };
        save_report(&pool, &report).await?;
        return Ok((pool, Some(report)));
    }

    let from_version = applied.iter().copied().max();
    let backup = backup_path(path, from_version.unwrap_or_default(), to_version);
    std::fs::create_dir_all(backup.parent().unwrap_or(Path::new(".")))?;
    sqlx::query("VACUUM INTO ?")
        .bind(backup.to_string_lossy().as_ref())
        .execute(&pool)
        .await
        .with_context(|| format!("backing up {} to {}", path.display(), backup.display()))?;
    info!(database = %database, from = ?from_version, to = to_version, backup = %backup.display(), "upgrading");
    let before = counts(&pool).await?;

    let outcome = async {
        // One version at a time, each checked right after it is applied
        // (while the tables its checks read still exist).
        let mut pending_versions: Vec<i64> = pending.iter().map(|(v, _)| *v).collect();
        pending_versions.sort();
        let mut checks = Vec::new();
        for v in &pending_versions {
            migrator.run_to(*v, &pool).await.with_context(|| format!("migration {v:04} failed"))?;
            checks.extend(run_checks(&pool, &backup, plan, &[*v]).await?);
        }
        let after = counts(&pool).await?;
        let allowed = |t: &str| plan.allow.iter().any(|a| a.table == t && pending_versions.contains(&a.version));
        let mut tables = Vec::new();
        let mut lost = Vec::new();
        for (t, b) in &before {
            let a = after.iter().find(|(n, _)| n == t).map(|(_, c)| *c);
            if a.is_none_or(|a| a < *b) && !allowed(t) {
                lost.push(match a {
                    Some(a) => format!("{t} ({b} → {a} rows)"),
                    None => format!("{t} ({b} rows, dropped)"),
                });
            }
            tables.push(TableCount { table: t.clone(), before: *b, after: a });
        }
        anyhow::ensure!(lost.is_empty(), "rows were lost: {}", lost.join(", "));
        let failed: Vec<&CheckResult> = checks.iter().filter(|c| !c.ok).collect();
        anyhow::ensure!(
            failed.is_empty(),
            "checks failed: {}",
            failed.iter().map(|c| format!("{} ({} before, {} after)", c.name, c.before, c.after)).collect::<Vec<_>>().join(", ")
        );
        let report = Report {
            at: crate::now_ms(),
            database: database.clone(),
            from_version,
            to_version,
            applied: pending.clone(),
            backup: Some(backup.to_string_lossy().into_owned()),
            tables,
            checks,
        };
        save_report(&pool, &report).await?;
        anyhow::Ok(report)
    }
    .await;

    match outcome {
        Ok(report) => {
            info!(database = %database, from = ?report.from_version, to = report.to_version, backup = ?report.backup, "upgraded");
            Ok((pool, Some(report)))
        }
        Err(e) => {
            pool.close().await;
            restore(path, &backup).with_context(|| {
                format!("the upgrade of {} failed ({e:#}) and restoring its backup {} failed too", path.display(), backup.display())
            })?;
            warn!(database = %database, error = %format!("{e:#}"), "upgrade failed; the backup was restored");
            Err(e.context(format!(
                "upgrading {} from version {} to {to_version} failed; it was restored from {}, unchanged",
                path.display(),
                from_version.unwrap_or_default(),
                backup.display()
            )))
        }
    }
}

/// Runs the upgrade on a copy of the database and returns what it would do,
/// leaving the database untouched. The copy is removed afterwards.
pub async fn check(path: &Path, opts: SqliteConnectOptions, migrator: &Migrator, plan: &Plan) -> anyhow::Result<Report> {
    anyhow::ensure!(path.exists(), "no database at {}", path.display());
    let dir = std::env::temp_dir().join(format!("c6-upgrade-check-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(path.file_name().unwrap_or_default());
    let result = async {
        {
            let source = SqliteConnectOptions::from_str("sqlite:")?.filename(path).read_only(true);
            let mut c = sqlx::SqliteConnection::connect_with(&source).await.with_context(|| format!("opening {}", path.display()))?;
            sqlx::query("VACUUM INTO ?").bind(copy.to_string_lossy().as_ref()).execute(&mut c).await.context("copying the database")?;
            c.close().await?;
        }
        let (pool, report) = open(&copy, opts.filename(&copy), SqlitePoolOptions::new().max_connections(2), migrator, plan).await?;
        pool.close().await;
        let applied = applied_on(&copy).await.unwrap_or_default();
        Ok(report.unwrap_or_else(|| Report {
            at: crate::now_ms(),
            database: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            from_version: applied.iter().copied().max(),
            to_version: applied.iter().copied().max().unwrap_or_default(),
            applied: Vec::new(),
            backup: None,
            tables: Vec::new(),
            checks: Vec::new(),
        }))
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    result.map(|mut r: Report| {
        // The copy's paths mean nothing to the reader.
        r.backup = r.backup.map(|_| "(made on upgrade, in backups/ next to the database)".to_owned());
        r.database = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        r
    })
}

/// Earlier upgrades' reports, newest first.
pub async fn reports(pool: &SqlitePool) -> anyhow::Result<Vec<Report>> {
    let exists: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'upgrades'").fetch_optional(pool).await?;
    if exists.is_none() {
        return Ok(Vec::new());
    }
    let rows: Vec<String> = sqlx::query_scalar("SELECT report FROM upgrades ORDER BY id DESC").fetch_all(pool).await?;
    Ok(rows.iter().filter_map(|r| serde_json::from_str(r).ok()).collect())
}

async fn applied_versions(pool: &SqlitePool) -> anyhow::Result<Vec<i64>> {
    let exists: Option<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'")
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = 1").fetch_all(pool).await?)
}

async fn applied_on(path: &Path) -> anyhow::Result<Vec<i64>> {
    let opts = SqliteConnectOptions::from_str("sqlite:")?.filename(path).read_only(true);
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
    let v = applied_versions(&pool).await;
    pool.close().await;
    v
}

/// Row counts of the database's own tables (not SQLite's, the migration
/// log's, the upgrade reports', or full-text indexes' internal tables).
async fn counts(pool: &SqlitePool) -> anyhow::Result<Vec<(String, i64)>> {
    let rows = sqlx::query("SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name").fetch_all(pool).await?;
    let virtuals: Vec<String> = rows
        .iter()
        .filter(|r| r.get::<Option<String>, _>("sql").is_some_and(|s| s.to_uppercase().starts_with("CREATE VIRTUAL TABLE")))
        .map(|r| r.get::<String, _>("name"))
        .collect();
    let mut out = Vec::new();
    for r in rows {
        let name: String = r.get("name");
        let internal = name.starts_with("sqlite_")
            || name == "_sqlx_migrations"
            || name == "upgrades"
            || virtuals.contains(&name)
            || virtuals.iter().any(|v| name.starts_with(&format!("{v}_")));
        if internal {
            continue;
        }
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\""))))
            .fetch_one(pool)
            .await?;
        out.push((name, n));
    }
    Ok(out)
}

async fn run_checks(pool: &SqlitePool, backup: &Path, plan: &Plan, pending: &[i64]) -> anyhow::Result<Vec<CheckResult>> {
    let due: Vec<&Check> = plan.checks.iter().filter(|c| pending.contains(&c.version)).collect();
    if due.is_empty() {
        return Ok(Vec::new());
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("ATTACH DATABASE ? AS before").bind(backup.to_string_lossy().as_ref()).execute(&mut *conn).await?;
    let mut out = Vec::new();
    let mut failure = None;
    for c in due {
        // A table the old database didn't have held nothing to move: the
        // check has nothing to compare.
        if let Err(e) = scalar(&mut conn, c.before).await
            && e.to_string().contains("no such table: before.")
        {
            out.push(CheckResult {
                name: c.name.to_owned(),
                ok: true,
                before: "(not in the old database)".into(),
                after: "(nothing to check)".into(),
            });
            continue;
        }
        let result = async {
            let b = scalar(&mut conn, c.before).await.with_context(|| format!("check {} (before)", c.name))?;
            let a = scalar(&mut conn, c.after).await.with_context(|| format!("check {} (after)", c.name))?;
            anyhow::Ok((b, a))
        }
        .await;
        match result {
            Ok((b, a)) => out.push(CheckResult { name: c.name.to_owned(), ok: a == b, before: b, after: a }),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    sqlx::query("DETACH DATABASE before").execute(&mut *conn).await?;
    match failure {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// A query's single value, as text. The SQL comes from a `Check`, written in
/// this codebase.
async fn scalar(conn: &mut sqlx::SqliteConnection, sql: &str) -> anyhow::Result<String> {
    let v: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT CAST(({sql}) AS TEXT)"))).fetch_one(&mut *conn).await?;
    Ok(v.unwrap_or_else(|| "NULL".to_owned()))
}

async fn save_report(pool: &SqlitePool, report: &Report) -> anyhow::Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS upgrades (id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, report TEXT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO upgrades (at, report) VALUES (?, ?)")
        .bind(report.at)
        .bind(serde_json::to_string(report)?)
        .execute(pool)
        .await?;
    Ok(())
}

fn backup_path(path: &Path, from: i64, to: i64) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new(".")).join("backups");
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "db".into());
    let base = format!("{stem}-v{from:04}-to-v{to:04}-{}", utc_stamp(crate::now_ms()));
    // A second attempt in the same second (after a failed one) gets its own.
    let mut candidate = dir.join(format!("{base}.db"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{base}-{n}.db"));
        n += 1;
    }
    candidate
}

/// `YYYYMMDD-HHMMSS` in UTC.
fn utc_stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Puts the backup back in place of the database (and its WAL files).
fn restore(path: &Path, backup: &Path) -> anyhow::Result<()> {
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            std::fs::remove_file(&side)?;
        }
    }
    std::fs::copy(backup, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A migrator from (version, description, sql) files written to `dir`.
    async fn migrator(dir: &Path, migrations: &[(i64, &str, &str)]) -> Migrator {
        let src = dir.join("migrations");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(&src).unwrap();
        for (v, d, sql) in migrations {
            std::fs::write(src.join(format!("{v:04}_{d}.sql")), sql).unwrap();
        }
        Migrator::new(src).await.unwrap()
    }

    fn opts(path: &Path) -> SqliteConnectOptions {
        SqliteConnectOptions::from_str("sqlite:").unwrap().filename(path).create_if_missing(true)
    }

    async fn open_db(path: &Path, m: &Migrator, plan: &Plan) -> anyhow::Result<(SqlitePool, Option<Report>)> {
        open(path, opts(path), SqlitePoolOptions::new().max_connections(2), m, plan).await
    }

    const V1: (i64, &str, &str) = (1, "init", "CREATE TABLE notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL);");

    /// A database at version 1 with three notes.
    async fn at_v1(dir: &Path) -> PathBuf {
        let path = dir.join("app.db");
        let m = migrator(dir, &[V1]).await;
        let (pool, report) = open_db(&path, &m, &Plan::default()).await.unwrap();
        assert_eq!(report.unwrap().from_version, None);
        for t in ["a", "b", "c"] {
            sqlx::query("INSERT INTO notes (text) VALUES (?)").bind(t).execute(&pool).await.unwrap();
        }
        pool.close().await;
        path
    }

    async fn notes(path: &Path) -> Vec<String> {
        let pool = SqlitePoolOptions::new().connect_with(opts(path)).await.unwrap();
        let v = sqlx::query_scalar("SELECT text FROM notes ORDER BY id").fetch_all(&pool).await.unwrap();
        pool.close().await;
        v
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir.join("backups")).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default()
    }

    #[tokio::test]
    async fn upgrades_with_a_backup_and_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = at_v1(dir.path()).await;
        let m = migrator(
            dir.path(),
            &[
                V1,
                (2, "tags", "CREATE TABLE tags (note INTEGER, tag TEXT); ALTER TABLE notes ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;"),
            ],
        )
        .await;
        // Nothing pending at first: no report.
        let (pool, report) = open_db(&path, &migrator(dir.path(), &[V1]).await, &Plan::default()).await.unwrap();
        assert!(report.is_none());
        pool.close().await;

        let (pool, report) = open_db(&path, &m, &Plan::default()).await.unwrap();
        let r = report.unwrap();
        assert_eq!((r.from_version, r.to_version), (Some(1), 2));
        assert_eq!(r.applied, vec![(2, "tags".to_owned())]);
        assert!(r.tables.contains(&TableCount { table: "notes".into(), before: 3, after: Some(3) }), "{r:?}");
        let b = backups(dir.path());
        assert_eq!(b.len(), 1);
        assert!(b[0].file_name().unwrap().to_string_lossy().starts_with("app-v0001-to-v0002-"), "{b:?}");
        assert_eq!(r.backup.as_deref(), Some(b[0].to_string_lossy().as_ref()));
        // The backup is the database as it was.
        assert_eq!(notes(&b[0]).await, vec!["a", "b", "c"]);
        // Reports are kept, newest first.
        let all = reports(&pool).await.unwrap();
        assert_eq!(all.iter().map(|r| r.to_version).collect::<Vec<_>>(), vec![2, 1]);
        assert!(r.text().contains("version 1 → 2") && r.text().contains("applied 0002 tags"), "{}", r.text());
        pool.close().await;
    }

    #[tokio::test]
    async fn a_failed_migration_restores_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = at_v1(dir.path()).await;
        // 2 succeeds and changes data; 3 fails: the database must be as it was before 2.
        let m = migrator(
            dir.path(),
            &[V1, (2, "rename", "UPDATE notes SET text = 'changed';"), (3, "broken", "ALTER TABLE nowhere ADD COLUMN x;")],
        )
        .await;
        let e = open_db(&path, &m, &Plan::default()).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("from version 1 to 3 failed") && msg.contains("restored") && msg.contains("nowhere"), "{msg}");
        assert_eq!(notes(&path).await, vec!["a", "b", "c"]);
        // Still at version 1, so the previous build runs on it.
        assert_eq!(applied_on(&path).await.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn lost_rows_fail_unless_a_migration_allows_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = at_v1(dir.path()).await;
        let m = migrator(dir.path(), &[V1, (2, "trim", "DELETE FROM notes WHERE text = 'b';")]).await;
        let e = open_db(&path, &m, &Plan::default()).await.unwrap_err();
        assert!(format!("{e:#}").contains("rows were lost: notes (3 → 2 rows)"), "{e:#}");
        assert_eq!(notes(&path).await.len(), 3);

        static ALLOW: [Allow; 1] = [Allow { version: 2, table: "notes" }];
        let (pool, report) = open_db(&path, &m, &Plan { checks: &[], allow: &ALLOW }).await.unwrap();
        assert!(report.unwrap().tables.contains(&TableCount { table: "notes".into(), before: 3, after: Some(2) }));
        pool.close().await;
    }

    #[tokio::test]
    async fn checks_compare_before_and_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = at_v1(dir.path()).await;
        // Notes move into a new table; the check proves every text arrived.
        let m = migrator(
            dir.path(),
            &[
                V1,
                (
                    2,
                    "move",
                    "CREATE TABLE records (body TEXT); INSERT INTO records SELECT text FROM notes WHERE text != 'c'; DROP TABLE notes;",
                ),
            ],
        )
        .await;
        static ALLOW: [Allow; 1] = [Allow { version: 2, table: "notes" }];
        static CHECK: [Check; 1] = [Check {
            version: 2,
            name: "every note moved",
            before: "SELECT group_concat(text) FROM (SELECT text FROM before.notes ORDER BY text)",
            after: "SELECT group_concat(body) FROM (SELECT body FROM records ORDER BY body)",
        }];
        let e = open_db(&path, &m, &Plan { checks: &CHECK, allow: &ALLOW }).await.unwrap_err();
        assert!(format!("{e:#}").contains("checks failed: every note moved (a,b,c before, a,b after)"), "{e:#}");
        assert_eq!(notes(&path).await.len(), 3, "restored");

        let m = migrator(
            dir.path(),
            &[V1, (2, "move", "CREATE TABLE records (body TEXT); INSERT INTO records SELECT text FROM notes; DROP TABLE notes;")],
        )
        .await;
        let (pool, report) = open_db(&path, &m, &Plan { checks: &CHECK, allow: &ALLOW }).await.unwrap();
        let r = report.unwrap();
        assert_eq!(
            r.checks,
            vec![CheckResult { name: "every note moved".into(), before: "a,b,c".into(), after: "a,b,c".into(), ok: true }]
        );
        assert!(r.tables.contains(&TableCount { table: "notes".into(), before: 3, after: None }));
        pool.close().await;
    }

    #[tokio::test]
    async fn a_check_on_a_table_the_old_database_lacked_does_not_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = at_v1(dir.path()).await;
        let m = migrator(dir.path(), &[V1, (2, "new", "CREATE TABLE extra (x TEXT);")]).await;
        static CHECK: [Check; 1] = [Check {
            version: 2,
            name: "extras kept",
            before: "SELECT count(*) FROM before.extras_v0",
            after: "SELECT count(*) FROM extra",
        }];
        let (pool, report) = open_db(&path, &m, &Plan { checks: &CHECK, allow: &[] }).await.unwrap();
        let r = report.unwrap();
        assert!(r.checks[0].ok && r.checks[0].before.contains("not in the old database"), "{:?}", r.checks);
        pool.close().await;
    }

    #[tokio::test]
    async fn each_version_is_checked_before_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let path = at_v1(dir.path()).await;
        // 3 renames the table 2's check reads: it must run between them.
        let m = migrator(
            dir.path(),
            &[
                V1,
                (2, "pin", "ALTER TABLE notes ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;"),
                (3, "retire", "ALTER TABLE notes RENAME TO legacy_notes;"),
            ],
        )
        .await;
        static ALLOW: [Allow; 1] = [Allow { version: 3, table: "notes" }];
        static CHECK: [Check; 1] = [Check {
            version: 2,
            name: "notes kept, none pinned",
            before: "SELECT count(*) FROM before.notes",
            after: "SELECT count(*) FROM notes WHERE pinned = 0",
        }];
        let (pool, report) = open_db(&path, &m, &Plan { checks: &CHECK, allow: &ALLOW }).await.unwrap();
        let r = report.unwrap();
        assert_eq!(r.applied.len(), 2);
        assert_eq!(r.checks, vec![CheckResult { name: "notes kept, none pinned".into(), before: "3".into(), after: "3".into(), ok: true }]);
        pool.close().await;
    }

    #[tokio::test]
    async fn check_leaves_the_database_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = at_v1(dir.path()).await;
        let m = migrator(dir.path(), &[V1, (2, "tags", "CREATE TABLE tags (tag TEXT);")]).await;
        let r = check(&path, opts(&path), &m, &Plan::default()).await.unwrap();
        assert_eq!((r.from_version, r.to_version, r.database.as_str()), (Some(1), 2, "app.db"));
        assert_eq!(applied_on(&path).await.unwrap(), vec![1], "untouched");
        assert!(backups(dir.path()).is_empty(), "no backup next to the real database");
        // A failing upgrade is reported, and still leaves it alone.
        let bad = migrator(dir.path(), &[V1, (2, "broken", "SELECT * FROM nowhere;")]).await;
        assert!(check(&path, opts(&path), &bad, &Plan::default()).await.is_err());
        assert_eq!(notes(&path).await.len(), 3);
    }

    #[test]
    fn stamps() {
        assert_eq!(utc_stamp(0), "19700101-000000");
        assert_eq!(utc_stamp(1_791_886_789_839), "20261013-101949");
        assert_eq!(utc_stamp(951_782_400_000), "20000229-000000");
    }
}
