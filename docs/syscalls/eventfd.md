---
schema: syscall-pages/v1
syscall: eventfd
nr_x86_64: 284
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+eventfd&body=Discussion+for+the+%60eventfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Feventfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60eventfd%60%2C+or+in+what+the+page+says%3F%0A
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

`eventfd(initval)` is the legacy, flagless form of `eventfd2`: it creates a descriptor that holds a 64-bit counter, which `write` adds to and `read` drains. Detcore turns the legacy call into `eventfd2` with flags 0 (`detcore/src/lib.rs:2574`) and handles it in `handle_eventfd2` (`detcore/src/syscalls/files.rs:4184`). The kernel keeps the counter. Detcore registers the descriptor and, in sequentialized `hermit run`, makes it nonblocking in the kernel without showing that to the guest, so that blocking reads and writes wait inside the deterministic scheduler.

Manual reference: Linux man-pages eventfd(2).

## Linux semantics

The call allocates an anonymous-inode file whose only state is an unsigned 64-bit counter, set to `initval`. The legacy syscall takes no flags argument, so the descriptor is always blocking, is not close-on-exec, and counts in normal (not semaphore) mode. `EFD_SEMAPHORE`, `EFD_NONBLOCK` and `EFD_CLOEXEC` exist only on `eventfd2`; a program that wants them after a legacy `eventfd` must use `fcntl`.

What the guest can observe afterwards comes through other syscalls on the descriptor:

- `read` of at least 8 bytes returns the counter and resets it to zero. If the counter is zero, `read` blocks, or fails with `EAGAIN` once `O_NONBLOCK` has been set with `fcntl`. A buffer shorter than 8 bytes gives `EINVAL`.
- `write` of an 8-byte value adds it to the counter. If the sum would exceed `0xfffffffffffffffe`, `write` blocks until a read makes room, or gives `EAGAIN` when nonblocking. Writing `0xffffffffffffffff` gives `EINVAL`, as does a buffer shorter than 8 bytes.
- `poll`, `select` and `epoll` report readable while the counter is non-zero and writable while at least 1 can be added.
- `/proc/<pid>/fdinfo/<fd>` shows `eventfd-count` and, on recent kernels, an `eventfd-id` derived from the kernel's own allocation.
- The descriptor is shared by `dup` and `fork` and survives `execve`, since the legacy call cannot set close-on-exec.

The counter is shared state, so with several readers and writers the value each `read` returns depends on the order of the operations.

### Flags and options

- F1: `initval`: an `unsigned int` initial counter. Every 32-bit value is accepted, including `0xffffffff` (host probe: `eventfd(4294967295) = 4`).
- F2: no flags argument: the legacy call behaves as `eventfd2(initval, 0)`, so the descriptor is blocking, inheritable across `execve`, and in counter mode. `EFD_SEMAPHORE`, `EFD_NONBLOCK` and `EFD_CLOEXEC` are not reachable through this syscall.
- F3: `read` on the descriptor: returns and zeroes the counter. It blocks while the counter is zero, gives `EAGAIN` once `O_NONBLOCK` has been set with `fcntl`, and gives `EINVAL` for a buffer shorter than 8 bytes.
- F4: `write` on the descriptor: adds an 8-byte value. It blocks, or gives `EAGAIN` when nonblocking, if the sum would exceed `0xfffffffffffffffe`. A value of `0xffffffffffffffff`, or a buffer shorter than 8 bytes, gives `EINVAL`.
- F5: readiness: `poll`, `select` and `epoll` report readable while the counter is non-zero and writable while it can still grow.
- F6: descriptor identity: the lowest free descriptor number is returned. `dup` and `fork` copies share one counter, and the descriptor stays open across `execve`.
- F7: `fcntl(F_SETFL, O_NONBLOCK)` and `fcntl(F_GETFL)`: the only way to make a legacy eventfd nonblocking, and the way a guest observes that state.
- F8: errors: `EMFILE` when the process descriptor table is full, `ENFILE` at the system-wide file limit, `ENODEV` if the anonymous inode cannot be mounted, `ENOMEM` when the kernel cannot allocate the file.
- F9: `/proc/<pid>/fdinfo/<fd>`: shows `eventfd-count` and a kernel-assigned `eventfd-id`.

## Determinism analysis

`linux_determinism: conditional`. With the same descriptor table and the same order of operations on the counter, the call returns the same descriptor and every later `read` sees the same value. It differs when:

- The order of reads and writes from different threads or processes changes. That decides the value each `read` returns and whether a blocking `read` or `write` waits.
- The descriptor table differs, which changes the number returned (F6), or the process or system file limits differ (F8).
- The program reads host-assigned identity: the `eventfd-id` line in fdinfo and the anonymous inode number that `fstat` reports.

### Nondeterminism sources

