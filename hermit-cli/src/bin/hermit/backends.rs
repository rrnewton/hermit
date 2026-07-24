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
//! * [`hermit::Backend::Sabre`] — SaBRe static rewriting with a Reverie plugin.
//!
//! The DBI backend runs the *real* guest ELF: [`run_dbi`] delegates to
//! [`reverie_dbi::DbiRunner`], which launches DynamoRIO's `drrun` with the
//! `reverie-dbi` native client. The client loads and executes `program`
//! in-process while counting branches, intercepting syscalls, and applying the
//! deterministic CPUID identity — no ptrace.
//!
//! Scope (DBI milestone 2b): this replaces the previous hand-rolled `drrun`
//! [`std::process::Command`] with the `reverie-dbi` library launcher (added as a
//! dependency in M2a), so the CLI and the crate share one launch path and gain
//! `DbiRunner`'s shebang handling and `ADDR_NO_RANDOMIZE` (ASLR-off) setup.
//!
//! It is deliberately NOT "Detcore over DbiGuest": the native client still runs
//! its compiled-in prototype tool, not Detcore. `DbiRunner` is a subprocess
//! launcher and exposes no `run::<Detcore>()`; loading Detcore as a Reverie
//! [`Tool`] through `reverie_dbi::DbiGuest` is blocked upstream in `reverie-dbi`
//! (the single-poll `run_ready` executor panics on `Poll::Pending`, and
//! `Guest::Stack`/`tail_inject`/`set_timer` are unimplemented) and is tracked as
//! later DBI milestone work. Consequently the DBI backend does not yet drive
//! Detcore's scheduler, so cross-thread determinism is not enforced the way the
//! ptrace backend enforces it, and `--strict`/`--verify` do not apply here.
//!
//! [`Tool`]: reverie::Tool

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as StdCommand;

use hermit::Error;
use hermit::ExitStatus;
use reverie_dbi::DbiRunner;

/// Runs `program`/`args` through the experimental DynamoRIO backend.
///
/// `reverie-dbi`'s native client is built by DynamoRIO's own CMake toolchain
/// (it is intentionally outside Cargo because DynamoRIO's package supplies the
/// required client linker flags). This delegates to [`reverie_dbi::DbiRunner`],
/// which invokes `drrun` with the prebuilt client. Configure it with two env
/// vars:
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

    // Preserve Hermit's explicit env-var contract (HERMIT_DRRUN / HERMIT_DBI_CLIENT)
    // rather than DbiRunner::from_env's DYNAMORIO_HOME lookup, but route the launch
    // through the shared library launcher.
    let runner = DbiRunner::new(&drrun, &client).map_err(|e| {
        Error::msg(format!(
            "failed to configure the DynamoRIO DBI runner (HERMIT_DRRUN={drrun}, \
             HERMIT_DBI_CLIENT={client}): {e}"
        ))
    })?;

    eprintln!("hermit: [dbi backend] running {program:?} under DynamoRIO ({drrun})");

    let mut guest = StdCommand::new(program);
    guest.args(args);

    let status = runner
        .status(&guest)
        .map_err(|e| Error::msg(format!("failed to launch drrun ({drrun}): {e}")))?;

    Ok(ExitStatus::Exited(status.code().unwrap_or(1)))
}

fn sabre_artifact(variable: &str, description: &str, executable: bool) -> Result<OsString, Error> {
    let value = std::env::var_os(variable).ok_or_else(|| {
        Error::msg(format!(
            "the sabre backend needs {variable}=<path-to-{description}>"
        ))
    })?;
    validate_sabre_artifact(Path::new(&value), variable, executable)
}

fn validate_sabre_artifact(
    requested_path: &Path,
    variable: &str,
    executable: bool,
) -> Result<OsString, Error> {
    let path = fs::canonicalize(requested_path).map_err(|error| {
        Error::msg(format!(
            "the sabre backend cannot access {variable}={}: {error}",
            requested_path.display()
        ))
    })?;
    let metadata = fs::metadata(&path).map_err(|error| {
        Error::msg(format!(
            "the sabre backend cannot inspect {variable}={}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(Error::msg(format!(
            "the sabre backend needs {variable}={} to be a regular file",
            path.display()
        )));
    }
    if executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::msg(format!(
            "the sabre backend needs {variable}={} to be executable",
            path.display()
        )));
    }
    Ok(path.into_os_string())
}

/// Runs `program` through the shared Reverie strace tool over SaBRe.
///
/// The SaBRe host and plugin live in the coordinated Reverie checkout, so
/// Hermit uses explicit artifact paths rather than taking an unreleased Cargo
/// dependency:
///
/// * `HERMIT_SABRE_RUNNER` — `reverie-sabre-strace` executable.
/// * `HERMIT_SABRE_BINARY` — pinned SaBRe executable.
/// * `HERMIT_SABRE_PLUGIN` — `libreverie_sabre_strace_plugin.so`.
// AUTONOMOUS-BOT-IMPLEMENTED
pub fn run_sabre(program: &Path, args: &[String]) -> Result<ExitStatus, Error> {
    let runner = sabre_artifact("HERMIT_SABRE_RUNNER", "reverie-sabre-strace", true)?;
    let sabre = sabre_artifact("HERMIT_SABRE_BINARY", "sabre", true)?;
    let plugin = sabre_artifact(
        "HERMIT_SABRE_PLUGIN",
        "libreverie_sabre_strace_plugin.so",
        false,
    )?;

    eprintln!("hermit: [sabre backend] tracing {program:?} with the shared Reverie tool");

    let status = StdCommand::new(&runner)
        .arg("--sabre")
        .arg(&sabre)
        .arg("--plugin")
        .arg(&plugin)
        .arg("--")
        .arg(program)
        .args(args)
        .status()
        .map_err(|error| {
            Error::msg(format!(
                "failed to launch the SaBRe runner {}: {error}",
                Path::new(&runner).display()
            ))
        })?;

    Ok(status.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sabre_artifact_returns_the_validated_absolute_path() {
        let file = tempfile::NamedTempFile::new_in(".").unwrap();
        let relative_path = file.path().file_name().unwrap();

        let resolved = validate_sabre_artifact(Path::new(relative_path), "test-artifact", false)
            .map(std::path::PathBuf::from)
            .unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, fs::canonicalize(file.path()).unwrap());
    }
}
