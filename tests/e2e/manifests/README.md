<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# Centralized e2e test manifests (schema v2)

These YAML files are the load-bearing policy source for Hermit's executable
end-to-end tests. Test programs contain behavior only; lane, mode, backend,
timeout, build flags, observation policy, and exclusion reasons belong here.
`target/debug/test-harness` loads them through the structured Rust parser in
`ci/manifest-plan`.

The 13 manifests separate calibrated blocking cells from discoverable migration
inventory. CI creates one independently schedulable run node for every bucket.
Six buckets currently contain calibrated blocking workloads:

- `system-utils.yaml`
- `data-handling.yaml`
- `determinism-stress.yaml`
- `language-runtimes.yaml`
- `applications.yaml`
- `c-programs.yaml` (eight calibrated Buck-derived C probes)

Eight additional `*-c.yaml`/`c-programs.yaml` buckets make 180 more C guests
centrally discoverable. Eight `c-programs.yaml` entries have calibrated
standalone build and output contracts and run in blocking CI; the remaining
172 C guests keep `ci = false` until they are calibrated. Buckets without a
calibrated cell still have a CI node that intentionally reports zero cells,
and the correspondence audit proves that this cannot hide a calibrated cell.
Every entry still declares all five modes and every backend exclusion, so
inventory does not silently imply support.

## Matrix symmetry and the test front door

Compatibility coverage enters through these shared schema-v2 manifests, not
through a backend-owned guest list. Every test declares all five modes, and
every non-naked mode partitions the complete `ptrace`, `dbt`, `kvm`, `sabre`,
and `liteinst` axis into enabled cells and explicit gaps. Any active mode must
include ptrace so the reference behavior is established before another backend
ratchets it.

`ci/matrix-symmetry-baseline.json` records the small amount of older policy
debt: ptrace-less manifest rows and guest fixtures owned by a backend-specific
or legacy backend-parity driver. `hermit-manifest-plan` requires that baseline
to match exactly, so private corpora cannot grow. Migrating a baseline entry to
a shared manifest is allowed, but the same change must remove it from the
baseline. This makes the shared test identity the row axis; backend support or
gaps remain cells of that one row rather than creating backend-private rows.

## Schema contract

Every entry under `test` names either a repo-relative `program` or a `direct` shell
command. Program extensions select the runner:

- `.sh`: execute the existing `--prepare`/`--run` protocol directly;
- `.c`: compile implicitly with `cc` plus optional `build.cflags`;
- `.rs`: compile implicitly with `rustc` plus optional `build.rustflags`.

`MODE` is always the outer axis. Every entry declares exactly these five
tables: `verify`, `chaos`, `replay`, `naked`, and `custom`. Each table has a
`backends_enabled` list and a `backends_disabled` table. The two must form a
complete, disjoint partition and every disabled backend needs a nonempty WHY.
For non-naked modes the axis is `ptrace`, `dbt`, `kvm`, `sabre`, and
`liteinst`; naked partitions only `native`.

```yaml
test:
  - id: example/test
    modes:
      verify:
        ci: true
        backends_enabled: [ptrace]
        backends_disabled:
          dbt: DBT coverage is owned by its backend parity partition
          kvm: KVM requires the privileged runner
          sabre: SaBRe requires its external runtime
          liteinst: LiteInst coverage is owned by its compatibility partition
```

The mode contracts are:

| Mode | Contract |
| --- | --- |
| `verify` | Run each enabled backend with `hermit run --strict --verify` |
| `chaos` | Search declared seeds and require cross-seed diversity plus exact within-seed reproduction |
| `replay` | Run ptrace `record start --strict --verify` in an isolated recording directory |
| `naked` | Opt-in meta-CI only; run natively three to five times and require declared variation |
| `custom` | Run declared edge-case Hermit arguments and require three to five identical observations |

An enabled `verify` cell is green only when its typed report records canonical
strictness, log comparison, positive INFO counts on both runs, bitwise parity,
and a matched verdict. Output-only, stripped, empty-log, malformed, or
contradictory reports are infrastructure errors rather than product results.

An enabled SaBRe cell has an additional execution-path contract. Every E2E
Hermit execution writes structured evidence into the cell capture: the
in-guest tool must have issued a coordinator RPC, and both
`ptrace_fallback_sites` and `trusted_shared_object_sites` must be zero. A
ptrace-installed SaBRe marker is
classified as fallback; a raw syscall observed in a trusted shared object is
classified as native execution outside the measured SaBRe path. Either makes
the cell fail even when status and stdout match. The JSONL result retains the
per-execution records and aggregate eligibility under `execution_path`.

