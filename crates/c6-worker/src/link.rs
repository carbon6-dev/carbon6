//! The sessions' view of the head connection: live events and credentials.
//! Works whether or not the worker is currently connected.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail};
use c6_proto::{CredentialGrant, CredentialRequest, EphemeralEvent, WorkerMsg, worker_msg};
use tokio::sync::{mpsc, oneshot};

const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Refresh credentials this long before they expire.
const EXPIRY_MARGIN_MS: i64 = 60_000;

#[derive(Default)]
pub struct HeadLink {
    outbound: Mutex<Option<mpsc::Sender<WorkerMsg>>>,
    waiters: Mutex<HashMap<u64, oneshot::Sender<CredentialGrant>>>,
    /// In memory only: credentials are never written to disk on workers.
    cache: Mutex<HashMap<String, CredentialGrant>>,
    next_request: AtomicU64,
}

impl HeadLink {
    pub(crate) fn connected(&self, tx: mpsc::Sender<WorkerMsg>) {
        *self.outbound.lock().unwrap() = Some(tx);
    }

    pub(crate) fn disconnected(&self) {
        *self.outbound.lock().unwrap() = None;
        self.waiters.lock().unwrap().clear(); // waiters see a closed channel
    }

    pub fn is_connected(&self) -> bool {
        self.outbound.lock().unwrap().is_some()
    }

    /// Best effort: dropped when disconnected or backed up.
    pub fn ephemeral(&self, ev: EphemeralEvent) {
        if let Some(tx) = self.outbound.lock().unwrap().as_ref() {
            let _ = tx.try_send(WorkerMsg { msg: Some(worker_msg::Msg::Ephemeral(ev)) });
        }
    }

    pub async fn credential(&self, connection_id: &str) -> anyhow::Result<CredentialGrant> {
        let now = c6_core::now_ms();
        let cached = self.cache.lock().unwrap().get(connection_id).cloned();
        if let Some(g) = &cached
            && (g.expires_at_ms == 0 || g.expires_at_ms - EXPIRY_MARGIN_MS > now)
        {
            return Ok(g.clone());
        }

        let tx = self.outbound.lock().unwrap().clone();
        let Some(tx) = tx else {
            // An expiring-but-valid token is better than nothing offline.
            if let Some(g) = cached.filter(|g| g.expires_at_ms > now) {
                return Ok(g);
            }
            bail!("not connected to the head and no cached credentials for this connection");
        };
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (done, wait) = oneshot::channel();
        self.waiters.lock().unwrap().insert(request_id, done);
        let req = CredentialRequest { request_id, connection_id: connection_id.to_owned() };
        tx.send(WorkerMsg { msg: Some(worker_msg::Msg::CredentialRequest(req)) }).await.map_err(|_| anyhow!("head connection closed"))?;

        let grant = tokio::time::timeout(CREDENTIAL_TIMEOUT, wait)
            .await
            .map_err(|_| anyhow!("timed out waiting for credentials from the head"))?
            .map_err(|_| anyhow!("head connection closed while fetching credentials"))?;
        if !grant.error.is_empty() {
            bail!("head refused credentials: {}", grant.error);
        }
        Ok(grant)
    }

    pub(crate) fn on_grant(&self, grant: CredentialGrant) {
        if grant.error.is_empty() {
            self.cache.lock().unwrap().insert(grant.connection_id.clone(), grant.clone());
        }
        if let Some(w) = self.waiters.lock().unwrap().remove(&grant.request_id) {
            let _ = w.send(grant);
        }
    }
}
