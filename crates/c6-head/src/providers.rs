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

/// Sign-ins in progress: state → (realm, instances, provider, redirect URI,
/// where to go back to, when).
type Pending = (String, Vec<String>, String, String, String, i64);
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
            "set your OAuth client first: the secrets {} and {}, in realm {realm} or root (on the app's page in Settings, or Settings → Secrets)",
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
    let redirect = redirect_uri(st, origin).await;
    connect_many(st, realm, &[instance.to_owned()], name, &redirect, "/settings/apps").await
}

/// One sign-in for several apps of a realm with the same provider (mail,
/// calendar and contacts of one account): every app's scopes, and the
/// account kept for each.
/// `redirect`: where the provider sends you back (the head's
/// `/auth/oauth/callback`, or an address registered before); `back`: the
/// page you return to.
pub async fn connect_many(
    st: &HeadState,
    realm: &str,
    instances: &[String],
    name: &str,
    redirect: &str,
    back: &str,
) -> anyhow::Result<String> {
    let first = instances.first().ok_or_else(|| bad_request("which apps?"))?;
    let mut p = provider(st, realm, first, name).await?;
    for i in &instances[1..] {
        for scope in provider(st, realm, i, name).await?.scopes {
            if !p.scopes.contains(&scope) {
                p.scopes.push(scope);
            }
        }
    }
    let (client_id, _) = client(st, realm, &p).await?;
    let redirect = redirect.to_owned();
    let state = hex::encode(rand::random::<[u8; 16]>());
    let now = c6_core::now_ms();
    {
        let mut pending = PENDING.lock().unwrap();
        pending.retain(|_, x| now - x.5 < 15 * 60_000);
        pending.insert(state.clone(), (realm.to_owned(), instances.to_vec(), name.to_owned(), redirect.clone(), back.to_owned(), now));
    }
    let mut u = url::Url::parse(&p.authorize_url)?;
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("response_type", "code")
            .append_pair(
                if p.scope_param.is_empty() { "scope" } else { &p.scope_param },
                &p.scopes.join(if p.scope_separator.is_empty() { " " } else { &p.scope_separator }),
            )
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
    let pending = PENDING.lock().unwrap().remove(&q.state);
    let page = pending.as_ref().map(|p| p.4.clone()).unwrap_or_else(|| "/settings/apps".into());
    let back = |note: &str| Redirect::to(&format!("{page}?oauth={}", urlencode(note))).into_response();
    if !q.error.is_empty() {
        return back(&q.error);
    }
    let Some((realm, instances, name, redirect, _, _)) = pending else {
        return back("that sign-in expired; try again");
    };
    match finish(&st, &realm, &instances, &name, &q.code, &redirect).await {
        Ok(account) => {
            info!(%account, apps = %instances.join(", "), provider = %name, "account connected");
            crate::memex::reload(&st).await;
            back("connected")
        }
        Err(e) => {
            warn!(apps = %instances.join(", "), provider = %name, "sign-in: {e:#}");
            back(&format!("{e:#}"))
        }
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn finish(st: &HeadState, realm: &str, instances: &[String], name: &str, code: &str, redirect: &str) -> anyhow::Result<String> {
    let p = provider(st, realm, instances.first().map(String::as_str).unwrap_or_default(), name).await?;
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
    let mine = at(&tokens, &p.token_path);
    let access = mine["access_token"].as_str().ok_or_else(|| anyhow::anyhow!("{name} gave no token: {}", tokens["error"]))?;
    // Without one, the token doesn't expire (it's kept as is); with an
    // expiry, it must come with a way to renew it.
    let refresh = mine["refresh_token"].as_str().unwrap_or_default();
    if refresh.is_empty() && mine["expires_in"].is_i64() {
        anyhow::bail!("{name} gave no refresh token (remove carbon6's access in that account and try again)");
    }
    let who = if p.userinfo_url.is_empty() {
        tokens.clone()
    } else {
        st.http.get(&p.userinfo_url).bearer_auth(access).send().await?.json().await?
    };
    let field = |f: &str, d: &str| if f.is_empty() { d.to_owned() } else { f.to_owned() };
    let (account, display) = match at(&who, &field(&p.account_field, "email")).as_str() {
        Some(a) => (a.to_owned(), at(&who, &field(&p.name_field, "name")).as_str().unwrap_or("").to_owned()),
        None if p.userinfo_url.is_empty() && p.account_field.is_empty() => (format!("{name} account"), String::new()),
        None => anyhow::bail!("{name} did not say which account this is"),
    };
    let expires = expiry(mine, refresh);
    let asked = p.scopes.join(" ");
    let scopes = mine["scope"].as_str().unwrap_or(&asked);
    for instance in instances {
        let a = Kept { realm, instance, provider: name, account: &account, name: &display, refresh, access, expires, scopes };
        keep(st, &a, None).await?;
    }
    Ok(account)
}

/// What's at a dotted path in a JSON value (`""`: the value itself).
fn at<'v>(v: &'v Value, path: &str) -> &'v Value {
    path.split('.').filter(|k| !k.is_empty()).fold(v, |v, k| &v[k])
}

