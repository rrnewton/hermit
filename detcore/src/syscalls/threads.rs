/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with threads and concurrency.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use procfs::process::Process;
use rand::Rng;
use reverie::Error;
use reverie::Guest;
use reverie::Pid;
use reverie::Signal;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Errno;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Timespec;
use reverie::syscalls::WaitPidFlag;
use tracing::debug;
use tracing::info;
use tracing::trace;

use crate::config::BlockingMode;
use crate::memory::MemoryMetadata;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::ExternalOpId;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::SchedValue;
use crate::syscalls::helpers::record_retry_event;
use crate::syscalls::helpers::retry_nonblocking_syscall;
use crate::syscalls::helpers::retry_nonblocking_syscall_with_timeout;
use crate::tool_global::FutexAction;
use crate::tool_global::ResumeStatus;
use crate::tool_global::create_child_thread;
use crate::tool_global::futex_action;
use crate::tool_global::resource_request;
use crate::tool_global::set_robust_list_owner_active;
use crate::tool_global::thread_observe_time;
use crate::tool_local::Detcore;
use crate::tool_local::PendingVfork;
use crate::types::DetTid;
use crate::types::LogicalTime;

// Preserve the historical Detcore ABI while hiding the host's configured CPU
// count. This represents one virtual CPU in a fixed 128-bit kernel mask.
const VIRTUAL_CPUSET_BYTES: usize = 16;

const ROBUST_LIST_LIMIT: usize = 2048;
const ROBUST_LIST_HEAD_SIZE: usize = 3 * std::mem::size_of::<usize>();
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

const FUTEX2_SIZE_MASK: u32 = 0x03;
const FUTEX2_SIZE_U32: u32 = 0x02;
const FUTEX2_NUMA: u32 = 0x04;
const FUTEX2_MPOL: u32 = 0x08;
const FUTEX2_PRIVATE: u32 = libc::FUTEX_PRIVATE_FLAG as u32;
const FUTEX2_VALID_MASK: u32 = FUTEX2_SIZE_MASK | FUTEX2_NUMA | FUTEX2_MPOL | FUTEX2_PRIVATE;
const CLONE_ARGS_SIZE_VER0: usize = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
// TODO-HUMAN-REVIEW(PR-659): Review native robust-list head ABI mirror.
struct RobustListHead {
    list_next: usize,
    futex_offset: isize,
    list_op_pending: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
// TODO-HUMAN-REVIEW(PR-659): Review native futex2 waiter ABI mirror.
struct FutexWaitv {
    val: u64,
    uaddr: u64,
    flags: u32,
    reserved: u32,
}

// TODO-HUMAN-REVIEW(PR-659): Review robust entry-relative futex address calculation.
fn robust_futex_address(entry: usize, offset: isize) -> Option<usize> {
    if offset >= 0 {
        entry.checked_add(offset as usize)
    } else {
        entry.checked_sub(offset.unsigned_abs())
    }
}

// TODO-HUMAN-REVIEW(PR-659): Review Linux robust-list PI pointer-tag decoding.
fn decode_robust_pointer(raw: usize) -> (usize, bool) {
    (raw & !1, raw & 1 != 0)
}

// TODO-HUMAN-REVIEW(PR-659): Review Linux default terminating-signal set.
fn default_signal_action_terminates(signum: i32) -> bool {
    matches!(
        signum,
        libc::SIGHUP
            | libc::SIGINT
            | libc::SIGQUIT
            | libc::SIGILL
            | libc::SIGTRAP
            | libc::SIGABRT
            | libc::SIGBUS
            | libc::SIGFPE
            | libc::SIGKILL
            | libc::SIGUSR1
            | libc::SIGSEGV
            | libc::SIGUSR2
            | libc::SIGPIPE
            | libc::SIGALRM
            | libc::SIGTERM
            | libc::SIGSTKFLT
            | libc::SIGPOLL
            | libc::SIGPROF
            | libc::SIGSYS
            | libc::SIGVTALRM
            | libc::SIGXCPU
            | libc::SIGXFSZ
            | libc::SIGPWR
    ) || (libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&signum)
}

// TODO-HUMAN-REVIEW(PR-659): Review forced descendant tracing for complete scheduler state.
fn sanitize_clone_flags(mut flags: CloneFlags) -> CloneFlags {
    flags.remove(CloneFlags::CLONE_UNTRACED);
    flags
}

// TODO-HUMAN-REVIEW(PR-659): Review clone3's versioned size bounds.
fn clone3_args_size_is_copyable(size: usize, page_size: usize) -> bool {
    (CLONE_ARGS_SIZE_VER0..=page_size).contains(&size)
}

// TODO-HUMAN-REVIEW(PR-659): Review futex2 U32 flag and errno policy.
fn validate_futex2_flags(flags: u32) -> Result<bool, Errno> {
    if flags & !FUTEX2_VALID_MASK != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & FUTEX2_SIZE_MASK != FUTEX2_SIZE_U32 {
        return Err(Errno::EINVAL);
    }
    if flags & (FUTEX2_NUMA | FUTEX2_MPOL) != 0 {
        return Err(Errno::ENOSYS);
    }
    Ok(flags & FUTEX2_PRIVATE != 0)
}

// TODO-HUMAN-REVIEW(PR-659): Review raw futex2 address validation and errno mapping.
fn futex2_address(raw: usize) -> Result<AddrMut<'static, u32>, Errno> {
    if raw == 0 {
        return Err(Errno::EFAULT);
    }
    if !raw.is_multiple_of(std::mem::align_of::<u32>()) {
        return Err(Errno::EINVAL);
    }
    AddrMut::from_raw(raw).ok_or(Errno::EFAULT)
}

// TODO-HUMAN-REVIEW(PR-659): Review FUTEX_WAKE_OP 12-bit sign extension.
fn sign_extend_12(value: u32) -> i32 {
    ((value << 20) as i32) >> 20
}

// TODO-HUMAN-REVIEW(PR-659): Review FUTEX_WAKE_OP signed operand decoding.
fn apply_futex_wake_op(encoded: u32, old: i32) -> Result<(i32, bool), Errno> {
    let op = (encoded >> 28) & 0x7;
    let shift_oparg = encoded & 0x8000_0000 != 0;
    let mut oparg = sign_extend_12((encoded >> 12) & 0x0fff);
    let cmparg = sign_extend_12(encoded & 0x0fff);
    let comparison = (encoded >> 24) & 0x0f;

    if shift_oparg {
        oparg = 1_i32.wrapping_shl((oparg & 31) as u32);
    }
    let new = match op {
        0 => oparg,
        1 => old.wrapping_add(oparg),
        2 => old | oparg,
        3 => old & !oparg,
        4 => old ^ oparg,
        _ => return Err(Errno::ENOSYS),
    };
    let wake_second = match comparison {
        0 => old == cmparg,
        1 => old != cmparg,
        2 => old < cmparg,
        3 => old <= cmparg,
        4 => old > cmparg,
        5 => old >= cmparg,
        _ => return Err(Errno::ENOSYS),
    };
    Ok((new, wake_second))
}

