//! Plan limits per connection (docs/analytics.md, "Plan limits"): how much
//! of a Claude Code subscription's or a ChatGPT plan's windows is left,
//! and when each starts afresh; shown with the connection wherever you
//! pick one.

use c6_api::{ConnectionQuota, QuotaWindow};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::now_ms;

/// Every connection's limits as last seen, kept fresh while the app is open.
#[derive(Clone, Copy)]
pub struct Quotas(pub RwSignal<Vec<ConnectionQuota>>);

thread_local! {
    /// The app's one copy: read from anywhere a connection is shown (tiles
    /// and popovers render outside the root's context).
    static QUOTAS: std::cell::OnceCell<Quotas> = const { std::cell::OnceCell::new() };
}

/// Read now and every minute (Claude Code's come with its turns, ChatGPT's
/// with model replies to the head).
pub fn provide() {
    let q = RwSignal::new(Vec::new());
    let load = move || {
        spawn_local(async move {
            if let Ok(v) = api::quotas().await {
                q.set(v);
            }
        })
    };
    load();
    set_interval(load, std::time::Duration::from_secs(60));
    QUOTAS.with(|c| {
        let _ = c.set(Quotas(q));
    });
}

/// Every connection's limits (tracked), once [`provide`] ran.
pub fn all() -> Vec<ConnectionQuota> {
    QUOTAS.with(|c| c.get().copied()).map(|q| q.0.get()).unwrap_or_default()
}

/// A connection's limits, if it has a plan with any.
pub fn of(connection: &str) -> Option<ConnectionQuota> {
    let q = QUOTAS.with(|c| c.get().copied())?;
    q.0.with(|v| v.iter().find(|q| q.connection_id == connection && !q.windows.is_empty()).cloned())
}

fn source_note(w: &QuotaWindow) -> Option<String> {
    w.origin.as_ref().filter(|o| o.source == "codex-cli").map(|o| format!("via Codex on {}", o.worker_name))
}

/// Worker diagnostics and snapshots with no safe connection-account match
/// live separately, never in a model picker or a connection's bars.
#[component]
pub fn UnmatchedCodexLimits(#[prop(default = false)] list_rows: bool) -> impl IntoView {
    move || {
        all()
            .into_iter()
            .filter(|q| q.connection_id.starts_with("codex-cli:"))
            .map(move |q| view! { <UnmatchedCodexRow q list_rows /> })
            .collect_view()
    }
}

fn unmatched_label(q: &ConnectionQuota) -> String {
    let origin = q.origin.as_ref();
    let worker = origin.map(|o| o.worker_name.as_str()).unwrap_or("unknown worker");
    let account =
        origin.and_then(|o| o.account_id.as_deref()).map(|s| s.chars().take(8).collect::<String>()).unwrap_or_else(|| "unknown".into());
    format!("Codex on {worker} · account {account}, matches no connection")
}

#[component]
fn UnmatchedCodexRow(q: ConnectionQuota, list_rows: bool) -> impl IntoView {
    let label = unmatched_label(&q);
    let reason = q
        .origin
        .as_ref()
        .map(|o| o.unmatched_reason.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "No email or account-id match".into());
    let error = q.error.clone();
    view! {
        <div class=if list_rows { "space-y-1 px-4 py-3 text-xs text-muted" } else { "space-y-1 text-xs text-muted" }>
            <div class="font-medium">{label}</div>
            <div>{reason}</div>
            <QuotaBars q />
            {(!error.is_empty()).then(|| view! { <div>{error}</div> })}
        </div>
    }
}

/// Last executable actually selected on a worker, including matched windows.
pub fn codex_command(worker: &str) -> Option<(String, bool)> {
    let quotas = all();
    quotas
        .iter()
        .flat_map(|q| q.origin.iter().chain(q.windows.iter().filter_map(|w| w.origin.as_ref())))
        .filter(|o| o.worker_id == worker && o.command.is_some())
        .max_by_key(|o| o.observed_at)
        .and_then(|o| o.command.clone().map(|p| (p, o.command_from_shell)))
}

