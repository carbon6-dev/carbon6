//! Decisions (docs/os.md §4.2): every time carbon6 needs you, in one list —
//! agents' questions, approvals of tool calls and workflow steps, the
//! assistant's questions, facts to confirm, disputes. Answer one here
//! (an option, a form, a note), snooze or dismiss it, or start a session
//! that helps you settle it.

use c6_api::Decision;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, View, use_workspace};

/// The header link, with how many are open.
#[component]
pub fn DecisionLink(class: &'static str) -> impl IntoView {
    let live = api::live();
    let tick = ticking();
    let count = LocalResource::new(move || {
        tick.track();
        live.sessions.track();
        api::decision_count()
    });
    view! {
        <Go to=View::Decisions class=class>
            "Decisions"
            {move || count.get().and_then(Result::ok).filter(|n| *n > 0).map(|n| view! {
                <span class="ml-1 rounded-full bg-sky-600 px-1.5 text-[10px] font-semibold text-white">{n}</span>
            })}
        </Go>
    }
}

/// A tick every 30 seconds while the view that asks lives: what waits on you
/// (a decision, something in another realm) can change without the live
/// feed saying so.
pub(crate) fn ticking() -> RwSignal<u32> {
    let tick = RwSignal::new(0u32);
    let handle = set_interval_with_handle(move || tick.update(|t| *t += 1), std::time::Duration::from_secs(30)).ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    tick
}

