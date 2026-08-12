# HANDOFF — hermit-audit — 2026-08-06 teardown

Slot `worktrees/audit`. Nothing uncommitted, nothing unpushed. HANDOFF.md is at the
slot root deliberately, not inside `worktrees/audit/hermit`, so it does not dirty
the hermit checkout.

## Published this session — all 7 branches pushed and remote-verified

| task | branch | SHA | PR | state |
| --- | --- | --- | --- | --- |
| `detinode-newtype-make-invalid-unrepresentable` | `fix/detinode-newtype` | `fad50bc75543094862249e45bbf5b7e942c6de19` | #1683 | IMPLEMENTED, receipt env-blocked |
| `execute-ambiguous-zero-fix-order-a3-a4-first` (B1/B2) | `fix/ambiguous-zero-e9patch-banner` | `63ec41017562f8d90592c2ebc4a77e07afc467c3` | #1731 | IMPLEMENTED |
| `validate-harness-detection-refuse-bare-in-dev-hermit` | `fix/validate-harness-detection` | `3678a3b7b7f5a629acd642ce89eb6383bf032409` | #1735 | IMPLEMENTED |
| `install-qemu-and-restore-missing-demo-packages` | `fix/demo05-hermetic-kernel-asset` | `d1d30349d7fc44188a24409f7566004dccbe16b5` | #1739 | IMPLEMENTED |
| `fix-7-boolean-blind-fixtures-emit-observed-values` | `fix/parity-fixtures-emit-observed-values` | `a8c9de86271cacbc3d9d86181fc8a21b7ca44d5c` | none | **ABANDON — superseded by #1719** |
| `demonstrate-then-enable-the-six-never-run-cells` | `fix/enable-demonstrated-parity-c-cells` | `445775ad64cfe7695cabf46f134ef7eb7792d1fd` | #1743 | IMPLEMENTED |
| `strict-error-handling-enforced-by-the-type-system` (part 1) | `tooling/deny-unused-must-use` | `780f4cd40d9f8f24958a4d279e3378367655e1c7` | #1744 | IMPLEMENTED, parts 2–3 open |

`audit/boolean-blindness-survey` is at `4c70658e7` with **zero commits** — survey only,
nothing to push. Safe to delete.

Parent artifacts pushed to `rrnewton/dev-hermit:main`: `03fc0f9e` (fixture scatter map),
`c351a22` (runtime-path correction), `375692d` (A3/A4 regression test + stub + the
A1–A5 patch).

## READ THIS FIRST — do not repeat these

1. **Do not land `a8c9de86`.** PR #1719 already emits observed values for all six of
   those fixtures and does it better: I printed the HOST default pipe size into the
   compared byte stream; #1719 prints only the guest-chosen value. Extend #1719.
2. **My A1–A5 parent fixes are uncommitted ON PURPOSE.**
   `ci-hub/parity/prefix_depth.sh` (`AM`) and `compat-envelope/render-scorecard.rs`
   (`MM`) are staged in the shared parent index **by another agent**; my +90/−8 sits on
   top of their blobs. Committing would publish their work under my commit. The delta
   is preserved at `ai_docs/patches/ambiguous-zero-A1-A5-20260806.patch` (pushed at
   `375692d`), so nothing is lost. I tried isolating it onto upstream — **it does not
   isolate**: the rebased renderer dies with `CSV missing required column parity`
   because their change updates the renderer and the CSV schema together. It must land
   *with* their change.
3. **Shared parent index is hazardous.** Five of my published artifacts were staged as
   **deletions** there; I re-added exactly those five paths. **Ten further staged
   deletions remain and are not mine** — notably `ci-hub/validate/__init__.py`, which
   would break the validate package if it landed. Owners should check.

## Next steps, highest value first

- **Boolean blindness is the largest open defect.** Re-derived on `origin/main`:
  **52 of the 74 classifiable fixtures are tally-only**; 22 already emit a value; 8
  unclassified (refusal fixtures that fail natively before their emit path — must be
  re-measured *under hermit*). This does not reconcile with the "46 of 73" line and the
  denominators differ (82 vs 73) — treat as unreconciled. Method that works: cache each
  fixture's full stdout to a file and classify in Python; a shell pipeline misclassifies.
- **Strict-green authority (`land-one-strict-green-authority-schema-and-verifier`)**: my
  six-PR inventory is on `inventory-what-the-six-open-prs-already-get-right`. Key points:
  #1741 is the only PR implementing P4 and the only tier vocabulary — build on it;
  #1734/#1737 have the P2/P3 pattern (vacuity guard + reason strings) — adopt it;
  #1692 and #1719 land as-is. **#1710 is UNASSESSED** (20 files, I ran out of context)
  and shares **five files** with #1741 — those two will conflict.
- **`ci/expected-e2e-plan.json` is a four-way conflict** (#1710, #1734, #1737, #1743).
  It is generated: **regenerate after the last one lands, never hand-merge.**
- **Parts 2–3 of the error-handling task.** 132 discard-shaped sites enumerated, but my
  "102 dangerous" figure is **wrong** — sampling showed most are `Option` discards, not
  `Result`. Filter by return type, not call shape. Part 3 untouched.

## Gates / blockers

- **No backend passes the strace litmus.** SaBRe is ptrace-hosted — proven, not
  inferred: under strace, `ptrace(PTRACE_TRACEME) = -1 EPERM` and hermit errors
  `failed to spawn ptraced SaBRe guest`. Without strace the same run reports
  *Determinism verified*, so the dependency is invisible unless the tracer slot is
  occupied. `liteinst` and `e9patch` self-describe as ptrace-hosted in `--help`;
  `dbi`/`kvm` untested.
- **PR #1683 has no validate receipt.** Not a product failure: the portable lane dies at
  `build.manifest_guests` because `lua5.4` and `ruby` are absent, and the manifest's
  `requires` field is parsed then discarded (`main.rs:491`, `let _requires`).
- Environment: build/link use `ignored/lu-parity`; **runtime `LD_LIBRARY_PATH` must be
  the fbsource libunwind** — lu-parity ships `libunwind-ptrace` as a static `.a` only.
  Also: hermit blocks reading stdin when stdin is a socket (this agent harness) — pass
  `</dev/null` or it hangs and looks like a product bug.
