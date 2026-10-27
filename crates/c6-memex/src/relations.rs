//! Who you correspond with (docs/relationships.md §2.1): the participant
//! index. For each address, the messages each way, threads, how many you
//! answered, when, and how much came in bulk; counted from the mail, no
//! model. Rebuilt in one pass when mail has changed, at most once a minute.

use sqlx::Row;

use crate::Memex;

/// Domains anyone can have an address at: knowing one person there says
/// nothing about the next.
pub const SHARED_DOMAINS: [&str; 14] = [
    "gmail.com",
    "googlemail.com",
    "yahoo.com",
    "hotmail.com",
    "outlook.com",
    "live.com",
    "icloud.com",
    "me.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "gmx.com",
    "fastmail.com",
    "hey.com",
];

/// First labels of the subdomains bulk mail is sent from.
const SENDING: [&str; 22] = [
    "e",
    "em",
    "email",
    "emails",
    "mail",
    "mailer",
    "mailing",
    "news",
    "newsletter",
    "updates",
    "update",
    "info",
    "marketing",
    "go",
    "comms",
    "t",
    "ma",
    "mg",
    "send",
    "engage",
    "hello",
    "notify",
];

/// Parts of an address that say no person reads it.
const AUTOMATED: [&str; 9] =
    ["noreply", "no-reply", "donotreply", "do-not-reply", "notification", "notifications", "alert", "mailer-daemon", "bounce"];

/// How often the index may be rebuilt.
const EVERY: i64 = 60_000;

pub fn domain(addr: &str) -> String {
    addr.rsplit_once('@').map(|(_, d)| d.to_lowercase()).unwrap_or_default()
}

pub fn shared(domain: &str) -> bool {
    SHARED_DOMAINS.contains(&domain)
}

/// Whether mail from this address is sent from a bulk-sending subdomain
/// (e.mycompany.com, updates.vendor.io).
pub fn sending_subdomain(addr: &str) -> bool {
    let d = domain(addr);
    let labels: Vec<&str> = d.split('.').collect();
    labels.len() >= 3 && SENDING.contains(&labels[0])
}

pub fn automated(addr: &str) -> bool {
    let local = addr.split('@').next().unwrap_or_default().to_lowercase();
    AUTOMATED.iter().any(|a| local.contains(a))
}

/// One address's numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counts {
    pub received: i64,
    pub sent_to: i64,
    pub threads: i64,
    pub answered: i64,
    pub bulk: i64,
    pub first_at: Option<i64>,
    pub last_at: Option<i64>,
}

/// Rebuilds the index if mail changed since it was built (and a minute has
/// passed). Whether it did.
pub async fn refresh(m: &Memex) -> anyhow::Result<bool> {
    let now = c6_core::now_ms();
    let (count, latest): (i64, Option<i64>) = sqlx::query_as("SELECT COUNT(*), MAX(date) FROM mail").fetch_one(&m.store.pool).await?;
    let mark = format!("{count}:{}", latest.unwrap_or(0));
    let state: Option<(String, i64)> =
        sqlx::query_as("SELECT mark, at FROM correspondents_built WHERE id = 1").fetch_optional(&m.store.pool).await?;
    if let Some((built, at)) = &state
        && (*built == mark || now - at < EVERY)
    {
        return Ok(false);
    }
    rebuild(m).await?;
    sqlx::query("INSERT INTO correspondents_built (id, mark, at) VALUES (1, ?, ?) ON CONFLICT(id) DO UPDATE SET mark = excluded.mark, at = excluded.at")
        .bind(&mark)
        .bind(now)
        .execute(&m.store.pool)
        .await?;
    Ok(true)
}

