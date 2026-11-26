//! Bounded build-directory inventory and conservative cleanup policy.
//!
//! Ownership is supplied from worker-created worktree placement, never inferred
//! from a `target` folder name. Callers pass freshly read active session roots.
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use c6_api::machines::MachineBuildFolder;

pub(crate) const DEFAULT_ARCHIVED_GRACE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// Incremental output is recognized in the context of its Rust output root,
/// not by its name alone. Revisit that context for a directly selected row;
/// this broadens discovery only, never the path authorized for deletion.
pub(crate) fn incremental_scan_root(fs: &impl FileSystem, path: &Path) -> PathBuf {
    if !path.file_name().is_some_and(|name| name == "incremental") {
        return path.to_owned();
    }
    let profile_root = path.parent().and_then(Path::parent).unwrap_or(path);
    for ancestor in profile_root.ancestors().take(64).take_while(|p| p.parent().is_some()) {
        if ancestor.file_name().is_some_and(|name| name == "target")
            || fs.entry(&ancestor.join(".rustc_info.json")).is_ok_and(|entry| entry.file && !entry.symlink)
        {
            return ancestor.to_owned();
        }
    }
    // Profile markers can prove a custom output root even without the
    // top-level rustc marker. The actual scan still has to establish this.
    profile_root.to_owned()
}

/// One durable reference to a build directory. Retain references after archive
/// so last use, attribution, and ownership survive worker/session restarts.
#[derive(Clone, Debug)]
pub(crate) struct Reference {
    pub path: PathBuf,
    /// An attribution/ownership scope is scanned but is never itself a
    /// removable build folder merely because it is registered.
    pub scope: bool,
    pub session_id: String,
    pub repository: String,
    pub carbon6_owned: bool,
    pub active: bool,
    pub archived_at_ms: Option<i64>,
    pub last_used_ms: Option<i64>,
}

/// Resolve journal cwd records on the blocking pool. Only the worker's own
/// worktree layout establishes ownership; repository roots outside it remain
/// external even when a carbon6 session happened to build there.
pub(crate) fn prepare_references(state: &Path, places: Vec<Reference>) -> (Vec<Reference>, bool) {
    let state = state.canonicalize().ok();
    let managed_root = |name: &str| {
        let path = state.as_ref()?.join(name);
        match path.canonicalize() {
            Ok(path) => Some(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(path),
            Err(_) => None,
        }
    };
    let worktrees = managed_root("worktrees");
    let scratch = managed_root("scratch");
    let mut references = Vec::new();
    let mut partial = false;
    for place in places {
        if !normalized(&place.path) {
            partial = true;
            continue;
        }
        let (cwd, existing_ancestor) = match place.path.canonicalize() {
            Ok(cwd) => (cwd, None),
            Err(_) => {
                // Find the first existing lexical ancestor. Do not skip over
                // permission errors or dangling links: they do not prove that
                // the requested path is absent or belongs to this directory.
                let mut resolved = None;
                for ancestor in place.path.ancestors().take(64) {
                    match std::fs::symlink_metadata(ancestor) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => break,
                        Ok(_) => {
                            if let Ok(existing) = ancestor.canonicalize()
                                && existing.is_dir()
                                && let Ok(suffix) = place.path.strip_prefix(ancestor)
                            {
                                resolved = Some((existing.join(suffix), Some(existing)));
                            }
                            break;
                        }
                    }
                }
                let Some(resolved) = resolved else {
                    partial = true;
                    continue;
                };
                resolved
            }
        };
        let owned_top = worktrees
            .as_ref()
            .and_then(|root| {
                let relative = cwd.strip_prefix(root).ok()?;
                let mut components = relative.components();
                Some(root.join(components.next()?).join(components.next()?))
            })
            .or_else(|| {
                scratch.as_ref().and_then(|root| {
                    let relative = cwd.strip_prefix(root).ok()?;
                    let session = relative.components().next()?;
                    (session.as_os_str() == std::ffi::OsStr::new(&place.session_id)).then(|| root.join(session))
                })
            });
        let top = if let Some(top) = &owned_top {
            if let Some(existing) = &existing_ancestor
                && !existing.starts_with(top)
            {
                // The nearest existing ancestor is above the managed top,
                // so the entire top (not merely a cwd child) is provably gone.
                // Automatic/manual cleanup holds the lifecycle writer while
                // resolving these inputs, excluding managed-top recreation.
                if !place.active {
                    continue;
                }
                partial = true;
            }
            top.clone()
        } else {
            let existing = existing_ancestor.as_deref().unwrap_or(&cwd);
            match existing.ancestors().take(64).find(|p| p.join(".git").exists()) {
                Some(repository) => repository.to_owned(),
                None if existing_ancestor.is_none() => cwd,
                None => {
                    // No managed top or repository scope proves attribution.
                    // Never silently lose even an idle unarchived reference.
                    partial = true;
                    continue;
                }
            }
        };
        references.push(Reference { path: top, scope: true, carbon6_owned: owned_top.is_some(), ..place });
    }
    (references, partial)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_entries: usize,
    pub max_roots: usize,
    pub max_folders: usize,
    pub max_depth: usize,
    pub max_duration: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_entries: 100_000, max_roots: 64, max_folders: 256, max_depth: 64, max_duration: Duration::from_secs(5) }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Inventory {
    pub folders: Vec<MachineBuildFolder>,
    pub entries: usize,
    pub partial: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub path: PathBuf,
    pub directory: bool,
    pub file: bool,
    pub symlink: bool,
    pub bytes: u64,
    pub modified_ms: Option<i64>,
    pub device: u64,
}

