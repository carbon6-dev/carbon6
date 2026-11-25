//! Worker-local artifacts for a fenced placement move.
//! Blocking API: call on a blocking thread, after fencing the source. The head
//! owns the commit decision; never abort after ownership changes.

use anyhow::{Context, Result, bail, ensure};
use nix::libc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_BYTES: u64 = 200_000_000;
const MAX_ENTRIES: usize = 4096;
const MAX_PATCH: u64 = 32 * 1024 * 1024;
const MAX_JSON: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub remote: String,
    /// Credential-free transport URL. Used only for an explicitly authorized clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_url: Option<String>,
    pub branch: String,
    pub head: String,
    pub subdir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub repo: Option<Repo>,
    pub native_resume: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    Directory,
    File,
    Bundle,
    Staged,
    Unstaged,
    Transcript,
    Metadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    kind: Kind,
    path: String,
    len: u64,
    mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    version: u32,
    pub move_id: Uuid,
    pub session_id: String,
    pub source: Source,
    pub bytes: u64,
    hashes: Vec<String>,
    entries: Vec<Entry>,
}

impl Manifest {
    pub fn copied_files(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter(|e| e.kind == Kind::File).map(|e| e.path.as_str())
    }
}

/// Target-local, obtained only from `preflight`, never from the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    repo_root: Option<PathBuf>,
    remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected: Option<Selected>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Selected {
    cwd: PathBuf,
    identity: (u64, u64),
    #[serde(default)]
    clone: bool,
    #[serde(default)]
    worktree: Option<WorktreeChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreeChoice {
    path: PathBuf,
    branch: String,
    base: String,
    track: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    pub cwd: PathBuf,
    /// The primary checkout for a linked worktree, or a standalone checkout.
    pub repo_root: Option<PathBuf>,
    pub branch: Option<String>,
    #[serde(default)]
    pub worktree_branch: String,
    pub native_resume: Option<Uuid>,
    pub metadata_dir: PathBuf,
    #[serde(default)]
    pub uncommitted_files: u32,
}

fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_dir(path.parent().context("directory has no parent")?),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let m = fs::symlink_metadata(path)?;
            ensure!(m.is_dir() && m.mode() & 0o077 == 0, "not a private move directory: {}", path.display());
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn locked(state: &Path, id: Uuid, side: &str) -> Result<(PathBuf, File)> {
    let root = state.canonicalize()?.join("moves");
    private_dir(&root)?;
    let dir = root.join(format!("{id}-{side}"));
    private_dir(&dir)?;
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(dir.join("lock"))?;
    lock.lock()?;
    Ok((dir, lock))
}

fn store(dir: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() as u64 <= MAX_JSON, "move receipt is too large");
    let temp = dir.join(format!(".{name}.new"));
    missing_ok(fs::remove_file(&temp))?;
    let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    fs::rename(&temp, dir.join(name))?;
    sync_dir(dir)
}

fn load<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Result<Option<T>> {
    match File::open(dir.join(name)) {
        Ok(f) => {
            let mut bytes = Vec::new();
            f.take(MAX_JSON + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() as u64 <= MAX_JSON, "oversized move receipt");
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn missing_ok(r: io::Result<()>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty() && path.len() <= 4096 && path.split('/').count() <= 32, "invalid artifact path length or depth");
    ensure!(
        path.split('/')
            .all(|p| !p.is_empty() && !matches!(p, "." | "..") && !p.eq_ignore_ascii_case(".git") && !p.contains(['\\', ':', '\0'])),
        "unsafe artifact path {path:?}"
    );
    Ok(())
}

fn open_at(dir: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK, 0o600) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
}

fn open_root(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "artifact root must be absolute");
    let mut dir = File::open("/")?;
    for part in path.components().skip(1) {
        let std::path::Component::Normal(part) = part else {
            bail!("unsafe artifact root");
        };
        dir = open_at(&dir, part, libc::O_RDONLY | libc::O_DIRECTORY)
            .with_context(|| format!("opening {} without symlinks", path.display()))?;
    }
    Ok(dir)
}

fn open_relative(root: &File, path: &str) -> Result<File> {
    validate_path(path)?;
    let mut parts = path.split('/').peekable();
    let mut f = root.try_clone()?;
    while let Some(p) = parts.next() {
        f = open_at(&f, OsStr::new(p), libc::O_RDONLY | if parts.peek().is_some() { libc::O_DIRECTORY } else { 0 })?;
    }
    Ok(f)
}

struct DirStream(*mut libc::DIR);
impl Drop for DirStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn names(dir: &File, remaining: usize) -> Result<Vec<String>> {
    let fd = dir.try_clone()?.into_raw_fd();
    let p = unsafe { libc::fdopendir(fd) };
    if p.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e.into());
    }
    let stream = DirStream(p);
    let mut out = Vec::new();
    loop {
        nix::errno::Errno::clear();
        let e = unsafe { libc::readdir(stream.0) };
        if e.is_null() {
            if nix::errno::Errno::last_raw() != 0 {
                return Err(io::Error::last_os_error().into());
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*e).d_name.as_ptr()) }.to_str()?;
        if !matches!(name, "." | "..") {
            ensure!(out.len() < remaining, "too many move entries");
            out.push(name.to_owned());
        }
    }
    out.sort_unstable();
    Ok(out)
}

fn stamp(m: &fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec())
}

struct Export {
    file: File,
    entries: Vec<Entry>,
    bytes: u64,
}
impl Export {
    fn add(&mut self, e: Entry, from: impl Read) -> Result<()> {
        ensure!(self.entries.len() < MAX_ENTRIES && e.len <= MAX_BYTES - self.bytes, "move exceeds artifact limits");
        ensure!(io::copy(&mut from.take(e.len + 1), &mut self.file)? == e.len, "artifact size changed while exporting");
        self.bytes += e.len;
        self.entries.push(e);
        Ok(())
    }

    fn file(&mut self, kind: Kind, path: String, f: &File) -> Result<()> {
        let m = f.metadata()?;
        ensure!(m.is_file(), "artifact {path:?} is not a regular file");
        self.add(Entry { kind, path, len: m.len(), mode: if kind == Kind::File { m.mode() & 0o777 } else { 0 } }, f)?;
        ensure!(stamp(&m) == stamp(&f.metadata()?), "artifact changed while exporting");
        Ok(())
    }

    fn scratch(&mut self, root: &File, parent: &str) -> Result<()> {
        let before = root.metadata()?;
        for name in names(root, MAX_ENTRIES - self.entries.len())? {
            let path = if parent.is_empty() { name.clone() } else { format!("{parent}/{name}") };
            validate_path(&path)?;
            let child = open_at(root, OsStr::new(&name), libc::O_RDONLY).with_context(|| format!("opening {path:?} (no symlinks)"))?;
            let m = child.metadata()?;
            if m.is_dir() {
                self.add(Entry { kind: Kind::Directory, path: path.clone(), len: 0, mode: m.mode() & 0o777 }, io::empty())?;
                self.scratch(&child, &path)?;
            } else {
                self.file(Kind::File, path, &child)?;
            }
        }
        ensure!(stamp(&before) == stamp(&root.metadata()?), "scratch directory changed while exporting");
        Ok(())
    }
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    for (k, _) in std::env::vars_os().filter(|(k, _)| k.as_bytes().starts_with(b"GIT_")) {
        c.env_remove(k);
    }
    c.current_dir(dir)
        .process_group(0)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-c", "core.protectHFS=true", "-c", "core.protectNTFS=true"])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    c
}

fn git_to(mut c: Command, limit: u64, out: &mut impl Write) -> Result<u64> {
    let mut child = c.spawn().context("starting Git for move")?;
    let result = (|| {
        let mut stdout = child.stdout.take().context("opening Git output")?;
        let fd = stdout.as_raw_fd();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut size = 0;
        let mut buf = [0_u8; 32 * 1024];
        loop {
            let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let wait = deadline.saturating_duration_since(Instant::now());
            ensure!(!wait.is_zero(), "Git timed out preparing a move");
            // No other thread reads this pipe; POLLIN/HUP gives a bounded read.
            let ready = unsafe { libc::poll(&mut poll, 1, wait.as_millis().max(1) as _) };
            if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            ensure!(ready > 0, "Git timed out or its pipe failed preparing a move");
            let n = stdout.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            ensure!(size <= limit, "Git move artifact exceeds its size limit");
            out.write_all(&buf[..n])?;
        }
        loop {
            if let Some(status) = child.try_wait()? {
                // Don't include argv: a configured remote can carry credentials.
                return if status.success() { Ok(size) } else { bail!("Git {:?} failed ({status})", c.get_args().nth(8)) };
            }
            ensure!(Instant::now() < deadline, "Git timed out preparing a move");
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() && child.try_wait().ok().flatten().is_none() {
        // Until reaped, this child's PID cannot be reused. Terminate any
        // descendants still holding the artifact pipe, then reap the child.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = child.wait();
    }
    result
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    git_to(command(dir, args), MAX_JSON, &mut out)?;
    Ok(out)
}

fn uncommitted_files(root: &Path) -> Result<u32> {
    let status = git(root, &["status", "--porcelain=v1", "-z", "--no-renames", "--untracked-files=all", "--ignore-submodules=none"])?;
    // Disabling rename detection gives one NUL-terminated record per dirty
    // path: an old/new pair is two paths, but staged + unstaged is one.
    Ok(u32::try_from(status.iter().filter(|&&byte| byte == 0).count())?)
}

fn text(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(dir, args)?)?.strip_suffix('\n').context("Git result has no line ending")?.to_owned())
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str().context("non-UTF-8 move path")
}

// Do not use session::normalize_remote as an authority check: that label
// discards non-default ports and accepts ext helpers. Local remotes match only
// on exactly the same absolute spelling; a local origin is never fetched.
fn remote_key(url: &str) -> Option<String> {
    let local = if url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("file://")) { &url[7..] } else { url };
    if let Some(path) = local.strip_prefix('/') {
        let path = path.trim_end_matches('/');
        if !path.contains(['\0', '\n', '\r', '\\', '%']) && path.split('/').all(|p| !p.is_empty() && !matches!(p, "." | "..")) {
            return Some(format!("file:///{path}"));
        }
        return None;
    }
    if url.contains(['\n', '\r', '\t', ' ', '?', '#', '\\', '%']) {
        return None;
    }
    let (scheme, authority, path) = if let Some((scheme, tail)) = url.split_once("://") {
        let (authority, path) = tail.split_once('/')?;
        (scheme, authority, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        ("ssh", authority, path)
    };
    let scheme = scheme.to_ascii_lowercase();
    let suffix = match scheme.as_str() {
        "ssh" => ":22",
        "https" => ":443",
        "http" => ":80",
        "git" => ":9418",
        _ => return None,
    };
    let authority = authority.rsplit('@').next()?;
    let host = authority.strip_suffix(suffix).unwrap_or(authority);
    let (host_part, port) = host.split_once(':').map_or((host, None), |(h, p)| (h, Some(p)));
    if host_part.is_empty()
        || !host_part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        || port.is_some_and(|p| p.parse::<u16>().ok().is_none_or(|p| p == 0))
    {
        return None;
    }
    // GitHub repository paths are case-insensitive. Other servers (and local
    // paths above) may distinguish case, so never fold those identities.
    let folded;
    let path = if host_part.eq_ignore_ascii_case("github.com") {
        folded = path.to_ascii_lowercase();
        folded.as_str()
    } else {
        path
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if !path.split('/').all(|p| !p.is_empty() && !matches!(p, "." | "..") && !p.contains(':')) {
        return None;
    }
    // For non-default ports don't match between protocols.
    Some(if port.is_some() {
        format!("{scheme}://{}/{path}", host.to_ascii_lowercase())
    } else {
        format!("{}/{path}", host.to_ascii_lowercase())
    })
}

fn remote(dir: &Path) -> Result<(String, String)> {
    let url = text(dir, &["remote", "get-url", "origin"]).or_else(|_| {
        let names = text(dir, &["remote"])?;
        let name = names.lines().next().context("repository has no Git remote")?;
        text(dir, &["remote", "get-url", "--", name])
    })?;
    let id = remote_key(&url).context("repository has no supported network Git remote")?;
    Ok((id, url))
}

fn strip_remote_credentials(url: &str) -> Option<String> {
    if let Some((scheme, tail)) = url.split_once("://").filter(|(scheme, _)| !scheme.eq_ignore_ascii_case("file")) {
        let scheme = scheme.to_ascii_lowercase();
        let (authority, path) = tail.split_once('/')?;
        let authority = match authority.rsplit_once('@') {
            Some((user, host)) if scheme != "ssh" || user.contains(':') => host,
            _ => authority,
        };
        Some(format!("{scheme}://{authority}/{path}"))
    } else {
        Some(url.to_owned())
    }
}

fn clone_remote(key: &str, url: &str) -> Option<(String, &'static str)> {
    let url = strip_remote_credentials(url)?;
    if remote_key(&url).as_deref() != Some(key) {
        return None;
    }
    if key.starts_with("file:///") {
        return Some((key.to_owned(), "file"));
    }
    let (scheme, authority, path) = if let Some((scheme, tail)) = url.split_once("://") {
        let (authority, path) = tail.split_once('/')?;
        (scheme, authority, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        ("ssh", authority, path)
    };
    let protocol = match scheme {
        "https" => "https",
        "http" => "http",
        "ssh" => "ssh",
        "git" => "git",
        _ => return None,
    };
    let word = |s: &str| !s.is_empty() && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    let host = if let Some((user, host)) = authority.rsplit_once('@') {
        if scheme != "ssh" || !word(user) {
            return None;
        }
        host
    } else {
        authority
    };
    if !word(host.split(':').next()?) || !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b)) {
        return None;
    }
    Some((url, protocol))
}

fn repo_root(cwd: &Path) -> Result<PathBuf> {
    Ok(Path::new(&text(cwd, &["rev-parse", "--show-toplevel"])?).canonicalize()?)
}

/// Inspect the source without freezing it; export checks this same descriptor
/// again. Pass `repo = false` only when intentionally transferring scratch.
pub fn describe(cwd: &Path, repo: bool, native_resume: Option<Uuid>) -> Result<Source> {
    let repo = if repo {
        let root = repo_root(cwd)?;
        let cwd = cwd.canonicalize()?;
        let subdir = path_str(cwd.strip_prefix(&root)?)?.to_owned();
        if !subdir.is_empty() {
            validate_path(&subdir)?;
        }
        let (remote, url) = remote(&root)?;
        Some(Repo {
            clone_url: clone_remote(&remote, &url).map(|(url, _)| url),
            remote,
            branch: text(&root, &["symbolic-ref", "--short", "HEAD"]).context("detached HEAD is unsupported for moves")?,
            head: text(&root, &["rev-parse", "--verify", "HEAD^{commit}"])?,
            subdir,
        })
    } else {
        None
    };
    Ok(Source { repo, native_resume })
}

/// Call only after the head has checked realm/worker access. Supply worker-local
/// known folders (e.g. prior placements/allowed picker roots, not a home scan).
pub fn preflight(source: &Source, known_folders: &[PathBuf]) -> Result<Target> {
    let Some(repo) = &source.repo else {
        return Ok(Target { repo_root: None, remote: None, selected: None });
    };
    known_match(repo, known_folders)?.context("no known destination repository matches; select or explicitly clone it first")
}

/// A deliberately bounded inventory: recorded folders, their immediate
/// parents, and worker-owned storage. Never recursively search the home tree.
pub(crate) fn repository_folders(state: &Path, known: &[PathBuf], wanted: Option<&str>) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    let mut roots = BTreeSet::from([state.to_path_buf()]);
    let mut exact = BTreeSet::new();
    for cwd in known {
        if let Some(top) = cwd.ancestors().find(|p| p.join(".git").exists()) {
            exact.insert(top.to_path_buf());
        }
        roots.insert(cwd.clone());
        if let Some(parent) = cwd.parent().filter(|p| p.parent().is_some()) {
            roots.insert(parent.to_path_buf());
        }
    }
    for path in exact {
        if let Ok(root) = repo_root(&path) {
            found.insert(root);
        }
    }
    let mut pending: VecDeque<_> = roots.into_iter().map(|p| (p, 0)).collect();
    let mut seen = BTreeSet::new();
    let mut budget = 1024;
    while let Some((path, depth)) = pending.pop_front() {
        if budget == 0 || found.len() >= 256 {
            break;
        }
        budget -= 1;
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if !meta.is_dir() || !seen.insert(path.clone()) {
            continue;
        }
        if path.join(".git").exists() {
            if let Ok(root) = repo_root(&path) {
                found.insert(root);
            }
            // A repository's own descendants are not discovery roots.
            continue;
        }
        if depth >= 3 {
            continue;
        }
        let Ok(children) = fs::read_dir(&path) else { continue };
        let mut children: Vec<_> = children.take(256).filter_map(Result::ok).collect();
        children.sort_by_key(|entry| entry.file_name());
        for entry in children {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || matches!(name.as_ref(), "target" | "node_modules" | "dist") {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push_back((entry.path(), depth + 1));
            }
        }
    }
    let mut folders: Vec<_> =
        found.into_iter().filter(|root| wanted.is_none_or(|key| remote(root).is_ok_and(|(actual, _)| actual == key))).collect();
    let state = state.canonicalize().unwrap_or_else(|_| state.to_path_buf());
    folders.sort_by_key(|root| {
        (root.starts_with(state.join("moves")) || root.starts_with(state.join("repositories")), root.join(".git").is_file(), root.clone())
    });
    folders
}

