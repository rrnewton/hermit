/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `mmap` is a tricky case. When using `MAP_ANONYMOUS` with a file descriptor of
//! -1, we are effectively allocating memory. If ASLR is turned off, this should
//! be deterministic and the mmap can be let through in this case. However, when
//! mapping a file descriptor, things get tricky. Memory writes from one thread
//! can affect the memory reads from another thread. During recording, we should
//! record the entire contents of the buffer. During replay, we need some way to
//! allocate the same size buffer at the same address. We do this by injecting
//!
//! ```ignore
//! mmap(desired_addr, length, flags | MAP_ANONYMOUS, -1, 0)
//! ```
//!
//! in order to get a blank memory map of the right size. We can then fill this
//! with the previously recorded bytes.

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Madvise;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Mmap;
use reverie::syscalls::Mprotect;
use reverie::syscalls::ProtFlags;

use super::Replayer;
use crate::event::MadviseRefill;

impl Replayer {
    pub(super) async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Mmap,
    ) -> Result<i64, Errno> {
        let flags = syscall.flags();

        // Let anonymous mappings through. This should already be deterministic
        // because ASLR is disabled.
        if flags.contains(MapFlags::MAP_ANONYMOUS) {
            return guest.inject_with_retry(syscall).await;
        }

        // Get the event. We need to know what pointer to use for the map.
        let event = next_event!(guest, Mmap)?;

        // This is safe since we only record non-NULL pointers.
        let addr = unsafe { AddrMut::<u8>::from_raw_unchecked(event.addr) };

        let len = syscall.len();
        let prot = syscall.prot();

        // On replay, we can't actually map the original file because it may not
        // exist. Instead, change this to be an anonymous mapping and write the
        // bytes that we recorded to it. This has the same effect, but without
        // requiring to map a real file.
        //
        // Write permission is also needed to be able to write the recorded
        // bytes to the mapping. After the data has been written, it can be
        // reset to the original protection value with a call to `mprotect`.
        let ptr = guest
            .inject_with_retry(
                syscall
                    .with_addr(Some(addr.cast::<libc::c_void>().into()))
                    .with_prot(prot | ProtFlags::PROT_WRITE)
                    .with_flags(flags | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            )
            .await?;

        // Make sure we got the pointer we wanted.
        assert_eq!(
            ptr as usize, event.addr,
            "Failed to inject mmap at desired address"
        );

        // Fill in the memory map.
        guest.memory().write_exact(addr.cast(), &event.buf).unwrap();

        // Reset the page protection to the original value (if it didn't already
        // have PROT_WRITE) so that we still correctly mimic the page protection
        // of the original mapping.
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(
                    Mprotect::new()
                        .with_addr(Some(addr.cast()))
                        .with_len(len)
                        .with_protection(prot),
                )
                .await?;
        }

        Ok(ptr)
    }

    /// Replays a guest-semantic `madvise` recorded by the recorder's
    /// `handle_madvise`: applies the advice live to the recorded prefix, then
    /// restores the file-backed contents the recording observed.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/3537): Audit the madvise refill and WIPEONFORK prefix.
    pub(super) async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Madvise,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Madvise)?;
        let len = syscall.len();
        assert!(
            event.live_len <= len,
            "Recorded madvise prefix {} exceeds the guest length {}",
            event.live_len,
            len
        );

        if event.live_len == len {
            let result = guest.inject_with_retry(syscall).await;
            assert_eq!(
                result,
                event.result,
                "Replayed madvise({:?}, {}, {}) diverged from the recording",
                syscall.addr(),
                len,
                syscall.advice()
            );
        } else if event.live_len > 0 {
            // The recording failed at a file mapping after this prefix; the
            // prefix's own outcome is subsumed by the recorded error.
            let _ = guest
                .inject_with_retry(syscall.with_len(event.live_len))
                .await;
        }

        for refill in &event.refills {
            let prot = ProtFlags::from_bits_truncate(refill.prot);
            if !prot.contains(ProtFlags::PROT_WRITE) {
                guest
                    .inject_with_retry(refill_protection(refill, prot | ProtFlags::PROT_WRITE))
                    .await?;
            }
            // This is safe since the recorder only records mapped addresses.
            let addr = unsafe { AddrMut::<u8>::from_raw_unchecked(refill.addr) };
            guest.memory().write_exact(addr, &refill.bytes).unwrap();
            if !prot.contains(ProtFlags::PROT_WRITE) {
                guest
                    .inject_with_retry(refill_protection(refill, prot))
                    .await?;
            }
        }

        event.result
    }
}

fn refill_protection(refill: &MadviseRefill, protection: ProtFlags) -> Mprotect {
    // This is safe since the recorder only records mapped addresses.
    let addr = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(refill.addr) };
    Mprotect::new()
        .with_addr(Some(addr))
        .with_len(refill.bytes.len())
        .with_protection(protection)
}
