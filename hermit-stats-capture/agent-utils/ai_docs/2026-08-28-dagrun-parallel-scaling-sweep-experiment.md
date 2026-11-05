# Dagrun target-time parallel-scaling sweep: design and experiment

**Date:** 2026-08-28
**Status:** implemented and experimentally evaluated

## Abstract

This change adds a graph-wide, target-time parallel-scaling experiment to `dagrun`. The first pass
is mandatory and atomic: every selected DAG node is run alone, in stable topological order, at a
coarse topology-derived width grid. If time remains, later atomic passes add integer midpoints while
retaining prior anchor widths, so the dataset becomes both denser and better replicated. The target
is therefore an allowance checked between passes, never a timeout that truncates a measurement.

The experiment records wall time, user and system CPU time, achieved cores, cgroup memory peak,
thread peak, throttling, pressure, ambient load, co-tenant and sweep provenance. Raw CSV remains the
source of truth. A deterministic machine/container-specific JSON model is refreshed beside it and
is never written into the authored DAG.

A boxed synthetic three-node experiment on an 88-core/176-thread AMD EPYC host collected 81
successful samples over the automatic nine-width grid. The fitted model selected 64 workers for an
embarrassingly parallel workload, four for a four-worker-limited workload, and one for deliberately
interfering serial work. The same data exposed a memory/concurrency local optimum in CPA; the
planner was consequently extended to compare feasible overlap levels by modeled makespan rather
than freezing the first overlap at which the one-worker seed fit.

## Questions

The work tests four claims:

1. A soft allowance can bound how many complete passes begin without ever killing pass 1 or a pass
   already in progress.
2. Isolated width sweeps can distinguish useful parallel scaling, a real plateau, and harmful
   parallelism using wall time and CPU work together.
3. Repeated width-specific cgroup peaks are sufficient to model memory as `M(step, p)` instead of
   treating memory as one width-independent scalar.
4. The planner can use measured `T(step, p)`, `C(step, p)`, and `M(step, p)` to trade inner width,
   graph critical path, CPU work, memory, and achievable cross-step overlap.

## Architecture and policy

### Sweep schedule

`dagrun sweep --target-time DURATION` performs a stable topological walk of the selected graph and
runs one node at a time. Dependencies determine order, but no other DAG node runs beside the node
being measured. A command's normal width-injection channel changes its inner parallelism; the
synthetic graph uses `cmdtype: generic-with-flag` with `jobs_flag: --jobs`.

Pass 1 contains powers of two through the process-visible physical-core count, followed by the
exact physical and effective-logical boundaries when they are not already present. Affinity and a
binding cgroup quota tighten those limits. Thus the host used here produced
`1,2,4,8,16,32,64,88,158`. The documented 158-core/316-thread case produces
`1,2,4,8,16,32,64,128,158,316`.

Pass 2 and later retain all prior widths and add the integer midpoint of every remaining gap. For
example, 48 is added between 32 and 64. Retaining anchors supplies replication as well as finer
resolution. `--repeat K` adds explicit within-pass replication. `--step` can restrict the graph and
`--jobs` can replace the automatic grid for controlled experiments.

The allowance is checked only before starting another pass. Pass 1 always completes. A later pass
that begins before the deadline also completes. The command reports elapsed time and positive
overrun explicitly.

### Data collection

Each persisted step sample contains the ordinary run telemetry plus sweep identity:

- elapsed wall time, user CPU time, system CPU time, and achieved effective cores;
- cgroup `memory.peak`, configured memory ceiling, memory-event deltas, and OOM evidence;
- descendant thread peak, CPU throttling, quota utilization, and cgroup CPU counters;
- external CPU demand, co-tenant counts, load averages, and CPU/memory/I/O PSI when available; and
- sweep ID, mode, pass, sample, repeat, width source, target, detected topology, and workload
  digest.

Rows now record `runner_name=sweep` and report `enforcement_kind=cgroup-v2` only when a cgroup
manager actually boxed the sample; deliberately unboxed sweeps say `unboxed`.

