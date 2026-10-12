//! "Sign in with ChatGPT" for open-source apps (OAuth 2.0 authorization code
//! with PKCE and dynamic client registration; no client secret). The head is
//! the only place tokens are exchanged and refreshed; workers receive
//! short-lived access tokens.
//!
//! The redirect must be a loopback URL. It points at the head's own UI port,
//! so a browser on the head machine completes the flow directly; from any
//! other machine the user pastes the final address back into the UI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bad_request;

const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
/// Where the ID token's signing keys and issuer are published.
const DISCOVERY_URL: &str = "https://auth.openai.com/.well-known/openid-configuration";
/// How long fetched signing keys are reused.
const KEYS_TTL_MS: i64 = 60 * 60 * 1000;
pub const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Required scope for using the ChatGPT plan.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// First-time registration entrypoint; the callback returns the real id.
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
/// The name OpenAI shows for carbon6 on first registration.
const AGENT_NAME: &str = "carbon6";
/// The loopback redirect's path (fixed by OpenAI; only the port varies).
pub const CALLBACK_PATH: &str = "/callback";
const PENDING_TTL_MS: i64 = 15 * 60 * 1000;
/// Refresh this long before expiry.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// What is stored (encrypted) for a `chatgpt` connection: one ChatGPT
/// account (or workspace), with the client id issued for it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatGptTokens {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
    pub scope: String,
    /// The account: the ID token's verified subject, and its email (empty
    /// for sign-ins from before carbon6 kept them).
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub email: String,
    /// The ChatGPT workspace, when the ID token names one.
    #[serde(default)]
    pub workspace: String,
    /// The last ID token, to name the account when signing in again.
    #[serde(default)]
    pub id_token: String,
}

/// Which account a sign-in is for.
#[derive(Clone, Debug, Default)]
pub struct Reauth {
    pub connection_id: String,
    pub client_id: String,
    pub id_token: String,
    pub sub: String,
}

struct Pending {
    verifier: String,
    redirect_uri: String,
    client_id: String,
    nonce: String,
    name: String,
    reauth: Option<Reauth>,
    created_ms: i64,
}

#[derive(Default)]
pub struct OAuth {
    pending: Mutex<HashMap<String, Pending>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// OpenAI's signing keys and issuer, with when they were fetched.
    keys: tokio::sync::Mutex<Option<(i64, Keys)>>,
}

pub struct Completed {
    pub name: String,
    pub tokens: ChatGptTokens,
    /// The connection signed in again, if this was a re-sign-in.
    pub reauth: Option<Reauth>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    id_token: Option<String>,
}

#[derive(Clone)]
struct Keys {
    issuer: String,
    jwks: Vec<serde_json::Value>,
}

/// Who an ID token says signed in.
#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    pub sub: String,
    pub email: String,
    pub workspace: String,
}

