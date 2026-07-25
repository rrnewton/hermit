# CI validation lanes as safe-ci-dag-runner DAGs

This directory holds a declarative migration of Hermit's CI validation lanes
onto [`safe-ci-dag-runner`](../../agent-utils/common/docs/safe-ci-dag-runner/README.md)
(from the `agent-utils` submodule). Each validation *gate* becomes a DAG node
with explicit dependencies and resource limits, so the scheduler can run
independent gates concurrently while boxing each one under its own cgroup
(memory cap + full process-subtree teardown).

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
ci/run-dag.sh hardware -j 1                    # PMU lane, one gate at a time
ci/run-dag.sh hosted   ascii                   # visualize instead of run
```

## Status: additive, not yet the blocking gate

This is a **parallel, opt-in** path. `validate.sh` remains the single source of
truth for gate *commands* and the current *blocking* CI checks. Nothing here
changes what gates CI requires today. The intent is to let a human compare the
DAG runner's results against `validate.sh` before flipping any required check
over to it. The opt-in workflow
[`.github/workflows/ci-dag.yml`](../../.github/workflows/ci-dag.yml) runs on
`workflow_dispatch` only, so it adds no per-PR load until deliberately invoked.

### Hard dependency: the `agent-utils` submodule must land first

The runner lives in the `agent-utils` submodule. **As of this branch's base,
`agent-utils` is not yet committed on `rrnewton/hermit:main`** — it exists only
as a staged submodule addition in the primary checkout (pointing at
`rrnewton/agent-utils`). This DAG path cannot run in CI until:

1. `agent-utils` is landed as a submodule on `rrnewton/hermit:main` (with an
   HTTPS URL in `.gitmodules` so GitHub-hosted runners can clone it), and
2. CI checks out submodules (`ci-dag.yml` uses `submodules: recursive`).

`ci/run-dag.sh` prefers the compiled Rust binary
(`agent-utils/rs/bin/safe-ci-dag-runner`, built by `agent-utils/setup`) and
falls back to the Python entrypoint (`agent-utils/py/bin/safe-ci-dag-runner`),
which is the only 0.1 implementation with Linux cgroup boxing + perf logging.

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
  `./validate.sh --strict-compat-only`, and (hardware) `rr.compat_baseline`
  runs `./validate.sh --rr-compat-only`. These flags build the release binary
  themselves.
- **Serial per-target loops are inlined** as a `for` loop with `set -e`
  (`test.hermit_integration`, `hw.integration`, and the `pmu.*` exact-case
  gates), matching `run_hermit_targets_serial` / `run_exact_detcore_cases`
  including their per-case `timeout`s.
- **The hosted `envelope_levels` gate is inlined** (L1–L4 over the three
  `ENVELOPE_PROBES`: `true`, `echo`, `date`) because it has no standalone
  `validate.sh` flag. It mirrors `run_hosted_envelope_levels` (validate.sh
  ~line 2573). If `ENVELOPE_PROBES` changes in `validate.sh`, update this node.

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
  `rss_baseline_bytes` of a schedulable set) fits the budget.

**Inner** — each gate's own box:

- `rss_baseline_bytes` — estimated peak RSS, the input to `-j` sizing.
- `hard_mem_max_bytes` — explicit inner cgroup `MemoryMax` (applied only under
  `--cgroups`); a gate that exceeds it is OOM-killed **in isolation** rather
  than taking down the run.
- `est_duration_s` — orders ready gates longest-first (packing only; never a
  correctness contract).
- `classification` — `cpu-bound` (compiles, PMU compute), `latency-bound`
  (guest execution / I/O), or `light` (fmt, contract checks).

> **The `rss_baseline_bytes` / `hard_mem_max_bytes` / `est_duration_s` values
> are hand-estimated, not measured.** They are safe starting points for
> `--max-mem` sizing and inner caps, not benchmarks. Refine them from a real
> run's `--perf-dir` CSVs (`ci/run-dag.sh hosted --perf-dir ./perf`) before
> relying on tight memory budgets.

## Conservatism and how to relax it

The `hermit_guest: 1` / `pmu: 1` serialization faithfully reproduces
`validate.sh`, which ran these gates strictly one-after-another. It is
intentionally conservative: as individual guest gates are shown to be safe to
co-run (e.g. distinct scratch directories, no shared fixture), drop their
`resources` hint (or raise the cap) to unlock more parallelism. The DAG shape
and dependencies stay the same.