/// Counts everything again from the mail table, in one transaction.
pub async fn rebuild(m: &Memex) -> anyhow::Result<()> {
    let me: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
    let me = serde_json::to_string(&me)?;
    // Yours: marked sent, or from one of your addresses.
    let mine = "(m.sent = 1 OR lower(m.from_addr) IN (SELECT value FROM json_each(?1)))";
    let mut tx = m.store.pool.begin().await?;
    sqlx::query("DELETE FROM correspondents").execute(&mut *tx).await?;
    // What they sent you.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO correspondents (addr, domain, name, received, sent_to, threads, answered, bulk, first_at, last_at) \
         SELECT lower(m.from_addr), '', MAX(m.from_name), COUNT(*), 0, COUNT(DISTINCT m.account || ':' || m.thread_id), 0, SUM(m.list), \
         MIN(m.date), MAX(m.date) FROM mail m WHERE m.draft = 0 AND m.from_addr != '' AND NOT {mine} GROUP BY lower(m.from_addr)"
    )))
    .bind(&me)
    .execute(&mut *tx)
    .await?;
    // What you sent them (to or cc).
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO correspondents (addr, domain, name, received, sent_to, threads, answered, bulk, first_at, last_at) \
         SELECT lower(json_extract(r.value, '$[0]')) AS a, '', MAX(json_extract(r.value, '$[1]')), 0, COUNT(*), 0, 0, 0, MIN(m.date), MAX(m.date) \
         FROM mail m, json_each(m.to_json) r WHERE m.draft = 0 AND {mine} GROUP BY a \
         ON CONFLICT(addr) DO UPDATE SET sent_to = sent_to + excluded.sent_to, \
         first_at = MIN(first_at, excluded.first_at), last_at = MAX(last_at, excluded.last_at)"
    )))
    .bind(&me)
    .execute(&mut *tx)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO correspondents (addr, domain, name, received, sent_to, threads, answered, bulk, first_at, last_at) \
         SELECT lower(json_extract(r.value, '$[0]')) AS a, '', MAX(json_extract(r.value, '$[1]')), 0, COUNT(*), 0, 0, 0, MIN(m.date), MAX(m.date) \
         FROM mail m, json_each(m.cc_json) r WHERE m.draft = 0 AND {mine} GROUP BY a \
         ON CONFLICT(addr) DO UPDATE SET sent_to = sent_to + excluded.sent_to, \
         first_at = MIN(first_at, excluded.first_at), last_at = MAX(last_at, excluded.last_at)"
    )))
    .bind(&me)
    .execute(&mut *tx)
    .await?;
    // Their threads you wrote in too.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE correspondents SET answered = x.n FROM (SELECT t.a, COUNT(*) AS n FROM \
         (SELECT DISTINCT lower(m.from_addr) AS a, m.account, m.thread_id FROM mail m WHERE m.draft = 0 AND NOT {mine}) t \
         JOIN (SELECT DISTINCT m.account, m.thread_id FROM mail m WHERE m.draft = 0 AND {mine}) y \
         ON y.account = t.account AND y.thread_id = t.thread_id GROUP BY t.a) x WHERE correspondents.addr = x.a"
    )))
    .bind(&me)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE correspondents SET domain = lower(substr(addr, instr(addr, '@') + 1))").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM correspondents WHERE addr = '' OR instr(addr, '@') = 0 OR addr IN (SELECT value FROM json_each(?))")
        .bind(&me)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

fn counts(r: &sqlx::sqlite::SqliteRow) -> Counts {
    Counts {
        received: r.get::<Option<i64>, _>("received").unwrap_or(0),
        sent_to: r.get::<Option<i64>, _>("sent_to").unwrap_or(0),
        threads: r.get::<Option<i64>, _>("threads").unwrap_or(0),
        answered: r.get::<Option<i64>, _>("answered").unwrap_or(0),
        bulk: r.get::<Option<i64>, _>("bulk").unwrap_or(0),
        first_at: r.get("first_at"),
        last_at: r.get("last_at"),
    }
}

/// An address's numbers (zero when the index doesn't know it).
pub async fn of(m: &Memex, addr: &str) -> anyhow::Result<Counts> {
    let r = sqlx::query("SELECT * FROM correspondents WHERE addr = ?").bind(addr.to_lowercase()).fetch_optional(&m.store.pool).await?;
    Ok(r.as_ref().map(counts).unwrap_or_default())
}

/// A domain's numbers (its addresses together) and how many addresses;
/// None for a shared domain.
pub async fn of_domain(m: &Memex, domain: &str) -> anyhow::Result<Option<(Counts, i64)>> {
    if domain.is_empty() || shared(domain) {
        return Ok(None);
    }
    let r = sqlx::query(
        "SELECT SUM(received) AS received, SUM(sent_to) AS sent_to, SUM(threads) AS threads, SUM(answered) AS answered, \
         SUM(bulk) AS bulk, MIN(first_at) AS first_at, MAX(last_at) AS last_at, COUNT(*) AS people FROM correspondents WHERE domain = ?",
    )
    .bind(domain)
    .fetch_one(&m.store.pool)
    .await?;
    Ok(Some((counts(&r), r.get("people"))))
}

