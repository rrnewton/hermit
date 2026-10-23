/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1c: the waiting thread's own side of the parked-wait checks
//! under the in-guest delivery gate (`Config::in_guest_signal_delivery`),
//! and the restart checkpoint they feed (design 16.11, D40, D50; IR-2).
//!
//! The gate is forced on in the Detcore `Config` with the in-guest backend's
//! capabilities. The test thread plays the guest thread: its `/proc` reads
//! are scripted (`signal_state_read_seam`), the backend's record of the
//! guest's mask (`Guest::resume_signal_mask`) is a value the test chooses,
//! and its scheduler RPCs are answered by the harness, which logs them. In
//! every scripted state the kernel's mask is the in-guest runtime's, all
//! blocked, unless a row says otherwise, so a reader that took `SigBlk`
//! instead of the published mask would find nothing deliverable.

use std::collections::VecDeque;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Stack;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use super::*;
use crate::ThreadState;
use crate::syscalls::KernelSignalState;
use crate::syscalls::signal_state_read_seam;
use crate::wait_signal_end::WaitSignalEnd;

/// The guest thread, as the scheduler and `/proc` number it.
const THREAD: DetTid = DetTid::from_raw(31);

/// Another guest process's thread, its process's leader.
const PEER: DetTid = DetTid::from_raw(41);

fn bit(signal: i32) -> u64 {
    1_u64 << (signal - 1)
}

/// A scripted `/proc` state: `pending` in the thread's queue, the kernel mask
/// the in-guest runtime's (all blocked), `caught` and `ignored`.
fn proc_state(pending: u64, caught: u64, ignored: u64) -> KernelSignalState {
    KernelSignalState {
        pending,
        thread_pending: pending,
        shared_pending: 0,
        blocked: !0,
        caught,
        ignored,
    }
}

/// Answer this thread's `/proc` reads of `THREAD` with `state`.
fn script(state: KernelSignalState) -> signal_state_read_seam::Installed {
    signal_state_read_seam::install(move |pid, tid| {
        (pid.as_raw() == THREAD.as_raw() && tid.as_raw() == THREAD.as_raw()).then_some(Ok(state))
    })
}

/// Answer this thread's `/proc` reads of `THREAD` with `own` and of `PEER`
/// with `peer` (an error: an unreadable status).
fn script_both(
    own: KernelSignalState,
    peer: Result<KernelSignalState, Errno>,
) -> signal_state_read_seam::Installed {
    signal_state_read_seam::install(move |pid, tid| {
        if pid.as_raw() == THREAD.as_raw() && tid.as_raw() == THREAD.as_raw() {
            Some(Ok(own))
        } else if tid.as_raw() == PEER.as_raw() {
            Some(peer)
        } else {
            None
        }
    })
}

/// A scratch stack over a leaked buffer of this process, so that injected
/// queries the harness answers can write where Detcore reads.
struct ScratchStack {
    base: usize,
    used: usize,
}

struct ScratchGuard;
impl Drop for ScratchGuard {
    fn drop(&mut self) {}
}

impl Stack for ScratchStack {
    type StackGuard = ScratchGuard;
    fn size(&self) -> usize {
        self.used
    }
    fn capacity(&self) -> usize {
        4096
    }
    fn push<'a, T>(&mut self, value: T) -> Addr<'a, T> {
        let at = self.reserve::<T>();
        // SAFETY: `reserve` handed out an aligned, unused slot of the buffer.
        unsafe { std::ptr::write(at.as_raw() as *mut T, value) };
        Addr::from_raw(at.as_raw()).unwrap()
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        let align = std::mem::align_of::<T>().max(8);
        let start = (self.base + self.used).next_multiple_of(align);
        self.used = start + std::mem::size_of::<T>() - self.base;
        assert!(self.used <= 4096, "the scratch stack is full");
        // SAFETY: the slot lies inside the leaked buffer.
        unsafe { std::ptr::write_bytes(start as *mut u8, 0, std::mem::size_of::<T>()) };
        AddrMut::from_raw(start).unwrap()
    }
    fn commit(self) -> Result<ScratchGuard, Errno> {
        Ok(ScratchGuard)
    }
}

/// What the harness answers a `RequestResources` with, in order; a request
/// with nothing left is answered `Normal`.
type Answers = VecDeque<ResumeStatus>;

/// A guest thread whose scheduler RPCs the harness answers and logs.
struct TurnGuest {
    config: Config,
    thread: ThreadState<()>,
    /// The guest's own mask as the backend records it.
    own_mask: Option<u64>,
    /// Every RPC, in order.
    rpcs: Mutex<Vec<GlobalRequest>>,
    answers: Mutex<Answers>,
    /// What a `FutexAction::WaitRequest` is answered with.
    futex_answer: Option<SchedValue>,
    /// `sa_flags` an injected `rt_sigaction` query reports.
    action_flags: u64,
    /// Every injected syscall, in order.
    injected: Vec<Sysno>,
    /// What each injected `read` returns, in order (`EAGAIN` once empty).
    read_results: VecDeque<Result<i64, Errno>>,
    /// What each injected `write` or `writev` returns, in order (`EAGAIN`
    /// once empty).
    write_results: VecDeque<Result<i64, Errno>>,
    /// What `GlobalRequest::KernelRunTarget` is answered with (D44).
    kernel_run: Option<crate::scheduler::KernelRunBlock>,
    /// The threads Detcore's model shows live, each its own process: what
    /// `GlobalRequest::ResolveKillTargets` answers.
    live: Vec<DetTid>,
    stack: usize,
}

impl TurnGuest {
    fn rpcs(&self) -> Vec<GlobalRequest> {
        self.rpcs.lock().unwrap().clone()
    }

