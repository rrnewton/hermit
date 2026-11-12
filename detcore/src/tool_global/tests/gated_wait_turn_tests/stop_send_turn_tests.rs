/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Stop-signal sends and armings the in-guest backends cannot hold
//! (https://github.com/rrnewton/hermit/issues/5430), with the parent module's
//! harness: `/proc` reads scripted, scheduler RPCs answered and logged, the
//! kernel's injections answered by the harness.
//!
//! A send of SIGTSTP, SIGTTIN or SIGTTOU is admitted only where Linux would
//! discard it when it is sent: `prepare_signal` returns `!sig_ignored(p, ..)`
//! for the task the send names (kernel/signal.c:967 at v6.9), and
//! `sig_ignored` never ignores a blocked signal (:103-122). So the target's
//! action must be `SIG_IGN` and the signal unblocked, in that task's
//! `/proc/<tid>/status`: for a process-directed send, the thread whose id is
//! the pid. SIGSTOP is always refused.

use super::*;
use crate::syscalls::ReservedSendSiginfo;
use crate::syscalls::ReservedSendTarget;

/// A thread of `PEER`'s process that is not its leader, numbered below it, so
/// that the process's live threads, in DetTid order, start with it.
const SIBLING: DetTid = DetTid::from_raw(40);

/// A scripted `/proc` state with nothing pending, `blocked` blocked, and the
/// dispositions `caught` and `ignored`.
fn status(blocked: u64, caught: u64, ignored: u64) -> KernelSignalState {
    KernelSignalState {
        blocked,
        ..proc_state(0, caught, ignored)
    }
}

/// The target forms of a send to `PEER`, and the sender's own process.
const FORMS: [&str; 5] = ["kill", "tkill", "tgkill", "pidfd", "self"];

fn target_of(form: &str) -> ReservedSendTarget {
    match form {
        "kill" => ReservedSendTarget::Process(PEER.as_raw()),
        "tkill" => ReservedSendTarget::Thread {
            tgid: None,
            tid: PEER.as_raw(),
        },
        "tgkill" => ReservedSendTarget::Thread {
            tgid: Some(PEER.as_raw()),
            tid: PEER.as_raw(),
        },
        "self" => ReservedSendTarget::Process(THREAD.as_raw()),
        _ => ReservedSendTarget::Pidfd {
            named: Some(DetPid::from_raw(PEER.as_raw())),
            thread: false,
        },
    }
}

/// `refuse_stop_signal_send` of `signal` by `form` under the gate, with the
/// target's `/proc` status `target` (the sender's own for the self form):
/// its answer and the WARNs it logged.
async fn send_under_the_gate(
    form: &str,
    signal: i32,
    target: Result<KernelSignalState, Errno>,
) -> (Option<Result<i64, Error>>, Vec<String>) {
    let own = target.unwrap_or_else(|_| proc_state(0, 0, 0));
    let _seam = if form == "self" {
        signal_state_read_seam::install(move |_, tid| {
            (tid.as_raw() == THREAD.as_raw()).then_some(target)
        })
    } else {
        script_both(own, target)
    };
    let log = WarnLog::default();
    let _subscriber = tracing::subscriber::set_default(log.clone());
    let mut guest = turn_guest(true, Some(0));
    let tool = tool(&guest);
    let result = tool
        .refuse_stop_signal_send(
            &mut guest,
            Sysno::kill,
            signal,
            target_of(form),
            ReservedSendSiginfo::None,
        )
        .await;
    let warns = log.0.lock().unwrap().clone();
    (result, warns)
}

fn refused(result: &Option<Result<i64, Error>>) -> bool {
    matches!(result, Some(Err(Error::Errno(Errno::EPERM))))
}

/// A SIGTSTP the target ignores but blocks is refused, by every target form:
/// Linux queues a blocked signal even while it is ignored, "since the signal
/// handler may change by the time it is unblocked" (kernel/signal.c:103-122
/// at v6.9), and the target stops once it restores `SIG_DFL` and unblocks it.
/// Mutation: the blocked-bit check dropped (each form admits the send).
#[tokio::test]
async fn a_blocked_ignored_sigtstp_is_refused() {
    let tstp = bit(libc::SIGTSTP);
    for form in FORMS {
        let (result, warns) =
            send_under_the_gate(form, libc::SIGTSTP, Ok(status(tstp, 0, tstp))).await;
        assert!(refused(&result), "{form}: {result:?}");
        assert_eq!(warns.len(), 1, "{form}: {warns:?}");
    }
}

