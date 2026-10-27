/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Process and thread IDs that Detcore chooses (`GuestIds::Chosen`).
//!
//! Without a PID namespace of its own (`--no-namespace`), a guest would get
//! the host's next free ID for every task, a different one in every run. With
//! the capability to choose IDs (`CAP_SYS_ADMIN` or `CAP_CHECKPOINT_RESTORE`
//! over the host's PID namespace), Detcore gives the guest's first process
//! the run's base ID and issues every later clone-family call as `clone3`
//! with `set_tid` naming the next ID, so the kernel itself hands out IDs that
//! depend only on the base and the guest's own order of creation. The kernel
//! then writes those IDs wherever it writes a task's ID: clone's return value
//! and `CLONE_PARENT_SETTID`/`CLONE_CHILD_SETTID` words, `getpid` and
//! `gettid`, `/proc`, wait results and `si_pid`, PI-futex owner words and
//! robust-list cleanup. Nothing needs translating.
//!
//! `clone3` is given exactly the arguments Linux's own `clone`, `fork` and
//! `vfork` pass to `kernel_clone` (kernel/fork.c), so the task created is the
//! one the guest's call would have created, with the chosen ID. A call
//! `clone3` cannot express, or one whose `clone3` could fare differently
//! (under a seccomp filter of the guest's own, or creating its task in a PID
//! namespace of the guest's), goes to the kernel as the guest made it, and
//! its task gets a host ID: a run that needs every ID chosen (`--strict`, an
//! explicit `--pid-base`) then ends, and any other run says its IDs are not
//! deterministic (`GlobalRequest::GaveHostId`).

use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::warn;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

/// `struct clone_args` through `cgroup`: `CLONE_ARGS_SIZE_VER2`
/// (include/uapi/linux/sched.h), the largest size Detcore rewrites.
pub(crate) const CLONE_ARGS_SIZE_VER2: usize = 88;
/// Through `set_tid_size`, the first size that carries `set_tid`.
const CLONE_ARGS_SIZE_VER1: usize = 80;
/// The first version, through `tls`: the smallest size the kernel accepts.
const CLONE_ARGS_SIZE_VER0: usize = 64;
const SET_TID_OFFSET: usize = 64;
const SET_TID_SIZE_OFFSET: usize = 72;

/// What becomes of a clone-family call when Detcore chooses the IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChosenIdCall {
    /// Issue `clone3` with these `struct clone_args` bytes, of which the
    /// first `size` are passed, once `set_tid` is filled in
    /// ([`ChosenIdCall::with_set_tid`]).
    Clone3 {
        args: [u8; CLONE_ARGS_SIZE_VER2],
        size: usize,
    },
    /// Issue the call as the guest made it: the kernel refuses it before it
    /// creates a task, so no ID is needed, and `clone3` could refuse it with
    /// another error.
    Unchanged,
    /// A guest `clone3` that passes `set_tid` itself, with its `struct
    /// clone_args` (the first 88 bytes, zero extended): its ID is the guest's
    /// choice if the array reaches Hermit's PID namespace
    /// (`Detcore::plan_clone_id`).
    GuestChosen { args: [u8; CLONE_ARGS_SIZE_VER2] },
    /// The call creates a task, and `clone3` cannot express it; the reason.
    Inexpressible(String),
}

impl ChosenIdCall {
    /// The arguments with `set_tid` pointing at `set_tid_address`, where the
    /// chosen ID is, and `set_tid_size` 1.
    pub(crate) fn with_set_tid(
        mut args: [u8; CLONE_ARGS_SIZE_VER2],
        set_tid_address: u64,
    ) -> [u8; CLONE_ARGS_SIZE_VER2] {
        args[SET_TID_OFFSET..SET_TID_OFFSET + 8].copy_from_slice(&set_tid_address.to_ne_bytes());
        args[SET_TID_SIZE_OFFSET..SET_TID_SIZE_OFFSET + 8].copy_from_slice(&1u64.to_ne_bytes());
        args
    }
}

/// The `clone3` form of the clone-family call `(sysno, args)`. A `clone3`
/// call's own `struct clone_args` is `clone3_args`, as many bytes as the call
/// passes, or `None` if they could not be read.
///
/// For `clone`, `fork` and `vfork` the fields are the `kernel_clone_args`
/// those calls build (kernel/fork.c, `SYSCALL_DEFINE5(clone)`, `fork`,
/// `vfork`), adjusted where `clone3`'s own checks (`clone3_args_valid`,
/// `copy_clone_args_from_user`) are stricter than what the kernel does with
/// the field anyway:
///
/// - `CLONE_DETACHED`, which `clone3` refuses, is dropped: `copy_process`
///   ignores it except beside `CLONE_PIDFD`, where it fails the call, which
///   is then left unchanged.
/// - The exit signal is 0 beside `CLONE_THREAD` or `CLONE_PARENT`, which
///   `clone3` requires: `copy_process` then takes -1 or the parent's exit
///   signal whatever was passed.
/// - A new stack pointer `sp` becomes `stack = sp - 1, stack_size = 1`:
///   `clone3_stack_valid` starts the child at `stack + stack_size`.
///
/// An exit signal above 64 is inexpressible: `clone` accepts it and `clone3`
/// does not. A `clone3` call is rewritten only when it passes no `set_tid` of
/// its own and its size is one of the three versions; Detcore then fills in
/// `set_tid`, extending a first-version struct to the second.
pub(crate) fn clone3_form(
    sysno: Sysno,
    args: &SyscallArgs,
    clone3_args: Option<&[u8]>,
) -> ChosenIdCall {
    let fields = match sysno {
        #[cfg(not(target_arch = "aarch64"))]
        Sysno::fork => LegacyClone {
            flags: 0,
            exit_signal: libc::SIGCHLD as u64,
            ..LegacyClone::default()
        },
        #[cfg(not(target_arch = "aarch64"))]
        Sysno::vfork => LegacyClone {
            flags: (libc::CLONE_VFORK | libc::CLONE_VM) as u64,
            exit_signal: libc::SIGCHLD as u64,
            ..LegacyClone::default()
        },
        Sysno::clone => LegacyClone::of_clone(args, CloneArgOrder::NATIVE),
        Sysno::clone3 => return clone3_with_room_for_set_tid(clone3_args),
        _ => return ChosenIdCall::Unchanged,
    };
    fields.as_clone3()
}

/// The `kernel_clone_args` fields a `clone`, `fork` or `vfork` builds.
#[derive(Clone, Copy, Debug, Default)]
struct LegacyClone {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    tls: u64,
}

/// The order of `clone`'s last two arguments (kernel/fork.c,
/// `SYSCALL_DEFINE5(clone)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloneArgOrder {
    /// `clone(flags, newsp, parent_tidptr, child_tidptr, tls)`: x86-64.
    #[cfg_attr(
        target_arch = "aarch64",
        allow(
            dead_code,
            reason = "the other architecture's order, built for its test"
        )
    )]
    ChildTidThenTls,
    /// `clone(flags, newsp, parent_tidptr, tls, child_tidptr)`: arm64
    /// (`CONFIG_CLONE_BACKWARDS`).
    #[cfg_attr(
        not(target_arch = "aarch64"),
        allow(
            dead_code,
            reason = "the other architecture's order, built for its test"
        )
    )]
    TlsThenChildTid,
}

