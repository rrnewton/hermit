/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::sync::OnceLock;

static LITEINST_RUNTIME: OnceLock<()> = OnceLock::new();

pub(super) fn hermit_binary() -> PathBuf {
    std::env::var_os("HERMIT_LITEINST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hermit")))
}

pub(super) fn liteinst_runtime_library() -> PathBuf {
    hermit_binary()
        .parent()
        .expect("Hermit test binary should have a profile directory")
        .join(hermit::liteinst_artifact::RUNTIME_NAME)
}

pub(super) fn ensure_liteinst_runtime() {
    LITEINST_RUNTIME.get_or_init(|| {
        let runtime = liteinst_runtime_library();
        hermit::validate_liteinst_detcore_runtime_library(&runtime).unwrap_or_else(|error| {
            panic!(
                "LiteInst tests require a runtime staged before the Hermit caller is built; \
                 stage the runtime with HERMIT_LITEINST_SOURCE_RECORD, then rebuild this test \
                 with that same record and both source roots: {}: {error}",
                runtime.display()
            )
        });
    });
}