    fn requests(&self) -> Vec<Resources> {
        self.rpcs()
            .into_iter()
            .filter_map(|rpc| match rpc {
                GlobalRequest::RequestResources(resources, _) => Some(resources),
                _ => None,
            })
            .collect()
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for TurnGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.rpcs.lock().unwrap().push(message.2.clone());
        let response = match message.2 {
            GlobalRequest::RequestResources(..) => GlobalResponse::RequestResources(
                self.answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(ResumeStatus::Normal),
            ),
            GlobalRequest::ReleaseAllResources => GlobalResponse::ReleaseAllResources(()),
            GlobalRequest::SetSavedGuestSigmask(_) => GlobalResponse::SetSavedGuestSigmask(()),
            GlobalRequest::FutexAction(_, FutexAction::WaitRequest(..), ..) => {
                GlobalResponse::FutexAction(self.futex_answer.clone())
            }
            GlobalRequest::FutexAction(..) => GlobalResponse::FutexAction(None),
            GlobalRequest::StoppingRefusal(_) => GlobalResponse::PolicyRefusal(()),
            GlobalRequest::KernelRunTarget(_) => GlobalResponse::KernelRunTarget(self.kernel_run),
            GlobalRequest::ResolveKillTargets(process) => {
                let target = DetTid::from_raw(process.as_raw());
                let live = self.live.contains(&target);
                GlobalResponse::ResolveKillTargets(
                    if live { vec![target] } else { Vec::new() },
                    Some(false),
                )
            }
            request => panic!("an unexpected RPC: {request:?}"),
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for TurnGuest {
    type Memory = LocalMemory;
    type Stack = ScratchStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(THREAD.as_raw())
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(THREAD.as_raw())
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> LocalMemory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    fn resume_signal_mask(&self) -> Option<u64> {
        self.own_mask
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        // SAFETY: a plain register record.
        unsafe { std::mem::zeroed() }
    }
    async fn stack(&mut self) -> ScratchStack {
        ScratchStack {
            base: self.stack,
            used: 0,
        }
    }
    async fn daemonize(&mut self) {
        panic!("a wait must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        self.injected.push(call.number());
        let (number, args) = call.into_parts();
        match Syscall::from_raw(number, args) {
            Syscall::Read(_) => self.read_results.pop_front().unwrap_or(Err(Errno::EAGAIN)),
            Syscall::Write(_) | Syscall::Writev(_) => {
                self.write_results.pop_front().unwrap_or(Err(Errno::EAGAIN))
            }
            // A send the harness lets through.
            Syscall::Tgkill(_) => Ok(0),
            Syscall::RtSigaction(query) => {
                if let Some(old) = query.old_action() {
                    let mut action = [0_u64; 4];
                    action[1] = self.action_flags;
                    LocalMemory::new().write_value(old.cast::<[u64; 4]>(), &action)?;
                }
                Ok(0)
            }
            Syscall::RtSigprocmask(query) => {
                if let Some(old) = query.oldset() {
                    LocalMemory::new().write_value(old.cast::<u64>(), &0)?;
                }
                Ok(0)
            }
            other => panic!("an unexpected injection: {other:?}"),
        }
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("a wait must not tail-inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        Ok(())
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        Ok(())
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        Ok(0)
    }
}

/// An in-guest thread with the delivery gate `gate`, whose backend records
/// the guest's own mask as `own_mask`.
fn turn_guest(gate: bool, own_mask: Option<u64>) -> TurnGuest {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let detpid = DetPid::from_raw(THREAD.as_raw());
    let mut thread = ThreadState::new(detpid, &config, ());
    thread.dettid = THREAD;
    thread.detpid = Some(detpid);
    let stack = Box::leak(Box::new([0_u64; 512])).as_mut_ptr() as usize;
    TurnGuest {
        config,
        thread,
        own_mask,
        rpcs: Mutex::new(Vec::new()),
        answers: Mutex::new(VecDeque::new()),
        futex_answer: Some(SchedValue::Value(0)),
        action_flags: 0,
        injected: Vec::new(),
        read_results: VecDeque::new(),
        write_results: VecDeque::new(),
        kernel_run: None,
        live: vec![THREAD, PEER],
        stack,
    }
}

fn tool(guest: &TurnGuest) -> Detcore {
    Detcore::new(Pid::from_raw(THREAD.as_raw()), &guest.config)
}

/// The rows every gated reader is checked against: the signal pending, the
/// guest's own mask, the dispositions, the end expected.
struct Row {
    name: &'static str,
    state: KernelSignalState,
    own: u64,
    end: Option<WaitSignalEnd>,
}

fn rows() -> Vec<Row> {
    let term = bit(libc::SIGTERM);
    let usr1 = bit(libc::SIGUSR1);
    vec![
        Row {
            name: "default-fatal, unblocked by the published mask",
            state: proc_state(term, 0, 0),
            own: 0,
            end: Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
        },
        Row {
            name: "caught",
            state: proc_state(usr1, usr1, 0),
            own: 0,
            end: Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
        },
        Row {
            name: "blocked by the published mask, the kernel's unblocked",
            state: KernelSignalState {
                blocked: 0,
                ..proc_state(term, 0, 0)
            },
            own: term,
            end: None,
        },
        Row {
            name: "SIG_IGN",
            state: proc_state(term, 0, term),
            own: 0,
            end: None,
        },
        Row {
            name: "default-ignored",
            state: proc_state(bit(libc::SIGURG), 0, 0),
            own: 0,
            end: None,
        },
    ]
}

/// D50 reader 8, `sleep_signal`: before the park (a signal sent before the
/// thread filed its sleep) and after a wake, the sleep ends exactly for a
/// deliverable signal, and records the end for the restart checkpoint.
/// Mutation "the at-park check removed": the `BeforePark` rows that must end
/// do not.
#[tokio::test]
async fn under_the_gate_a_sleep_ends_before_its_park_and_after_a_wake_only_for_a_deliverable_signal()
 {
    use crate::syscalls::SleepCheck;
    use crate::syscalls::SleepSignal;
    for row in rows() {
        for check in [
            SleepCheck::BeforePark,
            SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGTERM)])),
        ] {
            let _seam = script(row.state);
            let mut guest = turn_guest(true, Some(row.own));
            let verdict = crate::syscalls::sleep_signal(&mut guest, check.clone(), true)
                .await
                .unwrap();
            let expected = if row.end.is_some() {
                SleepSignal::Pending
            } else {
                SleepSignal::None
            };
            assert_eq!(verdict, expected, "{}: {check:?}", row.name);
            assert_eq!(guest.thread.wait_signal_end, row.end, "{}", row.name);
        }
    }
}

/// Signal phase 1's virtual SIGALRM wake ends a `pause` as a caught
/// delivery of SIGALRM, with no physical copy pending (IR-12), taking its
/// place on the shared queue in Linux's dequeue order: a deliverable
/// default-fatal signal that does not dump core still makes the end fatal,
/// and so does a core-dumping one dequeued first (a private one, or a shared
/// lower-numbered one), but not a shared core-dumping one SIGALRM precedes.
/// Mutation "any default-fatal signal makes the end fatal": the shared
/// SIGXCPU row becomes `Fatal(SIGXCPU)`.
#[tokio::test]
async fn under_the_gate_phase_1_s_virtual_sigalrm_wake_ends_a_pause_as_caught() {
    use crate::syscalls::SleepCheck;
    use crate::syscalls::SleepSignal;
    let wake = SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGALRM)]));
    let shared = |pending| KernelSignalState {
        thread_pending: 0,
        shared_pending: pending,
        ..proc_state(pending, 0, 0)
    };
    for (state, end) in [
        (proc_state(0, 0, 0), WaitSignalEnd::Caught(libc::SIGALRM)),
        (
            proc_state(bit(libc::SIGTERM), 0, 0),
            WaitSignalEnd::Fatal(libc::SIGTERM),
        ),
        (
            proc_state(bit(libc::SIGXCPU), 0, 0),
            WaitSignalEnd::Fatal(libc::SIGXCPU),
        ),
        (
            shared(bit(libc::SIGXCPU)),
            WaitSignalEnd::Caught(libc::SIGALRM),
        ),
        (
            shared(bit(libc::SIGQUIT)),
            WaitSignalEnd::Fatal(libc::SIGQUIT),
        ),
    ] {
        let _seam = script(state);
        let mut guest = turn_guest(true, Some(0));
        guest.thread.sigalrm_handled = true;
        let verdict = crate::syscalls::sleep_signal(&mut guest, wake.clone(), true)
            .await
            .unwrap();
        assert_eq!(verdict, SleepSignal::Pending);
        assert_eq!(guest.thread.wait_signal_end, Some(end));
    }
    // Not a phase-1 process: nothing pending, nothing ends it.
    let _seam = script(proc_state(0, 0, 0));
    let mut guest = turn_guest(true, Some(0));
    let verdict = crate::syscalls::sleep_signal(&mut guest, wake, true)
        .await
        .unwrap();
    assert_eq!(verdict, SleepSignal::None);
}

