/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic emulation of Linux's robust-futex owner-death protocol.
//!
//! Linux keeps a per-task singly linked list of the robust futexes a thread
//! currently owns (`set_robust_list(2)`). When the task dies, `mm_release()`
//! calls `exit_robust_list()`, which walks that list and, for every futex word
//! that still names the dying task, atomically replaces the word with
//! `(uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED` and — for non-PI futexes with
//! waiters — wakes exactly one waiter. glibc's `pthread_mutex_lock` then
//! observes `FUTEX_OWNER_DIED` and returns `EOWNERDEAD`.
//!
//! Detcore's precise futex model parks waiters in the scheduler's own
//! `futex_waiters` pool rather than in a kernel futex queue, so the kernel's
//! internal owner-death wake cannot reach them. This module re-implements the
//! kernel algorithm inside Detcore so the wake is issued against the modeled
//! waiter pool, deterministically and identically on every backend.
//!
//! The word transition is performed by Detcore as well, not left to the host
//! kernel. That is deliberate:
//!
//! * it removes an ordering race — Detcore issues the wake before the guest's
//!   `exit` reaches the kernel, so a waiter that is woken and re-reads the word
//!   must already see `FUTEX_OWNER_DIED`;
//! * it makes the behavior backend-independent — the KVM backend's guest pages
//!   are not the host task's robust list, so no host kernel would ever perform
//!   this transition for it;
//! * it is idempotent with respect to the host kernel. After Detcore clears the
//!   owner TID field, the kernel's later `handle_futex_death()` sees
//!   `(uval & FUTEX_TID_MASK) != task_pid_vnr(curr)` and does nothing.

/// `FUTEX_WAITERS` from `include/uapi/linux/futex.h`.
pub(crate) const FUTEX_WAITERS: u32 = 0x8000_0000;
/// `FUTEX_OWNER_DIED` from `include/uapi/linux/futex.h`.
pub(crate) const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
/// `FUTEX_TID_MASK` from `include/uapi/linux/futex.h`.
pub(crate) const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

/// `ROBUST_LIST_LIMIT` from `kernel/futex/core.c`: the kernel refuses to follow
/// more than this many entries, which bounds a corrupt or circular list.
pub(crate) const ROBUST_LIST_LIMIT: usize = 2048;

/// Byte offset of `robust_list_head.list.next`.
pub(crate) const HEAD_LIST_OFFSET: usize = 0;
/// Byte offset of `robust_list_head.futex_offset`.
pub(crate) const HEAD_FUTEX_OFFSET_OFFSET: usize = 8;
/// Byte offset of `robust_list_head.list_op_pending`.
pub(crate) const HEAD_LIST_OP_PENDING_OFFSET: usize = 16;
/// `sizeof(struct robust_list_head)` on 64-bit Linux. `set_robust_list(2)`
/// rejects any other length with `EINVAL`.
pub(crate) const ROBUST_LIST_HEAD_LEN: usize = 24;

/// One decoded `struct robust_list *` slot.
///
/// The kernel's `fetch_robust_entry()` stores the PI flag in bit 0 of the
/// pointer, so the entry address is the pointer with that bit cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RobustEntry {
    /// Guest address of the `struct robust_list` node, PI bit removed.
    pub address: usize,
    /// Whether this entry describes a priority-inheritance futex.
    pub is_pi: bool,
}

impl RobustEntry {
    pub(crate) fn decode(raw: u64) -> Self {
        Self {
            address: (raw & !1u64) as usize,
            is_pi: raw & 1 != 0,
        }
    }

    /// Whether this slot is the NULL terminator (`fetch_robust_entry` yields a
    /// null entry only for an unset `list_op_pending`).
    pub(crate) fn is_null(&self) -> bool {
        self.address == 0
    }
}

/// Resolve the futex word address for a list node, mirroring the kernel's
/// `(void __user *)entry + futex_offset`.
///
/// `futex_offset` is a signed value (glibc uses a negative one, because the
/// list node lives after the lock word inside `pthread_mutex_t`). Returns
/// `None` when the sum is not a plausible user address, which the kernel would
/// have discovered as a `get_user()` fault.
pub(crate) fn futex_word_address(entry: usize, futex_offset: i64) -> Option<usize> {
    let offset = isize::try_from(futex_offset).ok()?;
    let sum = entry.checked_add_signed(offset)?;
    (sum != 0).then_some(sum)
}

/// The state change `handle_futex_death()` applies to one futex word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FutexDeathTransition {
    /// Value to store back into the futex word.
    pub new_value: u32,
    /// Whether exactly one waiter must be woken afterwards.
    pub wake_one: bool,
}