The optional, high-volume modes are deliberately separate from the ordinary measurements.
`--profile-timeseries DURATION` records cgroup CPU/thread activity in `traces/<run_id>.csv`.
After the uninstrumented sweep chooses a useful plateau width, `--perf-record` (optionally with
`--perf-window DURATION`) and repeatable `--wprof-window DURATION` run additional trials at that
width and retain their manifests and artifacts under `captures/`. Instrumented trials never feed
back into the scaling model.

Every successful profiled sweep also refreshes a standalone interactive
`profile_report.html`. It combines the DAG, historical and current scaling curves, resource
response, within-step time series, and links to the optional capture artifacts.

### Storage policy

Profiles are written by default under `./.dagrun/profiles/`, relative to the caller's current
directory. Write-location precedence is:

1. `--no-profile` disables local writes;
2. `--output-dir DIR` (`--perf-dir DIR` is a backward-compatible alias);
3. `DAGRUN_PROFILE_DIR`;
4. `./.dagrun/profiles/`.

Reading is controlled independently by `--no-profile-feedback`. The raw
`step_profiles_<machine>_<container>.csv` dataset is authoritative and append-only. A successful
profiling-enabled sweep atomically refreshes
`scaling_model_<machine>_<container>.json` beside it. That JSON is a deterministic, replaceable
cache rebuilt from raw rows. Neither measurements nor fitted curves are written into the DAG;
the DAG contains portable commands and policy, not machine-specific observations.

Workload digests bind observations to command text, command type, width-injection channel, and
environment. Once an exact digest cohort exists, the model does not blend another identified
command shape into it. Portable summary schema 2 also keys its bounded reservoirs by
`(step, width, workload_digest)`; schema-1 rows remain a blank-digest compatibility fallback.

## Model

For each step and measured width `p`, the model retains:

- a MAD-trimmed robust wall estimate, both before and after ambient-contention adjustment;
- total measured CPU work `C(p) = user(p) + system(p)`;
- achieved effective cores and throttled time; and
- width-specific memory `M(p)`.

Speedup is `S(p) = T(p_min) / T(p)` and parallel efficiency is `S(p) / p`. The economic plateau is
defined globally, rather than by adjacent points:

1. normally exclude widths with `C(p) > 1.5 * C(p_min)`;
2. find the best wall time among the remaining widths within the available core budget; and
3. choose the narrowest width no more than 10% slower than that best wall time.

This makes the recommendation stable when a later pass inserts midpoint samples. A separate
regression width requires a slowdown greater than 5% relative to the fastest point and
non-overlapping observed wall ranges.

An exact `M(p)` is trusted only after at least three uncensored samples at that width. The model
uses the nearest-rank 90th percentile. A sample taken at an active memory ceiling is retained as a
lower bound rather than misrepresented as an exact peak. The planner applies exact-width memory
without scaling it a second time.

CPA uses measured CPU seconds as work area when present, falling back to `p*T(p)` for older rows.
Its lower bound is

```text
max(T_CP, sum_i C_i(p_i) / P)
```

where `T_CP` is the allocated critical-path length and `P` is the outer core budget. Widths are
chosen only from measured points and never beyond each step's economic plateau. With a memory
budget, CPA evaluates every feasible active-step ceiling from the requested maximum down to one,
runs the allocation at that fixed overlap, simulates the deterministic no-overcommit schedule,
and selects the smallest modeled makespan; exact ties keep the larger overlap. This last rule was
added after the experiment found that fixing the first seed-feasible overlap could trap the planner
at narrow widths.

## Experimental method

The benchmark graph is `examples/07-graph-scaling-sweep.yaml`. Its three chained nodes are:

- `scale.parallel`: fixed work split across every requested worker;
- `scale.four-core`: fixed work split across `min(p, 4)` workers; and
- `scale.sequential`: serial useful work plus competing, intentionally useless work when `p > 1`.

The chain makes topological order observable. Sweep execution still isolates each node and invokes
the command afresh at every width. Each worker touches private memory so per-width cgroup peaks are
observable.

### Provenance

- Repository base: `bea36ab6fbb2bf77048caa9d58e82dffcd0d9225`
- Staged implementation tree used to build the measurement binary:
  `0986cae0c41ad995f4330fce4b1caed5d95d3d89`
- `dagrun` version: 0.15.0
- Measurement binary SHA-256:
  `6dae51818b7a7b25af43cbd53e7d45ca643dee4f71a7c571bf3dccc0e0f74c86`
