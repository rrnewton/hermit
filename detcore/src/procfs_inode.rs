/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keys for numbering the procfs entries that the host renumbers.
//!
//! Linux numbers most procfs inodes when it builds their dentries: every
//! per-process entry (`/proc/<pid>`, `/proc/<pid>/fd/<n>`, ...) and every
//! `/proc/sys` entry takes the next value of one host-wide counter
//! (`get_next_ino`). An unused dentry can be evicted at any time by memory
//! pressure on the host, or dropped by a failed lookup, and the next lookup
//! or `readdir` builds it again under a new number. The inode pool numbers a
//! host `(device, inode)` it has not seen as a new file, so the guest saw a
//! number that depended on whether the host had rebuilt the entry since the
//! guest last saw it: a second `lsof` listing of `/proc/3/fd` after the host
//! rebuilt its entries got new numbers in one `--verify` run and the old ones
//! in the other.
//!
//! So a procfs sighting also names the entry ([`ProcfsName`]): the key of its
//! path within procfs ([`path_inode`]), which does not depend on where procfs
//! is mounted, and the task IDs that path names. The pool numbers an entry by
//! its host inode first: a host inode it already knows keeps its number, so
//! aliases (one entry listed in `/proc/<pid>/net` and reached as
//! `/proc/<pid>/task/<pid>/net/tcp`) share it. Only a host inode the pool has
//! never seen, reached by a path it has seen, is that path's entry rebuilt,
//! and takes the path's number. A path key only ever leads to a host key; it
//! numbers nothing itself. A task ID that a new thread reuses names a new
//! task, whose entries are new files ([`with_incarnation`]).
//!
//! Detcore names an entry only once the guest's mount table says its device
//! is a procfs mount ([`procfs_mounts`]), and reads its canonical path from a
//! magic link in the guest thread's `/proc/<tid>` ([`inspect_link`]): the
//! link of the descriptor or working directory the call described, or, for a
//! call that named a path, the link of an `O_PATH` descriptor that Detcore
//! opens with the guest's own path, directory and root, and closes again.
//! When it cannot name a procfs entry (the mount table cannot be read, the
//! guest has no descriptor left or no longer holds the path it named, the
//! object found is not the one the guest saw) it numbers the entry by its
//! host inode and records a determinism loss, so the run is not compared.
//!
//! Each of these numbers the entry as Detcore did before:
//!
//! - An entry rebuilt before Detcore first saw its path gets a new number,
//!   which that path then leads to.
//! - A procfs subtree bind-mounted elsewhere is named by the canonical path the
//!   tracer reads, as is any path under no mount point the guest's mount table
//!   lists (for a guest whose root differs from the tracer's), so the same
//!   entry reached another way can get another name.
//! - A path lookup crosses every mount on the guest's path again, so a mount
//!   on a FUSE file system whose server is a guest Detcore holds could stall
//!   it.
//! - `get_next_ino` wraps at 2^32, so a directory other than a procfs root can
//!   have inode 1, and has its link count reported as 1.
//! - Under a backend whose guest threads are not host tasks (KVM), there is no
//!   `/proc/<tid>` to read, and procfs entries keep their host keys.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use detcore_model::fd::RawInode;
use serde::Deserialize;
use serde::Serialize;

/// The inode number of every procfs root (`PROC_ROOT_INO` in Linux).
pub(crate) const PROC_ROOT_INO: u64 = 1;

/// The block size procfs reports (`s_blocksize` is 1024).
const PROC_BLOCK_SIZE: i64 = 1024;

/// The bit every path key sets, which no 32-bit procfs inode number has.
const PATH_KEY_BIT: u64 = 1 << 63;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// Continues a 64-bit FNV-1a hash from `hash` over `bytes`.
fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// Whether a stat result can describe a procfs file: procfs has an anonymous
/// device (major 0) and a 1024-byte block size. Other file systems match too
/// (devpts does), so a match only means the caller should ask the kernel.
pub(crate) fn may_be_procfs(device: u64, block_size: i64) -> bool {
    libc::major(device) == 0 && block_size == PROC_BLOCK_SIZE
}

