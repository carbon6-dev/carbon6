//! OAuth providers declared by packages (docs/knowledge.md §6.2, #2). A
//! package names a provider: where to authorize, where to trade codes and
//! refresh tokens, the scopes, and which secrets of the realm hold your
//! OAuth client. The head runs the sign-in at its own redirect address and
//! keeps each connected account's refresh token encrypted; an app's kit
//! jobs get short-lived access tokens only, in `C6_ACCOUNTS`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use c6_api as api;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::{HeadState, bad_request, not_found};

/// Sign-ins in progress: state → (realm, instance, provider, redirect URI, when).
type Pending = (String, String, String, String, i64);
static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);

/// What is wrong with a package's providers.
pub fn check(p: &api::AppPackage) -> Vec<String> {
    let mut out = Vec::new();
    for pr in &p.providers {
        let n = &pr.name;
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            out.push(format!("provider {n:?}: a name is letters, digits and -"));
        }
        for (what, u) in [("authorize_url", &pr.authorize_url), ("token_url", &pr.token_url)] {
            if url::Url::parse(u).is_err() {
                out.push(format!("provider {n}: {what} is a URL"));
            }
        }
        for s in [&pr.client_id_secret, &pr.client_secret_secret] {
            if !p.permissions.secrets.contains(s) {
                out.push(format!("provider {n}: the secret {s} is among those the package asks for (permissions.secrets)"));
            }
        }
    }
    out
}

async fn provider(st: &HeadState, realm: &str, instance: &str, name: &str) -> anyhow::Result<api::OAuthProvider> {
    let i = crate::instances::get(&st.store.pool, realm, instance)
        .await?
        .ok_or_else(|| not_found(format!("no app {instance} in realm {realm}")))?;
    i.package.providers.iter().find(|p| p.name == name).cloned().ok_or_else(|| not_found(format!("{instance} has no provider {name:?}")))
}

async fn client(st: &HeadState, realm: &str, p: &api::OAuthProvider) -> anyhow::Result<(String, String)> {
    let id = crate::secrets::value_in(st, realm, &p.client_id_secret).await?;
    let secret = crate::secrets::value_in(st, realm, &p.client_secret_secret).await?;
    match (id, secret) {
        (Some(i), Some(s)) if !i.is_empty() && !s.is_empty() => Ok((i, s)),
        _ => Err(bad_request(format!(
            "set your OAuth client first: the secrets {} and {} in realm {realm} (Settings → Secrets)",
            p.client_id_secret, p.client_secret_secret
        ))),
    }
}

/// Where providers send you back: the head's address (Settings →
/// Notifications), else the page's.
pub async fn redirect_uri(st: &HeadState, origin: Option<&str>) -> String {
    let base = st.store.setting(crate::notify::PUBLIC_URL).await.ok().flatten().filter(|u| !u.trim().is_empty());
    format!("{}/auth/oauth/callback", base.as_deref().or(origin).unwrap_or("http://localhost:7700").trim_end_matches('/'))
}

/// Starts a sign-in to an app's provider: the address to send you to.
pub async fn connect(st: &HeadState, realm: &str, instance: &str, name: &str, origin: Option<&str>) -> anyhow::Result<String> {
    let p = provider(st, realm, instance, name).await?;
    let (client_id, _) = client(st, realm, &p).await?;
    let redirect = redirect_uri(st, origin).await;
    let state = hex::encode(rand::random::<[u8; 16]>());
    let now = c6_core::now_ms();
    {
        let mut pending = PENDING.lock().unwrap();
        pending.retain(|_, x| now - x.4 < 15 * 60_000);
        pending.insert(state.clone(), (realm.to_owned(), instance.to_owned(), name.to_owned(), redirect.clone(), now));
    }
    let mut u = url::Url::parse(&p.authorize_url)?;
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("response_type", "code")
            .append_pair("scope", &p.scopes.join(" "))
            .append_pair("state", &state);
        for (k, v) in &p.params {
            q.append_pair(k, v);
        }
    }
    Ok(u.to_string())
}

#[derive(Deserialize)]
pub struct Callback {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: String,
}

