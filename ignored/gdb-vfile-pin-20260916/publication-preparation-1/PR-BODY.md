[hermit2, degraded-unresolved, gpt-6-astra, devbig014, role=impl]

## Plain Language Summary and Project Impact

Pin the already-landed Reverie GDB protocol repair from https://github.com/rrnewton/reverie/pull/559. GDB's unsupported `vFile:lstat` request previously closed the replay connection, followed by a GDB internal crash and the CLI test's 57-second timeout. The original replay-stage failure-classification test now completes in 0.558 seconds and satisfies its existing container-child-exit and SIGKILL assertions.

## Summary

Advance the uniform Reverie pin from `a158914eceeca02a9ab4c7dd4e9916926d5e5c1e` to landed `d87a03a312421d34dee81dae71aa395b40231e63`. This range includes the preceding landed `f83b6d8f45b5092676d9ffa1d99a742660eac661` paused/in-guest counter API change as well as the unsupported-vFile repair. The upstream protocol controls isolated the repair against that f83 base; they do not independently recertify the counter design.

The 12-file change updates eight Cargo manifests, two locks and two coupled CI pin bindings. The Cargo metadata contains 46 revision entries: 20 manifest entries and 26 lock source entries. Native DynamoRIO recipe inputs are identical across the pin range, so the existing 16-job clamp and 1050 effective-job-second threshold are carried unchanged, with explicit provenance. This is not a new timing measurement. No temporary CLI diagnostics or new test exclusions are included.

## Determinism

The landed protocol change recognizes an unsupported operation by its operation name and emits the existing empty unsupported reply. Supported-operation parsing and malformed-command failures retain their prior path; no timing, scheduling, filesystem lookup or invented successful Host I/O result is added by that repair. This pin does not change Hermit's scheduling implementation, comparators or numerical test limits.

## Linux Semantics

Unsupported GDB remote-file operations do not acquire host filesystem semantics. In particular, `lstat` is not implemented as host path access. Supported commands keep their existing validation. The original replay fixture still injects SIGKILL and requires the typed container-child failure classification.

## Validation

- At tested Hermit `932f7130ea3c77dcdb2547b6834f46f08bf593f0`, the unchanged official `full --allow-local-off-the-record-run --selected test.cli -j 1 --no-label-pr --verbose --run-timeout 11400` path passed all 12 prerequisite/consumer nodes and all 78 selected CLI methods, with zero retries. The 113 prepared identities comprise 78 executed, 34 existing skip matches and one existing ignored case. Framework and typed CPU attempt identities match exactly. Nextest took 64.011 seconds; the full invocation exited 0 in 1821.761 seconds.
- The affected method `hermit::cli$record_classifies_a_gdbserver_replay_stage_container_child_failure` uses the default ptrace replay/GDB path, default logging, and `record --verify-with-gdbex <existing Python SIGKILL;quit commands> -- /bin/true`. No new relaxation or comparison policy was added. Its result is component/L0 failure-classification evidence, not an L2 replay-equivalence claim.
- Upstream native controls: all 39 gdbstub tests passed; the identical new test module on old production had 37 passes and two intended parser/real-connection failures. Another 138 unrelated tests were filtered. Normal pin policy, locked workspace check, Clippy and formatting passed before the CLI run.
- Publication head `e208b86786ea7d8d7c7eb81e9783c8af04165c7f` appends current main `fe41081742652fce09fc22f20469df10fa09ddaa`. The exact 12-file pin patch, CLI source, image and all 12 selected DAG rows are unchanged. All 11 incoming files equal main. The composed source passed the supported whole-workspace Clippy check in 38.880 seconds, formatting in 2.652 seconds, shell syntax and whitespace checks, with source unchanged. No guest rerun at this later head is claimed.

The CLI run was an off-record selected component run, with no full-validation ledger receipt or full-main-green claim. All bound workload units/cgroups and original root PID generations were absent at terminal readback. The observer recorded driver absence with no reported errors, but its numeric process exit was not separately retained; no exit code is inferred from a collected unit.

Evidence: owned slot `worktrees/slots/dev-hermit-gdb-vfile-pin-20260916`, under `ignored/gdb-vfile-pin-20260916/official-cli-1/final-evidence/REPORT.md` and `publication-preparation-1/FINAL-SOURCE.json`. Full log: `/home/newton/work/dev-hermit/worktrees/slots/dev-hermit-gdb-vfile-pin-20260916/ignored/gdb-vfile-pin-20260916/official-cli-1/FULL.log`.

Task: vision-ci-signal-is-trustworthy-end-to-end
