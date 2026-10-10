//! Built-in agent tools and the `Tool` trait.

mod bash;
mod files;
mod search;
mod util;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use bash::Bash;
pub use files::{Edit, Read, Write};
pub use search::{Glob, Grep};

/// Output kept inline for the model; the rest is elided from the middle.
pub const MAX_OUTPUT_CHARS: usize = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Concurrency {
    /// Read-only; may run alongside other shared tools.
    Shared,
    /// Mutates state; runs alone, in the order the model issued it.
    Exclusive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub truncated: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, truncated: false }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, truncated: false }
    }

    /// Applies the inline size limit.
    pub fn limited(content: String, is_error: bool) -> Self {
        let (content, truncated) = util::truncate_middle(&content, MAX_OUTPUT_CHARS);
        Self { content, is_error, truncated }
    }
}

/// State shared by every tool call in a session.
#[derive(Debug)]
pub struct SessionTools {
    cwd: Mutex<PathBuf>,
    /// Files read (or written) this session, with the mtime we saw. Writes to
    /// existing files require a prior read and an unchanged mtime, so the
    /// model never clobbers content it has not seen.
    seen: Mutex<HashMap<PathBuf, Option<SystemTime>>>,
}

impl SessionTools {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd: Mutex::new(cwd), seen: Mutex::default() }
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }

    pub(crate) fn set_cwd(&self, cwd: PathBuf) {
        *self.cwd.lock().unwrap() = cwd;
    }

    /// Resolves a model-supplied path against the session's cwd.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = if let Some(rest) = path.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(path))
        } else {
            PathBuf::from(path)
        };
        if p.is_absolute() { p } else { self.cwd().join(p) }
    }

    pub(crate) fn mark_seen(&self, path: &Path) {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        self.seen.lock().unwrap().insert(path.to_owned(), mtime);
    }

    /// Err with a message for the model if `path` exists but may not be
    /// overwritten yet.
    pub(crate) fn check_writable(&self, path: &Path) -> Result<(), String> {
        let Ok(meta) = std::fs::metadata(path) else { return Ok(()) };
        match self.seen.lock().unwrap().get(path) {
            None => Err(format!("{} exists but has not been read in this session. Read it first.", path.display())),
            Some(seen) if *seen != meta.modified().ok() => {
                Err(format!("{} has changed on disk since it was last read. Read it again first.", path.display()))
            }
            Some(_) => Ok(()),
        }
    }
}

/// Receives streaming output (e.g. shell output) for live display.
pub type OutputSink = Arc<dyn Fn(&str) + Send + Sync>;

pub struct ToolCtx {
    pub session: Arc<SessionTools>,
    pub cancel: CancellationToken,
    pub output: OutputSink,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn concurrency(&self) -> Concurrency;
    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput;
}

/// The default tool set.
pub fn default_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Read), Arc::new(Write), Arc::new(Edit), Arc::new(Bash), Arc::new(Grep), Arc::new(Glob)]
}

pub(crate) fn parse_input<T: DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|e| ToolOutput::err(format!("invalid input: {e}")))
}
