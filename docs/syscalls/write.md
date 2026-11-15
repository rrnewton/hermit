---
schema: syscall-pages/v1
syscall: write
nr_x86_64: 1
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+write&body=Discussion+for+the+%60write%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwrite.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60write%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 13
---

## Summary

`write(fd, buf, count)` copies up to `count` bytes from guest memory to the object behind `fd`. Detcore handles it with a typed handler (`detcore/src/lib.rs:2358`, `handle_write` at `detcore/src/syscalls/files.rs:2226` at hermit f4055eb9cae4). The handler gives the written file a logical modification time, completes logically blocking writes to Detcore-managed pipes inside the scheduler, and repeats regular-file writes until the full count is written.

Manual reference: Linux man-pages write(2).

## Linux semantics

`write` writes at the current offset of the open file description and advances that offset by the count returned. With `O_APPEND`, each write first moves the offset to end of file, as one atomic step. Copies made by `dup` or `fork` share one offset. A successful write to a regular file sets the inode's mtime and ctime from the kernel's wall clock. Linux clamps `count` to `MAX_RW_COUNT` (`0x7ffff000`).

On a regular file a short count happens only when space runs out, when the file size limit is reached, or when a signal arrives after some bytes were copied. On a pipe, a write of at most `PIPE_BUF` (4096) bytes is atomic. A larger write may be interleaved with other writers, and it waits until a reader drains the pipe. Sockets behave similarly, using send-buffer space. With `O_NONBLOCK`, a write that cannot make progress fails with `EAGAIN`. A write that made partial progress returns the partial count. A write to a pipe or stream socket with no reader fails with `EPIPE` and sends `SIGPIPE` to the thread.

Checks run in this order: the descriptor must be open and writable (`EBADF`, which includes `O_PATH` and read-only descriptors). Then the buffer must be in the user address range (`EFAULT`). A fault part-way through the buffer gives a short count when some bytes were already written.

### Flags and options

- F1: regular file: writes at the file offset, extends the file past end of file, and advances the offset by the count returned.
- F2: `O_APPEND` on the open file description: each write moves the offset to end of file first, atomically with the write.
- F3: `count == 0`: on a regular file it returns 0 and changes nothing. It still reports `EBADF` for a descriptor that is not writable. On other file types the result is unspecified by POSIX; on Linux a zero-length pipe write returns 0.
- F4: pipe or FIFO: a write of at most `PIPE_BUF` (4096) bytes is atomic. A larger write may be split and interleaved with other writers. A blocking write waits until it has written every byte. With `O_NONBLOCK`, a full pipe gives `EAGAIN`, and a larger write may return a short count.
- F5: no reader: a write to a pipe with no read end open, or to a stream socket shut down for writing, fails with `EPIPE` and sends `SIGPIPE`. The default action of `SIGPIPE` kills the process.
- F6: socket: a stream socket waits for send-buffer space and may return a short count with `O_NONBLOCK`. A datagram socket sends one datagram, and one larger than the protocol limit gives `EMSGSIZE`. An unconnected datagram socket gives `EDESTADDRREQ`.
- F7: eventfd: adds an 8-byte value to the counter. `count < 8` or the value `0xffffffffffffffff` gives `EINVAL`. A write that would overflow the counter waits, or gives `EAGAIN` with `O_NONBLOCK`.
- F8: errors: `EBADF` for a descriptor that is not open, is read-only, or is `O_PATH`. `EFAULT` for an unreadable buffer, `EFBIG` past `RLIMIT_FSIZE` (which also sends `SIGXFSZ`), and `EINVAL` for an object that cannot be written or a misaligned `O_DIRECT` request. F14 and F15 hold the host-dependent errors.
- F9: signal interruption: `EINTR` if a handler runs before any byte is written, or the call restarts if the handler uses `SA_RESTART`. A positive short count if bytes were already written.
- F10: metadata: a successful write that changes the file sets mtime and ctime. It also clears the set-user-ID bit, and the set-group-ID bit when group execute is set, unless the writer has `CAP_FSETID`.
- F11: `count` above `MAX_RW_COUNT` (`0x7ffff000`) is clamped, so the call returns at most that many bytes.
- F12: terminals and character devices: a terminal applies output processing and may wait under flow control. `/dev/null` accepts every byte. The random devices mix the bytes into the entropy pool.
- F13: shared offset: `dup` and `fork` copies share one offset, so concurrent writers on the same description write disjoint regions. Which writer lands where depends on scheduling.
- F14: host-transient errors: `ENOSPC` or `EDQUOT` when the host filesystem's space or a host quota runs out, and `EIO` for a device error.
- F15: `EFBIG` for a write past the maximum file size of the host filesystem behind the file.

## Determinism analysis

`linux_determinism: conditional`. A single writer to a regular file with enough space repeats: same count, same offset, same file contents. The time stamps it sets do not repeat, and every other case can vary:

