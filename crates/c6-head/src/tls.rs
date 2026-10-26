//! HTTPS for the web UI (passkeys need a secure context).
//!
//! The certificate comes from `ui_tls_cert`/`ui_tls_key`, or from the head's
//! own web UI certificate authority (`ui_tls_head_ca`, see `ui_ca.rs`),
//! which keeps the head's names out of public Certificate Transparency
//! logs. Files are reloaded twice a day, so renewals need no restart.
//!
//! TLS and plain HTTP share the UI port: the first byte of a connection
//! tells them apart. Plain HTTP is redirected to the HTTPS name, except the
//! CA download (a device fetches it before it trusts the CA) and the
//! ChatGPT OAuth callback, which OpenAI requires on `http://127.0.0.1`.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::HeadConfig;
use crate::ui_ca::UiCa;

/// The UI's certificate and the name it is for.
pub struct UiTls {
    /// The HTTPS host name (the certificate's name).
    pub name: String,
    /// The head's CA, when it issued the certificate.
    pub ca: Option<Arc<UiCa>>,
    slot: Arc<Slot>,
    source: Source,
}

enum Source {
    Files { cert: PathBuf, key: PathBuf },
    HeadCa { names: Vec<String>, ips: Vec<IpAddr> },
}

#[derive(Debug)]
struct Slot(RwLock<Arc<CertifiedKey>>);

impl ResolvesServerCert for Slot {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.read().unwrap().clone())
    }
}

/// The name to serve HTTPS at with the head's CA: the first configured or
/// tailnet name with a dot, else the host name, else `localhost`.
pub fn head_ca_name(cfg: &HeadConfig) -> String {
    cfg.extra_names
        .iter()
        .find(|n| n.contains('.') && n.parse::<IpAddr>().is_err())
        .cloned()
        .or_else(|| gethostname::gethostname().into_string().ok().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_else(|| "localhost".into())
}

impl UiTls {
    /// The configured certificate, if any. `names` and `ips` are what the
    /// head answers to, for a certificate from its own CA.
    pub fn setup(cfg: &HeadConfig, names: &[String], ips: &[IpAddr]) -> anyhow::Result<Option<Self>> {
        let (name, source, ca) = match (&cfg.ui_tls_cert, &cfg.ui_tls_key) {
            (Some(cert), Some(key)) => (cert_name(cert, &cfg.extra_names)?, Source::Files { cert: cert.clone(), key: key.clone() }, None),
            (Some(_), None) | (None, Some(_)) => anyhow::bail!("ui_tls_cert and ui_tls_key go together"),
            (None, None) if cfg.ui_tls_head_ca => {
                let ca = Arc::new(UiCa::load_or_create(&cfg.data_dir.join("pki"), names)?);
                (head_ca_name(cfg), Source::HeadCa { names: names.to_vec(), ips: ips.to_vec() }, Some(ca))
            }
            (None, None) => return Ok(None),
        };
        let key = load(&source, ca.as_deref())?;
        info!(%name, "web UI serves HTTPS");
        Ok(Some(Self { name, ca, slot: Arc::new(Slot(RwLock::new(key))), source }))
    }

    pub fn acceptor(&self) -> anyhow::Result<TlsAcceptor> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(self.slot.clone());
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(TlsAcceptor::from(Arc::new(config)))
    }

    /// Reloads certificate files twice a day (the head's own certificate is
    /// issued fresh on every start).
    pub fn spawn_refresh(self: &Arc<Self>, shutdown: CancellationToken) {
        if !matches!(self.source, Source::Files { .. }) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(12 * 3600)) => {}
                }
                match load(&me.source, None) {
                    Ok(key) => *me.slot.0.write().unwrap() = key,
                    Err(e) => warn!("reloading the web UI certificate: {e:#}"),
                }
            }
        });
    }
}

fn load(source: &Source, ca: Option<&UiCa>) -> anyhow::Result<Arc<CertifiedKey>> {
    let (chain_pem, key_pem) = match (source, ca) {
        (Source::Files { cert, key }, _) => (
            std::fs::read(cert).with_context(|| format!("reading {}", cert.display()))?,
            std::fs::read(key).with_context(|| format!("reading {}", key.display()))?,
        ),
        (Source::HeadCa { names, ips }, Some(ca)) => {
            let (chain, key) = ca.issue(names, ips)?;
            (chain.into_bytes(), key.into_bytes())
        }
        (Source::HeadCa { .. }, None) => anyhow::bail!("no CA to issue the web UI certificate"),
    };
    let chain = CertificateDer::pem_slice_iter(&chain_pem).collect::<Result<Vec<_>, _>>().context("parsing the certificate chain")?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem).context("parsing the private key")?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).context("unsupported private key")?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

