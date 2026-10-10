//! gRPC services on the worker port: `Enrollment` and `WorkerHub`.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use c6_core::{WorkerId, now_ms, protocol_compatible};
use c6_proto::{
    EnrollRequest, EnrollResponse, HeadMsg, Hello, Pong, Welcome, WorkerMsg, enrollment_server::Enrollment, head_msg,
    worker_hub_server::WorkerHub, worker_msg,
};
use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, warn};

use crate::HeadState;
use crate::store::NewWorker;

/// How long a worker may stay silent before the head drops it. Workers ping
/// every 10s.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct EnrollmentSvc(pub Arc<HeadState>);

#[tonic::async_trait]
impl Enrollment for EnrollmentSvc {
    async fn enroll(&self, req: Request<EnrollRequest>) -> Result<Response<EnrollResponse>, Status> {
        let req = req.into_inner();
        let st = &self.0;
        let now = now_ms();
        let secret_hash = c6_core::sha256_hex(req.secret.as_bytes());

        let Some(requested_name) = st.store.consume_enrollment(&secret_hash, now).await.map_err(internal)? else {
            return Err(Status::permission_denied("invalid, used or expired enrollment token"));
        };

        let id = WorkerId::new();
        let issued = st.pki.issue_worker_cert(&req.csr_pem, id).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let name = requested_name.filter(|n| !n.is_empty()).unwrap_or_else(|| req.hostname.clone());
        let worker = NewWorker {
            id,
            name: &name,
            hostname: &req.hostname,
            os: &req.os,
            arch: &req.arch,
            version: &req.version,
            cert_serial: &issued.serial_hex,
        };
        st.store.insert_worker(&worker, &secret_hash, now).await.map_err(internal)?;
        st.registry.bump();
        info!(worker = %id, %name, hostname = %req.hostname, "worker enrolled");

        Ok(Response::new(EnrollResponse {
            worker_id: id.to_string(),
            worker_name: name,
            cert_pem: issued.cert_pem,
            ca_pem: st.pki.ca_cert_pem().to_owned(),
        }))
    }
}

pub struct HubSvc(pub Arc<HeadState>);

type HeadStream = Pin<Box<dyn Stream<Item = Result<HeadMsg, Status>> + Send>>;

#[tonic::async_trait]
impl WorkerHub for HubSvc {
    type AttachStream = HeadStream;

    async fn attach(&self, req: Request<Streaming<WorkerMsg>>) -> Result<Response<HeadStream>, Status> {
        let st = self.0.clone();

        // Identity comes from the verified client certificate only.
        let certs = req.peer_certs().ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let (worker_id, serial) = certs
            .first()
            .and_then(|c| crate::pki::worker_identity(c))
            .ok_or_else(|| Status::unauthenticated("certificate has no worker identity"))?;
        let row = st.store.worker(worker_id).await.map_err(internal)?.ok_or_else(|| Status::permission_denied("unknown worker"))?;
        if row.revoked_at.is_some() || row.cert_serial != serial {
            return Err(Status::permission_denied("worker revoked"));
        }

        let mut inbound = req.into_inner();
        let hello = match tokio::time::timeout(HELLO_TIMEOUT, inbound.message()).await {
            Ok(Ok(Some(WorkerMsg { msg: Some(worker_msg::Msg::Hello(h)) }))) => h,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(Status::deadline_exceeded("no Hello")),
            _ => return Err(Status::invalid_argument("first message must be Hello")),
        };
        if !protocol_compatible(hello.protocol_version) {
            return Err(Status::failed_precondition(format!(
                "protocol version {} not supported (head speaks {})",
                hello.protocol_version,
                c6_core::PROTOCOL_VERSION
            )));
        }
        let Hello { hostname, os, arch, version, .. } = &hello;
        st.store.worker_attached(worker_id, hostname, os, arch, version, now_ms()).await.map_err(internal)?;

        let (tx, rx) = mpsc::channel(64);
        let welcome = Welcome {
            protocol_version: c6_core::PROTOCOL_VERSION,
            head_version: c6_core::VERSION.into(),
            worker_id: worker_id.to_string(),
            worker_name: row.name.clone(),
        };
        tx.send(Ok(head(head_msg::Msg::Welcome(welcome)))).await.ok();

        tokio::spawn(async move {
            let guard = st.registry.attach(worker_id, now_ms());
            info!(worker = %worker_id, name = %row.name, version = %hello.version, "worker attached");
            let reason = serve_worker(&st, &guard, &mut inbound, &tx).await;
            st.store.touch_worker(worker_id, now_ms()).await.ok();
            info!(worker = %worker_id, %reason, "worker detached");
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

async fn serve_worker(
    st: &HeadState,
    guard: &crate::registry::ConnGuard<'_>,
    inbound: &mut Streaming<WorkerMsg>,
    tx: &mpsc::Sender<Result<HeadMsg, Status>>,
) -> String {
    loop {
        let msg = tokio::select! {
            _ = guard.cancel.cancelled() => {
                tx.send(Err(Status::aborted("connection replaced or revoked"))).await.ok();
                return "cancelled".into();
            }
            r = tokio::time::timeout(IDLE_TIMEOUT, inbound.message()) => r,
        };
        match msg {
            Err(_) => return "idle timeout".into(),
            Ok(Err(e)) => return format!("stream error: {}", e.message()),
            Ok(Ok(None)) => return "closed by worker".into(),
            Ok(Ok(Some(WorkerMsg { msg: Some(m) }))) => match m {
                worker_msg::Msg::Ping(p) => {
                    let rtt = (p.last_rtt_us > 0).then_some(p.last_rtt_us);
                    st.registry.heartbeat(guard.worker_id, guard.conn_id, now_ms(), rtt);
                    if tx.send(Ok(head(head_msg::Msg::Pong(Pong { nonce: p.nonce })))).await.is_err() {
                        return "response stream closed".into();
                    }
                }
                worker_msg::Msg::Hello(_) => warn!(worker = %guard.worker_id, "duplicate Hello ignored"),
            },
            Ok(Ok(Some(WorkerMsg { msg: None }))) => {} // unknown message from a newer worker
        }
    }
}

fn head(msg: head_msg::Msg) -> HeadMsg {
    HeadMsg { msg: Some(msg) }
}

fn internal(e: anyhow::Error) -> Status {
    warn!("internal error: {e:#}");
    Status::internal("internal error")
}
