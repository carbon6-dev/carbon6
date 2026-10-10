//! HTTPS for the web UI (passkeys need a secure context).
//!
//! The certificate comes from `ui_tls_cert`/`ui_tls_key`, or from Tailscale
//! (`ui_tls_tailscale`: `tailscale cert` for this machine's MagicDNS name,
//! a publicly trusted certificate). It is reloaded twice a day, so renewals
//! need no restart.
//!
//! TLS and plain HTTP share the UI port: the first byte of a connection
//! tells them apart. Plain HTTP is redirected to the HTTPS name, except the
//! ChatGPT OAuth callback, which OpenAI requires on `http://127.0.0.1`.

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

/// The UI's certificate and the name it is for.
pub struct UiTls {
    /// The HTTPS host name (the certificate's name).
    pub name: String,
    slot: Arc<Slot>,
    source: Source,
}

enum Source {
    Files { cert: PathBuf, key: PathBuf },
    Tailscale { dir: PathBuf },
}

#[derive(Debug)]
struct Slot(RwLock<Arc<CertifiedKey>>);

impl ResolvesServerCert for Slot {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.read().unwrap().clone())
    }
}

impl UiTls {
    /// The configured certificate, if any.
    pub async fn setup(cfg: &HeadConfig) -> anyhow::Result<Option<Self>> {
        let (name, source) = match (&cfg.ui_tls_cert, &cfg.ui_tls_key) {
            (Some(cert), Some(key)) => {
                let name = cert_name(cert)?;
                (name, Source::Files { cert: cert.clone(), key: key.clone() })
            }
            (Some(_), None) | (None, Some(_)) => anyhow::bail!("ui_tls_cert and ui_tls_key go together"),
            (None, None) if cfg.ui_tls_tailscale => {
                let Some(name) = crate::config::tailnet_names().await.into_iter().next() else {
                    warn!("ui_tls_tailscale is set, but Tailscale is not running here (or MagicDNS is off): serving plain HTTP");
                    return Ok(None);
                };
                let source = Source::Tailscale { dir: cfg.data_dir.join("ui-tls") };
                // Not fatal: sign-in still works with login links (and
                // passkeys on localhost) until HTTPS is available.
                match load(&name, &source).await {
                    Ok(key) => {
                        info!(%name, "web UI serves HTTPS");
                        return Ok(Some(Self { name, slot: Arc::new(Slot(RwLock::new(key))), source }));
                    }
                    Err(e) => {
                        warn!("no Tailscale certificate, serving plain HTTP: {e:#}");
                        return Ok(None);
                    }
                }
            }
            (None, None) => return Ok(None),
        };
        let key = load(&name, &source).await?;
        info!(%name, "web UI serves HTTPS");
        Ok(Some(Self { name, slot: Arc::new(Slot(RwLock::new(key))), source }))
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

    /// Reloads (and for Tailscale renews) the certificate twice a day.
    pub fn spawn_refresh(self: &Arc<Self>, shutdown: CancellationToken) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(12 * 3600)) => {}
                }
                match load(&me.name, &me.source).await {
                    Ok(key) => *me.slot.0.write().unwrap() = key,
                    Err(e) => warn!("refreshing the web UI certificate: {e:#}"),
                }
            }
        });
    }
}

async fn load(name: &str, source: &Source) -> anyhow::Result<Arc<CertifiedKey>> {
    let (cert, key) = match source {
        Source::Files { cert, key } => (cert.clone(), key.clone()),
        Source::Tailscale { dir } => {
            std::fs::create_dir_all(dir)?;
            let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
            // Renews only when the certificate is close to expiry.
            let out = tokio::process::Command::new(crate::config::tailscale_cli().await.context("tailscale CLI not found")?)
                .arg("cert")
                .arg("--cert-file")
                .arg(&cert)
                .arg("--key-file")
                .arg(&key)
                .arg(name)
                .output()
                .await
                .context("running tailscale cert")?;
            if !out.status.success() {
                anyhow::bail!(
                    "tailscale cert {name} failed: {}. HTTPS certificates must be enabled for the tailnet (admin console → DNS).",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            (cert, key)
        }
    };
    let chain = CertificateDer::pem_file_iter(&cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing {}", cert.display()))?;
    let key = PrivateKeyDer::from_pem_file(&key).with_context(|| format!("reading {}", key.display()))?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).context("unsupported private key")?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

/// The first DNS name in a certificate file.
pub(crate) fn cert_name(path: &Path) -> anyhow::Result<String> {
    let der = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading {}", path.display()))?
        .next()
        .context("no certificate in file")??;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).context("parsing certificate")?;
    let san = cert.subject_alternative_name().ok().flatten().context("certificate has no subject alternative names")?;
    san.value
        .general_names
        .iter()
        .find_map(|n| match n {
            x509_parser::extensions::GeneralName::DNSName(d) => Some(d.to_ascii_lowercase()),
            _ => None,
        })
        .context("certificate has no DNS name")
}

/// Serves the UI on `listener`. With `tls`, TLS connections get the app
/// (marked as TLS) and plain ones a redirect to `https://name:port`.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<(TlsAcceptor, String)>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let port = listener.local_addr()?.port();
    let secure_app = app.clone().layer(axum::Extension(crate::auth::Tls));
    let plain_app = match &tls {
        Some((_, name)) => app.layer(axum::middleware::from_fn_with_state(Arc::new(format!("https://{name}:{port}")), to_https)),
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

/// Plain HTTP when HTTPS is on: redirect, except the OAuth callback.
async fn to_https(State(base): State<Arc<String>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/auth/callback" {
        return next.run(req).await;
    }
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, format!("{base}{path}"))]).into_response()
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

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
        let r = plain.get(format!("http://127.0.0.1:{port}/auth/callback?code=x&state=y")).send().await.unwrap();
        assert_ne!(r.status(), StatusCode::PERMANENT_REDIRECT);
        head.shutdown().await.unwrap();
    }
}
