//! Documents (docs/docs.md §12): the Docs list, and a document's panel.
//! Click a block to edit its Markdown in place; typing sends the change
//! when you pause or leave the block, guarded by the version you read, so
//! someone else's edit since is never overwritten. A document updates live
//! as others (and agents) write it.

use c6_api::docs::{Doc, DocBatch, DocBlock, DocNew, DocOp, DocPatch, DocQuery, DocRestore, DocSummary, DocVersion};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Map, Value};

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, View, use_column, use_workspace};

fn since(at: i64) -> String {
    ago(now_ms() - at)
}

/// Who wrote something, as you read it.
pub fn author(by: &str) -> String {
    match by {
        "you" => "you".into(),
        "assistant" => "the assistant".into(),
        b if b.starts_with("session:") => "a session".into(),
        b if b.starts_with("token:") => "an API token".into(),
        b => b.to_owned(),
    }
}

/// A session's title for `session:<id>`, from the live feed.
fn author_named(by: &str) -> String {
    if let Some(id) = by.strip_prefix("session:") {
        let live = api::live();
        let title = live.sessions.with(|ss| ss.iter().find(|s| s.id == id).map(|s| s.title.clone()));
        return match title.filter(|t| !t.trim().is_empty()) {
            Some(t) if t.chars().count() > 32 => format!("session {}…", t.chars().take(30).collect::<String>().trim_end()),
            Some(t) => format!("session {t}"),
            None => "a session".into(),
        };
    }
    author(by)
}

fn kind_mark(kind: &str) -> &'static str {
    if kind == "deck" { "▦" } else { "▤" }
}

/// Reloads `tick` whenever a document changes over the live feed (`id`:
/// only that one).
fn reload_on_change(tick: RwSignal<u32>, id: Option<String>) {
    let live = api::live();
    Effect::new(move |_| {
        if let Some(c) = live.doc.get()
            && id.as_ref().is_none_or(|i| *i == c.id)
        {
            tick.update(|t| *t += 1);
        }
    });
}

/// The header link.
#[component]
pub fn DocsLink(class: &'static str) -> impl IntoView {
    view! { <Go to=View::Docs { id: String::new() } class=class title="Docs and decks: what people and agents write together">"Docs"</Go> }
}

/// What chips and embeds in a document panel share: the document, the
/// things its ref chips name as they are now, and the chip being changed
/// (its block, key and value).
#[derive(Clone, Copy)]
struct ChipCtx {
    doc: StoredValue<String>,
    refs: RwSignal<std::collections::HashMap<String, c6_api::work::LiveAttached>>,
    editing: RwSignal<Option<(String, String, Value)>>,
    send: StoredValue<Box<dyn Fn(Vec<DocOp>) + Send + Sync>>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// A block's Markdown as HTML with its chips as small widgets (docs/docs.md
/// §3): a date and a dropdown you click to change, a mention, and a ref
/// with the thing's title and state as they are now.
fn chips_html(text: &str, attrs: &Value, refs: &std::collections::HashMap<String, c6_api::work::LiveAttached>) -> String {
    let chips = attrs.get("chips").and_then(Value::as_object);
    let mut md = String::with_capacity(text.len());
    let mut shown = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<?c6 block ") {
        let after = &rest[i + 11..];
        let Some(j) = after.find("?>") else { break };
        let key = after[..j].trim();
        md.push_str(&rest[..i]);
        let v = chips.and_then(|c| c.get(key)).cloned().unwrap_or(Value::Null);
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        const CHIP: &str = "mx-0.5 rounded px-1 py-px text-[0.92em]";
        let html = match v["type"].as_str().unwrap_or("") {
            "date" => format!(
                "<button type=\"button\" data-chip=\"{}\" title=\"A date: click to change it\" class=\"{CHIP} bg-sky-50 text-sky-800 hover:bg-sky-100 dark:bg-sky-950/40 dark:text-sky-300\">📅 {}</button>",
                esc(key),
                esc(&s("value"))
            ),
            "mention" => format!(
                "<span class=\"{CHIP} bg-violet-50 text-violet-800 dark:bg-violet-950/40 dark:text-violet-300\">@{}</span>",
                esc(&if s("name").is_empty() { s("user") } else { s("name") })
            ),
            "dropdown" => format!(
                "<button type=\"button\" data-chip=\"{}\" title=\"Pick another\" class=\"{CHIP} bg-zinc-100 text-zinc-800 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-200\">{} ▾</button>",
                esc(key),
                esc(&s("value"))
            ),
            "ref" => {
                let r = s("ref");
                let live = refs.get(&r);
                let title = live
                    .map(|l| l.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| if s("title").is_empty() { r.clone() } else { s("title") });
                let state = live.map(|l| l.state.clone()).unwrap_or_default();
                format!(
                    "<button type=\"button\" data-ref=\"{}\" title=\"{}\" class=\"{CHIP} bg-emerald-50 text-emerald-800 hover:underline dark:bg-emerald-950/40 dark:text-emerald-300\">↗ {}{}</button>",
                    esc(&r),
                    esc(&r),
                    esc(&title),
                    if state.is_empty() {
                        String::new()
                    } else {
                        format!(" <span class=\"text-[10px] uppercase tracking-wide opacity-70\">{}</span>", esc(&state.replace('_', " ")))
                    }
                )
            }
            _ => format!("[{}]", esc(key)),
        };
        md.push_str(&format!("\u{E000}{}\u{E001}", shown.len()));
        shown.push(html);
        rest = &after[j + 2..];
    }
    md.push_str(rest);
    let mut html = crate::markdown::to_html(&md);
    for (i, h) in shown.iter().enumerate() {
        html = html.replace(&format!("\u{E000}{i}\u{E001}"), h);
    }
    html
}

/// The ref chips' references in a document.
fn ref_chips(d: &Doc) -> Vec<String> {
    fn walk(b: &DocBlock, out: &mut Vec<String>) {
        for v in b.attrs.get("chips").and_then(Value::as_object).into_iter().flat_map(|c| c.values()) {
            if v["type"] == "ref"
                && let Some(r) = v["ref"].as_str()
                && !out.iter().any(|x| x == r)
            {
                out.push(r.to_owned());
            }
        }
        for c in &b.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    for t in &d.tabs {
        for b in &t.blocks {
            walk(b, &mut out);
        }
    }
    out
}

/// Where a ref chip's thing opens.
fn ref_path(r: &str) -> Option<String> {
    let (kind, id) = r.split_once(':')?;
    Some(match kind {
        "session" => format!("/sessions/{id}"),
        "work" => format!("/work/{id}"),
        "project" => format!("/projects/{id}"),
        "doc" => format!("/docs/{id}"),
        "decision" => "/decisions".to_owned(),
        _ => return None,
    })
}

/// A chip or ref clicked in a block: handled here (and the block isn't
/// opened for editing).
fn chip_clicked(
    ev: &web_sys::MouseEvent,
    block: &DocBlock,
    ctx: ChipCtx,
    col: Option<crate::workspace::ColumnCtx>,
    ws: crate::workspace::Workspace,
) -> bool {
    use wasm_bindgen::JsCast;
    let Some(el) =
        ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()).and_then(|e| e.closest("[data-chip],[data-ref]").ok().flatten())
    else {
        return false;
    };
    ev.prevent_default();
    ev.stop_propagation();
    if let Some(r) = el.get_attribute("data-ref") {
        if r.starts_with("http://") || r.starts_with("https://") {
            let _ = window().open_with_url_and_target(&r, "_blank");
        } else if let Some(path) = ref_path(&r) {
            let to = View::from_path(&path);
            match col {
                Some(c) => c.open(to, How::Beside),
                None => ws.open(None, to, How::Beside),
            }
        }
        return true;
    }
    let key = el.get_attribute("data-chip").unwrap_or_default();
    fn find(b: &DocBlock, key: &str) -> Option<(String, Value)> {
        if let Some(v) = b.attrs.get("chips").and_then(|c| c.get(key)) {
            return Some((b.id.clone(), v.clone()));
        }
        b.children.iter().find_map(|c| find(c, key))
    }
    if let Some((b, v)) = find(block, &key) {
        ctx.editing.set(Some((b, key, v)));
    }
    true
}

