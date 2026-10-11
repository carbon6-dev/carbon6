//! Workflows: steps that run templates, wait for your approval or notify
//! you, in order. A step can use the workflow's input and earlier steps'
//! output, and run only when an earlier step's output says so (or changed
//! since the last run). Runs live in SQLite and survive restarts; one task
//! drives each run.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api as api;
use c6_core::now_ms;
use sqlx::Row;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{HeadState, bad_request, not_found};

pub const WORKFLOWS: &str = "workflows";

/// A run step's sessions may take this long before the step fails.
const STEP_TIMEOUT_MS: i64 = 6 * 3600 * 1000;

/// Run ids to drive (new, resumed after an approval, or after a restart).
pub struct Runner {
    tx: mpsc::UnboundedSender<String>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    /// Runs being driven, so a run has one driver.
    active: Mutex<std::collections::HashSet<String>>,
}

impl Default for Runner {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx: Mutex::new(Some(rx)), active: Mutex::default() }
    }
}

impl Runner {
    pub fn kick(&self, run_id: &str) {
        let _ = self.tx.send(run_id.to_owned());
    }
}

/// Drives runs as they are kicked; resumes unfinished ones at start.
pub fn spawn(st: Arc<HeadState>) {
    let Some(mut rx) = st.workflows.rx.lock().unwrap().take() else { return };
    let resume = st.clone();
    tokio::spawn(async move {
        match sqlx::query_scalar::<_, String>("SELECT id FROM workflow_runs WHERE status = 'running'").fetch_all(&resume.store.pool).await {
            Ok(ids) => ids.iter().for_each(|id| resume.workflows.kick(id)),
            Err(e) => warn!("loading workflow runs: {e:#}"),
        }
    });
    tokio::spawn(async move {
        loop {
            let id = tokio::select! {
                id = rx.recv() => match id { Some(id) => id, None => return },
                _ = st.shutdown.cancelled() => return,
            };
            if !st.workflows.active.lock().unwrap().insert(id.clone()) {
                continue;
            }
            let st = st.clone();
            tokio::spawn(async move {
                if let Err(e) = drive(&st, &id).await {
                    warn!(run = %id, "workflow run: {e:#}");
                    let _ = finish(&st, &id, "failed", Some(format!("{e:#}"))).await;
                }
                st.workflows.active.lock().unwrap().remove(&id);
            });
        }
    });
}

pub async fn workflows(st: &HeadState) -> anyhow::Result<Vec<api::Workflow>> {
    Ok(serde_json::from_str(&st.store.setting(WORKFLOWS).await?.unwrap_or_default()).unwrap_or_default())
}

/// Default step ids (step1, step2, …) where none is given.
fn normalized(mut w: api::Workflow) -> api::Workflow {
    for (i, s) in w.steps.iter_mut().enumerate() {
        if s.id.trim().is_empty() {
            s.id = format!("step{}", i + 1);
        }
        s.id = s.id.trim().to_owned();
    }
    w
}

fn plain_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || "_-.".contains(c))
}

