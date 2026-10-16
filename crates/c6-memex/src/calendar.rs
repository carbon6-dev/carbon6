//! The calendar store: every calendar of the calendar apps' accounts, as
//! they sync them (Google's shapes: events with their recurrence rules and
//! changed or cancelled instances); recurring events expanded for the
//! views here; free/busy. Creating, changing ("this one", "this and
//! following", "all"), deleting and RSVPs go through the app that owns
//! the account (docs/knowledge.md §6).

use c6_api as api;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;
use crate::people;
use crate::store::clip;

/// RFC 3339 (or a date, for all-day events) to ms.
fn parse_time(v: &Value) -> Option<(i64, bool)> {
    if let Some(dt) = v["dateTime"].as_str() {
        return DateTime::parse_from_rfc3339(dt).ok().map(|d| (d.timestamp_millis(), false));
    }
    let d = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    Some((Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?).timestamp_millis(), true))
}

/// Keeps a calendar an app syncs (whether it is shown stays yours).
pub async fn store_calendar(m: &Memex, account: &str, c: &Value) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO calendars (account, id, summary, color, is_primary, access, selected, time_zone) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(account, id) DO UPDATE SET summary = excluded.summary, color = excluded.color, is_primary = excluded.is_primary, \
         access = excluded.access, time_zone = excluded.time_zone",
    )
    .bind(account)
    .bind(c["id"].as_str().unwrap_or_default())
    .bind(c["summaryOverride"].as_str().or(c["summary"].as_str()).unwrap_or_default())
    .bind(c["backgroundColor"].as_str().unwrap_or(""))
    .bind(c["primary"].as_bool().unwrap_or(false) as i64)
    .bind(c["accessRole"].as_str().unwrap_or(""))
    .bind(c["selected"].as_bool().unwrap_or(true) as i64)
    .bind(c["timeZone"].as_str().unwrap_or("UTC"))
    .execute(&m.store.pool)
    .await?;
    Ok(())
}

