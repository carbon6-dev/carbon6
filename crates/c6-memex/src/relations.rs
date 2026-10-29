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

fn live_sql(domain: bool) -> String {
    let mine = "(sent = 1 OR lower(from_addr) IN (SELECT value FROM json_each(?2)))";
    // Trim back to the last @, including addresses with a quoted @ in the local part.
    let from = if domain {
        "instr(from_addr, '@') > 0 AND lower(substr(from_addr, length(rtrim(from_addr, replace(from_addr, '@', ''))) + 1)) = ?1"
    } else {
        "lower(from_addr) = ?1"
    };
    let to = if domain { "'%@' || ?1 || '\"%'" } else { "'%\"' || ?1 || '\"%'" };
    // The two outgoing branches are disjoint, and each has an index. Testing
    // recipients still reads JSON, but only on mail the owner sent.
    format!(
        "WITH theirs AS MATERIALIZED ( \
             SELECT account, thread_id, list, date, lower(from_addr) AS addr FROM mail \
             WHERE draft = 0 AND {from} AND NOT {mine} \
         ), outgoing AS ( \
             SELECT to_json, cc_json FROM mail WHERE draft = 0 AND sent = 1 \
             UNION ALL \
             SELECT to_json, cc_json FROM mail WHERE draft = 0 AND sent != 1 \
             AND lower(from_addr) IN (SELECT value FROM json_each(?2)) \
         ) \
         SELECT COUNT(*) AS received, COUNT(DISTINCT account || ':' || thread_id) AS threads, \
         COALESCE(SUM(list), 0) AS bulk, MIN(date) AS first_at, MAX(date) AS last_at, \
         (SELECT COUNT(*) FROM outgoing WHERE lower(to_json) LIKE {to} OR lower(cc_json) LIKE {to}) AS sent_to, \
         (SELECT COUNT(*) FROM (SELECT DISTINCT account, thread_id FROM theirs) t \
            WHERE EXISTS (SELECT 1 FROM mail y WHERE y.account = t.account AND y.thread_id = t.thread_id AND y.draft = 0 \
            AND (y.sent = 1 OR lower(y.from_addr) IN (SELECT value FROM json_each(?2))))) AS answered, \
         COUNT(DISTINCT addr) AS people FROM theirs"
    )
}

