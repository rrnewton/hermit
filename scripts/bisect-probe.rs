#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! ```
//!
//! Run a named LIST of e2e test ids as one bisection probe, and emit one verdict
//! per REQUESTED id.
//!
//! # Why this exists, and why it is not about speed
//!
//! Measured 2026-08-26 on `origin/main` at `82c0433ef9`, on the host recorded for
//! this file in `docs/TESTING_ENVIRONMENTS.md` under "Named measurement hosts":
//!
//! ```text
//! BUILD  36.33 s   hermit binary 36.21 + guest program 0.12 per test id
//! TEST    3.53 s   one cell, observed range 1.6 - 10.9
//! ```
//!
//! 91% of a one-test bisect step is a build that is paid once no matter how many
//! tests ride on it, so ten tests cost about 1.21x one test and batching is very
//! nearly free. But the per-invocation overhead of `test-harness` is only ~0.42 s,
//! so collapsing a shell loop into one process saves ~4 s of a 40-70 s probe.
//! **That is not the reason to build this.** The reason is that a shell loop
//! produces N artifacts and no statement of what was ASKED FOR: typo one id out of
//! ten, and the concatenated results simply have nine rows. Nothing distinguishes a
//! nine-id probe from a ten-id probe that lost one.
//!
//! The speed that IS available comes from concurrency, and only inside one driver:
//! `run --prebuilt` over six cells took 21.76 s serially and 11.55 s at `--jobs 6`,
//! floored by the slowest single cell at 10.9 s.
//!
//! # ⚠️ ENUMERATION COMES FROM `plan`; THE FAIL-CLOSED GUARD DOES NOT
//!
//! This driver enumerates cells with `test-harness plan --format json`, because
//! expanding an id into its (mode, backend) cells is exactly what `plan` is for --
//! a test id is not one cell (measured: 237 distinct portable ids expand to 304
//! required cells, median 1, max 3).
//!
//! **But `plan` does not fail closed on an unmatched id, and this driver must.**
//! Measured three times, `rc` captured on its own line each time:
//!
//! ```text
//! test-harness plan  --lane portable --test no-such-test-xyz   rc=0   (empty, silent)
//! test-harness run   --lane portable --test no-such-test-xyz   rc=2   "filters selected no cells"
//! ```
//!
//! The `cells.is_empty()` guard exists at `test-harness.rs:1089` (`build`) and
//! `:1236` (`run`) and NOWHERE ELSE; `plan()` at `:1052` has none, and it returns 0
//! for a real id too, so its exit code carries no information in either direction.
//! An id that matches nothing is the one input that must never read as "no failures
//! here" -- a bisect would converge, confidently, on the wrong commit. So the guard
//! lives HERE: every requested id that `plan` expands to zero cells is reported as
//! [`Verdict::Unselected`], and the run is refused before a single cell executes.
//!
//! # The verdict set
//!
//! One record per REQUESTED id, never per cell that happened to run:
//!
//! | verdict | meaning |
//! | --- | --- |
//! | `PASS` | every cell for this id ran and passed |
//! | `FAIL` | at least one cell ran and failed -- the only product signal |
//! | `ERROR` | at least one cell could not be run (harness/infrastructure) |
//! | `SKIPPED` | every cell was host-inapplicable |
//! | `UNSELECTED` | the id matched no cell at all -- ⚠️ NOT a pass |
//! | `MISSING` | `plan` expanded it, but the run produced no row for some cell |
//!
//! `UNSELECTED` and `MISSING` are the two that a naive probe reports as green, and
//! they are why the exit code is 3 rather than 1: a bisect driver that treats
//! "non-zero means the bug is present" would otherwise read a typo as a reproduction.
//! Only `PASS` for every requested id exits 0.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

/// Every id passed, every id ran, every cell passed.
const RC_OK: i32 = 0;
/// At least one cell ran and FAILED. The product signal a bisect is looking for.
const RC_FAIL: i32 = 1;
/// The probe could not be trusted: a usage error, or the harness could not be read.
const RC_UNUSABLE: i32 = 2;
/// ⚠️ AT LEAST ONE REQUESTED ID WAS NOT MEASURED -- unselected, missing, or errored.
/// Deliberately distinct from RC_FAIL: "the bug reproduced" and "I never looked" are
/// the two answers a bisect must never confuse, and both are non-zero.
const RC_NOT_MEASURED: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    Pass,
    Fail,
    Error,
    Skipped,
    Unselected,
    Missing,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Error => "ERROR",
            Verdict::Skipped => "SKIPPED",
            Verdict::Unselected => "UNSELECTED",
            Verdict::Missing => "MISSING",
        }
    }

    /// Did this id actually get measured? `Skipped` counts as measured: a
    /// host-inapplicable cell was looked at and declared out of scope, which is a
    /// different fact from never having been selected.
    fn measured(self) -> bool {
        matches!(self, Verdict::Pass | Verdict::Fail | Verdict::Skipped)
    }
}

