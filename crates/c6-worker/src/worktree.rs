//! A session's git worktree (docs/dev-starts.md): made as its template says
//! (branch, base, fetch, tracking, setup commands), and removed when its
//! work ends, only when nothing would be lost.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use c6_api::starts::{WorktreeEnd, WorktreeEnded};
use c6_proto::WorktreeSpec;
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::info;

/// How long one setup command may run.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The most of a setup command's output kept for the transcript.
const OUTPUT_CHARS: usize = 4000;
/// Refuse oversized snapshots instead of buffering arbitrarily large diffs.
const PATCH_BYTES: usize = 32 * 1024 * 1024;

/// A worktree made for a session.
#[derive(Debug)]
pub struct Made {
    /// The session's folder: the same place in the worktree as `cwd` in its
    /// repository.
    pub cwd: PathBuf,
    /// The worktree's own top folder.
    pub top: PathBuf,
    pub branch: String,
    pub root: String,
    /// The branch was made for it (not checked out again).
    pub new_branch: bool,
    /// Entry cleanup must not remove a different checkout set up at `top`.
    entry_admin: Option<std::fs::File>,
}

/// git in `dir`: its stdout, or its stderr as the error.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_bytes(dir, args, None, false).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

async fn bounded_output(reader: impl AsyncRead + Unpin, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut out).await?;
    if out.len() > limit {
        return Err(std::io::Error::other(format!("output exceeded {limit} bytes")));
    }
    Ok(out)
}

/// Keep patches and NUL-delimited path lists byte-exact. A no-index diff exits 1
/// when it finds changes. Feed patches through a private file, not the source tree.
async fn git_bytes(dir: &Path, args: &[&str], input: Option<&[u8]>, no_index_diff: bool) -> Result<Vec<u8>, String> {
    let stdin = match input {
        Some(bytes) => {
            let mut file = tempfile::tempfile().map_err(|e| format!("creating temporary patch: {e}"))?;
            file.write_all(bytes).and_then(|_| std::io::Seek::rewind(&mut file)).map_err(|e| format!("writing temporary patch: {e}"))?;
            Stdio::from(file)
        }
        None => Stdio::null(),
    };
    let mut command = c6_tools::process::session_command("git");
    crate::git_trust::configure(command.as_std_mut(), dir);
    let mut child = command
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("running git: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, stdout, stderr) = tokio::try_join!(child.wait(), bounded_output(stdout, PATCH_BYTES), crate::git_trust::stderr(stderr))
        .map_err(|e| format!("running git {}: {e}", args.join(" ")))?;
    if status.success() || (no_index_diff && status.code() == Some(1) && !stdout.is_empty()) {
        Ok(stdout)
    } else {
        Err(format!("{} ({status})", crate::git_trust::diagnostic(&stderr)))
    }
}

/// The last 8 hex digits of a session id.
pub fn short_id(session_id: &str) -> String {
    let mut v: Vec<char> = session_id.chars().filter(|c| c.is_ascii_hexdigit()).rev().take(8).collect();
    v.reverse();
    v.into_iter().collect()
}

/// Makes a worktree of the repository holding `cwd`, under `worktrees`.
/// A branch that exists already is checked out again; one pushed to a
/// remote (and not here) starts from there; else a new branch from the
/// base.
pub async fn create(worktrees: &Path, session_id: &str, cwd: &Path, spec: &WorktreeSpec) -> Result<Made, String> {
    create_at(worktrees, session_id, cwd, spec, None).await
}

fn repo_root(cwd: &Path) -> Result<&Path, String> {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .ok_or_else(|| format!("{} is not inside a git repository, so a worktree cannot be created", cwd.display()))
}

async fn resolve_commit(root: &Path, rev: &str) -> Result<String, String> {
    git(root, &["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{commit}}")]).await
}

