# Series quarantine triage — the 34 `missing_with_evidence` refusals (2026-10-27)

Written by musesc (Muse). Source: `readmission-summary.json` written
2026-10-27T09:32:19Z at ledger commit
`8e26a81f28b9e58b0e780b4cbe992c2c4315d790`, reproduced locally with
`series.readmission_plan` (69 s, rows=0, left_refused identical: 15/10/9).
Nothing was readmitted by hand. Raw captures:
`musesc-investigations/task2/refused_records.json`,
`refused_values.json` in the musesc slot.

## Summary

| group | n | class | disposition |
|---|---:|---|---|
| A. Declared-stdout reason on a replay cell | 15 | **projection bug** | fix in the patch (below); future runs and readmission then project them |
| B. Framework `errored`/`crash-error` over all-matched attempts | 10 new (+15 already acknowledged of the same shape) | **producer bug, already fixed on Hermit main** | acknowledge `refusal-stands`; do not readmit, do not relax the projection |
| C. Matched report "internally inconsistent" | 9 new (+2 acknowledged) | **projection bug** (8 of 9); 1 unresolved | fix in the patch; the 1 chaos/sabre record needs its raw input before any change |

## Group A — 15, projection bug (live)

Cells (all `replay/ptrace`, producer `pressure-test`):
`c-programs/syscall-file-metadata` ×8, `c-programs/threads-fd-churn` ×5,
`c-programs/syscall-file-io` ×2, in pressure surveys
`pressure-{0948c8eb,cbbe4c75,ebf077bc}-survey-ptrace` (2026-10-26/27 — this
class is still being produced).

The runner writes the exact-stdout mismatch reason
(`verify first run stdout (0 bytes, sha256 e3b0…) differs from the declared
expected_stdout …`, attempt FAIL, result `crash-error`). The projection
refuses it before judging the evidence:
`series.py:7293` required `mode == "verify"`.

That contradicts the producer's own contract, in three places:

- Hermit `ci/manifest-plan/src/runner.rs:911 expectation_recipe` matches
  `verify | replay` and reads the **verify** recipe, so a replay cell
  inherits `expected_stdout`/`expected_stdout_contains`;
  `cell_expected_stdout` (runner.rs:957) is documented "a selected verify
  or replay cell", and `declared_stdout_error` is applied to the replay
  record/replay comparison.
- The sibling declaration already crossed this bridge: series
  `_expected_guest_exit` (series.py:6308) accepts verify **or replay**,
  citing `runner::validate_result_expected_guest_exit`.
- Only `_expected_output_failure_reason` kept the verify-only rule.

**Smallest fix:** in `_expected_output_failure_reason`, accept
`mode in ("verify", "replay")` and name both in the refusal text. This is
in `series-projection-fix.patch`, with regression tests
(`ReplayDeclaredStdout`): replay and verify reasons are recognised on the
same evidence; chaos still refuses. Existing tests that assert the old
text are updated in the same patch.

## Group C — 9, projection bug (same root, second site)

Cells (all 2026-10-25 resurveys, producer `pressure-test`):
`c-programs/nanosleep-threads-simple/replay/ptrace` ×2,
`shared-futex-c/qemu-hello/replay/ptrace` ×2,
`system-utils/sh-exit-status/replay/ptrace` ×2,
`util-c/pmu-skid/replay/ptrace` ×2,
`determinism-stress/order-violation/chaos/sabre` ×1 (attempt 25).

The eight replay records are PASS rows carrying an inherited
`expected_guest_exit` — code 7 (`qemu-hello`), code 23 (`sh-exit-status`),
code 3 (`pmu-skid`), signal 11 (`nanosleep-threads-simple`) — with matched
reports whose guest disposition equals the declaration (verified against
the retained cell results in
`ignored/compat-envelope/pressure-7903351…-resurvey-ptrace/cells/`).
The projection refuses them at `series.py:7941`:

```python
disposition_consistent = mode == "verify" and guest == expected_guest_exit
```

