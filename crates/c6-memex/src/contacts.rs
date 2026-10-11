//! Google Contacts: synced both ways (People API, sync tokens); each
//! contact is a person in the graph with all of their addresses. People
//! you have written to (and heard from) become candidates; confirmed ones
//! (or ones passing your rule) are written back to Google, in a contact
//! group named carbon6.

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::info;

use crate::Memex;
use crate::google::{self, Account, status_of};

const FIELDS: &str = "names,emailAddresses,phoneNumbers,organizations,photos,memberships";
const GROUP: &str = "carbon6";

pub async fn sync(m: &Memex, acct: &Account) -> anyhow::Result<()> {
    let mut st = google::state(&m.store, &acct.id, "contacts").await?;
    let mut page: Option<String> = None;
    loop {
        let mut q = vec![("personFields", FIELDS.to_owned()), ("pageSize", "1000".to_owned()), ("requestSyncToken", "true".to_owned())];
        if let Some(t) = &st.cursor {
            q.push(("syncToken", t.clone()));
        }
        if let Some(p) = &page {
            q.push(("pageToken", p.clone()));
        }
        let v = match m.google.get(&acct.id, &m.google.people_url("/v1/people/me/connections"), &q).await {
            Ok(v) => v,
            Err(e) if matches!(status_of(&e), Some(400 | 410)) && st.cursor.is_some() => {
                st = google::SyncState { phase: "import".into(), ..Default::default() };
                google::save_state(&m.store, &acct.id, "contacts", &st, None).await?;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for p in v["connections"].as_array().into_iter().flatten() {
            store(m, acct, p).await?;
            st.done += 1;
        }
        page = v["nextPageToken"].as_str().map(str::to_owned);
        if page.is_none() {
            st.cursor = v["nextSyncToken"].as_str().map(str::to_owned).or(st.cursor);
            st.phase = "live".into();
            break;
        }
    }
    google::save_state(&m.store, &acct.id, "contacts", &st, None).await?;
    if m.contacts_auto() {
        for c in candidates(m).await?.into_iter().filter(|c| c.sent >= 2) {
            if let Err(e) = create(m, &acct.id, &c.name, &c.email).await {
                tracing::warn!(email = %c.email, "adding a contact: {e:#}");
            }
        }
    }
    Ok(())
}

/// Stores a contact: the row, and the person (all their addresses lead to
/// one thing).
async fn store(m: &Memex, acct: &Account, p: &Value) -> anyhow::Result<()> {
    let resource = p["resourceName"].as_str().unwrap_or_default();
    if p["metadata"]["deleted"].as_bool() == Some(true) {
        sqlx::query("DELETE FROM contacts WHERE account = ? AND resource = ?").bind(&acct.id).bind(resource).execute(&m.store.pool).await?;
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
            None => google::person(&m.store, e, &name).await?,
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
    .bind(&acct.id)
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

/// The carbon6 contact group's resource name (made on first use).
async fn group(m: &Memex, account: &str) -> anyhow::Result<String> {
    let v = m.google.get(account, &m.google.people_url("/v1/contactGroups"), &[("pageSize", "1000".into())]).await?;
    if let Some(g) = v["contactGroups"].as_array().into_iter().flatten().find(|g| g["name"].as_str() == Some(GROUP)) {
        return Ok(g["resourceName"].as_str().unwrap_or_default().to_owned());
    }
    let g = m
        .google
        .call(
            account,
            reqwest::Method::POST,
            &m.google.people_url("/v1/contactGroups"),
            &[],
            Some(&json!({"contactGroup": {"name": GROUP}})),
        )
        .await?;
    Ok(g["resourceName"].as_str().unwrap_or_default().to_owned())
}

/// Adds a contact in Google (in the carbon6 group), and here.
pub async fn create(m: &Memex, account: &str, name: &str, email: &str) -> anyhow::Result<api::Contact> {
    let acct = m.account(account)?;
    let g = group(m, &acct.id).await?;
    let body = json!({
        "names": [{"unstructuredName": name}],
        "emailAddresses": [{"value": email}],
        "memberships": [{"contactGroupMembership": {"contactGroupResourceName": g}}],
    });
    let p = m
        .google
        .call(
            &acct.id,
            reqwest::Method::POST,
            &m.google.people_url("/v1/people:createContact"),
            &[("personFields", FIELDS.into())],
            Some(&body),
        )
        .await?;
    store(m, &acct, &p).await?;
    info!(%email, "contact added to Google");
    Ok(list(m, email).await?.into_iter().next().unwrap_or_default())
}
