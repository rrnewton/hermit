---
schema: syscall-pages/v1
syscall: pwritev
nr_x86_64: 296
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/388
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pwritev&body=Discussion+for+the+%60pwritev%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpwritev.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pwritev%60%2C+or+in+what+the+page+says%3F%0A
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

`pwritev(fd, iov, iovcnt, pos_l, pos_h)` writes several buffers at an explicit file offset, without using or moving the file offset. Detcore has a typed handler (`detcore/src/syscalls/files.rs:2779`) that:

- refuses `/proc/self/timerslack_ns` the way Linux does
- takes a write permit on the file's resource
- sends the write to the kernel
- bumps the file's virtual mtime after a successful non-empty write

## Linux semantics

`do_pwritev` checks the arguments in this order:

1. A negative offset gives `EINVAL`, before the descriptor is looked up.
2. The descriptor is looked up. A missing descriptor or an `O_PATH` one gives `EBADF`.
3. A file without positional-write support gives `ESPIPE`. That covers pipes, FIFOs, sockets, terminals and `seq_file` procfs files.
4. `vfs_writev` checks that the file was opened for writing (`EBADF` if not), imports the iovec array, and checks the range.

The write takes the buffers in order. It can extend the file and leave a hole, it updates `mtime` and `ctime`, and it returns the number of bytes written. On x86_64 the whole 64-bit offset is `pos_l`, and `pos_h` is ignored. The 2026-07-24 host strace under Additional information shows a 3-byte write to a `/tmp` file returning 3.

### Flags and options

- F1: explicit offset: the buffers are written in order starting at `offset`. The file offset is neither used nor changed.
- F2: offset encoding on x86_64: the 64-bit offset is `pos_l`, and `pos_h` is ignored
- F3: negative offset: `EINVAL`. This is checked before the descriptor lookup, so `pwritev(-1, ..., -1)` gives `EINVAL`, not `EBADF`.
- F4: descriptor not open, or opened with `O_PATH`: `EBADF`
- F5: descriptor not opened for writing (`O_RDONLY`): `EBADF`
- F6: the iovec array:
  - `iovcnt` is truncated to 32 bits.
  - `iovcnt == 0` returns 0 and writes nothing.
  - `iovcnt` above `IOV_MAX` (1024) gives `EINVAL`.
  - An unreadable array or first buffer gives `EFAULT`.
  - A fault in a later buffer, after some bytes were written, returns the short count.
- F7: descriptor that does not support positional writes (pipe, FIFO, socket, terminal): `ESPIPE`
- F8: `O_APPEND`: on Linux the data is appended at end of file whatever `offset` says, as pwrite(2) documents under BUGS. A per-call `RWF_APPEND` exists only in `pwritev2`.
- F9: growth and overflow:
  - Writing past end of file extends it, and the gap reads as zeros.
  - A non-empty request with `offset + length` past `INT64_MAX` gives `EINVAL`.
  - F16 holds `EFBIG` past the filesystem's maximum file size.
- F10: `RLIMIT_FSIZE`: a write that starts at or past the limit raises `SIGXFSZ` and fails with `EFBIG`. A write that crosses the limit is cut short at it.
- F11: refusals from seals: `EPERM` on a memfd sealed with `F_SEAL_WRITE` or `F_SEAL_FUTURE_WRITE`, or for growth past the size on one sealed with `F_SEAL_GROW`. F17 and F18 hold the storage refusals.
- F12: side effects of a successful write: `mtime` and `ctime` are updated. Set-user-ID and set-group-ID bits are cleared when the writer lacks `CAP_FSETID`.
- F13: `/proc/self/timerslack_ns` and the other `seq_file` procfs files: `ESPIPE`, or `EINVAL` for a negative offset
- F14: `O_DIRECT` descriptors: the filesystem requires an aligned buffer, length and offset, and gives `EINVAL` otherwise
- F15: a signal during a slow write can make it return a short count or `EINTR`. F19 holds FUSE and network filesystems.
- F16: a write past the maximum file size of the host filesystem behind the guest's file: `EFBIG`.
- F17: `ENOSPC` when the host filesystem is full, `EDQUOT` when a host quota is exceeded.
- F18: `ETXTBSY` on a file the host has activated as swap.
- F19: on FUSE and network filesystems, a write can return a short count or an error that depends on the server and the network.

## Determinism analysis

`linux_determinism: conditional`. For a regular file the count returned and the resulting contents depend only on the arguments, the file's prior contents and size, and the guest's own limits, so the call repeats when nothing outside the guest touches the file. It stops repeating when processes outside the guest write the same file, when the host filesystem runs short of space or quota, or where a slow filesystem returns timing-dependent short counts. Pipes and sockets are not a source here: they give `ESPIPE` (F7), so the earlier wording that compared this call with ordering on pipes and sockets did not apply.

### Nondeterminism sources