/// Testable bounded filesystem interface: implementations must not follow
/// symlinks and must return at most `limit` children plus a truncation flag.
pub(crate) trait FileSystem {
    fn entry(&self, path: &Path) -> io::Result<Entry>;
    fn children(&self, path: &Path, limit: usize) -> io::Result<(Vec<PathBuf>, bool)>;
}

pub(crate) struct LocalFs;

impl FileSystem for LocalFs {
    fn entry(&self, path: &Path) -> io::Result<Entry> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Entry {
            path: path.to_owned(),
            directory: meta.is_dir(),
            file: meta.is_file(),
            symlink: meta.file_type().is_symlink(),
            bytes: if meta.is_file() { meta.len() } else { 0 },
            modified_ms: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis().min(i64::MAX as u128) as i64),
            device: meta.dev(),
        })
    }

    fn children(&self, path: &Path, limit: usize) -> io::Result<(Vec<PathBuf>, bool)> {
        let mut entries = std::fs::read_dir(path)?;
        let paths = entries.by_ref().take(limit).map(|entry| entry.map(|e| e.path())).collect::<io::Result<Vec<_>>>()?;
        Ok((paths, entries.next().is_some()))
    }
}

pub(crate) fn discover(roots: &[PathBuf], references: &[Reference], active_roots: &[PathBuf], limits: Limits) -> Inventory {
    let start = Instant::now();
    discover_with(&LocalFs, roots, references, active_roots, limits, || start.elapsed())
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn normalized(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|p| matches!(p, std::path::Component::ParentDir | std::path::Component::CurDir))
}

/// Root paths must be canonicalized by the caller. Traversal never resolves
/// symlinks. References are visited first, so a huge checkout cannot hide the
/// managed caches; all roots share the entry/time/folder limits.
pub(crate) fn discover_with(
    fs: &impl FileSystem,
    roots: &[PathBuf],
    references: &[Reference],
    active_roots: &[PathBuf],
    limits: Limits,
    elapsed: impl Fn() -> Duration,
) -> Inventory {
    let mut result = Inventory::default();
    let mut starts = Vec::new();
    let mut seen_roots = BTreeSet::new();
    for root in references.iter().map(|r| &r.path).chain(roots) {
        if !normalized(root) {
            result.partial = true;
        } else if seen_roots.insert(root.clone()) {
            if starts.len() < limits.max_roots {
                starts.push(root.clone());
            } else {
                result.partial = true;
            }
        }
    }
    let mut visited = BTreeSet::new();
    let mut observations = Vec::new();
    let mut folders = BTreeMap::<PathBuf, MachineBuildFolder>::new();
    let root_count = starts.len();
    for (index, root) in starts.into_iter().enumerate() {
        let remaining_roots = root_count - index;
        let entry_limit = result.entries + limits.max_entries.saturating_sub(result.entries).div_ceil(remaining_roots);
        let before = elapsed();
        let deadline = before + limits.max_duration.saturating_sub(before) / remaining_roots as u32;
        // DFS memory is bounded by depth * entry budget, with no recursive
        // filesystem calls or unbounded collection of a directory's entries.
        let mut pending = vec![(root, 0)];
        let mut device = None;
        while let Some((path, depth)) = pending.pop() {
            if result.entries >= entry_limit || elapsed() >= deadline {
                result.partial = true;
                break;
            }
            if !visited.insert(path.clone()) {
                continue;
            }
            result.entries += 1;
            let entry = match fs.entry(&path) {
                Ok(entry) if !entry.symlink => entry,
                Ok(_) => continue,
                Err(_) => {
                    result.partial = true;
                    continue;
                }
            };
            if device.is_some_and(|device| device != entry.device) {
                result.partial = true;
                continue;
            }
            device = Some(entry.device);
            let registered = references.iter().any(|r| !r.scope && r.path == path);
            let incremental =
                path.file_name().is_some_and(|name| name == "incremental") && folders.keys().any(|parent| path.starts_with(parent));
            let target = path.file_name().is_some_and(|name| name == "target");
            let children = if entry.directory && depth < limits.max_depth {
                match fs.children(&entry.path, entry_limit.saturating_sub(result.entries)) {
                    Ok((children, truncated)) => {
                        result.partial |= truncated;
                        children
                    }
                    Err(_) => {
                        result.partial = true;
                        Vec::new()
                    }
                }
            } else {
                result.partial |= entry.directory;
                Vec::new()
            };
            // Marker names alone are not proof: a source directory may have
            // similarly named files or symlinks. Never follow a marker link.
            let has_marker = |name: &str, directory| {
                children.iter().any(|p| {
                    p.file_name().is_some_and(|n| n == name)
                        && fs.entry(p).is_ok_and(|entry| !entry.symlink && if directory { entry.directory } else { entry.file })
                })
            };
            let custom = has_marker(".rustc_info.json", false);
            let profile = has_marker(".fingerprint", true) && has_marker("deps", true) && has_marker("build", true);
            let mut candidates = Vec::new();
            if entry.directory && (registered || target || incremental || custom) {
                let recognized = custom || (incremental && folders.iter().any(|(parent, f)| path.starts_with(parent) && !f.partial));
                candidates.push((path.clone(), incremental, recognized));
            }
            if profile && let Some(parent) = path.parent().filter(|p| **p != *Path::new("/")) {
                candidates.push((parent.to_owned(), false, true));
            }
            for (path, incremental, recognized) in candidates {
                if let Some(folder) = folders.get_mut(&path) {
                    // A profile found after visiting its parent confirms that
                    // this is build output rather than source named `target`.
                    if recognized {
                        folder.partial = false;
                    }
                    continue;
                }
                if folders.len() >= limits.max_folders {
                    result.partial = true;
                } else {
                    let refs: Vec<_> = references.iter().filter(|r| path.starts_with(&r.path)).collect();
                    let session_ids: BTreeSet<_> = refs.iter().map(|r| r.session_id.clone()).filter(|s| !s.is_empty()).collect();
                    let repositories: BTreeSet<_> = refs.iter().map(|r| r.repository.clone()).filter(|s| !s.is_empty()).collect();
                    let archived_at_ms = if !refs.is_empty() && refs.iter().all(|r| r.archived_at_ms.is_some()) {
                        refs.iter().filter_map(|r| r.archived_at_ms).max()
                    } else {
                        None
                    };
                    folders.insert(
                        path.clone(),
                        MachineBuildFolder {
                            path: path.to_string_lossy().into_owned(),
                            carbon6_owned: !refs.is_empty() && refs.iter().all(|r| r.carbon6_owned),
                            active: references.iter().any(|r| r.active && overlaps(&path, &r.path))
                                || (refs.is_empty() && references.iter().any(|r| r.active))
                                || active_roots.iter().any(|active| overlaps(&path, active)),
                            archived_at_ms,
                            last_used_ms: refs.iter().filter_map(|r| r.last_used_ms).max(),
                            session_ids: session_ids.into_iter().collect(),
                            repositories: repositories.into_iter().collect(),
                            incremental,
                            partial: !recognized || refs.iter().any(|r| r.scope && r.path == path),
                            ..Default::default()
                        },
                    );
                }
            }
            observations.push(entry);
            let mut children = children;
            children.sort();
            pending.extend(children.into_iter().rev().map(|path| (path, depth + 1)));
        }
    }
    // A truncated inventory cannot establish complete sizes/last-use. Fail
    // closed for automatic cleanup, even for roots visited before the limit.
    for folder in folders.values_mut() {
        folder.partial |= result.partial;
        folder.partial |= references.iter().any(|r| r.scope && r.path == Path::new(&folder.path));
        // Profile markers can identify a custom target after earlier siblings
        // were visited. Accumulate from the bounded observations, not just the
        // suffix visited after identification.
        for entry in observations.iter().filter(|e| e.path.starts_with(&folder.path)) {
            folder.bytes = folder.bytes.saturating_add(entry.bytes);
            folder.modified_ms = folder.modified_ms.max(entry.modified_ms);
            folder.last_used_ms = folder.last_used_ms.max(entry.modified_ms);
        }
    }
    result.folders = folders.into_values().collect();
    result
}

