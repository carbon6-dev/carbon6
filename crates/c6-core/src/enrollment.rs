//! Enrollment tokens: created by the head, pasted into `c6w enroll`.
//!
//! A token carries everything a worker needs to reach the head and trust it
//! on first contact: the worker-port URL, the SHA-256 fingerprint of the
//! head's CA certificate (DER), and a one-time secret.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "c6e1.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentToken {
    #[serde(rename = "u")]
    pub url: String,
    #[serde(rename = "f")]
    pub ca_sha256: String,
    #[serde(rename = "s")]
    pub secret: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("malformed enrollment token")]
    Malformed,
    #[error(
        "this is an API token (for the c6 CLI), not an enrollment token; enrolling needs an enrollment token (c6e1.…) from Settings → Memex (for a memex) or Workers → Add worker"
    )]
    ApiToken,
}

impl EnrollmentToken {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("token serializes");
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(s: &str) -> Result<Self, TokenError> {
        let s = s.trim();
        if s.starts_with("c6_") {
            return Err(TokenError::ApiToken);
        }
        let body = s.strip_prefix(PREFIX).ok_or(TokenError::Malformed)?;
        let json = URL_SAFE_NO_PAD.decode(body).map_err(|_| TokenError::Malformed)?;
        serde_json::from_slice(&json).map_err(|_| TokenError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = EnrollmentToken { url: "https://100.64.0.1:7701".into(), ca_sha256: "ab".repeat(32), secret: "s3cret".into() };
        assert_eq!(EnrollmentToken::decode(&t.encode()).unwrap(), t);
        assert!(EnrollmentToken::decode("nope").is_err());
        assert!(matches!(EnrollmentToken::decode("c6_1b37"), Err(TokenError::ApiToken)));
    }
}