- Processes outside the guest writing the same file. They change where `O_APPEND` writes land (F8) and what later reads see.
- Host free space and quota (F17)
- Short counts and `EINTR` on slow filesystems (F15, F19)
- Real `mtime`/`ctime` values (F12), which Hermit replaces with virtual time when metadata is virtualized

### Cross-host variation

- Filesystem type behind the guest's file: its maximum file size decides where `EFBIG` starts (F16).
- Device logical block size and filesystem `O_DIRECT` support: which alignments an `O_DIRECT` write accepts and which give `EINVAL` (F14).
- Host swap configuration: which files are active swap files and give `ETXTBSY` (F18).

## Hermit design

Target policy: `model`. The kernel performs the write, and Detcore orders it against other access to the same file and owns the timestamps the guest sees. What the handler at f4055eb9cae4 does, in order:

- A descriptor bound to timer slack gives `EINVAL` for a negative offset and `ESPIPE` otherwise (`detcore/src/syscalls/files.rs:2784`). That matches Linux for F13: `timer_slack_procfs_vector_io_matches_linux` asserts both results (`detcore/tests/misc/mod.rs:540` and `detcore/tests/misc/mod.rs:561`). The offset is decoded by `vectored_offset`, which on 64-bit uses `pos_l` alone (`detcore/src/syscalls/files.rs:137`), as F2 requires.
- The handler picks a resource: the descriptor's own resource, or else `ResourceID::FileContents` of the determinized inode (`detcore/src/syscalls/files.rs:2800`). It takes a write permit on that resource (`detcore/src/syscalls/files.rs:2810`), so guest readers that take the read permit, such as `preadv`, are ordered with the write.
- The write goes to the kernel through `record_or_replay_preserving_tool_errors` (`detcore/src/syscalls/files.rs:2815`). That gives Linux's own F1, F3 to F11 and F14 to F19.
- With `virtualize_metadata` on, a write that returns more than 0 calls `touch_file` (`detcore/src/syscalls/files.rs:2819`), which asks the global state to advance the file's virtual mtime (`detcore/src/tool_global.rs:3763`). That gives F12's timestamps in virtual time.

The handler and its sibling `preadv` were added by https://github.com/rrnewton/hermit/pull/794, and the code tag `TODO-HUMAN-REVIEW(#794)` at `detcore/src/syscalls/files.rs:2773` refers to it. Its doc comment says "Positioned writes target seekable files and do not block" (`detcore/src/syscalls/files.rs:2776`). That holds for local filesystems, but not for F15.

Why `model` and not the alternatives: `passthrough` would expose real `mtime` values and skip the resource ordering. `emulate` would duplicate the filesystem. `refuse` would break databases and archivers that write with `pwritev`.

What remains before the code fully realizes `model`:

- **Error precedence for a bad descriptor with a negative offset (F3).** Unlike `preadv`, which checks the offset first (`detcore/src/syscalls/files.rs:2651`), `handle_pwritev` starts with a descriptor lookup through `timer_slack_binding` (`detcore/src/syscalls/files.rs:2784`, which calls `with_detfd` at `detcore/src/syscalls/files.rs:517`). Detcore returns `EBADF` for a descriptor missing from its table (`detcore/src/tool_local.rs:740`). By this reading of the code, `pwritev(-1, iov, 1, -1)` gives `EBADF` under Hermit where Linux gives `EINVAL`. This is not measured yet; T2 measures it. The fix is to move the negative-offset check ahead of the lookup, as `preadv` does.
- **Mode bits after a write (F12).** `touch_file` advances only the virtual mtime. This body did not find whether the virtual metadata reflects the kernel clearing set-user-ID and set-group-ID bits after a write. T8 checks it.

`linux_determinism` stays `conditional`. Hermit removes the timestamp source and orders writes within the guest. Writers outside the guest and host free space remain.

## Recommended tests

- T1 (covers F1, F2): `pwritev` of two buffers at an explicit offset returns their total length, overwrites exactly that range, and leaves `lseek(fd, 0, SEEK_CUR)` unchanged. A raw `syscall(SYS_pwritev, ...)` with garbage in `pos_h` writes at the same place.
  Existing partial coverage: `tests/c/vectored_file_io.c:87` and `tests/c/vectored_file_io.c:89` (cell `c-programs/vectored-file-io`, `tests/e2e/manifests/c-programs.yaml:15323`, verify ci). It writes `"WX"` and `"YZ"` at offset 4, checks the count and the unchanged offset, and reads the result back at line 96. It does not test `pos_h`.
- T2 (covers F3, F4, F5): check:
  - `pwritev(-1, iov, 1, -1)` gives `EINVAL` (Hermit is expected to give `EBADF` today)
  - `pwritev(-1, iov, 1, 0)` gives `EBADF`
  - `O_RDONLY` and `O_PATH` descriptors give `EBADF`
