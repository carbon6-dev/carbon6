//! How dev work starts (docs/dev-starts.md): the template a project or work
//! item starts with, the worker a pool picks, the place a work item's work
//! lives, and what becomes of its worktree when the work ends.

use std::time::Duration;

use c6_api as api;
use c6_api::starts::{StartPlan, StartQuery, WorkPlace, WorktreeEnd, WorktreeEnded, WorktreeOptions};
use c6_core::{WorkerId, now_ms};
use c6_proto::session_event;
use serde_json::json;
use tracing::{info, warn};

use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};

/// The "ask" decision's answers.
pub const REMOVE: &str = "Remove";
pub const KEEP: &str = "Keep";

/// How long the worker may take to remove a worktree and run the hooks.
const END_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A template named as a "start with" must exist in the realm.
pub async fn check_template(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<api::Template> {
    template(st, realm, name).await?.ok_or_else(|| bad_request(format!("no template named {name:?} in realm {realm}")))
}

async fn template(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<Option<api::Template>> {
    Ok(crate::templates::templates_in(st, realm, None).await?.into_iter().map(|(t, _)| t).find(|t| t.name == name))
}

/// Running sessions per worker: how busy each is.
async fn busy(st: &HeadState) -> anyhow::Result<std::collections::HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT worker_id, count(*) FROM sessions WHERE archived = 0 AND status IN ('starting', 'running') GROUP BY worker_id",
    )
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// A pick-one template's pool in `realm` (docs/dev-starts.md §3): its named
/// workers and those with any of its labels (none named: every worker),
/// as (worker, online).
pub fn pool<'a>(t: &api::Template, workers: &'a [api::Worker]) -> Vec<&'a api::Worker> {
    let named = |w: &api::Worker| t.workers.iter().any(|n| n == &w.name || n == "*");
    let labelled = |w: &api::Worker| t.worker_labels.iter().any(|l| w.labels.contains(l));
    let open = t.workers.is_empty() && t.worker_labels.is_empty();
    workers.iter().filter(|w| w.status != api::WorkerStatus::Revoked && (open || named(w) || labelled(w))).collect()
}

/// Of `candidates`, the online one running the fewest sessions (by name on
/// a tie); `None` when none is online.
pub fn least_busy<'a>(candidates: &[&'a api::Worker], busy: &std::collections::HashMap<String, i64>) -> Option<&'a api::Worker> {
    candidates
        .iter()
        .filter(|w| w.status == api::WorkerStatus::Online)
        .min_by_key(|w| (busy.get(&w.id.to_string()).copied().unwrap_or(0), w.name.clone()))
        .copied()
}

/// What the pool is, in words: "dev-vm" or "rust-dev-01, rust-dev-02".
fn pool_name(t: &api::Template) -> String {
    let mut v: Vec<String> = t.worker_labels.iter().map(|l| format!("label {l}")).collect();
    v.extend(t.workers.iter().cloned());
    if v.is_empty() { "the workers".into() } else { v.join(", ") }
}

/// The worker a pick-one template takes in `realm`, and why.
pub async fn pick(st: &HeadState, t: &api::Template, realm: &str) -> anyhow::Result<(api::Worker, String)> {
    let workers = sessions_workers(st, realm).await?;
    let candidates = pool(t, &workers);
    let busy = busy(st).await?;
    let w = least_busy(&candidates, &busy)
        .ok_or_else(|| bad_request(format!("template {:?}: no worker of {} is online", t.name, pool_name(t))))?;
    let n = busy.get(&w.id.to_string()).copied().unwrap_or(0);
    let why = if candidates.len() > 1 {
        format!("{}, the least busy of {} ({n} running)", w.name, pool_name(t))
    } else {
        format!("{}, the only one of {}", w.name, pool_name(t))
    };
    Ok((w.clone(), why))
}

