//! Drafts (docs/brief.md §6): for every open request that needs you, a
//! reply waits before you open it. The model reads the request's
//! conversations, what else you and that person have said lately, what's
//! open between you, when you're free, and how you write; it answers where
//! the request's substance is, and leaves what only you can decide as
//! choices (`[[one | other]]`). Drafted again when a conversation moves on.
//! Sending is yours: from the brief, through the mail or Slack app (mail
//! with its undo window).

use c6_api as api;
use c6_llm::Block;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use tracing::warn;

use crate::Memex;
use crate::llm;
use crate::store::clip;

const MAX_CONTEXT: usize = 16_000;

/// Starts a draft owned by the memex, returning before the model answers.
/// The same claim covers automatic drafts and repeated clicks.
pub async fn start(m: &Memex, id: i64) -> anyhow::Result<api::Request> {
    let claim = m.claim(&format!("draft:{id}"));
    let mut r = crate::requests::get(m, id).await?;
    let Some(claim) = claim else { return Ok(r) };
    let (task, _) = m.drafting().ok_or_else(|| anyhow::anyhow!("choose an understanding or drafting model first (Settings → Memex)"))?;
    let m = m.this.upgrade().ok_or_else(|| anyhow::anyhow!("the memex is stopping"))?;
    let priority = c6_llm::pool::Priority::current();
    tokio::spawn(priority.scope(async move {
        let _claim = claim;
        // Deleting a realm closes its store. Stop before doing more work,
        // including when a model slot or its first response is still awaited.
        tokio::select! {
            biased;
            () = m.store.pool.close_event() => {}
            () = async {
                if let Err(e) = draft(&m, &task, id).await {
                    warn!(id, "drafting: {e:#}");
                    if let Err(store) = failed(&m, id, &e).await {
                        warn!(id, "keeping the draft's error: {store:#}");
                    }
                }
            } => {}
        }
    }));
    r.drafting = true;
    Ok(r)
}

async fn failed(m: &Memex, id: i64, error: &anyhow::Error) -> anyhow::Result<()> {
    let mut d = crate::requests::get(m, id).await?.draft.unwrap_or_default();
    d.error = Some(format!("{error:#}"));
    sqlx::query("UPDATE requests SET draft = ?, drafted_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&d)?)
        .bind(c6_core::now_ms())
        .bind(id)
        .execute(&m.store.pool)
        .await?;
    Ok(())
}

/// Writes one more draft, if a request needs one and there's a model and
/// budget. Whether it did.
pub async fn step(m: &Memex) -> anyhow::Result<bool> {
    let Some((task, per_day)) = m.drafting() else { return Ok(false) };
    if !crate::understand::can_call(m, &task, per_day).await? {
        return Ok(false);
    }
    // Needs you, open, never drafted or drafted before its news; the soonest
    // due first, then the newest.
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT r.id FROM requests r WHERE r.category = 'needs_you' AND (r.status = 'open' OR (r.status = 'snoozed' AND r.until <= ?1)) \
         AND (r.drafted_at IS NULL OR r.drafted_at < (SELECT MAX(noted_at) FROM request_sources s WHERE s.request_id = r.id)) \
         ORDER BY r.due IS NULL, r.due, r.updated_at DESC LIMIT 10",
    )
    .bind(c6_core::now_ms())
    .fetch_all(&m.store.pool)
    .await?;
    // The first no other step is drafting.
    let Some((id, _claim)) = ids.into_iter().find_map(|id| m.claim(&format!("draft:{id}")).map(|c| (id, c))) else { return Ok(false) };
    if let Err(e) = draft(m, &task, id).await {
        if llm::waits(m, &e).await? {
            warn!(id, "drafting waits: {e:#}");
            return Ok(false);
        }
        warn!(id, "drafting: {e:#}");
        // Not again until the conversation has news.
        failed(m, id, &e).await?;
    }
    Ok(true)
}

/// Drafts (again) a request's reply now.
pub async fn draft(m: &Memex, task: &llm::Task, id: i64) -> anyhow::Result<api::RequestDraft> {
    let r = crate::requests::get(m, id).await?;
    let context = context(m, &r).await?;
    let template = m.prompt("memex.requests.draft", api::prompts::REQUEST_DRAFT);
    let request = format!(
        "{} (from {} <{}>{})",
        r.title,
        r.person_name,
        r.person,
        r.due.and_then(chrono::DateTime::from_timestamp_millis).map(|d| format!(", by {}", d.format("%Y-%m-%d"))).unwrap_or_default()
    );
    let prompt = api::prompts::fill(&template, &[("request", &request), ("context", &clip(&context, MAX_CONTEXT))]);
    let system = m.prompt("memex.understand.system", api::prompts::UNDERSTAND_SYSTEM);
    crate::understand::count_call(m).await?;
    let v = llm::json_of(&llm::ask(m, task, "draft", &r.title, &system, vec![Block::Text(prompt)], 1500).await?)?;
    let text = v["text"].as_str().unwrap_or("").trim().to_owned();
    anyhow::ensure!(!text.is_empty(), "the model wrote no draft");
    // Where to answer: one of its conversations (the latest, if the model
    // named none of them).
    let target = v["target"]
        .as_str()
        .filter(|t| r.sources.iter().any(|s| s.target == *t))
        .map(str::to_owned)
        .or_else(|| r.sources.last().map(|s| s.target.clone()))
        .unwrap_or_default();
    let kind = if target.starts_with("chat:") { "chat" } else { "mail" };
    let d = api::RequestDraft {
        target,
        kind: kind.into(),
        text,
        note: v["note"].as_str().unwrap_or("").trim().to_owned(),
        error: None,
        at: c6_core::now_ms(),
        by: String::new(),
    };
    sqlx::query("UPDATE requests SET draft = ?, drafted_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&d)?)
        .bind(d.at)
        .bind(id)
        .execute(&m.store.pool)
        .await?;
    Ok(d)
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
}

/// What the model reads to draft: the conversations, recent exchanges with
/// the person, what's open between you, when you're free, your voice.
pub(crate) async fn context(m: &Memex, r: &api::Request) -> anyhow::Result<String> {
    let mut out = String::from("## The request's conversations (answer in one of them, by its target)\n");
    for s in &r.sources {
        out.push_str(&format!("\n### target: {} ({})\n", s.target, s.kind));
        out.push_str(&conversation(m, &s.target).await?);
    }
    let person = r.person.to_lowercase();
    if person.contains('@') {
        // Who they are to the owner.
        let cards = crate::cards::of(m, &person).await?;
        if !cards.is_empty() {
            out.push_str("\n## Who they are to you\n");
            for c in &cards {
                out.push_str(&format!("- {}\n", crate::cards::text(c)));
            }
        }
        // Other threads with them lately.
        let threads: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT subject, snippet, MAX(date) FROM mail WHERE (from_addr = ?1 OR to_json LIKE ?2) AND trash = 0 AND spam = 0 \
             GROUP BY account, thread_id ORDER BY MAX(date) DESC LIMIT 6",
        )
        .bind(&person)
        .bind(format!("%\"{person}\"%"))
        .fetch_all(&m.store.pool)
        .await?;
        if !threads.is_empty() {
            out.push_str("\n## Other recent mail with them\n");
            for (subject, snippet, at) in threads {
                out.push_str(&format!("- {} ({}): {}\n", subject, date(at), clip(&snippet, 200)));
            }
        }
    }
    let open: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT who, what, due FROM commitments WHERE status = 'open' AND (lower(who) = ?1 OR item_id IN \
         (SELECT item_id FROM mail WHERE from_addr = ?1 OR to_json LIKE ?2)) ORDER BY due IS NULL, due LIMIT 10",
    )
    .bind(&person)
    .bind(format!("%\"{person}\"%"))
    .fetch_all(&m.store.pool)
    .await?;
    if !open.is_empty() {
        out.push_str("\n## Open between you\n");
        for (who, what, due) in open {
            let who = if who == "me" { "the owner".to_owned() } else { who };
            out.push_str(&format!("- {who}: {what}{}\n", due.map(|d| format!(" (due {})", date(d))).unwrap_or_default()));
        }
    }
    out.push_str(&free(m, &invitations(m, r).await?).await?);
    out.push_str(&voice(m, &person).await?);
    Ok(out)
}