/// A SIGTSTP the target catches is refused, unblocked included: Linux
/// delivers it to the handler, so the send is not discarded, and the
/// in-guest backends admit no SIGTSTP handler to deliver it to.
#[tokio::test]
async fn a_caught_sigtstp_is_refused() {
    let tstp = bit(libc::SIGTSTP);
    for form in FORMS {
        let (result, warns) =
            send_under_the_gate(form, libc::SIGTSTP, Ok(status(0, tstp, 0))).await;
        assert!(refused(&result), "{form}: {result:?}");
        assert_eq!(warns.len(), 1, "{form}: {warns:?}");
    }
}

/// A SIGTSTP whose target's status cannot be read is refused, fail closed,
/// and the WARN says why.
#[tokio::test]
async fn a_sigtstp_whose_target_status_is_unreadable_is_refused() {
    for form in FORMS {
        let (result, warns) = send_under_the_gate(form, libc::SIGTSTP, Err(Errno::ESRCH)).await;
        assert!(refused(&result), "{form}: {result:?}");
        assert_eq!(warns.len(), 1, "{form}: {warns:?}");
        assert!(warns[0].contains("could not be read"), "{form}: {warns:?}");
    }
}

/// The positive control: a SIGTSTP, SIGTTIN or SIGTTOU the target ignores and
/// does not block is admitted, with no WARN: Linux discards it when it is
/// sent, so nothing can stop.
#[tokio::test]
async fn an_unblocked_ignored_stop_signal_is_admitted() {
    for signal in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
        let bit = bit(signal);
        for form in FORMS {
            let (result, warns) = send_under_the_gate(form, signal, Ok(status(0, 0, bit))).await;
            assert!(result.is_none(), "{form} {signal}: {result:?}");
            assert!(warns.is_empty(), "{form} {signal}: {warns:?}");
        }
    }
}

/// A process-directed SIGTSTP is judged by the leader's status, the task
/// Linux checks, not by whichever live thread comes first: with a sibling
/// numbered below the leader, a leader that blocks the ignored signal is
/// refused although the sibling does not block it, and a leader that does not
/// block it is admitted although the sibling does. Mutation: the process's
/// first live thread read instead of its leader (the first row is admitted).
#[tokio::test]
async fn a_process_directed_sigtstp_is_judged_by_the_leaders_status() {
    let tstp = bit(libc::SIGTSTP);
    for (leader_blocked, sibling_blocked, refuse) in [(tstp, 0, true), (0, tstp, false)] {
        let _seam = signal_state_read_seam::install(move |_, tid| {
            if tid.as_raw() == PEER.as_raw() {
                Some(Ok(status(leader_blocked, 0, tstp)))
            } else if tid.as_raw() == SIBLING.as_raw() {
                Some(Ok(status(sibling_blocked, 0, tstp)))
            } else {
                None
            }
        });
        let log = WarnLog::default();
        let _subscriber = tracing::subscriber::set_default(log.clone());
        let mut guest = turn_guest(true, Some(0));
        guest.threads_of = vec![(PEER, vec![SIBLING, PEER])];
        let tool = tool(&guest);
        for form in ["kill", "pidfd"] {
            let result = tool
                .refuse_stop_signal_send(
                    &mut guest,
                    Sysno::kill,
                    libc::SIGTSTP,
                    target_of(form),
                    ReservedSendSiginfo::None,
                )
                .await;
            assert_eq!(
                refused(&result),
                refuse,
                "{form}: leader blocked {leader_blocked:#x}, sibling {sibling_blocked:#x}: \
                 {result:?}"
            );
        }
    }
}

/// A pidfd descriptor at `fd` in `guest`'s table naming `target`, or naming
/// nothing Detcore knows (a received or inherited pidfd).
fn add_pidfd(guest: &mut TurnGuest, fd: i32, target: Option<DetPid>) {
    let pidfd = crate::fd::DetFd::new(
        fd,
        nix::fcntl::OFlag::O_CLOEXEC,
        crate::fd::FdType::Pidfd,
        crate::types::OpenFileId::new(THREAD, fd as u64),
    );
    if let Some(target) = target {
        pidfd.set_pidfd_target(target);
    }
    guest
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .file_handles
        .insert(fd, pidfd);
}

