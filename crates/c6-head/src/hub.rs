//! gRPC services on the worker port: `Enrollment` and `WorkerHub`.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use c6_core::{WorkerId, now_ms, protocol_compatible};
use c6_proto::{
    EnrollRequest, EnrollResponse, EventAck, HeadMsg, Hello, Pong, PtyFrame, SessionCursor, Welcome, WorkerMsg,
    enrollment_server::Enrollment, head_msg, pty_frame, worker_hub_server::WorkerHub, worker_msg,
};
use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, warn};

use crate::store::NewWorker;
use crate::{HeadState, sessions};

/// How long a worker may stay silent before the head drops it. Workers ping
/// every 10s.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const OUTBOUND_CAPACITY: usize = 1024;
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

type PtyStream = Pin<Box<dyn Stream<Item = Result<PtyFrame, Status>> + Send>>;

#[tonic::async_trait]
impl WorkerHub for HubSvc {
    type AttachStream = HeadStream;
    type PtyStream = PtyStream;

    /// A worker answering a terminal attach: hand the stream to the waiting
    /// browser connection identified by the attach token.
    async fn pty(&self, req: Request<Streaming<PtyFrame>>) -> Result<Response<PtyStream>, Status> {
        let certs = req.peer_certs().ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let (worker_id, _) = certs
            .first()
            .and_then(|c| crate::pki::worker_identity(c))
            .ok_or_else(|| Status::unauthenticated("certificate has no worker identity"))?;
        let mut inbound = req.into_inner();
        let open = match inbound.message().await? {
            Some(PtyFrame { frame: Some(pty_frame::Frame::Open(o)) }) => o,
            _ => return Err(Status::invalid_argument("first frame must be Open")),
        };
        let waiter = self.0.terminals.take(&open.attach_token, worker_id).ok_or_else(|| Status::not_found("no such attach"))?;
        let (tx, rx) = mpsc::channel(256);
        waiter.send((inbound, tx)).map_err(|_| Status::cancelled("viewer went away"))?;
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn attach(&self, req: Request<Streaming<WorkerMsg>>) -> Result<Response<HeadStream>, Status> {
        let st = self.0.clone();

        // Identity comes from the verified client certificate only.
        let certs = req.peer_certs().ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let (worker_id, serial) = certs
            .first()
            .and_then(|c| crate::pki::worker_identity(c))
            .ok_or_else(|| Status::unauthenticated("certificate has no worker identity"))?;
        let row = st.store.worker(worker_id).await.map_err(internal)?.ok_or_else(|| Status::permission_denied("unknown worker"))?;
        if row.revoked_at.is_some() || !crate::pki::same_serial(&row.cert_serial, &serial) {
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
        // A node's role is what it says the first time, then fixed: a
        // worker cannot later claim to be the memex, or the reverse.
        let says = if hello.capabilities.iter().any(|c| c == "memex") { "memex" } else { "worker" };
        let role = if row.role.is_empty() {
            st.store.set_worker_role(worker_id, says).await.map_err(internal)?;
            says.to_owned()
        } else if row.role != says {
            return Err(Status::failed_precondition(format!(
                "this node was enrolled as a {}; enroll the {says} with a new token",
                row.role
            )));
        } else {
            row.role.clone()
        };
        // The knowledge graph is the head's own now (docs/knowledge.md).
        if role == "memex" {
            return Err(Status::failed_precondition(
                "the memex is part of the head now: stop c6m (the head moves its data in when it upgrades)",
            ));
        }
        st.store.worker_attached(worker_id, hostname, os, arch, version, now_ms()).await.map_err(internal)?;

        let session_ids: Vec<String> = hello.sessions.iter().map(|c| c.session_id.clone()).collect();
        let acks = st.store.session_positions(worker_id, &session_ids).await.map_err(internal)?;
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let welcome = Welcome {
            protocol_version: c6_core::PROTOCOL_VERSION,
            head_version: c6_core::build().into(),
            worker_id: worker_id.to_string(),
            worker_name: row.name.clone(),
            acks: acks.into_iter().map(|(session_id, seq)| SessionCursor { session_id, seq }).collect(),
        };
        tx.send(Ok(head(head_msg::Msg::Welcome(welcome)))).await.ok();

        tokio::spawn(async move {
            let guard = st.registry.attach(worker_id, now_ms(), tx.clone(), hello.capabilities.clone(), &role);
            if role == "memex" {
                st.memex_feed.wake();
            }
            info!(worker = %worker_id, name = %row.name, version = %hello.version, "worker attached");
            let reason = serve_worker(&st, &guard, &mut inbound, &tx, &hello.capabilities).await;
            st.store.touch_worker(worker_id, now_ms()).await.ok();
            info!(worker = %worker_id, %reason, "worker detached");
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

async fn serve_worker(
    st: &Arc<HeadState>,
    guard: &crate::registry::ConnGuard<'_>,
    inbound: &mut Streaming<WorkerMsg>,
    tx: &mpsc::Sender<Result<HeadMsg, Status>>,
    capabilities: &[String],
) -> String {
    let worker_id = guard.worker_id;
    // Highest command seq sent on this connection; a new connection starts
    // over, and the worker ignores commands it already applied.
    let mut delivered: i64 = 0;
    guard.deliver.notify_one();
    let mut deadline = Instant::now() + IDLE_TIMEOUT;
    loop {
        let msg = tokio::select! {
            _ = guard.cancel.cancelled() => {
                tx.send(Err(Status::aborted("connection replaced or revoked"))).await.ok();
                return "cancelled".into();
            }
            _ = tokio::time::sleep_until(deadline.into()) => return "idle timeout".into(),
            _ = guard.deliver.notified() => {
                match deliver_pending(st, worker_id, &mut delivered, tx, capabilities).await {
                    Ok(true) => continue,
                    Ok(false) => return "response stream closed".into(),
                    Err(e) => {
                        warn!(worker = %worker_id, "delivering commands: {e:#}");
                        continue;
                    }
                }
            }
            r = inbound.message() => r,
        };
        deadline = Instant::now() + IDLE_TIMEOUT;
        let m = match msg {
            Err(e) => return format!("stream error: {}", e.message()),
            Ok(None) => return "closed by worker".into(),
            Ok(Some(WorkerMsg { msg: None })) => continue, // unknown message from a newer worker
            Ok(Some(WorkerMsg { msg: Some(m) })) => m,
        };
        match m {
            worker_msg::Msg::Ping(p) => {
                let rtt = (p.last_rtt_us > 0).then_some(p.last_rtt_us);
                st.registry.heartbeat(worker_id, guard.conn_id, now_ms(), rtt);
                // Never block on control replies: losing a pong is harmless.
                let _ = tx.try_send(Ok(head(head_msg::Msg::Pong(Pong { nonce: p.nonce }))));
            }
            worker_msg::Msg::Hello(_) => warn!(worker = %worker_id, "duplicate Hello ignored"),
            worker_msg::Msg::Events(batch) => {
                let session_id = batch.session_id.clone();
                match sessions::ingest(st, worker_id, batch).await {
                    // Acks are cumulative, so a dropped one is covered by the next.
                    Ok(Some(seq)) => {
                        let _ = tx.try_send(Ok(head(head_msg::Msg::Ack(EventAck { session_id, seq }))));
                    }
                    Ok(None) => {}
                    Err(e) => warn!(worker = %worker_id, session = %session_id, "storing events: {e:#}"),
                }
            }
            worker_msg::Msg::Ephemeral(ev) => sessions::on_ephemeral(st, worker_id, ev).await,
            worker_msg::Msg::Rejected(r) => {
                if let Err(e) = sessions::on_rejected(st, worker_id, r).await {
                    warn!(worker = %worker_id, "recording rejection: {e:#}");
                }
            }
            worker_msg::Msg::RpcResponse(resp) => guard.rpc_response(resp),
            worker_msg::Msg::HeadCall(call) => {
                // Off the read loop: a relayed call waits on another node.
                let st = st.clone();
                tokio::spawn(async move {
                    let params = serde_json::from_slice(&call.params_json).unwrap_or(serde_json::Value::Null);
                    let reply = match crate::fabric::handle(&st, worker_id, &call.method, params).await {
                        Ok(v) => c6_proto::HeadCallReply {
                            id: call.id,
                            result_json: serde_json::to_vec(&v).unwrap_or_default(),
                            error: String::new(),
                        },
                        Err(e) => c6_proto::HeadCallReply { id: call.id, result_json: Vec::new(), error: format!("{e:#}") },
                    };
                    st.registry.reply(worker_id, reply).await;
                });
            }
            worker_msg::Msg::CredentialRequest(req) => {
                let grant = sessions::credential(st, worker_id, req).await;
                if tx.send(Ok(head(head_msg::Msg::Credential(grant)))).await.is_err() {
                    return "response stream closed".into();
                }
            }
        }
    }
}

/// Sends pending commands newer than `delivered`, in order. Ok(false) if the
/// stream closed.
async fn deliver_pending(
    st: &HeadState,
    worker_id: WorkerId,
    delivered: &mut i64,
    tx: &mpsc::Sender<Result<HeadMsg, Status>>,
    capabilities: &[String],
) -> anyhow::Result<bool> {
    loop {
        let rows = st.store.pending_commands(worker_id, *delivered).await?;
        if rows.is_empty() {
            return Ok(true);
        }
        for row in rows {
            let mut cmd = c6_proto::decode_stored_command(&row.data)?;
            // Use this stream's Hello, not a replacement in the registry.
            let compatibility = match sessions::prepare_worker_subagents(capabilities, &mut cmd) {
                Ok(()) if matches!(cmd.kind, Some(c6_proto::session_command::Kind::Input(_))) => {
                    sessions::check_worker_subagent_input(st, capabilities, &cmd.session_id).await
                }
                result => result,
            };
            if let Err(error) = compatibility {
                // A transient store failure or malformed saved spec is not
                // a capability refusal: leave it pending for recovery.
                if error.downcast_ref::<crate::UserError>().is_none() {
                    return Err(error);
                }
                sessions::on_rejected(
                    st,
                    worker_id,
                    c6_proto::CommandRejected { command_id: cmd.command_id, session_id: cmd.session_id, reason: error.to_string() },
                )
                .await?;
                *delivered = row.seq;
                // Rejecting Create also retires its queued inputs. Reload
                // rather than sending those from this now-stale batch.
                break;
            }
            if let Some(c6_proto::session_command::Kind::Input(input)) = &mut cmd.kind {
                // Use this connection's Hello, not a replacement connection's
                // capabilities while the previous delivery task is exiting.
                sessions::prepare_worker_input(capabilities, input);
            }
            if tx.send(Ok(head(head_msg::Msg::Command(cmd)))).await.is_err() {
                return Ok(false);
            }
            *delivered = row.seq;
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

#[cfg(test)]
mod tests {
    use super::*;
    use c6_proto::{UserInput, session_command};
    use prost::Message;

    #[derive(Clone, PartialEq, prost::Message)]
    struct ForkInput {
        #[prost(bool, tag = "16")]
        human_origin: bool,
    }

    #[tokio::test]
    async fn claude_code_subagent_choices_follow_the_receiving_hello_on_create_and_restore() {
        let (_dir, head, _mock, _work) = crate::work::tests::routing_fixture().await;
        let st = &head.state;
        let choices = serde_json::json!([
            {"id": "api", "name": "API", "kind": "openai_compat"},
            {"id": "cc", "name": "Claude Code", "kind": "claude_code", "default_model": "opus"}
        ])
        .to_string();
        for capable in [false, true] {
            let worker = WorkerId::new();
            let mut capabilities = vec!["claude_code".into()];
            if capable {
                capabilities.push("claude_code_subagents_v1".into());
            }
            let (tx, mut rx) = mpsc::channel(8);
            // An unrelated/replacement capable worker is not evidence that
            // the receiving connection implements Claude Code subagents.
            let replacement = if capable { vec!["claude_code".into()] } else { vec!["claude_code_subagents_v1".into()] };
            let guard = st.registry.attach(worker, now_ms(), tx.clone(), replacement, "worker");
            let mut saved = Vec::new();
            for restore in [false, true] {
                let id = format!("choices-{capable}-{restore}");
                let spec = c6_proto::CreateSession { subagent_connections_json: choices.clone(), ..Default::default() };
                let kind = if restore {
                    session_command::Kind::Restore(c6_proto::Restore { spec: Some(spec), ..Default::default() })
                } else {
                    session_command::Kind::Create(spec)
                };
                let data =
                    c6_proto::SessionCommand { command_id: id.clone(), session_id: "session".into(), kind: Some(kind) }.encode_to_vec();
                saved.push(data.clone());
                st.store
                    .enqueue_command(&crate::store::NewCommand { id: &id, session_id: "session", worker_id: worker, data, text: None })
                    .await
                    .unwrap();
            }
            let mut delivered = 0;
            assert!(deliver_pending(st, worker, &mut delivered, &tx, &capabilities).await.unwrap());
            for _ in 0..2 {
                let message = rx.recv().await.unwrap().unwrap();
                let Some(head_msg::Msg::Command(command)) = message.msg else { panic!("session command") };
                let spec = match command.kind.unwrap() {
                    session_command::Kind::Create(spec) => spec,
                    session_command::Kind::Restore(restore) => restore.spec.unwrap(),
                    _ => panic!("create or restore"),
                };
                let choices: Vec<c6_api::SubagentConnection> = serde_json::from_str(&spec.subagent_connections_json).unwrap();
                assert_eq!(choices.iter().any(|c| c.kind == "claude_code"), capable);
                assert!(choices.iter().any(|c| c.kind == "openai_compat"));
            }
            assert!(rx.try_recv().is_err());
            let stored: Vec<_> = st.store.pending_commands(worker, 0).await.unwrap().into_iter().map(|c| c.data).collect();
            assert_eq!(stored, saved, "wire filtering must leave the durable choices intact for a later worker");
            drop(guard);
        }
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_old_worker_never_receives_an_unsupported_saved_target_or_its_following_inputs() {
        let (_dir, head, _mock, _work) = crate::work::tests::routing_fixture().await;
        let st = &head.state;
        let worker = WorkerId::new();
        sqlx::query("UPDATE sessions SET worker_id = ?, connection_id = 'api', subagent_connection_id = 'cc', subagent_model = 'opus' WHERE id = 'session'")
            .bind(worker.to_string())
            .execute(&st.store.pool)
            .await
            .unwrap();
        let target = c6_proto::ModelSpec {
            connection_id: "cc".into(),
            connection_kind: "claude_code".into(),
            model: "opus".into(),
            ..Default::default()
        };
        for (id, kind, text) in [
            (
                "restore",
                session_command::Kind::Restore(c6_proto::Restore {
                    spec: Some(c6_proto::CreateSession { subagent_target: Some(target.clone()), ..Default::default() }),
                    ..Default::default()
                }),
                None,
            ),
            ("set", session_command::Kind::SetSubagentModel(c6_proto::SetSubagentModel { model: Some(target.clone()) }), None),
            ("input", session_command::Kind::Input(UserInput { text: "continue".into(), ..Default::default() }), Some("continue")),
        ] {
            let data = c6_proto::SessionCommand { command_id: id.into(), session_id: "session".into(), kind: Some(kind) }.encode_to_vec();
            st.store.enqueue_command(&crate::store::NewCommand { id, session_id: "session", worker_id: worker, data, text }).await.unwrap();
        }
        let (tx, mut rx) = mpsc::channel(8);
        let mut delivered = 0;
        assert!(deliver_pending(st, worker, &mut delivered, &tx, &["claude_code".into()]).await.unwrap());
        assert!(rx.try_recv().is_err(), "neither the target nor a later input may run in the wrong runtime");
        let rejected: Vec<(String, String)> = sqlx::query_as("SELECT state, error FROM commands WHERE worker_id = ? ORDER BY seq")
            .bind(worker.to_string())
            .fetch_all(&st.store.pool)
            .await
            .unwrap();
        assert_eq!(rejected.len(), 3);
        assert!(rejected.iter().all(|(state, error)| state == "rejected" && error.contains("claude_code_subagents_v1")));
        let row = st.store.session("session").await.unwrap().unwrap();
        assert_eq!((row.subagent_connection_id.as_str(), row.subagent_model.as_str()), ("cc", "opus"));

        // Reconnecting an upgraded worker can use that unchanged target.
        let command = c6_proto::SessionCommand {
            command_id: "upgraded".into(),
            session_id: "session".into(),
            kind: Some(session_command::Kind::Create(c6_proto::CreateSession {
                subagent_target: Some(target.clone()),
                ..Default::default()
            })),
        };
        st.store
            .enqueue_command(&crate::store::NewCommand {
                id: "upgraded",
                session_id: "session",
                worker_id: worker,
                data: command.encode_to_vec(),
                text: None,
            })
            .await
            .unwrap();
        assert!(deliver_pending(st, worker, &mut delivered, &tx, &["claude_code_subagents_v1".into()]).await.unwrap());
        let Some(head_msg::Msg::Command(received)) = rx.recv().await.unwrap().unwrap().msg else { panic!("create") };
        assert_eq!(received, command);

        // Rejecting Create must also suppress inputs already fetched in
        // that delivery batch, without overwriting its saved target.
        let mut unsupported = command.clone();
        unsupported.command_id = "unsupported-create".into();
        let original = unsupported.encode_to_vec();
        st.store
            .enqueue_command(&crate::store::NewCommand {
                id: "unsupported-create",
                session_id: "session",
                worker_id: worker,
                data: original.clone(),
                text: None,
            })
            .await
            .unwrap();
        let input = c6_proto::SessionCommand {
            command_id: "after-create".into(),
            session_id: "session".into(),
            kind: Some(session_command::Kind::Input(UserInput { text: "do not run".into(), ..Default::default() })),
        };
        st.store
            .enqueue_command(&crate::store::NewCommand {
                id: "after-create",
                session_id: "session",
                worker_id: worker,
                data: input.encode_to_vec(),
                text: Some("do not run"),
            })
            .await
            .unwrap();
        assert!(deliver_pending(st, worker, &mut delivered, &tx, &["claude_code".into()]).await.unwrap());
        assert!(rx.try_recv().is_err());
        let row = st.store.session("session").await.unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert!(row.error.unwrap().contains("claude_code_subagents_v1"));
        assert_eq!((row.subagent_connection_id.as_str(), row.subagent_model.as_str()), ("cc", "opus"));
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT data FROM commands WHERE id = 'unsupported-create'").fetch_one(&st.store.pool).await.unwrap();
        assert_eq!(stored, original);
        assert!(st.store.pending_commands(worker, delivered).await.unwrap().is_empty());
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stored_legacy_and_enriched_inputs_deliver_with_only_the_target_connections_prompt_capability() {
        let (_dir, head, _mock, _work) = crate::work::tests::routing_fixture().await;
        let st = &head.state;
        for capable in [false, true] {
            let worker = WorkerId::new();
            let mut capabilities = vec!["queue_ops".into(), "send_now".into()];
            if capable {
                capabilities.push("user_input_prompt_v1".into());
            }
            for legacy in [true, false] {
                let id = format!("{capable}-{legacy}");
                let input = UserInput {
                    text: "original [@Plan](c6:doc:d1)".into(),
                    human_origin: true,
                    prompt: if legacy { String::new() } else { "original text plus enriched document".into() },
                    task_card_origin: c6_proto::TaskCardOrigin::Answer as i32,
                    ..Default::default()
                };
                let data = if legacy {
                    crate::store::tests::legacy_input_command(&id, "session", input)
                } else {
                    c6_proto::SessionCommand {
                        command_id: id.clone(),
                        session_id: "session".into(),
                        kind: Some(session_command::Kind::Input(input)),
                    }
                    .encode_to_vec()
                };
                st.store
                    .enqueue_command(&crate::store::NewCommand {
                        id: &id,
                        session_id: "session",
                        worker_id: worker,
                        data,
                        text: Some("original"),
                    })
                    .await
                    .unwrap();
            }
            let (tx, mut rx) = mpsc::channel(8);
            // A replacing connection must not cause a prompt to be sent to
            // the old connection whose own Hello did not advertise it.
            let guard = st.registry.attach(worker, now_ms(), tx.clone(), vec!["user_input_prompt_v1".into()], "worker");
            let mut delivered = 0;
            assert!(deliver_pending(st, worker, &mut delivered, &tx, &capabilities).await.unwrap());
            for legacy in [true, false] {
                let message = rx.recv().await.unwrap().unwrap();
                let Some(head_msg::Msg::Command(command)) = message.msg else { panic!("input command") };
                assert_eq!(command.command_id, format!("{capable}-{legacy}"));
                let Some(session_command::Kind::Input(input)) = command.kind else { panic!("input") };
                assert_eq!(input.text, "original [@Plan](c6:doc:d1)");
                assert!(input.human_origin);
                assert_eq!(input.task_card_origin, c6_proto::TaskCardOrigin::Answer as i32);
                assert_eq!(!input.prompt.is_empty(), capable && !legacy);
                if !capable {
                    ForkInput::decode(input.encode_to_vec().as_slice()).expect("old worker must not receive a string tag 16");
                }
                let stored = st.store.pending_input(&format!("{capable}-{legacy}")).await.unwrap().unwrap().2;
                let Some(session_command::Kind::Input(saved)) = c6_proto::decode_stored_command(&stored).unwrap().kind else {
                    panic!("saved input")
                };
                assert_eq!(!saved.prompt.is_empty(), !legacy, "wire downgrade must not erase saved enrichment");
            }
            assert!(rx.try_recv().is_err());
            drop(guard);
        }
        head.shutdown().await.unwrap();
    }
}
