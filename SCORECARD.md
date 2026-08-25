# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is SELECTED: it is listed in `ci/expected-e2e-plan.json` and is therefore required to pass by ordinary validation. **Red** is every other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, not omitted: a cell that cannot run is not green.

**Green does not mean measured, and it does not mean passing.** Selection, measurement, and result are three separate facts, and the Green column below reports only the first of them. A green cell that has never been executed once is the ordinary case, not an anomaly: green is a statement about what the plan REQUIRES, not about what has been OBSERVED. Whether a result was ever seen is a per-cell `measurement` field in `ci/compat-envelope/cells.json`, independent of colour and reading `never-measured`, `measured-and-passed`, or `diverged`; a cell can be green and `never-measured`, or red and `measured-and-passed`, and both combinations are present in the tracked file today. To count what has actually run, count that field -- do not count this table. Conflating the three has repeatedly produced project-status reports that quoted the Green total as a number of passing tests, which it has never been.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Green | Red | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 236 | 122 | 710 | 1068 |
| `dbt` | 0 | 61 | 1007 | 1068 |
| `kvm` | 1 | 22 | 1045 | 1068 |
| `sabre` | 57 | 87 | 924 | 1068 |
| `liteinst` | 3 | 50 | 1015 | 1068 |
| `native` | 0 | 33 | 323 | 356 |
| **Total** | **297** | **375** | **5024** | **5696** |

## Denominator, and why the percentage is not comparable across changes to it

Green is **297 of 5696**, which is **5.21%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **5024 of those 5696 cells are NOT APPLICABLE** — their backend is not enabled for their mode, so they were never asked to run and cannot pass or fail. Over the 672 cells that CAN run, green is **44.20%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 297 green cells measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly red RAISES the reported figure; adding honest red cells LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 232 / 356 | 0 / 356 | 1 / 356 | 57 / 356 | 3 / 356 | — | 293 | 1487 | 1780 |
| `replay` | 1 / 356 | 0 / 356 | 0 / 356 | 0 / 356 | 0 / 356 | — | 1 | 1779 | 1780 |
| `chaos` | 3 / 356 | 0 / 356 | 0 / 356 | 0 / 356 | 0 / 356 | — | 3 | 1777 | 1780 |
| `naked` | — | — | — | — | — | 0 / 356 | 0 | 356 | 356 |
| **Total** | | | | | | | **297** | **5399** | **5696** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 86 / 103 | 0 / 103 | 0 / 103 | 86 | 309 |
| `bin-c` | 0 / 2 | 0 / 2 | 0 / 2 | 0 | 6 |
| `c-programs` | 79 / 163 | 0 / 163 | 1 / 163 | 80 | 489 |
| `chaos-c` | 0 / 1 | 0 / 1 | 1 / 1 | 1 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 4 / 6 | 0 / 6 | 1 / 6 | 5 | 18 |
| `determinism-stress-c` | 7 / 11 | 0 / 11 | 0 / 11 | 7 | 33 |
| `language-runtimes` | 17 / 19 | 0 / 19 | 0 / 19 | 17 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 29 / 33 | 1 / 33 | 0 / 33 | 30 | 99 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 299 selected regression cells: the 297 green compatibility cells above (including 3 chaos-mode race-exposure checks), and 2 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.
