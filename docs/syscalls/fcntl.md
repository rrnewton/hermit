---
schema: syscall-pages/v1
syscall: fcntl
nr_x86_64: 72
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fcntl&body=Discussion+for+the+%60fcntl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffcntl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fcntl%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`fcntl(fd, cmd, arg)` is a multiplexer over one descriptor. Its commands duplicate descriptors, read and set descriptor and status flags, take POSIX and open-file-description record locks, route I/O signals, take leases, watch directories, size pipes, seal memfds and pass write-life hints. Detcore handles it (`detcore/src/syscalls/files.rs:3086`): it mirrors the descriptor-table commands into its own fd table, reports the logical `O_NONBLOCK` bit when it has forced sockets and pipes non-blocking, and refuses pipe growth past its pinned capacity. Every other command, including the blocking lock commands, goes to the kernel unchanged through the `[detcore-finishme]` default arm (`detcore/src/syscalls/files.rs:3179`).

Manual reference: Linux man-pages fcntl(2).

## Linux semantics

The descriptor flag (`FD_CLOEXEC`) belongs to the descriptor. Status flags, the access mode, OFD locks, leases, the owner of I/O signals and seals belong to the open file description or the inode, so `dup` and `fork` copies share them. POSIX record locks (`F_SETLK` family) belong to the process and the inode: any `close()` of any descriptor for that inode releases all of the process's locks on it, and they are not inherited across `fork`. OFD locks belong to the open file description and survive until its last descriptor closes.

On an `O_PATH` descriptor only `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD`, `F_GETFL` and the two query commands are allowed; everything else is `EBADF`. An unknown command is `EINVAL`.

Several commands make the kernel act later, outside the call: `O_ASYNC` with an owner delivers `SIGIO` (or the `F_SETSIG` signal) when I/O becomes possible, a lease delivers a signal when another process opens or truncates the file, and `F_NOTIFY` delivers a signal when a directory changes. `F_SETLKW` and `F_OFD_SETLKW` sleep in the kernel until the lock is granted, a signal interrupts them (`EINTR`), or deadlock detection fires (`EDEADLK`, POSIX locks only).

### Flags and options