impl CloneArgOrder {
    #[cfg(target_arch = "aarch64")]
    const NATIVE: Self = CloneArgOrder::TlsThenChildTid;
    #[cfg(not(target_arch = "aarch64"))]
    const NATIVE: Self = CloneArgOrder::ChildTidThenTls;
}

impl LegacyClone {
    /// `SYSCALL_DEFINE5(clone)`: the low 32 bits of the flags, less
    /// `CSIGNAL`, which is the exit signal; the parent TID address serves as
    /// the pidfd address too; the last two arguments in `order`.
    fn of_clone(args: &SyscallArgs, order: CloneArgOrder) -> Self {
        let low = args.arg0 as u32 as u64;
        let csignal = libc::CSIGNAL as u64;
        let (child_tid, tls) = match order {
            CloneArgOrder::ChildTidThenTls => (args.arg3, args.arg4),
            CloneArgOrder::TlsThenChildTid => (args.arg4, args.arg3),
        };
        LegacyClone {
            flags: low & !csignal,
            pidfd: args.arg2 as u64,
            child_tid: child_tid as u64,
            parent_tid: args.arg2 as u64,
            exit_signal: low & csignal,
            stack: args.arg1 as u64,
            tls: tls as u64,
        }
    }

    fn as_clone3(self) -> ChosenIdCall {
        let detached = libc::CLONE_DETACHED as u64;
        let pidfd = libc::CLONE_PIDFD as u64;
        // copy_process: CLONE_PIDFD beside CLONE_DETACHED is EINVAL, before
        // any task exists. (Beside CLONE_THREAD it is EINVAL only before
        // Linux 6.9; the rewritten call then fails the same way, and the
        // guest's own call decides.)
        if self.flags & pidfd != 0 && self.flags & detached != 0 {
            return ChosenIdCall::Unchanged;
        }
        let shares_parent = (libc::CLONE_THREAD | libc::CLONE_PARENT) as u64;
        let exit_signal = if self.flags & shares_parent != 0 {
            0
        } else {
            self.exit_signal
        };
        if exit_signal > 64 {
            return ChosenIdCall::Inexpressible(format!(
                "its exit signal {exit_signal} is not a signal number, which clone3 refuses"
            ));
        }
        let (stack, stack_size) = match self.stack {
            0 => (0, 0),
            sp => (sp - 1, 1),
        };
        let fields: [u64; 11] = [
            self.flags & !detached,
            self.pidfd,
            self.child_tid,
            self.parent_tid,
            exit_signal,
            stack,
            stack_size,
            self.tls,
            0, // set_tid, filled in later
            0, // set_tid_size
            0, // cgroup
        ];
        let mut args = [0u8; CLONE_ARGS_SIZE_VER2];
        for (chunk, field) in args.as_chunks_mut::<8>().0.iter_mut().zip(fields) {
            *chunk = field.to_ne_bytes();
        }
        ChosenIdCall::Clone3 {
            args,
            size: CLONE_ARGS_SIZE_VER1,
        }
    }
}

/// A guest's own `clone3` arguments, extended to carry `set_tid`.
///
/// `copy_clone_args_from_user` reads the first `min(size, 88)` bytes, zero
/// extends a shorter struct, and refuses a longer one whose extra bytes are
/// not all zero (`E2BIG`); a struct shorter than the first version is
/// `EINVAL`. So a struct of any size that the kernel accepts is the
/// second-version struct of its first 88 bytes, zero extended, and `set_tid`
/// is unset exactly when bytes 64 to 79 are zero: then Detcore fills it in.
fn clone3_with_room_for_set_tid(bytes: Option<&[u8]>) -> ChosenIdCall {
    let Some(bytes) = bytes else {
        // An unreadable struct is the kernel's EFAULT; one over a page, E2BIG.
        return ChosenIdCall::Unchanged;
    };
    let size = bytes.len();
    if size < CLONE_ARGS_SIZE_VER0 {
        // copy_clone_args_from_user: EINVAL.
        return ChosenIdCall::Unchanged;
    }
    if bytes
        .iter()
        .skip(CLONE_ARGS_SIZE_VER2)
        .any(|byte| *byte != 0)
    {
        // copy_clone_args_from_user: E2BIG.
        return ChosenIdCall::Unchanged;
    }
    let mut args = [0u8; CLONE_ARGS_SIZE_VER2];
    let known = size.min(CLONE_ARGS_SIZE_VER2);
    args[..known].copy_from_slice(&bytes[..known]);
    if args[SET_TID_OFFSET..CLONE_ARGS_SIZE_VER1]
        .iter()
        .any(|byte| *byte != 0)
    {
        // The guest names its task's ID itself, or passes `set_tid` and
        // `set_tid_size` the kernel refuses (`Detcore::plan_clone_id` tells
        // which).
        return ChosenIdCall::GuestChosen { args };
    }
    ChosenIdCall::Clone3 {
        args,
        size: known.max(CLONE_ARGS_SIZE_VER1),
    }
}

/// A clone-family call that is to create its task with a chosen ID.
#[derive(Debug)]
pub(crate) struct ChosenId {
    /// The ID taken for the task.
    id: i32,
    /// The call's fingerprint (`RestartedCloneId::fingerprint`), which a
    /// restart keeps its ID under.
    call: u64,
    /// `struct clone_args`, `set_tid` still unset.
    args: [u8; CLONE_ARGS_SIZE_VER2],
    /// How many of those bytes `clone3` is given.
    size: usize,
    /// The guest's call passed `CLONE_UNTRACED`, which `args` no longer do.
    untraced: bool,
}

