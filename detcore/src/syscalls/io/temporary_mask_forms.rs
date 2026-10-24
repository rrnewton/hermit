/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1d: the temporary-mask forms of `pselect6`, `ppoll`,
//! `epoll_pwait` and `epoll_pwait2` under the in-guest delivery gate
//! (`Config::in_guest_signal_delivery`; dev-hermit
//! ai_docs/2026-10-16-a5-signal-delivery-design.md, "3c.1 implementation
//! plan, revision 4", D37, D39, D50 readers (3)-(7), IR-3, IR-7, IR-9,
//! IR-22; <https://github.com/rrnewton/hermit/issues/4485>). Dark: nothing
//! sets the gate before step 3c.1f.
//!
//! Once the in-guest runtime runs under a mask of its own (step 3c.1e), no
//! injected call may carry a guest temporary mask that the kernel would keep
//! installed past the call (IR-9): a call that the mask's signal ends returns
//! `-ERESTARTNOHAND` or `-EINTR` and the kernel delivers the signal under
//! the temporary mask as the injection returns, into the runtime. So every
//! form decides in Detcore, from the thread's `/proc` status and the
//! temporary mask read at entry, in Linux's order, and every probe is a
//! zero-timeout call with no mask:
//!
//! - `pselect6` with a zero timeout (any `nfds`), or with more than 64
//!   descriptors and a nonzero timeout: [`Detcore::gated_staged_pselect6`],
//!   on staged copies of the sets (D39);
//! - `pselect6` with at most 64 descriptors and a nonzero timeout: the
//!   internal snapshot path (`handle_internal_pselect6`), whose probes carry
//!   no mask under the gate and which decides at each turn under the
//!   temporary mask (D50 reader (3), IR-9);
//! - masked `ppoll`: [`Detcore::gated_masked_ppoll`] (D50 reader (5));
//! - masked `epoll_pwait` and `epoll_pwait2`: with a nonzero timeout,
//!   [`Detcore::gated_masked_epoll`], with D37's `-EINTR` exception (D39,
//!   IR-3); with a zero timeout, one maskless probe in the call's turn, whose
//!   answer is Linux's (`ep_poll` returns events, else 0 before it tests
//!   `signal_pending`, fs/eventpoll.c:2010-2011 at v6.12), so no injection
//!   carries the guest's mask at all (IR-9, round-1 ruling).
//!
//! Every wait's first probe and first check run in the call's own turn,
//! before any scheduler request (round-1 ruling 2). Under premise P no other
//! guest thread runs before that request, so every instance pending at that
//! check was pending at the call's entry, which IR-7 decides under the
//! guest's own mask, and no cross-process `/proc` reader can run between the
//! entry and that check. From the first request on, the request publishes
//! the temporary mask, every instance still pending from entry is one both
//! masks block, and every later check decides under the temporary mask alone.
//! A wait that could park (a nonzero timeout) and whose first, in-turn probe
//! finds a descriptor ready or an error returns that result after one
//! yielding empty request ([`yield_after_in_turn_result`], round-2 ruling
//! 5): the gate runs with no timeslice, so a thread yields only at a
//! request, and a loop on an always-ready descriptor must not starve the
//! others.
//!
//! Every mask a form decides by and publishes is the one Linux installs,
//! without SIGKILL and SIGSTOP (`set_current_blocked`, kernel/signal.c:
//! 3089-3092 at v6.12, from `set_user_sigmask`, :3169).
//!
//! An argument Detcore's own read cannot reach is EFAULT only where Linux's
//! copy faults too, as the caller's `/proc/<tid>/smaps` shows
//! (`signal::user_copy_faults`, 3c.1c's rule); otherwise the named stopping
//! refusal HERMIT_UNREADABLE_SYSCALL_ARGUMENT, fail closed
//! ([`unreadable_argument`]).

use std::time::Duration;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Timespec;

use super::PSELECT6_INTERNAL_MAX_NFDS;
use super::PrivateCells;
use super::copy_pselect6_fd_set;
use super::ppoll_fds_range;
use super::ppoll_timeout_duration;
use super::pselect6_fd_set_len;
use super::read_pselect6_fd_set;
use super::stage_private_cells;
use super::write_pselect6_fd_set;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::TemporaryMaskCheck;
use crate::syscalls::helpers::kernel_run_entry_refusal;
use crate::syscalls::helpers::record_retry_event;
use crate::syscalls::helpers::temporary_mask_check;
use crate::syscalls::signal::kernel_installed_signal_mask;
use crate::syscalls::signal::read_kernel_sigset;
use crate::syscalls::threads::KERNEL_SIGSET_SIZE;
use crate::syscalls::threads::KernelSigset;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;

/// The size of the calling process's descriptor table (`max_fds`), which
/// Linux clamps a `select` family call's `n` to before it copies the sets
/// (`core_sys_select`, fs/select.c:638-643 at v6.12): `FDSize` in the
/// thread's `/proc` status, which renders `fdt->max_fds` (fs/proc/array.c).
/// Read only for a masked `pselect6` with more than 64 descriptors under the
/// gate; the in-guest backends run single-threaded processes, so no sibling
/// grows the table during the call.
///
/// A status that cannot be read or parsed is Hermit's own failure
/// (`Error::Tool`), never an errno the guest's call could see.
fn fd_table_max_fds(tid: reverie::Pid) -> Result<usize, Error> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status")).map_err(|error| {
        Error::Tool(anyhow::anyhow!(
            "reading /proc/{tid}/status for a pselect6's descriptor table size: {error}"
        ))
    })?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("FDSize:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| {
            Error::Tool(anyhow::anyhow!(
                "/proc/{tid}/status has no FDSize Detcore can parse"
            ))
        })
}

