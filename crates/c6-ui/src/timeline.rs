//! A session's timeline: its transcript folded into hours. Each hour is a
//! row ("14:00 · 3 turns · 41 steps"), closed but for the one a turn
//! runs in; an open hour lists a line per turn (who started it, what it
//! did, its steps), and a turn's line opens its events in full. What a
//! turn did comes from a small model when its hour is opened (the head
//! keeps it); until then, the first sentence of its last reply.

use std::collections::{HashMap, HashSet};

use c6_api::{Event, EventKind, TimelineTurn};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::session::{Streaming, TurnEvents, UsageStats};

const HOUR: i64 = 3_600_000;

/// The turns that started in an hour (local time), and their steps.
#[derive(Clone, Debug, PartialEq)]
pub struct Hour {
    /// When it starts (ms).
    pub start: i64,
    pub turns: Vec<TimelineTurn>,
    pub steps: u64,
    /// A turn runs in it.
    pub running: bool,
}

/// What the timeline shows of its turns.
#[derive(Clone, Debug, PartialEq)]
pub enum Loaded {
    Reading,
    Turns(Vec<TimelineTurn>),
    /// The head says its timeline is off (a 204).
    Off,
    Failed(String),
}

/// What the timeline shows after `prev`, given the head's answer: a 204
/// isn't an empty list, and an error is shown as one (unless turns are
/// shown already: those stay until the next answer).
pub fn answered(prev: &Loaded, got: Result<Option<Vec<TimelineTurn>>, String>) -> Loaded {
    match got {
        Ok(Some(t)) => Loaded::Turns(t),
        Ok(None) => Loaded::Off,
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
                h.steps += t.steps;
                h.running |= running;
                h.turns.push(t);
            }
            _ => out.push(Hour { start, steps: t.steps, running, turns: vec![t] }),
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

/// "3 turns · 41 steps".
pub fn hour_counts(h: &Hour) -> String {
    format!("{} · {}", counted(h.turns.len(), "turn", "turns"), counted(h.steps as usize, "step", "steps"))
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

const ROW: &str = "flex w-full items-baseline gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800/70";

/// The timeline of a session's transcript: its hours, the current one
/// open.
#[component]
pub fn Timeline(
    session: String,
    events: RwSignal<Vec<Event>>,
    from: RwSignal<u64>,
    streaming: RwSignal<Streaming>,
    usage: Memo<UsageStats>,
) -> impl IntoView {
    let id = StoredValue::new(session);
    // Read again as turns start and end.
    let marks = Memo::new(move |_| {
        events.with(|evs| evs.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. } | EventKind::TurnEnded { .. })).count())
    });
    let loaded = LocalResource::new(move || {
        marks.track();
        api::timeline(id.get_value(), None)
    });
    let state = RwSignal::new(Loaded::Reading);
    Effect::new(move || {
        if let Some(got) = loaded.get() {
            let next = answered(&state.get_untracked(), got);
            state.set(next);
        }
    });
    let all = Memo::new(move |_| {
        state.with(|s| match s {
            Loaded::Turns(t) => hours(t, local_offset),
            _ => Vec::new(),
        })
    });
    // The lines made so far, by a turn's first event.
    let lines = RwSignal::new(HashMap::<u64, String>::new());
    let open = RwSignal::new(HashSet::<i64>::new());
    // The hour a turn runs in opens by itself, once.
    let opened_by_itself = StoredValue::new(HashSet::<i64>::new());
    Effect::new(move || {
        for h in all.get().into_iter().filter(|h| h.running) {
            if opened_by_itself.with_value(|o| !o.contains(&h.start)) {
                opened_by_itself.update_value(|o| {
                    o.insert(h.start);
                });
                open.update(|o| {
                    o.insert(h.start);
                });
            }
        }
    });
    // An open hour's lines, once for each of its turns that ended.
    let asked = StoredValue::new(HashSet::<(i64, usize)>::new());
    Effect::new(move || {
        let open = open.get();
        for h in all.get().into_iter().filter(|h| open.contains(&h.start)) {
            let ended = h.turns.iter().filter(|t| t.end_seq > 0).count();
            let missing =
                h.turns.iter().any(|t| t.end_seq > 0 && t.summary.is_empty() && !lines.with_untracked(|l| l.contains_key(&t.start_seq)));
            if ended == 0 || !missing || asked.with_value(|a| a.contains(&(h.start, ended))) {
                continue;
            }
            asked.update_value(|a| {
                a.insert((h.start, ended));
            });
            spawn_local(async move {
                if let Ok(Some(got)) = api::timeline(id.get_value(), Some(h.start)).await {
                    lines.update(|l| l.extend(got.into_iter().filter(|t| !t.summary.is_empty()).map(|t| (t.start_seq, t.summary))));
                }
            });
        }
    });
    let shown_turns = RwSignal::new(HashSet::<u64>::new());
    view! {
        <div class="space-y-1">
            {move || match state.get() {
                Loaded::Reading => view! { <p class="text-sm text-muted">"Reading its turns…"</p> }.into_any(),
                Loaded::Turns(t) if t.is_empty() => view! { <p class="text-sm text-muted">"No turns yet."</p> }.into_any(),
                Loaded::Turns(_) => ().into_any(),
                Loaded::Off => view! { <p class="text-sm text-muted">"Its timeline is off (switched off elsewhere?)."</p> }.into_any(),
                Loaded::Failed(e) => view! {
                    <p class="text-sm text-red-600">
                        {format!("Couldn't read its turns: {e} ")}
                        <button class="underline" on:click=move |_| loaded.refetch()>"Try again"</button>
                    </p>
                }
                .into_any(),
            }}
            <For each=move || all.get() key=|h| (h.start, h.turns.len(), h.steps, h.running) let:h>
                {
                    let start = h.start;
                    let is_open = move || open.with(|o| o.contains(&start));
                    let label = format!("{} · {}", hour_label(start), hour_counts(&h));
                    let turns_here = h.turns.clone();
                    view! {
                        <div>
                            <button class=format!("{ROW} font-medium") aria-expanded=move || is_open().to_string()
                                on:click=move |_| open.update(|o| if !o.remove(&start) { o.insert(start); })>
                                <span class="w-3 shrink-0 text-xs text-muted">{move || if is_open() { "▾" } else { "▸" }}</span>
                                <span class="tabular-nums">{label}</span>
                                {h.running.then(|| view! { <span class="size-1.5 shrink-0 self-center animate-pulse rounded-full bg-blue-500"></span> })}
                            </button>
                            <Show when=is_open>
                                <div class="ml-5 space-y-0.5 border-l border-zinc-200 pl-2 dark:border-zinc-800">
                                    {turns_here.clone().into_iter().map(|t| turn_line(t, id, events, from, streaming, usage, lines, shown_turns)).collect_view()}
                                </div>
                            </Show>
                        </div>
                    }
                }
            </For>
        </div>
    }
}