- OS: Linux `7.1.3-0_fbk0_rc18_0_gd373cd4b8dbf`, x86-64
- CPU: AMD EPYC 9D64, 1 socket, 88 physical cores, SMT2, 176 online threads
- Sweep-visible topology: 88 physical cores and 158 effective logical CPUs
- Shared-slice CPU quota: 15,840%, or 158.4 core-equivalents
- Outer memory limit observed by the run: 201,112,985,600 bytes; swap disabled;
  `memory.oom.group=1`
- Per-step memory ceiling: 4 GiB
- Work per invocation: `DAGRUN_SYNTH_WORK=10000000`
- Repetitions: three at every width
- Sweep ID: `18d00b6798e0be4400285160`

Command:

```sh
DAGRUN_SYNTH_WORK=10000000 rs/target/release/dagrun sweep \
  --dag examples/07-graph-scaling-sweep.yaml \
  --target-time 0 \
  --repeat 3 \
  --output-dir /tmp/dagrun-scaling-paper-full.pMCnnH
```

The temporary store contained 81 rows: three steps times nine widths times three repeats. Every row
was successful, cgroup-boxed, attributed to the sweep runner, and free of timeout, CPU-timeout,
OOM, or memory-event increments. Every one of the 27 fitted width points therefore had three
uncensored memory samples and no censored floor. External demand ranged from 0.840 to 6.157 cores
(mean 1.921); the model's contention adjustment accounts for this measured background.

Artifact checksums (the machine-local files are deliberately not committed):

| Artifact | SHA-256 |
|---|---|
| Whole-run CSV | `5d469ab9017e3001028cb64d20d579e37c5a2f4efc85cfb562dca9313386685a` |
| Raw per-step CSV | `33cc3c2a5a86978d917e1a2c6e5aba9ff9d4c502f8b2a04f34f3ae817cdc17bd` |
| Derived model JSON | `7fa63747d2db21d1e261ae05b33776129ad26536284ff7b816a98861a9943d12` |

The retained dataset is also available as a
[standalone interactive report](assets/2026-08-28-dagrun-parallel-scaling-sweep/interactive-report.html)
with the CPU-weighted DAG and per-step scaling drill-down.

## Results

Values below are the saved model's contention-adjusted robust wall time, not the fastest row printed
by the live sweep table. CPU growth and memory growth are relative to the one-worker model point.

### Embarrassingly parallel work

| Workers | Wall (s) | Speedup | Efficiency | CPU (s) | CPU growth | Effective cores | Peak MiB | Memory growth |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2.585 | 1.000 | 1.000 | 2.526 | 1.000 | 0.957 | 45.4 | 1.000 |
| 2 | 1.411 | 1.833 | 0.916 | 2.540 | 1.006 | 1.773 | 46.0 | 1.013 |
| 4 | 0.806 | 3.206 | 0.802 | 2.561 | 1.014 | 3.137 | 46.2 | 1.017 |
| 8 | 0.510 | 5.066 | 0.633 | 2.579 | 1.021 | 5.007 | 52.5 | 1.155 |
| 16 | 0.371 | 6.970 | 0.436 | 2.607 | 1.032 | 7.000 | 73.4 | 1.617 |
| 32 | 0.351 | 7.359 | 0.230 | 2.788 | 1.104 | 8.286 | 114.9 | 2.530 |
| 64 | 0.291 | 8.879 | 0.139 | 3.725 | 1.475 | 12.716 | 199.1 | 4.383 |
| 88 | 0.291 | 8.879 | 0.101 | 4.170 | 1.651 | 14.243 | 261.8 | 5.765 |
| 158 | 0.350 | 7.387 | 0.047 | 6.237 | 2.469 | 17.656 | 446.2 | 9.824 |

The recommendation is 64 workers. It reaches the best modeled wall time while remaining just under
the 1.5x CPU-work guard. Eighty-eight workers tie the wall result but consume 1.65x baseline CPU;
158 is a statistically separated regression. The useful work is close to conserved through 32,
then process/scheduling overhead becomes material. Memory grows strongly with worker count, making
the explicit `M(p)` curve operationally important even before wall scaling reverses.