/// Stores (or removes) an event an app synced, and its item: what, when,
/// who. `source` is the app's (`app:<name>`).
pub async fn store_event(m: &Memex, source: &str, account: &str, email: &str, cal: &str, e: &Value) -> anyhow::Result<()> {
    let id = e["id"].as_str().unwrap_or_default();
    let status = e["status"].as_str().unwrap_or("confirmed");
    let existing: Option<Option<String>> =
        sqlx::query_scalar("SELECT item_id FROM events WHERE account = ? AND calendar_id = ? AND event_id = ?")
            .bind(account)
            .bind(cal)
            .bind(id)
            .fetch_optional(&m.store.pool)
            .await?;
    // A cancelled instance of a recurring event stays, to hide that
    // instance; a cancelled event goes.
    if status == "cancelled" && e["recurringEventId"].is_null() {
        if let Some(Some(item)) = &existing {
            m.store.delete(item).await?;
        }
        sqlx::query("DELETE FROM events WHERE account = ? AND calendar_id = ? AND event_id = ?")
            .bind(account)
            .bind(cal)
            .bind(id)
            .execute(&m.store.pool)
            .await?;
        return Ok(());
    }
    let (start, all_day) = parse_time(&e["start"]).unwrap_or((0, false));
    let (end, _) = parse_time(&e["end"]).unwrap_or((start, all_day));
    let original = parse_time(&e["originalStartTime"]).map(|(t, _)| t);
    let attendees = e["attendees"].as_array().cloned().unwrap_or_default();
    let my_status = attendees.iter().find(|a| a["self"].as_bool() == Some(true)).and_then(|a| a["responseStatus"].as_str()).unwrap_or("");
    let summary = e["summary"].as_str().unwrap_or("(no title)").to_owned();
    let conference = e["hangoutLink"].as_str().or(e["conferenceData"]["entryPoints"][0]["uri"].as_str()).unwrap_or("");
    // The item (not for cancelled instances): searchable, linked to people.
    let item = if status == "cancelled" {
        None
    } else {
        let who: Vec<String> = attendees
            .iter()
            .filter_map(|a| a["email"].as_str().map(|x| format!("{} <{x}>", a["displayName"].as_str().unwrap_or(""))))
            .collect();
        let it = api::MemexItemIn {
            kind: "event".into(),
            source: source.to_owned(),
            external_id: format!("{account}:{cal}:{id}"),
            title: summary.clone(),
            text: format!(
                "{}\n{}{}\n{}",
                summary,
                if e["location"].is_string() { format!("Where: {}\n", e["location"].as_str().unwrap_or("")) } else { String::new() },
                if who.is_empty() { String::new() } else { format!("Attendees: {}", who.join(", ")) },
                clip(e["description"].as_str().unwrap_or(""), 20_000)
            ),
            created_at: Some(start),
            meta: json!({"account": email, "calendar": cal, "event_id": id, "end": end, "all_day": all_day,
                         "location": e["location"].as_str().unwrap_or_default()}),
            ..Default::default()
        };
        let item = m.store.put(&it, &it.source).await?;
        m.store.clear_links(&item, "calendar").await?;
        for a in &attendees {
            if let Some(addr) = a["email"].as_str() {
                let p = people::person(&m.store, addr, a["displayName"].as_str().unwrap_or("")).await?;
                m.store.link(&item, "attendee", &p, "observed", "calendar", None).await?;
            }
        }
        Some(item)
    };
    sqlx::query(
        "INSERT INTO events (account, calendar_id, event_id, status, summary, description, location, start, end, all_day, time_zone, recurrence, \
         recurring_event_id, original_start, attendees, organizer, my_status, transparent, html_link, conference, updated, item_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account, calendar_id, event_id) DO UPDATE SET \
         status = excluded.status, summary = excluded.summary, description = excluded.description, location = excluded.location, start = excluded.start, \
         end = excluded.end, all_day = excluded.all_day, time_zone = excluded.time_zone, recurrence = excluded.recurrence, \
         recurring_event_id = excluded.recurring_event_id, original_start = excluded.original_start, attendees = excluded.attendees, \
         organizer = excluded.organizer, my_status = excluded.my_status, transparent = excluded.transparent, html_link = excluded.html_link, \
         conference = excluded.conference, updated = excluded.updated, item_id = excluded.item_id",
    )
    .bind(account)
    .bind(cal)
    .bind(id)
    .bind(status)
    .bind(&summary)
    .bind(e["description"].as_str().unwrap_or(""))
    .bind(e["location"].as_str().unwrap_or(""))
    .bind(start)
    .bind(end)
    .bind(all_day as i64)
    .bind(e["start"]["timeZone"].as_str().unwrap_or(""))
    .bind(e["recurrence"].to_string().replace("null", "[]"))
    .bind(e["recurringEventId"].as_str())
    .bind(original)
    .bind(Value::Array(attendees.clone()).to_string())
    .bind(e["organizer"]["email"].as_str().unwrap_or(""))
    .bind(my_status)
    .bind((e["transparency"].as_str() == Some("transparent")) as i64)
    .bind(e["htmlLink"].as_str().unwrap_or(""))
    .bind(conference)
    .bind(e["updated"].as_str().and_then(|u| DateTime::parse_from_rfc3339(u).ok()).map(|d| d.timestamp_millis()).unwrap_or(0))
    .bind(item)
    .execute(&m.store.pool)
    .await?;
    Ok(())
}

fn row_event(r: &sqlx::sqlite::SqliteRow, start: i64, end: i64, instance: bool) -> api::CalendarEvent {
    api::CalendarEvent {
        account: r.get("account"),
        calendar_id: r.get("calendar_id"),
        event_id: r.get("event_id"),
        instance_start: instance.then_some(start),
        summary: r.get("summary"),
        description: r.get("description"),
        location: r.get("location"),
        start,
        end,
        all_day: r.get::<i64, _>("all_day") != 0,
        recurring: instance || r.get::<String, _>("recurrence") != "[]" || r.get::<Option<String>, _>("recurring_event_id").is_some(),
        attendees: serde_json::from_str::<Vec<Value>>(&r.get::<String, _>("attendees"))
            .unwrap_or_default()
            .iter()
            .map(|a| api::Attendee {
                email: a["email"].as_str().unwrap_or("").into(),
                name: a["displayName"].as_str().unwrap_or("").into(),
                response: a["responseStatus"].as_str().unwrap_or("").into(),
                me: a["self"].as_bool().unwrap_or(false),
            })
            .collect(),
        my_status: r.get("my_status"),
        conference: r.get("conference"),
        html_link: r.get("html_link"),
        busy: r.get::<i64, _>("transparent") == 0 && r.get::<String, _>("my_status") != "declined",
    }
}

