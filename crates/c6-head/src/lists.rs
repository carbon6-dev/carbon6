//! carbon6's own configuration lists as system collections (docs/os.md
//! §12.2): skills, agents, commands, hooks, MCP servers, templates,
//! workflows, policies, model routes, prices, budgets, notification
//! channels, hosts, memex folders and scopes, and memory notes. Each list
//! lives in a realm (restrictions in root, the rest in home); a realm uses
//! root's entries plus its own (`effective`). The head's settings store
//! reads and writes these lists here, so every setting's endpoint and form
//! keeps working while each entry is a record with its own history.

use serde_json::{Value, json};
use sqlx::Row;

use crate::realms::{HOME, ROOT};

/// How a realm's entries combine with root's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    /// A realm's entry replaces root's with the same key; root's come first.
    Override,
    /// Root's, then the realm's (all apply; root's restrictions first).
    RootFirst,
    /// The realm's, then root's (the first match wins: the realm's own first).
    RealmFirst,
}

pub struct List {
    /// The settings key, and the collection's name.
    pub key: &'static str,
    pub description: &'static str,
    /// Where the list lives by default (and where the settings endpoints
    /// read and write it).
    pub realm: &'static str,
    /// The fields that identify an entry (for history, and for overriding).
    pub key_fields: &'static [&'static str],
    pub merge: Merge,
}

pub const LISTS: &[List] = &[
    List {
        key: "skills",
        description: "Skills: instructions agents load by name when they apply.",
        realm: HOME,
        key_fields: &["name", "applies_to"],
        merge: Merge::Override,
    },
    List {
        key: "agents",
        description: "Agents: subagents the model can launch.",
        realm: HOME,
        key_fields: &["name", "applies_to"],
        merge: Merge::Override,
    },
    List { key: "commands", description: "Slash commands.", realm: HOME, key_fields: &["name", "applies_to"], merge: Merge::Override },
    List {
        key: "hooks",
        description: "Hooks: commands run at points in each session.",
        realm: HOME,
        key_fields: &["name", "event", "matcher", "command"],
        merge: Merge::RootFirst,
    },
    List { key: "mcp_servers", description: "MCP servers given to sessions.", realm: HOME, key_fields: &["name"], merge: Merge::Override },
    List {
        key: "templates",
        description: "Templates: saved ways to start sessions.",
        realm: HOME,
        key_fields: &["name"],
        merge: Merge::Override,
    },
    List { key: "workflows", description: "Workflows: steps run in order.", realm: HOME, key_fields: &["name"], merge: Merge::Override },
    List {
        key: "policies",
        description: "Policies: ask or deny agents' tool calls.",
        realm: ROOT,
        key_fields: &["name"],
        merge: Merge::RootFirst,
    },
    List {
        key: "routes",
        description: "Model routing: which connection and model a session gets.",
        realm: ROOT,
        key_fields: &["name"],
        merge: Merge::RealmFirst,
    },
    List { key: "prices", description: "What model calls cost.", realm: ROOT, key_fields: &["model"], merge: Merge::Override },
    List { key: "budgets", description: "Spending limits.", realm: ROOT, key_fields: &["name"], merge: Merge::RootFirst },
    List { key: "channels", description: "Notification channels.", realm: HOME, key_fields: &["name"], merge: Merge::Override },
    List { key: "hosts", description: "Machines reached over SSH.", realm: HOME, key_fields: &["name"], merge: Merge::Override },
    List {
        key: "memex_folders",
        description: "Folders the memex keeps in sync.",
        realm: HOME,
        key_fields: &["path"],
        merge: Merge::Override,
    },
    List {
        key: "memex_scopes",
        description: "What sessions may see in the memex.",
        realm: HOME,
        key_fields: &["name"],
        merge: Merge::Override,
    },
    List {
        key: "views",
        description: "Views: the screens of a realm's menu (tables, boards, charts, feeds, pages).",
        realm: HOME,
        key_fields: &["name"],
        merge: Merge::Override,
    },
    List {
        key: "prompts",
        description: "What carbon6 tells models: sessions, the assistant, the memex.",
        realm: ROOT,
        key_fields: &["name"],
        merge: Merge::Override,
    },
];

pub fn list(key: &str) -> Option<&'static List> {
    LISTS.iter().find(|l| l.key == key)
}

/// An entry's identity, from its key fields.
fn identity(l: &List, entry: &Value) -> String {
    l.key_fields.iter().map(|k| entry.get(*k).map(|v| v.to_string()).unwrap_or_default()).collect::<Vec<_>>().join("\u{1f}")
}

/// The list's entries in one realm, in order, as a JSON array.
pub async fn read(pool: &sqlx::SqlitePool, key: &str, realm: &str) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query("SELECT fields FROM records WHERE realm = ? AND collection = ? ORDER BY position, created_at, id")
        .bind(realm)
        .bind(key)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().filter_map(|r| serde_json::from_str(&r.get::<String, _>("fields")).ok()).collect())
}

