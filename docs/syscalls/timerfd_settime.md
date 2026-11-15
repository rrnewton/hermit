---
schema: syscall-pages/v1
syscall: timerfd_settime
nr_x86_64: 286
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timerfd_settime&body=Discussion+for+the+%60timerfd_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimerfd_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timerfd_settime%60%2C+or+in+what+the+page+says%3F%0A
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

`timerfd_settime(fd, flags, new_value, old_value)` arms, re-arms or disarms a timerfd, and can return the previous setting. Detcore makes the call a scheduling point and then sends it to the kernel unchanged (`detcore/src/syscalls/files.rs:4265`, through `notification_fd_control` at `detcore/src/syscalls/files.rs:4254`). The kernel therefore arms a host timer, so expirations follow host time while the guest's clocks follow virtual time (https://github.com/rrnewton/hermit/issues/1923).

Manual reference: Linux man-pages timerfd_create(2), which documents timerfd_settime.

## Linux semantics

`new_value->it_value` sets the first expiration and `new_value->it_interval` the period after it. Without `TFD_TIMER_ABSTIME`, `it_value` is relative to the current value of the timer's clock; with it, `it_value` is an absolute time on that clock, and a time already past expires at once. An `it_value` of zero disarms the timer.

Every call resets the timer's expiration count to zero, so expirations that were not read are lost, and a thread blocked in `read` waits for the new deadline. If `old_value` is not null, the kernel stores the previous setting there: the time that was left before the next expiration, measured on the clock at the moment of the call, and the previous interval.

`TFD_TIMER_CANCEL_ON_SET` matters only for a `CLOCK_REALTIME` or `CLOCK_REALTIME_ALARM` timer armed with `TFD_TIMER_ABSTIME`: if the wall clock is then set, a `read` fails with `ECANCELED`. On other timers the kernel accepts the flag and ignores it (host probe: accepted on a `CLOCK_MONOTONIC` timer).

### Flags and options

- F1: `flags` = 0: `it_value` is relative to the current value of the timer's clock.
- F2: `TFD_TIMER_ABSTIME`: `it_value` is an absolute time on the timer's clock. A time already past expires at once.
- F3: `TFD_TIMER_CANCEL_ON_SET`, with `TFD_TIMER_ABSTIME` on a `CLOCK_REALTIME` or `CLOCK_REALTIME_ALARM` timer: a later change to the wall clock makes `read` fail with `ECANCELED`. On any other timer, or without `TFD_TIMER_ABSTIME`, the flag is accepted and has no effect.
- F4: a non-zero `it_interval`: after the first expiration the timer expires periodically, and `read` returns the number of expirations since the last read.
- F5: `it_value` of zero: disarm the timer, whatever `it_interval` holds.
- F6: re-arming: every call resets the unread expiration count to zero and moves the deadline seen by threads blocked in `read`, `poll` or `epoll`.
- F7: `old_value` not null: receives the previous setting, with the time that was left measured at the moment of the call. Null skips this.
- F8: invalid arguments: any other bit in `flags`, or a `tv_nsec` outside 0 to 999999999 or a negative `tv_sec` in either field, gives `EINVAL`.
- F9: `fd` is open but is not a timerfd: `EINVAL`. `fd` is not open: `EBADF`.
- F10: `new_value` is not readable, or `old_value` is not writable: `EFAULT`.

## Determinism analysis

`linux_determinism: nondeterministic`. The call reads the timer's clock on every successful use: a relative `it_value` starts counting from the current time, and `old_value` reports the time left at the moment of the call. Both depend on when the call runs. The expirations it schedules then reach `read`, `poll` and `epoll` at host times, and the number of expirations a periodic timer accumulates before a read depends on how long the reader took. Only the argument errors (F8 to F10) and disarming with a null `old_value` repeat exactly.

### Nondeterminism sources

- The current value of the timer's clock, which sets relative deadlines and the `old_value` result
- When the timer expires relative to the program's other work, which decides `read` and readiness results
- The number of periodic expirations that accumulate before a read
- For `CLOCK_REALTIME` timers, changes to the wall clock by other processes, which move absolute deadlines and trigger `ECANCELED`

