//! The carbon6 worker (`c6w`): a headless daemon that attaches to the head and
//! runs agent sessions on this machine.

pub mod claude_code;
pub mod conn;
pub mod enroll;
pub mod identity;
pub mod journal;
pub mod link;
pub mod pty;
pub mod rpc;
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use conn::ConnState;
pub use identity::Identity;

/// Everything a running worker shares between its connection and sessions.
pub struct Worker {
    pub identity: Identity,
    pub state_dir: PathBuf,
    pub journal: Arc<journal::Journal>,
    pub link: Arc<link::HeadLink>,
    pub sessions: session::SessionManager,
    pub terminals: pty::Terminals,
    /// The live connection to the head, for opening extra streams.
    channel: Mutex<Option<tonic::transport::Channel>>,
}

impl Worker {
    pub async fn open(state_dir: &Path, identity: Identity) -> anyhow::Result<Arc<Self>> {
        c6_llm::install_crypto_provider();
        let journal = Arc::new(journal::Journal::open(&state_dir.join("journal.db")).await?);
        let link = Arc::new(link::HeadLink::default());
        let sessions = session::SessionManager::new(journal.clone(), link.clone(), state_dir.join("worktrees"));
        sessions.recover().await?;
        Ok(Arc::new(Self {
            identity,
            state_dir: state_dir.to_owned(),
            journal,
            link,
            sessions,
            terminals: pty::Terminals::default(),
            channel: Mutex::new(None),
        }))
    }

    pub fn channel(&self) -> Option<tonic::transport::Channel> {
        self.channel.lock().unwrap().clone()
    }

    pub(crate) fn set_channel(&self, c: Option<tonic::transport::Channel>) {
        *self.channel.lock().unwrap() = c;
    }
}

/// Expands a leading `~` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            dirs::home_dir().map(|h| h.join(rest.trim_start_matches('/'))).unwrap_or_else(|| PathBuf::from(path))
        }
        _ => PathBuf::from(path),
    }
}

pub(crate) fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}
