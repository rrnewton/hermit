# CPA planner specification for dagrun

The `--planner cpa` mode uses measured speedup curves to allocate each step's inner-parallelism
width. This document gives the allocation rationale and the authoritative algorithm. The behavioral
differential compares the implementations' chosen widths, order, capacity-model lower bound,
no-overcommit reference makespan, and stop reason byte-for-byte.

The runtime deliberately permits oversubscription: `--max-steps`, dependencies, memory sizing, and
named resources decide which nodes overlap, while the outer cgroup makes them share `P` CPU
core-equivalents. CPA does **not** yet model the slowdown or benefit of a particular co-running set.
Its area/lower-bound/reference-schedule values describe a no-overcommit capacity model used to pick
widths and order; they are not a prediction or guarantee for the opportunistically overlapping run.

## 1. The problem, in plain language

We run a directed acyclic graph (DAG) of CI/build steps on **one machine** with a fixed budget of
`P` CPU cores (a cgroup CPU quota, or a CPU-affinity width). Steps are connected by **precedence**
edges: a step cannot start until every step it depends on has finished successfully.

Each step is **moldable**: we can hand it a number of inner worker threads `p` (its `-j` value —
e.g. `make -j p`, `cargo test -j p`, `pytest -n p`) *once, at launch*, and its wall-clock time then
follows a **measured** curve `T_i(p)` — the wall time of step *i* as a function of the cores we give
it. "Moldable" (fix the width at launch) is the middle of the three standard classes:

- **rigid** — the width is fixed in advance and the scheduler cannot change it;
- **moldable** — the scheduler picks the width once, at launch, and it stays fixed for the run;
- **malleable** — the width can change *while the step is running*.

We are moldable: a build's `-j` is chosen when we spawn it and does not change mid-flight.

The goal is to **minimize the makespan** — the wall-clock time from the first step starting to the
last step finishing — subject to five constraints that all coexist on the one machine:

1. **Precedence.** The DAG edges (`Step.deps`).
2. **A shared CPU-bandwidth budget `P`.** `run -j P` / `run --max-cpus P` sets the whole run's
   outer capacity to `P` core-equivalents and caps each runner-controlled step's width at `P`.
   Declared widths are not reservations: several active steps may request more than `P` in sum and
   contend inside the outer quota. When omitted, `P` is the conservative whole-core floor of the
   tightest binding ancestor `cpu.max`, capped by process affinity (and by `--cores K` when used).
   The default is also tightened by the shared aggregate slice's conservative 90% host budget,
   which prevents concurrent runner invocations from each assuming the whole machine.
   The boxed run scope also gets `CPUQuota=P*100%` as a bandwidth backstop. CFS quota is not an
   instantaneous CPU-identity/thread-count cap; exact eligible CPU identities are the separate
   `--cores K` cpuset feature.
3. **Per-step memory caps.** Each step has a modeled peak memory footprint; the worst-case sum over
   any set of steps that can co-run must fit a RAM budget. The reachable-concurrent-set model
   (`schedulable_peak_mem_bytes`) uses an exact-width measured peak when sufficiently replicated,
   otherwise the conservative width-scaled fallback. Exact-width evidence must be uncensored; a
   peak reached under an active ceiling is only a lower bound. Memory is therefore a function of
   width, not one scalar silently reused at every allocation.
4. **An active-step ceiling.** `run --max-steps S` independently bounds how many DAG nodes may be
   active. It defaults to `P`; `--max-mem` may derive a tighter ceiling.
5. **Named scarce-resource semaphores.** Arbitrary caller-named capacities in `DagConfig.resource_caps`
   (e.g. `{"browser": 1}`) bound how many demanders co-run — which in turn bounds the *achievable*
   concurrency and therefore how much of `P` can actually be kept busy.

The planner composes three parts:

- **Measured curves.** `load_step_speedups` reads the profile store and produces, per
  step, a `StepSpeedup` with one `SpeedupLevel` per measured width — carrying the robust
  (contention-discounted, outlier-trimmed) `wall_s`, the **total CPU-seconds** `cpu_s = user_s + sys_s`
  (a *work-conservation* signal), `effective_cores`, `throttled_s`, and the `speedup` versus the
  smallest measured width. It also computes a **per-step** `recommended_inner_jobs`: the narrowest
  width within 10% of the best eligible measured wall, normally excluding widths whose CPU work is
  more than 1.5 times the baseline width's work.
- **The list scheduler.** `Runner` is a greedy ready-set list scheduler: it launches
  every ready step (deps met, below `--max-steps`, and with named resources free) in
  a caller-supplied dispatch order, and `--planner critical-path` supplies a
  *critical-path-first* order (highest bottom-level first). This is exactly the "list-scheduling"
  back half of a two-phase moldable scheduler.
- **The allocator.** It picks each step's width `p_i` from isolated measured curves, then hands
  those widths and the allocation-aware ordering to the list scheduler. Unlike an isolated
  per-step plateau, the allocation accounts for the DAG's critical path and a no-overcommit area
  model. It does not account for the exact sibling set or slowdown when runtime widths overlap.

## 2. The makespan lower bound that drives everything

