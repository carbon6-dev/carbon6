//! The sessions' view of the head connection: live events and credentials.
//! Works whether or not the worker is currently connected.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

fn is_session_model(session_id: &str, connection_id: &str) -> bool {
    !session_id.is_empty() && !connection_id.starts_with("secret:")
}

/// Live deltas are batched for this long, so a fast model sends a few
/// messages per frame instead of one per token.
const COALESCE: Duration = Duration::from_millis(25);

/// The head, in the same process (docs/knowledge.md §3): the memex is part
/// of the head, so its link calls it directly instead of over the fabric.
#[async_trait::async_trait]
pub trait LocalHead: Send + Sync {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String>;
    async fn credential(&self, connection_id: &str) -> CredentialGrant;
    /// Records ChatGPT reply headers on calls made in the head's process.
    fn chatgpt_limits_hook(&self, _connection_id: &str) -> Option<c6_llm::openai_responses::ChatGptLimitsHook> {
        None
    }
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
    attached: tokio::sync::Notify,
    stopped: AtomicBool,
    /// Deltas waiting for the next flush; `Some` while a flush is scheduled.
    ephemeral_buf: Arc<Mutex<Option<Vec<EphemeralEvent>>>>,
    waiters: Mutex<HashMap<u64, oneshot::Sender<CredentialGrant>>>,
    /// By (session, connection). In memory only: credentials are never
    /// written to disk on workers.
    cache: Mutex<HashMap<(String, String), CredentialGrant>>,
    /// Invalidations also discard fills that started before them.
    cache_generation: AtomicU64,
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
        let mut outbound = self.outbound.lock().unwrap();
        let mut cache = self.cache.lock().unwrap();
        // Model grants bridge a transient outage, but a new attachment
        // must go through the head's current authorization again.
        self.cache_generation.fetch_add(1, Ordering::Relaxed);
        cache.retain(|(session, id), _| !is_session_model(session, id));
        self.stopped.store(false, Ordering::Relaxed);
        *outbound = Some(tx);
        self.attached.notify_waiters();
    }

    pub(crate) fn disconnected(&self) {
        let mut outbound = self.outbound.lock().unwrap();
        *outbound = None;
        self.waiters.lock().unwrap().clear(); // waiters see a closed channel
        self.calls.lock().unwrap().clear();
    }

    pub(crate) fn stop_credentials(&self) {
        let mut cache = self.cache.lock().unwrap();
        self.cache_generation.fetch_add(1, Ordering::Relaxed);
        cache.clear();
        self.stopped.store(true, Ordering::Relaxed);
        self.attached.notify_waiters();
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

    /// Best effort now; the journal's failed receipt repeats this rejection
    /// when the head redelivers after a disconnect.
    pub(crate) fn reject_command(&self, session_id: &str, command_id: &str, reason: &str) {
        let tx = self.outbound.lock().unwrap().clone();
        if let Some(tx) = tx {
            let rejected =
                c6_proto::CommandRejected { session_id: session_id.into(), command_id: command_id.into(), reason: reason.into() };
            let _ = tx.try_send(WorkerMsg { msg: Some(worker_msg::Msg::Rejected(rejected)) });
        }
    }

    /// Automatic notice delivery waits for the head, particularly on
    /// startup when no model credentials have been cached yet.
    pub(crate) async fn wait_connected(&self) {
        loop {
            let attached = self.attached.notified();
            tokio::pin!(attached);
            attached.as_mut().enable();
            if self.is_connected() {
                return;
            }
            attached.await;
        }
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

    /// Sends what is waiting to be coalesced now. Called before an event is
    /// logged, so a reply's last streamed text never reaches the head after
    /// the message it streamed (the UI would show that text again).
    pub fn flush_ephemeral(&self) {
        let Some(events) = self.ephemeral_buf.lock().unwrap().take() else { return };
        let Some(tx) = self.outbound.lock().unwrap().clone() else { return };
        for ev in events {
            let _ = tx.try_send(WorkerMsg { msg: Some(worker_msg::Msg::Ephemeral(ev)) });
        }
    }

    /// Credentials for the head's own knowledge store (in this process). A
    /// worker's sessions ask with `session_credential`.
    pub async fn credential(&self, connection_id: &str) -> anyhow::Result<CredentialGrant> {
        self.credential_with("", connection_id, false).await
    }

    /// Credentials on behalf of one of this worker's sessions (a subagent
    /// asks as its root): the head gives only what the session uses, as its
    /// realm allows.
    pub async fn session_credential(&self, session_id: &str, connection_id: &str) -> anyhow::Result<CredentialGrant> {
        self.credential_with(session_id, connection_id, false).await
    }

    /// For an accepted model turn with no usable grant, allow a brief
    /// reconnect to the broker instead of borrowing another session's grant.
    pub(crate) async fn model_credential(&self, session_id: &str, connection_id: &str) -> anyhow::Result<CredentialGrant> {
        let result = self.session_credential(session_id, connection_id).await;
        let Some(failure) = result.as_ref().err().and_then(|e| e.downcast_ref::<CredentialTransport>()) else { return result };
        if !is_session_model(session_id, connection_id) {
            return result;
        }
        let replacement = async {
            loop {
                let attached = self.attached.notified();
                tokio::pin!(attached);
                attached.as_mut().enable();
                if self.stopped.load(Ordering::Relaxed) {
                    return false;
                }
                if self.outbound.lock().unwrap().as_ref().is_some_and(|tx| {
                    !tx.is_closed() && !failure.0.as_ref().and_then(|old| old.upgrade()).is_some_and(|old| tx.same_channel(&old))
                }) {
                    return true;
                }
                attached.await;
            }
        };
        if matches!(tokio::time::timeout(CREDENTIAL_TIMEOUT, replacement).await, Ok(true)) {
            self.session_credential(session_id, connection_id).await
        } else {
            result
        }
    }

    /// Forget grants after a connection changes in the head's process.
    pub fn forget_connection(&self, connection_id: &str) {
        let mut cache = self.cache.lock().unwrap();
        self.cache_generation.fetch_add(1, Ordering::Relaxed);
        cache.retain(|(_, id), _| id != connection_id);
    }

    /// The window for this turn or recorded model; unavailable credentials
    /// and unknown servers keep the built-in figure. Claude Code is unchanged.
    pub async fn context_window(&self, session_id: &str, model: &c6_proto::ModelSpec) -> u64 {
        let built_in = c6_llm::window::built_in(&model.connection_kind, &model.model);
        if model.connection_kind == "claude_code" {
            return built_in;
        }
        let Ok(g) = self.session_credential(session_id, &model.connection_id).await else { return built_in };
        let e = c6_llm::pool::Endpoint {
            kind: g.kind,
            base_url: g.base_url,
            bearer: g.bearer,
            headers: g.headers,
            context_window: g.context_window,
        };
        c6_llm::window::known(&model.connection_id, &model.model, &e).await.unwrap_or(built_in)
    }

    /// The pool a connection is, when the head is in this process and it is
    /// one (pools are only for what runs in the head).
    pub async fn pool(&self, connection_id: &str) -> Option<Arc<c6_llm::pool::Pool>> {
        self.local.get()?.pool(connection_id).await
    }

    /// The head's quota recorder, only when it is in this process.
    pub fn chatgpt_limits_hook(&self, connection_id: &str) -> Option<c6_llm::openai_responses::ChatGptLimitsHook> {
        self.local.get()?.chatgpt_limits_hook(connection_id)
    }

    /// New credentials, after the provider refused the ones it had (HTTP
    /// 401): the head signs in again where it can (ChatGPT).
    pub async fn fresh_credential(&self, session_id: &str, connection_id: &str) -> anyhow::Result<CredentialGrant> {
        self.forget_session_credential(session_id, connection_id);
        self.credential_with(session_id, connection_id, true).await
    }

    fn forget_session_credential(&self, session_id: &str, connection_id: &str) {
        let mut cache = self.cache.lock().unwrap();
        self.cache_generation.fetch_add(1, Ordering::Relaxed);
        cache.remove(&(session_id.to_owned(), connection_id.to_owned()));
    }

    async fn credential_with(&self, session_id: &str, connection_id: &str, refresh: bool) -> anyhow::Result<CredentialGrant> {
        let now = c6_core::now_ms();
        let key = (session_id.to_owned(), connection_id.to_owned());
        let (tx, cached, generation) = {
            let outbound = self.outbound.lock().unwrap();
            let cache = self.cache.lock().unwrap();
            let cached = if refresh { None } else { cache.get(&key).cloned() };
            (outbound.clone(), cached, self.cache_generation.load(Ordering::Relaxed))
        };
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
            self.cache_grant(key, &grant, generation);
            return Ok(grant);
        }
        let Some(tx) = tx else {
            // An expiring-but-valid token is better than nothing offline.
            if let Some(g) = cached.filter(|g| g.expires_at_ms > now) {
                return Ok(g);
            }
            return Err(CredentialTransport(None).into());
        };
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (done, wait) = oneshot::channel();
        {
            let outbound = self.outbound.lock().unwrap();
            // Do not register a request after disconnect cleared the waiters.
            if !outbound.as_ref().is_some_and(|current| current.same_channel(&tx)) {
                return self.credential_transport_failure(&key, generation, refresh, &tx);
            }
            self.waiters.lock().unwrap().insert(request_id, done);
        }
        let _cleanup = CredentialWaiter { link: self, id: request_id };
        let req = CredentialRequest { request_id, connection_id: connection_id.to_owned(), refresh, session_id: session_id.to_owned() };
        let grant = match tokio::time::timeout(CREDENTIAL_TIMEOUT, async {
            tx.send(WorkerMsg { msg: Some(worker_msg::Msg::CredentialRequest(req)) }).await.ok()?;
            wait.await.ok()
        })
        .await
        {
            Ok(Some(grant)) => grant,
            Ok(None) => return self.credential_transport_failure(&key, generation, refresh, &tx),
            Err(_) if tx.is_closed() => return self.credential_transport_failure(&key, generation, refresh, &tx),
            Err(_) => bail!("timed out waiting for credentials from the head"),
        };
        let outbound = self.outbound.lock().unwrap();
        if self.stopped.load(Ordering::Relaxed) || outbound.as_ref().is_some_and(|current| !current.same_channel(&tx)) {
            // A reply queued just before a disconnect does not authorize
            // a call once a replacement head is attached (or the link stopped).
            return Err(CredentialTransport(Some(tx.downgrade())).into());
        }
        if !grant.error.is_empty() {
            self.forget_session_credential(session_id, connection_id);
            bail!("head refused credentials: {}", grant.error);
        }
        if is_session_model(session_id, connection_id) && grant.expires_at_ms != 0 && grant.expires_at_ms <= c6_core::now_ms() {
            self.forget_session_credential(session_id, connection_id);
            bail!("head returned expired model credentials");
        }
        self.cache_grant(key, &grant, generation);
        Ok(grant)
    }

    fn credential_transport_failure(
        &self,
        key: &(String, String),
        generation: u64,
        refresh: bool,
        tx: &mpsc::Sender<WorkerMsg>,
    ) -> anyhow::Result<CredentialGrant> {
        if !refresh && is_session_model(&key.0, &key.1) {
            let cache = self.cache.lock().unwrap();
            if generation == self.cache_generation.load(Ordering::Relaxed)
                && let Some(grant) = cache.get(key)
                && grant.expires_at_ms > c6_core::now_ms()
            {
                return Ok(grant.clone());
            }
        }
        Err(CredentialTransport(Some(tx.downgrade())).into())
    }

    fn cache_grant(&self, key: (String, String), grant: &CredentialGrant, generation: u64) {
        let mut cache = self.cache.lock().unwrap();
        if self.cache_generation.load(Ordering::Relaxed) == generation {
            cache.insert(key, grant.clone());
        }
    }

    pub(crate) fn on_grant(&self, grant: CredentialGrant) {
        if let Some(w) = self.waiters.lock().unwrap().remove(&grant.request_id) {
            let _ = w.send(grant);
        }
    }
}

