---
schema: syscall-pages/v1
syscall: preadv
nr_x86_64: 295
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/327
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+preadv&body=Discussion+for+the+%60preadv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpreadv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60preadv%60%2C+or+in+what+the+page+says%3F%0A
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

`preadv(fd, iov, iovcnt, pos_l, pos_h)` reads into several buffers from an explicit file offset, without using or moving the file offset. Detcore has a typed handler (`detcore/src/syscalls/files.rs:2645`) that:

- rejects negative offsets the way Linux does
- serves `/proc/self/timerslack_ns` from virtual state
- serves random devices from Hermit's canonical stream
- refuses other virtualized procfs files with `ENOSYS`
- sends everything else to the kernel under a read lock on the file's resource

## Linux semantics

`do_preadv` checks the arguments in this order:

1. A negative offset gives `EINVAL`, before the descriptor is looked up.
2. The descriptor is looked up. A missing descriptor or an `O_PATH` one gives `EBADF`.
3. A file without positional-read support gives `ESPIPE`. That covers pipes, FIFOs, sockets, and anything opened through `nonseekable_open`.
4. `vfs_readv` then checks that the file was opened for reading (`EBADF` if not) and imports the iovec array.

The read fills the buffers in order and returns the number of bytes copied. That number is short at end of file and can be short after a fault part-way through. The file offset is never read or written. On x86_64 the whole 64-bit offset is `pos_l`, and `pos_h` is ignored. The 2026-07-24 host strace under Additional information shows a 16-byte read of `/etc/hostname` at offset 0.

### Flags and options

- F1: explicit offset: reads start at `offset` and fill each buffer before the next. The file offset is neither used nor changed, so a `dup` copy sharing the open file description sees no movement.
- F2: offset encoding on x86_64: the 64-bit offset is `pos_l`, and `pos_h` is ignored
- F3: negative offset: `EINVAL`. This is checked before the descriptor lookup, so `preadv(-1, ..., -2)` gives `EINVAL`, not `EBADF`.
- F4: descriptor not open, or opened with `O_PATH`: `EBADF`
- F5: descriptor not opened for reading (`O_WRONLY`): `EBADF`. This is checked before the iovec array, so even `iovcnt == 0` gives `EBADF`.
- F6: the iovec array:
  - `iovcnt` is truncated to 32 bits, so `1UL << 32` acts as 0 and `(1UL << 32) | 1` as 1.
  - `iovcnt == 0` returns 0 and reads nothing.
  - `iovcnt` above `IOV_MAX` (1024) gives `EINVAL`.
  - An unreadable array or first buffer gives `EFAULT`.
  - A fault in a later buffer, after some bytes were copied, returns the short count.
- F7: descriptor that does not support positional reads (pipe, FIFO, socket, terminal): `ESPIPE`
- F8: end of file and short reads: a read that crosses end of file returns only the bytes before it, and a read at or past end of file returns 0. On FUSE and network filesystems, and after a signal, a regular-file read can also return a short count or `EINTR`.
- F9: offset overflow: a non-empty request with `offset + length` past `INT64_MAX` gives `EINVAL`. An empty request at `INT64_MAX` returns 0.
- F10: random devices (`/dev/urandom`, `/dev/random`): the offset is ignored, and each call returns fresh random bytes
- F11: procfs and sysfs pseudo-files: the kernel generates the contents from its own state when the read happens, and then serves the bytes at the given offset
- F12: `/proc/<pid>/task/<tid>/timerslack_ns` and `/proc/self/timerslack_ns`: a positional read of the decimal timer-slack text
- F13: a directory descriptor: `EISDIR`
- F14: `O_DIRECT` descriptors: the filesystem requires an aligned buffer, length and offset, and gives `EINVAL` otherwise

## Determinism analysis

`linux_determinism: conditional`. For a regular file, the bytes returned depend only on the file's contents and the offset, so the call repeats when nothing outside the guest changes the file. It does not repeat for pseudo-files that report kernel state, for random devices, or where a filesystem returns short counts that depend on timing. Pipes and sockets are not a source here: they give `ESPIPE` (F7), so the earlier wording that said reads from pipes and sockets were nondeterministic did not apply to this call.

### Nondeterminism sources

