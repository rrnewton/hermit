# dagrun profile feedback and modeling audit

> **Provenance.** This is a dated investigation record. It was produced against a private
> downstream workspace, so it refers to repositories, hosts and services outside this one
> that cannot be resolved from here. Projects that consume `agent-utils` appear under
> neutral labels — `consumer-a`, `consumer-b` — and those are REDACTIONS, not real names:
> `agent-utils` is a reusable library and has to read standalone, without naming whoever
> happens to use it. **The measurements, dates and findings are unchanged**; only names
> were replaced. Nothing here describes `agent-utils` itself.
> See `#86 scrub-client-names` and `#67 standalone-repo`.
>
> This record was written before the tool was renamed to `dagrun`. The rename was applied
> throughout: not only the tool's name but the identifiers that carry it — environment
> variables, paths, crate and package names, and cgroup scope prefixes — read as they do
> today rather than as they did on the day. **The measurements, dates, commands and
> findings are unchanged.** Said precisely because the paragraph above promises this record
> is kept as written, and a claim that nothing changed would not have been true.

Date: 2026-08-17

Audit bases:

- `agent-utils` `12d5b41a8e01207ea1bb5c8b929732ecf1945d2f`
- Consumer A `consumer-a:main` `770b95c505fa2794e2937b9245bb72b5f10ec340`
- Consumer A's pinned `agent-utils` submodule `b4951000ceccb08b16b1aab1f4cf303a4ca84172`
- Consumer B `origin/integration` `a4e5b7090150ee9ad7832a038cce36e5c415cd87` and
  `origin/main` `50660ef8b2b339c7c04af9a8dee162e3c597c239` (same relevant path; both pin
  `agent-utils` `cb1ea2e5f0eb79481f3eff93942699ed7e99adfb`)

This report distinguishes three questions that are easy to conflate:

1. Did a run collect profile rows?
2. Did a later run find and accept those rows?
3. Did an accepted model change scheduling or enforcement?

A path that only uploads CSV files answers the first question, not the second or third.

## Executive conclusion

The local collection mechanism is real and fairly extensive. A boxed step records wall time,
cgroup peak charged memory, CPU accounting, achieved parallelism, throttling, process/thread count,
host load, and pressure signals. Python and Rust serialize the same profile schema, and a
subsequent CLI run can read the matching store and replace authored duration and memory estimates.
The whole-run CSV is diagnostic only; planning reads the per-step CSV.

The end-to-end feedback story is much less reliable than the collector:

- A normal repeated CLI run from the same working directory can use its prior local store.
- A fresh or ephemeral CI job does not learn from an uploaded CSV artifact unless the workflow also
  restores it or configures `--profile-sync`. Collection alone does not close the loop.
- The default `greedy-lpt` planner uses learned duration only to change ready-step ordering. Learned
  RSS changes a step's memory cap only when the DAG has no explicit `hard_mem_max_bytes`, and it
  changes outer concurrency only when the caller supplies `--max-mem`.
- Parallel speedup modeling needs at least two distinct recorded `inner_jobs` widths. Normal repeated
  runs at one width do not create a curve. Only `--planner cpa` uses the curve to assign widths.
- No current planner jointly optimizes inner width and cross-step overlap. Greedy-LPT and
  critical-path only order ready steps. CPA chooses widths from isolated per-step curves and its
  reported makespan is a no-overcommit reference; the live scheduler may oversubscribe the boxed
  outer CPU bandwidth under `--max-steps`.
- On the audited code, a boxed step with no explicit `preferred_inner_jobs` is normally constrained
  by the DAG's default one-core `cpu.max`, but its profile row is mislabeled with the ambient
  container width. That can put a one-core observation in a many-core speedup bucket. The
  concurrent 0.13 work fixes this after the pinned audit base.
- The feedback reader accepts one successful sample as enough to override a DAG hint.
- The portable summary used by `--profile-sync` has a severe sampling defect that can invert the
  majority distribution and produce a catastrophically wrong model.

Consumer A is not currently using learned profiles in its important automated paths:

- `scripts/validate.rs` invokes the scheduler library directly, so it never runs the CLI's profile
  reader or planner. It forwards newly collected rows for diagnostics only.
- Hosted `run-node.sh` jobs write into fresh `$RUNNER_TEMP` directories and upload those directories,
  but the workflows do not restore them into a later run and do not configure `--profile-sync`.
- Every step in current `portable.json` has an explicit hard memory cap, so learned RSS would not
  alter the per-step cap or memory-aware concurrency even if the feedback store were restored.

The practical Consumer A verdict is therefore: a historical local store was demonstrably found and
selected by the `plan` command, but there is no paired run evidence proving those values changed
execution. Current CI and `validate.rs` treat profiles as output evidence, not input to the next
plan. Consumer A's current memory decisions are authored, not profile-derived.

