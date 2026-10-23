/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1d: the temporary-mask forms and the ignored-signal rules under
//! the in-guest delivery gate (`Config::in_guest_signal_delivery`; design
//! D37, D39, D50, IR-7, IR-9, IR-22), on the waiting thread's own side, with
//! the parent module's harness: the gate forced on in the Detcore `Config`,
//! `/proc` reads scripted, the guest's own mask a chosen value, scheduler
//! RPCs answered and logged, and the injections a kernel would answer (the
//! mask and action probes, `rt_sigtimedwait`, the poll probes) answered by
//! the harness and recorded.

use reverie::syscalls::RtSigaction;
use reverie::syscalls::RtSigsuspend;

use super::*;

/// Answer this thread's `/proc` reads of `THREAD` with `states` in turn,
/// the last one for every read after.
fn script_seq(states: Vec<KernelSignalState>) -> signal_state_read_seam::Installed {
    let mut states = VecDeque::from(states);
    signal_state_read_seam::install(move |pid, tid| {
        if pid.as_raw() != THREAD.as_raw() || tid.as_raw() != THREAD.as_raw() {
            return None;
        }
        let state = if states.len() > 1 {
            states.pop_front()
        } else {
            states.front().copied()
        };
        state.map(Ok)
    })
}

/// A leaked eight-byte cell holding `mask`, where a call names its mask.
fn mask_cell(mask: u64) -> usize {
    Box::leak(Box::new(mask)) as *mut u64 as usize
}

fn suspend(mask: u64) -> RtSigsuspend {
    RtSigsuspend::new()
        .with_mask(Addr::from_raw(mask_cell(mask)))
        .with_sigsetsize(8)
}

/// The requests the parked `rt_sigsuspend` filed.
fn suspend_requests(guest: &TurnGuest) -> Vec<Resources> {
    guest
        .requests()
        .into_iter()
        .filter(|request| request.fyi == crate::resources::RT_SIGSUSPEND_FYI)
        .collect()
}

/// Design D39 and the 3c.1d gate "rt_sigsuspend's eligibility uses the
/// temporary mask": under the gate `rt_sigsuspend` is parked, never run by
/// the kernel, and ends exactly for a default-fatal signal its temporary mask
/// lets in.
///
/// - (a) SIGTERM pending, blocked by the guest's own mask and let in by the
///   temporary one: the wait ends at once, `-ERESTARTNOHAND` with the D40
///   flag `Fatal(SIGTERM)`, nothing filed; the restart checkpoint turns it
///   into `-EINTR` with the interrupted code.
/// - (b) SIGTERM let in by the own mask at entry: D41's check under the own
///   mask still runs first, and the call is not run (`ERESTARTNOINTR`).
/// - (c) SIGTERM blocked by both: the wait parks with a request tagged
///   `RT_SIGSUSPEND_FYI` that publishes the temporary mask, and after a wake
///   ends for SIGQUIT, which the temporary mask lets in.
///
/// Mutation "the own mask decides eligibility": (a) parks and (c) ends at
/// once for nothing it can see. Mutation "the request publishes nothing":
/// (c)'s publication assertion fails.
#[tokio::test]
async fn under_the_gate_rt_sigsuspend_parks_and_decides_by_its_temporary_mask() {
    let term = bit(libc::SIGTERM);
    let quit = bit(libc::SIGQUIT);
    let call = Syscall::from(suspend(0));

    // (a)
    let _seam = script(proc_state(term, 0, 0));
    let mut guest = turn_guest(true, Some(term));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(0)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "(a): {result:?}"
    );
    assert_eq!(
        guest.thread.wait_signal_end,
        Some(WaitSignalEnd::Fatal(libc::SIGTERM))
    );
    assert!(suspend_requests(&guest).is_empty(), "(a): nothing filed");
    let (res, code) = tool.wait_end_checkpoint(&mut guest, &call, result).await;
    assert!(
        matches!(res, Err(Error::Errno(Errno::EINTR))),
        "(a): {res:?}"
    );
    assert_eq!(code, Some(Errno::ERESTARTNOHAND));
    drop(_seam);

    // (b)
    let _seam = script(proc_state(term, 0, 0));
    let mut guest = turn_guest(true, Some(0));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(term)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
        "(b): {result:?}"
    );
    assert!(guest.injected.is_empty(), "(b): not even the mask is read");
    drop(_seam);

    // (c): reads are D41's, the first check's, and the woken check's.
    let _seam = script_seq(vec![
        proc_state(term, 0, 0),
        proc_state(term, 0, 0),
        proc_state(term | quit, 0, 0),
    ]);
    let mut guest = turn_guest(true, Some(term));
    guest
        .answers
        .lock()
        .unwrap()
        .push_back(ResumeStatus::Signaled(None));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(term)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "(c): {result:?}"
    );
    assert_eq!(
        guest.thread.wait_signal_end,
        Some(WaitSignalEnd::Fatal(libc::SIGQUIT))
    );
    let filed = suspend_requests(&guest);
    assert_eq!(filed.len(), 1, "(c): parked once");
    assert_eq!(filed[0].blocked_signal_mask, Some(term));
    assert_eq!(
        filed[0].guest_signal_mask,
        Some(term),
        "(c): the request publishes the temporary mask"
    );
    assert!(matches!(
        filed[0].resources.keys().next(),
        Some(ResourceID::SleepUntil(deadline)) if deadline.is_indefinite()
    ));
}

