//! Remote file browsing and git changes on a worker.

use leptos::prelude::*;
use serde_json::Value;

use crate::api;
use crate::util::{ErrorBox, INPUT, ago, now_ms};

/// Directory listing plus file viewer, starting at `root`.
#[component]
pub fn FileBrowser(worker_id: String, #[prop(optional)] root: Option<String>) -> impl IntoView {
    let wid = StoredValue::new(worker_id);
    let dir = RwSignal::new(root.unwrap_or_default());
    let hidden = RwSignal::new(false);
    let open_file = RwSignal::new(None::<String>);
    let listing = LocalResource::new(move || api::fs_list(wid.get_value(), dir.get(), hidden.get()));
    let path_input = RwSignal::new(String::new());
    Effect::new(move || {
        if let Some(Ok(l)) = listing.get()
            && let Some(p) = l["path"].as_str()
        {
            path_input.set(p.to_owned());
        }
    });

    view! {
        <div class="grid gap-3 lg:grid-cols-[minmax(16rem,1fr)_2fr]">
            <div class="space-y-2">
                <div class="flex gap-2">
                    <button class="rounded-md border border-zinc-300 px-2 text-sm dark:border-zinc-700" title="Up"
                        on:click=move |_| {
                            let cur = path_input.get_untracked();
                            let parent = cur.rsplit_once('/').map(|(p, _)| if p.is_empty() { "/".to_owned() } else { p.to_owned() }).unwrap_or(cur);
                            dir.set(parent);
                        }>"↑"</button>
                    <input class=format!("{INPUT} font-mono text-xs") prop:value=move || path_input.get()
                        on:input=move |ev| path_input.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" { dir.set(path_input.get_untracked()) } />
                </div>
                <label class="flex items-center gap-1.5 text-xs text-zinc-500">
                    <input type="checkbox" prop:checked=move || hidden.get() on:change=move |ev| hidden.set(event_target_checked(&ev)) />
                    "Show hidden files"
                </label>
                <div class="max-h-[70vh] overflow-auto rounded-md border border-zinc-200 dark:border-zinc-800">
                    <Suspense fallback=|| view! { <p class="p-3 text-sm text-zinc-500">"Loading…"</p> }>
                        {move || listing.get().map(|r| match r {
                            Err(e) => view! { <div class="p-2"><ErrorBox message=e /></div> }.into_any(),
                            Ok(l) => {
                                let base = l["path"].as_str().unwrap_or("").to_owned();
                                let now = now_ms();
                                let entries = l["entries"].as_array().cloned().unwrap_or_default();
                                view! {
                                    <ul class="divide-y divide-zinc-100 text-sm dark:divide-zinc-900">
                                        {entries.into_iter().map(|e| {
                                            let name = e["name"].as_str().unwrap_or("").to_owned();
                                            let is_dir = e["kind"] == "dir";
                                            let full = if base.ends_with('/') { format!("{base}{name}") } else { format!("{base}/{name}") };
                                            let full2 = full.clone();
                                            let mtime = e["mtime"].as_i64();
                                            view! {
                                                <li class=move || if open_file.get().as_deref() == Some(full2.as_str()) { "bg-zinc-200 dark:bg-zinc-800" } else { "" }>
                                                    <button class="flex w-full items-center gap-2 px-2 py-1 text-left hover:bg-zinc-100 dark:hover:bg-zinc-900"
                                                        on:click=move |_| if is_dir { dir.set(full.clone()) } else { open_file.set(Some(full.clone())) }>
                                                        <span class="w-4 text-zinc-400">{if is_dir { "▸" } else { "" }}</span>
                                                        <span class=if is_dir { "flex-1 truncate font-medium" } else { "flex-1 truncate" }>{name}</span>
                                                        <span class="shrink-0 text-[11px] text-zinc-400">{mtime.map(|t| ago(now - t)).unwrap_or_default()}</span>
                                                    </button>
                                                </li>
                                            }
                                        }).collect_view()}
                                    </ul>
                                }.into_any()
                            }
                        })}
                    </Suspense>
                </div>
            </div>
            <div class="min-w-0">
                {move || match open_file.get() {
                    Some(p) => view! { <FileViewer worker_id=wid.get_value() path=p /> }.into_any(),
                    None => view! { <p class="py-10 text-center text-sm text-zinc-500">"Select a file to view it."</p> }.into_any(),
                }}
            </div>
        </div>
    }
}

