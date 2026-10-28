//! People, and the accounts apps connect: who mail is from and to, who
//! attends, who a contact is; and the mail, calendar and contacts accounts
//! of the apps that feed those stores (docs/knowledge.md §6).

use serde_json::Value;
use sqlx::Row;

use crate::store::Store;

/// An account an app connected (from the head's config): its id, address,
/// the app, and which stores it feeds (mail, calendar, contacts).
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub struct Account {
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub feeds: Vec<String>,
    /// Where its conversations open on the service's web (its app's).
    #[serde(default)]
    pub links: Option<c6_api::AppLinks>,
}

/// Where the memex's own Google sync was (before mail, calendar and
/// contacts were apps): for moving it to the apps.
pub async fn legacy_states(store: &Store) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query("SELECT * FROM sync_state ORDER BY account, kind").fetch_all(&store.pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "account": r.get::<String, _>("account"), "kind": r.get::<String, _>("kind"), "phase": r.get::<String, _>("phase"),
                "done": r.get::<i64, _>("done"), "error": r.get::<Option<String>, _>("error"), "updated_at": r.get::<i64, _>("updated_at"),
                "cursor": r.get::<Option<String>, _>("cursor"), "page_token": r.get::<Option<String>, _>("page_token"),
            })
        })
        .collect())
}

/// Free-mail domains: an address there says nothing about an organization.
const FREE_MAIL: [&str; 14] = [
    "gmail.com",
    "googlemail.com",
    "yahoo.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "fastmail.com",
    "gmx.com",
];

/// The person with this address (made on first sight), and their
/// organization when the domain names one.
pub async fn person(store: &Store, email: &str, name: &str) -> anyhow::Result<String> {
    let email = email.trim().to_lowercase();
    let key = format!("person:{email}");
    let id = match store.thing_by_key(&key).await? {
        Some(id) => {
            if !name.trim().is_empty() {
                store.rename_if_unnamed(&id, name.trim(), &email).await?;
            }
            id
        }
        None => store.ensure_thing(&key, "person", if name.trim().is_empty() { &email } else { name.trim() }).await?,
    };
    if let Some(domain) = email.rsplit_once('@').map(|(_, d)| d.to_owned())
        && !FREE_MAIL.contains(&domain.as_str())
        && !domain.is_empty()
    {
        let org = store.ensure_thing(&format!("org:{domain}"), "org", &domain).await?;
        store.link_things(&id, "member_of", &org, "observed", "mail").await?;
    }
    Ok(id)
}

/// Moves what the memex's own Google sync kept into the apps that sync it
/// now (docs/knowledge.md §6): each account's mail becomes the gmail app's
/// (its account id stays), its events the google-calendar app's and its
/// contacts the google-contacts app's, under their accounts' new ids.
/// `accounts`: [{old, mail, calendar, contacts}]. What was changed.
pub async fn move_to_apps(m: &crate::Memex, accounts: &Value) -> anyhow::Result<u64> {
    let mut n = 0;
    let mut tx = m.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    for a in accounts.as_array().into_iter().flatten() {
        let s = |k: &str| a[k].as_str().unwrap_or_default().to_owned();
        let (old, mail, cal, con) = (s("old"), s("mail"), s("calendar"), s("contacts"));
        n += sqlx::query("UPDATE items SET source = 'app:gmail', external_id = ? || ':' || external_id WHERE source = ?")
            .bind(&mail)
            .bind(format!("gmail:{old}"))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        n += sqlx::query(
            "UPDATE items SET source = 'app:google-calendar', external_id = ? || ':' || replace(external_id, '/', ':') WHERE source = ?",
        )
        .bind(&cal)
        .bind(format!("calendar:{old}"))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        for (table, to) in [("mail", &mail), ("label_names", &mail), ("outgoing", &mail)] {
            let sql = match table {
                "mail" => "UPDATE mail SET account = ? WHERE account = ?",
                "label_names" => "UPDATE label_names SET account = ? WHERE account = ?",
                _ => "UPDATE outgoing SET account = ? WHERE account = ?",
            };
            sqlx::query(sql).bind(to).bind(&old).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE calendars SET account = ? WHERE account = ?").bind(&cal).bind(&old).execute(&mut *tx).await?;
        sqlx::query("UPDATE events SET account = ? WHERE account = ?").bind(&cal).bind(&old).execute(&mut *tx).await?;
        sqlx::query("UPDATE contacts SET account = ? WHERE account = ?").bind(&con).bind(&old).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(n)
}
