//! Catching up on a session: when you come back to one that did something
//! since you last looked (5 steps or more, or something waits on you), a
//! card says what happened: a paragraph from a small model, and rows
//! counted by the head (waiting on you, finished, files changed and
//! commits, commands, messages), each opening to its items, which go to
//! their place in the transcript. Asked for as the session comes into view,
//! before it's marked seen, from when this page last had it on screen;
//! kept until you're caught up.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

/// A catch-up shown for a session.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub c: c6_api::Catchup,
    /// The card is shown (× puts it away; the line in the transcript stays
    /// until you're caught up).
    pub open: bool,
    /// The paragraph was asked for, and the answer is still to come.
    pub asked: bool,
    pub pending: bool,
}

/// The catch-ups of the sessions you came back to, by session.
pub fn cards() -> Option<RwSignal<HashMap<String, Card>>> {
    use_context::<crate::workspace::Workspace>().map(|w| w.catchups)
}

const LOOKED_KEY: &str = "c6.looked";
/// Sessions whose time on screen the browser keeps, at most (the latest).
const MAX_LOOKED: usize = 500;
/// How often the times of sessions still on screen are saved, at most.
const SAVE_EVERY_MS: i64 = 30_000;

/// When this page last had each session on screen, kept in the browser too
/// (a reload, a phone's page shown again): a catch-up counts from then, so
/// what you watched in a turn that goes on isn't offered again.
#[derive(Default)]
pub struct Looked {
    at: HashMap<String, i64>,
    /// When it was last saved.
    saved: i64,
}

impl Looked {
    pub fn restore() -> Self {
        let at = crate::util::storage_get(LOOKED_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { at, saved: 0 }
    }

    pub fn get(&self, id: &str) -> Option<i64> {
        self.at.get(id).copied()
    }

    /// `ids` are on screen at `now`, or were until then; saved at once if
    /// `save` (they left it), else at most every [`SAVE_EVERY_MS`].
    pub fn note<'a>(&mut self, ids: impl IntoIterator<Item = &'a String>, now: i64, save: bool) {
        let mut any = false;
        for id in ids {
            self.at.insert(id.clone(), now);
            any = true;
        }
        if !any {
            return;
        }
        keep_latest(&mut self.at, MAX_LOOKED);
        if save || now - self.saved >= SAVE_EVERY_MS {
            self.saved = now;
            crate::util::storage_set(LOOKED_KEY, &serde_json::to_string(&self.at).unwrap_or_default());
        }
    }
}

/// The `max` latest times, the rest forgotten.
fn keep_latest(at: &mut HashMap<String, i64>, max: usize) {
    if at.len() <= max {
        return;
    }
    let mut times: Vec<i64> = at.values().copied().collect();
    times.sort_unstable_by(|a, b| b.cmp(a));
    let oldest = times[max - 1];
    at.retain(|_, t| *t >= oldest);
    // Ties at the edge: any of them.
    while at.len() > max {
        let Some(k) = at.iter().find(|(_, t)| **t == oldest).map(|(k, _)| k.clone()) else { break };
        at.remove(&k);
    }
}

/// You came back to a session (`looked_at`: when this page last had it on
/// screen): what it did since you looked, as a card if it's worth one. A
/// card still open is brought up to date from where it started.
pub async fn look(cards: RwSignal<HashMap<String, Card>>, id: String, looked_at: Option<i64>) {
    let open = cards.with_untracked(|m| m.get(&id).filter(|c| c.open).map(|c| (c.c.since_seq, c.c.since_at)));
    let Ok(Some(mut c)) = api::catchup(id.clone(), open.map(|o| o.0), looked_at, false).await else { return };
    if let Some((_, at)) = open {
        c.since_at = at;
    }
    if c.offer {
        cards.update(|m| {
            m.insert(id, Card { c, open: true, asked: false, pending: false });
        });
    } else if open.is_none() && cards.with_untracked(|m| m.contains_key(&id)) {
        // Nothing new since one you put away: its line goes too.
        cards.update(|m| {
            m.remove(&id);
        });
    }
}