Consumer B is in the same operational state for a different reason. Its validator feeds authored
hints directly into the `dagrun` library scheduler and records the resulting measurements into a
separate Consumer B CSV schema. It has accumulated 1,820 local per-step rows, but there is no loader
or plan-application path, and the field names are incompatible with what `dagrun`'s feedback reader
expects. Consumer B therefore collects useful diagnostics but does not use models it has learned.

## Intended data flow

```text
boxed step
  -> in-memory measurement row
  -> machine/container-specific step_profiles_*.csv
  -> a later run's feedback reader
  -> robust per-step estimates and optional speedup curves
  -> planner
  -> applied DagConfig
  -> scheduler order, per-step caps, --max-mem width, and CPA allocations

                         local/remote summary backend
step rows -> bounded summary ---------------------------> later ephemeral run
```

The arrow labeled "later" is load-bearing. A run writes its rows after planning and execution, so
the first run in an empty store cannot benefit from itself.

The adjacent machine-level `<machine>.csv` contains whole-run diagnostics. It is not an input to
the estimator or planner.

## Collection behavior

### Store selection

For both implementations, profile writing is on by default. The write directory is selected in
this order:

1. `--no-profile`: do not write local CSVs.
2. `--perf-dir DIR`.
3. `DAGRUN_PROFILE_DIR`.
4. `./.dagrun/profiles/`, relative to the caller's current directory.

Feedback reading is separate:

1. `--no-profile-feedback`: do not read.
2. `--perf-dir DIR`.
3. `DAGRUN_PROFILE_DIR`.
4. `./.dagrun/profiles/`.

Consequences:

- `--no-profile` disables local CSV writing but still permits reading. With `--profile-sync`
  upload enabled, in-memory rows can still be published to the sync backend.
- `--no-profile-feedback` disables reading but still permits writing.
- A unique `--perf-dir` per invocation collects evidence but guarantees an empty feedback history
  unless that directory is pre-seeded.
- Running the same repository from different current directories fragments the default store.

The CLI names the files it wrote. It does not, by default, say whether it found a prior store, how
many models it accepted, or which decisions those models changed. `plan` or `run --show-plan` is the
only built-in way to inspect `est_source`, `rss_source`, and sample counts.

### What a row contains

Every completed step contributes an in-memory row. A real boxed run can populate:

- stable step tag and authored classification;
- recorded inner width (accurate for an explicit `preferred_inner_jobs`, but not for an undeclared
  step using the default CPU cap on the audited code);
- wall time and exit verdict;
- timeout, CPU-timeout, and OOM evidence;
- cgroup `memory.peak`;
- peak descendant thread count;
- `cpu.stat` counters;
- achieved cores, user/system CPU, throttled time, and quota utilization;
- external CPU attribution, load averages, co-tenant counts, and pressure-stall information.

An unboxed run still records wall/verdict fields, but cgroup-derived peak memory, CPU accounting,
throttling, and pressure enrichment are unavailable. In particular, an unboxed CI profile is not a
memory profile. The `peak_bytes` field is cgroup `memory.peak` (all memory charged to the step
cgroup), not process RSS despite the historical `rss_*` model names.

For a normal undeclared boxed step, both schedulers separately compute the enforced CPU width with
`effective_cpu_count(step, default_step_cpu_count)` (normally one), but compute the profile width
from `preferred_inner_jobs(step)` (which is `None`). The row then resolves that `None` through
`resolve_effective_inner_jobs` to the ambient container budget, and enrichment receives `None`, so
`quota_utilization_pct` is left blank. The command correctly receives no synthetic `-j1`; the bug
is in the recorded/modelled width and enrichment denominator, not command construction.

Rows use best-effort sidecar locking, but the Python and Rust protocols are incompatible and do not
provide a cross-engine serialization guarantee; the detailed failure modes are listed below. The
CSV schema widens when new columns appear. The local raw store is append-only and unbounded; only
the portable summary has a size bound.

### Row acceptance

The reader excludes rows that explicitly record a failure, nonzero return code, timeout, CPU
timeout, or OOM. Missing verdict cells are accepted by design. Malformed numeric cells are ignored
rather than raising. This one row-level gate applies to every metric, so an OOM row's
`memory.peak` is discarded along with its invalid wall-time sample.

Accepted rows are selected solely by:

- the exact profile filename for the current `machine_id` and `container_class`; and
- the step's `group.job` tag inside that file.

The reader does not bind a sample to a command digest, DAG digest, source ancestry, profile base
SHA, runner name, enforcement kind, or schema for the step's semantics. Those provenance columns
are written but not used to decide whether a row is applicable.

The summary format drops timestamp and provenance entirely. It also has no observation identifier.
The built-in seed path merges a downloaded shared summary with a fresh summary of the local raw
store unconditionally, so a persistent runner that previously uploaded those same rows
systematically double-counts its overlapping history.