Any mode may declare backend-specific guest arguments. The harness appends
these after the guest executable, separately from Hermit's own arguments:

```yaml
test:
  - id: example/test
    modes:
      verify:
        ci: false
        ci_disabled_reason: Not selected by ordinary validation yet
        backends_enabled: [ptrace, kvm]
        guest_args:
          ptrace: [multi]
          kvm: [multi]
```

Every `guest_args` key must name an enabled backend. Omitted backends receive
no guest arguments.

An enabled `naked` mode may set `ci = true` only with its bounded `runs` and
`assert.min_distinct` outcome oracle intact. Ordinary validation then executes
the native process repeatedly and fails the cell when it observes fewer than
the declared minimum distinct outcomes. A mode with no enabled backend remains
visible with `ci = false` and a reason for every disabled backend. Regular CI
executes only cells with `ci = true`; run one enabled manual cell with explicit
test and mode filters:

```sh
target/debug/test-harness run --include-manual --mode verify \
  --test c-programs/add-key-enosys
```

`--include-manual` requires both exact filters so a broad CI command cannot
accidentally pull the uncalibrated corpus into its run plan.

To measure one documented backend gap without first promoting it into the
known-green envelope, use all three exact cell filters:

```bash
target/debug/test-harness run --probe-disabled --test c-programs/example \
  --mode verify --backend sabre --results target/e2e/probe/results.jsonl
```

`--probe-disabled` selects from `backends_disabled`, is accepted only by
`run`, and cannot be combined with `--ci-only` or `--include-manual`. This is
the bounded expansion path: a passing probe is evidence for a later manifest
ratchet, not an implicit promotion into the regression envelope.
Callers that combine explicit mode/backend filters with CI policy must add
`--ci-only`. This is how `scripts/validate.rs quick` avoids expanding the manual C
inventory.

## Inventory and validation

`inventory/test-files.json` classifies every regular file and symlink below
`tests/` with a disposition, owning runner, and per-file justification. The
audit compares the inventory byte-for-byte with filesystem discovery, then
confirms that every manifest program is classified as `manifest-test`. Tests
retained under Cargo, Buck, integration, QEMU, or suite drivers explain the
build flags, arguments, expected results, hardware, or shared setup that their
owner supplies. Each exception names its exact owning runner and the file's
specific role; generic category-only justifications fail review even when the
inventory is mechanically complete.

`ci/expected-e2e-plan.json` ratchets the exact blocking cells. Adding, removing,
or reclassifying a `ci=true` cell fails validation until the expected plan is
updated in the same review.

A `ci = false` cell is never executed **and never compiled**, so its guest can
rot without any node noticing. Two mechanisms bound that. `manifest-plan`
rejects every enabled mode with `ci = false` unless it has a non-empty
`ci_disabled_reason`, and rejects a stale reason left behind on a `ci = true`
mode. Separately, `target/debug/test-harness audit-compile --category <bucket>` compiles every C guest
the bucket declares regardless of its `ci` flag; it is wired into the portable
DAG for `backend-parity-c` and fails closed on zero compiled.

Use the load-bearing entrypoints:

```sh
cargo run -p hermit-manifest-plan -- --format text
target/debug/test-harness validate
target/debug/test-harness plan --format json
target/debug/test-harness audit-gaps --format json
target/debug/test-harness build --lane portable --ci-only
target/debug/test-harness run --lane portable
target/debug/test-harness run --lane portable --category system-utils --ci-only --prebuilt
target/debug/test-harness run --mode naked --test system-utils/random-device
```

Both GitHub workflows and `scripts/validate.rs` execute the same portable and
privileged DAG files. Each DAG has a manifest guest-build barrier followed by
one structured selector per bucket. `audit-ci` fails if either caller stops
delegating to the shared plans, a bucket node disappears, a command diverges
from its selector, or the aggregate selected cells differ from the ratchet.

## Adding a test

1. Put behavior in a focused shell, C, or Rust source file.
2. Add it to exactly one bucket and declare all five modes.
3. Enable only combinations proven locally; justify every exclusion.
4. Add or update its exact entry in `inventory/test-files.json`.
5. Run `target/debug/test-harness validate` and the affected cells.
6. Add a structured DAG node when adding a bucket; validation fails until each
   lane has exactly one node per bucket.

Do not replace a semantic workload with `--help`, `--version`, or a no-op
launcher probe.
