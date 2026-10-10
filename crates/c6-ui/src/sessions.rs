//! Sessions list and the new-session form.

use c6_api::{Connection, CreateSession, Session, SessionStatus, WorkerStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::util::{BUTTON, ErrorBox, INPUT, StatusPill, ago, clock, storage_get, storage_set};

#[component]
pub fn SessionsPage() -> impl IntoView {
    let live = api::live();
    let now = clock();
    let connections = LocalResource::new(api::connections);

    view! {
        <main class="mx-auto max-w-6xl space-y-6 px-4 py-6">
            <Suspense fallback=|| ()>
                {move || connections.get().map(|r| match r {
                    Ok(conns) if conns.is_empty() => view! {
                        <div class="rounded-lg border border-dashed border-zinc-300 p-6 text-center text-sm text-zinc-600 dark:border-zinc-700 dark:text-zinc-400">
                            "Add an LLM connection in " <A href="/settings" attr:class="underline">"Settings"</A> " to start a session."
                        </div>
                    }.into_any(),
                    Ok(conns) => view! { <NewSession connections=conns /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>

            <section class="space-y-2">
                <h2 class="text-sm font-medium text-zinc-500">"Sessions"</h2>
                <div class="divide-y divide-zinc-200 overflow-hidden rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                    {move || {
                        let sessions = live.sessions.get();
                        if sessions.is_empty() {
                            view! { <p class="px-4 py-8 text-center text-sm text-zinc-500">"No sessions yet."</p> }.into_any()
                        } else {
                            sessions.into_iter().map(|s| view! { <SessionRow s now /> }).collect_view().into_any()
                        }
                    }}
                </div>
            </section>
        </main>
    }
}

#[component]
fn SessionRow(s: Session, now: RwSignal<i64>) -> impl IntoView {
    let live = api::live();
    let worker = live.worker_name(&s.worker_id.to_string());
    let updated = s.updated_at;
    view! {
        <A href=format!("/sessions/{}", s.id) attr:class="flex items-center gap-3 px-4 py-3 hover:bg-zinc-100 dark:hover:bg-zinc-900">
            <div class="min-w-0 flex-1">
                <div class="truncate font-medium">{s.title.clone()}</div>
                <div class="truncate text-xs text-zinc-500">
                    {worker} " · " <span class="font-mono">{s.cwd.clone()}</span> " · " {s.model.clone()}
                </div>
            </div>
            <StatusPill status=s.status />
            <span class="w-16 shrink-0 text-right text-xs text-zinc-500">{move || ago(now.get() - updated)}</span>
        </A>
    }
}

#[component]
fn NewSession(connections: Vec<Connection>) -> impl IntoView {
    let live = api::live();
    let navigate = use_navigate();

    let worker = RwSignal::new(storage_get("c6.worker").unwrap_or_default());
    let cwd = RwSignal::new(String::new());
    let connection = RwSignal::new(
        storage_get("c6.connection").filter(|c| connections.iter().any(|x| &x.id == c)).unwrap_or_else(|| connections[0].id.clone()),
    );
    let model = RwSignal::new(String::new());
    let effort = RwSignal::new(storage_get("c6.effort").unwrap_or_default());
    let worktree = RwSignal::new(storage_get("c6.worktree").as_deref() == Some("1"));
    let prompt = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let conns = StoredValue::new(connections);

    // Pick a worker once the fleet loads, preferring the last one used.
    Effect::new(move || {
        let ws = live.workers.get();
        let current = worker.get_untracked();
        if !ws.iter().any(|w| w.id.to_string() == current && w.status != WorkerStatus::Revoked)
            && let Some(w) =
                ws.iter().find(|w| w.status == WorkerStatus::Online).or_else(|| ws.iter().find(|w| w.status != WorkerStatus::Revoked))
        {
            worker.set(w.id.to_string());
        }
    });
    // Remember the last directory per worker.
    Effect::new(move || {
        let w = worker.get();
        cwd.set(storage_get(&format!("c6.cwd.{w}")).unwrap_or_default());
    });
    // Default model per connection; offer the provider's model list.
    let models = LocalResource::new(move || api::models(connection.get()));
    Effect::new(move || {
        let c = connection.get();
        let default = conns.with_value(|cs| cs.iter().find(|x| x.id == c).and_then(|x| x.default_model.clone()));
        model.set(storage_get(&format!("c6.model.{c}")).or(default).unwrap_or_default());
    });

    let submit = move || {
        if pending.get_untracked() {
            return;
        }
        let Ok(worker_id) = worker.get_untracked().parse() else {
            error.set(Some("Pick a worker".into()));
            return;
        };
        let req = CreateSession {
            worker_id,
            cwd: cwd.get_untracked(),
            connection_id: connection.get_untracked(),
            model: model.get_untracked(),
            title: None,
            prompt: Some(prompt.get_untracked()).filter(|p| !p.trim().is_empty()),
            effort: Some(effort.get_untracked()).filter(|e| !e.is_empty()),
            worktree: worktree.get_untracked(),
        };
        storage_set("c6.worktree", if worktree.get_untracked() { "1" } else { "0" });
        storage_set("c6.effort", &effort.get_untracked());
        storage_set("c6.worker", &req.worker_id.to_string());
        storage_set(&format!("c6.cwd.{}", req.worker_id), &req.cwd);
        storage_set("c6.connection", &req.connection_id);
        storage_set(&format!("c6.model.{}", req.connection_id), &req.model);
        pending.set(true);
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::create_session(req).await {
                Ok(s) => navigate(&format!("/sessions/{}", s.id), Default::default()),
                Err(e) => error.set(Some(e)),
            }
            pending.set(false);
        });
    };
    let submit_click = submit.clone();

    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900/50">
            <div class="grid gap-3 sm:grid-cols-[1fr_2fr_auto]">
                <label class="block text-xs text-zinc-500">
                    "Worker"
                    <select class=format!("mt-1 {INPUT}") on:change=move |ev| worker.set(event_target_value(&ev)) prop:value=move || worker.get()>
                        {move || live.workers.get().into_iter().filter(|w| w.status != WorkerStatus::Revoked).map(|w| {
                            let label = if w.status == WorkerStatus::Online { w.name.clone() } else { format!("{} (offline)", w.name) };
                            view! { <option value=w.id.to_string()>{label}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <label class="block text-xs text-zinc-500 sm:col-span-2">
                    "Working directory"
                    <input class=format!("mt-1 font-mono {INPUT}") placeholder="~/src/project"
                        prop:value=move || cwd.get() on:input=move |ev| cwd.set(event_target_value(&ev)) />
                </label>
                <label class="block text-xs text-zinc-500">
                    "Connection"
                    <select class=format!("mt-1 {INPUT}") on:change=move |ev| connection.set(event_target_value(&ev)) prop:value=move || connection.get()>
                        {conns.get_value().into_iter().map(|c| view! { <option value=c.id.clone()>{c.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                <label class="block text-xs text-zinc-500">
                    "Model"
                    <input class=format!("mt-1 font-mono {INPUT}") list="c6-models" placeholder="model id"
                        prop:value=move || model.get() on:input=move |ev| model.set(event_target_value(&ev)) />
                    <datalist id="c6-models">
                        {move || models.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|m| view! { <option value=m></option> }).collect_view()}
                    </datalist>
                </label>
                <label class="block text-xs text-zinc-500">
                    "Effort"
                    <select class=format!("mt-1 {INPUT}") prop:value=move || effort.get() on:change=move |ev| effort.set(event_target_value(&ev))>
                        <option value="">"default"</option>
                        <option value="low">"low"</option>
                        <option value="medium">"medium"</option>
                        <option value="high">"high"</option>
                        <option value="xhigh">"xhigh"</option>
                        <option value="max">"max"</option>
                    </select>
                </label>
            </div>
            <textarea class=format!("{INPUT} min-h-24 resize-y") placeholder="What should the agent do? (⌘/Ctrl+Enter to start)"
                prop:value=move || prompt.get()
                on:input=move |ev| prompt.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { ev.prevent_default(); submit(); }
                }
            ></textarea>
            {move || {
                let is_cc = conns.with_value(|cs| cs.iter().any(|c| c.id == connection.get() && c.kind == "claude_code"));
                let w = worker.get();
                let missing = live.workers.with(|ws| ws.iter().any(|x| x.id.to_string() == w && x.status == WorkerStatus::Online && !x.capabilities.iter().any(|c| c == "claude_code")));
                (is_cc && missing).then(|| view! {
                    <p class="text-sm text-amber-700 dark:text-amber-400">"This worker doesn't have the claude CLI installed (or it isn't on the worker's PATH; set C6_CLAUDE_BIN)."</p>
                })
            }}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex items-center justify-end gap-4">
                <label class="flex items-center gap-1.5 text-sm text-zinc-600 dark:text-zinc-400" title="Create a c6/… branch and a separate checkout for this session">
                    <input type="checkbox" prop:checked=move || worktree.get() on:change=move |ev| worktree.set(event_target_checked(&ev)) />
                    "New git worktree"
                </label>
                <button class=BUTTON disabled=move || pending.get() on:click=move |_| submit_click()>"Start session"</button>
            </div>
        </section>
    }
}

/// Live status for a session id, from the session list feed.
pub fn live_status(id: &str) -> Option<SessionStatus> {
    api::live().sessions.with(|ss| ss.iter().find(|s| s.id == id).map(|s| s.status))
}