/// Recognize a linked worktree selected directly in the folder picker.
pub(crate) async fn existing_place(cwd: &Path) -> Result<(String, String), String> {
    let Some(top) = cwd.ancestors().find(|p| p.join(".git").exists()) else {
        return Ok((String::new(), String::new()));
    };
    if !top.join(".git").is_file() {
        return Ok((String::new(), String::new()));
    }
    let common = git(top, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
    let common = Path::new(&common).canonicalize().map_err(|e| format!("Folder: cannot resolve the worktree repository: {e}"))?;
    let root = common.parent().ok_or_else(|| "Folder: worktree repository has no parent".to_owned())?;
    let branch =
        git(top, &["symbolic-ref", "--short", "HEAD"]).await.map_err(|_| "Worktree: choose a checkout with a branch".to_owned())?;
    validate_recorded_worktree(cwd, root, &branch).await?;
    Ok((root.display().to_string(), branch))
}

/// Entry shares the template creation path, but must validate the selected
/// base and branch *after* fetching and before creating anything.
async fn create_at(
    worktrees: &Path,
    session_id: &str,
    cwd: &Path,
    spec: &WorktreeSpec,
    required_head: Option<&str>,
) -> Result<Made, String> {
    let root = repo_root(cwd)?;
    let repo = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let short = short_id(session_id);
    let pattern = if spec.branch.trim().is_empty() { c6_api::starts::DEFAULT_BRANCH } else { spec.branch.trim() };
    let branch = pattern.replace("{{id}}", &short);
    git(root, &["check-ref-format", "--branch", &branch]).await.map_err(|_| format!("{branch:?} is not a valid git branch name"))?;
    let base = if spec.base.trim().is_empty() { "HEAD".to_owned() } else { spec.base.trim().to_owned() };
    if spec.fetch {
        // The base's remote (origin of origin/main), else every remote.
        let remotes = git(root, &["remote"]).await?;
        let remote = base.split_once('/').map(|(r, _)| r).filter(|r| remotes.lines().any(|l| l == *r));
        match remote {
            Some(r) => git(root, &["fetch", "--quiet", r]).await?,
            None => git(root, &["fetch", "--quiet", "--all"]).await?,
        };
    }
    let path = worktrees.join(&repo).join(&short);
    let path_s = path.display().to_string();
    let local_ref = format!("refs/heads/{branch}");
    let local = git(root, &["rev-parse", "--verify", "--quiet", &local_ref]).await.is_ok();
    let from = if local {
        local_ref
    } else {
        let pushed = git(root, &["for-each-ref", "--format=%(refname:short)", &format!("refs/remotes/*/{branch}")]).await?;
        pushed.lines().next().map(str::to_owned).unwrap_or_else(|| base.clone())
    };
    if let Some(head) = required_head {
        if resolve_commit(root, "HEAD").await? != head {
            return Err("source HEAD changed while preparing the worktree; retry entry".into());
        }
        if resolve_commit(root, &base).await? != head {
            return Err(format!("cannot carry changes: base {base} is not the source checkout's current HEAD"));
        }
        if resolve_commit(root, &from).await? != head {
            return Err(format!("cannot carry changes: branch {branch} ({from}) is not at the source checkout's current HEAD"));
        }
    }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("creating {}: {e}", worktrees.display()))?;
    let track = if spec.track { "--track" } else { "--no-track" };
    let mut made = Made {
        cwd: path.join(cwd.strip_prefix(root).unwrap_or(Path::new(""))),
        top: path,
        branch,
        root: root.display().to_string(),
        new_branch: !local,
        entry_admin: None,
    };
    // Don't run native checkout hooks during entry preparation. Explicit setup
    // runs below under our overlap checks. Template creation is unchanged.
    let mut add = if required_head.is_some() { vec!["-c", "core.hooksPath=/dev/null"] } else { vec![] };
    add.extend(["worktree", "add"]);
    if required_head.is_some() {
        match made.top.symlink_metadata() {
            Ok(_) => return Err(format!("cannot enter: worktree path {} already exists", made.top.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot check worktree path {}: {e}", made.top.display())),
        }
    }
    if required_head.is_some() && made.new_branch {
        // Claim the branch separately: checkout can fail after creating a
        // branch (for instance, a required smudge filter can fail). This way
        // we know whether a branch found after failure is ours to delete.
        git(root, &["branch", track, "--", &made.branch, &from]).await?;
    }
    if local || required_head.is_some() {
        add.extend(["--", &path_s, &made.branch]);
    } else {
        add.extend(["-b", &made.branch, track, "--", &path_s, &from]);
    }
    if let Err(e) = git(root, &add).await {
        if required_head.is_some() {
            return Err(cleanup_entry_creation(&made, e).await);
        }
        return Err(e);
    }
    if required_head.is_some() {
        // Keep the original directory open so its inode cannot be reused if
        // setup removes the worktree and creates another with the same name.
        let admin = git_path(&made.top, &["rev-parse", "--absolute-git-dir"])
            .await
            .and_then(|p| std::fs::File::open(&p).map_err(|e| format!("opening worktree metadata {}: {e}", p.display())));
        match admin {
            Ok(admin) => made.entry_admin = Some(admin),
            Err(e) => return Err(cleanup_entry_creation(&made, e).await),
        }
    }
    info!(session = %session_id, branch = %made.branch, path = %made.top.display(), "worktree created");
    Ok(made)
}

async fn cleanup_entry_creation(made: &Made, error: String) -> String {
    let root = Path::new(&made.root);
    // Git normally rolls back a failed add. If it registered our checkout
    // anyway, verify the path and branch before removing it.
    if validate_recorded_worktree(&made.top, root, &made.branch).await.is_ok() {
        return match discard_checked(made).await {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; worktree cleanup also failed: {cleanup}"),
        };
    }
    if made.new_branch
        && let Err(cleanup) = git(root, &["branch", "-D", "--", &made.branch]).await
    {
        return format!("{error}; branch cleanup also failed: {cleanup}");
    }
    error
}

/// Entry results. `changed_paths` is sorted, repository-relative, and counted
/// once per path even when both the staged and unstaged snapshots touch it.
#[derive(Debug)]
pub struct Entered {
    pub made: Made,
    pub changed_paths: Vec<String>,
    pub setup_log: String,
}

const DIFF_ARGS: &[&str] = &[
    "diff",
    "--binary",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--no-renames",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--submodule=short",
    "--ignore-submodules=none",
];

fn paths(bytes: &[u8]) -> impl Iterator<Item = Result<&str, String>> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).map_err(|_| "cannot carry or merge changes with a non-UTF-8 path".into()))
}

async fn patch_paths(root: &Path, patch: &[u8], changed: &mut BTreeSet<String>) -> Result<(), String> {
    if patch.is_empty() {
        return Ok(());
    }
    if patch.split(|b| *b == b'\n').any(|line| {
        (line.starts_with(b"index ")
            || line.starts_with(b"new file mode ")
            || line.starts_with(b"deleted file mode ")
            || line.starts_with(b"old mode ")
            || line.starts_with(b"new mode "))
            && line.ends_with(b" 160000")
    }) {
        return Err("cannot carry submodule changes as patches".into());
    }
    let stats = git_bytes(root, &["apply", "--numstat", "-z", "-"], Some(patch), false).await?;
    for stat in paths(&stats) {
        let stat = stat?;
        let (counts, path) = stat
            .split_once('\t')
            .and_then(|(added, rest)| rest.split_once('\t').map(|(removed, path)| ((added, removed), path)))
            .ok_or("git returned invalid patch statistics")?;
        if counts == ("-", "-") {
            return Err(format!("cannot carry binary changes: {path:?}"));
        }
        changed.insert(path.to_owned());
    }
    Ok(())
}

async fn status_bytes(root: &Path, ignored: bool) -> Result<Vec<u8>, String> {
    let mut args = vec!["status", "--porcelain=v1", "-z", "--no-renames", "--untracked-files=all", "--ignore-submodules=none"];
    if ignored {
        args.push("--ignored=matching");
    }
    git_bytes(root, &args, None, false).await
}

