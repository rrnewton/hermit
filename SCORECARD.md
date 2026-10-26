# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **11400** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **2274** are selected by full, **517** are not selected by full, and **8609** are **Not applicable**.

Every selected `verify` cell that does not declare the stripped comparator, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. None of the **2019** selected `verify` cells declares the stripped comparator. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 842 | 12 | 946 | 1800 |
| `dbt` | 255 | 1 | 1544 | 1800 |
| `kvm` | 564 | 0 | 1236 | 1800 |
| `sabre` | 0 | 471 | 1329 | 1800 |
| `liteinst` | 307 | 0 | 1493 | 1800 |
| `in-guest-trap` | 306 | 0 | 1494 | 1800 |
| `native` | 0 | 33 | 567 | 600 |
| **Total** | **2274** | **517** | **8609** | **11400** |

**Archived:** `sabre` (backend, archived 2026-10-25, nightly only) and `e9patch` (preprocessing with ptrace, not a backend, archived 2026-10-25, on demand only). They get no feature work and their tests left per-commit validation. An archived backend's cells that would be selected by full belong to the `archived-nightly` run type instead, so the table counts them as not selected by full, and the nightly pressure sweep runs them as red cells. `ci/manifest-plan/src/archived.rs` records what still runs per commit.

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **2274 of 11400**, which is **19.95%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `in-guest-trap`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **8609 of those 11400 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 2791 cells that CAN run, selected by full is **81.48%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 2274 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `in-guest-trap` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 587 / 600 | 255 / 600 | 564 / 600 | 0 / 600 | 307 / 600 | 306 / 600 | — | 2019 | 481 | 1100 | 3600 |
| `replay` | 248 / 600 | 0 / 600 | 0 / 600 | 0 / 600 | 0 / 600 | 0 / 600 | — | 248 | 3 | 3349 | 3600 |
| `chaos` | 7 / 600 | 0 / 600 | 0 / 600 | 0 / 600 | 0 / 600 | 0 / 600 | — | 7 | 0 | 3593 | 3600 |
| `naked` | — | — | — | — | — | — | 0 / 600 | 0 | 33 | 567 | 600 |
| **Total** | | | | | | | | **2274** | **517** | **8609** | **11400** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 4 / 6 | 0 / 6 | 0 / 6 | 4 | 18 |
| `bin-c` | 2 / 2 | 0 / 2 | 0 / 2 | 2 | 6 |
| `c-programs` | 282 / 284 | 35 / 284 | 3 / 284 | 320 | 852 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `compat` | 214 / 219 | 212 / 219 | 0 / 219 | 426 | 657 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 2 / 6 | 7 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 19 / 19 | 0 / 19 | 0 / 19 | 19 | 57 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 40 / 40 | 1 / 40 | 0 / 40 | 41 | 120 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 2279 cells: the 2274 comparable compatibility cells selected by full above (including 7 chaos-mode race-exposure checks), and 5 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `c-programs` | `c-programs/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `dbt` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/random-device-spellings` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Run history

Detailed observations and the generated history website live in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger). This catalogue records selection and applicability, not whether a cell has been measured. Run `./ci/compat-envelope/scorecard.rs show` with the ledger checkout available to read history.
