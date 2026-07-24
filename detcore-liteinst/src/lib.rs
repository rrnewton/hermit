/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-id): Review the preload constructor boundary.

//! Hermit-owned cdylib wrapper for Reverie's LiteInst preload runtime.

#![deny(missing_docs)]

/// Installs the LiteInst runtime once when this wrapper is preloaded.
///
/// Reverie's constructor feature is disabled so this is the only init-array
/// entry that activates the irreversible seccomp filter.
#[used]
#[unsafe(link_section = ".init_array")]
static DETCORE_LITEINST_INIT: unsafe extern "C" fn() =
    reverie_liteinst::reverie_liteinst_initialize;