- Processes outside the guest writing the file
- Pseudo-files whose contents are host kernel state, such as most of procfs and sysfs (F11)
- Random devices (F10)
- Short counts and `EINTR` on FUSE and network filesystems (F8)

## Hermit design

Target policy: `model`. The kernel serves regular files. Detcore serves the files whose contents it virtualizes, and it serializes access to the file resource. What the handler at f4055eb9cae4 does, in order:

- It decodes the offset with `vectored_offset`, which on 64-bit uses `pos_l` alone (`detcore/src/syscalls/files.rs:137`), as F2 requires. It returns `EINVAL` for a negative offset before any descriptor lookup (`detcore/src/syscalls/files.rs:2651`), as F3 requires.
- A descriptor bound to timer slack is served by `readv_timer_slack` at the explicit offset (`detcore/src/syscalls/files.rs:2656`), after an access check (`require_timer_slack_access`) for F12.
- A descriptor with a procfs position, meaning a procfs file Detcore virtualizes, gives `ENOSYS` (`detcore/src/syscalls/files.rs:2664`). Linux would return the bytes (F11). See the open question below.
- If the descriptor names a resource, the handler takes a read permit on it (`detcore/src/syscalls/files.rs:2675`). That orders the read against writers that take the write permit, such as `pwritev`.
- An `FdType::Rng` descriptor is served by `read_random_vectors` at the explicit offset (`detcore/src/syscalls/files.rs:2680`). The handler's doc comment says "RNG reads use the canonical stream at the explicit offset without advancing its shared cursor" (`detcore/src/syscalls/files.rs:2635`). That replaces the host randomness of F10 with a repeatable stream. Linux ignores the offset, so this is a deliberate difference.
- Everything else goes to the kernel through `record_or_replay_preserving_tool_errors` (`detcore/src/syscalls/files.rs:2693`). That gives Linux's own F1, F4 to F9, F13 and F14, and the bytes are recorded in record mode.

The handler and its sibling `pwritev` were added by https://github.com/rrnewton/hermit/pull/794, and the code tag `TODO-HUMAN-REVIEW(#794)` at `detcore/src/syscalls/files.rs:2632` refers to it.

Why `model` and not the alternatives: plain `passthrough` would leak host randomness through `/dev/urandom`, and the timer-slack value and procfs snapshots through the kernel's real files. `emulate` for regular files would duplicate the page cache for no determinism gain. `refuse` would break databases and loaders that read with `preadv`.

What remains before the code fully realizes `model`:

- **Virtualized procfs files.** `preadv` gives `ENOSYS` where `pread64` serves the same file from its snapshot. `pread64` initializes the snapshot and serves bytes at the offset (`detcore/src/syscalls/files.rs:1899` and `detcore/src/syscalls/files.rs:1910`). `readv` refuses the same way (`detcore/src/syscalls/files.rs:2578`). `tests/c/proc_locks.c:160` asserts the `ENOSYS`, so the current behaviour is pinned by a test, but it differs from Linux.
- **Short reads.** `pread64` retries short reads on regular files when `deterministic_io` is set (`detcore/src/syscalls/files.rs:1936`; option documented at `detcore-model/src/config.rs:440`). `preadv` has no such retry, so on filesystems that return short counts (F8) its result can vary even with that option on.

`linux_determinism` stays `conditional`. Hermit removes the random-device source and the virtual-file sources it handles. Host writers to shared files remain.

## Recommended tests

- T1 (covers F1, F2): `preadv` into two buffers at an explicit offset returns the expected bytes and leaves `lseek(fd, 0, SEEK_CUR)` unchanged. A raw `syscall(SYS_preadv, ...)` with garbage in `pos_h` reads the same bytes.
  Existing partial coverage: `tests/c/vectored_file_io.c:96` (cell `c-programs/vectored-file-io`, `tests/e2e/manifests/c-programs.yaml:15323`, verify ci), which reads 10 bytes into two buffers at offset 0 but does not check the offset afterwards. Also `positioned_result` in `tests/c/random_readv_stream.c:446`, which passes non-zero `pos_h` padding, but only on a random device.