- File time stamps come from the wall clock (F10).
- Pipes and sockets: how many bytes a nonblocking write accepts, and how writes larger than `PIPE_BUF` interleave, depend on when the reader and other writers ran (F4, F6, F13).
- Whether a reader still exists when a write runs decides between success and `EPIPE` with `SIGPIPE` (F5).
- Free space and quotas are host state (F14: `ENOSPC`, `EDQUOT`).
- Signals arriving at different points decide between `EINTR`, a restart and a short count (F9).
- Output to a terminal or network peer leaves the container.

The bytes a program writes are determined by its inputs. A program whose writes reach only its own files and its own readers repeats except for time stamps. Otherwise it repeats only if Hermit removes these sources.

### Cross-host variation

- Filesystem type behind the guest's file: its maximum file size decides where `EFBIG` starts (F15).
- Device logical block size and filesystem `O_DIRECT` support: which alignments an `O_DIRECT` write accepts and which give `EINVAL` (F8).

## Hermit design

Target policy: `model`. The kernel performs the write and keeps the file contents. Detcore controls when writes run, how blocking writes complete, and which time stamp the file shows. At hermit f4055eb9cae4, `handle_write` does the following:

- A descriptor bound to a task's `timerslack_ns` procfs file is handled entirely by Detcore (`detcore/src/syscalls/files.rs:2230-2234`, `write_timer_slack` at `detcore/src/syscalls/files.rs:698`).
- It reads the descriptor's type, blocking state and resource from Detcore's descriptor table (`detcore/src/syscalls/files.rs:2236-2252`). A descriptor Detcore does not track fails with `EBADF` from that table (`detcore/src/tool_local.rs:740`) before any kernel call.
- With metadata virtualization on (the default; `detcore-model/src/config.rs:247-253`), `touch_file` sets the inode's logical mtime from the global logical clock before the write runs (`detcore/src/syscalls/files.rs:2254-2259`, `touch_file` at `detcore/src/tool_global.rs:3763`, `recv_touch_file` at `detcore/src/tool_global.rs:2594`). When time virtualization is off, that mtime comes from the real clock instead (`detcore/src/tool_global.rs:2598-2605`). Stat calls later report this mtime (see the stat body). That covers F10's time-stamp source.
- It requests the descriptor's resource from the scheduler with write permission (`detcore/src/syscalls/files.rs:2261-2272`).
- A logically blocking write to a Detcore-managed pipe (physically nonblocking) goes to `execute_blocking_pipe_write` (`detcore/src/syscalls/files.rs:2280-2282`, helper at `detcore/src/syscalls/helpers.rs:496`). The helper clamps the count to `MAX_RW_COUNT` and retries the unwritten suffix while the scheduler runs other threads, until the write completes, a signal arrives, or an error occurs. A signal or error after partial progress returns the partial count, as Linux does. If another thread replaced the descriptor number in the meantime, the helper refuses the call with `EOPNOTSUPP` rather than write into a different object (`detcore/src/syscalls/helpers.rs:533-546`). That covers F4 and F9 for these pipes.
- Other physically nonblocking sockets, pipes and eventfds go through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:2283-2286`, helper at `detcore/src/syscalls/helpers.rs:202`).
- Any other write, with `deterministic_io` on, is repeated until the full count is written (`detcore/src/syscalls/files.rs:2287-2324`). An error after partial progress returns the partial count (`finish_partial_record_or_replay_write`, `detcore/src/tool_local.rs:413`). `hermit run` turns `deterministic_io` on unless `--no-deterministic-io` is passed (`hermit-cli/src/bin/hermit/run.rs:3668`). Record and replay turn both `deterministic_io` and metadata virtualization off (`hermit-cli/src/metadata.rs:404` and `hermit-cli/src/metadata.rs:406`). In that case a single write is recorded and replayed as the kernel returned it (`detcore/src/syscalls/files.rs:2325-2327`).

On ptrace, inherited standard descriptors are typed `Regular` (doc comment at `detcore/src/tool_local.rs:2570-2576`), so a write to a host stdout pipe takes the repeat-until-done path and blocks in the kernel, not in the scheduler.

Why `model`: `passthrough` would leave pipe write splits, wall-clock time stamps and blocking order to the host. `emulate` cannot work, because the kernel holds file contents and pipe buffers. `refuse` is impossible for a syscall every program uses.

## Recommended tests

- T1 (covers F1, F11, F13): write known blocks to a file through a descriptor and a `dup` copy, then read them back. Check every return value, the shared offset and the final contents under `hermit run --verify` on ptrace. Also check that `count = 0x80000000` from a mapping that large returns `0x7ffff000`.
  Existing candidates: `tests/c/file_io_roundtrip.c` (cell `c-programs/file-io-roundtrip`) and `tests/c/dup_shared_offset.c` (cell `c-programs/dup-shared-offset`), both verify ci on ptrace. Neither checks the clamp.
- T2 (covers F2): open a file with `O_APPEND` from a parent and a forked child, write from both, and check that no write overwrites another.
- T3 (covers F3, F8): `write(fd, buf, 0)` on a regular file returns 0 and leaves size and contents unchanged. A write on a read-only descriptor gives `EBADF`, from an unmapped buffer gives `EFAULT`, past `RLIMIT_FSIZE` gives `EFBIG` with `SIGXFSZ` ignored, and to a directory descriptor opened `O_PATH` gives `EBADF`.
  Existing partial candidate: `tests/c/openat_flags.c` (cell `c-programs/openat-flags`, verify ci on ptrace) checks `EBADF` for a write on an `O_RDONLY` descriptor.
- T4 (covers F4, F9): several forked writers write to one pipe with writes below and above `PIPE_BUF`. A reader checks that each small write arrives whole, and the run prints the read split, which must be identical across verify runs. Also check that a blocking pipe write interrupted by a signal handler returns `EINTR` before any progress, or the partial count after some progress.
  Existing candidates: `tests/c/pipe_multiwriter_ordering.c` (cell `c-programs/pipe-multiwriter-ordering`), `tests/c/short_io_split_identity.c` (cell `c-programs/short-io-split-identity`) and `tests/c/pipe_ipc.c` (cell `c-programs/pipe-ipc`), all verify ci on ptrace. None checks `EINTR`.
- T5 (covers F5): write to a pipe after closing its read end. Check `EPIPE`, and that `SIGPIPE` reaches a handler with the same siginfo in both runs.
  Existing candidate: `tests/c/sigpipe_siginfo.c` (cell `c-programs/sigpipe-siginfo`, verify ci on ptrace).
- T6 (covers F6, F7): write to a `socketpair` stream until `O_NONBLOCK` gives `EAGAIN`, and check the count accepted is identical across runs. Write to an eventfd with `count < 8` (`EINVAL`) and with a value that overflows the counter under `O_NONBLOCK` (`EAGAIN`).
  Existing partial candidates: `tests/c/shutdown_socketpair.c` (cell `c-programs/shutdown-socketpair`) and `tests/c/eventfd_probe.c` (cell `c-programs/eventfd-semantics`), both verify ci on ptrace.
- T7 (covers F12): raw `SYS_write` to stdout from a program with no libc, and two forked processes writing to stdout. The output, including its order, must be identical across runs.
  Existing candidates: `tests/c/simple/hello_nostdlib.c` (cell `c-programs/hello-nostdlib`) and `tests/c/simple/racewrite_nostdlib.c` (cell `c-programs/racewrite-nostdlib`), both verify ci on ptrace.
- T8 (covers F10): `stat` a file, then do a successful write, a failed write and a zero-length write, and `stat` after each. Check that mtime and ctime change only after the successful write, as on Linux, and that the set-user-ID bit is cleared. This also settles Q1.
  Existing partial candidate: `tests/c/file_timestamp_identity.c` (cell `system-utils/file-timestamp-identity`, verify ci on ptrace).
- T9 (covers F1, F4): record and replay a guest that writes a file and a pipe. Replay must reproduce every return value.
- U1 (covers F14; host-transient): `ENOSPC`, `EDQUOT` and `EIO` depend on the host filesystem's free space, quotas and storage health, shared with other processes. The write reaches the kernel through `record_or_replay_preserving_tool_errors` (`detcore/src/syscalls/files.rs:2298`), and these errors reach the guest today. F14 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F15; host-config): the maximum file size belongs to the host filesystem, which a container guest cannot choose, so CI cannot produce this `EFBIG` for every filesystem. Hermit forwards the kernel's answer unchanged (`detcore/src/syscalls/files.rs:2298`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_write sets a logical mtime, completes blocking writes to Detcore pipes in the scheduler, and repeats other writes to the full count under deterministic_io); linux_determinism stays conditional (single-writer regular files repeat except time stamps, pipes, sockets and EPIPE timing do not); 13 flag behaviours and 9 tests listed; open question Q1: touch_file bumps mtime before the write, even when it fails; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem maximum file size, O_DIRECT block size); F8 narrowed, new F14 ENOSPC/EDQUOT/EIO (host-transient U1) and F15 EFBIG past the filesystem maximum (host-config U2); no open question is answered by these decisions
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T5 covered by tests/c/sigpipe_siginfo.c, T7 covered by tests/c/simple/racewrite_nostdlib.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `write`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+write&body=Discussion+for+the+%60write%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwrite.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60write%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `write`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Q1 (hypothesis from reading the code, not measured): `touch_file` runs before the write and whether or not the write succeeds (`detcore/src/syscalls/files.rs:2254-2259`). Linux changes mtime only on a successful write that transferred data. A failed write (`EBADF` on a read-only descriptor, `ENOSPC`) or a zero-length write would therefore change the mtime Hermit reports, where Linux would not. This is deterministic, so it does not break repeatability, but it departs from Linux. A test such as T8 would settle it.
