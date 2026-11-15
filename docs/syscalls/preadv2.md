---
schema: syscall-pages/v1
syscall: preadv2
nr_x86_64: 327
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/328
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+preadv2&body=Discussion+for+the+%60preadv2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpreadv2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60preadv2%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 7
---

## Summary

`preadv2(fd, iov, iovcnt, pos_l, pos_h, flags)` is `preadv` with two additions: offset -1 means "read at the current file offset and advance it", as `readv` does, and a per-call `RWF_*` flags word. Detcore has a typed handler (`detcore/src/syscalls/files.rs:2713`) that:

- rejects offsets below -1 the way Linux does
- serves `/proc/self/timerslack_ns` from virtual state
- serves random devices from Hermit's canonical stream, with Linux's flag checks
- refuses other virtualized procfs files with `ENOSYS`
- sends everything else, flags unchanged, to the kernel under a read lock on the file's resource

## Linux semantics

Added in Linux 4.6. The syscall entry splits on the offset:

1. Offset -1 takes the `readv` path: the descriptor is looked up and the read uses and advances the file position. Pipes, FIFOs, sockets and terminals are allowed, and a read on a blocking descriptor with no data waits.
2. Any other negative offset gives `EINVAL`, before the descriptor is looked up.
3. Otherwise it takes the `preadv` path: the descriptor is looked up, a file without positional reads gives `ESPIPE`, and the file position is neither used nor changed.

Both paths then check that the file was opened for reading (`EBADF`), import the iovec array, return 0 for an empty request, check the offset range, and only then validate the flags. So an empty request returns 0 even with invalid flags, and an offset overflow is reported before an unknown flag. The flag checks, in order: unknown bits give `EOPNOTSUPP`; `RWF_APPEND` with `RWF_NOAPPEND` gives `EINVAL`; `RWF_NOWAIT` on a file without non-blocking support gives `EOPNOTSUPP`; `RWF_ATOMIC` on a read gives `EOPNOTSUPP`; `RWF_DONTCACHE` on a filesystem without support gives `EOPNOTSUPP`. A file whose operations lack `read_iter` accepts only `RWF_HIPRI`. The 2026-07-24 host strace under Additional information shows a 16-byte read of `/etc/hostname` at offset 0 with `RWF_SYNC|RWF_APPEND`, which a read ignores.

### Flags and options

- F1: offset -1: read at the current file position and advance it, as `readv` does. Works on pipes, FIFOs, sockets and terminals. A blocking descriptor with no data waits; an `O_NONBLOCK` one gives `EAGAIN`.
- F2: explicit offset (0 or more): reads start at `offset` and fill each buffer in turn. The file position is neither used nor changed, so a `dup` copy sees no movement.
- F3: offset encoding on x86_64: the 64-bit offset is `pos_l`, and `pos_h` is ignored.
- F4: offset below -1: `EINVAL`, checked before the descriptor lookup. So `preadv2(-1, ..., -2)` gives `EINVAL`, while `preadv2(-1, ..., -1)` and `preadv2(-1, ..., 0)` give `EBADF`.
- F5: descriptor not open, opened with `O_PATH`, or not opened for reading: `EBADF`. The read-mode check comes before the iovec array, so `iovcnt == 0` also gives `EBADF`.
- F6: the iovec array:
  - `iovcnt` is truncated to 32 bits, so `1UL << 32` acts as 0 and `(1UL << 32) | 1` as 1.
  - an empty request returns 0 before the flags are checked
  - `iovcnt` above `IOV_MAX` (1024) gives `EINVAL`
  - an unreadable array or first buffer gives `EFAULT`; a fault after some bytes were copied returns the short count