/// With the gate off the in-guest sleep keeps today's answers (its backend
/// does not report the guest's signal state): nothing ends it before the
/// park, and any wake ends it (`Held`), and no end is recorded.
#[tokio::test]
async fn with_the_gate_off_a_sleep_keeps_today_s_answers() {
    use crate::syscalls::SleepCheck;
    use crate::syscalls::SleepSignal;
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(false, Some(0));
    for (check, expected) in [
        (SleepCheck::BeforePark, SleepSignal::None),
        (
            SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGTERM)])),
            SleepSignal::Held,
        ),
    ] {
        let verdict = crate::syscalls::sleep_signal(&mut guest, check, true)
            .await
            .unwrap();
        assert_eq!(verdict, expected);
    }
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// D50 reader 9, `handle_futex_blocking`: a deliverable signal pending at the
/// park ends the wait with the futex's restart errno before any wait request,
/// and records the end; otherwise the wait publishes the guest's mask
/// (`SetSavedGuestSigmask`, the 3c.1b carry: a futex wait files no
/// `Resources` request) before its `FutexAction::WaitRequest`, whose watch
/// unblocks exactly what the published mask does. Mutations: "SigBlk used"
/// (the kernel mask blocks everything) and "publication after the wait
/// request".
#[tokio::test]
async fn under_the_gate_a_futex_wait_ends_at_its_park_or_publishes_its_mask_before_waiting() {
    let word: &'static mut u32 = Box::leak(Box::new(7));
    let address = word as *mut u32 as usize;
    let call = reverie::syscalls::Futex::new()
        .with_uaddr(AddrMut::from_raw(address))
        .with_futex_op(libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG)
        .with_val(7);
    for row in rows() {
        let _seam = script(row.state);
        let mut guest = turn_guest(true, Some(row.own));
        let tool = tool(&guest);
        let result = tool
            .handle_futex_blocking(
                &mut guest,
                call,
                7,
                crate::syscalls::FutexDeadline::FromCall,
            )
            .await;
        assert_eq!(guest.thread.wait_signal_end, row.end, "{}", row.name);
        let rpcs = guest.rpcs();
        if row.end.is_some() {
            assert!(
                matches!(result, Err(Error::Errno(Errno::ERESTARTSYS))),
                "{}: {result:?}",
                row.name
            );
            assert!(
                !rpcs
                    .iter()
                    .any(|rpc| matches!(rpc, GlobalRequest::FutexAction(..))),
                "{}: no wait request",
                row.name
            );
            continue;
        }
        assert_eq!(result.unwrap(), 0, "{}", row.name);
        let publication = rpcs
            .iter()
            .position(|rpc| *rpc == GlobalRequest::SetSavedGuestSigmask(Some(row.own)))
            .unwrap_or_else(|| panic!("{}: no publication in {rpcs:?}", row.name));
        let wait = rpcs
            .iter()
            .position(|rpc| {
                matches!(rpc, GlobalRequest::FutexAction(_, FutexAction::WaitRequest(_, Some(watch)), ..)
                    if watch.unblocked & bit(libc::SIGTERM) == !row.own & bit(libc::SIGTERM))
            })
            .unwrap_or_else(|| panic!("{}: no wait request in {rpcs:?}", row.name));
        assert!(
            publication < wait,
            "{}: published before the wait",
            row.name
        );
    }
}

