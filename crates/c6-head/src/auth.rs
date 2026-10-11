//! Web UI sign-in: passkeys, plus one-time login links for the first
//! sign-in and recovery (`c6h login-link`, run on the head).
//!
//! A signed-in browser holds a random session token in an HttpOnly cookie;
//! the head stores its SHA-256. Every `/api/` request needs a session
//! except the sign-in endpoints themselves. Scripts and the `c6` CLI use
//! API tokens instead (`Authorization: Bearer c6_…`), which reach
//! everything but sign-in management (`/api/v1/auth/`). Static assets are public, and
//! the ChatGPT OAuth callback is protected by its own `state`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use c6_api as api;
use c6_core::now_ms;
use sqlx::Row;

use crate::store::Store;
use crate::webauthn;
use crate::{HeadState, bad_request};

const COOKIE: &str = "c6_session";
const SESSION_MS: i64 = 90 * 24 * 3600 * 1000;
const LINK_MS: i64 = 15 * 60 * 1000;
const CODE_MS: i64 = 10 * 60 * 1000;
/// Wrong codes tolerated before every outstanding code is cancelled.
const CODE_TRIES: u32 = 5;
/// Crockford base32: no I, L, O or U, so codes read back unambiguously.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CHALLENGE_MS: i64 = 5 * 60 * 1000;
/// `last_seen_at` is refreshed at most this often.
const TOUCH_MS: i64 = 10 * 60 * 1000;
/// The one user's WebAuthn user handle (stable per head).
const USER_ID_KEY: &str = "auth.user_id";

/// Marks a request that arrived over TLS (set by the UI server).
#[derive(Clone, Copy)]
pub struct Tls;

pub struct Auth {
    pub required: bool,
    challenges: Mutex<HashMap<String, Pending>>,
    /// Wrong one-time codes since the last cancellation.
    code_failures: Mutex<u32>,
}

struct Pending {
    register: bool,
    rp_id: String,
    origin: String,
    expires: i64,
}

impl Auth {
    pub fn new(required: bool) -> Self {
        Self { required, challenges: Mutex::default(), code_failures: Mutex::default() }
    }

    fn issue(&self, register: bool, here: &Here) -> String {
        let challenge = webauthn::encode(&rand::random::<[u8; 32]>());
        let now = now_ms();
        let mut c = self.challenges.lock().unwrap();
        c.retain(|_, p| p.expires > now);
        c.insert(
            challenge.clone(),
            Pending { register, rp_id: here.rp_id.clone().unwrap_or_default(), origin: here.origin.clone(), expires: now + CHALLENGE_MS },
        );
        challenge
    }

    /// Takes the challenge a ceremony answers, if we issued it for this kind
    /// of ceremony and it is still fresh.
    fn take(&self, challenge: &str, register: bool) -> Option<Pending> {
        let p = self.challenges.lock().unwrap().remove(challenge)?;
        (p.register == register && p.expires > now_ms()).then_some(p)
    }
}

/// Where the page making a request was loaded from.
struct Here {
    origin: String,
    /// The WebAuthn RP id: the host name, when passkeys can work here (a
    /// secure context with a name, not an IP).
    rp_id: Option<String>,
}

fn here(req_headers: &HeaderMap, tls: bool) -> Here {
    let host = req_headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or("").to_owned(),
        None => host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h).to_owned(),
    };
    let is_ip = name.parse::<std::net::IpAddr>().is_ok();
    let localhost = name == "localhost" || name.ends_with(".localhost");
    let rp_id = (!is_ip && !name.is_empty() && (tls || localhost)).then_some(name);
    Here { origin: format!("{}://{host}", if tls { "https" } else { "http" }), rp_id }
}

fn sha256_hex(s: &str) -> String {
    c6_core::sha256_hex(s.as_bytes())
}

fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == COOKIE && !v.is_empty()).then(|| v.to_owned())
    })
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn session_cookie(token: &str, secure: bool, max_age_s: i64) -> String {
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_s}{}", if secure { "; Secure" } else { "" })
}

// --- Store ----------------------------------------------------------------------

