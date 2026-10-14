//! Collections (docs/os.md §4.1): typed tables of records. A table per
//! collection, with forms made from its schema, and each record's history.

use c6_api::{Collection, Record, RecordIn, RecordQuery};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Map, Value, json};

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, View};

/// Columns a table shows at most.
const MAX_COLUMNS: usize = 6;

#[component]
pub fn CollectionsPage(name: String) -> impl IntoView {
    if name.is_empty() { view! { <CollectionList /> }.into_any() } else { view! { <CollectionView name=name /> }.into_any() }
}

#[component]
fn CollectionList() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::collections()
    });
    let creating = RwSignal::new(false);
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-6">
            <div class="flex flex-wrap items-center gap-2">
                <h1 class="text-lg font-semibold">"Collections"</h1>
                <span class="flex-1 text-sm text-zinc-500">"Typed tables of records that apps and agents keep in your realm. Every record matches its collection's schema, and every change is kept."</span>
                <button class=BUTTON on:click=move |_| creating.update(|c| *c = !*c)>"New collection"</button>
            </div>
            {move || creating.get().then(|| view! { <NewCollection done=Callback::new(move |()| { creating.set(false); tick.update(|t| *t += 1); }) /> })}
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(cs) => {
                    let (system, mine): (Vec<Collection>, Vec<Collection>) = cs.into_iter().partition(|c| c.system);
                    view! {
                        {mine.is_empty().then(|| view! { <p class="text-sm text-zinc-500">"No collections yet. Agents create them with the records tools, apps bring their own, or make one here."</p> })}
                        {mine.into_iter().map(card).collect_view()}
                        <SeriesList />
                        {(!system.is_empty()).then(|| view! {
                            <details class="pt-2">
                                <summary class="cursor-pointer select-none text-sm text-zinc-500">{format!("carbon6's own configuration ({})", system.len())}</summary>
                                <div class="mt-2 space-y-2">{system.into_iter().map(card).collect_view()}</div>
                            </details>
                        })}
                    }.into_any()
                }
            })}
        </main>
    }
}

/// Series (docs/os.md §4.4): numbers over time, with today's and the
/// week's totals at a glance.
#[component]
fn SeriesList() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::series()
    });
    let (name, unit, dims) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    let error = RwSignal::new(None::<String>);
    let create = move |_| {
        let s = c6_api::SeriesIn {
            name: name.get_untracked(),
            unit: Some(unit.get_untracked()),
            dimensions: Some(dims.get_untracked().split(',').map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()).collect()),
            ..Default::default()
        };
        spawn_local(async move {
            match api::create_series(s).await {
                Ok(_) => {
                    name.set(String::new());
                    unit.set(String::new());
                    dims.set(String::new());
                    error.set(None);
                    tick.update(|t| *t += 1);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-2 pt-4">
            <h2 class="font-medium">"Series"</h2>
            <p class="text-sm text-zinc-500">"Numbers over time: points recorded by triggers, agents (series_record) or the API, with dimensions such as service. Triggers watch them for thresholds. Model usage is here too, computed from the usage ledger."</p>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(all) => all.into_iter().map(|s| {
                    let totals = LocalResource::new({
                        let n = s.name.clone();
                        move || {
                            let n = n.clone();
                            async move {
                                let day = api::query_series(n.clone(), c6_api::SeriesQuery { minutes: Some(1440), ..Default::default() }).await.ok();
                                let week = api::query_series(n, c6_api::SeriesQuery { minutes: Some(7 * 1440), ..Default::default() }).await.ok();
                                (day.and_then(|d| d.value), week.and_then(|w| w.value))
                            }
                        }
                    });
                    let n = s.name.clone();
                    let unit = s.unit.clone();
                    let fmt = move |v: Option<f64>| v.map(|v| if v.fract() == 0.0 { format!("{v}") } else { format!("{v:.2}") }).unwrap_or_else(|| "—".into());
                    view! {
                        <div class="flex flex-wrap items-center gap-2 rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-800">
                            <span class="font-mono font-medium">{s.name.clone()}</span>
                            <span class="text-xs text-zinc-500">{s.unit.clone()}</span>
                            {(!s.dimensions.is_empty()).then(|| view! { <span class="text-xs text-zinc-500">{format!("by {}", s.dimensions.join(", "))}</span> })}
                            <span class="min-w-0 flex-1 truncate text-xs text-zinc-500">{s.description.clone()}</span>
                            <span class="text-xs">{move || totals.get().map(|(d, w)| format!("sum 24h {} · 7d {} {unit}", fmt(d), fmt(w)))}</span>
                            {(!s.system).then(|| view! {
                                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                    let n = n.clone();
                                    if window().confirm_with_message(&format!("Delete series {n} and its points?")).unwrap_or(false) {
                                        spawn_local(async move {
                                            match api::delete_series(n).await {
                                                Ok(()) => tick.update(|t| *t += 1),
                                                Err(e) => error.set(Some(e)),
                                            }
                                        });
                                    }
                                }>"Delete"</button>
                            })}
                        </div>
                    }
                }).collect_view().into_any(),
            })}
            <div class="flex flex-wrap gap-2">
                <input class=format!("{INPUT} w-40") placeholder="name (e.g. revenue)" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} w-24") placeholder="unit" prop:value=move || unit.get() on:input=move |ev| unit.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} min-w-0 flex-1") placeholder="dimensions (e.g. service)" prop:value=move || dims.get() on:input=move |ev| dims.set(event_target_value(&ev)) />
                <button class=BUTTON on:click=create>"New series"</button>
            </div>
        </section>
    }
}

