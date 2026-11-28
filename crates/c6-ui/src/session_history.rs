//! A prompt index, deliberately independent of the transcript's drawing window.
//! Only explicit navigation touches the feed. No model calls.

use c6_api::{PromptsPage, SessionPrompt};
use leptos::prelude::*;

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;

pub(crate) fn position(ts: i64, first: i64, last: i64) -> f64 {
    if last <= first {
        return 0.5;
    }
    ((ts as f64 - first as f64) / (last as f64 - first as f64)).clamp(0.0, 1.0)
}

pub(crate) fn time_at(y: f64, first: i64, last: i64) -> i64 {
    if last <= first {
        return first;
    }
    (first as f64 + y.clamp(0.0, 1.0) * (last as f64 - first as f64)).round() as i64
}

fn nearest(prompts: &[SessionPrompt], ts: i64) -> Option<usize> {
    let i = prompts.partition_point(|p| p.ts < ts);
    match (i.checked_sub(1), prompts.get(i)) {
        (Some(prev), Some(next)) => Some(if ts.saturating_sub(prompts[prev].ts) <= next.ts.saturating_sub(ts) { prev } else { i }),
        (Some(prev), None) => Some(prev),
        (None, Some(_)) => Some(0),
        _ => None,
    }
}

/// Calendar grouping is injected for native tests, not tied to a UTC day.
fn groups(prompts: &[SessionPrompt], bucket: impl Fn(i64) -> i64) -> Vec<(i64, Vec<SessionPrompt>)> {
    let mut out: Vec<(i64, Vec<SessionPrompt>)> = Vec::new();
    for p in prompts {
        let key = bucket(p.ts);
        match out.last_mut() {
            Some((day, rows)) if *day == key => rows.push(p.clone()),
            _ => out.push((key, vec![p.clone()])),
        }
    }
    out
}

fn matches(p: &SessionPrompt, query: &str) -> bool {
    p.text.to_lowercase().contains(&query.trim().to_lowercase())
}

fn navigate(prompts: &[SessionPrompt], current: usize, key: &str, day: impl Fn(i64) -> i64) -> usize {
    if prompts.is_empty() {
        return 0;
    }
    let current = current.min(prompts.len() - 1);
    match key {
        "ArrowUp" => current.saturating_sub(1),
        "ArrowDown" => (current + 1).min(prompts.len() - 1),
        "Home" => 0,
        "End" => prompts.len() - 1,
        "PageUp" => (0..current).rev().find(|&i| day(prompts[i].ts) < day(prompts[current].ts)).unwrap_or(0),
        "PageDown" => (current + 1..prompts.len()).find(|&i| day(prompts[i].ts) > day(prompts[current].ts)).unwrap_or(prompts.len() - 1),
        _ => current,
    }
}

fn date(ts: i64) -> js_sys::Date {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64))
}

fn local_day(ts: i64) -> i64 {
    let d = date(ts);
    js_sys::Date::new_with_year_month_day(d.get_full_year(), d.get_month() as i32, d.get_date() as i32).get_time() as i64
}

fn day_label(ts: i64) -> String {
    match crate::when::heading(ts).as_str() {
        "Today" => "Today".into(),
        "Yesterday" => "Yesterday".into(),
        _ => {
            let d = date(ts);
            let opts = js_sys::Object::new();
            for (k, v) in [("weekday", "short"), ("month", "short"), ("day", "numeric")] {
                let _ = js_sys::Reflect::set(&opts, &k.into(), &v.into());
            }
            String::from(d.to_locale_date_string("default", &opts))
        }
    }
}

fn duration(p: &SessionPrompt) -> String {
    match p.duration_ms {
        Some(ms) => format!("ran {}", crate::when::ago(ms).trim_end_matches(" ago")),
        None if p.start.is_some() => "running".into(),
        _ => "not started".into(),
    }
}

/// Bounded density bins. On short histories each prompt has a mark; at small
/// scales adjacent marks (including busy days) share a weighted bin.
fn marks(prompts: &[SessionPrompt], first: i64, last: i64, day: impl Fn(i64) -> i64) -> Vec<(usize, usize)> {
    let mut bins = [0usize; 180];
    if prompts.len() > bins.len() {
        // Zoomed out: one weighted mark per local calendar day. Extremely
        // long histories combine adjacent days in the bounded pixel bins.
        let mut days = std::collections::BTreeMap::<i64, (i128, usize)>::new();
        for p in prompts {
            let (sum, count) = days.entry(day(p.ts)).or_default();
            *sum += i128::from(p.ts);
            *count += 1;
        }
        for (sum, count) in days.into_values() {
            let at = (sum / count as i128) as i64;
            bins[(position(at, first, last) * 179.0).round() as usize] += count;
        }
    } else {
        for p in prompts {
            bins[(position(p.ts, first, last) * 179.0).round() as usize] += 1;
        }
    }
    bins.into_iter().enumerate().filter(|(_, n)| *n > 0).collect()
}

