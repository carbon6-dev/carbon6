//! Durable head state (SQLite via sqlx).

use std::path::Path;
use std::str::FromStr;

use c6_core::WorkerId;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::{FromRow, Row};

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: SqlitePool,
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
pub const PER_REALM: &[&str] = &["session_suggestions", "web_search", "assistant", "subagent_limits", "brief"];

fn own_key(key: &str, realm: &str) -> String {
    format!("{key}@{realm}")
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
        Ok((Self { pool }, report))
    }

    /// Earlier upgrades of this database, newest first.
    pub async fn upgrades(&self) -> anyhow::Result<Vec<c6_core::upgrade::Report>> {
        c6_core::upgrade::reports(&self.pool).await
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
    pub created_at: i64,
    /// The realms it is available to (JSON array; empty: all).
    pub realms: String,
}

impl Store {
    pub async fn insert_connection(&self, c: &ConnectionRow) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO connections (id, name, kind, base_url, api_key, default_model, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&c.id)
            .bind(&c.name)
            .bind(&c.kind)
            .bind(&c.base_url)
            .bind(&c.api_key)
            .bind(&c.default_model)
            .bind(c.created_at)
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
}

/// What an event batch changed.
pub struct Ingested {
    pub last_seq: u64,
    pub events: Vec<c6_proto::SessionEvent>,
    /// Commands the worker has now applied.
    pub done_commands: Vec<String>,
}

