# CI validation lanes as safe-ci-dag-runner DAGs

This directory holds a declarative migration of Hermit's CI validation lanes
onto [`safe-ci-dag-runner`](../../agent-utils/common/docs/safe-ci-dag-runner/README.md)
(from the `agent-utils` submodule). Each validation *gate* becomes a DAG node
with explicit dependencies and resource limits, so the scheduler can run
independent gates concurrently. On hosts with delegated cgroup v2 support, it
can also box each node for memory limits and full process-subtree teardown.

- [`hosted.json`](hosted.json) — mirrors `validate.sh`'s **`--hosted-only`**
  lane (`run_hosted_only_suite`), the GitHub-hosted `regular` job in
  [`.github/workflows/ci-hosted.yml`](../../.github/workflows/ci-hosted.yml).
  No PMU / CPUID interception required.
- [`hardware.json`](hardware.json) — mirrors `validate.sh`'s **`--hardware-only`**
  lane (`run_hardware_validation`), the self-hosted `hardware` job in
  [`.github/workflows/ci-selfhosted.yml`](../../.github/workflows/ci-selfhosted.yml).
  Requires PMU + `/dev/kvm`.

Run a lane with the wrapper:

```sh
ci/run-dag.sh hosted   --max-mem 32G          # memory-aware -j
ci/run-dag.sh hardware -j 2                    # PMU lane, one gate at a time
ci/run-dag.sh hosted   ascii                   # visualize instead of run
```

On hosts with a systemd user session, `ci/run-dag.sh` automatically re-execs a
run inside a delegated cgroup-v2 scope. One complete run is hard-capped at 32
GiB with swap disabled, and every node requests its `hard_mem_max_bytes` cap.
Any delegation or cap-application failure is reported visibly by the runner.
Runs without `-j` or `--max-mem` use that same 32 GiB scheduler budget. All
concurrent runs also share a `safe-ci.slice` cap equal to the smaller of 64 GiB
and 75% of host RAM, so additional agents cannot expose the host to their sum.
The wrapper requires that shared cap to hold two complete outer scopes; the
default pair therefore needs roughly 86 GiB of host RAM and fails preflight on
smaller hosts instead of starting an underprovisioned run. `CI_DAG_JOBS` sets a
default explicit job count when callers do not pass `-j` or `--max-mem`.
Set `CI_DAG_OUTER_MEMORY_MAX` to tune the per-run cap or `CI_DAG_CGROUPS=0` only
when an already-isolated environment must opt out;
`CI_DAG_AGGREGATE_MEMORY_MAX` tunes the shared cap. Local hosts without a user
scope fail closed. GitHub-hosted disposable VMs opt out explicitly and run one
node at a time; they remain bounded by the VM rather than the shared host slice.

## Status: active validation lanes

`hosted.json` drives the required `Regular tests (GitHub-hosted)` job, and
`hardware.json` drives both self-hosted PMU entrypoints. Existing job names,
the merge-gate contract, and the outer PMU `flock` stay unchanged; only the
internal scheduler changes. `validate.sh` remains the source of truth for
individual gate commands.

Closed pull requests trigger a skipped workflow in the same concurrency group,
which cancels their queued or in-progress PMU run. The `mem_race` family and
three nonblocking post-DAG diagnostics run in the scheduled `super` tier so a
known host-sensitive hang cannot consume the required serialized lane.

The `Validation Levels` workflow no longer launches a second copy of
`--hosted-only` for every pull request. Its quick lane remains available by
manual dispatch, while merge-group hardware and scheduled super validation are
unchanged. The manual [`ci-dag.yml`](../../.github/workflows/ci-dag.yml)
workflow runs either DAG on demand.

### Runner dependency

This change pins `rrnewton/agent-utils` at v0.2.0 as an HTTPS submodule. Hosted
CI initializes only `agent-utils` instead of all submodules, then executes the
dependency-free Python runner so per-node performance CSVs are available
without an install step. `ci/run-dag.sh` also accepts
`SAFE_CI_DAG_RUNNER` for local or preinstalled binaries.

## Speed-to-signal audit

A successful PR run on 2026-07-26 provided the baseline:

- The blocking hosted validation took 14 minutes after setup.
- Eight nonblocking diagnostics then ran serially for another 20 minutes,
  extending the required workflow from useful signal at minute 17 to completion
  at minute 37.
- `Validation Levels` independently repeated the same 14-minute hosted suite,
  consuming another GitHub-hosted runner.

The diagnostics now run in the scheduled `super` tier. The required 16 GiB
GitHub-hosted lane explicitly uses `-j 1`; the measured local cold-build caps
are intentionally not treated as a portable VM memory model. Compile, lint, documentation, unit,
and contract nodes may overlap when dependencies allow, while Hermit guest
executions retain the
`hermit_guest: 1` exclusion. Per-node performance reports are uploaded from
every required run so estimates can be replaced with measurements.

## How gates map onto the DAG

`validate.sh` already encodes a hand-rolled DAG:

| `validate.sh` construct        | DAG equivalent                                   |
| ------------------------------ | ------------------------------------------------ |
| `run_check NAME cmd…`          | one node (serial via a shared scarce resource)   |
| `start_check NAME cmd…`        | one node with no scarce resource (parallelizes)  |
| `wait_for_background_checks`   | implicit — the scheduler joins on all nodes      |
| ordering "build, then the rest"| `deps: ["build.workspace"]`                       |