/// How much is left, by level: more than half (quiet grey), half down to
/// a fifth (yellow), a fifth down to 5% (amber), under 5% (red).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tone {
    Plenty,
    Half,
    Low,
    Out,
}

impl Tone {
    pub fn class(self) -> &'static str {
        match self {
            Tone::Plenty => "text-muted",
            Tone::Half => "text-yellow-600 dark:text-yellow-300",
            Tone::Low => "text-amber-600 dark:text-amber-400",
            Tone::Out => "text-red-600 dark:text-red-400",
        }
    }

    pub fn bar(self) -> &'static str {
        match self {
            Tone::Plenty => "bg-zinc-500 dark:bg-zinc-400",
            Tone::Half => "bg-yellow-400",
            Tone::Low => "bg-amber-500",
            Tone::Out => "bg-red-500",
        }
    }
}

/// The level for `left` percent left: over 50 plenty, 20 to 50 half, 5 to
/// 20 low, under 5 out. The provider's own word counts too: used up
/// ("rejected") is out, near its limit ("warning") at least low.
pub fn level(left: f64, status: &str) -> Tone {
    let t = if left < 5.0 {
        Tone::Out
    } else if left <= 20.0 {
        Tone::Low
    } else if left <= 50.0 {
        Tone::Half
    } else {
        Tone::Plenty
    };
    match status {
        "rejected" => Tone::Out,
        "warning" => t.max(Tone::Low),
        _ => t,
    }
}

/// Percent of a window left, whole (the number shown, so its level
/// matches what reads).
pub fn left(w: &QuotaWindow) -> f64 {
    (100.0 - w.used_percent).clamp(0.0, 100.0).round()
}

pub fn tone(w: &QuotaWindow) -> Tone {
    level(left(w), &w.status)
}

/// Used up: nothing left, or the provider says so.
fn used_up(w: &QuotaWindow) -> bool {
    left(w) == 0.0 || w.status == "rejected"
}

fn remaining(w: &QuotaWindow) -> String {
    if used_up(w) { "used up".to_owned() } else { format!("{:.0}% left", left(w)) }
}

/// A window's name to read: "5h" "5-hour", "30m" "30-minute", "week
/// (Opus)" "Week (Opus)".
pub fn window_label(name: &str) -> String {
    let count = |unit: char| name.strip_suffix(unit).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if let Some(n) = count('h') {
        return format!("{n}-hour");
    }
    if let Some(n) = count('m') {
        return format!("{n}-minute");
    }
    let mut c = name.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

const DAY_MS: i64 = 86_400_000;
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Month and day of the month of a day counted from 1970-01-01 (Howard
/// Hinnant's civil_from_days).
fn month_day(days: i64) -> (usize, i64) {
    let doe = (days + 719_468).rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (month as usize, doy - (153 * mp + 2) / 5 + 1)
}

/// Minutes into the day on a 12-hour clock: "3:40 pm", "12:05 am".
fn clock(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    format!("{}:{m:02} {}", if h % 12 == 0 { 12 } else { h % 12 }, if h < 12 { "am" } else { "pm" })
}

/// When a window starts afresh, in local time (`at_east` and `now_east`:
/// the zone's minutes east of UTC then and now), to the nearest minute:
/// "today 3:40 pm", "tomorrow 9:00 am", or "Sun Nov 8" then `sep` then
/// "9:00 am"; "now" once it's past.
pub fn resets_at(at: i64, now: i64, at_east: i64, now_east: i64, sep: &str) -> String {
    if at <= now {
        return "now".into();
    }
    let local = (at + 30_000).div_euclid(60_000) * 60_000 + at_east * 60_000;
    let day = local.div_euclid(DAY_MS);
    let today = (now + now_east * 60_000).div_euclid(DAY_MS);
    let time = clock(local.rem_euclid(DAY_MS) / 60_000);
    match day - today {
        0 => format!("today {time}"),
        1 => format!("tomorrow {time}"),
        _ => {
            let (month, d) = month_day(day);
            format!("{} {} {d}{sep}{time}", WEEKDAYS[(day + 4).rem_euclid(7) as usize], MONTHS[month - 1])
        }
    }
}

/// One window as a sentence, for a hover: "Week: 90% left · resets Sun Nov
/// 8, 9:00 am" (`when` gives the reset's phrase).
pub fn sentence_at(w: &QuotaWindow, when: &dyn Fn(i64) -> String) -> String {
    line_at(w, when)
}

/// One window, for Settings and the Usage page: "5-hour: 66% left · resets
/// today 3:40 pm".
pub fn line_at(w: &QuotaWindow, when: &dyn Fn(i64) -> String) -> String {
    let remaining = remaining(w);
    match w.resets_at {
        Some(at) => format!("{}: {remaining} · resets {}", window_label(&w.name), when(at)),
        None => format!("{}: {remaining}", window_label(&w.name)),
    }
}

/// Minutes east of UTC in the browser's zone at `ms`.
fn east(ms: i64) -> i64 {
    -js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64)).get_timezone_offset() as i64
}

