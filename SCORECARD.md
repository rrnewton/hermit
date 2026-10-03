# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **14864** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **1234** are selected by full, **715** are not selected by full, and **12915** are **Not applicable**.

Every selected `verify` cell that does not declare the stripped comparator, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. **189** of the **1224** selected `verify` cells declare `comparator: stripped` (`compat` on `ptrace`: 189). They run Hermit's default `--verify` and pass only on a verified, matched report of a non-empty stripped comparison; they are below L2, never `bitwise_parity`, and are counted in these tables as selected, not as canonical. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 564 | 373 | 1850 | 2787 |
| `dbt` | 26 | 59 | 2702 | 2787 |
| `kvm` | 258 | 8 | 2521 | 2787 |
| `sabre` | 240 | 239 | 2308 | 2787 |
| `liteinst` | 146 | 3 | 2638 | 2787 |
| `native` | 0 | 33 | 896 | 929 |
| **Total** | **1234** | **715** | **12915** | **14864** |

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **1234 of 14864**, which is **8.30%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **12915 of those 14864 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 1949 cells that CAN run, selected by full is **63.31%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 1234 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 554 / 929 | 26 / 929 | 258 / 929 | 240 / 929 | 146 / 929 | — | 1224 | 542 | 2879 | 4645 |
| `replay` | 4 / 929 | 0 / 929 | 0 / 929 | 0 / 929 | 0 / 929 | — | 4 | 139 | 4502 | 4645 |
| `chaos` | 6 / 929 | 0 / 929 | 0 / 929 | 0 / 929 | 0 / 929 | — | 6 | 1 | 4638 | 4645 |
| `naked` | — | — | — | — | — | 0 / 929 | 0 | 33 | 896 | 929 |
| **Total** | | | | | | | **1234** | **715** | **12915** | **14864** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `bin-c` | 1 / 2 | 0 / 2 | 0 / 2 | 1 | 6 |
| `c-programs` | 278 / 282 | 3 / 282 | 3 / 282 | 284 | 846 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `compat` | 189 / 551 | 0 / 551 | 0 / 551 | 189 | 1653 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 1 / 6 | 6 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 39 / 39 | 1 / 39 | 0 / 39 | 40 | 117 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 1239 cells: the 1234 comparable compatibility cells selected by full above (including 6 chaos-mode race-exposure checks), and 5 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

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
