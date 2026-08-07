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

/// Deterministic "virtual" inode.
///
/// This is a newtype rather than an alias to [`RawInode`] on purpose. A raw
/// host inode is environment-derived: it differs between hosts and between
/// runs on one host, so letting one reach a record that is specified to be
/// reproducible (`ResourceID::FileContents`, a guest-visible `st_ino`) makes
/// the deterministic log irreproducible. While this was an alias, exactly that
/// flow type-checked silently at three sites in `syscalls/files.rs`.
///
/// The only supported way to turn a host inode into a `DetInode` is the global
/// inode pool's minting path (`add_inode`, reached through the
/// `DeterminizeInode` RPC), which hands out a monotonic per-run ordinal. There
/// is deliberately **no** `From<RawInode>`, so any other conversion has to name
/// [`DetInode::from_ordinal`] and is therefore greppable and reviewable.
#[derive(
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
#[serde(transparent)]
pub struct DetInode(u64);

/// Render as the bare ordinal, not as `DetInode(4)`. `ResourceID` is `Debug`-
/// formatted straight into DETLOG records, so a derived `Debug` would change
/// `FileContents(4)` to `FileContents(DetInode(4))` and churn the record
/// framing for every log parser and comparator. The type is a compile-time
/// guard; it is deliberately invisible in the output.
impl std::fmt::Debug for DetInode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl DetInode {
    /// Mint a `DetInode` from an already-deterministic ordinal.
    ///
    /// Callers must pass a value that is a deterministic function of guest
    /// execution — the inode pool's monotonic counter, or a fixed offset
    /// constant. Passing a host inode here reintroduces the very leak the
    /// newtype exists to prevent.
    pub const fn from_ordinal(ordinal: u64) -> Self {
        Self(ordinal)
    }

    /// The underlying ordinal, for formatting and for guest-visible `st_ino`.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Derive a related deterministic inode at a fixed offset from this one.
    /// Deterministic in, deterministic out.
    pub const fn offset_by(self, delta: u64) -> Self {
        Self(self.0 + delta)
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

    /// `ResourceID` is `Debug`-formatted directly into DETLOG records, so the
    /// newtype must stay invisible in the output. A derived `Debug` would
    /// silently rewrite every `FileContents(4)` as `FileContents(DetInode(4))`
    /// and churn the record framing for every log parser and comparator.
    #[test]
    fn det_inode_renders_as_a_bare_ordinal() {
        let ino = DetInode::from_ordinal(4);
        assert_eq!(format!("{:?}", ino), "4");
        assert_eq!(format!("{}", ino), "4");
        assert_eq!(format!("{:?}", Some(ino)), "Some(4)");
    }

    /// The pool mints ordinals from a monotonic counter starting at 1, so a
    /// deterministic inode is small. This pins the round-trip the minting path
    /// depends on; a host inode reaching `FileContents` is prevented by the
    /// type, not by this test.
    #[test]
    fn det_inode_ordinal_round_trips_and_offsets() {
        assert_eq!(DetInode::from_ordinal(1).get(), 1);
        assert_eq!(DetInode::from_ordinal(1000).offset_by(2).get(), 1002);
        assert!(DetInode::from_ordinal(1) < DetInode::from_ordinal(2));
    }
}