- T2 (covers F3, F4, F5): on a regular file, check:
  - `preadv(-1, iov, 1, -2)` gives `EINVAL`
  - `preadv(-1, iov, 1, 0)` gives `EBADF`
  - `O_WRONLY` and `O_PATH` descriptors give `EBADF`, also with `iovcnt == 0`
  Existing coverage on a random device: `positioned_errors` in `tests/c/random_readv_stream.c:504` (cell `c-programs/random-readv-stream`, `tests/e2e/manifests/c-programs.yaml:4`, verify and replay ci on ptrace). Hermit serves that device itself, so a regular-file variant is needed to check the kernel path.
- T3 (covers F6): on a regular file, check:
  - `iovcnt` 0, `1UL << 32` and `(1UL << 32) | 1` return 0, 0 and 1 byte
  - 1025 and `~0UL` give `EINVAL`
  - an invalid array gives `EFAULT`
  - a valid first buffer followed by an invalid second one returns the first buffer's length
  The random-device cases exist in `tests/c/random_readv_stream.c:525`.
- T4 (covers F7): `preadv` on a pipe, a FIFO, an `AF_UNIX` socketpair and a pseudo-terminal gives `ESPIPE`.
- T5 (covers F8, F9): on a 10-byte file, a 16-byte read at offset 6 returns 4, and a read at offset 10 or 4096 returns 0. A 1-byte read at `INT64_MAX` gives `EINVAL`, and an empty request at `INT64_MAX` returns 0. The `INT64_MAX` cases exist for the random device in `tests/c/random_readv_stream.c:530`.
- T6 (covers F10): positioned reads of `/dev/urandom` follow Hermit's canonical stream at offsets 5, 65536, `0x100000005` and `INT64_MAX - 1`, and do not move the shared cursor seen through a `dup` copy.
  Existing candidate: `positioned_stream` in `tests/c/random_readv_stream.c:469`, same cell as T2.
- T7 (covers F11): `preadv` on a procfs file Detcore virtualizes. Today Hermit returns `ENOSYS` where Linux returns the file's bytes.
  Existing candidate pinning the current Hermit result: `tests/c/proc_locks.c:160` (cell `c-programs/proc-locks`, `tests/e2e/manifests/c-programs.yaml:5943`, verify ci). If the owner decides `preadv` should serve the snapshot, this assertion changes with the code.
- T8 (covers F12): `preadv` of `/proc/self/timerslack_ns` at offset 1 returns the tail of the virtual value, and offset -1 gives `EINVAL`.
  Existing candidate: `timer_slack_procfs_vector_io_matches_linux` at `detcore/tests/misc/mod.rs:434` (the `preadv` checks are at lines 588 and 613).
- T9 (covers F13, F14): `preadv` on a directory descriptor gives `EISDIR`. On a filesystem that supports `O_DIRECT`, an unaligned buffer gives `EINVAL`. Record the filesystem in the test output, because this is filesystem-dependent.
- T10 (covers F1, F8): record a guest that reads a regular file with `preadv`, then change the file and replay. Replay must return the recorded bytes and counts.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/327#issuecomment-5872923807
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/327: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; preadv records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/327 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel serves regular files under a resource read permit; Detcore serves timerslack_ns and the canonical RNG stream at the explicit offset); linux_determinism stays conditional; 14 flag behaviours and 10 tests listed; open question: preadv returns ENOSYS on virtualized procfs files where pread64 serves the snapshot; no coverage token, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T6 covered by tests/c/random_readv_stream.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `preadv`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+preadv&body=Discussion+for+the+%60preadv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpreadv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60preadv%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: preadv" (https://github.com/rrnewton/hermit/issues/327), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/327.

### Open questions

- Should `preadv` (and `readv`) on a procfs file Detcore virtualizes serve the snapshot as `pread64` does, instead of returning `ENOSYS` (`detcore/src/syscalls/files.rs:2664`, `detcore/src/syscalls/files.rs:2578`)? Linux returns the bytes. `tests/c/proc_locks.c:160` currently asserts `ENOSYS`, so changing it is an owner decision, not a bug fix.
- Should `preadv` honour `deterministic_io` short-read retries as `pread64` does (`detcore/src/syscalls/files.rs:1936`)?

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=preadv`, and by confirming hermit currently routes `preadv` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
preadv(5, [{iov_base="measurement-host", iov_len=16}], 1, 0) = 16
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/327#issuecomment-5872923807
