---
schema: syscall-pages/v1
syscall: close
nr_x86_64: 3
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+close&body=Discussion+for+the+%60close%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclose.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60close%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 12
---

## Summary

`close(fd)` removes a descriptor from the process's descriptor table and drops one reference to its open file description. The last reference releases the description, with side effects such as releasing POSIX record locks and signalling end of file to pipe readers. Detcore handles it with a typed handler (`detcore/src/lib.rs:2365`, `handle_close` at `detcore/src/syscalls/files.rs:1138` at hermit f4055eb9cae4). The handler runs the call in the kernel and mirrors the result in Detcore's descriptor table.

Manual reference: Linux man-pages close(2).

## Linux semantics

`close` takes no flags: its behaviour depends on the descriptor and on what else refers to the same open file description and inode. On Linux the descriptor is released before any flush runs, so after `close` returns, with any result except `EBADF`, the number is free and the next `open`, `dup` or `socket` may reuse it. Linux never retries a close after `EINTR`, and a program must not retry either: the number may already belong to another thread's new descriptor.

Effects of the call:

- Every close by a process releases all POSIX (`fcntl`) record locks that process holds on the inode, even through other descriptors.
- The close of the last reference to an open file description runs the file's release: `flock` and open file description locks are dropped, pipe and socket peers see end of file or `EPIPE`, a file that was unlinked while open is freed, and devices run their release handlers.
- Filesystems with deferred write-back, such as NFS and FUSE, report errors from that write-back as the result of `close`.

### Flags and options

- F1: valid descriptor: removes it from the table and returns 0. The number becomes free immediately.
- F2: invalid descriptor (never opened, already closed, negative or above the limit): fails with `EBADF` and changes nothing.
- F3: descriptor reuse: the next call that allocates a descriptor gets the lowest free number, which may be the one just closed.
- F4: `EINTR`: a signal during a flush makes `close` return `EINTR`, but the descriptor has already been released. `ERESTARTSYS` is converted to `EINTR` before returning to user space.
- F5: deferred write-back errors: `EIO`, `ENOSPC` and `EDQUOT` on NFS, FUSE and similar filesystems. The descriptor is still released.
- F6: POSIX record locks: the process's `fcntl` locks on the inode are released, through any descriptor. `flock` and open file description locks are released only with the last reference to their description.
- F7: pipes, FIFOs and sockets: closing the last write end makes readers see end of file. Closing the last read end makes writers get `EPIPE` and `SIGPIPE`. Closing a socket starts the protocol shutdown, which can send a FIN or an RST.
- F8: shared references: a description shared through `dup`, `fork` or `SCM_RIGHTS` stays open until its last reference closes. A closed duplicate does not affect the others.
- F9: unlinked files: an inode with no names left is freed when its last open description closes, and only then does the space return.
- F10: memory mappings: `mmap` regions of the file stay valid after `close`.
- F11: concurrent use: another thread blocked in `read`, `write` or `accept` on the same descriptor is not woken by `close` on Linux. Another thread can reuse the number as soon as `close` releases it.
- F12: close-on-exec interplay: descriptors with `FD_CLOEXEC` are closed by `execve` as if by `close`, with the same lock and last-reference effects.

## Determinism analysis

`linux_determinism: conditional`. For a single-threaded program on a local filesystem, `close` repeats: the same descriptor is released, the same number becomes free, and the result is 0 or `EBADF` by the same rules. It can vary in these ways:

- Write-back errors on network and user-space filesystems depend on remote and host state (F5).
- `EINTR` depends on when a signal arrives during a flush (F4).
- With threads sharing a descriptor table, which thread reuses the freed number depends on scheduling (F3, F11).
- Last-reference effects happen when the last holder closes, and that ordering between processes depends on scheduling. That decides when a peer sees end of file or `EPIPE`, and when a lock becomes free for a waiter (F6, F7, F8).

## Hermit design

Target policy: `model`. The kernel performs the close and owns the real descriptor table. Detcore mirrors the result so its own descriptor metadata, port bookkeeping and scheduler resources stay consistent. At hermit f4055eb9cae4, `handle_close` does the following:

