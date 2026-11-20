/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! The discriminator for a container child that a fault of Hermit's own in
//! the run's choice protocol exits 125.
//!
//! A run with a choice source (`--choice-tape`, `--choice-controller`) checks
//! that every draw takes the request its choice boundary announced, and that
//! every announcement is drawn. A failed check is a fault of Hermit's own (an
//! `internal:` reason, never a source's or the guest's), and the child exits
//! `HERMIT_INTERNAL_FAILURE_EXIT` at once (`detcore::choice`'s `fail_run`).
//! 125 is not that fault's alone, so just before the exit the child sets this
//! note, through the session's exit hook
//! (`detcore::choice::set_choice_internal_exit_hook`), and the parent reads it
//! when it classifies the 125 (`classify_container_result`): the run then
//! reports `cause=choice-internal`.
//!
//! The note is a `MAP_SHARED` cell, as the virtual-timeout note is
//! (`terminal_outcome_note`). A run with a choice source arms it
//! (`RunOpts::launch_in_container`) before it clones the container child,
//! which inherits the mapping and the hook; a guest loses both at its
//! `execve`. Every container launch clears a note already mapped
//! (`owned_container::run`), so a note always belongs to the latest child.

use std::io;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use hermit::Context;
use hermit::Error;

/// One shared note: whether the child exited for a fault in its choice
/// protocol.
struct Note {
    fault: NonNull<AtomicBool>,
}

// SAFETY: an initialized atomic in a MAP_SHARED mapping that is never
// unmapped; the atomic supplies synchronization.
unsafe impl Send for Note {}
unsafe impl Sync for Note {}

impl Note {
    fn map() -> io::Result<Self> {
        // SAFETY: anonymous shared mapping, no fixed address and no backing fd.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<AtomicBool>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let fault = NonNull::new(address.cast::<AtomicBool>())
            .expect("mmap returned a non-null non-failure address");
        // SAFETY: the fresh mapping is writable and suitably page-aligned.
        unsafe { fault.as_ptr().write(AtomicBool::new(false)) };
        Ok(Self { fault })
    }

    fn cell(&self) -> &AtomicBool {
        // SAFETY: initialized in `map` and never unmapped.
        unsafe { self.fault.as_ref() }
    }

    fn record(&self) {
        self.cell().store(true, Ordering::Release);
    }

    fn take(&self) -> bool {
        self.cell().swap(false, Ordering::Acquire)
    }
}

static NOTE: OnceLock<Note> = OnceLock::new();

/// The choice session's exit hook in a container child: leave the note for
/// the parent. An atomic store, so it is safe wherever the session exits from.
fn record_in_shared_note() {
    if let Some(note) = NOTE.get() {
        note.record();
    }
}

/// Before a container child of a run with a choice source is cloned: map the
/// note once and install the exit hook that writes it.
pub(super) fn arm() -> Result<(), Error> {
    if NOTE.get().is_none() {
        let note = Note::map().context("mapping the choice-protocol exit note")?;
        let _ = NOTE.set(note);
        detcore::choice::set_choice_internal_exit_hook(Some(record_in_shared_note));
    }
    clear();
    Ok(())
}

/// Before any container child is cloned: forget what an earlier child left,
/// so a 125 it never reported cannot be read as this child's. Maps nothing.
pub(super) fn clear() {
    if let Some(note) = NOTE.get() {
        note.take();
    }
}

/// Whether the latest container child left the note before exiting; reading
/// it clears it. `false` when no child was armed in this process.
pub(super) fn take() -> bool {
    NOTE.get().is_some_and(Note::take)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A note set by a forked child just before it exits 125, as a choice
    /// session's internal fault exits, reaches the parent once. A private note
    /// keeps this away from the process's own, which other tests'
    /// classifications read.
    #[test]
    fn a_choice_fault_noted_by_an_exiting_child_reaches_its_parent_once() {
        let note = Note::map().unwrap();
        assert!(!note.take(), "a fresh note is clear");
        // SAFETY: the child does only an atomic store and `_exit`, both
        // async-signal-safe, so forking a multi-threaded test process is sound.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", io::Error::last_os_error());
        if child == 0 {
            note.record();
            // SAFETY: async-signal-safe; runs no destructors in the child.
            unsafe { libc::_exit(hermit::HERMIT_INTERNAL_FAILURE_EXIT) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(
            libc::WEXITSTATUS(status),
            hermit::HERMIT_INTERNAL_FAILURE_EXIT
        );
        assert!(note.take());
        assert!(!note.take(), "read once");
    }
}