/// Pure model of the kernel's `handle_futex_death()` decision.
///
/// Returns `None` when the word does not name `owner_tid`, in which case Linux
/// leaves the word untouched and issues no wake.
pub(crate) fn futex_death_transition(
    uval: u32,
    owner_tid: u32,
    is_pi: bool,
) -> Option<FutexDeathTransition> {
    if uval & FUTEX_TID_MASK != owner_tid & FUTEX_TID_MASK {
        return None;
    }
    Some(FutexDeathTransition {
        new_value: (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED,
        // `handle_futex_death()`: `if (!pi && (uval & FUTEX_WAITERS))`.
        wake_one: !is_pi && uval & FUTEX_WAITERS != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// glibc's `futex_offset` is
    /// `offsetof(pthread_mutex_t, __data.__lock) - offsetof(pthread_mutex_t, __data.__list.__next)`,
    /// i.e. -32 on x86-64.
    const GLIBC_FUTEX_OFFSET: i64 = -32;

    #[test]
    fn pi_bit_is_carried_in_the_pointer_low_bit() {
        assert_eq!(
            RobustEntry::decode(0x0040_4120),
            RobustEntry {
                address: 0x0040_4120,
                is_pi: false
            }
        );
        assert_eq!(
            RobustEntry::decode(0x0040_4121),
            RobustEntry {
                address: 0x0040_4120,
                is_pi: true
            }
        );
        assert!(RobustEntry::decode(0).is_null());
        assert!(!RobustEntry::decode(0x0040_4120).is_null());
    }

    #[test]
    fn glibc_negative_futex_offset_resolves_to_the_lock_word() {
        // Observed layout from the `robust_futex_test` reproducer: the mutex is
        // at 0x404100 and its list node at 0x404120.
        assert_eq!(
            futex_word_address(0x0040_4120, GLIBC_FUTEX_OFFSET),
            Some(0x0040_4100)
        );
    }

    #[test]
    fn implausible_word_addresses_are_refused_rather_than_wrapping() {
        assert_eq!(futex_word_address(8, GLIBC_FUTEX_OFFSET), None);
        assert_eq!(futex_word_address(0, 0), None);
        assert_eq!(futex_word_address(usize::MAX, i64::MAX), None);
    }

    #[test]
    fn a_word_owned_by_another_thread_is_left_alone() {
        // Owner 5 dies; the word names thread 7.
        assert_eq!(futex_death_transition(FUTEX_WAITERS | 7, 5, false), None);
        assert_eq!(futex_death_transition(0, 5, false), None);
    }

    #[test]
    fn owner_death_marks_the_word_and_wakes_one_waiter() {
        // 0x80000005: FUTEX_WAITERS set, owner TID 5. This is the exact value
        // the reproducer's waiter passes to FUTEX_WAIT.
        let transition = futex_death_transition(0x8000_0005, 5, false).unwrap();
        assert_eq!(transition.new_value, FUTEX_WAITERS | FUTEX_OWNER_DIED);
        assert!(transition.wake_one);
    }

    #[test]
    fn owner_death_without_waiters_marks_the_word_but_wakes_nobody() {
        let transition = futex_death_transition(5, 5, false).unwrap();
        assert_eq!(transition.new_value, FUTEX_OWNER_DIED);
        assert!(!transition.wake_one);
    }

    #[test]
    fn pi_entries_are_marked_but_never_plain_woken() {
        // Linux hands PI wakeups to the rt_mutex owner-boosting path instead of
        // `futex_wake()`; mirror that by marking the word only.
        let transition = futex_death_transition(0x8000_0005, 5, true).unwrap();
        assert_eq!(transition.new_value, FUTEX_WAITERS | FUTEX_OWNER_DIED);
        assert!(!transition.wake_one);
    }

    #[test]
    fn an_already_dead_word_is_not_re_marked() {
        // After Detcore applies the transition the TID field is zero, so a
        // second pass (for instance the host kernel's own `exit_robust_list`)
        // finds no match. This is what makes the emulation idempotent.
        let once = futex_death_transition(0x8000_0005, 5, false).unwrap();
        assert_eq!(futex_death_transition(once.new_value, 5, false), None);
    }

    #[test]
    fn high_tid_words_compare_under_the_tid_mask_only() {
        // FUTEX_WAITERS|FUTEX_OWNER_DIED|tid must not be mistaken for a
        // different owner because of the flag bits.
        let uval = FUTEX_WAITERS | FUTEX_OWNER_DIED | 0x0000_1234;
        assert!(futex_death_transition(uval, 0x0000_1234, false).is_some());
        assert!(futex_death_transition(uval, 0x0000_1235, false).is_none());
    }
}