/// When a window starts afresh, in the browser's local time.
pub fn resets(at: i64, sep: &str) -> String {
    let now = now_ms();
    resets_at(at, now, east(at), east(now), sep)
}

pub fn sentence(w: &QuotaWindow) -> String {
    let sentence = sentence_at(w, &|at| resets(at, ", "));
    source_note(w).map(|note| format!("{sentence} ({note})")).unwrap_or(sentence)
}

pub fn line(w: &QuotaWindow) -> String {
    line_at(w, &|at| resets(at, ", "))
}

/// The option text for a connection in a picker: its name, and what's left
/// of its plan's tightest window ("Max · 90% left").
pub fn option_label(id: &str, name: &str) -> String {
    match of(id).as_ref().and_then(|q| q.tightest().map(remaining)) {
        Some(left) => format!("{name} · {left}"),
        None => name.to_owned(),
    }
}

/// What's left of the chosen connection's plan: beside a picker "90% left"
/// (every window on hover), or `chip`, after the model in the message
/// box's chip, "· 90% left" (the tightest window on hover). Nothing without a
/// plan.
#[component]
pub fn QuotaNote(#[prop(into)] connection: Signal<String>, #[prop(optional)] chip: bool) -> impl IntoView {
    move || {
        let q = of(&connection.get())?;
        let w = q.tightest()?;
        let t = tone(w);
        Some(if chip {
            view! {
                <span class="text-muted" aria-hidden="true">"·"</span>
                // A small bar of what's left before the number, so the
                // grey of plenty still reads as usage.
                <span class=format!("inline-flex items-center gap-1 whitespace-nowrap {}", t.class()) title=sentence(w)>
                    <span class="inline-block h-1 w-3.5 overflow-hidden rounded-full bg-zinc-300 dark:bg-zinc-700" aria-hidden="true">
                        <span class=format!("block h-full rounded-full {}", t.bar()) style=format!("width: {:.0}%", left(w))></span>
                    </span>
                    {remaining(w)}
                </span>
            }
            .into_any()
        } else {
            let title = q.windows.iter().map(sentence).collect::<Vec<_>>().join("\n");
            view! { <span class=format!("whitespace-nowrap text-xs {}", t.class()) title=title>{remaining(w)}</span> }.into_any()
        })
    }
}

