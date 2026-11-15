---
schema: syscall-pages/v1
syscall: writev
nr_x86_64: 20
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+writev&body=Discussion+for+the+%60writev%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwritev.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60writev%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`writev(fd, iov, iovcnt)` writes the concatenation of several buffers to a file descriptor as one call (a gather write). Detcore models it. File writes are ordered through a resource, internal pipes and sockets go through the deterministic nonblocking retry path, logically blocking pipe writes complete in a dedicated helper, results are recorded and replayed, and the virtual mtime advances (`detcore/src/syscalls/files.rs:2437-2507` at hermit f4055eb9cae4). One gap is known: a signal sent to a writer parked on a full pipe can hang the guest.

## Linux semantics

`writev` behaves like `write` of the buffers concatenated in array order. On a regular file it writes at the file offset, or at end of file with `O_APPEND`, and advances the offset. A regular-file write is atomic with respect to other `read`/`write` calls on the same open file.

On a pipe or FIFO:

- A total of at most `PIPE_BUF` (4096) bytes is written atomically: all or nothing, never interleaved with other writers.
- A larger write may be split and interleaved.
- A blocking writer waits for space until everything is written. If a signal arrives after some bytes went out, the call returns the partial count. If it arrives before any byte went out, the call gives `EINTR`, or restarts under `SA_RESTART`.
- An `O_NONBLOCK` writer gives `EAGAIN` when there is no space, or returns a partial count.
- Writing when no reader is left raises `SIGPIPE` and gives `EPIPE`.

Validation errors:

- `iovcnt` above `IOV_MAX` (1024), or a summed length above `SSIZE_MAX`, gives `EINVAL`.
- A bad iovec array or a bad first buffer gives `EFAULT`, and a failed write does not change mtime or ctime.
- `EBADF` for a closed fd or one not open for writing.

A successful write of one or more bytes to a file updates mtime and ctime.

### Flags and options

- F1: buffers are written in iovec order as one operation; the reader sees the concatenation.
- F2: `iovcnt` 0 returns 0; `iovcnt` above 1024, or a summed length overflowing `SSIZE_MAX`, gives `EINVAL`.
- F3: a total of at most `PIPE_BUF` (4096) bytes to a pipe is atomic and never interleaved with other writers.
- F4: a logically blocking pipe write larger than the free space blocks until it completes, including large multi-iovec writes and several blocked writers.
- F5: a signal delivered to a blocked pipe writer before any progress gives `EINTR`, or restarts under `SA_RESTART`.
- F6: a signal delivered after partial progress returns the partial byte count (handler caught, blocked, ignored or mixed).
- F7: an `O_NONBLOCK` pipe or socket with no space gives `EAGAIN` or a partial count.
- F8: a pipe or socket with no reader gives `EPIPE` and raises `SIGPIPE`.
- F9: a socket (for example an `AF_UNIX` stream socketpair) receives the gathered payload in order.
- F10: regular file: writes at the file offset, or at end of file under `O_APPEND`, and advances the offset; a short count is possible (`ENOSPC`, `RLIMIT_FSIZE`, which also gives `EFBIG`/`SIGXFSZ`).
- F11: `EFAULT` for a bad iovec array or buffer, with file timestamps unchanged by the failed call; iovec arrays held in read-only memory are valid.
- F12: metadata side effect: a write of one or more bytes updates mtime and ctime.
- F13: `EBADF` for a closed fd or one not open for writing.
- F14: a Detcore timer-slack procfs fd (`/proc/<pid>/timerslack_ns`): emulated write of the virtual value.
- F15: the fd number is replaced (closed and reused by another thread) while a blocking pipe writev is parked.

## Determinism analysis

A regular-file `writev` repeats given the same contents and offset, but the order against other writers of the same file depends on host scheduling, and mtime and ctime come from the host clock (F12).

On pipes and sockets, the following all depend on when the reader drains: how much a write moves before blocking, whether a blocked writer is interrupted before or after progress, and how writes over `PIPE_BUF` interleave between writers (F4, F5, F6). The host pipe capacity also affects this: 64 KiB by default, or two pages under `pipe-user-pages-soft` pressure.

Short counts on files depend on disk space and limits (F10). Hence `conditional`.

## Hermit design

The policy is `model`, as implemented at hermit f4055eb9cae4:

- **Dispatch:** `Syscall::Writev` goes to `handle_writev` (`detcore/src/lib.rs:2361`, with `TODO-HUMAN-REVIEW(#547)` at `detcore/src/lib.rs:2360`). The handler's contract is to "Preserve the initial writev as one kernel operation so its iovec order remains intact" (`detcore/src/syscalls/files.rs:2441-2443`) (F1).
- **Timer-slack procfs fds** are emulated after Linux-style iovec validation (`detcore/src/syscalls/files.rs:2449-2453`, validation at `detcore/src/syscalls/files.rs:191-224`, `writev_timer_slack` at `detcore/src/syscalls/files.rs:721`) (F14, F2).
- **Ordering:** a `Permission::W` resource is taken on the open file's contents (`detcore/src/syscalls/files.rs:2473-2484`).
- **Logically blocking internal pipes** are physically nonblocking because Hermit injects `O_NONBLOCK` at creation. They go to `execute_blocking_pipe_writev` (`detcore/src/syscalls/files.rs:2486-2489`, helper at `detcore/src/syscalls/helpers.rs:280-477`):
  - writes of at most `PIPE_BUF` are kept atomic (`detcore/src/syscalls/helpers.rs:334`) (F3);
  - the remainder after a partial write is issued as scalar `write` segments on the deterministic retry schedule (`detcore/src/syscalls/helpers.rs:438-447`) (F4);
  - a `Signaled` resume returns `EINTR` or the partial count (`detcore/src/syscalls/helpers.rs:387-391`, `detcore/src/syscalls/helpers.rs:459-460`) (F5, F6);
  - if the fd was replaced while parked with no bytes written, the operation is refused with `EOPNOTSUPP` through `refuse_unserviceable_operation` instead of writing to the new file (`detcore/src/syscalls/helpers.rs:393-407`) (F15).