/// The workers that run sessions for `realm`.
async fn sessions_workers(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::Worker>> {
    Ok(crate::api::workers_snapshot(st)
        .await?
        .into_iter()
        .filter(|w| !matches!(w.role.as_str(), "head" | "memex") && crate::realms::serves(&w.realms, realm))
        .collect())
}

/// What a new session for some work opens with (`GET /api/v1/starts`).
pub async fn plan(st: &HeadState, caller: &Caller, q: &StartQuery) -> anyhow::Result<StartPlan> {
    let asked = q.work.trim();
    let (project, work) = match asked.strip_prefix("project:") {
        Some(p) => (Some(p.to_owned()), None),
        None if asked.is_empty() => (None, None),
        None => {
            let (realm, f) = crate::work::fields_of(st, asked).await.ok_or_else(|| not_found(format!("no work item {asked}")))?;
            if !caller.sees(&realm) {
                return Err(not_found(format!("no work item {asked}")));
            }
            (Some(f.project.clone()).filter(|p| !p.is_empty()), Some((realm, f)))
        }
    };
    let project = match project {
        Some(p) => crate::projects::load(st, &p).await?.filter(|r| caller.sees(&r.realm)),
        None => None,
    };
    let realm = match (&work, &project) {
        (Some((r, _)), _) => r.clone(),
        (None, Some(p)) => p.realm.clone(),
        _ => caller.realm_for_new(None)?,
    };
    // The template: asked for, the work's, else its project's.
    let name = [
        q.template.trim(),
        work.as_ref().map_or("", |(_, f)| f.start_with.as_str()),
        project.as_ref().map_or("", |p| p.fields.start_with.as_str()),
    ]
    .into_iter()
    .find(|n| !n.is_empty())
    .unwrap_or("")
    .to_owned();
    let t = if name.is_empty() { None } else { template(st, &realm, &name).await? };
    // What names the branch: the work (its to-dos share its worktree),
    // else the to-do, else the first message.
    let todo = if q.todo.is_empty() { None } else { crate::work::todo_text(st, asked, &q.todo).await };
    let title = work.as_ref().map(|(_, f)| f.title.clone()).or(todo).unwrap_or_else(|| q.text.lines().next().unwrap_or("").to_owned());
    let mut out = StartPlan::default();
    if let Some(t) = &t {
        out.template = t.name.clone();
        out.cwd = t.cwd.trim().to_owned();
        out.worktree = t.worktree;
        out.options =
            WorktreeOptions { branch: api::starts::branch_name(&t.worktree_options.branch, &title, None), ..t.worktree_options.clone() };
        out.connection_id = connection_id(st, &t.connection).await;
        out.model = t.model.trim().to_owned();
        out.effort = t.effort.trim().to_owned();
        out.labels = t.labels.clone();
        out.labels.push(format!("template:{}", t.name));
        out.prompt = crate::templates::fill(&t.prompt, "", "").trim().to_owned();
    }
    let workers = sessions_workers(st, &realm).await?;
    let chosen = q.worker.trim();
    let by_id_or_name = |s: &str| workers.iter().find(|w| w.id.to_string() == s || w.name == s);
    // The work's place: back there, unless you picked another worker.
    if let Some(place) = work.as_ref().and_then(|(_, f)| f.place.clone()) {
        let there = by_id_or_name(&place.worker_id);
        let elsewhere = by_id_or_name(chosen).filter(|w| w.id.to_string() != place.worker_id);
        match (elsewhere, there) {
            (Some(w), _) => {
                out.worker_id = Some(w.id);
                out.worker_name = w.name.clone();
                out.worktree = true;
                out.options.branch = place.branch.clone();
                if out.cwd.is_empty() {
                    out.cwd = place.repo_root.clone();
                }
                out.why = format!("{}: a fresh worktree of {} (from its remote, if it was pushed)", w.name, place.branch);
            }
            (None, Some(w)) => {
                out.worker_id = Some(w.id);
                out.worker_name = w.name.clone();
                out.cwd = place.path.clone();
                out.worktree = false;
                out.reuse = true;
                out.options = WorktreeOptions { branch: place.branch.clone(), cleanup: place.cleanup.clone(), ..Default::default() };
                out.offline = w.status != api::WorkerStatus::Online;
                out.why = if out.offline {
                    format!(
                        "{}, where this work's worktree is, is offline: wait for it, or pick another worker for a fresh worktree of {} (from its remote, if it was pushed)",
                        w.name, place.branch
                    )
                } else {
                    format!("{}, where this work's worktree is ({})", w.name, place.branch)
                };
            }
            (None, None) => {
                out.offline = true;
                out.why = format!("{}, where this work's worktree was, is gone: pick another worker", place.worker);
            }
        }
        return Ok(out);
    }
    let Some(t) = &t else { return Ok(out) };
    let (w, why) = if let Some(w) = by_id_or_name(chosen) {
        (w.clone(), format!("{}, as you picked", w.name))
    } else if t.pick {
        pick(st, t, &realm).await?
    } else if let Some(w) = t.workers.iter().filter(|n| *n != "*").find_map(|n| workers.iter().find(|w| &w.name == n)) {
        (w.clone(), format!("{}, named by template {}", w.name, t.name))
    } else {
        let w = workers.iter().find(|w| w.status == api::WorkerStatus::Online).ok_or_else(|| bad_request("no worker is online"))?;
        (w.clone(), format!("{}, the online worker", w.name))
    };
    out.offline = w.status != api::WorkerStatus::Online;
    out.worker_id = Some(w.id);
    out.worker_name = w.name.clone();
    out.why = why;
    Ok(out)
}

/// A connection named in a template, by name or id, as an id.
async fn connection_id(st: &HeadState, c: &str) -> String {
    let c = c.trim();
    if c.is_empty() {
        return String::new();
    }
    st.store.connections().await.unwrap_or_default().into_iter().find(|x| x.name == c || x.id == c).map(|x| x.id).unwrap_or_default()
}

/// A session made a worktree as a template said: what to do with it at the
/// end, until it records its work's place.
pub async fn remember(st: &HeadState, session_id: &str, o: &WorktreeOptions, template: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO worktree_starts (session_id, cleanup, template) VALUES (?, ?, ?)")
        .bind(session_id)
        .bind(o.cleanup_mode())
        .bind(template)
        .execute(&st.store.pool)
        .await?;
    Ok(())
}

async fn remembered(st: &HeadState, session_id: &str) -> Option<(String, String)> {
    sqlx::query_as("SELECT cleanup, template FROM worktree_starts WHERE session_id = ?")
        .bind(session_id)
        .fetch_optional(&st.store.pool)
        .await
        .ok()
        .flatten()
}

/// Where a session's worktree is, from its first event.
async fn worktree_of(st: &HeadState, session_id: &str) -> Option<c6_proto::SessionStarted> {
    st.store.events_after(session_id, 0, 1).await.ok()?.into_iter().find_map(|e| match e.kind {
        Some(session_event::Kind::Started(s)) if !s.worktree_branch.is_empty() => Some(s),
        _ => None,
    })
}

/// Events came in: a session that made its worktree records its work's
/// place.
pub async fn on_events(st: &HeadState, session_id: &str, events: &[c6_proto::SessionEvent]) {
    if events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Started(s)) if !s.worktree_branch.is_empty())) {
        record_place(st, session_id).await;
    }
}