/// The model menu's Usage section, above its Model section: each window's
/// bar of what's left and when it starts afresh, then a divider. Nothing
/// without a plan.
#[component]
pub fn PlanMenu(#[prop(into)] connection: Signal<String>) -> impl IntoView {
    move || {
        let q = of(&connection.get())?;
        let error = (!q.error.is_empty()).then(|| q.error.clone());
        Some(view! {
            <div class="space-y-2 border-b border-zinc-200 pb-2 dark:border-zinc-700">
                <div class="font-medium uppercase tracking-wide text-muted">"Usage"</div>
                {q.windows.iter().map(|w| {
                    let t = tone(w);
                    let pct = left(w);
                    view! {
                        <div class="w-72 max-w-full" title=sentence(w)>
                            <div class="flex items-center gap-2">
                                <span class="w-28 shrink-0 truncate">{window_label(&w.name)}</span>
                                <div class="h-1 min-w-0 flex-1 overflow-hidden rounded bg-zinc-200 dark:bg-zinc-800">
                                    <div class=format!("h-full {}", t.bar()) style=format!("width:{pct:.0}%")></div>
                                </div>
                                <span class=format!("shrink-0 whitespace-nowrap {}", t.class())>{remaining(w)}</span>
                            </div>
                            {w.resets_at.map(|at| view! { <div class="text-muted">{format!("resets {}", resets(at, ", "))}</div> })}
                            {source_note(w).map(|note| view! { <div class="text-muted">{note}</div> })}
                        </div>
                    }
                }).collect_view()}
                {error.map(|e| view! { <div class="text-muted" title=e>"Last read failed; showing the last numbers seen."</div> })}
            </div>
        })
    }
}