## Model behavior

### Identity and store matching

`machine_id` normally identifies the CPU model, not a unique host. `container_class` includes
affinity width and effective CPU-quota information, but not the actual CPU list, NUMA placement, or
host identity. Consequences include:

- disjoint same-width cpusets and different same-model hosts can pool measurements;
- a harmless affinity/quota change can make all history disappear, with no fallback or warning.

Environment variables can override both identities for the feedback reader and sync path, but the
local CSV writer ignores those overrides and names files from the physical identity. With an
override set, a repeated local run can therefore read one filename and write another forever.
Summary merges refuse different identities.

### Duration and memory estimates

All accepted widths for a step are collapsed into one per-step estimate:

- duration: contention-discounted robust wall time;
- memory: nearest-rank 90th percentile of `peak_bytes`;
- sample count: every accepted row for that step.

For fewer than three duration samples, the robust estimator uses the minimum, on the assumption
that noise only makes work slower. At three or more samples it uses a median with median-absolute-
deviation trimming. The default minimum sample threshold is one, so a single successful observation
can replace an authored duration and RSS hint.

Collapsing all inner widths means a width-1 duration and width-32 duration participate in the same
scalar duration model. The separate speedup model retains widths, but the ordinary duration/RSS
model does not.

### Where learned duration matters

- `greedy-lpt` uses it to sort ready steps longest-first.
- `critical-path` uses it to compute bottom levels and prioritize the critical path.
- `cpa` uses width-specific curves where available and the scalar estimate as the fallback.

Learned duration does not change dependencies, named resource capacities, or step timeouts.

### Where learned memory matters

The plan replaces `rss_baseline_bytes` when the store has a usable peak. That learned baseline can:

- set the per-step memory cap as `mem_cap_factor * rss_baseline_bytes`, with the current CPU-bound
  width heuristic scaling above four inner jobs; and
- contribute to the modeled concurrent footprint used by `--max-mem`.

An explicit `hard_mem_max_bytes` wins over the learned baseline in both places. Therefore a DAG in
which every step has a hard cap can display `rss_source=store` while making no memory decision from
that value. On the audited base a hard-cap-only step was omitted from modeled concurrency; 0.15.0
fixes that defect as described below.

### Parallel speedup and CPA

The speedup reader groups samples by `(step, inner_jobs)`. A model needs at least two distinct
widths with wall data. At each width it estimates wall time, total CPU work, achieved cores, and
throttling. The recommended width advances only when the next measured width is at least 1.15 times
faster and total CPU work grows by no more than 1.5 times.

Those buckets are only meaningful when the recorded width is the applied width. On the audited
code, a boxed default-one-core step can be placed in the ambient-width bucket. A later explicit
`-j1` observation is split into a different bucket despite the same CPU cap, while a genuinely wide
observation can be merged with the mislabeled one-core sample. Either case can fabricate or erase a
speedup curve and can feed CPA a wall time measured under the wrong CPU allowance.

One sample at each of two widths is sufficient. If CPU-work data is missing, the work-growth guard
cannot reject an inefficient widening.

The default planner does not use that recommendation. `--planner cpa` uses the measured curve to
assign widths across the whole DAG under a core and optional memory budget.

## Conditions required for a model to have an effect

| Requirement | If absent |
| --- | --- |
| A prior store or downloaded summary exists | The run uses authored hints/defaults. |
| Current working directory/store path matches | The default local history is invisible. |
| Machine and container identity match exactly | The reader returns no rows. |
| Step tag still names the same work | Stale data is silently applied to changed work. |
| At least one accepted row exists | Scalar duration/RSS stays authored. |
| Boxed collection supplied `peak_bytes` | No learned memory value is available. |
| No explicit hard memory cap overrides RSS | Learned RSS can affect enforcement/sizing. |
| A hard cap, RSS baseline, or positive runtime default exists | Otherwise current sizing treats the runnable step as unbounded and refuses a finite budget. The audited base incorrectly omitted hard/default-only steps. |
| Caller supplies `--max-mem` | Learned RSS cannot change outer concurrency. |
| At least two inner widths were measured | No speedup curve exists. |
| Caller selects `--planner cpa` | A speedup curve is display-only, not an allocator input. |
| Ephemeral CI restores/syncs history | Uploaded current-run CSVs are diagnostics only. |

## Consumer A audit

### Current automated paths

The current Consumer A `main` audit used commit `770b95c505fa2794e2937b9245bb72b5f10ec340`.

1. `ci/run-dag.sh` forwards runner arguments. With no explicit path, a repeated local invocation can
   read and write the checkout-local default store.
