//! The worker's connection to the head: attach, heartbeat, reconnect.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use c6_proto::{HeadMsg, Hello, Ping, WorkerMsg, head_msg, worker_hub_client::WorkerHubClient, worker_msg};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tonic::transport::Endpoint;
use tracing::{info, warn};

use crate::identity::Identity;

const PING_INTERVAL: Duration = Duration::from_secs(10);
/// No message from the head for this long means the connection is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that lasted this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Attached { worker_name: String, head_version: String },
    Disconnected { reason: String },
}

/// Why `attach_once` ended.
enum Ended {
    /// Retry with backoff.
    Retry(anyhow::Error),
    /// The head will not accept this worker until something changes (revoked,
    /// incompatible version). Stop.
    Fatal(anyhow::Error),
    Shutdown,
}

/// Keeps the worker attached to the head until `shutdown` or a fatal
/// rejection.
pub async fn run(identity: Identity, state: watch::Sender<ConnState>, shutdown: CancellationToken) -> anyhow::Result<()> {
    let mut backoff = MIN_BACKOFF;
    loop {
        state.send_replace(ConnState::Connecting);
        let started = Instant::now();
        let err = match attach_once(&identity, &state, &shutdown).await {
            Ended::Shutdown => return Ok(()),
            Ended::Fatal(e) => {
                state.send_replace(ConnState::Disconnected { reason: format!("{e:#}") });
                return Err(e);
            }
            Ended::Retry(e) => e,
        };
        if started.elapsed() >= STABLE_AFTER {
            backoff = MIN_BACKOFF;
        }
        let delay = jitter(backoff);
        warn!(error = %format!("{err:#}"), retry_in = ?delay, "disconnected from head");
        state.send_replace(ConnState::Disconnected { reason: format!("{err:#}") });
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn attach_once(identity: &Identity, state: &watch::Sender<ConnState>, shutdown: &CancellationToken) -> Ended {
    let endpoint = match Endpoint::from_shared(identity.meta.head_url.clone()).and_then(|e| e.tls_config(identity.tls_config())) {
        Ok(e) => e
            .connect_timeout(Duration::from_secs(10))
            .http2_keep_alive_interval(Duration::from_secs(20))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true),
        Err(e) => return Ended::Fatal(anyhow::Error::new(e).context("invalid head URL")),
    };
    let channel = tokio::select! {
        _ = shutdown.cancelled() => return Ended::Shutdown,
        r = endpoint.connect() => match r {
            Ok(c) => c,
            Err(e) => return Ended::Retry(anyhow::Error::new(e).context(format!("connecting to {}", identity.meta.head_url))),
        },
    };

    let (tx, rx) = mpsc::channel::<WorkerMsg>(64);
    let hello = Hello {
        protocol_version: c6_core::PROTOCOL_VERSION,
        version: c6_core::VERSION.into(),
        hostname: crate::hostname(),
        os: c6_core::host_os().into(),
        arch: c6_core::host_arch().into(),
        capabilities: Vec::new(),
    };
    tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Hello(hello)) }).await.ok();

    let mut inbound = match WorkerHubClient::new(channel).attach(ReceiverStream::new(rx)).await {
        Ok(r) => r.into_inner(),
        Err(s) => return classify(s),
    };

    match tokio::time::timeout(READ_TIMEOUT, inbound.message()).await {
        Ok(Ok(Some(HeadMsg { msg: Some(head_msg::Msg::Welcome(w)) }))) => {
            info!(worker = %w.worker_id, name = %w.worker_name, head = %w.head_version, "attached to head");
            state.send_replace(ConnState::Attached { worker_name: w.worker_name, head_version: w.head_version });
        }
        Ok(Err(s)) => return classify(s),
        Err(_) => return Ended::Retry(anyhow::anyhow!("timed out waiting for Welcome")),
        _ => return Ended::Retry(anyhow::anyhow!("expected Welcome from head")),
    }

    let mut pings = tokio::time::interval(PING_INTERVAL);
    let mut nonce = 0u64;
    let mut in_flight: HashMap<u64, Instant> = HashMap::new();
    let mut last_rtt_us = 0u32;
    let mut deadline = Instant::now() + READ_TIMEOUT;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ended::Shutdown,
            _ = tokio::time::sleep_until(deadline.into()) => return Ended::Retry(anyhow::anyhow!("head unresponsive")),
            _ = pings.tick() => {
                nonce += 1;
                in_flight.insert(nonce, Instant::now());
                // Bound the map if pongs go missing.
                in_flight.retain(|n, _| nonce - n < 16);
                if tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Ping(Ping { nonce, last_rtt_us })) }).await.is_err() {
                    return Ended::Retry(anyhow::anyhow!("outbound stream closed"));
                }
            }
            msg = inbound.message() => match msg {
                Ok(Some(HeadMsg { msg: Some(m) })) => {
                    deadline = Instant::now() + READ_TIMEOUT;
                    match m {
                        head_msg::Msg::Pong(p) => {
                            if let Some(sent) = in_flight.remove(&p.nonce) {
                                last_rtt_us = sent.elapsed().as_micros().min(u32::MAX as u128) as u32;
                            }
                        }
                        head_msg::Msg::Welcome(_) => warn!("unexpected second Welcome"),
                    }
                }
                Ok(Some(HeadMsg { msg: None })) => deadline = Instant::now() + READ_TIMEOUT, // newer head
                Ok(None) => return Ended::Retry(anyhow::anyhow!("head closed the connection")),
                Err(s) => return classify(s),
            },
        }
    }
}

fn classify(s: tonic::Status) -> Ended {
    let err = anyhow::anyhow!("{}: {}", s.code(), s.message());
    match s.code() {
        Code::PermissionDenied | Code::Unauthenticated => {
            Ended::Fatal(err.context("head rejected this worker; re-enroll with `c6w enroll --force <token>`"))
        }
        Code::FailedPrecondition => Ended::Fatal(err),
        _ => Ended::Retry(err),
    }
}

fn jitter(d: Duration) -> Duration {
    d.mul_f64(rand::random_range(0.5..1.0))
}