/// How a clone-family call is issued, as [`Detcore::plan_clone_id`] decides.
#[derive(Debug)]
pub(crate) enum ClonePlan {
    /// As the guest made it, the kernel numbering its task: the run's PID
    /// namespace does, or the host does (`GuestIds::Namespace`, `Host`).
    Kernel,
    /// As `clone3` with `set_tid`.
    Chosen(ChosenId),
    /// As the guest made it, though Detcore chooses the IDs: the kernel is
    /// expected to refuse it, or its task gets a host ID, for `reason`. A task
    /// it does create is reported ([`Detcore::gave_host_id`]).
    AsMade { reason: String },
    /// As the guest made it: a guest `clone3` that names its task's ID in
    /// Hermit's PID namespace itself, or passes a `set_tid` the kernel
    /// refuses before creating a task.
    GuestChosen,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Decide how the guest's clone-family call `call` is issued, and, when
    /// Detcore chooses the IDs (`GuestIds::Chosen`), take the ID of the task
    /// it is to create: in the caller's turn, before anything the call does
    /// could let another thread run, so with sequentialized threads the order
    /// of the IDs is the guest's own order of calls. A call that was
    /// restarted keeps the ID it took (`ThreadState::restarted_clone_id`), if
    /// it comes back with the same arguments.
    ///
    /// A call that would create its task with a host ID ends a run that needs
    /// every ID chosen here, before any task exists. A guest `clone3` with a
    /// `set_tid` the kernel refuses goes to the kernel first, so its error
    /// stays the guest's answer.
    pub(crate) async fn plan_clone_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Result<ClonePlan, Error> {
        let detcore_model::pid::GuestIds::Chosen {
            refuse_host_ids, ..
        } = self.cfg.guest_ids
        else {
            return Ok(ClonePlan::Kernel);
        };
        let (sysno, args) = call.into_parts();
        let clone3_args = match call {
            Syscall::Clone3(clone) => read_clone_args(&guest.memory(), clone),
            _ => None,
        };
        let form = clone3_form(sysno, &args, clone3_args.as_deref());
        // A restarted call's ID is for that call only: taken here whatever
        // the plan, so that no other call (a signal handler's clone, or the
        // restarted call changed into another plan) inherits it.
        let restarted = guest.thread_state_mut().restarted_clone_id.take();
        let (args, size) = match form {
            ChosenIdCall::Clone3 { args, size } => (args, size),
            ChosenIdCall::GuestChosen { args } => {
                // A call the kernel refuses before creating a task goes to the
                // kernel, whose error is the guest's answer.
                let tid = guest.tid().as_raw();
                let Some(tids) = guest_set_tid(&guest.memory(), &args, tid) else {
                    return Ok(ClonePlan::GuestChosen);
                };
                let flags = u64::from_ne_bytes(args[..8].try_into().unwrap());
                if let Some(why) = untraced_clone(flags) {
                    // Detcore would neither see the task nor rewrite the
                    // guest's own array, so not even a run that takes host IDs
                    // can go on: the task would run outside the scheduler.
                    refuse_host_id(
                        guest,
                        format!("this clone3 with set_tid cannot be traced: {why}"),
                    )
                    .await
                }
                // The array runs from the new task's PID namespace outwards:
                // the guest chose the task's ID in Hermit's namespace only if
                // it is long enough to reach it.
                let why = match pid_namespace_depth(tid, flags) {
                    Some(depth) if tids.len() > depth => return Ok(ClonePlan::GuestChosen),
                    Some(depth) => format!(
                        "its set_tid names {} ID(s), but its task is created {depth} PID \
                         namespace(s) below Hermit's, so the kernel picks its ID in Hermit's",
                        tids.len()
                    ),
                    None => "the depth of the PID namespace its task is created in could not \
                             be read, so whether its set_tid reaches Hermit's namespace is \
                             unknown"
                        .to_owned(),
                };
                return self
                    .host_id_planned(guest, sysno, why, refuse_host_ids)
                    .await;
            }
            ChosenIdCall::Unchanged => {
                return Ok(ClonePlan::AsMade {
                    reason: "Detcore expected the kernel to refuse it".to_owned(),
                });
            }
            ChosenIdCall::Inexpressible(why) => {
                return self
                    .host_id_planned(guest, sysno, why, refuse_host_ids)
                    .await;
            }
        };
        let flags = u64::from_ne_bytes(args[..8].try_into().unwrap());
        if let Some(why) = self.clone3_could_fare_otherwise(guest, flags) {
            return self
                .host_id_planned(guest, sysno, why, refuse_host_ids)
                .await;
        }
        // A task Detcore numbers must be traced: its own clones reach Detcore
        // only then. `CLONE_UNTRACED` only keeps a tracer from following the
        // task, and the guest's tasks are Hermit's to trace, so the rewrite
        // drops it (as https://github.com/rrnewton/hermit/pull/4660 does for
        // every clone). Nothing else in the call depends on it.
        let untraced = flags & libc::CLONE_UNTRACED as u64 != 0;
        let mut args = args;
        args[..8].copy_from_slice(&(flags & !(libc::CLONE_UNTRACED as u64)).to_ne_bytes());
        let call = RestartedCloneId::fingerprint(sysno, &args, size);
        let id = match restarted {
            Some(restarted) if restarted.call == call => restarted.id,
            _ => match crate::tool_global::next_guest_id(guest).await {
                Some(id) => id,
                // Both sides read the same configuration, so this does not
                // happen; if it did, the task would get the kernel's ID.
                None => {
                    return self
                        .host_id_planned(
                            guest,
                            sysno,
                            "Detcore's global state chose no ID".to_owned(),
                            refuse_host_ids,
                        )
                        .await;
                }
            },
        };
        Ok(ClonePlan::Chosen(ChosenId {
            id,
            call,
            args,
            size,
            untraced,
        }))
    }

    /// Why the caller's `clone3` with these flags could fare otherwise than
    /// its own call, if it could: a seccomp filter the guest installed (only
    /// Reverie's and those Hermit inherited are known) could trap, kill or
    /// refuse `clone3` where it allows the guest's call; and in a PID
    /// namespace of the guest's own, `set_tid` would name an ID inside it.
    fn clone3_could_fare_otherwise<G: Guest<Self>>(&self, guest: &G, flags: u64) -> Option<String> {
        let tid = guest.tid().as_raw();
        if let Some(why) = nested_pid_namespace(tid, flags) {
            return Some(why);
        }
        let expected = match self.cfg.inherited_seccomp {
            // Reverie's own filter, and those Hermit runs under.
            detcore_model::config::InheritedSeccomp::Disabled => 1,
            detcore_model::config::InheritedSeccomp::Filter(Some(filters)) => filters + 1,
            _ => {
                return Some(
                    "Hermit could not count the seccomp filters it runs under, so a filter of \
                     the guest's own could not be told from them"
                        .to_owned(),
                );
            }
        };
        let status = std::fs::read_to_string(format!("/proc/{tid}/status")).unwrap_or_default();
        let filters = status
            .lines()
            .find_map(|line| line.strip_prefix("Seccomp_filters:"))
            .and_then(|count| count.trim().parse::<u32>().ok());
        match filters {
            Some(filters) if filters <= expected => None,
            Some(_) => Some(
                "the guest installed a seccomp filter, which could trap, kill or refuse clone3 \
                 where it allows this call"
                    .to_owned(),
            ),
            None => Some(
                "this kernel does not report the task's seccomp filters (Linux 5.9 and later \
                 do), so one of the guest's own could not be ruled out"
                    .to_owned(),
            ),
        }
    }

