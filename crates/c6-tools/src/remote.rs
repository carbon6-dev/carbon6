//! SSH targets: a session's commands and files on another machine, reached
//! from the worker with its own OpenSSH setup (keys, agent, ~/.ssh/config).
//!
//! One multiplexed connection per target (ControlMaster) keeps commands
//! fast. Scripts go over stdin into a private temporary file that deletes
//! itself as it starts, so nothing secret appears in a remote command line;
//! the script then runs with stdin from /dev/null.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

/// Marks the line on which a remote shell reports its final directory.
pub const CWD_MARK: &str = "__c6_cwd_6f1e__:";

pub struct SshHost {
    /// `user@host`, `host`, or an alias from ~/.ssh/config.
    pub target: String,
    /// The remote home folder (for `~/` paths).
    pub home: String,
    control_dir: PathBuf,
}

/// `s` quoted for a POSIX shell.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What is at a remote path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Missing,
    File,
    Dir,
}

/// What to pass ssh for a target: `user@host:port` is not something ssh
/// takes, but `ssh://user@host:port` is (OpenSSH 7.7+).
pub fn destination(target: &str) -> String {
    match target.rsplit_once(':') {
        Some((host, port))
            if !target.contains("://") && !host.contains(':') && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            format!("ssh://{target}")
        }
        _ => target.to_owned(),
    }
}

impl SshHost {
    pub fn new(target: &str, home: &str) -> Self {
        // Unix socket paths are short (about 104 bytes on macOS): keep the
        // control directory near the root.
        // SAFETY: getuid cannot fail and has no side effects.
        let uid = unsafe { libc_getuid() };
        Self { target: target.to_owned(), home: home.to_owned(), control_dir: PathBuf::from(format!("/tmp/c6-ssh-{uid}")) }
    }

    /// The ssh program (C6_SSH_BIN overrides, for tests).
    fn program() -> String {
        std::env::var("C6_SSH_BIN").unwrap_or_else(|_| "ssh".into())
    }