/// The first session of a work item made through a template records where
/// the work lives (docs/dev-starts.md §4). Called when it starts and when
/// it joins its work, whichever comes last.
pub async fn record_place(st: &HeadState, session_id: &str) {
    let Some((cleanup, template)) = remembered(st, session_id).await else { return };
    let Some(started) = worktree_of(st, session_id).await else { return };
    let Ok(Some(row)) = st.store.session(session_id).await else { return };
    let Ok(held) = crate::work::holding(st, &format!("session:{session_id}")).await else { return };
    for work in held {
        let place = WorkPlace {
            worker_id: row.worker_id.clone(),
            worker: row.worker_name.clone(),
            path: started.cwd.clone(),
            branch: started.worktree_branch.clone(),
            repo_root: started.repo_root.clone(),
            cleanup: cleanup.clone(),
            template: template.clone(),
            session: session_id.to_owned(),
            at: now_ms(),
        };
        match crate::work::set_place(st, &work, Some(place), true).await {
            Ok(true) => info!(%work, session = %session_id, branch = %started.worktree_branch, "work's place recorded"),
            Ok(false) => {}
            Err(e) => warn!(%work, "recording its place: {e:#}"),
        }
    }
}

/// A work item was closed: its place goes as its template says.
pub async fn work_ended(st: &HeadState, work: &str) {
    let Some((realm, f)) = crate::work::fields_of(st, work).await else { return };
    let Some(place) = f.place else { return };
    ended(st, &realm, &format!("work:{work}"), &f.title, place).await;
}