/// Events overlapping [from, to), recurring ones expanded, sorted.
pub async fn events(m: &Memex, from: i64, to: i64) -> anyhow::Result<Vec<api::CalendarEvent>> {
    let mut out = Vec::new();
    let selected = "(SELECT 1 FROM calendars c WHERE c.account = e.account AND c.id = e.calendar_id AND c.selected = 1)";
    // Single events and changed instances.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT * FROM events e WHERE status != 'cancelled' AND recurrence = '[]' AND start < ? AND end > ? AND EXISTS {selected}"
    )))
    .bind(to)
    .bind(from)
    .fetch_all(&m.store.pool)
    .await?;
    for r in &rows {
        out.push(row_event(r, r.get("start"), r.get("end"), false));
    }
    // Recurring series, expanded, minus their changed or cancelled
    // instances.
    let masters = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT * FROM events e WHERE status != 'cancelled' AND recurrence != '[]' AND start < ? AND EXISTS {selected}"
    )))
    .bind(to)
    .fetch_all(&m.store.pool)
    .await?;
    for r in &masters {
        let (acct, cal, id): (String, String, String) = (r.get("account"), r.get("calendar_id"), r.get("event_id"));
        let exceptions: Vec<i64> = sqlx::query_scalar("SELECT original_start FROM events WHERE account = ? AND calendar_id = ? AND recurring_event_id = ? AND original_start IS NOT NULL")
            .bind(&acct)
            .bind(&cal)
            .bind(&id)
            .fetch_all(&m.store.pool)
            .await?;
        let (start, end): (i64, i64) = (r.get("start"), r.get("end"));
        let lines: Vec<String> = serde_json::from_str(&r.get::<String, _>("recurrence")).unwrap_or_default();
        let tz: String = r.get("time_zone");
        for s in expand(start, &tz, &lines, from - (end - start), to)? {
            if !exceptions.contains(&s) {
                out.push(row_event(r, s, s + (end - start), true));
            }
        }
    }
    out.sort_by_key(|e| e.start);
    Ok(out)
}

/// The starts of a recurring event within [from, to), from its first
/// start, time zone and RRULE/EXDATE/RDATE lines.
pub fn expand(start: i64, tz: &str, lines: &[String], from: i64, to: i64) -> anyhow::Result<Vec<i64>> {
    let tz: rrule::Tz = tz.parse::<chrono_tz::Tz>().map(rrule::Tz::Tz).unwrap_or(rrule::Tz::UTC);
    let first = tz.timestamp_millis_opt(start).single().ok_or_else(|| anyhow::anyhow!("bad start"))?;
    let text = format!("DTSTART;TZID={}:{}\n{}", tz.name(), first.format("%Y%m%dT%H%M%S"), lines.join("\n"));
    let set: rrule::RRuleSet = text.parse().map_err(|e| anyhow::anyhow!("recurrence {lines:?}: {e}"))?;
    let after = tz.timestamp_millis_opt(from.max(start) - 1).single().ok_or_else(|| anyhow::anyhow!("bad range"))?;
    let before = tz.timestamp_millis_opt(to).single().ok_or_else(|| anyhow::anyhow!("bad range"))?;
    let r = set.after(after).before(before).all(2000);
    Ok(r.dates.iter().map(|d| d.timestamp_millis()).filter(|t| *t >= from.min(start).max(start) && *t < to).collect())
}

