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
