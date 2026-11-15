---
schema: syscall-pages/v1
syscall: readv
nr_x86_64: 19
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/395
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readv&body=Discussion+for+the+%60readv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readv%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 5
---

## Summary

`readv(fd, iov, iovcnt)` reads from a file descriptor into several buffers in order (a scatter read). Detcore models it: it orders file reads through a resource, puts internal pipes and sockets through its deterministic nonblocking retry path, serves `/dev/urandom` and `/dev/random` from the canonical random stream, and records and replays the rest (`detcore/src/syscalls/files.rs:2561-2629` at hermit f4055eb9cae4). It returns `ENOSYS` on Detcore procfs snapshot fds, a known gap.

## Linux semantics

`readv` behaves like `read` into the concatenation of the iovec buffers, filled in array order. On a seekable file it reads at the file offset and advances it by the count returned.

Validation and return values:

- `iovcnt` above `IOV_MAX` (1024), or a total length above `SSIZE_MAX`, gives `EINVAL`.
- An unreadable iovec array, or a first buffer that cannot be written, gives `EFAULT`.
- A bad buffer after some data was copied gives the partial count.
- At end of file it returns 0.
- On a pipe, socket or tty it blocks until some data is available unless the fd is `O_NONBLOCK`, in which case it gives `EAGAIN`. It returns whatever is available, up to the total length.
- A signal before any data gives `EINTR`, or the call restarts under `SA_RESTART`.
- `EBADF` for a closed fd or one not open for reading; `EISDIR` for a directory.

### Flags and options

- F1: `iovcnt` 0 returns 0 without touching the fd.
- F2: `iovcnt` greater than `IOV_MAX` (1024), or a summed length that overflows `SSIZE_MAX`, gives `EINVAL`.
- F3: a bad iovec array or first buffer gives `EFAULT`; a bad later buffer after progress returns the partial count.
- F4: regular file: buffers are filled in order, the file offset advances by the count, and end of file returns 0.
- F5: pipe, socket or tty that is logically blocking: waits for data, then returns what is available (a short count).
- F6: `O_NONBLOCK` pipe or socket with no data gives `EAGAIN`.
- F7: signal delivery while blocked: `EINTR`, or a restart under `SA_RESTART`.
- F8: `/dev/urandom` and `/dev/random` (Detcore `FdType::Rng`): data from Detcore's canonical random stream.
- F9: a Detcore procfs snapshot fd (one with a procfs cursor): Hermit returns `ENOSYS`, whereas Linux reads the file.
- F10: a `sock_diag` or `NETLINK_ROUTE` netlink socket with metadata virtualization: the reply segments are sanitized.
- F11: a Detcore timer-slack procfs fd (`/proc/<pid>/timerslack_ns`): emulated vector read of the virtual value.
- F12: `EBADF` for a closed fd or one not open for reading; `EISDIR` for a directory.

## Determinism analysis

A regular-file `readv` repeats given the same file contents and offset, but the order against concurrent writers of the same file depends on host scheduling.

Reads from pipes, sockets and ttys depend on when the other endpoint wrote. The count returned, whether the call blocks, and whether a signal interrupts it (F5, F6, F7) all vary from run to run. External data such as network peers and the terminal varies outright.

`/dev/urandom` and `/dev/random` are nondeterministic by design (F8). Procfs and netlink replies carry host state such as pids, inodes and counters (F9, F10). Hence `conditional`.

## Hermit design

Current behaviour (hermit 239ca43d3092): inotify rename cookies are virtualized on every read path, and blocking reads wait in the scheduler. Linux draws the cookie pairing `IN_MOVED_FROM` with `IN_MOVED_TO` from one host-wide counter, so after a successful `read`, `readv`, or current-offset `preadv2` on an inotify descriptor Detcore replaces each nonzero cookie with a run-wide virtual one, numbered from 1 in first-observation order (`detcore/src/syscalls/inotify.rs:9`); a pair keeps one value however far apart its halves are read, including events copied before a fault and descriptors received from another process. Instances Detcore creates are physically nonblocking with the guest's blocking mode kept by Detcore, so a blocking read that finds nothing queued waits in Detcore's turns instead of in the kernel. The text below predates the renumbering where it says events are not rewritten; event contents other than the cookie fields still come from the kernel.

The policy is `model`, as implemented at hermit f4055eb9cae4:

- **Dispatch** (`detcore/src/lib.rs:2382-2387`, with `TODO-HUMAN-REVIEW(#794)` at `detcore/src/lib.rs:2380`):
  - when `sock_diag_reply_fd` holds (metadata virtualization on, and a `sock_diag` or `NETLINK_ROUTE` fd: `detcore/src/syscalls/io.rs:1529-1535`), the call goes to `handle_sock_diag_readv`, which reads and then sanitizes the segments (`detcore/src/syscalls/io.rs:1698-1710`) (F10);
  - otherwise it goes to `handle_readv_with_output`. The random bytes it produces are kept for the detlog (`detcore/src/lib.rs:2027`, `detcore/src/lib.rs:2905`).