pub async fn check(st: &HeadState, list: &[api::Workflow]) -> anyhow::Result<()> {
    let templates = crate::templates::templates(st).await?;
    let mut names = std::collections::HashSet::new();
    for w in list {
        let n = w.name.trim();
        let what = |m: &str| bad_request(format!("workflow {n:?}: {m}"));
        if !plain_name(n) {
            return Err(bad_request(format!("workflow name {n:?} is not valid (letters, digits, - _ .)")));
        }
        if !names.insert(n) {
            return Err(bad_request(format!("two workflows are named {n:?}")));
        }
        if templates.iter().any(|t| t.name == n) {
            return Err(what("a template has this name too"));
        }
        if w.steps.is_empty() {
            return Err(what("it has no steps"));
        }
        let w = normalized(w.clone());
        let mut seen: Vec<&str> = Vec::new();
        for s in &w.steps {
            if !plain_name(&s.id) || seen.contains(&s.id.as_str()) {
                return Err(what(&format!("step id {:?} is not valid or not unique", s.id)));
            }
            match s.kind.as_str() {
                "run" => {
                    if !templates.iter().any(|t| t.name == s.template) {
                        return Err(what(&format!("step {}: no template named {:?}", s.id, s.template)));
                    }
                }
                "approve" | "notify" => {
                    if s.title.trim().is_empty() && s.message.trim().is_empty() {
                        return Err(what(&format!("step {}: give it a title or message", s.id)));
                    }
                }
                _ => return Err(what(&format!("step {}: the kind is run, approve or notify", s.id))),
            }
            if let Some(c) = &s.when {
                if !seen.contains(&c.step.as_str()) {
                    return Err(what(&format!("step {}: its condition names {:?}, which is not an earlier step", s.id, c.step)));
                }
                if c.contains.is_none() && c.not_contains.is_none() && !c.changed {
                    return Err(what(&format!("step {}: a condition needs contains, not_contains or changed", s.id)));
                }
            }
            seen.push(&s.id);
        }
    }
    Ok(())
}

