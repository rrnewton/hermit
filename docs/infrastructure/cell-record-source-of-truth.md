# The cell record: where it lives, who writes it, and what is missing

Status: design analysis, measured 2026-08-24 against hermit `8bc26dba1d`.
Nothing here is implemented as a consequence of this document; it exists so the
redesign is argued from the real architecture rather than from recollection.

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

## What a solution has to satisfy

Constraints that fall out of the measurements above, offered as requirements
rather than as a design:

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

## Related open work

| Item | State |
|---|---|
| [#2396](https://github.com/rrnewton/hermit/pull/2396) — 4th coordinate, `samples`, `provenance`, per-repo `depth`, `last_tested`, validate-side wiring, and the row-independent fold with `observe-results` | open, not landed |
| [#2444](https://github.com/rrnewton/hermit/pull/2444) — denominator provenance beside the published count | open, not landed |
| [#2446](https://github.com/rrnewton/hermit/pull/2446) — unrelated to storage; adds one manifest cell | open, not landed |

The fold work tracked as `implement-fold-option-b-skip-and-name-untrustworthy-rows`
is **not a separate PR**: row independence, per-row skips and the new
`observe-results` command are bundled inside #2396.
