---
schema: syscall-pages/v1
syscall: timer_settime
nr_x86_64: 223
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_settime&body=Discussion+for+the+%60timer_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_settime%60%2C+or+in+what+the+page+says%3F%0A
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

`timer_settime(timerid, flags, new_value, old_value)` arms, re-arms or disarms a POSIX timer created by `timer_create`, optionally returning the previous setting. Detcore emulates it (`detcore/src/syscalls/time.rs:507`): it stores the deadline and interval in its own per-process table, measured on the virtual clock, and registers the deadline with the deterministic scheduler, which sends the timer's signal when virtual time reaches it. Out-of-range time values are clamped instead of refused.

Manual reference: Linux man-pages timer_settime(2).

## Linux semantics

`new_value->it_value` is the first expiration and `new_value->it_interval` the reload period. With `flags` 0 the expiration is relative to the current time on the timer's clock; with `TIMER_ABSTIME` it is an absolute time on that clock, and a time already passed expires at once. A zero `it_value` disarms the timer. Re-arming replaces the previous setting. If `old_value` is non-NULL, it receives the previous setting in the same form `timer_gettime` returns.

Each expiration notifies as `timer_create` configured it. For a periodic timer, if the previous expiration signal is still pending the kernel does not queue another; it raises the overrun count that `timer_getoverrun` and `si_overrun` report. For `CLOCK_REALTIME` with `TIMER_ABSTIME`, a later `clock_settime` moves the expiration.

Errors: NULL `new_value` gives `EINVAL`; an unreadable `new_value` or unwritable `old_value` gives `EFAULT`; a `timespec` with a negative field or `tv_nsec` of 1 000 000 000 or more gives `EINVAL`; an unknown or deleted timer id gives `EINVAL`.

### Flags and options

- F1: `flags` 0: `it_value` is relative to the current time on the timer's clock.
- F2: `TIMER_ABSTIME`: `it_value` is an absolute time on the timer's clock; a time already passed expires at once.
- F3: zero `it_value`: disarm the timer, whatever `it_interval` says.
- F4: non-zero `it_interval`: periodic reload after each expiration; expirations that happen while the previous signal is pending are counted as overruns, not queued.
- F5: non-NULL `old_value`: receives the previous remaining time and interval.
- F6: NULL `new_value`: `EINVAL`; `new_value` or `old_value` pointing at bad memory: `EFAULT`.
- F7: invalid `timespec` in `it_value` or `it_interval` (negative field, or `tv_nsec` not below 1 000 000 000): `EINVAL`.
- F8: unknown or deleted timer id: `EINVAL`.
- F9: expiration notification as configured by `timer_create`: a signal for `SIGEV_SIGNAL` and `SIGEV_THREAD_ID`, nothing for `SIGEV_NONE`.
- F10: re-arming an armed timer replaces its previous deadline and interval.

## Determinism analysis

`linux_determinism: nondeterministic`. The deadline is measured on a host clock, so when the expiration signal arrives relative to the program's own progress depends on host load and scheduling, and the remaining time reported in `old_value` depends on how much real time has passed since the previous arming. A `CLOCK_REALTIME` deadline can also move when the host clock is set. The call repeats only for its error results and for a timer that was disarmed before the call.

### Nondeterminism sources

- Expiration timing relative to the program (F1, F2, F4, F9): host clock and scheduling.
- Remaining time in `old_value` (F5): elapsed real time.
- Overrun counts (F4): how many periods passed while the signal was pending, which is host timing.
- `clock_settime` or NTP adjustments for `CLOCK_REALTIME` absolute deadlines (F2).

## Hermit design

Target policy: `model`. Detcore owns the timer state and the clock, and the scheduler owns expirations. The handler at `detcore/src/syscalls/time.rs:507` (dispatch arm `detcore/src/lib.rs:2786`) never forwards the call:

- A NULL `new_value` gives `EINVAL` (`detcore/src/syscalls/time.rs:513`), as on Linux (F6).
- The current virtual time is read once (`detcore/src/syscalls/time.rs:518`). A zero value disarms; `TIMER_ABSTIME` takes the value as an absolute virtual time; otherwise the deadline is now plus the value (`detcore/src/syscalls/time.rs:519-526`, F1, F2, F3). The absolute case lines up with what the guest reads, because `clock_gettime` returns the same virtual clock for every clock id (`detcore/src/syscalls/time.rs:210-217`).
- The table update replaces the old setting and returns it (`detcore/src/tool_local.rs:215-230`, F10). An unknown id gives `EINVAL` (`detcore/src/syscalls/time.rs:534`, F8). The old setting is written to `old_value` when given (`detcore/src/syscalls/time.rs:537-543`, F5), with remaining time computed from the virtual clock (`detcore/src/tool_local.rs:312-322`).
- Timers with a deliverable signal are registered with the scheduler (`detcore/src/syscalls/time.rs:545-554`, `detcore/src/scheduler.rs:5801-5819`), one event per timer id (`detcore/src/scheduler/timed_waiters.rs:28`). A periodic timer is re-inserted at the old deadline plus the interval each time it fires (`detcore/src/scheduler/timed_waiters.rs:366-371`), and `fire_alarm` sends the signal (`detcore/src/scheduler.rs:3077-3094`, F9).

Why `model` and not the alternatives: forwarding to a host timer would bring back host timing, the main nondeterminism source; refusing would break runtimes that arm watchdogs (see the `timer_create` body).

What remains before the code fully realizes `model`:

- **Invalid `timespec` values are clamped, not refused (F7).** `timespec_to_ns` turns negative fields into 0 and folds any `tv_nsec` into the total (`detcore/src/syscalls/time.rs:56-62`). So a negative `it_value` disarms the timer, and `tv_nsec` 1 500 000 000 means 1.5 seconds, where Linux gives `EINVAL` for both.
- **Overruns are not counted (F4).** Every periodic expiration produces its own signal send, and `timer_getoverrun` always returns 0 (`detcore/src/syscalls/time.rs:589-607`). For a standard signal still pending, the extra send merges into the pending one, so the guest loses the count it would read on Linux.
- **Notification gaps inherited from `timer_create` (F9).** Timers with a real-time signal are stored without one and are never registered (`detcore/src/syscalls/time.rs:545`), signals arrive without `SI_TIMER` `siginfo` (`detcore/src/scheduler.rs:3308-3316`), and `SIGEV_THREAD_ID` is treated as process-directed. The `timer_create` body gives the evidence.

Target (owner decisions 13 and 6): deadlines are measured on the timer's own clock, and the arguments are checked in the kernel's order. The clock names are those defined in the `clock_gettime` body.

- A `CLOCK_REALTIME` or `CLOCK_TAI` timer takes a `TIMER_ABSTIME` value as virtual realtime, which is today's conversion (`detcore/src/syscalls/time.rs:521` to `detcore/src/syscalls/time.rs:523`).
- A `CLOCK_MONOTONIC` or `CLOCK_BOOTTIME` timer takes it as virtual uptime, and the deadline adds the virtual boot instant. Without that, a deadline the guest computed from `clock_gettime(CLOCK_MONOTONIC)` would already have passed.
- A CPU-time timer arms against its target's virtual CPU time, not against elapsed virtual time.
- `old_value` reports the remaining time on the timer's own clock.

Today every timer uses the one virtual clock (`detcore/src/syscalls/time.rs:518` to `detcore/src/syscalls/time.rs:526`).

Kernel order (`SYSCALL_DEFINE4(timer_settime)` and `do_timer_settime` in `kernel/time/posix-timers.c`):

1. A NULL `new_value` gives `EINVAL`.
2. Copying it in gives `EFAULT`.
3. `timespec64_valid` on `it_interval` and `it_value` gives `EINVAL` (F7). This replaces today's clamping in `timespec_to_ns` (`detcore/src/syscalls/time.rs:58`).
4. `lock_timer` gives `EINVAL` for an unknown id (F8).
5. The timer is changed.
6. Only then does copying out to `old_value` give `EFAULT`, with the new setting kept.

Today the `old_value` write fails through `?` before `register_posix_timer` runs (`detcore/src/syscalls/time.rs:543`, `detcore/src/syscalls/time.rs:545`). So an unwritable `old_value` leaves the table armed with no scheduler deadline, and the timer never fires. Under the target, the deadline is registered before the copy-out.

`linux_determinism` is `nondeterministic` for the native call; under Hermit, expiration times and `old_value` are functions of virtual time.

## Recommended tests

