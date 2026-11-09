//! Live worker connections.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use c6_core::WorkerId;
use c6_proto::{HeadMsg, SessionCommand, head_msg};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub type HeadTx = mpsc::Sender<Result<HeadMsg, tonic::Status>>;
type RpcWaiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>;

#[derive(Clone, Debug)]
pub struct LiveWorker {
    pub conn_id: u64,
    pub connected_since: i64,
    pub last_seen: i64,
    pub rtt_us: Option<u32>,
    pub capabilities: Vec<String>,
    /// "worker" or "memex".
    pub role: String,
    cancel: CancellationToken,
    tx: HeadTx,
    /// Requests awaiting this connection's response; dropped with it.
    rpc_waiters: RpcWaiters,
    /// Wakes the connection task to send newly queued commands. All durable
    /// command delivery goes through that one task, which keeps it in order.
    deliver: Arc<Notify>,
}

pub struct Registry {
    conns: Mutex<HashMap<WorkerId, LiveWorker>>,
    next_conn: AtomicU64,
    /// Bumped on every change; UI subscribers coalesce on it.
    changed: watch::Sender<u64>,
    /// Parent of every connection's token, so head shutdown ends them all.
    shutdown: CancellationToken,
}

/// Handle owned by a connection's task. Dropping it unregisters the
/// connection, unless a newer connection for the same worker replaced it.
pub struct ConnGuard<'a> {
    registry: &'a Registry,
    pub worker_id: WorkerId,
    pub conn_id: u64,
    pub cancel: CancellationToken,
    pub deliver: Arc<Notify>,
    rpc_waiters: RpcWaiters,
}

impl ConnGuard<'_> {
    /// Delivers a worker's response to the waiting request.
    pub fn rpc_response(&self, resp: c6_proto::RpcResponse) {
        if let Some(w) = self.rpc_waiters.lock().unwrap().remove(&resp.id) {
            let result = if resp.error.is_empty() {
                serde_json::from_slice(&resp.result_json).map_err(|e| format!("bad response from worker: {e}"))
            } else {
                Err(resp.error)
            };
            let _ = w.send(result);
        }
    }
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        let removed = {
            let mut conns = self.registry.conns.lock().unwrap();
            match conns.get(&self.worker_id) {
                Some(c) if c.conn_id == self.conn_id => conns.remove(&self.worker_id).is_some(),
                _ => false,
            }
        };
        if removed {
            self.registry.bump();
        }
    }
}

impl Registry {
    pub fn new(shutdown: CancellationToken) -> Self {
        Self { conns: Mutex::default(), next_conn: AtomicU64::new(1), changed: watch::channel(0).0, shutdown }
    }