    /// A call whose task will get a host ID, for `why`: the end of a run that
    /// needs every ID chosen, else a plan to issue it as made.
    async fn host_id_planned<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        why: String,
        refuse_host_ids: bool,
    ) -> Result<ClonePlan, Error> {
        let reason = format!("this {sysno} cannot be given a chosen process ID: {why}");
        if refuse_host_ids {
            refuse_host_id(guest, reason).await
        }
        Ok(ClonePlan::AsMade { reason })
    }

    /// Report that the guest's clone-family call created task `child` with a
    /// host ID, for `reason`: the end of a run that needs every ID chosen,
    /// else a warning and a count, which the summary shows and which marks
    /// the preemption record `GuestIds::Host`, so that a replay refuses it.
    pub(crate) async fn gave_host_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        child: i64,
        reason: &str,
    ) {
        let reason = format!("task {child} got a host ID, not a chosen one: {reason}");
        if matches!(
            self.cfg.guest_ids,
            detcore_model::pid::GuestIds::Chosen {
                refuse_host_ids: true,
                ..
            }
        ) {
            refuse_host_id(guest, reason).await
        }
        warn!(
            "[detcore, dtid {}] {reason}; that ID differs from run to run",
            guest.thread_state().dettid
        );
        crate::tool_global::note_host_id_task(guest).await;
    }

    /// Issue the guest's clone-family call `call` as `plan` says.
    pub(crate) async fn inject_clone_planned<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        plan: ClonePlan,
    ) -> Result<i64, Errno> {
        match plan {
            ClonePlan::Kernel | ClonePlan::GuestChosen => guest.inject(call).await,
            ClonePlan::AsMade { reason } => {
                let result = guest.inject(call).await;
                if let Ok(child) = result {
                    self.gave_host_id(guest, child, &reason).await;
                }
                result
            }
            ClonePlan::Chosen(chosen) => self.inject_with_chosen_id(guest, call, chosen).await,
        }
    }

    /// Issue the guest's clone-family call `call` as `clone3` creating its
    /// task with the ID `chosen` names, through scratch memory in the guest.
    ///
    /// When `clone3` fails, or its scratch memory cannot be written, the
    /// guest's own call decides the outcome, as if Detcore had not rewritten
    /// it: an error it also gets is the guest's answer, and a task it creates
    /// has a host ID ([`Self::gave_host_id`]). Where the failure is plainly
    /// `set_tid`'s own (the ID taken, or not one this caller may choose), a
    /// run that needs every ID chosen ends first, before any task gets a host
    /// ID. A restart keeps the ID for the restarted call.
    async fn inject_with_chosen_id<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        chosen: ChosenId,
    ) -> Result<i64, Errno> {
        let refuse_host_ids = matches!(
            self.cfg.guest_ids,
            detcore_model::pid::GuestIds::Chosen {
                refuse_host_ids: true,
                ..
            }
        );
        let ChosenId {
            id,
            call: fingerprint,
            args,
            size,
            untraced,
        } = chosen;
        let flags = u64::from_ne_bytes(args[..8].try_into().unwrap());
        // An ID at or above pid_max is EINVAL from set_tid (alloc_pid), which
        // the guest's own call would not get; decided here, before any call.
        if let Some(pid_max) = pid_max()
            && id >= pid_max
        {
            let cause = format!("ID {id} is not below pid_max, {pid_max}");
            if refuse_host_ids {
                refuse_host_id(
                    guest,
                    format!(
                        "this {} cannot create its task with its chosen ID: {cause}. The guest \
                         has created more tasks than the IDs from its base to pid_max hold; the \
                         run needs a lower base",
                        call.number()
                    ),
                )
                .await
            }
            return self.issue_as_made(guest, call, &cause, untraced).await;
        }
        let mut stack = guest.stack().await;
        let set_tid = stack.push(id as libc::pid_t);
        let args = stack.push(ChosenIdCall::with_set_tid(args, set_tid.as_raw() as u64));
        let cause = match stack.commit() {
            Ok(scratch) => {
                let rewritten = syscalls::Clone3::new()
                    .with_args(AddrMut::from_raw(args.as_raw()))
                    .with_size(size);
                let result = guest.inject(rewritten).await;
                drop(scratch);
                match result {
                    Ok(child) => {
                        debug!(
                            "[detcore, dtid {}] {} created task {child} with chosen ID {id}",
                            guest.thread_state().dettid,
                            call.number()
                        );
                        return Ok(child);
                    }
                    // Linux restarts the guest's call whole; it keeps its ID.
                    Err(errno) if is_restart(errno) => {
                        guest.thread_state_mut().restarted_clone_id = Some(RestartedCloneId {
                            id,
                            call: fingerprint,
                        });
                        return Err(errno);
                    }
                    Err(Errno::EEXIST) => {
                        let cause = format!("ID {id} is taken: {}", holder_of(id));
                        if refuse_host_ids {
                            refuse_host_id(
                                guest,
                                format!(
                                    "this {} cannot create its task with its chosen ID: {cause}. \
                                     Pass a --pid-base whose IDs no host task holds",
                                    call.number()
                                ),
                            )
                            .await
                        }
                        cause
                    }
                    Err(Errno::EPERM) if refuse_host_ids && flags & NAMESPACES == 0 => {
                        // Nothing else in a namespace-free clone needs a
                        // capability: the caller may not choose IDs (it gave
                        // up CAP_SYS_ADMIN and CAP_CHECKPOINT_RESTORE).
                        refuse_host_id(
                            guest,
                            format!(
                                "this {} cannot create its task with its chosen ID {id}: the \
                                 guest task may not choose IDs (EPERM; it has given up \
                                 CAP_SYS_ADMIN and CAP_CHECKPOINT_RESTORE)",
                                call.number()
                            ),
                        )
                        .await
                    }
                    // An error the guest's own call gets as well, with no
                    // task created: its answer.
                    Err(errno) if refuse_host_ids && is_guests_own_error(errno, flags) => {
                        return Err(errno);
                    }
                    Err(errno) => format!("clone3 with set_tid {id} failed with {errno}"),
                }
            }
            Err(error) => format!("its scratch memory could not be written: {error}"),
        };
        // A run that needs every ID chosen ends here, before the guest's own
        // call could create a task with a host ID (and, for vfork, run it).
        if refuse_host_ids || untraced {
            refuse_host_id(
                guest,
                format!(
                    "this {} cannot create its task with its chosen ID {id}: {cause}",
                    call.number()
                ),
            )
            .await
        }
        self.issue_as_made(guest, call, &cause, untraced).await
    }

    /// Issue the guest's own call `call`, whose task, if it creates one, has
    /// a host ID for `cause` ([`Self::gave_host_id`]). An untraced call is not
    /// issued: its task would run outside Detcore's scheduler, so even a run
    /// that takes host IDs ends.
    async fn issue_as_made<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        cause: &str,
        untraced: bool,
    ) -> Result<i64, Errno> {
        if untraced {
            refuse_host_id(
                guest,
                format!(
                    "this {} cannot create its task with its chosen ID ({cause}), and the guest's \
                     own call would create a task the tracer does not follow (CLONE_UNTRACED)",
                    call.number()
                ),
            )
            .await
        }
        let original = guest.inject(call).await;
        if let Ok(child) = original {
            self.gave_host_id(guest, child, cause).await;
        }
        original
    }
}

/// The ID a restarted clone-family call took (`ThreadState::restarted_clone_id`),
/// and `call`, a fingerprint of its number and `clone3` form. Linux restarts
/// the same call with the same arguments, so the restart matches it and keeps
/// the ID. The binding is by arguments, not by call instance: a call in
/// between with other arguments (a signal handler's `clone` with its own
/// stack) takes an ID of its own and retires this one; one with the same
/// arguments (a handler's `fork`) takes this ID, and the restart then takes
/// the next. Either way no ID is given twice, and where the handler runs is
/// decided by the scheduler, so the pairing is the same in every run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartedCloneId {
    pub(crate) id: i32,
    pub(crate) call: u64,
}

