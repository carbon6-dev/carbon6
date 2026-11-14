//! Session goals (docs/goals.md): what done looks like, set on a session.
//! After each turn a check (`session.goal_check`, one model call) decides
//! whether it is reached, needs you, or the session keeps going; then the
//! head sends it the next message (`session.goal_continue`). Turns, time
//! and stalls bound it, and anything that is yours (your messages, Stop,
//! an error) comes first.

use c6_api as api;
use c6_core::now_ms;
use c6_proto::session_event::Kind;
use c6_proto::{SessionEvent, TurnEndReason};
use sqlx::Row;
use tracing::{info, warn};

use crate::{HeadState, bad_request, not_found};

const DEFAULT_TURNS: u32 = 20;
const DEFAULT_HOURS: u32 = 8;
/// Checks in a row that saw nothing left change: no progress, it stops.
const MAX_STALLS: i64 = 2;
const MAX_TEXT: usize = 2_000;

fn row(r: &sqlx::sqlite::SqliteRow) -> api::SessionGoal {
    api::SessionGoal {
        text: r.get("text"),
        state: r.get("state"),
        turns: r.get::<i64, _>("turns") as u32,
        max_turns: r.get::<i64, _>("max_turns") as u32,
        deadline: r.get("deadline"),
        why: r.get("why"),
        left: r.get("left_to_do"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

pub async fn get(st: &HeadState, session: &str) -> anyhow::Result<Option<api::SessionGoal>> {
    let r = sqlx::query("SELECT * FROM goals WHERE session_id = ?").bind(session).fetch_optional(&st.store.pool).await?;
    Ok(r.as_ref().map(row))
}

/// Every session's goal, for the live session list.
pub async fn all(st: &HeadState) -> anyhow::Result<std::collections::HashMap<String, api::SessionGoal>> {
    let rows = sqlx::query("SELECT * FROM goals").fetch_all(&st.store.pool).await?;
    Ok(rows.iter().map(|r| (r.get::<String, _>("session_id"), row(r))).collect())
}

/// Sets (or replaces) a session's goal. An idle session starts toward it
/// at once; a working one is checked when its turn ends.
pub async fn set(st: &HeadState, session: &str, g: &api::GoalSet) -> anyhow::Result<api::SessionGoal> {
    let s = st.store.session(session).await?.ok_or_else(|| not_found("no such session"))?;
    if s.parent_call_id.is_some() || crate::watchers::is_watcher(&s) {
        return Err(bad_request("a goal is set on a session of yours, not a subagent or watcher"));
    }
    let text = g.text.trim();
    if text.is_empty() || text.chars().count() > MAX_TEXT {
        return Err(bad_request(format!("a goal says what done looks like, in up to {MAX_TEXT} characters")));
    }
    let max_turns = g.max_turns.unwrap_or(DEFAULT_TURNS).clamp(1, 200);
    let hours = g.max_hours.unwrap_or(DEFAULT_HOURS).clamp(1, 24 * 7);
    let now = now_ms();
    // Only what the log holds now is before the goal.
    let after = st.store.effective_events(session).await?.last().map_or(0, |e| e.seq);
    sqlx::query(
        "INSERT INTO goals (session_id, text, state, turns, max_turns, deadline, why, left_to_do, stalls, checked_seq, created_at, updated_at) \
         VALUES (?, ?, 'active', 0, ?, ?, '', '', 0, ?, ?, ?) \
         ON CONFLICT(session_id) DO UPDATE SET text = excluded.text, state = 'active', turns = 0, max_turns = excluded.max_turns, \
         deadline = excluded.deadline, why = '', left_to_do = '', stalls = 0, checked_seq = excluded.checked_seq, \
         created_at = excluded.created_at, updated_at = excluded.updated_at",
    )
    .bind(session)
    .bind(text)
    .bind(max_turns as i64)
    .bind(now + hours as i64 * 3_600_000)
    .bind(after as i64)
    .bind(now)
    .bind(now)
    .execute(&st.store.pool)
    .await?;
    st.registry.bump();
    if s.status != "running" && st.store.queued_inputs(session).await?.is_empty() {
        let template = crate::prompts::get(st, "session.goal_start", &s.realm).await;
        let message = api::prompts::fill(&template, &[("goal", text)]);
        if let Err(e) = crate::sessions::send_input(st, session, &api::SendMessage { text: message, ..Default::default() }).await {
            set_state(st, session, "needs_you", &format!("could not start toward it: {e:#}")).await?;
        } else {
            bump_turns(st, session).await?;
        }
    }
    get(st, session).await?.ok_or_else(|| anyhow::anyhow!("goal gone"))
}

/// Pause, resume (active) or stop a goal. Resuming an idle session sends
/// it on its way again.
pub async fn patch(st: &HeadState, session: &str, p: &api::GoalPatch) -> anyhow::Result<api::SessionGoal> {
    let g = get(st, session).await?.ok_or_else(|| not_found("this session has no goal"))?;
    // A reached goal stays reached: set a new one, or clear it.
    if g.state == "achieved" {
        return Err(bad_request("this goal is reached: set a new goal, or clear this one"));
    }
    match p.state.as_str() {
        "paused" | "stopped" => {
            set_state(st, session, &p.state, if p.state == "paused" { "paused by you" } else { "stopped by you" }).await?
        }
        "active" => {
            sqlx::query("UPDATE goals SET state = 'active', stalls = 0, why = '', updated_at = ? WHERE session_id = ?")
                .bind(now_ms())
                .bind(session)
                .execute(&st.store.pool)
                .await?;
            let s = st.store.session(session).await?.ok_or_else(|| not_found("no such session"))?;
            if s.status != "running" && st.store.queued_inputs(session).await?.is_empty() {
                let left = if g.left.is_empty() { "see the goal".to_owned() } else { g.left.clone() };
                send_continue(st, &s, &g, &left).await?;
            }
            st.registry.bump();
        }
        other => return Err(bad_request(format!("{other:?}: active, paused or stopped"))),
    }
    get(st, session).await?.ok_or_else(|| anyhow::anyhow!("goal gone"))
}

pub async fn clear(st: &HeadState, session: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM goals WHERE session_id = ?").bind(session).execute(&st.store.pool).await?;
    st.registry.bump();
    Ok(())
}

async fn set_state(st: &HeadState, session: &str, state: &str, why: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE goals SET state = ?, why = ?, updated_at = ? WHERE session_id = ?")
        .bind(state)
        .bind(why)
        .bind(now_ms())
        .bind(session)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(())
}

async fn bump_turns(st: &HeadState, session: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE goals SET turns = turns + 1, updated_at = ? WHERE session_id = ?")
        .bind(now_ms())
        .bind(session)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    Ok(())
}

async fn send_continue(st: &HeadState, s: &crate::store::SessionRow, g: &api::SessionGoal, left: &str) -> anyhow::Result<()> {
    let template = crate::prompts::get(st, "session.goal_continue", &s.realm).await;
    let text = api::prompts::fill(
        &template,
        &[("goal", &g.text), ("left", left), ("turn", &(g.turns + 1).to_string()), ("max", &g.max_turns.to_string())],
    );
    crate::sessions::send_input(st, &s.id, &api::SendMessage { text, ..Default::default() }).await?;
    bump_turns(st, &s.id).await
}

/// Tells you where a goal went.
fn tell(st: &HeadState, s: &crate::store::SessionRow, what: &str, why: &str, high: bool) {
    st.notifier.send(crate::notify::Note {
        event: "goal",
        title: format!("{}: {what}", s.title),
        message: why.to_owned(),
        priority: if high { "high" } else { "normal" },
        session_id: Some(s.id.clone()),
        labels: s.all_labels(),
        ..Default::default()
    });
}

/// A session's turns ending: a goal's check, in the background.
pub async fn on_events(st: &HeadState, session_id: &str, events: &[SessionEvent]) {
    let Some(end) = events.iter().rev().find(|e| matches!(e.kind, Some(Kind::TurnEnded(_)))) else { return };
    let Some(Kind::TurnEnded(t)) = &end.kind else { return };
    let (seq, reason) = (end.seq, TurnEndReason::try_from(t.reason).unwrap_or(TurnEndReason::Unspecified));
    // Claimed once per turn end: redelivery and concurrent batches check once.
    let claimed = sqlx::query("UPDATE goals SET checked_seq = ? WHERE session_id = ? AND state = 'active' AND checked_seq < ?")
        .bind(seq as i64)
        .bind(session_id)
        .bind(seq as i64)
        .execute(&st.store.pool)
        .await
        .map(|r| r.rows_affected() == 1)
        .unwrap_or(false);
    if !claimed {
        return;
    }
    let Some(arc) = st.arc() else { return };
    let session = session_id.to_owned();
    tokio::spawn(async move {
        if let Err(e) = check(&arc, &session, seq, reason).await {
            warn!(%session, "checking its goal: {e:#}");
            let _ = set_state(&arc, &session, "needs_you", &format!("the goal check failed: {e:#}")).await;
            if let Ok(Some(s)) = arc.store.session(&session).await {
                tell(&arc, &s, "goal check failed", &format!("{e:#}"), true);
            }
        }
    });
}

/// The check after a turn: reached, needs you, or keep going.
async fn check(st: &HeadState, session: &str, end_seq: u64, reason: TurnEndReason) -> anyhow::Result<()> {
    let Some(g) = get(st, session).await?.filter(|g| g.state == "active") else { return Ok(()) };
    let s = st.store.session(session).await?.ok_or_else(|| not_found("no such session"))?;
    match reason {
        // You stopped it: yours to resume.
        TurnEndReason::Interrupted => {
            set_state(st, session, "paused", "you stopped the turn; resume the goal to go on").await?;
            return Ok(());
        }
        // The worker carries a cut turn on by itself (docs/restarts.md).
        TurnEndReason::WorkerRestarted => return Ok(()),
        TurnEndReason::Error => {
            let why = "the turn ended with an error";
            set_state(st, session, "needs_you", why).await?;
            tell(st, &s, "goal needs you", why, true);
            return Ok(());
        }
        _ => {}
    }
    // Something of yours is next: it goes first, and the turn after it is
    // checked instead.
    if !st.store.queued_inputs(session).await?.is_empty() {
        return Ok(());
    }
    let events = st.store.effective_events(session).await?;
    let start =
        events.iter().rev().filter(|e| e.seq <= end_seq).find(|e| matches!(e.kind, Some(Kind::TurnStarted(_)))).map_or(0, |e| e.seq);
    let turn: Vec<SessionEvent> = events.into_iter().filter(|e| e.seq >= start && e.seq <= end_seq).collect();
    let digest = crate::watchers::digest(&turn);
    let template = crate::prompts::get(st, "session.goal_check", &s.realm).await;
    let previous = if g.left.is_empty() { "(nothing yet: this is the first check)" } else { &g.left };
    let prompt = api::prompts::fill(&template, &[("goal", &g.text), ("turn", &digest), ("previous", previous)]);
    let (connection, model) = model_for(st, &s).await?;
    let (v, _usage) = crate::watchers::ask_json(st, &s.realm, "goal check", session, &connection, &model, prompt).await?;
    let verdict = v["verdict"].as_str().unwrap_or("continue");
    let why = v["why"].as_str().unwrap_or("").trim().chars().take(500).collect::<String>();
    let left = v["left"].as_str().unwrap_or("").trim().chars().take(500).collect::<String>();
    // You may have paused or replaced it while the check ran.
    let Some(g) = get(st, session).await?.filter(|now| now.state == "active" && now.text == g.text) else { return Ok(()) };
    info!(%session, verdict, %why, "goal checked");
    match verdict {
        "achieved" => {
            set_state(st, session, "achieved", &why).await?;
            tell(st, &s, "goal reached", &why, false);
        }
        "needs_you" => {
            set_state(st, session, "needs_you", &why).await?;
            tell(st, &s, "goal needs you", &why, true);
        }
        _ => {
            let stalled = !g.left.is_empty() && same(&g.left, &left);
            let stalls: i64 = if stalled { stalls_of(st, session).await? + 1 } else { 0 };
            sqlx::query("UPDATE goals SET why = ?, left_to_do = ?, stalls = ?, updated_at = ? WHERE session_id = ?")
                .bind(&why)
                .bind(&left)
                .bind(stalls)
                .bind(now_ms())
                .bind(session)
                .execute(&st.store.pool)
                .await?;
            let out_of = if stalls >= MAX_STALLS {
                Some(format!("no progress in {MAX_STALLS} checks in a row: {left}"))
            } else if g.turns >= g.max_turns {
                Some(format!("used its {} turns; left: {left}", g.max_turns))
            } else if now_ms() >= g.deadline {
                Some(format!("ran out of time; left: {left}"))
            } else {
                None
            };
            match out_of {
                Some(why) => {
                    set_state(st, session, "stopped", &why).await?;
                    tell(st, &s, "goal stopped", &why, true);
                }
                None => {
                    let g = api::SessionGoal { left: left.clone(), ..g };
                    send_continue(st, &s, &g, if left.is_empty() { "(not said)" } else { &left }).await?;
                }
            }
        }
    }
    st.registry.bump();
    Ok(())
}

async fn stalls_of(st: &HeadState, session: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar("SELECT stalls FROM goals WHERE session_id = ?").bind(session).fetch_one(&st.store.pool).await?)
}

/// What is left, said the same way again.
fn same(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    norm(a) == norm(b)
}

/// The model that checks: a `task:goal` routing rule's, else the
/// session's own (when the head can call it), else Ask's.
async fn model_for(st: &HeadState, s: &crate::store::SessionRow) -> anyhow::Result<(String, String)> {
    let mut fallback = None;
    if !s.connection_id.is_empty() && crate::pools::cannot_answer(st, &s.connection_id).await.is_none() {
        fallback = Some((s.connection_id.clone(), s.model.clone()));
    }
    if fallback.is_none() {
        fallback = crate::assistant::model(st, &s.realm).await.ok().filter(|(c, _)| !c.is_empty());
    }
    crate::work::model_for_route(st, &s.realm, "task:goal", fallback)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no model to check the goal: add a routing rule for task:goal (Settings → Model routing)"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_same_left_said_again_is_a_stall() {
        assert!(super::same("Fix the  flaky test", "fix the flaky test"));
        assert!(!super::same("Fix the flaky test", "Push the branch"));
    }
}