    /// Registers a connection. A previous connection for the same worker is
    /// cancelled: the newest one wins.
    pub fn attach(&self, worker_id: WorkerId, now: i64, tx: HeadTx, capabilities: Vec<String>, role: &str) -> ConnGuard<'_> {
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let cancel = self.shutdown.child_token();
        let deliver = Arc::new(Notify::new());
        let rpc_waiters = RpcWaiters::default();
        let live = LiveWorker {
            conn_id,
            connected_since: now,
            last_seen: now,
            rtt_us: None,
            capabilities,
            role: role.to_owned(),
            cancel: cancel.clone(),
            tx,
            rpc_waiters: rpc_waiters.clone(),
            deliver: deliver.clone(),
        };
        if let Some(old) = self.conns.lock().unwrap().insert(worker_id, live) {
            old.cancel.cancel();
        }
        self.bump();
        ConnGuard { registry: self, worker_id, conn_id, cancel, deliver, rpc_waiters }
    }

    /// Asks the worker's connection (if any) to deliver pending commands.
    pub fn deliver(&self, worker_id: WorkerId) {
        if let Some(c) = self.conns.lock().unwrap().get(&worker_id) {
            c.deliver.notify_one();
        }
    }

    /// Calls a worker method and waits for its result.
    pub async fn call(
        &self,
        worker_id: WorkerId,
        method: &str,
        params: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        self.call_with_params(worker_id, method, |_| params, timeout).await
    }

    /// Builds parameters for the same connection that sends the request and
    /// owns its response waiter. The synchronous builder runs without the
    /// registry lock; a replacement cannot change this call's target.
    pub async fn call_with_params(
        &self,
        worker_id: WorkerId,
        method: &str,
        params: impl FnOnce(&LiveWorker) -> serde_json::Value + Send,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let live = self.get(worker_id).ok_or("worker is not connected")?;
        let params = params(&live);
        let (tx, waiters) = (live.tx, live.rpc_waiters);
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (done, wait) = oneshot::channel();
        waiters.lock().unwrap().insert(id, done);
        let req = c6_proto::RpcRequest { id, method: method.to_owned(), params_json: serde_json::to_vec(&params).unwrap_or_default() };
        if tx.send(Ok(HeadMsg { msg: Some(head_msg::Msg::RpcRequest(req)) })).await.is_err() {
            waiters.lock().unwrap().remove(&id);
            return Err("worker disconnected".into());
        }
        match tokio::time::timeout(timeout, wait).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("worker disconnected".into()),
            Err(_) => {
                waiters.lock().unwrap().remove(&id);
                Err("worker did not respond in time".into())
            }
        }
    }

    /// Sends a non-durable command now; false if the worker is not connected.
    pub fn send_command(&self, worker_id: WorkerId, cmd: SessionCommand) -> bool {
        let tx = self.conns.lock().unwrap().get(&worker_id).map(|c| c.tx.clone());
        tx.is_some_and(|tx| tx.try_send(Ok(HeadMsg { msg: Some(head_msg::Msg::Command(cmd)) })).is_ok())
    }

    pub fn heartbeat(&self, worker_id: WorkerId, conn_id: u64, now: i64, rtt_us: Option<u32>) {
        let mut conns = self.conns.lock().unwrap();
        if let Some(c) = conns.get_mut(&worker_id).filter(|c| c.conn_id == conn_id) {
            c.last_seen = now;
            if rtt_us.is_some() {
                c.rtt_us = rtt_us;
            }
        }
        drop(conns);
        self.bump();
    }

    /// Drops a worker's connection, if any (used on revoke).
    pub fn kick(&self, worker_id: WorkerId) {
        if let Some(c) = self.conns.lock().unwrap().get(&worker_id) {
            c.cancel.cancel();
        }
    }

    pub fn get(&self, worker_id: WorkerId) -> Option<LiveWorker> {
        self.conns.lock().unwrap().get(&worker_id).cloned()
    }

    /// Connected workers with a capability (e.g. `kits`), longest connected first.
    pub fn with_capability(&self, capability: &str) -> Vec<WorkerId> {
        let conns = self.conns.lock().unwrap();
        let mut v: Vec<(i64, WorkerId)> =
            conns.iter().filter(|(_, c)| c.capabilities.iter().any(|x| x == capability)).map(|(id, c)| (c.connected_since, *id)).collect();
        v.sort();
        v.into_iter().map(|(_, id)| id).collect()
    }

    /// The connected memex, if any.
    pub fn memex(&self) -> Option<WorkerId> {
        self.conns.lock().unwrap().iter().find(|(_, c)| c.role == "memex").map(|(id, _)| *id)
    }

    /// Replies to a node's call to the head.
    pub async fn reply(&self, worker_id: WorkerId, reply: c6_proto::HeadCallReply) {
        let tx = self.conns.lock().unwrap().get(&worker_id).map(|c| c.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(Ok(HeadMsg { msg: Some(head_msg::Msg::HeadCallReply(reply)) })).await;
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_connection_wins() {
        let reg = Registry::new(CancellationToken::new());
        let id = WorkerId::new();
        let tx = mpsc::channel(1).0;
        let first = reg.attach(id, 1, tx.clone(), vec![], "worker");
        let second = reg.attach(id, 2, tx, vec![], "worker");
        assert!(first.cancel.is_cancelled());
        drop(first); // must not unregister the newer connection
        assert_eq!(reg.get(id).unwrap().conn_id, second.conn_id);
        drop(second);
        assert!(reg.get(id).is_none());
    }

    #[tokio::test]
    async fn call_with_params_keeps_capabilities_sender_and_waiter_on_selected_connection_after_replacement() {
        use base64::Engine;
        use prost::Message;

        #[derive(Clone, PartialEq, prost::Message)]
        struct ForkInput {
            #[prost(bool, tag = "16")]
            human_origin: bool,
        }

        for capable in [false, true] {
            let reg = Registry::new(CancellationToken::new());
            let id = WorkerId::new();
            let (selected_tx, mut selected_rx) = mpsc::channel(1);
            let (replacement_tx, mut replacement_rx) = mpsc::channel(1);
            let capabilities = if capable { vec!["user_input_prompt_v1".into()] } else { vec![] };
            let selected = reg.attach(id, 1, selected_tx, capabilities, "worker");
            let replacement = Mutex::new(None);
            let input = c6_proto::UserInput { text: "original".into(), prompt: "expanded mention".into(), ..Default::default() };
            let call = reg.call_with_params(
                id,
                "session.queue_op",
                |live| {
                    assert_eq!(live.conn_id, selected.conn_id);
                    // Replace after target selection but before encoding. This
                    // also proves the builder does not hold the registry lock.
                    let capabilities = if capable { vec![] } else { vec!["user_input_prompt_v1".into()] };
                    *replacement.lock().unwrap() = Some(reg.attach(id, 2, replacement_tx, capabilities, "worker"));
                    let mut wire_input = input.clone();
                    crate::sessions::prepare_worker_input(&live.capabilities, &mut wire_input);
                    let data = base64::engine::general_purpose::STANDARD.encode(wire_input.encode_to_vec());
                    serde_json::json!({ "op": "update", "data": data })
                },
                std::time::Duration::from_secs(5),
            );
            let reply = async {
                let message = selected_rx.recv().await.unwrap().unwrap();
                let Some(head_msg::Msg::RpcRequest(request)) = message.msg else { panic!("queue RPC") };
                assert!(selected.cancel.is_cancelled());
                assert!(replacement_rx.try_recv().is_err(), "request must not follow the replacement connection");
                let params: serde_json::Value = serde_json::from_slice(&request.params_json).unwrap();
                let data = base64::engine::general_purpose::STANDARD.decode(params["data"].as_str().unwrap()).unwrap();
                let delivered = c6_proto::UserInput::decode(data.as_slice()).unwrap();
                assert_eq!(delivered.prompt, if capable { "expanded mention" } else { "" });
                if !capable {
                    ForkInput::decode(data.as_slice()).expect("selected old worker must not receive string tag 16");
                }
                // A replacement's response cannot settle the selected
                // connection's waiter, even with the same request id.
                replacement.lock().unwrap().as_ref().unwrap().rpc_response(c6_proto::RpcResponse {
                    id: request.id,
                    result_json: br#"{"connection":"replacement"}"#.to_vec(),
                    error: String::new(),
                });
                selected.rpc_response(c6_proto::RpcResponse {
                    id: request.id,
                    result_json: br#"{"connection":"selected"}"#.to_vec(),
                    error: String::new(),
                });
            };
            let (result, ()) = tokio::join!(call, reply);
            assert_eq!(result.unwrap()["connection"], "selected");
            assert_eq!(input.prompt, "expanded mention", "wire gating must leave the saved input intact");
        }
    }
}
