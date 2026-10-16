//! Views (docs/os.md §4.5): a realm's configured screens, rendered from
//! their configuration (Settings → Views): a table or board of records, a
//! record, a chart of a series, a feed of decisions or sessions, or a page
//! embedding others; and messages, a calendar or a gallery of items of a
//! shared kind (itemviews.rs). Each realm's views are in the header's menu.

use c6_api::{Record, RecordQuery, View as ViewDef, ViewAct};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Map, Value};

use crate::api;
use crate::util::{BUTTON_QUIET, ErrorBox, ago, now_ms};
use crate::workspace::{Go, View};

/// The header's menu of the realm's views.
#[component]
pub fn ViewsMenu(class: &'static str) -> impl IntoView {
    let views = LocalResource::new(move || {
        api::realm().track();
        api::views()
    });
    let menu = NodeRef::<leptos::html::Details>::new();
    let close = move |_| {
        if let Some(d) = menu.get_untracked() {
            d.set_open(false);
        }
    };
    move || {
        let vs: Vec<ViewDef> = views.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|v| v.menu).collect();
        (!vs.is_empty()).then(|| {
            view! {
                <details node_ref=menu class="relative shrink-0">
                    <summary class=format!("{class} cursor-pointer list-none whitespace-nowrap")>"Views ▾"</summary>
                    <div class="absolute left-0 top-8 z-40 flex w-56 flex-col rounded-lg border border-zinc-200 bg-white p-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900" on:click=close>
                        {vs.into_iter().map(|v| {
                            let title = if v.title.is_empty() { v.name.clone() } else { v.title.clone() };
                            view! { <Go to=View::App { name: v.name.clone(), record: String::new() } class="rounded-md px-2 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800">{title}</Go> }
                        }).collect_view()}
                    </div>
                </details>
            }
        })
    }
}

/// A view by name; with a record id, that record in the view's context.
#[component]
pub fn ViewPage(name: String, record: String) -> impl IntoView {
    let views = LocalResource::new(move || {
        api::realm().track();
        api::views()
    });
    view! {
        <main class="mx-auto max-w-6xl space-y-4 px-4 py-6">
            {move || views.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(all) => match all.iter().find(|v| v.name == name).cloned() {
                    None => view! { <p class="text-sm text-zinc-500">{format!("No view {name} in this realm (Settings → Views).")}</p> }.into_any(),
                    Some(v) => {
                        let title = if v.title.is_empty() { v.name.clone() } else { v.title.clone() };
                        view! {
                            <h1 class="text-lg font-semibold">{title}</h1>
                            {if record.is_empty() {
                                view! { <ViewBody view=v all=all /> }.into_any()
                            } else {
                                view! { <RecordBody view=v id=record.clone() /> }.into_any()
                            }}
                        }.into_any()
                    }
                },
            })}
        </main>
    }
}

/// A view's content (also embedded in pages).
#[component]
pub(crate) fn ViewBody(view: ViewDef, all: Vec<ViewDef>) -> impl IntoView {
    match view.kind.as_str() {
        "table" => view! { <TableView view=view /> }.into_any(),
        "board" => view! { <BoardView view=view /> }.into_any(),
        "chart" => view! { <ChartView view=view /> }.into_any(),
        "feed" => view! { <FeedView view=view /> }.into_any(),
        "page" => view! { <PageView view=view all=all /> }.into_any(),
        "messages" => view! { <crate::itemviews::MessagesView view=view /> }.into_any(),
        "calendar" => view! { <crate::itemviews::CalendarView view=view /> }.into_any(),
        "gallery" => view! { <crate::itemviews::GalleryView view=view /> }.into_any(),
        "record" => match view.source["id"].as_str() {
            Some(id) => view! { <RecordBody id=id.to_owned() view=view.clone() /> }.into_any(),
            None => view! { <TableView view=view /> }.into_any(),
        },
        other => view! { <p class="text-sm text-red-600">{format!("unknown kind {other}")}</p> }.into_any(),
    }
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().map(|x| text_of(Some(x))).collect::<Vec<_>>().join(", "),
        Some(v) => v.to_string(),
    }
}