/// Creates a worktree at the source checkout's current HEAD, copying staged,
/// unstaged and non-ignored untracked changes without removing them from the
/// source. Checkout hooks are disabled; explicit setup runs *before* patch
/// application and must not touch the carried paths or change HEAD.
pub async fn enter(worktrees: &Path, session_id: &str, cwd: &Path, spec: &WorktreeSpec) -> Result<Entered, String> {
    let root = repo_root(cwd)?;
    let head = resolve_commit(root, "HEAD").await?;
    if !git_bytes(root, &["ls-files", "--unmerged", "-z"], None, false).await?.is_empty() {
        return Err("cannot carry changes from an index with unresolved merge conflicts".into());
    }
    let staged = git_bytes(root, &[DIFF_ARGS, &["--cached", &head, "--"]].concat(), None, false).await?;
    let unstaged = git_bytes(root, &[DIFF_ARGS, &["--"]].concat(), None, false).await?;
    let mut changed = BTreeSet::new();
    patch_paths(root, &staged, &mut changed).await?;
    patch_paths(root, &unstaged, &mut changed).await?;

    let others = git_bytes(root, &["ls-files", "--others", "--exclude-standard", "-z"], None, false).await?;
    let mut untracked = Vec::new();
    let mut untracked_paths = BTreeSet::new();
    for path in paths(&others) {
        let path = path?;
        let ty = std::fs::symlink_metadata(root.join(path)).map_err(|e| format!("reading untracked path {path:?}: {e}"))?.file_type();
        if !ty.is_file() && !ty.is_symlink() {
            return Err(format!("cannot carry untracked path {path:?}: not a regular file or symlink"));
        }
        let patch = git_bytes(root, &[DIFF_ARGS, &["--no-index", "--", "/dev/null", path]].concat(), None, true).await?;
        if staged.len() + unstaged.len() + untracked.len() + patch.len() > PATCH_BYTES {
            return Err(format!("cannot carry changes: patches exceed {PATCH_BYTES} bytes"));
        }
        untracked_paths.insert(path.to_owned());
        untracked.extend(patch);
    }
    if staged.len() + unstaged.len() > PATCH_BYTES {
        return Err(format!("cannot carry changes: patches exceed {PATCH_BYTES} bytes"));
    }
    let mut patched_untracked = BTreeSet::new();
    patch_paths(root, &untracked, &mut patched_untracked).await?;
    if patched_untracked != untracked_paths {
        return Err("cannot carry changes: untracked paths did not match their patches".into());
    }
    changed.extend(patched_untracked);

    let made = create_at(worktrees, session_id, cwd, spec, Some(&head)).await?;
    match prepare_entry(&made, spec, &head, &changed, &staged, &unstaged, &untracked).await {
        Ok(setup_log) => Ok(Entered { made, changed_paths: changed.into_iter().collect(), setup_log }),
        Err(e) => {
            let cleanup = discard_checked(&made).await;
            Err(match cleanup {
                Ok(()) => e,
                Err(cleanup) => format!("{e}; worktree cleanup also failed: {cleanup}"),
            })
        }
    }
}

async fn prepare_entry(
    made: &Made,
    spec: &WorktreeSpec,
    head: &str,
    changed: &BTreeSet<String>,
    staged: &[u8],
    unstaged: &[u8],
    untracked: &[u8],
) -> Result<String, String> {
    check_entry_head(made, head).await?;
    let log = setup(made, &spec.setup).await?;
    check_prepared_checkout(made).await?;
    check_entry_head(made, head).await?;
    if !changed.is_empty() {
        // Check prefixes in both directions: setup can replace a parent
        // directory with a file, or vice versa. Include ignored setup outputs.
        for entry in paths(&status_bytes(&made.top, true).await?) {
            let entry = entry?;
            let path = entry.get(3..).ok_or("git returned invalid worktree status")?;
            if changed.iter().any(|p| Path::new(p).starts_with(path) || Path::new(path).starts_with(p)) {
                return Err(format!("cannot carry changes: setup or checkout conflicts with carried path {path:?}"));
            }
        }
    }
    for (name, patch, indexed) in [("staged", staged, true), ("unstaged", unstaged, false), ("untracked", untracked, false)] {
        if !patch.is_empty() {
            let mut args = vec!["apply", "--whitespace=nowarn", "--no-ignore-whitespace"];
            if indexed {
                args.push("--index");
            }
            args.push("-");
            git_bytes(&made.top, &args, Some(patch), false).await.map_err(|e| format!("cannot carry {name} changes: {e}"))?;
        }
    }
    if resolve_commit(Path::new(&made.root), "HEAD").await? != head {
        return Err("source HEAD changed while preparing the worktree; retry entry".into());
    }
    if validate_recorded_worktree(&made.cwd, Path::new(&made.root), &made.branch).await?
        != made.top.canonicalize().map_err(|e| e.to_string())?
    {
        return Err("the new session folder is outside its prepared worktree".into());
    }
    Ok(log)
}

async fn check_entry_head(made: &Made, head: &str) -> Result<(), String> {
    let expected = format!("refs/heads/{}", made.branch);
    if resolve_commit(&made.top, "HEAD").await? != head
        || git(&made.top, &["symbolic-ref", "-q", "HEAD"]).await.ok().as_deref() != Some(expected.as_str())
    {
        return Err("cannot carry changes: worktree checkout or setup changed the required branch or HEAD".into());
    }
    Ok(())
}

/// Return the recorded main checkout, keeping the linked checkout and branch.
/// Optional merge is fast-forward only and requires both checkouts to be clean.
pub async fn leave(cwd: &Path, root: &Path, branch: &str, merge: bool) -> Result<PathBuf, String> {
    let top = validate_recorded_worktree(cwd, root, branch).await?;
    let resolved = cwd.canonicalize().map_err(|e| format!("resolving worktree folder: {e}"))?;
    let relative = resolved.strip_prefix(&top).map_err(|_| "worktree folder is outside its checkout")?;
    let destination = root.join(relative);
    let main = root.canonicalize().map_err(|e| format!("resolving source checkout: {e}"))?;
    let target = destination.canonicalize().map_err(|e| format!("source folder {} is unavailable: {e}", destination.display()))?;
    if !target.is_dir() || !target.starts_with(main) {
        return Err("source folder is not a directory inside its recorded checkout".into());
    }
    if merge {
        let expected = format!("refs/heads/{branch}");
        let main_branch =
            git(root, &["symbolic-ref", "HEAD"]).await.map_err(|_| "cannot merge into a detached main checkout".to_owned())?;
        if main_branch == expected {
            return Err("cannot merge: the main checkout is already using the worktree branch".into());
        }
        for (dir, kind) in [(root, "main"), (top.as_path(), "linked")] {
            if !status_bytes(dir, false).await?.is_empty() {
                return Err(format!("cannot merge: {kind} checkout is dirty; commit or move its changes first"));
            }
        }
        let target = resolve_commit(&top, "HEAD").await?;
        // Don't let Git overwrite ignored files, or let a post-merge hook
        // mutate the main checkout after the cleanliness checks.
        git(root, &["-c", "core.hooksPath=/dev/null", "merge", "--ff-only", "--no-overwrite-ignore", "--", &target])
            .await
            .map_err(|e| format!("cannot merge worktree branch {branch} (fast-forward only): {e}"))?;
    }
    Ok(destination)
}