/// A connection's windows as bars of what's left, for Settings and the
/// Usage page.
#[component]
pub fn QuotaBars(q: ConnectionQuota) -> impl IntoView {
    let error = (!q.error.is_empty()).then(|| q.error.clone());
    view! {
        <div class="space-y-1">
            {q.windows.iter().map(|w| {
                let t = tone(w);
                let pct = left(w);
                let text = t.class();
                view! {
                    <div class="flex items-center gap-2 text-xs">
                        <div class="h-1.5 w-28 shrink-0 overflow-hidden rounded bg-zinc-200 dark:bg-zinc-800">
                            <div class=format!("h-full {}", t.bar()) style=format!("width:{pct:.1}%")></div>
                        </div>
                        <span class=text>{line(w)}</span>
                        {source_note(w).map(|note| view! { <span class="text-muted">{note}</span> })}
                    </div>
                }
            }).collect_view()}
            {error.map(|e| view! { <div class="text-xs text-muted" title=e.clone()>"Last read failed; showing the last numbers seen."</div> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmatched_codex_row_names_worker_masks_account_and_explains_no_match() {
        let q = ConnectionQuota {
            connection_id: "codex-cli:worker".into(),
            origin: Some(c6_api::QuotaOrigin {
                worker_name: "thalassa".into(),
                account_id: Some("93837d23-secret".into()),
                unmatched_reason: "No email or account-id match: connections have no account id; emails differ".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(unmatched_label(&q), "Codex on thalassa · account 93837d23, matches no connection");
        let owner = Owner::new();
        let html = owner.with(|| view! { <UnmatchedCodexRow q list_rows=true /> }.to_html());
        assert!(html.contains("px-4 py-3"), "styled as a connection-list row");
        assert!(html.contains("emails differ"));
        assert!(!html.contains("93837d23-secret"));
    }

    #[test]
    fn settings_and_usage_bars_show_remaining_not_used() {
        let owner = Owner::new();
        for (used, left, text) in [(37.0, 63.0, "Week: 63% left"), (100.0, 0.0, "Week: used up")] {
            let q = ConnectionQuota { windows: vec![w("week", used, None, "")], ..Default::default() };
            let html = owner.with(|| view! { <QuotaBars q /> }.to_html());
            assert!(html.contains(text), "{html}");
            assert!(html.contains(&format!("width:{left:.1}%")), "{html}");
            assert!(!html.contains("% used"));
        }
    }

    fn w(name: &str, used: f64, resets_at: Option<i64>, status: &str) -> QuotaWindow {
        QuotaWindow { name: name.into(), used_percent: used, resets_at, status: status.into(), ..Default::default() }
    }

    #[test]
    fn levels_by_what_is_left() {
        assert_eq!(level(100.0, ""), Tone::Plenty);
        assert_eq!(level(51.0, ""), Tone::Plenty);
        assert_eq!(level(50.0, ""), Tone::Half);
        assert_eq!(level(21.0, ""), Tone::Half);
        assert_eq!(level(20.0, ""), Tone::Low);
        assert_eq!(level(5.0, ""), Tone::Low);
        assert_eq!(level(4.0, ""), Tone::Out);
        assert_eq!(level(0.0, ""), Tone::Out);
        // The provider's word.
        assert_eq!(level(90.0, "warning"), Tone::Low);
        assert_eq!(level(3.0, "warning"), Tone::Out);
        assert_eq!(level(40.0, "rejected"), Tone::Out);
        assert_eq!(level(90.0, "allowed"), Tone::Plenty);
        // The number shown decides: 4.6% left reads 5%, low.
        assert_eq!(tone(&w("5h", 95.4, None, "")), Tone::Low);
        assert_eq!(tone(&w("5h", 95.6, None, "")), Tone::Out);
    }

    #[test]
    fn plenty_is_quiet_grey_then_yellow_amber_red() {
        let colours = |left: f64| (level(left, "").class(), level(left, "").bar());
        assert_eq!(colours(90.0), ("text-muted", "bg-zinc-500 dark:bg-zinc-400"));
        assert_eq!(colours(40.0), ("text-yellow-600 dark:text-yellow-300", "bg-yellow-400"));
        assert_eq!(colours(10.0), ("text-amber-600 dark:text-amber-400", "bg-amber-500"));
        assert_eq!(colours(2.0), ("text-red-600 dark:text-red-400", "bg-red-500"));
    }

    #[test]
    fn the_chip_shows_the_tightest_window() {
        let tightest_left = |q: &ConnectionQuota| q.tightest().map(|w| (left(w), tone(w)));
        let q = ConnectionQuota {
            windows: vec![w("5h", 34.0, None, ""), w("week", 71.0, None, ""), w("week (Opus)", 12.0, None, "")],
            ..Default::default()
        };
        assert_eq!(tightest_left(&q), Some((29.0, Tone::Half)));
        let q = ConnectionQuota { windows: vec![w("5h", 80.0, None, "rejected"), w("week", 96.0, None, "")], ..Default::default() };
        assert_eq!(tightest_left(&q), Some((20.0, Tone::Out)));
        assert_eq!(tightest_left(&ConnectionQuota::default()), None);
    }

    #[test]
    fn windows_read_by_name() {
        assert_eq!(window_label("5h"), "5-hour");
        assert_eq!(window_label("30m"), "30-minute");
        assert_eq!(window_label("week"), "Week");
        assert_eq!(window_label("week (Opus)"), "Week (Opus)");
        assert_eq!(window_label("overage"), "Overage");
        assert_eq!(window_label(""), "");
    }

    #[test]
    fn codex_provenance_is_quiet_and_specific_to_each_window() {
        let mut window = w("5h", 20.0, None, "");
        assert_eq!(source_note(&window), None);
        window.origin = Some(c6_api::QuotaOrigin { source: "codex-cli".into(), worker_name: "test-machine".into(), ..Default::default() });
        assert_eq!(source_note(&window).as_deref(), Some("via Codex on test-machine"));
        assert_eq!(sentence(&window), "5-hour: 80% left (via Codex on test-machine)");
        window.origin.as_mut().unwrap().source = "chatgpt".into();
        assert_eq!(source_note(&window), None);
    }

    // Mon Nov 2 2026, 10:15 am UTC.
    const NOW: i64 = 1_793_614_500_000;
    const MIN: i64 = 60_000;

    #[test]
    fn resets_today_tomorrow_or_on_a_date() {
        assert_eq!(resets_at(NOW + (5 * 60 + 25) * MIN, NOW, 0, 0, " · "), "today 3:40 pm");
        assert_eq!(resets_at(NOW + (22 * 60 + 45) * MIN, NOW, 0, 0, " · "), "tomorrow 9:00 am");
        let sun = NOW + 5 * DAY_MS + (22 * 60 + 45) * MIN;
        assert_eq!(resets_at(sun, NOW, 0, 0, " · "), "Sun Nov 8 · 9:00 am");
        assert_eq!(resets_at(sun, NOW, 0, 0, ", "), "Sun Nov 8, 9:00 am");
        // To the nearest minute.
        assert_eq!(resets_at(NOW + (5 * 60 + 25) * MIN - 20_000, NOW, 0, 0, ""), "today 3:40 pm");
        assert_eq!(resets_at(NOW, NOW, 0, 0, ""), "now");
        // Across a month and a year.
        assert_eq!(resets_at(NOW + 26 * DAY_MS + (13 * 60 + 45) * MIN, NOW, 0, 0, " "), "Sun Nov 29 12:00 am");
        assert_eq!(resets_at(NOW + 59 * DAY_MS + (13 * 60 + 45) * MIN, NOW, 0, 0, " "), "Fri Jan 1 12:00 am");
    }

    #[test]
    fn resets_in_local_time() {
        // 10:15 am UTC is 2:15 am in UTC-8; 11:00 pm UTC is 3:00 pm there.
        assert_eq!(resets_at(NOW + (12 * 60 + 45) * MIN, NOW, -480, -480, ""), "today 3:00 pm");
        // 11:30 pm UTC is 1:30 am tomorrow in UTC+2.
        assert_eq!(resets_at(NOW + (13 * 60 + 15) * MIN, NOW, 120, 120, ""), "tomorrow 1:30 am");
        // 12:30 am UTC Tuesday is still Monday, 4:30 pm, in UTC-8.
        assert_eq!(resets_at(NOW + (14 * 60 + 15) * MIN, NOW, -480, -480, ""), "today 4:30 pm");
        // Daylight saving ending in between: the zone at the reset counts.
        assert_eq!(resets_at(NOW + (12 * 60 + 45) * MIN, NOW, -480, -420, ""), "today 3:00 pm");
    }

    #[test]
    fn twelve_hour_clock() {
        assert_eq!(clock(0), "12:00 am");
        assert_eq!(clock(5), "12:05 am");
        assert_eq!(clock(9 * 60), "9:00 am");
        assert_eq!(clock(12 * 60), "12:00 pm");
        assert_eq!(clock(15 * 60 + 40), "3:40 pm");
        assert_eq!(clock(23 * 60 + 59), "11:59 pm");
    }

    #[test]
    fn a_window_reads_with_its_reset() {
        let when = |_: i64| "Sun Nov 8, 9:00 am".to_owned();
        assert_eq!(sentence_at(&w("week", 10.0, Some(1), ""), &when), "Week: 90% left · resets Sun Nov 8, 9:00 am");
        assert_eq!(sentence_at(&w("5h", 100.0, Some(1), "rejected"), &when), "5-hour: used up · resets Sun Nov 8, 9:00 am");
        assert_eq!(sentence_at(&w("week", 71.0, None, ""), &when), "Week: 29% left");
        assert_eq!(line_at(&w("5h", 34.4, Some(1), ""), &when), "5-hour: 66% left · resets Sun Nov 8, 9:00 am");
        assert_eq!(line_at(&w("week", 37.0, Some(1), ""), &when), "Week: 63% left · resets Sun Nov 8, 9:00 am");
        assert_eq!(line_at(&w("week", 100.0, None, "rejected"), &when), "Week: used up");
    }
}