fn random_b64(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

impl OAuth {
    /// Starts a sign-in and returns the URL to open: a new account (or
    /// workspace) registers afresh; `reauth` signs in an account again with
    /// its own client id. `host_id` is this head's stable
    /// `ext_agent_host_id`; `login_hint` an email to suggest.
    pub fn start(&self, name: &str, ui_port: u16, reauth: Option<Reauth>, host_id: &str, login_hint: Option<&str>) -> String {
        let verifier = random_b64(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_b64(24);
        let nonce = random_b64(24);
        let redirect_uri = format!("http://127.0.0.1:{ui_port}{CALLBACK_PATH}");
        let client_id = reauth.as_ref().map(|r| r.client_id.clone()).unwrap_or_else(|| DYNAMIC_CLIENT.to_owned());
        let mut url = url::Url::parse(AUTHORIZE_URL).expect("valid URL");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("client_id", &client_id);
            // The app's name, on first registration only.
            if client_id == DYNAMIC_CLIENT {
                q.append_pair("agent_name_hint", AGENT_NAME);
            }
            q.append_pair("ext_agent_host_id", host_id);
            if let Some(r) = reauth.as_ref().filter(|r| !r.id_token.is_empty()) {
                q.append_pair("id_token_hint", &r.id_token);
            }
            if let Some(h) = login_hint.map(str::trim).filter(|h| !h.is_empty()) {
                q.append_pair("login_hint", h);
            }
        }
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge);
        let now = c6_core::now_ms();
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| now - p.created_ms < PENDING_TTL_MS);
        pending.insert(state, Pending { verifier, redirect_uri, client_id, nonce, name: name.to_owned(), reauth, created_ms: now });
        url.into()
    }

    /// Completes a sign-in from the callback URL (or just its query string).
    pub async fn complete(&self, http: &reqwest::Client, callback: &str) -> anyhow::Result<Completed> {
        let query = callback.trim().split_once('?').map(|(_, q)| q).unwrap_or(callback.trim());
        let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
        if let Some(err) = params.get("error") {
            let desc = params.get("error_description").map(String::as_str).unwrap_or("");
            return Err(bad_request(format!("sign-in was not completed: {err} {desc}")));
        }
        let (Some(code), Some(state)) = (params.get("code"), params.get("state")) else {
            return Err(bad_request("that address has no code/state; paste the full address from the browser after approving"));
        };
        let pending = self.pending.lock().unwrap().remove(state).ok_or_else(|| bad_request("unknown or expired sign-in; start again"))?;
        // The callback carries the issued client id on first registration.
        let client_id = params.get("client_id").cloned().unwrap_or(pending.client_id);

        let resp: TokenResponse = token_request(
            http,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", &client_id),
                ("code_verifier", &pending.verifier),
                ("redirect_uri", &pending.redirect_uri),
                ("resource", RESOURCE),
            ],
        )
        .await?;
        let scope = resp.scope.clone().unwrap_or_default();
        if !scope.split_whitespace().any(|s| s == PLAN_SCOPE) {
            return Err(bad_request("ChatGPT plan usage was not granted; enable it when approving the sign-in"));
        }
        // Who signed in, from the verified ID token.
        let id_token = resp.id_token.clone().ok_or_else(|| bad_request("auth.openai.com returned no ID token"))?;
        let who = self.verify(http, &id_token, &client_id, Some(&pending.nonce)).await?;
        if let Some(r) = &pending.reauth
            && !r.sub.is_empty()
            && r.sub != who.sub
        {
            return Err(bad_request(format!(
                "that signed in a different ChatGPT account ({}); to add it, use Add a ChatGPT account instead",
                if who.email.is_empty() { &who.sub } else { &who.email }
            )));
        }
        let tokens = ChatGptTokens {
            client_id,
            access_token: resp.access_token,
            refresh_token: resp.refresh_token.context("no refresh token returned")?,
            expires_at_ms: c6_core::now_ms() + resp.expires_in.unwrap_or(3600) * 1000,
            scope,
            sub: who.sub,
            email: who.email,
            workspace: who.workspace,
            id_token,
        };
        Ok(Completed { name: pending.name, tokens, reauth: pending.reauth })
    }

    /// Serializes refreshes of one connection: refresh tokens rotate, so two
    /// concurrent refreshes would invalidate each other.
    pub fn lock_for(&self, connection_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().unwrap().entry(connection_id.to_owned()).or_default().clone()
    }

    pub fn needs_refresh(tokens: &ChatGptTokens) -> bool {
        tokens.expires_at_ms - REFRESH_MARGIN_MS <= c6_core::now_ms()
    }

    pub async fn refresh(&self, http: &reqwest::Client, tokens: ChatGptTokens) -> anyhow::Result<ChatGptTokens> {
        let resp: TokenResponse = token_request(
            http,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &tokens.refresh_token),
                ("client_id", &tokens.client_id),
                ("resource", RESOURCE),
            ],
        )
        .await
        .context("refreshing ChatGPT sign-in")?;
        // A new ID token must still be this account's.
        let mut id_token = tokens.id_token.clone();
        if let Some(t) = resp.id_token.filter(|t| !t.is_empty()) {
            let who = self.verify(http, &t, &tokens.client_id, None).await.context("checking the refreshed sign-in")?;
            if !tokens.sub.is_empty() && who.sub != tokens.sub {
                bail!("the refreshed sign-in is for a different ChatGPT account; sign in again");
            }
            id_token = t;
        }
        Ok(ChatGptTokens {
            client_id: tokens.client_id,
            access_token: resp.access_token,
            refresh_token: resp.refresh_token.unwrap_or(tokens.refresh_token),
            expires_at_ms: c6_core::now_ms() + resp.expires_in.unwrap_or(3600) * 1000,
            scope: resp.scope.unwrap_or(tokens.scope),
            sub: tokens.sub,
            email: tokens.email,
            workspace: tokens.workspace,
            id_token,
        })
    }

    /// OpenAI's signing keys and issuer (fetched at most hourly, and again
    /// for a key not yet seen).
    async fn keys(&self, http: &reqwest::Client, refetch: bool) -> anyhow::Result<Keys> {
        let mut cached = self.keys.lock().await;
        if let Some((at, k)) = cached.as_ref()
            && !refetch
            && c6_core::now_ms() - at < KEYS_TTL_MS
        {
            return Ok(k.clone());
        }
        let d: serde_json::Value =
            http.get(DISCOVERY_URL).send().await?.error_for_status()?.json().await.context("OpenAI's sign-in configuration")?;
        let issuer = d["issuer"].as_str().context("no issuer in OpenAI's sign-in configuration")?.to_owned();
        let jwks_uri = d["jwks_uri"].as_str().context("no jwks_uri in OpenAI's sign-in configuration")?;
        let j: serde_json::Value = http.get(jwks_uri).send().await?.error_for_status()?.json().await.context("OpenAI's signing keys")?;
        let keys = Keys { issuer, jwks: j["keys"].as_array().cloned().unwrap_or_default() };
        *cached = Some((c6_core::now_ms(), keys.clone()));
        Ok(keys)
    }

    /// Checks an ID token (signature, issuer, audience, expiry, and the
    /// nonce on sign-in) and says who it is for.
    pub async fn verify(&self, http: &reqwest::Client, token: &str, client_id: &str, nonce: Option<&str>) -> anyhow::Result<Identity> {
        let (header, claims, signed, sig) = split_jwt(token)?;
        let kid = header["kid"].as_str().unwrap_or("");
        anyhow::ensure!(header["alg"] == "RS256", "the ID token is not RS256-signed");
        let mut keys = self.keys(http, false).await?;
        if !keys.jwks.iter().any(|k| k["kid"] == kid) {
            keys = self.keys(http, true).await?;
        }
        let jwk = keys.jwks.iter().find(|k| k["kid"] == kid).ok_or_else(|| anyhow!("the ID token's key is not one of OpenAI's"))?;
        verify_rs256(jwk, signed.as_bytes(), &sig)?;
        check_claims(&claims, &keys.issuer, client_id, nonce, c6_core::now_ms() / 1000)
    }
}

