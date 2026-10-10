//! Server-side WebAuthn (passkeys) without attestation.
//!
//! Registration takes the public key the browser reports
//! (`AuthenticatorAttestationResponse.getPublicKey()`, SubjectPublicKeyInfo
//! DER), so no CBOR is needed: attestation is not requested, and only a
//! signed-in user can register a key. Sign-in verifies the assertion
//! signature over `authenticatorData || SHA-256(clientDataJSON)` with that
//! key. Algorithms: ES256 (-7), EdDSA (-8), RS256 (-257).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::signature;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const ES256: i64 = -7;
pub const EDDSA: i64 = -8;
pub const RS256: i64 = -257;

/// What a ceremony must match: the challenge we issued, the page origin
/// (`https://host:port`) and the relying party id (the host name).
pub struct Expect<'a> {
    pub challenge: &'a str,
    pub origin: &'a str,
    pub rp_id: &'a str,
}

/// A new passkey, as sent by the UI after `navigator.credentials.create`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub raw_id: String,
    pub client_data_json: String,
    pub authenticator_data: String,
    pub public_key: String,
    pub public_key_algorithm: i64,
}

/// A sign-in, as sent by the UI after `navigator.credentials.get`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assertion {
    pub raw_id: String,
    pub client_data_json: String,
    pub authenticator_data: String,
    pub signature: String,
}

/// A verified registration.
#[derive(Debug)]
pub struct NewCredential {
    pub id: String,
    pub public_key: Vec<u8>,
    pub alg: i64,
    pub sign_count: u32,
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
}

const UP: u8 = 0x01; // user present
const UV: u8 = 0x04; // user verified
const AT: u8 = 0x40; // attested credential data included

fn b64(s: &str, what: &str) -> anyhow::Result<Vec<u8>> {
    B64.decode(s.trim_end_matches('=')).map_err(|_| anyhow::anyhow!("{what} is not base64url"))
}

/// Checks clientDataJSON; returns its SHA-256.
fn check_client_data(raw: &[u8], kind: &str, expect: &Expect) -> anyhow::Result<[u8; 32]> {
    let cd: ClientData = serde_json::from_slice(raw).map_err(|_| anyhow::anyhow!("malformed clientDataJSON"))?;
    anyhow::ensure!(cd.kind == kind, "wrong ceremony type {}", cd.kind);
    anyhow::ensure!(cd.challenge.trim_end_matches('=') == expect.challenge, "challenge mismatch");
    anyhow::ensure!(cd.origin == expect.origin, "origin {} is not {}", cd.origin, expect.origin);
    Ok(Sha256::digest(raw).into())
}

/// Checks the RP id hash and flags; returns (flags, sign count).
fn check_auth_data(ad: &[u8], expect: &Expect) -> anyhow::Result<(u8, u32)> {
    anyhow::ensure!(ad.len() >= 37, "authenticator data too short");
    let rp_hash: [u8; 32] = Sha256::digest(expect.rp_id.as_bytes()).into();
    anyhow::ensure!(ad[..32] == rp_hash, "passkey is for another site");
    let flags = ad[32];
    anyhow::ensure!(flags & UP != 0, "user presence not confirmed");
    anyhow::ensure!(flags & UV != 0, "user verification (biometric or PIN) required");
    Ok((flags, u32::from_be_bytes(ad[33..37].try_into().unwrap())))
}

pub fn verify_registration(r: &Registration, expect: &Expect) -> anyhow::Result<NewCredential> {
    check_client_data(&b64(&r.client_data_json, "clientDataJSON")?, "webauthn.create", expect)?;
    let ad = b64(&r.authenticator_data, "authenticatorData")?;
    let (flags, sign_count) = check_auth_data(&ad, expect)?;
    anyhow::ensure!(flags & AT != 0, "no credential in authenticator data");
    // aaguid (16) | credential id length (2) | credential id | public key
    anyhow::ensure!(ad.len() >= 55, "authenticator data too short");
    let len = u16::from_be_bytes([ad[53], ad[54]]) as usize;
    let cred_id = ad.get(55..55 + len).ok_or_else(|| anyhow::anyhow!("truncated credential id"))?;
    let raw_id = b64(&r.raw_id, "rawId")?;
    anyhow::ensure!(cred_id == raw_id.as_slice(), "credential id mismatch");
    anyhow::ensure!([ES256, EDDSA, RS256].contains(&r.public_key_algorithm), "unsupported algorithm {}", r.public_key_algorithm);
    let public_key = b64(&r.public_key, "publicKey")?;
    spki_key(&public_key)?; // well-formed
    Ok(NewCredential { id: B64.encode(&raw_id), public_key, alg: r.public_key_algorithm, sign_count })
}