/// With the gate off an in-guest futex wait publishes nothing and reports no
/// watch, as before.
#[tokio::test]
async fn with_the_gate_off_a_futex_wait_publishes_nothing() {
    let word: &'static mut u32 = Box::leak(Box::new(7));
    let call = reverie::syscalls::Futex::new()
        .with_uaddr(AddrMut::from_raw(word as *mut u32 as usize))
        .with_futex_op(libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG)
        .with_val(7);
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(false, Some(0));
    let tool = tool(&guest);
    let result = tool
        .handle_futex_blocking(
            &mut guest,
            call,
            7,
            crate::syscalls::FutexDeadline::FromCall,
        )
        .await;
    assert_eq!(result.unwrap(), 0);
    let rpcs = guest.rpcs();
    assert!(
        !rpcs
            .iter()
            .any(|rpc| matches!(rpc, GlobalRequest::SetSavedGuestSigmask(_)))
    );
    assert!(rpcs.iter().any(|rpc| matches!(
        rpc,
        GlobalRequest::FutexAction(_, FutexAction::WaitRequest(_, None), ..)
    )));
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// D50 reader 2, the retry loop under `retry_nonblocking_syscall_with_timeout`
/// and `retry_nonblocking_syscall` (`retry_nonblocking_syscall_helper`), here
/// through the latter, as a polled read takes it: a restartable
/// read that would block ends with `ERESTARTSYS` once a deliverable signal is
/// pending, at its first turn (a signal sent before it parked), and records
/// the end; otherwise it polls on.
#[tokio::test]
async fn under_the_gate_a_restartable_poll_ends_where_it_would_block_only_for_a_deliverable_signal()
{
    for row in rows() {
        let _seam = script(row.state);
        let mut guest = turn_guest(true, Some(row.own));
        // Descriptor 3, a pipe Hermit made physically nonblocking, as the
        // internal-polling path requires.
        let pipe = crate::fd::DetFd::new(
            3,
            nix::fcntl::OFlag::O_RDONLY,
            crate::fd::FdType::Pipe,
            crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 99),
        );
        pipe.set_physically_nonblocking();
        guest
            .thread
            .file_metadata
            .lock()
            .unwrap()
            .file_handles
            .insert(3, pipe);
        // A first attempt that would block, then data.
        guest.read_results = VecDeque::from([Err(Errno::EAGAIN), Ok(4)]);
        let mut poll = Resources::new(THREAD);
        poll.insert(ResourceID::InternalIOPolling, Permission::R);
        poll.fyi("read");
        let read = reverie::syscalls::Read::new()
            .with_fd(3)
            .with_buf(AddrMut::from_raw(guest.stack))
            .with_len(8);
        let result = crate::syscalls::helpers::retry_nonblocking_syscall::<
            crate::record_or_replay::NoopTool,
            _,
            _,
        >(&mut guest, read, poll, None)
        .await;
        assert_eq!(guest.thread.wait_signal_end, row.end, "{}", row.name);
        if row.end.is_some() {
            assert!(
                matches!(result, Err(Error::Errno(Errno::ERESTARTSYS))),
                "{}: {result:?}",
                row.name
            );
        } else {
            assert_eq!(result.unwrap(), 4, "{}: it polled on", row.name);
        }
    }
}

/// D40's checkpoint, row by row (IR-2): the guest's result, and the code the
/// posthook checkpoint records. Mutation "interrupted dropped": the fatal row
/// returns no code.
#[tokio::test]
async fn under_the_gate_the_restart_checkpoint_gives_each_row_its_result() {
    use reverie::syscalls::Pause;
    let pause = Syscall::from(Pause::new());
    let code = |errno: Errno| Err::<i64, Error>(errno.into());
    let errno_of = |result: &Result<i64, Error>| match result {
        Err(Error::Errno(errno)) => Some(*errno),
        _ => None,
    };
    // name, flag, sa_flags, result, guest errno, interrupted code, refused
    type Case = (
        &'static str,
        Option<WaitSignalEnd>,
        u64,
        Errno,
        Option<Errno>,
        Option<Errno>,
        bool,
    );
    let cases: Vec<Case> = vec![
        (
            "fatal, ERESTARTNOHAND",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::EINTR),
            Some(Errno::ERESTARTNOHAND),
            false,
        ),
        (
            "fatal, ERESTARTSYS",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::ERESTARTSYS,
            Some(Errno::EINTR),
            Some(Errno::ERESTARTSYS),
            false,
        ),
        (
            "caught, ERESTARTNOHAND",
            Some(WaitSignalEnd::Caught(libc::SIGALRM)),
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::EINTR),
            None,
            false,
        ),
        (
            "caught, ERESTARTSYS without SA_RESTART",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            0,
            Errno::ERESTARTSYS,
            Some(Errno::EINTR),
            None,
            false,
        ),
        (
            "caught, ERESTARTSYS with SA_RESTART: the code is kept for step 3c.2's rewind",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            libc::SA_RESTART as u64,
            Errno::ERESTARTSYS,
            Some(Errno::ERESTARTSYS),
            None,
            false,
        ),
        (
            "no flag, ERESTARTSYS: Reverie's re-run (https://github.com/rrnewton/reverie/issues/362)",
            None,
            0,
            Errno::ERESTARTSYS,
            Some(Errno::ERESTARTSYS),
            None,
            false,
        ),
        (
            "no flag, ERESTARTNOHAND: refused",
            None,
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::ERESTARTNOHAND),
            None,
            true,
        ),
        (
            "no flag, ERESTARTNOINTR: refused",
            None,
            0,
            Errno::ERESTARTNOINTR,
            Some(Errno::ERESTARTNOINTR),
            None,
            true,
        ),
        (
            "no flag, ERESTART_RESTARTBLOCK: refused",
            None,
            0,
            Errno::ERESTART_RESTARTBLOCK,
            Some(Errno::ERESTART_RESTARTBLOCK),
            None,
            true,
        ),
        (
            "fatal, EINTR already: unchanged",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::EINTR,
            Some(Errno::EINTR),
            None,
            false,
        ),
    ];
    for (name, flag, sa_flags, result, guest_errno, interrupted, refused) in cases {
        let mut guest = turn_guest(true, Some(0));
        guest.action_flags = sa_flags;
        guest.thread.wait_signal_end = flag;
        let tool = tool(&guest);
        let (res, code_out) = tool
            .wait_end_checkpoint(&mut guest, &pause, code(result))
            .await;
        assert_eq!(errno_of(&res), guest_errno, "{name}");
        assert_eq!(code_out, interrupted, "{name}");
        assert_eq!(
            guest.thread.wait_signal_end, None,
            "{name}: the flag is taken"
        );
        let filed = guest.rpcs().iter().any(|rpc| {
            matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_RESTART_CODE_WITHOUT_SIGNAL"))
        });
        assert_eq!(filed, refused, "{name}");
    }
}

