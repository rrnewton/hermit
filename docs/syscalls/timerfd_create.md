---
schema: syscall-pages/v1
syscall: timerfd_create
nr_x86_64: 283
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timerfd_create&body=Discussion+for+the+%60timerfd_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimerfd_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timerfd_create%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`timerfd_create(clockid, flags)` creates a disarmed timer that reports expirations through a file descriptor. Detcore sends the call to the kernel through `record_or_replay` and registers the result as a timerfd with the guest's `TFD_NONBLOCK` and `TFD_CLOEXEC` flags (`detcore/src/syscalls/files.rs:4235`). The creation itself does not read a clock, but the timer it creates runs on the host clock, not on Hermit's virtual clock (https://github.com/rrnewton/hermit/issues/1923).

Manual reference: Linux man-pages timerfd_create(2).

## Linux semantics

The call allocates an anonymous-inode file holding a timer bound to `clockid`. The timer starts disarmed; `timerfd_settime` arms it, `timerfd_gettime` reads the time left, and `read` returns the number of expirations since the last read. The clock cannot be changed later, so `clockid` decides for the timer's whole life whether it follows wall-clock time (`CLOCK_REALTIME`), time since boot excluding suspend (`CLOCK_MONOTONIC`), or including suspend (`CLOCK_BOOTTIME`). The two alarm clocks also wake a suspended system and need `CAP_WAKE_ALARM`.

`TFD_NONBLOCK` sets `O_NONBLOCK` on the new open file description, and `TFD_CLOEXEC` sets close-on-exec on the descriptor. The kernel checks `flags` before `clockid`, and every other bit gives `EINVAL`.

### Flags and options

- F1: `CLOCK_REALTIME`: the timer follows the settable wall clock. An absolute deadline moves when the clock is set.
- F2: `CLOCK_MONOTONIC`: the timer follows the non-settable monotonic clock, which stops during suspend.
- F3: `CLOCK_BOOTTIME`: like `CLOCK_MONOTONIC`, but it keeps counting during suspend.
- F4: `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM`: like the clocks above, but expiry wakes a suspended system. Without `CAP_WAKE_ALARM` the call fails with `EPERM`.
- F5: any other `clockid`, for example `CLOCK_PROCESS_CPUTIME_ID` or `CLOCK_TAI`: `EINVAL`.
- F6: `TFD_NONBLOCK`: a `read` on a timer with no expirations gives `EAGAIN` instead of waiting.
- F7: `TFD_CLOEXEC`: the descriptor is closed by `execve`.
- F8: any other bit in `flags`, for example `TFD_TIMER_ABSTIME` (1), which belongs to `timerfd_settime`: `EINVAL`.
- F9: descriptor identity: the lowest free descriptor number is returned, and `dup` and `fork` copies share one timer.
- F10: errors: `EMFILE`, `ENFILE`, `ENODEV` (no anonymous inode mount) and `ENOMEM`.

## Determinism analysis

`linux_determinism: conditional`. The call reads no clock and the timer starts disarmed, so with the same descriptor table, the same limits and the same capabilities, it returns the same result. It differs when:

- The descriptor table differs, which changes the number returned (F9), or the process or system file limits differ (F10).
- The capability set differs, which decides between success and `EPERM` for the alarm clocks (F4).
- The program reads host-assigned identity, such as the anonymous inode number from `fstat`.

The timer the call creates is where the real nondeterminism lies: once armed, it expires according to a host clock. That belongs to `timerfd_settime`, `timerfd_gettime` and `read`, and is analysed in those bodies.

### Nondeterminism sources

- Descriptor-table state and file limits
- `CAP_WAKE_ALARM` in the caller's capability set
- Kernel-assigned identity: the anonymous inode number

## Hermit design

Target policy: `model`. The kernel keeps the timer object, so `poll`, `epoll` and `read` keep working on a real descriptor. Detcore must know enough about each timer to place its expirations on the virtual clock, starting with the clock it was created on.

What the code does at this SHA:

- `handle_timerfd_create` sends the call to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:4240`) and registers the descriptor with the guest's `TFD_CLOEXEC` and `TFD_NONBLOCK` bits as a `FdType::Timerfd` (`detcore/src/syscalls/files.rs:4241`). Its doc comment states that the call exposes only kernel validation, guest-visible flags and a descriptor number, and does not read the clock (`detcore/src/syscalls/files.rs:4233`). The kernel's checks give F5, F8 and F10 their Linux results.
- The descriptor keeps the guest's `O_NONBLOCK` setting in the kernel. A `read` on a `TFD_NONBLOCK` timer therefore runs once (`detcore/src/syscalls/helpers.rs:775`). A `read` on a blocking timer takes the blocking strategy (`detcore/src/syscalls/helpers.rs:772`) and runs through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:250`), which lets the thread rejoin the run queue at a time the scheduler does not choose (`detcore/src/syscalls/helpers.rs:155`).
- The call is subscribed in record/replay (`detcore/src/lib.rs:1177`).

