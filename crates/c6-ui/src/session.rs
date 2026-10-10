//! A session: streaming transcript, queued input, composer.

use std::collections::HashMap;

use c6_api::{ContentBlock, Event, EventKind, QueuedInput, Session, SessionStatus, StreamMsg, TurnEndReason};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, StatusPill, storage_get, storage_set};
use crate::workspace::{Go, ScrollBox, Target, View, use_column};
use crate::{api, markdown, sessions};

/// Live-only state, replaced by durable events as they arrive.
#[derive(Clone, Default)]
pub(crate) struct Streaming {
    pub(crate) text: String,
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

/// Token usage summed over model calls.
#[derive(Clone, Copy, Default, PartialEq)]
struct Tokens {
    calls: u64,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
}

impl Tokens {
    fn add(&mut self, kind: &EventKind) {
        if let EventKind::Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, .. } = kind {
            self.calls += 1;
            self.input += input_tokens;
            self.cache_read += cache_read_tokens;
            self.cache_write += cache_write_tokens;
            self.output += output_tokens;
            self.reasoning += reasoning_tokens;
        }
    }

    /// All prompt tokens, cached or not.
    fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// "12.3k in (85% cached, 1.2k written) · 950 out (300 reasoning)"
    fn summary(&self) -> String {
        let mut cache = vec![format!("{}% cached", (self.cache_read * 100).checked_div(self.prompt()).unwrap_or(0))];
        if self.cache_write > 0 {
            cache.push(format!("{} written", tokens(self.cache_write)));
        }
        let reasoning = if self.reasoning > 0 { format!(" ({} reasoning)", tokens(self.reasoning)) } else { String::new() };
        format!("{} in ({}) · {} out{reasoning}", tokens(self.prompt()), cache.join(", "), tokens(self.output))
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
struct TurnStats {
    tokens: Tokens,
    started: i64,
    ended: Option<i64>,
    /// Context size at the turn's last model call.
    context: Option<u64>,
}

/// Per-turn usage plus the session's totals and current context size.
#[derive(Clone, Default, PartialEq)]
struct UsageStats {
    turns: HashMap<u64, TurnStats>,
    current_turn: Option<u64>,
    total: Tokens,
    /// Prompt plus output of the last model call since the last compaction:
    /// what the next request starts from.
    context: Option<u64>,
    context_window: u64,
}

impl UsageStats {
    fn from_events(evs: &[Event]) -> Self {
        let mut st = Self::default();
        for e in evs {
            match &e.kind {
                EventKind::Started { context_window, .. } => st.context_window = *context_window,
                EventKind::TurnStarted { turn } => {
                    st.current_turn = Some(*turn);
                    st.turns.insert(*turn, TurnStats { started: e.ts, ..Default::default() });
                }
                EventKind::TurnEnded { turn, .. } => {
                    if let Some(t) = st.turns.get_mut(turn) {
                        t.ended = Some(e.ts);
                    }
                    st.current_turn = None;
                }
                EventKind::Compaction { .. } => st.context = None,
                EventKind::ModelSwitched { context_window, .. } => st.context_window = *context_window,
                EventKind::Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, .. } => {
                    st.total.add(&e.kind);
                    let ctx = input_tokens + cache_read_tokens + cache_write_tokens + output_tokens;
                    st.context = Some(ctx);
                    if let Some(t) = st.current_turn.and_then(|n| st.turns.get_mut(&n)) {
                        t.tokens.add(&e.kind);
                        t.context = Some(ctx);
                    }
                }
                _ => {}
            }
        }
        st
    }
}

fn duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s >= 60 { format!("{}m {:02}s", s / 60, s % 60) } else { format!("{s}s") }
}

/// One line of a turn's numbers; `elapsed` for a turn still running.
fn turn_line(t: &TurnStats, now: Option<i64>) -> String {
    let mut parts = Vec::new();
    if let Some(end) = t.ended.or(now) {
        parts.push(duration(end - t.started));
    }
    if t.tokens.calls > 0 {
        parts.push(format!("{} model call{}", t.tokens.calls, if t.tokens.calls == 1 { "" } else { "s" }));
        parts.push(t.tokens.summary());
    }
    if let Some(c) = t.context {
        parts.push(format!("context {}", tokens(c)));
    }
    parts.join(" · ")
}

/// Live state of one session's log, fed by its stream.
#[derive(Clone, Copy)]
pub(crate) struct Feed {
    pub(crate) events: RwSignal<Vec<Event>>,
    pub(crate) streaming: RwSignal<Streaming>,
    queued: RwSignal<Vec<QueuedInput>>,
    activity: RwSignal<Option<bool>>,
}

