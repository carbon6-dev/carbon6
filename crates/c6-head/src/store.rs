//! Durable head state (SQLite via sqlx).

use std::path::Path;
use std::str::FromStr;

use c6_core::WorkerId;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::{FromRow, Row};

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: SqlitePool,
    pub(crate) auth_cache: std::sync::Arc<crate::brief_cache::BriefCache<crate::auth::CachedAuth>>,
    pub(crate) realm_cache: std::sync::Arc<crate::brief_cache::BriefCache<bool>>,
}

#[derive(Clone, Debug, FromRow)]
pub struct WorkerRow {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: Option<String>,
    pub cert_serial: String,
    pub enrolled_at: i64,
    pub last_seen: Option<i64>,
    pub revoked_at: Option<i64>,
    /// "worker" or "memex"; empty until it first attaches.
    pub role: String,
    /// The realms it is available to (JSON array; empty: all).
    pub realms: String,
    /// Your labels for it (JSON array).
    pub labels: String,
}

impl WorkerRow {
    pub fn worker_id(&self) -> Option<WorkerId> {
        self.id.parse().ok()
    }
}

pub struct NewWorker<'a> {
    pub id: WorkerId,
    pub name: &'a str,
    pub hostname: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
    pub version: &'a str,
    pub cert_serial: &'a str,
}

tokio::task_local! {
    /// The realm a Settings request is for (`?realm=`): its lists are read
    /// and written there instead of where each list lives by default.
    pub static SETTINGS_REALM: String;
}

/// The head database's migrations.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// What upgrades of the head database check, and which migrations may
/// remove rows (docs/os.md §13).
pub const UPGRADES: c6_core::upgrade::Plan = c6_core::upgrade::Plan {
    checks: &[
        c6_core::upgrade::Check {
            version: 45,
            name: "every secret keeps its name and value",
            before: "SELECT count(*) || ':' || coalesce(group_concat(name || '=' || hex(value), '|'), '') \
                     FROM (SELECT * FROM before.secrets ORDER BY name)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(name || '=' || hex(value), '|'), '') \
                    FROM (SELECT * FROM secrets ORDER BY name)",
        },
        c6_core::upgrade::Check {
            version: 42,
            name: "every install keeps its id, package, version, realm and state, named after its package",
            before: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || package || '@' || version || ' in ' || realm || ' ' || state, '|'), '') \
                     FROM (SELECT * FROM before.installs ORDER BY id)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || package || '@' || version || ' in ' || realm || ' ' || state, '|'), '') \
                    FROM (SELECT * FROM installs WHERE name = package ORDER BY id)",
        },
        c6_core::upgrade::Check {
            version: 42,
            name: "the collections installed apps brought are theirs",
            before: "SELECT count(*) FROM before.collections c WHERE c.system = 0 AND EXISTS (SELECT 1 FROM before.installs i \
                     WHERE i.realm = c.realm AND i.state = 'installed' AND EXISTS (SELECT 1 FROM json_each(json_extract(i.installed, '$.contents.collections')) x \
                     WHERE json_extract(x.value, '$.name') = c.name))",
            after: "SELECT count(*) FROM collections WHERE owner != ''",
        },
        c6_core::upgrade::Check {
            version: 28,
            name: "every session has a realm",
            before: "SELECT count(*) FROM before.sessions",
            after: "SELECT count(*) FROM sessions WHERE realm IN ('home', 'root')",
        },
        c6_core::upgrade::Check {
            version: 28,
            name: "the assistant's conversations are in root",
            before: "SELECT count(*) FROM before.sessions WHERE worker_id IN (SELECT id FROM before.workers WHERE role = 'head')",
            after: "SELECT count(*) FROM sessions WHERE realm = 'root'",
        },
        c6_core::upgrade::Check {
            version: 28,
            name: "memory notes, action items, automations and secrets are in home",
            before: "SELECT (SELECT count(*) FROM before.memories) + (SELECT count(*) FROM before.todos) + (SELECT count(*) FROM before.automations) + (SELECT count(*) FROM before.secrets)",
            after: "SELECT (SELECT count(*) FROM memories WHERE realm = 'home') + (SELECT count(*) FROM todos WHERE realm = 'home') + (SELECT count(*) FROM automations WHERE realm = 'home') + (SELECT count(*) FROM secrets WHERE realm = 'home')",
        },
        c6_core::upgrade::Check {
            version: 28,
            name: "usage totals are unchanged",
            before: "SELECT coalesce(sum(input + output + cache_read + cache_write), 0) FROM before.usage",
            after: "SELECT coalesce(sum(input + output + cache_read + cache_write), 0) FROM usage",
        },
        c6_core::upgrade::Check {
            version: 28,
            name: "API tokens still reach every realm",
            before: "SELECT count(*) FROM before.api_tokens",
            after: "SELECT count(*) FROM api_tokens WHERE realm IS NULL",
        },
        c6_core::upgrade::Check {
            version: 30,
            name: "every entry of every settings list is a record",
            before: "SELECT coalesce(sum((SELECT count(*) FROM json_each(s.value) j WHERE j.type = 'object')), 0) FROM before.settings s \
                     WHERE s.key IN ('skills', 'agents', 'commands', 'hooks', 'mcp_servers', 'templates', 'workflows', 'policies', 'routes', \
                     'prices', 'budgets', 'channels', 'hosts', 'memex_folders', 'memex_scopes') AND json_valid(s.value) AND json_type(s.value) = 'array'",
            after: "SELECT count(*) FROM records WHERE created_by = 'upgrade'",
        },
        c6_core::upgrade::Check {
            version: 30,
            name: "each list's entries are unchanged and in order",
            before: "SELECT coalesce(group_concat(v, '|'), '') FROM (SELECT s.key || ':' || j.key || ':' || j.value AS v FROM before.settings s, json_each(s.value) j \
                     WHERE s.key IN ('skills', 'agents', 'commands', 'hooks', 'mcp_servers', 'templates', 'workflows', 'policies', 'routes', \
                     'prices', 'budgets', 'channels', 'hosts', 'memex_folders', 'memex_scopes') AND json_valid(s.value) AND json_type(s.value) = 'array' \
                     AND j.type = 'object' ORDER BY s.key, j.key)",
            after: "SELECT coalesce(group_concat(v, '|'), '') FROM (SELECT collection || ':' || position || ':' || fields AS v FROM records \
                    WHERE created_by = 'upgrade' ORDER BY collection, position)",
        },
        c6_core::upgrade::Check {
            version: 30,
            name: "every memory note is a record, unchanged",
            before: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || scope || '/' || text, '|'), '') FROM (SELECT * FROM before.memories ORDER BY id)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || json_extract(fields, '$.scope') || '/' || json_extract(fields, '$.text'), '|'), '') \
                    FROM (SELECT * FROM records WHERE collection = 'memory' ORDER BY id)",
        },
        c6_core::upgrade::Check {
            version: 31,
            name: "every action item is a decision with its id, title and context",
            before: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || title || '/' || context, '|'), '') FROM (SELECT * FROM before.todos ORDER BY id)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || title || '/' || context, '|'), '') FROM (SELECT * FROM decisions ORDER BY id)",
        },
        c6_core::upgrade::Check {
            version: 31,
            name: "done action items are answered decisions",
            before: "SELECT count(*) FROM before.todos WHERE state = 'done'",
            after: "SELECT count(*) FROM decisions WHERE state = 'answered'",
        },
        c6_core::upgrade::Check {
            version: 39,
            name: "every installed app is a package and an install of it, in its realm",
            before: "SELECT count(*) || ':' || coalesce(group_concat(app || '@' || version || ' in ' || realm || ' ' || state, '|'), '') FROM (SELECT * FROM before.apps ORDER BY realm)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(i.package || '@' || i.version || ' in ' || i.realm || ' ' || i.state, '|'), '') \
                    FROM (SELECT * FROM installs ORDER BY realm) i JOIN packages p ON p.digest = i.digest",
        },
        c6_core::upgrade::Check {
            version: 36,
            name: "each host's latest check is a point of host_up and host_check_ms",
            before: "SELECT count(*) || ':' || coalesce(group_concat(name || '=' || ok || '/' || latency_ms || '@' || at, '|'), '') FROM (SELECT * FROM before.host_checks ORDER BY name)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(json_extract(u.dims, '$.host') || '=' || CAST(u.value AS INTEGER) || '/' || CAST(l.value AS INTEGER) || '@' || u.ts, '|'), '') \
                    FROM (SELECT * FROM points WHERE series = 'host_up' ORDER BY json_extract(dims, '$.host')) u \
                    JOIN points l ON l.series = 'host_check_ms' AND l.dims = u.dims AND l.ts = u.ts",
        },
        c6_core::upgrade::Check {
            version: 35,
            name: "every automation is a trigger, with its id, name, schedule and webhook token",
            before: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || name || '/' || trigger || '/' || cron || '/' || token_hash || '/' || template, '|'), '') \
                     FROM (SELECT * FROM before.automations ORDER BY id)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(id || '=' || name || '/' || event || '/' || cron || '/' || token_hash || '/' || template, '|'), '') \
                    FROM (SELECT * FROM triggers WHERE action = 'start' ORDER BY id)",
        },
        c6_core::upgrade::Check {
            version: 35,
            name: "every automation run is a trigger run",
            before: "SELECT count(*) || ':' || coalesce(group_concat(run_id || '=' || automation_id || '/' || outcome, '|'), '') FROM (SELECT * FROM before.automation_runs ORDER BY run_id)",
            after: "SELECT count(*) || ':' || coalesce(group_concat(run_id || '=' || trigger_id || '/' || outcome, '|'), '') FROM (SELECT * FROM trigger_runs ORDER BY run_id)",
        },
        c6_core::upgrade::Check {
            version: 34,
            name: "commons exists, and every realm reads it",
            before: "SELECT 'yes'",
            after: "SELECT CASE WHEN EXISTS (SELECT 1 FROM realms WHERE id = 'commons') \
                    AND NOT EXISTS (SELECT 1 FROM realms WHERE reads_commons != 1) THEN 'yes' ELSE 'no' END",
        },
        c6_core::upgrade::Check {
            version: 30,
            name: "the original settings are kept",
            before: "SELECT count(*) FROM before.settings WHERE key IN ('skills', 'agents', 'commands', 'hooks', 'mcp_servers', 'templates', 'workflows', \
                     'policies', 'routes', 'prices', 'budgets', 'channels', 'hosts', 'memex_folders', 'memex_scopes')",
            after: "SELECT count(*) FROM legacy_settings",
        },
    ],
    allow: &[
        // The lists moved into records (kept in legacy_settings).
        c6_core::upgrade::Allow { version: 30, table: "settings" },
        // Memory notes moved into records (kept as legacy_memories).
        c6_core::upgrade::Allow { version: 30, table: "memories" },
        // Apps became packages and installs.
        c6_core::upgrade::Allow { version: 39, table: "apps" },
        // Automations became triggers (the same tables, renamed).
        c6_core::upgrade::Allow { version: 35, table: "automations" },
        c6_core::upgrade::Allow { version: 35, table: "automation_runs" },
        // Action items became decisions (the same table, renamed).
        c6_core::upgrade::Allow { version: 31, table: "todos" },
        // Follow-through was removed (its tables and settings).
        c6_core::upgrade::Allow { version: 27, table: "tracker_observations" },
        c6_core::upgrade::Allow { version: 27, table: "trackers" },
        c6_core::upgrade::Allow { version: 27, table: "suggestions" },
        c6_core::upgrade::Allow { version: 27, table: "settings" },
    ],
};