What remains before the code fully realizes `model`:

- **The clock is not recorded.** `add_fd` keeps only the flags and the descriptor type (`detcore/src/syscalls/files.rs:4241`), so Detcore cannot tell later whether a timer follows `CLOCK_REALTIME`, `CLOCK_MONOTONIC` or `CLOCK_BOOTTIME`. Placing expirations on the virtual clock, which `timerfd_settime` needs, starts with storing the clock here.
- **The timer runs on host time.** The XFAIL test `detcore/tests/lit/timerfd_virtual_time/hermit-run-strict.lit` records that `timerfd_settime` arms a host-kernel timer while `CLOCK_MONOTONIC` is virtualized, so a blocking read returns before the guest's virtual deadline (lines 1-4). The fix belongs to `timerfd_settime`, but the clock it needs is chosen here.
- **The alarm clocks (F4)** depend on a host capability. Hermit should decide whether a guest sees `EPERM` or success independently of the host's capability set; at this SHA it sees whatever the host kernel returns.

Target (owner decisions 13, 14 and 6): a timerfd is a virtual timer on the clock named at creation. The clock names are those defined in the `clock_gettime` body.

- `handle_timerfd_create` must record `clockid` with the descriptor. Today `add_fd` keeps only the flags and the type (`detcore/src/syscalls/files.rs:4241`).
- A `CLOCK_REALTIME` timer then measures virtual realtime. A `CLOCK_MONOTONIC` or `CLOCK_BOOTTIME` timer measures virtual uptime.
- Under decision 14, a blocking `read` becomes a non-blocking probe plus a scheduler wait for the timer's virtual deadline. Today it is the `record_or_replay_blocking` path (`detcore/src/syscalls/helpers.rs:250`, chosen at `detcore/src/syscalls/helpers.rs:772`).
- The mechanism, kernel timer kept for readiness or not, is still the open question below.

Kernel order (`SYSCALL_DEFINE2(timerfd_create)` in `fs/timerfd.c`):

1. A stray `flags` bit or a clock outside the five accepted ones gives `EINVAL` (F5, F8).
2. An alarm clock without `CAP_WAKE_ALARM` in the initial user namespace gives `EPERM` (F4).
3. Descriptor allocation errors come last (F10).

The kernel gives this order today because the call is forwarded (`detcore/src/syscalls/files.rs:4240`). Any design that stops forwarding owes it under decision 6. Whether the alarm result should stop depending on the host capability stays open. The sibling clock bodies give `EPERM` on every host when a guest arms an alarm clock.

## Recommended tests

- T1 (covers F1, F2, F3, F6, F7, F9): create one timer for each of `CLOCK_REALTIME`, `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` with every combination of `TFD_NONBLOCK` and `TFD_CLOEXEC`. Check the descriptor numbers, `F_GETFD` and `F_GETFL`, that a nonblocking `read` on a disarmed timer gives `EAGAIN`, and that `/proc/self/fdinfo` shows the right `clockid`. Run under `hermit run --verify` on ptrace.
  Existing candidate for one case: the `control-fds` scenario of `tests/c/epoll_determinism.c`, which creates a `CLOCK_MONOTONIC` timer with `TFD_CLOEXEC | TFD_NONBLOCK` and checks both flags (`tests/c/epoll_determinism.c:353`), run by `notification_control_syscalls_are_deterministic` at `hermit-cli/tests/epoll_determinism.rs:399`.
