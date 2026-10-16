//! Google Calendar: every calendar of every account, synced with sync
//! tokens; creating, changing ("this one", "this and following", "all")
//! and deleting events; RSVPs. Calendars and events go to the knowledge
//! store as Google gives them.

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};

use crate::{Account, Run, enc, status_of};

fn keep_calendar(r: &mut Run, a: &Account, c: &Value) {
    let id = c["id"].as_str().unwrap_or_default();
    r.json_item("application/vnd.c6.calendar+json", &format!("{}:{id}", a.id), json!({"account": a.id, "email": a.email, "calendar": c}));
}

fn keep_event(r: &mut Run, a: &Account, cal: &str, e: &Value) {
    let id = e["id"].as_str().unwrap_or_default();
    r.json_item(
        "application/vnd.c6.event+json",
        &format!("{}:{cal}:{id}", a.id),
        json!({"account": a.id, "email": a.email, "calendar": cal, "event": e}),
    );
}

/// Every account's calendars and their events (what changed).
pub async fn sync(r: &mut Run) -> anyhow::Result<()> {
    for a in r.accounts.clone() {
        let list = match r.get(&a, "/calendar/v3/users/me/calendarList", &[]).await {
            Ok(l) => l,
            Err(e) => {
                r.errors.push(format!("{}: calendars: {e:#}", a.email));
                continue;
            }
        };
        for c in list["items"].as_array().cloned().unwrap_or_default() {
            keep_calendar(r, &a, &c);
            let id = c["id"].as_str().unwrap_or_default().to_owned();
            let key = format!("cal:{}:{id}", a.id);
            let mut st = r.sync(&key);
            let error = events(r, &a, &id, &mut st).await.err().map(|e| format!("{e:#}"));
            if let Some(e) = &error {
                r.errors.push(format!("{}: calendar {id}: {e}", a.email));
            }
            r.save(&key, st, error);
        }
    }
    Ok(())
}

async fn events(r: &mut Run, a: &Account, cal: &str, st: &mut Value) -> anyhow::Result<()> {
    let path = format!("/calendar/v3/calendars/{}/events", enc(cal));
    let mut page: Option<String> = None;
    loop {
        let mut q = vec![("showDeleted", "true".to_owned()), ("maxResults", "250".to_owned())];
        if let Some(t) = st["cursor"].as_str() {
            q.push(("syncToken", t.to_owned()));
        }
        if let Some(p) = &page {
            q.push(("pageToken", p.clone()));
        }
        let v = match r.get(a, &path, &q).await {
            Ok(v) => v,
            Err(e) if status_of(&e) == Some(410) => {
                *st = json!({"phase": "import", "done": 0});
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for e in v["items"].as_array().into_iter().flatten() {
            keep_event(r, a, cal, e);
            st["done"] = json!(st["done"].as_i64().unwrap_or(0) + 1);
        }
        page = v["nextPageToken"].as_str().map(str::to_owned);
        if page.is_none() {
            if let Some(t) = v["nextSyncToken"].as_str() {
                st["cursor"] = json!(t);
            }
            st["phase"] = json!("live");
            return Ok(());
        }
    }
}

fn time_json(ms: i64, all_day: bool, tz: &str) -> Value {
    let t = Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now);
    if all_day {
        json!({"date": t.format("%Y-%m-%d").to_string()})
    } else {
        let mut v = json!({"dateTime": t.to_rfc3339()});
        if !tz.is_empty() {
            v["timeZone"] = json!(tz);
        }
        v
    }
}

/// RFC 3339 (or a date, for all-day events) to ms.
fn parse_time(v: &Value) -> Option<(i64, bool)> {
    if let Some(dt) = v["dateTime"].as_str() {
        return chrono::DateTime::parse_from_rfc3339(dt).ok().map(|d| (d.timestamp_millis(), false));
    }
    let d = chrono::NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    Some((Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?).timestamp_millis(), true))
}

/// The fields of an edit (docs: api::EventEdit) as Google's event.
fn event_body(e: &Value, all_day: bool, tz: &str) -> Value {
    let mut v = json!({});
    for k in ["summary", "description", "location"] {
        if let Some(s) = e[k].as_str() {
            v[k] = json!(s);
        }
    }
    if let (Some(s), Some(en)) = (e["start"].as_i64(), e["end"].as_i64()) {
        v["start"] = time_json(s, all_day, tz);
        v["end"] = time_json(en, all_day, tz);
    }
    if let Some(a) = e["attendees"].as_array() {
        v["attendees"] = json!(a.iter().map(|x| json!({"email": x})).collect::<Vec<_>>());
    }
    if let Some(r) = e["recurrence"].as_array() {
        v["recurrence"] = json!(r);
    }
    v
}

