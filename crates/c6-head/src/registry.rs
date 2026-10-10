//! Live worker connections.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use c6_core::WorkerId;
use c6_proto::{HeadMsg, SessionCommand, head_msg};
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

pub type HeadTx = mpsc::Sender<Result<HeadMsg, tonic::Status>>;

#[derive(Clone, Debug)]
pub struct LiveWorker {
    pub conn_id: u64,
    pub connected_since: i64,
    pub last_seen: i64,
    pub rtt_us: Option<u32>,
    pub capabilities: Vec<String>,
    cancel: CancellationToken,
    tx: HeadTx,
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
        let live = LiveWorker {
            conn_id,
            connected_since: now,
            last_seen: now,
            rtt_us: None,
            capabilities,
            cancel: cancel.clone(),
            tx,
            deliver: deliver.clone(),
        };
        if let Some(old) = self.conns.lock().unwrap().insert(worker_id, live) {
            old.cancel.cancel();
        }
        self.bump();
        ConnGuard { registry: self, worker_id, conn_id, cancel, deliver }
    }

    /// Asks the worker's connection (if any) to deliver pending commands.
    pub fn deliver(&self, worker_id: WorkerId) {
        if let Some(c) = self.conns.lock().unwrap().get(&worker_id) {
            c.deliver.notify_one();
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
