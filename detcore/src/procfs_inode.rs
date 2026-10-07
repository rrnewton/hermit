/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keys for numbering procfs inodes by path rather than by host inode.
//!
//! Linux numbers most procfs inodes when it builds their dentries: every
//! per-process entry (`/proc/<pid>`, `/proc/<pid>/fd/<n>`, ...) and every
//! `/proc/sys` entry takes the next value of one host-wide counter
//! (`get_next_ino`). An unused dentry can be evicted at any time by memory
//! pressure on the host, and the next lookup or `readdir` builds it again
//! under a new number. The inode pool numbers a host `(device, inode)` it
//! has not seen as a new file, so the guest saw a number that depended on
//! whether the host had evicted the dentry since the guest last saw it: a
//! second `lsof` that listed `/proc/3/fd` after the host rebuilt its entries
//! got new numbers in one `--verify` run and the old ones in the other.
//!
//! Within one procfs mount a path names the same entry for as long as the
//! entry exists, so these entries are keyed by their canonical path instead
//! (the path the kernel gives the open file, with `self` and `thread-self`
//! resolved). A procfs inode number is an `unsigned int`, so a path key, which
//! sets bit 63, can never equal a host inode on the same device. Static procfs
//! entries, whose numbers do not change, are keyed the same way, so `getdents`
//! and `stat` agree on every entry.
//!
//! A path that names a different entry later in the run, as `/proc/<pid>` does
//! after a PID is reused, keeps its number, where Linux would give the new
//! entry a new one.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

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
    /// The directory, when it is on procfs and its entries are keyed by path.
    procfs: Option<ProcfsDirectory>,
}

impl DirectoryInodes {
    pub(crate) fn new(device: u64, procfs: Option<ProcfsDirectory>) -> Self {
        Self { device, procfs }
    }

    /// The key for the entry named `name`, which the host listed with inode
    /// number `host_ino`.
    pub(crate) fn entry_inode(&self, name: &[u8], host_ino: u64) -> RawInode {
        match &self.procfs {
            Some(directory) => path_inode(self.device, &directory.entry_path(name)),
            None => RawInode::new(self.device, host_ino),
        }
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
    fn procfs_entry_key_ignores_the_host_inode() {
        let listing = proc_fd_listing();
        let before = listing.entry_inode(b"0", 4_123_456);
        let rebuilt = listing.entry_inode(b"0", 4_987_654);
        assert_eq!(before, rebuilt);
        // `stat` keys the same entry by the same path, so `d_ino` and
        // `st_ino` stay equal.
        assert_eq!(before, path_inode(PROC_DEVICE, Path::new("/proc/3/fd/0")));
        assert_ne!(before, listing.entry_inode(b"1", 4_123_456));
    }

    #[test]
    fn other_directories_keep_host_keys() {
        let device = libc::makedev(8, 1);
        let listing = DirectoryInodes::new(device, None);
        assert_eq!(listing.entry_inode(b"a", 77), RawInode::new(device, 77));
        assert_ne!(listing.entry_inode(b"a", 77), listing.entry_inode(b"a", 78));
    }

    #[test]
    fn dot_entries_name_the_directory_and_its_parent() {
        let listing = proc_fd_listing();
        assert_eq!(
            listing.entry_inode(b".", 9),
            path_inode(PROC_DEVICE, Path::new("/proc/3/fd"))
        );
        assert_eq!(
            listing.entry_inode(b"..", 9),
            path_inode(PROC_DEVICE, Path::new("/proc/3"))
        );
        let root = DirectoryInodes::new(
            PROC_DEVICE,
            Some(ProcfsDirectory::new(PathBuf::from("/proc"), true)),
        );
        let proc = path_inode(PROC_DEVICE, Path::new("/proc"));
        assert_eq!(root.entry_inode(b".", PROC_ROOT_INO), proc);
        assert_eq!(root.entry_inode(b"..", PROC_ROOT_INO), proc);
        assert_eq!(
            root.entry_inode(b"self", 4_026_531_841),
            path_inode(PROC_DEVICE, Path::new("/proc/self"))
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
}
