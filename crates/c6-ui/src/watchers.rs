//! Watchers (docs/watchers.md): Settings → Watchers, where each watcher is
//! turned on or off and given its budget, with what they found lately; and
//! the session's Glance line saying who watches it.

use c6_api::watchers::{Finding, Watcher, WatcherDef};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, View};

#[component]
pub fn WatchersSettings() -> impl IntoView {
    let list = LocalResource::new(api::watchers);
    let found = LocalResource::new(api::watcher_findings);
    let error = RwSignal::new(None::<String>);
    let save = Callback::new(move |d: WatcherDef| {
        spawn_local(async move {
            match api::put_watcher(d).await {
                Ok(_) => error.set(None),
                Err(e) => error.set(Some(e)),
            }
            list.refetch();
        })
    });
    let reset = Callback::new(move |name: String| {
        spawn_local(async move {
            if let Err(e) = api::reset_watcher(name).await {
                error.set(Some(e));
            }
            list.refetch();
        })
    });
    let run = Callback::new(move |name: String| {
        spawn_local(async move {
            if let Err(e) = api::run_watcher(name).await {
                error.set(Some(e));
            }
            list.refetch();
            found.refetch();
        })
    });
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Watchers"</h2>
                <p class="text-sm text-muted">
                    "Standing agents with their own agenda that follow sessions and act as things happen. Each sleeps until something it wakes on happens, reads only what changed, and calls its model only then. Its conversation is a session you can open and write to. Agendas are prompts (Settings → Prompts, watch.*)."
                </p>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                    Ok(v) => v.into_iter().map(|w| view! { <Row w save reset run /> }).collect_view().into_any(),
                })}
            </div>
            <h3 class="pt-2 text-sm font-medium">"What they found lately"</h3>
            {move || found.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.is_empty() => view! { <p class="text-sm text-muted">"Nothing yet."</p> }.into_any(),
                Ok(v) => view! { <ul class="space-y-1 text-sm">{v.into_iter().map(|f| view! { <FindingLine f /> }).collect_view()}</ul> }.into_any(),
            })}
        </section>
    }
}

#[component]
fn Row(w: Watcher, save: Callback<WatcherDef>, reset: Callback<String>, run: Callback<String>) -> impl IntoView {
    let def = StoredValue::new(w.def.clone());
    let with = move |f: &dyn Fn(&mut WatcherDef)| {
        let mut d = def.get_value();
        f(&mut d);
        save.run(d);
    };
    let name = w.def.name.clone();
    let (reset_name, run_name) = (name.clone(), name.clone());
    let scope = if w.def.scope == "all" {
        "everything".to_owned()
    } else if w.def.applies_to.is_empty() {
        "each session".to_owned()
    } else {
        format!("sessions with {}", w.def.applies_to.join(" "))
    };
    let all = w.def.scope == "all";
    view! {
        <div class="space-y-1 px-4 py-3">
            <div class="flex flex-wrap items-center gap-2">
                <crate::components::Switch on=Signal::stored(w.def.enabled)
                    on_change=move |on| with(&|d| d.enabled = on)
                    label=if w.def.title.is_empty() { w.def.name.clone() } else { w.def.title.clone() } />
                <span class="text-xs text-muted">{format!("{} · follows {scope}", w.def.name)}</span>
                {w.edited.then(|| view! {
                    <button class=BUTTON_QUIET title="Back to carbon6's version" on:click=move |_| reset.run(reset_name.clone())>"Reset"</button>
                })}
                {all.then(|| view! {
                    <button class=BUTTON_QUIET title="Wake it now" on:click=move |_| run.run(run_name.clone())>"Run now"</button>
                })}
            </div>
            <p class="text-sm text-muted">{w.def.description.clone()}</p>
            <p class="text-xs text-muted">
                {format!(
                    "Wakes on {}; may {}; agenda: {} · today: {} wakes, {} skips, {} tokens · {} findings",
                    if w.def.wakes.is_empty() { "tool calls a policy matched".to_owned() } else { w.def.wakes.join(", ") },
                    if w.def.outputs.is_empty() { "hold a call for you".to_owned() } else { w.def.outputs.join(", ") },
                    w.def.agenda,
                    w.woke,
                    w.skipped,
                    w.tokens_today,
                    w.findings
                )}
            </p>
            <div class="flex flex-wrap items-center gap-3 text-xs">
                <label class="flex items-center gap-1">
                    "Tokens a day"
                    <input type="number" min="0" class=format!("{INPUT} w-28! py-0.5") prop:value=w.def.daily_tokens.to_string()
                        on:change=move |ev| { if let Ok(n) = event_target_value(&ev).trim().parse::<u64>() { with(&|d| d.daily_tokens = n) } } />
                </label>
                {(!all).then(|| view! {
                    <crate::components::Switch on=Signal::stored(w.def.to_you)
                        on_change=move |on| with(&|d| d.to_you = on)
                        label="Everything to me" title="Findings go to you (marks, cards), never to the agent" />
                    <label class="flex items-center gap-1">
                        "Notes to the agent from"
                        <select class=format!("{INPUT} w-auto! py-0.5") prop:value=w.def.note_severity.clone()
                            on:change=move |ev| { let v = event_target_value(&ev); with(&|d| d.note_severity = v.clone()) }>
                            <option value="info">"info"</option>
                            <option value="important">"important"</option>
                            <option value="urgent">"urgent"</option>
                        </select>
                    </label>
                })}
            </div>
        </div>
    }
}

#[component]
fn FindingLine(f: Finding) -> impl IntoView {
    let when = ago(now_ms() - f.at);
    view! {
        <li class="flex flex-wrap items-baseline gap-x-2">
            <span class="text-xs text-muted">{format!("{} · {} · {}", f.watcher, f.kind, f.severity)}</span>
            <span class="min-w-0 flex-1">{f.text.clone()}</span>
            <span class="text-xs text-muted">{f.done.clone()}</span>
            <Go to=View::Session { id: f.session_id.clone(), target: None } class="text-xs text-violet-700 hover:underline dark:text-violet-400" title="The watcher's session">
                {when}
            </Go>
        </li>
    }
}

/// The Glance's line: the watchers following this session (each opens its
/// session) and what they found last.
#[component]
pub fn SessionWatchers(session_id: String, moved: Signal<String>) -> impl IntoView {
    let id = StoredValue::new(session_id);
    let watch = LocalResource::new(move || {
        moved.track();
        api::session_watch(id.get_value())
    });
    move || {
        let w = watch.get().and_then(Result::ok)?;
        if w.watchers.is_empty() {
            return None;
        }
        let last = w.findings.first().cloned();
        Some(view! {
            <span class="inline-flex flex-wrap items-baseline gap-x-2">
                {w.watchers.into_iter().map(|(name, sid)| view! {
                    <Go to=View::Session { id: sid, target: None } class="text-violet-700 hover:underline dark:text-violet-400" title="Its session">
                        {name}
                    </Go>
                }).collect_view()}
                {last.map(|f| view! { <span class="text-muted" title=f.text.clone()>{format!("· {}: {}", f.kind, f.text.chars().take(80).collect::<String>())}</span> })}
            </span>
        })
    }
}
