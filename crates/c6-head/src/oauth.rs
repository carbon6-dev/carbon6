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
pub const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Required scope for using the ChatGPT plan.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// First-time registration entrypoint; the callback returns the real id.
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const PENDING_TTL_MS: i64 = 15 * 60 * 1000;
/// Refresh this long before expiry.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// What is stored (encrypted) for a `chatgpt` connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatGptTokens {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
    pub scope: String,
}

struct Pending {
    verifier: String,
    redirect_uri: String,
    client_id: String,
    name: String,
    created_ms: i64,
}

#[derive(Default)]
pub struct OAuth {
    pending: Mutex<HashMap<String, Pending>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub struct Completed {
    pub name: String,
    pub tokens: ChatGptTokens,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

fn random_b64(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

impl OAuth {
    /// Starts a sign-in and returns the URL to open. `client_id` is an id
    /// issued by an earlier sign-in, if any.
    pub fn start(&self, name: &str, ui_port: u16, client_id: Option<String>) -> String {
        let verifier = random_b64(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_b64(24);
        let nonce = random_b64(24);
        let redirect_uri = format!("http://127.0.0.1:{ui_port}/auth/callback");
        let client_id = client_id.unwrap_or_else(|| DYNAMIC_CLIENT.to_owned());
        let mut url = url::Url::parse(AUTHORIZE_URL).expect("valid URL");
        url.query_pairs_mut()
            .append_pair("client_id", &client_id)
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
        pending.insert(state, Pending { verifier, redirect_uri, client_id, name: name.to_owned(), created_ms: now });
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
        let tokens = ChatGptTokens {
            client_id,
            access_token: resp.access_token,
            refresh_token: resp.refresh_token.context("no refresh token returned")?,
            expires_at_ms: c6_core::now_ms() + resp.expires_in.unwrap_or(3600) * 1000,
            scope,
        };
        Ok(Completed { name: pending.name, tokens })
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
        Ok(ChatGptTokens {
            client_id: tokens.client_id,
            access_token: resp.access_token,
            refresh_token: resp.refresh_token.unwrap_or(tokens.refresh_token),
            expires_at_ms: c6_core::now_ms() + resp.expires_in.unwrap_or(3600) * 1000,
            scope: resp.scope.unwrap_or(tokens.scope),
        })
    }
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
        let url = url::Url::parse(&o.start("me", 7700, None)).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "dynamic_agent_client");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:7700/auth/callback");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["scope"].contains("chatgpt.tokens.use.direct"));
        let p = o.pending.lock().unwrap();
        let pending = &p[&q["state"]];
        assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes())), q["code_challenge"]);
    }

    #[tokio::test]
    async fn complete_rejects_unknown_state_and_errors() {
        let o = OAuth::default();
        c6_llm::install_crypto_provider();
        let http = reqwest::Client::new();
        let e = o.complete(&http, "http://127.0.0.1:7700/auth/callback?code=x&state=nope").await.err().unwrap();
        assert!(e.to_string().contains("unknown or expired"));
        let e = o.complete(&http, "?error=access_denied&error_description=no").await.err().unwrap();
        assert!(e.to_string().contains("access_denied"));
    }
}
