//! Prepare a local scratch copy before recording a placement change.

use nix::libc;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

const MAX_BYTES: u64 = 200_000_000;
const MAX_ENTRIES: usize = 4096; // Includes directories, even empty ones.
const MAX_DEPTH: usize = 32;

type Identity = (libc::dev_t, libc::ino_t);

struct Entry {
    name: CString,
    path: String,
    source: Identity,
    kind: Kind,
}

enum Kind {
    File(u64),
    Directory { entries: Vec<Entry>, existing: Option<Identity> },
}

struct Created {
    path: String,
    parent: Identity,
    identity: Identity,
    directory: bool,
}

/// Copies already on disk, to be kept only after the placement event is recorded.
pub(crate) struct PreparedCopy {
    destination: File,
    created: Vec<Created>,
    paths: Vec<String>,
}

impl PreparedCopy {
    /// Both roots must be existing directories reached without symlinks or `..`.
    /// The caller should pass the resolved workspace paths if a system alias
    /// (such as macOS's `/tmp`) is part of the original spelling.
    pub(crate) fn prepare(source: &Path, destination: &Path) -> Result<Self, String> {
        let (source, source_chain) = open_root(source).map_err(|e| format!("opening scratch directory: {e}"))?;
        let (destination, destination_chain) = open_root(destination).map_err(|e| format!("opening destination directory: {e}"))?;
        let source_id = *source_chain.last().expect("root directory included");
        let destination_id = *destination_chain.last().expect("root directory included");
        if source_chain.contains(&destination_id) || destination_chain.contains(&source_id) {
            return Err("scratch and destination directories overlap".into());
        }

        let mut scan = Scan {
            remaining_entries: MAX_ENTRIES,
            remaining_bytes: MAX_BYTES,
            directories: HashSet::from([source_id]),
            destination_chain: &destination_chain,
            paths: Vec::new(),
        };
        let mut entries = scan.read(&source, "", 0)?;
        check_destination(&destination, &mut entries, &scan.directories, &source_chain, &mut HashSet::from([destination_id]))?;
        scan.paths.sort_unstable();

        let mut prepared = Self { destination, created: Vec::new(), paths: scan.paths };
        copy_entries(&source, &prepared.destination, destination_id, &entries, &mut prepared.created)?;
        Ok(prepared)
    }

    /// Sorted workspace-relative paths of the copied regular files.
    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }

    pub(crate) fn commit(mut self) {
        self.created.clear();
    }
}

impl Drop for PreparedCopy {
    fn drop(&mut self) {
        // Remove descendants before their parents, without recursive deletion.
        // If another process moves or replaces an entry, prefer leaving the
        // copy behind to intentionally deleting something we didn't create.
        for created in self.created.iter().rev() {
            let _ = remove_created(&self.destination, created);
        }
    }
}

struct Scan<'a> {
    remaining_entries: usize,
    remaining_bytes: u64,
    directories: HashSet<Identity>,
    destination_chain: &'a [Identity],
    paths: Vec<String>,
}

impl Scan<'_> {
    fn read(&mut self, directory: &File, prefix: &str, depth: usize) -> Result<Vec<Entry>, String> {
        let names = directory_names(directory, &mut self.remaining_entries).map_err(|e| at("reading scratch directory", prefix, e))?;
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let text = name.to_str().map_err(|_| format!("non-UTF-8 name in scratch directory {prefix:?}"))?;
            if text.is_empty() || text.contains('/') || matches!(text, "." | "..") {
                return Err(format!("invalid scratch entry name {text:?}"));
            }
            let path = if prefix.is_empty() { text.to_owned() } else { format!("{prefix}/{text}") };
            let stat = stat_at(directory, &name).map_err(|e| at("inspecting scratch entry", &path, e))?;
            let source = identity(&stat);
            let kind = match file_type(&stat) {
                libc::S_IFREG => {
                    let size = u64::try_from(stat.st_size).map_err(|_| format!("invalid size for scratch file {path:?}"))?;
                    self.remaining_bytes = self.remaining_bytes.checked_sub(size).ok_or("scratch copy exceeds the 200 MB size limit")?;
                    self.paths.push(path.clone());
                    Kind::File(size)
                }
                libc::S_IFDIR => {
                    if depth >= MAX_DEPTH {
                        return Err(format!("scratch copy exceeds {MAX_DEPTH} directory levels at {path:?}"));
                    }
                    if self.destination_chain.contains(&source) || !self.directories.insert(source) {
                        return Err(format!("overlapping scratch or destination directory at {path:?}"));
                    }
                    let child = checked_directory(directory, &name, source).map_err(|e| at("opening scratch directory", &path, e))?;
                    Kind::Directory { entries: self.read(&child, &path, depth + 1)?, existing: None }
                }
                _ => return Err(format!("scratch entry {path:?} is a symlink or special file")),
            };
            entries.push(Entry { name, path, source, kind });
        }
        Ok(entries)
    }
}

