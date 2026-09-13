[hermit2, mega-lander, unresolved, devbig014, role=impl]

## Plain Language Summary and Project Impact

Add an explicit per-attempt CPU-budget mode to the existing Nextest wrapper without enabling a production threshold. Before this change, the wrapper measured CPU but could not stop an over-budget attempt, and a successful direct child could hide a CPU-burning descendant. With an explicit budget, the wrapper supervises the complete descendant tree through cleanup, preserves typed first-cause ordering, emits a typed `cpu_timeout` with its budget, boundary observation, final CPU total, and accounting source, and leaves no surviving child.

The same focused slice raises only the outer wall backup for `super.dbt_strict_blocked_stdin_teardown_diagnostic` from 30 to 120 seconds so its inner guard can report the real failure, and updates the timeout-policy regression to the current canonical population of 733 required cells. Ordinary validation remains measurement-only while the project gathers enough comparable idle-host data to choose a defensible threshold.

Task: owner-design-tight-cpu-and-loose-wall-bounds-per-test

## Determinism

This changes validation supervision, not Detcore, guest-visible time, scheduling, syscall behavior, or the strict log comparator. Existing production Nextest invocations do not pass `--cpu-timeout-usec`, so they retain the measurement-only path.

On the opt-in path, terminal-cause selection has one linearization point: an external signal wins only before the wrapper reserves a non-signal decision; a later signal cannot relabel an exit or CPU decision during final accounting. A successful direct child remains supervised while descendants live and is promoted to `cpu_timeout` if the final subtree total crosses the explicit budget. Successful classifications are published atomically, while missing CPU accounting fails closed instead of silently disabling the bound.

## Linux Semantics

The measurement-only path preserves the child's ordinary exit or signal status and reports reaped `wait4` subtree CPU. When interrupted, it truthfully labels the combined live-descendant procfs snapshot plus reaped CPU as `procfs-descendants+wait4`.

The opt-in path uses Linux process groups, subreaping, procfs descendant accounting, `wait4`, and bounded `SIGTERM`/`SIGKILL` cleanup. It continues tracking descendants that change process groups, reaps the subtree before returning, and uses exit 124 for a CPU timeout. No Hermit syscall or backend semantics change.

## Calibration, Dormancy, and Limits

- Production wrapper arguments contain no CPU threshold; enforcement is dormant unless a caller explicitly supplies `--cpu-timeout-usec` and a termination grace.
- A production threshold will not be proposed before at least 10 comparable idle-host Nextest runs from the same Hermit SHA, host, toolchain, and constructed plan. Activation also requires a measured margin in which Nextest's outer termination grace exceeds the wrapper's child-cleanup grace; the current 2-second Nextest grace is unchanged.
- The opt-in monitor samples every 500 ms and procfs CPU values have clock-tick resolution, so the terminal total can exceed the configured boundary. A measurement-only signal record is a snapshot and excludes CPU consumed later during Nextest cleanup.
- The super DBT diagnostic keeps its 60-second CPU bound; only its outer wall backup changes to 120 seconds. That contains the 57-second default inner wall bound plus 2-second grace and the representative 86-second scaled bound plus grace.
- The 733-cell change ratchets the assertion to the already-current canonical plan; it does not alter test selection.

## Validation

Focused evidence at head `ed5bc5a141b91b4afb3ac3b2968fc7430cb0b815` (tree `cdf327063d81810f30cbdfe2006dcff0324d1231`, base `805ecd0042c9746c2eeaadfa5bac2c08490fd733`):

- The five commits replayed conflict-free onto current main. Range-diff matched every commit, and all five stable patch IDs are unchanged from approved head `410f0fb4a33efb837675dd6d0796f2fa6212540d`.
- Nextest CPU wrapper self-test: PASS, including the 500 ms escaped-descendant CPU boundary, typed `cpu_timeout`, deterministic post-reservation signal control, and no survivor.
- Counted-wrapper self-test: PASS.
- Typed Nextest result tests: 19/19 PASS.
- Timeout-policy tests: 8/8 PASS; timeout/configuration controls: 13/13 PASS.
- Manifest-plan tests: 321/321 PASS; generated DAG freshness: PASS.
- Formatting, Clippy, shell syntax, diff, and repository-cleanliness checks: PASS.
- Independent exact-head adversarial review: APPROVED with no findings.

This is focused validation only. A full validation run was not performed, production enforcement remains dormant, and this change makes no project-wide L0-L4 or backend assurance claim.
