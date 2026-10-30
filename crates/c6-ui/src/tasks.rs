//! Background task lifecycle history and read-only stream inspection.

use c6_api::{Event, EventKind};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON_QUIET, clock};
use crate::workspace::{Popover, below};

#[derive(Clone, PartialEq)]
struct Task {
    id: String,
    kind: String,
    description: String,
    since: i64,
    state: String,
    exit_code: i32,
    elapsed_ms: u64,
    notify: bool,
}

impl Task {
    fn running(&self) -> bool {
        self.state == "started"
    }

    fn can_stop(&self) -> bool {
        self.running() && self.kind != "claude_code"
    }

    fn status(&self) -> String {
        match self.state.as_str() {
            "started" => "running".into(),
            "finished" if self.kind == "claude_code" => "finished".into(),
            "finished" => format!("finished · exit {}", self.exit_code),
            "failed" => "failed".into(),
            "stopped" if self.kind == "claude_code" => "stopped (Claude Code interrupted)".into(),
            "stopped" => "stopped by the agent".into(),
            "stopped_by_user" => "stopped by you".into(),
            "lost" => "lost (worker restarted or CLI exited)".into(),
            other => other.into(),
        }
    }

    fn notification(&self) -> &'static str {
        if self.kind == "claude_code" {
            "Claude Code handles completion"
        } else if self.notify {
            "completion notifies agent"
        } else {
            "no agent completion notification"
        }
    }
}

/// Latest lifecycle record for every known task; running tasks come first.
/// No worker request is needed, including for ended or lost tasks.
fn lifecycle(events: &[Event]) -> Vec<Task> {
    let mut out: Vec<Task> = Vec::new();
    for e in events {
        if let EventKind::BackgroundTask { task_id, kind, description, state, exit_code, elapsed_ms, notify } = &e.kind {
            let since = out.iter().find(|t| &t.id == task_id).map(|t| t.since).unwrap_or(e.ts);
            out.retain(|r| &r.id != task_id);
            out.push(Task {
                id: task_id.clone(),
                kind: kind.clone(),
                description: description.clone(),
                since: if state == "started" { e.ts } else { since },
                state: state.clone(),
                exit_code: *exit_code,
                elapsed_ms: *elapsed_ms,
                notify: *notify,
            });
        }
    }
    out.sort_by_key(|t| !t.running());
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

/// Session task history, with a prominent running count and an inspector.
#[component]
pub fn RunningTasks(session: String, events: RwSignal<Vec<Event>>, #[prop(optional)] compact: bool) -> impl IntoView {
    let session = StoredValue::new(session);
    let list = Memo::new(move |_| events.with(|e| lifecycle(e)));
    let at = RwSignal::new(None::<(f64, f64)>);
    let selected = RwSignal::new(None::<String>);
    let now = clock();
    let error = RwSignal::new(None::<String>);
    view! {
        <Show when=move || !list.with(Vec::is_empty)>
                <div>
                    <button class="rounded-md border border-sky-300 bg-sky-50 px-2 py-1 text-xs text-sky-800 hover:bg-sky-100 dark:border-sky-900 dark:bg-sky-950 dark:text-sky-300"
                        title=move || list.with(|tasks| format!("Background tasks: {} running · {} total", tasks.iter().filter(|t| t.running()).count(), tasks.len()))
                        on:click=move |ev| {
                            ev.stop_propagation();
                            at.set(if at.get_untracked().is_some() { None } else { Some(below(&ev, 384.0)) });
                        }>
                        <strong>{move || list.with(|tasks| {
                            let n = tasks.iter().filter(|t| t.running()).count();
                            if compact { format!("⚙ {n}") } else { format!("⚙ {n} running") }
                        })}</strong>
                        {(!compact).then_some(move || list.with(|tasks| format!(" · {} tasks", tasks.len())))}
                    </button>
                    <Popover at class="w-96 max-w-[calc(100vw-1rem)] space-y-2 text-xs">
                            <p class="text-zinc-500">"Task history remains available when the worker is offline. Inspect streams with Refresh."</p>
                            {move || list.get().into_iter().map(|t| {
                                let id = t.id.clone();
                                let inspect_id = t.id.clone();
                                let running = t.running();
                                let can_stop = t.can_stop();
                                let status = t.status();
                                let notification = t.notification();
                                view! {
                                    <div class="flex flex-wrap items-center gap-2 border-t border-zinc-100 pt-2 dark:border-zinc-800">
                                        <span class="min-w-0 max-w-40 truncate font-mono text-zinc-500" title=t.id.clone()>{t.id.clone()}</span>
                                        <span class="min-w-0 flex-1 truncate font-mono" title=t.description.clone()>
                                            {if t.kind == "agent" { format!("agent: {}", t.description) } else { t.description.clone() }}
                                        </span>
                                        <span class="text-zinc-500">{status}</span>
                                        <span class="text-zinc-500">{notification}</span>
                                        <span class="text-zinc-500">{move || if running { elapsed(now.get() - t.since) } else if t.state == "lost" { "elapsed unknown".into() } else { elapsed(t.elapsed_ms as i64) }}</span>
                                        <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs") on:click=move |_| {
                                            selected.set(Some(inspect_id.clone()));
                                            at.set(None);
                                        }>"Inspect"</button>
                                        {can_stop.then(|| view! { <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs text-red-600") on:click=move |_| {
                                            let id = id.clone();
                                            spawn_local(async move {
                                                if let Err(e) = api::stop_task(session.get_value(), id).await {
                                                    error.set(Some(e));
                                                }
                                            });
                                        }>"Stop"</button> })}
                                    </div>
                                }
                            }).collect_view()}
                            {move || error.get().map(|e| view! { <p class="text-red-600">{e}</p> })}
                    </Popover>
                    {move || selected.get().map(|id| {
                        let task = Memo::new(move |_| list.with(|tasks| tasks.iter().find(|t| t.id == id).cloned()));
                        view! { <TaskInspector session=session.get_value() task on_close=Callback::new(move |_| selected.set(None)) /> }
                    })}
                </div>
        </Show>
    }
}