/// The query a view's source describes.
fn query_of(source: &Value) -> RecordQuery {
    RecordQuery {
        filter: source["where"].as_object().cloned().unwrap_or_default(),
        sort: source["sort"].as_str().unwrap_or("").to_owned(),
        limit: Some(source["limit"].as_u64().unwrap_or(200) as u32),
        text: source["text"].as_str().unwrap_or("").to_owned(),
        ..Default::default()
    }
}

/// The fields to show: the view's, else the schema's (required first).
fn fields_of(view: &ViewDef, schema: &Value) -> Vec<String> {
    if !view.fields.is_empty() {
        return view.fields.clone();
    }
    let required: Vec<String> = serde_json::from_value(schema["required"].clone()).unwrap_or_default();
    let mut rest: Vec<String> =
        schema["properties"].as_object().map(|p| p.keys().filter(|k| !required.contains(k)).cloned().collect()).unwrap_or_default();
    rest.sort();
    required.into_iter().chain(rest).collect()
}

/// Records of a view's collection, refreshed when `tick` changes.
fn records_of(view: &ViewDef, tick: RwSignal<u32>) -> LocalResource<Result<(c6_api::Collection, Vec<Record>), String>> {
    let collection = view.source["collection"].as_str().unwrap_or_default().to_owned();
    let q = query_of(&view.source);
    LocalResource::new(move || {
        tick.track();
        api::realm().track();
        let (c, q) = (collection.clone(), q.clone());
        async move {
            let col = api::collection(c.clone()).await?;
            let page = api::query_records(c, q).await?;
            Ok((col, page.records))
        }
    })
}