- F7: explicit offset on a descriptor without positional reads (pipe, FIFO, socket, terminal): `ESPIPE`
- F8: unknown flag bits: `EOPNOTSUPP`. This wins over the `RWF_APPEND`/`RWF_NOAPPEND` conflict of F12.
- F9: `RWF_HIPRI` (0x1): polled completion for `O_DIRECT` reads on devices that support polling; otherwise no effect. It is the only flag a file without `read_iter` accepts; any other flag there gives `EOPNOTSUPP`.
- F10: `RWF_DSYNC` (0x2) and `RWF_SYNC` (0x4): accepted on reads with no effect on the result.
- F11: `RWF_NOWAIT` (0x8): give `EAGAIN` instead of blocking when a pipe is empty; return data at once when it is available. F23 and F24 hold the host-dependent outcomes.
- F12: `RWF_APPEND` (0x10) and `RWF_NOAPPEND` (0x20): accepted on reads with no effect; both together give `EINVAL`.
- F13: `RWF_ATOMIC` (0x40): `EOPNOTSUPP` on any read.
- F14: `RWF_DONTCACHE` (0x80): an uncached buffered read where the filesystem supports it. F25 holds the `EOPNOTSUPP` case.
- F15: `RWF_NOSIGNAL` (0x100): Hermit's comment at `detcore/src/syscalls/files.rs:165` records that Linux 7.1 accepts it, including with `RWF_NOWAIT`, on random devices. It has no effect on a read.
- F16: end of file and short reads: a read that crosses end of file returns only the bytes before it, and a read at or past end of file returns 0. Pipes and sockets return whatever is available. FUSE and network filesystems, and signals, can give short counts or `EINTR`.
- F17: offset overflow: a non-empty request with `offset + length` past `INT64_MAX` gives `EINVAL`, before the flags are checked. An empty request at `INT64_MAX` returns 0.
- F18: random devices (`/dev/urandom`, `/dev/random`): the offset is ignored, and each call returns fresh random bytes.
- F19: procfs and sysfs pseudo-files: the kernel generates the contents from its own state when the read happens.
- F20: `/proc/self/timerslack_ns` and `/proc/<pid>/task/<tid>/timerslack_ns`: the decimal timer-slack text, read at the offset or, for -1, at the file position.
- F21: a directory descriptor: `EISDIR`.
- F22: `O_DIRECT` descriptors: the filesystem requires an aligned buffer, length and offset, and gives `EINVAL` otherwise.
- F23: `RWF_NOWAIT` on a regular file: `EAGAIN` or a short count when the data is not in the host page cache, which depends on host memory pressure and other processes.
- F24: `RWF_NOWAIT` on a file whose filesystem or driver does not support non-blocking reads: `EOPNOTSUPP`.
- F25: `RWF_DONTCACHE` on a filesystem without support, on a DAX file, or on a kernel older than the flag (where it is an unknown bit): `EOPNOTSUPP`.

## Determinism analysis

`linux_determinism: conditional`. For a regular file at an explicit offset, the bytes depend only on the file's contents and the offset, so the call repeats when nothing outside the guest changes the file. With offset -1 on a pipe or socket, the count depends on how much the writer had produced when the read ran, which depends on scheduling. `RWF_NOWAIT` adds a host-state dependence even for regular files: whether it returns data or `EAGAIN` depends on the host page cache. Flag support (F14, F24, F25) depends on the filesystem.

### Nondeterminism sources

- Processes outside the guest writing the file
- Pipe and socket reads at offset -1: the count depends on writer timing (F1, F16)
- `RWF_NOWAIT` on a regular file: `EAGAIN` or a short count depending on host page-cache residency (F23)
- Pseudo-files whose contents are host kernel state (F19)
- Random devices (F18)
- Short counts and `EINTR` on FUSE and network filesystems (F16)
- Filesystem-dependent flag support (F14, F22, F24, F25)

### Cross-host variation

- Filesystem type behind the guest's file: whether `RWF_NOWAIT` is supported (F24) and whether `RWF_DONTCACHE` works or gives `EOPNOTSUPP` (F14, F25).
- Device logical block size and filesystem `O_DIRECT` support: which alignments an `O_DIRECT` read accepts and which give `EINVAL` (F22).
- Kernel version: `RWF_DONTCACHE` and `RWF_NOSIGNAL` are recent flags; an older kernel treats them as unknown bits and gives `EOPNOTSUPP` where a newer one accepts them (F15, F25). On the random devices Detcore applies its own fixed checks instead (`detcore/src/syscalls/files.rs:2547`), so this applies to the kernel path only.

## Hermit design

Target policy: `model`. The kernel serves regular files. Detcore serves the files whose contents it virtualizes, and it serializes access to the file resource. What the handler at f4055eb9cae4 does, in order:

