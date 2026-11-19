//! Detect checkpoint Git children left behind by a pre-sidecar worker.
//!
//! Callers must already exclude other workers and sidecar participants. This
//! assumes the workers share a user and process view; it is not a check for
//! arbitrary Git invocations or writers in another PID namespace.

use std::path::Path;

/// An error means ownership is uncertain, not that the index lock is stale.
pub(crate) async fn running(git_dir: &Path) -> anyhow::Result<bool> {
    let git_dir = git_dir.to_owned();
    tokio::task::spawn_blocking(move || scan(&git_dir)).await?
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use anyhow::Context;
    use nix::libc;

    pub(super) fn scan(git_dir: &Path) -> anyhow::Result<bool> {
        let target = fs::metadata(git_dir).with_context(|| format!("identifying checkpoint repo {}", git_dir.display()))?;
        let worker = std::env::current_exe()?;
        // SAFETY: geteuid takes no arguments and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let mut uncertain = None;
        for process in processes(uid)? {
            let Process { pid, name } = process;
            // Read argv even for unrecognized executable names (e.g. a Git
            // launcher). Only fail on unreadable argv for plausible owners.
            let candidate = match command_line(pid) {
                Ok(Some(path)) => path,
                Ok(None) => continue,
                Err(err) => {
                    let name = Path::new(&name).file_name().unwrap_or_default();
                    if plausible_owner(name, worker.file_name()) && still_live(pid) {
                        uncertain = Some(err.context(format!("reading checkpoint Git argv for PID {pid}")));
                    }
                    continue;
                }
            };
            let resolved = if candidate.is_absolute() { Ok(candidate) } else { cwd(pid).map(|cwd| cwd.join(candidate)) };
            match resolved.and_then(fs::metadata) {
                // Filesystem identity handles symlinks, case-insensitive mounts,
                // and alternate spellings such as /tmp vs /private/tmp.
                Ok(repo) if (repo.dev(), repo.ino()) == (target.dev(), target.ino()) => return Ok(true),
                Ok(_) => {}
                Err(err) if still_live(pid) => {
                    uncertain = Some(anyhow::Error::from(err).context(format!("identifying checkpoint repo for PID {pid}")));
                }
                Err(_) => {}
            }
        }
        uncertain.map_or(Ok(false), Err)
    }

    fn plausible_owner(name: &OsStr, worker: Option<&OsStr>) -> bool {
        let name = name.as_bytes();
        name == b"git"
            || name == b"c6w"
            || worker.is_some_and(|worker| {
                let worker = worker.as_bytes();
                name == worker || (cfg!(target_os = "linux") && name.len() == 15 && worker.starts_with(name))
            })
    }

    struct Process {
        pid: i32,
        name: std::ffi::OsString,
    }

    fn git_dir<'a>(mut argv: impl Iterator<Item = &'a [u8]>) -> Option<PathBuf> {
        argv.next()?;
        if argv.next()? != b"--git-dir" {
            return None;
        }
        let dir = argv.next()?;
        if dir.is_empty() || argv.next()? != b"--work-tree" || argv.next()?.is_empty() {
            return None;
        }
        Some(PathBuf::from(OsStr::from_bytes(dir)))
    }

    fn gone(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(libc::ESRCH | libc::ENOENT))
    }

    #[cfg(target_os = "linux")]
    fn processes(uid: u32) -> anyhow::Result<Vec<Process>> {
        let mut found = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            if pid == std::process::id() as i32 {
                continue;
            }
            let status = match fs::read(entry.path().join("status")) {
                Ok(status) => status,
                Err(err) if gone(&err) => continue,
                Err(err) => return Err(err).with_context(|| format!("reading /proc/{pid}/status")),
            };
            let euid = status_field(&status, b"Uid:")
                .and_then(|uids| std::str::from_utf8(uids).ok()?.split_ascii_whitespace().nth(1)?.parse::<u32>().ok());
            if euid == Some(uid) && !dead(&status) {
                let name = status_field(&status, b"Name:").unwrap_or_default();
                found.push(Process { pid, name: OsStr::from_bytes(name).to_owned() });
            }
        }
        Ok(found)
    }

    #[cfg(target_os = "linux")]
    fn status_field<'a>(status: &'a [u8], field: &[u8]) -> Option<&'a [u8]> {
        status.split(|&byte| byte == b'\n').find_map(|line| line.strip_prefix(field).map(<[u8]>::trim_ascii_start))
    }

    #[cfg(target_os = "linux")]
    fn dead(status: &[u8]) -> bool {
        matches!(status_field(status, b"State:").and_then(|s| s.first()), Some(b'Z' | b'X' | b'x'))
    }

    #[cfg(target_os = "linux")]
    fn still_live(pid: i32) -> bool {
        match fs::read(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat_dead(&stat),
            Err(err) => !gone(&err),
        }
    }

    #[cfg(target_os = "linux")]
    fn stat_dead(stat: &[u8]) -> bool {
        // comm (field 2) may itself contain whitespace and parentheses.
        let Some(end) = stat.windows(2).rposition(|pair| pair == b") ") else { return false };
        let mut fields = stat[end + 2..].split(u8::is_ascii_whitespace).filter(|field| !field.is_empty());
        if matches!(fields.next(), Some(b"Z" | b"X" | b"x")) {
            return true;
        }
        // Linux can clear cmdline before the task reaches zombie state. A
        // running-state task with PF_EXITING set cannot return to Git code.
        // Flags are field 9 (index 6 after comm, five more after state).
        const PF_EXITING: u32 = 0x4;
        fields.nth(5).and_then(|flags| std::str::from_utf8(flags).ok()?.parse::<u32>().ok()).is_some_and(|flags| flags & PF_EXITING != 0)
    }

    #[cfg(all(test, target_os = "linux"))]
    #[test]
    fn stat_distinguishes_exit_from_an_empty_argv_during_exec() {
        assert!(stat_dead(b"42 (git) R 1 2 3 0 -1 4194308 10\n"));
        assert!(!stat_dead(b"42 (git) R 1 2 3 0 -1 4194304 4\n"));
        assert!(stat_dead(b"42 (a) (b) c)) R 1 2 3 0 -1 4 10\n"));
        assert!(!stat_dead(b"42 (x) Z 1 2 3) R 4 2 3 0 -1 0 10\n"));
        assert!(stat_dead(b"42 (git) Z 1 2 3 0 -1 0 10\n"));
        assert!(!stat_dead(b"42 (git) R 1 2 3 0 -1 invalid\n"));
        assert!(!stat_dead(b"42 (git) R 1 2 3 0 -1\n"));
    }

    #[cfg(target_os = "linux")]
    fn command_line(pid: i32) -> anyhow::Result<Option<PathBuf>> {
        let data = fs::read(format!("/proc/{pid}/cmdline"))?;
        anyhow::ensure!(data.last() == Some(&0), "empty or incomplete process argv");
        Ok(git_dir(data.split(|&byte| byte == 0)))
    }

    #[cfg(target_os = "linux")]
    fn cwd(pid: i32) -> io::Result<PathBuf> {
        Ok(PathBuf::from(format!("/proc/{pid}/cwd")))
    }

    #[cfg(target_os = "macos")]
    fn processes(uid: u32) -> anyhow::Result<Vec<Process>> {
        // ps enumerates IDs and names only: its rendered argv is ambiguous.
        let out =
            std::process::Command::new("/bin/ps").args(["-axww", "-o", "pid=", "-o", "uid=", "-o", "stat=", "-o", "comm="]).output()?;
        anyhow::ensure!(out.status.success(), "/bin/ps failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        let mut found = Vec::new();
        for mut line in out.stdout.split(|&byte| byte == b'\n').filter(|line| !line.is_empty()) {
            let mut field = || {
                line = line.trim_ascii_start();
                let end = line.iter().position(u8::is_ascii_whitespace).unwrap_or(line.len());
                let (value, rest) = line.split_at(end);
                line = rest;
                value
            };
            let pid: i32 = std::str::from_utf8(field())?.parse()?;
            // Darwin ps renders some UIDs as signed values (e.g. -2).
            let euid = std::str::from_utf8(field())?.parse::<i64>()? as u32;
            let state = field();
            if euid == uid && pid != std::process::id() as i32 && !state.starts_with(b"Z") {
                found.push(Process { pid, name: OsStr::from_bytes(line.trim_ascii()).to_owned() });
            }
        }
        Ok(found)
    }

    #[cfg(target_os = "macos")]
    fn still_live(pid: i32) -> bool {
        // SAFETY: proc_pidinfo writes at most the supplied record size.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as libc::c_int;
        let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
        if read == size {
            return info.pbi_status != libc::SZOMB;
        }
        !gone(&io::Error::last_os_error())
    }

    #[cfg(target_os = "macos")]
    fn command_line(pid: i32) -> anyhow::Result<Option<PathBuf>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size = 0;
        // SAFETY: sysctl is given writable size and, on the second call, a
        // buffer with the reported capacity.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        anyhow::ensure!((4..=16 * 1024 * 1024).contains(&size), "invalid process argv size {size}");
        let mut data = vec![0u8; size];
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, data.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        anyhow::ensure!((4..=data.len()).contains(&size), "invalid process argv reply size {size}");
        // argc, executable path, zero padding, then argc NUL-delimited args;
        // never mistake an environment variable after argv for an argument.
        let argc = i32::from_ne_bytes(data[..4].try_into()?);
        anyhow::ensure!(argc > 0, "empty process argv");
        let tail = &data[4..size];
        let path_end = tail.iter().position(|&b| b == 0).context("missing executable path terminator")?;
        let args = &tail[path_end + 1..];
        let first_arg = args.iter().position(|&b| b != 0).context("missing argv")?;
        let args = args[first_arg..].split_inclusive(|&b| b == 0).map_while(|arg| arg.strip_suffix(&[0])).take(argc as usize);
        // A read this short could be a process whose exec is still in flight.
        anyhow::ensure!(args.clone().count() == argc as usize, "short process argv");
        Ok(git_dir(args))
    }

    #[cfg(target_os = "macos")]
    fn cwd(pid: i32) -> io::Result<PathBuf> {
        // SAFETY: proc_pidinfo writes at most the supplied record size.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as libc::c_int;
        let read =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&mut info as *mut libc::proc_vnodepathinfo).cast(), size) };
        if read <= 0 {
            return Err(io::Error::last_os_error());
        }
        if read != size || info.pvi_cdir.vip_path[0][0] == 0 {
            return Err(io::Error::other("process cwd is unavailable"));
        }
        let bytes: Vec<u8> = info.pvi_cdir.vip_path.as_flattened().iter().copied().take_while(|&b| b != 0).map(|b| b as u8).collect();
        Ok(PathBuf::from(OsStr::from_bytes(&bytes)))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
