//! carbon6's own prompts (docs/os.md §4.6): every instruction carbon6 gives
//! a model is a record of the `prompts` system collection, in root, which
//! a realm can override by name. carbon6 ships a default for each; a
//! prompt nobody has edited follows the default when it changes, and an
//! edited one is never touched.

use c6_api as api;
use c6_api::prompts as text;
use serde_json::{Map, Value, json};
use sqlx::Row;

use crate::HeadState;
use crate::realms::ROOT;

/// The collection, and the settings key that reads and writes it.
pub const PROMPTS: &str = "prompts";

pub struct Prompt {
    pub name: &'static str,
    /// What it is for, and the placeholders it may use.
    pub description: &'static str,
    pub default: &'static str,
}

/// Added to the base prompt of side chats.
pub const SIDE_CHAT_NOTE: &str = "This is a side chat: the user branched off the conversation above to ask about it. \
The main session continues on its own and does not see this chat. Answer the user's questions here; \
look at files and run read-only commands as needed, but do not change files unless the user asks you to.";

pub const DEFAULTS: &[Prompt] = &[
    Prompt {
        name: "session.base",
        description: "What every agent session is told first (the environment and project instructions follow).",
        default: c6_agent::BASE_PROMPT,
    },
    Prompt { name: "session.side_chat", description: "Added for side chats.", default: SIDE_CHAT_NOTE },
    Prompt { name: "session.subagent", description: "Added to a subagent's task.", default: c6_agent::SUBAGENT_NOTE },
    Prompt {
        name: "session.compact",
        description: "What a summary must hold when a conversation is compacted.",
        default: c6_agent::COMPACT_PROMPT,
    },
    Prompt {
        name: "session.compact.full",
        description: "Why the conversation is compacted, when the context window is nearly full.",
        default: c6_agent::COMPACT_WHY,
    },
    Prompt {
        name: "session.compact.asked",
        description: "Why the conversation is compacted, when the user asked (/compact).",
        default: c6_agent::COMPACT_WHY_ASKED,
    },
    Prompt {
        name: "assistant.base",
        description: "The assistant's instructions. Placeholders: {{areas}}, {{context}} (what the user is looking at), {{today}}.",
        default: crate::assistant::PROMPT,
    },
    Prompt {
        name: "todo.session",
        description: "The first message of a session started from an action item. Placeholders: {{id}}, {{kind}}, {{title}}, {{detail}}, {{context}}.",
        default: text::TODO_SESSION,
    },
    Prompt {
        name: "memex.brief.knowledge",
        description: "What a session's brief says before its environment knowledge.",
        default: text::BRIEF_KNOWLEDGE,
    },
    Prompt {
        name: "memex.understand.system",
        description: "The system prompt of the memex's reading tasks.",
        default: text::UNDERSTAND_SYSTEM,
    },
    Prompt {
        name: "memex.understand.thread",
        description: "How to read an email thread. Placeholder: {{owner}} (your addresses).",
        default: text::UNDERSTAND_THREAD,
    },
    Prompt { name: "memex.understand.paper", description: "How to read a research paper.", default: text::UNDERSTAND_PAPER },
    Prompt { name: "memex.understand.session", description: "How to read a work session.", default: text::UNDERSTAND_SESSION },
    Prompt { name: "memex.understand.other", description: "How to read anything else.", default: text::UNDERSTAND_OTHER },
    Prompt {
        name: "memex.understand.thing",
        description: "How to sum up what is known about a thing. Placeholders: {{name}}, {{kind}}, {{records}}.",
        default: text::UNDERSTAND_THING,
    },
    Prompt { name: "memex.page.system", description: "The system prompt for writing a system's page.", default: text::PAGE_SYSTEM },
    Prompt {
        name: "memex.page",
        description: "How to write a system's page from its facts. Placeholders: {{name}}, {{key}}, {{facts}}.",
        default: text::PAGE,
    },
    Prompt { name: "memex.look.system", description: "The system prompt for looking at photos and videos.", default: text::LOOK_SYSTEM },
    Prompt { name: "memex.look", description: "How to describe a photo or video.", default: text::LOOK },
];

fn default_of(name: &str) -> &'static str {
    DEFAULTS.iter().find(|p| p.name == name).map(|p| p.default).unwrap_or("")
}