- T1 (covers F1, F5, F9): create a `SIGALRM` timer, arm it relative for 10 ms, read `old_value` (zero for a fresh timer), and check the signal arrives inside a bounded virtual-time window, under `hermit run --strict --verify` on ptrace.
  Existing candidates: `tests/bin/posix_timer_test.c` (cell `bin-c/posix-timer-test`, `tests/e2e/manifests/bin-c.yaml:4`, verify ci on ptrace) and `tests/c/timer_create_determinism.c` (cell `c-programs/timer-create-determinism`, `tests/e2e/manifests/c-programs.yaml:10299`, verify ci on ptrace), which checks the zero `old_value`.
- T2 (covers F2): read `clock_gettime(CLOCK_MONOTONIC)`, arm with `TIMER_ABSTIME` at that time plus 10 ms and at a time already passed; the first fires after 10 ms of virtual time, the second at once.
- T3 (covers F3, F10): arm, re-arm with a different value and check `old_value` reports the first arming; disarm with a zero `it_value` and a non-zero `it_interval` and check no signal arrives.
  Related existing unit tests: `resetting_reports_old_remaining` at `detcore/src/tool_local.rs:893` and `disarm_and_unknown_ids` at `detcore/src/tool_local.rs:920`.
- T4 (covers F4): a periodic `SIGUSR1` timer at 1 ms while the signal is blocked for 10 ms; after unblocking, exactly one signal arrives and `timer_getoverrun` reports the missed periods. The overrun check is expected to fail at this SHA.
  Related existing unit test: `periodic_remaining_advances_past_each_deadline` at `detcore/src/tool_local.rs:908`.
- T5 (covers F6, F7, F8): NULL `new_value` gives `EINVAL`; an unmapped `new_value` or `old_value` gives `EFAULT`; `tv_nsec` -1 and 1 000 000 000 in `it_value` and in `it_interval` give `EINVAL`; a deleted id gives `EINVAL`.
- T6 (covers F9, F10): arm timers from two threads with interleaved deadlines and check the order of expiration signals across two runs.
  Existing candidate: `tests/c/timer_family_identity.c` (cell `c-programs/timer-family-identity`, `tests/e2e/manifests/c-programs.yaml:17628`); its verify cell has `ci: false`.
- T7 (covers F2, F5, F6): create timers on `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` and `CLOCK_REALTIME`, read that clock with `clock_gettime`, and arm each with `TIMER_ABSTIME` at the reading plus 10 ms. Each must fire after 10 ms of virtual time, not at once. Arm a `CLOCK_PROCESS_CPUTIME_ID` timer for 10 ms while the process sleeps 1 s: it must not fire until the process has spun for 10 ms of virtual CPU time. Re-arm with `old_value` on a read-only page: the call gives `EFAULT`, and the new setting still fires.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (deadlines on the virtual clock, scheduler-delivered signals, periodic re-insertion); linux_determinism nondeterministic; gaps: invalid timespec clamped instead of EINVAL, no overrun accounting, notification gaps from timer_create; 10 flag behaviours and 6 tests listed; tracking_status stays todo with an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 and 6 (deadlines on the timer's clock, with the boot offset for MONOTONIC/BOOTTIME absolute values and virtual CPU time for CPU timers; kernel order NULL EINVAL, EFAULT, timespec EINVAL instead of clamping, id EINVAL, old_value EFAULT after the change, which today drops the scheduler registration); target_policy stays model; new T7; no questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timer_settime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_settime&body=Discussion+for+the+%60timer_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_settime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timer_settime`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The handler carries the tags `AUTONOMOUS-BOT-IMPLEMENTED` and `TODO-HUMAN-REVIEW(#869)` (`detcore/src/syscalls/time.rs:503-504`), from commit 5687ea1683 (2026-07-27), whose subject names https://github.com/rrnewton/hermit/pull/869. Related unit test: `saturated_posix_timer_alone_is_a_deadlock_not_a_bogus_signal` at `detcore/src/scheduler.rs:8853`. The sibling bodies `timer_create.md` and `timer_gettime.md` cover creation and reading.

### Open questions

- Should overruns be modelled, with one pending signal per timer and a per-timer overrun count, so `timer_getoverrun` and `si_overrun` match Linux? That changes how many signals a slow handler receives.
