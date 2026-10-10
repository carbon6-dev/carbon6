//! Checkpoints of a session's working directory, for rewinding.
//!
//! Each session has a private git repository in the worker's state
//! directory whose work tree is the session's directory. A checkpoint is a
//! commit of everything there (minus ignored files) made with that
//! repository's own index, so the user's repository (if any) is never
//! touched: no index changes, no refs, no stashes. Works in folders that
//! are not git repositories too.

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

    async fn git(&self, args: &[&str]) -> anyhow::Result<String> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(["-c", "user.name=carbon6", "-c", "user.email=carbon6@localhost", "-c", "core.autocrlf=false", "-c", "gc.auto=0"])
            .args(args)
            .current_dir(&self.work_tree)
            .kill_on_drop(true);
        let out = tokio::time::timeout(TIMEOUT, cmd.output()).await.context("git timed out")?.context("running git (is it installed?)")?;
        if !out.status.success() {
            anyhow::bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    async fn ensure(&self) -> anyhow::Result<()> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.git_dir)?;
        self.git(&["init", "--quiet"]).await?;
        std::fs::create_dir_all(self.git_dir.join("info"))?;
        std::fs::write(self.git_dir.join("info").join("exclude"), EXCLUDES)?;
        Ok(())
    }

    /// The current state of the work tree as a tree object.
    async fn tree(&self) -> anyhow::Result<String> {
        self.ensure().await?;
        self.git(&["add", "--all", "."]).await?;
        self.git(&["write-tree"]).await
    }

    /// Snapshots the work tree; returns the commit.
    pub async fn snapshot(&self) -> anyhow::Result<String> {
        let tree = self.tree().await?;
        let commit = self.git(&["commit-tree", &tree, "-m", "carbon6 checkpoint"]).await?;
        // A ref keeps it from being garbage-collected.
        self.git(&["update-ref", &format!("refs/checkpoints/{commit}"), &commit]).await?;
        Ok(commit)
    }

    /// Puts the work tree back to `commit`: changed and deleted files come
    /// back, files created since are removed. Ignored files are left alone.
    pub async fn restore(&self, commit: &str) -> anyhow::Result<()> {
        let now = self.tree().await?;
        let added = self.git(&["diff", "--name-only", "--no-renames", "--diff-filter=A", commit, &now]).await?;
        for rel in added.lines().filter(|l| !l.is_empty()) {
            let path = self.work_tree.join(rel);
            if path.starts_with(&self.work_tree) {
                let _ = std::fs::remove_file(&path);
            }
        }
        self.git(&["read-tree", commit]).await?;
        self.git(&["checkout-index", "--all", "--force"]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