impl Store {
    /// Creates a session and enqueues its commands atomically. Returns its
    /// colour (picked here when `s` has none).
    pub async fn create_session(&self, s: &SessionRow, commands: &[NewCommand<'_>]) -> anyhow::Result<String> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let colour = match (s.colour.is_empty(), &s.parent_id) {
            (true, None) => least_used_colour(&mut tx, &s.realm).await?,
            _ => s.colour.clone(),
        };
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, effort, parent_id, fork_seq, \
             labels, worker_name, ssh_target, subagent_model, realm, root_id, instance, colour, subagent_connection_id, subagent_effort) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .execute(&mut *tx)
        .await?;
        for c in commands {
            insert_command(&mut tx, c).await?;
        }
        tx.commit().await?;
        Ok(colour)
    }

    pub async fn session(&self, id: &str) -> anyhow::Result<Option<SessionRow>> {
        Ok(sqlx::query_as("SELECT * FROM sessions WHERE id = ?").bind(id).fetch_optional(&self.pool).await?)
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

    /// Subagents working now, per top-level session.
    pub async fn subagents_running(&self) -> anyhow::Result<std::collections::HashMap<String, u32>> {
        let rows = sqlx::query(
            "SELECT root_id, COUNT(*) AS n FROM sessions WHERE root_id IS NOT NULL AND parent_call_id IS NOT NULL \
             AND status = 'running' GROUP BY root_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get::<String, _>("root_id"), r.get::<i64, _>("n") as u32)).collect())
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
        let mut row = sqlx::query("SELECT last_seq, status FROM sessions WHERE id = ? AND worker_id = ?")
            .bind(session_id)
            .bind(worker_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if row.is_none() && create_subagent_session(&mut tx, worker_id, session_id, events).await? {
            row = sqlx::query("SELECT last_seq, status FROM sessions WHERE id = ?").bind(session_id).fetch_optional(&mut *tx).await?;
        }
        let Some(row) = row else {
            return Ok(None);
        };
        let mut last = row.get::<i64, _>("last_seq") as u64;
        let mut status: String = row.get("status");
        let mut stored = Vec::new();
        let mut done = Vec::new();
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
                Some(Kind::TurnStarted(_)) => status = "running".into(),
                Some(Kind::TurnEnded(t)) => {
                    status = "idle".into();
                    let reason = match c6_proto::TurnEndReason::try_from(t.reason) {
                        Ok(c6_proto::TurnEndReason::Completed) => "completed",
                        Ok(c6_proto::TurnEndReason::Interrupted) => "interrupted",
                        Ok(c6_proto::TurnEndReason::WorkerRestarted) => "worker_restarted",
                        _ => "error",
                    };
                    // Nothing waits past its turn.
                    sqlx::query("UPDATE sessions SET awaiting = 0, end_reason = ? WHERE id = ?")
                        .bind(reason)
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
        }
        tx.commit().await?;
        Ok(Some(Ingested { last_seq: last, events: stored, done_commands: done }))
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
        "SELECT connection_id, model, effort, labels, worker_name, ssh_target, realm, instance, colour, COALESCE(root_id, id) AS root FROM sessions WHERE id = ? AND worker_id = ?",
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
         labels, worker_name, ssh_target, realm, agent, root_id, observed, instance, colour) VALUES (?, ?, ?, ?, ?, ?, 'starting', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .execute(&mut *tx)
    .await?;
    Ok(true)
}

/// The colour a new top-level session in `realm` takes: the one least used
/// among the realm's most recently active sessions (so the ones you have
/// open tell apart), the first such in `SESSION_COLOURS`' order.
async fn least_used_colour(tx: &mut sqlx::SqliteConnection, realm: &str) -> anyhow::Result<String> {
    let recent: Vec<String> = sqlx::query_scalar(
        "SELECT colour FROM sessions WHERE archived = 0 AND parent_id IS NULL AND realm = ? ORDER BY updated_at DESC LIMIT ?",
    )
    .bind(realm)
    .bind(c6_api::SESSION_COLOURS.len() as i64)
    .fetch_all(&mut *tx)
    .await?;
    Ok(pick_colour(&recent).to_owned())
}

/// The first of `SESSION_COLOURS` used least often in `taken`.
pub fn pick_colour(taken: &[String]) -> &'static str {
    c6_api::SESSION_COLOURS.into_iter().min_by_key(|c| taken.iter().filter(|t| t == c).count()).unwrap_or("sky")
}

// --- Commands -----------------------------------------------------------------

pub struct NewCommand<'a> {
    pub id: &'a str,
    pub session_id: &'a str,
    pub worker_id: WorkerId,
    pub data: Vec<u8>,
    pub text: Option<&'a str>,
}

async fn insert_command(tx: &mut sqlx::SqliteConnection, c: &NewCommand<'_>) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO commands (id, session_id, worker_id, data, text, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(c.id)
        .bind(c.session_id)
        .bind(c.worker_id.to_string())
        .bind(&c.data)
        .bind(c.text)
        .bind(c6_core::now_ms())
        .execute(&mut *tx)
        .await?;
    Ok(())
}

impl Store {
    pub async fn enqueue_command(&self, c: &NewCommand<'_>) -> anyhow::Result<()> {
        let mut conn = self.pool.acquire().await?;
        insert_command(&mut conn, c).await
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

    /// Marks a pending command rejected. A rejected `Create` fails the
    /// session and everything queued behind it. Returns the session id.
    pub async fn reject_command(&self, worker_id: WorkerId, id: &str, reason: &str) -> anyhow::Result<Option<String>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(row) =
            sqlx::query("UPDATE commands SET state = 'rejected', error = ? WHERE id = ? AND worker_id = ? AND state = 'pending' RETURNING session_id, text")
                .bind(reason)
                .bind(id)
                .bind(worker_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(None);
        };
        let session_id: String = row.get("session_id");
        if row.get::<Option<String>, _>("text").is_none() {
            sqlx::query("UPDATE sessions SET status = 'failed', error = ?, updated_at = ? WHERE id = ?")
                .bind(reason)
                .bind(c6_core::now_ms())
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

    /// A pending input's text and encoded command.
    pub async fn pending_input(&self, command_id: &str) -> anyhow::Result<Option<(String, Vec<u8>)>> {
        Ok(sqlx::query("SELECT text, data FROM commands WHERE id = ? AND state = 'pending'")
            .bind(command_id)
            .fetch_optional(&self.pool)
            .await?
            .and_then(|r| Some((r.get::<Option<String>, _>("text")?, r.get::<Vec<u8>, _>("data")))))
    }

    /// Retires a rejected input and queues a restore followed by a copy of it.
    pub async fn replace_with_restore(&self, old_id: &str, restore: &NewCommand<'_>, input: &NewCommand<'_>) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE commands SET state = 'done' WHERE id = ?").bind(old_id).execute(&mut *tx).await?;
        insert_command(&mut tx, restore).await?;
        insert_command(&mut tx, input).await?;
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
}
