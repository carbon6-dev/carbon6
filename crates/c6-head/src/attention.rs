//! What needs you, in one list (`GET /api/v1/attention`): what waits on
//! you (open decisions, among them approvals of tool calls and agents'
//! questions; not task cards, which ask nothing), what's worth a look (a
//! turn that ended or failed since you last saw it, agents' answers to your
//! comments, one row per document, people waiting on you in the Brief),
//! and for other realms only how many wait there (docs/realms-ux.md §6).
//! What you've seen of a realm's answers and Brief is kept on the head (a
//! per-realm setting), so your phone and your desktop agree. Whatever may
//! change it pings the live feed (`LiveEvent::Attention`), at most once
//! every half second.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use c6_core::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use tokio::sync::{Notify, watch};
use tracing::warn;

use crate::HeadState;
use crate::docs::comments::Answered;
use crate::realms::Caller;

/// At most one ping this often.
const EVERY: Duration = Duration::from_millis(500);
/// How many turns and documents with answers a list holds.
const LIMIT: i64 = 50;
/// How many answered threads a realm's list reads (and what you've seen of
/// them keeps).
const ANSWERS: i64 = 500;
/// Where what you've seen of a realm's answers and Brief is kept (`Seen`).
pub const SEEN: &str = "attention_seen";
/// What counts as waiting: open decisions (and snoozed ones whose time has
/// come), not task cards.
const WAITING: &str = "(state = 'open' OR (state = 'snoozed' AND snooze_until <= ?)) AND kind != 'task'";

/// Pings for the live feed: say `changed` as often as you like; the feed
/// hears at most one every `EVERY`, and always one after the last change.
pub struct Pings {
    wake: Notify,
    tx: watch::Sender<u64>,
}

impl Default for Pings {
    fn default() -> Self {
        Self { wake: Notify::new(), tx: watch::channel(0).0 }
    }
}

impl Pings {
    /// What needs you may have changed.
    pub fn changed(&self) {
        self.wake.notify_one();
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }

    /// Turns changes into pings: one at once, then none for `every`; what
    /// changed meanwhile makes one more after it.
    async fn run(&self, every: Duration) {
        loop {
            self.wake.notified().await;
            self.tx.send_modify(|v| *v = v.wrapping_add(1));
            tokio::time::sleep(every).await;
        }
    }
}

pub fn spawn(st: Arc<HeadState>) {
    tokio::spawn(async move {
        tokio::select! {
            _ = st.shutdown.cancelled() => {}
            _ = st.attention.run(EVERY) => {}
        }
    });
}

/// What needs the caller: its realm's (the console's are root's), and how
/// many wait in each other realm it may know about.
pub async fn get(st: &HeadState, caller: &Caller) -> anyhow::Result<api::Attention> {
    let here = caller.listing_realm();
    let waiting = waiting(st, here).await?;
    let mut worth = turns(st, here, &waiting).await?;
    for realm in memex_realms(st, here).await? {
        let seen = seen_in(st, &realm).await;
        match crate::docs::comments::answered_in(st, &realm, ANSWERS).await {
            Ok(answers) => worth.extend(by_document(answers, &seen).into_iter().take(LIMIT as usize).map(|d| answers_row(d, &realm))),
            Err(e) => warn!(realm = %realm, "listing answered comments: {e:#}"),
        }
        if let Some((people, at)) = brief(st, &realm).await
            && at > seen.brief
        {
            worth.push(api::AttentionItem {
                key: "brief".into(),
                kind: "brief".into(),
                title: "People waiting on you in the Brief".into(),
                at,
                realm: Some(realm.clone()),
                count: Some(people as u64),
                ..Default::default()
            });
        }
    }
    worth.sort_by_key(|i| std::cmp::Reverse(i.at));
    // Root's token sees every realm here: there is nowhere else.
    let elsewhere = match here {
        None => Vec::new(),
        Some(here) => places(st, caller)
            .await?
            .into_iter()
            .filter(|p| p.id != here && p.waiting > 0)
            .map(|p| api::AttentionElsewhere { id: p.id, name: p.name, waiting: p.waiting })
            .collect(),
    };
    Ok(api::Attention { waiting, worth, elsewhere })
}

/// Open decisions, newest first, each with the session it's in (an
/// approval's or a question's from its reference).
async fn waiting(st: &HeadState, realm: Option<&str>) -> anyhow::Result<Vec<api::AttentionItem>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, kind, title, ref, context, realm, updated_at FROM decisions WHERE {WAITING} AND (? IS NULL OR realm = ?) \
         ORDER BY updated_at DESC, id DESC LIMIT 500"
    )))
    .bind(now_ms())
    .bind(realm)
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    waiting_items(st, rows).await
}

