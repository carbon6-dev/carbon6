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
use futures_util::TryStreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use tracing::warn;

use crate::Memex;
use crate::llm;
use crate::store::clip;

const MAX_CONTEXT: usize = 16_000;
const REQUEST_CONTEXT: usize = 500;
const CONVERSATION_CONTEXT: usize = 5500;
const INVITATION_CONTEXT: usize = 2500;
const CALENDAR_CONTEXT: usize = 3500;
const CARD_CONTEXT: usize = 700;
const COMMITMENT_CONTEXT: usize = 1000;
const RECENT_CONTEXT: usize = 1000;
const VOICE_CONTEXT: usize = 1300;
const OVERDUE_DAYS: i64 = 30;
const UNDATED_DAYS: i64 = 90;
const RECENT_DAYS: i64 = 180;
const VOICE_DAYS: i64 = 365;
const MAIL_WINDOW: i64 = 2000;
const MAIL_CANDIDATES: i64 = 100;
const VOICE_SAMPLE: usize = 350;
const DAY: i64 = 86_400_000;

// Bound the indexed date walk before any eligibility or recipient checks.
const MAIL_HISTORY: &str = "WITH candidates AS MATERIALIZED (\
    SELECT * FROM mail INDEXED BY mail_date WHERE date >= ?2 ORDER BY date DESC LIMIT ?4), \
    matching AS MATERIALIZED (SELECT * FROM candidates WHERE (?5 = 0 OR sent = 1) \
    AND trash = 0 AND spam = 0 AND draft = 0 \
    AND lower(ltrim(subject)) NOT LIKE 'fwd:%' AND lower(ltrim(subject)) NOT LIKE 'fw:%' \
    AND (?1 = '' OR (?5 = 0 AND lower(from_addr) = ?1) OR \
    EXISTS (SELECT 1 FROM json_each(to_json) WHERE lower(json_extract(value, '$[0]')) = ?1)) \
    ORDER BY date DESC LIMIT ?3) \
    SELECT m.*, i.body FROM matching m JOIN items i ON i.id = m.item_id ORDER BY m.date DESC";

fn absolute_link(text: &str) -> bool {
    let n = text.bytes().take_while(u8::is_ascii_alphabetic).count();
    text.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("www."))
        || (text.as_bytes().get(n) == Some(&b':')
            && matches!(text[..n].to_ascii_lowercase().as_str(), "http" | "https" | "mailto" | "ftp" | "ftps")
            && text[n + 1..].chars().next().is_some_and(|c| !c.is_whitespace()))
}

// Unmatched brackets end a path; a query consumes the rest of its run so its tokens cannot escape.
fn link_extent(text: &str) -> usize {
    let (mut round, mut square) = (0, 0);
    let mut end = text.len();
    for (n, c) in text.char_indices() {
        if matches!(c, '?' | '#')
            || (c == '%' && text.get(n..n + 3).is_some_and(|s| s.eq_ignore_ascii_case("%3f") || s.eq_ignore_ascii_case("%23")))
        {
            return text.find(char::is_whitespace).unwrap_or(text.len());
        }
        if c.is_whitespace() || "<>\"".contains(c) || (c == ')' && round == 0) || (c == ']' && square == 0) {
            end = n;
            break;
        }
        match c {
            '(' => round += 1,
            ')' => round -= 1,
            '[' => square += 1,
            ']' => square -= 1,
            _ => {}
        }
    }
    if text[end..].chars().next().is_none_or(char::is_whitespace) {
        end = text[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', '\'']).len();
    }
    end
}

fn clean_link(text: &str, limit: usize) -> String {
    let start = limit - text[..limit].trim_start().len();
    let end = start + link_extent(&text[start..]).min(limit - start);
    format!("{}{}{}", &text[..start], cut_link(&text[start..end]), &text[end..limit])
}

fn cut_link(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let cut = [text.find('?'), text.find('#'), lower.find("%3f"), lower.find("%23")].into_iter().flatten().min().unwrap_or(text.len());
    let path = &text[..cut];
    let www = path.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("www."));
    let parse = if www { format!("https://{path}") } else { path.to_owned() };
    if absolute_link(path)
        && let Ok(mut url) = reqwest::Url::parse(&parse)
    {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        if www { url.as_str().strip_prefix("https://").unwrap_or(url.as_str()).to_owned() } else { url.to_string() }
    } else {
        path.to_owned()
    }
}

fn link_list(mut text: &str, mut limit: usize, srcset: bool) -> String {
    let mut out = String::new();
    while limit > 0 {
        let space = limit - text[..limit].trim_start().len();
        out.push_str(&text[..space]);
        text = &text[space..];
        limit -= space;
        let end = text[..limit].find(char::is_whitespace).unwrap_or(limit);
        let token = &text[..end];
        out.push_str(&clean_link(text, end));
        text = &text[end..];
        limit -= end;
        if srcset && !token.ends_with(',') {
            let end = text[..limit].find(',').map_or(limit, |n| n + 1);
            out.push_str(&text[..end]);
            text = &text[end..];
            limit -= end;
        }
    }
    out
}

// Consume a URL-bearing attribute as a whole, including query-only values.
fn link_attribute(text: &str) -> Option<(usize, String)> {
    let n = text.find(|c: char| c.is_whitespace() || "=<>/\"'".contains(c)).unwrap_or(text.len());
    let name = text[..n].to_ascii_lowercase();
    if !["src", "href", "url"].iter().any(|part| name.contains(part))
        && !matches!(name.as_str(), "action" | "formaction" | "poster" | "cite" | "background" | "data" | "ping")
    {
        return None;
    }
    let rest = text[n..].trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| matches!(c, '\'' | '"'));
    let value = if quote.is_some() { &rest[1..] } else { rest };
    let end = if let Some(quote) = quote {
        value.find(quote).unwrap_or(value.len())
    } else {
        value.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(value.len())
    };
    let prefix = text.len() - value.len();
    let closing = usize::from(quote.is_some() && end < value.len());
    // A quoted value ends only at its closing quote, even inside a query.
    let cleaned = if quote.is_some() {
        format!("{}{}", cut_link(&value[..end]), &value[end..end + closing])
    } else if name.contains("srcset") {
        link_list(value, end + closing, true)
    } else if name == "ping" {
        link_list(value, end + closing, false)
    } else {
        clean_link(value, end + closing)
    };
    Some((prefix + end + closing, format!("{}{cleaned}", &text[..prefix])))
}

/// Text crosses into a draft here, including text derived from HTML.
pub(crate) fn clean(text: &str) -> String {
    let text: String = text
        .chars()
        .filter(|c| {
            !matches!(*c,
                '\u{00ad}' | '\u{034f}' | '\u{061c}' | '\u{115f}' | '\u{1160}' | '\u{17b4}' | '\u{17b5}' |
                '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' |
                '\u{2800}' | '\u{3164}' | '\u{feff}' | '\u{ffa0}' | '\u{e0000}'..='\u{e007f}'
            )
        })
        .collect();
    let mut out = String::new();
    let mut rest = text.as_str();
    let mut in_tag = false;
    let mut quote = None;
    while !rest.is_empty() {
        let boundary = out.chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric() && !"+-.".contains(c));
        if in_tag
            && quote.is_none()
            && out.chars().next_back().is_some_and(char::is_whitespace)
            && let Some((end, attribute)) = link_attribute(rest)
        {
            out.push_str(&attribute);
            rest = &rest[end..];
            continue;
        }
        let linked = !in_tag && (out.ends_with("](") || (boundary && rest.starts_with('/')));
        if (boundary && absolute_link(rest)) || linked {
            let delimiter = quote.or_else(|| out.ends_with('<').then_some('>'));
            let end = delimiter.map_or_else(|| link_extent(rest), |c| rest.find(c).unwrap_or(rest.len()));
            if end == 0 {
                let c = rest.chars().next().unwrap();
                out.push(c);
                rest = &rest[c.len_utf8()..];
                continue;
            }
            out.push_str(&cut_link(&rest[..end]));
            rest = &rest[end..];
        } else {
            let c = rest.chars().next().unwrap();
            if in_tag {
                if quote == Some(c) {
                    quote = None;
                } else if quote.is_none() && matches!(c, '\'' | '"') {
                    quote = Some(c);
                } else if quote.is_none() && c == '>' {
                    in_tag = false;
                }
            } else if c == '<' {
                in_tag = true;
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    let mut lines = Vec::new();
    let mut blank = false;
    for line in out.lines() {
        if !line.trim().is_empty() || !blank {
            lines.push(line.trim_end());
        }
        blank = line.trim().is_empty();
    }
    let mut text = lines.join("\n");
    if out.ends_with('\n') {
        text.push('\n');
    }
    text
}

// Messages arrive newest first; spend their share before rendering in date order.
fn conversations(header: &str, groups: &[(String, Vec<String>)], budget: usize) -> String {
    let mut out = clean(header);
    let share = budget.saturating_sub(out.chars().count()) / groups.len().max(1);
    for (heading, messages) in groups {
        let heading: String = clean(heading).chars().take(share).collect();
        let mut left = share - heading.chars().count();
        let mut kept = Vec::new();
        for message in messages {
            if left < 2 {
                break;
            }
            let text = clean(message);
            let text = if text.chars().count() > left { format!("{}…\n", text.chars().take(left - 2).collect::<String>()) } else { text };
            left -= text.chars().count();
            kept.push(text);
        }
        out.push_str(&heading);
        for message in kept.iter().rev() {
            out.push_str(message);
        }
    }
    out
}

fn assemble(sections: [(String, usize); 8], threads: &[(String, Vec<String>)]) -> String {
    let mut out = String::new();
    for (index, (text, budget)) in sections.into_iter().enumerate() {
        let text = if index == 1 { conversations(&text, threads, budget) } else { clean(&text) };
        let text = if out.ends_with("\n\n") { text.trim_start_matches('\n') } else { &text };
        if text.chars().count() > budget {
            out.extend(text.chars().take(budget - 2));
            out.push_str("…\n");
        } else {
            out.push_str(text);
        }
    }
    debug_assert!(out.chars().count() <= MAX_CONTEXT);
    out
}

async fn zone(m: &Memex) -> anyhow::Result<chrono_tz::Tz> {
    let zone: Option<String> =
        sqlx::query_scalar("SELECT time_zone FROM calendars WHERE is_primary = 1 LIMIT 1").fetch_optional(&m.store.pool).await?;
    Ok(zone.as_deref().and_then(|z| z.parse().ok()).unwrap_or(chrono_tz::UTC))
}

fn date(ms: i64, tz: chrono_tz::Tz) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| format!("{} ({})", d.with_timezone(&tz).format("%Y-%m-%d %H:%M"), tz.name()))
        .unwrap_or_default()
}

fn calendar_metadata(from: &str, parts: &str) -> bool {
    let parts: Vec<Value> = serde_json::from_str(parts).unwrap_or_default();
    from.eq_ignore_ascii_case("calendar-notification@google.com")
        || parts.iter().any(|p| p["name"].as_str().unwrap_or("").to_lowercase().ends_with(".ics") || p["mime"] == "text/calendar")
}

fn calendar_mail(row: &sqlx::sqlite::SqliteRow) -> bool {
    calendar_metadata(&row.get::<String, _>("from_addr"), &row.get::<String, _>("parts"))
}

async fn mail_text(m: &Memex, row: &sqlx::sqlite::SqliteRow) -> String {
    if let Some(hash) = row.get::<Option<String>, _>("text_blob")
        && let Ok(text) = tokio::fs::read_to_string(m.blob_path(&hash)).await
    {
        return text;
    }
    let body: String = row.get("body");
    body.split_once("\n\n").map_or(body.as_str(), |(_, b)| b).to_owned()
}

/// Writes one more draft, if a request needs one and there's a model and
/// budget. Whether it did.
pub async fn step(m: &Memex) -> anyhow::Result<bool> {
    let Some((task, per_day)) = m.understanding() else { return Ok(false) };
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
    if close_answered(m, id).await? {
        return Ok(true);
    }
    if let Err(e) = write_draft(m, &task, id).await {
        if llm::waits(m, &e).await? {
            warn!(id, "drafting waits: {e:#}");
            return Ok(false);
        }
        warn!(id, "drafting: {e:#}");
        // Not again until the conversation has news.
        sqlx::query("UPDATE requests SET draft = ?, drafted_at = ? WHERE id = ?")
            .bind(json!({"error": format!("{e:#}")}).to_string())
            .bind(c6_core::now_ms())
            .bind(id)
            .execute(&m.store.pool)
            .await?;
    }
    Ok(true)
}

pub(crate) fn task_context(context: &str) -> (&str, &str) {
    context.strip_prefix("## Request\n").unwrap_or(context).split_once('\n').unwrap_or(("", context))
}

/// Drafts (again) a request's reply now.
pub async fn draft(m: &Memex, task: &llm::Task, id: i64) -> anyhow::Result<api::RequestDraft> {
    anyhow::ensure!(!close_answered(m, id).await?, "This invitation was answered in Calendar.");
    write_draft(m, task, id).await?.ok_or_else(|| anyhow::anyhow!("The request closed while drafting."))
}

async fn write_draft(m: &Memex, task: &llm::Task, id: i64) -> anyhow::Result<Option<api::RequestDraft>> {
    let r = crate::requests::get(m, id).await?;
    let context = context(m, &r).await?;
    let template = m.prompt("memex.requests.draft", api::prompts::REQUEST_DRAFT);
    let (request, context) = task_context(&context);
    let prompt = api::prompts::fill(&template, &[("request", request), ("context", context)]);
    let system = m.prompt("memex.requests.draft.system", api::prompts::REQUEST_DRAFT_SYSTEM);
    crate::understand::count_call(m).await?;
    let v = llm::json_of(&llm::ask(m, task, "draft", &clip(&clean(&r.title), 120), &system, vec![Block::Text(prompt)], 1500).await?)?;
    let text = v["text"].as_str().unwrap_or("").trim().to_owned();
    anyhow::ensure!(!text.is_empty(), "the model wrote no draft");
    let target = draft_target(v["target"].as_str().unwrap_or(""), &r.sources);
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
    Ok(save_draft(m, id, &d).await?.then_some(d))
}

