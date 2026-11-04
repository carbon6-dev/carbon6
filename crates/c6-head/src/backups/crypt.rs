//! Encryption for backups (docs/backups.md §4): each object is compressed
//! and encrypted with its own key, wrapped to your public key, so only the
//! private key you keep can read it. Names are keyed hashes, so the same
//! content is stored once without saying what it is.

use std::io::{Read, Write};

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, XChaCha20Poly1305};
use x25519_dalek::{PublicKey, StaticSecret};

const OBJECT: &[u8; 4] = b"C6B1";
const INDEX: &[u8; 4] = b"C6I1";
const PUBLIC_PREFIX: &str = "c6pub1";
const PRIVATE_PREFIX: &str = "c6key1";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A new key pair, as text: (public, private).
pub fn new_key_pair() -> (String, String) {
    let secret = StaticSecret::from(rand::random::<[u8; 32]>());
    let public = PublicKey::from(&secret);
    (format!("{PUBLIC_PREFIX}{}", B64.encode(public.as_bytes())), format!("{PRIVATE_PREFIX}{}", B64.encode(secret.to_bytes())))
}

/// The public key a private key belongs to.
pub fn public_of(private: &str) -> anyhow::Result<String> {
    let s = private_key(private)?;
    Ok(format!("{PUBLIC_PREFIX}{}", B64.encode(PublicKey::from(&s).as_bytes())))
}

pub fn public_key(text: &str) -> anyhow::Result<PublicKey> {
    let b = text.trim().strip_prefix(PUBLIC_PREFIX).ok_or_else(|| anyhow!("a backup public key starts with {PUBLIC_PREFIX}"))?;
    let bytes: [u8; 32] = B64.decode(b)?.try_into().map_err(|_| anyhow!("a backup public key is 32 bytes"))?;
    Ok(PublicKey::from(bytes))
}

pub fn private_key(text: &str) -> anyhow::Result<StaticSecret> {
    let b = text.trim().strip_prefix(PRIVATE_PREFIX).ok_or_else(|| anyhow!("a backup key starts with {PRIVATE_PREFIX}"))?;
    let bytes: [u8; 32] = B64.decode(b)?.try_into().map_err(|_| anyhow!("a backup key is 32 bytes"))?;
    Ok(StaticSecret::from(bytes))
}

fn hkdf(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    struct Len;
    impl ring::hkdf::KeyType for Len {
        fn len(&self) -> usize {
            32
        }
    }
    let mut out = [0u8; 32];
    let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt).extract(ikm);
    // Expanding to 32 bytes with SHA-256 can't fail.
    prk.expand(&[info], Len).and_then(|okm| okm.fill(&mut out)).expect("hkdf to 32 bytes");
    out
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::with_capacity(data.len() / 2), flate2::Compression::fast());
    // Writing to a Vec can't fail.
    e.write_all(data).expect("compress");
    e.finish().expect("compress")
}

fn inflate(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 2);
    flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).context("decompressing")?;
    Ok(out)
}

/// `plaintext` compressed and encrypted to `to`.
pub fn seal(to: &PublicKey, plaintext: &[u8]) -> Vec<u8> {
    let data_key: [u8; 32] = rand::random();
    let eph = StaticSecret::from(rand::random::<[u8; 32]>());
    let eph_pub = PublicKey::from(&eph);
    let shared = eph.diffie_hellman(to);
    let wrap = hkdf(shared.as_bytes(), &[eph_pub.as_bytes().as_slice(), to.as_bytes()].concat(), b"c6 backup v1 wrap");
    // Each wrap key is used once, so a fixed nonce is safe.
    let wrapped =
        ChaCha20Poly1305::new(&wrap.into()).encrypt(&[0u8; 12].into(), data_key.as_slice()).expect("encrypting 32 bytes can't fail");
    let nonce: [u8; 24] = rand::random();
    let ct = XChaCha20Poly1305::new(&data_key.into()).encrypt(&nonce.into(), deflate(plaintext).as_slice()).expect("encrypting can't fail");
    [OBJECT.as_slice(), eph_pub.as_bytes(), &wrapped, &nonce, &ct].concat()
}