/// A conversation's last messages, as the model reads them.
pub(crate) async fn conversation(m: &Memex, target: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    if let Some(rest) = target.strip_prefix("thread:") {
        let (account, thread) = rest.rsplit_once(':').unwrap_or_default();
        let rows = sqlx::query(
            "SELECT m.from_addr, m.from_name, m.subject, m.date, m.sent, i.body FROM mail m JOIN items i ON i.id = m.item_id \
             WHERE m.account = ? AND m.thread_id = ? AND m.draft = 0 AND m.trash = 0 ORDER BY m.date DESC LIMIT 6",
        )
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?;
        for r in rows.iter().rev() {
            let who = if r.get::<i64, _>("sent") == 1 {
                "the owner".to_owned()
            } else {
                format!("{} <{}>", r.get::<String, _>("from_name"), r.get::<String, _>("from_addr"))
            };
            out.push_str(&format!(
                "--- {who}, {} — {}\n{}\n",
                date(r.get("date")),
                r.get::<String, _>("subject"),
                clip(&r.get::<String, _>("body"), 3000)
            ));
        }
        return Ok(out);
    }
    let parts: Vec<&str> = target.strip_prefix("chat:").unwrap_or_default().splitn(3, ':').collect();
    let f = |k: &str| format!("json_extract(meta, '$.{k}')");
    let sql = match parts.get(2) {
        Some(_) => format!(
            "SELECT body, meta, created_at FROM items WHERE kind = 'chat' AND {} = ?1 AND {} = ?2 AND ({} = ?3 OR {} = ?3) \
             ORDER BY created_at DESC LIMIT 20",
            f("workspace"),
            f("channel"),
            f("thread_ts"),
            f("ts")
        ),
        None => format!(
            "SELECT body, meta, created_at FROM items WHERE kind = 'chat' AND {} = ?1 AND {} = ?2 AND {} IS NULL \
             ORDER BY created_at DESC LIMIT 20",
            f("workspace"),
            f("channel"),
            f("thread_ts")
        ),
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(parts.first().copied().unwrap_or_default())
        .bind(parts.get(1).copied().unwrap_or_default())
        .bind(parts.get(2).copied().unwrap_or_default())
        .fetch_all(&m.store.pool)
        .await?;
    for r in rows.iter().rev() {
        let meta: Value = serde_json::from_str(&r.get::<String, _>("meta")).unwrap_or_default();
        let who = if meta["from_me"] == true { "the owner".to_owned() } else { meta["user_name"].as_str().unwrap_or("someone").to_owned() };
        let body: String = r.get("body");
        let said = body.split_once(": ").map_or(body.as_str(), |(_, t)| t);
        out.push_str(&format!("--- {who}, {}: {}\n", date(r.get("created_at")), clip(said, 1500)));
    }
    Ok(out)
}

/// An event linked from the message being answered. Mail and calendars
/// can belong to different installs. Titles and times cannot identify it.
struct Invitation {
    target: String,
    account: String,
    calendar: String,
    event: String,
    response: String,
    start: i64,
    end: i64,
    all_day: bool,
    recurring: bool,
    instances: HashSet<String>,
}

fn response(status: &str) -> &str {
    match status {
        "accepted" => "accepted",
        "declined" => "declined",
        "tentative" => "tentative",
        "needsAction" => "not yet answered",
        _ => "not recorded",
    }
}

/// Calendar keeps Google's htmlLink verbatim; invitation links use other
/// hosts and parameters. The eid identifies the event and its calendar.
fn eid(link: &str) -> Option<String> {
    let link = link.replace("&amp;", "&").replace("&#38;", "&").replace("&#x26;", "&");
    let mut url = reqwest::Url::parse(&link).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.host_str().is_some_and(|host| host == "google.com" || host.ends_with(".google.com"))
    {
        return None;
    }
    // Keep standard base64's '+' through query decoding.
    let query = url.query()?.replace('+', "%2B");
    url.set_query(Some(&query));
    let mut values = url.query_pairs().filter(|(name, _)| name == "eid");
    let (_, value) = values.next()?;
    let value = value.trim_end_matches('=');
    if value.is_empty() || values.next().is_some() {
        return None;
    }
    Some(value.to_owned())
}

/// Event ids in the message, read once before looking through the calendar.
fn event_ids(text: &str, html: &str, body: &str) -> HashSet<String> {
    let mut ids: HashSet<_> = text
        .split_whitespace()
        .chain(body.split_whitespace())
        .filter_map(|w| eid(w.trim_matches(['<', '>', '(', ')', '[', ']', '\'', '"'])))
        .collect();
    let lower = html.to_ascii_lowercase();
    for (at, _) in lower.match_indices("href") {
        if at == 0 || !lower.as_bytes()[at - 1].is_ascii_whitespace() {
            continue;
        }
        let Some(rest) = html[at + 4..].trim_start().strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let value = if rest.starts_with(['\'', '"']) {
            let quote = rest.as_bytes()[0] as char;
            rest[1..].split(quote).next().unwrap_or("")
        } else {
            rest.split(|c: char| c.is_ascii_whitespace() || c == '>').next().unwrap_or("")
        };
        if let Some(id) = eid(value) {
            ids.insert(id);
        }
    }
    ids
}

fn decoded_event_id(id: &str) -> Option<String> {
    use base64::{
        Engine,
        engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD},
    };
    let id = id.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(id).or_else(|_| STANDARD_NO_PAD.decode(id)).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (event, rest) = text.split_once(' ')?;
    (!event.is_empty() && !rest.trim().is_empty()).then(|| event.to_owned())
}

