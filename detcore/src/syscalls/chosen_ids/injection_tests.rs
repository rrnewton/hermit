/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Error attribution through the real chosen-ID injection path. The scripted
//! kernel distinguishes rewritten arguments from the original call, whose
//! outcome is independent. Heap-backed scratch here does not model the
//! lifetime of memory borrowed from a real guest stack.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

use futures::FutureExt;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::LocalMemory;

use super::*;
use crate::Config;
use crate::GlobalState;
use crate::ThreadState;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::types::DetPid;

#[derive(Default)]
struct Scratch(Vec<Box<[u8]>>);

impl Drop for Scratch {
    fn drop(&mut self) {}
}

impl Stack for Scratch {
    type StackGuard = Self;

    fn size(&self) -> usize {
        self.0.iter().map(|bytes| bytes.len()).sum()
    }

    fn capacity(&self) -> usize {
        1024
    }

    fn push<'a, T>(&mut self, value: T) -> Addr<'a, T> {
        // SAFETY: the production path stages initialized byte/pid_t arrays.
        // The Box remains live through the corresponding injection.
        let bytes =
            unsafe { std::slice::from_raw_parts(&value as *const T as *const u8, size_of::<T>()) };
        let storage = bytes.to_vec().into_boxed_slice();
        let address = Addr::from_raw(storage.as_ptr() as usize).unwrap();
        self.0.push(storage);
        address
    }

    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("the chosen-ID path does not reserve an output buffer")
    }

    fn commit(self) -> Result<Self::StackGuard, Errno> {
        Ok(self)
    }
}

#[derive(Debug)]
struct PolicyRefused(String);

struct CloneGuest {
    config: Config,
    thread: ThreadState<()>,
    original_args: Box<[u8; CLONE_ARGS_SIZE_VER2]>,
    original_expected: [u8; CLONE_ARGS_SIZE_VER2],
    original_set_tid: Option<Box<[i32; 1]>>,
    original_result: Result<i64, Errno>,
    rewritten_errno: Errno,
    attempts: Vec<bool>,
    host_id_reports: AtomicUsize,
}

impl CloneGuest {
    fn new(refuse_host_ids: bool, original_result: Result<i64, Errno>) -> Self {
        let config = Config {
            guest_ids: detcore_model::pid::GuestIds::Chosen {
                base: 5000,
                refuse_host_ids,
            },
            ..Config::default()
        };
        let mut original_args = Box::new([0; CLONE_ARGS_SIZE_VER2]);
        original_args[32..40].copy_from_slice(&(libc::SIGCHLD as u64).to_ne_bytes());
        Self {
            thread: ThreadState::new(DetPid::from_raw(5000), &config, ()),
            config,
            original_expected: *original_args,
            original_args,
            original_set_tid: None,
            original_result,
            rewritten_errno: Errno::EINVAL,
            attempts: Vec::new(),
            host_id_reports: AtomicUsize::new(0),
        }
    }

    fn original_call(&self) -> Syscall {
        Syscall::Clone3(
            syscalls::Clone3::new()
                .with_args(AddrMut::from_raw(self.original_args.as_ptr() as usize))
                .with_size(CLONE_ARGS_SIZE_VER2),
        )
    }