impl Feed {
    /// Follows the session's stream until the calling scope is cleaned up.
    pub(crate) fn open(id: String) -> Self {
        let feed = Feed {
            events: RwSignal::new(Vec::new()),
            streaming: RwSignal::new(Streaming::default()),
            queued: RwSignal::new(Vec::new()),
            activity: RwSignal::new(None),
        };
        let Feed { events, streaming, queued, activity } = feed;
        let handle = api::stream_session(
            id,
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
        feed
    }

    /// A turn is in progress (live activity, or an unclosed turn in the log).
    fn running(&self) -> bool {
        self.activity.get().unwrap_or_else(|| {
            self.events.with(|evs| {
                evs.iter().rev().find_map(|e| match e.kind {
                    EventKind::TurnStarted { .. } => Some(true),
                    EventKind::TurnEnded { .. } => Some(false),
                    _ => None,
                }) == Some(true)
            })
        })
    }
}

/// A session in a workspace column. `target`: a place to reveal once it
/// loads (from search).
#[component]
pub fn SessionView(id: String, target: Option<Target>) -> impl IntoView {
    let live = api::live();
    let col = use_column();
    // Keyboard shortcuts only act in the active column.
    let active = move || col.is_none_or(|c| c.active.get_untracked());
    let root = col.map(|c| c.root_id()).unwrap_or_default();
    let detail = LocalResource::new({
        let id = id.clone();
        move || api::session(id.clone())
    });
    let feed = Feed::open(id.clone());
    let Feed { events, streaming, queued, activity } = feed;
    let error = RwSignal::new(None::<String>);

    let status = {
        let id = id.clone();
        Memo::new(move |_| {
            sessions::live_status(&id)
                .or_else(|| detail.get().and_then(|d| d.ok()).map(|d| d.session.status))
                .unwrap_or(SessionStatus::Starting)
        })
    };
    let running = move || status.get() == SessionStatus::Running || activity.get() == Some(true);

    // Follow the bottom of the transcript unless the user scrolled up, or
    // came for a particular place (from search).
    let target = target.unwrap_or_default();
    let scroller = NodeRef::<leptos::html::Div>::new();
    let stick = RwSignal::new(target.anchor.is_none());
    let force_open = ForceOpen(RwSignal::new(target.open.clone()));
    provide_context(force_open);
    // Find in session: open when arriving from search (or via Find).
    let find_open = RwSignal::new(!target.q.trim().is_empty());
    let find_q = target.q.clone();
    if let Some(anchor) = target.anchor.clone() {
        let terms = serde_json::to_string(&crate::search::terms(&target.q)).unwrap_or_else(|_| "[]".into());
        let root = root.clone();
        match anchor.strip_prefix('e').and_then(|n| n.parse::<u64>().ok()) {
            // `e<seq>`: an event of this session, once it has loaded.
            Some(seq) => {
                let session = id.clone();
                let done = RwSignal::new(false);
                Effect::new(move || {
                    if done.get_untracked() {
                        return;
                    }
                    // A tool result shows in its call's card.
                    let el = events.with(|evs| {
                        evs.iter().find(|e| e.seq == seq).map(|e| match &e.kind {
                            EventKind::ToolResult { call_id, .. } => format!("call-{call_id}"),
                            _ => format!("{session}-e{seq}"),
                        })
                    });
                    let Some(el) = el else { return };
                    done.set(true);
                    reveal_when_rendered(root.clone(), el, terms.clone(), 0);
                });
            }
            None => reveal_when_rendered(root, anchor, terms, 0),
        }
    }
    let on_scroll = Callback::new(move |()| {
        if let Some(el) = scroller.get_untracked() {
            stick.set(el.scroll_height() - el.scroll_top() - el.client_height() < 120);
        }
    });
    Effect::new(move || {
        events.track();
        streaming.track();
        queued.track();
        if stick.get_untracked() {
            request_animation_frame(move || {
                if let Some(el) = scroller.get_untracked() {
                    el.set_scroll_top(el.scroll_height());
                }
            });
        }
    });

    let usage = Memo::new(move |_| events.with(|evs| UsageStats::from_events(evs)));
    let choice = ModelChoice::follow(events, Default::default());
    let model_label = Memo::new(move |_| {
        let (_, m, e) = choice.current.get();
        model_label(&m, &e)
    });
    let id_stored = StoredValue::new(id.clone());
    let tab = RwSignal::new("transcript");
    // Esc stops a running turn (unless typing in a field, e.g. a side chat).
    let esc_id = id.clone();
    let _esc = window_event_listener(leptos::ev::keydown, move |ev| {
        let in_field = ev
            .target()
            .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            .is_some_and(|el| matches!(el.tag_name().as_str(), "TEXTAREA" | "INPUT"));
        if ev.key() == "Escape" && running() && !in_field && active() {
            let id = esc_id.clone();
            spawn_local(async move {
                let _ = api::interrupt(id).await;
            });
        }
    });
    // Where the session really runs (its worktree, if it has one).
    let started = Memo::new(move |_| {
        events.with(|evs| match evs.first().map(|e| &e.kind) {
            Some(EventKind::Started { cwd, worktree_branch, .. }) => Some((cwd.clone(), worktree_branch.clone())),
            _ => None,
        })
    });

    let find_root = root.clone();
    view! {
        <div class="relative flex min-h-0 flex-1 flex-col">
        {move || find_open.get().then(|| view! { <FindBar session=id_stored.get_value() initial=find_q.clone() open=find_open force_open root=find_root.clone() /> })}
        <ScrollBox node_ref=scroller on_scroll>
        <div class="mx-auto max-w-4xl px-4 pb-8 pt-4">
            <Suspense fallback=|| ()>
                {move || detail.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(d) => {
                        let s = d.session.clone();
                        let worker_id = s.worker_id;
                        let online = move || live.workers.with(|ws| ws.iter().any(|w| w.id == worker_id && w.status == c6_api::WorkerStatus::Online));
                        view! {
                            <div class="mb-4 flex flex-wrap items-start justify-between gap-3 border-b border-zinc-200 pb-3 dark:border-zinc-800">
                                <div class="min-w-0">
                                    <h1 class="truncate text-lg font-semibold">{s.title.clone()}</h1>
                                    <div class="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-zinc-500">
                                        <span class="flex items-center gap-1">
                                            <span class=move || if online() { "h-1.5 w-1.5 rounded-full bg-emerald-500" } else { "h-1.5 w-1.5 rounded-full bg-zinc-400" }></span>
                                            {d.worker_name.clone()}
                                        </span>
                                        <span class="font-mono">{move || started.get().map(|(c, _)| c).unwrap_or_else(|| s.cwd.clone())}</span>
                                        {move || started.get().map(|(_, b)| b).filter(|b| !b.is_empty()).map(|b| view! {
                                            <span class="rounded bg-zinc-100 px-1 font-mono dark:bg-zinc-800">{b}</span>
                                        })}
                                        <span>{move || { let m = model_label.get(); if m.is_empty() { s.model.clone() } else { m } }}</span>
                                    </div>
                                </div>
                                <div class="flex items-center gap-2">
                                    <crate::fleet::TabButton tab label="Transcript" value="transcript" />
                                    <crate::fleet::TabButton tab label="Changes" value="changes" />
                                    <crate::fleet::TabButton tab label="Files" value="files" />
                                    <crate::fleet::TabButton tab label="Terminal" value="terminal" />
                                    <button class=format!("{BUTTON_QUIET} text-xs text-zinc-500") title="Find in this session" on:click=move |_| {
                                        tab.set("transcript");
                                        find_open.set(true);
                                    }>"Find"</button>
                                    {move || view! { <StatusPill status=status.get() /> }}
                                    <button class=format!("{BUTTON_QUIET} text-xs text-zinc-500") title="Archive the session and close this column" on:click=move |_| {
                                        spawn_local(async move {
                                            if api::archive(id_stored.get_value()).await.is_ok() && let Some(c) = col { c.close(); }
                                        });
                                    }>"Archive"</button>
                                </div>
                            </div>
                            <SessionStats usage />
                            {s.error.clone().map(|e| view! { <ErrorBox message=format!("The session failed to start: {e}") /> })}
                            {move || (!online() && (status.get() == SessionStatus::Running || !queued.with(Vec::is_empty))).then(|| view! {
                                <p class="mb-3 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
                                    "The worker is offline. A running turn continues there and syncs back; queued messages are delivered when it reconnects."
                                </p>
                            })}
                            {s.parent_id.clone().map(|p| {
                                let what = if s.parent_call_id.is_some() { "Subagent session · started by a Task call in " } else { "Side chat · branched from " };
                                view! { <p class="mb-3 text-sm text-zinc-500">{what}<Go to=View::session(p) class="underline">"its parent session"</Go></p> }
                            })}
                        }.into_any()
                    }
                })}
            </Suspense>

