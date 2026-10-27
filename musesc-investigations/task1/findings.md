# Task 1 — syscall histogram end-to-end investigation (2026-10-27)

Slot HEAD `5f53234fbb01` (Hermit main `b42d62e` cohort). Handoff: `/home/newton/work/dev-hermit/temp/syscov-handoff-20261027.md` §2–3.
All commands bounded (`timeout 60`, `head`); no hermit run, no writes outside this directory.

## 1. Newest full validate — does `results.jsonl` carry `syscall_histogram`? YES

Newest runs (`ls -lt /home/newton/work/dev-hermit/ignored/validate/runs/*.json`):

| run file | exit | executed/passed |
|---|---|---|
| `validate-claude-coord-5f53234fbb01-1793092042298447644-3326064-1fe501a7.json` (+ `.service-result.json`: `10582` executed, `10580` passed) | 1 | newest |
| `validate-claude-coord-5f53234fbb01-1793091018626068180-149984-92e3fcc5.json` (`11202`/`11200`) | 1 | second |

`e2e_result_root` from the newest JSON: `/home/newton/work/dev-hermit/ignored/validate/artifacts/validate-claude-coord-5f53234fbb01-1793092042298447644-3326064-1fe501a7/e2e` — 16 `results.jsonl` files (`portable/manifest_*/`, `privileged/manifest_*/`).

Counts (python3 scan of all 16 files; backend/mode are top-level fields, histogram at `runtime.run1.syscall_histogram`):

| backend | rows total | verify rows | run1 hist | run2 hist |
|---|---|---|---|---|
| ptrace | 851 | 592 | 599 | 599 |
| kvm | 569 | 569 | 569 | 569 |
| liteinst | 310 | 310 | 310 | 310 |
| in-guest-trap | 309 | 309 | 309 | 309 |
| dbt | 259 | 258 | 0 | 0 |
| **total** | **2298** | 2038 | **1787** | **1787** |

Notes: ptrace run1/run2 = 599 > 592 verify because 7 `chaos` rows also carry `runtime` (verified by scan). DBT `runtime` is `null` (handoff follow-up 2 confirmed). Second run is near-identical (2299 rows, 1786 hist; kvm 567 — 2 fewer).

Format sample (1 line, truncated): `{"backend":"kvm","mode":"verify","runtime.run1.syscall_histogram":{"architecture":"x86_64","observation_scope":{"kind":"complete"},"counts":[[0,440,0,440,0],[1,89,0,89,0],[3,283,0,283,0],[5,241,1,240,0],...]}}` — compact `[nr,calls,detcore,backend,refused]`, exactly as handoff §2.1 specifies.

## 2. Published series rows — NONE with histograms yet (spool only)

Ledger store: `ci-hub/series/series.py:101` `LEDGER_REPOSITORY = "https://github.com/rrnewton/hermit_test_ledger.git"`; local checkout `/home/newton/work/dev-hermit/hermit_test_ledger/`, shards `series/<team>/<host>/<YYYY-MM>.jsonl` (`series.py:347`). Local checkout contains only `series/hermit/devbig014/2026-08.jsonl`, `2026-09.jsonl`, `devbig030/2026-09.jsonl` — **no 2026-10 file**. `grep -c syscall_histogram .../2026-09.jsonl` = **0**.

Unpublished spool (newest): `/home/newton/work/dev-hermit/ignored/ci-hub/series-spool/validate-claude-coord-5f53234fbb01-1793092042298447644-3326064-1fe501a7.jsonl` (5,370,034 B) + the `92e3fcc5` sibling. Backend is encoded as last cell segment (`series.cell = ".../verify/<backend>"`); `series.runtime.run1` only (run2 present as object but never a histogram — matches the shared-checker rule):

| backend (spool) | rows | `series.runtime.run1.syscall_histogram` | run2 hist |
|---|---|---|---|
| ptrace | 851 | 599 | 0 |
| kvm | 569 | 569 | 0 |
| liteinst | 310 | 310 | 0 |
| in-guest-trap | 309 | 309 | 0 |
| dbt | 259 | 0 | 0 |
| total | 2298 | 1787 | 0 |

Spool histogram drops `observation_scope` (vs results.jsonl): `{"architecture":"x86_64","counts":[[0,440,0,440,0],...]}`.

Backlog tool (read-only): `python3 ci-hub/health/ledger_spool_backlog.py --store series --json` → `{"spool_depth":2,"state":"pending","oldest_entry":"validate-...-92e3fcc5.jsonl","oldest_age_seconds":1329.2}` — both 5f53234 batches still unpublished at investigation time. `ledger-publish-backlog.log` tail shows earlier publications succeeding (`series_commit ed2ced9...`, shards `series/hermit/devbig014/2026-10.part-0035.jsonl` on the remote), so the pipeline works; these two batches simply had not been mirrored yet.

