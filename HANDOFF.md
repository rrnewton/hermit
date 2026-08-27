# HANDOFF

## Current work

Task `green-cells-that-diverge-then-pass-on-retry-must-record-the-divergence` is complete and closed.

The exact subject was preserving a framework-written divergence when a green pressure-test cell passes on retry. The implementation touched `ci/compat-envelope/pressure-test.rs` and `ci/compat-envelope/scorecard.rs`.

- Producer and event-content fields landed through https://github.com/rrnewton/hermit/pull/2748 at Hermit main `ffd409b28ec81a97b6f713b11823e57169bc3220`.
- Green retry summary retention landed through https://github.com/rrnewton/hermit/pull/2751 at Hermit main `52d0b4d9f44b8ee4d98e89aafce11338d4c9f186`.
- Working branch before this handoff commit: `hermit-129/green-retry-summary` at `761780cc771598c857d7f2bbfc45ba306e7269c3`.
- Current fetched Hermit `origin/main`: `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab`.
- Hermit at current `origin/main` pins Reverie `ab07a89239150df3726a036bee9f5e897893dfc1`.
- Current fetched Reverie `origin/main`: `063fa37b05e562760f3d27c80ed8a6482b97b44a`.

The feature worktree was clean before adding this handoff. Both changed Hermit files were verified byte-identical on remote main after landing.

## Validation

At exact feature head `761780cc771598c857d7f2bbfc45ba306e7269c3`:

- `./ci/compat-envelope/pressure-test.rs self-test` passed.
- `./ci/compat-envelope/scorecard.rs self-test` passed.
- `cargo fmt --all -- --check` passed.
- `git diff --check origin/main...HEAD` passed before landing.

No guest/backend validate was run for this follow-up. Assurance was focused L0 script self-tests, default log level, no relaxations.

## Running validate

None. This task owns no validate handle or log path. The focused self-tests completed before this handoff.

## Next action

No implementation remains on this task. After reboot, first fetch Hermit main and confirm commits `ffd409b28ec81a97b6f713b11823e57169bc3220` and `52d0b4d9f44b8ee4d98e89aafce11338d4c9f186` remain represented by content on `origin/main`, then read TaskGraph for a new assignment. Exact repeated-evidence collapsing and counts remain owned by agent hermit-131; do not duplicate that work.

## Unverified

Nothing is pending verification for this task. Hermit main advanced after the landing; no claim is made that current main `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab` has received a new full validate because of this change.
