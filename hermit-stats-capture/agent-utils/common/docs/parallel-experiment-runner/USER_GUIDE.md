# parallel-experiment-runner — user guide

Run **N concurrent seed-sweep workers under RESOURCE CONTAINMENT**, using `dagrun`'s two-level
cgroup-v2 scope. A seed sweep is one command template with a `{seed}` placeholder, run over a
range of seeds — a chaos search, a fuzz sweep, a flaky-repro hunt, a parameter scan.

This tool exists because unbounded parallel experiments once left hundreds of processes piled up
on a host. Reproduction sharpened *how* they piled up, and corrected two first impressions:

- The CPU "saturation" was largely **phantom** — most of those processes were zombies and the
  alarming load average counts uninterruptible-sleep, not CPU demand (the box was ~64% idle). So
  concurrency should be sized from **measured idle headroom**, not from a load reading.
- The zombies were real and genuinely held **PID slots** at **zero CPU and zero memory** — a box
  that validated only cpu and memory would have reported perfect health while they piled up. That
  is what the **pids** axis catches. But their *cause* was not a leaked `tracing-appender` thread
  (that was a 15-char `comm`-truncation artifact, `tracing-appende`; five reproduced abandonment
  scenarios stranded **zero** workers). The cause was a **live, hung `target-runner --strict --verify`**
  — its main parked in tokio `epoll_wait` while the guest made no progress — holding a PID namespace
  open so the kernel never reaped the zombies inside it.

A live hang is exactly what a **cpu-time / wall backstop** exists to kill, and the box kills it
with a **cgroup-subtree `cgroup.kill`** that takes the hung main *and everything in its PID
namespace* at once — releasing the namespace so the zombies are reaped, rather than killing one pid
and inheriting the orphans. So containment is the whole fix: **concurrency is a declared, enforced
number sized from measured idle headroom**, every worker runs under real cgroup limits on four
axes, and a worker that breaches a limit — including a hang — is cleanly killed, subtree and
namespace included, with a message naming what breached and by how much.

## Resource containment, not a security sandbox

The threat model is a **bug in our own code**, not an adversary. We trust the workload's *intent*;
we distrust exactly one thing about it — its **resource usage**: it may leak memory, run forever,
or fork-bomb. So this is a **resource box**, not a security sandbox: it does **not** reach for
seccomp or user-namespace isolation. Containment is enforced on **four independent axes**, each
mapped to one named failure mode:

| axis | failure it stops | mechanism |
| --- | --- | --- |
| **cpu** | *run forever* | a CPU-**second** budget from the cgroup `cpu.stat` (not wall) |
| **memory** | *leak memory* | `memory.max`, OOM detected via `memory.events` |
| **pids** | *fork bomb* | `pids.max` — the only axis that stops PID exhaustion |
| **wall** | defence-in-depth backstop | wall timeout, **derived at ~3× the CPU budget** when unset |

The pids axis matters because neither `cpu.max` nor `memory.max` stops a fork bomb: it exhausts
the PID space, not CPU or memory. A `pids.max` breach **contains** the fork (the `clone`/`fork`
returns `EAGAIN`) rather than kernel-killing the worker, so the denied-fork count is captured and
the cpu/wall guard reaps the contained worker. The kill message then names the fork-bomb even
though the reaping cause was the wall/CPU guard.

It is a thin, additive generalization of `dagrun`: each seed becomes one contained `Step`, and
the existing scheduler runs them. There is **no second runner** — the containment, teardown,
CPU-second budget, and per-step measurement are all the CI runner's, reused.

## The four guarantees

1. **CPU-time budgets, not wall.** `--cpu-timeout N` is a per-worker **CPU-second** budget
   (user+system), measured from the worker's cgroup `cpu.stat` and immune to machine load. It is
   never derived from wall time: on an N-core host, N wall-seconds is up to N CPU-seconds, so a
   wall-derived CPU budget would be ~1/N of what a worker needs and would false-kill healthy
   workers in a way indistinguishable from a flaky test. Omit the flag to leave it **UNSET**
   (honest; the derived wall backstop still applies) rather than guess a too-tight number.
2. **Declared + enforced concurrency, sized from MEASURED headroom.** The width is resolved from
   three budgets — the coordinator's lane, live host capacity, and the measured per-worker
   footprint — and the round runs *exactly* that many workers, never "however many the caller
   spawned". Live CPU capacity is the **measured idle-core headroom** (total cores scaled by the
   idle fraction sampled from `/proc/stat`), **not** the total core count and **not** load average
   — so an idle box fans out generously and a genuinely busy one steps back, both from real CPU
   demand rather than a phantom-saturation proxy.
3. **Estimate up front, actuals on completion.** Before the sweep, a **derived** per-worker cost
   estimate (from prior samples of the same profile key) is printed — or an explicit **UNSET**
   when there is no comparable sample. Never a plausible-looking constant. After each round and at
   the end, the measured wall and CPU actuals are printed.