struct CredentialWaiter<'a> {
    link: &'a HeadLink,
    id: u64,
}

impl Drop for CredentialWaiter<'_> {
    fn drop(&mut self) {
        self.link.waiters.lock().unwrap().remove(&self.id);
    }
}

#[derive(Debug)]
struct CredentialTransport(Option<mpsc::WeakSender<WorkerMsg>>);

impl std::fmt::Display for CredentialTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "head connection closed while fetching credentials"
        } else {
            "not connected to the head and no cached credentials for this connection"
        })
    }
}

impl std::error::Error for CredentialTransport {}

/// A model client that takes its credentials from the head for every call
/// (cached until they near expiry), so a long turn never runs on an
/// expired token; when the provider refuses them anyway (HTTP 401), it
/// asks the head for new ones and tries once more.
pub struct LinkedClient {
    link: Arc<HeadLink>,
    session_id: String,
    connection_id: String,
    /// The client for the last grant's provider configuration.
    current: Mutex<Option<(CredentialGrant, Arc<dyn c6_llm::ModelClient>)>>,
}

impl LinkedClient {
    /// For `session_id`'s model calls (its root's, for a subagent).
    pub fn new(link: Arc<HeadLink>, session_id: &str, connection_id: &str) -> Self {
        Self { link, session_id: session_id.to_owned(), connection_id: connection_id.to_owned(), current: Mutex::default() }
    }

