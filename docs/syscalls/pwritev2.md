---
schema: syscall-pages/v1
syscall: pwritev2
nr_x86_64: 328
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/389
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pwritev2&body=Discussion+for+the+%60pwritev2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpwritev2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pwritev2%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`pwritev2(fd, iov, iovcnt, pos_l, pos_h, flags)` is `pwritev` with two additions: offset -1 means "write at the current file offset and advance it", as `writev` does, and a per-call `RWF_*` flags word. Detcore has a typed handler (`detcore/src/syscalls/files.rs:2832`) that:

- serves writes to `/proc/self/timerslack_ns` at offset -1 from virtual state, and refuses explicit offsets there as Linux does
- takes a write lock on the file's resource and sends everything else, flags unchanged, to the kernel
- advances the file's virtual mtime after a successful write when metadata is virtualized

## Linux semantics

Added in Linux 4.6. The syscall entry splits on the offset:

1. Offset -1 takes the `writev` path: the descriptor is looked up and the write uses and advances the file position (at end of file for `O_APPEND`). Pipes, FIFOs, sockets and terminals are allowed, and a write on a blocking descriptor without room waits.
2. Any other negative offset gives `EINVAL`, before the descriptor is looked up.
3. Otherwise it takes the `pwritev` path: the descriptor is looked up, a file without positional writes gives `ESPIPE`, and the file position is neither used nor changed.

Both paths then check that the file was opened for writing (`EBADF`), import the iovec array, return 0 for an empty request, check the offset range and file-size limit, and only then validate the flags: unknown bits give `EOPNOTSUPP`; `RWF_APPEND` with `RWF_NOAPPEND` gives `EINVAL`; `RWF_NOWAIT` on a file without non-blocking support gives `EOPNOTSUPP`; `RWF_ATOMIC` without atomic-write support gives `EOPNOTSUPP`; `RWF_DONTCACHE` without filesystem support gives `EOPNOTSUPP`. A file whose operations lack `write_iter` accepts only `RWF_HIPRI`. The 2026-07-24 host strace under Additional information shows unknown flag bits `0xf5a49300` giving `EOPNOTSUPP`.

### Flags and options

- F1: offset -1: write at the current file position and advance it, as `writev` does. Works on pipes, FIFOs, sockets and terminals. A blocking pipe write waits until the whole request fits (writes of up to `PIPE_BUF` bytes are atomic); an `O_NONBLOCK` one gives `EAGAIN` or a short count.
- F2: explicit offset (0 or more): writes start at `offset`, each buffer in turn, and the file position is neither used nor changed.
- F3: offset encoding on x86_64: the 64-bit offset is `pos_l`, and `pos_h` is ignored.
- F4: offset below -1: `EINVAL`, checked before the descriptor lookup. So `pwritev2(-1, ..., -2)` gives `EINVAL`, while `pwritev2(-1, ..., -1)` and `pwritev2(-1, ..., 0)` give `EBADF`.
- F5: descriptor not open, opened with `O_PATH`, or not opened for writing: `EBADF`, also with `iovcnt == 0`.
- F6: the iovec array: `iovcnt` is truncated to 32 bits; an empty request returns 0 before the flags are checked; above `IOV_MAX` (1024) gives `EINVAL`; an unreadable array or first buffer gives `EFAULT`; a fault after some bytes were written returns the short count.
- F7: explicit offset on a descriptor without positional writes (pipe, FIFO, socket, terminal): `ESPIPE`.
- F8: explicit offset on an `O_APPEND` descriptor: Linux appends at end of file and ignores the offset, unless `RWF_NOAPPEND` (F13) is given.
- F9: unknown flag bits: `EOPNOTSUPP`. This wins over the `RWF_APPEND`/`RWF_NOAPPEND` conflict.
- F10: `RWF_HIPRI` (0x1): polled completion for `O_DIRECT` writes on devices that support polling; otherwise no effect. The only flag a file without `write_iter` accepts.
- F11: `RWF_DSYNC` (0x2) and `RWF_SYNC` (0x4): this write behaves as if the descriptor had `O_DSYNC` or `O_SYNC`: it returns after the data (and, for `RWF_SYNC`, the metadata) reach stable storage.
- F12: `RWF_NOWAIT` (0x8): give `EAGAIN` instead of blocking, for example on a full pipe or when a buffered write would need to allocate blocks or wait for a lock; a short count is possible. `EOPNOTSUPP` on a file without non-blocking support.
- F13: `RWF_APPEND` (0x10) and `RWF_NOAPPEND` (0x20): `RWF_APPEND` writes at end of file for this call, whatever the offset. `RWF_NOAPPEND` makes an explicit-offset write on an `O_APPEND` descriptor honour the offset, and gives `EPERM` on an append-only inode. Both together give `EINVAL`.
- F14: `RWF_ATOMIC` (0x40): the write is not torn by a crash. It needs a file with atomic-write support (typically `O_DIRECT` on a supporting filesystem or block device), else `EOPNOTSUPP`; a length or alignment outside the limits `statx` reports gives `EINVAL`.
- F15: `RWF_DONTCACHE` (0x80): buffered write whose pages are dropped after writeback, where the filesystem supports it; `EOPNOTSUPP` otherwise and on DAX files.
- F16: `RWF_NOSIGNAL` (0x100): Hermit's comment at `detcore/src/syscalls/files.rs:165` records that Linux 7.1 accepts it. By its name it suppresses `SIGPIPE` for a write to a pipe or socket whose reader is gone, still returning `EPIPE`; this body did not confirm the write-side behaviour.
- F17: broken pipe or socket at offset -1: `EPIPE`, and `SIGPIPE` is sent to the calling thread unless F16 applies.
- F18: file growth and limits: a write past end of file extends it with a zero-filled gap; a non-empty request with `offset + length` past `INT64_MAX` gives `EINVAL`; with `RLIMIT_FSIZE`, a write starting at the limit gives `EFBIG` (and `SIGXFSZ`) and one crossing it returns the short count.
- F19: other failures from the filesystem: `ENOSPC`, `EDQUOT`, `EPERM` on a sealed memfd, `ETXTBSY`.
- F20: metadata: a write of more than 0 bytes updates `mtime` and `ctime`, and clears set-user-ID and set-group-ID bits for a caller without `CAP_FSETID`.
- F21: `/proc/self/timerslack_ns` and `/proc/<pid>/task/<tid>/timerslack_ns`: at offset -1 a decimal value sets the thread's timer slack; an explicit offset gives `ESPIPE`.
- F22: `O_DIRECT` descriptors: the filesystem requires an aligned buffer, length and offset, and gives `EINVAL` otherwise.

