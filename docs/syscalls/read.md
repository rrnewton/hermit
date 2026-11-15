---
schema: syscall-pages/v1
syscall: read
nr_x86_64: 0
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+read&body=Discussion+for+the+%60read%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fread.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60read%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 26
---

## Summary

`read(fd, buf, count)` copies up to `count` bytes from the object behind `fd` into guest memory. What it returns, and whether it waits, depends on the descriptor type. Detcore handles it with a typed handler (`detcore/src/lib.rs:2366-2369`, `handle_read` at `detcore/src/syscalls/files.rs:1758` at hermit f4055eb9cae4). The handler synthesizes random-device bytes and Detcore-classified procfs content, loops regular-file reads to completion, and turns waits on pipes, sockets and notification descriptors into reads the scheduler can see.

Manual reference: Linux man-pages read(2).

## Linux semantics

`read` reads from the current offset of the open file description and advances that offset by the count returned. Copies made by `dup` or `fork` share one offset. On a regular file the result is a function of the file contents, the offset and `count`. A short count happens only at end of file, or when a signal arrives after some bytes were copied. Linux clamps `count` to `MAX_RW_COUNT` (`0x7ffff000`).

On pipes, FIFOs, sockets, terminals and notification descriptors, `read` waits until data arrives or the peer goes away. It may then return fewer bytes than requested. With `O_NONBLOCK` set on the open file description it fails with `EAGAIN` instead of waiting. Content generated at read time, such as procfs, sysfs and the random devices, reflects live kernel state.

Checks run in this order: the descriptor must be open and readable (`EBADF`, which includes `O_PATH` and write-only descriptors). Then the buffer must be in the user address range (`EFAULT`); this check applies even when `count` is 0. Then the type check runs (`EISDIR` for a directory, `EINVAL` for an object that cannot be read).

### Flags and options

- F1: regular file: copies `min(count, bytes to EOF)` bytes from the file offset and advances it. Returns 0 at end of file.
- F2: `count == 0`: returns 0 without transferring data. It still reports `EBADF` for a descriptor that is not readable, and `EFAULT` for a buffer address beyond the user address range.
- F3: pipe or FIFO: waits until bytes are available, then returns what is there, up to `count`, so the count can be short. Returns 0 once every writer has closed. With `O_NONBLOCK`, an empty pipe gives `EAGAIN`.
- F4: socket (stream, datagram, netlink): waits for data. A stream may return a short count. A datagram longer than `count` is truncated. Returns 0 after an orderly peer shutdown. With `O_NONBLOCK` it gives `EAGAIN`.
- F5: notification descriptors: an eventfd returns its 8-byte counter, and `count < 8` gives `EINVAL`. A timerfd returns an 8-byte expiration count and waits until the timer fires. A signalfd returns whole `signalfd_siginfo` records. An inotify descriptor returns whole event records, and a buffer too small for the next event gives `EINVAL`.
- F6: character devices: `/dev/urandom` and `/dev/random` return random bytes, `/dev/null` returns 0 (end of file), and terminals apply the line discipline.
- F7: procfs and sysfs files: the kernel builds the content at read time from live state (memory counters, uptime, process status) and keeps a per-description position.
- F8: errors: `EBADF` for a descriptor that is not open, is write-only, or is `O_PATH`. `EFAULT` for an unwritable buffer, `EISDIR` for a directory, `EINVAL` for an object that cannot be read (for example a pidfd or an epoll descriptor) or a misaligned `O_DIRECT` request, and `EIO` for a device or filesystem error.
- F9: signal interruption: `EINTR` if a handler runs before any byte is copied, or the call restarts if the handler uses `SA_RESTART`. A positive short count if bytes were already copied.
- F10: shared offset: `dup` and `fork` copies share one offset, so readers on the same description consume disjoint bytes. Which reader gets which bytes depends on scheduling.
- F11: `count` above `MAX_RW_COUNT` (`0x7ffff000`) is clamped. The buffer range check uses the unclamped `count`.
- F12: other descriptor types: a memfd reads like a regular file, and a userfaultfd returns `uffd_msg` event records.

## Determinism analysis