/// Fold the per-cell outcomes for ONE requested id into that id's verdict.
///
/// ⚠️ ORDER MATTERS AND IT IS NOT ALPHABETICAL. `Missing` outranks everything
/// because an absent row means the cell was never observed, and a probe that
/// reported PASS while one of its cells went unobserved is the exact failure this
/// driver exists to prevent. `Error` outranks `Fail` for the same reason: an
/// infrastructure error is "I could not measure", not "the product is broken", and
/// a bisect that reads it as a reproduction converges on the wrong commit.
fn fold(expected: usize, outcomes: &[String]) -> Verdict {
    if expected == 0 {
        return Verdict::Unselected;
    }
    if outcomes.len() < expected {
        return Verdict::Missing;
    }
    if outcomes.iter().any(|o| o == "ERROR") {
        return Verdict::Error;
    }
    if outcomes.iter().any(|o| o == "FAIL") {
        return Verdict::Fail;
    }
    if !outcomes.is_empty() && outcomes.iter().all(|o| o == "HOST-INAPPLICABLE") {
        return Verdict::Skipped;
    }
    Verdict::Pass
}

/// One row per REQUESTED id, in request order, whatever the run produced.
///
/// ⚠️ THE LENGTH OF THIS IS THE WHOLE POINT, AND IT IS WHY THE FUNCTION IS PURE.
/// The failure a bisect cannot survive is an id that quietly vanishes: run ten,
/// report nine, and the missing one reads as "nothing failed here". So the output is
/// built by iterating the REQUESTED ids and looking each one up, never by iterating
/// the rows that came back -- a shape in which a dropped id is unrepresentable rather
/// than merely unlikely.
fn report_rows(
    ids: &BTreeSet<String>,
    counts: &BTreeMap<String, usize>,
    outcomes: &BTreeMap<String, Vec<String>>,
) -> Vec<(String, Verdict, Vec<String>, usize)> {
    ids.iter()
        .map(|id| {
            let expected = counts.get(id).copied().unwrap_or(0);
            let got = outcomes.get(id).cloned().unwrap_or_default();
            (id.clone(), fold(expected, &got), got, expected)
        })
        .collect()
}

/// The exit code for a whole probe, from its per-id verdicts.
fn probe_rc(verdicts: &[Verdict]) -> i32 {
    if verdicts.iter().any(|v| !v.measured()) {
        return RC_NOT_MEASURED;
    }
    if verdicts.iter().any(|v| *v == Verdict::Fail) {
        return RC_FAIL;
    }
    RC_OK
}

fn fail(message: &str) -> ! {
    eprintln!("bisect-probe: {message}");
    std::process::exit(RC_UNUSABLE);
}

fn repo_root() -> PathBuf {
    let here = Path::new(file!())
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    here.canonicalize().unwrap_or(here)
}