    fn client(&self, g: &CredentialGrant) -> Result<Arc<dyn c6_llm::ModelClient>, c6_llm::LlmError> {
        let mut current = self.current.lock().unwrap();
        if let Some((previous, c)) = current.as_ref()
            && previous.kind == g.kind
            && previous.base_url == g.base_url
            && previous.bearer == g.bearer
            && previous.headers == g.headers
        {
            return Ok(c.clone());
        }
        let c: Arc<dyn c6_llm::ModelClient> = Arc::from(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers.clone())?);
        *current = Some((g.clone(), c.clone()));
        Ok(c)
    }
}

#[async_trait::async_trait]
impl c6_llm::ModelClient for LinkedClient {
    async fn stream(&self, req: &c6_llm::ModelRequest) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        let unreachable = |e: anyhow::Error| c6_llm::LlmError::Network(format!("credentials from the head: {e:#}"));
        let g = self.link.model_credential(&self.session_id, &self.connection_id).await.map_err(unreachable)?;
        match self.client(&g)?.stream(req).await {
            Err(c6_llm::LlmError::Http { status: 401, body, .. }) => {
                tracing::warn!(connection = %self.connection_id, "the provider refused the credentials; asking the head for new ones: {body}");
                let g = self.link.fresh_credential(&self.session_id, &self.connection_id).await.map_err(unreachable)?;
                let retried = self.client(&g)?.stream(req).await;
                if matches!(&retried, Err(c6_llm::LlmError::Http { status: 401, .. })) {
                    self.link.forget_session_credential(&self.session_id, &self.connection_id);
                }
                retried
            }
            r => r,
        }
    }
}

