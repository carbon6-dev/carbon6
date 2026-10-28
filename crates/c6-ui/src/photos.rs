//! Photos and videos in the memex: a grid by month, search by description,
//! person, place or event, and a photo with its faces, place and caption.

use leptos::prelude::*;
use serde_json::{Value, json};

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};
use crate::when::full as day;
use crate::workspace::{Go, View};

fn month(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &"month".into(), &"long".into());
    let _ = js_sys::Reflect::set(&opts, &"year".into(), &"numeric".into());
    String::from(d.to_locale_date_string("default", &opts))
}

#[component]
pub fn PhotosPage() -> impl IntoView {
    let query = RwSignal::new(String::new());
    let asked = RwSignal::new(String::new());
    // A person, place or event (thing id), and its name.
    let about = RwSignal::new(None::<(String, String)>);
    let items = RwSignal::new(Vec::<Value>::new());
    let more = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let open = RwSignal::new(None::<String>);
    let facets = LocalResource::new(|| api::rpc("memex.media.facets", json!({})));
    let status = LocalResource::new(api::memex_status);
    let load = move |before: Option<i64>| {
        let (q, a) = (asked.get_untracked(), about.get_untracked().map(|a| a.0));
        leptos::task::spawn_local(async move {
            let mut p = json!({"q": q, "about": a, "limit": 120});
            if let Some(b) = before {
                p["before"] = b.into();
            }
            match api::rpc("memex.media.list", p).await {
                Ok(v) => {
                    let page = v.as_array().cloned().unwrap_or_default();
                    more.set(page.len() == 120 && q.trim().is_empty());
                    if before.is_some() {
                        items.update(|i| i.extend(page));
                    } else {
                        items.set(page);
                    }
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    Effect::new(move || {
        asked.track();
        about.track();
        load(None);
    });
    let search = move || asked.set(query.get_untracked());
    view! {
        <main class="mx-auto max-w-6xl space-y-4 px-4 py-4">
            <div class="flex flex-wrap items-center gap-2">
                <h1 class="text-lg font-semibold">"Photos"</h1>
                {move || status.get().and_then(Result::ok).map(|s| view! {
                    <span class="text-xs text-zinc-500">{format!("{} photos and videos · {} described", s.media, s.media_seen)}</span>
                })}
                <span class="flex-1"></span>
                <input class=format!("{INPUT} w-full sm:w-80") placeholder="Describe it: “whiteboard at the offsite”, “beach”, a place…"
                    prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { search() } />
                <button class=BUTTON on:click=move |_| search()>"Search"</button>
            </div>
            {move || facets.get().and_then(Result::ok).map(|f| view! { <Facets f about /> })}
            {move || about.get().map(|(_, name)| view! {
                <div class="flex items-center gap-2 text-sm">
                    <span>"Showing: " <b>{name}</b></span>
                    <button class=BUTTON_QUIET on:click=move |_| about.set(None)>"Show all"</button>
                </div>
            })}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || open.get().map(|id| view! { <PhotoView id open about /> })}
            {move || {
                let list = items.get();
                if list.is_empty() {
                    return view! {
                        <p class="text-sm text-zinc-500">
                            "No photos or videos yet. Add a folder of them under "
                            <Go to=View::Settings { note: None, section: "memex".into() } class="underline">"Settings → Memex folders"</Go>
                            "; they are read in place (nothing is copied but thumbnails)."
                        </p>
                    }.into_any();
                }
                // By month (search results keep their order, in one group).
                let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
                let searching = !asked.get_untracked().trim().is_empty();
                for it in list {
                    let key = if searching { "Best matches".to_owned() } else { it["taken_at"].as_i64().map(month).unwrap_or_default() };
                    match groups.last_mut() {
                        Some((k, g)) if *k == key => g.push(it),
                        _ => groups.push((key, vec![it])),
                    }
                }
                groups.into_iter().map(|(title, g)| view! {
                    <section class="space-y-1">
                        <h2 class="text-sm font-medium text-zinc-500">{title}</h2>
                        <div class="grid grid-cols-3 gap-1 sm:grid-cols-5 lg:grid-cols-8">
                            {g.into_iter().map(|it| tile(it, open)).collect_view()}
                        </div>
                    </section>
                }).collect_view().into_any()
            }}
            {move || more.get().then(|| view! {
                <div class="flex justify-center">
                    <button class=BUTTON_QUIET on:click=move |_| {
                        let last = items.with_untracked(|i| i.last().and_then(|x| x["taken_at"].as_i64()));
                        load(last);
                    }>"More"</button>
                </div>
            })}
        </main>
    }
}

fn tile(it: Value, open: RwSignal<Option<String>>) -> impl IntoView {
    let id = it["id"].as_str().unwrap_or_default().to_owned();
    let src = crate::api::url(&format!("/api/v1/memex/media/{id}/thumb"));
    let title = it["title"].as_str().unwrap_or_default().to_owned();
    let video = it["kind"] == "video";
    let duration = it["duration"].as_f64().map(|d| format!("{}:{:02}", d as u64 / 60, d as u64 % 60));
    let has_thumb = it["thumb"].as_bool().unwrap_or(false);
    let tip = title.clone();
    view! {
        <button class="group relative aspect-square overflow-hidden rounded bg-zinc-100 dark:bg-zinc-900" title=tip
            on:click=move |_| open.set(Some(id.clone()))>
            {if has_thumb {
                view! { <img src=src loading="lazy" alt=title class="h-full w-full object-cover transition group-hover:opacity-90" /> }.into_any()
            } else {
                view! { <span class="flex h-full w-full items-center justify-center p-1 text-[10px] text-zinc-500">{title}</span> }.into_any()
            }}
            {video.then(|| view! {
                <span class="absolute bottom-1 right-1 rounded bg-black/60 px-1 text-[10px] text-white">{format!("▶ {}", duration.clone().unwrap_or_default())}</span>
            })}
        </button>
    }
}

#[component]
fn Facets(f: Value, about: RwSignal<Option<(String, String)>>) -> impl IntoView {
    let row = |title: &'static str, list: Vec<Value>, faces: bool| {
        (!list.is_empty()).then(|| view! {
            <div class="flex flex-wrap items-center gap-1 text-xs">
                <span class="w-14 text-zinc-500">{title}</span>
                {list.into_iter().take(16).map(|t| {
                    let id = t["id"].as_str().unwrap_or_default().to_owned();
                    let name = t["name"].as_str().unwrap_or_default().to_owned();
                    let pick = (id.clone(), name.clone());
                    view! {
                        <button class="flex items-center gap-1 rounded-full border border-zinc-300 py-0.5 pl-0.5 pr-2 hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-900"
                            on:click=move |_| about.set(Some(pick.clone()))>
                            {faces.then(|| view! { <img src=crate::api::url(&format!("/api/v1/memex/face/{id}")) class="h-5 w-5 rounded-full object-cover" alt="" /> })}
                            <span class:pl-1=!faces>{name}</span>
                            <span class="text-zinc-400">{t["n"].as_i64().unwrap_or(0)}</span>
                        </button>
                    }
                }).collect_view()}
            </div>
        })
    };
    let get = |k: &str| f[k].as_array().cloned().unwrap_or_default();
    view! {
        <div class="space-y-1">
            {row("People", get("people"), true)}
            {row("Places", get("places"), false)}
            {row("Events", get("events"), false)}
        </div>
    }
}

#[component]
fn PhotoView(id: String, open: RwSignal<Option<String>>, about: RwSignal<Option<(String, String)>>) -> impl IntoView {
    let id2 = id.clone();
    let data = LocalResource::new(move || api::rpc("memex.media.get", json!({"id": id2.clone()})));
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <div class="flex justify-end gap-2">
                <a href=crate::api::url(&format!("/api/v1/memex/items/{id}/content")) target="_blank" rel="noopener" class="text-xs text-sky-700 hover:underline dark:text-sky-400">"Open original ↗"</a>
                <button class=BUTTON_QUIET on:click=move |_| open.set(None)>"Close"</button>
            </div>
            {move || data.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) => detail(v, about).into_any(),
            })}
        </section>
    }
}

