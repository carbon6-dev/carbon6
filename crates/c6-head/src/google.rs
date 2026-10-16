//! Gmail, Google Calendar and Google Contacts are apps (docs/knowledge.md
//! §6): `gmail`, `google-calendar` and `google-contacts`, on the `google`
//! kit, installed in home with your approval. Their packages are part of
//! carbon6 (crates/c6-google). This is what Settings → Mail & calendar
//! does with them: your OAuth client (home's secrets GOOGLE_CLIENT_ID and
//! GOOGLE_CLIENT_SECRET), one sign-in for all three apps, the accounts,
//! where their syncs are; and the move of what the memex synced itself
//! before, into the apps.

use std::collections::BTreeMap;

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::{HeadState, bad_request, not_found};

/// The apps, what each feeds, and their sync collections.
pub const APPS: [(&str, &str); 3] = [("gmail", "gmail_sync"), ("google-calendar", "calendar_sync"), ("google-contacts", "contacts_sync")];
const KIT: &str = "google";
const CLIENT_ID: &str = "GOOGLE_CLIENT_ID";
const CLIENT_SECRET: &str = "GOOGLE_CLIENT_SECRET";
/// What the memex kept before: the OAuth client.
const LEGACY_CLIENT: &str = "google_client";

/// The kit's and the apps' packages: manifests, and the kit's source.
pub fn packages() -> Vec<(Value, BTreeMap<String, Vec<u8>>)> {
    macro_rules! files {
        ($($path:literal),*) => {
            BTreeMap::from([$(($path.to_owned(), include_bytes!(concat!("../../c6-google/", $path)).to_vec())),*])
        };
    }
    let kit =
        files!("Dockerfile", "Cargo.toml", "Cargo.lock", "src/lib.rs", "src/main.rs", "src/mail.rs", "src/calendar.rs", "src/contacts.rs");
    let parse = |s: &str| serde_json::from_str::<Value>(s).expect("a package manifest");
    vec![
        (parse(include_str!("../../c6-google/kit.json")), kit),
        (parse(include_str!("../../c6-google/apps/gmail.json")), BTreeMap::new()),
        (parse(include_str!("../../c6-google/apps/google-calendar.json")), BTreeMap::new()),
        (parse(include_str!("../../c6-google/apps/google-contacts.json")), BTreeMap::new()),
    ]
}

async fn register(st: &HeadState) -> anyhow::Result<()> {
    for (manifest, files) in packages() {
        crate::packages::add(st, manifest, files, "system").await?;
    }
    Ok(())
}

/// The Google apps installed in home: (package, instance name).
pub async fn installed(st: &HeadState) -> anyhow::Result<Vec<(String, String)>> {
    Ok(crate::instances::list(&st.store.pool, Some(crate::realms::HOME))
        .await?
        .into_iter()
        .filter(|i| APPS.iter().any(|(p, _)| *p == i.package.app))
        .map(|i| (i.package.app.clone(), i.name.clone()))
        .collect())
}

/// Asks you to install the Google apps that aren't yet (a decision each).
pub async fn install(st: &HeadState) -> anyhow::Result<Vec<api::Install>> {
    register(st).await?;
    let have = installed(st).await?;
    let pending = crate::packages::installs(st).await?;
    let mut out = Vec::new();
    for (p, _) in APPS {
        if have.iter().any(|(x, _)| x == p) || pending.iter().any(|i| i.package == p && i.state == "installing") {
            continue;
        }
        out.push(
            crate::packages::install(st, &api::InstallIn { package: p.into(), realm: crate::realms::HOME.into(), ..Default::default() })
                .await?,
        );
    }
    Ok(out)
}