impl Store {
    /// A new one-time login link token, valid for 15 minutes.
    pub async fn create_login_link(&self) -> anyhow::Result<String> {
        let token = new_token();
        let now = now_ms();
        sqlx::query("INSERT INTO login_links (token_hash, created_at, expires_at) VALUES (?, ?, ?)")
            .bind(sha256_hex(&token))
            .bind(now)
            .bind(now + LINK_MS)
            .execute(&self.pool)
            .await?;
        Ok(token)
    }

    async fn redeem_login_link(&self, token: &str) -> anyhow::Result<bool> {
        self.redeem(token, "link").await
    }

    /// Uses up a link or code of `kind`. Each kind is only accepted at its
    /// own endpoint: short codes must not be guessable through the link one.
    async fn redeem(&self, secret: &str, kind: &str) -> anyhow::Result<bool> {
        let now = now_ms();
        let done =
            sqlx::query("UPDATE login_links SET used_at = ? WHERE token_hash = ? AND kind = ? AND used_at IS NULL AND expires_at > ?")
                .bind(now)
                .bind(sha256_hex(secret))
                .bind(kind)
                .bind(now)
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected() == 1)
    }

    /// A new one-time code ("K7QM-2XRD"), valid for 10 minutes.
    async fn create_login_code(&self) -> anyhow::Result<(String, i64)> {
        let raw: String = rand::random::<[u8; 8]>().iter().map(|b| CODE_ALPHABET[(*b & 31) as usize] as char).collect();
        let now = now_ms();
        sqlx::query("INSERT INTO login_links (token_hash, created_at, expires_at, kind) VALUES (?, ?, ?, 'code')")
            .bind(sha256_hex(&raw))
            .bind(now)
            .bind(now + CODE_MS)
            .execute(&self.pool)
            .await?;
        Ok((format!("{}-{}", &raw[..4], &raw[4..]), now + CODE_MS))
    }

    async fn cancel_login_codes(&self) -> anyhow::Result<()> {
        sqlx::query("UPDATE login_links SET used_at = ? WHERE kind = 'code' AND used_at IS NULL")
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn create_auth_session(&self, method: &str, user_agent: Option<&str>) -> anyhow::Result<String> {
        let token = new_token();
        let now = now_ms();
        sqlx::query(
            "INSERT INTO auth_sessions (token_hash, method, user_agent, created_at, last_seen_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(sha256_hex(&token))
        .bind(method)
        .bind(user_agent)
        .bind(now)
        .bind(now)
        .bind(now + SESSION_MS)
        .execute(&self.pool)
        .await?;
        Ok(token)
    }

    /// Whether `token` is a live session; keeps it alive.
    async fn check_auth_session(&self, token: &str) -> anyhow::Result<bool> {
        let hash = sha256_hex(token);
        let now = now_ms();
        let row = sqlx::query("SELECT last_seen_at FROM auth_sessions WHERE token_hash = ? AND expires_at > ?")
            .bind(&hash)
            .bind(now)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(false) };
        if now - row.get::<i64, _>("last_seen_at") > TOUCH_MS {
            sqlx::query("UPDATE auth_sessions SET last_seen_at = ?, expires_at = ? WHERE token_hash = ?")
                .bind(now)
                .bind(now + SESSION_MS)
                .bind(&hash)
                .execute(&self.pool)
                .await?;
        }
        Ok(true)
    }

    /// A new API token; the secret is returned once.
    pub async fn create_api_token(&self, name: &str) -> anyhow::Result<api::ApiToken> {
        let token = format!("c6_{}", new_token());
        let id = uuid::Uuid::now_v7().to_string();
        let now = now_ms();
        sqlx::query("INSERT INTO api_tokens (id, name, token_hash, created_at) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(name)
            .bind(sha256_hex(&token))
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(api::ApiToken { id, name: name.to_owned(), created_at: now, last_used_at: None, token: Some(token) })
    }

    async fn check_api_token(&self, token: &str) -> anyhow::Result<bool> {
        let hash = sha256_hex(token);
        let now = now_ms();
        let Some(row) =
            sqlx::query("SELECT last_used_at FROM api_tokens WHERE token_hash = ?").bind(&hash).fetch_optional(&self.pool).await?
        else {
            return Ok(false);
        };
        if now - row.get::<Option<i64>, _>("last_used_at").unwrap_or(0) > TOUCH_MS {
            sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE token_hash = ?").bind(now).bind(&hash).execute(&self.pool).await?;
        }
        Ok(true)
    }

    async fn api_tokens(&self) -> anyhow::Result<Vec<api::ApiToken>> {
        let rows =
            sqlx::query("SELECT id, name, created_at, last_used_at FROM api_tokens ORDER BY created_at").fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| api::ApiToken {
                id: r.get("id"),
                name: r.get("name"),
                created_at: r.get("created_at"),
                last_used_at: r.get("last_used_at"),
                token: None,
            })
            .collect())
    }

    async fn delete_auth_session(&self, id_prefix: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM auth_sessions WHERE substr(token_hash, 1, 16) = ?").bind(id_prefix).execute(&self.pool).await?;
        Ok(())
    }

    async fn auth_sessions(&self) -> anyhow::Result<Vec<(String, api::AuthSession)>> {
        let rows = sqlx::query("SELECT * FROM auth_sessions WHERE expires_at > ? ORDER BY last_seen_at DESC")
            .bind(now_ms())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let hash: String = r.get("token_hash");
                let s = api::AuthSession {
                    id: hash[..16].to_owned(),
                    method: r.get("method"),
                    user_agent: r.get("user_agent"),
                    created_at: r.get("created_at"),
                    last_seen_at: r.get("last_seen_at"),
                    current: false,
                };
                (hash, s)
            })
            .collect())
    }

    async fn passkeys(&self) -> anyhow::Result<Vec<api::Passkey>> {
        let rows =
            sqlx::query("SELECT id, name, rp_id, created_at, last_used_at FROM passkeys ORDER BY created_at").fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| api::Passkey {
                id: r.get("id"),
                name: r.get("name"),
                rp_id: r.get("rp_id"),
                created_at: r.get("created_at"),
                last_used_at: r.get("last_used_at"),
            })
            .collect())
    }

    async fn user_handle(&self) -> anyhow::Result<String> {
        if let Some(id) = self.setting(USER_ID_KEY).await? {
            return Ok(id);
        }
        let id = webauthn::encode(&rand::random::<[u8; 16]>());
        self.set_setting(USER_ID_KEY, &id).await?;
        Ok(id)
    }
}