/// How long a change in the live feed waits before the list loads again.
const QUIET: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether you're typing (a field has the focus): a reload waits.
/// You're typing in a page that reloads quietly: a decision, or a page
/// marked `data-hold-reloads` (a project, a work item, their lists). A reload
/// would rebuild what you're typing in and take the focus from it. A field
/// elsewhere (a session's message box in another tile, say) doesn't hold
/// reloads.
fn typing() -> bool {
    document().active_element().is_some_and(|el| {
        (matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT") || el.has_attribute("contenteditable"))
            && el.closest("[data-decisions], [data-hold-reloads]").ok().flatten().is_some()
    })
}

/// Loads a list again (`tick`) once sessions change, which may mean its
/// decisions did (the live feed says only that sessions did): at most once
/// every 10 seconds, and not while you type.
pub(crate) fn reload_quietly(tick: RwSignal<u32>) {
    let live = api::live();
    let due = StoredValue::new(false);
    Effect::new(move |seen: Option<()>| {
        live.sessions.track();
        if seen.is_some() {
            later(due, tick);
        }
    });
}

/// Loads again now (`tick`), or once you stop typing in the page: for a
/// page's own timer, which must not rebuild what you're typing in either.
pub(crate) fn tick_quietly(tick: RwSignal<u32>, due: StoredValue<bool>) {
    if typing() { later(due, tick) } else { tick.update(|t| *t += 1) }
}

fn later(due: StoredValue<bool>, tick: RwSignal<u32>) {
    if due.get_value() {
        return;
    }
    due.set_value(true);
    set_timeout(
        move || {
            // Not once the view that asked has gone.
            if due.try_set_value(false).is_some() {
                return;
            }
            if typing() { later(due, tick) } else { tick.update(|t| *t += 1) }
        },
        QUIET,
    );
}

/// Decisions as they're shown: kept while the next load comes (so nothing
/// blanks and the page doesn't jump), each card by its id and version, so
/// an unchanged card keeps what you typed in it.
#[component]
fn Cards(list: LocalResource<Result<Vec<Decision>, String>>, refresh: Callback<()>, #[prop(optional)] in_session: bool) -> impl IntoView {
    let shown = RwSignal::new(None::<Result<Vec<Decision>, String>>);
    Effect::new(move || {
        if let Some(r) = list.get() {
            shown.set(Some(r));
        }
    });
    // In order, each group's decisions together under its name.
    let groups = Memo::new(move |_| {
        let mut groups: Vec<(String, Vec<Decision>)> = Vec::new();
        for d in shown.get().and_then(Result::ok).unwrap_or_default() {
            match groups.iter_mut().find(|(g, _)| *g == d.group) {
                Some((_, ds)) => ds.push(d),
                None => groups.push((d.group.clone(), vec![d])),
            }
        }
        groups
    });
    // Which of loading, an error, none, or some: what changes the frame.
    let state = Memo::new(move |_| match shown.get() {
        None => 0,
        Some(Err(_)) => 1,
        Some(Ok(v)) if v.is_empty() => 2,
        Some(Ok(_)) => 3,
    });
    // Within a session: only what waits, under its heading.
    move || {
        match state.get() {
            1 if !in_session => view! { <ErrorBox message=shown.get_untracked().and_then(Result::err).unwrap_or_default() /> }.into_any(),
            2 if !in_session => view! { <p class="text-sm text-muted">"Nothing here."</p> }.into_any(),
            3 => view! {
                <div class=if in_session { "space-y-2" } else { "space-y-4" } data-decisions="">
                {waiting_heading(in_session, groups)}
                <For each=move || groups.get() key=|(g, _)| g.clone() let:group>
                    <div class="space-y-2">
                        {(!group.0.is_empty()).then(|| {
                            let g = group.0.clone();
                            let n = move || groups.with(|gs| gs.iter().find(|(x, _)| *x == g).map_or(0, |(_, ds)| ds.len()));
                            let name = group.0.clone();
                            view! { <h2 class="text-xs font-medium uppercase tracking-wide text-muted">{move || format!("{name} ({})", n())}</h2> }
                        })}
                        {
                            let g = group.0.clone();
                            view! {
                                <For each=move || groups.with(|gs| gs.iter().find(|(x, _)| *x == g).map(|(_, ds)| ds.clone()).unwrap_or_default())
                                    key=|d| (d.id, d.updated_at) let:d>
                                    <DecisionCard d refresh in_session />
                                </For>
                            }
                        }
                    </div>
                </For>
                </div>
            }
            .into_any(),
            _ => ().into_any(),
        }
    }
}

/// Task offers have their own heading; they are not waiting for an answer.
fn waiting_heading(in_session: bool, groups: Memo<Vec<(String, Vec<Decision>)>>) -> impl IntoView {
    view! {
        <Show when=move || in_session && groups.with(|gs| gs.iter().any(|(_, ds)| ds.iter().any(|d| d.kind != "task")))>
            <p class="text-xs font-medium uppercase tracking-wide text-muted">"Waiting for you"</p>
        </Show>
    }
}

#[component]
pub fn DecisionsPage() -> impl IntoView {
    let state = RwSignal::new("open".to_owned());
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        let s = state.get();
        let realm = api::realm().get();
        async move { api::decisions(s).await.map(|ds| ds.into_iter().filter(|d| realm.is_empty() || d.realm == realm).collect::<Vec<_>>()) }
    });
    reload_quietly(tick);
    let refresh = Callback::new(move |()| tick.update(|t| *t += 1));
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-6">
            <div class="flex flex-wrap items-center gap-2">
                <h1 class="text-lg font-semibold">"Decisions"</h1>
                <span class="flex-1 text-sm text-muted">"Everything that needs you. What something waits on comes first; agents that asked without stopping carry on, and get your answer as a message."</span>
                <select class=format!("{INPUT} w-auto py-1 text-sm") prop:value=move || state.get() on:change=move |ev| state.set(event_target_value(&ev))>
                    <option value="open">"Open"</option>
                    <option value="snoozed">"Snoozed"</option>
                    <option value="answered">"Answered"</option>
                    <option value="expired">"Expired"</option>
                    <option value="dismissed">"Dismissed"</option>
                    <option value="withdrawn">"Withdrawn"</option>
                </select>
            </div>
            <Cards list refresh />
        </main>
    }
}

/// A session's open decisions, answered where you are (Decisions lists
/// them too). Reloads as sessions change (at most every 10 seconds), and
/// every 30 seconds, not while you type.
#[component]
pub fn SessionDecisions(session: String) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let handle = set_interval_with_handle(
        move || {
            if !typing() {
                tick.update(|t| *t += 1)
            }
        },
        std::time::Duration::from_secs(30),
    )
    .ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    let session = StoredValue::new(session);
    let live = api::live();
    let due = StoredValue::new(false);
    Effect::new(move |seen: Option<()>| {
        live.attention.track();
        live.connected.track();
        if seen.is_some() {
            tick_quietly(tick, due);
        }
    });
    let list = LocalResource::new(move || {
        tick.track();
        api::session_decisions(session.get_value())
    });
    reload_quietly(tick);
    let refresh = Callback::new(move |()| tick.update(|t| *t += 1));
    view! { <Cards list refresh in_session=true /> }
}

