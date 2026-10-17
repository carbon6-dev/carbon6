//! Encryption at rest for credentials, under `<data>/master.key`.

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use chacha20poly1305::aead::{Aead, Key, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

const NONCE_LEN: usize = 12;

pub struct Secrets {
    cipher: ChaCha20Poly1305,
}

impl Secrets {
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        let key: [u8; 32] = match std::fs::read(path) {
            Ok(bytes) => bytes.try_into().map_err(|_| anyhow!("{} is not a 32-byte key", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key: [u8; 32] = rand::random();
                write_private(path, &key)?;
                key
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { cipher: ChaCha20Poly1305::new(&Key::<ChaCha20Poly1305>::from(key)) })
    }

    /// Returns nonce || ciphertext.
    pub fn encrypt(&self, plaintext: &str) -> anyhow::Result<Vec<u8>> {
        let nonce_bytes: [u8; NONCE_LEN] = rand::random();
        let ct = self.cipher.encrypt(&Nonce::from(nonce_bytes), plaintext.as_bytes()).map_err(|_| anyhow!("encryption failed"))?;
        Ok([nonce_bytes.as_slice(), &ct].concat())
    }

    pub fn decrypt(&self, blob: &[u8]) -> anyhow::Result<String> {
        if blob.len() < NONCE_LEN {
            bail!("ciphertext too short");
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into()?;
        let pt = self.cipher.decrypt(&Nonce::from(nonce), ct).map_err(|_| anyhow!("decryption failed (wrong master.key?)"))?;
        Ok(String::from_utf8(pt)?)
    }
}

fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(contents)?;
    f.sync_all()?;
    Ok(())
}

/// A secret's value, if `realm` has it, or root (root's are every
/// realm's, as its settings are): for a kit job's environment, an app's
/// OAuth client.
pub async fn value_in(st: &crate::HeadState, realm: &str, name: &str) -> anyhow::Result<Option<String>> {
    match st.store.secret(realm, name).await? {
        Some((blob, _, _)) => Ok(Some(st.secrets.decrypt(&blob)?)),
        None => Ok(None),
    }
}

/// Puts a secret in a realm (creating or replacing it).
pub async fn put_in(st: &crate::HeadState, realm: &str, name: &str, value: &str, description: &str) -> anyhow::Result<()> {
    st.store.put_secret(realm, name, Some(st.secrets.encrypt(value)?), description, &[]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_persistent_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        let a = Secrets::load_or_create(&path).unwrap();
        let blob = a.encrypt("sk-test").unwrap();
        let b = Secrets::load_or_create(&path).unwrap();
        assert_eq!(b.decrypt(&blob).unwrap(), "sk-test");
        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(b.decrypt(&tampered).is_err());
    }
}