`linux_determinism: conditional`. A read of a regular file that no other process is writing repeats exactly: same bytes, same count, same new offset. Every other source varies between runs:

- Pipes and sockets: how many bytes are available when the reader runs depends on when the writer ran, so both the count returned and the split of a stream into reads change with host scheduling (F3, F4, F10).
- Waits that end at a time set by another process or by real time: timerfd expirations, signalfd delivery, inotify event batching (F5).
- Content that is random or generated live: the random devices (F6), and procfs and sysfs values such as free memory, uptime and host process identities (F7).
- Signals arriving at different points, which decide between `EINTR`, a restart and a short count (F9).
- Data from outside the container: network peers, terminals, files changed by host processes.

A guest whose reads touch only its own regular files and its own deterministic writers repeats. Otherwise it repeats only if Hermit removes these sources.

## Hermit design

Current behaviour (hermit 239ca43d3092): inotify rename cookies are virtualized on every read path, and blocking reads wait in the scheduler. Linux draws the cookie pairing `IN_MOVED_FROM` with `IN_MOVED_TO` from one host-wide counter, so after a successful `read`, `readv`, or current-offset `preadv2` on an inotify descriptor Detcore replaces each nonzero cookie with a run-wide virtual one, numbered from 1 in first-observation order (`detcore/src/syscalls/inotify.rs:9`); a pair keeps one value however far apart its halves are read, including events copied before a fault and descriptors received from another process. Instances Detcore creates are physically nonblocking with the guest's blocking mode kept by Detcore, so a blocking read that finds nothing queued waits in Detcore's turns instead of in the kernel. The text below predates the renumbering where it says events are not rewritten; event contents other than the cookie fields still come from the kernel.

Target policy: `model`. The kernel stays the authority for file contents and descriptor state. Detcore decides what the guest sees for every source listed above. At hermit f4055eb9cae4 the dispatch sends `Syscall::Read` to `handle_sock_diag_read` when the descriptor is a sock-diag or `NETLINK_ROUTE` socket and metadata virtualization is on (`detcore/src/lib.rs:2366`, predicate at `detcore/src/syscalls/io.rs:1529`). Every other read goes to `handle_read` (`detcore/src/lib.rs:2369`). `handle_read` does the following:

- A descriptor bound to a task's `timerslack_ns` procfs file is served entirely from Detcore state (`detcore/src/syscalls/files.rs:1763`, `read_timer_slack` at `detcore/src/syscalls/files.rs:562`).
- `count == 0` (F2) on a random-device descriptor returns 0 after Detcore checks the access mode and the buffer address itself (`detcore/src/syscalls/files.rs:1769-1789`). The code comment explains why: in replay that descriptor is an eventfd placeholder, and the kernel would answer `EINVAL`. Every other zero-length read goes to the kernel to get its error checks (`detcore/src/syscalls/files.rs:1790-1792`).
- Procfs files that Detcore classified at open time are served from a snapshot Detcore builds (`detcore/src/syscalls/files.rs:1795-1809`). That covers the live-content source in F7 for the files it classifies.
- The handler then requests the descriptor's resource from the scheduler (`detcore/src/syscalls/files.rs:1822-1833`) and dispatches on the descriptor type:
  - `Rng` (opened as `/dev/random` or `/dev/urandom`): bytes come from Detcore's seeded stream, never from the host (`detcore/src/syscalls/files.rs:1836-1846`). That covers F6.
  - `Regular`: with `deterministic_io` on, `deterministic_read` repeats the read until `count` bytes or end of file (`detcore/src/syscalls/files.rs:1847-1852`, loop at `detcore/src/syscalls/files.rs:2056`). An error after partial progress returns the bytes already read (`detcore/src/syscalls/files.rs:2090`), as Linux does. `hermit run` turns `deterministic_io` on unless `--no-deterministic-io` is passed (`hermit-cli/src/bin/hermit/run.rs:3668`). Record and replay turn it off (`hermit-cli/src/metadata.rs:404`, in `record_or_replay_config` at `hermit-cli/src/metadata.rs:342`).
  - Signalfd, eventfd, timerfd and inotify, and also sockets and pipes, go through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854-1873`, helper at `detcore/src/syscalls/helpers.rs:202`). With threads sequentialized, a blocking read on such a descriptor is retried non-blockingly while the scheduler runs other threads (`detcore/src/syscalls/helpers.rs:254-267`), but only when Detcore made the descriptor physically non-blocking (`ioaction_based_on_fd_status`, `detcore/src/syscalls/helpers.rs:776-778`); a descriptor that is blocking in the kernel takes the blocking path instead (`detcore/src/syscalls/helpers.rs:770-772`, then `detcore/src/syscalls/helpers.rs:250`). See the gap below. In record/replay mode, an external descriptor takes the blocking path and its timing is recorded, not controlled (`detcore/src/syscalls/helpers.rs:241-250`).
  - Memfd, pidfd, userfaultfd and epoll descriptors go to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:1862-1864`).
