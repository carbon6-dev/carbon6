//! Session templates: a saved way to start a session (or one per target, a
//! fleet run), started by hand, on a schedule or by a webhook.

use c6_api as api;

use crate::{HeadState, bad_request, not_found};

pub const TEMPLATES: &str = "templates";

pub async fn templates(st: &HeadState) -> anyhow::Result<Vec<api::Template>> {
    let raw = st.store.setting(TEMPLATES).await?.unwrap_or_else(|| "[]".into());
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

/// The templates a realm uses: root's and its own.
pub async fn templates_for(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::Template>> {
    let raw = st.store.setting_for(TEMPLATES, realm).await?.unwrap_or_else(|| "[]".into());
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

/// The templates a realm's sessions use (`instance: None`), or an app
/// instance's, each with where it comes from (docs/apps.md §5.3).
pub async fn templates_in(
    st: &HeadState,
    realm: &str,
    instance: Option<&str>,
) -> anyhow::Result<Vec<(api::Template, crate::lists::Layer)>> {
    Ok(crate::lists::effective_tagged(&st.store.pool, TEMPLATES, realm, instance)
        .await?
        .into_iter()
        .filter_map(|(v, l)| serde_json::from_value(v).ok().map(|t| (t, l)))
        .collect())
}

/// Every template, with its realm: home's (root's apply everywhere and
/// are listed as home's), then each other realm's own and its apps'.
pub async fn all(st: &HeadState, caller: &crate::realms::Caller) -> anyhow::Result<Vec<api::RealmTemplate>> {
    let mut out = Vec::new();
    for r in crate::realms::list(st, caller).await? {
        if r.id == crate::realms::ROOT {
            continue;
        }
        let list: Vec<api::Template> = templates_in(st, &r.id, None)
            .await?
            .into_iter()
            .filter(|(_, l)| r.id == crate::realms::HOME || *l != crate::lists::Layer::Root)
            .map(|(t, _)| t)
            .collect();
        out.extend(list.into_iter().map(|template| api::RealmTemplate { realm: r.id.clone(), template }));
    }
    Ok(out)
}

/// The realm to run template `name` in when none was named: home if it
/// has one, else the one realm that does.
pub async fn realm_of(st: &HeadState, name: &str) -> anyhow::Result<String> {
    let all = all(st, &crate::realms::Caller::root()).await?;
    let realms: Vec<&str> = all.iter().filter(|t| t.template.name == name).map(|t| t.realm.as_str()).collect();
    match realms.as_slice() {
        [] => Err(not_found(format!("no template named {name:?}"))),
        r if r.contains(&crate::realms::HOME) => Ok(crate::realms::HOME.to_owned()),
        [one] => Ok((*one).to_owned()),
        several => Err(bad_request(format!("templates named {name:?} are in several realms ({}): name the realm", several.join(", ")))),
    }
}

/// Checks a list of templates before it is saved.
pub fn check(templates: &[api::Template]) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for t in templates {
        let n = t.name.trim();
        if n.is_empty() || !n.chars().all(|c| c.is_alphanumeric() || "_-.".contains(c)) {
            return Err(bad_request(format!("template name {n:?} is not valid (letters, digits, - _ .)")));
        }
        if !names.insert(n) {
            return Err(bad_request(format!("two templates are named {n:?}")));
        }
        // A subagent template may leave it to the Task call.
        if t.prompt.trim().is_empty() && !t.agents {
            return Err(bad_request(format!("template {n:?} needs a prompt")));
        }
        if t.worktree && t.cwd.trim().is_empty() {
            return Err(bad_request(format!("template {n:?}: a git worktree needs a folder")));
        }
        crate::sessions::check_labels(&t.labels)?;
        crate::sessions::check_selector(&t.hosts)?;
    }
    Ok(())
}

/// `prompt` with {{input}}, {{target}}, {{date}} and {{time}} filled in.
pub fn fill(prompt: &str, input: &str, target: &str) -> String {
    let now = chrono::Local::now();
    prompt
        .replace("{{input}}", input)
        .replace("{{target}}", target)
        .replace("{{date}}", &now.format("%Y-%m-%d").to_string())
        .replace("{{time}}", &now.format("%H:%M").to_string())
}

/// Starts the template `name`: a session per target. `labels` are added to
/// the template's (e.g. automation:<name>).
/// Starts the template's sessions in `realm`.
pub async fn run(st: &HeadState, name: &str, input: &str, labels: &[String], realm: &str) -> anyhow::Result<api::TemplateRun> {
    run_as(st, name, input, labels, realm, None).await
}

/// Starts a template as an app instance would (its own templates first,
/// its sessions acting as it), or as the realm (`instance: None`), where a
/// template an instance offers still runs as that instance.
pub async fn run_as(
    st: &HeadState,
    name: &str,
    input: &str,
    labels: &[String],
    realm: &str,
    instance: Option<&str>,
) -> anyhow::Result<api::TemplateRun> {
    start(st, name, input, labels, realm, instance, None).await
}

/// Starts a template for a session (`session.template_start`): on the
/// session's worker. One that names other workers, hosts or SSH targets
/// runs there only with your grant (kind `workers`, mode `start`).
pub async fn run_for_session(
    st: &HeadState,
    name: &str,
    input: &str,
    labels: &[String],
    realm: &str,
    session: &crate::store::SessionRow,
) -> anyhow::Result<api::TemplateRun> {
    start(st, name, input, labels, realm, None, Some(session)).await
}

/// Whether a worker runs sessions (not the head or the memex) for `realm`.
fn serves(w: &api::Worker, realm: &str) -> bool {
    !matches!(w.role.as_str(), "head" | "memex") && crate::realms::serves(&w.realms, realm)
}

/// Where a template's sessions run: a worker, with the SSH target or host
/// it works on through it (one session each).
type Target = (api::Worker, Option<String>);

/// The name a target goes by: its SSH target or host, else its worker.
fn target_name((w, ssh): &Target) -> String {
    ssh.clone().unwrap_or_else(|| w.name.clone())
}

/// The template's targets in `realm`, and the hosts that have none online.
/// From a session (`from`): its own worker only, unless the template names
/// others and you allowed that (by ticking its *Agents may start it*, or
/// with a `workers` grant).
async fn targets(
    st: &HeadState,
    name: &str,
    t: &api::Template,
    realm: &str,
    from: Option<&crate::store::SessionRow>,
) -> anyhow::Result<(Vec<Target>, Vec<String>)> {
    // Workers that serve the realm: by name, `*` for every online one, or
    // the online one.
    let snapshot = crate::api::workers_snapshot(st).await?;
    let all: Vec<api::Worker> = snapshot.iter().filter(|w| serves(w, realm)).cloned().collect();
    let online: Vec<&api::Worker> = all.iter().filter(|w| w.status == api::WorkerStatus::Online).collect();
    let mut workers: Vec<&api::Worker> = if t.workers.is_empty() {
        online.first().copied().into_iter().collect()
    } else if t.workers.iter().any(|w| w == "*") {
        online.clone()
    } else {
        let mut v = Vec::new();
        for n in &t.workers {
            match all.iter().find(|w| &w.name == n && w.status != api::WorkerStatus::Revoked) {
                Some(w) => v.push(w),
                None if snapshot.iter().any(|w| &w.name == n) => {
                    return Err(bad_request(format!("template {name:?}: worker {n:?} is not available to realm {realm}")));
                }
                None => return Err(bad_request(format!("template {name:?}: no worker named {n:?}"))),
            }
        }
        v
    };
    // From a session: its own worker, unless you let its realm reach further.
    if let Some(s) = from {
        let own = all
            .iter()
            .find(|w| w.id.to_string() == s.worker_id)
            .ok_or_else(|| bad_request(format!("template {name:?}: {} is not available to realm {realm}", s.worker_name)))?;
        let elsewhere = !t.hosts.is_empty() || !t.ssh.is_empty() || t.workers.iter().any(|w| w != "*" && *w != own.name);
        let by = format!("session:{}", s.id);
        if !elsewhere {
            workers = vec![own];
        } else if !(t.agents && realm == s.realm) && !crate::grants::may_reach(st, realm, &s.realm, name, &by).await {
            return Err(bad_request(format!(
                "template {name:?} runs on other workers, hosts or SSH targets; a session starts it only on its own worker unless you \
                 allow it: ask with grant_request (from_realm {realm}, kind workers, mode start, scope {{\"template\": {name:?}}})"
            )));
        }
    }
    if workers.is_empty() && t.hosts.is_empty() {
        return Err(bad_request(format!("template {name:?}: no worker is online")));
    }
    // One session per worker, or per SSH target on each worker, or per
    // host from the inventory (through its worker, else the template's).
    let mut out: Vec<Target> = Vec::new();
    let mut errors = Vec::new();
    if !t.hosts.is_empty() {
        let inventory = crate::hosts::hosts(st).await?;
        let chosen = crate::hosts::select(&inventory, &t.hosts);
        if chosen.is_empty() {
            return Err(bad_request(format!("template {name:?}: no host has the labels {}", t.hosts.join(", "))));
        }
        for h in chosen {
            let worker = if h.worker.trim().is_empty() { workers.first().copied() } else { crate::hosts::worker_for(h, &all) };
            match worker {
                Some(w) => out.push((w.clone(), Some(h.name.clone()))),
                None => errors.push(format!("{}: its worker is not online", h.name)),
            }
        }
    } else {
        for w in &workers {
            if t.ssh.is_empty() {
                out.push(((*w).clone(), None));
            } else {
                out.extend(t.ssh.iter().map(|h| ((*w).clone(), Some(h.clone()))));
            }
        }
    }
    Ok((out, errors))
}

/// The connection a template names, by name or id (empty: routing
/// rules choose, by its sessions' labels, which include template:<name>).
async fn connection_of(st: &HeadState, name: &str, t: &api::Template) -> anyhow::Result<String> {
    if t.connection.trim().is_empty() {
        return Ok(String::new());
    }
    let connections = st.store.connections().await?;
    Ok(connections
        .iter()
        .find(|c| c.name == t.connection || c.id == t.connection)
        .ok_or_else(|| bad_request(format!("template {name:?}: no connection {:?}", t.connection)))?
        .id
        .clone())
}

/// An 8-hex run id.
fn run_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()[24..].to_owned()
}

async fn start(
    st: &HeadState,
    name: &str,
    input: &str,
    labels: &[String],
    realm: &str,
    instance: Option<&str>,
    from: Option<&crate::store::SessionRow>,
) -> anyhow::Result<api::TemplateRun> {
    let (t, layer) = templates_in(st, realm, instance)
        .await?
        .into_iter()
        .find(|(t, _)| t.name == name)
        .ok_or_else(|| not_found(format!("no template named {name:?}")))?;
    let acting = match (instance, &layer) {
        (Some(me), _) => me.to_owned(),
        (None, crate::lists::Layer::Instance(n)) => n.clone(),
        _ => String::new(),
    };
    if !t.enabled {
        return Err(bad_request(format!("template {name:?} is switched off")));
    }
    let (targets, mut errors) = targets(st, name, &t, realm, from).await?;
    let connection_id = connection_of(st, name, &t).await?;
    let model = t.model.trim().to_owned();

    let run_id = run_id();
    let mut session_labels = t.labels.clone();
    session_labels.extend(labels.iter().cloned());
    session_labels.push(format!("template:{name}"));
    session_labels.push(format!("run:{run_id}"));
    let session_labels = crate::sessions::check_labels(&session_labels)?;

    let fleet = targets.len() > 1 || !t.hosts.is_empty();
    let mut sessions = Vec::new();
    for target in targets {
        let tname = target_name(&target);
        let (w, ssh) = target;
        let title = if !t.title.trim().is_empty() {
            fill(&t.title, input, &tname)
        } else if fleet {
            format!("{name} · {tname}")
        } else {
            name.to_owned()
        };
        let req = api::CreateSession {
            worker_id: w.id,
            cwd: t.cwd.clone(),
            connection_id: connection_id.clone(),
            model: model.clone(),
            title: Some(title),
            prompt: Some(fill(&t.prompt, input, &tname)),
            attachments: Vec::new(),
            labels: session_labels.clone(),
            ssh,
            effort: Some(t.effort.clone()).filter(|e| !e.is_empty()),
            worktree: t.worktree,
            realm: Some(realm.to_owned()),
            work: None,
            work_refs: Vec::new(),
            todo: None,
        };
        match crate::sessions::create_as(st, req, &acting).await {
            Ok(s) => sessions.push(s),
            Err(e) => errors.push(format!("{tname}: {e:#}")),
        }
    }
    if sessions.is_empty() && !errors.is_empty() {
        return Err(bad_request(errors.join("; ")));
    }
    Ok(api::TemplateRun { run_id, sessions, errors })
}

/// A remote subagent to start (docs/remote-subagents.md §5): the parent
/// session's `Task` call on a template.
pub struct SubagentStart<'a> {
    pub template: &'a str,
    /// Which of the template's targets, when it has several.
    pub on: &'a str,
    pub prompt: &'a str,
    pub description: &'a str,
    pub call_id: &'a str,
    /// The parent's depth in its tree (a top-level session is 0).
    pub depth: u32,
}

