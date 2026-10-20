//! Suggested next messages (docs/suggestions.md): after a turn finishes,
//! what you'd likely type next, made by a cheap model while you look at
//! the session, at most once per turn.

use c6_api as api;
use c6_core::now_ms;
use c6_llm::{Accumulator, Block, Message, ModelRequest, Role};
use c6_proto::{CredentialRequest, TurnEndReason, content_block, session_event};
use futures::StreamExt;
use sqlx::Row;
use tracing::warn;

use crate::HeadState;
use crate::realms::Caller;

/// The setting (per realm, else root's): `{"enabled": bool}`, on by default.
pub const KEY: &str = "session_suggestions";
/// The routing label a rule names to pick the model for suggestions.
pub const ROUTE_LABEL: &str = "task:suggest";
/// What the model is given at most (about 4,000 tokens).
const MAX_INPUT_CHARS: usize = 16_000;

pub async fn enabled(st: &HeadState, realm: &str) -> bool {
    st.store
        .setting_for(KEY, realm)
        .await
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
        .and_then(|v| v["enabled"].as_bool())
        .unwrap_or(true)
}

/// The suggestion for the session as it is now: made (or cached) when its
/// last turn finished and nothing waits on you; None otherwise.
pub async fn get(st: &HeadState, session_id: &str, caller: &Caller) -> anyhow::Result<Option<api::Suggestion>> {
    let Some(row) = st.store.session(session_id).await? else { return Ok(None) };
    if !caller.sees(&row.realm) || !enabled(st, &row.realm).await {
        return Ok(None);
    }
    let for_seq = row.last_seq;
    if let Some(r) = sqlx::query("SELECT text FROM suggestions WHERE session_id = ? AND for_seq = ?")
        .bind(session_id)
        .bind(for_seq)
        .fetch_optional(&st.store.pool)
        .await?
    {
        let text: String = r.get("text");
        return Ok((!text.is_empty()).then_some(api::Suggestion { text, for_seq }));
    }
    // Only for a finished turn, with nothing queued or asked.
    if row.status != "idle" || row.awaiting > 0 || row.connection_id.is_empty() || !st.store.queued_inputs(session_id).await?.is_empty() {
        return Ok(None);
    }
    let turn = st.store.turn_events(session_id, for_seq as u64).await?;
    let completed = turn
        .last()
        .is_some_and(|e| matches!(&e.kind, Some(session_event::Kind::TurnEnded(t)) if t.reason == TurnEndReason::Completed as i32));
    // Something asked of you there takes the answer; a task card asks nothing.
    let asked = crate::decisions::of_session(st, session_id, caller).await?.iter().any(|d| d.kind != "task");
    if !completed || asked {
        return Ok(None);
    }
    if crate::usage::blocked(st, &row.all_labels(), &row.realm).await?.is_some() {
        return Ok(None);
    }
    let text = match make(st, &row, for_seq).await {
        Ok(t) => t,
        Err(e) => {
            warn!(session = %session_id, "suggesting a next message: {e:#}");
            return Ok(None);
        }
    };
    sqlx::query(
        "INSERT INTO suggestions (session_id, for_seq, text, made_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT (session_id) DO UPDATE SET for_seq = excluded.for_seq, text = excluded.text, made_at = excluded.made_at",
    )
    .bind(session_id)
    .bind(for_seq)
    .bind(&text)
    .bind(now_ms())
    .execute(&st.store.pool)
    .await?;
    Ok((!text.is_empty()).then_some(api::Suggestion { text, for_seq }))
}

/// You saw it, or (`accepted`) took it: for the leverage measurement.
pub async fn seen(st: &HeadState, session_id: &str, for_seq: i64, accepted: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE suggestions SET shown = shown + ?, accepted = accepted + ? WHERE session_id = ? AND for_seq = ? AND text != ''")
        .bind(!accepted as i64)
        .bind(accepted as i64)
        .bind(session_id)
        .bind(for_seq)
        .execute(&st.store.pool)
        .await?;
    Ok(())
}

/// The connection and model: a routing rule for `task:suggest` that
/// applies to the session (its realm's rules, then root's), else the
/// session's own.
async fn model_for(st: &HeadState, row: &crate::store::SessionRow) -> anyhow::Result<(String, String)> {
    let mut labels = row.all_labels();
    labels.push(ROUTE_LABEL.into());
    let connections = st.store.connections().await?;
    for realm in [row.realm.as_str(), crate::realms::ROOT] {
        let routes: Vec<api::Route> =
            serde_json::from_str(&st.store.setting_for(crate::sessions::ROUTES, realm).await?.unwrap_or_default()).unwrap_or_default();
        for r in routes.iter().filter(|r| r.enabled && r.applies_to.iter().any(|l| l == ROUTE_LABEL)) {
            if !api::labels::applies(&r.applies_to, &labels) {
                continue;
            }
            let want = r.connection.trim();
            let connection = if want.is_empty() {
                Some(row.connection_id.clone())
            } else {
                connections.iter().find(|c| c.id == want || c.name == want).map(|c| c.id.clone())
            };
            if let Some(c) = connection {
                return Ok((c, r.model.trim().to_owned()));
            }
        }
    }
    Ok((row.connection_id.clone(), row.model.clone()))
}

