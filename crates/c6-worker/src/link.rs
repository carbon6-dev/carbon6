//! The sessions' view of the head connection: live events and credentials.
//! Works whether or not the worker is currently connected.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use c6_proto::{CredentialGrant, CredentialRequest, EphemeralEvent, WorkerMsg, ephemeral_event, worker_msg};
use futures::StreamExt;
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
    async fn realm_credential(&self, _realm: &str, connection_id: &str) -> CredentialGrant {
        self.credential(connection_id).await
    }
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
    // A turn recreates its model client; the actual caller's private
    // provider history stays known for as long as this worker does.
    alias_history: Arc<c6_llm::replay::AliasHistory>,
    /// Invalidations also discard fills that started before them.
    cache_generation: AtomicU64,
    next_request: AtomicU64,
    calls: Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>,
}

struct CallRegistration<'a> {
    link: &'a HeadLink,
    id: u64,
}

impl Drop for CallRegistration<'_> {
    fn drop(&mut self) {
        self.link.calls.lock().unwrap().remove(&self.id);
    }
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
        let call = c6_proto::HeadCall { id, method: method.to_owned(), params_json: serde_json::to_vec(&params)? };
        let (done, wait) = oneshot::channel();
        self.calls.lock().unwrap().insert(id, done);
        // A caller can cancel either the send or the reply wait. Replies and
        // disconnects may have already removed the record when we drop it.
        let _registration = CallRegistration { link: self, id };
        tx.send(WorkerMsg { msg: Some(worker_msg::Msg::HeadCall(call)) }).await.map_err(|_| anyhow!("head connection closed"))?;
        match tokio::time::timeout(CALL_TIMEOUT, wait).await {
            Ok(Ok(r)) => r.map_err(|e| anyhow!(e)),
            Ok(Err(_)) => bail!("head connection closed during {method}"),
            Err(_) => bail!("no answer from the head to {method} in {}s", CALL_TIMEOUT.as_secs()),
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

    /// Legacy epoch-zero rejection.
    #[allow(dead_code)]
    pub(crate) fn reject_command(&self, session_id: &str, command_id: &str, reason: &str) {
        self.reject_owned(session_id, command_id, reason, 0);
    }

    /// Best effort now; the journal's failed receipt repeats this rejection
    /// when the head redelivers after a disconnect.
    pub(crate) fn reject_owned(&self, session_id: &str, command_id: &str, reason: &str, ownership_epoch: u64) {
        let tx = self.outbound.lock().unwrap().clone();
        if let Some(tx) = tx {
            let rejected = c6_proto::CommandRejected {
                session_id: session_id.into(),
                command_id: command_id.into(),
                reason: reason.into(),
                ownership_epoch,
            };
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

    /// Refresh only the concrete account that wrote this prefix. Never resolve
    /// an alias, run replay preparation, or fall back to another account.
    pub async fn keep_warm(
        &self,
        session_id: &str,
        snapshot: &crate::journal::KeepWarmSnapshot,
        text: &str,
    ) -> anyhow::Result<c6_proto::Usage> {
        tokio::time::timeout(CALL_TIMEOUT, self.keep_warm_inner(session_id, snapshot, text, None))
            .await
            .map_err(|_| anyhow!("keep-warm call timed out"))?
    }

    /// Cache freshness bounds credential refresh too, not just dispatch. The
    /// actor computes this monotonic deadline from the prefix's remaining TTL.
    pub async fn keep_warm_until(
        &self,
        session_id: &str,
        snapshot: &crate::journal::KeepWarmSnapshot,
        text: &str,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<c6_proto::Usage> {
        let limit = deadline.min(tokio::time::Instant::now() + CALL_TIMEOUT);
        let deadline = crate::keep_warm::CacheDeadline::new(deadline);
        tokio::time::timeout_at(limit, self.keep_warm_inner(session_id, snapshot, text, Some(deadline)))
            .await
            .map_err(|_| anyhow!("keep-warm call timed out before cache expiry"))?
    }

    async fn keep_warm_inner(
        &self,
        session_id: &str,
        snapshot: &crate::journal::KeepWarmSnapshot,
        text: &str,
        deadline: Option<crate::keep_warm::CacheDeadline>,
    ) -> anyhow::Result<c6_proto::Usage> {
        anyhow::ensure!(deadline.is_none_or(|deadline| !deadline.expired()), "keep-warm cache already expired");
        let response = snapshot.response.as_ref().ok_or_else(|| anyhow!("keep-warm snapshot has no completed assistant response"))?;
        let g = self.fresh_credential(session_id, &snapshot.connection_id).await?;
        anyhow::ensure!(
            !g.is_alias
                && g.connection_id == snapshot.connection_id
                && (g.resolved_connection_id.is_empty() || g.resolved_connection_id == snapshot.connection_id),
            "keep-warm target is no longer the recorded concrete connection"
        );
        anyhow::ensure!(g.kind == snapshot.kind && g.base_url == snapshot.base_url, "keep-warm provider configuration changed");
        let mut client = c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers)?;
        if g.kind == "chatgpt"
            && let Some(hook) = self.chatgpt_limits_hook(&snapshot.connection_id)
        {
            client.set_chatgpt_limits_hook(hook);
        }
        let mut request = snapshot.request.clone();
        request.messages.push(response.clone());
        request.messages.push(c6_llm::Message::user_text(text));
        request.max_output_tokens = Some(1);
        anyhow::ensure!(deadline.is_none_or(|deadline| !deadline.expired()), "keep-warm cache expired during credential refresh");
        let mut stream = client.stream(&request).await?;
        let mut usage = c6_llm::Usage::default();
        while let Some(event) = stream.next().await {
            if let c6_llm::ModelEvent::Usage(observed) = event? {
                usage = observed;
            }
        }
        Ok(c6_proto::Usage {
            model: request.model,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            resolved_connection_id: snapshot.connection_id.clone(),
            resolved_connection_name: g.resolved_connection_name,
            keep_warm: true,
            ..Default::default()
        })
    }

    /// Credentials for a realm's in-process knowledge work. Authorize each
    /// request as that realm rather than sharing the worker credential cache.
    pub async fn realm_credential(&self, realm: &str, connection_id: &str) -> anyhow::Result<CredentialGrant> {
        let Some(local) = self.local.get() else { return self.credential(connection_id).await };
        let grant = local.realm_credential(realm, connection_id).await;
        anyhow::ensure!(grant.error.is_empty(), "{}", grant.error);
        Ok(grant)
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
        let resolved = if g.resolved_connection_id.is_empty() { &model.connection_id } else { &g.resolved_connection_id };
        let built_in = c6_llm::window::built_in(&g.kind, &model.model);
        let e = c6_llm::pool::Endpoint {
            kind: g.kind,
            base_url: g.base_url,
            bearer: g.bearer,
            headers: g.headers,
            context_window: g.context_window,
        };
        c6_llm::window::known(resolved, &model.model, &e).await.unwrap_or(built_in)
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
        let req = CredentialRequest {
            request_id,
            connection_id: connection_id.to_owned(),
            refresh,
            session_id: session_id.to_owned(),
            supports_alias: true,
        };
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
            if grant.is_alias {
                // Resolve again on the next request, even offline: reusing
                // the old account could bypass a switch or an access change.
                cache.remove(&key);
            } else {
                cache.insert(key, grant.clone());
            }
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
/// (cached until they near expiry, except for aliases), so a long turn never runs on an
/// expired token; when the provider refuses them anyway (HTTP 401), it
/// asks the head for new ones and tries once more.
pub struct LinkedClient {
    link: Arc<HeadLink>,
    session_id: String,
    connection_id: String,
    activity: Option<Arc<crate::session::Writer>>,
    /// The client for the last grant's provider configuration.
    current: Mutex<Option<(CredentialGrant, Arc<dyn c6_llm::ModelClient>)>>,
}

impl LinkedClient {
    /// For `session_id`'s model calls (its root's, for a subagent).
    pub fn new(link: Arc<HeadLink>, session_id: &str, connection_id: &str) -> Self {
        Self { link, session_id: session_id.to_owned(), connection_id: connection_id.to_owned(), activity: None, current: Mutex::default() }
    }

    pub(crate) fn track_activity(mut self, writer: &Arc<crate::session::Writer>) -> Self {
        self.activity = Some(writer.clone());
        self
    }

    fn client(&self, g: &CredentialGrant) -> Result<Arc<dyn c6_llm::ModelClient>, c6_llm::LlmError> {
        let mut current = self.current.lock().unwrap();
        if let Some((previous, c)) = current.as_ref()
            && previous.kind == g.kind
            && previous.resolved_connection_id == g.resolved_connection_id
            && previous.base_url == g.base_url
            && previous.bearer == g.bearer
            && previous.headers == g.headers
        {
            return Ok(c.clone());
        }
        let mut client = c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers.clone())?;
        if g.kind == "chatgpt" {
            let resolved = if g.resolved_connection_id.is_empty() { &self.connection_id } else { &g.resolved_connection_id };
            if let Some(hook) = self.link.chatgpt_limits_hook(resolved) {
                client.set_chatgpt_limits_hook(hook);
            }
        }
        let c: Arc<dyn c6_llm::ModelClient> = Arc::from(client);
        *current = Some((g.clone(), c.clone()));
        Ok(c)
    }

    async fn stream_with_grant(&self, req: &c6_llm::ModelRequest, g: &CredentialGrant) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        let client = self.client(g)?;
        let call_ms = self.activity.as_ref().map_or(0, |writer| writer.now_ms());
        if !g.is_alias {
            return self.capture_request(req, g, client.stream(req).await?, call_ms).await;
        }
        let mut req = req.clone();
        let replay = self.link.alias_history.prepare(&mut req, &self.connection_id, &g.kind, &g.resolved_connection_id);
        if g.kind == "chatgpt"
            && let Some(key) = req.cache_key.as_deref()
        {
            // Separate the backend's conversation affinity and prompt cache
            // by real account. The key still identifies the actual caller
            // (not the root that authorized a subagent's grant).
            req.cache_key = Some(format!("{key}:{}", g.resolved_connection_id));
        }
        let stream = self.capture_request(&req, g, replay.track(client.stream(&req).await?), call_ms).await?;
        let account = c6_llm::ModelAccount {
            resolved_connection_id: g.resolved_connection_id.clone(),
            resolved_connection_name: g.resolved_connection_name.clone(),
            alias_connection_name: g.alias_connection_name.clone(),
        };
        Ok(futures::stream::iter([Ok(c6_llm::ModelEvent::Account(account))]).chain(stream).boxed())
    }

    /// Called only after all provider/account transformations, with the exact
    /// request the concrete client accepted. The writer belongs to the actual
    /// caller (including children), not the root authorizing its credentials.
    async fn capture_request(
        &self,
        req: &c6_llm::ModelRequest,
        grant: &CredentialGrant,
        stream: c6_llm::EventStream,
        call_ms: i64,
    ) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        let Some(writer) = self.activity.clone() else { return Ok(stream) };
        if !writer.keep_warm_capture().0 {
            return Ok(stream);
        }
        let snapshot = crate::journal::KeepWarmSnapshot {
            request: req.clone(),
            response: None,
            connection_id: if grant.resolved_connection_id.is_empty() {
                self.connection_id.clone()
            } else {
                grant.resolved_connection_id.clone()
            },
            kind: grant.kind.clone(),
            base_url: grant.base_url.clone(),
            last_call_ms: call_ms,
            usage: c6_llm::Usage::default(),
        };
        let generation = writer
            .journal
            .reserve_keep_warm_snapshot(&writer.session_id, &snapshot.connection_id, snapshot.last_call_ms, writer.ownership_epoch)
            .await
            .map_err(|e| c6_llm::LlmError::Config(format!("reserving keep-warm request: {e:#}")))?;
        Ok(futures::stream::unfold(
            (stream, writer, snapshot, c6_llm::Accumulator::default(), generation, false, false),
            |(mut stream, writer, mut snapshot, mut accumulator, generation, mut usage_seen, mut persisted)| async move {
                let event = stream.next().await?;
                let recorded = async {
                    if let Ok(model_event) = &event {
                        if snapshot.response.is_none() {
                            accumulator.push(model_event);
                        }
                        match model_event {
                            c6_llm::ModelEvent::Usage(usage) => {
                                snapshot.usage = *usage;
                                usage_seen = true;
                            }
                            c6_llm::ModelEvent::Done { .. } if snapshot.response.is_none() => {
                                snapshot.response = Some(std::mem::take(&mut accumulator).finish(true));
                            }
                            _ => {}
                        }
                        // Serialize the full input/output once, and only after
                        // real usage proves this complete context is eligible.
                        if !persisted && usage_seen && snapshot.response.is_some() {
                            let (enabled, threshold) = writer.keep_warm_capture();
                            if enabled && crate::keep_warm::context_tokens(&snapshot.usage) >= threshold {
                                writer
                                    .journal
                                    .complete_keep_warm_snapshot(&writer.session_id, &generation, &snapshot, writer.ownership_epoch)
                                    .await
                                    .map_err(|e| c6_llm::LlmError::Config(format!("saving keep-warm output: {e:#}")))?;
                                persisted = true;
                            }
                        }
                    } else {
                        writer
                            .journal
                            .invalidate_keep_warm_generation(&writer.session_id, &generation, writer.ownership_epoch)
                            .await
                            .map_err(|e| c6_llm::LlmError::Config(format!("invalidating keep-warm request: {e:#}")))?;
                    }
                    Ok(())
                }
                .await;
                Some((recorded.and(event), (stream, writer, snapshot, accumulator, generation, usage_seen, persisted)))
            },
        )
        .boxed())
    }
}

#[async_trait::async_trait]
impl c6_llm::ModelClient for LinkedClient {
    async fn stream(&self, req: &c6_llm::ModelRequest) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        if let Some(writer) = &self.activity {
            writer.set_activity(c6_proto::SessionActivityKind::Model);
            writer
                .journal
                .clear_keep_warm_snapshot(&writer.session_id, writer.ownership_epoch)
                .await
                .map_err(|e| c6_llm::LlmError::Config(format!("invalidating keep-warm request: {e:#}")))?;
        }
        let unreachable = |e: anyhow::Error| c6_llm::LlmError::Network(format!("credentials from the head: {e:#}"));
        let g = self.link.model_credential(&self.session_id, &self.connection_id).await.map_err(unreachable)?;
        match self.stream_with_grant(req, &g).await {
            Err(c6_llm::LlmError::Http { status: 401, body, .. }) => {
                tracing::warn!(connection = %self.connection_id, "the provider refused the credentials; asking the head for new ones: {body}");
                let g = self.link.fresh_credential(&self.session_id, &self.connection_id).await.map_err(unreachable)?;
                let retried = self.stream_with_grant(req, &g).await;
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
/// the same stream (same session incarnation, kind, turn and place in the
/// log, or tool call).
fn push_merged(pending: &mut Vec<EphemeralEvent>, ev: EphemeralEvent) {
    use ephemeral_event::Kind;
    if let Some(last) = pending.last_mut()
        && last.session_id == ev.session_id
        && last.ownership_epoch == ev.ownership_epoch
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

    #[tokio::test]
    async fn cancelled_head_call_removes_registration_during_send_and_reply_wait() {
        for blocked_send in [true, false] {
            let link = HeadLink::default();
            let (tx, mut rx) = mpsc::channel(1);
            if blocked_send {
                tx.try_send(WorkerMsg::default()).unwrap();
            }
            link.connected(tx);
            let mut call = Box::pin(link.call("test", serde_json::Value::Null));
            assert!(futures::poll!(call.as_mut()).is_pending());
            assert_eq!(link.calls.lock().unwrap().len(), 1);
            assert_eq!(rx.try_recv().unwrap().msg.is_none(), blocked_send);
            drop(call);
            assert!(link.calls.lock().unwrap().is_empty());
            assert!(rx.try_recv().is_err());
        }
    }

    fn text(session: &str, turn: u64, t: &str) -> EphemeralEvent {
        EphemeralEvent {
            session_id: session.into(),
            kind: Some(ephemeral_event::Kind::Text(TextDelta { turn, text: t.into(), ..Default::default() })),
            ownership_epoch: 0,
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
                ownership_epoch: 0,
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
        assert!(req.supports_alias, "the worker must advertise that it won't cache alias grants");
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

    #[tokio::test]
    async fn keep_warm_reuses_saved_transformed_alias_prefix_after_restart() {
        use c6_llm::ModelClient;
        use serde_json::Value;

        struct Head {
            url: String,
            grants: Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl LocalHead for Head {
            async fn call(&self, _: &str, _: Value) -> Result<Value, String> {
                Err("unused".into())
            }
            async fn credential(&self, connection_id: &str) -> CredentialGrant {
                self.grants.lock().unwrap().push(connection_id.into());
                CredentialGrant {
                    connection_id: connection_id.into(),
                    resolved_connection_id: "account".into(),
                    is_alias: connection_id == "alias",
                    kind: "chatgpt".into(),
                    base_url: self.url.clone(),
                    bearer: "DO-NOT-PERSIST".into(),
                    ..Default::default()
                }
            }
        }
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let app = axum::Router::new().route("/responses", axum::routing::post({
            let requests = requests.clone();
            move |axum::Json(body): axum::Json<Value>| {
                let n = {
                    let mut requests = requests.lock().unwrap();
                    requests.push(body);
                    requests.len()
                };
                async move {
                    let reasoning = serde_json::json!({
                        "type": "response.output_item.done", "output_index": 0,
                        "item": {"type": "reasoning", "id": format!("rs_{n}"), "encrypted_content": format!("private-prefix-{n}"), "summary": []}
                    });
                    let message = serde_json::json!({
                        "type": "response.output_item.done", "output_index": 1,
                        "item": {"type": "message", "role": "assistant", "content": [{"type":"output_text", "text":format!("final assistant output {n}")}]}
                    });
                    let completed = serde_json::json!({
                        "type": "response.completed", "response": {"usage": {"input_tokens":70000, "input_tokens_details":{"cached_tokens":69000}, "output_tokens":1}}
                    });
                    ([("content-type", "text/event-stream")], format!("data: {reasoning}\n\ndata: {message}\n\ndata: {completed}\n\n"))
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let head = Arc::new(Head { url: format!("http://{}", listener.local_addr().unwrap()), grants: Default::default() });
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = Arc::new(crate::journal::Journal::open(&path).await.unwrap());
        journal.import("child", &c6_proto::CreateSession::default(), &[]).await.unwrap();
        let link = Arc::new(HeadLink::local(head.clone()));
        let writer = Arc::new(crate::session::Writer::new("child".into(), journal.clone(), link.clone(), 1));
        let client = LinkedClient::new(link.clone(), "root", "alias").track_activity(&writer);
        let mut request = c6_llm::ModelRequest {
            model: "m".into(),
            system: "unchanged system".into(),
            messages: vec![c6_llm::Message::user_text("hi")],
            tools: vec![c6_llm::ToolSpec {
                name: "lookup".into(),
                description: "unchanged tool prefix".into(),
                parameters: serde_json::json!({"type":"object", "properties":{"q":{"type":"string"}}}),
            }],
            max_output_tokens: None,
            cache_key: Some("child".into()),
            effort: Some("high".into()),
            web_search: true,
            json: true,
        };
        for n in 0..2 {
            let mut acc = c6_llm::Accumulator::default();
            let mut stream = client.stream(&request).await.unwrap();
            while let Some(event) = stream.next().await {
                acc.push(&event.unwrap());
            }
            if n == 0 {
                request.messages.push(acc.finish(true));
                request.messages.push(c6_llm::Message::user_text("wait"));
            }
        }
        let saved = journal.load_keep_warm_snapshot("child").await.unwrap().unwrap();
        assert_eq!(saved.connection_id, "account");
        assert_eq!(saved.request.cache_key.as_deref(), Some("child:account"));
        assert_eq!(saved.usage.cache_read_tokens, 69_000);
        assert_eq!(saved.request.messages.len(), request.messages.len(), "capture never mutates the original input");
        let response = saved.response.as_ref().unwrap();
        assert_eq!(response.role, c6_llm::Role::Assistant);
        assert!(response.blocks.iter().any(|b| b.text() == Some("final assistant output 2")));
        assert!(!serde_json::to_string(&saved).unwrap().contains("DO-NOT-PERSIST"));
        // The move spec carries the same snapshot without any head history.
        let (spec, _) = journal.load_session("child").await.unwrap().unwrap();
        let moved = crate::journal::Journal::open(&dir.path().join("moved.db")).await.unwrap();
        moved.import_owned("child", &spec, &[], 2, "move").await.unwrap();
        assert_eq!(
            serde_json::to_value(moved.load_keep_warm_snapshot("child").await.unwrap()).unwrap(),
            serde_json::to_value(Some(&saved)).unwrap()
        );
        drop(client);
        drop(writer);
        drop(journal);
        drop(link);
        let restarted_journal = crate::journal::Journal::open(&path).await.unwrap();
        let snapshot = restarted_journal.load_keep_warm_snapshot("child").await.unwrap().unwrap();
        let restarted_link = HeadLink::local(head.clone());
        let usage = restarted_link.keep_warm("root", &snapshot, "still waiting").await.unwrap();
        assert_eq!(serde_json::to_value(&snapshot).unwrap(), serde_json::to_value(&saved).unwrap(), "pings never mutate captured history");
        assert!(usage.keep_warm);
        assert_eq!(usage.resolved_connection_id, "account");
        assert_eq!(usage.cache_read_tokens, 69_000);
        assert_eq!(*head.grants.lock().unwrap(), ["alias", "alias", "account"]);
        let mut changed = snapshot.clone();
        changed.kind = "anthropic".into();
        assert!(restarted_link.keep_warm("root", &changed, "must not run").await.is_err());
        changed = snapshot.clone();
        changed.base_url.push_str("/changed");
        assert!(restarted_link.keep_warm("root", &changed, "must not run").await.is_err());
        changed = snapshot.clone();
        changed.connection_id = "alias".into();
        assert!(restarted_link.keep_warm("root", &changed, "must not route").await.is_err());
        let mut old_snapshot = serde_json::to_value(&snapshot).unwrap();
        old_snapshot.as_object_mut().unwrap().remove("response");
        let old_snapshot = serde_json::from_value(old_snapshot).unwrap();
        assert!(restarted_link.keep_warm("root", &old_snapshot, "must not run").await.is_err());
        let requests = requests.lock().unwrap();
        let previous = requests[1]["input"].as_array().unwrap();
        let ping = requests[2]["input"].as_array().unwrap();
        assert!(previous.iter().any(|item| item["encrypted_content"] == "private-prefix-1"));
        assert_eq!(serde_json::to_vec(previous).unwrap(), serde_json::to_vec(&ping[..previous.len()]).unwrap());
        assert_eq!(ping.len(), previous.len() + 3);
        assert_eq!(ping[previous.len()]["encrypted_content"], "private-prefix-2");
        assert_eq!(ping[previous.len() + 1]["role"], "assistant");
        assert_eq!(ping[previous.len() + 1]["content"][0]["text"], "final assistant output 2");
        assert_eq!(ping.last().unwrap()["role"], "user");
        assert_eq!(ping.last().unwrap()["content"][0]["text"], "still waiting");
        assert_eq!(requests[1]["prompt_cache_key"], requests[2]["prompt_cache_key"]);
        assert_eq!(requests[1]["instructions"], requests[2]["instructions"]);
        assert_eq!(requests[1]["tools"], requests[2]["tools"]);
        let mut expected_body = requests[1].clone();
        expected_body["input"] = requests[2]["input"].clone();
        assert_eq!(expected_body, requests[2], "all non-history provider settings stay unchanged");
        assert_eq!(requests.len(), 3, "changed configurations and aliases never dispatch");
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn keep_warm_timeout_covers_credentials_and_cancels_the_future() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        struct Head(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl LocalHead for Head {
            async fn call(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value, String> {
                unreachable!()
            }
            async fn credential(&self, _: &str) -> CredentialGrant {
                let _guard = DropFlag(self.0.clone());
                std::future::pending().await
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let link = HeadLink::local(Arc::new(Head(dropped.clone())));
        let snapshot = crate::journal::KeepWarmSnapshot {
            response: Some(c6_llm::Message { role: c6_llm::Role::Assistant, blocks: vec![c6_llm::Block::Text("done".into())] }),
            request: c6_llm::ModelRequest {
                model: "m".into(),
                system: String::new(),
                messages: vec![],
                tools: vec![],
                max_output_tokens: None,
                cache_key: None,
                effort: None,
                web_search: false,
                json: false,
            },
            connection_id: "account".into(),
            kind: "chatgpt".into(),
            base_url: String::new(),
            last_call_ms: 1,
            usage: c6_llm::Usage::default(),
        };
        let start = tokio::time::Instant::now();
        assert!(link.keep_warm("s", &snapshot, "wait").await.unwrap_err().to_string().contains("timed out"));
        assert_eq!(start.elapsed(), CALL_TIMEOUT);
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[tokio::test(start_paused = true)]
    async fn keep_warm_credential_refresh_cannot_outlive_cache_freshness() {
        struct Head {
            url: String,
            started: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl LocalHead for Head {
            async fn call(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value, String> {
                Err("unused".into())
            }
            async fn credential(&self, connection_id: &str) -> CredentialGrant {
                self.started.store(true, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(5)).await;
                CredentialGrant {
                    connection_id: connection_id.into(),
                    kind: "openai_compat".into(),
                    base_url: self.url.clone(),
                    ..Default::default()
                }
            }
        }
        let calls = Arc::new(AtomicU64::new(0));
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post({
                let calls = calls.clone();
                move || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { "data: [DONE]\n\n" }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let link = HeadLink::local(Arc::new(Head { url: url.clone(), started: started.clone() }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let snapshot = crate::journal::KeepWarmSnapshot {
            request: c6_llm::ModelRequest {
                model: "m".into(),
                system: String::new(),
                messages: vec![],
                tools: vec![],
                max_output_tokens: None,
                cache_key: None,
                effort: None,
                web_search: false,
                json: false,
            },
            response: Some(c6_llm::Message { role: c6_llm::Role::Assistant, blocks: vec![c6_llm::Block::Text("done".into())] }),
            connection_id: "account".into(),
            kind: "openai_compat".into(),
            base_url: url,
            last_call_ms: 1,
            usage: c6_llm::Usage::default(),
        };
        let start = tokio::time::Instant::now();
        assert!(link.keep_warm_until("s", &snapshot, "wait", start + Duration::from_secs(1)).await.is_err());
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert!(started.load(Ordering::Relaxed));
        assert_eq!(calls.load(Ordering::Relaxed), 0, "expired prefixes never reach the provider after a slow refresh");
        started.store(false, Ordering::Relaxed);
        assert!(link.keep_warm_until("s", &snapshot, "wait", start).await.is_err());
        assert!(!started.load(Ordering::Relaxed), "already-expired requests do not refresh credentials");
        server.abort();
    }

    #[tokio::test]
    async fn keep_warm_capture_waits_for_done_and_invalidates_stream_errors() {
        use c6_llm::{Block, LlmError, ModelEvent, ModelRequest};
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap());
        journal.import("s", &c6_proto::CreateSession::default(), &[]).await.unwrap();
        let link = Arc::new(HeadLink::default());
        let writer = Arc::new(crate::session::Writer::new("s".into(), journal.clone(), link.clone(), 1));
        let client = LinkedClient::new(link, "s", "account").track_activity(&writer);
        let request = ModelRequest {
            model: "m".into(),
            system: "exact system".into(),
            messages: vec![c6_llm::Message::user_text("input")],
            tools: vec![],
            max_output_tokens: Some(123),
            cache_key: Some("s".into()),
            effort: Some("high".into()),
            web_search: true,
            json: true,
        };
        let grant = CredentialGrant { kind: "anthropic".into(), base_url: "http://localhost".into(), ..Default::default() };
        let events = futures::stream::iter([
            Ok(ModelEvent::Block { index: 0, block: Block::SignedThinking { text: "reasoning".into(), signature: "signature".into() } }),
            Ok(ModelEvent::Block { index: 1, block: Block::Text("final answer".into()) }),
            Ok(ModelEvent::Usage(c6_llm::Usage { input_tokens: 70_000, ..Default::default() })),
            Ok(ModelEvent::Done { stop_reason: "stop".into() }),
            Err(LlmError::Network("stream failed".into())),
        ])
        .boxed();
        let mut stream = client.capture_request(&request, &grant, events, writer.now_ms()).await.unwrap();
        for _ in 0..3 {
            stream.next().await.unwrap().unwrap();
        }
        assert!(journal.load_keep_warm_snapshot("s").await.unwrap().is_none(), "usage alone never serializes an incomplete response");
        stream.next().await.unwrap().unwrap();
        let completed = journal.load_keep_warm_snapshot("s").await.unwrap().unwrap();
        assert_eq!(serde_json::to_value(&completed.request).unwrap(), serde_json::to_value(&request).unwrap());
        assert_eq!(
            completed.response.unwrap().blocks,
            [Block::SignedThinking { text: "reasoning".into(), signature: "signature".into() }, Block::Text("final answer".into()),]
        );
        assert!(stream.next().await.unwrap().is_err());
        assert!(journal.load_keep_warm_snapshot("s").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn keep_warm_capture_skips_small_or_unmeasured_context_and_fences_superseded_streams() {
        use c6_llm::{LlmError, ModelEvent, ModelRequest, Usage};
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap());
        journal.import("s", &c6_proto::CreateSession::default(), &[]).await.unwrap();
        let link = Arc::new(HeadLink::default());
        let writer = Arc::new(crate::session::Writer::new("s".into(), journal.clone(), link.clone(), 1));
        let client = LinkedClient::new(link, "s", "account").track_activity(&writer);
        let mut request = ModelRequest {
            model: "old".into(),
            system: String::new(),
            messages: vec![c6_llm::Message::user_text("input")],
            tools: vec![],
            max_output_tokens: None,
            cache_key: None,
            effort: None,
            web_search: false,
            json: false,
        };
        let grant = CredentialGrant { kind: "anthropic".into(), base_url: "http://localhost".into(), ..Default::default() };
        for usage in [None, Some(Usage { input_tokens: 49_999, ..Default::default() })] {
            let mut events = vec![ModelEvent::TextDelta("output".into())];
            if let Some(usage) = usage {
                events.push(ModelEvent::Usage(usage));
            }
            events.push(ModelEvent::Done { stop_reason: "stop".into() });
            let source = futures::stream::iter(events.clone().into_iter().map(Ok)).boxed();
            let stream = client.capture_request(&request, &grant, source, writer.now_ms()).await.unwrap();
            let actual: Vec<_> = stream.map(Result::unwrap).collect().await;
            assert_eq!(actual, events, "capture never changes the model stream");
            assert!(journal.load_keep_warm_snapshot("s").await.unwrap().is_none());
            assert!(journal.load_session("s").await.unwrap().unwrap().0.keep_warm_snapshot_json.is_none());
        }
        let complete = || {
            vec![
                Ok(ModelEvent::Usage(Usage { input_tokens: 50_000, ..Default::default() })),
                Ok(ModelEvent::Done { stop_reason: "stop".into() }),
            ]
        };
        let old_source = futures::stream::iter(complete().into_iter().chain([Err(LlmError::Network("late failure".into()))])).boxed();
        let mut old = client.capture_request(&request, &grant, old_source, writer.now_ms()).await.unwrap();
        request.model = "new".into();
        let mut new = client.capture_request(&request, &grant, futures::stream::iter(complete()).boxed(), writer.now_ms()).await.unwrap();
        for _ in 0..2 {
            old.next().await.unwrap().unwrap();
        }
        assert!(journal.load_keep_warm_snapshot("s").await.unwrap().is_none(), "superseded stream cannot populate a newer reservation");
        while let Some(event) = new.next().await {
            event.unwrap();
        }
        assert!(old.next().await.unwrap().is_err());
        assert_eq!(
            journal.load_keep_warm_snapshot("s").await.unwrap().unwrap().request.model,
            "new",
            "old errors cannot invalidate a newer prefix"
        );
    }

    #[tokio::test]
    async fn keep_warm_disabled_foreground_clears_old_snapshot_without_capturing_large_usage() {
        struct Head(String);
        #[async_trait::async_trait]
        impl LocalHead for Head {
            async fn call(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value, String> {
                Err("unused".into())
            }
            async fn credential(&self, connection_id: &str) -> CredentialGrant {
                CredentialGrant {
                    connection_id: connection_id.into(),
                    kind: "openai_compat".into(),
                    base_url: self.0.clone(),
                    ..Default::default()
                }
            }
        }
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                (
                    [("content-type", "text/event-stream")],
                    concat!(
                        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100000,\"completion_tokens\":1}}\n\n",
                        "data: [DONE]\n\n"
                    ),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let link = Arc::new(HeadLink::local(Arc::new(Head(format!("http://{}/v1", listener.local_addr().unwrap())))));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let model = c6_proto::ModelSpec {
            connection_id: "account".into(),
            connection_kind: "openai_compat".into(),
            model: "m".into(),
            ..Default::default()
        };
        let spec = c6_proto::CreateSession {
            model: Some(model.clone()),
            cwd: dir.path().display().to_string(),
            keep_warm_json: Some(r#"{"enabled":false}"#.into()),
            ..Default::default()
        };
        let started = c6_proto::SessionEvent {
            seq: 1,
            ts_ms: c6_core::now_ms(),
            kind: Some(c6_proto::session_event::Kind::Started(c6_proto::SessionStarted {
                model: Some(model),
                cwd: spec.cwd.clone(),
                tool_profile: "generic".into(),
                ..Default::default()
            })),
        };
        journal.import("s", &spec, &[started]).await.unwrap();
        let old = crate::journal::KeepWarmSnapshot {
            request: c6_llm::ModelRequest {
                model: "m".into(),
                system: String::new(),
                messages: vec![],
                tools: vec![],
                max_output_tokens: None,
                cache_key: None,
                effort: None,
                web_search: false,
                json: false,
            },
            response: Some(c6_llm::Message { role: c6_llm::Role::Assistant, blocks: vec![c6_llm::Block::Text("old".into())] }),
            connection_id: "account".into(),
            kind: "openai_compat".into(),
            base_url: String::new(),
            last_call_ms: 100,
            usage: c6_llm::Usage::default(),
        };
        journal.save_keep_warm_snapshot("s", &old, 0).await.unwrap();
        let manager = crate::session::SessionManager::new(journal.clone(), link, dir.path().join("worktrees"));
        manager
            .handle(c6_proto::SessionCommand {
                session_id: "s".into(),
                command_id: "input".into(),
                kind: Some(c6_proto::session_command::Kind::Input(c6_proto::UserInput { text: "hello".into(), ..Default::default() })),
                ..Default::default()
            })
            .await
            .unwrap();
        let events = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let events = journal.events_after("s", 0, 100).await.unwrap();
                if events.iter().any(|e| matches!(e.kind, Some(c6_proto::session_event::Kind::TurnEnded(_)))) {
                    break events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(events.iter().any(|e| matches!(&e.kind, Some(c6_proto::session_event::Kind::Usage(u)) if u.input_tokens == 100_000)));
        assert!(journal.load_keep_warm_snapshot("s").await.unwrap().is_none());
        assert!(journal.keep_warm_call_ms("s").await.unwrap().is_none());
        server.abort();
    }

    #[tokio::test]
    async fn chatgpt_alias_uses_the_child_and_real_account_for_cache_and_quota() {
        use c6_llm::{ModelClient, ModelEvent};
        use serde_json::Value;

        struct Head {
            url: String,
            grants: AtomicU64,
            quota_accounts: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl LocalHead for Head {
            async fn call(&self, _: &str, _: Value) -> Result<Value, String> {
                Err("unused".into())
            }

            async fn credential(&self, connection_id: &str) -> CredentialGrant {
                let account = if self.grants.fetch_add(1, Ordering::Relaxed) == 2 { "b" } else { "a" };
                CredentialGrant {
                    connection_id: connection_id.into(),
                    resolved_connection_id: account.into(),
                    is_alias: true,
                    kind: "chatgpt".into(),
                    // Deliberately identical: the client's quota hook must
                    // still be rebound when only the resolved account changes.
                    base_url: self.url.clone(),
                    bearer: "token".into(),
                    ..Default::default()
                }
            }

            fn chatgpt_limits_hook(&self, id: &str) -> Option<c6_llm::openai_responses::ChatGptLimitsHook> {
                let id = id.to_owned();
                let accounts = self.quota_accounts.clone();
                Some(Arc::new(move |_| {
                    accounts.lock().unwrap().push(id.clone());
                    Box::pin(async {})
                }))
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new().route(
            "/responses",
            axum::routing::post({
                let requests = requests.clone();
                move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                    let requests = requests.clone();
                    async move {
                        let header = headers["session_id"].to_str().unwrap().to_owned();
                        let cache = body["prompt_cache_key"].as_str().unwrap().to_owned();
                        let (_, account) = cache.rsplit_once(':').unwrap();
                        let item = serde_json::json!({
                            "type": "response.output_item.done", "output_index": 0,
                            "item": {"type": "reasoning", "id": format!("rs_{account}"), "encrypted_content": format!("encrypted-{account}"), "summary": []}
                        });
                        requests.lock().unwrap().push((header, cache, body["input"].clone()));
                        (
                            [("content-type", "text/event-stream"), ("x-codex-primary-used-percent", "10")],
                            format!("data: {item}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{}}}}\n\n"),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let head = Arc::new(Head { url, grants: Default::default(), quota_accounts: Default::default() });
        let link = Arc::new(HeadLink::local(head.clone()));
        let mut req = c6_llm::ModelRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![
                c6_llm::Message::user_text("hi"),
                c6_llm::Message {
                    role: c6_llm::Role::Assistant,
                    blocks: vec![
                        c6_llm::Block::Reasoning { id: "old".into(), encrypted: "unknown".into(), summary: String::new() },
                        c6_llm::Block::Text("ordinary text".into()),
                        c6_llm::Block::ToolUse { id: "call_1".into(), name: "tool".into(), input_json: "{}".into() },
                    ],
                },
                c6_llm::Message {
                    role: c6_llm::Role::User,
                    blocks: vec![c6_llm::Block::ToolResult { id: "call_1".into(), content: "ordinary result".into(), is_error: false }],
                },
            ],
            tools: Vec::new(),
            max_output_tokens: None,
            cache_key: Some("child".into()),
            effort: None,
            web_search: false,
            json: false,
        };
        let restarted = Arc::new(HeadLink::local(head.clone()));
        for (i, id) in ["a", "a", "b", "a", "a"].into_iter().enumerate() {
            // A new client each turn; the last call reloads on a new worker.
            let client = LinkedClient::new(if i == 4 { restarted.clone() } else { link.clone() }, "root", "alias");
            let mut stream = client.stream(&req).await.unwrap();
            let ModelEvent::Account(account) = stream.next().await.unwrap().unwrap() else { panic!("missing stream account") };
            assert_eq!(account.resolved_connection_id, id);
            let mut acc = c6_llm::Accumulator::default();
            while let Some(event) = stream.next().await {
                acc.push(&event.unwrap());
            }
            assert_eq!(acc.stop_reason.as_deref(), Some("stop"));
            if i == 0 || i == 2 {
                req.messages.push(acc.finish(true));
            }
        }
        assert_eq!(*head.quota_accounts.lock().unwrap(), ["a", "a", "b", "a", "a"]);
        let requests = requests.lock().unwrap();
        for ((header, key, input), (expected, private)) in requests.iter().zip([
            ("child:a", None),
            ("child:a", Some("encrypted-a")),
            ("child:b", None),
            ("child:a", Some("encrypted-a")),
            ("child:a", None),
        ]) {
            assert_eq!(header, expected);
            assert_eq!(key, expected);
            let items = input.as_array().unwrap();
            let encrypted: Vec<_> = items.iter().filter_map(|item| item["encrypted_content"].as_str()).collect();
            assert_eq!(encrypted, private.into_iter().collect::<Vec<_>>());
            assert!(items.iter().any(|item| item["content"][0]["text"] == "ordinary text"));
            assert!(items.iter().any(|item| item["type"] == "function_call"));
            assert!(items.iter().any(|item| item["type"] == "function_call_output" && item["output"] == "ordinary result"));
        }
        assert_eq!(requests.len(), 5);
        assert!(link.cache.lock().unwrap().is_empty());
        server.abort();
    }
}
