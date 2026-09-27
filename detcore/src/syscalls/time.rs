/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with threads and concurrency.
use std::time::Duration;

use nix::sys::signal::Signal;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::ClockId;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::Timespec;
use reverie::syscalls::Timeval;
use reverie::syscalls::family::NanosleepFamily;
use tracing::error;
use tracing::info;
use tracing::trace;

use crate::detlog;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::Priority;
use crate::scheduler::entropy_to_priority;
use crate::tool_global::ResumeStatus;
use crate::tool_global::register_posix_timer;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;

fn time_from_resources(rsrcs: &Resources) -> Option<LogicalTime> {
    if rsrcs.resources.len() > 1 {
        panic!(
            "time_from_resources: multiple resource ids in resource request: {:?}",
            rsrcs
        );
    }
    for rs in rsrcs.resources.iter() {
        if let (ResourceID::SleepUntil(tm), _) = rs {
            return Some(*tm);
        }
    }
    None
}

/// Flatten a `timespec` to nanoseconds. Negative fields are not valid for the
/// timer syscalls we handle; treat them as zero rather than panicking.
fn timespec_to_ns(ts: libc::timespec) -> u64 {
    let secs = ts.tv_sec.max(0) as u64;
    let nsec = ts.tv_nsec.max(0) as u64;
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

/// Inverse of [`timespec_to_ns`].
fn ns_to_timespec(ns: u64) -> libc::timespec {
    libc::timespec {
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (ns % 1_000_000_000) as libc::c_long,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-857): Query-only timex mode boundary.
fn timex_mode_is_query(modes: libc::c_uint) -> bool {
    modes == 0 || modes == libc::ADJ_OFFSET_SS_READ
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-857): Host-independent NTP discipline snapshot.
fn deterministic_timex(now: Timespec) -> libc::timex {
    // SAFETY: `libc::timex` contains only integer fields and padding; zero is a
    // valid baseline for the fields not modeled by Hermit.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    tx.status = libc::STA_UNSYNC;
    tx.tick = 10_000;
    tx.time = libc::timeval {
        tv_sec: now.tv_sec,
        tv_usec: now.tv_nsec / 1_000,
    };
    tx
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-845): Review SaBRe thread-local guest clock reads.
pub(crate) async fn guest_clock_time<G, T>(guest: &mut G) -> LogicalTime
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let raw = thread_observe_time(guest).await;
    guest.thread_state().observe_guest_clock(raw)
}

/// Every page boundary is a multiple of 4 KiB, and an eight-byte word crosses
/// at most one such multiple, so this locates the only point where a
/// page-granular store of a `timeval` word can stop partway through it.
const TV_WORD_PAGE_GRANULE: usize = 4096;

/// Replacing host time in the `tv` of a `gettimeofday` that failed with EFAULT
/// could not finish. `tv` may still hold host wall-clock time or part of a
/// word, so this is a failed run, not a guest errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TvRepairFailure {
    /// The word being stored: `tv_sec` or `tv_usec`.
    field: &'static str,
    kind: TvRepairFailureKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TvRepairFailureKind {
    /// A user-access copy failed with an errno other than EFAULT: the read or
    /// unchanged rewrite that checks whether one page of a word crossing a
    /// page boundary is writable, or a write of the word or of one of its two
    /// parts.
    Failed {
        operation: &'static str,
        errno: Errno,
    },
    /// A user-access copy reported zero bytes without EFAULT, or more bytes
    /// than it was given. No supported backend does either.
    ImpossibleCount {
        operation: &'static str,
        requested: usize,
        reported: usize,
    },
    /// Some of the word's bytes were stored and then a byte faulted. This
    /// happens when `tv_sec` crosses from a write-only page into an
    /// inaccessible or unmapped one, and otherwise only if a page's
    /// writability changed during the repair.
    PartlyStored { stored: usize },
}

impl std::fmt::Display for TvRepairFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "replacing host time in the {} of a failed gettimeofday: ",
            self.field
        )?;
        match self.kind {
            TvRepairFailureKind::Failed { operation, errno } => {
                write!(f, "user-access {operation} failed: {errno}")
            }
            TvRepairFailureKind::ImpossibleCount {
                operation,
                requested,
                reported,
            } => write!(
                f,
                "user-access {operation} reported {reported} of {requested} bytes"
            ),
            TvRepairFailureKind::PartlyStored { stored } => write!(
                f,
                "stored {stored} of its 8 bytes before a fault, leaving it partly stored"
            ),
        }
    }
}

impl std::error::Error for TvRepairFailure {}

/// Writes `bytes` at `addr` with user access, continuing after a short count
/// (which does not by itself mean the next byte is unwritable), and returns how
/// many bytes were written before a byte faulted.
fn write_user_prefix<M: MemoryAccess>(
    memory: &mut M,
    addr: AddrMut<u8>,
    bytes: &[u8],
    operation: &'static str,
) -> Result<usize, TvRepairFailureKind> {
    let mut written = 0;
    while written < bytes.len() {
        let Some(next) = addr
            .as_raw()
            .checked_add(written)
            .and_then(AddrMut::<u8>::from_raw)
        else {
            break;
        };
        let requested = bytes.len() - written;
        match memory.write_with_user_access(next, &bytes[written..]) {
            Ok(copied) if (1..=requested).contains(&copied) => written += copied,
            Ok(reported) => {
                return Err(TvRepairFailureKind::ImpossibleCount {
                    operation,
                    requested,
                    reported,
                });
            }
            Err(Errno::EFAULT) => break,
            Err(errno) => return Err(TvRepairFailureKind::Failed { operation, errno }),
        }
    }
    Ok(written)
}

/// Names for the read and the unchanged rewrite that check whether a word
/// crossing a page boundary can be written on its first page.
const FIRST_PAGE_CHECK: [&str; 2] = ["first-page read", "first-page rewrite"];
/// Names for the same check of the part on the word's second page.
const SECOND_PAGE_CHECK: [&str; 2] = ["second-page read", "second-page rewrite"];

