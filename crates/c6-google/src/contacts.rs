//! Google Contacts: synced with sync tokens (People API); people you write
//! to are added, in a contact group named carbon6.

use serde_json::{Value, json};

use crate::{Account, Run, status_of};

const FIELDS: &str = "names,emailAddresses,phoneNumbers,organizations,photos,memberships";
const GROUP: &str = "carbon6";

fn keep(r: &mut Run, a: &Account, p: &Value) {
    let resource = p["resourceName"].as_str().unwrap_or_default();
    r.json_item("application/vnd.c6.contact+json", &format!("{}:{resource}", a.id), json!({"account": a.id, "person": p}));
}

/// Every account's contacts (what changed).
pub async fn sync(r: &mut Run) -> anyhow::Result<()> {
    for a in r.accounts.clone() {
        let key = format!("contacts:{}", a.id);
        let mut st = r.sync(&key);
        let error = one(r, &a, &mut st).await.err().map(|e| format!("{e:#}"));
        if let Some(e) = &error {
            r.errors.push(format!("{}: contacts: {e}", a.email));
        }
        r.save(&key, st, error);
    }
    Ok(())
}

async fn one(r: &mut Run, a: &Account, st: &mut Value) -> anyhow::Result<()> {
    let url = format!("{}/v1/people/me/connections", r.people);
    let mut page: Option<String> = None;
    loop {
        let mut q = vec![("personFields", FIELDS.to_owned()), ("pageSize", "1000".to_owned()), ("requestSyncToken", "true".to_owned())];
        if let Some(t) = st["cursor"].as_str() {
            q.push(("syncToken", t.to_owned()));
        }
        if let Some(p) = &page {
            q.push(("pageToken", p.clone()));
        }
        let v = match r.get(a, &url, &q).await {
            Ok(v) => v,
            Err(e) if matches!(status_of(&e), Some(400 | 410)) && st["cursor"].is_string() => {
                *st = json!({"phase": "import", "done": 0});
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for p in v["connections"].as_array().into_iter().flatten() {
            keep(r, a, p);
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

/// The carbon6 contact group's resource name (made on first use).
async fn group(r: &Run, a: &Account) -> anyhow::Result<String> {
    let v = r.get(a, &format!("{}/v1/contactGroups", r.people), &[("pageSize", "1000".into())]).await?;
    if let Some(g) = v["contactGroups"].as_array().into_iter().flatten().find(|g| g["name"].as_str() == Some(GROUP)) {
        return Ok(g["resourceName"].as_str().unwrap_or_default().to_owned());
    }
    let g = r
        .call(a, reqwest::Method::POST, &format!("{}/v1/contactGroups", r.people), &[], Some(&json!({"contactGroup": {"name": GROUP}})))
        .await?;
    Ok(g["resourceName"].as_str().unwrap_or_default().to_owned())
}

/// `contacts-write`: adds a contact (`name`, `email`), or several (`people`:
/// [{name, email}], in one run), in the carbon6 group.
pub async fn write(r: &mut Run) -> anyhow::Result<()> {
    let a = r.account(r.input["account"].as_str().unwrap_or_default())?;
    let one = |v: &Value| (v["name"].as_str().unwrap_or_default().to_owned(), v["email"].as_str().unwrap_or_default().to_owned());
    let people: Vec<(String, String)> = match r.input["people"].as_array() {
        Some(list) => list.iter().map(one).collect(),
        None => vec![one(&r.input)],
    };
    anyhow::ensure!(!people.is_empty() && people.iter().all(|(_, e)| e.contains('@')), "each contact needs an address");
    let g = group(r, &a).await?;
    let (mut created, mut failed) = (Vec::new(), Vec::new());
    for (name, email) in people {
        let body = json!({
            "names": [{"unstructuredName": name}],
            "emailAddresses": [{"value": email}],
            "memberships": [{"contactGroupMembership": {"contactGroupResourceName": g}}],
        });
        let url = format!("{}/v1/people:createContact", r.people);
        match r.call(&a, reqwest::Method::POST, &url, &[("personFields", FIELDS.into())], Some(&body)).await {
            Ok(p) => {
                keep(r, &a, &p);
                created.push(json!({"email": email, "resource": p["resourceName"]}));
            }
            Err(e) => failed.push(json!({"email": email, "error": format!("{e:#}")})),
        }
    }
    anyhow::ensure!(!created.is_empty() || failed.is_empty(), "no contact was added: {}", failed[0]["error"]);
    r.result = json!({"account": a.id, "created": created, "failed": failed});
    Ok(())
}