/// When a token expires: its `expires_in`; one that can't be renewed and
/// says nothing doesn't expire.
fn expiry(tokens: &Value, refresh: &str) -> i64 {
    match tokens["expires_in"].as_i64() {
        Some(s) => c6_core::now_ms() + s * 1000,
        None if refresh.is_empty() => i64::MAX / 2,
        None => c6_core::now_ms() + 3600 * 1000,
    }
}

/// An account to keep for an app.
pub struct Kept<'a> {
    pub realm: &'a str,
    pub instance: &'a str,
    pub provider: &'a str,
    pub account: &'a str,
    pub name: &'a str,
    pub refresh: &'a str,
    pub access: &'a str,
    pub expires: i64,
    pub scopes: &'a str,
}

/// Keeps (or renews) an app's account; `id` as given (accounts moved in
/// keep theirs), else new.
pub async fn keep(st: &HeadState, a: &Kept<'_>, id: Option<&str>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO oauth_accounts (id, realm, instance, provider, account, name, refresh_token, access_token, access_expires, scopes, added_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (realm, instance, provider, account) DO UPDATE SET name = excluded.name, \
         refresh_token = excluded.refresh_token, access_token = excluded.access_token, access_expires = excluded.access_expires, \
         scopes = excluded.scopes",
    )
    .bind(id.map(str::to_owned).unwrap_or_else(|| uuid::Uuid::now_v7().simple().to_string()[20..].to_owned()))
    .bind(a.realm)
    .bind(a.instance)
    .bind(a.provider)
    .bind(a.account)
    .bind(a.name)
    .bind(st.secrets.encrypt(a.refresh)?)
    .bind(st.secrets.encrypt(a.access)?)
    .bind(a.expires)
    .bind(a.scopes)
    .bind(c6_core::now_ms())
    .execute(&st.store.pool)
    .await?;
    Ok(())
}

/// The accounts apps of a realm connected that feed carbon6's stores
/// (mail, calendar, contacts), for the knowledge store: {id, email, name,
/// app, feeds}.
pub async fn feeding(st: &HeadState, realm: &str) -> anyhow::Result<Value> {
    let mut out = Vec::new();
    for i in crate::instances::list(&st.store.pool, Some(realm)).await? {
        if i.package.feeds.is_empty() {
            continue;
        }
        for a in accounts(st, realm, &i.name).await? {
            out.push(json!({"id": a.id, "email": a.account, "name": a.name, "app": i.name, "feeds": i.package.feeds}));
        }
    }
    Ok(Value::Array(out))
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
    // A renewed token is where the first was, or at the top.
    let mine = if at(&tokens, &p.token_path)["access_token"].is_string() { at(&tokens, &p.token_path) } else { &tokens };
    let access =
        mine["access_token"].as_str().ok_or_else(|| anyhow::anyhow!("{name} refused to refresh the sign-in: {}", tokens["error"]))?;
    let expires = expiry(mine, &refresh);
    sqlx::query("UPDATE oauth_accounts SET access_token = ?, access_expires = ? WHERE id = ?")
        .bind(st.secrets.encrypt(access)?)
        .bind(expires)
        .bind(&id)
        .execute(&st.store.pool)
        .await?;
    Ok((access.to_owned(), expires))
}

/// The access tokens an app's kit jobs get: `C6_ACCOUNTS`, a JSON list of
/// {id, provider, account, access_token, expires_at}. An account whose sign-in
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
            Ok((token, expires)) => list.push(json!({"id": r.get::<String, _>("id"), "provider": r.get::<String, _>("provider"),
                "account": r.get::<String, _>("account"), "access_token": token, "expires_at": expires})),
            Err(e) => warn!(%instance, account = %r.get::<String, _>("account"), "access token: {e:#}"),
        }
    }
    Ok(vec![("C6_ACCOUNTS".to_owned(), Value::Array(list).to_string())])
}