For a DAG scheduled on `P` processors, any schedule's makespan `T` obeys two independent lower
bounds (Graham 1969; Brent 1974):

- **The critical-path (span) bound.** `T >= T_CP`, where `T_CP` is the length of the longest chain of
  dependent steps, each weighted by its chosen wall time `T_i(p_i)`. You cannot finish faster than
  the longest dependency chain, no matter how many cores you have.
- **The work/area bound.** `T >= A / P`, where each step contributes measured CPU service
  `C_i(p_i) = user_s + sys_s` when that signal is available. Older or unboxed samples without CPU
  accounting conservatively contribute `p_i * T_i(p_i)` instead. Thus
  `A = sum_i C_i(p_i)` is the best available estimate of total core-seconds of work at the chosen
  widths, and `P` is the core budget. You cannot finish faster than the total work spread perfectly
  across `P` cores.

So `T >= max(T_CP, A/P)`. Brent's theorem is the matching upper direction: a greedy (list) schedule
achieves `T <= T_1/P + T_inf` — within a factor ~2 of the max of the two terms (Graham's `2 - 1/P`
list-scheduling guarantee is the same phenomenon).

This inequality is the *entire* reason a moldable allocator is subtle, and why the implemented
algorithm has the shape it does:

- Widening one step (raising `p_i`) **shrinks `T_i(p_i)`**, which can shorten `T_CP` — good, *if that
  step is on the critical path*.
- But widening commonly **grows the measured CPU area** `A`: synchronization, scheduling, cache,
  and redundant-work costs increase `user_s + sys_s` even as wall time falls. Where direct CPU
  measurements are unavailable, the conservative `p_i * T_i(p_i)` fallback captures the same
  pressure. A larger `A` raises `A/P` — bad.

The two terms pull in opposite directions. The optimum of the two-term bound sits where they
**balance**: `T_CP ~= A/P`. Below that point the critical path dominates and you should spend cores to
shorten it; above it the area dominates and extra cores are wasted. An allocator that walks the
allocation toward `T_CP ~= A/P` is directly minimizing the quantity that lower-bounds (and, via
Graham/Brent, upper-bounds within ~2x) the makespan. This is precisely what CPA does.

## 3. Related work

### 3.1 Speedup models: analytic vs. measured

Classic moldable scheduling assumes an *analytic* speedup function `T_i(p)`:

- **Amdahl (1967)** — a fixed serial fraction caps speedup; `T(p) = T(1)·(s + (1-s)/p)`. Pessimistic
  for CI steps whose serial fraction is small but real (link steps, test-runner startup).
- **Downey (1997), "A model for speedup of parallel programs" (UC Berkeley TR CSD-97-933)** — a
  two-parameter model (average parallelism `A` and a variance `sigma`) giving a smooth, concave,
  saturating speedup curve. Downey's model is the canonical *parametric* stand-in when you have no
  measurements: near-linear up to `~A`, then a knee, then a plateau.

**We do not need to assume an analytic model — we measure the curve.**
`load_step_speedups()` gives `T_i(p)` as a set of real, robust,
contention-discounted points at the widths we have actually run
(`SpeedupLevel.wall_s`), plus the total-work signal `cpu_s`. The literature's analytic models exist
to *approximate* exactly the object we have empirically. This is our single biggest deviation from
the textbook (see §6) and it is a strength: the concavity, the plateau location, and the
work-conservation break are observed, not assumed. Amdahl and Downey remain useful as sanity checks
on measured-curve shape; the allocator does not synthesize missing wall points from either model.

The plateau definition is global and grid-invariant. Among measured widths within the core budget,
prefer widths whose CPU work is no more than `1.5x` the baseline width's CPU work, find the best
wall time in that eligible set, and choose the **narrowest** width no more than 10% slower than that
best.
For example, 7.2x speedup at eight workers is within 10% of a best-observed 7.9x at 64, so eight is
the economic recommendation. Inserting a new midpoint cannot change this merely by changing two
adjacent ratios. If CPU accounting is absent, that width is not penalized on invented data; if the
CPU filter would remove every within-budget point, the within-budget curve remains usable rather
than disappearing.

### 3.2 The area/critical-path allocation lineage

- **Belkhale & Banerjee (1990)** introduced allocating processors to *independent* moldable tasks by
  balancing per-task area, an early "area-aware" allotment heuristic.
- **Turek, Wolf & Yu (1992, SPAA), "Approximate algorithms for scheduling parallelizable tasks"** and
  **Ludwig & Tiwari (1994, SODA), "Scheduling malleable and nonmalleable parallel tasks"** established
  the **two-phase paradigm** for *independent* malleable tasks: **phase 1** chooses an *allotment* (a
  width per task), **phase 2** *list-schedules* the now-rigid tasks. Turek-Wolf-Yu give a 2-approximation
  by searching over a candidate makespan and solving the induced allotment; Ludwig-Tiwari improve the
  allotment step (a `(2+eps)` / practical improvement) using the tasks' *work* functions. The
  structural lesson — **allot, then list-schedule** — is exactly the architecture we adopt. The
  caveat: their approximation guarantees are for **independent** tasks (no precedence). Our DAG has
  precedence, so we borrow the *structure*, not the bound.
