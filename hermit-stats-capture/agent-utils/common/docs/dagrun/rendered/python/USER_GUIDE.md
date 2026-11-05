# dagrun user guide

`dagrun` models build and test work as a DAG, plans it deterministically, and
executes ready steps concurrently. Dependencies, memory budgets, named resource
caps, timeouts, CPU reservations, and Linux containment all constrain execution
without changing the graph's meaning.

## Installation and library use

```sh
python3 -m pip install dagrun
```

Python 3.10 or newer is required. Installation provides both console commands,
the typed `dagrun` package, and this guide as package data. The distribution and
the import name are the same word.

```python
from dagrun import DagConfig, Step, to_ascii

dag = DagConfig(steps=(Step("build", "app", "compile", "make build"),))
print(to_ascii(dag))
```

The package also exports strict JSON/YAML conversion, resource sizing,
pluggable containment and metrics protocols, plan construction, and profile
analysis. The `run_dag` library function is an explicit process-group-only
scheduler unless the caller supplies an enabled `CgroupManager`. Use the
console command on Linux with cgroup v2 and a delegated systemd user scope when
the package should establish and verify containment for you.

`run_dag(..., jobs=N)` treats `N` as a compatibility combined active-step and
per-step-width limit unless `core_budget` is supplied. Use
`run_dag_limited(..., max_steps=S, max_cpus=P)` for explicit independent
limits; the former `cpu_jobs=P` keyword remains a compatibility alias. A
pre-0.13 outer-fan-out-only library caller should migrate to the limited API and
choose both values deliberately. These library calls do not establish an outer
CPU quota: without caller-supplied containment, `max_cpus` caps individual
runner-controlled widths but does not cap aggregate bandwidth or serialize
overlapping widths.
The low-level `allocate_widths(...)` helper raises
`InfeasibleAllocationError` when a self-managed fixed command width exceeds
its core budget; 0.15 makes that refusal explicit instead of returning a
fictitious executable width.

## A first DAG

Each step is identified by a `group.job` tag. `deps` names predecessor tags.
Optional `labels` place the step in named subsets without changing its identity
or dependency edges.
The optional `hint` object supplies estimates and limits; top-level
`resource_caps` limits caller-defined scarce resources.

Every resource a step demands must have a cap declared. An UNDECLARED
resource is refused before any node starts, because it is not the same thing
as a cap of `0`: undeclared means capacity you forgot to grant, while `0`
means the step is blocked on purpose. Both would otherwise leave the step
permanently unready with nothing said, so declare the capacity — or write the
cap as `0` to say the block is deliberate.

```yaml
resource_caps:
  browser: 1
steps:
  - group: build
    job: app
    desc: compile the application
    cmd: make build
    hint:
      est_duration_s: 30
      classification: cpu-bound
      rss_baseline_bytes: 536870912
  - group: test
    job: unit
    cmd: make test
    deps: [build.app]
  - group: test
    job: browser
    cmd: ./run-browser-tests
    deps: [build.app]
    hint:
      resources: {browser: 1}
```

JSON and YAML express the same strict schema. File names ending in `.yaml` or
`.yml` select YAML; other paths select JSON. Use `--dag -` for JSON on standard
input.

### Compose DAG files at load time

The top-level `include` list composes reusable DAG fragments before planning or
execution. It is load-time syntax, not a runtime node: every command sees one
ordinary flattened graph, and canonical `dagrun json`/`yaml` output contains no
`include` key.

```yaml
include:
  - path: validation/portable.yaml
    namespace: portable
  - path: validation/native.yaml
    namespace: native
    after: [bootstrap.tools]
steps:
  - group: bootstrap
    job: tools
    cmd: ./prepare-tools
  - group: publish
    job: all
    cmd: ./publish
    deps: [portable.test.unit, native.test.unit]
```

`path` is relative to the file declaring it. After symlinks are resolved it
must remain beneath the top-level DAG's directory; absolute paths, URLs,
globs, tilde expansion, environment expansion, missing files, and directory
targets are refused. `namespace` is one segment matching
`[A-Za-z0-9][A-Za-z0-9_-]*`; nested namespaces join with dots.

Flattening is depth-first in include order, then local-step order. A namespace
prefixes each included step's `group`, and therefore its `group.job` tag. It
also prefixes that fragment's `deps`, `explains`, structured-result `owner`,
and `fail_fast_family`. Labels, commands, environment, write domains,
resources, and manifest identities are unchanged. The same canonical file may
be included under different effective namespaces, but including it twice under
the same namespace is an error. Include cycles name the complete canonical
file chain.

Composition is bounded before expansion can exhaust the loader: at most 128
include edges may be nested, at most 4,096 include instances may be loaded
(the root is not counted), and the flattened graph may contain at most 100,000
steps. Reusing one canonical file under different namespaces counts once per
instance. These bounds apply to path-based composition only; the context-free
single-document APIs keep their existing graph-size behavior.

Optional `after` is parent-owned cross-boundary wiring. Its tags are resolved
in the including file's namespace and injected as dependencies into every
entry node of the included flattened subgraph (a node with no dependency on
another node in that subgraph). This lets a parent put a fragment after an
outer setup step without teaching the child about its caller. There is no
magic `..` syntax. Every `after` tag must name a real step outside the included
subgraph, even when that subgraph is empty. Dependencies from outer steps to
the fragment name exact flattened tags, as `publish.all` does above. No
synthetic include or completion node is created, and `after` does not add an
implicit whole-subgraph completion barrier.

Nested schedulers are refused by default. A step that intentionally runs dagrun
may set `delegated_children: true`. Under cgroup boxing, the outer scheduler
keeps ownership of that step's whole subtree, places the command in a
`supervisor` leaf, and lends the empty step root to the inner scheduler for its
own child cgroups. The inner process verifies its live kernel membership before
accepting that path. The delegation is removed from ordinary child-step
environments, so it cannot silently authorize another nesting level. If the
outer run is itself explicitly unboxed, the same graph property authorizes a
named uncontained fallback for that step only. The child prints a warning and
uses process-group/procfs teardown and accounting; it never describes that lane
as cgroup containment. A malformed delegation marker or forged cgroup path is
still refused.

The parent attributes memory pressure at the boundary it owns. Ordinary steps
use the hierarchical `memory.events` counters. A delegated step uses
`memory.events.local`, so an inner step's independently classified OOM is not
reported again as an outer-step failure. An OOM caused by the parent's own
`memory.max` remains local to that delegation boundary even when the selected
victim is in a descendant. In that case the boundary can increment `oom` (or
`oom_group_kill`) while `oom_kill` is accounted only in the victim's child
cgroup. Any of those positive kernel OOM counters therefore fails the outer
step; a bare signal with no counter still does not.

`resource_caps` from all files form one union. Repeating a cap with the same
value is allowed; conflicting values are refused. The root owns global policy
(`default_step_timeout`, job channels, memory factors, and
`write_domain_policy`). A child may omit those fields or repeat the same
normalized value, but cannot override one. Only the root description is used.

The path-free `dag_from_json`/`dag_from_yaml` APIs and `--dag -` refuse an
`include` key because they have no safe base directory. Use `dag_from_path` or
pass a filename. The exact `include` key is reserved and its entry schema is
closed (`path`, `namespace`, optional `after`), so malformed composition can
never be silently ignored as an unknown top-level extension.

