//! A stand-in for S3 on loopback: path-style buckets, put, get, delete and
//! paged listing, refusing requests not signed with its access key. It
//! doesn't check signatures (the signer is checked against AWS's own
//! example instead), and can be switched off to act as unreachable.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

#[derive(Default)]
struct Inner {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    access_key: String,
    bucket: String,
    down: AtomicBool,
    puts: AtomicU64,
    put_bytes: AtomicU64,
}

#[derive(Clone)]
pub struct MockS3 {
    inner: Arc<Inner>,
    addr: std::net::SocketAddr,
}

/// Names in one page of a listing (small, so paging is exercised).
const PAGE: usize = 10;

impl MockS3 {
    pub async fn start(access_key: &str, bucket: &str) -> Self {
        let inner = Arc::new(Inner { access_key: access_key.into(), bucket: bucket.into(), ..Default::default() });
        let app = Router::new().route("/{bucket}", any(bucket_op)).route("/{bucket}/{*key}", any(object_op)).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { inner, addr }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every object's key.
    pub fn keys(&self) -> Vec<String> {
        self.inner.objects.lock().unwrap().keys().cloned().collect()
    }

    pub fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.objects.lock().unwrap().get(key).cloned()
    }

    /// Unreachable (every request fails) until `false`.
    pub fn set_down(&self, down: bool) {
        self.inner.down.store(down, Ordering::SeqCst);
    }

    /// How many objects were written, and their bytes.
    pub fn puts(&self) -> (u64, u64) {
        (self.inner.puts.load(Ordering::SeqCst), self.inner.put_bytes.load(Ordering::SeqCst))
    }
}

fn refused(st: &Inner, headers: &HeaderMap, bucket: &str) -> Option<Response> {
    if st.down.load(Ordering::SeqCst) {
        return Some((StatusCode::SERVICE_UNAVAILABLE, "<Error><Message>down</Message></Error>").into_response());
    }
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !auth.starts_with("AWS4-HMAC-SHA256 ") || !auth.contains(&format!("Credential={}/", st.access_key)) {
        return Some((StatusCode::FORBIDDEN, "<Error><Message>InvalidAccessKeyId</Message></Error>").into_response());
    }
    if bucket != st.bucket {
        return Some((StatusCode::NOT_FOUND, "<Error><Message>NoSuchBucket</Message></Error>").into_response());
    }
    None
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

async fn bucket_op(
    State(st): State<Arc<Inner>>,
    Path(bucket): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = refused(&st, &headers, &bucket) {
        return r;
    }
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let after = q.get("continuation-token").cloned().unwrap_or_default();
    let objects = st.objects.lock().unwrap();
    let matching: Vec<(&String, &Vec<u8>)> =
        objects.iter().filter(|(k, _)| k.starts_with(&prefix) && k.as_str() > after.as_str()).collect();
    let page = &matching[..matching.len().min(PAGE)];
    let truncated = matching.len() > PAGE;
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult>");
    for (k, v) in page {
        xml.push_str(&format!("<Contents><Key>{}</Key><Size>{}</Size></Contents>", esc(k), v.len()));
    }
    xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
    if truncated && let Some((k, _)) = page.last() {
        xml.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", esc(k)));
    }
    xml.push_str("</ListBucketResult>");
    (StatusCode::OK, xml).into_response()
}

async fn object_op(
    State(st): State<Arc<Inner>>,
    method: Method,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(r) = refused(&st, &headers, &bucket) {
        return r;
    }
    let mut objects = st.objects.lock().unwrap();
    match method {
        Method::PUT => {
            st.puts.fetch_add(1, Ordering::SeqCst);
            st.put_bytes.fetch_add(body.len() as u64, Ordering::SeqCst);
            objects.insert(key, body.to_vec());
            StatusCode::OK.into_response()
        }
        Method::GET => match objects.get(&key) {
            Some(v) => (StatusCode::OK, v.clone()).into_response(),
            None => (StatusCode::NOT_FOUND, "<Error><Message>NoSuchKey</Message></Error>").into_response(),
        },
        Method::DELETE => {
            objects.remove(&key);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}