/// Asks for the card's paragraph, once.
fn ask_summary(cards: RwSignal<HashMap<String, Card>>, id: String) {
    let Some(card) = cards.with_untracked(|m| m.get(&id).cloned()) else { return };
    if card.asked || !card.c.summary.is_empty() {
        return;
    }
    cards.update(|m| {
        if let Some(c) = m.get_mut(&id) {
            (c.asked, c.pending) = (true, true);
        }
    });
    spawn_local(async move {
        let got = api::catchup(id.clone(), Some(card.c.since_seq), None, true).await;
        cards.update(|m| {
            let Some(c) = m.get_mut(&id).filter(|c| c.c.since_seq == card.c.since_seq) else { return };
            c.pending = false;
            if let Ok(Some(mut got)) = got {
                got.since_at = card.c.since_at;
                c.c = got;
            }
        });
    });
}

/// You're caught up: the card and its line go, the head hears you've seen
/// it, and the next catch-up counts from now.
pub fn caught_up(ws: crate::workspace::Workspace, id: String) {
    ws.catchups.update(|m| {
        m.remove(&id);
    });
    ws.mark_looked([&id], true);
    spawn_local(api::session_seen(id));
}

/// The card put away (it comes again with what's new from now when you
/// come back).
pub fn put_away(ws: crate::workspace::Workspace, id: &str) {
    ws.catchups.update(|m| {
        if let Some(c) = m.get_mut(id) {
            c.open = false;
        }
    });
    ws.mark_looked([&id.to_owned()], true);
}

/// How long since: "42m", "3h", "2d".
pub fn since_text(at: i64) -> String {
    if at <= 0 {
        return "ever".into();
    }
    crate::when::ago(crate::util::now_ms() - at).trim_end_matches(" ago").to_owned()
}

/// The element of an item in the transcript: its tool call's card, else
/// its event.
fn place_of(session: &str, item: &c6_api::CatchupItem) -> Option<String> {
    if !item.call.is_empty() {
        Some(format!("call-{}", item.call))
    } else if item.seq > 0 {
        Some(format!("{session}-e{}", item.seq))
    } else {
        None
    }
}

/// The dot before a row, by its kind.
fn tone(kind: &str) -> &'static str {
    match kind {
        "waiting" => "bg-amber-500",
        "finished" => "bg-emerald-500",
        "changes" => "bg-sky-500",
        "commands" => "bg-zinc-400",
        _ => "bg-violet-500",
    }
}

