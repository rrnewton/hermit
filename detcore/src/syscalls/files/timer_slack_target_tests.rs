/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Call the real read handlers on a pre-bound timer-slack descriptor. The
//! observer, identity classifier and memory copy run unchanged; only the
//! backend's fstatat and gettid answers are scripted.

use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::LocalMemory;

use super::*;
use crate::Config;
use crate::GlobalState;
use crate::ThreadState;

const FD: RawFd = 70;
const IDENTITY: (u64, u64) = (11, 22);
const SLACK: u64 = 987_654_321;
const TEXT: &[u8] = b"987654321\n";
const ARENA_WORDS: usize = 32;
const PATH_BYTES: usize = 64;

// As in helpers.rs's WaitStack, scratch lives in a guest-owned aligned arena.
// This observer uses one fixed path buffer followed by one stat buffer.
struct ObservationStack {
    arena: usize,
}

struct ObservationStackGuard;
impl Drop for ObservationStackGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for ObservationStack {
    type StackGuard = ObservationStackGuard;
    fn size(&self) -> usize {
        panic!("timer-slack observation does not query stack size")
    }
    fn capacity(&self) -> usize {
        panic!("timer-slack observation does not query stack capacity")
    }
    fn push<'a, T>(&mut self, value: T) -> Addr<'a, T> {
        assert!(std::mem::size_of::<T>() <= PATH_BYTES);
        assert!(std::mem::align_of::<T>() <= std::mem::align_of::<u64>());
        // SAFETY: the guest owns this live, aligned arena for the entire call;
        // the first PATH_BYTES bytes have enough space and alignment for T.
        unsafe { std::ptr::write(self.arena as *mut T, value) };
        Addr::from_raw(self.arena).unwrap()
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        assert!(std::mem::size_of::<T>() <= ARENA_WORDS * 8 - PATH_BYTES);
        assert!(std::mem::align_of::<T>() <= std::mem::align_of::<u64>());
        let address = self.arena + PATH_BYTES;
        // SAFETY: this disjoint, aligned region belongs to the live arena and
        // has room for T. Stack::reserve promises zeroed bytes, not a T value.
        unsafe { std::ptr::write_bytes(address as *mut u8, 0, std::mem::size_of::<T>()) };
        AddrMut::from_raw(address).unwrap()
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        Ok(ObservationStackGuard)
    }
}