async fn invitations(m: &Memex, r: &api::Request) -> anyhow::Result<Vec<Invitation>> {
    let mut messages = Vec::new();
    for source in &r.sources {
        let Some((account, thread)) = source.target.strip_prefix("thread:").and_then(|s| s.rsplit_once(':')) else { continue };
        let mail = sqlx::query(
            "SELECT m.text_blob, m.html_blob, i.body FROM mail m JOIN items i ON i.id = m.item_id \
             WHERE m.account = ? AND m.thread_id = ? AND m.sent = 0 AND m.draft = 0 AND m.trash = 0 ORDER BY m.date DESC LIMIT 1",
        )
        .bind(account)
        .bind(thread)
        .fetch_optional(&m.store.pool)
        .await?;
        let Some(mail) = mail else { continue };
        let read = |hash: Option<String>| async move {
            match hash {
                Some(h) => tokio::fs::read_to_string(m.blob_path(&h)).await.unwrap_or_default(),
                None => String::new(),
            }
        };
        let text = read(mail.get("text_blob")).await;
        let html = read(mail.get("html_blob")).await;
        let body: String = mail.get("body");
        let ids = event_ids(&text, &html, &body);
        if ids.is_empty() {
            continue;
        }
        messages.push((source.target.clone(), ids));
    }
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT account, calendar_id, event_id, my_status, html_link, start, end, all_day, recurrence, recurring_event_id FROM events WHERE status != 'cancelled'",
    )
    .fetch_all(&m.store.pool)
    .await?;
    let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_event: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_series: HashMap<String, HashSet<String>> = HashMap::new();
    for (n, row) in rows.iter().enumerate() {
        if let Some(id) = eid(&row.get::<String, _>("html_link")) {
            exact.entry(id).or_default().push(n);
        }
        by_event.entry(row.get("event_id")).or_default().push(n);
        if let Some(series) = row.get::<Option<String>, _>("recurring_event_id") {
            by_series.entry(series).or_default().insert(row.get("event_id"));
        }
    }
    let mut out = Vec::new();
    for (target, ids) in messages {
        let mut matches: HashSet<usize> = ids.iter().filter_map(|id| exact.get(id)).flatten().copied().collect();
        // An alias in the invitation's eid can differ from the stored calendar id.
        if matches.is_empty() {
            matches = ids.iter().filter_map(|id| decoded_event_id(id)).filter_map(|id| by_event.get(&id)).flatten().copied().collect();
        }
        // More than one event (or calendar's copy) leaves the link ambiguous.
        if matches.len() == 1 {
            let event = &rows[*matches.iter().next().unwrap()];
            out.push(Invitation {
                target,
                account: event.get("account"),
                calendar: event.get("calendar_id"),
                event: event.get("event_id"),
                response: response(&event.get::<String, _>("my_status")).into(),
                start: event.get("start"),
                end: event.get("end"),
                all_day: event.get::<i64, _>("all_day") != 0,
                recurring: event.get::<String, _>("recurrence") != "[]",
                instances: by_series.get(&event.get::<String, _>("event_id")).cloned().unwrap_or_default(),
            });
        }
    }
    Ok(out)
}

