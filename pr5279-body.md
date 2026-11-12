[hermit2, museops, tbh-1005, devbig014, role=impl]
Owner: museops (Muse Code, devbig014) since 2026-11-08T05:10:49Z

## Background

`scripts/validate.rs --keep-going` sets `HERMIT_NEXTEST_FAIL_FAST=0` and `HERMIT_E2E_FAIL_FAST=0` in every step's env map (`apply_keep_going_nextest_env`), but a pinned-root node only receives the env names baked into its committed command: `ci/manifest-plan/src/validation_dag.rs` builds `--env NAME` arguments from `PINNED_ROOT_FORWARDED_ENV`, and `ci/hermetic/run-in-pinned-root.sh` turns only those names into podman `--env` flags (no `--env-host`, no wildcard). Neither marker was in the list, so all 87 pinned-root steps (33 counted-nextest, 24 e2e) always failed fast inside the container, where an unset variable means fail-fast = true (`ci/nextest-timeout-config.rs` `fail_fast_from_env`). Tonight's evidence: PR #4668's pre-flight of `privileged-test.pmu_detcore_time_cases` reported "4/36 tests run: 3 passed, 1 failed" and "32/36 tests were not run due to test failure", so the queue saw one failing test and the other 32 never ran. The existing self-test `keep_going_keeps_families_and_marks_every_step` checked only the env map, never what the command inside the container sees.

## Plain Language Summary and Project Impact

This forwards both keep-going markers into pinned-root nodes so `--keep-going` actually runs past the first failure there, matching its behaviour outside the pinned root. `PINNED_ROOT_FORWARDED_ENV` gains `HERMIT_NEXTEST_FAIL_FAST` and `HERMIT_E2E_FAIL_FAST` (forwarded only when set, like the neighbouring entries), and `ci/dag/validate.json` is regenerated with `generate-validation-dag --write`: all 87 of 87 pinned-root steps now carry both `--env` entries. Tests now assert the forwarding itself, not just the env map: a committed-DAG test over every pinned-root step, a fake-podman execution test through the real `run-in-pinned-root.sh` proving both markers reach the payload with value `0` (and are absent when unset), and the `validate.rs` keep-going self-test extended to check pinned-root commands. A real two-test pinned-root fixture was judged disproportionate (it needs a built pinned image and test binaries); the execution test plus the existing `fail_fast_from_env` rendering tests in `ci/nextest-timeout-config.rs` cover the chain from forwarded `0` to `fail-fast = false` to nextest running the whole batch. Default behaviour is unchanged: without `--keep-going` neither marker is set, nothing is forwarded with a value, and pinned-root nodes still fail fast.

## Determinism

CI-infra only. No guest-visible behaviour, no scheduler or virtual-time change; forwarding is name-only and the value comes from the step env `validate.rs` already sets, so a run's determinism envelope is untouched.

## Linux Semantics

N/A — CI-only change; no syscall, signal, `/proc` or `/sys` behaviour is affected.

## Validation

At head `dc67ffd0de3afad82a85cf87f602e3f2f5065b79`:

- `cargo test -p hermit-manifest-plan --lib` — 732 passed, 7 failed; the 7 failures (`runner::tests::ptrace_given_*`, `manifest_metadata::tests::shipped_export_*`, `live_cpu_accounting_*`) reproduce identically on the untouched base in this worktree (missing agent-utils/reverie build inputs), so they are pre-existing environment failures, not this change.
- New/focused tests pass: `every_committed_pinned_root_node_forwards_the_keep_going_markers` (87/87 pinned steps, both markers, committed and rebuilt commands), `pinned_wrapper_delivers_keep_going_markers_to_the_payload`, `pinned_wrapper_preserves_actual_renderer_literal_arguments_and_status`, `pinned_wrapper_forwards_admitted_nextest_width_without_changing_filter_tail`, `every_committed_pinned_root_node_forwards_the_rustc_stack_size`.
- `rust-script --test scripts/validate.rs keep_going_keeps_families_and_marks_every_step` — passes with the new pinned-root forwarding assertions. (`scripts/validate.rs --self-test` in this fresh worktree stops earlier on a missing `agent-utils/rs/bin/dagrun` binary, an environment gap unrelated to this change; the extended unit test is exercised directly above and by the `selftest_validate_rs` DAG node in pre-flight.)
- `cargo run -p hermit-manifest-plan --bin generate-validation-dag -- --check` — `ci/dag/validate.json is canonical and fresh`; regenerated count: 262 steps, 87 pinned-root, 87 forwarding both markers.

## Test plan

Exact head: `dc67ffd0de3afad82a85cf87f602e3f2f5065b79` (branch `museops/keep-going-pinned-root`, rebased on `origin/main` at `9d3dd84b4d0`). Review and pre-flight should bind this head: run the focused manifest-plan tests named above, the extended `validate.rs` keep-going unit test, and `generate-validation-dag --check`; a `--keep-going` validation run should show a failing pinned-root nextest node running its whole batch instead of stopping at the first failure. No assertion was weakened and the fail-fast default without `--keep-going` is unchanged.

