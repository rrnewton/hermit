/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! e9patch preload DSO that executes Hermit's Detcore tool inside each guest
//! process with no per-syscall ptrace round trip.
//!
//! The [`E9patchBackend::run_direct_with_preload`] launcher rewrites the guest
//! ELF with e9patch, injects this library through `LD_PRELOAD`, hands the
//! coordinator socket path through [`reverie_e9patch::COORDINATOR_ENV`], and
//! reaps the guest process tree with a lifecycle-only `TracerBuilder<()>`. The
//! `.init_array` constructor below reads that socket and installs the concrete
//! [`Detcore`] tool via [`reverie_e9patch::install_tool`], which publishes the
//! AOT callback, installs the in-process seccomp controller, and connects the
//! in-guest tool host to this process's coordinator. Every syscall is then
//! serviced in-guest: subscribed sites through the rewritten AOT callback and
//! un-instrumented sites fail closed through the shared `SIGSYS` handler — never
//! a ptrace trap.
//!
//! [`E9patchBackend::run_direct_with_preload`]: reverie_e9patch::E9patchBackend::run_direct_with_preload

use std::path::PathBuf;

use detcore::Detcore;

/// Installs the in-guest Detcore tool host if this process was launched by the
/// direct-tool e9patch backend.
///
/// The function is inert unless [`reverie_e9patch::COORDINATOR_ENV`] is present,
/// so linking the DSO into a process that was not launched through the
/// direct-tool backend does nothing. When the coordinator socket is present,
/// installation failure is fatal and fails closed: a guest whose determinism
/// tool could not be installed must not run natively.
///
/// # Safety
///
/// Invoked from `.init_array` before application-created threads start. It
/// installs process-global signal, seccomp, dispatcher, and AOT callback state
/// exactly once, matching the contract of [`reverie_e9patch::install_tool`].
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1638): Review the in-guest Detcore tool-host
// installation and fail-closed contract for the ptrace-free e9patch backend.
pub unsafe extern "C" fn detcore_e9patch_initialize() {
    let coordinator = match std::env::var_os(reverie_e9patch::COORDINATOR_ENV) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        // Not launched by the direct-tool backend: leave the guest unmodified.
        _ => return,
    };

    // SAFETY: the `.init_array` contract runs this before any guest thread is
    // created, satisfying `install_tool`'s once-before-threads requirement.
    if let Err(error) = unsafe { reverie_e9patch::install_tool::<Detcore>(&coordinator) } {
        eprintln!("detcore-e9patch: failed to install the in-guest Detcore tool: {error}");
        // Fail closed: a guest that cannot host its determinism tool must not
        // continue executing natively.
        unsafe {
            libc::_exit(127);
        }
    }
}

/// Arms the in-guest Detcore installer from the loader's `.init_array`.
///
/// Mirrors reverie-e9patch's own `preload-constructor` entry, but embeds the
/// concrete [`Detcore`] tool instead of the generic environment-driven runtime.
#[used]
#[unsafe(link_section = ".init_array")]
static DETCORE_E9PATCH_INIT: unsafe extern "C" fn() = detcore_e9patch_initialize;
