//! Protected Git configuration without discarding the user's explicit trust.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn configured() -> &'static [OsString] {
    static TRUST: OnceLock<Vec<OsString>> = OnceLock::new();
    TRUST.get_or_init(|| {
        let mut entries = Vec::new();
        for scope in ["--system", "--global"] {
            // No repository configuration, and no isolated environment: these
            // are the user's protected configuration trust decisions.
            if let Ok(out) = Command::new("git").args(["config", scope, "--null", "--get-all", "safe.directory"]).output()
                && out.status.success()
            {
                use std::os::unix::ffi::OsStringExt;
                // An empty entry resets earlier trust; retain it, discarding
                // only the final NUL terminator.
                let bytes = out.stdout.strip_suffix(&[0]).unwrap_or(&out.stdout);
                entries.extend(bytes.split(|b| *b == 0).map(|b| OsString::from_vec(b.to_vec())));
            }
        }
        entries
    })
}

fn paths(dir: &Path) -> Vec<PathBuf> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    let mut paths = vec![dir.clone()];
    if let Some(root) = dir.ancestors().find(|p| p.join(".git").exists()) {
        paths.push(root.to_owned());
        let dot = root.join(".git");
        let git = if dot.is_dir() {
            Some(dot)
        } else {
            std::fs::read_to_string(&dot).ok().and_then(|text| text.strip_prefix("gitdir: ").map(|p| root.join(p.trim())))
        };
        if let Some(git) = git {
            let git = git.canonicalize().unwrap_or(git);
            if let Ok(common) = std::fs::read_to_string(git.join("commondir")) {
                let common = git.join(common.trim());
                paths.push(common.canonicalize().unwrap_or(common));
            }
            paths.push(git);
        }
    }
    paths
}

fn trust(cmd: &mut Command, entries: &[OsString], dir: &Path) {
    for entry in entries.iter().cloned().chain(paths(dir).into_iter().map(PathBuf::into_os_string)) {
        let mut arg = OsString::from("safe.directory=");
        arg.push(entry);
        cmd.arg("-c").arg(arg);
    }
}

/// Only call for a folder chosen by the user or created/owned by carbon6.
/// SSH_AUTH_SOCK, HOME, keys and known_hosts remain available to SSH.
pub(crate) fn configure(cmd: &mut Command, dir: &Path) {
    configure_with(cmd, dir, configured());
}

fn configure_with(cmd: &mut Command, dir: &Path, entries: &[OsString]) {
    use std::os::unix::ffi::OsStrExt;
    for (key, _) in std::env::vars_os().filter(|(key, _)| {
        key.as_bytes().starts_with(b"GIT_")
            && !matches!(key.to_str(), Some("GIT_SSH" | "GIT_SSH_COMMAND" | "GIT_SSH_VARIANT" | "GIT_TEST_ASSUME_DIFFERENT_OWNER"))
    }) {
        cmd.env_remove(key);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.protectHFS=true",
            "-c",
            "core.protectNTFS=true",
        ]);
    trust(cmd, entries, dir);
}

pub(crate) fn created_dir(cmd: &mut Command, dir: &Path) {
    trust(cmd, &[], dir);
}

/// Keep the beginning of stderr while draining the pipe, even for noisy SSH.
pub(crate) async fn stderr(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut output = Vec::new();
    let mut buf = [0; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(output);
        }
        let keep = n.min((64 * 1024_usize).saturating_sub(output.len()));
        output.extend_from_slice(&buf[..keep]);
    }
}

pub(crate) fn diagnostic(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line =
        text.lines().map(str::trim).find(|s| !s.is_empty() && !s.starts_with("Cloning into ")).unwrap_or("Git returned no diagnostic");
    let line: String = line.chars().take(1024).collect();
    if text.contains("dubious ownership") {
        format!(
            "git: {}. The folder is owned by another user; add it to git's safe.directory, or choose it explicitly here so carbon6 trusts it",
            line.trim_start_matches("fatal: ")
        )
    } else {
        format!("git: {}", line.trim_start_matches("fatal: "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_trust_includes_user_entries_and_exact_folder() {
        let folder = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("git");
        let entries = [OsString::from("/srv/work/*"), OsString::from("/explicit/trust")];
        configure_with(&mut cmd, folder.path(), &entries);
        let args: Vec<_> = cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"safe.directory=/srv/work/*".to_owned()));
        assert!(args.contains(&"safe.directory=/explicit/trust".to_owned()));
        assert!(args.contains(&format!("safe.directory={}", folder.path().canonicalize().unwrap().display())));
        assert!(!args.contains(&"safe.directory=*".to_owned()));
        assert!(args.contains(&"core.hooksPath=/dev/null".to_owned()));
        assert!(args.contains(&"core.fsmonitor=false".to_owned()));
        let env: std::collections::BTreeMap<_, _> = cmd.get_envs().collect();
        assert_eq!(env[std::ffi::OsStr::new("GIT_CONFIG_GLOBAL")], Some(std::ffi::OsStr::new("/dev/null")));
        for name in ["SSH_AUTH_SOCK", "HOME", "GIT_SSH", "GIT_SSH_COMMAND", "GIT_SSH_VARIANT"] {
            assert!(!env.contains_key(std::ffi::OsStr::new(name)), "SSH environment must be inherited: {name}");
        }
    }

    #[test]
    fn linked_worktree_trust_includes_git_and_common_directories() {
        let folder = tempfile::tempdir().unwrap();
        let top = folder.path().join("checkout");
        let common = folder.path().join("common");
        let admin = common.join("worktrees/linked");
        std::fs::create_dir_all(&top).unwrap();
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(top.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let mut cmd = Command::new("git");
        trust(&mut cmd, &[], &top);
        let args: Vec<_> = cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect();
        for path in [top, admin, common] {
            assert!(args.contains(&format!("safe.directory={}", path.canonicalize().unwrap().display())));
        }
    }

    #[test]
    fn explicit_wildcard_and_reset_are_preserved_in_order() {
        let folder = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("git");
        trust(&mut cmd, &[OsString::from("*"), OsString::new(), OsString::from("/only-this")], folder.path());
        let values: Vec<_> = cmd.get_args().filter_map(|s| s.to_str()).filter(|s| s.starts_with("safe.directory=")).collect();
        assert_eq!(&values[..3], &["safe.directory=*", "safe.directory=", "safe.directory=/only-this"]);
    }

    #[tokio::test]
    async fn stderr_capture_is_bounded_but_drains_the_input() {
        let bytes = vec![b'e'; 128 * 1024];
        let mut input = bytes.as_slice();
        assert_eq!(stderr(&mut input).await.unwrap().len(), 64 * 1024);
        assert!(input.is_empty());
    }

    #[test]
    fn diagnostic_keeps_first_meaningful_line_and_explains_ownership() {
        assert_eq!(diagnostic(b"\n fatal: Permission denied (publickey).\nsecond line"), "git: Permission denied (publickey).");
        assert_eq!(diagnostic(b"Cloning into bare repository '.'...\ngit@host: Permission denied (publickey).\nfatal: Could not read from remote repository."), "git: git@host: Permission denied (publickey).");
        let error = diagnostic(b"fatal: detected dubious ownership in repository at '/repo'\n");
        assert!(error.contains("safe.directory"));
        assert!(!error.contains("Some("));
    }
}
