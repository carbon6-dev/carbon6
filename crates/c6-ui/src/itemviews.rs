//! Views of items of a shared kind (docs/knowledge.md §6.2, #7): messages
//! (email, by thread), a calendar (events, by week) and a gallery (photos
//! or videos). They draw items from whichever app owns them; their buttons
//! are the view's actions, which call the owning app's operations.

use c6_api::{View as ViewDef, ViewAct, ViewItem};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::calendar::{DAY, date_of, day_label, hm, midnight};
use crate::util::{BUTTON_QUIET, ErrorBox};
use crate::when;

/// The view's action buttons for one item.
#[component]
fn ItemActions(view: StoredValue<ViewDef>, item: String, tick: RwSignal<u32>, error: RwSignal<Option<String>>) -> impl IntoView {
    view.get_value()
        .actions
        .iter()
        .map(|a| {
            let (label, item) = (a.label.clone(), item.clone());
            let shown = a.label.clone();
            view! {
                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |ev| {
                    ev.stop_propagation();
                    let act = ViewAct { action: label.clone(), item: item.clone(), ..Default::default() };
                    let name = view.get_value().name;
                    spawn_local(async move {
                        match api::view_act(name, act).await {
                            Ok(_) => { error.set(None); tick.update(|t| *t += 1); }
                            Err(e) => error.set(Some(e)),
                        }
                    });
                }>{shown}</button>
            }
        })
        .collect_view()
}

/// Items of the view, newest first, with "older" paging.
fn newest(
    view: &ViewDef,
    kind: Signal<String>,
    tick: RwSignal<u32>,
    pages: RwSignal<Vec<i64>>,
) -> LocalResource<Result<Vec<ViewItem>, String>> {
    let name = view.name.clone();
    LocalResource::new(move || {
        tick.track();
        api::realm().track();
        let (name, kind, pages) = (name.clone(), kind.get(), pages.get());
        async move {
            let mut out: Vec<ViewItem> = Vec::new();
            for before in std::iter::once(None).chain(pages.into_iter().map(Some)) {
                let mut q = format!("limit=100&kind={kind}");
                if let Some(b) = before {
                    q.push_str(&format!("&before={b}"));
                }
                out.extend(api::view_items(name.clone(), q).await?);
            }
            Ok(out)
        }
    })
}

