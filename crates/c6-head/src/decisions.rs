//! Decisions (docs/os.md §4.2): every time carbon6 needs you, in one list.
//! An agent's question (asked without stopping: the answer comes back to
//! its session as a message), a tool call or workflow step waiting for
//! approval, a question from the assistant, something to confirm or
//! provide, a dispute to settle. A decision with `ref` has something
//! waiting on it: answering it here answers that, and answering that
//! elsewhere (the session, a link) settles the decision. With a deadline,
//! an unanswered decision takes its default answer (else "no").

use std::sync::Arc;
use std::time::Duration;

use c6_api as api;
use c6_core::now_ms;
use c6_proto::{SessionEvent, session_event};
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};

const KINDS: [&str; 8] = ["approve", "choose", "confirm", "provide", "question", "dispute", "follow_up", "task"];
/// How long a tool call waits for approval on the worker (then it is "no").
const APPROVAL_WAIT_MS: i64 = 60 * 60 * 1000;

fn decision_of(r: &sqlx::sqlite::SqliteRow) -> api::Decision {
    let json_col = |c: &str| serde_json::from_str::<Value>(&r.get::<String, _>(c)).unwrap_or(Value::Null);
    api::Decision {
        id: r.get("id"),
        realm: r.get("realm"),
        kind: r.get("kind"),
        title: r.get("title"),
        document: r.get("document"),
        context: serde_json::from_str(&r.get::<String, _>("context")).unwrap_or(json!({})),
        source: r.get("source"),
        priority: r.get("priority"),
        group: r.get("grp"),
        options: serde_json::from_value(json_col("options")).unwrap_or_default(),
        form: serde_json::from_value(json_col("form")).unwrap_or_default(),
        state: r.get("state"),
        snooze_until: r.get("snooze_until"),
        deadline: r.get("deadline"),
        default_answer: r.get("default_answer"),
        answer: r.get::<Option<String>, _>("answer").and_then(|a| serde_json::from_str(&a).ok()),
        note: r.get("note"),
        answered_via: r.get("answered_via"),
        answered_at: r.get("answered_at"),
        // Something waits on it, unless it's a task card (that only offers work).
        waiting: {
            let r: String = r.get("ref");
            !r.is_empty() && !r.starts_with("task:")
        },
        sessions: serde_json::from_str(&r.get::<String, _>("sessions")).unwrap_or_default(),
        count: r.get::<i64, _>("count") as u32,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Opens a decision in `realm`, with `reference` naming what waits on it
/// (empty: nothing). An open one of the same group there is raised again
/// instead; so is an open one with the same reference.
pub async fn open(st: &HeadState, d: &api::DecisionIn, realm: &str, reference: &str) -> anyhow::Result<api::Decision> {
    let title = d.title.trim();
    if title.is_empty() || title.len() > 500 {
        return Err(bad_request("a decision needs a title (up to 500 characters)"));
    }
    let kind = if KINDS.contains(&d.kind.as_str()) { d.kind.as_str() } else { "question" };
    let priority = if matches!(d.priority.as_str(), "high" | "low") { d.priority.as_str() } else { "normal" };
    if d.options.iter().any(|o| o.label.trim().is_empty()) {
        return Err(bad_request("every option needs a label"));
    }
    if let Some(def) = &d.default_answer
        && !d.options.iter().any(|o| &o.label == def)
    {
        return Err(bad_request(format!("the default answer {def:?} is not one of the options")));
    }
    let now = now_ms();
    let same: Option<i64> = if !reference.is_empty() {
        sqlx::query_scalar("SELECT id FROM decisions WHERE ref = ? AND state IN ('open', 'snoozed') LIMIT 1")
            .bind(reference)
            .fetch_optional(&st.store.pool)
            .await?
    } else if !d.group.is_empty() {
        sqlx::query_scalar("SELECT id FROM decisions WHERE grp = ? AND realm = ? AND state IN ('open', 'snoozed') LIMIT 1")
            .bind(&d.group)
            .bind(realm)
            .fetch_optional(&st.store.pool)
            .await?
    } else {
        None
    };
    if let Some(id) = same {
        sqlx::query(
            "UPDATE decisions SET count = count + 1, updated_at = ?, document = CASE WHEN ? = '' THEN document ELSE ? END WHERE id = ?",
        )
        .bind(now)
        .bind(&d.document)
        .bind(&d.document)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
        st.registry.bump();
        return get(st, id).await?.ok_or_else(|| anyhow::anyhow!("decision gone"));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO decisions (kind, title, document, context, source, priority, grp, created_at, updated_at, realm, options, form, \
         deadline, default_answer, ref) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(kind)
    .bind(title)
    .bind(d.document.trim())
    .bind(if d.context.is_null() { "{}".to_owned() } else { d.context.to_string() })
    .bind(if d.source.is_empty() { "you" } else { d.source.as_str() })
    .bind(priority)
    .bind(&d.group)
    .bind(now)
    .bind(now)
    .bind(realm)
    .bind(serde_json::to_string(&d.options)?)
    .bind(serde_json::to_string(&d.form)?)
    .bind(d.deadline)
    .bind(&d.default_answer)
    .bind(reference)
    .fetch_one(&st.store.pool)
    .await?;
    st.registry.bump();
    let decision = get(st, id).await?.ok_or_else(|| anyhow::anyhow!("decision gone"))?;
    announce(st, &decision, reference).await;
    crate::triggers::on_decision(st, &decision, "opened").await;
    if let Some(session) = reference.strip_prefix("approval:").and_then(|r| r.split(':').next()) {
        crate::triggers::on_session(st, session, "needs_approval", &decision.title).await;
    }
    Ok(decision)
}

/// Tells you about a new decision (approvals have their own notification,
/// with their link; those you raised yourself, and task cards, need none).
async fn announce(st: &HeadState, d: &api::Decision, reference: &str) {
    // Task cards aren't urgent: they wait in their session and in Decisions.
    if reference.starts_with("approval:") || reference.starts_with("workflow:") || reference.starts_with("task:") || d.source == "you" {
        return;
    }
    st.notifier.send(crate::notify::Note {
        event: "decision",
        title: format!("Decision needed: {}", d.title),
        message: d.document.chars().take(600).collect(),
        priority: if d.priority == "high" { "high" } else { "normal" },
        session_id: (!d.options.is_empty()).then(|| "dc:".to_owned()),
        call_id: (!d.options.is_empty()).then(|| d.id.to_string()),
        labels: Vec::new(),
        path: None,
    });
}

pub async fn get(st: &HeadState, id: i64) -> anyhow::Result<Option<api::Decision>> {
    Ok(sqlx::query("SELECT * FROM decisions WHERE id = ?").bind(id).fetch_optional(&st.store.pool).await?.as_ref().map(decision_of))
}

/// Decisions in a state ("open" includes snoozed ones whose time has
/// come), that the caller may see.
pub async fn list(st: &HeadState, state: &str, caller: &Caller) -> anyhow::Result<Vec<api::Decision>> {
    let now = now_ms();
    let rows =
        match state {
            "" | "open" => sqlx::query(
                "SELECT * FROM decisions WHERE (state = 'open' OR (state = 'snoozed' AND snooze_until <= ?)) AND (? IS NULL OR realm = ?) \
                 ORDER BY (ref = '' OR ref LIKE 'task:%'), CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END, grp, updated_at DESC",
            )
            .bind(now)
            .bind(caller.listing_realm())
            .bind(caller.listing_realm())
            .fetch_all(&st.store.pool)
            .await?,
            s => {
                sqlx::query("SELECT * FROM decisions WHERE state = ? AND (? IS NULL OR realm = ?) ORDER BY updated_at DESC LIMIT 200")
                    .bind(s)
                    .bind(caller.listing_realm())
                    .bind(caller.listing_realm())
                    .fetch_all(&st.store.pool)
                    .await?
            }
        };
    Ok(rows.iter().map(decision_of).collect())
}

/// A session's open decisions, to answer in it: what its agent raised or
/// asked without stopping, what it asked a grant for, and what it was
/// started to work on. Not its tool approvals and questions: the session
/// shows those itself, from its log.
pub async fn of_session(st: &HeadState, session_id: &str, caller: &Caller) -> anyhow::Result<Vec<api::Decision>> {
    let rows = sqlx::query(
        "SELECT * FROM decisions WHERE (state = 'open' OR (state = 'snoozed' AND snooze_until <= ?)) AND (? IS NULL OR realm = ?) \
         AND (json_extract(context, '$.session') = ? OR EXISTS (SELECT 1 FROM json_each(decisions.sessions) WHERE value = ?)) \
         AND ref NOT LIKE 'approval:%' AND ref NOT LIKE 'question:%' ORDER BY created_at",
    )
    .bind(now_ms())
    .bind(caller.listing_realm())
    .bind(caller.listing_realm())
    .bind(session_id)
    .bind(session_id)
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows.iter().map(decision_of).collect())
}

pub async fn open_count(st: &HeadState, caller: &Caller) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM decisions WHERE (state = 'open' OR (state = 'snoozed' AND snooze_until <= ?)) AND (? IS NULL OR realm = ?)",
    )
    .bind(now_ms())
    .bind(caller.listing_realm())
    .bind(caller.listing_realm())
    .fetch_one(&st.store.pool)
    .await?)
}