/// `pidfd_send_signal(fd, signal, info, flags)` through the real handler.
async fn pidfd_send(
    guest: &mut TurnGuest,
    fd: i32,
    signal: i32,
    info: usize,
    flags: u32,
) -> Result<Result<i64, Error>, String> {
    let call = Syscall::from_raw(
        Sysno::pidfd_send_signal,
        reverie::syscalls::SyscallArgs::new(
            fd as usize,
            signal as usize,
            info,
            flags as usize,
            0,
            0,
        ),
    );
    let tool = tool(guest);
    to_guest_end(tool.handle_pidfd_send_signal(guest, call, fd, signal, flags)).await
}

/// The `pidfd_send_signal` handler refuses a SIGSTOP to the process a pidfd
/// names under the gate, with the WARN, before anything reaches the kernel.
/// Mutation: the handler's stop-send call deleted (the send is injected).
#[tokio::test]
async fn the_pidfd_send_signal_handler_refuses_a_sigstop() {
    let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
    let log = WarnLog::default();
    let _subscriber = tracing::subscriber::set_default(log.clone());
    let mut guest = turn_guest(true, Some(0));
    use_real_test_thread(&mut guest);
    add_pidfd(&mut guest, 9, Some(DetPid::from_raw(PEER.as_raw())));
    let result = pidfd_send(&mut guest, 9, libc::SIGSTOP, 0, 0).await;
    assert!(
        matches!(result, Ok(Err(Error::Errno(Errno::EPERM)))),
        "{result:?}"
    );
    assert_eq!(log.0.lock().unwrap().len(), 1);
    assert!(guest.injected.is_empty(), "{:?}", guest.injected);
}

/// A stop signal through a pidfd whose target Detcore cannot name is refused,
/// fail closed (`TargetUnknown`), with no `/proc` read, after the kernel's
/// earlier answers, which keep their errno and log no WARN: an invalid flag
/// (EINVAL, checked before the descriptor), a descriptor that is not a pidfd
/// (EBADF), a siginfo Linux cannot copy (EFAULT) and one whose `si_signo` is
/// not the signal (EINVAL). Before, such a send went on to the kernel.
#[tokio::test]
async fn a_stop_signal_through_a_pidfd_detcore_cannot_name_is_refused() {
    // Any read of a target's status fails the test.
    let _seam = signal_state_read_seam::install(|pid, tid| {
        panic!("a /proc read of {pid}/{tid} for a target Detcore cannot name")
    });
    let rows: Vec<(&str, i32, i32, usize, u32, Option<Errno>)> = vec![
        ("SIGSTOP, no siginfo", 9, libc::SIGSTOP, 0, 0, None),
        ("SIGTSTP, no siginfo", 9, libc::SIGTSTP, 0, 0, None),
        (
            "SIGSTOP, a valid siginfo",
            9,
            libc::SIGSTOP,
            siginfo_page(libc::SIGSTOP, libc::SI_QUEUE, 0, libc::PROT_READ),
            0,
            None,
        ),
        (
            "SIGSTOP, an invalid flag",
            9,
            libc::SIGSTOP,
            0,
            0x8000_0000,
            Some(Errno::EINVAL),
        ),
        (
            "SIGSTOP, a pipe",
            5,
            libc::SIGSTOP,
            0,
            0,
            Some(Errno::EBADF),
        ),
        (
            "SIGSTOP, a siginfo in a PROT_NONE page",
            9,
            libc::SIGSTOP,
            siginfo_page(libc::SIGSTOP, libc::SI_QUEUE, 0, libc::PROT_NONE),
            0,
            Some(Errno::EFAULT),
        ),
        (
            "SIGSTOP, a si_signo that is not the signal",
            9,
            libc::SIGSTOP,
            siginfo_page(libc::SIGTERM, libc::SI_QUEUE, 0, libc::PROT_READ),
            0,
            Some(Errno::EINVAL),
        ),
    ];
    for (name, fd, signal, info, flags, errno) in rows {
        let log = WarnLog::default();
        let _subscriber = tracing::subscriber::set_default(log.clone());
        let mut guest = turn_guest(true, Some(0));
        use_real_test_thread(&mut guest);
        add_pidfd(&mut guest, 9, None);
        add_pipe(&mut guest, 5, crate::types::OpenFileId::new(THREAD, 5));
        let result = pidfd_send(&mut guest, fd, signal, info, flags).await;
        let wanted = errno.unwrap_or(Errno::EPERM);
        assert!(
            matches!(&result, Ok(Err(Error::Errno(got))) if *got == wanted),
            "{name}: {result:?}"
        );
        let warns = log.0.lock().unwrap().clone();
        assert_eq!(
            warns.len(),
            usize::from(errno.is_none()),
            "{name}: {warns:?}"
        );
        assert!(guest.injected.is_empty(), "{name}: {:?}", guest.injected);
    }
}