- It runs the call through `record_or_replay` (`detcore/src/syscalls/files.rs:1144`). The kernel's result reaches the guest unchanged (`detcore/src/syscalls/files.rs:1152`), so F2, F4 and F5 come from the kernel.
- Unless the result is `EBADF` or `ERESTARTSYS`, it treats the descriptor as released (`detcore/src/syscalls/files.rs:1145`). That matches Linux, which releases the descriptor even on `EINTR` and write-back errors.
- It removes the descriptor from Detcore's table. When that was the last descriptor in the table that refers to the open file description, it also releases any network port Detcore assigned to it (`detcore/src/syscalls/files.rs:1147-1148`, `remove_fd` at `detcore/src/tool_local.rs:766-769`, `release_port_for_open_file` at `detcore/src/syscalls/files.rs:952`).
- It takes no scheduler resource. Threads blocked on a Detcore-managed pipe or socket are polling through Detcore's scheduler, not waiting in the kernel (`detcore/src/syscalls/helpers.rs:254-267`). After a peer closes, their next retry sees end of file or `EPIPE`, as on Linux.

With threads sequentialized, the order of `close` against other threads' descriptor allocations is fixed by Detcore's schedule, which removes the race in F3 and F11 for a fixed seed.

Why `model`: `passthrough` would leave Detcore's descriptor table, which drives the handling of later reads, writes and opens on the same number, stale after a close. `emulate` and `refuse` cannot work, because the kernel owns the real table.

Gaps against `model`, by reading the code:

- A descriptor that the guest received outside Detcore's knowledge (not opened through a handled call) is still closed by the kernel. `remove_fd` then finds nothing and nothing else happens, which is harmless.
- Hypothesis from reading the code, not measured: a thread polling a descriptor that another thread closes retries the same descriptor number (`detcore/src/syscalls/helpers.rs:254-267`), so it would get `EBADF` or reach whatever object reused the number. Linux keeps the blocked call on the original open file description (F11). T7 would show it.
- `execute_blocking_pipe_write` notices when a concurrent `close` or `dup2` replaced the descriptor and refuses the rest of the write with `EOPNOTSUPP` instead of continuing on the original pipe as Linux would (`detcore/src/syscalls/helpers.rs:533-546`). That is a deliberate fail-closed departure from F11.

## Recommended tests

- T1 (covers F1, F2, F3): close a descriptor and check 0, close it again and check `EBADF`, close -1 and a number above the limit and check `EBADF`, then open twice and check the lowest freed numbers are reused, under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/record_replay_fd_close.c` (cell `c-programs/record-replay-fd-close`, verify ci on ptrace) checks slot reuse after close at `tests/c/record_replay_fd_close.c:104-128`. `tests/c/simple/pread64_nostdlib.c` (cell `c-programs/pread64-nostdlib`, verify ci on ptrace) checks a raw `SYS_close` returns 0 at `tests/c/simple/pread64_nostdlib.c:51`.
- T2 (covers F4): close a FUSE or NFS descriptor while a signal arrives. This needs a filesystem whose flush can block, so it is likely a manual test, or replaced by a review of the release rule at `detcore/src/syscalls/files.rs:1145`.
- T3 (covers F5): on a filesystem that reports write-back errors at close (for example a FUSE mount that fails flush), check that `close` returns the error and that the number is free afterwards. Manual, like T2.
- T4 (covers F6): a parent takes a `fcntl` write lock, opens the same file a second time and closes that second descriptor, and a forked child then checks the lock with `F_GETLK` and finds it released. Repeat with `flock`, which must stay held until the last reference closes.
- T5 (covers F7, F8): a forked child holds a duplicate of a pipe's write end. The parent closes its own write end, and the reader sees end of file only after the child closes too. Close the read end and check that the writer gets `EPIPE` and `SIGPIPE`.
  Existing partial candidates: `tests/c/pipe_ipc.c` (cell `c-programs/pipe-ipc`) and `tests/c/sigpipe_siginfo.c` (cell `c-programs/sigpipe-siginfo`), both verify ci on ptrace.
- T6 (covers F9, F10): unlink an open file, map it, close the descriptor, and check that the mapping still reads the data and that `statfs` free space changes only after `munmap`.
- T7 (covers F11): one thread closes a descriptor while another thread opens files in a loop, and check that the numbers each thread gets are identical across verify runs. In a second case, one thread blocks reading an empty pipe while another closes that read descriptor and a writer then writes; compare the reader's result with native Linux, where the read completes on the original pipe.
- T8 (covers F12): mark one of two descriptors `FD_CLOEXEC`, `execve` a helper, and check that only the other remains open.
  Existing candidate: `tests/c/record_replay_fd_close.c` checks close-on-exec descriptors across `execve`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_close runs close in the kernel, treats every result but EBADF and ERESTARTSYS as a release, and mirrors it in the descriptor table and port bookkeeping); linux_determinism stays conditional (single-threaded local-file closes repeat; write-back errors, EINTR and descriptor-reuse races do not); 12 flag behaviours and 8 tests listed; tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T8 covered by tests/c/record_replay_fd_close.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `close`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+close&body=Discussion+for+the+%60close%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclose.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60close%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `close`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
