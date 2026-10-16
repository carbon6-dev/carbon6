//! The carbon6 head (`c6h`): system of record, worker hub and web UI server.

pub mod api;
pub mod apps;
pub mod assistant;
pub mod auth;
pub mod catalog;
pub mod claims;
pub mod collections;
pub mod commons;
pub mod config;
mod convert;
mod cron;
pub mod decisions;
pub mod explorer;
mod export;
pub mod fabric;
pub mod files;
pub mod google;
pub mod grants;
pub mod guard;
pub mod hosts;
pub mod hub;
pub mod instances;
pub mod kitjobs;
pub mod knowledge;
pub mod lists;
pub mod memex;
pub mod memory;
pub mod notify;
pub mod oauth;
pub mod operations;
pub mod packages;
pub mod pki;
pub mod prompts;
pub mod providers;
pub mod push;
pub mod realms;
pub mod registry;
pub mod schema;
pub mod search;
pub mod secrets;
pub mod series;
pub mod sessions;
pub mod store;
pub mod telemetry;
pub mod templates;
pub mod terminals;
pub mod tls;
pub mod triggers;
pub mod ui_ca;
pub mod usage;
pub mod views;
mod webauthn;
pub mod workflows;

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
    /// Not allowed for this caller (a realm-scoped token).
    Forbidden(String),
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserError::BadRequest(m) | UserError::NotFound(m) | UserError::Forbidden(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for UserError {}

pub(crate) fn bad_request(msg: impl Into<String>) -> anyhow::Error {
    UserError::BadRequest(msg.into()).into()
}

pub(crate) fn forbidden(msg: impl Into<String>) -> anyhow::Error {
    UserError::Forbidden(msg.into()).into()
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
    pub auth: auth::Auth,
    /// The web UI's certificate authority, when the head issues its own
    /// HTTPS certificate (served at /ca.pem for devices to trust).
    pub ui_ca: Option<Arc<ui_ca::UiCa>>,
    pub otlp: Option<telemetry::Otlp>,
    /// For OAuth token exchange and model listing.
    pub http: reqwest::Client,
    /// The UI port, for the OAuth loopback redirect.
    pub ui_port: u16,
    /// URL workers dial, as put into enrollment tokens.
    pub worker_url: String,
    /// Where the web UI and API are reached (set once listening).
    pub ui_urls: std::sync::OnceLock<Vec<String>>,
    /// Notifications to your channels.
    pub notifier: notify::Notifier,
    /// Workflow runs to drive.
    pub workflows: workflows::Runner,
    /// Wakes what sends the memex its updates.
    pub memex_feed: memex::Feed,
    /// The assistant: its conversations run in the head.
    pub assistant: assistant::Assistant,
    pub shutdown: CancellationToken,
    /// Its database, PKI and keys (never offered by `files`).
    pub data_dir: std::path::PathBuf,
    /// The knowledge graph and its store (docs/knowledge.md), in this process.
    pub memex: std::sync::OnceLock<Arc<c6_memex::Memex>>,
}

/// A running head.
pub struct Head {
    pub state: Arc<HeadState>,
    pub ui_addrs: Vec<SocketAddr>,
    pub worker_addrs: Vec<SocketAddr>,
    /// Where to open the web UI.
    pub ui_urls: Vec<String>,
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
pub async fn start(mut cfg: HeadConfig) -> anyhow::Result<Head> {
    cfg.extra_names.extend(config::tailnet_names().await);
    let ips = cfg.bind_ips()?;
    c6_core::create_private_dir(&cfg.data_dir).with_context(|| format!("creating {}", cfg.data_dir.display()))?;

    let ui_listeners = bind_all(&ips, cfg.ui_port).await.context("binding UI port")?;
    let worker_listeners = bind_all(&ips, cfg.worker_port).await.context("binding worker port")?;
    let ui_addrs: Vec<_> = ui_listeners.iter().map(|l| l.local_addr()).collect::<Result<_, _>>()?;
    let worker_addrs: Vec<_> = worker_listeners.iter().map(|l| l.local_addr()).collect::<Result<_, _>>()?;

    // A wildcard bind (0.0.0.0) is reached at the interface addresses: those
    // go into the certificate, the advertised URL and the Host allowlist.
    let ui_reach = config::reachable_addrs(&ui_addrs)?;
    let worker_reach = config::reachable_addrs(&worker_addrs)?;
    let reach_ips: Vec<IpAddr> = worker_reach.iter().map(|a| a.ip()).collect();
    let (worker_url, cert_ips, dns_names) = endpoint_names(&cfg, &reach_ips, &worker_reach)?;

    let pki = pki::Pki::load_or_create(&cfg.data_dir.join("pki"), &cert_ips, &dns_names)?;

    // Only answer to our own names (see guard.rs).
    let mut names = dns_names.clone();
    names.extend(cfg.extra_names.iter().cloned());
    // Literal loopback addresses can't be rebound, and are how a published
    // container port is usually reached.
    names.extend(["127.0.0.1".to_owned(), "[::1]".to_owned()]);
    let ui_ips: Vec<IpAddr> = ui_reach.iter().map(|a| a.ip()).collect();
    let ui_tls = tls::UiTls::setup(&cfg, &names, &ui_ips)?.map(Arc::new);
    if let Some(t) = &ui_tls {
        names.push(t.name.clone());
    }
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
        auth: auth::Auth::new(cfg.auth),
        ui_ca: ui_tls.as_ref().and_then(|t| t.ca.clone()),
        otlp: telemetry::Otlp::from_config(cfg.otlp_endpoint.as_deref(), cfg.otlp_headers.clone()),
        http,
        ui_port: ui_addrs[0].port(),
        worker_url,
        ui_urls: Default::default(),
        notifier: Default::default(),
        workflows: Default::default(),
        memex_feed: Default::default(),
        assistant: Default::default(),
        shutdown: shutdown.clone(),
        data_dir: cfg.data_dir.clone(),
        memex: Default::default(),
    });

    let mut tasks = JoinSet::new();

    // The knowledge graph is the head's (docs/knowledge.md).
    knowledge::open(&state, cfg.memex_dirs.as_deref().unwrap_or_default()).await?;

    // Apps installed before instances read their code from their package
    // from now on (docs/apps.md §14).
    if let Err(e) = apps::adopt(&state).await {
        tracing::warn!("adopting apps installed before instances: {e:#}");
    }
    // And each is offered a move out of the realm it was given.
    if let Err(e) = apps::offer_moves(&state).await {
        tracing::warn!("offering to move apps out of their own realms: {e:#}");
    }

    // Scheduled automations.
    triggers::spawn_scheduler(state.clone());
    // Series thresholds and retention.
    series::spawn_sweeper(state.clone());
    notify::spawn(state.clone());
    usage::spawn_backfill(state.clone());
    hosts::spawn_checks(state.clone());
    workflows::spawn(state.clone());
    memex::spawn(state.clone());
    decisions::spawn_sweeper(state.clone());
    {
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = decisions::reconcile(&st).await {
                tracing::warn!("opening decisions for what waits: {e:#}");
            }
        });
    }
    assistant::start(state.clone(), api::router(state.clone())).await?;

    // Build the search index in the background (first start, or new format).
    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = search::backfill(&st.store).await {
            tracing::warn!("building the search index: {e:#}");
        }
    });

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

    if let Some(t) = &ui_tls {
        t.spawn_refresh(shutdown.clone());
    }
    let guard = guard::HostGuard::new(&ui_reach, &names);
    let app = api::router(state.clone()).layer(axum::middleware::from_fn_with_state(guard, guard::check));
    for listener in ui_listeners {
        let (app, token) = (app.clone(), shutdown.clone());
        let tls = match &ui_tls {
            Some(t) => Some((t.acceptor()?, t.name.clone())),
            None => None,
        };
        tasks.spawn(async move { tls::serve(listener, app, tls, token).await.context("UI port") });
    }
    let port = ui_addrs[0].port();
    let ui_urls = match &ui_tls {
        Some(t) => vec![format!("https://{}:{port}", t.name)],
        None => {
            // localhost (where passkeys work over HTTP) when reachable there,
            // and the tailnet name.
            let mut urls = Vec::new();
            if ui_reach.iter().any(|a| a.ip().is_loopback()) {
                urls.push(format!("http://localhost:{port}"));
            }
            urls.extend(cfg.extra_names.iter().find(|n| n.contains('.')).map(|n| format!("http://{n}:{port}")));
            if urls.is_empty() {
                urls = ui_addrs.iter().map(|a| format!("http://{a}")).collect();
            }
            urls
        }
    };

    if ui_tls.is_none()
        && let Some(a) = ui_addrs.iter().find(|a| !config::is_loopback_or_tailnet(&a.ip()))
    {
        tracing::warn!(
            addr = %a,
            "the web UI is plain HTTP on {}, so sign-in links and session cookies cross that network unencrypted; \
             set ui_tls_head_ca = true (or ui_tls_cert and ui_tls_key) in c6h.toml",
            a.ip()
        );
    }
    let _ = state.ui_urls.set(ui_urls.clone());
    info!(ui = ?ui_urls, worker = ?worker_addrs, worker_url = %state.worker_url, auth = cfg.auth, "head started");
    Ok(Head { state, ui_addrs, worker_addrs, ui_urls, shutdown, tasks })
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
/// A one-time login link, made without the head running (or beside it):
/// one URL per way to reach the UI.
pub async fn offline_login_link(cfg: HeadConfig) -> anyhow::Result<Vec<String>> {
    c6_core::create_private_dir(&cfg.data_dir)?;
    let store = store::Store::open(&cfg.data_dir.join("c6h.db")).await?;
    let token = store.create_login_link().await?;
    let port = cfg.ui_port;
    let tailnet = config::tailnet_names().await;
    let bases: Vec<String> = if let (Some(cert), Some(_)) = (&cfg.ui_tls_cert, &cfg.ui_tls_key) {
        vec![format!("https://{}:{port}", tls::cert_name(cert)?)]
    } else if cfg.ui_tls_head_ca {
        let mut cfg = cfg.clone();
        cfg.extra_names.extend(tailnet);
        vec![format!("https://{}:{port}", tls::head_ca_name(&cfg))]
    } else {
        std::iter::once("localhost".to_owned()).chain(tailnet.into_iter().take(1)).map(|h| format!("http://{h}:{port}")).collect()
    };
    Ok(bases.into_iter().map(|b| format!("{b}/login/{token}")).collect())
}

