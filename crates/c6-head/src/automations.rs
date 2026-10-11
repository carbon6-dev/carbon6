//! Automations: start a template on a schedule (cron, in the head's local
//! time) or when its webhook is called. A run's sessions are labelled
//! automation:<name> (and run:<id>), so they show up together and in the
//! inbox when they finish.

use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use c6_core::now_ms;
use chrono::Local;
#[cfg(test)]
use chrono::TimeZone;
use sqlx::Row;
use tracing::{info, warn};

use crate::cron::Schedule;
use crate::{HeadState, bad_request, not_found};

/// Webhook bodies beyond this are refused.
pub const MAX_WEBHOOK_BODY: usize = 1024 * 1024;

fn row_to_api(r: &sqlx::sqlite::SqliteRow) -> api::Automation {
    let trigger: String = r.get("trigger");
    let cron: String = r.get("cron");
    let enabled = r.get::<i64, _>("enabled") != 0;
    let next_run_at = (trigger == "schedule" && enabled)
        .then(|| Schedule::parse(&cron).ok()?.next_after(Local::now()).map(|t| t.timestamp_millis()))
        .flatten();
    api::Automation {
        id: r.get("id"),
        name: r.get("name"),
        template: r.get("template"),
        trigger,
        cron,
        input: r.get("input"),
        enabled,
        created_at: r.get("created_at"),
        last_run_at: r.get("last_run_at"),
        last_run_id: r.get("last_run_id"),
        last_error: r.get("last_error"),
        next_run_at,
        webhook_url: None,
        disable_after: r.get::<i64, _>("disable_after").max(0) as u32,
        recent: Vec::new(),
    }
}

/// With how its latest runs went.
async fn with_recent(st: &HeadState, mut a: api::Automation) -> anyhow::Result<api::Automation> {
    a.recent = sqlx::query_scalar("SELECT outcome FROM automation_runs WHERE automation_id = ? ORDER BY started_at DESC LIMIT 10")
        .bind(&a.id)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(a)
}

pub async fn list(st: &HeadState) -> anyhow::Result<Vec<api::Automation>> {
    let rows = sqlx::query("SELECT * FROM automations ORDER BY name").fetch_all(&st.store.pool).await?;
    let mut out = Vec::new();
    for r in &rows {
        out.push(with_recent(st, row_to_api(r)).await?);
    }
    Ok(out)
}

async fn get(st: &HeadState, id: &str) -> anyhow::Result<api::Automation> {
    let row = sqlx::query("SELECT * FROM automations WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?;
    with_recent(st, row.as_ref().map(row_to_api).ok_or_else(|| not_found("no such automation"))?).await
}

fn new_token() -> String {
    hex::encode(rand::random::<[u8; 24]>())
}

fn webhook_path(id: &str, token: &str) -> String {
    format!("/api/v1/webhooks/{id}/{token}")
}

async fn check(st: &HeadState, name: &str, template: &str, trigger: &str, cron: &str) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        return Err(bad_request("an automation needs a name"));
    }
    if !crate::templates::templates(st).await?.iter().any(|t| t.name == template) {
        return Err(bad_request(format!("no template named {template:?}")));
    }
    match trigger {
        "schedule" => Schedule::parse(cron).map(drop).map_err(bad_request),
        "webhook" => Ok(()),
        _ => Err(bad_request("the trigger is schedule or webhook")),
    }
}