/// Round-1 review (lane ruling 1): the parked `rt_sigsuspend` decides by,
/// and publishes, the mask Linux installs, which never holds SIGKILL or
/// SIGSTOP (`set_current_blocked`, kernel/signal.c:3089-3092 at v6.12, from
/// `sigsuspend`, :4700). The common `sigfillset(); sigdelset(SIGTERM);
/// sigsuspend()` pattern publishes the full mask less SIGTERM, SIGKILL and
/// SIGSTOP, so a cross-process `/proc` status, which shows the published mask
/// as `SigBlk` under the gate, has bits 9 and 19 clear as on Linux. Mutation
/// "the guest's mask published as written": the published mask has them.
#[tokio::test]
async fn a_parked_rt_sigsuspend_publishes_the_mask_linux_installs() {
    let term = bit(libc::SIGTERM);
    let unblockable = bit(libc::SIGKILL) | bit(libc::SIGSTOP);
    let _seam = script_seq(vec![
        proc_state(0, 0, 0),
        proc_state(0, 0, 0),
        proc_state(term, 0, 0),
    ]);
    let mut guest = turn_guest(true, Some(term));
    guest
        .answers
        .lock()
        .unwrap()
        .push_back(ResumeStatus::Signaled(None));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(!term)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "{result:?}"
    );
    let filed = suspend_requests(&guest);
    assert_eq!(filed.len(), 1);
    let installed = !term & !unblockable;
    assert_eq!(filed[0].guest_signal_mask, Some(installed));
    assert_eq!(filed[0].blocked_signal_mask, Some(installed));
}

/// HERMIT_TEMPORARY_MASK_CAUGHT_END (round-1 review): a caught signal the
/// temporary mask lets in would run its handler under that mask (step 3c.w,
/// unreachable in 3c.1): the check files the named stopping refusal, the
/// call returns `EINTR`, nothing is recorded for the restart checkpoint,
/// nothing is removed (a default-ignored SIGURG the masks would otherwise
/// remove stays) and nothing parks. SIGUSR1 is caught (`SigCgt`), blocked by
/// the own mask, let in by the temporary one. Mutations "a caught end
/// answered `Waits`" (the SIGURG is removed and the call parks) and
/// "answered `Fatal`" (`-ERESTARTNOHAND` and a D40 flag).
#[tokio::test]
async fn under_the_gate_a_caught_end_of_a_temporary_mask_wait_is_a_named_refusal() {
    let usr1 = bit(libc::SIGUSR1);
    let urg = bit(libc::SIGURG);
    let _seam = script(proc_state(usr1 | urg, usr1, 0));
    let mut guest = turn_guest(true, Some(usr1 | urg));
    let tool = super::tool(&guest);
    let result = tool.handle_rt_sigsuspend(&mut guest, suspend(0)).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::EINTR))),
        "{result:?}"
    );
    let refusals = guest
        .rpcs()
        .iter()
        .filter(|rpc| {
            matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_TEMPORARY_MASK_CAUGHT_END"))
        })
        .count();
    assert_eq!(refusals, 1);
    assert_eq!(guest.thread.wait_signal_end, None);
    assert!(guest.sigtimedwait_sets.is_empty(), "nothing removed");
    assert!(suspend_requests(&guest).is_empty(), "nothing parked");
}