/// Mounted only while open. The resource tracks lifecycle, never the clock or
/// model output, so ordinary session deltas do not cause snapshot polling.
#[component]
fn TaskInspector(session: String, task: Memo<Option<Task>>, on_close: Callback<()>) -> impl IntoView {
    let dialog = NodeRef::<leptos::html::Div>::new();
    Effect::new(move || {
        if let Some(el) = dialog.get() {
            let _ = el.focus();
        }
    });
    let snapshot = LocalResource::new(move || {
        let task = task.get();
        let session = session.clone();
        async move {
            let task = task.ok_or_else(|| "This task is no longer in the session history.".to_owned())?;
            api::task_snapshot(session, task.id).await
        }
    });
    view! {
        <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-2 sm:p-4"
            on:click=move |_| on_close.run(()) on:keydown=move |ev| if ev.key() == "Escape" {
                ev.prevent_default();
                ev.stop_propagation();
                on_close.run(());
            }>
            <div node_ref=dialog role="dialog" aria-modal="true" aria-label="Background task inspector" tabindex="-1"
                class="flex max-h-[90vh] w-full max-w-4xl flex-col gap-3 overflow-auto rounded-lg bg-white p-3 text-sm shadow-xl sm:p-4 dark:bg-zinc-900"
                on:click=|ev| ev.stop_propagation()>
                <div class="flex flex-wrap items-center gap-2">
                    <h2 class="min-w-0 flex-1 break-all font-medium">{move || task.get().map(|t| format!("Task {} · {}", t.id, t.kind))}</h2>
                    <button class=BUTTON_QUIET on:click=move |_| snapshot.refetch()>"Refresh"</button>
                    <button class=BUTTON_QUIET on:click=move |_| on_close.run(())>"Close"</button>
                </div>
                <p class="text-xs text-zinc-500">"Read-only live snapshot. Use Refresh to see new output; there is no automatic polling. Inspecting does not wake the model, consume output or change its read cursor."</p>
                {move || task.get().map(|t| view! {
                    <p class="text-xs text-zinc-500">{format!("Session history: {} · {}", t.status(), t.notification())}</p>
                    <pre class="whitespace-pre-wrap break-all font-mono text-xs">{t.description}</pre>
                })}
                <Suspense fallback=|| view! { <p role="status">"Loading task snapshot…"</p> }>
                    {move || snapshot.get().map(|result| match result {
                        Err(e) => view! {
                            <p role="alert" class="break-words text-red-600 dark:text-red-400">{format!("Snapshot unavailable (the worker may be offline): {e}. Task history above is still available. Try Refresh after reconnecting.")}</p>
                        }.into_any(),
                        Ok(s) => view! {
                            <div class="space-y-3">
                                <p>{format!("{} · elapsed {} · exit {}", if s.running { "running" } else { "not running" }, elapsed(s.elapsed_ms.min(i64::MAX as u64) as i64), s.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unavailable".into()))}</p>
                                <div>
                                    <h3 class="font-medium">"Command"</h3>
                                    <pre class="whitespace-pre-wrap break-all font-mono text-xs">{s.command}</pre>
                                </div>
                                <TaskStream label="stdin (read-only)" stream=s.stdin />
                                <TaskStream label="stdout" stream=s.stdout />
                                <TaskStream label="stderr" stream=s.stderr />
                                {s.combined.map(|stream| view! { <TaskStream label="Combined Claude Code output" stream /> })}
                            </div>
                        }.into_any(),
                    })}
                </Suspense>
            </div>
        </div>
    }
}

#[component]
fn TaskStream(label: &'static str, stream: c6_api::TaskStream) -> impl IntoView {
    view! {
        <section class="min-w-0 space-y-1">
            <h3 class="font-medium">{label}</h3>
            {(!stream.note.is_empty()).then(|| view! { <p class="break-words text-xs text-amber-700 dark:text-amber-400">{stream.note}</p> })}
            {(stream.dropped_bytes > 0).then(|| view! { <p class="text-xs text-amber-700 dark:text-amber-400">{format!("Truncated: {} earlier bytes dropped.", stream.dropped_bytes)}</p> })}
            <pre class="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded border border-zinc-200 bg-zinc-50 p-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-950">{if stream.text.is_empty() { "(no captured data)".into() } else { stream.text }}</pre>
        </section>
    }
}

/// A background task's end, as a line in the transcript.
pub fn ended_line(task_id: &str, kind: &str, description: &str, state: &str, exit_code: i32, elapsed_ms: u64) -> Option<AnyView> {
    let agent = kind == "agent";
    let native = kind == "claude_code";
    let (icon, what, tone) = match state {
        "started" => return None,
        "finished" if native => ("✓", "finished".to_owned(), "text-zinc-500"),
        "finished" if agent && exit_code == 0 => ("✓", "finished".to_owned(), "text-zinc-500"),
        "finished" if agent => ("✗", "failed".to_owned(), "text-red-600 dark:text-red-400"),
        "finished" if exit_code == 0 => ("✓", "exited 0".to_owned(), "text-zinc-500"),
        "finished" => ("✗", format!("exited {exit_code}"), "text-red-600 dark:text-red-400"),
        "failed" => ("✗", "failed".to_owned(), "text-red-600 dark:text-red-400"),
        "stopped" if native => ("■", "stopped (Claude Code interrupted)".to_owned(), "text-zinc-500"),
        "stopped" => ("■", "stopped by the agent".to_owned(), "text-zinc-500"),
        "stopped_by_user" => ("■", "stopped by you".to_owned(), "text-zinc-500"),
        "lost" => ("!", "lost (worker restarted or CLI exited)".to_owned(), "text-amber-700 dark:text-amber-400"),
        other => ("·", other.to_owned(), "text-zinc-500"),
    };
    let label = if agent { format!("agent: {description}") } else { description.to_owned() };
    let time = if state == "lost" { String::new() } else { format!(" · {}", elapsed(elapsed_ms as i64)) };
    let title = label.clone();
    Some(
        view! {
            <p class=format!("flex items-center gap-2 text-xs {tone}")>
                <span>"⚙"</span>
                <span class="max-w-24 shrink-0 truncate font-mono" title=task_id.to_owned()>{task_id.to_owned()}</span>
                <span class="min-w-0 truncate font-mono" title=title>{label}</span>
                <span class="shrink-0">{format!("{icon} {what}{time}")}</span>
            </p>
        }
        .into_any(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: u64, task: &str, state: &str) -> Event {
        Event {
            seq,
            ts: seq as i64 * 1000,
            kind: EventKind::BackgroundTask {
                task_id: task.into(),
                kind: "shell".into(),
                description: format!("command {task}"),
                state: state.into(),
                exit_code: if state == "finished" { 7 } else { 0 },
                elapsed_ms: 2500,
                notify: false,
            },
        }
    }

    #[test]
    fn lifecycle_retains_completed_stopped_and_lost_tasks() {
        let tasks = lifecycle(&[
            event(1, "done", "started"),
            event(2, "lost", "started"),
            event(3, "stopped", "started"),
            event(4, "done", "finished"),
            event(5, "lost", "lost"),
            event(6, "stopped", "stopped_by_user"),
            event(7, "agent_stop", "stopped"),
        ]);
        assert_eq!(tasks.len(), 4);
        assert!(tasks.iter().all(|t| !t.running()));
        assert_eq!(tasks[0].state, "finished");
        assert_eq!(tasks[0].exit_code, 7);
        assert_eq!(tasks[0].elapsed_ms, 2500);
        assert_eq!(tasks[0].since, 1000);
        assert_eq!(tasks[1].state, "lost");
        assert_eq!(tasks[1].since, 2000);
        assert_eq!(tasks[2].state, "stopped_by_user");
        assert_eq!(tasks[3].state, "stopped");
    }

    #[test]
    fn lifecycle_orders_running_first_without_losing_latest_record() {
        let mut events = vec![
            event(1, "ended", "finished"),
            event(2, "running_a", "started"),
            event(3, "lost", "lost"),
            event(4, "running_b", "started"),
        ];
        let tasks = lifecycle(&events);
        assert_eq!(tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["running_a", "running_b", "ended", "lost"]);
        events.push(event(5, "running_a", "finished"));
        let tasks = lifecycle(&events);
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[0].id, "running_b");
        assert_eq!(tasks.iter().filter(|t| t.running()).count(), 1);
        assert_eq!(tasks[3].id, "running_a");
        assert_eq!(tasks[3].state, "finished");
        assert_eq!(tasks[3].since, 2000);
    }

    #[test]
    fn lifecycle_ignores_unrelated_events_and_handles_empty_history() {
        assert!(lifecycle(&[]).is_empty());
        let events = vec![
            event(1, "task", "started"),
            Event { seq: 2, ts: 2000, kind: EventKind::Rewound { to_seq: 0, restored_files: false, restore_error: String::new() } },
        ];
        let tasks = lifecycle(&events);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].running());
        assert_eq!(tasks[0].description, "command task");
        assert_eq!(tasks[0].kind, "shell");
    }

    #[test]
    fn lifecycle_keeps_notification_policy() {
        let mut notified = event(1, "notified", "started");
        if let EventKind::BackgroundTask { notify, .. } = &mut notified.kind {
            *notify = true;
        }
        let tasks = lifecycle(&[notified, event(2, "watcher", "started")]);
        assert!(tasks[0].notify);
        assert_eq!(tasks[0].notification(), "completion notifies agent");
        assert!(!tasks[1].notify);
        assert_eq!(tasks[1].notification(), "no agent completion notification");
    }

    #[test]
    fn native_claude_code_handles_completion_and_cannot_be_stopped() {
        let mut native = event(1, "native", "started");
        if let EventKind::BackgroundTask { kind, .. } = &mut native.kind {
            *kind = "claude_code".into();
        }
        let tasks = lifecycle(&[native, event(2, "shell", "started"), event(3, "done", "finished")]);
        assert!(tasks[0].running());
        assert!(!tasks[0].can_stop());
        assert_eq!(tasks[0].notification(), "Claude Code handles completion");
        assert!(tasks[1].can_stop());
        assert!(!tasks[2].can_stop());
    }

    #[test]
    fn native_outcomes_do_not_claim_an_os_exit_code() {
        for (state, expected) in [
            ("finished", "finished"),
            ("failed", "failed"),
            ("stopped", "stopped (Claude Code interrupted)"),
            ("lost", "lost (worker restarted or CLI exited)"),
        ] {
            let mut native = event(1, "native", state);
            if let EventKind::BackgroundTask { kind, exit_code, .. } = &mut native.kind {
                *kind = "claude_code".into();
                *exit_code = 0;
            }
            let tasks = lifecycle(&[native]);
            assert_eq!(tasks[0].status(), expected);
            assert!(!tasks[0].running());
            assert!(!tasks[0].can_stop());
        }
        assert_eq!(lifecycle(&[event(1, "shell", "finished")])[0].status(), "finished · exit 7");
    }
}
