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
//! **`plan` now refuses an id that is in no manifest, but this driver still must
//! check, for a different reason.** This probe asks `plan` for the WHOLE LANE and
//! never passes the requested ids to it, so `plan`'s unknown-id guard is never
//! consulted here. An id that exists but selects no cell in this lane is a correct,
//! empty answer to `plan` and still has to be UNSELECTED for a bisection. The two
//! checks answer different questions and this one does not become redundant.
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
        .map(|id| report_row(id, counts, outcomes))
        .collect()
}

/// The row for ONE requested id. Factored out of [`report_rows`] so the streaming
/// printer and the end-of-run fold cannot drift apart.
///
/// ⚠️ THE STREAMING PRINTER MUST NOT BE A SECOND IMPLEMENTATION. Printing a line
/// as each id finishes, and separately folding the same map at the end for the exit
/// code, is two computations of one fact; if they ever disagree the operator reads
/// one answer on the terminal and `git bisect` acts on the other. One function,
/// called from both.
fn report_row(
    id: &str,
    counts: &BTreeMap<String, usize>,
    outcomes: &BTreeMap<String, Vec<String>>,
) -> (String, Verdict, Vec<String>, usize) {
    let expected = counts.get(id).copied().unwrap_or(0);
    let got = outcomes.get(id).cloned().unwrap_or_default();
    (id.to_string(), fold(expected, &got), got, expected)
}

/// Render one report line. Shared so a streamed line and a replayed line are
/// byte-identical.
fn format_row(id: &str, verdict: Verdict, got: &[String], expected: usize) -> String {
    format!(
        "{}\t{id}\t{}/{} cell(s): {}",
        verdict.label(),
        got.len(),
        expected,
        if got.is_empty() { "no rows".to_string() } else { got.join(",") }
    )
}

/// How an id reproduces: alone, only with its category around it, or not at all.
///
/// ⚠️ THIS EXISTS BECAUSE A LONE-TEST PROBE CAN BISECT A PHANTOM. The owner's
/// recorded case: THIRTEEN tests failed only under full-suite concurrency. Probed
/// one at a time they pass, so an automatic bisect would walk the whole range
/// finding nothing bad, and either report "no first-bad commit" or -- worse, if any
/// step flaked -- converge confidently on an innocent commit.
///
/// The node runs a whole CATEGORY together; this driver's default `--test` probe
/// runs ONE CELL ALONE. Those are different experiments, so which one reproduces is
/// a fact that has to be established per id BEFORE its bisect is trusted, not
/// assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repro {
    /// Fails alone AND in category. Safe to bisect with the cheap `--test` probe.
    Standalone,
    /// Passes alone, fails in category. MUST be bisected with a `--category` probe.
    SuiteOnly,
    /// Fails alone, passes in category. The probe is more sensitive than the node.
    AloneOnly,
    /// Passes both ways -- reproduction is ABSENT. There is nothing here to bisect.
    Absent,
    /// At least one of the two observations was not a measurement.
    Unmeasured,
}

impl Repro {
    fn label(self) -> &'static str {
        match self {
            Repro::Standalone => "STANDALONE",
            Repro::SuiteOnly => "SUITE-ONLY",
            Repro::AloneOnly => "ALONE-ONLY",
            Repro::Absent => "NO-REPRO",
            Repro::Unmeasured => "UNMEASURED",
        }
    }

    /// What a bisect of this id must use, in words the operator can act on.
    fn guidance(self, category: &str) -> String {
        match self {
            Repro::Standalone => "bisect with --test (cheap probe is faithful)".to_string(),
            Repro::SuiteOnly => format!(
                "⚠️ bisect with --category {category}; a --test probe finds NOTHING here"
            ),
            Repro::AloneOnly => {
                "⚠️ DO NOT BISECT YET: fails alone but passes in category, so the probe is \
                 measuring something the node does not"
                    .to_string()
            }
            Repro::Absent => {
                "nothing to bisect: does not reproduce either way at this commit".to_string()
            }
            Repro::Unmeasured => {
                "⚠️ NOT MEASURED: fix the probe input before bisecting anything".to_string()
            }
        }
    }
}