/// A turn's line: who started it, what it did, its steps; clicked, its
/// events in full below it.
#[allow(clippy::too_many_arguments)]
fn turn_line(
    t: TimelineTurn,
    id: StoredValue<String>,
    events: RwSignal<Vec<Event>>,
    from: RwSignal<u64>,
    streaming: RwSignal<Streaming>,
    usage: Memo<UsageStats>,
    lines: RwSignal<HashMap<u64, String>>,
    shown: RwSignal<HashSet<u64>>,
) -> impl IntoView {
    let start = t.start_seq;
    let who = who(&t);
    let fallback = what(&t).to_owned();
    let said = move || lines.with(|l| l.get(&start).cloned()).unwrap_or_else(|| fallback.clone());
    let steps = counted(t.steps as usize, "step", "steps");
    let running = t.end_seq == 0;
    let end = if running { u64::MAX } else { t.end_seq };
    let is_open = move || shown.with(|s| s.contains(&start));
    let at = crate::when::time(t.at);
    view! {
        <div>
            <button class=ROW aria-expanded=move || is_open().to_string() title=at
                on:click=move |_| shown.update(|s| if !s.remove(&start) { s.insert(start); })>
                <span class="min-w-0 flex-1">
                    <span class="block truncate font-medium">{who}</span>
                    <span class="block truncate text-xs text-muted">{said}</span>
                </span>
                <span class="shrink-0 text-xs tabular-nums text-muted">{if running { format!("{steps} · running") } else { steps }}</span>
            </button>
            <Show when=is_open>
                <div class="space-y-4 py-2 pl-2">
                    {move || turn_events(id.get_value(), start, end, events, from, streaming, usage)}
                </div>
            </Show>
        </div>
    }
}

