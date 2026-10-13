//! Self-configuration (docs/follow-through.md §5): sessions configure carbon6
//! for their project through the operation catalog, within rules that keep
//! a session from widening its own powers; the setup playbook runs once per
//! repo by itself; and kits that stop working get a repair.

use c6_api as api;
use serde_json::{Value, json};

use crate::catalog::{self, Effect};
use crate::store::SessionRow;
use crate::{HeadState, bad_request};

pub const SETTING: &str = "followthrough";

/// Writes a session may make (reads are always fine). Kits are proposed and
/// dry-run before they act; everything else here changes no one's powers.
const SESSION_WRITES: [&str; 14] = [
    "settings.kits.put",
    "todos.add",
    "todos.edit",
    "trackers.stop",
    "trackers.investigate",
    "memex.notes.create",
    "memex.trails.append",
    "memex.trails.update",
    "knowledge.record",
    "knowledge.set",
    "memories.create",
    "memories.update",
    "sessions.update",
    "sessions.labels.set",
];

pub async fn settings(st: &HeadState) -> anyhow::Result<api::FollowThroughSettings> {
    Ok(serde_json::from_str(&st.store.setting(SETTING).await?.unwrap_or_default()).unwrap_or_default())
}

/// A worker session calling the catalog (`c6.discover`, `c6.describe`,
/// `c6.call`).
pub async fn session_call(st: &HeadState, session: &SessionRow, method: &str, params: &Value) -> anyhow::Result<Value> {
    match method {
        "discover" => {
            Ok(json!(catalog::discover_text(params["query"].as_str().unwrap_or(""), params["area"].as_str().filter(|a| !a.is_empty()))))
        }
        "describe" => {
            let name = params["op"].as_str().unwrap_or("");
            let op = catalog::find(name).ok_or_else(|| bad_request(format!("no operation {name:?}; use c6_discover")))?;
            Ok(catalog::describe(op))
        }
        "call" => {
            let name = params["op"].as_str().unwrap_or("");
            let op = catalog::find(name).ok_or_else(|| bad_request(format!("no operation {name:?}; use c6_discover")))?;
            let input = if params["input"].is_object() { params["input"].clone() } else { json!({}) };
            match op.effect {
                Effect::Read => crate::assistant::execute(st, op, input, &format!("session:{}", session.id)).await,
                Effect::Secret => Err(bad_request(format!(
                    "{} returns a secret, which only the user sees: ask them (todo_add, kind provide) or have them use the assistant",
                    op.name
                ))),
                Effect::Destructive => Err(bad_request(format!(
                    "{} is destructive; sessions do not do that themselves. Raise it for the user with todo_add.",
                    op.name
                ))),
                Effect::Write if op.name == "settings.kits.put" => put_kits(st, session, &input).await,
                Effect::Write if SESSION_WRITES.contains(&op.name) => {
                    crate::assistant::execute(st, op, input, &format!("session:{}", session.id)).await
                }
                Effect::Write => Err(bad_request(format!(
                    "{} changes how sessions work (their tools, permissions or credentials); only the user does that. Raise it with todo_add.",
                    op.name
                ))),
            }
        }
        other => Err(bad_request(format!("unknown c6.{other}"))),
    }
}

/// A session's change to the kits: it may add or change kits, not remove
/// them. What it changed is proposed, dry-run on its worker, then active
/// (with an action item to review it) or broken (with one to fix it).
async fn put_kits(st: &HeadState, session: &SessionRow, input: &Value) -> anyhow::Result<Value> {
    let mut new: Vec<api::Kit> = serde_json::from_value(input["body"].clone()).map_err(|e| bad_request(format!("body: {e}")))?;
    let old = crate::followups::kits(st).await?;
    let gone: Vec<&str> = old.iter().filter(|o| !new.iter().any(|n| n.name == o.name)).map(|o| o.name.as_str()).collect();
    if !gone.is_empty() {
        return Err(bad_request(format!(
            "sessions do not remove kits ({}); send every kit back, and raise removing it with todo_add",
            gone.join(", ")
        )));
    }
    let mut changed = Vec::new();
    for k in new.iter_mut() {
        match old.iter().find(|o| o.name == k.name) {
            Some(o) if same(o, k) => k.state = o.state.clone(),
            _ => {
                k.state = "proposed".into();
                changed.push(k.name.clone());
            }
        }
    }
    crate::followups::check_kits(&new)?;
    st.store.set_setting(crate::followups::KITS, &serde_json::to_string(&new)?).await?;
    let mut results = Vec::new();
    for name in &changed {
        let r = crate::followups::dry_run(
            st,
            &api::KitDryRun { name: name.clone(), session_id: session.id.clone(), update_state: true, ..Default::default() },
        )
        .await;
        let ok = r.as_ref().is_ok_and(|v| v["ok"] == true);
        if ok {
            crate::todos::add(
                st,
                &api::TodoIn {
                    kind: "kit".into(),
                    title: format!("Review kit {name}"),
                    detail: format!(
                        "Session \"{}\" set it up and its dry run passed, so it is active. Look it over in Settings → Projects when you like.",
                        session.title
                    ),
                    context: json!({"kits": [name], "session": session.id}),
                    source: format!("session:{}", session.id),
                    priority: "low".into(),
                    group: format!("kit-review:{name}"),
                },
            )
            .await?;
        }
        results.push(json!({"kit": name, "active": ok, "dry_run": r.unwrap_or_else(|e| json!({"error": format!("{e:#}")}))}));
    }
    Ok(json!({"saved": true, "changed": results}))
}

