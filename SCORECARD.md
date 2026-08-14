# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is in `ci/expected-e2e-plan.json`, is not a chaos-mode race-exposure check, and is therefore required to pass by ordinary validation. **Red** is every other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, not omitted: a cell that cannot run is not green.

These are the current Basic Sanity Milestone 1 contracts. Every `verify` cell runs the same backend twice. Bare `--verify` uses the Stripped comparator, so these counts measure legacy same-backend repeatability; they do not establish strict INFO-log determinism or cross-backend parity.

| Backend | Green | Red | Total |
| --- | ---: | ---: | ---: |
| `ptrace` | 264 | 744 | 1008 |
| `dbt` | 9 | 999 | 1008 |
| `kvm` | 0 | 1008 | 1008 |
| `sabre` | 9 | 999 | 1008 |
| `liteinst` | 2 | 1006 | 1008 |
| `native` | 0 | 336 | 336 |
| **Total** | **284** | **5092** | **5376** |

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 147 / 336 | 9 / 336 | 0 / 336 | 9 / 336 | 2 / 336 | — | 167 | 1513 | 1680 |
| `replay` | 117 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 117 | 1563 | 1680 |
| `chaos` | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 0 | 1680 | 1680 |
| `naked` | — | — | — | — | — | 0 / 336 | 0 | 336 | 336 |
| **Total** | | | | | | | **284** | **5092** | **5376** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 1 / 6 | 0 / 6 | 4 | 18 |
| `backend-parity-c` | 78 / 95 | 68 / 95 | 0 / 95 | 146 | 285 |
| `bin-c` | 0 / 2 | 0 / 2 | 0 / 2 | 0 | 6 |
| `c-programs` | 10 / 160 | 8 / 160 | 0 / 160 | 18 | 480 |
| `chaos-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |
| `data-handling` | 5 / 5 | 1 / 5 | 0 / 5 | 6 | 15 |
| `debugger-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |
| `determinism-stress` | 4 / 6 | 4 / 6 | 0 / 6 | 8 | 18 |
| `determinism-stress-c` | 6 / 11 | 6 / 11 | 0 / 11 | 12 | 33 |
| `language-runtimes` | 18 / 19 | 10 / 19 | 0 / 19 | 28 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 23 / 25 | 19 / 25 | 0 / 25 | 42 | 75 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 288 selected regression cells: the 284 green compatibility cells above, 2 chaos-mode race-exposure checks, and 2 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.
