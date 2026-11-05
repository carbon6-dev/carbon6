//! A session's turns, two ways. An outline by hour in its glance, always
//! there, newest first: each hour a row ("14:00 · 3 turns"), closed but
//! for the latest; an open hour lists a line per turn, newest first (who
//! started it, what it did, its steps), and a line goes to its turn in the
//! transcript. And, with "Fold finished turns" on (⋯), the transcript
//! itself shows each finished turn as one row, opened in place by a click;
//! the running turn stays open, as do the latest finished one (until a
//! newer one starts) and one you opened or went to. What a turn did comes
//! from a small model when its hour is open in the outline on screen (the
//! head keeps it); until then, the first sentence of its last reply.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use c6_api::{Event, EventKind, TimelineTurn};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

const HOUR: i64 = 3_600_000;

/// The turns that started in an hour (local time).
#[derive(Clone, Debug, PartialEq)]
pub struct Hour {
    /// When it starts (ms).
    pub start: i64,
    pub turns: Vec<TimelineTurn>,
    /// A turn runs in it.
    pub running: bool,
}

/// What the head said of a session's turns.
#[derive(Clone, Debug, PartialEq)]
pub enum Loaded {
    Reading,
    Turns(Vec<TimelineTurn>),
    Failed(String),
}

/// What is shown after `prev`, given the head's answer: an error is shown
/// as one (unless turns are shown already: those stay until the next
/// answer).
pub fn answered(prev: &Loaded, got: Result<Vec<TimelineTurn>, String>) -> Loaded {
    match got {
        Ok(t) => Loaded::Turns(t),
        Err(_) if matches!(prev, Loaded::Turns(_)) => prev.clone(),
        Err(e) => Loaded::Failed(e),
    }
}

/// The turns by the hour they started in, oldest first. `offset`: how far
/// local time is ahead of UTC at a time (ms), so hours are the clock's
/// (a zone half an hour off UTC too).
pub fn hours(turns: &[TimelineTurn], offset: impl Fn(i64) -> i64) -> Vec<Hour> {
    let mut sorted = turns.to_vec();
    sorted.sort_by_key(|t| (t.at, t.start_seq));
    let mut out: Vec<Hour> = Vec::new();
    for t in sorted {
        let start = t.at - (t.at + offset(t.at)).rem_euclid(HOUR);
        let running = t.end_seq == 0;
        match out.last_mut() {
            Some(h) if h.start == start => {
                h.running |= running;
                h.turns.push(t);
            }
            _ => out.push(Hour { start, running, turns: vec![t] }),
        }
    }
    out
}

/// Who started a turn: "You: fix the pager", "From Release prep", "Task
/// done: npm test", "Comments on Q3 plan".
pub fn who(t: &TimelineTurn) -> String {
    let or = |s: &str, other: &str| if s.trim().is_empty() { other.to_owned() } else { s.to_owned() };
    match t.by.as_str() {
        "you" => format!("You: {}", t.words),
        "session" => format!("From {}", or(&t.from, "another session")),
        "task" => {
            if t.words.is_empty() {
                "Task done".to_owned()
            } else {
                format!("Task done: {}", t.words)
            }
        }
        "doc" => format!("Comments on {}", or(&t.from, "a document")),
        "agent" => "From the agent that started it".to_owned(),
        _ => format!("Turn {}", t.turn),
    }
}

/// What a turn did: the model's line, else its last reply's first
/// sentence.
pub fn what(t: &TimelineTurn) -> &str {
    if t.summary.is_empty() { &t.reply } else { &t.summary }
}

fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "3 turns".
pub fn hour_counts(h: &Hour) -> String {
    counted(h.turns.len(), "turn", "turns")
}