pub(crate) fn card_id(id: i64) -> String {
    format!("decision-{id}")
}

#[component]
fn DecisionCard(d: Decision, refresh: Callback<()>, #[prop(optional)] in_session: bool) -> impl IntoView {
    let ws = use_workspace();
    let id = d.id;
    let note = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let values: Vec<(c6_api::FormField, RwSignal<String>)> = d.form.iter().map(|f| (f.clone(), RwSignal::new(String::new()))).collect();
    let values = StoredValue::new(values);
    let set = move |body: serde_json::Value| {
        spawn_local(async move {
            match api::edit_decision(id, body).await {
                Ok(_) => refresh.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let answer = move |option: Option<String>| {
        let mut fields = serde_json::Map::new();
        for (f, v) in values.get_value() {
            let text = v.get_untracked();
            if !text.trim().is_empty() {
                fields.insert(f.name.clone(), json!(text));
            }
        }
        let a = c6_api::DecisionAnswer { option, values: fields, note: note.get_untracked() };
        busy.set(true);
        spawn_local(async move {
            match api::answer_decision(id, a).await {
                Ok(_) => refresh.run(()),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let start = move |_| {
        busy.set(true);
        spawn_local(async move {
            match api::decision_session(id).await {
                Ok(s) => {
                    ws.open(None, View::session(s.id), How::Beside);
                    refresh.run(());
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let facts: Vec<String> =
        d.context["facts"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let things: Vec<String> =
        d.context["things"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let session = d.context["session"].as_str().map(str::to_owned);
    let task_session = StoredValue::new(session.clone());
    let open = matches!(d.state.as_str(), "open" | "snoozed");
    let tone = match (d.waiting && open, d.priority.as_str()) {
        (true, _) => "border-sky-300 dark:border-sky-900",
        (_, "high") => "border-amber-300 dark:border-amber-900",
        _ => "border-zinc-200 dark:border-zinc-800",
    };
    let has_form = !d.form.is_empty();
    let deadline = d.deadline.filter(|_| open).map(|t| {
        let left = t - now_ms();
        let default = d.default_answer.clone().map(|a| format!(" (then: {a})")).unwrap_or_else(|| " (then: no)".into());
        if left > 0 { format!("answer within {}{default}", ago(left).trim_end_matches(" ago")) } else { "due now".to_owned() }
    });
    let answered = d.answer.as_ref().map(|a| {
        let mut parts: Vec<String> = a["option"].as_str().map(str::to_owned).into_iter().collect();
        if let Some(vs) = a["values"].as_object() {
            parts.extend(vs.iter().map(|(k, v)| format!("{k}: {}", v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))));
        }
        format!("{} {}", parts.join(" · "), if d.answered_via.is_empty() { String::new() } else { format!("(via {})", d.answered_via) })
    });
    view! {
        <section id=card_id(id) class=format!("space-y-2 rounded-lg border p-3 {tone}")>
            <div class="flex flex-wrap items-center gap-2">
                <span class="rounded bg-zinc-100 px-1.5 text-[11px] dark:bg-zinc-800">{d.kind.clone()}</span>
                {(d.waiting && open).then(|| view! { <span class="rounded bg-sky-100 px-1.5 text-[11px] text-sky-800 dark:bg-sky-950 dark:text-sky-300">"waiting"</span> })}
                <h2 class="min-w-0 flex-1 font-medium">{d.title.clone()}</h2>
                {(d.count > 1).then(|| view! { <span class="text-xs text-muted">{format!("raised {}×", d.count)}</span> })}
                <span class="text-xs text-muted">{ago(now_ms() - d.updated_at)}</span>
            </div>
            {(!d.document.is_empty()).then(|| view! { <div class="md text-sm text-zinc-700 dark:text-zinc-300" on:click=crate::mentions::link_clicks() inner_html=crate::markdown::to_html(&d.document)></div> })}
            // A task card: exactly what the new session would be told.
            {(d.kind == "task").then(|| d.context["prompt"].as_str().map(str::to_owned)).flatten().map(|p| view! {
                <details class="text-xs">
                    <summary class="cursor-pointer select-none text-muted">"What the new session is told"</summary>
                    <pre class="mt-1 max-h-64 overflow-auto whitespace-pre-wrap rounded bg-zinc-50 p-2 font-mono text-[11px] dark:bg-zinc-900">{p}</pre>
                </details>
            })}
            <div class="flex flex-wrap items-center gap-2 text-xs text-muted">
                {(!in_session).then(|| view! { <span>{format!("from {}", d.source)}</span> })}
                {session.filter(|_| !in_session).map(|s| view! { <Go to=View::session(s) class="text-sky-700 hover:underline dark:text-sky-400">"the session ↗"</Go> })}
                {things.into_iter().map(|k| view! { <span class="rounded bg-violet-100 px-1 font-mono text-violet-800 dark:bg-violet-950 dark:text-violet-300">{k}</span> }).collect_view()}
                {d.sessions.iter().map(|s| view! { <Go to=View::session(s.clone()) class="text-sky-700 hover:underline dark:text-sky-400">"working session ↗"</Go> }).collect_view()}
                {deadline.map(|t| view! { <span class="text-amber-700 dark:text-amber-400">{t}</span> })}
            </div>
            {(!facts.is_empty()).then(|| view! { <FactList ids=facts on_change=refresh /> })}
            {answered.map(|a| view! { <p class="text-sm text-emerald-700 dark:text-emerald-400">{format!("Answered: {a}")}</p> })}
            {(!d.note.is_empty()).then(|| view! { <p class="text-sm text-zinc-600 dark:text-zinc-400">{format!("Note: {}", d.note)}</p> })}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {open.then(|| if d.kind == "task" {
                let live = api::live();
                let proposing = Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| Some(&s.id) == task_session.read_value().as_ref()).cloned()));
                let session_title = Signal::derive(move || proposing.get().map(|s| s.title).filter(|t| !t.is_empty())
                    .or_else(|| task_session.get_value()).unwrap_or_else(|| "the proposing session".into()));
                let work_title = Signal::derive(move || proposing.get().and_then(|s| s.work).map(|w| w.title));
                view! { <TaskActions busy session_title work_title realm=d.realm.clone() answer=Callback::new(move |o| answer(Some(o))) /> }.into_any()
            } else { view! {
                {values.get_value().into_iter().map(|(f, v)| {
                    let label = format!("{}{}", if f.label.is_empty() { f.name.clone() } else { f.label.clone() }, if f.required { " *" } else { "" });
                    let input = if !f.choices.is_empty() {
                        let choices = f.choices.iter().map(|c| (c.clone(), c.clone())).collect();
                        view! { <crate::util::Choices choices value=v optional=true /> }.into_any()
                    } else if f.kind == "long" {
                        view! { <textarea class=format!("{INPUT} h-20 text-sm") prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev))></textarea> }.into_any()
                    } else {
                        view! { <input class=INPUT prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev)) /> }.into_any()
                    };
                    view! { <label class="block space-y-1"><span class="text-xs text-muted">{label}</span>{input}</label> }
                }).collect_view()}
                <input class=format!("{INPUT} py-1 text-sm") placeholder="A note with your answer (optional)"
                    prop:value=move || note.get() on:input=move |ev| note.set(event_target_value(&ev)) />
                <div class="flex flex-wrap items-center gap-2">
                    {d.options.iter().map(|o| {
                        let label = o.label.clone();
                        view! {
                            <button class=BUTTON title=o.does.clone() disabled=move || busy.get() on:click=move |_| answer(Some(label.clone()))>{o.label.clone()}</button>
                        }
                    }).collect_view()}
                    {(d.options.is_empty()).then(|| view! {
                        <button class=BUTTON disabled=move || busy.get() on:click=move |_| answer(None)>{if has_form { "Send" } else { "Settled" }}</button>
                    })}
                    {(!d.waiting && d.kind != "task").then(|| view! {
                        <button class=BUTTON_QUIET disabled=move || busy.get() on:click=start title="A session that helps you settle it, with its context">"Work on it"</button>
                        <button class=BUTTON_QUIET on:click=move |_| set(json!({"state": "snoozed", "snooze_until": now_ms() + 86_400_000}))>"Tomorrow"</button>
                        <button class=format!("{BUTTON_QUIET} text-muted") on:click=move |_| set(json!({"state": "dismissed"}))>"Dismiss"</button>
                    })}
                </div>
            }.into_any() })}
            {(!open && d.state == "dismissed").then(|| view! { <button class=BUTTON_QUIET on:click=move |_| set(json!({"state": "open"}))>"Reopen"</button> })}
        </section>
    }
}

/// Task labels are presentation only: keep the existing answers understood by
/// the head, including cards raised before the controls were restyled.
#[derive(Clone, Copy)]
enum TaskAction {
    Start,
    Here,
    Save,
    SaveNew,
    NotNow,
}

impl TaskAction {
    fn option(self) -> &'static str {
        match self {
            Self::Start => "Start it",
            Self::Here => "Do it here",
            Self::Save => "Add to this work",
            Self::SaveNew => "Add as new work",
            Self::NotNow => "Not now",
        }
    }
}

// An inset outline keeps secondary buttons exactly as tall as the primary.
const TASK_SECONDARY: &str = "ring-1 ring-inset ring-zinc-300 disabled:opacity-50 dark:ring-zinc-600";

/// One wrapping action row in the transcript and Decisions. The uncommon
/// destination picker lives in a portal, outside the tile and composer.
#[component]
fn TaskActions(
    busy: RwSignal<bool>,
    session_title: Signal<String>,
    work_title: Signal<Option<String>>,
    realm: String,
    answer: Callback<String>,
) -> impl IntoView {
    let chooser = RwSignal::new(false);
    let realm = StoredValue::new(realm);
    let trigger = NodeRef::<leptos::html::Button>::new();
    let close = Callback::new(move |()| {
        chooser.set(false);
        if let Some(button) = trigger.get() {
            let _ = button.focus();
        }
    });
    view! {
        {task_actions(busy, session_title, work_title, answer, chooser, trigger)}
        <Show when=move || chooser.get()>
            <TaskWorkChooser busy realm=realm.get_value() answer=Callback::new(move |option| {
                close.run(());
                answer.run(option);
            }) close />
        </Show>
    }
}

fn task_actions(
    busy: RwSignal<bool>,
    session_title: Signal<String>,
    work_title: Signal<Option<String>>,
    answer: Callback<String>,
    chooser: RwSignal<bool>,
    trigger: NodeRef<leptos::html::Button>,
) -> impl IntoView {
    let act = move |action: TaskAction| answer.run(action.option().into());
    let save_title = move || {
        work_title
            .get()
            .map(|w| format!("A to-do in “{w}”, to start later"))
            .unwrap_or_else(|| "A to-do in the session's work, to start later (new work if it belongs to none)".into())
    };
    view! {
        <div class="flex flex-wrap items-center gap-2">
            <button class=format!("{BUTTON} pointer-coarse:min-h-11") disabled=move || busy.get()
                title="A new session does it, with the prompt shown above"
                on:click=move |_| act(TaskAction::Start)>"Start session"</button>
            <button class=format!("{BUTTON_QUIET} {TASK_SECONDARY} pointer-coarse:min-h-11") disabled=move || busy.get()
                title=move || format!("The session that proposed it, {}, does it after what it's doing now", session_title.get())
                on:click=move |_| act(TaskAction::Here)>"Do it here"</button>
            <button class=format!("{BUTTON_QUIET} {TASK_SECONDARY} pointer-coarse:min-h-11") disabled=move || busy.get()
                title=save_title on:click=move |_| act(TaskAction::Save)>"Save for later"</button>
            <button class=format!("{BUTTON_QUIET} text-muted pointer-coarse:min-h-11") disabled=move || busy.get()
                title="Put this card away" on:click=move |_| act(TaskAction::NotNow)>"Not now"</button>
            <button node_ref=trigger class="rounded-md px-1 py-1.5 text-xs text-muted hover:underline disabled:opacity-50 pointer-coarse:min-h-11"
                disabled=move || busy.get() aria-haspopup="dialog" aria-expanded=move || chooser.get().to_string()
                on:click=move |_| chooser.set(true)>"Save elsewhere…"</button>
        </div>
    }
}

#[component]
fn TaskWorkChooser(busy: RwSignal<bool>, realm: String, answer: Callback<String>, close: Callback<()>) -> impl IntoView {
    let realm = StoredValue::new(realm);
    let works = LocalResource::new(move || api::task_work_list(realm.get_value()));
    let query = RwSignal::new(String::new());
    let search = NodeRef::<leptos::html::Input>::new();
    Effect::new(move || {
        if let Some(input) = search.get() {
            let _ = input.focus();
        }
    });
    let rows =
        Signal::derive(move || task_work_options(&works.get().and_then(Result::ok).unwrap_or_default(), &realm.get_value(), &query.get()));
    view! {
        <leptos::portal::Portal>
            <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" on:click=move |_| close.run(())
                on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); close.run(()); }>
                <div role="dialog" aria-modal="true" aria-label="Save elsewhere"
                    class="flex max-h-[80dvh] w-full max-w-lg flex-col gap-3 overflow-hidden rounded-lg bg-white p-4 text-left shadow-xl dark:bg-zinc-900"
                    on:click=|ev| ev.stop_propagation()>
                    <div class="flex shrink-0 items-center gap-2">
                        <h2 class="flex-1 font-medium">"Save elsewhere"</h2>
                        <button class=format!("{BUTTON_QUIET} pointer-coarse:min-h-11") aria-label="Close" on:click=move |_| close.run(())>"✕"</button>
                    </div>
                    <input node_ref=search class=format!("{INPUT} shrink-0") type="search" aria-label="Search open work" placeholder="Search open work…"
                        prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
                    <div class="min-h-0 overflow-y-auto">
                        {task_work_choices(busy, rows, answer)}
                        <Suspense fallback=|| view! { <p class="p-2 text-sm text-muted">"Loading work…"</p> }>
                            {move || works.get().map(|r| match r {
                                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                                Ok(_) => view! { <Show when=move || rows.get().is_empty()><p class="p-2 text-sm text-muted">"No matching open work."</p></Show> }.into_any(),
                            })}
                        </Suspense>
                    </div>
                </div>
            </div>
        </leptos::portal::Portal>
    }
}

fn task_work_options(works: &[c6_api::work::WorkItem], realm: &str, query: &str) -> Vec<(String, String)> {
    let query = query.trim().to_lowercase();
    works
        .iter()
        .filter(|w| w.realm == realm && w.fields.status == "open" && w.fields.title.to_lowercase().contains(&query))
        .map(|w| (format!("Add to work:{}", w.id), w.fields.title.clone()))
        .collect()
}

fn task_work_choices(busy: RwSignal<bool>, rows: Signal<Vec<(String, String)>>, answer: Callback<String>) -> impl IntoView {
    let class = "block w-full break-words rounded-md px-3 py-2 text-left text-sm hover:bg-zinc-100 disabled:opacity-50 pointer-coarse:min-h-11 dark:hover:bg-zinc-800";
    view! {
        <button class=class disabled=move || busy.get() on:click=move |_| answer.run(TaskAction::SaveNew.option().into())>"New work"</button>
        <For each=move || rows.get() key=|(option, _)| option.clone() let:row>
            <button class=class disabled=move || busy.get() on:click=move |_| answer.run(row.0.clone())>{row.1}</button>
        </For>
    }
}

/// Facts (by id) with what to do about each: confirm, retire, correct.
#[component]
pub fn FactList(ids: Vec<String>, #[prop(optional)] on_change: Option<Callback<()>>) -> impl IntoView {
    let ids = StoredValue::new(ids);
    let tick = RwSignal::new(0u32);
    let facts = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<c6_api::Fact>>("memex.knowledge.get", json!({"ids": ids.get_value()}))
    });
    let changed = move || {
        tick.update(|t| *t += 1);
        if let Some(c) = on_change {
            c.run(());
        }
    };
    view! {
        <Suspense fallback=|| ()>
            {move || facts.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(fs) => view! {
                    <ul class="space-y-2">{fs.into_iter().map(|f| view! { <FactRow f on_change=Callback::new(move |()| changed()) /> }).collect_view()}</ul>
                }.into_any(),
            })}
        </Suspense>
    }
}

