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
    pub fn attach(&self, worker_id: WorkerId, now: i64, tx: HeadTx, capabilities: Vec<String>) -> ConnGuard<'_> {
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
        let (tx, waiters) = {
            let conns = self.conns.lock().unwrap();
            let c = conns.get(&worker_id).ok_or("worker is not connected")?;
            (c.tx.clone(), c.rpc_waiters.clone())
        };
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
        let first = reg.attach(id, 1, tx.clone(), vec![]);
        let second = reg.attach(id, 2, tx, vec![]);
        assert!(first.cancel.is_cancelled());
        drop(first); // must not unregister the newer connection
        assert_eq!(reg.get(id).unwrap().conn_id, second.conn_id);
        drop(second);
        assert!(reg.get(id).is_none());
    }
}