/// With the gate off the checkpoint changes nothing and refuses nothing,
/// whatever the result.
#[tokio::test]
async fn with_the_gate_off_the_restart_checkpoint_is_the_identity() {
    let pause = Syscall::from(reverie::syscalls::Pause::new());
    for result in [
        Errno::ERESTARTSYS,
        Errno::ERESTARTNOINTR,
        Errno::ERESTARTNOHAND,
        Errno::ERESTART_RESTARTBLOCK,
        Errno::EINTR,
    ] {
        let mut guest = turn_guest(false, None);
        let tool = tool(&guest);
        let (res, code) = tool
            .wait_end_checkpoint(&mut guest, &pause, Err(result.into()))
            .await;
        assert!(matches!(res, Err(Error::Errno(errno)) if errno == result));
        assert_eq!(code, None);
        assert!(guest.rpcs().is_empty());
    }
}

/// IR-2's observers for a `pause` a default-fatal signal ends before its park,
/// through the whole syscall pipeline (`handle_syscall_event`): the backend
/// and `delivery_interruption` see `-EINTR`, no sleep request is filed, and the
/// posthook happens-before anchor fires as interrupted with the kernel's
/// `ERESTARTNOHAND`, because the guest never resumes. Mutations: the
/// checkpoint moved to `complete_syscall` (the backend, `delivery_interruption`
/// and DETLOG would see a raw 514), and `interrupted` dropped (the anchor
/// would file a plain checkpoint).
#[tokio::test]
async fn under_the_gate_a_pause_ended_by_a_fatal_signal_shows_eintr_to_every_observer() {
    use detcore_model::happens_before::HappensBeforeSpec;
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(true, Some(0));
    guest.config.happens_before = Some(
        HappensBeforeSpec::from_json(&format!(
            r#"{{"version": 1, "events": {{"p": {{"thread": "{}", "syscall": "pause",
                                         "phase": "posthook"}}}}}}"#,
            THREAD.as_raw()
        ))
        .unwrap()
        .normalize()
        .unwrap(),
    );
    let tool = <Detcore as Tool>::new(Pid::from_raw(THREAD.as_raw()), &guest.config);
    let result = tool
        .handle_syscall_event(&mut guest, Syscall::from(reverie::syscalls::Pause::new()))
        .await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::EINTR))),
        "{result:?}"
    );
    assert_eq!(
        guest.thread.delivery_interruption,
        Some(reverie::Interruption::Return {
            syscall: libc::SYS_pause,
            result: -i64::from(libc::EINTR),
        })
    );
    let requests = guest.requests();
    assert!(
        !requests.iter().any(|request| request
            .resources
            .keys()
            .any(|rid| matches!(rid, ResourceID::SleepUntil(_)))),
        "the pause never parked: {requests:?}"
    );
    let interrupted: Vec<_> = requests
        .iter()
        .flat_map(|request| request.resources.keys().cloned())
        .filter(|rid| {
            matches!(
                rid,
                ResourceID::HappensBeforePosthookInterrupted { .. }
                    | ResourceID::HappensBeforeSyscallCheckpoint { .. }
            )
        })
        .collect();
    assert_eq!(
        interrupted,
        [ResourceID::HappensBeforePosthookInterrupted {
            count: 1,
            anchors: vec!["p".to_owned()],
            code: "ERESTARTNOHAND".to_owned(),
        }]
    );
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// Registers descriptor `fd` of `guest` as a pipe Hermit made physically
/// nonblocking, with open file `id`.
fn add_pipe(guest: &mut TurnGuest, fd: i32, id: crate::types::OpenFileId) {
    let pipe = crate::fd::DetFd::new(fd, nix::fcntl::OFlag::O_WRONLY, crate::fd::FdType::Pipe, id);
    pipe.set_physically_nonblocking();
    guest
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .file_handles
        .insert(fd, pipe);
}

/// D50 reader 1's waiting side, a parked pipe write, scalar and vectored:
/// under the gate it installs no private mask (no `rt_sigprocmask` is
/// injected), and once an attempt would wait for room, a deliverable signal
/// ends it, with `ERESTARTSYS` before any byte and with the count after
/// some, and records the end; otherwise it waits on and completes.
/// Mutations: "SigBlk used" (the kernel mask blocks everything) and "the
/// private mask kept" (the `rt_sigprocmask` assertion).
#[tokio::test]
async fn under_the_gate_a_pipe_write_ends_where_it_would_wait_only_for_a_deliverable_signal() {
    let id = crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 77);
    for vectored in [false, true] {
        for (first, ended) in [
            (Err(Errno::EAGAIN), Err(Errno::ERESTARTSYS)),
            (Ok(8), Ok(8)),
        ] {
            // A vectored write this small is atomic: an attempt writes all of
            // it or nothing, so it never has a partial count to return.
            if vectored && first.is_ok() {
                continue;
            }
            for row in rows() {
                let _seam = script(row.state);
                let mut guest = turn_guest(true, Some(row.own));
                add_pipe(&mut guest, 4, id);
                let rest = if first.is_ok() { Ok(8) } else { Ok(16) };
                guest.write_results = VecDeque::from([first, rest]);
                let tool = tool(&guest);
                let data = guest.stack + 2048;
                let result = if vectored {
                    let iov = guest.stack + 1024;
                    LocalMemory::new()
                        .write_value(AddrMut::<[usize; 2]>::from_raw(iov).unwrap(), &[data, 16])
                        .unwrap();
                    let call = reverie::syscalls::Writev::new()
                        .with_fd(4)
                        .with_iov(Addr::from_raw(iov))
                        .with_len(1);
                    tool.execute_blocking_pipe_writev(&mut guest, call, id)
                        .await
                } else {
                    let call = reverie::syscalls::Write::new()
                        .with_fd(4)
                        .with_buf(Addr::from_raw(data))
                        .with_len(16);
                    tool.execute_blocking_pipe_write(&mut guest, call, id).await
                };
                let name = format!("{} vectored={vectored} first={first:?}", row.name);
                assert_eq!(guest.thread.wait_signal_end, row.end, "{name}");
                assert!(
                    !guest.injected.contains(&Sysno::rt_sigprocmask),
                    "{name}: a private mask was installed: {:?}",
                    guest.injected
                );
                match (row.end, &ended, &result) {
                    (Some(_), Err(errno), Err(Error::Errno(got))) => {
                        assert_eq!(errno, got, "{name}")
                    }
                    (Some(_), Ok(count), Ok(got)) => assert_eq!(count, got, "{name}"),
                    (None, _, Ok(got)) => assert_eq!(*got, 16, "{name}: it waited on"),
                    _ => panic!("{name}: {result:?}"),
                }
            }
        }
    }
}

