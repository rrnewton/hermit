/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit-owned constructor for the in-guest Detcore LiteInst runtime.

#![deny(missing_docs)]

// TODO-HUMAN-REVIEW(PR-1429): Trigger 2 - this constructor
// moves the Detcore Tool from the ptrace host into the instrumented guest.
/// Installs one process-local `Detcore` Tool when the LiteInst coordinator is active.
///
/// The generic `reverie-liteinst` constructor remains responsible for host-mode
/// compatibility runs. The coordinator selector is present only for the native
/// in-guest path, so the two constructors are mutually exclusive.
#[used]
#[unsafe(link_section = ".init_array")]
static DETCORE_LITEINST_INIT: unsafe extern "C" fn() = initialize;

unsafe extern "C" fn initialize() {
    let Some(socket) = std::env::var_os(reverie_liteinst::COORDINATOR_ENV) else {
        return;
    };

    // Keep the selector available to fork children. Exec remains fail-closed
    // until a lifecycle supervisor can rebootstrap the replacement image.
    if let Err(error) = unsafe {
        // This runtime currently rejects application-created threads, and
        // Hermit schedules at most one guest thread at a time. Preserve that
        // invariant when thread lifecycle support is added: otherwise this
        // constructor must select the concurrent publication entry point.
        reverie_liteinst::install_tool_quiescent::<detcore::Detcore>(socket)
    } {
        eprintln!("hermit-liteinst-runtime: initialization failed: {error}");
        unsafe { libc::_exit(127) };
    }
}
