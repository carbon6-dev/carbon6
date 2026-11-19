//! Checkpoints of a session's working directory, for rewinding.
//!
//! Each session has a private git repository in the worker's state
//! directory whose work tree is the session's directory. A checkpoint is a
//! commit of everything there (minus ignored files) made with that
//! repository's own index, so the user's repository (if any) is never
//! touched: no index changes, no refs, no stashes. Works in folders that
//! are not git repositories too.

use std::fs::{File, TryLockError};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

/// Never snapshotted (on top of the work tree's own .gitignore files).
const EXCLUDES: &str =
    "node_modules/\ntarget/\n.venv/\nvenv/\n__pycache__/\n.mypy_cache/\n.pytest_cache/\ndist/\nbuild/\n.next/\n.DS_Store\n";
const TIMEOUT: Duration = Duration::from_secs(120);

pub struct Checkpoints {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl Checkpoints {
    pub fn new(state_dir: &Path, session_id: &str, work_tree: &Path) -> Self {
        Self { git_dir: state_dir.join("checkpoints").join(format!("{session_id}.git")), work_tree: work_tree.to_owned() }
    }

    fn open_lock(git_dir: &Path) -> anyhow::Result<File> {
        std::fs::create_dir_all(git_dir)?;
        // Never unlink this inode: every instance and surviving git child must
        // contend on the same lock, including across a worker restart.
        Ok(File::options().read(true).write(true).create(true).truncate(false).open(git_dir.join("carbon6.lock"))?)
    }

