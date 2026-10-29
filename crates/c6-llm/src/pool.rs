//! Pools (docs/pools.md): a connection made of other connections, each
//! taking some calls at once (its slots). A call goes to the member with a
//! free slot that is least busy (calls in flight over slots); with every
//! slot taken it waits its turn: interactive calls first, first come first
//! served within each class. A member that
//! fails for now rests, twice as long each time in a row, and the call goes
//! to another. Members without the model asked for, or not for the
//! caller's realm, are passed over. With none left, the call fails with
//! `LlmError::Unavailable` (for now: the caller may ask again later).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::{EventStream, LlmError, ModelClient, ModelEvent, ModelRequest};

/// How long a member's model list is trusted.
const MODELS_TTL: Duration = Duration::from_secs(300);
/// How long a member rests after failing for now, and at most.
const REST_FIRST: Duration = Duration::from_secs(15);
const REST_MAX: Duration = Duration::from_secs(300);
/// The longest a call waits for a slot.
const MAX_WAIT: Duration = Duration::from_secs(600);

/// The caller's intent, kept through awaits and retries. Background is the
/// default; a task spawned for a caller must carry its priority explicitly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    #[default]
    Background,
    Interactive,
}

tokio::task_local! {
    static PRIORITY: Priority;
}

impl Priority {
    pub fn current() -> Self {
        PRIORITY.try_with(|p| *p).unwrap_or_default()
    }

    pub async fn scope<F: Future>(self, call: F) -> F::Output {
        PRIORITY.scope(self, call).await
    }
}

/// A member: one of carbon6's connections, its calls at once, and the
/// realms it is for (empty: all).
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub id: String,
    pub slots: u32,
    pub realms: Vec<String>,
}

/// Where a member is reached now.
#[derive(Clone, Debug, Default)]
pub struct Endpoint {
    /// The connection kind (`openai_compat`, `openai`, `anthropic`, …).
    pub kind: String,
    pub base_url: String,
    pub bearer: String,
    pub headers: HashMap<String, String>,
}

/// What a pool asks of its owner: a member's endpoint (its credentials may
/// change between calls).
#[async_trait]
pub trait Dial: Send + Sync {
    async fn endpoint(&self, member: &str) -> Result<Endpoint, LlmError>;
}

struct Seat {
    member: Member,
    busy: u32,
    /// Resting until then, after this many failures in a row.
    rest: Option<(Instant, u32)>,
    /// Its models, when fetched (none: it can't say, so any).
    models: Option<(Instant, Option<Vec<String>>)>,
}

impl Seat {
    fn resting(&self, now: Instant) -> bool {
        self.rest.is_some_and(|(until, _)| until > now)
    }
}

/// Where a waiting call is told which slot it got (or why none).
type Reply = oneshot::Sender<Result<Slot, LlmError>>;

struct Waiter {
    priority: Priority,
    /// The members that may take it.
    want: Vec<String>,
    tx: Reply,
}

#[derive(Default)]
struct State {
    seats: Vec<Seat>,
    waiting: VecDeque<Waiter>,
}

impl State {
    fn seat(&mut self, id: &str) -> Option<&mut Seat> {
        self.seats.iter_mut().find(|s| s.member.id == id)
    }

    /// Of `want`, the least busy member with a free slot (the first listed on a tie).
    fn least_busy(&self, want: &[String], now: Instant) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, s) in self.seats.iter().enumerate() {
            if !want.contains(&s.member.id) || s.resting(now) || s.busy >= s.member.slots {
                continue;
            }
            // busy/slots < best.busy/best.slots, without division.
            let better = best.is_none_or(|b| {
                let b = &self.seats[b];
                (s.busy as u64) * (b.member.slots as u64) < (b.busy as u64) * (s.member.slots as u64)
            });
            if better {
                best = Some(i);
            }
        }
        best
    }

    fn all_resting(&self, want: &[String], now: Instant) -> bool {
        self.seats.iter().filter(|s| want.contains(&s.member.id)).all(|s| s.resting(now))
    }
}

