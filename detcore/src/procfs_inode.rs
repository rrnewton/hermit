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
//! So a procfs sighting also names the entry's canonical path (the path the
//! kernel gives the open entry, with `self` and `thread-self` resolved), as a
//! [`path_inode`] key. The pool numbers an entry by its host inode first: a
//! host inode it already knows keeps its number, so aliases (one entry listed
//! in `/proc/<pid>/net` and reached as `/proc/<pid>/task/<pid>/net/tcp`)
//! share it. Only a host inode the pool has never seen, reached by a path it
//! has seen, is that path's entry rebuilt, and takes the path's number. A
//! path key only ever leads to a host key; it numbers nothing itself.
//!
//! Detcore finds the path in the tracer, without any guest-visible operation:
//! it opens the object again with `O_PATH` through the guest thread's
//! `/proc/<tid>` links, checks that what it opened is the same
//! `(device, inode)` on procfs, and reads the link of its own descriptor
//! ([`find_procfs_path`]). It touches the object only once the guest's mount
//! table says the device is a procfs mount ([`is_procfs_device`]): a tracer
//! access to a FUSE file whose server is a stopped guest would never return.
//!
//! Each of these numbers the entry as Detcore did before:
//!
//! - A path through `self` or `thread-self` resolves against the tracer, which
//!   has no PID in the guest's procfs, so such a lookup names no path. It
//!   still agrees with a listing of the same entry through the host inode,
//!   unless the host rebuilt the entry in between.
//! - A rebuilt entry first seen without its path (through `self`, or by a call
//!   that names no path) gets a new number, which its path then leads to.
//! - An entry rebuilt while reached through two mounts of procfs takes the
//!   number of the mount it is next seen through, as the two paths differ.
//! - A path that names a different entry later in the run, as `/proc/<pid>`
//!   does after a PID is reused, gives it the old entry's number, where Linux
//!   numbers it anew. Linux itself keeps a `/proc/<pid>/fd/<n>` number when a
//!   new file takes the descriptor and the dentry survives.
//! - An absolute path is looked up from the guest's root, so a root on a FUSE
//!   file system the guest serves itself could stall that lookup.

use std::collections::HashSet;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use detcore_model::fd::RawInode;

/// The inode number of every procfs root (`PROC_ROOT_INO` in Linux).
pub(crate) const PROC_ROOT_INO: u64 = 1;

/// The block size procfs reports (`s_blocksize` is 1024).
const PROC_BLOCK_SIZE: i64 = 1024;

/// The bit every path key sets, which no 32-bit procfs inode number has.
const PATH_KEY_BIT: u64 = 1 << 63;

/// Whether a stat result can describe a procfs file: procfs has an anonymous
/// device (major 0) and a 1024-byte block size. Other file systems match too
/// (devpts does), so a match only means the caller should ask the kernel.
pub(crate) fn may_be_procfs(device: u64, block_size: i64) -> bool {
    libc::major(device) == 0 && block_size == PROC_BLOCK_SIZE
}

/// The key for the procfs entry at the canonical `path` on `device`: the
/// 64-bit FNV-1a hash of the path's bytes, with [`PATH_KEY_BIT`] set.
pub(crate) fn path_inode(device: u64, path: &Path) -> RawInode {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = path
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(OFFSET_BASIS, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        });
    RawInode::new(device, PATH_KEY_BIT | hash)
}

/// Whether `device` is a procfs mount in the mount namespace of the guest
/// thread `tid`, by its `/proc/<tid>/mountinfo`, which Linux writes without
/// touching any mounted file system. Detcore refuses `mount`, `unshare` and
/// `setns` once it starts, so a guest's procfs mount stays one: a device found
/// to be one is remembered with its mount namespace, while any other device
/// is looked up again, as it may yet become one.
pub(crate) fn is_procfs_device(tid: i32, device: u64) -> bool {
    static PROCFS_DEVICES: OnceLock<Mutex<HashSet<(u64, u64)>>> = OnceLock::new();
    let Ok(namespace) = std::fs::metadata(format!("/proc/{tid}/ns/mnt")) else {
        return false;
    };
    let key = (namespace.ino(), device);
    let known = PROCFS_DEVICES.get_or_init(Default::default);
    if known.lock().unwrap().contains(&key) {
        return true;
    }
    let Ok(mountinfo) = std::fs::read(format!("/proc/{tid}/mountinfo")) else {
        return false;
    };
    let found = mountinfo_has_procfs_device(&mountinfo, device);
    if found {
        known.lock().unwrap().insert(key);
    }
    found
}

