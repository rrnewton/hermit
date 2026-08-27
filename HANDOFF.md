# HANDOFF

Task: `did-the-sabre-thread-and-clone-landing-move-the-four-sabre-divergences`.

I was comparing the three still-open SaBRe cells before and after the 166-line Reverie thread/clone landing. `startup-tls-guards` is separate and already explained by https://github.com/rrnewton/hermit/pull/2730.

- Slot: `/home/newton/work/dev-hermit/worktrees/hermit-126-sabre-divergences`
- Measured Hermit source: `bae4a34fec5bfe45dd8cad00adcd5b6ddb04abcc`.
- Slot HEAD after committing this handoff: `fc1ae478e35d5f10d4326c2eceed997601f634fc` before the final handoff update commit; the only change above the measured source is `HANDOFF.md`.
- Pre-change Reverie pin: `ab07a89239150df3726a036bee9f5e897893dfc1`
- Post-change Reverie tree to compare: `d3cd29e2fef334108a4e99739a6a38a744628702`; it contains tested pull-request head `b540fa1f9cdb60c397f43b182d86bb7482d6a373` by content.
- Current release Hermit binary SHA-256: `b8381e363b5dad66520f412414681a19c712d5b25020d55378424873e5360fbe`, built against the pre-change pin.

A focused pre-change SaBRe repeat completed; it was not a validate. Unified exec session `35245`, shell PID `2401897`. Results and reports are under:

- `/home/newton/work/dev-hermit/worktrees/hermit-126-sabre-divergences/ignored/h126-sabre-impact/safe-base-pthread`
- `/home/newton/work/dev-hermit/worktrees/hermit-126-sabre-divergences/ignored/h126-sabre-impact/safe-base-lock`
- `/home/newton/work/dev-hermit/worktrees/hermit-126-sabre-divergences/ignored/h126-sabre-impact/safe-base-ppoll`
- Progress: the adjacent `.out` files.
- Command: `/home/newton/work/dev-hermit/worktrees/hermit-126-sabre-divergences/ignored/h126-sabre-impact/run-safehermit.sh`

Final pre-change counts: pthread completed 20 attempts with 16 diverged and 4 matched; lock completed 20 with 16 diverged and 4 matched; ppoll completed 124 with 7 diverged and 117 matched. Every invocation used `/home/newton/work/dev-hermit/bin/safehermit` with a 30-second wall bound. No `no_result` occurred. No full validate is running.

Next:

1. Mechanically update the Hermit Reverie pins and derived pin/budget files to `d3cd29e2fef334108a4e99739a6a38a744628702`, run the pin checker, rebuild release Hermit/SaBRe artifacts through the focused pressure runner, and run identical counts into `safe-post-{pthread,lock,ppoll}`.
2. Restore the pre-change pin and require a clean worktree apart from the committed handoff.
3. Compare first divergence by event content plus syscall or scheduler turn, never record number.
4. Supersede the earlier direct-run TaskGraph note: its larger counts invoked `target/release/hermit` directly and do not satisfy the current `bin/safehermit` rule. Official pressure-runner evidence remains valid.

Unverified conclusion: existing evidence says the landing did not remove or relocate the three divergences. Source inspection agrees: all three guests use `pthread_create`/`pthread_join`, while the landing's guard is active only for process-forming clone/fork without `CLONE_VM`. The safehermit post-change half is still required before this is final.
