# Gate repair handoff

Owned task: `repair_three_current_main`, TaskGraph owner `dev-hermit`, canonical database `/home/newton/.tg/hermit2.db`.

This registered Hermit checkout is on `dev-hermit/ci-gate-repairs-20260916` at base `ba3bfc97671666ae946841804624e7a2c355a0e4`. Generation 1 binds the live coordinator/owner PID 2162531. Nested source remains AU 14589e875c08e458d2f8af5a6f78b12128d5a367 and rr 39e5c18e7e43236b7ca0fb1eb647fe9c93e3934e.

Checkpoint `2e792a13144f74fffccdc3aa5d6c40e70c46e924`, tree `3da3f9b66ac592586828adb7828b83f1a7d2b349`, commits exactly four paths for the three-target repair plus one exact-path regression in the existing cache-wrapper test. Tracked status is clean; no push. See `target/gate-repairs/DESIGN.md`, `working.diff`, and `static-checks.json`. Original proposal and allocation receipts remain under the parent `ignored/ci-hub/main-run1819-gate-repairs-20260916/`.

Static checks, all 27 scanner tests, the unchanged baseline-12 repository gate, portability, all 8 wrapper tests, per-site scanner negatives, exact old-path and wrong-path controls, five controlled plan cases, workspace formatting and all-target Clippy passed. See `target/gate-repairs/RESULTS.md` and `frozen-head.json` for exact commands, exits, timing, bounds, hashes and preserved setup failures.

The plan fixture uses real ancestry with a synthetic validation marker, existing plan-selection freshness relaxation, actual measured host facts, and output paths rebound into owned snapshotted state. It is not an actual outer validation execution. A separate ordinary real-validator selected-gate attempt at the committed source refused before nodes (exit 75, 0.292 seconds) because this required untracked handoff makes the checkout non-clean. No freshness/admission bypass was used; source bytes remained exact. Receipt: `target/gate-repairs/real-gate-run/`.

Next: coordinator independent exact-head reviews and publication. A clean publication checkout can attempt the ordinary selected gate without this slot's mandatory handoff. Do not alter the frozen source while reviews bind it. Do not touch the protected primary plugin, shared checkouts, other slots, or retained validation records.