/// When you're busy over the next week, in your calendar's time zone.
async fn free(m: &Memex, invitations: &[Invitation]) -> anyhow::Result<String> {
    let now = c6_core::now_ms();
    let until = now + 7 * 86_400_000;
    let zone: Option<String> =
        sqlx::query_scalar("SELECT time_zone FROM calendars WHERE is_primary = 1 LIMIT 1").fetch_optional(&m.store.pool).await?;
    let tz: chrono_tz::Tz = zone.as_deref().and_then(|z| z.parse().ok()).unwrap_or(chrono_tz::UTC);
    let at = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.with_timezone(&tz).format("%a %Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    };
    let mut out = format!("\n## The owner's calendar, next 7 days ({}; busy times, the rest is free)\n", tz.name());
    let events = if invitations.is_empty() {
        None
    } else {
        match crate::calendar::events(m, now, until).await {
            Ok(events) => Some(events),
            Err(e) => {
                warn!("draft calendar times could not be checked: {e:#}");
                out = format!("\n## The owner's calendar, next 7 days ({})\n- the owner's calendar could not be read\n", tz.name());
                None
            }
        }
    };
    if invitations.is_empty() {
        let busy = crate::calendar::busy(m, now, until).await.unwrap_or_default();
        if busy.is_empty() {
            out.push_str("- nothing booked\n");
        }
        for (s, e) in busy.iter().take(40) {
            out.push_str(&format!("- busy {} to {}\n", at(*s), at(*e)));
        }
    } else if let Some(events) = events {
        let mut spans = Vec::new();
        let mut lines = Vec::new();
        for e in events.iter().filter(|e| e.busy && !e.all_day) {
            let span = (e.start.max(now), e.end.min(until));
            if let Some(i) = invitations.iter().find(|i| {
                i.account == e.account && i.calendar == e.calendar_id && (i.event == e.event_id || i.instances.contains(&e.event_id))
            }) {
                lines.push((span, format!(" (the invitation being answered in {}; your response: {})", i.target, i.response)));
            } else {
                spans.push(span);
            }
        }
        let busy = crate::calendar::merge_busy(spans);
        if busy.len() > 40 {
            out = format!("\n## The owner's calendar, next 7 days ({}; busy times, list truncated)\n", tz.name());
        }
        // The limit applies to other busy blocks, never the invitation.
        lines.extend(busy.into_iter().take(40).map(|span| (span, String::new())));
        lines.sort_by_key(|(span, _)| *span);
        if lines.is_empty() {
            out.push_str("- nothing booked\n");
        }
        for ((s, e), note) in lines {
            out.push_str(&format!("- busy {} to {}{note}\n", at(s), at(e)));
        }
    }
    if !invitations.is_empty() {
        out.push_str("\n## Invitations being answered\n");
        for i in invitations {
            out.push_str(&format!("- {} links to an event already in your calendar; your response: {}\n", i.target, i.response));
            let (mut start, mut end) = (i.start, i.end);
            if i.recurring {
                match crate::calendar::next_occurrence(m, &i.account, &i.calendar, &i.event, now, tz).await {
                    Ok(Some(span)) => (start, end) = span,
                    Ok(None) => {
                        out.push_str("  no remaining occurrence found for this recurring event\n");
                        continue;
                    }
                    Err(e) => {
                        warn!("draft invitation time could not be checked: {e:#}");
                        out.push_str("  the invitation's time could not be checked\n");
                        continue;
                    }
                }
            }
            let next = if i.recurring { "; next occurrence of a recurring event" } else { "" };
            if i.all_day {
                let date = |ms| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
                out.push_str(&format!(
                    "  dates: {} to {} (all day; end date exclusive{next}; overlaps not checked)\n",
                    date(start),
                    date(end)
                ));
                continue;
            }
            out.push_str(&format!("  time: {} to {} ({}{next})\n", at(start), at(end), tz.name()));
            let spans = match invitation_overlaps(m, i, start, end).await {
                Ok(spans) => spans,
                Err(e) => {
                    warn!("draft invitation overlaps could not be checked: {e:#}");
                    out.push_str("  overlaps with the invitation could not be checked\n");
                    continue;
                }
            };
            let overlaps = crate::calendar::merge_busy(spans);
            if overlaps.is_empty() {
                out.push_str("  nothing else in your calendar overlaps it\n");
            } else {
                out.push_str("  other busy blocks overlapping it:\n");
                for (s, e) in overlaps {
                    out.push_str(&format!("  - {} to {}\n", at(s), at(e)));
                }
            }
        }
    }
    Ok(out)
}

async fn invitation_overlaps(m: &Memex, i: &Invitation, start: i64, end: i64) -> anyhow::Result<Vec<(i64, i64)>> {
    Ok(crate::calendar::events(m, start, end)
        .await?
        .into_iter()
        .filter(|e| e.busy && !e.all_day && e.event_id != i.event && !i.instances.contains(&e.event_id) && e.start < end && e.end > start)
        .map(|e| (e.start, e.end))
        .collect())
}

/// How you write: your last few messages to them, else to anyone.
async fn voice(m: &Memex, person: &str) -> anyhow::Result<String> {
    let mut rows: Vec<String> = sqlx::query_scalar(
        "SELECT i.body FROM mail m JOIN items i ON i.id = m.item_id WHERE m.sent = 1 AND m.to_json LIKE ? ORDER BY m.date DESC LIMIT 3",
    )
    .bind(format!("%\"{person}\"%"))
    .fetch_all(&m.store.pool)
    .await?;
    if rows.is_empty() {
        rows =
            sqlx::query_scalar("SELECT i.body FROM mail m JOIN items i ON i.id = m.item_id WHERE m.sent = 1 ORDER BY m.date DESC LIMIT 3")
                .fetch_all(&m.store.pool)
                .await?;
    }
    let chat: Vec<String> = sqlx::query_scalar(
        "SELECT body FROM items WHERE kind = 'chat' AND json_extract(meta, '$.from_me') = 1 ORDER BY created_at DESC LIMIT 5",
    )
    .fetch_all(&m.store.pool)
    .await?;
    if rows.is_empty() && chat.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::from("\n## How the owner writes (their recent messages)\n");
    for b in rows {
        // Without the headers and the quoted thread.
        let body = b.splitn(4, '\n').last().unwrap_or("");
        let mine: Vec<&str> = body.lines().take_while(|l| !l.starts_with('>') && !l.starts_with("On ")).collect();
        out.push_str(&format!("---\n{}\n", clip(mine.join("\n").trim(), 800)));
    }
    for c in chat {
        out.push_str(&format!("--- (chat) {}\n", clip(c.split_once(": ").map_or(c.as_str(), |(_, t)| t), 300)));
    }
    Ok(out)
}

/// Who and what a reply in a conversation goes to: the mail it answers
/// (and how), or the chat conversation or thread.
pub async fn reply_to(m: &Memex, target: &str) -> anyhow::Result<Value> {
    if let Some(rest) = target.strip_prefix("thread:") {
        let (account, thread) = rest.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("not a thread"))?;
        let last = sqlx::query(
            "SELECT item_id, from_addr, from_name, subject FROM mail WHERE account = ? AND thread_id = ? AND sent = 0 AND draft = 0 \
             ORDER BY date DESC LIMIT 1",
        )
        .bind(account)
        .bind(thread)
        .fetch_optional(&m.store.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("nobody else wrote in that thread"))?;
        let subject: String = last.get("subject");
        let subject = if subject.to_lowercase().starts_with("re:") { subject } else { format!("Re: {subject}") };
        return Ok(json!({
            "kind": "mail", "account": account, "to": [last.get::<String, _>("from_addr")],
            "to_name": last.get::<String, _>("from_name"), "subject": subject, "reply_to": last.get::<String, _>("item_id"),
        }));
    }
    let parts: Vec<&str> = target.strip_prefix("chat:").unwrap_or_default().splitn(3, ':').collect();
    anyhow::ensure!(parts.len() >= 2, "not a conversation");
    Ok(json!({"kind": "chat", "workspace": parts[0], "channel": parts[1], "thread_ts": parts.get(2)}))
}