/// The view's action buttons for one record.
#[component]
fn Actions(view: ViewDef, record: String, tick: RwSignal<u32>, error: RwSignal<Option<String>>) -> impl IntoView {
    view.actions
        .iter()
        .map(|a| {
            let (name, label, record) = (view.name.clone(), a.label.clone(), record.clone());
            let shown = a.label.clone();
            view! {
                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |ev| {
                    ev.stop_propagation();
                    let act = ViewAct { action: label.clone(), record: record.clone(), ..Default::default() };
                    let name = name.clone();
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

#[component]
fn TableView(view: ViewDef) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let error = RwSignal::new(None::<String>);
    let data = records_of(&view, tick);
    let v = StoredValue::new(view);
    view! {
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || data.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok((col, recs)) => {
                let view = v.get_value();
                let fields = fields_of(&view, &col.schema);
                view! {
                    <div class="overflow-x-auto rounded-md border border-zinc-200 dark:border-zinc-800">
                        <table class="w-full text-sm">
                            <thead class="bg-zinc-50 text-left text-xs text-zinc-500 dark:bg-zinc-900">
                                <tr>{fields.iter().map(|f| view! { <th class="px-2 py-1.5 font-medium">{f.clone()}</th> }).collect_view()}<th></th></tr>
                            </thead>
                            <tbody>
                                {recs.into_iter().map(|rec| {
                                    let cells = fields.iter().map(|f| text_of(rec.fields.get(f))).collect::<Vec<_>>();
                                    let first = cells.first().cloned().unwrap_or_default();
                                    let rest = cells.into_iter().skip(1).collect::<Vec<_>>();
                                    let to = View::App { name: view.name.clone(), record: rec.id.clone() };
                                    view! {
                                        <tr class="border-t border-zinc-100 dark:border-zinc-800">
                                            <td class="px-2 py-1.5"><Go to=to class="underline decoration-zinc-300 hover:decoration-zinc-600">{if first.is_empty() { "(open)".to_owned() } else { first }}</Go></td>
                                            {rest.into_iter().map(|c| view! { <td class="max-w-xs truncate px-2 py-1.5">{c}</td> }).collect_view()}
                                            <td class="whitespace-nowrap px-2 py-1 text-right"><Actions view=view.clone() record=rec.id.clone() tick error /></td>
                                        </tr>
                                    }
                                }).collect_view()}
                            </tbody>
                        </table>
                    </div>
                }.into_any()
            }
        })}
    }
}

#[component]
fn BoardView(view: ViewDef) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let error = RwSignal::new(None::<String>);
    let data = records_of(&view, tick);
    let v = StoredValue::new(view);
    // Moves a card: sets the column field.
    let set = move |collection: String, rec: Record, field: String, to: String| {
        spawn_local(async move {
            let mut fields = rec.fields.as_object().cloned().unwrap_or_default();
            fields.insert(field, Value::String(to));
            let r = c6_api::RecordIn { fields: Value::Object(fields), version: Some(rec.version), ..Default::default() };
            match api::save_record(collection, Some(rec.id.clone()), r).await {
                Ok(_) => {
                    error.set(None);
                    tick.update(|t| *t += 1);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || data.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok((col, recs)) => {
                let view = v.get_value();
                let field = view.group_by.clone();
                let mut columns = view.columns.clone();
                if columns.is_empty() {
                    columns = serde_json::from_value(col.schema["properties"][&field]["enum"].clone()).unwrap_or_default();
                }
                for r in &recs {
                    let c = text_of(r.fields.get(&field));
                    if !columns.contains(&c) {
                        columns.push(c);
                    }
                }
                let shown: Vec<String> = fields_of(&view, &col.schema).into_iter().filter(|f| *f != field).collect();
                let ncols = columns.len();
                view! {
                    <div class="flex gap-3 overflow-x-auto pb-2">
                        {columns.iter().enumerate().map(|(i, c)| {
                            let in_col: Vec<Record> = recs.iter().filter(|r| text_of(r.fields.get(&field)) == *c).cloned().collect();
                            let (prev, next) = (i.checked_sub(1).map(|p| columns[p].clone()), (i + 1 < ncols).then(|| columns[i + 1].clone()));
                            view! {
                                <div class="w-64 shrink-0 space-y-2 rounded-lg bg-zinc-100 p-2 dark:bg-zinc-900">
                                    <div class="flex items-center justify-between px-1 text-xs font-medium text-zinc-500">
                                        <span>{if c.is_empty() { "(none)".to_owned() } else { c.clone() }}</span><span>{in_col.len()}</span>
                                    </div>
                                    {in_col.into_iter().map(|rec| {
                                        let title = text_of(shown.first().and_then(|f| rec.fields.get(f)));
                                        let details = shown.iter().skip(1).take(3).filter_map(|f| {
                                            let t = text_of(rec.fields.get(f));
                                            (!t.is_empty()).then(|| format!("{f}: {t}"))
                                        }).collect::<Vec<_>>();
                                        let (r1, r2) = (rec.clone(), rec.clone());
                                        let (f1, f2) = (field.clone(), field.clone());
                                        let (c1, c2) = (col.name.clone(), col.name.clone());
                                        let (p, n) = (prev.clone(), next.clone());
                                        view! {
                                            <div class="space-y-1 rounded-md border border-zinc-200 bg-white p-2 text-sm shadow-sm dark:border-zinc-700 dark:bg-zinc-950">
                                                <Go to=View::App { name: view.name.clone(), record: rec.id.clone() } class="font-medium hover:underline">{if title.is_empty() { "(open)".to_owned() } else { title }}</Go>
                                                {details.into_iter().map(|d| view! { <p class="truncate text-xs text-zinc-500">{d}</p> }).collect_view()}
                                                <div class="flex flex-wrap items-center gap-1">
                                                    {p.map(|to| { let t = to.clone(); view! { <button class=format!("{BUTTON_QUIET} text-xs") title=format!("Move to {t}") on:click=move |_| set(c1.clone(), r1.clone(), f1.clone(), to.clone())>"←"</button> } })}
                                                    {n.map(|to| { let t = to.clone(); view! { <button class=format!("{BUTTON_QUIET} text-xs") title=format!("Move to {t}") on:click=move |_| set(c2.clone(), r2.clone(), f2.clone(), to.clone())>"→"</button> } })}
                                                    <Actions view=view.clone() record=rec.id.clone() tick error />
                                                </div>
                                            </div>
                                        }
                                    }).collect_view()}
                                </div>
                            }
                        }).collect_view()}
                    </div>
                }.into_any()
            }
        })}
    }
}

/// One record: its fields, links, labels, history, the view's actions,
/// and the sessions its actions started.
#[component]
fn RecordBody(view: ViewDef, id: String) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let error = RwSignal::new(None::<String>);
    let collection = view.source["collection"].as_str().unwrap_or_default().to_owned();
    let (c1, id1) = (collection.clone(), id.clone());
    let data = LocalResource::new(move || {
        tick.track();
        let (c, id) = (c1.clone(), id1.clone());
        async move {
            let mut q = RecordQuery { limit: Some(1), ..Default::default() };
            q.filter.insert("id".into(), Value::String(id.clone()));
            let rec = api::query_records(c.clone(), q).await?.records.into_iter().next().ok_or("no such record")?;
            let history = api::record_history(c, id).await.unwrap_or_default();
            Ok::<_, String>((rec, history))
        }
    });
    let live = crate::api::live();
    let label = format!("record:{id}");
    let v = StoredValue::new(view);
    view! {
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || data.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok((rec, history)) => {
                let view = v.get_value();
                let fields: Vec<(String, String)> = rec.fields.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), text_of(Some(v)))).collect()).unwrap_or_default();
                view! {
                    <div class="flex flex-wrap items-center gap-2">
                        <Go to=View::App { name: view.name.clone(), record: String::new() } class="text-sm text-zinc-500 hover:underline">{format!("← {}", if view.title.is_empty() { view.name.clone() } else { view.title.clone() })}</Go>
                        <span class="flex-1"></span>
                        <Actions view=view.clone() record=rec.id.clone() tick error />
                    </div>
                    <dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 rounded-md border border-zinc-200 p-3 text-sm dark:border-zinc-800">
                        {fields.into_iter().map(|(k, v)| view! { <dt class="text-zinc-500">{k}</dt><dd class="whitespace-pre-wrap break-words">{v}</dd> }).collect_view()}
                    </dl>
                    {(!rec.links.is_empty() || !rec.labels.is_empty()).then(|| view! {
                        <p class="text-xs text-zinc-500">{format!("links: {} · labels: {}", rec.links.join(", "), rec.labels.join(", "))}</p>
                    })}
                    <h2 class="text-sm font-medium">"Sessions"</h2>
                    {
                        let label = label.clone();
                        move || {
                            let ss: Vec<c6_api::Session> = live.sessions.get().into_iter().filter(|s| s.labels.contains(&label) || s.auto_labels.contains(&label)).collect();
                            if ss.is_empty() {
                                view! { <p class="text-xs text-zinc-500">"None yet: the view's actions start sessions about this record."</p> }.into_any()
                            } else {
                                ss.into_iter().map(|s| view! {
                                    <div class="text-sm"><Go to=View::Session { id: s.id.clone(), target: None } class="hover:underline">{s.title.clone()}</Go>
                                    <span class="ml-2 text-xs text-zinc-500">{format!("{:?} · {}", s.status, ago(now_ms() - s.updated_at))}</span></div>
                                }).collect_view().into_any()
                            }
                        }
                    }
                    <h2 class="text-sm font-medium">"History"</h2>
                    <div class="space-y-0.5 text-xs text-zinc-500">
                        {history.into_iter().rev().map(|h| view! { <p>{format!("v{} {} by {} · {}", h.version, h.op, h.changed_by, ago(now_ms() - h.changed_at))}</p> }).collect_view()}
                    </div>
                }.into_any()
            }
        })}
    }
}

