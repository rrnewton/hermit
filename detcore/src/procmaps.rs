/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub use procfs::process::MMapPath;
pub use procfs::process::MemoryMap;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;

use crate::Digest;

fn display_pathname(p: &MMapPath) -> String {
    match p {
        MMapPath::Vdso => String::from("[vsdo]"),
        MMapPath::Stack => String::from("[stack]"),
        MMapPath::TStack(tid) => format!("[tstack:{}]", tid),
        MMapPath::Vvar => String::from("[vvar]"),
        MMapPath::Vsyscall => String::from("[syscalls]"),
        MMapPath::Heap => String::from("[heap]"),
        MMapPath::Other(s) => format!("[other: {}]", s),
        MMapPath::Anonymous => String::from("[annonymous]"),
        MMapPath::Path(s) => s.display().to_string(),
        MMapPath::Rollup => String::from("[rollup]"),
        MMapPath::Vsys(vsys) => vsys.to_string(),
    }
}

pub fn display(map: &MemoryMap) -> String {
    format!(
        "{:#x}-{:#x} {:?} {:x} {:x}:{:x} {} {}",
        map.address.0,
        map.address.1,
        map.perms,
        map.offset,
        map.dev.0,
        map.dev.1,
        map.inode,
        display_pathname(&map.pathname)
    )
}

fn map_error(err: procfs::ProcError) -> reverie::Error {
    match err {
        procfs::ProcError::Io(err, _) => reverie::Error::Io(err),
        err => reverie::Error::Tool(anyhow::anyhow!(err)),
    }
}

pub fn from_pid<F>(pid: Pid, filter: F) -> Result<Vec<MemoryMap>, reverie::Error>
where
    F: Fn(&MemoryMap) -> bool,
{
    match procfs::process::Process::new(pid.as_raw()) {
        Ok(process) => match process.maps() {
            Ok(mut maps) => {
                maps.0.retain(filter);
                Ok(maps.0)
            }
            Err(err) => Err(map_error(err)),
        },
        Err(err) => Err(map_error(err)),
    }
}

pub fn compute_hash<G, T: Tool>(guest: &mut G, map: &MemoryMap) -> Result<Digest, reverie::Error>
where
    G: Guest<T>,
{
    compute_hash_range(guest, map.address.0, map.address.1)
}

/// Hash the guest bytes in the half-open guest-virtual range `[start, end)`,
/// read through `guest.memory()`. Used for backend-reported memory regions
/// ([`reverie::Guest::detlog_memory_regions`]) where the range is a guest
/// address rather than an entry parsed from a host `/proc/<pid>/maps`.
pub fn compute_hash_range<G, T: Tool>(
    guest: &mut G,
    start: u64,
    end: u64,
) -> Result<Digest, reverie::Error>
where
    G: Guest<T>,
{
    let size = end.saturating_sub(start) as usize;
    let memory = guest.memory();
    let mut buf = vec![0; size];
    let start_addr = start;
    let start = Addr::<u8>::from_raw(start as usize).unwrap();
    memory.read_values(start, buf.as_mut_slice())?;
    // TEMPORARY DIAGNOSTIC -- do not commit. Dumps the exact bytes that feed
    // the hash so two runs can be byte-diffed (task dbi_detlog_stack_hashes).
    if let Ok(dir) = std::env::var("HERMIT_DEBUG_DUMP_MEM") {
        use std::io::Write;
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = format!("{}/dump-{:03}-{:#x}.bin", dir, n, start_addr);
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(buf.as_slice());
        }
    }
    Ok(Digest::new(buf.as_slice()))
}
