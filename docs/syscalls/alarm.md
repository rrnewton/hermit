---
schema: syscall-pages/v1
syscall: alarm
nr_x86_64: 37
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+alarm&body=Discussion+for+the+%60alarm%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Falarm.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60alarm%60%2C+or+in+what+the+page+says%3F%0A
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

`alarm(seconds)` arranges for `SIGALRM` to be sent to the process after `seconds` of real time, replacing any earlier alarm, and returns the whole seconds that were left on the earlier one. `alarm(0)` cancels. Detcore handles it in `handle_alarm` (`detcore/src/syscalls/signal.rs:265`): the alarm is a timer in Detcore's virtual time, kept per process by the deterministic scheduler, and fired by it. Hermit rounds the returned remainder up (`detcore/src/syscalls/signal.rs:216`), while Linux rounds to the nearest second; see Hermit design.

Manual reference: Linux man-pages alarm(2).

## Linux semantics

There is one real-time timer per process, shared with `setitimer(ITIMER_REAL)`. `alarm(n)` with `n > 0` sets it to fire once after `n` seconds of wall-clock time, with no reload interval, replacing whatever was armed, including a periodic `setitimer` timer. `alarm(0)` disarms it. Either way, the return value is the time that was left on the previous timer, in whole seconds, or 0 if none was armed.

The kernel computes that return value in `alarm_setitimer` (`kernel/time/itimer.c`): it rounds to the nearest second, with half a second or more rounding up, and returns at least 1 if any time at all was left, so a pending alarm never reports 0.

When the timer fires, `SIGALRM` is sent to the process as a whole and is handled by any thread that does not block it. Its default action terminates the process. A child created by `fork` starts with no alarm. An alarm survives `execve`. The call cannot fail.

### Flags and options

- F1: `alarm(n)`, `n > 0`, with no timer armed: arm a one-shot timer for `n` seconds; returns 0
- F2: `alarm(n)`, `n > 0`, with a timer armed: replace it, including a periodic `setitimer(ITIMER_REAL)` timer, whose interval is dropped; returns the old remaining time in seconds
- F3: `alarm(0)`: cancel any armed timer; returns the old remaining time, or 0
- F4: the returned remainder is rounded to the nearest second (half a second or more rounds up), and is at least 1 whenever a timer was armed
- F5: the timer is shared with `setitimer(ITIMER_REAL)` and `getitimer(ITIMER_REAL)`: each sees the state the others set
- F6: expiry sends `SIGALRM` to the process; a thread that does not block it handles it, and with the default disposition the process terminates
- F7: the timer belongs to the process: a `fork` child starts with none, and `execve` keeps it

## Determinism analysis

`linux_determinism: nondeterministic`, changed from the rendered `conditional`. The timer counts wall-clock time, so the point in the program where `SIGALRM` arrives differs between runs, and so does the remainder returned by a second `alarm` call (F2, F3). Only `alarm(n)` with nothing armed (F1) repeats on its own.

## Hermit design

Target policy: `model`. The alarm is a virtual-time timer in Detcore's scheduler. When threads are sequentialized (`config.sequentialize_threads`, set at `hermit-cli/src/bin/hermit/run.rs:3667`), the handler at `detcore/src/syscalls/signal.rs:265` does this:

- It sends `register_alarm(seconds, interval 0, SIGALRM)` to the global scheduler (`detcore/src/syscalls/signal.rs:273`; RPC at `detcore/src/tool_global.rs:3951`). On ptrace, `replace_real_timer` (`detcore/src/scheduler/parked.rs:555`) calls the scheduler's `register_alarm` (`detcore/src/scheduler.rs:5772`).
- There, a zero duration removes the process's alarm, and any other value inserts a deadline at virtual now plus the duration with a zero interval, replacing the earlier entry. The timer is keyed by process id, `SignalTimerId::Alarm(detpid)` (`insert_alarm`, `detcore/src/scheduler/timed_waiters.rs:136`). The old remaining time is returned, or zero if none was armed. This covers F1, F2 and F3. `setitimer(ITIMER_REAL)` goes through the same function, and `getitimer` reads the same entry, which covers F5.
- When virtual time reaches the deadline, `fire_alarm` (`detcore/src/scheduler.rs:3077`) picks a target thread in the process with `select_signal_target` (`detcore/src/scheduler.rs:3100`) and delivers the signal (F6).
- A `fork` child has a new process id, so it has no alarm; the timer is dropped only when the process logically exits (`detcore/src/scheduler.rs:2352`), not at exec (F7).
- Without sequentialization, the call goes to the kernel unchanged (`detcore/src/syscalls/signal.rs:284`).

