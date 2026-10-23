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
//! - masked `epoll_pwait` and `epoll_pwait2` with a nonzero timeout:
//!   [`Detcore::gated_masked_epoll`], with D37's `-EINTR` exception (D39,
//!   IR-3). A zero timeout keeps the raw in-turn call, which never ends for a
//!   signal (`ep_poll` returns 0 before it tests `signal_pending`,
//!   fs/eventpoll.c:2010-2011 at v6.12) and so always puts the thread's own
//!   mask back (:2495).

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
use super::ppoll_start;
use super::ppoll_timeout_duration;
use super::pselect6_fd_set_len;
use super::read_ppoll_mask;
use super::read_pselect6_fd_set;
use super::sanitize_ppoll_signal_mask;
use super::stage_private_cells;
use super::validate_pselect6_wrapper;
use super::write_pselect6_fd_set;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::syscalls::helpers::TemporaryMaskCheck;
use crate::syscalls::helpers::kernel_run_entry_refusal;
use crate::syscalls::helpers::millis_duration_to_absolute_timeout;
use crate::syscalls::helpers::poll_deadline_for;
use crate::syscalls::helpers::record_retry_event;
use crate::syscalls::helpers::temporary_mask_check;
use crate::syscalls::helpers::temporary_mask_entry;
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
fn fd_table_max_fds(tid: reverie::Pid) -> Result<usize, Errno> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status"))
        .map_err(|error| error.raw_os_error().map_or(Errno::EIO, Errno::new))?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("FDSize:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or(Errno::EIO)
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
    ///    and the mask (`set_user_sigmask`, :756-758: `EINVAL`, `EFAULT`), as
    ///    before the gate.
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
        if let Some(argument) = call.sigmask() {
            validate_pselect6_wrapper::<_, Self>(guest, argument).await?;
        }
        let timeout = match call.timeout() {
            Some(timeout) => Some(ppoll_timeout_duration(guest.memory().read_value(timeout)?)?),
            None => None,
        };
        let mask = match call.sigmask() {
            Some(argument) => {
                let sigmask: usize = guest.memory().read_value(argument.cast::<usize>())?;
                let second = argument
                    .as_raw()
                    .checked_add(std::mem::size_of::<usize>())
                    .and_then(Addr::<usize>::from_raw)
                    .ok_or(Errno::EFAULT)?;
                let sigsetsize: usize = guest.memory().read_value(second)?;
                if sigmask == 0 {
                    None
                } else {
                    if sigsetsize != KERNEL_SIGSET_SIZE {
                        return Err(Errno::EINVAL.into());
                    }
                    let address = Addr::<libc::sigset_t>::from_raw(sigmask).ok_or(Errno::EFAULT)?;
                    let mask = read_kernel_sigset(guest, address).await?;
                    crate::syscalls::signal_shadow::note_wait_mask(guest, mask);
                    Some(sanitize_ppoll_signal_mask(mask))
                }
            }
            None => None,
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
    /// or with more than 64 descriptors and a nonzero timeout (D39), in
    /// Linux's order (`core_sys_select`, fs/select.c:620-691 at v6.12), on
    /// staged copies, never the guest's own sets:
    ///
    /// 1. The sets are read (`get_fd_set`, :670-673: `EFAULT`), each
    ///    `FDS_BYTES(min(n, max_fds))` long (:638-650; `max_fds` read only
    ///    for more than 64 descriptors, [`fd_table_max_fds`]), into staged
    ///    copies.
    /// 2. One zero-timeout `pselect6` with no mask polls the copies
    ///    (`do_select`): an error (`EBADF` for a descriptor not open,
    ///    `max_select_fd`, :417-445) is returned, and a count with the results
    ///    copied back in order (`set_fd_set`, :689-692: `EFAULT`).
    /// 3. Nothing ready: the check under the temporary mask
    ///    (`helpers::temporary_mask_check`, which removes the ignored signals
    ///    Linux would not hold, D37). A default-fatal signal the temporary
    ///    mask lets in returns `-ERESTARTNOHAND` with no copy-out (:682-687),
    ///    and the D40 flag, for the death path.
    /// 4. Otherwise a zero timeout copies the (empty) results back and
    ///    returns 0. A nonzero timeout would sleep under the temporary mask,
    ///    which the gate cannot run: `ENOSYS`, by name
    ///    (`masked_pselect6_nfds_refused_warning`, D39), a parity loss where
    ///    the base matched.
    ///
    /// The probe never carries the mask (IR-9). A probe that the kernel
    /// ended with a restart code (a pending signal the kernel's own mask
    /// lets in, which under the runtime mask only SIGKILL can be) copied
    /// nothing out, and counts as nothing ready with empty results. With more
    /// than 1024 descriptors to copy (one `fd_set`), the call is refused by
    /// name with `ENOSYS` after its arguments' checks.
    pub(super) async fn gated_staged_pselect6<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pselect6,
        mask: KernelSigset,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        let zero = timeout == Some(Duration::ZERO);
        // A zero-timeout call holds its turn throughout, so everything
        // pending at its check was pending at entry; a nonzero one observes
        // the clock first, which may let other threads run.
        let entry = if zero {
            !0
        } else {
            temporary_mask_entry(guest)?
        };
        let deadline = match timeout {
            Some(timeout) if !zero => Some(thread_observe_time(guest).await + timeout),
            _ => None,
        };
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
        if len > std::mem::size_of::<libc::fd_set>() {
            return refuse(guest, "more than 1024 descriptors to copy");
        }
        let mut originals = Vec::with_capacity(3);
        for set in [call.readfds(), call.writefds(), call.exceptfds()] {
            match read_pselect6_fd_set(guest, set, len) {
                Ok(bytes) => originals.push(bytes),
                Err(error) => {
                    self.write_pselect6_remaining(guest, call, deadline).await?;
                    return Err(error);
                }
            }
        }
        let mut stack = guest.stack().await;
        let readfds = call.readfds().map(|_| stack.reserve::<libc::fd_set>());
        let writefds = call.writefds().map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = call.exceptfds().map(|_| stack.reserve::<libc::fd_set>());
        let probe_timeout = stack.reserve::<Timespec>();
        let timerfd_scratch = crate::syscalls::timerfd::reserve_scratch(&mut stack);
        let _guard = stack.commit()?;
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
        // A virtual timerfd among the sets is readable exactly when its
        // deadline has passed (`crate::syscalls::timerfd`).
        crate::syscalls::timerfd::publish_expired_in(guest, Some(timerfd_scratch)).await;
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
                self.write_pselect6_remaining(guest, call, deadline).await?;
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
                self.write_pselect6_remaining(guest, call, deadline).await?;
                return Err(errno.into());
            }
        }
        let check = temporary_mask_check(guest, mask, Some(entry), false).await?;
        if let Some(errno) = check_result(check, Errno::ERESTARTNOHAND) {
            self.write_pselect6_remaining(guest, call, deadline).await?;
            return Err(errno.into());
        }
        if zero {
            copy_back(guest)?;
            return Ok(0);
        }
        drop(_guard);
        refuse(guest, "a nonzero timeout and nothing ready")
    }

    /// A masked `ppoll` under the gate, threads sequentialized, in run mode
    /// (D50 reader (5), replacing the `ENOSYS` of a masked `ppoll` that would
    /// wait), in Linux's order (`ppoll`, fs/select.c:1101-1124 at v6.12;
    /// `do_sys_poll`, :973-1044). The timeout was read before (`EFAULT`,
    /// `EINVAL`).
    ///
    /// 0. D41's check under the guest's own mask, as for `pselect6`.
    /// 1. The mask's size and the mask (`set_user_sigmask`: `EINVAL`,
    ///    `EFAULT`).
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
    ///    turn, until a descriptor is ready, an error, the check ends it, or
    ///    the deadline passes (0); the time left is written back.
    pub(super) async fn gated_masked_ppoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ppoll,
        timeout: Option<Duration>,
    ) -> Result<i64, Error> {
        if let Some(refused) = kernel_run_entry_refusal(guest)? {
            return Err(refused);
        }
        let Some(mask) = read_ppoll_mask(guest, call, true).await? else {
            unreachable!("a masked ppoll names its mask");
        };
        let mask = sanitize_ppoll_signal_mask(mask);
        let zero = timeout == Some(Duration::ZERO);
        let mut entry = Some(if zero {
            !0
        } else {
            temporary_mask_entry(guest)?
        });
        let (started_at, deadline) = if zero {
            (None, None)
        } else {
            ppoll_start(guest, timeout).await
        };
        let live: Vec<std::ops::Range<usize>> = ppoll_fds_range(call).into_iter().collect();
        let Some(staged) = stage_private_cells(guest, PrivateCells::ZeroTimeout, &live).await?
        else {
            // No room for the private zero timeout: the guest's own mask
            // could not be kept off the probe, so the call fails closed, as
            // a masked ppoll that would block did before the gate.
            return match self.refuse_unstageable_ppoll_copy(guest).await {
                Some(refused) => refused,
                None => Err(Errno::ENOSYS.into()),
            };
        };
        let probe = call
            .with_timeout(AddrMut::from_raw(staged.at))
            .with_sigmask(None);
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("ppoll");
        rsrc.poll_deadline = poll_deadline_for(guest, deadline);
        rsrc.set_signal_interrupt_errno(Errno::EINTR);
        rsrc.blocked_signal_mask = Some(mask);
        let result = loop {
            if !zero {
                resource_request(guest, rsrc.clone()).await;
            }
            let next_timerfd =
                crate::syscalls::timerfd::publish_expired_in(guest, staged.timerfd_scratch).await;
            rsrc.poll_deadline =
                crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
            match guest.inject_with_retry(probe).await {
                Ok(0) => {}
                Err(errno) if crate::wait_signal_end::is_restart_code(errno) => {}
                ready_or_error => break ready_or_error.map_err(Into::into),
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
            rsrc.poll_attempt += 1;
            if let Some(deadline) = deadline
                && thread_observe_time(guest).await >= deadline
            {
                break Ok(0);
            }
            record_retry_event(guest, probe).await;
        };
        staged.release(guest);
        if let (Some(timeout_address), Some(timeout), Some(started_at)) =
            (call.timeout(), timeout, started_at)
        {
            self.write_ppoll_remaining(guest, timeout_address, timeout, started_at)
                .await?;
        }
        result
    }

    /// A masked `epoll_pwait` with a nonzero timeout under the gate (D39):
    /// the mask's size (`EINVAL`) and the mask (`EFAULT`), read in Linux's
    /// order before the epoll descriptor (`do_epoll_pwait`,
    /// fs/eventpoll.c:2479-2498 at v6.12), then [`Self::gated_masked_epoll`].
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
        let address = call.sigmask().ok_or(Errno::EFAULT)?;
        let mask = crate::syscalls::signal::restart_if_first_probe_not_run(
            read_kernel_sigset(guest, address.cast()).await,
        )?;
        crate::syscalls::signal_shadow::note_wait_mask(guest, mask);
        let entry = temporary_mask_entry(guest)?;
        let deadline = millis_duration_to_absolute_timeout(guest, call.timeout()).await;
        self.gated_masked_epoll(
            guest,
            call.with_sigmask(None).with_timeout(0),
            sanitize_ppoll_signal_mask(mask),
            entry,
            deadline,
            "epoll_pwait",
        )
        .await
    }

    /// A masked `epoll_pwait2` with a nonzero timeout under the gate (D39):
    /// the timeout (`EFAULT`, `EINVAL`), then the mask's size and the mask,
    /// as Linux reads them (fs/eventpoll.c:2511-2549 at v6.12: the timeout at
    /// :2518-2522, the mask in `set_user_sigmask` after it), then
    /// [`Self::gated_masked_epoll`]. `first`: the mask's read is the call's
    /// first injection.
    pub(super) async fn gated_masked_epoll_pwait2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
        first: bool,
    ) -> Result<i64, Error> {
        if let Some(refused) = kernel_run_entry_refusal(guest)? {
            return Err(refused);
        }
        let timeout = match Addr::<Timespec>::from_raw(args.arg3) {
            Some(timeout) => Some(ppoll_timeout_duration(guest.memory().read_value(timeout)?)?),
            None => None,
        };
        if args.arg5 != KERNEL_SIGSET_SIZE {
            return Err(Errno::EINVAL.into());
        }
        let address = Addr::<libc::sigset_t>::from_raw(args.arg4).ok_or(Errno::EFAULT)?;
        let read = read_kernel_sigset(guest, address).await;
        let mask = if first {
            crate::syscalls::signal::restart_if_first_probe_not_run(read)?
        } else {
            read?
        };
        crate::syscalls::signal_shadow::note_wait_mask(guest, mask);
        let entry = temporary_mask_entry(guest)?;
        let (_, deadline) = ppoll_start(guest, timeout).await;
        let probe = syscalls::EpollPwait::new()
            .with_epfd(args.arg0 as i32)
            .with_events(AddrMut::from_raw(args.arg1))
            .with_maxevents(args.arg2 as i32)
            .with_timeout(0)
            .with_sigmask(None)
            .with_sigsetsize(args.arg5);
        self.gated_masked_epoll(
            guest,
            probe,
            sanitize_ppoll_signal_mask(mask),
            entry,
            deadline,
            "epoll_pwait2",
        )
        .await
    }

    /// The wait of a masked `epoll_pwait` or `epoll_pwait2` with a nonzero
    /// timeout under the gate, after its mask was read (`mask`, sanitized)
    /// and `I` taken at entry (`entry`): internal polling that publishes the
    /// temporary mask, with `probe` (an `epoll_pwait` with a zero timeout and
    /// no mask, IR-9) first on each turn, in `ep_poll`'s order
    /// (fs/eventpoll.c:1966-2025 at v6.12): ready events are returned (with
    /// any signal kept pending, IR-7 row (A)), and an error (`EBADF`,
    /// `EINVAL` from `do_epoll_wait`); then, with nothing ready, the check
    /// under the temporary mask with D37's exception: a default-fatal signal
    /// it lets in, or at the first check an ignored signal pending at entry
    /// that the own mask blocked and the temporary mask lets in (removed),
    /// returns `-EINTR` (:2017-2018); an ignored signal sent during the park
    /// outside the temporary mask was discarded by Linux at send, so it is
    /// removed and the wait goes on (IR-7 row (B)). The deadline returns 0.
    pub(super) async fn gated_masked_epoll<G: Guest<Self>>(
        &self,
        guest: &mut G,
        probe: syscalls::EpollPwait,
        mask: KernelSigset,
        entry: KernelSigset,
        deadline: Option<LogicalTime>,
        fyi: &'static str,
    ) -> Result<i64, Error> {
        let mut entry = Some(entry);
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi(fyi);
        rsrc.poll_deadline = poll_deadline_for(guest, deadline);
        rsrc.set_signal_interrupt_errno(Errno::EINTR);
        rsrc.blocked_signal_mask = Some(mask);
        loop {
            resource_request(guest, rsrc.clone()).await;
            let next_timerfd = crate::syscalls::timerfd::publish_expired(guest).await;
            rsrc.poll_deadline =
                crate::syscalls::timerfd::poll_deadline_with(guest, deadline, next_timerfd);
            let result = guest.inject_with_retry(probe).await;
            if !probe.syscall_would_have_blocked(result) {
                return result.map_err(Into::into);
            }
            let check = temporary_mask_check(guest, mask, entry.take(), true).await?;
            if let Some(errno) = check_result(check, Errno::EINTR) {
                return Err(errno.into());
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
