//! A session: streaming transcript, queued input, composer.

use std::collections::HashMap;

use c6_api::{ContentBlock, Event, EventKind, QueuedInput, Session, SessionStatus, StreamMsg, TurnEndReason, WorkerStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::details::GLANCE_WIDTH;
use crate::models::ModelInput;
use crate::panes::Pane;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, now_ms, storage_get, storage_set};
use crate::workspace::{ColumnCtx, Go, Popover, ScrollBox, Target, View, below, use_column};
use crate::{api, markdown, sessions};

/// The small bordered action buttons on a message row (Edit, Rewind, Ask).
const UBTN: &str = "rounded-md border border-zinc-200 bg-white px-1.5 py-0.5 text-[11px] text-muted shadow-sm hover:text-zinc-900 dark:border-zinc-700 dark:bg-zinc-900 dark:hover:text-zinc-100";

/// Live-only state, replaced by durable events as they arrive.
#[derive(Clone, Default)]
pub(crate) struct Streaming {
    pub(crate) text: String,
    thinking: String,
    tool_output: HashMap<String, String>,
    /// Waiting to try a failed model call again, and why.
    status: String,
}

#[derive(Clone, PartialEq)]
struct ToolResultView {
    output: String,
    is_error: bool,
    truncated: bool,
    duration_ms: u64,
    /// The images the tool showed the model.
    images: Vec<String>,
}

/// Token usage summed over model calls.
#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) struct Tokens {
    pub(crate) calls: u64,
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
    pub(crate) fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// "12.3k in (85% cached, 1.2k written) · 950 out (300 reasoning)"
    pub(crate) fn summary(&self) -> String {
        let mut cache = vec![format!("{}% cached", (self.cache_read * 100).checked_div(self.prompt()).unwrap_or(0))];
        if self.cache_write > 0 {
            cache.push(format!("{} written", tokens(self.cache_write)));
        }
        let reasoning = if self.reasoning > 0 { format!(" ({} reasoning)", tokens(self.reasoning)) } else { String::new() };
        format!("{} in ({}) · {} out{reasoning}", tokens(self.prompt()), cache.join(", "), tokens(self.output))
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) struct TurnStats {
    tokens: Tokens,
    started: i64,
    ended: Option<i64>,
    /// Context size at the turn's last model call.
    context: Option<u64>,
}

/// Per-turn usage plus the session's totals and current context size.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct UsageStats {
    pub(crate) turns: HashMap<u64, TurnStats>,
    current_turn: Option<u64>,
    pub(crate) total: Tokens,
    /// Prompt plus output of the last model call since the last compaction:
    /// what the next request starts from.
    pub(crate) context: Option<u64>,
    pub(crate) context_window: u64,
}

