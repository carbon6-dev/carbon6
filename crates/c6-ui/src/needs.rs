//! What needs you: a pill in the header with how many things wait on you
//! (amber), else how many are worth a look (grey), that opens the list of
//! them (`GET /api/v1/attention`): a drop-down on wide screens, a sheet
//! from the bottom on a phone. It loads again when the live feed says it
//! may have changed, never on a timer.

use c6_api::{Attention, AttentionElsewhere, AttentionItem};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::place::{self, Place};
use crate::util::now_ms;
use crate::workspace::{How, View, use_workspace};

/// How long a ping from the live feed waits for more before it loads.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// The pill's number, if there is one (beside its bell).
fn pill(n: usize) -> String {
    if n == 0 { String::new() } else { n.to_string() }
}

/// The pill's accessible name and hover.
fn label(waiting: usize, worth: usize) -> String {
    match (waiting, worth) {
        (0, 0) => "What needs you: nothing".into(),
        (w, 0) => format!("What needs you: {w} waiting"),
        (0, l) => format!("What needs you: {l} worth a look"),
        (w, l) => format!("What needs you: {w} waiting, {l} worth a look"),
    }
}

/// How long ago, as short as it gets: "now", "2m", "3h", "4d".
fn short_ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The small tag before a waiting item.
fn tag(kind: &str) -> &'static str {
    match kind {
        "approval" => "Approval",
        "question" => "Question",
        _ => "Decision",
    }
}

/// What a row says.
fn text(i: &AttentionItem) -> String {
    match i.kind.as_str() {
        "turn_ended" => format!("“{}” ended its turn", i.detail),
        "failed" => format!("“{}” failed", i.detail),
        "comment_answer" => match i.count.unwrap_or(1) {
            1 => format!("An answer on “{}”", i.title),
            n => format!("{n} answers on “{}”", i.title),
        },
        "brief" => match i.count.unwrap_or(0) {
            1 => "1 person waiting on you in the Brief".into(),
            n => format!("{n} people waiting on you in the Brief"),
        },
        _ => i.title.clone(),
    }
}

/// The colour of a worth-a-look row's dot.
fn dot(kind: &str) -> &'static str {
    match kind {
        "turn_ended" => "bg-emerald-500",
        "failed" => "bg-red-500",
        "comment_answer" => "bg-sky-500",
        _ => "bg-zinc-400",
    }
}

/// Where a row goes: an approval's or a question's session, Decisions for
/// another decision, a turn's session, the document of a comment answer,
/// the Brief.
fn target(i: &AttentionItem) -> View {
    match (i.kind.as_str(), &i.session_id, &i.doc_id) {
        ("approval" | "question", Some(s), _) => View::Session {
            id: s.clone(),
            target: i
                .call_id
                .as_ref()
                .map(|call| crate::workspace::Target { anchor: Some(crate::session::waiting_card_id(s, call)), ..Default::default() }),
        },
        ("turn_ended" | "failed", Some(s), _) => View::session(s.clone()),
        ("comment_answer", _, Some(d)) => View::Docs { id: d.clone() },
        ("brief", ..) => View::Brief,
        _ => View::Decisions,
    }
}

/// Whether opening a row means you've seen it (what's worth a look: a
/// turn's end, a document's answers, the Brief).
fn seen_by_opening(kind: &str) -> bool {
    matches!(kind, "turn_ended" | "failed" | "comment_answer" | "brief")
}

