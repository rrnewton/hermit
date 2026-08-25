# Storage redesign: validate and pressure-test records

Status: proposed plan. Part 1 measured 2026-08-24 against hermit `8bc26dba1d`;
population, landing states and the scheduling finding re-measured 2026-08-25
after #2396 and #2444 landed.

Part 1 states the architecture as it actually is, measured rather than recalled,
because an earlier description of it was wrong in a way that changed what kind
of problem this is. Part 2 is the redesign proposal. Nothing in Part 2 is
implemented; it is written so the owner can rule on a design rather than on a
recollection.

---

# Part 1 — The architecture as measured

## Why this document exists

Three tools can measure whether a cell flakes. None of them keeps the answer.
The question "has this cell ever failed, how often, and where did it diverge"
cannot be answered from anything checked in, and the reason is not that the
measurement is hard — it is that **no writer owns the series**.

This document states the architecture as measured, so the gap can be closed
deliberately instead of by adding a fourth measurer.

## The single most important correction

It is natural to describe this as "validate and the pressure test both write
`cells.json`, with different authority and no stated boundary". That is
**wrong**, and the correction matters because it changes what kind of problem
this is.

`ci/compat-envelope/scorecard.rs` is the **sole writer** of
`ci/compat-envelope/cells.json`. Measured:

- `ci/compat-envelope/pressure-test.rs` only **reads** it —
  `fs::read_to_string` at `pressure-test.rs:1652`, guarded by its own
  independent `TRACKED_CELLS_SCHEMA` pin at `pressure-test.rs:55`. Its writes go
  to `run.json`, `summary.json`, `runner-outcomes.json` and `dag.json`, all
  under ignored output.
- `scripts/validate.rs` only invokes `scorecard.rs verify-results`
  (`validate.rs:3012`) — a verification path. The compat-envelope README states
  the same intent: "Normal validation changes no tracked scorecard file."

So this is **not** two tools racing on a file. It is **one tool, two commands,
two input sources, two different authorities** — which means the boundary is
enforceable in one place rather than being an architectural property that has to
be negotiated between programs. That is a much cheaper problem than it looks.

| Command | Input source | Fields it owns |
|---|---|---|
| `scorecard.rs update` | the manifest + `ci/expected-e2e-plan.json` | `schema`, `id` (`lane`/`category`/`test`/`mode`/`backend`), `enabled`, `status`, `ci_disabled_reason` |
| `scorecard.rs update-observations` | a pressure-test `summary.json` | `observations[]` in full |

The boundary is real and currently correct in behaviour. It is simply **not
written down anywhere**, and nothing enforces that a future edit keeps it.

## The schema is not one type

`cells.json` has no single defining Rust type. It is serde-derived across six
types in one file, and that file is a `rust-script` executable rather than a
library — so there is no crate, no rustdoc, and no stable type link to hand
someone. The root is `TrackedCells`.

