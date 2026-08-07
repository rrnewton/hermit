/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Process-local memory mapping metadata used to resolve shared futex keys.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::types::FutexID;
use crate::types::MmId;
use crate::types::SharedMemoryObjectId;

const PAGE_SIZE: usize = 4096;

// Keep Detcore-owned mappings away from the conventional executable, heap, shared-library,
// stack, and backend-runtime regions. On x86-64, 0x4000_0000_0000..0x5000_0000_0000 lies
// between the low executable/heap area and the usual 0x5555... PIE / 0x7f... loader, stack,
// DynamoRIO, and Reverie mappings. MAP_32BIT gets a separate 1..2 GiB arena as Linux requires.
// These are allocation arenas, not returned constants: every live mapping occupies its
// page-rounded length and the allocator searches the remaining intervals from high addresses
// to low addresses, matching Linux's usual top-down direction.
//
// Guest-visible fixed mappings are recorded and skipped deterministically. The injected mmap
// still uses MAP_FIXED_NOREPLACE as a final collision check against mappings (such as a backend
// runtime) that are deliberately invisible to guest state. Such an unexpected collision fails
// closed; it must never trigger a retry based on backend-specific `/proc/maps` contents.
const ANONYMOUS_ARENA_START: usize = 0x4000_0000_0000;
const ANONYMOUS_ARENA_END: usize = 0x5000_0000_0000;
const MAP_32BIT_ARENA_START: usize = 0x4000_0000;
const MAP_32BIT_ARENA_END: usize = 0x8000_0000;

fn page_aligned_len(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    len.checked_add(PAGE_SIZE - 1)
        .expect("a successful memory range must fit in the address space")
        & !(PAGE_SIZE - 1)
}

/// Round an untrusted syscall length without panicking before Linux validates it.
pub(crate) fn checked_page_aligned_len(len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    len.checked_add(PAGE_SIZE - 1)
        .map(|rounded| rounded & !(PAGE_SIZE - 1))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SharedMapping {
    len: usize,
    object: SharedMemoryObjectId,
    object_offset: u64,
}

impl SharedMapping {
    fn end(self, start: usize) -> usize {
        start
            .checked_add(self.len)
            .expect("a successful memory mapping must fit in the address space")
    }

    fn offset_at(self, start: usize, address: usize) -> u64 {
        self.object_offset
            .checked_add((address - start) as u64)
            .expect("a mapped backing-object offset must fit in u64")
    }
}

/// Shared mappings visible in one Linux memory address space.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct MemoryMetadata {
    next_anonymous_sequence: u64,
    shared_mappings: BTreeMap<usize, SharedMapping>,
    /// Page-rounded ranges already occupied or reserved by guest mmap operations.
    #[serde(default)]
    address_mappings: BTreeMap<usize, usize>,
}

impl MemoryMetadata {
    /// Create an empty address-space model.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Resolve a futex address, falling back to a private key outside tracked shared mappings.
    pub(crate) fn futex_id(&self, mm: MmId, address: usize) -> FutexID {
        let Some((&start, &mapping)) = self.shared_mappings.range(..=address).next_back() else {
            return FutexID::private(mm, address);
        };
        let Some(word_end) = address.checked_add(std::mem::size_of::<u32>()) else {
            return FutexID::private(mm, address);
        };
        if word_end > mapping.end(start) {
            return FutexID::private(mm, address);
        }

        FutexID::shared(mapping.object, mapping.offset_at(start, address))
    }

    /// Reserve the next canonical address for a non-fixed anonymous mapping.
    ///
    /// The reservation is part of this address-space state, so threads sharing `CLONE_VM` share
    /// one sequence, while a fork inherits a snapshot and an exec starts a new sequence. The
    /// returned address varies with the requested lengths and with deterministic map/unmap
    /// history; it is not a frozen address.
    pub(crate) fn reserve_anonymous_address(
        &mut self,
        len: usize,
        map_32bit: bool,
    ) -> Option<usize> {
        let len = checked_page_aligned_len(len)?;
        if len == 0 {
            return None;
        }
        let (arena_start, arena_end) = if map_32bit {
            (MAP_32BIT_ARENA_START, MAP_32BIT_ARENA_END)
        } else {
            (ANONYMOUS_ARENA_START, ANONYMOUS_ARENA_END)
        };
        let start = self.highest_free_range(arena_start, arena_end, len)?;
        self.address_mappings.insert(start, len);
        Some(start)
    }

