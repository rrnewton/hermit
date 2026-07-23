/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Process/thread identity syscalls (`getpid`, `gettid`, `getppid`) and the
//! PID-targeted signal syscalls (`kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`).
//!
//! # PID virtualization
//!
//! Hermit runs the guest inside its own PID namespace (the container calls
//! `unshare(CLONE_NEWPID)`). The pids the guest observes are therefore already
//! isolated from the host and are assigned deterministically: detcore
//! serializes thread/process creation, so the kernel allocates namespace pids in
//! a reproducible order. Detcore consequently uses those namespace pids directly
//! as its [`DetPid`]/[`DetTid`] — i.e. the "virtual" pid and the "real"
//! (in-namespace) pid are the *same value* today. (Making them diverge is the
//! larger `T78538674` refactor, which would replace the identity translation in
//! [`Self::virtual_to_real_pid`]/[`Self::real_to_virtual_pid`] and every other
//! `DetTid <-> Pid` conversion site.)
//!
//! Before this module existed these syscalls had no handler, so they fell
//! through to detcore's fail-closed default and returned `ENOSYS`. That broke
//! the extremely common `kill(getpid(), sig)` self-signal pattern — `getpid`
//! returned `-ENOSYS`, so the subsequent `kill` targeted a bogus pid and no
//! signal was ever delivered. That is the root cause of the rr
//! `multiple_pending_signals_sequential` hang (whose child only makes progress
//! from inside its signal handler). These handlers make the pid and
//! pid-targeted-signal syscalls first-class, deterministic operations that work
//! under `--strict` without `--allow-passthrough`.
//!
//! Signal *delivery* remains deterministic through detcore's existing inbound
//! signal path: a signal raised by an injected `kill`/`tgkill` is caught at the
//! guest's signal-delivery stop and scheduled via `handle_signal_event` /
//! `ResourceID::InboundSignal`, exactly like any other asynchronous signal.
//!
//! [`DetPid`]: crate::types::DetPid
//! [`DetTid`]: crate::types::DetTid

use nix::sys::signal::Signal;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_global::send_signal;
use crate::types::DetTid;

impl<T: RecordOrReplay> Detcore<T> {
    /// Translate a guest-visible ("virtual") pid to the real in-namespace pid
    /// used when injecting a syscall into the kernel.
    ///
    /// Identity today (see the module docs); centralized so a future
    /// non-identity virtual-pid scheme only has to change this one place.
    fn virtual_to_real_pid(pid: libc::pid_t) -> libc::pid_t {
        pid
    }

    /// getpid(2): the calling process's (thread-group) pid.
    ///
    /// Deterministic namespace pid; injected so the kernel returns the same
    /// value the guest would see natively and would pass back to `kill`.
    pub async fn handle_getpid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getpid,
    ) -> Result<i64, Error> {
        Ok(guest.inject(call).await?)
    }

    /// gettid(2): the calling thread's id.
    pub async fn handle_gettid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Gettid,
    ) -> Result<i64, Error> {
        Ok(guest.inject(call).await?)
    }

    /// getppid(2): the parent process's pid (0 for the namespace root, whose
    /// parent lives outside the namespace).
    pub async fn handle_getppid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getppid,
    ) -> Result<i64, Error> {
        Ok(guest.inject(call).await?)
    }

    /// kill(2): send a signal to a process (or process group).
    ///
    /// For an ordinary process-directed signal (`pid > 0`, `sig != 0`) the
    /// delivery is routed through the global scheduler so that a target thread
    /// parked in the scheduler (e.g. blocked on a pipe read while another thread
    /// runs) is woken to receive it. This is what makes the common
    /// `kill(getpid(), sig)` self-signal pattern work under sequentialized
    /// execution instead of deadlocking. Process-group (`pid <= 0`) and
    /// existence-probe (`sig == 0`) forms have no single scheduler target, so
    /// they fall back to translated raw injection.
    pub async fn handle_kill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Kill,
    ) -> Result<i64, Error> {
        let pid = Self::virtual_to_real_pid(call.pid());
        let sig = call.sig();
        if pid > 0 && sig != 0 {
            if let Ok(signal) = Signal::try_from(sig) {
                send_signal(guest, DetTid::from_raw(pid), signal).await?;
                return Ok(0);
            }
        }
        let call = call.with_pid(pid);
        Ok(guest.inject(call).await?)
    }

    /// tkill(2): send a signal to a single thread.
    pub async fn handle_tkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tkill,
    ) -> Result<i64, Error> {
        let call = call.with_tid(Self::virtual_to_real_pid(call.tid()));
        Ok(guest.inject(call).await?)
    }

    /// tgkill(2): send a signal to a specific thread within a thread group.
    pub async fn handle_tgkill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Tgkill,
    ) -> Result<i64, Error> {
        let call = call
            .with_tgid(Self::virtual_to_real_pid(call.tgid()))
            .with_tid(Self::virtual_to_real_pid(call.tid()));
        Ok(guest.inject(call).await?)
    }

    /// rt_sigqueueinfo(2): send a signal plus an accompanying `siginfo_t` to a
    /// process. Only the destination thread-group pid needs translation; the
    /// `siginfo` payload is left untouched (its `si_pid` is filled in by the
    /// kernel relative to the sender).
    pub async fn handle_rt_sigqueueinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::RtSigqueueinfo,
    ) -> Result<i64, Error> {
        let call = call.with_tgid(Self::virtual_to_real_pid(call.tgid()));
        Ok(guest.inject(call).await?)
    }
}