#[component]
fn ChartView(view: ViewDef) -> impl IntoView {
    let s = view.source.clone();
    let name = s["series"].as_str().unwrap_or_default().to_owned();
    let q = c6_api::SeriesQuery {
        agg: s["agg"].as_str().unwrap_or("sum").to_owned(),
        minutes: Some(s["minutes"].as_u64().unwrap_or(30 * 1440)),
        bucket: s["bucket"].as_str().unwrap_or("day").to_owned(),
        filter: s["where"].as_object().cloned().unwrap_or_default(),
        ..Default::default()
    };
    let group_by = s["group_by"].as_str().unwrap_or("").to_owned();
    let data = LocalResource::new(move || {
        api::realm().track();
        let (name, q, g) = (name.clone(), q.clone(), group_by.clone());
        async move {
            let total = api::query_series(name.clone(), q.clone()).await?;
            let groups = if g.is_empty() {
                None
            } else {
                Some(api::query_series(name, c6_api::SeriesQuery { group_by: g, bucket: String::new(), ..q }).await?)
            };
            Ok::<_, String>((total, groups))
        }
    });
    view! {
        {move || data.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok((res, groups)) => {
                let max = res.buckets.iter().map(|b| b.1).fold(0.0f64, f64::max).max(1e-9);
                let n = res.buckets.len().max(1);
                let w = 600.0 / n as f64;
                let fmt = |v: f64| if v.fract() == 0.0 { format!("{v}") } else { format!("{v:.2}") };
                view! {
                    <p class="text-sm text-zinc-500">{format!("{} of {} · {}", res.agg, res.series, res.value.map(fmt).unwrap_or_else(|| "no points".into()))}</p>
                    <svg viewBox="0 0 600 160" class="h-40 w-full rounded-md border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
                        {res.buckets.iter().enumerate().map(|(i, (t, v))| {
                            let h = v / max * 140.0;
                            let title = format!("{}: {}", js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(*t as f64)).to_locale_date_string("default", &wasm_bindgen::JsValue::UNDEFINED), fmt(*v));
                            view! { <rect x=format!("{:.1}", i as f64 * w + 1.0) y=format!("{:.1}", 150.0 - h) width=format!("{:.1}", (w - 2.0).max(1.0)) height=format!("{h:.1}") class="fill-sky-500"><title>{title}</title></rect> }
                        }).collect_view()}
                    </svg>
                    {groups.map(|g| view! {
                        <div class="space-y-0.5 text-sm">
                            {g.groups.into_iter().map(|(k, v)| view! { <p><span class="font-mono">{text_of(Some(&k))}</span>{format!(": {}", fmt(v))}</p> }).collect_view()}
                        </div>
                    })}
                }.into_any()
            }
        })}
    }
}

