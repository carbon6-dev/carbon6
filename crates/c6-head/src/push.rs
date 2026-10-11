//! Web push: notifications from the head straight to your browsers and
//! phones (as a home-screen app), without a third-party channel. Each
//! device subscribes through the UI's service worker; the head encrypts
//! messages for it (RFC 8291, aes128gcm) and signs its requests to the
//! browser's push service with its own key (VAPID, RFC 8292).

use base64::Engine;
use c6_api as api;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, agreement, hkdf, signature};
use sqlx::Row;
use tracing::info;

use crate::{HeadState, bad_request};

const VAPID_KEY: &str = "vapid_key";
const RECORD_SIZE: u32 = 4096;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Decodes base64url with or without padding (browsers send either).
fn unb64(s: &str) -> anyhow::Result<Vec<u8>> {
    Ok(b64().decode(s.trim().trim_end_matches('='))?)
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    prk.expand(&[info], Len(len)).expect("HKDF length").fill(&mut out).expect("HKDF fill");
    out
}

/// The encrypted body, given the shared secret and the server's public key
/// (the deterministic part of RFC 8291, so it can be checked against the
/// RFC's example).
fn seal(ecdh_secret: &[u8], auth: &[u8], ua_public: &[u8], as_public: &[u8], salt: &[u8; 16], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh_secret);
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let ikm = expand(&prk_key, &key_info, 32);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let cek = expand(&prk, b"Content-Encoding: aes128gcm\0", 16);
    let nonce: [u8; 12] = expand(&prk, b"Content-Encoding: nonce\0", 12).try_into().expect("12 bytes");
    // One record: the plaintext and the last-record delimiter.
    let mut data = plaintext.to_vec();
    data.push(2);
    anyhow::ensure!(data.len() + 16 + 86 <= RECORD_SIZE as usize, "a push message is at most about 4 KB");
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow::anyhow!("bad key"))?);
    key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut data)
        .map_err(|_| anyhow::anyhow!("encrypting a push message"))?;
    let mut out = salt.to_vec();
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&data);
    Ok(out)
}

/// Encrypts `plaintext` for a subscription (its p256dh key and auth secret).
fn encrypt(p256dh: &str, auth: &str, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let rng = SystemRandom::new();
    let ua_public = unb64(p256dh)?;
    let auth = unb64(auth)?;
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).map_err(|_| anyhow::anyhow!("key generation"))?;
    let as_public = private.compute_public_key().map_err(|_| anyhow::anyhow!("public key"))?.as_ref().to_vec();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| anyhow::anyhow!("random"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &ua_public);
    let secret =
        agreement::agree_ephemeral(private, &peer, |s| s.to_vec()).map_err(|_| anyhow::anyhow!("the device's key is not valid"))?;
    seal(&secret, &auth, &ua_public, &as_public, &salt, plaintext)
}

/// The head's VAPID key pair (made on first use).
async fn vapid(st: &HeadState) -> anyhow::Result<signature::EcdsaKeyPair> {
    let rng = SystemRandom::new();
    let pkcs8 = match st.store.setting(VAPID_KEY).await? {
        Some(k) if !k.is_empty() => hex::decode(k)?,
        _ => {
            let doc = signature::EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                .map_err(|_| anyhow::anyhow!("key generation"))?;
            st.store.set_setting(VAPID_KEY, &hex::encode(doc.as_ref())).await?;
            doc.as_ref().to_vec()
        }
    };
    signature::EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
        .map_err(|_| anyhow::anyhow!("bad VAPID key"))
}

/// The public key browsers subscribe with (base64url).
pub async fn public_key(st: &HeadState) -> anyhow::Result<String> {
    use signature::KeyPair;
    Ok(b64().encode(vapid(st).await?.public_key().as_ref()))
}

/// `Authorization` for a push service: a signed, 12-hour JWT for its origin.
async fn authorization(st: &HeadState, endpoint: &str) -> anyhow::Result<String> {
    let public = st.store.setting(crate::notify::PUBLIC_URL).await?.unwrap_or_default();
    // Push services want a contact: the head's address, else a placeholder.
    let sub = if public.starts_with("https://") { public } else { "mailto:carbon6@localhost.invalid".into() };
    vapid_header(&vapid(st).await?, endpoint, &sub, chrono::Utc::now().timestamp() + 12 * 3600)
}