## Determinism analysis

`linux_determinism: conditional`. For a regular file at an explicit offset, the effect and result depend only on the arguments, the file's state and free space, so the call repeats when nothing outside the guest touches the file. With offset -1 on a pipe or socket, a non-blocking write's count depends on how much the reader had drained, which depends on scheduling. `RWF_NOWAIT` makes even regular-file writes depend on host locks and allocation state. Timestamps (F20) are host clock values. Flag support (F12, F14, F15) depends on the filesystem.

### Nondeterminism sources

- Processes outside the guest writing the same file
- Pipe and socket writes at offset -1: short counts and `EAGAIN` depend on reader timing (F1)
- `RWF_NOWAIT`: `EAGAIN` or short counts depending on host state (F12)
- Host clock in `mtime` and `ctime` (F20)
- Host free space and quotas (F19)
- Filesystem-dependent flag support (F12, F14, F15, F22)

## Hermit design

Target policy: `model`. The kernel performs the write, and Detcore orders it against other access to the same file and owns the timestamps the guest sees. What the handler at f4055eb9cae4 does, in order:

- A descriptor bound to timer slack: an offset below -1 gives `EINVAL` and an explicit offset gives `ESPIPE` (`detcore/src/syscalls/files.rs:2837` to `detcore/src/syscalls/files.rs:2844`). At offset -1 it checks write access and calls `writev_timer_slack` with the flags (`detcore/src/syscalls/files.rs:2845`), which gives `EOPNOTSUPP` for any flag but `RWF_HIPRI` (`detcore/src/syscalls/files.rs:732`). That gives F21 from virtual state. The offset is decoded by `vectored_offset`, which on 64-bit uses `pos_l` alone (`detcore/src/syscalls/files.rs:137`), as F3 requires.
- The handler picks a resource: the descriptor's own resource, or else `ResourceID::FileContents` of the determinized inode (`detcore/src/syscalls/files.rs:2860`). It takes a write permit on it (`detcore/src/syscalls/files.rs:2870`), so guest readers that take the read permit, such as `preadv2`, are ordered with the write.
- The write goes to the kernel through `record_or_replay_preserving_tool_errors` (`detcore/src/syscalls/files.rs:2875`), with the flags unchanged, as the doc comment says (`detcore/src/syscalls/files.rs:2830`). That gives Linux's own F2, F5 to F19 and F22, and a kernel-sent `SIGPIPE` for F17.
- With `virtualize_metadata` on, a write that returns more than 0 calls `touch_file` (`detcore/src/syscalls/files.rs:2879`), which asks the global state to advance the file's virtual mtime (`detcore/src/tool_global.rs:3763`). That gives F20's timestamps in virtual time.

The handler was added by https://github.com/rrnewton/hermit/pull/794, and the code tag `TODO-HUMAN-REVIEW(#794)` at `detcore/src/syscalls/files.rs:2829` refers to it.