fn ticks(first: i64, last: i64) -> Vec<(i64, String)> {
    if last < first {
        return vec![];
    }
    let mut out = vec![(first, day_label(first))];
    let start = date(first);
    let days = ((last.saturating_sub(first) / DAY + 1) / 10).max(1) as i32;
    let mut i = 0;
    loop {
        let at = js_sys::Date::new_with_year_month_day(start.get_full_year(), start.get_month() as i32, start.get_date() as i32 + i)
            .get_time() as i64;
        if at > last {
            break;
        }
        if at >= first {
            out.push((at, day_label(at)));
        }
        i += days;
        if out.len() >= 12 {
            break;
        }
    }
    // Hours today use the actual timezone offset at each instant (DST too).
    let today = local_day(crate::util::now_ms());
    let mut at = today;
    while at <= last && at < today.saturating_add(25 * HOUR) && local_day(at) == today {
        if at >= first {
            out.push((at, format!("{:02}:00", date(at).get_hours())));
        }
        at += HOUR;
    }
    out.sort_by_key(|(ts, _)| *ts);
    let mut last_y = -1.0;
    out.retain(|(ts, _)| {
        let y = position(*ts, first, last);
        if y - last_y < 0.065 {
            false
        } else {
            last_y = y;
            true
        }
    });
    out
}

#[derive(Clone, Copy)]
pub(crate) struct History {
    pub data: RwSignal<PromptsPage>,
    pub error: RwSignal<Option<String>>,
    loaded: LocalResource<Result<PromptsPage, String>>,
    pub jump: Callback<(u64, String)>,
    session: StoredValue<String>,
}

impl History {
    pub fn new(session: String, revision: Signal<(u64, Vec<c6_api::QueuedInput>, bool)>, jump: Callback<(u64, String)>) -> Self {
        let session = StoredValue::new(session);
        let mark = Memo::new(move |_| revision.get());
        let loaded = LocalResource::new(move || {
            let _ = mark.get();
            async move { crate::api::session_prompts(session.get_value()).await }
        });
        let data = RwSignal::new(PromptsPage::default());
        let error = RwSignal::new(None);
        Effect::new(move || {
            if let Some(result) = loaded.get() {
                match result {
                    Ok(mut page) => {
                        page.prompts.sort_by_key(|p| (p.ts, p.seq));
                        data.set(page);
                        error.set(None);
                    }
                    Err(e) => error.set(Some(e)), // Never replace existing rows on a transient failure.
                }
            }
        });
        Self { data, error, loaded, jump, session }
    }

    fn go(self, p: &SessionPrompt) {
        if !p.queued {
            self.jump.run((p.seq, format!("{}-e{}", self.session.get_value(), p.seq)));
        }
    }
}