/// Snoozes, dismisses or reopens a decision (answering is `answer`).
pub async fn edit(st: &HeadState, id: i64, e: &api::DecisionEdit) -> anyhow::Result<api::Decision> {
    let mut d = get(st, id).await?.ok_or_else(|| not_found("no such decision"))?;
    if let Some(s) = &e.state {
        if !matches!(s.as_str(), "open" | "snoozed" | "dismissed") {
            return Err(bad_request("a decision is opened, snoozed or dismissed here; answer it with …/answer"));
        }
        if d.waiting && s == "dismissed" {
            return Err(bad_request("something waits on this decision: answer it instead"));
        }
        d.state = s.clone();
    }
    if let Some(n) = &e.note {
        d.note = n.trim().to_owned();
    }
    if d.state == "snoozed" {
        d.snooze_until = Some(e.snooze_until.unwrap_or_else(|| now_ms() + 86_400_000));
    }
    sqlx::query("UPDATE decisions SET state = ?, note = ?, snooze_until = ?, updated_at = ? WHERE id = ?")
        .bind(&d.state)
        .bind(&d.note)
        .bind(d.snooze_until)
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(d)
}

/// Answers a decision: records it, then answers what waits on it.
pub async fn answer(st: &HeadState, id: i64, a: &api::DecisionAnswer, via: &str) -> anyhow::Result<api::Decision> {
    let d = get(st, id).await?.ok_or_else(|| not_found("no such decision"))?;
    if !matches!(d.state.as_str(), "open" | "snoozed") {
        return Err(bad_request(format!("decision {id} is already {}", d.state)));
    }
    let option = a.option.as_deref().map(str::trim).filter(|o| !o.is_empty());
    match option {
        Some(o) if !d.options.iter().any(|x| x.label == o) => {
            return Err(bad_request(format!("{o:?} is not one of its options")));
        }
        None if !d.options.is_empty() => return Err(bad_request("choose one of its options")),
        _ => {}
    }
    for f in &d.form {
        let v = a.values.get(&f.name);
        if f.required && v.is_none_or(|v| v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty())) {
            return Err(bad_request(format!("{} is required", if f.label.is_empty() { &f.name } else { &f.label })));
        }
        if !f.choices.is_empty()
            && let Some(s) = v.and_then(Value::as_str)
            && !f.choices.iter().any(|c| c == s)
        {
            return Err(bad_request(format!("{}: {s:?} is not one of its choices", f.name)));
        }
    }
    if a.values.keys().any(|k| !d.form.iter().any(|f| &f.name == k)) {
        return Err(bad_request("answer only the fields it asks for"));
    }
    let answer = json!({"option": option, "values": a.values});
    let reference: String = sqlx::query_scalar("SELECT ref FROM decisions WHERE id = ?").bind(id).fetch_one(&st.store.pool).await?;
    // What waits on it first: if that fails, the decision stays open.
    act(st, &d, &reference, option, &a.values, &a.note).await?;
    record(st, id, "answered", &answer, &a.note, via).await?;
    info!(decision = id, ?option, via, "decision answered");
    get(st, id).await?.ok_or_else(|| anyhow::anyhow!("decision gone"))
}

