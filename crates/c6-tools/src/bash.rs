//! bash: run a shell command in the session's working directory.

use std::io::Read as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use c6_llm::ToolSpec;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{Concurrency, Tool, ToolCtx, ToolOutput, parse_input};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Hard cap on captured bytes; inline output is further limited.
const MAX_CAPTURE: usize = 8 * 1024 * 1024;
/// After the shell exits, how long to keep reading output that background
/// children may still be writing to the shared pipe.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

pub struct Bash;

#[derive(Deserialize)]
struct BashInput {
    command: String,
    timeout_ms: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
    /// With run_in_background: tell the agent when it ends.
    #[serde(default = "yes")]
    notify: bool,
}

fn yes() -> bool {
    true
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a bash command and return its combined stdout and stderr. The working directory persists \
                          between calls (a `cd` carries over); environment changes do not. Commands are non-interactive: \
                          stdin is closed, so avoid commands that prompt. Default timeout 120000 ms, maximum 600000 ms. \
                          Prefer the read, grep and glob tools over cat, grep and find."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "timeout_ms": {"type": "integer", "minimum": 1, "description": "Timeout in milliseconds"},
                    "run_in_background": {"type": "boolean", "description": "Start the command in the background (servers, watchers, long builds) and return an id for bash_output / kill_shell. You are told when it finishes, so you need not poll."},
                    "notify": {"type": "boolean", "description": "With run_in_background: be told when it finishes (default true). Set false for servers and watchers meant to keep running."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let input: BashInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        if input.run_in_background {
            return match ctx.session.background.spawn(&input.command, &ctx.session.cwd(), input.notify) {
                Ok(id) => ToolOutput::ok(if input.notify {
                    format!(
                        "Started in the background as {id}. You will be told when it finishes (with the end of its output); meanwhile, read its output with the output tool or stop it with the kill tool."
                    )
                } else {
                    format!("Started in the background as {id}. Read its output with the output tool and stop it with the kill tool.")
                }),
                Err(e) => ToolOutput::err(format!("failed to start: {e}")),
            };
        }
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS));
        match run(ctx, &input.command, timeout).await {
            Ok(out) => out,
            Err(e) => ToolOutput::err(format!("failed to run command: {e}")),
        }
    }
}

async fn run(ctx: &ToolCtx, command: &str, timeout: Duration) -> std::io::Result<ToolOutput> {
    let cwd = ctx.session.cwd();
    let cwd_file = std::env::temp_dir().join(format!("c6-cwd-{}-{}", std::process::id(), unique()));
    // The EXIT trap records the final directory even if the command calls `exit`.
    let script = format!("trap 'pwd -P > \"$__C6_CWD_FILE\" 2>/dev/null' EXIT\n{command}\n");
    let shell = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };

    // One pipe for both streams keeps stdout and stderr interleaved in order.
    let (reader, writer) = std::io::pipe()?;
    let mut child = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(&script)
        .current_dir(&cwd)
        .env("__C6_CWD_FILE", &cwd_file)
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        .envs(ctx.session.secrets().env())
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?))
        .stderr(Stdio::from(writer))
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pgid = child.id().map(|id| Pid::from_raw(id as i32));

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // A thread, because the pipe is blocking and background children may hold
    // it open indefinitely; it exits once every writer is gone.
    std::thread::spawn(move || read_pipe(reader, tx));

    let started = Instant::now();
    let mut captured = Vec::new();
    let on_chunk = |chunk: Vec<u8>, captured: &mut Vec<u8>| {
        (ctx.output)(&ctx.session.secrets().redact(&String::from_utf8_lossy(&chunk)));
        let room = MAX_CAPTURE.saturating_sub(captured.len());
        captured.extend_from_slice(&chunk[..chunk.len().min(room)]);
    };

    enum End {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
    }
    let end = loop {
        tokio::select! {
            status = child.wait() => break End::Exited(status?),
            Some(chunk) = rx.recv() => on_chunk(chunk, &mut captured),
            _ = tokio::time::sleep(timeout.saturating_sub(started.elapsed())) => break End::TimedOut,
            _ = ctx.cancel.cancelled() => break End::Cancelled,
        }
    };
    if !matches!(end, End::Exited(_))
        && let Some(pgid) = pgid
    {
        let _ = killpg(pgid, Signal::SIGKILL);
        let _ = child.wait().await;
    }
    let grace = tokio::time::sleep(DRAIN_GRACE);
    tokio::pin!(grace);
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(c) => on_chunk(c, &mut captured),
                None => break,
            },
            _ = &mut grace => break,
        }
    }

    if let Ok(dir) = std::fs::read_to_string(&cwd_file) {
        let dir = PathBuf::from(dir.trim());
        if dir.is_dir() {
            ctx.session.set_cwd(dir);
        }
    }
    let _ = std::fs::remove_file(&cwd_file);

    let mut text = ctx.session.secrets().redact(&String::from_utf8_lossy(&captured));
    if captured.len() >= MAX_CAPTURE {
        text.push_str("\n[output capture limit reached]");
    }
    let (suffix, is_error) = match end {
        End::Exited(s) => match s.code() {
            Some(0) => (String::new(), false),
            Some(c) => (format!("[exit code {c}]"), false),
            None => ("[terminated by signal]".to_owned(), false),
        },
        End::TimedOut => (format!("[timed out after {} ms; process group killed]", timeout.as_millis()), true),
        End::Cancelled => ("[interrupted; process group killed]".to_owned(), true),
    };
    if !suffix.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&suffix);
    }
    if text.is_empty() {
        text = "(no output)".into();
    }
    Ok(ToolOutput::limited(text, is_error))
}

