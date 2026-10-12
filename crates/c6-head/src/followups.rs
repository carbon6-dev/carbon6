//! Follow-through (docs/follow-through.md): project kits react to what
//! sessions do (a push), offer next steps (open a PR) and start trackers
//! that follow what was started outside carbon6 (a CI run) until it ends,
//! then notify and, on failure, open an investigation.
//!
//! Nothing here knows any CI or forge: kits say what to match, what to run
//! (on the session's worker, a named worker, or the head) and how to read
//! it. Reactions are matched here, as events arrive, so they always use the
//! current kits.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use c6_api as api;
use c6_core::{WorkerId, now_ms};
use c6_proto::{SessionEvent, content_block, session_event};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::mpsc;
use tracing::warn;

use crate::store::SessionRow;
use crate::{HeadState, bad_request};

pub const KITS: &str = "kits";
const TERMINAL: [&str; 5] = ["succeeded", "failed", "cancelled", "stalled", "stopped"];
const PHASES: [&str; 6] = ["waiting", "running", "succeeded", "failed", "cancelled", "stalled"];

/// A tool call that finished, to match reactions against.
#[derive(Debug)]
struct Job {
    session_id: String,
    seq: u64,
    tool: String,
    input: Option<String>,
    output: String,
    is_error: bool,
}

#[derive(Default)]
pub struct Followups {
    tx: OnceLock<mpsc::UnboundedSender<Job>>,
    /// Tool calls' inputs by call id, until their results arrive.
    calls: Mutex<HashMap<String, String>>,
    /// Trackers being probed right now.
    busy: Mutex<std::collections::HashSet<i64>>,
}

/// Starts the reaction worker and the tracker scheduler.
pub fn spawn(st: Arc<HeadState>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
    let _ = st.followups.tx.set(tx);
    let s = st.clone();
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let s = s.clone();
            tokio::spawn(async move {
                if let Err(e) = react(&s, job).await {
                    warn!("reaction: {e:#}");
                }
            });
        }
    });
    tokio::spawn(async move {
        loop {
            if let Err(e) = poll_due(&st).await {
                warn!("trackers: {e:#}");
            }
            tokio::select! {
                _ = st.shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    });
}

/// New events from a session: finished tool calls go to the reactions.
pub fn on_events(st: &HeadState, session_id: &str, events: &[SessionEvent]) {
    let Some(tx) = st.followups.tx.get() else { return };
    for ev in events {
        match &ev.kind {
            Some(session_event::Kind::Assistant(m)) => {
                let mut calls = st.followups.calls.lock().unwrap();
                for b in &m.blocks {
                    if let Some(content_block::Block::ToolUse(t)) = &b.block {
                        calls.insert(t.id.clone(), t.input_json.clone());
                    }
                }
                // Bounded: inputs whose results never came.
                if calls.len() > 10_000 {
                    calls.clear();
                }
            }
            Some(session_event::Kind::ToolResult(r)) => {
                let input = st.followups.calls.lock().unwrap().remove(&r.call_id);
                let _ = tx.send(Job {
                    session_id: session_id.to_owned(),
                    seq: ev.seq,
                    tool: r.tool.clone(),
                    input,
                    output: r.output.clone(),
                    is_error: r.is_error,
                });
            }
            _ => {}
        }
    }
}

pub async fn kits(st: &HeadState) -> anyhow::Result<Vec<api::Kit>> {
    Ok(serde_json::from_str(&st.store.setting(KITS).await?.unwrap_or_default()).unwrap_or_default())
}

pub fn check_kits(kits: &[api::Kit]) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for k in kits {
        let k_name = k.name.trim();
        if k_name.is_empty() || !names.insert(k_name.to_owned()) {
            return Err(bad_request(format!("kit names must be unique and not empty ({k_name:?})")));
        }
        if !matches!(k.state.as_str(), "proposed" | "active" | "broken" | "disabled") {
            return Err(bad_request(format!("kit {k_name}: state is proposed, active, broken or disabled")));
        }
        crate::sessions::check_labels(&k.applies_to)?;
        let regex = |what: &str, r: &str| -> anyhow::Result<()> {
            if !r.is_empty() {
                regex::Regex::new(r).map_err(|e| bad_request(format!("kit {k_name}: {what}: {e}")))?;
            }
            Ok(())
        };
        let probe = |what: &str, p: &api::Probe| -> anyhow::Result<()> {
            if p.command.trim().is_empty() {
                return Err(bad_request(format!("kit {k_name}: {what} needs a command")));
            }
            match p.on.as_str() {
                "" | "session" | "head" => Ok(()),
                "worker" if !p.worker.trim().is_empty() => Ok(()),
                "worker" => Err(bad_request(format!("kit {k_name}: {what} runs on a worker: name it"))),
                other => Err(bad_request(format!("kit {k_name}: {what}: on is session, worker or head, not {other}"))),
            }
        };
        let trackers: Vec<&str> = k.trackers.iter().map(|t| t.name.as_str()).collect();
        let actions = |what: &str, acts: &[api::Action]| -> anyhow::Result<()> {
            for a in acts {
                match a.kind.as_str() {
                    "suggest" | "notify" | "tell" if !a.text.trim().is_empty() => {}
                    "suggest" | "notify" | "tell" => return Err(bad_request(format!("kit {k_name}: {what}: a {} needs text", a.kind))),
                    "track" if trackers.contains(&a.tracker.as_str()) => {}
                    "track" => return Err(bad_request(format!("kit {k_name}: {what}: no tracker {:?} in this kit", a.tracker))),
                    other => {
                        return Err(bad_request(format!("kit {k_name}: {what}: actions are suggest, track, notify or tell, not {other}")));
                    }
                }
                for c in &a.conditions {
                    if let Some(m) = &c.matches {
                        regex(what, m)?;
                    }
                }
            }
            Ok(())
        };
        for r in &k.reactions {
            let what = format!("reaction {}", r.name);
            regex(&what, &r.when.tool)?;
            regex(&what, &r.when.input)?;
            regex(&what, &r.when.output)?;
            if !matches!(r.when.result.as_str(), "" | "success" | "failure" | "any") {
                return Err(bad_request(format!("kit {k_name}: {what}: result is success, failure or any")));
            }
            if let Some(c) = &r.check {
                probe(&what, c)?;
            }
            actions(&what, &r.then)?;
        }
        for t in &k.trackers {
            let what = format!("tracker {}", t.name);
            probe(&what, &t.probe)?;
            if let Some(l) = &t.logs {
                probe(&what, l)?;
            }
            for p in &t.phases {
                if !PHASES.contains(&p.phase.as_str()) {
                    return Err(bad_request(format!("kit {k_name}: {what}: phases are {}", PHASES.join(", "))));
                }
            }
            actions(&what, &t.on_end)?;
        }
    }
    Ok(())
}