/// Everything answerable in a session, including its offered task cards,
/// and approvals/questions in its subagents at any depth. Side chats are
/// independent conversations, not subagents. Don't use the live session
/// list here: it deliberately contains only top-level sessions.
pub async fn of_session(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<Vec<api::AttentionItem>> {
    let rows = sqlx::query(
        "WITH RECURSIVE agents(id) AS (\
             SELECT id FROM sessions WHERE parent_id = ?1 AND parent_call_id IS NOT NULL \
             UNION SELECT s.id FROM sessions s JOIN agents a ON s.parent_id = a.id WHERE s.parent_call_id IS NOT NULL\
         ) SELECT id, kind, title, ref, context, sessions, realm, updated_at FROM decisions \
         WHERE (state = 'open' OR (state = 'snoozed' AND snooze_until <= ?2)) AND (?3 IS NULL OR realm = ?3) \
         AND (json_extract(context, '$.session') = ?1 \
              OR EXISTS (SELECT 1 FROM json_each(decisions.sessions) WHERE value = ?1) \
              OR ((ref LIKE 'approval:%' OR ref LIKE 'question:%') \
                  AND json_extract(context, '$.session') IN (SELECT id FROM agents))) \
         ORDER BY updated_at, id",
    )
    .bind(id)
    .bind(now_ms())
    .bind(caller.listing_realm())
    .fetch_all(&st.store.pool)
    .await?;
    waiting_items(st, rows).await
}

async fn waiting_items(st: &HeadState, rows: Vec<sqlx::sqlite::SqliteRow>) -> anyhow::Result<Vec<api::AttentionItem>> {
    let mut out = Vec::new();
    for r in rows {
        let id: i64 = r.get("id");
        let reference: String = r.get("ref");
        let context: Value = serde_json::from_str(&r.get::<String, _>("context")).unwrap_or(Value::Null);
        let mut parts = reference.splitn(3, ':');
        let (kind, session, call) = match (parts.next(), parts.next(), parts.next()) {
            (Some("approval"), Some(s), Some(c)) => ("approval", Some(s.to_owned()), Some(c.to_owned())),
            (Some("question"), Some(s), Some(c)) => ("question", Some(s.to_owned()), Some(c.to_owned())),
            _ => (
                if r.get::<String, _>("kind") == "task" { "task" } else { "decision" },
                context["session"].as_str().map(str::to_owned),
                None,
            ),
        };
        out.push(api::AttentionItem {
            key: format!("decision:{id}"),
            kind: kind.into(),
            title: r.get("title"),
            at: r.get("updated_at"),
            session_id: session,
            call_id: call,
            decision_id: Some(id),
            realm: Some(r.get("realm")),
            ..Default::default()
        });
    }
    // One title read for the whole list, not one full session read per
    // decision. A malformed reference must not disclose another realm's title.
    let ids: Vec<&str> = out.iter().filter_map(|i| i.session_id.as_deref()).collect();
    if !ids.is_empty() {
        let rows = sqlx::query("SELECT id, realm, title FROM sessions WHERE id IN (SELECT value FROM json_each(?))")
            .bind(serde_json::to_string(&ids)?)
            .fetch_all(&st.store.pool)
            .await
            .unwrap_or_else(|e| {
                warn!("reading attention session titles: {e:#}");
                Vec::new()
            });
        let titles: HashMap<(String, String), String> =
            rows.into_iter().map(|r| ((r.get("realm"), r.get("id")), display_title(r.get("title")))).collect();
        for item in &mut out {
            if let (Some(realm), Some(session)) = (&item.realm, &item.session_id) {
                item.detail = titles.get(&(realm.clone(), session.clone())).cloned().unwrap_or_default();
            }
        }
    }
    Ok(out)
}

fn display_title(title: String) -> String {
    if title.trim().is_empty() { "Untitled session".into() } else { title }
}

/// Sessions whose last turn ended (or that failed) since you saw them, as
/// work tells them (docs/work.md §3): not archived, not running, nothing
/// in them waiting on you.
async fn turns(st: &HeadState, realm: Option<&str>, waiting: &[api::AttentionItem]) -> anyhow::Result<Vec<api::AttentionItem>> {
    // Keep the candidate limit before state filtering, as in the live work
    // derivation. Read its session/seen/state and outstanding asks together,
    // rather than session_live's three database reads per candidate.
    let rows = sqlx::query(
        "WITH candidates AS (\
             SELECT id, realm, title, status, end_reason, awaiting, turn_ended_at FROM sessions \
             WHERE archived = 0 AND parent_id IS NULL AND (?1 IS NULL OR realm = ?1) \
             AND turn_ended_at > seen_at ORDER BY turn_ended_at DESC LIMIT ?2\
         ), asked(id) AS (\
             SELECT json_extract(context, '$.session') FROM decisions WHERE state = 'open' AND kind != 'task' \
             UNION SELECT j.value FROM decisions d, json_each(d.sessions) j WHERE d.state = 'open' AND d.kind != 'task'\
         ) SELECT * FROM candidates s WHERE awaiting <= 0 AND status NOT IN ('running', 'starting') \
         AND id NOT IN (SELECT id FROM asked WHERE typeof(id) = 'text') \
         ORDER BY turn_ended_at DESC",
    )
    .bind(realm)
    .bind(LIMIT)
    .fetch_all(&st.store.pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let (id, realm): (String, String) = (r.get("id"), r.get("realm"));
        if waiting.iter().any(|w| w.session_id.as_deref() == Some(id.as_str())) {
            continue;
        }
        let failed = r.get::<String, _>("status") == "failed" || r.get::<String, _>("end_reason") == "error";
        let title = display_title(r.get("title"));
        out.push(api::AttentionItem {
            key: format!("session:{id}"),
            kind: if failed { "failed" } else { "turn_ended" }.into(),
            detail: title.clone(),
            title,
            at: r.get("turn_ended_at"),
            session_id: Some(id),
            realm: Some(realm),
            ..Default::default()
        });
    }
    Ok(out)
}

/// The realms whose memex (documents, the Brief) the caller's list reads:
/// its own, or for root's token every realm's.
async fn memex_realms(st: &HeadState, realm: Option<&str>) -> anyhow::Result<Vec<String>> {
    if crate::knowledge::get(st).is_none() {
        return Ok(Vec::new());
    }
    let all = match realm {
        Some(r) => vec![r.to_owned()],
        None => crate::realms::list(st, &Caller::root()).await?.into_iter().map(|r| r.id).collect(),
    };
    Ok(all.into_iter().filter(|r| crate::memex::has_memex(r)).collect())
}

/// What you've seen of a realm's answers and Brief: the answer you saw in
/// each answered thread (its comment's id), and when the newest request you
/// saw in the Brief came.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Seen {
    #[serde(default)]
    threads: BTreeMap<String, String>,
    #[serde(default)]
    brief: i64,
}