struct TimerSlackGuest {
    config: Config,
    thread: ThreadState<()>,
    arena: Box<[u64; ARENA_WORDS]>,
    target: i32,
    observed: Result<(u64, u64), Errno>,
    visible_tid: Result<i64, Errno>,
    injected: Vec<Sysno>,
    stack_requests: usize,
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for TimerSlackGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        panic!("timer-slack reads must not add RPCs: {:?}", message.2)
    }
    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for TimerSlackGuest {
    type Memory = LocalMemory;
    type Stack = ObservationStack;
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
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("timer-slack reads need no registers")
    }
    async fn stack(&mut self) -> ObservationStack {
        self.stack_requests += 1;
        ObservationStack {
            arena: self.arena.as_mut_ptr() as usize,
        }
    }
    async fn daemonize(&mut self) {
        panic!("timer-slack reads must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        let (number, args) = syscall.into_parts();
        let path_address = args.arg1;
        self.injected.push(number);
        match Syscall::from_raw(number, args) {
            Syscall::Newfstatat(call) => {
                assert_eq!(call.dirfd(), libc::AT_FDCWD);
                assert_eq!(call.flags(), AtFlags::empty());
                assert_eq!(path_address, self.arena.as_ptr() as usize);
                let expected_path = format!("/proc/{}/timerslack_ns", self.target);
                let mut path = vec![0_u8; expected_path.len() + 1];
                self.memory()
                    .read_exact(Addr::from_raw(path_address).unwrap(), &mut path)?;
                assert_eq!(&path[..expected_path.len()], expected_path.as_bytes());
                assert_eq!(path[expected_path.len()], 0);
                let (device, inode) = self.observed?;
                // SAFETY: libc::stat consists of integer fields; all-zero is
                // a valid initial value for the fields this observer reads.
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                stat.st_dev = device;
                stat.st_ino = inode;
                self.memory().write_value(call.stat().unwrap().0, &stat)?;
                Ok(0)
            }
            Syscall::Gettid(_) => self.visible_tid,
            other => panic!("unexpected timer-slack injection: {other:?}"),
        }
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("timer-slack reads must return")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("timer-slack reads must not change the timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("timer-slack reads must not change the timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("timer-slack reads must not query the clock")
    }
}

fn setup(
    pid: i32,
    tid: i32,
    target: i32,
    nested: bool,
    flags: OFlag,
) -> (Detcore, TimerSlackGuest) {
    let config = Config {
        recordreplay_modes: false,
        ..Config::default()
    }
    .with_backend(|backend| backend.tool_shares_guest_descriptor_table = false);
    let mut thread = ThreadState::new(DetTid::from_raw(tid), &config, ());
    thread.detpid = Some(DetPid::from_raw(pid));
    thread.physical_tid = Some(509);
    thread.nested_pid_namespace = nested;
    thread.timer_slack_ns = SLACK;
    let mut procfs =
        ProcfsFile::from_path(Path::new(&format!("/proc/{target}/timerslack_ns"))).unwrap();
    procfs.bind_timer_slack_identity(IDENTITY.0, IDENTITY.1);
    thread.add_fd(FD, flags, FdType::Regular, None).unwrap();
    thread
        .with_detfd(FD, |fd| fd.set_procfs(procfs.clone()))
        .unwrap();
    // The Tool remains the original parent when current ThreadState is 9/9.
    let tool = Detcore::new(Pid::from_raw(3), &config);
    (
        tool,
        TimerSlackGuest {
            config,
            thread,
            arena: Box::new([0; ARENA_WORDS]),
            target,
            observed: Ok(IDENTITY),
            visible_tid: Ok(509),
            injected: Vec::new(),
            stack_requests: 0,
        },
    )
}

#[tokio::test]
async fn ordinary_read_uses_current_shared_tid_after_inode_observation() {
    for current in [3, 9] {
        let (tool, mut guest) = setup(current, current, current, false, OFlag::O_RDONLY);
        let mut bytes = [0xa5_u8; 32];
        let call = syscalls::Read::new()
            .with_fd(FD)
            .with_buf(AddrMut::from_raw(bytes.as_mut_ptr() as usize))
            .with_len(bytes.len());
        let result = tool.handle_read(&mut guest, call).await.unwrap();
        assert_eq!(result, TEXT.len() as i64);
        assert_eq!(&bytes[..TEXT.len()], TEXT);
        assert!(bytes[TEXT.len()..].iter().all(|byte| *byte == 0xa5));
        assert_eq!(guest.injected, [Sysno::newfstatat]);
        assert_eq!(guest.stack_requests, 1);
        assert_eq!(guest.thread.timer_slack_ns, SLACK);
    }
    // A process identity is not the current thread identity: /proc/self was
    // bound to leader 3, while the reading thread is 7.
    let (tool, mut guest) = setup(3, 7, 3, false, OFlag::O_RDONLY);
    let mut bytes = [0xa5_u8; 32];
    let call = syscalls::Read::new()
        .with_fd(FD)
        .with_buf(AddrMut::from_raw(bytes.as_mut_ptr() as usize))
        .with_len(bytes.len());
    let result = tool.handle_read(&mut guest, call).await;
    assert!(
        matches!(result, Err(Error::Errno(Errno::EPERM))),
        "{result:?}"
    );
    assert_eq!(guest.injected, [Sysno::newfstatat]);
    assert_eq!(bytes, [0xa5; 32]);
}

#[tokio::test]
async fn nested_read_keeps_visible_tid_and_delegated_errors() {
    for (target, observed, visible_tid, expected) in [
        (1, Ok(IDENTITY), Ok(1), Ok(TEXT.len() as i64)),
        (2, Ok(IDENTITY), Ok(1), Err(Errno::EPERM)),
        (1, Ok(IDENTITY), Err(Errno::EIO), Err(Errno::EIO)),
        // Observation happens first, but the nested getter's error is still
        // propagated before the existing inode classifier runs.
        (1, Err(Errno::ENOENT), Err(Errno::EIO), Err(Errno::EIO)),
    ] {
        let (tool, mut guest) = setup(9, 9, target, true, OFlag::O_RDONLY);
        guest.observed = observed;
        guest.visible_tid = visible_tid;
        let mut bytes = [0xa5_u8; 32];
        let call = syscalls::Read::new()
            .with_fd(FD)
            .with_buf(AddrMut::from_raw(bytes.as_mut_ptr() as usize))
            .with_len(bytes.len());
        let result = tool.handle_read(&mut guest, call).await;
        match (result, expected) {
            (Ok(actual), Ok(expected)) => {
                assert_eq!(actual, expected);
                assert_eq!(&bytes[..TEXT.len()], TEXT);
                assert!(bytes[TEXT.len()..].iter().all(|byte| *byte == 0xa5));
            }
            (Err(Error::Errno(actual)), Err(expected)) => {
                assert_eq!(actual, expected);
                assert_eq!(bytes, [0xa5; 32]);
            }
            other => panic!("wrong nested timer-slack read result: {other:?}"),
        }
        assert_eq!(guest.injected, [Sysno::newfstatat, Sysno::gettid]);
        assert_eq!(guest.stack_requests, 1);
    }
}

#[tokio::test]
async fn read_keeps_inode_and_observation_errors_ahead_of_identity_permission() {
    for nested in [false, true] {
        for (observed, expected) in [
            (Err(Errno::ENOENT), Errno::ESRCH),
            (Ok((IDENTITY.0, IDENTITY.1 + 1)), Errno::ESRCH),
            (Err(Errno::EACCES), Errno::EACCES),
        ] {
            let target = if nested { 2 } else { 9 };
            let (tool, mut guest) = setup(9, 9, target, nested, OFlag::O_RDONLY);
            guest.observed = observed;
            guest.visible_tid = Ok(1);
            let mut bytes = [0xa5_u8; 32];
            let call = syscalls::Read::new()
                .with_fd(FD)
                .with_buf(AddrMut::from_raw(bytes.as_mut_ptr() as usize))
                .with_len(bytes.len());
            let result = tool.handle_read(&mut guest, call).await;
            assert!(
                matches!(result, Err(Error::Errno(actual)) if actual == expected),
                "{result:?}"
            );
            if nested && observed != Err(Errno::EACCES) {
                assert_eq!(guest.injected, [Sysno::newfstatat, Sysno::gettid]);
            } else {
                assert_eq!(guest.injected, [Sysno::newfstatat]);
            }
            assert_eq!(guest.stack_requests, 1);
            assert_eq!(bytes, [0xa5; 32]);
        }
    }
}

#[tokio::test]
async fn zero_length_and_path_descriptor_reads_do_not_observe_or_query_tid() {
    for positioned in [false, true] {
        for (flags, length, expected) in [
            (OFlag::O_RDONLY, 0, Ok(0)),
            (OFlag::O_PATH, 0, Err(Errno::EBADF)),
            (OFlag::O_PATH, 8, Err(Errno::EBADF)),
        ] {
            let (tool, mut guest) = setup(9, 9, 1, true, flags);
            let result = if positioned {
                tool.handle_pread64(
                    &mut guest,
                    syscalls::Pread64::new()
                        .with_fd(FD)
                        .with_len(length)
                        .with_offset(0),
                )
                .await
            } else {
                tool.handle_read(
                    &mut guest,
                    syscalls::Read::new().with_fd(FD).with_len(length),
                )
                .await
            };
            match (result, expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected),
                (Err(Error::Errno(actual)), Err(expected)) => assert_eq!(actual, expected),
                other => panic!("wrong early timer-slack read result: {other:?}"),
            }
            assert!(guest.injected.is_empty());
            assert_eq!(guest.stack_requests, 0);
        }
    }
}