- **Radulescu & van Gemund (2001, ICPP), "A Low-Cost Approach towards Mixed Task and Data Parallel
  Scheduling" — CPA.** CPA brings the two-phase idea to *precedence-constrained* DAGs with a cheap,
  greedy allocation phase built directly on the `max(T_CP, A/P)` bound above. Its allocation phase:
  start every task at one processor; while the critical-path length exceeds the average area per
  processor (`T_CP > T_A`, with `T_A = A/P`), pick the task **on the current critical path** whose
  extra processor helps most and give it one more; recompute; repeat until `T_CP <= T_A`. Then
  list-schedule. CPA is O(cheap) and needs no makespan guess — it just walks to the balance point of
  the two-term bound. **This is the closest published match to our problem and the basis of the
  implementation.**
- **Bansal, Kumar & Singh (2006, Parallel Computing), "An improved two-step algorithm for task and
  data parallel scheduling in distributed memory machines" — MCPA (Modified CPA).** MCPA fixes CPA's
  main blind spot: CPA can over-allocate a critical-path task even when its *siblings* (tasks that
  will run at the same time) could have used those processors concurrently. MCPA makes the allocation
  **level/concurrency aware** — it limits a task's width by how many tasks are actually runnable
  alongside it. On a *small-`P` single machine*, where oversubscription directly causes CPU
  throttling, this insight matters. The current implementation does not implement MCPA's
  level-aware allocation; it is a roadmap direction once co-running measurements exist.

### 3.3 Approximation theory for the precedence case

- **Jansen & Zhang (2005 ISAAC / 2006 ACM TALG), "An approximation algorithm for scheduling malleable
  tasks under general precedence constraints"** give a constant-factor approximation (`~4.73`, improved
  in later work) for *exactly* our theoretical problem — malleable tasks with a general DAG. Together
  with **Jansen (2004, Algorithmica), the malleable-task FPTAS/AFPTAS line**, these results are the
  theoretical backdrop that *validates* optimizing the two-term `max(T_CP, A/P)` objective: they show
  it is the right quantity and that constant-factor guarantees are attainable. They are, however, LP-
  or dual-approximation-based, heavier than we need for CI DAGs of tens of steps, and they assume
  analytic monotone work functions. We treat them as justification, not as the implementation.

### 3.4 List-scheduling ORDER heuristics (phase 2)

The allocation phase decides *widths*; the list-scheduling phase decides *order among ready steps*.
The order literature is where our existing `--planner` lives:

- **HLFET (Adam, Chandy & Dickson, 1974, CACM), "A comparison of list schedules for parallel
  processing systems"** — Highest Level First with Estimated Times: order ready tasks by their
  **static bottom-level** (longest remaining weighted path to a sink). This is *exactly* our
  `--planner critical-path` (`estimates._bottom_levels` / `_plan_order`).
- **HEFT (Topcuoglu, Hariri & Wu, 2002, IEEE TPDS 13(3)), "Performance-Effective and Low-Complexity
  Task Scheduling for Heterogeneous Computing"** — upward-rank ordering plus an earliest-finish-time
  processor choice. HEFT is the most-cited DAG scheduler, **but it targets HETEROGENEOUS processors**:
  its cleverness is choosing *which* (different-speed) processor runs each task. **We are homogeneous**
  — one pool of identical cores — so HEFT's processor-selection half is moot for us; its ordering half
  reduces to the same bottom-level/upward-rank idea we already use. We cite HEFT to explain *why we do
  not adopt it wholesale*: its core contribution does not apply to a single homogeneous machine.

## 4. Implemented algorithm

The planner uses a **CPA-style critical-path / area-balancing moldable allocator**, driven by measured
`T_i(p)` curves. It feeds the critical-path list scheduler, with memory caps and the per-step width
ceiling `P` as hard constraints on allocation. Runtime overlap is opportunistic rather than part of
the allocator's model.

Greedy-LPT and critical-path choose only ready-step order from scalar estimates. CPA jointly
searches measured inner widths and memory-feasible active-step ceilings, but it still evaluates
those choices with isolated per-step curves. Profile rows do not identify the exact sibling set and
aggregate requested width that overlapped each sample. A future contention-aware planner needs that
context (plus width-specific CPU-demand estimates) to predict how two simultaneously active wide
steps share `P`; the current no-overcommit reference can choose serial versus concurrent capacity,
but cannot predict co-run interference or phase complementarity.

### 4.1 Raw dataset and derived sidecar

The authored DAG contains policy and portable hints, never a machine-specific fitted curve. Raw
per-step CSV rows under the profile store are the source of truth and are partitioned by machine and
container identity. They retain raw and contention-adjusted wall evidence, CPU work, achieved
parallelism, throttling, memory peaks, and the sweep provenance needed to distinguish passes and
repeats.

An opt-in interval sampler writes separate `traces/<run_id>.csv` files with start, periodic, and
final cgroup CPU/thread observations. These traces expose phase behavior inside one trial, but they
are diagnostic evidence rather than additional estimator samples: the aggregate
`step_profiles_*.csv` rows remain the model's input.