/// Adds the defaults to root's prompts; one nobody has edited follows its
/// default when that changes (kept apart by `updated_by`).
pub async fn seed(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let now = c6_core::now_ms();
    for (position, p) in DEFAULTS.iter().enumerate() {
        let fields = json!({"name": p.name, "description": p.description, "body": p.default});
        let current: Option<(String, String, i64, String)> = sqlx::query_as(
            "SELECT id, fields, version, updated_by FROM records WHERE realm = ? AND collection = ? AND json_extract(fields, '$.name') = ?",
        )
        .bind(ROOT)
        .bind(PROMPTS)
        .bind(p.name)
        .fetch_optional(pool)
        .await?;
        match current {
            None => {
                sqlx::query(
                    "INSERT INTO records (id, realm, collection, fields, version, created_by, updated_by, created_at, updated_at, position) \
                     VALUES (?, ?, ?, ?, 1, 'system', 'system', ?, ?, ?)",
                )
                .bind(uuid::Uuid::now_v7().to_string())
                .bind(ROOT)
                .bind(PROMPTS)
                .bind(fields.to_string())
                .bind(now)
                .bind(now)
                .bind(position as i64)
                .execute(pool)
                .await?;
            }
            Some((id, old, version, by)) if by == "system" && serde_json::from_str::<Value>(&old).ok().as_ref() != Some(&fields) => {
                sqlx::query("UPDATE records SET fields = ?, version = ?, updated_at = ? WHERE id = ?")
                    .bind(fields.to_string())
                    .bind(version + 1)
                    .bind(now)
                    .bind(&id)
                    .execute(pool)
                    .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Every prompt as `realm` uses it (root's, overridden by the realm's), by
/// name; a prompt missing from both is its default.
pub async fn all(st: &HeadState, realm: &str) -> Map<String, Value> {
    let mut out: Map<String, Value> = DEFAULTS.iter().map(|p| (p.name.to_owned(), json!(p.default))).collect();
    for e in crate::lists::effective(&st.store.pool, PROMPTS, realm).await.unwrap_or_default() {
        if let (Some(n), Some(b)) = (e["name"].as_str(), e["body"].as_str())
            && !b.trim().is_empty()
        {
            out.insert(n.to_owned(), json!(b));
        }
    }
    out
}

/// One prompt as `realm` uses it.
pub async fn get(st: &HeadState, name: &str, realm: &str) -> String {
    all(st, realm).await.get(name).and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| default_of(name).to_owned())
}

/// The prompts a worker gives the model itself, for a session's spec.
pub async fn for_worker(st: &HeadState, realm: &str) -> String {
    let all = all(st, realm).await;
    let picked: Map<String, Value> = all.into_iter().filter(|(k, _)| k == "session.subagent" || k.starts_with("session.compact")).collect();
    Value::Object(picked).to_string()
}

/// The memex's prompts (it works on home's knowledge).
pub async fn for_memex(st: &HeadState) -> Value {
    let all = all(st, crate::realms::HOME).await;
    Value::Object(all.into_iter().filter(|(k, _)| k.starts_with("memex.")).collect())
}

/// What a model is told when a conversation in `realm` is compacted.
pub async fn compact(st: &HeadState, realm: &str) -> c6_agent::CompactPrompts {
    let all = all(st, realm).await;
    let s = |k: &str, d: &str| all.get(k).and_then(Value::as_str).unwrap_or(d).to_owned();
    c6_agent::CompactPrompts {
        why_full: s("session.compact.full", c6_agent::COMPACT_WHY),
        why_asked: s("session.compact.asked", c6_agent::COMPACT_WHY_ASKED),
        instructions: s("session.compact", c6_agent::COMPACT_PROMPT),
    }
}

/// The prompts with their defaults, for Settings: root's (or a realm's)
/// text, and whether it was changed from the default.
pub async fn list(st: &HeadState) -> anyhow::Result<Vec<api::PromptInfo>> {
    let rows = sqlx::query("SELECT fields, updated_by FROM records WHERE realm = ? AND collection = ? ORDER BY position")
        .bind(ROOT)
        .bind(PROMPTS)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(DEFAULTS
        .iter()
        .map(|p| {
            let row = rows
                .iter()
                .find(|r| serde_json::from_str::<Value>(&r.get::<String, _>("fields")).ok().is_some_and(|f| f["name"] == p.name));
            let body = row
                .and_then(|r| serde_json::from_str::<Value>(&r.get::<String, _>("fields")).ok())
                .and_then(|f| f["body"].as_str().map(str::to_owned))
                .unwrap_or_else(|| p.default.to_owned());
            api::PromptInfo {
                name: p.name.into(),
                description: p.description.into(),
                edited: body != p.default,
                body,
                default: p.default.into(),
            }
        })
        .collect())
}

/// Changes root's text of a prompt (an empty text, or the default's,
/// returns it to following the default).
pub async fn set(st: &HeadState, name: &str, body: &str) -> anyhow::Result<()> {
    let p = DEFAULTS.iter().find(|p| p.name == name).ok_or_else(|| crate::not_found(format!("no prompt {name:?}")))?;
    let back_to_default = body.trim().is_empty() || body == p.default;
    let body = if back_to_default { p.default } else { body };
    let fields = json!({"name": p.name, "description": p.description, "body": body});
    let (id, version): (String, i64) =
        sqlx::query_as("SELECT id, version FROM records WHERE realm = ? AND collection = ? AND json_extract(fields, '$.name') = ?")
            .bind(ROOT)
            .bind(PROMPTS)
            .bind(name)
            .fetch_one(&st.store.pool)
            .await?;
    let by = if back_to_default { "system" } else { "you" };
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE records SET fields = ?, version = ?, updated_by = ?, updated_at = ? WHERE id = ?")
        .bind(fields.to_string())
        .bind(version + 1)
        .bind(by)
        .bind(c6_core::now_ms())
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT OR REPLACE INTO record_history (record_id, version, op, fields, changed_by, changed_at) VALUES (?, ?, 'update', ?, ?, ?)",
    )
    .bind(&id)
    .bind(version + 1)
    .bind(fields.to_string())
    .bind(if back_to_default { "you (back to the default)" } else { "you" })
    .bind(c6_core::now_ms())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::memex::reload(st).await;
    Ok(())
}
