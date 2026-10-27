/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Treat all Clippy warnings as errors.
#![deny(clippy::all)]
#![allow(clippy::uninlined_format_args)]
#![allow(
    unexpected_cfgs,
    reason = "`fbcode_build` is supplied by the internal Buck build"
)]

// The preinit section belongs to executable images, never to the shared rlib:
// hermit-cdylib also links the library and must not acquire a preinit array.
#[used]
#[unsafe(link_section = ".preinit_array")]
static CAPTURE_STARTUP_STDIN: unsafe extern "C" fn() = hermit::app::capture_startup_stdin;

#[fbinit::main]
fn main() {
    hermit::app::main();
}