/// Changing a date or picking a dropdown's option.
#[component]
fn ChipEditor(block: String, key: String, value: Value, ctx: ChipCtx) -> impl IntoView {
    let current = RwSignal::new(value["value"].as_str().unwrap_or("").to_owned());
    let kind = value["type"].as_str().unwrap_or("").to_owned();
    let options: Vec<String> = value["options"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let (b, k, v) = (StoredValue::new(block), StoredValue::new(key), StoredValue::new(value));
    let save = move |to: String| {
        let mut nv = v.get_value();
        nv["value"] = Value::String(to);
        let op = DocOp::Set {
            title: None,
            theme: None,
            size: None,
            tab: None,
            tab_title: None,
            tabs: None,
            block: Some(b.get_value()),
            chip: Some(k.get_value()),
            value: Some(nv),
        };
        ctx.editing.set(None);
        ctx.send.with_value(|f| f(vec![op]));
    };
    view! {
        <div class="my-1 flex flex-wrap items-center gap-2 rounded border border-zinc-200 bg-white p-2 text-sm shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
            on:click=|ev| ev.stop_propagation()>
            {if kind == "dropdown" {
                options.into_iter().map(|o| {
                    let picked = o == current.get_untracked();
                    let o2 = o.clone();
                    view! {
                        <button class=format!("rounded px-2 py-0.5 {}", if picked { "bg-violet-600 text-white" } else { "bg-zinc-100 hover:bg-zinc-200 dark:bg-zinc-800" })
                            on:click=move |_| save(o2.clone())>{o}</button>
                    }
                }).collect_view().into_any()
            } else if kind == "date" {
                view! {
                    <input type="date" class=format!("{INPUT} w-auto!") prop:value=move || current.get() on:input=move |ev| current.set(event_target_value(&ev)) />
                    <button class=format!("{BUTTON} text-xs") on:click=move |_| save(current.get_untracked())>"Set"</button>
                }.into_any()
            } else {
                view! {
                    <input class=format!("{INPUT} w-auto!") prop:value=move || current.get() on:input=move |ev| current.set(event_target_value(&ev)) />
                    <button class=format!("{BUTTON} text-xs") on:click=move |_| save(current.get_untracked())>"Set"</button>
                }.into_any()
            }}
            <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| ctx.editing.set(None)>"Cancel"</button>
        </div>
    }
}

/// An embed (docs/docs.md §3): a picture, a file, a diagram, a chart drawn
/// from its rows, or a live view of the realm.
#[component]
fn Embed(v: Value) -> impl IntoView {
    let ctx = use_context::<ChipCtx>();
    let doc = ctx.map(|c| c.doc.get_value()).unwrap_or_default();
    let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
    let file_url = |blob: &str| api::url(&format!("/api/v1/docs/{}/files/{}", api::enc(&doc), api::enc(blob)));
    match v["type"].as_str().unwrap_or("") {
        "image" => view! {
            <figure class="my-2">
                <img src=file_url(&s("blob")) alt=if s("alt").is_empty() { s("name") } else { s("alt") } class="max-h-[32rem] max-w-full rounded border border-zinc-200 dark:border-zinc-800" />
                {(!s("caption").is_empty()).then(|| view! { <figcaption class="mt-1 text-xs text-zinc-500">{s("caption")}</figcaption> })}
            </figure>
        }.into_any(),
        "file" => view! {
            <a href=file_url(&s("blob")) class="my-1 inline-flex items-center gap-2 rounded border border-zinc-200 px-2 py-1 text-sm text-sky-700 hover:bg-zinc-50 dark:border-zinc-800 dark:text-sky-400 dark:hover:bg-zinc-900"
                on:click=|ev| ev.stop_propagation()>
                "📎 " {s("name")}
                <span class="text-xs text-zinc-500">{v["size"].as_u64().map(|n| format!("{} KB", n.div_ceil(1024))).unwrap_or_default()}</span>
            </a>
        }.into_any(),
        // An agent's SVG, drawn as a picture: it can't run or restyle the page.
        "diagram" => {
            let svg = s("svg");
            let src = format!("data:image/svg+xml;base64,{}", base64_of(svg.as_bytes()));
            view! {
                <figure class="my-2">
                    <img src=src alt=s("title") class="max-w-full" />
                    {(!s("title").is_empty()).then(|| view! { <figcaption class="mt-1 text-xs text-zinc-500">{s("title")}</figcaption> })}
                </figure>
            }.into_any()
        }
        "chart" => view! { <Chart v=v.clone() /> }.into_any(),
        "view" => view! { <EmbedView name=s("view") /> }.into_any(),
        other => view! {
            <div class="rounded border border-zinc-200 px-3 py-2 text-sm text-zinc-500 dark:border-zinc-800">{format!("{other} (not shown here)")}</div>
        }.into_any(),
    }
}

const SERIES: [&str; 6] = ["#7c3aed", "#0284c7", "#059669", "#d97706", "#db2777", "#475569"];

/// A chart drawn from the rows the document keeps: {chart: bar or line,
/// title, columns: [label, series…], rows: [[label, value…]]}. Hover a
/// value to read it; "Data" shows the rows.
#[component]
fn Chart(v: Value) -> impl IntoView {
    let kind = v["chart"].as_str().unwrap_or("bar").to_owned();
    let title = v["title"].as_str().unwrap_or("").to_owned();
    let columns: Vec<String> = v["columns"].as_array().into_iter().flatten().map(|c| c.as_str().unwrap_or("").to_owned()).collect();
    let rows: Vec<Vec<Value>> = v["rows"].as_array().into_iter().flatten().filter_map(|r| r.as_array().cloned()).collect();
    let num = |x: &Value| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok())).unwrap_or(0.0);
    let series = columns.len().saturating_sub(1).max(rows.first().map(|r| r.len().saturating_sub(1)).unwrap_or(0));
    let values: Vec<f64> = rows.iter().flat_map(|r| r.iter().skip(1).map(num)).collect();
    let max = values.iter().cloned().fold(0.0_f64, f64::max).max(1e-9);
    let min = values.iter().cloned().fold(0.0_f64, f64::min);
    let (w, h, left, bottom, top) = (640.0, 260.0, 44.0, 28.0, 10.0);
    let plot_h = h - bottom - top;
    let y = move |val: f64| top + plot_h * (1.0 - (val - min) / (max - min));
    let n = rows.len().max(1) as f64;
    let band = (w - left - 8.0) / n;
    let mut marks = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let label = r.first().map(|l| l.as_str().map(str::to_owned).unwrap_or_else(|| l.to_string())).unwrap_or_default();
        let x0 = left + band * i as f64;
        marks.push(format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"middle\" fill=\"currentColor\" opacity=\"0.7\">{}</text>",
            x0 + band / 2.0,
            h - 10.0,
            esc(&label)
        ));
        if kind != "line" {
            let bw = (band * 0.8) / series.max(1) as f64;
            for k in 0..series {
                let val = r.get(k + 1).map(num).unwrap_or(0.0);
                let (yv, y0) = (y(val), y(0.0));
                let name = columns.get(k + 1).cloned().unwrap_or_default();
                marks.push(format!(
                    "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"{}\" rx=\"2\"><title>{}{}: {}</title></rect>",
                    x0 + band * 0.1 + bw * k as f64,
                    yv.min(y0),
                    (bw - 2.0).max(1.0),
                    (y0 - yv).abs().max(0.5),
                    SERIES[k % SERIES.len()],
                    esc(&label),
                    if name.is_empty() { String::new() } else { format!(" · {}", esc(&name)) },
                    val
                ));
            }
        }
    }
    if kind == "line" {
        for k in 0..series {
            let pts: Vec<(f64, f64, f64)> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let val = r.get(k + 1).map(num).unwrap_or(0.0);
                    (left + band * i as f64 + band / 2.0, y(val), val)
                })
                .collect();
            let path = pts.iter().map(|(x, y, _)| format!("{x:.1},{y:.1}")).collect::<Vec<_>>().join(" ");
            marks.push(format!("<polyline points=\"{path}\" fill=\"none\" stroke=\"{}\" stroke-width=\"2\" />", SERIES[k % SERIES.len()]));
            for (i, (x, yv, val)) in pts.iter().enumerate() {
                let label = rows[i].first().map(|l| l.as_str().map(str::to_owned).unwrap_or_else(|| l.to_string())).unwrap_or_default();
                marks.push(format!(
                    "<circle cx=\"{x:.1}\" cy=\"{yv:.1}\" r=\"3.5\" fill=\"{}\"><title>{}: {val}</title></circle>",
                    SERIES[k % SERIES.len()],
                    esc(&label)
                ));
            }
        }
    }
    // The axis: zero, and the top value.
    marks.push(format!(
        "<line x1=\"{left}\" x2=\"{:.1}\" y1=\"{:.1}\" y2=\"{:.1}\" stroke=\"currentColor\" opacity=\"0.3\" />",
        w - 8.0,
        y(0.0),
        y(0.0)
    ));
    for val in [max, min].into_iter().filter(|v| *v != 0.0).chain([0.0]) {
        marks.push(format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"end\" fill=\"currentColor\" opacity=\"0.6\">{}</text>",
            left - 6.0,
            y(val) + 4.0,
            if val.fract() == 0.0 { format!("{val:.0}") } else { format!("{val:.1}") }
        ));
    }
    let svg = format!("<svg viewBox=\"0 0 {w} {h}\" class=\"w-full\" role=\"img\">{}</svg>", marks.join(""));
    let data = RwSignal::new(false);
    let legend: Vec<String> = columns.iter().skip(1).cloned().collect();
    view! {
        <figure class="my-2 rounded border border-zinc-200 p-2 dark:border-zinc-800" on:click=|ev| ev.stop_propagation()>
            <div class="flex items-center gap-2 text-sm">
                <span class="flex-1 font-medium">{title}</span>
                {legend.iter().enumerate().map(|(k, l)| view! {
                    <span class="flex items-center gap-1 text-xs text-zinc-500"><span class="inline-block size-2 rounded-sm" style=format!("background:{}", SERIES[k % SERIES.len()])></span>{l.clone()}</span>
                }).collect_view()}
                <button class="text-xs text-zinc-500 hover:underline" on:click=move |_| data.update(|d| *d = !*d)>"Data"</button>
            </div>
            <div class="text-zinc-700 dark:text-zinc-300" inner_html=svg></div>
            {move || data.get().then(|| view! {
                <table class="mt-1 w-full text-xs">
                    <thead><tr>{columns.iter().map(|c| view! { <th class="px-1 text-left font-medium">{c.clone()}</th> }).collect_view()}</tr></thead>
                    <tbody>{rows.iter().map(|r| view! {
                        <tr>{r.iter().map(|c| view! { <td class="px-1">{c.as_str().map(str::to_owned).unwrap_or_else(|| c.to_string())}</td> }).collect_view()}</tr>
                    }).collect_view()}</tbody>
                </table>
            })}
        </figure>
    }
}