Each sweep row carries a stable digest of the step command, command type, width-injection channel,
and environment. Planning selects the current DAG's digest cohort exclusively once one exists;
identified rows from another command shape are never blended into it. Blank rows written before
digest tracking are used only while no exact cohort exists. Portable summary schema 2 makes the
digest part of each bounded `(step, width, digest)` reservoir, preventing a stale cohort from
evicting current samples during per-width subsampling; schema 1 is migrated as blank-digest data.

`load_step_speedups` rebuilds the model from those rows. After a successful
profiling-enabled sweep, `write_scaling_model` atomically refreshes the
deterministic JSON form outside the DAG at
`scaling_model_<machine_id>_<container_class>.json`; it is an inspectable,
replaceable cache, not a second source of policy. Its schema records the plateau
tolerance, CPU-work-growth guard, memory replication threshold,
recommended and regression widths, and every fitted level. Deleting it loses no measurement: the
raw CSV can recreate it. Each saved step also names the selected workload digest.

It has two phases (the Turek-Wolf-Yu / Ludwig-Tiwari structure):

- **Phase 1 — allocation:** a CPA-derived gradient loop picks each step's width `p_i`,
  snapping to the widths we have actually measured, balancing `T_CP` against `A/P` and stopping at
  each step's economic plateau or when a constraint binds.
- **Phase 2 — dispatch ordering:** recompute bottom-levels using the *allocated* weights
  `T_i(p_i)` and order ready steps by bottom-level (our `critical-path` planner). The live scheduler
  then admits ready work under `--max-steps`, dependencies, and named-resource gates; the outer CPU
  quota arbitrates overlapping widths.

### Why this over the alternatives

- **vs. per-step greedy-plateau.** `recommended_inner_jobs` optimizes each step *in isolation* — the
  narrowest economic near-best wall, capped at `P`. That is useful for one step but is DAG-blind and
  machine-blind: an off-path scaling step can receive a wide allocation that does not shorten the
  makespan, and independently chosen widths can collectively exceed `P`. CPA spends cores where they
  shorten the whole-DAG critical path. The measured plateau remains the upper bound when a curve
  exists; a curveless step stays rigid at its configured hint or one core (see §5).
- **vs. pure two-phase (Turek-Wolf-Yu / Ludwig-Tiwari).** Their guarantees assume **independent**
  tasks; our DAG has precedence, so the bound does not transfer, and their allotment search (binary-
  search a makespan, solve a knapsack-like allotment) is heavier and assumes analytic work functions.
  CPA gives us the same allot-then-schedule structure with a **precedence-aware**, guess-free,
  O(cheap) allocation loop that reads straight off our measured curves. We keep their architecture and
  drop their independence assumption.
- **vs. MCPA in full.** MCPA's concurrency-awareness is genuinely useful at small `P`, but it needs
  a model of what co-runs and how sharing changes each task. The current planner compares
  memory-feasible active-step ceilings around CPA's global CP/area loop, but it does not claim
  MCPA-style co-run slowdown modeling.
- **vs. Jansen-Zhang / Jansen FPTAS.** Constant-factor-optimal but LP-based, complex, and reliant on
  analytic monotone work functions — overkill for tens-of-steps CI DAGs and hard to make deterministic
  and dependency-light. We cite them as the theory that says our objective is the right one.

## 5. Algorithm specification

All arithmetic and tie-breaks are specified so both implementations agree bit-for-bit. The
behavioral differential compares plan output byte-for-byte.

### 5.1 Inputs

- `cfg: DagConfig` — the DAG, `resource_caps`, and memory tunables.
- `speedups: Mapping[tag, StepSpeedup]` — measured curves from `load_step_speedups()`. Each carries
  ascending `levels` with `inner_jobs`, `wall_s`, `cpu_s` (may be `None`), and the per-level
  `speedup`.
- `est: Mapping[tag, float]` — the resolved scalar duration per step (store-over-hint-over-default;
  ordinary memory feedback separately treats an authored RSS baseline as a floor),
  as `build_plan` already computes. Used when a step has no measured curve and for a self-managed
  fixed width whose curve has no exact point at that width.
- `P: int | None` — an explicit run uses its resolved total `--max-cpus` budget (including the
  inherited container/affinity and shared-slice tightening used by the CLI). Standalone CPA uses
  `container_core_budget()` because allocation requires a bound. Standalone greedy-LPT and
  critical-path plans have no run boundary, leave `P=None`, and do not bound display-only speedup
  recommendations.
- `mem_budget: int | None` — the RAM budget (as `--max-mem` already supplies to `jobs_for_budget`);
  `None` disables the memory constraint on allocation.
- `S: int | None` — the requested `--max-steps` ceiling. CPA may model a smaller overlap when the
  joint reachable footprint does not fit `mem_budget`, but never a larger one.

### 5.2 Per-step admissible widths and the measured wall function

For each step `i`, define its **admissible width set** `W_i`:

- If `i` is an intentional pre-execution skip, `W_i = {1}` with zero wall, CPU-area, and memory
  demand. It remains visible in plan output as `est_source=skip` but cannot suppress allocation for
  runnable work.