            {move || {
                let wid = detail.get().and_then(|d| d.ok()).map(|d| d.session.worker_id.to_string());
                let cwd = started.get().map(|(c, _)| c);
                match (tab.get(), wid, cwd) {
                    ("changes", Some(w), Some(c)) => view! { <crate::files::Changes worker_id=w path=c /> }.into_any(),
                    ("files", Some(w), Some(c)) => view! { <crate::files::FileBrowser worker_id=w root=c /> }.into_any(),
                    ("terminal", Some(w), Some(c)) => view! { <crate::term::Terminals worker_id=w default_cwd=c /> }.into_any(),
                    ("transcript", _, _) => ().into_any(),
                    _ => view! { <p class="py-6 text-center text-sm text-zinc-500">"Available once the session has started."</p> }.into_any(),
                }
            }}
            <div class:hidden=move || tab.get() != "transcript">
                {move || detail.get().and_then(Result::ok).map(|d| view! { <Transcript session=d.session feed running=Signal::derive(running) /> })}
            </div>

        </div>
        </ScrollBox>
            <Show when=move || tab.get() == "transcript" && !detail.get().and_then(|d| d.ok()).is_some_and(|d| d.session.parent_call_id.is_some())>
            <Composer id=id_stored.get_value() running=Signal::derive(running) disabled=Signal::derive(move || status.get() == SessionStatus::Failed) error model=choice />
            </Show>
        </div>
    }
}

