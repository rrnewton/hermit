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
}

/// Allocates exec table identities at the authenticated run-global boundary.
/// Generation zero belongs to initial/fork tables. Every reservation consumes a
/// fresh nonzero generation, even when exec is later cancelled or fails.
#[derive(Debug, Default)]
pub struct FilesIdAllocator {
    last_generation: u64,
}

impl FilesIdAllocator {
    /// Reserve one replacement identity. This allocator must be shared by all
    /// exec preparations in the run; per-table counters can alias after a
    /// nonleader assumes the process leader's TID.
    pub fn allocate_exec(&mut self, creator: DetTid) -> FilesId {
        let generation = self
            .last_generation
            .checked_add(1)
            .expect("descriptor table generation exhausted");
        self.last_generation = generation;
        FilesId {
            creator,
            generation,
        }
    }
}

/// Exact identity receipt issued by authenticated exec preparation. The old
/// table field is the caller's correlation value, not independent proof that
/// the global coordinator owns a registry of every active descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecFilesReceipt {
    /// Task which requested the actual exec attempt.
    pub caller: DetTid,
    /// Its registered thread-group identity.
    pub process: crate::pid::DetPid,
    /// The admitted pre-exec address-space incarnation.
    pub mm: crate::futex::MmId,
    /// Table held by the caller at preparation.
    pub old_files: FilesId,
    /// Globally reserved replacement table; also uniquely identifies the attempt.
    pub new_files: FilesId,
}

/// Identity of one numeric descriptor slot within a descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FdSlot {
    /// Descriptor table containing the slot.
    pub files: FilesId,
    /// Numeric descriptor within the table.
    pub fd: RawFd,
}

/// One installation in a numeric descriptor slot, including the referenced OFD.
/// Reinstalling even the same OFD at the same number creates a new generation.
/// This is a local mutation receipt, not independent proof of a kernel effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FdSlotBinding {
    /// Descriptor table and numeric slot.
    pub slot: FdSlot,
    /// Checked, nonzero installation generation within this descriptor table.
    pub generation: u64,
    /// Exact object installed at this incarnation of the slot.
    pub open_file: OpenFileId,
}

/// The network-relevant facts of an installed descriptor slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkFdSlot {
    /// Exact slot installation, never an implementation reference count.
    pub binding: FdSlotBinding,
    /// Slot-local close-on-exec bit; aliases can have different values.
    pub cloexec: bool,
}

/// Exact before/after facts retained until the run-global lifetime authority
/// acknowledges a successful local installation. This is not a kernel-effect
/// receipt and does not replace the adapter's authenticated mutation admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkFdSlotReplacement {
    /// Table whose slot was installed or replaced.
    pub files: FilesId,
    /// The checked generation consumed by this installation, including a
    /// non-network installation which replaced a network descriptor.
    pub installation_generation: u64,
    /// The prior tracked network installation, if any.
    pub before: Option<NetworkFdSlot>,
    /// The newly installed network descriptor, if any.
    pub after: Option<NetworkFdSlot>,
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

    #[test]
    fn exec_ids_survive_nonleader_reassignment_and_repeated_exec() {
        let leader = DetTid::from_raw(10);
        let nonleader = DetTid::from_raw(11);
        let initial = FilesId::initial(leader);
        let mut allocator = FilesIdAllocator::default();
        let first = allocator.allocate_exec(nonleader);
        let second = allocator.allocate_exec(leader);
        let third = allocator.allocate_exec(leader);
        assert_eq!(
            std::collections::HashSet::from([initial, first, second, third]).len(),
            4,
            "nonleader-to-leader reassignment must never reuse an earlier table ID"
        );
        assert_eq!(
            [
                initial.generation,
                first.generation,
                second.generation,
                third.generation
            ],
            [0, 1, 2, 3]
        );
        assert_eq!(
            [first.creator, second.creator, third.creator],
            [nonleader, leader, leader]
        );
    }

    #[test]
    fn exec_ids_do_not_collide_with_an_external_shared_table_after_private_thread_fork() {
        let leader = DetTid::from_raw(10);
        let nonleader = DetTid::from_raw(11);
        let mut allocator = FilesIdAllocator::default();
        let _first_exec = allocator.allocate_exec(leader);
        let externally_shared = allocator.allocate_exec(leader);
        // CLONE_THREAD without CLONE_FILES copies into a fresh generation0 table.
        let private_thread_table = FilesId::forked(nonleader);
        let nonleader_exec = allocator.allocate_exec(nonleader);
        let later_leader_exec = allocator.allocate_exec(leader);
        assert_ne!(later_leader_exec, externally_shared);
        assert_eq!(
            std::collections::HashSet::from([
                externally_shared,
                private_thread_table,
                nonleader_exec,
                later_leader_exec
            ])
            .len(),
            4
        );
        assert_eq!(externally_shared.generation, 2);
        assert_eq!(private_thread_table.generation, 0);
        assert_eq!(later_leader_exec.generation, 4);
    }

    #[test]
    fn cancelled_exec_reservations_and_other_processes_never_reuse_generations() {
        let mut allocator = FilesIdAllocator::default();
        let caller = DetTid::from_raw(10);
        let cancelled = allocator.allocate_exec(caller);
        let other = allocator.allocate_exec(DetTid::from_raw(20));
        let retry = allocator.allocate_exec(caller);
        assert_eq!(
            [cancelled.generation, other.generation, retry.generation],
            [1, 2, 3]
        );
        assert_ne!(cancelled, retry);
        assert_ne!(other, FilesId::initial(DetTid::from_raw(20)));
    }

    #[test]
    fn files_id_exec_generation_exhaustion_refuses_identity_reuse_without_mutation() {
        let mut allocator = FilesIdAllocator {
            last_generation: u64::MAX,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            allocator.allocate_exec(DetTid::from_raw(11))
        }));
        let failure = result.expect_err("exhaustion must not allocate a duplicate ID");
        let text = failure
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| failure.downcast_ref::<String>().map(String::as_str));
        assert_eq!(text, Some("descriptor table generation exhausted"));
        assert_eq!(allocator.last_generation, u64::MAX);
    }
}