pub fn status_tone(status: &str) -> &'static str {
    match status {
        "reviewed" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        "verified" => "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
        "observed" => "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
        "inferred" => "bg-violet-100 text-violet-800 dark:bg-violet-950 dark:text-violet-300",
        "disputed" => "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
        _ => "bg-zinc-100 text-muted line-through dark:bg-zinc-900",
    }
}

#[component]
pub fn FactRow(f: c6_api::Fact, on_change: Callback<()>) -> impl IntoView {
    let id = StoredValue::new(f.id.clone());
    let fixing = RwSignal::new(false);
    let text = RwSignal::new(f.claim.clone());
    let error = RwSignal::new(None::<String>);
    let send = move |body: serde_json::Value| {
        spawn_local(async move {
            let mut b = body;
            b["id"] = json!(id.get_value());
            match api::rpc("memex.knowledge.set", b).await {
                Ok(_) => on_change.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let current = f.valid_to.is_none();
    let last = f.evidence.last().cloned().unwrap_or_default();
    view! {
        <li class="space-y-1 rounded-md bg-zinc-50 p-2 text-sm dark:bg-zinc-900">
            <div class="flex flex-wrap items-center gap-2">
                <span class=format!("rounded px-1.5 text-[11px] {}", status_tone(&f.status))>{f.status.clone()}</span>
                <span class="text-[11px] text-muted">{f.volatility.clone()}{f.due.then_some(" · due")}</span>
                {(!f.labels.is_empty()).then(|| view! { <span class="text-[11px] text-muted">{format!("only {}", f.labels.join(", "))}</span> })}
            </div>
            <p>{f.negative.then_some("NOT: ")}{f.claim.clone()}</p>
            {(!f.conditions.is_empty()).then(|| view! { <p class="text-xs text-muted">{format!("when: {}", f.conditions)}</p> })}
            {f.check.clone().map(|c| view! { <p class="font-mono text-[11px] text-muted">{format!("check: {}", c.command)}</p> })}
            {(!last.is_null()).then(|| {
                let what = [last["command"].as_str(), last["source"].as_str()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                view! {
                    <details class="text-xs text-muted">
                        <summary class="cursor-pointer">{format!("evidence ({}) {}", f.evidence.len(), last["by"].as_str().unwrap_or(""))}</summary>
                        <p class="font-mono">{what}</p>
                        <pre class="max-h-40 overflow-auto whitespace-pre-wrap">{last["excerpt"].as_str().unwrap_or("").to_owned()}</pre>
                    </details>
                }
            })}
            {move || error.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
            {current.then(|| view! {
                <div class="flex flex-wrap gap-2 text-xs">
                    {(f.status != "reviewed").then(|| view! { <button class="text-emerald-700 hover:underline dark:text-emerald-400" on:click=move |_| send(json!({"status": "reviewed"}))>"Confirm"</button> })}
                    <button class="text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| fixing.update(|x| *x = !*x)>"Correct"</button>
                    <button class="text-red-600 hover:underline" on:click=move |_| send(json!({"status": "retired"}))>"No longer true"</button>
                </div>
            })}
            {move || fixing.get().then(|| view! {
                <div class="flex gap-2">
                    <input class=format!("{INPUT} min-w-0 flex-1 py-1 text-sm") prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev)) />
                    <button class=BUTTON on:click=move |_| send(json!({"claim": text.get_untracked()}))>"Save"</button>
                </div>
            })}
        </li>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_heading_requires_a_non_task_decision_in_the_session() {
        let html = |in_session: bool, kinds: &[&str]| {
            let owner = Owner::new();
            owner.with(|| {
                untrack(|| {
                    let decisions: Vec<Decision> = kinds.iter().map(|k| Decision { kind: (*k).into(), ..Default::default() }).collect();
                    let groups = Memo::new(move |_| vec![("Suggested tasks".into(), decisions.clone())]);
                    waiting_heading(in_session, groups).to_html()
                })
            })
        };
        for kinds in [vec![], vec!["task"], vec!["task", "task"]] {
            assert!(!html(true, &kinds).contains("Waiting for you"));
        }
        for kinds in [vec!["question"], vec!["task", "approve"], vec!["confirm", "task"]] {
            assert!(html(true, &kinds).contains("Waiting for you"));
            assert!(!html(false, &kinds).contains("Waiting for you"));
        }
    }

    #[test]
    fn task_actions_keep_the_existing_head_answers() {
        for (action, expected) in [
            (TaskAction::Start, "Start it"),
            (TaskAction::Here, "Do it here"),
            (TaskAction::Save, "Add to this work"),
            (TaskAction::SaveNew, "Add as new work"),
            (TaskAction::NotNow, "Not now"),
        ] {
            assert_eq!(action.option(), expected);
        }
    }

    fn actions_html(busy: bool, work: Option<&str>) -> String {
        let owner = Owner::new();
        owner.with(|| {
            untrack(|| {
                let busy = RwSignal::new(busy);
                let session_title = Signal::stored("The proposing session".to_owned());
                let work_title = Signal::stored(work.map(str::to_owned));
                task_actions(busy, session_title, work_title, Callback::new(|_| ()), RwSignal::new(false), NodeRef::new()).to_html()
            })
        })
    }

    #[test]
    fn task_actions_have_one_primary_and_no_general_decision_controls() {
        let html = actions_html(false, Some("Improve C6"));
        for label in ["Start session", "Do it here", "Save for later", "Not now", "Save elsewhere…"] {
            assert!(html.contains(label), "missing {label}: {html}");
        }
        for removed in [
            "Start it",
            "Add to this work",
            "Add as new work",
            "Work on it",
            "Tomorrow",
            "Dismiss",
            "<input",
            "<textarea",
            "▾",
            "<svg",
            "absolute",
            "role=\"menu\"",
        ] {
            assert!(!html.contains(removed), "unexpected {removed}: {html}");
        }
        assert_eq!(html.matches(BUTTON).count(), 1);
        assert_eq!(html.matches(TASK_SECONDARY).count(), 2);
        assert_eq!(html.matches("<button").count(), 5);
        assert_eq!(html.matches("pointer-coarse:min-h-11").count(), 5);
        assert_eq!(html.matches("flex flex-wrap items-center gap-2").count(), 1);
        assert!(html.contains("The session that proposed it, The proposing session, does it after"));
        assert!(html.contains("A to-do in “Improve C6”, to start later"));
        assert!(html.contains("aria-haspopup=\"dialog\""));
        assert!(html.contains("aria-expanded=\"false\""));
        assert!(!html.contains("role=\"menu\""));
    }

    #[test]
    fn task_actions_are_disabled_together_while_answering_and_explain_no_work_fallback() {
        let html = actions_html(true, None);
        assert_eq!(html.matches("<button").count(), 5);
        for button in html.split("<button").skip(1) {
            let opening = button.split('>').next().unwrap();
            assert!(opening.split_whitespace().any(|a| matches!(a, "disabled" | "disabled=\"\"" | "disabled=\"true\"")), "{opening}");
        }
        assert!(html.contains("new work if it belongs to none"));
    }

    #[test]
    fn task_work_chooser_offers_new_work_and_searchable_open_work_in_its_realm() {
        let work = |id: &str, realm: &str, title: &str, status: &str| c6_api::work::WorkItem {
            id: id.into(),
            realm: realm.into(),
            fields: c6_api::work::WorkFields { title: title.into(), status: status.into(), ..Default::default() },
            ..Default::default()
        };
        let works = vec![
            work("one", "home", "Improve C6", "open"),
            work("two", "home", "Fix the pager", "open"),
            work("closed", "home", "Old C6", "done"),
            work("private", "other", "Other C6", "open"),
        ];
        let all = task_work_options(&works, "home", "");
        assert_eq!(all, [("Add to work:one".into(), "Improve C6".into()), ("Add to work:two".into(), "Fix the pager".into())]);
        assert_eq!(task_work_options(&works, "home", " C6 "), all[..1]);
        assert!(task_work_options(&works, "home", "missing").is_empty());
        let owner = Owner::new();
        let html = owner.with(|| untrack(|| task_work_choices(RwSignal::new(false), Signal::stored(all), Callback::new(|_| ())).to_html()));
        assert_eq!(html.matches("New work").count(), 1);
        assert!(html.contains("Improve C6") && html.contains("Fix the pager"));
        assert_eq!(html.matches("<button").count(), 3);
        assert!(!html.contains("Save for later"));
    }
}
