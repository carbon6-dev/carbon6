//! Shared HTTP plumbing: a streaming-friendly client and POST with retries.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use tracing::warn;

use crate::LlmError;

/// Attempts at a request before giving up (`post_json`).
pub const MAX_ATTEMPTS: u32 = 5;

pub fn client() -> Result<reqwest::Client, LlmError> {
    crate::install_crypto_provider();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // No overall timeout: streams can legitimately run for many minutes.
        .read_timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| LlmError::Config(e.to_string()))
}

/// POSTs JSON, retrying connection errors, 408/409/429 and 5xx before any
/// response body is consumed, up to `attempts` times. Honours `retry-after`.
pub async fn post_json(
    http: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    body: &Value,
    attempts: u32,
) -> Result<reqwest::Response, LlmError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match post_once(http, url, headers, body).await {
            Ok(r) => return Ok(r),
            Err(e) if e.retryable() && attempt < attempts => {
                let wait = match &e {
                    LlmError::Http { retry_after: Some(d), .. } => *d,
                    _ => Duration::from_millis(1000 * 2u64.pow(attempt - 1)).min(Duration::from_secs(30)),
                };
                warn!(error = %e, attempt, ?wait, "model request failed; retrying");
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn post_once(
    http: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    body: &Value,
) -> Result<reqwest::Response, LlmError> {
    let mut req = http.post(url).json(body);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| LlmError::Network(e.to_string()))?;
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| Duration::from_secs_f64(s.clamp(0.0, 120.0)));
    let text = resp.text().await.unwrap_or_default();
    if (status == 400 || status == 413) && is_context_overflow(&text) {
        return Err(LlmError::ContextOverflow(truncate(&text, 500)));
    }
    Err(LlmError::Http { status, body: truncate(&text, 2000), retry_after })
}

/// Recognizes "too long" errors across providers.
pub fn is_context_overflow(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    ["context_length_exceeded", "prompt is too long", "maximum context length", "context window", "too many tokens", "exceeds the context"]
        .iter()
        .any(|p| b.contains(p))
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