/// The line under a waiting row: where it is, its realm if not this page's,
/// and how long ago.
fn under(i: &AttentionItem, here: &str, now: i64) -> String {
    let realm = i.realm.as_deref().filter(|r| *r != here);
    let when = short_ago(now - i.at);
    [Some(i.detail.as_str()).filter(|d| !d.is_empty()), realm, Some(when.as_str())].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// Why the list isn't there (it says so, rather than nothing needing you).
fn load_error(e: &str) -> String {
    format!("Couldn't load what needs you: {e}")
}

/// Another realm's row.
fn elsewhere_text(e: &AttentionElsewhere) -> String {
    let name = if e.id == "root" { "the console" } else { e.name.as_str() };
    format!("{} waiting in {name}", e.waiting)
}

#[component]
pub fn NeedsYou() -> impl IntoView {
    let live = api::live();
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::attention()
    });
    // The last list loaded, kept while the next one comes; why the last
    // load failed, if it did.
    let shown = RwSignal::new(Attention::default());
    let loaded = RwSignal::new(false);
    let failed = RwSignal::new(None::<String>);
    // While "Mark all seen" is on its way, a list loaded before it would
    // put the rows back for a moment.
    let marking = StoredValue::new(false);
    Effect::new(move || match list.get() {
        Some(Ok(_)) if marking.get_value() => {}
        Some(Ok(a)) => {
            failed.set(None);
            loaded.set(true);
            // The same list again draws nothing again: a busy realm pings
            // often, and a row drawn anew under the pointer loses its click.
            if shown.with_untracked(|s| *s != a) {
                shown.set(a);
            }
        }
        Some(Err(e)) => failed.set(Some(e)),
        None => {}
    });
    let retry = move || tick.update(|t| *t += 1);
    // A ping (and the feed coming back, having missed some) loads it again,
    // once a burst has settled.
    let due = StoredValue::new(false);
    Effect::new(move |seen: Option<()>| {
        live.attention.track();
        let up = live.connected.get();
        if seen.is_none() || !up || due.try_get_value() != Some(false) {
            return;
        }
        due.set_value(true);
        set_timeout(
            move || {
                if due.try_update_value(|d| *d = false).is_some() {
                    let _ = tick.try_update(|t| *t += 1);
                }
            },
            SETTLE,
        );
    });
    let open = RwSignal::new(false);
    // Escape closes it, wherever focus is, and puts focus back on the pill.
    let pop = StoredValue::new(crate::util::escapable(
        move || open.get(),
        move || {
            open.try_set(false);
        },
    ));
    let counts = move || shown.with(|a| (a.waiting.len(), a.worth.len()));
    let pill_class = move || {
        let base = "inline-flex h-5 items-center justify-center gap-0.5 rounded-full px-1.5 text-[11px] font-semibold";
        match counts() {
            (w, _) if w > 0 => format!("{base} bg-amber-500 text-white"),
            (_, l) if l > 0 => format!("{base} border border-zinc-400 text-zinc-600 dark:border-zinc-500 dark:text-zinc-300"),
            _ => format!("{base} border border-zinc-300 text-muted dark:border-zinc-700"),
        }
    };
    let number = move || {
        let (w, l) = counts();
        pill(if w > 0 { w } else { l })
    };
    // The rows go at once; the head can take a while to mark them all.
    let mark_all = move |_| {
        marking.set_value(true);
        shown.update(|a| a.worth.clear());
        spawn_local(async move {
            let _ = api::attention_seen(Vec::new()).await;
            marking.try_set_value(false);
            let _ = tick.try_update(|t| *t += 1);
        });
    };
    let close = Callback::new(move |()| open.set(false));
    let title = move || {
        let (w, l) = counts();
        match failed.get() {
            Some(e) if !loaded.get() => load_error(&e),
            _ => label(w, l),
        }
    };
    view! {
        <div class="relative shrink-0">
            <button class="flex items-center rounded-md px-1 py-1 hover:bg-zinc-200/60 dark:hover:bg-zinc-800"
                aria-haspopup="dialog" aria-expanded=move || open.get().to_string() aria-label=title title=title data-pop=pop.get_value()
                on:click=move |_| {
                    // Opened after a failed load, it tries again.
                    if !open.get_untracked() && failed.get_untracked().is_some() {
                        retry();
                    }
                    open.update(|o| *o = !*o);
                }>
                // A small bell, tight against its number.
                <span class=pill_class>
                    <svg viewBox="0 0 12 12" class="h-2.5 w-2.5 shrink-0" fill="currentColor" aria-hidden="true">
                        <path d="M2 9h8l-1.4-2V5a2.6 2.6 0 0 0-5.2 0v2z" />
                        <circle cx="6" cy="10.4" r="1.1" />
                    </svg>
                    {number}
                </span>
            </button>
            <Show when=move || open.get()>
                // A click outside closes it (on a phone the page dims).
                <div class="fixed inset-0 z-40 bg-black/30 md:bg-transparent" on:click=move |_| open.set(false)></div>
                <div role="dialog" aria-label="What needs you" data-pop=pop.get_value()
                    class="fixed inset-x-0 bottom-0 z-50 flex max-h-[80dvh] flex-col rounded-t-xl border border-zinc-200 bg-white text-sm shadow-lg md:absolute md:inset-x-auto md:bottom-auto md:right-0 md:top-9 md:max-h-[70vh] md:w-[420px] md:rounded-lg dark:border-zinc-700 dark:bg-zinc-900">
                    <div class="flex items-center gap-2 border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
                        <span class="flex-1 font-semibold">"Needs you"</span>
                        {move || (counts().1 > 0).then(|| view! {
                            <button class="rounded px-1.5 py-0.5 text-xs text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                                title="What's worth a look stops being so until something new comes" on:click=mark_all>"Mark all seen"</button>
                        })}
                    </div>
                    <div class="min-h-0 overflow-y-auto p-1 pb-4 md:pb-1">
                        {move || failed.get().map(|e| view! {
                            <div role="alert" class="m-1 flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-2 py-1.5 text-xs text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200">
                                <span class="min-w-0 flex-1 break-words">{load_error(&e)}</span>
                                <button class="shrink-0 underline" on:click=move |_| retry()>"Try again"</button>
                            </div>
                        })}
                        {move || {
                            let a = shown.get();
                            let now = now_ms();
                            if !loaded.get() {
                                return failed.with(Option::is_none).then(|| view! { <p class="px-2 py-6 text-center text-muted">"Loading…"</p> }).into_any();
                            }
                            if a.waiting.is_empty() && a.worth.is_empty() && a.elsewhere.is_empty() {
                                return view! { <p class="px-2 py-6 text-center text-muted">"Nothing needs you."</p> }.into_any();
                            }
                            let here = place::realm().unwrap_or_else(|| "root".into());
                            view! {
                                {(!a.waiting.is_empty()).then(|| view! {
                                    <Heading text=format!("Waiting on you · {}", a.waiting.len()) />
                                    {a.waiting.iter().map(|i| {
                                        let (kind, title, line) = (tag(&i.kind), i.title.clone(), under(i, &here, now));
                                        view! {
                                            <Row item=i.clone() close>
                                                <span class="mt-0.5 shrink-0 rounded bg-amber-100 px-1 text-[10px] font-semibold uppercase tracking-wide text-amber-800 dark:bg-amber-950 dark:text-amber-300">{kind}</span>
                                                <span class="min-w-0 flex-1">
                                                    <span class="block truncate">{title}</span>
                                                    <span class="block truncate text-xs text-muted">{line}</span>
                                                </span>
                                            </Row>
                                        }
                                    }).collect_view()}
                                })}
                                {(!a.worth.is_empty()).then(|| view! {
                                    <Heading text=format!("Worth a look · {}", a.worth.len()) />
                                    {a.worth.iter().map(|i| {
                                        let (colour, says, when) = (format!("mt-1.5 h-2 w-2 shrink-0 rounded-full {}", dot(&i.kind)), text(i), short_ago(now - i.at));
                                        view! {
                                            <Row item=i.clone() close>
                                                <span class=colour></span>
                                                <span class="min-w-0 flex-1 truncate">{says}</span>
                                                <span class="shrink-0 text-xs text-muted">{when}</span>
                                            </Row>
                                        }
                                    }).collect_view()}
                                })}
                                {(!a.elsewhere.is_empty()).then(|| view! {
                                    <div class="my-1 border-t border-zinc-200 dark:border-zinc-800"></div>
                                    {a.elsewhere.iter().map(|e| {
                                        let to = if e.id == "root" { Place::Console } else { Place::Realm(e.id.clone()) };
                                        view! {
                                            <a rel="external" href=place::url_of(&to) class="block rounded-md px-2 py-1.5 text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800">
                                                {elsewhere_text(e)}
                                            </a>
                                        }
                                    }).collect_view()}
                                })}
                            }.into_any()
                        }}
                    </div>
                </div>
            </Show>
        </div>
    }
}