async fn record(st: &HeadState, id: i64, state: &str, answer: &Value, note: &str, via: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE decisions SET state = ?, answer = ?, note = CASE WHEN ? = '' THEN note ELSE ? END, answered_via = ?, answered_at = ?, \
         updated_at = ? WHERE id = ?",
    )
    .bind(state)
    .bind(answer.to_string())
    .bind(note)
    .bind(note)
    .bind(via)
    .bind(now_ms())
    .bind(now_ms())
    .bind(id)
    .execute(&st.store.pool)
    .await?;
    st.registry.bump();
    if let Ok(Some(d)) = get(st, id).await {
        crate::triggers::on_decision(st, &d, state).await;
    }
    Ok(())
}

/// Answers what waits on a decision.
async fn act(
    st: &HeadState,
    d: &api::Decision,
    reference: &str,
    option: Option<&str>,
    values: &serde_json::Map<String, Value>,
    note: &str,
) -> anyhow::Result<()> {
    let mut parts = reference.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("approval"), Some(session), Some(call)) => {
            let allow = option == Some("Allow");
            if !crate::sessions::decide(st, session, call, allow).await? {
                return Err(anyhow::Error::new(crate::UserError::BadRequest(
                    "the session's worker is not connected; try again when it is back".into(),
                )));
            }
        }
        (Some("workflow"), Some(run), Some(step)) => {
            if !crate::workflows::decide(st, run, step, option == Some("Go ahead")).await? {
                return Err(bad_request("that step is not waiting any more"));
            }
        }
        (Some("question"), Some(session), Some(call)) => {
            if d.kind == "provide" && d.form.is_empty() {
                return Err(bad_request("this asks for a secret: answer it in the conversation"));
            }
            if !crate::sessions::answer_question(st, session, call, values.clone(), false).await? {
                return Err(bad_request("the conversation is not running"));
            }
        }
        (Some("grant"), Some(id), _) => {
            let allow = option == Some("Allow");
            crate::grants::answer_request(st, id, allow).await?;
            // The session that asked hears the answer.
            if let Some(session) = d.context["session"].as_str() {
                let text =
                    format!("Your grant request was {} (decision #{}: {}).", if allow { "allowed" } else { "denied" }, d.id, d.title);
                if let Err(e) = crate::sessions::send_input(st, session, &api::SendMessage { text, ..Default::default() }).await {
                    warn!(decision = d.id, "telling the session: {e:#}");
                }
            }
        }
        (Some("force"), Some(_), _) => {
            crate::collections::answer_force(st, d, option).await?;
        }
        (Some("app" | "package"), Some(_), Some(_)) => {
            crate::packages::answer(st, reference, option, values).await?;
        }
        (Some("update"), Some(version), _) => {
            crate::updates::answer(st, version, option).await?;
        }
        (Some("task"), Some(_), Some(_)) => {
            if option == Some("Start it") {
                start_task(st, d).await?;
            }
        }
        (Some("publish"), _, _) => {
            crate::commons::answer(st, d, option == Some("Publish")).await?;
        }
        (Some("session"), Some(session), _) => {
            // An agent asked without stopping: its answer, as a message.
            let mut text = format!("Your question was answered (decision #{}: {}).", d.id, d.title);
            if let Some(o) = option {
                text.push_str(&format!("\nAnswer: {o}"));
            }
            for (k, v) in values {
                text.push_str(&format!("\n{k}: {}", v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())));
            }
            if !note.trim().is_empty() {
                text.push_str(&format!("\nNote: {}", note.trim()));
            }
            crate::sessions::send_input(st, session, &api::SendMessage { text, ..Default::default() }).await?;
        }
        _ => {}
    }
    Ok(())
}