- A descriptor Detcore does not track fails with `EBADF` from Detcore's own table (`detcore/src/tool_local.rs:740`) before any kernel call. That agrees with Linux for a descriptor that is really closed.

Why `model` and not the alternatives: `passthrough` would expose host random bytes, live procfs values and kernel-scheduled waits, which are exactly the sources above. `emulate` cannot work for a regular file, because the kernel holds the data. `refuse` is impossible for the most common syscall there is.

Gaps against `model`, by reading the code:

- Only random devices and Detcore-classified procfs and sysfs files are synthesized. A read of any other live kernel file goes to the host as a `Regular` read.
- With `deterministic_io` off (record mode, or `--no-deterministic-io`), a regular-file short read reaches the guest as the kernel returned it.
- **Target (owner decision 14):** every blocking read on a notification descriptor is a non-blocking probe plus a scheduler wait. Eventfds already are in sequentialized run: `handle_eventfd2` adds `EFD_NONBLOCK` internally (`detcore/src/syscalls/files.rs:4189-4192`) and `F_SETFL` keeps it (`detcore/src/syscalls/files.rs:3113`). Signalfd, inotify and timerfd descriptors created without their non-blocking flag are physically blocking (`detcore/src/syscalls/files.rs:4212-4229`, `detcore/src/syscalls/files.rs:4283-4295`, `detcore/src/syscalls/files.rs:4241-4248`), so a blocking `read` on one goes to `record_or_replay_blocking` and sleeps in the kernel, and F5's wait point is host-decided. The fix is the eventfd recipe at creation plus the `F_SETFL` type list; the timerfd read itself moves to virtual time under decision 13. Not `refuse`: blocking reads are too widely used.
- The read-side split of a pipe stream depends on when the writer ran in Detcore's schedule. That is deterministic for a fixed seed but differs from native. `tests/c/short_io_split_identity.c` prints that split so verify compares it.

## Recommended tests