/// Sends a reply you approved (and edited) in one of a request's
/// conversations; the request is done.
pub async fn send(m: &Memex, id: i64, target: &str, text: &str) -> anyhow::Result<Value> {
    let r = crate::requests::get(m, id).await?;
    anyhow::ensure!(r.sources.iter().any(|s| s.target == target), "that conversation isn't one of the request's");
    anyhow::ensure!(!text.trim().is_empty(), "nothing to send");
    anyhow::ensure!(!(text.contains("[[") && text.contains("]]")), "choose first: the draft still has choices in [[…]]");
    let to = reply_to(m, target).await?;
    let sent = if to["kind"] == "mail" {
        let d = api::MailDraft {
            account: to["account"].as_str().unwrap_or_default().to_owned(),
            to: serde_json::from_value(to["to"].clone())?,
            subject: to["subject"].as_str().unwrap_or_default().to_owned(),
            body: text.to_owned(),
            reply_to: to["reply_to"].as_str().map(str::to_owned),
            ..Default::default()
        };
        json!(crate::mail::queue(m, &d, "you").await?)
    } else {
        let mut p = json!({"workspace": to["workspace"], "channel": to["channel"], "text": text, "made_by": "you"});
        if let Some(t) = to["thread_ts"].as_str() {
            p["thread_ts"] = json!(t);
        }
        crate::chat::method(m, "memex.chat.post", &p, &["chat".to_owned()]).await?
    };
    crate::requests::set(m, id, "done", None).await?;
    sqlx::query("UPDATE requests SET closed_by = 'replied' WHERE id = ?").bind(id).execute(&m.store.pool).await?;
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::{Parsed, Stored};

    #[tokio::test]
    async fn a_duplicate_does_not_restart_a_draft_that_finishes_during_its_read() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        sqlx::query(
            "INSERT INTO requests (id, person, title, category, created_at, updated_at) VALUES (1, 'pat@example.test', 'Review the notes', 'first_contact', 1, 1)",
        )
        .execute(&m.store.pool)
        .await
        .unwrap();
        let first = m.claim("draft:1").unwrap();
        // Hold every connection so the duplicate's request read must wait.
        let mut connections = Vec::new();
        for _ in 0..m.store.pool.options().get_max_connections() {
            connections.push(m.store.pool.acquire().await.unwrap());
        }
        let duplicate = start(&m, 1);
        tokio::pin!(duplicate);
        assert!(futures_util::poll!(&mut duplicate).is_pending());
        let finished = api::RequestDraft { text: "The notes look good.".into(), ..Default::default() };
        sqlx::query("UPDATE requests SET draft = ? WHERE id = 1")
            .bind(serde_json::to_string(&finished).unwrap())
            .execute(&mut *connections[0])
            .await
            .unwrap();
        drop(first);
        drop(connections);
        let r = c6_testkit::within("the duplicate reads the finished draft", duplicate).await.unwrap();
        assert_eq!(r.draft, Some(finished));
        assert!(!m.claimed("draft:1"), "the duplicate did not take a new claim");
        let calls: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_calls").fetch_one(&m.store.pool).await.unwrap();
        assert_eq!(calls, 0);
    }

    async fn event(m: &Memex, id: &str, start: i64, status: &str, link: &str) {
        span(m, id, start, start + 3_600_000, status, link).await;
    }

    async fn span(m: &Memex, id: &str, start: i64, end: i64, status: &str, link: &str) {
        let time = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339();
        crate::calendar::store_event(
            m,
            "test-calendar",
            "cal-acct",
            "me@example.com",
            "primary",
            &json!({
                "id": id, "summary": "Workshop", "start": {"dateTime": time(start)},
                "end": {"dateTime": time(end)}, "htmlLink": link,
                "attendees": [{"email": "me@example.com", "self": true, "responseStatus": status}],
            }),
        )
        .await
        .unwrap();
    }

    async fn message(m: &Memex, text: &str, html: Option<&str>) -> api::Request {
        let parsed = Parsed {
            message_id: "invite@example.com".into(),
            from: ("host@example.net".into(), "Host".into()),
            to: vec![("me@example.com".into(), String::new())],
            subject: "Workshop".into(),
            text: text.into(),
            html: html.map(|h| h.as_bytes().to_vec()),
            ..Default::default()
        };
        let at = Stored {
            account: "mail-acct",
            external_id: "invite",
            thread_id: "thread",
            source: "test-mail",
            rfc_id: &parsed.message_id,
            origin: "mirrored",
            owner: "me@example.com",
            message: "invite",
        };
        crate::mail::store(m, &parsed, "", &at, &["INBOX".into()], c6_core::now_ms()).await.unwrap();
        api::Request {
            sources: vec![api::RequestSource { target: "thread:mail-acct:thread".into(), kind: "mail".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn invitation_and_response_in_the_drafting_context() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "UTC"})).await.unwrap();
        let start = c6_core::now_ms() + 2 * 86_400_000;
        let link = "https://www.google.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20=";
        event(&m, "invited", start, "accepted", link).await;
        // Even at the same time, another event is not this invitation.
        event(&m, "other", start, "accepted", "https://www.google.com/calendar/event?eid=b3RoZXIgbWVAZXhhbXBsZS5jb20=").await;
        let r = message(
            &m,
            "Please join our workshop.",
            Some("<a HREF = 'https://calendar.google.com/calendar/event?action=VIEW&amp;eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20&amp;tok=invented&amp;ctz=Europe/Berlin&amp;hl=en&amp;es=0'>View invitation</a>"),
        )
        .await;
        let text = context(&m, &r).await.unwrap();
        let busy: Vec<_> = text.lines().filter(|l| l.starts_with("- busy ")).collect();
        assert_eq!(busy.len(), 2, "{text}");
        assert_eq!(busy.iter().filter(|l| l.contains("the invitation being answered")).count(), 1, "{text}");
        assert_eq!(busy.iter().filter(|l| l.contains("your response: accepted")).count(), 1, "{text}");
        assert!(text.contains("thread:mail-acct:thread links to an event already in your calendar; your response: accepted"), "{text}");

        for (status, response) in [("declined", "declined"), ("tentative", "tentative"), ("needsAction", "not yet answered")] {
            event(&m, "invited", start, status, link).await;
            let text = context(&m, &r).await.unwrap();
            assert!(text.contains(&format!("calendar; your response: {response}")), "{text}");
            if status == "declined" {
                assert_eq!(text.lines().filter(|l| l.starts_with("- busy ")).count(), 1, "{text}");
                assert!(!text.contains("the invitation being answered"), "{text}");
            }
        }
    }

    #[tokio::test]
    async fn without_a_unique_link_the_calendar_context_is_unchanged() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true})).await.unwrap();
        let start = c6_core::now_ms() + 2 * 86_400_000;
        let link = "https://www.google.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20=";
        event(&m, "invited", start, "accepted", link).await;
        let r = message(&m, "Please send the workshop notes.", None).await;
        let invitations = invitations(&m, &r).await.unwrap();
        assert!(invitations.is_empty());
        let at = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%a %Y-%m-%d %H:%M").to_string();
        let expected = format!(
            "\n## The owner's calendar, next 7 days (UTC; busy times, the rest is free)\n- busy {} to {}\n",
            at(start),
            at(start + 3_600_000)
        );
        assert_eq!(free(&m, &invitations).await.unwrap(), expected);
        let text = context(&m, &r).await.unwrap();
        assert!(text.ends_with(&expected), "{text}");
        assert!(!text.contains("your response:"), "{text}");

        // Two stored events share a URL: neither is safe to label.
        event(&m, "copy", start, "declined", link).await;
        let r = message(&m, link, None).await;
        assert!(super::invitations(&m, &r).await.unwrap().is_empty());
    }

    fn link(event: &str, calendar: &str) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        format!("https://www.google.com/calendar/event?eid={}", URL_SAFE_NO_PAD.encode(format!("{event} {calendar}")))
    }

    #[tokio::test]
    async fn aliases_match_only_a_unique_event_when_no_exact_eid_matches() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true})).await.unwrap();
        let start = c6_core::now_ms() + 2 * 86_400_000;
        event(&m, "dinner", start, "accepted", &link("dinner", "owner@example.test")).await;
        let r = message(&m, &link("dinner", "alias@example.test"), None).await;
        let found = invitations(&m, &r).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!((&*found[0].account, &*found[0].calendar, &*found[0].event), ("cal-acct", "primary", "dinner"));
        assert!(free(&m, &found).await.unwrap().contains("the invitation being answered"));

        crate::calendar::store_event(
            &m,
            "test-calendar",
            "other-acct",
            "owner@example.test",
            "other",
            &json!({
                "id": "dinner", "start": {"dateTime": "2030-01-01T00:00:00Z"}, "end": {"dateTime": "2030-01-01T01:00:00Z"},
                "htmlLink": link("dinner", "other@example.test"),
            }),
        )
        .await
        .unwrap();
        assert!(invitations(&m, &r).await.unwrap().is_empty(), "two calendars' copies are ambiguous");
        let r = message(&m, &link("dinner", "owner@example.test"), None).await;
        assert_eq!(invitations(&m, &r).await.unwrap().len(), 1, "an exact eid still identifies one copy");

        // Two exact matches must not fall back to the one matching event id.
        sqlx::query("DELETE FROM events WHERE account = 'other-acct'").execute(&m.store.pool).await.unwrap();
        event(&m, "different-id", start, "accepted", &link("dinner", "owner@example.test")).await;
        assert!(invitations(&m, &r).await.unwrap().is_empty());
        let r = message(&m, "https://calendar.google.com/calendar/event?eid=not-base64!", None).await;
        assert!(invitations(&m, &r).await.unwrap().is_empty());
    }

    #[test]
    fn decoded_eids_accept_both_alphabets_and_optional_padding() {
        use base64::{
            Engine,
            engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
        };
        for engine in [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD] {
            for id in ["dinner", "event-\u{ffff}"] {
                assert_eq!(decoded_event_id(&engine.encode(format!("{id} alias@example.test"))), Some(id.into()));
                let link = format!("https://calendar.google.com/calendar/event?eid={}", engine.encode(format!("{id} alias@example.test")));
                assert_eq!(decoded_event_id(&eid(&link).unwrap()), Some(id.into()));
            }
        }
        for text in ["", "dinner", " alias@example.test", "dinner "] {
            assert!(decoded_event_id(&STANDARD.encode(text)).is_none());
        }
        assert!(decoded_event_id(&STANDARD.encode([0xff, b' ', b'x'])).is_none());
        assert!(decoded_event_id("not-base64!").is_none());
    }

    #[tokio::test]
    async fn invitation_overlaps_use_its_own_span_and_exclude_copies_and_touching_ends() {
        use chrono::TimeZone;
        c6_llm::install_crypto_provider();
        for days in [2, 10] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            crate::calendar::store_calendar(&m, "elsewhere", &json!({"id": "primary", "primary": true, "timeZone": "UTC"})).await.unwrap();
            crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "timeZone": "America/Los_Angeles"})).await.unwrap();
            let tz = chrono_tz::America::Los_Angeles;
            let day = chrono::Utc::now().date_naive() + chrono::Duration::days(days);
            let time =
                |hour, minute| tz.from_local_datetime(&day.and_hms_opt(hour, minute, 0).unwrap()).single().unwrap().timestamp_millis();
            let at = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%a %Y-%m-%d %H:%M").to_string();
            span(&m, "dinner", time(17, 30), time(20, 0), "accepted", &link("dinner", "owner@example.test")).await;
            span(&m, "morning", time(6, 0), time(16, 0), "accepted", "").await;
            span(&m, "touching", time(16, 0), time(17, 30), "accepted", "").await;
            span(&m, "after", time(20, 0), time(22, 0), "accepted", "").await;
            span(&m, "declined", time(18, 0), time(19, 0), "declined", "").await;
            span(&m, "transparent", time(18, 0), time(19, 0), "accepted", "").await;
            sqlx::query("UPDATE events SET transparent = 1 WHERE event_id = 'transparent'").execute(&m.store.pool).await.unwrap();
            crate::calendar::store_event(
                &m,
                "test-calendar",
                "cal-acct",
                "owner@example.test",
                "primary",
                &json!({
                    "id": "all-day", "start": {"date": day.to_string()}, "end": {"date": day.succ_opt().unwrap().to_string()},
                }),
            )
            .await
            .unwrap();
            for (calendar, selected, id) in [("copy", true, "dinner"), ("hidden", false, "hidden")] {
                crate::calendar::store_calendar(&m, "other-acct", &json!({"id": calendar, "selected": selected})).await.unwrap();
                crate::calendar::store_event(
                    &m,
                    "test-calendar",
                    "other-acct",
                    "owner@example.test",
                    calendar,
                    &json!({
                        "id": id, "start": {"dateTime": chrono::DateTime::from_timestamp_millis(time(18, 0)).unwrap().to_rfc3339()},
                        "end": {"dateTime": chrono::DateTime::from_timestamp_millis(time(19, 0)).unwrap().to_rfc3339()},
                    }),
                )
                .await
                .unwrap();
            }
            let r = message(&m, &link("dinner", "owner@example.test"), None).await;
            let prefix = format!(
                "- thread:mail-acct:thread links to an event already in your calendar; your response: accepted\n  time: {} to {} (UTC)\n",
                at(time(17, 30)),
                at(time(20, 0))
            );
            let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
            assert_eq!(
                text.split_once("## Invitations being answered\n").unwrap().1,
                format!("{prefix}  nothing else in your calendar overlaps it\n")
            );
            span(&m, "overlap", time(19, 0), time(21, 0), "accepted", "").await;
            span(&m, "merged", time(19, 30), time(20, 30), "accepted", "").await;
            let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
            let overlaps = format!("  other busy blocks overlapping it:\n  - {} to {}\n", at(time(19, 0)), at(time(21, 0)));
            assert_eq!(text.split_once("## Invitations being answered\n").unwrap().1, format!("{prefix}{overlaps}"));
            span(&m, "dinner", time(17, 30), time(20, 0), "declined", &link("dinner", "owner@example.test")).await;
            let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
            assert_eq!(
                text.split_once("## Invitations being answered\n").unwrap().1,
                format!("{}{overlaps}", prefix.replace("accepted", "declined"))
            );
        }
    }

    #[tokio::test]
    async fn recurring_invitations_use_the_next_unended_occurrence_and_all_day_ones_keep_dates() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "UTC"})).await.unwrap();
        let day = 86_400_000;
        let now = c6_core::now_ms() / 1000 * 1000;
        span(&m, "series", now - 8 * day, now - 6 * day, "declined", &link("series", "owner@example.test")).await;
        sqlx::query("UPDATE events SET recurrence = ? WHERE event_id = 'series'")
            .bind(json!(["RRULE:FREQ=WEEKLY;COUNT=3"]).to_string())
            .execute(&m.store.pool)
            .await
            .unwrap();
        let r = message(&m, &link("series", "owner@example.test"), None).await;
        let at = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%a %Y-%m-%d %H:%M").to_string();
        let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
        assert!(
            text.contains(&format!("time: {} to {} (UTC; next occurrence of a recurring event)", at(now - day), at(now + day))),
            "{text}"
        );
        assert!(text.contains("nothing else in your calendar overlaps it"), "{text}");
        event(&m, "other", now, "accepted", "").await;
        let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
        assert!(text.ends_with(&format!("  other busy blocks overlapping it:\n  - {} to {}\n", at(now), at(now + 3_600_000))), "{text}");

        crate::calendar::store_event(
            &m,
            "test-calendar",
            "cal-acct",
            "owner@example.test",
            "primary",
            &json!({
                "id": "series", "start": {"date": "2030-04-02"}, "end": {"date": "2030-04-03"},
                "htmlLink": link("series", "owner@example.test"),
            }),
        )
        .await
        .unwrap();
        let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
        let section = text.split_once("## Invitations being answered\n").unwrap().1;
        assert!(section.ends_with("  dates: 2030-04-02 to 2030-04-03 (all day; end date exclusive; overlaps not checked)\n"), "{section}");
        assert!(!section.contains("overlaps it") && !section.contains("overlapping it"), "{section}");
    }

    #[tokio::test]
    async fn odd_recurrences_leave_invitation_facts_in_the_context() {
        c6_llm::install_crypto_provider();
        for recurrence in ["not json".to_owned(), json!(["RRULE:FREQ=UNSUPPORTED"]).to_string()] {
            for days in [2, 10] {
                let dir = tempfile::tempdir().unwrap();
                let m = Memex::open(dir.path(), Default::default()).await.unwrap();
                crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "UTC"}))
                    .await
                    .unwrap();
                let start = c6_core::now_ms() / 1000 * 1000 + days * 86_400_000;
                event(&m, "invited", start, "accepted", &link("invited", "owner@example.test")).await;
                let r = message(&m, &link("invited", "owner@example.test"), None).await;
                sqlx::query("UPDATE events SET recurrence = ? WHERE event_id = 'invited'")
                    .bind(&recurrence)
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
                let text = context(&m, &r).await.unwrap();
                let at = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%a %Y-%m-%d %H:%M").to_string();
                if recurrence == "not json" {
                    assert!(text.contains(&format!(
                        "your response: accepted\n  time: {} to {} (UTC; next occurrence of a recurring event)\n  nothing else in your calendar overlaps it\n",
                        at(start), at(start + 3_600_000)
                    )), "{text}");
                } else {
                    assert!(text.contains("your response: accepted\n  the invitation's time could not be checked\n"), "{text}");
                    assert!(!text.contains("nothing else in your calendar overlaps it"), "{text}");
                }

                sqlx::query("UPDATE events SET recurrence = '[]' WHERE event_id = 'invited'").execute(&m.store.pool).await.unwrap();
                event(&m, "unrelated", start, "accepted", "").await;
                sqlx::query("UPDATE events SET recurrence = ? WHERE event_id = 'unrelated'")
                    .bind(&recurrence)
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
                let text = context(&m, &r).await.unwrap();
                assert!(text.contains("your response: accepted\n  time: "), "{text}");
                if recurrence == "not json" {
                    assert!(
                        text.contains(&format!("  other busy blocks overlapping it:\n  - {} to {}\n", at(start), at(start + 3_600_000))),
                        "{text}"
                    );
                } else {
                    assert!(text.contains("  overlaps with the invitation could not be checked\n"), "{text}");
                }
                assert!(!text.contains("nothing else in your calendar overlaps it"), "{text}");
            }
        }
    }

    #[tokio::test]
    async fn an_unreadable_calendar_does_not_claim_the_owner_is_free() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "UTC"})).await.unwrap();
        let start = c6_core::now_ms() + 2 * 86_400_000;
        event(&m, "invited", start, "accepted", &link("invited", "owner@example.test")).await;
        event(&m, "appointment", start + 2 * 3_600_000, "accepted", "").await;
        event(&m, "unsupported", start, "accepted", "").await;
        sqlx::query("UPDATE events SET recurrence = ? WHERE event_id = 'unsupported'")
            .bind(json!(["RRULE:FREQ=UNSUPPORTED"]).to_string())
            .execute(&m.store.pool)
            .await
            .unwrap();
        let r = message(&m, &link("invited", "owner@example.test"), None).await;
        let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
        let (calendar, invitation) = text.split_once("\n## Invitations being answered\n").unwrap();
        assert_eq!(calendar, "\n## The owner's calendar, next 7 days (UTC)\n- the owner's calendar could not be read\n");
        assert!(!text.contains("the rest is free") && !text.contains("nothing booked"), "{text}");
        assert!(invitation.contains("your response: accepted\n  time: "), "{text}");
        assert!(invitation.ends_with("  overlaps with the invitation could not be checked\n"), "{text}");
    }

    #[tokio::test]
    async fn recurring_invitations_follow_cancelled_and_moved_instances() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "UTC"})).await.unwrap();
        let day = 86_400_000;
        let start =
            (chrono::Utc::now().date_naive() + chrono::Duration::days(1)).and_hms_opt(17, 30, 0).unwrap().and_utc().timestamp_millis();
        let duration = 150 * 60_000;
        span(&m, "series", start, start + duration, "accepted", &link("series", "owner@example.test")).await;
        sqlx::query("UPDATE events SET recurrence = ? WHERE event_id = 'series'")
            .bind(json!(["RRULE:FREQ=WEEKLY;COUNT=3"]).to_string())
            .execute(&m.store.pool)
            .await
            .unwrap();
        let time = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339();
        crate::calendar::store_event(
            &m,
            "test-calendar",
            "cal-acct",
            "owner@example.test",
            "primary",
            &json!({"id": "cancelled", "recurringEventId": "series", "status": "cancelled",
                "originalStartTime": {"dateTime": time(start)},
                "start": {"dateTime": time(start)}, "end": {"dateTime": time(start + duration)}}),
        )
        .await
        .unwrap();
        let r = message(&m, &link("series", "owner@example.test"), None).await;
        let at = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%a %Y-%m-%d %H:%M").to_string();
        let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
        assert!(
            text.contains(&format!(
                "  time: {} to {} (UTC; next occurrence of a recurring event)\n",
                at(start + 7 * day),
                at(start + 7 * day + duration)
            )),
            "{text}"
        );
        let moved = start + 8 * day + 30 * 60_000;
        for (calendar, id) in [("primary", "moved"), ("copy", "moved-copy")] {
            crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": calendar, "timeZone": "UTC"})).await.unwrap();
            crate::calendar::store_event(
                &m,
                "test-calendar",
                "cal-acct",
                "owner@example.test",
                calendar,
                &json!({"id": id, "recurringEventId": "series", "originalStartTime": {"dateTime": time(start + 7 * day)},
                    "start": {"dateTime": time(moved)}, "end": {"dateTime": time(moved + duration)}}),
            )
            .await
            .unwrap();
        }
        let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
        assert!(
            text.ends_with(&format!(
                "  time: {} to {} (UTC; next occurrence of a recurring event)\n  nothing else in your calendar overlaps it\n",
                at(moved),
                at(moved + duration)
            )),
            "{text}"
        );
        let events = crate::calendar::events(&m, start, start + 10 * day).await.unwrap();
        assert_eq!(events.iter().map(|e| (e.start, e.end)).collect::<Vec<_>>(), vec![(moved, moved + duration); 2]);
    }

    #[tokio::test]
    async fn all_day_weekly_invitations_use_dates_even_in_a_western_time_zone() {
        use chrono::Datelike;
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "America/New_York"}))
            .await
            .unwrap();
        let today = chrono::Utc::now().date_naive();
        let monday = today + chrono::Duration::days(14 - i64::from(today.weekday().num_days_from_monday()));
        let end = monday.succ_opt().unwrap();
        crate::calendar::store_event(
            &m,
            "test-calendar",
            "cal-acct",
            "owner@example.test",
            "primary",
            &json!({"id": "series", "start": {"date": monday.to_string()}, "end": {"date": end.to_string()},
                "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=3"], "htmlLink": link("series", "owner@example.test")}),
        )
        .await
        .unwrap();
        let r = message(&m, &link("series", "owner@example.test"), None).await;
        let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
        assert!(
            text.ends_with(&format!(
                "  dates: {monday} to {end} (all day; end date exclusive; next occurrence of a recurring event; overlaps not checked)\n"
            )),
            "{text}"
        );
    }

    #[tokio::test]
    async fn busy_blocks_are_merged_and_the_invitation_survives_the_limit() {
        c6_llm::install_crypto_provider();
        for adjacent in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "UTC"})).await.unwrap();
            let hour = 3_600_000;
            let start = c6_core::now_ms() + hour;
            let at = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%a %Y-%m-%d %H:%M").to_string();
            let mut expected = Vec::new();
            if adjacent {
                // Nine adjacent appointments per day should cost one line.
                for day in 0..6 {
                    let day_start = start + day * 24 * hour;
                    for n in 0..9 {
                        event(&m, &format!("day-{day}-{n}"), day_start + n * hour, "accepted", "").await;
                    }
                    expected.push(format!("- busy {} to {}", at(day_start), at(day_start + 9 * hour)));
                }
            } else {
                for n in 0..45 {
                    let s = start + n * 2 * hour;
                    event(&m, &format!("block-{n}"), s, "accepted", "").await;
                    if n < 40 {
                        expected.push(format!("- busy {} to {}", at(s), at(s + hour)));
                    }
                }
            }
            let link = "https://www.google.com/calendar/event?eid=aW52aXRlIG1lQGV4YW1wbGUuY29t";
            let invited = start + 6 * 24 * hour;
            event(&m, "invited", invited, "accepted", link).await;
            let r = message(&m, link, None).await;
            let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
            expected.push(format!(
                "- busy {} to {} (the invitation being answered in thread:mail-acct:thread; your response: accepted)",
                at(invited),
                at(invited + hour)
            ));
            let busy: Vec<_> = text.lines().filter(|l| l.starts_with("- busy ")).collect();
            assert_eq!(busy, expected, "{text}");
            assert_eq!(text.contains("list truncated"), !adjacent, "{text}");
            assert_eq!(text.contains("the rest is free"), adjacent, "{text}");

            // An invitation overlapping the first block stays separate and first.
            event(&m, "invited", start - hour / 2, "accepted", link).await;
            let text = free(&m, &invitations(&m, &r).await.unwrap()).await.unwrap();
            let busy: Vec<_> = text.lines().filter(|l| l.starts_with("- busy ")).collect();
            assert!(busy[0].contains("the invitation being answered"), "{text}");
            assert_eq!(&busy[1..], &expected[..expected.len() - 1], "{text}");
        }
    }

    #[test]
    fn event_ids_from_all_message_parts_are_collected_once() {
        let ids = event_ids(
            "View https://calendar.google.com/calendar/event?eid=first",
            "<a href='https://www.google.com/calendar/event?eid=second'>Another</a><a href='https://google.com/calendar/event?eid=first'>Again</a>",
            "See https://www.google.com/calendar/event?eid=third",
        );
        assert_eq!(ids, HashSet::from(["first".into(), "second".into(), "third".into()]));
    }

    fn links_to(text: &str, html: &str, link: &str) -> bool {
        eid(link).is_some_and(|id| event_ids(text, html, "").contains(&id))
    }

    #[test]
    fn event_ids_are_exact() {
        let link = "https://www.google.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20%3D";
        for url in [
            "https://calendar.google.com/calendar/event?action=VIEW&eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20&tok=invented&ctz=Europe/Berlin&hl=en&es=0",
            "https://calendar.google.com/calendar/event?hl=en&amp;eid=%6429ya3Nob3AgbWVAZXhhbXBsZS5jb20%3D%3D&amp;action=VIEW",
            "https://google.com/calendar/event?action=VIEW&eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20",
        ] {
            assert!(links_to(&format!("Join us: <{url}>"), "", link), "{url}");
            assert!(links_to("", &format!("<a HREF = '{url}'>Invitation</a>"), link), "{url}");
        }
        for response in [1, 2, 3] {
            let url = format!(
                "https://calendar.google.com/calendar/event?action=RESPOND&eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20&rst={response}&tok=invented&ctz=Europe%2FBerlin&hl=en&es=0"
            );
            assert!(links_to(&url, "", link));
            assert!(links_to("", &format!("<a href='{url}'>Reply</a>"), link));
        }
        for url in [
            "https://tickets.example.net/unsubscribe?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20",
            "https://notgoogle.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20",
            "https://google.com.example.net/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20",
            "https://calendar.google.com/calendar/event?eid=b3RoZXIgbWVAZXhhbXBsZS5jb20",
            "https://calendar.google.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20more",
            "https://calendar.google.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb",
            "https://calendar.google.com/calendar/event?xeid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20",
            "https://calendar.google.com/calendar/event?tok=eid%3Dd29ya3Nob3AgbWVAZXhhbXBsZS5jb20",
            "https://calendar.google.com/calendar/event?action=VIEW",
            "https://calendar.google.com/calendar/event?eid=",
            "https://calendar.google.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20&eid=other",
        ] {
            assert!(!links_to(url, "", link), "{url}");
            assert!(!links_to("", &format!("<a href=\"{url}\">Other</a>"), link), "{url}");
            assert!(!links_to(link, "", url), "{url}");
        }
        assert!(!links_to("Workshop", "", link));
        assert!(!links_to("", "", ""));
    }
}