pub(crate) fn list_repositories(state: &Path, known: &[PathBuf], cwd: Option<&Path>, wanted: Option<&str>) -> Result<serde_json::Value> {
    if let Some(cwd) = cwd {
        // Scratch sessions have no repository and should still be movable.
        return match repo_root(cwd) {
            Ok(root) => Ok(serde_json::json!({"remote": remote(&root)?.0, "folders": [root]})),
            Err(_) => Ok(serde_json::json!({"remote": null, "folders": []})),
        };
    }
    Ok(serde_json::json!({"remote": wanted, "folders": repository_folders(state, known, wanted)}))
}

fn clean_checkout(root: &Path) -> Result<()> {
    ensure!(
        git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignored=matching", "--ignore-submodules=none"])?
            .is_empty(),
        "Folder: destination checkout has staged, unstaged, untracked or ignored files; choose a clean checkout or a new worktree"
    );
    ensure!(
        git(root, &["ls-files", "-v", "-z"])?.split(|b| *b == 0).all(|p| p.is_empty() || p[0] == b'H'),
        "Folder: destination has assume-unchanged or skip-worktree index entries"
    );
    Ok(())
}

/// An explicit destination bypasses discovery ambiguity entirely. Its identity
/// is frozen into the head's plan and checked again before any target writes.
pub(crate) fn preflight_selected(source: &Source, state: &Path, id: Uuid, request: &serde_json::Value) -> Result<Target> {
    let raw = request["cwd"].as_str().context("Folder: choose a destination folder")?;
    let cwd = crate::expand_home(raw);
    ensure!(cwd.is_absolute(), "Folder: choose an absolute destination folder");
    let cwd = match cwd.canonicalize() {
        Ok(cwd) => cwd,
        Err(e) if e.kind() == io::ErrorKind::NotFound && request["clone"] == true => {
            ensure!(source.repo.is_some(), "Folder: scratch moves do not clone repositories");
            let parent = cwd
                .parent()
                .context("Folder: choose a new folder below an existing directory")?
                .canonicalize()
                .context("Folder: the clone parent directory must already exist")?;
            let name = cwd.file_name().context("Folder: choose a new clone folder")?;
            let cwd = parent.join(name);
            ensure!(
                fs::symlink_metadata(&cwd).is_err_and(|e| e.kind() == io::ErrorKind::NotFound),
                "Folder: clone destination already exists"
            );
            let meta = open_root(&parent)?.metadata()?;
            let mut target = preflight_with_clone(source, &[], state, true)?;
            target.selected = Some(Selected { cwd, identity: (meta.dev(), meta.ino()), clone: true, worktree: None });
            if !request["worktree"].is_null() {
                // Clone is separately authorized inventory creation, not part
                // of the session ownership transaction. Publish a clean seed
                // checkout once and retain it even if later move validation
                // refuses a base or branch. Unpushed source commits arrive in
                // the move bundle, not through this clone.
                let (dir, _lock) = locked(state, id, "clone")?;
                let work = dir.join("workspace");
                missing_ok(fs::remove_dir_all(&work))?;
                private_dir(&work)?;
                let seed = target.repo_root.as_ref().unwrap();
                let (_, url) = remote(seed)?;
                let url = strip_remote_credentials(&url).context("Folder: unsafe clone remote")?;
                let mut cmd = command(&work, &["clone", "--no-hardlinks", "--no-checkout", "--template=", "--", path_str(seed)?, "."]);
                cmd.env("GIT_ALLOW_PROTOCOL", "file");
                git_to(cmd, MAX_JSON, &mut io::sink()).context("Folder: cannot initialize selected clone")?;
                git(&work, &["remote", "set-url", "origin", &url])?;
                git(&work, &["reset", "--hard", "HEAD"])?;
                publish_clone(&dir, &target, &work)?;
                return preflight_selected(source, state, id, request);
            }
            return Ok(target);
        }
        Err(e) => return Err(e).context("Folder: choose an existing destination folder, or enable Clone here for a new folder"),
    };
    let identity = open_root(&cwd)?.metadata()?;
    let mut selected = Selected { cwd: cwd.clone(), identity: (identity.dev(), identity.ino()), clone: false, worktree: None };
    let Some(repo) = &source.repo else {
        ensure!(request["worktree"].is_null(), "Worktree: scratch moves cannot create a repository worktree");
        return Ok(Target { repo_root: None, remote: None, selected: Some(selected) });
    };
    let root = repo_root(&cwd).context("Folder: choose a checkout of the source repository, or enable Clone here in a new folder")?;
    ensure!(remote(&root)?.0 == repo.remote, "Folder: selected checkout does not match the source repository");
    if !request["worktree"].is_null() {
        let options: c6_api::starts::WorktreeOptions = serde_json::from_value(request["worktree"].clone())?;
        ensure!(options.setup.is_empty(), "Worktree: setup commands are not supported during a cross-worker transfer");
        ensure!(!options.track, "Worktree: tracking is not supported during a cross-worker transfer; set upstream after moving");
        ensure!(options.cleanup_mode() == "off", "Worktree: transferred worktrees must be retained (cleanup off)");
        let branch =
            if options.branch.trim().is_empty() { format!("c6/move-{id}") } else { options.branch.replace("{{id}}", &id.to_string()) };
        git(&root, &["check-ref-format", &format!("refs/heads/{branch}")]).context("Worktree: invalid destination branch")?;
        ensure!(
            git(&root, &["show-ref", "--verify", &format!("refs/heads/{branch}")]).is_err(),
            "Worktree: destination branch already exists; choose a new branch"
        );
        ensure!(!options.fetch, "Worktree: fetch the destination repository first, then move without fetch");
        if !options.base.trim().is_empty() {
            ensure!(
                text(&root, &["rev-parse", "--verify", "--end-of-options", &format!("{}^{{commit}}", options.base)])? == repo.head,
                "Worktree: the selected base must equal the source commit when transferring work"
            );
        }
        let path = state.canonicalize()?.join("moves").join(format!("{id}-import")).join("linked");
        selected.worktree = Some(WorktreeChoice { path, branch, base: options.base, track: options.track });
    } else {
        clean_checkout(&root)?;
        ensure!(
            text(&root, &["rev-parse", "HEAD"])? == repo.head,
            "Folder: destination is at a different commit; choose a new worktree to transfer the source commit safely"
        );
    }
    Ok(Target { repo_root: Some(root), remote: Some(repo.remote.clone()), selected: Some(selected) })
}

fn known_match(repo: &Repo, known_folders: &[PathBuf]) -> Result<Option<Target>> {
    let mut matched = BTreeMap::new();
    for cwd in known_folders {
        if let Ok(root) = repo_root(cwd)
            && let Ok((key, _)) = remote(&root)
            && key == repo.remote
        {
            matched.insert(root, key);
        }
    }
    ensure!(matched.len() <= 1, "multiple destination repositories match; select a specific repository");
    Ok(matched.into_iter().next().map(|(root, key)| Target { repo_root: Some(root), remote: Some(key), selected: None }))
}

fn is_bare_root(root: &Path) -> Result<bool> {
    Ok(text(root, &["rev-parse", "--is-bare-repository"])? == "true"
        && Path::new(&text(root, &["rev-parse", "--absolute-git-dir"])?).canonicalize()? == root)
}

fn publish_seed(from: &Path, to: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // Unlike rename(), these variants cannot replace an empty directory
        // created between preflight and publication.
        #[cfg(target_os = "linux")]
        let status = unsafe { libc::renameat2(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE) };
        #[cfg(target_os = "macos")]
        let status = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        if status != 0 {
            return Err(io::Error::last_os_error()).context("publishing managed repository without replacing an existing entry");
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (from, to);
        bail!("atomic managed repository publication is unsupported on this worker")
    }
}