    fn highest_free_range(
        &self,
        arena_start: usize,
        arena_end: usize,
        len: usize,
    ) -> Option<usize> {
        let mut candidate = arena_end.checked_sub(len)?;
        if candidate < arena_start {
            return None;
        }

        for (&mapping_start, &mapping_len) in self.address_mappings.range(..arena_end).rev() {
            let mapping_end = mapping_start.checked_add(mapping_len)?;
            if mapping_end <= arena_start || mapping_start >= arena_end {
                continue;
            }
            if candidate >= mapping_end {
                // The candidate is entirely above this mapping. Iteration is descending, so
                // every remaining mapping is lower and cannot overlap it either.
                break;
            }
            if candidate
                .checked_add(len)
                .is_some_and(|candidate_end| candidate_end <= mapping_start)
            {
                continue;
            }
            candidate = mapping_start.checked_sub(len)?;
            if candidate < arena_start {
                return None;
            }
        }
        Some(candidate)
    }

    /// Release a reservation after the corresponding mmap failed.
    pub(crate) fn release_address_reservation(&mut self, start: usize, len: usize) {
        let len = page_aligned_len(len);
        if self.address_mappings.get(&start) == Some(&len) {
            self.address_mappings.remove(&start);
        }
    }

    /// Record a successful mapping, including fixed and file-backed mappings that can intersect
    /// a canonical allocation arena.
    pub(crate) fn map_address(&mut self, start: usize, len: usize) {
        let len = page_aligned_len(len);
        if len == 0 {
            return;
        }
        self.unmap_address(start, len);
        self.address_mappings.insert(start, len);
    }

    fn unmap_address(&mut self, start: usize, len: usize) {
        let len = page_aligned_len(len);
        if len == 0 {
            return;
        }
        Self::unmap_tracked_ranges(&mut self.address_mappings, start, len);
    }

    fn unmap_tracked_ranges(
        ranges: &mut BTreeMap<usize, usize>,
        start: usize,
        len: usize,
    ) {
        let end = start
            .checked_add(len)
            .expect("a successful memory range operation must fit in the address space");
        let overlapping = ranges
            .range(..end)
            .filter_map(|(&mapping_start, &mapping_len)| {
                let mapping_end = mapping_start
                    .checked_add(mapping_len)
                    .expect("a tracked memory mapping must fit in the address space");
                (mapping_end > start).then_some((mapping_start, mapping_end))
            })
            .collect::<Vec<_>>();

        for (mapping_start, mapping_end) in overlapping {
            ranges.remove(&mapping_start);
            if mapping_start < start {
                ranges.insert(mapping_start, start - mapping_start);
            }
            if mapping_end > end {
                ranges.insert(end, mapping_end - end);
            }
        }
    }

    /// Record a new anonymous shared mapping.
    pub(crate) fn map_anonymous(&mut self, mm: MmId, start: usize, len: usize) {
        let object = SharedMemoryObjectId::Anonymous {
            origin: mm,
            sequence: self.next_anonymous_sequence,
        };
        self.next_anonymous_sequence = self
            .next_anonymous_sequence
            .checked_add(1)
            .expect("anonymous shared mapping sequence exhausted");
        self.insert_mapping(start, len, object, 0);
    }

    /// Record a mapping with a resolved backing object.
    pub(crate) fn map_object(
        &mut self,
        start: usize,
        len: usize,
        object: SharedMemoryObjectId,
        object_offset: u64,
    ) {
        self.insert_mapping(start, len, object, object_offset);
    }

    fn insert_mapping(
        &mut self,
        start: usize,
        len: usize,
        object: SharedMemoryObjectId,
        object_offset: u64,
    ) {
        let len = page_aligned_len(len);
        if len == 0 {
            return;
        }
        start
            .checked_add(len)
            .expect("a successful memory mapping must fit in the address space");
        self.unmap(start, len);
        self.shared_mappings.insert(
            start,
            SharedMapping {
                len,
                object,
                object_offset,
            },
        );
    }

