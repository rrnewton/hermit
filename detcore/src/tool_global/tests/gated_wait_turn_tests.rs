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
    /// `sa_mask` an injected `rt_sigaction` query reports, and whether the
    /// query fails instead.
    action_mask: u64,
    action_query_fails: bool,
    /// Every injected syscall, in order.
    injected: Vec<Sysno>,
    /// What each injected `read` returns, in order (`EAGAIN` once empty).
    read_results: VecDeque<Result<i64, Errno>>,
    /// What each injected `write` or `writev` returns, in order (`EAGAIN`
    /// once empty).
    write_results: VecDeque<Result<i64, Errno>>,
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
            Syscall::RtSigaction(query) => {
                if self.action_query_fails {
                    return Err(Errno::EFAULT);
                }
                if let Some(old) = query.old_action() {
                    let mut action = [0_u64; 4];
                    action[1] = self.action_flags;
                    action[3] = self.action_mask;
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
        action_mask: 0,
        action_query_fails: false,
        injected: Vec::new(),
        read_results: VecDeque::new(),
        write_results: VecDeque::new(),
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
/// lower-numbered one); a shared core-dumping one that SIGALRM precedes is
/// `BehindCaught`, which the restart checkpoint resolves by SIGALRM's mask
/// (`under_the_gate_the_restart_checkpoint_resolves_an_end_behind_a_caught_signal`).
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
    // A shared SIGXCPU behind the virtual SIGALRM.
    let _seam = script(shared(bit(libc::SIGXCPU)));
    let mut guest = turn_guest(true, Some(0));
    guest.thread.sigalrm_handled = true;
    let verdict = crate::syscalls::sleep_signal(&mut guest, wake.clone(), true)
        .await
        .unwrap();
    assert_eq!(verdict, SleepSignal::Pending);
    assert!(
        matches!(
            guest.thread.wait_signal_end,
            Some(WaitSignalEnd::BehindCaught(_))
        ),
        "{:?}",
        guest.thread.wait_signal_end
    );
    // Not a phase-1 process: nothing pending, nothing ends it.
    let _seam = script(proc_state(0, 0, 0));
    let mut guest = turn_guest(true, Some(0));
    let verdict = crate::syscalls::sleep_signal(&mut guest, wake, true)
        .await
        .unwrap();
    assert_eq!(verdict, SleepSignal::None);
}

/// The restart checkpoint resolves a `BehindCaught` end (Claude review round
/// 2): phase 1's virtual SIGALRM wakes a `pause` with a shared SIGXCPU
/// deliverable behind it, and the checkpoint reads SIGALRM's action with one
/// query `rt_sigaction`. (a) Its `sa_mask` holds SIGXCPU: `Caught(SIGALRM)`,
/// the restart table's `EINTR` and a plain checkpoint. (b) It does not:
/// `Fatal(SIGXCPU)`, `-EINTR` and the interrupted code. (e) The query fails:
/// fail closed, the stopping refusal HERMIT_CAUGHT_ACTION_UNREADABLE is filed
/// and the end is `Fatal`. Mutations: "the handler's sa_mask ignored" ((a)
/// gives the interrupted code), "no fail-closed" ((e) files nothing).
#[tokio::test]
async fn under_the_gate_the_restart_checkpoint_resolves_an_end_behind_a_caught_signal() {
    use crate::syscalls::SleepCheck;
    let wake = SleepCheck::AfterWake(Some(vec![SigWrapper(libc::SIGALRM)]));
    let xcpu = bit(libc::SIGXCPU);
    let state = KernelSignalState {
        thread_pending: 0,
        shared_pending: xcpu,
        ..proc_state(xcpu, 0, 0)
    };
    let pause = Syscall::from(reverie::syscalls::Pause::new());
    for (name, mask, fails, interrupted, refused) in [
        ("(a) SIGALRM's mask holds SIGXCPU", xcpu, false, None, false),
        (
            "(b) SIGALRM's mask does not hold SIGXCPU",
            0,
            false,
            Some(Errno::ERESTARTNOHAND),
            false,
        ),
        (
            "(e) the action cannot be read",
            xcpu,
            true,
            Some(Errno::ERESTARTNOHAND),
            true,
        ),
    ] {
        let _seam = script(state);
        let mut guest = turn_guest(true, Some(0));
        guest.thread.sigalrm_handled = true;
        guest.action_mask = mask;
        guest.action_query_fails = fails;
        let tool = tool(&guest);
        crate::syscalls::sleep_signal(&mut guest, wake.clone(), true)
            .await
            .unwrap();
        let (res, code) = tool
            .wait_end_checkpoint(&mut guest, &pause, Err(Errno::ERESTARTNOHAND.into()))
            .await;
        assert!(
            matches!(res, Err(Error::Errno(Errno::EINTR))),
            "{name}: {res:?}"
        );
        assert_eq!(code, interrupted, "{name}");
        assert_eq!(guest.injected, [Sysno::rt_sigaction], "{name}");
        let refusals = guest
            .rpcs()
            .iter()
            .filter(|rpc| {
                matches!(rpc, GlobalRequest::StoppingRefusal(reason)
                    if reason.contains("HERMIT_CAUGHT_ACTION_UNREADABLE"))
            })
            .count();
        assert_eq!(refusals, usize::from(refused), "{name}");
    }
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
/// posthook checkpoint records. Mutations: "interrupted dropped" (the fatal
/// row returns no code), "the restart-after-handler refusal removed" (the
/// caught ERESTARTNOINTR row returns the raw code and files nothing).
#[tokio::test]
async fn under_the_gate_the_restart_checkpoint_gives_each_row_its_result() {
    use reverie::syscalls::Pause;
    let pause = Syscall::from(Pause::new());
    let code = |errno: Errno| Err::<i64, Error>(errno.into());
    let errno_of = |result: &Result<i64, Error>| match result {
        Err(Error::Errno(errno)) => Some(*errno),
        _ => None,
    };
    // name, flag, sa_flags, result, guest errno, interrupted code, the
    // stopping refusal filed
    type Case = (
        &'static str,
        Option<WaitSignalEnd>,
        u64,
        Errno,
        Option<Errno>,
        Option<Errno>,
        Option<&'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "fatal, ERESTARTNOHAND",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::EINTR),
            Some(Errno::ERESTARTNOHAND),
            None,
        ),
        (
            "fatal, ERESTARTSYS",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::ERESTARTSYS,
            Some(Errno::EINTR),
            Some(Errno::ERESTARTSYS),
            None,
        ),
        (
            "caught, ERESTARTNOHAND",
            Some(WaitSignalEnd::Caught(libc::SIGALRM)),
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::EINTR),
            None,
            None,
        ),
        (
            "caught, ERESTARTSYS without SA_RESTART",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            0,
            Errno::ERESTARTSYS,
            Some(Errno::EINTR),
            None,
            None,
        ),
        (
            "caught, ERESTARTSYS with SA_RESTART: the code is kept for step 3c.2's rewind",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            libc::SA_RESTART as u64,
            Errno::ERESTARTSYS,
            Some(Errno::ERESTARTSYS),
            None,
            None,
        ),
        (
            "caught, ERESTARTNOINTR: the table restarts, which 3c.1 cannot honour: refused",
            Some(WaitSignalEnd::Caught(libc::SIGUSR1)),
            0,
            Errno::ERESTARTNOINTR,
            Some(Errno::EINTR),
            None,
            Some("HERMIT_RESTART_AFTER_HANDLER"),
        ),
        (
            "no flag, ERESTARTSYS: Reverie's re-run (https://github.com/rrnewton/reverie/issues/362)",
            None,
            0,
            Errno::ERESTARTSYS,
            Some(Errno::ERESTARTSYS),
            None,
            None,
        ),
        (
            "no flag, ERESTARTNOHAND: refused",
            None,
            0,
            Errno::ERESTARTNOHAND,
            Some(Errno::ERESTARTNOHAND),
            None,
            Some("HERMIT_RESTART_CODE_WITHOUT_SIGNAL"),
        ),
        (
            "no flag, ERESTARTNOINTR: refused",
            None,
            0,
            Errno::ERESTARTNOINTR,
            Some(Errno::ERESTARTNOINTR),
            None,
            Some("HERMIT_RESTART_CODE_WITHOUT_SIGNAL"),
        ),
        (
            "no flag, ERESTART_RESTARTBLOCK: refused",
            None,
            0,
            Errno::ERESTART_RESTARTBLOCK,
            Some(Errno::ERESTART_RESTARTBLOCK),
            None,
            Some("HERMIT_RESTART_CODE_WITHOUT_SIGNAL"),
        ),
        (
            "fatal, EINTR already: unchanged",
            Some(WaitSignalEnd::Fatal(libc::SIGTERM)),
            0,
            Errno::EINTR,
            Some(Errno::EINTR),
            None,
            None,
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
        let filed: Vec<&str> = guest
            .rpcs()
            .iter()
            .filter_map(|rpc| match rpc {
                GlobalRequest::StoppingRefusal(reason) => [
                    "HERMIT_RESTART_CODE_WITHOUT_SIGNAL",
                    "HERMIT_RESTART_AFTER_HANDLER",
                ]
                .into_iter()
                .find(|code| reason.contains(code)),
                _ => None,
            })
            .collect();
        assert_eq!(filed, Vec::from_iter(refused), "{name}");
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