/// A turn among a transcript's events, as the head counts them
/// (c6-head's timeline.rs): from the message that came before it (if one)
/// to its end; one that never said it ended (its worker went away) ends
/// where the next starts.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub turn: u64,
    /// Its first event.
    pub start: u64,
    /// Its last event; None while it runs (or the events end before it
    /// does).
    pub end: Option<u64>,
    /// When it started.
    pub at: i64,
    /// Its steps: each tool call, and each reply that called none.
    pub steps: u64,
}

/// The turns among `evs` (in order), each with where its events are.
/// Events before the first turn's start (the window opened in the middle
/// of a turn), or between one's end and the next's start, are in none.
pub fn spans(evs: &[Event]) -> Vec<(Span, Range<usize>)> {
    let mut out = Vec::new();
    // The message the next turn takes (written before it starts).
    let mut next: Option<usize> = None;
    let mut open: Option<(Span, usize)> = None;
    for (i, e) in evs.iter().enumerate() {
        match &e.kind {
            EventKind::User { command, .. } if command != "card-answer" && open.is_none() && next.is_none() => next = Some(i),
            EventKind::TurnStarted { turn, .. } => {
                if let Some((mut s, from)) = open.take() {
                    s.end = Some(e.seq.saturating_sub(1));
                    out.push((s, from..i));
                }
                let from = next.take().unwrap_or(i);
                open = Some((Span { turn: *turn, start: evs[from].seq, end: None, at: e.ts, steps: 0 }, from));
            }
            EventKind::Assistant { .. } => {
                if let Some((s, _)) = open.as_mut() {
                    s.steps += e.kind.steps();
                }
            }
            EventKind::TurnEnded { .. } => {
                if let Some((mut s, from)) = open.take() {
                    s.end = Some(e.seq);
                    out.push((s, from..i + 1));
                }
            }
            _ => {}
        }
    }
    if let Some((s, from)) = open {
        out.push((s, from..evs.len()));
    }
    out
}

/// What a transcript draws: an event, or a finished turn's row (its events
/// after it, when it's open).
#[derive(Clone, Debug, PartialEq)]
pub enum Piece {
    Event(Event),
    Turn(Span),
}

impl Piece {
    /// Its key among the transcript's pieces: an event's seq, or a row's
    /// turn's first event.
    pub fn key(&self) -> (u64, bool) {
        match self {
            Piece::Event(e) => (e.seq, false),
            Piece::Turn(s) => (s.start, true),
        }
    }
}

/// `evs` with each finished turn folded into its row. `open` says of a
/// finished turn: folded (`Some(false)`), its row and its events
/// (`Some(true)`), or as it is, with no row (None: one that was rewound).
/// A running turn is always as it is, and so is the latest turn while no
/// newer one has started (`to_end`: `evs` reach the log's end, so their
/// last turn is the latest), so the reply you just watched stays. Only the
/// events shown are copied.
pub fn fold(evs: &[Event], to_end: bool, open: impl Fn(&Span) -> Option<bool>) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut at = 0;
    let all = spans(evs);
    let last = all.len().checked_sub(1);
    for (i, (s, r)) in all.into_iter().enumerate() {
        out.extend(evs[at..r.start].iter().cloned().map(Piece::Event));
        let latest = to_end && Some(i) == last;
        let shown = if s.end.is_none() || latest { None } else { open(&s) };
        let events = shown != Some(false);
        if shown.is_some() {
            out.push(Piece::Turn(s));
        }
        if events {
            out.extend(evs[r.clone()].iter().cloned().map(Piece::Event));
        }
        at = r.end;
    }
    out.extend(evs[at..].iter().cloned().map(Piece::Event));
    out
}

/// How far local time is ahead of UTC at `at` (ms).
fn local_offset(at: i64) -> i64 {
    -(js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at as f64)).get_timezone_offset() as i64) * 60_000
}

/// "14:00", with the day when it isn't today ("Yesterday · 14:00").
fn hour_label(start: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(start as f64));
    let hh = format!("{:02}:00", d.get_hours());
    let day = crate::when::compact(start);
    let today = crate::when::heading(start) == "Today";
    if today { hh } else { format!("{day} · {hh}") }
}

