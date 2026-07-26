# HANDOFF: impl-ci-split-validate — DONE / LANDED

Task impl-ci-split-validate is CLOSED. PR #712 is MERGED to main as ca10792c
("Stabilize hosted and serialized PMU CI validation (#712)").

## Result
- Hosted CI GREEN at PR head 311290bd (run 30177134061) and on main ca10792c
  (run 30177839442). Required check merge-gate GREEN.
- Self-hosted = merge-gate's BONUS signal (not required); queued on main behind
  the jammed pmu-serial runner; will validate when it frees. thread_sync
  relocation locally validated to pass in ~4s on PMU hardware.

## What landed (on top of prior #712 commits)
- validate.sh: thread_sync_determinism moved from hosted "Portable Hermit
  integration targets" to self-hosted "Hardware Hermit integration targets"
  (fixes the 600s no-PMU hosted gate timeout; still runs on the PMU lane).
- validate.sh + ci-hosted.yml: the 4 python3 --verify liteinst tests
  (rejects_non_fork_clone, handles_inherited_ignored_sigchld,
  verifies_forked_guest, verifies_raw_fork_guest) skipped from the blocking
  "Portable CLI cases" and run together as the observable nonblocking hosted
  "LiteInst python3 --verify diagnostics" step.
- docs/ci-validate-alignment.md: counts updated (portable 461->457, hardware
  318->319, hosted diagnostics 8->11, total 15->18).

No test dropped; no product code changed.

## Follow-up (not this task)
- Confirm main self-hosted PMU lane green once hermit-ci-newton drains.
- Product fix for the pre-existing liteinst --verify python3-startup
  nondeterminism (bounded retry in backends.rs) belongs to #688 owner.

## Slot state
Feature branch merged + deleted upstream. Local branch retained for audit.
Slot is free for the coordinator to park or reclaim.
