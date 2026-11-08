//! Background task lifecycle history and read-only stream inspection.

use std::collections::HashMap;

use c6_api::{Event, EventKind, Session, SessionStatus, TurnEndReason};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::icons::{Icon, Size, icon};
use crate::util::{BUTTON_QUIET, clock};

#[derive(Clone, PartialEq)]
struct Task {
    id: String,
    kind: String,
    description: String,
    since: i64,
    since_seq: u64,
    updated_at: i64,
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

    fn elapsed(&self, now: i64) -> String {
        // A child-only fallback has no run start/duration evidence. Its
        // creation and last update are not this run's elapsed time.
        if self.since_seq == 0 {
            "Elapsed unknown".into()
        } else {
            elapsed(if self.running() { now - self.since } else { self.elapsed_ms.min(i64::MAX as u64) as i64 })
        }
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

/// Latest lifecycle record for every known task; running tasks come first.
/// No worker request is needed, including for ended or lost tasks.
fn lifecycle(events: &[Event]) -> Vec<Task> {
    let mut latest: HashMap<&str, Task> = HashMap::new();
    for e in events {
        if let EventKind::BackgroundTask { task_id, kind, description, state, exit_code, elapsed_ms, notify, native_type, foreground } =
            &e.kind
        {
            let before = latest.get(task_id.as_str());
            let since = before.map(|t| t.since).unwrap_or(e.ts);
            // Started again: a new run, unless it was running (Claude Code
            // moved it to the background).
            let since = if state == "started" && !before.is_some_and(Task::running) { e.ts } else { since };
            let since_seq =
                if state == "started" && !before.is_some_and(Task::running) { e.seq } else { before.map_or(e.seq, |t| t.since_seq) };
            latest.insert(
                task_id,
                Task {
                    id: task_id.clone(),
                    kind: kind.clone(),
                    description: description.clone(),
                    since,
                    since_seq,
                    updated_at: e.ts,
                    state: state.clone(),
                    exit_code: *exit_code,
                    elapsed_ms: *elapsed_ms,
                    notify: *notify,
                    native_type: native_type.clone(),
                    foreground: *foreground,
                },
            );
        }
    }
    // Walking back finds each task's last record first. Reverse each group
    // to retain last-record order, with running tasks still ahead of ended.
    let mut running = Vec::new();
    let mut ended = Vec::new();
    for e in events.iter().rev() {
        if let EventKind::BackgroundTask { task_id, .. } = &e.kind
            && let Some(task) = latest.remove(task_id.as_str())
        {
            if task.running() {
                running.push(task);
            } else {
                ended.push(task);
            }
        }
    }
    running.reverse();
    ended.reverse();
    running.extend(ended);
    running
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
#[cfg(test)]
fn running_counts(events: &[Event]) -> (usize, usize) {
    lifecycle(events)
        .iter()
        .filter(|t| t.running())
        .fold((0, 0), |(tasks, servers), t| if t.notify || t.kind == "claude_code" { (tasks + 1, servers) } else { (tasks, servers + 1) })
}

/// Distinct jobs actually running in the background, including servers.
/// A carbon6 background subagent also has a task record: count it once.
/// Claude's read-only mirrors are represented by its native task records,
/// whose foreground flag tells us whether they really run in the background.
pub(crate) fn background_count(jobs: &[Job]) -> usize {
    jobs.iter().filter(|j| j.task.running()).count()
}

/// One identity, with the latest run's state and all its task IDs. A resume
/// may allocate another task ID but must not allocate another visible job.
#[derive(Clone, PartialEq)]
pub(crate) struct Job {
    pub(crate) id: String,
    task: Task,
    child: Option<Session>,
    task_ids: Vec<String>,
}

fn native_identity(id: &str) -> Option<String> {
    let parts: Vec<serde_json::Value> = serde_json::from_str(id.strip_prefix("claude:")?).ok()?;
    Some(format!("claude:{}", serde_json::to_string(parts.get(..2)?).ok()?))
}

pub(crate) fn jobs(events: &[Event], children: &[Session]) -> Vec<Job> {
    // A log can straddle the worker upgrade that began linking native
    // mirrors. Resolve all explicit scope/task aliases before grouping so
    // unlinked runs share the linked child's identity in either order.
    // If runs have different mirrors, the latest explicit link by event
    // sequence chooses their representative child, even for exact links.
    let mut spawns: HashMap<&str, (&str, bool)> = HashMap::new();
    let mut task_children: HashMap<&str, &str> = HashMap::new();
    let mut native_children: HashMap<String, (u64, &str)> = HashMap::new();
    for e in events {
        if let EventKind::SubagentSpawned { child_session_id, background_task_id, description, .. } = &e.kind {
            // Description and exact task links keep their first log match;
            // any foreground-only spawn excludes a child-only fallback.
            let spawn = spawns.entry(child_session_id).or_insert((description, false));
            spawn.1 |= background_task_id.is_empty();
            task_children.entry(background_task_id).or_insert(child_session_id);
            if let Some(id) = native_identity(background_task_id) {
                let linked = native_children.entry(id).or_insert((e.seq, child_session_id));
                if e.seq > linked.0 {
                    *linked = (e.seq, child_session_id);
                }
            }
        }
    }
    let mut children_by_id = HashMap::new();
    for child in children {
        children_by_id.entry(child.id.as_str()).or_insert(child);
    }
    let mut out: Vec<Job> = Vec::new();
    let mut job_indices: HashMap<String, usize> = HashMap::new();
    for task in lifecycle(events).into_iter().filter(|t| !t.foreground) {
        let native = native_identity(&task.id);
        let child_id = native
            .as_ref()
            .and_then(|id| native_children.get(id).map(|(_, child)| *child))
            .or_else(|| task_children.get(task.id.as_str()).copied());
        let id = child_id.map(str::to_owned).or(native).unwrap_or_else(|| task.id.clone());
        if let Some(&index) = job_indices.get(&id) {
            let job = &mut out[index];
            job.task_ids.push(task.id.clone());
            if task.since_seq >= job.task.since_seq {
                job.task = task;
            }
        } else {
            let child = child_id.and_then(|id| children_by_id.get(id)).copied().cloned();
            job_indices.insert(id.clone(), out.len());
            out.push(Job { id, task_ids: vec![task.id.clone()], task, child });
        }
    }
    for child in children.iter().filter(|s| !s.observed) {
        if job_indices.contains_key(&child.id) {
            continue;
        }
        // A known foreground-only spawn belongs to the transcript, not here.
        let foreground = spawns.get(child.id.as_str()).is_some_and(|(_, foreground)| *foreground);
        if foreground {
            continue;
        }
        let state = if matches!(child.status, SessionStatus::Running | SessionStatus::Starting) {
            "started"
        } else if child.status == SessionStatus::Failed {
            "failed"
        } else {
            match child.end_reason {
                Some(TurnEndReason::Interrupted) => "stopped_by_user",
                Some(TurnEndReason::Error | TurnEndReason::WorkerRestarted | TurnEndReason::Unknown) => "failed",
                _ => "finished",
            }
        };
        job_indices.insert(child.id.clone(), out.len());
        out.push(Job {
            id: child.id.clone(),
            child: Some(child.clone()),
            task_ids: Vec::new(),
            task: Task {
                id: child.id.clone(),
                kind: "agent".into(),
                description: child.title.clone(),
                since: child.created_at,
                since_seq: 0,
                updated_at: child.updated_at,
                state: state.into(),
                exit_code: 0,
                elapsed_ms: 0,
                notify: true,
                native_type: String::new(),
                foreground: false,
            },
        });
    }
    for job in &mut out {
        let original = spawns.get(job.id.as_str()).map(|(description, _)| (*description).to_owned());
        if job.child.is_some() || original.is_some() {
            if job.task.kind != "claude_code" {
                job.task.kind = "agent".into();
            }
            job.task.description = job
                .child
                .as_ref()
                .map(|s| s.title.clone())
                .filter(|t| !t.trim().is_empty())
                .or(original.filter(|t| !t.trim().is_empty()))
                .unwrap_or_else(|| "Background agent".into());
        } else if job.task.description.trim().is_empty() {
            job.task.description = if job.task.native_type == "local_agent" { "Background agent" } else { "Background command" }.into();
        }
    }
    out.sort_by_key(|j| (j.task.since, j.task.since_seq, j.id.clone()));
    out
}

#[derive(Clone, PartialEq)]
pub(crate) enum Page {
    Landing,
    Details,
    History,
    Inspect(String),
    Actions(String),
}

pub(crate) fn job_list(chrome: crate::session::SessionChrome) -> Memo<Vec<Job>> {
    chrome.jobs
}

pub(crate) fn history_label(list: &[Job]) -> String {
    let failed = list.iter().filter(|j| !j.task.running() && j.task.failed()).count();
    let interrupted = list.iter().filter(|j| !j.task.running() && j.task.stopped()).count();
    let mut label = "History".to_owned();
    if failed > 0 {
        label.push_str(&format!(" · {failed} failed"));
    }
    if interrupted > 0 {
        label.push_str(&format!(" · {interrupted} interrupted"));
    }
    label
}

pub(crate) fn actions_label(list: &[Job], page: &Page) -> &'static str {
    let Page::Actions(id) = page else { return "Background job" };
    if list.iter().find(|j| &j.id == id).is_some_and(|j| j.task.kind == "agent" || j.task.native_type == "local_agent") {
        "Background agent"
    } else {
        "Background command"
    }
}

/// The same compact rows on all surfaces; only live identities land here.
#[component]
pub(crate) fn JobRows(
    list: Memo<Vec<Job>>,
    navigate: Callback<(Page, web_sys::MouseEvent)>,
    #[prop(default = false)] history: bool,
) -> impl IntoView {
    let now = clock();
    let filter = RwSignal::new("All");
    let limit = RwSignal::new(30usize);
    let rows = Memo::new(move |_| {
        let mut rows = list
            .get()
            .into_iter()
            .filter(|j| {
                if history {
                    !j.task.running()
                        && match filter.get() {
                            "Failed" => j.task.failed(),
                            "Interrupted" => j.task.stopped(),
                            _ => true,
                        }
                } else {
                    j.task.running()
                }
            })
            .collect::<Vec<_>>();
        if history {
            rows.sort_by_key(|j| std::cmp::Reverse(j.task.updated_at));
        }
        rows
    });
    view! {
        {history.then(|| view! {
            <div class="flex gap-2" aria-label="History filters">
                {["All", "Failed", "Interrupted"].into_iter().map(|value| view! {
                    <button class=format!("{BUTTON_QUIET} min-h-11") aria-pressed=move || (filter.get() == value).to_string()
                        on:click=move |_| { filter.set(value); limit.set(30); }>{value}</button>
                }).collect_view()}
            </div>
        })}
        <Show when=move || !rows.with(Vec::is_empty) fallback=move || view! { <p class="py-2 text-xs text-muted">{if history { "No matching history" } else { "Nothing running now" }}</p> }>
            <div class="divide-y divide-zinc-100 dark:divide-zinc-800">
                {move || rows.get().into_iter().take(if history { limit.get() } else { usize::MAX }).map(|job| {
                    let task = job.task;
                    let id = StoredValue::new(job.id);
                    let agent = task.kind == "agent" || task.native_type == "local_agent";
                    let kind = if agent { "Agent" } else if task.notify || task.kind == "claude_code" { "Command" } else { "Server / Watcher" };
                    let name = task.description.clone();
                    let actions = format!("Actions for {name}");
                    view! {
                        <div class="flex min-h-11 items-center gap-2 text-[13px]" data-background-job=id.get_value()>
                            <span role="img" aria-label=kind class="shrink-0 text-sky-600 dark:text-sky-400">{icon(if agent { Icon::Bot } else { Icon::SquareTerminal }, Size::S16)}</span>
                            <button class="min-h-11 min-w-0 flex-1 truncate text-left" on:click=move |ev| navigate.run((Page::Inspect(id.get_value()), ev))>{name}</button>
                            <span class="shrink-0 text-xs tabular-nums text-muted">{move || task.elapsed(now.get())}</span>
                            <button class=crate::components::SESSION_BAR_BUTTON aria-label=actions
                                on:click=move |ev| navigate.run((Page::Actions(id.get_value()), ev))>{crate::icons::ellipsis()}</button>
                        </div>
                    }
                }).collect_view()}
            </div>
        </Show>
        {move || (history && rows.with(Vec::len) > limit.get()).then(|| view! {
            <button class=format!("{BUTTON_QUIET} min-h-11") on:click=move |_| limit.update(|n| *n += 30)>"Show more"</button>
        })}
    }
}

/// The words beside the running-task spinner at the bottom of a transcript.
pub fn running_line(tasks: usize, servers: usize) -> String {
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    match (tasks, servers) {
        (0, s) => plural(s, "server", "servers"),
        (t, 0) => plural(t, "background task", "background tasks"),
        (t, s) => format!("{} · {}", plural(t, "background task", "background tasks"), plural(s, "server", "servers")),
    }
}

/// Claude Code's own agents running in the background (`local_agent`, not
/// holding the turn): a Stop stops them too (docs/background.md §4).
pub fn background_agents(events: &[Event]) -> usize {
    lifecycle(events).iter().filter(|t| t.running() && t.kind == "claude_code" && t.native_type == "local_agent" && !t.foreground).count()
}

pub(crate) fn stop_unavailable(connected: bool, session: Option<&Session>, workers: &[c6_api::Worker]) -> bool {
    !connected || session.is_none_or(|s| crate::session::worker_offline(s, workers))
}

/// Inspect and actions replace the sheet body; their live record also removes
/// Stop if the job ends while its menu is open. No optimistic stop success.
#[component]
pub(crate) fn JobDetail(
    chrome: crate::session::SessionChrome,
    list: Memo<Vec<Job>>,
    page: RwSignal<Page>,
    navigate: Callback<(Page, web_sys::MouseEvent)>,
    on_done: Callback<()>,
) -> impl IntoView {
    let selected = Memo::new(move |_| {
        let (Page::Inspect(id) | Page::Actions(id)) = page.get() else { return None };
        list.with(|rows| rows.iter().find(|j| j.id == id).cloned())
    });
    let task = Memo::new(move |_| selected.get().map(|j| j.task));
    let error = RwSignal::new(None::<String>);
    let stopping = RwSignal::new(false);
    let now = clock();
    let offline = Memo::new(move |_| chrome.background_stale());
    view! {
        {move || selected.get().map(|job| {
            let agent = job.task.kind == "agent" || job.task.native_type == "local_agent";
            view! {
                <h3 class="break-words font-semibold">{job.task.description.clone()}</h3>
                <p class="text-xs text-muted">{format!("{} · {}", if agent { "Agent" } else { "Command" }, job.task.status())}" · "{move || task.get().map(|t| t.elapsed(now.get()))}</p>
            }
        })}
        <Show when=move || matches!(page.get(), Page::Actions(_)) fallback=move || view! {
            {move || selected.get().map(|job| {
                if job.task_ids.is_empty() {
                    view! { <ChildSnapshot child=job.id /> }.into_any()
                } else {
                    view! { <TaskInspector session=chrome.id.get_value() task /> }.into_any()
                }
            })}
            <details class="text-xs">
                <summary class="min-h-11 cursor-pointer py-3">"Runs and raw IDs"</summary>
                <pre class="whitespace-pre-wrap break-all">{move || selected.get().map(|job| {
                    let mut lines = vec![format!("Job {}", job.id)];
                    chrome.events.with(|events| {
                        for e in events {
                            if let EventKind::BackgroundTask { task_id, state, elapsed_ms, .. } = &e.kind
                                && job.task_ids.contains(task_id) {
                                lines.push(format!("{} · {} · {} · {}", e.ts, task_id, state, elapsed(*elapsed_ms as i64)));
                            }
                        }
                    });
                    lines.join("\n")
                })}</pre>
            </details>
        }>
            <button class="flex min-h-11 w-full items-center gap-2 text-left" on:click=move |ev| {
                if let Some(job) = selected.get_untracked() { navigate.run((Page::Inspect(job.id), ev)); }
            }>{icon(Icon::Info, Size::S16)}"Inspect"{crate::icons::chevron_right()}</button>
            <Show when=move || selected.with(|j| j.as_ref().is_some_and(|j| j.task.can_stop() && (!j.task_ids.is_empty() || !j.child.as_ref().is_some_and(|s| s.observed))))>
                <button class="flex min-h-11 w-full items-center gap-2 border-t border-zinc-200 text-left text-red-600 disabled:opacity-40 dark:border-zinc-700 dark:text-red-400"
                    disabled=move || offline.get() || stopping.get() on:click=move |_| {
                        if offline.get_untracked() {
                            error.set(Some("Stop unavailable: reconnect and load the session's current state first.".into()));
                            return;
                        }
                        let Some(job) = selected.get_untracked() else { return };
                        error.set(None); stopping.set(true);
                        spawn_local(async move {
                            let result = if job.task_ids.is_empty() { api::interrupt(job.id).await }
                                else { api::stop_task(chrome.id.get_value(), job.task.id).await };
                            if let Err(e) = result { error.set(Some(e)); }
                            stopping.set(false);
                        });
                    }>{crate::icons::stop()}{move || if selected.with(|j| j.as_ref().is_some_and(|j| j.task.kind == "agent" || j.task.native_type == "local_agent")) { "Stop this agent" } else { "Stop this command" }}</button>
            </Show>
        </Show>
        {move || error.get().map(|message| view! { <p role="alert" class="break-words text-red-600 dark:text-red-400">{message}</p> })}
        {move || selected.get().and_then(|j| j.child).map(|child| view! {
            <button class="flex min-h-11 items-center gap-2" on:click=move |ev| {
                let id = child.id.clone();
                if ev.meta_key() || ev.ctrl_key() {
                    if let Some(col) = chrome.col { crate::panes::open(col, crate::workspace::View::session(id), true); }
                } else {
                    chrome.subagents.go(id.clone(), move || {
                        if let Some(col) = chrome.col { crate::panes::open(col, crate::workspace::View::session(id), false); }
                    });
                }
                on_done.run(());
            }>{icon(Icon::ExternalLink, Size::S16)}"Open thread"</button>
        })}
    }
}

#[component]
fn ChildSnapshot(child: String) -> impl IntoView {
    let snapshot = LocalResource::new(move || api::session_events_before(child.clone(), u64::MAX, 100, None));
    view! {
        <button class=format!("{BUTTON_QUIET} min-h-11") on:click=move |_| snapshot.refetch()>"Refresh"</button>
        <p class="text-xs text-muted">"Read-only output · latest 100 events"</p>
        <Suspense fallback=|| view! { <p>"Loading output…"</p> }>
            {move || snapshot.get().map(|result| match result {
                Err(message) => view! { <p role="alert">{message}</p> }.into_any(),
                Ok(events) => view! { <pre class="whitespace-pre-wrap break-words text-xs">{events.iter().filter_map(|e| match &e.kind {
                    EventKind::Assistant { blocks, .. } => Some(blocks.iter().filter_map(|b| match b { c6_api::ContentBlock::Text { text, .. } => Some(text.as_str()), _ => None }).collect::<Vec<_>>().join("\n")),
                    _ => None,
                }).collect::<Vec<_>>().join("\n")}</pre> }.into_any(),
            })}
        </Suspense>
    }
}

/// Mounted only while open. The resource tracks lifecycle, never the clock or
/// model output, so ordinary session deltas do not cause snapshot polling.
#[component]
fn TaskInspector(session: String, task: Memo<Option<Task>>) -> impl IntoView {
    let snapshot = LocalResource::new(move || {
        let task = task.get();
        let session = session.clone();
        async move {
            let task = task.ok_or_else(|| "This task is no longer in the session history.".to_owned())?;
            api::task_snapshot(session, task.id).await
        }
    });
    view! {
        <div class="space-y-3">
                <button class=format!("{BUTTON_QUIET} min-h-11") on:click=move |_| snapshot.refetch()>"Refresh"</button>
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
        "finished" if native || exit_code == 0 => crate::icons::check(),
        "finished" | "failed" => icon(Icon::CircleX, Size::S14),
        "stopped" | "stopped_by_user" => crate::icons::stop(),
        "lost" => crate::icons::warning(),
        _ => crate::icons::circle(),
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

    fn background_count(events: &[Event], subagents: &[Session]) -> usize {
        super::background_count(&jobs(events, subagents))
    }

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

    fn child_session(id: &str, title: &str, status: &str, observed: bool) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id, "worker_id": "00000000-0000-0000-0000-000000000001", "title": title, "cwd": "/src",
            "connection_id": "anthropic", "model": "m", "status": status, "error": null, "effort": null,
            "created_at": 0, "updated_at": 0, "last_seq": 0, "observed": observed
        }))
        .unwrap()
    }

    fn spawn_event(seq: u64, child: &str, task: &str, description: &str) -> Event {
        serde_json::from_value(serde_json::json!({
            "seq": seq, "ts": seq as i64 * 1000, "type": "subagent_spawned", "call_id": format!("call-{seq}"),
            "child_session_id": child, "background_task_id": task, "description": description
        }))
        .unwrap()
    }

    fn agent(seq: u64, task: &str, state: &str, description: &str) -> Event {
        let mut e = event(seq, task, state);
        if let EventKind::BackgroundTask { kind, description: text, notify, exit_code, .. } = &mut e.kind {
            *kind = "agent".into();
            *text = description.into();
            *notify = true;
            *exit_code = 0;
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
    fn jobs_merge_linked_continuations_and_count_only_the_latest_outcome() {
        let child = child_session("child", "Human child title", "running", false);
        let mut events = vec![
            spawn_event(1, "child", "bg1", "Original task description"),
            agent(2, "bg1", "started", "Original task description"),
            agent(3, "bg1", "failed", "Original task description"),
            spawn_event(4, "child", "bg2", "Original task description"),
            agent(5, "bg2", "started", "continue child"),
        ];
        let children = [child];
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1, "a continuation is another run, not another job");
        let job = &list[0];
        assert_eq!(job.id, "child");
        assert_eq!(job.task.id, "bg2");
        assert!(job.task.running());
        assert_eq!(job.task.description, "Human child title", "never expose the raw continue description as its name");
        assert_eq!((job.task.since, job.task.since_seq), (5000, 5), "elapsed starts at this run, not the original spawn");
        let mut ids = job.task_ids.clone();
        ids.sort();
        assert_eq!(ids, ["bg1", "bg2"], "Inspect retains both attempts' IDs");
        assert_eq!(background_count(&events, &children), 1);
        assert_eq!(history_label(&list), "History", "a resumed job's earlier failure is not a current problem count");

        events.push(agent(6, "bg2", "stopped_by_user", "continue child"));
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1);
        assert!(!list[0].task.running());
        assert_eq!(list[0].task.id, "bg2");
        assert_eq!(background_count(&events, &children), 0);
        assert_eq!(history_label(&list), "History · 1 interrupted", "only the latest run's outcome counts");

        events.extend([
            spawn_event(7, "child", "bg3", "Original task description"),
            agent(8, "bg3", "started", "continue bg1"),
            agent(9, "bg3", "finished", "continue bg1"),
        ]);
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task.id, "bg3");
        assert_eq!(list[0].task_ids.len(), 3);
        assert!(!list[0].task.failed());
        assert!(!list[0].task.stopped());
        assert_eq!(history_label(&list), "History", "a successful latest run clears earlier failure/interruption counts");
    }

    #[test]
    fn jobs_with_the_same_human_title_keep_distinct_identities() {
        let children = [
            child_session("child-a", "Review the layout", "running", false),
            child_session("child-b", "Review the layout", "running", false),
        ];
        let events = [
            spawn_event(1, "child-a", "bg1", "Review the layout"),
            agent(2, "bg1", "started", "Review the layout"),
            spawn_event(3, "child-b", "bg2", "Review the layout"),
            agent(4, "bg2", "started", "Review the layout"),
        ];
        let list = jobs(&events, &children);
        assert_eq!(list.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(), ["child-a", "child-b"]);
        assert!(list.iter().all(|j| j.task.description == "Review the layout" && j.task.running()));
        assert_eq!(background_count(&events, &children), 2, "merge identities, never names");
    }

    #[test]
    fn jobs_choose_the_later_start_sequence_when_timestamps_tie() {
        let mut events = [
            spawn_event(1, "child", "old", "Review"),
            agent(2, "old", "started", "Review"),
            agent(3, "old", "failed", "Review"),
            spawn_event(4, "child", "latest", "Review"),
            agent(5, "latest", "started", "continue child"),
        ];
        for e in &mut events {
            e.ts = 1000;
        }
        // lifecycle visits running tasks first, so the old failed run is
        // encountered later during merging despite its earlier start.
        let list = jobs(&events, &[]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task.id, "latest");
        assert!(list[0].task.running());
        assert_eq!((list[0].task.since, list[0].task.since_seq), (1000, 5));
        assert_eq!(list[0].task_ids.len(), 2);
        assert_eq!(history_label(&list), "History");
    }

    #[test]
    fn unlinked_native_jobs_collapse_runs_but_preserve_scope_and_task_identity() {
        let old = r#"claude:["scope-a","agent","run","zzz"]"#;
        let latest = r#"claude:["scope-a","agent","run","aaa"]"#;
        let other_scope = r#"claude:["scope-b","agent","run","aaa"]"#;
        let other_task = r#"claude:["scope-a","other","run","aaa"]"#;
        let events = [
            native(1, old, "started", "local_agent", false),
            native(2, old, "failed", "local_agent", false),
            native(3, latest, "started", "local_agent", false),
            native(4, latest, "finished", "local_agent", false),
            native(5, other_scope, "started", "local_agent", false),
            native(6, other_task, "started", "local_agent", false),
        ];
        let list = jobs(&events, &[]);
        assert_eq!(list.len(), 3);
        let job = list.iter().find(|j| j.id == r#"claude:["scope-a","agent"]"#).unwrap();
        assert_eq!(job.task.id, latest, "run IDs are opaque, not sorted lexically");
        assert_eq!(job.task_ids.len(), 2);
        assert_eq!(job.task.kind, "claude_code");
        assert_eq!(job.task.native_type, "local_agent");
        assert_eq!(job.task.status(), "finished");
        assert!(!job.task.failed(), "native completion has no OS exit-code semantics");
        assert!(list.iter().any(|j| j.id == r#"claude:["scope-b","agent"]"#));
        assert!(list.iter().any(|j| j.id == r#"claude:["scope-a","other"]"#));
        assert_eq!(background_count(&events, &[]), 2);
        assert_eq!(history_label(&list), "History");
    }

    #[test]
    fn linked_native_mirrors_keep_native_kind_and_real_task_target() {
        let old = r#"claude:["scope","agent","run","old"]"#;
        let latest = r#"claude:["scope","agent","run","new"]"#;
        let children = [child_session("mirror", "Human mirror title", "running", true)];
        let mut events = vec![
            spawn_event(1, "mirror", old, "Original agent description"),
            native(2, old, "started", "local_agent", false),
            native(3, old, "failed", "local_agent", false),
            spawn_event(4, "mirror", latest, "Original agent description"),
            native(5, latest, "started", "local_agent", false),
        ];
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1, "the observed mirror must not become another row");
        let job = &list[0];
        assert_eq!(job.id, "mirror", "an explicit spawn link takes precedence over the legacy native identity");
        assert_eq!(job.task.id, latest, "Stop targets the real native registry task, not the read-only mirror");
        assert!(job.child.as_ref().unwrap().observed);
        assert_eq!(job.task.description, "Human mirror title");
        assert_eq!(job.task.kind, "claude_code", "the human title must not change native lifecycle semantics to agent");
        assert_eq!(job.task.native_type, "local_agent");
        assert!(job.task.can_stop());
        assert_eq!(job.task.notification(), "Claude Code handles completion");
        assert_eq!(job.task_ids.len(), 2);
        assert_eq!(history_label(&list), "History");

        events.push(native(6, latest, "finished", "local_agent", false));
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task.kind, "claude_code");
        assert_eq!(list[0].task.status(), "finished", "never invent an OS exit status for a native agent");
        assert!(!list[0].task.can_stop());
        assert!(!list[0].task.failed());
        assert_eq!(history_label(&list), "History");
    }

    fn assert_mixed_native_links(link_latest: bool) {
        let old = r#"claude:["scope","agent","run","old"]"#;
        let latest = r#"claude:["scope","agent","run","new"]"#;
        let children = [child_session("mirror", "Human mirror title", "running", true)];
        let mut events = vec![
            native(1, old, "started", "local_agent", false),
            native(2, old, "failed", "local_agent", false),
            native(3, latest, "started", "local_agent", false),
            // Resolve links before grouping, even when their event comes
            // after the task records or only names the old failed run.
            spawn_event(4, "mirror", if link_latest { latest } else { old }, "Original agent description"),
        ];
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1, "linked and unlinked runs of the same native scope/task are one job");
        let job = &list[0];
        assert_eq!(job.id, "mirror");
        assert_eq!(job.child.as_ref().unwrap().id, "mirror", "the unlinked run acquires only the explicit identity alias");
        assert_eq!(job.task.id, latest, "online Stop keeps the real current registry task target");
        assert_eq!(job.task.kind, "claude_code");
        assert_eq!(job.task.native_type, "local_agent");
        assert_eq!(job.task.description, "Human mirror title");
        assert!(job.task.running() && job.task.can_stop());
        assert_eq!(job.task_ids.len(), 2);
        assert!(job.task_ids.iter().any(|id| id == old));
        assert!(job.task_ids.iter().any(|id| id == latest));
        assert_eq!(background_count(&events, &children), 1);
        assert_eq!(history_label(&list), "History", "the old failed attempt is not a separate history job");

        events.push(native(5, latest, "finished", "local_agent", false));
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task.id, latest);
        assert_eq!(list[0].task.status(), "finished");
        assert_eq!(list[0].task_ids.len(), 2);
        assert_eq!(background_count(&events, &children), 0);
        assert_eq!(history_label(&list), "History");

        let different_scope = r#"claude:["different-scope","agent","run","new"]"#;
        events.push(native(6, different_scope, "started", "local_agent", false));
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 2, "aliases must not cross native scopes");
        assert!(list.iter().any(|j| j.id == r#"claude:["different-scope","agent"]"# && j.child.is_none()));
    }

    #[test]
    fn unlinked_native_old_run_merges_under_the_latest_linked_mirror() {
        assert_mixed_native_links(true);
    }

    #[test]
    fn unlinked_native_latest_run_merges_under_the_old_linked_mirror() {
        assert_mixed_native_links(false);
    }

    #[test]
    fn native_scope_task_uses_latest_explicit_mirror_for_all_linked_runs() {
        let old = r#"claude:["scope","agent","run","old"]"#;
        let latest = r#"claude:["scope","agent","run","new"]"#;
        let other_scope = r#"claude:["other-scope","agent","run","new"]"#;
        let other_task = r#"claude:["scope","other-task","run","new"]"#;
        let children = [
            child_session("old-mirror", "Same title", "idle", true),
            child_session("new-mirror", "Same title", "running", true),
            child_session("other-scope", "Same title", "running", true),
            child_session("other-task", "Same title", "running", true),
        ];
        let mut events = vec![
            spawn_event(1, "old-mirror", old, "Same title"),
            native(2, old, "started", "local_agent", false),
            native(3, old, "failed", "local_agent", false),
            native(4, latest, "started", "local_agent", false),
            spawn_event(5, "new-mirror", latest, "Same title"),
        ];
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1, "exact links to different mirrors must not split one native scope/task");
        assert_eq!(list[0].id, "new-mirror", "latest explicit link sequence selects the representative mirror");
        assert_eq!(list[0].child.as_ref().unwrap().id, "new-mirror");
        assert_eq!(list[0].task.id, latest, "canonical mirror choice never changes the current native Stop target");
        assert_eq!(list[0].task_ids.len(), 2);
        assert_eq!(history_label(&list), "History");

        events.extend([
            spawn_event(6, "other-scope", other_scope, "Same title"),
            native(7, other_scope, "started", "local_agent", false),
            spawn_event(8, "other-task", other_task, "Same title"),
            native(9, other_task, "started", "local_agent", false),
        ]);
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 3, "same-title explicit aliases preserve unrelated scopes and tasks");
        assert!(list.iter().any(|j| j.id == "other-scope" && j.task.id == other_scope));
        assert!(list.iter().any(|j| j.id == "other-task" && j.task.id == other_task));
        assert_eq!(background_count(&events, &children), 3);
    }

    #[test]
    fn indexed_spawns_keep_first_exact_link_and_description_and_any_foreground_marker() {
        let children = [
            child_session("first", " ", "running", false),
            child_session("later", "Later mirror", "running", true),
            child_session("foreground", "Foreground", "running", false),
        ];
        let events = [
            spawn_event(1, "first", "task", "Original description"),
            spawn_event(2, "later", "task", "Later link"),
            spawn_event(3, "first", "continuation", "Later description"),
            spawn_event(4, "foreground", "unrecorded", "Background first"),
            spawn_event(5, "foreground", "", "Foreground later"),
            agent(6, "task", "started", "Task description"),
        ];
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "first", "exact task links keep their first event match");
        assert_eq!(list[0].task.description, "Original description", "a blank child title uses the first spawn description");
        assert_eq!(list[0].task.id, "task");
    }

    #[test]
    fn indexed_native_aliases_choose_highest_sequence_not_last_position_and_keep_first_tie() {
        let old = r#"claude:["scope","task","old"]"#;
        let latest = r#"claude:["scope","task","latest"]"#;
        let children = [
            child_session("new-mirror", "Newest", "running", true),
            child_session("old-mirror", "Old", "idle", true),
            child_session("tied-mirror", "Tied", "running", true),
        ];
        let events = [
            spawn_event(20, "new-mirror", latest, "Newest"),
            spawn_event(10, "old-mirror", old, "Old"),
            spawn_event(20, "tied-mirror", old, "Tied"),
            native(2, old, "finished", "local_agent", false),
            native(4, latest, "started", "local_agent", false),
        ];
        let list = jobs(&events, &children);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "new-mirror");
        assert_eq!(list[0].task.id, latest, "alias selection preserves the latest run's real Stop target");
        assert_eq!(list[0].task_ids.len(), 2);
    }

    #[test]
    fn long_history_merges_native_runs_excludes_foreground_children_and_keeps_original_descriptions() {
        const LANES: usize = 96;
        const RUNS: usize = 8;
        let mut events = Vec::new();
        let mut children = Vec::new();
        for lane in 0..LANES {
            let old_mirror = format!("old-mirror-{lane}");
            let mirror = format!("mirror-{lane}");
            let child = format!("child-{lane}");
            let foreground = format!("foreground-{lane}");
            children.extend([
                child_session(&old_mirror, "Same title", "idle", true),
                child_session(&mirror, " ", if lane % 3 == 0 { "idle" } else { "running" }, true),
                child_session(&child, " ", if lane % 2 == 0 { "idle" } else { "running" }, false),
                child_session(&foreground, "Same title", "running", false),
            ]);
            for run in 0..RUNS {
                let task = format!(r#"claude:["scope-{}","agent-{}","run-{run}"]"#, lane / 4, lane % 4);
                let linked = if run < RUNS / 2 { &old_mirror } else { &mirror };
                let description = if run == 0 || run == RUNS / 2 {
                    format!("Original native description {lane}")
                } else {
                    format!("continue native {lane}")
                };
                let seq = events.len() as u64 + 1;
                events.extend([spawn_event(seq, linked, &task, &description), native(seq + 1, &task, "started", "local_agent", false)]);
                if run + 1 < RUNS || lane % 3 == 0 {
                    events.push(native(seq + 2, &task, if run + 1 < RUNS { "failed" } else { "finished" }, "local_agent", false));
                }
            }
            let first = format!("ordinary-{lane}-first");
            let latest = format!("ordinary-{lane}-latest");
            let seq = events.len() as u64 + 1;
            events.extend([
                spawn_event(seq, &child, &first, &format!("Original child description {lane}")),
                agent(seq + 1, &first, "started", "Original task"),
                agent(seq + 2, &first, "failed", "Original task"),
                spawn_event(seq + 3, &child, &latest, "continue child"),
                agent(seq + 4, &latest, "started", "continue child"),
            ]);
            if lane % 2 == 0 {
                events.push(agent(seq + 5, &latest, "finished", "continue child"));
            }
            let seq = events.len() as u64 + 1;
            events.extend([
                spawn_event(seq, &foreground, "", "Foreground only"),
                native(seq + 1, &format!("foreground-command-{lane}"), "started", "local_bash", true),
            ]);
            for turn in 0..16 {
                let seq = events.len() as u64 + 1;
                events.push(Event {
                    seq,
                    ts: seq as i64 * 1000,
                    kind: EventKind::Assistant {
                        turn,
                        blocks: vec![c6_api::ContentBlock::Text { text: format!("Update {lane}/{turn}"), citations: Vec::new() }],
                        stop_reason: "end_turn".into(),
                    },
                });
            }
        }
        assert!(events.len() > 4000, "exercise a long log without a timing-dependent assertion");
        let list = jobs(&events, &children);
        assert_eq!(list.len(), LANES * 2, "native mirrors and foreground children must not become extra jobs");
        let by_id: HashMap<_, _> = list.iter().map(|job| (job.id.as_str(), job)).collect();
        for lane in 0..LANES {
            let native = by_id[format!("mirror-{lane}").as_str()];
            assert_eq!(native.task.id, format!(r#"claude:["scope-{}","agent-{}","run-{}"]"#, lane / 4, lane % 4, RUNS - 1));
            assert_eq!(native.task.kind, "claude_code");
            assert_eq!(native.task_ids.len(), RUNS);
            assert_eq!(native.task.running(), lane % 3 != 0);
            assert_eq!(native.task.description, format!("Original native description {lane}"));
            let ordinary = by_id[format!("child-{lane}").as_str()];
            assert_eq!(ordinary.task.id, format!("ordinary-{lane}-latest"));
            assert_eq!(ordinary.task_ids.len(), 2);
            assert_eq!(ordinary.task.running(), lane % 2 != 0);
            assert_eq!(ordinary.task.description, format!("Original child description {lane}"));
        }
        assert_eq!(super::background_count(&list), LANES / 3 * 2 + LANES / 2);
        assert_eq!(history_label(&list), "History", "earlier failed attempts must not leak into current outcomes");
    }

    #[test]
    fn job_surfaces_and_background_count_share_the_session_memos() {
        let tasks = include_str!("tasks.rs");
        let access = tasks.split("pub(crate) fn job_list(").nth(1).unwrap().split("\n}").next().unwrap();
        assert!(access.contains("chrome.jobs"));
        assert!(!access.contains("Memo::new") && !access.contains(".get("), "return the same memo, not a new reducer or cloned list");
        let source = include_str!("session.rs");
        let constructor =
            source.split("let jobs = Memo::new(move |_| {").nth(1).unwrap().split("let chrome = SessionChrome {").next().unwrap();
        assert!(constructor.contains("subagents.rows()") && constructor.contains("events.with(|evs| crate::tasks::jobs(evs, &rows))"));
        assert!(constructor.contains("let background = Memo::new(move |_| jobs.with(|list| crate::tasks::background_count(list)))"));
        assert!(!constructor.contains(".get("), "the background count borrows the shared list without cloning");
        let count = source.split("fn phone_background_count(").nth(1).unwrap().split("\n}").next().unwrap();
        assert!(count.contains("chrome.background.get()"));
        assert!(!count.contains("jobs(") && !count.contains("events.with") && !count.contains("subagents.rows()"));
        assert!(source.contains("pub(crate) jobs: Memo<Vec<crate::tasks::Job>>"));
    }

    #[test]
    fn stop_fails_closed_for_disconnected_missing_failed_or_offline_session_state() {
        let mut session = child_session("child", "Agent", "running", false);
        let worker: c6_api::Worker = serde_json::from_value(serde_json::json!({
            "id": session.worker_id, "name": "Worker", "hostname": "host", "os": "linux", "arch": "x86_64",
            "version": null, "status": "online", "enrolled_at": 0, "last_seen": null,
            "connected_since": null, "rtt_us": null
        }))
        .unwrap();
        let mut workers = [worker];
        assert!(!stop_unavailable(true, Some(&session), &workers), "a known online worker keeps real Stop available");
        assert!(stop_unavailable(false, Some(&session), &workers), "a stale online worker snapshot is not a live connection");
        assert!(stop_unavailable(true, None, &workers), "pending/missing session detail fails closed");
        let failed_detail: Result<&Session, &str> = Err("detail request failed");
        assert!(stop_unavailable(true, failed_detail.ok(), &workers), "failed detail fails closed too");
        for status in [c6_api::WorkerStatus::Offline, c6_api::WorkerStatus::Revoked] {
            workers[0].status = status;
            assert!(stop_unavailable(true, Some(&session), &workers));
        }
        assert!(stop_unavailable(true, Some(&session), &[]), "an absent worker is unavailable");
        session.on_head = true;
        assert!(!stop_unavailable(true, Some(&session), &[]), "known head sessions need no worker");
        assert!(stop_unavailable(false, Some(&session), &[]), "head sessions still require the live connection");
    }

    #[test]
    fn child_only_fallback_elapsed_is_unknown_until_run_evidence_exists() {
        let mut child = child_session("child", "Agent", "running", false);
        child.created_at = 1000;
        child.updated_at = 90000;
        let list = jobs(&[], &[child.clone()]);
        assert_eq!(list[0].task.elapsed(100000), "Elapsed unknown", "child age is not the current run's age");
        assert_eq!(list[0].task.elapsed_ms, 0);

        child.status = SessionStatus::Idle;
        child.end_reason = Some(TurnEndReason::Completed);
        let list = jobs(&[], &[child.clone()]);
        assert_eq!(list[0].task.elapsed(100000), "Elapsed unknown", "last update minus creation is not a completed run duration");

        let mut events = vec![spawn_event(1, "child", "bg1", "Agent"), agent(2, "bg1", "started", "Agent")];
        let list = jobs(&events, &[child.clone()]);
        assert_eq!(list[0].task.elapsed(9000), "7s", "an explicit run restores its real start time");
        events.push(agent(3, "bg1", "finished", "Agent"));
        let list = jobs(&events, &[child]);
        assert_eq!(list[0].task.elapsed(100000), "2s", "ended tasks use their recorded elapsed duration");
    }

    #[test]
    fn child_failure_without_a_turn_end_reason_is_a_failed_history_job() {
        let mut child = child_session("never-started", "Could not start", "failed", false);
        child.error = Some("worker could not start it".into());
        assert_eq!(child.end_reason, None);
        let list = jobs(&[], &[child]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task.state, "failed");
        assert!(list[0].task.failed());
        assert!(!list[0].task.running());
        assert!(!list[0].task.can_stop());
        assert_eq!(history_label(&list), "History · 1 failed");
    }

    #[test]
    fn explicitly_foreground_children_and_native_tasks_are_absent_from_background_jobs() {
        let native_id = r#"claude:["scope","foreground","run","one"]"#;
        let children = [
            child_session("foreground-child", "Foreground child", "running", false),
            child_session("mirror", "Foreground mirror", "running", true),
        ];
        let mut events = vec![
            spawn_event(1, "foreground-child", "", "Foreground child"),
            spawn_event(2, "mirror", native_id, "Foreground mirror"),
            native(3, native_id, "started", "local_agent", true),
            native(4, "foreground-command", "started", "local_bash", true),
        ];
        assert!(jobs(&events, &children).is_empty(), "a native identity link does not imply background execution");
        assert_eq!(background_count(&events, &children), 0);
        events.push(native(5, native_id, "failed", "local_agent", true));
        let list = jobs(&events, &children);
        assert!(list.is_empty(), "foreground-only failures do not enter background History either");
        assert_eq!(history_label(&list), "History");
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
        assert_eq!(running_line(2, 1), "2 background tasks · 1 server");
        assert_eq!(running_line(1, 2), "1 background task · 2 servers");
        assert_eq!(running_line(1, 0), "1 background task");
        assert_eq!(running_line(2, 0), "2 background tasks");
        assert_eq!(running_line(0, 1), "1 server");
        assert_eq!(running_line(0, 2), "2 servers");
    }
}
