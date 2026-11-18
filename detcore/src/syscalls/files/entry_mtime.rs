/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Experimental (provrepro, `--dir-mtime-follows-entries`): a directory's
//! virtual mtime follows the names added to and removed from it.
//!
//! Linux sets a directory's mtime and ctime to the current time whenever a
//! name is created in it, removed from it, or renamed into or out of it
//! (for example `shmem_mknod`, `shmem_unlink` and `shmem_rename2` in
//! mm/shmem.c, and ext4's `ext4_add_entry` and `ext4_delete_entry` callers).
//! A newly created object's mtime is its creation time. Detcore's virtual
//! mtimes otherwise change only on writes and `utimensat`, so a directory
//! keeps the mtime it was first seen with, or the one `tar` restored, and a
//! new file or directory starts at the epoch. A build that archives a
//! directory with `tar --clamp-mtime` then records a different mtime from the
//! one Linux would give it.
//!
//! After a successful entry-changing call, each parent directory it changed,
//! and the object it created, is resolved with an injected `fstatat` and
//! touched (virtual now). An `openat` with `O_CREAT` counts only when the name
//! did not exist before the call.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::StatPtr;
use reverie::syscalls::Syscall;

use crate::record_or_replay::RecordOrReplay;
use crate::tool_global::touch_file;
use crate::tool_local::Detcore;

/// Scratch pages: a path at offset 0, a `struct stat` at `STAT_OFFSET`.
const SCRATCH_LEN: usize = 8192;
const STAT_OFFSET: usize = 4096;

/// A name a call changes: the directory descriptor and the guest's spelling.
type Name = (i32, PathBuf);

/// The names whose parent directories a successful `call` changes, and
/// whether each one is created (so its own mtime is the creation time).
pub(crate) fn entry_changes<M: MemoryAccess>(memory: &M, call: &Syscall) -> Vec<(Name, bool)> {
    let read = |path: Option<PathPtr>| path.and_then(|p| p.read(memory).ok());
    let at = |dirfd: i32, path: Option<PathPtr>, created: bool| {
        read(path).map(|p| ((dirfd, p), created))
    };
    let cwd = libc::AT_FDCWD;
    match call {
        Syscall::Mkdirat(c) => at(c.dirfd(), c.path(), true).into_iter().collect(),
        Syscall::Mknodat(c) => at(c.dirfd(), c.path(), true).into_iter().collect(),
        Syscall::Symlinkat(c) => at(c.newdirfd(), c.linkpath(), true).into_iter().collect(),
        Syscall::Linkat(c) => at(c.newdirfd(), c.newpath(), false).into_iter().collect(),
        Syscall::Unlinkat(c) => at(c.dirfd(), c.path(), false).into_iter().collect(),
        Syscall::Renameat(c) => at(c.olddirfd(), c.oldpath(), false)
            .into_iter()
            .chain(at(c.newdirfd(), c.newpath(), false))
            .collect(),
        Syscall::Renameat2(c) => at(c.olddirfd(), c.oldpath(), false)
            .into_iter()
            .chain(at(c.newdirfd(), c.newpath(), false))
            .collect(),
        #[cfg(target_arch = "x86_64")]
        Syscall::Mkdir(c) => at(cwd, c.path(), true).into_iter().collect(),
        #[cfg(target_arch = "x86_64")]
        Syscall::Mknod(c) => at(cwd, c.path(), true).into_iter().collect(),
        #[cfg(target_arch = "x86_64")]
        Syscall::Symlink(c) => at(cwd, c.linkpath(), true).into_iter().collect(),
        #[cfg(target_arch = "x86_64")]
        Syscall::Link(c) => at(cwd, c.newpath(), false).into_iter().collect(),
        #[cfg(target_arch = "x86_64")]
        Syscall::Unlink(c) => at(cwd, c.path(), false).into_iter().collect(),
        #[cfg(target_arch = "x86_64")]
        Syscall::Rmdir(c) => at(cwd, c.path(), false).into_iter().collect(),
        #[cfg(target_arch = "x86_64")]
        Syscall::Rename(c) => at(cwd, c.oldpath(), false)
            .into_iter()
            .chain(at(cwd, c.newpath(), false))
            .collect(),
        _ => {
            let _ = cwd;
            Vec::new()
        }
    }
}