/// A session was archived: one in no work whose worktree a template made
/// goes as the template says.
pub async fn session_archived(st: &HeadState, session_id: &str) {
    let Some((cleanup, template)) = remembered(st, session_id).await else { return };
    if crate::work::holding(st, &format!("session:{session_id}")).await.is_ok_and(|h| !h.is_empty()) {
        return;
    }
    let Some(started) = worktree_of(st, session_id).await else { return };
    let Ok(Some(row)) = st.store.session(session_id).await else { return };
    let place = WorkPlace {
        worker_id: row.worker_id.clone(),
        worker: row.worker_name.clone(),
        path: started.cwd.clone(),
        branch: started.worktree_branch.clone(),
        repo_root: started.repo_root.clone(),
        cleanup,
        template,
        session: session_id.to_owned(),
        at: now_ms(),
    };
    let _ = sqlx::query("DELETE FROM worktree_starts WHERE session_id = ?").bind(session_id).execute(&st.store.pool).await;
    ended(st, &row.realm, &format!("session:{session_id}"), &row.title, place).await;
}

/// The end of `owner` (`work:<id>` or `session:<id>`): off runs the hooks,
/// ask opens a decision, auto removes what is safe to.
async fn ended(st: &HeadState, realm: &str, owner: &str, title: &str, place: WorkPlace) {
    match place.cleanup.as_str() {
        "ask" => {
            let d = api::DecisionIn {
                kind: "choose".into(),
                title: format!("Remove the worktree and branch {}?", place.branch),
                document: format!(
                    "{title} has ended. Its worktree is {} on {}. Remove takes it and the branch away, only if nothing would be lost (no uncommitted changes; its commits on a remote or merged); Keep leaves them.",
                    place.path, place.worker
                ),
                context: json!({"owner": owner, "title": title, "place": place}),
                source: owner.to_owned(),
                priority: "low".into(),
                group: "Worktrees".into(),
                options: vec![
                    api::DecisionOption {
                        label: REMOVE.into(),
                        does: "Removes the worktree and its branch, if nothing would be lost.".into(),
                    },
                    api::DecisionOption { label: KEEP.into(), does: "Leaves them where they are.".into() },
                ],
                ..Default::default()
            };
            let reference = format!("worktree:{}:{}", owner.replace(':', "-"), uuid::Uuid::now_v7().simple());
            if let Err(e) = crate::decisions::open(st, &d, realm, &reference).await {
                warn!(%owner, "asking about its worktree: {e:#}");
            }
        }
        mode => finish(st, realm, owner, title, &place, mode == "auto").await,
    }
}

/// The "ask" decision was answered.
pub async fn decided(st: &HeadState, d: &api::Decision, option: Option<&str>) -> anyhow::Result<()> {
    let place: WorkPlace = serde_json::from_value(d.context["place"].clone())?;
    let owner = d.context["owner"].as_str().unwrap_or("");
    let title = d.context["title"].as_str().unwrap_or("");
    finish(st, &d.realm, owner, title, &place, option == Some(REMOVE)).await;
    Ok(())
}