#### What the loader refuses, and where

**Every** subcommand that reads `--dag` — including the inspection ones (`list`,
`plan`, `ascii`, `dot`, `json`, `yaml`) — refuses these at load, with **exit 2**,
before any plan is built and before any step runs. `dagrun list --dag x.yaml`
is therefore a complete, cheap graph check that executes no step.

| refused | the refusal names |
|---|---|
| an unknown field in a `steps[]`, `hint`, or `write_domain_policy` object | the step (`steps[3] (test.unit)`), every unknown key, and the known ones |
| a wrong-typed field | the field and the type wanted |
| a duplicate step tag | the tag and how many times it was declared |
| a dependency no step declares | the step and the missing tag |
| a dependency cycle | **the cycle**, as `a.one -> b.two -> a.one` |
| a demand above a **positive** `resource_caps` entry | the step, the demand, and the cap |
| a malformed, escaping, cyclic, duplicate, or policy-conflicting `include` | the source file and offending entry/path/namespace |
| one of the six top-level keys the format does not carry (below) | each key |

Those objects are **closed**: a field they do not define is refused rather than
ignored, because an ignored field reads exactly like one that took effect —
`est_duration` for `est_duration_s` is not a smaller estimate, it is no estimate
at all, silently. The **top level** stays open (a key naming nothing at all is
tolerated, so forward-compatible additions keep loading), with the six named
exceptions below and the exact reserved `include` key described above.

Two related refusals happen slightly later, at the start of a run rather than at
load, because they are about capacity rather than about the document:

- a demand for a resource with **no** cap declared (see above — undeclared is
  not a cap of `0`), and
- a demand above a cap declared as exactly `0`, which stays the documented
  deliberate block.

The four *graph* refusals — duplicate tag, missing dependency, cycle, and a
demand above a positive cap — are re-checked at the scheduler's own entry point
as well, so a configuration built in memory by a library caller cannot bypass
the file loader. (The others are properties of the document text, which an
in-memory configuration does not have.)

#### Six top-level keys the document format does not carry (breaking change)

A DAG document is **refused at load** (exit 2) if it sets any of:

    default_step_mem_cap_bytes   default_step_cpu_timeout   cpu_timeout_platform
    default_step_cpu_count       cpu_timeout_multiplier     known_failures

These name real runner-configuration fields, but the document format has never
carried them: writing one had **no effect whatsoever**, and nothing said so — a
cap you thought you had set was silently the default. That is the reader's side
of the dropped-field bug, so the loader now names the key instead of ignoring
it. Both language editions refuse exactly the same six.

**This can break a document that loaded before.** Set these at the call site
(they are caller/platform policy, not properties of the graph — several have
`--cpu-timeout-multiplier`-style flags or environment variables), or delete
them: deleting changes nothing, because they were never in effect. A key that
names *nothing* at all is still tolerated, so forward-compatible additions keep
loading.

### Fail closed on protected artifact writes

An opt-in `write_domain_policy` turns per-step write declarations into a
pre-execution requirement. `allowed_domains` is a closed vocabulary;
`require_explicit: true` requires every step to carry `write_domains` (use `[]`
when the step writes none of the protected domains). Unknown, duplicate, or
missing domains stop the whole DAG before the first command starts.

Every non-empty declaration also names its structural guarantee:

- `artifact-producer` creates mutable inputs to a later publisher.
- `immutable-artifact-barrier` atomically publishes the immutable artifact.
- `artifact-barrier-dependent` must transitively depend on such a barrier; the
  runner verifies that graph relationship before execution.
- `explicitly-isolated` writes a package/path-disjoint output.

```yaml
write_domain_policy:
  require_explicit: true
  allowed_domains: [shared-target, isolated-target]
steps:
  - group: build
    job: publish
    cmd: ./publish-immutable
    write_domains: [shared-target]
    write_domain_guarantee: immutable-artifact-barrier
  - group: test
    job: unit
    cmd: ./run-shared-target-test
    deps: [build.publish]
    write_domains: [shared-target]
    write_domain_guarantee: artifact-barrier-dependent
  - group: build
    job: fixture
    cmd: ./build-fixture-in-private-target
    write_domains: [isolated-target]
    write_domain_guarantee: explicitly-isolated
```

Write domains are not scheduler semaphores. Disjoint writers retain their
parallelism, and a shared domain is not silently converted into one global
mutex. External writers remain outside the scheduler; an immutable publication
barrier shields consumers without pretending those writers were serialized.

## Inspect, convert, and visualize

```sh
dagrun list --dag pipeline.yaml
dagrun ascii --dag pipeline.yaml
dagrun dot --dag pipeline.yaml > pipeline.dot
dagrun json --dag pipeline.yaml > pipeline.json
dagrun yaml --dag pipeline.json > pipeline.yaml
```

`list` is a compact inventory. `ascii` shows dependency layers. `dot` emits
Graphviz input. `json` emits canonical JSON, while `yaml` emits stable YAML.

A large graph reads better one level up. `ascii` and `dot` accept two view
options:

```sh
dagrun dot --dag pipeline.yaml --group-by group | dot -Tpng -o groups.png
dagrun dot --dag pipeline.yaml --labels full --group-by group > full.dot
dagrun ascii --dag pipeline.yaml --labels quick --group-by group
```

- `--labels LABEL[,LABEL...]` keeps the steps carrying any named label, plus
  every dependency they require: the same steps `run --labels` would run.
- `--group-by group` draws one node per `group`, labelled with its step count,
  and one edge per (upstream group -> group) pair, labelled with how many
  step-level dependencies it merges. Dependencies inside a group draw no edge.
  In `ascii`, each group line lists its upstream groups with those counts.

Without either option the output is unchanged.

## Plan without running

```sh
dagrun plan --dag pipeline.yaml
dagrun plan --dag pipeline.yaml --planner critical-path
dagrun plan --dag pipeline.yaml --format json
```

The default `greedy-lpt` planner favors the longest ready step. The
`critical-path` planner favors the largest remaining weighted path. The `cpa`
planner additionally chooses inner widths from measured speedup curves. Stable
tag ordering breaks ties, so repeated plans over the same inputs are identical.

By default, past profile samples refine duration and resident-memory estimates. Duration may move
in either direction. An authored `rss_baseline_bytes` is a safety floor: ordinary feedback may
raise it when a run used more, but one quiet run never lowers the next run's enforced memory cap.
Use `--no-profile-feedback` when only authored hints may influence a plan.

### Learning a memory cap from profiles, safely

`run --profile-memory-feedback` is the separate, opt-in path allowed to lower an authored
`rss_baseline_bytes`. It derives the replacement from the store's **uncensored** peaks only.

A recorded `peak_bytes` does not on its own say what it measured. A step whose
peak reached the `memory.max` applied to it, or that the kernel reclaimed,
throttled or OOM-killed at that ceiling, used everything it was allowed and may
have wanted more. Treating that number as an observed maximum re-derives the cap
that produced it and freezes the mistake. So:

- a censored sample is used only as a **floor** — proof demand was at least that
  large — and can raise an estimate, never lower one;