// Where to answer: one of its conversations (the latest, if the model
// named none of them). It may copy the kind along with the heading's target.
fn draft_target(target: &str, sources: &[api::RequestSource]) -> String {
    let target = target.trim().trim_matches('`').trim();
    let target = target.strip_suffix(" (mail)").or_else(|| target.strip_suffix(" (chat)")).unwrap_or(target);
    let target = target.trim().trim_matches('`').trim();
    sources.iter().find(|s| s.target == target).or_else(|| sources.last()).map(|s| s.target.clone()).unwrap_or_default()
}

async fn save_draft(m: &Memex, id: i64, d: &api::RequestDraft) -> anyhow::Result<bool> {
    let saved = sqlx::query("UPDATE requests SET draft = ?, drafted_at = ? WHERE id = ? AND status IN ('open', 'snoozed')")
        .bind(serde_json::to_string(d)?)
        .bind(d.at)
        .bind(id)
        .execute(&m.store.pool)
        .await?
        .rows_affected();
    Ok(saved != 0)
}

/// What the model reads to draft: the conversations, recent exchanges with
/// the person, what's open between you, when you're free, your voice.
pub(crate) async fn context(m: &Memex, r: &api::Request) -> anyhow::Result<String> {
    let tz = zone(m).await?;
    let now = c6_core::now_ms();
    let invitations = invitations(m, r).await?;
    let request = format!(
        "{} (from {} <{}>{})",
        r.title,
        r.person_name,
        r.person,
        r.due.map(|d| format!(", by {}", date(d, tz))).unwrap_or_default()
    )
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ");
    let request = format!("## Request\n{request}\n\n");
    let conversations = String::from("## The request's conversations (answer in one of them, by its target)\n");
    let mut threads = Vec::new();
    for s in &r.sources {
        threads.push((format!("\n### target: {} ({})\n", s.target, s.kind), conversation(m, &s.target, tz).await?));
    }
    let person = r.person.to_lowercase();
    let mut cards = String::new();
    let mut recent = String::new();
    if person.contains('@') {
        for c in crate::cards::of(m, &person).await? {
            if cards.is_empty() {
                cards.push_str("\n## Who they are to you\n");
            }
            cards.push_str(&format!("- {}\n", crate::cards::text(&c)));
        }
        let mut rows = sqlx::query(MAIL_HISTORY)
            .bind(&person)
            .bind(now - RECENT_DAYS * DAY)
            .bind(MAIL_CANDIDATES)
            .bind(MAIL_WINDOW)
            .bind(false)
            .fetch(&m.store.pool);
        let mut seen: HashSet<String> = r.sources.iter().map(|s| s.target.clone()).collect();
        let mut count = 0;
        while let Some(row) = rows.try_next().await? {
            let target = format!("thread:{}:{}", row.get::<String, _>("account"), row.get::<String, _>("thread_id"));
            if seen.contains(&target) {
                continue;
            }
            let body = mail_text(m, &row).await;
            let subject: String = row.get("subject");
            if calendar_mail(&row) {
                continue;
            }
            seen.insert(target);
            if recent.is_empty() {
                recent.push_str("\n## Other recent mail with them\n");
            }
            recent.push_str(&format!("- {subject} ({}): {}\n", date(row.get("date"), tz), clip(&clean(&body), 120)));
            count += 1;
            if count == 6 {
                break;
            }
        }
    }
    let mine: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).filter(|a| !a.is_empty()).collect();
    // There is no "owed to whom" field; direct mail is the provenance we have.
    let open = sqlx::query(
        "WITH own(address) AS (SELECT value FROM json_each(?2)), \
         direct(item_id) AS (SELECT item_id FROM mail WHERE (lower(from_addr) = ?1 AND \
         EXISTS (SELECT 1 FROM json_each(to_json) WHERE lower(json_extract(value, '$[0]')) IN (SELECT address FROM own))) OR \
         ((sent = 1 OR lower(from_addr) IN (SELECT address FROM own)) AND \
         EXISTS (SELECT 1 FROM json_each(to_json) WHERE lower(json_extract(value, '$[0]')) = ?1))) \
         SELECT who, what, due FROM commitments c JOIN items i ON i.id = c.item_id WHERE c.status = 'open' \
         AND ((due IS NOT NULL AND due >= ?3) OR (due IS NULL AND i.created_at >= ?4)) \
         AND (lower(who) = ?1 OR lower(who) IN ('me', 'the owner') OR lower(who) IN (SELECT address FROM own)) \
         AND c.item_id IN (SELECT item_id FROM direct) ORDER BY due IS NULL, due, c.id LIMIT 10",
    )
    .bind(&person)
    .bind(json!(mine).to_string())
    .bind(now - OVERDUE_DAYS * DAY)
    .bind(now - UNDATED_DAYS * DAY)
    .fetch_all(&m.store.pool)
    .await?;
    let mut commitments = String::new();
    for row in open {
        if commitments.is_empty() {
            commitments.push_str("\n## Open between you\n");
        }
        let who: String = row.get("who");
        let who = if matches!(who.to_lowercase().as_str(), "me" | "the owner") || mine.contains(&who.to_lowercase()) {
            "the owner"
        } else {
            &who
        };
        let due: Option<i64> = row.get("due");
        commitments.push_str(&format!(
            "- {who}: {}{}\n",
            row.get::<String, _>("what"),
            due.map(|d| format!(" (due {})", date(d, tz))).unwrap_or_default()
        ));
    }
    let calendar = free(m, &invitations).await?;
    let (calendar, facts) = match calendar.split_once("\n## Invitations being answered\n") {
        Some((calendar, facts)) => (calendar.to_owned(), format!("\n## Invitations being answered\n{facts}")),
        None => (calendar, String::new()),
    };
    Ok(assemble(
        [
            (request, REQUEST_CONTEXT),
            (conversations, CONVERSATION_CONTEXT),
            (cards, CARD_CONTEXT),
            (recent, RECENT_CONTEXT),
            (commitments, COMMITMENT_CONTEXT),
            (calendar, CALENDAR_CONTEXT),
            (facts, INVITATION_CONTEXT),
            (voice(m, &person).await?, VOICE_CONTEXT),
        ],
        &threads,
    ))
}

/// A reply box reads the same messages and invitation facts as a draft.
pub(crate) async fn conversation_context(m: &Memex, target: &str) -> anyhow::Result<String> {
    let tz = zone(m).await?;
    let r = api::Request { sources: vec![api::RequestSource { target: target.to_owned(), ..Default::default() }], ..Default::default() };
    let invitations = invitations(m, &r).await?;
    let messages = conversation(m, target, tz).await?;
    let mut out = conversations("## The conversation\n", &[(String::new(), messages)], CONVERSATION_CONTEXT);
    if !invitations.is_empty()
        && let Some((_, facts)) = free(m, &invitations).await?.split_once("\n## Invitations being answered\n")
    {
        out.push('\n');
        out.push_str(&clip(&clean(&format!("## Invitations being answered\n{facts}")), INVITATION_CONTEXT - 1));
    }
    Ok(out)
}