4. **Clean kill naming the breach, and by how much.** A worker over its CPU budget, over
   `memory.max` (OOM), past its `pids.max`, or past its wall backstop is reaped via `cgroup.kill`
   (setsid-proof) and reported as e.g. `CPU-TIMEOUT: used 3.0s cpu >= budget 3s` or
   `PIDS-CAP: 4 fork/clone(s) denied at pids.max 16 (fork-bomb / PID-exhaustion containment)`.

## Quick start

```bash
parallel-experiment-runner run \
  --name chaos-divergence --seeds 0-199 \
  --cpu-cores 1 --memory 4G --cpu-timeout 120 --pids 512 \
  --hit-regex 'DIVERGENCE|panic' --max-concurrency 32 \
  --identity backend=ptrace image=demo5 \
  -- ./workload --chaos --seed {seed}
```

`--wall-timeout` is omitted above on purpose: with a `--cpu-timeout`, the wall backstop is
**derived at ~3× the CPU budget** (here 360s), so you set the authoritative CPU guard once and the
hang-guard follows. Pass `--wall-timeout` only to override that derivation.

* Every occurrence of `{seed}` in the argv after `--` is replaced with the concrete seed.
* Per-worker logs are written to `<work-dir>/ignored/logs/seed-<n>.log`; the run prints only an
  aggregate summary, so a wide round does not interleave hundreds of streams.
* The runner ramps width `1 → 2 → 4 → …`, measuring the real per-worker footprint at width 1
  before scaling, and downshifts immediately if the lane shrinks or the host gets busy. Width is
  always capped at 316, even if `--max-concurrency` requests more.

`plan-round` shows the resolved width and the lowered DAG without running anything (and needs no
cgroups) — use it to check how wide a round *would* go:

```bash
parallel-experiment-runner plan-round --name demo --seeds 0-99 \
  --cpu-cores 2 --memory 8G --max-concurrency 40 -- ./workload {seed}
```

## Hits, misses, and breaches

Each worker ends in exactly one status. **Breaches are classified first**, so a worker the runner
had to kill is never miscounted as a discovered hit:

| status | meaning |
| --- | --- |
| `hit` | the hit regex matched the log **or** the exit code was a declared hit code |
| `miss` | ran cleanly (exit 0) without the target condition |
| `command-error` | nonzero exit that is not a declared hit code |
| `cpu-timeout` | exceeded the CPU-second budget → reaped |
| `memory-cap` | hit `memory.max` → OOM-killed inside its own cgroup |
| `pids-cap` | hit `pids.max` → fork/clone denied (fork-bomb / PID-exhaustion containment) |
| `timeout` | exceeded the wall backstop (a hang burning no CPU) |
| `log-cap` | detached producer killed the worker at its output byte cap |
| `cancelled` | never ran / eager-cancelled |
| `resource-report-error` | configured detached-unit report was missing or malformed |

A `pids-cap` breach is special: `pids.max` **contains** the fork (`EAGAIN`) rather than killing
the worker, so the worker is reaped by the cpu/wall guard. The classifier still reports it as
`pids-cap` (ahead of `timeout`) using the in-memory denied-fork count, so a fork bomb is never
mislabelled as a plain hang.

A hunt for a bug sets `--hit-regex` (e.g. `panic|DIVERGENCE|ASAN`) and/or `--hit-exit-codes`
(e.g. `134` for SIGABRT). With neither, the default hit is exit code `0` (a sweep for successful
runs). The run exits nonzero if any worker breached a limit, so a wrapper notices.

## Profile keys and estimates

Samples are grouped by a **stable profile key** so estimates are apples-to-apples. The key hashes
the command template (with the seed removed), the `--identity` fields, and the hit semantics. Two
runs that differ on a mandatory identity dimension (`backend`, `kernel_id`, `image_id`, `vcpu`,
`guest_memory`, `accelerator`) never share history, even under a manual `--profile-key`.

With **no** `--identity` and no `--profile-key`, the key is *ephemeral*: the sweep still runs and
calibrates, but its samples are **not** persisted, so an unattributable run cannot contaminate
another sweep's estimates. Give at least one `--identity K=V` to build reusable history.

