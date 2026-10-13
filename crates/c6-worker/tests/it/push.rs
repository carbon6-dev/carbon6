//! Web push: a device subscribes with its key; a push channel sends it
//! messages it can decrypt, signed with the head's VAPID key; a gone
//! subscription is removed.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api::Connection;
use c6_head::HeadConfig;
use c6_testkit::MockLlm;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Requests the stand-in push service got: (headers, body).
type Received = Arc<Mutex<Vec<(axum::http::HeaderMap, Vec<u8>)>>>;

/// A stand-in push service: records (headers, body), answers `status`.
async fn push_service(status: Arc<AtomicU16>) -> (String, Received) {
    let got: Received = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().route(
        "/push/{id}",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let (g, status) = (g.clone(), status.clone());
            async move {
                g.lock().unwrap().push((headers, body.to_vec()));
                axum::http::StatusCode::from_u16(status.load(Ordering::Relaxed)).unwrap()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

struct Len(usize);
impl ring::hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// What the browser does: RFC 8291 decryption with its private key.
fn decrypt(ua: ring::agreement::EphemeralPrivateKey, ua_public: &[u8], auth: &[u8], body: &[u8]) -> Vec<u8> {
    use ring::{aead, agreement, hkdf};
    let expand = |prk: &hkdf::Prk, info: &[u8], n: usize| {
        let mut out = vec![0u8; n];
        prk.expand(&[info], Len(n)).unwrap().fill(&mut out).unwrap();
        out
    };
    let (salt, rest) = body.split_at(16);
    let idlen = rest[4] as usize;
    let (as_public, ciphertext) = (&rest[5..5 + idlen], &rest[5 + idlen..]);
    let secret =
        agreement::agree_ephemeral(ua, &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public), |s| s.to_vec()).unwrap();
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_public);
    info.extend_from_slice(as_public);
    let ikm = expand(&hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(&secret), &info, 32);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let key =
        aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &expand(&prk, b"Content-Encoding: aes128gcm\0", 16)).unwrap());
    let nonce = aead::Nonce::assume_unique_for_key(expand(&prk, b"Content-Encoding: nonce\0", 12).try_into().unwrap());
    let mut data = ciphertext.to_vec();
    let plain = key.open_in_place(nonce, aead::Aad::empty(), &mut data).unwrap();
    assert_eq!(plain.last(), Some(&2), "the last-record delimiter");
    plain[..plain.len() - 1].to_vec()
}

#[tokio::test]
async fn web_push_to_a_device() {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let _ = (&conn, &worker, &mock);
    let status = Arc::new(AtomicU16::new(201));
    let (service, got) = push_service(status.clone()).await;
    let key: c6_api::PushKey = http.get(format!("{api}/push/key")).send().await.unwrap().json().await.unwrap();
    assert_eq!(b64.decode(&key.key).unwrap().len(), 65, "an uncompressed P-256 key");

    // A device subscribes (as the browser's PushSubscription.toJSON()).
    let rng = ring::rand::SystemRandom::new();
    let ua = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng).unwrap();
    let ua_public = ua.compute_public_key().unwrap().as_ref().to_vec();
    let auth = [9u8; 16];
    let endpoint = format!("{service}/push/dev1");
    let sub = json!({"endpoint": endpoint, "keys": {"p256dh": b64.encode(&ua_public), "auth": b64.encode(auth)}, "name": "Phone"});
    let bad = json!({"endpoint": "http://example.com/x", "keys": {"p256dh": b64.encode(&ua_public), "auth": b64.encode(auth)}});
    assert_eq!(http.post(format!("{api}/push/subscriptions")).json(&bad).send().await.unwrap().status(), reqwest::StatusCode::BAD_REQUEST);
    let d: c6_api::PushDevice = http.post(format!("{api}/push/subscriptions")).json(&sub).send().await.unwrap().json().await.unwrap();
    assert_eq!(d.name, "Phone");

    let put = |v: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/channels"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert!(put(json!([{"name": "devices", "kind": "push"}])).await.is_success());
    let test = || {
        let (http, url) = (http.clone(), format!("{api}/settings/channels/test"));
        async move { http.post(url).json(&json!({"name": "devices"})).send().await.unwrap() }
    };
    assert_eq!(test().await.status(), reqwest::StatusCode::NO_CONTENT);
    let (headers, body) = got.lock().unwrap()[0].clone();
    assert_eq!(headers["content-encoding"], "aes128gcm");
    assert_eq!(headers["ttl"], "86400");
    let authz = headers["authorization"].to_str().unwrap().to_owned();
    let (t, k) = authz.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
    assert_eq!(k, key.key, "signed with the key devices subscribed with");
    let (input, sig) = t.rsplit_once('.').unwrap();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, b64.decode(k).unwrap())
        .verify(input.as_bytes(), &b64.decode(sig).unwrap())
        .expect("a valid VAPID signature");
    let claims: serde_json::Value = serde_json::from_slice(&b64.decode(input.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!(claims["aud"], service.as_str());
    let msg: serde_json::Value = serde_json::from_slice(&decrypt(ua, &ua_public, &auth, &body)).unwrap();
    assert_eq!((msg["title"].as_str(), msg["url"].as_str()), (Some("carbon6 test"), Some("/")));
    let devices: Vec<c6_api::PushDevice> = http.get(format!("{api}/push/subscriptions")).send().await.unwrap().json().await.unwrap();
    assert!(devices[0].last_sent_at.is_some());

    // The service says the subscription is gone: the device is removed.
    status.store(410, Ordering::Relaxed);
    assert_eq!(test().await.status(), reqwest::StatusCode::BAD_REQUEST);
    let devices: Vec<c6_api::PushDevice> = http.get(format!("{api}/push/subscriptions")).send().await.unwrap().json().await.unwrap();
    assert!(devices.is_empty());

    stop.cancel();
    head.shutdown().await.unwrap();
}
