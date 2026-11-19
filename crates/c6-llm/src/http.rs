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
    post_json_inner(http, url, headers, body, attempts).await.map_err(classify_payload_too_large)
}

async fn post_json_inner(
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

/// On a synchronous native PDF rejection or HTTP 413, submit the same
/// conversation once without native documents, if any were sent.
pub async fn post_with_document_fallback(
    http: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    mut body: Value,
    attempts: u32,
    (items_key, document_type): (&str, &str),
) -> Result<reqwest::Response, LlmError> {
    match post_json_inner(http, url, headers, &body, attempts).await {
        Err(LlmError::Http { status, body: ref why, .. })
            if (status == 413 || (matches!(status, 400 | 422) && native_document_rejection(why)))
                && strip_native_documents(&mut body, items_key, document_type) =>
        {
            post_json(http, url, headers, &body, 1).await
        }
        result => result.map_err(classify_payload_too_large),
    }
}

fn strip_native_documents(body: &mut Value, items_key: &str, document_type: &str) -> bool {
    let Some(items) = body.get_mut(items_key).and_then(Value::as_array_mut) else { return false };
    let mut removed = false;
    items.retain_mut(|item| {
        if item.get("role").and_then(Value::as_str) != Some("user") {
            return true;
        }
        let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) else { return true };
        let before = parts.len();
        parts.retain(|part| part.get("type").and_then(Value::as_str) != Some(document_type));
        if parts.len() != before {
            removed = true;
            return !parts.is_empty();
        }
        true
    });
    removed
}

fn native_document_rejection(why: &str) -> bool {
    fn identifies_document(s: &str) -> bool {
        s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|w| matches!(w, "pdf" | "pdfs" | "document" | "documents" | "file" | "files" | "input_file" | "file_data"))
    }
    fn rejected(message: &str, param: &str) -> bool {
        let message = message.to_ascii_lowercase();
        (identifies_document(&message) || identifies_document(&param.to_ascii_lowercase()))
            && [
                "unsupported",
                "not supported",
                "does not support",
                "invalid",
                "not a valid",
                "corrupt",
                "malformed",
                "encrypted",
                "password",
                "failed to parse",
                "could not parse",
                "failed to process",
                "could not process",
                "unable to process",
                "unable to read",
            ]
            .iter()
            .any(|&reason| message.contains(reason))
    }

    // Only inspect the diagnostic fields, not echoed request data (which may
    // itself contain the words "unsupported document", for example).
    let Ok(json) = serde_json::from_str::<Value>(why) else {
        // HTTP error bodies can be truncated. Don't search partial JSON that
        // could contain echoed native blocks unrelated to the failure.
        return !why.trim_start().starts_with(['{', '[']) && rejected(why, "");
    };
    let diagnostic = |error: &Value| {
        let message = error.get("message").or_else(|| error.get("msg")).and_then(Value::as_str).unwrap_or_default();
        let param = error.get("param").and_then(Value::as_str).unwrap_or_default();
        let code = error.get("code").and_then(Value::as_str).unwrap_or_default();
        rejected(message, param)
            || matches!(code, "invalid_file" | "unsupported_file" | "invalid_pdf" | "encrypted_pdf" | "invalid_file_data")
    };
    diagnostic(&json)
        || json.get("error").is_some_and(|error| diagnostic(error) || error.as_str().is_some_and(|msg| rejected(msg, "")))
        || json.get("detail").is_some_and(|detail| {
            detail.as_str().is_some_and(|msg| rejected(msg, "")) || detail.as_array().is_some_and(|entries| entries.iter().any(diagnostic))
        })
        || json.as_str().is_some_and(|msg| rejected(msg, ""))
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
    if status == 400 && is_context_overflow(&text) {
        return Err(LlmError::ContextOverflow(truncate(&text, 500)));
    }
    Err(LlmError::Http { status, body: truncate(&text, 2000), retry_after })
}

