//! Cron schedules: five fields (minute hour day-of-month month day-of-week)
//! with `*`, lists, ranges, steps and names (`mon`, `jan`), or `@hourly`,
//! `@daily`, `@weekly`, `@monthly`. As in cron, when both day fields are
//! restricted a day matches either.

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Timelike};

#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    minutes: Vec<bool>,
    hours: Vec<bool>,
    days: Vec<bool>,
    months: Vec<bool>,
    weekdays: Vec<bool>,
    days_any: bool,
    weekdays_any: bool,
}

fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<(Vec<bool>, bool), String> {
    let mut set = vec![false; max as usize + 1];
    let value = |s: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + min);
        }
        s.parse::<u32>().map_err(|_| format!("{s:?} is not a number"))
    };
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| format!("bad step in {part:?}"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("bad step in {part:?}"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            (v, if part.contains('/') { max } else { v })
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("{part:?} is out of range {min}-{max}"));
        }
        for v in (lo..=hi).step_by(step as usize) {
            set[v as usize] = true;
        }
    }
    Ok((set, text == "*"))
}

impl Schedule {
    pub fn parse(text: &str) -> Result<Schedule, String> {
        let expanded = match text.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            other => other,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        if f.len() != 5 {
            return Err("a schedule has five fields: minute hour day-of-month month day-of-week (e.g. 0 8 * * mon-fri)".into());
        }
        const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
        const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let (minutes, _) = field(f[0], 0, 59, &[])?;
        let (hours, _) = field(f[1], 0, 23, &[])?;
        let (days, days_any) = field(f[2], 1, 31, &[])?;
        let (months, _) = field(f[3], 1, 12, &MONTHS)?;
        let (mut weekdays, weekdays_any) = field(f[4], 0, 7, &DAYS)?;
        // 7 is Sunday too.
        if weekdays[7] {
            weekdays[0] = true;
        }
        Ok(Schedule { minutes, hours, days, months, weekdays, days_any, weekdays_any })
    }

    pub fn matches(&self, t: &DateTime<Local>) -> bool {
        let day = self.days[t.day() as usize];
        let weekday = self.weekdays[t.weekday().num_days_from_sunday() as usize];
        let day_ok = match (self.days_any, self.weekdays_any) {
            (true, true) => true,
            (false, true) => day,
            (true, false) => weekday,
            (false, false) => day || weekday,
        };
        self.minutes[t.minute() as usize] && self.hours[t.hour() as usize] && self.months[t.month() as usize] && day_ok
    }

    /// The next minute after `after` that matches (within a year).
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        // Absolute arithmetic: `with_minute` and friends fail on local times
        // that occur twice when the clocks go back.
        let mut t =
            after - Duration::seconds(after.second() as i64) - Duration::nanoseconds(after.nanosecond() as i64) + Duration::minutes(1);
        let end = after + Duration::days(366);
        // A year of whole days and hours is far fewer steps; the cap only
        // guards against a time zone surprise looping forever.
        for _ in 0..1_000_000 {
            if t >= end {
                break;
            }
            if !self.months[t.month() as usize] {
                // Skip to the next day (and on, quickly).
                t = next_day(t)?;
                continue;
            }
            if !self.hours[t.hour() as usize] {
                t += Duration::minutes(60 - t.minute() as i64);
                continue;
            }
            if self.matches(&t) {
                return Some(t);
            }
            t += Duration::minutes(1);
        }
        None
    }
}

/// The first moment of the next calendar day. Not `t + 1 day`: a day can
/// be 23 or 25 hours long where the clocks change.
fn next_day(t: DateTime<Local>) -> Option<DateTime<Local>> {
    let d = t.date_naive().succ_opt()?;
    // Midnight may not exist (clocks jumping at midnight): the earliest
    // time that does, within the first hours.
    (0..4).find_map(|h| Local.from_local_datetime(&d.and_time(NaiveTime::from_hms_opt(h, 0, 0)?)).earliest())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn schedules() {
        let weekdays = Schedule::parse("0 8 * * mon-fri").unwrap();
        // 2026-10-12 is a Monday.
        assert!(weekdays.matches(&at(2026, 10, 12, 8, 0)));
        assert!(!weekdays.matches(&at(2026, 10, 11, 8, 0)), "Sunday");
        assert!(!weekdays.matches(&at(2026, 10, 12, 8, 1)));
        assert_eq!(weekdays.next_after(at(2026, 10, 10, 9, 30)), Some(at(2026, 10, 12, 8, 0)));

        let every15 = Schedule::parse("*/15 * * * *").unwrap();
        assert_eq!(every15.next_after(at(2026, 10, 12, 8, 7)), Some(at(2026, 10, 12, 8, 15)));
        assert_eq!(Schedule::parse("@daily").unwrap().next_after(at(2026, 10, 12, 8, 7)), Some(at(2026, 10, 13, 0, 0)));
        assert!(Schedule::parse("0 9 1 * sun").unwrap().matches(&at(2026, 10, 11, 9, 0)), "either day field");
        assert!(Schedule::parse("30 6 * jan,jul 7").unwrap().matches(&at(2026, 7, 5, 6, 30)), "7 is Sunday");
        // Across the end of daylight saving time (in zones that have it).
        assert_eq!(Schedule::parse("0 0 1 1 *").unwrap().next_after(at(2026, 10, 20, 9, 0)), Some(at(2027, 1, 1, 0, 0)));
        assert_eq!(Schedule::parse("30 2 * * *").unwrap().next_after(at(2026, 10, 31, 23, 0)).map(|t| t.day()), Some(1));
        for bad in ["* * * *", "61 * * * *", "*/0 * * * *", "0 25 * * *", "0 8 * * funday"] {
            assert!(Schedule::parse(bad).is_err(), "{bad}");
        }
    }
}
