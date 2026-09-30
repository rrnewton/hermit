/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Guest output of the captured clock syscalls, for record and replay.
//!
//! `clock_gettime`, `gettimeofday` and `time` store their results in guest
//! memory, and Linux can store part of that output before it returns
//! `EFAULT`: `gettimeofday` stores `tv_sec`, then `tv_usec`, then the
//! timezone, and stops at the first store that faults. A post-call snapshot
//! alone cannot distinguish those stores from bytes that were merely readable
//! afterwards, so an `EFAULT` event also keeps the same readable prefix from
//! before the call. A byte that differs between the two was stored by the
//! call. Every other byte was not, and replay must not write it.
//!
//! Replay of an `EFAULT` event runs three phases across all of the call's
//! destinations together. It first validates that guest memory still holds
//! every pre-call byte, before any write. It then writes only the bytes that
//! differ, in Linux's store order, and finally verifies that every destination
//! holds its post-call bytes. Validating everything first matters when
//! destinations alias: once the timeval is restored, an aliased timezone no
//! longer holds its pre-call bytes. For the same reason, the write phase reads
//! each destination again just before writing it and skips a byte that already
//! holds its post-call value. Two mappings of one page alias without sharing an
//! address: with the timeval in a writable mapping and the timezone in a
//! read-only mapping of the same page, Linux stores the timeval and faults on
//! the timezone, yet the timezone's snapshots differ. Restoring the timeval
//! already restores those bytes, and writing them again through the read-only
//! mapping would fail. Successful calls use the same phases, requiring readable
//! destinations first and then writing every byte.

use std::fmt::Display;
use std::io;

use reverie::Errno;
use reverie::Error;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::AddrSliceMut;
use reverie::syscalls::MemoryAccess;

use crate::event::ClockOutput;

/// The largest captured output: a `timespec` or a `timeval`.
const MAX_OUTPUT: usize = 16;

// TODO-HUMAN-REVIEW(PR-3420): Review captured clock copyout and errno fidelity.
// https://github.com/rrnewton/hermit/pull/3420
/// One guest output of a captured clock syscall, named for diagnostics.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Destination {
    syscall: &'static str,
    name: &'static str,
    address: Option<usize>,
    size: usize,
}

impl Destination {
    pub(crate) fn new<T: Copy>(
        syscall: &'static str,
        name: &'static str,
        address: Option<AddrMut<'_, T>>,
    ) -> Self {
        let size = std::mem::size_of::<T>();
        assert!(size <= MAX_OUTPUT);
        Self {
            syscall,
            name,
            address: address.map(|address| address.as_raw()),
            size,
        }
    }

    fn failure(&self, message: impl Display) -> Error {
        Error::Tool(anyhow::anyhow!(
            "captured clock output: {} {}: {message}",
            self.syscall,
            self.name
        ))
    }