/// Rewrites the `len` bytes at `addr`, which lie on one page, with their own
/// contents, to learn whether they can be written without changing them.
/// Returns `None` if they cannot be read, and otherwise whether every byte was
/// written.
fn rewrite_in_place<M: MemoryAccess>(
    memory: &mut M,
    addr: AddrMut<u8>,
    len: usize,
    [read, rewrite]: [&'static str; 2],
) -> Result<Option<bool>, TvRepairFailureKind> {
    let mut current = [0; 8];
    let current = &mut current[..len];
    match memory.read_exact_with_user_access(addr, current) {
        Ok(()) => Ok(Some(
            write_user_prefix(memory, addr, current, rewrite)? == len,
        )),
        Err(Errno::EFAULT) => Ok(None),
        Err(errno) => Err(TvRepairFailureKind::Failed {
            operation: read,
            errno,
        }),
    }
}

/// Writes `bytes`, which are the whole word or its part on one page, after
/// `stored` of the word's other bytes were written, and returns whether all of
/// them were written. Writing none of them leaves the word as it was only if
/// none of its bytes were written before.
fn write_tv_bytes<M: MemoryAccess>(
    memory: &mut M,
    addr: AddrMut<u8>,
    bytes: &[u8],
    stored: usize,
    operation: &'static str,
) -> Result<bool, TvRepairFailureKind> {
    let written = write_user_prefix(memory, addr, bytes, operation)?;
    if written == bytes.len() {
        Ok(true)
    } else if written == 0 && stored == 0 {
        Ok(false)
    } else {
        Err(TvRepairFailureKind::PartlyStored {
            stored: stored + written,
        })
    }
}

/// Stores one eight-byte `timeval` word completely or not at all, as the
/// kernel's `put_user` does, and returns whether it was stored.
/// `writable_page` is the index of a 4 KiB granule already known to be
/// writable, because the previous word was stored and ended on it.
fn store_tv_word<M: MemoryAccess>(
    memory: &mut M,
    addr: AddrMut<u8>,
    word: [u8; 8],
    writable_page: Option<usize>,
) -> Result<bool, TvRepairFailureKind> {
    let first_page_len = word
        .len()
        .min(TV_WORD_PAGE_GRANULE - addr.as_raw() % TV_WORD_PAGE_GRANULE);
    let second_page = if first_page_len < word.len() {
        addr.as_raw()
            .checked_add(first_page_len)
            .and_then(AddrMut::<u8>::from_raw)
    } else {
        None
    };
    let Some(later) = second_page else {
        // A word on one page is written by one copy, which stores all of it
        // or nothing.
        return write_tv_bytes(memory, addr, &word, 0, "write");
    };
    let (first, second) = word.split_at(first_page_len);
    // A copy of a word crossing a page boundary stores the part on its first
    // page even when the second page is not writable, so the pages are checked
    // before the word is written. A part that can be read is checked by
    // rewriting it with its own bytes, which changes nothing.
    match rewrite_in_place(memory, later, second.len(), SECOND_PAGE_CHECK)? {
        Some(false) => return Ok(false),
        // This write stores nothing if the first page is not writable.
        Some(true) => return write_tv_bytes(memory, addr, &word, 0, "write"),
        // The second page is inaccessible, unmapped or write-only.
        None => {}
    }
    let first_page_writable = if writable_page == Some(addr.as_raw() / TV_WORD_PAGE_GRANULE) {
        Some(true)
    } else {
        rewrite_in_place(memory, addr, first.len(), FIRST_PAGE_CHECK)?
    };
    match first_page_writable {
        Some(false) => Ok(false),
        // The word is now stored exactly when its second-page part can be
        // written, so that part is written first.
        Some(true) => Ok(
            write_tv_bytes(memory, later, second, 0, "second-page write")?
                && write_tv_bytes(memory, addr, first, second.len(), "first-page write")?,
        ),
        // Neither part can be read, and nothing that does not write shows
        // whether either page is writable. Writing the word in order stores
        // nothing if the first page is not writable and all of it if both
        // are, but only its first part if only the first page is.
        None => write_tv_bytes(memory, addr, &word, 0, "write"),
    }
}

/// Replaces the host wall-clock time that a `gettimeofday` failing with EFAULT
/// may have stored in `tv` with virtual time, in exactly the words Linux
/// stored, wherever the backend's view of writability matches the kernel's,
/// except in one layout, described below, that returns a Tool error.
///
/// Linux stores `tv_sec`, then `tv_usec`, each with one eight-byte `put_user`,
/// and only then copies `tz`; the first fault ends the call with EFAULT. A
/// store that faults on either page it touches commits nothing, so each word
/// is stored whole or not at all, and nothing after an unstored word is
/// attempted. This repeats those stores with virtual time: a word keeps
/// virtual time only if all eight of its bytes can be written, and the first
/// word that cannot ends the repair.
///
/// Writability is what `MemoryAccess::write_with_user_access` reports. On
/// ptrace, SaBRe and DBT (whose guest memory is `LocalMemory`) that is one
/// `process_vm_writev`, which stops at the first page without write
/// permission. KVM stops at the first page its user-access tracker does not
/// mark writable; while tracking is disabled it checks only that the bytes lie
/// in guest memory, and installed ELF guests enable tracking before they run.
/// Either way, a copy of a word that crosses into an unwritable page stores
/// the part on its first page before the fault is seen, which the kernel's
/// store never does, and no user-access copy reports whether memory is
/// writable without writing it. So a word crossing a page boundary is stored
/// as follows:
///
/// - If its part on the second page can be read, that part is rewritten with
///   its own bytes. A rewrite that stops short means the word is left alone,
///   and a complete one means the whole word is then written, which stores
///   nothing if the first page is not writable.
/// - Otherwise, if its first page is known to be writable, the part on the
///   second page is written, and then, only if that succeeded, the part on
///   the first page. The first page is known to be writable if the previous
///   word was stored and ended on it, which always holds when `tv_usec` is
///   reached and crosses a page boundary, because `tv_sec` then lies wholly on
///   that page. It is also known to be writable if its part can be read and
///   rewritten with its own bytes; a rewrite there that stops short means the
///   word is left alone.
/// - Otherwise neither part can be read, and the whole word is written, which
///   stores nothing if the first page is not writable and all of the word if
///   both pages are. If only the first page is writable, as when `tv_sec`
///   crosses from a write-only page into an inaccessible or unmapped one, the
///   word's first part is left stored where Linux stores nothing, and a Tool
///   error is returned. Writing the second part first would fail the same way
///   for a write-only second page after an inaccessible first one.
///
/// A rewrite stores the bytes that were already there, so the contents
/// afterwards are what Linux leaves, but the page has been written: it is
/// dirty, a copy-on-write page has been copied, and a shared file page may be
/// written back and have its file's modification time updated. That also
/// happens to a page Linux did not write, when the word is then left alone
/// because its other page is not writable. A store to the rewritten bytes by
/// another process, or by another guest thread when threads are not
/// sequentialized, between their read and their rewrite is undone. Between
/// the two writes of a word written in two parts, it holds virtual time on
/// its second page and host time on its first. None of the copies are atomic
/// with respect to other guest threads, other processes using the same
/// memory, or mapping changes.
///
/// `write_with_user_access` adds no protection-key (PKRU) emulation, and the
/// `process_vm_writev` copies ignore protection keys, while the kernel's own
/// store honours them. A `tv` on a page whose protection key disables writes
/// therefore receives virtual time where Linux stored nothing.
///
/// EFAULT is an ordinary outcome of each copy. Any other error, an impossible
/// count, or a partly stored word is returned as a Tool error, because `tv`
/// may then hold host time or part of a word.
fn overwrite_failed_gettimeofday_tv<M: MemoryAccess>(
    memory: &mut M,
    tv_addr: AddrMut<Timeval>,
    tv: &Timeval,
) -> Result<(), Error> {
    let words = [
        (
            "tv_sec",
            std::mem::offset_of!(Timeval, tv_sec),
            tv.tv_sec.to_ne_bytes(),
        ),
        (
            "tv_usec",
            std::mem::offset_of!(Timeval, tv_usec),
            tv.tv_usec.to_ne_bytes(),
        ),
    ];
    // The granule holding the last byte of the word stored before, which has
    // just been written and so is known to be writable.
    let mut writable_page = None;
    for (field, offset, word) in words {
        let Some(addr) = tv_addr
            .as_raw()
            .checked_add(offset)
            .and_then(AddrMut::<u8>::from_raw)
        else {
            break;
        };
        match store_tv_word(memory, addr, word, writable_page) {
            Ok(true) => {
                writable_page = addr
                    .as_raw()
                    .checked_add(word.len() - 1)
                    .map(|last| last / TV_WORD_PAGE_GRANULE);
            }
            Ok(false) => break,
            Err(kind) => {
                return Err(Error::Tool(anyhow::Error::new(TvRepairFailure {
                    field,
                    kind,
                })));
            }
        }
    }
    Ok(())
}

fn remaining_sleep_duration(target: LogicalTime, now: LogicalTime) -> Duration {
    if target > now {
        target.duration_since(now)
    } else {
        Duration::ZERO
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Convenience function for constructing a sleep request with a nanosecond offset from "now".
    pub async fn sleep_request<G: Guest<Self>>(guest: &mut G, ns_delta: Duration) -> Resources {
        let base_time = thread_observe_time(guest).await;
        let target_time = base_time + ns_delta;
        let resource = ResourceID::SleepUntil(target_time);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Convenience function for constructing a sleep request with a absolute nanosecond value from the realtime clock.
    pub async fn sleep_request_abs<G: Guest<Self>>(guest: &mut G, time: LogicalTime) -> Resources {
        // TODO T124594597 Record-replay case requires better handling of time
        let resource = ResourceID::SleepUntil(time);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Convenience function for constructing a thread yield request.
    /// Implemented as a sleep ending at the epoch (in the past).
    pub fn yield_request<G: Guest<Self>>(guest: &mut G) -> Resources {
        let resource = ResourceID::SleepUntil(LogicalTime::from_nanos(0));
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// Construct a request for a strong, one-turn scheduler yield.
    pub fn sched_yield_request<G: Guest<Self>>(guest: &mut G) -> Resources {
        guest
            .thread_state()
            .mk_request(ResourceID::SchedYield, Permission::W)
    }

    /// Construct a random PriorityChangePoint request using the local PRNG.
    pub fn random_priority_changepoint_request<G: Guest<Self>>(
        guest: &mut G,
        change_time: LogicalTime,
    ) -> Resources {
        let entropy = guest.thread_state_mut().chaos_prng_next_u64("priority");
        let new_priority = entropy_to_priority(entropy);
        Self::priority_changepoint_request(guest, change_time, new_priority)
    }

    /// Construct a PriorityChangePoint request using the supplied time and priority.
    pub fn priority_changepoint_request<G: Guest<Self>>(
        guest: &mut G,
        change_time: LogicalTime,
        new_priority: Priority,
    ) -> Resources {
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1151)
        let epochs = guest.thread_state_mut().take_pending_chaos_epochs();
        let rcbs = guest.thread_state().committed_clock_value;
        let resource = ResourceID::PriorityChangePoint(new_priority, change_time, rcbs, epochs);
        guest.thread_state().mk_request(resource, Permission::W)
    }

    /// gettimeofday
    pub async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Gettimeofday,
    ) -> Result<i64, Error> {
        let time_ns = guest_clock_time(guest).await;

        // A call failing with EFAULT may still have stored host wall-clock time
        // in `tv`, so keep its result until `tv` holds virtual time.
        let result = self
            .record_or_replay_preserving_tool_errors(guest, call)
            .await;

        let mut memory = guest.memory();

        let tv: Timeval = time_ns.into();

        if let Some(tp) = call.tv() {
            match &result {
                Ok(_) => memory.write_value(tp, &tv)?,
                // Linux's gettimeofday fails only with EFAULT, which is taken
                // to be its own even when a seccomp filter returned it without
                // running the call. Any other error came from the backend, the
                // tool, a seccomp filter or a replayed log, and says nothing
                // about what reached `tv`, so memory is left alone.
                Err(Error::Errno(Errno::EFAULT)) => {
                    overwrite_failed_gettimeofday_tv(&mut memory, tp.into(), &tv)?
                }
                Err(_) => {}
            }
        }

        result
    }

    /// time
    pub async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Time,
    ) -> Result<i64, Error> {
        let time_ns = guest_clock_time(guest).await;
        let secs = time_ns.as_secs() as i64;

        if let Some(tloc) = call.tloc() {
            let mut memory = guest.memory();
            memory.write_value(tloc, &secs)?;
        }

        Ok(secs)
    }

    /// clock_gettime
    pub async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockGettime,
    ) -> Result<i64, Error> {
        let time_ns = guest_clock_time(guest).await;
        trace!("Converting nanoseconds into clock_gettime: {}", time_ns);

        let tp = call.tp().ok_or(Errno::EFAULT)?;

        let t: Timespec = time_ns.into();

        guest.memory().write_value(tp, &t)?;

        Ok(0)
    }

    /// clock_gettime
    pub async fn handle_clock_getres<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockGetres,
    ) -> Result<i64, Error> {
        // A NULL `res` pointer is valid for clock_getres: the kernel validates
        // the clockid and returns 0 without storing the resolution. GHC's RTS
        // probes the per-thread CPU clock exactly this way
        // (clock_getres(clockid, NULL)) in getCurrentThreadCPUTime, so
        // returning EFAULT here spuriously aborts the guest. Only write the
        // resolution when the caller supplied a destination.
        if let Some(res) = call.res() {
            // For now we report a constant clock res of 10ms:
            let clock_res = 10;

            let t = Timespec {
                tv_sec: 0,
                tv_nsec: 1000 * clock_res as i64,
            };

            guest.memory().write_value(res, &t)?;
        }

        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-857): Deterministic adjtimex query/refusal policy.
    /// Report Hermit's virtual clock with a fixed unsynchronized discipline.
    /// Adjustment modes are capability-gated host mutations and receive EPERM.
    pub async fn handle_adjtimex<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Adjtimex,
    ) -> Result<i64, Error> {
        self.write_deterministic_timex(guest, call.buf()).await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-857): Deterministic clock_adjtime query/refusal policy.
    /// Apply the adjtimex policy to CLOCK_REALTIME. Linux does not permit NTP
    /// adjustment of the other fixed clock IDs, so reject them with EOPNOTSUPP.
    pub async fn handle_clock_adjtime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ClockAdjtime,
    ) -> Result<i64, Error> {
        if call.clockid() != ClockId::CLOCK_REALTIME {
            return Err(Errno::EOPNOTSUPP.into());
        }
        self.write_deterministic_timex(guest, call.buf()).await
    }

    async fn write_deterministic_timex<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buf: Option<reverie::syscalls::AddrMut<'_, libc::timex>>,
    ) -> Result<i64, Error> {
        let buf = buf.ok_or(Errno::EFAULT)?;
        let request: libc::timex = guest.memory().read_value(buf)?;
        if !timex_mode_is_query(request.modes) {
            return Err(Errno::EPERM.into());
        }

        let now: Timespec = thread_observe_time(guest).await.into();
        guest.memory().write_value(buf, &deterministic_timex(now))?;
        Ok(libc::TIME_ERROR as i64)
    }

    /// Helper function to wait a given period, which may either succeed or be interrupted by a signal.
    /// Return 0 or EINTR respectively.
    async fn wait_and_return<R: Guest<Self>>(
        guest: &mut R,
        request: Resources,
        call: NanosleepFamily,
    ) -> Result<i64, Error> {
        let target_time = time_from_resources(&request).expect("a sleepuntil resource request");
        match crate::tool_global::parked_wait_request(
            guest,
            request,
            crate::scheduler::parked::ParkedWaitPolicy::NanosleepNoHandlerRestart {
                absolute_deadline: target_time,
            },
        )
        .await
        {
            ResumeStatus::Normal => Ok(0),
            ResumeStatus::Signaled(_) => {
                let now = thread_observe_time(guest).await;
                let delta = remaining_sleep_duration(target_time, now);
                // Linux never touches remain for TIMER_ABSTIME, even when
                // a caught signal interrupts the absolute sleep.
                let addr2 = if call.flags() & libc::TIMER_ABSTIME == 0 {
                    call.rem()
                } else {
                    None
                };
                if let Some(addr2) = addr2 {
                    info!(
                        "[interrupted] sleep till (until {}), woke up {:?} early, writing into nanosleep rem argument.",
                        target_time, delta
                    );
                    let t = Timespec {
                        tv_sec: delta.as_secs() as i64,
                        tv_nsec: delta.subsec_nanos() as i64,
                    };
                    guest.memory().write_value(addr2, &t)?;
                } else {
                    info!("[interrupted] nanosleep rem argument is null, not writing it.")
                }
                Err(reverie::Error::Errno(Errno::EINTR))
            }
        }
    }

    /// clock_nanosleep and nanosleep
    pub async fn handle_nanosleep_family<R: Guest<Self>>(
        &self,
        guest: &mut R,
        call: NanosleepFamily,
    ) -> Result<i64, Error> {
        if call.flags() > libc::TIMER_ABSTIME {
            trace!("Unhandled clock_nanosleep flags, letting syscall through...");
            return Ok(guest.inject(Syscall::from(call)).await?);
        }

        let addr = call.req().ok_or(Errno::EFAULT)?;
        let t: Timespec = guest.memory().read_value(addr)?;

        // Linux validates the requested interval BEFORE sleeping: nanosleep(2)
        // and clock_nanosleep(2) both fail EINVAL when tv_nsec is outside
        // [0, 999999999] or tv_sec is negative.
        //
        // Detcore skipped that check and fed the raw fields through `as u64`.
        // `Timespec` stores both as i64, so `tv_sec = -1` wrapped to
        // u64::MAX (~1.8e19 seconds) and became `SleepUntil(INDEFINITE)`: the
        // only guest thread parked with no deadline, the run queue emptied, and
        // `step2d_handle_empty_queue` deliberately never jumps the clock for an
        // indefinite waiter (doing so would also wake a `pause(2)`). The
        // container then died -- exit 1, "Sandbox container exited
        // unexpectedly" -- where Linux returns an errno and keeps running.
        //
        // A past *absolute* deadline is NOT an error and must still return 0,
        // so this rejects only malformed fields, never an early deadline.
        if t.tv_sec < 0 || t.tv_nsec < 0 || t.tv_nsec > 999_999_999 {
            return Err(Errno::EINVAL.into());
        }

        match call.flags() {
            0 => {
                if self.cfg.sequentialize_threads {
                    let time = Duration::from_secs(t.tv_sec as u64)
                        + Duration::from_nanos(t.tv_nsec as u64);
                    let request = Self::sleep_request(guest, time).await;
                    trace!(
                        "nanosleep adding delta {:?} to yield request {:?}",
                        time, &request
                    );
                    Self::wait_and_return(guest, request, call).await
                } else {
                    trace!("Not sequentializing threads, letting nanosleep through...");
                    Ok(guest.inject(Syscall::from(call)).await?)
                }
            }
            libc::TIMER_ABSTIME => {
                let target_time = LogicalTime::from_secs(t.tv_sec as u64)
                    + LogicalTime::from_nanos(t.tv_nsec as u64);
                if self.cfg.sequentialize_threads {
                    if self.cfg.virtualize_time {
                        let request = Self::sleep_request_abs(guest, target_time).await;
                        trace!(
                            "nanosleep setting absolute time {:?} to yield request {:?}",
                            target_time, &request
                        );
                        Self::wait_and_return(guest, request, call).await
                    } else {
                        // TODO T124594597: Record-replay case here, need better ideas to enable proper handling of this case.
                        error!(
                            "Sequentializing but not virtualizing, so can't rely on passed abs time, especially when replaying a recording, just yelding"
                        );
                        let request = Self::yield_request(guest);
                        Self::wait_and_return(guest, request, call).await
                    }
                } else if self.cfg.virtualize_time {
                    trace!(
                        "Not sequentializing, but virtualizing so calculating relative time and invoking nanosleep..."
                    );
                    let relative_ts = Self::relative_time_from_abs_target(guest, target_time).await;
                    let mut stack = guest.stack().await;
                    let req = stack.push(relative_ts);
                    stack.commit()?;
                    let modified_call = syscalls::Nanosleep::new().with_req(Some(req));
                    Ok(guest.inject(modified_call).await?)
                } else {
                    trace!(
                        "Not sequentializing threads not virtualizing, letting nanosleep through..."
                    );
                    Ok(guest.inject(Syscall::from(call)).await?)
                }
            }
            _ => unreachable!("Unexpected, unhandled flag value"),
        }
    }

    async fn relative_time_from_abs_target<G: Guest<Self>>(
        guest: &mut G,
        target_time: LogicalTime,
    ) -> Timespec {
        let base_time = thread_observe_time(guest).await;

        // An absolute deadline already in the past is NOT an error on Linux --
        // clock_nanosleep(TIMER_ABSTIME) simply returns 0 without sleeping.
        // `LogicalTime`'s `Sub` is a plain subtraction (unlike its `Add` impls,
        // which saturate deliberately), so `target_time - base_time` underflows
        // for a past deadline: a debug build panics with "attempt to subtract
        // with overflow", and a release build wraps to an enormous interval --
        // the same effectively-indefinite sleep this handler exists to avoid.
        //
        // Clamped here rather than by making the shared operator saturate,
        // because this is the only subtraction of two `LogicalTime`s in the
        // tree and a silently-saturating operator could hide a real underflow
        // in some future caller.
        let relative_logical = if target_time <= base_time {
            LogicalTime::from_nanos(0)
        } else {
            target_time - base_time
        };

        Timespec {
            tv_sec: relative_logical.as_secs() as i64,
            tv_nsec: relative_logical.subsec_nanos() as i64,
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// timer_create: allocate a per-process POSIX timer and hand back a
    /// deterministic id, retaining any scheduler-deliverable signal.
    pub async fn handle_timer_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerCreate,
    ) -> Result<i64, Error> {
        // The kernel writes the new timer id here; a null pointer is EFAULT.
        let timerid_ptr = call.timerid().ok_or(Errno::EFAULT)?;
        let clockid = call.clockid();
        let signal = if let Some(event_ptr) = call.sevp() {
            let event: libc::sigevent = guest.memory().read_value(event_ptr)?;
            match event.sigev_notify {
                libc::SIGEV_NONE => None,
                // Linux uses 4 for SIGEV_THREAD_ID. Treat it as process-directed
                // until Detcore tracks per-timer thread targeting.
                libc::SIGEV_SIGNAL | 4 => {
                    if !(1..=64).contains(&event.sigev_signo) {
                        return Err(Errno::EINVAL.into());
                    }
                    Signal::try_from(event.sigev_signo).ok()
                }
                _ => return Err(Errno::ENOSYS.into()),
            }
        } else {
            Some(Signal::SIGALRM)
        };
        let id = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.create(signal.map(|sig| sig as i32))
        };
        guest
            .memory()
            .write_value(timerid_ptr, &(id as libc::c_int))?;
        detlog!(
            "[dtid {}] timer_create(clockid={:?}) => deterministic timer id {}, signal {:?}",
            guest.thread_state().dettid,
            clockid,
            id,
            signal,
        );
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// timer_settime: arm or disarm a timer against the deterministic virtual
    /// clock. The old arming is reported through `old_value` when requested.
    pub async fn handle_timer_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerSettime,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let new_ptr = call.new_value().ok_or(Errno::EINVAL)?;
        let new: libc::itimerspec = guest.memory().read_value(new_ptr)?;
        let interval_ns = timespec_to_ns(new.it_interval);
        let value_ns = timespec_to_ns(new.it_value);

        let now = thread_observe_time(guest).await;
        let deadline = if value_ns == 0 {
            None
        } else if call.flags() & libc::TIMER_ABSTIME != 0 {
            // Absolute expiration is interpreted against the same virtual clock.
            Some(LogicalTime::from_nanos(value_ns))
        } else {
            Some(now + Duration::from_nanos(value_ns))
        };

        let (old, signal_number) = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            let old = timers.settime(id, interval_ns, deadline, now);
            let signal = timers.signal(id);
            (old, signal)
        };
        let (old_remaining_ns, old_interval_ns) = old.ok_or(Errno::EINVAL)?;
        let signal_number = signal_number.ok_or(Errno::EINVAL)?;

        if let Some(old_ptr) = call.old_value() {
            let old_spec = libc::itimerspec {
                it_interval: ns_to_timespec(old_interval_ns),
                it_value: ns_to_timespec(old_remaining_ns),
            };
            guest.memory().write_value(old_ptr, &old_spec)?;
        }

        if let Some(signal) = signal_number.and_then(|signum| Signal::try_from(signum).ok()) {
            register_posix_timer(
                guest,
                id,
                deadline,
                LogicalTime::from_nanos(interval_ns),
                signal,
            )
            .await;
        }

        detlog!(
            "[dtid {}] timer_settime(id={}, interval_ns={}, value_ns={}) armed against virtual clock",
            guest.thread_state().dettid,
            id,
            interval_ns,
            value_ns,
        );
        Ok(0)
    }

    /// timer_gettime: report the time remaining until the next expiration and
    /// the reload interval, both computed from the virtual clock.
    pub async fn handle_timer_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerGettime,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let value_ptr = call.value().ok_or(Errno::EFAULT)?;
        let now = thread_observe_time(guest).await;
        let cur = {
            let timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.gettime(id, now)
        };
        let (remaining_ns, interval_ns) = cur.ok_or(Errno::EINVAL)?;
        let spec = libc::itimerspec {
            it_interval: ns_to_timespec(interval_ns),
            it_value: ns_to_timespec(remaining_ns),
        };
        guest.memory().write_value(value_ptr, &spec)?;
        Ok(0)
    }

    /// timer_getoverrun: coalesced expiration accounting is not modeled, so the
    /// overrun count is always 0 for a live timer.
    pub async fn handle_timer_getoverrun<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerGetoverrun,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let exists = guest
            .thread_state()
            .posix_timers
            .lock()
            .unwrap()
            .contains(id);
        if exists {
            Ok(0)
        } else {
            Err(Errno::EINVAL.into())
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    /// timer_delete: destroy a timer created by `timer_create`.
    pub async fn handle_timer_delete<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerDelete,
    ) -> Result<i64, Error> {
        let id = call.timerid();
        let signal_number = guest
            .thread_state()
            .posix_timers
            .lock()
            .unwrap()
            .signal(id)
            .ok_or(Errno::EINVAL)?;
        let existed = {
            let mut timers = guest.thread_state().posix_timers.lock().unwrap();
            timers.remove(id)
        };
        if existed {
            if let Some(signal) = signal_number.and_then(|signum| Signal::try_from(signum).ok()) {
                register_posix_timer(guest, id, None, LogicalTime::ZERO, signal).await;
            }
            detlog!(
                "[dtid {}] timer_delete(id={})",
                guest.thread_state().dettid,
                id,
            );
            Ok(0)
        } else {
            Err(Errno::EINVAL.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timex_policy_distinguishes_queries_from_mutations() {
        assert!(timex_mode_is_query(0));
        assert!(timex_mode_is_query(libc::ADJ_OFFSET_SS_READ));
        assert!(!timex_mode_is_query(libc::ADJ_OFFSET));
        assert!(!timex_mode_is_query(libc::ADJ_FREQUENCY));
    }

    #[test]
    fn timex_snapshot_is_unsynchronized_and_uses_virtual_time() {
        let tx = deterministic_timex(Timespec {
            tv_sec: 123,
            tv_nsec: 456_789_000,
        });
        assert_eq!(tx.status, libc::STA_UNSYNC);
        assert_eq!(tx.tick, 10_000);
        assert_eq!(tx.time.tv_sec, 123);
        assert_eq!(tx.time.tv_usec, 456_789);
    }

    #[test]
    fn interrupted_sleep_remaining_time_floors_at_zero() {
        let target = LogicalTime::from_nanos(1_000);

        assert_eq!(
            remaining_sleep_duration(target, LogicalTime::from_nanos(750)),
            Duration::from_nanos(250)
        );
        assert_eq!(remaining_sleep_duration(target, target), Duration::ZERO);
        assert_eq!(
            remaining_sleep_duration(target, LogicalTime::from_nanos(1_250)),
            Duration::ZERO
        );
    }

    /// `overwrite_failed_gettimeofday_tv` against a model of two guest pages
    /// whose protections are set independently.
    mod failed_gettimeofday_tv {
        use std::cell::RefCell;
        use std::io::IoSlice;
        use std::io::IoSliceMut;

        use reverie::syscalls::Addr;

        use super::*;

        const BASE: usize = 0x10_0000;
        const PAGE: usize = TV_WORD_PAGE_GRANULE;
        /// The boundary between the two modelled pages.
        const BOUNDARY: usize = BASE + PAGE;
        const FILL: u8 = 0xa5;
        /// What the host stored in each word.
        const HOST_WORDS: [[u8; 8]; 2] = [[0x48; 8], [0x68; 8]];
        const VIRTUAL_TV: Timeval = Timeval {
            tv_sec: 0x5656_5656_5656_5656,
            tv_usec: 0x7575_7575_7575_7575,
        };

        fn virtual_words() -> [[u8; 8]; 2] {
            [
                VIRTUAL_TV.tv_sec.to_ne_bytes(),
                VIRTUAL_TV.tv_usec.to_ne_bytes(),
            ]
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Prot {
            ReadWrite,
            ReadOnly,
            WriteOnly,
            NoAccess,
        }

        impl Prot {
            const ALL: [Self; 4] = [
                Self::ReadWrite,
                Self::ReadOnly,
                Self::WriteOnly,
                Self::NoAccess,
            ];

            fn readable(self) -> bool {
                matches!(self, Self::ReadWrite | Self::ReadOnly)
            }

            fn writable(self) -> bool {
                matches!(self, Self::ReadWrite | Self::WriteOnly)
            }
        }

        /// Replaces the outcome of one user-access copy.
        #[derive(Clone, Copy, Debug)]
        enum Inject {
            /// Fail with this errno, copying nothing.
            Fail(Errno),
            /// Copy up to this many bytes, ignoring protections, and report
            /// this count.
            Count(usize),
        }

        /// Two guest pages. User-access copies behave like one single-iovec
        /// `process_vm_readv` or `process_vm_writev`: a read copies only if
        /// every byte is readable, and a write stores the prefix that lies on
        /// writable pages and fails with EFAULT if that prefix is empty.
        /// Debugger reads and writes panic.
        struct PageMemory {
            bytes: Vec<u8>,
            pages: [Prot; 2],
            /// Which bytes a write has stored, whether or not that changed
            /// them.
            written: Vec<bool>,
            /// Which bytes a write has ever given a different value.
            changed: Vec<bool>,
            /// Every user-access copy, in order: its kind, address and length.
            copies: RefCell<Vec<(&'static str, usize, usize)>>,
            /// The copy, by index into `copies`, whose outcome is replaced.
            inject: Option<(usize, Inject)>,
        }

        impl PageMemory {
            fn new(pages: [Prot; 2], bytes: Vec<u8>) -> Self {
                Self {
                    written: vec![false; bytes.len()],
                    changed: vec![false; bytes.len()],
                    bytes,
                    pages,
                    copies: RefCell::default(),
                    inject: None,
                }
            }

            fn record(&self, kind: &'static str, addr: usize, len: usize) -> Option<Inject> {
                let mut copies = self.copies.borrow_mut();
                copies.push((kind, addr, len));
                let index = copies.len() - 1;
                self.inject
                    .filter(|&(at, _)| at == index)
                    .map(|(_, inject)| inject)
            }

            /// How many bytes from `addr` lie on pages that `allowed` accepts.
            fn prefix(&self, addr: usize, len: usize, allowed: fn(Prot) -> bool) -> usize {
                (addr..addr + len)
                    .take_while(|byte| {
                        byte.checked_sub(BASE)
                            .and_then(|offset| self.pages.get(offset / PAGE))
                            .is_some_and(|&prot| allowed(prot))
                    })
                    .count()
            }

            /// The pages that writes stored bytes on without changing any.
            fn rewritten_pages(&self) -> Vec<usize> {
                (0..self.pages.len())
                    .filter(|&page| {
                        let bytes = page * PAGE..(page + 1) * PAGE;
                        self.written[bytes.clone()].contains(&true)
                            && !self.changed[bytes].contains(&true)
                    })
                    .collect()
            }
        }

        impl MemoryAccess for PageMemory {
            fn read_vectored(&self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
                panic!("the repair attempted a debugger read")
            }

            fn write_vectored(
                &mut self,
                _: &[IoSlice],
                _: &mut [IoSliceMut],
            ) -> Result<usize, Errno> {
                panic!("the repair attempted a debugger write")
            }

            fn read_exact_with_user_access<'a, A>(
                &self,
                addr: A,
                buf: &mut [u8],
            ) -> Result<(), Errno>
            where
                A: Into<Addr<'a, u8>>,
            {
                let addr = addr.into().as_raw();
                match self.record("read", addr, buf.len()) {
                    Some(Inject::Fail(errno)) => return Err(errno),
                    Some(Inject::Count(_)) => panic!("an exact read reports no count"),
                    None => {}
                }
                if self.prefix(addr, buf.len(), Prot::readable) < buf.len() {
                    return Err(Errno::EFAULT);
                }
                let offset = addr - BASE;
                buf.copy_from_slice(&self.bytes[offset..offset + buf.len()]);
                Ok(())
            }

            fn write_with_user_access(
                &mut self,
                addr: AddrMut<u8>,
                buf: &[u8],
            ) -> Result<usize, Errno> {
                let addr = addr.as_raw();
                let (copied, reported) = match self.record("write", addr, buf.len()) {
                    Some(Inject::Fail(errno)) => return Err(errno),
                    Some(Inject::Count(count)) => (count.min(buf.len()), count),
                    None => {
                        let copied = self.prefix(addr, buf.len(), Prot::writable);
                        if copied == 0 && !buf.is_empty() {
                            return Err(Errno::EFAULT);
                        }
                        (copied, copied)
                    }
                };
                for (at, &byte) in (addr - BASE..).zip(&buf[..copied]) {
                    self.written[at] = true;
                    self.changed[at] |= self.bytes[at] != byte;
                    self.bytes[at] = byte;
                }
                Ok(reported)
            }
        }

        /// The two pages as Linux leaves them when `tz` faults: each word is
        /// stored by one eight-byte store, which commits only if every byte it
        /// touches is on a writable page, and the first word that does not
        /// commit ends the call.
        fn linux(pages: [Prot; 2], tv_addr: usize, words: [[u8; 8]; 2]) -> Vec<u8> {
            let mut bytes = vec![FILL; 2 * PAGE];
            for (index, word) in words.into_iter().enumerate() {
                let start = tv_addr - BASE + 8 * index;
                if !(pages[start / PAGE].writable() && pages[(start + 7) / PAGE].writable()) {
                    break;
                }
                bytes[start..start + 8].copy_from_slice(&word);
            }
            bytes
        }

        /// The first word that Linux does not store: its index, its offset
        /// from `BASE`, and the protections of the pages holding its first and
        /// last bytes.
        fn first_unstored(pages: [Prot; 2], tv_addr: usize) -> Option<(usize, usize, Prot, Prot)> {
            (0..2).find_map(|index| {
                let start = tv_addr - BASE + 8 * index;
                let (first, last) = (pages[start / PAGE], pages[(start + 7) / PAGE]);
                (!(first.writable() && last.writable())).then_some((index, start, first, last))
            })
        }

        /// How many bytes of `tv_sec` the repair leaves stored where Linux
        /// stores none: those on its first page when it crosses from a
        /// write-only page into an inaccessible one. Neither part can be
        /// read, and no earlier word shows the first page writable, so the
        /// whole word is written, and that stops at the second page.
        fn partly_stored(pages: [Prot; 2], tv_addr: usize) -> Option<usize> {
            match first_unstored(pages, tv_addr)? {
                (0, start, Prot::WriteOnly, Prot::NoAccess) => Some(PAGE - start % PAGE),
                _ => None,
            }
        }

        /// The pages the repair writes only with their own bytes, although
        /// Linux does not write them: a page holding part of `tv_sec` that is
        /// rewritten to check it, when `tv_sec` is then left alone because
        /// its other page is not writable.
        fn rewritten_only(pages: [Prot; 2], tv_addr: usize) -> Vec<usize> {
            match first_unstored(pages, tv_addr) {
                // The readable second page is rewritten, and then the whole
                // word is written, which the first page refuses.
                Some((0, _, Prot::ReadOnly | Prot::NoAccess, Prot::ReadWrite)) => vec![1],
                // The second page cannot be read, so the first is rewritten,
                // and then the second refuses its part.
                Some((0, _, Prot::ReadWrite, Prot::NoAccess)) => vec![0],
                _ => Vec::new(),
            }
        }

        fn repair(memory: &mut PageMemory, tv_addr: usize) -> Result<(), Error> {
            overwrite_failed_gettimeofday_tv(
                memory,
                AddrMut::from_raw(tv_addr).unwrap(),
                &VIRTUAL_TV,
            )
        }

        fn failure(result: Result<(), Error>) -> TvRepairFailure {
            match result {
                Err(Error::Tool(error)) => *error
                    .downcast_ref::<TvRepairFailure>()
                    .expect("a typed repair failure"),
                other => panic!("expected a Tool error, got {other:?}"),
            }
        }

        fn window(bytes: &[u8]) -> &[u8] {
            &bytes[PAGE - 24..PAGE + 24]
        }

        #[test]
        fn stores_virtual_time_in_exactly_the_words_linux_stored() {
            for first in Prot::ALL {
                for second in Prot::ALL {
                    // `tv` wholly on the first page, `tv_usec` straddling, one
                    // word on each page, `tv_sec` straddling, and wholly on the
                    // second page.
                    for back in [16, 12, 8, 4, 0] {
                        let pages = [first, second];
                        let tv_addr = BOUNDARY - back;
                        let case = format!("pages {pages:?}, tv at boundary - {back}");
                        let host = linux(pages, tv_addr, HOST_WORDS);
                        let mut memory = PageMemory::new(pages, host.clone());
                        let result = repair(&mut memory, tv_addr);

                        let mut expected = linux(pages, tv_addr, virtual_words());
                        match partly_stored(pages, tv_addr) {
                            None => assert!(result.is_ok(), "{case}: {result:?}"),
                            Some(stored) => {
                                let start = tv_addr - BASE;
                                expected[start..start + stored]
                                    .copy_from_slice(&virtual_words()[0][..stored]);
                                assert_eq!(
                                    failure(result),
                                    TvRepairFailure {
                                        field: "tv_sec",
                                        kind: TvRepairFailureKind::PartlyStored { stored },
                                    },
                                    "{case}"
                                );
                            }
                        }
                        assert!(
                            memory.bytes == expected,
                            "{case}: bytes around the boundary are {:02x?}, expected {:02x?}",
                            window(&memory.bytes),
                            window(&expected),
                        );
                        let put_back: Vec<usize> = (0..host.len())
                            .filter(|&at| memory.changed[at] && memory.bytes[at] == host[at])
                            .collect();
                        assert!(
                            put_back.is_empty(),
                            "{case}: bytes changed and then put back at offsets {put_back:?}"
                        );
                        assert_eq!(
                            memory.rewritten_pages(),
                            rewritten_only(pages, tv_addr),
                            "{case}: pages written only with their own bytes"
                        );
                        for &(kind, addr, len) in memory.copies.borrow().iter() {
                            assert!(
                                addr >= tv_addr && addr + len <= tv_addr + 16,
                                "{case}: {kind} of {len} bytes at {addr:#x} is outside tv"
                            );
                        }
                    }
                }
            }
        }

        #[test]
        fn reports_every_other_copy_failure_as_a_typed_tool_error() {
            use TvRepairFailureKind::Failed;
            use TvRepairFailureKind::ImpossibleCount;
            use TvRepairFailureKind::PartlyStored;

            // An aligned `tv` on writable memory takes two copies: write
            // `tv_sec`, then write `tv_usec`.
            let aligned = ([Prot::ReadWrite; 2], BOUNDARY - 16);
            // `tv_sec` crossing into a readable page first checks that page:
            // it reads the four bytes there and writes them back unchanged.
            // Then it writes all eight, and `tv_usec` takes one more write.
            let checked = ([Prot::ReadWrite; 2], BOUNDARY - 4);
            // `tv_sec` crossing into an inaccessible page cannot read its
            // four second-page bytes (EFAULT), so it reads its four
            // first-page bytes and writes them back unchanged, and then
            // writes the second-page bytes (EFAULT), which ends the repair.
            let first_checked = ([Prot::ReadWrite, Prot::NoAccess], BOUNDARY - 4);
            // The same with a write-only second page, whose four bytes are
            // written, then the four first-page bytes; `tv_usec` takes one
            // more write.
            let two_parts = ([Prot::ReadWrite, Prot::WriteOnly], BOUNDARY - 4);
            // `tv_usec` crossing into an inaccessible page after `tv_sec` was
            // written wholly on its first page: write `tv_sec`, read the four
            // second-page bytes (EFAULT), write them (EFAULT).
            let known_page = ([Prot::WriteOnly, Prot::NoAccess], BOUNDARY - 12);
            // `tv_sec` crossing from a write-only page into an inaccessible
            // one: read both parts (EFAULT each), write all eight (four are
            // stored), and write the last four (EFAULT).
            let unknown = ([Prot::WriteOnly, Prot::NoAccess], BOUNDARY - 4);
            let cases = [
                (
                    aligned,
                    0,
                    Inject::Fail(Errno::ENOSYS),
                    "tv_sec",
                    Failed {
                        operation: "write",
                        errno: Errno::ENOSYS,
                    },
                ),
                (
                    aligned,
                    1,
                    Inject::Fail(Errno::EIO),
                    "tv_usec",
                    Failed {
                        operation: "write",
                        errno: Errno::EIO,
                    },
                ),
                (
                    aligned,
                    0,
                    Inject::Count(9),
                    "tv_sec",
                    ImpossibleCount {
                        operation: "write",
                        requested: 8,
                        reported: 9,
                    },
                ),
                (
                    aligned,
                    1,
                    Inject::Count(0),
                    "tv_usec",
                    ImpossibleCount {
                        operation: "write",
                        requested: 8,
                        reported: 0,
                    },
                ),
                (
                    checked,
                    0,
                    Inject::Fail(Errno::ENOSYS),
                    "tv_sec",
                    Failed {
                        operation: "second-page read",
                        errno: Errno::ENOSYS,
                    },
                ),
                (
                    checked,
                    1,
                    Inject::Fail(Errno::EIO),
                    "tv_sec",
                    Failed {
                        operation: "second-page rewrite",
                        errno: Errno::EIO,
                    },
                ),
                (
                    checked,
                    1,
                    Inject::Count(5),
                    "tv_sec",
                    ImpossibleCount {
                        operation: "second-page rewrite",
                        requested: 4,
                        reported: 5,
                    },
                ),
                (
                    checked,
                    1,
                    Inject::Count(0),
                    "tv_sec",
                    ImpossibleCount {
                        operation: "second-page rewrite",
                        requested: 4,
                        reported: 0,
                    },
                ),
                (
                    checked,
                    2,
                    Inject::Fail(Errno::EPERM),
                    "tv_sec",
                    Failed {
                        operation: "write",
                        errno: Errno::EPERM,
                    },
                ),
                (
                    checked,
                    3,
                    Inject::Fail(Errno::ESRCH),
                    "tv_usec",
                    Failed {
                        operation: "write",
                        errno: Errno::ESRCH,
                    },
                ),
                (
                    first_checked,
                    1,
                    Inject::Fail(Errno::ENOSYS),
                    "tv_sec",
                    Failed {
                        operation: "first-page read",
                        errno: Errno::ENOSYS,
                    },
                ),
                (
                    first_checked,
                    2,
                    Inject::Fail(Errno::EIO),
                    "tv_sec",
                    Failed {
                        operation: "first-page rewrite",
                        errno: Errno::EIO,
                    },
                ),
                (
                    first_checked,
                    2,
                    Inject::Count(5),
                    "tv_sec",
                    ImpossibleCount {
                        operation: "first-page rewrite",
                        requested: 4,
                        reported: 5,
                    },
                ),
                (
                    first_checked,
                    3,
                    Inject::Fail(Errno::EPERM),
                    "tv_sec",
                    Failed {
                        operation: "second-page write",
                        errno: Errno::EPERM,
                    },
                ),
                (
                    first_checked,
                    3,
                    Inject::Count(0),
                    "tv_sec",
                    ImpossibleCount {
                        operation: "second-page write",
                        requested: 4,
                        reported: 0,
                    },
                ),
                (
                    two_parts,
                    4,
                    Inject::Fail(Errno::EIO),
                    "tv_sec",
                    Failed {
                        operation: "first-page write",
                        errno: Errno::EIO,
                    },
                ),
                (
                    two_parts,
                    4,
                    Inject::Count(9),
                    "tv_sec",
                    ImpossibleCount {
                        operation: "first-page write",
                        requested: 4,
                        reported: 9,
                    },
                ),
                (
                    two_parts,
                    4,
                    Inject::Fail(Errno::EFAULT),
                    "tv_sec",
                    PartlyStored { stored: 4 },
                ),
                (
                    two_parts,
                    5,
                    Inject::Fail(Errno::ESRCH),
                    "tv_usec",
                    Failed {
                        operation: "write",
                        errno: Errno::ESRCH,
                    },
                ),
                (
                    known_page,
                    1,
                    Inject::Fail(Errno::ENOSYS),
                    "tv_usec",
                    Failed {
                        operation: "second-page read",
                        errno: Errno::ENOSYS,
                    },
                ),
                (
                    known_page,
                    2,
                    Inject::Fail(Errno::EIO),
                    "tv_usec",
                    Failed {
                        operation: "second-page write",
                        errno: Errno::EIO,
                    },
                ),
                (
                    unknown,
                    1,
                    Inject::Fail(Errno::EPERM),
                    "tv_sec",
                    Failed {
                        operation: "first-page read",
                        errno: Errno::EPERM,
                    },
                ),
                (
                    unknown,
                    2,
                    Inject::Fail(Errno::EIO),
                    "tv_sec",
                    Failed {
                        operation: "write",
                        errno: Errno::EIO,
                    },
                ),
                (
                    unknown,
                    3,
                    Inject::Fail(Errno::ENOSYS),
                    "tv_sec",
                    Failed {
                        operation: "write",
                        errno: Errno::ENOSYS,
                    },
                ),
            ];
            for ((pages, tv_addr), index, inject, field, kind) in cases {
                let case =
                    format!("{inject:?} at copy {index} for tv at {tv_addr:#x} in {pages:?}");
                let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
                memory.inject = Some((index, inject));
                assert_eq!(
                    failure(repair(&mut memory, tv_addr)),
                    TvRepairFailure { field, kind },
                    "{case}"
                );
                assert_eq!(
                    memory.copies.borrow().len(),
                    index + 1,
                    "{case}: a copy followed the failure"
                );
            }
        }

        #[test]
        fn a_crossing_word_rewrites_its_readable_second_page_part_first() {
            let tv_addr = BOUNDARY - 4;

            // A read-only second page stops the unchanged rewrite, so the first
            // page is never written.
            let pages = [Prot::ReadWrite, Prot::ReadOnly];
            let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
            repair(&mut memory, tv_addr).unwrap();
            assert_eq!(
                memory.copies.borrow()[..],
                [("read", BOUNDARY, 4), ("write", BOUNDARY, 4)]
            );

            // A writable one lets `tv_sec` be written, and `tv_usec`, wholly on
            // the second page, needs no check.
            let pages = [Prot::ReadWrite; 2];
            let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
            repair(&mut memory, tv_addr).unwrap();
            assert_eq!(
                memory.copies.borrow()[..],
                [
                    ("read", BOUNDARY, 4),
                    ("write", BOUNDARY, 4),
                    ("write", tv_addr, 8),
                    ("write", tv_addr + 8, 8),
                ]
            );
        }

        #[test]
        fn a_crossing_word_writes_an_unreadable_second_page_part_first() {
            let tv_addr = BOUNDARY - 4;

            // A readable first page is checked by an unchanged rewrite, and
            // then an inaccessible second page refuses its part, so the word
            // is left alone and no byte changes.
            let pages = [Prot::ReadWrite, Prot::NoAccess];
            let host = linux(pages, tv_addr, HOST_WORDS);
            let mut memory = PageMemory::new(pages, host.clone());
            repair(&mut memory, tv_addr).unwrap();
            assert_eq!(
                memory.copies.borrow()[..],
                [
                    ("read", BOUNDARY, 4),
                    ("read", tv_addr, 4),
                    ("write", tv_addr, 4),
                    ("write", BOUNDARY, 4),
                ]
            );
            assert!(memory.bytes == host && !memory.changed.contains(&true));

            // A write-only second page takes its part, and then the first
            // part is written.
            let pages = [Prot::ReadWrite, Prot::WriteOnly];
            let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
            repair(&mut memory, tv_addr).unwrap();
            assert_eq!(
                memory.copies.borrow()[..],
                [
                    ("read", BOUNDARY, 4),
                    ("read", tv_addr, 4),
                    ("write", tv_addr, 4),
                    ("write", BOUNDARY, 4),
                    ("write", tv_addr, 4),
                    ("write", tv_addr + 8, 8),
                ]
            );
            assert!(memory.bytes == linux(pages, tv_addr, virtual_words()));

            // `tv_usec` crossing after `tv_sec` was written wholly on the same
            // first page needs no first-page check, even though that page
            // cannot be read.
            let tv_addr = BOUNDARY - 12;
            for (second, both) in [(Prot::NoAccess, false), (Prot::WriteOnly, true)] {
                let pages = [Prot::WriteOnly, second];
                let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
                repair(&mut memory, tv_addr).unwrap();
                let mut copies = vec![
                    ("write", tv_addr, 8),
                    ("read", BOUNDARY, 4),
                    ("write", BOUNDARY, 4),
                ];
                if both {
                    copies.push(("write", BOUNDARY - 4, 4));
                }
                assert_eq!(memory.copies.borrow()[..], copies[..], "{pages:?}");
                assert!(
                    memory.bytes == linux(pages, tv_addr, virtual_words()),
                    "{pages:?}: {:02x?}",
                    window(&memory.bytes)
                );
            }

            // When neither part of `tv_sec` can be read, the whole word is
            // written, and a write-only first page keeps its part when the
            // second page refuses the rest.
            let tv_addr = BOUNDARY - 4;
            let pages = [Prot::WriteOnly, Prot::NoAccess];
            let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
            assert_eq!(
                failure(repair(&mut memory, tv_addr)).kind,
                TvRepairFailureKind::PartlyStored { stored: 4 }
            );
            assert_eq!(
                memory.copies.borrow()[..],
                [
                    ("read", BOUNDARY, 4),
                    ("read", tv_addr, 4),
                    ("write", tv_addr, 8),
                    ("write", BOUNDARY, 4),
                ]
            );
        }

        #[test]
        fn a_short_count_does_not_stop_the_store() {
            // A short count does not mean that the next byte is unwritable.
            let pages = [Prot::ReadWrite; 2];
            let tv_addr = BOUNDARY - 16;
            let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
            memory.inject = Some((0, Inject::Count(3)));
            repair(&mut memory, tv_addr).unwrap();
            assert!(
                memory.bytes == linux(pages, tv_addr, virtual_words()),
                "{:02x?}",
                window(&memory.bytes)
            );
            assert_eq!(
                memory.copies.borrow()[..],
                [
                    ("write", tv_addr, 8),
                    ("write", tv_addr + 3, 5),
                    ("write", tv_addr + 8, 8),
                ]
            );

            // A fault after one leaves the bytes before it stored, which is a
            // partly stored word, not an unstored one.
            let pages = [Prot::ReadWrite, Prot::NoAccess];
            let tv_addr = BOUNDARY - 4;
            let mut memory = PageMemory::new(pages, linux(pages, tv_addr, HOST_WORDS));
            memory.inject = Some((3, Inject::Count(3)));
            assert_eq!(
                failure(repair(&mut memory, tv_addr)).kind,
                TvRepairFailureKind::PartlyStored { stored: 3 }
            );
            assert_eq!(
                memory.copies.borrow()[3..],
                [("write", BOUNDARY, 4), ("write", BOUNDARY + 3, 1)]
            );
        }
    }
}
