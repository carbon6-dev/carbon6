//! Types shared by the head and workers.

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub mod enrollment;
#[cfg(feature = "update")]
pub mod update;
#[cfg(feature = "upgrade")]
pub mod upgrade;

/// Wire protocol version. The head accepts `PROTOCOL_VERSION` and
/// `PROTOCOL_VERSION - 1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Version of this build, shared by `c6h` and `c6w`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// This build's exact version: the release, or for a build between
/// releases `<release>+git<commit time>.<commit>`. The binaries read it at
/// build time (`C6_BUILD_VERSION`, which the Makefile and CI set) and give
/// it here as they start, so a new version recompiles only them; otherwise
/// (tests) it is [`VERSION`]. Updates compare it.
pub fn build() -> &'static str {
    BUILD.get().copied().unwrap_or(VERSION)
}

static BUILD: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Called first thing by a binary: `set_build(option_env!("C6_BUILD_VERSION"))`.
pub fn set_build(v: Option<&'static str>) {
    if let Some(v) = v.filter(|v| !v.is_empty()) {
        let _ = BUILD.set(v);
    }
}

/// Whether a peer speaking `version` can talk to this build.
pub fn protocol_compatible(version: u32) -> bool {
    version == PROTOCOL_VERSION || version + 1 == PROTOCOL_VERSION
}

/// URI scheme used in worker certificate SANs: `c6w://<worker-id>`.
pub const WORKER_URI_SCHEME: &str = "c6w";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerId(pub Uuid);

impl WorkerId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn uri(&self) -> String {
        format!("{WORKER_URI_SCHEME}://{}", self.0)
    }

    pub fn from_uri(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix(WORKER_URI_SCHEME)?.strip_prefix("://")?;
        Uuid::parse_str(rest).ok().map(Self)
    }
}

impl Default for WorkerId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for WorkerId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

/// Current time as Unix milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Lowercase hex SHA-256, used for certificate fingerprints and secret hashes.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Creates `dir` (and its parents) if needed and makes it private to this
/// user (0700 on Unix): state directories hold databases, transcripts and
/// keys.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Host facts reported by a worker.
pub fn host_os() -> &'static str {
    std::env::consts::OS
}

pub fn host_arch() -> &'static str {
    std::env::consts::ARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_uri_round_trip() {
        let id = WorkerId::new();
        assert_eq!(WorkerId::from_uri(&id.uri()), Some(id));
        assert_eq!(WorkerId::from_uri("https://x"), None);
    }

    #[test]
    fn protocol_window() {
        assert!(protocol_compatible(PROTOCOL_VERSION));
        assert!(!protocol_compatible(PROTOCOL_VERSION + 1));
    }

    #[cfg(unix)]
    #[test]
    fn private_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let new = tmp.path().join("a/b");
        create_private_dir(&new).unwrap();
        assert_eq!(mode(&new), 0o700);
        // An existing, looser one is tightened.
        let old = tmp.path().join("old");
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&old).unwrap();
        assert_eq!(mode(&old), 0o700);
    }
}
