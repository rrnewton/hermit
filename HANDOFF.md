# e9patch Real Backend Handoff

Task: `impl-e9patch-real-backend`

## Worktrees

- Hermit: `/home/newton/work/dev-hermit/worktrees/slot115`
  - branch `impl-e9patch-real-backend-hermit-slot115`
  - PR https://github.com/rrnewton/hermit/pull/711
  - dirty integration changes are intentional and uncommitted
- Reverie: `/home/newton/work/dev-hermit/worktrees_reverie/slot115`
  - branch `impl-e9patch-hardening-slot115`
  - PR https://github.com/rrnewton/reverie/pull/103
  - pushed head `b53734d09ea6c64c1bf8a6064a592ec0405f1f50`
  - one intentional uncommitted file: `reverie-e9patch/src/backend.rs`

## Completed

PR #102 landed the hybrid Reverie backend. PR #103 fixes first-round review
blockers: clone/fork child context, non-returning `rt_sigreturn`, exact trap RIP
provenance, rejected unrepresentable register writes, zero-site fallback,
namespace-preserving executable identity, backing-path lifetime, non-ELF
fallback, audit tags, and lifecycle regressions.

Validated before the latest uncommitted review fixes:

- Reverie normal e9patch/ptrace tests passed (84 plus doctest).
- Configured real e9tool suite passed 7/7: emulation, injection, unsubscribed
  delivery, clone, `rt_sigreturn`, marker collision, and non-ELF fallback.
- Reverie fmt and strict Clippy passed.
- Hosted PR #103 Regular and Host-dependent checks passed at `b53734d`; merge
  gate was red only because the PR is still draft/race-gated.
- Requested Hermit echo smoke passed.
- Nonzero-site identity guest passed L2 with one recovered/patched root syscall
  and `/proc/self/exe == argv[0]`, default logging, no relaxations.
- The same L2 test passed with both e9 tools stored under isolated `/tmp` as
  regular files.
- Focused Hermit e9 CLI tests passed, including run-only scope and
  `--no-namespace` rejection.

Task notes contain detailed evidence. Do not close until both PRs are landed and
the real CLI audit passes on Hermit `main`.

## Round-Two Review

Integration reviewer confirmed pins, testutils, Detcore `source="injected-trap"`
trace, identity L2, clone/signal tests, direct `/tmp` tools, and CLI scope. It
found three issues:

1. **Blocker:** safe public `hermit::run_with_backend` APIs call mount-preserving
   Reverie APIs without proving a private mount namespace.
2. **Major:** `/tmp` tool symlink aliases were canonicalized away, so Reverie
   reread an unmounted original environment path.
3. **Major:** zero-site diagnostic falsely reported `event_source=injected-trap`.

Fixes already partially applied:

- Hermit `e9patch::tool_paths()` now uses `std::path::absolute` and preserves
  the configured alias. This is uncommitted.
- Reverie zero-site diagnostic now selects `event_source=ptrace`.
- Reverie preserving APIs are now declared `pub async unsafe fn` with a Safety
  contract requiring a disposable private mount namespace.
- These latest Reverie changes are uncommitted and need fmt/tests/Clippy,
  commit, push, and re-review.

Core reviewer round-two attempts disconnected twice. Obtain a replacement core
review after the safety split is complete.

## Immediate Next Steps

1. Finish the Hermit safety split. A prepared patch exists at
   `/tmp/hermit-e9-container-dispatch.patch`, but **it has not been applied**;
   its dry run failed because the first import hunk count is wrong. Intended
   design:
   - Safe public `hermit::run_with_backend` and output variant must reject
     `Backend::E9patch`.
   - The CLI `run.rs`, only inside `with_container`, directly calls the new
     unsafe preserving Reverie APIs and cleans up Detcore global state.
   - Route both normal and verify/captured CLI paths through these local
     container-only helpers.
   - Add `TODO-HUMAN-REVIEW(PR-711)` beside the existing no-namespace check.
2. Add a safe-libhermit rejection regression.
3. Test the `/tmp` **symlink** tool case, not only regular files.
4. Run Reverie fmt, normal tests, 7 real tests, and strict Clippy; commit/push
   the uncommitted diagnostic/unsafe API changes.
5. Repin all Hermit Reverie manifests plus lock to the final PR #103 head. They
   currently pin `b53734d` consistently. Each new git revision causes a slow
   DynamoRIO rebuild; avoid repinning repeatedly.
6. Complete replacement round-two core review and integration re-review.
7. Mark PR #103 ready, rerun/obtain green merge gate, label per post-facto
   review, and squash-merge. Repin Hermit to the merge SHA.
8. Commit/push PR #711, update its body, run hosted CI, resolve failures, review,
   label, and merge.
9. On Hermit `main`, run backend-reality audit: e9 L2 for echo/true/cat, actual
   source-tagged event trace, INFO comparison with ptrace, and report honest
   hybrid limitations. Then close the task.

## Important Details

- Use `with-proxy` for every network command.
- Build with `CARGO_BUILD_JOBS=1` and explicit `/usr/bin/cc`; the host is busy.
- Each new Reverie git pin rebuilds DynamoRIO for 6-8 minutes.
- The current Hermit lockfile and manifests consistently use `b53734d`.
- PR #103 is still draft. PR #711 is ready but stale and must not land first.
- Shared preload/RPC crates are dependencies only; the active backend is honest
  hybrid e9-origin trap plus ptrace controller, not a ptrace-free in-guest RPC
  fast path.
- A prior task note was corrupted by shell backtick substitution; a following
  note prefixed `CORRECTION TO PRIOR NOTE` is authoritative.
