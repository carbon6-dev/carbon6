//! Embedded UI representations and JSON transfer compression.

use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::{EmbeddedFile, RustEmbed};
use tower_http::compression::{CompressionLayer, predicate::Predicate};

#[derive(RustEmbed)]
#[folder = "../c6-ui/dist"]
#[allow_missing = true]
struct UiAssets;

/// Compress API JSON, not event streams, WebSockets, or already precompressed UI
/// assets. Keeping this outside auth also covers JSON errors and login replies.
pub(crate) fn json_compression() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new().br(true).gzip(true).compress_when(
        |_status, _version, headers: &HeaderMap, _extensions: &axum::http::Extensions| {
            headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| {
                let mime = v.split(';').next().unwrap_or("").trim();
                mime == "application/json" || mime.starts_with("application/") && mime.ends_with("+json")
            })
        },
    )
}

pub(crate) async fn static_asset(uri: Uri, method: Method, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    // Sidecars are representations, not independently addressable assets.
    if path.ends_with(".br") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // SPA: unknown paths serve index.html and the client router takes over.
    let (path, file) = match UiAssets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => (path, f),
        None => match UiAssets::get("index.html") {
            Some(f) => ("index.html", f),
            None => return (StatusCode::NOT_FOUND, "web UI not built; run `make ui`").into_response(),
        },
    };
    asset_response(path, &method, &headers, file, UiAssets::get(&format!("{path}.br")))
}

fn asset_response(path: &str, method: &Method, headers: &HeaderMap, file: EmbeddedFile, br: Option<EmbeddedFile>) -> Response {
    let encoding = negotiate(headers, br.is_some());
    let Some(compressed) = encoding else {
        return (StatusCode::NOT_ACCEPTABLE, [(header::VARY, "Accept-Encoding")]).into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if is_fingerprinted(path) { "public, max-age=31536000, immutable" } else { "no-cache" };
    let file = if compressed { br.expect("negotiation requires a sidecar") } else { file };
    // Use rust-embed's precomputed hash, not a hash of the body on every request.
    // Strong tags belong to a representation: Brotli and identity differ.
    let etag = format!("\"{}\"", hex::encode(file.metadata.sha256_hash()));
    let matched = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .any(|value| value.split(',').map(str::trim).any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag));
    let mut response = if matched {
        if method == Method::GET || method == Method::HEAD { StatusCode::NOT_MODIFIED } else { StatusCode::PRECONDITION_FAILED }
            .into_response()
    } else {
        file.data.into_response()
    };
    let response_headers = response.headers_mut();
    response_headers.insert(header::CONTENT_TYPE, mime.as_ref().parse().expect("valid MIME"));
    response_headers.insert(header::CACHE_CONTROL, cache.parse().unwrap());
    response_headers.insert(header::VARY, header::HeaderValue::from_static("Accept-Encoding"));
    response_headers.insert(header::ETAG, etag.parse().unwrap());
    if compressed && response.status() != StatusCode::PRECONDITION_FAILED {
        response.headers_mut().insert(header::CONTENT_ENCODING, header::HeaderValue::from_static("br"));
    }
    response
}

/// Prefer an explicitly accepted Brotli representation; identity is the fallback
/// unless its explicit quality wins, or identity (including `*;q=0`) is refused.
/// A missing/empty field selects identity. Explicit codings override wildcards.
fn negotiate(headers: &HeaderMap, has_br: bool) -> Option<bool> {
    let (mut br, mut identity, mut wildcard) = (None, None, None);
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for coding in value.split(',') {
            let mut parts = coding.split(';');
            let name = parts.next().unwrap_or("").trim();
            let q = parts.next().map_or(1000, |param| {
                param
                    .trim()
                    .split_once('=')
                    .filter(|(key, _)| key.trim().eq_ignore_ascii_case("q"))
                    .and_then(|(_, value)| quality(value.trim()))
                    .unwrap_or(0)
            });
            let slot = if name.eq_ignore_ascii_case("br") {
                &mut br
            } else if name.eq_ignore_ascii_case("identity") {
                &mut identity
            } else if name == "*" {
                &mut wildcard
            } else {
                continue;
            };
            *slot = Some(slot.unwrap_or(0).max(q));
        }
    }
    let br = if has_br { br.or(wildcard).unwrap_or(0) } else { 0 };
    if br > 0 && br >= identity.unwrap_or(0) {
        Some(true)
    } else if identity.unwrap_or(if wildcard == Some(0) { 0 } else { 1000 }) > 0 {
        Some(false)
    } else {
        None
    }
}

/// RFC qvalues have at most three fractional digits and a range of 0..=1.
fn quality(s: &str) -> Option<u16> {
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let fraction = fraction.parse::<u16>().unwrap_or(0) * 10u16.pow(3 - fraction.len() as u32);
    match whole {
        "0" => Some(fraction),
        "1" if fraction == 0 => Some(1000),
        _ => None,
    }
}