- a row recording a step that **failed** — `ok` false, or a non-zero
  `returncode` such as 137 (SIGKILL, what an OOM kill from an enclosing cgroup
  looks like) — is censored for the same reason: it says where the step got to
  before it died, not where it was going;
- a sample that cannot say (no applied cap recorded, no event counters, no peak)
  is not evidence, and is counted and reported rather than assumed comfortable;
- a step needs at least five uncensored samples before its authored hint is
  replaced at all, and the estimate carries a 20% margin above the 9/10
  percentile;
- a step this path **declines** to estimate uses the larger of the baseline its
  author wrote and the floor its recorded peaks prove. Ordinary feedback already
  keeps the authored value as its floor; this opt-in path preserves that same
  guarantee when it declines to lower. Keeping the recorded floor matters in
  the other direction: a step whose
  every run was pinned to a 32 GiB ceiling has proven it needs at least that,
  and modelling it at the 1 GiB its author guessed would be the same ratchet
  running the other way. "We do not know the peak" and "we know the peak is at
  least X" are different answers, and a decline gives the second one whenever
  the evidence supports it — so a decline can only ever **raise** a step's
  baseline above what its author wrote, never lower it;
- `hard_mem_max_bytes` is never rewritten: an explicit hard cap is an
  instruction, not a guess.

Every step the store knows about gets one line on stderr saying what was decided
and on what evidence, including the steps that did **not** move — otherwise "the
cap did not change" and "the store had nothing usable to say" look the same:

```
dagrun: --profile-memory-feedback: g.build: rss_baseline_bytes=2576980377 [6 uncensored, 0 censored, 0 unprovenanced of 6; 6 uncensored sample(s), 9/10 percentile +20%]
dagrun: --profile-memory-feedback: g.link: keeping the authored hint [0 uncensored, 6 censored, 0 unprovenanced of 6; every one of 6 recorded peak(s) was censored by its applied cap]
dagrun: --profile-memory-feedback: g.test: no estimate; rss_baseline_bytes=34359738368, the proven floor [0 uncensored, 6 censored, 0 unprovenanced of 6; every one of 6 recorded peak(s) was censored by its applied cap]
```

`g.link` and `g.test` were declined on identical evidence; they differ only in
what their authors wrote. `g.link`'s hint already covers the floor its peaks
prove, so it is left alone. `g.test`'s does not, so the floor is used and the
line names the number rather than claiming the authored hint was kept.

The provenance columns this reads are written by the runner itself; a store
recorded before they existed reads as unprovenanced, which keeps every authored
hint. Combining the flag with `--no-profile-feedback` (which turns the store
reader off) derives nothing, and says so.

The estimate is applied after the plan is built, so `--show-plan` prints the
authored figures and the stderr lines above are the record of what actually
took effect.

## Run safely

```sh
dagrun run --dag pipeline.yaml --max-steps 2 --max-cpus 8
dagrun run --dag pipeline.yaml --max-mem 8G
dagrun run --dag pipeline.yaml --show-plan --profile
```

`-s` / `--max-steps` bounds how many DAG nodes may be active. `-j` /
`--max-cpus` independently sets the **total CPU capacity for the whole run**, in
core-equivalents, and caps any one runner-controlled step at that width. It does
not reserve or subtract declared widths as steps start. Thus `-s2 -j8` may run
two `-j8` steps together: their sixteen workers contend inside the outer
eight-core-equivalent quota. This oversubscription can help when work stalls or
parallel phases do not align, and can hurt when both steps are CPU-bound. The default CPU total is
the effective container/affinity capacity tightened by the shared aggregate
slice's 90% host budget. An undeclared
step separately keeps `default_step_cpu_count` (one in the CLI defaults).

**One coupling survives on purpose: an absent `--max-steps` defaults to the
RESOLVED `--max-cpus`** — the value you typed, not the ambient default. So `-j1`
alone runs the DAG serially, `-j3` alone permits three-way overlap, and `-j200`
alone permits two hundred active nodes. If you meant to change bandwidth only,
say what you meant about overlap too: pass `-s` as well. The hidden 0.13
`--jobs N` alias resolves to `--max-cpus` first and therefore sets the same
default. Nothing runs in the other direction: `-s` never changes the CPU budget.

A non-empty effective `jobs_flag` or `jobs_env` makes the inner width
runner-controlled: when an authored or profile-derived width exceeds
`--max-cpus`, the planner and scheduler cap the recommendation, guest-visible
width, and per-step `cpu.max` together. `jobs_flag` appends the width to the
command; `jobs_env` sets the named environment variable. At the DAG level,
`default_jobs_env` is inherited by steps; when that field is absent,
`DAGRUN_JOBS_ENV` supplies the host-specific default. A step-level
value overrides the default, `null` inherits it, and `""` opts that step out.
Names must be valid shell environment-variable identifiers or the DAG is
refused before a child starts. Some syntactically valid names are nevertheless
managed or readonly in the child shell (for example `BASHOPTS`). Immediately
before the guest command, the runner checks both assignment status and exact
readback; if the requested width did not stick, that step fails without running
the guest command.

`cmdtype` provides the known command-line forms without requiring each step to
spell its own `jobs_flag`. Its valid values and the exact value used for a width
of 3 are:

| `cmdtype` | `DAGRUN_EXTRA_ARGS` / appended text |
|---|---|
| `unknown` (default) | none; the existing `jobs_flag`/`jobs_env` rules still apply |
| `make` | `-j3` |
| `cargo-build` | `--jobs 3` |
| `cargo-test` | `--jobs 3` |
| `cargo-nextest` | `--test-threads 3` |
| `generic-dash-j-command` | `-j3` |
| `generic-with-flag` | the required step-level `jobs_flag`, rendered with 3 |

For a simple command, dagrun appends that text. For a compound command, put
`$DAGRUN_EXTRA_ARGS` or `${DAGRUN_EXTRA_ARGS}` unquoted exactly where the
arguments belong; detecting either form prevents a second appended copy. For
example, `prepare && build-tool $DAGRUN_EXTRA_ARGS` receives `--jobs 3` at that
position. The variable is set only when a known `cmdtype` has an effective
width. Under `unknown` it is removed from the step environment, including an
ambient or step-supplied value. A compound command with no placement is refused
rather than appending the arguments to whichever command happens to be last.

Shell tokenization matters. `-j3` contains one shell word. `--jobs 3` contains
two, so an unquoted `$DAGRUN_EXTRA_ARGS` expands and word-splits into the two
arguments `--jobs` and `3`. Quoting it as `"$DAGRUN_EXTRA_ARGS"` suppresses word
splitting and would pass the single wrong argument `--jobs 3`; dagrun refuses
that quoted form before any step starts for every command type whose value has
multiple words. Double-quoting a one-word value such as `-j3` still passes one
correct argument. Single quotes prevent variable expansion entirely and are
always refused when they surround `DAGRUN_EXTRA_ARGS`.
`generic-with-flag` requires a non-empty step-level `jobs_flag`; other known
values refuse a simultaneous non-empty `jobs_flag` rather than silently choosing
between two command-line descriptions.