// Preserve the raw 413 until the native document fallback has had a chance
// to handle it, then classify it as before for the caller.
fn classify_payload_too_large(error: LlmError) -> LlmError {
    match error {
        LlmError::Http { status: 413, body, .. } if is_context_overflow(&body) => LlmError::ContextOverflow(truncate(&body, 500)),
        error => error,
    }
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

#[cfg(test)]
mod tests {
    use c6_testkit::{MockLlm, Reply, within};
    use serde_json::json;

    use super::*;

    #[test]
    fn pdf_rejection_checks_diagnostics_not_echoed_input() {
        for why in [
            r#"{"error":{"message":"The PDF is encrypted"}}"#,
            r#"{"error":{"message":"Invalid value: 'input_file'"}}"#,
            r#"{"error":{"message":"Invalid base64","param":"input[0].content[1].file_data"}}"#,
            r#"{"error":{"code":"invalid_pdf"}}"#,
            "This model does not support document blocks",
        ] {
            assert!(native_document_rejection(why), "{why}");
        }
        for why in [
            r#"{"error":{"message":"Invalid model","input":{"type":"input_file"}}}"#,
            r#"{"error":{"message":"Invalid model","input":{"type":"input_file""#,
            "Unsupported tool: file_search",
            "Unsupported profile",
            "Invalid request",
        ] {
            assert!(!native_document_rejection(why), "{why}");
        }
    }

    #[tokio::test]
    async fn native_pdf_rejection_reposts_once_without_document_parts() {
        for (status, why, items_key, document_type) in [
            (400, "Unable to read corrupt PDF", "messages", "document"),
            (422, "Unable to read corrupt PDF", "input", "input_file"),
            (413, "", "messages", "document"),
            (413, "maximum context length exceeded", "input", "input_file"),
        ] {
            let mock = MockLlm::start().await;
            mock.push(Reply::Error(status, why.into()));
            // A retryable second status must still return without a third POST.
            mock.push(Reply::Status(500));
            let url = format!("{}/chat/completions", mock.base_url());
            let text = json!({"type": "text", "text": "Extracted text and worker-local path"});
            let image = json!({"type": "image", "source": "image-data"});
            let doc = json!({"type": document_type, "data": "pdf-data"});
            let tool_output = json!({"type": "function_call_output", "call_id": "call-1", "output": "ok"});
            let body = json!({"model": "mock-1", "stream": true, (items_key): [
                {"role": "user", "content": [text, doc, image]},
                tool_output,
                {"role": "user", "content": [doc]},
            ]});
            let expected = json!({"model": "mock-1", "stream": true, (items_key): [
                {"role": "user", "content": [text, image]},
                tool_output,
            ]});
            let result = within(
                "the single text fallback request",
                post_with_document_fallback(&client().unwrap(), &url, &HashMap::new(), body.clone(), 5, (items_key, document_type)),
            )
            .await;
            assert!(matches!(result, Err(LlmError::Http { status: 500, .. })));
            assert_eq!(mock.requests(), vec![body, expected]);
        }
    }

    #[tokio::test]
    async fn pdf_fallback_ignores_unrelated_errors_and_requests_without_documents() {
        for (status, why, part) in [
            (400, "unsupported model", json!({"type": "document"})),
            (400, "invalid PDF", json!({"type": "text", "text": "text only"})),
            (403, "invalid PDF", json!({"type": "document"})),
            (413, "request too large", json!({"type": "text", "text": "text only"})),
            (413, "maximum context length exceeded", json!({"type": "text", "text": "text only"})),
        ] {
            let mock = MockLlm::start().await;
            mock.push(Reply::Error(status, why.into()));
            let url = format!("{}/chat/completions", mock.base_url());
            let body = json!({"messages": [{"role": "user", "content": [part]}]});
            let result = within(
                "the original error without a fallback",
                post_with_document_fallback(&client().unwrap(), &url, &HashMap::new(), body, 1, ("messages", "document")),
            )
            .await;
            if status == 413 && is_context_overflow(why) {
                assert!(matches!(result, Err(LlmError::ContextOverflow(body)) if body == why));
            } else {
                assert!(matches!(result, Err(LlmError::Http { status: s, body, .. }) if s == status && body == why));
            }
            assert_eq!(mock.requests().len(), 1);
        }
    }
}
