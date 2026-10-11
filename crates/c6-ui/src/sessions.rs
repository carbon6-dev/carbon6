//! Sessions list and the new-session form.

use c6_api::{Connection, CreateSession, Session, SessionStatus, WorkerStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, ErrorBox, INPUT, StatusPill, ago, clock, storage_get, storage_set};
use crate::workspace::{Go, How, View, use_column, use_workspace};

#[component]
pub fn SessionsPage() -> impl IntoView {
    let live = api::live();
    let now = clock();
    let connections = LocalResource::new(api::connections);
    let query = RwSignal::new(String::new());
    let status_filter = RwSignal::new(String::new());
    let label_filter = crate::labels::use_filter().0;

    view! {
        <main class="mx-auto max-w-6xl space-y-6 px-4 py-6">
            <Suspense fallback=|| ()>
                {move || connections.get().map(|r| match r {
                    Ok(conns) if conns.is_empty() => view! {
                        <div class="rounded-lg border border-dashed border-zinc-300 p-6 text-center text-sm text-zinc-600 dark:border-zinc-700 dark:text-zinc-400">
                            "Add an LLM connection in " <Go to=View::Settings { note: None } class="underline">"Settings"</Go> " to start a session."
                        </div>
                    }.into_any(),
                    Ok(conns) => view! { <NewSession connections=conns /> <RunTemplate /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>

            <section class="space-y-2">
                <div class="flex flex-wrap items-center gap-2">
                    <h2 class="flex-1 text-sm font-medium text-zinc-500">"Sessions"</h2>
                    <input class=format!("{INPUT} w-56") placeholder="Search titles, labels, workers, paths" prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev)) />
                    <select class=format!("{INPUT} w-32") prop:value=move || status_filter.get() on:change=move |ev| status_filter.set(event_target_value(&ev))>
                        <option value="">"All"</option>
                        <option value="running">"Running"</option>
                        <option value="idle">"Idle"</option>
                        <option value="failed">"Failed"</option>
                    </select>
                </div>
                <crate::labels::LabelFilterBar />
                <div class="divide-y divide-zinc-200 overflow-hidden rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                    {move || {
                        let q = query.get().to_lowercase();
                        let st = status_filter.get();
                        let wanted = label_filter.get();
                        let sessions: Vec<Session> = live
                            .sessions
                            .get()
                            .into_iter()
                            .filter(|s| st.is_empty() || serde_json::to_value(s.status).ok().and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(st.as_str()))
                            .filter(|s| crate::labels::matches(s, &wanted))
                            .filter(|s| {
                                q.is_empty()
                                    || s.labels.iter().chain(s.auto_labels.iter()).any(|l| l.to_lowercase().contains(&q))
                                    || s.title.to_lowercase().contains(&q)
                                    || s.cwd.to_lowercase().contains(&q)
                                    || live.worker_name(&s.worker_id.to_string()).to_lowercase().contains(&q)
                            })
                            .collect();
                        if sessions.is_empty() {
                            let msg = if wanted.is_empty() && q.is_empty() { "No sessions yet." } else { "No sessions match." };
                            view! { <p class="px-4 py-8 text-center text-sm text-zinc-500">{msg}</p> }.into_any()
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
        <Go to=View::session(s.id.clone()) class="flex items-center gap-3 px-4 py-3 hover:bg-zinc-100 dark:hover:bg-zinc-900" title="Open beside this list (⌘-click: new tile)">
            <div class="min-w-0 flex-1">
                <div class="truncate font-medium">{s.title.clone()}</div>
                <div class="truncate text-xs text-zinc-500">
                    {worker} " · " <span class="font-mono">{match (&s.ssh, s.cwd.is_empty()) {
                        (Some(t), _) => format!("⇄ {t}:{}", s.cwd),
                        (None, true) => "scratch workspace".to_owned(),
                        (None, false) => s.cwd.clone(),
                    }}</span> " · " {s.model.clone()}
                </div>
                {(!s.labels.is_empty() || !crate::labels::shown_auto(&s.auto_labels).is_empty()).then(|| view! {
                    <div class="mt-1 flex flex-wrap gap-1">
                        {crate::labels::shown_auto(&s.auto_labels).into_iter().map(|l| view! { <crate::labels::LabelChip label=l auto=true stay=true /> }).collect_view()}
                        {s.labels.iter().cloned().map(|l| view! { <crate::labels::LabelChip label=l stay=true /> }).collect_view()}
                    </div>
                })}
            </div>
            <StatusPill status=s.status />
            <span class="w-16 shrink-0 text-right text-xs text-zinc-500">{move || ago(now.get() - updated)}</span>
        </Go>
    }
}

#[component]
fn NewSession(connections: Vec<Connection>) -> impl IntoView {
    let live = api::live();
    let (ws, col) = (use_workspace(), use_column());

    let worker = RwSignal::new(storage_get("c6.worker").unwrap_or_default());
    let cwd = RwSignal::new(String::new());
    let picking = RwSignal::new(false);
    // Or on an SSH host, through the worker: the target and a folder there.
    let remote = RwSignal::new(storage_get("c6.where").as_deref() == Some("ssh"));
    let ssh = RwSignal::new(String::new());
    let remote_dir = RwSignal::new(String::new());
    // Hosts from the inventory, offered by name.
    let inventory = LocalResource::new(api::hosts);
    let recent_ssh = move || -> Vec<String> {
        storage_get(&format!("c6.ssh.{}", worker.get())).map(|s| c6_api::labels::parse_labels(&s)).unwrap_or_default()
    };
    let worker_online = move || {
        let w = worker.get();
        live.workers.with(|ws| ws.iter().any(|x| x.id.to_string() == w && x.status == WorkerStatus::Online))
    };
    // Empty: "Auto", your routing rules choose the connection and model.
    let remembered_connection = storage_get("c6.connection").filter(|c| c.is_empty() || connections.iter().any(|x| &x.id == c));
    let connection = RwSignal::new(remembered_connection.clone().unwrap_or_else(|| connections[0].id.clone()));
    let has_routes = RwSignal::new(false);
    spawn_local(async move {
        let routes = api::get_setting("routes").await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
        let any = routes.iter().any(|r| r["enabled"].as_bool() != Some(false));
        has_routes.set(any);
        // First time here with rules set up: start on Auto.
        if any && remembered_connection.is_none() {
            connection.set(String::new());
        }
    });
    let auto = move || connection.get().is_empty();
    let model = RwSignal::new(String::new());
    let effort = RwSignal::new(storage_get("c6.effort").unwrap_or_default());
    let worktree = RwSignal::new(storage_get("c6.worktree").as_deref() == Some("1"));
    let prompt = RwSignal::new(String::new());
    // Your labels for the session (remembered per folder).
    let labels = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let conns = StoredValue::new(connections);
    // Effort only where the connection passes it on to the model.
    // On Auto, the rule sets it.
    let effort_supported = move || conns.with_value(|cs| cs.iter().find(|c| c.id == connection.get()).is_some_and(|c| c.supports_effort));

    // Pick a worker once the fleet loads: the last one used if it is online,
    // otherwise an online one. A worker the user picks by hand is kept.
    let chosen_by_hand = RwSignal::new(false);
    Effect::new(move || {
        let ws = live.workers.get();
        if chosen_by_hand.get_untracked() {
            return;
        }
        let current = worker.get_untracked();
        let status = |id: &str| ws.iter().find(|w| w.id.to_string() == id).map(|w| w.status);
        if status(&current) == Some(WorkerStatus::Online) {
            return;
        }
        let pick = ws
            .iter()
            .find(|w| w.status == WorkerStatus::Online)
            .or_else(|| (status(&current).is_none()).then(|| ws.iter().find(|w| w.status != WorkerStatus::Revoked)).flatten());
        if let Some(w) = pick {
            worker.set(w.id.to_string());
        }
    }); // Remember the last directory per worker.
    Effect::new(move || {
        let w = worker.get();
        let remembered = storage_get(&format!("c6.cwd.{w}")).unwrap_or_default();
        cwd.set(remembered.clone());
        // Forget a remembered folder that no longer exists there.
        if !remembered.is_empty() {
            spawn_local(async move {
                if api::fs_list(w.clone(), remembered.clone(), false).await.is_err() && cwd.get_untracked() == remembered {
                    cwd.set(String::new());
                }
            });
        }
    });
    Effect::new(move || {
        let (w, c) = (worker.get(), cwd.get());
        labels.set(storage_get(&format!("c6.labels.{w}.{c}")).unwrap_or_default());
    });
    // Default model per connection; offer the provider's model list.
    let models = LocalResource::new(move || {
        let c = connection.get();
        async move { if c.is_empty() { Ok(Vec::new()) } else { api::models(c).await } }
    });
    Effect::new(move || {
        let c = connection.get();
        if c.is_empty() {
            model.set(String::new());
            return;
        }
        let default = conns.with_value(|cs| cs.iter().find(|x| x.id == c).and_then(|x| x.default_model.clone()));
        model.set(storage_get(&format!("c6.model.{c}")).or(default).unwrap_or_default());
    });

    let submit = move || {
        if pending.get_untracked() {
            return;
        }
        let on_ssh = remote.get_untracked();
        let target = ssh.get_untracked().trim().to_owned();
        if on_ssh && target.is_empty() {
            error.set(Some("Enter the SSH host (user@host, or an alias from the worker's ~/.ssh/config)".into()));
            return;
        }
        if !on_ssh && cwd.get_untracked().is_empty() && worktree.get_untracked() {
            error.set(Some("A git worktree needs a folder".into()));
            return;
        }
        let Ok(worker_id) = worker.get_untracked().parse() else {
            error.set(Some("Pick a worker".into()));
            return;
        };
        let req = CreateSession {
            worker_id,
            cwd: if on_ssh { remote_dir.get_untracked().trim().to_owned() } else { cwd.get_untracked() },
            ssh: on_ssh.then(|| target.clone()),
            connection_id: connection.get_untracked(),
            model: model.get_untracked(),
            title: None,
            prompt: Some(prompt.get_untracked()).filter(|p| !p.trim().is_empty()),
            attachments: Vec::new(),
            labels: c6_api::labels::parse_labels(&labels.get_untracked()),
            effort: Some(effort.get_untracked()).filter(|e| !e.is_empty() && effort_supported()),
            worktree: worktree.get_untracked() && !on_ssh,
        };
        storage_set("c6.where", if on_ssh { "ssh" } else { "worker" });
        if on_ssh {
            // Recent targets of this worker, newest first.
            let mut recent = recent_ssh();
            recent.retain(|t| *t != target);
            recent.insert(0, target.clone());
            recent.truncate(10);
            storage_set(&format!("c6.ssh.{}", req.worker_id), &recent.join(","));
        }
        storage_set("c6.worktree", if worktree.get_untracked() { "1" } else { "0" });
        storage_set("c6.effort", &effort.get_untracked());
        storage_set("c6.worker", &req.worker_id.to_string());
        storage_set(&format!("c6.cwd.{}", req.worker_id), &req.cwd);
        storage_set(&format!("c6.labels.{}.{}", req.worker_id, req.cwd), &req.labels.join(", "));
        storage_set("c6.connection", &req.connection_id);
        if !req.connection_id.is_empty() {
            storage_set(&format!("c6.model.{}", req.connection_id), &req.model);
        }
        pending.set(true);
        spawn_local(async move {
            match api::create_session(req).await {
                // Beside the form, so another can start right away.
                Ok(s) => match col {
                    Some(c) => c.open(View::session(s.id), How::Beside),
                    None => ws.open(None, View::session(s.id), How::Beside),
                },
                Err(e) => error.set(Some(e)),
            }
            pending.set(false);
        });
    };
    let submit_click = submit;

    view! {
        {move || picking.get().then(|| view! {
            <crate::files::FolderPicker
                worker_id=worker.get_untracked()
                start=cwd.get_untracked()
                on_select=Callback::new(move |p: String| { cwd.set(p); picking.set(false); })
                on_cancel=Callback::new(move |_| picking.set(false))
            />
        })}
        <section class="space-y-3 rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900/50">
            <div class="grid gap-3 sm:grid-cols-[1fr_2fr_auto]">
                <label class="block text-xs text-zinc-500">
                    "Worker"
                    <select class=format!("mt-1 {INPUT}") on:change=move |ev| { chosen_by_hand.set(true); worker.set(event_target_value(&ev)) } prop:value=move || worker.get()>
                        {move || live.workers.get().into_iter().filter(|w| w.status != WorkerStatus::Revoked).map(|w| {
                            let label = if w.status == WorkerStatus::Online { w.name.clone() } else { format!("{} (offline)", w.name) };
                            view! { <option value=w.id.to_string()>{label}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <div class="block text-xs text-zinc-500 sm:col-span-2">
                    <div class="flex items-center gap-3">
                        <label class="flex items-center gap-1"><input type="radio" name="c6-where" prop:checked=move || !remote.get() on:change=move |_| remote.set(false) />"Folder on the worker"</label>
                        <label class="flex items-center gap-1" title="Work on another machine over SSH, using the worker's keys and ~/.ssh/config">
                            <input type="radio" name="c6-where" prop:checked=move || remote.get() on:change=move |_| remote.set(true) />"SSH host, through the worker"
                        </label>
                    </div>
                    <div class="mt-1 flex gap-2" class:hidden=move || !remote.get()>
                        <input class=format!("{INPUT} min-w-0 flex-1 font-mono") list="c6-ssh-recent" placeholder="user@host or an ssh_config alias"
                            prop:value=move || ssh.get() on:input=move |ev| ssh.set(event_target_value(&ev)) />
                        <datalist id="c6-ssh-recent">
                            {move || inventory.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|h| h.host.enabled).map(|h| {
                                let label = format!("{} · {}", h.host.target, h.host.labels.join(", "));
                                view! { <option value=h.host.name.clone() label=label></option> }
                            }).collect_view()}
                            {move || recent_ssh().into_iter().map(|t| view! { <option value=t></option> }).collect_view()}
                        </datalist>
                        <input class=format!("{INPUT} w-40! font-mono") placeholder="folder there (~)"
                            prop:value=move || remote_dir.get() on:input=move |ev| remote_dir.set(event_target_value(&ev)) />
                    </div>
                    <div class="mt-1 flex gap-2" class:hidden=move || remote.get()>
                        <button class=format!("{INPUT} min-w-0 flex-1 truncate text-left font-mono disabled:opacity-60")
                            disabled=move || !worker_online()
                            title=move || if worker_online() { "Choose a folder on the worker" } else { "The worker is offline" }
                            on:click=move |_| picking.set(true)>
                            {move || if cwd.get().is_empty() { "No folder: a scratch workspace (choose one…)".to_owned() } else { cwd.get() }}
                        </button>
                        {move || (!cwd.get().is_empty()).then(|| view! {
                            <button class="shrink-0 rounded-md px-2 text-xs text-zinc-500 hover:bg-zinc-200/60 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                                title="Work in a scratch workspace instead of a folder" on:click=move |_| cwd.set(String::new())>"✕"</button>
                        })}
                    </div>
                </div>
                <label class="block text-xs text-zinc-500">
                    "Connection"
                    <select class=format!("mt-1 {INPUT}") on:change=move |ev| connection.set(event_target_value(&ev)) prop:value=move || connection.get()>
                        {move || (has_routes.get() || auto()).then(|| view! { <option value="">"Auto (your routing rules)"</option> })}
                        {conns.get_value().into_iter().map(|c| view! { <option value=c.id.clone()>{c.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                {move || auto().then(|| view! {
                    <p class="text-xs text-zinc-500">
                        "The connection, model and effort come from the first rule in Settings → Model routing that matches this session's labels."
                    </p>
                })}
                <label class="block text-xs text-zinc-500" class:hidden=auto>
                    "Model"
                    <input class=format!("mt-1 font-mono {INPUT}") list="c6-models" placeholder="model id (empty: routing rules, else the default)"
                        prop:value=move || model.get() on:input=move |ev| model.set(event_target_value(&ev)) />
                    <datalist id="c6-models">
                        {move || models.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|m| view! { <option value=m></option> }).collect_view()}
                    </datalist>
                </label>
                <label class="block text-xs text-zinc-500" class:hidden=move || !effort_supported()>
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
            <label class="block text-xs text-zinc-500">
                "Labels"
                <input class=format!("mt-1 font-mono {INPUT}") placeholder="optional, e.g. client:acme, spike (repo, folder, worker and model are added automatically)"
                    prop:value=move || labels.get() on:input=move |ev| labels.set(event_target_value(&ev)) />
            </label>
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
                <label class="flex items-center gap-1.5 text-sm text-zinc-600 dark:text-zinc-400" class:hidden=move || remote.get() title="Create a c6/… branch and a separate checkout for this session">
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

/// Runs a template: one session, or a fleet run (then mission control,
/// filtered to the run).
#[component]
fn RunTemplate() -> impl IntoView {
    let (ws, col) = (use_workspace(), use_column());
    let filter = crate::labels::use_filter().0;
    let templates = LocalResource::new(api::templates);
    let chosen = RwSignal::new(String::new());
    let input = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<String, String>>);
    let run = move || {
        let name = chosen.get_untracked();
        if name.is_empty() || pending.get_untracked() {
            return;
        }
        pending.set(true);
        spawn_local(async move {
            match api::run_template(name, input.get_untracked()).await {
                Ok(r) => {
                    let mut note = format!("Started {} session{}", r.sessions.len(), if r.sessions.len() == 1 { "" } else { "s" });
                    if !r.errors.is_empty() {
                        note.push_str(&format!(" ({} could not start: {})", r.errors.len(), r.errors.join("; ")));
                    }
                    result.set(Some(Ok(note)));
                    input.set(String::new());
                    if let [one] = r.sessions.as_slice() {
                        match col {
                            Some(c) => c.open(View::session(one.id.clone()), How::Beside),
                            None => ws.open(None, View::session(one.id.clone()), How::Beside),
                        }
                    } else {
                        filter.set(vec![format!("run:{}", r.run_id)]);
                        ws.open(None, View::Mission, How::Beside);
                    }
                }
                Err(e) => result.set(Some(Err(e))),
            }
            pending.set(false);
        });
    };
    move || {
        let list: Vec<c6_api::Template> =
            templates.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|t| t.enabled).collect();
        (!list.is_empty()).then(|| {
            let descriptions: Vec<(String, String)> = list.iter().map(|t| (t.name.clone(), t.description.clone())).collect();
            let takes_input: Vec<String> = list.iter().filter(|t| t.prompt.contains("{{input}}")).map(|t| t.name.clone()).collect();
            let takes_input = StoredValue::new(takes_input);
            let wants_input = move || takes_input.with_value(|v| v.contains(&chosen.get()));
            view! {
                <section class="space-y-2 rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900/50">
                    <div class="flex flex-wrap items-center gap-2">
                        <span class="text-sm font-medium">"Run a template"</span>
                        <select class=format!("{INPUT} w-auto!") prop:value=move || chosen.get() on:change=move |ev| chosen.set(event_target_value(&ev))>
                            <option value="">"Choose…"</option>
                            {descriptions.iter().map(|(n, d)| view! { <option value=n.clone() title=d.clone()>{n.clone()}</option> }).collect_view()}
                        </select>
                        <input class=format!("{INPUT} min-w-48 flex-1") placeholder=move || if wants_input() { "input for {{input}}" } else { "(this template takes no input)" }
                            prop:disabled=move || !wants_input() prop:value=move || input.get() on:input=move |ev| input.set(event_target_value(&ev))
                            on:keydown=move |ev| if ev.key() == "Enter" { run() } />
                        <button class=BUTTON prop:disabled=move || chosen.get().is_empty() || pending.get() on:click=move |_| run()>"Run"</button>
                    </div>
                    {move || result.get().map(|r| match r {
                        Ok(m) => view! { <p class="text-sm text-emerald-600">{m}</p> }.into_any(),
                        Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    })}
                </section>
            }
        })
    }
}