Each node's tag is `group.job` (e.g. `build.workspace`, `lint.clippy`).

### Command fidelity

Node `cmd`s are the **verbatim** commands `validate.sh` runs, with three
deliberate exceptions, chosen to avoid duplicating script logic that has many
moving parts:

- **Composite envelope gates reuse `validate.sh`'s own standalone entrypoints**
  so there is one source of truth: `test.strict_compat` runs
  `./validate.sh --hosted-strict-compat-only`, and (hardware) `rr.compat_baseline`
  runs `./validate.sh --rr-compat-only`. The hardware flag builds release;
  hosted strict compatibility reuses `STRICT_COMPAT_HERMIT_BIN` from the
  preceding workspace build. Without that override, the strict flag builds
  release as before.
- **The DBI stderr-isolation CLI case is a separate 120-second node** so a
  backend hang fails quickly without consuming the aggregate CLI budget. The
  aggregate node skips that case, so the test set remains unchanged.
- **Hosted strict compatibility starts after every non-guest Cargo node** so
  its `shell-build` run1/run2 comparison cannot observe concurrent target or
  cache mutation. Those short nodes still run in parallel before the barrier.
- **Hermit integration targets use one Cargo invocation** with repeated
  `--test` selectors (`test.hermit_integration` and `hw.integration`). Cargo
  plans and links the selected targets together, then executes their separate
  test binaries serially. The `pmu.*` exact-case gates retain their `for` loops
  and per-case `timeout`s to preserve fail-fast hardware isolation.
- **The hosted `envelope_levels` gate is inlined** (L1–L4 over the three
  `ENVELOPE_PROBES`: `true`, `echo`, `date`) because it has no standalone
  `validate.sh` flag. It mirrors `run_hosted_envelope_levels` in `validate.sh`.
  If `ENVELOPE_PROBES` changes in `validate.sh`, update this node.

## Resource model (outer + inner limits)

The task's "outer + inner resource limits" map onto the runner's two knobs:

**Outer** — how many gates may co-run:

- `resource_caps` gates *scarce* resources. `hosted.json` sets
  `{"hermit_guest": 1}`; every gate that executes Hermit on guest programs
  carries `resources: {"hermit_guest": 1}`, so they run **one at a time**
  (they share the working filesystem, are mutually nondeterministic, and on a
  PMU host contend for the counter). Non-guest gates — `build`, `clippy`,
  `rustfmt`, doctests, `rustdoc`, nextest of non-Hermit crates — carry no
  scarce resource and parallelize freely. `hardware.json` uses `{"pmu": 1}`
  the same way; the PMU is genuinely exclusive, so that lane is essentially
  serial after the initial builds.
- `--max-mem SPEC` (or `-j N`) bounds total concurrency. With `--max-mem`, the
  runner picks the largest `-j` whose modeled worst-case footprint (summed
  `hard_mem_max_bytes` of a schedulable set) fits the budget. It always permits
  at least one node, even when that node's cap exceeds the scheduler budget;
  constrained hosted VMs therefore use explicit `-j 1` instead.

**Inner** — each gate's own box:

- `rss_baseline_bytes` — rounded observed peak RSS retained as profile data and
  used for sizing only when a hard cap is absent.
- `hard_mem_max_bytes` — explicit inner cgroup `MemoryMax` (applied only under
  `--cgroups`); a gate that exceeds it is OOM-killed **in isolation** rather
  than taking down the run.
- `est_duration_s` — orders ready gates longest-first (packing only; never a
  correctness contract).
- `classification` — `cpu-bound` (compiles, PMU compute), `latency-bound`
  (guest execution / I/O), or `light` (fmt, contract checks).

The hosted lane's memory values were measured at Hermit `c1c3eb2d` on an AMD
EPYC 9D85 host using cgroup-v2 `memory.peak`. Compile-sensitive nodes were
profiled from cold target directories. Baselines round observed peaks up to 128
MiB; hard caps round at least 1.5x the observed peak to 128 MiB, with a 256 MiB
minimum for small shell checks. The largest observed peaks were rustdoc 16.00
GiB, workspace build 8.65 GiB, and strict compatibility 8.36 GiB. Re-profile
after material dependency, toolchain, or test-matrix changes with
`ci/run-dag.sh hosted --perf-dir ./perf`.

The `setup.nextest` limit includes a separately profiled cold
`cargo install cargo-nextest` fallback (2.90 GiB peak), not only the installed
binary's fast version check.

The hardware lane's cold release build peaked at 10,006,720,512 bytes and uses
the same rounded 1.5x rule. Remaining hardware-only test values stay
conservative until a complete successful PMU profile is available; the current
single runner repeatedly times out in the parallel memory-race family before
later nodes execute.

## Conservatism and how to relax it

The `hermit_guest: 1` / `pmu: 1` serialization faithfully reproduces
`validate.sh`, which ran these gates strictly one-after-another. It is
intentionally conservative: as individual guest gates are shown to be safe to
co-run (e.g. distinct scratch directories, no shared fixture), drop their
`resources` hint (or raise the cap) to unlock more parallelism. The DAG shape
and dependencies stay the same.
