//! Releases carbon6 updates itself to (docs/updates.md): the manifest each
//! GitHub release carries (`release.json`), its signature, and comparing
//! versions.
//!
//! A release is trusted only if `release.json.sig` is an Ed25519 signature
//! of `release.json` by one of [`RELEASE_KEYS`]; every file installed must
//! then have the checksum the manifest gives it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The public halves of carbon6's release keys (hex, raw Ed25519). More
/// than one so a key can be rotated: ship the next one a release early.
pub const RELEASE_KEYS: &[&str] = &["be179101f8177b670fbafb6a7589e035c7e1b253dd7cbb81aa48cdcae3a55b1e"];

/// `release.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub protocol_version: u32,
    /// The oldest version this one updates from on its own.
    #[serde(default)]
    pub upgrade_from: String,
    /// Steps a person must take; a release with any is never applied on its own.
    #[serde(default)]
    pub manual: Option<String>,
    #[serde(default)]
    pub notes: String,
    /// File name → `sha256:<hex>`.
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the release's signature is not by a carbon6 release key")]
    BadSignature,
    #[error("the release manifest doesn't parse: {0}")]
    BadManifest(String),
}

/// Checks `sig` over `manifest` against `keys` (hex), and parses it.
pub fn verify(manifest: &[u8], sig: &[u8], keys: &[impl AsRef<str>]) -> Result<Manifest, Error> {
    let ok = keys
        .iter()
        .filter_map(|k| hex::decode(k.as_ref().trim()).ok())
        .any(|k| ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, k).verify(manifest, sig).is_ok());
    if !ok {
        return Err(Error::BadSignature);
    }
    let m: Manifest = serde_json::from_slice(manifest).map_err(|e| Error::BadManifest(e.to_string()))?;
    if parse(&m.version).is_none() {
        return Err(Error::BadManifest(format!("{:?} is not a version", m.version)));
    }
    Ok(m)
}

/// `sha256:<hex>` of `bytes`, as the manifest writes checksums.
pub fn checksum(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// A version's release part: `0.3.4`, `v0.3.4`, and builds between
/// releases (`0.3.4+git…`, brew's `0.3.4.<time>`) are all 0.3.4.
pub fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split(['+', '-']).next()?;
    let mut it = v.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// Whether `to` is a newer release than `from`.
pub fn newer(to: &str, from: &str) -> bool {
    matches!((parse(to), parse(from)), (Some(t), Some(f)) if t > f)
}

/// How big a step from `from` to `to` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Patch,
    Minor,
    Major,
}

pub fn step(from: &str, to: &str) -> Option<Step> {
    let (f, t) = (parse(from)?, parse(to)?);
    Some(if t.0 != f.0 {
        Step::Major
    } else if t.1 != f.1 {
        Step::Minor
    } else {
        Step::Patch
    })
}

/// Whether `from` may update to `m` without a person: no manual steps,
/// and not older than the oldest version it updates from.
pub fn unattended(from: &str, m: &Manifest) -> bool {
    m.manual.as_deref().is_none_or(|s| s.trim().is_empty()) && (m.upgrade_from.is_empty() || !newer(&m.upgrade_from, from))
}

