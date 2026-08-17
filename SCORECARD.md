# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is in `ci/expected-e2e-plan.json` and is therefore required to pass by ordinary validation. **Red** is every other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, not omitted: a cell that cannot run is not green.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Green | Red | Total |
| --- | ---: | ---: | ---: |
| `ptrace` | 223 | 785 | 1008 |
| `dbt` | 9 | 999 | 1008 |
| `kvm` | 8 | 1000 | 1008 |
| `sabre` | 58 | 950 | 1008 |
| `liteinst` | 6 | 1002 | 1008 |
| `native` | 0 | 336 | 336 |
| **Total** | **304** | **5072** | **5376** |

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 221 / 336 | 9 / 336 | 8 / 336 | 58 / 336 | 6 / 336 | — | 302 | 1378 | 1680 |
| `replay` | 1 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 1 | 1679 | 1680 |
| `chaos` | 1 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 1 | 1679 | 1680 |
| `naked` | — | — | — | — | — | 0 / 336 | 0 | 336 | 336 |
| **Total** | | | | | | | **304** | **5072** | **5376** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 80 / 95 | 0 / 95 | 0 / 95 | 80 | 285 |
| `bin-c` | 0 / 2 | 0 / 2 | 0 / 2 | 0 | 6 |
| `c-programs` | 81 / 160 | 0 / 160 | 0 / 160 | 81 | 480 |
| `chaos-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |
| `data-handling` | 5 / 5 | 0 / 5 | 0 / 5 | 5 | 15 |
| `debugger-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |
| `determinism-stress` | 4 / 6 | 0 / 6 | 1 / 6 | 5 | 18 |
| `determinism-stress-c` | 7 / 11 | 0 / 11 | 0 / 11 | 7 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 23 / 25 | 1 / 25 | 0 / 25 | 24 | 75 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 306 selected regression cells: the 304 green compatibility cells above (including 1 chaos-mode race-exposure checks), and 2 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.
