/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Pin the `plan` CALL SITE, not the helper it calls.
//!
//! ⚠️ WHY THIS FILE EXISTS. `ManifestSet::knows_test` has a unit test, and that test
//! stays green when the refusal in `print_plan` is deleted outright:
//! `agent(hermit-004)` measured exactly that on review — replacing
//! `if !manifests.knows_test(id)` with `if false` restored the original defect and
//! every suite still passed. "The helper answers correctly" and "`plan` consults it
//! and refuses" are different facts, and this pull request is for the second one.
//!
//! These are subprocess cases on purpose. `fail()` calls `process::exit`, so an
//! in-process test cannot observe the refusal, and any pure function extracted to
//! make it testable would reintroduce the same gap one layer down: a test on that
//! function would still not prove `plan` calls it.
//!
//! ⚠️ AND THE TWO rc=0 CASES CARRY AS MUCH WEIGHT AS THE rc=2 ONE. Without them the
//! guard could be widened to `cells.is_empty()` — the wrong fix, which would turn
//! `audit-gaps`'s legitimate "no gaps" answer into a failure — and a suite containing
//! only the refusal case would stay green through that change.

use std::path::PathBuf;
use std::process::Command;

/// A real test id that exists in the portable manifests but has no cells in the
/// privileged lane, so a lane-mismatched query for it is legitimately empty.
const REAL_ID: &str = "applications/c-toolchain-workflow";
const UNKNOWN_ID: &str = "no-such-test-xyz";

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/ci/manifest-plan.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("ci/manifest-plan must sit two levels below the repo root")
        .to_path_buf()
}

/// Run `test-harness` with `args` from the repo root and return its exit code.
///
/// The binary resolves its sibling `hermit-manifest-plan` relative to its own path,
/// and Cargo builds both bins of this package for an integration test, so the pair
/// is coherent. A `None` code means a signal, which is a harness failure rather than
/// a verdict and is asserted against explicitly.
fn harness(args: &[&str]) -> i32 {
    let out = Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .current_dir(repo_root())
        .args(args)
        .output()
        .expect("failed to run test-harness");
    out.status.code().unwrap_or_else(|| {
        panic!(
            "test-harness died on a signal for args {args:?}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn plan_refuses_an_unknown_test_id() {
    assert_eq!(
        harness(&["plan", "--lane", "portable", "--test", UNKNOWN_ID]),
        2,
        "`plan` must REFUSE an id that is in no manifest. It exited 0 with an empty \
         list before this guard, and exited 0 for a real id too, so its exit code \
         carried no information either way -- a bisection driving off `plan` reads a \
         typo as 'nothing failed here' and converges on the wrong commit."
    );
}

#[test]
fn plan_still_accepts_a_real_id_that_selects_nothing_in_this_lane() {
    assert_eq!(
        harness(&["plan", "--lane", "privileged", "--test", REAL_ID]),
        0,
        "a REAL id with no cells in the requested lane is a correct, well-formed query \
         and must stay rc=0. This is the case a `cells.is_empty()` guard would have \
         refused, which is why the check is 'unknown id' instead."
    );
}

#[test]
fn audit_gaps_still_reports_no_gaps_as_success() {
    assert_eq!(
        harness(&["audit-gaps", "--lane", "privileged", "--test", REAL_ID]),
        0,
        "`print_plan` also serves `audit-gaps`, where an empty answer legitimately \
         means NO GAPS. Turning that into a failure would trade a silent false pass \
         for a loud false failure."
    );
}

#[test]
fn plan_accepts_a_real_id_in_its_own_lane() {
    assert_eq!(
        harness(&["plan", "--lane", "portable", "--test", REAL_ID]),
        0,
        "the ordinary path must be untouched"
    );
}

/// ⚠️ THE CONTROL THAT MUST FAIL. Every assertion above is an exit code, and a
/// `test-harness` that could not run at all would exit non-zero for everything --
/// which satisfies the refusal case while telling us nothing. An unfiltered `plan`
/// must succeed AND print cells, so a broken binary cannot masquerade as a working
/// guard.
#[test]
fn an_unfiltered_plan_still_produces_cells() {
    let out = Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .current_dir(repo_root())
        .args(["plan", "--lane", "portable", "--format", "json"])
        .output()
        .expect("failed to run test-harness");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an unfiltered plan must succeed"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.trim().starts_with('[') && text.trim().len() > 2,
        "an unfiltered plan must print a non-empty cell array, or the exit codes above \
         prove nothing about the guard: stdout was {:?}",
        text.chars().take(120).collect::<String>()
    );
}
