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

// --- Playbooks ----------------------------------------------------------------

pub const PLAYBOOKS: &str = "playbooks";
/// Which playbooks already ran for which key (first_session: once per).
pub const RUNS: &str = "playbook_runs";
const TRIGGERS: [&str; 4] = ["first_session", "tracker_failed", "probe_failing", "todo_session"];

/// What carbon6 starts with: editable (Settings → Projects), not built in.
pub fn default_playbooks() -> Vec<api::Playbook> {
    let p = |name: &str, trigger: &str, once_per: &str, description: &str, prompt: &str| api::Playbook {
        name: name.into(),
        trigger: trigger.into(),
        applies_to: Vec::new(),
        once_per: once_per.into(),
        prompt: prompt.into(),
        description: description.into(),
        enabled: true,
    };
    vec![
        p(
            "Set up follow-through",
            "first_session",
            "repo",
            "The first session in a repository: work out how it is built and tested, and write a kit.",
            "Set up follow-through for {{key}} (docs/follow-through.md). Work on your own; never stop to ask the user: what you cannot settle \
becomes an action item (todo_add), and you carry on with your best assumption.

1. What is known: knowledge_list and memex_search for this repo and the systems it touches (CI, forge, clusters, networks). If kits \
already cover another repo with the same CI, start from them (c6_call settings.kits.get).
2. The repo: CI, deploy and infrastructure files (workflow templates, Kubernetes/Helm/Kustomize, Makefiles, docs) for what builds and \
tests it, where, and how a run is tied to a commit (labels, parameters, statuses).
3. The forge: on a recent pushed commit, the statuses and checks (they name the CI and link to it), required checks, webhooks.
4. The network: which carbon6 workers can reach the CI's address (c6_call workers.list; try the address from each where you can), and \
which CLIs and credentials each has.
5. Write it down as you go: knowledge_record for every fact (observed with evidence, or inferred with what it rests on), with a check \
command when one proves it and where it applies.
6. Write a kit (c6_describe settings.kits.put; c6_call settings.kits.put with every existing kit plus yours) for {{key}}: a reaction to \
pushes that offers opening a PR when there is none, and a tracker for the CI run (probing from a worker that reaches it; print \
{\"phase\",\"line\",\"url\"} JSON or map the output with phase rules; a logs command for investigations). It is dry-run when saved: if it is \
broken, fix it and save again.
7. Finish with a short summary of what you set up and what is left for the user (the action items you raised).",
        ),
        p(
            "Investigate a failure",
            "tracker_failed",
            "",
            "A tracker ended failed: explain why, and propose a fix on a branch of its own.",
            "The tracker \"{{tracker.title}}\" (kit {{tracker.kit}}) ended {{tracker.phase}}: {{tracker.line}}\n{{tracker.url}}\n\n{{logs}}\n\n\
Investigate why it failed and explain the cause briefly. Then propose a fix: create a new branch off the failing commit {{sha}} named \
fix/<branch>-<short sha>, make the change there, commit it, and show the diff. Do not push. Finish by offering next steps with the \
suggest tool: pushing the fix branch (and opening a PR), or applying the change to the working branch.",
        ),
        p(
            "Repair a kit",
            "probe_failing",
            "",
            "A tracker's probe keeps failing (or nothing ever showed up): work out what changed and fix the kit.",
            "The kit \"{{tracker.kit}}\" may be broken: its tracker \"{{tracker.name}}\" ({{tracker.title}}) {{why}}.\n\nIts definition:\n\
```json\n{{definition}}\n```\n\nWork out what changed: check the environment knowledge about these systems (knowledge_list), re-run the \
probe by hand where it runs, and compare. Record what you find (knowledge_record; contradicts for facts that no longer hold). Then fix the \
kit with c6_call settings.kits.put (send every kit back; it is dry-run when saved). If it needs the user (a credential, access), raise it \
with todo_add and say what you assumed.",
        ),
        p(
            "Work on an action item",
            "todo_session",
            "",
            "You started a session from an action item: help settle it with its context.",
            "Help me settle this action item.\n\nAction item #{{todo.id}} ({{todo.kind}}): {{todo.title}}\n{{todo.detail}}\n\nContext: \
{{todo.context}}\n\nIf it names facts, read them with knowledge_list (by id) and their evidence; re-check what you can by running their \
checks here. Then ask me what you need, settle it with me, and record the outcome: knowledge_set for facts I confirm, correct or retire, \
knowledge_record for anything new, and todo_resolve with a one-line resolution when we are done.",
        ),
    ]
}

/// The playbooks: yours, or what carbon6 starts with (until you save them).
/// The earlier on/off switch for setting up repos carries over.
pub async fn playbooks(st: &HeadState) -> anyhow::Result<Vec<api::Playbook>> {
    if let Some(saved) = st.store.setting(PLAYBOOKS).await? {
        return Ok(serde_json::from_str(&saved).unwrap_or_default());
    }
    let mut d = default_playbooks();
    if settings(st).await?.auto_setup == Some(false) {
        for p in d.iter_mut().filter(|p| p.trigger == "first_session") {
            p.enabled = false;
        }
    }
    Ok(d)
}

pub fn check_playbooks(list: &[api::Playbook]) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for p in list {
        if p.name.trim().is_empty() || !names.insert(p.name.trim().to_owned()) {
            return Err(bad_request(format!("playbook names must be unique and not empty ({:?})", p.name)));
        }
        if !TRIGGERS.contains(&p.trigger.as_str()) {
            return Err(bad_request(format!("playbook {}: the trigger is one of {}", p.name, TRIGGERS.join(", "))));
        }
        if p.prompt.trim().is_empty() {
            return Err(bad_request(format!("playbook {} needs a prompt", p.name)));
        }
        crate::sessions::check_selector(&p.applies_to)?;
    }
    Ok(())
}

/// The most specific enabled playbook for this trigger and these labels.
pub async fn pick(st: &HeadState, trigger: &str, labels: &[String]) -> anyhow::Result<Option<api::Playbook>> {
    let mut found: Vec<api::Playbook> = playbooks(st)
        .await?
        .into_iter()
        .filter(|p| p.enabled && p.trigger == trigger && api::labels::applies(&p.applies_to, labels))
        .collect();
    found.sort_by_key(|p| std::cmp::Reverse((p.applies_to.len(), p.applies_to.iter().map(String::len).sum::<usize>())));
    Ok(found.into_iter().next())
}

/// Which keys a playbook already ran for.
pub async fn runs(st: &HeadState) -> anyhow::Result<std::collections::BTreeMap<String, Vec<String>>> {
    if let Some(saved) = st.store.setting(RUNS).await? {
        return Ok(serde_json::from_str(&saved).unwrap_or_default());
    }
    // From before playbooks: the repos already set up.
    let mut m = std::collections::BTreeMap::new();
    let old = settings(st).await?.set_up;
    if !old.is_empty() {
        m.insert(default_playbooks()[0].name.clone(), old);
    }
    Ok(m)
}

async fn mark_run(st: &HeadState, name: &str, key: &str) -> anyhow::Result<bool> {
    let mut r = runs(st).await?;
    let keys = r.entry(name.to_owned()).or_default();
    if keys.iter().any(|k| k == key) {
        return Ok(false);
    }
    keys.push(key.to_owned());
    st.store.set_setting(RUNS, &serde_json::to_string(&r)?).await?;
    Ok(true)
}

/// A session started: the first_session playbook for its labels, once per
/// value of its once_per label.
pub async fn on_started(st: &HeadState, session_id: &str, seq: u64) -> anyhow::Result<()> {
    let Some(session) = st.store.session(session_id).await? else { return Ok(()) };
    if session.parent_id.is_some() || session.parent_call_id.is_some() {
        return Ok(());
    }
    let labels = session.all_labels();
    if labels.iter().any(|l| l == crate::assistant::LABEL || l == "todo") {
        return Ok(());
    }
    let Some(book) = pick(st, "first_session", &labels).await? else { return Ok(()) };
    let Some(key) = once_key(&book, &labels) else { return Ok(()) };
    // A repo with a kit already is set up.
    let kits = crate::followups::kits(st).await?;
    if kits.iter().any(|k| k.state != "disabled" && !k.applies_to.is_empty() && api::labels::applies(&k.applies_to, &labels)) {
        return Ok(());
    }
    if !mark_run(st, &book.name, &key).await? {
        return Ok(());
    }
    run_first_session(st, &session, &book, seq, &key).await?;
    Ok(())
}

fn once_key(book: &api::Playbook, labels: &[String]) -> Option<String> {
    let prefix = if book.once_per.is_empty() { "repo" } else { book.once_per.trim_end_matches(':') };
    labels.iter().find(|l| l.split_once(':').is_some_and(|(k, _)| k == prefix)).cloned()
}

async fn run_first_session(
    st: &HeadState,
    session: &SessionRow,
    book: &api::Playbook,
    seq: u64,
    key: &str,
) -> anyhow::Result<api::Session> {
    let ctx = json!({
        "key": key,
        "repo": key.split_once(':').map(|(_, v)| v).unwrap_or(key),
        "once_per": book.once_per,
        "session": {"id": session.id, "title": session.title, "cwd": session.cwd, "worker": session.worker_name},
        "labels": session.all_labels(),
    });
    side_chat(st, session, seq, &crate::followups::render(&book.prompt, &ctx)).await
}

async fn side_chat(st: &HeadState, session: &SessionRow, anchor: u64, prompt: &str) -> anyhow::Result<api::Session> {
    crate::sessions::create_side_chat(
        st,
        &session.id,
        api::CreateSideChat {
            anchor_seq: anchor,
            connection_id: session.connection_id.clone(),
            model: session.model.clone(),
            effort: None,
            prompt: prompt.to_owned(),
        },
    )
    .await
}

/// `POST /api/v1/kits/setup`: run a session's first_session playbook now
/// (even if it ran for its repo before).
pub async fn setup_now(st: &HeadState, session_id: &str) -> anyhow::Result<api::Session> {
    let session = st.store.session(session_id).await?.ok_or_else(|| crate::not_found("no such session"))?;
    let labels = session.all_labels();
    let book = pick(st, "first_session", &labels)
        .await?
        .ok_or_else(|| bad_request("no enabled first-session playbook applies to that session (Settings → Projects)"))?;
    let key = once_key(&book, &labels).ok_or_else(|| {
        bad_request(format!("that session has no {} label", if book.once_per.is_empty() { "repo" } else { &book.once_per }))
    })?;
    mark_run(st, &book.name, &key).await?;
    run_first_session(st, &session, &book, session.last_seq as u64, &key).await
}

/// The prompt for a failed tracker's investigation, if a playbook applies.
pub async fn investigation_prompt(
    st: &HeadState,
    session: &SessionRow,
    t: &api::Tracker,
    logs: &str,
    ctx: &Value,
) -> anyhow::Result<Option<String>> {
    let Some(book) = pick(st, "tracker_failed", &session.all_labels()).await? else { return Ok(None) };
    let c = json!({
        "tracker": t, "logs": logs, "vars": ctx["vars"],
        "sha": crate::followups::lookup(ctx, "vars.sha").cloned().unwrap_or(Value::Null),
        "session": {"id": session.id, "title": session.title, "cwd": session.cwd, "worker": session.worker_name},
    });
    Ok(Some(crate::followups::render(&book.prompt, &c)))
}

/// A tracker's probe keeps failing (or nothing ever showed up): the
/// probe_failing playbook in a side chat, if one applies.
pub async fn repair(
    st: &HeadState,
    session: &SessionRow,
    t: &api::Tracker,
    def: &api::TrackerDef,
    why: &str,
) -> anyhow::Result<Option<String>> {
    let Some(book) = pick(st, "probe_failing", &session.all_labels()).await? else { return Ok(None) };
    let c = json!({
        "tracker": t, "why": why, "definition": serde_json::to_string_pretty(def).unwrap_or_default(),
        "session": {"id": session.id, "title": session.title, "cwd": session.cwd, "worker": session.worker_name},
    });
    let side = side_chat(st, session, t.anchor_seq, &crate::followups::render(&book.prompt, &c)).await?;
    Ok(Some(side.id))
}

/// The prompt for a session started from an action item.
pub async fn todo_prompt(st: &HeadState, t: &api::Todo, labels: &[String]) -> anyhow::Result<String> {
    let c = json!({"todo": {"id": t.id, "kind": t.kind, "title": t.title, "detail": t.detail, "source": t.source,
        "context": serde_json::to_string_pretty(&t.context).unwrap_or_default()}});
    Ok(match pick(st, "todo_session", labels).await? {
        Some(book) => crate::followups::render(&book.prompt, &c),
        None => format!("Help me settle this action item: {}\n{}\n\nContext: {}", t.title, t.detail, t.context),
    })
}
