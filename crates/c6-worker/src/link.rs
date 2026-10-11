//! The sessions' view of the head connection: live events and credentials.
//! Works whether or not the worker is currently connected.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use c6_proto::{CredentialGrant, CredentialRequest, EphemeralEvent, WorkerMsg, ephemeral_event, worker_msg};
use tokio::sync::{mpsc, oneshot};

const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Calls to the head (and through it, to the memex).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Refresh credentials this long before they expire.
const EXPIRY_MARGIN_MS: i64 = 60_000;

/// Live deltas are batched for this long, so a fast model sends a few
/// messages per frame instead of one per token.
const COALESCE: Duration = Duration::from_millis(25);

#[derive(Default)]
pub struct HeadLink {
    outbound: Mutex<Option<mpsc::Sender<WorkerMsg>>>,
    /// Deltas waiting for the next flush; `Some` while a flush is scheduled.
    ephemeral_buf: Arc<Mutex<Option<Vec<EphemeralEvent>>>>,
    waiters: Mutex<HashMap<u64, oneshot::Sender<CredentialGrant>>>,
    /// In memory only: credentials are never written to disk on workers.
    cache: Mutex<HashMap<String, CredentialGrant>>,
    next_request: AtomicU64,
    calls: Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>,
}

impl HeadLink {
    pub(crate) fn connected(&self, tx: mpsc::Sender<WorkerMsg>) {
        *self.outbound.lock().unwrap() = Some(tx);
    }

    pub(crate) fn disconnected(&self) {
        *self.outbound.lock().unwrap() = None;
        self.waiters.lock().unwrap().clear(); // waiters see a closed channel
        self.calls.lock().unwrap().clear();
    }

    /// Calls the head: `method` with JSON `params`. What a node may call
    /// depends on its role (see the head's `fabric`).
    pub async fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let tx = self.outbound.lock().unwrap().clone().ok_or_else(|| anyhow!("not connected to the head"))?;
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (done, wait) = oneshot::channel();
        self.calls.lock().unwrap().insert(id, done);
        let call = c6_proto::HeadCall { id, method: method.to_owned(), params_json: serde_json::to_vec(&params)? };
        tx.send(WorkerMsg { msg: Some(worker_msg::Msg::HeadCall(call)) }).await.map_err(|_| anyhow!("head connection closed"))?;
        match tokio::time::timeout(CALL_TIMEOUT, wait).await {
            Ok(Ok(r)) => r.map_err(|e| anyhow!(e)),
            Ok(Err(_)) => bail!("head connection closed during {method}"),
            Err(_) => {
                self.calls.lock().unwrap().remove(&id);
                bail!("no answer from the head to {method} in {}s", CALL_TIMEOUT.as_secs())
            }
        }
    }

    pub(crate) fn on_call_reply(&self, reply: c6_proto::HeadCallReply) {
        if let Some(w) = self.calls.lock().unwrap().remove(&reply.id) {
            let r = if reply.error.is_empty() {
                serde_json::from_slice(&reply.result_json).map_err(|e| format!("bad reply from the head: {e}"))
            } else {
                Err(reply.error)
            };
            let _ = w.send(r);
        }
    }

    pub fn is_connected(&self) -> bool {
        self.outbound.lock().unwrap().is_some()
    }

    /// Best effort: dropped when disconnected or backed up. Consecutive
    /// deltas for the same stream are merged before sending.
    pub fn ephemeral(&self, ev: EphemeralEvent) {
        let Some(tx) = self.outbound.lock().unwrap().clone() else { return };
        let mut buf = self.ephemeral_buf.lock().unwrap();
        match buf.as_mut() {
            Some(pending) => push_merged(pending, ev),
            None => {
                *buf = Some(vec![ev]);
                let shared = self.ephemeral_buf.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(COALESCE).await;
                    let events = shared.lock().unwrap().take().unwrap_or_default();
                    for ev in events {
                        let _ = tx.try_send(WorkerMsg { msg: Some(worker_msg::Msg::Ephemeral(ev)) });
                    }
                });
            }
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

/// Appends `ev`, merging it into the previous event when both are text for
/// the same stream (same session, kind and turn or tool call).
fn push_merged(pending: &mut Vec<EphemeralEvent>, ev: EphemeralEvent) {
    use ephemeral_event::Kind;
    if let Some(last) = pending.last_mut()
        && last.session_id == ev.session_id
    {
        match (&mut last.kind, ev.kind.as_ref()) {
            (Some(Kind::Text(a)), Some(Kind::Text(b))) | (Some(Kind::Thinking(a)), Some(Kind::Thinking(b))) if a.turn == b.turn => {
                a.text.push_str(&b.text);
                return;
            }
            (Some(Kind::ToolOutput(a)), Some(Kind::ToolOutput(b))) if a.call_id == b.call_id => {
                a.text.push_str(&b.text);
                return;
            }
            _ => {}
        }
    }
    pending.push(ev);
}

#[cfg(test)]
mod tests {
    use c6_proto::{SessionActivity, TextDelta};

    use super::*;

    fn text(session: &str, turn: u64, t: &str) -> EphemeralEvent {
        EphemeralEvent { session_id: session.into(), kind: Some(ephemeral_event::Kind::Text(TextDelta { turn, text: t.into() })) }
    }

    #[test]
    fn merges_consecutive_deltas_of_one_stream() {
        let mut v = Vec::new();
        push_merged(&mut v, text("a", 1, "Hel"));
        push_merged(&mut v, text("a", 1, "lo"));
        push_merged(&mut v, text("b", 1, "x"));
        push_merged(
            &mut v,
            EphemeralEvent { session_id: "b".into(), kind: Some(ephemeral_event::Kind::Activity(SessionActivity { running: true })) },
        );
        push_merged(&mut v, text("b", 1, "y"));
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], text("a", 1, "Hello"));
    }
}
