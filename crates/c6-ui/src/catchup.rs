//! Catching up on a session: when you come back to one that did something
//! since you last looked (5 steps or more, or something waits on you), a
//! card says what happened: a paragraph from a small model, and rows
//! counted by the head (waiting on you, finished, files changed and
//! commits, commands, messages), each opening to its items, which go to
//! their place in the transcript. Asked for as the session comes into view,
//! before it's marked seen; kept until you're caught up.

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

/// You came back to a session: what it did since you looked, as a card if
/// it's worth one. A card still open is brought up to date from where it
/// started.
pub async fn look(cards: RwSignal<HashMap<String, Card>>, id: String) {
    let open = cards.with_untracked(|m| m.get(&id).filter(|c| c.open).map(|c| (c.c.since_seq, c.c.since_at)));
    let Ok(Some(mut c)) = api::catchup(id.clone(), open.map(|o| o.0), false).await else { return };
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
        let got = api::catchup(id.clone(), Some(card.c.since_seq), true).await;
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

/// You're caught up: the card and its line go, and the head hears you've
/// seen it.
pub fn caught_up(cards: RwSignal<HashMap<String, Card>>, id: String) {
    cards.update(|m| {
        m.remove(&id);
    });
    spawn_local(api::session_seen(id));
}

/// The card put away (it comes again with what's new when you come back).
pub fn put_away(cards: RwSignal<HashMap<String, Card>>, id: &str) {
    cards.update(|m| {
        if let Some(c) = m.get_mut(id) {
            c.open = false;
        }
    });
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
    let Some(cards) = cards() else { return ().into_any() };
    let id = StoredValue::new(session);
    let card = Memo::new(move |_| cards.with(|m| m.get(&id.get_value()).cloned()));
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
        {move || card.get().filter(|c| c.open).map(|card| {
            let c = card.c.clone();
            let since = since_text(c.since_at);
            let rows = c.rows.clone();
            view! {
                <div class=format!("space-y-2 text-sm {class}")>
                    {view! {
                        <div class="flex items-center gap-2">
                            // The glance's sections are in capitals; the time isn't.
                            <span class=if caps { "flex-1 text-[10px] font-medium tracking-wide text-muted" } else { "flex-1 font-medium" }>
                                {format!("{} · {since}", if caps { "SINCE YOU LOOKED" } else { "Since you looked" })}
                            </span>
                            <button class=crate::icons::BAR_BUTTON title="Put it away" aria-label="Close"
                                on:click=move |_| { put_away(cards, &id.get_value()); done() }>{crate::icons::x()}</button>
                        </div>
                    }}
                    {if !c.summary.is_empty() {
                        view! { <p class="leading-snug">{c.summary.clone()}</p> }.into_any()
                    } else if card.pending {
                        view! { <p class="animate-pulse text-xs text-muted">"Summing up…"</p> }.into_any()
                    } else {
                        ().into_any()
                    }}
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
                                        <span class="shrink-0 text-muted">{let o = is_open.clone(); move || if o() { "▾" } else { "▸" }}</span>
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
                            on:click=move |_| { caught_up(cards, id.get_value()); done() }>"Caught up ✓"</button>
                    </div>
                </div>
            }
        })}
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

/// The catch-up on a phone: a sheet from the bottom, closed by a tap
/// outside it, or as you act.
#[component]
pub fn CatchupSheet(session: String, jump: Callback<(u64, String)>, open: RwSignal<bool>) -> impl IntoView {
    let close = move || open.set(false);
    view! {
        <div class="fixed inset-0 z-40 bg-black/30" on:click=move |ev| { ev.stop_propagation(); close() }></div>
        <div class="fixed inset-x-0 bottom-0 z-50 max-h-[70dvh] overflow-y-auto rounded-t-xl border-t border-zinc-200 bg-white px-4 pb-6 pt-4 shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); close() }>
            <CatchupBody session jump on_done=move |()| close() />
        </div>
    }
}