/// Times you are busy in [from, to): merged intervals.
pub async fn busy(m: &Memex, from: i64, to: i64) -> anyhow::Result<Vec<(i64, i64)>> {
    let mut spans: Vec<(i64, i64)> =
        events(m, from, to).await?.into_iter().filter(|e| e.busy && !e.all_day).map(|e| (e.start.max(from), e.end.min(to))).collect();
    spans.sort();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (s, e) in spans {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    Ok(out)
}

pub async fn calendars(m: &Memex) -> anyhow::Result<Vec<api::Calendar>> {
    let rows = sqlx::query("SELECT * FROM calendars ORDER BY is_primary DESC, summary").fetch_all(&m.store.pool).await?;
    Ok(rows
        .iter()
        .map(|r| api::Calendar {
            account: r.get("account"),
            id: r.get("id"),
            summary: r.get("summary"),
            color: r.get("color"),
            primary: r.get::<i64, _>("is_primary") != 0,
            writable: matches!(r.get::<String, _>("access").as_str(), "owner" | "writer"),
            selected: r.get::<i64, _>("selected") != 0,
        })
        .collect())
}

pub async fn select(m: &Memex, account: &str, id: &str, on: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE calendars SET selected = ? WHERE account = ? AND id = ?")
        .bind(on as i64)
        .bind(account)
        .bind(id)
        .execute(&m.store.pool)
        .await?;
    Ok(())
}

/// Creates, changes, deletes or answers an event through the calendar
/// app that owns the account; what changed is kept as it syncs back. A
/// new event comes back.
pub async fn write(m: &Memex, action: &str, params: Value, by: &str) -> anyhow::Result<Value> {
    let mut e: api::EventEdit = if action == "rsvp" {
        api::EventEdit {
            account: params["account"].as_str().unwrap_or_default().to_owned(),
            calendar_id: params["calendar_id"].as_str().map(str::to_owned),
            event_id: params["event_id"].as_str().map(str::to_owned),
            instance_start: params["instance_start"].as_i64(),
            ..Default::default()
        }
    } else {
        serde_json::from_value(params.clone())?
    };
    let a = m.account("calendar", &e.account)?;
    e.account = a.id.clone();
    let mut edit = json!(e);
    if action == "rsvp" {
        edit["response"] = params["response"].clone();
    }
    let v = m.app_call(&a, action, json!({"edit": edit}), by).await?;
    if action == "create" {
        let r = &v["result"];
        let e = one(m, &a.id, r["calendar_id"].as_str().unwrap_or("primary"), r["event_id"].as_str().unwrap_or_default()).await?;
        return Ok(json!(e));
    }
    Ok(json!({}))
}

async fn one(m: &Memex, account: &str, cal: &str, id: &str) -> anyhow::Result<api::CalendarEvent> {
    let r = sqlx::query("SELECT * FROM events WHERE account = ? AND calendar_id = ? AND event_id = ?")
        .bind(account)
        .bind(cal)
        .bind(id)
        .fetch_one(&m.store.pool)
        .await?;
    Ok(row_event(&r, r.get("start"), r.get("end"), false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_expansion() {
        // Mondays 09:00 UTC from 2026-10-12, four times, except the second.
        let start = Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap().timestamp_millis();
        let day = 86_400_000;
        let lines = vec!["RRULE:FREQ=WEEKLY;COUNT=4".to_owned(), "EXDATE;TZID=UTC:20261019T090000".to_owned()];
        let got = expand(start, "UTC", &lines, start, start + 60 * day).unwrap();
        assert_eq!(got, vec![start, start + 14 * day, start + 21 * day]);
        // In a zone: the local time stays across the end of daylight saving.
        let ny = Utc.with_ymd_and_hms(2026, 10, 26, 13, 0, 0).unwrap().timestamp_millis(); // 09:00 EDT
        let got = expand(ny, "America/New_York", &["RRULE:FREQ=WEEKLY;COUNT=2".to_owned()], ny, ny + 30 * day).unwrap();
        assert_eq!(got[1] - got[0], 7 * day + 3_600_000, "09:00 EST is an hour later in UTC");
    }
}