2. `ci/run-node.sh` always passes `--perf-dir`. Locally its default
   `ignored/ci/perf/run-node/<lane>` can persist. In hosted CI, workflows set it to
   `${{ runner.temp }}/run-node-perf`.
3. The workflows upload those temporary directories under names such as
   `run-node-perf-preflight` and `run-node-perf-test-*`. They do not download a prior profile store
   into the next job/run and do not pass `--profile-sync`.
4. The manual `.github/workflows/ci-dag.yml` full-DAG lanes invoke `ci/run-dag.sh` without a
   persistent `--perf-dir`, restore, sync, or profile upload. Their checkout-local default store is
   ephemeral and discarded.
5. Portable hosted jobs opt into `--allow-cgroup-failure`. On the current initial no-scope path,
   that flag returns an unboxed manager without attempting scope re-exec, so those rows are
   timing-only. The privileged workflow currently uses `--unsafe-no-cgroups`, which also guarantees
   timing-only rows.
6. `scripts/validate.rs` calls `run_dag_boxed_deadline` directly. That API schedules the supplied
   config; it does not load CSVs, build a feedback plan, or apply one. `forward_step_profiles`
   appends the result rows for upload, but there is no corresponding reader in that path.
7. At the audited Consumer A commit, all 51 portable DAG steps have both
   `rss_baseline_bytes` and `hard_mem_max_bytes`; all nine privileged DAG steps also carry explicit
   hard caps. Learned RSS therefore cannot replace their enforced or modeled memory values.

### Positive local control

A historical Consumer A worktree contained a 53-row local profile store. Running its `plan` command
against that store produced 46 plan entries, all with `est_source=store` and `rss_source=store`.
Most had one sample and four had two; none had a multi-width speedup model. This proves only that
the local reader found and selected those rows (question 2), because `plan` does not apply or execute
the result. It does not prove a real run changed scheduling or enforcement (question 3). That
worktree was stale and is not evidence that current Consumer A CI closes the feedback loop.

### Consumer A verdict

| Path | Collects | Reuses learned history | Learned memory changes behavior |
| --- | --- | --- | --- |
| Repeated local `ci/run-dag.sh` in one checkout | Yes | Potentially yes | No for current hard-capped DAGs |
| Repeated local `ci/run-node.sh` with persistent ignored dir | Yes | Potentially yes | No for current hard-capped DAGs |
| Hosted portable shards | Yes, mostly timing | No | No |
| Hosted privileged validation | Yes, unboxed timing | No | No |
| Manual full-DAG workflow | Yes, ephemeral | No | No |
| `scripts/validate.rs` library scheduler | Yes, when explicitly forwarded | No | No |

Consumer A currently has profile collection and artifact retention, but not an automated
profile-feedback deployment.

## Consumer B audit

The latest Consumer B `origin/integration` and `origin/main` use the same relevant validation path
and both pin an older `agent-utils` revision from before the current feedback planner:

1. `scripts/validate.py` builds `DagConfig` values from authored `STEP_DURATION_HINT`,
   `PER_STEP_RSS_BASELINE`, `PER_STEP_MEMORY_MAX`, and scheduling-profile tables.
2. It invokes `run_dag(..., metrics=None)` directly. That scheduler API does not discover a profile
   store, build a feedback plan, or apply estimates.
3. After execution it translates `dagrun`'s compact rows into Consumer B's own `validate_perf`
   schema. The historical fields are named `duration_s` and `memory_peak_bytes`, while `dagrun`'s
   reader expects `elapsed_s` and `peak_bytes` in a `step_profiles_<identity>.csv` store of its
   own shape.
4. The local parent `validate_perf` directory contains 1,820 per-step observations across three
   machine/container files. Search of the validation path found analysis and append code but no
   `dagrun` estimate loader, and no `build_plan` or `apply_plan_to_config` call.

Consumer B is therefore not using profile-derived duration, memory, speedup, or scheduling decisions.
Its rows remain valuable for offline analysis, but they are not a feedback store.

## Test and review strength

The strongest existing evidence is cross-implementation consistency:

- Python and Rust compare deterministic plan output against a synthetic profile store.
- The differential checks learned duration/RSS sources, planner ordering, memory sizing, summary
  build/merge/plan equivalence, and CLI surfaces.
- Unit tests cover robust statistics, row filtering, speedup knees, CPA allocation, summary bounds,
  malformed cells, and sync backends.

The real-run differential is much weaker: it checks profile naming/header/newline compatibility,
not equality of measured row values. There is no boxed two-run test that collects run 1, proves run
2 selected the learned model, and then observes the changed runtime order, `memory.max`, or CPA
width in the guest process. There is also no boxed default-one-core test comparing the enforced
`cpu.max` width with the persisted `inner_jobs`, quota-utilization denominator, and speedup bucket.