/// An address's or a domain's numbers, counted from the mail now (the
/// index may be a minute behind; a conversation is sorted with exact
/// history). `domain`: every address at it. Also how many addresses.
pub(crate) async fn live(m: &Memex, who: &str, domain: bool) -> anyhow::Result<(Counts, i64)> {
    let me = serde_json::to_string(&m.accounts().iter().map(|a| a.email.to_lowercase()).collect::<Vec<_>>())?;
    let r = sqlx::query(sqlx::AssertSqlSafe(live_sql(domain))).bind(who.to_lowercase()).bind(&me).fetch_one(&m.store.pool).await?;
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

    #[tokio::test]
    async fn live_keeps_every_count() {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        let rows = [
            ("home", "answered", "Ann@X.Example", 0, 0, 0, "", ""),
            ("home", "answered", "ann@x.example", 0, 0, 0, "", ""),
            ("home", "answered", "joann@x.example", 0, 0, 0, "", ""),
            ("home", "answered", "ME@Mine.Example", 1, 0, 0, "ANN@X.EXAMPLE,joann@x.example", "ann@x.example"),
            ("home", "unanswered", "ann@x.example", 0, 0, 0, "", ""),
            ("other", "answered", "ann@x.example", 0, 0, 0, "", ""),
            ("home", "draft-reply", "bob@x.example", 0, 0, 0, "", ""),
            ("home", "draft-reply", "me@mine.example", 1, 1, 0, "bob@x.example", "ann@x.example"),
            ("home", "list", "news@x.example", 0, 0, 1, "", ""),
            ("home", "draft", "ann@x.example", 0, 1, 1, "", ""),
            ("home", "cc", "Me@Mine.Example", 0, 0, 0, "other@else.example", "AnN@x.Example"),
            ("home", "owner", "ALIAS@X.Example", 0, 0, 0, "ann@x.example", "joann@x.example"),
            ("home", "flag", "ann@x.example", 1, 0, 0, "joann@x.example", ""),
            ("home", "provider", "friend@gmail.com", 0, 0, 0, "", ""),
            ("home", "provider", "another@GMAIL.COM", 0, 0, 1, "", ""),
            ("home", "provider", "me@mine.example", 0, 0, 0, "friend@gmail.com", "another@gmail.com"),
            ("home", "suffix", "ann@ax.example", 0, 0, 0, "", ""),
            ("home", "suffix", "me@mine.example", 1, 0, 0, "ann@ax.example", ""),
            ("home", "quoted", "\"ann@work\"@x.example", 0, 0, 0, "", ""),
            ("home", "no-at", "x.example", 0, 0, 0, "", ""),
            ("home", "empty", "", 0, 0, 0, "", ""),
            ("home", "unicode", "Änne@X.Example", 0, 0, 0, "", ""),
            ("a:b", "c", "ann@x.example", 0, 0, 0, "", ""),
            ("a", "b:c", "ann@x.example", 0, 0, 0, "", ""),
            ("a:b", "c", "me@mine.example", 0, 0, 0, "ann@x.example", ""),
            ("a", "b:c", "me@mine.example", 1, 0, 0, "ann@x.example", ""),
        ];
        let recipients =
            |s: &str| serde_json::to_string(&s.split(',').filter(|a| !a.is_empty()).map(|a| (a, "")).collect::<Vec<_>>()).unwrap();
        for (i, (account, thread, from, sent, draft, list, to, cc)) in rows.into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO mail (item_id, account, gmail_id, thread_id, date, from_addr, sent, draft, list, to_json, cc_json, blob) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '')",
            )
            .bind(i.to_string())
            .bind(account)
            .bind(i.to_string())
            .bind(thread)
            .bind(i as i64)
            .bind(from)
            .bind(sent)
            .bind(draft)
            .bind(list)
            .bind(recipients(to))
            .bind(recipients(cc))
            .execute(&m.store.pool)
            .await
            .unwrap();
        }
        for owners in [vec!["Me@Mine.Example", "alias@X.example"], vec![], vec!["Me@Mine.Example", "ANN@X.EXAMPLE"]] {
            *m.accounts.lock().unwrap() =
                owners.iter().map(|email| crate::people::Account { email: (*email).into(), ..Default::default() }).collect();
            let me = serde_json::to_string(&owners.iter().map(|a| a.to_lowercase()).collect::<Vec<_>>()).unwrap();
            for (who, domain) in [
                ("Ann@X.Example", false),
                ("joann@x.example", false),
                ("ann@ax.example", false),
                ("bob@x.example", false),
                ("news@x.example", false),
                ("ME@MINE.EXAMPLE", false),
                ("alias@x.example", false),
                ("friend@gmail.com", false),
                ("nobody@absent.example", false),
                ("", false),
                ("X.Example", true),
                ("ax.example", true),
                ("GMAIL.COM", true),
                ("mine.example", true),
                ("absent.example", true),
                ("", true),
            ] {
                let old = sqlx::query(sqlx::AssertSqlSafe(old_live_sql(domain)))
                    .bind(who.to_lowercase())
                    .bind(&me)
                    .fetch_one(&m.store.pool)
                    .await
                    .unwrap();
                assert_eq!(
                    live(&m, who, domain).await.unwrap(),
                    (counts(&old), old.get("people")),
                    "{who}, domain={domain}, owners={owners:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn live_searches_mail_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("memex.db")).await.unwrap();
        let mut conn = store.pool.acquire().await.unwrap();
        for index in ["mail_live_from", "mail_live_domain", "mail_live_sent"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP INDEX {index}"))).execute(&mut *conn).await.unwrap();
        }
        for domain in [false, true] {
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {}", old_live_sql(domain))))
                .bind(if domain { "x.example" } else { "ann@x.example" })
                .bind(r#"["me@mine.example"]"#)
                .fetch_all(&mut *conn)
                .await
                .unwrap();
            let plan: Vec<String> = rows.iter().map(|r| r.get("detail")).collect();
            assert!(plan.iter().any(|d| d.starts_with("SCAN mail")), "{plan:#?}");
            println!("Before, domain={domain}: {plan:#?}");
        }
        sqlx::raw_sql(include_str!("../migrations/0024_live_mail.sql")).execute(&mut *conn).await.unwrap();
        for domain in [false, true] {
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {}", live_sql(domain))))
                .bind(if domain { "x.example" } else { "ann@x.example" })
                .bind(r#"["me@mine.example"]"#)
                .fetch_all(&mut *conn)
                .await
                .unwrap();
            let plan: Vec<String> = rows.iter().map(|r| r.get("detail")).collect();
            let from_index = if domain { "mail_live_domain" } else { "mail_live_from" };
            for index in [from_index, "mail_live_from", "mail_live_sent"] {
                assert!(plan.iter().any(|d| d.starts_with(&format!("SEARCH mail USING INDEX {index} ("))), "{plan:#?}");
            }
            assert!(
                plan.iter().any(|d| d.starts_with("SEARCH y ") && d.contains("USING INDEX mail_thread (account=? AND thread_id=?)")),
                "{plan:#?}"
            );
            assert!(!plan.iter().any(|d| d.starts_with("SCAN mail") || d.starts_with("SCAN y")), "{plan:#?}");
            println!("After, domain={domain}: {plan:#?}");
        }
    }

    fn old_live_sql(domain: bool) -> String {
        let mine = "(sent = 1 OR lower(from_addr) IN (SELECT value FROM json_each(?2)))";
        // Theirs: from the address, or from anyone at the domain.
        let from = if domain { "lower(from_addr) LIKE '%@' || ?1" } else { "lower(from_addr) = ?1" };
        // Yours to them: in to or cc.
        let to = if domain { "'%@' || ?1 || '\"%'" } else { "'%\"' || ?1 || '\"%'" };
        format!(
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
        )
    }

    #[test]
    fn who_gets_a_card() {
        let c = |sent_to, received, threads, bulk| Counts { sent_to, received, threads, bulk, ..Default::default() };
        let card = crate::cards::worth_a_card;
        assert!(card("ana@acme.com", &c(1, 0, 0, 0)), "you wrote to her");
        assert!(card("sam@growfast.io", &c(0, 4, 3, 0)), "three hand-written threads");
        assert!(!card("sam@growfast.io", &c(0, 1, 1, 0)), "one thread");
        assert!(!card("news@weekly.dev", &c(0, 9, 9, 9)), "a list");
        assert!(!card("team@comms.trinet.com", &c(0, 5, 5, 0)), "a bulk-sending subdomain");
        assert!(!card("noreply@x.io", &c(2, 5, 5, 0)), "automated");
    }

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