/// Verifies a sign-in against the stored key; returns the new sign count.
pub fn verify_assertion(a: &Assertion, expect: &Expect, public_key: &[u8], alg: i64, stored_count: u32) -> anyhow::Result<u32> {
    let cd_hash = check_client_data(&b64(&a.client_data_json, "clientDataJSON")?, "webauthn.get", expect)?;
    let ad = b64(&a.authenticator_data, "authenticatorData")?;
    let (_, count) = check_auth_data(&ad, expect)?;
    // Synced passkeys report 0; a counter that goes backwards means a clone.
    anyhow::ensure!(count == 0 || count > stored_count, "signature counter went backwards");
    let mut signed = ad;
    signed.extend_from_slice(&cd_hash);
    let sig = b64(&a.signature, "signature")?;
    let key = spki_key(public_key)?;
    let algorithm: &dyn signature::VerificationAlgorithm = match alg {
        ES256 => &signature::ECDSA_P256_SHA256_ASN1,
        EDDSA => &signature::ED25519,
        RS256 => &signature::RSA_PKCS1_2048_8192_SHA256,
        other => anyhow::bail!("unsupported algorithm {other}"),
    };
    signature::UnparsedPublicKey::new(algorithm, key).verify(&signed, &sig).map_err(|_| anyhow::anyhow!("bad signature"))?;
    Ok(count)
}

/// The key bits of a SubjectPublicKeyInfo: the EC point, the Ed25519 key or
/// the PKCS#1 RSA key, which is what `ring` takes for each algorithm.
fn spki_key(der: &[u8]) -> anyhow::Result<&[u8]> {
    let (tag, spki, _) = tlv(der)?;
    anyhow::ensure!(tag == 0x30, "public key is not a SEQUENCE");
    let (tag, _, rest) = tlv(spki)?; // AlgorithmIdentifier
    anyhow::ensure!(tag == 0x30, "malformed AlgorithmIdentifier");
    let (tag, bits, _) = tlv(rest)?;
    anyhow::ensure!(tag == 0x03 && bits.first() == Some(&0), "malformed public key BIT STRING");
    Ok(&bits[1..])
}

/// One DER TLV: (tag, contents, rest).
fn tlv(d: &[u8]) -> anyhow::Result<(u8, &[u8], &[u8])> {
    anyhow::ensure!(d.len() >= 2, "truncated DER");
    let (len, head) = match d[1] {
        n if n < 0x80 => (n as usize, 2),
        n @ 0x81..=0x82 => {
            let k = (n & 0x7f) as usize;
            anyhow::ensure!(d.len() >= 2 + k, "truncated DER");
            (d[2..2 + k].iter().fold(0usize, |acc, b| acc << 8 | *b as usize), 2 + k)
        }
        _ => anyhow::bail!("unsupported DER length"),
    };
    anyhow::ensure!(d.len() >= head + len, "truncated DER");
    Ok((d[0], &d[head..head + len], &d[head + len..]))
}

