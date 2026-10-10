//! A session: streaming transcript, queued input, composer.

use std::collections::HashMap;

use c6_api::{ContentBlock, Event, EventKind, QueuedInput, SessionStatus, StreamMsg, TurnEndReason};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_navigate, use_params_map};

use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, StatusPill};
use crate::{api, markdown, sessions};

/// Live-only state, replaced by durable events as they arrive.
#[derive(Clone, Default)]
struct Streaming {
    text: String,
    thinking: String,
    tool_output: HashMap<String, String>,
}

#[derive(Clone, PartialEq)]
struct ToolResultView {
    output: String,
    is_error: bool,
    truncated: bool,
    duration_ms: u64,
}

#[component]
pub fn SessionPage() -> impl IntoView {
    let params = use_params_map();
    let id = move || params.read().get("id").unwrap_or_default();
    view! { {move || { let id = id(); view! { <SessionView id /> } }} }
}

#[component]
fn SessionView(id: String) -> impl IntoView {
    let live = api::live();
    let detail = LocalResource::new({
        let id = id.clone();
        move || api::session(id.clone())
    });
    let events = RwSignal::new(Vec::<Event>::new());
    let streaming = RwSignal::new(Streaming::default());
    let queued = RwSignal::new(Vec::<QueuedInput>::new());
    let activity = RwSignal::new(None::<bool>);
    let error = RwSignal::new(None::<String>);

    let handle = api::stream_session(
        id.clone(),
        move |msg| match msg {
            StreamMsg::Event { event } => {
                streaming.update(|s| match &event.kind {
                    EventKind::Assistant { .. } => {
                        s.text.clear();
                        s.thinking.clear();
                    }
                    EventKind::ToolResult { call_id, .. } => {
                        s.tool_output.remove(call_id);
                    }
                    EventKind::TurnEnded { .. } => *s = Streaming::default(),
                    _ => {}
                });
                if let EventKind::TurnEnded { .. } = event.kind {
                    activity.set(Some(false));
                }
                if let EventKind::TurnStarted { .. } = event.kind {
                    activity.set(Some(true));
                }
                events.update(|evs| {
                    if evs.last().is_none_or(|l| l.seq < event.seq) {
                        evs.push(event)
                    }
                });
            }
            StreamMsg::Text { text, .. } => streaming.update(|s| s.text.push_str(&text)),
            StreamMsg::Thinking { text, .. } => streaming.update(|s| s.thinking.push_str(&text)),
            StreamMsg::ToolOutput { call_id, text } => streaming.update(|s| s.tool_output.entry(call_id).or_default().push_str(&text)),
            StreamMsg::Activity { running } => activity.set(Some(running)),
            StreamMsg::Queue { queued: q } => queued.set(q),
        },
        move || events.with_untracked(|e| e.last().map(|e| e.seq).unwrap_or(0)),
    );
    on_cleanup(move || handle.abort());

    let status = {
        let id = id.clone();
        Memo::new(move |_| {
            sessions::live_status(&id)
                .or_else(|| detail.get().and_then(|d| d.ok()).map(|d| d.session.status))
                .unwrap_or(SessionStatus::Starting)
        })
    };
    let running = move || status.get() == SessionStatus::Running || activity.get() == Some(true);

    // Follow the bottom of the transcript unless the user scrolled up.
    let stick = RwSignal::new(true);
    let _scroll = window_event_listener(leptos::ev::scroll, move |_| {
        let w = window();
        let doc_h = document().document_element().map(|e| e.scroll_height()).unwrap_or(0) as f64;
        let bottom = w.scroll_y().unwrap_or(0.0) + w.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        stick.set(doc_h - bottom < 120.0);
    });
    Effect::new(move || {
        events.track();
        streaming.track();
        queued.track();
        if stick.get_untracked() {
            request_animation_frame(|| {
                let h = document().document_element().map(|e| e.scroll_height()).unwrap_or(0);
                window().scroll_to_with_x_and_y(0.0, h as f64);
            });
        }
    });

    let results = Memo::new(move |_| {
        events.with(|evs| {
            evs.iter()
                .filter_map(|e| match &e.kind {
                    EventKind::ToolResult { call_id, output, is_error, truncated, duration_ms, .. } => Some((
                        call_id.clone(),
                        ToolResultView { output: output.clone(), is_error: *is_error, truncated: *truncated, duration_ms: *duration_ms },
                    )),
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        })
    });
    let usage = Memo::new(move |_| {
        events.with(|evs| {
            evs.iter().fold((0u64, 0u64, 0u64), |(i, c, o), e| match &e.kind {
                EventKind::Usage { input_tokens, cache_read_tokens, output_tokens, .. } => {
                    (i + input_tokens + cache_read_tokens, c + cache_read_tokens, o + output_tokens)
                }
                _ => (i, c, o),
            })
        })
    });

    let id_stored = StoredValue::new(id.clone());
    let navigate = use_navigate();

    view! {
        <main class="mx-auto max-w-4xl px-4 pb-48 pt-4">
            <Suspense fallback=|| ()>
                {move || detail.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(d) => {
                        let s = d.session.clone();
                        let worker_id = s.worker_id;
                        let online = move || live.workers.with(|ws| ws.iter().any(|w| w.id == worker_id && w.status == c6_api::WorkerStatus::Online));
                        let navigate = navigate.clone();
                        view! {
                            <div class="mb-4 flex flex-wrap items-start justify-between gap-3 border-b border-zinc-200 pb-3 dark:border-zinc-800">
                                <div class="min-w-0">
                                    <h1 class="truncate text-lg font-semibold">{s.title.clone()}</h1>
                                    <div class="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-zinc-500">
                                        <span class="flex items-center gap-1">
                                            <span class=move || if online() { "h-1.5 w-1.5 rounded-full bg-emerald-500" } else { "h-1.5 w-1.5 rounded-full bg-zinc-400" }></span>
                                            {d.worker_name.clone()}
                                        </span>
                                        <span class="font-mono">{s.cwd.clone()}</span>
                                        <span>{s.model.clone()}{s.effort.clone().map(|e| format!(" · {e}"))}</span>
                                        <span>{move || { let (i, c, o) = usage.get(); if i + o == 0 { String::new() } else { format!("{} in ({}% cached) · {} out", tokens(i), (c * 100).checked_div(i).unwrap_or(0), tokens(o)) } }}</span>
                                    </div>
                                </div>
                                <div class="flex items-center gap-2">
                                    {move || view! { <StatusPill status=status.get() /> }}
                                    <button class=format!("{BUTTON_QUIET} text-xs text-zinc-500") on:click=move |_| {
                                        let navigate = navigate.clone();
                                        spawn_local(async move {
                                            if api::archive(id_stored.get_value()).await.is_ok() { navigate("/", Default::default()); }
                                        });
                                    }>"Archive"</button>
                                </div>
                            </div>
                            {s.error.clone().map(|e| view! { <ErrorBox message=format!("The session failed to start: {e}") /> })}
                        }.into_any()
                    }
                })}
            </Suspense>

            <div class="space-y-4">
                // Events are append-only: keyed by seq, each renders once, so
                // expanded cards stay open while the turn continues.
                <For each=move || events.get() key=|e| e.seq let:e>
                    {render_event(&e, results, streaming)}
                </For>
                {move || streaming.with(|s| (!s.thinking.is_empty() || !s.text.is_empty()).then(|| view! {
                    <div class="space-y-2">
                        {(!s.thinking.is_empty()).then(|| view! { <Thinking text=s.thinking.clone() open=true /> })}
                        {(!s.text.is_empty()).then(|| view! { <div class="md" inner_html=markdown::to_html(&s.text)></div> })}
                    </div>
                }))}
                {move || (running() && streaming.with(|s| s.text.is_empty() && s.thinking.is_empty())).then(|| view! {
                    <div class="flex items-center gap-2 text-sm text-zinc-500">
                        <span class="h-2 w-2 animate-pulse rounded-full bg-blue-500"></span>"Working…"
                    </div>
                })}
                {move || queued.get().into_iter().map(|q| view! { <Queued q session_id=id_stored.get_value() /> }).collect_view()}
            </div>

            <Composer id=id_stored.get_value() running=Signal::derive(running) disabled=Signal::derive(move || status.get() == SessionStatus::Failed) error />
        </main>
    }
}

fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn render_event(e: &Event, results: Memo<HashMap<String, ToolResultView>>, streaming: RwSignal<Streaming>) -> AnyView {
    match &e.kind {
        EventKind::Started { system_prompt, tools, .. } => view! {
            <details class="text-xs text-zinc-500">
                <summary class="cursor-pointer select-none">{format!("Session started · {} tools", tools.len())}</summary>
                <pre class="mt-2 max-h-96 overflow-auto whitespace-pre-wrap rounded-md bg-zinc-100 p-3 font-mono text-[11px] dark:bg-zinc-900">{system_prompt.clone()}</pre>
            </details>
        }
        .into_any(),
        EventKind::User { text, .. } => view! {
            <div class="flex justify-end">
                <div class="max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-sm bg-zinc-200 px-4 py-2 text-sm dark:bg-zinc-800">{text.clone()}</div>
            </div>
        }
        .into_any(),
        EventKind::Assistant { blocks, stop_reason, .. } => {
            let interrupted = stop_reason == "interrupted";
            view! {
                <div class="space-y-2">
                    {blocks.iter().map(|b| match b {
                        ContentBlock::Text { text } => view! { <div class="md" inner_html=markdown::to_html(text)></div> }.into_any(),
                        ContentBlock::Thinking { text } => view! { <Thinking text=text.clone() open=false /> }.into_any(),
                        ContentBlock::ToolUse { id, name, input_json } => {
                            let (out_id, res_id) = (id.clone(), id.clone());
                            let live_output = Signal::derive(move || streaming.with(|s| s.tool_output.get(&out_id).cloned()));
                            let result = Signal::derive(move || results.with(|r| r.get(&res_id).cloned()));
                            view! { <ToolCard name=name.clone() input_json=input_json.clone() result live_output /> }.into_any()
                        }
                    }).collect_view()}
                    {interrupted.then(|| view! { <p class="text-xs italic text-zinc-500">"(response cut off)"</p> })}
                </div>
            }
            .into_any()
        }
        EventKind::Error { message } => view! { <ErrorBox message=message.clone() /> }.into_any(),
        EventKind::Compaction { summary, tokens_before } => view! {
            <details class="rounded-md border border-dashed border-zinc-300 px-3 py-2 text-xs text-zinc-500 dark:border-zinc-700">
                <summary class="cursor-pointer select-none">
                    {format!("Context compacted ({} tokens summarized); the model continues from this summary", tokens(*tokens_before))}
                </summary>
                <div class="md mt-2 text-zinc-700 dark:text-zinc-300" inner_html=markdown::to_html(summary)></div>
            </details>
        }
        .into_any(),
        EventKind::TurnEnded { reason, .. } => match reason {
            TurnEndReason::Interrupted => note("Interrupted"),
            TurnEndReason::WorkerRestarted => note("Stopped: the worker restarted during this turn"),
            _ => ().into_any(),
        },
        _ => ().into_any(),
    }
}