/// The name a certificate file serves HTTPS at: its first DNS name, unless
/// that is a wildcard, which is no address. Then the first of `extra_names`
/// the certificate covers, else its first name that isn't a wildcard.
pub(crate) fn cert_name(path: &Path, extra_names: &[String]) -> anyhow::Result<String> {
    let der = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading {}", path.display()))?
        .next()
        .context("no certificate in file")??;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).context("parsing certificate")?;
    let san = cert.subject_alternative_name().ok().flatten().context("certificate has no subject alternative names")?;
    let dns: Vec<String> = san
        .value
        .general_names
        .iter()
        .filter_map(|n| match n {
            x509_parser::extensions::GeneralName::DNSName(d) => Some(d.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    pick_name(&dns, extra_names).context("certificate has no DNS name")
}

fn pick_name(cert_names: &[String], extra_names: &[String]) -> Option<String> {
    let first = cert_names.first()?;
    if !first.starts_with("*.") {
        return Some(first.clone());
    }
    extra_names
        .iter()
        .map(|n| n.to_ascii_lowercase())
        .find(|n| cert_names.iter().any(|c| covers(c, n)))
        .or_else(|| cert_names.iter().find(|c| !c.starts_with("*.")).cloned())
        .or_else(|| Some(first.clone()))
}

/// Whether a certificate name covers `name`: the same name, or a wildcard
/// for exactly one more label (`*.example.com` covers `a.example.com`).
fn covers(cert_name: &str, name: &str) -> bool {
    match cert_name.strip_prefix("*.") {
        Some(parent) => name.split_once('.').is_some_and(|(label, rest)| !label.is_empty() && rest == parent),
        None => cert_name == name,
    }
}

/// Serves the UI on `listener`. With `tls`, TLS connections get the app
/// (marked as TLS) and plain ones a redirect to `public` (`ui_public_url`),
/// else to `https://name:port`.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<(TlsAcceptor, String)>,
    public: Option<String>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let port = listener.local_addr()?.port();
    let secure_app = app.clone().layer(axum::Extension(crate::auth::Tls));
    let plain_app = match &tls {
        Some((_, name)) => {
            let base = public.unwrap_or_else(|| format!("https://{name}:{port}"));
            app.layer(axum::middleware::from_fn_with_state(Arc::new(base), to_https))
        }
        None => app,
    };
    let acceptor = tls.map(|(a, _)| a);
    loop {
        let (stream, _) = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => { warn!("accepting a UI connection: {e}"); continue; }
            },
        };
        let (acceptor, secure_app, plain_app) = (acceptor.clone(), secure_app.clone(), plain_app.clone());
        tokio::spawn(async move {
            let mut first = [0u8; 1];
            let is_tls = acceptor.is_some()
                && matches!(tokio::time::timeout(Duration::from_secs(10), stream.peek(&mut first)).await, Ok(Ok(1)))
                && first[0] == 0x16; // TLS handshake record
            let served = match acceptor {
                Some(acceptor) if is_tls => match acceptor.accept(stream).await {
                    Ok(s) => serve_conn(TokioIo::new(s), secure_app).await,
                    Err(_) => return, // e.g. a client that does not trust the certificate
                },
                _ => serve_conn(TokioIo::new(stream), plain_app).await,
            };
            if let Err(e) = served {
                tracing::debug!("UI connection: {e}");
            }
        });
    }
}

async fn serve_conn<I>(io: TokioIo<I>, app: Router) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    auto::Builder::new(TokioExecutor::new()).serve_connection_with_upgrades(io, TowerToHyperService::new(app)).await
}