impl RestartedCloneId {
    /// FNV-1a over the call's number, size and `clone3` arguments.
    fn fingerprint(sysno: Sysno, args: &[u8], size: usize) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        let number = sysno as u64;
        for byte in number
            .to_ne_bytes()
            .iter()
            .chain((size as u64).to_ne_bytes().iter())
            .chain(args)
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

/// Why a clone-family call with `flags`, issued by `guest`, creates its task
/// in another PID namespace than Hermit's, where `set_tid` names the ID inside
/// it, if it does: with `CLONE_NEWPID`, or under a PID namespace the guest
/// made for its children. Without PID namespaces (no such links), there is
/// only the one.
fn nested_pid_namespace(tid: i32, flags: u64) -> Option<String> {
    if flags & libc::CLONE_NEWPID as u64 != 0 {
        return Some("it creates a PID namespace, where set_tid names an ID inside".to_owned());
    }
    let namespace = |path: &str| std::fs::read_link(path).ok();
    if let (Some(own), Some(children)) = (
        namespace("/proc/self/ns/pid"),
        namespace(&format!("/proc/{tid}/ns/pid_for_children")),
    ) && own != children
    {
        return Some(
            "its task is created in a PID namespace the guest made, where set_tid names an ID \
             inside"
                .to_owned(),
        );
    }
    None
}

/// The host's pid_max, read once: an ID at or above it is not one `set_tid`
/// can give.
fn pid_max() -> Option<i32> {
    static PID_MAX: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    *PID_MAX.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()?
            .trim()
            .parse()
            .ok()
    })
}

/// Whether `errno`, from the rewritten `clone3` of a call with `flags`, is
/// one the guest's own call gets as well, with no task created, rather than
/// one of `set_tid`'s own:
///
/// - the resource errors (`copy_process` running out of tasks, memory or
///   IDs: `EAGAIN`, `ENOMEM`, `ENOSPC`);
/// - `EINVAL`: once the chosen ID is below pid_max (checked before the call),
///   `set_tid` of one entry in Hermit's namespace, which has an init, cannot
///   cause it, and the rewrite keeps every check of `copy_process` and
///   `clone3_args_valid` the original call meets. One remains: a legacy
///   `clone` whose new stack pointer is not a user address passes `clone3`
///   a stack `clone3_stack_valid` refuses, where the guest's own call would
///   create a task that faults at once;
/// - `EINTR` (a fatal signal is pending: the task is about to die);
/// - `EPERM` beside a namespace flag, which needs a capability of its own
///   (`copy_namespaces`).
fn is_guests_own_error(errno: Errno, flags: u64) -> bool {
    [
        Errno::EAGAIN,
        Errno::ENOMEM,
        Errno::ENOSPC,
        Errno::EINVAL,
        Errno::EINTR,
    ]
    .contains(&errno)
        || (errno == Errno::EPERM && flags & NAMESPACES != 0)
}

/// The kernel's limit on PID namespace nesting (`MAX_PID_NS_LEVEL`), and so
/// on `set_tid_size`.
const MAX_SET_TID: u64 = 32;

/// A guest `clone3`'s own `set_tid` array, given its `struct clone_args`
/// `args`, if the kernel can accept the call as far as Detcore can tell from
/// the caller, thread `tid`: `None` for a form it refuses before creating a
/// task, which then goes to the kernel as made so its error stays the
/// guest's answer (`guest_set_tid_form`, an unreadable array (`EFAULT`), an
/// entry below 1 or a first entry other than 1 in a new PID namespace
/// (`EINVAL`), a caller with neither `CAP_SYS_ADMIN` nor
/// `CAP_CHECKPOINT_RESTORE` (`EPERM`)).
fn guest_set_tid<M: MemoryAccess>(
    memory: &M,
    args: &[u8; CLONE_ARGS_SIZE_VER2],
    tid: i32,
) -> Option<Vec<libc::pid_t>> {
    let (address, size) = guest_set_tid_form(args)?;
    let mut tids = vec![0 as libc::pid_t; size];
    memory
        .read_values(Addr::<libc::pid_t>::from_raw(address as usize)?, &mut tids)
        .ok()?;
    let flags = u64::from_ne_bytes(args[..8].try_into().unwrap());
    if tids.iter().any(|tid| *tid < 1) || (flags & libc::CLONE_NEWPID as u64 != 0 && tids[0] != 1) {
        return None;
    }
    const CAP_SYS_ADMIN: u32 = 21;
    const CAP_CHECKPOINT_RESTORE: u32 = 40;
    let capabilities = std::fs::read_to_string(format!("/proc/{tid}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("CapEff:"))
                .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        });
    let choose = (1u64 << CAP_SYS_ADMIN) | (1u64 << CAP_CHECKPOINT_RESTORE);
    if capabilities.is_some_and(|mask| mask & choose == 0) {
        return None;
    }
    Some(tids)
}

/// The address and length of a guest `clone3`'s own `set_tid` array, from
/// its `struct clone_args` `args`, unless the kernel refuses the call on
/// those bytes alone, before it creates a task, on every kernel that has
/// `set_tid`:
///
/// - `copy_clone_args_from_user`: an exit signal that is not a signal
///   number, `set_tid` without `set_tid_size` or the reverse, more than
///   `MAX_PID_NS_LEVEL` entries;
/// - `clone3_args_valid`: `CLONE_DETACHED`, a `CSIGNAL` bit other than
///   `CLONE_NEWTIME` (which Linux 6.3 allows), `CLONE_SIGHAND` beside
///   `CLONE_CLEAR_SIGHAND`, an exit signal beside `CLONE_THREAD` or
///   `CLONE_PARENT`, a stack without a size or the reverse;
/// - `copy_process`'s flag checks: `CLONE_FS` beside `CLONE_NEWNS` or
///   `CLONE_NEWUSER`, `CLONE_THREAD` without `CLONE_SIGHAND` or beside
///   `CLONE_NEWUSER` or `CLONE_NEWPID`, `CLONE_SIGHAND` without `CLONE_VM`,
///   `CLONE_PIDFD` beside `CLONE_DETACHED`.
///
/// Checks that depend on the caller's state (namespaces, a container init's
/// `CLONE_PARENT`) are left to the kernel.
fn guest_set_tid_form(args: &[u8; CLONE_ARGS_SIZE_VER2]) -> Option<(u64, usize)> {
    let field =
        |index: usize| u64::from_ne_bytes(args[index * 8..index * 8 + 8].try_into().unwrap());
    let (flags, exit_signal, stack, stack_size) = (field(0), field(4), field(5), field(6));
    let (set_tid, set_tid_size) = (field(8), field(9));
    let has = |bits: i32| flags & bits as u64 != 0;
    const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
    let invalid = exit_signal > 64
        || (set_tid == 0) != (set_tid_size == 0)
        || set_tid_size > MAX_SET_TID
        || has(libc::CLONE_DETACHED)
        || flags & (libc::CSIGNAL as u64 & !0x80) != 0
        || (has(libc::CLONE_SIGHAND) && flags & CLONE_CLEAR_SIGHAND != 0)
        || ((has(libc::CLONE_THREAD) || has(libc::CLONE_PARENT)) && exit_signal != 0)
        || (stack == 0) != (stack_size == 0)
        || (has(libc::CLONE_FS) && (has(libc::CLONE_NEWNS) || has(libc::CLONE_NEWUSER)))
        || (has(libc::CLONE_THREAD)
            && (!has(libc::CLONE_SIGHAND) || has(libc::CLONE_NEWUSER) || has(libc::CLONE_NEWPID)))
        || (has(libc::CLONE_SIGHAND) && !has(libc::CLONE_VM))
        || (has(libc::CLONE_PIDFD) && has(libc::CLONE_DETACHED));
    (!invalid && set_tid_size > 0).then_some((set_tid, set_tid_size as usize))
}