#[derive(Clone, Debug)]
pub(crate) enum CleanupRequest {
    Archived,
    DiskPressure {
        bytes_to_free: u64,
    },
    /// Confirmation binds to exactly one displayed path, never a wildcard or
    /// a parent directory. External folders (including incremental) require it.
    Confirmed {
        path: PathBuf,
        confirmed: bool,
    },
}

/// Planning is pure. Execute only after rechecking live references under the
/// journal lifecycle lock; a scan is not an authorization token.
pub(crate) fn plan(folders: &[MachineBuildFolder], request: &CleanupRequest, now_ms: i64, grace: Duration) -> Vec<MachineBuildFolder> {
    let grace_ms = grace.as_millis().min(i64::MAX as u128) as i64;
    let mut eligible: Vec<_> = folders
        .iter()
        .filter(|folder| {
            if folder.active || folder.partial || !normalized(Path::new(&folder.path)) {
                return false;
            }
            match request {
                CleanupRequest::Confirmed { path, confirmed } => *confirmed && path == Path::new(&folder.path),
                CleanupRequest::Archived | CleanupRequest::DiskPressure { .. } => {
                    folder.carbon6_owned
                        && folder.archived_at_ms.is_some_and(|archived| {
                            // Future clocks fail closed. New writes restart the grace.
                            let used = folder.last_used_ms.unwrap_or(archived).max(archived);
                            now_ms >= used && now_ms.saturating_sub(used) >= grace_ms
                        })
                }
            }
        })
        .cloned()
        .collect();
    eligible.sort_by(|a, b| a.last_used_ms.cmp(&b.last_used_ms).then_with(|| a.path.cmp(&b.path)));
    // Never schedule both a target and its incremental descendants or count
    // their bytes twice. Prefer the whole eligible target.
    let all_paths: Vec<_> = eligible.iter().map(|f| PathBuf::from(&f.path)).collect();
    eligible.retain(|folder| !all_paths.iter().any(|other| other != Path::new(&folder.path) && Path::new(&folder.path).starts_with(other)));
    if let CleanupRequest::DiskPressure { bytes_to_free } = request {
        let mut bytes = 0_u64;
        eligible.retain(|folder| {
            if bytes >= *bytes_to_free {
                return false;
            }
            bytes = bytes.saturating_add(folder.bytes);
            true
        });
    }
    eligible
}

