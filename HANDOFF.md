# HANDOFF — egress-probe2 (opus-5), 2026-08-06 teardown

Nothing uncommitted, nothing unpushed. Every branch below is verified AT THE REMOTE
by an `ls-remote` re-read, not inferred from a push exit code.

## Hermit branches (all pushed, all draft PRs open, NONE merged — det2 lands serially)

| task | branch | SHA | PR |
| --- | --- | --- | --- |
| fixture-clock-family-full-coverage | `fixture/clock-family-continuous-virtual-time` | `f706d3dc3d6f1086593a8f353d405b4a3cb86732` | #1713 |
| audit_detlog_record_framing / detlog-record-framing-standardize-all-backends | `fix/dbi-detlog-record-framing` | `57af652a7056f43e53dc9ecd79c295a1fd7f421e` | #1718 |
| dbi_log_file_is | `fix/dbi-honours-log-file` | `51c3769cf3fa7d829c115b4cfc1270ce70523ba4` | #1696 |
| wire-inert-phase2-guards-into-consumers | `fix/anchor-select-qualifying-baseline` | `58082897d51fb42ac885c97276caeca63b9fd4a2` | none yet |
| expand-strict-corpus-new-e2e | `feat/e2e-jit-and-thread-corpus` | `0cab21576285d05248b1dd561151591acaf223d6` | none yet |

All five slots: `dirty=0 unpushed=0` at teardown.

## Parent (dev-hermit) commits, all on remote main

- `a2e534a1ac968547aad8555639916c719239d1bb` — mutation_suite: register hermit contract fixtures + UNAVAILABLE state
- `6db2520fcbf7778e5ee6c34d4e1b83a3934fdd03` — per-backend fixture execution matrix (artifact + experiment)
- `01764587b5bf5e3d39a8705210e9205937f4f7c9` — herdr tab reaping policy (design)

## NEXT STEP / GATES

1. **No validate receipt for any hermit PR.** `ci-hub validate-lock` is box-exclusive; my clock-family
   run (`validate-egress-probe2-f706d3dc3d6f-1786027725.service`) was queue position 1 at teardown.
   The dbilog head `51c3769cf` came back **NEEDS-RERUN** — a *contention* no-result, not a product red.
   Successor: `ci-hub validate-run --attach <handle>`; do NOT relaunch.
2. **herdr-tab-reaping-policy is DESIGN ONLY** — artifact now at
   `ai_docs/herdr-tab-reaping-policy-20260806.md@0176458`. Implementation needs the agent-utils
   **serialize + re-pin** path; take the serialize slot FIRST.
3. **e9patch converge: notes are stale.** `install_hybrid_runtime` no longer returns `Unsupported` at
   `reverie-e9patch/src/runtime.rs:259` — it forwards, and the `Unsupported` now lives in
   `reverie-preload/src/lifecycle.rs:102-108` (`HybridPtrace::install`). Same defect, new address.
4. **Reverie primary is on a feature branch**, not main (`stack-ptracer/liteinst-stats-off-ptrace-crate`)
   — invariant violation, not mine, not touched.
5. **Parent local main has DIVERGED**: 24 ahead / 19 behind at teardown, and the 24 include other
   agents' commits. I did not rebase or merge them. My commits reached the remote via `commit-tree`
   on `origin/main` without moving any local ref.

## Hazard I caused, for the orphan triage

Twice earlier I ran `git update-ref refs/heads/main <sha>` after a plumbing push. The first moved
local main off `adf7819`, orphaning it (content preserved in the pushed `a2e534a`; already rescued
into `origin/rescue/ignored-artifact-detector`). **I stopped doing this** — the two commits above
were pushed without moving any local ref. `8ce511d7` / `9e0c5044` are not mine, so there is a
second source.

Also: `git restore --staged` on a shared index can *create* a phantom staged **deletion** later, if
HEAD advances past the file. Five of my landed paths were staged as `D` and would have been deleted
inside another agent's commit. Cleared.
