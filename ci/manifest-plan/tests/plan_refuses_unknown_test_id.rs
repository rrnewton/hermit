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
/// A second real portable id, so `--test` can be repeated with two values that are
/// each independently valid. Without a second VALID id the repeat case cannot be
/// distinguished from a lookup failure.
const SECOND_REAL_ID: &str = "applications/git-repository-workflow";
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

/// ⚠️ A REPEATED SELECTOR MUST REFUSE, BECAUSE LAST-VALUE-WINS UNDID THE GUARD ABOVE.
/// Measured at `979a50b17a75`, before the fix:
///
/// ```text
/// --test no-such-test-xyz                             rc=2
/// --test no-such-test-xyz --test applications/...      rc=0   []      <- the defect
/// --test applications/... --test no-such-test-xyz      rc=2
/// ```
///
/// The asymmetry is the whole finding: only the LAST occurrence was ever looked up,
/// so an unknown id in any position but the last produced a silent green -- the exact
/// failure the unknown-id refusal was added to remove, reappearing in the argument
/// parser instead of the id lookup. Found by the codex lane on this head and confirmed
/// independently by `agent(hermit-012)`; the claude lane, and I, had approved it.
#[test]
fn a_repeated_test_flag_is_refused_in_either_order() {
    assert_eq!(
        harness(&[
            "plan", "--lane", "portable", "--test", UNKNOWN_ID, "--test", REAL_ID
        ]),
        2,
        "an unknown id followed by a real one must REFUSE; before this fix it exited 0 \
         with an empty plan because the second --test overwrote the first"
    );
    assert_eq!(
        harness(&[
            "plan", "--lane", "portable", "--test", REAL_ID, "--test", UNKNOWN_ID
        ]),
        2,
        "and the reverse order too -- the fix must not depend on which one is last"
    );
}

/// Each single-valued selector with TWO INDEPENDENTLY VALID values for it.
///
/// ⚠️ THE VALUES ARE THE WHOLE POINT, AND THE FIRST VERSION OF THIS TEST GOT THEM
/// WRONG. It passed the placeholders `a` and `b`, which `--lane`, `--test`, `--mode`
/// and `--backend` each reject on their own. Those four assertions therefore could
/// not tell "refused because repeated" from "refused because the value is nonsense",
/// and they stayed green while the flag happily accepted repeats. Measured
/// 2026-08-26 by deleting `set_once` from all five arms of `parse()` and running the
/// binary directly: `--lane` 2, `--category` 0, `--test` 2, `--mode` 2,
/// `--backend` 2 — only `--category` moved, because `a` happens to be a legal
/// (merely empty) category. One of five bound, under a name claiming all five.
///
/// A test that cannot fail for four of the five reasons in its own name is worse
/// than no test: a reader grepping for coverage finds the name and stops looking.
const REPEATABLE_SELECTORS: [(&str, &str, &str); 5] = [
    ("--lane", "portable", "privileged"),
    ("--category", "applications", "c-programs"),
    ("--test", REAL_ID, SECOND_REAL_ID),
    ("--mode", "verify", "chaos"),
    ("--backend", "ptrace", "sabre"),
];

/// `plan` argv naming `flag` once per entry in `values`.
///
/// `--lane portable` is the ordinary base for these queries, but it must NOT be the
/// base for `--lane` itself: that would make the "repeat" case a triple and the
/// single-occurrence control a repeat, quietly inverting both.
fn plan_args<'a>(flag: &'a str, values: &[&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["plan"];
    if flag != "--lane" {
        argv.extend(["--lane", "portable"]);
    }
    for value in values {
        argv.extend([flag, value]);
    }
    argv
}

/// Every single-valued selector, not just the one the defect was found through.
/// `--jobs` already refused a repeat; these did not, and there is no reason for the
/// rule to differ per flag.
#[test]
fn every_single_valued_selector_refuses_a_repeat() {
    for (flag, first, second) in REPEATABLE_SELECTORS {
        assert_eq!(
            harness(&plan_args(flag, &[first, second])),
            2,
            "{flag} must be refused when repeated ({first} then {second}); both \
             values are accepted singly, so only the repeat can explain a refusal"
        );
    }
}

/// ⚠️ THE ATTRIBUTION CONTROL for the test above, and that test proves nothing
/// without it. Every value used there must be accepted ON ITS OWN, so that the only
/// thing a repeat adds is the repetition. This is exactly the check whose absence
/// let four of the five assertions above pass vacuously; if someone later swaps in a
/// value that is independently invalid, this fails and names the flag and the value
/// rather than silently hollowing out the test next to it.
#[test]
fn each_repeat_value_is_independently_accepted_alone() {
    for (flag, first, second) in REPEATABLE_SELECTORS {
        for value in [first, second] {
            assert_eq!(
                harness(&plan_args(flag, &[value])),
                0,
                "{flag} {value} must be accepted on its own, or a repeat's rc=2 \
                 cannot be attributed to the repeat"
            );
        }
    }
}

/// ⚠️ THE CONTROL, and it is not optional here. Every assertion above is an exit code
/// of 2, and a `test-harness` that cannot run at all exits 2 for everything --
/// `agent(hermit-012)` nearly filed a refutation from exactly that, and so did I,
/// because only one of the package's two binaries had been built. A SINGLE selector
/// must still be accepted, or the refusals above prove nothing.
#[test]
fn a_single_occurrence_of_each_selector_is_still_accepted() {
    assert_eq!(
        harness(&["plan", "--lane", "portable", "--test", REAL_ID]),
        0
    );
    assert_eq!(harness(&["plan", "--lane", "portable"]), 0);
}
