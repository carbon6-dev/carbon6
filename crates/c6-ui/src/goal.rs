//! A session's goal (docs/goals.md), above its message box: what done looks
//! like, where it stands (working toward it, paused, reached, needs you,
//! stopped), why, and Pause, Resume, Stop and Clear.

use c6_api::SessionGoal;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON_QUIET, ErrorBox};

/// Where a goal stands, in words, and its strip's frame.
fn standing(g: &SessionGoal) -> (String, &'static str) {
    let quiet = "border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900";
    match g.state.as_str() {
        "active" => (
            format!("working toward it · turn {} of {}", g.turns, g.max_turns),
            "border-sky-300 bg-sky-50 dark:border-sky-800 dark:bg-sky-950/40",
        ),
        "paused" => ("paused".into(), quiet),
        "achieved" => ("reached".into(), "border-emerald-300 bg-emerald-50 dark:border-emerald-800 dark:bg-emerald-950/40"),
        "needs_you" => ("needs you".into(), "border-amber-300 bg-amber-50 dark:border-amber-800 dark:bg-amber-950/40"),
        _ => ("stopped".into(), quiet),
    }
}

#[component]
pub fn GoalStrip(session: String) -> impl IntoView {
    let sid = StoredValue::new(session);
    let live = api::live();
    let goal = Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == sid.get_value()).and_then(|s| s.goal.clone())));
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let to = move |state: &'static str| {
        busy.set(true);
        spawn_local(async move {
            let r = if state == "clear" { api::goal_clear(sid.get_value()).await } else { api::goal_state(sid.get_value(), state).await };
            error.set(r.err());
            busy.set(false);
        });
    };
    move || {
        let g = goal.get()?;
        let (words, frame) = standing(&g);
        let state = g.state.clone();
        let detail = match state.as_str() {
            "active" if !g.left.is_empty() => format!("Left: {}", g.left),
            _ => g.why.clone(),
        };
        Some(view! {
            <div class=format!("rounded-lg border px-2 py-1.5 text-sm {frame}") data-goal="">
                <div class="flex items-center gap-2">
                    <span class="shrink-0 text-xs font-medium">"Goal"</span>
                    <span class="min-w-0 flex-1 truncate" title=g.text.clone()>{g.text.clone()}</span>
                    <span class="shrink-0 text-xs text-muted">{words}</span>
                    {(state == "active").then(|| view! {
                        <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") disabled=move || busy.get()
                            title="Finish this turn, then wait for you" on:click=move |_| to("paused")>"Pause"</button>
                        <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") disabled=move || busy.get()
                            title="Stop working toward it (this turn goes on)" on:click=move |_| to("stopped")>"Stop"</button>
                    })}
                    {(state != "active" && state != "achieved").then(|| view! {
                        <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") disabled=move || busy.get()
                            title="Go on toward it" on:click=move |_| to("active")>"Resume"</button>
                    })}
                    {(state != "active").then(|| view! {
                        <button class="text-xs text-muted hover:text-red-600" title="Clear the goal" aria-label="Clear the goal"
                            disabled=move || busy.get() on:click=move |_| to("clear")>{crate::icons::x()}</button>
                    })}
                </div>
                {(!detail.is_empty()).then(|| view! { <p class="mt-0.5 truncate text-xs text-muted" title=detail.clone()>{detail.clone()}</p> })}
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            </div>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_goal_says_where_it_stands() {
        let g = |state: &str| SessionGoal { state: state.into(), turns: 3, max_turns: 20, ..Default::default() };
        assert_eq!(standing(&g("active")).0, "working toward it · turn 3 of 20");
        assert_eq!(standing(&g("needs_you")).0, "needs you");
        assert_eq!(standing(&g("achieved")).0, "reached");
        assert_eq!(standing(&g("stopped")).0, "stopped");
    }
}