fn check_destination(
    directory: &File,
    entries: &mut [Entry],
    source_dirs: &HashSet<Identity>,
    source_chain: &[Identity],
    seen: &mut HashSet<Identity>,
) -> Result<(), String> {
    for entry in entries {
        let stat = match stat_at(directory, &entry.name) {
            Ok(stat) => stat,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(at("inspecting destination", &entry.path, e)),
        };
        match &mut entry.kind {
            Kind::Directory { entries, existing } if file_type(&stat) == libc::S_IFDIR => {
                let id = identity(&stat);
                if source_dirs.contains(&id) || source_chain.contains(&id) {
                    return Err(format!("destination directory at {:?} overlaps scratch", entry.path));
                }
                if !seen.insert(id) {
                    return Err(format!("destination conflict at {:?}: directory already appears in manifest", entry.path));
                }
                let child =
                    checked_directory(directory, &entry.name, id).map_err(|e| at("opening destination directory", &entry.path, e))?;
                check_destination(&child, entries, source_dirs, source_chain, seen)?;
                *existing = Some(id);
            }
            _ => return Err(format!("destination conflict at {:?}", entry.path)),
        }
    }
    Ok(())
}

fn copy_entries(source: &File, destination: &File, parent: Identity, entries: &[Entry], created: &mut Vec<Created>) -> Result<(), String> {
    for entry in entries {
        let error = |e| at("copying scratch entry", &entry.path, e);
        match &entry.kind {
            Kind::Directory { entries, existing } => {
                let (dst_child, id) = if let Some(id) = existing {
                    (checked_directory(destination, &entry.name, *id).map_err(error)?, *id)
                } else {
                    // mkdirat fails rather than using a directory created since preflight.
                    status(unsafe { libc::mkdirat(destination.as_raw_fd(), entry.name.as_ptr(), 0o755) }).map_err(error)?;
                    let stat = stat_at(destination, &entry.name).map_err(error)?;
                    if file_type(&stat) != libc::S_IFDIR {
                        return Err(error(changed()));
                    }
                    let id = identity(&stat);
                    created.push(Created { path: entry.path.clone(), parent, identity: id, directory: true });
                    (checked_directory(destination, &entry.name, id).map_err(error)?, id)
                };
                let src_child = checked_directory(source, &entry.name, entry.source).map_err(error)?;
                copy_entries(&src_child, &dst_child, id, entries, created)?;
            }
            Kind::File(size) => {
                check_file(&stat_at(source, &entry.name).map_err(error)?, entry.source, *size).map_err(error)?;
                // NONBLOCK prevents a substituted FIFO from blocking the worker.
                let src = open_at(source.as_raw_fd(), &entry.name, libc::O_RDONLY | libc::O_NONBLOCK, 0).map_err(error)?;
                let before = stat_fd(&src).map_err(error)?;
                check_file(&before, entry.source, *size).map_err(error)?;
                let mut dst =
                    open_at(destination.as_raw_fd(), &entry.name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600).map_err(error)?;
                let id = identity(&stat_fd(&dst).map_err(error)?);
                created.push(Created { path: entry.path.clone(), parent, identity: id, directory: false });
                if io::copy(&mut (&src).take(*size), &mut dst).map_err(error)? != *size {
                    return Err(error(changed()));
                }
                check_file(&stat_fd(&src).map_err(error)?, entry.source, *size).map_err(error)?;
                // Preserve ordinary permission bits (not set-id bits), so a
                // script copied out of scratch remains executable.
                status(unsafe { libc::fchmod(dst.as_raw_fd(), before.st_mode & 0o777) }).map_err(error)?;
            }
        }
    }
    Ok(())
}

fn check_file(stat: &libc::stat, expected: Identity, size: u64) -> io::Result<()> {
    if file_type(stat) != libc::S_IFREG || identity(stat) != expected || u64::try_from(stat.st_size).ok() != Some(size) {
        return Err(changed());
    }
    Ok(())
}