fn card(c: Collection) -> impl IntoView {
    view! {
                    <Go to=View::Collections { name: c.name.clone() } class="block rounded-md border border-zinc-200 px-4 py-3 hover:bg-zinc-50 dark:border-zinc-800 dark:hover:bg-zinc-900">
                        <div class="flex items-baseline gap-2">
                            <span class="font-medium">{c.name.clone()}</span>
                            <span class="text-xs text-zinc-500">{format!("{} record{}", c.records, if c.records == 1 { "" } else { "s" })}</span>
                            {c.system.then(|| view! { <span class="rounded bg-zinc-100 px-1 text-xs text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400">"carbon6's own"</span> })}
                        </div>
                        <div class="text-sm text-zinc-500">{c.description.clone()}</div>
                    </Go>
    }
}

#[component]
fn NewCollection(done: Callback<()>) -> impl IntoView {
    let name = RwSignal::new(String::new());
    let description = RwSignal::new(String::new());
    let schema = RwSignal::new(
        serde_json::to_string_pretty(
            &json!({"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}, "notes": {"type": "string"}}}),
        )
        .unwrap_or_default(),
    );
    let keys = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let save = move |_| {
        let schema: Value = match serde_json::from_str(&schema.get_untracked()) {
            Ok(v) => v,
            Err(e) => return error.set(Some(format!("the schema is not JSON: {e}"))),
        };
        let c = c6_api::CollectionIn {
            name: name.get_untracked(),
            description: Some(description.get_untracked()),
            schema: Some(schema),
            keys: Some(keys.get_untracked().split(',').map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()).collect()),
        };
        spawn_local(async move {
            match api::create_collection(c).await {
                Ok(_) => done.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-2 rounded-md border border-zinc-200 p-4 dark:border-zinc-800">
            <input class=INPUT placeholder="Name: letters, digits, - and _" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
            <input class=INPUT placeholder="What it holds" prop:value=move || description.get() on:input=move |ev| description.set(event_target_value(&ev)) />
            <label class="block text-xs text-zinc-500">"Schema (JSON Schema for one record; fields it does not name are rejected)"</label>
            <textarea class=format!("{INPUT} h-48 font-mono text-xs") prop:value=move || schema.get() on:input=move |ev| schema.set(event_target_value(&ev))></textarea>
            <input class=INPUT placeholder="Unique fields, comma-separated (optional)" prop:value=move || keys.get() on:input=move |ev| keys.set(event_target_value(&ev)) />
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <button class=BUTTON on:click=save>"Create"</button>
        </section>
    }
}

/// A field's value as table text.
fn cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => {
            a.iter().map(|x| x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(", ")
        }
        Some(other) => other.to_string(),
    }
}

#[component]
fn CollectionView(name: String) -> impl IntoView {
    let name = StoredValue::new(name);
    let tick = RwSignal::new(0u32);
    let text = RwSignal::new(String::new());
    let info = LocalResource::new(move || api::collection(name.get_value()));
    let page = LocalResource::new(move || {
        tick.track();
        let q = RecordQuery { text: text.get(), ..Default::default() };
        api::query_records(name.get_value(), q)
    });
    // None: no form; Some(None): a new record; Some(Some(r)): editing r.
    let editing = RwSignal::new(None::<Option<Record>>);
    let gone = RwSignal::new(None::<String>);
    let column = crate::workspace::use_column();
    let refresh = Callback::new(move |()| {
        editing.set(None);
        tick.update(|t| *t += 1);
    });
    view! {
        <main class="mx-auto max-w-5xl space-y-4 px-4 py-6">
            {move || info.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(c) => {
                    let columns: Vec<String> = ordered_fields(&c.schema).into_iter().map(|(k, _)| k).take(MAX_COLUMNS).collect();
                    let c = StoredValue::new(c);
                    let columns = StoredValue::new(columns);
                    view! {
                        <div class="flex flex-wrap items-center gap-2">
                            <Go to=View::Collections { name: String::new() } class=format!("{BUTTON_QUIET} text-sm")>"← Collections"</Go>
                            <h1 class="text-lg font-semibold">{c.with_value(|c| c.name.clone())}</h1>
                            <span class="flex-1 text-sm text-zinc-500">{c.with_value(|c| c.description.clone())}</span>
                            <input class=format!("{INPUT} w-48 py-1 text-sm") placeholder="Search" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev)) />
                            {(!c.with_value(|c| c.system)).then(|| view! { <button class=BUTTON on:click=move |_| editing.set(Some(None))>"Add a record"</button> })}
                            {c.with_value(|c| c.system).then(|| view! { <span class="text-xs text-zinc-500">"carbon6's own: change it under Settings"</span> })}
                            {(!c.with_value(|c| c.system)).then(|| view! {
                                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                    let n = name.get_value();
                                    if !window().confirm_with_message(&format!("Delete the collection {n} and all its records? Their history is kept.")).unwrap_or(false) {
                                        return;
                                    }
                                    spawn_local(async move {
                                        match api::drop_collection(n).await {
                                            Ok(()) => match column {
                                                Some(c) => c.replace(View::Collections { name: String::new() }),
                                                None => gone.set(Some("Deleted.".into())),
                                            },
                                            Err(e) => gone.set(Some(e)),
                                        }
                                    });
                                }>"Delete collection"</button>
                            })}
                        </div>
                        {move || gone.get().map(|e| view! { <ErrorBox message=e /> })}
                        {move || editing.get().map(|r| view! { <RecordForm collection=c.get_value() record=r done=refresh /> })}
                        {move || page.get().map(|p| match p {
                            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                            Ok(p) if p.records.is_empty() => view! { <p class="text-sm text-zinc-500">"No records."</p> }.into_any(),
                            Ok(p) => view! {
                                <div class="overflow-x-auto rounded-md border border-zinc-200 dark:border-zinc-800">
                                    <table class="w-full text-left text-sm">
                                        <thead class="bg-zinc-50 text-xs text-zinc-500 dark:bg-zinc-900">
                                            <tr>
                                                {columns.get_value().into_iter().map(|col| view! { <th class="px-3 py-2 font-medium">{col}</th> }).collect_view()}
                                                <th class="px-3 py-2 font-medium">"Changed"</th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            {p.records.into_iter().map(|r| {
                                                let row = r.clone();
                                                view! {
                                                    <tr class="cursor-pointer border-t border-zinc-200 hover:bg-zinc-50 dark:border-zinc-800 dark:hover:bg-zinc-900"
                                                        on:click=move |_| if !c.with_value(|c| c.system) { editing.set(Some(Some(row.clone()))) }>
                                                        {columns.get_value().into_iter().map(|col| view! { <td class="max-w-xs truncate px-3 py-1.5">{cell(r.fields.get(&col))}</td> }).collect_view()}
                                                        <td class="whitespace-nowrap px-3 py-1.5 text-xs text-zinc-500">{format!("{} · {}", ago(now_ms() - r.updated_at), r.updated_by)}</td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                                <p class="text-xs text-zinc-500">{format!("{} record{}", p.total, if p.total == 1 { "" } else { "s" })}</p>
                            }.into_any(),
                        })}
                    }.into_any()
                }
            })}
        </main>
    }
}

/// How a field is edited, from its schema.
#[derive(Clone)]
enum Editor {
    Text {
        long: bool,
    },
    Number {
        integer: bool,
    },
    Flag,
    Choice(Vec<String>),
    /// A list of strings, comma-separated.
    List,
    /// Anything else, as JSON.
    Json,
}

/// A schema's fields, required ones first.
fn ordered_fields(schema: &Value) -> Vec<(String, Value)> {
    let required: Vec<&str> = schema["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut fields: Vec<(String, Value)> =
        schema["properties"].as_object().map(|p| p.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    fields.sort_by_key(|(k, _)| (required.iter().position(|r| r == k).unwrap_or(usize::MAX), k.clone()));
    fields
}

/// Long text: a long limit, or a name that says so.
fn long_text(name: &str, schema: &Value) -> bool {
    schema["maxLength"].as_u64().is_some_and(|n| n > 200)
        || ["note", "description", "body", "text", "detail", "summary", "prompt"].iter().any(|w| name.to_lowercase().contains(w))
}

fn editor_for(name: &str, schema: &Value) -> Editor {
    if let Some(options) = schema["enum"].as_array().filter(|o| o.iter().all(Value::is_string)) {
        return Editor::Choice(options.iter().filter_map(|o| o.as_str().map(str::to_owned)).collect());
    }
    let ty = match &schema["type"] {
        Value::String(t) => t.clone(),
        // ["string", "null"] and the like: the non-null one.
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("").to_owned(),
        _ => String::new(),
    };
    match ty.as_str() {
        "string" => Editor::Text { long: long_text(name, schema) },
        "integer" => Editor::Number { integer: true },
        "number" => Editor::Number { integer: false },
        "boolean" => Editor::Flag,
        "array" if schema["items"]["type"] == "string" => Editor::List,
        _ => Editor::Json,
    }
}

/// A field's value as the editor's text.
fn as_text(editor: &Editor, v: Option<&Value>) -> String {
    match (editor, v) {
        (_, None | Some(Value::Null)) => String::new(),
        (Editor::List, Some(Value::Array(a))) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
        (Editor::Json, Some(v)) => serde_json::to_string_pretty(v).unwrap_or_default(),
        (_, Some(Value::String(s))) => s.clone(),
        (_, Some(v)) => v.to_string(),
    }
}

/// The editor's text as a field's value (`None`: leave the field out).
fn from_text(editor: &Editor, text: &str) -> Result<Option<Value>, String> {
    let t = text.trim();
    // Empty (a checkbox never touched, too): leave the field out.
    if t.is_empty() {
        return Ok(None);
    }
    Ok(Some(match editor {
        Editor::Text { .. } | Editor::Choice(_) => json!(text),
        Editor::Number { integer: true } => json!(t.parse::<i64>().map_err(|_| format!("{t:?} is not a whole number"))?),
        Editor::Number { integer: false } => match t.parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) => json!(t.parse::<f64>().map_err(|_| format!("{t:?} is not a number"))?),
        },
        Editor::Flag => json!(t == "true"),
        Editor::List => json!(t.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>()),
        Editor::Json => serde_json::from_str(t).map_err(|e| format!("not JSON: {e}"))?,
    }))
}

/// A form for a record, made from the collection's schema.
#[component]
fn RecordForm(collection: Collection, record: Option<Record>, done: Callback<()>) -> impl IntoView {
    let props = ordered_fields(&collection.schema);
    let required: Vec<String> = collection.schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let fields: Vec<(String, Editor, String, RwSignal<String>)> = props
        .iter()
        .map(|(k, s)| {
            let e = editor_for(k, s);
            let start = as_text(&e, record.as_ref().and_then(|r| r.fields.get(k)));
            (k.clone(), e, s["description"].as_str().unwrap_or("").to_owned(), RwSignal::new(start))
        })
        .collect();
    let fields = StoredValue::new(fields);
    let error = RwSignal::new(None::<String>);
    let name = StoredValue::new(collection.name.clone());
    let id = StoredValue::new(record.as_ref().map(|r| r.id.clone()));
    let version = record.as_ref().map(|r| r.version);
    let (links, labels) = (record.as_ref().map(|r| r.links.clone()), record.as_ref().map(|r| r.labels.clone()));
    let history = RwSignal::new(None::<Vec<c6_api::RecordChange>>);
    let save = move |_| {
        let mut out = Map::new();
        for (k, e, _, v) in fields.get_value() {
            match from_text(&e, &v.get_untracked()) {
                Ok(Some(x)) => {
                    out.insert(k, x);
                }
                Ok(None) => {}
                Err(msg) => return error.set(Some(format!("{k}: {msg}"))),
            }
        }
        let r = RecordIn { fields: Value::Object(out), links: links.clone(), labels: labels.clone(), version };
        spawn_local(async move {
            match api::save_record(name.get_value(), id.get_value(), r).await {
                Ok(_) => done.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let remove = move |_| {
        let Some(rid) = id.get_value() else { return };
        if !window().confirm_with_message("Delete this record? Its history is kept.").unwrap_or(false) {
            return;
        }
        spawn_local(async move {
            match api::delete_record(name.get_value(), rid).await {
                Ok(()) => done.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let show_history = move |_| {
        let Some(rid) = id.get_value() else { return };
        spawn_local(async move {
            match api::record_history(name.get_value(), rid).await {
                Ok(h) => history.set(Some(h)),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-3 rounded-md border border-zinc-200 p-4 dark:border-zinc-800">
            <div class="text-sm font-medium">{if record.is_some() { "Edit the record" } else { "A new record" }}</div>
            {fields.get_value().into_iter().map(|(k, e, help, v)| {
                let label = format!("{k}{}", if required.contains(&k) { " *" } else { "" });
                let input = match e {
                    Editor::Choice(options) => view! {
                        <select class=INPUT prop:value=move || v.get() on:change=move |ev| v.set(event_target_value(&ev))>
                            <option value="">"—"</option>
                            {options.into_iter().map(|o| view! { <option value=o.clone()>{o.clone()}</option> }).collect_view()}
                        </select>
                    }.into_any(),
                    Editor::Flag => view! {
                        <div><input type="checkbox" prop:checked=move || v.get() == "true" on:change=move |ev| v.set(event_target_checked(&ev).to_string()) /></div>
                    }.into_any(),
                    Editor::Text { long: true } | Editor::Json => view! {
                        <textarea class=format!("{INPUT} h-20 font-mono text-xs") prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev))></textarea>
                    }.into_any(),
                    Editor::List => view! {
                        <input class=INPUT placeholder="comma-separated" prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev)) />
                    }.into_any(),
                    _ => view! { <input class=INPUT prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev)) /> }.into_any(),
                };
                view! {
                    <label class="block space-y-1">
                        <span class="text-xs text-zinc-500">{label}{(!help.is_empty()).then(|| format!(" · {help}"))}</span>
                        {input}
                    </label>
                }
            }).collect_view()}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex gap-2">
                <button class=BUTTON on:click=save>"Save"</button>
                <button class=BUTTON_QUIET on:click=move |_| done.run(())>"Cancel"</button>
                <span class="flex-1"></span>
                {record.is_some().then(|| view! {
                    <button class=format!("{BUTTON_QUIET} text-xs") on:click=show_history>"History"</button>
                    <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=remove>"Delete"</button>
                })}
            </div>
            {move || history.get().map(|h| view! {
                <ol class="space-y-1 text-xs">
                    {h.into_iter().rev().map(|c| view! {
                        <li class="rounded bg-zinc-50 p-2 dark:bg-zinc-900">
                            <div class="text-zinc-500">{format!("v{} · {} · {} · {}", c.version, c.op, c.changed_by, ago(now_ms() - c.changed_at))}</div>
                            <pre class="whitespace-pre-wrap font-mono">{serde_json::to_string(&c.fields).unwrap_or_default()}</pre>
                        </li>
                    }).collect_view()}
                </ol>
            })}
        </section>
    }
}
