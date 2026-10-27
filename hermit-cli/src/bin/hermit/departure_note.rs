/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Where a run's schedule replay first left its schedule, kept where it
//! outlives the run.
//!
//! The replay reports its first departure in the run's summary
//! (`RunSummary::schedule_departure`), but a run that a timeout kills, or
//! that Hermit refuses, writes no summary, and analyze must still know that
//! the run did not follow its schedule. So the replay also hands the
//! departure, as it happens, to a hook (`detcore::set_schedule_departure_hook`)
//! that writes it here: a `MAP_SHARED` cell mapped in analyze's process
//! before the run, which the run's container child inherits with the hook.
//! Analyze reads it after the run, however the run ended.

use std::io;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use detcore::types::DetTid;
use detcore_model::summary::ScheduleDeparture;

/// One shared note: the departure's event plus one (zero for none), then the
/// two threads, the schedule's in the high half and the one that ran in the
/// low half.
struct Note {
    cells: NonNull<[AtomicU64; 2]>,
}

// SAFETY: initialized atomics in a MAP_SHARED mapping that is never
// unmapped; the atomics supply synchronization.
unsafe impl Send for Note {}
unsafe impl Sync for Note {}

impl Note {
    fn map() -> io::Result<Self> {
        // SAFETY: anonymous shared mapping, no fixed address and no backing fd.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<[AtomicU64; 2]>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let cells = NonNull::new(address.cast::<[AtomicU64; 2]>())
            .expect("mmap returned a non-null non-failure address");
        // SAFETY: the fresh mapping is writable and suitably page-aligned.
        unsafe { cells.as_ptr().write([AtomicU64::new(0), AtomicU64::new(0)]) };
        Ok(Self { cells })
    }

    fn cells(&self) -> &[AtomicU64; 2] {
        // SAFETY: initialized in `map` and never unmapped.
        unsafe { self.cells.as_ref() }
    }

    /// Keep `departure` unless one is kept already: the first one counts.
    fn record(&self, departure: ScheduleDeparture) {
        let [event, threads] = self.cells();
        if event.load(Ordering::Acquire) != 0 {
            return;
        }
        let expected = u64::from(departure.expected.as_raw() as u32);
        let running = u64::from(departure.running.as_raw() as u32);
        threads.store(expected << 32 | running, Ordering::Release);
        event.store(departure.event.saturating_add(1), Ordering::Release);
    }

    fn take(&self) -> Option<ScheduleDeparture> {
        let [event, threads] = self.cells();
        let event = event.swap(0, Ordering::Acquire).checked_sub(1)?;
        let threads = threads.load(Ordering::Acquire);
        Some(ScheduleDeparture {
            event,
            expected: DetTid::from_raw((threads >> 32) as u32 as i32),
            running: DetTid::from_raw(threads as u32 as i32),
        })
    }
}

static NOTE: OnceLock<Note> = OnceLock::new();

/// The replay's hook: keep the departure for analyze. Atomic stores only.
fn record_in_shared_note(departure: ScheduleDeparture) {
    if let Some(note) = NOTE.get() {
        note.record(departure);
    }
}

/// Before a run analyze starts: map the note once, install the hook that
/// writes it, and forget what an earlier run left.
pub(crate) fn arm() -> io::Result<()> {
    if NOTE.get().is_none() {
        let note = Note::map()?;
        let _ = NOTE.set(note);
        detcore::set_schedule_departure_hook(Some(record_in_shared_note));
    }
    take();
    Ok(())
}

/// The departure the latest run left, if any; reading it clears it.
pub(crate) fn take() -> Option<ScheduleDeparture> {
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
        let installed = detcore::schedule_departure_hook().expect("arm installs a hook");
        assert!(std::ptr::fn_addr_eq(
            installed,
            record_in_shared_note as fn(ScheduleDeparture)
        ));
    }

    /// A departure noted by a forked child that is then killed, as a run a
    /// timeout stops is, reaches the parent once, with both threads; only the
    /// first of two counts. A private note keeps this away from the
    /// process's own.
    #[test]
    fn a_departure_noted_by_a_killed_child_reaches_its_parent_once() {
        let note = Note::map().unwrap();
        let departure = ScheduleDeparture {
            event: 6843,
            expected: DetTid::from_raw(15360),
            running: DetTid::from_raw(15361),
        };
        // SAFETY: the child does only atomic stores and then waits to be
        // killed, all async-signal-safe.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", io::Error::last_os_error());
        if child == 0 {
            note.record(departure);
            note.record(ScheduleDeparture {
                event: 7000,
                ..departure
            });
            loop {
                // SAFETY: async-signal-safe; the parent kills this child.
                unsafe { libc::pause() };
            }
        }
        while note.cells()[0].load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        // SAFETY: kills and reaps the child forked above.
        unsafe {
            libc::kill(child, libc::SIGKILL);
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
        }
        assert_eq!(note.take(), Some(departure));
        assert_eq!(note.take(), None);
    }
}