pub struct Pool {
    /// The pool's connection id.
    pub id: String,
    dial: Arc<dyn Dial>,
    state: Mutex<State>,
}

/// A slot taken on a member, given back when dropped. Say how the call went
/// (`worked`, `failed`) so the member rests, or stops resting.
pub struct Slot {
    pool: Arc<Pool>,
    pub member: String,
    told: bool,
}

impl Slot {
    /// The member answered.
    pub fn worked(mut self) {
        self.told = true;
        self.pool.give_back(&self.member, Some(true));
    }

    /// The member failed for now: it rests.
    pub fn failed(mut self) {
        self.told = true;
        self.pool.give_back(&self.member, Some(false));
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.told {
            self.pool.give_back(&self.member, None);
        }
    }
}

impl Pool {
    pub fn new(id: &str, members: Vec<Member>, dial: Arc<dyn Dial>) -> Arc<Self> {
        let pool = Arc::new(Self { id: id.to_owned(), dial, state: Mutex::default() });
        pool.set_members(members);
        pool
    }

    /// Its members as configured now; those kept keep their calls in flight,
    /// rests and model lists.
    pub fn set_members(self: &Arc<Self>, members: Vec<Member>) {
        let sends = {
            let mut st = self.state.lock().unwrap();
            let same = st.seats.len() == members.len() && st.seats.iter().zip(&members).all(|(s, m)| s.member == *m);
            if same {
                return;
            }
            let mut old: HashMap<String, Seat> = st.seats.drain(..).map(|s| (s.member.id.clone(), s)).collect();
            st.seats = members
                .into_iter()
                .map(|member| match old.remove(&member.id) {
                    Some(s) => Seat { member, ..s },
                    None => Seat { member, busy: 0, rest: None, models: None },
                })
                .collect();
            // Calls in flight on members that left are given back to nobody.
            self.dispatch(&mut st)
        };
        send(sends);
    }

    pub fn members(&self) -> Vec<Member> {
        self.state.lock().unwrap().seats.iter().map(|s| s.member.clone()).collect()
    }

    /// Calls in flight on each member, by id.
    pub fn busy(&self) -> HashMap<String, u32> {
        self.state.lock().unwrap().seats.iter().map(|s| (s.member.id.clone(), s.busy)).collect()
    }

    pub async fn endpoint(&self, member: &str) -> Result<Endpoint, LlmError> {
        self.dial.endpoint(member).await
    }

    /// A slot on the least busy member that has `model` ("": any) and is
    /// for `realm`, waiting for one if all are taken.
    pub async fn acquire(self: &Arc<Self>, model: &str, realm: &str) -> Result<Slot, LlmError> {
        let want = self.candidates(model, realm).await?;
        let (tx, rx) = oneshot::channel();
        let sends = {
            let mut st = self.state.lock().unwrap();
            let priority = Priority::current();
            let at = st.waiting.iter().position(|w| w.priority < priority).unwrap_or(st.waiting.len());
            st.waiting.insert(at, Waiter { priority, want, tx });
            self.dispatch(&mut st)
        };
        send(sends);
        match tokio::time::timeout(MAX_WAIT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(LlmError::Unavailable(format!("pool {} closed", self.id))),
            Err(_) => Err(LlmError::Unavailable(format!("every member of pool {} stayed busy for {}s", self.id, MAX_WAIT.as_secs()))),
        }
    }