/// A JWT's header and claims (decoded), the signed part, and the signature.
fn split_jwt(token: &str) -> anyhow::Result<(serde_json::Value, serde_json::Value, String, Vec<u8>)> {
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        bail!("the ID token is not a JWT");
    };
    let decode = |p: &str| -> anyhow::Result<serde_json::Value> { Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p)?)?) };
    Ok((decode(h)?, decode(c)?, format!("{h}.{c}"), URL_SAFE_NO_PAD.decode(s)?))
}

fn verify_rs256(jwk: &serde_json::Value, signed: &[u8], sig: &[u8]) -> anyhow::Result<()> {
    let n = URL_SAFE_NO_PAD.decode(jwk["n"].as_str().context("key without n")?)?;
    let e = URL_SAFE_NO_PAD.decode(jwk["e"].as_str().context("key without e")?)?;
    ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
        .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed, sig)
        .map_err(|_| anyhow!("the ID token's signature does not verify"))
}

/// Issuer, audience, expiry (a minute's leeway) and nonce.
fn check_claims(c: &serde_json::Value, issuer: &str, client_id: &str, nonce: Option<&str>, now_s: i64) -> anyhow::Result<Identity> {
    anyhow::ensure!(c["iss"] == issuer, "the ID token is from {} , not {issuer}", c["iss"]);
    let aud_ok = match &c["aud"] {
        serde_json::Value::String(a) => a == client_id,
        serde_json::Value::Array(a) => a.iter().any(|x| x == client_id),
        _ => false,
    };
    anyhow::ensure!(aud_ok, "the ID token is not for this client");
    anyhow::ensure!(c["exp"].as_i64().is_some_and(|e| e + 60 >= now_s), "the ID token has expired");
    if let Some(n) = nonce {
        anyhow::ensure!(c["nonce"] == n, "the ID token's nonce does not match this sign-in");
    }
    let sub = c["sub"].as_str().filter(|s| !s.is_empty()).context("the ID token names no account")?.to_owned();
    let workspace = c["https://api.openai.com/auth"]["chatgpt_account_id"].as_str().unwrap_or("").to_owned();
    Ok(Identity { sub, email: c["email"].as_str().unwrap_or("").to_owned(), workspace })
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> anyhow::Result<TokenResponse> {
    let resp = http.post(TOKEN_URL).form(form).send().await.context("contacting auth.openai.com")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if status.as_u16() == 400 || status.as_u16() == 401 {
            bail!(anyhow!(crate::UserError::BadRequest(format!(
                "auth.openai.com rejected the request ({status}): {}",
                text.chars().take(300).collect::<String>()
            ))));
        }
        bail!("token endpoint returned {status}: {}", text.chars().take(300).collect::<String>());
    }
    serde_json::from_str(&text).context("parsing token response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_builds_pkce_url() {
        let o = OAuth::default();
        let url = url::Url::parse(&o.start("me", 7700, None, "urn:uuid:5f0c", None)).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "dynamic_agent_client");
        assert_eq!(q["agent_name_hint"], "carbon6");
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:5f0c");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:7700/callback");
        assert_eq!(q["resource"], "https://api.openai.com/v1");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["scope"].contains("chatgpt.tokens.use.direct"));
        let p = o.pending.lock().unwrap();
        let pending = &p[&q["state"]];
        assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes())), q["code_challenge"]);
    }

    #[test]
    fn reauth_has_no_name_hint() {
        let o = OAuth::default();
        let reauth = Reauth { connection_id: "c1".into(), client_id: "oaiapp_x".into(), id_token: "a.b.c".into(), sub: "user-1".into() };
        let url = url::Url::parse(&o.start("me", 7700, Some(reauth), "urn:uuid:5f0c", Some("me@example.com"))).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "oaiapp_x");
        assert!(!q.contains_key("agent_name_hint"));
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:5f0c");
        assert_eq!(q["id_token_hint"], "a.b.c");
        assert_eq!(q["login_hint"], "me@example.com");
    }

    #[test]
    fn id_token_claims() {
        let c = serde_json::json!({"iss": "https://auth.openai.com", "aud": ["oaiapp_x"], "exp": 2000, "nonce": "n1", "sub": "user-1", "email": "a@b.c"});
        let who = check_claims(&c, "https://auth.openai.com", "oaiapp_x", Some("n1"), 1990).unwrap();
        assert_eq!(who, Identity { sub: "user-1".into(), email: "a@b.c".into(), workspace: String::new() });
        let mut w = c.clone();
        w["https://api.openai.com/auth"] = serde_json::json!({"chatgpt_account_id": "ws-9"});
        assert_eq!(check_claims(&w, "https://auth.openai.com", "oaiapp_x", Some("n1"), 1990).unwrap().workspace, "ws-9");
        assert!(check_claims(&c, "https://auth.openai.com", "other", Some("n1"), 1990).is_err(), "audience");
        assert!(check_claims(&c, "https://evil.example", "oaiapp_x", Some("n1"), 1990).is_err(), "issuer");
        assert!(check_claims(&c, "https://auth.openai.com", "oaiapp_x", Some("n2"), 1990).is_err(), "nonce");
        assert!(check_claims(&c, "https://auth.openai.com", "oaiapp_x", Some("n1"), 2100).is_err(), "expired");
        // Refreshes carry no nonce to check.
        assert!(check_claims(&c, "https://auth.openai.com", "oaiapp_x", None, 1990).is_ok());
        assert!(split_jwt("not-a-jwt").is_err());
    }

    #[tokio::test]
    async fn complete_rejects_unknown_state_and_errors() {
        let o = OAuth::default();
        c6_llm::install_crypto_provider();
        let http = reqwest::Client::new();
        let e = o.complete(&http, "http://127.0.0.1:7700/callback?code=x&state=nope").await.err().unwrap();
        assert!(e.to_string().contains("unknown or expired"));
        let e = o.complete(&http, "?error=access_denied&error_description=no").await.err().unwrap();
        assert!(e.to_string().contains("access_denied"));
    }
}