/// Runs the head database's upgrade on a copy (`c6h upgrade --check`).
pub async fn check_upgrade(path: &Path) -> anyhow::Result<c6_core::upgrade::Report> {
    let opts = SqliteConnectOptions::from_str("sqlite:")?.journal_mode(SqliteJournalMode::Wal).foreign_keys(true);
    c6_core::upgrade::check(path, opts, &MIGRATOR, &UPGRADES).await
}

/// Settings each realm may set for itself; where it hasn't, root's applies
/// (docs/realms-ux.md §9). A realm's own is kept as `<key>@<realm>`.
pub const PER_REALM: &[&str] =
    &["session_suggestions", "web_search", "assistant", "subagent_limits", "claude_code_tools", "brief", "work", "docs", "attention_seen"];

/// How many backups of each database upgrades keep (docs/os.md §13).
const KEEP_BACKUPS: &str = "upgrade_backups_keep";

fn parse_keep(v: &str) -> usize {
    v.trim().parse::<usize>().map_or(c6_core::upgrade::KEEP_BACKUPS, |n| n.max(1))
}

/// The setting in a database not yet opened (and maybe not yet upgraded):
/// the default when there's none or it can't be read.
async fn peek_keep_backups(path: &Path) -> usize {
    if !path.exists() {
        return c6_core::upgrade::KEEP_BACKUPS;
    }
    let read = async {
        let opts = SqliteConnectOptions::from_str("sqlite:")?.filename(path).read_only(true);
        let mut c = <sqlx::SqliteConnection as sqlx::Connection>::connect_with(&opts).await?;
        let v: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(KEEP_BACKUPS).fetch_optional(&mut c).await?;
        sqlx::Connection::close(c).await?;
        anyhow::Ok(v)
    };
    read.await.ok().flatten().as_deref().map_or(c6_core::upgrade::KEEP_BACKUPS, parse_keep)
}

/// Removes older backups of every database in the head's folder (its own,
/// the knowledge store's, each realm's), keeping the newest `keep`.
pub fn prune_backups(data_dir: &Path, keep: usize) {
    let knowledge = data_dir.join("knowledge");
    let mut dbs = vec![data_dir.join("c6h.db"), knowledge.join("memex.db")];
    if let Ok(realms) = std::fs::read_dir(knowledge.join("realms")) {
        dbs.extend(realms.flatten().map(|r| r.path().join("memex.db")));
    }
    for db in dbs {
        if let Err(e) = c6_core::upgrade::prune_backups(&db, keep) {
            tracing::warn!(database = %db.display(), error = %format!("{e:#}"), "removing older backups failed");
        }
    }
}

fn own_key(key: &str, realm: &str) -> String {
    format!("{key}@{realm}")
}

/// The per-realm setting that says what new sessions start with (its
/// `catchup` and `fold`, beside suggestions' `enabled`).
const SESSION_VIEW_DEFAULTS: &str = "session_suggestions";

/// Where a session keeps how it shows itself to you (no table of its own).
fn session_view_key(id: &str) -> String {
    format!("session_view:{id}")
}

/// The realm a Settings request is for (none: root's).
fn request_realm() -> Option<String> {
    SETTINGS_REALM.try_with(Clone::clone).ok().filter(|r| r != crate::realms::ROOT)
}