/// What a catch-up says: its title and ×, the paragraph, the counted rows
/// (each opening to its items), Jump to it and Caught up. `jump`: to an
/// event (its seq, and the element to show); `caps`: titled as the
/// glance's sections are; `on_done`: after you act.
#[component]
pub fn CatchupBody(
    session: String,
    jump: Callback<(u64, String)>,
    #[prop(optional)] caps: bool,
    /// Around it, when there is one.
    #[prop(optional, into)]
    class: String,
    #[prop(optional, into)] on_done: Option<Callback<()>>,
) -> impl IntoView {
    let Some(ws) = use_context::<crate::workspace::Workspace>() else { return ().into_any() };
    let class = StoredValue::new(class);
    let cards = ws.catchups;
    let id = StoredValue::new(session);
    let card = Memo::new(move |_| cards.with(|m| m.get(&id.get_value()).cloned()));
    let open = Memo::new(move |_| card.with(|c| c.as_ref().is_some_and(|c| c.open)));
    let present = crate::util::presence(move || open.get());
    // Caught up removes the card from shared state immediately. Keep its
    // last visible content while the rise/sheet exits, not an empty shell.
    let retained = Memo::new(move |previous: Option<&Option<Card>>| card.get().filter(|c| c.open).or_else(|| previous.cloned().flatten()));
    let summary =
        Memo::new(move |_| retained.with(|c| c.as_ref().map(|c| c6_api::citations::clean(&c.c.summary).into_owned()).unwrap_or_default()));
    let pending = Memo::new(move |_| retained.with(|c| c.as_ref().is_some_and(|c| c.pending && c.c.summary.is_empty())));
    let pending_present = crate::util::presence(move || pending.get());
    Effect::new(move || {
        if card.with(|c| c.as_ref().is_some_and(|c| c.open && !c.asked)) {
            ask_summary(cards, id.get_value());
        }
    });
    let done = move || {
        if let Some(f) = on_done {
            f.run(());
        }
    };
    let expanded = RwSignal::new(HashSet::<String>::new());
    view! {
        <Show when=move || present.get()>
        <div class="c6-rise c6-presence" style="--rise:-8px;--leave:-6px" data-open=move || open.get().to_string() inert=move || !open.get()>
        <div class=format!("space-y-2 text-sm {}", class.get_value())>
        {move || retained.get().map(|card| {
            let since = since_text(card.c.since_at);
            view! {
                        <div class="flex items-center gap-2">
                            // The glance's sections are in capitals; the time isn't.
                            <span class=if caps { "flex-1 text-[10px] font-medium tracking-wide text-muted" } else { "flex-1 font-medium" }>
                                {format!("{} · {since}", if caps { "SINCE YOU LOOKED" } else { "Since you looked" })}
                            </span>
                            <button class=crate::icons::BAR_BUTTON title="Put it away" aria-label="Close"
                                on:click=move |_| { put_away(ws, &id.get_value()); done() }>{crate::icons::x()}</button>
                        </div>
            }
        })}
        <Show when=move || pending_present.get() || summary.with(|s| !s.is_empty())>
            // One grid slot: the plain pending label fades out over the
            // incoming paragraph, while the bounded parent resizes once.
            <div class="grid">
                <Show when=move || pending_present.get()>
                    <p class=move || if pending.get() { "col-start-1 row-start-1 text-xs text-muted" } else { "c6-crossfade-out col-start-1 row-start-1 text-xs text-muted" }
                        aria-hidden=move || (!pending.get()).to_string()>"Summing up…"</p>
                </Show>
                {move || Some(summary.get()).filter(|s| !s.is_empty()).map(|text| view! {
                    <p class="c6-crossfade col-start-1 row-start-1 leading-snug">{text}</p>
                })}
            </div>
        </Show>
        {move || retained.get().map(|card| {
            let c = card.c;
            let rows = c.rows.clone();
            view! {
                    <ul class="space-y-px">
                        {rows.into_iter().map(|r| {
                            let kind = r.kind.clone();
                            let is_open = { let kind = kind.clone(); move || expanded.with(|e| e.contains(&kind)) };
                            let items = r.items.clone();
                            let toggle = { let kind = kind.clone(); move |_| expanded.update(|e| if !e.remove(&kind) { e.insert(kind.clone()); }) };
                            let strong = if kind == "waiting" { "font-medium text-amber-800 dark:text-amber-300" } else { "" };
                            view! {
                                <li>
                                    <button class="flex w-full items-center gap-2 rounded px-1.5 py-1 text-left text-xs hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                        aria-expanded={let o = is_open.clone(); move || o().to_string()} on:click=toggle>
                                        <span class=format!("size-1.5 shrink-0 rounded-full {}", tone(&kind))></span>
                                        <span class=format!("min-w-0 flex-1 {strong}")>{r.text.clone()}</span>
                                        <span class="shrink-0 text-muted">{let o = is_open.clone(); move || if o() { crate::icons::chevron_down().into_any() } else { crate::icons::chevron_right().into_any() }}</span>
                                    </button>
                                    <Show when=is_open.clone()>
                                        <ul class="mb-1 ml-5 space-y-px">
                                            {items.clone().into_iter().map(|item| {
                                                let place = place_of(&id.get_value(), &item);
                                                let tone = if item.failed { "text-red-600 dark:text-red-400" } else { "text-soft" };
                                                let text = if item.failed { format!("{} · failed", item.text) } else { item.text.clone() };
                                                match place {
                                                    Some(el) => {
                                                        let seq = item.seq;
                                                        view! {
                                                            <li><button class=format!("w-full truncate rounded px-1 py-0.5 text-left text-xs hover:underline {tone}") title=text.clone()
                                                                on:click=move |_| { jump.run((seq, el.clone())); done() }>{text.clone()}</button></li>
                                                        }.into_any()
                                                    }
                                                    None => view! { <li class=format!("truncate px-1 py-0.5 text-xs {tone}") title=text.clone()>{text.clone()}</li> }.into_any(),
                                                }
                                            }).collect_view()}
                                        </ul>
                                    </Show>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                    <div class="flex items-center gap-2 pt-1">
                        <button class=crate::util::BUTTON_QUIET title="To where you left off in the transcript"
                            on:click=move |_| {
                                let s = id.get_value();
                                jump.run((c.since_seq, format!("here-{s}")));
                                done();
                            }>"Jump to it"</button>
                        <span class="flex-1"></span>
                        <button class=crate::util::BUTTON title="Mark it all seen and put the card away"
                            on:click=move |_| { caught_up(ws, id.get_value()); done() }>"Caught up "{crate::icons::check()}</button>
                    </div>
            }
        })}
        </div>
        </div>
        </Show>
    }
    .into_any()
}

/// The thin dashed line in the transcript where you left off.
#[component]
pub fn YouWereHere(session: String, at: i64) -> impl IntoView {
    view! {
        <div id=format!("here-{session}") class="flex items-center gap-2 text-[11px] text-muted" role="separator">
            <span class="h-px flex-1 border-t border-dashed border-zinc-300 dark:border-zinc-600"></span>
            <span>{format!("you were here · {} ago", since_text(at))}</span>
            <span class="h-px flex-1 border-t border-dashed border-zinc-300 dark:border-zinc-600"></span>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismissing_a_catchup_retains_its_summary_not_an_empty_sheet() {
        let source = include_str!("catchup.rs");
        let body = source.split("pub fn CatchupBody(").nth(1).unwrap().split("pub fn YouWereHere(").next().unwrap();
        assert!(body.contains("let present = crate::util::presence(move || open.get())"));
        assert!(body.contains("previous.cloned().flatten()"), "removing the shared card must not discard the exiting content");
        assert!(body.contains("<Show when=move || present.get()>"));
        assert!(body.contains("{move || retained.get().map(|card|"));
        assert!(body.contains("data-open=move || open.get().to_string() inert=move || !open.get()"));
        assert!(body.contains("--rise:-8px;--leave:-6px"), "catch-up exits fade upwards");
        assert!(body.contains("<Show when=move || pending_present.get()>"), "retain the outgoing pending label for its crossfade");
        assert!(body.contains("c6-crossfade col-start-1 row-start-1 leading-snug"));
        assert!(!body.contains("animate-pulse"), "pending is plain text, not a pulsing placeholder");
    }

    #[test]
    fn keeps_the_latest_times_on_screen() {
        let mut at: HashMap<String, i64> = (0..10).map(|i| (format!("s{i}"), i)).collect();
        keep_latest(&mut at, 10);
        assert_eq!(at.len(), 10);
        keep_latest(&mut at, 4);
        let mut kept: Vec<i64> = at.values().copied().collect();
        kept.sort();
        assert_eq!(kept, [6, 7, 8, 9]);
        // Ties at the edge: still no more than the most.
        let mut at: HashMap<String, i64> = (0..5).map(|i| (format!("s{i}"), 1)).collect();
        keep_latest(&mut at, 3);
        assert_eq!(at.len(), 3);
    }
}
