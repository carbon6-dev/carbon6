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
        if t.prompt.trim().is_empty() {
            return Err(bad_request(format!("template {n:?} needs a prompt")));
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
        } else if !crate::grants::may_reach(st, realm, &s.realm, name, &by).await {
            return Err(bad_request(format!(
                "template {name:?} runs on other workers, hosts or SSH targets; a session starts it only on its own worker unless you \
                 allow it: ask with grant_request (from_realm {realm}, kind workers, mode start, scope {{\"template\": {name:?}}})"
            )));
        }
    }
    if workers.is_empty() && t.hosts.is_empty() {
        return Err(bad_request(format!("template {name:?}: no worker is online")));
    }
    // Without a connection or model, routing rules choose (by the labels
    // below, which include template:<name>).
    let connection_id = if t.connection.trim().is_empty() {
        String::new()
    } else {
        let connections = st.store.connections().await?;
        connections
            .iter()
            .find(|c| c.name == t.connection || c.id == t.connection)
            .ok_or_else(|| bad_request(format!("template {name:?}: no connection {:?}", t.connection)))?
            .id
            .clone()
    };
    let model = t.model.trim().to_owned();

    let run_id = uuid::Uuid::now_v7().simple().to_string()[24..].to_owned();
    let mut session_labels = t.labels.clone();
    session_labels.extend(labels.iter().cloned());
    session_labels.push(format!("template:{name}"));
    session_labels.push(format!("run:{run_id}"));
    let session_labels = crate::sessions::check_labels(&session_labels)?;

    // One session per worker, or per SSH target on each worker, or per
    // host from the inventory (through its worker, else the template's).
    let mut targets: Vec<(&api::Worker, Option<String>)> = Vec::new();
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
                Some(w) => targets.push((w, Some(h.name.clone()))),
                None => errors.push(format!("{}: its worker is not online", h.name)),
            }
        }
    } else {
        for w in &workers {
            if t.ssh.is_empty() {
                targets.push((w, None));
            } else {
                targets.extend(t.ssh.iter().map(|h| (*w, Some(h.clone()))));
            }
        }
    }
    let fleet = targets.len() > 1 || !t.hosts.is_empty();
    let mut sessions = Vec::new();
    for (w, ssh) in targets {
        let target = ssh.clone().unwrap_or_else(|| w.name.clone());
        let req = api::CreateSession {
            worker_id: w.id,
            cwd: t.cwd.clone(),
            connection_id: connection_id.clone(),
            model: model.clone(),
            title: Some(if fleet { format!("{name} · {target}") } else { name.to_owned() }),
            prompt: Some(fill(&t.prompt, input, &target)),
            attachments: Vec::new(),
            labels: session_labels.clone(),
            ssh,
            effort: Some(t.effort.clone()).filter(|e| !e.is_empty()),
            worktree: false,
            realm: Some(realm.to_owned()),
        };
        match crate::sessions::create_as(st, req, &acting).await {
            Ok(s) => sessions.push(s),
            Err(e) => errors.push(format!("{target}: {e:#}")),
        }
    }
    if sessions.is_empty() && !errors.is_empty() {
        return Err(bad_request(errors.join("; ")));
    }
    Ok(api::TemplateRun { run_id, sessions, errors })
}