Under `cmdtype: unknown`, empty or whitespace-only effective flag **and**
environment channels prevent rewriting; paired with a positive `preferred_inner_jobs`, that
declares a self-managed fixed command width. If that declared width
exceeds the run budget, the run is refused before any DAG step process is
created because silently throttling (for example) a hardcoded `make -j32`
inside `--max-cpus 16` would oversubscribe and mislabel its memory/profile data.
File-backed runs reject before cgroup setup; a stdin DAG may already have
entered its outer scope before it can be read and validated. The single-step
sweep likewise refuses a step with no effective width channel,
since changing `sweep --jobs` would not change the guest. A graph-wide
target-time sweep handles such a fixed node explicitly instead: it characterizes
the configured width once when that width fits the effective CPU budget, or
visibly skips it when it cannot run honestly on this machine.

The runner cannot infer hidden concurrency that a command does not declare. An
arbitrary guest may still create more threads than `--max-cpus`; outer
`cpu.max` limits their total CPU bandwidth, not their count. Use a controllable
`cmdtype`, `jobs_flag`, or `jobs_env`; fix the command's own worker setting; or
use `--cores` when fixed CPU eligibility is required.

Under boxing the runner also exports a bounded build-worker width to each step
(never through `MAKEFLAGS`, which would reach determinism-sensitive targets), so
that a build tool cannot compute a width from the granted quota alone and
OOM-race the linker. **If no `jobs_env` channel targets that variable and you
set it yourself, your value wins.** When `jobs_env` names the same variable,
the admitted per-step width is intentionally more specific and wins inside the
child, including across the boxed scope's ambient export. A
quota is a ceiling, not a parallelism instruction, so the derived width applies
only when you expressed nothing — and in that case it is refined downward per
step from that step's own cores and memory cap. Either way the run prints one
line on stderr naming the variable, which value governs, and what the other one
would have been, so a later OOM is explicable. Your value is read once, in the
outermost process, and forwarded across the systemd re-exec under the separate
name `DAGRUN_OPERATOR_BUILD_JOBS`; the runner therefore never mistakes its own
scope-wide export for something you asked for.

`--max-mem` is a containment limit as well as a sizing input. Under boxing it becomes the
outer scope's `MemoryMax`, obeying the same one-way rule as
`DAGRUN_OUTER_MEMORY_MAX_BYTES`: it can tighten the derived 90%-of-`MemAvailable`
boundary, never widen it. The binding ceiling is named on stderr and the live value is read
back before work starts, so `--max-mem 20G` bounds what the run can actually take from the
host rather than only what its arithmetic assumed. A `MemoryMax` is a CEILING, not a
reservation: two such runs on one 32 GiB host are each bounded at 20 GiB, and neither is
holding 20 GiB — sizing a host for concurrent runs is still the caller's arithmetic. It is
a whole-run ceiling, not an admission gate either: two steps whose
caps each fit the budget can still be admitted together when their sum does not, and the
scope's own `memory.max` is then what stops the run.

`--max-mem` also derives a conservative, model-based `--max-steps` ceiling from the worst-case
footprint at each step's applied inner width. If an explicit step ceiling is
also present, the tighter value wins. Hard caps, learned/authored RSS, runtime
defaults, the outer safety factor, and selected `engine_only` steps all count;
intentional skips do not. If even one runnable step or the configured footprint
floor exceeds the budget, the run refuses instead of claiming one step fits.
CPA reports the same state as `infeasible-memory`. Named resources act as semaphores in
addition to the step and memory limits. A failed step prevents only dependent work. With
`--keep-going`, independent ready work continues to launch; without it, the final report names
every step that was not launched.

Under boxing, the run scope also receives `CPUQuota=<max-cpus>*100%`, and the
live `cpu.max` value is read back before work starts. This makes `--max-cpus N`
an N-core-equivalent **CPU-bandwidth** ceiling as well as the scheduler's width
ceiling for any one step. It is not an instantaneous thread-count or CPU-identity bound: CFS quota
may briefly run more than N runnable tasks on more than N CPUs and throttle them
later in the quota period. An unpinned run may also migrate from CPUs A/B to C/D
without exceeding its long-window budget. Use `--cores K` when exact eligible
CPU identities are required. The runner does not use `cpu.weight` as a cap.

Migration is deliberately explicit. Before 0.13, `run -j N` meant maximum
active steps; migrate that old intent to `run -s N`, or use `-s N -j N` when N
should bound both dimensions. In 0.13, the total-CPU long option was
`run --jobs N`; replace it with `run --max-cpus N`. The `-j N` shorthand keeps
its 0.13 total-CPU meaning. A hidden `run --jobs N` compatibility alias remains
temporarily so existing 0.13 scripts do not break, but it is omitted from help
and should not be combined with `--max-cpus`; differing simultaneous values are
rejected. New commands should use `--max-cpus`; the public
`sweep --jobs RANGE` spelling remains the option for inner widths being measured.
In 0.15, legal per-step widths no longer consume additive scheduler tokens:
`--max-steps` governs overlap and several steps may request more than
`--max-cpus` in aggregate while the boxed outer quota arbitrates their shared
bandwidth. Library callers that relied on 0.14's width-sum serialization should
use `max_steps`, named resources, or their own admission policy explicitly.

Dagrun refuses a `run` launched from inside another run's step. The outer scheduler sets
`DAGRUN_OUTER_RUN` to the step's `group.job` tag, so the refusal names the outer step that launched
it. This catches a second scheduler competing with its own parent for the same machine and
reporting an inner graph as though it were one outer step. A reviewed temporary exception may
pass `--allow-unwise-nest-dagruns`; the inner scheduler replaces the inherited marker with each of
its own step tags for any deeper descendant. Prefer flattening the caller into one DAG.

Per-step wall and CPU timeouts, memory limits, process-tree teardown, and OOM
attribution are enforced inside nested cgroups when the host supplies cgroup v2
and a delegated systemd user scope. If that capability is missing, the default
is to stop with a capability error. `--allow-cgroup-failure` accepts a
best-effort unboxed fallback with a warning. `--unsafe-no-cgroups` deliberately
skips containment even when available and should be reserved for reviewed use.

An unboxed run is **uncontained, not equivalent to boxed execution**. Exact CPU-time
accounting still requires the step cgroup's `cpu.stat`. Without it, the runner attempts a
best-effort procfs process-group lower bound: ordinary over-budget trees are reaped, but a process
that changes process group/session and CPU from exited descendants before their parent reaps them
can escape the measurement. The `capabilities` contract therefore remains
`uncontained.cpu_timeout=false`; stderr names the weaker fallback once per run instead of implying
cgroup-equivalent enforcement.

### A wall budget you did not write is derived, not guessed

A step's `timeout` is a wall-clock ceiling, and wall time is load-dependent: the
same number means something different on a laptop and on a 300-core host. So it
is only ever a hang backstop, and it is chosen to sit generously ABOVE the
CPU-second budget, which is the load-immune guard that should actually fire.

Omitting `timeout` therefore does not mean "unbounded", and it no longer means a
baked-in 1800 either. The effective bound is resolved, most specific first:

1. the step's own `timeout`, when it declares one;
2. the document's `default_step_timeout`, when it declares one;
3. **three times the step's platform-scaled `cpu_timeout`**, when the step
   declared a CPU budget and that is **larger** than 1800;
4. otherwise 1800 seconds.