/// Ask `plan` which cells each requested id expands to.
///
/// Returns a map from id to cell count. An id absent from `plan`'s output maps to
/// zero, which the caller turns into `UNSELECTED` -- see the module note: `plan`
/// will not do that for us.
fn enumerate(root: &Path, harness: &Path, lane: &str, ids: &BTreeSet<String>) -> BTreeMap<String, usize> {
    let out = Command::new(harness)
        .current_dir(root)
        .args(["plan", "--lane", lane, "--format", "json"])
        .output()
        .unwrap_or_else(|e| fail(&format!("could not run {}: {e}", harness.display())));
    if !out.status.success() {
        fail(&format!(
            "test-harness plan --lane {lane} exited {:?}; a failed enumeration is not an empty one",
            out.status.code()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| fail(&format!("could not parse plan output as JSON: {e}")));
    let rows = parsed.as_array().unwrap_or_else(|| fail("plan output was not a JSON array"));

    let mut counts: BTreeMap<String, usize> = ids.iter().map(|id| (id.clone(), 0)).collect();
    for row in rows {
        let id = row
            .get("id")
            .and_then(|i| i.get("test"))
            .or_else(|| row.get("test"))
            .and_then(|t| t.as_str());
        if let Some(id) = id {
            if let Some(slot) = counts.get_mut(id) {
                *slot += 1;
            }
        }
    }
    counts
}

fn main() {
    rust_script_prelude::init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--self-test") {
        std::process::exit(self_test());
    }
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "usage: bisect-probe.rs [--lane LANE] [--jobs N] ID [ID...]\n\
             \n\
             Runs a list of e2e test ids as one probe and prints one verdict per\n\
             REQUESTED id. Exit 0 only if every id passed; 1 if a cell FAILED;\n\
             3 if any id was not measured (UNSELECTED / MISSING / ERROR); 2 if the\n\
             probe itself could not run.\n\
             \n\
             An id that matches no cell is UNSELECTED and exits 3. It is NEVER a pass:\n\
             `test-harness plan` returns 0 for an unmatched id, so this driver supplies\n\
             the guard that plan does not have."
        );
        std::process::exit(RC_UNUSABLE);
    }

    let mut lane = "portable".to_string();
    let mut jobs = "8".to_string();
    let mut ids: BTreeSet<String> = BTreeSet::new();
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--lane" => lane = it.next().unwrap_or_else(|| fail("--lane needs a value")).clone(),
            "--jobs" => jobs = it.next().unwrap_or_else(|| fail("--jobs needs a value")).clone(),
            other if other.starts_with("--") => fail(&format!("unknown option {other}")),
            other => {
                ids.insert(other.to_string());
            }
        }
    }
    if ids.is_empty() {
        fail("no test ids given; an empty probe is not a passing probe");
    }

    let root = repo_root();
    let harness = root.join("target/debug/test-harness");
    if !harness.exists() {
        fail(&format!(
            "{} is missing; build it with `cargo build -p hermit-manifest-plan --bin test-harness`",
            harness.display()
        ));
    }

    // ⚠️ ENUMERATE AND REFUSE BEFORE RUNNING ANYTHING. An unmatched id is a defect in
    // the probe's INPUT, and discovering it after 40 seconds of cells is strictly
    // worse than discovering it now. This is also the check `plan` itself will not do.
    let counts = enumerate(&root, &harness, &lane, &ids);
    let unselected: Vec<&String> = counts.iter().filter(|(_, n)| **n == 0).map(|(k, _)| k).collect();
    if !unselected.is_empty() {
        for id in &unselected {
            println!("UNSELECTED\t{id}\t0 cells -- matched nothing in lane {lane}");
        }
        eprintln!(
            "bisect-probe: REFUSED: {} of {} requested id(s) matched no cell: {}. \
             An unmatched id is NOT a pass -- `test-harness plan` exits 0 for one, which is \
             why this driver checks. Nothing was run; fix the list and re-probe.",
            unselected.len(),
            ids.len(),
            unselected.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        );
        std::process::exit(RC_NOT_MEASURED);
    }

    // One `run` per id, because `--test` is singular and exact. Concurrency comes from
    // `--jobs` inside each invocation; the ~0.42s per-process overhead is noise beside
    // a 3.5s median cell, which is why this is a loop and not a harness change.
    let mut outcomes: BTreeMap<String, Vec<String>> = ids.iter().map(|i| (i.clone(), vec![])).collect();
    for id in &ids {
        let results = root.join(format!("target/bisect-probe-{}.jsonl", id.replace('/', "-")));
        let _ = std::fs::remove_file(&results);
        let status = Command::new(&harness)
            .current_dir(&root)
            .args(["run", "--lane", &lane, "--test", id, "--prebuilt", "--jobs", &jobs])
            .arg("--results")
            .arg(&results)
            // The child's own PASS/FAIL chatter would interleave with this driver's
            // one-line-per-id report and make the probe harder to read, not easier.
            // The rows are the record; they are read back from --results below.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if status.is_err() {
            outcomes.get_mut(id).map(|v| v.push("ERROR".to_string()));
            continue;
        }
        // ⚠️ READ THE ROWS, NOT THE EXIT CODE. `run` exits non-zero for a failed cell
        // AND for a harness error, and the per-row `outcome` is the only thing that
        // separates them. An unreadable results file leaves the vector short, which
        // folds to MISSING rather than to PASS.
        if let Ok(text) = std::fs::read_to_string(&results) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(row) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(o) = row.get("outcome").and_then(|o| o.as_str()) {
                        outcomes.get_mut(id).map(|v| v.push(o.to_string()));
                    }
                }
            }
        }
    }

    let report = report_rows(&ids, &counts, &outcomes);
    let verdicts: Vec<Verdict> = report.iter().map(|(_, v, _, _)| *v).collect();
    for (id, verdict, got, expected) in &report {
        println!(
            "{}\t{id}\t{}/{} cell(s): {}",
            verdict.label(),
            got.len(),
            expected,
            if got.is_empty() { "no rows".to_string() } else { got.join(",") }
        );
    }

    let rc = probe_rc(&verdicts);
    let not_measured = verdicts.iter().filter(|v| !v.measured()).count();
    eprintln!(
        "bisect-probe: {} id(s) requested, {} measured, {} NOT measured -- rc={rc}",
        ids.len(),
        ids.len() - not_measured,
        not_measured
    );
    std::process::exit(rc);
}