/// A session's transcript, with the side chats branching from its messages
/// shown under them (recursively).
#[component]
fn Transcript(session: Session, feed: Feed, running: Signal<bool>) -> impl IntoView {
    let Feed { events, streaming, queued, .. } = feed;
    let id = StoredValue::new(session.id.clone());
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
    let usage = Memo::new(move |_| events.with(|evs| UsageStats::from_events(evs)));
    // Ticks while a turn runs, for its elapsed time.
    let now = RwSignal::new(js_sys::Date::now() as i64);
    let ticker = set_interval_with_handle(move || now.set(js_sys::Date::now() as i64), std::time::Duration::from_secs(1)).ok();
    on_cleanup(move || {
        if let Some(t) = ticker {
            t.clear();
        }
    });

    // Child sessions: subagents (linked from their Task calls) and side
    // chats. Refreshed as tool results arrive and after a side chat starts.
    let refresh = RwSignal::new(0u32);
    let children = LocalResource::new(move || {
        results.track();
        refresh.track();
        api::children(id.get_value())
    });
    let child_list = Memo::new(move |_| children.get().and_then(Result::ok).unwrap_or_default());
    let child_of_call =
        Memo::new(move |_| child_list.get().into_iter().filter_map(|c| Some((c.parent_call_id?, c.id))).collect::<HashMap<_, _>>());
    provide_context(ChildLinks(child_of_call));

    let can_branch = session.parent_call_id.is_none();
    // A side chat's setup (prompt, tools) is the parent's: not worth a line.
    let side = session.fork_seq.is_some();
    let asking = RwSignal::new(None::<u64>);
    // A side chat just started here opens expanded.
    let opened = RwSignal::new(None::<String>);
    let defaults = StoredValue::new((session.connection_id.clone(), session.model.clone(), session.effort.clone().unwrap_or_default()));
    let on_done = Callback::new(move |s: Option<Session>| {
        asking.set(None);
        if let Some(s) = s {
            opened.set(Some(s.id));
            refresh.update(|n| *n += 1);
        }
    });

    view! {
        <div class="space-y-4">
            // Events are append-only: keyed by seq, each renders once, so
            // expanded cards stay open while the turn continues.
            <For each=move || events.get() key=|e| e.seq let:e>
                {
                    let seq = e.seq;
                    let anchorable = can_branch && matches!(e.kind, EventKind::User { .. } | EventKind::Assistant { .. });
                    view! {
                        <div id=format!("{}-e{seq}", id.get_value()) class=if anchorable { "group relative pb-7" } else { "group relative" }>
                            {(!(side && matches!(e.kind, EventKind::Started { .. }))).then(|| render_event(&e, results, streaming, usage))}
                            {anchorable.then(|| view! {
                                <button
                                    class="absolute bottom-0 right-0 rounded-md border border-zinc-200 bg-white px-1.5 py-0.5 text-[11px] text-zinc-500 opacity-0 shadow-sm transition group-hover:opacity-100 hover:text-zinc-900 dark:border-zinc-700 dark:bg-zinc-900 dark:hover:text-zinc-100"
                                    title="Ask about this in a side chat"
                                    on:click=move |_| asking.set(Some(seq))
                                >"↳ Ask"</button>
                            })}
                        </div>
                        <For
                            each=move || child_list.with(|cs| cs.iter().filter(|c| c.fork_seq == Some(seq)).cloned().collect::<Vec<_>>())
                            key=|c| c.id.clone()
                            let:c
                        >
                            {let open = opened.get_untracked().as_deref() == Some(c.id.as_str()); side_chat(c, open)}
                        </For>
                        {move || (asking.get() == Some(seq)).then(|| view! {
                            <NewSideChat parent=id.get_value() anchor_seq=seq defaults=defaults.get_value() on_done />
                        })}
                    }
                }
            </For>
            {move || streaming.with(|s| (!s.thinking.is_empty() || !s.text.is_empty()).then(|| view! {
                <div class="space-y-2">
                    {(!s.thinking.is_empty()).then(|| view! { <Thinking text=s.thinking.clone() open=true /> })}
                    {(!s.text.is_empty()).then(|| view! { <div class="md" inner_html=markdown::to_html(&s.text)></div> })}
                </div>
            }))}
            {move || (running.get() && streaming.with(|s| s.text.is_empty() && s.thinking.is_empty())).then(|| view! {
                <div class="flex items-center gap-2 text-sm text-zinc-500">
                    <span class="h-2 w-2 animate-pulse rounded-full bg-blue-500"></span>"Working…"
                </div>
            })}
            {move || running.get().then(|| usage.with(|u| u.current_turn.and_then(|n| u.turns.get(&n).copied()))).flatten().map(|t| view! {
                <p class="font-mono text-[11px] text-zinc-500">{move || format!("This turn: {}", turn_line(&t, Some(now.get())))}</p>
            })}
            {move || queued.get().into_iter().map(|q| view! { <Queued q session_id=id.get_value() /> }).collect_view()}
        </div>
    }
}

/// A collapsible side-chat thread. Returns `AnyView` to end the type
/// recursion (side chats contain transcripts that contain side chats).
/// Side chats a deep link expands (from `?open=`).
#[derive(Clone, Copy)]
struct ForceOpen(RwSignal<Vec<String>>);

fn side_chat(s: Session, open_now: bool) -> AnyView {
    let key = format!("c6.side.{}", s.id);
    // Opened for a deep link: open now, without changing the saved state.
    let force = use_context::<ForceOpen>();
    let forced = force.is_some_and(|f| f.0.with_untracked(|ids| ids.contains(&s.id)));
    let open = RwSignal::new(open_now || forced || storage_get(&key).is_some_and(|v| v == "1"));
    // Also when stepping through matches later.
    if let Some(f) = force {
        let id = s.id.clone();
        Effect::new(move || {
            if f.0.with(|ids| ids.contains(&id)) {
                open.set(true);
            }
        });
    }
    if open_now {
        storage_set(&key, "1");
    }
    let toggle = move |_| {
        open.update(|o| *o = !*o);
        storage_set(&key, if open.get_untracked() { "1" } else { "0" });
    };
    // The row's model is current as of loading the page; an open side chat
    // keeps it current from its log (model switches).
    let (title, model) = (s.title.clone(), RwSignal::new(model_label(&s.model, s.effort.as_deref().unwrap_or_default())));
    let s = StoredValue::new(s);
    view! {
        <div class="ml-2 border-l-2 border-sky-300 pl-3 dark:border-sky-800">
            <button class="flex w-full items-center gap-2 py-1 text-left text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-200" on:click=toggle>
                <span class="inline-block w-3 shrink-0 text-[9px] transition-transform" class:rotate-90=move || open.get()>"▶"</span>
                <span class="shrink-0 font-medium text-sky-700 dark:text-sky-400">"Side chat"</span>
                <span class="truncate text-zinc-700 dark:text-zinc-300">{title}</span>
                <span class="ml-auto shrink-0 font-mono">{move || model.get()}</span>
            </button>
            <Show when=move || open.get()>
                <SideChatBody session=s.get_value() model_label=model />
            </Show>
        </div>
    }
    .into_any()
}