/// What `seal` made, with the private key.
pub fn open(key: &StaticSecret, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
    if sealed.len() < 4 + 32 + 48 + 24 || &sealed[..4] != OBJECT {
        bail!("not a carbon6 backup object");
    }
    let eph_pub: [u8; 32] = sealed[4..36].try_into()?;
    let eph_pub = PublicKey::from(eph_pub);
    let ours = PublicKey::from(key);
    let shared = key.diffie_hellman(&eph_pub);
    let wrap = hkdf(shared.as_bytes(), &[eph_pub.as_bytes().as_slice(), ours.as_bytes()].concat(), b"c6 backup v1 wrap");
    let data_key = ChaCha20Poly1305::new(&wrap.into())
        .decrypt(&[0u8; 12].into(), &sealed[36..84])
        .map_err(|_| anyhow!("this backup was made for another key"))?;
    let data_key: [u8; 32] = data_key.try_into().map_err(|_| anyhow!("bad key"))?;
    let nonce: [u8; 24] = sealed[84..108].try_into()?;
    let pt = XChaCha20Poly1305::new(&data_key.into())
        .decrypt(&nonce.into(), &sealed[108..])
        .map_err(|_| anyhow!("a backup object was changed"))?;
    inflate(&pt)
}

/// The head's own key for backups: it names objects (keyed hashes) and
/// encrypts the indexes it reads back for pruning and checks. It is kept
/// in the head (under the master key) and, wrapped to your public key, at
/// each destination, so a restore has it too.
#[derive(Clone)]
pub struct HeadKey(pub [u8; 32]);

impl Default for HeadKey {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadKey {
    pub fn new() -> Self {
        Self(rand::random())
    }

    /// A keyed name for `what` (a chunk's content, a database's path…).
    pub fn name(&self, kind: &str, what: &[u8]) -> String {
        let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        let mut ctx = ring::hmac::Context::with_key(&k);
        ctx.update(kind.as_bytes());
        ctx.update(b":");
        ctx.update(what);
        hex::encode(&ctx.sign().as_ref()[..20])
    }

    fn index_cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&hkdf(&self.0, b"", b"c6 backup v1 index").into())
    }

    /// An index the head reads back itself.
    pub fn seal_index(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce: [u8; 24] = rand::random();
        let ct = self.index_cipher().encrypt(&nonce.into(), deflate(plaintext).as_slice()).expect("encrypting can't fail");
        [INDEX.as_slice(), &nonce, &ct].concat()
    }

    pub fn open_index(&self, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        if sealed.len() < 28 || &sealed[..4] != INDEX {
            bail!("not a backup index");
        }
        let nonce: [u8; 24] = sealed[4..28].try_into()?;
        let pt = self.index_cipher().decrypt(&nonce.into(), &sealed[28..]).map_err(|_| anyhow!("a backup index was changed"))?;
        inflate(&pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_private_key_opens_what_is_sealed() {
        let (public, private) = new_key_pair();
        assert_eq!(public_of(&private).unwrap(), public);
        let data = b"the head's database, page after page".repeat(100);
        let sealed = seal(&public_key(&public).unwrap(), &data);
        assert!(sealed.len() < data.len(), "compressed");
        assert!(!sealed.windows(8).any(|w| w == b"database"), "no plaintext in it");
        assert_eq!(open(&private_key(&private).unwrap(), &sealed).unwrap(), data);
        let (_, other) = new_key_pair();
        assert!(open(&private_key(&other).unwrap(), &sealed).is_err(), "another key can't");
        let mut changed = sealed.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(open(&private_key(&private).unwrap(), &changed).is_err(), "a change is caught");
    }

    #[test]
    fn names_hide_content_but_match_it() {
        let (a, b) = (HeadKey::new(), HeadKey::new());
        assert_eq!(a.name("chunk", b"x"), a.name("chunk", b"x"));
        assert_ne!(a.name("chunk", b"x"), a.name("chunk", b"y"));
        assert_ne!(a.name("chunk", b"x"), b.name("chunk", b"x"), "another head's names differ");
        assert_ne!(a.name("chunk", b"x"), a.name("blob", b"x"));
        let i = a.seal_index(b"[\"c1\",\"c2\"]");
        assert_eq!(a.open_index(&i).unwrap(), b"[\"c1\",\"c2\"]");
        assert!(b.open_index(&i).is_err());
    }
}