/// With the gate off `rt_sigsuspend` keeps its kernel-run path: no `/proc`
/// read, no parked request; it goes on to the `rt_sigpending` query of that
/// path (which this harness fails with ENOSYS). Mutation "the parked path
/// keyed on anything but the gate": it files a request or reads `/proc`.
#[tokio::test]
async fn with_the_gate_off_rt_sigsuspend_keeps_its_kernel_run_path() {
    let _seam = signal_state_read_seam::install(|_, _| panic!("a /proc read with the gate off"));
    let mut guest = turn_guest(false, Some(0));
    let tool = super::tool(&guest);
    let result = tool
        .handle_rt_sigsuspend(&mut guest, suspend(bit(libc::SIGTERM)))
        .await;
    assert!(result.is_err(), "{result:?}");
    assert!(guest.requests().is_empty());
    assert_eq!(guest.injected.first(), Some(&Sysno::rt_sigprocmask));
}

/// Design D37 rule (ii) with IR-7's per-instance masks, at a parked
/// `rt_sigsuspend`'s checks. Own mask: SIGURG, SIGWINCH. Temporary mask:
/// SIGWINCH, SIGCHLD.
///
/// - At the first check (everything pending was pending at entry, queued
///   under the own mask): SIGURG (own only: the transparent restart discards
///   it) and SIGCHLD (temporary only: discarded at send) are removed;
///   SIGWINCH (both) is kept.
/// - After a wake (sent during the park, queued under the temporary mask):
///   a `SIG_IGN` SIGUSR2 outside the temporary mask is removed; a SIGCHLD,
///   inside it, is kept; SIGWINCH, kept from entry, stays.
/// - The third check ends the wait for SIGTERM and removes nothing, though
///   a `SIG_IGN` SIGUSR2 outside the temporary mask is pending again (the
///   process dies at the return).
///
/// Mutations: "(i) using only `M_next`" (the first check keeps the SIGCHLD
/// Linux discarded at send); "`M_req` = the own mask at every check" (the
/// woken check removes the SIGCHLD Linux holds); "removal before the fatal
/// check" (the third check removes).
#[tokio::test]
async fn a_parked_rt_sigsuspend_removes_the_ignored_signals_linux_would_not_hold() {
    let urg = bit(libc::SIGURG);
    let winch = bit(libc::SIGWINCH);
    let chld = bit(libc::SIGCHLD);
    let usr2 = bit(libc::SIGUSR2);
    let term = bit(libc::SIGTERM);
    let own = urg | winch;
    let temporary = winch | chld;
    let _seam = script_seq(vec![
        proc_state(urg | winch | chld, 0, usr2),
        proc_state(urg | winch | chld, 0, usr2),
        proc_state(winch | chld | usr2, 0, usr2),
        proc_state(winch | chld | term | usr2, 0, usr2),
    ]);
    let mut guest = turn_guest(true, Some(own));
    for _ in 0..2 {
        guest
            .answers
            .lock()
            .unwrap()
            .push_back(ResumeStatus::Signaled(None));
    }
    let tool = super::tool(&guest);
    let result = tool
        .handle_rt_sigsuspend(&mut guest, suspend(temporary))
        .await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::ERESTARTNOHAND))),
        "{result:?}"
    );
    assert_eq!(guest.sigtimedwait_sets, [urg | chld, usr2]);
    assert_eq!(suspend_requests(&guest).len(), 2);
}

/// The removal dequeues until `EAGAIN`, one instance per call (a real-time
/// signal may have several), and an error other than `EAGAIN` is Hermit's
/// own failure (`Error::Tool`), never a guest errno. Mutations "a single
/// dequeue" (the second and third instances stay) and "the errno returned
/// as the guest's" (the EFAULT row).
#[tokio::test]
async fn the_removal_dequeues_every_instance_until_eagain() {
    let rt = 1_u64 << 33;
    let _seam = script(proc_state(rt, 0, rt));
    let mut guest = turn_guest(true, Some(0));
    guest.sigtimedwait_results = VecDeque::from([Ok(34), Ok(34), Ok(34), Err(Errno::EAGAIN)]);
    crate::syscalls::helpers::remove_ignored_signals(&mut guest, rt)
        .await
        .unwrap();
    assert_eq!(guest.sigtimedwait_sets, [rt; 4]);
    let mut guest = turn_guest(true, Some(0));
    guest.sigtimedwait_results = VecDeque::from([Err(Errno::EFAULT)]);
    let failed = crate::syscalls::helpers::remove_ignored_signals(&mut guest, rt).await;
    assert!(
        matches!(failed, Err(Error::Tool(_))),
        "Hermit's own failure, never a guest errno: {failed:?}"
    );
    let mut guest = turn_guest(true, Some(0));
    crate::syscalls::helpers::remove_ignored_signals(&mut guest, 0)
        .await
        .unwrap();
    assert!(
        guest.injected.is_empty(),
        "nothing to remove, nothing injected"
    );
}

