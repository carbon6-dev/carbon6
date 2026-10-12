//! The Memex page: search your knowledge store, read items and things
//! (labels as nodes), write notes. And the memex's settings.

use c6_api::{MemexHit, MemexStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use wasm_bindgen::prelude::*;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, View};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = c6Files, js_name = read)]
    async fn js_read_files(ev: &web_sys::Event) -> Result<JsValue, JsValue>;
}

const KINDS: [(&str, &str); 10] = [
    ("session", "Sessions"),
    ("note", "Notes"),
    ("web", "Web pages"),
    ("output", "Outputs"),
    ("document", "Documents"),
    ("paper", "Papers"),
    ("email", "Email"),
    ("event", "Events"),
    ("photo", "Photos"),
    ("video", "Videos"),
];

fn kind_tone(kind: &str) -> &'static str {
    match kind {
        "session" => "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
        "note" => "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
        "web" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        "output" => "bg-violet-100 text-violet-800 dark:bg-violet-950 dark:text-violet-300",
        _ => "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
    }
}

fn when(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    String::from(d.to_locale_date_string("default", &wasm_bindgen::JsValue::UNDEFINED))
}

#[component]
pub fn MemexPage() -> impl IntoView {
    let status = LocalResource::new(api::memex_status);
    let query = RwSignal::new(String::new());
    let kinds = RwSignal::new(Vec::<String>::new());
    // What was searched (on Enter or a filter change).
    let asked = RwSignal::new(None::<(String, Vec<String>)>);
    let results = LocalResource::new(move || {
        let a = asked.get();
        async move {
            match a {
                Some((q, k)) if !q.trim().is_empty() => api::memex_search(q, k).await.map(Some),
                _ => Ok(None),
            }
        }
    });
    // An item or thing open below the results.
    let open = RwSignal::new(None::<String>);
    let search = move || {
        open.set(None);
        asked.set(Some((query.get_untracked(), kinds.get_untracked())));
    };
    let tab = RwSignal::new("search");
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-4">
            <div class="flex gap-1 text-sm">
                {[("search", "Search"), ("review", "Review"), ("commitments", "Commitments"), ("trails", "Trails"), ("add", "Add files")].into_iter().map(|(t, label)| view! {
                    <button class=move || format!("rounded-md px-3 py-1 {}", if tab.get() == t { "bg-zinc-200 dark:bg-zinc-800" } else { "text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-900" })
                        on:click=move |_| tab.set(t)>{label}</button>
                }).collect_view()}
            </div>
            {move || match tab.get() {
                "trails" => view! { <Trails open /> }.into_any(),
                "review" => view! { <crate::understanding::Review open /> }.into_any(),
                "commitments" => view! { <crate::understanding::Commitments open /> }.into_any(),
                "add" => view! { <AddFiles /> }.into_any(),
                _ => ().into_any(),
            }}
            {move || open.get().filter(|_| tab.get() != "search").map(|id| view! { <Detail id open /> })}
            <div class="space-y-4" class:hidden=move || tab.get() != "search">
            <Suspense fallback=|| ()>
                {move || status.get().map(|r| match r {
                    Ok(s) => view! { <StatusLine s /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>
            <div class="flex gap-2">
                <input class=format!("{INPUT} min-w-0 flex-1") placeholder="Search everything: past sessions, notes, pages read, outputs…"
                    prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { search() } />
                <button class=BUTTON on:click=move |_| search()>"Search"</button>
            </div>
            <div class="flex flex-wrap gap-1 text-xs">
                {KINDS.iter().map(|(k, label)| {
                    let k = k.to_string();
                    let k2 = k.clone();
                    let on = move || kinds.with(|v| v.contains(&k));
                    view! {
                        <button class=move || format!("rounded-full border px-2 py-0.5 {}", if on() { "border-zinc-900 bg-zinc-900 text-white dark:border-zinc-100 dark:bg-zinc-100 dark:text-zinc-900" } else { "border-zinc-300 text-zinc-600 dark:border-zinc-700 dark:text-zinc-400" })
                            on:click=move |_| {
                                kinds.update(|v| if let Some(i) = v.iter().position(|x| *x == k2) { v.remove(i); } else { v.push(k2.clone()); });
                                if asked.with_untracked(Option::is_some) { search(); }
                            }>{*label}</button>
                    }
                }).collect_view()}
            </div>
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Searching…"</p> }>
                {move || results.get().map(|r| match r {
                    Ok(None) => view! { <NoteBox /> }.into_any(),
                    Ok(Some(hits)) if hits.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing matches."</p> }.into_any(),
                    Ok(Some(hits)) => view! {
                        <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                            {hits.into_iter().map(|h| view! { <Hit h open /> }).collect_view()}
                        </div>
                    }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>
            {move || open.get().map(|id| view! { <Detail id open /> })}
            </div>
        </main>
    }
}

/// Upload documents and papers.
#[component]
fn AddFiles() -> impl IntoView {
    let labels = RwSignal::new(String::new());
    let log = RwSignal::new(Vec::<Result<String, String>>::new());
    let pick = move |ev: web_sys::Event| {
        let labels = c6_api::labels::parse_labels(&labels.get_untracked());
        spawn_local(async move {
            let files: Vec<c6_api::AttachmentUpload> = match js_read_files(&ev).await.ok().and_then(|v| v.as_string()) {
                Some(json) => serde_json::from_str(&json).unwrap_or_default(),
                None => return,
            };
            for f in files {
                let name = f.name.clone();
                log.update(|l| l.push(Ok(format!("Reading {name}…"))));
                let r = api::memex_upload(c6_api::MemexUpload { name: f.name, mime: f.mime, data: f.data, labels: labels.clone() }).await;
                log.update(|l| {
                    l.pop();
                    l.push(
                        r.map(|v| {
                            format!(
                                "{name}: added as {} \u{201c}{}\u{201d}",
                                v["kind"].as_str().unwrap_or(""),
                                v["title"].as_str().unwrap_or("")
                            )
                        })
                        .map_err(|e| format!("{name}: {e}")),
                    );
                });
            }
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <p class="text-sm text-zinc-500">"PDF, Word (.docx), Markdown, HTML or text, up to 50 MB each. Research papers are recognized: their title, authors, identifiers and the papers they cite join the graph. To keep a folder in sync instead, add it under Settings → Memex folders."</p>
            <input class=format!("{INPUT} font-mono text-xs") placeholder="labels for these files, e.g. research, client:acme" prop:value=move || labels.get() on:input=move |ev| labels.set(event_target_value(&ev)) />
            <input type="file" multiple accept=".pdf,.docx,.md,.markdown,.txt,.html,.htm,.rst" class="text-sm" on:change=pick />
            <ul class="space-y-1 text-sm">
                {move || log.get().into_iter().map(|r| match r {
                    Ok(m) => view! { <li class="text-zinc-600 dark:text-zinc-400">{m}</li> }.into_any(),
                    Err(e) => view! { <li class="text-red-600">{e}</li> }.into_any(),
                }).collect_view()}
            </ul>
        </section>
    }
}

/// Trails: named paths through items and things.
#[component]
fn Trails(open: RwSignal<Option<String>>) -> impl IntoView {
    let list = LocalResource::new(api::memex_trails);
    let shown = RwSignal::new(None::<String>);
    view! {
        <section class="space-y-3">
            <p class="text-sm text-zinc-500">"Trails are paths through what you know, step by step, each with a note on why it matters. Add a step from any item (\u{201c}Add to a trail\u{201d}); agents add steps with memex_trail."</p>
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"No trails yet."</p> }.into_any(),
                Ok(v) => view! {
                    <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                        {v.into_iter().map(|t| {
                            let id = t.id.clone();
                            view! {
                                <button class="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900" on:click=move |_| shown.set(Some(id.clone()))>
                                    <span class="font-medium">{t.name.clone()}</span>
                                    <span class="text-xs text-zinc-500">{format!("{} steps · {}", t.step_count, when(t.updated_at))}</span>
                                </button>
                            }
                        }).collect_view()}
                    </div>
                }.into_any(),
            })}
            {move || shown.get().map(|id| view! { <TrailView id open on_change=move || list.refetch() /> })}
        </section>
    }
}

#[component]
fn TrailView(id: String, open: RwSignal<Option<String>>, on_change: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let tid = StoredValue::new(id.clone());
    let trail = LocalResource::new(move || api::memex_trail(tid.get_value()));
    let error = RwSignal::new(None::<String>);
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || trail.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(t) => view! {
                    <div class="flex items-center gap-2">
                        <h2 class="font-medium">{t.name.clone()}</h2>
                        <span class="flex-1"></span>
                        <button class="text-xs text-red-600 hover:underline" on:click=move |_| {
                            if !window().confirm_with_message("Delete this trail? (Its items stay.)").unwrap_or(false) { return; }
                            spawn_local(async move {
                                match api::memex_trail_delete(tid.get_value()).await { Ok(()) => on_change(), Err(e) => error.set(Some(e)) }
                            });
                        }>"Delete trail"</button>
                    </div>
                    <ol class="space-y-2">
                        {t.steps.into_iter().enumerate().map(|(i, s)| {
                            let (target, step) = (s.target.clone(), s.id);
                            view! {
                                <li class="rounded-md bg-zinc-50 p-2 text-sm dark:bg-zinc-900">
                                    <div class="flex flex-wrap items-center gap-2">
                                        <span class="text-xs text-zinc-400">{i + 1}"."</span>
                                        <span class=format!("rounded px-1.5 text-[11px] {}", kind_tone(&s.kind))>{s.kind.clone()}</span>
                                        <button class="font-medium hover:underline" on:click=move |_| open.set(Some(target.clone()))>{s.title.clone()}</button>
                                        <span class="text-xs text-zinc-500">{format!("by {}", s.made_by)}</span>
                                        <span class="flex-1"></span>
                                        <button class="text-xs text-zinc-500 hover:text-red-600" title="Remove this step" on:click=move |_| {
                                            spawn_local(async move {
                                                match api::memex_trail_remove(tid.get_value(), step).await { Ok(()) => trail.refetch(), Err(e) => error.set(Some(e)) }
                                            });
                                        }>"✕"</button>
                                    </div>
                                    {(!s.comment.is_empty()).then(|| view! { <p class="mt-1 text-zinc-600 dark:text-zinc-400">{s.comment.clone()}</p> })}
                                </li>
                            }
                        }).collect_view()}
                    </ol>
                }.into_any(),
            })}
        </section>
    }
}