/// Target-local opt-in clone. An already prepared seed is reusable without a
/// new network operation. `allow_clone` must come from the frozen user request,
/// not the source descriptor. No session actor has to be frozen for this call.
pub fn preflight_with_clone(source: &Source, known_folders: &[PathBuf], state: &Path, allow_clone: bool) -> Result<Target> {
    let Some(repo) = &source.repo else {
        return preflight(source, known_folders);
    };
    if let Some(target) = known_match(repo, known_folders)? {
        return Ok(target);
    }

    let stores = state.canonicalize()?.join("repositories");
    let key = c6_core::sha256_hex(repo.remote.as_bytes());
    let root = stores.join(&key);
    let target = Target { repo_root: Some(root.clone()), remote: Some(repo.remote.clone()), selected: None };
    if !allow_clone {
        ensure!(
            fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()),
            "no known destination repository matches; select or explicitly clone it first"
        );
        validate_target(source, &target)?;
        return Ok(target);
    }

    private_dir(&stores)?;
    // Serialize publication across moves matching this remote, not just
    // retries of one move. Never replace an existing directory or symlink.
    let lock =
        OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(stores.join(format!(".{key}.lock")))?;
    lock.lock()?;
    match fs::symlink_metadata(&root) {
        Ok(m) => {
            ensure!(m.is_dir(), "managed repository path conflicts with an existing entry");
            validate_target(source, &target)?;
            return Ok(target);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let (url, protocol) =
        repo.clone_url.as_deref().and_then(|url| clone_remote(&repo.remote, url)).context("no safe clone URL for this repository")?;
    let stage = stores.join(format!(".{key}.clone"));
    missing_ok(fs::remove_dir_all(&stage))?;
    private_dir(&stage)?;
    let result = (|| {
        let mut cmd = command(
            &stage,
            &["clone", "--bare", "--single-branch", "--no-tags", "--depth=1", "--no-local", "--template=", "--", &url, "."],
        );
        cmd.env("GIT_ALLOW_PROTOCOL", protocol).env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
        // Git streams the compressed pack to disk. Keep even that temporary
        // file bounded; the full stored tree is checked again before publish.
        unsafe {
            cmd.pre_exec(|| {
                let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                if libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
                limit.rlim_cur = limit.rlim_cur.min(MAX_BYTES as libc::rlim_t);
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        git_to(cmd, MAX_JSON, &mut io::sink()).context("cloning destination repository")?;
        ensure!(is_bare_root(&stage)? && remote(&stage)?.0 == repo.remote, "clone does not match the expected repository");
        let head = text(&stage, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        tree(&stage, &head)?;
        let mut remaining = MAX_BYTES;
        check_seed_size(&stage, &mut remaining)?;
        sync_checkout(&stage)?;
        publish_seed(&stage, &root)?;
        sync_dir(&stores)?;
        Ok(target)
    })();
    if result.is_err() {
        missing_ok(fs::remove_dir_all(&stage))?;
    }
    result
}

fn check_seed_size(dir: &Path, remaining: &mut u64) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            check_seed_size(&entry.path(), remaining)?;
        } else {
            ensure!(ty.is_file(), "unexpected symlink or special file in repository clone");
            *remaining = remaining.checked_sub(entry.metadata()?.len()).context("repository clone exceeds 200 MB")?;
        }
    }
    Ok(())
}

fn validate_target(source: &Source, target: &Target) -> Result<()> {
    if let Some(selected) = &target.selected {
        let check = if selected.clone { selected.cwd.parent().context("Folder: clone destination has no parent")? } else { &selected.cwd };
        let meta = open_root(check)?.metadata()?;
        ensure!((meta.dev(), meta.ino()) == selected.identity, "Folder: selected destination was replaced");
    }
    match (&source.repo, &target.repo_root) {
        (None, None) => Ok(()),
        (Some(repo), Some(root)) => {
            ensure!(
                target.remote.as_ref() == Some(&repo.remote)
                    && fs::symlink_metadata(root)?.is_dir()
                    && (repo_root(root).is_ok_and(|actual| actual == *root) || is_bare_root(root)?)
                    && remote(root)?.0 == repo.remote,
                "destination repository identity changed"
            );
            Ok(())
        }
        _ => bail!("target does not match move source"),
    }
}

fn tree(dir: &Path, head: &str) -> Result<()> {
    let mut files = 0;
    let mut bytes = 0;
    for row in git(dir, &["ls-tree", "-rlz", head])?.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let (meta, name) = std::str::from_utf8(row)?.split_once('\t').context("invalid Git tree row")?;
        validate_path(name)?;
        let mut fields = meta.split_whitespace();
        ensure!(matches!(fields.next(), Some("100644" | "100755")), "Git symlink or gitlink cannot be moved: {name:?}");
        ensure!(fields.next() == Some("blob"), "not a regular Git blob");
        bytes += fields.nth(1).context("Git blob is missing size")?.parse::<u64>()?.min(MAX_BYTES + 1);
        files += 1;
        ensure!(files <= MAX_ENTRIES && bytes <= MAX_BYTES, "Git checkout exceeds move limits");
    }
    Ok(())
}

fn git_blob(export: &mut Export, kind: Kind, cmd: Command, limit: u64) -> Result<()> {
    let len = git_to(cmd, limit.min(MAX_BYTES - export.bytes), &mut export.file)?;
    ensure!(export.entries.len() < MAX_ENTRIES, "move exceeds artifact limits");
    export.entries.push(Entry { kind, path: String::new(), len, mode: 0 });
    export.bytes += len;
    Ok(())
}

fn extract_repo(export: &mut Export, root: &Path, repo: &Repo) -> Result<()> {
    // -v includes index status: lowercase means assume-unchanged, and S
    // means skip-worktree. Both can hide edits from diff. Disable sparse
    // checkout for this read so Git doesn't clear S in memory for files it
    // finds on disk.
    const INDEX: &[&str] = &["-c", "core.sparseCheckout=false", "ls-files", "--stage", "-v", "-z"];
    tree(root, &repo.head)?;
    // Diff/index refresh can invoke a repository's custom clean filters. Do
    // not run them in a move, even if that means refusing a legitimate repo.
    let local_config = git(root, &["config", "--name-only", "--list", "-z"])?;
    ensure!(!local_config.split(|b| *b == 0).any(|k| k.starts_with(b"filter.")), "repositories with custom Git filters cannot move");
    let indexed = git(root, INDEX)?;
    let fd = open_root(root)?;
    for row in indexed.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let (meta, path) = std::str::from_utf8(row)?.split_once('\t').context("invalid Git index row")?;
        let (tag, meta) = meta.split_once(' ').context("invalid Git index status tag")?;
        ensure!(
            tag != "S" && !tag.as_bytes().iter().any(u8::is_ascii_lowercase),
            "Git assume-unchanged or skip-worktree index entry cannot be moved: {path:?}"
        );
        ensure!(
            meta.ends_with(" 0") && (meta.starts_with("100644 ") || meta.starts_with("100755 ")),
            "Git conflict, gitlink, or symlink in index: {path:?}"
        );
        validate_path(path)?;
        match open_relative(&fd, path) {
            Ok(f) => ensure!(f.metadata()?.is_file(), "non-regular Git working file: {path:?}"),
            Err(e) if e.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => {}
            Err(e) => return Err(e.context(format!("checking Git working file {path:?}"))),
        }
    }
    // Full single-head bundle: no remote push, no missing prerequisites, no
    // temporary source refs. Large histories are refused, not silently dropped.
    git_blob(export, Kind::Bundle, command(root, &["bundle", "create", "-", "HEAD"]), MAX_BYTES)?;
    const DIFF: &[&str] = &[
        "diff",
        "--binary",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-color",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    git_blob(export, Kind::Staged, command(root, &[DIFF, &["--cached", &repo.head, "--"]].concat()), MAX_PATCH)?;
    git_blob(export, Kind::Unstaged, command(root, &[DIFF, &["--"]].concat()), MAX_PATCH)?;
    for path in git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let path = std::str::from_utf8(path)?;
        export.file(Kind::File, path.to_owned(), &open_relative(&fd, path)?)?;
    }
    ensure!(
        text(root, &["rev-parse", "--verify", "HEAD^{commit}"])? == repo.head && git(root, INDEX)? == indexed,
        "source HEAD or index changed during export"
    );
    Ok(())
}

/// Identical to the CLI project-directory spelling in
/// `claude_code::transcript_of`. No fallback scan of other projects for moves.
fn project_name(cwd: &Path) -> Result<String> {
    Ok(path_str(cwd)?.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect())
}

fn mkdir_at(dir: &File, name: &str) -> Result<()> {
    let name = CString::new(name)?;
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        dir.sync_all()?;
    } else {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }
    Ok(())
}

fn native_config(config_dir: Option<&Path>) -> Result<PathBuf> {
    let path = std::path::absolute(config_dir.context("native move requires the worker's configured Claude directory")?)?;
    // macOS spells these system directories with root-level symlink aliases.
    // Resolve only the known aliases, not arbitrary links in a config path.
    #[cfg(target_os = "macos")]
    for (alias, real) in [("/tmp", "/private/tmp"), ("/var", "/private/var")] {
        if let Ok(suffix) = path.strip_prefix(alias) {
            return Ok(Path::new(real).join(suffix));
        }
    }
    Ok(path)
}

fn secure_directory(dir: &File, path: &Path, uid: u32, ancestor: bool, under_sticky: bool) -> Result<bool> {
    let meta = dir.metadata()?;
    ensure!(meta.uid() == uid || (!under_sticky && meta.uid() == 0), "untrusted owner of native transcript directory {}", path.display());
    let sticky = ancestor && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
    let shared = meta.mode() & 0o022 != 0;
    ensure!(!shared || sticky, "group/other-writable native transcript directory {}", path.display());
    Ok(shared && sticky)
}

fn project(config: &Path, cwd: &Path, create: bool) -> Result<File> {
    ensure!(config.is_absolute(), "native config directory must be absolute");
    let uid = unsafe { libc::geteuid() };
    let mut path = PathBuf::from("/");
    let mut dir = File::open(&path)?;
    let mut parts = config.components().skip(1).peekable();
    let mut under_sticky = secure_directory(&dir, &path, uid, parts.peek().is_some(), false)?;
    while let Some(part) = parts.next() {
        let std::path::Component::Normal(part) = part else {
            bail!("unsafe native config path {}", config.display());
        };
        path.push(part);
        dir = open_at(&dir, part, libc::O_RDONLY | libc::O_DIRECTORY)
            .with_context(|| format!("opening native transcript directory {} without symlinks", path.display()))?;
        // A root-owned shared sticky ancestor (e.g. /private/tmp) cannot
        // have our own directory entry replaced by another user.
        under_sticky = secure_directory(&dir, &path, uid, parts.peek().is_some(), under_sticky)?;
    }
    let root = dir;
    if create {
        mkdir_at(&root, "projects")?;
    }
    let projects = open_at(&root, OsStr::new("projects"), libc::O_RDONLY | libc::O_DIRECTORY)?;
    path.push("projects");
    secure_directory(&projects, &path, uid, false, false)?;
    let name = project_name(cwd)?;
    if create {
        mkdir_at(&projects, &name)?;
    }
    let project = open_at(&projects, OsStr::new(&name), libc::O_RDONLY | libc::O_DIRECTORY)?;
    path.push(name);
    // linkat/unlinkat act on names, not the inode we opened. No untrusted
    // user may substitute those names in the checked directory.
    secure_directory(&project, &path, uid, false, false)?;
    Ok(project)
}

fn native_names(id: Uuid, resume: Uuid) -> (CString, CString) {
    (CString::new(format!(".c6-move-{id}.jsonl")).unwrap(), CString::new(format!("{resume}.jsonl")).unwrap())
}

fn native_open(dir: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    open_at(dir, OsStr::from_bytes(name.to_bytes()), flags)
}

fn unlink_at(dir: &File, name: &CStr) -> Result<()> {
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } == 0 { Ok(()) } else { missing_ok(Err(io::Error::last_os_error())) }
}

fn native_identity(f: &File) -> Result<(u64, u64)> {
    let m = f.metadata()?;
    ensure!(m.is_file(), "not a regular native transcript");
    ensure!(m.uid() == 0 || m.uid() == unsafe { libc::geteuid() }, "untrusted owner of native transcript file");
    ensure!(m.mode() & 0o022 == 0, "group/other-writable native transcript file");
    Ok((m.dev(), m.ino()))
}

