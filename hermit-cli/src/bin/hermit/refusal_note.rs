/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Why Hermit's fail-closed policy refused a run, kept where it outlives the
//! run.
//!
//! Detcore files the reason with its coordinator before the run exits with
//! the policy-refusal status (`GlobalState::policy_refusal`): for an
//! unsupported syscall, the syscall and why it cannot be serviced. Only the
//! status crosses the container child's exit, so the reason hands itself, as
//! it is filed, to a hook (`detcore::set_policy_refusal_hook`) that writes it
//! here: a `MAP_SHARED` buffer mapped in analyze's process before the run,
//! which the run's container child inherits with the hook. Analyze reads it
//! after the run. A refusal that Detcore only prints leaves the buffer empty.

use std::io;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// The most of a reason kept, in bytes; a longer one is cut at a character
/// boundary.
const CAPACITY: usize = 4096 - std::mem::size_of::<AtomicU64>();

/// One shared note: the reason's length (zero for none), then its bytes,
/// written before the length.
#[repr(C)]
struct Cells {
    len: AtomicU64,
    bytes: [u8; CAPACITY],
}

struct Note {
    cells: NonNull<Cells>,
}

// SAFETY: a MAP_SHARED mapping that is never unmapped; the length is an
// atomic written after the bytes it covers, and read before them.
unsafe impl Send for Note {}
unsafe impl Sync for Note {}

impl Note {
    fn map() -> io::Result<Self> {
        // SAFETY: anonymous shared mapping, no fixed address and no backing
        // fd; the kernel zeroes it, which is an empty note.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<Cells>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let cells = NonNull::new(address.cast::<Cells>())
            .expect("mmap returned a non-null non-failure address");
        Ok(Self { cells })
    }

    fn len(&self) -> &AtomicU64 {
        // SAFETY: the mapping is live and suitably aligned; `len` is an
        // atomic, so a shared reference to it is sound across processes.
        unsafe { &(*self.cells.as_ptr()).len }
    }

    /// Keep `reason` unless one is kept already: the first one counts.
    fn record(&self, reason: &str) {
        if self.len().load(Ordering::Acquire) != 0 {
            return;
        }
        let mut end = reason.len().min(CAPACITY);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        // SAFETY: at most CAPACITY bytes into the mapping's byte array, which
        // nothing reads until the length below says they are there.
        unsafe {
            let bytes = std::ptr::addr_of_mut!((*self.cells.as_ptr()).bytes).cast::<u8>();
            std::ptr::copy_nonoverlapping(reason.as_ptr(), bytes, end);
        }
        self.len().store(end as u64, Ordering::Release);
    }

    fn take(&self) -> Option<String> {
        let len = self.len().swap(0, Ordering::Acquire) as usize;
        if len == 0 {
            return None;
        }
        let mut reason = vec![0; len.min(CAPACITY)];
        // SAFETY: the writer stored these bytes before the length it
        // released, which the swap above acquired.
        unsafe {
            let bytes = std::ptr::addr_of!((*self.cells.as_ptr()).bytes).cast::<u8>();
            std::ptr::copy_nonoverlapping(bytes, reason.as_mut_ptr(), reason.len());
        }
        Some(String::from_utf8_lossy(&reason).into_owned())
    }
}

static NOTE: OnceLock<Note> = OnceLock::new();

/// Detcore's hook: keep the reason for analyze. A copy and an atomic store.
fn record_in_shared_note(reason: &str) {
    if let Some(note) = NOTE.get() {
        note.record(reason);
    }
}

/// Before a run analyze starts: map the note once, install the hook that
/// writes it, and forget what an earlier run left.
pub(crate) fn arm() -> io::Result<()> {
    if NOTE.get().is_none() {
        let note = Note::map()?;
        let _ = NOTE.set(note);
        detcore::set_policy_refusal_hook(Some(record_in_shared_note));
    }
    take();
    Ok(())
}

/// The refusal reason the latest run left, if any; reading it clears it.
pub(crate) fn take() -> Option<String> {
    NOTE.get()?.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `arm` installs the hook that writes the note, which is what a run's
    /// container child inherits.
    #[test]
    fn arming_installs_the_hook_that_writes_the_note() {
        arm().unwrap();
        let installed = detcore::policy_refusal_hook().expect("arm installs a hook");
        assert!(std::ptr::fn_addr_eq(
            installed,
            record_in_shared_note as fn(&str)
        ));
    }

    /// A reason noted by a forked child that then exits, as a run refused
    /// with the policy-refusal status does, reaches the parent once; only the
    /// first of two counts, and a reason longer than the note is cut at a
    /// character boundary. A private note keeps this away from the
    /// process's own.
    #[test]
    fn a_refusal_reason_noted_by_an_exiting_child_reaches_its_parent_once() {
        let note = Note::map().unwrap();
        let reason = "openat2: RESOLVE_IN_ROOT cannot be serviced on this backend";
        // SAFETY: the child does only a copy, atomic stores and `_exit`.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", io::Error::last_os_error());
        if child == 0 {
            note.record(reason);
            note.record("a later refusal");
            // SAFETY: async-signal-safe; runs no destructors in the child.
            unsafe { libc::_exit(detcore_model::HERMIT_POLICY_REFUSAL_EXIT) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert_eq!(note.take().as_deref(), Some(reason));
        assert_eq!(note.take(), None);

        let long = "é".repeat(CAPACITY);
        note.record(&long);
        let kept = note.take().unwrap();
        assert!(
            kept.len() <= CAPACITY && long.starts_with(&kept),
            "{}",
            kept.len()
        );
    }
}