fn vapid_header(key: &signature::EcdsaKeyPair, endpoint: &str, sub: &str, exp: i64) -> anyhow::Result<String> {
    use signature::KeyPair;
    let aud = url::Url::parse(endpoint)?.origin().ascii_serialization();
    let header = b64().encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = b64().encode(serde_json::json!({"aud": aud, "exp": exp, "sub": sub}).to_string());
    let input = format!("{header}.{claims}");
    let sig = key.sign(&SystemRandom::new(), input.as_bytes()).map_err(|_| anyhow::anyhow!("signing"))?;
    Ok(format!("vapid t={input}.{}, k={}", b64().encode(sig.as_ref()), b64().encode(key.public_key().as_ref())))
}

/// A device's subscription id: a short hash of its endpoint.
fn id_of(endpoint: &str) -> String {
    c6_core::sha256_hex(endpoint.as_bytes())[..16].to_owned()
}

pub async fn subscribe(st: &HeadState, s: &api::PushSubscription) -> anyhow::Result<api::PushDevice> {
    let u = url::Url::parse(&s.endpoint).map_err(|_| bad_request("the endpoint is not a URL"))?;
    // Push services are https; plain http only on this machine (tests).
    let local = u.scheme() == "http" && matches!(u.host_str(), Some("127.0.0.1" | "localhost"));
    if u.scheme() != "https" && !local {
        return Err(bad_request("a push endpoint is https"));
    }
    if unb64(&s.keys.p256dh).map(|k| k.len()).unwrap_or(0) != 65 || unb64(&s.keys.auth).map(|k| k.len()).unwrap_or(0) != 16 {
        return Err(bad_request("the subscription's keys are not valid"));
    }
    let name =
        if s.name.trim().is_empty() { u.host_str().unwrap_or("a device").to_owned() } else { s.name.trim().chars().take(80).collect() };
    sqlx::query(
        "INSERT INTO push_subscriptions (id, endpoint, p256dh, auth, name, created_at) VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth, name = excluded.name",
    )
    .bind(id_of(&s.endpoint))
    .bind(&s.endpoint)
    .bind(s.keys.p256dh.trim())
    .bind(s.keys.auth.trim())
    .bind(&name)
    .bind(c6_core::now_ms())
    .execute(&st.store.pool)
    .await?;
    info!(device = %name, "push subscription added");
    Ok(devices(st).await?.into_iter().find(|d| d.id == id_of(&s.endpoint)).expect("just added"))
}

pub async fn devices(st: &HeadState) -> anyhow::Result<Vec<api::PushDevice>> {
    let rows = sqlx::query("SELECT id, endpoint, name, created_at, last_sent_at, last_error FROM push_subscriptions ORDER BY created_at")
        .fetch_all(&st.store.pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| api::PushDevice {
            id: r.get("id"),
            name: r.get("name"),
            endpoint: r.get("endpoint"),
            service: url::Url::parse(&r.get::<String, _>("endpoint"))
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default(),
            created_at: r.get("created_at"),
            last_sent_at: r.get("last_sent_at"),
            last_error: r.get("last_error"),
        })
        .collect())
}

pub async fn unsubscribe(st: &HeadState, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("DELETE FROM push_subscriptions WHERE id = ?").bind(id).execute(&st.store.pool).await?.rows_affected() > 0)
}