/// A session's turns as the head has them, for its glance's outline and
/// its folded turns' rows, and which finished turns its transcript shows
/// open. One per session pane.
#[derive(Clone, Copy)]
pub struct Timeline {
    session: StoredValue<String>,
    pub state: RwSignal<Loaded>,
    loaded: LocalResource<Option<Result<Vec<TimelineTurn>, String>>>,
    /// The lines made so far, by a turn's first event.
    lines: RwSignal<HashMap<u64, String>>,
    /// The hours whose lines were asked for, with how many of their turns
    /// had ended.
    asked: StoredValue<HashSet<(i64, usize)>>,
    /// Outlines on screen: the turns are read only while one is, or while
    /// the transcript folds.
    watching: RwSignal<u32>,
    /// The transcript shows each finished turn as one row ("Fold finished
    /// turns").
    pub fold: Signal<bool>,
    /// Events that keep their finished turn open while it folds: the first
    /// of one you opened, a place you went to.
    pins: RwSignal<BTreeSet<u64>>,
}

impl Timeline {
    pub fn new(session: String, events: RwSignal<Vec<Event>>, fold: Signal<bool>) -> Self {
        let session = StoredValue::new(session);
        let watching = RwSignal::new(0u32);
        let want = Memo::new(move |_| fold.get() || watching.get() > 0);
        // Read again as turns start and end (looked for from the end).
        let mark = Memo::new(move |_| {
            events.with(|evs| {
                evs.iter().rev().find(|e| matches!(e.kind, EventKind::TurnStarted { .. } | EventKind::TurnEnded { .. })).map(|e| e.seq)
            })
        });
        let loaded = LocalResource::new(move || {
            let (want, _) = (want.get(), mark.get());
            async move { if want { Some(api::timeline(session.get_value(), None).await) } else { None } }
        });
        let state = RwSignal::new(Loaded::Reading);
        Effect::new(move || {
            if let Some(Some(got)) = loaded.get() {
                let next = answered(&state.get_untracked(), got);
                state.set(next);
            }
        });
        Timeline {
            session,
            state,
            loaded,
            lines: RwSignal::new(HashMap::new()),
            asked: StoredValue::new(HashSet::new()),
            watching,
            fold,
            pins: RwSignal::new(BTreeSet::new()),
        }
    }

    /// Asks for an hour's lines, once for each of its turns that ended.
    fn ask(&self, h: &Hour) {
        let ended = h.turns.iter().filter(|t| t.end_seq > 0).count();
        let lines = self.lines;
        let missing =
            h.turns.iter().any(|t| t.end_seq > 0 && t.summary.is_empty() && !lines.with_untracked(|l| l.contains_key(&t.start_seq)));
        if ended == 0 || !missing || self.asked.with_value(|a| a.contains(&(h.start, ended))) {
            return;
        }
        self.asked.update_value(|a| {
            a.insert((h.start, ended));
        });
        let (id, start) = (self.session.get_value(), h.start);
        spawn_local(async move {
            if let Ok(got) = api::timeline(id, Some(start)).await {
                let _ = lines.try_update(|l| l.extend(got.into_iter().filter(|t| !t.summary.is_empty()).map(|t| (t.start_seq, t.summary))));
            }
        });
    }

    /// The head's word on a turn of the transcript (tracked).
    fn turn(&self, s: &Span) -> Option<TimelineTurn> {
        self.state.with(|st| match st {
            Loaded::Turns(t) => t.iter().find(|t| t.start_seq == s.start || (t.turn == s.turn && t.at == s.at)).cloned(),
            _ => None,
        })
    }

    /// Keeps the finished turn `seq` is in open while it folds.
    pub fn pin(&self, seq: u64) {
        if !self.pins.with_untracked(|p| p.contains(&seq)) {
            self.pins.update(|p| {
                p.insert(seq);
            });
        }
    }

