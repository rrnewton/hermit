# demos-calib — Demo 8 crash criterion: review, and two controls

SETTLED HEAD, HELD: `d04c62f74a3a262f579da1cc8722baec2ce5ef07`
on `rrnewton/hermit` branch `move-demos-directory-into-hermit`, appended to
`532d602832bdb3a8be9f49e7f255d737dcd3e4ac` (verified as an ancestor — no
rewrite). Verified by reading the branch back from the remote and by confirming
both new control strings are present in the remote blobs. The branch has a
stacked pull request, so keep it append-only.
Pull request: https://github.com/rrnewton/hermit/pull/2904
Task: `allow_merge_gate_to`, claimed by `reviewer-demos`.

**This lane is holding the head stable and will not move it.** Four lanes have
already been spent on heads that moved.

The full review and the mutation matrix are in the TaskGraph note on that task,
stored and verified byte-for-byte (8226 bytes, 132 lines). Read it first.

## What this lane did

Reviewed `532d60283` adversarially, then — at the owner's explicit instruction,
because the authoring lane was closed — wrote the two controls that review asked
for, as `d04c62f74`.

**The review's changes-requested verdict is NOT cleared, and must not be cleared
by this lane.** A second lane confirms. That separation is deliberate: the
project hit this exact tension before when an adopting agent withdrew its own
refusal.

## The review, in one paragraph

The trap check passed. `532d60283` repaired the CALIBRATION, not what the demo
tolerates — the demo got strictly stricter on both Step 2 and Step 4, and the
calibration's per-run budget now derives from `DEMO08_TIMEOUT` instead of a
hard-coded 150s. No goalpost lowering anywhere. Two findings: neither half of
the crash criterion (exit 134; a complete report) had a control that failed on
it alone, so either could be deleted with a green suite. `d04c62f74` closes
both. Findings 3-6 in the note are unaddressed and are notes, not blockers.

## What is still open

1. **Confirmation of `d04c62f74` by a lane that is not this one.** The two new
   controls and the changes-requested verdict both need it.
2. **The TaskGraph claim is held by this lane.** A confirming lane needs it
   released, or needs to be dispatched with that understood. Ask the owner;
   don't take it silently.
3. Exact-head Demo Hot Path evidence and a `Demo-Green-Review` attestation for
   `demos/08-btrfs-convert-uaf.sh`. Still honestly red: measured at this head,
   `scripts/check-demo-review.sh --range ccb3e5c07..HEAD` returns rc=1. This is
   also what settles finding 3 (the demo now depends on the real pipeline
   returning exactly 134, and no run at this head evidences that).
   `d04c62f74` itself touches no `demos/**` path, so the gate did not fire for
   it and **no override was used for it**.
4. The task's own headline requirement — Merge Gate obtaining and consuming an
   exact-head Demo Hot Path run, plus the `MERGE_GATE_V4_BLOB` update and
   readback. NOT done, unchanged from the previous handoff. No repository
   variable was changed and no workflow was dispatched.
5. A pull-request comment naming this settled head was NOT posted. The previous
   lane hit `ci-hub/bin/gh` refusing with `owner unresolved: identity resolver
   returned DISAGREEMENT` (`worktree-state.json` slot 106, `hermit-106`, status
   `lease-quarantined`, claiming `$TMUX_PANE` `%25`). This lane did not retry it
   and did not edit the registry. Needs owner authorization or a resolved
   identity.
6. The other review findings on this pull request (Demo 5/6 QMP repeat contract,
   Demo 2 release replay) are not this lane's and were not touched.

## Local environment note

Unchanged and independently confirmed: the real Demo 8 cannot run on this box.
`e2fsprogs-devel` is absent, so `configure --with-convert=ext2` fails with
`Package requirements (ext2fs) were not met`. The cached asset set at
`worktrees/slots/demo08review/ignored/demo08-btrfs` predates the fixture patch
that sets `abort_on_error=1`, so its use-after-free runs exit 1 rather than
aborting and cannot exercise the 134 path. Do not read that as evidence against
the 134 requirement — but do note it is exactly the shape the new
`complete-uaf-rc0` calibration control now refuses.

`wrkslots adopt demos-calib` still refuses on the unrelated pre-existing registry
inconsistency (`slot directory is missing or unsafe: worktrees/slots/lander-3`),
so this slot's owner remains unbound.
