//! Slack is an app (docs/slack.md): `slack`, on the `slack-api` kit,
//! installed in a realm with your approval. Its packages are part of
//! carbon6 (crates/c6-slack). This is what a realm's Settings → Slack does
//! with it: your own internal Slack app (made from the manifest here, its
//! client in the realm's secrets `SLACK_CLIENT_ID` and
//! `SLACK_CLIENT_SECRET`, else root's), the workspaces you connect, and
//! how far each one's sync is.

use std::collections::BTreeMap;

use c6_api as api;
use serde_json::{Value, json};
use tracing::warn;

use crate::{HeadState, bad_request, not_found};

pub const APP: &str = "slack";
const COLLECTION: &str = "slack_sync";
const CLIENT_ID: &str = "SLACK_CLIENT_ID";
const CLIENT_SECRET: &str = "SLACK_CLIENT_SECRET";

/// The kit's and the app's packages: manifests, and the kit's source.
pub fn packages() -> Vec<(Value, BTreeMap<String, Vec<u8>>)> {
    macro_rules! files {
        ($($path:literal),*) => {
            BTreeMap::from([$(($path.to_owned(), include_bytes!(concat!("../../c6-slack/", $path)).to_vec())),*])
        };
    }
    let kit = files!("Dockerfile", "Cargo.toml", "Cargo.lock", "src/lib.rs", "src/main.rs", "src/sync.rs");
    let parse = |s: &str| serde_json::from_str::<Value>(s).expect("a package manifest");
    vec![(parse(include_str!("../../c6-slack/kit.json")), kit), (parse(include_str!("../../c6-slack/apps/slack.json")), BTreeMap::new())]
}

/// Lists the packages in the registry (installing is yours to do).
pub async fn register(st: &HeadState) -> anyhow::Result<()> {
    for (manifest, files) in packages() {
        crate::packages::add(st, manifest, files, "system").await?;
    }
    Ok(())
}

/// The slack app's instance in a realm, if installed.
pub async fn instance(st: &HeadState, realm: &str) -> anyhow::Result<Option<String>> {
    Ok(crate::instances::list(&st.store.pool, Some(realm)).await?.into_iter().find(|i| i.package.app == APP).map(|i| i.name))
}

/// Asks you to install the slack app in a realm (a decision), unless it
/// is there or asked for.
pub async fn install(st: &HeadState, realm: &str) -> anyhow::Result<Option<api::Install>> {
    register(st).await?;
    if instance(st, realm).await?.is_some() {
        return Ok(None);
    }
    let pending = crate::packages::installs(st).await?;
    if pending.iter().any(|i| i.package == APP && i.realm == realm && i.state == "installing") {
        return Ok(None);
    }
    Ok(Some(crate::packages::install(st, &api::InstallIn { package: APP.into(), realm: realm.into(), ..Default::default() }).await?))
}

/// The user scopes the app asks for (its provider's).
fn scopes() -> Vec<String> {
    let app: Value = serde_json::from_str(include_str!("../../c6-slack/apps/slack.json")).expect("the slack app's manifest");
    app["providers"][0]["scopes"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_owned)).collect()
}

/// The manifest to make your internal Slack app from ("Create an app →
/// From a manifest"), with this head's address to come back to.
pub fn manifest(redirect: &str) -> Value {
    json!({
        "display_information": {"name": "carbon6", "description": "Your Slack, mirrored into your carbon6 memex."},
        "oauth_config": {"redirect_urls": [redirect], "scopes": {"user": scopes()}},
        "settings": {"org_deploy_enabled": false, "socket_mode_enabled": false, "token_rotation_enabled": false},
    })
}

/// A realm's secret, else root's.
async fn secret(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<Option<String>> {
    match crate::secrets::value_in(st, realm, name).await?.filter(|v| !v.is_empty()) {
        Some(v) => Ok(Some(v)),
        None => Ok(crate::secrets::value_in(st, crate::realms::ROOT, name).await?.filter(|v| !v.is_empty())),
    }
}

/// Everything Settings → Slack shows for a realm.
pub async fn setup(st: &HeadState, realm: &str, origin: Option<&str>) -> anyhow::Result<api::SlackSetup> {
    let redirect = crate::providers::redirect_uri(st, origin).await;
    let installed = instance(st, realm).await?;
    let waiting = crate::packages::installs(st).await?.iter().any(|i| i.package == APP && i.realm == realm && i.state == "installing");
    let accounts = match &installed {
        Some(i) => crate::providers::accounts(st, realm, i).await?,
        None => Vec::new(),
    };
    Ok(api::SlackSetup {
        client_id: secret(st, realm, CLIENT_ID).await?.unwrap_or_default(),
        has_secret: secret(st, realm, CLIENT_SECRET).await?.is_some(),
        manifest: serde_json::to_string_pretty(&manifest(&redirect))?,
        https: redirect.starts_with("https://"),
        redirect_uri: redirect,
        installed: installed.is_some(),
        waiting,
        workspaces: workspaces(st, realm, &accounts).await?,
    })
}

/// Sets the realm's Slack client (its own secrets; the console's are
/// root's, every realm's that has none).
pub async fn set_client(st: &HeadState, realm: &str, c: &api::SlackClientIn) -> anyhow::Result<()> {
    if c.client_id.trim().is_empty() {
        return Err(bad_request("the client ID is required"));
    }
    crate::secrets::put_in(st, realm, CLIENT_ID, c.client_id.trim(), "Your Slack app's client ID").await?;
    if let Some(s) = c.client_secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        crate::secrets::put_in(st, realm, CLIENT_SECRET, s, "Your Slack app's client secret").await?;
    }
    Ok(())
}

