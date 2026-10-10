//! Browser ↔ worker terminal bridging. A browser WebSocket asks a worker to
//! open a `Pty` stream back to the head, tagged with a one-time token; the
//! gRPC handler hands that stream to the waiting socket.

use std::collections::HashMap;
use std::sync::Mutex;

use c6_core::WorkerId;
use c6_proto::PtyFrame;
use tokio::sync::{mpsc, oneshot};
use tonic::{Status, Streaming};

pub type PtyPair = (Streaming<PtyFrame>, mpsc::Sender<Result<PtyFrame, Status>>);

#[derive(Default)]
pub struct Attaches {
    waiting: Mutex<HashMap<String, (WorkerId, oneshot::Sender<PtyPair>)>>,
}

impl Attaches {
    pub fn expect(&self, worker_id: WorkerId) -> (String, oneshot::Receiver<PtyPair>) {
        let token = hex::encode(rand::random::<[u8; 16]>());
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(token.clone(), (worker_id, tx));
        (token, rx)
    }

    /// The waiting viewer for `token`, if it belongs to `worker_id`.
    pub fn take(&self, token: &str, worker_id: WorkerId) -> Option<oneshot::Sender<PtyPair>> {
        let mut w = self.waiting.lock().unwrap();
        match w.get(token) {
            Some((owner, _)) if *owner == worker_id => w.remove(token).map(|(_, tx)| tx),
            _ => None,
        }
    }

    pub fn forget(&self, token: &str) {
        self.waiting.lock().unwrap().remove(token);
    }
}