/// What waits on `reference` was answered elsewhere (in its session, by
/// a link, or it gave up): its open decision follows.
pub async fn settle_ref(st: &HeadState, reference: &str, state: &str, answer: Value, via: &str) {
    let open: Vec<i64> = sqlx::query_scalar("SELECT id FROM decisions WHERE ref = ? AND state IN ('open', 'snoozed')")
        .bind(reference)
        .fetch_all(&st.store.pool)
        .await
        .unwrap_or_default();
    for id in open {
        if let Err(e) = record(st, id, state, &answer, "", via).await {
            warn!(decision = id, "settling: {e:#}");
        }
    }
}

/// Opens and settles decisions for what a session's events ask and
/// answer: tool calls waiting for approval, and the assistant's questions.
pub async fn on_events(st: &HeadState, session_id: &str, events: &[SessionEvent]) {
    use session_event::Kind;
    let relevant = events.iter().any(|e| matches!(e.kind, Some(Kind::Approval(_) | Kind::Question(_))));
    if !relevant {
        return;
    }
    let Ok(Some(session)) = st.store.session(session_id).await else { return };
    for ev in events {
        match &ev.kind {
            Some(Kind::Approval(a)) => {
                let reference = format!("approval:{session_id}:{}", a.call_id);
                if a.state == "requested" {
                    let d = approval_decision(&session.title, session_id, a, ev.ts_ms);
                    if let Err(e) = open(st, &d, &session.realm, &reference).await {
                        warn!(session = %session_id, "opening an approval decision: {e:#}");
                    }
                } else {
                    let option = match a.state.as_str() {
                        "allowed" => Some("Allow"),
                        "denied" => Some("Deny"),
                        _ => None,
                    };
                    let state = if option.is_some() { "answered" } else { "expired" };
                    settle_ref(st, &reference, state, json!({"option": option}), "the session").await;
                }
            }
            Some(Kind::Question(q)) => {
                let reference = format!("question:{session_id}:{}", q.call_id);
                if q.state == "asked" {
                    let d = question_decision(&session.title, session_id, q);
                    if let Err(e) = open(st, &d, &session.realm, &reference).await {
                        warn!(session = %session_id, "opening a question decision: {e:#}");
                    }
                } else {
                    let state = if q.state == "answered" { "answered" } else { "withdrawn" };
                    settle_ref(
                        st,
                        &reference,
                        state,
                        json!({"values": serde_json::from_str::<Value>(&q.answer_json).unwrap_or(Value::Null)}),
                        "the conversation",
                    )
                    .await;
                }
            }
            _ => {}
        }
    }
}

