//! Dates and times, shown the same way on every page, in the browser's
//! locale: compact in lists, cards and rows; full where one thing is open
//! (a message, a request, a reply's quote), always in view rather than on
//! hover; "5m ago" where a page shows recency. Calendars keep their own.
//!
//! | When              | Compact      | Full                       |
//! |-------------------|--------------|----------------------------|
//! | Today             | 3:41 PM      | Today, 3:41 PM             |
//! | Yesterday         | Yesterday    | Yesterday, 3:41 PM         |
//! | Earlier this year | Wed, Oct 14  | Wed, Oct 14, 3:41 PM       |
//! | Another year      | Oct 14, 2025 | Tue, Oct 14, 2025, 3:41 PM |

/// Which of the table's rows a day is, seen from today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Near {
    Today,
    Yesterday,
    /// This year: before yesterday, or ahead (a snooze, the next run).
    ThisYear,
    OtherYear,
}

/// Where `day` is from `today`: both local calendar days (year, month
/// 1–12, day of the month).
pub fn near(day: (i32, u32, u32), today: (i32, u32, u32)) -> Near {
    let (d, t) = (days(day), days(today));
    if d == t {
        Near::Today
    } else if d + 1 == t {
        Near::Yesterday
    } else if day.0 == today.0 {
        Near::ThisYear
    } else {
        Near::OtherYear
    }
}

/// Days since 1970-01-01 of a calendar day (proleptic Gregorian).
fn days((y, m, d): (i32, u32, u32)) -> i64 {
    let y = i64::from(if m <= 2 { y - 1 } else { y });
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let (m, d) = (i64::from(m), i64::from(d));
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn date(ms: i64) -> js_sys::Date {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64))
}

fn ymd(d: &js_sys::Date) -> (i32, u32, u32) {
    (d.get_full_year() as i32, d.get_month() + 1, d.get_date())
}

fn near_now(d: &js_sys::Date) -> Near {
    near(ymd(d), ymd(&js_sys::Date::new_0()))
}

fn opts(pairs: &[(&str, &str)]) -> js_sys::Object {
    let o = js_sys::Object::new();
    for (k, v) in pairs {
        let _ = js_sys::Reflect::set(&o, &(*k).into(), &(*v).into());
    }
    o
}

const WEEKDAY: [(&str, &str); 3] = [("weekday", "short"), ("month", "short"), ("day", "numeric")];
const WEEKDAY_YEAR: [(&str, &str); 4] = [("weekday", "short"), ("month", "short"), ("day", "numeric"), ("year", "numeric")];

fn day_of(d: &js_sys::Date, pairs: &[(&str, &str)]) -> String {
    String::from(d.to_locale_date_string("default", &opts(pairs)))
}

fn time_of(d: &js_sys::Date) -> String {
    String::from(d.to_locale_time_string_with_options("default", &opts(&[("hour", "numeric"), ("minute", "2-digit")])))
}

/// For lists, cards and rows: "3:41 PM", "Yesterday", "Wed, Oct 14",
/// "Oct 14, 2025".
pub fn compact(ms: i64) -> String {
    let d = date(ms);
    match near_now(&d) {
        Near::Today => time_of(&d),
        Near::Yesterday => "Yesterday".into(),
        Near::ThisYear => day_of(&d, &WEEKDAY),
        Near::OtherYear => day_of(&d, &[("month", "short"), ("day", "numeric"), ("year", "numeric")]),
    }
}

/// For what is open: "Today, 3:41 PM", "Yesterday, 3:41 PM", "Wed, Oct
/// 14, 3:41 PM", "Tue, Oct 14, 2025, 3:41 PM".
pub fn full(ms: i64) -> String {
    let d = date(ms);
    let at = time_of(&d);
    match near_now(&d) {
        Near::Today => format!("Today, {at}"),
        Near::Yesterday => format!("Yesterday, {at}"),
        Near::ThisYear => format!("{}, {at}", day_of(&d, &WEEKDAY)),
        Near::OtherYear => format!("{}, {at}", day_of(&d, &WEEKDAY_YEAR)),
    }
}

/// A reply's quote, as mail clients write it: "Wed, Oct 14, 2026 at 3:41 PM".
pub fn quote(ms: i64) -> String {
    let d = date(ms);
    format!("{} at {}", day_of(&d, &WEEKDAY_YEAR), time_of(&d))
}

/// Just the time: "3:41 PM" (a chat message's, under its day's heading).
pub fn time(ms: i64) -> String {
    time_of(&date(ms))
}

/// A day's heading: "Today", "Yesterday", "Monday, October 26" (with the
/// year when it isn't this one).
pub fn heading(ms: i64) -> String {
    let d = date(ms);
    match near_now(&d) {
        Near::Today => "Today".into(),
        Near::Yesterday => "Yesterday".into(),
        Near::ThisYear => day_of(&d, &[("weekday", "long"), ("month", "long"), ("day", "numeric")]),
        Near::OtherYear => day_of(&d, &[("weekday", "long"), ("month", "long"), ("day", "numeric"), ("year", "numeric")]),
    }
}

/// How long ago, for pages that show recency: "5s ago", "3m ago", "2h ago",
/// "4d ago".
pub fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::{Near, near};

    #[test]
    fn the_four_forms() {
        let today = (2026, 10, 27);
        assert_eq!(near((2026, 10, 27), today), Near::Today);
        assert_eq!(near((2026, 10, 26), today), Near::Yesterday);
        assert_eq!(near((2026, 10, 14), today), Near::ThisYear);
        assert_eq!(near((2026, 10, 28), today), Near::ThisYear, "ahead, this year");
        assert_eq!(near((2025, 10, 14), today), Near::OtherYear);
        // Across a month, a year and a leap day.
        assert_eq!(near((2026, 9, 30), (2026, 10, 1)), Near::Yesterday);
        assert_eq!(near((2025, 12, 31), (2026, 1, 1)), Near::Yesterday, "yesterday, though last year");
        assert_eq!(near((2028, 2, 29), (2028, 3, 1)), Near::Yesterday);
        assert_eq!(near((2027, 2, 28), (2027, 3, 1)), Near::Yesterday);
    }

    #[test]
    fn ago() {
        assert_eq!(super::ago(42_000), "42s ago");
        assert_eq!(super::ago(3 * 60_000), "3m ago");
        assert_eq!(super::ago(2 * 3_600_000), "2h ago");
        assert_eq!(super::ago(4 * 86_400_000), "4d ago");
    }
}