/// The key for the procfs entry at `path` within procfs on `device`: the
/// 64-bit FNV-1a hash of the path's bytes, with [`PATH_KEY_BIT`] set.
pub(crate) fn path_inode(device: u64, path: &Path) -> RawInode {
    RawInode::new(
        device,
        PATH_KEY_BIT | fnv1a(FNV_OFFSET_BASIS, path.as_os_str().as_bytes()),
    )
}

/// The path key `key` of an entry that names task ID `task`, for the
/// `incarnation`th thread created under that ID: the hash continued over
/// both, with [`PATH_KEY_BIT`] set.
pub(crate) fn with_incarnation(key: RawInode, task: i32, incarnation: u32) -> RawInode {
    let hash = fnv1a(
        fnv1a(key.ino, &task.to_le_bytes()),
        &incarnation.to_le_bytes(),
    );
    RawInode::new(key.dev, PATH_KEY_BIT | hash)
}

/// A procfs entry's name: the key of its path within procfs, and the task IDs
/// that path names, a process and then a thread in its `task` directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcfsName {
    pub path: RawInode,
    pub tasks: Vec<i32>,
}

impl ProcfsName {
    /// The name of the entry at `within`, its path within procfs on `device`.
    pub(crate) fn new(device: u64, within: &Path) -> Self {
        Self {
            path: path_inode(device, within),
            tasks: tasks_named(within),
        }
    }
}

/// The task ID a path component names, if it is one.
fn task_id(component: Option<Component<'_>>) -> Option<i32> {
    match component {
        Some(Component::Normal(name)) => name
            .to_str()?
            .parse::<i32>()
            .ok()
            .filter(|id| *id > 0 && name.as_bytes()[0] != b'+'),
        _ => None,
    }
}

/// The task IDs a path within procfs names: the process of `/<pid>/...`, and
/// then the thread of `/<pid>/task/<tid>/...`.
fn tasks_named(within: &Path) -> Vec<i32> {
    let mut components = within
        .components()
        .skip_while(|component| *component == Component::RootDir);
    let Some(pid) = task_id(components.next()) else {
        return Vec::new();
    };
    let mut tasks = vec![pid];
    if components.next() == Some(Component::Normal(OsStr::new("task")))
        && let Some(tid) = task_id(components.next())
    {
        tasks.push(tid);
    }
    tasks
}

/// A procfs mount in a guest's mount table: the directory of procfs it shows
/// (`/` unless a subtree is bind-mounted) and where it is mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcfsMount {
    root: PathBuf,
    mount_point: PathBuf,
}

/// The procfs mounts of `device` in the mount namespace of the guest thread
/// `tid`, by its `/proc/<tid>/mountinfo`, which Linux writes without touching
/// any mounted file system: `Ok(None)` when `device` is not procfs there, and
/// an error when the tracer cannot tell. Detcore refuses `mount`, `unshare`
/// and `setns` once it starts, so a guest's procfs mount stays one: the mounts
/// of a device found to be procfs are remembered with its mount namespace,
/// while any other device is looked up again, as it may yet become one.
pub(crate) fn procfs_mounts(tid: i32, device: u64) -> io::Result<Option<Arc<[ProcfsMount]>>> {
    type Known = HashMap<(u64, u64), Arc<[ProcfsMount]>>;
    static PROCFS_DEVICES: OnceLock<Mutex<Known>> = OnceLock::new();
    let namespace = std::fs::metadata(format!("/proc/{tid}/ns/mnt"))?;
    let key = (namespace.ino(), device);
    let known = PROCFS_DEVICES.get_or_init(Default::default);
    if let Some(mounts) = known.lock().unwrap().get(&key) {
        return Ok(Some(mounts.clone()));
    }
    let mountinfo = std::fs::read(format!("/proc/{tid}/mountinfo"))?;
    let mounts = procfs_mounts_in(&mountinfo, device);
    if mounts.is_empty() {
        return Ok(None);
    }
    let mounts: Arc<[ProcfsMount]> = mounts.into();
    known.lock().unwrap().insert(key, mounts.clone());
    Ok(Some(mounts))
}