    async fn lock(&self) -> anyhow::Result<File> {
        let lock = Self::open_lock(&self.git_dir)?;
        tokio::time::timeout(TIMEOUT, async {
            loop {
                match lock.try_lock() {
                    Ok(()) => {
                        // A pre-sidecar worker's orphan checkout may not own
                        // index.lock. Wait for it too before touching the tree.
                        // An exec/exit can also make argv temporarily unreadable:
                        // uncertainty must delay us, not authorize any writes.
                        let mut noted_uncertainty = false;
                        loop {
                            match crate::checkpoint_process::running(&self.git_dir).await {
                                Ok(false) => return Ok(lock),
                                Ok(true) => {}
                                Err(error) => {
                                    if !noted_uncertainty {
                                        tracing::warn!(repo = %self.git_dir.display(), error = %format_args!("{error:#}"), "waiting for uncertain checkpoint Git ownership");
                                        noted_uncertainty = true;
                                    }
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    }
                    Err(TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(25)).await,
                    Err(TryLockError::Error(e)) => return Err(e.into()),
                }
            }
        })
        .await
        .context("waiting for checkpoint repository")?
    }

    /// Sweep before session recovery, after the worker's singleton lock is held.
    /// A surviving git owns the inherited sidecar lock, so startup skips it.
    pub async fn recover(state_dir: &Path) -> anyhow::Result<()> {
        let entries = match std::fs::read_dir(state_dir.join("checkpoints")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.path().extension().is_none_or(|ext| ext != "git") {
                continue;
            }
            let git_dir = entry.path();
            let lock = Self::open_lock(&git_dir)?;
            match lock.try_lock() {
                Ok(()) => {
                    if let Err(error) = Self::remove_stale_index_lock(&git_dir).await {
                        tracing::warn!(repo = %git_dir.display(), error = %format_args!("{error:#}"), "could not recover checkpoint index lock");
                    }
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn remove_stale_index_lock(git_dir: &Path) -> anyhow::Result<bool> {
        let path = git_dir.join("index.lock");
        // Keep the original inode open so it cannot be recycled during the
        // process scan and confused with a replacement lock at the same path.
        let original = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let before = original.metadata()?;
        // Older workers did not have the inherited sidecar. Inspect their
        // actual argv and repository identity, not rendered/quoted ps paths.
        // The worker singleton excludes a new legacy checkpoint producer;
        // an uncertain process view never authorizes removing its index lock.
        if crate::checkpoint_process::running(git_dir).await? {
            tracing::debug!(repo = %git_dir.display(), "deferring checkpoint lock recovery while git is running");
            return Ok(false);
        }
        let after = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Ok(false);
        }
        std::fs::remove_file(&path)?;
        tracing::warn!(repo = %git_dir.display(), lock = %path.display(), "removed stale checkpoint index lock");
        Ok(true)
    }

    fn command(&self, args: &[&str], lock: &File) -> tokio::process::Command {
        let mut cmd = c6_tools::process::session_command("git");
        cmd.arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(["-c", "user.name=carbon6", "-c", "user.email=carbon6@localhost", "-c", "core.autocrlf=false", "-c", "gc.auto=0"])
            .args(args)
            .env("LC_ALL", "C")
            .env_remove("GIT_INDEX_FILE")
            .current_dir(&self.work_tree)
            .kill_on_drop(true);
        let fd = lock.as_raw_fd();
        // SAFETY: fcntl is async-signal-safe. Only the child's descriptor loses
        // CLOEXEC; it retains the flock after cancellation or worker death.
        // Closing the parent's File must NOT explicitly unlock the shared OFD.
        unsafe {
            cmd.pre_exec(move || {
                let flags = nix::libc::fcntl(fd, nix::libc::F_GETFD);
                if flags == -1 || nix::libc::fcntl(fd, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd
    }

    async fn git(&self, args: &[&str], lock: &File) -> anyhow::Result<String> {
        for attempt in 0..2 {
            let out = tokio::time::timeout(TIMEOUT, self.command(args, lock).output())
                .await
                .context("git timed out")?
                .context("running git (is it installed?)")?;
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned());
            }
            let error = String::from_utf8_lossy(&out.stderr);
            let index_lock = self.git_dir.join("index.lock");
            let owns_error = [&index_lock, &self.git_dir.canonicalize()?.join("index.lock")]
                .iter()
                .any(|path| error.contains(&format!("Unable to create '{}': File exists", path.display())));
            if attempt == 0 && owns_error && Self::remove_stale_index_lock(&self.git_dir).await? {
                tracing::info!(repo = %self.git_dir.display(), command = ?args.first(), "retrying checkpoint git after stale lock recovery");
                continue;
            }
            anyhow::bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
        }
        unreachable!("at most one checkpoint retry")
    }

    async fn ensure(&self, lock: &File) -> anyhow::Result<()> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.git_dir)?;
        self.git(&["init", "--quiet"], lock).await?;
        std::fs::create_dir_all(self.git_dir.join("info"))?;
        std::fs::write(self.git_dir.join("info").join("exclude"), EXCLUDES)?;
        Ok(())
    }

    /// The current state of the work tree as a tree object.
    async fn tree(&self, lock: &File) -> anyhow::Result<String> {
        self.ensure(lock).await?;
        self.git(&["add", "--all", "."], lock).await?;
        self.git(&["write-tree"], lock).await
    }

    /// Snapshots the work tree; returns the commit.
    pub async fn snapshot(&self) -> anyhow::Result<String> {
        let lock = self.lock().await?;
        let tree = self.tree(&lock).await?;
        let commit = self.git(&["commit-tree", &tree, "-m", "carbon6 checkpoint"], &lock).await?;
        // A ref keeps it from being garbage-collected.
        self.git(&["update-ref", &format!("refs/checkpoints/{commit}"), &commit], &lock).await?;
        Ok(commit)
    }

    /// Puts the work tree back to `commit`: changed and deleted files come
    /// back, files created since are removed. Ignored files are left alone.
    pub async fn restore(&self, commit: &str) -> anyhow::Result<()> {
        let lock = self.lock().await?;
        let now = self.tree(&lock).await?;
        let added = self.git(&["diff", "--name-only", "--no-renames", "--diff-filter=A", commit, &now], &lock).await?;
        for rel in added.lines().filter(|l| !l.is_empty()) {
            let path = self.work_tree.join(rel);
            if path.starts_with(&self.work_tree) {
                let _ = std::fs::remove_file(&path);
            }
        }
        self.git(&["read-tree", commit], &lock).await?;
        self.git(&["checkout-index", "--all", "--force"], &lock).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_process::{OwnershipUncertain, conclusive};

    #[tokio::test]
    async fn startup_recovers_stale_index_locks() {
        if !c6_testkit::have("git") {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cp = Checkpoints::new(state.path(), "s1", work.path());
        cp.snapshot().await.unwrap();
        std::fs::write(cp.git_dir.join("index.lock"), "interrupted index").unwrap();
        // Other parallel tests may fork while our descriptor is open. Even a
        // CLOEXEC descriptor retains its flock until that unrelated child
        // reaches exec, so a sweep is allowed to skip the temporarily busy repo.
        c6_testkit::eventually("startup removes the stale index lock", async || {
            Checkpoints::recover(state.path()).await.unwrap();
            (!cp.git_dir.join("index.lock").exists()).then_some(())
        })
        .await;
        cp.snapshot().await.unwrap();
    }

    #[tokio::test]
    async fn a_surviving_git_keeps_the_repository_locked() {
        if !c6_testkit::have("git") {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cp = Checkpoints::new(state.path(), "s1", work.path());
        cp.snapshot().await.unwrap();
        let lock = cp.lock().await.unwrap();
        // stdin holds a real git at a known boundary, without sleeps. Closing
        // our File models cancellation/worker death while git is still alive.
        let mut child = cp
            .command(&["hash-object", "--stdin"], &lock)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        drop(lock);
        let contender = Checkpoints::open_lock(&cp.git_dir).unwrap();
        assert!(matches!(contender.try_lock(), Err(TryLockError::WouldBlock)));
        std::fs::write(cp.git_dir.join("index.lock"), "live index").unwrap();
        Checkpoints::recover(state.path()).await.unwrap();
        assert!(cp.git_dir.join("index.lock").exists(), "startup must not steal a live git's lock");
        drop(child.stdin.take());
        assert!(c6_testkit::within("git exits after stdin EOF", child.wait()).await.unwrap().success());
        c6_testkit::eventually("git releases the inherited sidecar", async || contender.try_lock().ok()).await;
        drop(contender);
        c6_testkit::eventually("startup recovers the exited git's index lock", async || {
            Checkpoints::recover(state.path()).await.unwrap();
            (!cp.git_dir.join("index.lock").exists()).then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn separate_checkpoint_instances_serialize_the_whole_operation() {
        if !c6_testkit::have("git") {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("file"), "same tree").unwrap();
        let a = Checkpoints::new(state.path(), "s1", work.path());
        let b = Checkpoints::new(state.path(), "s1", work.path());
        let (a, b) = c6_testkit::within("concurrent checkpoints", async { tokio::join!(a.snapshot(), b.snapshot()) }).await;
        assert!(a.is_ok(), "{a:?}");
        assert!(b.is_ok(), "{b:?}");
    }

    #[tokio::test]
    async fn a_legacy_git_without_the_sidecar_is_not_mistaken_for_a_stale_lock() {
        if !c6_testkit::have("git") {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cp = Checkpoints::new(state.path(), "s1", work.path());
        cp.snapshot().await.unwrap();
        // The previous worker's argv shape, deliberately without our new
        // pre-exec hook/sidecar. Spawn's exec handshake and open stdin keep
        // this real git alive until the ownership checks have finished.
        let mut child = tokio::process::Command::new("git")
            .arg("--git-dir")
            .arg(&cp.git_dir)
            .arg("--work-tree")
            .arg(work.path())
            .args(["hash-object", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        std::fs::write(cp.git_dir.join("index.lock"), "live legacy index").unwrap();
        Checkpoints::recover(state.path()).await.unwrap();
        let lock = Checkpoints::open_lock(&cp.git_dir).unwrap();
        c6_testkit::eventually("legacy repo's sidecar is not inherited by an unrelated fork", async || lock.try_lock().ok()).await;
        let recovered = conclusive(async || Checkpoints::remove_stale_index_lock(&cp.git_dir).await).await.unwrap();
        let retained = std::fs::read_to_string(cp.git_dir.join("index.lock"));
        drop(child.stdin.take());
        assert!(c6_testkit::within("legacy git exits after stdin EOF", child.wait()).await.unwrap().success());
        assert!(!recovered, "must not recover over a live legacy git");
        assert_eq!(retained.unwrap(), "live legacy index");
        drop(lock);
        conclusive(async || cp.snapshot().await).await.unwrap();
        assert!(!cp.git_dir.join("index.lock").exists());
    }

    #[tokio::test]
    async fn a_git_failure_is_retried_only_once() {
        if !c6_testkit::have("git") {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cp = Checkpoints::new(state.path(), "s1", work.path());
        cp.snapshot().await.unwrap();
        let lock = cp.lock().await.unwrap();
        let alias = format!(
            "alias.fail=!printf x >> attempts; touch '{}'; printf \"Unable to create '{}': File exists\\n\" >&2; exit 1",
            cp.git_dir.join("index.lock").display(),
            cp.git_dir.join("index.lock").display()
        );
        let attempts = work.path().join("attempts");
        let error = conclusive(async || {
            std::fs::write(&attempts, "")?;
            let error = cp.git(&["-c", &alias, "fail"], &lock).await.unwrap_err();
            if error.is::<OwnershipUncertain>() {
                // This invocation must fail closed: only its initial Git
                // command ran, and the scanner must have left the lock intact.
                assert_eq!(std::fs::read_to_string(&attempts)?, "x");
                assert!(cp.git_dir.join("index.lock").exists());
                Err(error)
            } else {
                Ok(error)
            }
        })
        .await
        .unwrap();
        assert!(error.to_string().contains("File exists"));
        assert_eq!(std::fs::read_to_string(attempts).unwrap(), "xx");
        assert!(cp.git_dir.join("index.lock").exists(), "a second failure must not remove another lock or retry again");
    }

    #[tokio::test]
    async fn another_index_lock_error_does_not_authorize_recovery() {
        if !c6_testkit::have("git") {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cp = Checkpoints::new(state.path(), "s1", work.path());
        cp.snapshot().await.unwrap();
        let lock = cp.lock().await.unwrap();
        std::fs::write(cp.git_dir.join("index.lock"), "leave alone").unwrap();
        let alias = "alias.fail=!printf x >> attempts; printf \"Unable to create '/other/index.lock': File exists\\n\" >&2; exit 1";
        assert!(cp.git(&["-c", alias, "fail"], &lock).await.is_err());
        assert_eq!(std::fs::read_to_string(work.path().join("attempts")).unwrap(), "x");
        assert_eq!(std::fs::read_to_string(cp.git_dir.join("index.lock")).unwrap(), "leave alone");
    }

    #[tokio::test]
    async fn a_stale_index_lock_does_not_disable_future_checkpoints() {
        if !c6_testkit::have("git") {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cp = Checkpoints::new(state.path(), "s1", work.path());
        std::fs::write(work.path().join("file"), "before").unwrap();
        cp.snapshot().await.unwrap();
        // A killed git add leaves this file behind. No git process owns it.
        std::fs::write(cp.git_dir.join("index.lock"), "interrupted index").unwrap();
        std::fs::write(work.path().join("file"), "after").unwrap();
        let commit = conclusive(async || cp.snapshot().await).await.expect("a stale index.lock must be recovered");
        assert!(!cp.git_dir.join("index.lock").exists());
        std::fs::write(work.path().join("file"), "later").unwrap();
        cp.restore(&commit).await.unwrap();
        assert_eq!(std::fs::read_to_string(work.path().join("file")).unwrap(), "after");
    }

    #[tokio::test]
    async fn snapshot_and_restore() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let w = work.path();
        std::fs::write(w.join("a.txt"), "one").unwrap();
        std::fs::create_dir(w.join("src")).unwrap();
        std::fs::write(w.join("src/b.rs"), "fn b() {}").unwrap();
        std::fs::create_dir(w.join("node_modules")).unwrap();
        std::fs::write(w.join("node_modules/x.js"), "x").unwrap();
        let cp = Checkpoints::new(state.path(), "s1", w);
        let c1 = cp.snapshot().await.unwrap();

        // The agent edits, deletes and creates files.
        std::fs::write(w.join("a.txt"), "two").unwrap();
        std::fs::remove_file(w.join("src/b.rs")).unwrap();
        std::fs::write(w.join("new.txt"), "new").unwrap();
        std::fs::write(w.join("node_modules/y.js"), "y").unwrap();
        cp.snapshot().await.unwrap();

        cp.restore(&c1).await.unwrap();
        assert_eq!(std::fs::read_to_string(w.join("a.txt")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(w.join("src/b.rs")).unwrap(), "fn b() {}");
        assert!(!w.join("new.txt").exists(), "files created since are removed");
        assert!(w.join("node_modules/y.js").exists(), "excluded directories are left alone");
        // The user's directory has no git repository of ours in it.
        assert!(!w.join(".git").exists());
    }
}