- If `i` has `cmdtype: unknown` and empty/whitespace-only effective `jobs_flag` **and** `jobs_env` channels, its command
  manages a **fixed** width.
  The run refuses before process creation when its positive declared `preferred_inner_jobs > P`;
  otherwise `W_i` contains that declared width. With no positive declared width, the configured
  default is only a runner/cgroup cap (not a hidden guest worker count) and may be tightened to
  `P`. CPA never pretends it can
  resize a command that opted out of both injection channels. The legacy `sweep --step --jobs`
  form rejects such a step; a target-time graph sweep characterizes a fitting configured width once
  and visibly skips one wider than the effective machine budget. Pure CPA planning retains an
  over-budget fixed width and reports `infeasible-fixed-width`; its modeled
  makespan is infinity and no `alloc_inner_jobs` is published for that self-managed step. If the
  learned curve contains an exact level at the fixed width, its measured wall is used; otherwise
  the scalar resolved estimate remains the weight and the curve is diagnostic only.
- If `i` has a measured curve, begin with the measured widths no greater than
  `speedup.recommended_inner_jobs`, then keep those no greater than `P`. This reuses the
  global economic plateau (wall within 10% of the best eligible point and total CPU seconds
  normally no more than `1.5x` baseline) so the allocator cannot widen past the useful
  measurements. If every measured width exceeds `P`,
  that curve is unavailable at this budget; the step remains rigid at its authored width (or one),
  capped to `P`, using the scalar estimate rather than claiming an unexecutable measured point.
- If `i` has **no** measured curve, `W_i = { w_i }`, where `w_i` is the positive
  `preferred_inner_jobs` hint, else `default_step_cpu_count`, else `1`, capped at `P`. The step is
  rigid because no data justifies widening it.

Define the **measured wall** `T_i(p)` for `p in W_i` as that level's `wall_s`; for a curveless step,
`T_i(w_i) = est[i]`. `T_i` is only ever evaluated at admissible widths — **no interpolation or
extrapolation**, so allocations always sit on real measurements (a deliberate deviation, §6).

Sort each `W_i` ascending. Let `p_i` be the step's *current* allocation (an index/value into `W_i`);
let `next(p_i)` be the next-larger admissible width, or `None` if `p_i` is already the largest.

### 5.3 Seed the initial allocation

Set `p_i := min(W_i)` for every step — i.e. the **narrowest** admissible width (usually 1). This is
CPA's "start everyone at one processor" seed. Rationale: starting minimal and *growing* toward the
balance point makes each allocation index monotone and the loop finite; it also makes the result
independent of prior allocation state. Measured CPU work can move either way between noisy width
points, so the implementation does not claim the numeric area itself is monotone.

### 5.4 Derived quantities (recomputed each iteration)

- **Weighted bottom-levels.** Run the existing `_bottom_levels` with the *current* weights
  `w(i) = T_i(p_i)`. This yields, for every step, the longest remaining dependency chain in wall time
  at the current allocation.
- **Critical path and its length `T_CP`.** The existing `_critical_path` over those bottom-levels
  (start at the max bottom-level, follow max-bottom-level successors; ties by tag ascending). `T_CP`
  is the length; `CP` is the set of steps on it.
- **Area and the area term.** Let `C_i(p_i)` be the robust measured `user_s + sys_s` at the exact
  width when present, else the conservative fallback `p_i * T_i(p_i)`. Then
  `A = sum_i C_i(p_i)` and `T_A = A / P`. Intentional skips contribute zero.

### 5.5 The gradient step (which step to widen, and by how much)

While `T_CP > T_A` **and** at least one critical-path step can still be widened within all
constraints:

1. **Candidate set.** Consider only steps `i in CP` with `next(p_i) != None` (a wider admissible
   width exists).
2. **Marginal gain (the selection metric).** For each candidate, using the *measured* curve, compute
   the **actual** wall reduction from the next width and the cores it costs:

   ```
   delta_wall(i) = T_i(p_i) - T_i(next(p_i))          # measured signed wall change
   delta_cores(i) = next(p_i) - p_i                    # > 0
   gain(i) = delta_wall(i) / delta_cores(i)            # signed wall change per added core
   ```

   `gain` is reduction-per-added-core and can be negative for a locally noisy or regressing next
   point. It uses the real measured change, which is *strictly more
   accurate* than CPA's original `T_i(p_i)/p_i` proxy (that proxy assumes near-linear speedup; we have
   the curve, so we use it).
3. **Constraint filter — a candidate is admissible only if widening it keeps every constraint
   satisfied** (see §5.6). If widening `i` would violate a constraint, `i` is dropped from the
   candidate set. Since widths only grow, a core- or memory-blocked widening remains blocked.
4. **Pick and apply.** Choose the admissible candidate with the **greatest `gain`**; break ties by
   **smallest tag** (ascending, matching the existing critical-path tie-break). Set `p_i := next(p_i)`.
5. **Recompute** §5.4 and loop.

### 5.6 How the constraints bind the allocation

A tentative widening `p_i -> next(p_i)` is rejected unless **all** hold:

- **Total core budget.** A tentative wider point must not exceed `P`: `next(p_i) <= P`. There is no
  wider-than-budget escape or "run alone" exception.