- F1: `F_DUPFD` and `F_DUPFD_CLOEXEC`: return the lowest free descriptor at or above `arg`, sharing the open file description; `F_DUPFD_CLOEXEC` also sets `FD_CLOEXEC` on the copy. `EINVAL` when `arg` is negative or not below `RLIMIT_NOFILE`, `EMFILE` when none is free.
- F2: `F_GETFD` and `F_SETFD`: read or set `FD_CLOEXEC` on this descriptor only
- F3: `F_GETFL`: return the access mode and status flags of the open file description, including `O_NONBLOCK`, `O_APPEND`, `O_ASYNC`, `O_DIRECT`, `O_NOATIME` and the creation-time bits the kernel keeps (`O_LARGEFILE`, `O_PATH`)
- F4: `F_SETFL`: change only `O_APPEND`, `O_ASYNC`, `O_DIRECT`, `O_NOATIME` and `O_NONBLOCK`; other bits, including the access mode, are silently ignored. `EPERM` when setting `O_NOATIME` on a file the caller does not own.
- F5: `F_SETLK` and `F_GETLK`: non-blocking POSIX byte-range locks (`F_RDLCK`, `F_WRLCK`, `F_UNLCK` over `l_whence`, `l_start`, `l_len`). `F_SETLK` fails with `EAGAIN` or `EACCES` on conflict; `F_GETLK` reports the first conflicting lock, with the holder's pid in `l_pid` as seen from the caller's pid namespace (0 for a holder outside it), or sets `l_type` to `F_UNLCK`.
- F6: `F_SETLKW`: blocking POSIX lock. It sleeps until granted; `EINTR` on a signal, `EDEADLK` when the kernel detects a cycle among waiting processes.
- F7: `F_OFD_SETLK`, `F_OFD_SETLKW` and `F_OFD_GETLK`: open-file-description locks with the same ranges. `l_pid` must be 0 on input (`EINVAL` otherwise); `F_OFD_GETLK` reports `l_pid` as -1. `F_OFD_SETLKW` blocks like F6 but has no deadlock detection.
- F8: `F_SETOWN`, `F_GETOWN`, `F_SETOWN_EX` and `F_GETOWN_EX`: set or read the process, process group or thread (`F_OWNER_TID`, `F_OWNER_PID`, `F_OWNER_PGRP`) that receives `SIGIO` and `SIGURG` for this open file description
- F9: `F_SETSIG` and `F_GETSIG`: choose the signal sent for I/O readiness instead of `SIGIO`; a non-zero signal also passes `si_fd` and `si_band` to an `SA_SIGINFO` handler
- F10: `O_ASYNC` set through `F_SETFL` on a descriptor with an owner (F8): the kernel sends the F9 signal whenever input or output becomes possible, at a time set by I/O completion rather than by the guest
- F11: `F_SETLEASE` and `F_GETLEASE`: take, downgrade or release a read or write lease (`F_RDLCK`, `F_WRLCK`, `F_UNLCK`); when another process opens or truncates the file the kernel signals the holder and delays the opener for up to `/proc/sys/fs/lease-break-time` seconds
- F12: `F_NOTIFY`: dnotify on a directory descriptor (`DN_ACCESS`, `DN_MODIFY`, `DN_CREATE`, `DN_DELETE`, `DN_RENAME`, `DN_ATTRIB`, optionally `DN_MULTISHOT`); a change sends the F9 signal
- F13: `F_GETPIPE_SZ` and `F_SETPIPE_SZ`: read or change a pipe's capacity. The kernel rounds the request up to a power-of-two number of pages, returns `EPERM` above `/proc/sys/fs/pipe-max-size` for an unprivileged caller, and returns `EBUSY` when shrinking below the data already buffered.
- F14: `F_ADD_SEALS` and `F_GET_SEALS`: add or read `F_SEAL_SEAL`, `F_SEAL_SHRINK`, `F_SEAL_GROW`, `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` on a memfd; `EPERM` once `F_SEAL_SEAL` is set, `EBUSY` for `F_SEAL_WRITE` while a writable shared mapping exists, `EINVAL` on a file that does not support sealing
- F15: `F_GET_RW_HINT`, `F_SET_RW_HINT`, `F_GET_FILE_RW_HINT` and `F_SET_FILE_RW_HINT`: read or set the write-life hint (`RWH_WRITE_LIFE_*`) on the inode or the open file description
- F16: query commands added in Linux 6.10 and later: `F_DUPFD_QUERY` (does `arg` refer to the same open file description) and `F_CREATED_QUERY` (did the `open` that made this description create the file).
- F17: invalid use: a closed `fd` is `EBADF`; a command outside the `O_PATH` list on an `O_PATH` descriptor is `EBADF`; an unknown command, including kernel-internal numbers such as `F_CANCELLK` (1029) and the unimplemented `F_GETOWNER_UIDS` (17), is `EINVAL`
- F18: host-config `F_SETFL` failures: `EINVAL` when the host filesystem behind the descriptor rejects `O_DIRECT`, and `EPERM` when clearing `O_APPEND` on an inode the host marked append-only (setting that flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks).
- F19: host-config: on a kernel older than 6.10 the F16 query commands return `EINVAL`.

## Determinism analysis

`linux_determinism: conditional`. The descriptor-table and flag commands (F1 to F4, F8, F9, F13 to F16) return values fixed by the process's own descriptor state, with two exceptions: `F_SETPIPE_SZ` growth depends on the host sysctl `/proc/sys/fs/pipe-max-size`, and the descriptor numbers F1 returns depend on which numbers earlier calls left free, which is deterministic only if the guest's descriptor history is. The lock, lease, notification and asynchronous-signal commands (F5 to F7, F10 to F12) depend on other processes and on when the host completes I/O, so they repeat only when every contender is inside one deterministic schedule and nothing outside touches the file.

### Nondeterminism sources

- Competing lock holders, including processes outside the container: an `F_SETLK` answer and an `F_GETLK` report depend on who holds the range, and the `F_SETLKW` wake order depends on the host scheduler
- Signal timing: when `SIGIO`, a lease-break signal or a dnotify signal arrives depends on host I/O completion and on other processes; `EINTR` on a blocking lock depends on when a signal lands
- Host configuration: `pipe-max-size` for pipe growth, `lease-break-time` for leases, filesystem support for `O_DIRECT`, `O_NOATIME` ownership rules and sealing
- `F_GETLK` reports the holder's pid, which reflects real process creation order unless the pid namespace hides it

### Cross-host variation

