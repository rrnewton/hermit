---
schema: syscall-pages/v1
syscall: timerfd_gettime
nr_x86_64: 287
linux_determinism: nondeterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timerfd_gettime&body=Discussion+for+the+%60timerfd_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimerfd_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timerfd_gettime%60%2C+or+in+what+the+page+says%3F%0A
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

`timerfd_gettime(fd, curr_value)` reports how long a timerfd has left before its next expiration, and its interval. Detcore makes the call a scheduling point and then sends it to the kernel unchanged (`detcore/src/syscalls/files.rs:4274`, through `notification_fd_control` at `detcore/src/syscalls/files.rs:4254`), so the result is the host timer's time left, not virtual time (`detcore/src/syscall_time.rs:189`).

Manual reference: Linux man-pages timerfd_create(2), which documents timerfd_gettime.

## Linux semantics

The kernel stores the timer's current setting in `*curr_value`. `it_value` is the time left until the next expiration, always relative, even when the timer was armed with `TFD_TIMER_ABSTIME`; it is computed from the timer's clock at the moment of the call. `it_interval` is the period set by `timerfd_settime`. A disarmed timer reports zero in both fields. The call does not change the timer or its expiration count.

For a periodic timer whose expirations have not been read, `it_value` is the time left until the next expiration, not the first one. A one-shot timer that has expired reports zero.

### Flags and options

- F1: armed one-shot timer: `it_value` is the time left before it expires, relative even for an absolute deadline, and `it_interval` is zero.
- F2: armed periodic timer: `it_value` is the time left before the next expiration, whether or not earlier expirations have been read, and `it_interval` is the period.
- F3: disarmed timer, or a one-shot timer that has already expired: both fields are zero.
- F4: the call has no side effect: the setting and the unread expiration count stay as they were.
- F5: `fd` is open but is not a timerfd: `EINVAL`. `fd` is not open: `EBADF`.
- F6: `curr_value` is not writable: `EFAULT`.

## Determinism analysis

`linux_determinism: nondeterministic`. For an armed timer the result is the distance between the deadline and the clock's current value, so it changes with when the call runs. Only the disarmed case and the error cases repeat exactly.

### Nondeterminism sources

- The current value of the timer's clock at the moment of the call
- For a periodic timer, how many periods have passed, which decides which expiration `it_value` counts down to
- For `CLOCK_REALTIME` timers armed with an absolute deadline, changes to the wall clock by other processes

## Hermit design

Target policy: `emulate`. The answer is a time, and under Hermit it must come from the virtual clock. Detcore should compute it from its own record of the timer (clock, deadline and interval, stored when `timerfd_settime` arms it) and the guest's virtual time at the call, without asking the host timer. Error cases (F5, F6) can still come from the kernel or be checked first by Detcore.

What the code does at this SHA:

- `handle_timerfd_gettime` calls `notification_fd_control` (`detcore/src/syscalls/files.rs:4279`), which takes a scheduler resource for the calling thread (`detcore/src/syscalls/files.rs:4260`) and runs the call through `record_or_replay` (`detcore/src/syscalls/files.rs:4261`). The kernel fills `curr_value` from the host timer.
- `detcore/src/syscall_time.rs:189` states the gap directly: timerfd calls pass through to a host timer (https://github.com/rrnewton/hermit/issues/1923), so `timerfd_gettime` reports host time left, not virtual time. The same list names the call among those whose result is a timer value (`detcore/src/syscall_time.rs:221`), and the call is charged the virtual cost of a clock read (`detcore/src/syscall_time.rs:33`).
- `/proc/self/fdinfo` gives the same information through its `it_value` line, which the sanitizer passes through unchanged (`detcore/src/procfs.rs:2831`).

Why `emulate` and not the alternatives:

- `passthrough`, today's behaviour, shows host time to a guest whose clocks run on virtual time. A program that compares `timerfd_gettime` with `clock_gettime` sees two unrelated clocks.
- `model`, with the kernel answering, cannot work while the kernel timer runs on host time. Even in a design where Detcore arms the kernel timer at the virtual deadline, the kernel would report time left on the host clock.
- `refuse` would break programs that query their own timers, which Linux always allows.

What remains before the code realizes `emulate`:

- Detcore does not record a timer's clock or deadline. `handle_timerfd_create` keeps only flags and type (`detcore/src/syscalls/files.rs:4241`), and `timerfd_settime` arms the host timer without recording anything. Both must change first; see the `timerfd_create` and `timerfd_settime` bodies.
- No test at this SHA calls `timerfd_gettime`.

Target (owner decisions 13 and 6): `it_value` is the gap between the timer's virtual deadline and the current value of its clock: virtual realtime for a `CLOCK_REALTIME` timer, virtual uptime for `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` (clock names as defined in the `clock_gettime` body). It is computed from Detcore's record of the timer, without asking the host. Today the kernel answers from the host timer (`detcore/src/syscalls/files.rs:4261`).

Kernel order (`do_timerfd_gettime` and `SYSCALL_DEFINE2(timerfd_gettime)` in `fs/timerfd.c`):

1. A descriptor that is not open gives `EBADF`.
2. One that is not a timerfd gives `EINVAL`.
3. Copying out to `curr_value` gives `EFAULT` (F5, F6).

An emulated answer must keep that order. Today the kernel applies it because the call is forwarded.

## Recommended tests

- T1 (covers F1, F4): arm a `CLOCK_MONOTONIC` timer for 100 ms, do a fixed amount of work or a 30 ms `nanosleep`, call `timerfd_gettime`, and print `it_value`. Under `hermit run --verify` on ptrace, the value must be identical across runs and equal 100 ms minus the virtual time that passed; a nonblocking `read` afterwards must still give `EAGAIN`. Repeat with `TFD_TIMER_ABSTIME` and check `it_value` is still relative.
- T2 (covers F2): arm a periodic timer with `it_value` and `it_interval` both 10 ms, sleep 25 ms of virtual time without reading, and check `it_value` is 5 ms and `it_interval` 10 ms on every run.
- T3 (covers F3): a newly created timer, a timer disarmed with a zero `it_value`, and a one-shot timer read after it expired all report zero in both fields.
- T4 (covers F5, F6): `EINVAL` for a descriptor that is not a timerfd, `EBADF` for a closed descriptor, and `EFAULT` for a null `curr_value`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> nondeterministic (the result is time left on the timer's clock); target_policy investigate -> emulate (answer from Detcore's record of the timer and virtual time; today the call is serialized and the kernel reports host time left); related gains https://github.com/rrnewton/hermit/issues/1923; 6 flag behaviours and 4 tests listed, none existing; the timerfd virtual-time design is an open question for the owner; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 and 6 (time left on the timer's virtual realtime or virtual uptime clock from Detcore's record; kernel order EBADF, EINVAL, EFAULT kept by any emulation); target_policy stays emulate; no new F/T ids; no questions removed (the timerfd mechanism question is not decided)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timerfd_gettime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timerfd_gettime&body=Discussion+for+the+%60timerfd_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimerfd_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timerfd_gettime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timerfd_gettime`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

History: https://github.com/rrnewton/hermit/pull/246 (commit fe22343195) added the timerfd virtual-time XFAIL test `detcore/tests/lit/timerfd_virtual_time/hermit-run-strict.lit`; it reads expirations, not `timerfd_gettime`. `detcore/src/syscall_time.rs:189` cites https://github.com/rrnewton/hermit/issues/1923 for timerfd calls still using a host timer.

### Open questions

- Which design should place timerfd expirations on the virtual clock (see the same question in the `timerfd_settime` body)? `emulate` here holds under either design, because the time left must be computed on the virtual clock, but the record Detcore keeps per timer depends on that choice. This is a design decision for the owner.

### 2026-10-03 host strace evidence

Test host: Linux 7.1.3-0_fbk0_0_g295ad0959f34 x86_64; strace 6.12. A Python ctypes script issued raw `syscall(287, ...)` calls under strace on a `CLOCK_MONOTONIC` timerfd (descriptor 8), standard input (descriptor 0) and a closed descriptor (excerpt):

```text
timerfd_gettime(8, NULL)        = -1 EFAULT (Bad address)
timerfd_gettime(0, 0x7f0e9bc29110) = -1 EINVAL (Invalid argument)
timerfd_gettime(999, 0x7f0e9bc29110) = -1 EBADF (Bad file descriptor)
```
