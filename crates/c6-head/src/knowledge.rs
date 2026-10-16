//! Knowledge in the head (docs/knowledge.md): the knowledge graph is part of
//! the head, not a service beside it. The memex's store (items, things,
//! claims, facts, search and vectors, one database per realm) lives under
//! the head's data folder, and its link calls the head in this process
//! instead of over the fabric.

use std::sync::{Arc, Weak};

use c6_proto::{CredentialGrant, CredentialRequest};
use serde_json::Value;

use crate::HeadState;

/// The head, as the memex's link sees it.
struct Local {
    st: Weak<HeadState>,
}

#[async_trait::async_trait]
impl c6_worker::link::LocalHead for Local {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let st = self.st.upgrade().ok_or("the head stopped")?;
        crate::fabric::memex_call(&st, method, params).await.map_err(|e| format!("{e:#}"))
    }

    async fn credential(&self, connection_id: &str) -> CredentialGrant {
        let req = CredentialRequest { request_id: 0, connection_id: connection_id.to_owned() };
        match self.st.upgrade() {
            Some(st) => crate::sessions::credential_for(&st, "memex", None, "head", req).await,
            None => CredentialGrant { connection_id: connection_id.to_owned(), error: "the head stopped".into(), ..Default::default() },
        }
    }
}

/// The knowledge folder in the head's data folder.
pub fn dir(st: &HeadState) -> std::path::PathBuf {
    st.data_dir.join("knowledge")
}

/// Opens the knowledge store and starts its work (the head always has it).
pub async fn open(st: &Arc<HeadState>) -> anyhow::Result<()> {
    let link = Arc::new(c6_worker::link::HeadLink::local(Arc::new(Local { st: Arc::downgrade(st) })));
    let m = c6_memex::Memex::open(&dir(st), link).await?;
    m.spawn_jobs();
    let _ = st.memex.set(m);
    Ok(())
}

/// The knowledge store, once open.
pub fn get(st: &HeadState) -> Option<Arc<c6_memex::Memex>> {
    st.memex.get().cloned()
}