/// The prompt a template gives a subagent's task: its prompt with
/// {{input}} filled in, else followed by the task (or the task alone).
pub fn subagent_prompt(template: &str, task: &str, target: &str) -> String {
    if template.trim().is_empty() {
        task.to_owned()
    } else if template.contains("{{input}}") {
        fill(template, task, target)
    } else {
        format!("{}\n\n{task}", fill(template, "", target).trim_end())
    }
}

/// Starts a template as a remote subagent of `parent`: one session, in the
/// parent's realm, linked to its call. Returns it and where it runs.
pub async fn start_subagent(
    st: &HeadState,
    parent: &crate::store::SessionRow,
    req: &SubagentStart<'_>,
) -> anyhow::Result<(api::Session, api::RemoteSubagent)> {
    let name = req.template;
    let realm = parent.realm.clone();
    let Some((t, layer)) = templates_in(st, &realm, None).await?.into_iter().find(|(t, _)| t.name == name) else {
        return Err(not_found(format!("no template named {name:?}; {}", agent_list(st, &realm).await)));
    };
    if !t.enabled {
        return Err(bad_request(format!("template {name:?} is switched off")));
    }
    let by = format!("session:{}", parent.id);
    if !t.agents && !crate::grants::may_reach(st, &realm, &realm, name, &by).await {
        return Err(bad_request(format!(
            "template {name:?} is not one agents may start; {} (the user ticks Agents may start it on a template)",
            agent_list(st, &realm).await
        )));
    }
    let acting = match &layer {
        crate::lists::Layer::Instance(n) => n.clone(),
        _ => String::new(),
    };
    // Its tree's limits: how deep, and how many subagents run at once.
    let limits = crate::sessions::subagent_limits(st, Some(&realm)).await?;
    let max_depth = if limits.max_depth == 0 { api::DEFAULT_SUBAGENT_DEPTH } else { limits.max_depth };
    if req.depth >= max_depth {
        return Err(bad_request(format!("subagents nest at most {max_depth} deep; this session may not start more")));
    }
    let root = parent.root_id.clone().unwrap_or_else(|| parent.id.clone());
    let max_running = if limits.max_running == 0 { api::DEFAULT_RUNNING_SUBAGENTS } else { limits.max_running } as i64;
    let running = st.store.running_subagents(&root).await?;
    if running >= max_running {
        return Err(bad_request(format!(
            "{running} subagents are already working in this session; wait for one to finish (or stop one) before starting another"
        )));
    }
    let (targets, errors) = targets(st, name, &t, &realm, Some(parent)).await?;
    let names: Vec<String> = targets.iter().map(target_name).collect();
    let target = match targets.len() {
        0 => return Err(bad_request(format!("template {name:?}: nowhere to run ({})", errors.join("; ")))),
        1 if req.on.is_empty() || names[0] == req.on || targets[0].0.name == req.on => targets.into_iter().next().unwrap(),
        _ if req.on.is_empty() => {
            return Err(bad_request(format!("template {name:?} runs in several places: set on to one of {}", names.join(", "))));
        }
        _ => targets
            .into_iter()
            .find(|t| target_name(t) == req.on || (t.1.is_none() && t.0.name == req.on))
            .ok_or_else(|| bad_request(format!("template {name:?} has no place {:?}; set on to one of {}", req.on, names.join(", "))))?,
    };
    if target.0.status != api::WorkerStatus::Online {
        return Err(bad_request(format!("worker {} is offline; template {name:?} cannot start there now", target.0.name)));
    }
    let tname = target_name(&target);
    let (w, ssh) = target;
    let connection_id = connection_of(st, name, &t).await?;
    let mut labels = t.labels.clone();
    labels.push(format!("template:{name}"));
    labels.push(format!("run:{}", run_id()));
    labels.push(format!("started-by:{}", parent.id));
    let labels = crate::sessions::check_labels(&labels)?;
    let title = if t.title.trim().is_empty() { req.description.to_owned() } else { fill(&t.title, req.prompt, &tname) };
    let create = api::CreateSession {
        worker_id: w.id,
        cwd: t.cwd.clone(),
        connection_id,
        model: t.model.trim().to_owned(),
        title: Some(title),
        prompt: Some(subagent_prompt(&t.prompt, req.prompt, &tname)),
        attachments: Vec::new(),
        labels,
        ssh: ssh.clone(),
        effort: Some(t.effort.clone()).filter(|e| !e.is_empty()),
        worktree: t.worktree,
        realm: Some(realm.clone()),
        work: None,
        work_refs: Vec::new(),
        todo: None,
    };
    let link = crate::sessions::Link {
        parent_id: parent.id.clone(),
        parent_call_id: req.call_id.to_owned(),
        root_id: root,
        depth: req.depth + 1,
        template: name.to_owned(),
        colour: parent.colour.clone(),
    };
    let session = crate::sessions::create_linked(st, create, &acting, Some(link)).await?;
    let remote = api::RemoteSubagent {
        template: name.to_owned(),
        worker_id: w.id.to_string(),
        worker_name: w.name.clone(),
        target: ssh.unwrap_or_default(),
        cwd: t.cwd.clone(),
    };
    Ok((session, remote))
}

