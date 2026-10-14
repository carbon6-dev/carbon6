//! Remote file browsing and git changes on a worker.

use leptos::prelude::*;
use leptos::task::spawn_local;
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
    let not_repo = move || matches!(status.get(), Some(Ok(ref s)) if s["repo"] == false);
    view! {
        <div class="space-y-3">
            <Show when=not_repo>
                <p class="rounded-md border border-zinc-200 px-4 py-6 text-center text-sm text-zinc-500 dark:border-zinc-800">
                    "This folder isn't in a git repository, so there are no changes to show. Initialize one (for example "
                    <code class="font-mono">"git init"</code> " in a terminal) to track what the agent changes."
                </p>
            </Show>
            <Show when=move || !not_repo()>
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
            </Show>
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

/// Modal folder chooser for a worker: folders only, breadcrumb navigation,
/// and "New folder". `on_select` receives the chosen absolute path.
#[component]
pub fn FolderPicker(
    worker_id: String,
    #[prop(optional)] start: Option<String>,
    on_select: Callback<String>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let wid = StoredValue::new(worker_id);
    let dir = RwSignal::new(start.unwrap_or_default());
    let hidden = RwSignal::new(false);
    let listing = LocalResource::new(move || api::fs_list(wid.get_value(), dir.get(), hidden.get()));
    // The resolved absolute path of what is shown (the start may be "" or "~").
    let current = Memo::new(move |_| listing.get().and_then(|r| r.ok()).and_then(|l| l["path"].as_str().map(str::to_owned)));
    let new_name = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);

    // A remembered folder that no longer exists: fall back to home.
    Effect::new(move || {
        if let Some(Err(_)) = listing.get()
            && !dir.get_untracked().is_empty()
        {
            dir.set(String::new());
        }
    });

    let create = move || {
        let (Some(base), Some(name)) = (current.get_untracked(), new_name.get_untracked()) else { return };
        let name = name.trim().to_owned();
        if name.is_empty() || name.contains('/') {
            error.set(Some("Enter a folder name (without /).".into()));
            return;
        }
        let path = if base.ends_with('/') { format!("{base}{name}") } else { format!("{base}/{name}") };
        spawn_local(async move {
            match api::fs_mkdir(wid.get_value(), path).await {
                Ok(v) => {
                    new_name.set(None);
                    error.set(None);
                    dir.set(v["path"].as_str().unwrap_or_default().to_owned());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    view! {
        <div class="fixed inset-0 z-30 flex items-center justify-center bg-black/40 px-4" on:click=move |_| on_cancel.run(())>
            <div class="flex max-h-[85vh] w-full max-w-2xl flex-col gap-3 rounded-lg bg-white p-4 shadow-xl dark:bg-zinc-900" on:click=|ev| ev.stop_propagation()>
                <h2 class="font-medium">"Choose a working directory"</h2>
                // Breadcrumbs: each segment is clickable.
                <div class="flex flex-wrap items-center gap-0.5 font-mono text-xs">
                    {move || {
                        let path = current.get().unwrap_or_default();
                        let mut acc = String::new();
                        let mut crumbs = vec![("/".to_owned(), "/".to_owned())];
                        for seg in path.split('/').filter(|s| !s.is_empty()) {
                            acc.push('/');
                            acc.push_str(seg);
                            crumbs.push((format!("{seg}/"), acc.clone()));
                        }
                        crumbs.into_iter().map(|(label, target)| view! {
                            <button class="rounded px-1 py-0.5 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| dir.set(target.clone())>{label}</button>
                        }).collect_view()
                    }}
                </div>
                <div class="min-h-48 flex-1 overflow-auto rounded-md border border-zinc-200 dark:border-zinc-800">
                    <Suspense fallback=|| view! { <p class="p-3 text-sm text-zinc-500">"Loading…"</p> }>
                        {move || listing.get().map(|r| match r {
                            Err(e) => view! { <div class="p-2"><ErrorBox message=e /></div> }.into_any(),
                            Ok(l) => {
                                let base = l["path"].as_str().unwrap_or("").to_owned();
                                let dirs: Vec<String> = l["entries"].as_array().into_iter().flatten()
                                    .filter(|e| e["kind"] == "dir")
                                    .filter_map(|e| e["name"].as_str().map(str::to_owned))
                                    .collect();
                                if dirs.is_empty() {
                                    return view! { <p class="p-3 text-sm text-zinc-500">"No folders here."</p> }.into_any();
                                }
                                view! {
                                    <ul class="divide-y divide-zinc-100 text-sm dark:divide-zinc-900">
                                        {dirs.into_iter().map(|name| {
                                            let full = if base.ends_with('/') { format!("{base}{name}") } else { format!("{base}/{name}") };
                                            view! {
                                                <li><button class="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                                    on:click=move |_| dir.set(full.clone())>
                                                    <span class="text-zinc-400">"▸"</span><span class="truncate">{name}</span>
                                                </button></li>
                                            }
                                        }).collect_view()}
                                    </ul>
                                }.into_any()
                            }
                        })}
                    </Suspense>
                </div>
                {move || match new_name.get() {
                    Some(_) => view! {
                        <div class="flex gap-2">
                            <input class=format!("{INPUT}") placeholder="New folder name" autofocus
                                prop:value=move || new_name.get().unwrap_or_default()
                                on:input=move |ev| new_name.set(Some(event_target_value(&ev)))
                                on:keydown=move |ev| match ev.key().as_str() { "Enter" => create(), "Escape" => new_name.set(None), _ => {} } />
                            <button class=crate::util::BUTTON on:click=move |_| create()>"Create"</button>
                            <button class=crate::util::BUTTON_QUIET on:click=move |_| new_name.set(None)>"Cancel"</button>
                        </div>
                    }.into_any(),
                    None => ().into_any(),
                }}
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                <div class="flex flex-wrap items-center gap-2">
                    <button class=crate::util::BUTTON_QUIET on:click=move |_| { error.set(None); new_name.set(Some(String::new())) }>"New folder"</button>
                    <label class="flex items-center gap-1.5 text-xs text-zinc-500">
                        <input type="checkbox" prop:checked=move || hidden.get() on:change=move |ev| hidden.set(event_target_checked(&ev)) />
                        "Show hidden"
                    </label>
                    <span class="flex-1"></span>
                    <button class=crate::util::BUTTON_QUIET on:click=move |_| on_cancel.run(())>"Cancel"</button>
                    <button class=crate::util::BUTTON disabled=move || current.get().is_none()
                        on:click=move |_| if let Some(p) = current.get_untracked() { on_select.run(p) }>
                        {move || format!("Use {}", current.get().map(|p| p.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("/").to_owned()).unwrap_or_default())}
                    </button>
                </div>
            </div>
        </div>
    }
}

/// Where a file is picked from.
#[derive(Clone, Debug, PartialEq)]
enum Source {
    /// This browser's device (its own file chooser).
    Device,
    /// The head's disk.
    Head,
    /// A worker's disk: (id, name).
    Worker(String, String),
}

impl Source {
    fn key(&self) -> String {
        match self {
            Source::Device => "device".into(),
            Source::Head => "head".into(),
            Source::Worker(id, _) => id.clone(),
        }
    }
}

/// Modal file chooser: a file on this device, on the head or on a worker
/// (with folder navigation). `extensions` are the endings offered (others
/// behind "All files"). `on_pick` receives the file's name and text.
#[component]
pub fn FilePicker(
    title: &'static str,
    extensions: &'static [&'static str],
    on_pick: Callback<(String, String)>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let live = api::live();
    let workers = Memo::new(move |_| {
        live.workers.with(|ws| {
            ws.iter()
                .filter(|w| w.status == c6_api::WorkerStatus::Online)
                .map(|w| (w.id.to_string(), w.name.clone()))
                .collect::<Vec<_>>()
        })
    });
    // Where you picked from last time, and the folder you were in there.
    let remembered = crate::util::storage_get("c6.pick.source").unwrap_or_else(|| "head".into());
    let source = RwSignal::new(match remembered.as_str() {
        "device" => Source::Device,
        "head" => Source::Head,
        id => workers.get_untracked().into_iter().find(|(w, _)| w == id).map_or(Source::Head, |(w, n)| Source::Worker(w, n)),
    });
    let dir_of = |s: &Source| crate::util::storage_get(&format!("c6.pick.dir.{}", s.key())).unwrap_or_default();
    let dir = RwSignal::new(dir_of(&source.get_untracked()));
    let hidden = RwSignal::new(false);
    let all = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let listing = LocalResource::new(move || {
        let (s, d, h) = (source.get(), dir.get(), hidden.get());
        async move {
            match s {
                Source::Device => Ok(Value::Null),
                Source::Head => api::head_fs_list(d, h).await,
                Source::Worker(id, _) => api::fs_list(id, d, h).await,
            }
        }
    });
    let current = Memo::new(move |_| listing.get().and_then(|r| r.ok()).and_then(|l| l["path"].as_str().map(str::to_owned)));
    // A remembered folder that no longer exists: fall back to home.
    Effect::new(move || {
        if let Some(Err(_)) = listing.get()
            && !dir.get_untracked().is_empty()
        {
            dir.set(String::new());
        }
    });
    let choose = move |s: Source| {
        crate::util::storage_set("c6.pick.source", &s.key());
        dir.set(dir_of(&s));
        error.set(None);
        source.set(s);
    };
    let read = move |path: String, name: String| {
        busy.set(true);
        let s = source.get_untracked();
        if let Some(base) = current.get_untracked() {
            crate::util::storage_set(&format!("c6.pick.dir.{}", s.key()), &base);
        }
        spawn_local(async move {
            let r = match s {
                Source::Worker(id, _) => api::fs_read(id, path).await,
                _ => api::head_fs_read(path).await,
            };
            busy.set(false);
            match r {
                Ok(v) if v["binary"].as_bool() == Some(true) => error.set(Some(format!("{name} is not a text file"))),
                Ok(v) if v["truncated"].as_bool() == Some(true) => error.set(Some(format!("{name} is too large to load"))),
                Ok(v) => on_pick.run((name, v["content"].as_str().unwrap_or_default().to_owned())),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let device_input = NodeRef::<leptos::html::Input>::new();
    let from_device = move |ev: leptos::ev::Event| {
        use wasm_bindgen::JsCast;
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
        let name = file.name();
        spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(file.text()).await {
                Ok(v) => on_pick.run((name, v.as_string().unwrap_or_default())),
                Err(_) => error.set(Some("could not read the file".into())),
            }
        });
    };
    let accept = extensions.join(",");
    let tab = "rounded-md px-2.5 py-1 text-sm";
    view! {
        <div class="fixed inset-0 z-30 flex items-center justify-center bg-black/40 px-4" on:click=move |_| on_cancel.run(())>
            <div class="flex max-h-[85vh] w-full max-w-2xl flex-col gap-3 rounded-lg bg-white p-4 shadow-xl dark:bg-zinc-900" on:click=|ev| ev.stop_propagation()>
                <h2 class="font-medium">{title}</h2>
                <div class="flex flex-wrap gap-1">
                    {move || {
                        let mut sources = vec![(Source::Head, "Head".to_owned()), (Source::Device, "This device".to_owned())];
                        sources.extend(workers.get().into_iter().map(|(id, name)| (Source::Worker(id, name.clone()), format!("Worker {name}"))));
                        sources.into_iter().map(|(s, label)| {
                            let on = s.clone();
                            view! {
                                <button class=move || format!("{tab} {}", if source.with(|c| *c == on) { "bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-800" })
                                    on:click=move |_| choose(s.clone())>{label}</button>
                            }
                        }).collect_view()
                    }}
                </div>
                {move || if source.get() == Source::Device {
                    view! {
                        <div class="flex min-h-48 flex-col items-center justify-center gap-2 rounded-md border border-dashed border-zinc-300 p-6 text-sm text-zinc-500 dark:border-zinc-700">
                            <p>"A file on the device you are using now."</p>
                            <button class=crate::util::BUTTON on:click=move |_| if let Some(i) = device_input.get_untracked() { i.click(); }>"Choose a file…"</button>
                            <input node_ref=device_input type="file" accept=accept.clone() class="hidden" on:change=from_device />
                        </div>
                    }.into_any()
                } else {
                    view! {
                        // Breadcrumbs: each segment is clickable.
                        <div class="flex flex-wrap items-center gap-0.5 font-mono text-xs">
                            {move || {
                                let path = current.get().unwrap_or_default();
                                let mut acc = String::new();
                                let mut crumbs = vec![("/".to_owned(), "/".to_owned())];
                                for seg in path.split('/').filter(|s| !s.is_empty()) {
                                    acc.push('/');
                                    acc.push_str(seg);
                                    crumbs.push((format!("{seg}/"), acc.clone()));
                                }
                                crumbs.into_iter().map(|(label, target)| view! {
                                    <button class="rounded px-1 py-0.5 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| dir.set(target.clone())>{label}</button>
                                }).collect_view()
                            }}
                            <button class="ml-1 rounded px-1 py-0.5 text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800" title="Home folder" on:click=move |_| dir.set("~".into())>"~"</button>
                        </div>
                        <div class="min-h-48 flex-1 overflow-auto rounded-md border border-zinc-200 dark:border-zinc-800">
                            <Suspense fallback=|| view! { <p class="p-3 text-sm text-zinc-500">"Loading…"</p> }>
                                {move || listing.get().map(|r| match r {
                                    Err(e) => view! { <div class="p-2"><ErrorBox message=e /></div> }.into_any(),
                                    Ok(l) => {
                                        let base = l["path"].as_str().unwrap_or("").to_owned();
                                        let show_all = all.get();
                                        let rows: Vec<(String, bool)> = l["entries"].as_array().into_iter().flatten()
                                            .filter_map(|e| Some((e["name"].as_str()?.to_owned(), e["kind"] == "dir")))
                                            .filter(|(n, d)| *d || show_all || extensions.iter().any(|x| n.to_lowercase().ends_with(x)))
                                            .collect();
                                        if rows.is_empty() {
                                            return view! { <p class="p-3 text-sm text-zinc-500">"Nothing to pick here."</p> }.into_any();
                                        }
                                        view! {
                                            <ul class="divide-y divide-zinc-100 text-sm dark:divide-zinc-900">
                                                {rows.into_iter().map(|(name, is_dir)| {
                                                    let full = if base.ends_with('/') { format!("{base}{name}") } else { format!("{base}/{name}") };
                                                    let shown = name.clone();
                                                    view! {
                                                        <li><button class="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                                            on:click=move |_| if is_dir { dir.set(full.clone()) } else { read(full.clone(), name.clone()) }>
                                                            <span class="w-3 text-zinc-400">{if is_dir { "▸" } else { "" }}</span>
                                                            <span class=if is_dir { "truncate" } else { "truncate font-mono text-xs" }>{shown}</span>
                                                        </button></li>
                                                    }
                                                }).collect_view()}
                                            </ul>
                                        }.into_any()
                                    }
                                })}
                            </Suspense>
                        </div>
                    }.into_any()
                }}
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                <div class="flex flex-wrap items-center gap-3">
                    {move || (source.get() != Source::Device).then(|| view! {
                        <label class="flex items-center gap-1.5 text-xs text-zinc-500">
                            <input type="checkbox" prop:checked=move || all.get() on:change=move |ev| all.set(event_target_checked(&ev)) />
                            {format!("All files (not only {})", extensions.join(", "))}
                        </label>
                        <label class="flex items-center gap-1.5 text-xs text-zinc-500">
                            <input type="checkbox" prop:checked=move || hidden.get() on:change=move |ev| hidden.set(event_target_checked(&ev)) />
                            "Show hidden"
                        </label>
                    })}
                    <span class="flex-1"></span>
                    {move || busy.get().then(|| view! { <span class="text-xs text-zinc-500">"Reading…"</span> })}
                    <button class=crate::util::BUTTON_QUIET on:click=move |_| on_cancel.run(())>"Cancel"</button>
                </div>
            </div>
        </div>
    }
}
