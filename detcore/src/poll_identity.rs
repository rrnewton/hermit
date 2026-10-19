/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Study-branch experiment (`HERMIT_PARALLEL_POLL_IDENTITY=1`, off by
//! default): an `InternalIOPolling` request names what it waits on
//! (`Resources::poll_target`), so a resource model can tell two polling
//! retries on different objects apart. A request is a declaration only; the
//! scheduler does not act on the target, so this changes no schedule.
//!
//! The target comes from the syscall filing the request: a child wait names
//! the caller's process; a descriptor syscall names the open file description
//! of its descriptor; `poll`, `ppoll`, `select` and `pselect6` name every open
//! file description in their sets; `epoll_wait` names the epoll instance; a
//! futex wait names the address space and futex word. Identities are
//! Detcore's own (`OpenFileId`, `DetPid`, `MmId`), so they are the same in
//! every run. The syscall is the one `parallelism_probe::note_syscall`
//! recorded, which the local tool notes when this experiment or the profile
//! is on; like the profile, that is meaningful on the ptrace backend.

use std::sync::OnceLock;

use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::PollTarget;

/// The environment variable enabling poll identities.
pub const POLL_IDENTITY_ENV: &str = "HERMIT_PARALLEL_POLL_IDENTITY";

/// Whether poll identities are on.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(POLL_IDENTITY_ENV).is_some_and(|v| v == "1"))
}

/// Descriptor syscalls whose first argument is the descriptor they wait on.
const FD_FIRST: &[&str] = &[
    "read", "write", "readv", "writev", "pread64", "pwrite64", "preadv", "pwritev", "preadv2",
    "pwritev2", "recvfrom", "sendto", "recvmsg", "sendmsg", "recvmmsg", "sendmmsg", "accept",
    "accept4", "connect", "splice", "tee", "openat", "open",
];

/// Bound on the descriptors read from a poll or select set.
const MAX_FDS: usize = 1024;

/// The target of the polling request `guest`'s current syscall files, if it
/// can be named.
pub fn target<G, T>(guest: &G) -> Option<PollTarget>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettid = guest.thread_state().dettid;
    let (name, args) = crate::parallelism_probe::current_syscall(dettid)?;
    let open_file = |fd: i64| -> Option<detcore_model::fd::OpenFileId> {
        let fd = i32::try_from(fd).ok()?;
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.open_file_id())
            .ok()
    };
    let files = |fds: Vec<i64>| -> Option<PollTarget> {
        let mut ids: Vec<_> = fds.into_iter().filter_map(open_file).collect();
        ids.sort();
        ids.dedup();
        (!ids.is_empty()).then_some(PollTarget::OpenFiles(ids))
    };
    match name {
        "wait4" | "waitid" | "waitpid" => Some(PollTarget::Children(guest.thread_state().detpid?)),
        "epoll_wait" | "epoll_pwait" | "epoll_pwait2" => files(vec![args[0] as i64]),
        "futex" => Some(PollTarget::Futex(guest.thread_state().mm_id, args[0])),
        "poll" | "ppoll" => {
            let nfds = (args[1] as usize).min(MAX_FDS);
            let mut bytes = vec![0u8; nfds * 8];
            let base = Addr::<u8>::from_raw(args[0] as usize)?;
            guest.memory().read_exact(base, &mut bytes).ok()?;
            let fds = bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|entry| i32::from_ne_bytes([entry[0], entry[1], entry[2], entry[3]]) as i64)
                .filter(|fd| *fd >= 0)
                .collect();
            files(fds)
        }
        "select" | "pselect6" => {
            let nfds = (args[0] as usize).min(MAX_FDS);
            let mut fds = Vec::new();
            for set in [args[1], args[2]] {
                let Some(base) = Addr::<u8>::from_raw(set as usize) else {
                    continue;
                };
                let mut bits = vec![0u8; nfds.div_ceil(64) * 8];
                guest.memory().read_exact(base, &mut bits).ok()?;
                for fd in 0..nfds {
                    if bits[fd / 8] & (1 << (fd % 8)) != 0 {
                        fds.push(fd as i64);
                    }
                }
            }
            files(fds)
        }
        name if FD_FIRST.contains(&name) => files(vec![args[0] as i64]),
        _ => None,
    }
}