/// Email, as threads: the newest first on the left, the chosen thread's
/// messages (oldest first) on the right.
#[component]
pub fn MessagesView(view: ViewDef) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let error = RwSignal::new(None::<String>);
    let pages = RwSignal::new(Vec::<i64>::new());
    let chosen = RwSignal::new(None::<String>);
    let items = newest(&view, Signal::derive(String::new), tick, pages);
    let v = StoredValue::new(view);
    let thread_of =
        |i: &ViewItem| i.meta["thread_id"].as_str().filter(|t| !t.is_empty()).map(str::to_owned).unwrap_or_else(|| i.id.clone());
    view! {
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || items.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok(all) => {
                // Threads in the order of their newest message.
                let mut threads: Vec<(String, Vec<ViewItem>)> = Vec::new();
                for i in &all {
                    let t = thread_of(i);
                    match threads.iter_mut().find(|(k, _)| *k == t) {
                        Some((_, list)) => list.push(i.clone()),
                        None => threads.push((t, vec![i.clone()])),
                    }
                }
                let oldest = all.last().map(|i| i.created_at);
                let open = chosen.get().and_then(|c| threads.iter().find(|(t, _)| *t == c).cloned());
                view! {
                    <div class="grid gap-3 md:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
                        <div class="divide-y divide-zinc-100 rounded-md border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                            {threads.is_empty().then(|| view! { <p class="p-3 text-sm text-zinc-500">"No messages."</p> })}
                            {threads.iter().map(|(t, list)| {
                                let newest = &list[0];
                                let (t2, n) = (t.clone(), list.len());
                                let active = chosen.get().as_deref() == Some(t.as_str());
                                view! {
                                    <button class=format!("block w-full px-3 py-2 text-left text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900 {}", if active { "bg-zinc-100 dark:bg-zinc-800" } else { "" })
                                        on:click=move |_| chosen.set(Some(t2.clone()))>
                                        <div class="flex justify-between gap-2">
                                            <span class="truncate font-medium">{newest.meta["from"].as_str().unwrap_or("").to_owned()}</span>
                                            <span class="shrink-0 text-xs text-zinc-500">{when::compact(newest.created_at)}</span>
                                        </div>
                                        <div class="truncate">{newest.title.clone()}{(n > 1).then(|| format!(" ({n})"))}</div>
                                    </button>
                                }
                            }).collect_view()}
                            {oldest.filter(|_| all.len() >= 100 * (pages.get().len() + 1)).map(|o| view! {
                                <button class=format!("{BUTTON_QUIET} m-2 text-xs") on:click=move |_| pages.update(|p| p.push(o))>"Older"</button>
                            })}
                        </div>
                        <div class="space-y-3">
                            {match open {
                                None => view! { <p class="text-sm text-zinc-500">"Choose a thread."</p> }.into_any(),
                                Some((_, mut list)) => {
                                    list.reverse();
                                    let last = list.last().map(|i| i.id.clone()).unwrap_or_default();
                                    view! {
                                        <div class="flex flex-wrap gap-1"><ItemActions view=v item=last tick error /></div>
                                        {list.into_iter().map(|m| view! {
                                            <article class="rounded-md border border-zinc-200 p-3 text-sm dark:border-zinc-800">
                                                <div class="flex justify-between gap-2 text-xs text-zinc-500">
                                                    <span>{m.meta["from"].as_str().unwrap_or("").to_owned()}</span>
                                                    <span>{when::full(m.created_at)}</span>
                                                </div>
                                                <h3 class="font-medium">{m.title.clone()}</h3>
                                                <p class="mt-1 whitespace-pre-wrap">{m.text.clone()}</p>
                                            </article>
                                        }).collect_view()}
                                    }.into_any()
                                }
                            }}
                        </div>
                    </div>
                }.into_any()
            }
        })}
    }
}

/// Events, a week at a time, by day.
#[component]
pub fn CalendarView(view: ViewDef) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let error = RwSignal::new(None::<String>);
    // The week's first day (Monday).
    let monday = |ms: f64| {
        let d = midnight(ms);
        let wd = (date_of(d).get_day() + 6) % 7;
        d - wd as f64 * DAY
    };
    let start = RwSignal::new(monday(js_sys::Date::now()));
    let name = view.name.clone();
    let items = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        let (name, from) = (name.clone(), start.get());
        async move { api::view_items(name, format!("from={}&to={}&limit=500", from as i64, (from + 7.0 * DAY) as i64)).await }
    });
    let v = StoredValue::new(view);
    view! {
        <div class="flex items-center gap-2">
            <button class=BUTTON_QUIET on:click=move |_| start.update(|s| *s -= 7.0 * DAY)>"←"</button>
            <button class=BUTTON_QUIET on:click=move |_| start.set(monday(js_sys::Date::now()))>"This week"</button>
            <button class=BUTTON_QUIET on:click=move |_| start.update(|s| *s += 7.0 * DAY)>"→"</button>
            <span class="text-sm text-zinc-500">{move || format!("{} – {}", day_label(start.get()), day_label(start.get() + 6.0 * DAY))}</span>
        </div>
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || items.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok(all) => {
                let s = start.get();
                view! {
                    <div class="divide-y divide-zinc-100 rounded-md border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                        {(0..7).map(|d| {
                            let (from, to) = (s + d as f64 * DAY, s + (d + 1) as f64 * DAY);
                            let day: Vec<ViewItem> = all.iter().filter(|e| (e.created_at as f64) >= from && (e.created_at as f64) < to).cloned().collect();
                            view! {
                                <div class="flex gap-3 px-3 py-2 text-sm">
                                    <div class="w-28 shrink-0 text-zinc-500">{day_label(from)}</div>
                                    <div class="min-w-0 flex-1 space-y-1">
                                        {day.into_iter().map(|e| {
                                            let time = if e.meta["all_day"].as_bool() == Some(true) {
                                                "all day".to_owned()
                                            } else {
                                                match e.meta["end"].as_i64() {
                                                    Some(end) => format!("{}–{}", hm(e.created_at), hm(end)),
                                                    None => hm(e.created_at),
                                                }
                                            };
                                            view! {
                                                <div class="flex flex-wrap items-center gap-2">
                                                    <span class="w-24 shrink-0 text-xs text-zinc-500">{time}</span>
                                                    <span class="font-medium">{e.title.clone()}</span>
                                                    {e.meta["location"].as_str().map(|l| view! { <span class="text-xs text-zinc-500">{l.to_owned()}</span> })}
                                                    <ItemActions view=v item=e.id.clone() tick error />
                                                </div>
                                            }
                                        }).collect_view()}
                                    </div>
                                </div>
                            }
                        }).collect_view()}
                    </div>
                }.into_any()
            }
        })}
    }
}