pub async fn get_client(st: &HeadState) -> anyhow::Result<api::GoogleClient> {
    let id = crate::secrets::value_in(st, crate::realms::HOME, CLIENT_ID).await?.unwrap_or_default();
    let has_secret = crate::secrets::value_in(st, crate::realms::HOME, CLIENT_SECRET).await?.is_some_and(|s| !s.is_empty());
    let apps = installed(st).await?.into_iter().map(|(p, _)| p).collect();
    let waiting = crate::packages::installs(st)
        .await?
        .into_iter()
        .filter(|i| i.state == "installing" && APPS.iter().any(|(p, _)| *p == i.package))
        .map(|i| i.package)
        .collect();
    Ok(api::GoogleClient { client_id: id, has_secret, redirect_uri: String::new(), apps, waiting })
}

pub async fn set_client(st: &HeadState, c: &api::GoogleClientIn) -> anyhow::Result<()> {
    let id = c.client_id.trim();
    st.store.put_secret(CLIENT_ID, Some(st.secrets.encrypt(id)?), "Your Google OAuth client's id (Gmail, Calendar, Contacts)", &[]).await?;
    if let Some(s) = c.client_secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        st.store.put_secret(CLIENT_SECRET, Some(st.secrets.encrypt(s)?), "Your Google OAuth client's secret", &[]).await?;
    }
    Ok(())
}

/// Where Google sends you back: the address registered for this head
/// before Gmail was an app, so your OAuth client needs no change.
pub async fn redirect_uri(st: &HeadState, origin: Option<&str>) -> String {
    let base = st.store.setting(crate::notify::PUBLIC_URL).await.ok().flatten().filter(|u| !u.trim().is_empty());
    format!("{}/auth/google/callback", base.as_deref().or(origin).unwrap_or("http://localhost:7700").trim_end_matches('/'))
}

/// One sign-in for every Google app installed in home.
pub async fn connect(st: &HeadState, origin: Option<&str>) -> anyhow::Result<String> {
    let apps: Vec<String> = installed(st).await?.into_iter().map(|(_, n)| n).collect();
    if apps.is_empty() {
        return Err(bad_request("install the Google apps first (Settings → Mail & calendar)"));
    }
    let redirect = redirect_uri(st, origin).await;
    crate::providers::connect_many(st, crate::realms::HOME, &apps, "google", &redirect, "/settings").await
}

/// The accounts connected, once each (the gmail app's id when it has it).
pub async fn accounts(st: &HeadState) -> anyhow::Result<Vec<api::GoogleAccount>> {
    let mut out: Vec<api::GoogleAccount> = Vec::new();
    for (_, app) in installed(st).await? {
        for a in crate::providers::accounts(st, crate::realms::HOME, &app).await? {
            if !out.iter().any(|x| x.email.eq_ignore_ascii_case(&a.account)) {
                out.push(api::GoogleAccount { id: a.id, email: a.account, name: a.name, added_at: a.added_at });
            }
        }
    }
    Ok(out)
}

