/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Reconstruct each message's input before Linux writes its `msg_len` output.
//! Earlier messages' outputs can change later messages' metadata or payload.
//! Restoring every output at once would therefore describe the wrong send.

use std::io::IoSlice;
use std::io::IoSliceMut;

use reverie::syscalls::Addr;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sendmmsg;

use super::BufferExtent;
use super::completed_mmsghdr_count;
use super::iovec_extents;

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    base: usize,
    captured: Vec<u8>,
}

impl Snapshot {
    pub(crate) fn capture<M: MemoryAccess>(memory: &M, call: Sendmmsg) -> Self {
        let base = call.msgvec().map_or(0, |address| address.as_raw());
        let count = call.vlen().min(libc::UIO_MAXIOV as u32) as usize;
        let mut captured = vec![0; count * std::mem::size_of::<libc::mmsghdr>()];
        // A single bounded read preserves a readable prefix even when unused
        // later headers are inaccessible. Capture never changes the syscall's
        // validation order or returns a speculative error to the guest.
        let read = Addr::<u8>::from_raw(base)
            .and_then(|address| {
                base.checked_add(captured.len())?;
                memory.read(address, &mut captured).ok()
            })
            .unwrap_or(0);
        captured.truncate(read);
        Self { base, captured }
    }

    fn entry_address(&self, index: usize) -> Result<usize, Errno> {
        index
            .checked_mul(std::mem::size_of::<libc::mmsghdr>())
            .and_then(|offset| self.base.checked_add(offset))
            .ok_or(Errno::EFAULT)
    }

    fn output_offset(index: usize) -> Result<usize, Errno> {
        index
            .checked_mul(std::mem::size_of::<libc::mmsghdr>())
            .and_then(|offset| offset.checked_add(std::mem::offset_of!(libc::mmsghdr, msg_len)))
            .ok_or(Errno::EFAULT)
    }

    fn output_address(&self, index: usize) -> Result<usize, Errno> {
        self.base
            .checked_add(Self::output_offset(index)?)
            .ok_or(Errno::EFAULT)
    }
}

/// A read-only view of memory at the point just before one message was sent.
/// Linux has already updated all completed output fields in actual memory;
/// restore only this message's and later completed messages' entry bytes.
pub(super) struct MessageMemory<'a, M> {
    memory: &'a M,
    snapshot: &'a Snapshot,
    index: usize,
    completed: usize,
}

impl<M: MemoryAccess> MessageMemory<'_, M> {
    fn restore(&self, start: usize, bytes: &mut [u8]) -> Result<(), Errno> {
        let end = start.checked_add(bytes.len()).ok_or(Errno::EFAULT)?;
        for index in self.index..self.completed {
            let output = self.snapshot.output_address(index)?;
            let lo = start.max(output);
            let hi = end.min(output.checked_add(4).ok_or(Errno::EFAULT)?);
            if lo < hi {
                let offset = Snapshot::output_offset(index)? + (lo - output);
                let original = self
                    .snapshot
                    .captured
                    .get(offset..offset + hi - lo)
                    .ok_or(Errno::EFAULT)?;
                bytes[lo - start..hi - start].copy_from_slice(original);
            }
        }
        Ok(())
    }
}

impl<M: MemoryAccess> MemoryAccess for MessageMemory<'_, M> {
    fn read_vectored(
        &self,
        read_from: &[IoSlice],
        write_to: &mut [IoSliceMut],
    ) -> Result<usize, Errno> {
        let read = self.memory.read_vectored(read_from, write_to)?;
        let mut remaining = read;
        let mut destination = 0;
        let mut destination_offset = 0;
        for source in read_from {
            let mut source_offset = 0;
            while source_offset < source.len() && remaining > 0 {
                while destination < write_to.len()
                    && destination_offset == write_to[destination].len()
                {
                    destination += 1;
                    destination_offset = 0;
                }
                let target = write_to.get_mut(destination).ok_or(Errno::EFAULT)?;
                let length = remaining
                    .min(source.len() - source_offset)
                    .min(target.len() - destination_offset);
                let start = (source.as_ptr() as usize)
                    .checked_add(source_offset)
                    .ok_or(Errno::EFAULT)?;
                self.restore(
                    start,
                    &mut target[destination_offset..destination_offset + length],
                )?;
                source_offset += length;
                destination_offset += length;
                remaining -= length;
            }
        }
        if remaining != 0 {
            return Err(Errno::EFAULT);
        }
        Ok(read)
    }

    fn write_vectored(
        &mut self,
        _read_from: &[IoSlice],
        _write_to: &mut [IoSliceMut],
    ) -> Result<usize, Errno> {
        // Observation must never write entry bytes back into guest memory.
        Err(Errno::EPERM)
    }
}

