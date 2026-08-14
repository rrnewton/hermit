/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The scheduler daemon must emit nothing at INFO before the guest is
//! registered.
//!
//! INFO is the scope `--verify-strict` compares exactly
//! (`ComparedLogScope::Info`: "Every INFO message, exactly; DEBUG/TRACE captures
//! remain diagnostic"). Anything the daemon logs at INFO *before* it blocks on
//! guest registration is ordered by host async-task startup, not by guest
//! execution, so it races the root thread's `ThreadState::new` seeding messages.
//! Record and replay bias that race differently, which made record/replay parity
//! flaky rather than wrong: measured on the `system-utils/record-getpid` guest,
//! `record start --strict --verify --verify-strict` passed 17/20 and then 37/40,
//! with every divergence a pure permutation of the daemon line against the two
//! seeding lines and nothing else in the 86 compared messages differing. Moving
//! that one line to DEBUG took the same guest to 40/40.
//!
//! These tests bracket the change in both directions: the racing line must be
//! absent from the compared INFO envelope, the deterministic post-registration
//! milestone must still be present at INFO, and the diagnostic must still be
//! reachable at DEBUG.

use std::process::Command;

/// Emitted by the scheduler daemon BEFORE it waits for guest registration.
/// Its position is decided by host task startup, so it must not reach INFO.
const PRE_REGISTRATION_LINE: &str = "daemon task starting up";

/// Emitted AFTER the guest is queued, so it is ordered by guest execution and
/// belongs in the compared INFO envelope.
const POST_REGISTRATION_MILESTONE: &str = "guest in queue, scheduler proceeding";

fn hermit_stderr(log_level: &str) -> String {
    let output = Command::new("timeout")
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            log_level,
            "run",
            "--backend=ptrace",
            "--strict",
            "--base-env=minimal",
            "--",
            "/bin/true",
        ])
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit at --log {log_level}: {error}"));
    assert!(
        output.status.success(),
        "hermit run at --log {log_level} failed with {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The specific line that caused the flake must not be in the INFO envelope,
/// and the deterministic milestone must still be.
#[test]
fn scheduler_daemon_startup_line_is_not_in_the_compared_info_envelope() {
    let stderr = hermit_stderr("info");

    assert!(
        !stderr.contains(PRE_REGISTRATION_LINE),
        "the scheduler daemon's pre-registration line reached INFO, which is the scope \
         --verify-strict compares exactly. Its position is host task-startup order, not guest \
         execution, so at INFO it makes record/replay parity flaky. Keep it at DEBUG.\nstderr:\n{stderr}",
    );

    // Bracket the other direction: the fix must not have simply deleted the
    // diagnostic. The post-registration milestone is deterministic and stays.
    assert!(
        stderr.contains(POST_REGISTRATION_MILESTONE),
        "the deterministic post-registration scheduler milestone is missing from INFO; the \
         startup diagnostic was removed rather than re-scoped.\nstderr:\n{stderr}",
    );
}

/// The exact Detcore INFO messages that precede guest registration, in order.
///
/// Each is emitted on the sequenced setup path -- `RunQueue::new` for the
/// runqueue seed and `ThreadState::new` for the two root-thread seeds -- so
/// their order is fixed by construction, not by host task scheduling. Verified
/// byte-stable across 20 consecutive `--log=info` runs.
///
/// Deliberately NOT written as "no scheduler message may precede registration":
/// that predicate is wrong. `SCHEDRAND` is a `detcore::scheduler::runqueue`
/// message and is perfectly deterministic, because a constructor on the setup
/// path cannot race the root thread. The hazard is narrower -- a message emitted
/// from the *spawned daemon task* before it awaits registration -- and the log
/// text does not say which task emitted a line. So this pins the whole prefix
/// instead: any newly added pre-registration INFO message fails this test and
/// has to be classified by a human as sequenced or racing.
const EXPECTED_PRE_REGISTRATION_INFO: &[&str] = &[
    "DETLOG SCHEDRAND: seeding scheduler runqueue",
    "DETLOG USER RAND: seeding PRNG for root thread",
    "DETLOG CHAOSRAND: seeding chaos scheduler",
];

#[test]
fn pre_registration_info_prefix_is_exactly_the_sequenced_setup_messages() {
    let stderr = hermit_stderr("info");

    let milestone_index = stderr
        .lines()
        .position(|line| line.contains(POST_REGISTRATION_MILESTONE))
        .unwrap_or_else(|| {
            panic!("scheduler never reported guest registration at INFO\nstderr:\n{stderr}")
        });

    // Restrict to Detcore INFO lines: host-dependent warnings (for example the
    // ARCH_SET_CPUID notice on hosts without CPUID faulting) are logged at other
    // levels by other crates and must not make this test host-specific.
    let prefix: Vec<&str> = stderr
        .lines()
        .take(milestone_index)
        .filter(|line| line.contains(" INFO detcore"))
        .collect();

    assert_eq!(
        prefix.len(),
        EXPECTED_PRE_REGISTRATION_INFO.len(),
        "the set of Detcore INFO messages emitted before guest registration changed. Every such \
         message is compared exactly by --verify-strict, so a message emitted from a concurrent \
         task here reintroduces record/replay parity flakiness. Classify the new message: if it \
         is emitted on the sequenced setup path, add it to EXPECTED_PRE_REGISTRATION_INFO; if it \
         comes from the spawned scheduler daemon, emit it at DEBUG instead.\nobserved:\n{prefix:#?}",
    );

    for (observed, expected) in prefix.iter().zip(EXPECTED_PRE_REGISTRATION_INFO) {
        assert!(
            observed.contains(expected),
            "pre-registration INFO messages changed order or content; expected {expected:?} but \
             saw {observed:?}\nfull prefix:\n{prefix:#?}",
        );
    }
}

/// The diagnostic is re-scoped, not lost: it must still be reachable at DEBUG.
#[test]
fn scheduler_daemon_startup_line_remains_available_at_debug() {
    let stderr = hermit_stderr("debug");

    assert!(
        stderr.contains(PRE_REGISTRATION_LINE),
        "the scheduler daemon startup diagnostic is gone from DEBUG too; it should have been \
         re-scoped out of the compared INFO envelope, not deleted.\nstderr:\n{stderr}",
    );
}
