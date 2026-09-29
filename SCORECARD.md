# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **5984** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **895** are selected by full, **148** are not selected by full, and **4941** are **Not applicable**.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 367 | 13 | 742 | 1122 |
| `dbt` | 26 | 59 | 1037 | 1122 |
| `kvm` | 244 | 8 | 870 | 1122 |
| `sabre` | 112 | 32 | 978 | 1122 |
| `liteinst` | 146 | 3 | 973 | 1122 |
| `native` | 0 | 33 | 341 | 374 |
| **Total** | **895** | **148** | **4941** | **5984** |

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **895 of 5984**, which is **14.96%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **4941 of those 5984 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 1043 cells that CAN run, selected by full is **85.81%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 895 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 359 / 374 | 26 / 374 | 244 / 374 | 112 / 374 | 146 / 374 | — | 887 | 114 | 869 | 1870 |
| `replay` | 2 / 374 | 0 / 374 | 0 / 374 | 0 / 374 | 0 / 374 | — | 2 | 0 | 1868 | 1870 |
| `chaos` | 6 / 374 | 0 / 374 | 0 / 374 | 0 / 374 | 0 / 374 | — | 6 | 1 | 1863 | 1870 |
| `naked` | — | — | — | — | — | 0 / 374 | 0 | 33 | 341 | 374 |
| **Total** | | | | | | | **895** | **148** | **4941** | **5984** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `bin-c` | 1 / 2 | 0 / 2 | 0 / 2 | 1 | 6 |
| `c-programs` | 273 / 278 | 1 / 278 | 3 / 278 | 277 | 834 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 1 / 6 | 6 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 38 / 39 | 1 / 39 | 0 / 39 | 39 | 117 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 900 cells: the 895 comparable compatibility cells selected by full above (including 6 chaos-mode race-exposure checks), and 5 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `c-programs` | `c-programs/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `dbt` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `liteinst` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Run history

Detailed observations and the generated history website live in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger). This catalogue records selection and applicability, not whether a cell has been measured. Run `./ci/compat-envelope/scorecard.rs show` with the ledger checkout available to read history.
