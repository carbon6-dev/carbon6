//! Observational, portable machine telemetry and explicitly bounded disk reads.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use c6_api::machines::{MachineDisk, MachineDiskScan, MachineFolderUsage, MachineSample, MachineSessionUsage};
use sysinfo::{Disks, ProcessesToUpdate, System};

/// Stateful CPU counters. Keep one sampler alive between collection intervals;
/// never sleep in the collection path to manufacture a CPU measurement.
/// Collection walks the OS process table and reads mounted filesystems: call
/// from a blocking/background task, not an async event-ingest loop.
pub struct Sampler {
    system: System,
    disks: Disks,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_processes(ProcessesToUpdate::All, true);
        Self { system, disks: Disks::new() }
    }

    pub fn sample(&mut self) -> MachineSample {
        self.sample_with_roots(&[])
    }

    /// Include both active session roots and retained/held Claude process roots.
    /// A nested root wins over an ancestor root, independent of input ordering.
    pub fn sample_with_roots(&mut self, roots: &[(u32, String)]) -> MachineSample {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.disks.refresh(true);
        let load = System::load_average();
        let processes: Vec<_> = self
            .system
            .processes()
            .iter()
            .map(|(pid, p)| ProcessUsage {
                pid: pid.as_u32(),
                parent: p.parent().map(|p| p.as_u32()),
                process_group: process_group(pid.as_u32()),
                cpu_percent: f64::from(p.cpu_usage()),
                rss: p.memory(),
            })
            .collect();
        let mut disks: Vec<_> = self
            .disks
            .iter()
            .filter_map(|d| {
                let filesystem = d.file_system().to_string_lossy().into_owned();
                let mount = d.mount_point().to_string_lossy().into_owned();
                let device = d.name().to_string_lossy();
                if !is_real_filesystem(&filesystem, &mount, &device) || d.total_space() == 0 {
                    return None;
                }
                Some(MachineDisk {
                    mount,
                    filesystem,
                    total: d.total_space(),
                    available: d.available_space(),
                    used: d.total_space().saturating_sub(d.available_space()),
                    inodes_used: inodes_used(d.mount_point()),
                })
            })
            .collect();
        disks.sort_by(|a, b| a.mount.cmp(&b.mount));
        let (oom_kills, pressure) = linux_metrics();
        MachineSample {
            at_ms: crate::now_ms(),
            cpu_percent: f64::from(self.system.global_cpu_usage()),
            cpu_count: self.system.cpus().len(),
            load: [load.one, load.five, load.fifteen],
            memory_used: self.system.used_memory(),
            memory_available: self.system.available_memory(),
            memory_total: self.system.total_memory(),
            swap_used: self.system.used_swap(),
            swap_total: self.system.total_swap(),
            oom_kills,
            pressure,
            disks,
            sessions: attribute_processes(&processes, roots),
        }
    }
}

/// Kept separate from OS collection so attribution can be tested without forks
/// or wall-clock sleeps. CPU is percent of one logical CPU, RSS is bytes.
#[derive(Clone, Debug)]
pub struct ProcessUsage {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Retained process groups remain attributable after their leader exits.
    pub process_group: Option<u32>,
    pub cpu_percent: f64,
    pub rss: u64,
}

#[cfg(unix)]
fn process_group(pid: u32) -> Option<u32> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: getpgid takes only a PID and does not dereference any pointers.
    let group = unsafe { libc::getpgid(pid) };
    (group > 0).then_some(group as u32)
}

#[cfg(not(unix))]
fn process_group(_: u32) -> Option<u32> {
    None
}

pub fn attribute_processes(processes: &[ProcessUsage], roots: &[(u32, String)]) -> Vec<MachineSessionUsage> {
    let roots: HashMap<_, _> = roots.iter().map(|(pid, session)| (*pid, session)).collect();
    let parents: HashMap<_, _> = processes.iter().map(|p| (p.pid, p.parent)).collect();
    let mut sessions: BTreeMap<String, MachineSessionUsage> = BTreeMap::new();
    let mut counted = HashSet::new();
    for process in processes {
        if !counted.insert(process.pid) {
            continue;
        }
        let mut pid = process.pid;
        let mut seen = HashSet::new();
        let mut owner = None;
        while seen.insert(pid) {
            if let Some(session) = roots.get(&pid) {
                owner = Some(*session);
                break;
            }
            match parents.get(&pid).copied().flatten() {
                Some(parent) => pid = parent,
                None => break,
            }
        }
        // Prefer the nearest explicit ancestor above. If the group leader has
        // exited, its surviving/reparented members still belong to the root
        // recorded by the journal, even though it is absent from the snapshot.
        let owner = owner.or_else(|| {
            let group = process.process_group?;
            if parents.contains_key(&group) {
                return None;
            }
            roots.get(&group).copied()
        });
        if let Some(session) = owner {
            let usage = sessions
                .entry(session.clone())
                .or_insert_with(|| MachineSessionUsage { session_id: session.clone(), ..Default::default() });
            usage.cpu_percent += process.cpu_percent;
            usage.rss = usage.rss.saturating_add(process.rss);
        }
    }
    sessions.into_values().collect()
}