## Hermit design

Target policy: `model`. The kernel timerfd stays the object that `poll`, `epoll` and `read` see, but Detcore must own the deadline: it should compute it on the guest's virtual clock, deliver the expiration at the matching point of the virtual schedule, and answer `old_value` from its own record of the timer.

What the code does at this SHA:

- `handle_timerfd_settime` calls `notification_fd_control` (`detcore/src/syscalls/files.rs:4270`), which takes a scheduler resource for the calling thread (`detcore/src/syscalls/files.rs:4260`) and then runs the call through `record_or_replay` (`detcore/src/syscalls/files.rs:4261`). The order of `timerfd_settime` calls among threads is therefore fixed by the scheduler, but the arguments go to the kernel unchanged.
- The kernel arms a host timer. `detcore/src/syscall_time.rs:189` records that timerfd calls still pass through to a host timer and cites https://github.com/rrnewton/hermit/issues/1923. The XFAIL test `detcore/tests/lit/timerfd_virtual_time/hermit-run-strict.lit` records the consequence: a blocking read on a 10 ms timer returns after 10 ms of host time, before the guest's virtual deadline (lines 1-4).
- An absolute deadline (F2) that the guest computed from its virtual clock is compared with the host clock. Depending on how far the two clocks are apart, it expires at once or much later than the guest intended.
- `old_value` (F7) is written by the kernel with host time left.
- The fdinfo sanitizer passes the `ticks`, `settime flags`, `it_value` and `it_interval` lines through unchanged (`detcore/src/procfs.rs:2829`), so `/proc/self/fdinfo` shows the host timer's state.
- Detcore charges the call the virtual cost it uses for descriptor creation (`detcore/src/syscall_time.rs:111`).

Why `model` and not the alternatives:

- `passthrough` is what the code does today, and it lets host time decide when the guest sees an expiration.
- `emulate`, a timer kept entirely in Detcore with no kernel object, would also have to emulate every `poll`, `epoll`, `select` and `read` path on the descriptor. Keeping the kernel object and controlling when it becomes ready is smaller.
- `refuse` would break event loops (glib, libuv, systemd) that use timerfds for timeouts.

What remains before the code fully realizes `model`:

- Record each timer's clock at creation; `handle_timerfd_create` does not keep it (`detcore/src/syscalls/files.rs:4241`).
- Translate relative and absolute deadlines to the virtual clock, and make the timer ready at the matching virtual time, including periodic expirations (F4) and the reset on re-arm (F6).
- Fill `old_value` from Detcore's record of the timer instead of the host timer.
- Decide how `TFD_TIMER_CANCEL_ON_SET` (F3) interacts with a virtual `CLOCK_REALTIME`; the guest cannot set the host wall clock, so `ECANCELED` should come only from a change Hermit makes to the virtual clock.
- Report Detcore's view of the timer in fdinfo instead of the host's.

Target (owner decisions 13, 14 and 6): the timer is armed on its recorded virtual clock. The clock names are those defined in the `clock_gettime` body.

- A relative `it_value` counts from the current value of that clock.
- With `TFD_TIMER_ABSTIME`, a `CLOCK_REALTIME` timer takes `it_value` as virtual realtime, and a `CLOCK_MONOTONIC` or `CLOCK_BOOTTIME` timer takes it as virtual uptime, so the deadline adds the virtual boot instant. A deadline the guest computed from its own `clock_gettime` reading then lands where the guest intended (F2).
- `old_value` reports the time left on the same virtual clock (F7).
- Expirations reach `read`, `poll` and `epoll` at the matching point of the virtual schedule. Under decision 14 a blocking `read` is a non-blocking probe plus a scheduler wait for that deadline; the `timerfd_create` body describes today's blocking read.

Today the arguments reach a host timer unchanged (`detcore/src/syscalls/files.rs:4270`, through `record_or_replay` at `detcore/src/syscalls/files.rs:4261`). So relative deadlines count host time, and absolute deadlines are compared with the host clock.

Kernel order (`SYSCALL_DEFINE4(timerfd_settime)` and `do_timerfd_settime` in `fs/timerfd.c`):