/// Add an item or thing to a trail.
#[component]
fn AddToTrail(target: String) -> impl IntoView {
    let target = StoredValue::new(target);
    let (trail, comment) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let open = RwSignal::new(false);
    let note = RwSignal::new(None::<Result<String, String>>);
    let trails = LocalResource::new(api::memex_trails);
    let add = move |_| {
        let t = trail.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        spawn_local(async move {
            note.set(Some(
                api::memex_trail_append(t.clone(), target.get_value(), comment.get_untracked()).await.map(|_| format!("Added to {t}.")),
            ));
        });
    };
    view! {
        <div class="text-xs">
            <button class="text-sky-700 hover:underline dark:text-sky-400" class:hidden=move || open.get() on:click=move |_| open.set(true)>"Add to a trail"</button>
            <div class="flex flex-wrap items-center gap-2" class:hidden=move || !open.get()>
                <input class=format!("{INPUT} w-48 py-1 text-xs") list="c6-trails" placeholder="trail (new or existing)" prop:value=move || trail.get() on:input=move |ev| trail.set(event_target_value(&ev)) />
                <datalist id="c6-trails">{move || trails.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|t| view! { <option value=t.name.clone()></option> }).collect_view()}</datalist>
                <input class=format!("{INPUT} min-w-0 flex-1 py-1 text-xs") placeholder="why it matters" prop:value=move || comment.get() on:input=move |ev| comment.set(event_target_value(&ev)) />
                <button class=BUTTON_QUIET on:click=add>"Add"</button>
                {move || note.get().map(|r| match r { Ok(m) => view! { <span class="text-emerald-600">{m}</span> }.into_any(), Err(e) => view! { <span class="text-red-600">{e}</span> }.into_any() })}
            </div>
        </div>
    }
}