/// Appends `ev`, merging it into the previous event when both are text for
/// the same stream (same session, kind, turn and place in the log, or tool
/// call).
fn push_merged(pending: &mut Vec<EphemeralEvent>, ev: EphemeralEvent) {
    use ephemeral_event::Kind;
    if let Some(last) = pending.last_mut()
        && last.session_id == ev.session_id
    {
        match (&mut last.kind, ev.kind.as_ref()) {
            (Some(Kind::Text(a)), Some(Kind::Text(b))) | (Some(Kind::Thinking(a)), Some(Kind::Thinking(b)))
                if a.turn == b.turn && a.seq == b.seq =>
            {
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
        EphemeralEvent {
            session_id: session.into(),
            kind: Some(ephemeral_event::Kind::Text(TextDelta { turn, text: t.into(), ..Default::default() })),
        }
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

    /// Text waiting to be coalesced goes out before a logged event, on the
    /// same channel, so the head never gets a reply's last text after the
    /// reply.
    #[tokio::test]
    async fn waiting_text_is_sent_before_what_comes_next() {
        let link = HeadLink::default();
        let (tx, mut rx) = mpsc::channel(8);
        link.connected(tx);
        link.ephemeral(text("a", 1, "Hel"));
        link.ephemeral(text("a", 1, "lo"));
        assert!(rx.try_recv().is_err(), "coalescing: nothing yet");
        link.flush_ephemeral();
        let Some(worker_msg::Msg::Ephemeral(ev)) = rx.try_recv().unwrap().msg else { panic!("not the text") };
        assert_eq!(ev, text("a", 1, "Hello"));
        // The coalescing timer finds nothing left to send.
        link.flush_ephemeral();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn invalidation_discards_a_local_credential_fill_in_flight() {
        struct Head {
            window: AtomicU64,
            calls: AtomicU64,
            started: tokio::sync::Notify,
            resume: tokio::sync::Notify,
        }
        #[async_trait::async_trait]
        impl LocalHead for Head {
            async fn call(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value, String> {
                Err("unused".into())
            }
            async fn credential(&self, connection_id: &str) -> CredentialGrant {
                let context_window = self.window.load(Ordering::Relaxed);
                if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.started.notify_one();
                    self.resume.notified().await;
                }
                CredentialGrant { connection_id: connection_id.into(), context_window, ..Default::default() }
            }
        }
        let head = Arc::new(Head {
            window: AtomicU64::new(16_384),
            calls: AtomicU64::new(0),
            started: Default::default(),
            resume: Default::default(),
        });
        let link = Arc::new(HeadLink::local(head.clone()));
        let pending = {
            let link = link.clone();
            tokio::spawn(async move { link.credential("c").await.unwrap() })
        };
        c6_testkit::within("the old credential fetch to start", head.started.notified()).await;
        head.window.store(32_768, Ordering::Relaxed);
        link.forget_connection("c");
        // Cache the new grant before allowing the old fetch to finish.
        assert_eq!(link.credential("c").await.unwrap().context_window, 32_768);
        head.resume.notify_one();
        assert_eq!(c6_testkit::within("the old credential fetch to finish", pending).await.unwrap().context_window, 16_384);
        assert_eq!(link.credential("c").await.unwrap().context_window, 32_768);
        assert_eq!(head.calls.load(Ordering::Relaxed), 2);
    }

    fn take_request(rx: &mut mpsc::Receiver<WorkerMsg>) -> CredentialRequest {
        let Some(worker_msg::Msg::CredentialRequest(req)) = rx.try_recv().unwrap().msg else { panic!("expected credential request") };
        req
    }

    fn attach(link: &HeadLink) -> mpsc::Receiver<WorkerMsg> {
        let (tx, rx) = mpsc::channel(8);
        link.connected(tx);
        rx
    }

    fn seed_grant(link: &HeadLink, expires_at_ms: i64) -> (String, String) {
        let key = ("root".to_owned(), "c".to_owned());
        link.cache.lock().unwrap().insert(key.clone(), CredentialGrant { bearer: "old".into(), expires_at_ms, ..Default::default() });
        key
    }

    #[tokio::test]
    async fn model_cache_bridges_disconnect_but_reconnect_requires_authorization() {
        let link = HeadLink::default();
        let mut rx = attach(&link);
        let grant = link.model_credential("root", "c");
        tokio::pin!(grant);
        assert!(futures::poll!(&mut grant).is_pending());
        let req = take_request(&mut rx);
        assert_eq!((req.session_id.as_str(), req.connection_id.as_str()), ("root", "c"));
        link.on_grant(CredentialGrant { request_id: req.request_id, bearer: "initial".into(), ..Default::default() });
        assert_eq!(grant.await.unwrap().bearer, "initial");

        link.disconnected();
        assert_eq!(link.model_credential("root", "c").await.unwrap().bearer, "initial");
        assert!(link.session_credential("other", "c").await.is_err());
        assert!(link.session_credential("root", "other").await.is_err());

        let mut rx = attach(&link);
        let grant = link.model_credential("root", "c");
        tokio::pin!(grant);
        assert!(futures::poll!(&mut grant).is_pending());
        let req = take_request(&mut rx);
        link.on_grant(CredentialGrant { request_id: req.request_id, error: "no longer authorized".into(), ..Default::default() });
        assert_eq!(grant.await.unwrap_err().to_string(), "head refused credentials: no longer authorized");
        link.disconnected();
        assert!(link.session_credential("root", "c").await.is_err());
    }

    #[tokio::test]
    async fn cold_model_grant_waits_for_reattach_and_resends_if_the_stream_drops() {
        let link = HeadLink::default();
        let first = link.model_credential("root", "c");
        tokio::pin!(first);
        assert!(futures::poll!(&mut first).is_pending());
        let mut rx = attach(&link);
        assert!(futures::poll!(&mut first).is_pending());
        let req = take_request(&mut rx);
        link.on_grant(CredentialGrant { request_id: req.request_id, bearer: "first".into(), ..Default::default() });
        assert_eq!(first.await.unwrap().bearer, "first");

        let second = link.model_credential("root", "other");
        tokio::pin!(second);
        assert!(futures::poll!(&mut second).is_pending());
        let old = take_request(&mut rx);
        link.disconnected();
        assert!(futures::poll!(&mut second).is_pending());
        let mut rx = attach(&link);
        assert!(futures::poll!(&mut second).is_pending());
        let req = take_request(&mut rx);
        assert_ne!(old.request_id, req.request_id);
        assert_eq!((req.session_id.as_str(), req.connection_id.as_str()), ("root", "other"));
        link.on_grant(CredentialGrant { request_id: old.request_id, bearer: "obsolete".into(), ..Default::default() });
        assert!(futures::poll!(&mut second).is_pending());
        link.on_grant(CredentialGrant { request_id: req.request_id, bearer: "replacement".into(), ..Default::default() });
        assert_eq!(second.await.unwrap().bearer, "replacement");
    }

    #[tokio::test]
    async fn model_refresh_transport_failure_uses_only_an_unexpired_grant() {
        let link = HeadLink::default();
        let mut rx = attach(&link);
        let expires_at_ms = c6_core::now_ms() + EXPIRY_MARGIN_MS;
        let key = seed_grant(&link, expires_at_ms);

        let grant = link.model_credential("root", "c");
        tokio::pin!(grant);
        assert!(futures::poll!(&mut grant).is_pending());
        take_request(&mut rx);
        link.disconnected();
        assert_eq!(grant.await.unwrap().expires_at_ms, expires_at_ms);
        link.cache.lock().unwrap().get_mut(&key).unwrap().expires_at_ms = c6_core::now_ms() - 1;
        assert!(link.session_credential("root", "c").await.is_err());

        link.cache.lock().unwrap().get_mut(&key).unwrap().expires_at_ms = 0;
        assert!(link.fresh_credential("root", "c").await.is_err(), "a 401 must never reuse the rejected bearer");
        assert!(link.session_credential("root", "c").await.is_err());
    }

    #[tokio::test]
    async fn refusal_revokes_a_still_valid_model_grant_in_the_cache() {
        let link = HeadLink::default();
        let mut rx = attach(&link);
        seed_grant(&link, c6_core::now_ms() + EXPIRY_MARGIN_MS);
        let grant = link.model_credential("root", "c");
        tokio::pin!(grant);
        assert!(futures::poll!(&mut grant).is_pending());
        let req = take_request(&mut rx);
        link.on_grant(CredentialGrant { request_id: req.request_id, error: "denied".into(), ..Default::default() });
        assert_eq!(grant.await.unwrap_err().to_string(), "head refused credentials: denied");
        link.disconnected();
        assert!(link.session_credential("root", "c").await.is_err());
    }

    #[tokio::test]
    async fn stopping_credentials_clears_grants_and_releases_model_waiters() {
        let link = HeadLink::default();
        seed_grant(&link, 0);
        let grant = link.model_credential("root", "other");
        tokio::pin!(grant);
        assert!(futures::poll!(&mut grant).is_pending());
        link.stop_credentials();
        assert!(grant.await.is_err());
        assert!(link.session_credential("root", "c").await.is_err());
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
        let client = LinkedClient::new(link, "s1", "c1");
        let req = c6_llm::ModelRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![c6_llm::Message::user_text("hi")],
            tools: Vec::new(),
            max_output_tokens: None,
            cache_key: None,
            effort: None,
            web_search: false,
            json: false,
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
