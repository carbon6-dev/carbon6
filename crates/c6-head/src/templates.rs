//! Session templates: a saved way to start a session (or one per target, a
//! fleet run), started by hand, on a schedule or by a webhook.

use c6_api as api;

use crate::{HeadState, bad_request, not_found};

pub const TEMPLATES: &str = "templates";

pub async fn templates(st: &HeadState) -> anyhow::Result<Vec<api::Template>> {
    let raw = st.store.setting(TEMPLATES).await?.unwrap_or_else(|| "[]".into());
    Ok(serde_json::from_str(&raw).unwrap_or_default())
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
    let t = templates(st).await?.into_iter().find(|t| t.name == name).ok_or_else(|| not_found(format!("no template named {name:?}")))?;
    if !t.enabled {
        return Err(bad_request(format!("template {name:?} is switched off")));
    }
    // Workers: by name, `*` for every online one, or the online one.
    let all = crate::api::workers_snapshot(st).await?;
    let online: Vec<&api::Worker> = all.iter().filter(|w| w.status == api::WorkerStatus::Online).collect();
    let workers: Vec<&api::Worker> = if t.workers.is_empty() {
        online.first().copied().into_iter().collect()
    } else if t.workers.iter().any(|w| w == "*") {
        online.clone()
    } else {
        let mut v = Vec::new();
        for n in &t.workers {
            match all.iter().find(|w| &w.name == n && w.status != api::WorkerStatus::Revoked) {
                Some(w) => v.push(w),
                None => return Err(bad_request(format!("template {name:?}: no worker named {n:?}"))),
            }
        }
        v
    };
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
        match crate::sessions::create(st, req).await {
            Ok(s) => sessions.push(s),
            Err(e) => errors.push(format!("{target}: {e:#}")),
        }
    }
    if sessions.is_empty() && !errors.is_empty() {
        return Err(bad_request(errors.join("; ")));
    }
    Ok(api::TemplateRun { run_id, sessions, errors })
}
