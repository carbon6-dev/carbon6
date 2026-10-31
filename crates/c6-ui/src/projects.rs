//! Projects (docs/projects.md): long-running efforts made of work. A panel
//! of its own lists them; a project opens beside the list with its
//! purpose, its work (open first), its documents and links, and the ways to
//! move work in and out.

use c6_api::projects::{ProjectDetach, ProjectItem, ProjectNew, ProjectPatch, state_label};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, View, use_column, use_workspace};

fn since(at: i64) -> String {
    ago(now_ms() - at)
}

fn state_class(state: &str) -> &'static str {
    match state {
        "needs_you" => "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
        "stalled" | "quiet" => "bg-rose-100 text-rose-800 dark:bg-rose-900/40 dark:text-rose-300",
        "moving" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
        "on_track" => "bg-sky-100 text-sky-800 dark:bg-sky-900/40 dark:text-sky-300",
        _ => "bg-zinc-100 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400",
    }
}

#[component]
fn StatePill(state: String) -> impl IntoView {
    view! { <span class=format!("shrink-0 rounded px-1.5 py-0.5 text-[11px] font-medium {}", state_class(&state))>{state_label(&state)}</span> }
}

fn open_work(p: &ProjectItem) -> usize {
    p.work.iter().filter(|w| !matches!(w.state.as_str(), "done" | "dropped")).count()
}

/// The header link, with how many projects need a look.
#[component]
pub fn ProjectsLink(class: &'static str) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    crate::decisions::reload_quietly(tick);
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::projects_list(false)
    });
    view! {
        <Go to=View::Projects { id: String::new() } class=class title="Long-running efforts made of work">
            "Projects"
            {move || list.get().and_then(Result::ok).map(|l| l.iter().filter(|p| matches!(p.state.as_str(), "needs_you" | "stalled")).count()).filter(|n| *n > 0).map(|n| view! {
                <span class="ml-1 rounded-full bg-violet-600 px-1.5 text-[10px] font-semibold text-white">{n}</span>
            })}
        </Go>
    }
}