/// The procfs mounts of `device` a `mountinfo` table lists: lines whose third
/// field is the device's `major:minor` and whose file system type, the field
/// after the `-` separator that ends the optional fields, is `proc`.
fn procfs_mounts_in(mountinfo: &[u8], device: u64) -> Vec<ProcfsMount> {
    let wanted = format!("{}:{}", libc::major(device), libc::minor(device));
    mountinfo
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let fields: Vec<&[u8]> = line.split(|byte| *byte == b' ').collect();
            if fields.get(2).copied() != Some(wanted.as_bytes()) {
                return None;
            }
            let separator = 6 + fields.get(6..)?.iter().position(|field| *field == b"-")?;
            if fields.get(separator + 1).copied() != Some(b"proc".as_slice()) {
                return None;
            }
            Some(ProcfsMount {
                root: unescape(fields[3]),
                mount_point: unescape(fields[4]),
            })
        })
        .collect()
}

/// A `mountinfo` path with its octal escapes (`\040` for a space) decoded.
fn unescape(field: &[u8]) -> PathBuf {
    let mut path = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let octal = field
            .get(index + 1..index + 4)
            .filter(|digits| field[index] == b'\\' && digits.iter().all(u8::is_ascii_digit))
            .filter(|digits| digits.iter().all(|digit| *digit < b'8'));
        match octal {
            Some(digits) => {
                path.push(digits.iter().fold(0u8, |value, digit| {
                    value.wrapping_mul(8).wrapping_add(digit - b'0')
                }));
                index += 4;
            }
            None => {
                path.push(field[index]);
                index += 1;
            }
        }
    }
    PathBuf::from(OsString::from_vec(path))
}

/// The path within procfs of the object at the canonical path `canonical`:
/// its path below the deepest mount point of `mounts` that contains it,
/// joined to that mount's root. A path under no mount point is kept whole.
pub(crate) fn path_within(mounts: &[ProcfsMount], canonical: &Path) -> PathBuf {
    let deepest = mounts
        .iter()
        .filter_map(|mount| {
            let rest = canonical.strip_prefix(&mount.mount_point).ok()?;
            Some((mount, rest))
        })
        .max_by_key(|(mount, _)| mount.mount_point.components().count());
    match deepest {
        Some((mount, rest)) if rest.as_os_str().is_empty() => mount.root.clone(),
        Some((mount, rest)) => mount.root.join(rest),
        None => canonical.to_path_buf(),
    }
}

/// The canonical path of the procfs object behind the magic link `link` (a
/// `/proc/<tid>/fd/<n>` or `/proc/<tid>/cwd`), which must be on `device`, and
/// be inode `inode` when that is given. Following such a link touches only
/// procfs and the object, which is on procfs.
pub(crate) fn inspect_link(
    link: &Path,
    device: u64,
    inode: Option<u64>,
) -> Result<PathBuf, String> {
    let metadata = std::fs::metadata(link)
        .map_err(|error| format!("cannot stat {}: {error}", link.display()))?;
    if metadata.dev() != device || inode.is_some_and(|inode| metadata.ino() != inode) {
        return Err(format!(
            "{} is not the object the guest saw",
            link.display()
        ));
    }
    std::fs::read_link(link)
        .map_err(|error| format!("cannot read the link {}: {error}", link.display()))
}

/// A procfs directory being listed, by its path within procfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcfsDirectory {
    path: PathBuf,
}

impl ProcfsDirectory {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The path within procfs of the entry named `name` in this directory.
    /// The `..` of procfs's root is the root itself.
    fn entry_path(&self, name: &[u8]) -> PathBuf {
        match name {
            b"." => self.path.clone(),
            b".." => self.path.parent().unwrap_or(&self.path).to_path_buf(),
            name => self.path.join(OsStr::from_bytes(name)),
        }
    }
}

/// How to key the inode numbers of the entries of one directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectoryInodes {
    /// The host device of the directory, which every entry shares.
    device: u64,
    /// The directory, when it is on procfs and its entries name their paths.
    procfs: Option<ProcfsDirectory>,
}

impl DirectoryInodes {
    pub(crate) fn new(device: u64, procfs: Option<ProcfsDirectory>) -> Self {
        Self { device, procfs }
    }