### Four-worker-limited work

| Workers | Wall (s) | Speedup | Efficiency | CPU (s) | CPU growth | Effective cores | Peak MiB | Memory growth |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2.586 | 1.000 | 1.000 | 2.508 | 1.000 | 0.964 | 44.8 | 1.000 |
| 2 | 1.421 | 1.820 | 0.910 | 2.559 | 1.020 | 1.781 | 45.2 | 1.009 |
| 4 | 0.829 | 3.119 | 0.780 | 2.606 | 1.039 | 3.047 | 47.1 | 1.052 |
| 8 | 0.811 | 3.187 | 0.398 | 2.569 | 1.024 | 3.134 | 44.7 | 0.998 |
| 16 | 0.811 | 3.189 | 0.199 | 2.571 | 1.025 | 3.153 | 47.3 | 1.055 |
| 32 | 0.819 | 3.157 | 0.099 | 2.567 | 1.024 | 3.076 | 45.4 | 1.014 |
| 64 | 0.844 | 3.064 | 0.048 | 2.579 | 1.028 | 3.030 | 46.3 | 1.034 |
| 88 | 0.805 | 3.213 | 0.037 | 2.569 | 1.024 | 3.154 | 46.2 | 1.032 |
| 158 | 0.829 | 3.118 | 0.020 | 2.586 | 1.031 | 3.102 | 46.1 | 1.029 |

The recommendation is four workers. Although one noisy high-width point is slightly faster, four
is only 3.0% slower than the best eligible wall estimate and is therefore the narrowest point in
the global 10% plateau. CPU work and memory are essentially flat because the workload starts at
most four workers. This is the intended answer to “7.2x at 8 versus 7.9x at 64”: reserve the wider
capacity for other useful work when the narrower point is economically equivalent.

### Sequential/interfering work

| Workers | Wall (s) | Speedup | Efficiency | CPU (s) | CPU growth | Effective cores | Peak MiB | Memory growth |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2.620 | 1.000 | 1.000 | 2.528 | 1.000 | 0.959 | 57.4 | 1.000 |
| 2 | 3.848 | 0.681 | 0.340 | 3.760 | 1.487 | 0.967 | 57.6 | 1.004 |
| 4 | 3.248 | 0.807 | 0.202 | 4.381 | 1.733 | 1.340 | 44.6 | 0.778 |
| 8 | 2.950 | 0.888 | 0.111 | 4.860 | 1.922 | 1.583 | 62.8 | 1.094 |
| 16 | 2.796 | 0.937 | 0.059 | 4.896 | 1.937 | 1.735 | 87.4 | 1.524 |
| 32 | 2.876 | 0.911 | 0.028 | 5.199 | 2.057 | 1.870 | 128.8 | 2.245 |
| 64 | 2.731 | 0.959 | 0.015 | 6.092 | 2.410 | 2.206 | 215.5 | 3.757 |
| 88 | 2.731 | 0.959 | 0.011 | 6.698 | 2.650 | 2.454 | 263.4 | 4.593 |
| 158 | 2.812 | 0.932 | 0.006 | 8.823 | 3.490 | 2.956 | 451.6 | 7.875 |

The recommendation is one worker and the first confirmed regression is two. No wider point beats
the one-worker wall time. At 158 workers the job consumes 3.49 times the CPU work and 7.88 times the
peak memory while taking 7% longer. This is exactly the class of node a whole-graph planner should
keep narrow.

### Time allowance

The zero-second target intentionally forced the minimum-pass case. The complete 81-sample pass took
133.249 seconds, and the command reported an overrun of 133.249 seconds. It did not terminate a node
or omit a later node after crossing the target. This demonstrates that the control is a soft
between-pass allowance rather than a per-command kill deadline.

## Whole-graph planning consequence

For the motivating “A and B with 32 cores” question, consider independent copies of
`scale.parallel` and `scale.four-core`, with their authored dependency and hard 4-GiB benchmark caps
removed so the measured curves alone drive the thought experiment. The analytical memory envelope
is set to factor 1, floor 0, and safety factor 1; the real production defaults remain conservative.