/// Creates an automation; a webhook's response carries its URL (once).
pub async fn create(st: &HeadState, e: api::AutomationEdit) -> anyhow::Result<api::Automation> {
    let name = e.name.unwrap_or_default().trim().to_owned();
    let template = e.template.unwrap_or_default();
    let trigger = e.trigger.unwrap_or_else(|| "schedule".into());
    let cron = e.cron.unwrap_or_default().trim().to_owned();
    check(st, &name, &template, &trigger, &cron).await?;
    let id = uuid::Uuid::now_v7().simple().to_string();
    let token = (trigger == "webhook").then(new_token);
    sqlx::query(
        "INSERT INTO automations (id, name, template, trigger, cron, input, token_hash, enabled, created_at, disable_after) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&template)
    .bind(&trigger)
    .bind(&cron)
    .bind(e.input.unwrap_or_default())
    .bind(token.as_deref().map(|t| c6_core::sha256_hex(t.as_bytes())).unwrap_or_default())
    .bind(e.enabled.unwrap_or(true) as i64)
    .bind(now_ms())
    .bind(e.disable_after.unwrap_or(0) as i64)
    .execute(&st.store.pool)
    .await?;
    let mut a = get(st, &id).await?;
    a.webhook_url = token.map(|t| webhook_path(&id, &t));
    Ok(a)
}

pub async fn update(st: &HeadState, id: &str, e: api::AutomationEdit) -> anyhow::Result<api::Automation> {
    let cur = get(st, id).await?;
    let name = e.name.unwrap_or(cur.name).trim().to_owned();
    let template = e.template.unwrap_or(cur.template);
    let cron = e.cron.unwrap_or(cur.cron).trim().to_owned();
    check(st, &name, &template, &cur.trigger, &cron).await?;
    sqlx::query("UPDATE automations SET name = ?, template = ?, cron = ?, input = ?, enabled = ?, disable_after = ? WHERE id = ?")
        .bind(&name)
        .bind(&template)
        .bind(&cron)
        .bind(e.input.unwrap_or(cur.input))
        .bind(e.enabled.unwrap_or(cur.enabled) as i64)
        .bind(e.disable_after.unwrap_or(cur.disable_after) as i64)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    get(st, id).await
}

/// A new webhook token (the old URL stops working); the response has the URL.
pub async fn rotate(st: &HeadState, id: &str) -> anyhow::Result<api::Automation> {
    let mut a = get(st, id).await?;
    if a.trigger != "webhook" {
        return Err(bad_request("only webhooks have a URL"));
    }
    let token = new_token();
    sqlx::query("UPDATE automations SET token_hash = ? WHERE id = ?")
        .bind(c6_core::sha256_hex(token.as_bytes()))
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    a.webhook_url = Some(webhook_path(id, &token));
    Ok(a)
}

pub async fn delete(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let mut tx = st.store.pool.begin().await?;
    let done = sqlx::query("DELETE FROM automations WHERE id = ?").bind(id).execute(&mut *tx).await?;
    if done.rows_affected() == 0 {
        return Err(not_found("no such automation"));
    }
    sqlx::query("DELETE FROM run_sessions WHERE run_id IN (SELECT run_id FROM automation_runs WHERE automation_id = ?)")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM automation_runs WHERE automation_id = ?").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

fn automation_labels(a: &api::Automation) -> Vec<String> {
    vec![format!("automation:{}", label_safe(&a.name)), format!("template:{}", a.template)]
}

/// Runs an automation's template now, recording the run. `trigger`:
/// "schedule", "webhook" or "manual".
pub async fn fire(st: &HeadState, a: &api::Automation, input: &str, trigger: &str) -> anyhow::Result<api::TemplateRun> {
    let result = crate::templates::run(st, &a.template, input, &[format!("automation:{}", label_safe(&a.name))]).await;
    let (run_id, error) = match &result {
        Ok(r) => (Some(r.run_id.clone()), (!r.errors.is_empty()).then(|| r.errors.join("; "))),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    sqlx::query("UPDATE automations SET last_run_at = ?, last_run_id = COALESCE(?, last_run_id), last_error = ? WHERE id = ?")
        .bind(now_ms())
        .bind(&run_id)
        .bind(&error)
        .bind(&a.id)
        .execute(&st.store.pool)
        .await?;
    match &result {
        Ok(r) => info!(automation = %a.name, run = %r.run_id, sessions = r.sessions.len(), "automation ran"),
        Err(e) => warn!(automation = %a.name, "automation failed: {e:#}"),
    }
    if let Some(error) = &error {
        st.notifier.send(crate::notify::Note {
            event: "automation",
            title: format!("Automation {} failed", a.name),
            message: error.clone(),
            priority: "high",
            labels: automation_labels(a),
            ..Default::default()
        });
    }
    // The run, and its sessions (whose first turns decide how it went).
    let started = now_ms();
    let mut tx = st.store.pool.begin().await?;
    let (run_id, running) = match &result {
        Ok(r) => (r.run_id.clone(), !r.sessions.is_empty()),
        Err(_) => (uuid::Uuid::now_v7().simple().to_string()[24..].to_owned(), false),
    };
    sqlx::query(
        "INSERT INTO automation_runs (run_id, automation_id, started_at, finished_at, trigger, outcome, error) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&run_id)
    .bind(&a.id)
    .bind(started)
    .bind((!running).then_some(started))
    .bind(trigger)
    .bind(if running { "running" } else { "failed" })
    .bind(&error)
    .execute(&mut *tx)
    .await?;
    if let Ok(r) = &result {
        for s in &r.sessions {
            sqlx::query("INSERT OR IGNORE INTO run_sessions (session_id, run_id) VALUES (?, ?)")
                .bind(&s.id)
                .bind(&run_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    if !running {
        after_failure(st, &a.id).await;
    }
    result
}

/// A run's session finished its first turn (or failed to start):
/// `outcome` is "ok", "failed" or "stopped". When it was the run's last,
/// the run is done.
pub async fn session_ended(st: &HeadState, session_id: &str, outcome: &str) {
    if let Err(e) = end_session(st, session_id, outcome).await {
        warn!(session = %session_id, "recording an automation run: {e:#}");
    }
}

async fn end_session(st: &HeadState, session_id: &str, outcome: &str) -> anyhow::Result<()> {
    let Some(run_id) =
        sqlx::query_scalar::<_, String>("UPDATE run_sessions SET outcome = ? WHERE session_id = ? AND outcome IS NULL RETURNING run_id")
            .bind(outcome)
            .bind(session_id)
            .fetch_optional(&st.store.pool)
            .await?
    else {
        return Ok(());
    };
    let outcomes: Vec<Option<String>> =
        sqlx::query_scalar("SELECT outcome FROM run_sessions WHERE run_id = ?").bind(&run_id).fetch_all(&st.store.pool).await?;
    if outcomes.iter().any(Option::is_none) {
        return Ok(());
    }
    let row = sqlx::query("SELECT automation_id, error FROM automation_runs WHERE run_id = ? AND outcome = 'running'")
        .bind(&run_id)
        .fetch_optional(&st.store.pool)
        .await?;
    let Some(row) = row else { return Ok(()) };
    let automation_id: String = row.get("automation_id");
    // Targets that did not start count as failures too.
    let partly: bool = row.get::<Option<String>, _>("error").is_some();
    let has = |o: &str| outcomes.iter().any(|x| x.as_deref() == Some(o));
    let run_outcome = if has("failed") || partly {
        "failed"
    } else if has("stopped") {
        "stopped"
    } else {
        "ok"
    };
    sqlx::query("UPDATE automation_runs SET outcome = ?, finished_at = ? WHERE run_id = ?")
        .bind(run_outcome)
        .bind(now_ms())
        .bind(&run_id)
        .execute(&st.store.pool)
        .await?;
    if run_outcome == "failed" {
        if let Ok(a) = get(st, &automation_id).await {
            let failed = outcomes.iter().filter(|o| o.as_deref() == Some("failed")).count();
            st.notifier.send(crate::notify::Note {
                event: "automation",
                title: format!("Automation {} failed", a.name),
                message: format!("{failed} of {} session(s) in run {run_id} failed.", outcomes.len()),
                priority: "high",
                labels: automation_labels(&a),
                ..Default::default()
            });
        }
        after_failure(st, &automation_id).await;
    }
    Ok(())
}

/// After a failed run: switches the automation off once its last
/// `disable_after` runs all failed.
async fn after_failure(st: &HeadState, automation_id: &str) {
    let Ok(a) = get(st, automation_id).await else { return };
    let n = a.disable_after as usize;
    if n == 0 || !a.enabled {
        return;
    }
    let finished: Vec<String> = match sqlx::query_scalar(
        "SELECT outcome FROM automation_runs WHERE automation_id = ? AND outcome != 'running' ORDER BY started_at DESC LIMIT ?",
    )
    .bind(automation_id)
    .bind(n as i64)
    .fetch_all(&st.store.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!("loading automation runs: {e:#}");
            return;
        }
    };
    if finished.len() < n || finished.iter().any(|o| o != "failed") {
        return;
    }
    if let Err(e) = sqlx::query("UPDATE automations SET enabled = 0 WHERE id = ?").bind(automation_id).execute(&st.store.pool).await {
        warn!("switching an automation off: {e:#}");
        return;
    }
    warn!(automation = %a.name, runs = n, "automation switched off after failed runs");
    st.notifier.send(crate::notify::Note {
        event: "automation",
        title: format!("Automation {} switched off", a.name),
        message: format!("Its last {n} runs failed. Switch it back on in Settings → Automations."),
        priority: "high",
        labels: automation_labels(&a),
        ..Default::default()
    });
}

/// An automation's latest runs, newest first.
pub async fn runs(st: &HeadState, id: &str, limit: u32) -> anyhow::Result<Vec<api::AutomationRun>> {
    get(st, id).await?;
    let rows = sqlx::query("SELECT * FROM automation_runs WHERE automation_id = ? ORDER BY started_at DESC LIMIT ?")
        .bind(id)
        .bind(limit.clamp(1, 200) as i64)
        .fetch_all(&st.store.pool)
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let run_id: String = r.get("run_id");
        let members = sqlx::query(
            "SELECT rs.session_id, rs.outcome, COALESCE(s.title, '') AS title FROM run_sessions rs \
             LEFT JOIN sessions s ON s.id = rs.session_id WHERE rs.run_id = ? ORDER BY s.created_at",
        )
        .bind(&run_id)
        .fetch_all(&st.store.pool)
        .await?;
        let sessions: Vec<api::RunSession> = members
            .iter()
            .map(|m| api::RunSession {
                id: m.get("session_id"),
                title: m.get("title"),
                outcome: m.get::<Option<String>, _>("outcome").unwrap_or_default(),
            })
            .collect();
        let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
        let (cost, calls) = crate::usage::cost_of(st, &ids).await?;
        out.push(api::AutomationRun {
            run_id,
            started_at: r.get("started_at"),
            finished_at: r.get("finished_at"),
            trigger: r.get("trigger"),
            outcome: r.get("outcome"),
            error: r.get("error"),
            sessions,
            cost,
            calls,
        });
    }
    Ok(out)
}

/// Runs an automation by id (by hand).
pub async fn run_now(st: &HeadState, id: &str) -> anyhow::Result<api::TemplateRun> {
    let a = get(st, id).await?;
    fire(st, &a, &a.input, "manual").await
}

/// A webhook call: checks the token and runs the template with the body as
/// its input. `None`: no such webhook (or a wrong token).
pub async fn webhook(st: &HeadState, id: &str, token: &str, body: &[u8]) -> anyhow::Result<Option<api::TemplateRun>> {
    let row = sqlx::query("SELECT token_hash FROM automations WHERE id = ? AND trigger = 'webhook' AND enabled = 1")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let want: String = row.get("token_hash");
    let got = c6_core::sha256_hex(token.as_bytes());
    // Compare in constant time.
    if want.len() != got.len() || want.bytes().zip(got.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return Ok(None);
    }
    let a = get(st, id).await?;
    let text = String::from_utf8_lossy(body);
    // JSON is shown pretty, for the model.
    let input = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| text.into_owned());
    fire(st, &a, &input, "webhook").await.map(Some)
}

/// A name usable in a label.
fn label_safe(name: &str) -> String {
    name.trim().chars().map(|c| if c.is_whitespace() || c == ',' { '-' } else { c }).collect()
}

/// Runs scheduled automations: every 30 seconds, each enabled schedule
/// whose time came since the last check runs once. Times missed while the
/// head was down are not made up.
pub fn spawn_scheduler(st: Arc<HeadState>) {
    tokio::spawn(async move {
        let mut last = Local::now();
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            let now = Local::now();
            let due: Vec<api::Automation> = match list(&st).await {
                Ok(all) => all.into_iter().filter(|a| a.enabled && a.trigger == "schedule").filter(|a| due(&a.cron, last, now)).collect(),
                Err(e) => {
                    warn!("loading automations: {e:#}");
                    Vec::new()
                }
            };
            last = now;
            for a in due {
                let st = st.clone();
                tokio::spawn(async move {
                    let _ = fire(&st, &a, &a.input, "schedule").await;
                });
            }
        }
    });
}

/// Whether a schedule had a time in (`last`, `now`].
fn due(cron: &str, last: chrono::DateTime<Local>, now: chrono::DateTime<Local>) -> bool {
    Schedule::parse(cron).ok().and_then(|s| s.next_after(last)).is_some_and(|t| t <= now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_once_per_scheduled_minute() {
        let t = |h, m, s| Local.with_ymd_and_hms(2026, 10, 12, h, m, s).unwrap();
        // Checks every 30 s around 08:00 on a Monday.
        assert!(!due("0 8 * * mon-fri", t(7, 59, 0), t(7, 59, 30)));
        assert!(due("0 8 * * mon-fri", t(7, 59, 30), t(8, 0, 0)));
        assert!(!due("0 8 * * mon-fri", t(8, 0, 0), t(8, 0, 30)), "not twice");
        assert!(!due("0 8 * * sat", t(7, 59, 30), t(8, 0, 10)));
        assert!(!due("not a schedule", t(7, 0, 0), t(9, 0, 0)));
    }
}