    /// An ssh command for `remote` (a shell command line on the target).
    pub fn command(&self, remote: &str) -> tokio::process::Command {
        let _ = std::fs::create_dir_all(&self.control_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.control_dir, std::fs::Permissions::from_mode(0o700));
        }
        let mut c = tokio::process::Command::new(Self::program());
        c.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=30", "-o", "ControlMaster=auto"])
            .arg("-o")
            .arg(format!("ControlPath={}/%C", self.control_dir.display()))
            .args(["-o", "ControlPersist=600"])
            .arg(destination(&self.target))
            .arg(remote)
            .kill_on_drop(true);
        c
    }

    /// The remote command line that runs a script sent on stdin, without
    /// the script (or its secrets) ever being on a command line.
    pub fn script_runner() -> &'static str {
        // bash when there is one (the agent writes bash), else sh.
        r#"f=$(mktemp "${TMPDIR:-/tmp}/c6.XXXXXX") && cat > "$f" && if command -v bash >/dev/null 2>&1; then exec bash "$f" </dev/null; else exec sh "$f" </dev/null; fi"#
    }

    /// The script that runs `command` in `cwd` with `env`, recording its
    /// process (group) id in `pidfile` so it can be stopped, and reporting
    /// the final directory on a marked line (when `track_cwd`).
    pub fn script(command: &str, cwd: &Path, env: &[(String, String)], track_cwd: bool, pidfile: &str) -> String {
        let pf = quote(pidfile);
        let mut s = format!("rm -f \"$0\"\necho $$ > {pf}\n");
        for (k, v) in env {
            s.push_str(&format!("export {k}={}\n", quote(v)));
        }
        s.push_str("export PAGER=cat GIT_PAGER=cat GIT_TERMINAL_PROMPT=0 NO_COLOR=1 DEBIAN_FRONTEND=noninteractive\n");
        s.push_str(&format!("cd {} 2>/dev/null || cd\n", quote(&cwd.display().to_string())));
        if track_cwd {
            s.push_str(&format!("trap 'printf \"\\n{CWD_MARK}%s\\n\" \"$(pwd -P)\"; rm -f {pf}' EXIT\n", pf = pf.replace('\'', "'\\''")));
        } else {
            s.push_str(&format!("trap 'rm -f {}' EXIT\n", pf.replace('\'', "'\\''")));
        }
        s.push_str(command);
        s.push('\n');
        s
    }

    /// A fresh path for a remote process id file.
    pub fn pidfile() -> String {
        let mut b = [0u8; 12];
        getrandom(&mut b);
        format!("/tmp/c6-{}.pid", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
    }

    /// Stops the process group recorded in `pidfile` on the target.
    pub async fn kill(&self, pidfile: &str) {
        let line = format!("p=$(cat {pf} 2>/dev/null) && {{ kill -TERM -- -$p 2>/dev/null || kill -TERM $p; }}", pf = quote(pidfile));
        let _ = self.sh(&line, b"").await;
    }

    /// Runs `remote` with `stdin`; (exit code, stdout, stderr).
    pub async fn run(&self, remote: &str, stdin: &[u8], timeout: Duration) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
        let mut child = self
            .command(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run ssh: {e}"))?;
        if let Some(mut i) = child.stdin.take() {
            i.write_all(stdin).await.map_err(|e| format!("ssh: {e}"))?;
        }
        let out = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| format!("ssh to {} timed out", self.target))?
            .map_err(|e| format!("ssh: {e}"))?;
        Ok((out.status.code().unwrap_or(-1), out.stdout, out.stderr))
    }

    /// Runs a shell command line remotely; Err with stderr on failure
    /// (exit 255 is ssh itself failing to connect).
    pub async fn sh(&self, line: &str, stdin: &[u8]) -> Result<Vec<u8>, String> {
        let (code, out, err) = self.run(line, stdin, Duration::from_secs(120)).await?;
        match code {
            0 => Ok(out),
            255 => Err(format!("cannot reach {}: {}", self.target, String::from_utf8_lossy(&err).trim())),
            _ => Err(String::from_utf8_lossy(&err).trim().to_owned()),
        }
    }

    pub async fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        self.sh(&format!("cat -- {}", quote(&path.display().to_string())), b"").await
    }

    pub async fn write(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        let p = quote(&path.display().to_string());
        let dir = quote(&path.parent().unwrap_or(Path::new("/")).display().to_string());
        self.sh(&format!("mkdir -p -- {dir} && cat > {p}"), data).await.map(drop)
    }

    pub async fn remove(&self, path: &Path) -> Result<(), String> {
        self.sh(&format!("rm -f -- {}", quote(&path.display().to_string())), b"").await.map(drop)
    }

    /// What is at `path`, and its modification time (seconds) if anything.
    pub async fn stat(&self, path: &Path) -> Result<(Kind, Option<i64>), String> {
        let p = quote(&path.display().to_string());
        let line = format!(
            "if [ -d {p} ]; then echo d; elif [ -e {p} ]; then echo f; else echo n; fi; stat -c %Y -- {p} 2>/dev/null || stat -f %m -- {p} 2>/dev/null || true"
        );
        let out = String::from_utf8_lossy(&self.sh(&line, b"").await?).into_owned();
        let mut lines = out.lines();
        let kind = match lines.next() {
            Some("d") => Kind::Dir,
            Some("f") => Kind::File,
            _ => Kind::Missing,
        };
        Ok((kind, lines.next().and_then(|l| l.trim().parse().ok())))
    }

    /// Connects and returns (home folder, folder `dir` resolves to, a
    /// one-line description of the system).
    pub async fn probe(&self, dir: &str) -> Result<(String, String, String), String> {
        let dir = if dir.trim().is_empty() { "~".to_owned() } else { dir.trim().to_owned() };
        let cd = match dir.strip_prefix('~') {
            Some(rest) => format!("cd \"$HOME\"{}", quote(rest)),
            None => format!("cd {}", quote(&dir)),
        };
        let line = format!(
            "echo \"$HOME\"; {cd} || {{ echo \"no folder {d}\" >&2; exit 3; }}; pwd -P; {{ (. /etc/os-release 2>/dev/null && echo \"$PRETTY_NAME\") || sw_vers -productVersion 2>/dev/null || true; }} | head -1; uname -srm",
            d = dir.replace('"', "")
        );
        let out = self.sh(&line, b"").await?;
        let out = String::from_utf8_lossy(&out).into_owned();
        let mut l = out.lines();
        let home = l.next().unwrap_or("").to_owned();
        let cwd = l.next().filter(|c| !c.is_empty()).ok_or_else(|| format!("no folder {dir} on {}", self.target))?.to_owned();
        let system = l.collect::<Vec<_>>().join(" · ");
        Ok((home, cwd, system))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::{Bash, BashOutput, Edit, Glob, Grep, Read, SessionTools, Tool, ToolCtx, Write};

    #[test]
    fn destinations() {
        assert_eq!(destination("admin@web1:2222"), "ssh://admin@web1:2222");
        assert_eq!(destination("web1:22"), "ssh://web1:22");
        assert_eq!(destination("admin@web1"), "admin@web1");
        assert_eq!(destination("my-alias"), "my-alias");
        assert_eq!(destination("ssh://a@b:1"), "ssh://a@b:1");
        assert_eq!(destination("fe80::1"), "fe80::1", "a bare IPv6 address");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    /// A stand-in for ssh: drops the options and the target, then runs the
    /// command line with sh, as sshd would on the target.
    pub(crate) fn fake_ssh(dir: &Path) -> PathBuf {
        let script = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o|-p|-i|-l|-F) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
shift
[ -n "$C6_FAKE_HOME" ] && export HOME="$C6_FAKE_HOME" && cd "$HOME"
exec sh -c "$*"
"#;
        let path = dir.join("ssh");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn tools_work_on_an_ssh_target() {
        let bin = tempfile::tempdir().unwrap();
        // SAFETY: only this test sets it; others never use ssh.
        unsafe { std::env::set_var("C6_SSH_BIN", fake_ssh(bin.path())) };
        let remote = tempfile::tempdir().unwrap();
        let r = remote.path().canonicalize().unwrap();
        // SAFETY: as above.
        unsafe { std::env::set_var("C6_FAKE_HOME", &r) };
        let host = Arc::new(SshHost::new("me@box", &r.display().to_string()));
        let (home, cwd, system) = host.probe("~").await.unwrap();
        assert_eq!((home.as_str(), cwd.as_str()), (r.to_str().unwrap(), r.to_str().unwrap()), "{system}");
        assert!(host.probe("/no/such/dir").await.is_err());

        let session = Arc::new(SessionTools::with_ssh(r.clone(), Some(host.clone())));
        session.set_secrets(vec![("TOKEN".into(), "tok-123456".into())]);
        let ctx = ToolCtx { session: session.clone(), cancel: Default::default(), output: Arc::new(|_: &str| {}), call_id: "c".into() };

        // Commands: the directory carries over; secrets are there but hidden.
        let out = Bash.call(&ctx, json!({"command": "mkdir -p sub && cd sub && echo in $(basename $PWD) $TOKEN"})).await;
        assert_eq!(out.content, "in sub [secret:TOKEN]\n", "{out:?}");
        assert_eq!(session.cwd(), r.join("sub"));
        let out = Bash.call(&ctx, json!({"command": "pwd; exit 3"})).await;
        assert_eq!(out.content, format!("{}\n[exit code 3]", r.join("sub").display()));

        // Files, with the read-before-write rule.
        let w = Write.call(&ctx, json!({"path": "notes.txt", "content": "one\ntwo\n"})).await;
        assert!(w.content.starts_with("Created"), "{w:?}");
        assert_eq!(std::fs::read_to_string(r.join("sub/notes.txt")).unwrap(), "one\ntwo\n");
        std::fs::write(r.join("sub/other.txt"), "x").unwrap();
        assert!(Write.call(&ctx, json!({"path": "other.txt", "content": "y"})).await.is_error, "must read first");
        let rd = Read.call(&ctx, json!({"path": "~/sub/notes.txt"})).await;
        assert!(rd.content.contains("     2\ttwo"), "{rd:?}");
        let ed = Edit.call(&ctx, json!({"path": "notes.txt", "old_string": "two", "new_string": "2"})).await;
        assert!(!ed.is_error, "{ed:?}");
        assert_eq!(std::fs::read_to_string(r.join("sub/notes.txt")).unwrap(), "one\n2\n");

        // Search.
        let g = Grep.call(&ctx, json!({"pattern": "^on", "output_mode": "content"})).await;
        assert_eq!(g.content, "notes.txt:1:one", "{g:?}");
        let gl = Glob.call(&ctx, json!({"pattern": "*.txt"})).await;
        assert_eq!(gl.content, "notes.txt\nother.txt");

        // Background commands, stoppable.
        Bash.call(&ctx, json!({"command": "echo started; sleep 30", "run_in_background": true})).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let o = BashOutput::generic().call(&ctx, json!({"id": "bg1"})).await;
        assert!(o.content.starts_with("started") && o.content.contains("still running"), "{o:?}");
        assert_eq!(session.background.kill_by_user("bg1"), Some(true));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while session.background.list().iter().any(|t| t.running) {
            assert!(std::time::Instant::now() < deadline, "the remote task did not stop");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Random bytes from the OS.
fn getrandom(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(buf);
    }
}