#[component]
fn FeedView(view: ViewDef) -> impl IntoView {
    if let Some(labels) = view.source.get("sessions") {
        let labels: Vec<String> = serde_json::from_value(labels["labels"].clone()).unwrap_or_default();
        let live = crate::api::live();
        return view! {
            <div class="space-y-1">
                {move || {
                    let realm = api::realm().get();
                    let mut ss: Vec<c6_api::Session> = live.sessions.get().into_iter()
                        .filter(|s| realm.is_empty() || s.realm == realm)
                        .filter(|s| labels.iter().all(|l| s.labels.contains(l) || s.auto_labels.contains(l)))
                        .collect();
                    ss.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
                    ss.into_iter().take(50).map(|s| view! {
                        <div class="flex items-center gap-2 border-b border-zinc-100 py-1 text-sm dark:border-zinc-800">
                            <span class="w-24 shrink-0 text-xs text-zinc-500">{ago(now_ms() - s.updated_at)}</span>
                            <Go to=View::Session { id: s.id.clone(), target: None } class="min-w-0 flex-1 truncate hover:underline">{s.title.clone()}</Go>
                            <span class="text-xs text-zinc-500">{format!("{:?}", s.status)}</span>
                        </div>
                    }).collect_view()
                }}
            </div>
        }
        .into_any();
    }
    let tick = RwSignal::new(0u32);
    let state = view.source["decisions"].as_str().unwrap_or("open").to_owned();
    let data = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::decisions(state.clone())
    });
    let error = RwSignal::new(None::<String>);
    view! {
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || data.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok(ds) => {
                let realm = api::realm().get_untracked();
                let ds: Vec<c6_api::Decision> = ds.into_iter().filter(|d| realm.is_empty() || d.realm == realm).collect();
                if ds.is_empty() {
                    return view! { <p class="text-sm text-zinc-500">"Nothing waiting."</p> }.into_any();
                }
                ds.into_iter().map(|d| {
                    let id = d.id;
                    view! {
                        <div class="space-y-1 border-b border-zinc-100 py-2 text-sm dark:border-zinc-800">
                            <div class="flex items-center gap-2">
                                <span class="w-24 shrink-0 text-xs text-zinc-500">{ago(now_ms() - d.created_at)}</span>
                                <span class="min-w-0 flex-1 font-medium">{d.title.clone()}</span>
                                {(d.priority == "high").then(|| view! { <span class="text-xs text-red-600">"high"</span> })}
                            </div>
                            <div class="flex flex-wrap gap-1 pl-26 max-sm:pl-0">
                                {d.options.iter().map(|o| {
                                    let label = o.label.clone();
                                    let shown = o.label.clone();
                                    view! { <button class=format!("{BUTTON_QUIET} text-xs") title=o.does.clone() on:click=move |_| {
                                        let a = c6_api::DecisionAnswer { option: Some(label.clone()), values: Map::new(), note: String::new() };
                                        spawn_local(async move {
                                            match api::answer_decision(id, a).await {
                                                Ok(_) => { error.set(None); tick.update(|t| *t += 1); }
                                                Err(e) => error.set(Some(e)),
                                            }
                                        });
                                    }>{shown}</button> }
                                }).collect_view()}
                                <Go to=View::Decisions class="text-xs text-zinc-500 underline">"open in Decisions"</Go>
                            </div>
                        </div>
                    }
                }).collect_view().into_any()
            }
        })}
    }
    .into_any()
}