    /// The readable prefix of this destination, ending at its first unreadable
    /// word. Only an address overflow is an error.
    fn read_prefix<M: MemoryAccess>(&self, memory: &M) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::with_capacity(self.size);
        let Some(address) = self.address else {
            return Ok(bytes);
        };
        // Ptrace's small read uses PEEKDATA, which can capture write-only
        // output. Align each word so that a short output prefix next to an
        // unmapped page never makes PEEKDATA cross that page. Only output
        // bytes enter the result.
        while bytes.len() < self.size {
            let raw = address
                .checked_add(bytes.len())
                .ok_or_else(|| self.failure("output address overflow"))?;
            let aligned = raw & !7;
            let word = Addr::<u64>::from_raw(aligned)
                .ok_or(Errno::EFAULT)
                .and_then(|address| memory.read_value(address));
            let Ok(word) = word else {
                break;
            };
            let word = word.to_ne_bytes();
            let start = raw - aligned;
            let count = (8 - start).min(self.size - bytes.len());
            bytes.extend_from_slice(&word[start..start + count]);
        }
        Ok(bytes)
    }

    /// Reads this destination before the recorder injects the call.
    ///
    /// Only an `EFAULT` event keeps these bytes (see [`Self::capture`]).
    /// Together with the post-call bytes, they prove which bytes Linux stored
    /// before it faulted.
    ///
    /// Cost: the result is unknown until the call returns, so every recorded
    /// call with a non-NULL destination pays for this read. Under ptrace it is
    /// one `PTRACE_PEEKDATA` for each aligned word the destination touches:
    /// two for an aligned `timespec` or `timeval`, one for an aligned `time_t`
    /// or `timezone`, and one more for each when unaligned, so at most five for
    /// `gettimeofday`. In-process backends use one `process_vm_readv` per word
    /// instead. That is acceptable because the tracee is already stopped at the
    /// syscall, so the read adds no guest stop, and because it repeats the
    /// post-call read of the same words that every successful call already
    /// performs, next to an injected syscall that costs several ptrace stops.
    /// Only recording pays for this read. Replay reads each non-NULL
    /// destination of a successful or `EFAULT` call once before any write and
    /// once after all writes, and after `EFAULT` once more just before writing
    /// it. Replay reads nothing for any other failure, or for outputs that are
    /// malformed or contradict each other where they overlap.
    pub(crate) fn pre_call<M: MemoryAccess>(&self, memory: &M) -> Result<Vec<u8>, Error> {
        self.read_prefix(memory)
    }

    /// Captures this destination after the call, keeping `pre_call` (from
    /// [`Self::pre_call`]) only when the call failed with `EFAULT`.
    pub(crate) fn capture<M: MemoryAccess>(
        &self,
        memory: &M,
        result: Result<i64, Errno>,
        pre_call: Vec<u8>,
    ) -> Result<ClockOutput, Error> {
        let mut output = ClockOutput {
            pointer_present: self.address.is_some(),
            bytes: Vec::new(),
            pre_call_bytes: Vec::new(),
        };
        if self.address.is_none() {
            return Ok(output);
        }
        match result {
            Ok(_) => {
                output.bytes = self.read_prefix(memory)?;
                if output.bytes.len() != self.size {
                    return Err(self.failure("cannot read successful syscall output"));
                }
            }
            Err(Errno::EFAULT) => {
                output.bytes = self.read_prefix(memory)?;
                // Equal lengths put both snapshots over the same bytes, which
                // is what lets replay compare them byte for byte.
                if output.bytes.len() != pre_call.len() {
                    return Err(self.failure(format_args!(
                        "readable output changed during the call: {} bytes before, {} after",
                        pre_call.len(),
                        output.bytes.len()
                    )));
                }
                output.pre_call_bytes = pre_call;
            }
            // Linux reports the other failures, such as clock_gettime's
            // EINVAL for an invalid clock, before any store.
            Err(_) => {}
        }
        Ok(output)
    }

    fn check_shape(&self, result: Result<i64, Errno>, output: &ClockOutput) -> Result<(), Error> {
        let pre_call = &output.pre_call_bytes;
        let consistent = output.pointer_present == self.address.is_some()
            && output.bytes.len() <= self.size
            && match (self.address, result) {
                (None, _) => output.bytes.is_empty() && pre_call.is_empty(),
                (Some(_), Ok(_)) => output.bytes.len() == self.size && pre_call.is_empty(),
                (Some(_), Err(Errno::EFAULT)) => pre_call.len() == output.bytes.len(),
                (Some(_), Err(_)) => output.bytes.is_empty() && pre_call.is_empty(),
            };
        if consistent {
            Ok(())
        } else {
            Err(self.failure("recorded pointer shape or output length diverged"))
        }
    }

    fn restore_byte<M: MemoryAccess>(
        &self,
        memory: &mut M,
        address: usize,
        offset: usize,
        byte: u8,
    ) -> Result<(), Error> {
        let raw = address
            .checked_add(offset)
            .ok_or_else(|| self.failure("output address overflow"))?;
        let target = AddrMut::<u8>::from_raw(raw).ok_or_else(|| self.failure("null output"))?;
        // SAFETY: the one-byte remote slice is only handed to the kernel as an
        // iovec; it never becomes a Rust reference to guest memory.
        let mut remote = unsafe { AddrSliceMut::from_raw_parts(target, 1) };
        // Never use MemoryAccess::write here: ptrace's eight-byte POKEDATA
        // optimization bypasses user protections. A one-byte remote iovec both
        // respects those protections and restores an unaligned output that
        // ends partway through a word.
        let write = memory.write_vectored(
            &[io::IoSlice::new(std::slice::from_ref(&byte))],
            // SAFETY: as above, the remote iovec is only passed to the kernel.
            &mut [unsafe { remote.as_ioslice_mut() }],
        );
        match write {
            Ok(1) => Ok(()),
            write => Err(self.failure(format_args!(
                "cannot restore syscall output at byte {offset} (write returned {write:?})"
            ))),
        }
    }
}

/// Whether two recorded outputs, each starting at its guest address, hold
/// different bytes anywhere their address ranges overlap. The range
/// arithmetic uses `u128`, so it cannot overflow.
fn overlap_disagrees(
    first_address: usize,
    first: &[u8],
    second_address: usize,
    second: &[u8],
) -> bool {
    let (first_start, second_start) = (first_address as u128, second_address as u128);
    let start = first_start.max(second_start);
    let end = (first_start + first.len() as u128).min(second_start + second.len() as u128);
    (start..end).any(|address| {
        first[(address - first_start) as usize] != second[(address - second_start) as usize]
    })
}

