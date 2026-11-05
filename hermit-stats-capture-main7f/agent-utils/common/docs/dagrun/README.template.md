# dagrun

`dagrun` executes a directed acyclic graph of build and test steps with
dependency ordering, bounded concurrency, resource-aware planning, Linux cgroup
containment, and per-step profiling.

It provides:

- strict JSON and YAML DAG loading;
- deterministic list, visualization, conversion, and planning commands;
- independent active-step (`--max-steps`) and total CPU (`--max-cpus`) limits;
- memory- and named-resource-aware concurrent execution;
- nested cgroup-v2 containment with explicit unboxed opt-outs;
- profile feedback, parallel-speedup sweeps, and stress copies; and
- durable, collision-free CPU reservation for benchmark runs.

{{DISTRIBUTION}}

## Quick start

Save this as `pipeline.yaml`:

```yaml
steps:
  - group: build
    job: app
    desc: compile the application
    cmd: make build
  - group: test
    job: unit
    desc: run unit tests
    cmd: make test
    deps: [build.app]
```

Inspect and run it:

```sh
dagrun ascii --dag pipeline.yaml
dagrun run --dag pipeline.yaml
```

Containment is required by default. On a machine where cgroup-v2 delegation is
intentionally unavailable, add `--allow-cgroup-failure` to accept a visibly
unboxed run.

A `run` launched from one of another run's steps is refused and names the outer
run. Use `--allow-unwise-nest-dagruns` only for a reviewed temporary exception;
prefer flattening the caller into one DAG.

Useful discovery commands:

```sh
dagrun quickstart
dagrun --help
dagrun --userguide
dagrun capabilities
```

For example, `run -s2 -j200` permits at most two active DAG nodes while setting
the whole run's total CPU-bandwidth budget and each runner-controlled step's
maximum inner width to 200 core-equivalents. The two active steps may request
more than 200 workers in aggregate: the verified outer `cpu.max` makes them
share 200 core-equivalents instead of treating declared widths as reservations.
That quota is not an instantaneous thread-count or CPU-identity bound; use
`--cores K` for an exclusive fixed cpuset. Drop the `-s2` and the same command
permits **two hundred** active nodes: an absent `--max-steps` defaults to the
resolved `--max-cpus`, so a bandwidth number you type is also a concurrency
number unless you say otherwise. The long
spelling of `-j` is `--max-cpus`; migrate the 0.13 `run --jobs` spelling to it.
A hidden compatibility alias keeps existing 0.13 scripts working but is not
public run vocabulary; differing simultaneous values conflict and are rejected.
`sweep --jobs RANGE` remains the width-range option for a single-step speedup
experiment: a bare `--jobs 8` means the dense set `1..8`, not one run at width
8. `sweep --target-time 10m` instead walks one node at a time
in stable topological order and completes as many whole cumulative passes as
fit the soft allowance. Pass 1 always finishes; later passes bisect the existing
width gaps and rerun the retained anchor widths. `--step` and `--jobs` are
optional limits in target mode. With no `--jobs`, pass 1 uses powers of two
through the physical-core count and then the exact physical-core and logical-
thread counts (for 158/316: `1,2,4,8,16,32,64,128,158,316`). Because widths and passes deliberately rerun a
node in the same working tree, benchmark commands must be repeatable (or the
caller must restore their inputs and outputs between invocations). A known step `cmdtype` supplies the runner's
exact width arguments; a simple command receives them at the end, while a
compound command puts an unquoted `$DAGRUN_EXTRA_ARGS` where they belong. A
compound command without that placement is refused. `-j3` is one shell word;
`--jobs 3` is two, and quoting the variable would turn those two arguments into
one, so dagrun refuses that double-quoted multi-word form; single quotes prevent
expansion and are also refused. The valid values are `unknown` (default),
`make`, `cargo-build`, `cargo-test`, `cargo-nextest`,
`generic-dash-j-command`, and `generic-with-flag`. The last requires a
step-level `jobs_flag`. Under `unknown`, `DAGRUN_EXTRA_ARGS` is absent and the
existing `jobs_flag`/`jobs_env` rules apply. `default_jobs_env` supplies the
environment channel inherited by steps, and `DAGRUN_JOBS_ENV` supplies that
default when the document omits it. Empty effective channels prevent rewriting.
When paired with a positive declared width, that width is self-managed and the
run refuses it if it exceeds the total budget.

Greedy-LPT and critical-path choose only dispatch order. CPA chooses per-step
widths from isolated speedup curves and checks the largest dependency/resource-
reachable memory footprint. With `--max-mem`, it compares each feasible overlap
ceiling down to serial execution and selects the widths/overlap with the smallest
no-overcommit modeled makespan. It does not model how co-running work changes
either curve; its reference makespan explains allocation, while the live runtime
may oversubscribe the outer CPU quota.