fn self_test() -> i32 {
    let mut bad: Vec<String> = Vec::new();
    let pass = || vec!["PASS".to_string()];

    // ⚠️ THE CASE THE WHOLE DRIVER EXISTS FOR. Zero cells is UNSELECTED, never PASS.
    // `test-harness plan` exits 0 on an unmatched id (measured three times), so if this
    // fold ever returns Pass for expected == 0, every bisect step reads green and the
    // search converges confidently on the wrong commit.
    if fold(0, &[]) != Verdict::Unselected {
        bad.push("zero cells must be UNSELECTED, not a pass".into());
    }
    if fold(0, &pass()) != Verdict::Unselected {
        bad.push("zero EXPECTED cells stays UNSELECTED even if rows appear".into());
    }
    // A cell that plan promised and the run never reported is MISSING, not PASS.
    if fold(2, &pass()) != Verdict::Missing {
        bad.push("a short row set must be MISSING, not a pass".into());
    }
    // Precedence: cannot-measure outranks measured-and-broken.
    if fold(2, &["ERROR".into(), "FAIL".into()]) != Verdict::Error {
        bad.push("ERROR must outrank FAIL -- 'I could not look' is not 'it is broken'".into());
    }
    if fold(2, &["FAIL".into(), "PASS".into()]) != Verdict::Fail {
        bad.push("any FAIL makes the id FAIL".into());
    }
    if fold(1, &["HOST-INAPPLICABLE".into()]) != Verdict::Skipped {
        bad.push("an all-host-inapplicable id is SKIPPED".into());
    }
    if fold(1, &pass()) != Verdict::Pass {
        bad.push("a single passing cell is PASS".into());
    }

    // ⚠️ AND THE CONTROLS THAT MUST FAIL. Without these, a fold that returned
    // Unselected for everything would satisfy the two cases above, and a probe_rc that
    // returned 3 always would satisfy the not-measured cases. Both are the reassuring
    // direction: they make every bisect step look inconclusive rather than green, which
    // is safer but equally useless.
    if fold(1, &pass()) == Verdict::Unselected {
        bad.push("control: a real passing cell must NOT read as UNSELECTED".into());
    }
    if probe_rc(&[Verdict::Pass, Verdict::Pass]) != RC_OK {
        bad.push("control: an all-pass probe must exit 0".into());
    }

    // The exit codes a bisect driver reads. UNSELECTED must not share a code with FAIL,
    // or "the id was typo'd" and "the bug reproduced" become the same answer.
    if probe_rc(&[Verdict::Unselected]) != RC_NOT_MEASURED {
        bad.push("UNSELECTED must exit RC_NOT_MEASURED".into());
    }
    if probe_rc(&[Verdict::Missing]) != RC_NOT_MEASURED {
        bad.push("MISSING must exit RC_NOT_MEASURED".into());
    }
    if probe_rc(&[Verdict::Error]) != RC_NOT_MEASURED {
        bad.push("ERROR must exit RC_NOT_MEASURED".into());
    }
    if probe_rc(&[Verdict::Fail]) != RC_FAIL {
        bad.push("FAIL must exit RC_FAIL".into());
    }
    if RC_FAIL == RC_NOT_MEASURED {
        bad.push("RC_FAIL and RC_NOT_MEASURED must differ, or a typo reads as a repro".into());
    }
    // A mixed probe reports NOT MEASURED, not FAIL: one unmeasured id makes the whole
    // step untrustworthy even when another id genuinely failed.
    if probe_rc(&[Verdict::Fail, Verdict::Unselected]) != RC_NOT_MEASURED {
        bad.push("an unmeasured id must dominate a failing one".into());
    }

    for b in &bad {
        eprintln!("bisect-probe self-test: FAIL -- {b}");
    }
    if !bad.is_empty() {
        return 1;
    }
    println!(
        "PASS: bisect-probe folds per-cell outcomes into one verdict per REQUESTED id, \
         reports an unmatched id as UNSELECTED rather than a pass, and keeps \
         not-measured (rc=3) distinct from failed (rc=1)"
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// ⚠️ THE TEST THAT CANNOT BE SATISFIED BY ACCIDENT, asked for by
    /// `agent(hermit-005)` on the review of this file: the number of verdicts must
    /// equal the number of REQUESTED ids, for a mixed list. An implementation that
    /// iterated the rows that came back instead of the ids that were asked for would
    /// pass every other test here and fail this one.
    #[test]
    fn every_requested_id_gets_exactly_one_row() {
        let requested = ids(&["a/one", "b/two", "c/three", "d/four"]);
        let counts = BTreeMap::from([
            ("a/one".to_string(), 1usize),
            ("b/two".to_string(), 2),
            ("c/three".to_string(), 0), // matched nothing
            ("d/four".to_string(), 1),
        ]);
        let outcomes = BTreeMap::from([
            ("a/one".to_string(), vec!["PASS".to_string()]),
            ("b/two".to_string(), vec!["PASS".to_string()]), // one row short
            // c/three: no entry at all
            ("d/four".to_string(), vec!["FAIL".to_string()]),
        ]);
        let rows = report_rows(&requested, &counts, &outcomes);
        assert_eq!(
            rows.len(),
            requested.len(),
            "every requested id must produce exactly one row; a dropped id is a bisect \
             converging on a set it never ran"
        );
        let seen: BTreeSet<String> = rows.iter().map(|(id, _, _, _)| id.clone()).collect();
        assert_eq!(seen, requested, "the reported ids must be the requested ids");
    }

    /// A requested id that matches nothing is UNSELECTED and PRESENT in the output.
    /// Absence is the failure mode; reporting it is the fix.
    #[test]
    fn an_unmatched_id_is_unselected_and_still_reported() {
        let requested = ids(&["real/one", "typo/xyz"]);
        let counts = BTreeMap::from([("real/one".to_string(), 1usize), ("typo/xyz".to_string(), 0)]);
        let outcomes = BTreeMap::from([("real/one".to_string(), vec!["PASS".to_string()])]);
        let rows = report_rows(&requested, &counts, &outcomes);
        let typo = rows.iter().find(|(id, _, _, _)| id == "typo/xyz");
        let typo = typo.expect("the unmatched id must appear in the report, not vanish");
        assert_eq!(
            typo.1,
            Verdict::Unselected,
            "an id matching nothing must be UNSELECTED, never PASS"
        );
        assert_eq!(
            probe_rc(&rows.iter().map(|(_, v, _, _)| *v).collect::<Vec<_>>()),
            RC_NOT_MEASURED,
            "a probe containing an unmeasured id must not exit 0"
        );
    }

    /// The control that makes the pair above mean something: an all-real, all-passing
    /// list exits 0. Without it, a report_rows that returned UNSELECTED for everything
    /// would satisfy both tests above.
    #[test]
    fn an_all_real_all_passing_list_exits_zero() {
        let requested = ids(&["real/one", "real/two"]);
        let counts = BTreeMap::from([("real/one".to_string(), 1usize), ("real/two".to_string(), 1)]);
        let outcomes = BTreeMap::from([
            ("real/one".to_string(), vec!["PASS".to_string()]),
            ("real/two".to_string(), vec!["PASS".to_string()]),
        ]);
        let rows = report_rows(&requested, &counts, &outcomes);
        assert!(rows.iter().all(|(_, v, _, _)| *v == Verdict::Pass));
        assert_eq!(
            probe_rc(&rows.iter().map(|(_, v, _, _)| *v).collect::<Vec<_>>()),
            RC_OK
        );
    }

    /// `ERROR` outranks `FAIL`: "I could not measure" is not "the product is broken",
    /// and a bisect that reads an infrastructure error as a reproduction converges on
    /// the wrong commit.
    #[test]
    fn cannot_measure_outranks_measured_and_broken() {
        assert_eq!(fold(2, &["ERROR".into(), "FAIL".into()]), Verdict::Error);
        assert_eq!(fold(2, &["PASS".into()]), Verdict::Missing);
        assert_ne!(RC_FAIL, RC_NOT_MEASURED);
    }
}