/// Whether a `mountinfo` table lists a procfs mount of `device`: a line whose
/// third field is the device's `major:minor` and whose file system type, the
/// field after the `-` separator, is `proc`.
fn mountinfo_has_procfs_device(mountinfo: &[u8], device: u64) -> bool {
    let wanted = format!("{}:{}", libc::major(device), libc::minor(device));
    mountinfo.split(|byte| *byte == b'\n').any(|line| {
        let mut fields = line.split(|byte| *byte == b' ');
        fields.nth(2) == Some(wanted.as_bytes())
            && fields.skip_while(|field| *field != b"-").nth(1) == Some(b"proc".as_slice())
    })
}

/// How a guest call named the procfs object whose path is wanted.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProcfsLookup<'a> {
    /// The file open as this guest descriptor.
    Descriptor(RawFd),
    /// The file `path` names relative to the guest descriptor `dirfd`, or to
    /// the guest's working directory for `AT_FDCWD`, or the symbolic link
    /// itself when `follow` is false.
    Path {
        dirfd: RawFd,
        path: &'a Path,
        follow: bool,
    },
}

/// The canonical path of the procfs object `(device, inode)`, which the guest
/// thread `tid` named as `lookup`, found through that thread's `/proc/<tid>`
/// links: `None` when the tracer cannot open it again, or what it opens is not
/// that object on procfs. This touches the object, so call it only for a
/// device that [`is_procfs_device`] accepts.
pub(crate) fn find_procfs_path(
    tid: i32,
    lookup: ProcfsLookup<'_>,
    device: u64,
    inode: u64,
) -> Option<PathBuf> {
    let mut flags = libc::O_PATH | libc::O_CLOEXEC;
    let mut name = OsString::from(format!("/proc/{tid}/"));
    match lookup {
        ProcfsLookup::Descriptor(fd) => name.push(format!("fd/{fd}")),
        ProcfsLookup::Path {
            dirfd,
            path,
            follow,
        } => {
            if !follow {
                flags |= libc::O_NOFOLLOW;
            }
            if path.is_absolute() {
                name.push("root");
            } else if dirfd == libc::AT_FDCWD {
                name.push("cwd/");
            } else {
                name.push(format!("fd/{dirfd}/"));
            }
            name.push(path.as_os_str());
        }
    }
    let name = CString::new(name.into_vec()).ok()?;
    // SAFETY: `name` is NUL-terminated.
    let fd = unsafe { libc::open(name.as_ptr(), flags) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `open` returned a new descriptor that nothing else owns.
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let metadata = file.metadata().ok()?;
    if metadata.dev() != device || metadata.ino() != inode {
        return None;
    }
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `file` is open and `buf` is large enough for fstatfs.
    if unsafe { libc::fstatfs(file.as_raw_fd(), buf.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstatfs succeeded, so it filled `buf`.
    if unsafe { buf.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
        return None;
    }
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

/// A procfs directory being listed, by its canonical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcfsDirectory {
    path: PathBuf,
    /// Whether this is the root of its procfs mount, whose `..` entry names
    /// the root itself.
    root: bool,
}

impl ProcfsDirectory {
    pub(crate) fn new(path: PathBuf, root: bool) -> Self {
        Self { path, root }
    }

    /// The canonical path of the entry named `name` in this directory.
    fn entry_path(&self, name: &[u8]) -> PathBuf {
        match name {
            b"." => self.path.clone(),
            b".." if self.root => self.path.clone(),
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
    /// number `host_ino`: its host key, and its path key when the directory
    /// is on procfs.
    pub(crate) fn entry_inode(&self, name: &[u8], host_ino: u64) -> (RawInode, Option<RawInode>) {
        let path = self
            .procfs
            .as_ref()
            .map(|directory| path_inode(self.device, &directory.entry_path(name)));
        (RawInode::new(self.device, host_ino), path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_DEVICE: u64 = libc::makedev(0, 23);

    fn proc_fd_listing() -> DirectoryInodes {
        DirectoryInodes::new(
            PROC_DEVICE,
            Some(ProcfsDirectory::new(PathBuf::from("/proc/3/fd"), false)),
        )
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
        assert_eq!(
            before,
            Some(path_inode(PROC_DEVICE, Path::new("/proc/3/fd/0")))
        );
        assert_ne!(before, listing.entry_inode(b"1", 4_123_456).1);
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
        assert_eq!(
            listing.entry_inode(b".", 9).1,
            Some(path_inode(PROC_DEVICE, Path::new("/proc/3/fd")))
        );
        assert_eq!(
            listing.entry_inode(b"..", 9).1,
            Some(path_inode(PROC_DEVICE, Path::new("/proc/3")))
        );
        let root = DirectoryInodes::new(
            PROC_DEVICE,
            Some(ProcfsDirectory::new(PathBuf::from("/proc"), true)),
        );
        let proc = Some(path_inode(PROC_DEVICE, Path::new("/proc")));
        assert_eq!(root.entry_inode(b".", PROC_ROOT_INO).1, proc);
        assert_eq!(root.entry_inode(b"..", PROC_ROOT_INO).1, proc);
        assert_eq!(
            root.entry_inode(b"self", 4_026_531_841).1,
            Some(path_inode(PROC_DEVICE, Path::new("/proc/self")))
        );
    }

    #[test]
    fn path_keys_cannot_equal_procfs_inodes() {
        let paths = [
            "/proc",
            "/proc/1",
            "/proc/3/fd/0",
            "/proc/sys/kernel/pid_max",
        ];
        let keys: Vec<RawInode> = paths
            .iter()
            .map(|path| path_inode(PROC_DEVICE, Path::new(path)))
            .collect();
        for key in &keys {
            assert_eq!(key.dev, PROC_DEVICE);
            assert!(key.ino > u64::from(u32::MAX), "{key:?}");
        }
        for (index, key) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(key), "{key:?}");
        }
        let other_mount = libc::makedev(0, 24);
        assert_ne!(
            path_inode(PROC_DEVICE, Path::new("/proc/1")),
            path_inode(other_mount, Path::new("/proc/1"))
        );
    }

    #[test]
    fn only_anonymous_devices_with_1024_byte_blocks_may_be_procfs() {
        assert!(may_be_procfs(PROC_DEVICE, 1024));
        assert!(!may_be_procfs(libc::makedev(8, 1), 1024));
        assert!(!may_be_procfs(PROC_DEVICE, 4096));
    }

    #[test]
    fn mountinfo_names_procfs_devices_by_type() {
        let mountinfo = b"\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
23 1 0:22 / /dev/pts rw,nosuid,noexec,relatime shared:2 - devpts devpts rw,mode=620
24 1 0:23 / /mnt/a\\040b rw,relatime - fuse.squashfuse_ll squashfuse_ll rw,user_id=0
25 22 0:24 / /proc/sys/fs/binfmt_misc rw,relatime shared:7 - autofs systemd-1 rw
26 1 0:25 / /srv/proc rw - proc proc rw
";
        let procfs = |minor| mountinfo_has_procfs_device(mountinfo, libc::makedev(0, minor));
        assert!(procfs(21));
        assert!(procfs(25), "a mount with no optional fields");
        for minor in [22, 23, 24, 26] {
            assert!(!procfs(minor), "0:{minor}");
        }
        assert!(!mountinfo_has_procfs_device(
            mountinfo,
            libc::makedev(8, 21)
        ));
    }

    #[test]
    fn this_process_finds_its_own_procfs_entries() {
        let pid = std::process::id() as i32;
        let proc = std::fs::metadata("/proc").unwrap();
        assert!(is_procfs_device(pid, proc.dev()));
        assert!(is_procfs_device(pid, proc.dev()), "once remembered");
        assert!(!is_procfs_device(
            pid,
            std::fs::metadata("/").unwrap().dev()
        ));

        let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
        let stat = std::fs::metadata(&stat_path).unwrap();
        let file = File::open("/proc/self/stat").unwrap();
        let find = |lookup| find_procfs_path(pid, lookup, stat.dev(), stat.ino());
        assert_eq!(
            find(ProcfsLookup::Descriptor(file.as_raw_fd())),
            Some(stat_path.clone())
        );
        let by_path = ProcfsLookup::Path {
            dirfd: libc::AT_FDCWD,
            path: &stat_path,
            follow: true,
        };
        assert_eq!(find(by_path), Some(stat_path.clone()));
        let proc_dir = File::open("/proc").unwrap();
        let relative = PathBuf::from(format!("{pid}/stat"));
        let by_dirfd = ProcfsLookup::Path {
            dirfd: proc_dir.as_raw_fd(),
            path: &relative,
            follow: false,
        };
        assert_eq!(find(by_dirfd), Some(stat_path.clone()));
        // Another object, at the same path, is not found.
        assert_eq!(
            find_procfs_path(pid, by_path, stat.dev(), stat.ino() + 1),
            None
        );
    }
}
