/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A selected prototype Tool for testing control progress during a real RPC.
//!
//! This single-application-thread probe completes two tagged operations with
//! their exact coordinator replies. It is not a Detcore syscall policy.

use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

/// Raw argument tag used by the `runtime_capture_control` fixture.
pub const NATIVE_CONTROL_PROBE_TAG: u64 = 0x4350_5250;

/// The marker reaching the coordinator through the normal external RPC path.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NativeControlProbeRequest {
    /// One tagged operation, with the actual Tool ownership state at entry.
    Marker {
        /// First or second marker from the real guest.
        ordinal: u64,
        /// Live Tool callback depth, which must remain one across the RPC.
        callback_depth: u32,
        /// Marker callbacks completed before this one began.
        completed_markers: u64,
    },
}

/// Wire-compatible local placeholder; this probe requires an external server.
#[derive(Debug, Default)]
pub struct NativeControlProbeGlobal;

#[reverie::global_tool]
impl GlobalTool for NativeControlProbeGlobal {
    type Request = NativeControlProbeRequest;
    type Response = i64;
    type Config = ();

    async fn receive_rpc(&self, _from: Tid, _request: NativeControlProbeRequest) -> i64 {
        // A missing external coordinator cannot produce the expected reply.
        -(libc::ENOTCONN as i64)
    }
}

static CALLBACK_DEPTH: AtomicU32 = AtomicU32::new(0);
static COMPLETED_MARKERS: AtomicU64 = AtomicU64::new(0);

struct CallbackLease;

impl CallbackLease {
    fn enter() -> Self {
        assert_eq!(
            CALLBACK_DEPTH.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire),
            Ok(0),
            "native control must not re-enter the live Tool callback"
        );
        Self
    }
}

impl Drop for CallbackLease {
    fn drop(&mut self) {
        assert_eq!(CALLBACK_DEPTH.swap(0, Ordering::AcqRel), 1);
    }
}

/// An explicit prototype fixture Tool with a completed-original RPC tail.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeControlProbeTool;

#[reverie::tool]
impl Tool for NativeControlProbeTool {
    type GlobalState = NativeControlProbeGlobal;
    type ThreadState = ();

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        let _lease = CallbackLease::enter();
        if syscall.number() == Sysno::getuid {
            // The typed getuid syscall has no arguments. Read the original
            // registers so its otherwise-ignored fixture tag is not discarded.
            let registers = guest.regs().await;
            if registers.rdi == NATIVE_CONTROL_PROBE_TAG {
                let ordinal = registers.rsi;
                let completed = COMPLETED_MARKERS.load(Ordering::Acquire);
                assert!((1..=2).contains(&ordinal), "unexpected probe marker");
                assert_eq!(ordinal, completed + 1, "marker order changed");
                let response = guest
                    .send_rpc(NativeControlProbeRequest::Marker {
                        ordinal,
                        callback_depth: CALLBACK_DEPTH.load(Ordering::Acquire),
                        completed_markers: completed,
                    })
                    .await;
                assert_eq!(CALLBACK_DEPTH.load(Ordering::Acquire), 1);
                assert_eq!(COMPLETED_MARKERS.fetch_add(1, Ordering::AcqRel), completed);
                // Returning the literal response selects Suppress/action1.
                // The native owner can then finish the original callback after
                // its Cancel-intent ACK, without executing a deferred syscall.
                return Ok(response);
            }
        }
        guest.tail_inject(syscall).await
    }
}