/// `GET /auth/oauth/callback`: trades the code for tokens, keeps the
/// account, and goes back to Settings.
pub async fn callback(State(st): State<std::sync::Arc<HeadState>>, Query(q): Query<Callback>) -> Response {
    let back = |note: &str| Redirect::to(&format!("/settings/system?oauth={}", urlencode(note))).into_response();
    if !q.error.is_empty() {
        return back(&q.error);
    }
    let Some((realm, instance, name, redirect, _)) = PENDING.lock().unwrap().remove(&q.state) else {
        return back("that sign-in expired; try again");
    };
    match finish(&st, &realm, &instance, &name, &q.code, &redirect).await {
        Ok(account) => {
            info!(%account, %instance, provider = %name, "account connected");
            back("connected")
        }
        Err(e) => {
            warn!(%instance, provider = %name, "sign-in: {e:#}");
            back(&format!("{e:#}"))
        }
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn finish(st: &HeadState, realm: &str, instance: &str, name: &str, code: &str, redirect: &str) -> anyhow::Result<String> {
    let p = provider(st, realm, instance, name).await?;
    let (client_id, secret) = client(st, realm, &p).await?;
    let tokens: Value = st
        .http
        .post(&p.token_url)
        .form(&[
            ("code", code),
            ("client_id", &client_id),
            ("client_secret", &secret),
            ("redirect_uri", redirect),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?
        .json()
        .await?;
    let access = tokens["access_token"].as_str().ok_or_else(|| anyhow::anyhow!("{name} gave no token: {}", tokens["error"]))?;
    let refresh = tokens["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("{name} gave no refresh token (remove carbon6's access in that account and try again)"))?;
    let (account, display) = if p.userinfo_url.is_empty() {
        (format!("{name} account"), String::new())
    } else {
        let who: Value = st.http.get(&p.userinfo_url).bearer_auth(access).send().await?.json().await?;
        let field = if p.account_field.is_empty() { "email" } else { p.account_field.as_str() };
        let account = who[field].as_str().ok_or_else(|| anyhow::anyhow!("{name} did not say which account this is"))?.to_owned();
        (account, who["name"].as_str().unwrap_or("").to_owned())
    };
    let expires = c6_core::now_ms() + tokens["expires_in"].as_i64().unwrap_or(3600) * 1000;
    sqlx::query(
        "INSERT INTO oauth_accounts (id, realm, instance, provider, account, name, refresh_token, access_token, access_expires, scopes, added_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (realm, instance, provider, account) DO UPDATE SET name = excluded.name, \
         refresh_token = excluded.refresh_token, access_token = excluded.access_token, access_expires = excluded.access_expires, \
         scopes = excluded.scopes",
    )
    .bind(uuid::Uuid::now_v7().simple().to_string()[20..].to_owned())
    .bind(realm)
    .bind(instance)
    .bind(name)
    .bind(&account)
    .bind(&display)
    .bind(st.secrets.encrypt(refresh)?)
    .bind(st.secrets.encrypt(access)?)
    .bind(expires)
    .bind(tokens["scope"].as_str().unwrap_or(&p.scopes.join(" ")))
    .bind(c6_core::now_ms())
    .execute(&st.store.pool)
    .await?;
    Ok(account)
}

/// The accounts connected for an app.
pub async fn accounts(st: &HeadState, realm: &str, instance: &str) -> anyhow::Result<Vec<api::OAuthAccount>> {
    let rows =
        sqlx::query("SELECT id, provider, account, name, added_at FROM oauth_accounts WHERE realm = ? AND instance = ? ORDER BY added_at")
            .bind(realm)
            .bind(instance)
            .fetch_all(&st.store.pool)
            .await?;
    Ok(rows
        .iter()
        .map(|r| api::OAuthAccount {
            id: r.get("id"),
            provider: r.get("provider"),
            account: r.get("account"),
            name: r.get("name"),
            added_at: r.get("added_at"),
        })
        .collect())
}

pub async fn remove(st: &HeadState, realm: &str, instance: &str, id: &str) -> anyhow::Result<()> {
    let done = sqlx::query("DELETE FROM oauth_accounts WHERE id = ? AND realm = ? AND instance = ?")
        .bind(id)
        .bind(realm)
        .bind(instance)
        .execute(&st.store.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(not_found("no such account"));
    }
    Ok(())
}

/// A current access token for an account (refreshed when close to expiry).
async fn access_token(st: &HeadState, r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<(String, i64)> {
    let (id, realm, instance, name): (String, String, String, String) = (r.get("id"), r.get("realm"), r.get("instance"), r.get("provider"));
    let expires: i64 = r.get("access_expires");
    if let Some(enc) = r.get::<Option<Vec<u8>>, _>("access_token")
        && expires - 120_000 > c6_core::now_ms()
    {
        return Ok((st.secrets.decrypt(&enc)?, expires));
    }
    let p = provider(st, &realm, &instance, &name).await?;
    let (client_id, secret) = client(st, &realm, &p).await?;
    let refresh = st.secrets.decrypt(&r.get::<Vec<u8>, _>("refresh_token"))?;
    let tokens: Value = st
        .http
        .post(&p.token_url)
        .form(&[
            ("refresh_token", refresh.as_str()),
            ("client_id", &client_id),
            ("client_secret", &secret),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?
        .json()
        .await?;
    let access =
        tokens["access_token"].as_str().ok_or_else(|| anyhow::anyhow!("{name} refused to refresh the sign-in: {}", tokens["error"]))?;
    let expires = c6_core::now_ms() + tokens["expires_in"].as_i64().unwrap_or(3600) * 1000;
    sqlx::query("UPDATE oauth_accounts SET access_token = ?, access_expires = ? WHERE id = ?")
        .bind(st.secrets.encrypt(access)?)
        .bind(expires)
        .bind(&id)
        .execute(&st.store.pool)
        .await?;
    Ok((access.to_owned(), expires))
}

/// The access tokens an app's kit jobs get: `C6_ACCOUNTS`, a JSON list of
/// {provider, account, access_token, expires_at}. An account whose sign-in
/// needs attention is left out, and noted.
pub async fn tokens_for(st: &HeadState, realm: &str, instance: &str) -> anyhow::Result<Vec<(String, String)>> {
    let rows = sqlx::query("SELECT * FROM oauth_accounts WHERE realm = ? AND instance = ?")
        .bind(realm)
        .bind(instance)
        .fetch_all(&st.store.pool)
        .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut list = Vec::new();
    for r in &rows {
        match access_token(st, r).await {
            Ok((token, expires)) => list.push(json!({"provider": r.get::<String, _>("provider"), "account": r.get::<String, _>("account"),
                "access_token": token, "expires_at": expires})),
            Err(e) => warn!(%instance, account = %r.get::<String, _>("account"), "access token: {e:#}"),
        }
    }
    Ok(vec![("C6_ACCOUNTS".to_owned(), Value::Array(list).to_string())])
}
