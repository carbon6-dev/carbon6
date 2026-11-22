//! Private card discussions (docs/card-chat.md). Only the two tools below
//! are dispatched here; settlement remains in decisions' existing actions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use c6_api as api;
use c6_core::now_ms;
use c6_llm::{Block, Message, ModelRequest, Role, ToolSpec};
use serde_json::{Value, json};
use sqlx::Row;

use crate::store::SessionRow;
use crate::{HeadState, bad_request, not_found};

const MAX_TEXT: usize = 8_000;
const WINDOW: i64 = 200;

/// Serializes card actions with revisions, never the model calls themselves.
#[derive(Default)]
pub struct Runtime {
    cards: Mutex<HashMap<i64, Weak<tokio::sync::Mutex<()>>>>,
}

pub(crate) async fn lock(st: &HeadState, id: i64) -> tokio::sync::OwnedMutexGuard<()> {
    let mutex = {
        let mut cards = st.card_chat.cards.lock().unwrap();
        // Keep queued waiters and active guards, not every card ever visited.
        cards.retain(|_, mutex| mutex.strong_count() > 0);
        match cards.get(&id).and_then(Weak::upgrade) {
            Some(mutex) => mutex,
            None => {
                let mutex = Arc::new(tokio::sync::Mutex::new(()));
                cards.insert(id, Arc::downgrade(&mutex));
                mutex
            }
        }
    };
    mutex.lock_owned().await
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[tokio::test]
    async fn card_actions_wait_for_the_guard_and_read_state_after_acquiring_it() {
        let (_dir, head, _mock, _) = crate::work::tests::routing_fixture().await;
        let state = head.state.clone();
        let st = &state;
        let d =
            crate::decisions::open(st, &api::DecisionIn { title: "Guarded card".into(), ..Default::default() }, crate::realms::HOME, "")
                .await
                .unwrap();
        let guard = lock(st, d.id).await;
        let answer = api::DecisionAnswer::default();
        let edit = api::DecisionEdit::default();
        let input = json!({"title": "Stale revision"});
        let caller = crate::realms::Caller::root();
        let answer = crate::decisions::answer(st, d.id, &answer, "test");
        let revision = revise(st, &d, &input);
        let edit = crate::decisions::edit(st, d.id, &edit);
        let ask = ask_session(st, d.id, api::DecisionDiscuss { text: "Ask its source".into() }, &caller);
        tokio::pin!(answer, revision, edit, ask);
        assert!(futures::poll!(answer.as_mut()).is_pending());
        assert!(futures::poll!(revision.as_mut()).is_pending());
        assert!(futures::poll!(edit.as_mut()).is_pending());
        assert!(futures::poll!(ask.as_mut()).is_pending());

        // Simulate the action holding the guard closing the card. Every
        // waiter must use this state, not a snapshot read before waiting.
        sqlx::query("UPDATE decisions SET state = 'dismissed', title = 'Latest card' WHERE id = ?")
            .bind(d.id)
            .execute(&st.store.pool)
            .await
            .unwrap();
        drop(guard);
        assert!(answer.await.is_err());
        assert!(revision.await.is_err());
        let edited = edit.await.unwrap();
        assert_eq!(edited.state, "dismissed");
        assert_eq!(edited.title, "Latest card");
        assert!(ask.await.is_err());
        assert!(get(st, d.id).await.unwrap().revisions.is_empty());
        head.shutdown().await.unwrap();
    }
}

fn closed(d: &api::Decision) -> bool {
    !matches!(d.state.as_str(), "open" | "snoozed")
}

async fn card(st: &HeadState, id: i64) -> anyhow::Result<api::Decision> {
    crate::decisions::get(st, id).await?.ok_or_else(|| not_found("no such decision"))
}

async fn source(st: &HeadState, d: &api::Decision) -> anyhow::Result<Option<SessionRow>> {
    let id = d.context["session"].as_str().or_else(|| d.source.strip_prefix("session:"));
    let Some(id) = id else { return Ok(None) };
    Ok(st.store.session(id).await?.filter(|s| s.realm == d.realm))
}

