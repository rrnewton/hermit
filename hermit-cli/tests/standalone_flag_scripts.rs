// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Runs the `tests/standalone/*.sh` trace/replay drivers, which are the only
//! output-asserting coverage for several artifact-producing CLI flags.
//!
//! WHY THIS FILE EXISTS. Audited 2026-08-19: `--log-file`,
//! `--record-preemptions-to` and `--replay-schedule-from` had ZERO effective
//! coverage. Not because no test checked them -- the three scripts below check
//! them well -- but because nothing invoked the scripts. They had no Cargo,
//! nextest or manifest registration, so a correct test provided no coverage
//! while looking like coverage from the file listing.
//!
//! The scripts were also unrunnable in a public checkout: their guest command
//! hardcoded `./hermetic_infra/hermit/hermit-cli/src`, a Buck-internal path, so
//! `set -e` killed them on the first hermit invocation before any assertion.
//! That is fixed in the scripts themselves, which now prefer this repository's
//! layout and fall back to the internal one.
//!
//! WHAT THE SCRIPTS ASSERT, which is why they are worth wiring rather than
//! replacing: each records a schedule, replays it, and then checks the OUTPUT --
//! `grep "Trace loaded"` against the replay log (so `--log-file` must have been
//! written AND `--replay-schedule-from` must have taken effect), a DESYNC scan,
//! and a `hermit log-diff` between the two logs. `replay_and_print_stacktraces`
//! additionally reads `--stacktrace-event=N,PATH` output back through `jq`.
//!
//! MUTATION-PROVEN, both before and after wiring: a hermit shim that accepts
//! `--log-file=` and silently discards it makes these fail. A test that merely
//! passed a flag and ignored the result would not, and that is the distinction
//! this file is here to preserve. If you weaken these to "the script exited 0
//! for any reason", you have removed the coverage without removing the test.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// The repository root; `hermit-cli` sits directly inside it.
fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository root")
}

/// The hermit under test. `HERMIT_BIN` wins so the same scripts can be pointed
/// at an arbitrary build -- including a deliberately broken one, which is how
/// the mutation check is run.
fn hermit_under_test() -> PathBuf {
    std::env::var_os("HERMIT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hermit")))
}

/// Run one driver script and require BOTH a zero exit and its own success
/// marker.
///
/// The marker matters. `set -e` in the scripts makes most failures non-zero, but
/// requiring `Test passed.` as well means a script that is silently truncated,
/// or that grows an early `exit 0`, cannot pass by accident. Absence of a
/// failure is not evidence of the assertions having run.
fn run_driver(script: &str) {
    let path = repository().join("tests/standalone").join(script);
    assert!(path.is_file(), "missing driver script: {}", path.display());

    let output = Command::new("bash")
        .arg(&path)
        .arg(hermit_under_test())
        // The guest walks a repository-relative path, so the driver's working
        // directory is part of the contract, not incidental.
        .current_dir(repository())
        .output()
        .unwrap_or_else(|error| panic!("failed to start {}: {error}", path.display()));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{script} failed with {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status,
    );
    assert!(
        stdout.contains("Test passed.") || stderr.contains("Test passed."),
        "{script} exited 0 but never reported success; its assertions may not have run\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
}

/// `--log-file`, `--record-preemptions-to`, `--replay-schedule-from`.
#[test]
fn replay_trace_script_passes() {
    run_driver("replay_trace.sh");
}

/// The same three flags with `--chaos`, plus a syscall-sequence comparison
/// between the recorded and replayed runs.
#[test]
fn replay_chaos_trace_script_passes() {
    run_driver("replay_chaos_trace.sh");
}

/// Adds `--stacktrace-event=N,PATH`: the emitted stack files are parsed with
/// `jq` and their instruction pointers compared across record and replay.
#[test]
fn replay_and_print_stacktraces_script_passes() {
    run_driver("replay_and_print_stacktraces.sh");
}