fn approval_decision(title: &str, session_id: &str, a: &c6_proto::Approval, at: i64) -> api::DecisionIn {
    let mut document = format!("**{}** wants to run **{}**", title, a.tool);
    if !a.policy.is_empty() {
        document.push_str(&format!(" (policy: {})", a.policy));
    }
    document.push_str(".\n\n");
    if !a.message.is_empty() {
        document.push_str(&format!("{}\n\n", a.message));
    }
    document.push_str(&format!("```\n{}\n```", a.input.chars().take(4000).collect::<String>()));
    api::DecisionIn {
        kind: "approve".into(),
        title: format!("Allow {}?", a.tool),
        document,
        context: json!({"session": session_id, "call_id": a.call_id, "tool": a.tool}),
        source: format!("session:{session_id}"),
        priority: "high".into(),
        options: vec![
            api::DecisionOption { label: "Allow".into(), does: "The call runs.".into() },
            api::DecisionOption { label: "Deny".into(), does: "The agent is told you said no.".into() },
        ],
        deadline: Some(if at > 0 { at } else { now_ms() } + APPROVAL_WAIT_MS),
        default_answer: Some("Deny".into()),
        ..Default::default()
    }
}

fn question_decision(title: &str, session_id: &str, q: &c6_proto::Question) -> api::DecisionIn {
    let fields: Vec<Value> = serde_json::from_str(&q.fields_json).unwrap_or_default();
    let secret = fields.iter().any(|f| f["type"] == "secret");
    // A secret is never kept in a decision: it is answered in the conversation.
    let form: Vec<api::FormField> = if secret {
        Vec::new()
    } else {
        fields
            .iter()
            .map(|f| {
                let kind = f["type"].as_str().unwrap_or("text");
                let choices: Vec<String> = match kind {
                    "boolean" => vec!["yes".into(), "no".into()],
                    _ => f["choices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c["value"].as_str().or_else(|| c.as_str()).map(str::to_owned))
                        .collect(),
                };
                api::FormField {
                    name: f["name"].as_str().unwrap_or("").to_owned(),
                    label: f["label"].as_str().unwrap_or("").to_owned(),
                    kind: match kind {
                        "longtext" => "long".into(),
                        _ if !choices.is_empty() => "choice".into(),
                        _ => "text".into(),
                    },
                    choices,
                    required: f["required"].as_bool().unwrap_or(false),
                }
            })
            .collect()
    };
    api::DecisionIn {
        kind: if secret { "provide".into() } else { "question".into() },
        title: if q.title.is_empty() { format!("A question from {title}") } else { q.title.clone() },
        document: if secret {
            format!("**{title}** asks for something secret. Answer it in the conversation; secrets are never kept here.")
        } else {
            format!("Asked in **{title}**.")
        },
        context: json!({"session": session_id, "call_id": q.call_id}),
        source: format!("session:{session_id}"),
        priority: "high".into(),
        form,
        ..Default::default()
    }
}

/// Opens decisions for what already waits without one (after an upgrade,
/// or a head that was down): tool calls waiting for approval, and workflow
/// steps waiting at an approve step.
pub async fn reconcile(st: &HeadState) -> anyhow::Result<()> {
    let waiting: Vec<(String,)> = sqlx::query_as("SELECT id FROM sessions WHERE awaiting > 0").fetch_all(&st.store.pool).await?;
    for (session_id,) in waiting {
        let events = st.store.events_after(&session_id, 0, i64::MAX).await?;
        let mut pending: Vec<SessionEvent> = Vec::new();
        for e in events {
            if let Some(session_event::Kind::Approval(a)) = &e.kind {
                pending.retain(|p| !matches!(&p.kind, Some(session_event::Kind::Approval(x)) if x.call_id == a.call_id));
                if a.state == "requested" {
                    pending.push(e);
                }
            }
        }
        on_events(st, &session_id, &pending).await;
    }
    for (run, step, title, message, workflow, realm) in crate::workflows::all_waiting(st).await? {
        let reference = format!("workflow:{run}:{step}");
        if let Err(e) = open(st, &workflow_decision(&run, &step, &title, &message, &workflow), &realm, &reference).await {
            warn!(run = %run, "opening a workflow decision: {e:#}");
        }
    }
    Ok(())
}

/// The decision for a workflow's approve step.
pub fn workflow_decision(run: &str, step: &str, title: &str, message: &str, workflow: &str) -> api::DecisionIn {
    api::DecisionIn {
        kind: "approve".into(),
        title: if title.is_empty() { format!("Go ahead with {workflow}?") } else { title.to_owned() },
        document: format!("Workflow **{workflow}** waits for your go-ahead at step `{step}`.\n\n{message}"),
        context: json!({"workflow": workflow, "run": run, "step": step}),
        source: format!("workflow:{run}"),
        priority: "high".into(),
        options: vec![
            api::DecisionOption { label: "Go ahead".into(), does: "The workflow continues.".into() },
            api::DecisionOption { label: "Stop".into(), does: "The run ends here.".into() },
        ],
        ..Default::default()
    }
}

/// Takes the default answer of decisions past their deadline, every half
/// minute.
pub fn spawn_sweeper(st: Arc<HeadState>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            sweep(&st).await;
        }
    });
}