pub async fn get(st: &HeadState, id: i64) -> anyhow::Result<api::DecisionDiscussion> {
    let d = card(st, id).await?;
    let rows = sqlx::query("SELECT role, text, created_at FROM decision_discussion_messages WHERE decision_id = ? ORDER BY id")
        .bind(id)
        .fetch_all(&st.store.pool)
        .await?;
    let revisions = sqlx::query("SELECT before_text, after_text, created_at FROM decision_revisions WHERE decision_id = ? ORDER BY id")
        .bind(id)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(api::DecisionDiscussion {
        messages: rows
            .iter()
            .map(|r| api::DecisionDiscussionMessage { role: r.get("role"), text: r.get("text"), created_at: r.get("created_at") })
            .collect(),
        revisions: revisions
            .iter()
            .map(|r| {
                Ok(api::DecisionRevision {
                    before: serde_json::from_str(&r.get::<String, _>("before_text"))?,
                    after: serde_json::from_str(&r.get::<String, _>("after_text"))?,
                    created_at: r.get("created_at"),
                })
            })
            .collect::<anyhow::Result<_>>()?,
        closed: closed(&d),
        can_ask_session: !closed(&d) && source(st, &d).await?.is_some_and(|s| s.archived == 0 && s.status != "failed"),
    })
}

fn question(req: &api::DecisionDiscuss) -> anyhow::Result<&str> {
    let text = req.text.trim();
    if text.is_empty() || text.chars().count() > MAX_TEXT {
        return Err(bad_request(format!("a discussion message needs 1 to {MAX_TEXT} characters")));
    }
    Ok(text)
}

pub async fn discuss(st: &HeadState, id: i64, req: &api::DecisionDiscuss) -> anyhow::Result<api::DecisionDiscussion> {
    let text = question(req)?;
    let d = card(st, id).await?;
    if closed(&d) {
        return Err(bad_request("this card is closed; its discussion is read-only"));
    }
    let (connection, model) = model_for(st, &d).await?;
    sqlx::query("INSERT OR IGNORE INTO decision_discussions (decision_id) VALUES (?)").bind(id).execute(&st.store.pool).await?;
    // A lease also recovers from head shutdown during a model call.
    let lease = now_ms() + 180_000;
    let n = sqlx::query("UPDATE decision_discussions SET running_until = ? WHERE decision_id = ? AND running_until < ?")
        .bind(lease)
        .bind(id)
        .bind(now_ms())
        .execute(&st.store.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(bad_request("a reply is already running on this card"));
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), turn(st, &d, text, &connection, &model)).await;
    sqlx::query("UPDATE decision_discussions SET running_until = 0 WHERE decision_id = ? AND running_until = ?")
        .bind(id)
        .bind(lease)
        .execute(&st.store.pool)
        .await?;
    result.map_err(|_| anyhow::anyhow!("the card discussion timed out"))??;
    get(st, id).await
}

async fn model_for(st: &HeadState, d: &api::Decision) -> anyhow::Result<(String, String)> {
    let connections = st.store.effective_connections().await?;
    let available = |id: &str| {
        connections.iter().any(|c| c.id == id && (d.realm == crate::realms::ROOT || crate::realms::available(&c.realms, &d.realm)))
    };
    let mut fallback = None;
    if let Some(s) = source(st, d).await?
        && available(&s.connection_id)
        && crate::pools::cannot_answer(st, &s.connection_id).await.is_none()
    {
        fallback = Some((s.connection_id, s.model));
    }
    if fallback.is_none()
        && let Ok((c, m)) = crate::assistant::model(st, &d.realm).await
        && available(&c)
        && crate::pools::cannot_answer(st, &c).await.is_none()
    {
        fallback = Some((c, m));
    }
    crate::work::model_for_route(st, &d.realm, "task:card", fallback)
        .await?
        .ok_or_else(|| bad_request("no model for this card: add a task:card rule in Settings → Model routing"))
}

fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_source_events".into(),
            description: "Read a bounded digest from this card's source event log. Default: around where the card was raised.".into(),
            parameters: json!({"type":"object","properties":{"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":200}},"additionalProperties":false}),
        },
        ToolSpec {
            name: "revise_card".into(),
            description: "Revise card wording, keeping a before/after history. Task cards: title, why, prompt, worktree, labels. Other cards: title, document and explanatory context.discussion only. Never changes options or settlement.".into(),
            parameters: json!({"type":"object","properties":{"title":{"type":"string"},"why":{"type":"string"},"prompt":{"type":"string"},"worktree":{"type":"boolean"},"labels":{"type":"array","items":{"type":"string"}},"document":{"type":"string"},"context":{"type":"object"}},"additionalProperties":false}),
        },
    ]
}

#[derive(Debug, PartialEq)]
enum CardReply {
    Text(String),
    Tools(Vec<(String, String, String)>),
}