This is strong evidence that both implementations do the same thing on the fixtures. It is not
strong evidence that the statistical model is correct on adversarial distributions or that a real
consumer restores and uses the store. The present tests emphasize parity and algebraic summary
properties more than deployment closure and model validity.

## Adversarial review findings

An independent agent reviewed this report against the stated `agent-utils` commit, latest Consumer A
`origin/main`, and latest Consumer B integration/main paths under the explicit no-goalpost-moving
rule. The review caused material corrections rather than a lowered bar: it removed a false
Python/Rust core-budget-divergence claim contaminated by uncommitted work; narrowed the Consumer A
positive control from "applied" to "selected by plan"; added Consumer B's pinned pre-feedback
revision and missing verdict evidence; corrected hard-cap sizing, identity overrides, sync overlap,
workflow coverage, and default-width profile recording; marked the concurrent 0.13 plan-application
fix as outside the pinned audit base; and rejected a source-label count as proof that execution
changed. The findings below are the reviewed result.

### Blocker: the bounded summary can invert the observed distribution

The summary reservoir sorts samples by a hash of sample content and keeps the first `K`. Equal
sample values therefore have equal rank; duplicates do not receive independent observation keys.
A minority value whose content hash sorts first can occupy the entire reservoir.

Reproduction on the audited implementation:

- raw rows: 936 samples at 5 seconds and 64 samples at 50 seconds;
- raw robust estimate: 5 seconds;
- 64-sample portable summary: all 64 retained values are 50 seconds;
- summary-derived estimate: 50 seconds.

This breaks the claimed statistical stability of `--profile-sync`. Commutativity and bounded size
still hold, but they preserve the wrong sample. A summary can therefore make an ephemeral CI plan
substantially worse than the raw store it summarizes.

### High: samples are not bound to the work they measured

The reader ignores `git_sha`, `profile_base_sha`, `runner_name`, and `enforcement_kind`, and the row
has no command/DAG fingerprint. Reusing a `group.job` tag after changing its command can apply old
duration and memory data immediately. With a one-sample threshold, one stale row is sufficient.

### High: identity overrides split the local reader from the writer

`DAGRUN_MACHINE_ID` and `DAGRUN_CONTAINER_CLASS` are honored by the
feedback reader and summary sync, but the local CSV writer calls the physical-identity helpers
directly. A run with either override reads `step_profiles_<override>.csv` and writes
`step_profiles_<physical>.csv`; repeated executions never consume the history they just produced.
Existing cross fixtures use overrides to seed readers, not to perform a real write-then-read loop,
so they do not catch this.

### High (audited base; fixed in 0.15.0): planned width-dependent memory is not the runtime cap

The CPA footprint model scales CPU-bound memory with inner width through
`step_mem_cap_for_inner_jobs`. Both schedulers enforce the plain `step_mem_cap_bytes` result at
runtime instead. The planner can approve a wide allocation using a larger modeled allowance while
the step is actually boxed into the smaller base cap and OOM-killed.

Version 0.15.0 routes both runtime enforcement and ordinary `--max-mem` sizing through the same
effective preferred/default width and scaling helper. That closes the execution/planning mismatch,
but does not solve the statistical weakness that the scalar RSS baseline pools observations from
different widths and is then treated as a width-four base.

### High (audited base; fixed in 0.15.0): real runnable steps disappear from `--max-mem` and CPA sizing

The footprint enumerator first filters participating steps to those with a non-null
`rss_baseline_bytes`. It then calls a cap function that would honor `hard_mem_max_bytes`, but that
call is never reached for a hard-cap-only step. An undeclared step receiving the runtime's default
1-GiB cap is omitted for the same reason. It also unconditionally filters `engine_only` steps even
after subset selection has placed one in the runnable config; that flag is selection metadata, not
a zero-memory promise. Consequently `--max-mem` and CPA can admit concurrency that ignores real
enforced caps. The separate stress helper does charge hard caps and runtime defaults, but it shares
the erroneous `engine_only` exclusion. Consumer A happens not to trigger the baseline half because its
hard-capped steps also carry RSS baselines.

Version 0.15.0 includes every runnable non-skipped step, using hard caps, learned/authored RSS, or
the runtime default as applicable. Selected `engine_only` steps participate, stress uses the same
width-aware cap, and an uncharacterized step with no positive default is treated as unbounded
rather than free.

### High: success-only filtering creates memory survivorship bias

One row-level acceptance gate feeds both duration and memory models. Rejecting a failed/OOM wall
time is sensible, but it also discards the OOM run's `memory.peak`, even though that peak is useful
censored lower-bound evidence that the current cap is inadequate. The p90 memory model therefore
learns only from successful lower-memory runs and cannot raise itself in response to the failures
most relevant to memory sizing.

### High (audited base; fixed in concurrent 0.13): Python plan application drops policy fields

