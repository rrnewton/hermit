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

use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;
use std::sync::OnceLock;

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Madvise;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Mmap;
use reverie::syscalls::Mprotect;
use reverie::syscalls::Mremap;
use reverie::syscalls::ProtFlags;

use super::Replayer;
use crate::event::MadviseRefill;
use crate::event::SharedMapping;

const PAGE_SIZE: usize = 4096;

/// A replay mapping of a shared file, which later mappings of the file in the
/// same process alias.
#[derive(Debug, Clone, Copy)]
struct SharedReplayMapping {
    address: usize,
    /// Page-rounded length.
    length: usize,
    /// Offset in the file.
    offset: u64,
    /// Inode of the anonymous shared memory backing it, which tells whether
    /// the mapping is still in place.
    memory: u64,
}

/// A shared file as one replayed process maps it: the process, and the
/// file's identity in the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SharedFile {
    pid: i32,
    device: u64,
    inode: u64,
}

impl SharedFile {
    fn new(pid: i32, shared: SharedMapping) -> Self {
        Self {
            pid,
            device: shared.device,
            inode: shared.inode,
        }
    }
}

type SharedReplayMappings = HashMap<SharedFile, Vec<SharedReplayMapping>>;

/// Replay mappings of each shared file.
fn shared_replay_mappings() -> &'static Mutex<SharedReplayMappings> {
    static MAPPINGS: OnceLock<Mutex<SharedReplayMappings>> = OnceLock::new();
    MAPPINGS.get_or_init(Default::default)
}

fn remember_shared_mapping(
    pid: i32,
    shared: SharedMapping,
    address: usize,
    len: usize,
    memory: u64,
) {
    shared_replay_mappings()
        .lock()
        .expect("should not be poisoned: no code panics while holding it")
        .entry(SharedFile::new(pid, shared))
        .or_default()
        .push(SharedReplayMapping {
            address,
            length: len.next_multiple_of(PAGE_SIZE),
            offset: shared.offset,
            memory,
        });
}

/// An address in a replay mapping of `shared`'s file in process `pid`, still
/// in place, that covers `len` bytes from `shared.offset`, with the inode of
/// its shared memory. The latest such mapping wins.
fn shared_source(pid: i32, shared: SharedMapping, len: usize) -> Option<(usize, u64)> {
    let mappings = shared_replay_mappings()
        .lock()
        .expect("should not be poisoned: no code panics while holding it")
        .get(&SharedFile::new(pid, shared))
        .cloned()
        .unwrap_or_default();
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    mappings.iter().rev().find_map(|mapping| {
        let delta = usize::try_from(shared.offset.checked_sub(mapping.offset)?).ok()?;
        if delta % PAGE_SIZE != 0
            || delta.checked_add(len)? > mapping.length
            || shared_memory_at(&maps, mapping.address) != Some(mapping.memory)
        {
            return None;
        }
        Some((mapping.address + delta, mapping.memory))
    })
}

