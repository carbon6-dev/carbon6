//! The contacts store: the contacts apps' accounts' contacts, as they
//! sync them (the People API's shape); each contact is a person in the
//! graph with all of their addresses. People you have written to (and
//! heard from) become candidates; confirmed ones (or ones passing your
//! rule) are added through the contacts app (docs/knowledge.md §6).

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::info;

use crate::Memex;
use crate::people;

/// Stores a contact: the row, and the person (all their addresses lead to
/// one thing).
pub async fn store(m: &Memex, account: &str, p: &Value) -> anyhow::Result<()> {
    let resource = p["resourceName"].as_str().unwrap_or_default();
    if p["metadata"]["deleted"].as_bool() == Some(true) {
        sqlx::query("DELETE FROM contacts WHERE account = ? AND resource = ?").bind(account).bind(resource).execute(&m.store.pool).await?;
        return Ok(());
    }
    let name = p["names"][0]["displayName"].as_str().unwrap_or("").to_owned();
    let emails: Vec<String> =
        p["emailAddresses"].as_array().into_iter().flatten().filter_map(|e| e["value"].as_str().map(|v| v.trim().to_lowercase())).collect();
    let phones: Vec<String> =
        p["phoneNumbers"].as_array().into_iter().flatten().filter_map(|e| e["value"].as_str().map(str::to_owned)).collect();
    let orgs: Vec<String> =
        p["organizations"].as_array().into_iter().flatten().filter_map(|o| o["name"].as_str().map(str::to_owned)).collect();
    let groups: Vec<String> = p["memberships"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g["contactGroupMembership"]["contactGroupResourceName"].as_str().map(str::to_owned))
        .collect();
    let mut thing: Option<String> = None;
    for e in &emails {
        let id = match &thing {
            None => people::person(&m.store, e, &name).await?,
            Some(t) => {
                // The same person by another address.
                m.store.add_key(&format!("person:{e}"), t).await?;
                t.clone()
            }
        };
        thing = Some(id);
    }
    if let Some(t) = &thing
        && !name.is_empty()
    {
        m.store.rename_if_unnamed(t, &name, emails.first().map(String::as_str).unwrap_or("")).await?;
    }
    sqlx::query(
        "INSERT INTO contacts (account, resource, etag, name, emails, phones, orgs, photo, groups, thing_id, updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(account, resource) DO UPDATE SET etag = excluded.etag, name = excluded.name, emails = excluded.emails, phones = excluded.phones, \
         orgs = excluded.orgs, photo = excluded.photo, groups = excluded.groups, thing_id = excluded.thing_id, updated = excluded.updated",
    )
    .bind(account)
    .bind(resource)
    .bind(p["etag"].as_str().unwrap_or(""))
    .bind(&name)
    .bind(json!(emails).to_string())
    .bind(json!(phones).to_string())
    .bind(json!(orgs).to_string())
    .bind(p["photos"][0]["url"].as_str().unwrap_or(""))
    .bind(json!(groups).to_string())
    .bind(&thing)
    .bind(c6_core::now_ms())
    .execute(&m.store.pool)
    .await?;
    Ok(())
}

pub async fn list(m: &Memex, q: &str) -> anyhow::Result<Vec<api::Contact>> {
    let like = format!("%{}%", q.to_lowercase());
    let rows = sqlx::query("SELECT * FROM contacts WHERE lower(name) LIKE ? OR emails LIKE ? ORDER BY name COLLATE NOCASE LIMIT 500")
        .bind(&like)
        .bind(&like)
        .fetch_all(&m.store.pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| api::Contact {
            account: r.get("account"),
            resource: r.get("resource"),
            name: r.get("name"),
            emails: serde_json::from_str(&r.get::<String, _>("emails")).unwrap_or_default(),
            phones: serde_json::from_str(&r.get::<String, _>("phones")).unwrap_or_default(),
            orgs: serde_json::from_str(&r.get::<String, _>("orgs")).unwrap_or_default(),
            photo: r.get("photo"),
            thing_id: r.get("thing_id"),
        })
        .collect())
}

/// People you have written to and heard from who are not contacts yet,
/// most written to first.
pub async fn candidates(m: &Memex) -> anyhow::Result<Vec<api::ContactCandidate>> {
    let me: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
    let rows = sqlx::query(
        "WITH sent_to AS (SELECT lower(json_extract(t.value, '$[0]')) AS addr, COUNT(*) AS n FROM mail, json_each(mail.to_json) t WHERE mail.sent = 1 GROUP BY addr), \
         heard AS (SELECT from_addr AS addr, COUNT(*) AS n, MAX(from_name) AS name FROM mail WHERE sent = 0 AND list = 0 GROUP BY from_addr) \
         SELECT s.addr AS addr, s.n AS sent, h.n AS received, h.name AS name FROM sent_to s JOIN heard h ON h.addr = s.addr \
         WHERE NOT EXISTS (SELECT 1 FROM contacts c, json_each(c.emails) e WHERE e.value = s.addr) ORDER BY s.n DESC LIMIT 200",
    )
    .fetch_all(&m.store.pool)
    .await?;
    Ok(rows
        .iter()
        .filter(|r| !me.contains(&r.get::<String, _>("addr")))
        .map(|r| api::ContactCandidate {
            email: r.get("addr"),
            name: r.get::<Option<String>, _>("name").unwrap_or_default(),
            sent: r.get::<i64, _>("sent") as u32,
            received: r.get::<i64, _>("received") as u32,
        })
        .collect())
}

/// Adds a contact through the contacts app that owns the account; it is
/// kept as it comes back.
pub async fn create(m: &Memex, account: &str, name: &str, email: &str, by: &str) -> anyhow::Result<api::Contact> {
    let a = m.account("contacts", account)?;
    m.app_call(&a, "create", json!({"account": a.id, "name": name, "email": email}), by).await?;
    info!(%email, "contact added");
    Ok(list(m, email).await?.into_iter().next().unwrap_or_default())
}

/// Your rule (Settings → Memex: people you have written to twice and
/// heard from) adds them to your contacts.
pub async fn write_back(m: &Memex) -> anyhow::Result<()> {
    let Some(a) = m.accounts().into_iter().find(|a| a.feeds.iter().any(|f| f == "contacts")) else { return Ok(()) };
    for c in candidates(m).await?.into_iter().filter(|c| c.sent >= 2) {
        if let Err(e) = create(m, &a.id, &c.name, &c.email, "you").await {
            tracing::warn!(email = %c.email, "adding a contact: {e:#}");
        }
    }
    Ok(())
}
