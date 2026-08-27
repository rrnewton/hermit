# HANDOFF

Task: `hermit_2427_changes_requested` for https://github.com/rrnewton/hermit/pull/2427.

I was preparing the required exact-head full validation on restored `devbig030` access. No validation was launched before the reboot checkpoint, so there is no running unit or log path to preserve.

Working slot: `/home/newton/work/dev-hermit/worktrees/130/hermit-pr2427`

Hermit branch: `hermit-130/adopt-pr2427`

Hermit branch head and pushed PR head: `adcd6553356ab093bf38625678a9b280484c0076`

Freshly fetched Hermit `origin/main`: `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab`

The branch is clean, one commit ahead and six commits behind that main. A normal validate would refuse it as not containing current main. The implementation patch had remained range-diff `=` across the preceding rebases, but that has not yet been rechecked against `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab`.

Hermit's pinned Reverie revision in Cargo manifests and lockfiles: `ab07a89239150df3726a036bee9f5e897893dfc1`.

Current Reverie main measured by `git ls-remote` immediately before this handoff: `063fa37b05e562760f3d27c80ed8a6482b97b44a`.

Existing code approval: agent `hermit-131`, https://github.com/rrnewton/hermit/pull/2427#issuecomment-5439377770, originally published for `d254c5e6babe27b65b0364c78aa39d04807b3684`. Task notes record later range-diff-equivalent rebases through `adcd6553356ab093bf38625678a9b280484c0076`; exact binding after the next rebase remains to be verified.

Previous full validation at `d254c5e6babe27b65b0364c78aa39d04807b3684` failed only at `pre.reverie_pin`; 24 nodes were dependency-skipped and `executed_tests` was unknown. Log: `/home/newton/work/dev-hermit/ignored/validate/validate-full-d254c5e6babe-validate-hermit-130-d254c5e6babe-1787833010990971193-2801213-9e95d298.log`.

The later queued validation at `adcd6553356ab093bf38625678a9b280484c0076` was evicted before entering a box because main advanced. Log: `/home/newton/work/dev-hermit/ignored/validate/validate-hermit-130-adcd6553356a-1787835714674930717-2295564-169ffd73.log`.

Next action: fetch Hermit main, rebase the single PR commit onto `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab` or newer current main, require `git range-diff` equivalence, push the updated PR branch, obtain exact-head Codex binding from `hermit-131`, and immediately launch `ci-hub validate-run` for the new exact head on `devbig030`. Record the generated validate unit and durable log path in `hermit_2427_changes_requested`.

Comparison rule after validation: no branch cell may fail when baseline passes; failing-to-passing is desired. Exclude `pre.reverie_pin` and `check.lint_checks` because their verdict depends on run timing rather than the branch change. Read the trailing summary, per-test-id retry counts, and failed test-id list.

Safe Hermit wrapper rule: any Hermit invocation outside validate and end-to-end manifest infrastructure must use `bin/safehermit`.