pub fn is_real_filesystem(filesystem: &str, mount: &str, device: &str) -> bool {
    !matches!(
        filesystem,
        "proc"
            | "procfs"
            | "sysfs"
            | "devfs"
            | "devtmpfs"
            | "tmpfs"
            | "overlay"
            | "squashfs"
            | "autofs"
            | "cgroup"
            | "cgroup2"
            | "debugfs"
            | "tracefs"
            | "securityfs"
            | "pstore"
            | "efivarfs"
            | "mqueue"
            | "hugetlbfs"
            | "fusectl"
            | "configfs"
            | "binfmt_misc"
            | "rpc_pipefs"
            | "nsfs"
    ) && !device.starts_with("/dev/loop")
        && mount != "/snap"
        && !mount.starts_with("/snap/")
        && mount != "/var/lib/snapd/snap"
        && !mount.starts_with("/var/lib/snapd/snap/")
}

#[cfg(unix)]
// fsfilcnt_t is 32 bits on macOS and 64 bits on Linux.
#[allow(clippy::unnecessary_cast)]
fn inodes_used(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL terminated, stat points to writable storage, and is
    // only read after statvfs has successfully initialized it.
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };
    (stat.f_files > 0).then(|| (stat.f_files as u64).saturating_sub(stat.f_ffree as u64))
}

#[cfg(not(unix))]
fn inodes_used(_: &Path) -> Option<u64> {
    None
}

fn linux_metrics() -> (Option<u64>, BTreeMap<String, String>) {
    #[cfg(target_os = "linux")]
    {
        let oom = std::fs::read_to_string("/proc/vmstat")
            .ok()
            .and_then(|text| text.lines().find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse().ok()));
        let pressure = ["cpu", "memory", "io"]
            .into_iter()
            .filter_map(|kind| std::fs::read_to_string(format!("/proc/pressure/{kind}")).ok().map(|value| (kind.to_owned(), value)))
            .collect();
        (oom, pressure)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, BTreeMap::new())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DiskScanLimits {
    pub max_entries: usize,
    pub max_duration: Duration,
    pub max_folders: usize,
}

impl Default for DiskScanLimits {
    fn default() -> Self {
        Self { max_entries: 100_000, max_duration: Duration::from_secs(5), max_folders: 50 }
    }
}

/// Blocking filesystem work: run on a background/blocking worker, never an async
/// ingest task. Limits are checked between filesystem calls; an individual OS
/// call on an unresponsive filesystem cannot be interrupted portably.
pub fn scan_disk(roots: &[PathBuf], limits: DiskScanLimits) -> MachineDiskScan {
    let start = Instant::now();
    scan_disk_with_clock(roots, limits, crate::now_ms(), || start.elapsed())
}

