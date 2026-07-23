/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Execution-backend selection for `hermit run`.
//!
//! Hermit's production backend is `reverie-ptrace`, which runs an arbitrary ELF
//! guest under seccomp + ptrace. Two additional Reverie backends are wired here:
//!
//! * [`hermit::Backend::Dbi`] — in-process DynamoRIO instrumentation
//!   (`reverie-dbi`); shells out to `drrun` to run the real guest program.
//! * [`hermit::Backend::Kvm`] — a KVM prototype (`reverie-kvm`) that is **not
//!   yet functional**: it cannot load or execute a Linux ELF program. It is
//!   held unavailable/fail-closed (see [`hermit::Backend::ensure_available`]).
//!   Tracked by <https://github.com/rrnewton/hermit/issues/198>.

use std::path::Path;
use std::process::Command as StdCommand;

use hermit::Error;
use hermit::ExitStatus;

/// The KVM backend is not yet a working execution backend.
///
/// `reverie-kvm` cannot load or execute an arbitrary Linux ELF program: it has
/// no ELF loader, no protected/long-mode setup, and no guest-kernel/syscall
/// ABI. Rather than silently ignore `program` and run a hardcoded "hello world"
/// demo (fake functionality that misleads users and agents into thinking the
/// backend works), this returns a clear error naming the program it refused to
/// run.
///
/// `Backend::Kvm` already fails closed in `Backend::unavailable_reason`, so this
/// is normally unreachable; it exists as honest defense-in-depth in case that
/// gate is ever removed. Progress toward a real backend is tracked in the issue
/// referenced below.
pub fn run_kvm(program: &Path) -> Result<ExitStatus, Error> {
    Err(Error::msg(format!(
        "KVM backend is not yet functional: it cannot execute {program:?} (or any real ELF \
         program). It lacks an ELF loader, protected-mode setup, and a guest-kernel/syscall ABI. \
         See https://github.com/rrnewton/hermit/issues/198 for tracking and use `--backend ptrace` \
         (the default) to run programs."
    )))
}

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