fn same(a: &api::Kit, b: &api::Kit) -> bool {
    let strip = |k: &api::Kit| {
        let mut k = k.clone();
        k.state.clear();
        serde_json::to_value(k).unwrap_or_default()
    };
    strip(a) == strip(b)
}

/// The setup playbook (a generic procedure; it knows no CI or forge).
pub fn playbook(repo: &str) -> String {
    format!(
        "Set up follow-through for {repo} (docs/follow-through.md). Work on your own; never stop to ask the user: what you cannot settle \
becomes an action item (todo_add), and you carry on with your best assumption.

1. What is known: knowledge_list and memex_search for this repo and the systems it touches (CI, forge, clusters, networks). If kits \
already cover another repo with the same CI, start from them (c6_call settings.kits.get).
2. The repo: CI, deploy and infrastructure files (workflow templates, Kubernetes/Helm/Kustomize, Makefiles, docs) for what builds and \
tests it, where, and how a run is tied to a commit (labels, parameters, statuses).
3. The forge: on a recent pushed commit, the statuses and checks (they name the CI and link to it), required checks, webhooks (e.g. gh \
api repos/<owner>/<name>/commits/<sha>/status, gh api …/check-runs).
4. The network: which carbon6 workers can reach the CI's address (c6_call workers.list; try the address from each where you can), and \
which CLIs and credentials each has (argo, kubectl, gh…).
5. Write it down as you go: knowledge_record for every fact (observed with evidence, or inferred with what it rests on), with a check \
command when one proves it and where it applies.
6. Write a kit (c6_describe settings.kits.put; c6_call settings.kits.put with every existing kit plus yours) for {repo}: a reaction to \
pushes that offers opening a PR when there is none, and a tracker for the CI run (probing from a worker that reaches it; print \
{{\"phase\",\"line\",\"url\"}} JSON or map the output with phase rules; a logs command for investigations). It is dry-run when saved: if it is \
broken, fix it and save again.
7. Finish with a short summary of what you set up and what is left for the user (the action items you raised)."
    )
}

/// A session started: if its repo has no kit and was never set up, set it
/// up in a side chat (once per repo).
pub async fn on_started(st: &HeadState, session_id: &str, seq: u64) -> anyhow::Result<()> {
    let Some(session) = st.store.session(session_id).await? else { return Ok(()) };
    if session.parent_id.is_some() || session.parent_call_id.is_some() {
        return Ok(());
    }
    let labels = session.all_labels();
    if labels.iter().any(|l| l == crate::assistant::LABEL || l == "todo") {
        return Ok(());
    }
    let Some(repo) = labels.iter().find(|l| l.starts_with("repo:")).cloned() else { return Ok(()) };
    let mut s = settings(st).await?;
    if !s.auto_setup.unwrap_or(true) || s.set_up.contains(&repo) {
        return Ok(());
    }
    let kits = crate::followups::kits(st).await?;
    if kits.iter().any(|k| k.state != "disabled" && !k.applies_to.is_empty() && api::labels::applies(&k.applies_to, &labels)) {
        return Ok(());
    }
    s.set_up.push(repo.clone());
    st.store.set_setting(SETTING, &serde_json::to_string(&s)?).await?;
    start_setup(st, &session, seq, &repo).await?;
    Ok(())
}

pub async fn start_setup(st: &HeadState, session: &SessionRow, seq: u64, repo: &str) -> anyhow::Result<api::Session> {
    crate::sessions::create_side_chat(
        st,
        &session.id,
        api::CreateSideChat {
            anchor_seq: seq,
            connection_id: session.connection_id.clone(),
            model: session.model.clone(),
            effort: None,
            prompt: playbook(repo),
        },
    )
    .await
}

/// `POST /api/v1/kits/setup`: set a session's repo up now.
pub async fn setup_now(st: &HeadState, session_id: &str) -> anyhow::Result<api::Session> {
    let session = st.store.session(session_id).await?.ok_or_else(|| crate::not_found("no such session"))?;
    let repo = session
        .all_labels()
        .into_iter()
        .find(|l| l.starts_with("repo:"))
        .ok_or_else(|| bad_request("that session is not in a repository"))?;
    let mut s = settings(st).await?;
    if !s.set_up.contains(&repo) {
        s.set_up.push(repo.clone());
        st.store.set_setting(SETTING, &serde_json::to_string(&s)?).await?;
    }
    start_setup(st, &session, session.last_seq as u64, &repo).await
}

/// A tracker's probe keeps failing (or nothing ever showed up): a side chat
/// works out what changed and fixes the kit.
pub async fn repair(st: &HeadState, session: &SessionRow, t: &api::Tracker, def: &api::TrackerDef, why: &str) -> anyhow::Result<String> {
    let prompt = format!(
        "The kit \"{}\" may be broken: its tracker \"{}\" ({}) {why}.\n\nIts definition:\n```json\n{}\n```\n\nWork out what changed: check the \
environment knowledge about these systems (knowledge_list), re-run the probe by hand where it runs, and compare. Record what you find \
(knowledge_record; contradicts for facts that no longer hold). Then fix the kit with c6_call settings.kits.put (send every kit back; it \
is dry-run when saved). If it needs the user (a credential, access), raise it with todo_add and say what you assumed.",
        t.kit,
        t.name,
        t.title,
        serde_json::to_string_pretty(def).unwrap_or_default()
    );
    let side = crate::sessions::create_side_chat(
        st,
        &session.id,
        api::CreateSideChat {
            anchor_seq: t.anchor_seq,
            connection_id: session.connection_id.clone(),
            model: session.model.clone(),
            effort: None,
            prompt,
        },
    )
    .await?;
    Ok(side.id)
}