// --- Middleware -----------------------------------------------------------------

/// Endpoints reachable without a session.
const PUBLIC: [&str; 6] = [
    "/api/v1/auth/status",
    "/api/v1/auth/link",
    "/api/v1/auth/code",
    "/api/v1/auth/logout",
    "/api/v1/auth/passkeys/login/begin",
    "/api/v1/auth/passkeys/login/finish",
];

pub async fn require(State(st): State<Arc<HeadState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    // Webhooks carry their own credential (the token in the URL).
    if !st.auth.required || !path.starts_with("/api/") || PUBLIC.contains(&path) || path.starts_with("/api/v1/webhooks/") {
        return next.run(req).await;
    }
    if let Some(token) = bearer_token(req.headers()) {
        // API tokens cannot manage sign-in (no minting codes, tokens or
        // passkeys with one).
        let ok = !path.starts_with("/api/v1/auth/")
            && st.store.check_api_token(&token).await.unwrap_or_else(|e| {
                tracing::warn!("checking API token: {e:#}");
                false
            });
        return if ok {
            next.run(req).await
        } else {
            (StatusCode::UNAUTHORIZED, Json(api::ApiError { error: "invalid API token".into() })).into_response()
        };
    }
    let ok = match cookie_token(req.headers()) {
        Some(t) => st.store.check_auth_session(&t).await.unwrap_or_else(|e| {
            tracing::warn!("checking session: {e:#}");
            false
        }),
        None => false,
    };
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(api::ApiError { error: "sign in required".into() })).into_response()
    }
}

// --- Endpoints ------------------------------------------------------------------

