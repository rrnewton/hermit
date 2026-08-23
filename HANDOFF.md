# HANDOFF — hermit-det4 (2026-08-06, teardown)

All work is committed and pushed. **Nothing uncommitted, nothing unpushed.**

## This slot

| | |
| --- | --- |
| task | `detinode-newtype-make-invalid-unrepresentable` (Stack 1.1) |
| branch | `fix/detinode-newtype-host-inode-leak` |
| SHA | `cd8a43ab7030aaa24363eb267e97a6f23e55805d` |
| base | hermit main `4c70658e785834737cbe1524f77330c781a6f5ea` |
| PR | https://github.com/rrnewton/hermit/pull/1681 (draft) |
| state | IMPLEMENTED — code + tests green locally; **no green validate receipt** |

**Done.** `DetInode` is a real newtype over `u64` (private field, no `From<RawInode>`, sole
constructor `from_ordinal` with 3 callers). Four host-inode leak sites in
`detcore/src/syscalls/files.rs` now route through the existing `determinize_inode` path. The
compiler found a **fourth** site (`files.rs:849`, sendfile's `out_inode`) that a prior manual audit
had flagged as unverified. `Debug` is hand-written so DETLOG still reads `FileContents(4)`, not
`FileContents(DetInode(4))`.

**Verified.** e2e: `FileContents(221742951)`/`(221742955)` → `FileContents(4)` in both runs; logs
byte-identical with only the wall-clock prefix stripped; `log-diff` 140|140. Bracketed both ways —
re-planting the pre-fix expression gives `E0631` at that line. 386 + 56 tests, fmt clean, clippy 0.

**Next step.** One `validate-run` at `cd8a43ab7…`. Last attempt: 6 passed / 1 failed, and the
remaining red is **not** this diff — `[scheduler] reaped 4 leftover step cgroup(s) on exit`, an
environment fault. The two libunwind reds that preceded it are fixed and landed (parent `8117b39c`,
`c6265f5`).

**Gate.** Landing is serial and owned by hermit-det2. Do not merge out of band.

## My other branches (all pushed, all with draft PRs)

| branch | SHA | PR |
| --- | --- | --- |
| `landing/coalesce-clean7-onto-4c70658e7` | `5801ba524e46bbe6f7050e2b3bc208a8f1df2bf7` | #1670 |
| `feat/cross-backend-detlog-diff-harness` | `bc461a2608e2d7dca2f56293312e9bc2aa270182` | #1709 |
| `ci/dedicated-linux-boot-lane` | `97a5d6be758e55f53b8c0e36c1d69c975ff0d2ec` | #1736 |

`#1709` gained a final commit at teardown: the harness picked a non-empty `--log-file` over stderr,
which measured SaBRe against a 4-record stream instead of its 90-record one. Fixed to prefer the
richer stream; both bracket halves re-verified.

## Parent-side (all landed on `origin/main`, each verified by re-reading the ref)

| commit | what |
| --- | --- |
| `e9d433c7` | validate admission fetches through herdr-run — **this is what made agent validate possible at all** |
| `8117b39c`, `c6265f5` | validate unit gets libunwind's build/link/**runtime** paths (three different dirs; the runtime one is fbcode, not lu-parity) |
| `7ed27dc7` | reverie + agent-utils gitlinks repinned to tool-derived `origin/main` |
| `e2ae9b9b` | published two orphaned ai_docs commits; `70c985f` published eight more |
| `699963ae`, `20fa5c53`, `c08e2594` | prefix-parity depth · golden self-determinism per rung · the rung ladder |
| `661c30cc` | SaBRe ptracer-residual design (no code — the removal was not made) |
| `557bf8d8` | the rung harnesses, rescued from `ignored/` before teardown |

## Two things a successor should not have to rediscover

1. **The parent index is shared and was occupied by 106 other paths.** Every parent commit above
   was made with `commit-tree` and a temp `GIT_INDEX_FILE`, never `git add`. Three blobs staged in
   that index are **stale pre-fix copies** of `ci-hub/validate/{preflight_validate.py,start_unit.py,
   tests/test_start_unit.py}` — committing that set would revert `e9d433c7` and `8117b39c`/`c6265f5`
   and take the fleet's receipt path back down.
2. **Three "determinism failures" I found were unpinned guest inputs, not product bugs** — a shared
   Python `.pyc`, a run directory arriving via `execve` argv, and gcc's own leftover output file.
   Before blaming the reference, check what the guest reads that the previous run wrote.
