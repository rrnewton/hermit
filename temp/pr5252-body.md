[hermit2, musedrain, tbh-1005, devbig014, role=impl]
Owner: musedrain (Muse, devbig014) since 2026-11-07T15:55:00Z

# detcore: a timer stop short in progress has not passed its checkpoint

## Plain Language Summary and Project Impact

Fixes Hermit issue #5252 (internal dh-hiqgm): a strict --verify run of a
chaos guest could refuse itself deterministically depending only on the
guest's length. Hermit now waits for the recorded endpoint it has not
actually reached, instead of refusing a replay that is proceeding
faithfully.

## Background

`branch_free_race` under `--chaos-instruction-offset` verifies cleanly at
100,000 iterations but not at 25,000 or 50,000 (the issue keeps the test at
100,000; this change fixes the product defect underneath).

## Root cause

Deterministic DESYNC reproduced on main `b2bf4babf30` before this change:

```
DESYNC callback checkpoint 4 for 3 (no recorded position): expected
Origin { callback: Syscall(getrandom), site: Posthook, progress: 19266,
last_op: Some(Syscall(getrandom, Posthook)), time:
LogicalTime(1767225600000770195), rcbs: 19092 }, observed Origin {
callback: PmuTimer, site: Prehook, progress: 19264, last_op:
Some(Branch), time: LogicalTime(1767225600000760205), rcbs: 19093 } (hard)
```

`decide_row` (callback checkpoint accounting) treated `rcbs >
expected.rcbs` as proof the endpoint had been passed, even when progress
(event order) was still short. A PMU timer stop's skid can retire a branch
or two before the endpoint's event, and equal-RCB callback boundaries
already show RCBs do not order checkpoints. The timer arm in `lib.rs`
refused "past the endpoint" on RCBs alone for the same reason. This change
counts RCBs as passed evidence only once progress reaches the endpoint,
and arms for the next branch when an RCB target is already reached while
the endpoint's event is still ahead.

## Determinism

Progress is the passive sum of positive observed event counts — actual
event order, never the expected tape — so gating RCB evidence on it is
deterministic and environment-independent. A genuine overshoot is still
refused: past the endpoint in progress is Overshot, and at the endpoint's
progress more RCBs or another time/last-operation is Overshot/Incompatible.
Continuous virtual time is unchanged: no clock value, rounding, or
timeslice is altered; only the refusal predicate for an unreached
endpoint changes.

## Linux Semantics

No syscall result, errno, or guest-visible Linux behavior changes. This
is Detcore's internal replay accounting for recorded priority-history
rows; the guest executes the same syscalls in the same order.

## Test plan

Exact head: `8ed5e12d62315dfb438a305d1dfe76495e5fe07b` on `drain/5252`
(base main `b4ce9e923203967ece6120c1cc171c2307154aef`). All runs at that
head (ptrace backend, default log, no relaxations, `--offline --locked`):

- `a_chaos_instruction_offset_run_verifies_and_replays_at_its_offsets`
  with the guest argument at 25,000, 50,000, and 100,000 iterations:
  1 passed each (before this change, 25,000 and 50,000 failed 3/3 with
  the DESYNC above, reproduced exactly).
- New unit test `a_timer_stop_short_in_progress_waits_despite_more_rcbs`
  with the issue's exact Origin shape; `cargo test -p hermit-detcore
  --lib`: 3969 passed at the pre-rebase head, callback_history set
  re-verified after the rebase.
- `preemption_replay_preserves_equal_rcb_software_callback_boundaries`
  (the #4668 area test): 1 passed.
- `cargo fmt --all -- --check` clean; clippy `-D warnings` clean.

Dual review is routed (Muse + Astra) on the specific question whether
the relaxation can let a real desync through; the merge queue's
full-profile validate at the exact head is the landing gate.

## Human Review Required

Core Detcore replay/scheduling accounting change (trigger-class per the
coordinator's ruling); dual Muse + Astra review.