pub fn routes() -> Router<Arc<HeadState>> {
    Router::new()
        .route("/api/v1/auth/status", get(status))
        .route("/api/v1/auth/link", post(redeem_link))
        .route("/api/v1/auth/code", post(redeem_code))
        .route("/api/v1/auth/codes", post(create_code))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/passkeys/login/begin", post(login_begin))
        .route("/api/v1/auth/passkeys/login/finish", post(login_finish))
        .route("/api/v1/auth/passkeys/register/begin", post(register_begin))
        .route("/api/v1/auth/passkeys/register/finish", post(register_finish))
        .route("/api/v1/auth/passkeys", get(list_passkeys))
        .route("/api/v1/auth/passkeys/{id}", delete(delete_passkey).patch(rename_passkey))
        .route("/api/v1/auth/sessions", get(list_sessions))
        .route("/api/v1/auth/sessions/{id}", delete(delete_session))
        .route("/api/v1/auth/tokens", get(list_tokens).post(create_token))
        .route("/api/v1/auth/tokens/{id}", delete(delete_token))
}

type Res<T> = Result<T, crate::api::ApiErr>;

fn is_tls(req_ext: Option<axum::Extension<Tls>>) -> bool {
    req_ext.is_some()
}

async fn status(State(st): State<Arc<HeadState>>, tls: Option<axum::Extension<Tls>>, headers: HeaderMap) -> Res<Json<api::AuthStatus>> {
    let here = here(&headers, is_tls(tls));
    let authenticated = match cookie_token(&headers) {
        Some(t) => st.store.check_auth_session(&t).await?,
        None => false,
    };
    let has_passkeys = match &here.rp_id {
        Some(rp) => st.store.passkeys().await?.iter().any(|p| &p.rp_id == rp),
        None => false,
    };
    Ok(Json(api::AuthStatus {
        authenticated: authenticated || !st.auth.required,
        required: st.auth.required,
        passkeys_here: here.rp_id.is_some(),
        has_passkeys,
        head_ca: st.ui_ca.is_some(),
    }))
}

/// Starts a session and sets its cookie.
async fn sign_in(st: &HeadState, method: &str, headers: &HeaderMap, secure: bool) -> Res<Response> {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().take(200).collect::<String>());
    let token = st.store.create_auth_session(method, ua.as_deref()).await?;
    tracing::info!(method, "browser signed in");
    Ok(([(header::SET_COOKIE, session_cookie(&token, secure, SESSION_MS / 1000))], StatusCode::NO_CONTENT).into_response())
}

async fn redeem_link(
    State(st): State<Arc<HeadState>>,
    tls: Option<axum::Extension<Tls>>,
    headers: HeaderMap,
    Json(req): Json<api::RedeemLink>,
) -> Res<Response> {
    if !st.store.redeem_login_link(req.token.trim()).await? {
        return Err(bad_request("this login link is invalid, expired or already used; run `c6h login-link` for a new one").into());
    }
    sign_in(&st, "link", &headers, is_tls(tls)).await
}

/// The code as typed: case, spaces and dashes don't matter, and the
/// look-alikes O, I and L read as 0, 1 and 1.
fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

async fn create_code(State(st): State<Arc<HeadState>>) -> Res<Json<api::LoginCode>> {
    let (code, expires_at) = st.store.create_login_code().await?;
    tracing::info!("one-time sign-in code created");
    Ok(Json(api::LoginCode { code, expires_at }))
}

async fn redeem_code(
    State(st): State<Arc<HeadState>>,
    tls: Option<axum::Extension<Tls>>,
    headers: HeaderMap,
    Json(req): Json<api::RedeemCode>,
) -> Res<Response> {
    if st.store.redeem(&normalize_code(&req.code), "code").await? {
        *st.auth.code_failures.lock().unwrap() = 0;
        return sign_in(&st, "code", &headers, is_tls(tls)).await;
    }
    let cancel = {
        let mut failures = st.auth.code_failures.lock().unwrap();
        *failures += 1;
        let cancel = *failures >= CODE_TRIES;
        if cancel {
            *failures = 0;
        }
        cancel
    };
    if cancel {
        tracing::warn!("too many wrong sign-in codes: cancelled all outstanding codes");
        st.store.cancel_login_codes().await?;
        return Err(bad_request("too many wrong codes: all codes were cancelled; create a new one").into());
    }
    Err(bad_request("that code is wrong, expired or already used").into())
}

async fn logout(State(st): State<Arc<HeadState>>, tls: Option<axum::Extension<Tls>>, headers: HeaderMap) -> Res<Response> {
    if let Some(t) = cookie_token(&headers) {
        st.store.delete_auth_session(&sha256_hex(&t)[..16]).await?;
    }
    Ok(([(header::SET_COOKIE, session_cookie("", is_tls(tls), 0))], StatusCode::NO_CONTENT).into_response())
}

