//! When the brief comes (docs/brief.md §2.3): at the realm's brief times a
//! note of what needs you; at the close of day what you handled and what's
//! still yours; urgent requests at once (requests.rs). Times are in your
//! primary calendar's time zone. A time missed by more than three hours
//! (the head was off) is let go.

use chrono::{NaiveTime, TimeZone};
use serde_json::json;
use tracing::warn;

use crate::Memex;

/// How late a brief may still go.
const LATE: i64 = 3 * 3_600_000;

/// Tells you something, linking to the brief.
pub async fn notify(m: &Memex, title: &str, message: &str, priority: &str) {
    let path = format!("/r/{}/brief", m.realm);
    let r = m.link.call("notify", json!({"title": title, "message": message, "priority": priority, "path": path})).await;
    if let Err(e) = r {
        warn!("telling you: {e:#}");
    }
}

async fn zone(m: &Memex) -> chrono_tz::Tz {
    let z: Option<String> = sqlx::query_scalar("SELECT time_zone FROM calendars WHERE is_primary = 1 LIMIT 1")
        .fetch_optional(&m.store.pool)
        .await
        .ok()
        .flatten();
    z.and_then(|z| z.parse().ok()).unwrap_or(chrono_tz::UTC)
}

/// Sends what's due at `now` (ms): the brief, the close of day. What it sent,
/// as (title, message).
pub async fn due(m: &Memex, now: i64) -> anyhow::Result<Vec<(String, String)>> {
    let setting = m.brief_setting.lock().unwrap().clone();
    let tz = zone(m).await;
    let Some(local) = chrono::DateTime::from_timestamp_millis(now).map(|d| d.with_timezone(&tz)) else { return Ok(Vec::new()) };
    let day = local.date_naive();
    let at = |hhmm: &str| -> Option<i64> {
        let t = NaiveTime::parse_from_str(hhmm, "%H:%M").ok()?;
        tz.from_local_datetime(&day.and_time(t)).earliest().map(|d| d.timestamp_millis())
    };
    let mut out = Vec::new();
    // The latest brief time passed (not too late), if not sent yet.
    let latest =
        setting.times.iter().filter_map(|t| at(t).map(|ms| (t.clone(), ms))).filter(|(_, ms)| *ms <= now).max_by_key(|(_, ms)| *ms);
    if let Some((t, ms)) = latest
        && now - ms < LATE
        && first(m, &format!("brief:{day} {t}")).await?
        && let Some(note) = brief(m).await?
    {
        notify(m, &note.0, &note.1, "normal").await;
        out.push(note);
    }
    if let Some(ms) = at(&setting.close)
        && ms <= now
        && now - ms < LATE
        && first(m, &format!("close:{day}")).await?
    {
        let since = tz.from_local_datetime(&day.and_time(NaiveTime::MIN)).earliest().map(|d| d.timestamp_millis()).unwrap_or(now);
        if let Some(note) = close(m, since).await? {
            notify(m, &note.0, &note.1, "low").await;
            out.push(note);
        }
    }
    Ok(out)
}

/// Whether this slot is new (and now taken).
async fn first(m: &Memex, slot: &str) -> anyhow::Result<bool> {
    let r = sqlx::query("INSERT OR IGNORE INTO brief_sent (slot, at) VALUES (?, ?)")
        .bind(slot)
        .bind(c6_core::now_ms())
        .execute(&m.store.pool)
        .await?;
    Ok(r.rows_affected() == 1)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The brief as a note: what needs you (the first few), first contacts,
/// what was set aside. None when nothing needs you.
async fn brief(m: &Memex) -> anyhow::Result<Option<(String, String)>> {
    let open = crate::requests::list(m, "open", 200).await?;
    let needs: Vec<_> = open.iter().filter(|r| r.category == "needs_you").collect();
    let firsts = open.iter().filter(|r| r.category == "first_contact").count();
    if needs.is_empty() && firsts == 0 {
        return Ok(None);
    }
    let today = c6_core::now_ms() + 86_400_000;
    let soon = needs.iter().filter(|r| r.due.is_some_and(|d| d <= today)).count();
    let mut title = plural(needs.len(), "request needs you", "requests need you");
    if soon > 0 {
        title.push_str(&format!(" ({soon} due by tomorrow)"));
    }
    let mut lines: Vec<String> = needs
        .iter()
        .take(5)
        .map(|r| format!("• {}: {}", if r.person_name.is_empty() { &r.person } else { &r.person_name }, r.title))
        .collect();
    if firsts > 0 {
        lines.push(plural(firsts, "first contact", "first contacts"));
    }
    let counts = crate::triage::counts(m).await?;
    let aside = ["pitch", "newsletter", "notification", "noise"].iter().map(|k| counts[*k].as_i64().unwrap_or(0)).sum::<i64>();
    if aside > 0 {
        lines.push(format!("{aside} set aside (cold pitches, newsletters, notifications)"));
    }
    Ok(Some((format!("Brief: {title}"), lines.join("\n"))))
}

/// The close of day: what you handled since `since`, and what's still yours.
async fn close(m: &Memex, since: i64) -> anyhow::Result<Option<(String, String)>> {
    let handled: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests WHERE status IN ('done', 'dropped') AND updated_at >= ?")
        .bind(since)
        .fetch_one(&m.store.pool)
        .await?;
    let open = crate::requests::list(m, "open", 200).await?;
    let still: Vec<_> = open.iter().filter(|r| r.category == "needs_you").collect();
    if handled == 0 && still.is_empty() {
        return Ok(None);
    }
    let mut lines = vec![format!("Handled {} today.", plural(handled as usize, "request", "requests"))];
    if still.is_empty() {
        lines.push("Nothing else needs you.".into());
    } else {
        lines.push(format!("Still yours: {}", plural(still.len(), "request", "requests")));
        for r in still.iter().take(3) {
            let draft = if r.draft.as_ref().is_some_and(|d| d.error.is_none()) { " (draft ready)" } else { "" };
            lines.push(format!("• {}: {}{draft}", if r.person_name.is_empty() { &r.person } else { &r.person_name }, r.title));
        }
    }
    Ok(Some(("Close of day".into(), lines.join("\n"))))
}

/// Looks every minute.
pub fn spawn(m: std::sync::Arc<Memex>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            if m.link.is_connected()
                && let Err(e) = due(&m, c6_core::now_ms()).await
            {
                warn!("the brief: {e:#}");
            }
        }
    });
}
