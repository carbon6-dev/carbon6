//! Session labels: chips, the editor in a session's header, the label
//! filter of the session list, and what a session's labels make apply.

use c6_api::{Applied, EffectiveConfig};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{ErrorBox, INPUT};
use crate::workspace::{How, View, use_workspace};

/// The session list's label filter (sessions must have all of them).
#[derive(Clone, Copy)]
pub struct LabelFilter(pub RwSignal<Vec<String>>);

pub fn use_filter() -> LabelFilter {
    use_context::<LabelFilter>().expect("LabelFilter is provided at the root")
}

/// A label; clicking it lists the sessions that have it.
#[component]
pub fn LabelChip(label: String, #[prop(optional)] auto: bool, #[prop(optional)] on_remove: Option<Callback<()>>) -> impl IntoView {
    let filter = use_filter();
    let ws = use_workspace();
    let l = label.clone();
    let class = if auto {
        "inline-flex items-center gap-1 rounded bg-zinc-100 px-1.5 py-0.5 font-mono text-[11px] text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400"
    } else {
        "inline-flex items-center gap-1 rounded bg-sky-100 px-1.5 py-0.5 font-mono text-[11px] text-sky-800 dark:bg-sky-950 dark:text-sky-300"
    };
    view! {
        <span class=class>
            <button class="hover:underline" title="Sessions with this label" on:click=move |ev| {
                ev.stop_propagation();
                ev.prevent_default();
                filter.0.set(vec![l.clone()]);
                ws.open(None, View::Sessions, How::Beside);
            }>{label}</button>
            {on_remove.map(|cb| view! {
                <button class="opacity-60 hover:opacity-100" title="Remove this label" on:click=move |ev| { ev.stop_propagation(); cb.run(()); }>"×"</button>
            })}
        </span>
    }
}

/// The automatic labels worth showing next to yours: repo (the worker,
/// folder and model are shown anyway).
pub fn shown_auto(auto_labels: &[String]) -> Vec<String> {
    auto_labels.iter().filter(|l| l.starts_with("repo:")).cloned().collect()
}

/// A session's labels, editable: yours can be added and removed.
#[component]
pub fn SessionLabels(session: String, labels: Vec<String>, auto_labels: Vec<String>) -> impl IntoView {
    let session = StoredValue::new(session);
    let mine = RwSignal::new(labels);
    let auto = RwSignal::new(auto_labels);
    let draft = RwSignal::new(String::new());
    let adding = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let field = NodeRef::<leptos::html::Input>::new();
    // Focus the field when it appears (autofocus only works on page load).
    Effect::new(move || {
        if adding.get() {
            request_animation_frame(move || {
                if let Some(el) = field.get_untracked() {
                    let _ = el.focus();
                }
            });
        }
    });
    let save = move |next: Vec<String>| {
        spawn_local(async move {
            match api::set_labels(session.get_value(), next).await {
                Ok(s) => {
                    mine.set(s.labels);
                    auto.set(s.auto_labels);
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let add = move || {
        let new = c6_api::labels::parse_labels(&draft.get_untracked());
        if let Some(e) = new.iter().find_map(|l| c6_api::labels::check_user_label(l).err()) {
            error.set(Some(e));
            return;
        }
        let mut next = mine.get_untracked();
        for l in new {
            if !next.contains(&l) {
                next.push(l);
            }
        }
        draft.set(String::new());
        adding.set(false);
        save(next);
    };
    view! {
        <div class="mt-1 flex flex-wrap items-center gap-1">
            {move || shown_auto(&auto.get()).into_iter().map(|l| view! { <LabelChip label=l auto=true /> }).collect_view()}
            {move || mine.get().into_iter().map(|l| {
                let gone = l.clone();
                view! { <LabelChip label=l on_remove=Callback::new(move |_| save(mine.get_untracked().into_iter().filter(|x| *x != gone).collect())) /> }
            }).collect_view()}
            {move || if adding.get() {
                view! {
                    <input node_ref=field class=format!("{INPUT} w-48! py-0.5 font-mono text-xs") placeholder="label, e.g. client:acme"
                        prop:value=move || draft.get() on:input=move |ev| draft.set(event_target_value(&ev))
                        on:keydown=move |ev| match ev.key().as_str() {
                            "Enter" => { ev.prevent_default(); add(); }
                            "Escape" => { draft.set(String::new()); adding.set(false); }
                            _ => {}
                        }
                        on:blur=move |_| if draft.get_untracked().trim().is_empty() { adding.set(false) } else { add() } />
                }.into_any()
            } else {
                view! { <button class="rounded px-1 text-[11px] text-zinc-500 hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100" on:click=move |_| adding.set(true)>"+ label"</button> }.into_any()
            }}
            {move || error.get().map(|e| view! { <span class="text-xs text-red-600">{e}</span> })}
        </div>
    }
}

/// What applies to a session: commands, agents, skills and hooks from the
/// head's settings, matched against its labels.
#[component]
pub fn SessionConfig(session: String) -> impl IntoView {
    let session = StoredValue::new(session);
    let config = LocalResource::new(move || api::session_config(session.get_value()));
    view! {
        <div class="space-y-4 py-2">
            <p class="text-sm text-zinc-500">
                "What the head's settings give this session, by its labels. Commands follow label changes at once; agents, skills and hooks are fixed when a session starts, so a change applies to new sessions."
            </p>
            {move || config.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(c) => view! { <ConfigLists c /> }.into_any(),
            })}
        </div>
    }
}

#[component]
fn ConfigLists(c: EffectiveConfig) -> impl IntoView {
    let group = |title: &'static str, items: Vec<Applied>| {
        view! {
            <section class="space-y-1">
                <h3 class="text-sm font-medium">{title}</h3>
                {if items.is_empty() {
                    view! { <p class="text-xs text-zinc-500">"None set up."</p> }.into_any()
                } else {
                    items.into_iter().map(|a| {
                        let on = a.skipped.is_none();
                        view! {
                            <div class=format!("flex flex-wrap items-center gap-2 text-sm {}", if on { "" } else { "opacity-50" })>
                                <span>{if on { "✓" } else { "–" }}</span>
                                <span class="font-mono text-xs">{a.name}</span>
                                {a.applies_to.into_iter().map(|l| view! { <LabelChip label=l auto=true /> }).collect_view()}
                                {a.skipped.map(|s| view! { <span class="text-xs text-zinc-500">{s}</span> })}
                            </div>
                        }
                    }).collect_view().into_any()
                }}
            </section>
        }
    };
    view! {
        <div class="space-y-1">
            <h3 class="text-sm font-medium">"Labels"</h3>
            <div class="flex flex-wrap gap-1">{c.labels.into_iter().map(|l| view! { <LabelChip label=l auto=true /> }).collect_view()}</div>
        </div>
        {group("Slash commands", c.commands)}
        {group("Agents", c.agents)}
        {group("Skills", c.skills)}
        {group("Hooks", c.hooks)}
    }
}