use unix::scan;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn scan(_git_dir: &Path) -> anyhow::Result<bool> {
    anyhow::bail!("legacy checkpoint Git detection is unsupported on this platform")
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::{Command, Stdio};

    #[tokio::test]
    async fn identifies_only_the_live_git_process_for_this_repo() -> anyhow::Result<()> {
        if !c6_testkit::have("git") {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let work = temp.path();
        let repo = work.join("real repo.git");
        let other = work.join("other repo.git");
        for dir in [&repo, &other] {
            assert!(Command::new("git").arg("--git-dir").arg(dir).args(["init", "--quiet"]).status()?.success());
        }
        let link = work.join("alias repo.git");
        symlink(&repo, &link)?;
        for alias in [&link as &Path, Path::new("alias repo.git")] {
            let mut git = Command::new("git")
                .arg("--git-dir")
                .arg(alias)
                .arg("--work-tree")
                .arg(work)
                .args(["hash-object", "--stdin"])
                .current_dir(work)
                .stdin(Stdio::piped())
                .spawn()?;
            // Leave stdin open so the real Git child cannot finish. Always
            // reap it before asserting, including when detection fails.
            let actual = running(&repo).await;
            let unrelated = running(&other).await;
            let _ = git.kill();
            git.wait()?;
            assert!(actual?, "missed live Git for {alias:?}");
            assert!(!unrelated?, "matched another repo's Git");
            assert!(!running(&repo).await?, "matched the reaped Git process");
        }
        Ok(())
    }
}