/// A view of the realm, live, inside a doc (docs/os.md §4.5).
#[component]
fn EmbedView(name: String) -> impl IntoView {
    let views = LocalResource::new(api::views);
    view! {
        <div class="my-2 rounded border border-zinc-200 p-2 dark:border-zinc-800" on:click=|ev| ev.stop_propagation()>
            {move || views.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(all) => match all.iter().find(|v| v.name == name).cloned() {
                    None => view! { <p class="text-sm text-zinc-500">{format!("No view {name} in this realm.")}</p> }.into_any(),
                    Some(v) => view! {
                        <div class="mb-1 text-xs font-medium uppercase tracking-wide text-zinc-500">{if v.title.is_empty() { v.name.clone() } else { v.title.clone() }}</div>
                        <crate::views::ViewBody view=v all=all />
                    }.into_any(),
                },
            })}
        </div>
    }
}

// --- The list ---------------------------------------------------------------

#[component]
pub fn DocsPage() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    reload_on_change(tick, None);
    let kind = RwSignal::new(String::new());
    let q = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    let by = RwSignal::new(String::new());
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        let query = DocQuery {
            kind: Some(kind.get()).filter(|k| !k.is_empty()),
            q: Some(q.get()).filter(|k| !k.trim().is_empty()),
            label: Some(label.get()).filter(|k| !k.trim().is_empty()),
            by: Some(by.get()).filter(|k| !k.is_empty()),
            ..Default::default()
        };
        async move { api::docs_list(&query).await }
    });
    let shown = RwSignal::new(None::<Result<Vec<DocSummary>, String>>);
    Effect::new(move || {
        if let Some(r) = list.get() {
            shown.set(Some(r));
        }
    });
    let error = RwSignal::new(None::<String>);
    let making = RwSignal::new(None::<&'static str>);
    let title = RwSignal::new(String::new());
    let (col, ws) = (use_column(), use_workspace());
    let open_doc = move |id: String, how: How| match &col {
        Some(c) => c.open(View::Docs { id }, how),
        None => ws.open(None, View::Docs { id }, how),
    };
    let open_doc = StoredValue::new(open_doc);
    let make = move || {
        let Some(k) = making.get_untracked() else { return };
        let t = title.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        let n = DocNew { kind: k.into(), title: t, ..Default::default() };
        spawn_local(async move {
            match api::doc_create(&n).await {
                Ok(d) => {
                    making.set(None);
                    title.set(String::new());
                    open_doc.with_value(|f| f(d.summary.id, How::Beside));
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let importing = RwSignal::new(false);
    let import_file = move |ev: leptos::ev::Event| {
        use wasm_bindgen::JsCast;
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
        let name = file.name();
        importing.set(true);
        spawn_local(async move {
            let result = async {
                let buf =
                    wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await.map_err(|_| "could not read the file".to_owned())?;
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                let content = base64_of(&bytes);
                api::doc_import(&c6_api::docs::DocImport { name, content, ..Default::default() }).await
            }
            .await;
            importing.set(false);
            match result {
                Ok(d) => open_doc.with_value(|f| f(d.summary.id, How::Beside)),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let filter = |value: &'static str, label_text: &'static str| {
        view! {
            <button class=move || format!("rounded-md px-2 py-1 text-sm {}", if kind.get() == value { "bg-zinc-200 dark:bg-zinc-800" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-900" })
                on:click=move |_| kind.set(value.into())>{label_text}</button>
        }
    };
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-5">
            <header class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                <h1 class="text-xl font-semibold">"Docs"</h1>
                <span class="text-sm text-zinc-500">"Docs and decks you and agents write together"</span>
            </header>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex flex-wrap items-center gap-2">
                <button class=BUTTON on:click=move |_| making.set(Some("doc"))>"+ New doc"</button>
                <button class=BUTTON_QUIET on:click=move |_| making.set(Some("deck"))>"+ New deck"</button>
                <label class=format!("{BUTTON_QUIET} cursor-pointer") title="A Word, OpenDocument, PowerPoint, HTML or Markdown file becomes a doc or a deck">
                    {move || if importing.get() { "Importing…" } else { "Import…" }}
                    <input type="file" class="hidden" accept=".docx,.odt,.pptx,.html,.htm,.md,.markdown,.txt" on:change=import_file />
                </label>
                <span class="flex-1"></span>
                {filter("", "All")}
                {filter("doc", "Docs")}
                {filter("deck", "Decks")}
            </div>
            {move || making.get().map(|k| view! {
                <div class="flex gap-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                    <input class=INPUT placeholder=if k == "deck" { "The deck's title" } else { "The doc's title" } prop:value=move || title.get()
                        on:input=move |ev| title.set(event_target_value(&ev)) on:keydown=move |ev| if ev.key() == "Enter" { make() } />
                    <button class=BUTTON_QUIET on:click=move |_| making.set(None)>"Cancel"</button>
                    <button class=BUTTON on:click=move |_| make()>"Make it"</button>
                </div>
            })}
            <div class="flex flex-wrap gap-2">
                <input class=format!("{INPUT} max-w-xs") placeholder="Search titles…" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} max-w-[10rem]") placeholder="Label" prop:value=move || label.get() on:input=move |ev| label.set(event_target_value(&ev)) />
                <select class=format!("{INPUT} max-w-[10rem]") on:change=move |ev| by.set(event_target_value(&ev))>
                    <option value="">"Anyone"</option>
                    <option value="you">"You"</option>
                    <option value="session">"Sessions"</option>
                    <option value="assistant">"The assistant"</option>
                </select>
            </div>
            {move || match shown.get() {
                None => view! { <p class="text-sm text-zinc-500">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <ErrorBox message=e /> }.into_any(),
                Some(Ok(l)) if l.is_empty() => view! {
                    <p class="text-sm text-zinc-500">"No documents yet. Make one above, or ask a session to write one (it gets the doc tool)."</p>
                }.into_any(),
                Some(Ok(l)) => view! {
                    <div class="divide-y divide-zinc-100 dark:divide-zinc-900">
                        {l.into_iter().map(|d| {
                            let id = d.id.clone();
                            view! {
                                <a href=View::Docs { id: d.id.clone() }.href() class="flex items-center gap-2 px-2 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900"
                                    on:click=move |ev: web_sys::MouseEvent| {
                                        if ev.button() != 0 { return }
                                        ev.prevent_default();
                                        open_doc.with_value(|f| f(id.clone(), How::from_mouse(&ev)));
                                    }>
                                    <span class="shrink-0 text-zinc-400" title=d.kind.clone()>{kind_mark(&d.kind)}</span>
                                    <span class="min-w-0 flex-1 truncate font-medium">{d.title.clone()}</span>
                                    {(d.pending > 0).then(|| view! {
                                        <span class="shrink-0 rounded bg-violet-100 px-1.5 text-[11px] text-violet-800 dark:bg-violet-900/40 dark:text-violet-300"
                                            title=format!("Still being written by {}", d.writers.iter().map(|w| author_named(w)).collect::<Vec<_>>().join(", "))>
                                            {format!("✎ {} to write", d.pending)}
                                        </span>
                                    })}
                                    {d.labels.iter().map(|l| view! { <span class="shrink-0 text-[11px] text-zinc-500">{format!("#{l}")}</span> }).collect_view()}
                                    <span class="max-w-[40%] shrink truncate text-xs text-zinc-400">{format!("{} · {}", author_named(&d.updated_by), since(d.updated_at))}</span>
                                </a>
                            }
                        }).collect_view()}
                    </div>
                }.into_any(),
            }}
        </main>
    }
}

// --- A document -------------------------------------------------------------

/// A document, open: its tabs, blocks to click and edit, history, and what
/// refers to it.
#[component]
pub fn DocPage(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    reload_on_change(tick, Some(id.get_value()));
    let loaded = LocalResource::new(move || {
        tick.track();
        api::doc_get(id.get_value())
    });
    let doc = RwSignal::new(None::<Result<Doc, String>>);
    Effect::new(move || {
        if let Some(r) = loaded.get() {
            doc.set(Some(r));
        }
    });
    let tab = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let renaming = RwSignal::new(false);
    let title = RwSignal::new(String::new());
    let history = RwSignal::new(false);
    let exporting = RwSignal::new(false);
    let edit = RwSignal::new(None::<Editing>);
    let deck_pick = RwSignal::new(None::<String>);
    // The block (or slide) a new comment is about ("": the tab).
    let commenting = RwSignal::new(None::<String>);
    let draft = Draft { text: RwSignal::new(String::new()), typed: RwSignal::new(0) };
    let adding = RwSignal::new(String::new());
    let reload = move || tick.update(|t| *t += 1);
    let reload_cb = Callback::new(move |()| reload());
    let send = move |ops: Vec<DocOp>| {
        spawn_local(async move {
            match api::doc_ops(id.get_value(), &DocBatch { ops, answering: String::new() }).await {
                Ok(_) => reload(),
                Err(e) => {
                    error.set(Some(conflict_text(&e)));
                    reload();
                }
            }
        });
    };
    let send = StoredValue::new(send);
    // Chips and embeds: ref chips' things as they are now, the chip being
    // changed, and a way to send its change.
    let chip_ctx = ChipCtx {
        doc: id,
        refs: RwSignal::new(Default::default()),
        editing: RwSignal::new(None),
        send: StoredValue::new(Box::new(move |ops: Vec<DocOp>| send.with_value(|f| f(ops)))),
    };
    provide_context(chip_ctx);
    Effect::new(move || {
        let Some(Ok(d)) = doc.get() else { return };
        let refs = ref_chips(&d);
        if refs.is_empty() {
            return;
        }
        spawn_local(async move {
            if let Ok(live) = api::doc_refs(refs).await {
                chip_ctx.refs.try_set(live.into_iter().map(|l| (l.reference.clone(), l)).collect());
            }
        });
    });
    let attaching = RwSignal::new(false);
    let attach_file = move |ev: leptos::ev::Event| {
        use wasm_bindgen::JsCast;
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
        let (name, mime) = (file.name(), file.type_());
        attaching.set(true);
        spawn_local(async move {
            let result = async {
                let buf =
                    wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await.map_err(|_| "could not read the file".to_owned())?;
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                let f = api::doc_add_file(id.get_value(), &c6_api::docs::DocFileIn { name, content: base64_of(&bytes), mime }).await?;
                let kind = if f.mime.starts_with("image/") && f.mime != "image/svg+xml" { "image" } else { "file" };
                let mut blocks = Map::new();
                blocks
                    .insert("f".into(), serde_json::json!({"type": kind, "blob": f.blob, "name": f.name, "mime": f.mime, "size": f.size}));
                let t = tab.get_untracked();
                let op = DocOp::Insert {
                    markdown: "<?c6 block f?>".into(),
                    blocks,
                    before: None,
                    after: None,
                    section: None,
                    slide: None,
                    region: None,
                    tab: (!t.is_empty()).then_some(t),
                };
                api::doc_ops(id.get_value(), &DocBatch { ops: vec![op], answering: String::new() }).await.map(drop)
            }
            .await;
            attaching.set(false);
            match result {
                Ok(()) => reload(),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let used_in = LocalResource::new(move || {
        tick.track();
        api::work_of(format!("doc:{}", id.get_value()))
    });
    let in_projects = LocalResource::new(move || {
        tick.track();
        async move {
            let me = id.get_value();
            let all = api::projects_list(false).await.unwrap_or_default();
            all.into_iter()
                .filter_map(|p| p.docs.iter().find(|d| d.doc == me).map(|d| (p.id.clone(), p.fields.title.clone(), d.role.clone())))
                .collect::<Vec<_>>()
        }
    });
    let rename = move || {
        let t = title.get_untracked();
        renaming.set(false);
        if t.trim().is_empty() {
            return;
        }
        spawn_local(async move {
            match api::doc_patch(id.get_value(), &DocPatch { title: Some(t), labels: None }).await {
                Ok(d) => doc.set(Some(Ok(d))),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let add_block = move || {
        let md = adding.get_untracked();
        if md.trim().is_empty() {
            return;
        }
        adding.set(String::new());
        let tab_id = tab.get_untracked();
        let op = DocOp::Insert {
            markdown: md,
            blocks: Map::new(),
            before: None,
            after: None,
            section: None,
            slide: None,
            region: None,
            tab: Some(tab_id).filter(|t| !t.is_empty()),
        };
        send.with_value(|f| f(vec![op]));
    };
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-5">
            {move || error.get().map(|e| view! {
                <div class="flex items-start gap-2">
                    <div class="flex-1"><ErrorBox message=e /></div>
                    <button class="text-xs text-zinc-500" on:click=move |_| error.set(None)>"✕"</button>
                </div>
            })}
            {move || match doc.get() {
                None => view! { <p class="text-sm text-zinc-500">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <ErrorBox message=e /> }.into_any(),
                Some(Ok(d)) => {
                    let s = d.summary.clone();
                    let current = if d.tabs.iter().any(|t| t.id == tab.get_untracked()) { tab.get_untracked() } else { d.tabs.first().map(|t| t.id.clone()).unwrap_or_default() };
                    let blocks = d.tabs.iter().find(|t| t.id == current).map(|t| t.blocks.clone()).unwrap_or_default();
                    let rev = s.rev;
                    let kind = s.kind.clone();
                    let tabs = d.tabs.clone();
                    let t2 = s.title.clone();
                    view! {
                        <header class="space-y-1">
                            <div class="flex items-baseline gap-2">
                                <span class="text-zinc-400" title=s.kind.clone()>{kind_mark(&s.kind)}</span>
                                {move || if renaming.get() {
                                    view! {
                                        <input class=format!("{INPUT} text-lg font-semibold") prop:value=move || title.get()
                                            on:input=move |ev| title.set(event_target_value(&ev))
                                            on:keydown=move |ev| if ev.key() == "Enter" { rename() } else if ev.key() == "Escape" { renaming.set(false) }
                                            on:blur=move |_| rename() />
                                    }.into_any()
                                } else {
                                    let t = t2.clone();
                                    view! {
                                        <h1 class="min-w-0 flex-1 cursor-text text-xl font-semibold" title="Click to rename"
                                            on:click=move |_| { title.set(t.clone()); renaming.set(true); }>{t2.clone()}</h1>
                                    }.into_any()
                                }}
                                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| exporting.update(|h| *h = !*h)>"Export"</button>
                                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| history.update(|h| *h = !*h)>
                                    {move || if history.get() { "Hide history" } else { "History" }}
                                </button>
                            </div>
                            <p class="text-xs text-zinc-500">
                                {format!("Version {} · changed {} by {}", s.rev, since(s.updated_at), author_named(&s.updated_by))}
                                {(!s.labels.is_empty()).then(|| format!(" · {}", s.labels.iter().map(|l| format!("#{l}")).collect::<Vec<_>>().join(" ")))}
                            </p>
                            {(s.pending > 0).then(|| view! {
                                <p class="text-sm text-violet-700 dark:text-violet-300">
                                    {format!("✎ Being written by {}: {} {} to go", s.writers.iter().map(|w| author_named(w)).collect::<Vec<_>>().join(", "), s.pending, if s.pending == 1 { "part" } else { "parts" })}
                                </p>
                            })}
                        </header>
                        {(tabs.len() > 1).then(|| view! {
                            <nav class="flex flex-wrap gap-1 border-b border-zinc-200 dark:border-zinc-800">
                                {tabs.iter().map(|t| {
                                    let tid = t.id.clone();
                                    let on = t.id == current;
                                    view! {
                                        <button class=format!("-mb-px border-b-2 px-2 py-1 text-sm {}", if on { "border-zinc-900 font-medium dark:border-zinc-100" } else { "border-transparent text-zinc-500" })
                                            on:click=move |_| tab.set(tid.clone())>{t.title.clone()}</button>
                                    }
                                }).collect_view()}
                            </nav>
                        })}
                        {let k = kind.clone(); move || exporting.get().then(|| view! { <Exports id=id.get_value() kind=k.clone() /> })}
                        {move || history.get().then(|| view! { <History id=id.get_value() reload=reload_cb /> })}
                        {(kind == "deck").then(|| view! {
                            <crate::deck::DeckView doc=d.clone() send=Callback::new(move |ops: Vec<DocOp>| send.with_value(|f| f(ops))) error picked=deck_pick commenting />
                        })}
                        <article class="space-y-1" class:hidden=kind == "deck">
                            {blocks.iter().map(|b| view! {
                                <BlockView doc_id=id b=b.clone() rev=rev kind=kind.clone() edit draft error reload=reload_cb commenting />
                            }).collect_view()}
                            {blocks.is_empty().then(|| view! { <p class="text-sm text-zinc-500">"Empty. Write below to start it."</p> })}
                        </article>
                        <div class="space-y-1" class:hidden=s.kind == "deck">
                            <textarea class=format!("{INPUT} min-h-[4rem] font-mono text-sm")
                                placeholder=if s.kind == "deck" { "Add slides in Markdown (--- between slides)…" } else { "Add to the end in Markdown…" }
                                prop:value=move || adding.get() on:input=move |ev| adding.set(event_target_value(&ev))
                                on:keydown=move |ev| if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { add_block() } />
                            <div class="flex items-center justify-end gap-2">
                                <label class=format!("{BUTTON_QUIET} cursor-pointer text-xs") title="A picture shows in the doc; any other file is a link to download">
                                    {move || if attaching.get() { "Adding…" } else { "+ Picture or file" }}
                                    <input type="file" class="hidden" on:change=attach_file />
                                </label>
                                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| add_block()>"Add (⌘↵)"</button>
                            </div>
                        </div>
                        <Comments id=id.get_value() tick commenting />
                        {move || {
                            let w = used_in.get().and_then(Result::ok).unwrap_or_default();
                            let p = in_projects.get().unwrap_or_default();
                            (!w.is_empty() || !p.is_empty()).then_some((w, p))
                        }.map(|(w, p)| view! {
                            <section class="space-y-1 border-t border-zinc-200 pt-3 dark:border-zinc-800">
                                <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">"Refers to it"</h2>
                                {p.into_iter().map(|(pid, title, role)| view! {
                                    <Go to=View::Projects { id: pid } class="block text-sm text-sky-700 hover:underline dark:text-sky-400">
                                        {format!("Project · {title}{}", if role.is_empty() { String::new() } else { format!(" ({role})") })}
                                    </Go>
                                }).collect_view()}
                                {w.into_iter().map(|w| view! {
                                    <Go to=View::Work { id: w.id.clone() } class="block text-sm text-sky-700 hover:underline dark:text-sky-400">{format!("Work · {}", w.fields.title)}</Go>
                                }).collect_view()}
                            </section>
                        })}
                    }.into_any()
                }
            }}
        </main>
    }
}

/// A refusal as you read it: someone else's edit won.
fn conflict_text(e: &str) -> String {
    if e.contains("guard_mismatch") {
        format!("Someone changed that block while you were editing it; their edit wins and is shown now. ({e})")
    } else {
        e.to_owned()
    }
}

/// The block being edited: its draft, and the version and hash it was
/// read at (the guard its change is sent with). It lives with the
/// document, so a live update while you type keeps your draft.
#[derive(Clone)]
struct Editing {
    id: String,
    rev: i64,
    hash: String,
    /// What it said when you started (or last saved).
    original: String,
    chips: Map<String, Value>,
}

/// The draft of the block being edited, and a count of keystrokes (to
/// save once you pause). Made by the document, so they outlive its blocks.
#[derive(Clone, Copy)]
struct Draft {
    text: RwSignal<String>,
    typed: RwSignal<u32>,
}

/// Sends a block's draft, guarded by what it was read at. `keep`: you go on
/// editing (a pause), with the guard moved to what you saved. Emptied, the
/// block goes.
fn save(
    doc_id: String,
    edit: RwSignal<Option<Editing>>,
    draft: Draft,
    keep: bool,
    error: RwSignal<Option<String>>,
    reload: impl Fn() + 'static,
) {
    let Some(e) = edit.get_untracked() else { return };
    if !keep {
        edit.set(None);
    }
    let md = draft.text.get_untracked();
    if md == e.original {
        return;
    }
    if keep && md.trim().is_empty() {
        return;
    }
    let op = if md.trim().is_empty() {
        DocOp::Delete { id: e.id.clone(), if_rev: Some(e.rev), if_hash: Some(e.hash.clone()) }
    } else {
        DocOp::Replace {
            id: e.id.clone(),
            markdown: Some(md.clone()),
            blocks: e.chips.clone(),
            find: None,
            with_text: None,
            nth: None,
            cell: None,
            text: None,
            layout: None,
            notes: None,
            hidden: None,
            if_rev: Some(e.rev),
            if_hash: Some(e.hash.clone()),
        }
    };
    spawn_local(async move {
        match api::doc_ops(doc_id, &DocBatch { ops: vec![op], answering: String::new() }).await {
            Ok(a) => {
                if keep {
                    edit.update(|x| {
                        if let Some(x) = x.as_mut().filter(|x| x.id == e.id) {
                            x.rev = a.rev;
                            x.hash = a.hashes.get(&e.id).and_then(Value::as_str).unwrap_or(&x.hash).to_owned();
                            x.original = md;
                        }
                    });
                }
                reload();
            }
            Err(err) => {
                // Their edit wins: yours stays in the message to copy.
                if edit.get_untracked().is_some_and(|x| x.id == e.id) {
                    edit.set(None);
                }
                error.set(Some(format!("{}\n\nWhat you wrote:\n{md}", conflict_text(&err))));
                reload();
            }
        }
    });
}

/// One block: shown, or (clicked) edited as Markdown.
#[component]
fn BlockView(
    doc_id: StoredValue<String>,
    b: DocBlock,
    rev: i64,
    kind: String,
    edit: RwSignal<Option<Editing>>,
    draft: Draft,
    error: RwSignal<Option<String>>,
    reload: Callback<()>,
    commenting: RwSignal<Option<String>>,
) -> impl IntoView {
    let bid = StoredValue::new(b.id.clone());
    let block = StoredValue::new(b.clone());
    let hover = format!("{} · last written by {} (version {})", b.kind, author_named(&b.by), b.rev);
    let chip_ctx = use_context::<ChipCtx>();
    let (col, ws) = (use_column(), use_workspace());
    let is_editing = move || edit.with(|e| e.as_ref().is_some_and(|e| e.id == bid.get_value()));
    let begin = move |text: String| {
        let b = block.get_value();
        edit.set(Some(Editing {
            id: b.id.clone(),
            rev,
            hash: b.hash.clone(),
            original: text.clone(),
            chips: b.attrs.get("chips").and_then(Value::as_object).cloned().unwrap_or_default(),
        }));
        draft.text.set(text);
    };
    let start = move || {
        let b = block.get_value();
        if b.kind == "pending" {
            return;
        }
        // Another block being edited is saved first.
        if edit.get_untracked().is_some() {
            save(doc_id.get_value(), edit, draft, false, error, move || reload.run(()));
        }
        if b.kind == "slide" || b.kind == "embed" {
            // Its Markdown comes from the head (a slide's header and regions).
            spawn_local(async move {
                if let Ok(v) = api::doc_read(&doc_id.get_value(), &format!("view=block&id={}", api::enc(&b.id))).await {
                    begin(v["markdown"].as_str().unwrap_or("").to_owned());
                }
            });
            return;
        }
        begin(b.text.clone());
    };
    let finish = move |keep: bool| {
        if is_editing() {
            save(doc_id.get_value(), edit, draft, keep, error, move || reload.run(()));
        }
    };
    let typed = move || {
        draft.typed.update(|n| *n += 1);
        let n = draft.typed.get_untracked();
        // Saved once you pause. Only values the document owns are used:
        // a live update may have made this block anew by then.
        let (id, doc) = (bid.get_value(), doc_id.get_value());
        set_timeout(
            move || {
                if draft.typed.try_get_untracked() == Some(n)
                    && edit.try_with_untracked(|e| e.as_ref().is_some_and(|e| e.id == id)) == Some(true)
                {
                    save(doc, edit, draft, true, error, move || reload.run(()));
                }
            },
            std::time::Duration::from_millis(1500),
        );
    };
    view! {
        <div class="group relative rounded px-2 py-0.5 hover:bg-zinc-50 dark:hover:bg-zinc-900/60" title=hover data-block=b.id.clone()>
            {move || match edit.get().filter(|e| e.id == bid.get_value()) {
                Some(_) => {
                    view! {
                        <textarea class=format!("{INPUT} min-h-[3rem] font-mono text-sm") autofocus
                            prop:value=move || draft.text.get() on:input=move |ev| { draft.text.set(event_target_value(&ev)); typed(); }
                            on:blur=move |_| finish(false)
                            on:keydown=move |ev| if ev.key() == "Escape" { edit.set(None) } else if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { finish(false) } />
                    }.into_any()
                }
                None => {
                    let b = block.get_value();
                    view! {
                        <div class="cursor-text" on:click=move |ev| {
                            if let Some(c) = chip_ctx && chip_clicked(&ev, &block.get_value(), c, col, ws) { return; }
                            start()
                        }><Shown b=b kind=kind.clone() /></div>
                    }.into_any()
                }
            }}
            {move || chip_ctx.and_then(|c| c.editing.get().filter(|(b, _, _)| *b == bid.get_value() || block.get_value().children.iter().any(|x| x.id == *b)).map(|(b, k, v)| view! {
                <ChipEditor block=b key=k value=v ctx=c />
            }))}
            <button class="invisible absolute -right-6 top-0.5 text-xs text-zinc-400 hover:text-violet-600 group-hover:visible" title="Comment on this"
                on:click=move |_| commenting.set(Some(bid.get_value()))>"💬"</button>
        </div>
    }
}

/// A block as it reads.
#[component]
fn Shown(b: DocBlock, kind: String) -> impl IntoView {
    match b.kind.as_str() {
        "pending" => {
            let who = b.attrs["by"].as_str().map(author_named).unwrap_or_else(|| "an agent".into());
            view! {
                <div class="rounded border border-dashed border-violet-300 px-3 py-2 text-sm text-violet-800 dark:border-violet-800 dark:text-violet-300">
                    <span class="animate-pulse">"✎ "</span>{b.attrs["intent"].as_str().unwrap_or("").to_owned()}
                    <span class="text-xs text-violet-500">{format!(" — {who} is writing this")}</span>
                </div>
            }
            .into_any()
        }
        "slide" => {
            let layout = b.attrs["layout"].as_str().unwrap_or("body").to_owned();
            let notes = b.attrs["notes"].as_str().unwrap_or("").to_owned();
            let hidden = b.attrs["hidden"] == true;
            view! {
                <div class=format!("my-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800 {}", if hidden { "opacity-50" } else { "" })>
                    <div class="mb-1 text-[11px] uppercase tracking-wide text-zinc-400">{format!("slide · {layout}{}", if hidden { " · hidden" } else { "" })}</div>
                    {b.children.iter().map(|c| {
                        let refs = use_context::<ChipCtx>().map(|c| c.refs);
                        let c = c.clone();
                        if c.kind == "embed" {
                            view! { <Embed v=c.attrs.clone() /> }.into_any()
                        } else {
                            view! { <div class="md" inner_html=move || chips_html(&c.text, &c.attrs, &refs.map(|r| r.get()).unwrap_or_default())></div> }.into_any()
                        }
                    }).collect_view()}
                    {(!notes.is_empty()).then(|| view! { <p class="mt-2 border-t border-zinc-100 pt-1 text-xs text-zinc-500 dark:border-zinc-900">{format!("Notes: {notes}")}</p> })}
                </div>
            }
            .into_any()
        }
        "embed" => view! { <Embed v=b.attrs.clone() /> }.into_any(),
        _ => {
            let _ = kind;
            let refs = use_context::<ChipCtx>().map(|c| c.refs);
            view! { <div class="md" inner_html=move || chips_html(&b.text, &b.attrs, &refs.map(|r| r.get()).unwrap_or_default())></div> }
                .into_any()
        }
    }
}

/// Comments (docs/docs.md §8): its threads, open first, each with what
/// it's about, its comments, a reply box, "send to an agent" and Resolve; a
/// new thread on the block or slide you picked (💬), or on the whole tab.
#[component]
fn Comments(id: String, tick: RwSignal<u32>, commenting: RwSignal<Option<String>>) -> impl IntoView {
    let id = StoredValue::new(id);
    let threads = LocalResource::new(move || {
        tick.track();
        api::doc_threads(id.get_value())
    });
    let shown = RwSignal::new(Vec::<c6_api::docs::DocThread>::new());
    Effect::new(move || {
        if let Some(Ok(t)) = threads.get() {
            shown.set(t);
        }
    });
    let error = RwSignal::new(None::<String>);
    let (quote, text, to_agent) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(false));
    let show_resolved = RwSignal::new(false);
    let post = move || {
        let Some(block) = commenting.get_untracked() else { return };
        let t = c6_api::docs::ThreadNew {
            anchor: c6_api::docs::Anchor { block, quote: quote.get_untracked(), tab: String::new() },
            text: text.get_untracked(),
            to: if to_agent.get_untracked() { "agent".into() } else { String::new() },
        };
        spawn_local(async move {
            match api::doc_thread_new(id.get_value(), &t).await {
                Ok(_) => {
                    commenting.set(None);
                    quote.set(String::new());
                    text.set(String::new());
                    error.set(None);
                    tick.update(|t| *t += 1);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-2 border-t border-zinc-200 pt-3 dark:border-zinc-800">
            <div class="flex items-center gap-2">
                <h2 class="flex-1 text-xs font-medium uppercase tracking-wide text-zinc-500">"Comments"</h2>
                <button class="text-xs text-zinc-500 hover:underline" on:click=move |_| commenting.set(Some(String::new()))>"+ On the whole tab"</button>
                <label class="flex items-center gap-1 text-xs text-zinc-500">
                    <input type="checkbox" prop:checked=move || show_resolved.get() on:change=move |ev| show_resolved.set(event_target_checked(&ev)) />
                    "Resolved"
                </label>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || commenting.get().map(|b| view! {
                <div class="space-y-1 rounded-md border border-violet-300 p-2 dark:border-violet-800">
                    <div class="text-xs text-zinc-500">{if b.is_empty() { "On the whole tab".to_owned() } else { format!("On block {b}") }}</div>
                    {(!b.is_empty()).then(|| view! {
                        <input class=INPUT placeholder="A few of its words to point at (optional)" prop:value=move || quote.get() on:input=move |ev| quote.set(event_target_value(&ev)) />
                    })}
                    <textarea class=format!("{INPUT} min-h-[3rem]") placeholder="Your comment" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev)) />
                    <div class="flex items-center gap-2">
                        <label class="flex flex-1 items-center gap-1 text-xs text-zinc-600 dark:text-zinc-400"
                            title="The session that last wrote it gets the thread if it's idle; else a new one starts from the document's work or project">
                            <input type="checkbox" prop:checked=move || to_agent.get() on:change=move |ev| to_agent.set(event_target_checked(&ev)) />
                            "Send it to an agent"
                        </label>
                        <button class=BUTTON_QUIET on:click=move |_| commenting.set(None)>"Cancel"</button>
                        <button class=BUTTON on:click=move |_| post()>"Comment"</button>
                    </div>
                </div>
            })}
            {move || shown.get().into_iter().filter(|t| show_resolved.get() || t.state == "open").map(|t| view! { <Thread t doc=id.get_value() tick /> }).collect_view()}
            {move || shown.get().iter().all(|t| t.state != "open").then(|| view! { <p class="text-sm text-zinc-500">"No open comments. Hover a block and press 💬 to start one."</p> })}
        </section>
    }
}

/// One thread.
#[component]
fn Thread(t: c6_api::docs::DocThread, doc: String, tick: RwSignal<u32>) -> impl IntoView {
    let (doc, tid) = (StoredValue::new(doc), StoredValue::new(t.id.clone()));
    let reply = RwSignal::new(String::new());
    let to_agent = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let done = move |r: Result<c6_api::docs::DocThread, String>| match r {
        Ok(_) => {
            reply.set(String::new());
            tick.update(|t| *t += 1);
        }
        Err(e) => error.set(Some(e)),
    };
    let send = move || {
        let c = c6_api::docs::CommentNew {
            text: reply.get_untracked(),
            to: if to_agent.get_untracked() { "agent".into() } else { String::new() },
        };
        if c.text.trim().is_empty() {
            return;
        }
        spawn_local(async move { done(api::doc_thread_reply(doc.get_value(), tid.get_value(), &c).await) });
    };
    let open = t.state == "open";
    view! {
        <div class=format!("space-y-1 rounded-md border p-2 text-sm {}", if t.needs_you { "border-amber-300 bg-amber-50/50 dark:border-amber-800 dark:bg-amber-950/20" } else { "border-zinc-200 dark:border-zinc-800" })
            data-thread=t.id.clone()>
            <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
                <span class="min-w-0 flex-1 truncate">{format!("On {}", t.about)}</span>
                {t.detached.then(|| view! { <span class="rounded bg-zinc-100 px-1 dark:bg-zinc-800" title="What it was about is gone or changed">"detached"</span> })}
                {t.to_agent.then(|| view! { <span title=format!("Sent to {}", author_named(&format!("session:{}", t.agent)))>"→ agent"</span> })}
                {t.needs_you.then(|| view! { <span class="font-medium text-amber-700 dark:text-amber-300">"answered: your move"</span> })}
                {(!open).then(|| view! { <span>{format!("resolved by {}", author(&t.resolved_by))}</span> })}
            </div>
            {t.comments.iter().map(|c| view! {
                <div><span class="font-medium">{author_named(&c.by)}</span>{format!(" · {}", since(c.at))}<p class="whitespace-pre-wrap">{c.text.clone()}</p></div>
            }).collect_view()}
            {(!t.edits.is_empty()).then(|| view! { <div class="text-xs text-zinc-500">{format!("Edits for it: {}", t.edits.iter().map(|r| format!("v{r}")).collect::<Vec<_>>().join(", "))}</div> })}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex items-center gap-2">
                <input class=format!("{INPUT} min-w-0 flex-1") placeholder="Reply…" prop:value=move || reply.get() on:input=move |ev| reply.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { send() } />
                <label class="flex items-center gap-1 text-xs text-zinc-500" title="Send the thread to an agent">
                    <input type="checkbox" prop:checked=move || to_agent.get() on:change=move |ev| to_agent.set(event_target_checked(&ev)) />"agent"
                </label>
                {open.then(|| view! {
                    <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| spawn_local(async move { done(api::doc_thread_resolve(doc.get_value(), tid.get_value()).await) })>"Resolve"</button>
                })}
            </div>
        </div>
    }
}

/// Export: the formats a document goes to, and the files made before
/// (each with the version it was made from), to download again.
#[component]
fn Exports(id: String, kind: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    let past = LocalResource::new(move || {
        tick.track();
        api::doc_exports(id.get_value())
    });
    let busy = RwSignal::new(None::<&'static str>);
    let error = RwSignal::new(None::<String>);
    let formats: &'static [(&'static str, &'static str)] = if kind == "deck" {
        &[("pdf", "PDF"), ("pptx", "PowerPoint"), ("odp", "OpenDocument"), ("png", "Pictures"), ("html", "HTML"), ("md", "Markdown")]
    } else {
        &[("pdf", "PDF"), ("docx", "Word"), ("odt", "OpenDocument"), ("html", "HTML"), ("md", "Markdown")]
    };
    let go = move |f: &'static str| {
        busy.set(Some(f));
        error.set(None);
        spawn_local(async move {
            match api::doc_export(id.get_value(), f.into()).await {
                Ok(e) => {
                    let _ = window().location().set_href(&api::url(&e.url));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(None);
            tick.update(|t| *t += 1);
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-800">
            <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">"Export"</h2>
            <div class="flex flex-wrap gap-2">
                {formats.iter().map(|(f, label)| {
                    let f = *f;
                    view! {
                        <button class=BUTTON_QUIET disabled=move || busy.get().is_some() on:click=move |_| go(f)
                            title=if matches!(f, "md" | "html") { "Made by carbon6" } else { "Made by the render kit on a worker with Docker or Podman" }>
                            {move || if busy.get() == Some(f) { format!("{label}…") } else { (*label).to_owned() }}
                        </button>
                    }
                }).collect_view()}
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <ToGoogle id=id.get_value() />
            {move || past.get().and_then(Result::ok).filter(|p| !p.is_empty()).map(|p| view! {
                <div class="space-y-0.5">
                    <div class="text-xs text-zinc-500">"Made before"</div>
                    {p.into_iter().map(|e| view! {
                        <div class="flex items-center gap-2">
                            <a href=api::url(&e.url) class="min-w-0 flex-1 truncate text-sky-700 hover:underline dark:text-sky-400">{e.name.clone()}</a>
                            <span class="shrink-0 text-xs text-zinc-400">{format!("v{} · {} KB · {}", e.rev, e.size.div_ceil(1024), since(e.at))}</span>
                        </div>
                    }).collect_view()}
                </div>
            })}
        </section>
    }
}

/// Google Docs or Slides (docs/docs.md §10): connect an account (only the
/// files carbon6 makes), export, and after that update the Google file or
/// make a new one; a Google file edited in Google since is said first.
#[component]
fn ToGoogle(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    let google = LocalResource::new(move || {
        tick.track();
        api::doc_google(id.get_value())
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let changed = RwSignal::new(false);
    let go = move |new: bool, anyway: bool| {
        busy.set(true);
        error.set(None);
        changed.set(false);
        spawn_local(async move {
            if let Err(e) = api::doc_to_google(id.get_value(), c6_api::docs::GoogleExportIn { new, anyway, account: None }).await {
                changed.set(e.contains("changed in Google"));
                error.set(Some(e));
            }
            busy.set(false);
            tick.update(|t| *t += 1);
        });
    };
    let connect = move |_| {
        spawn_local(async move {
            let back = window().location().pathname().unwrap_or_default();
            match api::docs_google_connect(back, false).await {
                Ok(u) => {
                    let _ = window().location().set_href(&u);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        {move || google.get().and_then(Result::ok).map(|g| {
            let app = g.app.clone();
            view! {
                <div class="space-y-1 border-t border-zinc-200 pt-2 dark:border-zinc-800">
                    <div class="text-xs text-zinc-500">{app.clone()}</div>
                    {if g.accounts.is_empty() {
                        view! {
                            <div class="flex items-center gap-2">
                                <button class=BUTTON_QUIET on:click=connect>"Connect Google"</button>
                                <span class="text-xs text-zinc-500">"Only the files carbon6 makes in your Drive (drive.file)."</span>
                            </div>
                        }.into_any()
                    } else if let Some(f) = g.file.clone() {
                        view! {
                            <div class="space-y-1">
                                <div class="flex items-center gap-2">
                                    <a href=f.url.clone() target="_blank" rel="noopener" class="min-w-0 flex-1 truncate text-sky-700 hover:underline dark:text-sky-400">
                                        {format!("Open in {app}")}
                                    </a>
                                    <span class="shrink-0 text-xs text-zinc-400">{format!("v{} · {} · {}", f.rev, f.account, since(f.at))}</span>
                                </div>
                                <div class="flex flex-wrap gap-2">
                                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(false, false)>
                                        {move || if busy.get() { "Exporting…" } else { "Update the Google file" }}
                                    </button>
                                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(true, false)>"Make a new one"</button>
                                    {move || changed.get().then(|| view! {
                                        <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(false, true)
                                            title="Replaces the edits made in Google since the last export">"Update anyway"</button>
                                    })}
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(false, false)
                                title=format!("Made by the render kit, then uploaded to {} as {app}", g.accounts.join(", "))>
                                {move || if busy.get() { "Exporting…".to_owned() } else { format!("To {}", g.app) }}
                            </button>
                        }.into_any()
                    }}
                    {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                </div>
            }
        })}
    }
}

/// History: changes by author run, each version openable and restorable.
#[component]
fn History(id: String, reload: Callback<()>) -> impl IntoView {
    let id = StoredValue::new(id);
    let versions = LocalResource::new(move || api::doc_history(id.get_value()));
    let open = RwSignal::new(None::<i64>);
    let error = RwSignal::new(None::<String>);
    let restore = move |rev: i64, block: Option<String>| {
        spawn_local(async move {
            match api::doc_restore(id.get_value(), &DocRestore { rev, block }).await {
                Ok(_) => {
                    open.set(None);
                    reload.run(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-800">
            <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">"History"</h2>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || versions.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) => view! { <div class="space-y-2">{runs(&v).into_iter().map(|run| {
                    let first = run.last().map(|x| x.at).unwrap_or(0);
                    let last = run.first().map(|x| x.at).unwrap_or(0);
                    let by = run[0].by.clone();
                    view! {
                        <div>
                            <div class="text-xs text-zinc-500">{format!("{}, {} {}, {}{}", author_named(&by), run.len(), if run.len() == 1 { "edit" } else { "edits" },
                                when_hm(first), if last != first { format!("–{}", when_hm(last)) } else { String::new() })}</div>
                            {run.iter().map(|v| {
                                let rev = v.rev;
                                view! {
                                    <div class="flex items-center gap-2 pl-2">
                                        <span class="w-10 shrink-0 text-xs text-zinc-400">{format!("v{rev}")}</span>
                                        <span class="min-w-0 flex-1 truncate">{v.summary.clone()}</span>
                                        <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| open.set(if open.get_untracked() == Some(rev) { None } else { Some(rev) })>"Open"</button>
                                        <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" title="Bring this version back as a new change"
                                            on:click=move |_| restore(rev, None)>"Restore"</button>
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                    }
                }).collect_view()}</div> }.into_any(),
            })}
            {move || open.get().map(|rev| view! { <Version id=id.get_value() rev restore=Callback::new(move |(r, b): (i64, Option<String>)| restore(r, b)) /> })}
        </section>
    }
}

/// Versions grouped into runs by the same author.
fn runs(v: &[DocVersion]) -> Vec<Vec<DocVersion>> {
    let mut out: Vec<Vec<DocVersion>> = Vec::new();
    for x in v {
        match out.last_mut() {
            Some(run) if run[0].by == x.by && run.last().is_some_and(|l| l.at - x.at < 30 * 60 * 1000) => run.push(x.clone()),
            _ => out.push(vec![x.clone()]),
        }
    }
    out
}

fn when_hm(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

/// An earlier version, read only, with a Restore per block.
#[component]
fn Version(id: String, rev: i64, restore: Callback<(i64, Option<String>)>) -> impl IntoView {
    let id = StoredValue::new(id);
    let old = LocalResource::new(move || async move { api::doc_read(&id.get_value(), &format!("view=full&at={rev}")).await });
    view! {
        <div class="space-y-1 rounded border border-amber-200 bg-amber-50/50 p-2 dark:border-amber-900 dark:bg-amber-950/20">
            <div class="text-xs font-medium text-amber-800 dark:text-amber-300">{format!("As it was at version {rev}")}</div>
            {move || old.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) => {
                    let d: Option<Doc> = serde_json::from_value(v).ok();
                    view! { <div>{d.map(|d| d.tabs.into_iter().flat_map(|t| t.blocks).map(|b| {
                        let bid = b.id.clone();
                        view! {
                            <div class="group flex items-start gap-2">
                                <div class="min-w-0 flex-1"><Shown b=b kind=String::new() /></div>
                                <button class="invisible shrink-0 text-xs text-sky-700 group-hover:visible dark:text-sky-400" title="Bring back just this block"
                                    on:click=move |_| restore.run((rev, Some(bid.clone())))>"Restore block"</button>
                            </div>
                        }
                    }).collect_view())}</div> }.into_any()
                }
            })}
        </div>
    }
}

/// Follows a link to a document (or another carbon6 view) inside Markdown
/// that carbon6 rendered: it opens beside, as any link in a panel does.
pub fn follow_link(ev: &web_sys::MouseEvent, col: Option<crate::workspace::ColumnCtx>, ws: crate::workspace::Workspace) {
    use wasm_bindgen::JsCast;
    if ev.button() != 0 || ev.shift_key() || ev.alt_key() {
        return;
    }
    let Some(a) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()).and_then(|e| e.closest("a[href]").ok().flatten()) else {
        return;
    };
    let href = a.get_attribute("href").unwrap_or_default();
    let path = crate::place::strip(&href);
    if !path.starts_with("/docs/") {
        return;
    }
    ev.prevent_default();
    let to = View::from_path(&path);
    let how = How::from_mouse(ev);
    match col {
        Some(c) => c.open(to, how),
        None => ws.open(None, to, how),
    }
}

/// Bytes as base64 (for uploads).
fn base64_of(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_as_the_head_reads_it() {
        assert_eq!(super::base64_of(b"Man"), "TWFu");
        assert_eq!(super::base64_of(b"Ma"), "TWE=");
        assert_eq!(super::base64_of(b"M"), "TQ==");
        assert_eq!(super::base64_of(b""), "");
    }
}