#[component]
fn SideChatBody(session: Session, model_label: RwSignal<String>) -> impl IntoView {
    let feed = Feed::open(session.id.clone());
    let running = Signal::derive(move || feed.running());
    let error = RwSignal::new(None::<String>);
    let id = session.id.clone();
    let start = (session.connection_id.clone(), session.model.clone(), session.effort.clone().unwrap_or_default());
    let choice = ModelChoice::follow(feed.events, start);
    Effect::new(move || {
        let (_, m, e) = choice.current.get();
        model_label.set(self::model_label(&m, &e));
    });
    view! {
        <div class="space-y-3 pb-2 pt-1">
            <Transcript session feed running />
            <Composer id running disabled=Signal::derive(|| false) error inline=true model=choice />
        </div>
    }
}

/// The model picker of a side chat's reply box: sending with a different
/// choice than `current` switches the side chat to it.
#[derive(Clone, Copy)]
struct ModelChoice {
    connection: RwSignal<String>,
    model: RwSignal<String>,
    /// Empty for the model's default.
    effort: RwSignal<String>,
    /// (connection, model, effort) the session runs on now.
    current: Memo<(String, String, String)>,
}

impl ModelChoice {
    /// A picker that follows the session's model: its last switch, else the
    /// one it started with (`start` until the log has loaded). It resets
    /// to the current model whenever that changes.
    fn follow(events: RwSignal<Vec<Event>>, start: (String, String, String)) -> Self {
        let current = Memo::new(move |_| {
            events.with(|evs| {
                evs.iter()
                    .rev()
                    .find_map(|e| match &e.kind {
                        EventKind::ModelSwitched { connection_id, model, effort, .. }
                        | EventKind::Started { connection_id, model, effort, .. } => {
                            Some((connection_id.clone(), model.clone(), effort.clone()))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| start.clone())
            })
        });
        let choice = ModelChoice {
            connection: RwSignal::new(String::new()),
            model: RwSignal::new(String::new()),
            effort: RwSignal::new(String::new()),
            current,
        };
        Effect::new(move || {
            let (c, m, e) = current.get();
            choice.connection.set(c);
            choice.model.set(m);
            choice.effort.set(e);
        });
        choice
    }

    /// The (connection, model, effort) to switch to, if the choice differs.
    fn switch(&self) -> Option<(String, String, String)> {
        let chosen = (self.connection.get_untracked(), self.model.get_untracked().trim().to_owned(), self.effort.get_untracked());
        (!chosen.1.is_empty() && chosen != self.current.get_untracked()).then_some(chosen)
    }
}

/// Connection select plus a model field that suggests the connection's
/// models and also takes free text.
#[component]
fn ModelPicker(connection: RwSignal<String>, model: RwSignal<String>, effort: RwSignal<String>) -> impl IntoView {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let list_id = format!("c6-models-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let connections = LocalResource::new(api::connections);
    let models = LocalResource::new(move || api::models(connection.get()));
    // Effort only where the connection passes it on to the model.
    let supports_effort =
        move |id: &str| connections.get().and_then(Result::ok).is_some_and(|cs| cs.iter().any(|c| c.id == id && c.supports_effort));
    view! {
        <select class=format!("{SMALL_INPUT} max-w-48") on:change=move |ev| {
            let c = event_target_value(&ev);
            let default = connections.get().and_then(Result::ok).and_then(|cs| cs.into_iter().find(|x| x.id == c)).and_then(|x| x.default_model);
            if !supports_effort(&c) {
                effort.set(String::new());
            }
            connection.set(c);
            model.set(default.unwrap_or_default());
        }>
            {move || connections.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|c| {
                let id = c.id.clone();
                view! { <option value=c.id.clone() selected=move || connection.get() == id>{c.name.clone()}</option> }
            }).collect_view()}
        </select>
        <input class=format!("{SMALL_INPUT} w-56 font-mono") list=list_id.clone() placeholder="model id"
            prop:value=move || model.get() on:input=move |ev| model.set(event_target_value(&ev)) />
        <datalist id=list_id>
            {move || models.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|m| view! { <option value=m></option> }).collect_view()}
        </datalist>
        <select class=SMALL_INPUT class:hidden=move || !supports_effort(&connection.get()) title="Reasoning effort" on:change=move |ev| effort.set(event_target_value(&ev))>
            {EFFORTS.iter().map(|&(value, label)| view! {
                <option value=value selected=move || effort.get() == value>{label}</option>
            }).collect_view()}
        </select>
    }
}

/// Reasoning efforts; "" is the model's default.
const EFFORTS: [(&str, &str); 6] =
    [("", "default effort"), ("low", "low"), ("medium", "medium"), ("high", "high"), ("xhigh", "xhigh"), ("max", "max")];

const SMALL_INPUT: &str = "rounded-md border border-zinc-300 bg-white px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-900";

/// The question box that starts a side chat after `anchor_seq`.
#[component]
fn NewSideChat(parent: String, anchor_seq: u64, defaults: (String, String, String), on_done: Callback<Option<Session>>) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let (connection, model, effort) = (RwSignal::new(defaults.0), RwSignal::new(defaults.1), RwSignal::new(defaults.2));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let parent = StoredValue::new(parent);
    let send = move || {
        let prompt = text.get_untracked();
        if prompt.trim().is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        let req = c6_api::CreateSideChat {
            anchor_seq,
            connection_id: connection.get_untracked(),
            model: model.get_untracked(),
            effort: Some(effort.get_untracked()).filter(|e| !e.is_empty()),
            prompt,
        };
        spawn_local(async move {
            match api::create_side_chat(parent.get_value(), req).await {
                Ok(s) => on_done.run(Some(s)),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };
    let input = NodeRef::<leptos::html::Textarea>::new();
    Effect::new(move || {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });
    view! {
        <div class="ml-2 space-y-2 rounded-lg border border-sky-200 bg-sky-50/60 p-3 dark:border-sky-900 dark:bg-sky-950/30">
            <textarea
                node_ref=input
                class=format!("{INPUT} min-h-[2.75rem] resize-y")
                rows="2"
                placeholder="Ask about this in a side chat (Enter to send, Esc to cancel)"
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing() { ev.prevent_default(); send(); }
                    if ev.key() == "Escape" { on_done.run(None); }
                }
            ></textarea>
            <div class="flex flex-wrap items-center gap-2">
                <ModelPicker connection model effort />
                <span class="flex-1"></span>
                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| on_done.run(None)>"Cancel"</button>
                <button class=format!("{BUTTON} text-xs") prop:disabled=move || busy.get() on:click=move |_| send()>"Ask"</button>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        </div>
    }
}

/// "model · effort", or just the model when it runs at its default effort.
fn model_label(model: &str, effort: &str) -> String {
    if effort.is_empty() { model.to_owned() } else { format!("{model} · {effort}") }
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = c6Search, js_name = reveal)]
    fn js_reveal(root: &str, id: &str, terms_json: &str) -> bool;