/// Replays one captured call's outputs, listed in Linux's store order.
///
/// Every destination passes its shape check before any guest access. Outputs
/// whose address ranges overlap must then agree on every byte they share: on
/// the post-call bytes, and after `EFAULT` also on the pre-call bytes. A
/// faithful recording reads both from the same memory, so a disagreement is
/// refused before any guest access too. Every destination then passes
/// validation before the first write, and every write must succeed. A
/// mismatch or failed write is a typed Tool error that names the syscall and
/// the destination.
pub(crate) fn replay<M: MemoryAccess>(
    memory: &mut M,
    result: Result<i64, Errno>,
    outputs: &[(Destination, &ClockOutput)],
) -> Result<(), Error> {
    for (destination, output) in outputs {
        destination.check_shape(result, output)?;
    }
    let efault = match result {
        Ok(_) => false,
        Err(Errno::EFAULT) => true,
        // The shape check has already required these outputs to be empty.
        Err(_) => return Ok(()),
    };

    // Refuse overlapping outputs that contradict each other before any guest
    // access. No guest memory can satisfy both: a pre-call disagreement would
    // fail validation after reads, and a post-call one would fail the final
    // verification only after writes.
    for (later_index, (later, later_output)) in outputs.iter().enumerate() {
        let Some(later_address) = later.address else {
            continue;
        };
        for (earlier, earlier_output) in &outputs[..later_index] {
            let Some(earlier_address) = earlier.address else {
                continue;
            };
            let post = overlap_disagrees(
                earlier_address,
                &earlier_output.bytes,
                later_address,
                &later_output.bytes,
            );
            let pre = efault
                && overlap_disagrees(
                    earlier_address,
                    &earlier_output.pre_call_bytes,
                    later_address,
                    &later_output.pre_call_bytes,
                );
            if post || pre {
                return Err(later.failure(format_args!(
                    "recorded bytes contradict {} {} where the two outputs overlap",
                    earlier.syscall, earlier.name
                )));
            }
        }
    }

    // 1. Validate every destination before any write. After EFAULT, guest
    // memory must still hold the recorded pre-call bytes, so that replay
    // cannot create a store Linux never made or hide a divergence. A
    // successful copyout requires every destination to be readable.
    for (destination, output) in outputs {
        if destination.address.is_none() {
            continue;
        }
        let current = destination.read_prefix(memory)?;
        if !efault {
            if current.len() != destination.size {
                return Err(destination.failure("cannot read successful syscall output"));
            }
        } else if current.len() != output.pre_call_bytes.len() {
            return Err(destination.failure(format_args!(
                "output mapping diverged: {} readable bytes before the recorded call, {} now",
                output.pre_call_bytes.len(),
                current.len()
            )));
        } else if current != output.pre_call_bytes {
            return Err(destination.failure("guest bytes differ from the recorded pre-call bytes"));
        }
    }

    // 2. Apply in the listed order. A successful copyout writes every byte.
    // After EFAULT, write only a byte whose two snapshots differ, which the
    // call stored, and skip it when guest memory, read just before this
    // destination's writes, already holds its post-call value. That happens
    // when an earlier write in this loop reached it through another mapping
    // of the same memory, a physical alias that no address comparison can
    // see. Writing it again would change nothing, and Linux may never have
    // stored it through this destination: with tv in a writable mapping and
    // tz in a read-only mapping of one page, Linux stores tv and faults on tz,
    // yet tz's snapshots differ, so writing tz would refuse a faithful event.
    // Phase 3 still verifies every byte.
    for (destination, output) in outputs {
        let Some(address) = destination.address else {
            continue;
        };
        let current = if efault {
            destination.read_prefix(memory)?
        } else {
            Vec::new()
        };
        for (offset, byte) in output.bytes.iter().enumerate() {
            if efault
                && (output.pre_call_bytes[offset] == *byte || current.get(offset) == Some(byte))
            {
                continue;
            }
            destination.restore_byte(memory, address, offset, *byte)?;
        }
    }

    // 3. Verify every destination, including unchanged bytes on protected
    // pages. This also catches aliased outputs whose recorded bytes disagree.
    for (destination, output) in outputs {
        if destination.address.is_some() && destination.read_prefix(memory)? != output.bytes {
            return Err(destination.failure("restored bytes differ from recording"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Timespec;
    use reverie::syscalls::Timeval;
    use reverie::syscalls::Timezone;

    use super::*;

    /// Local memory that counts reads and writes. With `write_result` set, it
    /// returns that result for every write without touching memory.
    struct TestMemory {
        memory: LocalMemory,
        reads: Cell<usize>,
        writes: usize,
        write_result: Option<Result<usize, Errno>>,
    }

    impl TestMemory {
        fn new(write_result: Option<Result<usize, Errno>>) -> Self {
            Self {
                memory: LocalMemory::new(),
                reads: Cell::new(0),
                writes: 0,
                write_result,
            }
        }
    }

    impl MemoryAccess for TestMemory {
        fn read_vectored(
            &self,
            from: &[io::IoSlice],
            to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.reads.set(self.reads.get() + 1);
            self.memory.read_vectored(from, to)
        }

        fn write_vectored(
            &mut self,
            from: &[io::IoSlice],
            to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.writes += 1;
            match self.write_result {
                Some(result) => result,
                None => self.memory.write_vectored(from, to),
            }
        }

        fn write(&mut self, _addr: AddrMut<u8>, _buf: &[u8]) -> Result<usize, Errno> {
            panic!("captured clock replay must never use MemoryAccess::write");
        }
    }

    /// Two anonymous read-write pages, unmapped on drop.
    struct Pages {
        base: usize,
        page: usize,
    }

    impl Pages {
        fn new() -> Self {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    page * 2,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(base, libc::MAP_FAILED);
            Self {
                base: base as usize,
                page,
            }
        }

        /// The address where the second page starts.
        fn boundary(&self) -> usize {
            self.base + self.page
        }

        fn protect_second(&self, protection: libc::c_int) {
            let second = self.boundary() as *mut libc::c_void;
            assert_eq!(unsafe { libc::mprotect(second, self.page, protection) }, 0);
        }

        fn fill(&self, address: usize, bytes: &[u8]) {
            assert!(address >= self.base && address + bytes.len() <= self.base + self.page * 2);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len())
            };
        }

        fn bytes(&self, address: usize, len: usize) -> Vec<u8> {
            assert!(address >= self.base && address + len <= self.base + self.page * 2);
            (0..len)
                .map(|offset| unsafe { std::ptr::read_volatile((address + offset) as *const u8) })
                .collect()
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            unsafe { libc::munmap(self.base as *mut libc::c_void, self.page * 2) };
        }
    }

    /// One memfd page mapped twice with `MAP_SHARED`, so that a store through
    /// either view is visible through the other: a physical alias that no
    /// comparison of guest addresses can see. Both views are unmapped on drop.
    struct AliasedPage {
        view_a: usize,
        view_b: usize,
        page: usize,
    }

    impl AliasedPage {
        fn new(protection_a: libc::c_int, protection_b: libc::c_int) -> Self {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let fd =
                unsafe { libc::memfd_create(c"clock-output-alias".as_ptr(), libc::MFD_CLOEXEC) };
            assert!(fd >= 0, "memfd_create: {}", io::Error::last_os_error());
            assert_eq!(unsafe { libc::ftruncate(fd, page as libc::off_t) }, 0);
            let map = |protection| {
                let view = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        page,
                        protection,
                        libc::MAP_SHARED,
                        fd,
                        0,
                    )
                };
                assert_ne!(view, libc::MAP_FAILED);
                view as usize
            };
            let (view_a, view_b) = (map(protection_a), map(protection_b));
            // The two mappings keep the memfd alive.
            assert_eq!(unsafe { libc::close(fd) }, 0);
            Self {
                view_a,
                view_b,
                page,
            }
        }

        fn check_range(&self, address: usize, len: usize) {
            assert!(
                [self.view_a, self.view_b]
                    .into_iter()
                    .any(|view| address >= view && address + len <= view + self.page)
            );
        }

        /// Stores through a writable view.
        fn fill(&self, address: usize, bytes: &[u8]) {
            self.check_range(address, bytes.len());
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len())
            };
        }

        fn bytes(&self, address: usize, len: usize) -> Vec<u8> {
            self.check_range(address, len);
            (0..len)
                .map(|offset| unsafe { std::ptr::read_volatile((address + offset) as *const u8) })
                .collect()
        }
    }

    impl Drop for AliasedPage {
        fn drop(&mut self) {
            for view in [self.view_a, self.view_b] {
                unsafe { libc::munmap(view as *mut libc::c_void, self.page) };
            }
        }
    }

    fn gettimeofday(tv: usize, tz: usize) -> Result<i64, Errno> {
        let result = unsafe { libc::syscall(libc::SYS_gettimeofday, tv, tz) };
        if result == -1 {
            Err(Errno::last())
        } else {
            Ok(result)
        }
    }

    fn tool_message(error: Error) -> String {
        let Error::Tool(error) = error else {
            panic!("expected a Tool error, got {error:?}");
        };
        error.to_string()
    }

    fn output(bytes: Vec<u8>, pre_call_bytes: Vec<u8>) -> ClockOutput {
        ClockOutput {
            pointer_present: true,
            bytes,
            pre_call_bytes,
        }
    }

    #[test]
    fn replay_restores_partial_output_before_recorded_efault() {
        let mut target = [0x5a5a_5a5a_5a5a_5a5a_u64; 2];
        let address = AddrMut::<Timespec>::from_raw(target.as_mut_ptr() as usize);
        let destination = Destination::new("clock_gettime", "tp", address);
        let memory = LocalMemory::new();
        let pre_call = destination.pre_call(&memory).unwrap();
        let mut output = destination
            .capture(&memory, Err(Errno::EFAULT), pre_call)
            .unwrap();
        assert_eq!(output.pre_call_bytes, vec![0x5a; 16]);
        output.bytes[..4].copy_from_slice(&12345678_u32.to_ne_bytes());
        let expected = output.bytes.clone();
        let mut memory = TestMemory::new(None);
        replay(&mut memory, Err(Errno::EFAULT), &[(destination, &output)]).unwrap();
        assert_eq!(
            destination.read_prefix(&LocalMemory::new()).unwrap(),
            expected
        );
        assert_eq!(memory.writes, 4, "only the four changed bytes are written");
    }

    #[test]
    fn recorded_efault_does_not_hide_failure_to_restore_changed_bytes() {
        for (write_result, message) in [
            (
                Err(Errno::EFAULT),
                "captured clock output: time tloc: cannot restore syscall output at byte 0 \
                 (write returned Err(EFAULT))",
            ),
            (
                Ok(0),
                "captured clock output: time tloc: cannot restore syscall output at byte 0 \
                 (write returned Ok(0))",
            ),
            // A write that claims success without storing is caught by the
            // final verification.
            (
                Ok(1),
                "captured clock output: time tloc: restored bytes differ from recording",
            ),
        ] {
            let mut target = 7_u64;
            let address = AddrMut::<libc::time_t>::from_raw(&mut target as *mut u64 as usize);
            let destination = Destination::new("time", "tloc", address);
            let mut memory = TestMemory::new(Some(write_result));
            let recorded = output(11_u64.to_ne_bytes().to_vec(), 7_u64.to_ne_bytes().to_vec());
            let error =
                replay(&mut memory, Err(Errno::EFAULT), &[(destination, &recorded)]).unwrap_err();
            assert_eq!(tool_message(error), message);
            assert_eq!(target, 7);
            assert_eq!(memory.writes, 1);
        }
    }

    #[test]
    fn unchanged_protected_error_output_is_not_written() {
        for write_result in [Err(Errno::EFAULT), Ok(0)] {
            let mut target = 7_u64;
            let address = AddrMut::<libc::time_t>::from_raw(&mut target as *mut u64 as usize);
            let destination = Destination::new("time", "tloc", address);
            let mut memory = TestMemory::new(Some(write_result));
            let unchanged = output(target.to_ne_bytes().to_vec(), target.to_ne_bytes().to_vec());
            replay(
                &mut memory,
                Err(Errno::EFAULT),
                &[(destination, &unchanged)],
            )
            .unwrap();
            assert_eq!(memory.writes, 0);
            let successful = output(target.to_ne_bytes().to_vec(), Vec::new());
            let error = replay(&mut memory, Ok(0), &[(destination, &successful)]).unwrap_err();
            assert_eq!(
                tool_message(error),
                format!(
                    "captured clock output: time tloc: cannot restore syscall output at byte 0 \
                     (write returned {write_result:?})"
                )
            );
            assert_eq!(
                memory.writes, 1,
                "successful copyout must still require writable memory"
            );
        }
    }

    #[test]
    fn invalid_clock_id_does_not_read_or_write_the_destination() {
        let mut target = [0x5a_u64; 2];
        for raw in [1, target.as_mut_ptr() as usize] {
            let destination =
                Destination::new("clock_gettime", "tp", AddrMut::<Timespec>::from_raw(raw));
            // The recorder reads before it knows the result; capture then
            // discards those bytes for a failure Linux reports before any store.
            let pre_call = destination.pre_call(&LocalMemory::new()).unwrap();
            let mut memory = TestMemory::new(None);
            let output = destination
                .capture(&memory, Err(Errno::EINVAL), pre_call)
                .unwrap();
            assert!(output.pointer_present);
            assert!(output.bytes.is_empty());
            assert!(output.pre_call_bytes.is_empty());
            replay(&mut memory, Err(Errno::EINVAL), &[(destination, &output)]).unwrap();
            assert_eq!(memory.reads.get(), 0);
            assert_eq!(memory.writes, 0);
        }
        assert_eq!(target, [0x5a; 2]);
    }

    #[test]
    fn malformed_clock_output_refuses_before_copyout() {
        let mut target = 7_u64;
        let address = AddrMut::<libc::time_t>::from_raw(&mut target as *mut u64 as usize);
        let destination = Destination::new("time", "tloc", address);
        let absent = Destination::new("time", "tloc", None::<AddrMut<libc::time_t>>);
        for (destination, result, pointer_present, bytes, pre_call_bytes) in [
            (destination, Ok(0), true, vec![0; 9], vec![]),
            (destination, Ok(0), true, vec![0; 7], vec![]),
            (destination, Ok(0), false, vec![0; 8], vec![]),
            (destination, Err(Errno::EINVAL), true, vec![0; 8], vec![]),
            // Pre-call bytes belong only to EFAULT, and then cover exactly the
            // recorded post-call bytes.
            (destination, Ok(0), true, vec![0; 8], vec![0; 8]),
            (destination, Err(Errno::EINVAL), true, vec![], vec![0; 8]),
            (
                destination,
                Err(Errno::EFAULT),
                true,
                vec![0; 8],
                vec![0; 7],
            ),
            (destination, Err(Errno::EFAULT), true, vec![], vec![7]),
            (
                destination,
                Err(Errno::EFAULT),
                true,
                vec![0; 9],
                vec![0; 9],
            ),
            (absent, Err(Errno::EFAULT), false, vec![], vec![7]),
        ] {
            let mut memory = TestMemory::new(None);
            let recorded = ClockOutput {
                pointer_present,
                bytes,
                pre_call_bytes,
            };
            let error = replay(&mut memory, result, &[(destination, &recorded)]).unwrap_err();
            assert_eq!(
                tool_message(error),
                "captured clock output: time tloc: recorded pointer shape or output length diverged"
            );
            assert_eq!(target, 7);
            assert_eq!(memory.reads.get(), 0);
            assert_eq!(memory.writes, 0);
        }
    }

    #[test]
    fn efault_replay_refuses_a_timezone_linux_never_stored() {
        // gettimeofday((void *)1, &tz): Linux faults on tv_sec and never
        // reaches tz, so tz keeps its pre-call bytes.
        let pages = Pages::new();
        let tz_address = pages.base;
        pages.fill(tz_address, &[0x5a; 8]);
        let tv = Destination::new("gettimeofday", "tv", AddrMut::<Timeval>::from_raw(1));
        let tz = Destination::new(
            "gettimeofday",
            "tz",
            AddrMut::<Timezone>::from_raw(tz_address),
        );
        let memory = LocalMemory::new();
        let (tv_before, tz_before) = (tv.pre_call(&memory).unwrap(), tz.pre_call(&memory).unwrap());
        let result = gettimeofday(1, tz_address);
        assert_eq!(result, Err(Errno::EFAULT));
        let timeval = tv.capture(&memory, result, tv_before).unwrap();
        let timezone = tz.capture(&memory, result, tz_before).unwrap();
        assert!(timeval.bytes.is_empty() && timeval.pre_call_bytes.is_empty());
        assert_eq!(timezone.pre_call_bytes, vec![0x5a; 8]);
        assert_eq!(timezone.bytes, vec![0x5a; 8]);

        // Replay meets different timezone bytes. Restoring the post-call
        // snapshot would create a store Linux never made and hide this
        // divergence, so replay refuses before writing anything.
        pages.fill(tz_address, &[0xdd; 8]);
        let mut memory = TestMemory::new(None);
        let error = replay(&mut memory, result, &[(tv, &timeval), (tz, &timezone)]).unwrap_err();
        assert_eq!(
            tool_message(error),
            "captured clock output: gettimeofday tz: guest bytes differ from the recorded \
             pre-call bytes"
        );
        assert_eq!(memory.writes, 0);
        assert_eq!(pages.bytes(tz_address, 8), vec![0xdd; 8]);
    }

    #[test]
    fn efault_replay_validates_aliased_outputs_before_either_write() {
        // tv ends on a writable page with tv_usec on a read-only page, and tz
        // aliases tv_sec. Linux stores tv_sec, faults on tv_usec and never
        // reaches tz, but both destinations observe the stored tv_sec.
        let pages = Pages::new();
        let tv_address = pages.boundary() - 8;
        pages.fill(tv_address, &[0x5a; 16]);
        pages.protect_second(libc::PROT_READ);
        let tv = Destination::new(
            "gettimeofday",
            "tv",
            AddrMut::<Timeval>::from_raw(tv_address),
        );
        let tz = Destination::new(
            "gettimeofday",
            "tz",
            AddrMut::<Timezone>::from_raw(tv_address),
        );
        let memory = LocalMemory::new();
        let (tv_before, tz_before) = (tv.pre_call(&memory).unwrap(), tz.pre_call(&memory).unwrap());
        let result = gettimeofday(tv_address, tv_address);
        assert_eq!(result, Err(Errno::EFAULT));
        let timeval = tv.capture(&memory, result, tv_before).unwrap();
        let timezone = tz.capture(&memory, result, tz_before).unwrap();
        assert_eq!(timeval.pre_call_bytes, vec![0x5a; 16]);
        assert_ne!(timeval.bytes[..8], [0x5a; 8], "Linux stored tv_sec");
        assert_eq!(timeval.bytes[8..], [0x5a; 8], "Linux faulted on tv_usec");
        assert_eq!(timezone.pre_call_bytes, vec![0x5a; 8]);
        assert_eq!(timezone.bytes, timeval.bytes[..8]);

        // Joint validation accepts the recorded pre-call state, and both
        // destinations end at their post-call bytes.
        pages.fill(tv_address, &[0x5a; 8]);
        let mut memory = TestMemory::new(None);
        replay(&mut memory, result, &[(tv, &timeval), (tz, &timezone)]).unwrap();
        assert_eq!(pages.bytes(tv_address, 16), timeval.bytes);
        assert_eq!(pages.bytes(tv_address, 8), timezone.bytes);

        // A design that validates each destination only after restoring the
        // previous one refuses this faithful event: tz then already holds the
        // restored tv_sec instead of its pre-call bytes.
        pages.fill(tv_address, &[0x5a; 8]);
        replay(&mut memory, result, &[(tv, &timeval)]).unwrap();
        let error = replay(&mut memory, result, &[(tz, &timezone)]).unwrap_err();
        assert_eq!(
            tool_message(error),
            "captured clock output: gettimeofday tz: guest bytes differ from the recorded \
             pre-call bytes"
        );
    }

    #[test]
    fn efault_replay_skips_bytes_an_aliased_write_already_restored() {
        // tv is in a writable view of a page and tz in a read-only view of the
        // same page, at the same offset. Linux stores the whole timeval through
        // the writable view and then faults on tz, which it never stored. The
        // tz snapshots still differ, because the read-only view shows tv_sec.
        let alias = AliasedPage::new(libc::PROT_READ | libc::PROT_WRITE, libc::PROT_READ);
        alias.fill(alias.view_a, &[0x5a; 16]);
        let tv = Destination::new(
            "gettimeofday",
            "tv",
            AddrMut::<Timeval>::from_raw(alias.view_a),
        );
        let tz = Destination::new(
            "gettimeofday",
            "tz",
            AddrMut::<Timezone>::from_raw(alias.view_b),
        );
        let memory = LocalMemory::new();
        let (tv_before, tz_before) = (tv.pre_call(&memory).unwrap(), tz.pre_call(&memory).unwrap());
        let result = gettimeofday(alias.view_a, alias.view_b);
        assert_eq!(result, Err(Errno::EFAULT));
        let timeval = tv.capture(&memory, result, tv_before).unwrap();
        let timezone = tz.capture(&memory, result, tz_before).unwrap();
        assert_ne!(timeval.bytes[..8], [0x5a; 8], "Linux stored tv_sec");
        assert_eq!(timezone.pre_call_bytes, vec![0x5a; 8]);
        assert_eq!(timezone.bytes, timeval.bytes[..8]);

        // Restoring tv also restores tz through the alias. Writing tz's
        // changed bytes again would fail on the read-only view and refuse this
        // faithful event, so replay skips a byte that already holds its
        // post-call value.
        alias.fill(alias.view_a, &[0x5a; 16]);
        let mut memory = TestMemory::new(None);
        replay(&mut memory, result, &[(tv, &timeval), (tz, &timezone)]).unwrap();
        let changed = timeval.bytes.iter().filter(|&&byte| byte != 0x5a).count();
        assert_eq!(
            memory.writes, changed,
            "only the changed timeval bytes are written"
        );
        assert_eq!(alias.bytes(alias.view_a, 16), timeval.bytes);
        assert_eq!(alias.bytes(alias.view_b, 8), timezone.bytes);
    }

    #[test]
    fn efault_replay_refuses_a_stored_byte_it_cannot_write() {
        // gettimeofday(&tv, (void *)1) stores the whole timeval and then
        // faults on tz. At replay the same bytes are on a read-only page.
        let pages = Pages::new();
        let tv_address = pages.boundary();
        pages.fill(tv_address, &[0x5a; 16]);
        let tv = Destination::new(
            "gettimeofday",
            "tv",
            AddrMut::<Timeval>::from_raw(tv_address),
        );
        let tz = Destination::new("gettimeofday", "tz", AddrMut::<Timezone>::from_raw(1));
        let memory = LocalMemory::new();
        let (tv_before, tz_before) = (tv.pre_call(&memory).unwrap(), tz.pre_call(&memory).unwrap());
        let result = gettimeofday(tv_address, 1);
        assert_eq!(result, Err(Errno::EFAULT));
        let timeval = tv.capture(&memory, result, tv_before).unwrap();
        let timezone = tz.capture(&memory, result, tz_before).unwrap();
        let first_stored = (0..16)
            .find(|&offset| timeval.bytes[offset] != timeval.pre_call_bytes[offset])
            .expect("Linux stored the timeval");

        pages.fill(tv_address, &[0x5a; 16]);
        pages.protect_second(libc::PROT_READ);
        let mut memory = TestMemory::new(None);
        let error = replay(&mut memory, result, &[(tv, &timeval), (tz, &timezone)]).unwrap_err();
        assert_eq!(
            tool_message(error),
            format!(
                "captured clock output: gettimeofday tv: cannot restore syscall output at byte \
                 {first_stored} (write returned Ok(0))"
            )
        );
        assert_eq!(memory.writes, 1);
        assert_eq!(pages.bytes(tv_address, 16), vec![0x5a; 16]);
    }

    #[test]
    fn recorded_efault_refuses_a_readable_prefix_that_changed_during_the_call() {
        let pages = Pages::new();
        let tv_address = pages.boundary() - 8;
        pages.protect_second(libc::PROT_READ);
        let tv = Destination::new(
            "gettimeofday",
            "tv",
            AddrMut::<Timeval>::from_raw(tv_address),
        );
        let memory = LocalMemory::new();
        let pre_call = tv.pre_call(&memory).unwrap();
        assert_eq!(pre_call.len(), 16);
        // The second page stops being readable between the two snapshots, so
        // they no longer describe the same bytes.
        pages.protect_second(libc::PROT_NONE);
        let error = tv
            .capture(&memory, Err(Errno::EFAULT), pre_call)
            .unwrap_err();
        assert_eq!(
            tool_message(error),
            "captured clock output: gettimeofday tv: readable output changed during the call: \
             16 bytes before, 8 after"
        );
    }

    #[test]
    fn successful_replay_verifies_every_output_after_every_write() {
        // tz, at offset 8 of a second mapping of tv's page, aliases tv_usec
        // physically, which no comparison of addresses can see. A faithful
        // recording reads both snapshots from the same memory, so their shared
        // bytes agree; these disagree, and only the final verification can
        // catch it.
        let read_write = libc::PROT_READ | libc::PROT_WRITE;
        let alias = AliasedPage::new(read_write, read_write);
        let tv = Destination::new(
            "gettimeofday",
            "tv",
            AddrMut::<Timeval>::from_raw(alias.view_a),
        );
        let tz = Destination::new(
            "gettimeofday",
            "tz",
            AddrMut::<Timezone>::from_raw(alias.view_b + 8),
        );
        let mut timeval_bytes = vec![0x11; 8];
        timeval_bytes.extend([0x22; 8]);
        let timeval = output(timeval_bytes, Vec::new());
        let timezone = output(vec![0x33; 8], Vec::new());
        let mut memory = TestMemory::new(None);
        let error = replay(&mut memory, Ok(0), &[(tv, &timeval), (tz, &timezone)]).unwrap_err();
        assert_eq!(
            tool_message(error),
            "captured clock output: gettimeofday tv: restored bytes differ from recording"
        );
        assert_eq!(memory.writes, 24);
    }

    #[test]
    fn replay_refuses_overlapping_outputs_that_contradict_before_any_access() {
        // tz overlaps tv_usec. A faithful recording reads both snapshots from
        // the same memory, so their shared bytes agree. These disagree: first
        // the post-call bytes of a successful call, then the pre-call bytes
        // of an EFAULT call whose post-call bytes agree.
        let mut target = [0_u8; 16];
        let tv_address = target.as_mut_ptr() as usize;
        let tv = Destination::new(
            "gettimeofday",
            "tv",
            AddrMut::<Timeval>::from_raw(tv_address),
        );
        let tz = Destination::new(
            "gettimeofday",
            "tz",
            AddrMut::<Timezone>::from_raw(tv_address + 8),
        );
        let mut timeval_bytes = vec![0x11; 8];
        timeval_bytes.extend([0x22; 8]);
        for (result, timeval, timezone) in [
            (
                Ok(0),
                output(timeval_bytes.clone(), Vec::new()),
                output(vec![0x33; 8], Vec::new()),
            ),
            (
                Err(Errno::EFAULT),
                output(timeval_bytes.clone(), vec![0; 16]),
                output(vec![0x22; 8], vec![0x33; 8]),
            ),
        ] {
            let mut memory = TestMemory::new(None);
            let error =
                replay(&mut memory, result, &[(tv, &timeval), (tz, &timezone)]).unwrap_err();
            assert_eq!(
                tool_message(error),
                "captured clock output: gettimeofday tz: recorded bytes contradict gettimeofday \
                 tv where the two outputs overlap"
            );
            assert_eq!(memory.reads.get(), 0);
            assert_eq!(memory.writes, 0);
            assert_eq!(target, [0; 16]);
        }
    }
}