Runs and sweeps auto-append raw CSV measurements to `./.dagrun/profiles/`
relative to the current directory. Sweeps call this `--output-dir` (with
`--perf-dir` retained as an alias); other commands use `--perf-dir`.
`DAGRUN_PROFILE_DIR` is the secondary override, and `--no-profile` disables
writes. These options do not redirect terminal or child output. The command
always reports the artifacts it writes: whole-run and per-step CSVs, the
successful sweep's refreshed `scaling_model_*.json` and standalone interactive
`profile_report.html`, and optional time-series CSVs under `traces/`. Use
`--report-html FILE` to relocate the report or `--no-report` to omit it. The raw store, not
the authored DAG, is the source of learned scaling data. Every successful
profiling-enabled sweep atomically refreshes a deterministic machine/container-specific
`scaling_model_*.json` sidecar beside it; that cache can be rebuilt at any time.
For a view of parallelism within a step, `run` and `sweep` accept the opt-in
`--profile-timeseries DURATION` sampler (50ms through 10s). It requires active
cgroup-v2 containment and writes CPU/thread traces under
`<profile-dir>/traces/<run_id>.csv`; those high-volume rows are diagnostic data,
not independent trials consumed by the scaling estimator.
The HTML report combines observations across commits, offers last-N-commit and
environment/workload filters, sizes DAG nodes by CPU time, and drills into each
step's speedup, memory, CPU-efficiency, and interval-parallelism plots.

For deeper diagnosis, `sweep --perf-record [--perf-window DURATION]` and
repeatable `--wprof-window DURATION` run separate profiler-instrumented trials
at the economic sweet-spot width selected from the ordinary sweep. These
opt-in trials are never fed back into the scaling model. In target-time mode
they run after mandatory pass 1 and count against the allowance before another
refinement pass starts. `--profiler-sudo` is the only way dagrun adds `sudo -n`;
privilege escalation is never automatic. Private manifests and artifacts live
under `<output-dir>/captures/`.
Sweep rows and the sidecar carry a command-shape digest, and summaries retain
separate reservoirs per digest, so identified old command data is never mixed
into the current curve (blank pre-digest rows are a compatibility fallback only).
Every dagrun distribution uses the same explicit append-only profile schema
rather than serializing implementation-specific objects, so measurements remain
portable across compatible installations and future modeling tools.
Its economic plateau is the narrowest measured width within 10% of the best
eligible wall time, normally excluding widths that
consume more than 1.5x the baseline CPU seconds. CPA uses measured CPU seconds
for work area when present (`p * wall` otherwise) and trusts an exact-width
memory peak only after three uncensored samples at that width; capped peaks are
retained as lower bounds.

## Attributable test-runner timeouts

A DAG node can contain a parallel test runner. On a node timeout, `dagrun`
first freezes durable test and process evidence, sends `SIGTERM` so the inner
runner can flush its state, waits a bounded grace, and only then escalates to
cgroup-wide/process-group `SIGKILL`.

Use three strictly nested bounds: the test runner's per-test timeout below the
DAG step timeout, and the step timeout below the whole-DAG timeout. The
innermost bound produces the clearest named failure. The DAG's process snapshot
is a backstop for a missing or mis-sized runner-native timeout, not a substitute
for one.

For a controlled harness, emit explicit boundaries as work starts and ends:

```text
##TEST-START suite::case
##TEST-END suite::case PASS
```

Several tests may be live concurrently. The timeout report lists the complete
live set and elapsed time for each; the longest-running is labelled only as the
*likely* culprit when more than one remains. Set
`DAGRUN_LOG_DIR` to retain the incrementally flushed per-step log
and `journal.jsonl` even if an outer supervisor later kills the runner.

For third-party runners, the pre-signal `/proc` snapshot reports CPU-burning
and wall-stalled processes as distinct signatures. It binds a process to a
test only when argv supplies a direct test identifier. A one-process-per-test
runner can therefore be bound; a shared-process runner cannot. Without a
direct identifier or recognized output boundaries, the report says
attribution is unavailable rather than guessing.

## CPU-set companion

The distribution also installs `cpuset-alloc`. It reserves disjoint cores in a
durable cross-process ledger and launches a command in a mutation-verified
`AllowedCPUs` cgroup scope. It refuses to run when hard tree-wide pinning cannot
be proved:

```sh
cpuset-alloc run --cores 2 --tag benchmark -- ./benchmark
cpuset-alloc status
cpuset-alloc reclaim
```

`dagrun pin-run` provides the same hard reservation path from the main
command. `run --cores K` applies an exact cpuset only inside the runner's own
managed scope; it is incompatible with an unboxed opt-out. These commands never
fall back to an escapable process-affinity mask. All release live reservations
on exit and reclaim dead holders.

## License

MIT