The estimate is conservative where it matters: median wall (wall is contention-inflated, so its
median is the fairer central figure) and p90 CPU/memory (don't under-provision).

## Resource containment mechanics

Containment is **on by default**. On first launch the process re-execs into a transient
`systemd-run --user --scope` (a delegated cgroup) named under `parallel-experiment.slice`, then
carves a child cgroup per worker with its `memory.max` / `cpu.max` / `pids.max` caps. If cgroup-v2
+ a working `systemd --user` scope are unavailable, the run refuses with exit 3 rather than
silently run uncontained. `--allow-cgroup-failure` opts into an explicit UNCONTAINED run
(process-group teardown only, no per-worker caps) — not recommended, and the reason the box is the
point.

### The kill reclaims the namespace, not just the process

The zombie pile-up that motivated this tool looked at first like leaked teardown threads that a
"clean up after abandoned runs" reaper would fix. Reproduction refuted that: five abandonment
scenarios (SIGKILL the launcher mid-run, launcher-then-guest kill, `--verify` kill mid-Run-1, six
staggered kills) each stranded **zero** workers. A SIGKILLed launcher's direct children die with
it, and `install_scope_teardown`'s SIGTERM/SIGKILL/atexit hook `cgroup.kill`s the whole scope on
the exits that *do* run a handler. There is no leaked-thread class to reap on teardown.

What actually held the zombies open was a **live, hung** `target-runner --strict --verify` — its main
parked in tokio `epoll_wait` while the guest made no progress — keeping a PID namespace alive so the
kernel could not reap the zombies inside it. That is a running process over its time budget, and it
is precisely what the per-worker **cpu-time / wall backstop** kills. The requirement that follows is
about *how* the kill lands: it must reclaim the **namespace**, not just one process. It does — every
breach (and normal exit) routes through a **cgroup-subtree `cgroup.kill`** (see
`dagrun.teardown.reap`) that SIGKILLs *every* member of the worker's cgroup atomically, including
`setsid`/double-fork escapees a process-group kill would miss. When the hung main and all its
namespace peers die together, the namespace refcount drops to zero and the kernel reaps the zombies
— so the box does not kill one pid and inherit the orphans. No separate teardown reaper is needed;
the containment kill is the fix.

## Spec files

Instead of inline flags, pass `--spec sweep.json` (or `.yaml`, needing PyYAML):

```json
{
  "name": "chaos-divergence",
  "command": ["./workload", "--chaos", "--seed", "{seed}"],
  "worker_limits": {"cpu_cores": 1, "memory_bytes": 4294967296, "cpu_timeout_s": 120, "pids_max": 512},
  "hit": {"regex": "DIVERGENCE|panic", "hit_exit_codes": [134]},
  "identity": {"backend": "ptrace", "image": "demo5"}
}
```

## Flags

Run `parallel-experiment-runner run --help` for the full list. The important ones:

| flag | meaning |
| --- | --- |
| `--seeds` | seed spec: `0-99,200,300-305` (inclusive ranges + singletons) |
| `--cpu-cores` | per-worker CPU cores → inner `cpu.max` and the width core-unit |
| `--memory` | per-worker memory cap (e.g. `4G`) → inner `memory.max` |
| `--cpu-timeout` | per-worker CPU-second budget; omit = UNSET (never wall-derived) |
| `--pids` / `--max-pids` | per-worker `pids.max` (fork-bomb containment); omit = no cap |
| `--wall-timeout` | per-worker wall backstop; omit = derived at ~3× the CPU budget |
| `--disk` | per-worker disk reserve used to bound width (cgroup-v2 has no space controller) |
| `--hit-regex` / `--hit-exit-codes` | what marks a worker a HIT |
| `--identity K=V` | apples-to-apples fields hashed into the profile key |
| `--max-concurrency` | requested ceiling on concurrent workers; machine hard cap is 316 |
| `--slice-cpu/-memory/-disk` | the coordinator lane envelope (default: whole machine) |
| `--memory-reserve` | bytes kept outside the lane, subtracted from live `MemAvailable` every round |
| `--detached-resource-report` | per-seed report path containing `{seed}` for a child outside the worker cgroup |
| `--detached-unit-prefix` | ownership prefix required before a reported unit may be cleaned up |
| `--work-dir` / `--log-dir` / `--profile-store` | where logs + samples live |
| `--format` | `human` (default) or `json` |

## Detached child resource reports

A workload wrapper can move its expensive child into a transient service outside the runner's
per-worker cgroup. Without an explicit report, the wrapper's cgroup peak is not the child's peak
and calibration would fan out from a false low measurement.

Use both `--detached-resource-report PATH/{seed}.txt` and
`--detached-unit-prefix PREFIX`. Detached mode also requires explicit `--memory`,
`--cpu-timeout`, and `--pids` limits. The producer must apply matching memory, task-count,
CPU-quota, CPU-time, and wall-time controls to the detached unit before it starts the payload.
The append-friendly UTF-8 report accepts arbitrary text prefixes ending in `: ` and requires
exactly one value for each of these fields:

```text
wrapper: unit=worker-123
wrapper: memory_peak_bytes=1073741824
wrapper: cpu_usage_nsec=2500000000
wrapper: tasks_peak=42
wrapper: limit_memory_bytes=1073741824
wrapper: limit_swap_bytes=0
wrapper: limit_tasks=128
wrapper: limit_cpu_cores_micros=1000000
wrapper: limit_cpu_time_nsec=30000000000
wrapper: limit_wall_time_usec=90000000
wrapper: cpu_time_limit_hit=0
wrapper: wall_time_limit_hit=0
wrapper: log_cap_hit=0
wrapper: memory_oom_kills=0
wrapper: tasks_limit_hits=0
```

The runner verifies every reported limit against its declared `WorkerLimits`, folds the detached
breach counters into outcome classification, adds detached memory and CPU to the ordinary
worker-cgroup measurements, and exposes task peak in JSON output. A missing, malformed, or
mismatched report is an infrastructure breach and never a clean sample. On normal completion and
contained signal teardown it attempts to kill, stop, and reset only syntax-valid unit names
beginning with the declared prefix. This is an ownership boundary: never use a broad prefix shared
with unrelated units.