/// The 3c.1d gate "SIG_BLOCK of SIGCHLD with a send committed before the
/// grant": the SIGCHLD was sent while the guest's mask let it in, so Linux
/// discarded it, though the runtime's mask held it; the thread then blocked
/// SIGCHLD. Rule (i) at that call's return (`M_req` the mask before, `M_next`
/// after) removes it, and a cross-process status, read against the mask the
/// thread published at that request, does not show it. Mutation "(i) using
/// only `M_next`": the removal is empty and `SigPnd` shows SIGCHLD.
#[test]
fn a_sigchld_sent_before_its_block_was_granted_is_neither_kept_nor_shown() {
    let chld = bit(libc::SIGCHLD);
    let state = proc_state(chld, 0, 0);
    assert_eq!(crate::ignored_signals::removable(&state, 0, chld, 0), chld);
    assert_eq!(
        crate::ignored_signals::hidden_from_status(chld, 0, 0, 0),
        chld
    );
    assert_eq!(
        crate::ignored_signals::hidden_from_status(chld, 0, 0, chld),
        0,
        "once the block is published, a SIGCHLD sent after it is shown"
    );
}

/// Design D37 rule (iii) and the 3c.1d gate "SIG_IGN -> SIG_DFL of SIGTERM
/// with a send during the wait: no death". SIGTERM is `SIG_IGN` and the
/// guest's mask lets it in, so Linux discarded the instance sent during the
/// wait, which the runtime's mask held: the `rt_sigaction` that makes it
/// `SIG_DFL` removes it before the kernel installs the new action, so it
/// cannot kill at the return. A blocked SIGTERM stays (Linux queued it too),
/// a call that keeps it ignored removes nothing, and with the gate off
/// nothing is read or removed.
///
/// Mutations: "removed after the kernel installs the action" (the order
/// assertion), "the removal ignores `M_req`" (the blocked row removes),
/// "not keyed on a move away from ignored" (the SIG_IGN row removes).
#[tokio::test]
async fn an_rt_sigaction_that_unignores_removes_first_the_instances_linux_discarded() {
    let term = bit(libc::SIGTERM);
    let action = |handler: usize| {
        let cell: &'static mut [u64; 4] = Box::leak(Box::new([handler as u64, 0, 0, 0]));
        RtSigaction::new()
            .with_signum(libc::SIGTERM)
            .with_action(Some(
                Addr::from_raw(cell as *mut [u64; 4] as usize).unwrap(),
            ))
            .with_old_action(None)
            .with_sigsetsize(8)
    };
    for (name, gate, own, handler, removed) in [
        ("unblocked, to SIG_DFL", true, 0, libc::SIG_DFL, vec![term]),
        ("blocked, to SIG_DFL", true, term, libc::SIG_DFL, vec![]),
        ("unblocked, SIG_IGN again", true, 0, libc::SIG_IGN, vec![]),
        ("gate off", false, 0, libc::SIG_DFL, vec![]),
    ] {
        let _seam = script(proc_state(term, 0, term));
        let mut guest = turn_guest(gate, Some(own));
        let tool = super::tool(&guest);
        let result = tool.handle_rt_sigaction(&mut guest, action(handler)).await;
        assert_eq!(result.unwrap(), 0, "{name}");
        assert_eq!(guest.sigtimedwait_sets, removed, "{name}");
        let expected: Vec<Sysno> = if removed.is_empty() {
            vec![Sysno::rt_sigaction, Sysno::rt_sigaction]
        } else {
            vec![
                Sysno::rt_sigaction,
                Sysno::rt_sigtimedwait,
                Sysno::rt_sigaction,
            ]
        };
        assert_eq!(guest.injected, expected, "{name}");
    }
}

