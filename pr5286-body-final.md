[hermit2, museops, tbh-1005, devbig014, role=impl]
Owner: museops (muse, devbig014) since 2026-11-08T16:30Z

## Background

Owner P0: replace exact-count pins with identity census. Phase 1 landed 9d3dd84b (ci/census-diff + tests); phase 2 landed 35ea84b0 (report-only wiring in counted nodes). This PR enforces the census in counted nodes and deletes the nextest pin family (pins kept colliding with legitimate growth: validate.rs 2990->2991, cli_kvm 157->159). Silent-drop guarantee does not weaken: a disappearance without a record still fails, now naming the test IDs; totals are reported, never asserted.

## Enforcement (node-requires-base, hermit-side)

Chosen route: **the counted node requires its base hermit-side** (musereviewa option (b)), over the adapter-supplies-base alternative (option (a) preflight evidence from outside hermit). Rationale: after pin deletion the census is the only disappearance guard, so it must fail closed in-tree, independent of adapter config this repo cannot audit. `ci/run-nextest-counted.sh` passes `--require-base` on every counted node: with no `HERMIT_CENSUS_BASE_LISTED_IDS` the node exits 75 by name (`census-check: no base population supplied and --require-base is in force`): missing base is could-not-run, so validate reports COULD_NOT_RUN (requeue), not FAILED; a supplied but stale/short/unstamped base still FAILS by name. `HERMIT_CENSUS_REPORT_ONLY` is retired in counted nodes (ignored, with a warning) so a stale adapter setting cannot silently downgrade enforcement; `--report-only` remains a manual `ci/census-check.sh` debugging aid. The only baseless opt-out is the explicit, named `HERMIT_CENSUS_ALLOW_NO_BASE=1`, used visibly by local/standalone runs and fixtures (wrapper self-test, `validate_nextest_fixture`, `qualify-loader-verification`); queue/validate counted nodes never set it. A supplied-but-missing/empty base, candidate, or dropped file is likewise refused by name.

## Freshness (stamped base provenance)

The producer stamps the base file's first line `# census-base: commit=<full sha> count=<id count>` (no sidecar). `ci/census-check.sh` enforces it: unstamped refused (`census-base-provenance`), count mismatch refused as truncated (`census-base-truncated`), wrong commit refused as stale (`census-base-stale`; expected = `HERMIT_CENSUS_BASE_COMMIT` else merge-base with `origin/main`). `ci/census-diff` treats the header as metadata, never an ID.

## Pin-deletion map (55 deleted -> census coverage)

Every entry below maps to the same coverage: that node's retained `.listed-ids` diffed against the base population by `ci/census-diff` via `ci/census-check.sh` (`ci/run-nextest-counted.sh`), failing by name on an unrecorded disappearance; additions pass. Each deleted `NEXTEST_EXPECTED_COUNTS` pin is replaced by this enforced census **with a required, provenance-stamped base** (`--require-base`): no base exits 75 (could-not-run/requeue, never a silent pass); a stale or short base fails the node by name, so no deleted pin maps to a report-only pass. No node family is left uncovered, so no pin is restored.

The 55 deleted pins cover the counted nextest nodes (super/test/privileged families and their on-host twins); the full per-node list is in the head diff of `ci/manifest-plan/src/validation_dag_static.rs`.

`ci/dag/validate.json` regenerated with the generator only; zero `NEXTEST_EXPECTED_EXECUTED` pins remain; gates refuse a reintroduced pin by name.

## Kept pin (PR-B)

`scripts/validate.rs` population pin (`all.len() == 2994`) is KEPT: replacing it needs a manifest-cell census (candidate vs base `ManifestSet::select` ID set with omission records), and that base set cannot be supplied hermit-side yet. Scoped to PR-B; parity/catalogue families (423/215/4685, 76/21/104/192) are phase 3b.

## Test plan

Head `3ba71be4d6e2022012a22a742bdb72273f630173` on main `2f505f036f5211a39773b12c5febbf6b85ffbfa0` (rebases of the approved stack; latest range-diff all `=`). Generated pair regenerated on the new base; the only hand hunks are the disclosed pin deletions, the scorecard derivation, the pinned-root census mount, and the fixture census-env scrub. Checks at this head: census 32/32, sync-cell-config --check rc0, DAG --check canonical. Census base: `refs/census-bases/2f505f036f5211a39773b12c5febbf6b85ffbfa0` = `8b6ae4a5fcc79f7be7f3efab6969e2e673b77765`.
Rebased clean onto 18d387b4; range-diff all =. E2E node proof was at the identical 698e2fea stack (carried by range-diff). Wrapper fix (post canary-3): run-in-pinned-root.sh mounts the census base at /census-base and rewrites the census env paths for the guest (wrapper test 31 OK; real container proof reads the stamped listed-ids). End-to-end at this head via validate-run + start_unit (a946ef1f) + wrapper: test.liteinst_stacks PASS, "census liteinst_stacks: passed=9 failed=0", FINAL_VALIDATE_STATUS: PASSED. Fixture scrub (post canary, layer 5): the authentic-nextest fixture inside check.script_unit_tests spawns real counted runs for synthetic fixture.* suites; the outer HERMIT_CENSUS_BASE_DIR leaked in and census-check refused a suite no base can hold. scripts/lib/validate_nextest_fixture.rs now unsets every HERMIT_CENSUS_BASE_* in the fixture command (keeps explicit ALLOW_NO_BASE=1). Red-first: the exact unit test failed with the real census env present, passes after (99s); full validate-run proof at c38e788b018: check.script_unit_tests PASS (1201 tests) + test.liteinst_stacks PASS, FINAL_VALIDATE_STATUS: PASSED. Census base for the base commit: dev-hermit `refs/census-bases/2f505f036f5211a39773b12c5febbf6b85ffbfa0` = `8b6ae4a5fcc79f7be7f3efab6969e2e673b77765`, 23 stamped suite files in the local donor cache.

- **Census delta (this head):** `PINNED_ROOT_FORWARDED_ENV` forwards `HERMIT_CENSUS_BASE_COMMIT`/`BASE_DIR`/`BASE_LISTED_IDS`/`DROPPED_FILE` (87/87 pinned steps; committed-DAG test); `census-check.sh` resolves the per-suite base from `HERMIT_CENSUS_BASE_DIR` under the same freshness refusals; case-selected runs are named, not compared. Adapter supply landed on dev-hermit main as `913017e3a09e`.

- `python3 ./ci/test_census_diff.py`: **32/32 passed** at this head (fresh/vanished/stale/truncated/missing-suite, case-selected naming, exit-code mapping missing-base=75 / stale=2 / vanished=1, duplicate-id and malformed-JSON named refusals).
- `bash ci/run-nextest-counted.sh --self-test`: PASS (counted wrapper, incl. require-base opt-out paths in fixtures).
- `cargo test -p hermit-manifest-plan --lib -- validation_dag nextest_build_selections known_gaps`: **78 passed**.
- `cargo run -q -p hermit-manifest-plan --bin generate-validation-dag -- --check`: canonical and fresh. `make lint-check-census-diff lint-check-recount-pins lint-check-checker-scheduling`: pass. `cargo fmt --all -- --check`: green.
- **Red demo (7559, retained):** `known_gaps` (21 tests) with stamped base rc 0; one deleted test without record rc 1 naming it MISSING; with `Test-Dropped:` record rc 0 `omitted=1`.