    fn chosen(&self, id: i32) -> ClonePlan {
        ClonePlan::Chosen(ChosenId {
            set_tid: vec![id],
            id: Some(id),
            restart: None,
            args: *self.original_args,
            size: CLONE_ARGS_SIZE_VER2,
            untraced: false,
            guest_entries: false,
        })
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for CloneGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let mut request = message.2;
        while let GlobalRequest::WithForwardedCount(_, inner) = request {
            request = *inner;
        }
        match request {
            GlobalRequest::GaveHostId => {
                self.host_id_reports.fetch_add(1, SeqCst);
                (None, GlobalResponse::GaveHostId(()))
            }
            // Stop at the real named-refusal boundary, before its process
            // exit; the test inspects the diagnostic and injected calls.
            GlobalRequest::PolicyRefusal(reason) => std::panic::panic_any(PolicyRefused(reason)),
            request => panic!("unexpected clone-attribution RPC: {request:?}"),
        }
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for CloneGuest {
    type Memory = LocalMemory;
    type Stack = Scratch;

    fn tid(&self) -> Pid {
        Pid::from_raw(5000)
    }
    fn pid(&self) -> Pid {
        self.tid()
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("the chosen-ID path does not need guest registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        Scratch::default()
    }
    async fn daemonize(&mut self) {
        panic!("the chosen-ID path must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        let (number, args) = call.into_parts();
        let Syscall::Clone3(clone) = Syscall::from_raw(number, args) else {
            panic!("expected clone3, got {number}")
        };
        assert_eq!(clone.size(), CLONE_ARGS_SIZE_VER2);
        let original = clone.args().unwrap().as_raw() == self.original_args.as_ptr() as usize;
        let mut bytes = [0; CLONE_ARGS_SIZE_VER2];
        self.memory()
            .read_exact(clone.args().unwrap().cast::<u8>(), &mut bytes)
            .unwrap();
        self.attempts.push(original);
        if original {
            assert_eq!(bytes, self.original_expected);
            if let Some(ids) = &self.original_set_tid {
                let address = u64::from_ne_bytes(bytes[SET_TID_OFFSET..72].try_into().unwrap());
                assert_eq!(address as usize, ids.as_ptr() as usize);
                assert_eq!(&bytes[SET_TID_SIZE_OFFSET..80], &1_u64.to_ne_bytes());
            } else {
                assert_eq!(&bytes[SET_TID_OFFSET..SET_TID_SIZE_OFFSET + 8], &[0; 16]);
            }
            self.original_result
        } else {
            assert_eq!(
                &bytes[..SET_TID_OFFSET],
                &self.original_args[..SET_TID_OFFSET]
            );
            assert_eq!(&bytes[SET_TID_SIZE_OFFSET..80], &1_u64.to_ne_bytes());
            let address = u64::from_ne_bytes(bytes[SET_TID_OFFSET..72].try_into().unwrap());
            let id: i32 = self
                .memory()
                .read_value(Addr::from_raw(address as usize).unwrap())
                .unwrap();
            assert_eq!(id, 5001);
            Err(self.rewritten_errno)
        }
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("the chosen-ID path must not tail-inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("the chosen-ID path must not set a timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("the chosen-ID path must not set a timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("the chosen-ID path must not read a clock")
    }
}

#[tokio::test]
async fn an_added_id_einval_takes_the_original_calls_own_result() {
    for native_result in [Ok(6000), Err(Errno::EINVAL), Err(Errno::EFAULT)] {
        let mut guest = CloneGuest::new(false, native_result);
        let tool = Detcore::new(guest.tid(), &guest.config);
        let call = guest.original_call();
        let plan = guest.chosen(5001);
        let result = tool
            .inject_clone_planned(&mut guest, call, plan, false)
            .await;
        assert_eq!(result, native_result);
        assert_eq!(guest.attempts, [false, true]);
        assert_eq!(
            guest.host_id_reports.load(SeqCst),
            usize::from(native_result.is_ok())
        );
    }

    // A call with no added entry is still answered directly by the kernel.
    let mut guest = CloneGuest::new(false, Err(Errno::EINVAL));
    let ids = Box::new([i32::MAX]);
    guest.original_args = Box::new(ChosenIdCall::with_set_tid(
        *guest.original_args,
        ids.as_ptr() as u64,
        1,
    ));
    guest.original_expected = *guest.original_args;
    guest.original_set_tid = Some(ids);
    let tool = Detcore::new(guest.tid(), &guest.config);
    let call = guest.original_call();
    assert_eq!(
        tool.inject_clone_planned(&mut guest, call, ClonePlan::GuestChosen, false)
            .await,
        Err(Errno::EINVAL)
    );
    assert_eq!(guest.attempts, [true]);
    assert_eq!(guest.host_id_reports.load(SeqCst), 0);

    // Other native failures keep their original error attribution and do
    // not trigger a host-ID retry merely because an ID was added.
    for errno in [Errno::EFAULT, Errno::EAGAIN] {
        let mut guest = CloneGuest::new(false, Ok(6000));
        guest.rewritten_errno = errno;
        let tool = Detcore::new(guest.tid(), &guest.config);
        let call = guest.original_call();
        let plan = guest.chosen(5001);
        assert_eq!(
            tool.inject_clone_planned(&mut guest, call, plan, false)
                .await,
            Err(errno)
        );
        assert_eq!(guest.attempts, [false]);
        assert_eq!(guest.host_id_reports.load(SeqCst), 0);
    }
}

#[tokio::test]
async fn an_added_id_einval_is_a_named_refusal_when_host_ids_are_forbidden() {
    let mut guest = CloneGuest::new(true, Ok(6000));
    let tool = Detcore::new(guest.tid(), &guest.config);
    let call = guest.original_call();
    let plan = guest.chosen(5001);
    let refused =
        std::panic::AssertUnwindSafe(tool.inject_clone_planned(&mut guest, call, plan, false))
            .catch_unwind()
            .await
            .expect_err("an ambiguous rewritten EINVAL must refuse the chosen-ID guarantee");
    let reason = refused.downcast_ref::<PolicyRefused>().unwrap();
    assert!(
        reason.0.contains("needs every guest process ID chosen"),
        "{reason:?}"
    );
    assert!(reason.0.contains("EINVAL"), "{reason:?}");
    assert_eq!(guest.attempts, [false]);
    assert_eq!(guest.host_id_reports.load(SeqCst), 0);
}
