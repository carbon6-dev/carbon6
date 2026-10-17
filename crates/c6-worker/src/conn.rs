//! The worker's connection to the head: attach, heartbeat, event upload,
//! command delivery, reconnect.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use c6_proto::{
    CommandRejected, HeadMsg, Hello, Ping, RpcResponse, SessionCursor, SessionEventBatch, WorkerMsg, head_msg,
    worker_hub_client::WorkerHubClient, worker_msg,
};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tonic::transport::Endpoint;
use tracing::{info, warn};

use crate::Worker;
use crate::journal::Journal;

const PING_INTERVAL: Duration = Duration::from_secs(10);
/// No message from the head for this long means the connection is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that lasted this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Events per upload message.
const UPLOAD_BATCH: u32 = 256;
/// Largest message either way (a `Restore` carries a whole session log).
pub(crate) const MAX_MESSAGE: usize = 64 * 1024 * 1024;

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
pub async fn run(worker: Arc<Worker>, state: watch::Sender<ConnState>, shutdown: CancellationToken) -> anyhow::Result<()> {
    // An update in progress counts this start (docs/updates.md).
    crate::update::on_start(&worker, &shutdown)?;
    // Sessions from Claude Code and Codex, when the head says this worker imports.
    crate::imports::spawn(worker.clone(), shutdown.clone());
    let mut backoff = MIN_BACKOFF;
    loop {
        state.send_replace(ConnState::Connecting);
        let started = Instant::now();
        let ended = attach_once(&worker, &state, &shutdown).await;
        worker.link.disconnected();
        let err = match ended {
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

async fn attach_once(worker: &Arc<Worker>, state: &watch::Sender<ConnState>, shutdown: &CancellationToken) -> Ended {
    let identity = &worker.identity;
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
    let kits = c6_tools::kits::runtime().await.is_some();
    let hello = Hello {
        protocol_version: c6_core::PROTOCOL_VERSION,
        version: c6_core::VERSION.into(),
        hostname: crate::hostname(),
        os: c6_core::host_os().into(),
        arch: c6_core::host_arch().into(),
        capabilities: {
            let mut c: Vec<String> = crate::claude_code::find_claude().map(|_| "claude_code".to_owned()).into_iter().collect();
            // It runs kit jobs for the head (docs/knowledge.md §6.2).
            if kits {
                c.push("kits".into());
            }
            if let Some(ext) = worker.extension() {
                c.extend(ext.capabilities());
            }
            c
        },
        sessions: match worker.journal.unacked().await {
            Ok(cursors) => cursors.into_iter().map(|c| SessionCursor { session_id: c.session_id, seq: c.last_seq }).collect(),
            Err(e) => return Ended::Retry(e.context("reading journal")),
        },
    };
    tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Hello(hello)) }).await.ok();

    let client = WorkerHubClient::new(channel.clone()).max_decoding_message_size(MAX_MESSAGE).max_encoding_message_size(MAX_MESSAGE);
    let mut inbound = match client.clone().attach(ReceiverStream::new(rx)).await {
        Ok(r) => r.into_inner(),
        Err(s) => return classify(s),
    };

    match tokio::time::timeout(READ_TIMEOUT, inbound.message()).await {
        Ok(Ok(Some(HeadMsg { msg: Some(head_msg::Msg::Welcome(w)) }))) => {
            info!(worker = %w.worker_id, name = %w.worker_name, head = %w.head_version, "attached to head");
            for ack in &w.acks {
                if let Err(e) = worker.journal.set_acked(&ack.session_id, ack.seq).await {
                    return Ended::Retry(e.context("recording acks"));
                }
            }
            state.send_replace(ConnState::Attached { worker_name: w.worker_name, head_version: w.head_version });
        }
        Ok(Err(s)) => return classify(s),
        Err(_) => return Ended::Retry(anyhow::anyhow!("timed out waiting for Welcome")),
        _ => return Ended::Retry(anyhow::anyhow!("expected Welcome from head")),
    }

    worker.link.connected(tx.clone());
    worker.set_channel(Some(channel));
    crate::update::attached(worker);
    let _clear_channel = ClearChannel(worker.clone());
    let uploads = CancellationToken::new();
    let _stop_uploads = uploads.clone().drop_guard();
    // A separate task so a slow uplink never stops us reading acks and commands.
    tokio::spawn(upload(worker.journal.clone(), tx.clone(), uploads.clone()));
    // Commands are applied in order by their own task, so the read loop
    // keeps going while one waits on the head (e.g. for a credential, whose
    // reply arrives on this very stream).
    let (commands, mut queue) = mpsc::unbounded_channel::<c6_proto::SessionCommand>();
    {
        let (worker, tx, stop) = (worker.clone(), tx.clone(), uploads.clone());
        tokio::spawn(async move {
            loop {
                let cmd = tokio::select! {
                    _ = stop.cancelled() => return,
                    c = queue.recv() => match c {
                        Some(c) => c,
                        None => return,
                    },
                };
                let (command_id, session_id) = (cmd.command_id.clone(), cmd.session_id.clone());
                if let Err(reason) = worker.sessions.handle(cmd).await {
                    warn!(session = %session_id, %reason, "command rejected");
                    let rejected = CommandRejected { command_id, session_id, reason };
                    if tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Rejected(rejected)) }).await.is_err() {
                        return;
                    }
                }
            }
        });
    }

    let mut pings = tokio::time::interval(PING_INTERVAL);
    // Docker or Podman started after we attached: attach again, saying so.
    let mut look_for_kits = tokio::time::interval(Duration::from_secs(60));
    look_for_kits.tick().await;
    let mut nonce = 0u64;
    let mut in_flight: HashMap<u64, Instant> = HashMap::new();
    let mut last_rtt_us = 0u32;
    let mut deadline = Instant::now() + READ_TIMEOUT;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ended::Shutdown,
            _ = tokio::time::sleep_until(deadline.into()) => return Ended::Retry(anyhow::anyhow!("head unresponsive")),
            _ = look_for_kits.tick(), if !kits => {
                if c6_tools::kits::runtime().await.is_some() {
                    return Ended::Retry(anyhow::anyhow!("a container runtime is here now: attaching again to say so"));
                }
            }
            _ = pings.tick() => {
                nonce += 1;
                in_flight.insert(nonce, Instant::now());
                // Bound the map if pongs go missing.
                in_flight.retain(|n, _| nonce - n < 16);
                // Never block the read loop on a ping; the read deadline
                // catches a dead connection.
                if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(WorkerMsg { msg: Some(worker_msg::Msg::Ping(Ping { nonce, last_rtt_us })) }) {
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
                        head_msg::Msg::Ack(a) => {
                            if let Err(e) = worker.journal.set_acked(&a.session_id, a.seq).await {
                                warn!("recording ack: {e:#}");
                            }
                        }
                        head_msg::Msg::Command(cmd) => {
                            if commands.send(cmd).is_err() {
                                return Ended::Retry(anyhow::anyhow!("command queue closed"));
                            }
                        }
                        head_msg::Msg::Credential(grant) => worker.link.on_grant(grant),
                        head_msg::Msg::HeadCallReply(reply) => worker.link.on_call_reply(reply),
                        head_msg::Msg::RpcRequest(req) => {
                            // Off the read loop: requests may take a while (git, big files).
                            let (w, tx) = (worker.clone(), tx.clone());
                            tokio::spawn(async move {
                                let params = serde_json::from_slice(&req.params_json).unwrap_or(serde_json::Value::Null);
                                let resp = match crate::rpc::handle(&w, &req.method, params).await {
                                    Ok(v) => RpcResponse { id: req.id, result_json: serde_json::to_vec(&v).unwrap_or_default(), error: String::new() },
                                    Err(e) => RpcResponse { id: req.id, result_json: Vec::new(), error: format!("{e:#}") },
                                };
                                let _ = tx.send(WorkerMsg { msg: Some(worker_msg::Msg::RpcResponse(resp)) }).await;
                            });
                        }
                    }
                }
                Ok(Some(HeadMsg { msg: None })) => deadline = Instant::now() + READ_TIMEOUT, // newer head
                Ok(None) => return Ended::Retry(anyhow::anyhow!("head closed the connection")),
                Err(s) => return classify(s),
            },
        }
    }
}

