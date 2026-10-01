/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;

/// For now we use the definiton of `RawFd` from `std::os`.
// (Workaround: reexporting this type directly triggers a rust-anlazer glitch.)
pub type RawFd = std::os::unix::io::RawFd;

/// Nondeterministic "physical" inode
pub type RawInode = u64;

/// Nondeterministic "physical" identity of a file: the raw device number and
/// inode number exactly as one kernel interface reported them.
///
/// An inode number alone is not a file identity. Linux numbers inodes per
/// filesystem, so unrelated files on different filesystems share numbers, and
/// whether two of them coincide depends on host counters: fresh tmpfs mounts
/// all start at 1, the internal shm mount hands memfds per-CPU batches, and
/// pipes, sockets and procfs entries draw from `get_next_ino`. When
/// deterministic inodes were keyed on the inode alone, a coincidence present in
/// one run but not the other made the two runs mint different values for every
/// later file (<https://github.com/rrnewton/hermit/issues/3307>).
///
/// The device is the one `stat` reports for the file. Linux does not always
/// report one device per file across interfaces: on btrfs, `stat` reports a
/// per-subvolume device while `/proc/*/maps` reports the superblock device,
/// and on overlayfs maps reports the lower file's device. This type does not
/// guess that such devices are equal; a caller holding another interface's
/// pair must find `stat`'s identity for the same file or key on the pair it
/// has, accepting that the two views mint different values. Detcore's
/// `mapping_stat_identity` does the former for a maps line from the `fstat`
/// identity recorded when the guest mapped the file, or else by resolving the
/// line's path, and keys on the maps pair only when neither names the file.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub struct RawFileId {
    /// Raw device number (`st_dev`, or the device column of a maps line).
    pub device: u64,
    /// Raw inode number on that device.
    pub inode: RawInode,
}

impl RawFileId {
    /// Pair a raw device with a raw inode reported by the same interface.
    pub const fn new(device: u64, inode: RawInode) -> Self {
        Self { device, inode }
    }
}

/// Deterministic "virtual" inode.
///
/// Deliberately a newtype rather than an alias for [`RawInode`]. As an alias
/// the two were the same type to the compiler, so a host inode could be used
/// wherever a deterministic one was required and nothing diagnosed it; that is
/// how raw host inodes reached guest-visible `ResourceID`s.
///
/// There is intentionally no `From<RawInode>` impl. The only supported way to
/// turn a host inode into a `DetInode` is the determinization boundary in
/// `tool_global` (`determinize_inode` -> `add_inode`), which mints values from
/// a monotonic counter. [`DetInode::mint`] exists for that boundary and for the
/// handful of compile-time constants; every call site is a deliberate,
/// auditable assertion that the value is already deterministic.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub struct DetInode(RawInode);

impl DetInode {
    /// Assert that `value` is a deterministic inode.
    ///
    /// Reserved for the determinization boundary and for compile-time
    /// constants. Passing a host inode here reintroduces the leak this newtype
    /// exists to prevent.
    pub const fn mint(value: RawInode) -> Self {
        Self(value)
    }

    /// The underlying integer, for writing into guest-visible stat buffers.
    pub const fn as_raw(self) -> RawInode {
        self.0
    }
}

impl std::fmt::Display for DetInode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identity of a Linux descriptor table (`files_struct`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FilesId {
    creator: DetTid,
    generation: u64,
}

impl FilesId {
    /// Create the first descriptor table owned by a task.
    pub const fn initial(creator: DetTid) -> Self {
        Self {
            creator,
            generation: 0,
        }
    }

    /// Create a copied descriptor table for a newly created task.
    pub const fn forked(creator: DetTid) -> Self {
        Self::initial(creator)
    }

    /// Create the replacement table installed by exec.
    pub fn for_exec(self, creator: DetTid) -> Self {
        let generation = if self.creator == creator {
            self.generation + 1
        } else {
            0
        };
        Self {
            creator,
            generation,
        }
    }
}

/// Identity of one numeric descriptor slot within a descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FdSlot {
    /// Descriptor table containing the slot.
    pub files: FilesId,
    /// Numeric descriptor within the table.
    pub fd: RawFd,
}

/// Identity of a Linux open file description (`struct file`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub struct OpenFileId {
    creator: DetTid,
    sequence: u64,
}

const SOCKET_SEQUENCE_DOMAIN: u64 = 1 << 63;

impl OpenFileId {
    /// Create an identity from the task that observed the open and its local sequence.
    pub const fn new(creator: DetTid, sequence: u64) -> Self {
        assert!(sequence < SOCKET_SEQUENCE_DOMAIN);
        Self { creator, sequence }
    }

    /// Create a socket identity from a backend-independent socket-open sequence.
    pub const fn new_socket(creator: DetTid, sequence: u64) -> Self {
        assert!(sequence < SOCKET_SEQUENCE_DOMAIN);
        Self {
            creator,
            sequence: SOCKET_SEQUENCE_DOMAIN | sequence,
        }
    }

    /// Whether this identity came from the socket-specific allocation domain.
    pub const fn is_socket(self) -> bool {
        self.sequence & SOCKET_SEQUENCE_DOMAIN != 0
    }

    // TODO-HUMAN-REVIEW(PR-886): Review stable socket-cookie identity encoding.
    /// Encode the per-task socket-open sequence as a deterministic socket cookie.
    ///
    /// Linux promises that live socket cookies are unique and that descriptor aliases
    /// for one open file description share a cookie. Detcore's virtual task IDs and a
    /// socket-specific sequence provide those same properties for realistic descriptor
    /// counts while avoiding the kernel's host-global cookie allocator. The sequence is
    /// independent of regular-file opens because backend loaders do not expose the same
    /// dynamic-linker file operations to Detcore.
    pub fn deterministic_socket_cookie(self) -> u64 {
        let creator = self.creator.as_raw() as u32 as u64;
        (creator << 32) | (self.sequence & u32::MAX as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_socket_cookies_track_open_file_identity() {
        let first = OpenFileId::new_socket(DetTid::from_raw(3), 7);
        let alias = first;
        let next = OpenFileId::new_socket(DetTid::from_raw(3), 8);
        let other_task = OpenFileId::new_socket(DetTid::from_raw(4), 7);

        assert_ne!(first.deterministic_socket_cookie(), 0);
        assert!(first.is_socket());
        assert!(!OpenFileId::new(DetTid::from_raw(3), 7).is_socket());
        assert_eq!(
            first.deterministic_socket_cookie(),
            alias.deterministic_socket_cookie()
        );
        assert_ne!(
            first.deterministic_socket_cookie(),
            next.deterministic_socket_cookie()
        );
        assert_ne!(
            first.deterministic_socket_cookie(),
            other_task.deterministic_socket_cookie()
        );
        assert_ne!(first, OpenFileId::new(DetTid::from_raw(3), 7));
        assert_eq!(first.deterministic_socket_cookie(), (3_u64 << 32) | 7);
    }
}
