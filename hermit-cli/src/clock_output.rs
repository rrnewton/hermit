/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;

use reverie::Errno;
use reverie::Error;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::AddrSliceMut;
use reverie::syscalls::MemoryAccess;

use crate::event::ClockOutput;

fn failure(message: &str) -> Error {
    Error::Tool(anyhow::anyhow!("captured clock output: {message}"))
}

// TODO-HUMAN-REVIEW(PR-3212): Review captured clock copyout and errno fidelity.
// https://github.com/rrnewton/hermit/pull/3212
pub(crate) fn capture<M: MemoryAccess, T: Copy>(
    memory: &M,
    address: Option<AddrMut<'_, T>>,
    result: Result<i64, Errno>,
) -> Result<ClockOutput, Error> {
    let size = std::mem::size_of::<T>();
    assert!(size <= 16);
    let mut output = ClockOutput {
        pointer_present: address.is_some(),
        bytes: Vec::new(),
    };
    let Some(address) = address else {
        return Ok(output);
    };
    if !matches!(result, Ok(_) | Err(Errno::EFAULT)) {
        return Ok(output);
    }

    // Ptrace's small read uses PEEKDATA, which can capture write-only output.
    // Align each word so that a short output prefix next to an unmapped page
    // never makes PEEKDATA cross that page. Only output bytes enter the event.
    while output.bytes.len() < size {
        let raw = address
            .as_raw()
            .checked_add(output.bytes.len())
            .ok_or_else(|| failure("output address overflow"))?;
        let aligned = raw & !7;
        let word = Addr::<u64>::from_raw(aligned)
            .ok_or(Errno::EFAULT)
            .and_then(|address| memory.read_value(address));
        let word = match word {
            Ok(word) => word.to_ne_bytes(),
            Err(_) if result == Err(Errno::EFAULT) => break,
            Err(_) => return Err(failure("cannot read successful syscall output")),
        };
        let start = raw - aligned;
        let count = (8 - start).min(size - output.bytes.len());
        output.bytes.extend_from_slice(&word[start..start + count]);
    }
    Ok(output)
}