/// Validate a persisted worktree placement before using it. A session `cwd`
/// may be a subdirectory; the result is its canonical checkout root. The
/// recorded source must be a separate registered checkout of the same repo.
pub async fn validate_recorded_worktree(cwd: &Path, root: &Path, branch: &str) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| format!("recorded source checkout {} is unavailable: {e}", root.display()))?;
    let cwd = cwd.canonicalize().map_err(|e| format!("recorded worktree folder {} is unavailable: {e}", cwd.display()))?;
    if git_path(&root, &["rev-parse", "--show-toplevel"]).await? != root {
        return Err("recorded source checkout is not a repository root".into());
    }
    let top = git_path(&cwd, &["rev-parse", "--show-toplevel"]).await?;
    let common = git_path(&root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
    if !cwd.starts_with(&top)
        || top == root
        || git_path(&top, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await? != common
    {
        return Err("recorded worktree is not a separate checkout of the recorded source repository".into());
    }
    if git_path(&top, &["rev-parse", "--absolute-git-dir"]).await? == common {
        return Err("recorded worktree is the primary checkout, not a linked worktree".into());
    }
    let expected = format!("refs/heads/{branch}");
    if branch.is_empty() || git(&top, &["symbolic-ref", "-q", "HEAD"]).await.ok().as_deref() != Some(expected.as_str()) {
        return Err(format!("recorded worktree is no longer on branch {branch:?}"));
    }
    // A copied or redirected .git file can report the right branch and
    // common directory at the wrong path. Verify the repo's registration too.
    let registered = registered_worktrees(&root).await?;
    if !registered.contains_key(&root) || registered.get(&top).and_then(Option::as_deref) != Some(expected.as_str()) {
        return Err("recorded checkout path and branch do not match Git's worktree registration".into());
    }
    Ok(top)
}

async fn git_path(dir: &Path, args: &[&str]) -> Result<PathBuf, String> {
    let output = git_bytes(dir, args, None, false).await?;
    let path = std::str::from_utf8(output.strip_suffix(b"\n").unwrap_or(&output))
        .map_err(|_| format!("Git returned a non-UTF-8 path for {}", dir.display()))?;
    Path::new(path).canonicalize().map_err(|e| format!("resolving Git path {path:?}: {e}"))
}

async fn registered_worktrees(root: &Path) -> Result<BTreeMap<PathBuf, Option<String>>, String> {
    let listed = git_bytes(root, &["worktree", "list", "--porcelain", "-z"], None, false).await?;
    let mut registered = BTreeMap::new();
    let mut path = None;
    let mut branch = None;
    for line in listed.split(|b| *b == 0) {
        if let Some(dir) = line.strip_prefix(b"worktree ") {
            path = std::str::from_utf8(dir).ok().and_then(|dir| Path::new(dir).canonicalize().ok());
        } else if let Some(name) = line.strip_prefix(b"branch ") {
            branch = std::str::from_utf8(name).ok().map(str::to_owned);
        } else if line.is_empty() {
            if let Some(path) = path.take() {
                registered.insert(path, branch.take());
            }
            branch = None;
        }
    }
    Ok(registered)
}

/// Runs the setup commands in the new worktree, one after another. Returns
/// what they printed; the first that fails is the error, with its output.
pub async fn setup(made: &Made, commands: &[String]) -> Result<String, String> {
    let mut log = String::new();
    for line in commands.iter().map(|c| c.trim()).filter(|c| !c.is_empty()) {
        let run = c6_tools::process::session_command("sh")
            .arg("-c")
            .arg(line)
            .current_dir(&made.top)
            .env("C6_WORKTREE", &made.top)
            .env("C6_BRANCH", &made.branch)
            .env("C6_REPO", &made.root)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(SETUP_TIMEOUT, run).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("setup `{line}` could not run: {e}")),
            Err(_) => return Err(format!("setup `{line}` took longer than {} minutes", SETUP_TIMEOUT.as_secs() / 60)),
        };
        let mut said = String::from_utf8_lossy(&out.stdout).into_owned();
        said.push_str(&String::from_utf8_lossy(&out.stderr));
        let said = tail(said.trim());
        if !out.status.success() {
            let code = out.status.code().map_or("a signal".to_owned(), |c| format!("exit {c}"));
            return Err(format!("setup `{line}` failed ({code}){}", if said.is_empty() { String::new() } else { format!(":\n{said}") }));
        }
        log.push_str(&format!("$ {line}\n"));
        if !said.is_empty() {
            log.push_str(&said);
            log.push('\n');
        }
    }
    Ok(log)
}

/// Takes away a worktree whose setup failed, and the branch if it was new.
pub async fn discard(made: &Made) {
    let _ = discard_checked(made).await;
}

async fn discard_checked(made: &Made) -> Result<(), String> {
    let root = Path::new(&made.root);
    check_prepared_checkout(made).await?;
    git(root, &["worktree", "remove", "--force", &made.top.display().to_string()]).await?;
    if made.new_branch {
        git(root, &["branch", "-D", "--", &made.branch]).await?;
    }
    Ok(())
}

