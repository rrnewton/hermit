/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Explicit prototype probe for a real owned signal's borrowed RPC lifetime.
//! The three signal checkpoints exercise transport ownership, not Detcore's
//! prehook/InboundSignal/posthook policy. The separate CLI test uses Detcore.

use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Signal;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Errno;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

/// Raw getuid argument tag shared with the owned-signal fixture guest.
pub const OWNED_SIGNAL_PUBLICATION_PROBE_TAG: u64 = 0x4f53_5050;

/// Every request goes through the real external Runtime RPC endpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OwnedSignalPublicationProbeRequest {
    /// Parent's ordinary syscall marker before or after the two signal hooks.
    Marker {
        /// One before publication, two after both callbacks.
        ordinal: u64,
        /// First child's guest-visible, virtualized fork result.
        child_a: i32,
        /// Second child's guest-visible, virtualized fork result.
        child_b: i32,
        /// Parent's guest-visible process identity, distinct from native owner.
        virtual_parent: i32,
        /// Must be exactly one while the real Tool future is borrowed.
        callback_depth: u32,
        /// Authentic signal callbacks that have completed their final RPC.
        completed_signals: u64,
    },
    /// A real child announces itself before either release mechanism can fire.
    ChildReady {
        /// One for the first publisher, two for the second publisher.
        role: u64,
        /// Child's guest-visible getpid result, joined to the parent's fork result.
        virtual_pid: i32,
        /// Child's guest-visible getppid result, joined to the parent marker.
        virtual_parent: i32,
        /// The child is inside one real borrowed Tool callback.
        callback_depth: u32,
        /// No signal callback has completed in this independently held child.
        completed_signals: u64,
    },
    /// One of three consecutive RPCs while the same authentic callback is live.
    Signal {
        /// Callback number, with two controlled publishers in this fixture.
        ordinal: u64,
        /// Consecutive RPC checkpoint within the one callback.
        checkpoint: u64,
        /// Must stay one; the second signal may not recursively enter Tool.
        callback_depth: u32,
        /// Signal callbacks finished before this callback began.
        completed_signals: u64,
        /// Complete authentic Linux siginfo bytes supplied by the real Guest.
        siginfo: Vec<u8>,
    },
}

/// Local fallback deliberately cannot impersonate the external controller.
#[derive(Debug, Default)]
pub struct OwnedSignalPublicationProbeGlobal;

#[reverie::global_tool]
impl GlobalTool for OwnedSignalPublicationProbeGlobal {
    type Request = OwnedSignalPublicationProbeRequest;
    type Response = i64;
    type Config = ();

    async fn receive_rpc(&self, _from: Tid, _request: Self::Request) -> i64 {
        -(libc::ENOTCONN as i64)
    }
}

static CALLBACK_DEPTH: AtomicU32 = AtomicU32::new(0);
static STARTED_SIGNALS: AtomicU64 = AtomicU64::new(0);
static COMPLETED_SIGNALS: AtomicU64 = AtomicU64::new(0);

struct CallbackLease;

impl CallbackLease {
    fn enter() -> Self {
        assert_eq!(
            CALLBACK_DEPTH.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire),
            Ok(0),
            "owned signal control recursively entered a borrowed Tool"
        );
        Self
    }
}

impl Drop for CallbackLease {
    fn drop(&mut self) {
        assert_eq!(CALLBACK_DEPTH.swap(0, Ordering::AcqRel), 1);
    }
}

/// Test-only selection; authentic signals keep their original disposition.
#[derive(Clone, Copy, Debug, Default)]
pub struct OwnedSignalPublicationProbeTool;

#[reverie::tool]
impl Tool for OwnedSignalPublicationProbeTool {
    type GlobalState = OwnedSignalPublicationProbeGlobal;
    type ThreadState = ();

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        if syscall.number() == Sysno::getuid {
            let registers = guest.regs().await;
            if registers.rdi == OWNED_SIGNAL_PUBLICATION_PROBE_TAG {
                let _lease = CallbackLease::enter();
                let ordinal = registers.rsi;
                assert!((1..=4).contains(&ordinal), "unexpected fixture marker");
                let completed = COMPLETED_SIGNALS.load(Ordering::Acquire);
                if ordinal >= 3 {
                    assert_eq!(completed, 0, "child signalled before its release");
                    let role = ordinal - 2;
                    let virtual_pid = i32::try_from(registers.rdx).expect("child virtual PID");
                    let virtual_parent =
                        i32::try_from(registers.r10).expect("child virtual parent PID");
                    assert!(virtual_pid > 0 && virtual_parent > 0 && virtual_pid != virtual_parent);
                    let response = guest
                        .send_rpc(OwnedSignalPublicationProbeRequest::ChildReady {
                            role,
                            virtual_pid,
                            virtual_parent,
                            callback_depth: CALLBACK_DEPTH.load(Ordering::Acquire),
                            completed_signals: completed,
                        })
                        .await;
                    assert_eq!(CALLBACK_DEPTH.load(Ordering::Acquire), 1);
                    return Ok(response);
                }
                assert_eq!(
                    completed,
                    if ordinal == 1 { 0 } else { 2 },
                    "guest progressed before both authentic signal callbacks completed"
                );
                let child_a = i32::try_from(registers.rdx).expect("first child PID");
                let child_b = i32::try_from(registers.r10).expect("second child PID");
                let virtual_parent = i32::try_from(registers.r8).expect("parent virtual PID");
                assert!(child_a > 0 && child_b > 0 && child_a != child_b);
                assert!(
                    virtual_parent > 0 && virtual_parent != child_a && virtual_parent != child_b
                );
                let response = guest
                    .send_rpc(OwnedSignalPublicationProbeRequest::Marker {
                        ordinal,
                        child_a,
                        child_b,
                        virtual_parent,
                        callback_depth: CALLBACK_DEPTH.load(Ordering::Acquire),
                        completed_signals: completed,
                    })
                    .await;
                assert_eq!(CALLBACK_DEPTH.load(Ordering::Acquire), 1);
                return Ok(response);
            }
        }
        guest.tail_inject(syscall).await
    }

    async fn handle_signal_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: Signal,
    ) -> Result<Option<Signal>, Errno> {
        let _lease = CallbackLease::enter();
        assert_eq!(signal, Signal::SIGCHLD, "unexpected fixture signal");
        let info = guest
            .signal_info()
            .expect("authentic owned siginfo missing");
        let completed = COMPLETED_SIGNALS.load(Ordering::Acquire);
        let ordinal = STARTED_SIGNALS.fetch_add(1, Ordering::AcqRel) + 1;
        assert!((1..=2).contains(&ordinal), "duplicate signal callback");
        assert_eq!(ordinal, completed + 1, "signal callbacks overlapped");
        for checkpoint in 1..=3 {
            let reply = guest
                .send_rpc(OwnedSignalPublicationProbeRequest::Signal {
                    ordinal,
                    checkpoint,
                    callback_depth: CALLBACK_DEPTH.load(Ordering::Acquire),
                    completed_signals: completed,
                    siginfo: info.to_vec(),
                })
                .await;
            assert_eq!(reply, 0x535000 + (ordinal * 10 + checkpoint) as i64);
            assert_eq!(CALLBACK_DEPTH.load(Ordering::Acquire), 1);
            assert_eq!(guest.signal_info(), Some(info), "active siginfo changed");
        }
        assert_eq!(COMPLETED_SIGNALS.fetch_add(1, Ordering::AcqRel), completed);
        Ok(Some(signal))
    }
}