/// The last exchanges, as the model reads them: your messages, the agent's
/// final words, and which tools it used (never their output).
async fn exchanges(st: &HeadState, session_id: &str, last_seq: u64) -> anyhow::Result<String> {
    let mut turns: Vec<String> = Vec::new();
    let mut end = last_seq;
    let mut size = 0;
    while end > 0 && turns.len() < 3 {
        let events = st.store.turn_events(session_id, end).await?;
        let Some(first) = events.first() else { break };
        let mut you = Vec::new();
        let mut said = String::new();
        let mut tools: Vec<String> = Vec::new();
        for e in &events {
            match &e.kind {
                Some(session_event::Kind::User(u)) => you.push(u.text.trim().to_owned()),
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
                        said = text.trim().to_owned();
                    }
                }
                Some(session_event::Kind::ToolResult(t)) => {
                    tools.push(if t.is_error { format!("{} (failed)", t.tool) } else { t.tool.clone() })
                }
                _ => {}
            }
        }
        // The turn's own message comes just before its start.
        if you.is_empty()
            && let Some(prev) =
                st.store.events_after(session_id, first.seq.saturating_sub(3), 3).await?.into_iter().rev().find_map(|e| match e.kind {
                    Some(session_event::Kind::User(u)) if e.seq < first.seq => Some(u.text),
                    _ => None,
                })
        {
            you.push(prev.trim().to_owned());
        }
        let mut t = String::new();
        for y in you.iter().filter(|y| !y.is_empty()) {
            t.push_str(&format!("User: {y}\n"));
        }
        if !tools.is_empty() {
            t.push_str(&format!("(The agent used: {})\n", tools.join(", ")));
        }
        if !said.is_empty() {
            t.push_str(&format!("Agent: {said}\n"));
        }
        size += t.len();
        if size > MAX_INPUT_CHARS && !turns.is_empty() {
            break;
        }
        turns.push(t);
        end = first.seq.saturating_sub(1);
    }
    turns.reverse();
    let all = turns.join("\n");
    // The newest is kept whole; older text gives way.
    Ok(if all.len() > MAX_INPUT_CHARS { all[all.len() - MAX_INPUT_CHARS..].to_owned() } else { all })
}

async fn make(st: &HeadState, row: &crate::store::SessionRow, for_seq: i64) -> anyhow::Result<String> {
    let template = crate::prompts::get(st, "session.suggest", &row.realm).await;
    let labels = row.all_labels().join(", ");
    let prompt = api::prompts::fill(
        &template,
        &[("title", &row.title), ("labels", &labels), ("exchanges", &exchanges(st, &row.id, for_seq as u64).await?)],
    );
    let (connection, model) = model_for(st, row).await?;
    let grant =
        crate::sessions::credential_for(st, None, "head", CredentialRequest { connection_id: connection.clone(), ..Default::default() })
            .await;
    anyhow::ensure!(grant.error.is_empty(), "{}", grant.error);
    let client = c6_llm::client_for(&grant.kind, &grant.base_url, &grant.bearer, grant.headers.clone())?;
    let req = ModelRequest {
        model: model.clone(),
        system: String::new(),
        messages: vec![Message { role: Role::User, blocks: vec![Block::Text(prompt)] }],
        tools: Vec::new(),
        // Room for a thinking model's reasoning; the answer is cut to a line.
        max_output_tokens: Some(1024),
        cache_key: None,
        effort: Some("low".into()),
        web_search: false,
    };
    let mut stream = client.stream(&req).await?;
    let mut acc = Accumulator::default();
    while let Some(ev) = stream.next().await {
        acc.push(&ev?);
    }
    let usage = acc.usage;
    let msg = acc.finish(true);
    // Its tokens count toward the session (a row of its own, below zero).
    sqlx::query(
        "INSERT OR REPLACE INTO usage (session_id, seq, ts, connection_id, model, input, output, cache_read, cache_write, realm) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.id)
    .bind(-for_seq.max(1))
    .bind(now_ms())
    .bind(&connection)
    .bind(&model)
    .bind(usage.input_tokens as i64)
    .bind(usage.output_tokens as i64)
    .bind(usage.cache_read_tokens as i64)
    .bind(usage.cache_write_tokens as i64)
    .bind(&row.realm)
    .execute(&st.store.pool)
    .await?;
    let text: String = msg.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect();
    // No text at all (it spent its tokens thinking) is a failure, tried
    // again next time; "-" is an answer: no clear next step.
    anyhow::ensure!(!text.trim().is_empty(), "{model} gave no answer");
    Ok(clean(&text))
}

/// One line, as you'd type it; empty when the model saw no clear next step.
fn clean(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.trim_start_matches("User:").trim_start_matches("You:").trim();
    let line = line.trim_matches(|c| matches!(c, '"' | '“' | '”' | '\'' | '`')).trim();
    if line.is_empty() || line == "-" || line == "—" {
        return String::new();
    }
    line.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_the_answer() {
        assert_eq!(clean("\n\"Run the tests again\"\nbecause…"), "Run the tests again");
        assert_eq!(clean("User: commit this"), "commit this");
        assert_eq!(clean(" - "), "");
        assert_eq!(clean(""), "");
        assert_eq!(clean(&"x".repeat(500)).len(), 200);
    }
}
