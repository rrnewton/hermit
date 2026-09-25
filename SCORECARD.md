# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **5808** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **859** are selected by full, **149** are not selected by full, and **4800** are **Not applicable**.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 357 | 12 | 720 | 1089 |
| `dbt` | 0 | 61 | 1028 | 1089 |
| `kvm` | 244 | 8 | 837 | 1089 |
| `sabre` | 112 | 32 | 945 | 1089 |
| `liteinst` | 146 | 3 | 940 | 1089 |
| `native` | 0 | 33 | 330 | 363 |
| **Total** | **859** | **149** | **4800** | **5808** |

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **859 of 5808**, which is **14.79%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **4800 of those 5808 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 1008 cells that CAN run, selected by full is **85.22%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 859 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 349 / 363 | 0 / 363 | 244 / 363 | 112 / 363 | 146 / 363 | — | 851 | 115 | 849 | 1815 |
| `replay` | 2 / 363 | 0 / 363 | 0 / 363 | 0 / 363 | 0 / 363 | — | 2 | 0 | 1813 | 1815 |
| `chaos` | 6 / 363 | 0 / 363 | 0 / 363 | 0 / 363 | 0 / 363 | — | 6 | 1 | 1808 | 1815 |
| `naked` | — | — | — | — | — | 0 / 363 | 0 | 33 | 330 | 363 |
| **Total** | | | | | | | **859** | **149** | **4800** | **5808** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 103 / 104 | 0 / 104 | 0 / 104 | 103 | 312 |
| `bin-c` | 1 / 2 | 0 / 2 | 0 / 2 | 1 | 6 |
| `c-programs` | 165 / 168 | 1 / 168 | 3 / 168 | 169 | 504 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 1 / 6 | 6 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 33 / 34 | 1 / 34 | 0 / 34 | 34 | 102 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 862 cells: the 859 comparable compatibility cells selected by full above (including 6 chaos-mode race-exposure checks), and 3 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `backend-parity-c` | `backend-parity-c/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `liteinst` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Run history

Detailed observations and the generated history website live in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger). This catalogue records selection and applicability, not whether a cell has been measured. Run `./ci/compat-envelope/scorecard.rs show` with the ledger checkout available to read history.
