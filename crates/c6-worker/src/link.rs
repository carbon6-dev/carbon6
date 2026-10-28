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

/// The head, in the same process (docs/knowledge.md §3): the memex is part
/// of the head, so its link calls it directly instead of over the fabric.
#[async_trait::async_trait]
pub trait LocalHead: Send + Sync {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String>;
    async fn credential(&self, connection_id: &str) -> CredentialGrant;
    /// The pool a connection is, if it is one: the head spreads its calls
    /// over the pool's members.
    async fn pool(&self, _connection_id: &str) -> Option<Arc<c6_llm::pool::Pool>> {
        None
    }
}

#[derive(Default)]
pub struct HeadLink {
    /// Set when the head is in this process.
    local: std::sync::OnceLock<Arc<dyn LocalHead>>,
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
    /// A link to the head in this process.
    pub fn local(head: Arc<dyn LocalHead>) -> Self {
        let link = Self::default();
        let _ = link.local.set(head);
        link
    }

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
        if let Some(local) = self.local.get() {
            return local.call(method, params).await.map_err(|e| anyhow!(e));
        }
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
        self.local.get().is_some() || self.outbound.lock().unwrap().is_some()
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
        self.credential_with(connection_id, false).await
    }

    /// The pool a connection is, when the head is in this process and it is
    /// one (pools are only for what runs in the head).
    pub async fn pool(&self, connection_id: &str) -> Option<Arc<c6_llm::pool::Pool>> {
        self.local.get()?.pool(connection_id).await
    }

    /// New credentials, after the provider refused the ones it had (HTTP
    /// 401): the head signs in again where it can (ChatGPT).
    pub async fn fresh_credential(&self, connection_id: &str) -> anyhow::Result<CredentialGrant> {
        self.cache.lock().unwrap().remove(connection_id);
        self.credential_with(connection_id, true).await
    }

    async fn credential_with(&self, connection_id: &str, refresh: bool) -> anyhow::Result<CredentialGrant> {
        let now = c6_core::now_ms();
        let cached = self.cache.lock().unwrap().get(connection_id).cloned();
        if let Some(g) = &cached
            && (g.expires_at_ms == 0 || g.expires_at_ms - EXPIRY_MARGIN_MS > now)
        {
            return Ok(g.clone());
        }

        if let Some(local) = self.local.get() {
            let grant = local.credential(connection_id).await;
            if !grant.error.is_empty() {
                bail!("head refused credentials: {}", grant.error);
            }
            self.cache.lock().unwrap().insert(connection_id.to_owned(), grant.clone());
            return Ok(grant);
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
        let req = CredentialRequest { request_id, connection_id: connection_id.to_owned(), refresh };
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

/// A model client that takes its credentials from the head for every call
/// (cached until they near expiry), so a long turn never runs on an
/// expired token; when the provider refuses them anyway (HTTP 401), it
/// asks the head for new ones and tries once more.
pub struct LinkedClient {
    link: Arc<HeadLink>,
    connection_id: String,
    /// The client for the last bearer token, kept while the token is.
    current: Mutex<Option<(String, Arc<dyn c6_llm::ModelClient>)>>,
}

impl LinkedClient {
    pub fn new(link: Arc<HeadLink>, connection_id: &str) -> Self {
        Self { link, connection_id: connection_id.to_owned(), current: Mutex::default() }
    }

    fn client(&self, g: &CredentialGrant) -> Result<Arc<dyn c6_llm::ModelClient>, c6_llm::LlmError> {
        let mut current = self.current.lock().unwrap();
        if let Some((bearer, c)) = current.as_ref()
            && *bearer == g.bearer
        {
            return Ok(c.clone());
        }
        let c: Arc<dyn c6_llm::ModelClient> = Arc::from(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers.clone())?);
        *current = Some((g.bearer.clone(), c.clone()));
        Ok(c)
    }
}

#[async_trait::async_trait]
impl c6_llm::ModelClient for LinkedClient {
    async fn stream(&self, req: &c6_llm::ModelRequest) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        let unreachable = |e: anyhow::Error| c6_llm::LlmError::Network(format!("credentials from the head: {e:#}"));
        let g = self.link.credential(&self.connection_id).await.map_err(unreachable)?;
        match self.client(&g)?.stream(req).await {
            Err(c6_llm::LlmError::Http { status: 401, body, .. }) => {
                tracing::warn!(connection = %self.connection_id, "the provider refused the credentials; asking the head for new ones: {body}");
                let g = self.link.fresh_credential(&self.connection_id).await.map_err(unreachable)?;
                self.client(&g)?.stream(req).await
            }
            r => r,
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
            EphemeralEvent {
                session_id: "b".into(),
                kind: Some(ephemeral_event::Kind::Activity(SessionActivity { running: true, ..Default::default() })),
            },
        );
        push_merged(&mut v, text("b", 1, "y"));
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], text("a", 1, "Hello"));
    }

    /// A token the provider refuses (401) is traded for a new one from the
    /// head, and the call goes through.
    #[tokio::test]
    async fn refused_credentials_are_renewed() {
        use c6_llm::ModelClient;
        use futures::StreamExt;
        struct Head(std::sync::atomic::AtomicU32, String);
        #[async_trait::async_trait]
        impl LocalHead for Head {
            async fn call(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value, String> {
                Err("no".into())
            }
            async fn credential(&self, connection_id: &str) -> CredentialGrant {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                CredentialGrant {
                    connection_id: connection_id.into(),
                    kind: "openai_compat".into(),
                    base_url: self.1.clone(),
                    bearer: if n == 0 { "old".into() } else { "new".into() },
                    expires_at_ms: c6_core::now_ms() + 3_600_000,
                    ..Default::default()
                }
            }
        }
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|h: axum::http::HeaderMap| async move {
                if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer new") {
                    return (axum::http::StatusCode::UNAUTHORIZED, r#"{"error":{"code":"token_expired"}}"#.to_owned());
                }
                let chunks = [
                    r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                ];
                (axum::http::StatusCode::OK, chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>() + "data: [DONE]\n\n")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let head = Arc::new(Head(Default::default(), url));
        let link = Arc::new(HeadLink::local(head.clone()));
        let client = LinkedClient::new(link, "c1");
        let req = c6_llm::ModelRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![c6_llm::Message::user_text("hi")],
            tools: Vec::new(),
            max_output_tokens: None,
            cache_key: None,
            effort: None,
            web_search: false,
        };
        let mut stream = client.stream(&req).await.unwrap();
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let c6_llm::ModelEvent::TextDelta(t) = ev.unwrap() {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "hi");
        assert_eq!(head.0.load(Ordering::SeqCst), 2);
    }
}