impl UsageStats {
    fn from_events(evs: &[Event]) -> Self {
        let mut st = Self::default();
        for e in evs {
            match &e.kind {
                EventKind::Started { context_window, .. } => st.context_window = *context_window,
                EventKind::TurnStarted { turn, context_window } => {
                    if *context_window > 0 {
                        st.context_window = *context_window;
                    }
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
                EventKind::Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, context_window, .. } => {
                    // The model that made the call says its window (Claude Code's
                    // aliases don't).
                    if *context_window > 0 {
                        st.context_window = *context_window;
                    }
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

/// One message in the queue-edit box: a stable id (for reordering and
/// deleting it) and its text.
#[derive(Clone, PartialEq)]
pub(crate) struct QueueRow {
    id: u64,
    text: String,
}

/// Live state of one session's log, fed by its stream.
#[derive(Clone, Copy)]
pub(crate) struct Feed {
    pub(crate) events: RwSignal<Vec<Event>>,
    pub(crate) streaming: RwSignal<Streaming>,
    queued: RwSignal<Vec<QueuedInput>>,
    /// The queue-edit box is open (its messages are in `edit_rows`).
    pub(crate) edit_open: RwSignal<bool>,
    /// The messages pulled out of the queue, to be re-sent edited,
    /// reordered or dropped (in send order), while the box is open.
    pub(crate) edit_rows: RwSignal<Vec<QueueRow>>,
    activity: RwSignal<Option<bool>>,
    /// Secrets the assistant's operations returned, for you (shown while
    /// this page is open; never logged).
    secrets: RwSignal<Vec<SecretCard>>,
    /// The log has been replayed up to where the session was when opened.
    pub(crate) synced: RwSignal<bool>,
    /// The first event shown: before it, `events` has only the summary's
    /// (`EventKind::summary`). 1 (or 0): all of it.
    pub(crate) from: RwSignal<u64>,
    /// The first event not drawn, after a jump back (to a search hit):
    /// later ones as you scroll down. None: to the end.
    pub(crate) to: RwSignal<Option<u64>>,
    loading: RwSignal<bool>,
    id: StoredValue<String>,
}

/// How much of a log to open with.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Window {
    /// About the last [`TAIL`] events; earlier ones as you scroll up.
    Tail,
    /// The tail, and back to this event.
    At(u64),
    /// All of it.
    All,
}

/// How far the log has come, for streamed text that arrives after the
/// message it streamed (catching up, the stream sends what the log has
/// before what was live meanwhile): drawn again, it doubles the reply.
#[derive(Clone, Default)]
struct Logged {
    /// The last turn the log says ended.
    ended: u64,
    /// The last reply, or start or end of a turn: what streamed before it
    /// is in it (or was dropped).
    settled: u64,
    /// The calls whose results are in: their output is.
    results: std::collections::HashSet<String>,
}

impl Logged {
    /// `e` is in the log, replayed or live.
    fn note(&mut self, e: &Event) {
        match e.kind {
            EventKind::TurnEnded { turn, .. } => {
                self.ended = self.ended.max(turn);
                self.settled = self.settled.max(e.seq);
            }
            EventKind::Assistant { .. } | EventKind::TurnStarted { .. } => self.settled = self.settled.max(e.seq),
            EventKind::ToolResult { ref call_id, .. } => {
                self.results.insert(call_id.clone());
            }
            _ => {}
        }
    }

    /// A live event: noted, and what streamed for it leaves `s`.
    fn event(&mut self, s: &mut Streaming, e: &Event) {
        self.note(e);
        match &e.kind {
            EventKind::Assistant { .. } => {
                s.text.clear();
                s.thinking.clear();
            }
            EventKind::ToolResult { call_id, .. } => {
                s.tool_output.remove(call_id);
            }
            // A new turn shows only what it streams.
            EventKind::TurnEnded { .. } | EventKind::TurnStarted { .. } => *s = Streaming::default(),
            _ => {}
        }
    }

    /// Text streamed in `turn` when the log's next event was `seq` is
    /// already in a message: its turn ended, or a reply (or turn's start or
    /// end) from `seq` on is in. 0: not said.
    fn has(&self, turn: u64, seq: u64) -> bool {
        (turn != 0 && turn <= self.ended) || (seq != 0 && seq <= self.settled)
    }

    /// Streamed text, thinking or tool output: added to `s` unless the log
    /// has it. Whether it was.
    fn stream(&self, s: &mut Streaming, msg: StreamMsg) -> bool {
        match msg {
            StreamMsg::Text { turn, text, seq } if !self.has(turn, seq) => s.text.push_str(&text),
            StreamMsg::Thinking { turn, text, seq } if !self.has(turn, seq) => s.thinking.push_str(&text),
            StreamMsg::ToolOutput { call_id, text } if !self.results.contains(&call_id) => {
                s.tool_output.entry(call_id).or_default().push_str(&text)
            }
            _ => return false,
        }
        true
    }
}

/// Events in a window, and loaded at a time before it.
const TAIL: u64 = 300;

#[derive(Clone, Debug, PartialEq)]
struct SecretCard {
    call_id: String,
    title: String,
    value: String,
    note: String,
}

impl Feed {
    /// Follows the session's stream until the calling scope is cleaned up.
    pub(crate) fn open(id: String, window: Window) -> Self {
        let feed = Feed {
            events: RwSignal::new(Vec::new()),
            streaming: RwSignal::new(Streaming::default()),
            queued: RwSignal::new(Vec::new()),
            edit_open: RwSignal::new(false),
            edit_rows: RwSignal::new(Vec::new()),
            activity: RwSignal::new(None),
            secrets: RwSignal::new(Vec::new()),
            synced: RwSignal::new(false),
            from: RwSignal::new(0),
            to: RwSignal::new(None),
            loading: RwSignal::new(false),
            id: StoredValue::new(id.clone()),
        };
        let Feed { events, streaming, queued, activity, secrets, synced, from, to, .. } = feed;
        // The replay arrives all at once (not an event at a time, each
        // drawn and scrolled to).
        let replay = StoredValue::new(Vec::<Event>::new());
        let again = StoredValue::new(false);
        // How far the log has come: streamed text that arrives late is
        // already in its messages.
        let logged = StoredValue::new(Logged::default());
        let handle = api::stream_session(
            id,
            move |msg| match msg {
                // Replayed turns don't start and stop it again (the log says
                // whether one is running).
                StreamMsg::Event { event } if !synced.get_untracked() || again.get_value() => {
                    logged.update_value(|l| l.note(&event));
                    let last =
                        replay.with_value(|r| r.last().map(|e| e.seq)).or_else(|| events.with_untracked(|e| e.last().map(|e| e.seq)));
                    if last.is_none_or(|l| l < event.seq) {
                        replay.update_value(|r| r.push(event));
                    }
                }
                StreamMsg::Event { event } => {
                    streaming.update(|s| logged.update_value(|l| l.event(s, &event)));
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
                StreamMsg::Window { from: f } => from.set(f),
                msg @ (StreamMsg::Text { .. } | StreamMsg::Thinking { .. } | StreamMsg::ToolOutput { .. }) => {
                    streaming.maybe_update(|s| logged.with_value(|l| l.stream(s, msg)))
                }
                StreamMsg::Activity { running, status } => {
                    activity.set(Some(running));
                    // A retry starts the response over: drop what the failed call streamed.
                    if !status.is_empty() {
                        streaming.set(Streaming { status, ..Default::default() });
                    } else {
                        streaming.update(|s| s.status.clear());
                    }
                }
                StreamMsg::Queue { queued: q } => queued.set(q),
                StreamMsg::Synced => {
                    let r = replay.try_update_value(std::mem::take).unwrap_or_default();
                    // Opened at a place: drawn from there, not to the end.
                    if let Window::At(at) = window {
                        to.set(bound(&r, at));
                    }
                    // Missed while away: what streamed for it goes, and
                    // whether a turn runs is as its last start or end says.
                    if again.get_value() && !r.is_empty() {
                        streaming.update(|s| logged.update_value(|l| r.iter().for_each(|e| l.event(s, e))));
                        let running = r.iter().rev().find_map(|e| match e.kind {
                            EventKind::TurnStarted { .. } => Some(true),
                            EventKind::TurnEnded { .. } => Some(false),
                            _ => None,
                        });
                        if let Some(running) = running {
                            activity.set(Some(running));
                        }
                    }
                    if !r.is_empty() {
                        events.update(|evs| evs.extend(r));
                    }
                    again.set_value(false);
                    synced.set(true);
                }
                StreamMsg::Secret { call_id, title, value, note } => secrets.update(|s| s.push(SecretCard { call_id, title, value, note })),
            },
            move || {
                // Connecting again (a phone waking, the network back): what
                // was missed arrives all at once too, not drawn and scrolled
                // to an event at a time (it stuttered).
                if synced.try_get_untracked() == Some(true) {
                    again.set_value(true);
                }
                let last = replay.with_value(|r| r.last().map(|e| e.seq)).or_else(|| events.with_untracked(|e| e.last().map(|e| e.seq)));
                match (last, window) {
                    (Some(n), _) => format!("after={n}"),
                    (None, Window::All) => "after=0".into(),
                    (None, Window::Tail) => format!("after=0&tail={TAIL}"),
                    (None, Window::At(at)) => format!("after=0&tail={TAIL}&at={at}"),
                }
            },
        );
        on_cleanup(move || handle.abort());
        feed
    }

    /// Whether events before the window are left to load.
    pub(crate) fn more(&self) -> bool {
        self.from.get() > 1
    }

    /// The window starts earlier: about [`TAIL`] events, or back to `at`.
    /// `then`: once that's drawn.
    pub(crate) fn older(self, at: Option<u64>, then: impl FnOnce() + 'static) {
        let before = self.from.get_untracked();
        if before <= 1 || self.loading.get_untracked() {
            return;
        }
        self.loading.set(true);
        spawn_local(async move {
            let page = api::session_events_before(self.id.get_value(), before, TAIL, at).await;
            self.loading.set(false);
            let Ok(page) = page else { return };
            let start = page.first().map_or(1, |e| e.seq);
            // In place of the summary's events from there.
            self.events.update(|evs| {
                let (a, b) = (evs.partition_point(|e| e.seq < start), evs.partition_point(|e| e.seq < before));
                evs.splice(a..b, page);
            });
            // A jump: drawn from there (not if the session has gone meanwhile).
            if let Some(at) = at
                && let Some(to) = self.events.try_with_untracked(|evs| bound(evs, at))
            {
                self.to.set(to);
            }
            self.from.set(start);
            request_animation_frame(then);
        });
    }

    /// Drawn further down: about [`TAIL`] more events.
    pub(crate) fn later(self) {
        if let Some(to) = self.to.get_untracked() {
            self.to.set(self.events.with_untracked(|evs| bound(evs, to)));
        }
    }

    /// Back to the end: the last [`TAIL`] events or so.
    pub(crate) fn latest(self) {
        self.cut(0);
        self.to.set(None);
    }

    /// Back to the last [`TAIL`] events or so (from one of your messages),
    /// once the window has grown to twice that.
    pub(crate) fn trim(self) {
        if self.to.get_untracked().is_none() {
            self.cut(2 * TAIL);
        }
    }

    fn cut(self, over: u64) {
        let from = self.from.get_untracked();
        let cut = self.events.with_untracked(|evs| {
            let shown = &evs[evs.partition_point(|e| e.seq < from.max(1))..];
            if (shown.len() as u64) < over.max(TAIL + 1) {
                return None;
            }
            let at = shown.len() - TAIL as usize;
            shown[..=at].iter().rev().find(|e| matches!(e.kind, EventKind::User { .. })).map(|e| e.seq).filter(|&s| s > from)
        });
        let Some(cut) = cut else { return };
        self.events.update(|evs| evs.retain(|e| e.seq >= cut || e.kind.summary()));
        self.from.set(cut);
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
    let active = move || col.is_none_or(|c| c.active.try_get_untracked() == Some(true));
    let root = col.map(|c| c.root_id()).unwrap_or_default();
    // A document this session starts opens in a tile of its own (the
    // session stays), so you watch it being written (docs/docs.md §6).
    {
        let by = format!("session:{id}");
        let seen = live.doc.get_untracked();
        Effect::new(move |_| {
            let Some(c) = live.doc.get() else { return };
            if c.what == "created"
                && c.by == by
                && Some(&c) != seen.as_ref()
                && let Some(col) = col
            {
                col.open(View::Docs { id: c.id.clone() }, crate::workspace::How::OwnTile);
            }
        });
    }
    let detail = LocalResource::new({
        let id = id.clone();
        move || api::session(id.clone())
    });
    let target = target.unwrap_or_default();
    // Where it was found: a window reaching back to there, if that's known.
    let window = match target.at.or_else(|| target.anchor.as_deref().and_then(|a| anchor_seq(a, &id))) {
        Some(seq) => Window::At(seq),
        None if target.anchor.is_some() => Window::All,
        None => Window::Tail,
    };
    let feed = Feed::open(id.clone(), window);
    let Feed { events, streaming, queued, activity, .. } = feed;
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
    let scroller = NodeRef::<leptos::html::Div>::new();
    let stick = RwSignal::new(target.anchor.is_none());
    // Scrolled more than a screen above the end, and how many events there
    // were when you left the end (to count what came since).
    let far = RwSignal::new(false);
    let left_at = RwSignal::new(0usize);
    Effect::new(move |was: Option<bool>| {
        let now = stick.get();
        if was == Some(true) && !now {
            left_at.set(events.with_untracked(Vec::len));
        }
        now
    });
    let force_open = ForceOpen(RwSignal::new(target.open.clone()));
    provide_context(force_open);
    // How it shows itself (a catch-up card, its finished turns folded), the
    // same on every device.
    let view_settings = RwSignal::new(None::<c6_api::SessionViewSettings>);
    {
        let id = id.clone();
        spawn_local(async move {
            if let Ok(v) = api::session_view(id).await {
                let _ = view_settings.try_set(Some(v));
            }
        });
    }
    // Its turns: the glance's outline, and which finished turns fold.
    let timeline = crate::timeline::Timeline::new(
        id.clone(),
        events,
        Signal::derive(move || crate::timeline::SHOWN && view_settings.with(|v| v.as_ref().is_some_and(|v| v.fold))),
    );
    provide_context(timeline);
    let reach = RwSignal::new(HashMap::new());
    provide_context(Reach(reach));
    provide_context(Follow(stick));
    // Drawn only part way (after a jump back): its last event isn't the
    // end, so it isn't followed.
    Effect::new(move || {
        if feed.to.get().is_some() && stick.get() {
            stick.set(false);
        }
    });
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
                    timeline.pin(seq);
                    reveal_when_rendered(root.clone(), el, terms.clone(), 0);
                });
            }
            None if anchor.starts_with(&format!("waiting-{id}-")) => {
                let call = anchor.strip_prefix(&format!("waiting-{id}-")).unwrap_or_default().to_owned();
                let done = RwSignal::new(false);
                Effect::new(move || {
                    if done.get_untracked() {
                        return;
                    }
                    let seq = events.with(|evs| waiting_call_seq(evs, &call));
                    let Some(seq) = seq else { return };
                    done.set(true);
                    timeline.pin(seq);
                    reveal_when_rendered(root.clone(), anchor.clone(), terms.clone(), 0);
                });
            }
            None => reveal_when_rendered(root, anchor, terms, 0),
        }
    }
    Effect::new(move || {
        events.track();
        streaming.track();
        queued.track();
        if stick.get() {
            // Back at the end: what you scrolled back to goes again.
            feed.trim();
            request_animation_frame(move || {
                if stick.try_get_untracked() == Some(true)
                    && let Some(el) = scroller.try_get_untracked().flatten()
                {
                    crate::workspace::glide_to_end(&el);
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
    // Where the session really runs (its worktree, if it has one).
    let started = Memo::new(move |_| {
        events.with(|evs| match evs.first().map(|e| &e.kind) {
            Some(EventKind::Started { cwd, worktree_branch, repo_root, .. }) => {
                Some(Started { cwd: cwd.clone(), branch: worktree_branch.clone(), repo: repo_root.clone() })
            }
            _ => None,
        })
    });
    // Its folder's git state, for the Changes count and the branch: not
    // over SSH, and while its worker is online.
    let git_at = Memo::new(move |_| {
        let s = detail.get().and_then(Result::ok)?.session;
        let cwd = started.get()?.cwd;
        (s.ssh.is_none() && live.workers.with(|ws| !worker_offline(&s, ws))).then(|| (s.worker_id.to_string(), cwd))
    });
    let git = crate::details::git_state(git_at, events);
    let meta_root = StoredValue::new(root.clone());
    let subagents = SubagentTree::new(id_stored, detail, events, force_open, meta_root, timeline);
    // Unlike the live session list, this includes approvals/questions in
    // subagents, even when their threads (or this session's old turns)
    // aren't drawn. Keep the last answer while a ping reloads it.
    let waiting_list = LocalResource::new(move || {
        live.attention.track();
        live.connected.track();
        api::session_waiting(id_stored.get_value())
    });
    let waiting_shown = RwSignal::new(Vec::new());
    Effect::new(move || {
        if let Some(Ok(items)) = waiting_list.get() {
            waiting_shown.set(items);
        }
    });
    let waiting = Memo::new(move |_| waiting_shown.with(|items| events.with(|evs| waiting_on_you(&id_stored.get_value(), evs, items))));
    // What runs in its background now: subagents and tasks (servers too),
    // a line each.
    let busy = Memo::new(move |_| {
        let subagents = subagents.running_titles().into_iter().map(|t| format!("Subagent: {t}"));
        let tasks = events.with(|e| crate::tasks::running(e)).into_iter().map(|t| format!("Task: {t}"));
        subagents.chain(tasks).collect::<Vec<_>>()
    });

    let find_root = root.clone();
    // "New like this": a popup with the new-session form, filled in from
    // this session as it is now (its labels, its current model).
    let like_this = RwSignal::new(None::<sessions::SessionSeed>);
    let open_like_this = move || {
        let id = id_stored.get_value();
        let Some(d) = detail.get_untracked().and_then(Result::ok) else { return };
        let s = live.sessions.with_untracked(|ss| ss.iter().find(|s| s.id == id).cloned()).unwrap_or(d.session);
        let mut seed = sessions::SessionSeed::from_session(&s);
        let (c, m, e) = choice.current.get_untracked();
        if !c.is_empty() {
            (seed.connection_id, seed.model, seed.effort) = (c, m, e);
        }
        seed.worktree = started.get_untracked().is_some_and(|s| !s.branch.is_empty());
        like_this.set(Some(seed));
    };
    // "Save as template": its place and model, for agents or you to start
    // sessions like it (docs/remote-subagents.md §4).
    let as_template = RwSignal::new(None::<sessions::SessionSeed>);
    let open_as_template = move || {
        let id = id_stored.get_value();
        let Some(d) = detail.get_untracked().and_then(Result::ok) else { return };
        let s = live.sessions.with_untracked(|ss| ss.iter().find(|s| s.id == id).cloned()).unwrap_or(d.session);
        let mut seed = sessions::SessionSeed::from_session(&s);
        let (c, m, e) = choice.current.get_untracked();
        if !c.is_empty() {
            (seed.connection_id, seed.model, seed.effort) = (c, m, e);
        }
        // The session's folder is the repository a worktree was made of.
        seed.worktree = started.get_untracked().is_some_and(|s| !s.branch.is_empty());
        as_template.set(Some(seed));
    };
    // The glance: a column beside the transcript while the session's
    // column is wide, else a popover from the context ring (a sheet from
    // the bottom on a phone).
    let pane_el = NodeRef::<leptos::html::Div>::new();
    let wide = RwSignal::new(false);
    watch_width(pane_el, wide);
    provide_context(GlanceBeside(wide));
    let glance_at = RwSignal::new(None::<(f64, f64)>);
    let sheet = RwSignal::new(false);
    Effect::new(move || {
        if wide.get() {
            glance_at.set(None);
            sheet.set(false);
        }
    });
    let details = RwSignal::new(false);
    // What runs in its background (◌ N): a popover, a sheet on a phone.
    let background_at = RwSignal::new(None::<(f64, f64)>);
    let background_sheet = RwSignal::new(false);
    // To a place in the transcript: loaded back to there, and its turn open
    // if finished turns fold.
    let reach_to = Callback::new(move |seq: u64| {
        stick.set(false);
        if seq > 0 {
            timeline.pin(seq);
            reach.update(|r| {
                r.insert(id_stored.get_value(), seq);
            });
        }
    });
    let jump = Callback::new(move |(seq, el): (u64, String)| {
        reach_to.run(seq);
        reveal_when_rendered(meta_root.get_value(), el, "[]".into(), 0);
    });
    // Its catch-up, where it isn't the glance column's first section: a
    // popover from the pill over the transcript, or a phone's sheet.
    let cards = crate::catchup::cards();
    let card_open = Memo::new(move |_| cards.and_then(|c| c.with(|m| m.get(&id_stored.get_value()).filter(|c| c.open).map(|c| c.c.steps))));
    let catch_at = RwSignal::new(None::<(f64, f64)>);
    let catch_sheet = RwSignal::new(false);
    Effect::new(move || {
        if wide.get() || card_open.get().is_none() {
            catch_at.set(None);
            catch_sheet.set(false);
        }
    });
    let chrome = SessionChrome {
        token: SessionChrome::next_token(),
        id: id_stored,
        detail,
        find_open,
        like_this: Callback::new(move |()| open_like_this()),
        as_template: Callback::new(move |()| open_as_template()),
        col,
        events,
        from: feed.from,
        usage,
        started,
        model_label,
        connection: Signal::derive(move || choice.current.with(|(c, _, _)| c.clone())),
        git,
        subagents,
        running: Signal::derive(running),
        busy,
        waiting,
        root: meta_root,
        wide,
        glance_at,
        sheet,
        details,
        background_at,
        background_sheet,
        view_settings,
        timeline,
        reach_to,
    };
    // Esc closes the session's details or its glance; pressed twice within
    // 1.5s it stops a running turn, from the message box too (once only
    // arms it, and Stop reads "Esc", so a stray Esc doesn't stop the
    // agent); ⌘I opens and closes
    // the glance where it isn't a column, ⌘F finds in the session.
    let esc_id = id.clone();
    let esc_armed = RwSignal::new(None::<i64>);
    provide_context(EscArmed(esc_armed));
    // Claude Code's own background agents stop with the turn: Stop and Esc
    // ask first while any run.
    let guard =
        StopGuard { agents: Memo::new(move |_| events.with(|e| crate::tasks::background_agents(e))), confirm: RwSignal::new(false) };
    provide_context(guard);
    // Removed when the session goes: left behind, it read its disposed
    // signals on every key, a panic each, until the app ran out of stack.
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if !active() {
            return;
        }
        let in_field = ev
            .target()
            .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            .is_some_and(|el| matches!(el.tag_name().as_str(), "TEXTAREA" | "INPUT"));
        // Taken already (⌘I is italic in a chat's composer).
        let command = (ev.meta_key() || ev.ctrl_key()) && !ev.alt_key() && !ev.shift_key() && !ev.default_prevented();
        match ev.key().as_str() {
            "Escape" if guard.confirm.get_untracked() => guard.confirm.set(false),
            "Escape" if details.get_untracked() => details.set(false),
            "Escape" if !in_field && (glance_at.get_untracked().is_some() || sheet.get_untracked()) => chrome.close_glance(),
            "Escape" if !in_field && background_sheet.get_untracked() => background_sheet.set(false),
            // Taken already: a menu, a mention list or a suggestion put away.
            "Escape" if running() && !ev.default_prevented() => {
                let now = now_ms();
                if esc_armed.get_untracked().is_some_and(|at| now - at < 1500) {
                    esc_armed.set(None);
                    if guard.agents.get_untracked() > 0 {
                        guard.confirm.set(true);
                    } else {
                        let id = esc_id.clone();
                        spawn_local(async move {
                            let _ = api::interrupt(id).await;
                        });
                    }
                } else {
                    esc_armed.set(Some(now));
                    set_timeout(
                        move || {
                            // Not if the session has gone meanwhile.
                            if esc_armed.try_get_untracked() == Some(Some(now)) {
                                esc_armed.set(None)
                            }
                        },
                        std::time::Duration::from_millis(1500),
                    );
                }
            }
            // Not in a field, where it may be italic.
            "i" | "I" if command && !in_field => {
                ev.prevent_default();
                chrome.toggle_glance(None);
            }
            "f" | "F" if command => {
                ev.prevent_default();
                find_open.set(true);
            }
            _ => {}
        }
    });
    on_cleanup(move || keys.remove());
    // The tile's bar offers this session's panes and actions, wherever you
    // are in the transcript.
    if let Some(c) = col {
        let ws = c.ws;
        ws.chrome.update(|m| {
            m.insert(c.col, chrome);
        });
        on_cleanup(move || {
            if ws.chrome.with_untracked(|m| m.get(&c.col).is_some_and(|x| x.token == chrome.token)) {
                ws.chrome.update(|m| {
                    m.remove(&c.col);
                });
            }
        });
    }
    // Its panes and actions are in the tile's bar while that bar is this
    // session's; with another view beside it (a doc), in a strip of its own.
    let own_strip = move || col.is_none_or(|c| !c.in_bar());
    view! {
        // Measured: the glance is a column only when the session is wide.
        <div node_ref=pane_el class="@container relative flex min-h-0 flex-1 flex-col">
        {move || like_this.get().map(|seed| view! { <sessions::NewSessionDialog seed on_close=move |()| like_this.set(None) /> })}
        {move || as_template.get().map(|seed| view! { <sessions::SaveTemplateDialog seed on_close=move |()| as_template.set(None) /> })}
        {move || details.get().then(|| view! { <crate::details::SessionDetails session=id_stored.get_value() usage on_close=move |()| details.set(false) /> })}
        <Show when=own_strip>
            <SessionStrip chrome />
        </Show>
        <div class="relative flex min-h-0 min-w-0 flex-1 flex-col">
        // At the transcript's right edge, not the column's.
        {move || find_open.get().then(|| view! {
            <div class=format!("pointer-events-none absolute inset-x-0 top-0 z-30 {READING_ROW}")>
                <div class=format!("relative {READING} [&>*]:pointer-events-auto")>
                    <FindBar session=id_stored.get_value() initial=find_q.clone() open=find_open force_open root=find_root.clone() />
                </div>
                {glance_room(wide)}
            </div>
        })}
        // Only up and down: what's wider (code, a table) scrolls within itself.
        <ScrollBox node_ref=scroller follow=stick class="overflow-x-hidden" on_scroll=Callback::new(move |()| {
            if let Some(el) = scroller.get_untracked() {
                far.set(el.scroll_height() - el.scroll_top() - el.client_height() > el.client_height());
            }
        })>
        // The transcript at its reading width and, when the session is wide,
        // the glance beside it, the two centred together; the glance stays
        // in sight as the transcript scrolls.
        <div class=format!("{READING_ROW} items-start")>
        <div class=format!("{READING} px-4 pb-8 pt-4")>
            // Its catch-up, where the glance isn't a column: a pill that
            // stays at the top as the transcript scrolls.
            {move || card_open.get().filter(|_| !wide.get()).map(|steps| view! {
                <div class="pointer-events-none sticky top-2 z-20 mb-3 flex justify-center">
                    <button class="pointer-events-auto rounded-full border border-sky-300 bg-sky-50/95 px-3 py-1 text-xs font-medium text-sky-800 shadow-md hover:bg-sky-100 dark:border-sky-800 dark:bg-sky-950/95 dark:text-sky-200 dark:hover:bg-sky-900"
                        title="What it did since you looked"
                        on:click=move |ev| if crate::util::phone() { catch_sheet.set(true) } else { catch_at.set(Some(below(&ev, 384.0))) }>
                        {format!("Catch up · {steps} new")}
                    </button>
                </div>
            })}
            <Suspense fallback=|| ()>
                {move || detail.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(d) => {
                        let s = d.session.clone();
                        let on = s.clone();
                        let online = move || live.workers.with(|ws| !worker_offline(&on, ws));
                        view! {
                            {s.error.clone().map(|e| view! { <ErrorBox message=format!("The session failed to start: {e}") /> })}
                            {move || (!online() && (status.get() == SessionStatus::Running || !queued.with(Vec::is_empty))).then(|| view! {
                                <p class="mb-3 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
                                    "The worker is offline. A running turn continues there and syncs back; queued messages are delivered when it reconnects."
                                </p>
                            })}
                            {s.parent_id.clone().map(|p| if s.parent_call_id.is_some() {
                                view! { <Lineage session=d.session.clone() /> }.into_any()
                            } else {
                                view! { <p class="mb-3 text-sm text-muted">"Side chat · branched from "<Go to=View::session(p) class="underline">"its parent session"</Go></p> }.into_any()
                            })}
                        }.into_any()
                    }
                })}
            </Suspense>
            {move || detail.get().and_then(Result::ok).map(|d| view! {
                <Transcript session=d.session feed running=Signal::derive(running) timeline />
            })}
        </div>
        <Show when=move || wide.get()>
            <aside class=format!("sticky top-0 max-h-[calc(100dvh-8rem)] overflow-y-auto pb-4 pt-4 {GLANCE_COLUMN}")>
                <crate::catchup::CatchupBody session=id_stored.get_value() jump caps=true
                    class="mb-2 border-b border-zinc-200 px-1.5 pb-3 dark:border-zinc-800" />
                <crate::details::Glance chrome />
            </aside>
        </Show>
        </div>
        // Scrolled up a screen or more: back to the end in one click, with
        // what came in meanwhile counted. It stays at the box's lower edge.
        {move || far.get().then(|| {
            let fresh = events.with(|evs| evs.len()).saturating_sub(left_at.get());
            view! {
                <div class=format!("pointer-events-none sticky bottom-3 {READING_ROW}")>
                    <div class=format!("flex justify-end px-4 {READING}")>
                    <button class="pointer-events-auto rounded-full border border-zinc-300 bg-white/95 px-3 py-1 text-xs font-medium shadow-md hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-900/95 dark:hover:bg-zinc-800"
                        title="To the latest (End)"
                        on:click=move |_| {
                            if feed.to.get_untracked().is_some() {
                                feed.latest();
                            }
                            stick.set(true);
                            far.set(false);
                            if let Some(el) = scroller.get_untracked() {
                                el.set_scroll_top(el.scroll_height());
                            }
                        }>
                        {crate::icons::arrow_down()}" "{if fresh > 0 { format!("{fresh} new") } else { "Latest".to_owned() }}
                    </button>
                    </div>
                    {glance_room(wide)}
                </div>
            }
        })}
        </ScrollBox>
            <SubagentWaiting chrome />
            {move || {
                // Claude Code's own subagents take no messages.
                if detail.get().and_then(|d| d.ok()).is_some_and(|d| d.session.observed) {
                    return ().into_any();
                }
                // A subagent's reply box can wake the agent that started it.
                let subagent = detail.get().and_then(|d| d.ok()).is_some_and(|d| d.session.parent_call_id.is_some());
                let wake = subagent.then(|| RwSignal::new(false));
                let files = Signal::derive(move || {
                    // @-completion lists the worker's files: not for SSH sessions.
                    let s = detail.get().and_then(|d| d.ok())?.session;
                    let cwd = started.get()?.cwd;
                    s.ssh.is_none().then_some((s.worker_id.to_string(), cwd))
                });
                match wake {
                    Some(w) => view! {
                        <Composer id=id_stored.get_value() running=Signal::derive(running) disabled=Signal::derive(move || status.get() == SessionStatus::Failed) error files wake=w />
                    }.into_any(),
                    None => {
                        let row = detail.get().and_then(|d| d.ok()).map(|d| d.session);
                        // Claude Code runs on the worker itself, not over SSH;
                        // Ask runs it on the head's machine, if it has it.
                        let no_claude_code = row.as_ref().is_some_and(|s| s.ssh.is_some());
                        let ask = row.as_ref().is_some_and(|s| s.on_head);
                        let start = row.map(|s| (s.subagent_connection_id, s.subagent_model, s.subagent_effort)).unwrap_or_default();
                        let subagents = SubagentChoice::follow(events, start, choice.current);
                        view! {
                            <Composer id=id_stored.get_value() running=Signal::derive(running) disabled=Signal::derive(move || status.get() == SessionStatus::Failed) error model=choice files subagents no_claude_code ask />
                        }.into_any()
                    }
                }
            }}
        </div>
        <Popover at=glance_at class="w-72">
            <crate::details::Glance chrome on_done=move |()| glance_at.set(None) />
        </Popover>
        {move || sheet.get().then(|| view! {
            <BottomSheet open=sheet><crate::details::Glance chrome on_done=move |()| sheet.set(false) /></BottomSheet>
        })}
        <Popover at=background_at class="w-96 max-w-[calc(100vw-1rem)]">
            <BackgroundWork chrome on_done=move |()| background_at.set(None) />
        </Popover>
        {move || background_sheet.get().then(|| view! {
            <BottomSheet open=background_sheet><BackgroundWork chrome on_done=move |()| background_sheet.set(false) /></BottomSheet>
        })}
        <Popover at=catch_at class="w-[min(24rem,calc(100vw-1rem))] p-3">
            <crate::catchup::CatchupBody session=id_stored.get_value() jump on_done=move |()| catch_at.set(None) />
        </Popover>
        {move || catch_sheet.get().then(|| view! { <crate::catchup::CatchupSheet session=id_stored.get_value() jump open=catch_sheet /> })}
        </div>
    }
}

/// The transcript's reading column (and what lines up with it: the message
/// box, the find bar): at most 56rem, centred.
const READING: &str = "w-full min-w-0 max-w-4xl";
/// A row centring the reading column and, when the glance is beside it,
/// the gap and the glance (or the room they take), as one group.
const READING_ROW: &str = "flex justify-center gap-6";

/// How wide the session's column must be for the glance to be a column
/// beside the transcript: the reading column (896px), the gap (24px), the
/// glance and a 16px margin on either side.
const GLANCE_COLUMN_MIN: i32 = 896 + 24 + GLANCE_WIDTH as i32 + 2 * 16;

/// The glance as a column: 288px where it starts, growing to 380px with
/// what the session's column has left after the reading column (56rem),
/// the gap (1.5rem) and the margins (1rem each): 100% − 59.5rem.
const GLANCE_COLUMN: &str = "w-[clamp(18rem,calc(100%_-_59.5rem),23.75rem)] shrink-0";

/// Where the glance is beside the transcript: the room it and the gap
/// take, so what lines up with the transcript keeps its left edge.
fn glance_room(beside: RwSignal<bool>) -> impl IntoView {
    move || beside.get().then(|| view! { <div class=GLANCE_COLUMN aria-hidden="true"></div> })
}

/// The glance is beside the transcript (for the message box, to line up
/// with it).
#[derive(Clone, Copy)]
struct GlanceBeside(RwSignal<bool>);

/// Keeps `wide` saying whether the element is wide enough for the glance
/// beside it, as it resizes (its tile, a pane beside it, the window).
fn watch_width(el: NodeRef<leptos::html::Div>, wide: RwSignal<bool>) {
    type ResizeWatch = (web_sys::ResizeObserver, wasm_bindgen::closure::Closure<dyn Fn(js_sys::Array)>);
    let observer = StoredValue::new_local(None::<ResizeWatch>);
    Effect::new(move || {
        let Some(node) = el.get() else { return };
        // One at a time: an element drawn anew isn't watched twice.
        observer.update_value(|o| {
            if let Some((ro, _)) = o.take() {
                ro.disconnect();
            }
        });
        let measure = wasm_bindgen::closure::Closure::<dyn Fn(js_sys::Array)>::new(move |_: js_sys::Array| {
            // A last resize may come after the pane has gone; hidden (a
            // phone's other pane), it keeps what it was.
            if let Some(n) = el.try_get_untracked().flatten()
                && n.client_width() > 0
            {
                let w = n.client_width() >= GLANCE_COLUMN_MIN;
                if wide.try_get_untracked() != Some(w) {
                    wide.set(w);
                }
            }
        });
        if let Ok(obs) = web_sys::ResizeObserver::new(wasm_bindgen::JsCast::unchecked_ref(measure.as_ref())) {
            obs.observe(&node);
            observer.set_value(Some((obs, measure)));
        }
    });
    on_cleanup(move || {
        observer.update_value(|o| {
            if let Some((ro, _)) = o.take() {
                ro.disconnect();
            }
        });
    });
}

/// Where a session works, from its first event: its folder (its worktree,
/// if it has one), the worktree's branch, and the repository it's of.
#[derive(Clone, PartialEq)]
pub(crate) struct Started {
    pub(crate) cwd: String,
    pub(crate) branch: String,
    pub(crate) repo: String,
}

/// What a session pane offers its tile's bar (its panes, what runs in it,
/// its context ring and its ⋯), and its glance.
#[derive(Clone, Copy)]
pub struct SessionChrome {
    /// Which pane registered it (a pane's view can be replaced in place).
    token: u64,
    pub(crate) id: StoredValue<String>,
    pub(crate) detail: LocalResource<Result<c6_api::SessionDetail, String>>,
    find_open: RwSignal<bool>,
    like_this: Callback<()>,
    as_template: Callback<()>,
    pub(crate) col: Option<ColumnCtx>,
    pub(crate) events: RwSignal<Vec<Event>>,
    /// The first event its transcript shows (see [`Feed::from`]).
    from: RwSignal<u64>,
    pub(crate) usage: Memo<UsageStats>,
    pub(crate) started: Memo<Option<Started>>,
    pub(crate) model_label: Memo<String>,
    /// The connection it uses now.
    pub(crate) connection: Signal<String>,
    pub(crate) git: LocalResource<Option<(usize, String)>>,
    pub(crate) subagents: SubagentTree,
    /// A turn is running.
    pub(crate) running: Signal<bool>,
    /// The background tasks and subagents that run now, a line each.
    busy: Memo<Vec<String>>,
    /// Its decisions/cards and approvals/questions throughout its subagents.
    pub(crate) waiting: Memo<Vec<Waiting>>,
    /// Its pane's root, where its transcript's events are found.
    root: StoredValue<String>,
    /// The glance is a column beside the transcript.
    wide: RwSignal<bool>,
    /// The glance as a popover, while open.
    glance_at: RwSignal<Option<(f64, f64)>>,
    /// The glance as a phone's bottom sheet, while open.
    sheet: RwSignal<bool>,
    /// The session's details dialog is open.
    details: RwSignal<bool>,
    /// What runs in its background (◌ N), as a popover, while open.
    background_at: RwSignal<Option<(f64, f64)>>,
    /// The same as a phone's bottom sheet, while open.
    background_sheet: RwSignal<bool>,
    /// Whether it shows a catch-up card and folds its finished turns (None
    /// until read).
    view_settings: RwSignal<Option<c6_api::SessionViewSettings>>,
    /// Its turns (the glance's outline), and which finished turns fold.
    pub(crate) timeline: crate::timeline::Timeline,
    /// To an event in the transcript: loaded back to it, its turn open.
    reach_to: Callback<u64>,
}

impl SessionChrome {
    fn next_token() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// It works over SSH: no Changes or Files (the worker can't look at
    /// that machine's files).
    fn ssh(&self) -> bool {
        self.detail.get().and_then(Result::ok).is_some_and(|d| d.session.ssh.is_some())
    }

    /// Whether its pane is on screen (tracked, from the layout).
    fn pane_open(&self, pane: Pane) -> bool {
        let id = self.id.get_value();
        self.col.is_some_and(|c| crate::panes::shown(c.ws, pane, &id).is_some_and(|(_, _, on)| on))
    }

    /// Its pane opened, or closed if it's open; ⌘/Ctrl-click: in a new tile.
    fn toggle_pane(&self, pane: Pane, ev: &web_sys::MouseEvent) {
        if let Some(c) = self.col {
            crate::panes::toggle(c, pane, &self.id.get_value(), ev.meta_key() || ev.ctrl_key());
        }
    }

    /// Its pane opened, unless it is already.
    fn show_pane(&self, pane: Pane, ev: &web_sys::MouseEvent) {
        if let Some(c) = self.col {
            crate::panes::show(c, pane, &self.id.get_value(), ev.meta_key() || ev.ctrl_key());
        }
    }

    fn ring_id(&self) -> String {
        format!("ring-{}", self.token)
    }

    /// Opens or closes the glance where it isn't a column: under `anchor`
    /// (by default the context ring), its right edge with the anchor's, or
    /// on a phone as a sheet from the bottom.
    fn toggle_glance(&self, anchor: Option<web_sys::Element>) {
        if self.wide.get_untracked() {
            return;
        }
        if self.glance_at.get_untracked().is_some() || self.sheet.get_untracked() {
            return self.close_glance();
        }
        if crate::util::phone() {
            return self.sheet.set(true);
        }
        let rect = anchor.or_else(|| document().get_element_by_id(&self.ring_id())).map(|el| el.get_bounding_client_rect());
        self.glance_at.set(Some(match rect {
            Some(r) => ((r.right() - GLANCE_WIDTH).max(8.0), r.bottom() + 4.0),
            None => (8.0, 48.0),
        }));
    }

    fn close_glance(&self) {
        self.glance_at.set(None);
        self.sheet.set(false);
    }

    /// Goes to the answerable card: here, in a drawn subagent thread, or
    /// in its own session if that thread isn't drawn.
    pub(crate) fn go_waiting(&self, waiting: Waiting) {
        match waiting.target {
            WaitingTarget::Decision(id) => {
                reveal_when_rendered(self.root.get_value(), crate::decisions::card_id(id), "[]".into(), 0);
            }
            WaitingTarget::Call { session, call, seq } => {
                let anchor = waiting_card_id(&session, &call);
                let view = View::Session {
                    id: session.clone(),
                    target: Some(Target { anchor: Some(anchor.clone()), at: seq, ..Default::default() }),
                };
                let col = self.col;
                let elsewhere = move || {
                    if let Some(c) = col {
                        c.open(view, crate::workspace::How::Beside);
                    }
                };
                if session == self.id.get_value() {
                    if let Some(seq) = seq {
                        self.timeline.pin(seq);
                    }
                    reveal_or(self.root.get_value(), vec![anchor], 0, elsewhere);
                } else {
                    self.subagents.go_to(session, Some(anchor), elsewhere);
                }
            }
        }
    }

    /// Goes to a turn in the transcript (loaded back to it, if it starts
    /// before what's loaded), and flashes it: its row while finished turns
    /// fold (opened), else its first message, or its first reply when
    /// nothing was said.
    pub(crate) fn go_turn(&self, t: &c6_api::TimelineTurn) {
        let (id, start, finished) = (self.id.get_value(), t.start_seq, t.end_seq > 0);
        let end = if finished { t.end_seq } else { u64::MAX };
        self.reach_to.run(start);
        let (events, from, timeline) = (self.events, self.from, self.timeline);
        // Its row first (a turn that was rewound has none).
        let row = (finished && timeline.fold.get_untracked()).then(|| format!("{id}-t{start}"));
        reveal_picked(
            self.root.get_value(),
            move || {
                // Not until it's loaded back to its start.
                if from.try_get_untracked().is_none_or(|f| f > start) {
                    return Vec::new();
                }
                let first = events.try_with_untracked(|evs| {
                    let these = &evs[evs.partition_point(|e| e.seq < start)..evs.partition_point(|e| e.seq <= end)];
                    these.iter().find(|e| matches!(e.kind, EventKind::User { .. } | EventKind::Assistant { .. })).map(|e| e.seq)
                });
                row.clone().into_iter().chain(first.flatten().map(|seq| format!("{id}-e{seq}"))).collect()
            },
            0,
        );
    }

    /// What a click on its state in the tile's bar does: to what waits on
    /// you, else what runs in its background, else the glance (where it
    /// isn't a column).
    pub(crate) fn status_title(&self) -> &'static str {
        if !self.waiting.with(Vec::is_empty) {
            "Go to what waits on you"
        } else if !self.busy.with(Vec::is_empty) {
            "What runs in the background"
        } else if self.wide.get() {
            "Its own turn's state"
        } else {
            "The glance (⌘I)"
        }
    }

    /// A click on its state in the tile's bar (see [`Self::status_title`]).
    pub(crate) fn status_click(&self, ev: &web_sys::MouseEvent) {
        if let Some(w) = self.waiting.with_untracked(|w| w.first().cloned()) {
            self.go_waiting(w);
        } else if !self.busy.with_untracked(Vec::is_empty) {
            self.toggle_background(ev);
        } else {
            self.toggle_glance(ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()));
        }
    }

    /// Opens or closes what runs in its background (◌ N): under what was
    /// clicked, or on a phone as a sheet from the bottom.
    fn toggle_background(&self, ev: &web_sys::MouseEvent) {
        if self.background_at.get_untracked().is_some() || self.background_sheet.get_untracked() {
            self.background_at.set(None);
            self.background_sheet.set(false);
        } else if crate::util::phone() {
            self.background_sheet.set(true);
        } else {
            self.background_at.set(Some(below(ev, 384.0)));
        }
    }
}

/// A text box to give a session a new name: Enter saves, Escape (or the
/// Cancel button) leaves it as it was. `on_done` runs after either.
/// Suggest fills it with a name a model makes from how the session
/// started, to start from.
#[component]
pub fn RenameForm(session: String, title: String, #[prop(into)] on_done: Callback<()>) -> impl IntoView {
    let id = StoredValue::new(session);
    let draft = RwSignal::new(title.clone());
    let error = RwSignal::new(None::<String>);
    let suggesting = RwSignal::new(false);
    let input = NodeRef::<leptos::html::Input>::new();
    let suggest = move |_| {
        suggesting.set(true);
        error.set(None);
        spawn_local(async move {
            let got = api::suggest_name(id.get_value()).await;
            let _ = suggesting.try_set(false);
            match got {
                Ok(name) => {
                    let _ = draft.try_set(name);
                    if let Some(el) = input.get_untracked() {
                        let _ = el.focus();
                        el.select();
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };
    let save = Callback::new(move |()| {
        let name = draft.get_untracked().trim().to_owned();
        if name.is_empty() || name == title.trim() {
            return on_done.run(());
        }
        spawn_local(async move {
            match api::rename(id.get_value(), name).await {
                Ok(()) => on_done.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    });
    view! {
        <div class="space-y-1" on:click=|ev| ev.stop_propagation()>
            <div class="flex items-center gap-1.5">
                <input node_ref=input class=format!("{INPUT} min-w-0 flex-1 py-1") prop:value=move || draft.get() autofocus maxlength="120" placeholder="Session name"
                    on:input=move |ev| draft.set(event_target_value(&ev))
                    on:keydown=move |ev| match ev.key().as_str() {
                        "Enter" => save.run(()),
                        "Escape" => { ev.stop_propagation(); on_done.run(()) }
                        _ => {}
                    } />
                <button class=format!("{BUTTON} py-1 text-xs") on:click=move |_| save.run(())>"Save"</button>
                <button class=format!("{BUTTON_QUIET} py-1 text-xs text-muted") on:click=move |_| on_done.run(())>"Cancel"</button>
            </div>
            <button class=format!("{BUTTON_QUIET} flex items-center gap-1 px-1.5 py-1 text-xs text-muted") disabled=move || suggesting.get() on:click=suggest>
                {crate::icons::sparkles()}{move || if suggesting.get() { "Suggesting…" } else { "Suggest" }}
            </button>
            {move || error.get().map(|e| view! { <p class="text-[11px] text-red-600">{e}</p> })}
        </div>
    }
}

/// The tile bar's ⋯ for a session: what you can do with it. `on_rename`
/// opens the tile's rename box; `on_done` closes the menu, after any
/// choice.
#[component]
pub fn SessionMenu(chrome: SessionChrome, #[prop(into)] on_rename: Callback<()>, #[prop(into)] on_done: Callback<()>) -> impl IntoView {
    let SessionChrome { id, detail, find_open, like_this, as_template, details, col, view_settings, .. } = chrome;
    let session = move || detail.get().and_then(Result::ok).map(|d| d.session);
    // Its catch-up card and folding its finished turns, on or off (kept on
    // the head). The switch moves once the head has it.
    let cards = crate::catchup::cards();
    let set_view = move |change: fn(&mut c6_api::SessionViewSettings, bool), on: bool| {
        let Some(mut v) = view_settings.get_untracked() else { return };
        change(&mut v, on);
        let id = id.get_value();
        spawn_local(async move {
            if api::set_session_view(id.clone(), v.clone()).await.is_err() {
                return;
            }
            if !v.catchup
                && let Some(c) = cards
            {
                c.update(|m| {
                    m.remove(&id);
                });
            }
            let _ = view_settings.try_set(Some(v));
        });
    };
    let flag = move |f: fn(&c6_api::SessionViewSettings) -> bool| Signal::derive(move || view_settings.with(|v| v.as_ref().is_some_and(f)));
    // Your own sessions are renamed; subagents and observed ones aren't.
    let renamable = move || session().is_some_and(|s| s.parent_id.is_none() && !s.observed);
    let seeds = move || session().is_some_and(|s| !s.observed && s.instance.is_empty());
    view! {
        <div class="flex flex-col">
            <Show when=renamable>
                <button class=MENU_ITEM on:click=move |_| { on_rename.run(()); on_done.run(()) }>
                    "Rename…"<span class=MENU_HINT>"double-click the title"</span>
                </button>
            </Show>
            <button class=MENU_ITEM on:click=move |_| {
                find_open.set(true);
                on_done.run(());
            }>"Find in session"<span class=MENU_HINT>"⌘F"</span></button>
            <div class=MENU_RULE></div>
            <Show when=move || !chrome.ssh()>
                <button class=MENU_ITEM title="Its folder, beside the transcript (⌘-click: a new tile)"
                    on:click=move |ev| { chrome.show_pane(Pane::Files, &ev); on_done.run(()) }>
                    {crate::icons::folder()}"Files"
                </button>
            </Show>
            <button class=MENU_ITEM title="What it published, beside the transcript (⌘-click: a new tile)"
                on:click=move |ev| { chrome.show_pane(Pane::Outputs, &ev); on_done.run(()) }>
                {crate::icons::package()}"Outputs"
            </button>
            <button class=MENU_ITEM on:click=move |_| { details.set(true); on_done.run(()) }>
                "Session details…"<span class=MENU_HINT>"usage, config"</span>
            </button>
            <Show when=seeds>
                <div class=MENU_RULE></div>
                <button class=MENU_ITEM
                    title="Start a new, empty session with this one's worker, folder, model and labels (you can change them first)"
                    on:click=move |_| { like_this.run(()); on_done.run(()) }>"New like this…"</button>
                <button class=MENU_ITEM
                    title="Save this session's worker, folder, model and labels as a template, which agents can start as a remote subagent"
                    on:click=move |_| { as_template.run(()); on_done.run(()) }>"Save as template…"</button>
            </Show>
            <Show when=move || view_settings.with(Option::is_some)>
                <div class=MENU_RULE></div>
                <MenuSwitch on=flag(|v| v.catchup) label="Catch-up" hint="a card when you come back"
                    on_change=move |on| set_view(|v, on| v.catchup = on, on) />
                {crate::timeline::SHOWN.then(|| view! {
                    <MenuSwitch on=flag(|v| v.fold) label="Fold finished turns" hint="one row per finished turn"
                        on_change=move |on| set_view(|v, on| v.fold = on, on) />
                })}
            </Show>
            <div class=MENU_RULE></div>
            <button class=format!("{MENU_ITEM} text-red-600 dark:text-red-400") title="Archive the session and close this column" on:click=move |_| {
                spawn_local(async move {
                    if api::archive(id.get_value()).await.is_ok() && let Some(c) = col { c.close(); }
                });
                on_done.run(());
            }>"Archive"</button>
        </div>
    }
}

/// An on/off item of the ⋯ menu: the switch, what it is and a hint. The
/// menu stays open, so you see it change.
#[component]
fn MenuSwitch(
    #[prop(into)] on: Signal<bool>,
    on_change: impl Fn(bool) + 'static,
    label: &'static str,
    hint: &'static str,
) -> impl IntoView {
    view! {
        <button type="button" role="switch" class=MENU_ITEM aria-checked=move || on.get().to_string() on:click=move |_| on_change(!on.get_untracked())>
            <span class=move || format!("relative h-3.5 w-[26px] shrink-0 rounded-full transition-colors {}", if on.get() { "bg-sky-600" } else { "bg-zinc-300 dark:bg-zinc-600" })>
                <span class=move || format!("absolute left-0.5 top-0.5 h-2.5 w-2.5 rounded-full bg-white shadow-sm transition-transform {}", if on.get() { "translate-x-3" } else { "" })></span>
            </span>
            {label}
            <span class=MENU_HINT>{hint}</span>
        </button>
    }
}

/// An icon button of a session's: 28px square round a 16px icon (40px on
/// a touch screen), filled only while what it opens is on screen. Hover
/// only with a mouse (a tapped button on a phone keeps no look), and a ring
/// for the keyboard's focus alone.
fn icon_class(open: bool) -> &'static str {
    if open {
        "flex h-7 min-w-7 shrink-0 items-center justify-center gap-0.5 rounded px-1.5 outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 pointer-coarse:h-10 pointer-coarse:min-w-10 bg-zinc-200 text-zinc-900 dark:bg-zinc-700 dark:text-zinc-100"
    } else {
        "flex h-7 min-w-7 shrink-0 items-center justify-center gap-0.5 rounded px-1.5 outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 pointer-coarse:h-10 pointer-coarse:min-w-10 text-muted pointer-fine:hover:bg-zinc-200/70 pointer-fine:hover:text-zinc-900 dark:pointer-fine:hover:bg-zinc-800 dark:pointer-fine:hover:text-zinc-100"
    }
}

/// How full the context is, as a 16px ring (amber from 80%).
fn context_ring(pct: Option<u64>) -> impl IntoView {
    // The circle's length: 2π × 6.
    const ROUND: f64 = 37.7;
    let p = pct.unwrap_or(0).min(100) as f64;
    let tone = if p >= 80.0 { "stroke-amber-500" } else { "stroke-blue-500" };
    view! {
        <svg viewBox="0 0 16 16" class="size-4 shrink-0" fill="none" stroke-width="2" aria-hidden="true">
            <circle cx="8" cy="8" r="6" class="stroke-zinc-300 dark:stroke-zinc-600" />
            <circle cx="8" cy="8" r="6" class=tone
                stroke-dasharray=format!("{:.2} {ROUND}", ROUND * p / 100.0) transform="rotate(-90 8 8)" />
        </svg>
    }
}

/// What runs in a session's background and its panes, for its tile's bar
/// (or its own strip): ◌ N while a task or subagent runs (each named on
/// hover, listed with Inspect and Stop on a click), then
/// Changes (with how many files changed), Terminal, and the context ring
/// while the glance isn't a column.
#[component]
pub fn SessionControls(chrome: SessionChrome) -> impl IntoView {
    let changed = move || chrome.git.get().flatten().map(|g| g.0).filter(|n| *n > 0);
    let glance_open = move || chrome.glance_at.get().is_some() || chrome.sheet.get();
    let pct = move || chrome.usage.with(|u| u.context.and_then(|used| (used * 100).checked_div(u.context_window)));
    let ring_title = move || {
        chrome.usage.with(|u| match u.context {
            Some(used) => match (used * 100).checked_div(u.context_window) {
                Some(p) => format!("Context: {p}% used ({} of {} tokens) · the glance (⌘I)", tokens(used), tokens(u.context_window)),
                None => format!("Context: {} tokens · the glance (⌘I)", tokens(used)),
            },
            None => "Context: known after the first model call · the glance (⌘I)".to_owned(),
        })
    };
    view! {
        {move || {
            let lines = chrome.busy.get();
            let n = lines.len();
            // A spinner and the count: what they are on hover, and in full
            // (with Inspect and Stop) on a click.
            let what = format!("{n} running in the background:");
            let open = move || chrome.background_at.get().is_some() || chrome.background_sheet.get();
            (n > 0).then(|| view! {
                <button class=move || format!("flex h-7 shrink-0 items-center gap-1 rounded px-1.5 text-xs text-blue-600 outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 pointer-coarse:h-10 pointer-coarse:px-2 dark:text-blue-400 {}",
                        if open() { "bg-zinc-200 dark:bg-zinc-700" } else { "pointer-fine:hover:bg-zinc-200/70 dark:pointer-fine:hover:bg-zinc-800" })
                    title=format!("{what}\n{}", lines.join("\n")) aria-label=format!("{what} {}", lines.join("; "))
                    aria-haspopup="dialog" aria-expanded=move || open().to_string()
                    on:click=move |ev| chrome.toggle_background(&ev)>
                    <svg viewBox="0 0 16 16" class="size-3.5 shrink-0 animate-spin" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true">
                        <circle cx="8" cy="8" r="6" stroke-opacity="0.3" />
                        <path d="M14 8a6 6 0 0 0-6-6" />
                    </svg>
                    <span class="tabular-nums">{n}</span>
                </button>
            })
        }}
        <div class="flex shrink-0 items-center gap-0.5">
            <Show when=move || !chrome.ssh()>
                <button class=move || icon_class(chrome.pane_open(Pane::Changes)) aria-label="Changes"
                    aria-pressed=move || chrome.pane_open(Pane::Changes).to_string()
                    title=move || match changed() {
                        Some(1) => "Changes: 1 file (⌘-click: a new tile)".to_owned(),
                        Some(n) => format!("Changes: {n} files (⌘-click: a new tile)"),
                        None => "Changes (⌘-click: a new tile)".to_owned(),
                    }
                    on:click=move |ev| chrome.toggle_pane(Pane::Changes, &ev)>
                    {crate::icons::file_diff()}
                    {move || changed().map(|n| view! { <span class="text-[10px] leading-none tabular-nums">{n}</span> })}
                </button>
            </Show>
            <button class=move || icon_class(chrome.pane_open(Pane::Terminal)) aria-label="Terminal" title="Terminal (⌘-click: a new tile)"
                aria-pressed=move || chrome.pane_open(Pane::Terminal).to_string()
                on:click=move |ev| chrome.toggle_pane(Pane::Terminal, &ev)>
                {crate::icons::square_terminal()}
            </button>
            <Show when=move || !chrome.wide.get()>
                <button id=chrome.ring_id() class=move || icon_class(glance_open()) aria-label="Glance" title=ring_title
                    aria-expanded=move || glance_open().to_string()
                    on:click=move |ev| {
                        let el = ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok());
                        chrome.toggle_glance(el);
                    }>
                    {move || context_ring(pct())}
                </button>
            </Show>
        </div>
    }
}

/// A session's own strip, when its tile's bar speaks for another view
/// beside it (a doc): its panes and its actions (⋯), at the right.
#[component]
fn SessionStrip(chrome: SessionChrome) -> impl IntoView {
    let live = api::live();
    let menu_at = RwSignal::new(None::<(f64, f64)>);
    let rename_at = RwSignal::new(None::<(f64, f64)>);
    // Renaming from here opens under this ⋯ (the tile's title is another
    // view's).
    let rename_from = StoredValue::new((8.0, 48.0));
    view! {
        <div class="flex h-9 shrink-0 items-center justify-end gap-0.5 border-b border-zinc-200 bg-zinc-50 px-2 text-xs pointer-coarse:h-11 pointer-coarse:gap-0 dark:border-zinc-800 dark:bg-zinc-900/60">
            <SessionControls chrome />
            <span class="mx-1 h-4 w-px shrink-0 bg-zinc-200 dark:bg-zinc-700"></span>
            <button class=crate::icons::BAR_BUTTON aria-label="Actions"
                title="Actions: rename, find, files, outputs, details, new like this, save as template, catch-up, fold finished turns, archive"
                on:click=move |ev| {
                    rename_from.set_value(below(&ev, 320.0));
                    menu_at.set(if menu_at.get_untracked().is_some() { None } else { Some(below(&ev, 240.0)) });
                }>{crate::icons::ellipsis()}</button>
            <Popover at=menu_at class="w-60">
                <SessionMenu chrome on_rename=move |()| rename_at.set(Some(rename_from.get_value())) on_done=move |()| menu_at.set(None) />
            </Popover>
            <Popover at=rename_at class="w-[min(22rem,calc(100vw-1rem))] p-3">
                // The name read untracked: a rebuilt form loses what was typed.
                {move || {
                    let id = chrome.id.get_value();
                    let title = live
                        .sessions
                        .with_untracked(|ss| ss.iter().find(|s| s.id == id).map(|s| s.title.clone()))
                        .or_else(|| chrome.detail.get_untracked().and_then(Result::ok).map(|d| d.session.title))
                        .unwrap_or_default();
                    view! { <RenameForm session=id title on_done=move |()| rename_at.set(None) /> }
                }}
            </Popover>
        </div>
    }
}

/// What runs in a session's background, from its bar's ◌ N: its subagents
/// and tasks running, then the last few that failed or were stopped, each
/// with its state, name and time; a task with Inspect and (while it runs)
/// Stop, a subagent going to its thread. Show all lists every one.
#[component]
fn BackgroundWork(chrome: SessionChrome, #[prop(into)] on_done: Callback<()>) -> impl IntoView {
    let col = use_column();
    let all = RwSignal::new(false);
    let now = crate::util::clock();
    let subagents = Memo::new(move |_| {
        let rows: Vec<Session> = chrome.subagents.rows().into_iter().map(|(s, _)| s).collect();
        let total = rows.len();
        (short_subagents(rows, all.get()), total)
    });
    let left_out = Memo::new(move |_| {
        let subs = chrome.subagents.rows();
        let shown = short_subagents(subs.iter().map(|(s, _)| s.clone()).collect(), false).len();
        subs.len() - shown + chrome.events.with(|e| crate::tasks::left_out(e))
    });
    let heading = "text-[10px] font-medium uppercase tracking-wide text-muted";
    view! {
        <div class="space-y-2 p-1 text-xs">
            <p class="text-muted">{move || match chrome.busy.with(Vec::len) {
                0 => "Nothing runs in the background now.".to_owned(),
                n => format!("{n} running in the background"),
            }}</p>
            {move || {
                let (rows, _) = subagents.get();
                (!rows.is_empty()).then(|| view! {
                    <div class="space-y-0.5">
                        <p class=heading>"Subagents"</p>
                        {rows.into_iter().map(|s| {
                            let running = s.status == SessionStatus::Running;
                            let (pill, tone) = subagent_state(running, s.end_reason);
                            let id = s.id.clone();
                            let (from, to) = (s.created_at, s.updated_at);
                            view! {
                                <button class="flex w-full items-center gap-2 rounded px-1 py-1.5 text-left pointer-fine:hover:bg-zinc-100 dark:pointer-fine:hover:bg-zinc-800"
                                    title="Go to it" on:click=move |_| {
                                        let own = id.clone();
                                        chrome.subagents.go(id.clone(), move || {
                                            if let Some(c) = col {
                                                c.open(View::Session { id: own, target: None }, crate::workspace::How::Beside);
                                            }
                                        });
                                        on_done.run(())
                                    }>
                                    <span class=format!("w-24 shrink-0 {tone}")>{pill}</span>
                                    <span class="min-w-0 flex-1 truncate">{s.title.clone()}</span>
                                    <span class="shrink-0 tabular-nums text-muted">{move || crate::tasks::elapsed(if running { now.get() - from } else { to - from })}</span>
                                </button>
                            }
                        }).collect_view()}
                    </div>
                })
            }}
            <crate::tasks::BackgroundTasks session=chrome.id.get_value() events=chrome.events all />
            {move || (!all.get() && left_out.get() > 0).then(|| view! {
                <button class="px-1 text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| all.set(true)>
                    {format!("Show all ({} more)", left_out.get())}
                </button>
            })}
        </div>
    }
}

/// A session's subagents for its ◌ N: those running, then the last three
/// that failed or were stopped; `all`: every one.
fn short_subagents(rows: Vec<Session>, all: bool) -> Vec<Session> {
    let (running, mut ended): (Vec<Session>, Vec<Session>) = rows.into_iter().partition(|s| s.status == SessionStatus::Running);
    if !all {
        ended.retain(|s| subagent_failed(s) || s.end_reason == Some(TurnEndReason::Interrupted));
        ended.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        ended.truncate(3);
    }
    running.into_iter().chain(ended).collect()
}

/// A sheet from the bottom on a phone (the glance, what runs in the
/// background), about half the screen, closed by a tap outside it, its ✕,
/// or pulling its handle down.
#[component]
fn BottomSheet(open: RwSignal<bool>, children: Children) -> impl IntoView {
    let close = move || open.set(false);
    let pulled_from = StoredValue::new(None::<i32>);
    view! {
        <div class="fixed inset-0 z-40 bg-black/30" on:click=move |ev| { ev.stop_propagation(); close() }></div>
        <div class="fixed inset-x-0 bottom-0 z-50 flex max-h-[50dvh] flex-col rounded-t-xl border-t border-zinc-200 bg-white text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); close() }>
            <div class="relative flex h-9 shrink-0 touch-none items-center justify-center pointer-coarse:h-12"
                on:pointerdown=move |ev: web_sys::PointerEvent| pulled_from.set_value(Some(ev.client_y()))
                on:pointerup=move |ev: web_sys::PointerEvent| {
                    if pulled_from.get_value().is_some_and(|y| ev.client_y() - y > 40) {
                        close();
                    }
                    pulled_from.set_value(None);
                }>
                <span class="h-1 w-10 rounded-full bg-zinc-300 dark:bg-zinc-600"></span>
                <button class=format!("{} absolute right-1 top-1", crate::icons::BAR_BUTTON) title="Close" aria-label="Close" on:click=move |_| close()>{crate::icons::x()}</button>
            </div>
            <div class="min-h-0 flex-1 overflow-y-auto px-2 pb-4">
                {children()}
            </div>
        </div>
    }
}

/// Where a subagent sits: the sessions above it, each linking to the call
/// that started the next.
#[component]
fn Lineage(session: Session) -> impl IntoView {
    let chain = LocalResource::new({
        let session = session.clone();
        move || {
            let session = session.clone();
            async move {
                // (id, title, the call in it that started the next one).
                let mut out: Vec<(String, String, String)> = Vec::new();
                let mut call = session.parent_call_id.clone().unwrap_or_default();
                let mut up = session.parent_id.clone();
                while let Some(p) = up.take().filter(|_| out.len() < 12) {
                    let Ok(d) = api::session(p.clone()).await else { break };
                    out.push((p, d.session.title.clone(), call.clone()));
                    call = d.session.parent_call_id.clone().unwrap_or_default();
                    up = d.session.parent_id.clone().filter(|_| d.session.parent_call_id.is_some());
                }
                out.reverse();
                out
            }
        }
    });
    let agent = (!session.agent.is_empty()).then(|| format!(" ({})", session.agent));
    view! {
        <p class="mb-3 flex flex-wrap items-center gap-1 text-sm text-muted">
            <span class="font-medium text-violet-700 dark:text-violet-400">"Subagent"</span>
            {agent}
            <span>"·"</span>
            {move || chain.get().unwrap_or_default().into_iter().map(|(id, title, call)| {
                let target = crate::workspace::Target { q: String::new(), anchor: Some(format!("call-{call}")), open: Vec::new(), at: None };
                view! {
                    <Go to=View::Session { id, target: Some(target) } class="max-w-60 truncate underline">{title}</Go>
                    <span>"›"</span>
                }
            }).collect_view()}
            <span class="max-w-60 truncate text-zinc-700 dark:text-zinc-300">{session.title.clone()}</span>
        </p>
    }
}

/// A session's subagents at any depth, each with its depth below it,
/// fetched again as they start, end or report back.
#[derive(Clone, Copy)]
pub(crate) struct SubagentTree {
    list: Memo<Vec<(Session, usize)>>,
    session: StoredValue<String>,
    /// The pane's root, where their threads are found.
    root: StoredValue<String>,
    force_open: ForceOpen,
    /// The session's events, and which of its finished turns are open (a
    /// thread under a folded turn's call opens it).
    events: RwSignal<Vec<Event>>,
    timeline: crate::timeline::Timeline,
}

impl SubagentTree {
    fn new(
        session: StoredValue<String>,
        detail: LocalResource<Result<c6_api::SessionDetail, String>>,
        events: RwSignal<Vec<Event>>,
        force_open: ForceOpen,
        root: StoredValue<String>,
        timeline: crate::timeline::Timeline,
    ) -> Self {
        let live = api::live();
        // How many work now in its whole tree (from the live session list).
        let top = Memo::new(move |_| detail.get().and_then(Result::ok).map(|d| d.session.root_id.unwrap_or(d.session.id)));
        let working = Memo::new(move |_| {
            let top = top.get();
            live.sessions.with(|ss| ss.iter().find(|s| Some(&s.id) == top.as_ref()).map_or(0, |s| s.subagents_running))
        });
        let marks = Memo::new(move |_| {
            events.with(|evs| {
                evs.iter()
                    .filter(|e| {
                        matches!(
                            e.kind,
                            EventKind::ToolResult { .. } | EventKind::SubagentSpawned { .. } | EventKind::BackgroundTask { .. }
                        )
                    })
                    .count()
            })
        });
        let tree = LocalResource::new(move || {
            marks.track();
            working.track();
            live.attention.track();
            live.connected.track();
            api::descendants(session.get_value())
        });
        let list = Memo::new(move |_| {
            let all = tree.get().and_then(Result::ok).unwrap_or_default();
            // Subagents, each with its depth below this session.
            let parent_of: HashMap<String, Option<String>> = all.iter().map(|s| (s.id.clone(), s.parent_id.clone())).collect();
            all.into_iter()
                .filter(|s| s.parent_call_id.is_some())
                .map(|s| {
                    let mut depth = 0;
                    let mut up = s.parent_id.clone();
                    let mut seen = std::collections::HashSet::new();
                    while let Some(p) = up.filter(|p| *p != session.get_value() && seen.insert(p.clone())) {
                        depth += 1;
                        up = parent_of.get(&p).cloned().flatten();
                    }
                    (s, depth)
                })
                .collect::<Vec<_>>()
        });
        Self { list, session, root, force_open, events, timeline }
    }

    /// Its subagents, each with its depth below the session.
    pub(crate) fn rows(&self) -> Vec<(Session, usize)> {
        self.list.get()
    }

    /// The titles of those that work now.
    pub(crate) fn running_titles(&self) -> Vec<String> {
        self.list.with(|l| l.iter().filter(|(s, _)| s.status == SessionStatus::Running).map(|(s, _)| s.title.clone()).collect())
    }

    /// Goes to a subagent's thread in the transcript, else the call it
    /// hangs from (a command Claude Code reports as a subagent has no
    /// thread), else `elsewhere` (its own session, say): never nothing.
    pub(crate) fn go(&self, id: String, elsewhere: impl FnOnce() + 'static) {
        self.go_to(id, None, elsewhere);
    }

    fn go_to(&self, id: String, card: Option<String>, elsewhere: impl FnOnce() + 'static) {
        // Open the threads on the way down, then the one itself.
        let mut chain = vec![id.clone()];
        let rows = self.list.get_untracked();
        let mut up = rows.iter().find(|(s, _)| s.id == id).and_then(|(s, _)| s.parent_id.clone());
        while let Some(p) = up.filter(|p| *p != self.session.get_value() && !chain.contains(p)) {
            chain.push(p.clone());
            up = rows.iter().find(|(s, _)| s.id == p).and_then(|(s, _)| s.parent_id.clone());
        }
        // The call it hangs from in this session's transcript: its turn opens.
        let call = chain.last().and_then(|top| rows.iter().find(|(s, _)| s.id == *top)).and_then(|(s, _)| s.parent_call_id.clone());
        let anchors = match card {
            Some(card) => vec![card],
            None => [Some(format!("sub-{id}")), call.clone().map(|c| format!("call-{c}"))].into_iter().flatten().collect(),
        };
        let seq = call.and_then(|call| {
            self.events.with_untracked(|evs| {
                evs.iter()
                    .rev()
                    .find(|e| match &e.kind {
                        EventKind::Assistant { blocks, .. } => {
                            blocks.iter().any(|b| matches!(b, c6_api::ContentBlock::ToolUse { id, .. } if *id == call))
                        }
                        _ => false,
                    })
                    .map(|e| e.seq)
            })
        });
        if let Some(seq) = seq {
            self.timeline.pin(seq);
        }
        self.force_open.0.update(|ids| ids.extend(chain));
        reveal_or(self.root.get_value(), anchors, 0, elsewhere);
    }
}

/// Approvals answered (allowed, denied or expired), by call.
fn decided_calls(evs: &[Event]) -> std::collections::HashSet<String> {
    evs.iter()
        .filter_map(|e| match &e.kind {
            EventKind::Approval { call_id, state, .. } if state != "requested" => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

/// Tool calls' results, by call.
fn tool_results(session: &str, evs: &[Event]) -> HashMap<String, ToolResultView> {
    evs.iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { call_id, output, is_error, truncated, duration_ms, images, .. } => Some((
                call_id.clone(),
                ToolResultView {
                    output: output.clone(),
                    is_error: *is_error,
                    truncated: *truncated,
                    duration_ms: *duration_ms,
                    images: (0..*images)
                        .map(|i| crate::api::url(&format!("/api/v1/sessions/{session}/attachments/{}/{i}", e.seq)))
                        .collect(),
                },
            )),
            _ => None,
        })
        .collect()
}

/// A session's transcript, with the side chats branching from its messages
/// shown under them (recursively). `timeline` (a session's own
/// transcript): its turns, each finished one a row while they fold.
#[component]
fn Transcript(
    session: Session,
    feed: Feed,
    running: Signal<bool>,
    #[prop(optional)] timeline: Option<crate::timeline::Timeline>,
) -> impl IntoView {
    let Feed { events, streaming, queued, edit_open, edit_rows, secrets, from, to, .. } = feed;
    let id = StoredValue::new(session.id.clone());
    let worker_id = session.worker_id;
    // Pulling a queued message (and the ones after it) into the edit box:
    // it is canceled and returned, and the box opens or takes it in. The
    // rows live in `edit_rows`, read by the box's list; the open flag is
    // what the box's mount depends on. A second pull only appends to
    // `edit_rows` (the flag is already open, so the box's body does not
    // re-run), and your in-progress edits stay.
    let pull = Callback::new(move |pulled: Vec<QueuedInput>| {
        edit_rows.update(|all| {
            let first = all.iter().map(|r| r.id).max().unwrap_or(0) + 1;
            for (i, p) in pulled.iter().enumerate() {
                all.push(QueueRow { id: first + i as u64, text: p.text.clone() });
            }
        });
        if !edit_open.get_untracked() {
            edit_open.set(true);
        }
    });
    // Find asking for an event before the window: load back to it.
    if let Some(Reach(reach)) = use_context::<Reach>() {
        Effect::new(move || {
            let Some(seq) = reach.with(|r| r.get(&id.get_value()).copied()) else { return };
            if seq < from.get() {
                feed.older(Some(seq), || ());
            } else if to.get().is_some_and(|t| seq >= t) {
                to.set(events.with_untracked(|evs| bound(evs, seq)));
            }
        });
    }
    // Scrolled up to the start of the window: the turns before it, where
    // you are staying put.
    let top = NodeRef::<leptos::html::Div>::new();
    let earlier = move || {
        let sc = top.get_untracked().and_then(|el| scroll_parent(&el));
        let height = sc.as_ref().map_or(0, |s| s.scroll_height());
        feed.older(None, move || {
            if let Some(sc) = sc {
                sc.set_scroll_top(sc.scroll_top() + sc.scroll_height() - height);
            }
        });
    };
    // Only once you've scrolled up (not while it opens at the end).
    let follow = use_context::<Follow>();
    let bottom = NodeRef::<leptos::html::Div>::new();
    on_sight(top, move || {
        if follow.is_some_and(|Follow(f)| !f.get_untracked()) {
            earlier();
        }
    });
    on_sight(bottom, move || feed.later());
    let decided = Memo::new(move |_| events.with(|evs| decided_calls(evs)));
    let results = Memo::new(move |_| events.with(|evs| tool_results(&id.get_value(), evs)));
    let usage = Memo::new(move |_| events.with(|evs| UsageStats::from_events(evs)));
    // Where you left off, while you catch up.
    let cards = crate::catchup::cards();
    let here = Memo::new(move |_| {
        cards.and_then(|c| c.with(|m| m.get(&id.get_value()).map(|c| (c.c.since_seq, c.c.since_at)))).filter(|(s, _)| *s > 0)
    });
    // Ticks for a turn's elapsed time and, while idle, the notice grace.
    let now = RwSignal::new(js_sys::Date::now() as i64);
    let ticker = set_interval_with_handle(move || now.set(js_sys::Date::now() as i64), std::time::Duration::from_secs(1)).ok();
    on_cleanup(move || {
        if let Some(t) = ticker {
            t.clear();
        }
    });

    // Child sessions: subagents (threads under their calls) and side
    // chats. Refreshed as subagents start and end, and after a side chat
    // starts.
    let refresh = RwSignal::new(0u32);
    let marks = Memo::new(move |_| {
        events.with(|evs| {
            evs.iter()
                .filter(|e| {
                    matches!(e.kind, EventKind::ToolResult { .. } | EventKind::SubagentSpawned { .. } | EventKind::BackgroundTask { .. })
                })
                .count()
        })
    });
    // And as subagents anywhere in the tree start or stop working (your own
    // messages to them leave no event here).
    let working = tree_working(session.root_id.clone().unwrap_or_else(|| session.id.clone()));
    let children = LocalResource::new(move || {
        marks.track();
        refresh.track();
        working.track();
        api::children(id.get_value())
    });
    let child_list = Memo::new(move |_| children.get().and_then(Result::ok).unwrap_or_default());
    provide_context(Subagents::new(events, child_list));

    // Side chats branch off any session; subagents are not rewound.
    let can_branch = true;
    let can_rewind = session.parent_call_id.is_none();
    // A side chat's setup (prompt, tools) is the parent's: not worth a line.
    let side = session.fork_seq.is_some();
    let asking = RwSignal::new(None::<u64>);
    // Rewinding or editing a message: (seq, edit).
    let rewinding = RwSignal::new(None::<(u64, bool)>);
    // Events no longer counting since a rewind: seq → the Rewound's seq;
    // and how many each rewind covers.
    let rewound = Memo::new(move |_| {
        events.with(|evs| {
            let mut by = HashMap::<u64, u64>::new();
            let mut counts = HashMap::<u64, usize>::new();
            for (i, e) in evs.iter().enumerate() {
                if let EventKind::Rewound { to_seq, .. } = e.kind {
                    for x in &evs[..i] {
                        if x.seq >= to_seq && !by.contains_key(&x.seq) && !matches!(x.kind, EventKind::Checkpoint { .. }) {
                            by.insert(x.seq, e.seq);
                            *counts.entry(e.seq).or_default() += 1;
                        }
                    }
                }
            }
            (by, counts)
        })
    });
    // What's drawn: the window (before it, only the summary's events), to
    // the end or where a jump stops it; while finished turns fold, each one
    // row (its events too when open), so a long session draws fewer.
    let pieces = Memo::new(move |_| {
        let (from, to) = (from.get(), to.get().unwrap_or(u64::MAX));
        events.with(|evs| {
            let shown = &evs[evs.partition_point(|e| e.seq < from)..evs.partition_point(|e| e.seq < to)];
            match timeline {
                Some(tl) if tl.fold.get() => rewound.with(|(by, _)| tl.pieces(shown, to == u64::MAX, |seq| by.contains_key(&seq))),
                _ => shown.iter().cloned().map(crate::timeline::Piece::Event).collect::<Vec<_>>(),
            }
        })
    });
    // Rewinds whose events are shown.
    let shown = RwSignal::new(std::collections::HashSet::<u64>::new());
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

    let stream_text = Memo::new(move |_| streaming.with(|s| s.text.clone()));
    let stream_thinking = Memo::new(move |_| streaming.with(|s| s.thinking.clone()));
    let stream_status = Memo::new(move |_| streaming.with(|s| s.status.clone()));
    let show_thinking = crate::settings::show_thinking();
    let has_text = Memo::new(move |_| stream_text.with(|t| !t.is_empty()));
    let has_thinking = Memo::new(move |_| show_thinking.get() && stream_thinking.with(|t| !t.is_empty()));
    // What arrives once the transcript is drawn fades in (not what it opens with).
    let arrived = Arrived(StoredValue::new(false));
    provide_context(arrived);
    Effect::new(move || {
        if feed.synced.get() {
            request_animation_frame(move || arrived.0.set_value(true));
        }
    });

    view! {
        // A long unbroken string (a path, a URL) wraps instead of widening it.
        <div class="space-y-4 [overflow-wrap:anywhere]">
            // Events are append-only: keyed by seq, each renders once, so
            // expanded cards stay open while the turn continues (a folded
            // turn's row too, as it opens and folds).
            {move || feed.more().then(|| view! {
                <div node_ref=top class="flex justify-center">
                    <button class="text-xs text-muted underline" on:click=move |_| earlier()>
                        {move || if feed.loading.get() { "Loading earlier messages…" } else { "Earlier messages" }}
                    </button>
                </div>
            })}
            <For each=move || pieces.get() key=crate::timeline::Piece::key let:piece>
                {match piece {
                    crate::timeline::Piece::Turn(span) => match timeline {
                        Some(tl) => view! { <crate::timeline::TurnRow session=id.get_value() span timeline=tl /> }.into_any(),
                        None => ().into_any(),
                    },
                    crate::timeline::Piece::Event(e) => {
                        let seq = e.seq;
                        let only_thinking = thinking_only(&e.kind);
                        let anchorable = can_branch && matches!(e.kind, EventKind::User { .. } | EventKind::Assistant { .. });
                        let user_text = match &e.kind {
                            EventKind::User { text, .. } => Some(text.clone()),
                            _ => None,
                        };
                        let is_user = user_text.is_some();
                        let by = move || rewound.with(|(by, _)| by.get(&seq).copied());
                        let hidden = move || by().is_some_and(|r| !shown.with(|s| s.contains(&r)));
                        let marker = match &e.kind {
                            EventKind::Rewound { to_seq, restored_files, restore_error } => {
                                Some((*to_seq, *restored_files, restore_error.clone()))
                            }
                            _ => None,
                        };
                        // A reply's text was already showing as it streamed; its tool cards fade in themselves.
                        let arrive = if matches!(e.kind, EventKind::Assistant { .. }) { "" } else { Arrived::class() };
                        view! {
                            <div
                                id=format!("{}-e{seq}", id.get_value())
                                class=move || format!("group relative {arrive} {} {} {}",
                                    if anchorable { "pb-7" } else { "" },
                                    if hidden() || (only_thinking && !show_thinking.get()) { "hidden" } else { "" },
                                    if by().is_some() { "opacity-50" } else { "" })
                            >
                                {match marker {
                                    Some((to_seq, restored, err)) => {
                                        let what = events.with_untracked(|evs| evs.iter().find_map(|x| match &x.kind {
                                            EventKind::User { text, .. } if x.seq == to_seq => Some(text.chars().take(60).collect::<String>()),
                                            _ => None,
                                        })).unwrap_or_default();
                                        let files = if restored { " · files restored".to_owned() } else if err.is_empty() { String::new() } else { format!(" · files not restored: {err}") };
                                        view! {
                                            <div class="flex flex-wrap items-center gap-2 rounded-md border border-dashed border-zinc-300 px-3 py-1.5 text-xs text-muted dark:border-zinc-700">
                                                <span>{crate::icons::undo()}" "{format!("Rewound to before “{what}”{files}")}</span>
                                                <span class="flex-1"></span>
                                                <button class="underline" on:click=move |_| shown.update(|s| if !s.remove(&seq) { s.insert(seq); })>
                                                    {move || {
                                                        let n = rewound.with(|(_, c)| c.get(&seq).copied().unwrap_or(0));
                                                        if shown.with(|s| s.contains(&seq)) { format!("hide {n} rewound") } else { format!("show {n} rewound") }
                                                    }}
                                                </button>
                                            </div>
                                        }.into_any()
                                    }
                                    None => (!(side && matches!(e.kind, EventKind::Started { .. }))).then(|| render_event(&id.get_value(), &e, results, decided, streaming, usage)).into_any(),
                                }}
                                {anchorable.then(|| view! {
                                    <div class="absolute bottom-0 right-0 flex gap-1 opacity-0 transition group-hover:opacity-100">
                                        {move || (can_rewind && is_user && by().is_none()).then(|| view! {
                                            <button class=UBTN title="Change this message and send it again (rewinds to before it)" on:click=move |_| rewinding.set(Some((seq, true)))>{crate::icons::pencil()}" Edit"</button>
                                            <button class=UBTN title="Rewind the conversation (and optionally files) to before this message" on:click=move |_| rewinding.set(Some((seq, false)))>{crate::icons::undo()}" Rewind"</button>
                                        })}
                                        <button class=UBTN title="Ask about this in a side chat" on:click=move |_| asking.set(Some(seq))>{crate::icons::corner_down_right()}" Ask"</button>
                                    </div>
                                })}
                            </div>
                            // Where you left off, while you catch up.
                            {move || here.get().filter(|(s, _)| *s == seq).map(|(_, at)| view! {
                                <crate::catchup::YouWereHere session=id.get_value() at />
                            })}
                            {move || rewinding.get().filter(|(s, _)| *s == seq).map(|(_, edit)| view! {
                                <RewindPanel session=id.get_value() seq text=user_text.clone().unwrap_or_default() edit on_done=Callback::new(move |()| rewinding.set(None)) />
                            })}
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
                        }.into_any()
                    }
                }}
            </For>
            {move || to.get().is_some().then(|| view! {
                <div node_ref=bottom class="flex justify-center gap-3">
                    <button class="text-xs text-muted underline" on:click=move |_| feed.later()>"Later messages"</button>
                    <button class="text-xs text-muted underline" on:click=move |_| {
                        feed.latest();
                        if let Some(Follow(f)) = follow { f.set(true) }
                    }>"Latest "{crate::icons::arrow_down()}</button>
                </div>
            })}
            <crate::decisions::SessionDecisions session=id.get_value() />
            {move || secrets.get().into_iter().map(|c| view! { <SecretCardView card=c on_dismiss=Callback::new(move |id: String| secrets.update(|s| s.retain(|x| x.call_id != id))) /> }).collect_view()}
            // What streams in: the same elements, their text growing (not
            // drawn again for every token).
            {move || (has_thinking.get() || has_text.get()).then(|| view! {
                <div class="space-y-2">
                    {move || has_thinking.get().then(|| view! { <Thinking text=stream_thinking show=show_thinking /> })}
                    {move || has_text.get().then(|| view! { <div class="md" on:click=crate::mentions::link_clicks() inner_html=move || markdown::to_html(&stream_text.get())></div> })}
                </div>
            })}
            // Through the whole turn, so the end of the page doesn't come and
            // go as text starts and stops streaming.
            {move || running.get().then(|| view! {
                <div class="flex items-center gap-2 text-sm text-muted">
                    <span class="h-2 w-2 animate-pulse rounded-full bg-blue-500"></span>
                    {move || stream_status.with(|s| if s.is_empty() { "Working…".to_owned() } else { s.clone() })}
                </div>
            })}
            {move || running.get().then(|| usage.with(|u| u.current_turn.and_then(|n| u.turns.get(&n).copied()))).flatten().map(|t| view! {
                <p class="font-mono text-[11px] text-muted">{move || format!("This turn: {}", turn_line(&t, Some(now.get())))}</p>
            })}
            {move || edit_open.get().then(|| view! { <QueueEditBox rows=edit_rows session_id=id.get_value() on_close=Callback::new(move |_| { edit_open.set(false); edit_rows.update(Vec::clear); }) /> })}
            {move || queued.get().into_iter().map(|q| view! { <Queued q session_id=id.get_value() worker_id=worker_id pull=pull /> }).collect_view()}
            {move || (!running.get()).then(|| view! { <WaitingNotices events now session_id=id.get_value() /> })}
            // What still runs in the background, while or after it works.
            <crate::tasks::RunningTasks session=id.get_value() events />
        </div>
    }
}

/// An answerable card, independent of whether its events are drawn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Waiting {
    pub(crate) what: String,
    kind: String,
    title: String,
    target: WaitingTarget,
}

#[derive(Clone, Debug, PartialEq)]
enum WaitingTarget {
    Decision(i64),
    Call { session: String, call: String, seq: Option<u64> },
}

pub(crate) fn waiting_card_id(session: &str, call: &str) -> String {
    format!("waiting-{session}-{call}")
}

fn waiting_call_seq(evs: &[Event], call: &str) -> Option<u64> {
    evs.iter().find_map(|e| match &e.kind {
        EventKind::Approval { call_id, state, .. } if call_id == call && state == "requested" => Some(e.seq),
        EventKind::Question { call_id, state, .. } if call_id == call && state == "asked" => Some(e.seq),
        _ => None,
    })
}

/// The head's open decisions are authoritative, even outside the loaded
/// event window. A loaded event gives a more precise jump within this pane.
fn waiting_on_you(session: &str, evs: &[Event], items: &[c6_api::AttentionItem]) -> Vec<Waiting> {
    items
        .iter()
        .filter_map(|i| {
            let target = match (i.kind.as_str(), &i.session_id, &i.call_id) {
                ("approval" | "question", Some(s), Some(call)) => {
                    let seq = (s == session).then(|| waiting_call_seq(evs, call)).flatten();
                    WaitingTarget::Call { session: s.clone(), call: call.clone(), seq }
                }
                _ => WaitingTarget::Decision(i.decision_id?),
            };
            let child = matches!(&target, WaitingTarget::Call { session: s, .. } if s != session);
            Some(Waiting {
                what: format!("{}: {}{}", i.kind, i.title, if child { format!(" · {}", i.detail) } else { String::new() }),
                kind: i.kind.clone(),
                title: i.detail.clone(),
                target,
            })
        })
        .collect()
}

/// One line per waiting card, in the held-notices style, outside the
/// transcript's scroll area so an old/offscreen thread can't hide it.
#[component]
fn SubagentWaiting(chrome: SessionChrome) -> impl IntoView {
    move || {
        let id = chrome.id.get_value();
        let children = chrome.waiting.with(|ws| {
            ws.iter().filter(|w| matches!(&w.target, WaitingTarget::Call { session, .. } if *session != id)).cloned().collect::<Vec<_>>()
        });
        (!children.is_empty()).then(|| view! {
            <div class=format!("{READING_ROW} shrink-0 px-4 pt-2")>
                <div class=format!("{READING} space-y-1 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-sm dark:border-amber-800 dark:bg-amber-950")>
                    {children.into_iter().map(|w| {
                        let line = format!("1 {} waiting in {}", w.kind, w.title);
                        view! {
                            <div class="flex items-center gap-3">
                                <span class="min-w-0 flex-1 truncate text-amber-800 dark:text-amber-300" title=w.what.clone()>{line}</span>
                                <button class=BUTTON on:click=move |_| chrome.go_waiting(w.clone())>"Show"</button>
                            </div>
                        }
                    }).collect_view()}
                </div>
                {glance_room(chrome.wide)}
            </div>
        })
    }
}

/// Don't flash the held-notices box while an idle agent takes its input.
const NOTICE_GRACE_MS: i64 = 3_000;

/// The notices no turn has taken after the grace (docs/background.md §5): from a session
/// message, or a background task's end or a subagent's report, each held
/// by the hourly limit or for your next message. Its first line, and who
/// sent it.
fn waiting_notices(evs: &[Event], now: i64) -> Vec<(String, String, i64)> {
    let taken: std::collections::HashSet<&str> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::User { command_id, .. } => Some(command_id.as_str()),
            _ => None,
        })
        .collect();
    evs.iter()
        .filter_map(|e| match &e.kind {
            EventKind::NoticeQueued { command_id, text, from_title, deliver_at_ms, .. }
                if !taken.contains(command_id.as_str()) && now.saturating_sub(e.ts) >= NOTICE_GRACE_MS =>
            {
                Some((from_title.clone(), text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_owned(), *deliver_at_ms))
            }
            _ => None,
        })
        .collect()
}

/// While the session is idle: the notices waiting for its agent, and a way
/// to deliver them now.
#[component]
fn WaitingNotices(events: RwSignal<Vec<Event>>, now: RwSignal<i64>, session_id: String) -> impl IntoView {
    let waiting = Memo::new(move |_| events.with(|evs| waiting_notices(evs, now.get())));
    let status = RwSignal::new(None::<String>);
    let id = StoredValue::new(session_id);
    let deliver = move |_| {
        spawn_local(async move {
            status.set(api::deliver_notices(id.get_value()).await.err());
        });
    };
    move || {
        let w = waiting.get();
        (!w.is_empty()).then(|| {
            let n = w.len();
            let delivery = w.iter().filter_map(|(_, _, at)| (*at > 0).then_some(*at)).min();
            view! {
                <div class="space-y-1 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-sm dark:border-amber-800 dark:bg-amber-950">
                    <div class="flex items-center gap-3">
                        <span class="flex-1 text-amber-800 dark:text-amber-300">
                            {if n == 1 { "1 notice waiting for the agent".to_owned() } else { format!("{n} notices waiting for the agent") }}
                            {delivery.map(|at| format!(" — delivers itself at {}", crate::when::time(at)))}
                        </span>
                        <button class=BUTTON on:click=deliver title="Start a turn with them now, whatever the hourly limit">"Deliver now"</button>
                    </div>
                    <ul class="space-y-0.5 text-xs text-zinc-600 dark:text-zinc-400">
                        {w.into_iter().map(|(from, line, _)| view! {
                            <li class="truncate">{if from.is_empty() { line } else { format!("From {from}: {line}") }}</li>
                        }).collect_view()}
                    </ul>
                    {move || status.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
                </div>
            }
        })
    }
}

/// Calls `f` as the element comes into view (or near it): scrolled to, not
/// where it is when it's drawn.
fn on_sight(el: NodeRef<leptos::html::Div>, f: impl Fn() + Clone + 'static) {
    Effect::new(move || {
        let Some(el) = el.get() else { return };
        let f = f.clone();
        let mut first = true;
        let cb = wasm_bindgen::closure::Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let seen =
                entries.iter().any(|e| wasm_bindgen::JsCast::unchecked_into::<web_sys::IntersectionObserverEntry>(e).is_intersecting());
            if seen && !std::mem::take(&mut first) {
                f();
            }
            first = false;
        });
        let opts = web_sys::IntersectionObserverInit::new();
        opts.set_root_margin("200px");
        if let Ok(o) = web_sys::IntersectionObserver::new_with_options(wasm_bindgen::JsCast::unchecked_ref(cb.as_ref()), &opts) {
            o.observe(&el);
            let kept = send_wrapper::SendWrapper::new((o, cb));
            on_cleanup(move || kept.0.disconnect());
        }
    });
}

/// Rewind to before a message, or edit it and send it again (a rewind
/// followed by the new message).
#[component]
fn RewindPanel(session: String, seq: u64, text: String, edit: bool, on_done: Callback<()>) -> impl IntoView {
    let draft = RwSignal::new(text);
    let draft_box = NodeRef::<leptos::html::Textarea>::new();
    crate::util::autosize(draft_box, move || draft.track());
    let restore = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let session = StoredValue::new(session);
    let go = move || {
        if busy.get_untracked() {
            return;
        }
        let text = draft.get_untracked();
        if edit && text.trim().is_empty() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let r = match api::rewind(session.get_value(), seq, restore.get_untracked()).await {
                Ok(()) if edit => api::send_message(session.get_value(), text, None, Vec::new()).await.map(drop),
                other => other,
            };
            busy.set(false);
            match r {
                Ok(()) => on_done.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <div class="space-y-2 rounded-lg border border-amber-200 bg-amber-50/60 p-3 text-sm dark:border-amber-900 dark:bg-amber-950/30">
            {if edit {
                view! {
                    <textarea node_ref=draft_box class=format!("{INPUT} {} min-h-[3rem]", crate::util::GROWS) prop:value=move || draft.get()
                        on:input=move |ev| draft.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if crate::util::sends(&ev) { ev.prevent_default(); go(); }
                            if ev.key() == "Escape" { on_done.run(()); }
                        }></textarea>
                }.into_any()
            } else {
                view! { <p class="text-zinc-700 dark:text-zinc-300">"Rewind the conversation to before this message? The messages after it stop counting (they stay visible, folded)."</p> }.into_any()
            }}
            <div class="flex flex-wrap items-center gap-3">
                <label class="flex items-center gap-1.5 text-xs text-zinc-600 dark:text-zinc-400" title="Files in the session's folder go back to how they were before this message. Other sessions in the same folder see the change too.">
                    <input type="checkbox" prop:checked=move || restore.get() on:change=move |ev| restore.set(event_target_checked(&ev)) />
                    "Also restore files to before this message"
                </label>
                <span class="flex-1"></span>
                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| on_done.run(())>"Cancel"</button>
                <button class=format!("{BUTTON} text-xs") prop:disabled=move || busy.get() on:click=move |_| go()>{if edit { "Resend" } else { "Rewind" }}</button>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
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
            <button class="flex w-full items-center gap-2 py-1 text-left text-xs text-muted hover:text-zinc-900 dark:hover:text-zinc-200" on:click=toggle>
                <span class="inline-block w-3 shrink-0 text-[9px] transition-transform" class:rotate-90=move || open.get()>{crate::icons::chevron_right()}</span>
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
    let feed = Feed::open(session.id.clone(), Window::Tail);
    let running = Signal::derive(move || feed.running());
    let error = RwSignal::new(None::<String>);
    let id = session.id.clone();
    let start = (session.connection_id.clone(), session.model.clone(), session.effort.clone().unwrap_or_default());
    let choice = ModelChoice::follow(feed.events, start);
    let place = (session.worker_id.to_string(), session.cwd.clone());
    let no_claude_code = session.ssh.is_some();
    Effect::new(move || {
        let (_, m, e) = choice.current.get();
        model_label.set(self::model_label(&m, &e));
    });
    view! {
        <div class="space-y-3 pb-2 pt-1">
            <Transcript session feed running />
            <Composer id running disabled=Signal::derive(|| false) error inline=true model=choice files=Signal::derive(move || Some(place.clone())) no_claude_code />
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
        // Keep incomplete choices too: sending must report them, not silently
        // leave the session on its previous connection.
        (chosen != self.current.get_untracked()).then_some(chosen)
    }
}

/// The picker for a session's subagents (docs/subagents.md §14): it
/// follows their default, its last change in the log, else the one on the
/// session's row (`start`). An empty model: the session's own.
#[derive(Clone, Copy)]
struct SubagentChoice {
    connection: RwSignal<String>,
    model: RwSignal<String>,
    effort: RwSignal<String>,
    /// (connection, model, effort) they run on by default now.
    current: Memo<(String, String, String)>,
    /// The session's own (connection, model, effort).
    own: Memo<(String, String, String)>,
}

impl SubagentChoice {
    /// `own`: the session's connection, shown while the default names none.
    fn follow(events: RwSignal<Vec<Event>>, start: (String, String, String), own: Memo<(String, String, String)>) -> Self {
        let current = Memo::new(move |_| {
            events.with(|evs| {
                evs.iter()
                    .rev()
                    .find_map(|e| match &e.kind {
                        EventKind::SubagentModelSet { connection_id, model, effort } => {
                            Some((connection_id.clone(), model.clone(), effort.clone()))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| start.clone())
            })
        });
        let choice = SubagentChoice {
            connection: RwSignal::new(String::new()),
            model: RwSignal::new(String::new()),
            effort: RwSignal::new(String::new()),
            current,
            own,
        };
        Effect::new(move || {
            let (c, m, e) = current.get();
            let (own_c, own_m, _) = own.get();
            choice.connection.set(if c.is_empty() { own_c } else { c });
            choice.model.set(if m.is_empty() { own_m } else { m });
            choice.effort.set(e);
        });
        choice
    }

    /// What to set, if the picker differs from the default now.
    fn change(&self) -> Option<c6_api::SetSubagentModel> {
        let (c, m, e) = (self.connection.get(), self.model.get().trim().to_owned(), self.effort.get());
        // Shown as they run now: the session's own when none is set.
        let (cc, cm, ce) = match self.current.get() {
            (_, cm, _) if cm.is_empty() => {
                let (oc, om, _) = self.own.get();
                (oc, om, String::new())
            }
            (cc, cm, ce) if cc.is_empty() => (self.own.get().0, cm, ce),
            now => now,
        };
        (!m.is_empty() && (c != cc || m != cm || e != ce)).then_some(c6_api::SetSubagentModel { connection_id: c, model: m, effort: e })
    }
}

/// Claude Code can choose its own model; API connections need an explicit one.
fn picker_model(kind: &str, default: Option<String>) -> String {
    default.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| if kind == "claude_code" { "default".to_owned() } else { String::new() })
}

/// Connection select plus a model field that lists the connections' models
/// and also takes free text. `no_claude_code`: leave Claude Code out (it
/// cannot run there, e.g. over SSH or for subagents). `auto`: offer Auto
/// (connection "": your routing rules choose) while it says so, or while
/// Auto is chosen; on Auto there's no model to type.
#[component]
pub(crate) fn ModelPicker(
    connection: RwSignal<String>,
    model: RwSignal<String>,
    effort: RwSignal<String>,
    #[prop(optional)] no_claude_code: bool,
    /// Why Claude Code can't run here (Ask on a head without `claude`):
    /// its connections are shown, not chosen.
    #[prop(optional)]
    claude_off: Option<Signal<Option<String>>>,
    #[prop(optional, into)] auto: Option<Signal<bool>>,
) -> impl IntoView {
    let connections = LocalResource::new(api::connections);
    // Effort only where the connection passes it on to the model.
    let supports_effort =
        move |id: &str| connections.get().and_then(Result::ok).is_some_and(|cs| cs.iter().any(|c| c.id == id && c.supports_effort));
    view! {
        <select class=format!("{SMALL_INPUT} max-w-48") on:change=move |ev| {
            let c = event_target_value(&ev);
            let chosen = connections.get().and_then(Result::ok).and_then(|cs| cs.into_iter().find(|x| x.id == c));
            let default = chosen.map(|x| picker_model(&x.kind, x.default_model)).unwrap_or_default();
            if !supports_effort(&c) {
                effort.set(String::new());
            }
            connection.set(c);
            model.set(default);
        }>
            {move || auto.filter(|a| a.get() || connection.with(String::is_empty)).map(|_| view! {
                <option value="" selected=move || connection.with(String::is_empty)>"Auto (your routing rules)"</option>
            })}
            {move || connections.get().and_then(Result::ok).unwrap_or_default().into_iter()
                .filter(|c| !(no_claude_code && c.kind == "claude_code") || connection.get_untracked() == c.id)
                .map(|c| {
                let id = c.id.clone();
                let off = (c.kind == "claude_code").then(|| claude_off.and_then(|o| o.get())).flatten();
                let label = match &off {
                    Some(why) => format!("{} — {why}", c.name),
                    None => crate::quota::option_label(&c.id, &c.name),
                };
                view! { <option value=c.id.clone() disabled=off.is_some() selected=move || connection.get() == id>{label}</option> }
            }).collect_view()}
        </select>
        <crate::quota::QuotaNote connection />
        <div class="w-56 max-w-full" class:hidden=move || auto.is_some() && connection.with(String::is_empty)><ModelInput connection=connection value=model on_set=move |v| model.set(v) class=SMALL_INPUT.to_owned() placeholder="model id" /></div>
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

/// What a switch between Claude Code and carbon6's own agent does
/// (docs/architecture.md, "Model switching").
const HANDOFF_HELP: &str = "Claude Code and carbon6's agent are different runtimes, each with its own tools, system prompt and memory of \
    the conversation. Switching between them hands the session over: it stays this session, with its log, but the next turn runs on the \
    other runtime, which gets a transcript of the conversation so far (your messages, the replies, and tool calls with their output, long \
    ones shortened) as context. Hidden reasoning, provider-side state and unfinished tool calls do not carry over, and you cannot rewind to \
    before the handoff.";

const SMALL_INPUT: &str =
    "rounded-md border border-zinc-300 bg-white px-2 py-1 text-xs text-zinc-900 dark:border-zinc-600 dark:bg-zinc-900 dark:text-zinc-100";

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
    crate::util::autosize(input, move || text.track());
    Effect::new(move || {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });
    view! {
        <div class="ml-2 space-y-2 rounded-lg border border-sky-200 bg-sky-50/60 p-3 dark:border-sky-900 dark:bg-sky-950/30">
            <textarea
                node_ref=input
                class=format!("{INPUT} {} min-h-[2.75rem]", crate::util::GROWS)
                rows="2"
                placeholder=if crate::util::touch_first() { "Ask about this in a side chat" } else { "Ask about this in a side chat (Enter to send, Esc to cancel)" }
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if crate::util::sends(&ev) { ev.prevent_default(); send(); }
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

/// The message box's Send: a small soft square (`rounded-full` makes it a
/// circle), quiet while there's nothing to send, sky once there is.
pub(crate) const SEND_SHAPE: &str = "rounded-md";
pub(crate) const SEND: &str = "flex size-5.5 shrink-0 items-center justify-center disabled:opacity-50";
pub(crate) const SEND_EMPTY: &str = "bg-zinc-600 text-zinc-200";
pub(crate) const SEND_READY: &str = "bg-sky-600 text-white hover:bg-sky-700";

/// One Esc pressed while the agent runs (when, in ms): a second within
/// 1.5s stops it, and until then Stop reads "Esc".
#[derive(Clone, Copy)]
struct EscArmed(RwSignal<Option<i64>>);

/// Stop (and Esc twice) while Claude Code's own agents run in the
/// background: an interrupt stops them too, so Stop asks first
/// (docs/background.md §4). Send now spares them.
#[derive(Clone, Copy)]
struct StopGuard {
    /// How many run now.
    agents: Memo<usize>,
    /// The question is open, by the Stop button.
    confirm: RwSignal<bool>,
}

/// What Stop asks while `n` background agents run.
fn stop_question(n: usize) -> String {
    let agents = if n == 1 { "1 background agent".to_owned() } else { format!("{n} background agents") };
    format!("This also stops {agents}. Stop anyway?")
}

/// Stop: a thin ring, a small square in it.
const STOP: &str = "flex size-5 shrink-0 items-center justify-center rounded-full border-[1.2px] border-zinc-400 text-muted hover:border-zinc-500 hover:text-zinc-500 disabled:opacity-50";

/// Send's icon, an arrow up: its one path, to change it.
const SEND_ICON: &str = "M8 13V3M3.5 7.5 8 3l4.5 4.5";

pub(crate) fn send_icon() -> impl IntoView {
    view! {
        <svg viewBox="0 0 16 16" class="h-3 w-3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d=SEND_ICON />
        </svg>
    }
}

fn stop_icon() -> impl IntoView {
    view! {
        <svg viewBox="0 0 16 16" class="h-2 w-2" fill="currentColor" aria-hidden="true">
            <rect x="1" y="1" width="14" height="14" rx="2.5" />
        </svg>
    }
}

/// The tile bar's ⋯ menu: an item, the hint after it, and the line
/// between groups.
const MENU_ITEM: &str = "flex w-full items-center gap-3 rounded px-2 py-1 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800";
const MENU_HINT: &str = "ml-auto text-[11px] text-muted";
const MENU_RULE: &str = "my-1 border-t border-zinc-200 dark:border-zinc-700";

/// The message box's model chip: quiet, grey text with a background only on
/// hover, so the toolbar doesn't compete with what you're typing.
const CHIP: &str = "inline-flex h-6 max-w-full items-center gap-1 rounded-md px-1.5 text-muted hover:bg-zinc-100 hover:text-zinc-800 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";

/// A small on/off switch (the look of the document comments' "Send to agent").
#[component]
fn Switch(
    #[prop(into)] on: Signal<bool>,
    on_change: impl Fn(bool) + 'static,
    label: &'static str,
    #[prop(optional, into)] title: String,
) -> impl IntoView {
    view! {
        <button type="button" role="switch" aria-checked=move || on.get().to_string() title=title
            class="inline-flex items-center gap-1.5 rounded text-zinc-600 dark:text-zinc-400" on:click=move |_| on_change(!on.get_untracked())>
            <span class=move || format!("relative h-3.5 w-[26px] shrink-0 rounded-full transition-colors {}", if on.get() { "bg-sky-600" } else { "bg-zinc-300 dark:bg-zinc-600" })>
                <span class=move || format!("absolute left-0.5 top-0.5 h-2.5 w-2.5 rounded-full bg-white shadow-sm transition-transform {}", if on.get() { "translate-x-3" } else { "" })></span>
            </span>
            {label}
        </button>
    }
}

const SUBAGENTS_HELP: &str = "The agent starts a subagent with its Task tool when it decides a piece of work suits one (or you ask). This is the connection and model they run on, unless a call or a named agent asks for another. carbon6 runs them itself, whatever this session runs on.";

/// A model change waiting for Send, as (what it does, more about it).
fn switch_note(m: ModelChoice, kinds: Vec<c6_api::Connection>) -> Option<(AnyView, String)> {
    let _ = (m.connection.get(), m.model.get(), m.effort.get(), m.current.get());
    let (to, model, effort) = m.switch()?;
    if to.trim().is_empty() || model.is_empty() {
        return Some((view! { "choose a connection and model before sending" }.into_any(), String::new()));
    }
    // Into or out of Claude Code: a handoff, not a switch.
    let is_cc = |id: &str| kinds.iter().any(|c| c.id == id && c.kind == "claude_code");
    let (from_cc, to_cc) = (is_cc(&m.current.get_untracked().0), is_cc(&to));
    if from_cc == to_cc {
        return Some((
            view! { {crate::icons::arrow_right()}" "{format!("{} on send", model_label(&model, &effort))} }.into_any(),
            String::new(),
        ));
    }
    let (to, from) = if to_cc { ("Claude Code", "carbon6's agent") } else { ("carbon6's agent", "Claude Code") };
    Some((
        view! { {format!("hands over to {to} on send")}" "{crate::icons::info()} }.into_any(),
        format!("It gets a transcript of the conversation, not {from}'s reasoning or tool state. {HANDOFF_HELP}"),
    ))
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
    let (reach, follow) = (use_context::<Reach>(), use_context::<Follow>());
    let timeline = use_context::<crate::timeline::Timeline>();
    let go = move |i: usize| {
        let Some(m) = found.get_untracked() else { return };
        let Some(hit) = m.matches.get(i).cloned() else { return };
        current.set(Some(i));
        // Its transcripts load back to it, if they start after it; you're
        // looking there now, not at the end.
        if let Some(Follow(f)) = follow {
            f.set(false);
        }
        if let Some(Reach(r)) = reach {
            r.update(|r| {
                r.insert(session.get_value(), hit.at);
                if let Some(own) = hit.open.last() {
                    r.insert(own.clone(), hit.seq);
                }
            });
        }
        // Its turn opens, while finished turns fold.
        if let Some(tl) = timeline {
            tl.pin(hit.at);
        }
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
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key().eq_ignore_ascii_case("g")
            && (ev.meta_key() || ev.ctrl_key())
            && col.is_none_or(|c| c.active.try_get_untracked() == Some(true))
        {
            ev.prevent_default();
            step(if ev.shift_key() { -1 } else { 1 });
        }
    });
    on_cleanup(move || keys.remove());
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
            <span class="min-w-16 px-1 text-center text-xs tabular-nums text-muted" title=move || error.get().unwrap_or_default()>
                {move || match (error.get(), found.get()) {
                    (Some(_), _) => "—".to_owned(),
                    (None, Some(m)) if m.matches.is_empty() => "no matches".to_owned(),
                    (None, Some(m)) => format!("{} of {}{}", current.get().map_or(0, |i| i + 1), m.matches.len(), if m.truncated { "+" } else { "" }),
                    (None, None) => String::new(),
                }}
            </span>
            <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs") title="Previous match (Shift+Enter, ⇧⌘G)" on:click=move |_| step(-1)>{crate::icons::arrow_up()}</button>
            <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs") title="Next match (Enter, ⌘G)" on:click=move |_| step(1)>{crate::icons::arrow_down()}</button>
            <button class=format!("{BUTTON_QUIET} px-2 py-1 text-xs text-muted") title="Close (Esc)" on:click=move |_| close()>{crate::icons::x()}</button>
        </div>
    }
}

/// Where drawing a log from event `at` stops: one of your messages about
/// [`TAIL`] events on; None: at the end.
fn bound(evs: &[Event], at: u64) -> Option<u64> {
    let i = evs.partition_point(|e| e.seq < at) + TAIL as usize;
    evs.get(i..)?.iter().find(|e| matches!(e.kind, EventKind::User { .. })).map(|e| e.seq)
}

/// The session's worker is away, so it can't run there now. Never for one
/// the head runs itself (the assistant): it has no worker.
pub(crate) fn worker_offline(s: &Session, workers: &[c6_api::Worker]) -> bool {
    !s.on_head && !workers.iter().any(|w| w.id == s.worker_id && w.status == c6_api::WorkerStatus::Online)
}

/// The nearest ancestor that scrolls.
fn scroll_parent(el: &web_sys::Element) -> Option<web_sys::Element> {
    let mut at = el.parent_element();
    while let Some(e) = at {
        let style = window().get_computed_style(&e).ok().flatten();
        if style.and_then(|s| s.get_property_value("overflow-y").ok()).is_some_and(|o| o == "auto" || o == "scroll") {
            return Some(e);
        }
        at = e.parent_element();
    }
    None
}

/// The event an anchor of this session names (`e<seq>`, `<session>-e<seq>`).
fn anchor_seq(anchor: &str, session: &str) -> Option<u64> {
    let seq = anchor.strip_prefix(session).and_then(|a| a.strip_prefix('-')).unwrap_or(anchor);
    seq.strip_prefix('e')?.parse().ok()
}

/// Sessions' transcripts asked to load back to an event (by Find), by
/// session: each loads its own.
#[derive(Clone, Copy)]
struct Reach(RwSignal<HashMap<String, u64>>);

/// The page follows the end of the transcript (you haven't scrolled up).
#[derive(Clone, Copy)]
struct Follow(RwSignal<bool>);

/// The transcript has drawn what it opened with: what is drawn now has
/// just arrived (and fades in).
#[derive(Clone, Copy)]
struct Arrived(StoredValue<bool>);

impl Arrived {
    /// The class for something drawn now: `c6-arrive` once the transcript
    /// is drawn.
    fn class() -> &'static str {
        if use_context::<Arrived>().is_some_and(|a| a.0.get_value()) { "c6-arrive" } else { "" }
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

/// Reveals the first of `ids` that exists, trying for about a second and a
/// half (a thread opens, a turn unfolds), else does `elsewhere`.
fn reveal_or(root: String, ids: Vec<String>, tries: u32, elsewhere: impl FnOnce() + 'static) {
    set_timeout(
        move || {
            if ids.iter().any(|id| js_reveal(&root, id, "[]")) {
                return;
            }
            if tries < 30 {
                reveal_or(root, ids, tries + 1, elsewhere);
            } else {
                elsewhere();
            }
        },
        std::time::Duration::from_millis(50),
    );
}

/// Reveals the first of the elements `pick` names (asked again each try:
/// what they are may be known only once events load) that exists.
fn reveal_picked(root: String, pick: impl Fn() -> Vec<String> + 'static, tries: u32) {
    set_timeout(
        move || {
            if !pick().iter().any(|id| js_reveal(&root, id, "[]")) && tries < 200 {
                reveal_picked(root, pick, tries + 1);
            }
        },
        std::time::Duration::from_millis(50),
    );
}

pub(crate) fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// The files a paste, drop or picker brought (`ev`), added to `attachments`
/// within the head's limits; else why not, in `error`.
pub(crate) fn add_attachments(ev: web_sys::Event, attachments: RwSignal<Vec<c6_api::AttachmentUpload>>, error: RwSignal<Option<String>>) {
    spawn_local(async move {
        match js_read_files(&ev).await.ok().and_then(|v| v.as_string()) {
            Some(json) => match serde_json::from_str::<Vec<c6_api::AttachmentUpload>>(&json) {
                Ok(files) => match too_large(&attachments.get_untracked(), &files) {
                    Some(e) => error.set(Some(e)),
                    None => attachments.update(|a| a.extend(files)),
                },
                Err(e) => error.set(Some(e.to_string())),
            },
            None => error.set(Some("could not read the file".into())),
        }
    });
}

/// Why `adding` can't join the attachments already `held`, if it can't: the
/// head takes 10 MB a file and 20 MB a message (c6-head sessions.rs).
pub(crate) fn too_large(held: &[c6_api::AttachmentUpload], adding: &[c6_api::AttachmentUpload]) -> Option<String> {
    const MB: usize = 1024 * 1024;
    let size = |a: &c6_api::AttachmentUpload| a.data.trim().len() / 4 * 3;
    let mb = |n: usize| format!("{:.1} MB", n as f64 / MB as f64);
    if let Some(a) = adding.iter().find(|a| size(a) > 10 * MB) {
        return Some(format!("{} is too large to attach ({}; the limit is 10 MB a file)", a.name, mb(size(a))));
    }
    let total: usize = held.iter().chain(adding).map(size).sum();
    (total > 20 * MB).then(|| format!("Too much to send in one message ({}; the limit is 20 MB in all)", mb(total)))
}

/// A message from another session's agent: not a bubble of yours but a card
/// on the left in the sender's mark. It says what it is, names the sender
/// as a link, leads with the message's first line (agents are asked to make
/// it say what it's about), and folds the rest; what the agent was given
/// sits behind a disclosure.
#[component]
fn SessionMessage(from: String, title: String, text: String, prompt: String) -> impl IntoView {
    let mark = Memo::new({
        let from = from.clone();
        move |_| crate::marks::of_id(&from)
    });
    let (subject, rest) = subject_of(&text);
    let open = RwSignal::new(false);
    let name = if title.is_empty() { "another session".to_owned() } else { title };
    view! {
        <div class="flex flex-col items-start">
            <div class="max-w-[85%] space-y-1.5 rounded-r-md border-l-4 bg-zinc-50 px-3 py-2 text-sm dark:bg-zinc-900/60"
                style=move || format!("border-left-color:{}", mark.get().map_or("#a1a1aa", |m| crate::marks::colour_hex(&m)))>
                <p class="text-[11px] font-medium uppercase tracking-wide text-sky-700 dark:text-sky-400">{crate::icons::arrow_down_right()}" Message from another session"</p>
                <Go to=View::session(from.clone()) stop=true
                    class="inline-flex max-w-full items-center gap-1.5 rounded-md border border-sky-300 px-2 py-0.5 text-xs text-sky-800 hover:bg-sky-50 dark:border-sky-800 dark:text-sky-300 dark:hover:bg-sky-950"
                    title="Open the session that sent it">
                    {move || mark.get().map(|m| view! { <span class="h-2.5 w-2.5 shrink-0 rounded-full" style=crate::marks::fill(&m)></span> })}
                    <span class="truncate font-medium">{name}</span>
                    <span class="shrink-0">{crate::icons::square_arrow_out()}</span>
                </Go>
                <p class="font-medium text-zinc-900 dark:text-zinc-100">{subject}</p>
                {rest.map(|rest| view! {
                    {move || open.get().then(|| view! { <div class="whitespace-pre-wrap text-zinc-800 dark:text-zinc-200">{rest.clone()}</div> })}
                    <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| open.update(|o| *o = !*o)>
                        {move || if open.get() { "Show less" } else { "Show the full message" }}
                    </button>
                })}
                {(!prompt.is_empty()).then(|| view! {
                    <details class="text-xs text-muted">
                        <summary class="cursor-pointer select-none">"What the agent received"</summary>
                        <pre class="mt-1 max-h-80 overflow-auto whitespace-pre-wrap rounded-md bg-zinc-100 p-3 font-mono text-[11px] dark:bg-zinc-900">{prompt}</pre>
                    </details>
                })}
            </div>
        </div>
    }
}

/// A message's lead and the rest: its first line, cut at a sentence's end
/// (or with …) when that line runs past 160 characters.
fn subject_of(text: &str) -> (String, Option<String>) {
    let text = text.trim();
    let (first, after) = text.split_once('\n').map_or((text, ""), |(a, b)| (a.trim(), b.trim()));
    let (lead, spill) = if first.chars().count() <= 160 {
        (first.to_owned(), String::new())
    } else {
        let head: String = first.chars().take(160).collect();
        match head.rfind(". ") {
            Some(i) if i > 40 => (head[..=i].to_owned(), first[i + 1..].trim().to_owned()),
            _ => (format!("{}…", head.trim_end()), first.to_owned()),
        }
    };
    let rest = [spill.as_str(), after].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n");
    (lead, (!rest.is_empty()).then_some(rest))
}

/// A button on the dark backdrop behind a full-size image.
const FULL_BUTTON: &str =
    "shrink-0 whitespace-nowrap rounded-md bg-zinc-800 px-3 py-1.5 text-zinc-100 ring-1 ring-zinc-600 hover:bg-zinc-700";

/// An image full size over the page, focused so Esc closes it, as Close or
/// a click outside it does. `info` (its size, say) shows by its name, and
/// `children` (more buttons) before Close. `fill`: as big as the screen
/// allows, even past its own size (a drawing, sharp at any size).
#[component]
pub(crate) fn FullImage(
    src: String,
    name: String,
    #[prop(optional, into)] info: String,
    #[prop(optional)] fill: bool,
    #[prop(into)] close: Callback<()>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let el = NodeRef::<leptos::html::Div>::new();
    Effect::new(move || {
        if let Some(el) = el.get() {
            let _ = el.focus();
        }
    });
    view! {
        <div node_ref=el tabindex="-1" class="fixed inset-0 z-50 flex flex-col items-center justify-center gap-3 bg-black/90 p-4 outline-none backdrop-blur-sm"
            on:click=move |_| close.run(())
            on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); close.run(()) }>
            <img class=if fill { "h-[80dvh] w-[92vw] rounded-md bg-white object-contain shadow-2xl" } else { "max-h-[80dvh] max-w-[92vw] rounded-md object-contain shadow-2xl" } src=src alt=name.clone() on:click=|ev| ev.stop_propagation() />
            // The name gets its own line (cut short), so the buttons never wrap
            // or run into it on a phone.
            <div class="flex w-full max-w-[92vw] flex-col items-center gap-2 text-sm text-zinc-100" on:click=|ev| ev.stop_propagation()>
                <div class="flex min-w-0 max-w-full items-baseline gap-2">
                    <span class="truncate">{name}</span>
                    {(!info.is_empty()).then(|| view! { <span class="shrink-0 text-zinc-400">{info}</span> })}
                </div>
                <div class="flex items-center gap-2">
                {children.map(|c| c())}
                <button class=FULL_BUTTON title="Close (Esc)" on:click=move |_| close.run(())>"Close"</button>
                </div>
            </div>
        </div>
    }
}

/// An image sent in the transcript: a click shows it full size, and ⌘-,
/// Ctrl-, Shift- or middle-click opens it in a new tab as a link would.
#[component]
fn TranscriptImage(src: String, name: String) -> impl IntoView {
    let open = RwSignal::new(false);
    let (full, title) = (src.clone(), name.clone());
    let (href, hover) = (src.clone(), name.clone());
    view! {
        <a href=href target="_blank" title=hover
            on:click=move |ev: web_sys::MouseEvent| {
                if ev.button() == 0 && !(ev.meta_key() || ev.ctrl_key() || ev.shift_key() || ev.alt_key()) {
                    ev.prevent_default();
                    open.set(true);
                }
            }>
            <img src=src alt=name class="max-h-40 max-w-60 rounded-lg border border-zinc-300 object-cover dark:border-zinc-700" />
        </a>
        {move || open.get().then(|| {
            let href = full.clone();
            view! {
                <FullImage src=full.clone() name=title.clone() close=move |_| open.set(false)>
                    <a class=FULL_BUTTON href=href target="_blank" on:click=move |_| open.set(false)>"Open in new tab"</a>
                </FullImage>
            }
        })}
    }
}

/// A file sent in the transcript (pictures use `TranscriptImage`).
fn transcript_file(src: String, name: String, size: u64) -> AnyView {
    view! {
        <a href=src target="_blank" title=format!("{size} bytes")
            class="flex items-center gap-1 rounded-md border border-zinc-300 bg-white px-2 py-1 font-mono text-xs text-zinc-600 hover:text-zinc-900 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400 dark:hover:text-zinc-100">
            {crate::icons::file()}{name}
        </a>
    }.into_any()
}

/// An agent's notice, independent of the rest of the event log.
fn notice_line(title: &str, message: &str, priority: &str) -> AnyView {
    let tone = if priority == "high" {
        "border-amber-300 bg-amber-50 dark:border-amber-900 dark:bg-amber-950"
    } else {
        "border-zinc-200 dark:border-zinc-800"
    };
    view! {
        <div class=format!("rounded-md border px-3 py-2 text-sm {tone}")>
            <p class="flex items-center gap-2 font-medium"><span>{crate::icons::bell()}</span><span>{title.to_owned()}</span></p>
            {(!message.is_empty()).then(|| view! { <p class="mt-1 whitespace-pre-wrap text-xs text-zinc-600 dark:text-zinc-400">{message.to_owned()}</p> })}
        </div>
    }.into_any()
}

/// A memory change keeps its brain icon beside what happened.
fn memory_line(op: &str, id: &str, scope: &str, text: &str) -> AnyView {
    let what = if op == "forget" {
        format!("Forgot note {id}")
    } else {
        format!("Remembered ({}): {text}", if scope.is_empty() { "every session" } else { scope })
    };
    view! { <p class="flex items-start gap-2 text-xs text-muted"><span>{crate::icons::brain()}</span><span class="whitespace-pre-wrap">{what}</span></p> }.into_any()
}

fn render_event(
    session: &str,
    e: &Event,
    results: Memo<HashMap<String, ToolResultView>>,
    decided: Memo<std::collections::HashSet<String>>,
    streaming: RwSignal<Streaming>,
    usage: Memo<UsageStats>,
) -> AnyView {
    match &e.kind {
        EventKind::Started { system_prompt, tools, .. } => view! {
            <details class="text-xs text-muted">
                <summary class="cursor-pointer select-none">{format!("Session started · {} tools", tools.len())}</summary>
                <pre class="mt-2 max-h-96 overflow-auto whitespace-pre-wrap rounded-md bg-zinc-100 p-3 font-mono text-[11px] dark:bg-zinc-900">{system_prompt.clone()}</pre>
            </details>
        }
        .into_any(),
        EventKind::Output { name, title, mime, size } => crate::outputs::published_line(session, e.seq, name, title, mime, *size),
        EventKind::Approval { call_id, state, tool, input, policy, message } => match state.as_str() {
            "requested" => {
                // Open until the log says what became of it.
                let call = call_id.clone();
                let open = Signal::derive(move || !decided.with(|d| d.contains(&call)) && !results.with(|r| r.contains_key(&call)));
                view! { <ApprovalCard session=session.to_owned() call_id=call_id.clone() tool=tool.clone() input=input.clone() policy=policy.clone() message=message.clone() open /> }.into_any()
            }
            s => {
                let (icon, what, tone) = match s {
                    "allowed" => (crate::icons::check().into_any(), "Approved", "text-emerald-700 dark:text-emerald-400"),
                    "denied" => (crate::icons::x().into_any(), "Denied", "text-red-600 dark:text-red-400"),
                    _ => (crate::icons::hourglass().into_any(), "No answer: not run", "text-muted"),
                };
                view! { <p class=format!("flex items-center gap-2 text-xs {tone}")><span>{icon}</span><span>{what}</span></p> }.into_any()
            }
        },
        EventKind::Question { call_id, title, fields, state, answer } => match state.as_str() {
            "asked" => {
                let call = call_id.clone();
                let open = Signal::derive(move || results.with(|r| !r.contains_key(&call)));
                view! { <QuestionCard session=session.to_owned() call_id=call_id.clone() title=title.clone() fields=fields.clone() open /> }.into_any()
            }
            "answered" => {
                let shown: Vec<String> = fields
                    .iter()
                    .filter_map(|f| {
                        let v = &answer[&f.name];
                        if v.is_null() {
                            return None;
                        }
                        let text = match v {
                            serde_json::Value::String(s) => f.choices.iter().find(|c| &c.value == s).map(|c| c.label.clone()).unwrap_or_else(|| s.clone()),
                            other => other.to_string(),
                        };
                        Some(format!("{}: {text}", if f.label.is_empty() { &f.name } else { &f.label }))
                    })
                    .collect();
                view! { <p class="flex items-start gap-2 text-xs text-muted"><span>{crate::icons::pencil()}</span><span>{format!("Answered “{title}” · {}", shown.join(" · "))}</span></p> }.into_any()
            }
            _ => view! { <p class="flex items-center gap-2 text-xs text-muted"><span>{crate::icons::x()}</span><span>{format!("Not answered: {title}")}</span></p> }.into_any(),
        },
        EventKind::Notice { title, message, priority } => notice_line(title, message, priority),
        EventKind::Memory { op, id, scope, text } => memory_line(op, id, scope, text),
        EventKind::BackgroundTask { task_id, kind, description, state, exit_code, elapsed_ms, .. } => {
            crate::tasks::ended_line(task_id, kind, description, state, *exit_code, *elapsed_ms).unwrap_or_else(|| ().into_any())
        }
        // What the agent was told about a task: folded.
        EventKind::User { text, command, .. } if command == "task-notification" => view! {
            <details class="text-xs text-muted">
                <summary class="cursor-pointer select-none">{crate::icons::corner_down_right()}" the agent was told"</summary>
                <pre class="mt-1 max-h-60 overflow-auto whitespace-pre-wrap rounded-md bg-zinc-100 p-2 font-mono text-[11px] dark:bg-zinc-900">{text.clone()}</pre>
            </details>
        }
        .into_any(),
        EventKind::User { text, command, .. } if command == "stop-hook" => view! {
            <p class="flex items-start gap-2 text-xs text-amber-700 dark:text-amber-400"><span>{crate::icons::rotate()}</span><span class="whitespace-pre-wrap">{text.clone()}</span></p>
        }
        .into_any(),
        // From another session's agent (docs/background.md §6).
        EventKind::User { text, prompt, from_session_id, from_title, .. } if !from_session_id.is_empty() => view! {
            <SessionMessage from=from_session_id.clone() title=from_title.clone() text=text.clone() prompt=prompt.clone() />
        }
        .into_any(),
        EventKind::User { text, attachments, command, prompt, blocked, delegated_by, .. } => {
            let url = |i: usize| crate::api::url(&format!("/api/v1/sessions/{session}/attachments/{}/{i}", e.seq));
            // In a subagent: who sent it.
            let from = if !delegated_by.is_empty() {
                Some((crate::icons::arrow_down_right().into_any(), "from the agent that started it"))
            } else if command == "resume" {
                Some((crate::icons::rotate().into_any(), "resumed by you"))
            } else {
                None
            };
            let delegated = !delegated_by.is_empty();
            let shown = attachments
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    if a.mime.starts_with("image/") {
                        view! { <TranscriptImage src=url(i) name=a.name.clone() /> }.into_any()
                    } else {
                        transcript_file(url(i), a.name.clone(), a.size)
                    }
                })
                .collect_view();
            view! {
                <div class="flex flex-col items-end gap-1.5">
                    {from.map(|(icon, text)| view! { <p class="text-[11px] text-violet-700 dark:text-violet-400">{icon}" "{text}</p> })}
                    {(!attachments.is_empty()).then(|| view! { <div class="flex max-w-[85%] flex-wrap justify-end gap-1.5">{shown}</div> })}
                    {(!text.is_empty()).then(|| view! {
                        <div class=format!("max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-sm bg-zinc-200 px-4 py-2 text-sm dark:bg-zinc-800 {}", if blocked.is_empty() { "" } else { "line-through opacity-60" })>{text.clone()}</div>
                    })}
                    {(!blocked.is_empty()).then(|| view! {
                        <p class="max-w-[85%] text-right text-xs text-red-600 dark:text-red-400">{format!("Not sent: a UserPromptSubmit hook blocked it. {blocked}")}</p>
                    })}
                    {(!prompt.is_empty()).then(|| view! {
                        <details class="max-w-[85%] text-xs text-muted">
                            <summary class="cursor-pointer select-none text-right">{if delegated { "sent with the subagent note".to_owned() } else if command.is_empty() { "sent with hook context".to_owned() } else { format!("/{command} expanded to") }}</summary>
                            <pre class="mt-1 max-h-80 overflow-auto whitespace-pre-wrap rounded-md bg-zinc-100 p-3 font-mono text-[11px] dark:bg-zinc-900">{prompt.clone()}</pre>
                        </details>
                    })}
                </div>
            }
            .into_any()
        }
        EventKind::Assistant { blocks, stop_reason, .. } => {
            let interrupted = stop_reason == "interrupted";
            view! {
                <div class="space-y-2">
                    {blocks.iter().map(|b| match b {
                        ContentBlock::ServerTool { label } => view! {
                            <p class="flex items-center gap-2 text-xs text-muted"><span>{crate::icons::search()}</span><span class="font-mono">{label.clone()}</span></p>
                        }.into_any(),
                        ContentBlock::Text { text } => view! { <div class="md" on:click=crate::mentions::link_clicks() inner_html=markdown::to_html(text)></div> }.into_any(),
                        ContentBlock::Thinking { text } => view! { <Thinking text=text.clone() show=crate::settings::show_thinking() /> }.into_any(),
                        ContentBlock::ToolUse { id, name, input_json } => {
                            // Memos: a card redraws for its own result and output, not
                            // for every other call's (or every streamed token).
                            let (out_id, res_id) = (id.clone(), id.clone());
                            let live_output = Memo::new(move |_| streaming.with(|s| s.tool_output.get(&out_id).cloned()));
                            let result = Memo::new(move |_| results.with(|r| r.get(&res_id).cloned()));
                            view! { <ToolCard name=name.clone() input_json=input_json.clone() call_id=id.clone() result=result.into() live_output=live_output.into() /> }.into_any()
                        }
                    }).collect_view()}
                    {interrupted.then(|| view! { <p class="text-xs italic text-muted">"(response cut off)"</p> })}
                </div>
            }
            .into_any()
        }
        EventKind::Error { message } => view! { <ErrorBox message=message.clone() /> }.into_any(),
        EventKind::ModelSwitched { model, handoff: None, .. } => view! {
            <p class="flex items-center gap-2 text-xs text-muted">
                <span class="h-px flex-1 bg-zinc-200 dark:bg-zinc-800"></span>
                {format!("Switched to {model}")}
                <span class="h-px flex-1 bg-zinc-200 dark:bg-zinc-800"></span>
            </p>
        }
        .into_any(),
        // A handoff between runtimes: what the new one was given.
        EventKind::ModelSwitched { model, handoff: Some(h), .. } => {
            let runtime = if h.into_claude_code() { "Claude Code".to_owned() } else { format!("carbon6's agent ({} tools)", h.tool_profile) };
            let shown = (!h.into_claude_code()).then(|| (h.system_prompt.clone(), h.tools.join(", ")));
            view! {
                <div class="space-y-1 rounded-md border border-dashed border-amber-300 px-3 py-2 text-xs text-zinc-600 dark:border-amber-900 dark:text-zinc-400">
                    <p class="font-medium" title=HANDOFF_HELP>{format!("Handed over to {runtime}, on {model}")}" "{crate::icons::info()}</p>
                    <p>"It got the conversation so far as a transcript: your messages, the replies, and tool calls with their output. The other runtime's reasoning and tool state did not carry over."</p>
                    {shown.map(|(prompt, tools)| view! {
                        <details>
                            <summary class="cursor-pointer select-none text-muted">"Its system prompt and tools"</summary>
                            <p class="mt-1">{tools}</p>
                            <pre class="mt-1 max-h-96 overflow-auto whitespace-pre-wrap rounded-md bg-zinc-100 p-3 font-mono text-[11px] dark:bg-zinc-900">{prompt}</pre>
                        </details>
                    })}
                </div>
            }
            .into_any()
        }
        EventKind::Compaction { summary, tokens_before } => view! {
            <details class="rounded-md border border-dashed border-zinc-300 px-3 py-2 text-xs text-muted dark:border-zinc-700">
                <summary class="cursor-pointer select-none">
                    {format!("Context compacted ({} tokens summarized); the model continues from this summary", tokens(*tokens_before))}
                </summary>
                <div class="md mt-2 text-zinc-700 dark:text-zinc-300" on:click=crate::mentions::link_clicks() inner_html=markdown::to_html(summary)></div>
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
                {note.map(|n| view! { <p class="text-center text-xs text-muted">{n}</p> })}
                {move || usage.with(|u| u.turns.get(&turn).copied()).map(|t| view! {
                    <p class="border-t border-zinc-200 pt-1 font-mono text-[11px] text-muted dark:border-zinc-800">
                        {format!("Turn {turn} · {}", turn_line(&t, None))}
                    </p>
                })}
            }
            .into_any()
        }
        _ => ().into_any(),
    }
}

/// A hidden reasoning-only reply must not leave an empty event (or its
/// branching buttons and padding) between the tool cards around it.
fn thinking_only(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Assistant { blocks, stop_reason, .. }
        if stop_reason != "interrupted" && blocks.iter().all(|b| matches!(b, ContentBlock::Thinking { .. })))
}

#[component]
fn Thinking(#[prop(into)] text: Signal<String>, #[prop(into)] show: Signal<bool>) -> impl IntoView {
    view! {
        <Show when=move || show.get()>
            <details class="border-l-2 border-zinc-300 pl-3 text-sm text-muted dark:border-zinc-700" open=true>
                <summary class="cursor-pointer select-none text-xs">"Thinking"</summary>
                <div class="mt-1 whitespace-pre-wrap text-xs">{move || text.get()}</div>
            </details>
        </Show>
    }
}

/// A session's subagents: by the call that started each (from the log's
/// `SubagentSpawned`, so a thread shows as soon as one starts, else from
/// the child's row), by background task id, and their rows as last read.
#[derive(Clone, Copy)]
struct Subagents {
    by_call: Memo<HashMap<String, String>>,
    by_task: Memo<HashMap<String, String>>,
    rows: Memo<HashMap<String, Session>>,
}

impl Subagents {
    fn new(events: RwSignal<Vec<Event>>, children: Memo<Vec<Session>>) -> Self {
        let spawned = Memo::new(move |_| {
            events.with(|evs| {
                evs.iter()
                    .filter_map(|e| match &e.kind {
                        EventKind::SubagentSpawned { call_id, child_session_id, background_task_id, .. } => {
                            Some((call_id.clone(), child_session_id.clone(), background_task_id.clone()))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
        });
        let by_call = Memo::new(move |_| {
            let mut m: HashMap<String, String> =
                children.with(|cs| cs.iter().filter_map(|c| Some((c.parent_call_id.clone()?, c.id.clone()))).collect());
            spawned.with(|sp| m.extend(sp.iter().map(|(call, child, _)| (call.clone(), child.clone()))));
            m
        });
        let by_task = Memo::new(move |_| {
            spawned.with(|sp| sp.iter().filter(|(_, _, t)| !t.is_empty()).map(|(_, child, t)| (t.clone(), child.clone())).collect())
        });
        let rows = Memo::new(move |_| {
            children.with(|cs| cs.iter().filter(|c| c.parent_call_id.is_some()).map(|c| (c.id.clone(), c.clone())).collect())
        });
        Self { by_call, by_task, rows }
    }

    /// A subagent named as a SendMessage call names it: its session id or
    /// background task id.
    fn resolve(&self, to: &str) -> Option<String> {
        if self.by_call.with(|m| m.values().any(|c| c == to)) {
            return Some(to.to_owned());
        }
        self.by_task.with(|m| m.get(to).cloned())
    }
}

/// How many subagents work now in the tree of the top-level session `root`
/// (from the live session list).
fn tree_working(root: String) -> Memo<u32> {
    let live = api::live();
    Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == root).map_or(0, |s| s.subagents_running)))
}

/// A subagent that ended badly: failed, or lost with its worker (not
/// done, and not stopped).
pub(crate) fn subagent_failed(s: &Session) -> bool {
    s.status != SessionStatus::Running && s.end_reason.is_some_and(|r| !matches!(r, TurnEndReason::Completed | TurnEndReason::Interrupted))
}

/// How a subagent stands: (pill, its colors).
fn subagent_state(running: bool, end: Option<TurnEndReason>) -> (AnyView, &'static str) {
    match (running, end) {
        (true, _) => (view! { {crate::icons::circle()}" running" }.into_any(), "text-blue-600 dark:text-blue-400"),
        (false, Some(TurnEndReason::Completed)) => {
            (view! { {crate::icons::check()}" done" }.into_any(), "text-emerald-700 dark:text-emerald-400")
        }
        (false, Some(TurnEndReason::Interrupted)) => (view! { "interrupted" }.into_any(), "text-amber-700 dark:text-amber-400"),
        (false, Some(TurnEndReason::WorkerRestarted)) => {
            (view! { {crate::icons::warning()}" lost" }.into_any(), "text-amber-700 dark:text-amber-400")
        }
        (false, Some(_)) => (view! { {crate::icons::x()}" failed" }.into_any(), "text-red-600 dark:text-red-400"),
        (false, None) => (view! { {crate::icons::circle()}" starting" }.into_any(), "text-muted"),
    }
}

/// Where a subagent runs: "connection · model · effort" (a subagent may
/// run on another connection than its parent's).
fn runs_on(row: Memo<Option<Session>>, connections: LocalResource<Result<Vec<c6_api::Connection>, String>>) -> String {
    let live = api::live();
    row.with(|r| {
        let Some(r) = r else { return String::new() };
        let model = model_label(&r.model, r.effort.as_deref().unwrap_or_default());
        let name = connections.get().and_then(Result::ok).and_then(|cs| cs.into_iter().find(|c| c.id == r.connection_id).map(|c| c.name));
        let model = match name {
            Some(n) => format!("{n} · {model}"),
            None => model,
        };
        // A remote subagent: where it runs, too (docs/remote-subagents.md).
        if r.remote_template.is_empty() {
            return model;
        }
        let worker = live.worker_name(&r.worker_id.to_string());
        let place = match (&r.ssh, r.cwd.is_empty()) {
            (Some(t), _) => format!("{worker} ⇄ {t}:{}", r.cwd),
            (None, true) => worker,
            (None, false) => format!("{worker}:{}", r.cwd),
        };
        format!("{place} · {model}")
    })
}

/// A subagent's name in its thread's header: its named agent, or the
/// template a remote one was started from.
fn subagent_name(r: &Session) -> Option<AnyView> {
    if !r.remote_template.is_empty() {
        return Some(view! { {crate::icons::layout_template()}" "{r.remote_template.clone()} }.into_any());
    }
    Some(r.agent.clone()).filter(|a| !a.is_empty()).map(IntoAny::into_any)
}

/// Scrolls the element with this id into view.
/// Scrolls to the element `id`; whether there was one.
fn scroll_to(id: &str) -> bool {
    match web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id(id)) {
        Some(el) => {
            el.scroll_into_view();
            true
        }
        None => false,
    }
}

/// A subagent's thread under the call that started it: its state, a live
/// look at its last steps while it runs, and, opened, its conversation
/// with Stop, Resume and a reply box. `waiting`: the call still waits on
/// it. Returns `AnyView` to end the type recursion (threads hold
/// transcripts that hold threads).
fn subagent_thread(id: String, subagents: Subagents, waiting: Signal<bool>) -> AnyView {
    let key = StoredValue::new(format!("c6.sub.{id}"));
    let saved = storage_get(&key.get_value());
    let force = use_context::<ForceOpen>();
    let forced = force.is_some_and(|f| f.0.with_untracked(|ids| ids.contains(&id)));
    // Opened by you (saved), or for a deep link or the subagents list.
    let open = RwSignal::new(forced || saved.as_deref() == Some("1"));
    if let Some(f) = force {
        let id = id.clone();
        Effect::new(move || {
            if f.0.with(|ids| ids.contains(&id)) {
                open.set(true);
            }
        });
    }
    let row = {
        let id = id.clone();
        Memo::new(move |_| subagents.rows.with(|m| m.get(&id).cloned()))
    };
    let toggle = move |_: web_sys::MouseEvent| {
        open.update(|o| *o = !*o);
        storage_set(&key.get_value(), if open.get_untracked() { "1" } else { "0" });
    };
    // While it runs or is open, its log is followed live.
    let busy = Signal::derive(move || waiting.get() || row.with(|r| r.as_ref().is_some_and(|r| r.status == SessionStatus::Running)));
    let live = move || open.get() || busy.get();
    let sid = StoredValue::new(id.clone());
    let connections = LocalResource::new(api::connections);
    view! {
        <div id=format!("sub-{id}") class="mt-1 border-l-2 border-violet-300 pl-3 dark:border-violet-800">
            {move || if live() {
                view! { <SubagentLive id=sid.get_value() row open toggle=Callback::new(toggle) /> }.into_any()
            } else {
                let (pill, tone) = row.with(|r| subagent_state(false, r.as_ref().and_then(|r| r.end_reason)));
                view! {
                    <button class="flex w-full items-center gap-2 py-1 text-left text-xs text-muted hover:text-zinc-900 dark:hover:text-zinc-200" on:click=toggle>
                        <span class="inline-block w-3 shrink-0 text-[9px]">{crate::icons::chevron_right()}</span>
                        <span class="shrink-0 font-medium text-violet-700 dark:text-violet-400">"Subagent"</span>
                        {move || row.with(|r| r.as_ref().and_then(subagent_name)).map(|a| view! { <span class="shrink-0 whitespace-nowrap font-mono" title="Started from this template, where it says">{a}</span> })}
                        <span class=format!("shrink-0 {tone}")>{pill}</span>
                        // Where and on what it runs gives way first (cut
                        // short, whole on hover): squeezed, the name went a
                        // letter a line.
                        {move || {
                            let on = runs_on(row, connections);
                            let title = on.clone();
                            view! { <span class="ml-auto min-w-0 truncate font-mono" title=title>{on}</span> }
                        }}
                    </button>
                }.into_any()
            }}
        </div>
    }
    .into_any()
}

/// A subagent's thread while it is followed: header with its numbers, its
/// last steps (running, not opened) or its whole conversation (opened).
#[component]
fn SubagentLive(id: String, row: Memo<Option<Session>>, open: RwSignal<bool>, toggle: Callback<web_sys::MouseEvent>) -> impl IntoView {
    let feed = Feed::open(id.clone(), Window::Tail);
    let running = Signal::derive(move || feed.running());
    let error = RwSignal::new(None::<String>);
    let wake = RwSignal::new(false);
    let sid = StoredValue::new(id.clone());
    let connections = LocalResource::new(api::connections);
    let end = Memo::new(move |_| {
        feed.events.with(|evs| {
            evs.iter().rev().find_map(|e| match &e.kind {
                EventKind::TurnEnded { reason, .. } => Some(*reason),
                _ => None,
            })
        })
    });
    let calls = Memo::new(move |_| feed.events.with(|evs| evs.iter().filter(|e| matches!(e.kind, EventKind::ToolResult { .. })).count()));
    let now = RwSignal::new(js_sys::Date::now() as i64);
    let ticker = set_interval_with_handle(move || now.set(js_sys::Date::now() as i64), std::time::Duration::from_secs(1)).ok();
    on_cleanup(move || {
        if let Some(t) = ticker {
            t.clear();
        }
    });
    let usage = Memo::new(move |_| feed.events.with(|evs| UsageStats::from_events(evs)));
    // Time spent working: its turns, the running one so far.
    let elapsed = move || {
        let ms: i64 = usage.with(|u| u.turns.values().map(|t| t.ended.unwrap_or_else(|| now.get()) - t.started).sum());
        (ms > 0).then(|| duration(ms))
    };
    let resumable = move || {
        !running.get() && matches!(end.get(), Some(TurnEndReason::Interrupted | TurnEndReason::Error | TurnEndReason::WorkerRestarted))
    };
    let stop = move |_| {
        spawn_local(async move {
            if let Err(e) = api::interrupt(sid.get_value()).await {
                error.set(Some(e));
            }
        });
    };
    let resume = move |_| {
        spawn_local(async move {
            match api::resume(sid.get_value(), wake.get_untracked()).await {
                Ok(_) => error.set(None),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <button class="flex w-full items-center gap-2 py-1 text-left text-xs text-muted hover:text-zinc-900 dark:hover:text-zinc-200" on:click=move |ev| toggle.run(ev)>
            <span class="inline-block w-3 shrink-0 text-[9px] transition-transform" class:rotate-90=move || open.get()>{crate::icons::chevron_right()}</span>
            <span class="shrink-0 font-medium text-violet-700 dark:text-violet-400">"Subagent"</span>
            {move || row.with(|r| r.as_ref().and_then(subagent_name)).map(|a| view! { <span class="shrink-0 whitespace-nowrap font-mono" title="Started from this template, where it says">{a}</span> })}
            {move || {
                let (pill, tone) = subagent_state(running.get(), end.get());
                view! { <span class=format!("shrink-0 {tone}")>{pill}</span> }
            }}
            <span class="shrink-0 whitespace-nowrap">{move || {
                let n = calls.get();
                let mut parts = vec![format!("{n} call{}", if n == 1 { "" } else { "s" })];
                parts.extend(elapsed());
                let t = usage.with(|u| u.total);
                if t.calls > 0 {
                    parts.push(format!("{} in · {} out", tokens(t.prompt()), tokens(t.output)));
                }
                parts.join(" · ")
            }}</span>
            {move || {
                let on = runs_on(row, connections);
                let title = on.clone();
                view! { <span class="ml-auto min-w-0 truncate font-mono" title=title>{on}</span> }
            }}
            <Go to=View::session(id.clone()) class="shrink-0 text-violet-700 hover:underline dark:text-violet-400" title="Open subagent session" stop=true>{crate::icons::square_arrow_out()}</Go>
        </button>
        {move || if open.get() {
            match row.get() {
                // Claude Code's own subagent: shown, not run from here.
                Some(s) if s.observed => view! {
                    <div class="space-y-3 pb-2 pt-1">
                        <Transcript session=s feed running />
                        <p class="text-xs text-muted">"Claude Code's own subagent: Claude Code runs and continues it."</p>
                    </div>
                }.into_any(),
                Some(s) => view! {
                    <div class="space-y-3 pb-2 pt-1">
                        <Transcript session=s feed running />
                        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                        <div class="flex flex-wrap items-center gap-2">
                            {move || running.get().then(|| view! {
                                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") title="Stop this subagent's turn (not the agent that started it)" on:click=stop>{crate::icons::stop()}" Stop"</button>
                            })}
                            {move || resumable().then(|| view! {
                                <button class=format!("{BUTTON} text-xs") title="Continue it where it stopped, with the resume prompt (Settings → Prompts)" on:click=resume>{crate::icons::rotate()}" Resume"</button>
                            })}
                        </div>
                        <Composer id=sid.get_value() running disabled=Signal::derive(|| false) error inline=true wake />
                    </div>
                }.into_any(),
                None => view! { <p class="py-1 text-xs text-muted">"Starting…"</p> }.into_any(),
            }
        } else {
            view! { <SubagentPeek events=feed.events streaming=feed.streaming /> }.into_any()
        }}
    }
}

/// A running subagent's last few steps, followed live.
#[component]
fn SubagentPeek(events: RwSignal<Vec<Event>>, streaming: RwSignal<Streaming>) -> impl IntoView {
    let lines = Memo::new(move |_| {
        events.with(|evs| {
            let mut out: Vec<(&'static str, String)> = Vec::new();
            for e in evs {
                match &e.kind {
                    // From the agent that started it, or from you.
                    EventKind::User { text, delegated_by, command, .. } if command != "task-notification" && !text.is_empty() => {
                        let mark = if command == "session-message" {
                            "message"
                        } else if delegated_by.is_empty() {
                            "edit"
                        } else {
                            "delegated"
                        };
                        out.push((mark, text.lines().next().unwrap_or_default().to_owned()));
                    }
                    EventKind::Assistant { blocks, .. } => {
                        for b in blocks {
                            match b {
                                ContentBlock::Text { text } if !text.trim().is_empty() => {
                                    out.push(("", text.trim().lines().next().unwrap_or_default().to_owned()))
                                }
                                ContentBlock::ToolUse { name, input_json, .. } => {
                                    let input = serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null);
                                    out.push(("tool", format!("{} {}", tool_label(name).0, summarize(name, &input))));
                                }
                                _ => {}
                            }
                        }
                    }
                    EventKind::Error { message } => out.push(("error", message.lines().next().unwrap_or_default().to_owned())),
                    _ => {}
                }
            }
            let skip = out.len().saturating_sub(3);
            out.split_off(skip)
        })
    });
    view! {
        <div class="space-y-0.5 pb-1 text-xs text-muted">
            {move || lines.get().into_iter().map(|(mark, text)| view! {
                <p class="flex gap-2 truncate"><span class="w-3 shrink-0">{match mark {
                    "message" | "delegated" => crate::icons::message().into_any(),
                    "edit" => crate::icons::pencil().into_any(),
                    "tool" => crate::icons::chevron_right().into_any(),
                    "error" => crate::icons::x().into_any(),
                    _ => ().into_any(),
                }}</span><span class="truncate font-mono">{text}</span></p>
            }).collect_view()}
            {move || streaming.with(|s| s.text.trim().lines().last().map(str::to_owned)).filter(|t| !t.is_empty()).map(|t| view! {
                <p class="flex gap-2 truncate italic"><span class="w-3 shrink-0"></span><span class="truncate">{t}</span></p>
            })}
        </div>
    }
}

/// Normalizes tool names across profiles (Claude, Codex, generic).
/// A tool's name as people read it: an MCP server's tool
/// (`mcp__carbon6__SendMessage`) as its words ("Send message") and its
/// server ("carbon6"); Ask's tools for carbon6's operations as "carbon6"
/// (what they do, in words, is their summary); any other tool as it's
/// named.
fn tool_label(name: &str) -> (String, Option<String>) {
    if matches!(tool_kind(name), "c6_call" | "c6_discover" | "c6_describe") {
        return ("carbon6".to_owned(), None);
    }
    let Some(rest) = name.strip_prefix("mcp__") else { return (name.to_owned(), None) };
    let Some((server, tool)) = rest.split_once("__") else { return (name.to_owned(), None) };
    let server = server.strip_prefix("claude_ai_").unwrap_or(server).replace('_', " ");
    (words(tool), Some(server))
}

/// `SendMessage`, `send_message` or `settings.routes.put` as "Send
/// message", "Settings routes put".
fn words(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c == '_' || c == '-' || c == '.' {
            out.push(' ');
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        out.extend(if out.is_empty() { c.to_uppercase().collect::<Vec<_>>() } else { c.to_lowercase().collect() });
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tool_kind(name: &str) -> &str {
    match name {
        "Bash" | "shell" => "bash",
        "Read" => "read",
        "Write" => "write",
        "Edit" => "edit",
        "Grep" => "grep",
        "Glob" => "glob",
        "TodoWrite" | "update_plan" => "plan",
        "Task" | "mcp__carbon6__Task" => "task",
        "SendMessage" | "mcp__carbon6__SendMessage" => "send_message",
        "message_session" | "MessageSession" | "mcp__carbon6__MessageSession" => "message_session",
        "list_sessions" | "ListSessions" | "mcp__carbon6__ListSessions" => "list_sessions",
        "WebFetch" => "web_fetch",
        "BashOutput" | "mcp__carbon6__TaskOutput" => "bash_output",
        "KillShell" | "mcp__carbon6__TaskStop" => "kill_shell",
        "Skill" => "load_skill",
        "doc" | "Doc" | "mcp__carbon6__doc" => "doc",
        "c6_ask" | "mcp__carbon6__ask" => "ask",
        "app_call" | "AppCall" | "mcp__carbon6__app_call" | "mcp__carbon6__AppCall" => "app_call",
        // Ask's (docs/assistant.md): an operation run, looked up, read.
        "c6_call" | "c6_discover" | "c6_describe" => name,
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
        "send_message" => s("message").lines().next().unwrap_or("").to_owned(),
        // To whom (the session's title, when it's in the live list), then what.
        "message_session" => {
            let first = s("message").lines().next().unwrap_or("").to_owned();
            let to = s("to");
            let title = use_context::<crate::api::LiveFeed>()
                .and_then(|l| l.sessions.with(|ss| ss.iter().find(|x| x.id == to).map(|x| x.title.clone())))
                .filter(|t| !t.trim().is_empty());
            match title {
                Some(t) => format!("to {t}: {first}"),
                None => first,
            }
        }
        "list_sessions" => s("query"),
        "web_fetch" => s("url"),
        "bash_output" | "kill_shell" => input
            .get("id")
            .or_else(|| input.get("bash_id"))
            .or_else(|| input.get("shell_id"))
            .or_else(|| input.get("task_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned(),
        "load_skill" => input.get("skill").or_else(|| input.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_owned(),
        "doc" => {
            let what = s("action");
            let title = s("title");
            if title.is_empty() { format!("{what} {}", s("id")) } else { format!("{what} \"{title}\"") }
        }
        // Which operation, with the first of its input; none: a listing.
        "app_call" => {
            let (instance, op) = (s("instance"), s("operation"));
            if op.is_empty() {
                return "list the app operations it can call".into();
            }
            let args: Vec<String> = input
                .get("input")
                .and_then(|v| v.as_object())
                .map(|o| {
                    o.iter().take(2).map(|(k, v)| format!("{k}: {}", v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect()
                })
                .unwrap_or_default();
            let target = if instance.is_empty() { op } else { format!("{instance}.{op}") };
            if args.is_empty() { format!("run {target}") } else { format!("run {target} ({})", args.join(", ")) }
        }
        // What Ask does, in words rather than its JSON.
        "c6_call" => match s("op").as_str() {
            "" => "Run a carbon6 operation".into(),
            "sessions.create" => "Start a session".into(),
            "docs.create" => "Make a document".into(),
            op => words(op),
        },
        "c6_discover" => match (s("query"), s("area")) {
            (q, _) if !q.trim().is_empty() => format!("Look up {}", q.trim()),
            (_, a) if !a.is_empty() => format!("Look up what {a} can do"),
            _ => "Look up what carbon6 can do".into(),
        },
        "c6_describe" => format!("Read how {} works", s("op")),
        // Its title, else its first question.
        "ask" => {
            let first = input.pointer("/fields/0/label").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(s("title")).filter(|t| !t.is_empty()).unwrap_or(first)
        }
        "plan" => {
            let items = plan_items(input);
            let done = items.iter().filter(|(_, st)| st == "completed").count();
            format!("{done}/{} done", items.len())
        }
        _ => input.to_string(),
    }
}

/// The operation of Ask's approval ("<op> <input JSON>") as a c6_call's
/// input; anything else (a session's first message): null.
fn c6_approval_op(input: &str) -> serde_json::Value {
    input
        .split_once(' ')
        .filter(|(op, rest)| op.contains('.') && serde_json::from_str::<serde_json::Value>(rest).is_ok())
        .map_or(serde_json::Value::Null, |(op, _)| serde_json::json!({ "op": op }))
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

/// The session a `sessions.create` call made, from its result (the
/// session, as JSON; its start if the output was cut short): its id and
/// title.
fn made_session(output: &str) -> Option<(String, String)> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(output) {
        let id = v["id"].as_str().filter(|i| !i.is_empty())?.to_owned();
        return Some((id, v["title"].as_str().unwrap_or("").to_owned()));
    }
    let field = |k: &str| {
        let at = output.find(&format!("\"{k}\":"))? + k.len() + 3;
        let rest = output[at..].trim_start().strip_prefix('"')?;
        Some(rest[..rest.find('"')?].to_owned())
    };
    Some((field("id").filter(|i| !i.is_empty())?, field("title").unwrap_or_default()))
}

/// A session Ask started: its colour as a stripe, its title, how it
/// stands and where it runs, live; it opens the session (⌘/Ctrl-click: in
/// a new tile). One line with an Open button on a desktop, two short lines
/// to tap on a phone.
#[component]
fn MadeSession(id: String, title: String) -> impl IntoView {
    let live = api::live();
    let (col, ws) = (use_column(), crate::workspace::use_workspace());
    let row = {
        let id = id.clone();
        Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == id).cloned()))
    };
    let target = StoredValue::new(id.clone());
    let open = move |ev: web_sys::MouseEvent| {
        // Not the tool card's details opening and closing.
        ev.prevent_default();
        ev.stop_propagation();
        let (to, how) = (View::session(target.get_value()), crate::workspace::How::from_mouse(&ev));
        match col {
            Some(c) => c.open(to, how),
            None => ws.open(None, to, how),
        }
    };
    let hashed = c6_api::marks::hashed(&id).to_owned();
    view! {
        <div class="mt-1.5 flex basis-full cursor-pointer items-stretch overflow-hidden rounded-md border border-zinc-200 bg-white hover:bg-zinc-50 dark:border-zinc-700 dark:bg-zinc-900 dark:hover:bg-zinc-800"
            title="Open the session (⌘-click: in a new tile)" on:click=open>
            <span class="w-1 shrink-0" style=move || crate::marks::fill(&row.with(|r| r.as_ref().map(crate::marks::of)).unwrap_or_else(|| hashed.clone()))></span>
            {move || {
                let s = row.get();
                let title = s.as_ref().map(|s| s.title.clone()).filter(|t| !t.trim().is_empty())
                    .or_else(|| Some(title.clone()).filter(|t| !t.trim().is_empty()))
                    .unwrap_or_else(|| "New session".into());
                let (cls, word) = match &s {
                    Some(s) if s.awaiting_approval => ("text-amber-700 dark:text-amber-400", "needs you"),
                    Some(s) => match s.display_status() {
                        SessionStatus::Running => ("text-blue-600 dark:text-blue-400", "running"),
                        SessionStatus::Idle => ("text-muted", "idle"),
                        SessionStatus::Starting => ("text-amber-700 dark:text-amber-400", "starting"),
                        SessionStatus::Failed => ("text-red-600 dark:text-red-400", "failed"),
                    },
                    None => ("text-muted", "starting"),
                };
                let place = s.as_ref().map(|s| live.session_worker(s));
                view! {
                    <div class="flex min-w-0 flex-1 flex-col px-2 py-1 md:flex-row md:items-center md:gap-2">
                        <span class="truncate text-sm font-medium">{title}</span>
                        <span class="flex min-w-0 shrink-0 items-center gap-1.5 text-xs">
                            <span class=cls>{(word == "running").then(crate::icons::circle)}" "{word}</span>
                            {place.map(|p| view! { <span class="truncate text-muted">{format!("· {p}")}</span> })}
                        </span>
                    </div>
                }
            }}
            <button class="mr-1.5 hidden shrink-0 self-center rounded-md border border-zinc-200 px-2 py-0.5 text-xs hover:bg-zinc-100 md:block dark:border-zinc-700 dark:hover:bg-zinc-700"
                on:click=open>"Open"</button>
        </div>
    }
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
        "send_message" | "message_session" => input.get("message").and_then(|v| v.as_str()).map(str::to_owned),
        "read" | "glob" | "grep" | "plan" | "web_fetch" | "bash_output" | "kill_shell" | "load_skill" | "list_sessions" => None,
        _ => serde_json::to_string_pretty(&input).ok(),
    };
    let plan = (kind == "plan").then(|| plan_items(&input));
    // A Task call's subagent, shown as a thread under the card; the one a
    // SendMessage call continues.
    let subagents = use_context::<Subagents>();
    let thread = (kind == "task").then_some(()).and(subagents).map(|s| {
        let call = call_id.clone();
        (s, Memo::new(move |_| s.by_call.with(|m| m.get(&call).cloned())))
    });
    let continued = (kind == "send_message").then_some(()).and(subagents).map(|s| {
        let to = input.get("to").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        Memo::new(move |_| s.resolve(&to))
    });
    // Ask's operation, if it is one: a session or a document it made shows
    // as a card.
    let op = if kind == "c6_call" { input.get("op").and_then(|v| v.as_str()).unwrap_or("").to_owned() } else { String::new() };
    let worded = matches!(kind.as_str(), "c6_call" | "c6_discover" | "c6_describe");
    let made_session = (op == "sessions.create")
        .then(|| Memo::new(move |_| result.with(|r| r.as_ref().filter(|r| !r.is_error).and_then(|r| made_session(&r.output)))));
    // A document it wrote: a card that opens it (docs/docs.md §12).
    let doc_card = (kind == "doc" || op == "docs.create").then(|| {
        let action = if op == "docs.create" { "created" } else { input.get("action").and_then(|v| v.as_str()).unwrap_or("") }.to_owned();
        Signal::derive(move || {
            let r = result.get().filter(|r| !r.is_error)?;
            let v: serde_json::Value = serde_json::from_str(&r.output).ok()?;
            let d = v.get("doc")?;
            let what = match v["did"].as_str().unwrap_or(&action) {
                "created" => "made".to_owned(),
                "edited" => {
                    let n: usize = v["changed"].as_array().map(Vec::len).unwrap_or(0);
                    format!("{n} {} changed", if n == 1 { "block" } else { "blocks" })
                }
                other => other.to_owned(),
            };
            Some((
                d["id"].as_str()?.to_owned(),
                d["title"].as_str().unwrap_or("").to_owned(),
                d["kind"].as_str().unwrap_or("doc").to_owned(),
                what,
            ))
        })
    });
    // A message to another session: to whom (its title, from the result),
    // and a way there (docs/background.md §6).
    let sent_to = (kind == "message_session").then(|| {
        let to = input.get("to").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        let title = Memo::new(move |_| result.get().filter(|r| !r.is_error).and_then(|r| r.output.split('"').nth(1).map(str::to_owned)));
        (to, title)
    });
    let live_shown = Memo::new(move |_| result.with(Option::is_none) && live_output.with(Option::is_some));
    let live_box = NodeRef::<leptos::html::Pre>::new();
    let live_tail = StoredValue::new(true);
    Effect::new(move || {
        live_output.track();
        if live_tail.get_value() {
            request_animation_frame(move || {
                if let Some(el) = live_box.try_get_untracked().flatten() {
                    el.set_scroll_top(el.scroll_height());
                }
            });
        }
    });
    // Where a search hit in this call's output scrolls to.
    let dom_id = format!("call-{call_id}");
    let started_open = plan.is_some();
    view! {
        <div id=dom_id class=format!("rounded-md border border-zinc-200 text-sm dark:border-zinc-800 {}", Arrived::class())>
        <details class="group" open=started_open>
            // A session it made shows on a line of its own below.
            <summary class=if made_session.is_some() {
                "flex cursor-pointer select-none flex-wrap items-center gap-x-2 px-3 py-1.5"
            } else {
                "flex cursor-pointer select-none items-center gap-2 px-3 py-1.5"
            }>
                {move || {
                    let (icon, cls) = match result.get() {
                        None => (crate::icons::circle().into_any(), "animate-pulse text-blue-500"),
                        Some(r) if r.is_error => (crate::icons::x().into_any(), "text-red-500"),
                        Some(_) => (crate::icons::check().into_any(), "text-emerald-600"),
                    };
                    view! { <span class=format!("shrink-0 text-xs {cls}")>{icon}</span> }
                }}
                {
                    // The name never gives way: the summary and the
                    // recipient do (a long one squeezed it to a letter a line).
                    let (label, server) = tool_label(&name);
                    view! {
                        <span class="shrink-0 whitespace-nowrap text-xs font-semibold" title=name.clone()>{label}</span>
                        {server.map(|s| view! { <span class="shrink-0 whitespace-nowrap rounded bg-zinc-100 px-1 text-[10px] text-muted dark:bg-zinc-800" title=name.clone()>{s}</span> })}
                    }
                }
                <span class=if worded { "min-w-0 flex-1 truncate text-xs text-zinc-600 dark:text-zinc-300" } else { "min-w-0 flex-1 truncate font-mono text-xs text-soft" }>{summary}</span>
                {sent_to.map(|(to, title)| view! {
                    <Go to=View::session(to.clone()) class="min-w-0 max-w-[50%] truncate text-[11px] text-sky-700 hover:underline dark:text-sky-400" stop=true>
                        {crate::icons::arrow_right()}" "{move || format!("sent to {}", title.get().unwrap_or_else(|| "a session".into()))}" "{crate::icons::arrow_up_right()}
                    </Go>
                })}
                {move || continued.and_then(|c| c.get()).map(|cid| {
                    let target = format!("sub-{cid}");
                    // Its thread may be far above what's drawn: then its
                    // own session, beside.
                    let col = use_column();
                    view! {
                        <button class="text-[11px] text-violet-700 hover:underline dark:text-violet-400" title="Go to its thread"
                            on:click=move |ev| {
                                ev.prevent_default();
                                ev.stop_propagation();
                                if !scroll_to(&target)
                                    && let Some(c) = col
                                {
                                    c.open(View::session(cid.clone()), crate::workspace::How::Beside);
                                }
                            }>{crate::icons::rotate()}" its thread"</button>
                    }
                })}
                {move || result.get().map(|r| view! { <span class="shrink-0 whitespace-nowrap text-[11px] text-muted">{format!("{:.1}s", r.duration_ms as f64 / 1000.0)}</span> })}
                {move || made_session.and_then(|m| m.get()).map(|(id, title)| view! { <MadeSession id title /> })}
            </summary>
            <div class="space-y-2 border-t border-zinc-200 px-3 py-2 dark:border-zinc-800">
                {plan.map(|items| view! {
                    <ul class="space-y-1 text-sm">
                        {items.into_iter().map(|(text, status)| {
                            let (mark, cls) = match status.as_str() {
                                "completed" => (crate::icons::check_box(true).into_any(), "text-muted line-through"),
                                "in_progress" => (crate::icons::play().into_any(), "font-medium"),
                                _ => (crate::icons::check_box(false).into_any(), ""),
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
                    {r.truncated.then(|| view! { <p class="text-[11px] text-muted">"Output was truncated for the model."</p> })}
                })}
            </div>
        </details>
        {move || doc_card.and_then(|c| c.get()).map(|(id, title, kind, what)| view! {
            <div class="mx-3 mb-2">
                <crate::workspace::Go to=crate::workspace::View::Docs { id } class="inline-flex items-center gap-2 rounded-md border border-zinc-200 px-2 py-1 text-sm hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-900">
                    <span class="text-muted">{if kind == "deck" { crate::icons::chart().into_any() } else { crate::icons::file().into_any() }}</span>
                    <span class="font-medium">{title}</span>
                    <span class="text-xs text-muted">{what}</span>
                </crate::workspace::Go>
            </div>
        })}
        // What the tool showed the model, visible without expanding the card.
        {move || result.get().filter(|r| !r.images.is_empty()).map(|r| view! {
            <div class="mx-3 mb-2 flex flex-wrap gap-2">
                {r.images.into_iter().map(|u| {
                    let href = u.clone();
                    view! {
                    <a href=href target="_blank" title="What the model saw">
                        <img src=u class="max-h-40 rounded border border-zinc-200 dark:border-zinc-700" loading="lazy" />
                    </a>
                    }
                }).collect_view()}
            </div>
        })}
        // Live output of a running tool, visible without expanding the card:
        // one box whose text grows, following its end unless you scroll up in it.
        {move || live_shown.get().then(|| view! {
            <pre node_ref=live_box class="mx-3 mb-2 max-h-48 overflow-auto whitespace-pre-wrap rounded bg-zinc-100 p-2 font-mono text-xs dark:bg-zinc-900"
                on:scroll=move |_| if let Some(el) = live_box.get_untracked() { live_tail.set_value(el.scroll_height() - el.scroll_top() - el.client_height() < 24) }>
                {move || live_output.get().unwrap_or_default()}
            </pre>
        })}
        {thread.map(|(s, child)| {
            let waiting = Signal::derive(move || result.with(Option::is_none));
            move || child.get().map(|cid| view! { <div class="px-3 pb-2">{subagent_thread(cid, s, waiting)}</div> })
        })}
        </div>
    }
}

#[component]
fn Queued(q: QueuedInput, session_id: String, worker_id: c6_api::Uuid, pull: Callback<Vec<QueuedInput>>) -> impl IntoView {
    let rejected = q.rejected.clone();
    // The worker handed it to the agent: it reads it at its next step.
    let with_agent = q.with_agent;
    let text = q.text;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (session_id, command_id, text) = (StoredValue::new(session_id), StoredValue::new(q.command_id), StoredValue::new(text));
    // A connected worker that is older than the head cannot edit or cancel
    // its queued inputs: hide the buttons (the head would 409 them). An away
    // worker, or one the live feed has not listed yet, gets the click and
    // says so then (503).
    let queue_ops = Signal::derive(move || {
        !api::live().workers.with(|ws| {
            ws.iter()
                .find(|w| w.id == worker_id)
                .is_some_and(|w| w.status == WorkerStatus::Online && !w.capabilities.iter().any(|c| c == "queue_ops"))
        })
    });
    // Pulls this message and the ones queued after it into the edit box
    // (the head cancels them); the box opens or takes them in.
    let do_pull = move || {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let (id, cid, pull) = (session_id.get_value(), command_id.get_value(), pull);
        spawn_local(async move {
            match api::pull_queued(id, cid).await {
                Ok(pulled) => pull.run(pulled),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="flex flex-col items-end gap-1">
            // Queued at the head: dashed; with the agent: solid, as good as sent.
            <div class=if rejected.is_some() {
                "max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-sm border border-red-300 px-4 py-2 text-sm opacity-70 dark:border-red-900"
            } else if with_agent {
                "max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-sm border border-zinc-300 px-4 py-2 text-sm text-zinc-700 dark:border-zinc-700 dark:text-zinc-300"
            } else {
                "max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-sm border border-dashed border-zinc-300 px-4 py-2 text-sm text-muted dark:border-zinc-700"
            }>
                {text.get_value()}
            </div>
            {match rejected {
                Some(reason) => view! {
                    <span class="text-xs text-red-600">
                        {format!("Not delivered: {reason}")} " "
                        <button class="underline" on:click=move |_| { let id = session_id.get_value(); spawn_local(async move { let _ = api::dismiss_rejected(id).await; }); }>"Dismiss"</button>
                    </span>
                }.into_any(),
                // With the agent already: it reads it at its next step; Send now
                // gives it to the agent at once (docs/background.md §4).
                None if with_agent => view! {
                    <span class="flex items-center gap-2 text-xs text-muted">
                        <button class=UBTN title="The agent gets this now, not at its next step (Claude Code moves what it waits on to the background; its background agents go on)"
                            prop:disabled=move || busy.get() on:click=move |_| {
                                busy.set(true);
                                let (id, cid) = (session_id.get_value(), command_id.get_value());
                                spawn_local(async move {
                                    if let Err(e) = api::send_queued_now(id, cid).await { error.set(Some(e)); }
                                    busy.set(false);
                                });
                            }>"Send now"</button>
                    </span>
                }.into_any(),
                None => view! {
                    <span class="flex items-center gap-2 text-xs text-muted">
                        "queued"
                        {move || queue_ops.get().then(|| view! {
                            <button class=UBTN title="Edit this message and the ones queued after it, then send them again"
                                prop:disabled=move || busy.get() on:click=move |_| do_pull()>{crate::icons::pencil()}" Edit"</button>
                            <button class=UBTN title="Remove this message from the queue"
                                on:click=move |_| { let (id, cid) = (session_id.get_value(), command_id.get_value()); spawn_local(async move {
                                    if let Err(e) = api::cancel_queued_input(id, cid).await { error.set(Some(e)); }
                                }); }>{crate::icons::x()}" Cancel"</button>
                        })}
                    </span>
                }.into_any(),
            }}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        </div>
    }
}

/// The queue-edit box: the messages pulled out of the queue, each a
/// textarea — edited in place, moved up or down, dropped, or new ones
/// added. Sending re-queues them, in order, as fresh messages; closing it
/// discards them (they were pulled out of the queue, not held). Its rows
/// are shared (`rows`): the box is opened by `edit_open`, so a second
/// pull appends to it without re-running this body.
#[component]
fn QueueEditBox(rows: RwSignal<Vec<QueueRow>>, session_id: String, on_close: Callback<()>) -> impl IntoView {
    let session_id = StoredValue::new(session_id);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let send = move || {
        if busy.get_untracked() {
            return;
        }
        let texts: Vec<(u64, String)> =
            rows.with_untracked(|r| r.iter().map(|x| (x.id, x.text.trim().to_owned())).filter(|(_, t)| !t.is_empty()).collect());
        if texts.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        let id = session_id.get_value();
        spawn_local(async move {
            let mut sent = Vec::new();
            for (row_id, text) in texts {
                match api::send_message(id.clone(), text, None, Vec::new()).await {
                    Ok(_) => sent.push(row_id),
                    Err(e) => {
                        error.set(Some(e));
                        break;
                    }
                }
            }
            busy.set(false);
            if error.get_untracked().is_none() {
                on_close.run(());
            } else {
                // The ones that made it are in the queue again: drop them
                // here, so sending again does not double them.
                rows.update(|r| r.retain(|x| !sent.contains(&x.id)));
            }
        });
    };
    view! {
        <div class="w-full rounded-xl border border-zinc-300 bg-white p-3 text-sm dark:border-zinc-700 dark:bg-zinc-900">
            <div class="mb-2 flex items-center gap-2 text-xs text-muted">
                "Edit the queued messages, then send them again"
                <span class="flex-1"></span>
                <button class=format!("{BUTTON_QUIET} text-xs") title="Discard these messages (they were pulled out of the queue)"
                    prop:disabled=move || busy.get() on:click=move |_| on_close.run(())>"Discard"</button>
            </div>
            <div class="flex flex-col gap-2">
                <For each=move || rows.get() key=|r| r.id let:row>
                    <QueueEditRow id=row.id rows=rows />
                </For>
            </div>
            <div class="mt-2 flex items-center gap-3">
                <button class=format!("{BUTTON_QUIET} text-xs") prop:disabled=move || busy.get()
                    on:click=move |_| rows.update(|r| {
                        let next = r.iter().map(|x| x.id).max().unwrap_or(0) + 1;
                        r.push(QueueRow { id: next, text: String::new() });
                    })>{crate::icons::plus()}" Add a message"</button>
                <span class="flex-1"></span>
                <span class="text-xs text-muted">"Closing this discards these messages"</span>
                <button class=format!("{BUTTON} text-xs") prop:disabled=move || busy.get() on:click=move |_| send()>"Send"</button>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        </div>
    }
}

/// One message in the queue-edit box.
#[component]
fn QueueEditRow(id: u64, rows: RwSignal<Vec<QueueRow>>) -> impl IntoView {
    view! {
        <div class="flex items-start gap-1.5">
            <textarea class=format!("{INPUT} min-h-[2.5rem] resize-y") rows=2
                prop:value=move || rows.with(|r| r.iter().find(|x| x.id == id).map(|x| x.text.clone()).unwrap_or_default())
                on:input=move |ev| {
                    let text = event_target_value(&ev);
                    rows.update(|r| if let Some(x) = r.iter_mut().find(|x| x.id == id) { x.text = text; });
                }></textarea>
            <div class="flex flex-col gap-1">
                <button class=UBTN title="Move up"
                    on:click=move |_| rows.update(|r| if let Some(i) = r.iter().position(|x| x.id == id) && i > 0 { r.swap(i - 1, i); })>{crate::icons::arrow_up()}</button>
                <button class=UBTN title="Move down"
                    on:click=move |_| rows.update(|r| if let Some(i) = r.iter().position(|x| x.id == id) && i + 1 < r.len() { r.swap(i, i + 1); })>{crate::icons::arrow_down()}</button>
                <button class=UBTN title="Drop this message"
                    on:click=move |_| rows.update(|r| r.retain(|x| x.id != id))>{crate::icons::x()}</button>
            </div>
        </div>
    }
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = c6Files, js_name = has)]
    pub(crate) fn js_has_files(ev: &web_sys::Event) -> bool;

    #[wasm_bindgen(catch, js_namespace = c6Files, js_name = read)]
    pub(crate) async fn js_read_files(ev: &web_sys::Event) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
}

/// The slash command being typed: the text up to the caret, while it is
/// still the first word.
fn command_at(text: &str, caret: usize) -> Option<String> {
    let before: String = text.chars().take(caret).collect();
    let q = before.strip_prefix('/')?;
    (!q.contains(char::is_whitespace) && !q.contains('/')).then(|| q.to_owned())
}

/// Starts a browser download of `url`.
fn download(url: &str) {
    use wasm_bindgen::JsCast;
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    if let Ok(a) = doc.create_element("a") {
        let _ = a.set_attribute("href", url);
        let _ = a.set_attribute("download", "");
        if let Ok(a) = a.dyn_into::<web_sys::HtmlElement>() {
            a.click();
        }
    }
}

#[derive(Clone, PartialEq)]
enum Suggestion {
    File(String, bool),
    /// A thing in the memex: (what to insert after @memex:, kind, name).
    Thing(String, String, String),
    /// For Ask (docs/ask.md §4): (title, kind, reference): a project, doc,
    /// work, or any memex item, read when the message reaches it.
    Ref(String, String, String),
    Command(c6_api::CommandInfo),
}

/// The `@` mention being typed at the caret: (where the @ is, the query).
fn mention_at(text: &str, caret: usize) -> Option<(usize, String)> {
    let before: String = text.chars().take(caret).collect();
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    let starts_word = before[..at].chars().last().is_none_or(char::is_whitespace);
    (starts_word && !query.contains(char::is_whitespace)).then(|| (before[..at].chars().count(), query.to_owned()))
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
    /// (worker id, directory): for @-mention completion.
    #[prop(optional, into)]
    files: Option<Signal<Option<(String, String)>>>,
    /// A subagent's reply box: whether its report should wake its parent.
    #[prop(optional)]
    wake: Option<RwSignal<bool>>,
    /// Offer a picker for the model its subagents run on.
    #[prop(optional)]
    subagents: Option<SubagentChoice>,
    /// Leave Claude Code out of the model picker (an SSH session).
    #[prop(optional)]
    no_claude_code: bool,
    /// An Ask conversation: @ also offers projects, docs, work and your
    /// mail, chat and calendar (docs/ask.md §4).
    #[prop(optional)]
    ask: bool,
) -> impl IntoView {
    let subagent_id = StoredValue::new(id.clone());
    // In Ask, Claude Code runs on the head's machine: why it can't, if so.
    let claude_check = LocalResource::new(move || async move { if ask { api::ask_claude().await.ok() } else { None } });
    let claude_off = Signal::derive(move || claude_check.get().flatten().and_then(|c| (!c.available).then_some(c.reason)));
    // Connection kinds: a switch into or out of Claude Code is a handoff.
    let kinds = LocalResource::new(api::connections);
    let text = crate::util::draft(&id);
    let (attachments, draft_note, draft_ready) = crate::util::draft_attachments(Some(&id));
    // What you'd likely type next (docs/suggestions.md): fetched when a turn
    // ends, shown faintly while the box is empty, taken with a tap or Tab.
    let next = RwSignal::new(None::<c6_api::Suggestion>);
    let suggest_for = id.clone();
    let suggests = !inline && wake.is_none();
    Effect::new(move |was: Option<bool>| {
        let now = running.get();
        if suggests && !now && was != Some(false) {
            let id = suggest_for.clone();
            spawn_local(async move {
                let s = api::session_suggestion(id.clone()).await.ok().flatten();
                if let Some(s) = &s {
                    api::suggestion_seen(id, s.for_seq, false).await;
                }
                next.set(s);
            });
        } else if now {
            next.set(None);
        }
        now
    });
    let shown_next = move || next.get().filter(|_| text.with(String::is_empty) && !running.get()).map(|s| s.text);
    // The model row, on phones (always shown on wider screens).
    // Whether the model chip's menu (the model and its subagents') is open.
    let picking = RwSignal::new(false);
    let toggle = move || picking.update(|p| *p = !*p);
    let picking_pop = StoredValue::new(crate::util::dismissable(
        move || picking.get(),
        move || {
            picking.try_set(false);
        },
    ));
    let id = StoredValue::new(id);
    let area = NodeRef::<leptos::html::Textarea>::new();
    // A suggestion shown in the empty box is sized to show all of it (on a
    // phone it ran past two lines and was cut off); typing sizes to the text.
    crate::util::autosize_hint(
        area,
        move || {
            text.track();
            next.track();
            running.track();
        },
        shown_next,
    );
    // One path for a tap and Tab: fill the draft, never send it. Focus in the
    // gesture handler itself so a phone can open its software keyboard.
    let accept_next = move || {
        if disabled.get_untracked() {
            return;
        }
        let Some(s) = shown_next() else { return };
        let Some(n) = next.get_untracked() else { return };
        // Kept: emptying the box again brings it back (until the next turn,
        // or Esc).
        text.set(s.clone());
        if let Some(el) = area.get_untracked() {
            el.set_value(&s);
            let _ = el.focus();
            let caret = s.encode_utf16().count() as u32;
            let _ = el.set_selection_range(caret, caret);
        }
        spawn_local(api::suggestion_seen(id.get_value(), n.for_seq, true));
    };
    // @-mention completion: (where the @ is, query), the matches, and the
    // highlighted one.
    let mention = RwSignal::new(None::<(usize, String)>);
    let picked = RwSignal::new(0usize);
    let matches = LocalResource::new(move || {
        let q = mention.get().map(|(_, q)| q);
        let place = files.and_then(|f| f.get());
        async move {
            match (q, place) {
                (Some(q), Some((worker, root))) => api::fs_find(worker, root, q).await.unwrap_or_default(),
                _ => Vec::new(),
            }
        }
    });
    // Things in the memex (people, clients, hosts, topics) for @.
    let things = LocalResource::new(move || {
        let q = mention.get().map(|(_, q)| q).filter(|q| q.chars().count() >= 2);
        async move {
            match q {
                Some(q) => api::memex_mentions(q).await.unwrap_or_default(),
                None => Vec::new(),
            }
        }
    });
    // In Ask: your projects, docs, work, and memex items of any kind.
    let refs = LocalResource::new(move || {
        let q = mention.get().map(|(_, q)| q).filter(|q| ask && q.chars().count() >= 2);
        async move {
            match q {
                Some(q) => api::ask_mentionables(q).await.unwrap_or_default(),
                None => Vec::new(),
            }
        }
    });
    // Slash commands: the query, and the commands the head offers.
    let command = RwSignal::new(None::<String>);
    let commands = LocalResource::new(move || async move {
        let mut list = api::commands(id.get_value()).await.unwrap_or_default();
        // A side chat has no /clear.
        list.retain(|c| !(inline && c.name == "clear" && c.source == "built-in"));
        list
    });
    let suggestions = move || -> Vec<Suggestion> {
        if let Some(q) = command.get() {
            let q = q.to_lowercase();
            let all = commands.get().unwrap_or_default();
            let (mut starts, contains): (Vec<_>, Vec<_>) =
                all.into_iter().filter(|c| c.name.to_lowercase().contains(&q)).partition(|c| c.name.to_lowercase().starts_with(&q));
            starts.extend(contains);
            return starts.into_iter().map(Suggestion::Command).collect();
        }
        if mention.with(Option::is_some) {
            let mut list: Vec<Suggestion> = matches.get().unwrap_or_default().into_iter().map(|(p, d)| Suggestion::File(p, d)).collect();
            list.extend(things.get().unwrap_or_default().into_iter().take(8).map(|t| {
                let key = if t.label.is_empty() { t.id.clone() } else { t.label.clone() };
                Suggestion::Thing(key, t.kind, t.name)
            }));
            list.extend(refs.get().unwrap_or_default().into_iter().take(10).map(|m| Suggestion::Ref(m.title, m.kind, m.reference)));
            list
        } else {
            Vec::new()
        }
    };
    let update_mention = move || {
        if let Some(el) = area.get_untracked() {
            let caret = el.selection_start().ok().flatten().unwrap_or(0) as usize;
            let t = text.get_untracked();
            let m = mention_at(&t, caret);
            let c = command_at(&t, caret);
            if m.as_ref().map(|x| &x.1) != mention.get_untracked().as_ref().map(|x| &x.1) || c != command.get_untracked() {
                picked.set(0);
            }
            mention.set(m);
            command.set(c);
        }
    };
    // The suggestions go on a press outside them and the box, or Escape.
    let suggesting = Memo::new(move |_| !suggestions().is_empty());
    let suggest_pop = StoredValue::new(crate::util::dismissable(
        move || suggesting.get(),
        move || {
            mention.try_set(None);
            command.try_set(None);
        },
    ));
    let complete_command = move |name: String| {
        let t = text.get_untracked();
        let rest = t.split_once(char::is_whitespace).map(|(_, r)| r.trim_start().to_owned()).unwrap_or_default();
        let insert = format!("/{name} ");
        let caret = insert.chars().count() as u32;
        text.set(insert + &rest);
        command.set(None);
        request_animation_frame(move || {
            if let Some(el) = area.try_get_untracked().flatten() {
                let _ = el.focus();
                let _ = el.set_selection_range(caret, caret);
            }
        });
    };
    let complete = move |path: String, dir: bool| {
        let Some((at, query)) = mention.get_untracked() else { return };
        let t = text.get_untracked();
        let chars: Vec<char> = t.chars().collect();
        let end = (at + 1 + query.chars().count()).min(chars.len());
        let insert = format!("@{path}{} ", if dir { "/" } else { "" });
        let new: String = chars[..at].iter().collect::<String>() + &insert + &chars[end..].iter().collect::<String>();
        let caret = (at + insert.chars().count()) as u32;
        text.set(new);
        mention.set(None);
        request_animation_frame(move || {
            if let Some(el) = area.try_get_untracked().flatten() {
                let _ = el.focus();
                let _ = el.set_selection_range(caret, caret);
            }
        });
    };
    // A reference for Ask, as a link the head reads when it arrives.
    let complete_ref = move |title: String, reference: String| {
        let Some((at, query)) = mention.get_untracked() else { return };
        let t = text.get_untracked();
        let chars: Vec<char> = t.chars().collect();
        let end = (at + 1 + query.chars().count()).min(chars.len());
        let insert = format!("[@{}](c6:{reference}) ", title.replace(['[', ']'], ""));
        let new: String = chars[..at].iter().collect::<String>() + &insert + &chars[end..].iter().collect::<String>();
        let caret = (at + insert.chars().count()) as u32;
        text.set(new);
        mention.set(None);
        request_animation_frame(move || {
            if let Some(el) = area.get_untracked() {
                let _ = el.focus();
                let _ = el.set_selection_range(caret, caret);
            }
        });
    };
    let add_files = move |ev: web_sys::Event| {
        spawn_local(async move {
            match js_read_files(&ev).await.ok().and_then(|v| v.as_string()) {
                Some(json) => match serde_json::from_str::<Vec<c6_api::AttachmentUpload>>(&json) {
                    Ok(files) => match too_large(&attachments.get_untracked(), &files) {
                        Some(e) => error.set(Some(e)),
                        None => attachments.update(|a| a.extend(files)),
                    },
                    Err(e) => error.set(Some(e.to_string())),
                },
                None => error.set(Some("could not read the file".into())),
            }
        });
    };
    let (col, ws) = (crate::workspace::use_column(), crate::workspace::use_workspace());
    // `now`: Send now (the agent gets it at once, not at its next step).
    let send_with = move |now: bool| {
        let t = text.get_untracked();
        let uploads = attachments.get_untracked();
        let saved_note = draft_note.get_untracked();
        if (t.trim().is_empty() && uploads.is_empty()) || disabled.get_untracked() || !draft_ready.get_untracked() {
            return;
        }
        // Commands the UI runs (unless a custom command took the name).
        let custom = |n: &str| {
            commands.get_untracked().and_then(|l| l.into_iter().find(|c| c.name == n).map(|c| c.source == "custom")).unwrap_or(false)
        };
        match t.trim() {
            "/export" if !custom("export") => {
                text.set(String::new());
                command.set(None);
                download(&crate::api::url(&format!("/api/v1/sessions/{}/export", id.get_value())));
                return;
            }
            "/clear" if !inline && !custom("clear") => {
                let Some(m) = model else { return };
                text.set(String::new());
                command.set(None);
                spawn_local(async move {
                    // The fresh session is where this one is (worker, folder,
                    // SSH target), with its labels.
                    let d = match api::session(id.get_value()).await {
                        Ok(d) => d,
                        Err(e) => {
                            error.set(Some(e));
                            return;
                        }
                    };
                    // It carries on this one's work (docs/work.md §4).
                    let work = api::work_of(format!("session:{}", d.session.id)).await.ok().and_then(|w| w.first().map(|w| w.id.clone()));
                    let req = c6_api::CreateSession {
                        work,
                        work_refs: Vec::new(),
                        todo: None,
                        labels: d.session.labels,
                        worker_id: d.session.worker_id,
                        cwd: d.session.cwd,
                        ssh: d.session.ssh,
                        connection_id: m.connection.get_untracked(),
                        model: m.model.get_untracked(),
                        title: None,
                        prompt: None,
                        attachments: Vec::new(),
                        effort: Some(m.effort.get_untracked()).filter(|e| !e.is_empty()),
                        worktree: false,
                        worktree_options: None,
                        realm: None,
                    };
                    match api::create_session(req).await {
                        Ok(s) => match col {
                            Some(c) => c.replace(crate::workspace::View::session(s.id)),
                            None => ws.open(None, crate::workspace::View::session(s.id), crate::workspace::How::Beside),
                        },
                        Err(e) => error.set(Some(e)),
                    }
                });
                return;
            }
            _ => {}
        }
        let switch = model.and_then(|m| m.switch());
        if switch.as_ref().is_some_and(|(c, m, _)| c.trim().is_empty() || m.is_empty()) {
            error.set(Some("Choose a connection and a model before sending; the session has not switched.".to_owned()));
            return;
        }
        text.set(String::new());
        // Sent: the suggestion was for before this message.
        next.set(None);
        attachments.set(Vec::new());
        draft_note.set(None);
        // Also clear a loss marker when no files survived restoration: the
        // empty attachment signal alone would not trigger a storage write.
        crate::util::forget_draft(&id.get_value());
        mention.set(None);
        command.set(None);
        let waking = wake.is_some_and(|w| w.get_untracked());
        spawn_local(async move {
            if let Err(e) = api::send_message_waking(id.get_value(), t.clone(), switch, uploads.clone(), waking, now).await {
                error.set(Some(e));
                text.set(t);
                attachments.set(uploads);
                draft_note.set(saved_note);
            } else {
                error.set(None);
            }
        });
    };
    let interrupt = move || {
        spawn_local(async move {
            if let Err(e) = api::interrupt(id.get_value()).await {
                error.set(Some(e));
            }
        });
    };
    let send = move || send_with(false);
    // The session's own Stop asks first while Claude Code's background agents
    // run (a side chat's box is another session's).
    let guard = if inline { None } else { use_context::<StopGuard>() };
    let stop = move |_| match guard {
        Some(g) if g.agents.get_untracked() > 0 => g.confirm.set(true),
        _ => interrupt(),
    };
    Effect::new(move || {
        if let Some(g) = guard
            && !running.get()
        {
            g.confirm.set(false);
        }
    });
    let esc_armed = use_context::<EscArmed>();
    // The session's own box keeps in line with its transcript.
    let beside = if inline { None } else { use_context::<GlanceBeside>() };
    // The ▾ beside Send while the agent works.
    let send_menu = RwSignal::new(false);
    let send_pop = StoredValue::new(crate::util::dismissable(
        move || send_menu.get(),
        move || {
            send_menu.try_set(false);
        },
    ));
    // Send now: the agent gets it at once. Claude Code moves what holds its
    // turn to the background (or stops only that turn), sparing its
    // background agents; carbon6's own loop stops the step and this starts
    // the next (docs/background.md §4).
    let send_now = move || {
        send_menu.set(false);
        send_with(true);
    };
    let has_text = Memo::new(move |_| !text.with(|t| t.trim().is_empty()) || !attachments.with(Vec::is_empty));
    // The ▾ menu goes with the buttons it belongs to.
    Effect::new(move || {
        if !(running.get() && has_text.get()) {
            send_menu.set(false);
        }
    });
    let picker = NodeRef::<leptos::html::Input>::new();
    let touch = crate::util::touch_first();
    // Desktop stays compact; touch gets a 36px primary target. The take
    // button shares this size and the toolbar's 4px right inset.
    const TAP_SEND: &str = "pointer-coarse:size-9";
    let send_label = if touch { "Send" } else { "Send (Enter)" };
    // Esc stops the session the page shows (not a side chat's).
    let stop_label = if touch || inline { "Stop" } else { "Stop (Esc)" };
    // A pending image shown full size (its index).
    let preview = RwSignal::new(None::<usize>);
    view! {
        <div
            class=if inline { "" } else { "shrink-0 border-t border-zinc-200 bg-zinc-50 dark:border-zinc-800 dark:bg-zinc-950" }
            on:dragover=|ev| ev.prevent_default()
            on:drop=move |ev| {
                let ev: web_sys::Event = ev.into();
                if js_has_files(&ev) {
                    ev.prevent_default();
                    add_files(ev);
                }
            }
        >
            // In line with the transcript, the glance's room beside it.
            <div class=if inline { "" } else { READING_ROW }>
            <div class=if inline { "space-y-2" } else { "w-full min-w-0 max-w-4xl space-y-2 px-2 py-1.5 sm:px-3 sm:py-2" }>
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                {move || (!draft_ready.get()).then(|| view! { <p class="text-xs text-muted" role="status">"Restoring draft attachments…"</p> })}
                {move || draft_note.get().map(|note| view! { <p class="text-xs text-muted" role="status">{note}</p> })}
                // One box: what's attached, the text, and a toolbar along its
                // bottom edge (📎, the chips, Send).
                <div class="relative rounded-xl border border-zinc-300 bg-white has-[textarea:focus]:border-zinc-400 has-[textarea:focus]:ring-2 has-[textarea:focus]:ring-zinc-300/60 dark:border-zinc-700 dark:bg-zinc-900 dark:has-[textarea:focus]:border-zinc-500 dark:has-[textarea:focus]:ring-zinc-700/60">
                {move || (!attachments.with(Vec::is_empty)).then(|| view! {
                    <div class="flex flex-wrap gap-2 px-2 pt-2">
                        {attachments.get().into_iter().enumerate().map(|(i, a)| {
                            let image = a.mime.starts_with("image/");
                            view! {
                                <div class="group relative flex items-center gap-1.5 rounded-md border border-zinc-200 bg-white p-1 pr-2 text-xs dark:border-zinc-700 dark:bg-zinc-900">
                                    {if image {
                                        view! {
                                            <button class="shrink-0 rounded focus-visible:ring-2 focus-visible:ring-blue-500" title="See it full size" on:click=move |_| preview.set(Some(i))>
                                                <img class="h-10 w-10 rounded object-cover" src=format!("data:{};base64,{}", a.mime, a.data) />
                                            </button>
                                        }.into_any()
                                    } else {
                                        view! { <span class="px-1">{crate::icons::file()}</span> }.into_any()
                                    }}
                                    <span class="max-w-40 truncate">{a.name.clone()}</span>
                                    <button class="text-muted hover:text-zinc-900 dark:hover:text-zinc-100" title="Remove" on:click=move |_| attachments.update(|v| { v.remove(i); })>{crate::icons::x()}</button>
                                </div>
                            }
                        }).collect_view()}
                    </div>
                })}
                // An image waiting to go, full size: Esc or a click outside closes it.
                {move || preview.get().and_then(|i| attachments.with(|v| v.get(i).cloned()).map(|a| (i, a))).map(|(i, a)| {
                    let size = a.data.len() * 3 / 4;
                    let size = if size >= 1 << 20 { format!("{:.1} MB", size as f64 / (1 << 20) as f64) } else { format!("{} KB", size.div_ceil(1024)) };
                    view! {
                        <FullImage src=format!("data:{};base64,{}", a.mime, a.data) name=a.name.clone() info=size close=move |_| preview.set(None)>
                            <button class=FULL_BUTTON
                                on:click=move |_| { attachments.update(|v| { if i < v.len() { v.remove(i); } }); preview.set(None) }>"Remove"</button>
                        </FullImage>
                    }
                })}
                    // @-mention suggestions, above the box.
                    {move || {
                        let list = suggestions();
                        (!list.is_empty()).then(|| view! {
                            <div data-pop=suggest_pop.get_value() class="absolute bottom-full left-0 z-30 mb-1 max-h-64 w-full max-w-lg overflow-y-auto rounded-md border border-zinc-200 bg-white py-1 text-xs shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
                                {list.into_iter().enumerate().map(|(i, s)| {
                                    let row = move || format!("block w-full truncate px-3 py-1 text-left {}", if picked.get() == i { "bg-zinc-100 dark:bg-zinc-800" } else { "" });
                                    match s {
                                        Suggestion::File(path, dir) => {
                                            let p2 = path.clone();
                                            view! {
                                                <button class=move || format!("{} font-mono", row()) on:mousedown=move |ev| { ev.prevent_default(); complete(p2.clone(), dir); }>
                                                    {if dir { format!("{path}/") } else { path }}
                                                </button>
                                            }.into_any()
                                        }
                                        Suggestion::Thing(key, kind, name) => {
                                            let k2 = key.clone();
                                            view! {
                                                <button class=row on:mousedown=move |ev| { ev.prevent_default(); complete(format!("memex:{k2}"), false); }>
                                                    <span class="rounded bg-violet-100 px-1 text-[10px] text-violet-800 dark:bg-violet-950 dark:text-violet-300">{kind}</span>
                                                    <span class="ml-1">{name}</span>
                                                    <span class="ml-2 font-mono text-muted">{key}</span>
                                                </button>
                                            }.into_any()
                                        }
                                        Suggestion::Ref(title, kind, reference) => {
                                            let (t2, r2) = (title.clone(), reference.clone());
                                            view! {
                                                <button class=row on:mousedown=move |ev| { ev.prevent_default(); complete_ref(t2.clone(), r2.clone()); }>
                                                    <span class="rounded bg-sky-100 px-1 text-[10px] text-sky-800 dark:bg-sky-950 dark:text-sky-300">{kind}</span>
                                                    <span class="ml-1">{title}</span>
                                                </button>
                                            }.into_any()
                                        }
                                        Suggestion::Command(c) => {
                                            let n2 = c.name.clone();
                                            view! {
                                                <button class=row on:mousedown=move |ev| { ev.prevent_default(); complete_command(n2.clone()); }>
                                                    <span class="font-mono">{format!("/{}", c.name)}</span>
                                                    {(!c.argument_hint.is_empty()).then(|| view! { <span class="ml-1 font-mono text-muted">{c.argument_hint.clone()}</span> })}
                                                    <span class="ml-2 text-muted">{c.description.clone()}</span>
                                                    {(c.source != "built-in").then(|| view! { <span class="ml-2 rounded bg-zinc-100 px-1 text-[10px] text-muted dark:bg-zinc-800">{c.source.clone()}</span> })}
                                                </button>
                                            }.into_any()
                                        }
                                    }
                                }).collect_view()}
                            </div>
                        })
                    }}
                    // The suggestion is the empty box's text, in italics, with a
                    // small ⇥ at its right edge: tap it, or press Tab or →.
                    // Any text in the box (typed, pasted) puts both away.
                    <div class="relative">
                    <textarea
                        node_ref=area
                        class=move || format!("block w-full bg-transparent px-3 pb-1.5 pt-2 text-sm outline-none {} {}", crate::util::GROWS, match (shown_next().is_some(), touch) { (true, true) => "pr-11 placeholder:italic", (true, false) => "placeholder:italic", _ => "" })
                        rows="1"
                        enterkeyhint=if touch { "enter" } else { "send" }
                        data-pop=suggest_pop.get_value()
                        placeholder=move || if let Some(s) = shown_next() { s } else if disabled.get() { "This session failed to start.".to_owned() } else if touch {
                            // A phone's box is narrow: a hint that fits.
                            if running.get() { "Read at its next step…" } else { "@ to mention · / for commands" }.to_owned()
                        } else if running.get() { "Read at the agent's next step…".to_owned() } else { "@ to mention · / for commands".to_owned() }
                        prop:disabled=move || disabled.get()
                        prop:value=move || text.get()
                        on:input=move |ev| {
                            text.set(event_target_value(&ev));
                            update_mention();
                            if let Some(el) = area.get_untracked() { crate::util::fit(&el); }
                        }
                        on:click=move |_| update_mention()
                        on:paste=move |ev| {
                            let ev: web_sys::Event = ev.into();
                            if js_has_files(&ev) {
                                ev.prevent_default();
                                add_files(ev);
                            }
                        }
                        on:keydown=move |ev| {
                            let list = suggestions();
                            if !list.is_empty() {
                                match ev.key().as_str() {
                                    "ArrowDown" => { ev.prevent_default(); picked.update(|p| *p = (*p + 1) % list.len()); return; }
                                    "ArrowUp" => { ev.prevent_default(); picked.update(|p| *p = (*p + list.len() - 1) % list.len()); return; }
                                    "Enter" | "Tab" => {
                                        let s = list[picked.get_untracked().min(list.len() - 1)].clone();
                                        match s {
                                            Suggestion::File(path, dir) => complete(path, dir),
                                            Suggestion::Thing(key, _, _) => complete(format!("memex:{key}"), false),
                                            Suggestion::Ref(title, _, reference) => complete_ref(title, reference),
                                            // Enter on a command already typed in full sends it.
                                            Suggestion::Command(c) if crate::util::sends(&ev) && command.get_untracked().as_deref() == Some(c.name.as_str()) => {
                                                ev.prevent_default();
                                                command.set(None);
                                                send();
                                                return;
                                            }
                                            Suggestion::Command(c) => complete_command(c.name),
                                        }
                                        ev.prevent_default();
                                        return;
                                    }
                                    "Escape" => { ev.prevent_default(); ev.stop_propagation(); mention.set(None); command.set(None); return; }
                                    _ => {}
                                }
                            }
                            // The suggested next message: Tab (or →, the box being
                            // empty) takes it, Esc puts it away.
                            if shown_next().is_some() && matches!(ev.key().as_str(), "Tab" | "Escape" | "ArrowRight") {
                                ev.prevent_default();
                                if ev.key() != "Escape" {
                                    accept_next();
                                } else {
                                    next.set(None);
                                }
                                return;
                            }
                            // While the agent works, ⌘/Ctrl+Shift+Enter is Send now.
                            if crate::util::sends_now(&ev) && running.get_untracked() && has_text.get_untracked() {
                                ev.prevent_default();
                                send_now();
                            } else if crate::util::sends(&ev) {
                                ev.prevent_default();
                                send();
                            }
                        }
                        on:keyup=move |ev| if matches!(ev.key().as_str(), "ArrowLeft" | "ArrowRight" | "Home" | "End") { update_mention() }
                    ></textarea>
                    // Touch screens only (a keyboard has Tab): drawn, not a ⇥
                    // character, which a phone may show as a coloured emoji.
                    {move || (touch && shown_next().is_some()).then(|| view! {
                        <button type="button"
                            class=format!("{SEND} {TAP_SEND} absolute right-1 top-1.5 rounded border border-sky-300 bg-sky-50 text-sky-700 hover:bg-sky-100 dark:border-sky-800 dark:bg-sky-950 dark:text-sky-300 dark:hover:bg-sky-900")
                            title="Use the suggestion: it goes in the box to edit or send, not sent" aria-label="Use the suggestion"
                            prop:disabled=move || disabled.get()
                            // Keep the box's focus (and a phone's keyboard) as it's tapped.
                            on:mousedown=|ev| ev.prevent_default()
                            on:click=move |_| accept_next()>
                            <svg viewBox="0 0 16 16" class="h-3.5 w-3.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                <path d="M2 8h9M8 4.5 11.5 8 8 11.5M13.5 3.5v9" />
                            </svg>
                        </button>
                    })}
                    </div>
                    // The toolbar: +, the model (its chip opening one menu above it
                    // for the plan's limits, the model and its subagents', a change waiting for Send
                    // beside), and Send.
                    <div class="flex items-center gap-1 px-1 pb-1">
                        // Drawn, not an emoji, so it matches the other icons.
                        <button class="flex size-6.5 shrink-0 items-center justify-center rounded-md text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                            title="Attach images or files (or paste / drop them)" aria-label="Attach images or files" on:click=move |_| { if let Some(i) = picker.get_untracked() { i.click(); } }>
                            <svg viewBox="0 0 16 16" class="h-3.5 w-3.5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
                                <path d="M8 2.5v11M2.5 8h11" />
                            </svg>
                        </button>
                        <input node_ref=picker type="file" multiple class="hidden" on:change=add_files />
                {(model.is_some() || subagents.is_some()).then(|| view! {
                    <div class="relative flex min-w-0 flex-1 flex-wrap items-center gap-1 text-xs">
                        {model.map(|m| view! {
                            {move || {
                                let (_, mm, e) = m.current.get();
                                let mut label = model_label(&mm, &e);
                                // Subagents on another model say so; on the
                                // session's, nothing.
                                if let Some((_, sm, se)) = subagents.map(|s| s.current.get()).filter(|(_, sm, _)| !sm.is_empty()) {
                                    label.push_str(&format!(" · subagents {}", model_label(&sm, &se)));
                                }
                                view! {
                                    <button class=CHIP title="The model this session and its subagents run on: change them" aria-label=format!("Model: {label}, change it")
                                        aria-expanded=move || picking.get().to_string() data-pop=picking_pop.get_value() on:click=move |_| toggle()>
                                        <span class="min-w-0 truncate">{label.clone()}</span>
                                        // What's left of its plan, when it has one: "· 90%".
                                        <crate::quota::QuotaNote connection=Signal::derive(move || m.current.get().0) chip=true />
                                        <span class="text-muted">{crate::icons::chevron_down()}</span>
                                    </button>
                                }
                            }}
                            {move || switch_note(m, kinds.get().and_then(Result::ok).unwrap_or_default()).map(|(text, title)| view! {
                                <button class="rounded-full bg-amber-100 px-2 py-0.5 text-amber-800 dark:bg-amber-950 dark:text-amber-300" title=title data-pop=picking_pop.get_value() on:click=move |_| toggle()>{text}</button>
                            })}
                        })}
                        // No model chip to hold them (no caller does this today):
                        // subagents keep a chip of their own.
                        {subagents.filter(|_| model.is_none()).map(|s| view! {
                            {move || { let (_, mm, e) = s.current.get(); let label = (!mm.is_empty()).then(|| model_label(&mm, &e)); view! {
                                <button class=CHIP title=SUBAGENTS_HELP aria-label=format!("Subagents' model: {}, change it", label.as_deref().unwrap_or("the session's"))
                                    aria-expanded=move || picking.get().to_string() data-pop=picking_pop.get_value() on:click=move |_| toggle()>
                                    <span class="min-w-0 truncate">{match &label { Some(l) => format!("subagents: {l}"), None => "subagents".to_owned() }}</span>
                                    <span class="text-muted">{crate::icons::chevron_down()}</span>
                                </button>
                            } }}
                        })}
                        {move || picking.get().then(|| view! {
                            <div data-pop=picking_pop.get_value() class="absolute bottom-full left-0 z-40 mb-1 w-max max-w-[90vw] sm:max-w-xl space-y-2 rounded-lg border border-zinc-200 bg-white p-3 text-zinc-700 shadow-lg dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                                // The plan's windows first, when it has one.
                                {model.map(|m| view! { <crate::quota::PlanMenu connection=Signal::derive(move || m.current.get().0) /> })}
                                {model.map(|m| view! {
                                    <div class="font-medium">"Model"</div>
                                    <div class="flex flex-wrap items-center gap-2"><ModelPicker connection=m.connection model=m.model effort=m.effort no_claude_code claude_off=claude_off /></div>
                                    {move || switch_note(m, kinds.get().and_then(Result::ok).unwrap_or_default()).map(|(text, title)| view! {
                                        <p class="text-amber-700 dark:text-amber-400">{text}{(!title.is_empty()).then(|| view! { <span class="block text-muted">{title}</span> })}</p>
                                    })}
                                    <p class="text-muted">"It switches when you send your next message."</p>
                                })}
                                {subagents.map(|s| {
                                    let set = move |req: c6_api::SetSubagentModel| {
                                        spawn_local(async move {
                                            if let Err(e) = api::set_subagent_model(subagent_id.get_value(), req).await {
                                                error.set(Some(e));
                                            }
                                        });
                                    };
                                    // Turned off here but nothing Set yet: the
                                    // picker shows until they Set (or close this).
                                    let choosing = RwSignal::new(false);
                                    let same = Signal::derive(move || s.current.get().1.is_empty() && !choosing.get());
                                    // Once a change lands, the switch follows it.
                                    Effect::new(move || { s.current.track(); choosing.set(false); });
                                    let flip = move |on: bool| {
                                        choosing.set(!on);
                                        if on && !s.current.get_untracked().1.is_empty() {
                                            set(c6_api::SetSubagentModel::default());
                                        }
                                    };
                                    view! {
                                        <div class=if model.is_some() { "space-y-2 border-t border-zinc-200 pt-2 dark:border-zinc-700" } else { "space-y-2" }>
                                            <div class="font-medium">"Subagents"</div>
                                            <p class="max-w-md text-muted">{SUBAGENTS_HELP}</p>
                                            <Switch on=same on_change=flip label="Same as the session" title="Run them on the session's own model, or turn off to choose theirs" />
                                            {move || (!same.get()).then(|| view! {
                                                <div class="flex flex-wrap items-center gap-2">
                                                    <ModelPicker connection=s.connection model=s.model effort=s.effort no_claude_code=true />
                                                    {move || s.change().map(|req| view! {
                                                        <button class="rounded-md bg-violet-600 px-2 py-1 text-white hover:bg-violet-700" on:click=move |_| set(req.clone())>"Set"</button>
                                                    })}
                                                </div>
                                            })}
                                        </div>
                                    }
                                })}
                            </div>
                        })}
                    </div>
                })}
                        <div class="relative ml-auto flex shrink-0 items-center gap-0.5">
                        {move || if running.get() && esc_armed.is_some_and(|EscArmed(a)| a.get().is_some()) {
                            // One Esc pressed: Stop says what the next one does.
                            view! { <button class=format!("{STOP} w-auto px-1.5 text-[10px] font-semibold") title="Press Esc again to stop" aria-label="Press Esc again to stop" on:click=stop>"Esc"</button> }.into_any()
                        } else if running.get() && has_text.get() {
                            // Both runtimes take it into the running turn (Claude Code's
                            // process stays up: docs/background.md §4).
                            // Send (the agent reads it at its next step), and ▾
                            // beside it for Send now (at once).
                            view! {
                                // One split button: Send, then its ▾ (Send now) past a
                                // full-height darker line, so the two read as one. Its
                                // halves are rounded, not the box: a clipping box hid
                                // the menu above it.
                                <div class="relative flex items-stretch">
                                    <button class=format!("{SEND} {TAP_SEND} rounded-l-md rounded-r-none {SEND_READY}") title=format!("{send_label}: the agent reads it at its next step") aria-label=send_label
                                        prop:disabled=move || disabled.get() || !draft_ready.get() on:click=move |_| send()>{send_icon()}</button>
                                    <button class="flex h-5.5 w-4 shrink-0 items-center justify-center rounded-r-md border-l border-sky-800 bg-sky-600 text-white hover:bg-sky-700 pointer-coarse:h-9 pointer-coarse:w-7"
                                        title="More ways to send" aria-label="More ways to send"
                                        aria-haspopup="menu" prop:aria-expanded=move || send_menu.get().to_string() data-pop=send_pop.get_value()
                                        on:click=move |ev| { ev.stop_propagation(); send_menu.update(|o| *o = !*o); }>
                                        <svg viewBox="0 0 10 10" class="h-2.5 w-2.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.5 4 5 6.5 7.5 4" /></svg>
                                    </button>
                                    {move || send_menu.get().then(|| view! {
                                        <div role="menu" data-pop=send_pop.get_value() class="absolute bottom-full right-0 z-40 mb-1 w-max rounded-md border border-zinc-200 bg-white py-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
                                            <button role="menuitem" class="flex w-full items-center gap-4 px-3 py-1.5 text-left hover:bg-zinc-100 pointer-coarse:py-3 dark:hover:bg-zinc-800"
                                                title="The agent gets it now, not at its next step (Claude Code moves what it waits on to the background; its background agents go on)" on:click=move |_| send_now()>
                                                <span>"Send now"</span>
                                                <span class="text-xs text-muted">"⌘/Ctrl ⇧ ↵"</span>
                                            </button>
                                        </div>
                                    })}
                                </div>
                            }.into_any()
                        } else if running.get() {
                            view! { <button class=STOP title=stop_label aria-label="Stop" on:click=stop>{stop_icon()}</button> }.into_any()
                        } else {
                            view! {
                                <button class=move || format!("{SEND} {TAP_SEND} {SEND_SHAPE} {}", if has_text.get() { SEND_READY } else { SEND_EMPTY })
                                    title=send_label aria-label=send_label prop:disabled=move || disabled.get() || !draft_ready.get() on:click=move |_| send()>{send_icon()}</button>
                            }.into_any()
                        }}
                        // Stop (or Esc twice) while Claude Code's own agents run
                        // in the background: they'd stop too, so it asks.
                        {move || guard.filter(|g| g.confirm.get() && running.get()).map(|g| view! {
                            <div class="fixed inset-0 z-30" on:click=move |_| g.confirm.set(false)></div>
                            <div role="alertdialog" class="absolute bottom-full right-0 z-40 mb-1 w-72 max-w-[90vw] space-y-2 rounded-md border border-zinc-200 bg-white p-3 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
                                <p>{move || stop_question(g.agents.get())}</p>
                                <div class="flex flex-wrap justify-end gap-2">
                                    {move || has_text.get().then(|| view! {
                                        <button class="rounded-md px-2 py-1 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                            title="Don't stop: the agent reads your message at its next step"
                                            on:click=move |_| { g.confirm.set(false); send(); }>"Send at the next step instead"</button>
                                    })}
                                    <button class="rounded-md px-2 py-1 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| g.confirm.set(false)>"Cancel"</button>
                                    <button class="rounded-md bg-red-600 px-2 py-1 text-white hover:bg-red-700"
                                        on:click=move |_| { g.confirm.set(false); interrupt(); }>"Stop anyway"</button>
                                </div>
                            </div>
                        })}
                        </div>
                    </div>
                </div>
                {wake.map(|w| view! {
                    <label class="flex items-center gap-1.5 self-start text-xs text-muted"
                        title="Its report reaches the agent that started it at that agent's next step if it is working; otherwise with its next message, unless this wakes it now.">
                        <input type="checkbox" prop:checked=move || w.get() on:change=move |ev| w.set(event_target_checked(&ev)) />
                        "Wake the agent that started it"
                    </label>
                })}
            </div>
            {beside.map(|GlanceBeside(b)| glance_room(b))}
            </div>
        </div>
    }
}

/// A tool call held for your approval: what it would do, and Allow / Deny
/// while the session still waits.
#[component]
fn ApprovalCard(
    session: String,
    call_id: String,
    tool: String,
    input: String,
    policy: String,
    message: String,
    open: Signal<bool>,
) -> impl IntoView {
    let live = api::live();
    let sid = StoredValue::new(session);
    let call = StoredValue::new(call_id);
    let sent = RwSignal::new(None::<Result<bool, String>>);
    // Its session's row says whether it waits; a subagent's isn't in the live
    // list, so its log alone says (the head refuses a stale answer).
    let waiting =
        move || open.get() && live.sessions.with(|ss| ss.iter().find(|s| s.id == sid.get_value()).is_none_or(|s| s.awaiting_approval));
    // Ask's carbon6 call is "<op> <input>", or what its message says it
    // gives (a session's first message), shown as it is.
    let c6 = tool_kind(&tool) == "c6_call";
    let parsed =
        if c6 { c6_approval_op(&input) } else { serde_json::from_str::<serde_json::Value>(&input).unwrap_or(serde_json::Value::Null) };
    // A shell command reads best as itself.
    let shown = parsed["command"].as_str().filter(|_| !c6).map(str::to_owned).unwrap_or_else(|| input.clone());
    // What it would do, in words, where the tool is one we can say it for.
    let (label, server) = tool_label(&tool);
    let raw = parsed.to_string();
    let what = Some(summarize(&tool, &parsed)).filter(|w| !w.is_empty() && *w != raw && *w != shown && !(c6 && parsed.is_null()));
    let answer = move |allow: bool| {
        spawn_local(async move {
            sent.set(Some(api::decide(sid.get_value(), call.get_value(), allow).await.map(|_| allow)));
        });
    };
    view! {
        <div id=waiting_card_id(&sid.get_value(), &call.get_value()) class="space-y-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-900 dark:bg-amber-950/60">
            <div class="flex flex-wrap items-center gap-2">
                <span>{crate::icons::warning()}</span>
                <span class="font-medium">"Waiting for your approval"</span>
                <span class="text-xs text-muted">{format!("policy “{policy}”")}</span>
            </div>
            {(!message.is_empty()).then(|| view! { <p class="text-xs text-zinc-600 dark:text-zinc-400">{message.clone()}</p> })}
            <div class="text-sm" title=tool.clone()>
                <span class="font-medium">{label}</span>
                {what.map(|w| format!(": {w}"))}
                {server.map(|s| view! { <span class="text-xs text-muted">{format!(" · {s}")}</span> })}
            </div>
            <pre class="max-h-48 overflow-auto whitespace-pre-wrap rounded-md bg-white p-2 font-mono text-xs dark:bg-zinc-900">{shown}</pre>
            {move || match sent.get() {
                Some(Ok(true)) => view! { <p class="text-xs text-emerald-700">"Approved."</p> }.into_any(),
                Some(Ok(false)) => view! { <p class="text-xs text-red-600">"Denied."</p> }.into_any(),
                Some(Err(e)) => view! { <p class="text-xs text-red-600">{e}</p> }.into_any(),
                None if waiting() => view! {
                    <div class="flex gap-2">
                        <button class=BUTTON on:click=move |_| answer(true)>"Allow"</button>
                        <button class="rounded-md border border-zinc-300 px-3 py-1.5 text-sm hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800" on:click=move |_| answer(false)>"Deny"</button>
                    </div>
                }.into_any(),
                None => ().into_any(),
            }}
        </div>
    }
}

/// The assistant's question: a form while it waits, with choices from your
/// setup.
#[component]
fn QuestionCard(session: String, call_id: String, title: String, fields: Vec<c6_api::QuestionField>, open: Signal<bool>) -> impl IntoView {
    let sid = StoredValue::new(session);
    let call = StoredValue::new(call_id);
    let values: Vec<(c6_api::QuestionField, RwSignal<String>)> = fields
        .into_iter()
        .map(|f| {
            let initial = match &f.default {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Null) | None => match f.r#type.as_str() {
                    "boolean" => "false".into(),
                    "choice" => f.choices.first().map(|c| c.value.clone()).filter(|_| f.required).unwrap_or_default(),
                    _ => String::new(),
                },
                Some(v) => v.to_string(),
            };
            (f, RwSignal::new(initial))
        })
        .collect();
    let values = StoredValue::new(values);
    let error = RwSignal::new(None::<String>);
    let sent = RwSignal::new(false);
    let submit = move |cancelled: bool| {
        let mut out = serde_json::Map::new();
        for (f, v) in values.get_value() {
            let raw = v.get_untracked();
            if f.required && raw.trim().is_empty() && !cancelled {
                error.set(Some(format!("{} is needed", if f.label.is_empty() { &f.name } else { &f.label })));
                return;
            }
            let value = match f.r#type.as_str() {
                _ if raw.trim().is_empty() => serde_json::Value::Null,
                "number" => match raw.trim().parse::<f64>() {
                    Ok(n) => serde_json::json!(n),
                    Err(_) => {
                        error.set(Some(format!("{} is a number", f.label)));
                        return;
                    }
                },
                "boolean" => serde_json::Value::Bool(raw == "true"),
                _ => serde_json::Value::String(raw),
            };
            out.insert(f.name.clone(), value);
        }
        sent.set(true);
        spawn_local(async move {
            if let Err(e) = api::answer(sid.get_value(), call.get_value(), out, cancelled).await {
                error.set(Some(e));
                sent.set(false);
            }
        });
    };
    view! {
        <div id=waiting_card_id(&sid.get_value(), &call.get_value()) class="space-y-3 rounded-lg border border-sky-300 bg-sky-50 p-3 text-sm dark:border-sky-900 dark:bg-sky-950/40">
            <p class="flex items-center gap-2 font-medium"><span>{crate::icons::pencil()}</span><span>{title}</span></p>
            {move || open.get().then(|| view! {
                <div class="space-y-2">
                    {values.get_value().into_iter().map(|(f, v)| {
                        let label = if f.label.is_empty() { f.name.clone() } else { f.label.clone() };
                        let input = match f.r#type.as_str() {
                            "choice" => {
                                let choices = f.choices.iter().map(|c| (c.value.clone(), if c.label.is_empty() { c.value.clone() } else { c.label.clone() })).collect();
                                view! { <crate::util::Choices choices value=v optional=!f.required /> }.into_any()
                            }
                            "boolean" => view! {
                                <input type="checkbox" prop:checked=move || v.get() == "true" on:change=move |ev| v.set(event_target_checked(&ev).to_string()) />
                            }.into_any(),
                            "longtext" => view! {
                                <textarea class=format!("{INPUT} min-h-20 text-sm") prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev))></textarea>
                            }.into_any(),
                            kind => {
                                let t = match kind { "secret" => "password", "number" => "number", _ => "text" };
                                view! { <input type=t autocomplete="off" class=format!("{INPUT} py-1 text-sm") prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev)) /> }.into_any()
                            }
                        };
                        view! {
                            <label class="block space-y-0.5">
                                <span class="text-xs text-zinc-600 dark:text-zinc-400">{label}{f.required.then_some(" *")}</span>
                                {input}
                                {(!f.description.is_empty()).then(|| view! { <span class="block text-[11px] text-muted">{f.description.clone()}</span> })}
                                {(f.r#type == "secret").then(|| view! { <span class="block text-[11px] text-muted">"Kept from the model: it only passes it on."</span> })}
                            </label>
                        }
                    }).collect_view()}
                    {move || error.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
                    <div class="flex gap-2">
                        <button class=BUTTON disabled=move || sent.get() on:click=move |_| submit(false)>"Send"</button>
                        <button class="rounded-md border border-zinc-300 px-3 py-1.5 text-sm hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800" disabled=move || sent.get() on:click=move |_| submit(true)>"Cancel"</button>
                    </div>
                </div>
            })}
        </div>
    }
}

/// A secret an operation returned, for you: copy it now.
#[component]
fn SecretCardView(card: SecretCard, on_dismiss: Callback<String>) -> impl IntoView {
    let copied = RwSignal::new(false);
    let value = StoredValue::new(card.value.clone());
    let id = card.call_id.clone();
    view! {
        <div class="space-y-2 rounded-lg border border-emerald-300 bg-emerald-50 p-3 text-sm dark:border-emerald-900 dark:bg-emerald-950/40">
            <div class="flex items-center gap-2">
                <span>{crate::icons::key()}</span>
                <span class="font-medium">{card.title.clone()}</span>
                <span class="text-xs text-muted">"only you see this; it is not kept"</span>
                <span class="flex-1"></span>
                <button class="text-xs text-muted hover:underline" on:click=move |_| on_dismiss.run(id.clone())>"Dismiss"</button>
            </div>
            <pre class="select-all overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-white p-2 font-mono text-xs dark:bg-zinc-900">{card.value.clone()}</pre>
            <div class="flex items-center gap-2">
                <button class=BUTTON on:click=move |_| {
                    let _ = window().navigator().clipboard().write_text(&value.get_value());
                    copied.set(true);
                }>{move || if copied.get() { "Copied" } else { "Copy" }}</button>
                <span class="text-xs text-zinc-600 dark:text-zinc-400">{card.note.clone()}</span>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn thinking_is_absent_by_default_and_expanded_when_enabled() {
        let owner = Owner::new();
        owner.with(|| {
            let render = |show| {
                view! {
                    <Thinking text="Reasoning, not a status update".to_owned() show=Signal::stored(show) />
                }
                .to_html()
            };
            let hidden = render(c6_api::TranscriptSettings::default().show_thinking);
            assert!(!hidden.contains("Reasoning"), "hidden thinking must not enter the DOM: {hidden}");
            assert!(!hidden.contains("<details"), "no empty thinking box: {hidden}");
            assert!(!hidden.contains("<summary"), "no folded label: {hidden}");
            let shown = render(true);
            assert!(shown.contains("Reasoning, not a status update"), "{shown}");
            assert!(shown.contains(">Thinking</summary>"), "{shown}");
            let details = shown.split('>').find(|s| s.contains("<details")).unwrap();
            assert!(details.contains(" open"), "thinking starts expanded: {shown}");
        });
    }

    #[test]
    fn thinking_only_events_hide_without_hiding_tools_text_or_interruption() {
        let thinking = ContentBlock::Thinking { text: "Reasoning".into() };
        let event = |blocks, stop_reason: &str| EventKind::Assistant { turn: 1, blocks, stop_reason: stop_reason.into() };
        assert!(thinking_only(&event(vec![thinking.clone()], "tool_use")), "no empty event or branching controls");
        assert!(!thinking_only(&event(vec![thinking.clone()], "interrupted")), "keep the interruption notice");
        assert!(
            !thinking_only(&event(vec![thinking.clone(), ContentBlock::Text { text: "Status update".into() }], "end_turn")),
            "keep ordinary text"
        );
        assert!(
            !thinking_only(&event(
                vec![thinking, ContentBlock::ToolUse { id: "t1".into(), name: "bash".into(), input_json: "{}".into() }],
                "tool_use"
            )),
            "a reasoning-plus-tools turn still shows its tool cards"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn subagent_statuses_and_template_names_keep_their_icons() {
        for (running, end, text) in [
            (true, None, "running"),
            (false, Some(TurnEndReason::Completed), "done"),
            (false, Some(TurnEndReason::WorkerRestarted), "lost"),
            (false, Some(TurnEndReason::Error), "failed"),
            (false, None, "starting"),
        ] {
            let html = subagent_state(running, end).0.to_html();
            assert!(html.contains("<svg"), "{text} lost its icon: {html}");
            assert!(html.contains(text), "{html}");
            crate::icons::tests::assert_no_icon_glyphs(&html);
        }
        let mut s = session(W, false);
        s.remote_template = "review".into();
        let html = subagent_name(&s).unwrap().to_html();
        assert!(html.contains("<svg"), "template lost its icon: {html}");
        assert!(html.contains("review"), "{html}");
        // This state never had a symbol; don't invent one during restoration.
        assert!(!subagent_state(false, Some(TurnEndReason::Interrupted)).0.to_html().contains("<svg"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn rendered_session_chrome_keeps_drawn_icons_not_emoji() {
        use crate::icons::tests::assert_no_icon_glyphs;
        let owner = Owner::new();
        owner.with(|| {
            provide_context(api::LiveFeed {
                workers: signal(Vec::new()).0,
                sessions: signal(Vec::new()).0,
                connected: signal(false).0,
                doc: signal(None).0,
                marks: signal(Default::default()).0,
                attention: signal(0).0,
            });
            // Content deliberately has no emoji: this checks carbon6's
            // markup, not a user's text, filename or an agent's reply.
            let fixtures = [
                notice_line("Build finished", "Ready", "high").to_html(),
                notice_line("Build finished", "", "normal").to_html(),
                memory_line("remember", "n1", "", "Use this branch").to_html(),
                memory_line("forget", "n1", "", "").to_html(),
                transcript_file("/notes.txt".into(), "notes.txt".into(), 42).to_html(),
                crate::outputs::published_line("s1", 1, "report.txt", "Report", "text/plain", 42).to_html(),
                view! { <ApprovalCard session="s1".into() call_id="a1".into() tool="bash".into() input="echo ready".into() policy="ask".into() message="Review".into() open=Signal::derive(|| true) /> }.to_html(),
                view! { <QuestionCard session="s1".into() call_id="q1".into() title="Which branch?".into() fields=Vec::new() open=Signal::derive(|| true) /> }.to_html(),
            ];
            for html in fixtures {
                assert!(!html.is_empty(), "empty fixture");
                assert!(html.contains("<svg"), "keep the icon: {html}");
                assert_no_icon_glyphs(&html);
            }
            let card = SecretCard { call_id: "secret1".into(), title: "Token".into(), value: "sample".into(), note: "Copy now".into() };
            let html = view! { <SecretCardView card on_dismiss=Callback::new(|_: String| {}) /> }.to_html();
            assert!(html.contains("<svg"), "the key must be drawn: {html}");
            assert_no_icon_glyphs(&html);
        });
    }

    #[test]
    fn what_waits_on_you_includes_decisions_cards_and_subagents_outside_the_loaded_log() {
        let approval = |seq: u64, call: &str, state: &str| Event {
            seq,
            ts: 0,
            kind: EventKind::Approval {
                call_id: call.into(),
                state: state.into(),
                tool: "bash".into(),
                input: r#"{"command":"cargo publish\n--dry-run"}"#.into(),
                policy: "ask".into(),
                message: String::new(),
            },
        };
        let question = |seq: u64, call: &str, state: &str| Event {
            seq,
            ts: 0,
            kind: EventKind::Question {
                call_id: call.into(),
                title: "Which branch?".into(),
                fields: Vec::new(),
                state: state.into(),
                answer: serde_json::Value::Null,
            },
        };
        let evs = [
            approval(1, "a1", "requested"),
            question(2, "q1", "asked"),
            approval(3, "a2", "requested"),
            approval(4, "a2", "allowed"),
            question(5, "q2", "asked"),
            question(6, "q2", "answered"),
        ];
        let item = |kind: &str, session: &str, call: Option<&str>, decision: i64| c6_api::AttentionItem {
            kind: kind.into(),
            title: "Choose a branch".into(),
            detail: "Nested agent".into(),
            session_id: Some(session.into()),
            call_id: call.map(str::to_owned),
            decision_id: Some(decision),
            ..Default::default()
        };
        let items = [
            item("approval", "parent", Some("a1"), 1),
            item("question", "parent", Some("q1"), 2),
            item("decision", "parent", None, 3),
            item("task", "parent", None, 4),
            item("approval", "nested", Some("a3"), 5),
            item("question", "nested", Some("q3"), 6),
        ];
        let w = waiting_on_you("parent", &evs, &items);
        assert_eq!(w.len(), 6);
        assert_eq!(w[0].target, WaitingTarget::Call { session: "parent".into(), call: "a1".into(), seq: Some(1) });
        assert_eq!(w[1].target, WaitingTarget::Call { session: "parent".into(), call: "q1".into(), seq: Some(2) });
        assert_eq!(w[2].target, WaitingTarget::Decision(3));
        assert_eq!(w[3].target, WaitingTarget::Decision(4));
        assert_eq!(w[4].target, WaitingTarget::Call { session: "nested".into(), call: "a3".into(), seq: None });
        assert!(w[5].what.contains("Nested agent"));
        // Answered cards don't survive just because their old log is loaded.
        assert!(waiting_on_you("parent", &evs, &[]).is_empty());
        // An approval outside the event window still has an answerable target.
        assert_eq!(
            waiting_on_you("parent", &[], &items)[0].target,
            WaitingTarget::Call { session: "parent".into(), call: "a1".into(), seq: None }
        );
    }

    /// Text streamed for a turn the log says ended is already in its
    /// messages: shown again, it doubles the reply, then stands in for the
    /// next turn's until that one's first message arrives.
    #[test]
    fn late_text_for_an_ended_turn_is_not_shown() {
        let l = Logged { ended: 3, ..Default::default() };
        assert!(l.has(3, 0), "its turn ended");
        assert!(l.has(2, 0), "an earlier turn");
        assert!(!l.has(4, 0), "the turn running now");
        assert!(!Logged::default().has(1, 0), "no turn has ended yet");
        assert!(!l.has(0, 0), "a stream that names no turn is shown");
    }

    fn logged(seq: u64, kind: serde_json::Value) -> Event {
        let mut e = kind;
        e["seq"] = seq.into();
        e["ts"] = 0.into();
        serde_json::from_value(e).unwrap()
    }

    fn reply(seq: u64, text: &str) -> Event {
        logged(
            seq,
            serde_json::json!({"type": "assistant", "turn": 3, "blocks": [{"type": "text", "text": text}], "stop_reason": "end_turn"}),
        )
    }

    fn result(seq: u64, call_id: &str) -> Event {
        logged(
            seq,
            serde_json::json!({"type": "tool_result", "call_id": call_id, "tool": "Bash", "output": "", "is_error": false, "truncated": false, "duration_ms": 0}),
        )
    }

    fn text(seq: u64, text: &str) -> StreamMsg {
        StreamMsg::Text { turn: 3, text: text.into(), seq }
    }

    /// Opened (or caught up) while a turn runs: the head sends the log, which
    /// already has a reply, then what streamed live meanwhile, that reply's
    /// text too. Its turn is still running, so only where the text streamed
    /// in the log tells it is in that reply; the reply is drawn once.
    #[test]
    fn text_a_reply_in_the_log_already_has_is_not_shown_again() {
        let (mut l, mut s) = (Logged::default(), Streaming::default());
        // Replayed: the turn's start, a tool's result, then the reply (seq 12).
        for e in
            [logged(10, serde_json::json!({"type": "turn_started", "turn": 3})), result(11, "c"), reply(12, "Now the catch-up preview.")]
        {
            l.note(&e);
        }
        // Then, live, the text that reply streamed (the log was at 12).
        l.stream(&mut s, text(12, "Now the catch-up "));
        l.stream(&mut s, text(12, "preview."));
        assert_eq!(s.text, "", "the reply is in the log: not drawn twice");
        // The next reply streams from 13 on: shown.
        l.stream(&mut s, text(13, "Next"));
        assert_eq!(s.text, "Next");
    }

    /// Live, in order: a reply clears what it streamed, and text for it that
    /// still comes after it is dropped, not the next reply's.
    #[test]
    fn a_reply_settles_what_streamed_before_it() {
        let (mut l, mut s) = (Logged::default(), Streaming::default());
        l.stream(&mut s, text(12, "Hello"));
        assert_eq!(s.text, "Hello");
        l.event(&mut s, &reply(12, "Hello there."));
        assert_eq!(s.text, "");
        l.stream(&mut s, text(12, " there."));
        assert_eq!(s.text, "", "late text for the reply drawn above");
        // A tool's result or a usage report in between settles nothing.
        l.event(&mut s, &result(13, "c"));
        l.stream(&mut s, text(14, "Then"));
        assert_eq!(s.text, "Then");
        l.stream(&mut s, StreamMsg::Text { turn: 0, text: ".".into(), seq: 0 });
        assert_eq!(s.text, "Then.", "a stream that says nothing (an older worker) is shown");
    }

    #[test]
    fn stop_says_how_many_background_agents_it_stops() {
        assert_eq!(stop_question(1), "This also stops 1 background agent. Stop anyway?");
        assert_eq!(stop_question(3), "This also stops 3 background agents. Stop anyway?");
    }

    fn output(call_id: &str, text: &str) -> StreamMsg {
        StreamMsg::ToolOutput { call_id: call_id.into(), text: text.into() }
    }

    /// A call's output streamed live is dropped once its result is in,
    /// replayed (catching up, the result comes before the output that was
    /// live meanwhile) or live; another call's still shows.
    #[test]
    fn output_of_a_call_whose_result_is_in_is_not_kept() {
        let (mut l, mut s) = (Logged::default(), Streaming::default());
        l.note(&result(11, "c"));
        assert!(!l.stream(&mut s, output("c", "building…")), "its result was replayed");
        assert!(l.stream(&mut s, output("d", "running")));
        l.event(&mut s, &result(12, "d"));
        assert!(!l.stream(&mut s, output("d", " more")), "its result came live");
        assert!(s.tool_output.is_empty(), "{:?}", s.tool_output);
        assert!(l.stream(&mut s, output("e", "next")));
        assert_eq!(s.tool_output.get("e").map(String::as_str), Some("next"));
    }

    fn file(name: &str, mb: usize) -> c6_api::AttachmentUpload {
        c6_api::AttachmentUpload { name: name.into(), mime: "image/png".into(), data: "A".repeat(mb * 1024 * 1024 / 3 * 4) }
    }

    #[test]
    fn a_session_message_leads_with_its_first_line() {
        assert_eq!(subject_of("Ready to merge.\nDetails here."), ("Ready to merge.".into(), Some("Details here.".into())));
        assert_eq!(subject_of("Short."), ("Short.".into(), None));
        let long = format!("{} Then more words follow here.", "A first sentence that runs on. ".repeat(6));
        let (lead, rest) = subject_of(&long);
        assert!(lead.ends_with('.') && lead.chars().count() <= 160, "{lead}");
        assert!(rest.is_some_and(|r| r.contains("more words")));
    }

    #[test]
    fn an_app_call_reads_as_what_it_does() {
        assert_eq!(summarize("app_call", &serde_json::json!({})), "list the app operations it can call");
        let add = serde_json::json!({"instance": "crm", "operation": "add", "input": {"name": "Barbara"}});
        assert_eq!(summarize("mcp__carbon6__AppCall", &add), "run crm.add (name: Barbara)");
    }

    #[test]
    fn a_file_over_10_mb_or_a_message_over_20_mb_is_refused_with_its_size() {
        assert_eq!(too_large(&[], &[file("small.png", 3)]), None);
        assert!(too_large(&[], &[file("big.png", 11)]).is_some_and(|e| e.contains("big.png") && e.contains("10 MB a file")));
        assert!(too_large(&[file("a.png", 9), file("b.png", 9)], &[file("c.png", 9)]).is_some_and(|e| e.contains("20 MB in all")));
    }

    /// A notice is waiting until a turn takes it.
    #[test]
    fn notices_wait_until_a_turn_takes_them() {
        let ev = |seq, ts, kind| Event { seq, ts, kind };
        let queued = |id: &str, from: &str| EventKind::NoticeQueued {
            command_id: id.into(),
            text: format!("about {id}\nmore"),
            command: String::new(),
            from_title: from.into(),
            deliver_at_ms: 42,
        };
        let user = |id: &str| EventKind::User {
            command_id: id.into(),
            text: String::new(),
            attachments: Vec::new(),
            command: String::new(),
            prompt: String::new(),
            blocked: String::new(),
            delegated_by: String::new(),
            wake: false,
            from_session_id: String::new(),
            from_title: String::new(),
        };
        let ts = 10_000;
        let evs = [ev(1, ts, queued("task:bg1:finished", "")), ev(2, ts + 47, user("task:bg1:finished"))];
        assert!(waiting_notices(&evs, ts + 47).is_empty());
        assert!(waiting_notices(&evs, ts + NOTICE_GRACE_MS).is_empty());

        let mut evs = evs.to_vec();
        evs.push(ev(3, ts, queued("m-2", "Alice")));
        assert_eq!(waiting_notices(&evs, ts + NOTICE_GRACE_MS + 1), [("Alice".to_owned(), "about m-2".to_owned(), 42)]);
    }

    #[test]
    fn notices_show_only_after_their_own_timestamp_grace_including_quota_holds() {
        let ts = 10_000;
        let mut ev = Event {
            seq: 1,
            ts,
            kind: EventKind::NoticeQueued {
                command_id: "m-1".into(),
                text: "\nReady\nMore details".into(),
                command: String::new(),
                from_title: "Alice".into(),
                deliver_at_ms: 0,
            },
        };
        let expected = |at| [("Alice".to_owned(), "Ready".to_owned(), at)];
        let evs = std::slice::from_ref(&ev);
        assert!(waiting_notices(evs, ts - 1).is_empty());
        assert!(waiting_notices(evs, ts + 47).is_empty());
        assert!(waiting_notices(evs, ts + NOTICE_GRACE_MS - 1).is_empty());
        assert_eq!(waiting_notices(evs, ts + NOTICE_GRACE_MS), expected(0));
        assert_eq!(waiting_notices(evs, ts + NOTICE_GRACE_MS + 60_000), expected(0));

        let delivery = ts + 3_600_000;
        if let EventKind::NoticeQueued { deliver_at_ms, .. } = &mut ev.kind {
            *deliver_at_ms = delivery;
        }
        let evs = std::slice::from_ref(&ev);
        assert!(waiting_notices(evs, ts + NOTICE_GRACE_MS - 1).is_empty());
        assert_eq!(waiting_notices(evs, ts + NOTICE_GRACE_MS + 1), expected(delivery));
    }

    /// A message to another session reads as one, in every tool profile's
    /// naming and over Claude Code's MCP bridge.
    #[test]
    fn session_message_calls_read_as_messages() {
        let input = serde_json::json!({"to": "0199-b", "message": "What is your status?\nMore."});
        for name in ["message_session", "MessageSession", "mcp__carbon6__MessageSession"] {
            assert_eq!(tool_kind(name), "message_session");
            assert_eq!(summarize(name, &input), "What is your status?");
        }
        assert_eq!(tool_kind("mcp__carbon6__ListSessions"), "list_sessions");
        assert_eq!(summarize("mcp__carbon6__TaskStop", &serde_json::json!({"task_id": "bg3"})), "bg3");
    }

    fn session(worker: &str, on_head: bool) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": "s", "worker_id": worker, "title": "t", "cwd": "", "connection_id": "c", "model": "m",
            "status": "running", "error": null, "created_at": 0, "updated_at": 0, "last_seq": 0, "on_head": on_head
        }))
        .unwrap()
    }

    fn worker(id: &str, status: &str) -> c6_api::Worker {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": "w", "hostname": "w", "os": "linux", "arch": "x86_64", "version": null,
            "status": status, "enrolled_at": 0, "last_seen": null, "connected_since": null, "rtt_us": null
        }))
        .unwrap()
    }

    const W: &str = "00000000-0000-0000-0000-000000000001";
    const HEAD: &str = "00000000-0000-0000-0000-0000000000ff";

    #[test]
    fn a_session_the_head_runs_is_never_offline() {
        // The head's own worker is not in the workers list at all.
        assert!(!worker_offline(&session(HEAD, true), &[worker(W, "online")]));
        assert!(!worker_offline(&session(HEAD, true), &[]));
    }

    #[test]
    fn a_session_whose_worker_is_away_is_offline() {
        assert!(worker_offline(&session(W, false), &[worker(W, "offline")]));
        assert!(worker_offline(&session(W, false), &[]), "not listed: not connected");
        assert!(!worker_offline(&session(W, false), &[worker(W, "online")]));
    }

    #[test]
    fn a_calls_model_sets_the_window() {
        let ev = |seq: u64, v: serde_json::Value| -> Event {
            let mut e = serde_json::json!({"seq": seq, "ts": 0});
            e.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
            serde_json::from_value(e).unwrap()
        };
        let usage = |window: u64| {
            serde_json::json!({"type": "usage", "model": "claude-opus-5-5", "input_tokens": 10, "output_tokens": 5,
                "cache_read_tokens": 300_000, "cache_write_tokens": 0, "reasoning_tokens": 0, "context_window": window})
        };
        // An older worker's usage says no window: the session's stays.
        let u = UsageStats::from_events(&[
            ev(1, serde_json::json!({"type": "turn_started", "turn": 1, "context_window": 128_000})),
            ev(2, usage(0)),
        ]);
        assert_eq!((u.context, u.context_window), (Some(300_015), 128_000));
        let u = UsageStats::from_events(&[
            ev(1, serde_json::json!({"type": "turn_started", "turn": 1, "context_window": 128_000})),
            ev(2, usage(1_000_000)),
        ]);
        assert_eq!((u.context, u.context_window), (Some(300_015), 1_000_000));
    }

    #[test]
    fn mcp_tools_read_as_words() {
        assert_eq!(tool_label("mcp__carbon6__SendMessage"), ("Send message".to_owned(), Some("carbon6".to_owned())));
        assert_eq!(tool_label("mcp__claude_ai_Slack__slack_read_channel"), ("Slack read channel".to_owned(), Some("Slack".to_owned())));
        assert_eq!(tool_label("mcp__carbon6__doc"), ("Doc".to_owned(), Some("carbon6".to_owned())));
        assert_eq!(tool_label("Bash"), ("Bash".to_owned(), None), "other tools as named");
        assert_eq!(tool_label("web_fetch"), ("web_fetch".to_owned(), None));
        assert_eq!(tool_label("c6_call"), ("carbon6".to_owned(), None), "Ask's: what it does is the summary");
    }

    #[test]
    fn asks_operations_read_as_words() {
        let call = |op: &str| summarize("c6_call", &serde_json::json!({"op": op, "input": {"body": {"prompt": "x"}}}));
        assert_eq!(call("sessions.create"), "Start a session");
        assert_eq!(call("docs.create"), "Make a document");
        assert_eq!(call("settings.routes.put"), "Settings routes put");
        assert_eq!(call("workers.enrollments.create"), "Workers enrollments create");
        assert_eq!(summarize("c6_call", &serde_json::Value::Null), "Run a carbon6 operation");
        // Its approvals: the operation, else (a session's first message) nothing to say.
        assert_eq!(summarize("c6_call", &c6_approval_op(r#"connections.delete {"id":"c1"}"#)), "Connections delete");
        assert!(c6_approval_op("Check the disk.usage on thalassa").is_null());
        assert!(c6_approval_op("README.md is out of date: fix it").is_null());
        let discover = |v: serde_json::Value| summarize("c6_discover", &v);
        assert_eq!(
            discover(serde_json::json!({"query": " enrollment token for a new worker "})),
            "Look up enrollment token for a new worker"
        );
        assert_eq!(discover(serde_json::json!({"area": "sessions"})), "Look up what sessions can do");
        assert_eq!(discover(serde_json::json!({})), "Look up what carbon6 can do");
        assert_eq!(summarize("c6_describe", &serde_json::json!({"op": "sessions.create"})), "Read how sessions.create works");
    }

    #[test]
    fn the_session_a_call_made() {
        let made =
            serde_json::json!({"id": "01a18f50-9f48-7567-8070-43e1f17ba642", "title": "Fix the login", "work": {"id": "w1", "title": "t"}});
        let whole = serde_json::to_string_pretty(&made).unwrap();
        let want = Some(("01a18f50-9f48-7567-8070-43e1f17ba642".to_owned(), "Fix the login".to_owned()));
        assert_eq!(made_session(&whole), want);
        assert_eq!(made_session(&whole[..whole.len() - 30]), want, "cut short");
        assert_eq!(made_session("no operation \"sessions.create\""), None);
        assert_eq!(made_session("{\"error\": \"x\"}"), None);
    }

    fn log(users: &[u64], n: u64) -> Vec<Event> {
        (1..=n)
            .map(|seq| {
                let kind = if users.contains(&seq) {
                    serde_json::json!({"type": "user", "command_id": "", "text": "hi"})
                } else {
                    serde_json::json!({"type": "turn_started", "turn": seq})
                };
                let mut e = serde_json::json!({"seq": seq, "ts": 0});
                e.as_object_mut().unwrap().extend(kind.as_object().unwrap().clone());
                serde_json::from_value(e).unwrap()
            })
            .collect()
    }

    #[test]
    fn context_meter_prefers_the_turn_window_and_accepts_older_events() {
        let events: Vec<Event> = [
            serde_json::json!({"seq": 1, "ts": 0, "type": "model_switched", "model": "qwen", "connection_id": "c", "effort": "", "context_window": 128_000}),
            serde_json::json!({"seq": 2, "ts": 0, "type": "turn_started", "turn": 1, "context_window": 16_384}),
            serde_json::json!({"seq": 3, "ts": 0, "type": "turn_started", "turn": 2}),
        ].into_iter().map(|v| serde_json::from_value(v).unwrap()).collect();
        assert_eq!(UsageStats::from_events(&events[..1]).context_window, 128_000);
        assert_eq!(UsageStats::from_events(&events).context_window, 16_384);
    }

    #[test]
    fn connection_without_a_default_does_not_silently_cancel_a_switch() {
        assert_eq!(picker_model("claude_code", None), "default");
        assert_eq!(picker_model("claude_code", Some("  ".into())), "default");
        assert_eq!(picker_model("claude_code", Some("sonnet".into())), "sonnet");
        assert_eq!(picker_model("anthropic", None), "");
        assert_eq!(picker_model("anthropic", Some("opus".into())), "opus");

        let owner = Owner::new();
        owner.with(|| {
            let current = ("api".to_owned(), "old-model".to_owned(), String::new());
            let choice = ModelChoice {
                connection: RwSignal::new(current.0.clone()),
                model: RwSignal::new(current.1.clone()),
                effort: RwSignal::new(current.2.clone()),
                current: Memo::new(move |_| current.clone()),
            };
            assert_eq!(choice.switch(), None);
            choice.connection.set("cc".into());
            choice.model.set(picker_model("claude_code", None));
            assert_eq!(choice.switch(), Some(("cc".into(), "default".into(), String::new())));
            // Clearing the model must stay a pending choice, so the composer
            // can report the missing model instead of dropping the switch.
            choice.model.set("  ".into());
            assert_eq!(choice.switch(), Some(("cc".into(), String::new(), String::new())));
        });
    }

    #[test]
    fn a_jump_draws_to_a_message_about_a_window_on() {
        let evs = log(&[1, 200, 400, 650, 1000], 1200);
        // From 50: at least TAIL on, then the next of your messages.
        assert_eq!(bound(&evs, 50), Some(400));
        assert_eq!(bound(&evs, 400), Some(1000));
        // None after it: to the end.
        assert_eq!(bound(&evs, 800), None);
    }

    #[test]
    fn anchors_name_their_events() {
        assert_eq!(anchor_seq("s1-e42", "s1"), Some(42));
        assert_eq!(anchor_seq("e7", "s1"), Some(7));
        assert_eq!(anchor_seq("call-abc", "s1"), None);
        assert_eq!(anchor_seq("s2-e42", "s1"), None);
    }
}