fn detail(v: Value, about: RwSignal<Option<(String, String)>>) -> impl IntoView {
    let item = v["item"].clone();
    let id = item["id"].as_str().unwrap_or_default().to_owned();
    let faces = v["faces"].as_array().cloned().unwrap_or_default();
    let frames = v["frames"].as_u64().unwrap_or(0);
    let things = v["things"].as_array().cloned().unwrap_or_default();
    let when = v["taken_at"].as_i64().map(day).unwrap_or_default();
    let map = match (v["lat"].as_f64(), v["lon"].as_f64()) {
        (Some(la), Some(lo)) => Some(format!("https://www.openstreetmap.org/?mlat={la}&mlon={lo}#map=14/{la}/{lo}")),
        _ => None,
    };
    let src = crate::api::url(&format!("/api/v1/memex/media/{id}/thumb"));
    view! {
        <div class="grid gap-3 md:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
            <div class="space-y-2">
                <div class="relative">
                    <img src=src class="w-full rounded" alt=item["title"].as_str().unwrap_or_default().to_owned() />
                    {faces.iter().map(|f| {
                        let style = format!("left:{}%;top:{}%;width:{}%;height:{}%",
                            f["x"].as_f64().unwrap_or(0.0) * 100.0, f["y"].as_f64().unwrap_or(0.0) * 100.0,
                            f["w"].as_f64().unwrap_or(0.0) * 100.0, f["h"].as_f64().unwrap_or(0.0) * 100.0);
                        let name = f["person"]["name"].as_str().unwrap_or("?").to_owned();
                        view! {
                            <div class="absolute rounded border-2 border-white/80 shadow" style=style>
                                <span class="absolute -bottom-5 left-0 whitespace-nowrap rounded bg-black/60 px-1 text-[10px] text-white">{name}</span>
                            </div>
                        }
                    }).collect_view()}
                </div>
                {(frames > 1).then(|| view! {
                    <div class="flex gap-1 overflow-x-auto">
                        {(0..frames).map(|n| view! { <img src=crate::api::url(&format!("/api/v1/memex/media/{id}/thumb?frame={n}")) class="h-16 rounded" alt="a frame" /> }).collect_view()}
                    </div>
                })}
            </div>
            <div class="space-y-2 text-sm">
                <h2 class="font-medium">{item["title"].as_str().unwrap_or_default().to_owned()}</h2>
                <p class="text-xs text-zinc-500">
                    {when}
                    {(!v["camera"].as_str().unwrap_or_default().is_empty()).then(|| format!(" · {}", v["camera"].as_str().unwrap_or_default()))}
                    {map.map(|u| view! { " · " <a href=u target="_blank" rel="noopener noreferrer" class="text-sky-700 hover:underline dark:text-sky-400">"map ↗"</a> })}
                </p>
                <div class="flex flex-wrap gap-1">
                    {things.into_iter().map(|t| {
                        let pick = (t["id"].as_str().unwrap_or_default().to_owned(), t["name"].as_str().unwrap_or_default().to_owned());
                        view! {
                            <button class="rounded bg-zinc-100 px-1.5 text-[11px] hover:underline dark:bg-zinc-800" title=t["kind"].as_str().unwrap_or_default().to_owned()
                                on:click=move |_| about.set(Some(pick.clone()))>{t["name"].as_str().unwrap_or_default().to_owned()}</button>
                        }
                    }).collect_view()}
                </div>
                <pre class="max-h-80 overflow-auto whitespace-pre-wrap rounded bg-zinc-50 p-2 text-xs dark:bg-zinc-900">{item["text"].as_str().unwrap_or_default().to_owned()}</pre>
                {v["error"].as_str().map(|e| view! { <p class="text-xs text-amber-700">{e.to_owned()}</p> })}
                <p class="text-xs text-zinc-500">"Unnamed faces are named under Memex → Review."</p>
            </div>
        </div>
    }
}