fn scan_disk_with_clock(roots: &[PathBuf], limits: DiskScanLimits, at_ms: i64, elapsed: impl Fn() -> Duration) -> MachineDiskScan {
    struct Folder {
        path: PathBuf,
        entries: std::fs::ReadDir,
        bytes: u64,
        modified_ms: Option<i64>,
    }
    let mut result = MachineDiskScan { at_ms, ..Default::default() };
    let mut roots = roots.to_vec();
    roots.sort();
    roots.dedup();
    let roots: Vec<_> = roots.iter().filter(|root| !roots.iter().any(|other| *root != other && root.starts_with(other))).collect();
    let mut stack: Vec<Folder> = Vec::new();
    let mut next_roots = roots.into_iter();
    let mut pending = next_roots.next().cloned();
    loop {
        if pending.is_none() && stack.is_empty() {
            pending = next_roots.next().cloned();
            if pending.is_none() {
                break;
            }
        }
        if result.entries >= limits.max_entries || elapsed() >= limits.max_duration {
            result.partial = true;
            break;
        }
        if let Some(path) = pending.take() {
            result.entries += 1;
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    result.partial = true;
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                match std::fs::read_dir(&path) {
                    Ok(entries) => stack.push(Folder {
                        path,
                        entries,
                        bytes: 0,
                        modified_ms: metadata
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_millis().min(i64::MAX as u128) as i64),
                    }),
                    Err(_) => result.partial = true,
                }
            } else if metadata.is_file()
                && let Some(parent) = stack.last_mut()
            {
                parent.bytes = parent.bytes.saturating_add(metadata.len());
            }
        } else if let Some(folder) = stack.last_mut() {
            match folder.entries.next() {
                Some(Ok(entry)) => pending = Some(entry.path()),
                Some(Err(_)) => {
                    result.entries += 1;
                    result.partial = true;
                }
                None => {
                    let folder = stack.pop().unwrap();
                    if let Some(parent) = stack.last_mut() {
                        parent.bytes = parent.bytes.saturating_add(folder.bytes);
                    }
                    result.folders.push(MachineFolderUsage {
                        path: folder.path.to_string_lossy().into_owned(),
                        bytes: folder.bytes,
                        modified_ms: folder.modified_ms,
                    });
                }
            }
        }
    }
    // Preserve already observed sizes even when a cap interrupts a directory.
    while let Some(folder) = stack.pop() {
        if let Some(parent) = stack.last_mut() {
            parent.bytes = parent.bytes.saturating_add(folder.bytes);
        }
        result.folders.push(MachineFolderUsage {
            path: folder.path.to_string_lossy().into_owned(),
            bytes: folder.bytes,
            modified_ms: folder.modified_ms,
        });
    }
    result.folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    result.folders.truncate(limits.max_folders);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_only_real_filesystems() {
        for fs in ["tmpfs", "overlay", "proc", "squashfs", "devfs", "cgroup2"] {
            assert!(!is_real_filesystem(fs, "/", "device"));
        }
        assert!(!is_real_filesystem("ext4", "/snap/app/1", "/dev/loop0"));
        assert!(!is_real_filesystem("ext4", "/data", "/dev/loop1"));
        assert!(is_real_filesystem("ext4", "/", "/dev/sda1"));
        assert!(is_real_filesystem("apfs", "/System/Volumes/Data", "/dev/disk3s1"));
    }

    #[test]
    fn descendants_held_roots_overlap_and_cycles() {
        let processes = [(1, None), (2, Some(1)), (3, Some(2)), (4, Some(3)), (10, None), (11, Some(10)), (20, Some(21)), (21, Some(20))]
            .map(|(pid, parent)| ProcessUsage { pid, parent, process_group: None, cpu_percent: 2.0, rss: 10 });
        let roots = [(1, "active".into()), (3, "nested".into()), (10, "held".into())];
        let usage = attribute_processes(&processes, &roots);
        assert_eq!(usage.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(), ["active", "held", "nested"]);
        assert!(usage.iter().all(|s| s.cpu_percent == 4.0 && s.rss == 20));
        let mut reversed = roots.to_vec();
        reversed.reverse();
        assert_eq!(usage, attribute_processes(&processes, &reversed));
    }

    #[test]
    fn orphan_group_members_keep_owner_but_nested_roots_win() {
        let processes = [
            ProcessUsage { pid: 11, parent: Some(1), process_group: Some(10), cpu_percent: 3.0, rss: 20 },
            ProcessUsage { pid: 12, parent: Some(11), process_group: Some(10), cpu_percent: 4.0, rss: 30 },
            ProcessUsage { pid: 13, parent: Some(12), process_group: Some(10), cpu_percent: 5.0, rss: 40 },
            // An unrelated orphan must not acquire ownership.
            ProcessUsage { pid: 20, parent: Some(1), process_group: Some(19), cpu_percent: 99.0, rss: 99 },
        ];
        let roots = [(10, "held".into()), (12, "nested".into())];
        let usage = attribute_processes(&processes, &roots);
        assert_eq!(
            usage,
            [
                MachineSessionUsage { session_id: "held".into(), cpu_percent: 3.0, rss: 20 },
                MachineSessionUsage { session_id: "nested".into(), cpu_percent: 9.0, rss: 70 },
            ]
        );
        let mut reversed = roots.to_vec();
        reversed.reverse();
        assert_eq!(usage, attribute_processes(&processes, &reversed));
    }

    #[test]
    fn scan_sizes_overlap_mtime_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("big")).unwrap();
        std::fs::write(root.join("small"), [0; 3]).unwrap();
        std::fs::write(root.join("big/data"), [0; 11]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, root.join("cycle")).unwrap();
        let result = scan_disk_with_clock(&[root.clone(), root.join("big")], DiskScanLimits::default(), 123, || Duration::ZERO);
        assert!(!result.partial);
        assert_eq!(result.at_ms, 123);
        assert_eq!(result.folders.len(), 2);
        assert_eq!(result.folders[0].bytes, 14);
        assert_eq!(result.folders[1].bytes, 11);
        assert!(result.folders.iter().all(|f| f.modified_ms.is_some()));
    }

    #[test]
    fn scan_caps_and_errors_without_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), [0; 10]).unwrap();
        let roots = [tmp.path().to_path_buf()];
        let limits = DiskScanLimits { max_entries: 1, ..Default::default() };
        let result = scan_disk_with_clock(&roots, limits, 0, || Duration::ZERO);
        assert!(result.partial);
        assert_eq!(result.entries, 1);
        let result = scan_disk_with_clock(&roots, DiskScanLimits::default(), 0, || Duration::from_secs(100));
        assert!(result.partial);
        assert_eq!(result.entries, 0);
        let result = scan_disk_with_clock(&[tmp.path().join("missing")], DiskScanLimits::default(), 0, || Duration::ZERO);
        assert!(result.partial);
    }
}