#[component]
pub fn ProjectsPage() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    crate::decisions::reload_quietly(tick);
    let show_closed = RwSignal::new(false);
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::projects_list(false)
    });
    let closed = LocalResource::new(move || {
        let on = show_closed.get();
        tick.track();
        async move { if on { api::projects_list(true).await } else { Ok(Vec::new()) } }
    });
    let error = RwSignal::new(None::<String>);
    let adding = RwSignal::new(false);
    let (title, purpose) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let (col, ws) = (use_column(), use_workspace());
    let open_project = move |id: String, how: How| match &col {
        Some(c) => c.open(View::Projects { id }, how),
        None => ws.open(None, View::Projects { id }, how),
    };
    let open_project = StoredValue::new(open_project);
    let add = move || {
        let t = title.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        let n = ProjectNew { title: t, purpose: purpose.get_untracked(), ..Default::default() };
        spawn_local(async move {
            match api::project_add(&n).await {
                Ok(p) => {
                    adding.set(false);
                    title.set(String::new());
                    purpose.set(String::new());
                    tick.update(|t| *t += 1);
                    open_project.with_value(|f| f(p.id, How::Beside));
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-5">
            <header class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                <h1 class="text-xl font-semibold">"Projects"</h1>
                <span class="text-sm text-zinc-500">"Long-running efforts: their work, documents and phases"</span>
            </header>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex flex-wrap gap-2">
                <button class=BUTTON on:click=move |_| adding.update(|a| *a = !*a)>"+ New project"</button>
            </div>
            {move || adding.get().then(|| view! {
                <div class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                    <input class=INPUT placeholder="Its name: Realms for the team" prop:value=move || title.get()
                        on:input=move |ev| title.set(event_target_value(&ev)) on:keydown=move |ev| if ev.key() == "Enter" { add() } />
                    <textarea class=format!("{INPUT} min-h-[4rem]") placeholder="What it's for, and what success means"
                        prop:value=move || purpose.get() on:input=move |ev| purpose.set(event_target_value(&ev)) />
                    <div class="flex justify-end gap-2">
                        <button class=BUTTON_QUIET on:click=move |_| adding.set(false)>"Cancel"</button>
                        <button class=BUTTON on:click=move |_| add()>"Start it"</button>
                    </div>
                </div>
            })}
            {move || match list.get() {
                None => view! { <p class="text-sm text-zinc-500">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <ErrorBox message=e /> }.into_any(),
                Some(Ok(l)) if l.is_empty() => view! {
                    <p class="text-sm text-zinc-500">"No projects yet. A project holds the work of an effort that lasts weeks or months, with its plan and other documents."</p>
                }.into_any(),
                Some(Ok(l)) => view! {
                    <div class="space-y-1">
                        {l.into_iter().map(|p| {
                            let id = p.id.clone();
                            let first = p.fields.note.as_ref().and_then(|n| n.text.lines().next().map(str::to_owned));
                            let n = open_work(&p);
                            view! {
                                <button class="block w-full rounded-md border border-zinc-200 px-3 py-2 text-left hover:bg-zinc-50 dark:border-zinc-800 dark:hover:bg-zinc-900"
                                    on:click=move |ev| open_project.with_value(|f| f(id.clone(), How::from_mouse(&ev)))>
                                    <div class="flex items-center gap-2">
                                        <span class="min-w-[6rem] flex-1 truncate font-medium">{p.fields.title.clone()}</span>
                                        <StatePill state=p.state.clone() />
                                        <span class="shrink-0 text-xs text-zinc-400">{since(p.fields.moved_at.max(p.updated_at))}</span>
                                    </div>
                                    <p class="truncate text-xs text-zinc-600 dark:text-zinc-400">{first.unwrap_or_else(|| p.why.clone())}</p>
                                    <p class="text-xs text-zinc-500">{format!("{n} open · {} in all{}", p.work.len(), if p.docs.is_empty() { String::new() } else { format!(" · {} documents", p.docs.len()) })}</p>
                                </button>
                            }
                        }).collect_view()}
                    </div>
                }.into_any(),
            }}
            <div class="pt-2">
                <label class="flex items-center gap-1.5 text-sm text-zinc-500">
                    <input type="checkbox" prop:checked=move || show_closed.get() on:change=move |ev| show_closed.set(event_target_checked(&ev)) />
                    "Show done and dropped projects"
                </label>
                {move || show_closed.get().then(|| view! {
                    <div class="mt-2 space-y-1">
                        {move || closed.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|p| {
                            let id = p.id.clone();
                            view! {
                                <button class="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                    on:click=move |ev| open_project.with_value(|f| f(id.clone(), How::from_mouse(&ev)))>
                                    <StatePill state=p.state.clone() />
                                    <span class="min-w-0 flex-1 truncate">{p.fields.title.clone()}</span>
                                    <span class="text-xs text-zinc-400">{p.fields.closed_at.map(since).unwrap_or_default()}</span>
                                </button>
                            }
                        }).collect_view()}
                    </div>
                })}
            </div>
        </main>
    }
}

/// A project: what it's for, its work, documents and links.
#[component]
pub fn ProjectPage(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    crate::decisions::reload_quietly(tick);
    let loaded = LocalResource::new(move || {
        tick.track();
        api::project_get(id.get_value())
    });
    let shown = RwSignal::new(None::<Result<ProjectItem, String>>);
    Effect::new(move || {
        if let Some(r) = loaded.get() {
            shown.set(Some(r));
        }
    });
    let all_work = LocalResource::new(move || {
        tick.track();
        api::work_list(false)
    });
    let others = LocalResource::new(move || {
        tick.track();
        api::projects_list(false)
    });
    let error = RwSignal::new(None::<String>);
    let apply = move |r: Result<ProjectItem, String>| match r {
        Ok(p) => shown.set(Some(Ok(p))),
        Err(e) => error.set(Some(e)),
    };
    let patch = move |p: ProjectPatch| spawn_local(async move { apply(api::project_patch(id.get_value(), &p).await) });
    let editing = RwSignal::new(false);
    let (title, purpose) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let merging = RwSignal::new(false);
    let save = move || {
        editing.set(false);
        patch(ProjectPatch { title: Some(title.get_untracked()), purpose: Some(purpose.get_untracked()), ..Default::default() });
    };
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-5">
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || match shown.get() {
                None => view! { <p class="text-sm text-zinc-500">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <ErrorBox message=e /> }.into_any(),
                Some(Ok(p)) => {
                    let active = p.fields.status == "active";
                    let closed = matches!(p.fields.status.as_str(), "done" | "dropped");
                    let (t, pu) = (p.fields.title.clone(), p.fields.purpose.clone());
                    let mine = p.id.clone();
                    let open: Vec<_> = p.work.iter().filter(|w| !matches!(w.state.as_str(), "done" | "dropped")).cloned().collect();
                    let done: Vec<_> = p.work.iter().filter(|w| matches!(w.state.as_str(), "done" | "dropped")).cloned().collect();
                    let in_it: Vec<String> = p.work.iter().map(|w| w.id.clone()).collect();
                    view! {
                        <header class="space-y-1">
                            {move || if editing.get() {
                                view! {
                                    <div class="space-y-2">
                                        <input class=INPUT prop:value=move || title.get() on:input=move |ev| title.set(event_target_value(&ev)) />
                                        <textarea class=format!("{INPUT} min-h-[4rem]") placeholder="What it's for, and what success means"
                                            prop:value=move || purpose.get() on:input=move |ev| purpose.set(event_target_value(&ev)) />
                                        <div class="flex justify-end gap-2">
                                            <button class=BUTTON_QUIET on:click=move |_| editing.set(false)>"Cancel"</button>
                                            <button class=BUTTON on:click=move |_| save()>"Save"</button>
                                        </div>
                                    </div>
                                }.into_any()
                            } else {
                                let (t2, p2) = (t.clone(), pu.clone());
                                view! {
                                    <div class="flex items-start gap-2">
                                        <h1 class="min-w-0 flex-1 text-xl font-semibold">{t.clone()}</h1>
                                        <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| { title.set(t2.clone()); purpose.set(p2.clone()); editing.set(true); }>"Edit"</button>
                                    </div>
                                    <p class="whitespace-pre-wrap text-sm text-zinc-600 dark:text-zinc-400">{if pu.is_empty() { "Purpose: not said yet".to_owned() } else { pu.clone() }}</p>
                                }.into_any()
                            }}
                            <div class="flex flex-wrap items-center gap-2 text-sm">
                                <StatePill state=p.state.clone() />
                                <span class="text-zinc-600 dark:text-zinc-400">{p.why.clone()}</span>
                            </div>
                        </header>
                        <div class="flex flex-wrap gap-2">
                            {active.then(|| view! {
                                <button class=BUTTON_QUIET on:click=move |_| patch(ProjectPatch { status: Some("paused".into()), ..Default::default() })>"Pause"</button>
                                <button class=BUTTON_QUIET on:click=move |_| patch(ProjectPatch { status: Some("done".into()), ..Default::default() })>"Done"</button>
                                <button class=BUTTON_QUIET on:click=move |_| patch(ProjectPatch { status: Some("dropped".into()), ..Default::default() })>"Drop"</button>
                                <button class=BUTTON_QUIET on:click=move |_| merging.update(|m| *m = !*m)>"Merge…"</button>
                            })}
                            {(!active).then(|| view! {
                                <button class=BUTTON on:click=move |_| patch(ProjectPatch { status: Some("active".into()), ..Default::default() })>
                                    {if closed { "Reopen" } else { "Resume" }}
                                </button>
                            })}
                        </div>
                        {move || merging.get().then(|| {
                            let me = mine.clone();
                            view! {
                                <div class="flex items-center gap-2 text-sm">
                                    <span class="text-zinc-500">"Merge into this:"</span>
                                    <select class=format!("{INPUT} w-auto!") on:change=move |ev| {
                                        let from = event_target_value(&ev);
                                        if from.is_empty() { return; }
                                        merging.set(false);
                                        spawn_local(async move { apply(api::project_merge(id.get_value(), from).await) });
                                    }>
                                        <option value="">"Choose a project…"</option>
                                        {others.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|o| o.id != me).map(|o| view! {
                                            <option value=o.id.clone()>{o.fields.title.clone()}</option>
                                        }).collect_view()}
                                    </select>
                                </div>
                            }
                        })}
                        <section class="space-y-1">
                            <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">{format!("Work ({} open)", open.len())}</h2>
                            {open.into_iter().map(|w| view! { <WorkRow w project=id.get_value() apply=Callback::new(apply) /> }).collect_view()}
                            <div class="flex items-center gap-2 pt-1 text-sm">
                                <select class=format!("{INPUT} w-auto! max-w-full") on:change=move |ev| {
                                    let work = event_target_value(&ev);
                                    if work.is_empty() { return; }
                                    spawn_local(async move { apply(api::project_work(id.get_value(), work, String::new()).await) });
                                }>
                                    <option value="">"+ Bring in open work…"</option>
                                    {move || all_work.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|w| !in_it.contains(&w.id)).map(|w| view! {
                                        <option value=w.id.clone()>{if w.project_title.is_empty() { w.fields.title.clone() } else { format!("{} (in {})", w.fields.title, w.project_title) }}</option>
                                    }).collect_view()}
                                </select>
                            </div>
                        </section>
                        {(!p.docs.is_empty()).then(|| view! {
                            <section class="space-y-1">
                                <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">"Documents"</h2>
                                {p.docs.iter().map(|d| view! {
                                    <div class="flex items-center gap-2 px-2 py-1 text-sm">
                                        <Go to=View::Docs { id: d.doc.clone() } class="min-w-0 flex-1 truncate text-sky-700 hover:underline dark:text-sky-400">
                                            {format!("{} {}", if d.kind == "deck" { "▦" } else { "▤" }, d.title)}
                                        </Go>
                                        {(!d.role.is_empty()).then(|| view! { <span class="text-xs text-zinc-500">{d.role.clone()}</span> })}
                                    </div>
                                }).collect_view()}
                            </section>
                        })}
                        {(!done.is_empty()).then(|| view! {
                            <section class="space-y-1">
                                <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">"Closed work"</h2>
                                {done.into_iter().map(|w| view! {
                                    <div class="flex items-center gap-2 px-2 py-0.5 text-sm text-zinc-500">
                                        <StatePill state=w.state.clone() />
                                        <Go to=View::Work { id: w.id.clone() } class="min-w-0 flex-1 truncate hover:underline">{w.title.clone()}</Go>
                                        <span class="text-xs text-zinc-400">{w.closed_at.map(since).unwrap_or_default()}</span>
                                    </div>
                                }).collect_view()}
                            </section>
                        })}
                    }.into_any()
                }
            }}
        </main>
    }
}