- It decodes the offset with `vectored_offset`, which on 64-bit uses `pos_l` alone (`detcore/src/syscalls/files.rs:137`), as F3 requires. It returns `EINVAL` for an offset below -1 before any descriptor lookup (`detcore/src/syscalls/files.rs:2720`), as F4 requires.
- A descriptor bound to timer slack is served by `readv_timer_slack` at the explicit offset, or at the file position for -1 (`detcore/src/syscalls/files.rs:2723`), after `require_timer_slack_access`. `readv_timer_slack` returns 0 for an empty request and gives `EOPNOTSUPP` for any flag but `RWF_HIPRI` (`detcore/src/syscalls/files.rs:632` and `detcore/src/syscalls/files.rs:635`), which is the F9 rule for a file without `read_iter` (F20).
- A descriptor with a procfs position, meaning a procfs file Detcore virtualizes, gives `ENOSYS` (`detcore/src/syscalls/files.rs:2736`). Linux would return the bytes (F19). `preadv` and `readv` refuse the same way.
- If the descriptor names a resource, the handler takes a read permit on it (`detcore/src/syscalls/files.rs:2747`), which orders the read against writers that take the write permit.
- An `FdType::Rng` descriptor is served by `read_random_vectors` (`detcore/src/syscalls/files.rs:2752`) with `offset: None` for -1, which reads the descriptor's shared stream and advances it, and the explicit offset otherwise (`detcore/src/syscalls/files.rs:2759`). That replaces the host randomness of F18 with a repeatable stream. `read_random_vectors` checks read access, imports the whole iovec array, and then calls `validate_random_vector_read` (`detcore/src/syscalls/files.rs:2547`), which mirrors Linux's order: an empty request returns 0 (`detcore/src/syscalls/files.rs:155`), an offset overflow gives `EINVAL` (`detcore/src/syscalls/files.rs:158`), unknown bits give `EOPNOTSUPP` (`detcore/src/syscalls/files.rs:179`), `RWF_APPEND|RWF_NOAPPEND` gives `EINVAL` (`detcore/src/syscalls/files.rs:182`), and `RWF_ATOMIC` or `RWF_DONTCACHE` gives `EOPNOTSUPP` (`detcore/src/syscalls/files.rs:185`).
- Everything else goes to the kernel through `record_or_replay_preserving_tool_errors` (`detcore/src/syscalls/files.rs:2765`), with the flags unchanged. That gives Linux's own F1, F2, F5 to F14, F16, F17, F21 and F22, and the bytes are recorded in record mode.

The handler was added by https://github.com/rrnewton/hermit/pull/794, and the code tag `TODO-HUMAN-REVIEW(#794)` at `detcore/src/syscalls/files.rs:2701` refers to it. That pull request's commit message describes `preadv2` as mirroring `pread64`, "positioned reads on seekable files do not block". The handler's doc comment does name the offset -1 shared-stream case (`detcore/src/syscalls/files.rs:2702`), but only the random-device branch treats it specially.

Why `model` and not the alternatives: plain `passthrough` would leak host randomness through `/dev/urandom` and the real timer-slack value. `emulate` for regular files would duplicate the page cache for no determinism gain. `refuse` would break programs that read through glibc's `preadv2` wrapper.

What remains before the code fully realizes `model`:

- **Offset -1 on pipes and sockets (F1).** `readv` sends a read on a physically non-blocking socket, pipe or eventfd through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:2618`), which turns a would-block result into a deterministic wait. `handle_pipe2` makes every pipe physically non-blocking whenever `use_nonblocking_sockets()` holds, record and replay included (`detcore/src/syscalls/files.rs:3584`). `handle_preadv2` has no such branch: with offset -1 on such a pipe it goes straight to the kernel. By this reading of the code, a guest calling `preadv2(pipe_read_end, ..., -1, 0)` on an empty, logically blocking pipe sees `EAGAIN` under Hermit where Linux waits for the writer. Also, there is no `NonblockableSyscall` implementation for `Preadv2` in `detcore/src/syscalls/helpers.rs` (`Readv` has one at `detcore/src/syscalls/helpers.rs:1213`). Not measured; T1 measures it.
- **Target (owner decision 14):** a `preadv2` without `RWF_NOWAIT` that can block, which means offset -1 on a pipe, socket or other stream, is a non-blocking probe plus a scheduler wait. `handle_preadv2` routes such a call through `execute_nonblockable_fd_syscall`, as `readv` does (`detcore/src/syscalls/files.rs:2618-2621`), which needs a `NonblockableSyscall` implementation for `Preadv2` like the one for `Readv` (`detcore/src/syscalls/helpers.rs:1213`). The probe carries the guest's flags, and a guest-requested `O_NONBLOCK` still gives `EAGAIN` at once. Today every such call goes to the kernel unchanged (`detcore/src/syscalls/files.rs:2765`), so on a Hermit-made non-blocking pipe it returns `EAGAIN` where Linux waits (F1), and on a physically blocking descriptor it waits in the kernel holding the turn. Positioned reads of regular files do not block, and the call is too widely used to refuse, so `target_policy` stays `model`.
- **`RWF_NOWAIT` (F11, F23).** The flag is forwarded unchanged, so on a regular file the result depends on the host page cache. Hermit could mask the flag, fail it closed, or accept the dependence; that is an owner decision (see Open questions). T7 measures the current behaviour.
- **Virtualized procfs files (F19).** `ENOSYS`, as for `preadv` and `readv`.

`linux_determinism` stays `conditional`. Hermit removes the random-device and virtual-file sources it handles. Host writers to shared files, `RWF_NOWAIT` and, until the gap above is closed, pipe and socket timing at offset -1 remain.

## Recommended tests

- T1 (covers F1, F16): offset -1 on a regular file reads at and advances the file position. On a pipe, a read at offset -1 waits until a second thread writes, then returns the written count; an `O_NONBLOCK` empty pipe gives `EAGAIN`. Run it several times under `--verify`.
  Existing partial coverage: `positioned_stream` in `tests/c/random_readv_stream.c:479` reads two bytes at offset -1 and checks that the shared stream advanced, but only on a random device (cell `c-programs/random-readv-stream`, `tests/e2e/manifests/c-programs.yaml:4`, verify and replay ci on ptrace).
- T2 (covers F2, F3): `preadv2` at explicit offsets returns the expected bytes and leaves `lseek(fd, 0, SEEK_CUR)` unchanged. A raw `syscall(SYS_preadv2, ...)` with garbage in `pos_h` reads the same bytes.
  Existing candidates: `tests/c/preadv2_flags.c:75` and `tests/c/preadv2_flags.c:81` read back at offsets 0 and 6 (cell `c-programs/preadv2-flags`, `tests/e2e/manifests/c-programs.yaml:13212`, verify ci on ptrace); `positioned_result` in `tests/c/random_readv_stream.c:447` passes `0x123456789abcdef0` in `pos_h`, on a random device.
- T3 (covers F4, F5, F6): on a regular file, check:
  - `preadv2(-1, iov, 1, -2)` gives `EINVAL`; offset -1 and 0 give `EBADF`
  - `O_WRONLY` and `O_PATH` descriptors give `EBADF`, also with `iovcnt == 0`
  - `iovcnt` 0, `1UL << 32` and `(1UL << 32) | 1` return 0, 0 and 1 byte; 1025 and `~0UL` give `EINVAL`; an invalid array gives `EFAULT`
  Existing coverage on a random device: `positioned_errors` in `tests/c/random_readv_stream.c:504`, whose `version2` pass covers each case (lines 513 to 533), same cell as T1. A regular-file variant is needed to check the kernel path.
- T4 (covers F7): `preadv2` at offset 0 on a pipe, a FIFO, an `AF_UNIX` socketpair and a pseudo-terminal gives `ESPIPE`.
- T5 (covers F8, F12, F13, F14, F15): the flag-policy table, on a regular file: unknown bits give `EOPNOTSUPP`; `RWF_APPEND|RWF_NOAPPEND` gives `EINVAL`; unknown bits with that pair give `EOPNOTSUPP`; `RWF_ATOMIC` gives `EOPNOTSUPP`; `RWF_DONTCACHE` succeeds or gives `EOPNOTSUPP` according to the filesystem (record which); `RWF_NOSIGNAL` succeeds; every flag with `iovcnt == 0` returns 0.
  Existing candidate on a random device: the table in `tests/c/random_readv_stream.c:541`, same cell as T1.
- T6 (covers F9, F10): `RWF_HIPRI`, `RWF_DSYNC` and `RWF_SYNC` on a regular file return the same bytes as flags 0.
  Existing candidate on a random device: the `{1, 0}`, `{2, 0}` and `{4, 0}` rows of the T5 table.
- T7 (covers F11): `RWF_NOWAIT` on a regular file that was just written (so it is cached) returns data, and on an empty pipe gives `EAGAIN`. Record whether Hermit returns the same result in every `--verify` run.
- U1 (covers F23; host-transient): whether a regular-file `RWF_NOWAIT` read finds its data in the host page cache depends on host memory pressure and other processes. Hermit forwards the flag unchanged (`detcore/src/syscalls/files.rs:2765`), so the `EAGAIN` or short count reaches the guest today. F23 counts toward done only once Hermit hides it or reports it as an infrastructure error.
- U2 (covers F24, F25; host-config): the filesystem and kernel behind the guest's file decide whether `RWF_NOWAIT` and `RWF_DONTCACHE` give `EOPNOTSUPP`, so the CI host produces at most one outcome for each. Hermit forwards the flags unchanged to the kernel (`detcore/src/syscalls/files.rs:2765`).
- T8 (covers F17): on a regular file, a 1-byte read at `INT64_MAX` gives `EINVAL`, also with an unknown flag; an empty request at `INT64_MAX` returns 0. The random-device cases exist at `tests/c/random_readv_stream.c:530`, `tests/c/random_readv_stream.c:531` and `tests/c/random_readv_stream.c:584`.
- T9 (covers F18): positioned and offset -1 reads of `/dev/urandom` follow Hermit's canonical stream.
  Existing candidate: `positioned_stream` in `tests/c/random_readv_stream.c:469`, same cell as T1.
- T10 (covers F19): `preadv2` on a procfs file Detcore virtualizes. Today Hermit returns `ENOSYS` where Linux returns the file's bytes.
- T11 (covers F20): `preadv2` of `/proc/self/timerslack_ns` at offset -1 with `RWF_HIPRI` returns the virtual value, and a second flag gives `EOPNOTSUPP`.
  Existing candidate: `timer_slack_procfs_vector_io_matches_linux` at `detcore/tests/misc/mod.rs:434` (the `preadv2` check is at lines 597 to 608).
- T12 (covers F21, F22): `preadv2` on a directory descriptor gives `EISDIR`. On a filesystem that supports `O_DIRECT`, an unaligned buffer gives `EINVAL`. Record the filesystem in the output.
- T13 (covers F1, F2): record a guest that reads a regular file and a pipe with `preadv2`, then change the file and replay. Replay must return the recorded bytes and counts.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/328#issuecomment-5872926157
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/328: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; preadv2 records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/328 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel serves files under a resource read permit with flags forwarded; Detcore serves timerslack_ns and the canonical RNG stream with Linux's RWF checks); linux_determinism stays conditional; 22 flag behaviours and 13 tests listed; open questions: offset -1 on a Hermit-nonblocking pipe has no nonblockable path (by code reading EAGAIN where Linux blocks), RWF_NOWAIT policy, ENOSYS on virtualized procfs; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem flag support, O_DIRECT block size, kernel version for recent RWF_ flags); F11 narrowed to the pipe and cached cases, new F23 page-cache EAGAIN (host-transient U1) and F24 EOPNOTSUPP for RWF_NOWAIT (host-config U2); F14 narrowed, new F25 EOPNOTSUPP for RWF_DONTCACHE (U2); no open question is answered by these decisions
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied (Target bullet: offset -1 on pipes and sockets goes through execute_nonblockable_fd_syscall with a new NonblockableSyscall impl for Preadv2; today it is forwarded and returns EAGAIN on a Hermit-made non-blocking pipe); target_policy stays model; no new F/T ids; removed the offset -1 nonblockable-path question
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T9 covered by tests/c/random_readv_stream.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `preadv2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+preadv2&body=Discussion+for+the+%60preadv2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpreadv2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60preadv2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: preadv2" (https://github.com/rrnewton/hermit/issues/328), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/328.

### Open questions

- `RWF_NOWAIT`: keep forwarding it (result depends on the host page cache), mask it, or fail it closed under `--strict`?
- Should `preadv2` on a procfs file Detcore virtualizes serve the snapshot, as `pread64` does, instead of `ENOSYS` (`detcore/src/syscalls/files.rs:2736`)? The same question is open on `preadv`.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=preadv2`, and by confirming hermit currently routes `preadv2` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
preadv2(5, [{iov_base="measurement-host", iov_len=16}], 1, 0, RWF_SYNC|RWF_APPEND) = 16
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/328#issuecomment-5872926157