- **Memory response is exact-width when sufficiently replicated.** At a measured width
  with at least three **uncensored** peak observations, use that width's nearest-rank
  90th-percentile `memory.peak` directly as `M_i(p)`. A capped peak remains a lower bound and can
  raise the requirement, but is never treated as exact demand. The measurement already describes
  that width, so it carries width provenance and is not multiplied by the fallback width heuristic
  again. With fewer than three exact peaks, or no exact-width point, fall back to the ordinary
  pooled/profile or authored RSS baseline and
  `step_mem_cap_for_inner_jobs` (a CPU-bound fallback stays flat through four workers and then
  scales approximately as `base * p / 4`). An authored `hard_mem_max_bytes` remains authoritative.
  Before widening, compute the largest dependency- and resource-compatible concurrent footprint
  at the candidate widths. `--max-steps` is a ceiling, not a required overlap. With a memory budget,
  run the fixed-overlap CPA gradient once for every ceiling from the requested maximum down to one,
  discard candidates whose narrow seed cannot fit, and score each survivor with the deterministic
  no-overcommit reference schedule. Select the smallest modeled makespan; exact ties retain the
  larger overlap. This prevents a barely feasible many-step seed from trapping every task at a
  narrow width when fewer, wider concurrent tasks finish sooner. If no one-step seed fits, the
  allocation is infeasible. Hard-cap-only, default-capped, and selected `engine_only` steps
  participate; intentional skips do not. `mem_budget` comes from `--max-mem`; absent, the memory
  constraint is off and only the requested ceiling is evaluated. Exact
  dependency/resource-compatible subset enumeration in the later active-step sizing pass is capped
  at 100,000 candidates; wider searches conservatively sum the largest caps.
- **Named-resource feasibility is unchanged by width** (widths do not change `hint.resources`), so
  named-resource caps never *block a widening*. They still serialize live steps that demand the
  same resource, but CPA does not model that serialization in its area term.
- **Economic plateau already enforced** by `W_i` truncation (§5.2): widths above the recommended
  point are not admissible. The CPU-work filter normally bounds that recommendation; when it would
  remove every within-budget point, the within-budget set is retained rather than erasing all
  evidence.

### 5.7 Handing allotments to the runtime

The allocator outputs `p_i` per step and passes the result to the runner:

1. **Bake executable widths in.** For each feasible runner-controlled step, produce a `DagConfig`
   copy whose `Step.hint.preferred_inner_jobs = p_i` (analogous to `apply_plan_to_config`). This
   flows through `command_with_inner_jobs` and/or `env_with_inner_jobs` (the width actually handed
   to the step) and through `step_mem_cap_for_inner_jobs` (the memory cap). Self-managed steps retain their declared width
   and have `alloc_inner_jobs = null`; intentional skips retain their authored hints when a plan is
   applied.
2. **Order.** Compute the same order as the critical-path planner, using the *allocated* weights
   `T_i(p_i)` rather than the width-one or hinted weights. Highest weighted bottom-level first, ties
   by tag.
3. **Permit bounded oversubscription.** The ready-set loop launches a ready step when dependencies
   are met, named resources fit, and fewer than `S = --max-steps` nodes are active. It does **not**
   subtract `p_i` from a shared admission-token pool. Runner-controlled authored widths above `P`
   are still visibly capped before planning and execution, including the command's appended jobs
   flag or jobs environment variable and per-step `cpu.max`; a self-managed fixed width above `P` is infeasible and cannot be
   executed. Multiple legal widths may therefore sum above `P`, with the verified outer `cpu.max`
   providing the actual shared-bandwidth boundary.

The result: CPA selects useful individual widths and a critical-path-aware order, while runtime may
overlap those widths beyond `P`. That can improve throughput for stalls and phase mismatch or lose
throughput to contention; current profiles do not supply enough co-run context for CPA to predict
which outcome will occur.

### 5.8 Stop conditions (any one ends the loop)

- **Balance reached:** `T_CP <= T_A`. The critical path no longer dominates the two-term bound, so
  this CPA loop has no reason to widen further. The planner reports this stop reason as `balanced`.
- **No admissible candidate:** every critical-path step is already at its plateau-truncated max
  width, at `P`, or blocked by the memory budget. Additional cores cannot help the current critical
  path. The planner distinguishes three sub-cases in the reported stop reason: `knee-exhausted` (no
  critical-path step has any wider admissible width left — including the per-step `P` cap, which is
  enforced by construction via the `W_i` truncation in §5.2), and `mem-capped` / `core-capped` (a
  wider width exists but every candidate was rejected by the memory budget / the core cap this
  iteration). Runner-controlled `W_i` sets are truncated to widths `<= P`, so `core-capped` is a
  defensive backstop that does not arise in practice; the P-cap surfaces as `knee-exhausted`.
  A self-managed fixed width above `P` follows the explicit infeasible case below instead.
- **Fixed-point safety net:** if the bounded loop ever exhausts without another classified stop,
  report `fixed-point`.
