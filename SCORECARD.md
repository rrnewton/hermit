# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the latest validate result merged for the cell passed. **Red** means the latest merged result failed, or no passing result has been merged. `ci/expected-e2e-plan.json` separately names the cells that ordinary validation must run: a selected red cell remains blocking and is not removed from that plan. Manifest-disabled combinations are red, not omitted: a cell that cannot run is not green.

These are the current Basic Sanity contracts. Every `verify` cell, every `replay` cell, and every seed in a selected `chaos` cell requires a typed canonical verdict. Verification passes on a supported canonical evidence path require non-vacuous INFO-log bitwise parity. Direct DBT verification currently fails closed with `no_result` pending a protected Reverie internal descriptor; KVM remains output-only and therefore unqualified for that claim.

| Backend | Green | Red | Total |
| --- | ---: | ---: | ---: |
| `ptrace` | 150 | 858 | 1008 |
| `dbt` | 9 | 999 | 1008 |
| `kvm` | 0 | 1008 | 1008 |
| `sabre` | 9 | 999 | 1008 |
| `liteinst` | 2 | 1006 | 1008 |
| `native` | 0 | 336 | 336 |
| **Total** | **170** | **5206** | **5376** |

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 147 / 336 | 9 / 336 | 0 / 336 | 9 / 336 | 2 / 336 | — | 167 | 1513 | 1680 |
| `replay` | 1 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 1 | 1679 | 1680 |
| `chaos` | 2 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 2 | 1678 | 1680 |
| `naked` | — | — | — | — | — | 0 / 336 | 0 | 336 | 336 |
| **Total** | | | | | | | **170** | **5206** | **5376** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 78 / 95 | 0 / 95 | 0 / 95 | 78 | 285 |
| `bin-c` | 0 / 2 | 0 / 2 | 0 / 2 | 0 | 6 |
| `c-programs` | 10 / 160 | 0 / 160 | 0 / 160 | 10 | 480 |
| `chaos-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |
| `data-handling` | 5 / 5 | 0 / 5 | 0 / 5 | 5 | 15 |
| `debugger-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |
| `determinism-stress` | 4 / 6 | 0 / 6 | 2 / 6 | 6 | 18 |
| `determinism-stress-c` | 6 / 11 | 0 / 11 | 0 / 11 | 6 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 23 / 25 | 1 / 25 | 0 / 25 | 24 | 75 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 172 selected regression cells: 170 currently green comparable cells, 0 currently red comparable cells, and 2 explicit custom commands outside the comparable denominator (including 2 selected chaos-mode race-exposure checks). A passing validate must produce a fresh PASS result for every selected cell. Recording a selected cell red reports the failure; it does not remove or excuse the cell.
