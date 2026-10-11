//! Google accounts: sign-in through your own OAuth client (Settings → Mail
//! & calendar), tokens kept encrypted on the head, and short-lived access
//! tokens for the memex (only), which syncs Gmail, Calendar and Contacts.

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

/// Settings: the OAuth client's id, and its secret (encrypted, hex).
const CLIENT: &str = "google_client";
const SCOPES: &str = "openid email profile https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/gmail.send \
                      https://www.googleapis.com/auth/calendar https://www.googleapis.com/auth/contacts";

/// Google's endpoints; tests point them elsewhere.
pub struct Endpoints {
    pub auth: String,
    pub token: String,
    pub api: String,
    pub userinfo: String,
    pub people: String,
}

pub fn endpoints() -> Endpoints {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
    let api = env("C6_GOOGLE_API_URL", "https://www.googleapis.com");
    Endpoints {
        auth: env("C6_GOOGLE_AUTH_URL", "https://accounts.google.com/o/oauth2/v2/auth"),
        token: env("C6_GOOGLE_TOKEN_URL", "https://oauth2.googleapis.com/token"),
        userinfo: format!("{}/oauth2/v3/userinfo", api.trim_end_matches('/')),
        people: env("C6_GOOGLE_PEOPLE_URL", "https://people.googleapis.com"),
        api,
    }
}

/// Sign-ins in progress: state → (redirect URI, when).
static PENDING: LazyLock<Mutex<HashMap<String, (String, i64)>>> = LazyLock::new(Default::default);

#[derive(Default, Deserialize, serde::Serialize)]
struct Client {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    secret: String,
}

async fn client(st: &HeadState) -> anyhow::Result<(String, String)> {
    let c: Client = serde_json::from_str(&st.store.setting(CLIENT).await?.unwrap_or_default()).unwrap_or_default();
    if c.client_id.is_empty() || c.secret.is_empty() {
        return Err(bad_request("set your Google OAuth client first (Settings → Mail & calendar)"));
    }
    Ok((c.client_id, st.secrets.decrypt(&hex::decode(&c.secret)?)?))
}

pub async fn get_client(st: &HeadState) -> anyhow::Result<api::GoogleClient> {
    let c: Client = serde_json::from_str(&st.store.setting(CLIENT).await?.unwrap_or_default()).unwrap_or_default();
    Ok(api::GoogleClient { client_id: c.client_id, has_secret: !c.secret.is_empty(), redirect_uri: String::new() })
}

pub async fn set_client(st: &HeadState, c: &api::GoogleClientIn) -> anyhow::Result<()> {
    let mut cur: Client = serde_json::from_str(&st.store.setting(CLIENT).await?.unwrap_or_default()).unwrap_or_default();
    cur.client_id = c.client_id.trim().to_owned();
    if let Some(s) = c.client_secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        cur.secret = hex::encode(st.secrets.encrypt(s)?);
    }
    st.store.set_setting(CLIENT, &serde_json::to_string(&cur)?).await
}

/// Where Google sends you back: the head's address (Settings →
/// Notifications), else the page's.
pub async fn redirect_uri(st: &HeadState, origin: Option<&str>) -> String {
    let base = st.store.setting(crate::notify::PUBLIC_URL).await.ok().flatten().filter(|u| !u.trim().is_empty());
    format!("{}/auth/google/callback", base.as_deref().or(origin).unwrap_or("http://localhost:7700").trim_end_matches('/'))
}