/// Disconnects an account from every Google app.
pub async fn remove(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let email: String = sqlx::query_scalar("SELECT account FROM oauth_accounts WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found("no such account"))?;
    for (_, app) in installed(st).await? {
        sqlx::query("DELETE FROM oauth_accounts WHERE realm = ? AND instance = ? AND account = ?")
            .bind(crate::realms::HOME)
            .bind(&app)
            .bind(&email)
            .execute(&st.store.pool)
            .await?;
    }
    crate::memex::reload(st).await;
    Ok(())
}

/// The accounts and where each sync is (for the mail client): kinds
/// `mail`, `cal:<calendar>` and `contacts`.
pub async fn status(st: &HeadState) -> anyhow::Result<Value> {
    let accounts: Vec<Value> = accounts(st).await?.into_iter().map(|a| json!({"id": a.id, "email": a.email})).collect();
    let emails: BTreeMap<String, String> = sqlx::query_as("SELECT id, account FROM oauth_accounts WHERE realm = ?")
        .bind(crate::realms::HOME)
        .fetch_all(&st.store.pool)
        .await?
        .into_iter()
        .collect();
    let mut sync = Vec::new();
    for (p, app) in installed(st).await? {
        let Some((_, col)) = APPS.iter().find(|(x, _)| *x == p) else { continue };
        let Ok(page) =
            crate::collections::query(st, crate::realms::HOME, col, &api::RecordQuery { limit: Some(1000), ..Default::default() }).await
        else {
            continue;
        };
        for r in page.records {
            let f = &r.fields;
            let key = f["key"].as_str().unwrap_or_default();
            let (account, kind) = match key.split(':').collect::<Vec<_>>().as_slice() {
                ["mail", a] => (a.to_string(), "mail".to_owned()),
                ["contacts", a] => (a.to_string(), "contacts".to_owned()),
                ["cal", a, rest @ ..] => (a.to_string(), format!("cal:{}", rest.join(":"))),
                _ => continue,
            };
            sync.push(json!({"account": account, "email": emails.get(&account), "kind": kind, "phase": f["phase"], "done": f["done"],
                             "error": f["error"], "updated_at": f["updated_at"], "app": app}));
        }
    }
    Ok(json!({"accounts": accounts, "sync": sync}))
}

/// Runs the Google apps' syncs now.
pub async fn sync_now(st: &std::sync::Arc<HeadState>) -> anyhow::Result<()> {
    let apps: Vec<String> = installed(st).await?.into_iter().map(|(_, n)| n).collect();
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM triggers WHERE realm = ? AND action = 'kit' AND owner IN (SELECT value FROM json_each(?))")
            .bind(crate::realms::HOME)
            .bind(serde_json::to_string(&apps)?)
            .fetch_all(&st.store.pool)
            .await?;
    for id in ids {
        let st = st.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::triggers::run_now(&st, &id).await {
                warn!("Google sync: {e:#}");
            }
        });
    }
    Ok(())
}