    #[wasm_bindgen(js_namespace = c6Search, js_name = clear)]
    fn js_clear_highlights(root: &str);

    #[wasm_bindgen(js_namespace = c6Search, js_name = highlightAll)]
    fn js_highlight_all(root: &str, terms_json: &str);
}

/// Find in session: steps through the matches of a query in this session
/// and its side chats, expanding side chats and blocks on the way.
#[component]
fn FindBar(session: String, initial: String, open: RwSignal<bool>, force_open: ForceOpen, root: String) -> impl IntoView {
    let col = use_column();
    let root = StoredValue::new(root);
    let text = RwSignal::new(initial.clone());
    let applied = RwSignal::new(initial);
    let current = RwSignal::new(None::<usize>);
    let error = RwSignal::new(None::<String>);
    let session = StoredValue::new(session);
    let matches = LocalResource::new(move || {
        let q = applied.get();
        async move { if q.trim().is_empty() { Ok(None) } else { api::session_matches(session.get_value(), q).await.map(Some) } }
    });
    let found = Memo::new(move |_| matches.get().and_then(|r| r.ok()).flatten());
    let go = move |i: usize| {
        let Some(m) = found.get_untracked() else { return };
        let Some(hit) = m.matches.get(i).cloned() else { return };
        current.set(Some(i));
        force_open.0.update(|ids| ids.extend(hit.open.iter().filter(|o| !ids.contains(o)).cloned().collect::<Vec<_>>()));
        reveal_when_rendered(root.get_value(), hit.anchor, serde_json::to_string(&m.terms).unwrap_or_else(|_| "[]".into()), 0);
    };
    let step = move |delta: isize| {
        let n = found.with_untracked(|m| m.as_ref().map_or(0, |m| m.matches.len()));
        if n == 0 {
            return;
        }
        let next = match current.get_untracked() {
            Some(i) => (i as isize + delta).rem_euclid(n as isize) as usize,
            None if delta < 0 => n - 1,
            None => 0,
        };
        go(next);
    };
    // When results arrive: start at the deep-linked match if there is one
    // (the page already revealed it), else at the first.
    Effect::new(move || {
        let Some(r) = matches.get() else { return };
        match r {
            Err(e) => error.set(Some(e)),
            Ok(None) => {
                js_clear_highlights(&root.get_value());
                current.set(None);
            }
            Ok(Some(m)) => {
                error.set(None);
                js_highlight_all(&root.get_value(), &serde_json::to_string(&m.terms).unwrap_or_else(|_| "[]".into()));
                let hash = window().location().hash().unwrap_or_default();
                match m.matches.iter().position(|x| format!("#{}", x.anchor) == hash) {
                    Some(i) if current.get_untracked().is_none() => current.set(Some(i)),
                    _ if !m.matches.is_empty() => go(0),
                    _ => current.set(None),
                }
            }
        }
    });
    let close = move || {
        js_clear_highlights(&root.get_value());
        open.set(false);
    };
    // Leaving the page (or closing the bar) stops highlighting.
    on_cleanup(move || js_clear_highlights(&root.get_value()));
    let _keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key().eq_ignore_ascii_case("g") && (ev.meta_key() || ev.ctrl_key()) && col.is_none_or(|c| c.active.get_untracked()) {
            ev.prevent_default();
            step(if ev.shift_key() { -1 } else { 1 });
        }
    });
    let input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move || {
        if let Some(el) = input.get()
            && applied.get_untracked().is_empty()
        {
            let _ = el.focus();
        }
    });
    view! {
        <div data-c6-nohl class="absolute right-3 top-2 z-30 flex items-center gap-1 rounded-lg border border-zinc-200 bg-white/95 p-1.5 text-sm shadow-md backdrop-blur dark:border-zinc-700 dark:bg-zinc-900/95">
            <input
                node_ref=input
                type="search"
                class="w-48 rounded-md border border-zinc-300 bg-white px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-900"
                placeholder="Find in this session"
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| match ev.key().as_str() {
                    "Enter" if text.get_untracked() != applied.get_untracked() => {
                        current.set(None);
                        applied.set(text.get_untracked());
                    }
                    "Enter" => step(if ev.shift_key() { -1 } else { 1 }),
                    "Escape" => close(),
                    _ => {}
                }
            />
            <span class="min-w-16 px-1 text-center text-xs tabular-nums text-zinc-500" title=move || error.get().unwrap_or_default()>
                {move || match (error.get(), found.get()) {
                    (Some(_), _) => "—".to_owned(),
                    (None, Some(m)) if m.matches.is_empty() => "no matches".to_owned(),
                    (None, Some(m)) => format!("{} of {}{}", current.get().map_or(0, |i| i + 1), m.matches.len(), if m.truncated { "+" } else { "" }),
                    (None, None) => String::new(),
                }}
            </span>
            <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs") title="Previous match (Shift+Enter, ⇧⌘G)" on:click=move |_| step(-1)>"↑"</button>
            <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs") title="Next match (Enter, ⌘G)" on:click=move |_| step(1)>"↓"</button>
            <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs text-zinc-500") title="Close (Esc)" on:click=move |_| close()>"✕"</button>
        </div>
    }
}