/// Starts a sign-in: the address to send you to.
pub async fn connect(st: &HeadState, origin: Option<&str>) -> anyhow::Result<String> {
    let (client_id, _) = client(st).await?;
    let redirect = redirect_uri(st, origin).await;
    let state = hex::encode(rand::random::<[u8; 16]>());
    let now = c6_core::now_ms();
    let mut pending = PENDING.lock().unwrap();
    pending.retain(|_, (_, t)| now - *t < 15 * 60_000);
    pending.insert(state.clone(), (redirect.clone(), now));
    let mut u = url::Url::parse(&endpoints().auth)?;
    u.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("include_granted_scopes", "true")
        .append_pair("state", &state);
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

/// `GET /auth/google/callback`: trades the code for tokens, stores the
/// account, and goes back to Settings.
pub async fn callback(State(st): State<std::sync::Arc<HeadState>>, Query(q): Query<Callback>) -> Response {
    let back = |note: &str| Redirect::to(&format!("/settings?google={}", urlencode(note))).into_response();
    if !q.error.is_empty() {
        return back(&q.error);
    }
    let Some((redirect, _)) = PENDING.lock().unwrap().remove(&q.state) else { return back("that sign-in expired; try again") };
    match finish(&st, &q.code, &redirect).await {
        Ok(email) => {
            info!(%email, "Google account connected");
            st.memex_feed.wake();
            back("connected")
        }
        Err(e) => {
            warn!("Google sign-in: {e:#}");
            back(&format!("{e:#}"))
        }
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn finish(st: &HeadState, code: &str, redirect: &str) -> anyhow::Result<String> {
    let (client_id, secret) = client(st).await?;
    let tokens: Value = st
        .http
        .post(endpoints().token)
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
    let access = tokens["access_token"].as_str().ok_or_else(|| anyhow::anyhow!("Google gave no token: {tokens}"))?;
    let refresh = tokens["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Google gave no refresh token (remove carbon6's access in your Google account and try again)"))?;
    let who: Value = st.http.get(endpoints().userinfo).bearer_auth(access).send().await?.json().await?;
    let email = who["email"].as_str().ok_or_else(|| anyhow::anyhow!("Google did not say which account this is"))?.to_owned();
    let expires = c6_core::now_ms() + tokens["expires_in"].as_i64().unwrap_or(3600) * 1000;
    sqlx::query(
        "INSERT INTO google_accounts (id, email, name, refresh_token, access_token, access_expires, scopes, added_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(email) DO UPDATE SET name = excluded.name, refresh_token = excluded.refresh_token, access_token = excluded.access_token, \
         access_expires = excluded.access_expires, scopes = excluded.scopes",
    )
    .bind(uuid::Uuid::now_v7().simple().to_string()[20..].to_owned())
    .bind(&email)
    .bind(who["name"].as_str().unwrap_or(""))
    .bind(st.secrets.encrypt(refresh)?)
    .bind(st.secrets.encrypt(access)?)
    .bind(expires)
    .bind(tokens["scope"].as_str().unwrap_or(SCOPES))
    .bind(c6_core::now_ms())
    .execute(&st.store.pool)
    .await?;
    Ok(email)
}

pub async fn accounts(st: &HeadState) -> anyhow::Result<Vec<api::GoogleAccount>> {
    let rows = sqlx::query("SELECT id, email, name, added_at FROM google_accounts ORDER BY added_at").fetch_all(&st.store.pool).await?;
    Ok(rows
        .iter()
        .map(|r| api::GoogleAccount { id: r.get("id"), email: r.get("email"), name: r.get("name"), added_at: r.get("added_at") })
        .collect())
}

pub async fn remove(st: &HeadState, id: &str) -> anyhow::Result<()> {
    if sqlx::query("DELETE FROM google_accounts WHERE id = ?").bind(id).execute(&st.store.pool).await?.rows_affected() == 0 {
        return Err(not_found("no such account"));
    }
    Ok(())
}

/// A current access token for an account (refreshed here when close to
/// expiry): (token, expires at ms).
pub async fn access_token(st: &HeadState, id: &str) -> anyhow::Result<(String, i64)> {
    let r = sqlx::query("SELECT refresh_token, access_token, access_expires FROM google_accounts WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found("no such Google account"))?;
    let expires: i64 = r.get("access_expires");
    if let Some(enc) = r.get::<Option<Vec<u8>>, _>("access_token")
        && expires - 120_000 > c6_core::now_ms()
    {
        return Ok((st.secrets.decrypt(&enc)?, expires));
    }
    let (client_id, secret) = client(st).await?;
    let refresh = st.secrets.decrypt(&r.get::<Vec<u8>, _>("refresh_token"))?;
    let tokens: Value = st
        .http
        .post(endpoints().token)
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
        tokens["access_token"].as_str().ok_or_else(|| anyhow::anyhow!("Google refused to refresh the sign-in: {}", tokens["error"]))?;
    let expires = c6_core::now_ms() + tokens["expires_in"].as_i64().unwrap_or(3600) * 1000;
    sqlx::query("UPDATE google_accounts SET access_token = ?, access_expires = ? WHERE id = ?")
        .bind(st.secrets.encrypt(access)?)
        .bind(expires)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    Ok((access.to_owned(), expires))
}

/// For the memex's config: the accounts it syncs.
pub async fn for_memex(st: &HeadState) -> Value {
    json!(
        accounts(st)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|a| json!({"id": a.id, "email": a.email, "name": a.name}))
            .collect::<Vec<_>>()
    )
}