- **Other physically nonblocking pipes, sockets and eventfds** go through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:2490-2493`, `detcore/src/syscalls/helpers.rs:196-278`): internal fds are retried deterministically, and external fds block on the host under record/replay (F7, F8, F9).
- **Everything else** is a single `record_or_replay_preserving_tool_errors` (`detcore/src/syscalls/files.rs:2494-2497`) (F10, F11, F13). Unlike `write` (`detcore/src/syscalls/files.rs:2287-2324`) and `pwrite64` (`detcore/src/syscalls/files.rs:2372-2422`), there is no short-write completion loop for regular files, so a host short count (F10) reaches the guest as is.
- **Metadata:** with metadata virtualization on and at least one byte written, `touch_file` advances the virtual mtime (`detcore/src/syscalls/files.rs:2499-2503`) (F12).

**Known gap:** `tests/c/writev_determinism.c:1376-1391` records that interrupting a writer parked on a full pipe with a signal, where Linux returns the positive partial count, hangs under Hermit (measured 2026-09-01 on ptrace with `--strict`). The comment attributes it to the shared InternalIOPolling retry loop not seeing the `Signaled` status for a parked thread. That mode is off the default path. This is a defect inside a `model` policy, not a reason to choose another policy, so `tracking_status` stays `todo`.

## Recommended tests

- T1 (covers F1, F3, F4, F5, F6, F9, F11): stdout, pipe and socketpair gathers, an atomic write to a full pipe, a read-only iovec array, a large iovec snapshot, large blocking pipe and scalar writes, two blocked writers, `EFAULT` with timestamps preserved, signal interrupt and restart, and four partial-signal variants. Existing candidate: the default path of `tests/c/writev_determinism.c` (manifest cell `c-programs/writev-determinism`, `tests/e2e/manifests/c-programs.yaml:10862`, verify on ptrace with `ci: true`).
- T2 (covers F14, F2): vector write to a timer-slack procfs fd matches Linux. Existing candidate: `timer_slack_procfs_vector_io_matches_linux` in `detcore/tests/misc/mod.rs:434`.
- T3 (covers F15, F4, F5): the fd replaced while parked is refused with `EOPNOTSUPP` before any byte reaches the new pipe; signal interrupt and restart; and record mode on pipe retries (`record-pipe`) and on stdout (`record`, with replay matching). Existing candidate: `writev_uses_fd_aware_scheduling_and_verifies` in `hermit-cli/tests/writev_determinism.rs` (it runs `tests/c/writev_determinism.c` with `fd-replacement` at lines 327-360, `record-pipe` at lines 379-401 and `record` at lines 403-428). It passes no `--backend`, so it runs on the default backend.
- T4 (covers F6): the `interrupted-writes` mode of `tests/c/writev_determinism.c` must return the positive partial count instead of hanging; enable it once the retry-loop gap is fixed.
- T5 (covers F2, F13): `iovcnt` 0, `iovcnt` 1025, a summed length overflowing `SSIZE_MAX`, a closed fd and an `O_RDONLY` fd each return the Linux result.
- T6 (covers F7, F8): writev to a full `O_NONBLOCK` pipe gives `EAGAIN` or a partial count; writev to a pipe whose reader closed gives `EPIPE`, and `SIGPIPE` is delivered (or ignored), identically across runs.
- T7 (covers F10, F12): writev to a regular file under `O_APPEND` and at an offset, checking the offset, the content and the virtualized mtime; with `RLIMIT_FSIZE` set below the total and `SIGXFSZ` ignored, expect a short count, then `EFBIG`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (resource-ordered writev, blocking-pipe helper keeps PIPE_BUF atomicity and signal semantics, fd-replacement refusal, record/replay, mtime virtualization); linux_determinism conditional confirmed; 15 flags, T1-T7 proposed; known gap: interrupted full-pipe writer hangs (writev_determinism.c:1376-1391); no regular-file short-write loop; tracking_status stays todo.
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/writev_determinism.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `writev`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+writev&body=Discussion+for+the+%60writev%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwritev.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60writev%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `writev`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should `handle_writev` complete short writes on regular files under `deterministic_io`, as `write` and `pwrite64` do? Today a host short count reaches the guest unchanged (`detcore/src/syscalls/files.rs:2494-2497`).
- Fix the InternalIOPolling retry loop so that a signal delivered to a thread parked in `execute_blocking_pipe_writev` surfaces as a partial count. Until then, the `interrupted-writes` mode of `tests/c/writev_determinism.c` hangs.