    /// Whether the finished turn from `start` to `end` shows its events
    /// (tracked).
    pub fn is_open(&self, start: u64, end: u64) -> bool {
        self.pins.with(|p| p.range(start..=end).next().is_some())
    }

    /// Opens the turn, or folds it again.
    fn toggle(&self, start: u64, end: u64) {
        self.pins.update(|p| {
            let inside: Vec<u64> = p.range(start..=end).copied().collect();
            if inside.is_empty() {
                p.insert(start);
            }
            for seq in inside {
                p.remove(&seq);
            }
        });
    }

    /// `evs` as the transcript draws them while finished turns fold: each
    /// one row, its events too while it's open, but one that was rewound
    /// (`rewound`, of its first event) as it is, and the latest while no
    /// newer one started (`to_end`: see [`fold`]) (tracked).
    pub fn pieces(&self, evs: &[Event], to_end: bool, rewound: impl Fn(u64) -> bool) -> Vec<Piece> {
        self.pins
            .with(|p| fold(evs, to_end, |s| (!rewound(s.start)).then(|| p.range(s.start..=s.end.unwrap_or(u64::MAX)).next().is_some())))
    }
}

const ROW: &str = "flex w-full items-baseline gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800/70";
const LINE: &str =
    "flex w-full min-w-0 items-baseline gap-2 rounded px-1.5 py-1 text-left text-xs leading-snug hover:bg-zinc-100 dark:hover:bg-zinc-800";

/// A finished turn's row in the transcript, while its turns fold: ▸, who
/// started it, what it did, when and its steps; a click opens it in place,
/// and another folds it.
#[component]
pub fn TurnRow(session: String, span: Span, timeline: Timeline) -> impl IntoView {
    let tl = timeline;
    let (start, end, turn) = (span.start, span.end.unwrap_or(u64::MAX), span.turn);
    let found = {
        let span = span.clone();
        Memo::new(move |_| tl.turn(&span))
    };
    let by = move || found.with(|t| t.as_ref().map(who)).unwrap_or_else(|| format!("Turn {turn}"));
    let said = move || {
        tl.lines.with(|l| l.get(&start).cloned()).or_else(|| found.with(|t| t.as_ref().map(|t| what(t).to_owned()))).unwrap_or_default()
    };
    let open = move || tl.is_open(start, end);
    let meta = format!("{} · {}", crate::when::time(span.at), counted(span.steps as usize, "step", "steps"));
    view! {
        <div id=format!("{session}-t{start}") class="c6-fold">
            <button class=ROW aria-expanded=move || open().to_string() title=crate::when::full(span.at)
                on:click=move |_| tl.toggle(start, end)>
                <span class="w-3 shrink-0 text-xs text-muted">{move || if open() { crate::icons::chevron_down().into_any() } else { crate::icons::chevron_right().into_any() }}</span>
                <span class="min-w-0 flex-1">
                    <span class="block truncate font-medium">{by}</span>
                    {move || Some(said()).filter(|s| !s.is_empty()).map(|s| view! { <span class="block truncate text-xs text-soft">{s}</span> })}
                </span>
                <span class="shrink-0 text-xs tabular-nums text-muted">{meta}</span>
            </button>
        </div>
    }
}

/// Hours, and the turns in each, newest first (`hours` gives them oldest
/// first): the latest activity at the top.
pub fn newest_first(mut hours: Vec<Hour>) -> Vec<Hour> {
    hours.reverse();
    for h in &mut hours {
        h.turns.reverse();
    }
    hours
}