/// A5 step 3c.1d, round-1 ruling 4: an argument of `call` (`what`, the bytes
/// `[start, start + len)`) that Detcore's own read could not reach. On the
/// in-guest backends that read refuses some memory x86's `copy_from_user`
/// reads (a write-only mapping), so this is EFAULT only where Linux's copy
/// faults too, as the caller's `/proc/<tid>/smaps` shows in its own turn
/// (`signal::user_copy_faults`: a byte in no mapping and not below a
/// grow-down one, or in a mapping with no read, write or execute permission).
/// Any other case is not knowable: the named stopping refusal
/// HERMIT_UNREADABLE_SYSCALL_ARGUMENT (D49, which ends the run under the
/// gate), fail closed; were the run to go on, `ENOSYS`.
async fn unreadable_argument<T, G>(
    guest: &mut G,
    call: &'static str,
    what: &'static str,
    start: usize,
    len: usize,
) -> Error
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if crate::syscalls::signal::user_copy_faults(guest.tid().as_raw(), start, len) == Some(true) {
        return Errno::EFAULT.into();
    }
    let reason = crate::refused_signal_controls::unreadable_syscall_argument_refusal(
        call,
        what,
        start,
        guest.thread_state().dettid.as_raw(),
    );
    tracing::error!("{}", reason);
    crate::tool_global::stopping_refusal(guest, reason).await;
    Errno::ENOSYS.into()
}

/// A value of type `V` at `address`, an argument of `call` (`what`), read
/// through Detcore's own memory access; a failed read is classified by
/// [`unreadable_argument`].
async fn read_argument<V: Copy, T, G>(
    guest: &mut G,
    call: &'static str,
    what: &'static str,
    address: usize,
) -> Result<V, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let value = Addr::<V>::from_raw(address).and_then(|at| guest.memory().read_value(at).ok());
    match value {
        Some(value) => Ok(value),
        None => {
            Err(unreadable_argument(guest, call, what, address, std::mem::size_of::<V>()).await)
        }
    }
}

/// A temporary mask at `address`, an argument of `call`: the kernel-access
/// probe and the read (`read_kernel_sigset`), with an `EFAULT` from either
/// classified by [`unreadable_argument`]; `first` says it is the call's
/// first injection (`restart_if_first_probe_not_run`). The mask Linux
/// installs from it (without SIGKILL and SIGSTOP) is returned.
async fn read_temporary_mask<T, G>(
    guest: &mut G,
    call: &'static str,
    address: usize,
    first: bool,
) -> Result<KernelSigset, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(at) = Addr::<libc::sigset_t>::from_raw(address) else {
        return Err(Errno::EFAULT.into());
    };
    let read = read_kernel_sigset(guest, at).await;
    let read = if first {
        crate::syscalls::signal::restart_if_first_probe_not_run(read)
    } else {
        read
    };
    match read {
        Ok(mask) => {
            crate::syscalls::signal_shadow::note_wait_mask(guest, mask);
            Ok(kernel_installed_signal_mask(mask))
        }
        Err(Error::Errno(Errno::EFAULT)) => {
            Err(
                unreadable_argument(guest, call, "the signal mask", address, KERNEL_SIGSET_SIZE)
                    .await,
            )
        }
        Err(error) => Err(error),
    }
}

/// A `select` family set of `len` bytes at `set`, an argument of `call`,
/// read as `read_pselect6_fd_set` reads it, with a failed read classified by
/// [`unreadable_argument`].
pub(super) async fn read_gated_fd_set<T, G>(
    guest: &mut G,
    call: &'static str,
    set: Option<AddrMut<'_, libc::fd_set>>,
    len: usize,
) -> Result<Option<Vec<u8>>, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    match read_pselect6_fd_set(guest, set, len) {
        Ok(bytes) => Ok(bytes),
        Err(_) => {
            let start = set.map_or(0, |set| set.as_raw());
            Err(unreadable_argument(guest, call, "a descriptor set", start, len).await)
        }
    }
}

/// The read, write and except sets of `call`, each `len` bytes, read in
/// that order by [`read_gated_fd_set`] (`core_sys_select`'s `get_fd_set`
/// calls, fs/select.c:670-673 at v6.12): the first that faults is the error.
pub(super) async fn read_gated_fd_sets<T, G>(
    guest: &mut G,
    caller: &'static str,
    call: syscalls::Pselect6,
    len: usize,
) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let readfds = read_gated_fd_set(guest, caller, call.readfds(), len).await?;
    let writefds = read_gated_fd_set(guest, caller, call.writefds(), len).await?;
    let exceptfds = read_gated_fd_set(guest, caller, call.exceptfds(), len).await?;
    Ok((readfds, writefds, exceptfds))
}

/// Round-2 ruling 5: one yielding empty request after a result that a
/// wait's first probe found in the call's own turn, before the call returns
/// it. The gate runs with no timeslice (`two_phase_refusal` refuses
/// `target_timeslice` and `max_timeslice`), so a thread yields only when it
/// makes a request, and a masked wait returning at once on an always-ready
/// descriptor, in a loop, would starve every other thread; before the gate
/// such a wait requested before its first probe. The result is already
/// decided. The request names no resource and no temporary mask, so it
/// publishes the guest's own mask (`publish_request_guest_mask`), the mask
/// Linux has put back by the time the call returns; the instances pending
/// from entry stay pending under it (IR-7 row (A)).
pub(super) async fn yield_after_in_turn_result<T, G>(guest: &mut G)
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let dettid = guest.thread_state().dettid;
    resource_request(guest, Resources::new(dettid)).await; // empty request
}

