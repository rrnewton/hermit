/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic policies for memory-management advice.

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;

use crate::Detcore;
use crate::RecordOrReplay;
use crate::procmaps;

const PAGE_SIZE: usize = 4096;
// Added in Linux 6.13 and not yet exposed by the pinned libc crate.
const MADV_GUARD_INSTALL: i32 = 102;
const MADV_GUARD_REMOVE: i32 = 103;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MadviseAction {
    Forward,
    Ignore,
    Reject(Errno),
    Unknown,
}

const fn madvise_action(advice: i32) -> MadviseAction {
    match advice {
        // Conventional advice and operations with guest-visible memory, fork,
        // backing-store, dump, or guard semantics must reach the kernel.
        libc::MADV_NORMAL
        | libc::MADV_RANDOM
        | libc::MADV_SEQUENTIAL
        | libc::MADV_WILLNEED
        | libc::MADV_DONTNEED
        | libc::MADV_REMOVE
        | libc::MADV_DONTFORK
        | libc::MADV_DOFORK
        | libc::MADV_DONTDUMP
        | libc::MADV_DODUMP
        | libc::MADV_WIPEONFORK
        | libc::MADV_KEEPONFORK
        | libc::MADV_DONTNEED_LOCKED
        | MADV_GUARD_INSTALL
        | MADV_GUARD_REMOVE => MadviseAction::Forward,

        // These are optional reclaim or asynchronous VM-policy controls. Their host
        // effects depend on memory pressure, KSM, and THP activity. Hermit accepts
        // them as fixed no-ops after common range validation; it deliberately does
        // not reproduce each advice's host- and mapping-specific EINVAL cases.
        libc::MADV_FREE
        | libc::MADV_MERGEABLE
        | libc::MADV_UNMERGEABLE
        | libc::MADV_HUGEPAGE
        | libc::MADV_NOHUGEPAGE
        | libc::MADV_COLD
        | libc::MADV_PAGEOUT => MadviseAction::Ignore,

        // Successful population and collapse promise synchronous, resource-
        // dependent work. Report the same deterministic error Linux uses when
        // an advice value is unsupported so callers can take their fallback.
        libc::MADV_POPULATE_READ | libc::MADV_POPULATE_WRITE | libc::MADV_COLLAPSE => {
            MadviseAction::Reject(Errno::EINVAL)
        }

        // Never let a guest inject host memory failures, even if the container
        // unexpectedly has enough privilege to make these operations succeed.
        libc::MADV_HWPOISON | libc::MADV_SOFT_OFFLINE => MadviseAction::Reject(Errno::EPERM),

        _ => MadviseAction::Unknown,
    }
}

fn validate_common_range<G, T>(guest: &G, call: syscalls::Madvise) -> Result<(), Error>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if call.len() == 0 {
        return Ok(());
    }

    let start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
    if !start.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL.into());
    }
    let end = start.checked_add(call.len()).ok_or(Errno::ENOMEM)?;
    let start = u64::try_from(start).map_err(|_| Errno::ENOMEM)?;
    let end = u64::try_from(end).map_err(|_| Errno::ENOMEM)?;
    let maps = procmaps::from_pid(guest.pid(), |map| {
        map.address.1 > start && map.address.0 < end
    })?;

    let mut covered_through = start;
    for map in maps {
        if map.address.0 > covered_through {
            return Err(Errno::ENOMEM.into());
        }
        covered_through = covered_through.max(map.address.1);
        if covered_through >= end {
            return Ok(());
        }
    }
    Err(Errno::ENOMEM.into())
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Apply a deterministic policy to `madvise(2)`.
    ///
    /// Advice with guest-visible semantics is record/replay-aware passthrough.
    /// Reclaim and asynchronous VM-policy advice receives fixed success after
    /// non-mutating common range validation, without exposing host memory pressure.
    /// Resource-dependent synchronous operations and hardware-failure injection are
    /// refused with fixed errors after the same validation. As on Linux, a zero-length
    /// call succeeds for every known advice value and probes whether it is recognized.
    // AUTONOMOUS-BOT-IMPLEMENTED
    pub async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Madvise,
    ) -> Result<i64, Error> {
        let advice = call.advice();
        match madvise_action(advice) {
            MadviseAction::Forward => Ok(self.record_or_replay(guest, call).await?),
            MadviseAction::Ignore => {
                validate_common_range(guest, call)?;
                crate::detlog!(
                    "[dtid {}] madvise advice {} accepted as deterministic no-op",
                    guest.thread_state().dettid,
                    advice,
                );
                Ok(0)
            }
            MadviseAction::Reject(errno) => {
                validate_common_range(guest, call)?;
                if call.len() == 0 {
                    return Ok(0);
                }
                crate::detlog!(
                    "[dtid {}] madvise advice {} rejected with {}",
                    guest.thread_state().dettid,
                    advice,
                    errno,
                );
                Err(errno.into())
            }
            MadviseAction::Unknown => {
                crate::detlog!(
                    "[dtid {}] unknown madvise advice {} rejected with EINVAL",
                    guest.thread_state().dettid,
                    advice,
                );
                Err(Errno::EINVAL.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn madvise_known_linux_advice_has_an_explicit_policy() {
        for advice in [
            libc::MADV_NORMAL,
            libc::MADV_RANDOM,
            libc::MADV_SEQUENTIAL,
            libc::MADV_WILLNEED,
            libc::MADV_DONTNEED,
            libc::MADV_REMOVE,
            libc::MADV_DONTFORK,
            libc::MADV_DOFORK,
            libc::MADV_DONTDUMP,
            libc::MADV_DODUMP,
            libc::MADV_WIPEONFORK,
            libc::MADV_KEEPONFORK,
            libc::MADV_DONTNEED_LOCKED,
            MADV_GUARD_INSTALL,
            MADV_GUARD_REMOVE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Forward);
        }

        for advice in [
            libc::MADV_FREE,
            libc::MADV_MERGEABLE,
            libc::MADV_UNMERGEABLE,
            libc::MADV_HUGEPAGE,
            libc::MADV_NOHUGEPAGE,
            libc::MADV_COLD,
            libc::MADV_PAGEOUT,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Ignore);
        }

        for advice in [
            libc::MADV_POPULATE_READ,
            libc::MADV_POPULATE_WRITE,
            libc::MADV_COLLAPSE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EINVAL));
        }
        for advice in [libc::MADV_HWPOISON, libc::MADV_SOFT_OFFLINE] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EPERM));
        }
        assert_eq!(madvise_action(i32::MAX), MadviseAction::Unknown);
    }
}
