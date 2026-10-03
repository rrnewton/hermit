/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::unix::fs::FileExt;

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Madvise;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Mmap;

use super::Recorder;
use crate::event::MadviseEvent;
use crate::event::MadviseRefill;
use crate::event::MmapEvent;
use crate::event::SyscallEvent;

const PAGE_SIZE: usize = 4096;
// Added in Linux 6.13 and not yet exposed by the pinned libc crate.
const MADV_GUARD_REMOVE: i32 = 103;

/// One line of `/proc/<pid>/maps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapping {
    start: usize,
    end: usize,
    /// `PROT_*` bits from the permission column.
    prot: i32,
    private: bool,
    /// Whether the mapping has a backing inode. Replay represents recorded
    /// file mappings as anonymous memory, so these are the ranges whose
    /// contents can differ after advice that drops pages.
    file_backed: bool,
}

fn parse_maps(text: &str) -> Vec<Mapping> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let (start, end) = fields.next()?.split_once('-')?;
            let perms = fields.next()?.as_bytes();
            let _offset = fields.next()?;
            let _device = fields.next()?;
            let inode = fields.next()?;
            if perms.len() < 4 {
                return None;
            }
            let mut prot = libc::PROT_NONE;
            for (flag, bit) in [
                (b'r', libc::PROT_READ),
                (b'w', libc::PROT_WRITE),
                (b'x', libc::PROT_EXEC),
            ] {
                if perms[..3].contains(&flag) {
                    prot |= bit;
                }
            }
            Some(Mapping {
                start: usize::from_str_radix(start, 16).ok()?,
                end: usize::from_str_radix(end, 16).ok()?,
                prot,
                private: perms[3] == b'p',
                file_backed: inode != "0",
            })
        })
        .collect()
}

/// Reads the guest's mappings. Recording cannot continue without them: an
/// error returned to the guest would leave no event for replay to consume.
fn read_mappings(path: &str) -> Vec<Mapping> {
    let maps = std::fs::read(path)
        .unwrap_or_else(|error| panic!("Cannot record madvise: {path}: {error}"));
    parse_maps(&String::from_utf8_lossy(&maps))
}

/// The parts of `[start, end)` covered by private file-backed mappings, each
/// with the protection of its mapping.
///
/// Only these read back differently in replay after their pages are dropped.
/// A shared file mapping keeps its contents in the page cache, and replay's
/// shared anonymous stand-in keeps them in shmem, so neither needs a refill.
fn private_file_ranges(mappings: &[Mapping], start: usize, end: usize) -> Vec<(usize, usize, i32)> {
    mappings
        .iter()
        .filter(|mapping| mapping.file_backed && mapping.private)
        .filter_map(|mapping| {
            let lo = mapping.start.max(start);
            let hi = mapping.end.min(end);
            (lo < hi).then_some((lo, hi, mapping.prot))
        })
        .collect()
}

/// Offset of the first private file-backed mapping in `[start, end)`.
///
/// Linux refuses `MADV_WIPEONFORK` with EINVAL at the first VMA that has a
/// file, after applying the advice to the VMAs before it. Replay's anonymous
/// replacement would accept the advice, so replay applies it only to this
/// prefix. Shared mappings stay shared in replay and fail there too.
fn wipeonfork_live_len(mappings: &[Mapping], start: usize, end: usize) -> Option<usize> {
    mappings
        .iter()
        .find(|mapping| {
            mapping.file_backed && mapping.private && mapping.start < end && start < mapping.end
        })
        .map(|mapping| mapping.start.max(start) - start)
}

/// Reads `[start, end)` of the guest through `/proc/<tid>/mem`, which also
/// reads mappings without `PROT_READ`. Pages that cannot be read (guard pages,
/// pages beyond the end of the file) are skipped; the readable runs are
/// returned with their addresses.
fn read_readable_runs(mem: &std::fs::File, start: usize, end: usize) -> Vec<(usize, Vec<u8>)> {
    let mut runs: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut pos = start;
    let mut run_open = false;
    while pos < end {
        let mut buf = vec![0u8; end - pos];
        match mem.read_at(&mut buf, pos as u64) {
            Ok(n) if n > 0 => {
                buf.truncate(n);
                if run_open {
                    runs.last_mut().unwrap().1.extend_from_slice(&buf);
                } else {
                    runs.push((pos, buf));
                    run_open = true;
                }
                pos += n;
            }
            _ => {
                run_open = false;
                pos = (pos & !(PAGE_SIZE - 1)) + PAGE_SIZE;
            }
        }
    }
    runs
}

