//! Pools (docs/pools.md): connections made of other connections, for what
//! runs in the head (the memex, the assistant, suggestions). Which member
//! takes a call is `c6_llm::pool`'s; the head keeps one pool per pool
//! connection, with its members as configured, and gives them credentials.
//! Sessions don't use pools: their workers dial their connection
//! themselves.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use c6_api as api;
use c6_llm::LlmError;
use c6_llm::pool::{Endpoint, Member, Pool};
use c6_proto::CredentialRequest;
use tracing::warn;

use crate::store::ConnectionRow;
use crate::{HeadState, bad_request};

/// The connection kind.
pub const KIND: &str = "pool";
/// The most calls at once a member may take.
pub const MAX_SLOTS: u32 = 64;

/// The head's pools, by connection id.
#[derive(Default)]
pub struct Pools {
    pools: Mutex<HashMap<String, Arc<Pool>>>,
}

/// Members' credentials, as the head gives them to itself.
struct Dial {
    head: Weak<HeadState>,
}

#[async_trait::async_trait]
impl c6_llm::pool::Dial for Dial {
    async fn endpoint(&self, member: &str) -> Result<Endpoint, LlmError> {
        let st = self.head.upgrade().ok_or_else(|| LlmError::Unavailable("the head stopped".into()))?;
        let req = CredentialRequest { connection_id: member.to_owned(), ..Default::default() };
        let g = crate::sessions::credential_for(&st, None, "head", req).await;
        if !g.error.is_empty() {
            return Err(LlmError::Config(format!("pool member {member}: {}", g.error)));
        }
        Ok(Endpoint { kind: g.kind, base_url: g.base_url, bearer: g.bearer, headers: g.headers })
    }
}

/// A pool connection's members, as saved.
pub fn members(c: &ConnectionRow) -> Vec<api::PoolMember> {
    serde_json::from_str(&c.members).unwrap_or_default()
}

/// The pool a connection is, if it is one, with its members as configured
/// now (a member since deleted is left out).
pub async fn get(st: &HeadState, id: &str) -> anyhow::Result<Option<Arc<Pool>>> {
    let Some(c) = st.store.connection(id).await?.filter(|c| c.kind == KIND) else { return Ok(None) };
    let mut list = Vec::new();
    for m in members(&c) {
        match st.store.connection(&m.connection_id).await? {
            Some(mc) => list.push(Member { id: mc.id, slots: m.slots, realms: serde_json::from_str(&mc.realms).unwrap_or_default() }),
            None => warn!(pool = %c.name, member = %m.connection_id, "a pool's member is gone"),
        }
    }
    let mut pools = st.pools.pools.lock().unwrap();
    let pool = pools.entry(c.id.clone()).or_insert_with(|| {
        let head = st.arc().map(|a| Arc::downgrade(&a)).unwrap_or_default();
        Pool::new(&c.id, Vec::new(), Arc::new(Dial { head }))
    });
    pool.set_members(list);
    Ok(Some(pool.clone()))
}

/// A model client for something the head runs in `realm`: through the pool
/// when the connection is one, else with its own credentials. Its calls
/// show in the activity panel as `origin`.
pub async fn client(
    st: &HeadState,
    connection_id: &str,
    realm: &str,
    origin: c6_llm::activity::Origin,
) -> anyhow::Result<Box<dyn c6_llm::ModelClient>> {
    if let Some(pool) = get(st, connection_id).await? {
        return Ok(Box::new(c6_llm::pool::PoolClient::new(pool, realm, origin)));
    }
    let req = CredentialRequest { connection_id: connection_id.to_owned(), ..Default::default() };
    let g = crate::sessions::credential_for(st, None, "head", req).await;
    anyhow::ensure!(g.error.is_empty(), "{}", g.error);
    Ok(c6_llm::activity::track(c6_llm::client_for(&g.kind, &g.base_url, &g.bearer, g.headers)?, origin))
}

/// Checks a pool's members before they are saved: other connections (by
/// id or name), not pools nor Claude Code, each once, with 1 to 64 calls at
/// once. The members, by id.
pub async fn check_members(st: &HeadState, pool_id: &str, members: &[api::PoolMember]) -> anyhow::Result<Vec<api::PoolMember>> {
    if members.is_empty() {
        return Err(bad_request("a pool needs at least one member"));
    }
    let connections = st.store.connections().await?;
    let mut out: Vec<api::PoolMember> = Vec::new();
    for m in members {
        let want = m.connection_id.trim();
        let c = connections
            .iter()
            .find(|c| c.id == want || c.name == want)
            .ok_or_else(|| bad_request(format!("a pool's member: no connection {want:?}")))?;
        if c.kind == KIND || c.id == pool_id {
            return Err(bad_request(format!("{} is a pool: a pool's members are other connections", c.name)));
        }
        if c.kind == "claude_code" {
            return Err(bad_request(format!("{} is Claude Code, which runs on workers: a pool's members are API connections", c.name)));
        }
        if !(1..=MAX_SLOTS).contains(&m.slots) {
            return Err(bad_request(format!("{}: calls at once is 1 to {MAX_SLOTS}", c.name)));
        }
        if out.iter().any(|o| o.connection_id == c.id) {
            return Err(bad_request(format!("{} is in the pool twice", c.name)));
        }
        out.push(api::PoolMember { connection_id: c.id.clone(), slots: m.slots });
    }
    Ok(out)
}

/// Refuses a pool where a worker dials the connection (sessions, routing
/// rules): pools are for what runs in the head.
pub fn not_for_sessions(c: &ConnectionRow, what: &str) -> anyhow::Result<()> {
    if c.kind == KIND {
        return Err(bad_request(format!(
            "{what}: {} is a pool, which only what runs in the head uses (the memex, the assistant, suggestions); sessions dial their \
             connection from their worker, so choose one of its members",
            c.name
        )));
    }
    Ok(())
}

/// The models a pool offers: each member's, together.
pub async fn models(st: &HeadState, c: &ConnectionRow) -> anyhow::Result<Vec<String>> {
    let mut all = Vec::new();
    for m in members(c) {
        let Some(mc) = st.store.connection(&m.connection_id).await? else { continue };
        match crate::api::connection_models_of(st, &mc).await {
            Ok(models) => all.extend(models),
            Err(e) => warn!(pool = %c.name, member = %mc.name, "a pool member's models: {}", e.1),
        }
    }
    all.sort();
    all.dedup();
    Ok(all)
}