/// The templates agents may start in `realm`, for an error.
async fn agent_list(st: &HeadState, realm: &str) -> String {
    let names: Vec<String> = templates_in(st, realm, None)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|(t, _)| t.agents && t.enabled)
        .map(|(t, _)| t.name)
        .collect();
    if names.is_empty() { "agents may start none here".into() } else { format!("agents may start: {}", names.join(", ")) }
}

/// The templates a session in `realm` may start as remote subagents, as
/// `Task` lists them.
pub async fn for_subagents(st: &HeadState, realm: &str, instance: &str) -> Vec<api::SubagentTemplate> {
    let instance = Some(instance).filter(|i| !i.is_empty());
    let Ok(list) = templates_in(st, realm, instance).await else { return Vec::new() };
    let mut out = Vec::new();
    for (t, _) in list.into_iter().filter(|(t, _)| t.agents && t.enabled) {
        let names: Vec<String> = match targets(st, &t.name, &t, realm, None).await {
            Ok((targets, _)) => targets.iter().map(target_name).collect(),
            Err(_) => Vec::new(),
        };
        let where_ = if names.is_empty() {
            if t.workers.is_empty() { "a worker".to_owned() } else { t.workers.join(", ") }
        } else {
            names.join(" | ")
        };
        let mut place = if t.cwd.trim().is_empty() { format!("{where_}: a scratch folder") } else { format!("{where_}:{}", t.cwd.trim()) };
        if t.worktree {
            place.push_str(" (new worktree)");
        }
        let model = match (t.connection.trim(), t.model.trim()) {
            ("", "") => String::new(),
            ("", m) => m.to_owned(),
            (c, "") => c.to_owned(),
            (c, m) => format!("{c} · {m}"),
        };
        out.push(api::SubagentTemplate {
            name: t.name.clone(),
            description: t.description.clone(),
            place,
            model,
            targets: if names.len() > 1 { names } else { Vec::new() },
        });
    }
    out
}
