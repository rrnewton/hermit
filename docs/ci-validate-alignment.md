<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# CI and scripts/validate.rs alignment

Hermit CI is partitioned by host capability, not by test duration:

| Lane | Workflow and runner | Local command | Capability contract |
| --- | --- | --- | --- |
| Portable | `ci-portable.yml`, `ubuntu-latest` | `./scripts/validate.rs portable-only --no-label-pr` | No PMU counters, CPUID faulting, or KVM |
| Privileged | `ci-privileged.yml`, `[Linux, X64, hermit, pmu]` | `./scripts/validate.rs --privileged-only --no-label-pr` | PMU overflow delivery, CPUID faulting, and read/write `/dev/kvm` |

Local exact-head validation is the required broad product evidence. The
portable workflow supplies a second, non-gating signal after integration-branch
pushes or manual dispatches. The privileged workflow is a manually dispatched
capability sentinel and must finish in less than five minutes. Long PMU stress,
debugger, language-runtime, and application matrices do not belong in the
scarce privileged lane.

## Multi-mode E2E harness

`target/debug/test-harness` discovers tests from the schema-v2 YAML bucket files under
`tests/e2e/manifests/`. The CI-enabled category set is:

- `system-utils`
- `data-handling`
- `determinism-stress`
- `language-runtimes`
- `applications`

The 2026-08-17 Rust cutover deliberately stopped adding
`--no-virtualize-cpuid` and `--max-timeslice=disabled` to portable cells. At
base `302a1a9c0fde564db0292d1ea5cabc91343e79bc`, that changes the invocation of
168 required portable verify/chaos cells (166 verify and 2 chaos). This count
becomes stale whenever the required plan changes; every result row records the
literal argv so the policy remains observable rather than inferred.

Eight additional C-corpus buckets are centrally discoverable with `ci=false`
until each direct guest's standalone build and output contract is calibrated.
They participate in schema, inventory, and disabled-backend audits without
silently expanding the blocking CI denominator.

Test programs contain no policy annotations. Each central entry declares its
program path, lane, observation tuple, timeout, and all five modes. Mode is the
outer list; each mode partitions its complete backend set between
`backends-enabled` and `backends-disabled`, with a reason for every disabled
backend. `target/debug/test-harness validate` fails on an invalid partition, stale
inventory, unclassified file under `tests/`, or replay backend other than
ptrace.

The modes have distinct contracts:

| Mode | Contract |
| --- | --- |
| `naked` | Explicit meta-CI only: run three to five times without Hermit and require declared nondeterminism |
| `verify` | Run every allowlisted backend with `--strict --verify` |
| `replay` | Run ptrace `record start --strict --verify` in an isolated recording directory |
| `chaos` | Require cross-seed diversity and exact within-seed reproduction |
| `custom` | Run Hermit with manifest-declared edge-case arguments |

Portable Hermit cells add `--no-virtualize-cpuid` and
`--max-timeslice=disabled`. Every result records the source SHA and dirty bit,
test and binary hashes, effective arguments, relaxations, lane, mode, backend,
duration, and outcome. JSONL, JUnit XML, and a denominator-aware summary are
stored below `target/e2e/` and uploaded by both workflows.

Each cell receives repo-local `HOME`, `XDG_CONFIG_HOME`, fixtures, captures,
and recording directories. Hermit guests use the isolated `/tmp/hermit-e2e`
logical work path so built-in verification cannot leak run-one mutations into
run two. The checked-in XDG seed is under `tests/e2e/xdg-config/`; developer
configuration is never read.

## DAG wiring

The focused `test.recorded_clocks` node runs six maintained ptrace cases:
captured clock output/errno replay, uncaptured clock refusal, previous-clock
format refusal, the existing pre-flock format refusal, exec continuity, and
thread clock ordering. Its hosted twin is
assigned to the portable workflow's integration shard; both require exactly
six executed tests. The `recorder-clock-focused` label prepares only the two
default-feature harnesses and the 43 source-bound `record_replay` workloads.
The flock fixture retains its existing runtime `cc` compilation inside the
unchanged per-test bounds; it is not part of that prepared workload population.
`build.recorded_clocks` runs before `build.workspace`, so full preparation is
the last metadata publisher in broad profiles. An explicit focused selection
must include the focused producer and test, in the same filesystem root;
selecting only the test does not supply its prepared artifacts. The new
producer adds a provisional 1200-second cold-build bound to the hosted debug
job's existing 1800-second critical path. The selected path is now
`setup.nextest` (600 seconds), focused preparation (1200 seconds), then
`build.workspace` (1200 seconds). Its 55-minute outer budget covers those 3000
seconds plus 300 seconds of setup and artifact overhead. The CI audit derives
this path from the actual selected DAG nodes and compares it with the workflow
bound; these are declared budgets, not a measured cold-build duration. The
six tests retain their existing 22-second CPU and 57-second wall limits and
zero retries. The original exec/thread cases now use the same strict INFO
comparator as the captured-output replay case, retaining their existing guests
and success assertions while also requiring canonical parity and nonempty logs.

The serial consumer has a provisional 180-second CPU / 420-second wall bound.
Six test CPU allowances total 132 seconds; the remaining 48 seconds account for
cleanup and orchestration outside those allowances. Earlier failed attempts
reached 24.011 CPU seconds during teardown, so 132 seconds alone would not bound
the node. The wall allocation is six times (57 seconds plus 2 seconds of grace
plus 5 seconds for hard reaping), plus 36 seconds for orchestration. These are
finite aggregate budgets, not a measured six-test maximum or an increase to any
test's allowance. The unchanged memory bounds are a 1-GiB scheduling baseline
and 2-GiB hard cap. The separate manifest gate retains its 600-second CPU and
900-second wall limits.

`ci/dag/validate.json` is the single committed validation graph. Its `portable`
label selects one metadata node and one resource-serialized E2E node per
category, plus their dependency closure. Its `privileged` label selects:

- the focused Hermit and Detcore build;
- CPUID-faulting validation;
- PMU overflow/skid validation; and
- the KVM E2E shell/environment sentinel.

The 139-program record/replay compatibility ratchet runs as a separate step in
the manually dispatched full-validation job. The privileged workflow's outer
bound is kept beyond the selected graph's computed critical path, and the
manifest audit fails if either side changes without the other.

Both `scripts/validate.rs` and GitHub Actions select labels from this exact DAG.
Use `dagrun ascii --dag ci/dag/validate.json` to inspect the superset without
running tests. `target/debug/test-harness audit-ci`
checks unique node IDs, dependency references, Rust harness commands, and the
typed manifest bucket selectors in both labelled populations. It also compares the required cells with
`ci/expected-e2e-plan.json`, so the blocking denominator cannot silently shrink.

## Reconciliation checklist

When adding or changing an E2E test:

1. Put the workload in a focused shell, C, or Rust source file.
2. Add it to exactly one bucket manifest and declare all five modes.
3. Add only locally proven backend combinations to an allowlist.
4. Run `target/debug/test-harness validate` and inspect `plan --format json`.
5. Run the affected mode/backend cells and retain their JSONL/JUnit results.
6. Update `tests/e2e/manifests/inventory/test-files.json` with its disposition and runner.
7. Update the owning DAG only when a category or capability dependency changes.
8. Never replace a semantic workload with `--help`, `--version`, or a no-op
   launcher probe.
