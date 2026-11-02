//! What needs you, in one list (`GET /api/v1/attention`): what waits on
//! you (open decisions, among them approvals of tool calls and agents'
//! questions; not task cards, which ask nothing), what's worth a look (a
//! turn that ended or failed since you last saw it, an agent's answer to
//! your comment, people waiting on you in the Brief), and for other realms
//! only how many wait there (docs/realms-ux.md §6). Whatever may change it
//! pings the live feed (`LiveEvent::Attention`), at most once every half
//! second.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use c6_core::now_ms;
use serde_json::Value;
use sqlx::Row;
use tokio::sync::{Notify, watch};
use tracing::warn;

use crate::HeadState;
use crate::realms::Caller;

/// At most one ping this often.
const EVERY: Duration = Duration::from_millis(500);
/// How many turns and comment answers a list holds.
const LIMIT: i64 = 50;
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
        match crate::docs::comments::answered_in(st, &realm, LIMIT).await {
            Ok(answers) => worth.extend(answers.into_iter().map(|a| api::AttentionItem {
                key: format!("thread:{}:{}", a.doc, a.thread),
                kind: "comment_answer".into(),
                title: a.about,
                detail: a.title,
                at: a.at,
                doc_id: Some(a.doc),
                thread_id: Some(a.thread),
                realm: Some(realm.clone()),
                ..Default::default()
            })),
            Err(e) => warn!(realm = %realm, "listing answered comments: {e:#}"),
        }
        if let Some(b) = brief(st, &realm).await {
            worth.push(b);
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
        "SELECT id, title, ref, context, realm, updated_at FROM decisions WHERE {WAITING} AND (? IS NULL OR realm = ?) \
         ORDER BY updated_at DESC, id DESC LIMIT 500"
    )))
    .bind(now_ms())
    .bind(realm)
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    let mut titles = HashMap::<String, String>::new();
    let mut out = Vec::new();
    for r in rows {
        let id: i64 = r.get("id");
        let reference: String = r.get("ref");
        let context: Value = serde_json::from_str(&r.get::<String, _>("context")).unwrap_or(Value::Null);
        let mut parts = reference.splitn(3, ':');
        let (kind, session, call) = match (parts.next(), parts.next(), parts.next()) {
            (Some("approval"), Some(s), Some(c)) => ("approval", Some(s.to_owned()), Some(c.to_owned())),
            (Some("question"), Some(s), Some(c)) => ("question", Some(s.to_owned()), Some(c.to_owned())),
            _ => ("decision", context["session"].as_str().map(str::to_owned), None),
        };
        let detail = match &session {
            Some(s) => session_title(st, &mut titles, s).await,
            None => String::new(),
        };
        out.push(api::AttentionItem {
            key: format!("decision:{id}"),
            kind: kind.into(),
            title: r.get("title"),
            detail,
            at: r.get("updated_at"),
            session_id: session,
            call_id: call,
            decision_id: Some(id),
            realm: Some(r.get("realm")),
            ..Default::default()
        });
    }
    Ok(out)
}

async fn session_title(st: &HeadState, titles: &mut HashMap<String, String>, id: &str) -> String {
    if let Some(t) = titles.get(id) {
        return t.clone();
    }
    let t = match st.store.session(id).await {
        Ok(Some(s)) if !s.title.trim().is_empty() => s.title,
        Ok(Some(_)) => "Untitled session".into(),
        _ => String::new(),
    };
    titles.insert(id.to_owned(), t.clone());
    t
}

/// Sessions whose last turn ended (or that failed) since you saw them, as
/// work tells them (docs/work.md §3): not archived, not running, nothing
/// in them waiting on you.
async fn turns(st: &HeadState, realm: Option<&str>, waiting: &[api::AttentionItem]) -> anyhow::Result<Vec<api::AttentionItem>> {
    // The few that might be: their turn ended after you last saw them.
    let rows = sqlx::query(
        "SELECT id, realm, turn_ended_at FROM sessions WHERE archived = 0 AND parent_id IS NULL AND (? IS NULL OR realm = ?) \
         AND turn_ended_at > seen_at ORDER BY turn_ended_at DESC LIMIT ?",
    )
    .bind(realm)
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
        let l = crate::work::session_live(st, &realm, &id).await?;
        if l.seen || !matches!(l.state.as_str(), "idle" | "failed") {
            continue;
        }
        out.push(api::AttentionItem {
            key: format!("session:{id}"),
            kind: if l.state == "failed" { "failed" } else { "turn_ended" }.into(),
            detail: l.title.clone(),
            title: l.title,
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

/// The Brief's row: how many people wait on you there, if any.
async fn brief(st: &HeadState, realm: &str) -> Option<api::AttentionItem> {
    let m = crate::knowledge::get(st)?.of_realm(realm).await.ok()?;
    let (people, at) = match c6_memex::requests::waiting_people(&m).await {
        Ok(n) => n,
        Err(e) => {
            warn!(realm = %realm, "counting the Brief's requests: {e:#}");
            return None;
        }
    };
    (people > 0).then(|| api::AttentionItem {
        key: "brief".into(),
        kind: "brief".into(),
        title: "People waiting on you in the Brief".into(),
        at,
        realm: Some(realm.to_owned()),
        count: Some(people as u64),
        ..Default::default()
    })
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
    let mut out = Vec::new();
    for r in crate::realms::list(st, &Caller::root()).await? {
        let console = r.id == crate::realms::ROOT;
        if (console && !caller.browser) || !(caller.sees_others() || caller.sees(&r.id)) {
            continue;
        }
        let waiting = waiting_count(st, &r.id).await?;
        out.push(api::RealmWaiting { id: r.id, name: r.name, waiting: waiting.max(0) as u64 });
    }
    Ok(out)
}

/// You've seen these worth-a-look items (none named: every one the caller
/// sees). A turn that ended or failed is marked seen, as opening its
/// session does. What waits on you is never cleared here (it leaves once
/// answered, snoozed or dismissed); nor is an agent's answer to a comment
/// (it leaves when you reply or resolve the thread) or the Brief's row
/// (when the people in it are answered there).
pub async fn seen(st: &HeadState, caller: &Caller, keys: &[String]) -> anyhow::Result<()> {
    let keys = if keys.is_empty() { get(st, caller).await?.worth.into_iter().map(|i| i.key).collect() } else { keys.to_vec() };
    let now = now_ms();
    for key in &keys {
        let Some(id) = key.strip_prefix("session:") else { continue };
        // Only the caller's own (the console's: root's).
        match st.store.session(id).await? {
            Some(s) if caller.listing_realm().is_none_or(|r| r == s.realm) => crate::work::seen_session(st, id, now).await?,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
