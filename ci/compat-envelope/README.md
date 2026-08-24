# Compatibility scorecard

The compatibility scorecard answers one question from a Hermit checkout: how
many manifest-declared test, mode, and backend combinations are inside the
known-green regression envelope at this commit?

Start at [`SCORECARD.md`](../../SCORECARD.md). It is intentionally a small,
versioned table. The stable per-cell identities behind the totals live in
[`cells.json`](cells.json). Raw results, logs, durations, timestamps, and host
data are not versioned; each validate run retains those under `ignored/`.

The denominator is the complete comparable manifest matrix, not just the
combinations that happen to be enabled today. For `N` manifest tests, verify,
replay, and chaos span five Hermit backends, while native contributes one
naked-execution control: `N × (5 × 3 + 1)` cells. Native is shown as a sixth
backend in the table, but it does not have replay or chaos cells, so the formula
is not `N × 6 × 3`. Explicit `custom` commands still run when selected by
ordinary validation, but they are not multiplied across every test/backend
pair: unlike the three common modes, they do not define a uniform product-wide
denominator.

For one dated example only: on 2026-08-13, `N = 336`, so the comparable matrix
has `336 × (5 × 3 + 1) = 5,376` cells. The checked-in table is generated from
the live manifest and changes automatically when a manifest test is added.

`hermit-manifest-plan --format
matrix-json` emits both sides of each manifest's required enabled/disabled
partition. It also emits the validated per-test timeout and the number of
`execute_attempt` calls the existing harness makes for each mode. A seedless
chaos mode has `attempts: null`: it remains red but has no command to run. The
pressure-test entry point consumes this output instead of parsing the manifest
a second way. A disabled combination is red; a cell that cannot run is not
green. The existing `--format json` and text views remain enabled-only because
they are execution plans rather than scorecards.

## Ordinary validation

Run:

```console
./validate.sh
```

The path is deliberately direct:

1. `hermit-manifest-plan` validates the complete matrix and emits the enabled
   execution plan.
2. `ci/expected-e2e-plan.json` identifies the cells ordinary validation runs.
3. Each manifest bucket writes schema-4 `results.jsonl` rows, including the
   literal argv, explicit environment, working directory, and pasteable shell
   command, to a unique durable
   result directory.
4. The final `scorecard.compatibility` node requires a clean, exact-HEAD PASS
   row for every selected cell and prints the table.
5. The checked-in table and cell identities must still equal what the manifest
   and expected plan derive. Normal validation changes no tracked scorecard
   file.

`SCORECARD.md` reports the current regression-cell count. Explicit custom
commands remain required validation checks even though they are outside this
uniform comparable denominator.

The Basic Sanity Milestone 1 `verify` cells run each selected backend twice against
itself. Bare `--verify` still uses the legacy Stripped comparator. These cells
therefore measure same-backend repeatability under the current contract; they
do not establish strict INFO-log determinism or cross-backend parity. The
scorecard says this directly and reports no cross-backend parity count until
the manifest has cells that really compare fresh ptrace and non-ptrace logs.

A green cell turning red makes validate fail. The normal response is to fix the
regression. Moving the cell out of the selected plan is not a fix, and
`scorecard.rs update` refuses green-to-red movement unless an explicit
compatibility-standard transition requests it.

See every command and the exact green definition with:

```console
./ci/compat-envelope/scorecard.rs --help
```

## Updating the checked-in table

After deliberately adding a newly proven cell to `ci/expected-e2e-plan.json`,
run:

```console
./ci/compat-envelope/scorecard.rs update
git diff -- SCORECARD.md ci/compat-envelope/cells.json
./validate.sh
```

Review the table delta and the exact cell identity. The update command does not
run a test and cannot turn a red cell green by itself; the subsequent validate
must execute the newly selected cell.

## Red cells and the periodic full-matrix run

Every manifest cell outside the green set is red, including cells that have not
run and cells that cannot currently run. That conservative classification is
intentional: absence of evidence is not green.

The per-cell `observations` arrays are written only from completed, clean
pressure-test summaries. Ordinary validation never changes them. During
investigation, probe one exact red cell with a tight wall-clock cap:

```console
./ci/compat-envelope/pressure-test.rs run \
  --test applications/example-timed-progress-bar \
  --mode verify --backend ptrace --cell-timeout 60
```

Repeat that same tracked red cell without changing its scorecard state:

```console
./ci/compat-envelope/pressure-test.rs run \
  --test applications/example-timed-progress-bar \
  --mode verify --backend ptrace --repetitions 20 --jobs 4 --cell-timeout 60
```

The repeated summary retains the outcome distribution, the divergence-rate
Wilson interval, and histograms for each published first-divergence coordinate.
Every bound names N, configured cell width, scheduling mode, manifest mode, and
the exact Hermit and Detcore trees. It also reports the repetition at which each
observed minimum last changed, including whether it changed at N. Repetition
uses one shared build and fixture preparation followed by N independent boxed
cell nodes; it never writes the tracked scorecard.

For a reproducible bounded sample across verify, replay, and chaos, run:

```console
./ci/compat-envelope/pressure-test.rs run \
  --sample 10 --seed 42 --cell-timeout 60
```

Add `--mode verify` to sample only the first improvement target. Custom commands
and native naked controls are not part of an unqualified random sample. The
seed and every selected identity are retained in `run.json`.
Chaos cells whose manifests declare no seeds remain red but are not executable.
An exact request refuses before creating a plan; a batch reports and omits
those cells instead of inventing a default seed or recording a zero-execution
failure. The scorecard denominator is unchanged.

Generate the same graph without executing it by replacing `run` with `plan`
and supplying `--results DIR`. A request for every red cell is accepted only
when its declared worst-case occupancy fits `--run-timeout`; otherwise the tool
refuses and tells the caller to select a bounded sample or deliberately provide
a larger wall-clock bound. It does not pretend that thousands of cells fit in
the two-hour default.

The current improvement sequence starts with verify. A verify-only sample does
not change its denominator or green definition:

```console
./ci/compat-envelope/pressure-test.rs run \
  --mode verify --sample 10 --seed 42 --cell-timeout 60
```

The command reuses the canonical Hermit/resource build nodes, serializes
fixture preparation, and gives every red cell its own cgroup-boxed node.
The plan derives that build closure from the selected cells: a sample without
LiteInst does not build the LiteInst runtime, while any sample containing a
LiteInst cell retains the complete canonical LiteInst build chain.
`run` first materializes the exact committed SHA in a temporary local clone,
so ignored Cargo output in the primary checkout cannot change the experiment
and no shared worktree registry is touched. The generated clone is removed
afterward while the run directory remains retained.
Enabled red cells use the ordinary exact-cell selector; disabled red cells use
the harness's explicit `--probe-disabled` selector. Each cell gets at most the
shipped portable DAG's existing 600-second bucket allowance; the manifest's
smaller per-attempt timeout still applies inside it. Expected nonzero exits,
timeouts, OOMs, and no-result outcomes stay red but do not stop later cells. If the
cgroup runner itself stops after a bounded cell is killed, the command keeps a
conservative attempt marker and starts another DAG pass; completed builds,
preparations, and cells are not repeated. KVM cells retain the canonical
privileged DAG's 16 GiB hard cap even when their manifest lane is portable. A
malformed published per-cell artifact, or a missing artifact without a
narrowly proven runner timeout or OOM, becomes an infrastructure-error row;
the tool finishes the table and writes `summary.json`, then returns nonzero
rather than claiming a complete population. The retained runner profile is
what distinguishes an OOM or boxed cell timeout from an ordinary nonzero
harness exit. A timeout requires either that exact runner row plus the attempt marker,
or the test harness's separate GNU-timeout signal report plus its named
per-attempt timeout result; exit 124 alone is not timeout evidence. Cell results
are published from an `in-progress` path only after the harness returns; that
path is never terminal evidence, so an empty file created before a runner kill
cannot masquerade as a malformed terminal result. The combined `crash-error`
result contains remaining nonzero harness
exits, including signal-caused crashes when the shell reports a nonzero status;
the pressure runner does not currently distinguish the originating signal. A
missing result, verification report, or retained log is accepted only when an
exact-SHA, exact-step runner row records an OOM kill and the cell's numeric
attempt marker exists. Any artifact that does exist must still parse and match
the selected cell.

The ignored run directory retains `dag.json`, `run.json`, captured per-cell
stdout/stderr, result rows, runner profiles, and `summary.json`. Verify-mode
attempts also retain both raw INFO logs named by Hermit. A ptrace verify attempt
runs the same Hermit binary's one-input `log-diff` command and retains the
normalized first-run INFO stream for later cross-backend parity work. Retaining
that input is preparation, not a parity result.
Replay-mode raw-log retention is not implemented yet. A one-time PASS is
printed as a candidate for repeated confirmation; it never edits the tracked
green set automatically.
See the complete command contract with:

```console
./ci/compat-envelope/pressure-test.rs --help
```

This ports the useful one-box-per-red-cell shape from the old parent-workspace
`compat-envelope/expansion-dag.rs`. It deliberately does not port the parent
CSV dependency, invented fallback backend multipliers, or evidence-directory
deletion.

After a clean periodic run, deliberately merge its red-cell measurements with:

```console
./ci/compat-envelope/scorecard.rs update-observations \
  --summary ignored/compat-envelope/pressure-<SHA>-<time>/summary.json
git diff -- ci/compat-envelope/cells.json
```

The command requires the summary's Hermit commit and Detcore tree to equal the
clean checkout at `HEAD`, refuses infrastructure-error rows, and updates only
red-cell observations. For repeated measurements of the same Detcore tree, it
retains the exact Hermit commits measured, every observed result, and the
earliest and latest first-divergence scheduler turn and virtual nanosecond. A
determinism, replay, or parity failure with no measurable divergence point
keeps null fields; it does not get a guessed number. A new Detcore tree gets a
separate observation. Neither this command nor ordinary green regression
validation changes the green set.