fn day(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn times(n: i64) -> String {
    if n == 1 { "once".into() } else { format!("{n} times") }
}

/// An address's or a domain's numbers, counted from the mail now (the
/// index may be a minute behind; a conversation is sorted with exact
/// history). `domain`: every address at it. Also how many addresses.
pub(crate) async fn live(m: &Memex, who: &str, domain: bool) -> anyhow::Result<(Counts, i64)> {
    let me = serde_json::to_string(&m.accounts().iter().map(|a| a.email.to_lowercase()).collect::<Vec<_>>())?;
    let mine = "(sent = 1 OR lower(from_addr) IN (SELECT value FROM json_each(?2)))";
    // Theirs: from the address, or from anyone at the domain.
    let from = if domain { "lower(from_addr) LIKE '%@' || ?1" } else { "lower(from_addr) = ?1" };
    // Yours to them: in to or cc.
    let to = if domain { "'%@' || ?1 || '\"%'" } else { "'%\"' || ?1 || '\"%'" };
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT \
         (SELECT COUNT(*) FROM mail WHERE draft = 0 AND {from} AND NOT {mine}) AS received, \
         (SELECT COUNT(DISTINCT account || ':' || thread_id) FROM mail WHERE draft = 0 AND {from} AND NOT {mine}) AS threads, \
         (SELECT COALESCE(SUM(list), 0) FROM mail WHERE draft = 0 AND {from} AND NOT {mine}) AS bulk, \
         (SELECT MIN(date) FROM mail WHERE draft = 0 AND {from} AND NOT {mine}) AS first_at, \
         (SELECT MAX(date) FROM mail WHERE draft = 0 AND {from} AND NOT {mine}) AS last_at, \
         (SELECT COUNT(*) FROM mail WHERE draft = 0 AND {mine} AND (lower(to_json) LIKE {to} OR lower(cc_json) LIKE {to})) AS sent_to, \
         (SELECT COUNT(*) FROM (SELECT DISTINCT account, thread_id FROM mail WHERE draft = 0 AND {from} AND NOT {mine}) t \
            WHERE EXISTS (SELECT 1 FROM mail y WHERE y.account = t.account AND y.thread_id = t.thread_id AND y.draft = 0 \
            AND (y.sent = 1 OR lower(y.from_addr) IN (SELECT value FROM json_each(?2))))) AS answered, \
         (SELECT COUNT(DISTINCT lower(from_addr)) FROM mail WHERE draft = 0 AND {from} AND NOT {mine}) AS people"
    )))
    .bind(who.to_lowercase())
    .bind(&me)
    .fetch_one(&m.store.pool)
    .await?;
    Ok((counts(&r), r.get("people")))
}

/// The history with a sender, as the sorting and drafting prompts get it:
/// counted from the owner's mail, with what the rules see (bulk, automated).
pub async fn history(m: &Memex, addr: &str, name: &str) -> anyhow::Result<String> {
    let addr = addr.to_lowercase();
    let (c, _) = live(m, &addr, false).await?;
    let who = if name.is_empty() { addr.clone() } else { format!("{name} <{addr}>") };
    let mut lines = vec![format!(
        "- The owner wrote to {addr} {}; {who} wrote {} message{} in {} thread{}{}; the owner answered in {} of those threads.",
        times(c.sent_to),
        c.received,
        if c.received == 1 { "" } else { "s" },
        c.threads,
        if c.threads == 1 { "" } else { "s" },
        if c.received > 0 { format!(", from {} to {}", day(c.first_at), day(c.last_at)) } else { String::new() },
        c.answered
    )];
    let d = domain(&addr);
    if shared(&d) {
        lines.push(format!("- {d} is a shared mail provider: the address says nothing about a company."));
    } else if !d.is_empty() {
        let (dc, people) = live(m, &d, true).await?;
        lines.push(format!(
            "- At {d} ({people} sender{}): the owner wrote {}, they wrote {} message{} in {} thread{}, answered in {}.",
            if people == 1 { "" } else { "s" },
            times(dc.sent_to),
            dc.received,
            if dc.received == 1 { "" } else { "s" },
            dc.threads,
            if dc.threads == 1 { "" } else { "s" },
            dc.answered
        ));
    }
    if c.received > 0 && c.bulk > 0 {
        lines.push(format!("- {} of their {} messages came with mailing-list headers (bulk-sent).", c.bulk, c.received));
    }
    if sending_subdomain(&addr) {
        lines.push(format!("- {d} is a bulk-sending subdomain."));
    }
    if automated(&addr) {
        lines.push("- The address is an automated one (no person reads it).".into());
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn senders() {
        assert!(sending_subdomain("marc@updates.papermark.com"));
        assert!(sending_subdomain("ca@e.myfilingservices.com"));
        assert!(!sending_subdomain("ana@acme.com"));
        assert!(!sending_subdomain("bo@mail.com"), "a provider, not a subdomain");
        assert!(automated("notifications@github.com") && automated("no-reply@x.io"));
        assert!(!automated("ana@acme.com"));
        assert_eq!(domain("Ana@Acme.COM"), "acme.com");
    }
}
