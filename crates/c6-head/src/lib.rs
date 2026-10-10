//! The carbon6 head (`c6h`): system of record, worker hub and web UI server.

pub mod api;
pub mod config;
mod convert;
pub mod guard;
pub mod hub;
pub mod oauth;
pub mod pki;
pub mod registry;
pub mod secrets;
pub mod sessions;
pub mod store;
pub mod telemetry;
pub mod terminals;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use c6_proto::{enrollment_server::EnrollmentServer, worker_hub_server::WorkerHubServer};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, Server, ServerTlsConfig};
use tracing::info;

pub use config::HeadConfig;

/// An error caused by the request, shown to the user as is.
#[derive(Debug)]
pub enum UserError {
    BadRequest(String),
    NotFound(String),
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserError::BadRequest(m) | UserError::NotFound(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for UserError {}

pub(crate) fn bad_request(msg: impl Into<String>) -> anyhow::Error {
    UserError::BadRequest(msg.into()).into()
}

pub(crate) fn not_found(msg: impl Into<String>) -> anyhow::Error {
    UserError::NotFound(msg.into()).into()
}

pub struct HeadState {
    pub store: store::Store,
    pub pki: pki::Pki,
    pub registry: registry::Registry,
    pub secrets: secrets::Secrets,
    pub hub: sessions::LiveHub,
    pub oauth: oauth::OAuth,
    pub terminals: terminals::Attaches,
    pub otlp: Option<telemetry::Otlp>,
    /// For OAuth token exchange and model listing.
    pub http: reqwest::Client,
    /// The UI port, for the OAuth loopback redirect.
    pub ui_port: u16,
    /// URL workers dial, as put into enrollment tokens.
    pub worker_url: String,
    pub shutdown: CancellationToken,
}

/// A running head.
pub struct Head {
    pub state: Arc<HeadState>,
    pub ui_addrs: Vec<SocketAddr>,
    pub worker_addrs: Vec<SocketAddr>,
    shutdown: CancellationToken,
    tasks: JoinSet<anyhow::Result<()>>,
}

impl Head {
    /// Waits until a server task fails or shutdown is requested.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        while let Some(r) = self.tasks.join_next().await {
            r??;
            if !self.shutdown.is_cancelled() {
                anyhow::bail!("server task exited unexpectedly");
            }
        }
        Ok(())
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.shutdown.cancel();
        self.wait().await
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }
}

/// Binds all listeners, prepares state and starts serving.
pub async fn start(cfg: HeadConfig) -> anyhow::Result<Head> {
    let ips = cfg.bind_ips()?;
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("creating {}", cfg.data_dir.display()))?;

    let ui_listeners = bind_all(&ips, cfg.ui_port).await.context("binding UI port")?;
    let worker_listeners = bind_all(&ips, cfg.worker_port).await.context("binding worker port")?;
    let ui_addrs: Vec<_> = ui_listeners.iter().map(|l| l.local_addr()).collect::<Result<_, _>>()?;
    let worker_addrs: Vec<_> = worker_listeners.iter().map(|l| l.local_addr()).collect::<Result<_, _>>()?;

    let (worker_url, cert_ips, dns_names) = endpoint_names(&cfg, &ips, &worker_addrs)?;

    let pki = pki::Pki::load_or_create(&cfg.data_dir.join("pki"), &cert_ips, &dns_names)?;
    let store = store::Store::open(&cfg.data_dir.join("c6h.db")).await?;
    let shutdown = CancellationToken::new();
    let registry = registry::Registry::new(shutdown.clone());
    let secrets = secrets::Secrets::load_or_create(&cfg.data_dir.join("master.key"))?;
    c6_llm::install_crypto_provider();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let state = Arc::new(HeadState {
        store,
        pki,
        registry,
        secrets,
        hub: Default::default(),
        oauth: Default::default(),
        terminals: Default::default(),
        otlp: telemetry::Otlp::from_config(cfg.otlp_endpoint.as_deref(), cfg.otlp_headers.clone()),
        http,
        ui_port: ui_addrs[0].port(),
        worker_url,
        shutdown: shutdown.clone(),
    });

    let mut tasks = JoinSet::new();

    let tls = ServerTlsConfig::new()
        .identity(state.pki.server_identity())
        .client_ca_root(Certificate::from_pem(state.pki.ca_cert_pem()))
        // Enrollment runs before the worker has a certificate; Attach checks
        // for one itself.
        .client_auth_optional(true);
    let incoming = futures::stream::select_all(worker_listeners.into_iter().map(TcpListenerStream::new));
    let grpc = Server::builder()
        .tls_config(tls)?
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
        .add_service(EnrollmentServer::new(hub::EnrollmentSvc(state.clone())))
        .add_service(
            WorkerHubServer::new(hub::HubSvc(state.clone()))
                .max_decoding_message_size(64 * 1024 * 1024)
                .max_encoding_message_size(64 * 1024 * 1024),
        );
    let token = shutdown.clone();
    tasks.spawn(async move { grpc.serve_with_incoming_shutdown(incoming, token.cancelled_owned()).await.context("worker port") });

    // Only answer to our own names (see guard.rs).
    let mut names = dns_names.clone();
    names.extend(cfg.extra_names.iter().cloned());
    // Literal loopback addresses can't be rebound, and are how a published
    // container port is usually reached.
    names.extend(["127.0.0.1".to_owned(), "[::1]".to_owned()]);
    let guard = guard::HostGuard::new(&ui_addrs, &names);
    let app = api::router(state.clone()).layer(axum::middleware::from_fn_with_state(guard, guard::check));
    for listener in ui_listeners {
        let app = app.clone();
        let token = shutdown.clone();
        tasks.spawn(async move { axum::serve(listener, app).with_graceful_shutdown(token.cancelled_owned()).await.context("UI port") });
    }

    info!(ui = ?ui_addrs, worker = ?worker_addrs, worker_url = %state.worker_url, "head started");
    Ok(Head { state, ui_addrs, worker_addrs, shutdown, tasks })
}