- **Infeasible fixed width:** a non-skipped self-managed command declares a width above `P` that
  CPA cannot rewrite. Preserve that width, publish no executable per-step allocation, and report
  `infeasible-fixed-width` with an infinite modeled makespan. Run entry points reject the same
  configuration before a DAG step can start.
- **Infeasible memory:** at least one runnable step's narrowest width-aware footprint (including
  the run-level floor and safety factor) exceeds `mem_budget`. Report `infeasible-memory`, publish an infinite
  no-overcommit reference makespan and no executable allocation, and refuse a run before any DAG
  step starts. The low-level allocator returns its typed infeasibility error instead of an
  executable-looking width map.

Because every applied step strictly increases some `p_i` within a finite `W_i`, the loop runs at
most `sum_i (|W_i| - 1)` iterations — trivially bounded even when measured CPU area is not monotone.

### 5.9 Cross-implementation determinism

The behavioral differential compares plan output byte-for-byte, so the allocator must be
deterministic and identical across implementations:

- **Canonical floating-point operations.** Compute `gain(i)`, `T_CP`, and `T_A` with the same IEEE-754
  operations in the same order. Plan output formats seconds to three decimals, but comparisons use
  the unrounded values.
- **Total tie-break order.** When two candidates have equal `gain`, pick the smallest tag. Critical
  path and dispatch ordering use the equivalent `(-bottom_level, tag)` order.
- **Canonical iteration order.** Iterate steps in `cfg` registration order for the area sum and use
  a tag-ascending scan that keeps the first maximum for the gradient tie-break.
- **Integer core arithmetic.** `delta_cores` and `P` are pure integers.

The differential checks the resulting plan JSON and text, including chosen widths, area,
critical-path length, lower bound, modeled makespan, and stop reason.

The modeled makespan reported alongside the lower bound is a deterministic **no-overcommit
reference** list-schedule of the allocated widths: it respects DAG dependencies and packs widths
within `P`, so it is `>=` the capacity model's `max(T_CP, area/P)` lower bound. The live scheduler
does not enact that packing. Its actual makespan may be better or worse because concurrent widths
share the outer quota and the model has no co-run slowdown term.

The `plan` and `--show-plan` output includes the allocator's stop reason, core budget,
critical-path length, area term, lower bound, and modeled makespan. It includes
`alloc_inner_jobs` only for runner-controlled CPA steps; ordering-only planners and self-managed
fixed commands use null. Plan JSON exposes the same information in its `allocation` object. For
`run --show-plan`, every planner bounds
displayed recommendations by the resolved run `--max-cpus` value. Standalone ordering-only plans
have no run boundary and leave recommendations unbounded; standalone CPA uses the effective
ambient budget because it must allocate widths.

## 6. Our deviations from the textbook

1. **Empirical curves, not analytic models.** CPA/Jansen assume an analytic `T_i(p)` (Amdahl- or
   Downey-shaped, monotone, concave). We read `T_i(p)` from the **measured** profile store. We
   therefore (a) evaluate `T_i` only at measured widths — no interpolation/extrapolation, so an
   allocation is always backed by a real measurement; and (b) get the plateau and work-conservation
   break *observed*, not assumed. A step with too few samples remains rigid instead of receiving a
   synthetic curve.
2. **Single machine, inner-jobs, cgroup-boxed — not a distributed multiprocessor.** The "processors"
   a task receives are inner `-j` threads inside one machine's cgroup CPU budget, not nodes of a
   cluster. There is no data-redistribution or communication cost term (the classic CPA/mixed-parallel
   concern); the cross-step coupling is **shared CPU, RAM, and named semaphores** on the one box.
   The outer CPU quota enforces aggregate bandwidth, but the planner does not model contention
   between a particular set of overlapping steps.
3. **Two extra constraint classes the textbook omits.** Standard moldable scheduling constrains only
   processors. We additionally constrain **per-step memory** (and, crucially, an empirical or
   conservatively scaled memory response at each width) and **named scarce-resource semaphores**.
   Both can *block a widening*
   or *bound achievable concurrency* — first-class inputs to the allocator, not afterthoughts.
4. **A work-conservation (efficiency) filter, not just a wall-time optimum.** Textbook CPA stops on
   the pure `T_CP <= T_A` balance. We normally exclude a width from the per-step economic plateau
   when its measured `cpu_s` exceeds `1.5x` the baseline width's CPU work, because on a shared
   machine burning extra core-seconds for a marginal wall gain steals capacity from other useful
   work. The allocator's area term also uses measured CPU seconds directly, falling back to
   `p*T(p)` only when they are unavailable. This measured-work treatment is not part of classical
   CPA.
5. **Determinism as a hard requirement.** The classical algorithms are described over reals with
   arbitrary tie-breaking. Both implementations require **bit-for-bit** identical decisions, so
   operations use canonical ordering and a total tag tie-break (§5.9).

## 7. Verification contract

- **Behavioral differential.** Multi-width fixtures cover a scaling critical-path step, an off-path
  plateau, and a memory-heavy step. The harness compares widths, order, modeled values, and stop
  reason across implementations.
- **Unit tests.** Tests cover a balanced linear chain, plateau avoidance, memory-blocked widening,
  opportunistic runtime overcommit, rigid curveless steps, infeasible self-managed widths,
  plan-application refusal preservation, and allocator idempotence.