Rust clones `DagConfig` before replacing step hints. Python reconstructs a new `DagConfig` but does
not copy every field. In a direct empty-feedback reproduction, Python changed a custom
`default_step_mem_cap_bytes` to 1 GiB, `default_step_cpu_count` to 1,
`default_step_cpu_timeout` to 10 seconds, cleared `known_failures`, reset
`cpu_timeout_multiplier` to 1.0, and cleared `cpu_timeout_platform`. The CLI later reapplies an
explicit multiplier, but library callers and the other policy fields remain affected. This is both
a Python/Rust divergence and a hidden execution-policy change in the feedback path.

Status: fixed by the concurrent 0.13 work after the pinned audit base. The current worktree uses
`dataclasses.replace(cfg, steps=..., resource_caps=...)` rather than reconstructing `DagConfig`
field by field, with regression coverage that preserves `default_step_cpu_count` through plan
application before the run-budget clamp. As with the ancestor-quota correction below, this fix was
developed alongside the audit and is outside `12d5b41a`; the finding describes the audited commit,
not the corrected 0.13 implementation.

### High: default-one-core steps are recorded as ambient-width steps

This defect is present at the pinned `12d5b41a` base and was not introduced by the concurrent
`-s`/`-j` work.

Both audited schedulers distinguish command parallelism from cgroup enforcement correctly when a
step omits `preferred_inner_jobs`: no potentially unsupported `-j1` is appended, while
`effective_cpu_count(step, default_step_cpu_count)` supplies the normal one-core `cpu.max`.
Profiling loses that distinction. The row's `inner_jobs` and the enrichment call both receive the
optional declared width (`None`) instead of the resolved applied CPU width. `None` is serialized as
`container_core_budget()` and suppresses `quota_utilization_pct`, so a step that actually ran in a
one-core child cgroup can be persisted as `inner_jobs=316` on a wide host.

This corrupts the width-specific evidence rather than merely its label. A later explicit width-one
sample is treated as a different level despite equivalent enforcement; a true ambient/wide sample
can share a bucket with the one-core observation; and CPA can therefore infer a false plateau,
speedup, or per-core area. It also prevents quota utilization from revealing that the default cap
was saturated.

The 0.13 correction keeps the optional declared width for command rewriting, but passes the
resolved applied `cpu_count` to row serialization and enrichment only when boxing is active. Its
boxed recurrence tests leave `preferred_inner_jobs` unset, retain the default one-core cap, and
assert in both engines that `inner_jobs=1` and quota utilization has a one-core denominator. The
unboxed cross-check deliberately retains the ambient fallback, because no cgroup-applied default
width can honestly be claimed there.

Status: fixed by the concurrent 0.13 work after the pinned audit base. The finding describes
`12d5b41a`; the corrected schedulers and their boxed/unboxed tests land with this report.

### High: ambient-width modeling ignores binding ancestor CPU quotas

Both committed implementations derive profile identity from the effective CPU quota by walking the
current cgroup and its ancestors, but `container_core_budget()` reads only the cgroup mount root's
`cpu.max` and otherwise falls back to process affinity. In a nested systemd scope with a two-core
ancestor quota and a 316-CPU affinity mask, an ambient step can therefore be recorded/modelled as
width 316 and CPA can allocate against 316 even though the kernel grants two core-equivalents.
There is also a parity bug at fractional quotas: Python's `round` is ties-to-even while Rust's
`f64::round` is ties-away from zero, so 2.5 cores becomes two versus three; rounding upward can
overstate the binding bandwidth in either case. The concurrent `-s`/`-j` implementation corrects
this path with ancestor-aware conservative integer flooring. That fix was developed alongside this
audit and is outside the stated `12d5b41a` audit base; it was not treated as evidence that the
audited code was already correct. Python and Rust must remain cross-tested on nested quotas,
affinity, and 0.5/1.5/2.5-core fixtures.

### Medium: one observation overrides authored policy

`DEFAULT_MIN_SAMPLES` is one. A single lucky, cold, warm, noisy, or otherwise unrepresentative
success replaces both duration and RSS hints. Robust estimators cannot provide robustness with one
sample.

### Medium: normal scalar estimates collapse unlike widths

Duration and RSS aggregate across every `inner_jobs` bucket. A step measured at materially different
widths gets one scalar median/p90, which may correspond to no actual width. The separate speedup
curve does not repair ordinary greedy/critical-path plans or the RSS baseline.

### Medium: profiles cannot model beneficial or harmful co-running widths

Rows record rich load, pressure, throttling, CPU-time, and memory diagnostics, but they do not bind
an observation to the exact simultaneously active sibling tags and widths or to the aggregate
requested width at launch. The scalar estimator also discounts ambient contention, which can
normalize away the effect a co-running model would need to learn. Consequently the current data can
fit `T_i(p)` for one step at width `p`, but not `T_i(p | siblings, requested load, P)`. Allowing two
wide steps to share the outer quota is an explicit runtime policy, not a model-derived claim that
overlap is faster.