#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct CleanupResult {
    pub removed: Vec<String>,
    pub bytes: u64,
    pub errors: Vec<String>,
    /// Caller must deliver this through the worker's durable notice path.
    pub notice: Option<String>,
}

/// Inject removal and a fresh live-state check so tests never touch real build
/// output. Production passes `remove_local` while holding acquisition exclusion.
pub(crate) fn cleanup_with(
    planned: &[MachineBuildFolder],
    pressure: bool,
    mut protected: impl FnMut(&Path) -> bool,
    mut remove: impl FnMut(&Path) -> io::Result<()>,
) -> CleanupResult {
    let mut result = CleanupResult::default();
    for folder in planned {
        let path = Path::new(&folder.path);
        if folder.active || protected(path) {
            result.errors.push(format!("{}: build folder is active", path.display()));
            continue;
        }
        match remove(path) {
            Ok(()) => {
                result.removed.push(folder.path.clone());
                result.bytes = result.bytes.saturating_add(folder.bytes);
            }
            Err(error) => result.errors.push(format!("{}: {error}", path.display())),
        }
    }
    if pressure && !result.removed.is_empty() {
        let paths: String = result.removed.iter().take(16).cloned().collect::<Vec<_>>().join(", ").chars().take(3500).collect();
        result.notice = Some(format!(
            "Disk pressure: removed {} inactive carbon6 build folder(s), approximately {} bytes: {}",
            result.removed.len(),
            result.bytes,
            paths
        ));
    }
    result
}

/// Anchor every operation to held directory descriptors, rejecting symlinks in
/// every ancestor. No child symlink is ever followed, even if entries change
/// during removal. The caller must exclude cache acquisition throughout.
pub(crate) fn remove_local(path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    if !normalized(path) || path.parent().is_none() || path == Path::new("/") {
        return Err(io::Error::other("refusing non-canonical build folder"));
    }
    let mut parent = open_directory(nix::libc::AT_FDCWD, c"/")?;
    for component in path.parent().unwrap().components() {
        if let std::path::Component::Normal(name) = component {
            parent = open_directory(parent.as_raw_fd(), &c_name(name.as_bytes())?)?;
        }
    }
    let name = c_name(path.file_name().ok_or_else(|| io::Error::other("build folder has no name"))?.as_bytes())?;
    let root = open_directory(parent.as_raw_fd(), &name)?;
    let started = Instant::now();
    let mut remaining = 1_000_000;
    remove_contents(&root, root.metadata()?.dev(), 0, &mut remaining, &started)?;
    unlink_directory(&parent, &name, &root)
}

fn c_name(name: &[u8]) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn open_directory(parent: i32, name: &std::ffi::CStr) -> io::Result<std::fs::File> {
    use nix::libc;
    use std::os::fd::FromRawFd;
    // name is one component, except `/` when opening the root. Every later
    // lookup is relative to a descriptor we already own.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { std::fs::File::from_raw_fd(fd) }) }
}