- **Timer-slack procfs fds** are emulated (`detcore/src/syscalls/files.rs:2567-2573`, `readv_timer_slack` at `detcore/src/syscalls/files.rs:622`). Iovec validation (`detcore/src/syscalls/files.rs:191-224`) mirrors Linux: `EINVAL` above `UIO_MAXIOV`, empty for 0, `EFAULT` for a NULL array, `EINVAL` on length overflow (F11, F1, F2, F3).
- **Procfs snapshot fds** return `ENOSYS` (`detcore/src/syscalls/files.rs:2575-2580`). `read` and `pread64` serve the same snapshot (see the unit tests at `detcore/src/syscalls/files.rs:4703-4715`), so this is a compatibility gap rather than a determinism hole: the guest gets a fixed `ENOSYS` and no host data (F9).
- **Ordering:** a `Permission::R` resource is taken on the file's contents (`detcore/src/syscalls/files.rs:2593-2604`).
- **Random devices:** `FdType::Rng` goes to `read_random_vectors` (`detcore/src/syscalls/files.rs:2606-2617`). It checks read access, imports and validates the iovecs, and fills them from the canonical stream through the shared cursor (`detcore/src/syscalls/files.rs:2527-2559`) (F8).
- **Pipes, sockets and eventfds** that are physically nonblocking go through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:2618-2621`, `detcore/src/syscalls/helpers.rs:196-278`):
  - an internal fd under sequentialization is made nonblocking and retried on the deterministic schedule with an InternalIOPolling resource (`detcore/src/syscalls/helpers.rs:254-267`);
  - external fds, and record/replay on external fds, take the blocking path (`detcore/src/syscalls/helpers.rs:241-250`), whose completion time is host-decided and is captured by record/replay (F5, F6, F7).
- **Everything else** uses `record_or_replay` (`detcore/src/syscalls/files.rs:2622-2625`) (F4, F12).

Why not `emulate` or `refuse`: `readv` is used by glibc stdio and by many programs, and the data has to come from the real file or endpoint. The open item is F9: whether `readv` on a procfs snapshot fd should be served like `read` instead of `ENOSYS`. It is listed under Open questions.

## Recommended tests

- T1 (covers F8): repeated `readv` from `/dev/urandom` returns the canonical stream identically across runs and in replay. Existing candidate: `tests/c/random_readv_stream.c` (manifest cell `c-programs/random-readv-stream`, `tests/e2e/manifests/c-programs.yaml:4`, verify on ptrace and kvm, replay on ptrace).
- T2 (covers F4, F5): `readv` over a pipe into several iovecs fills them in order, identically across runs. Existing candidates: `tests/c/vectored_io.c` (cell `c-programs/vectored-io`) and `tests/c/ppoll_readv.c` (cell `c-programs/ppoll-readv`), both verify on ptrace.
- T3 (covers F3, F11): on a timer-slack procfs fd, a bad second iovec returns the partial count, as Linux does. Existing candidate: `timer_slack_procfs_vector_io_matches_linux` in `detcore/tests/misc/mod.rs:434` (lines 514-518).
- T4 (covers F1, F2, F12): `iovcnt` 0, `iovcnt` 1025, a summed length that overflows `SSIZE_MAX`, a closed fd, an `O_WRONLY` fd and a directory fd each return the Linux result on a regular file.
- T5 (covers F6, F7): `readv` on an empty `O_NONBLOCK` pipe gives `EAGAIN`; a blocking `readv` interrupted by a timer signal gives `EINTR` without `SA_RESTART` and completes with it, identically across runs under `--verify`.
- T6 (covers F9): `readv` on a procfs snapshot fd (for example `/proc/self/stat`) pins today's `ENOSYS`, or the snapshot bytes if the open question is resolved that way.
- T7 (covers F10): a `sock_diag` dump read with `readv` under metadata virtualization yields sanitized inode fields identically across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/395: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (resource-ordered file reads, nonblocking retry for internal pipes, canonical RNG stream, sock_diag sanitizing, record/replay); linux_determinism stays conditional; 12 flags, T1-T7 proposed; open question: ENOSYS on procfs snapshot fds; tracking_status stays todo.
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/random_readv_stream.c, T2 covered by tests/c/ppoll_readv.c, T6 covered by tests/c/proc_locks.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated: inotify cookie renumbering now applies to this read path; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `readv`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readv&body=Discussion+for+the+%60readv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readv%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: readv" (https://github.com/rrnewton/hermit/issues/395), legacy variant A1.

The 2026-07-24 evidence below describes how the call was routed at that time. At hermit f4055eb9cae4, `readv` has its own typed handler (see Hermit design), and the "temporary-passthrough" wording no longer applies.

### Open questions

- Should `readv` on a Detcore procfs snapshot fd be served from the snapshot, as `read` and `pread64` are, instead of returning `ENOSYS` (`detcore/src/syscalls/files.rs:2575-2580`)? Programs that read `/proc` files with `readv` (for example through some stdio implementations) fail under Hermit today. The behaviour is deterministic either way.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=readv` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `readv` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
readv(3, [{iov_base="measurement-host", iov_len=16}], 1) = 16
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