async fn check_prepared_checkout(made: &Made) -> Result<(), String> {
    if let Some(admin) = &made.entry_admin {
        let top = made.top.canonicalize().map_err(|e| format!("finding prepared worktree {}: {e}", made.top.display()))?;
        let current = git_path(&top, &["rev-parse", "--absolute-git-dir"]).await?;
        let metadata = |e| format!("reading worktree metadata for {}: {e}", top.display());
        let before = admin.metadata().map_err(metadata)?;
        let now = current.metadata().map_err(metadata)?;
        if (before.dev(), before.ino()) != (now.dev(), now.ino()) || !registered_worktrees(Path::new(&made.root)).await?.contains_key(&top)
        {
            return Err(format!("{} is no longer the prepared worktree", made.top.display()));
        }
    }
    Ok(())
}

fn tail(s: &str) -> String {
    let n = s.chars().count();
    if n <= OUTPUT_CHARS { s.to_owned() } else { format!("…{}", s.chars().skip(n - OUTPUT_CHARS).collect::<String>()) }
}

/// What would be lost by removing the worktree at `top` and its branch:
/// uncommitted changes, and commits on no remote and in no other branch.
pub async fn at_risk(top: &Path, branch: &str) -> Result<Vec<String>, String> {
    let mut kept = Vec::new();
    let status = git(top, &["status", "--porcelain"]).await?;
    let changes = status.lines().filter(|l| !l.trim().is_empty()).count();
    if changes > 0 {
        kept.push(format!("{changes} uncommitted change{} in {}", if changes == 1 { "" } else { "s" }, top.display()));
    }
    let exclude = format!("--exclude={branch}");
    let only = git(top, &["rev-list", "--count", branch, "--not", &exclude, "--branches", "--remotes"]).await?;
    let only: usize = only.parse().unwrap_or(0);
    if only > 0 {
        kept.push(format!(
            "{only} commit{} on {branch} {} on no remote and in no other branch",
            if only == 1 { "" } else { "s" },
            if only == 1 { "is" } else { "are" }
        ));
    }
    Ok(kept)
}

