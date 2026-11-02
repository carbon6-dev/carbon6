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
/// every few minutes on the head).
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

/// How it reads: fine, getting low, or out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Fine,
    Low,
    Out,
}

impl Tone {
    pub fn class(self) -> &'static str {
        match self {
            Tone::Fine => "text-zinc-500",
            Tone::Low => "text-amber-600 dark:text-amber-400",
            Tone::Out => "font-medium text-red-600 dark:text-red-400",
        }
    }

    pub fn bar(self) -> &'static str {
        match self {
            Tone::Fine => "bg-emerald-500",
            Tone::Low => "bg-amber-500",
            Tone::Out => "bg-red-500",
        }
    }
}

pub fn tone(w: &QuotaWindow) -> Tone {
    if w.status == "rejected" || w.used_percent >= 100.0 {
        Tone::Out
    } else if w.status == "warning" || w.used_percent >= 80.0 {
        Tone::Low
    } else {
        Tone::Fine
    }
}

/// When it starts afresh, short: "40m", "2h", "Thu" (`day` names the
/// weekday a time falls on).
fn renews(at: i64, now: i64, day: &dyn Fn(i64) -> String) -> String {
    let m = ((at - now) / 60_000).max(0);
    match m {
        0 => "now".into(),
        m if m < 60 => format!("{m}m"),
        m if m < 24 * 60 => format!("{}h", (m + 30) / 60),
        _ => day(at),
    }
}

/// "in 2h 10m", "Thu 09:00".
fn renews_long(at: i64, now: i64, when: &dyn Fn(i64) -> String) -> String {
    let m = ((at - now) / 60_000).max(0);
    match m {
        0 => "now".into(),
        m if m < 24 * 60 => format!("in {}h {}m", m / 60, m % 60).replace("in 0h ", "in "),
        _ => when(at),
    }
}

/// The tightest window, short, for a picker: "66% left · renews 2h",
/// "week 29% left · Thu", "5h used up · renews 40m".
pub fn compact_at(q: &ConnectionQuota, now: i64, day: &dyn Fn(i64) -> String) -> Option<(String, Tone)> {
    let w = q.tightest()?;
    let t = tone(w);
    let left = (100.0 - w.used_percent).clamp(0.0, 100.0);
    let what = if t == Tone::Out { format!("{} used up", w.name) } else { format!("{} {left:.0}% left", w.name) };
    let text = match w.resets_at {
        Some(at) => format!("{what} · renews {}", renews(at, now, day)),
        None => what,
    };
    Some((text, t))
}

/// One window, for Settings and the Usage page: "5h: 34% used · resets in 2h 10m".
pub fn line_at(w: &QuotaWindow, now: i64, when: &dyn Fn(i64) -> String) -> String {
    let used = if tone(w) == Tone::Out { "used up".to_owned() } else { format!("{:.0}% used", w.used_percent) };
    match w.resets_at {
        Some(at) => format!("{}: {used} · resets {}", w.name, renews_long(at, now, when)),
        None => format!("{}: {used}", w.name),
    }
}

fn weekday(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][d.get_day() as usize % 7].to_owned()
}

fn weekday_time(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    format!("{} {:02}:{:02}", weekday(ms), d.get_hours(), d.get_minutes())
}

pub fn compact(q: &ConnectionQuota) -> Option<(String, Tone)> {
    compact_at(q, now_ms(), &weekday)
}

pub fn line(w: &QuotaWindow) -> String {
    line_at(w, now_ms(), &weekday_time)
}

/// The option text for a connection in a picker: its name, and what's left
/// of its plan.
pub fn option_label(id: &str, name: &str) -> String {
    match of(id).as_ref().and_then(compact) {
        Some((left, _)) => format!("{name} — {left}"),
        None => name.to_owned(),
    }
}

/// What's left of the chosen connection's plan, beside a picker.
#[component]
pub fn QuotaNote(#[prop(into)] connection: Signal<String>) -> impl IntoView {
    move || {
        let q = of(&connection.get())?;
        let (text, t) = compact(&q)?;
        let title = q.windows.iter().map(line).collect::<Vec<_>>().join("\n");
        Some(view! { <span class=format!("whitespace-nowrap text-xs {}", t.class()) title=title>{text}</span> })
    }
}

/// A connection's windows as bars, for Settings and the Usage page.
#[component]
pub fn QuotaBars(q: ConnectionQuota) -> impl IntoView {
    let error = (!q.error.is_empty()).then(|| q.error.clone());
    view! {
        <div class="space-y-1">
            {q.windows.iter().map(|w| {
                let t = tone(w);
                let pct = w.used_percent.clamp(0.0, 100.0);
                view! {
                    <div class="flex items-center gap-2 text-xs">
                        <div class="h-1.5 w-28 shrink-0 overflow-hidden rounded bg-zinc-200 dark:bg-zinc-800">
                            <div class=format!("h-full {}", t.bar()) style=format!("width:{pct:.1}%")></div>
                        </div>
                        <span class=t.class()>{line(w)}</span>
                    </div>
                }
            }).collect_view()}
            {error.map(|e| view! { <div class="text-xs text-zinc-500" title=e.clone()>"Last read failed; showing the last numbers seen."</div> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &str, used: f64, resets_at: Option<i64>, status: &str) -> QuotaWindow {
        QuotaWindow { name: name.into(), used_percent: used, resets_at, status: status.into() }
    }

    #[test]
    fn pickers_show_the_tightest_window() {
        let day = |_: i64| "Thu".to_owned();
        let q = ConnectionQuota {
            windows: vec![w("5h", 34.0, Some(2 * 3_600_000), ""), w("week", 71.0, Some(5 * 86_400_000), "")],
            ..Default::default()
        };
        assert_eq!(compact_at(&q, 0, &day), Some(("week 29% left · renews Thu".into(), Tone::Fine)));
        let q = ConnectionQuota {
            windows: vec![w("5h", 100.0, Some(40 * 60_000), "rejected"), w("week", 96.0, None, "warning")],
            ..Default::default()
        };
        assert_eq!(compact_at(&q, 0, &day), Some(("5h used up · renews 40m".into(), Tone::Out)));
        let q = ConnectionQuota { windows: vec![w("week", 85.0, None, "")], ..Default::default() };
        assert_eq!(compact_at(&q, 0, &day), Some(("week 15% left".into(), Tone::Low)));
        assert_eq!(compact_at(&ConnectionQuota::default(), 0, &day), None);
    }

    #[test]
    fn a_window_reads_with_its_reset() {
        let when = |_: i64| "Thu 09:00".to_owned();
        assert_eq!(line_at(&w("5h", 34.4, Some(130 * 60_000), ""), 0, &when), "5h: 34% used · resets in 2h 10m");
        assert_eq!(line_at(&w("5h", 34.0, Some(20 * 60_000), ""), 0, &when), "5h: 34% used · resets in 20m");
        assert_eq!(line_at(&w("week", 71.0, Some(3 * 86_400_000), ""), 0, &when), "week: 71% used · resets Thu 09:00");
        assert_eq!(line_at(&w("week", 100.0, None, "rejected"), 0, &when), "week: used up");
    }
}