/// Reveals a search hit once its element exists (events render in batches).
fn reveal_when_rendered(root: String, id: String, terms: String, tries: u32) {
    set_timeout(
        move || {
            // Nested side chats load their events one after another.
            if !js_reveal(&root, &id, &terms) && tries < 200 {
                reveal_when_rendered(root, id, terms, tries + 1);
            }
        },
        std::time::Duration::from_millis(50),
    );
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

#[component]
fn SessionStats(usage: Memo<UsageStats>) -> impl IntoView {
    move || {
        usage.with(|u| {
            (u.total.calls > 0).then(|| {
                let context = u.context.map(|c| {
                    let pct = (c * 100).checked_div(u.context_window);
                    let bar = match pct {
                        Some(p) if p >= 85 => "bg-red-500",
                        Some(p) if p >= 60 => "bg-amber-500",
                        _ => "bg-blue-500",
                    };
                    let label = match pct {
                        Some(p) => format!("{} / {} ({p}%)", tokens(c), tokens(u.context_window)),
                        None => tokens(c),
                    };
                    let width = format!("width: {}%", pct.unwrap_or(0).min(100));
                    view! {
                        <span class="flex items-center gap-1.5" title="Tokens the next model request starts from. Compaction runs at 85% of the window.">
                            "Context"
                            <span class="inline-block h-1.5 w-20 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-800">
                                <span class=format!("block h-full {bar}") style=width></span>
                            </span>
                            <span class="text-zinc-700 dark:text-zinc-300">{label}</span>
                        </span>
                    }
                });
                let turns = u.turns.len();
                let calls = u.total.calls;
                view! {
                    <div class="mb-4 flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-[11px] text-zinc-500">
                        {context}
                        <span title="Totals for the session">
                            {format!("Session: {} · {turns} turn{} · {calls} model call{}", u.total.summary(), if turns == 1 { "" } else { "s" }, if calls == 1 { "" } else { "s" })}
                        </span>
                    </div>
                }
            })
        })
    }
}

fn render_event(
    e: &Event,
    results: Memo<HashMap<String, ToolResultView>>,
    streaming: RwSignal<Streaming>,
    usage: Memo<UsageStats>,
) -> AnyView {
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
                        ContentBlock::ServerTool { label } => view! {
                            <p class="flex items-center gap-2 text-xs text-zinc-500"><span>"⌕"</span><span class="font-mono">{label.clone()}</span></p>
                        }.into_any(),
                        ContentBlock::Text { text } => view! { <div class="md" inner_html=markdown::to_html(text)></div> }.into_any(),
                        ContentBlock::Thinking { text } => view! { <Thinking text=text.clone() open=false /> }.into_any(),
                        ContentBlock::ToolUse { id, name, input_json } => {
                            let (out_id, res_id) = (id.clone(), id.clone());
                            let live_output = Signal::derive(move || streaming.with(|s| s.tool_output.get(&out_id).cloned()));
                            let result = Signal::derive(move || results.with(|r| r.get(&res_id).cloned()));
                            view! { <ToolCard name=name.clone() input_json=input_json.clone() call_id=id.clone() result live_output /> }.into_any()
                        }
                    }).collect_view()}
                    {interrupted.then(|| view! { <p class="text-xs italic text-zinc-500">"(response cut off)"</p> })}
                </div>
            }
            .into_any()
        }
        EventKind::Error { message } => view! { <ErrorBox message=message.clone() /> }.into_any(),
        EventKind::ModelSwitched { model, .. } => view! {
            <p class="flex items-center gap-2 text-xs text-zinc-500">
                <span class="h-px flex-1 bg-zinc-200 dark:bg-zinc-800"></span>
                {format!("Switched to {model}")}
                <span class="h-px flex-1 bg-zinc-200 dark:bg-zinc-800"></span>
            </p>
        }
        .into_any(),
        EventKind::Compaction { summary, tokens_before } => view! {
            <details class="rounded-md border border-dashed border-zinc-300 px-3 py-2 text-xs text-zinc-500 dark:border-zinc-700">
                <summary class="cursor-pointer select-none">
                    {format!("Context compacted ({} tokens summarized); the model continues from this summary", tokens(*tokens_before))}
                </summary>
                <div class="md mt-2 text-zinc-700 dark:text-zinc-300" inner_html=markdown::to_html(summary)></div>
            </details>
        }
        .into_any(),
        EventKind::TurnEnded { reason, turn } => {
            let turn = *turn;
            let note = match reason {
                TurnEndReason::Interrupted => Some("Interrupted"),
                TurnEndReason::WorkerRestarted => Some("Stopped: the worker restarted during this turn"),
                _ => None,
            };
            view! {
                {note.map(|n| view! { <p class="text-center text-xs text-zinc-500">{n}</p> })}
                {move || usage.with(|u| u.turns.get(&turn).copied()).map(|t| view! {
                    <p class="border-t border-zinc-200 pt-1 font-mono text-[11px] text-zinc-400 dark:border-zinc-800">
                        {format!("Turn {turn} · {}", turn_line(&t, None))}
                    </p>
                })}
            }
            .into_any()
        }
        _ => ().into_any(),
    }
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