/// The realm's own record only: a realm that has none starts empty rather
/// than borrowing root's (which would hide its Brief row by root's time).
async fn seen_in(st: &HeadState, realm: &str) -> Seen {
    if realm != crate::realms::ROOT && !st.store.has_own_setting(SEEN, realm).await.unwrap_or(false) {
        return Seen::default();
    }
    st.store.setting_for(SEEN, realm).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

async fn keep_seen(st: &HeadState, realm: &str, seen: &Seen) -> anyhow::Result<()> {
    let value = serde_json::to_string(seen)?;
    crate::store::SETTINGS_REALM.scope(realm.to_owned(), st.store.set_setting(SEEN, &value)).await
}

/// The answers you haven't seen, by document: the one with the newest
/// first, each one's newest thread first. A thread you saw comes back when
/// an agent answers it again.
fn by_document(answers: Vec<Answered>, seen: &Seen) -> Vec<Vec<Answered>> {
    let mut docs: Vec<Vec<Answered>> = Vec::new();
    for a in answers.into_iter().filter(|a| seen.threads.get(&a.thread) != Some(&a.comment)) {
        match docs.iter_mut().find(|d| d[0].doc == a.doc) {
            Some(d) => d.push(a),
            None => docs.push(vec![a]),
        }
    }
    docs
}

/// A document's row: how many of its threads have an answer you haven't
/// seen (the thread's subject if just one).
fn answers_row(threads: Vec<Answered>, realm: &str) -> api::AttentionItem {
    let n = threads.len();
    let first = &threads[0];
    api::AttentionItem {
        key: format!("doc:{}", first.doc),
        kind: "comment_answer".into(),
        title: first.title.clone(),
        detail: if n == 1 { first.about.clone() } else { format!("{n} answers") },
        at: first.at,
        doc_id: Some(first.doc.clone()),
        thread_id: (n == 1).then(|| first.thread.clone()),
        realm: Some(realm.to_owned()),
        count: Some(n as u64),
        ..Default::default()
    }
}

/// How many people wait on you in the Brief, if any, and when the newest
/// of their requests came.
async fn brief(st: &HeadState, realm: &str) -> Option<(i64, i64)> {
    let m = crate::knowledge::get(st)?.of_realm(realm).await.ok()?;
    match c6_memex::requests::waiting_people(&m).await {
        Ok((people, at)) => (people > 0).then_some((people, at)),
        Err(e) => {
            warn!(realm = %realm, "counting the Brief's requests: {e:#}");
            None
        }
    }
}

/// Marks seen what `keys` name (none: everything): a document's answers
/// (`doc:<doc>`), a thread's (`thread:<doc>:<thread>`), the Brief's
/// requests so far (`brief`, given when the newest came). Threads no longer
/// answered (replied to, resolved, gone) are forgotten.
fn mark(seen: &mut Seen, answers: &[Answered], brief: Option<i64>, keys: &[String]) {
    let named = |k: String| keys.is_empty() || keys.contains(&k);
    for a in answers {
        if named(format!("doc:{}", a.doc)) || named(format!("thread:{}:{}", a.doc, a.thread)) {
            seen.threads.insert(a.thread.clone(), a.comment.clone());
        }
    }
    seen.threads.retain(|t, _| answers.iter().any(|a| a.thread == *t));
    if let Some(at) = brief
        && named("brief".into())
    {
        seen.brief = seen.brief.max(at);
    }
}

/// How many things wait on you in a realm: what `waiting` lists there.
pub async fn waiting_count(st: &HeadState, realm: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM decisions WHERE {WAITING} AND realm = ?")))
        .bind(now_ms())
        .bind(realm)
        .fetch_one(&st.store.pool)
        .await?)
}

