//! Background tasks in a session: the running ones (with a stop button) and
//! how ended ones show in the transcript.

use c6_api::{Event, EventKind};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON_QUIET, clock};

#[derive(Clone, PartialEq)]
struct Running {
    id: String,
    kind: String,
    description: String,
    since: i64,
}

/// Tasks started and not (yet) ended, from the session's events.
fn running(events: &[Event]) -> Vec<Running> {
    let mut out: Vec<Running> = Vec::new();
    for e in events {
        if let EventKind::BackgroundTask { task_id, kind, description, state, .. } = &e.kind {
            out.retain(|r| &r.id != task_id);
            if state == "started" {
                out.push(Running { id: task_id.clone(), kind: kind.clone(), description: description.clone(), since: e.ts });
            }
        }
    }
    out
}

pub fn elapsed(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s >= 3600 {
        format!("{}h {}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// "⚙ 2 running": the session's running background tasks, each with how
/// long it has run and a stop button.
#[component]
pub fn RunningTasks(session: String, events: RwSignal<Vec<Event>>) -> impl IntoView {
    let session = StoredValue::new(session);
    let list = Memo::new(move |_| events.with(|e| running(e)));
    let open = RwSignal::new(false);
    let now = clock();
    let error = RwSignal::new(None::<String>);
    move || {
        let tasks = list.get();
        (!tasks.is_empty()).then(|| {
            let n = tasks.len();
            view! {
                <div class="relative">
                    <button class="rounded-md border border-sky-300 bg-sky-50 px-2 py-1 text-xs text-sky-800 hover:bg-sky-100 dark:border-sky-900 dark:bg-sky-950 dark:text-sky-300"
                        title="Background tasks" on:click=move |_| open.update(|o| *o = !*o)>
                        {format!("⚙ {n} running")}
                    </button>
                    {move || open.get().then(|| view! {
                        <div class="absolute right-0 z-30 mt-1 w-96 max-w-[90vw] space-y-1 rounded-md border border-zinc-200 bg-white p-2 text-xs shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
                            <p class="text-zinc-500">"The agent is told when each one ends."</p>
                            {list.get().into_iter().map(|t| {
                                let id = t.id.clone();
                                view! {
                                    <div class="flex items-center gap-2">
                                        <span class="font-mono text-zinc-500">{t.id.clone()}</span>
                                        <span class="min-w-0 flex-1 truncate font-mono" title=t.description.clone()>
                                            {if t.kind == "agent" { format!("agent: {}", t.description) } else { t.description.clone() }}
                                        </span>
                                        <span class="text-zinc-500">{move || elapsed(now.get() - t.since)}</span>
                                        <button class=format!("{BUTTON_QUIET} px-1.5 py-0.5 text-xs text-red-600") on:click=move |_| {
                                            let id = id.clone();
                                            spawn_local(async move {
                                                if let Err(e) = api::stop_task(session.get_value(), id).await {
                                                    error.set(Some(e));
                                                }
                                            });
                                        }>"Stop"</button>
                                    </div>
                                }
                            }).collect_view()}
                            {move || error.get().map(|e| view! { <p class="text-red-600">{e}</p> })}
                        </div>
                    })}
                </div>
            }
        })
    }
}

/// A background task's end, as a line in the transcript.
pub fn ended_line(task_id: &str, kind: &str, description: &str, state: &str, exit_code: i32, elapsed_ms: u64) -> Option<AnyView> {
    let agent = kind == "agent";
    let (icon, what, tone) = match state {
        "started" => return None,
        "finished" if agent && exit_code == 0 => ("✓", "finished".to_owned(), "text-zinc-500"),
        "finished" if agent => ("✗", "failed".to_owned(), "text-red-600 dark:text-red-400"),
        "finished" if exit_code == 0 => ("✓", "exited 0".to_owned(), "text-zinc-500"),
        "finished" => ("✗", format!("exited {exit_code}"), "text-red-600 dark:text-red-400"),
        "stopped" => ("■", "stopped by the agent".to_owned(), "text-zinc-500"),
        "stopped_by_user" => ("■", "stopped by you".to_owned(), "text-zinc-500"),
        "lost" => ("!", "lost (the worker restarted)".to_owned(), "text-amber-700 dark:text-amber-400"),
        other => ("·", other.to_owned(), "text-zinc-500"),
    };
    let label = if agent { format!("agent: {description}") } else { description.to_owned() };
    let time = if state == "lost" { String::new() } else { format!(" · {}", elapsed(elapsed_ms as i64)) };
    let title = label.clone();
    Some(
        view! {
            <p class=format!("flex items-center gap-2 text-xs {tone}")>
                <span>"⚙"</span>
                <span class="font-mono">{task_id.to_owned()}</span>
                <span class="min-w-0 truncate font-mono" title=title>{label}</span>
                <span class="shrink-0">{format!("{icon} {what}{time}")}</span>
            </p>
        }
        .into_any(),
    )
}