fn note(text: &'static str) -> AnyView {
    view! { <p class="text-center text-xs text-zinc-500">{text}</p> }.into_any()
}

#[component]
fn Thinking(text: String, open: bool) -> impl IntoView {
    view! {
        <details class="border-l-2 border-zinc-300 pl-3 text-sm text-zinc-500 dark:border-zinc-700" open=open>
            <summary class="cursor-pointer select-none text-xs">"Thinking"</summary>
            <div class="mt-1 whitespace-pre-wrap text-xs">{text}</div>
        </details>
    }
}

/// Normalizes tool names across profiles (Claude, Codex, generic).
fn tool_kind(name: &str) -> &str {
    match name {
        "Bash" | "shell" => "bash",
        "Read" => "read",
        "Write" => "write",
        "Edit" => "edit",
        "Grep" => "grep",
        "Glob" => "glob",
        "TodoWrite" | "update_plan" => "plan",
        other => other,
    }
}

/// One-line description of a tool call from its input.
fn summarize(name: &str, input: &serde_json::Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let path = || input.get("file_path").or_else(|| input.get("path")).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    match tool_kind(name) {
        "bash" => match input.get("command") {
            Some(serde_json::Value::Array(argv)) => {
                let argv: Vec<&str> = argv.iter().filter_map(|a| a.as_str()).collect();
                let script = if argv.len() == 3 && argv[1].ends_with('c') { argv[2].to_owned() } else { argv.join(" ") };
                script.lines().next().unwrap_or("").to_owned()
            }
            _ => {
                let desc = s("description");
                if desc.is_empty() { s("command").lines().next().unwrap_or("").to_owned() } else { desc }
            }
        },
        "read" | "write" | "edit" => path(),
        "grep" => {
            format!("{}{}", s("pattern"), input.get("path").and_then(|v| v.as_str()).map(|p| format!("  in {p}")).unwrap_or_default())
        }
        "glob" => s("pattern"),
        "apply_patch" => {
            s("input").lines().filter_map(|l| l.strip_prefix("*** ").filter(|r| r.contains("File:"))).collect::<Vec<_>>().join(", ")
        }
        "plan" => {
            let items = plan_items(input);
            let done = items.iter().filter(|(_, st)| st == "completed").count();
            format!("{done}/{} done", items.len())
        }
        _ => input.to_string(),
    }
}