The result is not an 8/24 or 16/16 split. CPA starts from the narrowest measured widths and widens
the current critical path by measured marginal wall reduction. It reaches 4/4: A takes 0.806 s and
B takes 0.829 s. Widening A further cannot improve the two-task makespan because B is now the
bottleneck; widening B is outside B's economic four-worker plateau. Total measured CPU area is only
about 5.17 CPU-seconds, so the 32-core area bound is about 0.161 s and the critical-path term
dominates.

| Memory budget | A width / peak | B width / peak | Modeled overlap | Modeled makespan | Stop reason |
|---:|---:|---:|---:|---:|---|
| none, 120 MiB, 100 MiB, 94 MiB | 4 / 46.2 MiB | 4 / 47.1 MiB | 2 | 0.829 s | knee exhausted |
| 93 MiB | 2 / 46.0 MiB | 2 / 45.2 MiB | 2 | 1.421 s | memory capped |
| 90 MiB, 80 MiB, 70 MiB, 50 MiB | 4 / 46.2 MiB | 4 / 47.1 MiB | 1 | 1.635 s | knee exhausted |
| 45 MiB | minimum is 45.4 MiB (does not fit) | minimum is 44.8 MiB | 1 | infeasible | infeasible memory |

The 90 MiB case is the regression that changed the allocator. Two one-worker peaks total slightly
more than 90 MiB, so concurrency two is infeasible. The corrected search selects serial 4/4 in
1.635 s instead of either declaring failure or retaining a slower narrow allocation. At 93 MiB,
concurrent 2/2 fits and beats serial 4/4, so the planner retains overlap. This makes the memory
decision monotone in modeled outcome rather than in the order constraints happened to be tested.

For an independent `scale.parallel` plus `scale.sequential` pair, CPA leaves both at one worker.
The sequential step is already the 2.620-second bottleneck and has no positive eligible widening;
widening the parallel step cannot reduce the pair's 2.620-second completion time. In general, a
work-inefficient task receives more cores only when a measured positive marginal gain shortens the
current critical path enough to outweigh the area pressure and stays within the CPU-growth guard.

## Limitations

- This is one host, one kernel, one Python multiprocessing implementation, and one work quantum.
- Three repeats per width are enough to activate exact-width memory modeling, but not enough to
  characterize long-tailed production variance.
- The full experiment exercised one mandatory pass. Unit and cross-language tests cover midpoint
  generation, cumulative later passes, and soft-deadline pass admission. A separate boxed control
  run with `--target-time 2s --jobs 1,4` completed pass 1 in 1.274 seconds, then ran cumulative pass
  2 at `1,2,4` to completion; total time was 3.161 seconds and the reported overrun was 1.161
  seconds.
- The workload digest identifies command shape and configured environment, not arbitrary external
  input contents. Callers still need stable commands and immutable or explicitly restored inputs.
- The sweep isolates DAG nodes from each other, but unavoidable machine-wide background demand is
  observed rather than eliminated. The recorded range was modest and contention adjustment was
  active.
- The synthetic process-pool benchmark exposes scheduler and process-start costs as well as pure
  compute scaling. That is useful for end-to-end job sizing, but it is not a microarchitectural
  kernel benchmark.
- This historical run did not request the now-available opt-in `perf`/`wprof`
  follow-up captures.

## Reproduction and acceptance criteria

Run the graph-wide benchmark with the automatic topology grid:

```sh
DAGRUN_SYNTH_WORK=10000000 dagrun sweep \
  --dag examples/07-graph-scaling-sweep.yaml \
  --target-time 0 \
  --repeat 3 \
  --output-dir /tmp/dagrun-scaling-study
```

Inspect the raw rows and rebuildable sidecar under that directory. A valid reproduction should
show:

- all three nodes in topological order and never overlapping;
- a complete first pass even though target time is zero;
- `3 * width_count * repeat_count` successful profile rows;
- sweep, workload, containment, timing, CPU, memory, throttling, and ambient provenance;
- a plateau near four for `scale.four-core`;
- one worker and a regression marker for `scale.sequential`; and
- rising speedup followed by overhead/CPU-work pressure for `scale.parallel`.

The exact numeric knee of `scale.parallel` is intentionally machine-dependent. That is why the
model lives beside machine/container-specific measurements rather than in the portable DAG.