fn passkey_host(here: &Here) -> Res<String> {
    here.rp_id.clone().ok_or_else(|| {
        bad_request("passkeys need HTTPS and a host name (or localhost); open the head by its HTTPS name, or use a login link").into()
    })
}

async fn login_begin(
    State(st): State<Arc<HeadState>>,
    tls: Option<axum::Extension<Tls>>,
    headers: HeaderMap,
) -> Res<Json<serde_json::Value>> {
    let here = here(&headers, is_tls(tls));
    let rp_id = passkey_host(&here)?;
    let challenge = st.auth.issue(false, &here);
    // Discoverable credentials: the browser offers the passkeys it has for
    // this site, no user name needed.
    Ok(Json(serde_json::json!({
        "challenge": challenge, "rpId": rp_id, "userVerification": "required", "timeout": 120000, "allowCredentials": [],
    })))
}

/// The challenge a ceremony answers, from its clientDataJSON.
fn challenge_of(client_data_json: &str) -> Option<String> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(client_data_json.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    Some(v["challenge"].as_str()?.trim_end_matches('=').to_owned())
}

async fn login_finish(
    State(st): State<Arc<HeadState>>,
    tls: Option<axum::Extension<Tls>>,
    headers: HeaderMap,
    Json(a): Json<webauthn::Assertion>,
) -> Res<Response> {
    let secure = is_tls(tls);
    let fail = || -> crate::api::ApiErr { bad_request("passkey sign-in failed").into() };
    let challenge = challenge_of(&a.client_data_json).ok_or_else(fail)?;
    let pending = st.auth.take(&challenge, false).ok_or_else(fail)?;
    let id = webauthn::encode(
        &base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, a.raw_id.trim_end_matches('=')).map_err(|_| fail())?,
    );
    let row = sqlx::query("SELECT public_key, alg, sign_count FROM passkeys WHERE id = ? AND rp_id = ?")
        .bind(&id)
        .bind(&pending.rp_id)
        .fetch_optional(&st.store.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let Some(row) = row else {
        return Err(bad_request("this passkey is not registered with this head (it may have been removed)").into());
    };
    let expect = webauthn::Expect { challenge: &challenge, origin: &pending.origin, rp_id: &pending.rp_id };
    let count = webauthn::verify_assertion(
        &a,
        &expect,
        &row.get::<Vec<u8>, _>("public_key"),
        row.get("alg"),
        row.get::<i64, _>("sign_count") as u32,
    )
    .map_err(|e| {
        tracing::warn!("passkey sign-in rejected: {e:#}");
        fail()
    })?;
    sqlx::query("UPDATE passkeys SET sign_count = ?, last_used_at = ? WHERE id = ?")
        .bind(count as i64)
        .bind(now_ms())
        .bind(&id)
        .execute(&st.store.pool)
        .await
        .map_err(anyhow::Error::from)?;
    sign_in(&st, "passkey", &headers, secure).await
}

async fn register_begin(
    State(st): State<Arc<HeadState>>,
    tls: Option<axum::Extension<Tls>>,
    headers: HeaderMap,
) -> Res<Json<serde_json::Value>> {
    let here = here(&headers, is_tls(tls));
    let rp_id = passkey_host(&here)?;
    let user = st.store.user_handle().await?;
    let exclude: Vec<_> = st
        .store
        .passkeys()
        .await?
        .into_iter()
        .filter(|p| p.rp_id == rp_id)
        .map(|p| serde_json::json!({"type": "public-key", "id": p.id}))
        .collect();
    let challenge = st.auth.issue(true, &here);
    Ok(Json(serde_json::json!({
        "challenge": challenge,
        "rp": {"id": rp_id, "name": "carbon6"},
        "user": {"id": user, "name": "carbon6", "displayName": "carbon6"},
        "pubKeyCredParams": [
            {"type": "public-key", "alg": webauthn::ES256},
            {"type": "public-key", "alg": webauthn::EDDSA},
            {"type": "public-key", "alg": webauthn::RS256},
        ],
        "authenticatorSelection": {"residentKey": "required", "requireResidentKey": true, "userVerification": "required"},
        "attestation": "none",
        "excludeCredentials": exclude,
        "timeout": 120000,
    })))
}