/// The URL workers dial, plus the IPs and DNS names the server certificate
/// must cover.
fn endpoint_names(cfg: &HeadConfig, ips: &[IpAddr], worker_addrs: &[SocketAddr]) -> anyhow::Result<(String, Vec<IpAddr>, Vec<String>)> {
    let advertise = cfg.advertise_host(worker_addrs);
    let worker_port = worker_addrs.iter().find(|a| advertise.contains(&a.ip().to_string())).unwrap_or(&worker_addrs[0]).port();
    let worker_url = format!("https://{advertise}:{worker_port}");

    let mut dns_names = vec!["localhost".to_owned()];
    dns_names.extend(gethostname::gethostname().into_string().ok());
    dns_names.extend(cfg.extra_names.iter().cloned());
    let advertise_bare = advertise.trim_start_matches('[').trim_end_matches(']');
    let mut cert_ips = ips.to_vec();
    match advertise_bare.parse::<IpAddr>() {
        Ok(ip) => cert_ips.push(ip),
        Err(_) => dns_names.push(advertise_bare.to_owned()),
    }
    cert_ips.push("127.0.0.1".parse()?);
    cert_ips.sort();
    cert_ips.dedup();
    dns_names.sort();
    dns_names.dedup();
    Ok((worker_url, cert_ips, dns_names))
}

const ENROLLMENT_TTL_MS: i64 = 60 * 60 * 1000;

pub async fn create_enrollment(
    store: &store::Store,
    ca_sha256: &str,
    worker_url: &str,
    name: Option<&str>,
) -> anyhow::Result<c6_api::Enrollment> {
    let secret = hex::encode(rand::random::<[u8; 32]>());
    let now = c6_core::now_ms();
    let expires_at = now + ENROLLMENT_TTL_MS;
    store.create_enrollment(&c6_core::sha256_hex(secret.as_bytes()), name, now, expires_at).await?;
    let token = c6_core::enrollment::EnrollmentToken { url: worker_url.to_owned(), ca_sha256: ca_sha256.to_owned(), secret };
    Ok(c6_api::Enrollment { token: token.encode(), expires_at })
}

/// Creates an enrollment without a running head (for `c6h enroll-token`).
/// Safe to run alongside a running head: SQLite WAL allows it.
pub async fn offline_enrollment(cfg: HeadConfig, name: Option<&str>) -> anyhow::Result<c6_api::Enrollment> {
    let ips = cfg.bind_ips()?;
    let worker_addrs: Vec<_> = ips.iter().map(|ip| SocketAddr::new(*ip, cfg.worker_port)).collect();
    let (worker_url, cert_ips, dns_names) = endpoint_names(&cfg, &ips, &worker_addrs)?;
    std::fs::create_dir_all(&cfg.data_dir)?;
    let pki = pki::Pki::load_or_create(&cfg.data_dir.join("pki"), &cert_ips, &dns_names)?;
    let store = store::Store::open(&cfg.data_dir.join("c6h.db")).await?;
    create_enrollment(&store, pki.ca_sha256(), &worker_url, name).await
}

async fn bind_all(ips: &[IpAddr], port: u16) -> anyhow::Result<Vec<TcpListener>> {
    let mut out = Vec::new();
    for ip in ips {
        let addr = SocketAddr::new(*ip, port);
        out.push(TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?);
    }
    Ok(out)
}