#[derive(Clone, Copy)]
struct ChildLinks(Memo<HashMap<String, String>>);

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
        "Task" => "task",
        "WebFetch" => "web_fetch",
        "BashOutput" => "bash_output",
        "KillShell" => "kill_shell",
        "Skill" => "load_skill",
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
        "task" => s("description"),
        "web_fetch" => s("url"),
        "bash_output" | "kill_shell" => input
            .get("id")
            .or_else(|| input.get("bash_id"))
            .or_else(|| input.get("shell_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned(),
        "load_skill" => input.get("skill").or_else(|| input.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_owned(),
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
    call_id: String,
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
        "task" => input.get("prompt").and_then(|v| v.as_str()).map(str::to_owned),
        "read" | "glob" | "grep" | "plan" | "web_fetch" | "bash_output" | "kill_shell" | "load_skill" => None,
        _ => serde_json::to_string_pretty(&input).ok(),
    };
    let plan = (kind == "plan").then(|| plan_items(&input));
    let child = (kind == "task").then(use_context::<ChildLinks>).flatten();
    let child_call = call_id.clone();
    // Where a search hit in this call's output scrolls to.
    let dom_id = format!("call-{call_id}");
    let started_open = plan.is_some();
    view! {
        <div id=dom_id class="rounded-md border border-zinc-200 text-sm dark:border-zinc-800">
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
                {move || child.and_then(|c| c.0.with(|m| m.get(&child_call).cloned())).map(|cid| view! {
                    <Go to=View::session(cid) class="text-[11px] text-blue-600 hover:underline dark:text-blue-400" stop=true>"open subagent"</Go>
                })}
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
fn Composer(
    id: String,
    running: Signal<bool>,
    disabled: Signal<bool>,
    error: RwSignal<Option<String>>,
    /// Inside a side chat instead of fixed at the bottom of the page.
    #[prop(optional)]
    inline: bool,
    /// Offer a model picker; sending with another model switches to it.
    #[prop(optional)]
    model: Option<ModelChoice>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let id = StoredValue::new(id);
    let send = move || {
        let t = text.get_untracked();
        if t.trim().is_empty() || disabled.get_untracked() {
            return;
        }
        let switch = model.and_then(|m| m.switch());
        text.set(String::new());
        spawn_local(async move {
            if let Err(e) = api::send_message(id.get_value(), t.clone(), switch).await {
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
        <div class=if inline { "" } else { "shrink-0 border-t border-zinc-200 bg-zinc-50 dark:border-zinc-800 dark:bg-zinc-950" }>
            <div class=if inline { "space-y-2" } else { "mx-auto max-w-4xl space-y-2 px-4 py-3" }>
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                <div class="flex items-end gap-2">
                    <textarea
                        class=format!("{INPUT} max-h-60 min-h-[2.75rem] resize-y")
                        rows="2"
                        placeholder=move || if disabled.get() { "This session failed to start." } else if running.get() { "Send a message; it reaches the agent at its next step…" } else if inline { "Reply in this side chat" } else { "Message the agent (Enter to send, Shift+Enter for a new line)" }
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
                {model.map(|m| view! {
                    <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
                        <span>"Model"</span>
                        <ModelPicker connection=m.connection model=m.model effort=m.effort />
                        {move || {
                            let _ = (m.connection.get(), m.model.get(), m.effort.get());
                            m.switch().map(|_| view! { <span class="text-amber-600 dark:text-amber-400">"switches on send"</span> })
                        }}
                    </div>
                })}
            </div>
        </div>
    }
}
