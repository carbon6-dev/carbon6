//! Fleet page: workers, their status, and enrolling new ones.

use std::time::Duration;

use c6_api::{Enrollment, Worker, WorkerStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use crate::api;

fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

#[component]
pub fn FleetPage() -> impl IntoView {
    let workers = api::live().workers;
    view! { <main class="mx-auto max-w-6xl space-y-6 px-4 py-6"><Fleet workers /><Hosts /></main> }
}

/// The host inventory and whether each host answers.
#[component]
fn Hosts() -> impl IntoView {
    let hosts = LocalResource::new(api::hosts);
    let checking = RwSignal::new(std::collections::HashSet::<String>::new());
    let error = RwSignal::new(None::<String>);
    let check = move |name: String| {
        checking.update(|c| {
            c.insert(name.clone());
        });
        spawn_local(async move {
            if let Err(e) = api::check_host(name.clone()).await {
                error.set(Some(e));
            }
            checking.update(|c| {
                c.remove(&name);
            });
            hosts.refetch();
        });
    };
    view! {
        <section class="space-y-2">
            <div>
                <h2 class="text-base font-semibold">"Hosts"</h2>
                <p class="text-sm text-zinc-500">"Machines your workers reach over SSH. Sessions on them get their labels; templates can run on every host with given labels. Add and edit them in Settings → Hosts."</p>
            </div>
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
            <Suspense fallback=|| ()>
                {move || hosts.get().map(|r| match r {
                    Ok(hs) if hs.is_empty() => view! { <p class="text-sm text-zinc-500">"No hosts yet."</p> }.into_any(),
                    Ok(hs) => view! {
                        <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                            {hs.into_iter().map(|s| {
                                let name = s.host.name.clone();
                                let n2 = name.clone();
                                let (dot, state) = match &s.check {
                                    None => ("bg-zinc-400", "not checked".to_owned()),
                                    Some(c) if c.ok => ("bg-emerald-500", format!("reachable · {} ms · {}", c.latency_ms, c.message)),
                                    Some(c) => ("bg-red-500", format!("unreachable: {}", c.message)),
                                };
                                let when = s.check.as_ref().map(|c| format!("checked {} ago via {}", ago(now_ms() - c.at_ms), c.worker)).unwrap_or_default();
                                view! {
                                    <div class="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2 text-sm">
                                        <span class=format!("h-2 w-2 shrink-0 rounded-full {dot}")></span>
                                        <span class="font-medium">{name.clone()}</span>
                                        <span class="font-mono text-xs text-zinc-500">{s.host.target.clone()}</span>
                                        {(!s.host.worker.is_empty()).then(|| view! { <span class="text-xs text-zinc-500">{format!("via {}", s.host.worker)}</span> })}
                                        <span class="flex flex-wrap gap-1">{s.host.labels.iter().map(|l| view! { <span class="rounded bg-zinc-100 px-1.5 text-[11px] dark:bg-zinc-800">{l.clone()}</span> }).collect_view()}</span>
                                        {(!s.host.enabled).then(|| view! { <span class="text-xs text-zinc-500">"(off)"</span> })}
                                        <span class="flex-1"></span>
                                        <span class="max-w-md truncate text-xs text-zinc-500" title=when.clone()>{state}</span>
                                        <button class="rounded-md px-2 py-0.5 text-xs text-zinc-600 hover:bg-zinc-200/60 disabled:opacity-50 dark:text-zinc-400 dark:hover:bg-zinc-800"
                                            disabled=move || checking.with(|c| c.contains(&n2)) on:click=move |_| check(name.clone())>
                                            {let n3 = s.host.name.clone(); move || if checking.with(|c| c.contains(&n3)) { "Checking…" } else { "Check" }}
                                        </button>
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                    }.into_any(),
                    Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

/// One worker: its files, git changes and terminals.
#[component]
pub fn WorkerPage(id: String) -> impl IntoView {
    let live = api::live();
    let tab = RwSignal::new("files");
    // Only the id drives the page body; live status updates touch just the
    // header, so open terminals are not torn down on every heartbeat.
    let id = Memo::new(move |_| id.clone());
    let worker = move || live.workers.with(|ws| ws.iter().find(|w| w.id.to_string() == id.get()).cloned());
    view! {
        <main class="mx-auto max-w-6xl space-y-4 px-4 py-6">
            <div class="flex items-center gap-3">
                <h1 class="text-lg font-semibold">{move || worker().map(|w| w.name).unwrap_or_default()}</h1>
                {move || worker().map(|w| {
                    let online = w.status == WorkerStatus::Online;
                    view! { <span class=if online { "text-xs text-emerald-600" } else { "text-xs text-zinc-500" }>{if online { "online" } else { "offline" }}</span> }
                })}
                <span class="flex-1"></span>
                <TabButton tab label="Files" value="files" />
                <TabButton tab label="Terminals" value="terminals" />
            </div>
            {move || worker().filter(|w| w.status != WorkerStatus::Online).map(|_| view! {
                <p class="text-sm text-amber-700 dark:text-amber-400">"This worker is offline; files and terminals are unavailable until it reconnects."</p>
            })}
            {move || {
                let id = id.get();
                match tab.get() {
                    "terminals" => view! { <crate::term::Terminals worker_id=id /> }.into_any(),
                    _ => view! { <crate::files::FileBrowser worker_id=id /> }.into_any(),
                }
            }}
        </main>
    }
}

#[component]
pub fn TabButton(tab: RwSignal<&'static str>, label: &'static str, value: &'static str) -> impl IntoView {
    view! {
        <button
            class=move || if tab.get() == value { "rounded-md bg-zinc-200 px-2.5 py-1 text-sm dark:bg-zinc-800" } else { "rounded-md px-2.5 py-1 text-sm text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-900" }
            on:click=move |_| tab.set(value)
        >
            {label}
        </button>
    }
}

#[component]
pub fn Fleet(workers: ReadSignal<Vec<Worker>>) -> impl IntoView {
    // Ticks so "last seen" stays fresh between snapshots.
    let now = RwSignal::new(now_ms());
    set_interval(move || now.set(now_ms()), Duration::from_secs(5));
    let error = RwSignal::new(None::<String>);

    let online = move || workers.with(|ws| ws.iter().filter(|w| w.status == WorkerStatus::Online).count());
    let active = move || workers.with(|ws| ws.iter().filter(|w| w.status != WorkerStatus::Revoked).count());

    view! {
        <section class="space-y-4">
            <div class="flex items-end justify-between gap-4">
                <div>
                    <h1 class="text-lg font-semibold">"Workers"</h1>
                    <p class="text-sm text-zinc-500">{move || format!("{} online · {} enrolled", online(), active())}</p>
                </div>
                <AddWorker />
            </div>

            {move || error.get().map(|e| view! {
                <div class="rounded-md border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200">{e}</div>
            })}

            <div class="overflow-x-auto rounded-lg border border-zinc-200 dark:border-zinc-800">
                <table class="w-full text-sm">
                    <thead class="bg-zinc-100 text-left text-xs uppercase tracking-wide text-zinc-500 dark:bg-zinc-900">
                        <tr>
                            <th class="px-3 py-2 font-medium">"Name"</th>
                            <th class="px-3 py-2 font-medium">"Host"</th>
                            <th class="px-3 py-2 font-medium">"Platform"</th>
                            <th class="px-3 py-2 font-medium">"Version"</th>
                            <th class="px-3 py-2 font-medium">"Last seen"</th>
                            <th class="px-3 py-2 font-medium text-right">"RTT"</th>
                            <th class="px-3 py-2"></th>
                        </tr>
                    </thead>
                    <tbody class="divide-y divide-zinc-200 dark:divide-zinc-800">
                        {move || {
                            let ws = workers.get();
                            if ws.is_empty() {
                                view! {
                                    <tr><td colspan="7" class="px-3 py-10 text-center text-zinc-500">
                                        "No workers yet. Add one to get started."
                                    </td></tr>
                                }.into_any()
                            } else {
                                ws.into_iter().map(|w| view! { <WorkerRow w now error /> }).collect_view().into_any()
                            }
                        }}
                    </tbody>
                </table>
            </div>
        </section>
    }
}

#[component]
fn WorkerRow(w: Worker, now: RwSignal<i64>, error: RwSignal<Option<String>>) -> impl IntoView {
    let (dot, label) = match w.status {
        WorkerStatus::Online => ("bg-emerald-500", "online"),
        WorkerStatus::Offline => ("bg-zinc-400", "offline"),
        WorkerStatus::Revoked => ("bg-red-500", "revoked"),
    };
    let revoked = w.status == WorkerStatus::Revoked;
    let id = w.id.to_string();
    let last_seen = w.last_seen;
    let online = w.status == WorkerStatus::Online;

    let name = w.name.clone();
    let act = move |id: String| {
        move |_| {
            let id = id.clone();
            let confirm_msg = if revoked {
                format!("Remove {name}? It will be forgotten entirely.")
            } else {
                format!("Revoke {name}? It will be disconnected and must re-enroll.")
            };
            if !window().confirm_with_message(&confirm_msg).unwrap_or(false) {
                return;
            }
            spawn_local(async move {
                let r = if revoked { api::delete_worker(id).await } else { api::revoke_worker(id).await };
                error.set(r.err());
            });
        }
    };

    view! {
        <tr class=if revoked { "opacity-60" } else { "" }>
            <td class="px-3 py-2">
                <div class="flex items-center gap-2">
                    <span class=format!("h-2 w-2 shrink-0 rounded-full {dot}") title=label></span>
                    <crate::workspace::Go to=crate::workspace::View::Worker { id: w.id.to_string() } class="font-medium hover:underline">{w.name.clone()}</crate::workspace::Go>
                </div>
            </td>
            <td class="px-3 py-2 text-zinc-600 dark:text-zinc-400">{w.hostname}</td>
            <td class="px-3 py-2 text-zinc-600 dark:text-zinc-400">{format!("{}/{}", w.os, w.arch)}</td>
            <td class="px-3 py-2 font-mono text-xs text-zinc-600 dark:text-zinc-400">{w.version.unwrap_or_default()}</td>
            <td class="px-3 py-2 text-zinc-600 dark:text-zinc-400">
                {move || if online { "now".to_owned() } else { last_seen.map(|t| ago(now.get() - t)).unwrap_or_else(|| "never".into()) }}
            </td>
            <td class="px-3 py-2 text-right font-mono text-xs text-zinc-600 dark:text-zinc-400">
                {w.rtt_us.map(|us| format!("{:.1} ms", us as f64 / 1000.0)).unwrap_or_default()}
            </td>
            <td class="px-3 py-2 text-right">
                <button
                    class="rounded px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-100 hover:text-red-600 dark:hover:bg-zinc-800"
                    on:click=act(id)
                >
                    {if revoked { "Remove" } else { "Revoke" }}
                </button>
            </td>
        </tr>
    }
}

fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

#[component]
fn AddWorker() -> impl IntoView {
    let open = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<Enrollment, String>>);

    let generate = move |_| {
        pending.set(true);
        let n = name.get_untracked().trim().to_owned();
        spawn_local(async move {
            result.set(Some(api::create_enrollment((!n.is_empty()).then_some(n)).await));
            pending.set(false);
        });
    };
    let close = move |_| {
        open.set(false);
        result.set(None);
        name.set(String::new());
    };

    view! {
        <button
            class="rounded-md bg-zinc-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-zinc-700 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
            on:click=move |_| open.set(true)
        >
            "Add worker"
        </button>
        <Show when=move || open.get()>
            <div class="fixed inset-0 z-10 flex items-center justify-center bg-black/40 px-4" on:click=close>
                <div
                    class="w-full max-w-xl space-y-4 rounded-lg bg-white p-5 shadow-xl dark:bg-zinc-900"
                    on:click=|ev| ev.stop_propagation()
                >
                    <h2 class="font-semibold">"Add a worker"</h2>
                    {move || match result.get() {
                        None => view! {
                            <div class="space-y-3">
                                <label class="block text-sm">
                                    <span class="text-zinc-600 dark:text-zinc-400">"Name (optional, defaults to hostname)"</span>
                                    <input
                                        class="mt-1 w-full rounded-md border border-zinc-300 bg-transparent px-2 py-1.5 dark:border-zinc-700"
                                        prop:value=move || name.get()
                                        on:input=move |ev| name.set(event_target_value(&ev))
                                    />
                                </label>
                                <div class="flex justify-end gap-2">
                                    <button class="rounded-md px-3 py-1.5 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=close>"Cancel"</button>
                                    <button
                                        class="rounded-md bg-zinc-900 px-3 py-1.5 text-sm font-medium text-white disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900"
                                        disabled=move || pending.get()
                                        on:click=generate
                                    >
                                        "Create enrollment token"
                                    </button>
                                </div>
                            </div>
                        }.into_any(),
                        Some(Ok(e)) => view! { <TokenView enrollment=e on_done=close /> }.into_any(),
                        Some(Err(msg)) => view! {
                            <div class="space-y-3">
                                <p class="text-sm text-red-600">{msg}</p>
                                <div class="flex justify-end"><button class="rounded-md px-3 py-1.5 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=close>"Close"</button></div>
                            </div>
                        }.into_any(),
                    }}
                </div>
            </div>
        </Show>
    }
}

#[component]
fn TokenView(enrollment: Enrollment, on_done: impl Fn(leptos::ev::MouseEvent) + 'static) -> impl IntoView {
    let command = format!("c6w enroll {}", enrollment.token);
    let minutes = ((enrollment.expires_at - now_ms()) / 60_000).max(0);
    let copied = RwSignal::new(false);
    let input_ref = NodeRef::<leptos::html::Input>::new();

    let copy = {
        let command = command.clone();
        move |_| {
            let w = window();
            // The Clipboard API only exists in secure contexts; the head is
            // usually plain HTTP on a private address, so fall back to
            // selecting the text for a manual copy.
            if w.is_secure_context() {
                let _ = w.navigator().clipboard().write_text(&command);
                copied.set(true);
            } else if let Some(input) = input_ref.get() {
                let input: web_sys::HtmlInputElement = input.unchecked_into();
                input.select();
            }
        }
    };

    view! {
        <div class="space-y-3 text-sm">
            <p class="text-zinc-600 dark:text-zinc-400">
                "Run this on the worker machine. The token works once and expires in "
                {minutes} " minutes."
            </p>
            <div class="flex gap-2">
                <input
                    node_ref=input_ref
                    readonly
                    class="w-full rounded-md border border-zinc-300 bg-zinc-50 px-2 py-1.5 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-950"
                    prop:value=command.clone()
                    on:focus=|ev| {
                        let input: web_sys::HtmlInputElement = event_target(&ev);
                        input.select();
                    }
                />
                <button class="shrink-0 rounded-md border border-zinc-300 px-3 py-1.5 hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800" on:click=copy>
                    {move || if copied.get() { "Copied" } else { "Copy" }}
                </button>
            </div>
            <p class="text-zinc-500">"Then start it with " <code class="font-mono">"c6w run"</code> ". It will appear here once attached."</p>
            <div class="flex justify-end">
                <button class="rounded-md px-3 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=on_done>"Done"</button>
            </div>
        </div>
    }
}