    /// The keys for the entry named `name`, which the host listed with inode
    /// number `host_ino`: its host key, and its name when the directory is on
    /// procfs.
    pub(crate) fn entry_inode(&self, name: &[u8], host_ino: u64) -> (RawInode, Option<ProcfsName>) {
        let procfs = self
            .procfs
            .as_ref()
            .map(|directory| ProcfsName::new(self.device, &directory.entry_path(name)));
        (RawInode::new(self.device, host_ino), procfs)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsRawFd;

    use super::*;

    const PROC_DEVICE: u64 = libc::makedev(0, 23);

    fn proc_fd_listing() -> DirectoryInodes {
        DirectoryInodes::new(
            PROC_DEVICE,
            Some(ProcfsDirectory::new(PathBuf::from("/3/fd"))),
        )
    }

    fn name(path: &str) -> Option<ProcfsName> {
        Some(ProcfsName::new(PROC_DEVICE, Path::new(path)))
    }

    #[test]
    fn a_procfs_entry_names_its_path_whatever_its_host_inode() {
        let listing = proc_fd_listing();
        let (host, before) = listing.entry_inode(b"0", 4_123_456);
        let (rebuilt_host, rebuilt) = listing.entry_inode(b"0", 4_987_654);
        assert_eq!(host, RawInode::new(PROC_DEVICE, 4_123_456));
        assert_eq!(rebuilt_host, RawInode::new(PROC_DEVICE, 4_987_654));
        assert_eq!(before, rebuilt);
        // `stat` names the same entry by the same path.
        assert_eq!(before, name("/3/fd/0"));
        assert_eq!(before.unwrap().tasks, vec![3]);
        assert_ne!(listing.entry_inode(b"1", 4_123_456).1, name("/3/fd/0"));
    }

    #[test]
    fn other_directories_keep_host_keys() {
        let device = libc::makedev(8, 1);
        let listing = DirectoryInodes::new(device, None);
        assert_eq!(
            listing.entry_inode(b"a", 77),
            (RawInode::new(device, 77), None)
        );
        assert_ne!(listing.entry_inode(b"a", 77), listing.entry_inode(b"a", 78));
    }

    #[test]
    fn dot_entries_name_the_directory_and_its_parent() {
        let listing = proc_fd_listing();
        assert_eq!(listing.entry_inode(b".", 9).1, name("/3/fd"));
        assert_eq!(listing.entry_inode(b"..", 9).1, name("/3"));
        let root =
            DirectoryInodes::new(PROC_DEVICE, Some(ProcfsDirectory::new(PathBuf::from("/"))));
        assert_eq!(root.entry_inode(b".", PROC_ROOT_INO).1, name("/"));
        assert_eq!(root.entry_inode(b"..", PROC_ROOT_INO).1, name("/"));
        assert_eq!(root.entry_inode(b"self", 4_026_531_841).1, name("/self"));
        assert_eq!(
            root.entry_inode(b"self", 4_026_531_841).1.unwrap().tasks,
            Vec::<i32>::new()
        );
    }

    #[test]
    fn path_keys_cannot_equal_procfs_inodes() {
        let paths = ["/", "/1", "/3/fd/0", "/sys/kernel/pid_max"];
        let mut keys: Vec<RawInode> = paths
            .iter()
            .map(|path| path_inode(PROC_DEVICE, Path::new(path)))
            .collect();
        keys.push(with_incarnation(keys[1], 1, 2));
        keys.push(with_incarnation(keys[1], 1, 3));
        for key in &keys {
            assert_eq!(key.dev, PROC_DEVICE);
            assert!(key.ino > u64::from(u32::MAX), "{key:?}");
        }
        for (index, key) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(key), "{key:?}");
        }
        let other_mount = libc::makedev(0, 24);
        assert_ne!(
            path_inode(PROC_DEVICE, Path::new("/1")),
            path_inode(other_mount, Path::new("/1"))
        );
    }

    #[test]
    fn a_path_names_its_process_and_thread() {
        let tasks = |path: &str| tasks_named(Path::new(path));
        assert_eq!(tasks("/3"), vec![3]);
        assert_eq!(tasks("/3/fd/0"), vec![3]);
        assert_eq!(tasks("/3/task/7/fd/0"), vec![3, 7]);
        assert_eq!(tasks("/3/task"), vec![3]);
        assert_eq!(tasks("/3/task/self"), vec![3]);
        for path in [
            "/",
            "/self",
            "/sys/kernel/pid_max",
            "/0",
            "/-3",
            "/+3",
            "/3 (deleted)",
        ] {
            assert_eq!(tasks(path), Vec::<i32>::new(), "{path}");
        }
    }

    #[test]
    fn only_anonymous_devices_with_1024_byte_blocks_may_be_procfs() {
        assert!(may_be_procfs(PROC_DEVICE, 1024));
        assert!(!may_be_procfs(libc::makedev(8, 1), 1024));
        assert!(!may_be_procfs(PROC_DEVICE, 4096));
    }

    const MOUNTINFO: &[u8] = b"\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
