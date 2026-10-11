//! Interactive terminals on this worker. A terminal outlives any viewer: it
//! keeps a scrollback buffer, and each attach replays it before streaming
//! live output. Terminals end when their shell exits or the worker stops.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use bytes::Bytes;
use c6_proto::{PtyExit, PtyFrame, PtyOpen, pty_frame, worker_hub_client::WorkerHubClient};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

const SCROLLBACK_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: String,
    pub cwd: String,
    pub created_at: i64,
    pub exited: Option<i32>,
}

struct Terminal {
    info: TerminalInfo,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Scrollback and the live channel share a lock so an attach sees every
    /// byte exactly once.
    output: Mutex<(VecDeque<u8>, broadcast::Sender<Bytes>)>,
    exited: watch::Sender<Option<i32>>,
}

#[derive(Default)]
pub struct Terminals {
    map: Mutex<HashMap<String, Arc<Terminal>>>,
}

#[derive(Deserialize)]
pub struct OpenParams {
    pub cwd: Option<String>,
    /// A shell on this SSH target instead (`cwd` is a folder there).
    #[serde(default)]
    pub ssh: Option<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    120
}

fn default_rows() -> u16 {
    32
}

impl Terminals {
    pub fn open(&self, p: OpenParams) -> anyhow::Result<TerminalInfo> {
        let ssh = p.ssh.filter(|t| !t.trim().is_empty());
        let remote_dir = p.cwd.clone().unwrap_or_default();
        let cwd = if ssh.is_some() {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        } else {
            p.cwd.filter(|c| !c.is_empty()).map(|c| crate::expand_home(&c)).or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("/"))
        };
        if !cwd.is_dir() {
            anyhow::bail!("{} is not a directory on this worker", cwd.display());
        }
        let pair = native_pty_system().openpty(PtySize { rows: p.rows, cols: p.cols, pixel_width: 0, pixel_height: 0 })?;
        let mut cmd = match &ssh {
            // A login shell on the target, in the session's folder there.
            Some(target) => {
                let mut c = CommandBuilder::new(std::env::var("C6_SSH_BIN").unwrap_or_else(|_| "ssh".into()));
                let start = if remote_dir.is_empty() {
                    "exec \"${SHELL:-/bin/sh}\" -l".to_owned()
                } else {
                    format!("cd {} 2>/dev/null; exec \"${{SHELL:-/bin/sh}}\" -l", c6_tools::remote::quote(&remote_dir))
                };
                c.args(["-t", "-o", "ServerAliveInterval=30", target.as_str(), start.as_str()]);
                c
            }
            None => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
                let mut c = CommandBuilder::new(&shell);
                c.arg("-l");
                c
            }
        };
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let mut child = pair.slave.spawn_command(cmd).context("starting shell")?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let id = uuid::Uuid::now_v7().to_string();
        let shown = match &ssh {
            Some(t) => format!("{t}:{remote_dir}"),
            None => cwd.display().to_string(),
        };
        let info = TerminalInfo { id: id.clone(), cwd: shown, created_at: c6_core::now_ms(), exited: None };
        let term = Arc::new(Terminal {
            info: info.clone(),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            output: Mutex::new((VecDeque::new(), broadcast::channel(256).0)),
            exited: watch::channel(None).0,
        });
        self.map.lock().unwrap().insert(id.clone(), term.clone());

        let t = term.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut out = t.output.lock().unwrap();
                        out.0.extend(&buf[..n]);
                        let excess = out.0.len().saturating_sub(SCROLLBACK_BYTES);
                        out.0.drain(..excess);
                        let _ = out.1.send(Bytes::copy_from_slice(&buf[..n]));
                    }
                }
            }
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            t.exited.send_replace(Some(code));
        });
        info!(terminal = %id, cwd = %info.cwd, "terminal opened");
        Ok(info)
    }

    pub fn list(&self) -> Vec<TerminalInfo> {
        let mut v: Vec<TerminalInfo> =
            self.map.lock().unwrap().values().map(|t| TerminalInfo { exited: *t.exited.borrow(), ..t.info.clone() }).collect();
        v.sort_by_key(|t| t.created_at);
        v
    }

    pub fn close(&self, id: &str) -> bool {
        // Dropping the master hangs up the shell (SIGHUP).
        self.map.lock().unwrap().remove(id).is_some()
    }

    /// Opens a `Pty` stream to the head for `id`, tagged with the head's
    /// attach token, and bridges it until either side ends.
    pub async fn attach(&self, channel: tonic::transport::Channel, id: &str, token: String) -> anyhow::Result<()> {
        let term = self.map.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("no such terminal"))?;
        let (tx, rx) = mpsc::channel::<PtyFrame>(256);
        let frame = |f| PtyFrame { frame: Some(f) };
        tx.send(frame(pty_frame::Frame::Open(PtyOpen { terminal_id: id.to_owned(), attach_token: token }))).await?;
        let (snapshot, mut live) = {
            let out = term.output.lock().unwrap();
            (out.0.iter().copied().collect::<Vec<u8>>(), out.1.subscribe())
        };
        if !snapshot.is_empty() {
            tx.send(frame(pty_frame::Frame::Data(snapshot))).await?;
        }
        let mut inbound = WorkerHubClient::new(channel).pty(ReceiverStream::new(rx)).await?.into_inner();

        let mut exited = term.exited.subscribe();
        let already = *exited.borrow_and_update(); // don't hold the borrow across await
        if let Some(code) = already {
            tx.send(frame(pty_frame::Frame::Exit(PtyExit { code }))).await?;
        }
        let out_tx = tx.clone();
        let pump = tokio::spawn(async move {
            loop {
                tokio::select! {
                    chunk = live.recv() => match chunk {
                        Ok(b) => if out_tx.send(frame(pty_frame::Frame::Data(b.to_vec()))).await.is_err() { return },
                        Err(broadcast::error::RecvError::Lagged(n)) => warn!(skipped = n, "terminal viewer fell behind"),
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    r = exited.changed() => {
                        let code = *exited.borrow();
                        if r.is_err() || code.is_some() {
                            let code = code.unwrap_or(-1);
                            let _ = out_tx.send(frame(pty_frame::Frame::Exit(PtyExit { code }))).await;
                            return;
                        }
                    }
                }
            }
        });

        while let Ok(Some(f)) = inbound.message().await {
            match f.frame {
                Some(pty_frame::Frame::Data(d)) => {
                    let mut w = term.writer.lock().unwrap();
                    let _ = w.write_all(&d).and_then(|_| w.flush());
                }
                Some(pty_frame::Frame::Resize(s)) if s.cols > 0 && s.rows > 0 => {
                    let _ = term.master.lock().unwrap().resize(PtySize {
                        rows: s.rows as u16,
                        cols: s.cols as u16,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                _ => {}
            }
        }
        pump.abort();
        Ok(())
    }
}