/// Round-3 ruling 1: the deadline of a temporary-mask wait, observed only
/// after its first probe and check, so that no RPC at all comes between the
/// call's entry and that check (`send_and_update_time` allows any request to
/// let another thread run; a signal committed then must not be decided as an
/// instance pending at entry, IR-7). The observation itself
/// (`GlobalTimeLowerBound`) grants no turn and moves no time
/// (`a_clock_observation_grants_no_turn_and_moves_no_time`), and within the
/// call's own turn no guest code runs before it, so global time can have
/// moved since the entry only through a sibling running concurrently (a
/// background call), as for the base's own timed waits, which observe through
/// the same RPC. With threads sequentialized the deadline is therefore the one
/// Linux takes at entry (`poll_select_set_timeout` in `do_pselect` and
/// `ppoll`, fs/select.c:752 and :1113 at v6.12). `None` for no timeout.
pub(super) async fn observe_deadline<T, G>(
    guest: &mut G,
    timeout: Option<Duration>,
) -> Option<(LogicalTime, LogicalTime)>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let timeout = timeout?;
    let started_at = thread_observe_time(guest).await;
    Some((started_at, started_at + timeout))
}

/// Round-3 ruling 1: the at-entry set of a wait's first check
/// (`temporary_mask_check`'s `entry`). The one RPC a wait must make before its
/// first probe is the publication of expired virtual timerfds, which a probe
/// must see as Linux's first pass does; it observes the clock only while the
/// process holds an armed virtual timerfd. Then the signals pending before
/// it are read first (one `/proc` read, no RPC), so a signal committed during
/// a hand-off that publication allows is decided under the temporary mask,
/// as one sent during the call. Otherwise everything pending at the first
/// check was pending at entry (`!0`). The clock observation that RPC makes
/// grants no turn, but a sibling in a background call can run concurrently;
/// for what another thread may do during it, three named residuals of the
/// armed-timerfd case are carried to step 3c.1e (review round 4): the
/// temporary mask is not yet published, so a cross-process `/proc` reader
/// sees the previous mask; this set holds signal numbers, not instances, so
/// an ignored signal pending at entry and sent again then, under a temporary
/// mask that blocks it, is removed whole; and a deadline observed later
/// reads any time that thread added.
pub(super) fn entry_before_publication<T, G>(guest: &G) -> Result<KernelSigset, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if !crate::syscalls::timerfd::holds_armed_virtual_timerfd(guest) {
        return Ok(!0);
    }
    Ok(crate::syscalls::threads::read_thread_wait_signal_state(guest.pid(), guest.tid())?.pending)
}