#[component]
fn StatusLine(s: MemexStatus) -> impl IntoView {
    if !s.connected {
        return view! {
            <div class="rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
                "No memex is connected. Set one up in "
                <Go to=View::Settings { note: None, section: "memex".into() } class="underline">"Settings → Memex"</Go>
                {(s.outbox > 0).then(|| format!(" ({} updates are waiting for it).", s.outbox))}
            </div>
        }
        .into_any();
    }
    let total: u64 = s.items.values().sum();
    let parts: Vec<String> = s.items.iter().map(|(k, n)| format!("{n} {k}")).collect();
    let meaning = if s.embedding_model.is_empty() {
        "keyword search only (choose an embedding model in Settings → Memex)".to_owned()
    } else {
        format!("{} of {} passages embedded with {}", s.embedded, s.passages, s.embedding_model)
    };
    view! {
        <div class="space-y-1 text-xs text-zinc-500">
            <p>{format!("{} · {total} items ({}) · {} things · {meaning}", s.node.clone().unwrap_or_default(), parts.join(", "), s.things)}</p>
            {s.last_error.clone().map(|e| view! { <p class="text-red-600">{e}</p> })}
        </div>
    }
    .into_any()
}

#[component]
fn Hit(h: MemexHit, open: RwSignal<Option<String>>) -> impl IntoView {
    let id = h.id.clone();
    let session = (h.kind == "session" && h.source == "carbon6").then(|| h.external_id.clone());
    view! {
        <div class="space-y-1 px-3 py-2">
            <div class="flex flex-wrap items-center gap-2">
                <span class=format!("rounded px-1.5 text-[11px] {}", kind_tone(&h.kind))>{h.kind.clone()}</span>
                <button class="text-left font-medium hover:underline" on:click=move |_| open.set(Some(id.clone()))>{h.title.clone()}</button>
                <span class="text-xs text-zinc-500">{when(h.created_at)}</span>
                {h.matched.iter().map(|m| view! { <span class="text-[10px] uppercase tracking-wide text-zinc-400">{m.clone()}</span> }).collect_view()}
                <span class="flex-1"></span>
                {session.map(|sid| view! { <Go to=View::session(sid) class="text-xs text-sky-700 hover:underline dark:text-sky-400">"open session ↗"</Go> })}
                {h.url.clone().map(|u| view! { <a href=u.clone() target="_blank" rel="noopener noreferrer" class="text-xs text-sky-700 hover:underline dark:text-sky-400">"page ↗"</a> })}
            </div>
            <p class="text-sm text-zinc-600 dark:text-zinc-400">{h.snippet.clone()}</p>
        </div>
    }
}

