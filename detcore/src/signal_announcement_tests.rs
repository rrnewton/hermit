/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The two-phase signal delivery (`Tool::announce_signal_event`,
//! `Tool::commit_signal_event`, `Tool::withdraw_signal_event`), through
//! Detcore's own implementation: the announcement takes the delivery's turn,
//! the commit takes none, and an announcement settles once.

use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::SignalAnnouncement;
use reverie::Stack;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallInfo;

use crate::Config;
use crate::Detcore;
use crate::GlobalState;
use crate::ThreadState;
use crate::resources::ResourceID;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::types::DetPid;

/// A thread whose scheduler grants every request at once and records, per
/// RPC, whether it was a resource request and for which resources.
struct AnnounceGuest {
    config: Config,
    thread: ThreadState<()>,
    requests: Mutex<Vec<Vec<ResourceID>>>,
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("a delivery needs no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("a delivery needs no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("a delivery needs no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("a delivery needs no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("a delivery needs no scratch stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for AnnounceGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let response = match message.2 {
            GlobalRequest::RequestResources(request, _) => {
                self.requests
                    .lock()
                    .unwrap()
                    .push(request.resources.keys().cloned().collect());
                GlobalResponse::RequestResources(crate::tool_global::ResumeStatus::Normal)
            }
            request => panic!("a delivery of SIGUSR1 sent an unexpected RPC: {request:?}"),
        };
        (None, response)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for AnnounceGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    // The test thread itself, so that what Detcore reads of the kernel's
    // signal state (`delivery_runs_handler`) is a real thread's: this one,
    // which catches neither SIGUSR1 nor SIGUSR2 nor signal 40.
    fn pid(&self) -> Pid {
        Pid::from_raw(std::process::id() as i32)
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(unsafe { libc::gettid() })
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
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("a delivery needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("a delivery needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("a delivery must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!("a delivery must not inject: {:?}", call.number())
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("a delivery must not tail-inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("no timer is configured")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("no timer is configured")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("no PMU clock is configured")
    }
}

/// Detcore and a thread of the in-guest backend, with no timeslice, as it runs.
fn setup() -> (Detcore, AnnounceGuest) {
    setup_with(|_| {})
}

fn setup_with(adjust: impl FnOnce(&mut Config)) -> (Detcore, AnnounceGuest) {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        ..Config::default()
    };
    // The in-guest backend's capabilities, the backend this protocol is for.
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    adjust(&mut config);
    // The test thread's own IDs (see `AnnounceGuest::tid`).
    let pid = std::process::id() as i32;
    let tid = unsafe { libc::gettid() };
    let mut thread = ThreadState::new(DetPid::from_raw(tid), &config, ());
    thread.detpid = Some(DetPid::from_raw(pid));
    let tool = <Detcore as Tool>::new(Pid::from_raw(tid), &config);
    (
        tool,
        AnnounceGuest {
            config,
            thread,
            requests: Mutex::new(Vec::new()),
        },
    )
}

fn turns(guest: &AnnounceGuest) -> Vec<Vec<ResourceID>> {
    guest.requests.lock().unwrap().clone()
}

/// The announcement files the delivery's `InboundSignal` request and does
/// nothing only a delivery justifies; the commit files no request, does that
/// (the sender's hold for the signal ends, the delivery is counted) and
/// delivers the announced signal; the announcement is then settled, so a
/// second commit of it is `EINVAL`.
#[tokio::test]
async fn announce_takes_the_turn_and_commit_delivers_without_one() {
    let (tool, mut guest) = setup();
    guest.thread.own_signal_turn_hold = Some(libc::SIGUSR1);
    let count = guest.thread.stats.signal_count;
    let announcement = tool
        .announce_signal_event(&mut guest, libc::SIGUSR1)
        .await
        .unwrap();
    assert_eq!(announcement.signal(), libc::SIGUSR1);
    let after_announce = turns(&guest);
    assert!(
        after_announce
            .iter()
            .any(|resources| resources.contains(&ResourceID::InboundSignal(
                crate::types::SigWrapper(libc::SIGUSR1)
            ))),
        "the announcement took no InboundSignal turn: {after_announce:?}"
    );
    assert_eq!(guest.thread.signal_announcements.len(), 1);
    assert_eq!(guest.thread.own_signal_turn_hold, Some(libc::SIGUSR1));
    assert_eq!(guest.thread.stats.signal_count, count);
    assert!(!guest.thread.delivered_since_request);

    let delivered = tool
        .commit_signal_event(&mut guest, announcement)
        .await
        .unwrap();
    assert_eq!(delivered, Some(libc::SIGUSR1));
    assert_eq!(
        turns(&guest),
        after_announce,
        "the commit filed a scheduler request"
    );
    assert!(guest.thread.signal_announcements.is_empty());
    assert_eq!(guest.thread.own_signal_turn_hold, None);
    assert_eq!(guest.thread.stats.signal_count, count + 1);
    // This thread does not catch SIGUSR1, so no handler continues.
    assert!(!guest.thread.delivered_since_request);
    assert_eq!(
        tool.commit_signal_event(&mut guest, announcement).await,
        Err(Errno::EINVAL)
    );
}

/// A withdrawn announcement is settled: it can no longer be committed, and
/// withdrawing it touches no other announcement.
#[tokio::test]
async fn withdraw_settles_only_its_own_announcement() {
    let (tool, mut guest) = setup();
    let first = tool
        .announce_signal_event(&mut guest, libc::SIGUSR1)
        .await
        .unwrap();
    let second = tool
        .announce_signal_event(&mut guest, libc::SIGUSR2)
        .await
        .unwrap();
    assert_ne!(first.token(), second.token());
    guest.thread.own_signal_turn_hold = Some(libc::SIGUSR1);
    let count = guest.thread.stats.signal_count;
    // A withdrawal with the right token but another signal is not this one.
    tool.withdraw_signal_event(
        &mut guest.thread,
        SignalAnnouncement::new(libc::SIGUSR2, first.token()),
    );
    assert_eq!(guest.thread.signal_announcements.len(), 2);
    tool.withdraw_signal_event(&mut guest.thread, first);
    assert_eq!(guest.thread.signal_announcements.len(), 1);
    // Nothing a delivery justifies happened for the withdrawn one.
    assert_eq!(guest.thread.own_signal_turn_hold, Some(libc::SIGUSR1));
    assert_eq!(guest.thread.stats.signal_count, count);
    assert_eq!(
        tool.commit_signal_event(&mut guest, first).await,
        Err(Errno::EINVAL)
    );
    assert_eq!(
        tool.commit_signal_event(&mut guest, second).await.unwrap(),
        Some(libc::SIGUSR2)
    );
    assert!(guest.thread.signal_announcements.is_empty());
}

/// An announcement whose signal does not match its token's is not this
/// thread's: `EINVAL`, and nothing is settled.
#[tokio::test]
async fn a_mismatched_announcement_is_refused() {
    let (tool, mut guest) = setup();
    let held = tool
        .announce_signal_event(&mut guest, libc::SIGUSR1)
        .await
        .unwrap();
    let forged = SignalAnnouncement::new(libc::SIGUSR2, held.token());
    assert_eq!(
        tool.commit_signal_event(&mut guest, forged).await,
        Err(Errno::EINVAL)
    );
    assert_eq!(guest.thread.signal_announcements.len(), 1);
}

/// A realtime signal, which `Signal` cannot name, goes through both halves.
#[tokio::test]
async fn a_realtime_signal_is_announced_and_committed() {
    let (tool, mut guest) = setup();
    let announcement = tool.announce_signal_event(&mut guest, 40).await.unwrap();
    assert_eq!(
        tool.commit_signal_event(&mut guest, announcement)
            .await
            .unwrap(),
        Some(40)
    );
}

/// A number that is not a signal is `EINVAL`, before any turn.
#[tokio::test]
async fn a_number_outside_1_to_64_is_refused_before_any_turn() {
    let (tool, mut guest) = setup();
    for signal in [0, 65, -1] {
        assert_eq!(
            tool.announce_signal_event(&mut guest, signal).await,
            Err(Errno::EINVAL)
        );
    }
    assert!(turns(&guest).is_empty());
    assert!(guest.thread.signal_announcements.is_empty());
}

/// Every configuration in which a commit could file a scheduler request, or
/// would read the thread's queues or the delivery's siginfo, is refused at
/// the announcement with `EOPNOTSUPP`, before any turn.
#[tokio::test]
async fn configurations_the_commit_cannot_keep_are_refused_before_any_turn() {
    let refused: [(&str, fn(&mut Config)); 4] = [
        ("discarded-at-send record", |config| {
            config.backend.supports_blocked_wait_signal_interruption = true;
        }),
        ("kernel child-exit signals", |config| {
            config.backend.reports_kernel_child_exit_signals = true;
        }),
        ("thread-directed process signals", |config| {
            config.backend.requires_thread_directed_process_signals = true;
        }),
        ("a timeslice", |config| {
            config.max_timeslice = std::num::NonZeroU64::new(1_000_000);
        }),
    ];
    for (name, adjust) in refused {
        let (tool, mut guest) = setup_with(adjust);
        assert!(
            super::two_phase_refusal(&guest.config).is_some(),
            "{name}: not refused"
        );
        assert_eq!(
            tool.announce_signal_event(&mut guest, libc::SIGUSR1).await,
            Err(Errno::EOPNOTSUPP),
            "{name}"
        );
        assert!(turns(&guest).is_empty(), "{name}: a turn was taken");
    }
    let (_, guest) = setup();
    assert_eq!(super::two_phase_refusal(&guest.config), None);
}