1. Copying `new_value` in gives `EFAULT` (F10).
2. An unknown bit in `flags`, or an invalid `timespec` in either field, gives `EINVAL` (F8).
3. A descriptor that is not open gives `EBADF`, and one that is not a timerfd gives `EINVAL` (F9).
4. An alarm timer without `CAP_WAKE_ALARM` gives `EPERM`. Under Hermit this follows whatever `timerfd_create` decides for alarm clocks.
5. The timer is changed.
6. Only then does copying out to `old_value` give `EFAULT`, with the new setting kept (F10).

An emulated or modelled call must keep that order. Today the kernel applies it because the call is forwarded.

## Recommended tests

- T1 (covers F1, F6): under `hermit run --verify` on ptrace, arm a blocking `CLOCK_MONOTONIC` timer for 10 ms, read it, and check that the virtual `CLOCK_MONOTONIC` has advanced by at least 10 ms; then arm it for 1 s and immediately re-arm it for 10 ms, and check that a read returns 1 expiration after about 10 ms of virtual time.
  Existing candidate: `detcore/tests/lit/timerfd_virtual_time/main.c`, run by `detcore/tests/lit/timerfd_virtual_time/hermit-run-strict.lit` and marked XFAIL at this SHA. It checks the first half; the re-arm half needs a new test.
- T2 (covers F2): arm a timer with `TFD_TIMER_ABSTIME` at the guest's virtual `CLOCK_MONOTONIC` now plus 10 ms, read it, and check that the guest's clock has passed the deadline. Also arm one with an absolute time already past and check it expires at once.
  Existing candidate for the past-deadline case: the `mixed` scenario of `tests/c/epoll_determinism.c`, which arms a timer at absolute `CLOCK_MONOTONIC` 1 s (`tests/c/epoll_determinism.c:272`), run by `mixed_fd_readiness_is_deterministic` at `hermit-cli/tests/epoll_determinism.rs:387`.
- T3 (covers F3): `TFD_TIMER_CANCEL_ON_SET` is accepted on a `CLOCK_MONOTONIC` timer and on a relative `CLOCK_REALTIME` timer, and an absolute `CLOCK_REALTIME` timer armed with it does not report `ECANCELED` when nothing changes the guest's wall clock.
- T4 (covers F4, F5): arm a periodic timer with `it_value` and `it_interval` both 5 ms, wait 22 ms of virtual time using `nanosleep`, and check that `read` returns 4 expirations on every run; then disarm it with a zero `it_value` and check that a nonblocking read gives `EAGAIN`.
- T5 (covers F7): arm a timer for 100 ms, do a fixed amount of work, re-arm it, and print `old_value`. The value must be identical across runs and match the virtual time that passed.
- T6 (covers F8, F9, F10): check `EINVAL` for `flags` 4, for `tv_nsec` 1000000000 and for a negative `tv_sec`; `EINVAL` for a descriptor that is not a timerfd; `EBADF` for a closed descriptor; and `EFAULT` for a null `new_value`.
- T7 (covers F1, F4): a thread waiting in `epoll_wait` on a timerfd armed for 40 ms, alongside other timer families in another thread, must produce the same event order on every run.
  Existing candidate: `tests/c/timer_family_identity.c` (cell `c-programs/timer-family-identity`, `tests/e2e/manifests/c-programs.yaml:17629`), whose second thread arms a 40 ms `CLOCK_MONOTONIC` timerfd (`tests/c/timer_family_identity.c:97`). The cell is registered on ptrace with `ci: false`, so it does not run in CI.