/// Sends a message to every subscribed device. Devices whose subscription
/// is gone (404, 410) are removed. Err: no device got it.
pub async fn send_all(st: &HeadState, payload: &serde_json::Value, urgent: bool) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT id, endpoint, p256dh, auth, name FROM push_subscriptions").fetch_all(&st.store.pool).await?;
    anyhow::ensure!(!rows.is_empty(), "no device has turned on notifications (Settings → Notifications, on each device)");
    let body = payload.to_string();
    let mut errors = Vec::new();
    for r in &rows {
        let (id, endpoint, name): (String, String, String) = (r.get("id"), r.get("endpoint"), r.get("name"));
        let result = async {
            let data = encrypt(&r.get::<String, _>("p256dh"), &r.get::<String, _>("auth"), body.as_bytes())?;
            let resp = st
                .http
                .post(&endpoint)
                .header("Authorization", authorization(st, &endpoint).await?)
                .header("Content-Encoding", "aes128gcm")
                .header("Content-Type", "application/octet-stream")
                .header("TTL", "86400")
                .header("Urgency", if urgent { "high" } else { "normal" })
                .body(data)
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await?;
            let status = resp.status();
            if status.is_success() {
                return anyhow::Ok(());
            }
            if status.as_u16() == 404 || status.as_u16() == 410 {
                unsubscribe(st, &id).await?;
                anyhow::bail!("{name}: its subscription has ended (removed)");
            }
            anyhow::bail!("{name}: {status}: {}", resp.text().await.unwrap_or_default().chars().take(200).collect::<String>())
        }
        .await;
        let (sent, error) = match &result {
            Ok(()) => (Some(c6_core::now_ms()), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        sqlx::query("UPDATE push_subscriptions SET last_sent_at = COALESCE(?, last_sent_at), last_error = ? WHERE id = ?")
            .bind(sent)
            .bind(&error)
            .bind(&id)
            .execute(&st.store.pool)
            .await?;
        if let Some(e) = error {
            errors.push(e);
        }
    }
    anyhow::ensure!(errors.len() < rows.len(), "{}", errors.join("; "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Vec<u8> {
        unb64(&s.replace([' ', '\n'], "")).unwrap()
    }

    /// RFC 8291, Appendix A.
    #[test]
    fn rfc8291_example() {
        let out = seal(
            &d("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
            &d("BTBZMqHH6r4Tts7J_aSIgg"),
            &d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
            &d("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
            &d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap(),
            &d("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24"),
        )
        .unwrap();
        let mut want =
            d("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        want.extend(d("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"));
        assert_eq!(out, want);
    }

    /// What a browser does with a message: the reverse of `seal`.
    #[test]
    fn a_device_can_read_it() {
        let rng = SystemRandom::new();
        let ua = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = ua.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [7u8; 16];
        let body = encrypt(&b64().encode(&ua_public), &b64().encode(auth), b"{\"title\":\"hi\"}").unwrap();
        let (salt, rest) = body.split_at(16);
        assert_eq!(u32::from_be_bytes(rest[..4].try_into().unwrap()), RECORD_SIZE);
        let as_public = &rest[5..5 + rest[4] as usize];
        let ciphertext = &rest[5 + rest[4] as usize..];
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let secret = agreement::agree_ephemeral(ua, &peer, |s| s.to_vec()).unwrap();
        let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, &auth).extract(&secret);
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&ua_public);
        key_info.extend_from_slice(as_public);
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&expand(&prk_key, &key_info, 32));
        let key =
            aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &expand(&prk, b"Content-Encoding: aes128gcm\0", 16)).unwrap());
        let nonce = aead::Nonce::assume_unique_for_key(expand(&prk, b"Content-Encoding: nonce\0", 12).try_into().unwrap());
        let mut data = ciphertext.to_vec();
        let plain = key.open_in_place(nonce, aead::Aad::empty(), &mut data).unwrap();
        assert_eq!(plain, b"{\"title\":\"hi\"}\x02");
    }

    #[test]
    fn vapid_jwt_verifies() {
        use signature::KeyPair;
        let rng = SystemRandom::new();
        let doc = signature::EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = signature::EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, doc.as_ref(), &rng).unwrap();
        let h = vapid_header(&key, "https://fcm.googleapis.com/fcm/send/abc", "mailto:x@y.invalid", 2_000_000_000).unwrap();
        let (t, k) = h.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(unb64(k).unwrap(), key.public_key().as_ref());
        let (input, sig) = t.rsplit_once('.').unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&unb64(input.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, unb64(k).unwrap())
            .verify(input.as_bytes(), &unb64(sig).unwrap())
            .expect("a valid ES256 signature");
    }
}