// The staging hardlink is an ownership witness, including after a crash at
// linkat. Never unlink a canonical transcript unless both names still refer
// to the inode the move prepared. Existing native histories are not replaced.
fn native_remove(dir: &File, id: Uuid, resume: Uuid, rollback: bool) -> Result<()> {
    let (temp, final_name) = native_names(id, resume);
    match native_open(dir, &temp, libc::O_RDONLY) {
        Ok(staged) => {
            let staged = native_identity(&staged)?;
            if rollback
                && let Ok(installed) = native_open(dir, &final_name, libc::O_RDONLY)
                && native_identity(&installed).is_ok_and(|installed| installed == staged)
            {
                unlink_at(dir, &final_name)?;
            }
            unlink_at(dir, &temp)?;
            dir.sync_all()?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn native_install(dir: &File, id: Uuid, resume: Uuid, from: impl Read, len: u64) -> Result<()> {
    let (temp, final_name) = native_names(id, resume);
    native_remove(dir, id, resume, true)?;
    let mut f = native_open(dir, &temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
    copy_exact(from, &mut f, len)?;
    f.sync_all()?;
    dir.sync_all()?;
    let fd = dir.as_raw_fd();
    if unsafe { libc::linkat(fd, temp.as_ptr(), fd, final_name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error()).context("destination native transcript already exists or cannot be installed");
    }
    dir.sync_all()?;
    Ok(())
}

fn copy_exact(from: impl Read, to: &mut impl Write, size: u64) -> Result<()> {
    ensure!(io::copy(&mut from.take(size), to)? == size, "incomplete move artifact");
    Ok(())
}

fn chunk_len(bytes: u64, offset: u64) -> Result<usize> {
    ensure!(offset < bytes && offset.is_multiple_of(CHUNK_BYTES as u64), "invalid move chunk offset");
    Ok((bytes - offset).min(CHUNK_BYTES as u64) as usize)
}

fn chunk(file: &mut File, bytes: u64, offset: u64) -> Result<Vec<u8>> {
    let mut data = vec![0; chunk_len(bytes, offset)?];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut data)?;
    Ok(data)
}

fn check_chunk(m: &Manifest, offset: u64, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() == chunk_len(m.bytes, offset)? && c6_core::sha256_hex(bytes) == m.hashes[offset as usize / CHUNK_BYTES],
        "move chunk length or digest mismatch"
    );
    Ok(())
}

fn validate(m: &Manifest) -> Result<()> {
    ensure!(m.version == 1 && !m.session_id.is_empty() && m.session_id.len() <= 128 && m.bytes <= MAX_BYTES, "invalid move manifest");
    ensure!(
        m.entries.len() <= MAX_ENTRIES
            && m.hashes.len() as u64 == m.bytes.div_ceil(CHUNK_BYTES as u64)
            && m.hashes.iter().all(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid move manifest limits"
    );
    if let Some(r) = &m.source.repo {
        ensure!(matches!(r.head.len(), 40 | 64) && r.head.bytes().all(|b| b.is_ascii_hexdigit()), "invalid Git commit");
        ensure!(!r.branch.is_empty() && r.branch.len() <= 1024 && !r.branch.starts_with('-'), "invalid move Git branch");
        if !r.subdir.is_empty() {
            validate_path(&r.subdir)?;
        }
    }
    let mut paths = BTreeMap::new();
    let mut unique = [false; 4];
    let mut metadata = [false; 2];
    let mut total = 0;
    for e in &m.entries {
        ensure!(e.mode & !0o777 == 0 && e.len <= MAX_BYTES - total, "invalid move entry size or mode");
        total += e.len;
        match e.kind {
            Kind::Directory | Kind::File => {
                validate_path(&e.path)?;
                if e.kind == Kind::Directory {
                    ensure!(e.len == 0 && m.source.repo.is_none(), "invalid directory entry");
                }
                ensure!(paths.insert(e.path.as_str(), e.kind).is_none(), "duplicate move path");
            }
            Kind::Metadata => {
                let idx = match e.path.as_str() {
                    "spec.pb" => 0,
                    "events.pb" => 1,
                    _ => bail!("unknown move metadata file"),
                };
                ensure!(e.mode == 0 && !metadata[idx], "invalid or duplicate move metadata");
                metadata[idx] = true;
            }
            k => {
                let (idx, allowed, limit) = match k {
                    Kind::Bundle => (0, m.source.repo.is_some() && e.len > 0, MAX_BYTES),
                    Kind::Staged => (1, m.source.repo.is_some(), MAX_PATCH),
                    Kind::Unstaged => (2, m.source.repo.is_some(), MAX_PATCH),
                    Kind::Transcript => (3, m.source.native_resume.is_some() && e.len > 0, MAX_BYTES),
                    _ => unreachable!(),
                };
                ensure!(allowed && !unique[idx] && e.path.is_empty() && e.mode == 0 && e.len <= limit, "invalid special move entry");
                unique[idx] = true;
            }
        }
    }
    for path in paths.keys() {
        for (i, _) in path.match_indices('/') {
            ensure!(!matches!(paths.get(&path[..i]), Some(Kind::File)), "conflicting artifact paths");
        }
    }
    ensure!(
        total == m.bytes
            && unique == [m.source.repo.is_some(), m.source.repo.is_some(), m.source.repo.is_some(), m.source.native_resume.is_some()],
        "move manifest is incomplete"
    );
    Ok(())
}

/// Capture a frozen source. `config_dir` is the source worker's Claude config
/// root (not `projects`). The caller resolves any home fallback explicitly.
pub fn prepare_export(
    state: &Path,
    id: Uuid,
    session: &str,
    cwd: &Path,
    source: &Source,
    config_dir: Option<&Path>,
    metadata: &[(&str, &[u8])],
) -> Result<Manifest> {
    let (dir, _lock) = locked(state, id, "export")?;
    ensure!(!dir.join("finished").exists(), "export is already retired");
    if let Some(old) = load::<Manifest>(&dir, "manifest")? {
        ensure!(old.session_id == session && old.source == *source, "move ID already has a different export");
        return Ok(old);
    }
    let cwd = cwd.canonicalize()?;
    ensure!(describe(&cwd, source.repo.is_some(), source.native_resume)? == *source, "move source changed since selection");
    struct Building(PathBuf);
    impl Drop for Building {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let building = Building(dir.join("payload.building"));
    let temp = &building.0;
    missing_ok(fs::remove_file(temp))?;
    let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(temp)?;
    let mut out = Export { file, entries: Vec::new(), bytes: 0 };
    if let Some(repo) = &source.repo {
        extract_repo(&mut out, &repo_root(&cwd)?, repo)?;
    } else {
        out.scratch(&open_root(&cwd)?, "")?;
    }
    if let Some(native) = source.native_resume {
        let path = format!("{native}.jsonl");
        let project = project(&native_config(config_dir)?, &cwd, false)?;
        let transcript = open_at(&project, OsStr::new(&path), libc::O_RDONLY).context("opening original Claude transcript")?;
        native_identity(&transcript).context("checking original Claude transcript")?;
        out.file(Kind::Transcript, String::new(), &transcript)?;
    }
    for &(name, data) in metadata {
        ensure!(matches!(name, "spec.pb" | "events.pb"), "unknown move metadata file");
        out.add(Entry { kind: Kind::Metadata, path: name.to_owned(), len: data.len() as u64, mode: 0 }, data)?;
    }
    let mut m = Manifest {
        version: 1,
        move_id: id,
        session_id: session.to_owned(),
        source: source.clone(),
        bytes: out.bytes,
        hashes: Vec::new(),
        entries: out.entries,
    };
    for offset in (0..m.bytes).step_by(CHUNK_BYTES) {
        m.hashes.push(c6_core::sha256_hex(&chunk(&mut out.file, m.bytes, offset)?));
    }
    validate(&m)?;
    out.file.sync_all()?;
    fs::rename(temp, dir.join("payload"))?;
    sync_dir(&dir)?;
    store(&dir, "manifest", &m)?;
    Ok(m)
}

pub fn read_chunk(state: &Path, id: Uuid, offset: u64) -> Result<Vec<u8>> {
    let (dir, _lock) = locked(state, id, "export")?;
    let m = load::<Manifest>(&dir, "manifest")?.context("move export is not ready")?;
    let data = chunk(&mut File::open(dir.join("payload"))?, m.bytes, offset)?;
    check_chunk(&m, offset, &data)?;
    Ok(data)
}

pub fn finish_export(state: &Path, id: Uuid) -> Result<()> {
    let (dir, _lock) = locked(state, id, "export")?;
    store(&dir, "finished", &true)?;
    missing_ok(fs::remove_file(dir.join("payload.building")))?;
    missing_ok(fs::remove_file(dir.join("payload")))?;
    sync_dir(&dir)
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Import {
    manifest: Manifest,
    target: Target,
}

pub fn begin_import(state: &Path, m: &Manifest, target: &Target) -> Result<()> {
    validate(m)?;
    let (dir, _lock) = locked(state, m.move_id, "import")?;
    ensure!(!dir.join("aborted").exists(), "move import was aborted");
    if let Some(i) = load::<Import>(&dir, "import")? {
        ensure!(i.manifest == *m && i.target == *target, "move ID already has a different import");
        return Ok(());
    }
    validate_target(&m.source, target)?;
    OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(dir.join("payload"))?.sync_all()?;
    store(&dir, "import", &Import { manifest: m.clone(), target: target.clone() })
}

/// Sequential fixed frames. Exactly repeated chunks return success; interrupted
/// partial writes are replaced only at the current chunk boundary.
pub fn write_chunk(state: &Path, id: Uuid, offset: u64, data: &[u8]) -> Result<()> {
    let (dir, _lock) = locked(state, id, "import")?;
    ensure!(!dir.join("aborted").exists(), "move import was aborted");
    let i = load::<Import>(&dir, "import")?.context("move import not started")?;
    check_chunk(&i.manifest, offset, data)?;
    if dir.join("committed").exists() {
        return Ok(());
    }
    let mut f = OpenOptions::new().read(true).write(true).open(dir.join("payload"))?;
    let len = f.metadata()?.len();
    let end = offset + data.len() as u64;
    ensure!(offset <= len, "out-of-order move chunk");
    if end <= len {
        ensure!(chunk(&mut f, i.manifest.bytes, offset)? == data, "conflicting duplicate move chunk");
    } else {
        f.set_len(offset)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
    }
    f.sync_all()?;
    Ok(())
}

fn extract_file(dir: &Path, name: &str, payload: &mut File, len: u64) -> Result<PathBuf> {
    let path = dir.join(name);
    let mut out = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&path)?;
    copy_exact(payload, &mut out, len)?;
    Ok(path)
}

fn restore_repo(root: &Path, repo: &Repo, target: &Target, bundle: &Path, patches: [&Path; 2]) -> Result<()> {
    git(root, &["check-ref-format", &format!("refs/heads/{}", repo.branch)])?;
    let format = if repo.head.len() == 64 { "--object-format=sha256" } else { "--object-format=sha1" };
    git(root, &["init", "--template=", format, "."])?;
    let bundle = path_str(bundle)?;
    ensure!(
        text(root, &["bundle", "list-heads", bundle])? == format!("{} HEAD", repo.head),
        "bundle does not advertise the expected commit"
    );
    git(root, &["bundle", "verify", bundle])?;
    git(root, &["bundle", "unbundle", bundle])?;
    tree(root, &repo.head)?;
    git(root, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", repo.branch)])?;
    // Only the brand-new unpublished checkout. Nothing in the target's
    // known repo (including Git refs and the object database) is modified.
    git(root, &["reset", "--hard", &repo.head])?;
    let (_, url) = remote(target.repo_root.as_deref().context("missing target repository")?)?;
    // Don't copy HTTP usernames/tokens or SSH passwords into the new clone.
    let url = strip_remote_credentials(&url).context("invalid target remote")?;
    git(root, &["remote", "add", "origin", &url])?;
    for (patch, indexed) in patches.into_iter().zip([true, false]) {
        if patch.metadata()?.len() == 0 {
            continue;
        }
        let bytes = fs::read(patch)?;
        ensure!(
            !bytes.split(|b| *b == b'\n').any(|line| {
                line == b"GIT binary patch"
                    || line.starts_with(b"Binary files ")
                    || ((line.starts_with(b"index ")
                        || line.starts_with(b"old mode ")
                        || line.starts_with(b"new mode ")
                        || line.starts_with(b"new file mode ")
                        || line.starts_with(b"deleted file mode "))
                        && (line.ends_with(b" 120000") || line.ends_with(b" 160000")))
            }),
            "patch includes a binary change, Git symlink, or gitlink"
        );
        let apply = ["apply", "--whitespace=nowarn", "--no-ignore-whitespace"];
        let index = if indexed { &["--index"][..] } else { &[] };
        let path = ["--", path_str(patch)?];
        git(root, &[&apply, index, &["--check"], &path].concat())?;
        git(root, &[&apply, index, &path].concat())?;
    }
    Ok(())
}

fn output_entry(root: &File, entry: &Entry, from: &mut File) -> Result<()> {
    let mut dir = root.try_clone()?;
    validate_path(&entry.path)?;
    let mut parts = entry.path.split('/').peekable();
    while let Some(p) = parts.next() {
        if parts.peek().is_some() || entry.kind == Kind::Directory {
            mkdir_at(&dir, p)?;
            dir = open_at(&dir, OsStr::new(p), libc::O_RDONLY | libc::O_DIRECTORY)?;
        } else {
            let mut out = open_at(&dir, OsStr::new(p), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
                .with_context(|| format!("destination conflicts with {:?}", entry.path))?;
            copy_exact(from, &mut out, entry.len)?;
            out.set_permissions(fs::Permissions::from_mode(entry.mode))?;
            out.sync_all()?;
            dir.sync_all()?;
            return Ok(());
        }
    }
    Ok(())
}

fn sync_checkout(path: &Path) -> Result<()> {
    for item in fs::read_dir(path)? {
        let item = item?;
        let ty = item.file_type()?;
        if ty.is_dir() {
            sync_checkout(&item.path())?;
        } else {
            ensure!(ty.is_file(), "unexpected symlink or special file in prepared checkout");
            File::open(item.path())?.sync_all()?;
        }
    }
    sync_dir(path)
}

fn native_cleanup(dir: &Path, id: Uuid, rollback: bool) -> Result<()> {
    if let Some(intent) = load::<NativeIntent>(dir, "native")? {
        let opened = project(&intent.config_dir, &intent.cwd, false);
        match opened {
            Ok(native) => native_remove(&native, id, intent.resume, rollback)?,
            Err(e) if e.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct NativeIntent {
    config_dir: PathBuf,
    cwd: PathBuf,
    resume: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    hash: String,
    mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileChange {
    path: String,
    before: Option<Fingerprint>,
    after: Option<Fingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyReceipt {
    root: PathBuf,
    identity: (u64, u64),
    files: Vec<FileChange>,
    directories: Vec<String>,
    index: Option<PathBuf>,
    #[serde(default)]
    index_identity: Option<(u64, u64)>,
    #[serde(default)]
    head: Option<String>,
    index_before: Option<Fingerprint>,
    index_after: Option<Fingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CloneReceipt {
    cwd: PathBuf,
    identity: (u64, u64),
    digest: String,
}

fn checkout_digest(root: &Path) -> Result<String> {
    fn walk(root: &File, prefix: &str, entries: &mut BTreeMap<String, String>, bytes: &mut u64) -> Result<()> {
        for name in names(root, MAX_ENTRIES)? {
            ensure!(
                entries.len() < MAX_ENTRIES && prefix.split('/').count() <= 40,
                "Folder: clone is too large to protect transactionally"
            );
            let path = format!("{prefix}/{name}");
            let mut file = open_at(root, OsStr::new(&name), libc::O_RDONLY)?;
            let meta = file.metadata()?;
            if meta.is_dir() {
                entries.insert(path.clone(), format!("dir:{}", meta.mode() & 0o777));
                walk(&file, &path, entries, bytes)?;
            } else {
                *bytes += meta.len();
                ensure!(*bytes <= MAX_BYTES * 2, "Folder: clone is too large to protect transactionally");
                let state = fingerprint(&mut file)?;
                entries.insert(path, format!("{}:{}", state.mode, state.hash));
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    walk(&open_root(root)?, "", &mut entries, &mut 0)?;
    Ok(c6_core::sha256_hex(&serde_json::to_vec(&entries)?))
}

fn publish_clone(dir: &Path, target: &Target, work: &Path) -> Result<PathBuf> {
    let selected = target.selected.as_ref().context("missing clone selection")?;
    let receipt = if let Some(receipt) = load::<CloneReceipt>(dir, "clone-publish")? {
        receipt
    } else {
        let meta = open_root(work)?.metadata()?;
        let receipt = CloneReceipt { cwd: selected.cwd.clone(), identity: (meta.dev(), meta.ino()), digest: checkout_digest(work)? };
        store(dir, "clone-publish", &receipt)?;
        receipt
    };
    if receipt.cwd.exists() {
        let meta = open_root(&receipt.cwd)?.metadata()?;
        ensure!(
            (meta.dev(), meta.ino()) == receipt.identity && checkout_digest(&receipt.cwd)? == receipt.digest,
            "Folder: clone destination changed; refusing to overwrite it"
        );
    } else {
        // A replay may have rebuilt the private workspace after a crash before
        // publication. Freeze that replacement inode before the no-replace CAS.
        let meta = open_root(work)?.metadata()?;
        let receipt = CloneReceipt { identity: (meta.dev(), meta.ino()), digest: checkout_digest(work)?, ..receipt };
        store(dir, "clone-publish", &receipt)?;
        publish_seed(work, &receipt.cwd).context("Folder: clone destination already exists or cannot be published")?;
        sync_dir(receipt.cwd.parent().unwrap())?;
        sync_dir(dir)?;
    }
    Ok(selected.cwd.clone())
}

fn fingerprint(file: &mut File) -> Result<Fingerprint> {
    let meta = file.metadata()?;
    ensure!(meta.is_file() && meta.len() <= MAX_BYTES, "Folder: destination contains an unsupported file");
    let mut bytes = Vec::new();
    (&mut *file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(stamp(&meta) == stamp(&file.metadata()?) && bytes.len() as u64 == meta.len(), "Folder: destination file changed while reading");
    Ok(Fingerprint { hash: c6_core::sha256_hex(&bytes), mode: meta.mode() & 0o777 })
}

fn file_state(root: &File, path: &str) -> Result<Option<Fingerprint>> {
    match open_relative(root, path) {
        Ok(mut file) => Ok(Some(fingerprint(&mut file)?)),
        Err(e) if e.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => Ok(None),
        Err(e) => Err(e.context(format!("Folder: cannot safely inspect {path:?}"))),
    }
}

fn path_state(path: &Path) -> Result<Option<Fingerprint>> {
    let parent = open_root(path.parent().context("file has no parent")?)?;
    file_state(&parent, path.file_name().and_then(OsStr::to_str).context("non-UTF-8 file name")?)
}

fn parent_for(root: &File, path: &str, create: bool) -> Result<(File, String)> {
    validate_path(path)?;
    let mut parent = root.try_clone()?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return Ok((parent, part.to_owned()));
        }
        if create {
            mkdir_at(&parent, part)?;
        }
        parent = open_at(&parent, OsStr::new(part), libc::O_RDONLY | libc::O_DIRECTORY)?;
    }
    bail!("empty move path")
}

fn unlink_workspace_file(parent: &File, name: &str) -> Result<()> {
    let name = CString::new(name)?;
    unlink_at(parent, &name)?;
    parent.sync_all()?;
    Ok(())
}

/// Never expose a partially written destination file. Temporary names contain
/// a fresh UUID; a crash before rename can leave an inert temporary file, but
/// never a truncated user file. All overwritten contents have durable backups.
fn replace_file(root: &File, path: &str, source: Option<(&Path, u32)>) -> Result<()> {
    let (parent, name) = parent_for(root, path, source.is_some())?;
    let Some((source, mode)) = source else { return unlink_workspace_file(&parent, &name) };
    let temp = format!(".c6-move-{}.tmp", Uuid::now_v7());
    let mut output = open_at(&parent, OsStr::new(&temp), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
    let result = (|| {
        let mut input = File::open(source)?;
        ensure!(io::copy(&mut input, &mut output)? <= MAX_BYTES, "oversized prepared move file");
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        output.sync_all()?;
        let from = CString::new(temp.as_str())?;
        let to = CString::new(name.as_str())?;
        ensure!(
            unsafe { libc::renameat(parent.as_raw_fd(), from.as_ptr(), parent.as_raw_fd(), to.as_ptr()) } == 0,
            "Folder: cannot publish transferred file: {}",
            io::Error::last_os_error()
        );
        parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlink_workspace_file(&parent, &temp);
    }
    result
}

fn workspace_files(root: &File, prefix: &str, files: &mut BTreeSet<String>, directories: &mut BTreeSet<String>) -> Result<()> {
    for name in names(root, MAX_ENTRIES)? {
        if name == ".git" {
            continue;
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        validate_path(&path)?;
        let file = open_at(root, OsStr::new(&name), libc::O_RDONLY)?;
        let meta = file.metadata()?;
        ensure!(files.len() + directories.len() < MAX_ENTRIES, "Folder: too many checkout files");
        if meta.is_dir() {
            directories.insert(path.clone());
            workspace_files(&file, &path, files, directories)?;
        } else {
            ensure!(meta.is_file(), "Folder: checkout contains a special file");
            files.insert(path);
        }
    }
    Ok(())
}

fn owned_branch(root: &Path, choice: &WorktreeChoice, id: Uuid, head: &str) -> bool {
    let reference = format!("refs/heads/{}", choice.branch);
    text(root, &["rev-parse", "--verify", &reference]).ok().as_deref() == Some(head)
        && text(root, &["reflog", "show", "-1", "--format=%gs", &reference]).ok().as_deref() == Some(&format!("carbon6 move {id}"))
}

fn verify_linked(root: &Path, choice: &WorktreeChoice) -> Result<()> {
    ensure!(repo_root(&choice.path)? == choice.path, "Worktree: move destination was replaced");
    ensure!(
        text(&choice.path, &["symbolic-ref", "HEAD"])? == format!("refs/heads/{}", choice.branch),
        "Worktree: destination branch changed"
    );
    ensure!(
        text(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?
            == text(&choice.path, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?,
        "Worktree: repository identity changed"
    );
    let expected = format!("worktree {}", choice.path.display());
    let registered = git(root, &["worktree", "list", "--porcelain", "-z"])?;
    ensure!(registered.split(|b| *b == 0).any(|entry| entry == expected.as_bytes()), "Worktree: destination registration changed");
    Ok(())
}

fn linked_identity(choice: &WorktreeChoice) -> Result<((u64, u64), (u64, u64))> {
    let top = open_root(&choice.path)?.metadata()?;
    let admin = PathBuf::from(text(&choice.path, &["rev-parse", "--absolute-git-dir"])?).canonicalize()?;
    let admin = open_root(&admin)?.metadata()?;
    Ok(((top.dev(), top.ino()), (admin.dev(), admin.ino())))
}

fn verify_owned_linked(dir: &Path, choice: &WorktreeChoice) -> Result<()> {
    let witness = dir.join("worktree-git");
    if let Ok(witness) = fs::symlink_metadata(&witness) {
        let current = fs::symlink_metadata(choice.path.join(".git"))?;
        ensure!(
            current.is_file() && (current.dev(), current.ino()) == (witness.dev(), witness.ino()),
            "Worktree: checkout Git pointer was replaced; refusing to modify it"
        );
    }
    if let Some(identity) = load::<((u64, u64), (u64, u64))>(dir, "worktree-made")? {
        ensure!(linked_identity(choice)? == identity, "Worktree: checkout or registration was replaced; refusing to modify it");
    }
    Ok(())
}

fn make_linked(dir: &Path, target: &Target, repo: &Repo, id: Uuid) -> Result<PathBuf> {
    let selected = target.selected.as_ref().context("missing selected destination")?;
    let choice = selected.worktree.as_ref().context("missing worktree choice")?;
    let root = target.repo_root.as_deref().context("missing repository")?;
    // The validated bundle introduces only immutable objects, not a ref or a
    // checkout mutation. Source commits need not have been pushed anywhere.
    git(root, &["bundle", "unbundle", path_str(&dir.join("unpack/repo.bundle"))?])?;
    let reference = format!("refs/heads/{}", choice.branch);
    if !owned_branch(root, choice, id, &repo.head) {
        git(
            root,
            &["update-ref", "--create-reflog", "-m", &format!("carbon6 move {id}"), &reference, &repo.head, &"0".repeat(repo.head.len())],
        )
        .context("Worktree: destination branch already exists or could not be reserved")?;
    }
    if !choice.path.exists() {
        git(root, &["worktree", "add", "--no-checkout", "--", path_str(&choice.path)?, &choice.branch])
            .context("Worktree: cannot create destination worktree")?;
    }
    verify_linked(root, choice)?;
    verify_owned_linked(dir, choice)?;
    // Keep the original Git-pointer inode alive across crashes/replacements.
    // A recreated checkout cannot accidentally pass an inode-reuse check.
    let witness = dir.join("worktree-git");
    if !witness.exists() {
        fs::hard_link(choice.path.join(".git"), &witness)?;
        File::open(&witness)?.sync_all()?;
        sync_dir(dir)?;
    }
    store(dir, "worktree-made", &linked_identity(choice)?)?;
    Ok(choice.path.clone())
}

fn index_command(root: &Path, index: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = command(root, args);
    cmd.env("GIT_INDEX_FILE", index);
    git_to(cmd, MAX_JSON, &mut io::sink())?;
    Ok(())
}

/// Git's conventional index lock is an atomic symlink to our private receipt.
/// Replays recognize precisely that link; unrelated lock files are never
/// removed. Index writes use a separate atomic replacement while holding it.
fn index_lock(dir: &Path, index: &Path) -> Result<PathBuf> {
    let lock = index.with_file_name("index.lock");
    let marker = dir.join("apply");
    match std::os::unix::fs::symlink(&marker, &lock) {
        Ok(()) => sync_dir(lock.parent().unwrap())?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(fs::read_link(&lock).ok().as_ref() == Some(&marker), "Folder: destination index is locked by another Git operation");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(lock)
}

fn unlock_index(dir: &Path, lock: &Path) -> Result<()> {
    ensure!(fs::read_link(lock)? == dir.join("apply"), "Folder: destination index lock changed");
    fs::remove_file(lock)?;
    sync_dir(lock.parent().unwrap())
}

fn prepare_apply(dir: &Path, m: &Manifest, target: &Target, work: &Path) -> Result<ApplyReceipt> {
    if let Some(receipt) = load(dir, "apply")? {
        return Ok(receipt);
    }
    let selected = target.selected.as_ref().context("missing selected destination")?;
    let is_new = selected.worktree.is_some();
    let root = if let Some(repo) = &m.source.repo {
        if is_new { make_linked(dir, target, repo, m.move_id)? } else { target.repo_root.clone().unwrap() }
    } else {
        selected.cwd.clone()
    };
    ensure!(is_new || (!root.starts_with(dir) && !dir.starts_with(&root)), "Folder: destination overlaps move storage");
    let destination = open_root(&root)?;
    let meta = destination.metadata()?;
    let stage = open_root(work)?;
    let mut paths = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut index = None;
    let mut index_identity = None;
    let mut index_before = None;
    let mut index_after = None;
    if let Some(repo) = &m.source.repo {
        if !is_new {
            clean_checkout(&root)?;
            ensure!(text(&root, &["rev-parse", "HEAD"])? == repo.head, "Folder: destination commit changed since preflight");
        }
        if is_new {
            workspace_files(&stage, "", &mut paths, &mut directories)?;
        } else {
            for args in [
                vec!["diff", "--name-only", "--no-renames", "-z", "--cached", "HEAD", "--"],
                vec!["diff", "--name-only", "--no-renames", "-z", "--"],
            ] {
                for path in git(work, &args)?.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                    paths.insert(std::str::from_utf8(path)?.to_owned());
                }
            }
            paths.extend(m.copied_files().map(str::to_owned));
            directories.extend(m.entries.iter().filter(|e| e.kind == Kind::Directory).map(|e| e.path.clone()));
        }
        let index_path = PathBuf::from(text(&root, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?);
        let admin = open_root(index_path.parent().unwrap())?.metadata()?;
        index_identity = Some((admin.dev(), admin.ino()));
        index_before = path_state(&index_path)?;
        if index_before.is_some() {
            fs::copy(&index_path, dir.join("index-before"))?;
            File::open(dir.join("index-before"))?.sync_all()?;
            ensure!(path_state(&dir.join("index-before"))? == index_before, "Folder: destination index changed during backup");
        }
        let proposed = dir.join("index-after");
        missing_ok(fs::remove_file(&proposed))?;
        missing_ok(fs::remove_file(dir.join("index-after.lock")))?;
        index_command(&root, &proposed, &["read-tree", &repo.head])?;
        let staged = dir.join("unpack/staged.patch");
        if staged.metadata()?.len() != 0 {
            index_command(&root, &proposed, &["apply", "--cached", "--whitespace=nowarn", "--", path_str(&staged)?])?;
        }
        File::open(&proposed)?.sync_all()?;
        index_after = path_state(&proposed)?;
        index = Some(index_path);
    } else {
        workspace_files(&stage, "", &mut paths, &mut directories)?;
    }
    private_dir(&dir.join("backup"))?;
    let mut files = Vec::new();
    let mut backup_bytes = 0;
    for path in paths {
        validate_path(&path)?;
        let before = file_state(&destination, &path)?;
        ensure!(m.source.repo.is_some() || before.is_none(), "Folder: scratch copy conflicts with existing file {path:?}");
        let after = file_state(&stage, &path)?;
        if before == after {
            continue;
        }
        if let Some(before_state) = &before {
            let input = open_relative(&destination, &path)?;
            let backup = dir.join("backup").join(files.len().to_string());
            let mut output = File::create(&backup)?;
            backup_bytes += io::copy(&mut input.take(MAX_BYTES + 1), &mut output)?;
            ensure!(backup_bytes <= MAX_BYTES, "Folder: destination backup exceeds transfer limit");
            output.set_permissions(fs::Permissions::from_mode(before_state.mode))?;
            output.sync_all()?;
            ensure!(path_state(&backup)? == before, "Folder: destination file changed during backup");
            ensure!(file_state(&destination, &path)? == before, "Folder: destination changed while backing up {path:?}");
        }
        files.push(FileChange { path, before, after });
    }
    sync_dir(&dir.join("backup"))?;
    let receipt = ApplyReceipt {
        root,
        identity: (meta.dev(), meta.ino()),
        files,
        directories: directories.into_iter().collect(),
        index,
        index_identity,
        head: m.source.repo.as_ref().map(|repo| repo.head.clone()),
        index_before,
        index_after,
    };
    store(dir, "apply", &receipt)?;
    Ok(receipt)
}

fn apply_selected(dir: &Path, receipt: &ApplyReceipt, work: &Path, rollback: bool) -> Result<()> {
    let root = open_root(&receipt.root)?;
    let meta = root.metadata()?;
    ensure!((meta.dev(), meta.ino()) == receipt.identity, "Folder: prepared destination was replaced; leaving its files untouched");
    if let Some(head) = &receipt.head {
        ensure!(
            text(&receipt.root, &["rev-parse", "HEAD"])? == *head,
            "Folder: destination HEAD changed; refusing to overwrite its checkout"
        );
    }
    if let (Some(index), Some(identity)) = (&receipt.index, receipt.index_identity) {
        let admin = open_root(index.parent().unwrap())?.metadata()?;
        ensure!((admin.dev(), admin.ino()) == identity, "Folder: destination Git metadata was replaced; refusing to overwrite it");
    }
    let lock = receipt.index.as_deref().map(|index| index_lock(dir, index)).transpose()?;
    let result = (|| {
        if let Some(index) = &receipt.index {
            let current = path_state(index)?;
            ensure!(
                current == receipt.index_before || current == receipt.index_after,
                "Folder: destination index changed; refusing to overwrite it"
            );
        }
        // Validate the complete set first. In particular, abort must not
        // restore half the checkout and only then discover an external edit.
        for change in &receipt.files {
            let current = file_state(&root, &change.path)?;
            ensure!(
                current == change.before || current == change.after,
                "Folder: destination file {:?} changed; refusing to overwrite it",
                change.path
            );
        }
        if !rollback {
            for path in &receipt.directories {
                let (parent, name) = parent_for(&root, path, true)?;
                mkdir_at(&parent, &name)?;
            }
        }
        for (n, change) in receipt.files.iter().enumerate() {
            let desired = if rollback { &change.before } else { &change.after };
            let current = file_state(&root, &change.path)?;
            ensure!(
                current == change.before || current == change.after,
                "Folder: destination file {:?} changed; refusing to overwrite it",
                change.path
            );
            if current == *desired {
                continue;
            }
            let source =
                desired.as_ref().map(|f| (if rollback { dir.join("backup").join(n.to_string()) } else { work.join(&change.path) }, f.mode));
            replace_file(&root, &change.path, source.as_ref().map(|(p, mode)| (p.as_path(), *mode)))?;
        }
        if let Some(index) = &receipt.index {
            let desired = if rollback { &receipt.index_before } else { &receipt.index_after };
            let current = path_state(index)?;
            ensure!(
                current == receipt.index_before || current == receipt.index_after,
                "Folder: destination index changed; refusing to overwrite it"
            );
            if current != *desired {
                let parent = open_root(index.parent().unwrap())?;
                let name = index.file_name().unwrap().to_str().context("non-UTF-8 index path")?;
                let source = desired.as_ref().map(|f| (dir.join(if rollback { "index-before" } else { "index-after" }), f.mode));
                replace_file(&parent, name, source.as_ref().map(|(p, mode)| (p.as_path(), *mode)))?;
            }
        }
        root.sync_all()?;
        Ok(())
    })();
    if let Some(lock) = lock {
        unlock_index(dir, &lock)?;
    }
    result
}

fn rollback_selected(dir: &Path, id: Uuid) -> Result<()> {
    let Some(import) = load::<Import>(dir, "import")? else { return Ok(()) };
    let Some(selected) = &import.target.selected else { return Ok(()) };
    if selected.clone {
        if let Some(receipt) = load::<CloneReceipt>(dir, "clone-publish")? {
            match fs::symlink_metadata(&receipt.cwd) {
                Ok(meta) => {
                    ensure!(
                        (meta.dev(), meta.ino()) == receipt.identity && meta.is_dir() && checkout_digest(&receipt.cwd)? == receipt.digest,
                        "Folder: cloned destination changed; refusing to remove it"
                    );
                    fs::remove_dir_all(&receipt.cwd)?;
                    sync_dir(receipt.cwd.parent().unwrap())?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        return Ok(());
    }
    if let (Some(choice), Some(repo), Some(root)) = (&selected.worktree, &import.manifest.source.repo, &import.target.repo_root)
        && choice.path.exists()
    {
        ensure!(owned_branch(root, choice, id, &repo.head), "Worktree: destination branch changed; refusing cleanup");
        verify_linked(root, choice)?;
        verify_owned_linked(dir, choice)?;
    }
    if (selected.worktree.is_none() || selected.worktree.as_ref().is_some_and(|w| w.path.exists()))
        && let Some(receipt) = load::<ApplyReceipt>(dir, "apply")?
    {
        apply_selected(dir, &receipt, &dir.join("workspace"), true)?;
    }
    if let (Some(choice), Some(repo), Some(root)) = (&selected.worktree, &import.manifest.source.repo, &import.target.repo_root) {
        if choice.path.exists() {
            ensure!(owned_branch(root, choice, id, &repo.head), "Worktree: destination branch changed; refusing cleanup");
            verify_linked(root, choice)?;
            let mut files = BTreeSet::new();
            let mut directories = BTreeSet::new();
            workspace_files(&open_root(&choice.path)?, "", &mut files, &mut directories)?;
            ensure!(files.is_empty(), "Worktree: destination contains external files; refusing to remove it");
            let allowed =
                load::<ApplyReceipt>(dir, "apply")?.map(|r| r.directories.into_iter().collect::<BTreeSet<_>>()).unwrap_or_default();
            ensure!(directories.is_subset(&allowed), "Worktree: destination contains external folders; refusing to remove it");
            git(root, &["worktree", "remove", "--force", "--", path_str(&choice.path)?])?;
        }
        if owned_branch(root, choice, id, &repo.head) {
            let branch = format!("branch refs/heads/{}", choice.branch);
            ensure!(
                !git(root, &["worktree", "list", "--porcelain", "-z"])?.split(|b| *b == 0).any(|entry| entry == branch.as_bytes()),
                "Worktree: transferred branch is in use by another checkout; leaving it untouched"
            );
            git(root, &["update-ref", "-d", &format!("refs/heads/{}", choice.branch), &repo.head])?;
        }
    }
    Ok(())
}

fn selected_place(target: &Target, receipt: &ApplyReceipt) -> Result<(PathBuf, Option<PathBuf>, String)> {
    let selected = target.selected.as_ref().context("missing selection")?;
    if target.repo_root.is_none() {
        return Ok((selected.cwd.clone(), None, String::new()));
    }
    let cwd = if selected.worktree.is_some() {
        receipt.root.join(selected.cwd.strip_prefix(target.repo_root.as_ref().unwrap())?)
    } else {
        selected.cwd.clone()
    };
    open_root(&cwd).context("Folder: selected relative folder does not exist in transferred checkout")?;
    if receipt.root.join(".git").is_file() {
        let common = PathBuf::from(text(&receipt.root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?).canonicalize()?;
        let primary = common.parent().context("Worktree: repository has no primary checkout")?.to_path_buf();
        ensure!(repo_root(&primary)? == primary, "Worktree: bare repository worktrees are not supported");
        let branch = text(&receipt.root, &["symbolic-ref", "--short", "HEAD"]).context("Worktree: choose a checkout with a branch")?;
        Ok((cwd, Some(primary), branch))
    } else {
        Ok((cwd, Some(receipt.root.clone()), String::new()))
    }
}

/// Prepare once but do not start an actor. A partially applied checkout belongs
/// only to this UUID and is rebuilt on retry. An existing unrelated transcript
/// or checkout in the known repo is never used as a restore destination.
pub fn finalize_import(state: &Path, id: Uuid, config_dir: Option<&Path>) -> Result<Prepared> {
    let (dir, _lock) = locked(state, id, "import")?;
    ensure!(!dir.join("aborted").exists(), "move import was aborted");
    if let Some(done) = load(&dir, "prepared")? {
        return Ok(done);
    }
    let Import { manifest: m, target } = load(&dir, "import")?.context("move import not started")?;
    validate_target(&m.source, &target)?;
    let mut payload = File::open(dir.join("payload"))?;
    ensure!(payload.metadata()?.len() == m.bytes, "move payload is incomplete");
    for offset in (0..m.bytes).step_by(CHUNK_BYTES) {
        check_chunk(&m, offset, &chunk(&mut payload, m.bytes, offset)?)?;
    }
    let work = dir.join("workspace");
    let stage = dir.join("unpack");
    let metadata_dir = dir.join("metadata");
    native_cleanup(&dir, id, true)?;
    missing_ok(fs::remove_dir_all(&work))?;
    missing_ok(fs::remove_dir_all(&stage))?;
    missing_ok(fs::remove_dir_all(&metadata_dir))?;
    private_dir(&work)?;
    private_dir(&stage)?;
    private_dir(&metadata_dir)?;
    let result = (|| {
        let mut offset = 0;
        let mut blobs = BTreeMap::new();
        let mut workspace = Vec::new();
        let mut transcript = None;
        for e in &m.entries {
            match e.kind {
                Kind::Bundle | Kind::Staged | Kind::Unstaged => {
                    let name = match e.kind {
                        Kind::Bundle => "repo.bundle",
                        Kind::Staged => "staged.patch",
                        _ => "unstaged.patch",
                    };
                    payload.seek(SeekFrom::Start(offset))?;
                    blobs.insert(name, extract_file(&stage, name, &mut payload, e.len)?);
                }
                Kind::Transcript => transcript = Some((offset, e.len)),
                Kind::Metadata => {
                    payload.seek(SeekFrom::Start(offset))?;
                    let path = extract_file(&metadata_dir, &e.path, &mut payload, e.len)?;
                    File::open(path)?.sync_all()?;
                }
                _ => workspace.push((offset, e)),
            }
            offset += e.len;
        }
        if let Some(repo) = &m.source.repo {
            restore_repo(&work, repo, &target, &blobs["repo.bundle"], [&blobs["staged.patch"], &blobs["unstaged.patch"]])?;
            sync_checkout(&work)?;
        }
        let root = open_root(&work)?;
        let mut mode_later = Vec::new();
        for (offset, e) in workspace {
            payload.seek(SeekFrom::Start(offset))?;
            output_entry(&root, e, &mut payload)?;
            if e.kind == Kind::Directory {
                mode_later.push(e);
            }
        }
        mode_later.sort_unstable_by_key(|e| std::cmp::Reverse(e.path.matches('/').count()));
        for e in mode_later {
            let dir = open_relative(&root, &e.path)?;
            dir.set_permissions(fs::Permissions::from_mode(e.mode & 0o777))?;
            dir.sync_all()?;
        }
        let uncommitted_files = if m.source.repo.is_some() { uncommitted_files(&work)? } else { 0 };
        let (cwd, repo_root, worktree_branch) = if target.selected.as_ref().is_some_and(|s| s.clone) {
            let cwd = publish_clone(&dir, &target, &work)?;
            (cwd.clone(), Some(cwd), String::new())
        } else if target.selected.is_some() {
            let receipt = prepare_apply(&dir, &m, &target, &work)?;
            apply_selected(&dir, &receipt, &work, false)?;
            selected_place(&target, &receipt)?
        } else {
            (
                m.source.repo.as_ref().map_or_else(|| work.clone(), |r| work.join(&r.subdir)),
                m.source.repo.as_ref().map(|_| work.clone()),
                String::new(),
            )
        };
        open_root(&cwd).context("source relative cwd does not exist in prepared checkout")?;
        if let Some((offset, len)) = transcript {
            let resume = m.source.native_resume.context("missing native conversation ID")?;
            let config_dir = native_config(config_dir)?;
            let project = project(&config_dir, &cwd, true)?;
            store(&dir, "native", &NativeIntent { config_dir, cwd: cwd.clone(), resume })?;
            payload.seek(SeekFrom::Start(offset))?;
            native_install(&project, id, resume, &mut payload, len)?;
        }
        let done = Prepared {
            cwd,
            repo_root,
            branch: m.source.repo.as_ref().map(|r| r.branch.clone()),
            worktree_branch,
            native_resume: m.source.native_resume,
            metadata_dir: metadata_dir.clone(),
            uncommitted_files,
        };
        fs::remove_dir_all(&stage)?;
        if work.exists() {
            sync_dir(&work)?;
        }
        sync_dir(&metadata_dir)?;
        store(&dir, "prepared", &done)?;
        Ok(done)
    })();
    // If the receipt exists, a directory fsync failure can leave commit status
    // uncertain. Preserve the workspace; retries inspect the same receipt.
    if result.is_err() && !dir.join("prepared").exists() {
        native_cleanup(&dir, id, true)?;
        rollback_selected(&dir, id)?;
        // The old baseline is valid again. A retry needs a new receipt if its
        // move-owned linked worktree was removed during rollback.
        missing_ok(fs::remove_file(dir.join("apply")))?;
        missing_ok(fs::remove_dir_all(&work))?;
        missing_ok(fs::remove_dir_all(&stage))?;
        missing_ok(fs::remove_dir_all(&metadata_dir))?;
    }
    result
}

/// After ownership CAS. Durable tombstone comes first, so late aborts refuse.
pub fn commit_import(state: &Path, id: Uuid) -> Result<()> {
    let (dir, _lock) = locked(state, id, "import")?;
    ensure!(!dir.join("aborted").exists() && dir.join("prepared").exists(), "move is not prepared");
    store(&dir, "committed", &true)?;
    native_cleanup(&dir, id, false)?;
    missing_ok(fs::remove_dir_all(dir.join("backup")))?;
    missing_ok(fs::remove_file(dir.join("index-before")))?;
    missing_ok(fs::remove_file(dir.join("index-after")))?;
    missing_ok(fs::remove_file(dir.join("worktree-git")))?;
    missing_ok(fs::remove_file(dir.join("payload")))?;
    missing_ok(fs::remove_dir_all(dir.join("metadata")))?;
    sync_dir(&dir)
}

/// Only after definitive head abort, never on an ambiguous CAS response.
pub fn abort_import(state: &Path, id: Uuid) -> Result<()> {
    let (dir, _lock) = locked(state, id, "import")?;
    ensure!(!dir.join("committed").exists(), "move is already committed");
    store(&dir, "aborted", &true)?;
    native_cleanup(&dir, id, true)?;
    rollback_selected(&dir, id)?;
    missing_ok(fs::remove_dir_all(dir.join("workspace")))?;
    missing_ok(fs::remove_dir_all(dir.join("unpack")))?;
    missing_ok(fs::remove_dir_all(dir.join("metadata")))?;
    missing_ok(fs::remove_file(dir.join("payload")))?;
    sync_dir(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(root: &Path, name: &str) -> PathBuf {
        let p = root.join(name);
        fs::create_dir(&p).unwrap();
        p
    }

    fn replay(from: &Path, to: &Path, manifest: &Manifest, target: &Target) {
        begin_import(to, manifest, target).unwrap();
        begin_import(to, manifest, target).unwrap();
        for offset in (0..manifest.bytes).step_by(CHUNK_BYTES) {
            let data = read_chunk(from, manifest.move_id, offset).unwrap();
            write_chunk(to, manifest.move_id, offset, &data).unwrap();
            write_chunk(to, manifest.move_id, offset, &data).unwrap();
        }
    }

    #[test]
    fn scratch_retries_and_rollback_keep_source() {
        let t = tempfile::tempdir().unwrap();
        let from = folder(t.path(), "from");
        let to = folder(t.path(), "to");
        let cwd = folder(t.path(), "source");
        let nested = folder(&cwd, "nested");
        folder(&nested, "empty");
        fs::write(nested.join("blob"), vec![42; CHUNK_BYTES + 7]).unwrap();
        fs::set_permissions(nested.join("blob"), fs::Permissions::from_mode(0o751)).unwrap();
        let source = describe(&cwd, false, None).unwrap();
        let id = Uuid::now_v7();
        let metadata = [("spec.pb", &b"spec"[..]), ("events.pb", &b"events"[..])];
        let m = prepare_export(&from, id, "session", &cwd, &source, None, &metadata).unwrap();
        assert_eq!(prepare_export(&from, id, "session", &cwd, &source, None, &metadata).unwrap(), m);
        let target = preflight(&source, &[]).unwrap();
        begin_import(&to, &m, &target).unwrap();
        assert!(finalize_import(&to, id, None).is_err());
        assert!(write_chunk(&to, id, 0, &vec![8; CHUNK_BYTES]).is_err());
        let (dst, lock) = locked(&to, id, "import").unwrap();
        fs::write(dst.join("payload"), vec![42; 17]).unwrap(); // torn first write
        drop(lock);
        replay(&from, &to, &m, &target);
        let prepared = finalize_import(&to, id, None).unwrap();
        assert_eq!(finalize_import(&to, id, None).unwrap(), prepared);
        assert_eq!(prepared.uncommitted_files, 0);
        assert!(prepared.cwd.join("nested/empty").is_dir());
        assert_eq!(fs::read(prepared.metadata_dir.join("events.pb")).unwrap(), b"events");
        assert_eq!(fs::read(prepared.cwd.join("nested/blob")).unwrap(), vec![42; CHUNK_BYTES + 7]);
        assert_eq!(fs::metadata(prepared.cwd.join("nested/blob")).unwrap().mode() & 0o777, 0o751);
        abort_import(&to, id).unwrap();
        abort_import(&to, id).unwrap();
        assert!(!prepared.cwd.exists() && cwd.join("nested/blob").exists());
        assert!(begin_import(&to, &m, &target).is_err());
        finish_export(&from, id).unwrap();
        assert!(read_chunk(&from, id, 0).is_err());
    }

    #[test]
    fn symlinks_and_traversal_refuse() {
        let t = tempfile::tempdir().unwrap();
        let from = folder(t.path(), "from");
        let cwd = folder(t.path(), "source");
        let outside = t.path().join("outside");
        fs::write(&outside, "do not read").unwrap();
        std::os::unix::fs::symlink(&outside, cwd.join("link")).unwrap();
        let source = describe(&cwd, false, None).unwrap();
        assert!(prepare_export(&from, Uuid::now_v7(), "session", &cwd, &source, None, &[]).is_err());
        assert!(validate_path("dir/../../outside").is_err());
        assert!(validate_path("dir/.GiT/ref").is_err());
    }

    #[test]
    fn remote_equivalence_does_not_drop_nondefault_ports() {
        assert_eq!(remote_key("git@EXAMPLE.invalid:team/repo.git"), remote_key("https://example.invalid:443/team/repo"));
        assert_eq!(remote_key("git@GitHub.com:Carbon6-Dev/Falcon.GIT/"), remote_key("https://github.com/carbon6-dev/falcon"));
        assert_eq!(remote_key("HTTPS://GitHub.com:443/Carbon6-Dev/Falcon.GIT/"), remote_key("ssh://git@github.com/carbon6-dev/falcon.git"));
        assert_ne!(remote_key("https://example.invalid/Team/Repo.git"), remote_key("https://example.invalid/team/repo.git"));
        assert_ne!(remote_key("/tmp/Repo.git"), remote_key("/tmp/repo.git"));
        assert_ne!(remote_key("ssh://git@example.invalid:2222/team/repo"), remote_key("ssh://git@example.invalid/team/repo"));
        assert_eq!(remote_key("/tmp/test origin.git"), remote_key("file:///tmp/test origin.git"));
        assert!(remote_key("ext::command argument").is_none());
        assert!(remote_key("../local.git").is_none());
        let key = "example.invalid/team/repo";
        assert_eq!(clone_remote(key, "https://user:secret@example.invalid/team/repo").unwrap().0, "https://example.invalid/team/repo");
        assert!(clone_remote(key, "https://example.invalid/a/different/repo").is_none());
        assert!(clone_remote("example.invalid/team/repo;id", "git@example.invalid:team/repo;id").is_none());
    }

    fn selected_repositories(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let from = folder(root, "from");
        let to = folder(root, "to");
        let source = folder(root, "source");
        git(&source, &["init", "-b", "main", "--template="]).unwrap();
        git(&source, &["config", "user.name", "Move Test"]).unwrap();
        git(&source, &["config", "user.email", "move@example.invalid"]).unwrap();
        fs::write(source.join("tracked"), "base\n").unwrap();
        git(&source, &["add", "."]).unwrap();
        git(&source, &["commit", "-qm", "base"]).unwrap();
        let destination = root.join("destination");
        let mut clone =
            command(root, &["clone", "--no-hardlinks", "--template=", "--", path_str(&source).unwrap(), path_str(&destination).unwrap()]);
        clone.env("GIT_ALLOW_PROTOCOL", "file");
        git_to(clone, MAX_JSON, &mut io::sink()).unwrap();
        for repo in [&source, &destination] {
            git(repo, &["remote", "remove", "origin"]).ok();
            git(repo, &["remote", "add", "origin", "https://github.com/Carbon6-Dev/Falcon.git"]).unwrap();
            git(repo, &["config", "user.name", "Move Test"]).unwrap();
            git(repo, &["config", "user.email", "move@example.invalid"]).unwrap();
        }
        (from, to, source, destination)
    }

    fn dirty_source(source: &Path) {
        fs::write(source.join("tracked"), "staged\n").unwrap();
        git(source, &["add", "tracked"]).unwrap();
        fs::write(source.join("tracked"), "unstaged\n").unwrap();
        fs::write(source.join("untracked"), "draft\n").unwrap();
    }

    #[test]
    fn selected_checkout_retries_partial_application_and_rolls_back_exact_index() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, source, destination) = selected_repositories(t.path());
        dirty_source(&source);
        let descriptor = describe(&source, true, None).unwrap();
        let id = Uuid::now_v7();
        let target = preflight_selected(&descriptor, &to, id, &serde_json::json!({"cwd": destination})).unwrap();
        let before_index = fs::read(destination.join(".git/index")).unwrap();
        let manifest = prepare_export(&from, id, "session", &source, &descriptor, None, &[]).unwrap();
        replay(&from, &to, &manifest, &target);
        let prepared = finalize_import(&to, id, None).unwrap();
        assert_eq!(prepared.cwd, destination.canonicalize().unwrap());
        assert_eq!(git(&destination, &["show", ":tracked"]).unwrap(), b"staged\n");
        assert_eq!(fs::read(destination.join("tracked")).unwrap(), b"unstaged\n");
        assert_eq!(fs::read(destination.join("untracked")).unwrap(), b"draft\n");
        // Crash after some paths were applied, before the prepared receipt:
        // both old and desired states must be recognized without re-backing-up.
        let artifact = to.canonicalize().unwrap().join("moves").join(format!("{id}-import"));
        fs::remove_file(artifact.join("prepared")).unwrap();
        fs::write(destination.join("tracked"), "base\n").unwrap();
        fs::write(destination.join(".git/index"), &before_index).unwrap();
        assert_eq!(finalize_import(&to, id, None).unwrap(), prepared);
        abort_import(&to, id).unwrap();
        abort_import(&to, id).unwrap();
        assert_eq!(fs::read(destination.join(".git/index")).unwrap(), before_index);
        assert_eq!(fs::read(destination.join("tracked")).unwrap(), b"base\n");
        assert!(!destination.join("untracked").exists());
        assert_eq!(fs::read(source.join("tracked")).unwrap(), b"unstaged\n");
        assert_eq!(git(&source, &["show", ":tracked"]).unwrap(), b"staged\n");
    }

    #[test]
    fn selected_checkout_refuses_dirty_targets_and_preserves_external_edits_on_abort() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, source, destination) = selected_repositories(t.path());
        dirty_source(&source);
        let descriptor = describe(&source, true, None).unwrap();
        let id = Uuid::now_v7();
        fs::write(destination.join("tracked"), "target edits\n").unwrap();
        assert!(
            preflight_selected(&descriptor, &to, id, &serde_json::json!({"cwd": destination})).unwrap_err().to_string().contains("Folder:")
        );
        fs::write(destination.join("tracked"), "base\n").unwrap();
        let target = preflight_selected(&descriptor, &to, id, &serde_json::json!({"cwd": destination})).unwrap();
        let manifest = prepare_export(&from, id, "session", &source, &descriptor, None, &[]).unwrap();
        replay(&from, &to, &manifest, &target);
        finalize_import(&to, id, None).unwrap();
        fs::write(destination.join("tracked"), "external edits\n").unwrap();
        assert!(abort_import(&to, id).unwrap_err().to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read(destination.join("tracked")).unwrap(), b"external edits\n");
        assert_eq!(git(&destination, &["show", ":tracked"]).unwrap(), b"staged\n");
    }

    #[test]
    fn selected_worktree_retries_and_rolls_back_only_its_own_branch_and_checkout() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, source, destination) = selected_repositories(t.path());
        fs::write(source.join("unpushed"), "local commit\n").unwrap();
        git(&source, &["add", "."]).unwrap();
        git(&source, &["commit", "-qm", "unpushed"]).unwrap();
        dirty_source(&source);
        // A new worktree must not require cleaning or changing the selected
        // checkout, even when it has both staged and unstaged local work.
        fs::write(destination.join("tracked"), "target staged\n").unwrap();
        git(&destination, &["add", "tracked"]).unwrap();
        fs::write(destination.join("tracked"), "target dirty\n").unwrap();
        let original_index = fs::read(destination.join(".git/index")).unwrap();
        let descriptor = describe(&source, true, None).unwrap();
        let id = Uuid::now_v7();
        let request = serde_json::json!({"cwd": destination, "worktree": {"branch": "selected-transfer"}});
        let target = preflight_selected(&descriptor, &to, id, &request).unwrap();
        let manifest = prepare_export(&from, id, "session", &source, &descriptor, None, &[]).unwrap();
        replay(&from, &to, &manifest, &target);
        let prepared = finalize_import(&to, id, None).unwrap();
        assert_eq!(prepared.worktree_branch, "selected-transfer");
        assert_eq!(prepared.repo_root, Some(destination.canonicalize().unwrap()));
        assert_eq!(git(&prepared.cwd, &["show", ":tracked"]).unwrap(), b"staged\n");
        assert_eq!(fs::read(prepared.cwd.join("tracked")).unwrap(), b"unstaged\n");
        assert_eq!(fs::read(prepared.cwd.join("unpushed")).unwrap(), b"local commit\n");
        let artifact = to.canonicalize().unwrap().join("moves").join(format!("{id}-import"));
        fs::remove_file(artifact.join("prepared")).unwrap();
        assert_eq!(finalize_import(&to, id, None).unwrap(), prepared);
        abort_import(&to, id).unwrap();
        abort_import(&to, id).unwrap();
        assert!(!prepared.cwd.exists());
        assert!(git(&destination, &["show-ref", "--verify", "refs/heads/selected-transfer"]).is_err());
        assert_eq!(fs::read(destination.join(".git/index")).unwrap(), original_index);
        assert_eq!(fs::read(destination.join("tracked")).unwrap(), b"target dirty\n");
    }

    #[test]
    fn repository_discovery_finds_unused_worker_checkout_and_known_siblings() {
        let t = tempfile::tempdir().unwrap();
        let (_, state, _, destination) = selected_repositories(t.path());
        let unused = state.join("unused");
        fs::rename(&destination, &unused).unwrap();
        let remote = remote(&unused).unwrap().0;
        assert!(repository_folders(&state, &[], Some(&remote)).contains(&unused.canonicalize().unwrap()));
        let known = folder(t.path(), "known-scratch");
        assert!(repository_folders(&folder(t.path(), "other-state"), &[known], Some(&remote)).contains(&unused.canonicalize().unwrap()));
    }

    #[test]
    fn selected_clone_publication_retries_and_does_not_remove_external_changes() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, source, _) = selected_repositories(t.path());
        git(&source, &["remote", "set-url", "origin", path_str(&source).unwrap()]).unwrap();
        dirty_source(&source);
        let descriptor = describe(&source, true, None).unwrap();
        let id = Uuid::now_v7();
        let cwd = t.path().canonicalize().unwrap().join("new-clone");
        let request = serde_json::json!({"cwd": cwd, "clone": true});
        let target = preflight_selected(&descriptor, &to, id, &request).unwrap();
        assert!(!cwd.exists(), "a direct clone publishes the transferred snapshot during preparation");
        let manifest = prepare_export(&from, id, "session", &source, &descriptor, None, &[]).unwrap();
        replay(&from, &to, &manifest, &target);
        let prepared = finalize_import(&to, id, None).unwrap();
        assert_eq!(prepared.cwd, cwd);
        assert_eq!(git(&cwd, &["show", ":tracked"]).unwrap(), b"staged\n");
        assert_eq!(fs::read(cwd.join("tracked")).unwrap(), b"unstaged\n");
        let artifact = to.canonicalize().unwrap().join("moves").join(format!("{id}-import"));
        fs::remove_file(artifact.join("prepared")).unwrap();
        assert_eq!(finalize_import(&to, id, None).unwrap(), prepared);
        fs::write(cwd.join("external"), "keep this\n").unwrap();
        assert!(abort_import(&to, id).unwrap_err().to_string().contains("refusing to remove"));
        assert_eq!(fs::read(cwd.join("external")).unwrap(), b"keep this\n");
        fs::remove_file(cwd.join("external")).unwrap();
        abort_import(&to, id).unwrap();
        abort_import(&to, id).unwrap();
        assert!(!cwd.exists());
        assert_eq!(fs::read(source.join("tracked")).unwrap(), b"unstaged\n");
    }

    #[test]
    fn clone_and_linked_worktree_are_one_request_and_abort_retains_the_authorized_clone() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, source, _) = selected_repositories(t.path());
        git(&source, &["remote", "set-url", "origin", path_str(&source).unwrap()]).unwrap();
        dirty_source(&source);
        let descriptor = describe(&source, true, None).unwrap();
        let id = Uuid::now_v7();
        let cwd = t.path().join("new-seed-checkout");
        let request = serde_json::json!({"cwd": cwd, "clone": true, "worktree": {"branch": "combined-clone-transfer"}});
        let target = preflight_selected(&descriptor, &to, id, &request).unwrap();
        assert!(cwd.join(".git").is_dir());
        assert!(git(&cwd, &["status", "--porcelain=v1"]).unwrap().is_empty());
        assert_eq!(preflight_selected(&descriptor, &to, id, &request).unwrap(), target);
        let manifest = prepare_export(&from, id, "session", &source, &descriptor, None, &[]).unwrap();
        replay(&from, &to, &manifest, &target);
        let prepared = finalize_import(&to, id, None).unwrap();
        assert_eq!(prepared.repo_root, Some(cwd.canonicalize().unwrap()));
        assert_eq!(prepared.worktree_branch, "combined-clone-transfer");
        assert_eq!(git(&prepared.cwd, &["show", ":tracked"]).unwrap(), b"staged\n");
        abort_import(&to, id).unwrap();
        assert!(!prepared.cwd.exists());
        assert!(cwd.join(".git").is_dir());
        assert_eq!(fs::read(cwd.join("tracked")).unwrap(), b"base\n");
    }

    #[test]
    fn clone_requires_opt_in_and_reuses_the_managed_bare_seed() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let state = folder(&root, "target-state");
        let upstream = folder(&root, "upstream.git");
        let source = folder(&root, "source");
        git(&upstream, &["init", "--bare", "-b", "main", "--template="]).unwrap();
        git(&source, &["init", "-b", "main", "--template="]).unwrap();
        git(&source, &["config", "user.email", "move@example.invalid"]).unwrap();
        git(&source, &["config", "user.name", "Move Test"]).unwrap();
        fs::write(source.join("a.txt"), "one\n").unwrap();
        git(&source, &["add", "."]).unwrap();
        git(&source, &["commit", "-m", "published"]).unwrap();
        let url = format!("file://{}", upstream.display());
        git(&source, &["remote", "add", "origin", &url]).unwrap();
        git(&source, &["push", "origin", "main"]).unwrap();
        fs::write(source.join("a.txt"), "two\n").unwrap();
        git(&source, &["commit", "-am", "unpushed"]).unwrap();
        let descriptor = describe(&source, true, None).unwrap();
        assert!(preflight_with_clone(&descriptor, &[], &state, false).is_err());
        assert!(!state.join("repositories").exists());
        let target = preflight_with_clone(&descriptor, &[], &state, true).unwrap();
        let seed = target.repo_root.as_ref().unwrap();
        assert!(is_bare_root(seed).unwrap());
        assert_ne!(text(seed, &["rev-parse", "HEAD"]).unwrap(), descriptor.repo.as_ref().unwrap().head);
        fs::rename(&upstream, root.join("upstream-disabled.git")).unwrap();
        assert_eq!(preflight_with_clone(&descriptor, &[], &state, false).unwrap(), target);
        assert_eq!(preflight_with_clone(&descriptor, &[], &state, true).unwrap(), target);

        let from = folder(&root, "source-state");
        let id = Uuid::now_v7();
        let manifest = prepare_export(&from, id, "session", &source, &descriptor, None, &[]).unwrap();
        replay(&from, &state, &manifest, &target);
        let prepared = finalize_import(&state, id, None).unwrap();
        assert_eq!(fs::read(prepared.cwd.join("a.txt")).unwrap(), b"two\n");
        abort_import(&state, id).unwrap();
        assert!(seed.is_dir());
    }

    #[test]
    fn git_roundtrip_keeps_unpushed_commits_and_index() {
        let t = tempfile::tempdir().unwrap();
        let from = folder(t.path(), "from");
        let to = folder(t.path(), "to");
        let repo = folder(t.path(), "src-repo");
        let known = folder(t.path(), "known");
        for p in [&repo, &known] {
            git(p, &["init", "-b", "main", "--template="]).unwrap();
            git(p, &["config", "user.email", "move@example.invalid"]).unwrap();
            git(p, &["config", "user.name", "Move Test"]).unwrap();
        }
        git(&repo, &["remote", "add", "origin", "git@example.invalid:team/repo.git"]).unwrap();
        git(&known, &["remote", "add", "origin", "https://account@example.invalid/team/repo/"]).unwrap();
        let cwd = folder(&repo, "src");
        fs::write(cwd.join("tracked"), "base\n").unwrap();
        fs::write(cwd.join("rename-old"), "rename me\n").unwrap();
        fs::write(repo.join(".gitignore"), "ignored\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "unpushed"]).unwrap();
        git(&repo, &["mv", "src/rename-old", "src/rename-new"]).unwrap();
        fs::write(cwd.join("tracked"), "staged\n").unwrap();
        git(&repo, &["add", "src/tracked"]).unwrap();
        fs::write(cwd.join("tracked"), "unstaged\n").unwrap();
        fs::write(cwd.join("untracked"), [0, 1, 2]).unwrap();
        fs::write(cwd.join("ignored"), "local only").unwrap();
        fs::write(known.join("sentinel"), "target stays untouched").unwrap();
        let source = describe(&cwd, true, None).unwrap();
        assert!(preflight(&source, &[]).is_err());
        let target = preflight(&source, std::slice::from_ref(&known)).unwrap();
        let id = Uuid::now_v7();
        let m = prepare_export(&from, id, "session", &cwd, &source, None, &[]).unwrap();
        replay(&from, &to, &m, &target);
        let prepared = finalize_import(&to, id, None).unwrap();
        let root = prepared.repo_root.as_ref().unwrap();
        assert_eq!(prepared.uncommitted_files, 4); // rename pair, tracked, untracked; not ignored
        assert_eq!(prepared.uncommitted_files, uncommitted_files(&repo).unwrap());
        assert_eq!(text(root, &["rev-parse", "HEAD"]).unwrap(), source.repo.unwrap().head);
        assert_eq!(git(root, &["show", ":src/tracked"]).unwrap(), b"staged\n");
        assert_eq!(fs::read(prepared.cwd.join("tracked")).unwrap(), b"unstaged\n");
        assert_eq!(fs::read(prepared.cwd.join("untracked")).unwrap(), [0, 1, 2]);
        assert!(!prepared.cwd.join("ignored").exists());
        assert_eq!(fs::read(known.join("sentinel")).unwrap(), b"target stays untouched");
        assert_eq!(fs::read(cwd.join("tracked")).unwrap(), b"unstaged\n");
        commit_import(&to, id).unwrap();
        assert!(abort_import(&to, id).is_err());
        assert_eq!(finalize_import(&to, id, None).unwrap(), prepared);
    }

    #[test]
    fn git_refuses_index_flags_that_hide_edits() {
        let t = tempfile::tempdir().unwrap();
        let from = folder(t.path(), "from");
        let repo = folder(t.path(), "repo");
        git(&repo, &["init", "-b", "main", "--template="]).unwrap();
        git(&repo, &["config", "user.email", "move@example.invalid"]).unwrap();
        git(&repo, &["config", "user.name", "Move Test"]).unwrap();
        git(&repo, &["remote", "add", "origin", "git@example.invalid:team/repo.git"]).unwrap();
        folder(&repo, "excluded");
        let path = "excluded/tracked\nfile";
        fs::write(repo.join(path), b"base\n").unwrap();
        git(&repo, &["add", "--", path]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        let source = describe(&repo, true, None).unwrap();
        let refuses = || {
            let index = repo.join(".git/index");
            let original_index = fs::read(&index).unwrap();
            let id = Uuid::now_v7();
            let err = prepare_export(&from, id, "session", &repo, &source, None, &[]).unwrap_err();
            assert!(err.to_string().contains("assume-unchanged or skip-worktree"), "{err:#}");
            assert_eq!(fs::read(repo.join(path)).unwrap(), b"hidden edit\n");
            assert_eq!(fs::read(&index).unwrap(), original_index);
            let (artifact, _lock) = locked(&from, id, "export").unwrap();
            for entry in ["manifest", "payload", "payload.building"] {
                assert!(!artifact.join(entry).exists());
            }
        };
        for (set, unset, tag) in [("--assume-unchanged", "--no-assume-unchanged", b'h'), ("--skip-worktree", "--no-skip-worktree", b'S')] {
            fs::write(repo.join(path), b"base\n").unwrap();
            git(&repo, &["update-index", set, "--", path]).unwrap();
            fs::write(repo.join(path), b"hidden edit\n").unwrap();
            assert!(git(&repo, &["diff", "--", path]).unwrap().is_empty());
            assert_eq!(git(&repo, &["ls-files", "-v", "-z"]).unwrap()[0], tag);
            refuses();
            git(&repo, &["update-index", unset, "--", path]).unwrap();
        }
        fs::write(repo.join(path), b"base\n").unwrap();
        git(&repo, &["update-index", "--refresh"]).unwrap();
        git(&repo, &["sparse-checkout", "set", "--cone", "included"]).unwrap();
        assert!(!repo.join(path).exists());
        fs::create_dir_all(repo.join("excluded")).unwrap();
        fs::write(repo.join(path), b"hidden edit\n").unwrap();
        refuses();
    }

    #[test]
    fn native_rehomes_jsonl_without_scan_or_overwrite() {
        // Exercise the normal root-owned sticky temp ancestor; on macOS the
        // input config paths also exercise the system /tmp symlink alias.
        let t = tempfile::tempdir_in("/tmp").unwrap();
        let path = t.path().canonicalize().unwrap();
        let from = folder(&path, "from");
        let to = folder(&path, "to");
        let cwd = folder(&path, "old workspace");
        // Explicit temporary CLAUDE_CONFIG_DIR values; no mutation of the
        // process-wide environment (tests run in parallel).
        let from_config = folder(t.path(), "source-claude-config");
        let to_config = folder(t.path(), "destination-claude-config");
        let native = Uuid::now_v7();
        let old = folder(&folder(&from_config, "projects"), &project_name(&cwd).unwrap()).join(format!("{native}.jsonl"));
        let jsonl = b"{\"type\":\"user\"}\n";
        fs::write(&old, jsonl).unwrap();
        let source = describe(&cwd, false, Some(native)).unwrap();
        let plan = preflight(&source, &[]).unwrap();
        let id = Uuid::now_v7();
        let m = prepare_export(&from, id, "session", &cwd, &source, Some(&from_config), &[]).unwrap();
        replay(&from, &to, &m, &plan);
        let (artifact, lock) = locked(&to, id, "import").unwrap();
        drop(lock);
        let target_project = folder(&folder(&to_config, "projects"), &project_name(&artifact.join("workspace")).unwrap());
        let new = target_project.join(format!("{native}.jsonl"));
        fs::write(&new, "existing unrelated transcript").unwrap();
        assert!(finalize_import(&to, id, Some(&to_config)).is_err());
        assert_eq!(fs::read(&new).unwrap(), b"existing unrelated transcript");
        fs::remove_file(&new).unwrap();
        let prepared = finalize_import(&to, id, Some(&to_config)).unwrap();
        assert_eq!(prepared.native_resume, Some(native));
        assert_eq!(fs::read(&new).unwrap(), jsonl);
        abort_import(&to, id).unwrap();
        assert!(!new.exists());
        assert_eq!(fs::read(old).unwrap(), jsonl);
    }

    #[test]
    fn native_refuses_writable_transcript_paths() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().canonicalize().unwrap();
        let from = folder(&path, "from");
        let to = folder(&path, "to");
        let cwd = folder(&path, "workspace");
        let ancestor = folder(&path, "ancestor");
        let config = folder(&ancestor, "config");
        let projects = folder(&config, "projects");
        let project = folder(&projects, &project_name(&cwd).unwrap());
        let resume = Uuid::now_v7();
        let transcript = project.join(format!("{resume}.jsonl"));
        let jsonl = b"{\"type\":\"user\"}\n";
        fs::write(&transcript, jsonl).unwrap();
        let source = describe(&cwd, false, Some(resume)).unwrap();
        let mode = |p: &Path, mode| fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
        for (p, reject, restore) in [
            (&ancestor, 0o775, 0o755),
            (&config, 0o1777, 0o755),
            (&projects, 0o757, 0o755),
            (&project, 0o775, 0o755),
            (&transcript, 0o666, 0o644),
        ] {
            mode(p, reject);
            let err = prepare_export(&from, Uuid::now_v7(), "session", &cwd, &source, Some(&config), &[]).unwrap_err();
            assert!(format!("{err:#}").contains("group/other-writable native transcript"), "{err:#}");
            mode(p, restore);
        }
        let alias = path.join("config-symlink");
        std::os::unix::fs::symlink(&config, &alias).unwrap();
        let err = prepare_export(&from, Uuid::now_v7(), "session", &cwd, &source, Some(&alias), &[]).unwrap_err();
        assert!(format!("{err:#}").contains("without symlinks"), "{err:#}");

        let id = Uuid::now_v7();
        let manifest = prepare_export(&from, id, "session", &cwd, &source, Some(&config), &[]).unwrap();
        replay(&from, &to, &manifest, &preflight(&source, &[]).unwrap());
        let dest = folder(&path, "destination-config");
        let dest_projects = folder(&dest, "projects");
        let (artifact, lock) = locked(&to, id, "import").unwrap();
        drop(lock);
        let dest_project = folder(&dest_projects, &project_name(&artifact.join("workspace")).unwrap());
        let installed = dest_project.join(format!("{resume}.jsonl"));
        let staged = dest_project.join(format!(".c6-move-{id}.jsonl"));
        mode(&dest_project, 0o757);
        let err = finalize_import(&to, id, Some(&dest)).unwrap_err();
        assert!(err.to_string().contains("group/other-writable native transcript directory"), "{err:#}");
        assert!(!staged.exists() && !installed.exists());
        mode(&dest_project, 0o755);
        finalize_import(&to, id, Some(&dest)).unwrap();
        mode(&dest_project, 0o775);
        let err = abort_import(&to, id).unwrap_err();
        assert!(err.to_string().contains("group/other-writable native transcript directory"), "{err:#}");
        assert!(staged.exists() && installed.exists());
        mode(&dest_project, 0o755);
        mode(&staged, 0o666);
        let err = abort_import(&to, id).unwrap_err();
        assert!(err.to_string().contains("group/other-writable native transcript file"), "{err:#}");
        assert!(staged.exists() && installed.exists());
        mode(&staged, 0o600);
        abort_import(&to, id).unwrap();
        assert!(!staged.exists() && !installed.exists());
        assert_eq!(fs::read(transcript).unwrap(), jsonl);
    }
}