/// With the gate off a restartable poll keeps today's path: no gated check,
/// so a pending fatal signal does not end it, and no end is recorded.
#[tokio::test]
async fn with_the_gate_off_a_restartable_poll_keeps_today_s_path() {
    let _seam = script(proc_state(bit(libc::SIGTERM), 0, 0));
    let mut guest = turn_guest(false, Some(0));
    let pipe = crate::fd::DetFd::new(
        3,
        nix::fcntl::OFlag::O_RDONLY,
        crate::fd::FdType::Pipe,
        crate::types::OpenFileId::new(DetPid::from_raw(THREAD.as_raw()), 99),
    );
    pipe.set_physically_nonblocking();
    guest
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .file_handles
        .insert(3, pipe);
    guest.read_results = VecDeque::from([Err(Errno::EAGAIN), Ok(4)]);
    let mut poll = Resources::new(THREAD);
    poll.insert(ResourceID::InternalIOPolling, Permission::R);
    poll.fyi("read");
    let read = reverie::syscalls::Read::new()
        .with_fd(3)
        .with_buf(AddrMut::from_raw(guest.stack))
        .with_len(8);
    let result = crate::syscalls::helpers::retry_nonblocking_syscall::<
        crate::record_or_replay::NoopTool,
        _,
        _,
    >(&mut guest, read, poll, None)
    .await;
    assert_eq!(result.unwrap(), 4);
    assert_eq!(guest.thread.wait_signal_end, None);
}

/// D41 with IR-5 and IR-14: under the gate, a default-fatal signal
/// deliverable when a kernel-run call begins means the call is not run. Each
/// family returns `ERESTARTNOINTR` with the end recorded as `Fatal` (the
/// checkpoint makes it `-EINTR`), before any guest-visible effect: no
/// scheduler request, no injection, and for a vfork parent neither
/// `clone_flags` nor `pending_vfork` set. Representative buffers: a read into
/// a read-only page (which, run, would fault), and an `rt_sigsuspend` with an
/// invalid mask size and a null mask (which, run, would be `EINVAL`).
/// Mutation "the check moved after the first guest-visible effect": the read
/// injects and faults, the vfork leaves `clone_flags` set.
#[tokio::test]
async fn under_the_gate_a_kernel_run_call_with_a_fatal_signal_at_its_entry_is_not_run() {
    let fatal = proc_state(bit(libc::SIGTERM), 0, 0);
    let readonly: &'static [u8; 16] = Box::leak(Box::new([0; 16]));
    let read = reverie::syscalls::Read::new()
        .with_fd(5)
        .with_buf(AddrMut::from_raw(readonly.as_ptr() as usize))
        .with_len(16);
    let sigsuspend = reverie::syscalls::RtSigsuspend::new()
        .with_mask(None)
        .with_sigsetsize(3);
    let vfork = reverie::syscalls::family::CloneFamily::Vfork(reverie::syscalls::Vfork::new());
    for family in ["background read", "rt_sigsuspend", "vfork"] {
        let _seam = script(fatal);
        let mut guest = turn_guest(true, Some(0));
        let tool = tool(&guest);
        let result = match family {
            "background read" => {
                tool.record_or_replay_blocking(&mut guest, read.into())
                    .await
            }
            "rt_sigsuspend" => tool.handle_rt_sigsuspend(&mut guest, sigsuspend).await,
            _ => tool.handle_clone_family(&mut guest, vfork).await,
        };
        assert!(
            matches!(result, Err(Error::Errno(Errno::ERESTARTNOINTR))),
            "{family}: {result:?}"
        );
        assert_eq!(
            guest.thread.wait_signal_end,
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            "{family}"
        );
        assert!(guest.rpcs().is_empty(), "{family}: {:?}", guest.rpcs());
        assert!(guest.injected.is_empty(), "{family}: {:?}", guest.injected);
        assert_eq!(guest.thread.clone_flags, None, "{family}");
        assert!(guest.thread.pending_vfork.is_none(), "{family}");
        // The checkpoint's row for it: -EINTR, interrupted.
        guest.thread.wait_signal_end = Some(WaitSignalEnd::Fatal(libc::SIGTERM));
        let (res, code) = tool
            .wait_end_checkpoint(
                &mut guest,
                &Syscall::from(read),
                Err(Errno::ERESTARTNOINTR.into()),
            )
            .await;
        assert!(matches!(res, Err(Error::Errno(Errno::EINTR))), "{family}");
        assert_eq!(code, Some(Errno::ERESTARTNOINTR), "{family}");
    }
}

/// D41 refuses only a default-fatal signal at entry: a caught one, a blocked
/// one, or the gate off, and the background read is filed and run, here
/// faulting on its read-only buffer as the kernel would.
#[tokio::test]
async fn a_kernel_run_call_runs_when_no_default_fatal_signal_is_deliverable_at_its_entry() {
    let readonly: &'static [u8; 16] = Box::leak(Box::new([0; 16]));
    let read = reverie::syscalls::Read::new()
        .with_fd(5)
        .with_buf(AddrMut::from_raw(readonly.as_ptr() as usize))
        .with_len(16);
    for (name, gate, state, own) in [
        (
            "caught",
            true,
            proc_state(bit(libc::SIGUSR1), bit(libc::SIGUSR1), 0),
            0,
        ),
        (
            "blocked",
            true,
            proc_state(bit(libc::SIGTERM), 0, 0),
            bit(libc::SIGTERM),
        ),
        ("gate off", false, proc_state(bit(libc::SIGTERM), 0, 0), 0),
    ] {
        let _seam = script(state);
        let mut guest = turn_guest(gate, Some(own));
        guest.read_results = VecDeque::from([Err(Errno::EFAULT)]);
        let tool = tool(&guest);
        let result = tool
            .record_or_replay_blocking(&mut guest, read.into())
            .await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::EFAULT))),
            "{name}: {result:?}"
        );
        assert_eq!(guest.injected, [Sysno::read], "{name}");
        let filed: Vec<_> = guest
            .requests()
            .iter()
            .flat_map(|request| request.resources.keys().cloned())
            .collect();
        assert!(
            matches!(
                filed.as_slice(),
                [
                    ResourceID::BlockingExternalIO(_),
                    ResourceID::BlockedExternalContinue(_)
                ]
            ),
            "{name}: {filed:?}"
        );
        assert_eq!(guest.thread.wait_signal_end, None, "{name}");
    }
}