Quarantine: `ignored/ci-hub/series-quarantine/readmission-summary.json` (written 2026-10-27T09:24:05Z) — refused reasons are only `projection: ...` / `shared-checker: row contains a workspace-local owner path`; **zero syscall-histogram reasons**. A `--store series-quarantine --json` run did not finish within the bounded window (it scans published quarantine at the ledger commit); the summary file above is the same data its gate reads (`series.write_readmission_summary`), so the conclusion stands.

**Blocker for published-row counts:** wait for the next series publication (mirror) of the two spool batches, then recount in `hermit_test_ledger/series/hermit/devbig014/2026-10*.jsonl`.

## 3. Compatibility website — code ready, no local build, no nightly log here

Builder: `/home/newton/work/dev-hermit/ci-hub/compatibility-website/` (`build_data.py`, `render_page.py`, `build_and_serve.py`).
Syscalls page: `render_page.py:2772 SYSCALLS_PAGE = "syscalls/index.html"`, generated at `:7884` via `_syscalls_page` (`:5288`); coverage logic `_syscall_coverage` (`:5241`) admits a histogram only if its calls sum to run1 `syscalls` (`_admitted_syscall_histogram :5205`); pinned table comes from the projection's `syscall_table` (`build_data.py:1232 _syscall_table`, carried at `:2842`).

Local build: **not attempted (heavy) and not possible as-is here** — handoff claim verified in code: `build_and_serve.py:43 DEFAULT_OUTPUT_ROOT = REPOSITORY.parent / "<name>-compatibility-website"`, and `_source_checkouts()` (`:1448`) walks every repo ancestor stat-ing `CHECKOUT_MARKERS = {.git,.hg,.sl,.svn,.eden}`; `_output_location` refuses any output overlapping a source checkout. In this environment that ancestor walk reaches `/home/.git`, matching the handoff's "cannot run here" note.
Nightly log: `ignored/ci-hub/compatibility-website/nightly/run.log` — **directory does not exist** (`ls` → No such file). No built `syscalls/index.html` anywhere under the parent (bounded `find -maxdepth 4`). Live page remains the GitHub Pages URL in handoff §2.3, to be checked after the next 08:23 UTC docs rebuild.

## 4. Coverage-gap readiness — preliminary computed from spool (unpublished)

Pinned table source found: `detcore-model/src/syscall_histogram.rs:487 pinned_syscall_table()` → `(0..SYSCALL_TABLE_SIZE).filter_map(|slot| Sysno::new(slot + Sysno::first().id()))` where `Sysno` is `reverie_syscalls::Sysno` = `syscalls 0.6.18` crate enum in `~/.cargo/registry/src/index.crates.io-*/syscalls-0.6.18/src/arch/x86_64.rs`. That file has exactly **373** distinct `(name = nr)` entries, nr 0–461 (462 slots, gaps unnamed) — matches handoff. Extracted to `pinned_373.txt` (`nr<TAB>name`, 373 lines).

Histograms use **nr numbers**, mapped via that table. Preliminary results from the newest spool (1798→1787 histogram rows; full detail in `coverage_prelim.json`):

| backend | exercised | never |
|---|---|---|
| ptrace | 257 | 116 |
| kvm | 255 | 118 |
| liteinst | 233 | 140 |
| in-guest-trap | 233 | 140 |
| dbt | 0 | 373 |

Union (all backends) exercised **257**, never-anywhere **116**; intersection (histogram backends only) **232**; complete-scope union (ptrace+kvm) 257, partial union (liteinst+in-guest-trap) 233.

Caveat: this is from the *unpublished spool*, not published series rows. Once §2's publication lands, re-run the same script against `hermit_test_ledger/series/...2026-10*.jsonl` for the citable version. The website reducer counts only the latest *passing* cell per backend; this prelim counts every spool row — numbers will differ slightly.

## 5. LinuxEmu cross-reference

`/home/newton/work/dev-hermit/ai_docs/2026-10-25-linuxemu-scope.md` **exists in the parent only** (579 lines; absent in this slot's `ai_docs/`). §4 "Per-syscall table" (line 182+) format: `| nr | syscall | LinuxEmu | How, and why | gVisor | KVM corpus evidence |` with 374 rows (nr 0…; e.g. `| 0 | read | Partial | regular reads clip at 16 MiB... | FULL | 555 cells, 20629 calls; ... |`). §0 summary counts: Implemented 107, Partial 70, Refused-Detcore 32, Refused-other 3, Not-implemented-in-scope 71, Out-of-scope 91 (total 374 — one more than the 373 pinned names; the extra row is worth checking when crossing).

Ready to cross: for each of the 116 never-exercised names, join `pinned_373.txt` nr → LinuxEmu §4 status; and KVM-exercised (255) ∩ LinuxEmu `Refused`/`Out of scope` gives the "exercised but LinuxEmu refuses" list. Not computed here — needs the published rows first per handoff §3, and the 374-vs-373 discrepancy resolved.

## What is needed next

1. Next series publication (mirror) → recount published rows ( §2 table against ledger files).
2. Next docs rebuild (daily 08:23 UTC) → check the live Syscalls page backend table.
3. Then produce the dated `ai_docs/` coverage-gap report (handoff §3) from published rows.