#[component]
fn FileViewer(worker_id: String, path: String) -> impl IntoView {
    let file = LocalResource::new({
        let (w, p) = (worker_id.clone(), path.clone());
        move || api::fs_read(w.clone(), p.clone())
    });
    let is_md = path.ends_with(".md") || path.ends_with(".markdown");
    let rendered = RwSignal::new(is_md);
    view! {
        <div class="space-y-2">
            <div class="flex items-center gap-2">
                <span class="min-w-0 flex-1 truncate font-mono text-xs text-zinc-500">{path.clone()}</span>
                {is_md.then(|| view! {
                    <button class="rounded px-2 py-0.5 text-xs hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| rendered.update(|r| *r = !*r)>
                        {move || if rendered.get() { "Source" } else { "Rendered" }}
                    </button>
                })}
            </div>
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Loading…"</p> }>
                {move || file.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(f) if f["binary"] == true => view! { <p class="text-sm text-zinc-500">{format!("Binary file ({} bytes)", f["size"])}</p> }.into_any(),
                    Ok(f) => {
                        let text = f["content"].as_str().unwrap_or("").to_owned();
                        let truncated = f["truncated"] == true;
                        view! {
                            {truncated.then(|| view! { <p class="text-xs text-amber-600">"Showing the first 2 MB."</p> })}
                            {move || if rendered.get() {
                                view! { <div class="md max-h-[70vh] overflow-auto rounded-md border border-zinc-200 p-4 dark:border-zinc-800" inner_html=crate::markdown::to_html(&text)></div> }.into_any()
                            } else {
                                view! { <CodeView text=text.clone() /> }.into_any()
                            }}
                        }.into_any()
                    }
                })}
            </Suspense>
        </div>
    }
}

/// Text with line numbers.
#[component]
fn CodeView(text: String) -> impl IntoView {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let width = lines.len().to_string().len();
    view! {
        <pre class="max-h-[70vh] overflow-auto rounded-md border border-zinc-200 bg-white p-2 font-mono text-xs leading-5 dark:border-zinc-800 dark:bg-zinc-950">
            {lines.into_iter().enumerate().map(|(i, l)| view! {
                <div class="flex"><span class="mr-3 select-none text-right text-zinc-400" style=format!("min-width:{width}ch")>{i + 1}</span><span class="whitespace-pre">{l}</span></div>
            }).collect_view()}
        </pre>
    }
}

/// Git status and the working-tree diff for `path`.
#[component]
pub fn Changes(worker_id: String, path: String) -> impl IntoView {
    let (w1, p1, w2, p2) = (worker_id.clone(), path.clone(), worker_id, path);
    let status = LocalResource::new(move || api::git_status(w1.clone(), p1.clone()));
    let diff = LocalResource::new(move || api::git_diff(w2.clone(), p2.clone()));
    let refresh = move |_| {
        status.refetch();
        diff.refetch();
    };
    view! {
        <div class="space-y-3">
            <Suspense fallback=|| ()>
                {move || status.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(s) => view! { <StatusSummary s /> }.into_any(),
                })}
            </Suspense>
            <div class="flex justify-end">
                <button class="rounded-md px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=refresh>"Refresh"</button>
            </div>
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Loading diff…"</p> }>
                {move || diff.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(d) => view! { <DiffView diff=d["diff"].as_str().unwrap_or("").to_owned() truncated=d["truncated"] == true /> }.into_any(),
                })}
            </Suspense>
        </div>
    }
}