**Rule 3 only ever loosens.** It is floored at 1800, so nothing that ran under
1800 seconds before runs under less now. Without that floor it silently retimed
every already-authored step that declared a CPU budget: `{"cmd": "git fetch
...", "cpu_timeout": 5}` burns about five CPU-seconds and blocks for minutes on
the network, and a fifteen-second ceiling reaps it and calls it a hang. Wall
time is unbounded relative to CPU time for anything that blocks, so a
CPU-derived ceiling is sound only as an upper bound. The case rule 3 is for is
the other one: a step declaring `cpu_timeout: 900` had a 1800-second wall
ceiling its own CPU guard could reach — at a 2.5x `cpu_timeout_multiplier` the
enforced budget is 2250 seconds, above the wall bound — so the wall guard fired
first and reported a hang where the truth was a slow machine. That step now gets
2700 seconds.

Rule 3 uses the **declared** CPU budget, not the small default every step
carries, so a step that declares nothing keeps its 1800-second backstop instead
of silently dropping to thirty seconds. It tracks the **platform-scaled** budget,
so raising `cpu_timeout_multiplier` for a slow platform moves the backstop with
it rather than letting the wall guard start racing the CPU guard there.

One thing to know when upgrading: the `--run-timeout` ordering check below is
applied to the resolved value, so a graph with a large `cpu_timeout` and a run
budget between 1800 and the derived ceiling is now **refused before it starts**.
That refusal is correct — the step really can occupy the run for that long — and
it is loud, which is the opposite of the silent retiming the floor prevents.

Because omission is now meaningful, an omitted `timeout` is also omitted when the
DAG is written back out — writing `0` would read as "no wall bound", which is the
opposite of what it means. The `--run-timeout` ordering check below is applied to
the resolved value, so a step that never wrote a number is still refused when its
effective ceiling would reach the run's.

### Bound the whole run, not only its steps

Per-step budgets cannot bound a run: any number of individually-legal steps can
sum past any ceiling. `--run-timeout SECONDS` adds an outer wall budget for the
run itself.

```sh
dagrun run --dag pipeline.yaml --run-timeout 900
```

On breach the scheduler stops launching, terminates every in-flight step's whole
process tree, marks those steps aborted with that reason, and **returns** — it
writes its profile rows and hands back a verdict rather than leaving the process
to be killed from outside, which would discard the evidence the bound exists to
capture. `DAGRUN_RUN_TIMEOUT` sets the same budget for a wrapper that
cannot edit the command line.

The bounds are ordered, and the ordering is the point:

| bound | enforced by | on breach |
| --- | --- | --- |
| per-step wall / CPU | the runner (CPU budget needs a cgroup) | that step dies and is named |
| whole-run wall | the runner | in-flight steps cut, rows written, verdict returned |
| scope `RuntimeMaxSec` | systemd, when boxed | the whole scope dies |

Each level exists to stop the next one from firing. The scope budget is derived
automatically as the run budget plus the larger of 60 s and a tenth of it, and
the in-scope process reads the property back off the live unit rather than
trusting the request; a mismatch is an error unless `--allow-cgroup-failure`.

Because a step allowed to run as long as the whole run could only ever be
terminated by the outer bound — attributing the overrun to the run instead of to
the node that caused it — a run whose steps declare a wall budget at least as
large as `--run-timeout` is **refused before anything starts**, with the
offending steps named.

Use `capabilities` for the machine-readable enforcement manifest. It has two
objects, `contained` and `uncontained`, carrying the same key set: the first is
what a boxed run enforces, the second is what survives when boxing is off.

## Select, parameterize, and stress a step

`--selected` runs the named tags and every dependency they require. Selecting a leaf therefore
runs its full ancestry in dependency order:

```sh
dagrun run --dag pipeline.yaml --selected test.unit
```

`--labels` runs every step carrying any requested label, plus their complete
dependency ancestry. Labels are independent of `group.job` tags, so a graph can
commit its complete step population once and expose stable subsets without
rewriting the document:

```yaml
- group: test
  job: unit
  labels: [quick, full]
  cmd: make test
  deps: [build.app]
```

```sh
dagrun run --dag pipeline.yaml --labels quick
dagrun run --dag pipeline.yaml --labels quick,super
```

Unknown labels are refused. `--labels` and `--selected` cannot be combined;
use labels for declared subsets and tags for individual-step selection.

When all prerequisite outputs are already present and must not be rebuilt,
`--ignore-selected-deps` runs only the named tags and drops dependency edges to steps outside the
selection:

```sh
dagrun run --dag pipeline.yaml --selected test.unit --ignore-selected-deps
```

A command opts into passthrough arguments by including the reserved `{args}`
token:

```yaml
- group: test
  job: unit
  cmd: pytest {args}
```

```sh
dagrun run --dag pipeline.yaml --selected test.unit --args='-k retry'
```

Passing `--args` is rejected unless a selected command declares the token.
Without `--args`, the token is removed. `--stress N` duplicates the selected
graph at generation into `N` disconnected components with no edges between
copies. Each copy retains the original graph's internal dependency edges.
Named-resource scheduling is removed from the generated copies, so
`--max-steps` controls how many copied nodes may be active while `--max-cpus`
caps each copy's requested width and their shared outer CPU bandwidth. The report includes the exact pass ratio
and the largest number of step child processes measured alive at once. The
modeled memory footprint must still fit the box. Expansion is also refused when
it would create more than 100,000 generated DAG nodes/control units, so a tiny
guest memory hint cannot turn `--stress` into an unbounded host-side allocation.

A singleton DAG can be generated on the fly; no `N`-node file is required:

```sh
printf '%s\n' '{"steps":[{"group":"stress","job":"singleton","cmd":"sleep 2"}]}' |
  dagrun run --dag - --stress 100 --max-steps 100 --max-cpus 100 --no-profile
```

### Repeat runs that write N distinct outputs

Every copy runs the *same* command, so a command that writes a fixed path has
`N` copies writing one file: the last writer wins, nothing errors, and anyone
reading the result is looking at one sample wearing the label of `N`. Each copy
therefore gets two variables in its environment:

| variable | value |
| --- | --- |
| `DAGRUN_COPY` | this copy's index, zero-padded to the width of `N` (`03` of `10`) |
| `DAGRUN_COPIES` | `N` |

Both are unset when the graph was not multiplied, so a command can tell a single
run from a copy:

```yaml
- group: demo
  job: repeat
  cmd: ./measure --out out/run-${DAGRUN_COPY:-single}.log
```

```sh
dagrun run --dag pipeline.yaml --selected demo.repeat --ignore-selected-deps --stress 10 -j 10
```

That produces `out/run-01.log` ... `out/run-10.log` from one invocation.
Repeating a run N times and comparing the results is a common reason to multiply
a graph, so prefer this to a hand-rolled loop.

> **If you are comparing the runs themselves, keep the identifier out of the
> program under test.** A process's environment is placed on its initial stack,
> so a tool that hashes or compares process state sees a per-copy variable as a
> difference -- and then N copies differ for a reason you introduced rather than
> one you were measuring. Measured with such a tool: with no extra variable the
> hash was stable across runs; adding `X=aaaa` changed it, and `X=bbbb` -- the
> same *length*, different content -- changed it again to a third value, so this
> is not only an alignment shift.
>
> Launch the program under test with whatever it offers for a fixed, minimal
> environment. Measured on the same tool, doing so made it blind to all of the
> above, 4 of 4 identical, while the copies still wrote 10 distinct files. The
> same DAG *without* that isolation produced 10 different results from 10
> identical runs -- and the runner reported `10/10 passed` either way, so nothing
> warns you.
>
> This applies equally to `DAGRUN_STEP`, the ownership nonce the
> scheduler sets on every step, which contains a pid and a nanosecond timestamp
> and so differs on every run.

