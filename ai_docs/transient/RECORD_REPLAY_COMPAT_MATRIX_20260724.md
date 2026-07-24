# Record/Replay Compatibility Matrix - 2026-07-24

## Scope

This matrix measures every program in the 147-row `validate.sh` strict
compatibility corpus at Hermit `5e66b600097deefb09c78510228815a0cbd2f83c`.
Each selected row was recorded with `hermit record start`, replayed with
`hermit replay --autopilot`, and required matching stdout.

Environment: ptrace backend, default log level, no relaxations, CentOS Stream 9.
Record/replay is reported as end-to-end R/R assurance, not as an AGENTS.md L2
strict-run claim.

## Result

| Outcome | Programs | Share |
| --- | ---: | ---: |
| PASS | 128 | 87.1% |
| FAIL | 19 | 12.9% |
| Total | 147 | 100% |

The result exceeds the task's 80% compatibility target. The blocking baseline
command completed 128/128 in 73 seconds:

```bash
RR_COMPAT_PHASE_TIMEOUT_SECONDS=30 VALIDATE_LABEL_PR=0 \
  ./validate.sh --rr-compat-only --no-label-pr
```

Passing programs (128):

```text
addr2line, ar, arch, as, awk, b2sum, base32, base64, basename, bash, bc,
bracket, bzip2, c++filt, cal, cat, chrt, cksum, cmp, column, comm, cpp, cut,
date, dd, df, dirname, du, echo, egrep, elfedit, env, expand, expr, factor,
fgrep, file, find, flock, fmt, fold, g++, gcc, gcov, getopt, git, gprof, grep,
groups, gzip, head, hexdump, hostname, iconv, id, ionice, java, join, ld,
logger, logname, ls, lua, make, md5sum, nice, nl, nm, nohup, nproc, numfmt,
objcopy, objdump, od, openssl, paste, perl, pinky, pr, printenv, printf, ptx,
pwd, python3, ranlib, readelf, readlink, realpath, rev, sed, seq, sha1sum,
sha224sum, sha256sum, sha384sum, sha512sum, shuf, size, sleep, sort, split,
sqlite3, stat, stdbuf, strings, strip, sum, tac, taskset, tee, test, tr, true,
tsort, tty, uname, unexpand, uniq, uptime, users, wc, wc-lines, whoami, xargs,
xxd, xz, yes, zstd
```

## Failing Rows

| Program | Record | Replay | Stdout | First observed blocker |
| --- | ---: | ---: | --- | --- |
| cargo | 0 | 124 | different | Replay event stream ends in `replayer/network.rs` while a rustup sibling is blocked in epoll. |
| rustc | 0 | 124 | different | Same rustup exec/epoll cancellation failure as cargo. |
| node | 0 | 124 | equal | Replay syscall/fd sequence diverges in `replayer/mod.rs`. |
| mktemp | 0 | 1 | equal | Replay does not materialize the recorded temporary directory for the later `rmdir`. |
| diff | 124 | 125 | different | Record panics decoding `FICLONE(3)` used by `cp`; replay is not attempted. |
| patch | 0 | 124 | different | Writable filesystem state is not materialized; replay takes a different path lookup branch. |
| tar | 0 | 124 | different | Created archive/input state is absent during replay, causing syscall divergence. |
| cp | 124 | 125 | different | Record panics decoding `FICLONE(3)`; replay is not attempted. |
| mv | 0 | 124 | different | Recorded rename/create state is absent during replay. |
| rm | 0 | 124 | different | Recorded create/unlink state is absent during replay. |
| mkdir | 0 | 124 | different | Recorded directory creation is not physically applied during replay. |
| rmdir | 0 | 124 | different | Recorded directory creation/removal state is absent during replay. |
| touch | 0 | 124 | different | Recorded file creation/timestamp state is absent during replay. |
| chmod | 0 | 124 | different | Recorded file creation/mode state is absent during replay. |
| chown | 0 | 124 | different | Recorded file creation/ownership state is absent during replay. |
| ln | 0 | 143 | different | Recorded hard/symbolic links are absent during replay. |
| csplit | 0 | 124 | different | Created input/output files are absent during replay; syscall sequence diverges at event 5. |
| install | 1 | 125 | different | Record panics decoding `FICLONE(3)` and reaches its record timeout; replay is not attempted. |
| mkfifo | 0 | 124 | different | Recorded FIFO creation is absent during replay; syscall sequence diverges at event 128. |

Exit 124 is the per-phase timeout after the replay emitted its fail-closed
panic. Exit 125 means replay was intentionally not started after record failed.
Task-owned replay processes that survived a timeout were explicitly terminated.

## Blocker Ownership

These failures are not independent defects suitable for nineteen local
allowances:

- Cargo/rustc have a prepared epoll-cancellation fix in TaskGraph task
  `impl-rr-cargo-rustc-replay-fix`; its recovery worktree remains uncommitted.
- Node reached a later fd-mapping divergence after the ioctl fix tracked by
  <https://github.com/rrnewton/hermit/pull/260>.
- Read-only filesystem snapshot work is in stacked draft
  <https://github.com/rrnewton/hermit/pull/591>, but these rows require physical
  replay of writable filesystem mutations.
- `FICLONE` decoding must be fixed before `diff`, `cp`, and `install` can finish
  recording, but writable-state replay is still required after that decoder
  fix.

The validation change keeps the 128 passing rows as the blocking gate and makes
the remaining 19 labels an explicit, disjoint known-failure set. Any new strict
corpus row missing from both sets now fails validation instead of silently
appearing as an unclassified skip.