For replay this is always false, so every declared-exit replay pass is
"internally inconsistent". Same defect as Group A, second site: the
declaration is admitted for replay at row level (`_expected_guest_exit`)
but not credited at attempt level.

**Smallest fix:** `mode in ("verify", "replay")`, in the same patch, with
a regression test (`ReplayDeclaredGuestExitDisposition`) built on the
`qemu-hello` shape (code 7, `--verify-allow=both`, `record start`). The
test fails on the unpatched projection with the exact refusal above and
passes patched.

The ninth record (`order-violation/chaos/sabre`, attempt 25) could not be
confirmed from retained data: the same cell's retained chaos file shows
attempt 25 as a diverged report, so the refused input (from the earlier
051230 run, not retained) may differ. **Unresolved — do not touch it on
this evidence.** If its raw input is recovered and shows the same
declared-exit shape, the same fix covers it; if it shows a genuinely
contradictory matched report, it stays refused.

## Group B — 10 new, producer bug already fixed; acknowledge, don't readmit

New cells (all `chaos`, 32 matched PASS attempts, result `crash-error`,
failure_class `product_failure`): `c-programs/ipc-determinism/chaos/dbt`
×2, `c-programs/thread-sync-determinism/chaos/{dbt,sabre}` ×4,
`determinism-stress-c/thread-contention/chaos/{dbt,sabre}` ×4, from the
2026-10-25 resurveys. The 15 already-acknowledged records of this reason
are the same shape on `c-programs/fp-reduction-nondeterminism/chaos/ptrace`
(2026-10-10…12 validates).

The input is self-contradicting as written: a crash-error whose every
attempt matched with bitwise parity supports no crash. The projection is
right to refuse it — projecting it would either credit a pass the
framework did not write or keep a red no attempt supports.

The producer side is already fixed on Hermit main:
`25dc3c69958` (2026-10-26) "Name a chaos cell's floor failure over matched
attempts chaos-floor-failure" adds `chaos_floor_result`
(`ci/manifest-plan/src/runner.rs:8443`), which renames exactly this
crash-error to `chaos-floor-failure` when the population floor failed and
every attempt matched; the projection already projects that result to
`errored` over matched attempts (series.py:8573, 8730). All refused inputs
of this class predate that commit (newest 2026-10-25), and no record of
this class appears after it in the summary window.

**Disposition:** acknowledge these 10 (and, when reviewed, the chaos/sabre
straggler in Group C only if it proves to be this shape) with
`producer_fix` citing Hermit `25dc3c69958`, following the 36 existing
acknowledgements in `ci-hub/series/quarantine-acknowledgements.jsonl`.
Draft lines are in `quarantine-acknowledgement-drafts.jsonl` beside this
report — they need each record's `raw_sha256`/`quarantine_event_id` filled
from the published quarantine record before use, and a reviewer signature;
they are drafts, not applied acknowledgements. No projection change, no
readmission.

## What the patch does not do

- It does not readmit anything. After it lands on dev-hermit, the next
  readmission replay projects the Group A/C inputs itself; that is the
  designed path.
- It does not relax any evidence check: replay attempts are judged on the
  same compared outputs, dispositions, and invocation rules as verify;
  only the mode gate changes.
- Group B stays refused by design.

## Verification

- `test_replay_declaration_and_chaos_floor.py` on the patched tree:
  10 passed (3 new tests fail on the unpatched projection, pass patched).
- Targeted `CellResultProjection` declared-stdout tests in
  `test_series_invariants.py`: patched vs unpatched differ by exactly the
  two subtests whose expected refusal text changed (both now pass); the
  remaining subtest failures are identical on both versions in this
  overlay environment (they need the shared checker built from a Hermit
  checkout, absent here) — no regression from the patch.
- `series-projection-fix.patch` applies cleanly (`git apply --check`) to a
  pristine copy of `ci-hub/series` and produces a byte-identical
  `series.py` to the tested staging tree.
- Readmission reproduced the exact 34 (15/10/9) before the fix; after the
  patch lands, the expected signature is 10 remaining (Group B) plus the
  1 unresolved chaos record, all acknowledged-or-standing.