    /// Remove a range, retaining any mapped portions on either side.
    pub(crate) fn unmap(&mut self, start: usize, len: usize) {
        let len = page_aligned_len(len);
        if len == 0 {
            return;
        }
        let end = start
            .checked_add(len)
            .expect("a successful memory range operation must fit in the address space");
        self.unmap_address(start, len);
        let overlapping = self
            .shared_mappings
            .range(..end)
            .filter_map(|(&mapping_start, &mapping)| {
                (mapping.end(mapping_start) > start).then_some((mapping_start, mapping))
            })
            .collect::<Vec<_>>();

        for (mapping_start, mapping) in overlapping {
            self.shared_mappings.remove(&mapping_start);
            let mapping_end = mapping.end(mapping_start);
            if mapping_start < start {
                self.shared_mappings.insert(
                    mapping_start,
                    SharedMapping {
                        len: start - mapping_start,
                        ..mapping
                    },
                );
            }
            if mapping_end > end {
                self.shared_mappings.insert(
                    end,
                    SharedMapping {
                        len: mapping_end - end,
                        object_offset: mapping.offset_at(mapping_start, end),
                        ..mapping
                    },
                );
            }
        }
    }

    /// Move or resize a mapping after a successful `mremap`.
    pub(crate) fn remap(
        &mut self,
        old_start: usize,
        old_len: usize,
        new_start: usize,
        new_len: usize,
    ) {
        let old_len = page_aligned_len(old_len);
        let new_len = page_aligned_len(new_len);
        let old_end = old_start
            .checked_add(old_len)
            .expect("a successful mremap source must fit in the address space");
        let source = self
            .shared_mappings
            .range(..=old_start)
            .next_back()
            .and_then(|(&mapping_start, &mapping)| {
                (mapping.end(mapping_start) >= old_end)
                    .then_some((mapping.object, mapping.offset_at(mapping_start, old_start)))
            });
        self.unmap(old_start, old_len);
        self.unmap(new_start, new_len);
        if let Some((object, object_offset)) = source {
            self.insert_mapping(new_start, new_len, object, object_offset);
        }
        self.map_address(new_start, new_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DetTid;

    fn mm(tid: i32) -> MmId {
        MmId::initial(DetTid::from_raw(tid))
    }

    #[test]
    fn file_mappings_alias_by_backing_offset() {
        let mut mappings = MemoryMetadata::new();
        let object = SharedMemoryObjectId::File {
            device: 1,
            inode: 2,
        };
        mappings.map_object(0x1000, 0x1000, object, 0);
        mappings.map_object(0x4000, 0x1000, object, 0);
        mappings.map_object(0x8000, 0x1000, object, 0x1000);

        assert_eq!(
            mappings.futex_id(mm(10), 0x1010),
            mappings.futex_id(mm(11), 0x4010),
            "aliases of one file offset must share a futex key"
        );
        assert_ne!(
            mappings.futex_id(mm(10), 0x1010),
            mappings.futex_id(mm(10), 0x8010),
            "different file offsets must not alias"
        );
        assert!(
            matches!(mappings.futex_id(mm(10), 0x1ffc), FutexID::Shared { .. }),
            "mmap lengths must be rounded to the kernel's page boundary"
        );
    }

    #[test]
    fn forked_anonymous_mapping_retains_identity() {
        let mut parent = MemoryMetadata::new();
        parent.map_anonymous(mm(10), 0x1000, 0x1000);
        let mut child = parent.clone();

        assert_eq!(
            parent.futex_id(mm(10), 0x1010),
            child.futex_id(mm(11), 0x1010),
            "fork must retain the backing identity of inherited shared mappings"
        );
        child.map_anonymous(mm(11), 0x4000, 0x1000);
        assert_ne!(
            parent.futex_id(mm(10), 0x1010),
            child.futex_id(mm(11), 0x4010),
            "independent anonymous mappings must use distinct objects"
        );
    }

    #[test]
    fn unmap_and_remap_preserve_only_live_aliases() {
        let mut mappings = MemoryMetadata::new();
        mappings.map_anonymous(mm(10), 0x1000, 0x3000);
        let original = mappings.futex_id(mm(10), 0x2010);

        mappings.unmap(0x2000, 0x1000);
        assert_eq!(
            mappings.futex_id(mm(10), 0x2010),
            FutexID::private(mm(10), 0x2010),
            "an unmapped word must no longer resolve through its old object"
        );
        assert!(
            matches!(mappings.futex_id(mm(10), 0x3010), FutexID::Shared { .. }),
            "the right-hand mapping fragment must retain its shared identity"
        );

        mappings.map_anonymous(mm(10), 0x5000, 0x1000);
        let before_remap = mappings.futex_id(mm(10), 0x5010);
        mappings.remap(0x5000, 0x1000, 0x9000, 0x1000);
        assert_eq!(
            before_remap,
            mappings.futex_id(mm(10), 0x9010),
            "mremap must retain the backing-object offset"
        );
        assert_ne!(original, before_remap);
    }

    #[test]
    fn anonymous_allocator_is_a_length_sensitive_top_down_sequence() {
        let mut mappings = MemoryMetadata::new();
        let lengths = [PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE, 4 * PAGE_SIZE];
        let addresses = lengths.map(|len| {
            mappings
                .reserve_anonymous_address(len, false)
                .expect("canonical arena should have space")
        });

        assert_eq!(addresses[0], ANONYMOUS_ARENA_END - PAGE_SIZE);
        assert_eq!(addresses[1], addresses[0] - 2 * PAGE_SIZE);
        assert_eq!(addresses[2], addresses[1] - 3 * PAGE_SIZE);
        assert_eq!(addresses[3], addresses[2] - 4 * PAGE_SIZE);
        assert_eq!(
            addresses
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            4
        );
    }

    #[test]
    fn anonymous_allocator_reuses_unmapped_space_and_avoids_fixed_ranges() {
        let mut mappings = MemoryMetadata::new();
        let first = mappings
            .reserve_anonymous_address(PAGE_SIZE, false)
            .unwrap();
        mappings.unmap(first, PAGE_SIZE);
        assert_eq!(
            mappings.reserve_anonymous_address(PAGE_SIZE, false),
            Some(first),
            "the topmost freed interval should be reused"
        );

        let fixed_start = first - 3 * PAGE_SIZE;
        mappings.map_address(fixed_start, 2 * PAGE_SIZE);
        assert_eq!(
            mappings.reserve_anonymous_address(2 * PAGE_SIZE, false),
            Some(fixed_start - 2 * PAGE_SIZE),
            "allocation must skip a recorded fixed mapping"
        );
    }

    #[test]
    fn far_lower_fixed_mapping_does_not_displace_the_highest_free_range() {
        let mut mappings = MemoryMetadata::new();
        let first = mappings
            .reserve_anonymous_address(PAGE_SIZE, false)
            .unwrap();
        mappings.map_address(ANONYMOUS_ARENA_START + 10 * PAGE_SIZE, 2 * PAGE_SIZE);

        assert_eq!(
            mappings.reserve_anonymous_address(2 * PAGE_SIZE, false),
            Some(first - 2 * PAGE_SIZE),
            "a disjoint lower mapping must not push a valid highest candidate downward"
        );
    }

    #[test]
    fn forked_allocator_inherits_but_then_diverges_independently() {
        let mut parent = MemoryMetadata::new();
        parent.reserve_anonymous_address(PAGE_SIZE, false).unwrap();
        let mut child = parent.clone();

        let parent_next = parent
            .reserve_anonymous_address(2 * PAGE_SIZE, false)
            .unwrap();
        let child_next = child
            .reserve_anonymous_address(2 * PAGE_SIZE, false)
            .unwrap();
        assert_eq!(
            parent_next, child_next,
            "fork must inherit allocation history"
        );

        parent
            .reserve_anonymous_address(3 * PAGE_SIZE, false)
            .unwrap();
        assert_ne!(
            parent.reserve_anonymous_address(PAGE_SIZE, false),
            child.reserve_anonymous_address(PAGE_SIZE, false),
            "post-fork address spaces must allocate independently"
        );
    }

    #[test]
    fn map_32bit_allocations_stay_below_two_gibibytes() {
        let mut mappings = MemoryMetadata::new();
        let start = mappings
            .reserve_anonymous_address(3 * PAGE_SIZE, true)
            .unwrap();
        assert!(start >= MAP_32BIT_ARENA_START);
        assert!(start + 3 * PAGE_SIZE <= MAP_32BIT_ARENA_END);
        assert!(start + 3 * PAGE_SIZE <= 0x8000_0000);
    }

    #[test]
    fn invalid_or_exhausted_anonymous_requests_do_not_panic_or_escape_the_arena() {
        let mut mappings = MemoryMetadata::new();
        assert_eq!(mappings.reserve_anonymous_address(usize::MAX, false), None);
        let too_large = ANONYMOUS_ARENA_END - ANONYMOUS_ARENA_START + PAGE_SIZE;
        assert_eq!(mappings.reserve_anonymous_address(too_large, false), None);
    }

}