fn checked_directory(directory: &File, name: &CStr, expected: Identity) -> io::Result<File> {
    let child = open_directory(directory.as_raw_fd(), name)?;
    if identity(&stat_fd(&child)?) != expected {
        return Err(changed());
    }
    Ok(child)
}

fn remove_created(root: &File, entry: &Created) -> io::Result<()> {
    let mut names = entry.path.split('/').peekable();
    let mut opened = None;
    while let Some(name) = names.next() {
        let name = c_name(name.as_bytes())?;
        let parent = opened.as_ref().unwrap_or(root);
        if names.peek().is_some() {
            opened = Some(open_directory(parent.as_raw_fd(), &name)?);
            continue;
        }
        if identity(&stat_fd(parent)?) != entry.parent || identity(&stat_at(parent, &name)?) != entry.identity {
            return Err(changed());
        }
        let flags = if entry.directory { libc::AT_REMOVEDIR } else { 0 };
        return status(unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) });
    }
    Ok(())
}

fn open_root(path: &Path) -> io::Result<(File, Vec<Identity>)> {
    let absolute = if path.is_absolute() { path.to_owned() } else { std::env::current_dir()?.join(path) };
    let mut root = open_directory(libc::AT_FDCWD, c"/")?;
    let mut chain = vec![identity(&stat_fd(&root)?)];
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                root = open_directory(root.as_raw_fd(), &c_name(name.as_bytes())?)?;
                chain.push(identity(&stat_fd(&root)?));
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "parent traversal is not allowed")),
        }
    }
    Ok((root, chain))
}

fn open_directory(parent: libc::c_int, name: &CStr) -> io::Result<File> {
    open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}

fn open_at(parent: libc::c_int, name: &CStr, flags: libc::c_int, mode: libc::mode_t) -> io::Result<File> {
    // All callers use a held directory descriptor and a single name (or `/` for the root).
    // The variadic mode argument uses C integer promotion on macOS.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW, libc::c_uint::from(mode)) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
}

fn stat_fd(file: &File) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::uninit();
    status(unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) })?;
    Ok(unsafe { stat.assume_init() })
}