## Profiles, sweeps, and portable summaries

Runs and sweeps append raw resource samples to `./.dagrun/profiles/` by default,
relative to the process's current working directory. For `sweep`, the primary
spelling is `--output-dir DIR`; the older `--perf-dir DIR` spelling remains an
alias. Other commands use `--perf-dir`. An explicit directory wins over
`DAGRUN_PROFILE_DIR`, which wins over the default. These flags change only the
artifact-store location: they do **not** redirect the sweep table, diagnostics,
or child-process output. `--no-profile` disables profile-store writes. Reading is independent: use
`--no-profile-feedback` when a plan must ignore an existing store. Every writer
prints the paths it wrote, so profiling is never silent. A sweep's store can
contain:

- `<machine_id>.csv`, the whole-trial rows;
- `step_profiles_<machine_id>_<container_class>.csv`, the raw per-step samples;
- `scaling_model_<machine_id>_<container_class>.json`, the derived model refreshed
  after a successful sweep; and
- `profile_report.html`, a standalone interactive history explorer refreshed
  after a successful sweep;
- `traces/<run_id>.csv` when `--profile-timeseries` is enabled; and
- private `captures/*/manifest.json` plus profiler artifacts when an expensive
  `perf` or `wprof` capture is requested.

Hidden `.locks/` directories contain persistent zero-byte advisory-lock files;
they coordinate concurrent writers and are not profiling records.

The store is machine-local measurement data and should normally be ignored by
source control.

### Single-step dense mode

Without `--target-time`, both `--step` and `--jobs` are required:

```sh
dagrun sweep --dag pipeline.yaml --step build.app --jobs 1..8
```

Here `--jobs 8` means **eight distinct trials**, at widths 1 through 8; it does
not mean one trial at width 8. `LO..HI` is likewise inclusive.

### Graph-wide target-time mode

For a graph-wide experiment, give a soft target allowance:

```sh
dagrun sweep --dag pipeline.yaml --target-time 10m
```

`--target-time` accepts a bare number of seconds or an `ms`, `s`, `m`, or `h`
suffix. It is an allowance, not a timeout: pass 1 always completes, even when it
runs past the target, and reports both its elapsed time and overrun. A later
pass starts only while elapsed time is still below the target; after it starts,
the entire pass is atomic and is never killed for crossing the target.

Within each pass, nodes run **one at a time** in stable topological order, so a
node's measurements are not contaminated by another DAG node running beside
it. Dependencies choose this visitation order; they are not rerun beside the
node being measured. Pass 1 uses powers of two through the process-visible physical-core count,
then the exact physical-core and logical-thread counts. CPU affinity and Linux
sysfs provide that topology, tightened by any effective cgroup CPU quota. On a
158-core / 316-thread machine, the grid is therefore
`1,2,4,8,16,32,64,128,158,316`. Pass 2 keeps every one of those widths and adds
the integer midpoint of each remaining gap (including 48 between 32 and 64);
later passes repeat that cumulative bisection. Re-running the anchors gives the
model replication as well as denser coverage.

In target mode, `--step TAG` optionally limits the experiment to one node and
`--jobs` optionally replaces the automatic first grid. It accepts the established
`LO..HI` and bare `N` forms, plus comma/range lists such as `1,2,4,8` or
`1,2,4..8`. A bare `--jobs 316` requests every width from 1 through 316; omit
`--jobs` to get the sparse topology grid instead.
`--repeat K` repeats every width within every pass: every sample is persisted,
while the displayed width row keeps the fastest wall time. A target of zero is
useful for a mandatory-pass-only smoke run. Intentionally omitted nodes are
reported without spawning; fixed/self-managed nodes are characterized only
once as described above. A failed width aborts the sweep with a nonzero result;
already-written raw rows remain, but the derived sidecar is refreshed only
after a successful sweep.

Every successful profiling-enabled sweep refreshes the standalone interactive
report at `<output-dir>/profile_report.html`. Use `--report-html FILE` to place
that HTML elsewhere, or `--no-report` to retain the CSV/model outputs without
building the report. The page embeds its data and code, so it can be opened
directly without a web server or network access. It provides:

- a Graphviz-style DAG whose node area is proportional to CPU time;
- click-through step detail;
- speedup, memory-response, and CPU-work-efficiency scatterplots with fitted
  per-width lines;
- filters for machine/container, workload revision, all commits, or the latest
  N commits; and
- interval parallelism plots when time-series rows are present.

A sweep deliberately invokes each resizable command many times in the same
working tree. The command must therefore be repeatable: it should perform the
same work from the same inputs at every width, rather than turning later samples
into incremental or no-op runs. If a workload mutates its inputs or outputs,
restore them outside dagrun before starting the sweep or benchmark a clean,
repeatable wrapper. Dagrun cannot infer a safe reset command for arbitrary DAG
work and will not delete build products on the user's behalf.

Each sample keeps the ordinary wall time, user and system CPU time, peak RSS,
effective cores, throttling, ambient-load and pressure signals when the host
can supply them. Target sweeps also stamp `sweep_mode`, `sweep_id`,
`sweep_pass`, `sweep_sample`, `sweep_repeat`, `sweep_width_source`,
`sweep_target_s`, `sweep_physical_cores`, `sweep_logical_cpus`, and a stable
`workload_digest`, so repeated and refined measurements remain attributable.
Planning selects the digest for the command shape currently in the DAG. Once an
exact match exists for a step, rows carrying another non-empty digest are never
mixed into that curve; before then, blank rows from stores created before digest
tracking remain a compatibility fallback.

### Portable profile compatibility

The on-disk store uses one append-only, column-named CSV contract. Compatible
dagrun installations can append data and rebuild estimates from rows written by
one another. New columns are appended, unknown columns are preserved or ignored
as appropriate, and missing cells remain “measurement unavailable” rather than
being coerced to zero. New records use the same UTC timestamp, lowercase
boolean, numeric, quoting, and LF line-ending conventions.

This is an explicit wire format, not automatic serialization of in-memory
objects. Each implementation converts its native measurement/model types to the
shared schema and validates that contract with compatibility tests.
Portable summary JSON and derived scaling-model JSON likewise use explicit,
deterministic schemas. Profiler-capture manifests use the shared
`dagrun-profile-capture-v1` JSON schema and explicitly mark their instrumented
trials as excluded from the model. The raw per-step CSV remains the authoritative
longitudinal dataset; `scaling_model_*.json` is a rebuildable cache.

### Parallelism over time

Aggregate wall and CPU time can hide a sequential startup or shutdown phase.
Add `--profile-timeseries DURATION` to `run` or `sweep` to sample each active
step's cgroup CPU counters and descendant thread count during its lifetime:

```sh
dagrun sweep --dag pipeline.yaml --step build.app --jobs 1..8 \
  --profile-timeseries 250ms --output-dir /tmp/dagrun-build-study
```