Why `model`: a host alarm would fire at a wall-clock moment and destroy reproducibility. `emulate` without a timer would never deliver the signal. `refuse` would break the many programs that use `alarm` as a watchdog.

**Fidelity gap, F4.** The remainder is converted by `logical_time_to_alarm_seconds` (`detcore/src/syscalls/signal.rs:216`), which is `as_nanos().div_ceil(1_000_000_000)`: it always rounds up. Linux rounds to the nearest second. With 1.3 seconds left, Linux returns 1 and Hermit returns 2. The 1-second minimum matches. The unit test `alarm_remaining_seconds_round_up` (`detcore/src/syscalls/signal.rs:903`) pins the round-up behaviour, including 1.000000001 seconds giving 2 where Linux gives 1. The values are deterministic under Hermit, but they differ from what the same program sees natively.

## Recommended tests

- T1 (covers F1, F6): with a `SIGALRM` handler installed, `alarm(1)` returns 0, and the handler interrupts a following `pause()`, which returns `EINTR`.
  Existing candidates: `tests/c/pause_alarm_interrupt.c` (cell `c-programs/pause-alarm-interrupt`, `tests/e2e/manifests/c-programs.yaml:4715`, verify ci on ptrace), which does not check the return value of `alarm`; `tests/c/signal_determinism.c:176`, which asserts `alarm(1) != 0` is false before a blocking read (cell `c-programs/signal-determinism`, `tests/e2e/manifests/c-programs.yaml:8132`, verify ci on ptrace); and `tests/c/hello_alarm.c` (cell `c-programs/hello-alarm`, `tests/e2e/manifests/c-programs.yaml:2105`).
- T2 (covers F2, F3, F4): `alarm(5)`, then a 1.7-second `clock_nanosleep`, then `alarm(10)` must return 3 as on Linux (3.3 seconds were left; the current round-up gives 4), then `alarm(0)` must return 10. Use remainders whose fraction is below half a second so the test fails under the current round-up.
- T3 (covers F2, F5): arm `setitimer(ITIMER_REAL, {{1, 0}, {3, 0}})`, then `alarm(5)` returns 3. `getitimer(ITIMER_REAL)` must then show a zero interval and at most 5 seconds left, and only one `SIGALRM` must arrive over the next 8 seconds.
- T4 (covers F6): with `SIGALRM` at its default disposition, `alarm(1)` followed by `pause()` ends the process with `SIGALRM`, with the same wait status in every run.
- T5 (covers F6): in a process with two threads, one blocking `SIGALRM`, the handler must run on the other thread, the same thread in every run.
- T6 (covers F7): `alarm(2)` then `fork`: the child's `alarm(0)` returns 0 while the parent's returns the remaining time. `alarm(2)` then `execve` of a helper whose first `alarm(0)` returns 2.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (a per-process virtual-time ITIMER_REAL in Detcore's scheduler, shared with setitimer and getitimer); linux_determinism conditional -> nondeterministic; 7 flag behaviours and 6 tests listed; fidelity gap: the returned remainder rounds up (div_ceil) where Linux rounds to the nearest second, left as an open question; no coverage token yet, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `alarm`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+alarm&body=Discussion+for+the+%60alarm%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Falarm.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60alarm%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `alarm`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should `logical_time_to_alarm_seconds` (`detcore/src/syscalls/signal.rs:216`) follow Linux's `alarm_setitimer` rounding: round to the nearest second, half a second or more up, at least 1 when a timer was armed? Today it always rounds up, and its unit test (`detcore/src/syscalls/signal.rs:903`) pins that. Fixing it changes values a guest observes, so it needs an owner decision and a product issue; T2 is written to fail until then.