/// The directory holding `path`'s last component, spelled relative to the
/// same directory descriptor: `.` for a bare name, `/` at the root.
fn parent_of(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &bytes[..end];
    match trimmed.iter().rposition(|b| *b == b'/') {
        None => PathBuf::from("."),
        Some(0) => PathBuf::from("/"),
        Some(slash) => PathBuf::from(OsStr::from_bytes(&trimmed[..slash])),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Whether `call` is one whose success changes names in a directory.
    pub(crate) fn call_changes_entries<G: Guest<Self>>(&self, guest: &G, call: &Syscall) -> bool {
        !entry_changes(&guest.memory(), call).is_empty()
    }

    /// Run an entry-changing pass-through call, then touch what it changed.
    pub(crate) async fn passthrough_touching_entries<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let changes = entry_changes(&guest.memory(), &call);
        let result = self.passthrough(guest, call).await;
        if matches!(result, Ok(ret) if ret >= 0) && !changes.is_empty() {
            self.touch_changed_entries(guest, &changes).await;
        }
        result
    }

    /// `openat` with `O_CREAT`: touch the parent and the new file only when
    /// the name did not exist before the call.
    pub(crate) async fn openat_touching_entries<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Result<i64, Error> {
        let path = call.path().and_then(|p| p.read(&guest.memory()).ok());
        let creates = call.flags().contains(OFlag::O_CREAT);
        let existed = match (&path, creates) {
            (Some(path), true) if !call.flags().contains(OFlag::O_EXCL) => self
                .lookup_entry(guest, call.dirfd(), path, false)
                .await
                .is_some(),
            _ => false,
        };
        let result = Box::pin(self.handle_openat(guest, call)).await;
        if let (Ok(fd), Some(path), true, false) = (&result, path, creates, existed)
            && *fd >= 0
        {
            self.touch_changed_entries(guest, &[((call.dirfd(), path), true)])
                .await;
        }
        result
    }

    async fn touch_changed_entries<G: Guest<Self>>(&self, guest: &mut G, changes: &[(Name, bool)]) {
        for ((dirfd, path), created) in changes {
            if let Some(dir) = self
                .lookup_entry(guest, *dirfd, &parent_of(path), true)
                .await
            {
                touch_file(guest, dir).await;
            }
            if *created && let Some(object) = self.lookup_entry(guest, *dirfd, path, false).await {
                touch_file(guest, object).await;
            }
        }
    }

    /// The `(dev, ino)` of `path` from `dirfd`, by an injected `fstatat`
    /// into private scratch pages; `None` on any failure.
    async fn lookup_entry<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: i32,
        path: &Path,
        follow: bool,
    ) -> Option<crate::types::RawInode> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.len() >= STAT_OFFSET {
            return None;
        }
        let base = guest
            .inject_with_retry(Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(SCRATCH_LEN)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await
            .ok()
            .and_then(|addr| usize::try_from(addr).ok())?;
        let found = self
            .lookup_in_scratch(guest, base, dirfd, bytes, follow)
            .await;
        let _ = guest
            .inject_with_retry(Syscall::Munmap(
                syscalls::Munmap::new()
                    .with_addr(reverie::syscalls::Addr::from_raw(base))
                    .with_len(SCRATCH_LEN),
            ))
            .await;
        found
    }

    async fn lookup_in_scratch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        base: usize,
        dirfd: i32,
        bytes: &[u8],
        follow: bool,
    ) -> Option<crate::types::RawInode> {
        let mut spelled = bytes.to_vec();
        spelled.push(0);
        guest
            .memory()
            .write_exact(AddrMut::from_raw(base)?, &spelled)
            .ok()?;
        let path = PathPtr::from_ptr(base as *const libc::c_char)?;
        let stat = StatPtr::from_ptr((base + STAT_OFFSET) as *const libc::stat)?;
        let flags = if follow {
            AtFlags::empty()
        } else {
            AtFlags::AT_SYMLINK_NOFOLLOW
        };
        guest
            .inject_with_retry(crate::arch::fstatat_syscall(
                syscalls::Fstatat::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(path))
                    .with_stat(Some(stat))
                    .with_flags(flags),
            ))
            .await
            .ok()?;
        let st = stat.read(&guest.memory()).ok()?;
        Some(crate::types::RawInode::new(st.st_dev, st.st_ino))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_are_spelled_from_the_same_descriptor() {
        assert_eq!(parent_of(Path::new("x")), PathBuf::from("."));
        assert_eq!(parent_of(Path::new("d/x")), PathBuf::from("d"));
        assert_eq!(parent_of(Path::new("d/e/x/")), PathBuf::from("d/e"));
        assert_eq!(parent_of(Path::new("/x")), PathBuf::from("/"));
        assert_eq!(parent_of(Path::new("/a/b")), PathBuf::from("/a"));
    }
}