impl Recorder {
    pub(super) async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Mmap,
    ) -> Result<i64, Errno> {
        // Let anonymous mappings through. This should already be deterministic
        // because ASLR is disabled.
        if syscall.flags().contains(MapFlags::MAP_ANONYMOUS) {
            return guest.inject(syscall).await;
        }

        let len = syscall.len();

        // Do the injection. We need to record the pointer the mapping is at and
        // all of the bytes contained in the mapping. This is very inefficient
        // for large mappings and should be replaced by just letting the mmap
        // through to the real file.
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            match result {
                Ok(addr) => {
                    let addr = Addr::from_raw(addr as usize).ok_or(Errno::EINVAL)?;

                    // NOTE: We can't use `read_exact` here to slurp up the
                    // memory map bytes. The memory size may be larger than the
                    // physical file size for dynamic libraries. The following
                    // `read` will read up to the physical file length, not the
                    // length specified on the memory map. The left over bytes
                    // that extend past the end of the physical file should be
                    // set to zeros when we replay this `mmap`.
                    let mut buf = vec![0u8; len];
                    let physical_length = guest.memory().read(addr, &mut buf)?;

                    // Don't store more bytes than we need to. When we create
                    // the anonymous map later, the extra bytes will be
                    // initialized to zero automatically.
                    buf.truncate(physical_length);

                    Ok(SyscallEvent::Mmap(MmapEvent {
                        addr: addr.as_raw(),
                        buf,
                    }))
                }
                Err(errno) => Err(errno),
            },
        );

        result
    }

    /// Records a guest-semantic `madvise`.
    ///
    /// The advice runs live in both recording and replay. Replay's anonymous
    /// stand-ins for file mappings differ in two guest-visible ways, which the
    /// event carries: pages dropped from a file mapping read back as file
    /// contents here but as zeros in replay, and `MADV_WIPEONFORK` fails at a
    /// private file mapping here but would succeed in replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3589): Audit the madvise refill and WIPEONFORK prefix.
    pub(super) async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Madvise,
    ) -> Result<i64, Errno> {
        let advice = syscall.advice();
        let start = syscall.addr().map(AddrMut::as_raw).unwrap_or(0);
        let len = syscall.len();
        // Detcore has already rejected unaligned or overflowing ranges.
        let end = start.saturating_add(len).next_multiple_of(PAGE_SIZE);

        let drops_pages = matches!(
            advice,
            libc::MADV_DONTNEED | libc::MADV_DONTNEED_LOCKED | MADV_GUARD_REMOVE
        );
        // The thread's own maps: the thread-group leader's is empty once it has
        // exited.
        let maps_path = format!("/proc/{}/maps", guest.tid().as_raw());

        let wipeonfork_prefix = if advice == libc::MADV_WIPEONFORK {
            let mappings = read_mappings(&maps_path);
            wipeonfork_live_len(&mappings, start, end)
        } else {
            None
        };

        let result = guest.inject(syscall).await;

        // Linux reports EINVAL at the first private file mapping and returns
        // without visiting the rest of the range. Replay reproduces that by
        // applying the advice only up to that mapping. Any other outcome
        // means Linux accepted or failed independently of the file mappings,
        // so replay applies the whole range and must reproduce the result.
        let live_len = match (wipeonfork_prefix, result) {
            (Some(prefix), Err(Errno::EINVAL)) => prefix,
            _ => len,
        };

        let mut refills = Vec::new();
        if drops_pages {
            let mappings = read_mappings(&maps_path);
            let ranges = private_file_ranges(&mappings, start, end);
            if !ranges.is_empty() {
                let mem_path = format!("/proc/{}/mem", guest.tid().as_raw());
                // The advice has already taken effect, so failing the guest
                // call now would leave the recording without its event.
                let mem = std::fs::File::open(&mem_path)
                    .unwrap_or_else(|error| panic!("Cannot record madvise: {mem_path}: {error}"));
                for (lo, hi, prot) in ranges {
                    for (addr, bytes) in read_readable_runs(&mem, lo, hi) {
                        refills.push(MadviseRefill { addr, bytes, prot });
                    }
                }
            }
        }

        self.record_event(
            guest,
            Ok(SyscallEvent::Madvise(MadviseEvent {
                result,
                live_len,
                refills,
            })),
        );

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
00400000-00401000 r-xp 00000000 08:01 1234 /usr/bin/guest
7f0000000000-7f0000002000 rw-p 00000000 00:00 0
7f0000002000-7f0000003000 rw-p 00001000 08:01 1234 /usr/bin/guest
7f0000003000-7f0000004000 ---s 00000000 00:05 77 /dev/zero (deleted)
7ffffffde000-7ffffffff000 rw-p 00000000 00:00 0 [stack]
";

    #[test]
    fn parses_maps_lines() {
        let mappings = parse_maps(MAPS);
        assert_eq!(mappings.len(), 5);
        assert_eq!(
            mappings[0],
            Mapping {
                start: 0x400000,
                end: 0x401000,
                prot: libc::PROT_READ | libc::PROT_EXEC,
                private: true,
                file_backed: true,
            }
        );
        assert!(!mappings[1].file_backed);
        assert_eq!(mappings[3].prot, libc::PROT_NONE);
        assert!(!mappings[3].private);
        assert!(mappings[3].file_backed);
    }

    #[test]
    fn private_file_ranges_clip_to_the_advised_range() {
        let mappings = parse_maps(MAPS);
        assert_eq!(
            private_file_ranges(&mappings, 0x7f0000001000, 0x7f0000004000),
            vec![(
                0x7f0000002000,
                0x7f0000003000,
                libc::PROT_READ | libc::PROT_WRITE
            )]
        );
        assert!(private_file_ranges(&mappings, 0x7f0000000000, 0x7f0000002000).is_empty());
    }

    #[test]
    fn private_file_ranges_skip_shared_mappings() {
        let mappings = parse_maps(MAPS);
        assert!(private_file_ranges(&mappings, 0x7f0000003000, 0x7f0000004000).is_empty());
    }

    #[test]
    fn wipeonfork_prefix_stops_at_the_first_private_file_mapping() {
        let mappings = parse_maps(MAPS);
        assert_eq!(
            wipeonfork_live_len(&mappings, 0x7f0000000000, 0x7f0000004000),
            Some(0x2000)
        );
        assert_eq!(
            wipeonfork_live_len(&mappings, 0x7f0000002000, 0x7f0000003000),
            Some(0)
        );
        // A shared mapping fails in replay too, so it needs no prefix.
        assert_eq!(
            wipeonfork_live_len(&mappings, 0x7f0000003000, 0x7f0000004000),
            None
        );
    }
}