/// The file of release `m` that installs `bin` (c6h, c6w, c6) by `method`
/// on this `os` and `arch` (`std::env::consts`), and its checksum.
pub fn asset(m: &Manifest, bin: &str, method: &str, os: &str, arch: &str) -> Option<(String, String)> {
    let v = &m.version;
    let name = match (method, os) {
        ("deb", "linux") => format!("{bin}_{v}_{}.deb", if arch == "aarch64" { "arm64" } else { "amd64" }),
        ("tarball", "linux") => format!("carbon6-{v}-linux-{}.tar.gz", if arch == "aarch64" { "arm64" } else { "amd64" }),
        ("tarball" | "brew", "macos") => format!("{bin}-{v}-darwin-{}.tar.gz", if arch == "aarch64" { "arm64" } else { "x86_64" }),
        _ => return None,
    };
    m.assets.get(&name).map(|sum| (name, sum.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn key() -> (Ed25519KeyPair, String) {
        let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let k = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        let public = hex::encode(k.public_key().as_ref());
        (k, public)
    }

    #[test]
    fn only_signed_manifests_verify() {
        let (k, public) = key();
        let body = br#"{"version":"0.3.4","assets":{"c6h_0.3.4_amd64.deb":"sha256:ab"}}"#;
        let sig = k.sign(body);
        let m = verify(body, sig.as_ref(), &[&public]).unwrap();
        assert_eq!(m.version, "0.3.4");
        // Another key, a changed byte, or no keys at all: refused.
        let (_, other) = key();
        assert_eq!(verify(body, sig.as_ref(), &[&other]), Err(Error::BadSignature));
        let mut changed = body.to_vec();
        changed[14] = b'5';
        assert_eq!(verify(&changed, sig.as_ref(), &[&public]), Err(Error::BadSignature));
        assert_eq!(verify(body, sig.as_ref(), &[] as &[&str]), Err(Error::BadSignature));
        // Rotation: any listed key will do.
        assert!(verify(body, sig.as_ref(), &[&other, &public]).is_ok());
    }

    /// The release key signs with `openssl pkeyutl -sign -rawin` in the
    /// publish job (packaging/release/manifest.sh); ring verifies that.
    #[test]
    fn openssl_signatures_by_the_release_key_verify() {
        let sig = hex::decode("562e5b2510bf4cb3a967352e3e0c8c30cae846772519c3a6dffbd4612d50660bdeba0bf3c877f43d6a7d6cb20aec89b7dcc2f66a87fb4734c8437fabdeb6c705").unwrap();
        assert_eq!(verify(br#"{"version":"0.0.1"}"#, &sig, RELEASE_KEYS).unwrap().version, "0.0.1");
    }

    #[test]
    fn versions() {
        assert!(newer("0.3.4", "0.3.3") && newer("0.4.0", "0.3.9") && newer("v1.0.0", "0.9.9"));
        assert!(!newer("0.3.3", "0.3.3") && !newer("0.3.3", "0.3.3+git1700000000.abc") && !newer("0.3.3", "0.3.3.1700000000"));
        assert!(newer("0.3.4", "0.3.3.1700000000"));
        assert_eq!(step("0.3.3", "0.3.4"), Some(Step::Patch));
        assert_eq!(step("0.3.3", "0.4.0"), Some(Step::Minor));
        assert_eq!(step("0.3.3", "1.0.0"), Some(Step::Major));
        let mut m = Manifest { version: "0.3.4".into(), upgrade_from: "0.3.0".into(), ..Default::default() };
        assert!(unattended("0.3.1", &m) && unattended("0.3.0", &m) && !unattended("0.2.2", &m));
        m.manual = Some("Stop c6m first.".into());
        assert!(!unattended("0.3.3", &m));
    }

    #[test]
    fn assets_by_install() {
        let m = Manifest {
            version: "0.3.4".into(),
            assets: [
                ("c6w_0.3.4_arm64.deb", "sha256:1"),
                ("carbon6-0.3.4-linux-amd64.tar.gz", "sha256:2"),
                ("c6h-0.3.4-darwin-arm64.tar.gz", "sha256:3"),
            ]
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect(),
            ..Default::default()
        };
        assert_eq!(asset(&m, "c6w", "deb", "linux", "aarch64").unwrap().1, "sha256:1");
        assert_eq!(asset(&m, "c6h", "tarball", "linux", "x86_64").unwrap().1, "sha256:2");
        assert_eq!(asset(&m, "c6h", "brew", "macos", "aarch64").unwrap().1, "sha256:3");
        assert_eq!(asset(&m, "c6h", "source", "macos", "aarch64"), None);
    }
}