/// What a check found, as the call's error, or `None` when the wait goes on.
fn check_result(check: TemporaryMaskCheck, restart: Errno) -> Option<Errno> {
    match check {
        TemporaryMaskCheck::Waits => None,
        TemporaryMaskCheck::Fatal => Some(restart),
        TemporaryMaskCheck::EpollInterrupted | TemporaryMaskCheck::Refused => Some(Errno::EINTR),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// `pselect6` under the in-guest delivery gate, threads sequentialized,
    /// in run mode (D39, D50 reader (3), IR-9), in Linux's order
    /// (fs/select.c at v6.12):
    ///
    /// 0. A call with a mask wrapper first takes D41's check under the
    ///    guest's own mask (`kernel_run_entry_refusal`): a default-fatal
    ///    signal deliverable before the call ends the process before it,
    ///    and the call is not run. Without it, a signal the temporary mask
    ///    blocks would be held for as long as the call waits.
    /// 1. The wrapper (`get_sigset_argpack`, :775-803: `EFAULT`), the
    ///    timeout (`do_pselect`, :739-753: `EFAULT`, `EINVAL`), the mask's size
    ///    and the mask (`set_user_sigmask`, :756-758: `EINVAL`, `EFAULT`), each
    ///    read without scratch memory, a failed read classified.
    /// 2. `n < 0` (`core_sys_select`, :634-635): `EINVAL`.
    /// 3. No temporary mask: the call as before the gate (a raw zero-timeout
    ///    call, which installs no mask; the kernel-run call for more than 64
    ///    descriptors; otherwise the internal path, which decides under the
    ///    guest's own mask).
    /// 4. A temporary mask and a zero timeout, or more than 64 descriptors:
    ///    [`Self::gated_staged_pselect6`]. Otherwise the internal path, under
    ///    the temporary mask.
    pub(super) async fn gated_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
    ) -> Result<i64, Error> {
        if call.sigmask().is_some()
            && let Some(refused) = kernel_run_entry_refusal(guest)?
        {
            return Err(refused);
        }
        // The wrapper is read first, as Linux copies it first, with a failed
        // read classified (`read_argument`): no scratch is needed, so a tight
        // stack cannot turn it into an EFAULT of Hermit's own (round-3 ruling
        // 3; the gate runs only in-guest backends, whose reads respect the
        // page permissions, so the kernel's own validation of the wrapper,
        // `validate_pselect6_wrapper`, adds nothing here).
        let wrapper = match call.sigmask() {
            Some(argument) => Some(
                read_argument::<[usize; 2], _, _>(
                    guest,
                    "pselect6",
                    "the mask wrapper",
                    argument.as_raw(),
                )
                .await?,
            ),
            None => None,
        };
        let timeout = match call.timeout() {
            Some(timeout) => Some(ppoll_timeout_duration(
                read_argument::<Timespec, _, _>(guest, "pselect6", "the timeout", timeout.as_raw())
                    .await?,
            )?),
            None => None,
        };
        let mask = match wrapper {
            Some([0, _]) | None => None,
            Some([sigmask, sigsetsize]) => {
                if sigsetsize != KERNEL_SIGSET_SIZE {
                    return Err(Errno::EINVAL.into());
                }
                // Now the call's first injection.
                Some(read_temporary_mask(guest, "pselect6", sigmask, true).await?)
            }
        };
        if call.nfds() < 0 {
            return Err(Errno::EINVAL.into());
        }
        let zero = timeout == Some(Duration::ZERO);
        match mask {
            None if zero => {
                crate::syscalls::timerfd::publish_expired(guest).await;
                Ok(guest.inject(call).await?)
            }
            None if call.nfds() > PSELECT6_INTERNAL_MAX_NFDS => {
                self.record_or_replay_blocking_with_mask(guest, Syscall::Pselect6(call), None)
                    .await
            }
            None => {
                self.handle_internal_pselect6(guest, call.with_sigmask(None), timeout, None)
                    .await
            }
            Some(mask) if zero || call.nfds() > PSELECT6_INTERNAL_MAX_NFDS => {
                self.gated_staged_pselect6(guest, call, mask, timeout).await
            }
            Some(mask) => {
                self.handle_internal_pselect6(guest, call.with_sigmask(None), timeout, Some(mask))
                    .await
            }
        }
    }

    /// A masked `pselect6` under the gate with a zero timeout (any `nfds`),
    /// or with more than 64 descriptors and a nonzero timeout (D39 as D54
    /// amends it), in Linux's order (`core_sys_select`, fs/select.c:620-691
    /// at v6.12):
    ///
    /// 1. The sets are read (`get_fd_set`, :670-673: `EFAULT`), each
    ///    `FDS_BYTES(min(n, max_fds))` long (:638-650; `max_fds` read only
    ///    for more than 64 descriptors, [`fd_table_max_fds`]), and kept.
    /// 2. One maskless zero-timeout probe. Where the caller's
    ///    `/proc/<tid>/smaps` shows every set writable
    ///    (`signal::user_copy_out_faults`), it runs on the guest's own sets
    ///    (round-3 ruling 3), so the kernel's in-place value-result writes are
    ///    Linux's own: an error (`EBADF` for a descriptor not open,
    ///    `max_select_fd`, :417-445) is returned with the sets untouched, a
    ///    count with the sets written (`set_fd_set`, :689-692), and nothing
    ///    ready leaves them zeroed. A zero timeout probes with the guest's own
    ///    zero timespec, so nothing is staged and a tight stack or overlapping
    ///    buffers change nothing; a nonzero one stages only a private zero
    ///    timeout clear of every set (`stage_private_cells`: the stack
    ///    scratch, else a red-zone window), and with no room left for it the
    ///    call is refused by name before the probe. A set the kernel could not
    ///    write back (read-only, or unknown) is probed on copies in the stack
    ///    scratch instead, and Detcore copies the results back where Linux
    ///    copies out, so its EFAULT never comes before a signal that ends the
    ///    wait; with no scratch for the copies the call is refused by name,
    ///    never with an EFAULT of Hermit's own.
    /// 3. Nothing ready: the check under the temporary mask
    ///    (`helpers::temporary_mask_check`, which removes the ignored signals
    ///    Linux would not hold, D37). A default-fatal signal the temporary
    ///    mask lets in returns `-ERESTARTNOHAND` with no copy-out (:682-687),
    ///    so the sets the probe zeroed are put back, and the D40 flag is set.
    /// 4. Otherwise a zero timeout returns 0, with the sets zeroed as Linux
    ///    copies them out. A nonzero timeout would sleep under the temporary
    ///    mask, which the gate cannot run: `ENOSYS`, by name
    ///    (`masked_pselect6_nfds_refused_warning`), with the sets put back, a
    ///    parity loss where the base matched.
    ///
    /// The probe never carries the mask (IR-9). A probe the kernel ended with
    /// a restart code (a pending signal the kernel's own mask lets in, which
    /// under the runtime mask only SIGKILL can be) wrote nothing, and counts
    /// as nothing ready with empty sets. With more than 1024 descriptors to
    /// copy, the call is refused by name with `ENOSYS` once the sets have
    /// been read (D54), so their EFAULT comes first; an EBADF, which only a
    /// probe finds, is not reported then. No RPC runs before the probe and the
    /// check but the timerfd publication, whose hand-off the at-entry set
    /// covers ([`entry_before_publication`]); the clock is observed only for
    /// the time left that a return writes back (round-3 ruling 1). A count or
    /// an error that a nonzero timeout's probe finds is returned after one
    /// yielding empty request ([`yield_after_in_turn_result`], round-3 ruling
    /// 2).
    pub(super) async fn gated_staged_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        mask: KernelSigset,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        let zero = timeout == Some(Duration::ZERO);
        let refuse = |guest: &G, why: &str| {
            tracing::warn!(
                "{}",
                crate::refused_signal_controls::masked_pselect6_nfds_refused_warning(
                    guest.thread_state().dettid.as_raw(),
                    call.nfds(),
                    why,
                )
            );
            Err(Errno::ENOSYS.into())
        };
        let nfds = call.nfds();
        let counted = if nfds > PSELECT6_INTERNAL_MAX_NFDS {
            let max_fds = fd_table_max_fds(guest.tid())?;
            nfds.min(i32::try_from(max_fds).unwrap_or(i32::MAX))
        } else {
            nfds
        };
        let len = pselect6_fd_set_len(counted)?;
        // The sets are read first, into heap buffers, so their EFAULT comes
        // before any refusal (`get_fd_set`, fs/select.c:670-673).
        let originals = match read_gated_fd_sets(guest, "pselect6", call, len).await {
            Ok((readfds, writefds, exceptfds)) => [readfds, writefds, exceptfds],
            Err(error) => {
                self.write_staged_pselect6_remaining(guest, call, timeout)
                    .await?;
                return Err(error);
            }
        };
        if len > std::mem::size_of::<libc::fd_set>() {
            return refuse(
                guest,
                "more than 1024 descriptors to copy (an EBADF among them, which only a poll \
                 would find, is not reported)",
            );
        }
        let entry = entry_before_publication(guest)?;
        // A virtual timerfd among the sets is readable exactly when its
        // deadline has passed (`crate::syscalls::timerfd`).
        crate::syscalls::timerfd::publish_expired(guest).await;
        // In place where the kernel can write every set back, as the caller's
        // `/proc/<tid>/smaps` shows; otherwise on copies, so that a set Linux
        // cannot write back gives EFAULT only where Linux copies out (a
        // count, or nothing ready with no signal pending, :682-692), never
        // for a signal that ends the wait first. smaps' write permission can
        // miss a fault (a protection key denying writes, userfaultfd write
        // protection, a shared mapping beyond the file's end; see
        // `signal::user_copy_out_faults`): a named residual there.
        let in_place = [call.readfds(), call.writefds(), call.exceptfds()]
            .into_iter()
            .flatten()
            .all(|set| {
                crate::syscalls::signal::user_copy_out_faults(
                    guest.tid().as_raw(),
                    set.as_raw(),
                    len,
                ) == Some(false)
            });
        let live: Vec<std::ops::Range<usize>> = [call.readfds(), call.writefds(), call.exceptfds()]
            .into_iter()
            .flatten()
            .map(|set| set.as_raw()..set.as_raw() + len)
            .collect();
        let put_back = |guest: &mut G, sets: &[Option<Vec<u8>>; 3]| -> Result<(), Error> {
            write_pselect6_fd_set(guest, call.readfds(), &sets[0])?;
            write_pselect6_fd_set(guest, call.writefds(), &sets[1])?;
            write_pselect6_fd_set(guest, call.exceptfds(), &sets[2])?;
            Ok(())
        };
        let empty = |sets: &[Option<Vec<u8>>; 3]| {
            [0, 1, 2].map(|index| sets[index].as_ref().map(|set| vec![0; set.len()]))
        };
        if in_place {
            let staged = if zero {
                None
            } else {
                match stage_private_cells(guest, PrivateCells::ZeroTimeout, &live).await? {
                    Some(staged) => Some(staged),
                    None => {
                        return refuse(
                            guest,
                            "no room for a private zero timeout (a stack with no scratch below \
                             its red zone, whose red zone the sets fill)",
                        );
                    }
                }
            };
            let probe = match &staged {
                Some(staged) => call.with_timeout(AddrMut::from_raw(staged.at)),
                None => call,
            }
            .with_sigmask(None);
            let result = guest.inject(probe).await;
            if let Some(staged) = staged {
                staged.release(guest);
            }
            match result {
                Ok(0) => {}
                Err(errno) if crate::wait_signal_end::is_restart_code(errno) => {
                    put_back(guest, &empty(&originals))?;
                }
                ready_or_error => {
                    self.write_staged_pselect6_remaining(guest, call, timeout)
                        .await?;
                    if !zero {
                        yield_after_in_turn_result(guest).await;
                    }
                    return ready_or_error.map_err(Into::into);
                }
            }
            let check = temporary_mask_check(guest, mask, Some(entry), false).await?;
            if let Some(errno) = check_result(check, Errno::ERESTARTNOHAND) {
                put_back(guest, &originals)?;
                self.write_staged_pselect6_remaining(guest, call, timeout)
                    .await?;
                return Err(errno.into());
            }
            if zero {
                return Ok(0);
            }
            put_back(guest, &originals)?;
            return refuse(guest, "a nonzero timeout and nothing ready");
        }
        // On copies: the probe polls staged copies of the sets, and Detcore
        // copies the results back where Linux would.
        let mut stack = guest.stack().await;
        let readfds = call.readfds().map(|_| stack.reserve::<libc::fd_set>());
        let writefds = call.writefds().map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = call.exceptfds().map(|_| stack.reserve::<libc::fd_set>());
        let probe_timeout = stack.reserve::<Timespec>();
        let Ok(_guard) = stack.commit() else {
            // Hermit's own limit, never an EFAULT of the guest's call (round-3
            // ruling 3): no scratch for copies of sets the kernel could not
            // write back in place.
            return refuse(
                guest,
                "no scratch memory for copies of descriptor sets the kernel cannot write back",
            );
        };
        let probe = call
            .with_readfds(readfds)
            .with_writefds(writefds)
            .with_exceptfds(exceptfds)
            .with_timeout(Some(probe_timeout))
            .with_sigmask(None);
        guest.memory().write_value(
            probe_timeout,
            &Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        )?;
        write_pselect6_fd_set(guest, probe.readfds(), &originals[0])?;
        write_pselect6_fd_set(guest, probe.writefds(), &originals[1])?;
        write_pselect6_fd_set(guest, probe.exceptfds(), &originals[2])?;
        let copy_back = |guest: &mut G| -> Result<(), Error> {
            for (source, destination) in [
                (probe.readfds(), call.readfds()),
                (probe.writefds(), call.writefds()),
                (probe.exceptfds(), call.exceptfds()),
            ] {
                copy_pselect6_fd_set(guest, source, destination, len)?;
            }
            Ok(())
        };
        match guest.inject(probe).await {
            Ok(0) => {}
            Ok(count) => {
                let copied = copy_back(guest);
                self.write_staged_pselect6_remaining(guest, call, timeout)
                    .await?;
                if !zero {
                    yield_after_in_turn_result(guest).await;
                }
                copied?;
                return Ok(count);
            }
            Err(errno) if crate::wait_signal_end::is_restart_code(errno) => {
                let empty = Some(vec![0; len]);
                write_pselect6_fd_set(guest, probe.readfds(), &empty)?;
                write_pselect6_fd_set(guest, probe.writefds(), &empty)?;
                write_pselect6_fd_set(guest, probe.exceptfds(), &empty)?;
            }
            Err(errno) => {
                self.write_staged_pselect6_remaining(guest, call, timeout)
                    .await?;
                if !zero {
                    yield_after_in_turn_result(guest).await;
                }
                return Err(errno.into());
            }
        }
        let check = temporary_mask_check(guest, mask, Some(entry), false).await?;
        if let Some(errno) = check_result(check, Errno::ERESTARTNOHAND) {
            self.write_staged_pselect6_remaining(guest, call, timeout)
                .await?;
            return Err(errno.into());
        }
        if zero {
            copy_back(guest)?;
            return Ok(0);
        }
        drop(_guard);
        refuse(guest, "a nonzero timeout and nothing ready")
    }

    /// The time left a staged `pselect6` writes back as it returns, measured
    /// from a deadline observed then ([`observe_deadline`]): Linux writes it
    /// for any result of a nonzero timeout (`poll_select_finish`,
    /// fs/select.c:295-301 at v6.12), and none for a zero one.
    async fn write_staged_pselect6_remaining<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        timeout: Option<Duration>,
    ) -> Result<(), Error> {
        if timeout == Some(Duration::ZERO) {
            return Ok(());
        }
        let deadline = observe_deadline(guest, timeout)
            .await
            .map(|(_, deadline)| deadline);
        self.write_pselect6_remaining(guest, call, deadline).await
    }

    /// A masked `ppoll` under the gate, threads sequentialized, in run mode
    /// (D50 reader (5), replacing the `ENOSYS` of a masked `ppoll` that would
    /// wait), in Linux's order (`ppoll`, fs/select.c:1101-1124 at v6.12;
    /// `do_sys_poll`, :973-1044).
    ///
    /// 0. D41's check under the guest's own mask, as for `pselect6`.
    /// 1. The timeout (`EFAULT`, `EINVAL`, :1108-1114), then the mask's size
    ///    and the mask (`set_user_sigmask`, :1117: `EINVAL`, `EFAULT`).
    /// 2. Each probe is the call itself on the guest's pollfd array with a
    ///    private zero timeout and no mask (IR-9), so the kernel makes the
    ///    `nfds > RLIMIT_NOFILE` check (`EINVAL`, :987-988), reads the array
    ///    (`EFAULT`, :997-999), polls it, and always copies `revents` back
    ///    (:1018-1030), a fault there giving `EFAULT`. A count or an error is
    ///    returned.
    /// 3. Nothing ready: the check under the temporary mask. A default-fatal
    ///    signal it lets in returns `-ERESTARTNOHAND`, with `revents` already
    ///    copied out (`do_poll` returns it, :936-939, and `do_sys_poll` copies
    ///    `revents` out before returning it) and
    ///    the D40 flag; a read-only array has failed the probe with `EFAULT`
    ///    first, and no death follows (the mask is put back,
    ///    `poll_select_finish`, :301). Ignored signals the temporary mask lets
    ///    in are removed (D37), so the call waits on (Linux's transparent
    ///    restart).
    /// 4. A zero timeout returns 0. Otherwise the wait parks as internal
    ///    polling that publishes the temporary mask, probe first on each
    ///    turn (the first one, and its check, in the call's own turn), until
    ///    a descriptor is ready, an error, the check ends it, or the deadline
    ///    passes (0); the time left is written back. A ready count or an
    ///    error from the first, in-turn probe of a nonzero timeout is
    ///    returned after one yielding empty request
    ///    ([`yield_after_in_turn_result`]).
    ///
    /// On a stack with no room for the private zero timeout, the kernel's
    /// answer comes first, from a stage-free `poll` probe with an immediate
    /// zero timeout (round-2 ruling 2): its count or error is returned; only
    /// nothing ready is refused, by name, as before the gate.
    pub(super) async fn gated_masked_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
    ) -> Result<i64, Error> {
        if let Some(refused) = kernel_run_entry_refusal(guest)? {
            return Err(refused);
        }
        let timeout = match call.timeout() {
            Some(timeout) => Some(ppoll_timeout_duration(
                read_argument::<Timespec, _, _>(guest, "ppoll", "the timeout", timeout.as_raw())
                    .await?,
            )?),
            None => None,
        };
        if call.sigsetsize() != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let mask_address = call.sigmask().map_or(0, |mask| mask.as_raw());
        let mask = read_temporary_mask(guest, "ppoll", mask_address, true).await?;
        let zero = timeout == Some(Duration::ZERO);
        // The clock is observed only after the first probe and its check
        // (`observe_deadline`, round-3 ruling 1).
        let mut timing: Option<(LogicalTime, LogicalTime)> = None;
        let live: Vec<std::ops::Range<usize>> = ppoll_fds_range(call).into_iter().collect();
        let Some(staged) = stage_private_cells(guest, PrivateCells::ZeroTimeout, &live).await?
        else {
            // No room for the private zero timeout (round-2 ruling 2): the
            // kernel's answer comes first, from a stage-free probe, `poll`
            // with an immediate zero timeout and no mask on the guest's own
            // array, which makes `do_sys_poll`'s checks (fs/select.c:973-1044
            // at v6.12: `nfds > RLIMIT_NOFILE` EINVAL, the copy-in EFAULT,
            // the poll, `revents` always copied back) as the ppoll would.
            // A count or an error is returned, as the ppoll returns it.
            // Nothing ready (or a restart code, which only SIGKILL can give
            // under the runtime mask) would go on to the check and the wait,
            // which need the private cell: the call fails closed then, as a
            // masked ppoll that would block did before the gate.
            // This probe cannot publish expired virtual timerfds first, as
            // that needs scratch memory too, so while a probe may need that
            // publication (an armed virtual timerfd the array may name) a
            // count or 0 could miss a timerfd Linux reports ready: only an
            // error, which no readiness changes, is returned then, and
            // anything else keeps the refusal (round-3 ruling 5).
            let poll = syscalls::Poll::new()
                .with_fds(call.fds().map(|fds| fds.cast()))
                .with_nfds(call.nfds())
                .with_timeout(0);
            let unpublished = crate::syscalls::timerfd::may_publish(guest, Syscall::Ppoll(call));
            let result = match guest.inject(poll).await {
                Ok(0) => None,
                Err(errno) if crate::wait_signal_end::is_restart_code(errno) => None,
                Ok(_) if unpublished => None,
                ready_or_error => Some(ready_or_error.map_err(Error::from)),
            };
            let Some(result) = result else {
                return match self.refuse_unstageable_ppoll_copy(guest).await {
                    Some(refused) => refused,
                    None => Err(Errno::ENOSYS.into()),
                };
            };
            // The time left is written before the yield (round-3 ruling 6).
            if let (Some(timeout_address), Some(timeout), false) = (call.timeout(), timeout, zero) {
                let started_at = thread_observe_time(guest).await;
                self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                    .await?;
            }
            if !zero {
                yield_after_in_turn_result(guest).await;
            }
            return result;
        };
        let probe = call
            .with_timeout(AddrMut::from_raw(staged.at))
            .with_sigmask(None);
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("ppoll");
        rsrc.set_signal_interrupt_errno(Errno::EINTR);
        rsrc.blocked_signal_mask = Some(mask);
        let mut entry = Some(entry_before_publication(guest)?);
        let mut first = true;
        // Whether the time left was written before the ruling-5 yield.
        let mut written = false;
        let result = loop {
            // The first probe and check run in the call's own turn.
            let in_turn = first;
            if !in_turn {
                resource_request(guest, rsrc.clone()).await;
            }
            first = false;
            let next_timerfd =
                crate::syscalls::timerfd::publish_expired_in(guest, staged.timerfd_scratch).await;
            let deadline = timing.map(|(_, deadline)| deadline);
            rsrc.poll_deadline =
                crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
            match guest.inject_with_retry(probe).await {
                Ok(0) => {}
                Err(errno) if crate::wait_signal_end::is_restart_code(errno) => {}
                ready_or_error => {
                    if in_turn && !zero {
                        // The time left is written before the yield (round-3
                        // ruling 6).
                        if let (Some(timeout_address), Some(timeout)) = (call.timeout(), timeout) {
                            let started_at = thread_observe_time(guest).await;
                            self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                                .await?;
                            written = true;
                        }
                        yield_after_in_turn_result(guest).await;
                    }
                    break ready_or_error.map_err(Into::into);
                }
            }
            match temporary_mask_check(guest, mask, entry.take(), false).await {
                Ok(check) => {
                    if let Some(errno) = check_result(check, Errno::ERESTARTNOHAND) {
                        break Err(errno.into());
                    }
                }
                Err(error) => break Err(error),
            }
            if zero {
                break Ok(0);
            }
            if in_turn {
                timing = observe_deadline(guest, timeout).await;
                rsrc.poll_deadline = crate::syscalls::timerfd::poll_deadline_with(
                    guest,
                    timing.map(|(_, deadline)| deadline),
                    next_timerfd,
                );
            }
            rsrc.poll_attempt += 1;
            if let Some((_, deadline)) = timing
                && thread_observe_time(guest).await >= deadline
            {
                break Ok(0);
            }
            record_retry_event(guest, probe).await;
        };
        staged.release(guest);
        if let (Some(timeout_address), Some(timeout), false, false) =
            (call.timeout(), timeout, zero, written)
        {
            let started_at = match timing {
                Some((started_at, _)) => started_at,
                None => thread_observe_time(guest).await,
            };
            self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                .await?;
        }
        result
    }

    /// A masked `epoll_pwait` under the gate (D39): D41's check, then the
    /// mask's size (`EINVAL`) and the mask (`EFAULT`), read in Linux's order
    /// before the epoll descriptor (`do_epoll_pwait`,
    /// fs/eventpoll.c:2479-2498 at v6.12), then [`Self::gated_masked_epoll`]
    /// for a nonzero timeout, or [`Self::gated_zero_timeout_epoll`].
    pub(super) async fn gated_masked_epoll_pwait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        if let Some(refused) = kernel_run_entry_refusal(guest)? {
            return Err(refused);
        }
        if call.sigsetsize() != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let address = call.sigmask().map_or(0, |mask| mask.as_raw());
        let mask = read_temporary_mask(guest, "epoll_pwait", address, true).await?;
        let probe = call.with_sigmask(None).with_timeout(0);
        if call.timeout() == 0 {
            return self.gated_zero_timeout_epoll(guest, probe).await;
        }
        // A negative timeout waits forever (`ep_poll`'s `timeout` NULL).
        let timeout = u64::try_from(call.timeout())
            .ok()
            .map(Duration::from_millis);
        self.gated_masked_epoll(guest, probe, mask, timeout, "epoll_pwait")
            .await
    }

    /// A masked `epoll_pwait` or `epoll_pwait2` with a zero timeout under the
    /// gate, once its mask has been read (round-1 ruling 3, IR-9 literal):
    /// one `probe` (an `epoll_pwait` with a zero timeout and no mask) in the
    /// call's turn, after the empty request and the timerfd publication the
    /// raw zero-timeout path makes. Linux's answer to the masked call is the
    /// same: `ep_poll` returns ready events, and with a zero timeout 0 before
    /// it tests `signal_pending` (fs/eventpoll.c:2010-2011 at v6.12), and the
    /// thread's own mask goes back since the result is not `-EINTR` (:2495),
    /// so the temporary mask never decides anything, and nothing is removed:
    /// a ready or zero return keeps every pending signal (IR-7 row (A)).
    async fn gated_zero_timeout_epoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::EpollPwait,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await; // empty request
        crate::syscalls::timerfd::publish_expired(guest).await;
        Ok(self.record_or_replay(guest, probe).await?)
    }

    /// A masked `epoll_pwait2` under the gate (D39): D41's check, the timeout
    /// (`EFAULT`, `EINVAL`), then the mask's size and the mask, as Linux
    /// reads them (fs/eventpoll.c:2511-2549 at v6.12: the timeout at
    /// :2518-2522, the mask in `set_user_sigmask` after it), then
    /// [`Self::gated_masked_epoll`] for a nonzero timeout or
    /// [`Self::gated_zero_timeout_epoll`]. `first`: the mask's read is the
    /// call's first injection.
    pub(super) async fn gated_masked_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
        first: bool,
    ) -> Result<i64, Error> {
        if let Some(refused) = kernel_run_entry_refusal(guest)? {
            return Err(refused);
        }
        let timeout = match args.arg3 {
            0 => None,
            address => Some(ppoll_timeout_duration(
                read_argument::<Timespec, _, _>(guest, "epoll_pwait2", "the timeout", address)
                    .await?,
            )?),
        };
        if args.arg5 != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let mask = read_temporary_mask(guest, "epoll_pwait2", args.arg4, first).await?;
        let probe = syscalls::EpollPwait::new()
            .with_epfd(args.arg0 as i32)
            .with_events(AddrMut::from_raw(args.arg1))
            .with_maxevents(args.arg2 as i32)
            .with_timeout(0)
            .with_sigmask(None)
            .with_sigsetsize(args.arg5);
        if timeout == Some(Duration::ZERO) {
            return self.gated_zero_timeout_epoll(guest, probe).await;
        }
        self.gated_masked_epoll(guest, probe, mask, timeout, "epoll_pwait2")
            .await
    }

    /// The wait of a masked `epoll_pwait` or `epoll_pwait2` with a nonzero
    /// timeout under the gate, after its mask was read (`mask`, as Linux
    /// installs it): internal polling that publishes the temporary mask, with
    /// `probe` (an `epoll_pwait` with a zero timeout and no mask, IR-9) first
    /// on each turn (the first probe and its check in the call's own turn,
    /// so the first check sees exactly the instances pending at entry), in
    /// `ep_poll`'s order
    /// (fs/eventpoll.c:1966-2025 at v6.12): ready events are returned (with
    /// any signal kept pending, IR-7 row (A)), and an error (`EBADF`,
    /// `EINVAL` from `do_epoll_wait`); then, with nothing ready, the check
    /// under the temporary mask with D37's exception: a default-fatal signal
    /// it lets in, or at the first check an ignored signal pending at entry
    /// that the own mask blocked and the temporary mask lets in (removed),
    /// returns `-EINTR` (:2017-2018); an ignored signal sent during the park
    /// outside the temporary mask was discarded by Linux at send, so it is
    /// removed and the wait goes on (IR-7 row (B)). The deadline returns 0.
    /// Events or an error from the first, in-turn probe are returned after
    /// one yielding empty request ([`yield_after_in_turn_result`]). The
    /// deadline (`timeout` from now, `None` for none) is observed only after
    /// the first probe and check ([`observe_deadline`], round-3 ruling 1).
    pub(super) async fn gated_masked_epoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::EpollPwait,
        mask: KernelSigset,
        timeout: Option<Duration>,
        fyi: &'static str,
    ) -> Result<i64, Error> {
        let mut entry = Some(entry_before_publication(guest)?);
        let mut first = true;
        let mut deadline: Option<LogicalTime> = None;
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi(fyi);
        rsrc.set_signal_interrupt_errno(Errno::EINTR);
        rsrc.blocked_signal_mask = Some(mask);
        loop {
            // The first probe and check run in the call's own turn.
            let in_turn = first;
            if !in_turn {
                resource_request(guest, rsrc.clone()).await;
            }
            first = false;
            let next_timerfd = crate::syscalls::timerfd::publish_expired(guest).await;
            rsrc.poll_deadline =
                crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
            let result = guest.inject_with_retry(probe).await;
            if !probe.syscall_would_have_blocked(result) {
                if in_turn {
                    yield_after_in_turn_result(guest).await;
                }
                return result.map_err(Into::into);
            }
            let check = temporary_mask_check(guest, mask, entry.take(), true).await?;
            if let Some(errno) = check_result(check, Errno::EINTR) {
                return Err(errno.into());
            }
            if in_turn {
                deadline = observe_deadline(guest, timeout)
                    .await
                    .map(|(_, deadline)| deadline);
                rsrc.poll_deadline =
                    crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
            }
            rsrc.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                return Ok(0);
            }
            record_retry_event(guest, probe).await;
        }
    }
}
