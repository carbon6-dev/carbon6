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
        name: "session.subagent_resume",
        description: "Sent to a subagent when you resume it after a turn that did not finish.",
        default: c6_agent::SUBAGENT_RESUME,
    },
    Prompt {
        name: "session.subagent_partial",
        description: "Told to an agent after the summary of a subagent that did not finish.",
        default: c6_agent::SUBAGENT_PARTIAL,
    },
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
        name: "session.suggest",
        description: "How a session's next message is suggested (docs/suggestions.md). Placeholders: {{title}}, {{labels}}, {{exchanges}} (the last exchanges).",
        default: text::SUGGEST,
    },
    Prompt {
        name: "session.task_cards",
        description: "When agents propose task cards: work for a separate session, started with one click (docs/task-cards.md).",
        default: text::TASK_CARDS,
    },
    Prompt {
        name: "assistant.base",
        description: "The assistant's instructions. Placeholders: {{areas}}, {{context}} (what the user is looking at), {{today}}, {{realm}} (the realm it works in).",
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
    Prompt {
        name: "memex.triage",
        description: "How mail and chat from people you know are sorted for the brief (what needs you, or a pitch). Mail comes with its history with the sender (counted from your mail) and what the rules noticed. Placeholders: {{owner}}, {{examples}} (your corrections), {{urgent}}.",
        default: text::TRIAGE,
    },
    Prompt {
        name: "memex.relations.card",
        description: "How a person's or organization's card is written: who they are to you, from your mail's history with them and their threads' summaries; grown as new threads come. Placeholders: {{who}}, {{history}}, {{threads}}, {{previous}}.",
        default: text::RELATION_CARD,
    },
    Prompt {
        name: "memex.triage.pitch",
        description: "How mail from someone you've never written to (nor anyone at their company) is sorted: a cold pitch, a real first contact, a newsletter. It comes with the history with the sender and what the rules noticed. Placeholders: {{owner}}, {{examples}}.",
        default: text::TRIAGE_PITCH,
    },
    Prompt {
        name: "memex.triage.urgent",
        description: "What may interrupt you: a request that needs you and fits this is told to you at once (if the realm's Brief setting allows); the rest waits for the next brief.",
        default: text::TRIAGE_URGENT,
    },
    Prompt {
        name: "memex.triage.signals",
        description: "What marks a sales sequence from a stranger, no model needed: tracking domains of sequence tools, and (phrase:) follow-ups in a thread you never answered. One per line.",
        default: text::TRIAGE_SIGNALS,
    },
    Prompt {
        name: "memex.requests.same",
        description: "Whether someone's new message is the same request as one of theirs already open (joined), or another. Placeholders: {{open}}, {{new}}.",
        default: text::REQUEST_SAME,
    },
    Prompt {
        name: "memex.requests.draft",
        description: "How a reply is drafted for a request that needs you: voice, facts only, choices as [[a | b]] for what only you decide. Placeholders: {{request}}, {{context}}.",
        default: text::REQUEST_DRAFT,
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
    let picked: Map<String, Value> = all
        .into_iter()
        .filter(|(k, _)| k.starts_with("session.subagent") || k.starts_with("session.compact") || k == "session.task_cards")
        .collect();
    Value::Object(picked).to_string()
}

/// The memex's prompts as `realm` uses them (each realm's knowledge is
/// read and sorted with its own).
pub async fn for_memex(st: &HeadState, realm: &str) -> Value {
    let all = all(st, realm).await;
    Value::Object(all.into_iter().filter(|(k, _)| k.starts_with("memex.")).collect())
}

/// Every realm's memex prompts, by realm.
pub async fn for_memex_realms(st: &HeadState) -> Value {
    let realms: Vec<String> = sqlx::query_scalar("SELECT id FROM realms").fetch_all(&st.store.pool).await.unwrap_or_default();
    let mut out = Map::new();
    for r in realms.into_iter().filter(|r| r != ROOT) {
        out.insert(r.clone(), for_memex(st, &r).await);
    }
    Value::Object(out)
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

/// The realm a Settings request is for (root's by default).
fn request_realm() -> String {
    crate::store::SETTINGS_REALM.try_with(Clone::clone).unwrap_or_else(|_| ROOT.to_owned())
}

/// A realm's own prompts (its overrides of root's).
async fn own(st: &HeadState, realm: &str) -> anyhow::Result<Vec<Value>> {
    crate::lists::read(&st.store.pool, PROMPTS, realm).await
}

/// The prompts with their defaults, for Settings: root's text and whether
/// it was changed from the default; in a realm, the text it uses, whether
/// it sets its own, and root's it follows otherwise.
pub async fn list(st: &HeadState) -> anyhow::Result<Vec<api::PromptInfo>> {
    let realm = request_realm();
    if realm != ROOT {
        let (mine, roots) = (own(st, &realm).await?, all(st, ROOT).await);
        let used = all(st, &realm).await;
        return Ok(DEFAULTS
            .iter()
            .map(|p| {
                let text = |m: &Map<String, Value>| m.get(p.name).and_then(Value::as_str).unwrap_or(p.default).to_owned();
                api::PromptInfo {
                    name: p.name.into(),
                    description: p.description.into(),
                    body: text(&used),
                    default: p.default.into(),
                    edited: mine.iter().any(|e| e["name"] == p.name),
                    inherited: Some(text(&roots)),
                }
            })
            .collect());
    }
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
                inherited: None,
            }
        })
        .collect())
}

/// Changes root's text of a prompt (an empty text, or the default's,
/// returns it to following the default).
pub async fn set(st: &HeadState, name: &str, body: &str) -> anyhow::Result<()> {
    let p = DEFAULTS.iter().find(|p| p.name == name).ok_or_else(|| crate::not_found(format!("no prompt {name:?}")))?;
    // In a realm: its own text, or (empty) back to following root's.
    let realm = request_realm();
    if realm != ROOT {
        let mut mine: Vec<Value> = own(st, &realm).await?.into_iter().filter(|e| e["name"] != p.name).collect();
        if !body.trim().is_empty() {
            mine.push(json!({"name": p.name, "description": p.description, "body": body}));
        }
        crate::lists::write(&st.store.pool, PROMPTS, &realm, &mine, "you").await?;
        // Each realm's memex reads with its own.
        crate::memex::reload(st).await;
        return Ok(());
    }
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
