//! Background task lifecycle history and read-only stream inspection.

use c6_api::{Event, EventKind, Session, SessionStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::icons::{Icon, Size, icon};
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
    /// Claude Code's kind for its own task (`local_agent`, ...), and whether
    /// it holds the turn.
    native_type: String,
    foreground: bool,
}

impl Task {
    fn running(&self) -> bool {
        self.state == "started"
    }

    fn can_stop(&self) -> bool {
        self.running()
    }

    /// Stopped: by you, the agent or Claude Code.
    fn stopped(&self) -> bool {
        self.state.starts_with("stopped")
    }

    /// It ended badly: failed, lost, or a command that exited non-zero.
    fn failed(&self) -> bool {
        match self.state.as_str() {
            "failed" | "lost" => true,
            "finished" => self.kind != "claude_code" && self.exit_code != 0,
            _ => false,
        }
    }

    fn status(&self) -> String {
        match self.state.as_str() {
            "started" => "running".into(),
            "finished" if self.kind == "claude_code" => "finished".into(),
            "finished" => format!("finished · exit {}", self.exit_code),
            "failed" => "failed".into(),
            "stopped" if self.kind == "claude_code" => "stopped by Claude Code".into(),
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

/// A task's state as a dot: running, finished, failed, stopped or lost.
fn state_dot(state: &str, running: bool) -> &'static str {
    match state {
        _ if running => "animate-pulse bg-blue-500",
        "finished" => "bg-emerald-500",
        "failed" => "bg-red-500",
        "lost" => "bg-amber-500",
        _ => "bg-zinc-400",
    }
}

/// What kind of task, in words.
fn kind_name(kind: &str) -> &'static str {
    match kind {
        "claude_code" => "Claude Code",
        "agent" => "agent",
        _ => "shell",
    }
}

/// Latest lifecycle record for every known task; running tasks come first.
/// No worker request is needed, including for ended or lost tasks.
fn lifecycle(events: &[Event]) -> Vec<Task> {
    let mut out: Vec<Task> = Vec::new();
    for e in events {
        if let EventKind::BackgroundTask { task_id, kind, description, state, exit_code, elapsed_ms, notify, native_type, foreground } =
            &e.kind
        {
            let before = out.iter().find(|t| &t.id == task_id);
            let since = before.map(|t| t.since).unwrap_or(e.ts);
            // Started again: a new run, unless it was running (Claude Code
            // moved it to the background).
            let since = if state == "started" && !before.is_some_and(Task::running) { e.ts } else { since };
            out.retain(|r| &r.id != task_id);
            out.push(Task {
                id: task_id.clone(),
                kind: kind.clone(),
                description: description.clone(),
                since,
                state: state.clone(),
                exit_code: *exit_code,
                elapsed_ms: *elapsed_ms,
                notify: *notify,
                native_type: native_type.clone(),
                foreground: *foreground,
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

/// What's running now: the tasks that keep the session showing as running
/// (those that tell the agent when they end, and Claude Code's own), and
/// the servers and watchers that don't.
pub fn running_counts(events: &[Event]) -> (usize, usize) {
    lifecycle(events)
        .iter()
        .filter(|t| t.running())
        .fold((0, 0), |(tasks, servers), t| if t.notify || t.kind == "claude_code" { (tasks + 1, servers) } else { (tasks, servers + 1) })
}

/// Distinct jobs actually running in the background, including servers.
/// A carbon6 background subagent also has a task record: count it once.
/// Claude's read-only mirrors are represented by its native task records,
/// whose foreground flag tells us whether they really run in the background.
pub(crate) fn background_count(events: &[Event], subagents: &[Session]) -> usize {
    let children = subagents.iter().filter(|s| s.status == SessionStatus::Running && !s.observed).collect::<Vec<_>>();
    let tasks = lifecycle(events)
        .into_iter()
        .filter(|t| t.running() && !t.foreground)
        .filter(|t| {
            !events.iter().any(|e| match &e.kind {
                EventKind::SubagentSpawned { child_session_id, background_task_id, .. } => {
                    background_task_id == &t.id && children.iter().any(|s| &s.id == child_session_id)
                }
                _ => false,
            })
        })
        .count();
    children.len() + tasks
}

/// The words beside the running-task spinner at the bottom of a transcript.
pub fn running_line(tasks: usize, servers: usize) -> String {
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    match (tasks, servers) {
        (0, s) => format!("{} running", plural(s, "server", "servers")),
        (t, 0) => format!("{} running", plural(t, "background task", "background tasks")),
        (t, s) => format!("{} running · {}", plural(t, "task", "tasks"), plural(s, "server", "servers")),
    }
}

/// Claude Code's own agents running in the background (`local_agent`, not
/// holding the turn): a Stop stops them too (docs/background.md §4).
pub fn background_agents(events: &[Event]) -> usize {
    lifecycle(events).iter().filter(|t| t.running() && t.kind == "claude_code" && t.native_type == "local_agent" && !t.foreground).count()
}

/// What the tasks running now are, servers and watchers too.
pub fn running(events: &[Event]) -> Vec<String> {
    lifecycle(events).into_iter().filter(Task::running).map(|t| t.description).collect()
}

/// The last few tasks that failed or were stopped, beside those running.
const RECENT: usize = 3;

/// The tasks a session's ◌ N lists: those running, then the last few that
/// failed or were stopped, newest first; `all`: every one.
fn short_list(tasks: Vec<Task>, all: bool) -> Vec<Task> {
    if all {
        return tasks;
    }
    let (running, ended): (Vec<Task>, Vec<Task>) = tasks.into_iter().partition(Task::running);
    let recent = ended.into_iter().rev().filter(|t| t.failed() || t.stopped()).take(RECENT);
    running.into_iter().chain(recent).collect()
}

/// How many tasks the short list leaves out.
pub fn left_out(events: &[Event]) -> usize {
    let tasks = lifecycle(events);
    let total = tasks.len();
    total - short_list(tasks, false).len()
}

/// A session's tasks for its bar's ◌ N (see [`short_list`]), each with its
/// state and time, Inspect, and Stop while it runs. Nothing when there are
/// none to show.
#[component]
pub fn BackgroundTasks(session: String, events: RwSignal<Vec<Event>>, #[prop(into)] all: Signal<bool>) -> impl IntoView {
    let session = StoredValue::new(session);
    let list = Memo::new(move |_| short_list(events.with(|e| lifecycle(e)), all.get()));
    let selected = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    view! {
        <Show when=move || !list.with(Vec::is_empty)>
            <div class="space-y-2">
                <p class="text-[10px] font-medium uppercase tracking-wide text-muted">"Tasks"</p>
                {task_rows(session, list, error, Callback::new(move |id| selected.set(Some(id))))}
            </div>
        </Show>
        {inspector(session, list, selected)}
    }
}

/// A quiet line for the bottom of the transcript, shown only while
/// something runs (docs/background.md §4): it opens the session's tasks.
#[component]
pub fn RunningTasks(session: String, events: RwSignal<Vec<Event>>) -> impl IntoView {
    let session = StoredValue::new(session);
    let list = Memo::new(move |_| events.with(|e| lifecycle(e)));
    let counts = Memo::new(move |_| events.with(|e| running_counts(e)));
    let at = RwSignal::new(None::<(f64, f64)>);
    let selected = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    view! {
        <Show when=move || counts.with(|(t, s)| t + s > 0)>
            <div>
                <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" title="Open the background tasks"
                    on:click=move |ev| {
                        ev.stop_propagation();
                        at.set(if at.get_untracked().is_some() { None } else { Some(below(&ev, 384.0)) });
                    }>
                    <span class="inline-flex animate-spin">{icon(Icon::LoaderCircle, Size::S14)}</span>" "{move || counts.with(|(t, s)| running_line(*t, *s))}" "{crate::icons::chevron_right()}
                </button>
                <Popover at class="w-96 max-w-[calc(100vw-1rem)] space-y-2 text-xs">
                    <p class="text-muted">"Task history remains available when the worker is offline. Inspect streams with Refresh."</p>
                    {task_rows(session, list, error, Callback::new(move |id| {
                        selected.set(Some(id));
                        at.set(None);
                    }))}
                </Popover>
                {inspector(session, list, selected)}
            </div>
        </Show>
    }
}

/// The tasks, each with what it is, how it stands, Inspect and (while it
/// runs) Stop.
fn task_rows(
    session: StoredValue<String>,
    list: Memo<Vec<Task>>,
    error: RwSignal<Option<String>>,
    inspect: Callback<String>,
) -> impl IntoView {
    let now = clock();
    view! {
        {move || list.get().into_iter().map(|t| {
            let id = t.id.clone();
            let inspect_id = t.id.clone();
            let running = t.running();
            let can_stop = t.can_stop();
            let status = t.status();
            let notification = t.notification();
            // What it is first, in words; its id and how its
            // end is reported on hover.
            let about = format!("{} · {notification}", t.id);
            view! {
                <div class="space-y-1 border-t border-zinc-100 pt-2 dark:border-zinc-800">
                    <div class="flex items-start gap-2" title=about>
                        <span class=format!("mt-1 h-2 w-2 shrink-0 rounded-full {}", state_dot(&t.state, running))></span>
                        <span class="line-clamp-2 min-w-0 flex-1 text-[13px] text-zinc-800 dark:text-zinc-100">{t.description.clone()}</span>
                    </div>
                    <div class="flex flex-wrap items-center gap-2 pl-4 text-zinc-600 dark:text-zinc-400">
                        <span class="rounded bg-zinc-100 px-1 text-[10px] dark:bg-zinc-800">{kind_name(&t.kind)}</span>
                        <span>{status}</span>
                        <span>{move || if running { elapsed(now.get() - t.since) } else if t.state == "lost" { "elapsed unknown".into() } else { elapsed(t.elapsed_ms as i64) }}</span>
                        <span class="flex-1"></span>
                        <button class="rounded-md border border-zinc-200 px-2 py-0.5 text-zinc-700 hover:bg-zinc-100 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800" on:click=move |_| {
                            inspect.run(inspect_id.clone());
                        }>"Inspect"</button>
                        {can_stop.then(|| view! { <button class="rounded-md border border-red-200 px-2 py-0.5 text-red-700 hover:bg-red-50 dark:border-red-900 dark:text-red-400 dark:hover:bg-red-950" on:click=move |_| {
                        let id = id.clone();
                        spawn_local(async move {
                            if let Err(e) = api::stop_task(session.get_value(), id).await {
                                error.set(Some(e));
                            }
                        });
                    }>"Stop"</button> })}
                    </div>
                </div>
            }
        }).collect_view()}
        {move || error.get().map(|e| view! { <p class="text-red-600">{e}</p> })}
    }
}

/// The inspector of the task chosen, while one is.
fn inspector(session: StoredValue<String>, list: Memo<Vec<Task>>, selected: RwSignal<Option<String>>) -> impl IntoView {
    move || {
        selected.get().map(|id| {
            let task = Memo::new(move |_| list.with(|tasks| tasks.iter().find(|t| t.id == id).cloned()));
            view! {
                <leptos::portal::Portal>
                    <TaskInspector session=session.get_value() task on_close=Callback::new(move |_| selected.set(None)) />
                </leptos::portal::Portal>
            }
        })
    }
}

/// Mounted only while open. The resource tracks lifecycle, never the clock or
/// model output, so ordinary session deltas do not cause snapshot polling.
#[component]
fn TaskInspector(session: String, task: Memo<Option<Task>>, on_close: Callback<()>) -> impl IntoView {
    let dialog = NodeRef::<leptos::html::Div>::new();
    let pop = crate::util::escapable(|| true, move || on_close.run(()));
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
        <div class="phone-dialog fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-2 sm:p-4"
            on:click=move |_| on_close.run(())>
            <div node_ref=dialog role="dialog" aria-modal="true" aria-label="Background task inspector" data-pop=pop tabindex="-1"
                class="flex max-h-[90vh] w-full max-w-4xl flex-col gap-3 overflow-auto rounded-lg bg-white p-3 text-sm shadow-xl sm:p-4 dark:bg-zinc-900"
                on:click=|ev| ev.stop_propagation()>
                <div class="flex flex-wrap items-center gap-2">
                    <h2 class="min-w-0 flex-1 break-all font-medium">{move || task.get().map(|t| format!("Task {} · {}", t.id, t.kind))}</h2>
                    <button class=BUTTON_QUIET on:click=move |_| snapshot.refetch()>"Refresh"</button>
                    <button class=BUTTON_QUIET on:click=move |_| on_close.run(())>"Close"</button>
                </div>
                <p class="text-xs text-muted">"Read-only live snapshot. Use Refresh to see new output; there is no automatic polling. Inspecting does not wake the model, consume output or change its read cursor."</p>
                {move || task.get().map(|t| view! {
                    <p class="text-xs text-muted">{format!("Session history: {} · {}", t.status(), t.notification())}</p>
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
    let status_icon = match state {
        "finished" if native || exit_code == 0 => crate::icons::check().into_any(),
        "finished" | "failed" => icon(Icon::CircleX, Size::S14).into_any(),
        "stopped" | "stopped_by_user" => crate::icons::stop().into_any(),
        "lost" => crate::icons::warning().into_any(),
        _ => crate::icons::circle().into_any(),
    };
    let (what, tone) = match state {
        "started" => return None,
        "finished" if native => ("finished".to_owned(), "text-muted"),
        "finished" if agent && exit_code == 0 => ("finished".to_owned(), "text-muted"),
        "finished" if agent => ("failed".to_owned(), "text-red-600 dark:text-red-400"),
        "finished" if exit_code == 0 => ("exited 0".to_owned(), "text-muted"),
        "finished" => (format!("exited {exit_code}"), "text-red-600 dark:text-red-400"),
        "failed" => ("failed".to_owned(), "text-red-600 dark:text-red-400"),
        "stopped" if native => ("stopped by Claude Code".to_owned(), "text-muted"),
        "stopped" => ("stopped by the agent".to_owned(), "text-muted"),
        "stopped_by_user" => ("stopped by you".to_owned(), "text-muted"),
        "lost" => ("lost (worker restarted or CLI exited)".to_owned(), "text-amber-700 dark:text-amber-400"),
        other => (other.to_owned(), "text-muted"),
    };
    let label = if agent { format!("agent: {description}") } else { description.to_owned() };
    let time = if state == "lost" { String::new() } else { format!(" · {}", elapsed(elapsed_ms as i64)) };
    let title = label.clone();
    Some(
        view! {
            <p class=format!("flex items-center gap-2 text-xs {tone}")>
                <span>{crate::icons::square_terminal()}</span>
                <span class="min-w-0 truncate" title=format!("{title} · {task_id}")>{label}</span>
                <span class="shrink-0">{status_icon}" "{format!("{what}{time}")}</span>
            </p>
        }
        .into_any(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn task_endings_keep_both_the_task_and_status_icons() {
        for (kind, state, exit_code, text) in [
            ("claude_code", "finished", 1, "finished"),
            ("agent", "finished", 0, "finished"),
            ("agent", "finished", 1, "failed"),
            ("shell", "finished", 0, "exited 0"),
            ("shell", "finished", 1, "exited 1"),
            ("shell", "failed", 1, "failed"),
            ("claude_code", "stopped", 0, "stopped by Claude Code"),
            ("shell", "stopped", 0, "stopped by the agent"),
            ("shell", "stopped_by_user", 0, "stopped by you"),
            ("shell", "lost", 0, "lost (worker restarted or CLI exited)"),
            ("shell", "unknown", 0, "unknown"),
        ] {
            let html = ended_line("t1", kind, "Build", state, exit_code, 1000).unwrap().to_html();
            assert_eq!(html.matches("<svg").count(), 2, "keep both icons for {kind}/{state}: {html}");
            assert!(html.contains(text), "{html}");
            if state == "failed" || (state == "finished" && kind != "claude_code" && exit_code != 0) {
                assert!(html.contains(&icon(Icon::CircleX, Size::S14).to_html()), "failed tasks keep the error icon: {html}");
            }
            crate::icons::tests::assert_no_icon_glyphs(&html);
        }
        assert!(ended_line("t1", "shell", "Build", "started", 0, 0).is_none());
    }

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
                native_type: String::new(),
                foreground: false,
            },
        }
    }

    /// One of Claude Code's own tasks: its kind, and whether it holds the turn.
    fn native(seq: u64, task: &str, state: &str, native: &str, fg: bool) -> Event {
        let mut e = event(seq, task, state);
        if let EventKind::BackgroundTask { kind, native_type, foreground, .. } = &mut e.kind {
            *kind = "claude_code".into();
            *native_type = native.into();
            *foreground = fg;
        }
        e
    }

    #[test]
    fn background_agents_are_claude_codes_running_agents_not_in_the_turn() {
        let evs = [
            native(1, "bg-agent", "started", "local_agent", false),
            native(2, "fg-agent", "started", "local_agent", true),
            native(3, "command", "started", "local_bash", false),
            native(4, "done-agent", "started", "local_agent", false),
            native(5, "done-agent", "finished", "local_agent", false),
            event(6, "shell", "started"),
        ];
        assert_eq!(background_agents(&evs), 1);
        // The CLI moved the foreground one to the background: it counts, and
        // it has run since it started.
        let mut moved = evs.to_vec();
        moved.push(native(7, "fg-agent", "started", "local_agent", false));
        assert_eq!(background_agents(&moved), 2);
        let task = lifecycle(&moved).into_iter().find(|t| t.id == "fg-agent").unwrap();
        assert_eq!(task.since, 2000);
    }

    #[test]
    fn phone_background_count_is_positive_only_for_current_distinct_background_jobs() {
        let mut events = vec![
            native(1, "foreground", "started", "local_agent", true),
            native(2, "native", "started", "local_agent", false),
            event(3, "server", "started"),
            event(4, "done", "started"),
            event(5, "done", "finished"),
        ];
        assert_eq!(background_count(&[], &[]), 0);
        assert_eq!(background_count(&events, &[]), 2, "neither foreground nor completed tasks count");
        let child = |id: &str, status: &str, observed: bool| {
            serde_json::from_value::<Session>(serde_json::json!({
                "id": id, "worker_id": "00000000-0000-0000-0000-000000000001", "title": id, "cwd": "/src",
                "connection_id": "anthropic", "model": "m", "status": status, "error": null, "effort": null,
                "created_at": 0, "updated_at": 0, "last_seq": 0, "observed": observed
            }))
            .unwrap()
        };
        let children = [child("child", "running", false), child("mirror", "running", true), child("ended", "idle", false)];
        assert_eq!(background_count(&events, &children), 3, "a running ordinary subagent counts; its native mirror does not count twice");
        let spawn: Event = serde_json::from_value(serde_json::json!({
            "seq": 6, "ts": 6000, "type": "subagent_spawned", "call_id": "call", "child_session_id": "child",
            "background_task_id": "child-task", "description": "Child"
        }))
        .unwrap();
        events.extend([spawn, event(7, "child-task", "started")]);
        assert_eq!(background_count(&events, &children), 3, "a child and its background task are one job");
        events.extend([event(8, "server", "finished"), native(9, "native", "finished", "local_agent", false)]);
        assert_eq!(background_count(&events, &children), 1);
        events.push(event(10, "child-task", "finished"));
        assert_eq!(background_count(&events, &[]), 0, "a stale foreground task cannot leave Background 0/1 behind");
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
    fn native_claude_code_handles_completion_and_can_be_stopped() {
        let mut native = event(1, "native", "started");
        if let EventKind::BackgroundTask { kind, .. } = &mut native.kind {
            *kind = "claude_code".into();
        }
        let tasks = lifecycle(&[native, event(2, "shell", "started"), event(3, "done", "finished")]);
        assert!(tasks[0].running());
        assert!(tasks[0].can_stop());
        assert_eq!(tasks[0].notification(), "Claude Code handles completion");
        assert!(tasks[1].can_stop());
        assert!(!tasks[2].can_stop());
    }

    #[test]
    fn native_outcomes_do_not_claim_an_os_exit_code() {
        for (state, expected) in [
            ("finished", "finished"),
            ("failed", "failed"),
            ("stopped", "stopped by Claude Code"),
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

    #[test]
    fn a_task_ended_badly_when_it_failed_was_lost_or_exited_non_zero() {
        let with = |task: &str, state: &str, kind: &str, exit: i32| {
            let mut e = event(1, task, state);
            if let EventKind::BackgroundTask { kind: k, exit_code, .. } = &mut e.kind {
                *k = kind.into();
                *exit_code = exit;
            }
            e
        };
        let evs = [
            with("build", "started", "shell", 0),
            with("ok", "finished", "shell", 0),
            with("tests", "finished", "shell", 101),
            with("agent", "finished", "agent", 1),
            with("native", "finished", "claude_code", 1),
            with("gone", "lost", "shell", 0),
            with("broke", "failed", "shell", 0),
            with("mine", "stopped_by_user", "shell", 0),
        ];
        let failed: Vec<String> = lifecycle(&evs).into_iter().filter(Task::failed).map(|t| t.id).collect();
        assert_eq!(failed, ["tests", "agent", "gone", "broke"]);
    }

    #[test]
    fn the_short_list_has_the_running_and_the_last_few_stopped_or_failed() {
        let evs = [
            event(1, "old fail", "failed"),
            event(2, "ok", "finished"),
            event(3, "stopped", "stopped_by_user"),
            event(4, "f2", "failed"),
            event(5, "f3", "lost"),
            event(6, "f4", "failed"),
            event(7, "run", "started"),
        ];
        let ids = |all| short_list(lifecycle(&evs), all).into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(false), ["run", "f4", "f3", "f2"], "the newest three ended badly, after what runs");
        assert_eq!(ids(true).len(), 7);
        assert_eq!(left_out(&evs), 3);
    }

    #[test]
    fn the_transcript_line_counts_tasks_and_servers_apart() {
        let with = |task: &str, state: &str, kind: &str, notify: bool| {
            let mut e = event(1, task, state);
            if let EventKind::BackgroundTask { kind: k, notify: n, .. } = &mut e.kind {
                *k = kind.into();
                *n = notify;
            }
            e
        };
        let evs = [
            with("build", "started", "shell", true),
            with("server", "started", "shell", false),
            with("native", "started", "claude_code", false),
            with("done", "started", "shell", true),
            with("done", "finished", "shell", true),
        ];
        assert_eq!(running_counts(&evs), (2, 1), "Claude Code's own count as tasks");
        assert_eq!(running_line(2, 1), "2 tasks running · 1 server");
        assert_eq!(running_line(1, 0), "1 background task running");
        assert_eq!(running_line(0, 2), "2 servers running");
    }
}