    /// Runs `call` on a member's endpoint, on another member when one fails
    /// for now, until one answers or none is left.
    pub async fn run<T, F, Fut>(self: &Arc<Self>, model: &str, realm: &str, call: F) -> Result<T, LlmError>
    where
        F: Fn(Endpoint) -> Fut,
        Fut: Future<Output = Result<T, LlmError>>,
    {
        loop {
            let slot = self.acquire(model, realm).await?;
            let endpoint = match self.dial.endpoint(&slot.member).await {
                Ok(e) => e,
                Err(e) => {
                    warn!(pool = %self.id, member = %slot.member, "a pool member's credentials: {e}");
                    slot.failed();
                    continue;
                }
            };
            match call(endpoint).await {
                Ok(v) => {
                    slot.worked();
                    return Ok(v);
                }
                Err(e) if e.retryable() => {
                    warn!(pool = %self.id, member = %slot.member, "a pool member failed for now: {e}");
                    slot.failed();
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The members that may take a call for `model` in `realm`.
    async fn candidates(&self, model: &str, realm: &str) -> Result<Vec<String>, LlmError> {
        let now = Instant::now();
        let (mine, stale): (Vec<String>, Vec<String>) = {
            let st = self.state.lock().unwrap();
            let mine: Vec<&Seat> =
                st.seats.iter().filter(|s| s.member.realms.is_empty() || s.member.realms.iter().any(|r| r == realm)).collect();
            let stale = mine
                .iter()
                .filter(|s| {
                    !model.is_empty() && !s.resting(now) && s.models.as_ref().is_none_or(|(at, _)| now.duration_since(*at) > MODELS_TTL)
                })
                .map(|s| s.member.id.clone())
                .collect();
            (mine.iter().map(|s| s.member.id.clone()).collect(), stale)
        };
        if mine.is_empty() {
            return Err(LlmError::Unavailable(format!("no member of pool {} is for realm {realm}", self.id)));
        }
        for id in stale {
            let listed = match self.dial.endpoint(&id).await {
                Ok(e) => list_models(&e).await,
                Err(e) => Err(e),
            };
            let mut st = self.state.lock().unwrap();
            let Some(seat) = st.seat(&id) else { continue };
            match listed {
                Ok(models) => seat.models = Some((Instant::now(), Some(models))),
                // Up, but it can't say (no model list): it may have any.
                Err(LlmError::Http { status, .. }) if status < 500 && status != 408 && status != 429 => {
                    seat.models = Some((Instant::now(), None))
                }
                Err(e) => {
                    warn!(pool = %self.id, member = %id, "a pool member's models: {e}");
                    rest(seat);
                }
            }
        }
        let st = self.state.lock().unwrap();
        let want: Vec<String> = st
            .seats
            .iter()
            .filter(|s| mine.contains(&s.member.id))
            .filter(|s| match &s.models {
                Some((_, Some(models))) => has_model(models, model),
                _ => true,
            })
            .map(|s| s.member.id.clone())
            .collect();
        if want.is_empty() {
            return Err(LlmError::Unavailable(format!("no member of pool {} has the model {model}", self.id)));
        }
        Ok(want)
    }

    fn give_back(self: &Arc<Self>, member: &str, worked: Option<bool>) {
        let sends = {
            let mut st = self.state.lock().unwrap();
            if let Some(seat) = st.seat(member) {
                seat.busy = seat.busy.saturating_sub(1);
                match worked {
                    Some(true) => seat.rest = None,
                    Some(false) => {
                        warn!(pool = %self.id, %member, "a pool member rests after failing for now");
                        rest(seat);
                    }
                    None => {}
                }
            }
            self.dispatch(&mut st)
        };
        send(sends);
    }

    /// Gives free slots to waiters, by priority then arrival; fails those
    /// whose members all rest. What to send, once the lock is let go.
    fn dispatch(self: &Arc<Self>, st: &mut State) -> Vec<(Reply, Result<Slot, LlmError>)> {
        let now = Instant::now();
        let mut out = Vec::new();
        let mut i = 0;
        while i < st.waiting.len() {
            let w = &st.waiting[i];
            if w.tx.is_closed() {
                st.waiting.remove(i);
                continue;
            }
            if st.all_resting(&w.want, now) {
                let w = st.waiting.remove(i).unwrap();
                out.push((w.tx, Err(LlmError::Unavailable(format!("every member of pool {} that could take it is resting", self.id)))));
                continue;
            }
            match st.least_busy(&w.want, now) {
                Some(seat) => {
                    st.seats[seat].busy += 1;
                    let member = st.seats[seat].member.id.clone();
                    let w = st.waiting.remove(i).unwrap();
                    out.push((w.tx, Ok(Slot { pool: self.clone(), member, told: false })));
                }
                None => i += 1,
            }
        }
        out
    }
}

/// Sends what `dispatch` decided. A slot whose caller gave up is dropped,
/// and so given back.
fn send(sends: Vec<(Reply, Result<Slot, LlmError>)>) {
    for (tx, r) in sends {
        let _ = tx.send(r);
    }
}

fn rest(seat: &mut Seat) {
    let failures = seat.rest.map_or(0, |(_, n)| n) + 1;
    let wait = REST_FIRST.saturating_mul(1 << (failures - 1).min(10)).min(REST_MAX);
    seat.rest = Some((Instant::now() + wait, failures));
}

/// Whether a member's models include `model` (Ollama's `name` is `name:latest`).
fn has_model(models: &[String], model: &str) -> bool {
    let bare = |m: &str| m.strip_suffix(":latest").unwrap_or(m).to_owned();
    model.is_empty() || models.is_empty() || models.iter().any(|m| bare(m) == bare(model))
}

async fn list_models(e: &Endpoint) -> Result<Vec<String>, LlmError> {
    match e.kind.as_str() {
        "anthropic" => crate::anthropic::list_models(&e.base_url, &e.bearer).await,
        _ => crate::openai_chat::list_models(&e.base_url, &e.bearer).await,
    }
}

/// A model client over a pool: each call goes to a member as the pool
/// picks (`Pool::acquire`), holding its slot until the answer ends, and to
/// another member when one fails before answering. Each call shows in the
/// activity panel with the member that served it.
pub struct PoolClient {
    pool: Arc<Pool>,
    realm: String,
    origin: crate::activity::Origin,
    limits: Option<crate::limit::Limits>,
}

impl PoolClient {
    pub fn new(pool: Arc<Pool>, realm: &str, origin: crate::activity::Origin) -> Self {
        Self { pool, realm: realm.to_owned(), origin, limits: None }
    }

    /// Each member's call held to `limits`: one that stalls rests.
    pub fn with_limits(self, limits: crate::limit::Limits) -> Self {
        Self { limits: Some(limits), ..self }
    }
}

#[async_trait]
impl ModelClient for PoolClient {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        loop {
            let slot = self.pool.acquire(&req.model, &self.realm).await?;
            let e = match self.pool.dial.endpoint(&slot.member).await {
                Ok(e) => e,
                Err(e) => {
                    warn!(pool = %self.pool.id, member = %slot.member, "a pool member's credentials: {e}");
                    slot.failed();
                    continue;
                }
            };
            let origin = crate::activity::Origin { connection: slot.member.clone(), via: self.pool.id.clone(), ..self.origin.clone() };
            let mut client = crate::client_once(&e.kind, &e.base_url, &e.bearer, e.headers)?;
            if let Some(l) = self.limits {
                client = crate::limit::limited(client, l);
            }
            let client = crate::activity::track(client, origin);
            match client.stream(req).await {
                Ok(stream) => {
                    info!(pool = %self.pool.id, member = %slot.member, model = %req.model, "a pool member took a call");
                    return Ok(held(stream, slot));
                }
                Err(e) if e.retryable() => {
                    warn!(pool = %self.pool.id, member = %slot.member, "a pool member failed for now: {e}");
                    slot.failed();
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// The stream, holding its slot until it ends: done, the member worked;
/// failed for now, it rests.
fn held(stream: EventStream, slot: Slot) -> EventStream {
    let slot = Mutex::new(Some(slot));
    stream
        .map(move |ev| {
            match &ev {
                Ok(ModelEvent::Done { .. }) => {
                    if let Some(s) = slot.lock().unwrap().take() {
                        s.worked();
                    }
                }
                Err(e) if e.retryable() => {
                    if let Some(s) = slot.lock().unwrap().take() {
                        s.failed();
                    }
                }
                _ => {}
            }
            ev
        })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Members at fake addresses.
    struct Fake;

    #[async_trait]
    impl Dial for Fake {
        async fn endpoint(&self, member: &str) -> Result<Endpoint, LlmError> {
            Ok(Endpoint { kind: "fake".into(), base_url: format!("fake://{member}"), ..Default::default() })
        }
    }

    fn member(id: &str, slots: u32, realms: &[&str]) -> Member {
        Member { id: id.into(), slots, realms: realms.iter().map(|r| r.to_string()).collect() }
    }

    /// A pool whose members' model lists are known already.
    fn pool(members: Vec<Member>, models: &[(&str, &[&str])]) -> Arc<Pool> {
        let p = Pool::new("p", members, Arc::new(Fake));
        let mut st = p.state.lock().unwrap();
        for seat in st.seats.iter_mut() {
            let listed = models.iter().find(|(id, _)| *id == seat.member.id).map(|(_, m)| m.iter().map(|s| s.to_string()).collect());
            seat.models = Some((Instant::now(), listed));
        }
        drop(st);
        p
    }

    #[tokio::test]
    async fn the_least_busy_member_with_a_free_slot() {
        let p = pool(vec![member("mac", 2, &[]), member("mumuzz", 1, &[])], &[]);
        let a = p.acquire("m", "home").await.unwrap();
        let b = p.acquire("m", "home").await.unwrap();
        // 0/2 then 0/1 (the first listed on a tie); then 1/2 beats 1/1.
        assert_eq!((a.member.as_str(), b.member.as_str()), ("mac", "mumuzz"));
        let c = p.acquire("m", "home").await.unwrap();
        assert_eq!(c.member, "mac");
        assert_eq!(p.busy(), HashMap::from([("mac".into(), 2), ("mumuzz".into(), 1)]));
        drop((a, b, c));
        assert_eq!(p.busy(), HashMap::from([("mac".into(), 0), ("mumuzz".into(), 0)]));
    }

    #[tokio::test]
    async fn interactive_waiters_go_first_without_preempting() {
        use futures::FutureExt;
        let p = pool(vec![member("one", 2, &[])], &[]);
        let held = p.acquire("m", "home").await.unwrap();
        let other = p.acquire("m", "home").await.unwrap();
        let b1 = p.acquire("m", "home");
        let b2 = p.acquire("m", "home");
        let i1 = Priority::Interactive.scope(p.acquire("m", "home"));
        let i2 = Priority::Interactive.scope(p.acquire("m", "home"));
        tokio::pin!(b1, b2, i1, i2);
        // Polling each future to Pending puts it in the queue, in this order.
        assert!(futures::poll!(&mut b1).is_pending());
        assert!(futures::poll!(&mut b2).is_pending());
        assert!(futures::poll!(&mut i1).is_pending());
        assert!(futures::poll!(&mut i2).is_pending());
        assert_eq!(p.busy()["one"], 2);
        drop(held);
        assert!(futures::poll!(&mut b1).is_pending());
        assert!(futures::poll!(&mut b2).is_pending());
        assert!(futures::poll!(&mut i2).is_pending());
        let slot = i1.as_mut().now_or_never().expect("the first interactive call has the slot").unwrap();
        assert_eq!(p.busy()["one"], 2, "the other running call still holds its slot");
        drop(slot);
        let slot = i2.as_mut().now_or_never().expect("the second interactive call has the slot").unwrap();
        assert!(futures::poll!(&mut b1).is_pending());
        drop(slot);
        let slot = b1.as_mut().now_or_never().expect("the first background call has the slot").unwrap();
        assert!(futures::poll!(&mut b2).is_pending());
        drop(slot);
        drop(b2.as_mut().now_or_never().expect("the second background call has the slot").unwrap());
        drop(other);
        assert_eq!(p.busy()["one"], 0);
    }

    #[tokio::test]
    async fn every_slot_taken_waits_its_turn() {
        let p = pool(vec![member("one", 1, &[])], &[]);
        let first = p.acquire("m", "home").await.unwrap();
        let queued = async |n: usize| {
            while p.state.lock().unwrap().waiting.len() < n {
                tokio::task::yield_now().await;
            }
        };
        let (p2, p3) = (p.clone(), p.clone());
        let second = tokio::spawn(async move { p2.acquire("m", "home").await.map(|s| s.member.clone()) });
        queued(1).await;
        let third = tokio::spawn(async move { p3.acquire("m", "home").await });
        queued(2).await;
        drop(first);
        assert_eq!(second.await.unwrap().unwrap(), "one", "the first to come");
        // Its slot was dropped with its task: the third has it now.
        assert_eq!(third.await.unwrap().unwrap().member, "one");
    }

    #[tokio::test]
    async fn a_failing_member_rests_and_the_call_goes_to_another() {
        let p = pool(vec![member("mac", 4, &[]), member("mumuzz", 1, &[])], &[]);
        let tried = Mutex::new(Vec::new());
        let r = p
            .run("m", "home", |e| {
                tried.lock().unwrap().push(e.base_url.clone());
                async move { if e.base_url == "fake://mac" { Err(LlmError::Network("connection refused".into())) } else { Ok(e.base_url) } }
            })
            .await
            .unwrap();
        assert_eq!(r, "fake://mumuzz");
        assert_eq!(*tried.lock().unwrap(), vec!["fake://mac", "fake://mumuzz"]);
        // Resting: the next call skips it though it is the least busy.
        assert_eq!(p.acquire("m", "home").await.unwrap().member, "mumuzz");
        // A failure that isn't for now is the call's own: no other member.
        let r = p.run("m", "home", |_| async { Err::<(), _>(LlmError::Http { status: 400, body: "bad".into(), retry_after: None }) }).await;
        assert!(matches!(r, Err(LlmError::Http { status: 400, .. })));
        // All resting: unavailable, for now.
        p.acquire("m", "home").await.unwrap().failed();
        assert!(matches!(p.acquire("m", "home").await, Err(LlmError::Unavailable(_))));
    }

    #[tokio::test]
    async fn members_without_the_model_or_the_realm_are_passed_over() {
        let p = pool(
            vec![member("mac", 1, &["home"]), member("mumuzz", 1, &[]), member("lab", 1, &["mach5-agents"])],
            &[("mac", &["qwen3.6:35b", "nomic-embed-text:latest"]), ("mumuzz", &["nomic-embed-text:latest"]), ("lab", &["qwen3.6:35b"])],
        );
        assert_eq!(p.acquire("qwen3.6:35b", "home").await.unwrap().member, "mac");
        assert_eq!(p.acquire("qwen3.6:35b", "mach5-agents").await.unwrap().member, "lab");
        // `name` is `name:latest`.
        let (a, b) = (p.acquire("nomic-embed-text", "home").await.unwrap(), p.acquire("nomic-embed-text", "home").await.unwrap());
        assert_eq!((a.member.as_str(), b.member.as_str()), ("mac", "mumuzz"));
        assert!(matches!(p.acquire("llama4", "home").await, Err(LlmError::Unavailable(m)) if m.contains("llama4")));
        // mumuzz is for every realm, but hasn't it.
        assert!(matches!(p.acquire("qwen3.6:35b", "other").await, Err(LlmError::Unavailable(m)) if m.contains("qwen3.6:35b")));
        let home_only = pool(vec![member("mac", 1, &["home"])], &[]);
        assert!(matches!(home_only.acquire("m", "mach5-agents").await, Err(LlmError::Unavailable(m)) if m.contains("realm mach5-agents")));
    }

    #[tokio::test]
    async fn a_member_that_cannot_list_its_models_may_have_any() {
        let p = pool(vec![member("old", 1, &[])], &[]);
        p.state.lock().unwrap().seats[0].models = Some((Instant::now(), None));
        assert_eq!(p.acquire("anything", "home").await.unwrap().member, "old");
    }

    #[tokio::test]
    async fn new_members_keep_what_the_old_had() {
        let p = pool(vec![member("mac", 1, &[]), member("mumuzz", 1, &[])], &[]);
        let held = p.acquire("m", "home").await.unwrap();
        p.set_members(vec![member("mac", 2, &[])]);
        assert_eq!(p.busy(), HashMap::from([("mac".into(), 1)]), "its call in flight is kept");
        drop(held);
        assert_eq!(p.busy(), HashMap::from([("mac".into(), 0)]));
    }
}