/// Markdown with embedded views: {{view:name}}.
#[component]
fn PageView(view: ViewDef, all: Vec<ViewDef>) -> impl IntoView {
    let mut parts: Vec<AnyView> = Vec::new();
    let mut rest = view.body.as_str();
    while let Some(i) = rest.find("{{view:") {
        let Some(j) = rest[i..].find("}}") else { break };
        let before = &rest[..i];
        if !before.trim().is_empty() {
            parts.push(view! { <div class="md text-sm" inner_html=crate::markdown::to_html(before)></div> }.into_any());
        }
        let name = rest[i + 7..i + j].trim();
        // A page embeds other views, not pages (no loops).
        match all.iter().find(|v| v.name == name && v.kind != "page") {
            Some(v) => {
                let title = if v.title.is_empty() { v.name.clone() } else { v.title.clone() };
                parts.push(
                    view! {
                        <section class="space-y-2">
                            <Go to=View::App { name: v.name.clone(), record: String::new() } class="text-sm font-medium hover:underline">{title}</Go>
                            <ViewBody view=v.clone() all=Vec::new() />
                        </section>
                    }
                    .into_any(),
                );
            }
            None => parts.push(view! { <p class="text-sm text-red-600">{format!("(no view {name} to embed)")}</p> }.into_any()),
        }
        rest = &rest[i + j + 2..];
    }
    if !rest.trim().is_empty() {
        parts.push(view! { <div class="md text-sm" inner_html=crate::markdown::to_html(rest)></div> }.into_any());
    }
    view! { <div class="space-y-4">{parts}</div> }
}