/// Keeps `seen` saying whether `el` is on screen (not in a hidden tile, nor
/// scrolled out of its glance).
fn watch_sight(el: NodeRef<leptos::html::Div>, seen: RwSignal<bool>) {
    Effect::new(move || {
        let Some(el) = el.get() else { return };
        let cb = wasm_bindgen::closure::Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            if let Some(e) = entries.iter().last() {
                let _ = seen.try_set(wasm_bindgen::JsCast::unchecked_into::<web_sys::IntersectionObserverEntry>(e).is_intersecting());
            }
        });
        if let Ok(o) = web_sys::IntersectionObserver::new(wasm_bindgen::JsCast::unchecked_ref(cb.as_ref())) {
            o.observe(&el);
            let kept = send_wrapper::SendWrapper::new((o, cb));
            on_cleanup(move || kept.0.disconnect());
        }
    });
}

/// The glance's outline of a session's turns: its hours, newest first, the
/// latest open, each open hour's turns a line each, newest first; `on_go`:
/// to a turn in the transcript. An open hour's lines are asked for only
/// while the outline is on screen.
/// Off until it is redesigned: no outline in the glance, no folded turns,
/// and so no turns read and no lines asked for.
pub const SHOWN: bool = false;

#[component]
pub fn Outline(timeline: Timeline, #[prop(into)] on_go: Callback<TimelineTurn>) -> impl IntoView {
    let tl = timeline;
    tl.watching.update(|n| *n += 1);
    on_cleanup(move || {
        let _ = tl.watching.try_update(|n| *n = n.saturating_sub(1));
    });
    let all = Memo::new(move |_| {
        tl.state.with(|s| match s {
            Loaded::Turns(t) => newest_first(hours(t, local_offset)),
            _ => Vec::new(),
        })
    });
    let root = NodeRef::<leptos::html::Div>::new();
    let seen = RwSignal::new(false);
    watch_sight(root, seen);
    let open = RwSignal::new(HashSet::<i64>::new());
    // The latest hour opens by itself, once (and a new one as it starts).
    let opened_by_itself = StoredValue::new(HashSet::<i64>::new());
    Effect::new(move || {
        let Some(latest) = all.with(|a| a.first().map(|h| h.start)) else { return };
        if opened_by_itself.with_value(|o| !o.contains(&latest)) {
            opened_by_itself.update_value(|o| {
                o.insert(latest);
            });
            open.update(|o| {
                o.insert(latest);
            });
        }
    });
    // An open hour's lines, made as it opens (while it can be seen).
    Effect::new(move || {
        if !seen.get() {
            return;
        }
        let open = open.get();
        all.with(|a| {
            for h in a.iter().filter(|h| open.contains(&h.start)) {
                tl.ask(h);
            }
        });
    });
    view! {
        <div node_ref=root>
            {move || match tl.state.get() {
                Loaded::Reading => view! { <p class="px-1.5 py-1 text-xs text-muted">"Reading its turns…"</p> }.into_any(),
                Loaded::Turns(t) if t.is_empty() => view! { <p class="px-1.5 py-1 text-xs text-muted">"No turns yet."</p> }.into_any(),
                Loaded::Turns(_) => ().into_any(),
                Loaded::Failed(e) => view! {
                    <p class="px-1.5 py-1 text-xs text-red-600">
                        {format!("Couldn't read its turns: {e} ")}
                        <button class="underline" on:click=move |_| tl.loaded.refetch()>"Try again"</button>
                    </p>
                }
                .into_any(),
            }}
            <For each=move || all.get() key=|h| (h.start, h.turns.len(), h.turns.iter().map(|t| t.steps).sum::<u64>(), h.running) let:h>
                {
                    let start = h.start;
                    let is_open = move || open.with(|o| o.contains(&start));
                    let label = format!("{} · {}", hour_label(start), hour_counts(&h));
                    let turns_here = h.turns.clone();
                    view! {
                        <div>
                            <button class=format!("{LINE} font-medium") aria-expanded=move || is_open().to_string()
                                on:click=move |_| open.update(|o| if !o.remove(&start) { o.insert(start); })>
                                <span class="w-3 shrink-0 text-muted">{move || if is_open() { crate::icons::chevron_down().into_any() } else { crate::icons::chevron_right().into_any() }}</span>
                                <span class="tabular-nums">{label}</span>
                                {h.running.then(|| view! { <span class="size-1.5 shrink-0 self-center animate-pulse rounded-full bg-blue-500"></span> })}
                            </button>
                            <Show when=is_open>
                                <div class="mb-1 ml-3 border-l border-zinc-200 pl-1 dark:border-zinc-800">
                                    {turns_here.clone().into_iter().map(|t| outline_turn(t, tl, on_go)).collect_view()}
                                </div>
                            </Show>
                        </div>
                    }
                }
            </For>
        </div>
    }
}