/// A turn's events: from the transcript when it has them, else read.
fn turn_events(
    session: String,
    start: u64,
    end: u64,
    events: RwSignal<Vec<Event>>,
    from: RwSignal<u64>,
    streaming: RwSignal<Streaming>,
    usage: Memo<UsageStats>,
) -> AnyView {
    let loaded = from.get_untracked() <= start && events.with_untracked(|evs| evs.iter().any(|e| e.seq == start));
    if loaded {
        let these =
            Signal::derive(move || events.with(|evs| evs.iter().filter(|e| e.seq >= start && e.seq <= end).cloned().collect::<Vec<_>>()));
        return view! { <TurnEvents session events=these streaming usage /> }.into_any();
    }
    let read = RwSignal::new(None::<Result<Vec<Event>, String>>);
    let id = session.clone();
    spawn_local(async move {
        read.set(Some(
            api::session_events_after(id, start.saturating_sub(1)).await.map(|evs| evs.into_iter().filter(|e| e.seq <= end).collect()),
        ));
    });
    view! {
        {move || match read.get() {
            None => view! { <p class="text-xs text-muted">"Loading…"</p> }.into_any(),
            Some(Err(e)) => view! { <p class="text-xs text-red-600">{e}</p> }.into_any(),
            Some(Ok(evs)) => {
                let these = Signal::derive(move || evs.clone());
                view! { <TurnEvents session=session.clone() events=these streaming usage /> }.into_any()
            }
        }}
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(turn: u64, at: i64, steps: u64, end: u64) -> TimelineTurn {
        TimelineTurn { turn, start_seq: turn * 10, end_seq: end, at, steps, ..Default::default() }
    }

    #[test]
    fn turns_fold_into_the_clock_s_hours() {
        let h = |n: i64| n * HOUR;
        let turns =
            vec![turn(2, h(9) + 50 * 60_000, 4, 29), turn(1, h(9) + 5 * 60_000, 3, 19), turn(3, h(10) + 1, 2, 39), turn(4, h(12), 1, 0)];
        let got = hours(&turns, |_| 0);
        let summary: Vec<(i64, Vec<u64>, u64, bool)> =
            got.iter().map(|h| (h.start, h.turns.iter().map(|t| t.turn).collect(), h.steps, h.running)).collect();
        assert_eq!(summary, [(h(9), vec![1, 2], 7, false), (h(10), vec![3], 2, false), (h(12), vec![4], 1, true)]);
        assert_eq!(hour_counts(&got[0]), "2 turns · 7 steps");
        assert_eq!(hour_counts(&got[1]), "1 turn · 2 steps");
        // Half an hour ahead of UTC: 9:35 UTC is 10:05 local, in the hour
        // that started at 9:30 UTC.
        let got = hours(&[turn(1, h(9) + 35 * 60_000, 1, 9)], |_| 30 * 60_000);
        assert_eq!(got[0].start, h(9) + 30 * 60_000);
        assert!(hours(&[], |_| 0).is_empty());
    }

    #[test]
    fn an_answer_that_isn_t_turns_isn_t_shown_as_none() {
        let turns = Loaded::Turns(vec![turn(1, 0, 3, 19)]);
        assert_eq!(answered(&Loaded::Reading, Ok(Some(vec![]))), Loaded::Turns(vec![]));
        // A 204: its timeline is off on the head, not empty.
        assert_eq!(answered(&Loaded::Reading, Ok(None)), Loaded::Off);
        assert_eq!(answered(&turns, Ok(None)), Loaded::Off);
        // An error is shown, unless turns are shown already.
        assert_eq!(answered(&Loaded::Reading, Err("500: boom".into())), Loaded::Failed("500: boom".into()));
        assert_eq!(answered(&Loaded::Off, Err("500: boom".into())), Loaded::Failed("500: boom".into()));
        assert_eq!(answered(&turns, Err("500: boom".into())), turns);
        assert_eq!(answered(&Loaded::Failed("x".into()), Ok(Some(vec![turn(2, 0, 1, 9)]))), Loaded::Turns(vec![turn(2, 0, 1, 9)]));
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
}
