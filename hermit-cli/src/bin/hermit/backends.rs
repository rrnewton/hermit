/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED (recreate of PR #181, which conflicted on rebase)

//! Execution-backend dispatch for `hermit run`.
//!
//! Hermit's production backend is `reverie-ptrace`, which runs an arbitrary ELF
//! guest under seccomp + ptrace. The experimental DynamoRIO backend is wired
//! here so the same Detcore/Reverie contracts can be exercised over an
//! alternative execution mechanism:
//!
//! * [`hermit::Backend::Dbi`] — in-process DynamoRIO instrumentation (`reverie-dbi`).
//!
//! The DBI backend runs the *real* guest ELF: [`run_dbi`] shells out to
//! DynamoRIO's `drrun` with the `reverie-dbi` client, which loads and executes
//! `program` in-process while counting branches, intercepting syscalls, and
//! applying the deterministic CPUID identity — no ptrace. It is still a
//! prototype: it does not yet drive Detcore's scheduler, so cross-thread
//! determinism is not enforced the way the ptrace backend enforces it.

use std::path::Path;
use std::process::Command as StdCommand;

use hermit::Error;
use hermit::ExitStatus;

/// Runs `program`/`args` through the experimental DynamoRIO backend.
///
/// `reverie-dbi`'s native client is built and launched by DynamoRIO's own
/// toolchain (it is intentionally outside Cargo because DynamoRIO's CMake
/// package supplies the required client linker flags). This shells out to
/// `drrun` with the prebuilt client. Configure it with two env vars:
///
/// * `HERMIT_DRRUN` — path to DynamoRIO's `drrun`.
/// * `HERMIT_DBI_CLIENT` — path to `libreverie_dbi_client.so`.
pub fn run_dbi(program: &Path, args: &[String]) -> Result<ExitStatus, Error> {
    let drrun = std::env::var("HERMIT_DRRUN").map_err(|_| {
        Error::msg(
            "the dbi backend needs the DynamoRIO SDK. Set HERMIT_DRRUN=<dynamorio>/bin64/drrun and \
             HERMIT_DBI_CLIENT=<...>/libreverie_dbi_client.so (build the client with \
             reverie-dbi/scripts/build-client.sh). See reverie-dbi/README.md.",
        )
    })?;
    let client = std::env::var("HERMIT_DBI_CLIENT").map_err(|_| {
        Error::msg("the dbi backend needs HERMIT_DBI_CLIENT=<...>/libreverie_dbi_client.so")
    })?;

    eprintln!("hermit: [dbi backend] running {program:?} under DynamoRIO ({drrun})");

    let status = StdCommand::new(&drrun)
        .arg("-disable_rseq")
        .arg("-c")
        .arg(&client)
        .arg("--")
        .arg(program)
        .args(args)
        .status()
        .map_err(|e| Error::msg(format!("failed to launch drrun ({drrun}): {e}")))?;

    Ok(ExitStatus::Exited(status.code().unwrap_or(1)))
}