/// Has the worker remove the worktree (if `remove` and safe) and run the
/// WorkEnded hooks, tells you what's left, and forgets the place.
async fn finish(st: &HeadState, realm: &str, owner: &str, title: &str, place: &WorkPlace, remove: bool) {
    let hooks = work_ended_hooks(st, realm).await;
    let labels = match st.store.session(&place.session).await {
        Ok(Some(s)) => s.all_labels(),
        _ => Vec::new(),
    };
    let result = if !remove && hooks == "[]" {
        Ok(WorktreeEnded::default())
    } else {
        let req = WorktreeEnd {
            path: place.path.clone(),
            branch: place.branch.clone(),
            repo_root: place.repo_root.clone(),
            remove,
            hooks_json: hooks,
        };
        let mut params = serde_json::to_value(&req).unwrap_or_default();
        params["labels"] = json!(labels);
        match place.worker_id.parse().map(WorkerId) {
            Ok(id) if st.registry.get(id).is_some() => st
                .registry
                .call(id, "worktree.end", params, END_TIMEOUT)
                .await
                .and_then(|v| serde_json::from_value::<WorktreeEnded>(v).map_err(|e| e.to_string())),
            _ => Err(format!("{} is offline", place.worker)),
        }
    };
    let (message, priority) = match &result {
        Ok(r) if r.removed => (format!("Removed the worktree {} and the branch {} on {}.", place.path, place.branch, place.worker), "low"),
        Ok(r) if remove => (format!("Kept the worktree {} on {}: {}.", place.path, place.worker, r.kept.join("; ")), "normal"),
        Ok(_) => (String::new(), "low"),
        Err(e) => (format!("Nothing was removed from {}: {e}.", place.worker), "normal"),
    };
    let hook_lines = result.as_ref().map(|r| r.hooks.clone()).unwrap_or_default();
    info!(%owner, branch = %place.branch, removed = result.as_ref().is_ok_and(|r| r.removed), "work's place ended");
    if !message.is_empty() || !hook_lines.is_empty() {
        let mut text = message;
        for h in &hook_lines {
            text.push_str(&format!("\nWorkEnded hook: {h}"));
        }
        st.notifier.send(crate::notify::Note {
            event: "work",
            title: format!("{title}: its worktree"),
            message: text.trim().to_owned(),
            priority,
            session_id: Some(place.session.clone()).filter(|s| !s.is_empty()),
            labels,
            ..Default::default()
        });
    }
    if let Some(work) = owner.strip_prefix("work:")
        && let Err(e) = crate::work::set_place(st, work, None, false).await
    {
        warn!(%work, "forgetting its place: {e:#}");
    }
}

/// The realm's WorkEnded hooks, as JSON for the worker.
async fn work_ended_hooks(st: &HeadState, realm: &str) -> String {
    let raw = st.store.setting_in(crate::sessions::HOOKS, realm, "").await.ok().flatten().unwrap_or_else(|| "[]".into());
    let hooks: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    let ours: Vec<_> = hooks.into_iter().filter(|h| h["event"] == "WorkEnded").collect();
    serde_json::to_string(&ours).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(name: &str, online: bool, labels: &[&str]) -> api::Worker {
        api::Worker {
            id: uuid::Uuid::now_v7(),
            name: name.into(),
            hostname: name.into(),
            os: String::new(),
            arch: String::new(),
            version: None,
            status: if online { api::WorkerStatus::Online } else { api::WorkerStatus::Offline },
            enrolled_at: 0,
            last_seen: None,
            connected_since: None,
            rtt_us: None,
            capabilities: Vec::new(),
            role: "worker".into(),
            realms: Vec::new(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        }
    }

    #[test]
    fn a_pool_picks_the_least_busy_online_worker() {
        let ws =
            vec![worker("a", true, &["dev-vm"]), worker("b", true, &["dev-vm"]), worker("c", false, &["dev-vm"]), worker("d", true, &[])];
        let t = api::Template { name: "t".into(), pick: true, worker_labels: vec!["dev-vm".into()], ..Default::default() };
        let p = pool(&t, &ws);
        assert_eq!(p.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        let mut busy = std::collections::HashMap::new();
        busy.insert(ws[0].id.to_string(), 2);
        busy.insert(ws[1].id.to_string(), 1);
        busy.insert(ws[2].id.to_string(), 0);
        assert_eq!(least_busy(&p, &busy).unwrap().name, "b", "c is idle but offline");
        busy.insert(ws[1].id.to_string(), 2);
        assert_eq!(least_busy(&p, &busy).unwrap().name, "a", "a tie goes by name");
        // Named workers join the labelled ones; nothing named: every worker.
        let t = api::Template { workers: vec!["d".into()], ..t };
        assert_eq!(pool(&t, &ws).len(), 4);
        let t = api::Template { workers: vec![], worker_labels: vec![], ..t };
        assert_eq!(pool(&t, &ws).len(), 4);
        let none = vec![worker("x", false, &["dev-vm"])];
        assert!(least_busy(&pool(&t, &none), &busy).is_none());
    }
}