/// Photos (or videos), newest first, as a grid; one chosen, larger.
#[component]
pub fn GalleryView(view: ViewDef) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let error = RwSignal::new(None::<String>);
    let pages = RwSignal::new(Vec::<i64>::new());
    let own = view.source["items"].as_str().unwrap_or("photo").to_owned();
    let kind = RwSignal::new(own.clone());
    let chosen = RwSignal::new(None::<ViewItem>);
    let items = newest(&view, kind.into(), tick, pages);
    let v = StoredValue::new(view);
    let name = v.get_value().name;
    let name2 = name.clone();
    view! {
        <div class="flex gap-1">
            {["photo", "video"].into_iter().map(|k| view! {
                <button class=move || format!("{BUTTON_QUIET} text-xs {}", if kind.get() == k { "bg-zinc-100 dark:bg-zinc-800" } else { "" })
                    on:click=move |_| { kind.set(k.to_owned()); pages.set(Vec::new()); chosen.set(None); }>
                    {if k == "photo" { "Photos" } else { "Videos" }}
                </button>
            }).collect_view()}
        </div>
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || chosen.get().map(|i| {
            let src = api::view_thumb(&name, &i.id);
            view! {
                <figure class="space-y-2 rounded-md border border-zinc-200 p-3 dark:border-zinc-800">
                    <img src=src class="max-h-[60vh] rounded" alt=i.title.clone() />
                    <figcaption class="text-sm">
                        <span class="font-medium">{i.title.clone()}</span>
                        <span class="ml-2 text-xs text-zinc-500">{when::full(i.created_at)}</span>
                        <p class="whitespace-pre-wrap text-zinc-600 dark:text-zinc-400">{i.text.clone()}</p>
                    </figcaption>
                    <div class="flex flex-wrap gap-1">
                        <ItemActions view=v item=i.id.clone() tick error />
                        <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| chosen.set(None)>"Close"</button>
                    </div>
                </figure>
            }
        })}
        {move || items.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok(all) => {
                let oldest = all.last().map(|i| i.created_at);
                let more = all.len() >= 100 * (pages.get().len() + 1);
                view! {
                    {all.is_empty().then(|| view! { <p class="text-sm text-zinc-500">"Nothing here yet."</p> })}
                    <div class="grid grid-cols-3 gap-2 sm:grid-cols-4 lg:grid-cols-6">
                        {all.into_iter().map(|i| {
                            let src = api::view_thumb(&name2, &i.id);
                            let alt = i.title.clone();
                            view! {
                                <button class="aspect-square overflow-hidden rounded bg-zinc-100 dark:bg-zinc-800" title=i.title.clone() on:click=move |_| chosen.set(Some(i.clone()))>
                                    <img src=src class="h-full w-full object-cover" loading="lazy" alt=alt />
                                </button>
                            }
                        }).collect_view()}
                    </div>
                    {oldest.filter(|_| more).map(|o| view! {
                        <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| pages.update(|p| p.push(o))>"Older"</button>
                    })}
                }.into_any()
            }
        })}
    }
}