/// Where Slack sends you to sign in; you come back to the realm's
/// Settings → Slack.
pub async fn connect(st: &HeadState, realm: &str, origin: Option<&str>) -> anyhow::Result<String> {
    let app = instance(st, realm).await?.ok_or_else(|| bad_request(format!("install the slack app in {realm} first")))?;
    let redirect = crate::providers::redirect_uri(st, origin).await;
    crate::providers::connect_many(st, realm, &[app], "slack", &redirect, &format!("/r/{realm}/settings/slack")).await
}

/// Disconnects a workspace: its sync stops; what it brought stays until
/// you remove the app.
pub async fn remove(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<()> {
    let app = instance(st, realm).await?.ok_or_else(|| not_found("no slack app here"))?;
    let gone = sqlx::query("DELETE FROM oauth_accounts WHERE id = ? AND realm = ? AND instance = ?")
        .bind(id)
        .bind(realm)
        .bind(&app)
        .execute(&st.store.pool)
        .await?
        .rows_affected();
    if gone == 0 {
        return Err(not_found("no such workspace"));
    }
    crate::memex::reload(st).await;
    Ok(())
}

/// Each connected workspace, and how far its sync is.
async fn workspaces(st: &HeadState, realm: &str, accounts: &[api::OAuthAccount]) -> anyhow::Result<Vec<api::SlackWorkspace>> {
    let records = crate::collections::query(st, realm, COLLECTION, &api::RecordQuery { limit: Some(5000), ..Default::default() })
        .await
        .map(|p| p.records)
        .unwrap_or_default();
    Ok(accounts
        .iter()
        .map(|a| {
            let team = records.iter().map(|r| &r.fields).find(|f| f["key"] == format!("team:{}", a.account));
            let chans: Vec<&Value> = records
                .iter()
                .map(|r| &r.fields)
                .filter(|f| f["key"].as_str().is_some_and(|k| k.starts_with(&format!("chan:{}:", a.account))))
                .collect();
            api::SlackWorkspace {
                id: a.id.clone(),
                team: a.account.clone(),
                name: a.name.clone(),
                added_at: a.added_at,
                conversations: chans.len() as u64,
                imported: chans.iter().filter(|c| c["import_done"] == true).count() as u64,
                people: team.and_then(|t| t["users"].as_object()).map_or(0, |u| u.len() as u64),
                files_waiting: team.and_then(|t| t["files_queue"].as_array()).map_or(0, |q| q.len() as u64),
                retry_after: team.and_then(|t| t["retry_after"].as_i64()),
                error: team.and_then(|t| t["error"].as_str()).map(str::to_owned),
                synced_at: team.and_then(|t| t["updated_at"].as_i64()),
            }
        })
        .collect())
}

/// Runs a realm's Slack sync now.
pub async fn sync_now(st: &std::sync::Arc<HeadState>, realm: &str) -> anyhow::Result<()> {
    let app = instance(st, realm).await?.ok_or_else(|| bad_request("no slack app here"))?;
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM triggers WHERE realm = ? AND action = 'kit' AND owner = ?")
        .bind(realm)
        .bind(&app)
        .fetch_all(&st.store.pool)
        .await?;
    for id in ids {
        let st = st.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::triggers::run_now(&st, &id).await {
                warn!("Slack sync: {e:#}");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kit's package is content-addressed: its source can't change
    /// under the same version. Change the source (or the app's manifest),
    /// and bump its version, and the digest in crates/c6-slack/kit.digest.
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
        let recorded = include_str!("../../c6-slack/kit.digest").trim();
        assert_eq!(
            recorded, now,
            "the slack packages changed: bump their versions (kit.json, apps/slack.json), and write `{now}` to crates/c6-slack/kit.digest"
        );
    }

    #[test]
    fn the_manifest_asks_for_user_scopes() {
        let m = manifest("https://head.example:7700/auth/oauth/callback");
        let user = m["oauth_config"]["scopes"]["user"].as_array().unwrap();
        assert!(user.iter().any(|s| s == "im:history") && user.iter().any(|s| s == "chat:write"), "{m}");
        assert_eq!(m["oauth_config"]["redirect_urls"][0], "https://head.example:7700/auth/oauth/callback");
    }
}