/// Trunk's outputs: `name-<hex u64>.ext`, and `name-<hex u64>_bg.wasm`.
/// Its hexadecimal hashes are not zero-padded, so leading zeroes shorten them.
fn is_fingerprinted(path: &str) -> bool {
    let Some((stem, extension)) = path.rsplit('/').next().unwrap_or(path).rsplit_once('.') else { return false };
    // Copied icons such as icon-192.png have numeric sizes, not build hashes.
    if !matches!(extension, "wasm" | "js" | "css") {
        return false;
    }
    let stem = stem.strip_suffix("_bg").unwrap_or(stem);
    stem.rsplit_once('-').is_some_and(|(_, h)| (1..=16).contains(&h.len()) && h.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        routing::get,
    };
    use tower::ServiceExt;

    fn headers(value: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(value) = value {
            h.insert(header::ACCEPT_ENCODING, value.parse().unwrap());
        }
        h
    }

    fn file(data: &'static [u8]) -> EmbeddedFile {
        EmbeddedFile { data: data.into(), metadata: rust_embed::Metadata::__rust_embed_new([data[0]; 32], None, None) }
    }

    #[test]
    fn accepts_qvalues_wildcards_and_identity() {
        for (accept, expected) in [
            (None, Some(false)),
            (Some(""), Some(false)),
            (Some("gzip"), Some(false)),
            (Some("br"), Some(true)),
            (Some("gzip, br;q=0.5"), Some(true)),
            (Some("br;q=0"), Some(false)),
            (Some("BR; Q=0.25"), Some(true)),
            (Some("*;q=1, br;q=0"), Some(false)),
            (Some("*;q=0"), None),
            (Some("*;q=0, br;q=0.1"), Some(true)),
            (Some("br;q=0.5, identity;q=0.9"), Some(false)),
            (Some("br;q=0, identity;q=0"), None),
            (Some("br;q=NaN"), Some(false)),
            (Some("br;q=1.001"), Some(false)),
            (Some("br;q=0.0001"), Some(false)),
        ] {
            assert_eq!(negotiate(&headers(accept), true), expected, "{accept:?}");
        }
        assert_eq!(negotiate(&headers(Some("br")), false), Some(false));
        assert_eq!(negotiate(&headers(Some("br, identity;q=0")), false), None);
        let mut h = headers(Some("gzip"));
        h.append(header::ACCEPT_ENCODING, "br".parse().unwrap());
        assert_eq!(negotiate(&h, true), Some(true));
    }

    #[tokio::test]
    async fn sidecars_keep_mime_cache_and_vary_even_for_identity() {
        for (accept, compressed) in [(None, false), (Some("br;q=0"), false), (Some("br"), true)] {
            let response =
                asset_response("c6-ui-789d462898aabf89_bg.wasm", &Method::GET, &headers(accept), file(b"wasm"), Some(file(b"brotli")));
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
            assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
            assert_eq!(response.headers().get(header::CONTENT_ENCODING).map(|h| h.to_str().unwrap()), compressed.then_some("br"));
            let body = to_bytes(response.into_body(), 100).await.unwrap();
            assert_eq!(body.as_ref(), if compressed { b"brotli".as_slice() } else { b"wasm".as_slice() });
        }
        let response = asset_response("js/theme.js", &Method::GET, &headers(Some("br, identity;q=0")), file(b"js"), None);
        assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
        assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
    }

    #[tokio::test]
    async fn revalidation_uses_embed_hash_and_keeps_representation_headers() {
        for path in ["index.html", "js/theme.js", "vendor/xterm.css", "sw.js"] {
            for accept in [None, Some("br")] {
                let mut request = headers(accept);
                let first = asset_response(path, &Method::GET, &request, file(b"original"), Some(file(b"compressed")));
                assert_eq!(first.status(), StatusCode::OK);
                let etag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
                let expected_hash = if accept.is_some() { b'c' } else { b'o' };
                assert_eq!(etag, format!("\"{}\"", hex::encode([expected_hash; 32])));
                for value in [etag.clone(), format!("\"older\", W/{etag}"), "*".to_owned()] {
                    request.insert(header::IF_NONE_MATCH, value.parse().unwrap());
                    for method in [Method::GET, Method::HEAD] {
                        let response = asset_response(path, &method, &request, file(b"original"), Some(file(b"compressed")));
                        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
                        for name in [header::ETAG, header::CACHE_CONTROL, header::VARY, header::CONTENT_ENCODING] {
                            assert_eq!(response.headers().get(&name), first.headers().get(&name));
                        }
                        assert!(to_bytes(response.into_body(), 100).await.unwrap().is_empty());
                    }
                }
                request.insert(header::IF_NONE_MATCH, "\"older\"".parse().unwrap());
                let response = asset_response(path, &Method::GET, &request, file(b"original"), Some(file(b"compressed")));
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
            }
        }
        let original = asset_response("index.html", &Method::GET, &headers(None), file(b"original"), Some(file(b"compressed")));
        let mut request = headers(Some("br"));
        request.insert(header::IF_NONE_MATCH, original.headers()[header::ETAG].clone());
        assert_eq!(
            asset_response("index.html", &Method::GET, &request, file(b"original"), Some(file(b"compressed"))).status(),
            StatusCode::OK
        );
        request = headers(None);
        request.insert(header::IF_NONE_MATCH, original.headers()[header::ETAG].clone());
        assert_eq!(asset_response("index.html", &Method::GET, &request, file(b"new deploy"), None).status(), StatusCode::OK);
        assert_eq!(
            asset_response("index.html", &Method::POST, &request, file(b"original"), None).status(),
            StatusCode::PRECONDITION_FAILED
        );
        request.insert(header::IF_NONE_MATCH, "\"older\"".parse().unwrap());
        request.append(header::IF_NONE_MATCH, original.headers()[header::ETAG].clone());
        assert_eq!(asset_response("index.html", &Method::GET, &request, file(b"original"), None).status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn head_and_304_are_bodyless_through_the_router_and_compression_layer() {
        let app = Router::new()
            .route(
                "/",
                get(|method: Method, request: HeaderMap| async move {
                    asset_response("index.html", &method, &request, file(b"original"), Some(file(b"compressed")))
                }),
            )
            .layer(json_compression());
        let response = app.clone().oneshot(axum::http::Request::builder().body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        let etag = response.headers()[header::ETAG].clone();
        for method in [Method::GET, Method::HEAD] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder().method(method.clone()).header(header::IF_NONE_MATCH, &etag).body(Body::empty()).unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(response.headers()[header::ETAG], etag);
            assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
            assert!(to_bytes(response.into_body(), 100).await.unwrap().is_empty());
            let response = app
                .clone()
                .oneshot(axum::http::Request::builder().method(method).header(header::ACCEPT_ENCODING, "br").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.headers()[header::CONTENT_ENCODING], "br");
            assert_ne!(response.headers()[header::ETAG], etag);
        }
        let response = app.oneshot(axum::http::Request::builder().method(Method::HEAD).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(to_bytes(response.into_body(), 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn json_compression_negotiates_br_and_gzip_but_leaves_streams_alone() {
        for (accept, expected) in
            [("br", Some("br")), ("gzip", Some("gzip")), ("br;q=0, gzip;q=0", None), ("br;q=0.2, gzip;q=0.8", Some("gzip"))]
        {
            let app = Router::new().route("/", get(|| async { axum::Json(vec!["hello"; 100]) })).layer(json_compression());
            let response = app
                .oneshot(axum::http::Request::builder().uri("/").header(header::ACCEPT_ENCODING, accept).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.headers().get(header::CONTENT_ENCODING).map(|v| v.to_str().unwrap()), expected);
            assert_eq!(response.headers()[header::VARY], "accept-encoding");
            assert!(!to_bytes(response.into_body(), 10000).await.unwrap().is_empty());
        }
        let app = Router::new()
            .route("/", get(|| async { ([(header::CONTENT_TYPE, "text/event-stream")], "data: hello\n\n") }))
            .layer(json_compression());
        let response =
            app.oneshot(axum::http::Request::builder().header(header::ACCEPT_ENCODING, "br").body(Body::empty()).unwrap()).await.unwrap();
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
    }

    #[test]
    fn trunk_outputs_are_fingerprinted_and_the_rest_not() {
        assert!(is_fingerprinted("c6-ui-789d462898aabf89_bg.wasm"));
        assert!(is_fingerprinted("c6-ui-1de8bb420f162f4a.js"));
        assert!(is_fingerprinted("input-d3c90d3631cf98b5.css"));
        // A real production build emitted this unpadded fifteen-digit hash.
        assert!(is_fingerprinted("c6-ui-60f6b9ce24946dd_bg.wasm"));
        assert!(is_fingerprinted("c6-ui-60f6b9ce24946dd.js"));
        assert!(is_fingerprinted("nested/input-a.css"));
        assert!(is_fingerprinted("c6-ui-0_bg.wasm"));
        for path in [
            "index.html",
            "vendor/xterm.js",
            "js/terminal.js",
            "icon-192.png",
            "logo-a.svg",
            "page-a.html",
            "c6-ui-notahexhashxxxxx_bg.wasm",
            "c6-ui-_bg.wasm",
            "c6-ui-123456789abcdef01_bg.wasm",
        ] {
            assert!(!is_fingerprinted(path));
        }
    }
}