- **Synthetic graph benchmark.** `examples/07-graph-scaling-sweep.yaml` contains an embarrassingly
  parallel node, a node that saturates at four workers, and a sequential node whose extra workers
  add interference. Every command accepts the generic `--jobs` channel, so the full target-time
  sweep exercises topology order, plateau detection, CPU-work growth, and width-specific memory.
- **Selection.** `cpa` is explicit; the default remains `greedy-lpt`. `--no-profile-feedback`
  removes measured curves, so curveless steps remain rigid.

## 8. References

Load-bearing citations; web-confirmed where noted.

1. G. M. Amdahl (1967). "Validity of the single processor approach to achieving large scale computing
   capabilities." *AFIPS Spring Joint Computer Conference*, pp. 483–485.
2. R. L. Graham (1969). "Bounds on multiprocessing timing anomalies." *SIAM Journal on Applied
   Mathematics*, 17(2):416–429. (List-scheduling `2 - 1/P` bound; the `max(T_CP, work/P)` intuition.)
3. R. P. Brent (1974). "The parallel evaluation of general arithmetic expressions." *Journal of the
   ACM*, 21(2):201–206. (The `T_p <= T_1/p + T_inf` work/span bound.)
4. T. L. Adam, K. M. Chandy, J. R. Dickson (1974). "A comparison of list schedules for parallel
   processing systems." *Communications of the ACM*, 17(12):685–690. (HLFET — bottom-level ordering.)
5. K. P. Belkhale, P. Banerjee (1990). "An approximate algorithm for the partitionable independent
   task scheduling problem." *International Conference on Parallel Processing (ICPP)*. (Early
   area-balancing allotment.)
6. J. Turek, J. L. Wolf, P. S. Yu (1992). "Approximate algorithms for scheduling parallelizable
   tasks." *4th ACM Symposium on Parallel Algorithms and Architectures (SPAA)*, pp. 323–332.
   (Two-phase allot-then-schedule; 2-approximation for independent malleable tasks.)
7. W. Ludwig, P. Tiwari (1994). "Scheduling malleable and nonmalleable parallel tasks." *5th ACM-SIAM
   Symposium on Discrete Algorithms (SODA)*, pp. 167–176. (Improved allotment via work functions.)
8. A. B. Downey (1997). "A model for speedup of parallel programs." Technical Report UCB/CSD-97-933,
   EECS Department, University of California, Berkeley. (Web-confirmed: report CSD-97-933.)
9. A. Radulescu, A. J. C. van Gemund (2001). "A Low-Cost Approach towards Mixed Task and Data Parallel
   Scheduling." *30th International Conference on Parallel Processing (ICPP)*, pp. 69–76. (**CPA** — the
   critical-path/area-balancing moldable allocator we base this design on.)
10. H. Topcuoglu, S. Hariri, M.-Y. Wu (2002). "Performance-Effective and Low-Complexity Task Scheduling
    for Heterogeneous Computing." *IEEE Transactions on Parallel and Distributed Systems*,
    13(3):260–274. (**HEFT** — cited to explain why its heterogeneous-processor core does not apply to
    our homogeneous single machine.)
11. K. Jansen (2004). "Scheduling malleable parallel tasks: An asymptotic fully polynomial-time
    approximation scheme." *Algorithmica*, 39(1):59–81. (Malleable-task approximation theory.)
12. K. Jansen, H. Zhang (2005/2006). "An approximation algorithm for scheduling malleable tasks under
    general precedence constraints." *ISAAC 2005*; journal version *ACM Transactions on Algorithms*,
    2(3):416–434, 2006. (Web-confirmed title; constant-factor approx for precedence-constrained
    malleable tasks — our theoretical backdrop.)
13. S. Bansal, P. Kumar, K. Singh (2006). "An improved two-step algorithm for task and data parallel
    scheduling in distributed memory machines." *Parallel Computing*, 32(10):759–774. (**MCPA** —
    concurrency-aware CPA; web-confirmed.)

## 9. Implementation map

- Curves: `load_step_speedups`, `StepSpeedup`, `SpeedupLevel`, and `_build_step_speedup`.
- Sweep design: `stable_topological_steps`, topology discovery, cumulative width grids, and
  `workload_digest` in `sweep.py` / `sweep.rs`.
- Derived model cache: `scaling_model_to_json`, `scaling_model_path`, and `write_scaling_model`.
- Order and bottom-levels: `_bottom_levels`, `_critical_path`, `_plan_order`, and
  `Planner.CRITICAL_PATH`.
- Memory: `schedulable_peak_mem_bytes`, `step_mem_cap_for_inner_jobs`, and `jobs_for_budget`.
- Core budget: `container_core_budget`, the shared-slice budget, and the run CLI's
  `_select_max_cpus` / `select_max_cpus` resolution.
- Named resources and dispatch: `Runner`, `_res_free`, `_acquire`, and `_release`.
- Allocation and phase-two configuration: `allocate_widths`, `build_plan`, and
  `apply_plan_to_config`.

The behavioral differential supplies the cross-implementation contract for this map.
