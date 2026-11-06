//! Ask the user's shell, not a list of installation directories. Never source
//! its environment into the worker or expose banner/output text in diagnostics.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, ensure};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

fn shell_command(shell: Option<&OsStr>) -> tokio::process::Command {
    let shell = shell.filter(|s| !s.is_empty());
    let mut command = tokio::process::Command::new(shell.unwrap_or(OsStr::new("/bin/sh")));
    command.args([if shell.is_some() { "-lic" } else { "-lc" }, "command -v codex"]);
    command
}

pub(super) async fn from_shell(shell: Option<&OsStr>, timeout: Duration, stop: &CancellationToken) -> anyhow::Result<PathBuf> {
    let mut child = shell_command(shell)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("Could not ask the user's shell for Codex: set its path")?;
    let pid = child.id();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let exchange = async {
        let mut last_path = None;
        let mut total = 0;
        loop {
            let mut line = Vec::new();
            let n = (&mut stdout).take(super::MAX_LINE).read_until(b'\n', &mut line).await?;
            if n == 0 {
                break;
            }
            total += n;
            ensure!(n < super::MAX_LINE as usize && total <= super::MAX_LINE as usize * 16, "Codex shell output too large: set its path");
            if let Ok(line) = std::str::from_utf8(&line) {
                let path = Path::new(line.trim());
                if path.is_absolute() {
                    last_path = Some(path.to_owned());
                }
            }
        }
        ensure!(child.wait().await?.success(), "Codex not found: set its path");
        last_path.context("Codex not found: set its path")
    };
    let result = tokio::select! {
        _ = stop.cancelled() => Err(anyhow::anyhow!("Codex discovery cancelled")),
        r = tokio::time::timeout(timeout, exchange) => r.unwrap_or_else(|_| Err(anyhow::anyhow!("Codex shell discovery timed out: set its path"))),
    };
    if let Some(pid) = pid {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
    }
    let _ = child.kill().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn unset_shell_uses_sh_login_without_interactive_flag() {
        for shell in [None, Some(OsStr::new(""))] {
            let command = shell_command(shell);
            assert_eq!(command.as_std().get_program(), "/bin/sh");
            assert_eq!(command.as_std().get_args().collect::<Vec<_>>(), ["-lc", "command -v codex"]);
        }
    }

    async fn fake(dir: &Path, body: &str) -> PathBuf {
        let shell = dir.join("shell");
        std::fs::write(&shell, format!("#!/bin/sh\n[ \"$1\" = --version ] && exit 0\n{body}\n")).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        c6_testkit::runnable(&shell, &["--version"]).await;
        shell
    }

    #[tokio::test]
    async fn shell_discovery_ignores_noise_takes_last_absolute_line_and_closes_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake(
            dir.path(),
            r#"
[ "$1" = -lic ] && [ "$2" = 'command -v codex' ] || exit 2
if read -r unexpected; then exit 3; fi
printf '%s\n' 'Welcome!' '/old/codex' 'codex is an alias' '/chosen/bin/codex' 'logout'
printf '%s\n' '/stderr/not-codex' >&2
"#,
        )
        .await;
        let found = from_shell(Some(shell.as_os_str()), c6_testkit::wait::deadline(), &CancellationToken::new()).await.unwrap();
        assert_eq!(found, Path::new("/chosen/bin/codex"));
    }

    #[tokio::test]
    async fn shell_discovery_rejects_non_paths_and_does_not_report_shell_output() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake(dir.path(), "printf '%s\\n' 'secret-banner' 'codex' 'alias codex=secret'; exit 0").await;
        let error = from_shell(Some(shell.as_os_str()), c6_testkit::wait::deadline(), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error.to_string(), "Codex not found: set its path");
    }

    #[tokio::test]
    async fn cancelled_discovery_reaps_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake(dir.path(), &format!("printf '%s' $$ > '{}/pid'\nwhile :; do :; done", dir.path().display())).await;
        let stop = CancellationToken::new();
        let token = stop.clone();
        let run = tokio::spawn(async move { from_shell(Some(shell.as_os_str()), c6_testkit::wait::deadline(), &token).await });
        c6_testkit::eventually("the shell started", async || dir.path().join("pid").exists().then_some(())).await;
        stop.cancel();
        let error = c6_testkit::within("shell discovery cancelled", run).await.unwrap().unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        let pid: i32 = std::fs::read_to_string(dir.path().join("pid")).unwrap().parse().unwrap();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err());
    }

    #[tokio::test]
    async fn a_shell_that_never_closes_stdout_times_out_without_output_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake(dir.path(), "printf '%s\\n' secret-banner; while :; do :; done").await;
        // Always pending: this program cannot finish. A zero product timeout
        // tests the timeout path without imposing a speed limit on success.
        let error = from_shell(Some(shell.as_os_str()), Duration::ZERO, &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error.to_string(), "Codex shell discovery timed out: set its path");
    }
}
