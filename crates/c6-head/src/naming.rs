//! Sessions name themselves (docs/suggestions.md §7): once a session's
//! first turn finishes, the model a `task:name` routing rule picks (else
//! the `task:suggest` rule's, else the session's own) reads how it
//! started and gives it a short name in place of its first line, once. A
//! name you give it wins; subagents and side chats keep their titles.

use c6_api as api;
use c6_proto::{TurnEndReason, content_block, session_event};
use tracing::warn;

use crate::HeadState;
use crate::store::SessionRow;

/// The routing label a rule names to pick the model for names.
pub const ROUTE_LABEL: &str = "task:name";
/// A first message of at most this many words ("hi", "continue") says too
/// little: the name waits for the next finished turn.
const FEW_WORDS: usize = 3;
/// The longest name kept.
const MAX_CHARS: usize = 60;
/// What the model reads at most, and of each message.
const MAX_INPUT_CHARS: usize = 8_000;
const MAX_MESSAGE_CHARS: usize = 2_000;
/// How far into a session its opening is looked for, in pages of events.
const PAGES: usize = 20;
const PAGE: i64 = 500;

/// Where a session that waits for its name says how far it got (a
/// settings key: no column of its own). No key: it was named, you named
/// it, or it is never named by itself (it isn't new, or it had a name).
fn key(id: &str) -> String {
    format!("session_naming:{id}")
}
/// Its first finished turn names it.
const FIRST: &str = "first";
/// Its first message said too little: the next finished turn names it.
const NEXT: &str = "next";
/// It's being named now.
const NAMING: &str = "naming";

/// A new session of yours, or a conversation with the assistant, with
/// its first line for a title: named once its first turn finishes.
pub async fn expect(st: &HeadState, id: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)").bind(key(id)).bind(FIRST).execute(&st.store.pool).await?;
    Ok(())
}

/// You named the session (or undid its name): never named by itself.
pub async fn forget(st: &HeadState, id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM settings WHERE key = ?").bind(key(id)).execute(&st.store.pool).await?;
    Ok(())
}

/// Whether sessions in `realm` name themselves (Settings → Suggestions;
/// on by default).
pub async fn enabled(st: &HeadState, realm: &str) -> bool {
    st.store
        .setting_for(crate::suggestions::KEY, realm)
        .await
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<api::SuggestionSettings>(&v).ok())
        .is_none_or(|s| s.names)
}

async fn waiting(st: &HeadState, id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(key(id)).fetch_optional(&st.store.pool).await?)
}

/// Moves a session waiting for its name from `from` to `to`; false when
/// it was elsewhere (named meanwhile, or by you).
async fn swap(st: &HeadState, id: &str, from: &str, to: &str) -> anyhow::Result<bool> {
    let r = sqlx::query("UPDATE settings SET value = ? WHERE key = ? AND value = ?")
        .bind(to)
        .bind(key(id))
        .bind(from)
        .execute(&st.store.pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// New events: a finished turn of a session that waits for its name names
/// it, in the background (the model takes a moment).
pub async fn on_events(st: &HeadState, session_id: &str, events: &[c6_proto::SessionEvent]) {
    let finished =
        events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(t)) if t.reason == TurnEndReason::Completed as i32));
    if !finished {
        return;
    }
    let state = match waiting(st, session_id).await {
        Ok(Some(s)) if s == FIRST || s == NEXT => s,
        Ok(_) => return,
        Err(e) => {
            warn!(session = %session_id, "reading whether it waits for a name: {e:#}");
            return;
        }
    };
    let Some(arc) = st.arc() else { return };
    let id = session_id.to_owned();
    tokio::spawn(async move {
        if let Err(e) = after_turn(&arc, &id, &state).await {
            warn!(session = %id, "naming the session: {e:#}");
        }
    });
}

async fn after_turn(st: &HeadState, id: &str, state: &str) -> anyhow::Result<()> {
    let Some(row) = st.store.session(id).await? else { return forget(st, id).await };
    // Subagents and side chats keep their titles; off, none is made.
    if row.parent_id.is_some() || row.observed || !enabled(st, &row.realm).await {
        return forget(st, id).await;
    }
    let (opening, few) = opening(st, id).await?;
    if state == FIRST && few {
        swap(st, id, FIRST, NEXT).await?;
        return Ok(());
    }
    if !swap(st, id, state, NAMING).await? {
        return Ok(());
    }
    // Made once: a call that fails leaves the first line (Rename's Suggest
    // can still make one).
    let name = match make(st, &row, &opening).await {
        Ok(n) => n,
        Err(e) => {
            forget(st, id).await?;
            return Err(e);
        }
    };
    // In place of the title it had, unless you renamed it meanwhile (that
    // took the key away).
    let mut tx = st.store.pool.begin().await?;
    let named = !name.is_empty()
        && name != row.title
        && sqlx::query(
            "UPDATE sessions SET title = ? WHERE id = ? AND title = ? AND EXISTS (SELECT 1 FROM settings WHERE key = ? AND value = ?)",
        )
        .bind(&name)
        .bind(id)
        .bind(&row.title)
        .bind(key(id))
        .bind(NAMING)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
    sqlx::query("DELETE FROM settings WHERE key = ?").bind(key(id)).execute(&mut *tx).await?;
    tx.commit().await?;
    if named {
        st.registry.bump();
    }
    Ok(())
}

