/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Resolve the Hermit executable selected for this test process.
///
/// Validation exports `HERMIT_BIN` only after verifying the content-addressed
/// artifact. Ordinary `cargo test` invocations retain Cargo's compile-time
/// binary path as their fallback.
pub fn hermit_binary() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("HERMIT_BIN")
            .filter(|path| !path.is_empty())
            .or_else(|| option_env!("CARGO_BIN_EXE_hermit").map(OsString::from))
            .map(PathBuf::from)
            .expect("HERMIT_BIN and CARGO_BIN_EXE_hermit are both unavailable")
    })
    .as_path()
}
