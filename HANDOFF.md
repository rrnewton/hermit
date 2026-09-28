# Handoff — slot demo08review

Written 2026-09-01 at ~97% context. Nothing is in flight; no worktrees left behind; slot tree clean.

## The one answer someone is waiting on

**Detcore DOES intercept and determinize `getrandom`, on every backend.** The owner's recollection
is correct and the relay ("hermit not reproducing a value its guest reads from getrandom") was
wrong. But the sharper reframing offered — an interception *with a hole* — is **also wrong**. The
interception is whole.

- Handler `detcore/src/syscalls/misc.rs:625`, fills via `fill_random_bytes` (`:465`, operative line
  `:491`) from a per-thread `Pcg64Mcg` seeded from the thread's **creation pedigree**
  (`tool_local.rs:2057` root, `lib.rs:1551` children) — not host entropy, not host Tid.
- Dispatched `lib.rs:2387`, subscribed `lib.rs:1124` inside `subscriptions()` (`lib.rs:1062`).
  Skipped **only** under `--passthru-opt`.
- Backend-independent (Detcore is the shared Reverie tool). Two additions, not exceptions: SaBRe
  detours the libc wrapper (`detcore-sabre/src/lib.rs:293`); record/replay have their own handlers
  (`hermit-cli/src/recorder/random.rs:18`, `replayer/random.rs:17`).
- Verified fresh: 12/12 runs of a direct `getrandom` guest give byte-identical draws; and in **one**
  12-run batch of the btrfs fixture the getrandom byte-hashes were identical 12/12 **while the UUID
  took two values (8 and 4)**.
- Cause of the UUID variation: libuuid XORs a `random()` mask over those bytes whose seed and
  stream position come from `gettimeofday`. The moving input is the **virtual clock** — the ~10 µs
  excursion filed separately. `strace`'s attribution to getrandom is incomplete, not wrong.

Full detail is in the task note on `btrfs-target-uuid-is-nondeterministic-under-strict` (closed).

## Open work I touched, with state

| item | state |
|---|---|
| PR 2910 (scalar blocking pipe write) | open, MERGEABLE/CLEAN, `acb2839ab`. Product + test half both green. Unlanded on the receipt blocker only. |
| PR 2909 (five KVM cells, observations) | open, unlanded, receipt blocker only. Row in backlog, I still own it. |
| PR 2694 (inherited-stdio append) | open at `446667cf0`, objection met with two mutations. **I am an author — do not let me approve it.** Binding needs the codex lane or the owner. |
| PR 2223 (robust futex) | open at `4767148cb`, ready, MERGEABLE/CLEAN, one merge from done. |
| PR 1689 | left open. Retired nothing: the retirement was already done, and the gate's second "marker" is prose in `issuecomment-5250055861`. Live findings behind it. |
| PR 2419, 2717 | closed as superseded, branches retained. |

## What I would do next, in order

1. **wrkslots outranks all of it.** `REFUSED: repository path must be relative to
   /home/newton/work/dev-hermit: '/home/newton/work/agent-utils'`, rc=3, on *every* read-only
   subcommand — so it is the registry read, not any target. It takes `ci-hub validate-run` with it,
   which is the landing admission gate, so **no PR on this host can get a receipt**. Both slots
   FREE, load-probe SUITABLE, main at `COMMITS-SINCE-GREEN 9 / NO-RECORD=9`. Invariant is
   `agent-utils/py/wrkslots/cli.py:3162`. **Not** from `.wrkslots.yml` (clean, schema 2, no
   `repositories` key), not ci-hub, not the environment — I did not find the caller supplying that
   path and did not guess.
2. `writev_determinism` is **red on main** (stale literal at `rs:66`); PR 2910 fixes it in passing.
   Anyone judging that test's colour should know it is not their change.
3. The interrupted-writes design question is with the owner — waking an `InternalIOPolling` target
   without rewriting its request into `{InboundSignal}` collides with the one-resource-per-request
   invariant (`scheduler.rs:3606`).

## Two habits that earned their keep tonight

- **Run the control at base.** Twice I was one step from filing a wrong finding — "the fix broke
  `writev_determinism`" (it was already red at main) and "the gate double-counts the refusal" (it
  does not; the second marker is prose). Both died to a control, not to more thinking.
- **A test encoding a past failure is evidence, not an obstacle.** The obvious interrupted-writes
  repair made a reproducer match native and re-created a scheduler panic that a sibling test
  documents. Reverting was the result, not a failure to finish.

## Paths a successor needs

- Retained evidence: `ignored/validate/demo08cert-20260831/`, `ignored/validate/kvm-five-20260831/`.
- Demo 8 assets: `ignored/demo08-btrfs` (symlink to the shared parent dir) — **do not modify its
  `.crash-seed`**.
- Every ad-hoc hermit run goes through `bin/safehermit`. Do not put assets under host `/tmp`.