- Thread and process scheduling around a shared counter
- Wake-up order when several threads wait in `read` or `write` on one eventfd
- System-wide file limits (`ENFILE`) and memory pressure (`ENOMEM`)
- Kernel-assigned identity: `eventfd-id` and the anonymous inode number

## Hermit design

Target policy: `model`. The kernel stays the store for the counter and Detcore owns everything that can make the result depend on host timing: when a thread waits, the order in which threads touch the counter, and the identity the guest sees.

What the code does at this SHA:

- The legacy call reaches the `eventfd2` handler (`detcore/src/lib.rs:2574`). The conversion in the pinned reverie (`reverie-syscalls/src/syscalls.rs:3088` at reverie d766df20b780) sets the flags argument to 0 (`reverie-syscalls/src/syscalls.rs:3094`), so the kernel sees `eventfd2(initval, 0)`, which matches F2.
- In sequentialized `hermit run` outside record/replay, Detcore adds `EFD_NONBLOCK` to the call it sends to the kernel (`detcore/src/syscalls/files.rs:4189`). It registers the descriptor with the guest's own flags (`detcore/src/syscalls/files.rs:4197`), so the guest still sees a blocking descriptor, and marks it physically nonblocking (`detcore/src/syscalls/files.rs:4206`).
- A `read` or `write` on the descriptor goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854`). For a descriptor the guest believes is blocking but the kernel treats as nonblocking, `ioaction_based_on_fd_status` chooses a retry (`detcore/src/syscalls/helpers.rs:776`), and the retry runs under the scheduler's `InternalIOPolling` resource (`detcore/src/syscalls/helpers.rs:254`). A waiting thread is therefore parked by the deterministic scheduler, not by the kernel.
- `fcntl(F_SETFL)` keeps an eventfd nonblocking in the kernel whatever the guest asks (`detcore/src/syscalls/files.rs:3111`), and `fcntl(F_GETFL)` reports the guest's own `O_NONBLOCK` setting (`detcore/src/syscalls/files.rs:3097`). That realizes F7.
- With metadata virtualization on, `add_fd` records an `fstat` result for the descriptor (`detcore/src/syscalls/files.rs:923`). The fdinfo sanitizer replaces `eventfd-id` with the virtual open-file id (`detcore/src/procfs.rs:2809`) and passes `eventfd-count` through unchanged (`detcore/src/procfs.rs:2825`), because the count is guest state, not host identity. That covers F9.
- Both `eventfd` and `eventfd2` are subscribed in record/replay (`detcore/src/lib.rs:1171`), and the handler goes through `record_or_replay` (`detcore/src/syscalls/files.rs:4196`).

Why `model` and not the alternatives:

- `passthrough` would let a thread sleep inside the kernel on an empty counter, where the sequentialized scheduler cannot see it.
- `emulate`, a Detcore-owned counter with no kernel file, would also have to emulate `poll`, `epoll`, `select`, `splice` and every other path that can touch the descriptor, and would lose the kernel's exact error checks. The kernel already gives a correct counter once Detcore controls the waits.
- `refuse` would break libuv, Node.js, glib and other event loops that wake threads through an eventfd.

Target (owner decision 14): a blocking `read` or `write` is a non-blocking probe plus a scheduler wait. Sequentialized `hermit run` already does this through the hidden `EFD_NONBLOCK` (`detcore/src/syscalls/files.rs:4189`) and the `NonblockizeRetry` strategy (`detcore/src/syscalls/helpers.rs:776`, `detcore/src/syscalls/helpers.rs:254`). Record/replay still takes the blocking path below; whether an eventfd counts as internal there stays an owner question.

What remains before the code fully realizes `model`:

- **Record/replay.** With `recordreplay_modes` set, the handler leaves the descriptor blocking in the kernel (`detcore/src/syscalls/files.rs:4190`). `execute_nonblockable_fd_syscall` then sends a blocking `read` or `write` down the external blocking path (`detcore/src/syscalls/helpers.rs:242`), because only pipes count as internal descriptors (`detcore/src/syscalls/helpers.rs:804`). That path lets the thread leave the run queue and rejoin at a time the scheduler does not choose (`detcore/src/syscalls/helpers.rs:155`). The same comment names this as the cause of the record/replay pipe deadlock. By that reasoning, two guest threads that hand work to each other through a blocking eventfd can hang in record mode. This is a hypothesis from the code, not a measurement; T7 tests it.
- **No test issues the legacy syscall.** Every existing test uses the glibc wrapper, which issues `eventfd2` (the host trace under Additional information shows Python's `os.eventfd` doing the same). The flags-zero conversion in F2 is therefore not tested on the legacy number.

## Recommended tests

- T1 (covers F1, F2, F6): call `syscall(SYS_eventfd, 3)` and `syscall(SYS_eventfd, 0xffffffff)` directly. Check the returned descriptors are the lowest free numbers, that `fcntl(F_GETFD)` shows no close-on-exec, that `fcntl(F_GETFL)` shows no `O_NONBLOCK`, that a `read` returns the initial value, and that a `fork` child and a `dup` copy see the same counter. Run under `hermit run --verify` on ptrace.
- T2 (covers F3, F4): on a raw legacy eventfd, check counter arithmetic (several writes, then one read returns the sum), `EINVAL` for a 4-byte read and a 4-byte write, `EINVAL` for writing `0xffffffffffffffff`, and that a nonblocking write which would pass `0xfffffffffffffffe` gives `EAGAIN`.
  Existing candidate for the eventfd2 form: `tests/c/eventfd_probe.c` (cell `c-programs/eventfd-semantics`, `tests/e2e/manifests/c-programs.yaml:15196`, verify ci on ptrace). It checks counter and semaphore arithmetic through glibc, so it never issues syscall 284 and never checks the error cases.
- T3 (covers F3, F7): two threads, one blocked in `read` on an empty legacy eventfd, the other writing to it after some work. Under `hermit run --verify` the reader must wake with the written value on every run, and `fcntl(F_GETFL)` must report the descriptor blocking before and after. Then set `O_NONBLOCK` with `fcntl` and check a read on the empty counter gives `EAGAIN` and `F_GETFL` shows the flag.
  Existing candidate for the flag reporting: `notification_control_syscalls_are_deterministic` at `hermit-cli/tests/epoll_determinism.rs:399`, which runs the `control-fds` scenario of `tests/c/epoll_determinism.c`. That scenario creates a blocking eventfd with `eventfd(1, EFD_CLOEXEC)` and checks that `F_GETFL` does not show `O_NONBLOCK` (`tests/c/epoll_determinism.c:348`). It goes through glibc, so it issues `eventfd2`.
- T4 (covers F5): put a legacy eventfd in an epoll set with other ready descriptors and check the order and count of events across runs.
  Existing candidate for the eventfd2 form: `mixed_epoll_sources_are_deterministic` at `detcore/tests/misc/notification_fds.rs:322`, which adds an `EFD_NONBLOCK` eventfd (`detcore/tests/misc/notification_fds.rs:255`) to an epoll set with a timerfd, a signalfd and an inotify descriptor.
- T5 (covers F8): with `RLIMIT_NOFILE` lowered to the number of open descriptors, `syscall(SYS_eventfd, 0)` gives `EMFILE`. `ENFILE`, `ENODEV` and `ENOMEM` cannot be produced safely in a test; record them as untested.
- T6 (covers F9): read `/proc/self/fdinfo/<fd>` for a legacy eventfd after writing 5 and compare across runs. `eventfd-count` must show 5 and `eventfd-id` must be identical across runs.
  Related existing test: `guest_pipe_socket_and_anon_fdinfo_ignore_hermit_stdio_shape` at `hermit-cli/tests/procfs_determinism.rs:1460`, which checks that eventfd fdinfo keeps its `eventfd-count` line (`hermit-cli/tests/procfs_determinism.rs:1517`).
- T7 (covers F3, F4): record and replay two threads that pass a token back and forth through two blocking eventfds. Recording must finish, and replay must reproduce the output.
  Related existing test: `record_node_eventfd_epoll_sequence` at `hermit-cli/tests/record_replay.rs:1854`, which skips itself when `node` is not installed.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel keeps the counter; Detcore converts the legacy call to eventfd2 with flags 0, makes the descriptor nonblocking in the kernel in sequentialized run, and parks blocking reads and writes in its own scheduler); linux_determinism stays conditional; 9 flag behaviours and 7 tests listed; record/replay still uses the external blocking path, and no test issues syscall 284; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decision 14 (already realized in sequentialized run; record/replay gap kept); target_policy unchanged; no new F/T ids; no questions removed (record/replay internal-descriptor question not answered by decisions 6-15).

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `eventfd`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+eventfd&body=Discussion+for+the+%60eventfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Feventfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60eventfd%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `eventfd`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

History: notification-descriptor determinism was documented and tested in https://github.com/rrnewton/hermit/pull/549 (commit 3c49d197b4, "Document and verify notification fd determinism"), which added the `control-fds` scenario cited in T3.

### Open questions

- Should an eventfd count as a container-internal descriptor in record/replay, the way pipes do (`detcore/src/syscalls/helpers.rs:804`)? An eventfd created inside the guest has no outside writer unless it was passed out over a socket, so the deterministic retry path would apply. That is a design decision for the owner; T7 measures whether the current behaviour hangs.

### 2026-10-03 host strace evidence

Test host: Linux 7.1.3-0_fbk0_0_g295ad0959f34 x86_64; strace 6.12. A Python ctypes script issued raw `syscall(284, ...)` calls under strace (excerpt):

```text
eventfd(3)                      = 3
eventfd(4294967295)             = 4
eventfd2(0, EFD_CLOEXEC)        = 9
```

The third line is Python's `os.eventfd(0)`, which shows that the library wrapper issues `eventfd2`, not syscall 284.