fn unlink_directory(parent: &std::fs::File, name: &std::ffi::CStr, expected: &std::fs::File) -> io::Result<()> {
    use nix::libc;
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let mut held = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(expected.as_raw_fd(), held.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let held = unsafe { held.assume_init() };
    if stat.st_dev != held.st_dev || stat.st_ino != held.st_ino || stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(io::Error::other("build directory changed during removal"));
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct DirectoryStream(*mut nix::libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { nix::libc::closedir(self.0) };
    }
}

fn remove_contents(directory: &std::fs::File, device: u64, depth: usize, remaining: &mut usize, started: &Instant) -> io::Result<()> {
    use nix::libc;
    use std::ffi::CStr;
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::os::unix::fs::MetadataExt;
    if depth >= 64 {
        return Err(io::Error::other("build folder cleanup depth limit reached"));
    }
    if directory.metadata()?.dev() != device {
        return Err(io::Error::other("refusing to cross a build folder filesystem boundary"));
    }
    let reader = open_directory(directory.as_raw_fd(), c".")?;
    let stream = unsafe { libc::fdopendir(reader.as_raw_fd()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _ = reader.into_raw_fd(); // closedir owns the fd after fdopendir.
    let stream = DirectoryStream(stream);
    loop {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err(io::Error::other("build folder cleanup time limit reached"));
        }
        #[cfg(target_vendor = "apple")]
        let errno = unsafe { libc::__error() };
        #[cfg(not(target_vendor = "apple"))]
        let errno = unsafe { libc::__errno_location() };
        unsafe { *errno = 0 };
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = unsafe { *errno };
            return if error == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(error)) };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name == c"." || name == c".." {
            continue;
        }
        *remaining = remaining.checked_sub(1).ok_or_else(|| io::Error::other("build folder cleanup entry limit reached"))?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(directory.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = open_directory(directory.as_raw_fd(), name)?;
            remove_contents(&child, device, depth + 1, remaining, started)?;
            unlink_directory(directory, name, &child)?;
        } else if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeFs(BTreeMap<PathBuf, Entry>);

    impl FakeFs {
        fn add(&mut self, path: &str, directory: bool, bytes: u64, modified_ms: i64) {
            self.0.insert(
                path.into(),
                Entry { path: path.into(), directory, file: !directory, symlink: false, bytes, modified_ms: Some(modified_ms), device: 1 },
            );
        }
    }

    impl FileSystem for FakeFs {
        fn entry(&self, path: &Path) -> io::Result<Entry> {
            self.0.get(path).cloned().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn children(&self, path: &Path, limit: usize) -> io::Result<(Vec<PathBuf>, bool)> {
            let children: Vec<_> = self.0.keys().filter(|p| p.parent() == Some(path)).cloned().collect();
            let partial = children.len() > limit;
            Ok((children.into_iter().take(limit).collect(), partial))
        }
    }

    fn reference(session: &str) -> Reference {
        Reference {
            path: "/owned/target".into(),
            scope: false,
            session_id: session.into(),
            repository: "/repo".into(),
            carbon6_owned: true,
            active: false,
            archived_at_ms: Some(10),
            last_used_ms: Some(20),
        }
    }

    #[test]
    fn owned_scopes_cover_nested_worktree_and_scratch_outputs_but_not_the_workspace_itself() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().canonicalize().unwrap();
        let worktree = state.join("worktrees/repo/session");
        let scratch = state.join("scratch/scratch-session");
        for path in [worktree.join("nested/custom-output"), scratch.join("nested/target")] {
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(".rustc_info.json"), "{}").unwrap();
        }
        // Even a Rust marker accidentally written into a workspace must not
        // authorize removing that workspace as though it were only output.
        std::fs::write(worktree.join(".rustc_info.json"), "{}").unwrap();
        let mut a = reference("session");
        a.path = worktree.join("nested");
        let mut b = reference("scratch-session");
        b.path = scratch.join("nested");
        let (references, partial) = prepare_references(&state, vec![a, b]);
        assert!(!partial);
        assert!(references.iter().all(|r| r.scope && r.carbon6_owned));
        let scan = discover(&[], &references, &[], Limits::default());
        assert!(!scan.partial);
        let planned = plan(&scan.folders, &CleanupRequest::Archived, i64::MAX, DEFAULT_ARCHIVED_GRACE);
        assert_eq!(planned.len(), 2);
        assert!(planned.iter().all(|f| Path::new(&f.path) != worktree && Path::new(&f.path) != scratch));
        assert!(planned.iter().any(|f| f.path.ends_with("nested/custom-output")));
        assert!(planned.iter().any(|f| f.path.ends_with("nested/target")));
    }

    #[test]
    fn missing_cwd_children_preserve_idle_unarchived_and_active_shared_worktree_protection() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().canonicalize().unwrap();
        let worktree = state.join("worktrees/repo/archived");
        let target = worktree.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        let mut archived = reference("archived");
        archived.path = worktree.clone();
        let mut shared = reference("unarchived");
        shared.path = worktree.join("deleted-subdir/nested");
        shared.archived_at_ms = None;
        for active in [false, true] {
            shared.active = active;
            let (references, partial) = prepare_references(&state, vec![archived.clone(), shared.clone()]);
            assert!(!partial, "the existing managed top proves the scope of its missing child");
            assert_eq!(references.len(), 2);
            assert!(references.iter().all(|r| r.path == worktree && r.carbon6_owned));
            let scan = discover(&[], &references, &[], Limits::default());
            assert!(!scan.partial);
            let folder = scan.folders.iter().find(|f| Path::new(&f.path) == target).unwrap();
            assert_eq!(folder.archived_at_ms, None);
            assert_eq!(folder.active, active);
            assert!(folder.session_ids.iter().any(|id| id == "unarchived"));
            for request in [CleanupRequest::Archived, CleanupRequest::DiskPressure { bytes_to_free: u64::MAX }] {
                assert!(plan(&scan.folders, &request, i64::MAX, Duration::ZERO).is_empty());
            }
            if active {
                let request = CleanupRequest::Confirmed { path: target.clone(), confirmed: true };
                assert!(plan(&scan.folders, &request, i64::MAX, Duration::ZERO).is_empty());
            }
        }
    }

    #[test]
    fn only_proven_absent_idle_managed_tops_are_skipped_and_unknown_paths_are_partial() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(state.join("worktrees/repo")).unwrap();
        let mut removed = reference("removed");
        removed.path = state.join("worktrees/repo/removed/deleted-child");
        let (references, partial) = prepare_references(&state, vec![removed.clone()]);
        assert!(!partial && references.is_empty(), "a removed managed top must not permanently prevent unrelated cleanup");
        removed.active = true;
        let (references, partial) = prepare_references(&state, vec![removed]);
        assert!(partial, "an active writer with an absent top is not safe to ignore");
        assert_eq!(references.len(), 1);
        let mut unknown = reference("unknown");
        unknown.path = state.join("external/deleted-child");
        let (_, partial) = prepare_references(&state, vec![unknown]);
        assert!(partial, "missing external placement without a proven repository scope is uncertain");
        symlink(state.join("nowhere"), state.join("worktrees/repo/dangling")).unwrap();
        let mut dangling = reference("dangling");
        dangling.path = state.join("worktrees/repo/dangling/child");
        let (_, partial) = prepare_references(&state, vec![dangling]);
        assert!(partial, "a dangling managed-top symlink is not proof that the top is absent");
    }

    #[test]
    fn missing_external_cwd_child_preserves_a_proven_repository_scope() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().canonicalize().unwrap();
        let repository = state.join("external");
        std::fs::create_dir_all(repository.join(".git")).unwrap();
        let mut place = reference("external");
        place.path = repository.join("deleted-child");
        place.archived_at_ms = None;
        let (references, partial) = prepare_references(&state, vec![place]);
        assert!(!partial);
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].path, repository);
        assert!(!references[0].carbon6_owned);
        assert_eq!(references[0].archived_at_ms, None);
    }

    #[test]
    fn names_are_not_ownership_and_unarchived_idle_folders_never_auto_clean() {
        let mut fs = FakeFs::default();
        fs.add("/owned", true, 0, 1);
        fs.add("/owned/target", true, 0, 1);
        fs.add("/owned/target/source", false, 10, 2);
        let mut scope = reference("session");
        scope.path = "/owned".into();
        scope.scope = true;
        let scan = discover_with(&fs, &[], &[scope], &[], Limits::default(), || Duration::ZERO);
        assert_eq!(scan.folders.len(), 1);
        assert!(scan.folders[0].partial, "a source folder named target lacks Rust output markers");
        assert!(plan(&scan.folders, &CleanupRequest::DiskPressure { bytes_to_free: u64::MAX }, i64::MAX, Duration::ZERO).is_empty());
        let mut idle = folder("/owned/target", 1);
        idle.archived_at_ms = None;
        assert!(plan(&[idle], &CleanupRequest::DiskPressure { bytes_to_free: u64::MAX }, i64::MAX, Duration::ZERO).is_empty());
    }

    fn folder(path: &str, used: i64) -> MachineBuildFolder {
        MachineBuildFolder {
            path: path.into(),
            bytes: 100,
            last_used_ms: Some(used),
            carbon6_owned: true,
            archived_at_ms: Some(10),
            ..Default::default()
        }
    }

    #[test]
    fn sizes_and_shared_attribution_include_descendants_and_latest_use() {
        let mut fs = FakeFs::default();
        fs.add("/owned/target", true, 0, 1);
        fs.add("/owned/target/debug", true, 0, 2);
        fs.add("/owned/target/debug/incremental", true, 0, 3);
        fs.add("/owned/target/debug/incremental/a", false, 40, 60);
        fs.add("/owned/target/debug/app", false, 80, 50);
        let a = reference("a");
        let mut b = reference("b");
        b.active = true;
        b.archived_at_ms = None;
        let scan = discover_with(&fs, &[], &[a, b], &[], Limits::default(), || Duration::ZERO);
        assert!(!scan.partial);
        let target = scan.folders.iter().find(|f| f.path == "/owned/target").unwrap();
        assert_eq!(target.bytes, 120);
        assert_eq!(target.last_used_ms, Some(60));
        assert_eq!(target.session_ids, ["a", "b"]);
        assert_eq!(target.repositories, ["/repo"]);
        assert!(target.active && target.carbon6_owned);
        assert_eq!(target.archived_at_ms, None);
        let incremental = scan.folders.iter().find(|f| f.incremental).unwrap();
        assert_eq!(incremental.bytes, 40);
        assert!(incremental.active);
    }

    #[test]
    fn marker_names_only_prove_output_when_the_entry_types_match_without_symlinks() {
        let mut scope = reference("session");
        scope.path = "/owned".into();
        scope.scope = true;
        for (directory, symlink) in [(true, false), (false, true), (false, false)] {
            let mut fs = FakeFs::default();
            fs.add("/owned", true, 0, 1);
            fs.add("/owned/custom-output", true, 0, 1);
            fs.add("/owned/custom-output/.rustc_info.json", directory, 2, 1);
            fs.0.get_mut(Path::new("/owned/custom-output/.rustc_info.json")).unwrap().symlink = symlink;
            let scan = discover_with(&fs, &[], std::slice::from_ref(&scope), &[], Limits::default(), || Duration::ZERO);
            let planned = plan(&scan.folders, &CleanupRequest::Archived, i64::MAX, DEFAULT_ARCHIVED_GRACE);
            assert_eq!(planned.len(), usize::from(!directory && !symlink));
        }
        let mut fs = FakeFs::default();
        fs.add("/owned", true, 0, 1);
        fs.add("/owned/debug", true, 0, 1);
        for name in [".fingerprint", "deps", "build"] {
            fs.add(&format!("/owned/debug/{name}"), false, 2, 1);
        }
        let scan = discover_with(&fs, &[], &[scope], &[], Limits::default(), || Duration::ZERO);
        assert!(scan.folders.is_empty(), "ordinary files with profile-marker names must not prove a build root");
    }

    #[test]
    fn custom_target_roots_are_found_by_rust_markers_but_never_owned() {
        let mut fs = FakeFs::default();
        fs.add("/targets", true, 0, 1);
        fs.add("/targets/session-a", true, 0, 1);
        fs.add("/targets/session-a/.rustc_info.json", false, 10, 2);
        fs.add("/targets/session-b", true, 0, 1);
        fs.add("/targets/session-b/debug", true, 0, 1);
        for name in [".fingerprint", "build", "deps", "incremental"] {
            fs.add(&format!("/targets/session-b/debug/{name}"), true, 0, 1);
        }
        fs.add("/targets/session-b/aaa", false, 20, 9);
        fs.add("/targets/session-b/debug/deps/a", false, 30, 8);
        let scan = discover_with(&fs, &["/targets".into()], &[], &[], Limits::default(), || Duration::ZERO);
        assert!(!scan.partial);
        assert_eq!(scan.folders.len(), 3);
        assert!(scan.folders.iter().all(|f| !f.carbon6_owned));
        assert_eq!(scan.folders.iter().find(|f| f.path == "/targets/session-b").unwrap().bytes, 50);
        assert!(plan(&scan.folders, &CleanupRequest::DiskPressure { bytes_to_free: u64::MAX }, i64::MAX, Duration::ZERO).is_empty());
    }

    #[test]
    fn entry_time_depth_folder_and_root_limits_fail_closed_without_sleeping() {
        let mut fs = FakeFs::default();
        fs.add("/owned/target", true, 0, 1);
        fs.add("/owned/target/debug", true, 0, 2);
        fs.add("/owned/target/debug/a", false, 10, 3);
        for limits in [
            Limits { max_entries: 2, ..Limits::default() },
            Limits { max_depth: 0, ..Limits::default() },
            Limits { max_roots: 0, ..Limits::default() },
            Limits { max_folders: 0, ..Limits::default() },
        ] {
            let scan = discover_with(&fs, &[], &[reference("a")], &[], limits, || Duration::ZERO);
            assert!(scan.partial);
            assert!(scan.entries <= limits.max_entries);
            assert!(scan.folders.iter().all(|f| f.partial));
            assert!(plan(&scan.folders, &CleanupRequest::Archived, i64::MAX, Duration::ZERO).is_empty());
        }
        let clock = std::cell::Cell::new(Duration::ZERO);
        let scan = discover_with(&fs, &[], &[reference("a")], &[], Limits::default(), || {
            let now = clock.get();
            clock.set(now + Duration::from_secs(5));
            now
        });
        assert!(scan.partial);
        assert_eq!(scan.entries, 0);
    }

    #[test]
    fn symlinks_and_duplicate_roots_never_inflate_size_or_traverse_outside() {
        let mut fs = FakeFs::default();
        fs.add("/owned/target", true, 0, 1);
        fs.add("/owned/target/a", false, 12, 2);
        fs.add("/owned/target/link", true, 0, 3);
        fs.0.get_mut(Path::new("/owned/target/link")).unwrap().symlink = true;
        fs.add("/owned/target/link/outside", false, 999, 5);
        let scan =
            discover_with(&fs, &["/owned/target".into(), "/owned/target/a".into()], &[reference("a")], &[], Limits::default(), || {
                Duration::ZERO
            });
        assert!(!scan.partial);
        assert_eq!(scan.folders[0].bytes, 12);
        assert_eq!(scan.folders[0].last_used_ms, Some(20));
    }

    #[test]
    fn nested_filesystems_are_not_traversed_or_automatically_cleaned() {
        let mut fs = FakeFs::default();
        fs.add("/owned/target", true, 0, 1);
        fs.add("/owned/target/.rustc_info.json", false, 2, 1);
        fs.add("/owned/target/mounted", true, 0, 1);
        fs.0.get_mut(Path::new("/owned/target/mounted")).unwrap().device = 2;
        fs.add("/owned/target/mounted/keep", false, 999, 1);
        let scan = discover_with(&fs, &[], &[reference("s")], &[], Limits::default(), || Duration::ZERO);
        assert!(scan.partial);
        assert_eq!(scan.folders[0].bytes, 2);
        assert!(plan(&scan.folders, &CleanupRequest::Archived, i64::MAX, Duration::ZERO).is_empty());
    }

    #[test]
    fn archive_grace_is_three_days_and_restarts_at_latest_use() {
        let grace_ms = DEFAULT_ARCHIVED_GRACE.as_millis() as i64;
        let candidate = folder("/owned/target", 20);
        assert!(plan(std::slice::from_ref(&candidate), &CleanupRequest::Archived, 20 + grace_ms - 1, DEFAULT_ARCHIVED_GRACE).is_empty());
        assert_eq!(plan(std::slice::from_ref(&candidate), &CleanupRequest::Archived, 20 + grace_ms, DEFAULT_ARCHIVED_GRACE).len(), 1);
        assert!(plan(&[candidate], &CleanupRequest::Archived, 19, Duration::ZERO).is_empty());
    }

    #[test]
    fn pressure_uses_owned_oldest_first_and_does_not_double_count_incremental() {
        let old = folder("/owned/old/target", 1);
        let incremental = folder("/owned/old/target/debug/incremental", 0);
        let new = folder("/owned/new/target", 3);
        let mut external = folder("/external/target", 0);
        external.carbon6_owned = false;
        let mut active = folder("/owned/active/target", 0);
        active.active = true;
        let planned = plan(
            &[new, external, active, incremental, old],
            &CleanupRequest::DiskPressure { bytes_to_free: 100 },
            i64::MAX,
            DEFAULT_ARCHIVED_GRACE,
        );
        assert_eq!(planned.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["/owned/old/target"]);
        let result = cleanup_with(&planned, true, |_| false, |_| Ok(()));
        assert_eq!(result.bytes, 100);
        assert!(result.notice.as_ref().unwrap().contains("/owned/old/target"));
    }

    #[test]
    fn external_and_incremental_require_exact_explicit_confirmation() {
        let mut external = folder("/external/target", 1);
        external.carbon6_owned = false;
        let mut incremental = external.clone();
        incremental.path = "/external/target/debug/incremental".into();
        incremental.incremental = true;
        let folders = [external, incremental];
        for path in ["/external", "/external/target", "/external/target/debug/incremental"] {
            assert!(plan(&folders, &CleanupRequest::Confirmed { path: path.into(), confirmed: false }, 100, Duration::ZERO).is_empty());
        }
        let planned =
            plan(&folders, &CleanupRequest::Confirmed { path: folders[1].path.clone().into(), confirmed: true }, 100, Duration::ZERO);
        assert_eq!(planned.len(), 1);
        assert!(planned[0].incremental);
        let mut incomplete = folders[1].clone();
        incomplete.partial = true;
        let request = CleanupRequest::Confirmed { path: incomplete.path.clone().into(), confirmed: true };
        assert!(plan(&[incomplete], &request, 100, Duration::ZERO).is_empty(), "confirmation must not override an incomplete scan");
    }

    #[test]
    fn directly_selected_external_incremental_is_reproved_from_ancestor_without_broadening_removal() {
        for (root, profile) in [("/external/target", "debug"), ("/external/custom-cache", "triple/debug")] {
            let mut fs = FakeFs::default();
            fs.add(root, true, 0, 1);
            fs.add(&format!("{root}/.rustc_info.json"), false, 2, 1);
            if profile.contains('/') {
                fs.add(&format!("{root}/triple"), true, 0, 1);
            }
            fs.add(&format!("{root}/{profile}"), true, 0, 1);
            fs.add(&format!("{root}/{profile}/app"), false, 80, 1);
            let incremental = PathBuf::from(format!("{root}/{profile}/incremental"));
            fs.add(incremental.to_str().unwrap(), true, 0, 1);
            fs.add(&format!("{}/object", incremental.display()), false, 40, 1);
            let scan_root = incremental_scan_root(&fs, &incremental);
            assert_eq!(scan_root, Path::new(root));
            let scan = discover_with(&fs, &[scan_root], &[], &[], Limits::default(), || Duration::ZERO);
            assert!(!scan.partial);
            let request = CleanupRequest::Confirmed { path: incremental.clone(), confirmed: true };
            let planned = plan(&scan.folders, &request, 100, Duration::ZERO);
            assert_eq!(planned.len(), 1);
            assert!(planned[0].incremental && !planned[0].carbon6_owned && !planned[0].partial);
            assert_eq!(Path::new(&planned[0].path), incremental);
            assert_eq!(planned[0].bytes, 40, "whole target and sibling binary are not selected");
        }
    }

    #[test]
    fn fresh_active_check_and_removal_errors_never_claim_reclaimed_bytes() {
        let planned = [folder("/owned/target", 1)];
        let result = cleanup_with(&planned, true, |_| true, |_| panic!("active folders must not reach the filesystem"));
        assert!(result.removed.is_empty() && result.notice.is_none());
        assert_eq!(result.bytes, 0);
        let result = cleanup_with(&planned, true, |_| false, |_| Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        assert_eq!(result.bytes, 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.notice.is_none());
    }

    #[test]
    fn local_removal_rejects_symlink_ancestors_and_unlinks_only_child_links() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), "keep").unwrap();
        symlink(&outside, root.join("alias")).unwrap();
        assert!(remove_local(&root.join("alias")).is_err());
        assert!(remove_local(&root.join("alias/keep")).is_err());
        let target = root.join("target");
        std::fs::create_dir_all(target.join("debug/incremental")).unwrap();
        symlink(&outside, target.join("debug/link")).unwrap();
        std::fs::write(target.join("debug/incremental/object"), "compiled").unwrap();
        remove_local(&target).unwrap();
        assert!(!target.exists());
        assert!(outside.join("keep").exists());
        assert!(remove_local(Path::new("/")).is_err());
        assert!(remove_local(Path::new("/tmp/../somewhere")).is_err());
    }

    #[tokio::test]
    async fn cleanup_exclusion_cannot_queue_behind_a_turn_or_race_new_shell_reservations() {
        let lock = std::sync::Arc::new(tokio::sync::RwLock::new(()));
        let turn = lock.clone().read_owned().await;
        assert!(lock.clone().try_write_owned().is_err());
        // A failed cleanup attempt must not queue a writer and deadlock the
        // turn's nested shell reservation reader.
        let shell = lock.clone().try_read_owned().unwrap();
        drop((turn, shell));
        let cleanup = lock.clone().try_write_owned().unwrap();
        assert!(lock.clone().try_read_owned().is_err());
        drop(cleanup);
        assert!(lock.try_read_owned().is_ok());
    }
}