/// Starts a workflow; its run (driven in the background).
pub async fn start(st: &HeadState, name: &str, input: &str, trigger: &str) -> anyhow::Result<api::WorkflowRun> {
    let w = workflows(st).await?.into_iter().find(|w| w.name == name).ok_or_else(|| not_found(format!("no workflow named {name:?}")))?;
    if !w.enabled {
        return Err(bad_request(format!("workflow {name:?} is switched off")));
    }
    let w = normalized(w);
    let id = uuid::Uuid::now_v7().simple().to_string()[20..].to_owned();
    let steps: Vec<api::WorkflowStepRun> = w
        .steps
        .iter()
        .map(|s| api::WorkflowStepRun { id: s.id.clone(), kind: s.kind.clone(), status: "pending".into(), ..Default::default() })
        .collect();
    sqlx::query(
        "INSERT INTO workflow_runs (id, workflow, input, started_at, trigger, status, steps, definition) VALUES (?, ?, ?, ?, ?, 'running', ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(input)
    .bind(now_ms())
    .bind(trigger)
    .bind(serde_json::to_string(&steps)?)
    .bind(serde_json::to_string(&w)?)
    .execute(&st.store.pool)
    .await?;
    info!(workflow = %name, run = %id, "workflow started");
    st.workflows.kick(&id);
    get(st, &id).await
}

struct Run {
    id: String,
    workflow: api::Workflow,
    input: String,
    status: String,
    steps: Vec<api::WorkflowStepRun>,
}

async fn load(st: &HeadState, id: &str) -> anyhow::Result<Run> {
    let r = sqlx::query("SELECT * FROM workflow_runs WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found("no such workflow run"))?;
    Ok(Run {
        id: r.get("id"),
        workflow: serde_json::from_str(&r.get::<String, _>("definition"))?,
        input: r.get("input"),
        status: r.get("status"),
        steps: serde_json::from_str(&r.get::<String, _>("steps"))?,
    })
}

async fn save_steps(st: &HeadState, id: &str, steps: &[api::WorkflowStepRun], status: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE workflow_runs SET steps = ?, status = ? WHERE id = ?")
        .bind(serde_json::to_string(steps)?)
        .bind(status)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    Ok(())
}

fn labels(w: &api::Workflow, run_id: &str) -> Vec<String> {
    vec![format!("workflow:{}", w.name), format!("wfrun:{run_id}")]
}

/// `text` with {{input}} and {{steps.<id>.output}} filled in.
fn render(text: &str, input: &str, steps: &[api::WorkflowStepRun]) -> String {
    let mut out = text.replace("{{input}}", input);
    for s in steps {
        out = out.replace(&format!("{{{{steps.{}.output}}}}", s.id), &s.output);
    }
    out
}

/// What `changed` compares: the last `RESULT:` line, else the output.
fn result_of(output: &str) -> &str {
    output.lines().rev().find_map(|l| l.trim().strip_prefix("RESULT:").map(str::trim)).unwrap_or_else(|| output.trim())
}

/// The step's output in the workflow's previous finished run.
async fn previous_output(st: &HeadState, workflow: &str, run_id: &str, step: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query(
        "SELECT steps FROM workflow_runs WHERE workflow = ? AND id != ? AND finished_at IS NOT NULL ORDER BY started_at DESC LIMIT 1",
    )
    .bind(workflow)
    .bind(run_id)
    .fetch_optional(&st.store.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let steps: Vec<api::WorkflowStepRun> = serde_json::from_str(&row.get::<String, _>("steps"))?;
    Ok(steps.into_iter().find(|s| s.id == step && s.status == "ok").map(|s| s.output))
}

async fn holds(st: &HeadState, run: &Run, c: &api::StepCondition) -> anyhow::Result<bool> {
    let output = run.steps.iter().find(|s| s.id == c.step).map(|s| s.output.as_str()).unwrap_or("");
    if let Some(x) = &c.contains
        && !output.contains(x.as_str())
    {
        return Ok(false);
    }
    if let Some(x) = &c.not_contains
        && output.contains(x.as_str())
    {
        return Ok(false);
    }
    if c.changed {
        let before = previous_output(st, &run.workflow.name, &run.id, &c.step).await?;
        if before.as_deref().map(result_of) == Some(result_of(output)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The text of a session's last reply.
async fn last_reply(st: &HeadState, session_id: &str) -> String {
    use c6_proto::content_block::Block;
    let Ok(events) = st.store.events_after(session_id, 0, i64::MAX).await else { return String::new() };
    events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            Some(c6_proto::session_event::Kind::Assistant(m)) => {
                let t: String =
                    m.blocks.iter().filter_map(|b| if let Some(Block::Text(t)) = &b.block { Some(t.as_str()) } else { None }).collect();
                Some(t).filter(|t| !t.trim().is_empty())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The key a step's sessions are tracked under (see `run_sessions`).
fn step_key(run_id: &str, step: &str) -> String {
    format!("wf:{run_id}:{step}")
}

async fn drive(st: &HeadState, id: &str) -> anyhow::Result<()> {
    loop {
        let mut run = load(st, id).await?;
        if run.status != "running" {
            return Ok(());
        }
        let Some(i) = run.steps.iter().position(|s| matches!(s.status.as_str(), "pending" | "running" | "waiting")) else {
            let failed = run.steps.iter().any(|s| s.status == "failed");
            return finish(st, id, if failed { "failed" } else { "ok" }, None).await;
        };
        let def = run.workflow.steps[i].clone();
        match run.steps[i].status.as_str() {
            "pending" => {
                if let Some(c) = &def.when
                    && !holds(st, &run, c).await?
                {
                    run.steps[i].status = "skipped".into();
                    save_steps(st, id, &run.steps, "running").await?;
                    continue;
                }
                match def.kind.as_str() {
                    "run" => {
                        let input = render(&def.input, &run.input, &run.steps);
                        match crate::templates::run(st, &def.template, &input, &labels(&run.workflow, id)).await {
                            Ok(r) => {
                                let key = step_key(id, &def.id);
                                for s in &r.sessions {
                                    sqlx::query("INSERT OR IGNORE INTO run_sessions (session_id, run_id) VALUES (?, ?)")
                                        .bind(&s.id)
                                        .bind(&key)
                                        .execute(&st.store.pool)
                                        .await?;
                                }
                                let step = &mut run.steps[i];
                                step.sessions = r.sessions.iter().map(|s| s.id.clone()).collect();
                                step.error = (!r.errors.is_empty()).then(|| r.errors.join("; "));
                                step.status = "running".into();
                                step.started_at = Some(now_ms());
                            }
                            Err(e) => {
                                run.steps[i].status = "failed".into();
                                run.steps[i].error = Some(format!("{e:#}"));
                                if !def.continue_on_failure {
                                    save_steps(st, id, &run.steps, "running").await?;
                                    return finish(st, id, "failed", Some(format!("step {}: {e:#}", def.id))).await;
                                }
                            }
                        }
                        save_steps(st, id, &run.steps, "running").await?;
                    }
                    "notify" => {
                        let title = render(&def.title, &run.input, &run.steps);
                        let message = render(&def.message, &run.input, &run.steps);
                        st.notifier.send(crate::notify::Note {
                            event: "notice",
                            title: if title.is_empty() { format!("Workflow {}", run.workflow.name) } else { title.clone() },
                            message: message.clone(),
                            priority: "normal",
                            labels: labels(&run.workflow, id),
                            ..Default::default()
                        });
                        run.steps[i].status = "ok".into();
                        run.steps[i].output = if message.is_empty() { title } else { message };
                        run.steps[i].started_at = Some(now_ms());
                        run.steps[i].finished_at = Some(now_ms());
                        save_steps(st, id, &run.steps, "running").await?;
                    }
                    _ => {
                        // approve: wait for your answer (POST …/decide, or the link).
                        let title = render(&def.title, &run.input, &run.steps);
                        let message = render(&def.message, &run.input, &run.steps);
                        run.steps[i].status = "waiting".into();
                        run.steps[i].output = message.clone();
                        run.steps[i].started_at = Some(now_ms());
                        save_steps(st, id, &run.steps, "waiting").await?;
                        st.notifier.send(crate::notify::Note {
                            event: "approval",
                            title: format!("Approval needed: {}", if title.is_empty() { &run.workflow.name } else { &title }),
                            message: format!("Workflow {} waits for your go-ahead.\n{message}", run.workflow.name),
                            priority: "high",
                            session_id: Some(format!("wf:{id}")),
                            call_id: Some(def.id.clone()),
                            labels: labels(&run.workflow, id),
                        });
                        return Ok(());
                    }
                }
            }
            "running" => {
                // A run step: wait for its sessions' first turns.
                let key = step_key(id, &def.id);
                let started = run.steps[i].started_at.unwrap_or_else(now_ms);
                let outcomes: Vec<Option<String>> = loop {
                    let o: Vec<Option<String>> = sqlx::query_scalar("SELECT outcome FROM run_sessions WHERE run_id = ?")
                        .bind(&key)
                        .fetch_all(&st.store.pool)
                        .await?;
                    if o.iter().all(Option::is_some) || now_ms() - started > STEP_TIMEOUT_MS || st.shutdown.is_cancelled() {
                        break o;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                };
                if st.shutdown.is_cancelled() {
                    return Ok(());
                }
                let mut parts = Vec::new();
                let many = run.steps[i].sessions.len() > 1;
                for sid in run.steps[i].sessions.clone() {
                    let reply = last_reply(st, &sid).await;
                    if many {
                        let title = st.store.session(&sid).await?.map(|s| s.title).unwrap_or_default();
                        parts.push(format!("## {title}\n{reply}"));
                    } else {
                        parts.push(reply);
                    }
                }
                let ok = !outcomes.is_empty() && outcomes.iter().all(|o| o.as_deref() == Some("ok")) && run.steps[i].error.is_none();
                let step = &mut run.steps[i];
                step.output = parts.join("\n\n");
                step.status = if ok { "ok" } else { "failed" }.into();
                step.finished_at = Some(now_ms());
                if outcomes.iter().any(Option::is_none) {
                    step.error = Some("its sessions did not finish in 6 hours".into());
                }
                save_steps(st, id, &run.steps, "running").await?;
                if !ok && !def.continue_on_failure {
                    return finish(st, id, "failed", Some(format!("step {} failed", def.id))).await;
                }
            }
            _ => return Ok(()), // waiting
        }
    }
}

/// Ends a run: its status, a notification if it failed, and the
/// automation run it belongs to, if any.
async fn finish(st: &HeadState, id: &str, status: &str, error: Option<String>) -> anyhow::Result<()> {
    let done = sqlx::query("UPDATE workflow_runs SET status = ?, error = ?, finished_at = ? WHERE id = ? AND finished_at IS NULL")
        .bind(status)
        .bind(&error)
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Ok(());
    }
    let run = load(st, id).await?;
    info!(workflow = %run.workflow.name, run = %id, status, "workflow finished");
    if status == "failed" {
        st.notifier.send(crate::notify::Note {
            event: "automation",
            title: format!("Workflow {} failed", run.workflow.name),
            message: error.unwrap_or_default(),
            priority: "high",
            labels: labels(&run.workflow, id),
            ..Default::default()
        });
    }
    crate::automations::workflow_finished(st, id, if status == "denied" { "stopped" } else { status }).await;
    Ok(())
}

/// Your answer to an approve step. `false`: nothing waits there.
pub async fn decide(st: &HeadState, run_id: &str, step: &str, allow: bool) -> anyhow::Result<bool> {
    let mut run = load(st, run_id).await?;
    if run.status != "waiting" {
        return Ok(false);
    }
    let Some(s) = run.steps.iter_mut().find(|s| s.id == step && s.status == "waiting") else { return Ok(false) };
    s.status = if allow { "ok" } else { "denied" }.into();
    s.finished_at = Some(now_ms());
    if allow {
        save_steps(st, run_id, &run.steps, "running").await?;
        st.workflows.kick(run_id);
    } else {
        save_steps(st, run_id, &run.steps, "denied").await?;
        finish(st, run_id, "denied", Some(format!("you said no at {step}"))).await?;
    }
    Ok(true)
}

/// What an approve step asks, if it still waits: (workflow, title, message).
pub async fn waiting(st: &HeadState, run_id: &str, step: &str) -> Option<(String, String, String)> {
    let run = load(st, run_id).await.ok()?;
    let s = run.steps.iter().find(|s| s.id == step && s.status == "waiting")?;
    let def = run.workflow.steps.iter().find(|d| d.id == step)?;
    Some((run.workflow.name.clone(), render(&def.title, &run.input, &run.steps), s.output.clone()))
}

fn row_to_api(r: &sqlx::sqlite::SqliteRow) -> api::WorkflowRun {
    api::WorkflowRun {
        id: r.get("id"),
        workflow: r.get("workflow"),
        input: r.get("input"),
        started_at: r.get("started_at"),
        finished_at: r.get("finished_at"),
        trigger: r.get("trigger"),
        status: r.get("status"),
        error: r.get("error"),
        steps: serde_json::from_str(&r.get::<String, _>("steps")).unwrap_or_default(),
    }
}

pub async fn get(st: &HeadState, id: &str) -> anyhow::Result<api::WorkflowRun> {
    let r = sqlx::query("SELECT * FROM workflow_runs WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found("no such workflow run"))?;
    Ok(row_to_api(&r))
}

/// Latest runs, newest first (of one workflow, or all).
pub async fn runs(st: &HeadState, workflow: Option<&str>, limit: u32) -> anyhow::Result<Vec<api::WorkflowRun>> {
    let rows = sqlx::query("SELECT * FROM workflow_runs WHERE (? IS NULL OR workflow = ?) ORDER BY started_at DESC LIMIT ?")
        .bind(workflow)
        .bind(workflow)
        .bind(limit.clamp(1, 200) as i64)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(rows.iter().map(row_to_api).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_and_results() {
        let steps = vec![api::WorkflowStepRun { id: "check".into(), output: "All fine.\nRESULT: ok".into(), ..Default::default() }];
        assert_eq!(render("In: {{input}} / {{steps.check.output}}", "x", &steps), "In: x / All fine.\nRESULT: ok");
        assert_eq!(result_of("Disk at 91%.\nRESULT: web1 disk 91%\n"), "web1 disk 91%");
        assert_eq!(result_of(" no marker "), "no marker");
    }
}