#[component]
pub(crate) fn PromptList(history: History, #[prop(into)] on_go: Callback<()>) -> impl IntoView {
    let query = RwSignal::new(String::new());
    let replies = RwSignal::new(false);
    let limit = RwSignal::new(100usize);
    let filtered = Memo::new(move |_| {
        history.data.with(|d| {
            let mut rows: Vec<_> = d.prompts.iter().chain(d.queued.iter()).filter(|p| matches(p, &query.get())).cloned().collect();
            rows.sort_by_key(|p| std::cmp::Reverse((p.ts, p.seq)));
            rows
        })
    });
    view! {
        <section class="c6-prompt-list px-1.5 py-3" aria-label="Your prompts">
            <div class="flex items-center justify-between gap-2">
                <h3 class=crate::components::SECTION_LABEL>"Your prompts"</h3>
                <button class="text-xs text-muted hover:underline" on:click=move |_| history.loaded.refetch()>"Refresh prompts"</button>
            </div>
            <input class=format!("{} my-2 w-full", crate::util::INPUT) type="search" aria-label="Filter your prompts" placeholder="Filter your prompts"
                prop:value=move || query.get() on:input=move |ev| { query.set(event_target_value(&ev)); limit.set(100); } />
            <label class="flex items-center gap-2 text-xs text-muted">
                <input type="checkbox" class=crate::components::CHECKBOX prop:checked=move || replies.get()
                    on:change=move |ev| replies.set(event_target_checked(&ev)) />"Show reply's first sentence"
            </label>
            {move || history.error.get().map(|e| view! { <p role="status" class="text-xs text-red-600">{e}</p> })}
            <div class="max-h-96 overflow-y-auto overscroll-contain">
                <For each=move || {
                    let rows = filtered.get();
                    groups(&rows[..rows.len().min(limit.get())], local_day).into_iter().map(|(day, _)| day).collect::<Vec<_>>()
                } key=|day| *day children=move |day| view! {
                    <h4 class="sticky top-0 bg-white py-2 text-xs font-medium dark:bg-zinc-900">{day_label(day)}</h4>
                    <For each=move || filtered.with(|rows| rows.iter().take(limit.get()).filter(|p| local_day(p.ts) == day).cloned().collect::<Vec<_>>())
                        key=|p| (p.seq, p.command_id.clone()) children=move |p| {
                        let prompt = Memo::new(move |_| history.data.with(|d| d.prompts.iter().chain(d.queued.iter())
                            .find(|q| q.seq == p.seq && q.command_id == p.command_id).cloned().unwrap_or_else(|| p.clone())));
                        view! { <PromptRow prompt replies history on_go /> }
                    } />
                } />
            </div>
            <Show when=move || history.loaded.get().is_none() && history.data.with(|d| d.prompts.is_empty())>
                <p class="py-2 text-xs text-muted" role="status">"Reading prompts…"</p>
            </Show>
            <Show when=move || history.loaded.get().is_some() && history.error.with(Option::is_none) && filtered.with(Vec::is_empty)>
                <p class="py-2 text-xs text-muted">"No prompts found."</p>
            </Show>
            <Show when=move || filtered.with(Vec::len) > limit.get()>
                <button class="py-2 text-xs text-sky-700" on:click=move |_| limit.update(|n| *n += 100)>"Show more prompts"</button>
            </Show>
        </section>
    }
}

#[component]
fn PromptRow(prompt: Memo<SessionPrompt>, replies: RwSignal<bool>, history: History, on_go: Callback<()>) -> impl IntoView {
    move || {
        let p = prompt.get();
        let copy = p.text.clone();
        let go = p.clone();
        let reply = p.reply.clone();
        let label = if p.queued {
            "Up next".into()
        } else if p.origin == "session" {
            format!("From {}", if p.from_title.is_empty() { "another session" } else { &p.from_title })
        } else if p.origin == "follow_up" {
            "Follow-up".into()
        } else {
            match p.provenance.as_deref() {
                Some(c6_api::MessageOrigin::Api { token_name }) => format!("Via API · {token_name}"),
                Some(c6_api::MessageOrigin::Unknown) => "Origin unknown".into(),
                Some(c6_api::MessageOrigin::Session) => "From another session".into(),
                Some(c6_api::MessageOrigin::TaskCard) => "From a task card".into(),
                Some(c6_api::MessageOrigin::TaskCardAnswer) => "Task-card answer".into(),
                _ => String::new(),
            }
        };
        view! {
            <div class="group border-b border-zinc-200 py-2 text-xs dark:border-zinc-800" data-prompt-seq=p.seq>
                <div class="text-muted"><time title=crate::when::full(p.ts)>{crate::when::time(p.ts)}</time>" · "{duration(&p)}" "{label}</div>
                <p class="break-words py-1">{if p.first_line.is_empty() { "(attachment)".into() } else { p.first_line }}</p>
                <Show when=move || replies.get()><p class="mb-1 break-words text-muted">{reply.clone()}</p></Show>
                <div class="flex items-center gap-3">
                    <button class="min-h-7 text-sky-700 hover:underline disabled:opacity-40 dark:text-sky-400 pointer-coarse:min-h-11" disabled=p.queued
                        on:click=move |_| { history.go(&go); on_go.run(()); }>"Jump to it"</button>
                    <crate::components::CopyButton text=Signal::derive(move || copy.clone()) label="Copy prompt" caption="Copy" />
                </div>
            </div>
        }
    }
}