/// The "Codex H" row (design 3c.1 plan, revision 2's round-1 table) and the
/// 3c.1d gate "phase 1 still refuses temporary-mask calls in a SIGALRM
/// process": in a process whose SIGALRM is a phase-1 guest handler, every
/// temporary-mask call (`rt_sigsuspend`, a masked `pselect6`, `ppoll`,
/// `epoll_pwait`, `epoll_pwait2`) is refused with EOPNOTSUPP before its
/// handler runs, through the whole pipeline (`handle_syscall_event`), under
/// the gate too: with a default-fatal signal its temporary mask lets in
/// pending, nothing is parked, no D40 flag is recorded, and the guest sees
/// EOPNOTSUPP. Mutation "the gated form placed before phase 1's gate": the
/// call ends with EINTR for the fatal signal instead.
#[tokio::test]
async fn phase_1_still_refuses_every_temporary_mask_call_in_a_process_that_handles_sigalrm() {
    let term = bit(libc::SIGTERM);
    let empty = mask_cell(0);
    let wrapper: &'static mut [usize; 2] = Box::leak(Box::new([empty, 8]));
    let wrapper = wrapper as *mut [usize; 2] as usize;
    let zero: &'static mut [i64; 2] = Box::leak(Box::new([0, 0]));
    let zero = zero as *mut [i64; 2] as usize;
    let calls: Vec<(&str, Syscall)> = vec![
        ("rt_sigsuspend", Syscall::from(suspend(0))),
        (
            "pselect6",
            Syscall::from(
                reverie::syscalls::Pselect6::new()
                    .with_nfds(0)
                    .with_timeout(AddrMut::from_raw(zero))
                    .with_sigmask(Addr::from_raw(wrapper)),
            ),
        ),
        (
            "ppoll",
            Syscall::from(
                reverie::syscalls::Ppoll::new()
                    .with_nfds(0)
                    .with_timeout(AddrMut::from_raw(zero))
                    .with_sigmask(Addr::from_raw(empty))
                    .with_sigsetsize(8),
            ),
        ),
        (
            "epoll_pwait",
            Syscall::from(
                reverie::syscalls::EpollPwait::new()
                    .with_epfd(3)
                    .with_maxevents(1)
                    .with_timeout(1000)
                    .with_sigmask(Addr::from_raw(empty))
                    .with_sigsetsize(8),
            ),
        ),
        (
            "epoll_pwait2",
            Syscall::from_raw(
                Sysno::epoll_pwait2,
                reverie::syscalls::SyscallArgs::new(3, 0, 1, zero, empty, 8),
            ),
        ),
    ];
    for (name, call) in calls {
        let _seam = script(proc_state(term, 0, 0));
        let mut guest = turn_guest(true, Some(term));
        guest.thread.sigalrm_handled = true;
        guest.thread.sigalrm_blocked_published = false;
        let tool = <Detcore as Tool>::new(Pid::from_raw(THREAD.as_raw()), &guest.config);
        let result = tool.handle_syscall_event(&mut guest, call).await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::EOPNOTSUPP))),
            "{name}: {result:?}"
        );
        assert_eq!(guest.thread.wait_signal_end, None, "{name}");
        assert!(suspend_requests(&guest).is_empty(), "{name}");
        assert!(guest.probe_masks.is_empty(), "{name}: no probe ran");
    }
}

/// Round-3 ruling 4: the removal of ignored signals stages its two cells on
/// the scratch stack; a stack with no room is Hermit's own failure
/// (`Error::Tool`), never an EFAULT the guest's `rt_sigaction`,
/// `rt_sigsuspend` or masked wait could return. Mutation "the staging error
/// returned as it is" (EFAULT).
#[tokio::test]
async fn a_removal_with_no_scratch_stack_is_hermit_s_own_failure() {
    let mut guest = turn_guest(true, Some(0));
    guest.tight_stack = true;
    let result =
        crate::syscalls::helpers::remove_ignored_signals(&mut guest, bit(libc::SIGURG)).await;
    assert!(
        matches!(result, Err(Error::Tool(_))),
        "Hermit's own failure: {result:?}"
    );
    assert!(guest.sigtimedwait_sets.is_empty(), "nothing injected");
}