/// The inode of the shared memory mapped at `address` in a `/proc/<pid>/maps`
/// listing, if a shared mapping starts there.
fn shared_memory_at(maps: &str, address: usize) -> Option<u64> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (start, _) = fields.next()?.split_once('-')?;
        let perms = fields.next()?;
        let _offset = fields.next()?;
        let _device = fields.next()?;
        let inode = fields.next()?;
        if usize::from_str_radix(start, 16).ok()? != address || !perms.ends_with('s') {
            return None;
        }
        inode.parse().ok()
    })
}

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

        if let Some(shared) = event.shared
            && let Some(ptr) = self
                .alias_shared_mapping(guest, event.addr, shared, len, prot)
                .await?
        {
            return Ok(ptr);
        }

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

        if let Some(shared) = event.shared
            && flags.contains(MapFlags::MAP_SHARED)
        {
            let pid = guest.pid().as_raw();
            let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
            if let Some(memory) = shared_memory_at(&maps, event.addr) {
                remember_shared_mapping(pid, shared, event.addr, len, memory);
            }
        }

        Ok(ptr)
    }
    /// Replays a `MAP_SHARED` mapping of a file this process already mapped
    /// as an alias of that earlier mapping, so that writes through either
    /// appear in both, as they did in the recording. Replay maps a file as
    /// anonymous memory, so separate mappings would otherwise be separate
    /// copies: one thread's writes to a memfd shared with another through two
    /// mappings never reached the other. Returns `None`, and maps nothing, when
    /// no earlier mapping in place covers the range; mappings made by different
    /// processes stay separate copies.
    async fn alias_shared_mapping<G: Guest<Self>>(
        &self,
        guest: &mut G,
        address: usize,
        shared: SharedMapping,
        len: usize,
        prot: ProtFlags,
    ) -> Result<Option<i64>, Errno> {
        let pid = guest.pid().as_raw();
        let Some((source, memory)) = shared_source(pid, shared, len) else {
            return Ok(None);
        };
        // Take the recorded address the way an unaliased replay mapping does,
        // as a hint that must be honored, so a diverged address space fails
        // here rather than having a mapping replaced under it.
        let placeholder = guest
            .inject_with_retry(
                Mmap::new()
                    .with_addr(AddrMut::<libc::c_void>::from_raw(address).map(Into::into))
                    .with_len(len)
                    .with_prot(ProtFlags::PROT_NONE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            )
            .await?;
        assert_eq!(
            placeholder as usize, address,
            "Failed to inject mmap at desired address"
        );
        // A zero old length asks Linux for a second mapping of the same shared
        // pages, which replaces the placeholder. The new mapping takes the
        // source's protection, so it is set below.
        let ptr = guest
            .inject_with_retry(
                Mremap::new()
                    .with_addr(AddrMut::from_raw(source))
                    .with_old_len(0)
                    .with_new_len(len)
                    .with_flags((libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED) as usize)
                    .with_new_addr(AddrMut::from_raw(address)),
            )
            .await?;
        assert_eq!(
            ptr as usize, address,
            "Failed to alias a shared mapping at the recorded address"
        );
        guest
            .inject_with_retry(
                Mprotect::new()
                    .with_addr(AddrMut::from_raw(address).map(|addr: AddrMut<u8>| addr.cast()))
                    .with_len(len)
                    .with_protection(prot),
            )
            .await?;
        remember_shared_mapping(pid, shared, address, len, memory);
        Ok(Some(ptr))
    }

    /// Replays a guest-semantic `madvise` recorded by the recorder's
    /// `handle_madvise`: applies the advice live to the recorded prefix, then
    /// restores the file-backed contents the recording observed.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3589): Audit the madvise refill and WIPEONFORK prefix.
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
            // Some recorded outcomes depend on the mapping type, which replay's
            // anonymous stand-ins do not preserve: MADV_GUARD_INSTALL on a
            // private file mapping fails with EINVAL before Linux 6.15, and
            // advice on device or hugetlbfs mappings can fail where anonymous
            // memory accepts it. Those fail the assertion below loudly.
            let result = guest.inject_with_retry(syscall).await;
            assert_eq!(
                result,
                event.result,
                "Replayed madvise({:?}, {}, {}) diverged from the recording; replay \
                 cannot reproduce this outcome",
                syscall.addr(),
                len,
                syscall.advice()
            );
        } else if event.live_len > 0 {
            // The recording failed at a file mapping after this prefix, so the
            // recorded error is the result. Before reaching it, Linux may have
            // passed holes (ENOMEM) or a shared mapping (EINVAL); anything else
            // means the prefix was not advised as it was when recording.
            let result = guest
                .inject_with_retry(syscall.with_len(event.live_len))
                .await;
            assert!(
                matches!(result, Ok(0) | Err(Errno::ENOMEM) | Err(Errno::EINVAL)),
                "Replayed madvise prefix ({:?}, {}, {}) returned {:?}; replay cannot \
                 reproduce the recorded {:?}",
                syscall.addr(),
                event.live_len,
                syscall.advice(),
                result,
                event.result
            );
        }

        if !event.refills.is_empty() {
            let mem_path = format!("/proc/{}/mem", guest.tid().as_raw());
            let mem = std::fs::File::open(&mem_path)
                .unwrap_or_else(|error| panic!("Cannot replay madvise: {mem_path}: {error}"));
            for refill in &event.refills {
                // Only write pages that differ: the range may include mappings
                // replay maps from the real file, such as executables, where a
                // write would create private copies the recording did not have.
                let mut current = vec![0u8; refill.bytes.len()];
                let readable = read_prefix(&mem, refill.addr, &mut current);
                for (offset, run) in differing_pages(&current[..readable], &refill.bytes) {
                    self.write_refill(guest, refill, offset, run).await;
                }
            }
        }

        event.result
    }
}