/// Google's id for an instance of a recurring event.
pub fn instance_id(id: &str, start: i64, all_day: bool) -> String {
    let t = Utc.timestamp_millis_opt(start).single().unwrap_or_else(Utc::now);
    if all_day { format!("{id}_{}", t.format("%Y%m%d")) } else { format!("{id}_{}", t.format("%Y%m%dT%H%M%SZ")) }
}

pub fn with_until(rule: &str, until: &str) -> String {
    let parts: Vec<&str> =
        rule.trim_start_matches("RRULE:").split(';').filter(|p| !p.starts_with("UNTIL=") && !p.starts_with("COUNT=")).collect();
    format!("RRULE:{};UNTIL={until}", parts.join(";"))
}

fn without_count_until(rule: &str) -> String {
    let parts: Vec<&str> =
        rule.trim_start_matches("RRULE:").split(';').filter(|p| !p.starts_with("UNTIL=") && !p.starts_with("COUNT=")).collect();
    format!("RRULE:{}", parts.join(";"))
}

/// The event as Google has it now.
async fn master(r: &Run, a: &Account, base: &str, id: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(!id.is_empty(), "which event?");
    r.get(a, &format!("{base}/{}", enc(id)), &[]).await
}

/// `calendar-write`: `{action: create | update | delete | rsvp, edit}`,
/// where `edit` is what the calendar asks for (account, calendar_id,
/// event_id, instance_start, scope, the fields; for an RSVP, response).
/// What changed is kept; `result.json` names the event.
pub async fn write(r: &mut Run) -> anyhow::Result<()> {
    let e = r.input["edit"].clone();
    let a = r.account(e["account"].as_str().unwrap_or_default())?;
    let cal = e["calendar_id"].as_str().filter(|c| !c.is_empty()).unwrap_or("primary").to_owned();
    let tz = r
        .get(&a, &format!("/calendar/v3/users/me/calendarList/{}", enc(&cal)), &[])
        .await
        .ok()
        .and_then(|c| c["timeZone"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "UTC".into());
    let base = format!("/calendar/v3/calendars/{}/events", enc(&cal));
    let send = [("sendUpdates", "all".to_owned())];
    let id = e["event_id"].as_str().unwrap_or_default().to_owned();
    let scope = e["scope"].as_str().unwrap_or("all");
    let at = e["instance_start"].as_i64();
    use reqwest::Method as M;
    match r.input["action"].as_str().unwrap_or_default() {
        "create" => {
            anyhow::ensure!(
                e["start"].is_i64() && e["end"].is_i64() && e["summary"].is_string(),
                "an event needs a title, a start and an end"
            );
            let all_day = e["all_day"].as_bool().unwrap_or(false);
            let v = r.call(&a, M::POST, &base, &send, Some(&event_body(&e, all_day, &tz))).await?;
            keep_event(r, &a, &cal, &v);
            r.result = json!({"account": a.id, "calendar_id": cal, "event_id": v["id"]});
        }
        "update" => {
            let m = master(r, &a, &base, &id).await?;
            let all_day = parse_time(&m["start"]).is_some_and(|(_, d)| d);
            match (scope, at) {
                ("this", Some(at)) => {
                    let iid = instance_id(&id, at, all_day);
                    let v = r.call(&a, M::PATCH, &format!("{base}/{}", enc(&iid)), &send, Some(&event_body(&e, all_day, &tz))).await?;
                    keep_event(r, &a, &cal, &v);
                }
                ("following", Some(at)) => {
                    // End the series before this instance; start a new one here.
                    let lines: Vec<String> = serde_json::from_value(m["recurrence"].clone()).unwrap_or_default();
                    let until = Utc.timestamp_millis_opt(at - 1000).single().unwrap_or_else(Utc::now).format("%Y%m%dT%H%M%SZ").to_string();
                    let ended: Vec<String> =
                        lines.iter().map(|l| if l.starts_with("RRULE:") { with_until(l, &until) } else { l.clone() }).collect();
                    let v = r.call(&a, M::PATCH, &format!("{base}/{}", enc(&id)), &send, Some(&json!({"recurrence": ended}))).await?;
                    keep_event(r, &a, &cal, &v);
                    let (start, end) = (parse_time(&m["start"]).map_or(at, |t| t.0), parse_time(&m["end"]).map_or(at, |t| t.0));
                    let mut next = e.clone();
                    next["start"] = json!(e["start"].as_i64().unwrap_or(at));
                    next["end"] = json!(e["end"].as_i64().unwrap_or(at + (end - start)));
                    if !next["summary"].is_string() {
                        next["summary"] = m["summary"].clone();
                    }
                    next["recurrence"] = json!(
                        lines.iter().map(|l| if l.starts_with("RRULE:") { without_count_until(l) } else { l.clone() }).collect::<Vec<_>>()
                    );
                    let v = r.call(&a, M::POST, &base, &send, Some(&event_body(&next, all_day, &tz))).await?;
                    keep_event(r, &a, &cal, &v);
                }
                _ => {
                    let v = r.call(&a, M::PATCH, &format!("{base}/{}", enc(&id)), &send, Some(&event_body(&e, all_day, &tz))).await?;
                    keep_event(r, &a, &cal, &v);
                }
            }
            r.result = json!({"account": a.id, "calendar_id": cal, "event_id": id});
        }
        "delete" => {
            match (scope, at) {
                ("this", Some(at)) => {
                    let m = master(r, &a, &base, &id).await?;
                    let all_day = parse_time(&m["start"]).is_some_and(|(_, d)| d);
                    let iid = instance_id(&id, at, all_day);
                    r.call(&a, M::DELETE, &format!("{base}/{}", enc(&iid)), &send, None).await?;
                    // The instance stays, cancelled, to hide it.
                    let t = time_json(at, all_day, "");
                    keep_event(
                        r,
                        &a,
                        &cal,
                        &json!({"id": iid, "status": "cancelled", "recurringEventId": id, "originalStartTime": t, "start": t, "end": t}),
                    );
                }
                ("following", Some(at)) => {
                    let m = master(r, &a, &base, &id).await?;
                    let lines: Vec<String> = serde_json::from_value(m["recurrence"].clone()).unwrap_or_default();
                    let until = Utc.timestamp_millis_opt(at - 1000).single().unwrap_or_else(Utc::now).format("%Y%m%dT%H%M%SZ").to_string();
                    let ended: Vec<String> =
                        lines.iter().map(|l| if l.starts_with("RRULE:") { with_until(l, &until) } else { l.clone() }).collect();
                    let v = r.call(&a, M::PATCH, &format!("{base}/{}", enc(&id)), &[], Some(&json!({"recurrence": ended}))).await?;
                    keep_event(r, &a, &cal, &v);
                }
                _ => {
                    anyhow::ensure!(!id.is_empty(), "which event?");
                    r.call(&a, M::DELETE, &format!("{base}/{}", enc(&id)), &send, None).await?;
                    keep_event(r, &a, &cal, &json!({"id": id, "status": "cancelled"}));
                }
            }
            r.result = json!({"account": a.id, "calendar_id": cal, "event_id": id});
        }
        "rsvp" => {
            let response = e["response"].as_str().unwrap_or_default();
            anyhow::ensure!(matches!(response, "accepted" | "declined" | "tentative"), "the answer is accepted, declined or tentative");
            let m = master(r, &a, &base, &id).await?;
            let mut attendees: Vec<Value> = serde_json::from_value(m["attendees"].clone()).unwrap_or_default();
            let mine = a.email.to_lowercase();
            let mut found = false;
            for x in attendees.iter_mut() {
                if x["self"].as_bool() == Some(true) || x["email"].as_str().map(str::to_lowercase).as_deref() == Some(mine.as_str()) {
                    x["responseStatus"] = json!(response);
                    found = true;
                }
            }
            anyhow::ensure!(found, "you are not invited to this event");
            let all_day = parse_time(&m["start"]).is_some_and(|(_, d)| d);
            let target = match at {
                Some(at) => instance_id(&id, at, all_day),
                None => id.clone(),
            };
            let v = r.call(&a, M::PATCH, &format!("{base}/{}", enc(&target)), &send, Some(&json!({"attendees": attendees}))).await?;
            keep_event(r, &a, &cal, &v);
            r.result = json!({"account": a.id, "calendar_id": cal, "event_id": id});
        }
        other => anyhow::bail!("no action {other:?} (create, update, delete or rsvp)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_and_instances() {
        assert_eq!(
            with_until("RRULE:FREQ=WEEKLY;COUNT=10;BYDAY=MO", "20261101T000000Z"),
            "RRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20261101T000000Z"
        );
        assert_eq!(without_count_until("RRULE:FREQ=DAILY;COUNT=3"), "RRULE:FREQ=DAILY");
        let start = Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap().timestamp_millis();
        assert_eq!(instance_id("abc", start, false), "abc_20261012T090000Z");
        assert_eq!(instance_id("abc", start, true), "abc_20261012");
    }
}
