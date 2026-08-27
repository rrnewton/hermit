# HANDOFF

Task: `drain-open-pr-hermit-2694`, https://github.com/rrnewton/hermit/pull/2694.

Slot: `/home/newton/work/dev-hermit/worktrees/hermit-133-pr2694`.
Branch: `hermit-133/pr2694-sendfile`.
Hermit head before this handoff commit: `b350eb1b63d68a31f5b8d49b479b22d8a94118ed`.
That branch was rebased onto fetched Hermit main `fc9b323cfb298063fec4a1fb56f93ec291c36c6c`; the shared remote-tracking ref advanced during the focused rerun to `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab`, so the branch must be rebased again before landing.
The tree pins Reverie `ab07a89239150df3726a036bee9f5e897893dfc1`.
The live PR branch was still `f6d7aac97b855c2271e6c76862aecdc2dad8e3c3` when last read.

Subject: meet the standing Codex objection that inherited-stdio logical `O_APPEND` was honored only by `write` and `writev`. The fix returns `EINVAL` for `sendfile` to an inherited append-mode regular output, rewrites `pwrite64` and `pwritev` to the current end, and supplies `RWF_APPEND` to `pwritev2` unless `RWF_NOAPPEND` is explicit. The audit found `copy_file_range` already returns `ENOSYS`; `splice`, `tee`, and `vmsplice` fail closed. The new regression verifies exact bytes, errno/result, input offset, inherited descriptor offset, and unchanged supervisor flags.

Evidence already recorded in TaskGraph: native `sendfile` returns `-1/EINVAL`, preserves the seven-byte prefix and leaves input offset zero; the prior PR product returned 6, consumed the input and overwrote the prefix. Native `pwrite`, `pwritev`, and `pwritev2` append while preserving the descriptor offset; the prior product overwrote at offset zero. Before the latest rebase, focused ptrace passed 1 of 1 across `sendfile`, `pwrite64`, `pwritev`, `pwritev2`, and `RWF_NOAPPEND`; focused KVM passed 1 of 1 across its accepted `sendfile` path plus a pipe control. KVM independently rejects positioned writes to standard descriptors (`pwrite64` with `EBADF`; `pwritev`/`pwritev2` with `ENOSYS`), so the KVM test does not call those accepted paths.

Running work at checkpoint: focused ptrace+KVM rerun on the rebased tree, shell PID 2525700 / cargo PID 2526049, session handle `21678` (tool continuation cell `2249`), log `/tmp/hermit-133-pr2694-rebased-focused.log`. It was still compiling after package-cache waits when reboot was announced. This is not a full validate and may die. A safety-branch push of `b350eb1b63d68a31f5b8d49b479b22d8a94118ed` was also in progress under wrapper session `7811`; verify the remote ref rather than trusting that report.

Next: verify `origin/hermit-133/pr2694-sendfile` by content; fetch current main and live PR head; rebase this complete six-commit PR onto current main; recount `run_kvm_` tests and keep `ci/dag/privileged.json` exact; rerun the two focused tests; run mutation controls that separately remove the `sendfile`, `pwrite64`, `pwritev`, and `pwritev2` handling; run the inherited-stdio set, Detcore library tests, fmt, clippy and diff checks; publish the corrected PR head with an expected-SHA lease; post the disposition on the PR; obtain fresh exact-head independent reviews and an exact-head ci-hub validate receipt; then land with `ci-hub/bin/gh-merge-verified 2694 --repo rrnewton/hermit -- --rebase` and verify every changed path against freshly fetched `origin/main`.

Unverified: the rebased focused two-test run had not completed at checkpoint. The current branch has not yet been rebased from `fc9b323cfb298063fec4a1fb56f93ec291c36c6c` onto `26f5bc2de3d328b64c41c42ef093ecb9d89e98ab`. No full validation exists for this corrected head.