pub(super) fn message_memory<'a, M>(
    memory: &'a M,
    snapshot: &'a Snapshot,
    index: usize,
    completed: usize,
) -> MessageMemory<'a, M> {
    MessageMemory {
        memory,
        snapshot,
        index,
        completed,
    }
}

/// Return each completed message's exact extents with its message index, which
/// selects the same memory view when the payload is hashed afterward.
pub(super) fn extents<M: MemoryAccess>(
    memory: &M,
    call: Sendmmsg,
    ret: i64,
    snapshot: &Snapshot,
) -> Result<Vec<(BufferExtent, usize)>, reverie::Error> {
    let completed = completed_mmsghdr_count(call.vlen(), ret);
    let mut extents = Vec::new();
    if completed == 0 {
        return Ok(extents);
    }
    if call.msgvec().map_or(0, |address| address.as_raw()) != snapshot.base {
        return Err(Errno::EFAULT.into());
    }
    for index in 0..completed {
        // The returned length belongs to actual output memory, not the
        // reconstructed input view. Only metadata and payload use that view.
        let moved: u32 = memory.read_value(
            Addr::<u32>::from_raw(snapshot.output_address(index)?).ok_or(Errno::EFAULT)?,
        )?;
        let view = message_memory(memory, snapshot, index, completed);
        let header: libc::msghdr = view.read_value(
            Addr::<libc::msghdr>::from_raw(snapshot.entry_address(index)?).ok_or(Errno::EFAULT)?,
        )?;
        extents.extend(
            iovec_extents(
                &view,
                header.msg_iov as usize,
                header.msg_iovlen,
                i64::from(moved),
            )?
            .into_iter()
            .map(|extent| (extent, index)),
        );
    }
    Ok(extents)
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;

    use super::*;

    struct VectoredLocalMemory;

    impl MemoryAccess for VectoredLocalMemory {
        fn read_vectored(
            &self,
            read_from: &[IoSlice],
            write_to: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            let remote = read_from
                .iter()
                .map(|slice| libc::iovec {
                    iov_base: slice.as_ptr().cast_mut().cast(),
                    iov_len: slice.len(),
                })
                .collect::<Vec<_>>();
            let local = write_to
                .iter_mut()
                .map(|slice| libc::iovec {
                    iov_base: slice.as_mut_ptr().cast(),
                    iov_len: slice.len(),
                })
                .collect::<Vec<_>>();
            // The fixture uses readable allocations in this process. The
            // kernel fills both destination slices, whereas LocalMemory's
            // read_vectored deliberately stops at the first nonempty slice.
            let read = unsafe {
                libc::process_vm_readv(
                    libc::getpid(),
                    local.as_ptr(),
                    local.len() as libc::c_ulong,
                    remote.as_ptr(),
                    remote.len() as libc::c_ulong,
                    0,
                )
            };
            usize::try_from(read).map_err(|_| Errno::EFAULT)
        }

        fn write_vectored(
            &mut self,
            _read_from: &[IoSlice],
            _write_to: &mut [IoSliceMut],
        ) -> Result<usize, Errno> {
            Err(Errno::EPERM)
        }
    }

    #[test]
    fn each_message_sees_only_earlier_output_writes_in_its_payload() {
        let mut headers: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        headers[0].msg_len = 0x41414141;
        let output = (&raw mut headers[0].msg_len) as usize;
        let iov = libc::iovec {
            iov_base: output as *mut libc::c_void,
            iov_len: 4,
        };
        for header in &mut headers {
            header.msg_hdr.msg_iov = (&raw const iov).cast_mut();
            header.msg_hdr.msg_iovlen = 1;
        }
        let call = Sendmmsg::new()
            .with_msgvec(Addr::from_ptr(headers.as_ptr().cast::<libc::msghdr>()))
            .with_vlen(2);
        let memory = reverie::syscalls::LocalMemory::new();
        let snapshot = Snapshot::capture(&memory, call);
        headers[0].msg_len = 4;
        headers[1].msg_len = 4;
        let observed = extents(&memory, call, 2, &snapshot).unwrap();
        assert_eq!(
            observed,
            vec![
                (
                    BufferExtent {
                        addr: output as u64,
                        len: 4
                    },
                    0
                ),
                (
                    BufferExtent {
                        addr: output as u64,
                        len: 4
                    },
                    1
                )
            ]
        );
        let first = message_memory(&memory, &snapshot, 0, 2);
        let second = message_memory(&memory, &snapshot, 1, 2);
        let (first_digest, _, _) = super::super::extent_digests(&first, output as u64, 4).unwrap();
        let (second_digest, _, _) =
            super::super::extent_digests(&second, output as u64, 4).unwrap();
        assert_eq!(first_digest, crate::Digest::new(b"AAAA"));
        assert_eq!(second_digest, crate::Digest::new(&4u32.to_ne_bytes()));
        assert_eq!(headers[0].msg_len, 4);
        assert_eq!(headers[1].msg_len, 4);

        // Different destination slice boundaries must not change the overlay.
        let source = unsafe { std::slice::from_raw_parts(output as *const u8, 4) };
        let mut one = [0; 1];
        let mut three = [0; 3];
        let vectored_memory = VectoredLocalMemory;
        let first = message_memory(&vectored_memory, &snapshot, 0, 2);
        assert_eq!(
            first
                .read_vectored(
                    &[IoSlice::new(source)],
                    &mut [IoSliceMut::new(&mut one), IoSliceMut::new(&mut three)]
                )
                .unwrap(),
            4
        );
        assert_eq!(one, *b"A");
        assert_eq!(three, *b"AAA");
    }

    #[test]
    fn a_later_iovec_import_observes_an_earlier_message_length() {
        let mut headers: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
        let first_iov = libc::iovec {
            iov_base: 0x1000usize as *mut libc::c_void,
            iov_len: 12,
        };
        let second_iov = unsafe {
            (headers.as_mut_ptr() as *mut u8)
                .add(
                    std::mem::offset_of!(libc::mmsghdr, msg_len)
                        - std::mem::offset_of!(libc::iovec, iov_len),
                )
                .cast::<libc::iovec>()
        };
        // This deliberately overlays msg_flags/padding and msg_len/padding.
        // Raw writes avoid creating overlapping Rust references.
        unsafe {
            second_iov.write_unaligned(libc::iovec {
                iov_base: 0x2000usize as *mut libc::c_void,
                iov_len: 4,
            });
        }
        headers[0].msg_hdr.msg_iov = (&raw const first_iov).cast_mut();
        headers[0].msg_hdr.msg_iovlen = 1;
        headers[1].msg_hdr.msg_iov = second_iov;
        headers[1].msg_hdr.msg_iovlen = 1;
        let call = Sendmmsg::new()
            .with_msgvec(Addr::from_ptr(headers.as_ptr().cast::<libc::msghdr>()))
            .with_vlen(2);
        let memory = LocalMemory::new();
        let snapshot = Snapshot::capture(&memory, call);
        headers[0].msg_len = 12;
        headers[1].msg_len = 12;
        assert_eq!(
            extents(&memory, call, 2, &snapshot).unwrap(),
            vec![
                (
                    BufferExtent {
                        addr: 0x1000,
                        len: 12
                    },
                    0
                ),
                (
                    BufferExtent {
                        addr: 0x2000,
                        len: 12
                    },
                    1
                )
            ]
        );
        assert_eq!(headers[0].msg_len, 12);
        assert_eq!(headers[1].msg_len, 12);
    }
}