/// D44(b), one refusal per kernel-run resource: a send to a thread the
/// scheduler's committed state shows blocked in a background external call,
/// an `rt_sigsuspend` (under its temporary mask) or a vfork parent's wait,
/// of a signal that would interrupt it, files exactly one
/// `HERMIT_KERNEL_RUN_SIGNAL_REFUSED` stopping refusal, in the sender's turn,
/// and gets `EPERM`; nothing is sent. Not refused, and sent: a caught or
/// core-dumping signal to a vfork parent, a signal the call's mask blocks, an
/// ignored one, and any to a thread not so blocked. Unreadable dispositions
/// refuse, fail closed. SIGKILL and signal 0 never ask, nor does any send
/// with the gate off. The run-wide exit the refusal takes under the gate is
/// pinned by `stopping_refusal_tests`.
#[tokio::test]
async fn under_the_gate_a_send_to_a_kernel_run_call_is_refused_once_or_sent() {
    use crate::scheduler::KernelRunBlock;
    let usr1 = bit(libc::SIGUSR1);
    let term = bit(libc::SIGTERM);
    let external = KernelRunBlock {
        vfork: false,
        mask: Some(0),
    };
    let sigsuspend = KernelRunBlock {
        vfork: false,
        mask: Some(term),
    };
    let vfork = KernelRunBlock {
        vfork: true,
        mask: Some(0),
    };
    let readable = |caught| Ok(proc_state(0, caught, bit(libc::SIGHUP)));
    // name, what the scheduler holds, signal, the target's status, refused
    type Case = (
        &'static str,
        Option<KernelRunBlock>,
        i32,
        Result<KernelSignalState, Errno>,
        bool,
    );
    let cases: Vec<Case> = vec![
        (
            "external, default-fatal",
            Some(external),
            libc::SIGTERM,
            readable(0),
            true,
        ),
        (
            "external, caught",
            Some(external),
            libc::SIGUSR1,
            readable(usr1),
            true,
        ),
        (
            "rt_sigsuspend, unblocked by its temporary mask",
            Some(sigsuspend),
            libc::SIGUSR1,
            readable(usr1),
            true,
        ),
        (
            "rt_sigsuspend, blocked by its temporary mask",
            Some(sigsuspend),
            libc::SIGTERM,
            readable(0),
            false,
        ),
        (
            "vfork, default-fatal",
            Some(vfork),
            libc::SIGTERM,
            readable(0),
            true,
        ),
        (
            "vfork, caught",
            Some(vfork),
            libc::SIGUSR1,
            readable(usr1),
            false,
        ),
        (
            "vfork, core-dumping",
            Some(vfork),
            libc::SIGQUIT,
            readable(0),
            false,
        ),
        (
            "external, SIG_IGN",
            Some(external),
            libc::SIGHUP,
            readable(0),
            false,
        ),
        (
            "not blocked in a kernel-run call",
            None,
            libc::SIGTERM,
            readable(0),
            false,
        ),
        (
            "external, unreadable",
            Some(external),
            libc::SIGUSR2,
            Err(Errno::ESRCH),
            true,
        ),
    ];
    for (name, block, signal, peer, refused) in cases {
        let _seam = script_both(proc_state(0, 0, 0), peer);
        let mut guest = turn_guest(true, Some(0));
        guest.kernel_run = block;
        let tool = tool(&guest);
        let tgkill = reverie::syscalls::Tgkill::new()
            .with_tgid(PEER.as_raw())
            .with_tid(PEER.as_raw())
            .with_sig(signal);
        let result = tool
            .send_guest_signal(
                &mut guest,
                tgkill,
                PEER,
                signal,
                Some(DetPid::from_raw(PEER.as_raw())),
                true,
                PEER,
            )
            .await;
        let refusals = guest
            .rpcs()
            .iter()
            .filter(|rpc| {
                matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                if reason.contains("HERMIT_KERNEL_RUN_SIGNAL_REFUSED"))
            })
            .count();
        if refused {
            assert!(
                matches!(result, Err(Error::Errno(Errno::EPERM))),
                "{name}: {result:?}"
            );
            assert_eq!(refusals, 1, "{name}");
            assert!(guest.injected.is_empty(), "{name}: nothing sent");
        } else {
            assert_eq!(result.unwrap(), 0, "{name}");
            assert_eq!(refusals, 0, "{name}");
            assert_eq!(guest.injected, [Sysno::tgkill], "{name}: sent");
        }
    }
    // SIGKILL, signal 0 and the gate off never ask the scheduler.
    for (gate, signal) in [(true, libc::SIGKILL), (true, 0), (false, libc::SIGTERM)] {
        let _seam = script_both(proc_state(0, 0, 0), readable(0));
        let mut guest = turn_guest(gate, Some(0));
        guest.kernel_run = Some(external);
        let tool = tool(&guest);
        let tgkill = reverie::syscalls::Tgkill::new()
            .with_tgid(PEER.as_raw())
            .with_tid(PEER.as_raw())
            .with_sig(signal);
        let _ = tool
            .send_guest_signal(
                &mut guest,
                tgkill,
                PEER,
                signal,
                Some(DetPid::from_raw(PEER.as_raw())),
                true,
                PEER,
            )
            .await;
        assert!(
            !guest
                .rpcs()
                .iter()
                .any(|rpc| matches!(rpc, GlobalRequest::KernelRunTarget(_))),
            "gate={gate} signal={signal}"
        );
    }
}

/// Collects the WARN messages that name `HERMIT_STOP_SIGNAL_SEND_REFUSED`.
#[derive(Clone, Default)]
struct WarnLog(std::sync::Arc<Mutex<Vec<String>>>);

struct MessageVisitor(Option<String>);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

