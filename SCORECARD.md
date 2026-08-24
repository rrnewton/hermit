# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is in `ci/expected-e2e-plan.json` and is therefore required to pass by ordinary validation. **Red** is every other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, not omitted: a cell that cannot run is not green.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Green | Red | Total |
| --- | ---: | ---: | ---: |
| `ptrace` | 227 | 808 | 1035 |
| `dbt` | 0 | 1035 | 1035 |
| `kvm` | 0 | 1035 | 1035 |
| `sabre` | 53 | 982 | 1035 |
| `liteinst` | 0 | 1035 | 1035 |
| `native` | 0 | 345 | 345 |
| **Total** | **280** | **5240** | **5520** |

## Denominator, and why the percentage is not comparable across changes to it

Green is **280 of 5520**, which is **5.07%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly red RAISES the reported figure; adding honest red cells LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 224 / 345 | 0 / 345 | 0 / 345 | 53 / 345 | 0 / 345 | — | 277 | 1448 | 1725 |
| `replay` | 1 / 345 | 0 / 345 | 0 / 345 | 0 / 345 | 0 / 345 | — | 1 | 1724 | 1725 |
| `chaos` | 2 / 345 | 0 / 345 | 0 / 345 | 0 / 345 | 0 / 345 | — | 2 | 1723 | 1725 |
| `naked` | — | — | — | — | — | 0 / 345 | 0 | 345 | 345 |
| **Total** | | | | | | | **280** | **5240** | **5520** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 85 / 100 | 0 / 100 | 0 / 100 | 85 | 300 |
| `bin-c` | 0 / 2 | 0 / 2 | 0 / 2 | 0 | 6 |
| `c-programs` | 75 / 160 | 0 / 160 | 0 / 160 | 75 | 480 |
| `chaos-c` | 0 / 1 | 0 / 1 | 1 / 1 | 1 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |
| `determinism-stress` | 4 / 6 | 0 / 6 | 1 / 6 | 5 | 18 |
| `determinism-stress-c` | 7 / 11 | 0 / 11 | 0 / 11 | 7 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 26 / 28 | 1 / 28 | 0 / 28 | 27 | 84 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 282 selected regression cells: the 280 green compatibility cells above (including 2 chaos-mode race-exposure checks), and 2 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.