// TODO-HUMAN-REVIEW(PR-659): Review typed futex wait completion decoding.
fn decode_futex_wait_result(answer: Option<SchedValue>) -> Result<i64, Errno> {
    match answer {
        None | Some(SchedValue::Value(_)) => Ok(0),
        Some(SchedValue::TimeOut) => Err(Errno::ETIMEDOUT),
        Some(SchedValue::Interrupted) => Err(Errno::EINTR),
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WaitidSigchldFields {
    pid: libc::pid_t,
    uid: libc::uid_t,
    status: libc::c_int,
    utime: libc::c_long,
    stime: libc::c_long,
}

#[repr(C)]
union WaitidSiginfoFields {
    _alignment: *mut libc::c_void,
    sigchld: WaitidSigchldFields,
}

#[repr(C)]
struct WaitidSiginfoHead {
    _base: [libc::c_int; 3],
    fields: WaitidSiginfoFields,
}

fn canonicalize_waitid_siginfo(info: &mut libc::siginfo_t) {
    debug_assert!(
        std::mem::size_of::<WaitidSiginfoHead>() <= std::mem::size_of::<libc::siginfo_t>()
    );
    // SAFETY: Linux siginfo_t starts with three c_int fields followed by a
    // pointer-aligned union. Its SIGCHLD member is pid, uid, status, utime,
    // and stime in that order. The local repr(C) mirror changes only the two
    // host CPU-accounting fields and preserves the kernel-populated event.
    let sigchld = unsafe {
        &mut (*(info as *mut libc::siginfo_t).cast::<WaitidSiginfoHead>())
            .fields
            .sigchld
    };
    sigchld.utime = 0;
    sigchld.stime = 0;
}

fn snapshot_process_group(pid: Pid) -> Result<libc::pid_t, Errno> {
    let pgrp = Process::new(pid.as_raw())
        .and_then(|process| process.stat())
        .map(|stat| stat.pgrp)
        .map_err(|_| Errno::ESRCH)?;
    if pgrp == 0 {
        Err(Errno::EOPNOTSUPP)
    } else {
        Ok(pgrp)
    }
}

fn guest_fd_status_flags(pid: Pid, fd: libc::c_int) -> Result<libc::c_int, Errno> {
    let path = format!("/proc/{}/fdinfo/{}", pid.as_raw(), fd);
    let contents = std::fs::read_to_string(path).map_err(|_| Errno::EBADF)?;
    let flags = contents
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .map(str::trim)
        .ok_or(Errno::EINVAL)?;
    libc::c_int::from_str_radix(flags, 8).map_err(|_| Errno::EINVAL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FutexTimeout {
    Relative(u64),
    Absolute(LogicalTime),
}

fn parse_futex_timeout(futex_op: i32, timeout: Timespec) -> Result<FutexTimeout, Errno> {
    let seconds = u64::try_from(timeout.tv_sec).map_err(|_| Errno::EINVAL)?;
    let nanoseconds = u64::try_from(timeout.tv_nsec).map_err(|_| Errno::EINVAL)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }

    let timeout_nanos = seconds
        .checked_mul(1_000_000_000)
        .and_then(|nanos| nanos.checked_add(nanoseconds))
        .ok_or(Errno::EINVAL)?;
    // Mask off FUTEX_PRIVATE_FLAG / FUTEX_CLOCK_REALTIME before matching the
    // command: FUTEX_WAIT_BITSET measures its timeout as an *absolute* deadline,
    // whereas plain FUTEX_WAIT uses a *relative* one. A private-flagged
    // FUTEX_WAIT_BITSET (e.g. 0x89) must still be recognized as the BITSET
    // command; comparing the raw op would misclassify it as relative and add
    // the absolute deadline to the current time (leaking the epoch).
    if futex_op & libc::FUTEX_CMD_MASK == libc::FUTEX_WAIT_BITSET {
        Ok(FutexTimeout::Absolute(LogicalTime::from_nanos(
            timeout_nanos,
        )))
    } else {
        Ok(FutexTimeout::Relative(timeout_nanos))
    }
}

fn rebase_absolute_timeout(
    deadline: LogicalTime,
    clock_now: LogicalTime,
    logical_now: LogicalTime,
) -> LogicalTime {
    logical_now + Duration::from_nanos(deadline.as_nanos().saturating_sub(clock_now.as_nanos()))
}

impl<T: RecordOrReplay> Detcore<T> {
    // TODO-HUMAN-REVIEW(PR-659): Review caught-signal disposition detection.
    pub(crate) fn signal_has_user_handler<G: Guest<Self>>(
        &self,
        guest: &G,
        signal: Signal,
    ) -> bool {
        let signum = signal as i32;
        let status = match Process::new(guest.pid().as_raw()).and_then(|process| process.status()) {
            Ok(status) => status,
            Err(error) => {
                debug!("could not inspect signal disposition for {signal}: {error}");
                return true;
            }
        };
        let signal_mask = 1_u64 << (signum - 1);
        status.sigcgt & signal_mask != 0
    }

    // TODO-HUMAN-REVIEW(PR-659): Review fatal-signal disposition detection for robust cleanup.
    pub(crate) fn signal_will_terminate_thread_group<G: Guest<Self>>(
        &self,
        guest: &G,
        signal: Signal,
    ) -> bool {
        let signum = signal as i32;
        if !default_signal_action_terminates(signum) {
            return false;
        }
        if signum == libc::SIGKILL {
            return true;
        }

        let status = match Process::new(guest.pid().as_raw()).and_then(|process| process.status()) {
            Ok(status) => status,
            Err(error) => {
                debug!("could not inspect signal disposition for {signal}: {error}");
                return false;
            }
        };
        let signal_mask = 1_u64 << (signum - 1);
        status.sigign & signal_mask == 0 && status.sigcgt & signal_mask == 0
    }

    async fn futex_timeout_deadline<G: Guest<Self>>(
        &self,
        guest: &mut G,
        futex_flags: i32,
        timeout: Option<Addr<'_, Timespec>>,
    ) -> Result<Option<LogicalTime>, Error> {
        let Some(timeout) = timeout else {
            return Ok(None);
        };
        let timeout = parse_futex_timeout(futex_flags, guest.memory().read_value(timeout)?)?;
        match timeout {
            FutexTimeout::Relative(nanos) => {
                let now = thread_observe_time(guest).await;
                Ok(Some(now + Duration::from_nanos(nanos)))
            }
            FutexTimeout::Absolute(deadline) if self.cfg.virtualize_time => Ok(Some(deadline)),
            FutexTimeout::Absolute(deadline) => {
                let clockid = if futex_flags & libc::FUTEX_CLOCK_REALTIME != 0 {
                    syscalls::ClockId::CLOCK_REALTIME
                } else {
                    syscalls::ClockId::CLOCK_MONOTONIC
                };

                let mut stack = guest.stack().await;
                let clock_output = syscalls::TimespecMutPtr(stack.reserve());
                let _stack_guard = stack.commit()?;
                self.record_or_replay(
                    guest,
                    syscalls::ClockGettime::new()
                        .with_clockid(clockid)
                        .with_tp(Some(clock_output)),
                )
                .await?;
                let clock_now = match parse_futex_timeout(
                    libc::FUTEX_WAIT_BITSET,
                    guest.memory().read_value(clock_output.0)?,
                )? {
                    FutexTimeout::Absolute(time) => time,
                    FutexTimeout::Relative(_) => unreachable!(),
                };
                let logical_now = thread_observe_time(guest).await;
                Ok(Some(rebase_absolute_timeout(
                    deadline,
                    clock_now,
                    logical_now,
                )))
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-659): Review robust-list registration mirroring.
    /// Register the kernel robust list and mirror its head in the thread-group registry.
    pub async fn handle_set_robust_list<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SetRobustList,
    ) -> Result<i64, Error> {
        let head = call.head().map(AddrMut::as_raw);
        let result = guest.inject(call).await?;
        if result == 0 {
            let dettid = guest.thread_state().dettid;
            guest
                .thread_state()
                .robust_list_heads
                .lock()
                .expect("robust-list registry mutex poisoned")
                .insert(dettid, head);
            self.refresh_robust_list_activity(guest).await;
        }
        Ok(result)
    }

    // TODO-HUMAN-REVIEW(PR-659): Review deterministic robust-list activity observation.
    /// Publish robust-list activity changes observed when Detcore regains control.
    pub(crate) async fn refresh_robust_list_activity<G: Guest<Self>>(&self, guest: &mut G) {
        if !self.cfg.sequentialize_threads || self.cfg.debug_futex_mode != BlockingMode::Precise {
            return;
        }
        let dettid = guest.thread_state().dettid;
        let head = guest
            .thread_state()
            .robust_list_heads
            .lock()
            .expect("robust-list registry mutex poisoned")
            .get(&dettid)
            .copied()
            .flatten();
        let active = match head {
            None => false,
            Some(head_address) => {
                let Some(head_ptr) = Addr::<RobustListHead>::from_raw(head_address) else {
                    return;
                };
                let Ok(head_value) = guest.memory().read_value(head_ptr) else {
                    return;
                };
                head_value.list_op_pending != 0 || head_value.list_next != head_address
            }
        };
        if guest.thread_state().robust_list_active == active {
            return;
        }
        guest.thread_state_mut().robust_list_active = active;
        set_robust_list_owner_active(guest, active).await;
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-659): Review same-thread-group robust-list lookup policy.
    /// Return a modeled robust-list registration for a thread in this thread group.
    pub fn handle_get_robust_list<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::GetRobustList,
    ) -> Result<i64, Error> {
        let current = guest.thread_state().dettid;
        let target = if call.pid() == 0 {
            current
        } else {
            DetTid::from_raw(call.pid())
        };
        let head = guest
            .thread_state()
            .robust_list_heads
            .lock()
            .expect("robust-list registry mutex poisoned")
            .get(&target)
            .copied()
            .ok_or(Errno::ESRCH)?
            .unwrap_or(0);
        let len_ptr = call.len_ptr().ok_or(Errno::EFAULT)?;
        let head_ptr = call.head_ptr().ok_or(Errno::EFAULT)?;
        guest
            .memory()
            .write_value(len_ptr, &ROBUST_LIST_HEAD_SIZE)?;
        let raw_head_ptr = AddrMut::<usize>::from_raw(head_ptr.as_raw()).ok_or(Errno::EFAULT)?;
        guest.memory().write_value(raw_head_ptr, &head)?;
        Ok(0)
    }

    // TODO-HUMAN-REVIEW(PR-659): Review precise-mode robust owner-death lifecycle.
    pub(crate) async fn cleanup_registered_robust_lists<G: Guest<Self>>(
        &self,
        guest: &mut G,
        whole_group: bool,
    ) {
        if !self.cfg.sequentialize_threads || self.cfg.debug_futex_mode != BlockingMode::Precise {
            return;
        }
        let current = guest.thread_state().dettid;
        let registrations = {
            let mut heads = guest
                .thread_state()
                .robust_list_heads
                .lock()
                .expect("robust-list registry mutex poisoned");
            if whole_group {
                heads
                    .iter_mut()
                    .filter_map(|(&owner, head)| head.take().map(|head| (owner, head)))
                    .collect::<Vec<_>>()
            } else {
                heads
                    .get_mut(&current)
                    .and_then(Option::take)
                    .map(|head| vec![(current, head)])
                    .unwrap_or_default()
            }
        };

        for (owner, head) in registrations {
            self.cleanup_robust_list_head(guest, owner, head).await;
        }
    }

    // TODO-HUMAN-REVIEW(PR-659): Review robust-list traversal and pending-entry ordering.
    async fn cleanup_robust_list_head<G: Guest<Self>>(
        &self,
        guest: &mut G,
        owner: DetTid,
        head_address: usize,
    ) {
        let Some(head_ptr) = Addr::<RobustListHead>::from_raw(head_address) else {
            return;
        };
        let head = match guest.memory().read_value(head_ptr) {
            Ok(head) => head,
            Err(error) => {
                debug!("could not read robust-list head at {head_address:#x}: {error}");
                return;
            }
        };

        let (pending, pending_pi) = decode_robust_pointer(head.list_op_pending);
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        let (mut current, mut current_pi) = decode_robust_pointer(head.list_next);
        for _ in 0..ROBUST_LIST_LIMIT {
            if current == head_address {
                break;
            }
            if current == 0 || !seen.insert(current) {
                debug!("invalid or cyclic robust list at {current:#x}");
                break;
            }
            if current != pending {
                entries.push((current, current_pi));
            }
            let Some(next_ptr) = Addr::<usize>::from_raw(current) else {
                break;
            };
            let raw_next = match guest.memory().read_value(next_ptr) {
                Ok(next) => next,
                Err(error) => {
                    debug!("could not read robust-list entry at {current:#x}: {error}");
                    break;
                }
            };
            (current, current_pi) = decode_robust_pointer(raw_next);
        }

        for (entry, pi) in entries {
            self.cleanup_robust_futex(guest, owner, entry, head.futex_offset, pi, false)
                .await;
        }
        if pending != 0 {
            self.cleanup_robust_futex(guest, owner, pending, head.futex_offset, pending_pi, true)
                .await;
        }
    }

    // TODO-HUMAN-REVIEW(PR-659): Review individual robust futex owner-death transition.
    async fn cleanup_robust_futex<G: Guest<Self>>(
        &self,
        guest: &mut G,
        owner: DetTid,
        entry: usize,
        offset: isize,
        pi: bool,
        pending: bool,
    ) {
        if pi {
            return;
        }
        let Some(address) = robust_futex_address(entry, offset) else {
            return;
        };
        if !address.is_multiple_of(std::mem::align_of::<u32>()) {
            return;
        }
        let Some(word_ptr) = AddrMut::<u32>::from_raw(address) else {
            return;
        };
        let Ok(old) = guest.memory().read_value(word_ptr) else {
            return;
        };
        let owner_tid = owner.as_raw() as u32 & FUTEX_TID_MASK;
        let old_owner = old & FUTEX_TID_MASK;
        let wake_value = if pending && old_owner == 0 {
            Some(old)
        } else if old_owner == owner_tid {
            let new = (old & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
            if guest.memory().write_value(word_ptr, &new).is_err() {
                return;
            }
            (old & FUTEX_WAITERS != 0).then_some(new)
        } else {
            None
        };

        if let Some(value) = wake_value {
            let futexid = guest.thread_state().futex_id(address, false);
            let _ = futex_action(
                guest,
                FutexAction::WakeRequest(1),
                &futexid,
                value as i32,
                u32::MAX,
            )
            .await;
            let _ = futex_action(
                guest,
                FutexAction::WakeFinished(1),
                &futexid,
                value as i32,
                u32::MAX,
            )
            .await;
        }
    }

    // TODO-HUMAN-REVIEW(PR-659): Review deterministic futex timeout and EINTR results.
    async fn handle_futex_wait_result<G: Guest<Self>>(
        &self,
        guest: &mut G,
        futexid: &crate::types::FutexID,
        initial_value: i32,
        mask: u32,
        deadline: Option<LogicalTime>,
    ) -> Result<i64, Error> {
        let answer = futex_action(
            guest,
            FutexAction::WaitRequest(deadline),
            futexid,
            initial_value,
            mask,
        )
        .await;
        let result = decode_futex_wait_result(answer).map_err(Error::Errno);
        futex_action(
            guest,
            FutexAction::WaitFinished,
            futexid,
            initial_value,
            mask,
        )
        .await;
        result
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-659): Review raw futex2 U32 wake ABI and scheduler mapping.
    /// Wake modeled U32 futex2 waiters without entering the host scheduler.
    pub async fn handle_futex2_wake<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
    ) -> Result<i64, Error> {
        let address = futex2_address(args.arg0)?;
        let mask = u32::try_from(args.arg1).map_err(|_| Errno::EINVAL)?;
        if mask == 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let count = args.arg2 as i32;
        if count < 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let private = validate_futex2_flags(args.arg3 as u32)?;
        let futexid = guest.thread_state().futex_id(address.as_raw(), private);
        let woken = futex_action(guest, FutexAction::WakeRequest(count), &futexid, 0, mask)
            .await
            .expect("futex2 wake must return a count");
        match woken {
            SchedValue::Value(value) => Ok(value as i64),
            SchedValue::TimeOut | SchedValue::Interrupted => {
                unreachable!("wake does not block")
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-659): Review raw futex2 U32 wait ABI and logical timeout.
    /// Wait on a modeled U32 futex2 word using Detcore logical time.
    pub async fn handle_futex2_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
    ) -> Result<i64, Error> {
        let address = futex2_address(args.arg0)?;
        let expected = u32::try_from(args.arg1).map_err(|_| Errno::EINVAL)?;
        let mask = u32::try_from(args.arg2).map_err(|_| Errno::EINVAL)?;
        if mask == 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let private = validate_futex2_flags(args.arg3 as u32)?;
        let timeout = Addr::<Timespec>::from_raw(args.arg4);
        let clockid = args.arg5 as i32;
        if timeout.is_some() && clockid != libc::CLOCK_MONOTONIC && clockid != libc::CLOCK_REALTIME
        {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let mut legacy_flags = libc::FUTEX_WAIT_BITSET;
        if private {
            legacy_flags |= libc::FUTEX_PRIVATE_FLAG;
        }
        if timeout.is_some() && clockid == libc::CLOCK_REALTIME {
            legacy_flags |= libc::FUTEX_CLOCK_REALTIME;
        }
        let deadline = self
            .futex_timeout_deadline(guest, legacy_flags, timeout)
            .await?;
        let observed = guest.memory().read_value(address)?;
        if observed != expected {
            return Err(Error::Errno(Errno::EAGAIN));
        }
        let futexid = guest.thread_state().futex_id(address.as_raw(), private);
        self.handle_futex_wait_result(guest, &futexid, observed as i32, mask, deadline)
            .await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-659): Review raw futex2 U32 requeue ABI and queue ordering.
    /// Wake and requeue modeled U32 futex2 waiters deterministically.
    pub async fn handle_futex2_requeue<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
    ) -> Result<i64, Error> {
        if args.arg1 != 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let max_wake = args.arg2 as i32;
        let max_requeue = args.arg3 as i32;
        if max_wake < 0 || max_requeue < 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let first_ptr = Addr::<FutexWaitv>::from_raw(args.arg0).ok_or(Errno::EINVAL)?;
        let second_raw = args
            .arg0
            .checked_add(std::mem::size_of::<FutexWaitv>())
            .ok_or(Errno::EFAULT)?;
        let second_ptr = Addr::<FutexWaitv>::from_raw(second_raw).ok_or(Errno::EFAULT)?;
        let first = guest.memory().read_value(first_ptr)?;
        let second = guest.memory().read_value(second_ptr)?;
        if first.reserved != 0 || second.reserved != 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let source_private = validate_futex2_flags(first.flags)?;
        let target_private = validate_futex2_flags(second.flags)?;
        let expected = u32::try_from(first.val).map_err(|_| Errno::EINVAL)?;
        let _ = u32::try_from(second.val).map_err(|_| Errno::EINVAL)?;
        let source_address = futex2_address(first.uaddr as usize)?;
        let target_address = futex2_address(second.uaddr as usize)?;
        let observed = guest.memory().read_value(source_address)?;
        if observed != expected {
            return Err(Error::Errno(Errno::EAGAIN));
        }
        let source = guest
            .thread_state()
            .futex_id(source_address.as_raw(), source_private);
        let target = guest
            .thread_state()
            .futex_id(target_address.as_raw(), target_private);
        let changed = futex_action(
            guest,
            FutexAction::RequeueRequest {
                target,
                max_wake,
                max_requeue,
            },
            &source,
            observed as i32,
            u32::MAX,
        )
        .await
        .expect("futex2 requeue must return a count");
        match changed {
            SchedValue::Value(value) => Ok(value as i64),
            SchedValue::TimeOut | SchedValue::Interrupted => {
                unreachable!("requeue does not block")
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-659): Replace fixed refusal with multi-queue wait modeling.
    /// Refuse vector waits until one thread can register on multiple modeled queues.
    pub fn handle_futex2_waitv(&self, _args: SyscallArgs) -> Result<i64, Error> {
        Err(Error::Errno(Errno::ENOSYS))
    }

    // TODO-HUMAN-REVIEW(PR-659): Review legacy futex requeue ABI and scheduler transitions.
    async fn handle_futex_requeue<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
        initial_value: i32,
        compare: bool,
    ) -> Result<i64, Error> {
        let (_, raw) = call.into_parts();
        let max_wake = call.val();
        let max_requeue = raw.arg3 as i32;
        if max_wake < 0 || max_requeue < 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        if compare && initial_value != call.val3() {
            return Err(Error::Errno(Errno::EAGAIN));
        }
        let source_address = call.uaddr().ok_or(Errno::EFAULT)?;
        let target_address = call.uaddr2().ok_or(Errno::EFAULT)?;
        if !target_address
            .as_raw()
            .is_multiple_of(std::mem::align_of::<u32>())
        {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let private = call.futex_op() & libc::FUTEX_PRIVATE_FLAG != 0;
        let source = guest
            .thread_state()
            .futex_id(source_address.as_raw(), private);
        let target = guest
            .thread_state()
            .futex_id(target_address.as_raw(), private);
        let changed = futex_action(
            guest,
            FutexAction::RequeueRequest {
                target,
                max_wake,
                max_requeue,
            },
            &source,
            initial_value,
            u32::MAX,
        )
        .await
        .expect("futex requeue must return a count");
        match changed {
            SchedValue::Value(value) => Ok(value as i64),
            SchedValue::TimeOut | SchedValue::Interrupted => {
                unreachable!("requeue does not block")
            }
        }
    }

    // TODO-HUMAN-REVIEW(PR-659): Review legacy FUTEX_WAKE_OP atomicity and decoding.
    async fn handle_futex_wake_op<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
        initial_value: i32,
    ) -> Result<i64, Error> {
        let (_, raw) = call.into_parts();
        let max_wake = call.val();
        let max_wake_target = raw.arg3 as i32;
        if max_wake < 0 || max_wake_target < 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let source_address = call.uaddr().ok_or(Errno::EFAULT)?;
        let target_address = call.uaddr2().ok_or(Errno::EFAULT)?;
        if !target_address
            .as_raw()
            .is_multiple_of(std::mem::align_of::<u32>())
        {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let old = guest.memory().read_value(target_address)?;
        let (new, wake_target) = apply_futex_wake_op(call.val3() as u32, old)?;
        guest.memory().write_value(target_address, &new)?;
        let private = call.futex_op() & libc::FUTEX_PRIVATE_FLAG != 0;
        let source = guest
            .thread_state()
            .futex_id(source_address.as_raw(), private);
        let target = guest
            .thread_state()
            .futex_id(target_address.as_raw(), private);
        let woken = futex_action(
            guest,
            FutexAction::WakeOpRequest {
                target,
                max_wake,
                max_wake_target: if wake_target { max_wake_target } else { 0 },
            },
            &source,
            initial_value,
            u32::MAX,
        )
        .await
        .expect("futex wake-op must return a count");
        match woken {
            SchedValue::Value(value) => Ok(value as i64),
            SchedValue::TimeOut | SchedValue::Interrupted => {
                unreachable!("wake-op does not block")
            }
        }
    }
    /// Clone, clone3, fork, vfork system calls
    pub async fn handle_clone_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        clone_family: syscalls::family::CloneFamily,
    ) -> Result<i64, Error> {
        let (original_flags, ctid, clone3_bytes) = match clone_family {
            syscalls::family::CloneFamily::Clone3(call) => {
                let max_size = usize::try_from(procfs::page_size())
                    .expect("host page size does not fit in usize");
                if !clone3_args_size_is_copyable(call.size(), max_size) {
                    (CloneFlags::empty(), 0, None)
                } else {
                    let args_address = call.args().ok_or(Error::Errno(Errno::EFAULT))?;
                    let mut bytes = vec![0; call.size()];
                    guest.memory().read_exact(args_address.cast(), &mut bytes)?;
                    let flags = CloneFlags::from_bits_retain(u64::from_ne_bytes(
                        bytes[0..8].try_into().expect("clone3 flags prefix missing"),
                    ));
                    let child_tid = u64::from_ne_bytes(
                        bytes[16..24]
                            .try_into()
                            .expect("clone3 child_tid prefix missing"),
                    ) as usize;
                    (flags, child_tid, Some(bytes))
                }
            }
            other => (
                other.flags(&guest.memory()),
                other.child_tid(&guest.memory()),
                None,
            ),
        };
        let flags = sanitize_clone_flags(original_flags);
        let mut clone3_scratch_mapping = None;
        let clone_family = if flags == original_flags {
            clone_family
        } else {
            info!(
                "[detcore, dtid {}] removing CLONE_UNTRACED to keep the child under deterministic scheduling",
                guest.thread_state().dettid
            );
            match clone_family {
                syscalls::family::CloneFamily::Clone(call) => {
                    syscalls::family::CloneFamily::Clone(call.with_flags(
                        nix::sched::CloneFlags::from_bits_retain(flags.bits() as libc::c_int),
                    ))
                }
                syscalls::family::CloneFamily::Clone3(call) => {
                    let mut bytes = clone3_bytes
                        .expect("sanitized clone3 flags require a validated argument copy");
                    bytes[0..8].copy_from_slice(&flags.bits().to_ne_bytes());
                    let mapping_len = bytes.len();
                    let mapped = guest
                        .inject_with_retry(Syscall::Mmap(
                            syscalls::Mmap::new()
                                .with_addr(None)
                                .with_len(mapping_len)
                                .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                                .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                                .with_fd(-1)
                                .with_offset(0),
                        ))
                        .await?;
                    let mapped =
                        usize::try_from(mapped).map_err(|_| Error::Errno(Errno::EFAULT))?;
                    let mapping_addr = Addr::from_raw(mapped).ok_or(Error::Errno(Errno::EFAULT))?;
                    let sanitized_args =
                        AddrMut::from_raw(mapped).ok_or(Error::Errno(Errno::EFAULT))?;
                    if let Err(write_error) =
                        guest.memory().write_exact(sanitized_args.cast(), &bytes)
                    {
                        guest
                            .inject_with_retry(Syscall::Munmap(
                                syscalls::Munmap::new()
                                    .with_addr(Some(mapping_addr))
                                    .with_len(mapping_len),
                            ))
                            .await
                            .unwrap_or_else(|cleanup_error| {
                                panic!(
                                    "failed to populate clone3 scratch ({write_error}); cleanup failed ({cleanup_error})"
                                )
                            });
                        return Err(Error::Errno(write_error));
                    }
                    let mapping_addr_mut = AddrMut::from_raw(mapped)
                        .expect("clone3 scratch mmap returned a null mutable address");
                    // TODO-HUMAN-REVIEW(PR-659): Review non-inheritance of clone3 scratch VMAs.
                    if let Err(advice_error) = guest
                        .inject_with_retry(Syscall::Madvise(
                            syscalls::Madvise::new()
                                .with_addr(Some(mapping_addr_mut))
                                .with_len(mapping_len)
                                .with_advice(libc::MADV_DONTFORK),
                        ))
                        .await
                    {
                        guest
                            .inject_with_retry(Syscall::Munmap(
                                syscalls::Munmap::new()
                                    .with_addr(Some(mapping_addr))
                                    .with_len(mapping_len),
                            ))
                            .await
                            .unwrap_or_else(|cleanup_error| {
                                panic!(
                                    "failed to mark clone3 scratch MADV_DONTFORK ({advice_error}); cleanup failed ({cleanup_error})"
                                )
                            });
                        return Err(Error::Errno(advice_error));
                    }
                    clone3_scratch_mapping = Some((mapping_addr, mapping_len));
                    syscalls::family::CloneFamily::Clone3(call.with_args(Some(sanitized_args)))
                }
                other => other,
            }
        };
        let is_vfork = flags.contains(CloneFlags::CLONE_VFORK);

        let ts = guest.thread_state_mut();
        assert_eq!(ts.clone_flags, None);
        assert!(ts.pending_vfork.is_none());
        ts.clone_flags = Some(flags);

        let parent_dettid = ts.dettid;
        let child_priority_entropy = if is_vfork
            && self.cfg.chaos
            && self.cfg.replay_preemptions_from.is_none()
            && self.cfg.replay_schedule_from.is_none()
        {
            let mut parent_chaos_prng = ts.chaos_prng.clone();
            Some(parent_chaos_prng.next_u64())
        } else {
            None
        };
        if is_vfork {
            ts.pending_vfork = Some(PendingVfork {
                parent_dettid,
                parent_detpid: ts.detpid.expect("detpid unset"),
                child_tid_addr: ctid,
                flags,
                child_priority_entropy,
            });
        }

        trace!("[detcore, dtid {}] parent invoking clone.", parent_dettid);
        let vfork_op_id =
            ExternalOpId::new(parent_dettid, guest.thread_state().stats.syscall_count);

        // The kernel blocks a CLONE_VFORK parent until its child execs or exits.
        // Remove it from Detcore's run queue before entering that blocking call.
        if is_vfork && self.cfg.sequentialize_threads {
            let mut resources = Resources::new(parent_dettid);
            resources.insert(ResourceID::BlockingExternalIO(vfork_op_id), Permission::RW);
            resources.fyi("clone_vfork");
            resource_request(guest, resources).await;
        }

        let maybe_res = guest.inject(Syscall::from(clone_family)).await;
        if let Some((mapping_addr, mapping_len)) = clone3_scratch_mapping {
            guest
                .inject_with_retry(Syscall::Munmap(
                    syscalls::Munmap::new()
                        .with_addr(Some(mapping_addr))
                        .with_len(mapping_len),
                ))
                .await
                .unwrap_or_else(|error| panic!("failed to unmap clone3 scratch: {error}"));
        }

        if is_vfork && self.cfg.sequentialize_threads {
            let mut resources = Resources::new(parent_dettid);
            resources.insert(
                ResourceID::BlockedExternalContinue(vfork_op_id),
                Permission::RW,
            );
            resources.fyi("clone_vfork");
            resource_request(guest, resources).await;
        }

        let ts = guest.thread_state_mut();
        ts.clone_flags = None; // Unset, now that it has been read by the child.
        ts.pending_vfork = None;

        let res = maybe_res?;

        // Match ordinary clone: the parent consumes the priority entropy after
        // the child has inherited the parent state.
        if is_vfork
            && self.cfg.chaos
            && self.cfg.replay_preemptions_from.is_none()
            && self.cfg.replay_schedule_from.is_none()
        {
            let _ = guest
                .thread_state_mut()
                .chaos_prng_next_u64("child_priority");
        }

        let child_tid = Pid::from_raw(res as i32);
        let child_dettid = DetTid::from_raw(child_tid.into()); // TODO(T78538674), virtualized tid/pid
        trace!(
            "[detcore] dtid {} cloned, continuing parent + register new thread.",
            child_dettid
        );

        if !is_vfork {
            create_child_thread(guest, child_dettid, ctid, Some(flags)).await;
        }

        {
            // The child will have updated their pedigree, we update ours before continuing.
            let parent_pedigree = &mut guest.thread_state_mut().pedigree;
            let child_pedigree = parent_pedigree.fork_mut();
            debug!(
                "[dtid {}] after creating child thread (tid {}, pedigree {}) parents pedigree becomes {}",
                parent_dettid, child_dettid, child_pedigree, parent_pedigree,
            );
        }

        Ok(child_dettid.as_raw() as i64)
    }

    /// Exit system call
    pub async fn handle_exit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Exit,
    ) -> Result<i64, Error> {
        let request = guest.thread_state().mk_request(
            ResourceID::Exit {
                group: false,
                process: guest.thread_state().detpid.expect("detpid unset"),
                mm: guest.thread_state().mm_id,
            },
            Permission::RW,
        );
        resource_request(guest, request).await;
        self.cleanup_registered_robust_lists(guest, false).await;
        // It's ok here that we skip running the posthook:
        guest.tail_inject(call).await
    }

    /// Exit_group system call
    pub async fn handle_exit_group<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::ExitGroup,
    ) -> Result<i64, Error> {
        let request = guest.thread_state().mk_request(
            ResourceID::Exit {
                group: true,
                process: guest.thread_state().detpid.expect("detpid unset"),
                mm: guest.thread_state().mm_id,
            },
            Permission::RW,
        );
        resource_request(guest, request).await;
        self.cleanup_registered_robust_lists(guest, true).await;
        // It's ok here that we skip running the posthook:
        guest.tail_inject(call).await
    }

    /// Futex system call, which can block.
    pub async fn handle_futex<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let futex_command = call.futex_op() & libc::FUTEX_CMD_MASK;
        if call.futex_op() & libc::FUTEX_CLOCK_REALTIME != 0
            && futex_command != libc::FUTEX_WAIT_BITSET
        {
            return Err(Error::Errno(Errno::ENOSYS));
        }
        let ptr = match call.uaddr() {
            None => {
                // null pointer error:
                return Ok(guest.inject(call).await?);
            }
            Some(x) => x,
        };
        if !ptr.as_raw().is_multiple_of(std::mem::align_of::<u32>()) {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let init_val = guest.memory().read_value(ptr)?;
        trace!(
            "[detcore, dtid {}] futex op with memory address containing value {}",
            &dettid, init_val
        );

        if !self.cfg.sequentialize_threads {
            Ok(guest.inject(call).await?)
        } else {
            match self.cfg.debug_futex_mode {
                BlockingMode::Precise => self.handle_futex_blocking(guest, call, init_val).await,
                BlockingMode::Polling => self.handle_futex_polling(guest, call, init_val).await,
                BlockingMode::External => self.record_or_replay_blocking(guest, call.into()).await,
            }
        }
    }

    /// Blocking (precise) Futex implementation.
    /// Here we use a two-phase request to the scheduler: before and after the futex wait/wake
    /// side effects. We EMULATE futex calls and NEVER run them inside the kernel.
    pub async fn handle_futex_blocking<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
        init_val: i32,
    ) -> Result<i64, Error> {
        let ptr = call.uaddr().unwrap();
        let futexid = guest.thread_state().futex_id(
            AddrMut::as_raw(ptr),
            call.futex_op() & libc::FUTEX_PRIVATE_FLAG != 0,
        );
        let futex_op = call.futex_op() & libc::FUTEX_CMD_MASK;
        let bitset = match futex_op {
            libc::FUTEX_WAKE_BITSET | libc::FUTEX_WAIT_BITSET => call.val3() as u32,
            _ => u32::MAX,
        };
        if bitset == 0 {
            return Err(Error::Errno(Errno::EINVAL));
        }
        let dettid = guest.thread_state().dettid;
        match futex_op {
            libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
                let num = match futex_action(
                    guest,
                    FutexAction::WakeRequest(call.val()),
                    &futexid,
                    init_val,
                    bitset,
                )
                .await
                .expect("futex wake must return value")
                {
                    SchedValue::Value(num) => num,
                    SchedValue::TimeOut | SchedValue::Interrupted => {
                        unreachable!("futex wake does not block")
                    }
                };
                trace!(
                    "[detcore, dtid {}] emulated futex wake committed, memory value is {}, expected {}",
                    &dettid,
                    guest.memory().read_value(ptr).unwrap(),
                    call.val(),
                );
                let _ = futex_action(
                    guest,
                    FutexAction::WakeFinished(0),
                    &futexid,
                    init_val,
                    bitset,
                )
                .await;
                Ok(num as i64)
            }
            libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
                if init_val != call.val() {
                    info!(
                        "[detcore, dtid {}] Futex wait running immediately because it will fizzle ({} != {}).",
                        &dettid,
                        init_val,
                        call.val()
                    );
                    Err(Error::Errno(Errno::EAGAIN))
                } else {
                    let maybe_timeout_lt = self
                        .futex_timeout_deadline(guest, call.futex_op(), call.timeout())
                        .await?;
                    self.handle_futex_wait_result(
                        guest,
                        &futexid,
                        init_val,
                        bitset,
                        maybe_timeout_lt,
                    )
                    .await
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            libc::FUTEX_REQUEUE => {
                self.handle_futex_requeue(guest, call, init_val, false)
                    .await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            libc::FUTEX_CMP_REQUEUE => self.handle_futex_requeue(guest, call, init_val, true).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            libc::FUTEX_WAKE_OP => self.handle_futex_wake_op(guest, call, init_val).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            libc::FUTEX_FD
            | libc::FUTEX_LOCK_PI
            | libc::FUTEX_UNLOCK_PI
            | libc::FUTEX_TRYLOCK_PI
            | libc::FUTEX_WAIT_REQUEUE_PI
            | libc::FUTEX_CMP_REQUEUE_PI
            | libc::FUTEX_LOCK_PI2 => Err(Error::Errno(Errno::ENOSYS)),
            _ => Err(Error::Errno(Errno::ENOSYS)),
        }
    }

    /// Futex system call, alternative implemenattion where we treat futexes as InternalIOPolling
    /// operations.
    pub async fn handle_futex_polling<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Futex,
        init_val: i32,
    ) -> Result<i64, Error> {
        fn make_futex_wake_request(dettid: DetTid) -> Resources {
            let mut rsrc = Resources::new(dettid);
            rsrc.fyi("futex_wake");
            rsrc
        }

        fn make_futex_wait_request(dettid: DetTid) -> Resources {
            let mut rsrc = Resources::new(dettid);
            rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
            rsrc.fyi("futex_wait");
            rsrc
        }

        let dettid = guest.thread_state().dettid;
        let futex_op = call.futex_op() & libc::FUTEX_CMD_MASK;
        match futex_op {
            libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
                let rsrc = make_futex_wake_request(dettid);
                resource_request(guest, rsrc.clone()).await; // Linearize this operation as a separate COMMIT.
                let res = guest.inject(call).await;
                // FIXME: With the non-blocking version of futex_wait, `res` will always be 0.  It
                // is quite difficult to tell how many polling waiters we unblocked with a given
                // wake, without going back to modeling futexes like `handle_futex_blocking` does.
                Ok(res?)
            }
            libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
                if init_val != call.val() {
                    info!(
                        "[detcore, dtid {}] Futex wait running immediately because it will fizzle ({} != {}).",
                        dettid,
                        init_val,
                        call.val()
                    );
                    let res = guest.inject(call).await;
                    Ok(res?)
                } else {
                    let rsrc = make_futex_wait_request(dettid);
                    let deadline = self
                        .futex_timeout_deadline(guest, call.futex_op(), call.timeout())
                        .await?;
                    let res =
                        retry_nonblocking_syscall_with_timeout(guest, call, rsrc, deadline).await?;
                    trace!(
                        "[detcore, dtid {}] after futex wait, memory value is {}",
                        &dettid,
                        guest.memory().read_value(call.uaddr().unwrap()).unwrap()
                    );
                    Ok(res)
                }
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            libc::FUTEX_REQUEUE
            | libc::FUTEX_CMP_REQUEUE
            | libc::FUTEX_WAKE_OP
            | libc::FUTEX_FD
            | libc::FUTEX_LOCK_PI
            | libc::FUTEX_UNLOCK_PI
            | libc::FUTEX_TRYLOCK_PI
            | libc::FUTEX_WAIT_REQUEUE_PI
            | libc::FUTEX_CMP_REQUEUE_PI
            | libc::FUTEX_LOCK_PI2 => Err(Error::Errno(Errno::ENOSYS)),
            _ => Err(Error::Errno(Errno::ENOSYS)),
        }
    }

    /// Execveat system call.  Doesn't return if successful.
    pub async fn handle_execveat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Execveat,
    ) -> Result<i64, Error> {
        let (old_metadata, old_memory_metadata, table_is_shared, dettid, old_mm_id) = {
            let thread_state = guest.thread_state();
            (
                Arc::clone(&thread_state.file_metadata),
                Arc::clone(&thread_state.memory_metadata),
                Arc::strong_count(&thread_state.file_metadata) > 1,
                thread_state.dettid,
                thread_state.mm_id,
            )
        };
        let (new_metadata, closed_open_files) = {
            let metadata = old_metadata.lock().unwrap();
            (
                metadata.for_exec(dettid),
                metadata.open_files_closed_on_exec(table_is_shared),
            )
        };

        {
            let thread_state = guest.thread_state_mut();
            thread_state.file_metadata = Arc::new(Mutex::new(new_metadata));
            thread_state.memory_metadata = Arc::new(Mutex::new(MemoryMetadata::new()));
            thread_state.mm_id = old_mm_id.for_exec(dettid);
        }

        let mut released_ports = Vec::new();
        for open_file_id in closed_open_files {
            if let Some(port) = self.release_port_for_open_file(guest, open_file_id).await {
                released_ports.push((open_file_id, port));
            }
        }

        // execve(2) doesn't return upon success.
        let errno = self.record_or_replay(guest, call).await.unwrap_err();

        for (open_file_id, port) in released_ports {
            self.restore_port_for_open_file(guest, open_file_id, port)
                .await;
        }

        {
            let thread_state = guest.thread_state_mut();
            thread_state.file_metadata = old_metadata;
            thread_state.memory_metadata = old_memory_metadata;
            thread_state.mm_id = old_mm_id;
        }

        Err(errno.into())
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#258): Confirm one-turn exclusion semantics across scheduler modes.
    /// End the current logical timeslice for a sequentialized sched_yield.
    pub async fn handle_sched_yield<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedYield,
    ) -> Result<i64, Error> {
        if self.cfg.sequentialize_threads {
            // In chaos mode, thread-interleaving diversity (and thus fairness)
            // comes entirely from re-randomizing thread priorities at
            // preemption-timer expirations. When timer preemption is disabled
            // (`--max-timeslice disabled`), priorities are fixed at thread
            // creation and never change. A plain yield only re-enqueues the
            // caller at the back of its own (fixed) priority level, so a thread
            // that spins on sched_yield while holding the numerically-lowest
            // priority is always reselected first and starves every thread it is
            // waiting on (GH #81). Treat sched_yield as an explicit chaos
            // reprioritization point: draw a fresh random priority for the
            // caller so it cedes the CPU and other runnable threads can make
            // progress. This mirrors what `end_timeslice` does at a timer-driven
            // preemption point, and is recorded for chaos replay.
            if self.cfg.chaos && self.cfg.max_timeslice.is_none() {
                let change_time = guest.thread_state().thread_logical_time.as_nanos();
                let request = Self::random_priority_changepoint_request(guest, change_time);
                resource_request(guest, request).await;
            } else if !self.cfg.chaos && self.cfg.replay_preemptions_from.is_some() {
                if self.cfg.max_timeslice.is_some() {
                    guest
                        .thread_state_mut()
                        .reset_timeslice_for_explicit_yield();
                }
                let request = Self::sched_yield_request(guest);
                resource_request(guest, request).await;
            } else if self.cfg.chaos || self.cfg.replay_schedule_from.is_some() {
                let request = Self::yield_request(guest);
                resource_request(guest, request).await;
            } else {
                self.end_timeslice_for_sched_yield(guest).await;
            }
            trace!("sched_yield yielded to the scheduler; NOT performing actual syscall");
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// wait4 system call
    /// This is handled by the scheduler and not passed to the record/replay layer.
    // TODO-HUMAN-REVIEW(PR-587): Confirm wait4 rusage canonicalization boundaries.
    pub async fn handle_wait4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Wait4,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let mut rsrc = Resources::new(dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("wait4");

        let value = if call.options().contains(WaitPidFlag::WNOHANG) {
            resource_request(guest, rsrc.clone()).await;
            info!(
                "[dtid {}] Executing non-blocking wait4 in one shot.",
                dettid
            );
            guest.inject_with_retry(call).await?
        } else {
            // wait4 is a scheduler poll, not a record/replay data read (see doc above),
            // so it is not routed through the record/replay subtool.
            retry_nonblocking_syscall(guest, call, rsrc, None).await?
        };
        if value > 0
            && let Some(rusage) = call.rusage()
        {
            // Host CPU and scheduling counters are not deterministic.
            let usage: libc::rusage = unsafe { std::mem::zeroed() };
            guest.memory().write_value(rusage, &usage)?;
        }
        Ok(value)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#274): Review waitid polling and compatibility boundaries.
    /// waitid system call
    /// This is handled by the scheduler and not passed to the record/replay layer.
    pub async fn handle_waitid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Waitid,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let mut rsrc = Resources::new(dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("waitid");

        let event_options = libc::WEXITED | libc::WSTOPPED | libc::WCONTINUED;
        let allowed_options = event_options
            | libc::WNOHANG
            | libc::WNOWAIT
            | libc::__WNOTHREAD
            | libc::__WALL
            | libc::__WCLONE;
        if call.options() & event_options == 0 || call.options() & !allowed_options != 0 {
            return Err(Errno::EINVAL.into());
        }

        // POSIX requires non-null infop. Linux accepts null, but that form can
        // expose host rusage and requires backend-neutral scratch memory for
        // deterministic polling. Reject it uniformly instead of diverging or
        // panicking on DBI's unsupported scratch stack.
        if call.info().is_none() {
            return Err(Errno::EFAULT.into());
        }

        // Linux snapshots P_PGID with id 0 at syscall entry. Preserve that
        // identity across polling calls without issuing a guest-visible syscall.
        if call.which() == libc::P_PGID as i32 && call.pid() == 0 {
            call = call.with_pid(snapshot_process_group(guest.pid())?);
        }

        // A blocking waitid on an O_NONBLOCK pidfd must return EAGAIN rather
        // than being converted to WNOHANG. Acquire the scheduler resource first,
        // then snapshot fdinfo and issue the one-shot wait without another yield.
        let pidfd_nonblocking =
            if call.which() == libc::P_PIDFD as i32 && call.options() & libc::WNOHANG == 0 {
                resource_request(guest, rsrc.clone()).await;
                guest_fd_status_flags(guest.pid(), call.pid())? & libc::O_NONBLOCK != 0
            } else {
                false
            };
        if call.which() == libc::P_PIDFD as i32
            && call.options() & libc::WNOHANG == 0
            && !pidfd_nonblocking
        {
            // Polling a numeric pidfd cannot preserve Linux's held file
            // reference if another thread closes and reuses the descriptor.
            // Reject the blocking form until Detcore can retain that identity.
            return Err(Errno::EOPNOTSUPP.into());
        }
        let info = call.info().expect("waitid infop checked above");

        // Unlike wait4, waitid returns zero both when it reports a child event and
        // when WNOHANG finds nothing. Polling must inspect si_pid to distinguish
        // those cases.
        // Known limitation: without backend-neutral scratch memory, an invalid
        // non-null infop faults on the first physical poll rather than after a
        // child becomes waitable.
        // siginfo_t has no portable initializer. An all-zero value is the
        // waitid WNOHANG sentinel defined by POSIX and Linux.
        let empty_info: libc::siginfo_t = unsafe { std::mem::zeroed() };

        if call.options() & libc::WNOHANG != 0 || pidfd_nonblocking {
            if !pidfd_nonblocking {
                resource_request(guest, rsrc).await;
            }
            info!(
                "[dtid {}] Executing non-blocking waitid in one shot.",
                dettid
            );
            guest.memory().write_value(info, &empty_info)?;
            let value = guest.inject_with_retry(call).await?;
            let mut info_value: libc::siginfo_t = guest.memory().read_value(info)?;
            // SAFETY: waitid writes either zeroed output or the SIGCHLD
            // siginfo_t variant, for which libc exposes si_pid.
            if unsafe { info_value.si_pid() } != 0 {
                canonicalize_waitid_siginfo(&mut info_value);
                guest.memory().write_value(info, &info_value)?;
                if let Some(rusage) = call.rusage() {
                    // Host CPU and scheduling counters are not deterministic.
                    let usage: libc::rusage = unsafe { std::mem::zeroed() };
                    guest.memory().write_value(rusage, &usage)?;
                }
            }
            return Ok(value);
        }

        let poll_call = call.with_options(call.options() | libc::WNOHANG);
        let mut first_poll = true;
        loop {
            let signaled = !first_poll
                && resource_request(guest, rsrc.clone()).await == ResumeStatus::Signaled;
            first_poll = false;

            guest.memory().write_value(info, &empty_info)?;
            let result = guest.inject_with_retry(poll_call).await;
            match result {
                Ok(value) => {
                    let mut info_value: libc::siginfo_t = guest.memory().read_value(info)?;
                    // waitid writes the SIGCHLD variant of siginfo_t. A zeroed
                    // structure is used only for the no-event WNOHANG result.
                    let child_pid = unsafe { info_value.si_pid() };
                    if child_pid != 0 {
                        canonicalize_waitid_siginfo(&mut info_value);
                        guest.memory().write_value(info, &info_value)?;
                        if let Some(rusage) = call.rusage() {
                            // Host CPU and scheduling counters are not deterministic.
                            let usage: libc::rusage = unsafe { std::mem::zeroed() };
                            guest.memory().write_value(rusage, &usage)?;
                        }
                        return Ok(value);
                    }

                    if signaled {
                        return Err(Errno::ERESTARTSYS.into());
                    }
                    rsrc.poll_attempt += 1;
                    trace!(
                        "Retry #{} for waitid because no child state is ready",
                        rsrc.poll_attempt
                    );
                    record_retry_event(guest, poll_call).await;
                }
                Err(errno) => return Err(errno.into()),
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#546)
    /// Accept valid affinity masks without changing the host scheduler.
    pub async fn handle_sched_setaffinity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedSetaffinity,
    ) -> Result<i64, Error> {
        let size_bytes = call.len() as usize;
        if size_bytes == 0 {
            return Err(Errno::EINVAL.into());
        }

        let mask = call.mask().ok_or(Errno::EFAULT)?;
        let mask: Addr<u8> = mask.cast();
        let mut requested = [0u8; VIRTUAL_CPUSET_BYTES];
        let bytes_to_read = size_bytes.min(VIRTUAL_CPUSET_BYTES);
        guest
            .memory()
            .read_exact(mask, &mut requested[..bytes_to_read])?;
        info!(
            "Suppressing sched_setaffinity mask {:?}; affinity remains virtual CPU 0",
            requested
        );
        Ok(0)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#546)
    /// Report that we are on cpu 0, irrespective of what physical CPU we are on.
    pub async fn handle_sched_getaffinity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::SchedGetaffinity,
    ) -> Result<i64, Error> {
        let size_bytes: usize = call.len() as usize;
        if size_bytes < VIRTUAL_CPUSET_BYTES
            || !size_bytes.is_multiple_of(std::mem::size_of::<libc::c_ulong>())
        {
            return Err(Errno::EINVAL.into());
        }

        // N.B. we can't use an opaque, type-safe representation such as
        // nix::sched::CpuSet currently.  The problem is that the
        // SchedGetAffinity syscall treats this field as a u64.
        let mut cpu_set = [0u8; VIRTUAL_CPUSET_BYTES];
        cpu_set[0] = 1;

        info!(
            "Suppressing sched_getaffinity and returning {}-byte virtualized result, {:?}",
            VIRTUAL_CPUSET_BYTES, cpu_set
        );
        if let Some(mask) = call.mask() {
            let mask: AddrMut<u8> = mask.cast();
            guest.memory().write_exact(mask, &cpu_set)?;
            // From the man page:
            // > On success, the raw sched_getaffinity() system call returns the size (in bytes) of
            // > the cpumask_t data type that is used internally by the kernel to represent the CPU
            // > set bit mask.
            Ok(VIRTUAL_CPUSET_BYTES as i64)
        } else {
            Err(Error::Errno(Errno::EFAULT))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waitid_siginfo_canonicalization_clears_only_cpu_accounting() {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        info.si_signo = libc::SIGCHLD;
        info.si_code = libc::CLD_EXITED;
        // SAFETY: This uses the same Linux SIGCHLD layout mirror validated by
        // canonicalize_waitid_siginfo.
        let fields = unsafe {
            &mut (*(std::ptr::addr_of_mut!(info)).cast::<WaitidSiginfoHead>())
                .fields
                .sigchld
        };
        fields.pid = 123;
        fields.uid = 456;
        fields.status = 7;
        fields.utime = 8;
        fields.stime = 9;

        canonicalize_waitid_siginfo(&mut info);

        assert_eq!(info.si_signo, libc::SIGCHLD);
        assert_eq!(info.si_code, libc::CLD_EXITED);
        assert_eq!(unsafe { info.si_pid() }, 123);
        assert_eq!(unsafe { info.si_uid() }, 456);
        assert_eq!(unsafe { info.si_status() }, 7);
        assert_eq!(unsafe { info.si_utime() }, 0);
        assert_eq!(unsafe { info.si_stime() }, 0);
    }

    #[test]
    fn futex_timeout_units_and_modes_match_linux() {
        let timeout = Timespec {
            tv_sec: 2,
            tv_nsec: 3,
        };
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT, timeout),
            Ok(FutexTimeout::Relative(2_000_000_003))
        );
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT_BITSET, timeout),
            Ok(FutexTimeout::Absolute(LogicalTime::from_nanos(
                2_000_000_003
            )))
        );
        // The command bits must be matched after masking off FUTEX_PRIVATE_FLAG
        // (and FUTEX_CLOCK_REALTIME): a private FUTEX_WAIT_BITSET still uses an
        // absolute deadline, and a private FUTEX_WAIT still uses a relative one.
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG, timeout),
            Ok(FutexTimeout::Absolute(LogicalTime::from_nanos(
                2_000_000_003
            )))
        );
        assert_eq!(
            parse_futex_timeout(libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG, timeout),
            Ok(FutexTimeout::Relative(2_000_000_003))
        );
    }

    #[test]
    fn futex_wait_results_do_not_alias_replay_timeslices_and_signals() {
        assert_eq!(decode_futex_wait_result(None), Ok(0));
        assert_eq!(decode_futex_wait_result(Some(SchedValue::Value(4))), Ok(0));
        assert_eq!(
            decode_futex_wait_result(Some(SchedValue::TimeOut)),
            Err(Errno::ETIMEDOUT)
        );
        assert_eq!(
            decode_futex_wait_result(Some(SchedValue::Interrupted)),
            Err(Errno::EINTR)
        );
    }

    #[test]
    fn robust_pointer_decoding_preserves_pi_tag() {
        assert_eq!(decode_robust_pointer(0), (0, false));
        assert_eq!(decode_robust_pointer(0x1000), (0x1000, false));
        assert_eq!(decode_robust_pointer(0x1001), (0x1000, true));
    }

    #[test]
    fn only_default_terminating_signals_trigger_group_cleanup() {
        assert!(default_signal_action_terminates(libc::SIGKILL));
        assert!(default_signal_action_terminates(libc::SIGTERM));
        assert!(default_signal_action_terminates(libc::SIGRTMIN()));
        assert!(!default_signal_action_terminates(libc::SIGCHLD));
        assert!(!default_signal_action_terminates(libc::SIGSTOP));
    }

    #[test]
    fn clone_untraced_is_removed_from_guest_flags() {
        let flags =
            CloneFlags::CLONE_UNTRACED | CloneFlags::CLONE_VM | CloneFlags::CLONE_CHILD_CLEARTID;
        assert_eq!(
            sanitize_clone_flags(flags),
            CloneFlags::CLONE_VM | CloneFlags::CLONE_CHILD_CLEARTID
        );
    }

    #[test]
    fn clone3_copy_accepts_versioned_and_forward_compatible_sizes() {
        let page_size = 4096;
        assert!(!clone3_args_size_is_copyable(63, page_size));
        for size in [64, 80, 88, 96, page_size] {
            assert!(clone3_args_size_is_copyable(size, page_size));
        }
        assert!(!clone3_args_size_is_copyable(page_size + 1, page_size));
    }

    #[test]
    fn futex_wake_op_decodes_signed_arguments_and_comparisons() {
        let set_one_if_zero = 1_u32 << 12;
        assert_eq!(apply_futex_wake_op(set_one_if_zero, 0), Ok((1, true)));
        assert_eq!(apply_futex_wake_op(set_one_if_zero, 7), Ok((1, false)));

        let add_minus_one_if_positive = (1_u32 << 28) | (4_u32 << 24) | (0x0fff_u32 << 12);
        assert_eq!(
            apply_futex_wake_op(add_minus_one_if_positive, 3),
            Ok((2, true))
        );
        assert_eq!(apply_futex_wake_op(7_u32 << 28, 0), Err(Errno::ENOSYS));
    }

    #[test]
    fn futex2_flags_accept_only_supported_u32_words() {
        assert_eq!(validate_futex2_flags(FUTEX2_SIZE_U32), Ok(false));
        assert_eq!(
            validate_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_PRIVATE),
            Ok(true)
        );
        assert_eq!(validate_futex2_flags(0), Err(Errno::EINVAL));
        assert_eq!(
            validate_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_NUMA),
            Err(Errno::ENOSYS)
        );
        assert_eq!(validate_futex2_flags(0x10), Err(Errno::EINVAL));
    }
    #[test]
    fn absolute_futex_timeout_is_rebased_to_logical_time() {
        let logical_now = LogicalTime::from_secs(100);
        let clock_now = LogicalTime::from_secs(5_000);
        let deadline = clock_now + Duration::from_millis(100);
        assert_eq!(
            rebase_absolute_timeout(deadline, clock_now, logical_now),
            logical_now + Duration::from_millis(100)
        );
        assert_eq!(
            rebase_absolute_timeout(
                clock_now - LogicalTime::from_nanos(1),
                clock_now,
                logical_now
            ),
            logical_now
        );
    }

    #[test]
    fn futex_timeout_rejects_invalid_timespecs() {
        assert_eq!(
            parse_futex_timeout(
                libc::FUTEX_WAIT,
                Timespec {
                    tv_sec: -1,
                    tv_nsec: 0,
                },
            ),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            parse_futex_timeout(
                libc::FUTEX_WAIT_BITSET,
                Timespec {
                    tv_sec: 0,
                    tv_nsec: 1_000_000_000,
                },
            ),
            Err(Errno::EINVAL)
        );
    }
}