/// (text, status) for update_plan / TodoWrite inputs.
fn plan_items(input: &serde_json::Value) -> Vec<(String, String)> {
    let list = input.get("plan").or_else(|| input.get("todos")).and_then(|v| v.as_array()).cloned().unwrap_or_default();
    list.iter()
        .map(|i| {
            let text = i.get("step").or_else(|| i.get("content")).and_then(|v| v.as_str()).unwrap_or("").to_owned();
            (text, i.get("status").and_then(|v| v.as_str()).unwrap_or("pending").to_owned())
        })
        .collect()
}

#[component]
fn ToolCard(
    name: String,
    input_json: String,
    result: Signal<Option<ToolResultView>>,
    live_output: Signal<Option<String>>,
) -> impl IntoView {
    let input: serde_json::Value = serde_json::from_str(&input_json).unwrap_or(serde_json::Value::Null);
    let summary = summarize(&name, &input);
    let kind = tool_kind(&name).to_owned();
    let detail_input = match kind.as_str() {
        "write" => input.get("content").and_then(|v| v.as_str()).map(str::to_owned),
        "edit" => Some(format!(
            "- {}\n+ {}",
            input.get("old_string").and_then(|v| v.as_str()).unwrap_or("").replace('\n', "\n- "),
            input.get("new_string").and_then(|v| v.as_str()).unwrap_or("").replace('\n', "\n+ ")
        )),
        "bash" => {
            let s = summarize(&name, &input);
            let full = match input.get("command") {
                Some(serde_json::Value::String(c)) => c.clone(),
                Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" "),
                _ => String::new(),
            };
            (full.contains('\n') || full != s).then_some(full)
        }
        "apply_patch" => input.get("input").and_then(|v| v.as_str()).map(str::to_owned),
        "read" | "glob" | "grep" | "plan" => None,
        _ => serde_json::to_string_pretty(&input).ok(),
    };
    let plan = (kind == "plan").then(|| plan_items(&input));
    let started_open = plan.is_some();
    view! {
        <div class="rounded-md border border-zinc-200 text-sm dark:border-zinc-800">
        <details class="group" open=started_open>
            <summary class="flex cursor-pointer select-none items-center gap-2 px-3 py-1.5">
                {move || {
                    let (icon, cls) = match result.get() {
                        None => ("●", "animate-pulse text-blue-500"),
                        Some(r) if r.is_error => ("✕", "text-red-500"),
                        Some(_) => ("✓", "text-emerald-600"),
                    };
                    view! { <span class=format!("text-xs {cls}")>{icon}</span> }
                }}
                <span class="font-mono text-xs font-semibold">{name.clone()}</span>
                <span class="min-w-0 flex-1 truncate font-mono text-xs text-zinc-500">{summary}</span>
                {move || result.get().map(|r| view! { <span class="text-[11px] text-zinc-400">{format!("{:.1}s", r.duration_ms as f64 / 1000.0)}</span> })}
            </summary>
            <div class="space-y-2 border-t border-zinc-200 px-3 py-2 dark:border-zinc-800">
                {plan.map(|items| view! {
                    <ul class="space-y-1 text-sm">
                        {items.into_iter().map(|(text, status)| {
                            let (mark, cls) = match status.as_str() {
                                "completed" => ("☑", "text-zinc-400 line-through"),
                                "in_progress" => ("▶", "font-medium"),
                                _ => ("☐", ""),
                            };
                            view! { <li class=format!("flex gap-2 {cls}")><span>{mark}</span><span>{text}</span></li> }
                        }).collect_view()}
                    </ul>
                })}
                {detail_input.map(|d| view! { <pre class="max-h-64 overflow-auto whitespace-pre-wrap font-mono text-xs text-zinc-600 dark:text-zinc-400">{d}</pre> })}
                {move || result.get().map(|r| view! {
                    <pre class=format!("max-h-80 overflow-auto whitespace-pre-wrap rounded bg-zinc-100 p-2 font-mono text-xs dark:bg-zinc-900 {}", if r.is_error { "text-red-700 dark:text-red-300" } else { "" })>
                        {r.output}
                    </pre>
                    {r.truncated.then(|| view! { <p class="text-[11px] text-zinc-500">"Output was truncated for the model."</p> })}
                })}
            </div>
        </details>
        // Live output of a running tool, visible without expanding the card.
        {move || result.with(Option::is_none).then(|| live_output.get()).flatten().map(|o| view! {
            <pre class="mx-3 mb-2 max-h-48 overflow-auto whitespace-pre-wrap rounded bg-zinc-100 p-2 font-mono text-xs dark:bg-zinc-900">{o}</pre>
        })}
        </div>
    }
}