/// Classify one id from its two observations. Pure, so the precedence is testable.
///
/// ⚠️ UNMEASURED OUTRANKS EVERYTHING, for the same reason `Error` outranks `Fail` in
/// [`fold`]: "I could not look" must never be folded into a statement about the
/// product. An `Absent` produced by two failed measurements would read as "nothing to
/// bisect" and quietly retire a real failing test.
fn classify_repro(alone: Verdict, in_category: Verdict) -> Repro {
    if !alone.measured() || !in_category.measured() {
        return Repro::Unmeasured;
    }
    match (alone == Verdict::Fail, in_category == Verdict::Fail) {
        (true, true) => Repro::Standalone,
        (false, true) => Repro::SuiteOnly,
        (true, false) => Repro::AloneOnly,
        (false, false) => Repro::Absent,
    }
}

/// One round of the batched bisection: which ids are now localised, and which
/// still need a narrower search.
///
/// ⚠️ THIS IS THE PART THAT MAKES BATCHING LOSSLESS, and it is pure so that the
/// reasoning is testable without spending hours of real probes.
///
/// The combined probe uses OR semantics, so `git`'s one-bit answer locates the first
/// commit where ANY pending id fails. That commit is the first-bad for exactly the
/// ids that FAIL AT IT -- and says nothing about the others except that their own
/// first-bad must lie strictly LATER. So each round:
///
///   * resolves every id failing at the boundary, and
///   * moves the lower bound to the boundary for everything still pending.
///
/// Each round therefore resolves at least one id and strictly shrinks the range, so
/// the loop terminates in at most one round per id.
///
/// ⚠️ AN EMPTY CULPRIT SET IS A CONTRADICTION, NOT AN EMPTY ROUND. The boundary was
/// found because the probe said FAIL there; if re-probing at that same commit names
/// nobody, the probe is not reproducible (a flake, or a concurrency-sensitive id
/// being run at the wrong concurrency -- see [`Repro`]). Returning "no culprits" and
/// looping would spin forever on the same boundary, so the caller must stop and say
/// so. This function reports it rather than papering over it.
fn round_outcome(
    pending: &BTreeSet<String>,
    verdicts_at_boundary: &BTreeMap<String, Verdict>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let culprits: BTreeSet<String> = pending
        .iter()
        .filter(|id| verdicts_at_boundary.get(*id) == Some(&Verdict::Fail))
        .cloned()
        .collect();
    let still_pending: BTreeSet<String> =
        pending.difference(&culprits).cloned().collect();
    (culprits, still_pending)
}

/// The exit code for a whole probe, from its per-id verdicts.
fn probe_rc(verdicts: &[Verdict]) -> i32 {
    if verdicts.iter().any(|v| !v.measured()) {
        return RC_NOT_MEASURED;
    }
    if verdicts.contains(&Verdict::Fail) {
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

/// What one `plan` invocation tells us about the requested ids.
#[derive(Default)]
struct Plan {
    /// Cells per REQUESTED id. Zero means UNSELECTED -- never a pass.
    counts: BTreeMap<String, usize>,
    /// Which category each requested id lives in, for the category probe.
    category_of: BTreeMap<String, String>,
    /// Cells per category across the whole lane, so a report can say how many
    /// siblings a suite-only reproduction actually needs around it.
    category_size: BTreeMap<String, usize>,
}

/// Ask `plan` which cells each requested id expands to.
///
/// Returns a map from id to cell count. An id absent from `plan`'s output maps to
/// zero, which the caller turns into `UNSELECTED` -- see the module note: `plan`
/// will not do that for us.
fn enumerate(root: &Path, harness: &Path, lane: &str, ids: &BTreeSet<String>) -> Plan {
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
    let mut category_of: BTreeMap<String, String> = BTreeMap::new();
    let mut category_size: BTreeMap<String, usize> = BTreeMap::new();
    for row in rows {
        // Category and whole-lane category sizes are collected on the SAME PASS,
        // because `--repro-check` has to run an id's WHOLE category and therefore
        // needs to know which category it is in and how big that is. A second `plan`
        // invocation to learn it could disagree with this one.
        let cat = row.get("category").and_then(|c| c.as_str());
        let id = row
            .get("id")
            .and_then(|i| i.get("test"))
            .or_else(|| row.get("test"))
            .and_then(|t| t.as_str());
        if let (Some(id), Some(cat)) = (id, cat) {
            if counts.contains_key(id) {
                category_of.insert(id.to_string(), cat.to_string());
            }
            *category_size.entry(cat.to_string()).or_insert(0) += 1;
        }
        if let Some(id) = id {
            if let Some(slot) = counts.get_mut(id) {
                *slot += 1;
            }
        }
    }
    Plan { counts, category_of, category_size }
}

/// Run ONE `test-harness` selection and return per-id cell outcomes, attributed by
/// each row's own `test` field.
///
/// ⚠️ ATTRIBUTION IS BY THE ROW, NOT BY WHAT WE ASKED FOR. When the selection is a
/// whole `--category`, the results file holds rows for every id in that category, so
/// pushing every row's outcome onto the requested id -- which is sound for a
/// single-id `--test` run -- would report another test's failure as this one's. That
/// is a wrong bisect. Keying on `row["test"]` is correct for both selections, and
/// `interested` bounds what we keep so a category run does not silently widen the
/// report.
fn run_selection(
    root: &Path,
    harness: &Path,
    lane: &str,
    jobs: &str,
    select_flag: &str,
    select_value: &str,
    interested: &BTreeSet<String>,
    tag: &str,
) -> BTreeMap<String, Vec<String>> {
    let mut outcomes: BTreeMap<String, Vec<String>> =
        interested.iter().map(|i| (i.clone(), vec![])).collect();
    let results = root.join(format!(
        "target/bisect-probe-{}-{}.jsonl",
        tag,
        select_value.replace('/', "-")
    ));
    let _ = std::fs::remove_file(&results);
    let status = Command::new(harness)
        .current_dir(root)
        .args(["run", "--lane", lane, select_flag, select_value, "--prebuilt", "--jobs", jobs])
        .arg("--results")
        .arg(&results)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if status.is_err() {
        for id in interested {
            if let Some(v) = outcomes.get_mut(id) {
                v.push("ERROR".to_string());
            }
        }
        return outcomes;
    }
    // ⚠️ READ THE ROWS, NOT THE EXIT CODE. `run` exits non-zero for a failed cell AND
    // for a harness error, and the per-row `outcome` is the only thing that separates
    // them. An unreadable results file leaves the vectors short, which folds to
    // MISSING rather than to PASS.
    if let Ok(text) = std::fs::read_to_string(&results) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(row) = serde_json::from_str::<serde_json::Value>(line) {
                let rid = row.get("test").and_then(|t| t.as_str()).unwrap_or_default();
                if let Some(o) = row.get("outcome").and_then(|o| o.as_str()) {
                    if let Some(v) = outcomes.get_mut(rid) {
                        v.push(o.to_string());
                    }
                }
            }
        }
    }
    outcomes
}

