//! Built-in agent tools and the `Tool` trait.

pub mod agents;
pub mod api_reference;
pub mod apps;
mod background;
mod bash;
pub mod browser;
mod files;
pub mod hooks;
pub mod kits;
pub mod knowledge;
pub mod memex;
pub mod memory;
pub mod notify;
mod patch;
mod plan;
pub mod profiles;
pub mod publish;
pub mod records;
pub mod remote;
mod search;
pub mod templates;
mod util;
mod web;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use agents::{AgentDef, ConnectionInfo, ModelChoice, SendMessage, Skill, SkillTool, Subagents, Task};
pub use background::{Background, BashOutput, KillShell, TaskEvent, TaskInfo, TaskKind, TaskSink, TaskState};
pub use bash::Bash;
pub use files::{Edit, Read, Write};
pub use hooks::Hooks;
pub use memory::{Forget, MemoryChange, MemorySink, Remember};
pub use notify::{Notice, NoticeSink, Notify};
pub use patch::ApplyPatch;
pub use plan::Plan;
pub use profiles::tools_for;
pub use publish::{Publish, PublishSink, Published};
pub use search::{Glob, Grep};
pub use web::{SearchConfig, WebFetch, WebSearch};

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
    /// Images for the model to see: (media type, bytes).
    pub images: Vec<(String, Vec<u8>)>,
}

/// Images larger than this are not shown to the model (providers refuse
/// them); the tool says so instead.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, truncated: false, images: Vec::new() }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, truncated: false, images: Vec::new() }
    }

    /// Applies the inline size limit.
    pub fn limited(content: String, is_error: bool) -> Self {
        let (content, truncated) = util::truncate_middle(&content, MAX_OUTPUT_CHARS);
        Self { content, is_error, truncated, images: Vec::new() }
    }

    /// With an image for the model to see (PNG, JPEG, GIF or WebP).
    pub fn with_image(mut self, media_type: &str, data: Vec<u8>) -> Self {
        if data.len() > MAX_IMAGE_BYTES {
            self.content.push_str(&format!("\n(The image is {} KB, too large to show; make it smaller.)", data.len() / 1024));
        } else {
            self.images.push((media_type.to_owned(), data));
        }
        self
    }
}

/// A session's secrets, shared with its background tasks.
#[derive(Clone, Default)]
pub struct Secrets(Arc<Mutex<Vec<(String, String)>>>);

impl Secrets {
    pub fn env(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }

    /// A secret's value, if it applies to the session.
    pub fn get(&self, name: &str) -> Option<String> {
        self.0.lock().unwrap().iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    /// `text` with every secret's value replaced by `[secret:NAME]`.
    pub fn redact(&self, text: &str) -> String {
        let secrets = self.0.lock().unwrap();
        let mut out = text.to_owned();
        // Longest first, so a value containing another is replaced whole.
        let mut by_len: Vec<&(String, String)> = secrets.iter().filter(|(_, v)| v.len() >= 4).collect();
        by_len.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (name, value) in by_len {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("[secret:{name}]"));
            }
        }
        out
    }
}

/// Calls the head on the session's behalf (the memex tools use it).
#[async_trait]
pub trait HeadCaller: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// State shared by every tool call in a session.
pub struct SessionTools {
    cwd: Mutex<PathBuf>,
    pub background: Background,
    subagents: Mutex<Option<Arc<dyn Subagents>>>,
    hooks: Mutex<Option<Arc<dyn Hooks>>>,
    /// Secrets: environment variables for shell commands, redacted from
    /// their output.
    secrets: Secrets,
    /// The machine the session works on over SSH, if not this one.
    ssh: Option<Arc<remote::SshHost>>,
    /// Where published files go (the session's log).
    publisher: Mutex<Option<PublishSink>>,
    /// Where memory changes go (the session's log, then the head).
    memory: Mutex<Option<MemorySink>>,
    /// Where notices go (the session's log, then your channels).
    notices: Mutex<Option<NoticeSink>>,
    /// The session's web browser, started on first use.
    pub browser: browser::BrowserSlot,
    head: Mutex<Option<Arc<dyn HeadCaller>>>,
    /// Files read (or written) this session, with the mtime we saw. Writes to
    /// existing files require a prior read and an unchanged mtime, so the
    /// model never clobbers content it has not seen.
    seen: Mutex<HashMap<PathBuf, Option<i64>>>,
}