fn stat_at(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::uninit();
    status(unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(unsafe { stat.assume_init() })
}

fn file_type(stat: &libc::stat) -> libc::mode_t {
    stat.st_mode & libc::S_IFMT
}

fn identity(stat: &libc::stat) -> Identity {
    (stat.st_dev, stat.st_ino)
}

fn status(result: libc::c_int) -> io::Result<()> {
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn c_name(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn changed() -> io::Error {
    io::Error::other("filesystem entry changed during scratch copy")
}

fn at(action: &str, path: &str, error: io::Error) -> String {
    format!("{action} {path:?}: {error}")
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn directory_names(directory: &File, remaining: &mut usize) -> io::Result<Vec<CString>> {
    let reader = open_directory(directory.as_raw_fd(), c".")?;
    let stream = unsafe { libc::fdopendir(reader.as_raw_fd()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _ = reader.into_raw_fd(); // closedir owns the descriptor on success.
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        // readdir uses the same null return for EOF and error. Reset the
        // calling thread's errno to distinguish those before each read.
        #[cfg(target_vendor = "apple")]
        let errno = unsafe { libc::__error() };
        #[cfg(not(target_vendor = "apple"))]
        let errno = unsafe { libc::__errno_location() };
        unsafe { *errno = 0 };
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = unsafe { *errno };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name == c"." || name == c".." {
            continue;
        }
        *remaining = remaining.checked_sub(1).ok_or_else(|| io::Error::other(format!("scratch copy exceeds {MAX_ENTRIES} entries")))?;
        names.push(name.to_owned());
    }
    names.sort_unstable();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    fn roots() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let source = parent.join("source");
        let destination = parent.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        (temp, source, destination)
    }

    #[test]
    fn copies_nested_files_and_commits_sorted_manifest() {
        let (_temp, source, destination) = roots();
        fs::create_dir_all(source.join("dir/empty")).unwrap();
        fs::write(source.join("z"), "last").unwrap();
        fs::write(source.join("dir/a"), "first").unwrap();
        let copy = PreparedCopy::prepare(&source, &destination).unwrap();
        assert_eq!(copy.paths(), ["dir/a", "z"]);
        assert_eq!(fs::read(destination.join("dir/a")).unwrap(), b"first");
        copy.commit();
        assert!(destination.join("dir/empty").is_dir());
        assert_eq!(fs::read(destination.join("z")).unwrap(), b"last");
        assert_eq!(fs::read(source.join("z")).unwrap(), b"last");
    }

    #[test]
    fn refuses_conflicts_before_copying() {
        let (_temp, source, destination) = roots();
        fs::write(source.join("a"), "first").unwrap();
        fs::create_dir(source.join("z")).unwrap();
        fs::create_dir(destination.join("z")).unwrap();
        fs::write(source.join("z/file"), "new").unwrap();
        fs::write(destination.join("z/file"), "old").unwrap();
        let error = PreparedCopy::prepare(&source, &destination).err().unwrap();
        assert!(error.contains("z/file"), "{error}");
        assert!(!destination.join("a").exists());
        assert_eq!(fs::read(destination.join("z/file")).unwrap(), b"old");
    }

    #[test]
    fn preserves_executable_permissions_without_set_id_bits() {
        let (_temp, source, destination) = roots();
        fs::write(source.join("run"), "#!/bin/sh\ntrue\n").unwrap();
        fs::set_permissions(source.join("run"), fs::Permissions::from_mode(0o4750)).unwrap();
        PreparedCopy::prepare(&source, &destination).unwrap().commit();
        assert_eq!(fs::metadata(destination.join("run")).unwrap().permissions().mode() & 0o7777, 0o750);
    }

    #[test]
    fn refuses_source_and_destination_symlinks() {
        let (_temp, source, destination) = roots();
        let target = source.join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, source.join("link")).unwrap();
        assert!(PreparedCopy::prepare(&source, &destination).is_err());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
        fs::remove_file(source.join("link")).unwrap();
        symlink(&target, destination.join("target")).unwrap();
        assert!(PreparedCopy::prepare(&source, &destination).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert!(PreparedCopy::prepare(&source, &destination.join("target")).is_err());
    }

    #[test]
    fn refuses_special_files() {
        let (_temp, source, destination) = roots();
        let (root, _) = open_root(&source).unwrap();
        status(unsafe { libc::mkfifoat(root.as_raw_fd(), c"fifo".as_ptr(), 0o600) }).unwrap();
        assert!(PreparedCopy::prepare(&source, &destination).is_err());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn refuses_oversize_aggregate_using_sparse_files() {
        let (_temp, source, destination) = roots();
        File::create(source.join("a")).unwrap().set_len(MAX_BYTES / 2).unwrap();
        File::create(source.join("b")).unwrap().set_len(MAX_BYTES / 2 + 1).unwrap();
        let error = PreparedCopy::prepare(&source, &destination).err().unwrap();
        assert!(error.contains("size limit"), "{error}");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn directory_reads_stop_at_entry_budget() {
        let (_temp, source, _) = roots();
        fs::create_dir(source.join("a")).unwrap();
        fs::write(source.join("b"), "").unwrap();
        let (root, _) = open_root(&source).unwrap();
        assert_eq!(directory_names(&root, &mut 2).unwrap().len(), 2);
        assert!(directory_names(&root, &mut 1).is_err());
    }

    #[test]
    fn rollback_leaves_existing_directories_and_files() {
        let (_temp, source, destination) = roots();
        fs::create_dir_all(source.join("shared/new")).unwrap();
        fs::create_dir(destination.join("shared")).unwrap();
        fs::write(source.join("shared/new/copied"), "new").unwrap();
        fs::write(source.join("shared/copied"), "new").unwrap();
        fs::write(destination.join("shared/keep"), "old").unwrap();
        let copy = PreparedCopy::prepare(&source, &destination).unwrap();
        assert!(destination.join("shared/new/copied").is_file());
        drop(copy);
        assert!(!destination.join("shared/new").exists());
        assert!(!destination.join("shared/copied").exists());
        assert_eq!(fs::read(destination.join("shared/keep")).unwrap(), b"old");
        assert!(source.join("shared/new/copied").is_file());
    }

    #[test]
    fn refuses_overlapping_roots_and_parent_traversal() {
        let (_temp, source, destination) = roots();
        let nested = source.join("nested");
        fs::create_dir(&nested).unwrap();
        assert!(PreparedCopy::prepare(&source, &source).is_err());
        assert!(PreparedCopy::prepare(&source, &nested).is_err());
        assert!(PreparedCopy::prepare(&nested, &source).is_err());
        assert!(PreparedCopy::prepare(&source, &destination.join("../destination")).is_err());
    }
}