- [`scorecard.rs#L97-L146`](https://github.com/rrnewton/hermit/blob/8bc26dba1d53cbb8120db6f73456ad5ea788a8ca/ci/compat-envelope/scorecard.rs#L97-L146)
  — `TrackedCells`, `TrackedCell`, `CiDisabledReasonData`, `CellStatus`, `Observation`
- [`scorecard.rs#L222-L226`](https://github.com/rrnewton/hermit/blob/8bc26dba1d53cbb8120db6f73456ad5ea788a8ca/ci/compat-envelope/scorecard.rs#L222-L226)
  — `ObservedRange`

`ci_disabled_reason` is prose, not structured divergence data:
`CiDisabledReasonData { result: Option<String>, evidence: Option<String>,
reason: String }`. Its population is sparse (373 of 5520 cells when measured; the denominator has since grown to 5584) and **that is
correct** — a note exists only where someone tried and failed. It is not a
backfill target, and reading its sparseness as missing data is a misreading.

## Where the records actually live, and the unexplained split

| Producer | Raw output (all untracked) | Durable record | Repo | Version controlled |
|---|---|---|---|---|
| validate | `ignored/validate/artifacts/`, `ignored/validate/runs/` (774 run records) | `ledger/<product>/<host>/<YYYY-MM>.jsonl` — 3 shard files, 1,959 rows | **parent** `dev-hermit` | **YES** |
| pressure test | `ignored/compat-envelope/<run>/` (10 run dirs) | `ci/compat-envelope/cells.json` | **hermit** | **YES** |
| `bin/hermit-repeat` | `ignored/hermit-repeat/` (51 dirs) | **none** | parent | n/a — nothing is kept |

**Both durable records are version controlled.** This is worth stating
explicitly because the phrase "the ledger lives in the parent" invites the
reading that it is scratch state, and it is not: `ledger/` is tracked in the
`dev-hermit` git repository, as are `ci-hub/validate/flaky-cells.json` and
`bin/hermit-repeat`. The asymmetry is **which repository**, not whether.

**The split is unexplained.** Validate's raw output and its ledger live in the
parent; the pressure test's raw output and its tracked table live in hermit.
No document states why, and the two halves have different properties as a
result:

- A pressure result committed to `cells.json` **moves the hermit SHA**, so it
  changes the thing being measured. A validate result committed to the parent
  ledger does not.
- Conversely, the parent ledger is not visible from a hermit checkout, so a
  hermit-side reader cannot see validate history at all — not because it is
  untracked, but because it is tracked in a *different* repository.

Whichever way the redesign goes, the split should be a **stated decision** with
its consequence named, rather than an accident of where each tool was written.

## What is measured today and thrown away

- **`bin/hermit-repeat` keeps nothing.** It measures distributions across
  repeated runs and writes 51 output directories under
  `ignored/hermit-repeat/`. A grep across every `.py`, `.rs` and `.sh` in the
  workspace finds **no consumer of that path**. The measurement is real; the
  series does not exist.
- **`ci-hub/validate/flaky-cells.json` is hand-maintained.** It holds exactly
  **one** entry — `command_strict_verify`, 9 pass / 1 fail of 10, `measured_at`
  2026-08-04, three weeks stale as of this writing. It is consumed by
  `scripts/lib/validate_runtime.rs:89` and `ci-hub/validate/flake_class.py` to
  reclassify a red as needs-rerun. Its own `_comment` requires a measured
  pass/fail sample with provenance before a cell may be added — a good rule with
  no mechanism behind it, which is why there is one entry.
- **`nextest` has zero retries configured** anywhere in the tree. Nothing
  incidentally samples a cell more than once.
- **`ObservedRange` is populated on 2 of 5584 cells**, and the precise version is
  worse than the round number: only **one** of those two carries an actual
  divergence range (`data-handling/sqlite-query-determinism verify sabre`:
  record 93–94, scheduler turn 68–69, virtual nanoseconds …317250–…354000,
  `samples: 2`). The other is a `pass` with all four coordinates null.
  Re-measured on `main` after
  [#2396](https://github.com/rrnewton/hermit/pull/2396) landed 2026-08-25.
- **Both existing observations are `provenance: pressure-test`.** The
  validate-side writer `observe-results` landed with #2396 and has written
  **zero** rows, because nothing invokes it. See the next section.

## The divergence coordinates

`Observation` carries three coordinate ranges on `main`
(`first_divergent_record`, `first_divergent_scheduler_turn`,
`first_divergent_virtual_nanoseconds`) plus a fourth, `first_divergent_syscall`,
which landed with #2396 on 2026-08-25 and is null in the one populated cell.

These are **different keyspaces and must never be read against one another's
axis**. One real measured divergence was record 98, syscall 37, scheduler turn
4. A reader who averages or compares across coordinates is producing nonsense.

On `main` the validate side does not populate any of them: the harness in
`ci/manifest-plan/src/` mentions the coordinates only as literal `null` in a
`canonical_verdict.rs` test string. The value is computed in
`hermit-cli/src/bin/hermit/verify.rs` and **dropped one hop before any cell sees
it**. #2396 adds the wiring (`runner.rs` +218, `canonical_verdict.rs` +126).

## NOTHING SCHEDULES THE PRODUCER

This is the finding that determines whether the series can ever exist, and it
outranks every schema question below.

**Measured 2026-08-25, exhaustively. No scheduler of any kind invokes
`pressure-test.rs`, `scorecard.rs update-observations`, or
`scorecard.rs observe-results`:**

| Candidate scheduler | Result |
|---|---|
| `ci/dag/*.json` validate DAG nodes | no node invokes any of the three |
| `scripts/validate.rs` | invokes `scorecard.rs verify-results` ONLY — read-only |
| `.github/workflows/` | no reference to any of the three |
| `crontab -l` | no matching entry |
| systemd user timers | none; the only relevant timer is `hermit-health-tick` |
| `ci-hub/health/tick-hub.yaml` gates | zero gates reference them (its six "observation" hits are the English word, in comments about the ledger spool and saturation gates) |

The code says the same thing in its own doc comment on `observations`:

> ORDINARY VALIDATE STILL NEVER CHANGES THIS ARRAY. Two commands write it, both
> explicit and opt-in […] Neither runs as part of a normal validate, so the
> tracked file stays untouched by routine runs.

That opt-in design is CORRECT — a routine validate must not rewrite the tracked
table. But "opt-in" was only ever half a design, and the other half was never
built: **nothing else opts in either.**

### The consequence, stated plainly

The divergence data populates **only when a coordinator asks for a campaign by
hand.** There is no periodic job, no post-validate hook, no trigger of any kind.

So the population will stay at 2 of 5584 cells — one of them carrying an actual
range — indefinitely, however many producers exist and however good the schema
is. Running more campaigns by hand does not fix this; it is the same manual act
repeated, and it stops the moment nobody remembers to do it.

The sharpest evidence is `observe-results`. It is the validate-side writer, it
landed with #2396, it works, and it has written **zero rows** — because no
caller exists. A writer with no scheduler is indistinguishable from an absent
writer at the only thing that matters, which is whether rows accumulate.

### What this means for the plan

Phase 3 below ("point the producers at it") is therefore NOT the small wiring
step it reads as. It is the load-bearing phase, and it needs a scheduling
decision the repository has never made:

- **What triggers a sample?** Per validate run, per landed commit, periodic
  (nightly/weekly), or on a red cell transitioning?
- **Who pays for it?** The box is already the bottleneck — main lands roughly
  one commit every 22 minutes against a ~26-minute full receipt, so a campaign
  competes with landings for the validate lock.
- **What stops it perturbing the measured tree?** Per requirement 5 below, if
  the trigger writes to `cells.json` it moves the hermit SHA on every sample.

None of those are schema questions, which is why they are stated here rather
than in the schema sections. A durable series with a perfect schema and no
trigger is still an empty table.

## The gap, stated exactly

The gap is **not a measurer.** Three tools can already measure a flake rate,
each in its own format:

1. the pressure test, via `--repetitions` (already on `main`);
2. `bin/hermit-repeat`;
3. repeated validate runs recorded in the parent ledger.

The gap is a **durable per-cell flake-rate series with a writer** — one owner,
one location, one schema, appended to over time rather than recomputed and
discarded.

Everything needed to produce a sample exists. Nothing is responsible for keeping
the sequence of samples.

---

# Part 2 — The redesign

## What a solution has to satisfy

Constraints that fall out of the measurements above. These are the acceptance
criteria for the design that follows:

1. **One writer per field, stated.** The `update` / `update-observations` split
   already works; it needs to be written down and enforced, not rebuilt.
2. **Appending must not corrupt the ratchet.** `status` is a ratchet; a flake
   series is evidence. They have different lifetimes and must not be able to
   overwrite each other.
3. **A repeat run at the same tree resets a range rather than accumulating
   across trees.** Ranges are already keyed by `(detcore_tree, provenance)`.
4. **A sample count is mandatory, not optional.** "earliest 80, latest 500" is a
   different claim over two runs than over fifty, and the pair alone cannot
   distinguish them. #2396 adds `samples` per coordinate for exactly this.
5. **Writing results must not move the SHA being measured**, or the act of
   recording perturbs the experiment. This is the parent-versus-hermit split
   above, and it is the one genuinely open architectural question.
6. **Absence must stay distinguishable from zero.** An empty field means "never
   written", not "never diverged"; `ci_disabled_reason`'s sparseness is correct
   and must not be backfilled into a false denominator.

## The proposal: a series in the parent, a projection in hermit

The single decision that drives everything else is **where the series lives**,
because requirement 5 conflicts with the current location. Three options were
considered:

| Option | Series location | Satisfies "recording must not move the measured SHA" |
|---|---|---|
| A | `cells.json` in hermit, as today | **No** — every recorded sample moves the hermit SHA |
| B | a new tracked file in hermit | **No** — same defect, tidier |
| C | the parent, beside the existing ledger | **Yes** |

**Recommendation: option C.** It follows the grain of what already works. The
parent already holds the only durable series in the system — the version
controlled `ledger/`, sharded by product, host and month across 3 files holding
1,959 rows — with a spool/union model already built for concurrent writers. A per-cell series is the
same shape of data with a different key, so it reuses the sharding, the
concurrency story, and the "does not perturb the measured tree" property for
free.

**The cost of option C, stated rather than hidden:** a hermit checkout cannot
see the parent. Someone reading `cells.json` from a bare hermit clone would see
no history at all. The mitigation is the second half of the proposal.

### The two halves

**The series (parent, authoritative, append-only).** One row per measurement,
never rewritten:

```
(cell_id, hermit_sha, detcore_tree, provenance, result,
 first_divergent_{record,scheduler_turn,virtual_nanoseconds,syscall},
 depth{hermit,reverie}, run_id, invocation)
```

Append-only is what makes it a *series* rather than a snapshot: a flake rate is
`fails / (fails + passes)` over rows, and no existing store can answer that
because every existing store overwrites.

**The projection (hermit, derived, deliberate).** `cells.json`'s `observations`
array becomes explicitly a **projection of the series at a stated point**, not
an independent record. It carries what a reader needs in-repo — the ranges, the
`samples` count, the `provenance`, and critically the `last_tested`
`{hermit_sha, detcore_tree, depth}` staleness marker — and it is regenerated
only when someone deliberately publishes, so it moves the hermit SHA on a human
decision rather than on every measurement.

This is the part worth noticing: **`last_tested` already exists in #2396.** The
projection's staleness marker is built. So is `provenance`, so is per-repo
`depth`, so is `samples`. The redesign is substantially *completing #2396's
direction*, not replacing it.

### The writer is built; the trigger is not

The gap is NOT the writer. As of 2026-08-25 every writer is on main; what is
missing is the store and, more importantly, anything that CALLS them:

| Piece | State |
|---|---|
| `observe-results` command (the append entry point) | **landed**, #2396 — but never invoked, zero rows written |
| validate-side wiring so a validate run can emit a divergence position at all | **landed**, #2396 (`runner.rs` +218, `canonical_verdict.rs` +126) |
| row-independent fold, so a batch of N cells equals N single-cell runs | **landed**, #2396 |
| `samples`, `provenance`, `depth`, `last_tested` | **landed**, #2396 |
| the append-only series store itself | **not written** |
| **a trigger that invokes any writer** | **not written, and not designed** |
| a third producer path for `hermit-repeat` | **not written** |
| `flaky-cells.json` derived rather than hand-kept | **not written** |

The honest summary is that this redesign is now a store plus a TRIGGER, not a
rewrite — #2396 landed on 2026-08-25, so every writer in the table above except
the store itself is on main. The trigger is the part nobody has designed; see
"NOTHING SCHEDULES THE PRODUCER" above.

### Phases, in dependency order

**Phase 0 — land [#2396](https://github.com/rrnewton/hermit/pull/2396). DONE,
landed 2026-08-25.** Its types and its `observe-results` entry point are on main,
so the remaining phases can be built without rebasing twice.

**Phase 1 — enforce the writer boundary in code.** Today the
`update` / `update-observations` split is correct by convention and unenforced.
Make `update` refuse to modify `observations`, make `update-observations` refuse
to modify `id`/`enabled`/`status`/`ci_disabled_reason`, and add a self-test that
fails if either does. This is small, has no dependency on the store, and turns
the one enforceable boundary into an enforced one.
*Done when:* a deliberately mis-scoped write fails the self-test.

**Phase 2 — the series store.** Define the row schema above and site it in the
parent beside the ledger, reusing the shard/spool/union mechanism.
*Done when:* two concurrent writers can append without loss, proven by test
rather than asserted.

**Phase 3 — point the producers at it.** Pressure test via `observe-results`;
validate via the #2396 runner wiring; `hermit-repeat` via a summary emitter in
the same schema, which is what stops its output directories being dead.
*Done when:* all three producers append rows and a flake rate can be computed
for a cell that all three have touched.

**Phase 4 — make `observations` a projection.** Regenerate from the series;
require `last_tested`; state in the file that it is derived.
*Done when:* deleting and regenerating `observations` reproduces it exactly from
the series.

**Phase 5 — derive `flaky-cells.json`.** Compute it from the series and delete
the hand-maintained list. `flake_class.py` and `validate_runtime.rs:89` keep
their interface and gain a denominator that is not three weeks old.
*Done when:* the file is generated and its single hand entry either reproduces
from measured rows or is dropped as unsupported.

### Deliberate non-goals

- **Do not add a fourth measurer.** Three exist. The whole point is that
  measurement is not the gap.
- **Do not turn on `nextest` retries.** Retries hide flakes; this design
  measures them. Zero retries is the correct setting and should stay.
- **Do not backfill `ci_disabled_reason`.** Its sparseness is correct — a note
  exists only where someone tried and failed. Backfilling it would manufacture a
  denominator that means nothing.
- **Do not let the projection become writable.** If anything writes
  `observations` other than the regeneration step, the series stops being
  authoritative and the two records drift.

### What would reverse this recommendation

If the parent workspace is ever not guaranteed present for a hermit
checkout that needs history — for example if hermit CI must answer "is this cell
flaky" from a bare clone with no parent — then option C's visibility cost
becomes disqualifying and option B is correct despite moving the SHA. That is a
question about how hermit is consumed, not about this data, and the owner is
better placed to answer it than the measurements are.

## Related open work

| Item | State |
|---|---|
| [#2396](https://github.com/rrnewton/hermit/pull/2396) — 4th coordinate, `samples`, `provenance`, per-repo `depth`, `last_tested`, validate-side wiring, and the row-independent fold with `observe-results` | **LANDED 2026-08-25** |
| [#2444](https://github.com/rrnewton/hermit/pull/2444) — denominator provenance beside the published count | **LANDED 2026-08-25** |
| [#2446](https://github.com/rrnewton/hermit/pull/2446) — unrelated to storage; adds one manifest cell | open, not landed |

The fold work tracked as `implement-fold-option-b-skip-and-name-untrustworthy-rows`
is **not a separate PR**: row independence, per-row skips and the new
`observe-results` command are bundled inside #2396.