### Medium: feedback absence and ineffectiveness are hard to see

The writer loudly names output files, but a default run does not state that no matching input store
was found, that zero estimates were selected, or that hard caps made every learned RSS value inert.
This lets a consumer believe "profiling is enabled" when only collection is enabled.

### Medium: unboxed CI artifacts look complete but lack the critical resource data

Unboxed rows share the standard CSV schema but leave peak memory and cgroup enrichment blank. A
workflow can upload a normal-looking profile artifact that cannot train the memory or CPU model.
The reader records `enforcement_kind` but does not filter on it.

### Medium: GitHub artifact sync is intentionally lossy under concurrent publishers

The GitHub-artifacts backend performs no atomic read-modify-write. Concurrent jobs can download the
same prior artifact and later uploads can lose one contribution. The git-branch backend is the
available atomic concurrent-update backend, but it does not repair duplicate-observation accounting
inside the summary format.

### Medium: the contention correction is not a trustworthy work model

The estimator subtracts the step cgroup's CPU from whole-host `/proc/stat` activity, divides by the
step affinity width, and linearly discounts elapsed time by the resulting "external" fraction. Work
on disjoint host CPUs and concurrent sibling DAG steps therefore count as contention. The same
correction is applied to CPU, I/O, latency, and light steps, and values clamp at 95%, so a sample can
be reduced to 5% of measured wall time. Several PSI/co-tenant columns written by the collector do
not match the names consumed by the reader and are currently inert.

### Medium (audited base; fixed in 0.15.0): `--max-mem` can report a schedule that does not fit its budget

If one step's modeled footprint exceeds the entire requested budget, sizing still returns a
one-step schedule rather than refusing it. `--max-mem` is also an admission model, not an enforced
outer `memory.max`. The command therefore means "limit modeled concurrent footprint" rather than
"the run cannot exceed this many bytes."

Version 0.15.0 returns an explicit zero-step infeasibility sentinel and the CLI refuses it. CPA
checks every narrowest one-step allocation against learned RSS plus the same floor/safety factor and reports
`infeasible-memory` instead of applying an executable allocation.

### High (audited base; fixed in 0.15.0): stress sizing could certify a different graph than execution

The original stress guard ran before `--max-cpus` clamping and before profile/CPA application. It
could overcharge a width that execution would lower, or approve authored RSS before feedback raised
the final cap. Very small characterized guest caps also provided no host-side control-plane floor,
so a huge `--stress` value could allocate an enormous generated DAG before the guest memory check
became relevant.

Version 0.15.0 preflights the CPU-capped authored copy, then rechecks the already-expanded final
post-feedback graph without multiplying it twice. A positive per-copy control-plane floor remains
in force, and expansion refuses more than 100,000 generated nodes/control units before allocation.

### Medium (audited base; bounded in 0.15.0): memory-footprint planning is combinatorial

Footprint sizing enumerates every dependency/resource-compatible combination up to the candidate
step count. `jobs_for_budget` repeats that search for successive counts, and CPA repeats it while
widening. The Rust helper additionally materializes each combination set eagerly. On a wide,
lightly constrained DAG this is exponential and can make planning itself unusable or exhaust
memory; existing correctness fixtures are small and do not establish a practical bound.

Version 0.15.0 bounds exact enumeration to 100,000 subsets, then deterministically falls back to
the sum of the largest candidate caps while ignoring dependencies/resources. The fallback runs
before dependency closure; closure is iterative, and one-step probes use an O(n) maximum scan. The
fallback is less precise but conservative and keeps wide-DAG planning bounded in both implementations.

### Medium: profile persistence has writer and failure-mode weaknesses

The portable summary cannot identify duplicate observations. The normal seed path unconditionally
merges downloaded history with the local raw-store summary, so persistent runners systematically
double-weight overlapping rows they previously uploaded. The GitHub backend can also lose
concurrent contributions. The raw writers have
cross-language lock hazards: Python uses `flock` on a sidecar inode, while Rust treats exclusive
pathname creation as the lock and eventually writes unlocked after a short wait. The protocols do
not serialize each other. Python also unlinks the lock pathname before releasing the flock,
allowing a new inode to be locked by a third writer. Rust additionally treats profile-read errors
as an absent store, which can hide a corrupt or unreadable feedback source as an ordinary cold
start.

## Recommendations

### Correctness before broader deployment

1. Replace content-ranked reservoir truncation with a deterministic sample of observation
   identities, or store value multiplicities explicitly. Add the 936/64 inversion as a regression
   in both languages.
2. Bind every sample to a stable work identity: at least a command/environment/resource-policy
   digest plus model schema. Define an explicit compatibility rule for source revisions instead of
   ignoring the recorded SHAs.
