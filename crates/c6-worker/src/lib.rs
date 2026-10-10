//! The carbon6 worker (`c6w`): a headless daemon that attaches to the head and
//! runs agent sessions on this machine.

pub mod conn;
pub mod enroll;
pub mod identity;
pub mod journal;
pub mod link;
pub mod session;

use std::path::Path;
use std::sync::Arc;

pub use conn::ConnState;
pub use identity::Identity;

/// Everything a running worker shares between its connection and sessions.
pub struct Worker {
    pub identity: Identity,
    pub journal: Arc<journal::Journal>,
    pub link: Arc<link::HeadLink>,
    pub sessions: session::SessionManager,
}

impl Worker {
    pub async fn open(state_dir: &Path, identity: Identity) -> anyhow::Result<Arc<Self>> {
        c6_llm::install_crypto_provider();
        let journal = Arc::new(journal::Journal::open(&state_dir.join("journal.db")).await?);
        let link = Arc::new(link::HeadLink::default());
        let sessions = session::SessionManager::new(journal.clone(), link.clone());
        sessions.recover().await?;
        Ok(Arc::new(Self { identity, journal, link, sessions }))
    }
}

pub(crate) fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}
