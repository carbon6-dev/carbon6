//! The worker's connection to the head: attach, heartbeat, event upload,
//! command delivery, reconnect.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use c6_proto::{
    CommandRejected, HeadMsg, Hello, Ping, RpcResponse, SessionCursor, SessionEvent, SessionEventBatch, WorkerMsg, head_msg,
    session_command, worker_hub_client::WorkerHubClient, worker_msg,
};
use prost::Message;
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
/// Encoded bytes per upload message, including the `WorkerMsg` envelope.
const UPLOAD_BYTES: usize = 32 * 1024 * 1024;
/// Largest message either way (a `Restore` carries a whole session log).
pub(crate) const MAX_MESSAGE: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Attached { worker_name: String, head_version: String },
    Disconnected { reason: String },
}

/// How often what kit runs left on the container runtime is looked for.
const SWEEP: Duration = Duration::from_secs(10 * 60);

/// Removes what this worker's kit runs left on the container runtime and
/// no run owns (c6_tools::kits::sweep): at start, then every few minutes.
fn sweep_kits(shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            if let Some(rt) = c6_tools::kits::runtime().await {
                let n = c6_tools::kits::sweep(&rt).await;
                if n > 0 {
                    info!(n, "removed what kit runs left on the container runtime");
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(SWEEP) => {}
            }
        }
    });
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
    crate::codex_limits::spawn(worker.clone(), shutdown.clone());
    sweep_kits(shutdown.clone());
    let mut backoff = MIN_BACKOFF;
    loop {
        state.send_replace(ConnState::Connecting);
        let started = Instant::now();
        let ended = attach_once(&worker, &state, &shutdown).await;
        if !matches!(&ended, Ended::Retry(_)) {
            worker.link.stop_credentials();
        }
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
    // It runs kit jobs for the head (docs/knowledge.md §6.2), unless told not to.
    let offers_kits = worker.runs_kit_jobs();
    let kits = offers_kits && c6_tools::kits::runtime().await.is_some();
    let mut capabilities = crate::capabilities(worker, crate::claude_code::find_claude().is_some(), kits);
    capabilities.push(c6_proto::SESSION_MOVE_CAPABILITY.into());
    capabilities.push(c6_proto::SESSION_MOVE_TARGET_CAPABILITY.into());
    capabilities.push("notice_settings_v1".into());
    capabilities.push(c6_proto::SESSION_SPEC_REFRESH_CAPABILITY.into());
    let hello = Hello {
        protocol_version: c6_core::PROTOCOL_VERSION,
        version: c6_core::build().into(),
        hostname: crate::hostname(),
        os: c6_core::host_os().into(),
        arch: c6_core::host_arch().into(),
        capabilities,
        sessions: match worker.journal.unacked().await {
            Ok(cursors) => {
                cursors.into_iter().map(|c| SessionCursor { session_id: c.session_id, seq: c.last_seq, ownership_epoch: c.epoch }).collect()
            }
            Err(e) => return Ended::Retry(e.context("reading journal")),
        },
    };
    // Fence queued automatic work on reconnect too, not just Worker::open.
    // Active turns/native work continue; new dispatch waits for this attach.
    let spec_generation = worker.sessions.await_initial_specs();
    tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Hello(hello)) }).await.ok();

    let client = WorkerHubClient::new(channel.clone()).max_decoding_message_size(MAX_MESSAGE).max_encoding_message_size(MAX_MESSAGE);
    let mut inbound = match client.clone().attach(ReceiverStream::new(rx)).await {
        Ok(r) => r.into_inner(),
        Err(s) => return classify(s),
    };

    match tokio::time::timeout(READ_TIMEOUT, inbound.message()).await {
        Ok(Ok(Some(HeadMsg { msg: Some(head_msg::Msg::Welcome(w)) }))) => {
            info!(worker = %w.worker_id, name = %w.worker_name, head = %w.head_version, "attached to head");
            worker.machine_sample_secs.store(w.machine_sample_secs.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
            let warning = w.machine_disk_warn_percent.filter(|n| n.is_finite() && (0.0..=100.0).contains(n)).unwrap_or(10.0);
            worker.machine_disk_warn_percent.store(warning.to_bits(), std::sync::atomic::Ordering::Relaxed);
            worker.sessions.launch_pressure.configure_disk(w.machine_disk_hold_percent);
            if !w.session_spec_refresh {
                worker.sessions.specs_ready_for(spec_generation);
            }
            for ack in &w.acks {
                if let Err(e) = worker.journal.set_acked_owned(&ack.session_id, ack.seq, ack.ownership_epoch).await {
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
    tokio::spawn(crate::machines::run(worker.clone(), tx.clone(), uploads.clone()));
    let requests = dispatch_rpcs(worker.clone(), tx.clone(), uploads.clone());
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
                let ownership_epoch = cmd.ownership_epoch;
                // The manager can recover a lost cache via Restore before checking
                // ownership. No other command can install a new incarnation.
                if !matches!(&cmd.kind, Some(session_command::Kind::Restore(_) | session_command::Kind::SpecsReady(_)))
                    && let Err(reason) = worker.journal.check_ownership(&session_id, ownership_epoch).await
                {
                    // A lost cache has neither an ownership row nor a session.
                    // Let the head recover its current owner as for legacy cache loss.
                    let reason = if ownership_epoch != 0
                        && matches!(&cmd.kind, Some(session_command::Kind::Input(_)))
                        && matches!(worker.journal.ownership(&session_id).await, Ok((0, false, _)))
                        && matches!(worker.journal.has_session(&session_id).await, Ok(false))
                    {
                        "unknown session on this worker".into()
                    } else {
                        format!("{reason:#}")
                    };
                    if reject_command(&tx, command_id, session_id, ownership_epoch, reason).await.is_err() {
                        return;
                    }
                    continue;
                }
                let result = match cmd.kind {
                    Some(session_command::Kind::RefreshSpec(spec)) => {
                        worker.sessions.refresh_spec_for(&session_id, spec, spec_generation).await
                    }
                    Some(session_command::Kind::SpecsReady(_)) => {
                        // An old connection's command task cannot release a
                        // newer attach's generation while it is shutting down.
                        worker.sessions.specs_ready_for(spec_generation);
                        Ok(())
                    }
                    Some(session_command::Kind::ChangePlace(place)) => {
                        // Put the move in the mailbox before dispatching the next command,
                        // but let the actor finish slow workspace preparation on its own.
                        match worker.sessions.enqueue_change_place(&session_id, &command_id, place).await {
                            Ok(Some(result)) => {
                                let (tx, stop) = (tx.clone(), stop.clone());
                                tokio::spawn(async move {
                                    tokio::select! {
                                        _ = stop.cancelled() => {},
                                        _ = async {
                                            if let Err(reason) = result.await {
                                                let _ = reject_command(&tx, command_id, session_id, ownership_epoch, reason).await;
                                            }
                                        } => {},
                                    }
                                });
                                continue;
                            }
                            Ok(None) => continue, // already applied; the journal event is its ack
                            Err(reason) => Err(reason),
                        }
                    }
                    _ => worker.sessions.handle(cmd).await,
                };
                if let Err(reason) = result
                    && reject_command(&tx, command_id, session_id, ownership_epoch, reason).await.is_err()
                {
                    return;
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
            _ = look_for_kits.tick(), if offers_kits && !kits => {
                if c6_tools::kits::runtime().await.is_some() {
                    return Ended::Retry(anyhow::anyhow!("a container runtime is here now: attaching again to say so"));
                }
            }
            _ = pings.tick() => {
                worker.sessions.report_disk_pressure();
                nonce += 1;
                in_flight.insert(nonce, Instant::now());
                // Bound the map if pongs go missing.
                in_flight.retain(|n, _| nonce - n < 16);
                // Never block the read loop on a ping; the read deadline
                // catches a dead connection.
                let liveness = Some(worker.sessions.liveness_report());
                if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(WorkerMsg { msg: Some(worker_msg::Msg::Ping(Ping { nonce, last_rtt_us, liveness })) }) {
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
                            if let Err(e) = worker.journal.set_acked_owned(&a.session_id, a.seq, a.ownership_epoch).await {
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
                            if requests.send(req).is_err() {
                                return Ended::Retry(anyhow::anyhow!("RPC queue closed"));
                            }
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

/// Prepare in wire order, off the ACK read loop (JSON inputs can be large).
/// Kit registration must precede a following cancellation, even before its
/// independently running execution future is first polled.
fn dispatch_rpcs(worker: Arc<Worker>, tx: mpsc::Sender<WorkerMsg>, stop: CancellationToken) -> mpsc::UnboundedSender<c6_proto::RpcRequest> {
    let (requests, mut queue) = mpsc::unbounded_channel::<c6_proto::RpcRequest>();
    tokio::spawn(async move {
        loop {
            let req = tokio::select! {
                _ = stop.cancelled() => return,
                request = queue.recv() => match request { Some(request) => request, None => return },
            };
            let mut params = serde_json::from_slice(&req.params_json).unwrap_or(serde_json::Value::Null);
            let job = (req.method == "kit.job").then(|| crate::rpc::kit_job(worker.clone(), params.take()));
            let (worker, tx, stop) = (worker.clone(), tx.clone(), stop.clone());
            tokio::spawn(async move {
                let result = if let Some(job) = job {
                    tokio::select! {
                        biased;
                        _ = stop.cancelled() => return,
                        result = job => result,
                    }
                } else {
                    crate::rpc::handle(&worker, &req.method, params).await
                };
                let resp = match result {
                    Ok(value) => {
                        RpcResponse { id: req.id, result_json: serde_json::to_vec(&value).unwrap_or_default(), error: String::new() }
                    }
                    Err(error) => RpcResponse { id: req.id, result_json: Vec::new(), error: format!("{error:#}") },
                };
                let _ = tx.send(WorkerMsg { msg: Some(worker_msg::Msg::RpcResponse(resp)) }).await;
            });
        }
    });
    requests
}

async fn reject_command(
    tx: &mpsc::Sender<WorkerMsg>,
    command_id: String,
    session_id: String,
    ownership_epoch: u64,
    reason: String,
) -> Result<(), mpsc::error::SendError<WorkerMsg>> {
    warn!(session = %session_id, %reason, "command rejected");
    let rejected = CommandRejected { command_id, session_id, reason, ownership_epoch };
    tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Rejected(rejected)) }).await
}

/// Streams journaled events the head has not acknowledged, in seq order per
/// session, starting from the head's acked position.
async fn upload(journal: Arc<Journal>, tx: mpsc::Sender<WorkerMsg>, stop: CancellationToken) {
    let mut sent: HashMap<String, (u64, u64)> = HashMap::new(); // (epoch, seq)
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
            let from = match sent.get(&c.session_id) {
                Some(&(epoch, seq)) if epoch == c.epoch => seq.max(c.acked_seq),
                _ => c.acked_seq,
            };
            if c.last_seq <= from {
                continue;
            }
            let events = match journal.events_after_owned(&c.session_id, from, UPLOAD_BATCH, c.epoch).await {
                Ok(e) if !e.is_empty() => e,
                Ok(_) => continue,
                Err(e) => {
                    warn!("reading journal: {e:#}");
                    continue;
                }
            };
            let batch = upload_batch(c.session_id.clone(), c.epoch, events, UPLOAD_BYTES);
            let last = batch.events.last().map(|e| e.seq).unwrap_or(from);
            tokio::select! {
                _ = stop.cancelled() => return,
                r = tx.send(WorkerMsg { msg: Some(worker_msg::Msg::Events(batch)) }) => if r.is_err() { return },
            }
            sent.insert(c.session_id, (c.epoch, last));
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

fn upload_batch(session_id: String, ownership_epoch: u64, mut events: Vec<SessionEvent>, max_bytes: usize) -> SessionEventBatch {
    use prost::encoding::{encoded_len_varint, key_len, message};

    let mut batch = SessionEventBatch { session_id, events: Vec::new(), ownership_epoch };
    let mut batch_len = batch.encoded_len();
    let fitting = events
        .iter()
        .take_while(|event| {
            batch_len += message::encoded_len(2, *event); // SessionEventBatch.events
            let envelope_len = key_len(3) + encoded_len_varint(batch_len as u64); // WorkerMsg.events
            batch_len + envelope_len <= max_bytes
        })
        .count();
    // Always attempt the first event, even if it alone exceeds the batch target.
    events.truncate(fitting.max(1));
    batch.events = events;
    batch
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_upload_batch_counts_envelope_and_keeps_a_contiguous_prefix() {
        let session_id = "session".to_owned();
        let events: Vec<_> = (1..=3)
            .map(|seq| SessionEvent {
                seq,
                kind: Some(c6_proto::session_event::Kind::User(c6_proto::UserMessage {
                    attachments: vec![c6_proto::Attachment { data: vec![0; 128], ..Default::default() }],
                    ..Default::default()
                })),
                ..Default::default()
            })
            .collect();
        let wire_len = |batch| WorkerMsg { msg: Some(worker_msg::Msg::Events(batch)) }.encoded_len();
        let two_events = wire_len(SessionEventBatch { session_id: session_id.clone(), events: events[..2].to_vec(), ownership_epoch: 0 });

        let batch = upload_batch(session_id.clone(), 0, events.clone(), two_events);
        assert_eq!(batch.events, events[..2]);
        assert_eq!(wire_len(batch), two_events);
        let batch = upload_batch(session_id.clone(), 0, events.clone(), two_events - 1);
        assert_eq!(batch.events, events[..1]);
        assert!(wire_len(batch) < two_events);
        let first = events[..1].to_vec();
        let batch = upload_batch(session_id, 0, events, 1);
        assert_eq!(batch.events, first, "a single event still goes out above the batch target");
    }
}