/// Decisions past their deadline take their default answer (else "no"),
/// and what waits on them gets it.
pub async fn sweep(st: &HeadState) {
    let due: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM decisions WHERE state IN ('open', 'snoozed') AND deadline IS NOT NULL AND deadline <= ?")
            .bind(now_ms())
            .fetch_all(&st.store.pool)
            .await
            .unwrap_or_default();
    for id in due {
        let Ok(Some(d)) = get(st, id).await else { continue };
        let reference: String =
            sqlx::query_scalar("SELECT ref FROM decisions WHERE id = ?").bind(id).fetch_one(&st.store.pool).await.unwrap_or_default();
        let option = d.default_answer.clone();
        if let Some(o) = &option
            && let Err(e) = act(st, &d, &reference, Some(o), &serde_json::Map::new(), "").await
        {
            warn!(decision = id, "taking the default answer: {e:#}");
        }
        let _ = record(st, id, "expired", &json!({"option": option}), "", "its deadline").await;
    }
}

/// Starts a session to help you settle a decision: on the worker its
/// context names, else the first one online; in its realm, with its prompt.
pub async fn start_session(st: &HeadState, id: i64) -> anyhow::Result<api::Session> {
    let d = get(st, id).await?.ok_or_else(|| not_found("no such decision"))?;
    let workers = st.store.workers().await?;
    let online: Vec<_> = workers
        .iter()
        .filter(|w| {
            w.revoked_at.is_none()
                && w.role != "memex"
                && w.role != "head"
                && w.id.parse().ok().is_some_and(|id| st.registry.get(id).is_some())
        })
        .collect();
    let wanted = d.context["worker"].as_str().unwrap_or("");
    let worker = online
        .iter()
        .find(|w| !wanted.is_empty() && (w.name == wanted || w.id == wanted))
        .or_else(|| online.first())
        .ok_or_else(|| bad_request("no worker is online to work on it"))?;
    let template = crate::prompts::get(st, "todo.session", &d.realm).await;
    let (id_text, context) = (d.id.to_string(), serde_json::to_string_pretty(&d.context).unwrap_or_default());
    let prompt = c6_api::prompts::fill(
        &template,
        &[("id", &id_text), ("kind", &d.kind), ("title", &d.title), ("detail", &d.document), ("context", &context)],
    );
    let req: api::CreateSession = serde_json::from_value(json!({
        "worker_id": worker.id, "cwd": "", "connection_id": "", "model": "", "realm": d.realm,
        "prompt": prompt, "title": format!("Decision: {}", crate::util_clip(&d.title, 60)), "labels": ["decision"],
    }))?;
    let session = crate::sessions::create(st, req).await?;
    let mut sessions = d.sessions.clone();
    sessions.push(session.id.clone());
    sqlx::query("UPDATE decisions SET sessions = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&sessions)?)
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(session)
}

/// An agent's calls, in its session's realm: raise something for later
/// (`todo`), settle what it was started for (`todo_resolve`), or ask and
/// carry on (`decision`: the answer comes back as a message).
pub async fn from_session(st: &HeadState, session_id: &str, what: &str, params: &Value) -> anyhow::Result<Value> {
    let realm = crate::realms::of_session(st, session_id).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
    match what {
        "todo" | "decision" => {
            let mut d: api::DecisionIn = serde_json::from_value(params.clone()).map_err(|e| bad_request(format!("{e}")))?;
            d.source = format!("session:{session_id}");
            if let Some(m) = params["deadline_minutes"].as_i64().filter(|m| *m > 0) {
                d.deadline = Some(now_ms() + m * 60_000);
            }
            let mut ctx = if d.context.is_object() { d.context.clone() } else { json!({}) };
            ctx["session"] = json!(session_id);
            d.context = ctx;
            if what == "decision" && !d.options.is_empty() && d.kind == "question" {
                d.kind = "choose".into();
            }
            let reference = if what == "decision" { format!("session:{session_id}:{}", uuid::Uuid::now_v7()) } else { String::new() };
            let decision = open(st, &d, &realm, &reference).await?;
            Ok(json!({"id": decision.id, "count": decision.count}))
        }
        "todo_resolve" => {
            let id = params["id"].as_i64().ok_or_else(|| bad_request("id is required"))?;
            let d = get(st, id).await?.filter(|d| d.realm == realm).ok_or_else(|| not_found("no such decision"))?;
            if d.waiting {
                return Err(bad_request("something waits on this decision; the user answers it"));
            }
            let note = params["resolution"].as_str().unwrap_or("").trim().to_owned();
            record(st, id, "answered", &json!({"option": Value::Null}), &note, &format!("session:{session_id}")).await?;
            Ok(json!({"id": id, "state": "answered"}))
        }
        other => Err(bad_request(format!("a worker may not call session.{other}"))),
    }
}

/// An agent proposes a task for a separate session, or withdraws one
/// (docs/task-cards.md): a card in its session and in Decisions.
pub async fn task_card(st: &HeadState, session_id: &str, what: &str, params: &Value) -> anyhow::Result<Value> {
    let realm = crate::realms::of_session(st, session_id).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
    let prefix = format!("task:{session_id}:");
    if what == "task_withdraw" {
        let id = params["id"].as_i64().ok_or_else(|| bad_request("id is required"))?;
        let reference: Option<String> = sqlx::query_scalar("SELECT ref FROM decisions WHERE id = ? AND state IN ('open', 'snoozed')")
            .bind(id)
            .fetch_optional(&st.store.pool)
            .await?;
        let reference =
            reference.filter(|r| r.starts_with(&prefix)).ok_or_else(|| not_found("no open task card of this session by that id"))?;
        let why = params["why"].as_str().unwrap_or("").trim().to_owned();
        settle_ref(st, &reference, "withdrawn", json!({"why": why}), &format!("session:{session_id}")).await;
        return Ok(json!({"id": id}));
    }
    let s = |k: &str| params[k].as_str().unwrap_or("").trim().to_owned();
    let (title, why, prompt) = (s("title"), s("why"), s("prompt"));
    if title.is_empty() || prompt.is_empty() {
        return Err(bad_request("a task card has a title and a prompt"));
    }
    // The same title, still open in this session: the same card.
    let same: Option<(i64, i64)> = sqlx::query_as(
        "SELECT id, count FROM decisions WHERE ref LIKE ? AND title = ? AND state IN ('open', 'snoozed') ORDER BY id LIMIT 1",
    )
    .bind(format!("{prefix}%"))
    .bind(&title)
    .fetch_optional(&st.store.pool)
    .await?;
    if let Some((id, count)) = same {
        sqlx::query("UPDATE decisions SET count = count + 1, updated_at = ? WHERE id = ?")
            .bind(now_ms())
            .bind(id)
            .execute(&st.store.pool)
            .await?;
        return Ok(json!({"id": id, "count": count + 1}));
    }
    let row = st.store.session(session_id).await?.ok_or_else(|| not_found("no such session"))?;
    // Where it worked: its folder, or the repository its worktree came from.
    let started = st.store.events_after(session_id, 0, 1).await?.into_iter().find_map(|e| match e.kind {
        Some(session_event::Kind::Started(s)) => Some(s),
        _ => None,
    });
    let in_worktree = started.as_ref().is_some_and(|s| !s.worktree_branch.is_empty());
    let folder = match started.as_ref() {
        Some(s) if in_worktree && !s.repo_root.is_empty() => s.repo_root.clone(),
        Some(s) => s.cwd.clone(),
        None => row.cwd.clone(),
    };
    let worktree = params["worktree"].as_bool().unwrap_or(in_worktree);
    let labels: Vec<String> = params["labels"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_owned)).collect();
    let d = api::DecisionIn {
        kind: "task".into(),
        title,
        document: why,
        context: json!({"session": session_id, "prompt": prompt, "worker": row.worker_id, "cwd": folder, "ssh": row.ssh_target,
            "worktree": worktree, "labels": labels}),
        source: format!("session:{session_id}"),
        priority: "low".into(),
        group: "Suggested tasks".into(),
        options: vec![
            api::DecisionOption { label: "Start it".into(), does: "A new session does it, with the prompt below.".into() },
            api::DecisionOption { label: "Not now".into(), does: "The card goes away.".into() },
        ],
        ..Default::default()
    };
    let decision = open(st, &d, &realm, &format!("{prefix}{}", uuid::Uuid::now_v7())).await?;
    Ok(json!({"id": decision.id, "count": decision.count}))
}

