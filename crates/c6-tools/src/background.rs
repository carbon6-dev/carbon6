//! Background shell commands: started by `bash` with `run_in_background`,
//! polled with `bash_output`, stopped with `kill_shell`.

use std::collections::HashMap;
use std::io::Read as _;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use c6_llm::ToolSpec;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// Output kept per background command; older output is dropped.
const KEEP_BYTES: usize = 1024 * 1024;

pub struct BackgroundProc {
    pub command: String,
    pgid: Option<Pid>,
    out: Mutex<Buf>,
    status: Mutex<Option<i32>>,
}

#[derive(Default)]
struct Buf {
    data: Vec<u8>,
    /// Bytes dropped from the front, so `read` positions stay absolute.
    dropped: usize,
    /// Absolute position up to which the model has seen output.
    read: usize,
}

#[derive(Default)]
pub struct Background {
    procs: Mutex<HashMap<String, Arc<BackgroundProc>>>,
    next: std::sync::atomic::AtomicU64,
}

impl Background {
    pub fn spawn(&self, command: &str, cwd: &std::path::Path) -> std::io::Result<String> {
        let (reader, writer) = std::io::pipe()?;
        let shell = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        let mut child = tokio::process::Command::new(shell)
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer.try_clone()?))
            .stderr(Stdio::from(writer))
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let id = format!("bg{n}");
        let proc = Arc::new(BackgroundProc {
            command: command.to_owned(),
            pgid: child.id().map(|p| Pid::from_raw(p as i32)),
            out: Mutex::default(),
            status: Mutex::new(None),
        });
        let p = proc.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 16 * 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut out = p.out.lock().unwrap();
                out.data.extend_from_slice(&buf[..n]);
                if out.data.len() > KEEP_BYTES {
                    let excess = out.data.len() - KEEP_BYTES;
                    out.data.drain(..excess);
                    out.dropped += excess;
                }
            }
        });
        let p = proc.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
            *p.status.lock().unwrap() = Some(code);
        });
        self.procs.lock().unwrap().insert(id.clone(), proc);
        Ok(id)
    }

    fn get(&self, id: &str) -> Option<Arc<BackgroundProc>> {
        self.procs.lock().unwrap().get(id).cloned()
    }

    /// Output since the last read, and the exit status if finished.
    pub fn read_new(&self, id: &str) -> Option<(String, Option<i32>, bool)> {
        let p = self.get(id)?;
        let status = *p.status.lock().unwrap();
        let mut out = p.out.lock().unwrap();
        let start = out.read.max(out.dropped);
        let skipped = start > out.read;
        let text = String::from_utf8_lossy(&out.data[start - out.dropped..]).into_owned();
        out.read = out.dropped + out.data.len();
        Some((text, status, skipped))
    }

    pub fn command(&self, id: &str) -> Option<String> {
        self.get(id).map(|p| p.command.clone())
    }

    pub fn kill(&self, id: &str) -> Option<bool> {
        let p = self.get(id)?;
        if p.status.lock().unwrap().is_some() {
            return Some(false);
        }
        if let Some(pg) = p.pgid {
            let _ = killpg(pg, Signal::SIGTERM);
        }
        Some(true)
    }
}

pub struct BashOutput {
    name: &'static str,
    id_key: &'static str,
}

impl BashOutput {
    pub fn generic() -> Self {
        Self { name: "bash_output", id_key: "id" }
    }

    pub fn claude() -> Self {
        Self { name: "BashOutput", id_key: "bash_id" }
    }
}

#[async_trait]
impl Tool for BashOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "Returns new output from a background command started with run_in_background, and whether it has finished.".into(),
            parameters: json!({"type": "object", "properties": {self.id_key: {"type": "string", "description": "The background command's id"}},
                "required": [self.id_key], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let id = input[self.id_key].as_str().unwrap_or_default();
        let Some((text, status, skipped)) = ctx.session.background.read_new(id) else {
            return ToolOutput::err(format!("no background command {id:?}"));
        };
        let state = match status {
            None => "still running".to_owned(),
            Some(c) => format!("exited with code {c}"),
        };
        let mut out = String::new();
        if skipped {
            out.push_str("[older output was dropped]\n");
        }
        out.push_str(if text.is_empty() { "(no new output)" } else { &text });
        out.push_str(&format!("\n[{id}: {state}]"));
        ToolOutput::limited(out, false)
    }
}

pub struct KillShell {
    name: &'static str,
    id_key: &'static str,
}

impl KillShell {
    pub fn generic() -> Self {
        Self { name: "kill_shell", id_key: "id" }
    }

    pub fn claude() -> Self {
        Self { name: "KillShell", id_key: "shell_id" }
    }
}

#[async_trait]
impl Tool for KillShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "Stops a background command and its child processes.".into(),
            parameters: json!({"type": "object", "properties": {self.id_key: {"type": "string", "description": "The background command's id"}},
                "required": [self.id_key], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let id = input[self.id_key].as_str().unwrap_or_default();
        match ctx.session.background.kill(id) {
            None => ToolOutput::err(format!("no background command {id:?}")),
            Some(false) => ToolOutput::ok(format!("{id} had already exited")),
            Some(true) => ToolOutput::ok(format!("Stopped {id}")),
        }
    }
}
