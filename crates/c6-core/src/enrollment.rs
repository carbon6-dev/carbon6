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
#[error("malformed enrollment token (expected the whole c6e1.… token from the head; create a new one if it was cut short)")]
pub struct TokenError;

impl EnrollmentToken {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("token serializes");
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(s: &str) -> Result<Self, TokenError> {
        // The token wherever it is in what was pasted (with a command
        // around it, or broken across lines).
        let joined: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let start = joined.find(PREFIX).ok_or(TokenError)?;
        let body: String = joined[start + PREFIX.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let json = URL_SAFE_NO_PAD.decode(&body).map_err(|_| TokenError)?;
        serde_json::from_slice(&json).map_err(|_| TokenError)
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
        assert!(EnrollmentToken::decode("c6e1.…").is_err());
        // Pasted with the command around it.
        assert_eq!(EnrollmentToken::decode(&format!("c6m enroll {}\nc6m run", t.encode())).unwrap(), t);
    }
}