- Filesystem type behind the descriptor: whether `F_SETFL(O_DIRECT)` succeeds or gives `EINVAL` (F18).
- Inode flags on host files the guest sees: an append-only flag set on the host makes clearing `O_APPEND` give `EPERM` (F18).
- Kernel version: `F_DUPFD_QUERY` and `F_CREATED_QUERY` (F16) work on 6.10 and later and give `EINVAL` before (F19).

## Hermit design

Target policy: `model`. The kernel performs every command; Detcore keeps the guest-visible fd table and flag state consistent with what it already models elsewhere, and replaces the answers that would leak host state. The handler at `detcore/src/syscalls/files.rs:3086` does this per command:

- F1: `F_DUPFD` and `F_DUPFD_CLOEXEC` (`detcore/src/syscalls/files.rs:3133`) run in the kernel and are mirrored with `dup_fd`, carrying the close-on-exec bit computed at `detcore/src/syscalls/files.rs:3092`.
- F2: `F_SETFD` is mirrored into Detcore's fd entry with `set_cloexec` (`detcore/src/syscalls/files.rs:3141`). `F_GETFD` goes to the kernel through the default arm.
- F3: `F_GETFL` (`detcore/src/syscalls/files.rs:3097`) takes the physical flags from the kernel and replaces the `O_NONBLOCK` bit with the logical one Detcore stored, so a guest never sees the non-blocking mode Detcore forced on its sockets, pipes and eventfds.
- F4: `F_SETFL` (`detcore/src/syscalls/files.rs:3109`) ORs `O_NONBLOCK` into the physical call when `use_nonblocking_sockets()` (`detcore-model/src/config.rs:872`) holds, record/replay is off, and the descriptor is a socket, pipe or eventfd. On success it stores the guest's own flags with `set_status_flags`, which keeps only the bits Linux lets `F_SETFL` change (`detcore/src/fd.rs:569`).
- F13: growth above `DETERMINISTIC_PIPE_CAPACITY_BYTES` (8192, `detcore/src/syscalls/files.rs:256`) is refused with `EPERM` before the kernel is asked (`detcore/src/syscalls/files.rs:3172`, boundary in `pipe_capacity_request_exceeds_ceiling` at `detcore/src/syscalls/files.rs:266`). The comment at `detcore/src/syscalls/files.rs:3148` (`TODO-HUMAN-REVIEW(PR-2568)`, https://github.com/rrnewton/hermit/pull/2568) explains that on a default host the same guest would otherwise get a 1 MiB pipe and on a hardened host `EPERM`. Shrinking and `F_GETPIPE_SZ` go to the kernel.
- Everything else, F5 to F12 and F14 to F17, reaches the default arm (`detcore/src/syscalls/files.rs:3179`), which logs `[detcore-finishme]: fcntl unhandled cases` and calls `record_or_replay` (`detcore/src/tool_local.rs:382`). In run mode that is the kernel call; under record/replay the recorder captures the result.

Why `model` and not the alternatives: the descriptor-table commands must be mirrored or Detcore's fd table diverges from the kernel's, which rules out `passthrough` (and `detcore_class` is `determinized`, so `passthrough` is not available). `emulate` would have to reimplement locks, leases and seals that the kernel already does correctly for a single deterministic schedule. `refuse` would break every program that calls `fcntl(fd, F_GETFL)` or `F_SETFD`.

What remains before the code fully realizes `model`:

- **Blocking locks (F6, F7).** No code in `detcore/src` handles `F_SETLKW` or `F_OFD_SETLKW`; both are forwarded as written. A contended request therefore sleeps inside the kernel, where the sequentialized scheduler cannot see it. The `flock` handler's doc comment records the consequence for the sibling call: forwarding "is what `fcntl` already does for POSIX record locks" (`detcore/src/syscalls/files.rs:1207`), the host-holder leak "is the same leak `fcntl` record locks have always had here" (`detcore/src/syscalls/files.rs:1225`), and a thread parked in a kernel lock wait wedged a four-way contention guest (`detcore/src/syscalls/files.rs:1237`). By that reasoning a contended `F_SETLKW` between two guest processes can hang the container; this is inferred from the code, not measured for `fcntl`. The `flock` approach, probing with the non-blocking form and refusing with `ENOLCK` on contention, would carry over directly.
- **Target (owner decision 14):** `F_SETLKW` and `F_OFD_SETLKW` become a non-blocking probe with `F_SETLK` or `F_OFD_SETLK` plus a scheduler wait: a contended probe parks the thread in Detcore's scheduler and retries at a scheduled point, so the lock is granted in schedule order, and a signal delivered while parked ends the wait with `EINTR` (F6, F7). Because no thread then waits inside the kernel, the kernel's POSIX deadlock check, which looks at blocked waiters, can no longer fire, so Detcore must model `EDEADLK` for a cycle of parked guest `F_SETLKW` waiters (F6); `F_OFD_SETLKW` has no deadlock detection on Linux and needs none. Today both commands reach the kernel unchanged through the default arm (`detcore/src/syscalls/files.rs:3179-3184`) and a contended request waits in the kernel holding the turn. `F_SETLKW` is too widely used to refuse (standing policy), so `target_policy` stays `model`; the `flock`-style `ENOLCK` refusal on contention would be an interim measure, not the target.
- **Asynchronous kernel signals (F10, F11, F12).** `O_ASYNC` is stored as a logical flag, but the kernel's `SIGIO`, lease-break and dnotify signals are delivered at host-chosen times that Detcore does not schedule. Whether Hermit should refuse these, model them, or accept them as outside events is an owner decision (see Open questions).
- **`F_GETLK` pid (F5).** The kernel reports `l_pid` in the guest's pid namespace (`unshare(CLONE_NEWUSER|CLONE_NEWPID)` at `hermit-cli/src/bin/hermit/container.rs:1404`); whether that number always equals the pid Detcore shows the guest elsewhere has not been checked.

`linux_determinism` stays `conditional`: Hermit removes the pipe-capacity source and the visible non-blocking leak, but the lock, lease and signal sources remain for anything outside one deterministic schedule.

## Recommended tests

- T1 (covers F1, F2): `F_DUPFD` and `F_DUPFD_CLOEXEC` return the lowest free descriptor at or above `arg` and share the file offset; `F_SETFD`/`F_GETFD` round-trip `FD_CLOEXEC`; the copy made by `F_DUPFD_CLOEXEC` closes across `execve` and the original does not; `F_DUPFD` with a negative `arg` gives `EINVAL`.
  Existing candidates: `tests/c/fd_duplication.c` (cell `c-programs/fd-duplication`, `tests/e2e/manifests/c-programs.yaml:15855`) and the lit test `detcore/tests/lit/fcntl_dupfd/main.rs`. `detcore/tests/lit/file_write_race/main.rs` also exercises `F_DUPFD_CLOEXEC`, but its `hermit run --strict --verify` line is marked FIXME and does not run.
- T2 (covers F3, F4): on a pipe and a socket under `--strict`, `F_GETFL` reports `O_NONBLOCK` clear until the guest sets it, set after `F_SETFL(O_NONBLOCK)`, and clear again after clearing it; `F_SETFL` with `O_RDWR` or `O_CREAT` leaves the access mode unchanged; `O_APPEND` set through `F_SETFL` makes writes land at the end.
  Existing candidates: `tests/c/append_pwrite.c` (cell `c-programs/append-pwrite`, `tests/e2e/manifests/c-programs.yaml:14856`) for `O_APPEND`, and `tests/c/ioctl_fionread.c` (cell `c-programs/ioctl-fionread`, `tests/e2e/manifests/c-programs.yaml:12139`), which cross-checks `O_NONBLOCK` through `F_GETFL`.
- T3 (covers F5, F7): non-blocking POSIX and OFD locks: `F_SETLK` and `F_OFD_SETLK` succeed uncontended, conflict across two processes with `EAGAIN`, and `F_GETLK`/`F_OFD_GETLK` report the holder (`l_pid` the holder's guest pid for POSIX, -1 for OFD); closing any descriptor drops POSIX locks but not OFD locks; `F_OFD_SETLK` with a non-zero `l_pid` gives `EINVAL`.
  Existing candidates: `tests/c/record_lock.c` (cell `c-programs/record-lock`, `tests/e2e/manifests/c-programs.yaml:16199`, single-process `F_SETLK`/`F_GETLK`) and `tests/c/proc_locks.c` (cell `c-programs/proc-locks`, `tests/e2e/manifests/c-programs.yaml:5943`, which selects `F_SETLK`, `lockf` or `F_OFD_SETLK` through `LOCK_API`).
- T4 (covers F6, F7): two guest processes contend with `F_SETLKW`, and separately with `F_OFD_SETLKW`; record whether Hermit hangs, returns, or refuses. A bounded timeout around the run is required. No existing test.
- T5 (covers F8, F9): owner and signal round-trips with `F_SETOWN`/`F_GETOWN`, `F_SETSIG`/`F_GETSIG` and `F_SETOWN_EX`/`F_GETOWN_EX`.
  Existing candidate: `tests/c/fcntl_owner.c` (cell `c-programs/fcntl-owner`, `tests/e2e/manifests/c-programs.yaml:16486`, verify on ptrace in CI). It registers owners but never delivers a signal.
- T6 (covers F10, F9): set `O_ASYNC` with an owner on a pipe, write to the other end from a second guest thread, and check under `run --verify` that the `SIGIO` (or `F_SETSIG` signal) arrives at the same point in both runs. No existing test.
- T7 (covers F11, F12): take a read lease and a dnotify watch inside the guest, trigger them from a second guest process, and record whether the signals arrive deterministically; `F_GETLEASE` reports the lease type. No existing test.
- T8 (covers F13): `F_GETPIPE_SZ` reports 8192 on a new pipe; shrinking to 4096 round-trips; growth above 8192 returns `EPERM` and leaves 8192.
  Existing candidates: `tests/c/pipe_capacity.c` (cell `c-programs/pipe-capacity`, `tests/e2e/manifests/c-programs.yaml:16062`), `tests/c/pipe_capacity_pin.c` (cell `c-programs/pipe-capacity-pin`, `tests/e2e/manifests/c-programs.yaml:16132`), and `a_guest_cannot_escape_the_deterministic_pipe_capacity_pin` at `hermit-cli/tests/cli.rs:8852`.
- T9 (covers F14): on a `memfd_create(MFD_ALLOW_SEALING)` descriptor, add each seal and read it back; `F_SEAL_SEAL` then makes `F_ADD_SEALS` fail with `EPERM`; a sealed `F_SEAL_GROW` file rejects `ftruncate` growth. No existing test.
- T10 (covers F15, F16, F17): `F_SET_RW_HINT`/`F_GET_RW_HINT` round-trip; `F_DUPFD_QUERY` on a `dup` copy returns 1 and on a separate open returns 0 (the older-kernel `EINVAL` is F19, recorded as U2 rather than tested); a closed fd gives `EBADF`; `F_SETLK` on an `O_PATH` descriptor gives `EBADF`; command 1029 and an unused number give `EINVAL`. No existing test.
- U1 (covers F18; host-config): the host's filesystem decides whether `O_DIRECT` is accepted, and only the host can mark an inode append-only, so CI cannot produce both errors on demand. Detcore forwards `F_SETFL` to the kernel and returns its errno unchanged (`detcore/src/syscalls/files.rs:3120`).
- U2 (covers F19; host-config): a CI host on Linux 6.10 or later cannot produce the older-kernel `EINVAL`. The query commands reach the kernel unchanged through the default arm (`detcore/src/syscalls/files.rs:3184`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel executes; Detcore mirrors dup/FD_CLOEXEC, reports logical O_NONBLOCK, refuses pipe growth past 8192; all other commands forwarded by the finishme arm); linux_determinism stays conditional; 17 flag behaviours and 10 tests listed; open questions on blocking locks and asynchronous signals recorded; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type, inode flags on host files, kernel version); O_DIRECT EINVAL and append-only EPERM split from F4 into new F18 (host-config U1); the pre-6.10 EINVAL split from F16 into new F19 (host-config U2); T10 no longer accepts EINVAL; no open question is resolved by decisions 1 or 2
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied (Target bullet: F_SETLKW and F_OFD_SETLKW become an F_SETLK/F_OFD_SETLK probe plus a scheduler wait, with Detcore modelling EDEADLK among parked POSIX waiters; today both are forwarded and block in the kernel); target_policy stays model; no new F/T ids; removed the F_SETLKW flock-treatment question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fcntl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fcntl&body=Discussion+for+the+%60fcntl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffcntl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fcntl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `fcntl`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`fcntl` is in Detcore's optional pass-through subscription list (`detcore/src/lib.rs:1131`), and its time cost is charged in the I/O group (`detcore/src/syscall_time.rs:83`).

### Open questions

- Should `O_ASYNC` with an owner, `F_SETLEASE` and `F_NOTIFY` be refused, modelled as scheduler events, or accepted as host-timed events? Each makes the kernel send a signal at a time Detcore does not choose.
- Does the `l_pid` that `F_GETLK` returns match the guest pid Detcore reports through `getpid` for the holder in every configuration? Not checked.
