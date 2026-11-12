[hermit2, museops, tbh-1005, devbig014, role=impl]
Owner: museops (muse, devbig014) since 2026-11-08T05:46:50Z

Background

Phase 2 of the owner P0 count-pin replacement (beads dh-u50k6 / dh-qi4tu). Phase 1 landed as 9d3dd84b (#5277): ci/census-diff + ci/test_census_diff.py + lint-check-census-diff. This PR wires the census into the counted validate path in report-only mode and deletes no pins, per the dh-qi4tu sequence (gate first report-only, then enforce, then delete).

Wiring: ci/run-nextest-counted.sh now, after retaining each counted node's .listed-ids population, invokes the new ci/census-check.sh, which diffs that listed population against a base (last landed main) population with ci/census-diff. The queue adapter supplies the base via HERMIT_CENSUS_BASE_LISTED_IDS, drop records (Test-Dropped: <id> -- <reason> from git log base..head) via HERMIT_CENSUS_DROPPED_FILE, and HERMIT_CENSUS_REPORT_ONLY=1 downgrades a disappearance to a report. With a base supplied, an unrecorded disappearance fails the node naming the IDs; additions pass; totals are reported, never asserted.

Why no pins are deleted in this PR: the counted wrapper already supersedes NEXTEST_EXPECTED_EXECUTED with its listed-derived count, but the cross-run silent-drop guarantee (a test deleted from source vanishes from both the listed and the outcome populations of a single run) only holds once the adapter caches and supplies the base population. Deleting NEXTEST_EXPECTED_COUNTS (55 entries) or the scripts/validate.rs:20689 all-owner pin (now 2991) before that would weaken the guarantee this work must not weaken. The validate.rs pin is likewise kept: a manifest-cell census needs a base ManifestSet::select ID set from the last landed main checkout, which only the adapter/queue side can provide; the in-repo derived identity checks (owner union == all-owner set, plan-length equality) already stay.

Pin-deletion map (deferred, not deleted here):
- NEXTEST_EXPECTED_COUNTS 55 entries + 50 materialised NEXTEST_EXPECTED_EXECUTED in ci/dag/validate.json -> replaced by adapter-supplied base census above; delete in the enforce PR once HERMIT_CENSUS_BASE_LISTED_IDS is supplied in validate and one queue day of report-only output is clean.
- scripts/validate.rs all-owner 2991 -> replaced by a manifest-cell census (candidate ManifestSet::select IDs vs base IDs, same omission-record rule); needs base select set from the adapter side.
- Remaining families (parity 423/215/4685, catalogue) untouched; ETA after nextest enforce lands.

Delta on stamped head e9b48a1 (runner fix only; phase-2 scope unchanged, no pin deletions): Makefile lint-check-census-diff ran `python3 ./ci/test_census_diff.py`, but that file only defined pytest-style test_* functions with no __main__ runner, so the target ran ZERO assertions and always exited 0. Both ci/test_census_diff.py and sibling ci/test_recount_pins.py (same bug) now carry a stdlib-only __main__ runner: discovers every test_* function, gives each its own temp dir (tmp_path equivalent), prints PASS/FAIL per test, exits non-zero on failure, and fails with a named "zero tests collected" error (exit 2) if none are found. No literal test-count pin. pytest 9.1.1 exists in this dev env but is declared nowhere in the repo/validate env, so the runner adds no dependency; Makefile lines unchanged.

Test plan (new head 7efc5b198849be29b9c31946973e484613cd5256):
- make lint-check-census-diff: 17/17 tests passed, exit 0. make lint-check-recount-pins: 2/2 passed, exit 0.
- Red demo: mutated test_skip_with_reason_passes_and_is_reported to assert returncode == 999 -> target printed "FAIL test_skip_with_reason_passes_and_is_reported" and exited non-zero (make Error 1); restored -> green. Zero-collect demo: copy with test_* renamed -> "census-diff test runner: ERROR: zero tests collected", exit 2.
- python3 -m pytest -q ci/test_census_diff.py ci/test_recount_pins.py: 19 passed (collection unaffected).
- Prior head e9b48a1 evidence retained: python3 -m pytest ci/test_census_diff.py: 17 passed, including wired-path tests (a) listed removal fails naming it, (b) skip-with-reason passes/reported, (c) new test passes, (d) no-base report-only passes. make lint-check-checker-scheduling: pass.
- generate-validation-dag --check: not run in this fresh worktree (prebuilt rust-script requires the build.rust_scripts producer output); no DAG or Rust files changed in this PR, pre-flight will run it. scripts/validate.rs --self-test not run (refuses in a slot per phase 1); no validate.rs change in this PR.

Seat: ONE reviewer, musereviewa (stamped phase 1), goalpost: silent-drop guarantee must not weaken; totals reported never asserted. Not submitted to queue; handed to claudemq only after stamp.