/// Plain HTTP when HTTPS is on: redirect, except the OAuth callback and
/// the CA download.
async fn to_https(State(base): State<Arc<String>>, req: Request, next: Next) -> Response {
    if matches!(req.uri().path(), "/callback" | "/ca.pem" | "/ca.crt" | "/ca.mobileconfig") {
        return next.run(req).await;
    }
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, format!("{base}{path}"))]).into_response()
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    #[test]
    fn names_from_certificate_files() {
        let pick = |cert: &[&str], extra: &[&str]| {
            let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            super::pick_name(&strings(cert), &strings(extra))
        };
        // The certificate's first name, as before, unless it is a wildcard.
        assert_eq!(pick(&["head.example.com", "*.example.com"], &["other.example.com"]).as_deref(), Some("head.example.com"));
        // A wildcard is no address: the first configured name the certificate covers ...
        assert_eq!(pick(&["*.local.example.com"], &["localhost", "Head.Local.Example.com"]).as_deref(), Some("head.local.example.com"));
        assert_eq!(pick(&["*.example.com", "head.other.com"], &["head.other.com"]).as_deref(), Some("head.other.com"));
        // ... a wildcard covering one label only ...
        assert_eq!(pick(&["*.example.com"], &["a.b.example.com", "head.example.com"]).as_deref(), Some("head.example.com"));
        // ... else the certificate's first other name, else the wildcard.
        assert_eq!(pick(&["*.example.com", "example.com"], &[]).as_deref(), Some("example.com"));
        assert_eq!(pick(&["*.example.com"], &["head.other.com"]).as_deref(), Some("*.example.com"));
        assert_eq!(pick(&[], &["head.example.com"]), None);
    }

    #[tokio::test]
    async fn wildcard_certificate_and_public_url() {
        let dir = tempfile::tempdir().unwrap();
        let ck = rcgen::generate_simple_self_signed(vec!["*.example.test".to_owned()]).unwrap();
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&cert, ck.cert.pem()).unwrap();
        std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
        let cfg = crate::HeadConfig {
            data_dir: dir.path().join("head"),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            extra_names: vec!["head.example.test".into()],
            ui_tls_cert: Some(cert),
            ui_tls_key: Some(key),
            ..Default::default()
        };
        let plain = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();

        // The head goes by the configured name its wildcard covers.
        let head = crate::start(cfg.clone()).await.unwrap();
        let port = head.ui_addrs[0].port();
        assert_eq!(head.ui_urls, [format!("https://head.example.test:{port}")]);
        let r = plain.get(format!("http://127.0.0.1:{port}/x")).send().await.unwrap();
        assert_eq!(r.headers()["location"], format!("https://head.example.test:{port}/x").as_str());
        head.shutdown().await.unwrap();

        // Behind a proxy on 443, its links and redirect use the public address.
        let cfg = crate::HeadConfig { ui_public_url: Some("https://head.example.test/".into()), ..cfg };
        let head = crate::start(cfg.clone()).await.unwrap();
        let port = head.ui_addrs[0].port();
        assert_eq!(head.ui_urls, ["https://head.example.test"]);
        let r = plain.get(format!("http://127.0.0.1:{port}/sessions/x?y=1")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(r.headers()["location"], "https://head.example.test/sessions/x?y=1");
        head.shutdown().await.unwrap();
        let links = crate::offline_login_link(cfg).await.unwrap();
        assert!(links.len() == 1 && links[0].starts_with("https://head.example.test/login/"), "{links:?}");
    }

    #[tokio::test]
    async fn https_and_plain_on_one_port() {
        let dir = tempfile::tempdir().unwrap();
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&cert, ck.cert.pem()).unwrap();
        std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().join("head"),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            ui_tls_cert: Some(cert),
            ui_tls_key: Some(key),
            ..Default::default()
        })
        .await
        .unwrap();
        let port = head.ui_addrs[0].port();
        assert_eq!(head.ui_urls, [format!("https://localhost:{port}")]);

        let https = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(ck.cert.pem().as_bytes()).unwrap())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let r = https.get(format!("https://localhost:{port}/api/v1/auth/status")).send().await.unwrap();
        assert_eq!(r.version(), reqwest::Version::HTTP_2);
        let st: serde_json::Value = r.json().await.unwrap();
        assert_eq!(st["passkeys_here"], true);

        // Sessions made over HTTPS get Secure cookies.
        let token = head.state.store.create_login_link().await.unwrap();
        let r = https
            .post(format!("https://localhost:{port}/api/v1/auth/link"))
            .json(&serde_json::json!({"token": token}))
            .send()
            .await
            .unwrap();
        assert!(r.headers()["set-cookie"].to_str().unwrap().contains("; Secure"));

        // Plain HTTP on the same port redirects, except the OAuth callback.
        let plain = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let r = plain.get(format!("http://127.0.0.1:{port}/sessions/x?y=1")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(r.headers()["location"], format!("https://localhost:{port}/sessions/x?y=1").as_str());
        let r = plain.get(format!("http://127.0.0.1:{port}/callback?code=x&state=y")).send().await.unwrap();
        assert_ne!(r.status(), StatusCode::PERMANENT_REDIRECT);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn head_ca_serves_https_and_its_ca() {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            ui_tls_head_ca: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let port = head.ui_addrs[0].port();
        let plain = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();

        // A device fetches the CA over plain HTTP, before it trusts it.
        let r = plain.get(format!("http://127.0.0.1:{port}/ca.pem")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let ca_pem = r.text().await.unwrap();
        let r = plain.get(format!("http://127.0.0.1:{port}/ca.mobileconfig")).send().await.unwrap();
        assert_eq!(r.headers()["content-type"], "application/x-apple-aspen-config");
        let r = plain.get(format!("http://127.0.0.1:{port}/ca.crt")).send().await.unwrap();
        assert_eq!(r.headers()["content-type"], "application/x-x509-ca-cert");
        let der = r.bytes().await.unwrap();
        assert_eq!(der.first(), Some(&0x30), "DER, not PEM");
        assert_eq!(plain.get(format!("http://127.0.0.1:{port}/")).send().await.unwrap().status(), StatusCode::PERMANENT_REDIRECT);

        // Trusting only that CA, HTTPS to the head's names works.
        let https =
            reqwest::Client::builder().tls_certs_only([reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap()]).build().unwrap();
        let st: serde_json::Value =
            https.get(format!("https://localhost:{port}/api/v1/auth/status")).send().await.unwrap().json().await.unwrap();
        assert_eq!((st["passkeys_here"].as_bool(), st["head_ca"].as_bool()), (Some(true), Some(true)));
        head.shutdown().await.unwrap();
    }
}