/// What the memex synced itself before Gmail, Calendar and Contacts were
/// apps moves into them, once, at start: the apps are installed in home,
/// your OAuth client and accounts become theirs, and the mail, events and
/// contacts already here, with where each sync was, become the apps' (so
/// nothing is fetched again).
pub async fn move_in(st: &HeadState) -> anyhow::Result<()> {
    if st.store.setting("google_moved").await?.is_some() {
        return Ok(());
    }
    let rows = sqlx::query("SELECT * FROM google_accounts ORDER BY added_at").fetch_all(&st.store.pool).await.unwrap_or_default();
    let legacy: Value = serde_json::from_str(&st.store.setting(LEGACY_CLIENT).await?.unwrap_or_default()).unwrap_or_default();
    if rows.is_empty() && legacy["client_id"].as_str().is_none_or(str::is_empty) {
        st.store.set_setting("google_moved", "1").await?;
        return Ok(());
    }
    // The apps, approved: you had approved the memex's Google sync.
    register(st).await?;
    for (p, _) in APPS {
        if installed(st).await?.iter().any(|(x, _)| x == p) {
            continue;
        }
        let i =
            crate::packages::install(st, &api::InstallIn { package: p.into(), realm: crate::realms::HOME.into(), ..Default::default() })
                .await?;
        crate::packages::answer(st, &format!("package:install:{}", i.id), Some("Install"), &serde_json::Map::new()).await?;
    }
    // Your OAuth client.
    if let Some(id) = legacy["client_id"].as_str().filter(|i| !i.is_empty()) {
        let secret = legacy["secret"].as_str().and_then(|h| hex::decode(h).ok()).and_then(|b| st.secrets.decrypt(&b).ok());
        set_client(st, &api::GoogleClientIn { client_id: id.into(), client_secret: secret }).await?;
    }
    // Each account, for each app: gmail keeps its id (the mail is filed by
    // it); the calendar's and contacts' get theirs.
    let mut moves = Vec::new();
    for r in &rows {
        let old: String = r.get("id");
        let (email, name): (String, String) = (r.get("email"), r.get("name"));
        let refresh = st.secrets.decrypt(&r.get::<Vec<u8>, _>("refresh_token"))?;
        let access = r.get::<Option<Vec<u8>>, _>("access_token").and_then(|b| st.secrets.decrypt(&b).ok()).unwrap_or_default();
        let scopes: String = r.get("scopes");
        let ids = [old.clone(), format!("{old}c"), format!("{old}p")];
        for ((p, _), id) in APPS.iter().zip(&ids) {
            let a = crate::providers::Kept {
                realm: crate::realms::HOME,
                instance: p,
                provider: "google",
                account: &email,
                name: &name,
                refresh: &refresh,
                access: &access,
                expires: r.get("access_expires"),
                scopes: &scopes,
            };
            crate::providers::keep(st, &a, Some(id)).await?;
        }
        moves.push(json!({"old": old, "mail": ids[0], "calendar": ids[1], "contacts": ids[2]}));
    }
    // What the memex kept, and where its syncs were.
    let syncs = crate::memex::call(st, "memex.legacy_sync", json!({})).await.unwrap_or(json!([]));
    crate::memex::call(st, "memex.move_google", json!({"accounts": moves})).await?;
    for s in syncs.as_array().into_iter().flatten() {
        let (old, kind) = (s["account"].as_str().unwrap_or_default(), s["kind"].as_str().unwrap_or_default());
        let Some(m) = moves.iter().find(|m| m["old"] == old) else { continue };
        let (app, col, key) = if kind == "mail" {
            ("gmail", "gmail_sync", format!("mail:{}", m["mail"].as_str().unwrap_or_default()))
        } else if kind == "contacts" {
            ("google-contacts", "contacts_sync", format!("contacts:{}", m["contacts"].as_str().unwrap_or_default()))
        } else if let Some(cal) = kind.strip_prefix("cal:") {
            ("google-calendar", "calendar_sync", format!("cal:{}:{cal}", m["calendar"].as_str().unwrap_or_default()))
        } else {
            continue;
        };
        let fields = json!({"key": key, "cursor": s["cursor"], "page_token": s["page_token"], "phase": s["phase"], "done": s["done"],
                            "error": Value::Null, "updated_at": c6_core::now_ms()});
        let rec = api::RecordIn { fields, ..Default::default() };
        if let Err(e) = crate::collections::put(st, crate::realms::HOME, col, None, &rec, &format!("app:{app}")).await {
            warn!(%key, "moving where a sync was: {e:#}");
        }
    }
    st.store.set_setting("google_moved", "1").await?;
    crate::memex::reload(st).await;
    info!(accounts = rows.len(), "Gmail, Calendar and Contacts moved into their apps");
    let d = api::DecisionIn {
        kind: "follow_up".into(),
        title: "Gmail, Calendar and Contacts are apps now".into(),
        document: "They run as the apps gmail, google-calendar and google-contacts in home, on the google kit, with your accounts and \
                   everything already synced. Their syncs run on a worker with Docker or Podman (the kit is built there once, and \
                   reaches only Google). Settings → Mail & calendar manages your accounts."
            .into(),
        source: "google".into(),
        ..Default::default()
    };
    crate::decisions::open(st, &d, crate::realms::HOME, "google:moved").await?;
    Ok(())
}

/// The kit's name (for tests that run it in this process).
pub fn kit() -> &'static str {
    KIT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kit's package is content-addressed: its source can't change
    /// under the same version. Change the source (or an app's manifest),
    /// and bump its version, and the digest in crates/c6-google/kit.digest.
    #[test]
    fn the_packages_follow_their_source() {
        let mut h = String::new();
        for (manifest, files) in packages() {
            h.push_str(&format!(
                "{} {} {}\n",
                manifest["package"],
                manifest["version"],
                c6_core::sha256_hex(manifest.to_string().as_bytes())
            ));
            for (path, data) in &files {
                h.push_str(&format!("{path} {}\n", c6_core::sha256_hex(data)));
            }
        }
        let now = c6_core::sha256_hex(h.as_bytes());
        let recorded = include_str!("../../c6-google/kit.digest").trim();
        assert_eq!(
            recorded, now,
            "the google packages changed: bump their versions (kit.json, apps/*.json), and write `{now}` to crates/c6-google/kit.digest"
        );
    }
}
