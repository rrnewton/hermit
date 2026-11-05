---
name: parallel-experiment-runner
description: Run N concurrent seed-sweep workers under RESOURCE CONTAINMENT via dagrun. A seed sweep is one command template with a {seed} placeholder run over a range of seeds — a chaos search, fuzz sweep, flaky-repro hunt, or parameter scan. Concurrency is a declared, enforced number (not unbounded fan-out) sized from MEASURED idle-core headroom (from /proc/stat, not total cores and not load average); every worker is contained on FOUR resource axes — cpu (CPU-second budget = 'run forever'), memory (memory.max = 'leak'), pids (pids.max = 'fork bomb' / PID-slot exhaustion, which zombies cause at zero CPU and zero memory), and a wall backstop derived at ~3× the CPU budget — with a clean kill naming what breached and by how much; a breach kill is a cgroup-subtree cgroup.kill that reclaims the whole subtree AND the PID namespace, so a hung worker (e.g. a consumer's own `run --strict --verify` parked in epoll_wait, the real cause of the zombie pile-up) is killed with its namespace rather than leaving orphans behind; per-worker footprint is measured via a 1→2→4 calibration ramp; an up-front derived cost estimate (or honest UNSET) is printed before the sweep and measured actuals after. This is a resource box against a BUG in our own code, NOT a security sandbox — no seccomp, no user-namespace isolation. Use when running many parallel experiment instances that must not saturate the host, when a sweep needs real per-worker resource limits, or when hunting a seed that reproduces a bug. It reuses dagrun's containment — it is not a second runner.
---

# parallel-experiment-runner

Runs a seed sweep as N resource-contained workers under `dagrun`'s two-level cgroup-v2 scope.
Exists to prevent unbounded parallel experiments piling up processes on a host.
It is a resource box against a BUG in our own code (leak, run-forever, fork-bomb) — NOT a security
sandbox: no seccomp, no user-namespace isolation. Concurrency is declared+enforced, sized from
**measured idle-core headroom** (from `/proc/stat`, not total cores and not the misleading load
average, which counts uninterruptible-sleep/zombies). Containment is four-axis — **cpu**
(CPU-second budget, not wall), **memory** (`memory.max`), **pids** (`pids.max` — stops a fork bomb
*and* the PID-slot exhaustion that zombies cause at zero CPU/memory, invisible to a cpu+memory-only
box), and a **wall** backstop derived at ~3× the CPU budget when unset; cost is estimated up front
and measured after; a breach is a clean kill naming what breached and by how much.

The zombie pile-up that motivated this tool was NOT a leaked teardown thread (reproduction refuted
that: five abandonment scenarios stranded zero workers; the `tracing-appende` name was a 15-char
`comm` truncation). It was a **live, hung** `run --strict --verify` of a consuming repository's own tool (main parked in tokio
`epoll_wait`) holding a PID namespace open. A hang is what the **cpu-time / wall backstop** kills,
and the kill is a **cgroup-subtree `cgroup.kill`** that reclaims the whole subtree AND the PID
namespace (killing the hung main with every namespace peer, so the kernel reaps the zombies) —
not one pid leaving orphans. Containment is the fix; no separate teardown reaper is needed.

The CLI is the source of truth for usage — do not rely on this file for details. Run:

- `parallel-experiment-runner quickstart` — self-contained getting-started tour.
- `parallel-experiment-runner --help` — commands and flags.
- `parallel-experiment-runner --userguide` — the full user guide (complete reference).

Canonical commands:

- `parallel-experiment-runner run --name S --seeds 0-199 --cpu-cores 1 --memory 4G --cpu-timeout 120 --pids 512 --hit-regex 'panic|DIVERGENCE' --max-concurrency 32 --identity backend=ptrace image=demo5 -- ./workload --chaos --seed {seed}` — run a contained sweep.
- `parallel-experiment-runner plan-round --seeds 0-99 --cpu-cores 2 --memory 8G --max-concurrency 40 -- ./workload {seed}` — resolve + print one round's enforced width and DAG (dry, no cgroups).
- `parallel-experiment-runner run --spec sweep.json` — drive the sweep from a JSON/YAML spec file.

Key rules: `--cpu-timeout` is a CPU-SECOND budget (omit = UNSET; never derive it from wall time).
`--pids` sets `pids.max` (fork-bomb containment; omit = no cap). `--wall-timeout` omitted ⇒ derived
at ~3× the CPU budget, not a hardcoded default. Give at least one `--identity K=V` for reusable
cost estimates (no identity ⇒ ephemeral, not persisted). Containment is on by default; the run
refuses (exit 3) rather than run uncontained unless `--allow-cgroup-failure` is given.