impl tracing::Subscriber for WarnLog {
    // "Sometimes" for every callsite, and a WARN hint: tracing caches each
    // callsite's interest across every registered subscriber, so a test on
    // another thread of the same process that registers its own subscriber
    // could otherwise leave the WARN callsite cached as never, and the event
    // would not reach `event`.
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
            && message.contains("HERMIT_STOP_SIGNAL_SEND_REFUSED")
        {
            self.0.lock().unwrap().push(message);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// D48 with IR-4(b): under the gate a stop-signal send gets `EPERM` and the
/// named WARN, by each target form and to the sender itself: SIGSTOP always;
/// SIGTSTP, SIGTTIN and SIGTTOU while the target's action is `SIG_DFL` in its
/// `/proc` status, read in the sender's turn, and when that status cannot be
/// read (fail closed). Not refused: SIGTSTP the target ignores or catches,
/// SIGCONT, a target with no live thread (its ordinary answer), and every
/// send with the gate off.
#[tokio::test]
async fn under_the_gate_a_stop_signal_send_is_refused_with_eperm_and_a_warn() {
    use crate::syscalls::ReservedSendTarget;
    let tstp = bit(libc::SIGTSTP);
    let peer_target = |form: &str| match form {
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
    };
    let cases: Vec<(&str, i32, Result<KernelSignalState, Errno>, bool)> = vec![
        ("SIGSTOP", libc::SIGSTOP, Ok(proc_state(0, 0, 0)), true),
        (
            "SIGTSTP SIG_DFL",
            libc::SIGTSTP,
            Ok(proc_state(0, 0, 0)),
            true,
        ),
        (
            "SIGTTIN SIG_DFL",
            libc::SIGTTIN,
            Ok(proc_state(0, 0, 0)),
            true,
        ),
        (
            "SIGTTOU SIG_DFL",
            libc::SIGTTOU,
            Ok(proc_state(0, 0, 0)),
            true,
        ),
        (
            "SIGTSTP SIG_IGN",
            libc::SIGTSTP,
            Ok(proc_state(0, 0, tstp)),
            false,
        ),
        (
            "SIGTSTP caught",
            libc::SIGTSTP,
            Ok(proc_state(0, tstp, 0)),
            false,
        ),
        (
            "SIGTSTP, status unreadable",
            libc::SIGTSTP,
            Err(Errno::ESRCH),
            true,
        ),
        ("SIGCONT", libc::SIGCONT, Ok(proc_state(0, 0, 0)), false),
    ];
    for form in ["kill", "tkill", "tgkill", "pidfd", "self"] {
        for (name, signal, status, refused) in &cases {
            let status = *status;
            // The sender's own status, for the self row, is the peer's.
            let own = status.unwrap_or_else(|_| proc_state(0, 0, 0));
            let _seam = if form == "self" {
                signal_state_read_seam::install(move |_, tid| {
                    (tid.as_raw() == THREAD.as_raw()).then_some(status)
                })
            } else {
                script_both(own, status)
            };
            let log = WarnLog::default();
            let _subscriber = tracing::subscriber::set_default(log.clone());
            let mut guest = turn_guest(true, Some(0));
            let tool = tool(&guest);
            let result = tool
                .refuse_stop_signal_send(&mut guest, Sysno::kill, *signal, peer_target(form))
                .await;
            let warns = log.0.lock().unwrap().len();
            if *refused {
                assert!(
                    matches!(result, Some(Err(Error::Errno(Errno::EPERM)))),
                    "{form}: {name}: {result:?}"
                );
                assert_eq!(warns, 1, "{form}: {name}");
            } else {
                assert!(result.is_none(), "{form}: {name}: {result:?}");
                assert_eq!(warns, 0, "{form}: {name}");
            }
        }
    }
    // No live target keeps the ordinary path; the gate off refuses nothing.
    let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
    let mut guest = turn_guest(true, Some(0));
    guest.live = vec![THREAD];
    let tool = tool(&guest);
    assert!(
        tool.refuse_stop_signal_send(
            &mut guest,
            Sysno::kill,
            libc::SIGSTOP,
            ReservedSendTarget::Process(PEER.as_raw())
        )
        .await
        .is_none()
    );
    let mut guest = turn_guest(false, Some(0));
    assert!(
        tool.refuse_stop_signal_send(
            &mut guest,
            Sysno::kill,
            libc::SIGSTOP,
            ReservedSendTarget::Process(PEER.as_raw())
        )
        .await
        .is_none()
    );
    assert!(guest.rpcs().is_empty());
}

/// D48's hooks: each send handler (`kill`, `tkill`, `tgkill`,
/// `rt_sigqueueinfo`, `rt_tgsigqueueinfo`) refuses a SIGSTOP under the gate
/// before anything else reaches the scheduler or the kernel. (The
/// `pidfd_send_signal` hook sits beside its reserved-signal refusal and is
/// covered through `refuse_stop_signal_send`'s pidfd form above.)
#[tokio::test]
async fn under_the_gate_every_send_handler_refuses_a_sigstop() {
    let _seam = script_both(proc_state(0, 0, 0), Ok(proc_state(0, 0, 0)));
    let peer = PEER.as_raw();
    for handler in [
        "kill",
        "tkill",
        "tgkill",
        "rt_sigqueueinfo",
        "rt_tgsigqueueinfo",
    ] {
        let mut guest = turn_guest(true, Some(0));
        let tool = tool(&guest);
        let result = match handler {
            "kill" => {
                tool.handle_kill(
                    &mut guest,
                    reverie::syscalls::Kill::new()
                        .with_pid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            "tkill" => {
                tool.handle_tkill(
                    &mut guest,
                    reverie::syscalls::Tkill::new()
                        .with_tid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            "tgkill" => {
                tool.handle_tgkill(
                    &mut guest,
                    reverie::syscalls::Tgkill::new()
                        .with_tgid(peer)
                        .with_tid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            "rt_sigqueueinfo" => {
                tool.handle_rt_sigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtSigqueueinfo::new()
                        .with_tgid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
            _ => {
                tool.handle_rt_tgsigqueueinfo(
                    &mut guest,
                    reverie::syscalls::RtTgsigqueueinfo::new()
                        .with_tgid(peer)
                        .with_tid(peer)
                        .with_sig(libc::SIGSTOP),
                )
                .await
            }
        };
        assert!(
            matches!(result, Err(Error::Errno(Errno::EPERM))),
            "{handler}: {result:?}"
        );
        assert!(guest.injected.is_empty(), "{handler}: {:?}", guest.injected);
        assert!(
            guest
                .rpcs()
                .iter()
                .all(|rpc| matches!(rpc, GlobalRequest::ResolveKillTargets(_))),
            "{handler}: {:?}",
            guest.rpcs()
        );
    }
}
