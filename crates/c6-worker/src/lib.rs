//! The carbon6 worker (`c6w`): a headless daemon that attaches to the head and
//! runs agent sessions on this machine.

pub mod checkpoint;
pub mod claude_code;
pub mod commands;
pub mod conn;
pub mod enroll;
pub mod hooks;
pub mod identity;
pub mod imports;
pub mod journal;
pub mod link;
mod mcp_bridge;
pub mod mentions;
pub mod policy;
pub mod pty;
pub mod rpc;
pub mod session;
mod subagent;
pub mod update;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;

pub use conn::ConnState;
pub use identity::Identity;

/// Idle, synced sessions stay in the journal this long.
const JOURNAL_KEEP_MS: i64 = 7 * 24 * 3600 * 1000;

/// More of what a node does, beyond running sessions: its own RPC methods
/// and capabilities (the memex is a node like this).
#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    /// Added to the Hello (e.g. "memex").
    fn capabilities(&self) -> Vec<String>;
    /// Handles a method the worker itself does not; None: not ours either.
    async fn handle(&self, method: &str, params: serde_json::Value) -> Option<anyhow::Result<serde_json::Value>>;
}

/// What tests set on a worker instead of the environment (docs/testing.md).
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    /// Session imports: whose home to read, how often, and how long a file
    /// must be unchanged.
    pub import_home: Option<PathBuf>,
    pub import_every: Option<std::time::Duration>,
    pub import_settle: Option<std::time::Duration>,
    /// Updates (debug builds only): the binary to replace, the release keys,
    /// and the version to say it runs.
    pub update_exe: Option<PathBuf>,
    pub update_keys: Option<Vec<String>>,
    pub update_as_version: Option<String>,
}

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
    extension: Mutex<Option<Arc<dyn Extension>>>,
    /// Updating to a release the head says is out (docs/updates.md).
    pub update: update::Updating,
    /// Settings tests give this worker (never the process's environment,
    /// which the tests running beside them share).
    pub overrides: Mutex<Overrides>,
    /// Held for the process lifetime: one worker per state directory.
    _lock: std::fs::File,
}

impl Worker {
    pub async fn open(state_dir: &Path, identity: Identity) -> anyhow::Result<Arc<Self>> {
        c6_llm::install_crypto_provider();
        c6_core::create_private_dir(state_dir)?;
        let lock_path = state_dir.join("c6w.lock");
        let lock = std::fs::File::create(&lock_path).with_context(|| format!("creating {}", lock_path.display()))?;
        if lock.try_lock().is_err() {
            anyhow::bail!("another c6w is already running with state directory {}", state_dir.display());
        }
        let journal = Arc::new(journal::Journal::open(&state_dir.join("journal.db")).await?);
        let link = Arc::new(link::HeadLink::default());
        let sessions = session::SessionManager::new(journal.clone(), link.clone(), state_dir.join("worktrees"));
        sessions.recover().await?;
        // Keep the journal small: idle, fully synced sessions can be restored
        // from the head when they are used again.
        let orphans = journal.drop_orphans().await?;
        if orphans > 0 {
            tracing::warn!(orphans, "dropped journal events of sessions with no session row; the head will restore them");
        }
        let trimmed = journal.trim(c6_core::now_ms() - JOURNAL_KEEP_MS, &[]).await?;
        if trimmed > 0 {
            tracing::info!(trimmed, "trimmed idle sessions from the journal");
        }
        Ok(Arc::new(Self {
            identity,
            state_dir: state_dir.to_owned(),
            journal,
            link,
            sessions,
            terminals: pty::Terminals::default(),
            channel: Mutex::new(None),
            extension: Mutex::new(None),
            update: Default::default(),
            overrides: Default::default(),
            _lock: lock,
        }))
    }

    /// Adds an extension (before connecting, so its capabilities are in the
    /// Hello).
    pub fn set_extension(&self, ext: Arc<dyn Extension>) {
        *self.extension.lock().unwrap() = Some(ext);
    }

    pub fn extension(&self) -> Option<Arc<dyn Extension>> {
        self.extension.lock().unwrap().clone()
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