/// A name for a session as it started (the Rename box's Suggest): the
/// same call as after its first turn, made now.
pub async fn suggest(st: &HeadState, row: &SessionRow) -> anyhow::Result<String> {
    let (opening, _) = opening(st, &row.id).await?;
    anyhow::ensure!(!opening.is_empty(), "nothing has been said in it yet");
    let name = make(st, row, &opening).await?;
    anyhow::ensure!(!name.is_empty(), "the model gave no name");
    Ok(name)
}

async fn make(st: &HeadState, row: &SessionRow, opening: &str) -> anyhow::Result<String> {
    let template = crate::prompts::get(st, "session.name", &row.realm).await;
    let prompt = api::prompts::fill(&template, &[("exchanges", opening)]);
    Ok(clean(&crate::catchup::ask(st, row, ROUTE_LABEL, "session name", prompt, 1024).await?))
}

/// How the session started, as the model reads it: your messages and the
/// agent's final reply of its first turn, or of its first two when your
/// first message has only a few words (never tool output). With whether
/// it had only a few.
async fn opening(st: &HeadState, id: &str) -> anyhow::Result<(String, bool)> {
    let clip = |t: &str| -> String {
        let t = t.trim();
        if t.chars().count() > MAX_MESSAGE_CHARS {
            format!("{}…", t.chars().take(MAX_MESSAGE_CHARS).collect::<String>())
        } else {
            t.to_owned()
        }
    };
    let mut out = String::new();
    let mut few = None;
    let mut said = String::new();
    let mut replies = 0;
    let mut after = 0;
    'pages: for _ in 0..PAGES {
        let page = st.store.events_after(id, after, PAGE).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for e in &page {
            match &e.kind {
                // Yours, not another session's agent's.
                Some(session_event::Kind::User(u)) if u.from_session_id.is_empty() && !u.text.trim().is_empty() => {
                    few.get_or_insert_with(|| few_words(&u.text));
                    out.push_str(&format!("User: {}\n", clip(&u.text)));
                }
                Some(session_event::Kind::Assistant(a)) => {
                    let text: String = a
                        .blocks
                        .iter()
                        .filter_map(|b| match &b.block {
                            Some(content_block::Block::Text(t)) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect();
                    if !text.trim().is_empty() {
                        said = text;
                    }
                }
                Some(session_event::Kind::TurnEnded(_)) if !said.is_empty() => {
                    out.push_str(&format!("Agent: {}\n\n", clip(&std::mem::take(&mut said))));
                    replies += 1;
                    let turns = if few == Some(true) { 2 } else { 1 };
                    if replies >= turns || out.len() > MAX_INPUT_CHARS {
                        break 'pages;
                    }
                }
                _ => {}
            }
        }
    }
    let out = out.trim_end();
    let out = if out.len() > MAX_INPUT_CHARS { out.chars().take(MAX_INPUT_CHARS).collect() } else { out.to_owned() };
    Ok((out, few.unwrap_or(false)))
}

fn few_words(text: &str) -> bool {
    text.split_whitespace().count() <= FEW_WORDS
}

/// A short name as a title: its first line, without quotes, labels or
/// closing punctuation, at most [`MAX_CHARS`], starting with a capital.
fn clean(text: &str) -> String {
    let mut line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    for prefix in ["Name:", "Title:", "name:", "title:"] {
        line = line.strip_prefix(prefix).unwrap_or(line).trim();
    }
    let mut line = line.to_owned();
    loop {
        let trimmed = line
            .trim_matches(['"', '“', '”', '\'', '‘', '’', '`', '*', '_', '#'])
            .trim_end_matches(['.', '!', '?', ',', ';', ':', '…'])
            .trim()
            .to_owned();
        if trimmed == line {
            break;
        }
        line = trimmed;
    }
    let mut name = String::new();
    for word in line.split_whitespace() {
        if name.chars().count() + 1 + word.chars().count() > MAX_CHARS {
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    if name.is_empty() {
        // One long word: cut.
        name = line.chars().take(MAX_CHARS).collect();
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_the_name() {
        assert_eq!(clean("\"Realm switching speed\".\nBecause…"), "Realm switching speed");
        assert_eq!(clean("Title: **Pager off-by-one fix**"), "Pager off-by-one fix");
        assert_eq!(clean("postgres upgrade plan!"), "Postgres upgrade plan");
        assert_eq!(clean("   "), "");
        assert!(clean(&"word ".repeat(40)).chars().count() <= MAX_CHARS);
        assert_eq!(clean(&"x".repeat(100)).chars().count(), MAX_CHARS);
    }

    #[test]
    fn a_few_words_wait() {
        assert!(few_words("hi"));
        assert!(few_words(" continue please now "));
        assert!(!few_words("fix the pager bug"));
    }
}