/// A piece of open work in the project, with a way out.
#[component]
fn WorkRow(w: c6_api::projects::ProjectWork, project: String, apply: Callback<Result<ProjectItem, String>>) -> impl IntoView {
    let wid = w.id.clone();
    view! {
        <div class="flex items-center gap-2 rounded px-2 py-1 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900">
            <span class=format!("shrink-0 rounded px-1.5 py-0.5 text-[11px] font-medium {}", state_class(if w.state == "needs_you" { "needs_you" } else if w.state == "moving" { "moving" } else { "" }))>
                {c6_api::work::state_label(&w.state)}
            </span>
            <span class="min-w-[8rem] flex-1 truncate">
                <Go to=View::Work { id: w.id.clone() } class="text-violet-700 hover:underline dark:text-violet-400" title=w.why.clone()>{w.title.clone()}</Go>
                {(!w.why.is_empty()).then(|| view! { <span class="ml-2 text-xs text-zinc-500">{w.why.clone()}</span> })}
            </span>
            <span class="shrink-0 text-xs text-zinc-400">{(w.moved_at > 0).then(|| since(w.moved_at))}</span>
            <button class="shrink-0 text-xs text-zinc-400 hover:text-red-600" title="Take it out of the project (it stays in your work)"
                on:click=move |_| {
                    let (p, d) = (project.clone(), ProjectDetach { work: Some(wid.clone()), ..Default::default() });
                    spawn_local(async move { apply.run(api::project_detach(p, &d).await) });
                }>"✕"</button>
        </div>
    }
}