/// Establish, per id, whether it reproduces ALONE or only with its category around
/// it. See [`Repro`] for why this must precede any bisect.
fn repro_check(
    root: &Path,
    harness: &Path,
    lane: &str,
    jobs: &str,
    ids: &BTreeSet<String>,
    counts: &BTreeMap<String, usize>,
    categories: &BTreeMap<String, String>,
    category_sizes: &BTreeMap<String, usize>,
) -> i32 {
    use std::io::Write;
    let mut worst = RC_OK;
    for id in ids {
        let unknown = String::from("<unknown>");
        let category = categories.get(id).unwrap_or(&unknown).clone();

        let alone_out =
            run_selection(root, harness, lane, jobs, "--test", id, &BTreeSet::from([id.clone()]), "alone");
        let (_, alone, _, _) = report_row(id, counts, &alone_out);

        // The whole category, exactly as the node runs it, then read back only this
        // id's rows. `expected` stays the id's own cell count, so a short row set is
        // still MISSING rather than a pass.
        let cat_out = run_selection(
            root,
            harness,
            lane,
            jobs,
            "--category",
            &category,
            &BTreeSet::from([id.clone()]),
            "incat",
        );
        let (_, in_cat, _, _) = report_row(id, counts, &cat_out);

        let verdict = classify_repro(alone, in_cat);
        let siblings = category_sizes.get(&category).copied().unwrap_or(0);
        println!(
            "{}\t{id}\talone={} in-category={} ({} cell(s) in {}): {}",
            verdict.label(),
            alone.label(),
            in_cat.label(),
            siblings,
            category,
            verdict.guidance(&category)
        );
        let _ = std::io::stdout().flush();

        worst = match verdict {
            Repro::Unmeasured | Repro::AloneOnly => RC_NOT_MEASURED,
            Repro::Standalone | Repro::SuiteOnly if worst == RC_OK => RC_FAIL,
            _ => worst,
        };
    }
    eprintln!(
        "bisect-probe: repro-check complete over {} id(s). ⚠️ A SUITE-ONLY id bisected \
         with a --test probe finds nothing; use --category for those.",
        ids.len()
    );
    worst
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} exited {:?}: {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commits that could be the first bad one: `(good, bad]`, oldest first.
///
/// ⚠️ FIRST-PARENT, because `main` here is a squash-merge history: every landing
/// rewrites the sha and appears as one first-parent commit. Walking into merge
/// parents would offer commits that were never a state of `main` and can neither be
/// built nor blamed.
fn commit_range(root: &Path, good: &str, bad: &str) -> Result<Vec<String>, String> {
    let text = git(root, &["rev-list", "--first-parent", "--reverse", &format!("{good}..{bad}")])?;
    Ok(text.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())
}

/// Check out one commit, run the build, and probe every pending id there.
///
/// Returns one verdict per pending id. A build failure makes every id `Error` at
/// this commit -- NOT `Pass` and NOT `Fail` -- so the binary search treats the
/// commit as unmeasured rather than silently choosing a direction from it.
#[allow(clippy::too_many_arguments)]
fn probe_at(
    root: &Path,
    harness: &Path,
    lane: &str,
    jobs: &str,
    build: &str,
    commit: &str,
    pending: &BTreeSet<String>,
    counts: &BTreeMap<String, usize>,
) -> BTreeMap<String, Verdict> {
    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    if let Err(e) = git(root, &["checkout", "--detach", "--force", commit]) {
        eprintln!("bisect-probe: cannot check out {commit}: {e}");
        for id in pending {
            verdicts.insert(id.clone(), Verdict::Error);
        }
        return verdicts;
    }
    let built = Command::new("sh")
        .current_dir(root)
        .arg("-c")
        .arg(build)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !built {
        // ⚠️ A BUILD FAILURE IS NOT A PRODUCT VERDICT. Reading it as "bad" is how a
        // bisect blames the commit that broke the build for a test failure that
        // started somewhere else entirely.
        eprintln!("bisect-probe: build FAILED at {commit}; every id is ERROR here, not bad");
        for id in pending {
            verdicts.insert(id.clone(), Verdict::Error);
        }
        return verdicts;
    }
    for id in pending {
        let out = run_selection(
            root,
            harness,
            lane,
            jobs,
            "--test",
            id,
            &BTreeSet::from([id.clone()]),
            "bisect",
        );
        let (_, v, _, _) = report_row(id, counts, &out);
        verdicts.insert(id.clone(), v);
    }
    verdicts
}

/// Batched bisection with automatic follow-ups: every id gets its OWN first-bad
/// commit, not just the earliest one.
///
/// ⚠️ THIS IS WHAT MAKES BATCHING LOSSLESS. A single combined bisect answers only
/// "where did the FIRST of these break", and the other ids ride along for free but
/// come back unlocalised. After each boundary this resolves the ids that fail AT it,
/// drops them, and re-bisects the REMAINDER over `(boundary, bad]` -- see
/// [`round_outcome`] for the argument that this terminates and is correct.
#[allow(clippy::too_many_arguments)]
fn bisect(
    root: &Path,
    harness: &Path,
    lane: &str,
    jobs: &str,
    build: &str,
    good: &str,
    bad: &str,
    ids: &BTreeSet<String>,
    counts: &BTreeMap<String, usize>,
    categories: &BTreeMap<String, String>,
) -> i32 {
    use std::io::Write;

    // ⚠️ REFUSE TO BISECT ANYTHING THAT DOES NOT REPRODUCE STANDALONE, and establish
    // that AT `bad`, where the failure is known to exist. This is the guard that
    // stops the whole apparatus from confidently bisecting a phantom: a suite-only id
    // probed with `--test` passes at every commit, so the search would report "no
    // first-bad commit in range" for a test that is genuinely broken.
    eprintln!("bisect-probe: establishing reproduction at bad={bad} before searching");
    if let Err(e) = git(root, &["checkout", "--detach", "--force", bad]) {
        eprintln!("bisect-probe: cannot check out bad={bad}: {e}");
        return RC_UNUSABLE;
    }
    if !Command::new("sh").current_dir(root).arg("-c").arg(build).status().map(|s| s.success()).unwrap_or(false) {
        eprintln!("bisect-probe: build FAILED at bad={bad}; nothing can be established");
        return RC_UNUSABLE;
    }
    let mut pending: BTreeSet<String> = BTreeSet::new();
    for id in ids {
        let unknown = String::from("<unknown>");
        let cat = categories.get(id).unwrap_or(&unknown);
        let alone = run_selection(root, harness, lane, jobs, "--test", id, &BTreeSet::from([id.clone()]), "alone");
        let (_, a, _, _) = report_row(id, counts, &alone);
        let incat = run_selection(root, harness, lane, jobs, "--category", cat, &BTreeSet::from([id.clone()]), "incat");
        let (_, c, _, _) = report_row(id, counts, &incat);
        let repro = classify_repro(a, c);
        println!("{}\t{id}\tat bad={bad}: {}", repro.label(), repro.guidance(cat));
        let _ = std::io::stdout().flush();
        if repro == Repro::Standalone {
            pending.insert(id.clone());
        }
    }
    if pending.is_empty() {
        eprintln!(
            "bisect-probe: REFUSED: none of the {} requested id(s) reproduces STANDALONE at \
             bad={bad}, so a --test bisect would search for something it cannot observe. \
             Nothing was bisected.",
            ids.len()
        );
        return RC_NOT_MEASURED;
    }
    if pending.len() < ids.len() {
        eprintln!(
            "bisect-probe: ⚠️ bisecting only the {} STANDALONE id(s) of {} requested; the rest \
             are named above and need a --category probe this driver does not yet bisect with.",
            pending.len(),
            ids.len()
        );
    }

    let candidates = match commit_range(root, good, bad) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bisect-probe: {e}");
            return RC_UNUSABLE;
        }
    };
    if candidates.is_empty() {
        eprintln!("bisect-probe: REFUSED: {good}..{bad} contains no first-parent commits");
        return RC_UNUSABLE;
    }
    eprintln!("bisect-probe: {} candidate commit(s) in {good}..{bad}", candidates.len());

    let mut cache: BTreeMap<String, BTreeMap<String, Verdict>> = BTreeMap::new();
    let mut resolved: Vec<(String, String)> = Vec::new();
    let mut lo = 0usize;
    let mut rc = RC_OK;

    while !pending.is_empty() {
        // Binary search for the FIRST index in [lo, len) where any pending id fails.
        // Invariant: everything below `left` is known clean for `pending`.
        let (mut left, mut right) = (lo, candidates.len());
        let mut found: Option<usize> = None;
        while left < right {
            let mid = left + (right - left) / 2;
            let commit = candidates[mid].clone();
            let verdicts = cache.entry(commit.clone()).or_insert_with(|| {
                eprintln!(
                    "bisect-probe: probing {} ({} id(s) pending, step {}/{})",
                    &commit[..12.min(commit.len())],
                    pending.len(),
                    mid + 1,
                    candidates.len()
                );
                probe_at(root, harness, lane, jobs, build, &commit, &pending, counts)
            });
            let any_fail = pending.iter().any(|id| verdicts.get(id) == Some(&Verdict::Fail));
            if any_fail {
                found = Some(mid);
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        let Some(idx) = found else {
            eprintln!(
                "bisect-probe: NOT-LOCALISED: no commit in the remaining range makes any of {} \
                 pending id(s) fail. They are listed below as unresolved.",
                pending.len()
            );
            for id in &pending {
                println!("NOT-LOCALISED\t{id}\tno failing commit found in {good}..{bad}");
            }
            rc = RC_NOT_MEASURED;
            break;
        };
        let boundary = candidates[idx].clone();
        let verdicts = cache.get(&boundary).cloned().unwrap_or_default();
        let (culprits, still) = round_outcome(&pending, &verdicts);
        if culprits.is_empty() {
            // See round_outcome: this is a contradiction, and looping would spin.
            eprintln!(
                "bisect-probe: CONTRADICTION at {boundary}: the search reached it because a probe \
                 FAILED there, but a re-read names no failing id. The probe is not reproducible \
                 at this commit -- suspect a flake or a concurrency-sensitive id. STOPPING."
            );
            for id in &pending {
                println!("UNRESOLVED\t{id}\tcontradictory probe at {boundary}");
            }
            rc = RC_NOT_MEASURED;
            break;
        }
        for id in &culprits {
            println!("FIRST-BAD\t{id}\t{boundary}");
            let _ = std::io::stdout().flush();
            resolved.push((id.clone(), boundary.clone()));
        }
        if rc == RC_OK {
            rc = RC_FAIL;
        }
        pending = still;
        // Everything at or before the boundary is clean for whoever is left: they
        // passed there, so their own first-bad is strictly later.
        lo = idx + 1;
        if !pending.is_empty() && lo >= candidates.len() {
            for id in &pending {
                println!("NOT-LOCALISED\t{id}\tno failing commit after {boundary}");
            }
            rc = RC_NOT_MEASURED;
            break;
        }
    }

    eprintln!(
        "bisect-probe: {} id(s) localised, {} unresolved, {} distinct commit(s) probed",
        resolved.len(),
        pending.len(),
        cache.len()
    );
    rc
}

fn main() {
    rust_script_prelude::init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--self-test") {
        std::process::exit(self_test());
    }
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "usage: bisect-probe.rs [--lane LANE] [--jobs N] [--repro-check] ID [ID...]\n\
             \n\
             Runs a list of e2e test ids as one probe and prints one verdict per\n\
             REQUESTED id, STREAMED as each id finishes. Exit 0 only if every id\n\
             passed; 1 if a cell FAILED; 3 if any id was not measured\n\
             (UNSELECTED / MISSING / ERROR); 2 if the probe itself could not run.\n\
             \n\
             --repro-check  Do not probe for a bisect. Instead run each id ALONE and\n\
             then run its WHOLE CATEGORY, and report which of the two reproduces.\n\
             ⚠️ RUN THIS BEFORE TRUSTING ANY BISECT. A test that fails only with its\n\
             category around it passes when probed alone, so a --test bisect walks the\n\
             whole range finding nothing -- or converges on an innocent commit.\n\
             \n\
             --jobs defaults to 1, which is what eleven of the thirteen e2e nodes use\n\
             (and test-harness's own default). Raise it only to match a node that\n\
             genuinely runs at 8.\n\
             \n\
             An id that matches no cell is UNSELECTED and exits 3. It is NEVER a pass.\n\
             `test-harness plan` refuses an id that is in no manifest, but this probe\n\
             asks for the whole lane WITHOUT the ids, so that guard never sees them --\n\
             and a real id with no cells in this lane is a legitimate empty answer to\n\
             plan while still being unusable here."
        );
        std::process::exit(RC_UNUSABLE);
    }

    let mut lane = "portable".to_string();
    // ⚠️ THE DEFAULT IS 1 BECAUSE THE NODES RUN AT 1, AND A BISECT MUST REPRODUCE THE
    // INVOCATION THAT OBSERVED THE FAILURE. Measured 2026-08-26 against
    // ci/dag/portable.json: of the thirteen `e2e.manifest_*` nodes, ELEVEN pass no
    // `--jobs` at all -- and `test-harness` itself defaults to 1
    // (test-harness.rs, `ScheduledWorkerCapacity::new(args.jobs.unwrap_or(1))`) --
    // `e2e.manifest_system_utils` pins `--jobs 1` explicitly, and only
    // `e2e.manifest_backend_parity_c` and `e2e.manifest_c_programs` use `--jobs 8`.
    //
    // This driver defaulted to 8. An id expands to up to 3 cells (median 1, max 3),
    // so for a multi-cell id the probe was running its cells CONCURRENTLY while the
    // node ran them one at a time. A concurrency-sensitive failure measured at a
    // concurrency the node never uses is a bisect on a different phenomenon.
    //
    // Override with `--jobs` when bisecting an id from one of the two categories that
    // genuinely runs at 8; the honest default is the one eleven of thirteen use.
    let mut jobs = "1".to_string();
    let mut ids: BTreeSet<String> = BTreeSet::new();
    let mut repro_only = false;
    let mut good: Option<String> = None;
    let mut bad: Option<String> = None;
    // The e2e cells need a hermit binary and their guest programs; `--prebuilt` means
    // this driver must supply them, and it must be the SAME build the node uses or
    // the probe measures a different artifact.
    let mut build =
        "cargo build -p hermit-manifest-plan --bins && cargo build --bin hermit".to_string();
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--lane" => lane = it.next().unwrap_or_else(|| fail("--lane needs a value")).clone(),
            "--jobs" => jobs = it.next().unwrap_or_else(|| fail("--jobs needs a value")).clone(),
            "--repro-check" => repro_only = true,
            "--good" => good = Some(it.next().unwrap_or_else(|| fail("--good needs a value")).clone()),
            "--bad" => bad = Some(it.next().unwrap_or_else(|| fail("--bad needs a value")).clone()),
            "--build" => build = it.next().unwrap_or_else(|| fail("--build needs a value")).clone(),
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
    let plan = enumerate(&root, &harness, &lane, &ids);
    let counts = plan.counts.clone();
    let unselected: Vec<&String> = counts.iter().filter(|(_, n)| **n == 0).map(|(k, _)| k).collect();
    if !unselected.is_empty() {
        for id in &unselected {
            println!("UNSELECTED\t{id}\t0 cells -- matched nothing in lane {lane}");
        }
        eprintln!(
            "bisect-probe: REFUSED: {} of {} requested id(s) matched no cell: {}. \
             An unmatched id is NOT a pass. `test-harness plan` refuses an id that is in no \
             manifest, but this probe requests the whole lane without the ids, so that guard \
             never sees them -- and a real id with no cells here is legitimately empty to plan \
             yet unusable for a bisection. Nothing was run; fix the list and re-probe.",
            unselected.len(),
            ids.len(),
            unselected.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        );
        std::process::exit(RC_NOT_MEASURED);
    }

    if let (Some(good), Some(bad)) = (&good, &bad) {
        // ⚠️ THIS MODE CHECKS OUT COMMITS, so it must never run where someone else's
        // work or the shared primary would be clobbered. Both guards are refusals,
        // not warnings: a bisect that destroys uncommitted work to answer a question
        // is not a trade anyone agreed to.
        match git(&root, &["status", "--porcelain"]) {
            Ok(s) if !s.trim().is_empty() => fail(
                "REFUSED: the working tree is dirty and this mode checks out commits. \
                 Commit, stash, or use a dedicated worktree.",
            ),
            Err(e) => fail(&format!("cannot read git status: {e}")),
            _ => {}
        }
        if root == Path::new("/home/newton/work/dev-hermit/hermit") {
            fail(
                "REFUSED: this is the shared primary hermit checkout. Bisecting here would \
                 move it under every other agent. Allocate a worktree slot and run there.",
            );
        }
        std::process::exit(bisect(
            &root, &harness, &lane, &jobs, &build, good, bad, &ids, &counts, &plan.category_of,
        ));
    }
    if good.is_some() != bad.is_some() {
        fail("--good and --bad must be given together; one alone names no range");
    }

    if repro_only {
        std::process::exit(repro_check(
            &root,
            &harness,
            &lane,
            &jobs,
            &ids,
            &counts,
            &plan.category_of,
            &plan.category_size,
        ));
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
            if let Some(v) = outcomes.get_mut(id) {
                v.push("ERROR".to_string());
            }
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
                        if let Some(v) = outcomes.get_mut(id) {
                            v.push(o.to_string());
                        }
                    }
                }
            }
        }

        // ⚠️ PRINT THIS ID NOW, AND FLUSH. A batched sweep with follow-up bisections
        // runs for hours; output withheld until the end is output nobody sees in time
        // to act on. Measured basis for the batching in the header: ~3.5 s per cell
        // against a 36 s build, so a ten-id probe is minutes and a full bisect over it
        // is hours.
        //
        // The flush is not decoration. stdout is a PIPE whenever this runs under
        // `git bisect run` or through `tee`, and a piped stdout is block-buffered, so
        // without an explicit flush every line would sit in the buffer until exit --
        // which is exactly the behaviour this change exists to remove. Rust flushes
        // on normal exit but this driver ends in `std::process::exit`, which runs no
        // destructors, so unflushed bytes would be lost outright.
        let (_, verdict, got, expected) = report_row(id, &counts, &outcomes);
        println!("{}", format_row(id, verdict, &got, expected));
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    // Re-folded from the same map through the same function, so these verdicts cannot
    // disagree with the lines already printed above.
    let report = report_rows(&ids, &counts, &outcomes);
    let verdicts: Vec<Verdict> = report.iter().map(|(_, v, _, _)| *v).collect();

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

    // ── The standalone-vs-suite classifier ──────────────────────────────────────
    // ⚠️ THE CASE THE OWNER NAMED: thirteen tests that failed only under full-suite
    // concurrency. A lone-test probe passes, the category probe fails, and the id
    // MUST NOT be bisected with --test.
    if classify_repro(Verdict::Pass, Verdict::Fail) != Repro::SuiteOnly {
        bad.push("passes alone + fails in category must be SUITE-ONLY".into());
    }
    if classify_repro(Verdict::Fail, Verdict::Fail) != Repro::Standalone {
        bad.push("fails both ways is STANDALONE and safe for a --test probe".into());
    }
    if classify_repro(Verdict::Fail, Verdict::Pass) != Repro::AloneOnly {
        bad.push("fails alone + passes in category must be ALONE-ONLY".into());
    }
    if classify_repro(Verdict::Pass, Verdict::Pass) != Repro::Absent {
        bad.push("passes both ways is NO-REPRO -- there is nothing to bisect".into());
    }
    // Unmeasured outranks everything, in BOTH positions.
    if classify_repro(Verdict::Unselected, Verdict::Fail) != Repro::Unmeasured {
        bad.push("an unmeasured standalone observation must dominate".into());
    }
    if classify_repro(Verdict::Fail, Verdict::Missing) != Repro::Unmeasured {
        bad.push("an unmeasured in-category observation must dominate".into());
    }
    // ⚠️ THE CONTROL. Without it, a classifier returning SuiteOnly for everything
    // would satisfy the case that matters most and look correct.
    if classify_repro(Verdict::Pass, Verdict::Pass) == Repro::SuiteOnly {
        bad.push("control: a clean id must NOT be classified SUITE-ONLY".into());
    }
    if Repro::SuiteOnly.guidance("c-programs") == Repro::Standalone.guidance("c-programs") {
        bad.push("control: SUITE-ONLY and STANDALONE must not give the same guidance".into());
    }

    // ── The batched-bisection round planner ─────────────────────────────────────
    let pend = |v: &[&str]| -> BTreeSet<String> { v.iter().map(|s| s.to_string()).collect() };
    let at = |v: &[(&str, Verdict)]| -> BTreeMap<String, Verdict> {
        v.iter().map(|(k, d)| (k.to_string(), *d)).collect()
    };

    // The ordinary round: the boundary belongs to B alone; A and C keep searching.
    let (culprits, rest) = round_outcome(
        &pend(&["A", "B", "C"]),
        &at(&[("A", Verdict::Pass), ("B", Verdict::Fail), ("C", Verdict::Pass)]),
    );
    if culprits != pend(&["B"]) {
        bad.push("only the ids FAILING at the boundary are localised there".into());
    }
    if rest != pend(&["A", "C"]) {
        bad.push("ids passing at the boundary must stay pending".into());
    }

    // Two ids can share one boundary; both are resolved in the same round.
    let (culprits, rest) = round_outcome(
        &pend(&["A", "B"]),
        &at(&[("A", Verdict::Fail), ("B", Verdict::Fail)]),
    );
    if culprits != pend(&["A", "B"]) || !rest.is_empty() {
        bad.push("ids sharing a boundary are resolved together, leaving nothing pending".into());
    }

    // ⚠️ THE CONTRADICTION THAT MUST NOT LOOP. The boundary was found because the
    // probe said FAIL there; if nobody fails on re-probe the probe is not
    // reproducible. An empty culprit set with a non-empty pending set is how the
    // caller detects that and STOPS, rather than re-bisecting the same range forever.
    let (culprits, rest) = round_outcome(
        &pend(&["A", "B"]),
        &at(&[("A", Verdict::Pass), ("B", Verdict::Pass)]),
    );
    if !culprits.is_empty() {
        bad.push("no id failing at the boundary must yield NO culprits".into());
    }
    if rest != pend(&["A", "B"]) {
        bad.push("a contradictory round must leave pending untouched so the caller can stop".into());
    }

    // ⚠️ AN ID NOT IN `pending` MUST NEVER BE RESOLVED, even if it fails at the
    // boundary. Already-localised ids keep their earlier, EARLIER first-bad; letting
    // a later round overwrite them would silently move a resolved answer.
    let (culprits, _) = round_outcome(
        &pend(&["A"]),
        &at(&[("A", Verdict::Fail), ("ALREADY", Verdict::Fail)]),
    );
    if culprits != pend(&["A"]) {
        bad.push("round_outcome must never resolve an id outside `pending`".into());
    }

    // Progress: every non-contradictory round strictly shrinks `pending`, which is
    // what bounds the loop at one round per id.
    let start = pend(&["A", "B", "C"]);
    let (c, r) = round_outcome(
        &start,
        &at(&[("A", Verdict::Fail), ("B", Verdict::Pass), ("C", Verdict::Pass)]),
    );
    if !c.is_empty() && r.len() >= start.len() {
        bad.push("a round that resolves anything must shrink the pending set".into());
    }

    for b in &bad {
        eprintln!("bisect-probe self-test: FAIL -- {b}");
    }
    if !bad.is_empty() {
        return 1;
    }
    println!(
        "PASS: bisect-probe folds per-cell outcomes into one verdict per REQUESTED id, \
         reports an unmatched id as UNSELECTED rather than a pass, keeps \
         not-measured (rc=3) distinct from failed (rc=1), separates a standalone \
         reproduction from a suite-only one, and localises exactly the ids that fail \
         at a bisection boundary"
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