3. Use one resolved feedback identity for local reads, writes, and sync. Add an override-enabled
   two-run test, and either encode actual placement/NUMA distinctions or document deliberate
   pooling rather than implying affinity width identifies placement.
4. Retain the 0.15.0 change that makes runtime memory enforcement and ordinary `--max-mem` sizing
   use the same width-dependent function, and add a live boxed end-to-end test that would OOM under
   the old mismatch.
5. Retain the 0.15.0 coverage of hard-cap-only, runtime-default-capped, and selected `engine_only`
   steps in every sizing path including stress. Keep each case tested against its actual enforced
   cap and preserve the conservative bounded-search fallback for wide DAGs.
   Keep stress's final post-feedback check, per-copy control-plane floor, and generated-node bound
   paired across implementations.
6. Split row admission by metric: reject failed wall-time samples, but retain OOM peaks as censored
   lower bounds or at least explicit cap-inadequacy evidence that can safely raise/review memory.
7. Retain the concurrent 0.13 `replace(cfg, ...)` plan-application fix and cross-test every
   top-level field, not only CPU-count policy, through plan application.
8. Derive ambient/CPA core budgets from the tightest ancestor quota and process affinity in both
   engines, and record the resolved applied per-step CPU width rather than the optional declared
   command width. Add nested-cgroup fixtures that distinguish a two-core quota from wide affinity,
   plus boxed undeclared/default-one-core fixtures proving row width, quota utilization, and
   speedup-bucket identity in both engines.
9. Replace the contention discount with a model validated by step class and CPU placement, or keep
   the raw measurement when the available telemetry cannot distinguish sibling/disjoint work.
   Record exact active sibling tags/widths, aggregate requested width, run `P`, and start/end
   overlap so a future scheduler can model whether oversubscription helps instead of inferring it
   from host load alone.
10. Retain the 0.15.0 refusal when even one required step cannot fit, and clearly separate modeled
    admission from an enforced outer memory ceiling.
11. Give raw and summary observations stable IDs; make merges idempotent; and make profile lock/read
    failures fail visibly rather than silently dropping history or writing outside mutual exclusion.
12. Retain the 0.15.0 bounded conservative fallback and benchmark it on realistic 50-plus-step wide
    DAGs in both implementations.

### Make model use observable

13. Emit a compact plan receipt on every run: store path/summary source, matching identity, accepted
   and rejected row counts, steps whose duration/RSS came from the store, steps whose hard caps made
   learned RSS inert, and whether a speedup curve affected an allocation.
14. Add a strict mode that requires usable feedback when a consumer claims profile-guided planning.
   Absence, identity mismatch, or zero applicable models should then fail rather than silently fall
   back.
15. Raise or separately configure the sample threshold for duration and memory, and require
    meaningful per-width support before a two-point speedup curve can drive CPA. Report uncertainty
    instead of treating one observation as a stable model.
16. Keep scalar duration/RSS models width-specific, or define and test a principled projection from
   width-specific measurements to the width being planned.

### Close the Consumer A loop deliberately

17. Choose a persistent feedback mechanism for Consumer A CI: the atomic git-branch summary backend or
    a workflow cache/artifact protocol that downloads before planning and uploads after execution.
18. Run the memory-producing lanes boxed. An unboxed artifact cannot supply learned peak memory.
19. Decide whether Consumer A's explicit hard caps are policy ceilings or generated estimates. If they
    remain authoritative, do not claim learned memory drives the run; use profiles to propose and
    review cap changes instead.
20. Retain the actual run-emitted receipt from recommendation 13, including a digest of the plan
    and feedback input it executed. Assert exact expected store-derived tags and a real decision
    delta against `--no-profile-feedback`; a separate `plan --format json` artifact is useful only
    as supplementary evidence because it may not include the same sync/download state. A
    source-label count alone is insufficient because hard caps can make every learned RSS inert.
21. Add a real two-run consumer test: run once into an empty store, run again from the same or synced
    store, and prove a deliberately different learned estimate changes the second plan while a
    changed command identity refuses the stale sample.

### Close the Consumer B loop deliberately

22. Either emit `dagrun`'s exact profile schema into a configured feedback store and apply a plan
    before `run_dag`, or add an explicit, tested adapter for new Consumer B rows. The legacy schema
    omits success, return-code, timeout, CPU-timeout, and OOM evidence, so its ambiguous historical
    rows must remain diagnostics-only unless independently classified; merely renaming
    `duration_s`/`memory_peak_bytes` would train on failures, because `dagrun` accepts a missing
    verdict cell. Do not relabel the CSV as feedback without proving the second run changes.
23. Preserve Consumer B's authored hard limits as policy ceilings and keep learned estimates separate,
    so adopting feedback cannot silently weaken resource containment.