/// How many PID namespaces out of the one a clone-family call with `flags`
/// by thread `tid` creates its task in Hermit's own is: 0 for Hermit's, one
/// more with `CLONE_NEWPID`, and the depth of a namespace the guest made for
/// its children (`pid_for_children`), walked up with `NS_GET_PARENT`. `None`
/// if it cannot be read, or the walk does not reach Hermit's within the
/// kernel's nesting limit.
fn pid_namespace_depth(tid: i32, flags: u64) -> Option<usize> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;
    // ioctl_ns(2): _IO(0xb7, 0x2).
    const NS_GET_PARENT: u64 = 0xb702;
    let own = std::fs::metadata("/proc/self/ns/pid").ok()?;
    let mut namespace = std::fs::File::open(format!("/proc/{tid}/ns/pid_for_children")).ok()?;
    let new = usize::from(flags & libc::CLONE_NEWPID as u64 != 0);
    for depth in 0..=MAX_SET_TID as usize {
        let here = namespace.metadata().ok()?;
        if (here.dev(), here.ino()) == (own.dev(), own.ino()) {
            return Some(depth + new);
        }
        // SAFETY: NS_GET_PARENT takes no argument and returns a new file
        // descriptor, owned from here on, or -1.
        let parent = unsafe { libc::ioctl(namespace.as_raw_fd(), NS_GET_PARENT as _) };
        if parent < 0 {
            return None;
        }
        // SAFETY: `parent` is a file descriptor nothing else owns.
        namespace = unsafe { std::fs::File::from_raw_fd(parent) };
    }
    None
}

/// Why a guest call with `flags` creates a task the tracer does not follow
/// (`CLONE_UNTRACED`), if it does: Detcore would see none of that task's own
/// clone calls, and the task would run outside its scheduler. A call Detcore
/// rewrites drops the flag; a guest `clone3` with its own `set_tid` is not
/// rewritten, so it is refused.
fn untraced_clone(flags: u64) -> Option<String> {
    (flags & libc::CLONE_UNTRACED as u64 != 0).then(|| {
        "it creates a task the tracer does not follow (CLONE_UNTRACED), which would run outside \
         Detcore's scheduler"
            .to_owned()
    })
}

/// The namespace flags of `clone`, any of which may need a capability.
const NAMESPACES: u64 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET) as u64
    // CLONE_NEWTIME (0x80), which libc does not name, needs CAP_SYS_ADMIN
    // too (copy_namespaces). Only clone3 can pass it, and only from Linux
    // 6.3: before, clone3_args_valid refuses it with EINVAL, which the
    // guest's own clone3 gets as well.
    | 0x80;

/// Whether `errno` is one of the kernel's restart codes, which a call returns
/// only to be restarted.
fn is_restart(errno: Errno) -> bool {
    [
        Errno::ERESTARTSYS,
        Errno::ERESTARTNOINTR,
        Errno::ERESTARTNOHAND,
        Errno::ERESTART_RESTARTBLOCK,
    ]
    .contains(&errno)
}

/// A `clone3` call's `struct clone_args`, all the bytes it passes, if they
/// can be read and number at most a page, the kernel's limit (E2BIG).
fn read_clone_args<M: MemoryAccess>(memory: &M, clone: &syscalls::Clone3) -> Option<Vec<u8>> {
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    if clone.size() > page {
        return None;
    }
    let mut bytes = vec![0u8; clone.size()];
    memory
        .read_exact(Addr::<u8>::from_raw(clone.args()?.as_raw())?, &mut bytes)
        .ok()?;
    Some(bytes)
}

/// End a run that needs every ID chosen (`GuestIds::Chosen::refuse_host_ids`:
/// `--strict` or an explicit `--pid-base`) because a task would have, or has,
/// a host ID: `reason` says why. The run ends with Hermit's policy-refusal
/// status.
async fn refuse_host_id<T, G>(guest: &mut G, reason: String) -> !
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let reason = format!(
        "this run needs every guest process ID chosen (--strict or --pid-base), and a task would \
         have, or has, a host ID, which differs from run to run: {reason}"
    );
    tracing::error!("[detcore, dtid {}] {reason}", guest.thread_state().dettid);
    crate::tool_global::report_policy_refusal(guest, reason).await;
    crate::tool_global::unrecoverable_shutdown(guest, detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
        .await
}