Why `model` and not the alternatives: `passthrough` would expose real `mtime` values, skip the resource ordering and write the real timer slack. `emulate` would duplicate the filesystem. `refuse` would break programs that write through glibc's `pwritev2` wrapper, including its `RWF_APPEND` form.

What remains before the code fully realizes `model`:

- **Error precedence for a bad descriptor with an offset below -1 (F4).** The first thing `handle_pwritev2` does is `timer_slack_binding`, which looks the descriptor up with `with_detfd` (`detcore/src/syscalls/files.rs:2837` and `detcore/src/syscalls/files.rs:517`); Detcore returns `EBADF` for a descriptor missing from its table (`detcore/src/tool_local.rs:740`). The offset check comes only inside the timer-slack branch. By this reading of the code, `pwritev2(-1, iov, 1, -2)` gives `EBADF` under Hermit where Linux gives `EINVAL`. `handle_preadv2` checks the offset first (`detcore/src/syscalls/files.rs:2720`). Not measured; T3 measures it.
- **Offset -1 on pipes and sockets (F1).** `writev` sends a write on a physically non-blocking, logically blocking pipe through `execute_blocking_pipe_writev`, and one on a physically non-blocking socket, pipe or eventfd through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:2486` and `detcore/src/syscalls/files.rs:2490`). `handle_pipe2` makes every pipe physically non-blocking whenever `use_nonblocking_sockets()` holds, record and replay included (`detcore/src/syscalls/files.rs:3584`). `handle_pwritev2` has neither branch. By this reading of the code, a large `pwritev2(pipe_write_end, ..., -1, 0)` into a nearly full, logically blocking pipe returns a short count or `EAGAIN` under Hermit where Linux waits until the whole write is done. Not measured; T1 measures it.
- **Target (owner decisions 6, 14):** decision 6 puts the offset check first, in kernel order: the `pwritev2` entry point sends `pos == -1` to `do_writev` and every other offset to `do_pwritev`, which gives `EINVAL` for `pos < 0` before `fdget` looks the descriptor up, so `handle_pwritev2` should test `offset < -1` before `timer_slack_binding` (`detcore/src/syscalls/files.rs:2837`), as `handle_preadv2` already does (`detcore/src/syscalls/files.rs:2720`); T3 checks it. Decision 14 sets offset -1 on a logically blocking pipe or socket to a non-blocking probe plus a scheduler wait: take the `execute_blocking_pipe_writev` and `execute_nonblockable_fd_syscall` branches that `writev` takes (`detcore/src/syscalls/files.rs:2486` and `detcore/src/syscalls/files.rs:2490`), which needs a `NonblockableSyscall` implementation for `Pwritev2` alongside the one for `Writev` (`detcore/src/syscalls/helpers.rs:1230`). Today every write goes through `record_or_replay_preserving_tool_errors` (`detcore/src/syscalls/files.rs:2875`). The fix is not `refuse`: glibc's `pwritev2` wrapper reaches this path.
- **`RWF_NOWAIT` (F12).** Forwarded unchanged, so the result can depend on host state. Owner decision (see Open questions).
- **Mode bits after a write (F20).** `touch_file` advances only the virtual mtime. This body did not find whether the virtual metadata reflects the kernel clearing set-user-ID and set-group-ID bits. T10 checks it.

`linux_determinism` stays `conditional`. Hermit removes the timestamp source and orders writes within the guest. Writers outside the guest, host free space, `RWF_NOWAIT` and, until the pipe gap above is closed, pipe timing at offset -1 remain.

## Recommended tests

- T1 (covers F1): offset -1 on a regular file writes at and advances the file position, and on an `O_APPEND` file writes at end of file. On a pipe, a 128 KiB write at offset -1 with a reader thread draining slowly returns 131072 in every `--verify` run; an `O_NONBLOCK` full pipe gives `EAGAIN`.
- T2 (covers F2, F3): `pwritev2` at explicit offsets writes the expected bytes and leaves `lseek(fd, 0, SEEK_CUR)` unchanged; a raw `syscall(SYS_pwritev2, ...)` with garbage in `pos_h` writes at the same place.
  Existing partial coverage: `tests/c/preadv2_flags.c:65` and `tests/c/preadv2_flags.c:69` write 6 bytes at offsets 0 and 6 and read them back (cell `c-programs/preadv2-flags`, `tests/e2e/manifests/c-programs.yaml:13212`, verify ci on ptrace). It does not check the file position or `pos_h`.
- T3 (covers F4, F5, F6): check:
  - `pwritev2(-1, iov, 1, -2)` gives `EINVAL` (Hermit is expected to give `EBADF` today); offset -1 and 0 give `EBADF`
  - `O_RDONLY` and `O_PATH` descriptors give `EBADF`, also with `iovcnt == 0`
  - `iovcnt` 0, `1UL << 32` and `(1UL << 32) | 1` write 0, 0 and one buffer; 1025 and `~0UL` give `EINVAL`; an invalid array gives `EFAULT`; a valid first buffer followed by an invalid second one returns the first buffer's length
- T4 (covers F7): `pwritev2` at offset 0 on a pipe, a FIFO, an `AF_UNIX` socketpair and a pseudo-terminal gives `ESPIPE`, and nothing reaches the reader.
- T5 (covers F8, F13): on an `O_APPEND` file, `pwritev2` at offset 0 with flags 0 appends; with `RWF_NOAPPEND` it overwrites offset 0. On a plain file, `RWF_APPEND` at offset 0 appends. `RWF_APPEND|RWF_NOAPPEND` gives `EINVAL`.
  Existing partial coverage: `tests/c/preadv2_flags.c:88` appends with `RWF_APPEND` at offset -1 and reads it back at offset 12 (same cell as T2).
- T6 (covers F9, F10, F11, F16): the flag-policy table on a regular file: unknown bits give `EOPNOTSUPP` (also with `RWF_APPEND|RWF_NOAPPEND`); `RWF_HIPRI`, `RWF_DSYNC`, `RWF_SYNC` and `RWF_NOSIGNAL` write the same bytes as flags 0; every flag with `iovcnt == 0` returns 0.
- T7 (covers F12, F14, F15): `RWF_NOWAIT` on a full non-blocking pipe gives `EAGAIN`; `RWF_ATOMIC` on a buffered descriptor gives `EOPNOTSUPP`; `RWF_DONTCACHE` succeeds or gives `EOPNOTSUPP` according to the filesystem. Record the filesystem and require the same result in every `--verify` run.
- T8 (covers F16, F17): `pwritev2` at offset -1 to a pipe whose read end is closed gives `EPIPE` and delivers `SIGPIPE` with the expected `siginfo`; with `RWF_NOSIGNAL` on a host that supports it, `EPIPE` without the signal.
  Existing candidate for the flags-0 case: `tests/c/sigpipe_siginfo.c:105` (cell `c-programs/sigpipe-siginfo`, `tests/e2e/manifests/c-programs.yaml:8207`, verify ci on ptrace).
- T9 (covers F18, F19): a write at `size + 4096` extends the file with zeros; a 1-byte write at `INT64_MAX` gives `EINVAL`; with `SIGXFSZ` ignored and `RLIMIT_FSIZE` set, a write at the limit gives `EFBIG` and one crossing it returns the short count; on a memfd sealed with `F_SEAL_WRITE`, `EPERM`.
- T10 (covers F20): after `pwritev2`, `stat` shows later `mtime` and `ctime` with the same values in every `--verify` run; a zero-length write leaves them unchanged; for a guest without `CAP_FSETID`, the set-user-ID bit is cleared and Hermit's `stat` agrees.
- T11 (covers F21): `pwritev2` of a decimal value to `/proc/self/timerslack_ns` at offset -1 with `RWF_HIPRI` sets the slack seen by `PR_GET_TIMERSLACK`; offset 0 gives `ESPIPE`; offset -2 gives `EINVAL`.
  Existing candidate: `timer_slack_procfs_vector_io_matches_linux` at `detcore/tests/misc/mod.rs:434` (the `pwritev2` check is at lines 548 to 559).
- T12 (covers F22): on a filesystem that supports `O_DIRECT`, an unaligned buffer gives `EINVAL`. Record the filesystem in the output.
- T13 (covers F1, F2, F20): record a guest that writes a file and a pipe with `pwritev2` and reads them back, and replay it. Replay must give the same output and the same virtual `mtime`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/389: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel performs the write under a W permit with flags forwarded; virtual mtime via touch_file; timerslack_ns served at offset -1 and refused at explicit offsets like Linux); linux_determinism stays conditional; 22 flag behaviours and 13 tests listed; open questions: by code reading pwritev2(-1, iov, 1, -2) gives EBADF where Linux gives EINVAL, offset -1 on a Hermit-nonblocking pipe skips the blocking-pipe path, RWF_NOWAIT policy; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decisions 6 (offset check before descriptor lookup, kernel do_pwritev order) and 14 (offset -1 pipe/socket write becomes probe plus scheduler wait); target_policy unchanged; no new F/T ids; removed the offset -1 pipe-path and offset-check-order questions.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pwritev2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pwritev2&body=Discussion+for+the+%60pwritev2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpwritev2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pwritev2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pwritev2" (https://github.com/rrnewton/hermit/issues/389), legacy variant A1.

### Open questions

- `RWF_NOWAIT`: keep forwarding it, mask it, or fail it closed under `--strict`?

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=pwritev2` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `pwritev2` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
pwritev2(4, [{iov_base="vec", iov_len=3}], 1, 0, 0xf5a49300 /* RWF_??? */) = -1 EOPNOTSUPP (Operation not supported)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