- T8 (covers F8, F9, F10): check the kernel order with a raw `syscall(SYS_timerfd_settime, ...)`. A null `new_value` on a closed descriptor gives `EFAULT`; `flags` 4 on a closed descriptor gives `EINVAL`; a valid call with `old_value` on a read-only page gives `EFAULT`, and `timerfd_gettime` then shows the new setting, which still expires. Run it under `hermit run --verify` on ptrace and compare with the host.
- T9 (covers F2, F7): arm a `CLOCK_REALTIME` timer with `TFD_TIMER_ABSTIME` at the guest's virtual `CLOCK_REALTIME` reading plus 10 ms, and a `CLOCK_BOOTTIME` timer the same way from a `CLOCK_BOOTTIME` reading. Neither may expire at once, and each read must return 1 after 10 ms of virtual time. Then re-arm one before it expires and check that `old_value` equals 10 ms minus the virtual time that passed, on every run.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> nondeterministic (relative deadlines and old_value read the timer's clock); target_policy model (keep the kernel timerfd, but Detcore must own the deadline on the virtual clock; today the call is serialized and then arms a host timer); related gains https://github.com/rrnewton/hermit/issues/1923; 10 flag behaviours and 7 tests listed; the virtual-time design is an open question for the owner; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13, 14 and 6 (arm on the recorded virtual realtime or virtual uptime clock, absolute MONOTONIC/BOOTTIME deadlines add the boot instant, old_value from virtual time, blocking read as probe plus scheduler wait; kernel order EFAULT, EINVAL, EBADF/EINVAL, EPERM, change, old_value EFAULT); target_policy stays model; new T8, T9; no questions removed (mechanism and ECANCELED questions are not decided)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timerfd_settime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timerfd_settime&body=Discussion+for+the+%60timerfd_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimerfd_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timerfd_settime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timerfd_settime`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

History:

- https://github.com/rrnewton/hermit/pull/246 (commit fe22343195, "Add timerfd virtual-time XFAIL coverage") added the XFAIL test cited in T1.
- https://github.com/rrnewton/hermit/pull/549 (commit 3c49d197b4, "Document and verify notification fd determinism") added the scenarios cited in T2.
- `detcore/src/syscall_time.rs:189` cites https://github.com/rrnewton/hermit/issues/1923 for timerfd calls still using a host timer.

### Open questions

- Which design should place timerfd expirations on the virtual clock? One option keeps the host timer for readiness and has Detcore answer `read` and `timerfd_gettime` from the virtual clock; another has Detcore arm the kernel timer, with a zero or past deadline, only when the virtual deadline is reached in the schedule, so readiness and `read` stay consistent. Coordinator notes (not verified at this SHA; no commit in its history refers to it) describe an earlier pull request, https://github.com/rrnewton/hermit/pull/1213, that tried the first option and was held back because `poll`/`epoll` readiness and `read` disagreed. This is a design decision for the owner and affects all three timerfd syscalls.
- Should `TFD_TIMER_CANCEL_ON_SET` ever produce `ECANCELED` under Hermit, given that the guest cannot set the host clock?

### 2026-10-03 host strace evidence

Test host: Linux 7.1.3-0_fbk0_0_g295ad0959f34 x86_64; strace 6.12. A Python ctypes script issued raw `syscall(286, ...)` calls under strace on a `CLOCK_MONOTONIC` timerfd (descriptor 8) and on standard input (descriptor 0) (excerpt):

```text
timerfd_settime(8, 0x4 /* TFD_??? */, {it_interval={tv_sec=0, tv_nsec=0}, it_value={tv_sec=0, tv_nsec=0}}, NULL) = -1 EINVAL (Invalid argument)
timerfd_settime(8, TFD_TIMER_CANCEL_ON_SET, {it_interval={tv_sec=0, tv_nsec=0}, it_value={tv_sec=0, tv_nsec=0}}, NULL) = 0
timerfd_settime(8, TFD_TIMER_ABSTIME|TFD_TIMER_CANCEL_ON_SET, {it_interval={tv_sec=0, tv_nsec=0}, it_value={tv_sec=0, tv_nsec=0}}, NULL) = 0
timerfd_settime(8, 0, {it_interval={tv_sec=0, tv_nsec=0}, it_value={tv_sec=0, tv_nsec=1000000000}}, NULL) = -1 EINVAL (Invalid argument)
timerfd_settime(8, 0, NULL, NULL) = -1 EFAULT (Bad address)
timerfd_settime(0, 0, {it_interval={tv_sec=0, tv_nsec=0}, it_value={tv_sec=0, tv_nsec=0}}, NULL) = -1 EINVAL (Invalid argument)
```