/// A turn's line in the outline: who started it, what it did, its steps.
fn outline_turn(t: TimelineTurn, tl: Timeline, on_go: Callback<TimelineTurn>) -> impl IntoView {
    let start = t.start_seq;
    let by = who(&t);
    let fallback = what(&t).to_owned();
    let said = move || tl.lines.with(|l| l.get(&start).cloned()).unwrap_or_else(|| fallback.clone());
    let steps = counted(t.steps as usize, "step", "steps");
    let running = t.end_seq == 0;
    let title = format!("{} · go to it in the transcript", crate::when::full(t.at));
    view! {
        <button class=LINE title=title on:click=move |_| on_go.run(t.clone())>
            <span class="min-w-0 flex-1">
                <span class="block truncate font-medium">{by}</span>
                <span class="block truncate text-muted">{said}</span>
            </span>
            <span class="shrink-0 tabular-nums text-muted">{if running { format!("{steps} · running") } else { steps }}</span>
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c6_api::{ContentBlock, TurnEndReason};

    fn turn(turn: u64, at: i64, steps: u64, end: u64) -> TimelineTurn {
        TimelineTurn { turn, start_seq: turn * 10, end_seq: end, at, steps, ..Default::default() }
    }

    #[test]
    fn turns_fold_into_the_clock_s_hours() {
        let h = |n: i64| n * HOUR;
        let turns =
            vec![turn(2, h(9) + 50 * 60_000, 4, 29), turn(1, h(9) + 5 * 60_000, 3, 19), turn(3, h(10) + 1, 2, 39), turn(4, h(12), 1, 0)];
        let got = hours(&turns, |_| 0);
        let summary: Vec<(i64, Vec<u64>, bool)> =
            got.iter().map(|h| (h.start, h.turns.iter().map(|t| t.turn).collect(), h.running)).collect();
        assert_eq!(summary, [(h(9), vec![1, 2], false), (h(10), vec![3], false), (h(12), vec![4], true)]);
        assert_eq!(hour_counts(&got[0]), "2 turns");
        assert_eq!(hour_counts(&got[1]), "1 turn");
        // The outline shows them newest first, and so the turns in an hour.
        let shown: Vec<(i64, Vec<u64>)> = newest_first(got).iter().map(|h| (h.start, h.turns.iter().map(|t| t.turn).collect())).collect();
        assert_eq!(shown, [(h(12), vec![4]), (h(10), vec![3]), (h(9), vec![2, 1])]);
        // Half an hour ahead of UTC: 9:35 UTC is 10:05 local, in the hour
        // that started at 9:30 UTC.
        let got = hours(&[turn(1, h(9) + 35 * 60_000, 1, 9)], |_| 30 * 60_000);
        assert_eq!(got[0].start, h(9) + 30 * 60_000);
        assert!(hours(&[], |_| 0).is_empty());
    }

    #[test]
    fn an_error_isn_t_shown_as_no_turns() {
        let turns = Loaded::Turns(vec![turn(1, 0, 3, 19)]);
        assert_eq!(answered(&Loaded::Reading, Ok(vec![])), Loaded::Turns(vec![]));
        // An error is shown, unless turns are shown already.
        assert_eq!(answered(&Loaded::Reading, Err("500: boom".into())), Loaded::Failed("500: boom".into()));
        assert_eq!(answered(&turns, Err("500: boom".into())), turns);
        assert_eq!(answered(&Loaded::Failed("x".into()), Ok(vec![turn(2, 0, 1, 9)])), Loaded::Turns(vec![turn(2, 0, 1, 9)]));
    }

    #[test]
    fn a_turn_says_who_started_it() {
        let by = |by: &str, from: &str, words: &str| TimelineTurn {
            turn: 7,
            by: by.into(),
            from: from.into(),
            words: words.into(),
            ..Default::default()
        };
        assert_eq!(who(&by("you", "", "fix the pager")), "You: fix the pager");
        assert_eq!(who(&by("session", "Release prep", "is it in?")), "From Release prep");
        assert_eq!(who(&by("session", "", "")), "From another session");
        assert_eq!(who(&by("task", "", "npm test")), "Task done: npm test");
        assert_eq!(who(&by("task", "", "")), "Task done");
        assert_eq!(who(&by("doc", "Q3 plan", "")), "Comments on Q3 plan");
        assert_eq!(who(&by("agent", "", "go")), "From the agent that started it");
        assert_eq!(who(&by("", "", "")), "Turn 7");
        let mut t = by("you", "", "x");
        t.reply = "Fixed it.".into();
        assert_eq!(what(&t), "Fixed it.");
        t.summary = "Fixed the pager".into();
        assert_eq!(what(&t), "Fixed the pager");
    }

    fn ev(seq: u64, kind: EventKind) -> Event {
        Event { seq, ts: seq as i64 * 1000, kind }
    }

    fn user(seq: u64, text: &str) -> Event {
        serde_json::from_value(serde_json::json!({"seq": seq, "ts": seq * 1000, "type": "user", "command_id": "c", "text": text})).unwrap()
    }

    fn started(seq: u64, turn: u64) -> Event {
        ev(seq, EventKind::TurnStarted { turn, context_window: 0 })
    }

    fn ended(seq: u64, turn: u64) -> Event {
        ev(seq, EventKind::TurnEnded { turn, reason: TurnEndReason::Completed })
    }

    fn reply(seq: u64, turn: u64, calls: usize) -> Event {
        let mut blocks: Vec<ContentBlock> =
            (0..calls).map(|i| ContentBlock::ToolUse { id: format!("c{seq}{i}"), name: "Bash".into(), input_json: "{}".into() }).collect();
        if calls == 0 {
            blocks.push(ContentBlock::Text { text: "Done.".into(), citations: vec![] });
        }
        ev(seq, EventKind::Assistant { turn, blocks, stop_reason: String::new() })
    }

    fn other(seq: u64) -> Event {
        ev(seq, EventKind::Checkpoint { error: String::new() })
    }

    /// A session's log: your message and its turn; a checkpoint; another
    /// message and turn; a turn nothing said started, which never said it
    /// ended; and the turn that runs now, a message joining it.
    fn log() -> Vec<Event> {
        vec![
            other(1),
            user(2, "fix the pager"),
            started(3, 1),
            reply(4, 1, 2),
            reply(5, 1, 0),
            ended(6, 1),
            other(7),
            user(8, "and the tests"),
            started(9, 2),
            reply(10, 2, 0),
            ended(11, 2),
            started(12, 3),
            reply(13, 3, 1),
            started(14, 4),
            user(15, "also this"),
            reply(16, 4, 0),
        ]
    }

    fn keys(p: &[Piece]) -> Vec<(u64, bool)> {
        p.iter().map(Piece::key).collect()
    }

    #[test]
    fn events_belong_to_their_turns() {
        let evs = log();
        type Got = (u64, u64, Option<u64>, u64, Range<usize>);
        let got: Vec<Got> = spans(&evs).into_iter().map(|(s, r)| (s.turn, s.start, s.end, s.steps, r)).collect();
        assert_eq!(
            got,
            [
                // From your message to its end.
                (1, 2, Some(6), 3, 1..6),
                (2, 8, Some(11), 1, 7..11),
                // Nothing said: from its start; it ends where the next starts.
                (3, 12, Some(13), 1, 11..13),
                // Running: a message sent meanwhile is in it.
                (4, 14, None, 1, 13..16),
            ]
        );
        assert_eq!(spans(&evs)[0].0.at, 3000, "when it started");
        // A window that opens in the middle of a turn: what's before the
        // next one's message is in none.
        let got: Vec<(u64, u64)> = spans(&evs[3..]).into_iter().map(|(s, _)| (s.turn, s.start)).collect();
        assert_eq!(got, [(2, 8), (3, 12), (4, 14)]);
    }

    #[test]
    fn card_arrivals_stay_visible_between_turns_without_starting_one() {
        let mut card = user(1, "Not now: task card #185 \"Cleanup\".");
        if let EventKind::User { command, .. } = &mut card.kind {
            *command = "card-answer".into();
        }
        let evs = [card, user(2, "continue"), started(3, 1), ended(4, 1)];
        assert!(spans(&evs[..1]).is_empty(), "no turn for the card answer");
        assert_eq!(spans(&evs)[0].0.start, 2, "the actual input starts the turn");
        assert_eq!(
            keys(&fold(&evs, false, |_| Some(false))),
            [(1, false), (2, true)],
            "folding preserves the arrival as its own visible line"
        );
    }

    #[test]
    fn finished_turns_fold_and_the_running_one_stays_open() {
        let evs = log();
        // All folded: a row each, the events between them, the running turn
        // in full.
        let got = fold(&evs, true, |_| Some(false));
        assert_eq!(keys(&got), [(1, false), (2, true), (7, false), (8, true), (12, true), (14, false), (15, false), (16, false)]);
        assert!(matches!(&got[1], Piece::Turn(s) if s.end == Some(6) && s.steps == 3));
        // One opened: its row, then its events.
        let got = fold(&evs, true, |s| Some(s.start == 8));
        assert_eq!(
            keys(&got),
            [
                (1, false),
                (2, true),
                (7, false),
                (8, true),
                (8, false),
                (9, false),
                (10, false),
                (11, false),
                (12, true),
                (14, false),
                (15, false),
                (16, false)
            ]
        );
        // One rewound: as it is, with no row.
        let got = fold(&evs, true, |s| (s.start != 2).then_some(false));
        assert_eq!(keys(&got)[..7], [(1, false), (2, false), (3, false), (4, false), (5, false), (6, false), (7, false)]);
        // Its end not yet loaded (a window drawn part way): open.
        let got = fold(&evs[..4], true, |_| Some(false));
        assert_eq!(keys(&got), [(1, false), (2, false), (3, false), (4, false)]);
    }

    #[test]
    fn the_latest_finished_turn_stays_open_until_a_newer_one_starts() {
        let evs = log();
        // Turn 2 just ended, nothing newer: its reply stays; turn 1 folds.
        let got = fold(&evs[..11], true, |_| Some(false));
        assert_eq!(keys(&got), [(1, false), (2, true), (7, false), (8, false), (9, false), (10, false), (11, false)]);
        // Turn 3 started: turn 2 folds, turn 3 runs.
        let got = fold(&evs[..12], true, |_| Some(false));
        assert_eq!(keys(&got), [(1, false), (2, true), (7, false), (8, true), (12, false)]);
        // Drawn only part way (a jump back): newer turns follow, so the
        // last one drawn folds.
        let got = fold(&evs[..11], false, |_| Some(false));
        assert_eq!(keys(&got), [(1, false), (2, true), (7, false), (8, true)]);
    }
}