/// What a realm uses: root's entries and its own, combined by the list's
/// rule.
pub async fn effective(pool: &sqlx::SqlitePool, key: &str, realm: &str) -> anyhow::Result<Vec<Value>> {
    let Some(l) = list(key) else { return Ok(Vec::new()) };
    let root = read(pool, key, ROOT).await?;
    if realm == ROOT {
        return Ok(root);
    }
    let own = read(pool, key, realm).await?;
    Ok(match l.merge {
        Merge::RootFirst => root.into_iter().chain(own).collect(),
        Merge::RealmFirst => own.into_iter().chain(root).collect(),
        Merge::Override => {
            let mine: std::collections::HashSet<String> = own.iter().map(|e| identity(l, e)).collect();
            root.into_iter().filter(|e| !mine.contains(&identity(l, e))).chain(own).collect()
        }
    })
}

/// Replaces the list's entries in one realm with `entries`, in order:
/// entries that stay (by identity) are updated in place, keeping their
/// history; the rest are added or deleted.
pub async fn write(pool: &sqlx::SqlitePool, key: &str, realm: &str, entries: &[Value], by: &str) -> anyhow::Result<()> {
    let Some(l) = list(key) else { anyhow::bail!("{key} is not a system list") };
    let now = c6_core::now_ms();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let rows = sqlx::query("SELECT id, fields, version, position FROM records WHERE realm = ? AND collection = ?")
        .bind(realm)
        .bind(key)
        .fetch_all(&mut *tx)
        .await?;
    // Existing entries by identity (a list may repeat one: first come, first matched).
    let mut existing: Vec<(String, String, Value, i64, i64)> = rows
        .iter()
        .map(|r| {
            let fields: Value = serde_json::from_str(&r.get::<String, _>("fields")).unwrap_or(Value::Null);
            (identity(l, &fields), r.get("id"), fields, r.get("version"), r.get("position"))
        })
        .collect();
    for (position, entry) in entries.iter().enumerate() {
        let id = identity(l, entry);
        match existing.iter().position(|(i, ..)| *i == id) {
            Some(at) => {
                let (_, rid, fields, version, pos) = existing.remove(at);
                if fields != *entry || pos != position as i64 {
                    sqlx::query("UPDATE records SET fields = ?, version = ?, position = ?, updated_by = ?, updated_at = ? WHERE id = ?")
                        .bind(entry.to_string())
                        .bind(version + 1)
                        .bind(position as i64)
                        .bind(by)
                        .bind(now)
                        .bind(&rid)
                        .execute(&mut *tx)
                        .await?;
                    if fields != *entry {
                        history(&mut tx, &rid, version + 1, "update", entry, by, now).await?;
                    }
                }
            }
            None => {
                let rid = uuid::Uuid::now_v7().to_string();
                sqlx::query(
                    "INSERT INTO records (id, realm, collection, fields, version, created_by, updated_by, created_at, updated_at, position) \
                     VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?)",
                )
                .bind(&rid)
                .bind(realm)
                .bind(key)
                .bind(entry.to_string())
                .bind(by)
                .bind(by)
                .bind(now)
                .bind(now)
                .bind(position as i64)
                .execute(&mut *tx)
                .await?;
                history(&mut tx, &rid, 1, "create", entry, by, now).await?;
            }
        }
    }
    for (_, rid, fields, version, _) in existing {
        sqlx::query("DELETE FROM records WHERE id = ?").bind(&rid).execute(&mut *tx).await?;
        history(&mut tx, &rid, version + 1, "delete", &fields, by, now).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn history(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    version: i64,
    op: &str,
    fields: &Value,
    by: &str,
    at: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO record_history (record_id, version, op, fields, changed_by, changed_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(id)
        .bind(version)
        .bind(op)
        .bind(fields.to_string())
        .bind(by)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Memory notes: a system collection too, kept by `memory` (each note a
/// record with its own id), and not a settings list.
pub const MEMORY: &str = "memory";

/// Makes sure every system collection exists in root and home, with the
/// schema of today's code (code owns these schemas, so they follow it).
pub async fn ensure(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let now = c6_core::now_ms();
    let memory = [(MEMORY, "Memory notes: durable notes sessions start with, scoped by a label.")];
    for (key, description) in LISTS.iter().map(|l| (l.key, l.description)).chain(memory) {
        let l = (key, description);
        let schema = crate::catalog::list_schema(l.0).unwrap_or_else(|| json!({"type": "object", "additionalProperties": true}));
        let realms: Vec<String> = sqlx::query_scalar("SELECT id FROM realms").fetch_all(pool).await?;
        for realm in realms.iter().map(String::as_str).chain([ROOT, HOME]) {
            sqlx::query(
                "INSERT INTO collections (realm, name, description, schema, keys, system, created_by, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, '[]', 1, 'system', ?, ?) \
                 ON CONFLICT (realm, name) DO UPDATE SET description = excluded.description, schema = excluded.schema, system = 1",
            )
            .bind(realm)
            .bind(l.0)
            .bind(l.1)
            .bind(schema.to_string())
            .bind(now)
            .bind(now)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}