/// An item in full, or a thing with its notes, items and related things.
#[component]
fn Detail(id: String, open: RwSignal<Option<String>>) -> impl IntoView {
    let data = LocalResource::new(move || api::memex_get(id.clone()));
    let error = RwSignal::new(None::<String>);
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div class="flex justify-end"><button class=BUTTON_QUIET on:click=move |_| open.set(None)>"Close"</button></div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || data.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.get("item").is_some() => item_view(v["item"].clone(), open, error).into_any(),
                Ok(v) => thing_view(v["thing"].clone(), open).into_any(),
            })}
        </section>
    }
}

fn chips(about: Vec<Value>, open: RwSignal<Option<String>>) -> impl IntoView {
    about
        .into_iter()
        .map(|t| {
            let label = t["label"].as_str().unwrap_or_default().to_owned();
            let l2 = label.clone();
            view! { <button class="rounded bg-zinc-100 px-1.5 text-[11px] hover:underline dark:bg-zinc-800" on:click=move |_| open.set(Some(l2.clone()))>{label}</button> }
        })
        .collect_view()
}

fn item_view(it: Value, open: RwSignal<Option<String>>, error: RwSignal<Option<String>>) -> impl IntoView {
    let s = |k: &str| it[k].as_str().unwrap_or_default().to_owned();
    let id = s("id");
    let has_original = it["meta"]["blob"].is_string() || it["meta"]["path"].is_string();
    let original = format!("/api/v1/memex/items/{id}/content");
    let trail_target = id.clone();
    let about = it["about"].as_array().cloned().unwrap_or_default();
    let remove = move |_| {
        if !window().confirm_with_message("Delete this from the memex? (The original, if any, is not touched.)").unwrap_or(false) {
            return;
        }
        let id = id.clone();
        spawn_local(async move {
            match api::memex_delete(id).await {
                Ok(()) => open.set(None),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <div class="flex flex-wrap items-center gap-2">
            <span class=format!("rounded px-1.5 text-[11px] {}", kind_tone(&s("kind")))>{s("kind")}</span>
            <h2 class="font-medium">{s("title")}</h2>
            <span class="text-xs text-zinc-500">{format!("{} · from {}", when(it["created_at"].as_i64().unwrap_or(0)), s("source"))}</span>
            <span class="flex-1"></span>
            {has_original.then(|| view! { <a href=original.clone() target="_blank" rel="noopener" class="text-xs text-sky-700 hover:underline dark:text-sky-400">"Open original ↗"</a> })}
            <button class="text-xs text-red-600 hover:underline" on:click=remove>"Delete"</button>
        </div>
        <div class="flex flex-wrap gap-1">{chips(about, open)}</div>
        <AddToTrail target=trail_target />
        <pre class="max-h-[32rem] overflow-auto whitespace-pre-wrap rounded-md bg-zinc-50 p-3 text-sm dark:bg-zinc-900">{s("text")}</pre>
    }
}

fn thing_view(t: Value, open: RwSignal<Option<String>>) -> impl IntoView {
    let thing = &t["thing"];
    let list = |key: &str| -> Vec<MemexHit> { serde_json::from_value(t[key].clone()).unwrap_or_default() };
    let (notes, items) = (list("notes"), list("items"));
    let related = t["related"].as_array().cloned().unwrap_or_default();
    let key = thing["id"].as_str().unwrap_or_default().to_owned();
    let rows = move |hits: Vec<MemexHit>| {
        hits.into_iter()
            .map(|h| {
                let id = h.id.clone();
                view! {
                    <li class="text-sm">
                        <span class=format!("mr-1 rounded px-1 text-[10px] {}", kind_tone(&h.kind))>{h.kind.clone()}</span>
                        <button class="hover:underline" on:click=move |_| open.set(Some(id.clone()))>{h.title.clone()}</button>
                        <span class="ml-1 text-xs text-zinc-500">{ago(now_ms() - h.created_at)}" ago"</span>
                    </li>
                }
            })
            .collect_view()
    };
    view! {
        <div class="flex items-center gap-2">
            <span class="rounded bg-violet-100 px-1.5 text-[11px] text-violet-800 dark:bg-violet-950 dark:text-violet-300">{thing["kind"].as_str().unwrap_or_default().to_owned()}</span>
            <h2 class="text-lg font-semibold">{thing["name"].as_str().unwrap_or_default().to_owned()}</h2>
            <span class="font-mono text-xs text-zinc-500">{thing["label"].as_str().unwrap_or_default().to_owned()}</span>
        </div>
        {(!notes.is_empty()).then(|| view! { <h3 class="text-xs font-medium uppercase text-zinc-500">"Notes"</h3><ul class="space-y-1">{rows(notes.clone())}</ul> })}
        {(!items.is_empty()).then(|| view! { <h3 class="text-xs font-medium uppercase text-zinc-500">"Items"</h3><ul class="space-y-1">{rows(items.clone())}</ul> })}
        {(!related.is_empty()).then(|| view! {
            <h3 class="text-xs font-medium uppercase text-zinc-500">"Related"</h3>
            <div class="flex flex-wrap gap-1">{chips(related.iter().map(|p| p[0].clone()).collect::<Vec<_>>(), open)}</div>
        })}
        <crate::understanding::ThingMore key open />
    }
}

/// Write a note (shown before any search).
#[component]
fn NoteBox() -> impl IntoView {
    let (text, about) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let status = RwSignal::new(None::<Result<String, String>>);
    let save = move |_| {
        let t = text.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        let labels = c6_api::labels::parse_labels(&about.get_untracked());
        spawn_local(async move {
            match api::memex_note(t, labels).await {
                Ok(_) => {
                    text.set(String::new());
                    status.set(Some(Ok("Saved to the memex.".into())));
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <h2 class="text-sm font-medium">"Add a note"</h2>
            <textarea class=format!("{INPUT} min-h-20") placeholder="A fact, decision or insight worth keeping…" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))></textarea>
            <div class="flex flex-wrap items-center gap-2">
                <input class=format!("{INPUT} min-w-0 flex-1 font-mono text-xs") placeholder="about (labels): e.g. client:acme, host:web1 — empty: every session"
                    prop:value=move || about.get() on:input=move |ev| about.set(event_target_value(&ev)) />
                <button class=BUTTON on:click=save>"Save"</button>
            </div>
            {move || status.get().map(|r| match r {
                Ok(m) => view! { <p class="text-xs text-emerald-600">{m}</p> }.into_any(),
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            })}
        </section>
    }
}

/// Settings → Memex: connecting one, its embedding model, the brief size.
#[component]
pub fn MemexSettings() -> impl IntoView {
    let status = LocalResource::new(api::memex_status);
    let loaded = LocalResource::new(move || api::get_setting("memex"));
    let connections = LocalResource::new(api::connections);
    let (conn, model, chars) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    let (auto, approve) = (RwSignal::new(false), RwSignal::new(true));
    // Model tasks: (connection, model) for understanding, vision, transcription.
    let tasks: [(&'static str, RwSignal<String>, RwSignal<String>); 3] =
        ["understanding", "vision", "transcription"].map(|k| (k, RwSignal::new(String::new()), RwSignal::new(String::new())));
    let per_day = RwSignal::new(String::new());
    // Files on the memex's machine: places, face models.
    let files: [(&'static str, RwSignal<String>); 3] =
        ["places_file", "face_detector", "face_embedder"].map(|k| (k, RwSignal::new(String::new())));
    Effect::new(move || {
        if let Some(Ok(v)) = loaded.get() {
            auto.set(v["contacts_auto"].as_bool().unwrap_or(false));
            approve.set(v["send_needs_approval"].as_bool().unwrap_or(true));
            conn.set(v["embedding"]["connection"].as_str().unwrap_or_default().to_owned());
            model.set(v["embedding"]["model"].as_str().unwrap_or_default().to_owned());
            chars.set(v["brief_chars"].as_u64().map(|n| n.to_string()).unwrap_or_default());
            for (k, c, m) in tasks {
                c.set(v[k]["connection"].as_str().unwrap_or_default().to_owned());
                m.set(v[k]["model"].as_str().unwrap_or_default().to_owned());
            }
            per_day.set(v["calls_per_day"].as_u64().map(|n| n.to_string()).unwrap_or_default());
            for (k, f) in files {
                f.set(v[k].as_str().unwrap_or_default().to_owned());
            }
        }
    });
    let token = RwSignal::new(None::<String>);
    let note = RwSignal::new(None::<Result<String, String>>);
    let make_token = move |_| {
        spawn_local(async move {
            match api::create_enrollment(Some("memex".into())).await {
                Ok(e) => token.set(Some(e.token)),
                Err(e) => note.set(Some(Err(e))),
            }
        });
    };
    let save = move |_| {
        let embedding = (!conn.get_untracked().is_empty())
            .then(|| serde_json::json!({"connection": conn.get_untracked(), "model": model.get_untracked().trim()}));
        let mut v = serde_json::json!({});
        if let Some(e) = embedding {
            v["embedding"] = e;
        }
        if let Ok(n) = chars.get_untracked().trim().parse::<u32>() {
            v["brief_chars"] = n.into();
        }
        v["contacts_auto"] = auto.get_untracked().into();
        v["send_needs_approval"] = approve.get_untracked().into();
        for (k, c, m) in tasks {
            if !c.get_untracked().is_empty() {
                v[k] = serde_json::json!({"connection": c.get_untracked(), "model": m.get_untracked().trim()});
            }
        }
        if let Ok(n) = per_day.get_untracked().trim().parse::<u32>() {
            v["calls_per_day"] = n.into();
        }
        for (k, f) in files {
            let path = f.get_untracked().trim().to_owned();
            if !path.is_empty() {
                v[k] = path.into();
            }
        }
        spawn_local(async move {
            note.set(Some(
                api::put_setting("memex", v)
                    .await
                    .map(|_| "Saved. The memex picks it up within a minute; new passages are embedded as they come.".to_owned()),
            ));
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Memex"</h2>
                <p class="text-sm text-zinc-500">"Your knowledge store, a separate service (c6m) on the fabric that can run on any machine. carbon6 sends it what your sessions do (digests, outputs, pages read, memory notes); sessions start with a brief from it about their labels, and agents can search it and save notes. What a session may see is set by scope rules below."</p>
            </div>
            {move || status.get().and_then(Result::ok).map(|s| if s.connected {
                view! { <p class="text-sm text-emerald-700 dark:text-emerald-400">{format!("Connected: {} · {} items", s.node.clone().unwrap_or_default(), s.items.values().sum::<u64>())}</p> }.into_any()
            } else {
                view! {
                    <div class="space-y-2 text-sm">
                        <p>"Not connected. On the machine that should hold it (this one is fine), with c6m installed:"</p>
                        <button class=BUTTON_QUIET on:click=make_token>"Create an enrollment token"</button>
                        {move || token.get().map(|t| view! {
                            <pre class="select-all overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-zinc-100 p-2 font-mono text-xs dark:bg-zinc-900">{format!("c6m enroll {t}\nc6m run")}</pre>
                        })}
                    </div>
                }.into_any()
            })}
            <div class="grid gap-2 sm:grid-cols-3">
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Embedding connection"</span>
                    <select class=format!("{INPUT} py-1 text-sm") prop:value=move || conn.get() on:change=move |ev| conn.set(event_target_value(&ev))>
                        <option value="">"None (keyword search only)"</option>
                        {move || connections.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| !matches!(c.kind.as_str(), "anthropic" | "claude_code" | "chatgpt")).map(|c| view! { <option value=c.name.clone()>{c.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Embedding model"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="e.g. nomic-embed-text, text-embedding-3-small" prop:value=move || model.get() on:input=move |ev| model.set(event_target_value(&ev)) />
                </label>
                <label class="flex items-center gap-2 text-sm sm:col-span-3">
                    <input type="checkbox" prop:checked=move || auto.get() on:change=move |ev| auto.set(event_target_checked(&ev)) />
                    "Add people you correspond with (written to twice, heard from) to Google Contacts, in a carbon6 group"
                </label>
                <label class="flex items-center gap-2 text-sm sm:col-span-3">
                    <input type="checkbox" prop:checked=move || approve.get() on:change=move |ev| approve.set(event_target_checked(&ev)) />
                    "Agents' mail and calendar invitations wait for my approval"
                </label>
                {tasks.into_iter().map(|(k, c, m)| {
                    let (title, hint) = match k {
                        "understanding" => ("Understanding", "e.g. claude-haiku-4-5, gpt-5-mini, qwen3:8b"),
                        "vision" => ("Vision (photos)", "e.g. claude-haiku-4-5, gpt-5-mini, qwen2.5vl"),
                        _ => ("Transcription (videos)", "e.g. whisper-1, gpt-4o-mini-transcribe"),
                    };
                    view! {
                        <label class="space-y-0.5">
                            <span class="text-xs text-zinc-500">{format!("{title}: connection")}</span>
                            <select class=format!("{INPUT} py-1 text-sm") prop:value=move || c.get() on:change=move |ev| c.set(event_target_value(&ev))>
                                <option value="">"None (off)"</option>
                                {move || connections.get().and_then(Result::ok).unwrap_or_default().into_iter()
                                    .filter(|x| x.kind != "claude_code" && (k != "transcription" || !matches!(x.kind.as_str(), "anthropic" | "chatgpt")))
                                    .map(|x| view! { <option value=x.name.clone()>{x.name.clone()}</option> }).collect_view()}
                            </select>
                        </label>
                        <label class="space-y-0.5 sm:col-span-2">
                            <span class="text-xs text-zinc-500">{format!("{title}: model")}</span>
                            <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder=hint prop:value=move || m.get() on:input=move |ev| m.set(event_target_value(&ev)) />
                        </label>
                    }
                }).collect_view()}
                <p class="text-xs text-zinc-500 sm:col-span-3">"Understanding reads mail threads, sessions, documents and papers for summaries, topics, commitments and claims, newest first. Vision captions photos and video frames. Everything they make is marked as inferred, with its model; change the model and it is redone."</p>
                {files.into_iter().map(|(k, f)| {
                    let (title, hint) = match k {
                        "places_file" => ("Places (GeoNames file)", "e.g. ~/memex/cities1000.txt"),
                        "face_detector" => ("Face detector (ONNX)", "e.g. ~/memex/version-RFB-320.onnx"),
                        _ => ("Face embedder (ONNX)", "e.g. ~/memex/arcface.onnx"),
                    };
                    view! {
                        <label class="space-y-0.5">
                            <span class="text-xs text-zinc-500">{title}</span>
                            <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder=hint prop:value=move || f.get() on:input=move |ev| f.set(event_target_value(&ev)) />
                        </label>
                    }
                }).collect_view()}
                <p class="text-xs text-zinc-500 sm:col-span-3">"Photos and videos in memex folders are read in place. Files above are paths on the memex's machine; nothing is downloaded for you. Places: a GeoNames cities file names where photos were taken. Faces: an UltraFace-style detector and an ArcFace- or MobileFaceNet-style embedder; faces are grouped and you name them in Review. Videos get keyframes and transcripts when ffmpeg is installed there."</p>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Model calls a day"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="500" prop:value=move || per_day.get() on:input=move |ev| per_day.set(event_target_value(&ev)) />
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Brief size (characters)"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="4000; 0 for none" prop:value=move || chars.get() on:input=move |ev| chars.set(event_target_value(&ev)) />
                </label>
            </div>
            <div class="flex items-center justify-end gap-3">
                {move || note.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                <button class=BUTTON_QUIET on:click=move |_| {
                    spawn_local(async move {
                        note.set(Some(api::memex_scan().await.map(|v| format!("Scanned the memex folders: {} files read.", v["read"].as_u64().unwrap_or(0)))));
                    });
                }>"Scan folders now"</button>
                <button class=BUTTON on:click=save>"Save"</button>
            </div>
        </section>
    }
}