/// The work ended (`worktree.end`): removes the worktree and its branch
/// when asked and nothing would be lost, then runs the WorkEnded hooks.
pub async fn end(req: &WorktreeEnd, labels: &[String]) -> WorktreeEnded {
    let mut out = WorktreeEnded::default();
    let path = PathBuf::from(&req.path);
    let top = match git(&path, &["rev-parse", "--show-toplevel"]).await {
        Ok(t) => Some(PathBuf::from(t)),
        Err(_) => None,
    };
    let root = if !req.repo_root.is_empty() { PathBuf::from(&req.repo_root) } else { top.clone().unwrap_or_else(|| path.clone()) };
    match &top {
        None => out.kept.push(format!("{} is gone already", req.path)),
        Some(top) if req.remove => match at_risk(top, &req.branch).await {
            Ok(risks) if risks.is_empty() => {
                // Clean (submodules included), so --force only gets past a
                // worktree with submodules, which git won't remove otherwise.
                match git(&root, &["worktree", "remove", "--force", &top.display().to_string()]).await {
                    Ok(_) => {
                        out.removed = true;
                        if let Err(e) = git(&root, &["branch", "-D", &req.branch]).await {
                            out.kept.push(e);
                        }
                        info!(worktree = %top.display(), branch = %req.branch, "worktree removed");
                    }
                    Err(e) => out.kept.push(e),
                }
            }
            Ok(risks) => out.kept = risks,
            Err(e) => out.kept.push(e),
        },
        Some(_) => {}
    }
    let worktree = top.as_ref().unwrap_or(&path).display().to_string();
    let cwd = if root.is_dir() { root.clone() } else { std::env::temp_dir() };
    if let Some(hooks) = crate::hooks::HookRunner::new("", &cwd, &req.hooks_json, labels) {
        let removed = if out.removed { "1" } else { "0" };
        let repo = root.display().to_string();
        let hooks = hooks.with_env(&[("C6_WORKTREE", &worktree), ("C6_BRANCH", &req.branch), ("C6_REPO", &repo), ("C6_REMOVED", removed)]);
        let payload =
            serde_json::json!({"worktree": worktree, "branch": req.branch, "repo": repo, "removed": out.removed, "kept": out.kept});
        out.hooks = hooks.run_reporting("WorkEnded", payload).await;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn sh_out(dir: &Path, script: &str) -> String {
        let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    const GIT: &str = "git -c user.name=t -c user.email=t@t -c protocol.file.allow=always";

    /// An "upstream" repository with a submodule, and a clone of it whose
    /// main line upstream has since moved on.
    fn repos(tmp: &Path) -> PathBuf {
        let lib = tmp.join("lib");
        let up = tmp.join("upstream");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&up).unwrap();
        sh(&lib, &format!("git init -q -b main && echo lib > lib.txt && git add . && {GIT} commit -qm lib"));
        sh(&up, &format!("git init -q -b master && echo one > a.txt && git add . && {GIT} commit -qm one"));
        sh(&up, &format!("{GIT} submodule -q add {} lib && {GIT} commit -qm sub", lib.display()));
        sh(tmp, &format!("{GIT} clone -q {} clone", up.display()));
        sh(&up, &format!("echo two > b.txt && git add . && {GIT} commit -qm two"));
        tmp.join("clone")
    }

    #[tokio::test]
    async fn a_template_s_worktree_from_the_latest_base_with_its_submodules() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let spec = WorktreeSpec {
            branch: "me/kafka_ingest".into(),
            base: "origin/master".into(),
            fetch: true,
            track: false,
            setup: vec![format!("{GIT} submodule update --init --recursive"), "echo ready".into()],
        };
        let wt = tmp.path().join("wt");
        let made = create(&wt, "0190-aaaa-bbbb-1234abcd", &clone, &spec).await.unwrap();
        assert_eq!(made.branch, "me/kafka_ingest");
        assert_eq!(made.top, wt.join("clone").join("1234abcd"));
        // Fetched first: the upstream's second commit is there.
        assert!(made.top.join("b.txt").exists());
        // Not tracking origin/master.
        assert_eq!(sh_out(&made.top, "git config branch.me/kafka_ingest.merge || echo none"), "none");
        let log = setup(&made, &spec.setup).await.unwrap();
        assert!(log.contains("$ echo ready\nready"), "{log}");
        assert!(made.top.join("lib/lib.txt").exists(), "the submodule is checked out");
    }

    #[tokio::test]
    async fn defaults_branch_from_head_and_tracking_is_a_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let made = create(&wt, "s-00000001", &clone.join("lib"), &WorktreeSpec::default()).await.unwrap();
        assert_eq!(made.branch, "c6/00000001");
        // The same place in the worktree as the folder was in the repository.
        assert_eq!(made.cwd, made.top.join("lib"));
        assert!(!made.top.join("b.txt").exists(), "HEAD, not fetched");
        let spec = WorktreeSpec { branch: "t/{{id}}".into(), base: "origin/master".into(), track: true, ..Default::default() };
        let made = create(&wt, "s-00000002", &clone, &spec).await.unwrap();
        assert_eq!(made.branch, "t/00000002");
        assert_eq!(sh_out(&made.top, "git config branch.t/00000002.merge"), "refs/heads/master");
        let bad = WorktreeSpec { branch: "a..b".into(), ..Default::default() };
        assert!(create(&wt, "s-00000003", &clone, &bad).await.unwrap_err().contains("not a valid git branch name"));
    }

    #[tokio::test]
    async fn a_failing_setup_command_stops_the_start_with_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let made = create(&tmp.path().join("wt"), "s-00000004", &clone, &WorktreeSpec::default()).await.unwrap();
        let err = setup(&made, &["echo first".into(), "echo broken >&2; exit 3".into(), "touch never".into()]).await.unwrap_err();
        assert!(err.contains("`echo broken >&2; exit 3` failed (exit 3):\nbroken"), "{err}");
        assert!(!made.top.join("never").exists());
        discard(&made).await;
        assert!(!made.top.exists());
        assert_eq!(sh_out(&clone, "git branch --list c6/00000004"), "");
    }

    #[tokio::test]
    async fn a_pushed_branch_starts_a_fresh_worktree_from_its_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        sh(
            &tmp.path().join("upstream"),
            &format!("git checkout -qb me/elsewhere && echo x > x.txt && git add . && {GIT} commit -qm x && git checkout -q master"),
        );
        let spec = WorktreeSpec { branch: "me/elsewhere".into(), base: "origin/master".into(), fetch: true, ..Default::default() };
        let made = create(&tmp.path().join("wt"), "s-00000005", &clone, &spec).await.unwrap();
        assert!(made.top.join("x.txt").exists(), "from the pushed branch, not the base");
    }

    #[tokio::test]
    async fn entry_carries_staged_unstaged_and_untracked_contents_without_touching_source() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        sh(
            &clone,
            "echo staged > a.txt; git add a.txt; echo working > a.txt; mkdir 'new dir'; printf 'untracked\n' > 'new dir/spaced file'; touch empty",
        );
        let head = sh_out(&clone, "git rev-parse HEAD");
        let status = sh_out(&clone, "git status --porcelain --untracked-files=all");
        let spec = WorktreeSpec { setup: vec!["echo ready; touch setup-output".into()], ..Default::default() };
        let entered = enter(&wt, "s-00000010", &clone, &spec).await.unwrap();
        assert_eq!(entered.changed_paths, ["a.txt", "empty", "new dir/spaced file"]);
        assert!(entered.setup_log.contains("\nready\n"));
        assert_eq!(sh_out(&entered.made.top, "git rev-parse HEAD"), head);
        assert_eq!(sh_out(&entered.made.top, "git show :a.txt"), "staged");
        assert_eq!(std::fs::read(entered.made.top.join("a.txt")).unwrap(), b"working\n");
        assert_eq!(std::fs::read(entered.made.top.join("new dir/spaced file")).unwrap(), b"untracked\n");
        assert!(entered.made.top.join("empty").is_file() && entered.made.top.join("setup-output").is_file());
        assert_eq!(sh_out(&clone, "git status --porcelain --untracked-files=all"), status);
        assert_eq!(sh_out(&clone, "git show :a.txt"), "staged");
        assert_eq!(std::fs::read(clone.join("a.txt")).unwrap(), b"working\n");
        assert_eq!(std::fs::read(clone.join("new dir/spaced file")).unwrap(), b"untracked\n");
    }

    #[tokio::test]
    async fn entry_rejects_binary_or_unmerged_changes_before_creating_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let spec = WorktreeSpec::default();
        std::fs::write(clone.join("binary"), b"abc\0xyz").unwrap();
        let err = enter(&wt, "s-00000011", &clone, &spec).await.unwrap_err();
        assert!(err.contains("binary changes"), "{err}");
        sh(&clone, "git add binary");
        let err = enter(&wt, "s-00000011", &clone, &spec).await.unwrap_err();
        assert!(err.contains("binary changes"), "{err}");
        sh(&clone, "git rm -f binary");
        sh(
            &clone,
            &format!(
                "git checkout -qb other; echo other > a.txt; {GIT} commit -qam other; git checkout -q master; echo main > a.txt; {GIT} commit -qam main"
            ),
        );
        sh(&clone, &format!("{GIT} merge other; test $? -eq 1"));
        let err = enter(&wt, "s-00000011", &clone, &spec).await.unwrap_err();
        assert!(err.contains("unresolved merge conflicts"), "{err}");
        assert!(!wt.exists());
        assert_eq!(sh_out(&clone, "git branch --list c6/00000011"), "");
    }

    #[tokio::test]
    async fn entry_refuses_setup_overlap_and_cleans_up_only_its_own_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        sh(&clone, "echo mine > a.txt; echo mine > untracked");
        let spec = WorktreeSpec { setup: vec!["echo other > untracked".into()], ..Default::default() };
        let err = enter(&wt, "s-00000012", &clone, &spec).await.unwrap_err();
        assert!(err.contains("conflicts with carried path"), "{err}");
        assert!(!wt.join("clone/00000012").exists());
        assert_eq!(sh_out(&clone, "git branch --list c6/00000012"), "");
        assert_eq!(std::fs::read(clone.join("untracked")).unwrap(), b"mine\n");

        sh(&clone, "git branch existing");
        let spec = WorktreeSpec { branch: "existing".into(), ..spec };
        assert!(enter(&wt, "s-00000012", &clone, &spec).await.is_err());
        assert!(!wt.join("clone/00000012").exists());
        assert_eq!(sh_out(&clone, "git branch --list existing"), "existing");
    }

    #[tokio::test]
    async fn entry_refuses_non_head_base_and_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let spec = WorktreeSpec { base: "origin/master".into(), fetch: true, ..Default::default() };
        let err = enter(&wt, "s-00000013", &clone, &spec).await.unwrap_err();
        assert!(err.contains("base origin/master is not"), "{err}");
        sh(&tmp.path().join("upstream"), "git branch remote-only");
        let spec = WorktreeSpec { branch: "remote-only".into(), fetch: true, ..Default::default() };
        let err = enter(&wt, "s-00000013", &clone, &spec).await.unwrap_err();
        assert!(err.contains("branch remote-only (origin/remote-only) is not"), "{err}");
        sh(&clone, "git branch local-only origin/master");
        let spec = WorktreeSpec { branch: "local-only".into(), ..Default::default() };
        let err = enter(&wt, "s-00000013", &clone, &spec).await.unwrap_err();
        assert!(err.contains("branch local-only (refs/heads/local-only) is not"), "{err}");
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn leave_keeps_the_worktree_and_only_merges_clean_fast_forwards() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let made = enter(&wt, "s-00000014", &clone, &WorktreeSpec::default()).await.unwrap().made;
        let head = sh_out(&clone, "git rev-parse HEAD");
        sh(&made.top, "echo work > work.txt");
        assert_eq!(leave(&made.cwd, &clone, &made.branch, false).await.unwrap(), clone);
        let err = leave(&made.cwd, &clone, &made.branch, true).await.unwrap_err();
        assert!(err.contains("linked checkout is dirty"), "{err}");
        sh(&made.top, &format!("git add work.txt; {GIT} commit -qm work"));
        sh(&clone, "echo main > main.txt");
        let err = leave(&made.cwd, &clone, &made.branch, true).await.unwrap_err();
        assert!(err.contains("main checkout is dirty"), "{err}");
        sh(&clone, "rm main.txt");
        assert_eq!(sh_out(&clone, "git rev-parse HEAD"), head);
        assert_eq!(leave(&made.cwd, &clone, &made.branch, true).await.unwrap(), clone);
        assert_eq!(std::fs::read(clone.join("work.txt")).unwrap(), b"work\n");
        assert!(made.top.is_dir());
        assert_eq!(sh_out(&clone, "git branch --list c6/00000014"), "+ c6/00000014");

        sh(&made.top, &format!("echo linked >> work.txt; {GIT} commit -qam linked"));
        sh(&clone, &format!("echo main >> work.txt; {GIT} commit -qam main"));
        let before = sh_out(&clone, "git rev-parse HEAD");
        let err = leave(&made.cwd, &clone, &made.branch, true).await.unwrap_err();
        assert!(err.contains("fast-forward only"), "{err}");
        assert_eq!(sh_out(&clone, "git rev-parse HEAD"), before);
        assert_eq!(sh_out(&clone, "git status --porcelain"), "");
    }

    #[tokio::test]
    async fn recorded_worktree_validation_rejects_wrong_branch_repo_or_checkout_path() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let made = enter(&wt, "s-00000015", &clone, &WorktreeSpec::default()).await.unwrap().made;
        let nested = made.top.join("subdir");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(validate_recorded_worktree(&nested, &clone, &made.branch).await.unwrap(), made.top.canonicalize().unwrap());

        std::fs::create_dir(clone.join("subdir")).unwrap();
        assert_eq!(leave(&nested, &clone, &made.branch, false).await.unwrap(), clone.join("subdir"));

        let err = leave(&nested, &clone, "wrong", false).await.unwrap_err();
        assert!(err.contains("no longer on branch"), "{err}");
        let err = leave(&nested, &tmp.path().join("upstream"), &made.branch, false).await.unwrap_err();
        assert!(err.contains("separate checkout"), "{err}");
        let err = leave(&nested, &clone.join("lib"), &made.branch, false).await.unwrap_err();
        assert!(err.contains("not a repository root"), "{err}");
        let err = leave(&clone, &clone, "master", false).await.unwrap_err();
        assert!(err.contains("separate checkout"), "{err}");

        let fake = tmp.path().join("fake-worktree");
        std::fs::create_dir(&fake).unwrap();
        std::fs::copy(made.top.join(".git"), fake.join(".git")).unwrap();
        let err = leave(&fake, &clone, &made.branch, false).await.unwrap_err();
        assert!(err.contains("registration"), "{err}");

        sh(&made.top, "git checkout -q --detach");
        let err = leave(&made.cwd, &clone, &made.branch, false).await.unwrap_err();
        assert!(err.contains("no longer on branch"), "{err}");
        assert!(made.top.exists());
    }

    #[tokio::test]
    async fn existing_place_leaves_scratch_and_primary_checkouts_unmarked() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(existing_place(tmp.path()).await.unwrap(), (String::new(), String::new()));
        sh(tmp.path(), &format!("git init -q -b main && echo base > tracked && git add . && {GIT} commit -qm base"));
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(existing_place(tmp.path()).await.unwrap(), (String::new(), String::new()));
        assert_eq!(existing_place(&nested).await.unwrap(), (String::new(), String::new()));
    }

    #[tokio::test]
    async fn existing_place_recognizes_nested_linked_checkout_without_changing_dirty_files() {
        let tmp = tempfile::tempdir().unwrap();
        sh(tmp.path(), &format!("git init -q -b main && echo base > tracked && git add . && {GIT} commit -qm base"));
        sh(tmp.path(), "git worktree add -q -b linked linked-checkout");
        let linked = tmp.path().join("linked-checkout");
        let nested = linked.join("nested");
        std::fs::create_dir(&nested).unwrap();
        sh(&linked, "echo staged > tracked && git add tracked && echo unstaged > tracked && echo local > nested/untracked");
        let status = sh_out(&linked, "git status --porcelain");
        let index = sh_out(&linked, "git show :tracked");
        let expected = (tmp.path().canonicalize().unwrap().display().to_string(), "linked".to_owned());
        assert_eq!(existing_place(&linked).await.unwrap(), expected);
        assert_eq!(existing_place(&nested).await.unwrap(), expected);
        assert_eq!(sh_out(&linked, "git status --porcelain"), status);
        assert_eq!(sh_out(&linked, "git show :tracked"), index);
        assert_eq!(std::fs::read(linked.join("tracked")).unwrap(), b"unstaged\n");
        assert_eq!(std::fs::read(nested.join("untracked")).unwrap(), b"local\n");
    }

    #[tokio::test]
    async fn existing_place_rejects_unregistered_and_detached_linked_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        sh(tmp.path(), &format!("git init -q -b main && echo base > tracked && git add . && {GIT} commit -qm base"));
        sh(tmp.path(), "git worktree add -q -b linked linked-checkout");
        let linked = tmp.path().join("linked-checkout");
        let fake = tmp.path().join("fake-checkout");
        std::fs::create_dir(&fake).unwrap();
        std::fs::copy(linked.join(".git"), fake.join(".git")).unwrap();
        let err = existing_place(&fake).await.unwrap_err();
        assert!(err.contains("registration"), "{err}");
        sh(&linked, "git checkout -q --detach");
        let err = existing_place(&linked).await.unwrap_err();
        assert!(err.contains("choose a checkout with a branch"), "{err}");
        assert!(linked.join("tracked").exists());
    }

    #[tokio::test]
    async fn entry_cleanup_removes_its_detached_checkout_but_not_a_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let spec = WorktreeSpec { setup: vec!["git checkout -q --detach".into()], ..Default::default() };
        let err = enter(&wt, "s-00000016", &clone, &spec).await.unwrap_err();
        assert!(err.contains("setup changed the required branch"), "{err}");
        assert!(!wt.join("clone/00000016").exists());
        assert_eq!(sh_out(&clone, "git branch --list c6/00000016"), "");

        let made = enter(&wt, "s-00000016", &clone, &WorktreeSpec::default()).await.unwrap().made;
        let top = made.top.display();
        sh(&clone, &format!("git worktree remove '{top}' && git worktree add -q '{top}' c6/00000016"));
        let err = discard_checked(&made).await.unwrap_err();
        assert!(err.contains("no longer the prepared worktree"), "{err}");
        assert!(made.top.exists());
        assert!(validate_recorded_worktree(&made.cwd, &clone, &made.branch).await.is_ok());
    }

    #[tokio::test]
    async fn the_end_removes_only_what_is_safe_and_runs_the_hook_with_its_place() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let end_of = |made: &Made, remove: bool, hooks: &str| WorktreeEnd {
            path: made.cwd.display().to_string(),
            branch: made.branch.clone(),
            repo_root: made.root.clone(),
            remove,
            hooks_json: hooks.to_owned(),
        };
        let branch_exists = |b: &str| !sh_out(&clone, &format!("git branch --list {b}")).is_empty();

        // Uncommitted changes: kept.
        let dirty = create(&wt, "s-0000000a", &clone, &WorktreeSpec { branch: "w/dirty".into(), ..Default::default() }).await.unwrap();
        sh(&dirty.top, "echo change >> a.txt");
        let r = end(&end_of(&dirty, true, ""), &[]).await;
        assert!(!r.removed && r.kept[0].contains("1 uncommitted change"), "{r:?}");
        assert!(dirty.top.exists() && branch_exists("w/dirty"));

        // A commit on no remote: kept.
        let ahead = create(&wt, "s-0000000b", &clone, &WorktreeSpec { branch: "w/ahead".into(), ..Default::default() }).await.unwrap();
        sh(&ahead.top, &format!("echo c > c.txt && git add . && {GIT} commit -qm c"));
        let r = end(&end_of(&ahead, true, ""), &[]).await;
        assert!(!r.removed && r.kept[0].contains("1 commit on w/ahead is on no remote"), "{r:?}");

        // Pushed: removed, branch and all, and the hook sees where it was.
        sh(&ahead.top, "git push -q origin w/ahead");
        let ahead_top = ahead.top.canonicalize().unwrap();
        let out = tmp.path().join("hook.out");
        let hooks = serde_json::json!([{"event": "WorkEnded", "command": format!(
            "echo \"$C6_WORKTREE|$C6_BRANCH|$C6_REPO|$C6_REMOVED\" > {}; cat > {}.json", out.display(), out.display())}]);
        let r = end(&end_of(&ahead, true, &hooks.to_string()), &[]).await;
        assert!(r.removed && r.kept.is_empty(), "{r:?}");
        assert!(!ahead.top.exists() && !branch_exists("w/ahead"));
        let said = std::fs::read_to_string(&out).unwrap();
        assert_eq!(said.trim(), format!("{}|w/ahead|{}|1", ahead_top.display(), ahead.root));
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(format!("{}.json", out.display())).unwrap()).unwrap();
        assert_eq!(payload["hook_event_name"], "WorkEnded");
        assert_eq!(r.hooks.len(), 1);

        // Merged into another branch (not pushed): removed.
        let merged = create(&wt, "s-0000000c", &clone, &WorktreeSpec { branch: "w/merged".into(), ..Default::default() }).await.unwrap();
        sh(&merged.top, &format!("echo m > m.txt && git add . && {GIT} commit -qm m"));
        sh(&clone, &format!("{GIT} merge -q --ff-only w/merged"));
        let r = end(&end_of(&merged, true, ""), &[]).await;
        assert!(r.removed, "{r:?}");

        // Not asked to remove: kept, and the hook says so.
        let keep = create(&wt, "s-0000000d", &clone, &WorktreeSpec { branch: "w/keep".into(), ..Default::default() }).await.unwrap();
        let r = end(&end_of(&keep, false, &hooks.to_string()), &[]).await;
        assert!(!r.removed && keep.top.exists());
        assert!(std::fs::read_to_string(&out).unwrap().trim().ends_with(&format!("|w/keep|{}|0", keep.root)));
    }
}