#[component]
fn StatusSummary(s: Value) -> impl IntoView {
    let branch = s["branch"].as_str().unwrap_or("(detached)").to_owned();
    let ahead = s["ahead"].as_i64().unwrap_or(0);
    let behind = s["behind"].as_i64().unwrap_or(0);
    let files = s["files"].as_array().cloned().unwrap_or_default();
    view! {
        <div class="text-sm">
            <div class="flex flex-wrap items-center gap-2">
                <span class="rounded bg-zinc-100 px-1.5 py-0.5 font-mono text-xs dark:bg-zinc-800">{branch}</span>
                {s["upstream"].as_str().map(|u| view! { <span class="text-xs text-zinc-500">{format!("→ {u} · {ahead} ahead, {behind} behind")}</span> })}
                <span class="text-xs text-zinc-500">{format!("{} changed file{}", files.len(), if files.len() == 1 { "" } else { "s" })}</span>
            </div>
            <ul class="mt-2 grid gap-x-4 font-mono text-xs sm:grid-cols-2">
                {files.into_iter().map(|f| {
                    let code = format!("{}{}", f["index"].as_str().unwrap_or(" "), f["worktree"].as_str().unwrap_or(" "));
                    let cls = match code.as_str() { "??" => "text-emerald-600", c if c.contains('D') => "text-red-600", _ => "text-amber-600" };
                    view! { <li class="truncate"><span class=format!("mr-2 {cls}")>{code}</span>{f["path"].as_str().unwrap_or("").to_owned()}</li> }
                }).collect_view()}
            </ul>
        </div>
    }
}

/// Unified diff, one collapsible section per file.
#[component]
pub fn DiffView(diff: String, truncated: bool) -> impl IntoView {
    if diff.trim().is_empty() {
        return view! { <p class="py-6 text-center text-sm text-zinc-500">"No changes."</p> }.into_any();
    }
    let mut files: Vec<(String, Vec<String>)> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            let name = line.rsplit(" b/").next().unwrap_or(line).to_owned();
            files.push((name, Vec::new()));
        } else if let Some((_, lines)) = files.last_mut() {
            lines.push(line.to_owned());
        }
    }
    view! {
        <div class="space-y-2">
            {truncated.then(|| view! { <p class="text-xs text-amber-600">"The diff is large; showing the first 4 MB."</p> })}
            {files.into_iter().map(|(name, lines)| {
                let adds = lines.iter().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
                let dels = lines.iter().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
                let open = lines.len() < 400;
                view! {
                    <details class="rounded-md border border-zinc-200 dark:border-zinc-800" open=open>
                        <summary class="flex cursor-pointer select-none items-center gap-2 px-3 py-1.5 text-sm">
                            <span class="min-w-0 flex-1 truncate font-mono text-xs">{name}</span>
                            <span class="text-xs text-emerald-600">{format!("+{adds}")}</span>
                            <span class="text-xs text-red-600">{format!("-{dels}")}</span>
                        </summary>
                        <pre class="overflow-x-auto border-t border-zinc-200 font-mono text-xs leading-5 dark:border-zinc-800">
                            {lines.into_iter().filter(|l| !l.starts_with("index ") && !l.starts_with("--- ") && !l.starts_with("+++ ")).map(|l| {
                                let cls = if l.starts_with("@@") {
                                    "bg-blue-50 text-blue-700 dark:bg-blue-950/50 dark:text-blue-300"
                                } else if l.starts_with('+') {
                                    "bg-emerald-50 dark:bg-emerald-950/40"
                                } else if l.starts_with('-') {
                                    "bg-red-50 dark:bg-red-950/40"
                                } else {
                                    ""
                                };
                                view! { <div class=format!("whitespace-pre px-3 {cls}")>{if l.is_empty() { " ".to_owned() } else { l }}</div> }
                            }).collect_view()}
                        </pre>
                    </details>
                }
            }).collect_view()}
        </div>
    }
    .into_any()
}