impl Store {
    /// The database (for tests that set up what an earlier version left).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self::open_reporting(path).await?.0)
    }

    /// Opens the database, upgrading it first if needed, with the upgrade's
    /// report (`None` when nothing needed doing).
    pub async fn open_reporting(path: &Path) -> anyhow::Result<(Self, Option<c6_core::upgrade::Report>)> {
        // Every database in the head's folder keeps as many backups as it
        // says, read before this one's upgrade removes any.
        if let Some(dir) = path.parent() {
            c6_core::upgrade::keep_backups(dir, peek_keep_backups(path).await);
            // The head checkpoints the databases in its folder itself
            // (docs/backups.md §3).
            c6_core::wal::manual_under(dir);
        }
        let opts = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let (pool, report) = c6_core::upgrade::open(path, opts, SqlitePoolOptions::new().max_connections(8), &MIGRATOR, &UPGRADES).await?;
        crate::lists::ensure(&pool).await?;
        crate::prompts::seed(&pool).await?;
        crate::claims::seed(&pool).await?;
        crate::packages::fix_legacy(&pool).await?;
        Ok((Self { pool, auth_cache: Default::default(), realm_cache: Default::default() }, report))
    }

    /// Earlier upgrades of this database, newest first.
    pub async fn upgrades(&self) -> anyhow::Result<Vec<c6_core::upgrade::Report>> {
        c6_core::upgrade::reports(&self.pool).await
    }

    /// How many backups of each database upgrades keep.
    pub async fn keep_backups(&self) -> anyhow::Result<usize> {
        Ok(self.raw_setting(KEEP_BACKUPS).await?.as_deref().map_or(c6_core::upgrade::KEEP_BACKUPS, parse_keep))
    }

    /// Sets it for every database in this database's folder.
    pub async fn set_keep_backups(&self, keep: usize) -> anyhow::Result<()> {
        let keep = keep.max(1);
        self.set_setting(KEEP_BACKUPS, &keep.to_string()).await?;
        if let Some(dir) = self.pool.connect_options().get_filename().parent() {
            c6_core::upgrade::keep_backups(dir, keep);
        }
        Ok(())
    }

    pub async fn create_enrollment(&self, secret_hash: &str, name: Option<&str>, now: i64, expires_at: i64) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO enrollments (secret_hash, name, created_at, expires_at) VALUES (?, ?, ?, ?)")
            .bind(secret_hash)
            .bind(name)
            .bind(now)
            .bind(expires_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Atomically marks an enrollment used. Returns its requested name
    /// (`Some(None)` if it had none) or `None` if it is unknown, used or expired.
    pub async fn consume_enrollment(&self, secret_hash: &str, now: i64) -> anyhow::Result<Option<Option<String>>> {
        let row =
            sqlx::query("UPDATE enrollments SET used_at = ? WHERE secret_hash = ? AND used_at IS NULL AND expires_at > ? RETURNING name")
                .bind(now)
                .bind(secret_hash)
                .bind(now)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.get::<Option<String>, _>("name")))
    }

    pub async fn insert_worker(&self, w: &NewWorker<'_>, secret_hash: &str, now: i64) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO workers (id, name, hostname, os, arch, version, cert_serial, enrolled_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(w.id.to_string())
        .bind(w.name)
        .bind(w.hostname)
        .bind(w.os)
        .bind(w.arch)
        .bind(w.version)
        .bind(w.cert_serial)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE enrollments SET worker_id = ? WHERE secret_hash = ?")
            .bind(w.id.to_string())
            .bind(secret_hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn worker(&self, id: WorkerId) -> anyhow::Result<Option<WorkerRow>> {
        Ok(sqlx::query_as("SELECT * FROM workers WHERE id = ?").bind(id.to_string()).fetch_optional(&self.pool).await?)
    }

    pub async fn workers(&self) -> anyhow::Result<Vec<WorkerRow>> {
        // Not the head's own (it runs the assistant's conversations).
        Ok(sqlx::query_as("SELECT * FROM workers WHERE role != 'head' ORDER BY name, id").fetch_all(&self.pool).await?)
    }

    /// Records facts from a worker's `Hello`.
    pub async fn set_worker_role(&self, id: WorkerId, role: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE workers SET role = ? WHERE id = ?").bind(role).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn worker_attached(&self, id: WorkerId, hostname: &str, os: &str, arch: &str, version: &str, now: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE workers SET hostname = ?, os = ?, arch = ?, version = ?, last_seen = ? WHERE id = ?")
            .bind(hostname)
            .bind(os)
            .bind(arch)
            .bind(version)
            .bind(now)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn touch_worker(&self, id: WorkerId, now: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE workers SET last_seen = ? WHERE id = ?").bind(now).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }

    /// Returns false if the worker does not exist.
    pub async fn revoke_worker(&self, id: WorkerId, now: i64) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE workers SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ? AND role != 'head'")
            .bind(now)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn delete_worker(&self, id: WorkerId) -> anyhow::Result<bool> {
        let r = sqlx::query("DELETE FROM workers WHERE id = ? AND role != 'head'").bind(id.to_string()).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }
}

// --- Connections --------------------------------------------------------------

#[derive(Clone, Debug, FromRow)]
pub struct ConnectionRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: Option<Vec<u8>>,
    pub default_model: Option<String>,
    pub context_window: Option<i64>,
    pub created_at: i64,
    /// The realms it is available to (JSON array; empty: all).
    pub realms: String,
    /// A pool's members (JSON array of `api::PoolMember`).
    pub members: String,
    /// The model subagents of its sessions run on when nothing else says.
    pub subagent_model: Option<String>,
    /// Percent of the context window at which its sessions compact.
    pub compact_at: Option<i64>,
}

impl Store {
    pub async fn insert_connection(&self, c: &ConnectionRow) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO connections (id, name, kind, base_url, api_key, default_model, created_at, members, context_window, subagent_model, compact_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&c.id)
        .bind(&c.name)
        .bind(&c.kind)
        .bind(&c.base_url)
        .bind(&c.api_key)
        .bind(&c.default_model)
        .bind(c.created_at)
        .bind(&c.members)
        .bind(c.context_window)
        .bind(&c.subagent_model)
        .bind(c.compact_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sets a connection's name, members, optional context window and its
    /// sessions' defaults.
    pub async fn update_connection(&self, c: &ConnectionRow) -> anyhow::Result<()> {
        sqlx::query("UPDATE connections SET name = ?, members = ?, context_window = ?, subagent_model = ?, compact_at = ? WHERE id = ?")
            .bind(&c.name)
            .bind(&c.members)
            .bind(c.context_window)
            .bind(&c.subagent_model)
            .bind(c.compact_at)
            .bind(&c.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn connections(&self) -> anyhow::Result<Vec<ConnectionRow>> {
        Ok(sqlx::query_as("SELECT * FROM connections ORDER BY name").fetch_all(&self.pool).await?)
    }

    /// The connection and model of your latest session (on a connection, or
    /// on any).
    pub async fn latest_model(&self, connection_id: &str) -> anyhow::Result<Option<(String, String)>> {
        Ok(sqlx::query_as(
            "SELECT connection_id, model FROM sessions WHERE (? = '' OR connection_id = ?) AND parent_id IS NULL AND model != '' \
             AND connection_id IN (SELECT id FROM connections) ORDER BY created_at DESC LIMIT 1",
        )
        .bind(connection_id)
        .bind(connection_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn connection(&self, id: &str) -> anyhow::Result<Option<ConnectionRow>> {
        Ok(sqlx::query_as("SELECT * FROM connections WHERE id = ?").bind(id).fetch_optional(&self.pool).await?)
    }

    pub async fn set_connection_secret(&self, id: &str, secret: &[u8]) -> anyhow::Result<()> {
        sqlx::query("UPDATE connections SET api_key = ? WHERE id = ?").bind(secret).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn delete_connection(&self, id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("DELETE FROM connections WHERE id = ?").bind(id).execute(&self.pool).await?.rows_affected() > 0)
    }
}

// --- Sessions and events ------------------------------------------------------

#[derive(Clone, Debug, FromRow)]
pub struct SessionRow {
    pub id: String,
    pub worker_id: String,
    pub title: String,
    pub cwd: String,
    pub connection_id: String,
    pub model: String,
    pub status: String,
    pub error: Option<String>,
    pub effort: Option<String>,
    pub parent_id: Option<String>,
    pub parent_call_id: Option<String>,
    pub fork_seq: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seq: i64,
    pub archived: i64,
    /// Your labels, as a JSON array.
    pub labels: String,
    pub worker_name: String,
    pub repo: String,
    pub folder: String,
    pub home: String,
    pub ssh_target: String,
    /// Tool calls waiting for approval.
    pub awaiting: i64,
    /// The model for subagents, from a routing rule or picked in the
    /// session; empty: `model`.
    pub subagent_model: String,
    /// With `subagent_model`: the connection subagents run on (empty: the
    /// session's) and their effort.
    pub subagent_connection_id: String,
    pub subagent_effort: String,
    /// Tools from the routing rule, as a JSON array; `[]`: all tools.
    pub tools: String,
    /// The realm it runs in (docs/os.md §3).
    pub realm: String,
    /// Subagents: the named agent; empty for a general-purpose one.
    pub agent: String,
    /// How its last turn ended (`completed`, `interrupted`, `error`,
    /// `worker_restarted`); empty before one did.
    pub end_reason: String,
    /// The top-level session of its tree; `None` for a top-level session.
    pub root_id: Option<String>,
    /// A subagent of Claude Code's, mirrored from its stream: read-only.
    pub observed: bool,
    /// The app instance it acts as (docs/apps.md §5.3); empty: the realm.
    pub instance: String,
    /// Its colour (`c6_api::SESSION_COLOURS`); empty when creating one: a
    /// top-level session gets the one least used among its realm's recent
    /// sessions.
    pub colour: String,
    /// A remote subagent: the template the head started it from
    /// (docs/remote-subagents.md); empty for every other session.
    pub remote_template: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionActivity {
    pub subagents_running: u32,
    pub side_chats_running: u32,
    /// Its background tasks that keep it showing as running, and the
    /// servers and watchers that don't.
    pub tasks_running: u32,
    pub servers_running: u32,
}

impl SessionRow {
    pub fn user_labels(&self) -> Vec<String> {
        serde_json::from_str(&self.labels).unwrap_or_default()
    }

    /// All its labels: yours, then the automatic ones.
    pub fn all_labels(&self) -> Vec<String> {
        let folder =
            if self.folder.is_empty() && self.cwd.starts_with('/') && self.ssh_target.is_empty() { &self.cwd } else { &self.folder };
        let mut l = self.user_labels();
        l.extend(c6_api::labels::auto_labels(&self.worker_name, &self.repo, folder, &self.home, &self.model, &self.ssh_target));
        if !self.instance.is_empty() {
            l.push(format!("app:{}", self.instance));
        }
        l
    }

    /// The labels the head passes to the worker (which adds the rest).
    pub fn head_labels(&self) -> Vec<String> {
        let mut l = self.user_labels();
        if !self.worker_name.is_empty() {
            l.push(format!("worker:{}", self.worker_name));
        }
        l
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct CommandRow {
    pub seq: i64,
    pub id: String,
    pub session_id: String,
    pub worker_id: String,
    pub data: Vec<u8>,
    pub text: Option<String>,
    pub created_at: i64,
    pub state: String,
    pub error: Option<String>,
    /// The worker handed it to the agent, which reads it at its next step.
    pub with_agent: i64,
}

/// What an event batch changed.
pub struct Ingested {
    pub last_seq: u64,
    pub events: Vec<c6_proto::SessionEvent>,
    /// Commands the worker has now applied.
    pub done_commands: Vec<String>,
    /// Commands now with the agent (`InputDelivered`).
    pub delivered_commands: Vec<String>,
}

impl Store {
    /// Creates a session and enqueues its commands atomically: `held` ones
    /// wait for `release_held`. Returns its colour (picked here when `s`
    /// has none).
    pub async fn create_session(&self, s: &SessionRow, commands: &[NewCommand<'_>], held: bool) -> anyhow::Result<String> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let colour = match (s.colour.is_empty(), &s.parent_id) {
            (true, None) => least_used_colour(&mut tx, &s.realm).await?,
            _ => s.colour.clone(),
        };
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, effort, parent_id, fork_seq, \
             labels, worker_name, ssh_target, subagent_model, realm, root_id, instance, colour, subagent_connection_id, subagent_effort, tools, \
             parent_call_id, agent, remote_template) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&s.id)
        .bind(&s.worker_id)
        .bind(&s.title)
        .bind(&s.cwd)
        .bind(&s.connection_id)
        .bind(&s.model)
        .bind(&s.status)
        .bind(s.created_at)
        .bind(s.updated_at)
        .bind(&s.effort)
        .bind(&s.parent_id)
        .bind(s.fork_seq)
        .bind(&s.labels)
        .bind(&s.worker_name)
        .bind(&s.ssh_target)
        .bind(&s.subagent_model)
        .bind(&s.realm)
        .bind(&s.root_id)
        .bind(&s.instance)
        .bind(&colour)
        .bind(&s.subagent_connection_id)
        .bind(&s.subagent_effort)
        .bind(&s.tools)
        .bind(&s.parent_call_id)
        .bind(&s.agent)
        .bind(&s.remote_template)
        .execute(&mut *tx)
        .await?;
        for c in commands {
            insert_command(&mut tx, c, held).await?;
        }
        // Your new session starts as its realm shows new sessions (Settings
        // → Suggestions: the catch-up card, finished turns folded).
        if s.parent_id.is_none() {
            let mut defaults = None;
            for key in [own_key(SESSION_VIEW_DEFAULTS, &s.realm), SESSION_VIEW_DEFAULTS.to_owned()] {
                defaults =
                    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?").bind(key).fetch_optional(&mut *tx).await?;
                if defaults.is_some() {
                    break;
                }
            }
            let view: c6_api::SessionViewSettings = defaults.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
            if view != c6_api::SessionViewSettings::default() {
                sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
                    .bind(session_view_key(&s.id))
                    .bind(serde_json::to_string(&view)?)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(colour)
    }

    /// How a session shows itself to you (the catch-up card, finished turns
    /// folded).
    pub async fn session_view(&self, id: &str) -> anyhow::Result<c6_api::SessionViewSettings> {
        Ok(self.raw_setting(&session_view_key(id)).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
    }

    pub async fn set_session_view(&self, id: &str, view: &c6_api::SessionViewSettings) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(session_view_key(id))
            .bind(serde_json::to_string(view)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// How many subagents of a tree are working now.
    pub async fn running_subagents(&self, root: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE root_id = ? AND parent_call_id IS NOT NULL AND status = 'running'")
            .bind(root)
            .fetch_one(&self.pool)
            .await?)
    }

    /// Records the report obligation for an input of a remote child.
    /// `call_id` is empty for a steer without a waiting tool.
    pub async fn add_remote_delegation(&self, command_id: &str, child_id: &str, parent_id: &str, call_id: &str) -> anyhow::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO remote_delegations (command_id, child_id, parent_id, call_id, state, created_at) VALUES (?, ?, ?, ?, 'waiting', ?)")
            .bind(command_id)
            .bind(child_id)
            .bind(parent_id)
            .bind(call_id)
            .bind(c6_core::now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Pending report obligations: (input/turn id, call or empty for a notice).
    pub async fn waiting_delegations(&self, child_id: &str) -> anyhow::Result<Vec<(String, String)>> {
        Ok(sqlx::query_as(
            "SELECT command_id, call_id FROM remote_delegations WHERE child_id = ? AND state = 'waiting' ORDER BY created_at",
        )
        .bind(child_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Marks a delegation reported; false if it already was.
    pub async fn delegation_reported(&self, command_id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("UPDATE remote_delegations SET state = 'reported' WHERE command_id = ? AND state = 'waiting'")
            .bind(command_id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// A delegation's input was sent again under a new id (a restore).
    pub async fn rename_delegation(&self, old: &str, new: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE remote_delegations SET command_id = ? WHERE command_id = ?").bind(new).bind(old).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn session(&self, id: &str) -> anyhow::Result<Option<SessionRow>> {
        Ok(sqlx::query_as("SELECT * FROM sessions WHERE id = ?").bind(id).fetch_optional(&self.pool).await?)
    }

    /// When a session's last turn ended (or it failed) and when you last
    /// saw it (docs/work.md §3); 0 for never.
    pub async fn turn_seen(&self, id: &str) -> anyhow::Result<(i64, i64)> {
        let r = sqlx::query("SELECT turn_ended_at, seen_at FROM sessions WHERE id = ?").bind(id).fetch_optional(&self.pool).await?;
        Ok(r.map(|r| (r.get("turn_ended_at"), r.get("seen_at"))).unwrap_or_default())
    }

    /// You've seen how a session stands, at `at`: a turn that ended before
    /// no longer asks for you. Whether it had one you hadn't seen.
    pub async fn mark_seen(&self, id: &str, at: i64) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE sessions SET seen_at = ? WHERE id = ? AND seen_at < turn_ended_at AND turn_ended_at <= ?")
            .bind(at)
            .bind(id)
            .bind(at)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// The most recently active top-level sessions, of one realm (or
    /// every realm's): a realm's own, so a busy realm can't crowd another
    /// out of its list.
    pub async fn sessions(&self, realm: Option<&str>, limit: i64) -> anyhow::Result<Vec<SessionRow>> {
        Ok(sqlx::query_as(
            "SELECT * FROM sessions WHERE archived = 0 AND parent_id IS NULL AND (? IS NULL OR realm = ?) ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(realm)
        .bind(realm)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The most recently active side chats of one realm (not subagents).
    pub async fn side_chats(&self, realm: &str, limit: i64) -> anyhow::Result<Vec<SessionRow>> {
        Ok(sqlx::query_as(
            "SELECT * FROM sessions WHERE archived = 0 AND parent_id IS NOT NULL AND parent_call_id IS NULL AND fork_seq IS NOT NULL \
             AND realm = ? ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(realm)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Running descendants for display, in one batch for list/feed and detail.
    /// Side chats count for each ancestor; subagents retain their top-level
    /// root semantics. Never follow a relationship into a different realm.
    pub async fn session_activity(&self, realm: Option<&str>) -> anyhow::Result<std::collections::HashMap<String, SessionActivity>> {
        let rows = sqlx::query(
            "WITH RECURSIVE side_ancestors(id, realm, child_id) AS (\
                 SELECT p.id, p.realm, s.id FROM sessions s JOIN sessions p ON p.id = s.parent_id AND p.realm = s.realm \
                 WHERE s.fork_seq IS NOT NULL AND s.parent_call_id IS NULL AND s.status = 'running' AND (?1 IS NULL OR s.realm = ?1) \
                 UNION \
                 SELECT p.id, p.realm, a.child_id FROM side_ancestors a JOIN sessions s ON s.id = a.id \
                 JOIN sessions p ON p.id = s.parent_id AND p.realm = a.realm\
             ), activity AS (\
                 SELECT id, 0 AS subagents, COUNT(*) AS side_chats, 0 AS tasks, 0 AS servers FROM side_ancestors GROUP BY id \
                 UNION ALL \
                 SELECT p.id, COUNT(*), 0, 0, 0 FROM sessions s JOIN sessions p ON p.id = s.root_id AND p.realm = s.realm \
                 WHERE s.parent_call_id IS NOT NULL AND s.status = 'running' AND (?1 IS NULL OR s.realm = ?1) GROUP BY p.id \
                 UNION ALL \
                 SELECT t.session_id, 0, 0, SUM(t.counted), SUM(1 - t.counted) FROM running_tasks t JOIN sessions s ON s.id = t.session_id \
                 WHERE (?1 IS NULL OR s.realm = ?1) GROUP BY t.session_id\
             ) SELECT id, SUM(subagents) AS subagents, SUM(side_chats) AS side_chats, SUM(tasks) AS tasks, SUM(servers) AS servers \
             FROM activity GROUP BY id",
        )
        .bind(realm)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("id"),
                    SessionActivity {
                        subagents_running: r.get::<i64, _>("subagents") as u32,
                        side_chats_running: r.get::<i64, _>("side_chats") as u32,
                        tasks_running: r.get::<i64, _>("tasks") as u32,
                        servers_running: r.get::<i64, _>("servers") as u32,
                    },
                )
            })
            .collect())
    }

    /// Every session below `id`: its side chats and subagents, theirs, and so
    /// on, oldest first.
    pub async fn descendant_sessions(&self, id: &str) -> anyhow::Result<Vec<SessionRow>> {
        Ok(sqlx::query_as(
            "WITH RECURSIVE tree(id) AS (SELECT id FROM sessions WHERE parent_id = ? \
             UNION SELECT s.id FROM sessions s JOIN tree ON s.parent_id = tree.id) \
             SELECT * FROM sessions WHERE id IN (SELECT id FROM tree) ORDER BY created_at",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn child_sessions(&self, parent_id: &str) -> anyhow::Result<Vec<SessionRow>> {
        Ok(sqlx::query_as("SELECT * FROM sessions WHERE parent_id = ? ORDER BY created_at").bind(parent_id).fetch_all(&self.pool).await?)
    }

    /// Secrets (names and descriptions), of one realm or (`None`) all.
    pub async fn secrets(&self, realm: Option<&str>) -> anyhow::Result<Vec<c6_api::SecretInfo>> {
        let rows = sqlx::query(
            "SELECT realm, name, description, applies_to, updated_at FROM secrets WHERE (? IS NULL OR realm = ?) ORDER BY name, realm",
        )
        .bind(realm)
        .bind(realm)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| c6_api::SecretInfo {
                name: r.get("name"),
                description: r.get("description"),
                applies_to: serde_json::from_str(&r.get::<String, _>("applies_to")).unwrap_or_default(),
                updated_at: r.get("updated_at"),
                realm: r.get("realm"),
            })
            .collect())
    }

    /// The secrets a realm's sessions have: its own, and root's it doesn't
    /// have under the same name.
    pub async fn secrets_for(&self, realm: &str) -> anyhow::Result<Vec<c6_api::SecretInfo>> {
        let mut own = self.secrets(Some(realm)).await?;
        if realm != crate::realms::ROOT {
            for s in self.secrets(Some(crate::realms::ROOT)).await? {
                if !own.iter().any(|o| o.name == s.name) {
                    own.push(s);
                }
            }
            own.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Ok(own)
    }

    /// (encrypted value, applies_to, realm) of a secret a realm has: its
    /// own, else root's.
    pub async fn secret(&self, realm: &str, name: &str) -> anyhow::Result<Option<(Vec<u8>, Vec<String>, String)>> {
        let row = sqlx::query(
            "SELECT value, applies_to, realm FROM secrets WHERE name = ? AND realm IN (?, 'root') ORDER BY realm = 'root' LIMIT 1",
        )
        .bind(name)
        .bind(realm)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get("value"), serde_json::from_str(&r.get::<String, _>("applies_to")).unwrap_or_default(), r.get("realm"))))
    }

    /// Creates or updates a realm's secret; `value: None` keeps the current
    /// value (and fails for a new secret).
    pub async fn put_secret(
        &self,
        realm: &str,
        name: &str,
        value: Option<Vec<u8>>,
        description: &str,
        applies_to: &[String],
    ) -> anyhow::Result<bool> {
        let now = c6_core::now_ms();
        let labels = serde_json::to_string(applies_to)?;
        let done = match value {
            Some(v) => sqlx::query(
                "INSERT INTO secrets (realm, name, value, description, applies_to, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(realm, name) DO UPDATE SET value = excluded.value, description = excluded.description, \
                 applies_to = excluded.applies_to, updated_at = excluded.updated_at",
            )
            .bind(realm)
            .bind(name)
            .bind(v)
            .bind(description)
            .bind(&labels)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            None => sqlx::query("UPDATE secrets SET description = ?, applies_to = ?, updated_at = ? WHERE realm = ? AND name = ?")
                .bind(description)
                .bind(&labels)
                .bind(now)
                .bind(realm)
                .bind(name)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        };
        Ok(done > 0)
    }

    /// Moves a secret to another realm, unless that realm has one already.
    pub async fn move_secret(&self, name: &str, from: &str, to: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE OR IGNORE secrets SET realm = ? WHERE realm = ? AND name = ?")
            .bind(to)
            .bind(from)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_secret(&self, realm: &str, name: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query("DELETE FROM secrets WHERE realm = ? AND name = ?")
            .bind(realm)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    pub async fn set_session_colour(&self, id: &str, colour: &str) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE sessions SET colour = ? WHERE id = ?").bind(colour).bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }

    /// The default for a session's subagents (all empty: its own model).
    pub async fn set_subagent_model(&self, id: &str, connection_id: &str, model: &str, effort: &str) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE sessions SET subagent_connection_id = ?, subagent_model = ?, subagent_effort = ? WHERE id = ?")
            .bind(connection_id)
            .bind(model)
            .bind(effort)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_session_labels(&self, id: &str, labels: &[String]) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE sessions SET labels = ?, updated_at = ? WHERE id = ?")
            .bind(serde_json::to_string(labels)?)
            .bind(c6_core::now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// A setting's value. A system list (docs/os.md §12.2) is read from its
    /// collection in the realm it lives in (or, during a Settings request
    /// for one realm, that realm's own entries), as a JSON array.
    pub async fn setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        if let Some(l) = crate::lists::list(key) {
            let realm = SETTINGS_REALM.try_with(Clone::clone).unwrap_or_else(|_| l.realm.to_owned());
            return Ok(Some(serde_json::to_string(&crate::lists::read(&self.pool, key, &realm).await?)?));
        }
        match request_realm().filter(|_| PER_REALM.contains(&key)) {
            Some(realm) => self.per_realm(key, &realm).await,
            None => self.raw_setting(key).await,
        }
    }

    async fn raw_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query("SELECT value FROM settings WHERE key = ?").bind(key).fetch_optional(&self.pool).await?.map(|r| r.get("value")))
    }

    /// A per-realm setting as `realm` has it: its own, else root's.
    async fn per_realm(&self, key: &str, realm: &str) -> anyhow::Result<Option<String>> {
        match self.raw_setting(&own_key(key, realm)).await? {
            Some(v) => Ok(Some(v)),
            None => self.raw_setting(key).await,
        }
    }

    /// Whether `realm` has its own value of a per-realm setting.
    pub async fn has_own_setting(&self, key: &str, realm: &str) -> anyhow::Result<bool> {
        Ok(self.raw_setting(&own_key(key, realm)).await?.is_some())
    }

    /// Drops `realm`'s own value of a per-realm setting: root's applies.
    pub async fn drop_own_setting(&self, key: &str, realm: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM settings WHERE key = ?").bind(own_key(key, realm)).execute(&self.pool).await?;
        Ok(())
    }

    /// A system list as `realm` uses it (root's entries and its own), as a
    /// JSON array; any other setting as it is.
    pub async fn setting_for(&self, key: &str, realm: &str) -> anyhow::Result<Option<String>> {
        if crate::lists::list(key).is_some() {
            return Ok(Some(serde_json::to_string(&crate::lists::effective(&self.pool, key, realm).await?)?));
        }
        if PER_REALM.contains(&key) && realm != crate::realms::ROOT {
            return self.per_realm(key, realm).await;
        }
        self.raw_setting(key).await
    }

    /// A system list as an app instance's sessions use it (docs/apps.md
    /// §5.3), or as the realm's do (`instance` empty), as a JSON array.
    pub async fn setting_in(&self, key: &str, realm: &str, instance: &str) -> anyhow::Result<Option<String>> {
        if crate::lists::list(key).is_some() {
            let inst = Some(instance).filter(|i| !i.is_empty());
            let v: Vec<serde_json::Value> =
                crate::lists::effective_tagged(&self.pool, key, realm, inst).await?.into_iter().map(|(v, _)| v).collect();
            return Ok(Some(serde_json::to_string(&v)?));
        }
        self.setting(key).await
    }

    /// Sets a setting. A system list is written to its collection (its
    /// entries become records, each with its own history).
    pub async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        if let Some(l) = crate::lists::list(key) {
            let entries: Vec<serde_json::Value> = serde_json::from_str(value)?;
            let realm = SETTINGS_REALM.try_with(Clone::clone).unwrap_or_else(|_| l.realm.to_owned());
            return crate::lists::write(&self.pool, key, &realm, &entries, "you").await;
        }
        let key = match request_realm().filter(|_| PER_REALM.contains(&key)) {
            Some(realm) => own_key(key, &realm),
            None => key.to_owned(),
        };
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_worker_labels(&self, id: &str, labels: &[String]) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE workers SET labels = ? WHERE id = ? AND role != 'head'")
            .bind(serde_json::to_string(labels)?)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_archived(&self, id: &str, archived: bool) -> anyhow::Result<bool> {
        // With its side chats and subagents, all the way down.
        let r = sqlx::query(
            "WITH RECURSIVE tree(id) AS (SELECT ? UNION SELECT s.id FROM sessions s JOIN tree ON s.parent_id = tree.id) \
             UPDATE sessions SET archived = ? WHERE id IN (SELECT id FROM tree)",
        )
        .bind(id)
        .bind(archived as i64)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_title(&self, id: &str, title: &str) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE sessions SET title = ? WHERE id = ?").bind(title).bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }

    /// Last durable seq for each of `ids` that belongs to `worker_id`.
    pub async fn session_positions(&self, worker_id: WorkerId, ids: &[String]) -> anyhow::Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(row) = sqlx::query("SELECT last_seq FROM sessions WHERE id = ? AND worker_id = ?")
                .bind(id)
                .bind(worker_id.to_string())
                .fetch_optional(&self.pool)
                .await?
            {
                out.push((id.clone(), row.get::<i64, _>("last_seq") as u64));
            }
        }
        Ok(out)
    }

    /// Stores the next contiguous events of a worker's session; duplicates
    /// are skipped and a gap stops the batch (the worker resends from the
    /// ack). Returns `None` if the session is unknown or not this worker's.
    pub async fn ingest(
        &self,
        worker_id: WorkerId,
        session_id: &str,
        events: &[c6_proto::SessionEvent],
    ) -> anyhow::Result<Option<Ingested>> {
        use c6_proto::session_event::Kind;
        use prost::Message;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut row = sqlx::query("SELECT last_seq, status, remote_template FROM sessions WHERE id = ? AND worker_id = ?")
            .bind(session_id)
            .bind(worker_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if row.is_none() && create_subagent_session(&mut tx, worker_id, session_id, events).await? {
            row = sqlx::query("SELECT last_seq, status, remote_template FROM sessions WHERE id = ?")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await?;
        }
        let Some(row) = row else {
            return Ok(None);
        };
        let mut last = row.get::<i64, _>("last_seq") as u64;
        let mut status: String = row.get("status");
        let remote = !row.get::<String, _>("remote_template").is_empty();
        let mut stored = Vec::new();
        let mut done = Vec::new();
        let mut delivered: Vec<String> = Vec::new();
        for ev in events {
            if ev.seq <= last {
                continue;
            }
            if ev.seq != last + 1 {
                break;
            }
            sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES (?, ?, ?, ?)")
                .bind(session_id)
                .bind(ev.seq as i64)
                .bind(ev.ts_ms)
                .bind(ev.encode_to_vec())
                .execute(&mut *tx)
                .await?;
            crate::search::index_event(&mut tx, session_id, ev).await?;
            crate::analytics::mark(&mut tx, session_id, ev).await?;
            last = ev.seq;
            match &ev.kind {
                Some(Kind::Started(s)) => {
                    status = "idle".into();
                    done.push(s.command_id.clone());
                    // A scratch session learns its folder here.
                    sqlx::query(
                        "UPDATE sessions SET repo = ?, folder = ?, home = ?, cwd = CASE WHEN cwd = '' THEN ? ELSE cwd END WHERE id = ?",
                    )
                    .bind(&s.repo)
                    .bind(&s.cwd)
                    .bind(&s.home)
                    .bind(&s.cwd)
                    .bind(session_id)
                    .execute(&mut *tx)
                    .await?;
                }
                Some(Kind::User(u)) => done.push(u.command_id.clone()),
                // A session message the worker recorded as a notice is
                // applied: it is delivered from its log from now on.
                Some(Kind::NoticeQueued(n)) => done.push(n.command_id.clone()),
                Some(Kind::InputDelivered(d)) => delivered.push(d.command_id.clone()),
                // Its background tasks while they run. One that tells the
                // agent when it ends (and every one of Claude Code's own) keeps
                // the session showing as running; a server or watcher doesn't.
                Some(Kind::BackgroundTask(t)) if t.state == "started" => {
                    sqlx::query("INSERT OR REPLACE INTO running_tasks (session_id, task_id, counted) VALUES (?, ?, ?)")
                        .bind(session_id)
                        .bind(&t.task_id)
                        .bind((t.notify || t.kind == "claude_code") as i64)
                        .execute(&mut *tx)
                        .await?;
                }
                Some(Kind::BackgroundTask(t)) => {
                    sqlx::query("DELETE FROM running_tasks WHERE session_id = ? AND task_id = ?")
                        .bind(session_id)
                        .bind(&t.task_id)
                        .execute(&mut *tx)
                        .await?;
                }
                Some(Kind::Memory(m)) => crate::memory::apply(&mut tx, session_id, m).await?,
                Some(Kind::Usage(u)) => crate::usage::record(&mut tx, session_id, ev.seq, ev.ts_ms, u).await?,
                Some(Kind::Approval(a)) => {
                    let delta = if a.state == "requested" { 1 } else { -1 };
                    sqlx::query("UPDATE sessions SET awaiting = MAX(awaiting + ?, 0) WHERE id = ?")
                        .bind(delta)
                        .bind(session_id)
                        .execute(&mut *tx)
                        .await?;
                }
                Some(Kind::Rewound(r)) => done.push(r.command_id.clone()),
                // A remote subagent's report reached its parent.
                Some(Kind::SubagentReported(r)) => done.push(r.command_id.clone()),
                Some(Kind::TurnStarted(_)) => status = "running".into(),
                Some(Kind::TurnEnded(t)) => {
                    // Commit the report obligation with the turn end, not
                    // in its downstream hook. This includes turns resumed
                    // by the child's own task notices, with no waiting call.
                    if remote {
                        sqlx::query("INSERT OR IGNORE INTO remote_delegations (command_id, child_id, parent_id, call_id, state, created_at) SELECT ?, id, parent_id, '', 'waiting', ? FROM sessions WHERE id = ? AND parent_id IS NOT NULL")
                            .bind(crate::remote::turn_report_id(session_id, ev.seq))
                            .bind(ev.ts_ms)
                            .bind(session_id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    status = "idle".into();
                    let reason = match c6_proto::TurnEndReason::try_from(t.reason) {
                        Ok(c6_proto::TurnEndReason::Completed) => "completed",
                        Ok(c6_proto::TurnEndReason::Interrupted) => "interrupted",
                        Ok(c6_proto::TurnEndReason::WorkerRestarted) => "worker_restarted",
                        _ => "error",
                    };
                    // Nothing waits past its turn; when it ended, for
                    // whether you've seen it (docs/work.md §3).
                    sqlx::query("UPDATE sessions SET awaiting = 0, end_reason = ?, turn_ended_at = ? WHERE id = ?")
                        .bind(reason)
                        .bind(c6_core::now_ms())
                        .bind(session_id)
                        .execute(&mut *tx)
                        .await?;
                }
                Some(Kind::SubagentModelSet(m)) => {
                    done.push(m.command_id.clone());
                    let spec = m.model.clone().unwrap_or_default();
                    sqlx::query("UPDATE sessions SET subagent_connection_id = ?, subagent_model = ?, subagent_effort = ? WHERE id = ?")
                        .bind(&spec.connection_id)
                        .bind(&spec.model)
                        .bind(&spec.effort)
                        .bind(session_id)
                        .execute(&mut *tx)
                        .await?;
                }
                Some(Kind::ModelSwitched(m)) => {
                    let spec = m.model.clone().unwrap_or_default();
                    sqlx::query("UPDATE sessions SET connection_id = ?, model = ?, effort = ? WHERE id = ?")
                        .bind(&spec.connection_id)
                        .bind(&spec.model)
                        .bind(Some(spec.effort).filter(|e| !e.is_empty()))
                        .bind(session_id)
                        .execute(&mut *tx)
                        .await?;
                }
                _ => {}
            }
            stored.push(ev.clone());
        }
        if !stored.is_empty() {
            sqlx::query("UPDATE sessions SET last_seq = ?, status = ?, updated_at = ? WHERE id = ?")
                .bind(last as i64)
                .bind(&status)
                .bind(c6_core::now_ms())
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            for id in &done {
                sqlx::query("UPDATE commands SET state = 'done' WHERE id = ?").bind(id).execute(&mut *tx).await?;
            }
            for id in &delivered {
                sqlx::query("UPDATE commands SET with_agent = 1 WHERE id = ? AND state = 'pending'").bind(id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(Some(Ingested { last_seq: last, events: stored, done_commands: done, delivered_commands: delivered }))
    }

    /// The events of the turn that ends at `end_seq`, from its TurnStarted.
    pub async fn turn_events(&self, session_id: &str, end_seq: u64) -> anyhow::Result<Vec<c6_proto::SessionEvent>> {
        use prost::Message;
        let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? AND seq <= ? ORDER BY seq DESC LIMIT 5000")
            .bind(session_id)
            .bind(end_seq as i64)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let ev = c6_proto::SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?;
            let start = matches!(ev.kind, Some(c6_proto::session_event::Kind::TurnStarted(_)));
            out.push(ev);
            if start {
                break;
            }
        }
        out.reverse();
        Ok(out)
    }

    /// The events since the turn before the one that ends at `end_seq`: the
    /// inputs that turn took (written before its TurnStarted) and the turn.
    pub async fn since_last_turn(&self, session_id: &str, end_seq: u64) -> anyhow::Result<Vec<c6_proto::SessionEvent>> {
        use prost::Message;
        let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? AND seq <= ? ORDER BY seq DESC LIMIT 5000")
            .bind(session_id)
            .bind(end_seq as i64)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let ev = c6_proto::SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?;
            if ev.seq < end_seq && matches!(ev.kind, Some(c6_proto::session_event::Kind::TurnEnded(_))) {
                break;
            }
            out.push(ev);
        }
        out.reverse();
        Ok(out)
    }

    /// Where a window of the log reaching back to event `seq` starts: the
    /// last of your messages at or before it (so its turn shows whole),
    /// looking back at most `lookback` events; else `seq`.
    pub async fn turn_start(&self, session_id: &str, seq: u64, lookback: u64) -> anyhow::Result<u64> {
        if seq <= 1 {
            return Ok(1);
        }
        let lo = seq.saturating_sub(lookback).max(1);
        let page = self.events_after(session_id, lo - 1, (seq - lo + 1) as i64).await?;
        let user = page.iter().rev().find(|e| matches!(e.kind, Some(c6_proto::session_event::Kind::User(_))));
        Ok(user.map_or(seq, |e| e.seq))
    }

    pub async fn last_seq(&self, session_id: &str) -> anyhow::Result<u64> {
        let n: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM events WHERE session_id = ?").bind(session_id).fetch_one(&self.pool).await?;
        Ok(n.unwrap_or(0) as u64)
    }

    /// The session's last event at or before `ts` (0: none).
    pub async fn seq_at(&self, session_id: &str, ts: i64) -> anyhow::Result<u64> {
        let n: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM events WHERE session_id = ? AND ts <= ?")
            .bind(session_id)
            .bind(ts)
            .fetch_one(&self.pool)
            .await?;
        Ok(n.unwrap_or(0) as u64)
    }

    /// The current conversation, not the append-only audit log. Apply
    /// rewinds before callers take a prefix, window or digest: a marker
    /// outside that window can withdraw events inside it.
    pub async fn effective_events(&self, session_id: &str) -> anyhow::Result<Vec<c6_proto::SessionEvent>> {
        Ok(c6_agent::effective_events(&self.events_after(session_id, 0, i64::MAX).await?))
    }

    pub async fn events_after(&self, session_id: &str, after: u64, limit: i64) -> anyhow::Result<Vec<c6_proto::SessionEvent>> {
        use prost::Message;
        let rows = sqlx::query("SELECT data FROM events WHERE session_id = ? AND seq > ? ORDER BY seq LIMIT ?")
            .bind(session_id)
            .bind(after as i64)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|r| Ok(c6_proto::SessionEvent::decode(r.get::<Vec<u8>, _>("data").as_slice())?)).collect()
    }
}

/// Subagent sessions are created by the worker. Accept one when its first
/// event names a parent session that belongs to the same worker.
async fn create_subagent_session(
    tx: &mut sqlx::SqliteConnection,
    worker_id: WorkerId,
    session_id: &str,
    events: &[c6_proto::SessionEvent],
) -> anyhow::Result<bool> {
    let Some(c6_proto::session_event::Kind::Started(s)) = events.first().filter(|e| e.seq == 1).and_then(|e| e.kind.as_ref()) else {
        return Ok(false);
    };
    if s.parent_session_id.is_empty() {
        return Ok(false);
    }
    let parent = sqlx::query(
        "SELECT connection_id, model, effort, labels, worker_name, ssh_target, realm, instance, colour, tools, COALESCE(root_id, id) AS root FROM sessions WHERE id = ? AND worker_id = ?",
    )
    .bind(&s.parent_session_id)
    .bind(worker_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(parent) = parent else { return Ok(false) };
    let now = c6_core::now_ms();
    // It runs on its own connection, model and effort (a subagent may run on
    // another connection than its parent's); a log from before subagents
    // named them: its parent's.
    let own = s.model.as_ref().filter(|m| !m.connection_id.is_empty());
    let connection_id = own.map_or_else(|| parent.get::<String, _>("connection_id"), |m| m.connection_id.clone());
    let effort = match own {
        Some(m) => Some(m.effort.clone()).filter(|e| !e.is_empty()),
        None => parent.get::<Option<String>, _>("effort"),
    };
    sqlx::query(
        "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, effort, parent_id, parent_call_id, \
         labels, worker_name, ssh_target, realm, agent, root_id, observed, instance, colour, tools) VALUES (?, ?, ?, ?, ?, ?, 'starting', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(worker_id.to_string())
    .bind(if s.title.is_empty() { "Subagent" } else { &s.title })
    .bind(&s.cwd)
    .bind(connection_id)
    .bind(s.model.as_ref().map(|m| m.model.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| parent.get::<String, _>("model")))
    .bind(now)
    .bind(now)
    .bind(effort)
    .bind(&s.parent_session_id)
    .bind(&s.parent_call_id)
    .bind(parent.get::<String, _>("labels"))
    .bind(parent.get::<String, _>("worker_name"))
    .bind(parent.get::<String, _>("ssh_target"))
    // A subagent works in its parent's realm.
    .bind(parent.get::<String, _>("realm"))
    .bind(&s.agent)
    .bind(parent.get::<String, _>("root"))
    // Claude Code's own subagents are mirrored, not run.
    .bind(s.tool_profile == "claude_code")
    // And acts as the instance its parent acts as.
    .bind(parent.get::<String, _>("instance"))
    // And has its colour.
    .bind(parent.get::<String, _>("colour"))
    .bind(parent.get::<String, _>("tools"))
    .execute(&mut *tx)
    .await?;
    Ok(true)
}

/// The mark a new top-level session in `realm` takes: the one least used
/// among the realm's most recently active sessions (so the ones you have
/// open tell apart), the first such in `c6_api::marks::all`'s order.
async fn least_used_colour(tx: &mut sqlx::SqliteConnection, realm: &str) -> anyhow::Result<String> {
    let recent: Vec<String> = sqlx::query_scalar(
        "SELECT colour FROM sessions WHERE archived = 0 AND parent_id IS NULL AND realm = ? ORDER BY updated_at DESC LIMIT ?",
    )
    .bind(realm)
    .bind(c6_api::marks::COLOURS.len() as i64)
    .fetch_all(&mut *tx)
    .await?;
    Ok(c6_api::marks::pick(&recent))
}

// --- Commands -----------------------------------------------------------------

pub struct NewCommand<'a> {
    pub id: &'a str,
    pub session_id: &'a str,
    pub worker_id: WorkerId,
    pub data: Vec<u8>,
    pub text: Option<&'a str>,
}

/// Queues `c`: held when asked or while its session has held commands (so
/// it stays behind them), else pending.
async fn insert_command(tx: &mut sqlx::SqliteConnection, c: &NewCommand<'_>, held: bool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO commands (id, session_id, worker_id, data, text, created_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, \
         CASE WHEN ?7 OR EXISTS (SELECT 1 FROM commands WHERE session_id = ?2 AND state = 'held') THEN 'held' ELSE 'pending' END)",
    )
    .bind(c.id)
    .bind(c.session_id)
    .bind(c.worker_id.to_string())
    .bind(&c.data)
    .bind(c.text)
    .bind(c6_core::now_ms())
    .bind(held)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

impl Store {
    /// Rebuild the session-message rate window from durable commands once
    /// on head startup. Pending, delivered, rejected and withdrawn sends
    /// all used a slot. A restore's replay is not a new send.
    pub(crate) async fn recent_session_messages(
        &self,
        since: i64,
    ) -> anyhow::Result<std::collections::HashMap<String, std::collections::VecDeque<i64>>> {
        use c6_proto::{SessionCommand, session_command};
        use prost::Message;

        let rows = sqlx::query(
            "SELECT c.created_at, c.data, previous.data AS previous_data FROM commands c \
             LEFT JOIN commands previous ON previous.seq = c.seq - 1 AND previous.session_id = c.session_id \
             WHERE c.created_at >= ? ORDER BY c.created_at, c.seq",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let mut sent: std::collections::HashMap<String, std::collections::VecDeque<i64>> = Default::default();
        for row in rows {
            let data: Vec<u8> = row.get("data");
            let Ok(SessionCommand { kind: Some(session_command::Kind::Input(input)), .. }) = SessionCommand::decode(data.as_slice()) else {
                continue;
            };
            if input.from_session_id.is_empty() {
                continue;
            }
            // replace_with_restore inserts Restore and its Input together
            // in that order, retaining the original send's command row.
            let previous: Option<Vec<u8>> = row.get("previous_data");
            if previous
                .as_deref()
                .and_then(|data| SessionCommand::decode(data).ok())
                .is_some_and(|c| matches!(c.kind, Some(session_command::Kind::Restore(_))))
            {
                continue;
            }
            sent.entry(input.from_session_id).or_default().push_back(row.get("created_at"));
        }
        Ok(sent)
    }

    pub async fn enqueue_command(&self, c: &NewCommand<'_>) -> anyhow::Result<()> {
        let mut conn = self.pool.acquire().await?;
        insert_command(&mut conn, c, false).await
    }

    /// Retiring delivery obligations and queuing their report are one
    /// transaction. A stable report id makes replay safe even after ack.
    pub async fn enqueue_subagent_report(&self, c: &NewCommand<'_>, inputs: &[String]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM commands WHERE id = ?)").bind(c.id).fetch_one(&mut *tx).await?;
        if !exists {
            insert_command(&mut tx, c, false).await?;
        }
        for input in inputs {
            sqlx::query("UPDATE remote_delegations SET state = 'reported' WHERE command_id = ? AND state = 'waiting'")
                .bind(input)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// A durable, visible queue error, even when the report's parent has
    /// disappeared. It is never sent to a model as if it were user input.
    pub async fn subagent_report_failed(&self, c: &NewCommand<'_>, why: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM commands WHERE id = ?)").bind(c.id).fetch_one(&mut *tx).await?;
        if !exists {
            insert_command(&mut tx, c, false).await?;
        }
        sqlx::query("UPDATE commands SET state = 'rejected', error = ? WHERE id = ?").bind(why).bind(c.id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sessions whose commands are held.
    pub async fn held_sessions(&self) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT DISTINCT session_id FROM commands WHERE state = 'held'").fetch_all(&self.pool).await?)
    }

    /// Hands a session's held commands to delivery, in their order, with
    /// `create` (its Create's id and new data) replacing that one. They are
    /// queued anew, after anything a worker's connection was already sent.
    /// Returns how many there were.
    pub async fn release_held(&self, session_id: &str, create: Option<(&str, &[u8])>) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut held: Vec<CommandRow> = sqlx::query_as("DELETE FROM commands WHERE session_id = ? AND state = 'held' RETURNING *")
            .bind(session_id)
            .fetch_all(&mut *tx)
            .await?;
        held.sort_by_key(|c| c.seq);
        for c in &held {
            let data = match create {
                Some((id, data)) if id == c.id => data,
                _ => &c.data,
            };
            sqlx::query("INSERT INTO commands (id, session_id, worker_id, data, text, created_at) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(&c.id)
                .bind(&c.session_id)
                .bind(&c.worker_id)
                .bind(data)
                .bind(&c.text)
                .bind(c.created_at)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(held.len())
    }

    /// Fails a session whose commands are held, and them with it.
    pub async fn fail_held(&self, session_id: &str, reason: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = c6_core::now_ms();
        sqlx::query("UPDATE sessions SET status = 'failed', error = ?, updated_at = ?, turn_ended_at = ? WHERE id = ?")
            .bind(reason)
            .bind(now)
            .bind(now)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE commands SET state = 'rejected', error = 'session failed to start' WHERE session_id = ? AND state = 'held'")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pending commands for a worker after `after_seq`, in delivery order.
    pub async fn pending_commands(&self, worker_id: WorkerId, after_seq: i64) -> anyhow::Result<Vec<CommandRow>> {
        Ok(sqlx::query_as("SELECT * FROM commands WHERE worker_id = ? AND state = 'pending' AND seq > ? ORDER BY seq LIMIT 500")
            .bind(worker_id.to_string())
            .bind(after_seq)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Inputs not yet applied (or rejected) for a session, oldest first.
    pub async fn queued_inputs(&self, session_id: &str) -> anyhow::Result<Vec<CommandRow>> {
        Ok(sqlx::query_as("SELECT * FROM commands WHERE session_id = ? AND state != 'done' AND text IS NOT NULL ORDER BY seq")
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// The session's queued messages from one of them on, in queue order:
    /// the anchor and every message queued after it (other queued
    /// operations, which carry no text, are left alone). `None` if the
    /// anchor is not a queued message of the session.
    pub async fn pending_tail(&self, session_id: &str, anchor: &str) -> anyhow::Result<Option<Vec<CommandRow>>> {
        let Some(seq) = sqlx::query("SELECT seq FROM commands WHERE id = ? AND session_id = ? AND state = 'pending' AND text IS NOT NULL")
            .bind(anchor)
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.get::<i64, _>(0))
        else {
            return Ok(None);
        };
        let rows = sqlx::query_as(
            "SELECT * FROM commands WHERE session_id = ? AND state = 'pending' AND text IS NOT NULL AND seq >= ? ORDER BY seq",
        )
        .bind(session_id)
        .bind(seq)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(rows))
    }

    /// Marks a pending command rejected. A rejected `Create` fails the
    /// session and everything queued behind it. Returns the session id.
    pub async fn reject_command(&self, worker_id: WorkerId, id: &str, reason: &str) -> anyhow::Result<Option<String>> {
        use prost::Message;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(row) =
            sqlx::query("UPDATE commands SET state = 'rejected', error = ? WHERE id = ? AND worker_id = ? AND state = 'pending' RETURNING session_id, text, data")
                .bind(reason)
                .bind(id)
                .bind(worker_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(None);
        };
        let session_id: String = row.get("session_id");
        let kind = c6_proto::SessionCommand::decode(row.get::<Vec<u8>, _>("data").as_slice()).ok().and_then(|c| c.kind);
        if let Some(c6_proto::session_command::Kind::DelegationEnded(d)) = kind {
            // A rejected report is not a failed Create. Keep its parent
            // usable and show the actual delivery failure in its queue.
            sqlx::query("UPDATE commands SET text = ? WHERE id = ?")
                .bind(format!("Subagent report delivery failed: report from {}. {reason}", d.child_session_id))
                .bind(id)
                .execute(&mut *tx)
                .await?;
        } else if row.get::<Option<String>, _>("text").is_none() {
            let now = c6_core::now_ms();
            sqlx::query("UPDATE sessions SET status = 'failed', error = ?, updated_at = ?, turn_ended_at = ? WHERE id = ?")
                .bind(reason)
                .bind(now)
                .bind(now)
                .bind(&session_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE commands SET state = 'rejected', error = 'session failed to start' WHERE session_id = ? AND state = 'pending'",
            )
            .bind(&session_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Some(session_id))
    }

    /// A pending input's text, creation time and encoded command.
    pub async fn pending_input(&self, command_id: &str) -> anyhow::Result<Option<(String, i64, Vec<u8>)>> {
        Ok(sqlx::query("SELECT text, created_at, data FROM commands WHERE id = ? AND state = 'pending'")
            .bind(command_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| (r.get::<Option<String>, _>("text").unwrap_or_default(), r.get("created_at"), r.get::<Vec<u8>, _>("data"))))
    }

    /// Retires a rejected input and queues a restore followed by a copy of it.
    pub async fn replace_with_restore(&self, old_id: &str, restore: &NewCommand<'_>, input: &NewCommand<'_>) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE commands SET state = 'done' WHERE id = ?").bind(old_id).execute(&mut *tx).await?;
        insert_command(&mut tx, restore, false).await?;
        insert_command(&mut tx, input, false).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Clears rejected inputs from a session's queue (the user dismissed them).
    pub async fn dismiss_rejected(&self, session_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE commands SET state = 'done' WHERE session_id = ? AND state = 'rejected'")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Rewrites a queued input in place (the user edited it). `false` if it
    /// is no longer pending (delivered, or canceled, in the meantime).
    pub async fn update_pending_command(&self, id: &str, data: &[u8], text: Option<&str>) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE commands SET data = ?, text = ? WHERE id = ? AND state = 'pending'")
            .bind(data)
            .bind(text)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Cancels a queued input (the user withdrew it): it is never delivered.
    /// `false` if it is no longer pending.
    pub async fn cancel_pending_command(&self, id: &str) -> anyhow::Result<bool> {
        let r = sqlx::query("UPDATE commands SET state = 'done' WHERE id = ? AND state = 'pending'").bind(id).execute(&self.pool).await?;
        Ok(r.rows_affected() > 0)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A full raw conversation with a withdrawn turn (including a summary),
    /// a replacement, and a second rewind. Used by model-context readers.
    pub(crate) fn rewind_history() -> Vec<c6_proto::SessionEvent> {
        use c6_proto::{AssistantMessage, ContentBlock, SessionEvent, UserMessage, content_block, session_event::Kind};
        let user = |text: &str| Kind::User(UserMessage { text: text.into(), ..Default::default() });
        let answer = |turn, text: &str| {
            Kind::Assistant(AssistantMessage {
                turn,
                blocks: vec![ContentBlock { block: Some(content_block::Block::Text(text.into())), ..Default::default() }],
                ..Default::default()
            })
        };
        [
            Kind::Started(Default::default()),
            user("KEEP original user question"),
            Kind::TurnStarted(c6_proto::TurnStarted { turn: 1, ..Default::default() }),
            answer(1, "KEEP_ANSWER"),
            Kind::TurnEnded(c6_proto::TurnEnded { turn: 1, reason: c6_proto::TurnEndReason::Completed as i32 }),
            user("DROP_PRIVATE withdrawn question"),
            Kind::TurnStarted(c6_proto::TurnStarted { turn: 2, ..Default::default() }),
            answer(2, "PRIVATE_WITHDRAWN_ANSWER"),
            Kind::ToolResult(c6_proto::ToolResult { output: "PRIVATE_TOOL_RESULT".into(), ..Default::default() }),
            Kind::Compaction(c6_proto::Compaction { summary: "PRIVATE_SUMMARY".into(), ..Default::default() }),
            Kind::TurnEnded(c6_proto::TurnEnded { turn: 2, reason: c6_proto::TurnEndReason::Completed as i32 }),
            Kind::Rewound(c6_proto::Rewound { to_seq: 6, ..Default::default() }),
            user("KEEP replacement user question"),
            Kind::TurnStarted(c6_proto::TurnStarted { turn: 3, ..Default::default() }),
            answer(3, "KEEP_REPLACEMENT_ANSWER"),
            Kind::TurnEnded(c6_proto::TurnEnded { turn: 3, reason: c6_proto::TurnEndReason::Completed as i32 }),
            user("DROP_PRIVATE again"),
            answer(4, "PRIVATE_WITHDRAWN_AGAIN"),
            Kind::Rewound(c6_proto::Rewound { to_seq: 17, ..Default::default() }),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SessionEvent { seq: i as u64 + 1, ts_ms: i as i64, kind: Some(kind) })
        .collect()
    }

    pub(crate) async fn insert_events(store: &Store, id: &str, events: &[c6_proto::SessionEvent]) {
        use prost::Message;
        for e in events {
            sqlx::query("INSERT INTO events (session_id, seq, ts, data) VALUES (?, ?, ?, ?)")
                .bind(id)
                .bind(e.seq as i64)
                .bind(e.ts_ms)
                .bind(e.encode_to_vec())
                .execute(&store.pool)
                .await
                .unwrap();
        }
    }

    fn session(id: &str, worker_id: WorkerId) -> SessionRow {
        SessionRow {
            id: id.into(),
            worker_id: worker_id.to_string(),
            title: "t".into(),
            cwd: String::new(),
            connection_id: "c".into(),
            model: "m".into(),
            status: "starting".into(),
            error: None,
            effort: None,
            parent_id: None,
            parent_call_id: None,
            fork_seq: None,
            created_at: 0,
            updated_at: 0,
            last_seq: 0,
            archived: 0,
            labels: "[]".into(),
            worker_name: "w".into(),
            repo: String::new(),
            folder: String::new(),
            home: String::new(),
            ssh_target: String::new(),
            awaiting: 0,
            subagent_model: String::new(),
            subagent_connection_id: String::new(),
            subagent_effort: String::new(),
            tools: "[]".into(),
            realm: "home".into(),
            agent: String::new(),
            end_reason: String::new(),
            root_id: None,
            observed: false,
            instance: String::new(),
            colour: String::new(),
            remote_template: String::new(),
        }
    }

    fn command<'a>(id: &'a str, session_id: &'a str, worker_id: WorkerId, text: Option<&'a str>) -> NewCommand<'a> {
        NewCommand { id, session_id, worker_id, data: id.as_bytes().to_vec(), text }
    }

    #[tokio::test]
    async fn a_head_restart_recovers_a_steered_report_committed_before_its_hook() {
        recover_report_after_ingest("", "steering-call", false).await;
    }

    #[tokio::test]
    async fn a_head_restart_recovers_a_report_from_the_childs_own_task_notice() {
        recover_report_after_ingest("task-notification", "", false).await;
    }

    #[tokio::test]
    async fn a_missing_parent_leaves_a_visible_report_failure_and_pending_obligation() {
        recover_report_after_ingest("", "steering-call", true).await;
    }

    async fn recover_report_after_ingest(command: &str, delegated_by: &str, missing_parent: bool) {
        use c6_proto::{SessionCommand, SessionEvent, TurnEndReason, content_block, session_command, session_event::Kind};
        use prost::Message;

        let dir = tempfile::tempdir().unwrap();
        let config = || crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        };
        let head = crate::start(config()).await.unwrap();
        let w = WorkerId::new();
        if !missing_parent {
            head.state.store.create_session(&session("parent", w), &[], false).await.unwrap();
        }
        let mut child = session("child", w);
        child.parent_id = Some("parent".into());
        child.parent_call_id = Some("original-call".into());
        child.remote_template = "probe".into();
        head.state.store.create_session(&child, &[], false).await.unwrap();
        if !delegated_by.is_empty() {
            head.state.store.add_remote_delegation("input", "child", "parent", "").await.unwrap();
        }
        let kinds = [
            Kind::Started(Default::default()),
            Kind::User(c6_proto::UserMessage {
                command_id: "input".into(),
                command: command.into(),
                delegated_by: delegated_by.into(),
                text: "finish the work".into(),
                ..Default::default()
            }),
            Kind::TurnStarted(c6_proto::TurnStarted { turn: 1, ..Default::default() }),
            Kind::Assistant(c6_proto::AssistantMessage {
                blocks: vec![c6_proto::ContentBlock {
                    block: Some(content_block::Block::Text("The recovered final report.".into())),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            Kind::TurnEnded(c6_proto::TurnEnded { turn: 1, reason: TurnEndReason::Completed as i32 }),
        ];
        let events: Vec<_> = kinds
            .into_iter()
            .enumerate()
            .map(|(i, kind)| SessionEvent { seq: i as u64 + 1, ts_ms: c6_core::now_ms(), kind: Some(kind) })
            .collect();
        // Precisely the crash boundary: the events (and per-turn obligation)
        // commit, but sessions::ingest's relay hook has not run.
        head.state.store.ingest(w, "child", &events).await.unwrap().unwrap();
        assert!(!head.state.store.waiting_delegations("child").await.unwrap().is_empty());
        assert!(head.state.store.pending_commands(w, 0).await.unwrap().is_empty());
        head.shutdown().await.unwrap();
        let head = crate::start(config()).await.unwrap();
        if missing_parent {
            let failures = head.state.store.queued_inputs("child").await.unwrap();
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].state, "rejected");
            assert!(failures[0].text.as_ref().unwrap().contains("Subagent report delivery failed"));
            assert!(!head.state.store.waiting_delegations("child").await.unwrap().is_empty());
            head.state.store.create_session(&session("parent", w), &[], false).await.unwrap();
            crate::remote::recover(&head.state).await.unwrap();
            assert!(head.state.store.waiting_delegations("child").await.unwrap().is_empty());
            assert!(head.state.store.queued_inputs("child").await.unwrap().is_empty(), "successful retry clears the failure");
            assert_eq!(head.state.store.pending_commands(w, 0).await.unwrap().len(), 1);
        } else {
            let reports = head.state.store.pending_commands(w, 0).await.unwrap();
            assert_eq!(reports.len(), 1);
            let Some(session_command::Kind::DelegationEnded(ended)) = SessionCommand::decode(reports[0].data.as_slice()).unwrap().kind
            else {
                panic!("a report")
            };
            assert_eq!(ended.child_session_id, "child");
            assert_eq!(ended.report, "The recovered final report.");
            assert!(ended.wake && ended.call_id.is_empty());
            assert!(head.state.store.waiting_delegations("child").await.unwrap().is_empty());
            crate::remote::recover(&head.state).await.unwrap();
            assert_eq!(head.state.store.pending_commands(w, 0).await.unwrap().len(), 1, "recovery queues one report, not one per replay");
            head.state.store.reject_command(w, &reports[0].id, "parent cannot accept the report").await.unwrap();
            assert_ne!(
                head.state.store.session("parent").await.unwrap().unwrap().status,
                "failed",
                "a rejected report is not a rejected Create"
            );
            let rejected = head.state.store.queued_inputs("parent").await.unwrap();
            assert_eq!(rejected.len(), 1);
            assert!(rejected[0].text.as_ref().unwrap().contains("Subagent report delivery failed"));
            assert!(rejected[0].error.as_ref().unwrap().contains("parent cannot accept"));
        }
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn message_rate_window_counts_senders_states_and_age_but_not_restore_replays() {
        use c6_proto::{SessionCommand, UserInput, session_command};
        use prost::Message;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let worker = WorkerId::new();
        let input = |id: &'static str, from: &str| NewCommand {
            id,
            session_id: "target",
            worker_id: worker,
            data: SessionCommand {
                command_id: id.into(),
                session_id: "target".into(),
                kind: Some(session_command::Kind::Input(UserInput { from_session_id: from.into(), ..Default::default() })),
            }
            .encode_to_vec(),
            text: None,
        };
        for (id, from, state, at) in [
            ("old", "alice", "done", 999),
            ("pending", "alice", "pending", 1_000),
            ("done", "alice", "done", 1_001),
            ("rejected", "alice", "rejected", 1_002),
            ("bob", "bob", "pending", 1_003),
            ("user", "", "pending", 1_004),
        ] {
            let c = input(id, from);
            store.enqueue_command(&c).await.unwrap();
            sqlx::query("UPDATE commands SET created_at = ?, state = ? WHERE id = ?")
                .bind(at)
                .bind(state)
                .bind(id)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        let restore = NewCommand {
            id: "restore",
            session_id: "target",
            worker_id: worker,
            data: SessionCommand { kind: Some(session_command::Kind::Restore(Default::default())), ..Default::default() }.encode_to_vec(),
            text: None,
        };
        store.replace_with_restore("pending", &restore, &input("replay", "alice")).await.unwrap();
        let sent = store.recent_session_messages(1_000).await.unwrap();
        assert_eq!(sent["alice"], [1_000, 1_001, 1_002]);
        assert_eq!(sent["bob"], [1_003]);
        assert_eq!(sent.len(), 2, "person/API inputs do not count as session sends");
    }

    #[tokio::test]
    async fn session_message_limit_survives_a_head_restart_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let config = || crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        };
        let head = crate::start(config()).await.unwrap();
        let worker = WorkerId::new();
        let (alice, bob) = (session("alice", worker), session("bob", worker));
        for s in [&alice, &bob] {
            head.state.store.create_session(s, &[], false).await.unwrap();
        }
        let limits = c6_api::SubagentLimits { messages_per_hour: 2, ..Default::default() };
        head.state.store.set_setting(crate::sessions::SUBAGENT_LIMITS, &serde_json::to_string(&limits).unwrap()).await.unwrap();
        let message = serde_json::json!({"to": "bob", "message": "queued while offline"});
        for _ in 0..2 {
            crate::sessions::message_from(&head.state, &alice, &message).await.unwrap();
        }
        assert!(crate::sessions::message_from(&head.state, &alice, &message).await.unwrap_err().to_string().contains("last hour"));
        head.shutdown().await.unwrap();

        let head = crate::start(config()).await.unwrap();
        assert!(crate::sessions::message_from(&head.state, &alice, &message).await.unwrap_err().to_string().contains("last hour"));
        // The window belongs to the sending session, not the recipient.
        crate::sessions::message_from(&head.state, &bob, &serde_json::json!({"to": "alice", "message": "reply"})).await.unwrap();
        sqlx::query("UPDATE commands SET created_at = ?")
            .bind(c6_core::now_ms() - 3_600_001)
            .execute(&head.state.store.pool)
            .await
            .unwrap();
        head.shutdown().await.unwrap();

        let head = crate::start(config()).await.unwrap();
        crate::sessions::message_from(&head.state, &alice, &message).await.unwrap();
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn held_commands_wait_and_then_come_after_what_was_sent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let w = WorkerId::new();
        let ids = |rows: &[CommandRow]| rows.iter().map(|c| c.id.clone()).collect::<Vec<_>>();

        store
            .create_session(&session("a", w), &[command("a-create", "a", w, None), command("a-input", "a", w, Some("hi"))], true)
            .await
            .unwrap();
        // Queued behind a held start: held too.
        store.enqueue_command(&command("a-more", "a", w, Some("and"))).await.unwrap();
        // Another session's, sent while a's are held.
        store.create_session(&session("b", w), &[command("b-create", "b", w, None)], false).await.unwrap();
        let sent = store.pending_commands(w, 0).await.unwrap();
        assert_eq!(ids(&sent), ["b-create"]);
        assert_eq!(ids(&store.queued_inputs("a").await.unwrap()), ["a-input", "a-more"], "held inputs show as queued");
        assert_eq!(store.held_sessions().await.unwrap(), ["a"]);

        let created_at = store.queued_inputs("a").await.unwrap()[0].created_at;
        assert_eq!(store.release_held("a", Some(("a-create", b"with brief"))).await.unwrap(), 3);
        let after = store.pending_commands(w, sent[0].seq).await.unwrap();
        assert_eq!(ids(&after), ["a-create", "a-input", "a-more"], "in order, after what was sent");
        assert_eq!(after[0].data, b"with brief");
        assert_eq!(after[1].data, b"a-input");
        assert_eq!(after[1].created_at, created_at);
        assert!(store.held_sessions().await.unwrap().is_empty());
        assert_eq!(store.release_held("a", None).await.unwrap(), 0, "released once");
        store.enqueue_command(&command("a-later", "a", w, None)).await.unwrap();
        assert_eq!(store.pending_commands(w, after[2].seq).await.unwrap()[0].id, "a-later", "no longer held");
    }

    #[tokio::test]
    async fn a_failed_hand_over_fails_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let w = WorkerId::new();
        store
            .create_session(&session("a", w), &[command("a-create", "a", w, None), command("a-input", "a", w, Some("hi"))], true)
            .await
            .unwrap();
        store.fail_held("a", "could not hand it to its worker: disk full").await.unwrap();
        let s = store.session("a").await.unwrap().unwrap();
        assert_eq!((s.status.as_str(), s.error.as_deref()), ("failed", Some("could not hand it to its worker: disk full")));
        let queued = store.queued_inputs("a").await.unwrap();
        assert_eq!((queued.len(), queued[0].state.as_str()), (1, "rejected"));
        assert!(store.held_sessions().await.unwrap().is_empty());
        assert!(store.pending_commands(w, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failure_is_seen_once_you_look_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let w = WorkerId::new();
        store.create_session(&session("a", w), &[command("a-create", "a", w, None)], true).await.unwrap();
        assert_eq!(store.turn_seen("a").await.unwrap(), (0, 0));
        assert!(!store.mark_seen("a", c6_core::now_ms()).await.unwrap(), "nothing ended yet");
        store.fail_held("a", "could not hand it to its worker").await.unwrap();
        let (ended, seen) = store.turn_seen("a").await.unwrap();
        assert!(ended > 0 && seen == 0, "{ended} {seen}");
        assert!(store.mark_seen("a", ended).await.unwrap());
        assert!(!store.mark_seen("a", ended + 5).await.unwrap(), "already seen");
        assert_eq!(store.turn_seen("a").await.unwrap(), (ended, ended));
    }
}