/// What holds `id` on the host, for a refusal: the task's name and its
/// process, or that only a process group or session holds it.
pub fn holder_of(id: i32) -> String {
    let status = std::fs::read_to_string(format!("/proc/{id}/status")).ok();
    let field = |name: &str| {
        status.as_deref().and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|value| value.trim().to_owned())
        })
    };
    match (field("Name:"), field("Tgid:")) {
        (Some(name), Some(tgid)) if tgid == id.to_string() => {
            format!("process {id} ({name})")
        }
        (Some(name), Some(tgid)) => format!("thread {id} ({name}) of process {tgid}"),
        _ => format!(
            "no live task: a process group or session of an exited process keeps ID {id}, or \
             its holder has exited since"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(call: &ChosenIdCall) -> ([u64; 11], usize) {
        match call {
            ChosenIdCall::Clone3 { args, size } => {
                let mut fields = [0u64; 11];
                for (field, chunk) in fields.iter_mut().zip(args.as_chunks::<8>().0) {
                    *field = u64::from_ne_bytes(*chunk);
                }
                (fields, *size)
            }
            other => panic!("expected a clone3 form, got {other:?}"),
        }
    }

    fn clone_args(flags: u64, sp: u64, ptid: u64, ctid: u64, tls: u64) -> SyscallArgs {
        #[cfg(target_arch = "aarch64")]
        let (arg3, arg4) = (tls, ctid);
        #[cfg(not(target_arch = "aarch64"))]
        let (arg3, arg4) = (ctid, tls);
        SyscallArgs {
            arg0: flags as usize,
            arg1: sp as usize,
            arg2: ptid as usize,
            arg3: arg3 as usize,
            arg4: arg4 as usize,
            arg5: 0,
        }
    }

    const THREAD_FLAGS: u64 = (libc::CLONE_VM
        | libc::CLONE_FS
        | libc::CLONE_FILES
        | libc::CLONE_SIGHAND
        | libc::CLONE_THREAD
        | libc::CLONE_SYSVSEM
        | libc::CLONE_SETTLS
        | libc::CLONE_PARENT_SETTID
        | libc::CLONE_CHILD_CLEARTID) as u64;

    /// A C library's thread creation keeps every field, in clone3's order,
    /// with the new stack pointer as the top of a one-byte stack.
    #[test]
    fn a_thread_creation_keeps_every_field() {
        let call = clone3_form(
            Sysno::clone,
            &clone_args(THREAD_FLAGS, 0x7000_1000, 0x5000, 0x5000, 0x6000),
            None,
        );
        assert_eq!(
            fields(&call),
            (
                [
                    THREAD_FLAGS,
                    0x5000,
                    0x5000,
                    0x5000,
                    0,
                    0x7000_0fff,
                    1,
                    0x6000,
                    0,
                    0,
                    0
                ],
                CLONE_ARGS_SIZE_VER1
            )
        );
    }

    /// The exit signal leaves the flags; beside CLONE_THREAD or CLONE_PARENT
    /// it becomes 0, which copy_process ignores anyway.
    #[test]
    fn the_exit_signal_moves_out_of_the_flags() {
        let sigchld = libc::SIGCHLD as u64;
        let (process, _) = fields(&clone3_form(
            Sysno::clone,
            &clone_args(sigchld, 0, 0, 0, 0),
            None,
        ));
        assert_eq!(
            (process[0], process[4], process[5], process[6]),
            (0, sigchld, 0, 0)
        );
        for shared in [
            libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM,
            libc::CLONE_PARENT,
        ] {
            let (fields, _) = fields(&clone3_form(
                Sysno::clone,
                &clone_args(shared as u64 | sigchld, 0, 0, 0, 0),
                None,
            ));
            assert_eq!((fields[0], fields[4]), (shared as u64, 0));
        }
    }

    /// Bits above the low 32 never reach the kernel from clone; neither does
    /// CLONE_DETACHED, which clone3 refuses.
    #[test]
    fn high_bits_and_clone_detached_are_dropped() {
        let flags = (1u64 << 40) | libc::CLONE_DETACHED as u64 | libc::CLONE_FILES as u64;
        let (fields, _) = fields(&clone3_form(
            Sysno::clone,
            &clone_args(flags, 0, 0, 0, 0),
            None,
        ));
        assert_eq!(fields[0], libc::CLONE_FILES as u64);
    }

    /// A call every kernel refuses before creating a task goes unchanged;
    /// CLONE_PIDFD beside CLONE_THREAD, which only kernels before 6.9 refuse,
    /// is rewritten, and its clone3 fails as the call would. An exit signal
    /// clone3 cannot carry is inexpressible, except beside CLONE_THREAD or
    /// CLONE_PARENT, where copy_process ignores it.
    #[test]
    fn refused_and_inexpressible_calls() {
        let detached = (libc::CLONE_PIDFD | libc::CLONE_DETACHED) as u64;
        assert_eq!(
            clone3_form(Sysno::clone, &clone_args(detached, 0, 0, 0, 0), None),
            ChosenIdCall::Unchanged
        );
        let pidfd_thread =
            (libc::CLONE_PIDFD | libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as u64;
        let (pidfd_fields, _) = fields(&clone3_form(
            Sysno::clone,
            &clone_args(pidfd_thread, 0, 0, 0, 0),
            None,
        ));
        assert_eq!(pidfd_fields[0], pidfd_thread);
        assert!(matches!(
            clone3_form(Sysno::clone, &clone_args(0x80, 0, 0, 0, 0), None),
            ChosenIdCall::Inexpressible(_)
        ));
        let thread = (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as u64;
        let (thread_fields, _) = fields(&clone3_form(
            Sysno::clone,
            &clone_args(thread | 0x80, 0, 0, 0, 0),
            None,
        ));
        assert_eq!((thread_fields[0], thread_fields[4]), (thread, 0));
    }

    /// clone's last two arguments are the child TID address and the TLS on
    /// x86-64, and the other way round on arm64 (`CONFIG_CLONE_BACKWARDS`):
    /// each order puts each in its clone3 field, whichever architecture runs
    /// the test.
    #[test]
    fn both_clone_argument_orders() {
        // clone(flags, newsp, parent_tidptr, arg3, arg4).
        let args = SyscallArgs::new(THREAD_FLAGS as usize, 0, 0x5000, 0x3333, 0x4444, 0);
        for (order, child_tid, tls) in [
            (CloneArgOrder::ChildTidThenTls, 0x3333, 0x4444),
            (CloneArgOrder::TlsThenChildTid, 0x4444, 0x3333),
        ] {
            let (fields, _) = fields(&LegacyClone::of_clone(&args, order).as_clone3());
            // clone_args: flags, pidfd, child_tid, parent_tid, exit_signal,
            // stack, stack_size, tls.
            assert_eq!((fields[2], fields[3], fields[7]), (child_tid, 0x5000, tls));
        }
        #[cfg(target_arch = "aarch64")]
        assert_eq!(CloneArgOrder::NATIVE, CloneArgOrder::TlsThenChildTid);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(CloneArgOrder::NATIVE, CloneArgOrder::ChildTidThenTls);
    }

    /// fork and vfork are the kernel's fixed argument sets.
    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn fork_and_vfork() {
        let none = clone_args(0, 0, 0, 0, 0);
        let (fork, _) = fields(&clone3_form(Sysno::fork, &none, None));
        assert_eq!((fork[0], fork[4]), (0, libc::SIGCHLD as u64));
        let (vfork, _) = fields(&clone3_form(Sysno::vfork, &none, None));
        assert_eq!(
            (vfork[0], vfork[4]),
            (
                (libc::CLONE_VFORK | libc::CLONE_VM) as u64,
                libc::SIGCHLD as u64
            )
        );
    }

    /// A guest's clone3 keeps its bytes and grows to the size that carries
    /// set_tid. One that sets set_tid itself (also within a short struct,
    /// whose bytes from 64 are set_tid's) is the guest's choice. One the
    /// kernel refuses for its size or bytes (under 64; over 88 with a nonzero
    /// byte past them; unreadable) goes unchanged, and a longer one whose
    /// extra bytes are zero is its first 88.
    #[test]
    fn a_guest_clone3() {
        let none = SyscallArgs::new(0, 0, 0, 0, 0, 0);
        let mut ver0 = [0u8; CLONE_ARGS_SIZE_VER0];
        ver0[..8].copy_from_slice(&(libc::CLONE_FILES as u64).to_ne_bytes());
        let (ver0_fields, size) = fields(&clone3_form(Sysno::clone3, &none, Some(&ver0)));
        assert_eq!(
            (ver0_fields[0], size),
            (libc::CLONE_FILES as u64, CLONE_ARGS_SIZE_VER1)
        );

        let mut own = [0u8; CLONE_ARGS_SIZE_VER2];
        own[SET_TID_OFFSET..SET_TID_OFFSET + 8].copy_from_slice(&0x1000u64.to_ne_bytes());
        own[SET_TID_SIZE_OFFSET..SET_TID_SIZE_OFFSET + 8].copy_from_slice(&1u64.to_ne_bytes());
        assert_eq!(
            clone3_form(Sysno::clone3, &none, Some(&own)),
            ChosenIdCall::GuestChosen { args: own }
        );
        let mut short = [0u8; 70];
        short[SET_TID_OFFSET] = 1;
        assert!(matches!(
            clone3_form(Sysno::clone3, &none, Some(&short)),
            ChosenIdCall::GuestChosen { .. }
        ));
        for refused in [&[0u8; 32][..], &[[0u8; 96], [1u8; 96]].concat()[..]] {
            assert_eq!(
                clone3_form(Sysno::clone3, &none, Some(refused)),
                ChosenIdCall::Unchanged
            );
        }
        assert_eq!(
            clone3_form(Sysno::clone3, &none, None),
            ChosenIdCall::Unchanged
        );
        let mut long = [0u8; 96];
        long[..8].copy_from_slice(&(libc::CLONE_FILES as u64).to_ne_bytes());
        let (long_fields, size) = fields(&clone3_form(Sysno::clone3, &none, Some(&long)));
        assert_eq!(
            (long_fields[0], size),
            (libc::CLONE_FILES as u64, CLONE_ARGS_SIZE_VER2)
        );
    }

    #[test]
    fn set_tid_is_filled_in() {
        let args = ChosenIdCall::with_set_tid([0u8; CLONE_ARGS_SIZE_VER2], 0x1234);
        assert_eq!(
            u64::from_ne_bytes(args[SET_TID_OFFSET..SET_TID_OFFSET + 8].try_into().unwrap()),
            0x1234
        );
        assert_eq!(
            u64::from_ne_bytes(
                args[SET_TID_SIZE_OFFSET..SET_TID_SIZE_OFFSET + 8]
                    .try_into()
                    .unwrap()
            ),
            1
        );
    }

    /// A call creating a task the tracer does not follow is named as such
    /// (`untraced_clone`): Detcore drops the flag where it rewrites the call,
    /// and refuses a guest `clone3` with its own `set_tid` that carries it.
    #[test]
    fn an_untraced_clone_is_not_given_a_chosen_id() {
        let thread = (libc::CLONE_VM | libc::CLONE_THREAD | libc::CLONE_SIGHAND) as u64;
        assert_eq!(super::untraced_clone(thread), None);
        let untraced = super::untraced_clone(thread | libc::CLONE_UNTRACED as u64);
        assert!(
            untraced.is_some_and(|why| why.contains("CLONE_UNTRACED")),
            "an untraced clone must be named"
        );
    }

    /// A guest `clone3` whose own `set_tid` the kernel refuses on its bytes
    /// alone is told apart from one it can accept, so its error stays the
    /// guest's: a valid array of one or two entries; an array without a size
    /// or the reverse, or too long; and flags, exit signal or stack shapes
    /// `clone3` or `copy_process` refuse.
    #[test]
    fn a_guest_set_tid_the_kernel_refuses_goes_to_the_kernel() {
        let args = |fields: &[(usize, u64)]| {
            let mut args = [0u8; CLONE_ARGS_SIZE_VER2];
            for (index, value) in fields {
                args[index * 8..index * 8 + 8].copy_from_slice(&value.to_ne_bytes());
            }
            args
        };
        let newpid = libc::CLONE_NEWPID as u64;
        let sigchld = libc::SIGCHLD as u64;
        let form = |fields: &[(usize, u64)]| super::guest_set_tid_form(&args(fields));
        assert_eq!(
            form(&[(0, newpid), (4, sigchld), (8, 0x1000), (9, 1)]),
            Some((0x1000, 1))
        );
        assert_eq!(
            form(&[(0, newpid), (4, sigchld), (8, 0x1000), (9, 2)]),
            Some((0x1000, 2))
        );
        for refused in [
            vec![(4, sigchld), (8, 0x1000)],
            vec![(4, sigchld), (9, 1)],
            vec![(4, sigchld), (8, 0x1000), (9, 33)],
            vec![(4, 65), (8, 0x1000), (9, 1)],
            vec![
                (0, libc::CLONE_SIGHAND as u64),
                (4, sigchld),
                (8, 0x1000),
                (9, 1),
            ],
            vec![(0, libc::CLONE_DETACHED as u64), (8, 0x1000), (9, 1)],
            vec![(0, libc::SIGCHLD as u64), (8, 0x1000), (9, 1)],
            vec![(4, sigchld), (5, 0x2000), (8, 0x1000), (9, 1)],
            vec![(4, sigchld), (6, 64), (8, 0x1000), (9, 1)],
            vec![
                (
                    0,
                    (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as u64 | newpid,
                ),
                (8, 0x1000),
                (9, 1),
            ],
        ] {
            assert_eq!(form(&refused), None, "{refused:x?}");
        }
        // CLONE_NEWTIME (0x80) in CSIGNAL is left to the kernel: 6.3 allows it.
        assert_eq!(
            form(&[(0, 0x80), (4, sigchld), (8, 0x1000), (9, 1)]),
            Some((0x1000, 1))
        );
    }

    /// The depth of the PID namespace a call creates its task in, out of
    /// Hermit's: this test's own namespace is Hermit's here, so 0, and one
    /// more with CLONE_NEWPID.
    #[test]
    fn a_pid_namespace_depth_counts_from_hermits() {
        let own = unsafe { libc::gettid() };
        assert_eq!(super::pid_namespace_depth(own, 0), Some(0));
        assert_eq!(
            super::pid_namespace_depth(own, libc::CLONE_NEWPID as u64),
            Some(1)
        );
    }

    /// A restarted call keeps its ID only for itself: the same number and
    /// arguments fingerprint alike, and any other call, such as a signal
    /// handler's fork between the interruption and the restart, differently.
    #[test]
    fn a_restarted_clone_keeps_its_id_only_for_the_same_call() {
        use super::RestartedCloneId;
        let args = [7u8; super::CLONE_ARGS_SIZE_VER2];
        let mut other = args;
        other[0] ^= 1;
        let call = RestartedCloneId::fingerprint(Sysno::clone, &args, 64);
        assert_eq!(call, RestartedCloneId::fingerprint(Sysno::clone, &args, 64));
        assert_ne!(
            call,
            RestartedCloneId::fingerprint(Sysno::clone, &other, 64)
        );
        assert_ne!(call, RestartedCloneId::fingerprint(Sysno::fork, &args, 64));
        assert_ne!(call, RestartedCloneId::fingerprint(Sysno::clone, &args, 88));
    }

    /// A guest's own set_tid, or Detcore's, names an ID in Hermit's PID
    /// namespace only when the task is created there.
    #[test]
    fn a_new_pid_namespace_is_one_set_tid_cannot_number_for_hermit() {
        let own = unsafe { libc::gettid() };
        assert!(
            super::nested_pid_namespace(own, libc::CLONE_NEWPID as u64)
                .is_some_and(|why| why.contains("creates a PID namespace"))
        );
        assert_eq!(
            super::nested_pid_namespace(own, libc::CLONE_VM as u64),
            None
        );
    }

    /// EPERM is set_tid's own only where nothing else in the call needs a
    /// capability: every namespace flag, CLONE_NEWTIME included, does. The
    /// errors the guest's own call gets as well (resources, EINVAL, EINTR,
    /// EPERM beside a namespace flag) are its answer; EEXIST and a
    /// namespace-free EPERM are set_tid's.
    #[test]
    fn eperm_and_resource_errors_are_attributed_to_their_cause() {
        assert_ne!(super::NAMESPACES & 0x80, 0, "CLONE_NEWTIME");
        assert_ne!(super::NAMESPACES & libc::CLONE_NEWUSER as u64, 0);
        let fork = libc::SIGCHLD as u64;
        let newpid = libc::CLONE_NEWPID as u64;
        for errno in [
            Errno::EAGAIN,
            Errno::ENOMEM,
            Errno::ENOSPC,
            Errno::EINVAL,
            Errno::EINTR,
        ] {
            assert!(super::is_guests_own_error(errno, fork), "{errno}");
        }
        assert!(super::is_guests_own_error(Errno::EPERM, newpid));
        for errno in [Errno::EEXIST, Errno::EPERM] {
            assert!(!super::is_guests_own_error(errno, fork), "{errno}");
        }
    }
}