/// A new API token, minted on the head machine (`c6h api-token`).
pub async fn offline_api_token(cfg: HeadConfig, name: &str, realm: Option<&str>) -> anyhow::Result<String> {
    c6_core::create_private_dir(&cfg.data_dir)?;
    let store = store::Store::open(&cfg.data_dir.join("c6h.db")).await?;
    if let Some(r) = realm {
        anyhow::ensure!(r != realms::ROOT, "a token for root is one with no realm: leave --realm out");
        let known: Option<String> = sqlx::query_scalar("SELECT id FROM realms WHERE id = ?").bind(r).fetch_optional(&store.pool).await?;
        anyhow::ensure!(known.is_some(), "no realm {r:?}");
    }
    Ok(store.create_api_token(name, realm).await?.token.unwrap_or_default())
}

pub async fn offline_enrollment(mut cfg: HeadConfig, name: Option<&str>) -> anyhow::Result<c6_api::Enrollment> {
    cfg.extra_names.extend(config::tailnet_names().await);
    let ips = cfg.bind_ips()?;
    let worker_addrs: Vec<_> = ips.iter().map(|ip| SocketAddr::new(*ip, cfg.worker_port)).collect();
    let worker_addrs = config::reachable_addrs(&worker_addrs)?;
    let ips: Vec<IpAddr> = worker_addrs.iter().map(|a| a.ip()).collect();
    let (worker_url, cert_ips, dns_names) = endpoint_names(&cfg, &ips, &worker_addrs)?;
    c6_core::create_private_dir(&cfg.data_dir)?;
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

/// `s` cut to at most `n` characters (with an ellipsis).
pub(crate) fn util_clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}
