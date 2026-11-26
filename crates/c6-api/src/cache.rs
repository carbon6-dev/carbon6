//! Observed prompt-cache lifetime, keyed by the account and model actually used.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CacheLifetime {
    pub lifetime_seconds: Option<u64>,
    /// Enough comparable calls bracketed an expiry, or demonstrated a longer TTL.
    pub learned: bool,
    pub cache_observed: bool,
    /// Cache-read price / full-prefix rewrite price; absent without known prices.
    pub read_to_rewrite_ratio: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelCacheLifetime {
    pub connection_id: String,
    pub model: String,
    #[serde(flatten)]
    pub estimate: CacheLifetime,
}