- T3 (covers F6): on a regular file, check:
  - `iovcnt` 0, `1UL << 32` and `(1UL << 32) | 1` write 0, 0 and one buffer
  - 1025 and `~0UL` give `EINVAL`
  - an invalid array gives `EFAULT`
  - a valid first buffer followed by an invalid second one returns the first buffer's length
- T4 (covers F7): `pwritev` on a pipe, a FIFO, an `AF_UNIX` socketpair and a pseudo-terminal gives `ESPIPE`, and nothing reaches the reader.
- T5 (covers F8): on a file opened with `O_APPEND`, `pwritev` at offset 0 appends at end of file. Check the file size and contents.
- T6 (covers F9, F10): a write at `size + 4096` extends the file, and the gap reads as zeros. A 1-byte write at `INT64_MAX` gives `EINVAL`. With `SIGXFSZ` ignored and `RLIMIT_FSIZE` set, a write starting at the limit gives `EFBIG`, and one crossing it returns the short count.
- T7 (covers F11): on a memfd sealed with `F_SEAL_WRITE`, `pwritev` gives `EPERM`. With `F_SEAL_GROW`, a write inside the current size succeeds and one past it gives `EPERM`. `ENOSPC`, `EDQUOT` and `ETXTBSY` are not portable in CI; they are F17 and F18, recorded by U1 and U2.
- T8 (covers F12): after `pwritev`, `stat` shows a later `mtime` and `ctime`, with the same values in every `--verify` run. A zero-length `pwritev` leaves them unchanged. For a guest without `CAP_FSETID`, check that the set-user-ID bit is cleared and that Hermit's `stat` agrees with the kernel.
- T9 (covers F13): `pwritev` on `/proc/self/timerslack_ns` gives `EINVAL` for offset -1 and `ESPIPE` for offset 0.
  Existing candidate: `timer_slack_procfs_vector_io_matches_linux` at `detcore/tests/misc/mod.rs:434` (the `pwritev` checks are at lines 540 and 561).
- T10 (covers F14, F15): on a filesystem that supports `O_DIRECT`, an unaligned buffer gives `EINVAL`. Record the filesystem in the output, because this is filesystem-dependent. Short counts on FUSE and network filesystems are not reproducible in CI (F19, recorded by U3), so this test only records the regular-file path.
- U1 (covers F17; host-transient): `ENOSPC` and `EDQUOT` depend on the host filesystem's free space and quotas, shared with other processes. Hermit forwards the write to the kernel (`detcore/src/syscalls/files.rs:2815`) and hands these errors to the guest today. F17 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F16, F18; host-config): the maximum file size of the host filesystem and the host's swap configuration decide `EFBIG` and `ETXTBSY`; a container guest can neither choose the filesystem nor activate swap. Hermit forwards the kernel's answer unchanged (`detcore/src/syscalls/files.rs:2815`).
- U3 (covers F19; host-transient): short counts and errors on FUSE and network filesystems depend on the server and the network. Hermit forwards the write (`detcore/src/syscalls/files.rs:2815`) and hands the result to the guest today. F19 counts toward done only once Hermit hides these results or reports them as an infrastructure error.
- T11 (covers F1, F12): record a guest that writes a file with `pwritev` and then reads it back with `preadv`, and replay it. Replay must give the same output and the same virtual `mtime`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/388: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel performs the write under a W permit on the file resource; virtual mtime via touch_file; timerslack_ns refused like Linux); linux_determinism stays conditional; 15 flag behaviours and 11 tests listed; open question: by code reading pwritev(-1, iov, 1, -1) gives EBADF under Hermit where Linux gives EINVAL; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem maximum file size, O_DIRECT block size, host swap configuration); F9 narrowed, new F16 EFBIG (host-config U2); F11 narrowed to memfd seals, new F17 ENOSPC/EDQUOT (host-transient U1) and F18 ETXTBSY on swap (host-config U2); F15 narrowed to the signal case, new F19 FUSE and network filesystems (host-transient U3); no open question is answered by these decisions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pwritev`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pwritev&body=Discussion+for+the+%60pwritev%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpwritev.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pwritev%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pwritev" (https://github.com/rrnewton/hermit/issues/388), legacy variant A1.

### Open questions

- Error precedence (F3): `handle_pwritev` looks the descriptor up in Detcore's table (`detcore/src/syscalls/files.rs:2784`) before it checks the offset, so by this reading `pwritev(-1, iov, 1, -1)` gives `EBADF` where Linux gives `EINVAL`. `preadv` checks the offset first (`detcore/src/syscalls/files.rs:2651`). Should the offset check move ahead of the lookup? T2 would measure the current result first.
- Mode bits (F12): does Hermit's virtual metadata track the kernel clearing set-user-ID and set-group-ID bits after a write by a guest without `CAP_FSETID`? This body did not find the answer in the code.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=pwritev` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `pwritev` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
pwritev(4, [{iov_base="vec", iov_len=3}], 1, 0) = 3
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