async fn register_finish(State(st): State<Arc<HeadState>>, Json(req): Json<api::RegisterPasskey>) -> Res<Json<api::Passkey>> {
    let reg: webauthn::Registration = serde_json::from_value(req.credential).map_err(|e| bad_request(format!("malformed passkey: {e}")))?;
    let challenge = challenge_of(&reg.client_data_json).ok_or_else(|| bad_request("malformed passkey"))?;
    let pending = st.auth.take(&challenge, true).ok_or_else(|| bad_request("the passkey request expired; try again"))?;
    let expect = webauthn::Expect { challenge: &challenge, origin: &pending.origin, rp_id: &pending.rp_id };
    let cred = webauthn::verify_registration(&reg, &expect).map_err(|e| bad_request(format!("passkey rejected: {e}")))?;
    let name = Some(req.name.trim()).filter(|n| !n.is_empty()).unwrap_or("Passkey").chars().take(80).collect::<String>();
    let now = now_ms();
    sqlx::query("INSERT INTO passkeys (id, rp_id, public_key, alg, name, sign_count, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&cred.id)
        .bind(&pending.rp_id)
        .bind(&cred.public_key)
        .bind(cred.alg)
        .bind(&name)
        .bind(cred.sign_count as i64)
        .bind(now)
        .execute(&st.store.pool)
        .await
        .map_err(|_| bad_request("this passkey is already registered"))?;
    tracing::info!(name, rp_id = %pending.rp_id, "passkey registered");
    Ok(Json(api::Passkey { id: cred.id, name, rp_id: pending.rp_id, created_at: now, last_used_at: None }))
}

async fn list_passkeys(State(st): State<Arc<HeadState>>) -> Res<Json<Vec<api::Passkey>>> {
    Ok(Json(st.store.passkeys().await?))
}

async fn rename_passkey(State(st): State<Arc<HeadState>>, Path(id): Path<String>, Json(req): Json<api::RenamePasskey>) -> Res<StatusCode> {
    let name = req.name.trim().chars().take(80).collect::<String>();
    if name.is_empty() {
        return Err(bad_request("a passkey needs a name").into());
    }
    let done = sqlx::query("UPDATE passkeys SET name = ? WHERE id = ?")
        .bind(&name)
        .bind(&id)
        .execute(&st.store.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if done.rows_affected() == 0 {
        return Err(crate::not_found("no such passkey").into());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_passkey(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> Res<StatusCode> {
    sqlx::query("DELETE FROM passkeys WHERE id = ?").bind(&id).execute(&st.store.pool).await.map_err(anyhow::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(State(st): State<Arc<HeadState>>, headers: HeaderMap) -> Res<Json<Vec<api::AuthSession>>> {
    let mine = cookie_token(&headers).map(|t| sha256_hex(&t));
    Ok(Json(
        st.store
            .auth_sessions()
            .await?
            .into_iter()
            .map(|(hash, mut s)| {
                s.current = mine.as_deref() == Some(hash.as_str());
                s
            })
            .collect(),
    ))
}

async fn delete_session(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> Res<StatusCode> {
    st.store.delete_auth_session(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_tokens(State(st): State<Arc<HeadState>>) -> Res<Json<Vec<api::ApiToken>>> {
    Ok(Json(st.store.api_tokens().await?))
}

async fn create_token(State(st): State<Arc<HeadState>>, Json(req): Json<api::CreateApiToken>) -> Res<(StatusCode, Json<api::ApiToken>)> {
    let name = req.name.trim().chars().take(80).collect::<String>();
    if name.is_empty() {
        return Err(bad_request("a token needs a name").into());
    }
    Ok((StatusCode::CREATED, Json(st.store.create_api_token(&name).await?)))
}

async fn delete_token(State(st): State<Arc<HeadState>>, Path(id): Path<String>) -> Res<StatusCode> {
    sqlx::query("DELETE FROM api_tokens WHERE id = ?").bind(&id).execute(&st.store.pool).await.map_err(anyhow::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use serde_json::{Value, json};

    use crate::webauthn::{self, tests::SoftKey};

    /// The session cookie a response set.
    fn cookie(r: &reqwest::Response) -> String {
        let c = r.headers().get("set-cookie").expect("sets a cookie").to_str().unwrap();
        c.split(';').next().unwrap().to_owned()
    }

    #[tokio::test]
    async fn links_passkeys_and_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            ..Default::default()
        })
        .await
        .unwrap();
        // `localhost` is a secure context, so passkeys work over plain HTTP.
        let origin = format!("http://localhost:{}", head.ui_addrs[0].port());
        let api = format!("{origin}/api/v1");
        let http = reqwest::Client::new();
        let get = |path: &str, cookie: Option<&str>| {
            let mut r = http.get(format!("{api}{path}"));
            if let Some(c) = cookie {
                r = r.header("cookie", c);
            }
            r.send()
        };
        let post = |path: &str, cookie: Option<&str>, body: Value| {
            let mut r = http.post(format!("{api}{path}")).json(&body);
            if let Some(c) = cookie {
                r = r.header("cookie", c);
            }
            r.send()
        };

        // Signed out: the API is closed, status is open.
        assert_eq!(get("/workers", None).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let st: Value = get("/auth/status", None).await.unwrap().json().await.unwrap();
        assert_eq!((st["authenticated"].as_bool(), st["passkeys_here"].as_bool()), (Some(false), Some(true)));

        // A login link works once.
        let token = head.state.store.create_login_link().await.unwrap();
        let r = post("/auth/link", None, json!({"token": token})).await.unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let link_session = cookie(&r);
        assert_eq!(post("/auth/link", None, json!({"token": token})).await.unwrap().status(), StatusCode::BAD_REQUEST);
        assert_eq!(get("/workers", Some(&link_session)).await.unwrap().status(), StatusCode::OK);

        // Register a passkey while signed in.
        let key = SoftKey::new();
        assert_eq!(post("/auth/passkeys/register/begin", None, json!({})).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let opts: Value = post("/auth/passkeys/register/begin", Some(&link_session), json!({})).await.unwrap().json().await.unwrap();
        assert_eq!(opts["rp"]["id"], "localhost");
        let cred = key.register(opts["challenge"].as_str().unwrap(), &origin, "localhost");
        let r = post("/auth/passkeys/register/finish", Some(&link_session), json!({"name": "laptop", "credential": cred})).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());

        // Rename it.
        let id = webauthn::encode(&key.id);
        let rename = |name: &str| {
            http.patch(format!("{api}/auth/passkeys/{id}")).header("cookie", &link_session).json(&json!({"name": name})).send()
        };
        assert_eq!(rename("  ").await.unwrap().status(), StatusCode::BAD_REQUEST);
        assert_eq!(rename("Google Password Manager").await.unwrap().status(), StatusCode::NO_CONTENT);
        let keys: Vec<Value> = get("/auth/passkeys", Some(&link_session)).await.unwrap().json().await.unwrap();
        assert_eq!(keys[0]["name"], "Google Password Manager");

        // Sign out, then back in with the passkey.
        post("/auth/logout", Some(&link_session), json!({})).await.unwrap();
        assert_eq!(get("/workers", Some(&link_session)).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let opts: Value = post("/auth/passkeys/login/begin", None, json!({})).await.unwrap().json().await.unwrap();
        let challenge = opts["challenge"].as_str().unwrap();
        let wrong = key.sign_in(challenge, "http://evil.localhost:1", "localhost", 0);
        assert_eq!(post("/auth/passkeys/login/finish", None, wrong).await.unwrap().status(), StatusCode::BAD_REQUEST);
        // A failed attempt used up that challenge.
        let opts: Value = post("/auth/passkeys/login/begin", None, json!({})).await.unwrap().json().await.unwrap();
        let r = post("/auth/passkeys/login/finish", None, key.sign_in(opts["challenge"].as_str().unwrap(), &origin, "localhost", 0))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let passkey_session = cookie(&r);
        assert_eq!(get("/workers", Some(&passkey_session)).await.unwrap().status(), StatusCode::OK);
        let st: Value = get("/auth/status", Some(&passkey_session)).await.unwrap().json().await.unwrap();
        assert_eq!((st["authenticated"].as_bool(), st["has_passkeys"].as_bool()), (Some(true), Some(true)));

        // A one-time code from a signed-in browser signs in another one.
        assert_eq!(post("/auth/codes", None, json!({})).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let code: Value = post("/auth/codes", Some(&passkey_session), json!({})).await.unwrap().json().await.unwrap();
        let code = code["code"].as_str().unwrap().to_owned();
        assert_eq!(code.len(), 9, "{code}");
        // Not accepted as a link token.
        assert_eq!(post("/auth/link", None, json!({"token": code})).await.unwrap().status(), StatusCode::BAD_REQUEST);
        // Case and dashes don't matter.
        let typed = code.replace('-', " ").to_lowercase();
        let r = post("/auth/code", None, json!({"code": typed})).await.unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(get("/workers", Some(&cookie(&r))).await.unwrap().status(), StatusCode::OK);
        assert_eq!(post("/auth/code", None, json!({"code": code})).await.unwrap().status(), StatusCode::BAD_REQUEST, "single use");

        // Guessing: five wrong codes cancel the outstanding ones.
        let fresh: Value = post("/auth/codes", Some(&passkey_session), json!({})).await.unwrap().json().await.unwrap();
        // (The reused code above already counted as one.)
        let mut cancelled = false;
        for _ in 0..5 {
            let r = post("/auth/code", None, json!({"code": "0000-0000"})).await.unwrap();
            cancelled |= r.text().await.unwrap().contains("cancelled");
        }
        assert!(cancelled);
        let r = post("/auth/code", None, json!({"code": fresh["code"]})).await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "cancelled after too many wrong codes");

        // Sessions can be listed and revoked.
        let sessions: Vec<Value> = get("/auth/sessions", Some(&passkey_session)).await.unwrap().json().await.unwrap();
        let mine = sessions.iter().find(|s| s["current"] == true).unwrap();
        assert_eq!(mine["method"], "passkey");
        http.delete(format!("{api}/auth/sessions/{}", mine["id"].as_str().unwrap()))
            .header("cookie", &passkey_session)
            .send()
            .await
            .unwrap();
        assert_eq!(get("/workers", Some(&passkey_session)).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn no_passkeys_on_plain_http_to_an_address() {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            ..Default::default()
        })
        .await
        .unwrap();
        let api = format!("http://{}/api/v1", head.ui_addrs[0]);
        let st: Value = reqwest::get(format!("{api}/auth/status")).await.unwrap().json().await.unwrap();
        assert_eq!(st["passkeys_here"], false);
        let r = reqwest::Client::new().post(format!("{api}/auth/passkeys/login/begin")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn api_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            ..Default::default()
        })
        .await
        .unwrap();
        let api = format!("http://localhost:{}/api/v1", head.ui_addrs[0].port());
        let http = reqwest::Client::new();
        // A browser session (from a login link) creates a token.
        let link = head.state.store.create_login_link().await.unwrap();
        let r = http.post(format!("{api}/auth/link")).json(&json!({"token": link})).send().await.unwrap();
        let c = cookie(&r);
        let created: Value = http
            .post(format!("{api}/auth/tokens"))
            .header("cookie", &c)
            .json(&json!({"name": "ci"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = created["token"].as_str().unwrap().to_owned();
        assert!(token.starts_with("c6_"));

        let with = |path: &str, t: &str| http.get(format!("{api}{path}")).bearer_auth(t).send();
        assert_eq!(with("/sessions", &token).await.unwrap().status(), StatusCode::OK);
        assert_eq!(with("/sessions", "c6_wrong").await.unwrap().status(), StatusCode::UNAUTHORIZED);
        // Not sign-in management.
        assert_eq!(with("/auth/tokens", &token).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let r = http.post(format!("{api}/auth/codes")).bearer_auth(&token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // Listed without the secret; revoked, it stops working.
        let list: Value = http.get(format!("{api}/auth/tokens")).header("cookie", &c).send().await.unwrap().json().await.unwrap();
        assert_eq!(list[0]["name"], "ci");
        assert!(list[0].get("token").is_none());
        let id = list[0]["id"].as_str().unwrap();
        http.delete(format!("{api}/auth/tokens/{id}")).header("cookie", &c).send().await.unwrap();
        assert_eq!(with("/sessions", &token).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        head.shutdown().await.unwrap();
    }
}