impl SessionTools {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_ssh(cwd, None)
    }

    /// A session on another machine (`cwd` is a folder there).
    pub fn with_ssh(cwd: PathBuf, ssh: Option<Arc<remote::SshHost>>) -> Self {
        let secrets = Secrets::default();
        Self {
            cwd: Mutex::new(cwd),
            seen: Mutex::default(),
            background: Background::with_secrets(secrets.clone(), ssh.clone()),
            subagents: Mutex::default(),
            hooks: Mutex::default(),
            secrets,
            ssh,
            publisher: Mutex::default(),
            memory: Mutex::default(),
            notices: Mutex::default(),
            browser: Default::default(),
            head: Mutex::default(),
        }
    }

    pub fn set_memory(&self, m: Option<MemorySink>) {
        *self.memory.lock().unwrap() = m;
    }

    pub fn memory(&self) -> Option<MemorySink> {
        self.memory.lock().unwrap().clone()
    }

    pub fn set_head(&self, h: Option<Arc<dyn HeadCaller>>) {
        *self.head.lock().unwrap() = h;
    }

    pub fn head(&self) -> Option<Arc<dyn HeadCaller>> {
        self.head.lock().unwrap().clone()
    }

    pub fn set_notices(&self, n: Option<NoticeSink>) {
        *self.notices.lock().unwrap() = n;
    }

    pub fn notices(&self) -> Option<NoticeSink> {
        self.notices.lock().unwrap().clone()
    }

    pub fn set_publisher(&self, p: Option<PublishSink>) {
        *self.publisher.lock().unwrap() = p;
    }

    pub fn publisher(&self) -> Option<PublishSink> {
        self.publisher.lock().unwrap().clone()
    }

    pub fn ssh(&self) -> Option<Arc<remote::SshHost>> {
        self.ssh.clone()
    }

    /// Reads a file (here or on the SSH target).
    pub(crate) async fn fs_read(&self, path: &Path) -> Result<Vec<u8>, String> {
        match &self.ssh {
            Some(h) => h.read(path).await,
            None => tokio::fs::read(path).await.map_err(|e| e.to_string()),
        }
    }

    /// Writes a file, creating its folder.
    pub(crate) async fn fs_write(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        match &self.ssh {
            Some(h) => h.write(path, data).await,
            None => {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                tokio::fs::write(path, data).await.map_err(|e| e.to_string())
            }
        }
    }

    pub(crate) async fn fs_remove(&self, path: &Path) -> Result<(), String> {
        match &self.ssh {
            Some(h) => h.remove(path).await,
            None => tokio::fs::remove_file(path).await.map_err(|e| e.to_string()),
        }
    }

    /// What is at `path`, and its modification time.
    pub(crate) async fn fs_stat(&self, path: &Path) -> (remote::Kind, Option<i64>) {
        match &self.ssh {
            Some(h) => h.stat(path).await.unwrap_or((remote::Kind::Missing, None)),
            None => match tokio::fs::metadata(path).await {
                Err(_) => (remote::Kind::Missing, None),
                Ok(m) => {
                    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64);
                    (if m.is_dir() { remote::Kind::Dir } else { remote::Kind::File }, mtime)
                }
            },
        }
    }

    /// Sets the secrets shell commands get: (environment variable, value).
    pub fn set_secrets(&self, secrets: Vec<(String, String)>) {
        *self.secrets.0.lock().unwrap() = secrets;
    }

    pub fn secrets(&self) -> &Secrets {
        &self.secrets
    }

    /// Sets the hooks run around this session's tool calls.
    pub fn set_hooks(&self, h: Option<Arc<dyn Hooks>>) {
        *self.hooks.lock().unwrap() = h;
    }

    pub fn hooks(&self) -> Option<Arc<dyn Hooks>> {
        self.hooks.lock().unwrap().clone()
    }

    /// Whether the session works on another machine (over SSH).
    pub fn is_remote(&self) -> bool {
        self.ssh.is_some()
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }

    /// Sets who runs `Task` subagents for the current turn.
    pub fn set_subagents(&self, s: Option<Arc<dyn Subagents>>) {
        *self.subagents.lock().unwrap() = s;
    }

    pub fn subagents(&self) -> Option<Arc<dyn Subagents>> {
        self.subagents.lock().unwrap().clone()
    }

    pub(crate) fn set_cwd(&self, cwd: PathBuf) {
        *self.cwd.lock().unwrap() = cwd;
    }

    /// Resolves a model-supplied path against the session's cwd (on the
    /// SSH target, `~` is the remote home).
    pub fn resolve(&self, path: &str) -> PathBuf {
        let home = match &self.ssh {
            Some(h) => Some(PathBuf::from(&h.home)),
            None => dirs::home_dir(),
        };
        let p = if let Some(rest) = path.strip_prefix("~/") {
            home.map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(path))
        } else if path == "~" {
            home.unwrap_or_else(|| PathBuf::from(path))
        } else {
            PathBuf::from(path)
        };
        if p.is_absolute() { p } else { self.cwd().join(p) }
    }

    pub(crate) async fn mark_seen(&self, path: &Path) {
        let (_, mtime) = self.fs_stat(path).await;
        self.seen.lock().unwrap().insert(path.to_owned(), mtime);
    }

    /// Err with a message for the model if `path` exists but may not be
    /// overwritten yet.
    pub(crate) async fn check_writable(&self, path: &Path) -> Result<(), String> {
        let (kind, mtime) = self.fs_stat(path).await;
        if kind == remote::Kind::Missing {
            return Ok(());
        }
        match self.seen.lock().unwrap().get(path) {
            None => Err(format!("{} exists but has not been read in this session. Read it first.", path.display())),
            Some(seen) if *seen != mtime => {
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
    /// The model's id for this call.
    pub call_id: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn concurrency(&self) -> Concurrency;
    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput;
}

/// The generic tool set (for models without a family-specific profile).
pub fn default_tools() -> Vec<Arc<dyn Tool>> {
    tools_for(c6_llm::ToolProfile::Generic)
}

pub(crate) fn parse_input<T: DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|e| ToolOutput::err(format!("invalid input: {e}")))
}