/// Streams journaled events the head has not acknowledged, in seq order per
/// session, starting from the head's acked position.
async fn upload(journal: Arc<Journal>, tx: mpsc::Sender<WorkerMsg>, stop: CancellationToken) {
    let mut sent: HashMap<String, u64> = HashMap::new();
    loop {
        let mut progressed = false;
        let cursors = match journal.unacked().await {
            Ok(c) => c,
            Err(e) => {
                warn!("reading journal: {e:#}");
                Vec::new()
            }
        };
        for c in cursors {
            let from = sent.get(&c.session_id).copied().unwrap_or(0).max(c.acked_seq);
            if c.last_seq <= from {
                continue;
            }
            let events = match journal.events_after(&c.session_id, from, UPLOAD_BATCH).await {
                Ok(e) if !e.is_empty() => e,
                Ok(_) => continue,
                Err(e) => {
                    warn!("reading journal: {e:#}");
                    continue;
                }
            };
            let last = events.last().map(|e| e.seq).unwrap_or(from);
            let batch = SessionEventBatch { session_id: c.session_id.clone(), events };
            tokio::select! {
                _ = stop.cancelled() => return,
                r = tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Events(batch)) }) => if r.is_err() { return },
            }
            sent.insert(c.session_id, last);
            progressed = true;
        }
        if !progressed {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = journal.appended.notified() => {}
            }
        }
    }
}

struct ClearChannel(Arc<Worker>);

impl Drop for ClearChannel {
    fn drop(&mut self) {
        self.0.set_channel(None);
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