impl Replayer {
    /// Writes `refill.bytes[offset..offset + len]`, lifting write protection
    /// for the duration of the write.
    ///
    /// The advice has already been applied, so a failure here panics rather
    /// than return an error the recording never saw.
    async fn write_refill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        refill: &MadviseRefill,
        offset: usize,
        len: usize,
    ) {
        let start = refill.addr + offset;
        let prot = ProtFlags::from_bits_truncate(refill.prot);
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(protection(start, len, prot | ProtFlags::PROT_WRITE))
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot unprotect madvise refill at {start:#x}: {err}")
                });
        }
        // This is safe since the recorder only records mapped addresses.
        let addr = unsafe { AddrMut::<u8>::from_raw_unchecked(start) };
        guest
            .memory()
            .write_exact(addr, &refill.bytes[offset..offset + len])
            .unwrap();
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(protection(start, len, prot))
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot reprotect madvise refill at {start:#x}: {err}")
                });
        }
    }
}

fn protection(start: usize, len: usize, protection: ProtFlags) -> Mprotect {
    // This is safe since the recorder only records mapped addresses.
    let addr = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(start) };
    Mprotect::new()
        .with_addr(Some(addr))
        .with_len(len)
        .with_protection(protection)
}

/// Reads guest memory at `addr` through `/proc/<tid>/mem`, which also reads
/// mappings without `PROT_READ`, and returns how many leading bytes it read.
fn read_prefix(mem: &std::fs::File, addr: usize, buf: &mut [u8]) -> usize {
    let mut done = 0;
    while done < buf.len() {
        match mem.read_at(&mut buf[done..], (addr + done) as u64) {
            Ok(n) if n > 0 => done += n,
            _ => break,
        }
    }
    done
}

/// Page-aligned `(offset, len)` runs where `recorded` differs from `current`.
/// Bytes past the end of `current` (unreadable in replay) always differ.
fn differing_pages(current: &[u8], recorded: &[u8]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for offset in (0..recorded.len()).step_by(PAGE_SIZE) {
        let end = (offset + PAGE_SIZE).min(recorded.len());
        if current.get(offset..end) == Some(&recorded[offset..end]) {
            continue;
        }
        match runs.last_mut() {
            Some((start, len)) if *start + *len == offset => *len = end - *start,
            _ => runs.push((offset, end - offset)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differing_pages_merges_adjacent_changed_pages() {
        let recorded = vec![1u8; 4 * PAGE_SIZE];
        let mut current = recorded.clone();
        current[PAGE_SIZE + 7] = 0;
        current[2 * PAGE_SIZE] = 0;
        assert_eq!(
            differing_pages(&current, &recorded),
            vec![(PAGE_SIZE, 2 * PAGE_SIZE)]
        );
        assert!(differing_pages(&recorded, &recorded).is_empty());
    }

    #[test]
    fn differing_pages_treats_unread_bytes_as_changed() {
        let recorded = vec![1u8; 3 * PAGE_SIZE + 10];
        let current = recorded[..PAGE_SIZE].to_vec();
        assert_eq!(
            differing_pages(&current, &recorded),
            vec![(PAGE_SIZE, 2 * PAGE_SIZE + 10)]
        );
    }
}