#[component]
fn Queued(q: QueuedInput, session_id: String) -> impl IntoView {
    let rejected = q.rejected.clone();
    view! {
        <div class="flex flex-col items-end gap-1">
            <div class=if rejected.is_some() { "max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-sm border border-red-300 px-4 py-2 text-sm opacity-70 dark:border-red-900" } else { "max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-sm border border-dashed border-zinc-300 px-4 py-2 text-sm text-zinc-500 dark:border-zinc-700" }>
                {q.text.clone()}
            </div>
            {match rejected {
                Some(reason) => view! {
                    <span class="text-xs text-red-600">
                        {format!("Not delivered: {reason}")} " "
                        <button class="underline" on:click=move |_| { let id = session_id.clone(); spawn_local(async move { let _ = api::dismiss_rejected(id).await; }); }>"Dismiss"</button>
                    </span>
                }.into_any(),
                None => view! { <span class="text-xs text-zinc-500">"queued"</span> }.into_any(),
            }}
        </div>
    }
}

#[component]
fn Composer(id: String, running: Signal<bool>, disabled: Signal<bool>, error: RwSignal<Option<String>>) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let id = StoredValue::new(id);
    let send = move || {
        let t = text.get_untracked();
        if t.trim().is_empty() || disabled.get_untracked() {
            return;
        }
        text.set(String::new());
        spawn_local(async move {
            if let Err(e) = api::send_message(id.get_value(), t.clone()).await {
                error.set(Some(e));
                text.set(t);
            } else {
                error.set(None);
            }
        });
    };
    let stop = move |_| {
        spawn_local(async move {
            if let Err(e) = api::interrupt(id.get_value()).await {
                error.set(Some(e));
            }
        });
    };
    view! {
        <div class="fixed inset-x-0 bottom-0 z-10 border-t border-zinc-200 bg-zinc-50/95 backdrop-blur dark:border-zinc-800 dark:bg-zinc-950/95">
            <div class="mx-auto max-w-4xl space-y-2 px-4 py-3">
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                <div class="flex items-end gap-2">
                    <textarea
                        class=format!("{INPUT} max-h-60 min-h-[2.75rem] resize-y")
                        rows="2"
                        placeholder=move || if disabled.get() { "This session failed to start." } else if running.get() { "Send a message; it reaches the agent at its next step…" } else { "Message the agent (Enter to send, Shift+Enter for a new line)" }
                        prop:disabled=move || disabled.get()
                        prop:value=move || text.get()
                        on:input=move |ev| text.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing() { ev.prevent_default(); send(); }
                        }
                    ></textarea>
                    {move || if running.get() {
                        view! { <button class="rounded-md border border-zinc-300 px-3 py-2 text-sm hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800" on:click=stop>"Stop"</button> }.into_any()
                    } else {
                        view! { <button class=format!("{BUTTON} py-2") prop:disabled=move || disabled.get() on:click=move |_| send()>"Send"</button> }.into_any()
                    }}
                </div>
            </div>
        </div>
    }
}