- T1 (covers F1, F10): write a known stream to a file, then read it back in fixed-size chunks through a descriptor and a `dup` copy. Check every return value, the shared offset, and the 0 at end of file, under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/file_io_roundtrip.c` (cell `c-programs/file-io-roundtrip`, verify ci on ptrace) and `tests/c/dup_shared_offset.c` (cell `c-programs/dup-shared-offset`, verify ci on ptrace).
- T2 (covers F2, F6, F8): on `/dev/urandom` opened read-only, write-only and read-write, check that `read(fd, NULL, 0)` returns 0 or `EBADF`, that a zero-length read at an address beyond the user range returns `EFAULT` or `EBADF`, and that bytes are identical across the two verify runs and across record and replay.
  Existing candidates: `tests/c/random_readv_stream.c` (cell `c-programs/random-readv-stream`, verify and replay ci on ptrace; the raw `SYS_read` zero-length check is at `tests/c/random_readv_stream.c:239`), `tests/c/random_sources.c` (cell `c-programs/random-sources`), and `getrandom_intercepted` at `detcore/tests/misc/mod.rs:1842`, which reads 8 bytes from both random devices with raw `SYS_read` but skips on hosts without perf counters.
- T3 (covers F3, F9): a parent reads a pipe that one or more forked children write. Check EOF after the last writer closes, and that both the byte stream and the read split are identical across verify runs. Also check that a blocking pipe read interrupted by a signal handler returns `EINTR`, or restarts with `SA_RESTART`, the same way in both runs.
  Existing candidates: `tests/c/pipe_ipc.c` (cell `c-programs/pipe-ipc`), `tests/c/pipe_multiwriter_ordering.c` (cell `c-programs/pipe-multiwriter-ordering`) and `tests/c/short_io_split_identity.c` (cell `c-programs/short-io-split-identity`), all verify ci on ptrace. None of them checks `EINTR`.
- T4 (covers F4): a `socketpair` stream read after the peer's `shutdown(SHUT_WR)` returns 0. A datagram larger than `count` is truncated. With `O_NONBLOCK`, an empty socket gives `EAGAIN`.
  Existing partial candidate: `tests/c/shutdown_socketpair.c` (cell `c-programs/shutdown-socketpair`, verify ci on ptrace).
- T5 (covers F5): read a timerfd, a signalfd and an inotify descriptor, and check that values and event order repeat across runs. Also check eventfd `count < 8` returns `EINVAL`.
  Existing candidates: `timerfd_expiry_is_deterministic`, `signalfd_delivery_is_deterministic` and `inotify_order_is_deterministic` at `detcore/tests/misc/notification_fds.rs:307`, `detcore/tests/misc/notification_fds.rs:312` and `detcore/tests/misc/notification_fds.rs:317`, plus `tests/c/eventfd_probe.c` (cell `c-programs/eventfd-semantics`, verify ci on ptrace).
- T6 (covers F7): read `/proc/meminfo` and other Detcore-classified procfs files twice in one run and across runs. Check that the values are identical across runs, and that offsets and partial reads follow seq_file rules.
  Existing candidates: `tests/c/meminfo_free_deterministic.c` (cell `c-programs/meminfo-free-deterministic`) and `tests/c/procfs_identity_agreement.c` (cell `c-programs/procfs-identity-agreement`), both verify ci on ptrace.
- T7 (covers F8): `read` on a write-only descriptor (`EBADF`), a closed descriptor (`EBADF`), an `O_PATH` descriptor (`EBADF`), a directory (`EISDIR`), an epoll descriptor (`EINVAL`), and into a read-only mapping (`EFAULT`).
  Existing partial candidate: `tests/c/openat_flags.c` (cell `c-programs/openat-flags`, verify ci on ptrace) checks `EBADF` for a read on an `O_WRONLY` descriptor.
- T8 (covers F11, F12): read a small file with `count = 0x80000000` into a buffer whose range fits below the user address limit, and check that the call returns the file length. Read a memfd as a regular file.
- T9 (covers F1, F3): record and replay a guest that reads a regular file and a pipe. Replay must reproduce every return value and byte without touching the host file.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_read synthesizes random-device and classified procfs bytes, loops regular-file reads under deterministic_io, and turns pipe, socket and notification waits into scheduler-visible retries); linux_determinism stays conditional (regular files repeat, pipes, sockets, devices and procfs do not); 12 flag behaviours and 9 tests listed; no coverage token recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decision 14 (blocking notification-descriptor reads become probe plus scheduler wait; corrected the claim that signalfd, inotify and timerfd reads already retry non-blockingly); target_policy unchanged; no new F/T ids; no questions removed.
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/random_readv_stream.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated: inotify cookie renumbering now applies to this read path; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `read`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+read&body=Discussion+for+the+%60read%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fread.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60read%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `read`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

Notes for the next agent:

- The lit tests `detcore/tests/lit/child_should_inherit_fds/main.rs` and `detcore/tests/lit/dup2_existing_fd/main.rs` name `SYS_read` only in comments, which say that a read there would time out. They run through Buck's lit runner, not as a cargo test target or an e2e cell, so they cannot be cited as coverage.
- The header comment of `tests/c/short_io_split_identity.c` records an earlier measurement: a blocking pipe write returned a stable short count under Hermit. `execute_blocking_pipe_write` (`detcore/src/syscalls/helpers.rs:496`) was added to complete such writes. Whether the measurement still holds is not established here; the write body tracks it.
