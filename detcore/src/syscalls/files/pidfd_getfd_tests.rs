/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exercise the real handler's refusal boundary, record/replay delegation,
//! and descriptor capture. Only the backend's injected syscall is scripted.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;

use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::SyscallArgs;

use super::*;
use crate::Config;
use crate::GlobalState;
use crate::ThreadState;

const PIDFD: RawFd = 70;
const SOURCE: RawFd = 71;
const DUPLICATE: RawFd = 72;

struct PidfdGuest {
    config: Config,
    thread: ThreadState<()>,
    injected_ids: (i64, i64),
    injected: Vec<Sysno>,
    pidfd_calls: Vec<[usize; 3]>,
    result: Result<i64, Errno>,
    rebind_source_on_injection: bool,
}

struct UnusedStack;
struct UnusedStackGuard;
impl Drop for UnusedStackGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for UnusedStack {
    type StackGuard = UnusedStackGuard;
    fn size(&self) -> usize {
        panic!("pidfd_getfd needs no scratch stack")
    }
    fn capacity(&self) -> usize {
        panic!("pidfd_getfd needs no scratch stack")
    }
    fn push<'a, T>(&mut self, _: T) -> Addr<'a, T> {
        panic!("pidfd_getfd needs no scratch stack")
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("pidfd_getfd needs no scratch stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("pidfd_getfd needs no scratch stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for PidfdGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        panic!("these pidfd_getfd cases need no RPC: {:?}", message.2)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for PidfdGuest {
    type Memory = LocalMemory;
    type Stack = UnusedStack;
    fn pid(&self) -> Pid {
        Pid::from_raw(501)
    }
    fn tid(&self) -> Pid {
        Pid::from_raw(509)
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> LocalMemory {
        panic!("pidfd_getfd needs no guest memory")
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("pidfd_getfd needs no registers")
    }
    async fn stack(&mut self) -> UnusedStack {
        panic!("pidfd_getfd needs no scratch stack")
    }
    async fn daemonize(&mut self) {
        panic!("pidfd_getfd must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        let (number, args) = syscall.into_parts();
        self.injected.push(number);
        match number {
            Sysno::getpid => Ok(self.injected_ids.0),
            Sysno::gettid => Ok(self.injected_ids.1),
            Sysno::pidfd_getfd => {
                self.pidfd_calls.push([args.arg0, args.arg1, args.arg2]);
                if args.arg2 == 0 {
                    assert_eq!(
                        self.thread
                            .with_detfd(SOURCE, |fd| fd.open_file_alias_count())
                            .unwrap(),
                        2,
                        "the source must be captured before record/replay delegates"
                    );
                    if self.rebind_source_on_injection {
                        assert_eq!(self.thread.remove_fd(SOURCE), None);
                        self.thread
                            .add_fd(SOURCE, OFlag::empty(), FdType::Regular, None)
                            .unwrap();
                    }
                }
                self.result
            }
            other => panic!("unexpected injection: {other:?}"),
        }
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("pidfd_getfd must return through the handler")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("pidfd_getfd must not replace the timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("pidfd_getfd must not replace the timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("pidfd_getfd needs no PMU clock")
    }
}

fn setup(pid: i32, tid: i32, nested: bool, strict: bool) -> (Detcore, PidfdGuest) {
    let config = Config {
        sequentialize_threads: true,
        panic_on_unsupported_syscalls: strict,
        exit_on_unsupported_syscall: true,
        shutdown_on_unsupported_syscall: false,
        ..Config::default()
    }
    .with_backend(|backend| backend.tool_shares_guest_descriptor_table = false);
    let mut thread = ThreadState::new(DetTid::from_raw(tid), &config, ());
    thread.detpid = Some(DetPid::from_raw(pid));
    thread.physical_tid = Some(509);
    thread.nested_pid_namespace = nested;
    thread
        .add_fd(PIDFD, OFlag::O_CLOEXEC, FdType::Pidfd, None)
        .unwrap();
    thread
        .add_fd(SOURCE, OFlag::empty(), FdType::Regular, None)
        .unwrap();
    // A copied child may keep this Tool identity; its current ThreadState is
    // the source of its container identity. Physical identities differ above.
    let tool = Detcore::new(Pid::from_raw(3), &config);
    (
        tool,
        PidfdGuest {
            config,
            thread,
            // The refusal cases use matching injected and shared identities;
            // physical backend identities remain different.
            injected_ids: (i64::from(pid), i64::from(tid)),
            injected: Vec::new(),
            pidfd_calls: Vec::new(),
            result: Ok(i64::from(DUPLICATE)),
            rebind_source_on_injection: false,
        },
    )
}

fn call(pidfd: RawFd, targetfd: RawFd, flags: u32) -> Syscall {
    Syscall::Other(
        Sysno::pidfd_getfd,
        SyscallArgs::new(pidfd as usize, targetfd as usize, flags as usize, 0, 0, 0),
    )
}

#[derive(Clone, Default)]
struct ErrorMessages(Arc<Mutex<Vec<String>>>);
impl tracing::Subscriber for ErrorMessages {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() != tracing::Level::ERROR {
            return;
        }
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    write!(self.0, "{value:?}").unwrap();
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push(message.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn unsupported_target_uses_named_refusal_policy() {
    // A foreign target, a nonleader's own TGID, and an unknown target all
    // lack proof that targetfd belongs to this exact descriptor table.
    for (pid, tid, target) in [(3, 3, Some(9)), (3, 7, Some(3)), (3, 3, None)] {
        for nested in [false, true] {
            for strict in [true, false] {
                let (tool, mut guest) = setup(pid, tid, nested, strict);
                if let Some(target) = target {
                    guest
                        .thread
                        .with_detfd(PIDFD, |fd| fd.set_pidfd_target(DetPid::from_raw(target)))
                        .unwrap();
                }
                let messages = ErrorMessages::default();
                let _capture = tracing::subscriber::set_default(messages.clone());
                let result = tool
                    .handle_pidfd_getfd(&mut guest, call(PIDFD, SOURCE, 0), PIDFD, SOURCE, 0)
                    .await;
                match result {
                    Err(Error::Tool(error)) if strict => assert_eq!(
                        error
                            .downcast_ref::<crate::UnsupportedSyscallError>()
                            .map(|error| error.0),
                        Some(Sysno::pidfd_getfd)
                    ),
                    Err(Error::Errno(errno)) if !strict => assert_eq!(errno, Errno::EOPNOTSUPP),
                    other => panic!(
                        "wrong target refusal: pid={pid}, tid={tid}, target={target:?}, nested={nested}, strict={strict}: {other:?}"
                    ),
                }
                let messages = messages.0.lock().unwrap();
                assert!(
                    messages
                        .iter()
                        .any(|message| message.contains("pidfd_getfd")
                            && message.contains("cross-task")
                            && message.contains("open-file-description")),
                    "the refusal must name the missing capability at ERROR: {messages:?}"
                );
                assert!(guest.pidfd_calls.is_empty());
                assert!(!guest.injected.contains(&Sysno::close));
                assert_eq!(
                    guest
                        .thread
                        .with_detfd(SOURCE, |fd| fd.open_file_alias_count()),
                    Ok(1)
                );
                assert_eq!(
                    guest.thread.with_detfd(DUPLICATE, |_| ()),
                    Err(Errno::EBADF)
                );
            }
        }
    }
}

#[tokio::test]
async fn descriptor_errors_and_reserved_flags_keep_linux_errno() {
    // A regular descriptor in the pidfd slot, either absent descriptor, and
    // a valid own pidfd with an absent target all retain ordinary EBADF.
    for (pidfd, targetfd) in [(SOURCE, SOURCE), (-1, SOURCE), (PIDFD, -1)] {
        let (tool, mut guest) = setup(3, 3, false, true);
        guest
            .thread
            .with_detfd(PIDFD, |fd| fd.set_pidfd_target(DetPid::from_raw(3)))
            .unwrap();
        let result = tool
            .handle_pidfd_getfd(&mut guest, call(pidfd, targetfd, 0), pidfd, targetfd, 0)
            .await;
        assert!(
            matches!(result, Err(Error::Errno(Errno::EBADF))),
            "{result:?}"
        );
        assert!(guest.pidfd_calls.is_empty());
    }
    // Linux's flags-first validation must delegate the original call, before
    // either getter or descriptor validation, for all four combinations.
    for pidfd in [PIDFD, -1] {
        for targetfd in [SOURCE, -1] {
            let (tool, mut guest) = setup(3, 7, false, true);
            guest.result = Err(Errno::EINVAL);
            let result = tool
                .handle_pidfd_getfd(&mut guest, call(pidfd, targetfd, 1), pidfd, targetfd, 1)
                .await;
            assert!(
                matches!(result, Err(Error::Errno(Errno::EINVAL))),
                "{result:?}"
            );
            assert_eq!(guest.injected, [Sysno::pidfd_getfd]);
            assert_eq!(guest.pidfd_calls, [[pidfd as usize, targetfd as usize, 1]]);
        }
    }
}

#[tokio::test]
async fn kernel_failure_remains_errno_and_abandons_capture() {
    for errno in [Errno::EOPNOTSUPP, Errno::EBADF] {
        let (tool, mut guest) = setup(3, 3, false, true);
        guest
            .thread
            .with_detfd(PIDFD, |fd| fd.set_pidfd_target(DetPid::from_raw(3)))
            .unwrap();
        guest.result = Err(errno);
        let messages = ErrorMessages::default();
        let _capture = tracing::subscriber::set_default(messages.clone());
        let result = tool
            .handle_pidfd_getfd(&mut guest, call(PIDFD, SOURCE, 0), PIDFD, SOURCE, 0)
            .await;
        assert!(
            matches!(result, Err(Error::Errno(actual)) if actual == errno),
            "{result:?}"
        );
        assert_eq!(guest.pidfd_calls, [[PIDFD as usize, SOURCE as usize, 0]]);
        assert_eq!(
            guest
                .thread
                .with_detfd(SOURCE, |fd| fd.open_file_alias_count()),
            Ok(1)
        );
        assert_eq!(
            guest.thread.with_detfd(DUPLICATE, |_| ()),
            Err(Errno::EBADF)
        );
        assert!(messages.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn supported_target_installs_the_captured_ofd() {
    for nested in [false, true] {
        let (tool, mut guest) = setup(3, 3, nested, true);
        guest
            .thread
            .with_detfd(PIDFD, |fd| fd.set_pidfd_target(DetPid::from_raw(3)))
            .unwrap();
        let source_id = guest
            .thread
            .with_detfd(SOURCE, |fd| fd.open_file_id())
            .unwrap();
        // Reusing the numeric source after the call starts must not make the
        // installed duplicate point at the replacement open file description.
        guest.rebind_source_on_injection = true;
        let result = tool
            .handle_pidfd_getfd(&mut guest, call(PIDFD, SOURCE, 0), PIDFD, SOURCE, 0)
            .await
            .unwrap();
        assert_eq!(result, i64::from(DUPLICATE));
        assert_eq!(guest.pidfd_calls, [[PIDFD as usize, SOURCE as usize, 0]]);
        assert_eq!(
            guest
                .thread
                .with_detfd(DUPLICATE, |fd| (fd.open_file_id(), fd.is_cloexec())),
            Ok((source_id, true))
        );
        assert_ne!(
            guest
                .thread
                .with_detfd(SOURCE, |fd| fd.open_file_id())
                .unwrap(),
            source_id
        );
    }
}