The interval accepts 50ms through 10s, including the ordinary `ms`, `s`, `m`,
and `h` duration suffixes. Time-series collection requires active cgroup-v2
containment and fails before starting a step when that evidence source is not
available. It cannot be combined with `--no-profile`.

Each trace contains an explicit start sample, periodic samples scheduled against
absolute deadlines, and a final sample before cgroup cleanup. Rows report
cumulative user/system/total CPU, interval effective cores, interval throttled
time, elapsed time, and observed descendant thread count. Missing or reset
counters stay blank rather than being reported as zero.

These higher-volume rows are written separately as
`<profile-dir>/traces/<run_id>.csv`; the command prints the exact path. Sweep
provenance follows the fixed trace columns. Traces diagnose phase behavior but
are not treated as independent trials by the duration, memory, or scaling
estimators; the aggregate `step_profiles_*.csv` rows remain their dataset.

### Opt-in perf and wprof captures

Expensive profilers run only when requested. Dagrun first completes the normal,
uninstrumented sweep and fits the economic plateau from **that invocation's**
samples. It then runs a fresh isolated copy of each selected resizable step at
that width, so profiler overhead never contaminates the ordinary scaling model:

```sh
dagrun sweep --dag pipeline.yaml --target-time 10m \
  --perf-record --perf-window 400ms \
  --wprof-window 400ms --wprof-window 400ms
```

`--perf-window DURATION` implies `--perf-record`; without an explicit window,
perf records the centered 80% of the expected step duration. Each repeated
`--wprof-window` requests another independent centered capture. Short steps have
their windows clipped so the profiler still avoids the first and last 10% when
possible. `--profiler-sudo` explicitly prefixes profiler commands with
`sudo -n`; privilege escalation is never automatic.

In target-time mode these requested captures run immediately after mandatory
pass 1. Their elapsed time counts against the overall allowance before dagrun
decides whether to begin a refinement pass. A capture that has started is not
killed merely because the allowance expires. Tool preflight or capture failure
is loud and nonzero, while the private failure manifest and diagnostics remain
available under `captures/`.

### Dataset versus model

The raw per-step CSV is the source of truth. The authored DAG remains portable
policy: measured speedup and memory curves are **not written back into it**.
The planner rebuilds a machine/container-specific model from the raw store. A
successful profiling-enabled sweep atomically refreshes its deterministic
derived sidecar beside the CSV as
`scaling_model_<machine_id>_<container_class>.json`; it is an
inspectable cache, not an authored input, and can always be deleted and rebuilt
from the dataset. The sidecar records the selected workload digest. Portable
summary schema 2 partitions its bounded reservoirs by `(step, width, digest)`,
so a stale cohort cannot consume the current cohort's per-width sample budget;
schema-1 summaries remain readable as blank-digest compatibility data.

For every step and measured width, the model retains robust wall time (both raw
and contention-discounted), total CPU seconds, achieved effective cores,
throttling, and a width-specific memory peak. A curve requires at least two
distinct positive widths. The recommended economic plateau is the **narrowest
measured width within 10% of the best eligible measured wall time**, not an
adjacent-ratio heuristic, so adding a midpoint does not move the answer merely
by changing its neighbors. Widths whose measured CPU seconds exceed 1.5 times
the baseline width's CPU seconds are normally excluded from plateau candidates;
missing CPU data does not fabricate a penalty. A separately reported
`regression_inner_jobs` requires both more than a 5% slowdown versus the fastest
width and non-overlapping observed wall-time ranges, distinguishing a real
cliff from a flat, noisy plateau.

CPA uses measured `user_s + sys_s` as a width's CPU-work area when available.
For older or unboxed rows without that signal it conservatively falls back to
`inner_jobs * modeled_wall_s`. Memory is also width-specific: the exact-width
90th-percentile peak becomes `M(p)` after at least three **uncensored** peak
samples at that width. A peak observed at an active memory ceiling is retained
only as a lower bound, never mistaken for exact demand. Until enough exact
evidence exists, allocation falls back to the ordinary pooled/profile or
authored RSS estimate and the conservative width-scaling rule; an authored hard
cap still wins. Exact `M(p)` carries its measurement width into runtime sizing,
so it is not multiplied by the old width heuristic a second time.

For a 32-core example, CPA can compare A at eight workers plus B at twenty-four
against both CPU capacity and their joint `M_A(8) + M_B(24)` footprint. Its
`--max-steps` input is an upper bound, not a demand: when the requested overlap
is memory-constrained, the model runs the fixed-overlap allocation at every
ceiling down to serial execution and chooses the candidate with the smallest
no-overcommit modeled makespan (ties retain more overlap). Only when no serial
seed fits does it declare memory infeasible. Among feasible measured widths it
spends the next cores where the positive critical-path wall reduction per added
core is largest, while excluding CPU-work-inefficient points. Thus a poorly
scaling task does not automatically receive either more or fewer cores;
dependency criticality, marginal wall benefit, work inflation, memory, and the
serial-versus-concurrent tradeoff together decide.

The `summary` command builds, merges, inspects, and plans from bounded portable
profile summaries. `run --profile-sync BACKEND` can download a shared summary
before planning and upload merged samples afterward. Run the relevant command
with `--help` for backend and direction syntax.

## Boxing one command

Boxing is this tool's primary purpose, and a DAG is not required to use it:

```sh
dagrun box --mem 512M --timeout 30 --cores 2 -- ./probe.sh
```

`box` builds a one-step DAG in memory and hands it to the ordinary run path, so
it is exactly equivalent to hand-writing the corresponding singleton-DAG file --
same containment, same evidence, same exit codes -- and nothing about the run is
special-cased for it.

`--mem` is applied twice on purpose: as the outer scope's `MemoryMax` and as the
command's own inner `memory.max`, so a breach is an OOM kill inside the box
rather than pressure on the host. It is also what the boxed step is MODELLED at,
so a small ceiling is admitted on its own terms: a DAG file's uncharacterized
steps are sized against an 8 GiB floor, and measuring `--mem 512M` against that
floor would refuse every value the flag exists to ask for. `--timeout` is a WALL
ceiling; the CPU ceiling
is derived from it as `--timeout x --cores`, because the small ten-second
per-step CPU floor is a forcing function for an undeclared DAG node and would
otherwise cut an honest ad-hoc command short for a reason its author never asked
about.

`box --cores K` is a CPU BANDWIDTH cap (`cpu.max` of K cores, plus an outer
budget of K), and is deliberately not the same thing as `run --cores K`, which
is a hard cpuset PIN that fails closed without an exact cgroup cpuset. Boxing one
command should not require that capability. Aliases `-j` and `--max-cpus` name
the same knob.

Argv after `--` is shell-quoted element by element, so an argument containing a
space, a quote or a `$(...)` stays one argument instead of becoming shell syntax.

## Host-wide memory admission

`--max-mem` gates ONE process against a snapshot of the host. It has no notion
of what other runner invocations have already committed to, so two runs started
a second apart each see the same headroom and both take it. `run --admission`
adds the missing shared state:

```sh
dagrun run --dag dag.json --max-mem 16G --admission
dagrun run --dag dag.json --max-mem 16G --admission 600   # wait up to 10 min
```

