# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is SELECTED: it is listed in `ci/expected-e2e-plan.json` and is therefore required to pass by ordinary validation. **Red** is every other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, not omitted: a cell that cannot run is not green.

**Green does not mean measured, and it does not mean passing.** Selection, measurement, and result are three separate facts, and the Green column below reports only the first of them. A green cell that has never been executed once is the ordinary case, not an anomaly: green is a statement about what the plan REQUIRES, not about what has been OBSERVED. Whether a result was ever seen is a per-cell `measurement` field in `ci/compat-envelope/cells.json`, independent of colour and reading `never-measured`, `measured-and-passed`, or `diverged`; a cell can be green and `never-measured`, or red and `measured-and-passed`, and both combinations are present in the tracked file today. To count what has actually run, count that field -- do not count this table. Conflating the three has repeatedly produced project-status reports that quoted the Green total as a number of passing tests, which it has never been.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Green | Red | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 235 | 119 | 705 | 1059 |
| `dbt` | 0 | 60 | 999 | 1059 |
| `kvm` | 0 | 23 | 1036 | 1059 |
| `sabre` | 56 | 86 | 917 | 1059 |
| `liteinst` | 5 | 48 | 1006 | 1059 |
| `native` | 0 | 33 | 320 | 353 |
| **Total** | **296** | **369** | **4983** | **5648** |

## Denominator, and why the percentage is not comparable across changes to it

Green is **296 of 5648**, which is **5.24%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **4983 of those 5648 cells are NOT APPLICABLE** — their backend is not enabled for their mode, so they were never asked to run and cannot pass or fail. Over the 665 cells that CAN run, green is **44.51%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 296 green cells measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly red RAISES the reported figure; adding honest red cells LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 232 / 353 | 0 / 353 | 0 / 353 | 56 / 353 | 5 / 353 | — | 293 | 1472 | 1765 |
| `replay` | 1 / 353 | 0 / 353 | 0 / 353 | 0 / 353 | 0 / 353 | — | 1 | 1764 | 1765 |
| `chaos` | 2 / 353 | 0 / 353 | 0 / 353 | 0 / 353 | 0 / 353 | — | 2 | 1763 | 1765 |
| `naked` | — | — | — | — | — | 0 / 353 | 0 | 353 | 353 |
| **Total** | | | | | | | **296** | **5352** | **5648** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 86 / 102 | 0 / 102 | 0 / 102 | 86 | 306 |
| `bin-c` | 0 / 2 | 0 / 2 | 0 / 2 | 0 | 6 |
| `c-programs` | 78 / 162 | 0 / 162 | 0 / 162 | 78 | 486 |
| `chaos-c` | 0 / 1 | 0 / 1 | 1 / 1 | 1 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 4 / 6 | 0 / 6 | 1 / 6 | 5 | 18 |
| `determinism-stress-c` | 7 / 11 | 0 / 11 | 0 / 11 | 7 | 33 |
| `language-runtimes` | 17 / 19 | 0 / 19 | 0 / 19 | 17 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 30 / 32 | 1 / 32 | 0 / 32 | 31 | 96 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 298 selected regression cells: the 296 green compatibility cells above (including 2 chaos-mode race-exposure checks), and 2 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.
