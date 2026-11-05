"""parallel-experiment-runner: run N concurrent seed-sweep workers under RESOURCE CONTAINMENT.

This is a thin, additive generalization of ``dagrun`` for the seed-sweep shape: a
single command template with a ``{seed}`` placeholder, run over a range of seeds, with each worker
contained by the SAME two-level cgroup-v2 mechanism CI steps use. The threat model is a BUG in our
OWN code — not an adversary — so this is a *resource box*, not a security sandbox: no seccomp, no
user-namespace isolation. We distrust exactly one thing about a worker: its RESOURCE USAGE.

That distrust is enforced on FOUR independent axes, each mapped to a named failure mode:

* **cpu** — 'run forever': a CPU-SECOND budget measured from the cgroup ``cpu.stat`` (NOT wall
  clock, which conflates a slow build with a spin loop).
* **memory** — 'leak memory': ``memory.max`` with OOM detection via ``memory.events``.
* **pids** — 'fork bomb': ``pids.max``. Neither ``cpu.max`` nor ``memory.max`` stops PID
  exhaustion; only a pids cap does. A breach CONTAINS the fork (EAGAIN), it does not kernel-kill,
  so the cpu/wall guard reaps the contained worker and the denied-fork count names the cause.
* **wall** — defence-in-depth BACKSTOP only, derived at ~3x the CPU budget when left unset.

The hard requirements, each realized by a specific piece:

1. CPU-TIME (not wall) budgets -> :attr:`model.WorkerLimits.cpu_timeout_s` lowered onto
   ``dagrun.Step.cpu_timeout`` (measured from the cgroup ``cpu.stat``; ``None`` = UNSET,
   never wall-derived).
2. DECLARED + ENFORCED concurrency, sized from MEASURED headroom -> :mod:`calibrate` resolves a
   width from the lane, live capacity, and measured per-worker footprint; live CPU capacity is the
   MEASURED idle-core headroom sampled from ``/proc/stat`` (not total cores, not load average, which
   counts uninterruptible-sleep/zombies), so the round runs EXACTLY that many workers.
3. Up-front ESTIMATE + measured ACTUAL -> :class:`profile.ProfileStore` (derived estimate or an
   honest UNSET) and :class:`model.RoundResult` / :class:`execute.SweepResult` (measured actuals).
4. Clean kill NAMING the breach + BY HOW MUCH -> :func:`execute._breach_message` + the breach
   statuses in :mod:`model` (including ``pids-cap``, which the reason string alone would mask).
5. KILL RECLAIMS THE NAMESPACE, not just the process. The real-world pile-up that motivated this
   tool was NOT a leaked ``tracing-appender`` thread on teardown (reproduction refuted that: five
   abandonment scenarios stranded zero workers, and the ``appender-holds-namespace`` link was a
   15-char ``comm`` truncation artifact). It was a LIVE, HUNG ``target-runner --strict --verify`` —
   its main parked in tokio ``epoll_wait`` while the guest made no progress — holding a PID
   namespace open so its zombies (which cost zero CPU and zero memory, invisible to a
   cpu-and-memory-only box) were never reaped. A hang is exactly what the per-worker cpu-time /
   wall backstop (guarantee 1) exists to kill, and the kill goes through a cgroup-subtree
   ``cgroup.kill`` (``dagrun.teardown.reap``), which SIGKILLs EVERY member of the
   step's cgroup at once — so the hung main dies WITH everything in its PID namespace, releasing
   the namespace and letting the kernel reap the zombies, instead of killing one pid and
   inheriting the orphans. Containment is the fix; no separate teardown reaper is required.

The programmatic entry point is :func:`execute.run_sweep`; the CLI is :func:`cli.main`.
"""

from __future__ import annotations

__version__: str = "0.3.1"

__all__ = ["__version__"]