Admission reserves the run's `--max-mem` against a durable, `flock`-serialized
ledger every runner on the host shares -- the sibling of the core-reservation
ledger, with the same `(pid, /proc starttime)` dead-holder reclaim, so a crashed
run cannot subtract memory forever. It requires `--max-mem`: admission reserves
a NUMBER, and the only figure available without one describes the whole host.

There are three answers, not two:

| Verdict | Meaning |
|---|---|
| GRANT | Reserved and held for the life of the run. |
| QUEUE | It would fit on a quiet host, so waiting can help. Says how many holders are ahead, and which resource is in the way. |
| REFUSE | Bigger than the whole-host budget, so waiting can never help. Says the largest number that could be granted. |

A queued run exits 4 by default and prints why; pass `--admission SECONDS` to
wait instead, up to a ceiling of 86400 (one day) -- a longer wait is refused as
a usage error rather than accepted and then never honoured. Exit 4 is distinct
from 2 (bad usage) and 3 (cgroup boxing unavailable) so a retrying scheduler can
tell "the host is busy, come back" from "this invocation is wrong" without
parsing prose.

Inside a boxed run the reservation is made ONCE. Boxing re-execs the runner into
its systemd scope with `execvp`, which keeps the pid and the `/proc` start time,
so the run finds its OWN record in the ledger and does not ask again -- it would
otherwise queue behind itself. That skip is granted by the ledger record, not by
the in-scope environment variable: a runner invoked as a *step* of a boxed run
inherits the same variable while holding no reservation, and is admitted on its
own merits like any other run.

A grant needs BOTH `reserved + requested <= whole-host budget` (the condition
other runners affect) and `requested <= live headroom` (the condition non-runner
tenants affect -- a ledger cannot see a database that grew). The two are
reported separately because the remedies differ.

The budget defaults to 85% of `MemTotal` less a flat 8 GiB margin, and that
margin is capped at one eighth of `MemTotal` so it can never swallow a small
host's whole budget: held back whole, 8 GiB of an 8 GiB machine would leave a
budget of zero and refuse every run on that host.

Override the ledger path with `DAGRUN_MEM_LEDGER`, the aggregate budget with
`DAGRUN_ADMISSION_BUDGET_BYTES`, and the live-headroom reading with
`DAGRUN_ADMISSION_HEADROOM_BYTES`; an unparseable override is reported and the
host is measured instead, never silently taken as zero.

A boxed run admits ONCE, before cgroup bring-up: a run that is going to wait
should not be holding a systemd scope while it waits, and a run that is going to
be refused should never have created one. Boxing then re-execs the runner into
that scope, which keeps the pid and the `/proc` start time, so the reservation
already recorded is still this run's own and is not taken a second time.

## Collision-free CPU reservations

For benchmark isolation, reserve a chosen number of least-busy allowed cores
and run a complete process tree on them:

```sh
dagrun pin-run --cores 2 --tag parser-bench -- ./bench-parser
dagrun run --dag benchmarks.yaml --cores 4
```

Reservations use a durable cross-process ledger, never choose cores held by a
concurrent reservation, release on normal or failing exit, and reclaim records
whose owning process is dead. Enforcement is fail-closed: the exact reserved set
must become the effective cgroup cpuset. A process-affinity mask is not accepted
because a descendant can replace it.

No central daemon is required for the current fixed-slice mode: every allocator
serializes through the shared durable ledger and claims an explicit set of CPU
IDs. A future service could provide dynamic or fair machine-wide allocation,
but ordinary `--max-cpus` deliberately remains a shared bandwidth and per-step
width limit rather than pretending to hand out exclusive moving CPU slices.

The ledger path is `DAGRUN_CORE_LEDGER` when that variable is set. Otherwise it
is `$XDG_RUNTIME_DIR/dagrun/core-reservations.json` when the runtime directory
exists, or `<system-temporary-directory>/dagrun-<uid>/core-reservations.json`.
Serialization uses the private sibling `core-reservations.json.lock`; both
commands use the same files. A crashed holder is identified by PID plus process
start time and reclaimed by the next ledger operation, so the state records live
ownership rather than a permanent allocation. Unsafe, foreign, non-regular, or
malformed state is refused instead of ignored.

`pin-run` creates a transient `AllowedCPUs` scope and mutation-checks it before
launch. `run --cores K` changes only the runner's own verified scope, so it fails
with a capability result when combined with `--allow-cgroup-failure` or
`--unsafe-no-cgroups`. Reservation exhaustion also fails instead of choosing a
core held by another process.

The companion command exposes the allocator directly:

```sh
cpuset-alloc run --cores 2 --tag parser-bench -- ./bench-parser
cpuset-alloc status
cpuset-alloc reclaim
cpuset-alloc selftest
```

The `--` separator before the wrapped command is required. `selftest` directly
attempts to move a child onto an excluded CPU and verifies every assigned CPU is
usable; a missing or inconclusive mutation is a failure, not evidence of a hard
bound. CPU pinning is intended for controlled measurements, not ordinary CI.

## The names runners coordinate through

Independent runs on one host agree with each other through four named things:
the environment variables that configure them, every one of which begins
`DAGRUN_`; the shared systemd slice `dagrun.slice`, whose single `CPUQuota`
bounds the sum of every boxed run; the core-reservation and memory-admission
ledgers described above; and the default profile store `./.dagrun/`.

Those names *are* the coordination, so a run that uses different ones does not
coordinate at all — and none of these mismatches announces itself:

- A variable whose name does not begin `DAGRUN_` is not read. The run takes its
  default instead, and says nothing.
- Runs that box under different slice names occupy different places in the
  cgroup tree, so no single `CPUQuota` bounds their sum. Runs that read
  different ledger files hand out the same cores to two owners and admit twice
  against the same RAM. Let the host drain before changing which names are in
  use, or accept that reservations and admissions hold only among the runs that
  share them.
- An operator drop-in applies to the slice it is filed under, so install it as
  `dagrun.slice.d/`.
- A profile store kept under some other directory name is not found. The
  planner then works from the estimates written in the DAG rather than from
  measurements; it does not fail, and `--profile` will show it learning again
  from the first run onwards.

## Command summary

| Command | Purpose |
|---|---|
| `run` | Execute a DAG under scheduling and containment constraints. |
| `box` | Box ONE ad-hoc command with `--mem`/`--timeout`/`--cores`, no DAG file. |
| `plan` | Show estimates, critical path, widths, and order without running. |
| `list` / `ascii` / `dot` | Inspect or visualize the graph. |
| `json` / `yaml` | Validate and convert the DAG. |
| `sweep` | Measure one step or a whole DAG across inner job widths and target-time passes. |
| `summary` | Build, merge, inspect, or consume portable profile summaries. |
| `pin-run` | Reserve disjoint cores and run one command tree. |
| `capabilities` | Print the enforcement-capability manifest. |
| `quickstart` | Print a runnable introduction. |

All commands support `--help`; the top level supports `--version` and
`--userguide`.

## Exit behavior

Successful inspection and all-passing runs exit zero. Invalid input or command
usage is nonzero. A run with a failed step is nonzero. Failure to establish
required containment has a distinct nonzero capability result. `pin-run` and
`cpuset-alloc run` return the wrapped command's exit status after releasing the
reservation; signal termination uses the conventional `128 + signal` status.

## License

MIT