- T2 (covers F4, F5, F8): check `EINVAL` for `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_TAI` and a `flags` value of 1, and record what `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM` return. Without `CAP_WAKE_ALARM` both must give `EPERM`, and the result must be identical across runs.
- T3 (covers F10): with `RLIMIT_NOFILE` lowered to the number of open descriptors, the call gives `EMFILE`. `ENFILE`, `ENODEV` and `ENOMEM` cannot be produced safely in a test; record them as untested.
- T4 (covers F2, F6): create a `CLOCK_MONOTONIC` timer, arm it for a short interval, and read its expirations; the output must be identical across runs.
  Existing candidate: `timerfd_expiry_is_deterministic` at `detcore/tests/misc/notification_fds.rs:307`, which arms a `TFD_NONBLOCK` timer for 1 ns and reads it in an `EAGAIN` and `sched_yield` loop, five times under Detcore. The XFAIL `detcore/tests/lit/timerfd_virtual_time/hermit-run-strict.lit` covers the blocking case and is expected to fail at this SHA.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel keeps the timer object; Detcore registers the descriptor but does not record the clock, which virtual-time arming needs); linux_determinism stays conditional; related gains https://github.com/rrnewton/hermit/issues/1923 (timerfd uses a host timer); 10 flag behaviours and 4 tests listed; the timerfd virtual-time design is an open question for the owner; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13, 14 and 6 (record clockid so the timer runs on virtual realtime or virtual uptime; blocking read becomes probe plus scheduler wait instead of record_or_replay_blocking; kernel order EINVAL, EPERM, allocation errors); target_policy stays model; no new F/T ids; no questions removed (mechanism and alarm-capability questions are not decided)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timerfd_create`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timerfd_create&body=Discussion+for+the+%60timerfd_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimerfd_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timerfd_create%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timerfd_create`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

History:

- https://github.com/rrnewton/hermit/pull/246 (commit fe22343195, "Add timerfd virtual-time XFAIL coverage") added `detcore/tests/lit/timerfd_virtual_time/`, which expects a blocking read on a 10 ms timer to return only after 10 ms of virtual `CLOCK_MONOTONIC` time and is marked XFAIL.
- https://github.com/rrnewton/hermit/pull/549 (commit 3c49d197b4, "Document and verify notification fd determinism") added the `control-fds` scenario cited in T1.
- `detcore/src/syscall_time.rs:189` cites https://github.com/rrnewton/hermit/issues/1923 for timerfd calls still using a host timer.

### Open questions

- Which design should place timerfd expirations on the virtual clock? One option keeps the kernel timer for readiness and has Detcore answer `read` and `timerfd_gettime` from the virtual clock; another has Detcore arm the kernel timer only when the virtual deadline is reached in the schedule, so readiness and `read` stay consistent. Coordinator notes (not verified at this SHA; no commit in its history refers to it) describe an earlier pull request, https://github.com/rrnewton/hermit/pull/1213, that tried the first option and was held back because `poll`/`epoll` readiness and `read` disagreed. This is a design decision for the owner and affects all three timerfd syscalls.
- Should a guest's `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM` result depend on the host's `CAP_WAKE_ALARM`?

### 2026-10-03 host strace evidence

Test host: Linux 7.1.3-0_fbk0_0_g295ad0959f34 x86_64; strace 6.12. A Python ctypes script issued raw `syscall(283, ...)` calls under strace (excerpt), not as root:

```text
timerfd_create(CLOCK_MONOTONIC, 0) = 6
timerfd_create(CLOCK_MONOTONIC, TFD_TIMER_ABSTIME) = -1 EINVAL (Invalid argument)
timerfd_create(CLOCK_PROCESS_CPUTIME_ID, 0) = -1 EINVAL (Invalid argument)
timerfd_create(CLOCK_BOOTTIME, 0) = 7
timerfd_create(CLOCK_REALTIME_ALARM, 0) = -1 EPERM (Operation not permitted)
timerfd_create(CLOCK_BOOTTIME_ALARM, 0) = -1 EPERM (Operation not permitted)
timerfd_create(CLOCK_TAI, 0)    = -1 EINVAL (Invalid argument)
```