23 1 0:22 / /dev/pts rw,nosuid,noexec,relatime shared:2 - devpts devpts rw,mode=620
24 1 0:23 / /mnt/a\\040b rw,relatime - fuse.squashfuse_ll squashfuse_ll rw,user_id=0
25 22 0:24 / /proc/sys/fs/binfmt_misc rw,relatime shared:7 - autofs systemd-1 rw
26 1 0:25 / /srv/proc rw - proc proc rw
27 1 0:21 /sys /srv/my\\040sys rw master:3 - proc proc rw
";

    #[test]
    fn mountinfo_names_procfs_mounts_by_type() {
        let mounts = |minor| procfs_mounts_in(MOUNTINFO, libc::makedev(0, minor));
        let mount = |root: &str, mount_point: &str| ProcfsMount {
            root: PathBuf::from(root),
            mount_point: PathBuf::from(mount_point),
        };
        assert_eq!(
            mounts(21),
            vec![mount("/", "/proc"), mount("/sys", "/srv/my sys")]
        );
        assert_eq!(
            mounts(25),
            vec![mount("/", "/srv/proc")],
            "a mount with no optional fields"
        );
        for minor in [22, 23, 24, 26] {
            assert!(mounts(minor).is_empty(), "0:{minor}");
        }
        assert!(procfs_mounts_in(MOUNTINFO, libc::makedev(8, 21)).is_empty());
    }

    #[test]
    fn a_path_within_procfs_does_not_depend_on_the_mount() {
        let mounts = procfs_mounts_in(MOUNTINFO, libc::makedev(0, 21));
        let within = |path: &str| path_within(&mounts, Path::new(path));
        assert_eq!(within("/proc"), PathBuf::from("/"));
        assert_eq!(within("/proc/3/fd/0"), PathBuf::from("/3/fd/0"));
        assert_eq!(within("/srv/my sys/kernel"), PathBuf::from("/sys/kernel"));
        assert_eq!(within("/proc/sys/kernel"), PathBuf::from("/sys/kernel"));
        assert_eq!(within("/procfs/3"), PathBuf::from("/procfs/3"));
        assert_eq!(within("/elsewhere/3"), PathBuf::from("/elsewhere/3"));
    }

    #[test]
    fn this_process_reads_its_own_procfs_entries() {
        let pid = std::process::id() as i32;
        let proc = std::fs::metadata("/proc").unwrap();
        let mounts = procfs_mounts(pid, proc.dev()).unwrap().unwrap();
        assert!(
            procfs_mounts(pid, proc.dev()).unwrap().is_some(),
            "once remembered"
        );
        assert!(
            procfs_mounts(pid, std::fs::metadata("/").unwrap().dev())
                .unwrap()
                .is_none()
        );
        assert!(procfs_mounts(-1, proc.dev()).is_err(), "no such thread");

        let stat = std::fs::metadata(format!("/proc/{pid}/stat")).unwrap();
        let file = File::open("/proc/self/stat").unwrap();
        let link = PathBuf::from(format!("/proc/{pid}/fd/{}", file.as_raw_fd()));
        let canonical = inspect_link(&link, stat.dev(), Some(stat.ino())).unwrap();
        assert_eq!(
            path_within(&mounts, &canonical),
            PathBuf::from(format!("/{pid}/stat"))
        );
        assert_eq!(
            inspect_link(&link, stat.dev(), None).unwrap(),
            canonical,
            "a lookup that may find the entry rebuilt checks only the device"
        );
        assert!(inspect_link(&link, stat.dev(), Some(stat.ino() + 1)).is_err());
        assert!(inspect_link(&link, stat.dev() + 1, None).is_err());
        let cwd = PathBuf::from(format!("/proc/{pid}/cwd"));
        let here = std::fs::metadata(".").unwrap();
        assert_eq!(
            inspect_link(&cwd, here.dev(), Some(here.ino())).unwrap(),
            std::env::current_dir().unwrap()
        );
    }
}