/// A conversation's last messages, as the model reads them.
async fn conversation(m: &Memex, target: &str, tz: chrono_tz::Tz) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(rest) = target.strip_prefix("thread:") {
        let (account, thread) = rest.rsplit_once(':').unwrap_or_default();
        let rows = sqlx::query(
            "SELECT m.*, i.body FROM mail m JOIN items i ON i.id = m.item_id \
             WHERE m.account = ? AND m.thread_id = ? AND m.draft = 0 AND m.trash = 0 ORDER BY m.date DESC LIMIT 6",
        )
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?;
        for r in &rows {
            let who = if r.get::<i64, _>("sent") == 1 {
                "the owner".to_owned()
            } else {
                format!("{} <{}>", r.get::<String, _>("from_name"), r.get::<String, _>("from_addr"))
            };
            let mut recipients = String::new();
            for (label, column) in [("To", "to_json"), ("Cc", "cc_json")] {
                let addresses: Vec<(String, String)> = serde_json::from_str(&r.get::<String, _>(column)).unwrap_or_default();
                if addresses.is_empty() {
                    continue;
                }
                let mut line = format!("{label}: ");
                for (n, (address, name)) in addresses.iter().enumerate() {
                    let name = name.replace(['<', '>'], "");
                    let recipient = if name.trim().is_empty() { address.clone() } else { format!("{} <{address}>", name.trim()) };
                    let separator = if n == 0 { "" } else { ", " };
                    let remaining = addresses.len() - n - 1;
                    let suffix = if remaining == 0 { String::new() } else { format!(", and {remaining} more") };
                    if line.chars().count() + separator.len() + recipient.chars().count() + suffix.len() > 300 {
                        line.push_str(&format!("{separator}and {} more", addresses.len() - n));
                        break;
                    }
                    line.push_str(separator);
                    line.push_str(&recipient);
                }
                recipients.push_str(&line);
                recipients.push('\n');
            }
            let body = mail_text(m, r).await;
            out.push(format!(
                "--- {who}, {} — {}\n{recipients}{}\n",
                date(r.get("date"), tz),
                r.get::<String, _>("subject"),
                clip(&body, 3000)
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
    for r in &rows {
        let meta: Value = serde_json::from_str(&r.get::<String, _>("meta")).unwrap_or_default();
        let who = if meta["from_me"] == true { "the owner".to_owned() } else { meta["user_name"].as_str().unwrap_or("someone").to_owned() };
        let body: String = r.get("body");
        let said = body.split_once(": ").map_or(body.as_str(), |(_, t)| t);
        out.push(format!("--- {who}, {}: {}\n", date(r.get("created_at"), tz), clip(said, 1500)));
    }
    Ok(out)
}

/// An event linked from the message being answered. Mail and calendars
/// can belong to different installs. Titles and times cannot identify it.
struct Invitation {
    target: String,
    title: String,
    organizer: String,
    optional: bool,
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

struct InvitationEvents {
    rows: Vec<sqlx::sqlite::SqliteRow>,
    exact: HashMap<String, Vec<usize>>,
    by_event: HashMap<String, Vec<usize>>,
    by_series: HashMap<String, HashSet<String>>,
}

impl InvitationEvents {
    async fn load(m: &Memex) -> anyhow::Result<Self> {
        let stored = sqlx::query(
            "SELECT account, calendar_id, event_id, my_status, html_link, start, end, all_day, recurrence, recurring_event_id, \
             summary, organizer, attendees, COALESCE(updated, 0) AS updated, \
             EXISTS (SELECT 1 FROM calendars c WHERE c.account = events.account AND c.id = events.calendar_id AND c.is_primary = 1) AS is_primary \
             FROM events WHERE status != 'cancelled'",
        )
        .fetch_all(&m.store.pool)
        .await?;
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_event: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_series: HashMap<String, HashSet<String>> = HashMap::new();
        let mut rows = Vec::new();
        for row in stored {
            // Copies of the invited series aren't conflicts, even on shared calendars.
            if let Some(series) = row.get::<Option<String>, _>("recurring_event_id") {
                by_series.entry(series).or_default().insert(row.get("event_id"));
            }
            if !row.get::<bool, _>("is_primary") {
                continue;
            }
            let n = rows.len();
            if let Some(id) = eid(&row.get::<String, _>("html_link")) {
                exact.entry(id).or_default().push(n);
            }
            by_event.entry(row.get("event_id")).or_default().push(n);
            rows.push(row);
        }
        Ok(Self { rows, exact, by_event, by_series })
    }

    fn find(&self, ids: &HashSet<String>) -> Option<&sqlx::sqlite::SqliteRow> {
        let mut matches: HashSet<usize> = ids.iter().filter_map(|id| self.exact.get(id)).flatten().copied().collect();
        // An alias in the invitation's eid can differ from the stored calendar id.
        if matches.is_empty() {
            matches = ids.iter().filter_map(|id| decoded_event_id(id)).filter_map(|id| self.by_event.get(&id)).flatten().copied().collect();
        }
        // More than one event (or calendar's copy) leaves the link ambiguous.
        (matches.len() == 1).then(|| &self.rows[*matches.iter().next().unwrap()])
    }
}

/// Close only when every incoming message is an invitation to the same answered event.
async fn answered(m: &Memex, r: &api::Request, events: Option<&InvitationEvents>) -> anyhow::Result<bool> {
    if r.category != "needs_you"
        || !(matches!(r.status.as_str(), "open" | "snoozed") || (r.status == "done" && r.closed_by.as_deref() == Some("calendar")))
    {
        return Ok(false);
    }
    let marked: bool = sqlx::query_scalar("SELECT invitation FROM requests WHERE id = ?").bind(r.id).fetch_one(&m.store.pool).await?;
    if !marked {
        return Ok(false);
    }
    let sources: Vec<(String, i64, i64, String)> =
        sqlx::query_as("SELECT target, at, noted_at, seen_mail FROM request_sources WHERE request_id = ? ORDER BY target")
            .bind(r.id)
            .fetch_all(&m.store.pool)
            .await?;
    if sources.is_empty() {
        return Ok(false);
    }
    let mut loaded = None;
    let mut matched: Option<crate::requests::CalendarAnswer> = None;
    for (target, _, _, seen_mail) in &sources {
        let Some((account, thread)) = target.strip_prefix("thread:").and_then(|s| s.rsplit_once(':')) else {
            return Ok(false);
        };
        let mails = sqlx::query(
            "SELECT m.item_id, m.date, m.from_addr, m.subject, m.parts, m.text_blob, m.html_blob, i.body FROM mail m JOIN items i ON i.id = m.item_id \
             WHERE m.account = ? AND m.thread_id = ? AND m.sent = 0 AND m.draft = 0 AND m.trash = 0 ORDER BY m.item_id",
        )
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?;
        // The invitation mark covers exactly the incoming mail triage read.
        let ids: Vec<String> = mails.iter().map(|mail| mail.get("item_id")).collect();
        if ids.is_empty() || serde_json::from_str::<Vec<String>>(seen_mail).ok().as_ref() != Some(&ids) {
            return Ok(false);
        }
        for mail in mails {
            let text = match mail.get::<Option<String>, _>("text_blob") {
                Some(hash) => {
                    let Ok(text) = tokio::fs::read_to_string(m.blob_path(&hash)).await else { return Ok(false) };
                    text
                }
                None => String::new(),
            };
            if !calendar_metadata(&mail.get::<String, _>("from_addr"), &mail.get::<String, _>("parts")) {
                return Ok(false);
            }
            let html = match mail.get::<Option<String>, _>("html_blob") {
                Some(hash) => {
                    let Ok(text) = tokio::fs::read_to_string(m.blob_path(&hash)).await else { return Ok(false) };
                    text
                }
                None => String::new(),
            };
            let ids = event_ids(&text, &html, &mail.get::<String, _>("body"));
            if ids.is_empty() {
                return Ok(false);
            }
            if events.is_none() && loaded.is_none() {
                loaded = Some(InvitationEvents::load(m).await?);
            }
            let Some(event) = events.or(loaded.as_ref()).unwrap().find(&ids) else {
                return Ok(false);
            };
            if ids.iter().any(|id| {
                events.or(loaded.as_ref()).unwrap().find(&HashSet::from([id.clone()])).is_none_or(|other| {
                    ["account", "calendar_id", "event_id"].iter().any(|key| other.get::<String, _>(*key) != event.get::<String, _>(*key))
                })
            }) {
                return Ok(false);
            }
            let updated: i64 = event.get("updated");
            // An organizer update mailed after its Calendar timestamp stays open, even if the answer was kept.
            if updated <= 0 || updated < mail.get::<i64, _>("date") {
                return Ok(false);
            }
            let key = (event.get::<String, _>("account"), event.get::<String, _>("calendar_id"), event.get::<String, _>("event_id"));
            if matched.as_ref().is_some_and(|old| (&old.account, &old.calendar, &old.event) != (&key.0, &key.1, &key.2)) {
                return Ok(false);
            }
            matched = Some(crate::requests::CalendarAnswer {
                account: key.0,
                calendar: key.1,
                event: key.2,
                status: event.get("my_status"),
                updated,
            });
        }
    }
    let Some(event) = matched else { return Ok(false) };
    let reason = match event.status.as_str() {
        "accepted" => "You accepted it in Calendar.",
        "declined" => "You declined it in Calendar.",
        "tentative" => "You answered maybe in Calendar.",
        _ => return Ok(false),
    };
    crate::requests::close_calendar(m, r.id, reason, &sources, &event).await
}

pub(crate) async fn close_answered(m: &Memex, id: i64) -> anyhow::Result<bool> {
    let r = crate::requests::get(m, id).await?;
    if r.category != "needs_you" || r.sources.iter().any(|s| !s.target.starts_with("thread:")) {
        return Ok(false);
    }
    answered(m, &r, None).await
}

/// One event lookup for the whole sweep; a change during it schedules another pass.
pub(crate) async fn sweep_answered(m: &Memex) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let Some(_claim) = m.claim("calendar:requests") else { return Ok(()) };
    if !m.calendar_changed.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    let result = async {
        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM requests WHERE invitation = 1 AND category = 'needs_you' AND status IN ('open', 'snoozed')")
                .fetch_all(&m.store.pool)
                .await?;
        if !ids.is_empty() {
            let events = InvitationEvents::load(m).await?;
            for id in ids {
                answered(m, &crate::requests::get(m, id).await?, Some(&events)).await?;
            }
        }
        anyhow::Ok(())
    }
    .await;
    if result.is_err() {
        m.calendar_changed.store(true, Ordering::Release);
    }
    result
}

async fn invitations(m: &Memex, r: &api::Request) -> anyhow::Result<Vec<Invitation>> {
    let mine: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
    let mut messages = Vec::new();
    for source in &r.sources {
        let Some((account, thread)) = source.target.strip_prefix("thread:").and_then(|s| s.rsplit_once(':')) else { continue };
        let mails = sqlx::query(
            "SELECT m.text_blob, m.html_blob, i.body FROM mail m JOIN items i ON i.id = m.item_id \
             WHERE m.account = ? AND m.thread_id = ? AND m.sent = 0 AND m.draft = 0 AND m.trash = 0 ORDER BY m.date DESC LIMIT 6",
        )
        .bind(account)
        .bind(thread)
        .fetch_all(&m.store.pool)
        .await?;
        for mail in mails {
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
    }
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let events = InvitationEvents::load(m).await?;
    let mut out = Vec::new();
    for (target, ids) in messages {
        if let Some(event) = events.find(&ids) {
            let attendees: Vec<Value> = serde_json::from_str(&event.get::<String, _>("attendees")).unwrap_or_default();
            let optional = attendees.iter().any(|a| {
                (a["self"] == true || a["email"].as_str().is_some_and(|e| mine.contains(&e.to_lowercase()))) && a["optional"] == true
            });
            out.push(Invitation {
                target,
                optional,
                title: event.get("summary"),
                organizer: event.get("organizer"),
                account: event.get("account"),
                calendar: event.get("calendar_id"),
                event: event.get("event_id"),
                response: response(&event.get::<String, _>("my_status")).into(),
                start: event.get("start"),
                end: event.get("end"),
                all_day: event.get::<i64, _>("all_day") != 0,
                recurring: event.get::<String, _>("recurrence") != "[]",
                instances: events.by_series.get(&event.get::<String, _>("event_id")).cloned().unwrap_or_default(),
            });
        }
    }
    Ok(out)
}

fn invitation_facts(i: &Invitation) -> String {
    let mut out =
        format!("Calendar invitation: {}\nOrganizer: {}\n", i.title, if i.organizer.is_empty() { "not recorded" } else { &i.organizer });
    if i.optional {
        out.push_str("Your attendance: optional\n");
    }
    out.push_str(&format!("Your response: {}\n", i.response));
    if matches!(i.response.as_str(), "accepted" | "declined" | "tentative") {
        out.push_str("The owner has already answered this invitation in Calendar.\n");
    }
    out
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
        let mut seen = HashSet::new();
        for i in invitations {
            if !seen.insert((&i.account, &i.calendar, &i.event)) {
                continue;
            }
            out.push_str(&format!("- {} links to an event already in your calendar; your response: {}\n", i.target, i.response));
            for line in invitation_facts(i).lines() {
                out.push_str(&format!("  {line}\n"));
            }
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
    let mut samples = Vec::new();
    for to in [person, ""] {
        let mut rows = sqlx::query(MAIL_HISTORY)
            .bind(to)
            .bind(c6_core::now_ms() - VOICE_DAYS * DAY)
            .bind(MAIL_CANDIDATES)
            .bind(MAIL_WINDOW)
            .bind(true)
            .fetch(&m.store.pool);
        while let Some(row) = rows.try_next().await? {
            let body = mail_text(m, &row).await;
            if body.trim().is_empty() {
                continue;
            }
            samples.push(format!("---\n{}\n", clip(&clean(&body), VOICE_SAMPLE)));
            if samples.len() == 3 {
                break;
            }
        }
        if !samples.is_empty() || to.is_empty() {
            break;
        }
    }
    let chat: Vec<String> = sqlx::query_scalar(
        "SELECT body FROM items WHERE kind = 'chat' AND json_extract(meta, '$.from_me') = 1 ORDER BY created_at DESC LIMIT 5",
    )
    .fetch_all(&m.store.pool)
    .await?;
    for body in chat {
        let said = body.split_once(": ").map_or(body.as_str(), |(_, t)| t);
        samples.push(format!("--- (chat) {}\n", clip(&clean(said), 300)));
    }
    Ok(if samples.is_empty() { String::new() } else { format!("\n## How the owner writes (their recent messages)\n{}", samples.concat()) })
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
        sqlx::query("INSERT OR IGNORE INTO calendars (account, id, summary, is_primary) VALUES ('cal-acct', 'primary', '', 1)")
            .execute(&m.store.pool)
            .await
            .unwrap();
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

    async fn kept_mail(m: &Memex, id: &str, thread: &str, at: i64, parsed: Parsed, sent: bool) -> String {
        let stored = Stored {
            account: "mail-acct",
            external_id: id,
            thread_id: thread,
            source: "test-mail",
            rfc_id: id,
            origin: "mirrored",
            owner: "me@example.com",
            message: id,
        };
        crate::mail::store(m, &parsed, "", &stored, &[if sent { "SENT" } else { "INBOX" }.into()], at).await.unwrap()
    }

    #[tokio::test]
    async fn context_keeps_messages_and_calendar_facts_with_private_links_removed() {
        use chrono::TimeZone;
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        m.accounts.lock().unwrap().push(crate::people::Account { email: "me@example.com".into(), ..Default::default() });
        let tz = chrono_tz::Pacific::Honolulu;
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": tz.name()})).await.unwrap();
        let day = chrono::Utc::now().with_timezone(&tz).date_naive();
        let at = |days, hour, minute| {
            tz.from_local_datetime(&(day + chrono::Duration::days(days)).and_hms_opt(hour, minute, 0).unwrap())
                .single()
                .unwrap()
                .timestamp_millis()
        };
        let start = at(2, 17, 30);
        let end = at(2, 20, 0);
        crate::calendar::store_event(
            &m,
            "test-calendar",
            "cal-acct",
            "me@example.com",
            "primary",
            &json!({
                "id": "dinner", "summary": "Dinner by the bay", "organizer": {"email": "host@example.net"},
                "start": {"dateTime": chrono::DateTime::from_timestamp_millis(start).unwrap().to_rfc3339()},
                "end": {"dateTime": chrono::DateTime::from_timestamp_millis(end).unwrap().to_rfc3339()},
                "attendees": [{"email": "me@example.com", "self": true, "optional": true, "responseStatus": "accepted"}],
                "htmlLink": link("dinner", "me@example.com"),
            }),
        )
        .await
        .unwrap();
        let url = format!(
            "https://calendar.google.com/calendar/event?action=VIEW&eid={}&tok=invitation-secret&ctz=Pacific/Honolulu",
            eid(&link("dinner", "me@example.com")).unwrap()
        );
        let body = format!(
            "You have been invited to the following event.\n\nDinner by the bay\n\nWhen\nSaturday 03:30–06:00 UTC\n\nWhere\nHarbor House\n\nGuests\nhost@example.net - organizer\nme@example.com - optional\n\nView your event at {url}\nYes - Maybe - No\n\nInvitation from Google Calendar: {url}\nYou are receiving this email because you are subscribed to calendar notifications.\nForwarding this invitation could allow any recipient to send a response to the organizer.\nLearn more at https://support.google.com/calendar?answer=123\n"
        );
        let incoming = |subject: &str, body: &str| Parsed {
            from: ("host@example.net".into(), "Host".into()),
            to: vec![("me@example.com".into(), "Me".into())],
            subject: subject.into(),
            text: body.into(),
            ..Default::default()
        };
        let outgoing = |subject: &str, body: &str| Parsed {
            from: ("me@example.com".into(), "Me".into()),
            to: vec![("host@example.net".into(), "Host".into())],
            subject: subject.into(),
            text: body.into(),
            ..Default::default()
        };
        let invitation = kept_mail(
            &m,
            "invite",
            "invitation",
            at(0, 8, 0),
            Parsed {
                attachments: vec![json!({"name": "invite.ics", "mime": "text/calendar"})],
                ..incoming("Invitation: Dinner by the bay", &body)
            },
            false,
        )
        .await;
        for (n, subject, body) in [
            (0, "Fw: Payment receipt", "FORWARDED RECEIPT https://invoice.example.test/pay?auth_token=invoice-secret"),
            (1, "fWD: Offers", "FORWARDED MARKETING\u{034f}\u{00ad} invisible spacer sale"),
            (2, "Receipt", "AUTO GENERATED This is an automatically generated message about your payment."),
            (
                3,
                "Re: Dinner",
                "Thanks for inviting us. We are looking forward to dinner together.\n\nOn Monday, Host <host@example.net> wrote:\n> QUOTED EARLIER MESSAGE\n\n-- \nPRIVATE SIGNATURE\nSent from my phone",
            ),
        ] {
            kept_mail(&m, &format!("voice-{n}"), &format!("voice-{n}"), at(-200 - n, 10, 0), outgoing(subject, body), true).await;
        }
        let recent = kept_mail(
            &m,
            "recent",
            "ordinary",
            at(-1, 9, 0),
            incoming(
                "Harbor directions",
                "Meet us beside the blue gate. Can you bring chairs? Map: www.harbor.example.test/map?auth_token=map-secret#pin\u{034f}\u{00ad}",
            ),
            false,
        )
        .await;
        let old = kept_mail(&m, "old", "old-thread", at(-190, 10, 0), incoming("OLD THREAD", "OLD MAIL TEXT"), false).await;
        kept_mail(
            &m,
            "forward",
            "forward-thread",
            at(-1, 10, 0),
            incoming("Fwd: Receipt", "OTHER FORWARD https://invoice.example.test/pay?auth_token=forward-secret"),
            false,
        )
        .await;
        kept_mail(
            &m,
            "calendar",
            "calendar-thread",
            at(-1, 11, 0),
            Parsed {
                attachments: vec![json!({"name": "invite.ics", "mime": "text/calendar"})],
                ..incoming("CALENDAR NOTIFICATION", "The event has changed.")
            },
            false,
        )
        .await;
        let copied = kept_mail(
            &m,
            "copied",
            "copied-thread",
            at(-1, 12, 0),
            Parsed {
                from: ("third@example.test".into(), "Third".into()),
                to: vec![("me@example.com".into(), "Me".into())],
                cc: vec![("host@example.net".into(), "Host".into())],
                subject: "Third party work".into(),
                text: "Please send the third party report.".into(),
                ..Default::default()
            },
            false,
        )
        .await;
        let owner_copied = kept_mail(
            &m,
            "owner-copied",
            "owner-copied",
            at(-1, 13, 0),
            Parsed {
                to: vec![("third@example.test".into(), "Third".into())],
                cc: vec![("me@example.com".into(), "Me".into())],
                ..incoming("A separate plan", "Please arrange that with the other guest.")
            },
            false,
        )
        .await;
        for (who, what, due, created, item) in [
            ("host@example.net", "Bring the seating plan", Some(at(1, 12, 0)), at(0, 7, 0), &invitation),
            (
                "me@example.com",
                "Bring dessert https://menu.example.test/options?auth_token=menu-secret\u{034f}\u{00ad}",
                Some(at(1, 13, 0)),
                at(0, 7, 0),
                &recent,
            ),
            ("host@example.net", "STALE COMMITMENT", Some(at(-1000, 12, 0)), at(-1001, 7, 0), &invitation),
            ("host@example.net", "OLD UNDATED COMMITMENT", None, at(-100, 7, 0), &old),
            ("host@example.net", "PERSON OWES THIRD PARTY", Some(at(1, 12, 0)), at(0, 7, 0), &copied),
            ("host@example.net", "PERSON ON OWNER CC MAIL", Some(at(1, 12, 0)), at(0, 7, 0), &owner_copied),
            ("me@example.com", "OWNER ON OWNER CC MAIL", Some(at(1, 12, 0)), at(0, 7, 0), &owner_copied),
            ("third@example.test", "THIRD PARTY COMMITMENT", Some(at(1, 12, 0)), at(0, 7, 0), &copied),
            ("me@example.com", "OWNER COMMITMENT ON COPIED MAIL", Some(at(1, 12, 0)), at(0, 7, 0), &copied),
        ] {
            sqlx::query("INSERT INTO commitments (item_id, who, what, due, made_by, created_at) VALUES (?, ?, ?, ?, 'test', ?)")
                .bind(item)
                .bind(who)
                .bind(what)
                .bind(due)
                .bind(created)
                .execute(&m.store.pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO cards (key, name, card, threads, model, updated_at) VALUES ('person:host@example.net', 'Host', ?, 1, 'test', 0)")
            .bind(json!({"relationship": "Dinner friend\u{034f}", "story": "Plans at https://host.example.test/plans?auth_token=card-secret#details"}).to_string())
            .execute(&m.store.pool).await.unwrap();
        let request = api::Request {
            title: format!(
                "RSVP to dinner? <img src=\"image.png?auth_token=secret\"> <a href = 'pay?auth_token=secret'>Pay</a> https://r.test/https%3A%2F%2Fx.test%2F%3Ftok%3Dsecret {url}\u{034f}"
            ),
            person: "host@example.net".into(),
            person_name: "Host".into(),
            sources: vec![api::RequestSource { target: "thread:mail-acct:invitation".into(), kind: "mail".into(), ..Default::default() }],
            ..Default::default()
        };
        let text = context(&m, &request).await.unwrap();
        let (activity_title, _) = task_context(&text);
        assert!(activity_title.contains("RSVP to dinner?"));
        assert!(!clip(activity_title, 120).contains("secret"));
        assert!(!activity_title.contains("auth_token"));
        assert!(!activity_title.contains("%3F"));
        for wanted in [
            "You are receiving this email",
            "Forwarding this invitation",
            "Learn more",
            "Yes - Maybe - No",
            "AUTO GENERATED",
            "QUOTED EARLIER",
            "PRIVATE SIGNATURE",
            "Sent from my",
            "UTC",
            "Calendar invitation: Dinner by the bay",
            "Organizer: host@example.net",
            "Your attendance: optional",
            "Your response: accepted",
            "already answered this invitation in Calendar",
            "nothing else in your calendar overlaps it",
            "Thanks for inviting us. We are looking forward to dinner together.",
            "Bring the seating plan",
            "the owner: Bring dessert",
            "Harbor directions",
            "Can you bring chairs?",
            "www.harbor.example.test/map",
            "https://host.example.test/plans",
        ] {
            assert!(text.contains(wanted), "missing {wanted}: {text}");
        }
        for absent in [
            "tok=",
            "auth_token",
            "secret",
            "\u{034f}",
            "\u{00ad}",
            "FORWARDED",
            "STALE COMMITMENT",
            "OLD UNDATED",
            "THIRD PARTY COMMITMENT",
            "PERSON OWES THIRD PARTY",
            "PERSON ON OWNER CC MAIL",
            "OWNER ON OWNER CC MAIL",
            "OWNER COMMITMENT ON COPIED MAIL",
            "me@example.com:",
            "OLD THREAD",
            "OLD MAIL TEXT",
            "OTHER FORWARD",
            "CALENDAR NOTIFICATION",
            "\n\n\n",
        ] {
            assert!(!text.contains(absent), "unexpected {absent}: {text}");
        }
        let other = text.split_once("## Other recent mail with them\n").unwrap().1.split("\n## ").next().unwrap();
        assert!(!other.contains("Dinner by the bay"), "{other}");
        assert!(text.chars().count() <= MAX_CONTEXT);
        println!("INVENTED CONTEXT START\n{text}INVENTED CONTEXT END");
    }

    #[tokio::test]
    async fn unmatched_invitations_and_forwarded_text_keep_everything_but_link_tokens() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let body = "You have been invited to the following event.\n\nA new workshop\nWhen\nFriday afternoon\nWhere\nThe library\nView your event at https://calendar.google.com/calendar/event?eid=unmatched&tok=secret\nYes - Maybe - No\nInvitation from Google Calendar\nYou are receiving this email because you use Calendar.\nForwarding this invitation could allow other people to reply.\nLearn more";
        let r = message(&m, body, None).await;
        let text = context(&m, &r).await.unwrap();
        assert!(text.contains("A new workshop\nWhen\nFriday afternoon\nWhere\nThe library"), "{text}");
        assert!(text.contains("could allow") && text.contains("receiving this email") && text.contains("Yes - Maybe"), "{text}");
        assert!(!text.contains("secret") && !text.contains("tok="), "{text}");
        let r = message(&m, "Please check this invoice.\n---------- Forwarded message ---------\nInvoice amount: 42.\nhttps://invoice.example.test/pay?auth_token=secret", None).await;
        let text = context(&m, &r).await.unwrap();
        assert!(text.contains("Invoice amount: 42."), "{text}");
        assert!(!text.contains("auth_token") && !text.contains("secret"), "{text}");
    }

    #[tokio::test]
    async fn voice_uses_short_replies_before_falling_back_to_anyone() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        for (id, to, body) in
            [("short", "host@example.net", "Thanks!"), ("other", "other@example.test", "Here are my own words about the next steps.")]
        {
            kept_mail(
                &m,
                id,
                id,
                c6_core::now_ms(),
                Parsed {
                    subject: "Re: Next steps".into(),
                    from: ("me@example.com".into(), "Me".into()),
                    to: vec![(to.into(), String::new())],
                    text: body.into(),
                    ..Default::default()
                },
                true,
            )
            .await;
        }
        let text = voice(&m, "host@example.net").await.unwrap();
        assert!(text.contains("Thanks!"));
        assert!(!text.contains("Here are my own words about the next steps."));
        sqlx::query("DELETE FROM mail WHERE gmail_id = 'short'").execute(&m.store.pool).await.unwrap();
        assert!(voice(&m, "host@example.net").await.unwrap().contains("Here are my own words about the next steps."));
        sqlx::query("DELETE FROM mail WHERE gmail_id = 'other'").execute(&m.store.pool).await.unwrap();
        assert!(voice(&m, "host@example.net").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn optional_attendance_comes_only_from_calendar() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        m.accounts.lock().unwrap().push(crate::people::Account { email: "me@example.com".into(), ..Default::default() });
        let url = link("workshop", "me@example.com");
        event(&m, "workshop", c6_core::now_ms() + DAY, "accepted", &url).await;
        let r = message(&m, &format!("Your attendance is optional.\n{url}"), None).await;
        for (attendees, optional) in [
            (json!([]), false),
            (json!([{"self":true}]), false),
            (json!([{"self":true, "optional":false}]), false),
            (json!([{"self":true, "optional":true}]), true),
            (json!([{"email":"ME@example.com", "optional":true}]), true),
            (json!([{"email":"other@example.net", "optional":true}]), false),
        ] {
            sqlx::query("UPDATE events SET attendees = ?").bind(attendees.to_string()).execute(&m.store.pool).await.unwrap();
            let text = context(&m, &r).await.unwrap();
            assert_eq!(text.contains("Your attendance: optional"), optional, "{text}");
            assert_eq!(text.contains("Your attendance:"), optional, "{text}");
        }
    }

    #[tokio::test]
    async fn event_links_keep_human_prose_and_summarize_updates_once() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let now = c6_core::now_ms();
        let url = link("workshop", "me@example.com");
        event(&m, "workshop", now + DAY, "accepted", &url).await;
        let mut r = message(&m, &format!("Please send the slides for this workshop. {url}"), None).await;
        for n in 0..2 {
            kept_mail(
                &m,
                &format!("update-{n}"),
                if n == 0 { "thread" } else { "other" },
                now + n,
                Parsed {
                    from: ("host@example.net".into(), "Host".into()),
                    subject: "Updated invitation".into(),
                    text: format!("You have been invited to the following event.\nWorkshop\nInvitation from Google Calendar: {url}"),
                    ..Default::default()
                },
                false,
            )
            .await;
        }
        r.sources.push(api::RequestSource { target: "thread:mail-acct:other".into(), kind: "mail".into(), ..Default::default() });
        let text = context(&m, &r).await.unwrap();
        assert!(text.contains("Please send the slides for this workshop."), "{text}");
        assert_eq!(text.matches("You have been invited to the following event.").count(), 2, "{text}");
        assert_eq!(text.matches("Calendar invitation: Workshop").count(), 1, "{text}");
        assert_eq!(text.matches("links to an event already in your calendar").count(), 1, "{text}");
        assert_eq!(text.matches("nothing else in your calendar overlaps it").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn invitation_facts_survive_newer_long_messages() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let now = c6_core::now_ms();
        let url = link("workshop", "me@example.com");
        event(&m, "workshop", now + DAY, "accepted", &url).await;
        sqlx::query("UPDATE events SET organizer = 'host@example.net', attendees = ?")
            .bind(json!([{"self": true, "optional": true}]).to_string())
            .execute(&m.store.pool)
            .await
            .unwrap();
        let r =
            message(&m, &format!("You have been invited to the following event.\nWorkshop\nInvitation from Google Calendar {url}"), None)
                .await;
        sqlx::query("UPDATE mail SET date = ?").bind(now).execute(&m.store.pool).await.unwrap();
        for n in 1..=2 {
            kept_mail(
                &m,
                &format!("long-{n}"),
                "thread",
                now + n * 1000,
                Parsed {
                    from: ("host@example.net".into(), "Host".into()),
                    subject: "Further details".into(),
                    text: "More details. ".repeat(240),
                    ..Default::default()
                },
                false,
            )
            .await;
        }
        let text = context(&m, &r).await.unwrap();
        let (conversation, facts) = text.split_once("## Invitations being answered\n").unwrap();
        assert!(!conversation.contains("You have been invited to the following event."), "{text}");
        for fact in [
            "Calendar invitation: Workshop",
            "Organizer: host@example.net",
            "Your attendance: optional",
            "Your response: accepted",
            "  time: ",
            "nothing else in your calendar overlaps it",
        ] {
            assert!(facts.contains(fact), "missing {fact}: {text}");
        }
        assert!(text.chars().count() <= MAX_CONTEXT);
    }

    #[tokio::test]
    async fn history_bounds_the_index_walk_before_matching_recipients() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let now = c6_core::now_ms();
        kept_mail(
            &m,
            "older",
            "older",
            now - 1000,
            Parsed {
                from: ("me@example.com".into(), "Me".into()),
                to: vec![("host@example.net".into(), "Host".into())],
                subject: "Older qualifying mail".into(),
                text: "Here are my own words about our plans.".into(),
                ..Default::default()
            },
            true,
        )
        .await;
        sqlx::query(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < ?1) \
            INSERT INTO mail (item_id, account, gmail_id, thread_id, date, from_addr, to_json, blob) \
            SELECT 'other-' || x, 'mail-acct', 'other-' || x, 'other', ?2 + x, 'stranger@example.test', '[]', '' FROM n",
        )
        .bind(MAIL_WINDOW)
        .bind(now)
        .execute(&m.store.pool)
        .await
        .unwrap();
        for sent in [false, true] {
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {MAIL_HISTORY}")))
                .bind("host@example.net")
                .bind(now - VOICE_DAYS * DAY)
                .bind(MAIL_CANDIDATES)
                .bind(MAIL_WINDOW)
                .bind(sent)
                .fetch_all(&m.store.pool)
                .await
                .unwrap();
            let plan: Vec<String> = rows.iter().map(|r| r.get("detail")).collect();
            assert!(plan.iter().any(|line| line == "MATERIALIZE candidates"), "{plan:#?}");
            assert!(plan.iter().any(|line| line.starts_with("SEARCH mail USING INDEX mail_date (date>?")), "{plan:#?}");
            assert!(!plan.iter().any(|line| line.starts_with("SCAN mail")), "{plan:#?}");
            println!("MAIL HISTORY PLAN sent={sent}: {plan:#?}");
            let found = sqlx::query(MAIL_HISTORY)
                .bind("host@example.net")
                .bind(now - VOICE_DAYS * DAY)
                .bind(MAIL_CANDIDATES)
                .bind(MAIL_WINDOW)
                .bind(sent)
                .fetch_all(&m.store.pool)
                .await
                .unwrap();
            assert!(found.is_empty(), "recipient matching must not reach the 2001st row");
        }
        sqlx::query("DELETE FROM mail WHERE gmail_id = 'other-1'").execute(&m.store.pool).await.unwrap();
        let r = api::Request { person: "host@example.net".into(), ..Default::default() };
        let text = context(&m, &r).await.unwrap();
        assert!(text.contains("## Other recent mail"), "{text}");
        assert!(text.contains("## How the owner writes"), "{text}");
        assert!(text.contains("Here are my own words about our plans."), "{text}");
    }

    #[tokio::test]
    async fn conversation_budget_keeps_the_latest_message_in_date_order() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let now = c6_core::now_ms();
        for (n, body) in
            [(0, "Oldest material. ".repeat(400)), (1, "Middle material. ".repeat(400)), (2, "Please send the final slides?".into())]
        {
            kept_mail(
                &m,
                &format!("message-{n}"),
                "long",
                now + n,
                Parsed {
                    from: ("host@example.net".into(), "Host".into()),
                    subject: format!("Message {n}"),
                    text: body,
                    ..Default::default()
                },
                false,
            )
            .await;
        }
        let r = api::Request {
            sources: vec![api::RequestSource { target: "thread:mail-acct:long".into(), kind: "mail".into(), ..Default::default() }],
            ..Default::default()
        };
        let text = context(&m, &r).await.unwrap();
        let oldest = text.find("Oldest material.").unwrap();
        let middle = text.find("Middle material.").unwrap();
        let latest = text.find("Please send the final slides?").unwrap();
        assert!(oldest < middle && middle < latest, "{text}");
        assert!(text.chars().count() <= MAX_CONTEXT);
    }

    #[tokio::test]
    async fn mail_history_limits_candidates_before_reading_bodies() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let now = c6_core::now_ms();
        let mail = |body: &str| Parsed {
            from: ("me@example.com".into(), "Me".into()),
            to: vec![("host@example.net".into(), "Host".into())],
            subject: "Next steps".into(),
            text: body.into(),
            ..Default::default()
        };
        kept_mail(&m, "qualifying", "qualifying", now - 1000, mail("Here are my own words about our next steps."), true).await;
        for n in 0..MAIL_CANDIDATES {
            // Empty samples and the request's own thread use all the candidate slots.
            kept_mail(&m, &format!("empty-{n}"), "empty", now + n, mail(""), true).await;
        }
        let r = api::Request {
            person: "host@example.net".into(),
            sources: vec![api::RequestSource { target: "thread:mail-acct:empty".into(), kind: "mail".into(), ..Default::default() }],
            ..Default::default()
        };
        let text = context(&m, &r).await.unwrap();
        assert!(!text.contains("## Other recent mail"), "{text}");
        assert!(!text.contains("## How the owner writes"), "{text}");
        sqlx::query("DELETE FROM mail WHERE gmail_id = 'empty-0'").execute(&m.store.pool).await.unwrap();
        // Cheap filters don't consume the candidate budget.
        for n in 0..MAIL_CANDIDATES {
            kept_mail(
                &m,
                &format!("forward-{n}"),
                "forward",
                now + n,
                Parsed { subject: "fWd: Receipt".into(), ..mail("Forwarded receipt") },
                true,
            )
            .await;
        }
        let text = context(&m, &r).await.unwrap();
        assert!(text.contains("## Other recent mail"), "{text}");
        assert!(text.contains("## How the owner writes"), "{text}");
        assert!(text.contains("Here are my own words about our next steps."), "{text}");
        sqlx::query("UPDATE mail SET date = ? WHERE gmail_id = 'qualifying'").bind(now - 366 * DAY).execute(&m.store.pool).await.unwrap();
        let text = context(&m, &r).await.unwrap();
        assert!(!text.contains("## Other recent mail"), "{text}");
        assert!(!text.contains("## How the owner writes"), "{text}");
    }

    #[test]
    fn link_boundaries_keep_prose_and_balanced_paths() {
        assert_eq!(clean("https://x.test/p?name=one)&auth_token=SECRET"), "https://x.test/p");
        assert_eq!(clean("(see https://x.test/p?q=1) and more"), "(see https://x.test/p and more");
        assert_eq!(clean("https://en.wikipedia.org/wiki/Foo_(bar)"), "https://en.wikipedia.org/wiki/Foo_(bar)");
        for query in ["?", "#", "%3F", "%23"] {
            assert_eq!(clean(&format!("https://x.test/p{query}name=one]&auth_token=SECRET next")), "https://x.test/p next");
        }
        for (text, expected) in [
            ("See [plan](https://x.test/p?token=secret)—bring chairs.", "See [plan](https://x.test/p chairs."),
            ("[plan](https://x.test/p)?", "[plan](https://x.test/p)?"),
            ("https://en.wikipedia.org/wiki/Function_(mathematics)?oldid=123", "https://en.wikipedia.org/wiki/Function_(mathematics)"),
            ("(see https://x.test/p?t=1)", "(see https://x.test/p"),
            ("https://x.test/p?q=[1]&t=2.", "https://x.test/p"),
            ("Is it https://x.test/p?", "Is it https://x.test/p"),
            (r#"<a href="https://x.test/p?name=O'Reilly&amp;auth_token=secret">"#, r#"<a href="https://x.test/p">"#),
            (r#"<a href="https://x.test/p?name=one)&amp;auth_token=secret">"#, r#"<a href="https://x.test/p">"#),
            ("<https://x.test/p?name=O'Reilly&auth_token=secret>", "<https://x.test/p>"),
            ("<https://x.test/p?name=one)&auth_token=secret> after", "<https://x.test/p> after"),
            (r#"<input value='https://x.test/p?name=O"Reilly&auth_token=secret'>"#, "<input value='https://x.test/p'>"),
            (r#"<img data-src="image.png%23name=one)&auth_token=secret">"#, r#"<img data-src="image.png">"#),
            (r#"<a href="https://x.test/p?name=one) more&auth_token=secret">after"#, r#"<a href="https://x.test/p">after"#),
            ("[plan](https://x.test/a_(b[c])?q=[1])—bring chairs.", "[plan](https://x.test/a_(b[c]) chairs."),
            ("[see https://x.test/p?t=1]—bring chairs.", "[see https://x.test/p chairs."),
            ("[plan](/a_(b)?token=secret)—bring chairs.", "[plan](/a_(b) chairs."),
            ("https://x.test/a_(b) https://x.test/c[d]", "https://x.test/a_(b) https://x.test/c[d]"),
            ("https://x.test/p?q=(one(two))&t=2: next", "https://x.test/p next"),
            ("https://x.test/p?q=secret;!? next", "https://x.test/p next"),
            ("https://x.test/p?q=secret.) next", "https://x.test/p next"),
            (r#"<img data-src="image.png?auth_token=secret">"#, r#"<img data-src="image.png">"#),
            (r#"<a href="https://x.test/a_(b)?token=secret">"#, r#"<a href="https://x.test/a_(b)">"#),
            (r#"<a href="?">"#, r#"<a href="">"#),
            ("<a href=https://x.test/p?>", "<a href=https://x.test/p>"),
            ("<img srcset=https://x.test/p?>", "<img srcset=https://x.test/p>"),
            (r#"<a href="  https://x.test/p?token=secret">"#, r#"<a href="  https://x.test/p">"#),
            (r#"<img data-srcset="a_(b).png?t=1 1x, c[d].png?t=2 2x">"#, r#"<img data-srcset="a_(b).png">"#),
        ] {
            assert_eq!(clean(text), expected, "{text}");
        }
    }

    #[test]
    fn context_urls_keep_only_their_location() {
        for suffix in [
            "?filter[name]=a&auth_token=secret",
            "?filter={name}&auth_token=secret",
            "?filter=(name)&auth_token=secret",
            "%23access_token%3Dsecret",
            "%3fauth_token%3dsecret",
        ] {
            assert_eq!(clean(&format!("https://x.test/p{suffix}")), "https://x.test/p");
        }
        assert_eq!(clean("https://r.test/https%3A%2F%2Fx.test%2F%23access_token%3Dsecret"), "https://r.test/https%3A%2F%2Fx.test%2F");
        assert_eq!(clean("See (https://x.test/p?filter[name]=a&auth_token=secret)."), "See (https://x.test/p");
        for attr in [
            "href",
            "src",
            "data-src",
            "data-href",
            "data-url",
            "data_src",
            "xlink:href",
            "photo_url",
            "imageSrc",
            "action",
            "formaction",
            "poster",
            "cite",
            "background",
            "data",
        ] {
            for value in ["?auth_token=secret", "#secret", "%3Fauth_token=secret", "%23secret", "p?filter[name]=a&auth_token=secret"] {
                let path = if value.starts_with('p') { "p" } else { "" };
                assert_eq!(clean(&format!("<a {attr} = \"{value}\">")), format!("<a {attr} = \"{path}\">"));
            }
        }
        assert_eq!(clean(r#"<a HREF=?auth_token=secret>link</a>"#), r#"<a HREF=>link</a>"#);
        assert_eq!(
            clean(r#"<input value="C#developer" title="Who?Me" data-note="use href=?secret" alt="/Why?Me">"#),
            r#"<input value="C#developer" title="Who?Me" data-note="use href=?secret" alt="/Why?Me">"#
        );
        assert_eq!(clean(r#"<input value="https://x.test/p?filter[name]=a&secret=1">"#), r#"<input value="https://x.test/p">"#);
        assert_eq!(clean(r#"<img srcset="one.png?secret=1 1x, two.png%23secret 2x, three.png?secret=3 3x">"#), r#"<img srcset="one.png">"#);
        assert_eq!(clean(r#"<img srcset="one.png?secret=1, two.png?secret=2">"#), r#"<img srcset="one.png">"#);
        assert_eq!(clean(r#"<a ping="?secret=1 /two#secret https://x.test/three?secret=3">"#), r#"<a ping="">"#);

        assert_eq!(clean("Re:lunch? Time:17:30 Note:see"), "Re:lunch? Time:17:30 Note:see");
        assert_eq!(
            clean(r#"<p title = "Re:lunch? Time:17:30 Note:see">Lunch?</p> x=Ready?"#),
            r#"<p title = "Re:lunch? Time:17:30 Note:see">Lunch?</p> x=Ready?"#
        );
        assert_eq!(clean("https://r.test/https%3A%2F%2Fx.test%2F%3Ftok%3Dsecret"), "https://r.test/https%3A%2F%2Fx.test%2F");
        assert_eq!(clean("https://r.test/go%3ftok%3dsecret"), "https://r.test/go");
        assert_eq!(
            clean(
                r#"<img src="image.png?auth_token=secret"> <a href = "pay?auth_token=secret">Pay</a> <img data-src = 'image.png?auth_token=secret'>"#
            ),
            r#"<img src="image.png"> <a href = "pay">Pay</a> <img data-src = 'image.png'>"#
        );
        assert_eq!(clean("<a href=''>empty</a>"), "<a href=''>empty</a>");
        assert_eq!(clean("mailto:person@example.test?subject=secret"), "mailto:person@example.test");
        assert_eq!(
            clean("[invoice](/pay?auth_token=secret#part) <a HREF='pay?token=secret'>Pay</a> /map#secret"),
            "[invoice](/pay <a HREF='pay'>Pay</a> /map"
        );
        assert_eq!(
            clean(
                "[invoice](https://pay.example.test/invoice?auth_token=secret#part) <a href=\"https://calendar.google.com/calendar/event?eid=abc&amp;tok=secret\">event</a> www.example.test/map?token=secret#place"
            ),
            "[invoice](https://pay.example.test/invoice <a href=\"https://calendar.google.com/calendar/event\">event</a> www.example.test/map"
        );
        assert_eq!(
            clean("HTTPS://x.example/a?token=(secret)&next=private https://x.example/b#secret ftp://x.example/c?secret=1"),
            "https://x.example/a https://x.example/b ftp://x.example/c"
        );
    }

    #[test]
    fn context_characters_and_blank_lines_are_cleaned() {
        assert_eq!(clean("A\u{e0061}B"), "AB");
        let fillers =
            "\u{200b}\u{200c}\u{200d}\u{2060}\u{feff}\u{034f}\u{00ad}\u{180e}\u{115f}\u{1160}\u{3164}\u{ffa0}\u{2800}\u{17b4}\u{17b5}";
        assert_eq!(clean(&format!("a{fillers}b\n\n \n\nlast\n")), "ab\n\nlast\n");
    }

    #[test]
    fn section_budgets_keep_conversations_invitations_and_calendar() {
        let text = assemble(
            [
                ("## Request\nReply\n\n".into(), REQUEST_CONTEXT),
                ("## Conversation\nKeep the conversation\n".into(), CONVERSATION_CONTEXT),
                ("Card noise ".repeat(MAX_CONTEXT), CARD_CONTEXT),
                ("Mail noise ".repeat(MAX_CONTEXT), RECENT_CONTEXT),
                ("Commitment noise ".repeat(MAX_CONTEXT), COMMITMENT_CONTEXT),
                ("\n## Calendar\nKeep the calendar\n".into(), CALENDAR_CONTEXT),
                ("\n## Invitations\nKeep the invitation facts\n".into(), INVITATION_CONTEXT),
                ("Voice noise ".repeat(MAX_CONTEXT), VOICE_CONTEXT),
            ],
            &[],
        );
        for kept in ["Keep the conversation", "Keep the calendar", "Keep the invitation facts"] {
            assert!(text.contains(kept));
        }
        assert!(text.chars().count() <= MAX_CONTEXT);
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

        crate::calendar::store_calendar(&m, "other-acct", &json!({"id": "other", "primary": true})).await.unwrap();
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
            crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "America/Los_Angeles"}))
                .await
                .unwrap();
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
                "- thread:mail-acct:thread links to an event already in your calendar; your response: accepted\n  Calendar invitation: Workshop\n  Organizer: not recorded\n  Your response: accepted\n  The owner has already answered this invitation in Calendar.\n  time: {} to {} (UTC)\n",
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
                    assert!(
                        text.contains(&format!(
                            "  time: {} to {} (UTC; next occurrence of a recurring event)\n  nothing else in your calendar overlaps it\n",
                            at(start),
                            at(start + 3_600_000)
                        )),
                        "{text}"
                    );
                } else {
                    assert!(
                        text.contains("your response: accepted") && text.contains("  the invitation's time could not be checked\n"),
                        "{text}"
                    );
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
                assert!(text.contains("your response: accepted") && text.contains("  time: "), "{text}");
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
        assert!(invitation.contains("your response: accepted") && invitation.contains("  time: "), "{text}");
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
            crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": calendar, "primary": calendar == "primary", "timeZone": "UTC"}))
                .await
                .unwrap();
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

    #[test]
    fn apostrophes_inside_links_do_not_expose_queries() {
        for (text, expected) in [
            ("https://x.test/p?name=O'Reilly&auth_token=secret", "https://x.test/p"),
            ("See [text](https://x.test/p?name=O'Reilly&auth_token=secret).", "See [text](https://x.test/p"),
            ("See 'https://x.test/p?name=O'Reilly&auth_token=secret'.", "See 'https://x.test/p"),
            ("https://x.test/O'Reilly", "https://x.test/O'Reilly"),
        ] {
            assert_eq!(clean(text), expected);
        }
    }
    #[tokio::test]
    async fn reply_boxes_share_cleaned_messages_and_invitation_facts() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id": "primary", "primary": true, "timeZone": "Pacific/Honolulu"}))
            .await
            .unwrap();
        let url = "https://calendar.google.com/calendar/event?eid=d29ya3Nob3AgbWVAZXhhbXBsZS5jb20&tok=invite-secret&ctz=Pacific/Honolulu";
        crate::calendar::store_event(
            &m,
            "test-calendar",
            "cal-acct",
            "me@example.com",
            "primary",
            &json!({
                "id": "workshop", "summary": "Workshop", "htmlLink": url,
                "start": {"dateTime": "2030-11-01T17:30:00Z"}, "end": {"dateTime": "2030-11-01T18:30:00Z"},
                "organizer": {"email": "host@example.net"},
                "attendees": [{"email": "me@example.com", "self": true, "optional": true, "responseStatus": "accepted"}],
            }),
        )
        .await
        .unwrap();
        message(&m, &format!(
            "You have been invited to the following event.\nWorkshop\nWhen\nFriday afternoon\nGuests\nhost@example.net - organizer\nView your event at {url}\nInvitation from Google Calendar: {url}\nYou are receiving this email because you use Calendar."
        ), None).await;
        kept_mail(
            &m,
            "directions",
            "thread",
            c6_core::now_ms() + 1,
            Parsed {
                from: ("host@example.net".into(), "Host".into()),
                to: vec![
                    ("bob@example.net".into(), "Bob".into()),
                    ("jane@example.net".into(), "Doe, <Jane>".into()),
                    ("guest@example.net".into(), String::new()),
                ],
                cc: vec![("me@example.com".into(), "Me".into())],
                subject: "Directions".into(),
                text: "Please read the route at https://host.example.net/route?auth_token=route-secret#turns\u{034f}\u{00ad}".into(),
                ..Default::default()
            },
            false,
        )
        .await;
        kept_mail(
            &m,
            "plan",
            "thread",
            c6_core::now_ms() + 2,
            Parsed {
                from: ("host@example.net".into(), "Host".into()),
                to: (0..12).map(|n| (format!("person{n}@example.net"), format!("User {n}"))).collect(),
                subject: "Shared plan".into(),
                text: "[plan](/plan?token=secret)".into(),
                ..Default::default()
            },
            false,
        )
        .await;
        sqlx::query(
            "INSERT INTO cards (key, name, card, threads, model, updated_at) VALUES ('person:host@example.net', 'Host', ?, 1, 'test', 0)",
        )
        .bind(
            json!({"relationship": "Workshop friend\u{034f}", "story": "Plans at https://host.example.net/plans?auth_token=card-secret"})
                .to_string(),
        )
        .execute(&m.store.pool)
        .await
        .unwrap();
        crate::requests::follow(
            &m,
            &crate::requests::Found {
                target: "thread:mail-acct:thread",
                kind: "mail",
                category: "needs_you",
                reason: "Decision details https://host.example.net/decision?auth_token=reason-secret\u{00ad}",
                asks: "Review the invitation https://host.example.net/request?auth_token=title-secret\u{034f}",
                due: None,
                person: "host@example.net",
                person_name: "Host",
                title: "Workshop",
                at: c6_core::now_ms(),
                seen_mail: &[],
                urgent: false,
                invitation: false,
            },
            None,
        )
        .await
        .unwrap();
        let id = crate::requests::open_of(&m, "thread:mail-acct:thread").await.unwrap().unwrap();
        let own = "My link https://mine.example.net/reply?token=keep-this#here\u{034f}";
        for key in ["thread:mail-acct:thread".to_owned(), format!("request:{id}")] {
            let prompt = crate::compose::context(&m, &key, own, &[]).await.unwrap();
            let (context, draft) = prompt.split_once("## My draft so far\n").unwrap();
            for wanted in [
                "To: Bob <bob@example.net>, Doe, Jane <jane@example.net>, guest@example.net\nCc: Me <me@example.com>\nPlease read the route at https://host.example.net/route",
                "[plan](/plan",
                "You have been invited to the following event.\nWorkshop\nWhen\nFriday afternoon",
                "You are receiving this email because you use Calendar.",
                "## Invitations being answered",
                "Calendar invitation: Workshop",
                "Organizer: host@example.net",
                "Your attendance: optional",
                "Your response: accepted",
                "2030-11-01 07:30",
                "Pacific/Honolulu",
                "nothing else in your calendar overlaps it",
                "Workshop friend",
                "https://host.example.net/plans",
            ] {
                assert!(context.contains(wanted), "{key}: missing {wanted}: {context}");
            }
            for absent in ["tok=", "token", "-secret", "?", "\u{034f}", "\u{00ad}"] {
                assert!(!context.contains(absent), "{key}: contains {absent}: {context}");
            }
            let plan = context.split_once("— Shared plan\n").unwrap().1.split_once("[plan]").unwrap().0;
            let to = plan.lines().next().unwrap();
            assert!(to.contains("and ") && to.ends_with(" more"), "{to}");
            assert!(to.chars().count() <= 300, "{to}");
            assert_eq!(to.matches('<').count(), to.matches('>').count(), "{to}");
            assert!(!plan.contains("Cc:"), "{plan}");
            assert!(draft.starts_with(&format!("{own}\n")), "the owner's draft stays verbatim");
            if key.starts_with("request:") {
                assert_eq!(context.matches("## What they need").count(), 1);
                assert_eq!(context.matches("Review the invitation").count(), 1);
                assert!(context.contains("Decision details https://host.example.net/decision"));
                assert!(!context.contains("## Request"));
            }
        }
    }

    #[tokio::test]
    async fn undated_commitments_use_the_source_date() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        m.accounts.lock().unwrap().push(crate::people::Account { email: "me@example.com".into(), ..Default::default() });
        let now = c6_core::now_ms();
        for (id, days, due, what) in [
            ("old", 200, None, "Old undated promise"),
            ("recent", 10, None, "Recent undated promise"),
            ("dated", 200, Some(now + DAY), "Still due soon"),
        ] {
            let item = kept_mail(
                &m,
                id,
                id,
                now - days * DAY,
                Parsed {
                    from: ("host@example.net".into(), "Host".into()),
                    to: vec![("me@example.com".into(), "Me".into())],
                    subject: "Plans".into(),
                    text: "Here are the plans we discussed.".into(),
                    ..Default::default()
                },
                false,
            )
            .await;
            sqlx::query(
                "INSERT INTO commitments (item_id, who, what, due, made_by, created_at) VALUES (?, 'host@example.net', ?, ?, 'test', ?)",
            )
            .bind(item)
            .bind(what)
            .bind(due)
            .bind(now)
            .execute(&m.store.pool)
            .await
            .unwrap();
        }
        let r = api::Request { person: "host@example.net".into(), ..Default::default() };
        let text = context(&m, &r).await.unwrap();
        let open = text.split_once("## Open between you\n").unwrap().1.split("\n## ").next().unwrap();
        assert!(open.contains("Recent undated promise"), "{open}");
        assert!(open.contains("Still due soon"), "{open}");
        assert!(!open.contains("Old undated promise"), "{open}");
    }

    #[test]
    fn draft_targets_can_include_the_headings_kind() {
        let mail = api::RequestSource { target: "thread:mail-acct:thread".into(), kind: "mail".into(), ..Default::default() };
        let chat = api::RequestSource { target: "chat:workspace:channel:thread".into(), kind: "chat".into(), ..Default::default() };
        for sources in [[mail.clone(), chat.clone()], [chat, mail]] {
            for source in &sources {
                for target in [
                    source.target.clone(),
                    format!("  {} ({})  ", source.target, source.kind),
                    format!(" `{} ({})` ", source.target, source.kind),
                    format!("`{}` ({})", source.target, source.kind),
                ] {
                    assert_eq!(draft_target(&target, &sources), source.target, "{target}");
                }
            }
            assert_eq!(draft_target("unknown", &sources), sources.last().unwrap().target);
        }
        assert_eq!(draft_target("unknown", &[]), "");
    }
    async fn invitation_request(m: &Memex) -> i64 {
        invitation_request_with(m, true).await
    }

    async fn invitation_request_with(m: &Memex, invitation: bool) -> i64 {
        let at = sqlx::query_scalar("SELECT COALESCE(MAX(date), 0) FROM mail WHERE account = 'mail-acct' AND thread_id = 'thread'")
            .fetch_one(&m.store.pool)
            .await
            .unwrap();
        crate::requests::follow(
            m,
            &crate::requests::Found {
                target: "thread:mail-acct:thread",
                kind: "mail",
                category: "needs_you",
                reason: "Please RSVP",
                asks: "RSVP to Workshop",
                due: None,
                person: "host@example.net",
                person_name: "Host",
                title: "Workshop",
                at,
                seen_mail: &sqlx::query_scalar("SELECT item_id FROM mail WHERE account = 'mail-acct' AND thread_id = 'thread' AND sent = 0 AND draft = 0 AND trash = 0 ORDER BY item_id")
                    .fetch_all(&m.store.pool).await.unwrap(),
                urgent: true,
                invitation,
            },
            None,
        )
        .await
        .unwrap();
        sqlx::query_scalar("SELECT MAX(id) FROM requests").fetch_one(&m.store.pool).await.unwrap()
    }

    fn notification(url: &str) -> String {
        format!(
            "You have been invited to the following event.\nWorkshop\n\nWhen\nFriday afternoon\n\nGuests\nhost@example.net - organizer\nme@example.com\n\nView your event at {url}\nYes - Maybe - No\nInvitation from Google Calendar: {url}"
        )
    }

    fn title_notification(url: &str) -> String {
        format!(
            r#"Workshop
Tuesday Nov 3, 2026 ⋅ 5:30pm – 8pm
Hawaii-Aleutian Standard Time



Organizer
Alex Morgan
host@example.net

Guests
Sam Rivera
Jamie Chen
View all guest info
{url}

Reply for me@example.com and view more details
{url}
Your attendance is optional.

~~//~~
Invitation from Google Calendar: {url}

You are receiving this email because you are subscribed to calendar
notifications. To stop receiving these emails, go to
https://calendar.google.com/calendar/settings
change "Other notifications".

Forwarding this invitation could allow any recipient to send a response to
the organizer, be added to the guest list, invite others regardless of
their own invitation status, or modify your RSVP.

Learn more https://support.google.com/calendar/answer/37135
"#
        )
    }

    async fn response_event(m: &Memex, id: &str, start: i64, status: &str, link: &str, updated: i64) {
        let time = |ms| chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339();
        crate::calendar::store_event(
            m,
            "test-calendar",
            "cal-acct",
            "me@example.com",
            "primary",
            &json!({
                "id": id, "summary": "Workshop", "start": {"dateTime": time(start)},
                "end": {"dateTime": time(start + 3_600_000)}, "updated": time(updated), "htmlLink": link,
                "attendees": [{"email": "me@example.com", "self": true, "responseStatus": status}],
            }),
        )
        .await
        .unwrap();
    }

    async fn fresh_event(m: &Memex, id: &str, start: i64, status: &str, link: &str) {
        let updated: Option<i64> =
            sqlx::query_scalar("SELECT updated FROM events WHERE account = 'cal-acct' AND calendar_id = 'primary' AND event_id = ?")
                .bind(id)
                .fetch_optional(&m.store.pool)
                .await
                .unwrap();
        response_event(m, id, start, status, link, updated.unwrap_or_else(c6_core::now_ms)).await;
    }

    #[tokio::test]
    async fn calendar_responses_close_requests_before_notifications_or_drafts() {
        c6_llm::install_crypto_provider();
        for (status, reason) in [
            ("accepted", "You accepted it in Calendar."),
            ("declined", "You declined it in Calendar."),
            ("tentative", "You answered maybe in Calendar."),
        ] {
            for (before, current) in [(true, false), (false, false), (true, true), (false, true)] {
                let dir = tempfile::tempdir().unwrap();
                let m = Memex::open(dir.path(), Default::default()).await.unwrap();
                let url = link("workshop", "me@example.com");
                calendar_message(&m, &if current { title_notification(&url) } else { notification(&url) }, None).await;
                if before {
                    fresh_event(&m, "workshop", c6_core::now_ms() + DAY, status, &url).await;
                }
                m.brief_setting.lock().unwrap().urgent = before;
                let id = invitation_request(&m).await;
                if !before {
                    assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "open");
                    if status == "accepted" {
                        let key = format!("request:{id}");
                        crate::compose::set(&m, &key, "Looking forward to the workshop.", "session:s1").await.unwrap();
                        crate::compose::link(&m, &key, "s1").await.unwrap();
                    }
                    sweep_answered(&m).await.unwrap();
                    assert!(!m.calendar_changed.load(std::sync::atomic::Ordering::Acquire));
                    fresh_event(&m, "workshop", c6_core::now_ms() + DAY, status, &url).await;
                    assert!(m.calendar_changed.load(std::sync::atomic::Ordering::Acquire));
                    sweep_answered(&m).await.unwrap();
                }
                let r = crate::requests::get(&m, id).await.unwrap();
                assert_eq!(r.status, "done");
                assert_eq!(r.closed_by.as_deref(), Some("calendar"));
                assert_eq!(r.reason, reason);
                if !before && status == "accepted" {
                    let kept = crate::compose::get(&m, &format!("request:{id}")).await.unwrap();
                    assert_eq!(
                        (kept["text"].as_str(), kept["by"].as_str(), kept["session"].as_str()),
                        (Some("Looking forward to the workshop."), Some("session:s1"), Some("s1")),
                    );
                } else {
                    assert!(r.draft.is_none());
                }
                assert!(r.notified_at.is_none());
                let task = llm::Task { connection_id: "unused".into(), model: "unused".into(), effort: None };
                assert!(draft(&m, &task, id).await.unwrap_err().to_string().contains("answered in Calendar"));
                let calls: i64 =
                    sqlx::query_scalar("SELECT COALESCE(SUM(calls), 0) FROM model_calls").fetch_one(&m.store.pool).await.unwrap();
                assert_eq!(calls, 0);
            }
        }
    }

    #[tokio::test]
    async fn calendar_sweep_and_drafting_check_existing_requests() {
        c6_llm::install_crypto_provider();
        for path in ["restart", "step", "follow", "draft"] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let url = link("workshop", "me@example.com");
            calendar_message(&m, &notification(&url), None).await;
            fresh_event(&m, "workshop", c6_core::now_ms() + DAY, "needsAction", &url).await;
            let id = invitation_request(&m).await;
            sweep_answered(&m).await.unwrap();
            fresh_event(&m, "workshop", c6_core::now_ms() + DAY, "needsAction", &url).await;
            assert!(!m.calendar_changed.load(std::sync::atomic::Ordering::Acquire));
            fresh_event(&m, "workshop", c6_core::now_ms() + DAY, "accepted", &url).await;
            let m = if path == "restart" {
                m.store.pool.close().await;
                Memex::open(dir.path(), Default::default()).await.unwrap()
            } else {
                m
            };
            if path == "restart" {
                assert!(m.calendar_changed.load(std::sync::atomic::Ordering::Acquire));
                sweep_answered(&m).await.unwrap();
            } else if path == "follow" {
                assert_eq!(invitation_request(&m).await, id);
            } else if path == "draft" {
                let task = llm::Task { connection_id: "unused".into(), model: "unused".into(), effort: None };
                assert!(draft(&m, &task, id).await.unwrap_err().to_string().contains("answered in Calendar"));
            } else {
                m.set_tasks(crate::Tasks {
                    understanding: Some(llm::Task { connection_id: "unused".into(), model: "unused".into(), effort: None }),
                    per_day: 10,
                    ..Default::default()
                });
                assert!(step(&m).await.unwrap());
            }
            let r = crate::requests::get(&m, id).await.unwrap();
            assert_eq!(r.status, "done");
            assert!(r.draft.is_none());
            let calls: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(calls), 0) FROM model_calls").fetch_one(&m.store.pool).await.unwrap();
            assert_eq!(calls, 0);
        }
    }

    #[tokio::test]
    async fn calendar_closure_leaves_mixed_or_uncertain_requests_open() {
        c6_llm::install_crypto_provider();
        for case in [
            "needsAction",
            "human",
            "unmarked",
            "unreadable",
            "chat",
            "thread",
            "ambiguous",
            "unmatched",
            "extra link",
            "two events",
            "snoozed",
            "expired",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let url = link("workshop", "me@example.com");
            let mut text = notification(&url);
            if case == "extra link" {
                text.push_str(&format!("\n{}", link("unmatched", "me@example.com")));
            }
            calendar_message(&m, &text, None).await;
            let id = invitation_request(&m).await;
            if case == "unmarked" {
                sqlx::query("UPDATE requests SET invitation = 0 WHERE id = ?").bind(id).execute(&m.store.pool).await.unwrap();
            }
            if matches!(case, "human" | "thread" | "two events") {
                let text =
                    if case == "two events" { notification(&link("dinner", "me@example.com")) } else { "Please send the slides.".into() };
                kept_mail(
                    &m,
                    "question",
                    if case == "thread" { "other" } else { "thread" },
                    c6_core::now_ms() - 10 * DAY,
                    Parsed {
                        from: ("host@example.net".into(), "Host".into()),
                        subject: "Question".into(),
                        text,
                        attachments: if case == "two events" { vec![json!({"mime":"text/calendar"})] } else { vec![] },
                        ..Default::default()
                    },
                    false,
                )
                .await;
                // The human message is older than the drafting context's six-message window.
                if case == "human" {
                    for n in 0..7 {
                        kept_mail(
                            &m,
                            &format!("update-{n}"),
                            "thread",
                            c6_core::now_ms(),
                            Parsed {
                                from: ("host@example.net".into(), "Host".into()),
                                subject: "Updated invitation".into(),
                                attachments: vec![json!({"mime":"text/calendar", "name":"attachment"})],
                                text: notification(&url),
                                ..Default::default()
                            },
                            false,
                        )
                        .await;
                    }
                }
            }
            if matches!(case, "chat" | "thread") {
                let target = if case == "chat" { "chat:room" } else { "thread:mail-acct:other" };
                sqlx::query("INSERT INTO request_sources (request_id, target, kind, at, noted_at, seen_mail) VALUES (?, ?, ?, 0, 0, '[]')")
                    .bind(id)
                    .bind(target)
                    .bind(if case == "chat" { "chat" } else { "mail" })
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
            }
            if matches!(case, "snoozed" | "expired") {
                sqlx::query("UPDATE requests SET status = 'snoozed', until = ? WHERE id = ?")
                    .bind(c6_core::now_ms() + if case == "snoozed" { DAY } else { -DAY })
                    .bind(id)
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
            }
            if case == "unreadable" {
                sqlx::query("UPDATE mail SET text_blob = 'missing-invented-blob' WHERE thread_id = 'thread'")
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
            }
            if case != "unmatched" {
                fresh_event(&m, "workshop", c6_core::now_ms() + DAY, if case == "needsAction" { "needsAction" } else { "accepted" }, &url)
                    .await;
            }
            if case == "ambiguous" {
                fresh_event(&m, "copy", c6_core::now_ms() + DAY, "accepted", &url).await;
            }
            if case == "two events" {
                fresh_event(&m, "dinner", c6_core::now_ms() + DAY, "accepted", &link("dinner", "me@example.com")).await;
            }
            sweep_answered(&m).await.unwrap();
            let r = crate::requests::get(&m, id).await.unwrap();
            assert_eq!(r.status, if matches!(case, "expired" | "snoozed") { "done" } else { "open" }, "{case}");
            assert!(r.draft.is_none(), "{case}");
        }
    }

    #[tokio::test]
    async fn reinvitations_wait_for_a_current_calendar_answer() {
        c6_llm::install_crypto_provider();
        let date = 1_900_000_000_000;
        for (status, updated) in [("declined", date - 300_000), ("accepted", date - 300_000), ("accepted", 0)] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let url = link("workshop", "me@example.com");
            response_event(&m, "workshop", date + DAY, status, &url, updated).await;
            calendar_message(&m, &notification(&url), None).await;
            sqlx::query("UPDATE mail SET date = ?").bind(date).execute(&m.store.pool).await.unwrap();
            let id = invitation_request(&m).await;
            assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "open");
            sweep_answered(&m).await.unwrap();
            assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "open");
            if status == "declined" {
                response_event(&m, "workshop", date + DAY, "needsAction", &url, date).await;
                sweep_answered(&m).await.unwrap();
                assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "open");
            }
            // A newer timestamp alone must schedule a sweep, even if the answer is unchanged.
            assert!(!m.calendar_changed.load(std::sync::atomic::Ordering::Acquire));
            response_event(&m, "workshop", date + DAY, "accepted", &url, date + 60_000).await;
            assert!(m.calendar_changed.load(std::sync::atomic::Ordering::Acquire));
            sweep_answered(&m).await.unwrap();
            assert_eq!(crate::requests::get(&m, id).await.unwrap().closed_by.as_deref(), Some("calendar"));
        }
    }

    #[tokio::test]
    async fn invitation_freshness_uses_the_latest_message_without_an_allowance() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let date = 1_900_000_000_000;
        let url = link("workshop", "me@example.com");
        calendar_message(&m, &notification(&url), None).await;
        sqlx::query("UPDATE mail SET date = ?").bind(date - DAY).execute(&m.store.pool).await.unwrap();
        let id = invitation_request(&m).await;
        kept_mail(
            &m,
            "update",
            "thread",
            date,
            Parsed {
                from: ("host@example.net".into(), "Host".into()),
                subject: "Updated invitation".into(),
                attachments: vec![json!({"mime":"text/calendar", "name":"attachment"})],
                text: notification(&url),
                ..Default::default()
            },
            false,
        )
        .await;
        assert_eq!(invitation_request(&m).await, id);
        response_event(&m, "workshop", date + DAY, "accepted", &url, date - 1).await;
        sweep_answered(&m).await.unwrap();
        assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "open");
        response_event(&m, "workshop", date + DAY, "accepted", &url, date).await;
        sweep_answered(&m).await.unwrap();
        assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "done");
    }

    #[tokio::test]
    async fn calendar_close_requires_the_sources_and_status_that_were_checked() {
        c6_llm::install_crypto_provider();
        for change in ["added", "noted", "at", "seen", "unmarked", "done", "dropped", "unchanged"] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            calendar_message(&m, &notification(&link("workshop", "me@example.com")), None).await;
            let id = invitation_request(&m).await;
            let sources: Vec<(String, i64, i64, String)> =
                sqlx::query_as("SELECT target, at, noted_at, seen_mail FROM request_sources WHERE request_id = ? ORDER BY target")
                    .bind(id)
                    .fetch_all(&m.store.pool)
                    .await
                    .unwrap();
            // These changes happen after reading the sources and before the closing UPDATE.
            match change {
                "added" => {
                    sqlx::query(
                        "INSERT INTO request_sources (request_id, target, kind, at, noted_at, seen_mail) VALUES (?, 'chat:room', 'chat', 0, 0, '[]')",
                    )
                    .bind(id)
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
                }
                "unmarked" => {
                    sqlx::query("UPDATE requests SET invitation = 0 WHERE id = ?").bind(id).execute(&m.store.pool).await.unwrap();
                }
                "at" => {
                    sqlx::query("UPDATE request_sources SET at = at + 1 WHERE request_id = ?")
                        .bind(id)
                        .execute(&m.store.pool)
                        .await
                        .unwrap();
                }
                "seen" => {
                    sqlx::query("UPDATE request_sources SET seen_mail = '[]' WHERE request_id = ?")
                        .bind(id)
                        .execute(&m.store.pool)
                        .await
                        .unwrap();
                }
                "noted" => {
                    sqlx::query("UPDATE request_sources SET noted_at = noted_at + 1 WHERE request_id = ?")
                        .bind(id)
                        .execute(&m.store.pool)
                        .await
                        .unwrap();
                }
                "done" | "dropped" => {
                    crate::requests::close(&m, id, change, "you").await.unwrap();
                }
                _ => {}
            }
            let updated = c6_core::now_ms();
            response_event(&m, "workshop", updated + DAY, "accepted", &link("workshop", "me@example.com"), updated).await;
            let event = crate::requests::CalendarAnswer {
                account: "cal-acct".into(),
                calendar: "primary".into(),
                event: "workshop".into(),
                status: "accepted".into(),
                updated,
            };
            let closed = crate::requests::close_calendar(&m, id, "You accepted it in Calendar.", &sources, &event).await.unwrap();
            assert_eq!(closed, change == "unchanged", "{change}");
            let r = crate::requests::get(&m, id).await.unwrap();
            if change == "unchanged" {
                assert_eq!(r.closed_by.as_deref(), Some("calendar"));
            } else {
                assert_eq!(r.reason, "Please RSVP");
                assert_eq!(r.status, if matches!(change, "done" | "dropped") { change } else { "open" });
            }
        }
    }

    #[tokio::test]
    async fn a_closed_request_does_not_keep_a_late_draft() {
        c6_llm::install_crypto_provider();
        for status in ["open", "snoozed", "done", "dropped"] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            calendar_message(&m, "Please send the slides.", None).await;
            let id = invitation_request(&m).await;
            let old = api::RequestDraft { text: "The previous draft.".into(), at: 10, ..Default::default() };
            save_draft(&m, id, &old).await.unwrap();
            crate::requests::set(&m, id, status, Some(c6_core::now_ms() + DAY)).await.unwrap();
            let late = api::RequestDraft { text: "The late draft.".into(), at: 20, ..Default::default() };
            let saved = save_draft(&m, id, &late).await;
            let r = crate::requests::get(&m, id).await.unwrap();
            if matches!(status, "done" | "dropped") {
                assert!(!saved.unwrap());
                assert_eq!(r.draft, Some(old));
            } else {
                assert!(saved.unwrap());
                assert_eq!(r.draft, Some(late));
            }
        }
    }

    #[tokio::test]
    async fn calendar_close_rechecks_the_event_at_the_write() {
        c6_llm::install_crypto_provider();
        for change in ["status", "updated", "deleted", "unchanged"] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let url = link("workshop", "me@example.com");
            calendar_message(&m, &notification(&url), None).await;
            let id = invitation_request(&m).await;
            let updated = c6_core::now_ms();
            response_event(&m, "workshop", updated + DAY, "accepted", &url, updated).await;
            let sources =
                sqlx::query_as("SELECT target, at, noted_at, seen_mail FROM request_sources WHERE request_id = ? ORDER BY target")
                    .bind(id)
                    .fetch_all(&m.store.pool)
                    .await
                    .unwrap();
            let answer = crate::requests::CalendarAnswer {
                account: "cal-acct".into(),
                calendar: "primary".into(),
                event: "workshop".into(),
                status: "accepted".into(),
                updated,
            };
            match change {
                "status" => {
                    sqlx::query("UPDATE events SET my_status = 'needsAction'").execute(&m.store.pool).await.unwrap();
                }
                "updated" => {
                    sqlx::query("UPDATE events SET updated = updated + 1").execute(&m.store.pool).await.unwrap();
                }
                "deleted" => {
                    sqlx::query("DELETE FROM events").execute(&m.store.pool).await.unwrap();
                }
                _ => {}
            }
            assert_eq!(
                crate::requests::close_calendar(&m, id, "You accepted it in Calendar.", &sources, &answer).await.unwrap(),
                change == "unchanged"
            );
            assert_eq!(crate::requests::get(&m, id).await.unwrap().status, if change == "unchanged" { "done" } else { "open" });
        }
    }

    #[tokio::test]
    async fn a_calendar_closed_request_must_still_have_only_invitations() {
        c6_llm::install_crypto_provider();
        for change in ["chat", "human"] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let url = link("workshop", "me@example.com");
            calendar_message(&m, &notification(&url), None).await;
            fresh_event(&m, "workshop", c6_core::now_ms() + DAY, "accepted", &url).await;
            let id = invitation_request(&m).await;
            assert!(close_answered(&m, id).await.unwrap());
            if change == "chat" {
                sqlx::query("INSERT INTO request_sources (request_id, target, kind, at, noted_at, seen_mail) VALUES (?, 'chat:room', 'chat', 0, 0, '[]')")
                    .bind(id)
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
            } else {
                kept_mail(
                    &m,
                    "question",
                    "thread",
                    c6_core::now_ms(),
                    Parsed {
                        from: ("host@example.net".into(), "Host".into()),
                        subject: "Question".into(),
                        text: "Can you bring the projector?".into(),
                        ..Default::default()
                    },
                    false,
                )
                .await;
            }
            assert!(!close_answered(&m, id).await.unwrap());
        }
    }

    async fn calendar_message(m: &Memex, text: &str, html: Option<&str>) -> api::Request {
        let r = message(m, text, html).await;
        sqlx::query("UPDATE mail SET parts = ? WHERE account = 'mail-acct' AND thread_id = 'thread'")
            .bind(json!([{"mime":"text/calendar","name":"attachment"},{"mime":"application/ics","name":"invite.ics"}]).to_string())
            .execute(&m.store.pool)
            .await
            .unwrap();
        r
    }

    #[test]
    fn calendar_metadata_does_not_depend_on_the_message_layout() {
        for (sender, parts, expected) in [
            ("calendar-notification@google.com", "[]", true),
            ("host@example.net", r#"[{"mime":"text/calendar","name":"attachment"}]"#, true),
            ("host@example.net", r#"[{"mime":"application/ics","name":"invite.ICS"}]"#, true),
            ("host@example.net", r#"[{"mime":"text/plain","name":"note.txt"}]"#, false),
            ("host@example.net", "[]", false),
            ("host@example.net", "broken json", false),
        ] {
            assert_eq!(calendar_metadata(sender, parts), expected, "{sender}: {parts}");
        }
    }

    #[tokio::test]
    async fn invitation_facts_use_only_the_owners_primary_calendar() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id":"shared", "primary":false})).await.unwrap();
        crate::calendar::store_calendar(&m, "other-acct", &json!({"id":"shared", "primary":true})).await.unwrap();
        let url = link("workshop", "colleague@example.net");
        crate::calendar::store_event(
            &m,
            "test-calendar",
            "cal-acct",
            "me@example.com",
            "shared",
            &json!({
                "id":"workshop", "summary":"Workshop", "htmlLink":url,
                "start":{"dateTime":"2030-01-01T17:00:00Z"}, "end":{"dateTime":"2030-01-01T18:00:00Z"},
                "attendees":[{"email":"colleague@example.net", "self":true, "responseStatus":"accepted", "optional":true}]
            }),
        )
        .await
        .unwrap();
        for url in [&url, &link("workshop", "alias@example.net")] {
            let r = message(&m, url, None).await;
            assert!(invitations(&m, &r).await.unwrap().is_empty());
            let text = context(&m, &r).await.unwrap();
            assert!(!text.contains("## Invitations being answered"), "{text}");
            assert!(!text.contains("accepted"), "{text}");
            assert!(!text.contains("Your attendance:"), "{text}");
        }
        let r = message(&m, &link("workshop", "alias@example.net"), None).await;
        event(&m, "workshop", c6_core::now_ms() + DAY, "needsAction", &link("workshop", "me@example.com")).await;
        let found = invitations(&m, &r).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].calendar, "primary");
        assert_eq!(found[0].response, "not yet answered");
        assert!(!found[0].optional);
    }

    #[tokio::test]
    async fn calendar_answers_wait_until_triage_has_seen_the_latest_mail() {
        c6_llm::install_crypto_provider();
        for invitation in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let date = 1_900_000_000_000;
            let url = link("workshop", "me@example.com");
            calendar_message(&m, &notification(&url), None).await;
            sqlx::query("UPDATE mail SET date = ?").bind(date).execute(&m.store.pool).await.unwrap();
            let id = invitation_request(&m).await;
            kept_mail(
                &m,
                "new",
                "thread",
                date + 60_000,
                Parsed {
                    from: ("host@example.net".into(), "Host".into()),
                    text: if invitation { notification(&url) } else { format!("{url}\nAlso please bring the projector.") },
                    attachments: vec![json!({"mime":"application/ics", "name":"invite.ics"})],
                    ..Default::default()
                },
                false,
            )
            .await;
            response_event(&m, "workshop", date + DAY, "accepted", &url, date + 120_000).await;
            // The calendar sweep still runs when there is no model to judge the new mail.
            sweep_answered(&m).await.unwrap();
            assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "open");
            assert!(!close_answered(&m, id).await.unwrap());
            assert_eq!(invitation_request_with(&m, invitation).await, id);
            let r = crate::requests::get(&m, id).await.unwrap();
            assert_eq!(r.status, if invitation { "done" } else { "open" });
            let marked: bool =
                sqlx::query_scalar("SELECT invitation FROM requests WHERE id = ?").bind(id).fetch_one(&m.store.pool).await.unwrap();
            assert_eq!(marked, invitation);
        }
    }

    #[tokio::test]
    async fn calendar_close_rechecks_incoming_mail_at_the_write() {
        c6_llm::install_crypto_provider();
        for change in ["newer", "same time", "older", "late", "trash", "unseen", "removed", "ignored", "other thread", "unchanged"] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let date = 1_900_000_000_000;
            let url = link("workshop", "me@example.com");
            calendar_message(&m, &notification(&url), None).await;
            sqlx::query("UPDATE mail SET date = ?").bind(date).execute(&m.store.pool).await.unwrap();
            let pending = pending_mail(&m, &url, date - 1).await;
            let id = invitation_request(&m).await;
            if change == "unseen" {
                sqlx::query("UPDATE request_sources SET seen_mail = '[]' WHERE request_id = ?")
                    .bind(id)
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
            }
            let sources =
                sqlx::query_as("SELECT target, at, noted_at, seen_mail FROM request_sources WHERE request_id = ? ORDER BY target")
                    .bind(id)
                    .fetch_all(&m.store.pool)
                    .await
                    .unwrap();
            response_event(&m, "workshop", date + DAY, "accepted", &url, date + 120_000).await;
            let event = crate::requests::CalendarAnswer {
                account: "cal-acct".into(),
                calendar: "primary".into(),
                event: "workshop".into(),
                status: "accepted".into(),
                updated: date + 120_000,
            };
            // Mail changes after the snapshots were read, without touching the source or event.
            if matches!(change, "newer" | "same time" | "older" | "other thread") {
                kept_mail(
                    &m,
                    "new",
                    if change == "other thread" { "other" } else { "thread" },
                    date + match change {
                        "older" => -1,
                        "same time" => 0,
                        _ => 1,
                    },
                    Parsed {
                        from: ("host@example.net".into(), "Host".into()),
                        text: format!("{url}\nAlso please bring the projector."),
                        attachments: vec![json!({"mime":"application/ics", "name":"invite.ics"})],
                        ..Default::default()
                    },
                    false,
                )
                .await;
            } else if change == "late" {
                finish_mail(&m, &pending, date - 1).await;
            } else if change == "trash" {
                sqlx::query("UPDATE mail SET trash = 1").execute(&m.store.pool).await.unwrap();
            } else if change == "removed" {
                sqlx::query("DELETE FROM mail").execute(&m.store.pool).await.unwrap();
            } else if change == "ignored" {
                for flag in ["sent", "draft", "trash"] {
                    let item = kept_mail(
                        &m,
                        flag,
                        "thread",
                        date + 1,
                        Parsed { text: "An ignored message".into(), ..Default::default() },
                        flag == "sent",
                    )
                    .await;
                    sqlx::query("UPDATE mail SET draft = ?, trash = ? WHERE item_id = ?")
                        .bind(flag == "draft")
                        .bind(flag == "trash")
                        .bind(item)
                        .execute(&m.store.pool)
                        .await
                        .unwrap();
                }
            }
            let closed = crate::requests::close_calendar(&m, id, "You accepted it in Calendar.", &sources, &event).await.unwrap();
            let expected = matches!(change, "unchanged" | "ignored" | "other thread");
            assert_eq!(closed, expected, "{change}");
            assert_eq!(crate::requests::get(&m, id).await.unwrap().status, if expected { "done" } else { "open" }, "{change}");
        }
    }

    #[tokio::test]
    async fn a_colleagues_shared_calendar_cannot_close_a_request() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let url = link("workshop", "colleague@example.net");
        calendar_message(&m, &notification(&link("workshop", "me@example.com")), None).await;
        let id = invitation_request(&m).await;
        crate::calendar::store_calendar(&m, "cal-acct", &json!({"id":"shared", "primary":false})).await.unwrap();
        crate::calendar::store_event(
            &m,
            "test-calendar",
            "cal-acct",
            "me@example.com",
            "shared",
            &json!({
                "id":"workshop", "summary":"Workshop", "htmlLink":url, "updated":chrono::Utc::now().to_rfc3339(),
                "start":{"dateTime":"2030-01-01T17:00:00Z"}, "end":{"dateTime":"2030-01-01T18:00:00Z"},
                "attendees":[{"email":"colleague@example.net", "self":true, "responseStatus":"accepted", "optional":true}]
            }),
        )
        .await
        .unwrap();
        sweep_answered(&m).await.unwrap();
        assert!(!close_answered(&m, id).await.unwrap());
        assert_eq!(crate::requests::get(&m, id).await.unwrap().status, "open");
    }

    // Stage an item before its mail row, as mail::store does across its two writes.
    async fn pending_mail(m: &Memex, url: &str, date: i64) -> String {
        m.store
            .put(
                &api::MemexItemIn {
                    kind: "mail".into(),
                    source: "test-mail".into(),
                    external_id: "late".into(),
                    text: format!("{url}\nAlso bring the projector."),
                    created_at: Some(date),
                    ..Default::default()
                },
                "test-mail",
            )
            .await
            .unwrap()
    }

    async fn finish_mail(m: &Memex, item: &str, date: i64) {
        sqlx::query("INSERT INTO mail (item_id, account, gmail_id, thread_id, date, blob, parts) VALUES (?, 'mail-acct', 'late', 'thread', ?, '', ?)")
            .bind(item).bind(date).bind(json!([{"mime":"application/ics", "name":"invite.ics"}]).to_string())
            .execute(&m.store.pool).await.unwrap();
    }

    #[tokio::test]
    async fn calendar_answers_require_exactly_the_mail_triage_read() {
        c6_llm::install_crypto_provider();
        for change in ["late", "trash", "unseen", "unchanged"] {
            let dir = tempfile::tempdir().unwrap();
            let m = Memex::open(dir.path(), Default::default()).await.unwrap();
            let date = 1_900_000_000_000;
            let url = link("workshop", "me@example.com");
            calendar_message(&m, &notification(&url), None).await;
            sqlx::query("UPDATE mail SET date = ?").bind(date).execute(&m.store.pool).await.unwrap();
            let pending = pending_mail(&m, &url, date - 1).await;
            let id = invitation_request(&m).await;
            match change {
                "late" => finish_mail(&m, &pending, date - 1).await,
                "trash" => {
                    sqlx::query("UPDATE mail SET trash = 1").execute(&m.store.pool).await.unwrap();
                }
                "unseen" => {
                    sqlx::query("UPDATE request_sources SET seen_mail = '[]'").execute(&m.store.pool).await.unwrap();
                }
                _ => {}
            }
            response_event(&m, "workshop", date + DAY, "accepted", &url, date + 1).await;
            sweep_answered(&m).await.unwrap();
            assert_eq!(crate::requests::get(&m, id).await.unwrap().status, if change == "unchanged" { "done" } else { "open" }, "{change}");
        }
    }
}