/// ClaudeOnce has no native tool transport. Normalize its strict text
/// protocol into the same calls, without granting any additional tools.
fn normalize_reply(reply: &Message, json_protocol: bool) -> anyhow::Result<CardReply> {
    let text: String = reply.blocks.iter().filter_map(Block::text).collect();
    if !json_protocol {
        let calls: Vec<_> = reply.tool_uses().map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.to_owned())).collect();
        return Ok(if calls.is_empty() { CardReply::Text(text) } else { CardReply::Tools(calls) });
    }
    #[derive(serde::Deserialize)]
    #[serde(untagged, deny_unknown_fields)]
    enum JsonReply {
        Reply { reply: String },
        Tool { tool: String, input: serde_json::Map<String, Value> },
    }
    match serde_json::from_str::<JsonReply>(&text)? {
        JsonReply::Reply { reply } => Ok(CardReply::Text(reply)),
        JsonReply::Tool { tool, input } => Ok(CardReply::Tools(vec![("card-json-tool".into(), tool, Value::Object(input).to_string())])),
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_once_json_turn_revises_the_card_and_saves_a_plain_text_reply() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake-claude");
        let first = json!({"result": json!({"tool":"revise_card","input":{"title":"Revised through ClaudeOnce"}}).to_string(),
                           "usage":{"input_tokens":1,"output_tokens":1}});
        let last = json!({"result": json!({"reply":"I revised the card title."}).to_string(),
                          "usage":{"input_tokens":1,"output_tokens":1}});
        std::fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
[ "$1" = --version ] && exit 0
input=$(cat)
case "$input" in *'"tool_protocol":"json"'*) ;; *) exit 9 ;; esac
case "$input" in
  *'"tool_result":'*)
    case "$input" in *'"is_error":false'*) ;; *) exit 10 ;; esac
    printf '%s\n' '{last}' ;;
  *) printf '%s\n' '{first}' ;;