/// The template context for a session.
fn session_ctx(s: &SessionRow) -> Value {
    let labels = s.all_labels();
    let repo = labels.iter().find_map(|l| l.strip_prefix("repo:")).unwrap_or("").to_owned();
    json!({"id": s.id, "title": s.title, "cwd": s.cwd, "worker": s.worker_name, "repo": repo, "labels": labels})
}

/// The value at a dotted path (`check.json.items.0.name`).
pub fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    let mut v = ctx;
    for part in path.trim().split('.').filter(|p| !p.is_empty()) {
        v = match v {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Fills `{{path}}` from the context (missing: empty).
pub fn render(tpl: &str, ctx: &Value) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find("}}") {
            Some(j) => {
                out.push_str(&lookup(ctx, &rest[i + 2..i + 2 + j]).map(as_text).unwrap_or_default());
                rest = &rest[i + 2 + j + 2..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether all conditions hold.
pub fn holds(conds: &[api::Condition], ctx: &Value) -> bool {
    conds.iter().all(|c| {
        let v = lookup(ctx, &c.var);
        let present = v.is_some_and(|v| !(v.is_null() || v == "" || v.as_array().is_some_and(|a| a.is_empty())));
        if let Some(e) = c.exists
            && e != present
        {
            return false;
        }
        let same = |want: &Value| v.is_some_and(|v| v == want || as_text(v) == as_text(want));
        if let Some(e) = &c.equals
            && !same(e)
        {
            return false;
        }
        if let Some(n) = &c.not_equals
            && same(n)
        {
            return false;
        }
        if let Some(m) = &c.matches {
            let text = v.map(as_text).unwrap_or_default();
            if !regex::Regex::new(m).is_ok_and(|r| r.is_match(&text)) {
                return false;
            }
        }
        true
    })
}

/// Runs a probe; its output as a context value: exit, stdout, stderr,
/// lines, json (when stdout is JSON).
pub async fn run_probe(
    st: &HeadState,
    session: Option<&SessionRow>,
    labels: &[String],
    p: &api::Probe,
    ctx: &Value,
) -> anyhow::Result<Value> {
    let command = render(&p.command, ctx);
    let mut env = serde_json::Map::new();
    for name in &p.secrets {
        env.insert(name.clone(), json!(secret_value(st, name, labels, if p.on == "worker" { &p.worker } else { "" }).await?));
    }
    let timeout = p.timeout_s.unwrap_or(60).clamp(1, 600);
    let params = |cwd: &str| json!({"command": command, "cwd": cwd, "env": env, "timeout_s": timeout});
    let wait = Duration::from_secs(timeout + 15);
    let out = match p.on.as_str() {
        "head" => run_local(&command, &env, timeout).await?,
        "worker" => {
            let w = st
                .store
                .workers()
                .await?
                .into_iter()
                .find(|w| w.name == p.worker || w.id == p.worker)
                .ok_or_else(|| anyhow::anyhow!("no worker {:?}", p.worker))?;
            st.registry
                .call(w.id.parse::<WorkerId>()?, "cmd.run", params(""), wait)
                .await
                .map_err(|e| anyhow::anyhow!("worker {}: {e}", w.name))?
        }
        _ => {
            let s = session.ok_or_else(|| anyhow::anyhow!("this probe runs in a session; give one"))?;
            let cwd = if s.cwd.starts_with('/') && s.ssh_target.is_empty() { s.cwd.as_str() } else { "" };
            st.registry
                .call(s.worker_id.parse::<WorkerId>()?, "cmd.run", params(cwd), wait)
                .await
                .map_err(|e| anyhow::anyhow!("worker {}: {e}", s.worker_name))?
        }
    };
    let stdout = out["stdout"].as_str().unwrap_or("").to_owned();
    let json: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    Ok(json!({
        "command": command,
        "exit": out["exit"],
        "stdout": stdout,
        "stderr": out["stderr"],
        "lines": stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>(),
        "json": json,
    }))
}

async fn run_local(command: &str, env: &serde_json::Map<String, Value>, timeout: u64) -> anyhow::Result<Value> {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(command).stdin(std::process::Stdio::null()).kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v.as_str().unwrap_or_default());
    }
    Ok(match tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await {
        Ok(o) => {
            let o = o?;
            json!({"exit": o.status.code().unwrap_or(-1), "stdout": String::from_utf8_lossy(&o.stdout), "stderr": String::from_utf8_lossy(&o.stderr)})
        }
        Err(_) => json!({"exit": -1, "stdout": "", "stderr": format!("stopped after {timeout}s")}),
    })
}

/// A secret's value for a probe: it must apply to the session's labels
/// (and, if it names workers, to the worker it runs on).
async fn secret_value(st: &HeadState, name: &str, labels: &[String], worker: &str) -> anyhow::Result<String> {
    let (blob, applies_to) = st.store.secret(name).await?.ok_or_else(|| anyhow::anyhow!("no secret {name}"))?;
    let (workers, rest): (Vec<String>, Vec<String>) = applies_to.into_iter().partition(|l| l.starts_with("worker:"));
    anyhow::ensure!(
        workers.is_empty() || worker.is_empty() || workers.iter().any(|w| w == &format!("worker:{worker}")),
        "secret {name} is not for worker {worker}"
    );
    anyhow::ensure!(api::labels::applies(&rest, labels), "secret {name} does not apply to this session's labels");
    st.secrets.decrypt(&blob)
}

/// A finished tool call: the kits' reactions that match it.
async fn react(st: &HeadState, job: Job) -> anyhow::Result<()> {
    let Some(session) = st.store.session(&job.session_id).await? else { return Ok(()) };
    if session.parent_call_id.is_some() {
        return Ok(()); // subagents: their parent's tools are what count
    }
    let labels = session.all_labels();
    let kits: Vec<api::Kit> = kits(st)
        .await?
        .into_iter()
        .filter(|k| k.state == "active" && !k.reactions.is_empty() && api::labels::applies(&k.applies_to, &labels))
        .collect();
    if kits.is_empty() {
        return Ok(());
    }
    let input = match job.input {
        Some(i) => i,
        None => find_input(st, &job.session_id, job.seq).await.unwrap_or_default(),
    };
    for kit in &kits {
        for r in &kit.reactions {
            let w = &r.when;
            let re = |p: &str, text: &str| p.is_empty() || regex::Regex::new(p).is_ok_and(|r| r.is_match(text));
            let result_ok = match w.result.as_str() {
                "failure" => job.is_error,
                "any" => true,
                _ => !job.is_error,
            };
            if !result_ok || !re(&w.tool, &job.tool) || !re(&w.input, &input) || !re(&w.output, &job.output) {
                continue;
            }
            let mut vars = serde_json::Map::new();
            if !w.output.is_empty()
                && let Ok(rx) = regex::Regex::new(&w.output)
                && let Some(c) = rx.captures(&job.output)
            {
                for name in rx.capture_names().flatten() {
                    if let Some(m) = c.name(name) {
                        vars.insert(name.to_owned(), json!(m.as_str()));
                    }
                }
            }
            let mut ctx = json!({
                "session": session_ctx(&session),
                "vars": vars,
                "call": {"tool": job.tool, "input": serde_json::from_str::<Value>(&input).unwrap_or(json!(input)), "output": crate::util_clip(&job.output, 4000)},
            });
            if let Some(check) = &r.check {
                ctx["check"] = match run_probe(st, Some(&session), &labels, check, &ctx).await {
                    Ok(v) => v,
                    Err(e) => json!({"error": format!("{e:#}"), "exit": -1}),
                };
            }
            let source = format!("{}/{}", kit.name, r.name);
            for a in &r.then {
                if holds(&a.conditions, &ctx)
                    && let Err(e) = act(st, &session, kit, a, &ctx, job.seq, &source).await
                {
                    warn!(kit = %kit.name, reaction = %r.name, "action: {e:#}");
                }
            }
        }
    }
    Ok(())
}

/// A tool call's input, from the log (when the head restarted between the
/// call and its result).
async fn find_input(st: &HeadState, session_id: &str, seq: u64) -> anyhow::Result<String> {
    let events = st.store.events_after(session_id, seq.saturating_sub(200), 200).await?;
    let call = events.iter().find(|e| e.seq == seq).and_then(|e| match &e.kind {
        Some(session_event::Kind::ToolResult(r)) => Some(r.call_id.clone()),
        _ => None,
    });
    let Some(call) = call else { return Ok(String::new()) };
    for e in events.iter().rev() {
        if let Some(session_event::Kind::Assistant(m)) = &e.kind {
            for b in &m.blocks {
                if let Some(content_block::Block::ToolUse(t)) = &b.block
                    && t.id == call
                {
                    return Ok(t.input_json.clone());
                }
            }
        }
    }
    Ok(String::new())
}

async fn act(
    st: &HeadState,
    session: &SessionRow,
    kit: &api::Kit,
    a: &api::Action,
    ctx: &Value,
    anchor: u64,
    source: &str,
) -> anyhow::Result<()> {
    let text = render(&a.text, ctx);
    match a.kind.as_str() {
        "suggest" => {
            let prompt = if a.prompt.is_empty() { text.clone() } else { render(&a.prompt, ctx) };
            suggest(st, &session.id, anchor, "next", &text, &prompt, source).await?;
        }
        "tell" => {
            suggest(st, &session.id, anchor, "note", &text, &text, source).await?;
        }
        "notify" => st.notifier.send(crate::notify::Note {
            event: "notice",
            title: text,
            message: session.title.clone(),
            priority: "normal",
            session_id: Some(session.id.clone()),
            labels: session.all_labels(),
            call_id: None,
        }),
        "track" => {
            let def = kit.trackers.iter().find(|t| t.name == a.tracker).ok_or_else(|| anyhow::anyhow!("no tracker {}", a.tracker))?;
            start_tracker(st, session, kit, def, ctx, anchor).await?;
        }
        other => anyhow::bail!("unknown action {other}"),
    }
    Ok(())
}

/// Offers a next step (or a note for the agent); not twice while open.
pub async fn suggest(
    st: &HeadState,
    session_id: &str,
    anchor: u64,
    kind: &str,
    text: &str,
    prompt: &str,
    source: &str,
) -> anyhow::Result<Option<i64>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let dup: Option<i64> =
        sqlx::query_scalar("SELECT id FROM suggestions WHERE session_id = ? AND kind = ? AND text = ? AND state = 'open'")
            .bind(session_id)
            .bind(kind)
            .bind(text)
            .fetch_optional(&st.store.pool)
            .await?;
    if dup.is_some() {
        return Ok(None);
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO suggestions (session_id, anchor_seq, kind, text, prompt, source, created_at) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(session_id)
    .bind(anchor as i64)
    .bind(kind)
    .bind(text)
    .bind(prompt.trim())
    .bind(source)
    .bind(now_ms())
    .fetch_one(&st.store.pool)
    .await?;
    publish_suggestion(st, id).await;
    Ok(Some(id))
}

fn suggestion_of(r: &sqlx::sqlite::SqliteRow) -> api::Suggestion {
    api::Suggestion {
        id: r.get("id"),
        session_id: r.get("session_id"),
        anchor_seq: r.get::<i64, _>("anchor_seq") as u64,
        kind: r.get("kind"),
        text: r.get("text"),
        prompt: r.get("prompt"),
        source: r.get("source"),
        state: r.get("state"),
        created_at: r.get("created_at"),
    }
}

async fn publish_suggestion(st: &HeadState, id: i64) {
    if let Ok(Some(r)) = sqlx::query("SELECT * FROM suggestions WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await {
        let s = suggestion_of(&r);
        st.hub.publish(&s.session_id.clone(), api::StreamMsg::Suggestion { suggestion: s });
    }
}

/// Marks a suggestion taken or dismissed. The taken one's prompt.
pub async fn decide_suggestion(st: &HeadState, session_id: &str, id: i64, take: bool) -> anyhow::Result<String> {
    let r = sqlx::query("SELECT * FROM suggestions WHERE id = ? AND session_id = ?")
        .bind(id)
        .bind(session_id)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| crate::not_found("no such next step"))?;
    sqlx::query("UPDATE suggestions SET state = ? WHERE id = ?")
        .bind(if take { "taken" } else { "dismissed" })
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    publish_suggestion(st, id).await;
    Ok(r.get("prompt"))
}

/// What goes to the model with your next message: notes (once), open next
/// steps, and trackers that ended since.
pub async fn context_for_input(st: &HeadState, session_id: &str) -> String {
    let Ok(rows) = sqlx::query("SELECT * FROM suggestions WHERE session_id = ? AND state = 'open' ORDER BY id")
        .bind(session_id)
        .fetch_all(&st.store.pool)
        .await
    else {
        return String::new();
    };
    let (mut notes, mut next) = (Vec::new(), Vec::new());
    for r in &rows {
        let s = suggestion_of(r);
        if s.kind == "note" {
            notes.push(s);
        } else {
            next.push(s);
        }
    }
    if notes.is_empty() && next.is_empty() {
        return String::new();
    }
    let mut out = String::from("<carbon6-followups>\n");
    for n in &notes {
        out.push_str(&format!("Note: {}\n", n.text));
    }
    if !next.is_empty() {
        out.push_str("Next steps offered to the user (a \"yes\" may mean one of these):\n");
        for n in &next {
            out.push_str(&format!("- {}\n", n.text));
        }
    }
    out.push_str("</carbon6-followups>");
    for n in notes {
        let _ = sqlx::query("UPDATE suggestions SET state = 'delivered' WHERE id = ?").bind(n.id).execute(&st.store.pool).await;
        publish_suggestion(st, n.id).await;
    }
    out
}

fn tracker_of(r: &sqlx::sqlite::SqliteRow) -> api::Tracker {
    api::Tracker {
        id: r.get("id"),
        session_id: r.get("session_id"),
        anchor_seq: r.get::<i64, _>("anchor_seq") as u64,
        kit: r.get("kit"),
        name: r.get("name"),
        title: r.get("title"),
        phase: r.get("phase"),
        line: r.get("line"),
        url: r.get("url"),
        error: r.get("error"),
        started_at: r.get("started_at"),
        updated_at: r.get("updated_at"),
        ended_at: r.get("ended_at"),
        investigation: r.get("investigation"),
    }
}

pub async fn tracker(st: &HeadState, id: i64) -> anyhow::Result<Option<api::Tracker>> {
    Ok(sqlx::query("SELECT * FROM trackers WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?.as_ref().map(tracker_of))
}

pub async fn observations(st: &HeadState, id: i64) -> anyhow::Result<Vec<api::TrackerObservation>> {
    let rows =
        sqlx::query("SELECT * FROM tracker_observations WHERE tracker_id = ? ORDER BY ts, id").bind(id).fetch_all(&st.store.pool).await?;
    Ok(rows
        .iter()
        .map(|r| api::TrackerObservation {
            ts: r.get("ts"),
            phase: r.get("phase"),
            line: r.get("line"),
            url: r.get("url"),
            output: r.get("output"),
        })
        .collect())
}

async fn publish_tracker(st: &HeadState, id: i64) {
    if let Ok(Some(t)) = tracker(st, id).await {
        st.hub.publish(&t.session_id.clone(), api::StreamMsg::Tracker { tracker: t });
        st.registry.bump();
    }
}

/// A session's next steps and trackers.
pub async fn for_session(st: &HeadState, session_id: &str) -> anyhow::Result<api::Followups> {
    let s = sqlx::query("SELECT * FROM suggestions WHERE session_id = ? ORDER BY id").bind(session_id).fetch_all(&st.store.pool).await?;
    let t = sqlx::query("SELECT * FROM trackers WHERE session_id = ? ORDER BY id").bind(session_id).fetch_all(&st.store.pool).await?;
    Ok(api::Followups { suggestions: s.iter().map(suggestion_of).collect(), trackers: t.iter().map(tracker_of).collect() })
}

async fn start_tracker(
    st: &HeadState,
    session: &SessionRow,
    kit: &api::Kit,
    def: &api::TrackerDef,
    ctx: &Value,
    anchor: u64,
) -> anyhow::Result<()> {
    let title = {
        let t = render(&def.title, ctx);
        if t.trim().is_empty() { def.name.clone() } else { t }
    };
    // Not twice for the same thing.
    let dup: Option<i64> =
        sqlx::query_scalar("SELECT id FROM trackers WHERE session_id = ? AND kit = ? AND name = ? AND title = ? AND ended_at IS NULL")
            .bind(&session.id)
            .bind(&kit.name)
            .bind(&def.name)
            .bind(&title)
            .fetch_optional(&st.store.pool)
            .await?;
    if dup.is_some() {
        return Ok(());
    }
    let now = now_ms();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO trackers (session_id, anchor_seq, kit, name, title, def, vars, phase, line, started_at, updated_at, next_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'waiting', 'starting…', ?, ?, ?) RETURNING id",
    )
    .bind(&session.id)
    .bind(anchor as i64)
    .bind(&kit.name)
    .bind(&def.name)
    .bind(&title)
    .bind(serde_json::to_string(def)?)
    .bind(ctx.to_string())
    .bind(now)
    .bind(now)
    .bind(now)
    .fetch_one(&st.store.pool)
    .await?;
    // Its history starts here.
    sqlx::query("INSERT INTO tracker_observations (tracker_id, ts, phase, line, url, output) VALUES (?, ?, 'waiting', 'started', '', '')")
        .bind(id)
        .bind(now)
        .execute(&st.store.pool)
        .await?;
    publish_tracker(st, id).await;
    Ok(())
}

/// Probes the trackers that are due.
async fn poll_due(st: &Arc<HeadState>) -> anyhow::Result<()> {
    let due: Vec<i64> = sqlx::query_scalar("SELECT id FROM trackers WHERE ended_at IS NULL AND next_at <= ? ORDER BY next_at LIMIT 50")
        .bind(now_ms())
        .fetch_all(&st.store.pool)
        .await?;
    for id in due {
        if !st.followups.busy.lock().unwrap().insert(id) {
            continue;
        }
        let st = st.clone();
        tokio::spawn(async move {
            if let Err(e) = poll(&st, id).await {
                warn!(tracker = id, "polling: {e:#}");
            }
            st.followups.busy.lock().unwrap().remove(&id);
        });
    }
    Ok(())
}

async fn poll(st: &Arc<HeadState>, id: i64) -> anyhow::Result<()> {
    let r = sqlx::query("SELECT * FROM trackers WHERE id = ?").bind(id).fetch_one(&st.store.pool).await?;
    let t = tracker_of(&r);
    let def: api::TrackerDef = serde_json::from_str(&r.get::<String, _>("def"))?;
    let mut ctx: Value = serde_json::from_str(&r.get::<String, _>("vars")).unwrap_or(json!({}));
    let session = st.store.session(&t.session_id).await?;
    let labels = session.as_ref().map(SessionRow::all_labels).unwrap_or_default();
    let now = now_ms();
    let interval = def.interval_s.unwrap_or(30).clamp(1, 3600) as i64 * 1000;
    let (mut phase, mut line, mut url, mut error, mut output) =
        (t.phase.clone(), t.line.clone(), t.url.clone(), String::new(), String::new());
    match run_probe(st, session.as_ref(), &labels, &def.probe, &ctx).await {
        Ok(out) => {
            output = crate::util_clip(out["stdout"].as_str().unwrap_or(""), 8000);
            ctx["probe"] = out.clone();
            let j = &out["json"];
            if let Some(p) = j["phase"].as_str().filter(|p| PHASES.contains(p)) {
                phase = p.to_owned();
                line = j["line"].as_str().map(str::to_owned).unwrap_or_else(|| phase.clone());
                url = j["url"].as_str().map(str::to_owned).unwrap_or(url);
            } else if let Some(rule) = def.phases.iter().find(|p| holds(&p.conditions, &ctx)) {
                phase = rule.phase.clone();
                line = line_or(&render(&def.line, &ctx), &phase);
                let u = render(&def.url, &ctx);
                if !u.is_empty() {
                    url = u;
                }
            } else if out["exit"].as_i64() != Some(0) {
                error = format!("probe exited {}: {}", out["exit"], crate::util_clip(out["stderr"].as_str().unwrap_or(""), 300));
            }
        }
        Err(e) => error = format!("{e:#}"),
    }
    if line == "starting…" && phase == "waiting" {
        line = "waiting…".into();
    }
    // Waiting too long, or going on too long.
    let age = (now - t.started_at) / 1000;
    if phase == "waiting" && age as u64 > def.wait_s.unwrap_or(900) {
        phase = "stalled".into();
        line = format!("nothing to follow after {} minutes", age / 60);
    } else if age as u64 > def.give_up_s.unwrap_or(21_600) && !TERMINAL.contains(&phase.as_str()) {
        phase = "stalled".into();
        line = format!("gave up after {} hours ({line})", age / 3600);
    }
    let changed = (phase.as_str(), line.as_str(), url.as_str()) != (t.phase.as_str(), t.line.as_str(), t.url.as_str());
    if changed {
        sqlx::query("INSERT INTO tracker_observations (tracker_id, ts, phase, line, url, output) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(id)
            .bind(now)
            .bind(&phase)
            .bind(&line)
            .bind(&url)
            .bind(&output)
            .execute(&st.store.pool)
            .await?;
    }
    let ended = TERMINAL.contains(&phase.as_str());
    sqlx::query(
        "UPDATE trackers SET phase = ?, line = ?, url = ?, error = ?, updated_at = ?, next_at = ?, ended_at = ?, vars = ? WHERE id = ?",
    )
    .bind(&phase)
    .bind(&line)
    .bind(&url)
    .bind(&error)
    .bind(now)
    .bind(now + interval)
    .bind(ended.then_some(now))
    .bind(ctx.to_string())
    .bind(id)
    .execute(&st.store.pool)
    .await?;
    if changed || error != t.error {
        publish_tracker(st, id).await;
    }
    if ended && let Some(session) = session {
        finish(st, &session, id, &def, &mut ctx, &phase, &line, &url).await?;
    }
    Ok(())
}

/// A tracker ended: tell the agent, notify, its on_end actions, and on
/// failure an investigation.
#[allow(clippy::too_many_arguments)]
async fn finish(
    st: &HeadState,
    session: &SessionRow,
    id: i64,
    def: &api::TrackerDef,
    ctx: &mut Value,
    phase: &str,
    line: &str,
    url: &str,
) -> anyhow::Result<()> {
    let t = tracker(st, id).await?.ok_or_else(|| anyhow::anyhow!("tracker gone"))?;
    ctx["phase"] = json!(phase);
    ctx["line"] = json!(line);
    ctx["url"] = json!(url);
    let summary = format!("{} {phase}: {line}{}", t.title, if url.is_empty() { String::new() } else { format!(" ({url})") });
    suggest(st, &session.id, t.anchor_seq, "note", &format!("Tracker ended — {summary}"), "", &format!("{}/{}", t.kit, t.name)).await?;
    st.notifier.send(crate::notify::Note {
        event: if phase == "succeeded" { "finished" } else { "failed" },
        title: format!("{}: {phase}", t.title),
        message: format!("{line}\n{}", session.title),
        priority: if phase == "failed" { "high" } else { "normal" },
        session_id: Some(session.id.clone()),
        labels: session.all_labels(),
        call_id: None,
    });
    let kit = kits(st).await?.into_iter().find(|k| k.name == t.kit).unwrap_or_default();
    for a in &def.on_end {
        if holds(&a.conditions, ctx)
            && let Err(e) = act(st, session, &kit, a, ctx, t.anchor_seq, &format!("{}/{}", t.kit, t.name)).await
        {
            warn!(tracker = id, "on_end: {e:#}");
        }
    }
    if phase == "failed" && def.investigate.unwrap_or(true) {
        investigate(st, session, id, def, ctx).await?;
    }
    Ok(())
}

/// Starts an investigation of a failed tracker: a side chat at its anchor
/// that explains the failure and proposes a fix on a branch of its own.
pub async fn investigate(st: &HeadState, session: &SessionRow, id: i64, def: &api::TrackerDef, ctx: &Value) -> anyhow::Result<String> {
    let t = tracker(st, id).await?.ok_or_else(|| crate::not_found("no such tracker"))?;
    if let Some(existing) = &t.investigation {
        return Ok(existing.clone());
    }
    let labels = session.all_labels();
    let logs = match &def.logs {
        Some(p) => match run_probe(st, Some(session), &labels, p, ctx).await {
            Ok(out) => format!(
                "Logs (from `{}`):\n```\n{}\n{}\n```",
                out["command"].as_str().unwrap_or(""),
                tail(out["stdout"].as_str().unwrap_or(""), 20_000),
                tail(out["stderr"].as_str().unwrap_or(""), 4000)
            ),
            Err(e) => format!("Fetching the logs failed: {e:#}"),
        },
        None => "No logs command is configured for this tracker; find the logs from the link if you can reach it.".into(),
    };
    let sha = lookup(ctx, "vars.sha").map(as_text).unwrap_or_default();
    let prompt = format!(
        "The tracker \"{}\" (kit {}) ended {}: {}{}\n\n{logs}\n\nInvestigate why it failed and explain the cause briefly. Then propose a fix: \
create a new branch off the failing commit{} named fix/<branch>-<short sha>, make the change there, commit it, and show the diff. Do not push. \
Finish by offering next steps with the suggest tool: pushing the fix branch (and opening a PR), or applying the change to the working branch.",
        t.title,
        t.kit,
        t.phase,
        t.line,
        if t.url.is_empty() { String::new() } else { format!("\n{}", t.url) },
        if sha.is_empty() { String::new() } else { format!(" ({sha})") },
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
    sqlx::query("UPDATE trackers SET investigation = ? WHERE id = ?").bind(&side.id).bind(id).execute(&st.store.pool).await?;
    publish_tracker(st, id).await;
    Ok(side.id)
}

/// A rendered line, or the phase when it says nothing (its values are not
/// there yet).
fn line_or(line: &str, phase: &str) -> String {
    if line.chars().any(char::is_alphanumeric) { line.trim().to_owned() } else { phase.to_owned() }
}

fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut start = s.len() - n;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// Stops following a tracker.
pub async fn stop(st: &HeadState, id: i64) -> anyhow::Result<()> {
    let n = sqlx::query("UPDATE trackers SET phase = 'stopped', line = 'stopped by you', ended_at = ? WHERE id = ? AND ended_at IS NULL")
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(bad_request("that tracker is not running"));
    }
    publish_tracker(st, id).await;
    Ok(())
}

/// Investigates a tracker now (from the UI).
pub async fn investigate_now(st: &HeadState, id: i64) -> anyhow::Result<String> {
    let r = sqlx::query("SELECT * FROM trackers WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| crate::not_found("no such tracker"))?;
    let def: api::TrackerDef = serde_json::from_str(&r.get::<String, _>("def"))?;
    let ctx: Value = serde_json::from_str(&r.get::<String, _>("vars")).unwrap_or(json!({}));
    let session = st.store.session(&r.get::<String, _>("session_id")).await?.ok_or_else(|| crate::not_found("the session is gone"))?;
    investigate(st, &session, id, &def, &ctx).await
}

/// Runs a kit's checks and probes once, to see that they work.
pub async fn dry_run(st: &HeadState, req: &api::KitDryRun) -> anyhow::Result<Value> {
    let kit = match &req.kit {
        Some(k) => k.clone(),
        None => {
            kits(st).await?.into_iter().find(|k| k.name == req.name).ok_or_else(|| crate::not_found(format!("no kit {:?}", req.name)))?
        }
    };
    check_kits(std::slice::from_ref(&kit))?;
    let session = if req.session_id.is_empty() { None } else { st.store.session(&req.session_id).await? };
    // Without a session, a worker stands in for the session's.
    let stand_in = |p: &api::Probe| -> api::Probe {
        let mut p = p.clone();
        if session.is_none() && !req.worker.is_empty() && matches!(p.on.as_str(), "" | "session") {
            p.on = "worker".into();
            p.worker = req.worker.clone();
        }
        p
    };
    let labels = session.as_ref().map(SessionRow::all_labels).unwrap_or_default();
    let ctx = json!({"session": session.as_ref().map(session_ctx).unwrap_or(json!({})), "vars": req.vars});
    let mut ok = true;
    let mut reactions = Vec::new();
    for r in &kit.reactions {
        let Some(c) = &r.check else {
            reactions.push(json!({"name": r.name, "check": null}));
            continue;
        };
        match run_probe(st, session.as_ref(), &labels, &stand_in(c), &ctx).await {
            Ok(out) => {
                let mut c2 = ctx.clone();
                c2["check"] = out.clone();
                let fire: Vec<String> =
                    r.then.iter().filter(|a| holds(&a.conditions, &c2)).map(|a| format!("{} {}", a.kind, render(&a.text, &c2))).collect();
                ok &= out["exit"].as_i64() == Some(0);
                reactions.push(json!({"name": r.name, "check": out, "would": fire}));
            }
            Err(e) => {
                ok = false;
                reactions.push(json!({"name": r.name, "error": format!("{e:#}")}));
            }
        }
    }
    let mut trackers = Vec::new();
    for t in &kit.trackers {
        match run_probe(st, session.as_ref(), &labels, &stand_in(&t.probe), &ctx).await {
            Ok(out) => {
                let mut c2 = ctx.clone();
                c2["probe"] = out.clone();
                let (phase, line, url) = match out["json"]["phase"].as_str() {
                    Some(p) => (
                        p.to_owned(),
                        out["json"]["line"].as_str().unwrap_or(p).to_owned(),
                        out["json"]["url"].as_str().unwrap_or("").to_owned(),
                    ),
                    None => match t.phases.iter().find(|p| holds(&p.conditions, &c2)) {
                        Some(rule) => (rule.phase.clone(), line_or(&render(&t.line, &c2), &rule.phase), render(&t.url, &c2)),
                        None => ("(no phase matched)".into(), String::new(), String::new()),
                    },
                };
                ok &= PHASES.contains(&phase.as_str());
                trackers.push(json!({"name": t.name, "phase": phase, "line": line, "url": url, "probe": out}));
            }
            Err(e) => {
                ok = false;
                trackers.push(json!({"name": t.name, "error": format!("{e:#}")}));
            }
        }
    }
    if req.update_state && req.kit.is_none() {
        let mut all = kits(st).await?;
        if let Some(k) = all.iter_mut().find(|k| k.name == kit.name) {
            k.state = if ok { "active" } else { "broken" }.into();
        }
        st.store.set_setting(KITS, &serde_json::to_string(&all)?).await?;
        if !ok {
            crate::todos::add(
                st,
                &api::TodoIn {
                    kind: "kit".into(),
                    title: format!("Kit {} does not work", kit.name),
                    detail: "Its dry run failed; it is marked broken and does nothing until fixed.".into(),
                    context: json!({"kits": [kit.name]}),
                    source: "kits".into(),
                    priority: "normal".into(),
                    group: format!("kit:{}", kit.name),
                },
            )
            .await?;
        }
    }
    Ok(json!({"ok": ok, "reactions": reactions, "trackers": trackers}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_and_conditions() {
        let ctx =
            json!({"vars": {"sha": "abc123"}, "check": {"exit": 0, "json": {"state": "OPEN", "items": [{"n": 2}]}, "lines": ["main"]}});
        assert_eq!(
            render("CI for {{vars.sha}} on {{check.lines.0}} ({{check.json.items.0.n}}){{missing}}", &ctx),
            "CI for abc123 on main (2)"
        );
        assert_eq!(render("{{ vars.sha }} {{unclosed", &ctx), "abc123 {{unclosed");
        let c = |v: Value| -> api::Condition { serde_json::from_value(v).unwrap() };
        assert!(holds(&[c(json!({"var": "check.exit", "equals": 0}))], &ctx));
        assert!(holds(&[c(json!({"var": "check.exit", "equals": "0"}))], &ctx), "numbers compare as text too");
        assert!(holds(&[c(json!({"var": "check.json.state", "matches": "^OPEN$"}))], &ctx));
        assert!(holds(&[c(json!({"var": "check.json.number", "exists": false}))], &ctx));
        assert!(!holds(&[c(json!({"var": "vars.sha", "not_equals": "abc123"}))], &ctx));
    }

    #[test]
    fn kits_are_checked() {
        let kit = |v: Value| -> Vec<api::Kit> { vec![serde_json::from_value(v).unwrap()] };
        let good = kit(
            json!({"name": "ci", "reactions": [{"name": "push", "when": {"tool": "bash", "input": "git push"}, "then": [{"kind": "track", "tracker": "ci"}]}],
            "trackers": [{"name": "ci", "probe": {"command": "true", "on": "worker", "worker": "build-1"}}]}),
        );
        assert!(check_kits(&good).is_ok());
        assert!(check_kits(&kit(json!({"name": "x", "reactions": [{"name": "r", "when": {"tool": "("}}]}))).is_err());
        assert!(
            check_kits(&kit(
                json!({"name": "x", "reactions": [{"name": "r", "when": {}, "then": [{"kind": "track", "tracker": "nope"}]}]})
            ))
            .is_err()
        );
        assert!(check_kits(&kit(json!({"name": "x", "trackers": [{"name": "t", "probe": {"command": "x", "on": "worker"}}]}))).is_err());
    }
}