#[component]
pub(crate) fn Scrubber(history: History, open_sheet: RwSignal<bool>) -> impl IntoView {
    let rail = NodeRef::<leptos::html::Div>::new();
    let selected = RwSignal::new(None::<u64>);
    let preview = RwSignal::new(false);
    let dragging = RwSignal::new(false);
    let last_jump = StoredValue::new(None::<u64>);
    let bounds = move || history.data.with(|d| (d.first_ts.unwrap_or(0), d.last_ts.unwrap_or(0)));
    let pick = move |client_y: i32, go: bool| {
        let Some(el) = rail.get_untracked() else { return };
        let rect = el.get_bounding_client_rect();
        let (first, last) = bounds();
        let ts = time_at((f64::from(client_y) - rect.top()) / rect.height().max(1.0), first, last);
        history.data.with_untracked(|d| {
            if let Some(i) = nearest(&d.prompts, ts) {
                let p = &d.prompts[i];
                selected.set(Some(p.seq));
                if go && last_jump.get_value() != Some(p.seq) {
                    last_jump.set_value(Some(p.seq));
                    history.go(p);
                }
            }
        });
    };
    let current = Memo::new(move |_| {
        history.data.with(|d| selected.get().and_then(|seq| d.prompts.iter().find(|p| p.seq == seq)).or_else(|| d.prompts.last()).cloned())
    });
    view! {
        <div class="c6-history-edge">
            <div node_ref=rail class="c6-history-ruler" role="slider" tabindex="0" aria-label="Session prompt history"
                aria-orientation="vertical" aria-valuemin=0 aria-valuemax=100
                aria-valuenow=move || current.get().map(|p| { let (a, b) = bounds(); (position(p.ts, a, b) * 100.0).round() as i64 })
                aria-valuetext=move || current.get().map(|p| format!("{}: {}", crate::when::full(p.ts), p.first_line))
                on:focus=move |_| preview.set(true) on:blur=move |_| preview.set(false)
                on:pointerenter=move |_| preview.set(true)
                on:pointerleave=move |_| if !dragging.get() { preview.set(false); }
                on:pointerdown=move |ev: web_sys::PointerEvent| {
                    if crate::util::phone() { open_sheet.set(true); return; }
                    ev.prevent_default();
                    if let Some(el) = rail.get_untracked() { let _ = el.focus(); let _ = el.set_pointer_capture(ev.pointer_id()); }
                    dragging.set(true); preview.set(true); last_jump.set_value(None); pick(ev.client_y(), true);
                }
                on:pointermove=move |ev: web_sys::PointerEvent| if !crate::util::phone() { pick(ev.client_y(), dragging.get()); }
                on:pointerup=move |ev: web_sys::PointerEvent| {
                    dragging.set(false);
                    if let Some(el) = rail.get_untracked() { let _ = el.release_pointer_capture(ev.pointer_id()); }
                }
                on:pointercancel=move |_| dragging.set(false)
                on:click=move |_| if crate::util::phone() { open_sheet.set(true); }
                on:keydown=move |ev| {
                    if matches!(ev.key().as_str(), "ArrowUp" | "ArrowDown" | "PageUp" | "PageDown" | "Home" | "End") {
                        ev.prevent_default(); ev.stop_propagation();
                        history.data.with_untracked(|d| {
                            if d.prompts.is_empty() { return; }
                            let i = selected.get_untracked().and_then(|s| d.prompts.iter().position(|p| p.seq == s)).unwrap_or(d.prompts.len() - 1);
                            let next = navigate(&d.prompts, i, &ev.key(), local_day);
                            selected.set(Some(d.prompts[next].seq)); history.go(&d.prompts[next]); preview.set(true);
                        });
                    } else if ev.key() == "Enter" && crate::util::phone() { open_sheet.set(true); }
                }>
                <For each=move || { let (a, b) = bounds(); history.data.with(|d| marks(&d.prompts, a, b, local_day)) } key=|mark| *mark children=move |(bin, count)| view! {
                    <span class="c6-history-mark" style=format!("top:{}%;opacity:{};height:{}px", bin as f64 / 179.0 * 100.0, (0.35 + (count as f64).ln_1p() / 5.0).min(1.0), if count > 3 { 3 } else { 1 })></span>
                } />
                {move || current.get().map(|p| view! {
                    <span class="c6-history-thumb" style=move || { let (a, b) = bounds(); format!("top:{}%", position(p.ts, a, b) * 100.0) }></span>
                })}
                <For each=move || { let (a, b) = bounds(); if history.data.with(|d| d.prompts.is_empty()) { vec![] } else { ticks(a, b) } } key=|(ts, _)| *ts children=move |(ts, label)| {
                    view! { <span class="c6-history-tick" style=move || { let (a, b) = bounds(); format!("top:{}%", position(ts, a, b) * 100.0) }>{label}</span> }
                } />
                <Show when=move || preview.get()>
                    {move || current.get().map(|p| view! {
                        <div class="c6-history-preview" role="tooltip">
                            <div class="font-medium">{p.first_line}</div>
                            <div class="text-muted">{crate::when::full(p.ts)}" · "{duration(&p)}</div>
                        </div>
                    })}
                </Show>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(seq: u64, ts: i64, text: &str) -> SessionPrompt {
        SessionPrompt { seq, ts, text: text.into(), ..Default::default() }
    }

    #[test]
    fn mapping_preserves_quiet_gaps_and_round_trips() {
        let (first, last) = (1_790_000_000_000, 1_790_900_000_000);
        for at in [first, first + 100, first + 4 * DAY, last] {
            assert!((time_at(position(at, first, last), first, last) - at).abs() <= 1);
        }
        assert_eq!(position(first - 1, first, last), 0.0);
        assert_eq!(position(last + 1, first, last), 1.0);
        assert_eq!(position(42, 42, 42), 0.5);
        assert_eq!(time_at(0.9, 42, 42), 42);
        assert_eq!(time_at(-1.0, first, last), first);
        assert_eq!(time_at(2.0, first, last), last);
    }

    #[test]
    fn grouping_uses_local_calendar_day_and_hour_including_half_hour_offsets() {
        let rows = [prompt(1, HOUR, "a"), prompt(2, HOUR + 10, "b"), prompt(3, 2 * HOUR, "c"), prompt(4, DAY, "d")];
        let days = groups(&rows, |ts| ts.div_euclid(DAY));
        assert_eq!(days.iter().map(|(day, ps)| (*day, ps.len())).collect::<Vec<_>>(), [(0, 3), (1, 1)]);
        let hours = groups(&rows, |ts| (ts + 30 * 60_000).div_euclid(HOUR));
        assert_eq!(hours.iter().map(|(hour, ps)| (*hour, ps.len())).collect::<Vec<_>>(), [(1, 2), (2, 1), (24, 1)]);
        // A local day shortened to 23h by DST: grouping must use calendar
        // boundaries, not a fixed division by 24h.
        let rows = [prompt(1, 22 * HOUR, "before"), prompt(2, 23 * HOUR, "after")];
        assert_eq!(groups(&rows, |ts| if ts < 23 * HOUR { 0 } else { 23 * HOUR }).len(), 2);
        assert!(groups(&[], |_| 0).is_empty());
    }

    #[test]
    fn keyboard_moves_by_prompt_and_populated_calendar_day() {
        let rows = [prompt(1, 0, "a"), prompt(2, HOUR, "b"), prompt(3, DAY + HOUR, "c"), prompt(4, 3 * DAY, "d")];
        let day = |ts: i64| ts.div_euclid(DAY);
        for (current, key, expected) in [
            (0, "ArrowUp", 0),
            (0, "ArrowDown", 1),
            (3, "ArrowDown", 3),
            (1, "PageDown", 2),
            (2, "PageUp", 1),
            (3, "PageUp", 2),
            (0, "End", 3),
            (3, "Home", 0),
            (1, "x", 1),
        ] {
            assert_eq!(navigate(&rows, current, key, day), expected, "{current} {key}");
        }
        assert_eq!(navigate(&[], 99, "ArrowDown", day), 0);
        assert_eq!(nearest(&rows, HOUR / 2), Some(0));
        assert_eq!(nearest(&rows, DAY), Some(2));
        assert_eq!(nearest(&[], 0), None);
    }

    #[test]
    fn filter_searches_complete_input_but_never_the_reply_or_sender() {
        let p = SessionPrompt { reply: "Secret reply".into(), from_title: "Sender".into(), ..prompt(1, 0, "First line\nDetailed NEEDLE") };
        assert!(matches(&p, " needle "));
        assert!(matches(&p, ""));
        assert!(!matches(&p, "secret"));
        assert!(!matches(&p, "sender"));
    }

    #[test]
    fn thousands_of_prompts_have_bounded_marks_not_thousands_of_elements() {
        let rows = (0..10_000).map(|i| prompt(i + 1, i as i64 * HOUR, "a")).collect::<Vec<_>>();
        let bins = marks(&rows, rows[0].ts, rows.last().unwrap().ts, |ts| ts.div_euclid(DAY));
        assert!(bins.len() <= 180);
        assert_eq!(bins.iter().map(|(_, n)| n).sum::<usize>(), rows.len());
        assert_eq!(marks(&[], 0, 0, |_| 0), []);
        let day = (0..1000).map(|i| prompt(i + 1, i as i64, "a")).collect::<Vec<_>>();
        assert_eq!(marks(&day, 0, DAY, |_| 0).len(), 1, "a busy day groups into one mark at small scale");
    }
}