pub(crate) fn replay<M: MemoryAccess, T: Copy>(
    memory: &mut M,
    address: Option<AddrMut<'_, T>>,
    result: Result<i64, Errno>,
    output: ClockOutput,
) -> Result<(), Error> {
    let size = std::mem::size_of::<T>();
    assert!(size <= 16);
    if output.pointer_present != address.is_some()
        || output.bytes.len() > size
        || (address.is_none() && !output.bytes.is_empty())
        || (result.is_ok() && address.is_some() && output.bytes.len() != size)
        || (!matches!(result, Ok(_) | Err(Errno::EFAULT)) && !output.bytes.is_empty())
    {
        return Err(failure("recorded pointer shape or output length diverged"));
    }
    if let Some(address) = address {
        let before = capture(memory, Some(address), result)?;
        if before.bytes.len() != output.bytes.len() {
            return Err(failure("output mapping diverged"));
        }
        // Never use MemoryAccess::write here: ptrace's eight-byte POKEDATA
        // optimization bypasses user protections. One-byte remote iovecs both
        // respect those protections and restore every writable prefix, even
        // for an unaligned output ending partway through a word.
        for (offset, byte) in output.bytes.iter().enumerate() {
            // The snapshot also includes unchanged bytes after a failed
            // copyout. Do not write those, especially on protected pages.
            if result == Err(Errno::EFAULT) && before.bytes[offset] == *byte {
                continue;
            }
            let raw = address
                .as_raw()
                .checked_add(offset)
                .ok_or_else(|| failure("output address overflow"))?;
            let addr = AddrMut::<u8>::from_raw(raw).ok_or_else(|| failure("null output"))?;
            let mut remote = unsafe { AddrSliceMut::from_raw_parts(addr, 1) };
            let write = memory.write_vectored(
                &[io::IoSlice::new(std::slice::from_ref(byte))],
                &mut [unsafe { remote.as_ioslice_mut() }],
            );
            match write {
                Ok(1) => {}
                Ok(0) | Err(Errno::EFAULT) if result == Err(Errno::EFAULT) => break,
                _ => return Err(failure("cannot restore syscall output")),
            }
        }
        // EFAULT is not permission to lose an earlier write. Verify every
        // captured byte, including an unchanged protected suffix, before
        // returning the recorded error. This also detects changed mappings.
        if capture(memory, Some(address), result)?.bytes != output.bytes {
            return Err(failure("restored bytes differ from recording"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::LocalMemory;

    use super::*;

    struct DenyWrites {
        memory: LocalMemory,
        writes: usize,
        write_result: Result<usize, Errno>,
    }

    impl MemoryAccess for DenyWrites {
        fn read_vectored(
            &self,
            from: &[io::IoSlice],
            to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.memory.read_vectored(from, to)
        }

        fn write_vectored(
            &mut self,
            _from: &[io::IoSlice],
            _to: &mut [io::IoSliceMut],
        ) -> Result<usize, Errno> {
            self.writes += 1;
            self.write_result
        }
    }

    #[test]
    fn replay_restores_partial_output_before_recorded_efault() {
        let mut target = [0x5a_u64; 2];
        let address = AddrMut::from_raw(target.as_mut_ptr() as usize).unwrap();
        let mut output =
            capture::<_, [u64; 2]>(&LocalMemory::new(), Some(address), Err(Errno::EFAULT)).unwrap();
        output.bytes[..4].copy_from_slice(&12345678_u32.to_ne_bytes());
        let expected = output.bytes.clone();
        replay(
            &mut LocalMemory::new(),
            Some(address),
            Err(Errno::EFAULT),
            output,
        )
        .unwrap();
        assert_eq!(
            capture(&LocalMemory::new(), Some(address), Ok(0))
                .unwrap()
                .bytes,
            expected
        );
    }

    #[test]
    fn recorded_efault_does_not_hide_failure_to_restore_changed_bytes() {
        for write_result in [Err(Errno::EFAULT), Ok(0)] {
            let mut target = 7_u64;
            let address = AddrMut::<u64>::from_raw(&mut target as *mut u64 as usize).unwrap();
            let mut memory = DenyWrites {
                memory: LocalMemory::new(),
                writes: 0,
                write_result,
            };
            let error = replay(
                &mut memory,
                Some(address),
                Err(Errno::EFAULT),
                ClockOutput {
                    pointer_present: true,
                    bytes: 11_u64.to_ne_bytes().to_vec(),
                },
            )
            .unwrap_err();
            assert!(matches!(error, Error::Tool(_)));
            let Error::Tool(error) = error else {
                unreachable!();
            };
            assert_eq!(
                error.to_string(),
                "captured clock output: restored bytes differ from recording"
            );
            assert_eq!(target, 7);
            assert_eq!(memory.writes, 1);
        }
    }

    #[test]
    fn unchanged_protected_error_output_is_not_written() {
        for write_result in [Err(Errno::EFAULT), Ok(0)] {
            let mut target = 7_u64;
            let address = AddrMut::<u64>::from_raw(&mut target as *mut u64 as usize).unwrap();
            let mut memory = DenyWrites {
                memory: LocalMemory::new(),
                writes: 0,
                write_result,
            };
            replay(
                &mut memory,
                Some(address),
                Err(Errno::EFAULT),
                ClockOutput {
                    pointer_present: true,
                    bytes: target.to_ne_bytes().to_vec(),
                },
            )
            .unwrap();
            assert_eq!(memory.writes, 0);
            let error = replay(
                &mut memory,
                Some(address),
                Ok(0),
                ClockOutput {
                    pointer_present: true,
                    bytes: target.to_ne_bytes().to_vec(),
                },
            )
            .unwrap_err();
            assert!(matches!(error, Error::Tool(_)));
            let Error::Tool(error) = error else {
                unreachable!();
            };
            assert_eq!(
                error.to_string(),
                "captured clock output: cannot restore syscall output"
            );
            assert_eq!(
                memory.writes, 1,
                "successful copyout must still require writable memory"
            );
        }
    }

    #[test]
    fn invalid_clock_id_does_not_read_or_write_the_destination() {
        let address = AddrMut::<u64>::from_raw(1);
        let output = capture(&LocalMemory::new(), address, Err(Errno::EINVAL)).unwrap();
        assert!(output.pointer_present);
        assert!(output.bytes.is_empty());
        replay(&mut LocalMemory::new(), address, Err(Errno::EINVAL), output).unwrap();
    }

    #[test]
    fn malformed_clock_output_refuses_before_copyout() {
        let mut target = 7_u64;
        let address = AddrMut::<u64>::from_raw(&mut target as *mut u64 as usize);
        for (result, pointer_present, bytes) in [
            (Ok(0), true, vec![0; 9]),
            (Ok(0), true, vec![0; 7]),
            (Ok(0), false, vec![0; 8]),
            (Err(Errno::EINVAL), true, vec![0; 8]),
        ] {
            let error = replay(
                &mut LocalMemory::new(),
                address,
                result,
                ClockOutput {
                    pointer_present,
                    bytes,
                },
            )
            .unwrap_err();
            assert!(matches!(error, Error::Tool(_)));
            assert_eq!(target, 7);
        }
    }
}