#[component]
pub(crate) fn Heading(text: String) -> impl IntoView {
    view! { <div class="px-2 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wide text-muted">{text}</div> }
}

/// A row: goes to the item in the focused tile (⌘/Ctrl-click: a new one)
/// and closes the list; what's worth a look is seen once you open it.
#[component]
fn Row(item: AttentionItem, close: Callback<()>, children: Children) -> impl IntoView {
    let ws = use_workspace();
    let to = target(&item);
    let href = to.href();
    let to = StoredValue::new(to);
    let (key, seen) = (item.key.clone(), seen_by_opening(&item.kind));
    view! {
        <a href=href class="flex items-start gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |ev: web_sys::MouseEvent| {
            if ev.button() != 0 || ev.shift_key() || ev.alt_key() {
                return;
            }
            ev.prevent_default();
            ws.open(None, to.get_value(), How::from_mouse(&ev));
            if seen {
                let key = key.clone();
                spawn_local(async move {
                    let _ = api::attention_seen(vec![key]).await;
                });
            }
            close.run(());
        }>{children()}</a>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str) -> AttentionItem {
        AttentionItem { kind: kind.into(), title: "Allow shell?".into(), detail: "Fix the build".into(), ..Default::default() }
    }

    #[test]
    fn rows_say_what_needs_you() {
        assert_eq!(text(&item("turn_ended")), "“Fix the build” ended its turn");
        assert_eq!(text(&item("failed")), "“Fix the build” failed");
        let answers = AttentionItem { title: "Plan".into(), detail: "3 answers".into(), count: Some(3), ..item("comment_answer") };
        assert_eq!(text(&answers), "3 answers on “Plan”");
        assert_eq!(text(&AttentionItem { count: Some(1), detail: "“per seat”".into(), ..answers }), "An answer on “Plan”");
        assert_eq!(text(&AttentionItem { count: Some(3), ..item("brief") }), "3 people waiting on you in the Brief");
        assert_eq!(text(&AttentionItem { count: Some(1), ..item("brief") }), "1 person waiting on you in the Brief");
        assert_eq!(text(&item("approval")), "Allow shell?");
        assert_eq!((tag("approval"), tag("question"), tag("decision")), ("Approval", "Question", "Decision"));
    }

    #[test]
    fn the_pill_names_both_counts() {
        assert_eq!(label(3, 2), "What needs you: 3 waiting, 2 worth a look");
        assert_eq!(label(3, 0), "What needs you: 3 waiting");
        assert_eq!(label(0, 2), "What needs you: 2 worth a look");
        assert_eq!(label(0, 0), "What needs you: nothing");
        assert_eq!(pill(0), "");
        assert_eq!(pill(4), "4");
    }

    #[test]
    fn a_failed_load_says_why() {
        assert_eq!(load_error("no realm \"mach5\""), "Couldn't load what needs you: no realm \"mach5\"");
    }

    #[test]
    fn approvals_and_questions_open_at_the_answerable_card_not_the_session_tail() {
        for kind in ["approval", "question"] {
            let i = AttentionItem { session_id: Some("child".into()), call_id: Some("call-1".into()), ..item(kind) };
            assert_eq!(
                target(&i),
                View::Session {
                    id: "child".into(),
                    target: Some(crate::workspace::Target { anchor: Some("waiting-child-call-1".into()), ..Default::default() }),
                }
            );
        }
        assert_eq!(target(&item("decision")), View::Decisions);
    }

    #[test]
    fn times_are_short() {
        assert_eq!(
            (short_ago(5_000), short_ago(120_000), short_ago(3 * 3_600_000), short_ago(2 * 86_400_000)),
            ("now".into(), "2m".into(), "3h".into(), "2d".into())
        );
    }

    #[test]
    fn under_a_row_its_realm_only_if_another() {
        let i = AttentionItem { realm: Some("home".into()), at: 0, ..item("approval") };
        assert_eq!(under(&i, "home", 120_000), "Fix the build · 2m");
        assert_eq!(under(&i, "lab", 120_000), "Fix the build · home · 2m");
        assert_eq!(under(&AttentionItem { detail: String::new(), ..i }, "home", 0), "now");
    }

    #[test]
    fn other_realms_say_only_how_many() {
        let e = AttentionElsewhere { id: "lab".into(), name: "Lab".into(), waiting: 2 };
        assert_eq!(elsewhere_text(&e), "2 waiting in Lab");
        assert_eq!(elsewhere_text(&AttentionElsewhere { id: "root".into(), ..e }), "2 waiting in the console");
    }

    #[test]
    fn rows_go_where_the_thing_is() {
        let s = Some("s1".to_owned());
        assert_eq!(target(&AttentionItem { session_id: s.clone(), ..item("approval") }), View::session("s1"));
        assert_eq!(target(&AttentionItem { session_id: s.clone(), ..item("failed") }), View::session("s1"));
        assert_eq!(target(&AttentionItem { session_id: s, ..item("decision") }), View::Decisions);
        assert_eq!(target(&AttentionItem { doc_id: Some("d1".into()), ..item("comment_answer") }), View::Docs { id: "d1".into() });
        assert_eq!(target(&item("brief")), View::Brief);
        assert!(["turn_ended", "failed", "comment_answer", "brief"].into_iter().all(seen_by_opening));
        assert!(!["approval", "question", "decision"].into_iter().any(seen_by_opening));
    }
}