/// The kernel's `struct sigevent`: value, signo, notify, then the union whose
/// first member is the thread id of `SIGEV_THREAD_ID`; 64 bytes.
#[repr(C)]
struct KernelSigevent {
    value: u64,
    signo: i32,
    notify: i32,
    tid: i32,
    pad: [i32; 11],
}

/// `timer_create(clock, event, &id)` through the real handler, with the gate
/// `gate`, and its WARNs naming a refused stop-signal arming.
async fn timer_create(
    gate: bool,
    clock: i32,
    event: Option<KernelSigevent>,
) -> (Result<i64, Error>, usize) {
    use reverie::syscalls::FromToRaw;
    let log = ArmingLog::default();
    let _subscriber = tracing::subscriber::set_default(log.clone());
    let mut guest = turn_guest(gate, Some(0));
    let tool = tool(&guest);
    let id = Box::leak(Box::new(0_i32)) as *mut i32 as usize;
    let mut call = reverie::syscalls::TimerCreate::new()
        .with_clockid(reverie::syscalls::ClockId::from_raw(clock as _))
        .with_timerid(AddrMut::from_raw(id));
    if let Some(event) = event {
        let event = Box::leak(Box::new(event)) as *mut KernelSigevent as usize;
        call = call.with_sevp(AddrMut::from_raw(event));
    }
    let result = tool.handle_timer_create(&mut guest, call).await;
    let warns = log.0.lock().unwrap().len();
    (result, warns)
}

fn sigevent(notify: i32, signo: i32, tid: i32) -> Option<KernelSigevent> {
    Some(KernelSigevent {
        value: 0,
        signo,
        notify,
        tid,
        pad: [0; 11],
    })
}