/// What waits on you in each realm the caller may know about: counts only,
/// for the realm switcher (`GET /api/v1/elsewhere`). A realm's token
/// learns only about its own; the console's (root's) is your browser's.
pub async fn places(st: &HeadState, caller: &Caller) -> anyhow::Result<Vec<api::RealmWaiting>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT realm, COUNT(*) AS n FROM decisions WHERE {WAITING} GROUP BY realm")))
        .bind(now_ms())
        .fetch_all(&st.store.pool)
        .await?;
    let counts: HashMap<String, i64> = rows.into_iter().map(|r| (r.get("realm"), r.get("n"))).collect();
    let mut out = Vec::new();
    for r in crate::realms::list(st, &Caller::root()).await? {
        let console = r.id == crate::realms::ROOT;
        if (console && !caller.browser) || !(caller.sees_others() || caller.sees(&r.id)) {
            continue;
        }
        let waiting = counts.get(&r.id).copied().unwrap_or(0);
        out.push(api::RealmWaiting { id: r.id, name: r.name, waiting: waiting.max(0) as u64 });
    }
    Ok(out)
}

/// You've seen these worth-a-look items (none named: every one the caller
/// sees), only the caller's own (the console's: root's). A turn that ended
/// or failed is marked seen, as opening its session does; a document's
/// answers stay seen until an agent answers again, the Brief's row until a
/// newer request comes. What waits on you is never cleared here (it leaves
/// once answered, snoozed or dismissed).
pub async fn seen(st: &HeadState, caller: &Caller, keys: &[String]) -> anyhow::Result<()> {
    let here = caller.listing_realm();
    let sessions: Vec<String> = if keys.is_empty() {
        let waiting = waiting(st, here).await?;
        turns(st, here, &waiting).await?.into_iter().filter_map(|i| i.session_id).collect()
    } else {
        keys.iter().filter_map(|k| k.strip_prefix("session:")).map(str::to_owned).collect()
    };
    // One change for them all, not one per session: each change has every
    // open page load its lists again.
    let now = now_ms();
    let mut any = false;
    for id in &sessions {
        match st.store.session(id).await? {
            Some(s) if here.is_none_or(|r| r == s.realm) => any |= st.store.mark_seen(id, now).await?,
            _ => {}
        }
    }
    if any {
        st.registry.bump();
        st.attention.changed();
    }
    for realm in memex_realms(st, here).await? {
        let answers = match crate::docs::comments::answered_in(st, &realm, ANSWERS).await {
            Ok(a) => a,
            Err(e) => {
                warn!(realm = %realm, "listing answered comments: {e:#}");
                continue;
            }
        };
        let brief = brief(st, &realm).await.map(|(_, at)| at);
        let mut seen = seen_in(st, &realm).await;
        let before = seen.clone();
        mark(&mut seen, &answers, brief, keys);
        if seen != before {
            keep_seen(st, &realm, &seen).await?;
            st.attention.changed();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_head() -> (tempfile::TempDir, crate::Head) {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        (dir, head)
    }

    async fn attention_session(st: &HeadState, id: &str, title: &str, realm: &str) {
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, realm, turn_ended_at) \
             VALUES (?, 'w', ?, '', 'c', 'm', 'idle', 0, 0, ?, 1)",
        )
        .bind(id)
        .bind(title)
        .bind(realm)
        .execute(&st.store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn batched_waiting_titles_handle_duplicates_missing_blank_and_cross_realm_references() {
        let (_dir, head) = test_head().await;
        let st = &head.state;
        attention_session(st, "named", "A session", "home").await;
        attention_session(st, "blank", "  ", "home").await;
        attention_session(st, "private", "Another realm's title", "commons").await;
        for (title, session, reference) in [
            ("First", "named", "approval:named:one"),
            ("Second", "named", "question:named:two"),
            ("Blank", "blank", ""),
            ("Missing", "missing", ""),
            ("Cross realm", "private", ""),
        ] {
            crate::decisions::open(
                st,
                &api::DecisionIn { title: title.into(), context: serde_json::json!({"session": session}), ..Default::default() },
                "home",
                reference,
            )
            .await
            .unwrap();
        }
        for realm in [None, Some("home")] {
            let items = waiting(st, realm).await.unwrap();
            let details: HashMap<_, _> = items.iter().map(|i| (i.title.as_str(), i.detail.as_str())).collect();
            assert_eq!(details.len(), 5);
            assert_eq!(details["First"], "A session");
            assert_eq!(details["Second"], "A session");
            assert_eq!(details["Blank"], "Untitled session");
            assert_eq!(details["Missing"], "");
            assert_eq!(details["Cross realm"], "");
        }
        for (realm, kind, state, until) in [
            ("commons", "question", "open", None),
            ("commons", "task", "open", None),
            ("commons", "question", "snoozed", Some(0)),
            ("commons", "question", "snoozed", Some(i64::MAX)),
            ("root", "question", "open", None),
        ] {
            let d = crate::decisions::open(
                st,
                &api::DecisionIn { title: "Other realm".into(), kind: kind.into(), ..Default::default() },
                realm,
                "",
            )
            .await
            .unwrap();
            sqlx::query("UPDATE decisions SET state = ?, snooze_until = ? WHERE id = ?")
                .bind(state)
                .bind(until)
                .bind(d.id)
                .execute(&st.store.pool)
                .await
                .unwrap();
        }
        let scoped = places(st, &Caller::in_realm("home")).await.unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!((scoped[0].id.as_str(), scoped[0].waiting), ("home", 5));
        assert!(places(st, &Caller::root()).await.unwrap().iter().all(|r| r.id != "root"));
        let browser = Caller { browser: true, ..Caller::in_realm("home") };
        let all = places(st, &browser).await.unwrap();
        for realm in &all {
            assert_eq!(realm.waiting, waiting_count(st, &realm.id).await.unwrap() as u64);
        }
        let counts: HashMap<_, _> = all.iter().map(|r| (r.id.as_str(), r.waiting)).collect();
        assert_eq!(counts["commons"], 2);
        assert_eq!(counts["root"], 1);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn batched_turns_match_live_work_state_and_keep_the_candidate_limit() {
        let (_dir, head) = test_head().await;
        let st = &head.state;
        let ids = [
            "idle",
            "failed",
            "end_error",
            "running",
            "starting",
            "awaiting",
            "seen",
            "archived",
            "child",
            "context",
            "linked",
            "task",
            "snoozed",
            "due",
            "answered",
            "123",
        ];
        for id in ids {
            attention_session(st, id, if id == "idle" { "  " } else { id }, "home").await;
        }
        attention_session(st, "other", "Other realm", "commons").await;
        for sql in [
            "UPDATE sessions SET status = 'failed' WHERE id = 'failed'",
            "UPDATE sessions SET end_reason = 'error' WHERE id = 'end_error'",
            "UPDATE sessions SET status = id, end_reason = 'error' WHERE id IN ('running', 'starting')",
            "UPDATE sessions SET awaiting = 1 WHERE id = 'awaiting'",
            "UPDATE sessions SET seen_at = turn_ended_at WHERE id = 'seen'",
            "UPDATE sessions SET archived = 1 WHERE id = 'archived'",
            "UPDATE sessions SET parent_id = 'idle' WHERE id = 'child'",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&st.store.pool).await.unwrap();
        }
        for (session, kind, state, context, linked, until) in [
            ("context", "question", "open", true, false, None),
            ("linked", "question", "open", false, true, None),
            ("task", "task", "open", true, false, None),
            ("snoozed", "question", "snoozed", true, false, Some(i64::MAX)),
            ("due", "question", "snoozed", true, false, Some(0)),
            ("answered", "question", "answered", true, false, None),
        ] {
            let d = crate::decisions::open(
                st,
                &api::DecisionIn {
                    title: session.into(),
                    kind: kind.into(),
                    context: if context { serde_json::json!({"session": session}) } else { serde_json::json!({}) },
                    ..Default::default()
                },
                "home",
                "",
            )
            .await
            .unwrap();
            sqlx::query("UPDATE decisions SET state = ?, sessions = ?, snooze_until = ? WHERE id = ?")
                .bind(state)
                .bind(if linked { serde_json::json!([session]) } else { serde_json::json!([]) }.to_string())
                .bind(until)
                .bind(d.id)
                .execute(&st.store.pool)
                .await
                .unwrap();
        }
        // The live derivation binds a text session id. JSON numbers are
        // not session references, even if their printed form matches an id.
        let numeric = crate::decisions::open(
            st,
            &api::DecisionIn {
                title: "Numeric, not a session reference".into(),
                context: serde_json::json!({"session": 123}),
                ..Default::default()
            },
            "home",
            "",
        )
        .await
        .unwrap();
        sqlx::query("UPDATE decisions SET sessions = '[123]' WHERE id = ?").bind(numeric.id).execute(&st.store.pool).await.unwrap();
        let waiting = waiting(st, Some("home")).await.unwrap();
        let items = turns(st, Some("home"), &waiting).await.unwrap();
        let actual: std::collections::BTreeSet<_> = items.iter().filter_map(|i| i.session_id.as_deref()).collect();
        assert_eq!(actual, ["idle", "failed", "end_error", "task", "snoozed", "answered", "123"].into_iter().collect());
        assert_eq!(items.iter().find(|i| i.session_id.as_deref() == Some("idle")).unwrap().title, "Untitled session");
        for item in &items {
            let live = crate::work::session_live(st, "home", item.session_id.as_deref().unwrap()).await.unwrap();
            assert_eq!(item.title, live.title);
            assert_eq!(item.kind, if live.state == "failed" { "failed" } else { "turn_ended" });
            assert!(!live.seen);
        }
        assert!(turns(st, None, &waiting).await.unwrap().iter().any(|i| i.session_id.as_deref() == Some("other")));

        // Newer running sessions still occupy candidate slots. Filtering
        // before LIMIT would surface older idle sessions the old list hid.
        for n in 0..55 {
            let id = format!("bulk-{n}");
            attention_session(st, &id, &id, "home").await;
            sqlx::query("UPDATE sessions SET turn_ended_at = ?, status = ? WHERE id = ?")
                .bind(100 + n)
                .bind(if n >= 30 { "running" } else { "idle" })
                .bind(id)
                .execute(&st.store.pool)
                .await
                .unwrap();
        }
        let items = turns(st, Some("home"), &waiting).await.unwrap();
        assert_eq!(items.len(), 25);
        assert_eq!(items.first().unwrap().session_id.as_deref(), Some("bulk-29"));
        assert_eq!(items.last().unwrap().session_id.as_deref(), Some("bulk-5"));
        head.shutdown().await.unwrap();
    }

    /// Exercise real head ingestion, including the worker-created child
    /// rows: neither the live-list filter nor nested ancestry may hide a
    /// held call from attention or the normal notification sender.
    #[tokio::test]
    async fn subagent_approvals_reach_attention_notifications_and_the_parent_waiting_list() {
        use c6_core::WorkerId;
        use c6_proto::{Approval, Question, SessionEvent, SessionEventBatch, SessionStarted, session_event::Kind};
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let st = &head.state;
        let worker = WorkerId::new();
        sqlx::query(
            "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, labels) \
                     VALUES ('parent', ?, 'Parent', '', 'c', 'm', 'running', 0, 0, '[\"project:test\"]')",
        )
        .bind(worker.to_string())
        .execute(&st.store.pool)
        .await
        .unwrap();

        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
                let sent = sent.clone();
                async move {
                    sent.send(v).unwrap();
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hook = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        st.store.set_setting(crate::notify::PUBLIC_URL, "https://carbon6.test").await.unwrap();
        st.store
            .set_setting(
                crate::notify::CHANNELS,
                &json!([{
                    "name": "phone", "kind": "webhook", "url": hook, "events": ["approval", "notice"], "applies_to": ["project:test"]
                }])
                .to_string(),
            )
            .await
            .unwrap();

        let event = |seq, kind| SessionEvent { seq, ts_ms: now_ms(), kind: Some(kind) };
        for (id, parent, call) in [("child", "parent", "task-1"), ("grandchild", "child", "task-2")] {
            let events = vec![event(
                1,
                Kind::Started(SessionStarted {
                    parent_session_id: parent.into(),
                    parent_call_id: call.into(),
                    title: id.into(),
                    ..Default::default()
                }),
            )];
            assert_eq!(crate::sessions::ingest(st, worker, SessionEventBatch { session_id: id.into(), events }).await.unwrap(), Some(1));
        }
        let held = SessionEventBatch {
            session_id: "grandchild".into(),
            events: vec![event(
                2,
                Kind::Approval(Approval {
                    call_id: "push".into(),
                    state: "requested".into(),
                    tool: "bash".into(),
                    input: r#"{"command":"git push"}"#.into(),
                    policy: "Pushes and pull requests need your approval".into(),
                    ..Default::default()
                }),
            )],
        };
        crate::sessions::ingest(st, worker, held.clone()).await.unwrap();
        // A resent batch must not open another decision or notify again.
        crate::sessions::ingest(st, worker, held).await.unwrap();
        let note = c6_testkit::within("the subagent approval notification", received.recv()).await.unwrap();
        assert_eq!(note["event"], "approval");
        assert_eq!(note["session_id"], "grandchild");
        assert_eq!(note["title"], "Approval needed: grandchild");
        assert_eq!(note["priority"], "high");
        assert!(note["approve_url"].as_str().unwrap().starts_with("https://carbon6.test/approve/"));
        let waiting = get(st, &Caller::in_realm("home")).await.unwrap().waiting;
        assert_eq!(waiting.len(), 1);
        assert_eq!(
            (waiting[0].kind.as_str(), waiting[0].session_id.as_deref(), waiting[0].call_id.as_deref()),
            ("approval", Some("grandchild"), Some("push"))
        );
        assert_eq!(waiting_count(st, "home").await.unwrap(), 1);
        assert!(
            st.store.sessions(Some("home"), 100).await.unwrap().iter().all(|s| s.id != "grandchild"),
            "the live list excludes subagents"
        );

        crate::sessions::ingest(
            st,
            worker,
            SessionEventBatch {
                session_id: "child".into(),
                events: vec![event(
                    2,
                    Kind::Question(Question {
                        call_id: "branch".into(),
                        state: "asked".into(),
                        title: "Which branch?".into(),
                        ..Default::default()
                    }),
                )],
            },
        )
        .await
        .unwrap();
        for id in ["parent", "child"] {
            let list = of_session(st, id, &Caller::in_realm("home")).await.unwrap();
            assert_eq!(list.len(), 2, "{id}: {list:?}");
            assert!(list.iter().any(|i| i.kind == "approval" && i.session_id.as_deref() == Some("grandchild")));
            assert!(list.iter().any(|i| i.kind == "question" && i.session_id.as_deref() == Some("child")));
        }
        assert!(of_session(st, "parent", &Caller::in_realm("commons")).await.unwrap().is_empty());

        // A session's own decisions and offered task cards belong in its
        // header too, including decisions linked through working sessions.
        for (kind, title) in [("question", "Choose a release"), ("task", "Review the release")] {
            crate::decisions::open(
                st,
                &api::DecisionIn { kind: kind.into(), title: title.into(), context: json!({"session": "parent"}), ..Default::default() },
                "home",
                "",
            )
            .await
            .unwrap();
        }
        let linked = crate::decisions::open(st, &api::DecisionIn { title: "Working on this".into(), ..Default::default() }, "home", "")
            .await
            .unwrap();
        sqlx::query("UPDATE decisions SET sessions = '[\"parent\"]' WHERE id = ?").bind(linked.id).execute(&st.store.pool).await.unwrap();
        let list = of_session(st, "parent", &Caller::in_realm("home")).await.unwrap();
        assert_eq!(list.len(), 5);
        assert!(list.iter().any(|i| i.title == "Choose a release" && i.kind == "decision"));
        assert!(list.iter().any(|i| i.title == "Review the release" && i.kind == "task"));
        assert!(list.iter().any(|i| i.decision_id == Some(linked.id)));
        assert_eq!(get(st, &Caller::in_realm("home")).await.unwrap().waiting.len(), 4, "offered tasks still don't inflate What needs you");

        // The browser/API path has the same list, and is a realm-scoped
        // read in the operation catalog (not an accidental admin route).
        let url = format!("http://{}/api/v1/sessions/parent/waiting", head.ui_addrs[0]);
        let home = st.store.create_api_token("home reader", Some("home")).await.unwrap().token.unwrap();
        let response = st.http.get(&url).bearer_auth(home).send().await.unwrap().error_for_status().unwrap();
        assert_eq!(response.json::<Vec<api::AttentionItem>>().await.unwrap(), list);
        let other = st.store.create_api_token("other reader", Some("commons")).await.unwrap().token.unwrap();
        assert_eq!(st.http.get(&url).bearer_auth(other).send().await.unwrap().status(), reqwest::StatusCode::NOT_FOUND);

        // Settling a child call removes it from every ancestor, not just
        // from the thread where it was answered.
        crate::sessions::ingest(
            st,
            worker,
            SessionEventBatch {
                session_id: "grandchild".into(),
                events: vec![event(3, Kind::Approval(Approval { call_id: "push".into(), state: "denied".into(), ..Default::default() }))],
            },
        )
        .await
        .unwrap();
        assert!(of_session(st, "parent", &Caller::in_realm("home")).await.unwrap().iter().all(|i| i.kind != "approval"));
        // A later note is a delivery barrier: a duplicate approval would
        // arrive before this, not merely after a too-early try_recv.
        crate::sessions::ingest(
            st,
            worker,
            SessionEventBatch {
                session_id: "grandchild".into(),
                events: vec![event(
                    4,
                    Kind::Notice(c6_proto::Notice { title: "Delivery barrier".into(), message: "done".into(), ..Default::default() }),
                )],
            },
        )
        .await
        .unwrap();
        let next = c6_testkit::within("the notification delivery barrier", received.recv()).await.unwrap();
        assert_eq!(next["event"], "notice", "the repeated request and its outcome send no second approval notification");
        head.shutdown().await.unwrap();
        server.abort();
    }

    /// A burst of changes makes one ping at once and at most one after it;
    /// a change later makes another.
    #[tokio::test]
    async fn bursts_are_coalesced() {
        let pings = Arc::new(Pings::default());
        let mut rx = pings.subscribe();
        let every = Duration::from_millis(200);
        let runner = {
            let p = pings.clone();
            tokio::spawn(async move { p.run(every).await })
        };
        for _ in 0..100 {
            pings.changed();
        }
        c6_testkit::within("the first ping", rx.changed()).await.unwrap();
        // Nothing more than the one after the window (rule 4: may sleep).
        tokio::time::sleep(every * 3).await;
        let after_burst = *rx.borrow_and_update();
        assert!((1..=2).contains(&after_burst), "{after_burst} pings for one burst");
        pings.changed();
        c6_testkit::within("a ping for a later change", rx.changed()).await.unwrap();
        assert!(*rx.borrow() > after_burst);
        runner.abort();
    }

    fn answer(doc: &str, thread: &str, comment: &str, at: i64) -> Answered {
        Answered {
            doc: doc.into(),
            title: format!("Doc {doc}"),
            thread: thread.into(),
            about: format!("“{thread}”"),
            comment: comment.into(),
            at,
        }
    }

    /// One row per document: how many answers (the thread's subject if
    /// one), the newest's time; a thread you saw only comes back with a
    /// newer answer.
    #[test]
    fn answers_are_one_row_per_document() {
        let answers = || vec![answer("d1", "t1", "c1", 30), answer("d2", "t3", "c3", 20), answer("d1", "t2", "c2", 10)];
        let rows: Vec<_> = by_document(answers(), &Seen::default()).into_iter().map(|d| answers_row(d, "home")).collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let (r1, r2) = (&rows[0], &rows[1]);
        assert_eq!((r1.key.as_str(), r1.title.as_str(), r1.detail.as_str()), ("doc:d1", "Doc d1", "2 answers"));
        assert_eq!((r1.at, r1.count), (30, Some(2)), "the newest answer's time");
        assert_eq!((r1.doc_id.as_deref(), r1.thread_id.as_deref(), r1.realm.as_deref()), (Some("d1"), None, Some("home")));
        assert_eq!((r2.detail.as_str(), r2.thread_id.as_deref(), r2.count), ("“t3”", Some("t3"), Some(1)));
        // Seen t1's answer: d1 is down to t2. Another answer in t1 brings it back.
        let seen = Seen { threads: [("t1".to_owned(), "c1".to_owned())].into(), brief: 0 };
        let docs = by_document(answers(), &seen);
        assert_eq!(docs.iter().map(|d| d[0].doc.as_str()).collect::<Vec<_>>(), ["d2", "d1"], "d2 has the newest answer now");
        assert_eq!(docs[1].iter().map(|a| a.thread.as_str()).collect::<Vec<_>>(), ["t2"]);
        let again = vec![answer("d1", "t1", "c9", 40), answer("d1", "t2", "c2", 10)];
        assert_eq!(by_document(again, &seen)[0].len(), 2);
    }

    /// Marking all seen covers every answer and the Brief; a document's key
    /// only its threads, the Brief's only it; threads no longer answered are
    /// forgotten, and a newer request in the Brief isn't seen.
    #[test]
    fn marking_seen_covers_what_it_names() {
        let answers = [answer("d1", "t1", "c1", 30), answer("d2", "t3", "c3", 20), answer("d1", "t2", "c2", 10)];
        let mut seen = Seen { threads: [("gone".to_owned(), "c0".to_owned())].into(), brief: 0 };
        mark(&mut seen, &answers, Some(50), &["doc:d1".into()]);
        assert_eq!(seen.threads, [("t1".to_owned(), "c1".to_owned()), ("t2".to_owned(), "c2".to_owned())].into());
        assert_eq!(seen.brief, 0, "the Brief wasn't named");
        assert!(by_document(answers.to_vec(), &seen).iter().all(|d| d[0].doc == "d2"));
        mark(&mut seen, &answers, Some(50), &["thread:d2:t3".into(), "brief".into()]);
        assert_eq!((seen.threads.len(), seen.brief), (3, 50));
        // Everything: every answer, and the Brief up to its newest request.
        let mut all = Seen::default();
        mark(&mut all, &answers[..1], Some(60), &[]);
        assert_eq!((all.threads.len(), all.brief), (1, 60));
        assert!(by_document(answers[..1].to_vec(), &all).is_empty());
        // The Brief's newest request answered: what was seen stays seen.
        mark(&mut all, &[], Some(40), &[]);
        assert_eq!((all.threads.len(), all.brief), (0, 60));
    }
}
