//! Browser-facing protection for an API without logins.
//!
//! The head trusts its network, but a web page the user visits can still
//! reach it through their browser: cross-site WebSockets need no CORS
//! approval (a terminal is a shell), simple cross-site POSTs need no
//! preflight, and DNS rebinding can make a hostile name resolve to the head.
//! So every request must name the head in `Host`, and any request a browser
//! marks with a foreign `Origin` is refused.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

#[derive(Clone, Debug)]
pub struct HostGuard {
    allowed: Arc<HashSet<String>>,
}

impl HostGuard {
    /// Accepts each listen address, plus each name on each listen port.
    pub fn new(listen: &[SocketAddr], names: &[String]) -> Self {
        let mut allowed = HashSet::new();
        for a in listen {
            let host = match a {
                SocketAddr::V4(v4) => v4.ip().to_string(),
                SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
            };
            allowed.insert(format!("{host}:{}", a.port()));
            for n in names {
                allowed.insert(format!("{}:{}", n.to_ascii_lowercase(), a.port()));
            }
        }
        Self { allowed: Arc::new(allowed) }
    }

    fn host_ok(&self, host: &str) -> bool {
        self.allowed.contains(&host.to_ascii_lowercase())
    }
}

pub async fn check(State(guard): State<HostGuard>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_owned();
    if !guard.host_ok(&host) {
        let msg = format!(
            "carbon6 does not answer to the host name {host:?}. If you reach the head by this name, add it to extra_names in c6h.toml."
        );
        return (StatusCode::MISDIRECTED_REQUEST, msg).into_response();
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("null");
        let same = origin.split_once("://").map(|(_, rest)| rest.trim_end_matches('/')).is_some_and(|o| o.eq_ignore_ascii_case(&host));
        if !same {
            return (StatusCode::FORBIDDEN, "cross-origin requests are not allowed").into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::*;

    fn app() -> Router {
        let guard = HostGuard::new(
            &["100.64.1.2:7700".parse().unwrap(), "127.0.0.1:7700".parse().unwrap()],
            &["localhost".into(), "Head.TS.net".into()],
        );
        Router::new()
            .route("/x", get(|| async { "ok" }))
            .route("/y", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(guard, check))
    }

    async fn status(host: &str, origin: Option<&str>, method: &str, path: &str) -> u16 {
        let mut b = axum::http::Request::builder().method(method).uri(path).header("host", host);
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        app().oneshot(b.body(Body::empty()).unwrap()).await.unwrap().status().as_u16()
    }

    #[tokio::test]
    async fn host_and_origin_rules() {
        assert_eq!(status("100.64.1.2:7700", None, "GET", "/x").await, 200);
        assert_eq!(status("head.ts.net:7700", Some("http://head.ts.net:7700"), "POST", "/y").await, 200);
        assert_eq!(status("localhost:7700", Some("http://localhost:7700"), "GET", "/x").await, 200);
        // DNS rebinding: a foreign name pointing at the head.
        assert_eq!(status("evil.example:7700", None, "GET", "/x").await, 421);
        // A page on another site posting to the head.
        assert_eq!(status("127.0.0.1:7700", Some("https://evil.example"), "POST", "/y").await, 403);
        // ... or opening a WebSocket (a GET with an Origin).
        assert_eq!(status("127.0.0.1:7700", Some("https://evil.example"), "GET", "/x").await, 403);
        assert_eq!(status("127.0.0.1:7700", Some("null"), "GET", "/x").await, 403);
    }
}