fn read_pipe(mut reader: std::io::PipeReader, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::SessionTools;

    fn ctx(dir: &std::path::Path, streamed: Arc<Mutex<String>>) -> ToolCtx {
        ToolCtx {
            session: Arc::new(SessionTools::new(dir.to_owned())),
            cancel: Default::default(),
            output: Arc::new(move |s: &str| streamed.lock().unwrap().push_str(s)),
            call_id: String::new(),
        }
    }

    #[tokio::test]
    async fn output_exit_code_and_cwd_persistence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let streamed = Arc::new(Mutex::new(String::new()));
        let ctx = ctx(dir.path(), streamed.clone());

        let r = Bash.call(&ctx, json!({"command": "echo out; echo err >&2; exit 3"})).await;
        assert_eq!(r.content, "out\nerr\n[exit code 3]");
        assert_eq!(*streamed.lock().unwrap(), "out\nerr\n");

        Bash.call(&ctx, json!({"command": "cd sub"})).await;
        let r = Bash.call(&ctx, json!({"command": "pwd -P"})).await;
        assert!(r.content.trim().ends_with("/sub"), "{r:?}");
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), Default::default());
        let started = Instant::now();
        let r = Bash.call(&ctx, json!({"command": "sleep 30 & sleep 30", "timeout_ms": 300})).await;
        assert!(r.is_error && r.content.contains("timed out"), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn run_in_background_then_read_and_kill() {
        use crate::{BashOutput, KillShell};
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), Default::default());
        let r = Bash.call(&ctx, json!({"command": "echo first; sleep 30", "run_in_background": true})).await;
        assert!(r.content.contains("bg1"), "{r:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r = BashOutput::generic().call(&ctx, json!({"id": "bg1"})).await;
        assert!(r.content.starts_with("first\n") && r.content.contains("still running"), "{r:?}");
        let r = BashOutput::generic().call(&ctx, json!({"id": "bg1"})).await;
        assert!(r.content.starts_with("(no new output)"), "{r:?}");
        let r = KillShell::generic().call(&ctx, json!({"id": "bg1"})).await;
        assert_eq!(r.content, "Stopped bg1");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r = BashOutput::generic().call(&ctx, json!({"id": "bg1"})).await;
        assert!(r.content.contains("exited"), "{r:?}");
    }

    #[tokio::test]
    async fn background_child_does_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), Default::default());
        let started = Instant::now();
        let r = Bash.call(&ctx, json!({"command": "(sleep 5 &) ; echo started"})).await;
        assert_eq!(r.content, "started\n");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn background_tasks_report_when_they_end() {
        use crate::{KillShell, TaskKind, TaskState};
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), Default::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.session.background.set_sink(Some(tx));
        let next = async |rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::TaskEvent>| {
            tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap()
        };

        Bash.call(&ctx, json!({"command": "for i in 1 2 3; do echo line $i; done; exit 3", "run_in_background": true})).await;
        let started = next(&mut rx).await;
        assert_eq!((started.id.as_str(), started.kind, started.state, started.notify), ("bg1", TaskKind::Shell, TaskState::Started, true));
        let done = next(&mut rx).await;
        assert_eq!((done.state, done.exit_code), (TaskState::Finished, Some(3)));
        assert_eq!(done.output, "line 1\nline 2\nline 3", "the end of its output");

        // Stopped by the agent, or by the user; a quiet one says so.
        Bash.call(&ctx, json!({"command": "sleep 30", "run_in_background": true, "notify": false})).await;
        assert!(!next(&mut rx).await.notify);
        KillShell::generic().call(&ctx, json!({"id": "bg2"})).await;
        assert_eq!(next(&mut rx).await.state, TaskState::Stopped);
        Bash.call(&ctx, json!({"command": "sleep 30", "run_in_background": true})).await;
        next(&mut rx).await;
        assert_eq!(ctx.session.background.kill_by_user("bg3"), Some(true));
        assert_eq!(next(&mut rx).await.state, TaskState::StoppedByUser);
        assert!(ctx.session.background.list().iter().all(|t| !t.running));

        // A background subagent's report.
        let (id, _cancel) = ctx.session.background.start_agent("look around", true);
        assert_eq!(next(&mut rx).await.kind, TaskKind::Agent);
        ctx.session.background.finish_agent(&id, Ok("found it".into()));
        let done = next(&mut rx).await;
        assert_eq!((done.state, done.exit_code, done.output.as_str()), (TaskState::Finished, Some(0), "found it"));
    }

    #[tokio::test]
    async fn secrets_are_usable_but_never_shown() {
        use crate::BashOutput;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), Default::default());
        ctx.session.set_secrets(vec![("API_TOKEN".into(), "s3cr3t-value-123".into())]);
        let r = Bash.call(&ctx, json!({"command": "test \"$API_TOKEN\" = s3cr3t-value-123 && echo ok; echo token=$API_TOKEN"})).await;
        assert_eq!(r.content, "ok\ntoken=[secret:API_TOKEN]\n", "{r:?}");
        Bash.call(&ctx, json!({"command": "echo bg=$API_TOKEN", "run_in_background": true})).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r = BashOutput::generic().call(&ctx, json!({"id": "bg1"})).await;
        assert!(r.content.starts_with("bg=[secret:API_TOKEN]") && !r.content.contains("s3cr3t"), "{r:?}");
    }
}
