# Storage redesign: validate and pressure-test records

Status: proposed plan, measured 2026-08-24 against hermit `8bc26dba1d`.

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
reason: String }`. Its population is sparse (373 of 5520 cells) and **that is
correct** — a note exists only where someone tried and failed. It is not a
backfill target, and reading its sparseness as missing data is a misreading.

## Where the records actually live, and the unexplained split

| Producer | Raw output | Durable/tracked record | Repo |
|---|---|---|---|
| validate | `ignored/validate/artifacts/`, `ignored/validate/runs/` (774 run records) | `ledger/hermit`, `ledger/reverie` (2 shards) | **parent** `dev-hermit` |
| pressure test | `ignored/compat-envelope/<run>/` (10 run dirs) | `ci/compat-envelope/cells.json` | **hermit** |
| `bin/hermit-repeat` | `ignored/hermit-repeat/` (51 dirs) | **none** | parent |

**The split is unexplained.** Validate's raw output and its ledger live in the
parent; the pressure test's raw output and its tracked table live in hermit.
No document states why, and the two halves have different properties as a
result:

- A pressure result committed to `cells.json` **moves the hermit SHA**, so it
  changes the thing being measured. A validate result written to the parent
  ledger does not.
- Conversely, the parent ledger is not visible from a hermit checkout, so a
  hermit-side reader cannot see validate history at all.

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
- **`ObservedRange` is populated on 2 of 5520 cells** — and the precise version
  is worse than the round number. On `main` it is **0 of 5520**; the two
  populated cells exist only on the unlanded PR
  [#2396](https://github.com/rrnewton/hermit/pull/2396), and of those two only
  **one** actually carries ranges (`data-handling/sqlite-query-determinism
  verify sabre`: record 93–94, scheduler turn 68–69, virtual nanoseconds
  …317250–…354000, `samples: 2`). The other is a `pass` with all coordinates
  null.

## The divergence coordinates

`Observation` carries three coordinate ranges on `main`
(`first_divergent_record`, `first_divergent_scheduler_turn`,
`first_divergent_virtual_nanoseconds`) and a fourth,
`first_divergent_syscall`, only on unlanded #2396.

These are **different keyspaces and must never be read against one another's
axis**. One real measured divergence was record 98, syscall 37, scheduler turn
4. A reader who averages or compares across coordinates is producing nonsense.

On `main` the validate side does not populate any of them: the harness in
`ci/manifest-plan/src/` mentions the coordinates only as literal `null` in a
`canonical_verdict.rs` test string. The value is computed in
`hermit-cli/src/bin/hermit/verify.rs` and **dropped one hop before any cell sees
it**. #2396 adds the wiring (`runner.rs` +218, `canonical_verdict.rs` +126).

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
parent already holds the only durable series in the system — `ledger/hermit` and
`ledger/reverie`, sharded per repository, fed by 774 run records — with a
spool/union model already built for concurrent writers. A per-cell series is the
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

### The writer is 60% built and unlanded

The gap is a writer, and most of one already exists on an unlanded branch:

| Piece | State |
|---|---|
| `observe-results` command (the append entry point) | written, #2396, unlanded |
| validate-side wiring so a validate run can emit a divergence position at all | written, #2396, unlanded (`runner.rs` +218, `canonical_verdict.rs` +126) |
| row-independent fold, so a batch of N cells equals N single-cell runs | written, #2396, unlanded |
| `samples`, `provenance`, `depth`, `last_tested` | written, #2396, unlanded |
| the append-only series store itself | **not written** |
| a third producer path for `hermit-repeat` | **not written** |
| `flaky-cells.json` derived rather than hand-kept | **not written** |

The honest summary is that this redesign is one unlanded PR plus a store, not a
rewrite.

### Phases, in dependency order

**Phase 0 — land [#2396](https://github.com/rrnewton/hermit/pull/2396).**
Everything else builds on its types and its `observe-results` entry point.
Nothing new should be written until it lands, or the work will be rebased twice.
*Done when:* it is on main and the four coordinates can be written by something.

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
| [#2396](https://github.com/rrnewton/hermit/pull/2396) — 4th coordinate, `samples`, `provenance`, per-repo `depth`, `last_tested`, validate-side wiring, and the row-independent fold with `observe-results` | open, not landed |
| [#2444](https://github.com/rrnewton/hermit/pull/2444) — denominator provenance beside the published count | open, not landed |
| [#2446](https://github.com/rrnewton/hermit/pull/2446) — unrelated to storage; adds one manifest cell | open, not landed |

The fold work tracked as `implement-fold-option-b-skip-and-name-untrustworthy-rows`
is **not a separate PR**: row independence, per-row skips and the new
`observe-results` command are bundled inside #2396.