pub fn encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    use super::*;

    /// A software passkey (ES256) for tests.
    pub struct SoftKey {
        pair: EcdsaKeyPair,
        pub id: Vec<u8>,
        rng: SystemRandom,
    }

    /// SubjectPublicKeyInfo prefix for a P-256 key (uncompressed point follows).
    const P256_SPKI: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01,
        0x07, 0x03, 0x42, 0x00,
    ];

    impl SoftKey {
        pub fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
            Self { pair, id: (0..16).collect(), rng }
        }

        fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
            serde_json::to_vec(&serde_json::json!({"type": kind, "challenge": challenge, "origin": origin})).unwrap()
        }

        fn auth_data(rp_id: &str, flags: u8, count: u32) -> Vec<u8> {
            let mut ad = Sha256::digest(rp_id.as_bytes()).to_vec();
            ad.push(flags);
            ad.extend_from_slice(&count.to_be_bytes());
            ad
        }

        /// The JSON the UI sends after `navigator.credentials.create`.
        pub fn register(&self, challenge: &str, origin: &str, rp_id: &str) -> serde_json::Value {
            let mut ad = Self::auth_data(rp_id, UP | UV | AT, 0);
            ad.extend_from_slice(&[0; 16]);
            ad.extend_from_slice(&(self.id.len() as u16).to_be_bytes());
            ad.extend_from_slice(&self.id);
            let mut spki = P256_SPKI.to_vec();
            spki.extend_from_slice(self.pair.public_key().as_ref());
            serde_json::json!({
                "id": encode(&self.id), "rawId": encode(&self.id),
                "clientDataJson": encode(&Self::client_data("webauthn.create", challenge, origin)),
                "authenticatorData": encode(&ad), "publicKey": encode(&spki), "publicKeyAlgorithm": ES256,
            })
        }

        /// The JSON the UI sends after `navigator.credentials.get`.
        pub fn sign_in(&self, challenge: &str, origin: &str, rp_id: &str, count: u32) -> serde_json::Value {
            let ad = Self::auth_data(rp_id, UP | UV, count);
            let cd = Self::client_data("webauthn.get", challenge, origin);
            let mut signed = ad.clone();
            signed.extend_from_slice(&Sha256::digest(&cd));
            let sig = self.pair.sign(&self.rng, &signed).unwrap();
            serde_json::json!({
                "id": encode(&self.id), "rawId": encode(&self.id),
                "clientDataJson": encode(&cd), "authenticatorData": encode(&ad), "signature": encode(sig.as_ref()),
            })
        }
    }

    const ORIGIN: &str = "https://head.example.ts.net:7700";
    const RP: &str = "head.example.ts.net";

    fn expect(challenge: &str) -> Expect<'_> {
        Expect { challenge, origin: ORIGIN, rp_id: RP }
    }

    #[test]
    fn register_and_sign_in() {
        let key = SoftKey::new();
        let reg: Registration = serde_json::from_value(key.register("c1", ORIGIN, RP)).unwrap();
        let cred = verify_registration(&reg, &expect("c1")).unwrap();
        assert_eq!(cred.id, encode(&key.id));

        let a: Assertion = serde_json::from_value(key.sign_in("c2", ORIGIN, RP, 5)).unwrap();
        assert_eq!(verify_assertion(&a, &expect("c2"), &cred.public_key, cred.alg, 4).unwrap(), 5);
        // Replays and stale counters fail.
        assert!(verify_assertion(&a, &expect("c3"), &cred.public_key, cred.alg, 4).is_err(), "other challenge");
        assert!(verify_assertion(&a, &expect("c2"), &cred.public_key, cred.alg, 5).is_err(), "counter not increasing");
    }

    #[test]
    fn rejects_other_sites_and_keys() {
        let key = SoftKey::new();
        let cred = verify_registration(&serde_json::from_value(key.register("c", ORIGIN, RP)).unwrap(), &expect("c")).unwrap();
        let wrong_origin: Assertion = serde_json::from_value(key.sign_in("c", "https://evil.example", RP, 0)).unwrap();
        assert!(verify_assertion(&wrong_origin, &expect("c"), &cred.public_key, cred.alg, 0).is_err());
        let wrong_rp: Assertion = serde_json::from_value(key.sign_in("c", ORIGIN, "evil.example", 0)).unwrap();
        assert!(verify_assertion(&wrong_rp, &expect("c"), &cred.public_key, cred.alg, 0).is_err());
        let other = SoftKey::new();
        let forged: Assertion = serde_json::from_value(other.sign_in("c", ORIGIN, RP, 0)).unwrap();
        assert!(verify_assertion(&forged, &expect("c"), &cred.public_key, cred.alg, 0).is_err(), "signed by another key");
    }
}