/// Collects the WARN messages that name `HERMIT_STOP_SIGNAL_ARMING_REFUSED`.
#[derive(Clone, Default)]
struct ArmingLog(std::sync::Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for ArmingLog {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::WARN)
    }
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = MessageVisitor(None);
        event.record(&mut visitor);
        if let Some(message) = visitor.0
            && message.contains("HERMIT_STOP_SIGNAL_ARMING_REFUSED")
        {
            self.0.lock().unwrap().push(message);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// A POSIX timer whose notification is a stop signal is refused when it is
/// created, with EPERM and the named WARN, for each notification that
/// delivers a signal (`SIGEV_SIGNAL`, `SIGEV_THREAD`, `SIGEV_THREAD_ID` naming
/// a thread of the group): at its expiry Detcore's scheduler would send the
/// signal itself, through no guest send handler. Linux's earlier answers keep
/// precedence: an unsupported clock (EOPNOTSUPP, Linux's answer right after
/// it copies the sigevent), an invalid notification, an out-of-range signal
/// and a `SIGEV_THREAD_ID` thread outside the group (EINVAL). Not refused:
/// `SIGEV_NONE`, which notifies nothing, a null sigevent (SIGALRM), and every
/// timer with the gate off. Mutation: the stop-signal timer check removed
/// (the first three rows create their timers).
#[tokio::test]
async fn a_stop_signal_timer_is_refused_when_it_is_created() {
    const SIGEV_THREAD_ID: i32 = reverie::syscalls::linux_abi::SIGEV_THREAD_ID;
    let own = THREAD.as_raw();
    let peer = PEER.as_raw();
    let monotonic = libc::CLOCK_MONOTONIC;
    let rows: Vec<(
        &str,
        bool,
        i32,
        Option<KernelSigevent>,
        Result<i64, Errno>,
        usize,
    )> = vec![
        (
            "SIGEV_SIGNAL SIGTSTP",
            true,
            monotonic,
            sigevent(libc::SIGEV_SIGNAL, libc::SIGTSTP, 0),
            Err(Errno::EPERM),
            1,
        ),
        (
            "SIGEV_THREAD SIGTTIN",
            true,
            monotonic,
            sigevent(libc::SIGEV_THREAD, libc::SIGTTIN, 0),
            Err(Errno::EPERM),
            1,
        ),
        (
            "SIGEV_THREAD_ID SIGSTOP to a thread of the group",
            true,
            monotonic,
            sigevent(SIGEV_THREAD_ID, libc::SIGSTOP, own),
            Err(Errno::EPERM),
            1,
        ),
        (
            "an unsupported clock with SIGTSTP",
            true,
            libc::CLOCK_MONOTONIC_RAW,
            sigevent(libc::SIGEV_SIGNAL, libc::SIGTSTP, 0),
            Err(Errno::EOPNOTSUPP),
            0,
        ),
        (
            "an invalid notification",
            true,
            monotonic,
            sigevent(99, libc::SIGTSTP, 0),
            Err(Errno::EINVAL),
            0,
        ),
        (
            "an out-of-range signal",
            true,
            monotonic,
            sigevent(libc::SIGEV_SIGNAL, 65, 0),
            Err(Errno::EINVAL),
            0,
        ),
        (
            "SIGEV_THREAD_ID SIGSTOP to a thread outside the group",
            true,
            monotonic,
            sigevent(SIGEV_THREAD_ID, libc::SIGSTOP, peer),
            Err(Errno::EINVAL),
            0,
        ),
        (
            "SIGEV_NONE with SIGTSTP",
            true,
            monotonic,
            sigevent(libc::SIGEV_NONE, libc::SIGTSTP, 0),
            Ok(0),
            0,
        ),
        ("a null sigevent", true, monotonic, None, Ok(0), 0),
        (
            "SIGEV_SIGNAL SIGTSTP with the gate off",
            false,
            monotonic,
            sigevent(libc::SIGEV_SIGNAL, libc::SIGTSTP, 0),
            Ok(0),
            0,
        ),
    ];
    for (name, gate, clock, event, expected, warns) in rows {
        let (result, logged) = timer_create(gate, clock, event).await;
        match expected {
            Ok(value) => assert!(
                matches!(result, Ok(got) if got == value),
                "{name}: {result:?}"
            ),
            Err(errno) => assert!(
                matches!(result, Err(Error::Errno(got)) if got == errno),
                "{name}: {result:?}"
            ),
        }
        assert_eq!(logged, warns, "{name}");
    }
}

/// The premise that keeps `real_blocked` out of the admission rule: Detcore
/// never leaves a guest thread sleeping in a host `rt_sigtimedwait` with a
/// nonzero timeout. A guest's wait with a five-second timeout is polled with
/// zero-timeout injections, with the gate on and off; Linux installs
/// `real_blocked` only for a wait that may sleep (`if (!sig && timeout)`,
/// kernel/signal.c:3646 at v6.9), so `/proc`'s `SigBlk` is the whole blocked
/// set a stop-send check must read.
#[tokio::test]
async fn a_guest_sigtimedwait_is_polled_with_zero_timeouts_only() {
    for gate in [true, false] {
        let _seam = script(proc_state(0, 0, 0));
        let mut guest = turn_guest(gate, Some(0));
        guest.sigtimedwait_results = VecDeque::from([Err(Errno::EAGAIN), Ok(libc::SIGUSR1 as i64)]);
        let tool = tool(&guest);
        let set = Box::leak(Box::new(bit(libc::SIGUSR1))) as *mut u64 as usize;
        let timeout = Box::leak(Box::new([5_i64, 0])) as *mut [i64; 2] as usize;
        let call = reverie::syscalls::RtSigtimedwait::new()
            .with_set(AddrMut::from_raw(set))
            .with_timeout(Addr::from_raw(timeout))
            .with_sigsetsize(8);
        let result = tool.handle_rt_sigtimedwait(&mut guest, call).await;
        assert!(
            matches!(result, Ok(signal) if signal == libc::SIGUSR1 as i64),
            "gate={gate}: {result:?}"
        );
        assert!(
            !guest.sigtimedwait_timeouts.is_empty(),
            "gate={gate}: nothing injected"
        );
        assert!(
            guest
                .sigtimedwait_timeouts
                .iter()
                .all(|timeout| *timeout == Some([0, 0])),
            "gate={gate}: {:?}",
            guest.sigtimedwait_timeouts
        );
    }
}