esac
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        c6_testkit::runnable(&executable, &["--version"]).await;
        let head = crate::start(crate::HeadConfig {
            claude_bin: Some(executable),
            data_dir: dir.path().join("head"),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let st = &head.state;
        sqlx::query("INSERT INTO connections (id, name, kind, base_url, created_at) VALUES ('cc', 'cc', 'claude_code', '', 0)")
            .execute(&st.store.pool)
            .await
            .unwrap();
        st.store
            .set_setting(crate::assistant::SETTING, &json!({"model":{"connection":"cc","model":"fake-model"}}).to_string())
            .await
            .unwrap();
        st.store
            .set_setting(
                crate::sessions::ROUTES,
                &json!([{"name":"card","applies_to":["task:card"],"connection":"cc","model":"fake-model","enabled":true}]).to_string(),
            )
            .await
            .unwrap();
        let d =
            crate::decisions::open(st, &api::DecisionIn { title: "Original title".into(), ..Default::default() }, crate::realms::HOME, "")
                .await
                .unwrap();
        let question = "Please revise the title.";
        let history = discuss(st, d.id, &api::DecisionDiscuss { text: question.into() }).await.unwrap();
        let updated = card(st, d.id).await.unwrap();
        assert_eq!(updated.title, "Revised through ClaudeOnce");
        assert_eq!(updated.state, "open");
        assert_eq!(updated.options, d.options);
        assert_eq!(history.messages.len(), 2);
        assert_eq!((&*history.messages[0].role, &*history.messages[0].text), ("user", question));
        assert_eq!((&*history.messages[1].role, &*history.messages[1].text), ("assistant", "I revised the card title."));
        assert_eq!(history.revisions.len(), 1);
        assert_eq!(history.revisions[0].before["title"], d.title);
        assert_eq!(history.revisions[0].after["title"], updated.title);
        assert_eq!(get(st, d.id).await.unwrap(), history);
        head.shutdown().await.unwrap();
    }

    fn reply(text: &str) -> Message {
        Message { role: Role::Assistant, blocks: vec![Block::Text(text.into())] }
    }

    #[test]
    fn json_protocol_normalizes_replies_and_both_discussion_tools() {
        assert_eq!(normalize_reply(&reply(r#"{"reply":"Explanation"}"#), true).unwrap(), CardReply::Text("Explanation".into()));
        for name in ["read_source_events", "revise_card"] {
            let text = json!({"tool": name, "input": {"title": "New wording"}}).to_string();
            assert_eq!(
                normalize_reply(&reply(&text), true).unwrap(),
                CardReply::Tools(vec![("card-json-tool".into(), name.into(), json!({"title": "New wording"}).to_string())])
            );
        }
    }

    #[test]
    fn json_protocol_is_strict_and_native_transport_is_unchanged() {
        for text in [
            "not JSON",
            "```json\n{\"reply\":\"Explanation\"}\n```",
            r#"{"reply":"Explanation","tool":"revise_card","input":{}}"#,
            r#"{"tool":"revise_card"}"#,
            r#"{"tool":"revise_card","input":"not an object"}"#,
            r#"{"reply":42}"#,
            r#"{"reply":"Explanation","extra":true}"#,
            r#"{"reply":"first","reply":"second"}"#,
            r#"[{"reply":"Explanation"}]"#,
            r#"{"reply":"Explanation"} trailing"#,
        ] {
            assert!(normalize_reply(&reply(text), true).is_err(), "{text}");
        }
        let native = Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse { id: "native-id".into(), name: "revise_card".into(), input_json: "{}".into() }],
        };
        assert_eq!(
            normalize_reply(&native, false).unwrap(),
            CardReply::Tools(vec![("native-id".into(), "revise_card".into(), "{}".into())])
        );
        let json_text = r#"{"tool":"revise_card","input":{}}"#;
        assert_eq!(normalize_reply(&reply(json_text), false).unwrap(), CardReply::Text(json_text.into()));
    }

    #[tokio::test]
    async fn json_calls_still_use_the_existing_allowlist_and_malformed_replies_do_not_revise() {
        let (_dir, head, _mock, _) = crate::work::tests::routing_fixture().await;
        let st = &head.state;
        let d =
            crate::decisions::open(st, &api::DecisionIn { title: "Protected card".into(), ..Default::default() }, crate::realms::HOME, "")
                .await
                .unwrap();
        let unavailable = normalize_reply(&reply(r#"{"tool":"decision_answer","input":{}}"#), true).unwrap();
        let CardReply::Tools(calls) = unavailable else { panic!("expected a normalized call") };
        for (_, name, input) in calls {
            assert!(tool(st, d.id, &name, &input).await.is_err());
        }
        assert!(normalize_reply(&reply(r#"{"tool":"revise_card","input":{"title":"Must not persist"},"reply":"Done"}"#), true).is_err());
        assert_eq!(card(st, d.id).await.unwrap(), d);
        let history = get(st, d.id).await.unwrap();
        assert!(history.messages.is_empty() && history.revisions.is_empty());
        head.shutdown().await.unwrap();
    }
}

async fn source_events(st: &HeadState, d: &api::Decision, input: &Value) -> anyhow::Result<String> {
    let Some(s) = source(st, d).await? else { return Ok("(no source session)".into()) };
    let anchor = st.store.seq_at(&s.id, d.created_at).await?;
    let after = input.get("after").map(|n| n.as_u64().ok_or_else(|| bad_request("after must be a nonnegative sequence"))).transpose()?;
    let after = after.unwrap_or_else(|| anchor.saturating_sub(100));
    let limit = input.get("limit").map(|n| n.as_u64().ok_or_else(|| bad_request("limit must be a positive integer"))).transpose()?;
    let limit = limit.unwrap_or(WINDOW as u64).clamp(1, WINDOW as u64);
    let events = st.store.events_after(&s.id, after.min(i64::MAX as u64), limit as i64).await?;
    Ok(crate::watchers::digest(&events))
}

async fn turn(st: &HeadState, d: &api::Decision, text: &str, connection: &str, model: &str) -> anyhow::Result<()> {
    let digest = source_events(st, d, &json!({})).await?;
    let history = get(st, d.id).await?;
    let source_context = if let Some(s) = source(st, d).await? {
        let started = st.store.events_after(&s.id, 0, 1).await?.into_iter().find_map(|e| match e.kind {
            Some(c6_proto::session_event::Kind::Started(s)) => Some(s),
            _ => None,
        });
        Some(json!({"session":s.id,"title":s.title,"worker":s.worker_id,
            "cwd":started.as_ref().map_or(s.cwd.as_str(), |s| s.cwd.as_str()),"repo":s.repo,
            "worktree":started.as_ref().is_some_and(|s| !s.worktree_branch.is_empty()),
            "worktree_branch":started.as_ref().map(|s| &s.worktree_branch),
            "labels":serde_json::from_str::<Value>(&s.labels).unwrap_or(json!([])),"ssh":s.ssh_target}))
    } else {
        None
    };
    let json_protocol = st.store.connections().await?.iter().any(|c| c.id == connection && c.kind == "claude_code");
    let mut seed = json!({"card": d, "source":source_context, "source_digest": digest});
    if json_protocol {
        seed["tool_protocol"] = json!("json");
        seed["tools"] = serde_json::to_value(tools())?;
    }
    let mut messages = vec![Message::user_text(seed.to_string())];
    let start = history.messages.len().saturating_sub(32);
    for m in &history.messages[start..] {
        messages.push(Message {
            role: if m.role == "assistant" { Role::Assistant } else { Role::User },
            blocks: vec![Block::Text(m.text.clone())],
        });
    }
    messages.push(Message::user_text(text));
    append(st, d.id, "user", text).await?;
    let mut req = ModelRequest {
        model: model.to_owned(),
        system: crate::prompts::get(st, "decision.discuss", &d.realm).await,
        messages,
        tools: if json_protocol { Vec::new() } else { tools() },
        max_output_tokens: Some(4096),
        cache_key: None,
        effort: Some("low".into()),
        web_search: false,
        json: json_protocol,
    };
    for _ in 0..8 {
        if closed(&card(st, d.id).await?) {
            return Err(bad_request("this card closed while replying; discussion is read-only"));
        }
        let (reply, _) =
            crate::watchers::ask_message(st, &d.realm, "card discussion", &format!("decision:{}", d.id), connection, &req).await?;
        let calls = match normalize_reply(&reply, json_protocol)? {
            CardReply::Text(text) => {
                if text.trim().is_empty() {
                    return Err(anyhow::anyhow!("the card model gave no reply"));
                }
                append(st, d.id, "assistant", &text.chars().take(32_000).collect::<String>()).await?;
                return Ok(());
            }
            CardReply::Tools(calls) => calls,
        };
        req.messages.push(reply);
        let mut results = Vec::new();
        for (id, name, input) in calls {
            let result = tool(st, d.id, &name, &input).await;
            let (content, is_error) = match result {
                Ok(v) => (v.to_string(), false),
                Err(e) => (format!("{e:#}"), true),
            };
            results.push(if json_protocol {
                Block::Text(json!({"tool_result": {"tool": name, "content": content, "is_error": is_error}}).to_string())
            } else {
                Block::ToolResult { id, content, is_error }
            });
        }
        req.messages.push(Message { role: Role::User, blocks: results });
    }
    Err(anyhow::anyhow!("the card discussion used its eight model steps; try a shorter question"))
}

async fn append(st: &HeadState, id: i64, role: &str, text: &str) -> anyhow::Result<()> {
    // Closing the card wins even when it races a streamed reply.
    let n = sqlx::query(
        "INSERT INTO decision_discussion_messages (decision_id, role, text, created_at) \
         SELECT id, ?, ?, ? FROM decisions WHERE id = ? AND state IN ('open', 'snoozed')",
    )
    .bind(role)
    .bind(text)
    .bind(now_ms())
    .bind(id)
    .execute(&st.store.pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(bad_request("this card is closed; its discussion is read-only"));
    }
    Ok(())
}

async fn tool(st: &HeadState, id: i64, name: &str, input: &str) -> anyhow::Result<Value> {
    let d = card(st, id).await?;
    if closed(&d) {
        return Err(bad_request("this card is closed; its discussion is read-only"));
    }
    let input: Value = serde_json::from_str(input)?;
    match name {
        "read_source_events" => Ok(json!({"digest": source_events(st, &d, &input).await?})),
        "revise_card" => revise(st, &d, &input).await,
        _ => Err(bad_request("this discussion only has read_source_events and revise_card")),
    }
}

fn snapshot(d: &api::Decision) -> Value {
    json!({"title":d.title,"document":d.document,"context":d.context})
}

fn revised(d: &api::Decision, input: &Value) -> anyhow::Result<api::Decision> {
    let obj = input.as_object().ok_or_else(|| bad_request("a revision must be an object"))?;
    if d.kind == "task" && !d.context.is_object() {
        return Err(bad_request("this task card has no valid object context to revise"));
    }
    let allowed: &[&str] =
        if d.kind == "task" { &["title", "why", "prompt", "worktree", "labels"] } else { &["title", "document", "context"] };
    if obj.is_empty() || obj.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(bad_request("only this card's wording may be revised, never its options, origin or state"));
    }
    let mut next = d.clone();
    for (key, value) in obj {
        match key.as_str() {
            "title" | "why" | "prompt" | "document" => {
                let text = value.as_str().ok_or_else(|| bad_request("card text must be a string"))?.trim();
                let max = if key == "title" { 500 } else { 64_000 };
                if text.chars().count() > max || (matches!(key.as_str(), "title" | "prompt") && text.is_empty()) {
                    return Err(bad_request(format!("{key} must be nonempty and at most {max} characters")));
                }
                match key.as_str() {
                    "title" => next.title = text.into(),
                    "prompt" => next.context["prompt"] = json!(text),
                    _ => next.document = text.into(),
                }
            }
            "worktree" => next.context["worktree"] = json!(value.as_bool().ok_or_else(|| bad_request("worktree must be a boolean"))?),
            "labels" => {
                let labels: Vec<String> = serde_json::from_value(value.clone()).map_err(|_| bad_request("labels must be strings"))?;
                if labels.len() > 50 || labels.iter().any(|s| s.len() > 200) {
                    return Err(bad_request("use up to 50 short labels"));
                }
                next.context["labels"] = json!(labels);
            }
            "context" => {
                let ctx = value.as_object().ok_or_else(|| bad_request("context must be an object"))?;
                if ctx.keys().any(|k| k != "discussion")
                    || ctx.values().any(|v| !v.is_string() || v.as_str().is_some_and(|s| s.len() > 64_000))
                {
                    return Err(bad_request("only explanatory context.discussion text may change; existing action context is protected"));
                }
                if let Some(note) = ctx.get("discussion") {
                    if !next.context.is_object() {
                        return Err(bad_request("this card's existing non-object action context cannot be replaced"));
                    }
                    next.context["discussion"] = note.clone();
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(next)
}

async fn revise(st: &HeadState, d: &api::Decision, input: &Value) -> anyhow::Result<Value> {
    let _guard = lock(st, d.id).await;
    let current = card(st, d.id).await?;
    if closed(&current) || snapshot(&current) != snapshot(d) {
        return Err(bad_request("the card closed or changed while revising; read it again"));
    }
    let d = &current;
    let next = revised(d, input)?;
    let before = snapshot(d);
    let after = snapshot(&next);
    if before == after {
        return Ok(after);
    }
    let now = now_ms();
    let mut tx = st.store.pool.begin().await?;
    let n = sqlx::query(
        "UPDATE decisions SET title = ?, document = ?, context = ?, updated_at = ? WHERE id = ? \
         AND state IN ('open', 'snoozed') AND title = ? AND document = ? AND context = ?",
    )
    .bind(&next.title)
    .bind(&next.document)
    .bind(next.context.to_string())
    .bind(now)
    .bind(d.id)
    .bind(&d.title)
    .bind(&d.document)
    .bind(d.context.to_string())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(bad_request("the card closed or changed while revising; read it again"));
    }
    sqlx::query("INSERT INTO decision_revisions (decision_id, before_text, after_text, created_at) VALUES (?, ?, ?, ?)")
        .bind(d.id)
        .bind(before.to_string())
        .bind(after.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    st.registry.bump();
    Ok(after)
}

pub async fn ask_session(
    st: &HeadState,
    id: i64,
    req: api::DecisionDiscuss,
    caller: &crate::realms::Caller,
) -> anyhow::Result<api::Session> {
    let text = question(&req)?;
    let _guard = lock(st, id).await;
    let d = card(st, id).await?;
    if closed(&d) {
        return Err(bad_request("this card is closed; its discussion is read-only"));
    }
    let s =
        source(st, &d).await?.filter(|s| s.archived == 0).ok_or_else(|| bad_request("the source session is unavailable or archived"))?;
    let anchor = st.store.last_seq(&s.id).await?;
    crate::sessions::create_side_chat_from(
        st,
        &s.id,
        api::CreateSideChat {
            anchor_seq: anchor,
            prompt: format!("{}\n\n{}", json!({"card": d}), text),
            connection_id: s.connection_id,
            model: s.model,
            effort: s.effort,
        },
        caller,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::tests::{note_routes, routing_fixture};
    use c6_testkit::{Reply, eventually};

    async fn proposal(st: &HeadState) -> api::Decision {
        crate::decisions::open(
            st,
            &api::DecisionIn {
                kind: "task".into(),
                title: "Remove the retry adapter".into(),
                document: "The client already retries.".into(),
                source: "session:session".into(),
                context: json!({"session": "session", "prompt": "Remove the adapter and run tests.", "worker": "", "cwd": ""}),
                ..Default::default()
            },
            crate::realms::HOME,
            "",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn routing_prefers_task_card_then_callable_source_then_ask() {
        let (_dir, head, mock, _) = routing_fixture().await;
        let st = &head.state;
        // Opening the card refreshes the work item attached to its source session.
        // Disable automatic work notes so only routing is under test.
        st.store.set_setting(crate::work::SETTING, r#"{"notes":false}"#).await.unwrap();
        let mut d = proposal(st).await;
        st.store
            .set_setting(crate::assistant::SETTING, &json!({"model": {"connection": "api", "model": "ask-model"}}).to_string())
            .await
            .unwrap();

        // The fixture's source uses Claude Code, unavailable on this head.
        assert_eq!(model_for(st, &d).await.unwrap(), ("api".into(), "ask-model".into()));
        sqlx::query("UPDATE sessions SET connection_id = 'api', model = 'source-model' WHERE id = 'session'")
            .execute(&st.store.pool)
            .await
            .unwrap();
        assert_eq!(model_for(st, &d).await.unwrap(), ("api".into(), "source-model".into()));
        note_routes(st, &[("task:card", "card-model")]).await;
        assert_eq!(model_for(st, &d).await.unwrap(), ("api".into(), "card-model".into()));

        // Uncallable and unrelated rules cannot replace a usable source.
        st.store
            .set_setting(
                crate::sessions::ROUTES,
                &json!([
                    {"name": "unavailable", "applies_to": ["task:card"], "connection": "cc", "model": "opus", "enabled": true},
                    {"name": "other task", "applies_to": ["task:suggest"], "connection": "api", "model": "not-card", "enabled": true}
                ])
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(model_for(st, &d).await.unwrap(), ("api".into(), "source-model".into()));
        note_routes(st, &[]).await;
        d.context = json!({});
        d.source = "you".into();
        assert_eq!(model_for(st, &d).await.unwrap(), ("api".into(), "ask-model".into()));
        st.store
            .set_setting(crate::assistant::SETTING, &json!({"model": {"connection": "cc", "model": "opus"}}).to_string())
            .await
            .unwrap();
        assert!(model_for(st, &d).await.is_err(), "an unavailable Ask fallback must not be returned as a callable model");
        let requests = mock.requests();
        assert!(requests.is_empty(), "routing itself must not call the model: {requests:#?}");
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn routing_never_uses_a_connection_outside_the_cards_realm() {
        let (_dir, head, _mock, _) = routing_fixture().await;
        let st = &head.state;
        let d = proposal(st).await;
        sqlx::query("INSERT INTO connections (id, name, kind, base_url, created_at, realms) VALUES ('restricted', 'restricted', 'openai_compat', '', 0, '[\"lab\"]')")
            .execute(&st.store.pool).await.unwrap();
        sqlx::query("UPDATE sessions SET connection_id = 'restricted', model = 'source-model' WHERE id = 'session'")
            .execute(&st.store.pool)
            .await
            .unwrap();
        st.store
            .set_setting(crate::assistant::SETTING, &json!({"model":{"connection":"api","model":"ask-model"}}).to_string())
            .await
            .unwrap();
        assert_eq!(model_for(st, &d).await.unwrap(), ("api".into(), "ask-model".into()));
        st.store
            .set_setting(crate::assistant::SETTING, &json!({"model":{"connection":"restricted","model":"ask-model"}}).to_string())
            .await
            .unwrap();
        assert!(model_for(st, &d).await.is_err());
        note_routes(st, &[("task:card", "card-model")]).await;
        assert_eq!(model_for(st, &d).await.unwrap(), ("api".into(), "card-model".into()));
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn revisions_are_durable_and_stale_or_closed_writes_are_rejected() {
        let (dir, head, _mock, _) = routing_fixture().await;
        let st = &head.state;
        let d = proposal(st).await;
        let before = snapshot(&d);
        let after = revise(
            st,
            &d,
            &json!({"title": "Keep the public shim", "why": "Only the adapter is redundant.",
                                          "prompt": "Remove only the adapter, preserving the public shim."}),
        )
        .await
        .unwrap();
        let history = get(st, d.id).await.unwrap();
        assert_eq!(history.revisions.len(), 1);
        assert_eq!(history.revisions[0].before, before);
        assert_eq!(history.revisions[0].after, after);
        assert!(history.revisions[0].created_at > 0);
        assert_eq!(snapshot(&card(st, d.id).await.unwrap()), after);

        // A stale snapshot must not overwrite the newer card or add history.
        assert!(revise(st, &d, &json!({"title": "Stale writer"})).await.is_err());
        let latest = card(st, d.id).await.unwrap();
        revise(st, &latest, &json!({"title": latest.title})).await.unwrap();
        assert_eq!(get(st, d.id).await.unwrap().revisions.len(), 1, "no-op revisions add no history");
        crate::decisions::edit(st, d.id, &api::DecisionEdit { state: Some("dismissed".into()), ..Default::default() }).await.unwrap();
        assert!(revise(st, &latest, &json!({"title": "Too late"})).await.is_err());
        assert!(append(st, d.id, "assistant", "A late reply").await.is_err());
        assert!(discuss(st, d.id, &api::DecisionDiscuss { text: "Explain this closed card".into() }).await.is_err());
        let closed = get(st, d.id).await.unwrap();
        assert!(closed.closed);
        assert!(closed.messages.is_empty());
        assert_eq!(closed.revisions, history.revisions);

        // Read through a separately opened store, not the in-memory result.
        let reopened = crate::store::Store::open(&dir.path().join("c6h.db")).await.unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT before_text, after_text FROM decision_revisions WHERE decision_id = ? ORDER BY id")
                .bind(d.id)
                .fetch_all(&reopened.pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(&rows[0].0).unwrap(), before);
        assert_eq!(serde_json::from_str::<Value>(&rows[0].1).unwrap(), after);
        reopened.pool.close().await;
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn source_event_reads_validate_sequences_and_cap_each_page() {
        use c6_proto::{SessionEvent, UserMessage, session_event::Kind};
        let (_dir, head, _mock, _) = routing_fixture().await;
        let st = &head.state;
        let events: Vec<_> = (1..=205)
            .map(|seq| SessionEvent {
                seq,
                ts_ms: seq as i64,
                kind: Some(Kind::User(UserMessage { text: format!("source-event-{seq:03}"), ..Default::default() })),
            })
            .collect();
        crate::store::tests::insert_events(&st.store, "session", &events).await;
        let d = proposal(st).await;
        let page = source_events(st, &d, &json!({"after": 0, "limit": u64::MAX})).await.unwrap();
        assert_eq!(page.lines().count(), WINDOW as usize);
        assert!(page.contains("source-event-001") && page.contains("source-event-200"), "{page}");
        assert!(!page.contains("source-event-201"));
        let page = source_events(st, &d, &json!({"after": 198, "limit": 2})).await.unwrap();
        assert_eq!(page.lines().count(), 2);
        assert!(page.contains("source-event-199") && page.contains("source-event-200"));
        assert_eq!(source_events(st, &d, &json!({"after": 0, "limit": 0})).await.unwrap().lines().count(), 1);
        assert!(source_events(st, &d, &json!({"after": u64::MAX})).await.unwrap().is_empty());
        for input in [json!({"after": -1}), json!({"after": 1.5}), json!({"after": "1"}), json!({"limit": -1}), json!({"limit": "2"})] {
            assert!(source_events(st, &d, &input).await.is_err(), "{input}");
        }
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_running_discussion_holds_a_lease_and_releases_it_after_reply() {
        let (_dir, head, mock, _) = routing_fixture().await;
        note_routes(&head.state, &[("task:card", "card-model")]).await;
        let d = proposal(&head.state).await;
        let gate = mock.push_gated(Reply::Text("The client already retries.".into()));
        let st = head.state.clone();
        let pending =
            tokio::spawn(async move { discuss(&st, d.id, &api::DecisionDiscuss { text: "Why remove the adapter?".into() }).await });
        eventually("the gated card model call", async || (!mock.requests().is_empty()).then_some(())).await;
        let error = discuss(&head.state, d.id, &api::DecisionDiscuss { text: "An overlapping question".into() }).await.unwrap_err();
        assert!(error.to_string().contains("already running"), "{error:#}");
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(get(&head.state, d.id).await.unwrap().messages.len(), 1, "the rejected question was not appended");
        gate.notify_one();
        let first = c6_testkit::within("the gated discussion reply", pending).await.unwrap().unwrap();
        assert_eq!(first.messages.len(), 2);
        let lease: i64 = sqlx::query_scalar("SELECT running_until FROM decision_discussions WHERE decision_id = ?")
            .bind(d.id)
            .fetch_one(&head.state.store.pool)
            .await
            .unwrap();
        assert_eq!(lease, 0);
        mock.push(Reply::Text("Keep the shim, remove the adapter.".into()));
        let next = discuss(&head.state, d.id, &api::DecisionDiscuss { text: "What should stay?".into() }).await.unwrap();
        assert_eq!(next.messages.len(), 4);
        assert_eq!(mock.requests().len(), 2);
        head.shutdown().await.unwrap();
    }

    #[test]
    fn questions_are_trimmed_and_bounded_in_characters() {
        assert_eq!(question(&api::DecisionDiscuss { text: "  Explain this card.\n".into() }).unwrap(), "Explain this card.");
        assert!(question(&api::DecisionDiscuss { text: " \n\t".into() }).is_err());
        let boundary = api::DecisionDiscuss { text: "é".repeat(MAX_TEXT) };
        assert_eq!(question(&boundary).unwrap().chars().count(), MAX_TEXT);
        assert!(question(&api::DecisionDiscuss { text: "é".repeat(MAX_TEXT + 1) }).is_err());
    }

    #[test]
    fn malformed_task_context_is_rejected_without_panicking() {
        for context in [Value::Null, json!("not an object"), json!([]), json!(7)] {
            let d = api::Decision { kind: "task".into(), title: "Malformed old task".into(), context, ..Default::default() };
            for input in [json!({"prompt": "A new prompt"}), json!({"worktree": false}), json!({"labels": ["reviewed"]})] {
                assert!(revised(&d, &input).is_err(), "malformed task context must not panic or lose its origin: {input}");
            }
        }
    }
}