/// "Start it": a session that does the card's task, where its session worked.
async fn start_task(st: &HeadState, d: &api::Decision) -> anyhow::Result<()> {
    let c = &d.context;
    let s = |k: &str| c[k].as_str().unwrap_or("").to_owned();
    let workers = st.store.workers().await?;
    let online = |w: &&crate::store::WorkerRow| {
        w.revoked_at.is_none() && w.role == "worker" && w.id.parse().ok().is_some_and(|id| st.registry.get(id).is_some())
    };
    let worker = workers
        .iter()
        .filter(online)
        .find(|w| w.id == s("worker"))
        .or_else(|| workers.iter().find(online))
        .ok_or_else(|| bad_request("no worker is online to start it"))?;
    let mut labels: Vec<String> = c["labels"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_owned)).collect();
    labels.extend(["task-card".to_owned(), format!("from:{}", s("session"))]);
    let req: api::CreateSession = serde_json::from_value(json!({
        "worker_id": worker.id, "cwd": s("cwd"), "connection_id": "", "model": "", "realm": d.realm,
        "ssh": if s("ssh").is_empty() { Value::Null } else { json!(s("ssh")) },
        "worktree": c["worktree"].as_bool().unwrap_or(false),
        "prompt": s("prompt"), "title": d.title, "labels": labels,
    }))?;
    let session = crate::sessions::create(st, req).await?;
    let mut sessions = d.sessions.clone();
    sessions.push(session.id.clone());
    sqlx::query("UPDATE decisions SET sessions = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&sessions)?)
        .bind(now_ms())
        .bind(d.id)
        .execute(&st.store.pool)
        .await?;
    info!(decision = d.id, session = %session.id, "task card started");
    Ok(())
}
