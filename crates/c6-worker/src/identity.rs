//! The worker's enrolled identity: key, client certificate, pinned CA and the
//! head's address. This is the only state c6w keeps outside caches.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use c6_core::WorkerId;
use serde::{Deserialize, Serialize};
use tonic::transport::{Certificate, ClientTlsConfig, Identity as TlsIdentity};

const META: &str = "worker.toml";
const KEY: &str = "worker.key";
const CERT: &str = "worker.crt";
const CA: &str = "ca.crt";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityMeta {
    pub worker_id: WorkerId,
    pub worker_name: String,
    pub head_url: String,
}

#[derive(Clone)]
pub struct Identity {
    pub meta: IdentityMeta,
    pub key_pem: String,
    pub cert_pem: String,
    pub ca_pem: String,
}

/// `~/.local/state/c6w` on Linux; macOS has no state dir, so the local data dir.
pub fn default_state_dir() -> PathBuf {
    dirs::state_dir().or_else(dirs::data_local_dir).unwrap_or_else(|| PathBuf::from(".")).join("c6w")
}

pub fn identity_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("identity")
}

impl Identity {
    pub fn exists(state_dir: &Path) -> bool {
        identity_dir(state_dir).join(META).exists()
    }

    pub fn load(state_dir: &Path) -> anyhow::Result<Self> {
        let dir = identity_dir(state_dir);
        if !dir.join(META).exists() {
            bail!("not enrolled (no identity in {}); run `c6w enroll <token>`", dir.display());
        }
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).with_context(|| format!("reading {}", dir.join(name).display()));
        let meta: IdentityMeta = toml::from_str(&read(META)?).context("parsing worker.toml")?;
        Ok(Self { meta, key_pem: read(KEY)?, cert_pem: read(CERT)?, ca_pem: read(CA)? })
    }

    pub fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        let dir = identity_dir(state_dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        write_file(&dir.join(KEY), &self.key_pem, 0o600)?;
        write_file(&dir.join(CERT), &self.cert_pem, 0o644)?;
        write_file(&dir.join(CA), &self.ca_pem, 0o644)?;
        // Written last: its presence marks a complete identity.
        write_file(&dir.join(META), &toml::to_string(&self.meta)?, 0o644)?;
        Ok(())
    }

    pub fn tls_config(&self) -> ClientTlsConfig {
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.ca_pem))
            .identity(TlsIdentity::from_pem(&self.cert_pem, &self.key_pem))
    }
}

fn write_file(path: &Path, contents: &str, mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
