---
schema: syscall-pages/v1
syscall: setitimer
nr_x86_64: 38
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/432
  - https://github.com/rrnewton/hermit/issues/447
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setitimer&body=Discussion+for+the+%60setitimer%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetitimer.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setitimer%60%2C+or+in+what+the+page+says%3F%0A
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

Arms or disarms one of three per-process interval timers (ITIMER_REAL, ITIMER_VIRTUAL, ITIMER_PROF). When a timer expires, the kernel sends SIGALRM, SIGVTALRM or SIGPROF. Hermit models ITIMER_REAL on Detcore logical time, so expiry happens at the same point in every run. It refuses the two CPU-time timers with `ENOSYS`.

Manual reference: [setitimer(2)](https://man7.org/linux/man-pages/man2/setitimer.2.html).

## Linux semantics

`setitimer(which, new_value, old_value)` replaces the timer named by `which` with `new_value->it_value` (time to the first expiry) and `new_value->it_interval` (reload period). If `old_value` is not NULL, the previous remaining time and interval are written to it. The kernel copies `new_value` in before it checks `which`, so a bad pointer gives `EFAULT` even when `which` is also invalid. It writes `old_value` out after the timer has already been changed.

- ITIMER_REAL counts wall-clock time. ITIMER_VIRTUAL counts the process's user CPU time. ITIMER_PROF counts user and system CPU time.
- The timers belong to the process (thread group) and are shared by all its threads. The signal is sent to the process.
- A periodic ITIMER_REAL is rearmed when its SIGALRM is dequeued, not when it fires. Overruns are absorbed by moving the next deadline forward; they are never queued as extra signals.
- `alarm(2)` and ITIMER_REAL are the same timer.
- After `fork` the child starts with all timers disarmed. `execve` keeps them.
- A host probe on 2026-07-24 disarmed ITIMER_REAL with an all-zero value and got 0, with the old value reported as all zeros (see Additional information).

### Flags and options

- F1: `which = ITIMER_REAL`: wall-clock timer that sends SIGALRM.
- F2: `which = ITIMER_VIRTUAL`: user-CPU-time timer that sends SIGVTALRM.
- F3: `which = ITIMER_PROF`: user-plus-system CPU-time timer that sends SIGPROF.
- F4: any other `which` gives `EINVAL`.
- F5: nonzero `it_value` with zero `it_interval`: a one-shot timer.
- F6: nonzero `it_interval`: a periodic timer, reloaded with the interval. For ITIMER_REAL the reload happens when the pending SIGALRM is dequeued, and missed periods are skipped, not queued.
- F7: zero `it_value`: disarm. The interval is ignored and cleared.
- F8: non-NULL `old_value` receives the previous remaining time and interval. An active timer whose expiry has passed but whose signal was not yet handled reports a minimum of 1 µs, never zero. A NULL `old_value` means nothing is reported.
- F9: NULL `new_value`: Linux treats it as an all-zero value (a disarm) and logs a one-time "Misfeature" kernel warning.
- F10: `tv_usec` outside 0..999999, or a negative `tv_sec`, in either field gives `EINVAL`. Very large values are clamped, not refused.
- F11: `EFAULT` for an unreadable `new_value`, or for an unwritable `old_value`; in the second case the timer has already been changed.
- F12: process scope: one timer of each kind per thread group, shared by threads. A `fork` child starts disarmed, and `execve` keeps the timers.
- F13: ITIMER_REAL is shared with `alarm(2)`: each call replaces and reports the other's state.

## Determinism analysis

`linux_determinism: nondeterministic`. Natively, every observable output depends on time the program does not control:

- The remaining time reported in `old_value` depends on how much real or CPU time passed since the timer was armed.
- When the signal arrives, relative to the guest's instructions, depends on host scheduling, timer granularity and system load.
- For a periodic timer, the number of missed periods (overruns) depends on how late the handler runs.
- For ITIMER_VIRTUAL and ITIMER_PROF, expiry depends on CPU accounting, which depends on cache, frequency and preemption.

### Nondeterminism sources

- Elapsed wall-clock time (F1) and consumed CPU time (F2, F3) drive expiry, the remaining time and overruns.
- Asynchronous signal delivery competes with the guest's own execution and with other signals.
- A call repeats only for inputs that read no clock: a disarm of an already disarmed timer (F7), and every error case (F4, F10, F11).

## Hermit design

Target policy: `model`. Detcore owns ITIMER_REAL completely and puts it on logical time, so the timer's deadlines, the remaining time it reports and the point where SIGALRM is delivered come from the deterministic scheduler, not the host clock. The dispatch arm at `detcore/src/lib.rs:2483` sends the call to `handle_setitimer` at `detcore/src/syscalls/signal.rs:292`:

- Without `sequentialize_threads` the call is injected unchanged (`detcore/src/syscalls/signal.rs:297`). There is no logical clock to put it on.
- Any `which` other than ITIMER_REAL returns `ENOSYS` (`detcore/src/syscalls/signal.rs:304`).
- The new value is read from guest memory and converted with `timeval_to_logical_time` (`detcore/src/syscalls/signal.rs:192`). That function refuses a negative `tv_sec`, a negative `tv_usec` or `tv_usec >= 1000000` with `EINVAL`, matching F10.
- `register_alarm` (`detcore/src/tool_global.rs:3951`) replaces the process's single ITIMER_REAL state in the global scheduler and returns the old remaining time and interval. That state is also used by `handle_alarm` (`detcore/src/syscalls/signal.rs:265`), which matches F13. If the scheduler cannot register the timer, the thread is parked as failed (`detcore/src/tool_global.rs:2803`).
- `old_value` is written only after the timer was replaced, as Linux does (F11).

The timer state lives in `detcore/src/scheduler/real_timer.rs`, whose module comment calls it "Process-owned ITIMER_REAL state" (`detcore/src/scheduler/real_timer.rs:8`):

- `replace` (`detcore/src/scheduler/real_timer.rs:357`) disarms and clears the interval for a zero value, and otherwise sets the deadline to now plus the value (F5, F7).
- `dequeue` (`detcore/src/scheduler/real_timer.rs:505`) rearms a periodic timer only when the shared SIGALRM is dequeued, and skips whole missed periods. This is the Linux overrun rule (F6). The unit test `disarm_clears_interval_and_dequeue_does_not_restart_it` (`detcore/src/scheduler/real_timer.rs:662`) checks that a disarm stops the reload.
- `active_remaining` (`detcore/src/scheduler/real_timer.rs:112`) reports at least 1 µs for an overdue active timer, like the kernel's `itimer_get_remtime` (F8).
- `bind` (`detcore/src/scheduler/real_timer.rs:153`) gives a new process a disarmed timer, and `rebind_after_exec` (`detcore/src/scheduler/real_timer.rs:267`) keeps the timer across a successful exec (F12).

Why `model` and not the alternatives:

- `passthrough` would put a host wall-clock timer inside a deterministic run, which is the exact nondeterminism this call introduces.
- `emulate` (for example, accept the call and never fire) would break every program that uses SIGALRM for timeouts.
- `refuse` would break `alarm`-style timeouts in shells, test harnesses and network clients.

What remains before the code fully realizes `model` (each line compares the code above with the Linux behaviour in the F list):

- **F2, F3:** ITIMER_VIRTUAL and ITIMER_PROF return `ENOSYS`; Linux accepts them. `handle_getitimer` (`detcore/src/syscalls/signal.rs:327`) reports them as zero, which is consistent with "never armed". A program that profiles with SIGPROF or SIGVTALRM sees a failure Linux would not give. The target below moves them onto virtual CPU time.
- **F4:** an invalid `which` returns `ENOSYS`; Linux returns `EINVAL`. `getitimer` already returns `EINVAL` for the same input (`detcore/src/syscalls/signal.rs:345`).
- **F4 versus F11 ordering:** Hermit checks `which` before it reads `new_value`, so an invalid `which` with a bad pointer gives `ENOSYS`, where Linux gives `EFAULT`.
- **F9:** a NULL `new_value` returns `EFAULT` (`detcore/src/syscalls/signal.rs:308`); Linux disarms the timer and returns 0.
- **F10:** a value too large to hold in 64-bit nanoseconds returns `EINVAL`; Linux clamps it.
- Reported times are truncated to whole microseconds by `logical_time_to_timeval` (`detcore/src/syscalls/signal.rs:207`), which is within Linux's own rounding.

Target (owner decisions 13 and 6): ITIMER_VIRTUAL and ITIMER_PROF move off `ENOSYS` (`detcore/src/syscalls/signal.rs:304`) onto virtual CPU time, as defined in the `clock_gettime` body.

- ITIMER_VIRTUAL counts the process's virtual user CPU time and sends SIGVTALRM (F2).
- ITIMER_PROF counts virtual user plus system CPU time and sends SIGPROF (F3).
- Both read the per-process counters that `process_cpu_time` keeps (`detcore/src/tool_local.rs:2146`). That function already separates user and system time, so ITIMER_PROF needs no new split.
- Neither timer advances while every thread of the process is blocked. Expiry is the point in the virtual schedule where the counter passes the deadline. Like ITIMER_REAL, the signal goes to the process, a periodic timer reloads with its interval, and `old_value` reports remaining CPU time (F6, F8).
- ITIMER_REAL stays on virtual time, as it is today.

Kernel order (`SYSCALL_DEFINE3(setitimer)`, `get_itimerval` and `do_setitimer` in `kernel/time/itimer.c`):

1. A non-NULL `new_value` is copied in; an unreadable one gives `EFAULT` (F11).
2. An invalid `timeval` in either field gives `EINVAL` (F10).
3. A NULL `new_value` is treated as all zeros, which disarms (F9).
4. An unknown `which` gives `EINVAL` (F4).
5. The timer is changed.
6. Only then does copying out to `old_value` give `EFAULT`, with the new setting kept (F11).

Today `which` is checked first and gives `ENOSYS` (`detcore/src/syscalls/signal.rs:304`), and a NULL `new_value` gives `EFAULT` (`detcore/src/syscalls/signal.rs:308`). Steps 5 and 6 already match for ITIMER_REAL. The unsequentialized injection at `detcore/src/syscalls/signal.rs:297` is outside this target: it is the mode with no logical clock.

`linux_determinism` stays `nondeterministic`: it describes the native call. Under Hermit, ITIMER_REAL is deterministic because the logical clock replaces wall-clock time.

## Recommended tests

- T1 (covers F1, F5, F6, F8): arm a one-shot and then a periodic ITIMER_REAL under `hermit run --verify` on ptrace. Count SIGALRM deliveries, read `old_value` on each rearm, and check that two runs print identical sequences.
  Existing candidates: `tests/c/setitimer_determinism.c` (cell `c-programs/setitimer-determinism`, `tests/e2e/manifests/c-programs.yaml:7925`, verify ci on ptrace; a 10 ms periodic timer and twenty deliveries) and `tests/c/periodic_setitimer_delivery.c` (cell `c-programs/periodic-setitimer-delivery`, `tests/e2e/manifests/c-programs.yaml:5045`, verify ci on ptrace; exactly three deliveries, then a disarm).
- T2 (covers F7, F8): arm, then disarm with a zero `it_value` and a nonzero interval. `old_value` must report the previous state, `getitimer` must then read zeros, and no further SIGALRM may arrive.
  Existing candidate: `tests/c/getitimer_determinism_probe.c` (cell `c-programs/getitimer-determinism-probe`, `tests/e2e/manifests/c-programs.yaml:1989`, verify ci on ptrace). Related unit test: `disarm_clears_interval_and_dequeue_does_not_restart_it` at `detcore/src/scheduler/real_timer.rs:662`.
- T3 (covers F2, F3): arm ITIMER_VIRTUAL and ITIMER_PROF and record the return value and errno, then burn CPU and record whether SIGVTALRM or SIGPROF arrives. Linux and the target (owner decision 13) return 0 and deliver the signal after the armed amount of virtual CPU time, identically across runs; a timer armed while the process sleeps in `nanosleep` must not fire during the sleep. Hermit returns `ENOSYS` at this SHA.
  Existing partial candidate: `tests/c/timer_family_identity.c:197` (cell `c-programs/timer-family-identity`, `tests/e2e/manifests/c-programs.yaml:17628`, ptrace, verify ci false). It covers ITIMER_VIRTUAL only and ignores the return value.
- T4 (covers F4, F10, F11): with raw `syscall(SYS_setitimer, ...)`, check `which = 3` gives `EINVAL`; `tv_usec = 1000000` and a negative `tv_sec` give `EINVAL`; an unmapped `new_value` gives `EFAULT`, also when `which` is invalid; and an unmapped `old_value` gives `EFAULT` while the new timer is armed anyway, as a following `getitimer` shows.
- T5 (covers F9): `setitimer(ITIMER_REAL, NULL, &old)` after arming a timer. Linux returns 0, reports the old value and disarms. At this SHA Hermit returns `EFAULT`.
- T6 (covers F12, F13): arm ITIMER_REAL, then `fork`. The child's `getitimer` must read zeros and the child must get no SIGALRM, while the parent still gets its signal. Arm a timer and `execve` a probe that reads `getitimer`; the timer must survive. In a second thread, call `alarm(5)` after `setitimer` and check that `alarm` returns the remaining time of the setitimer timer.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/432: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/447 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (ITIMER_REAL modeled on logical time); linux_determinism stays nondeterministic; merged duplicate https://github.com/rrnewton/hermit/issues/447; gaps against Linux listed (VIRTUAL/PROF ENOSYS, invalid which ENOSYS not EINVAL, NULL new_value EFAULT not disarm); left todo with an open question on ITIMER_VIRTUAL/ITIMER_PROF.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 and 6 (ITIMER_VIRTUAL and ITIMER_PROF move from ENOSYS to virtual user and user+system CPU time; kernel order new_value EFAULT, timeval EINVAL, NULL as disarm, which EINVAL, change, old_value EFAULT); target_policy stays model; no new F/T ids (T3 now asserts the target); removed the open question on modelling or refusing ITIMER_VIRTUAL/ITIMER_PROF and its heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setitimer`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setitimer&body=Discussion+for+the+%60setitimer%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetitimer.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setitimer%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setitimer" (https://github.com/rrnewton/hermit/issues/432), legacy variant A1. Duplicate https://github.com/rrnewton/hermit/issues/447 was filed the same day with the same title and variant; it is merged into this body, and its comment history (0 comments) stays on that issue. The comment history of https://github.com/rrnewton/hermit/issues/432 stays on that issue.

How the merge was reconciled: the two issues made the same claims (three timers, three signals, expiry measured against real or CPU time, a disarm of ITIMER_REAL returns 0, legacy action NEEDS-HANDLER). They differed only in their strace probes: https://github.com/rrnewton/hermit/issues/432 passed a non-NULL `old_value` and https://github.com/rrnewton/hermit/issues/447 passed NULL. Both are kept below and are consistent with each other. Both issues said Hermit forwarded the call through a fallback block in `detcore/src/syscall_classification.rs`. That was true at `dd60278f` and is history at f4055eb9, where the call is dispatched to `handle_setitimer`.

### 2026-07-24 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setitimer`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setitimer` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setitimer(ITIMER_REAL, {it_interval={tv_sec=0, tv_usec=0}, it_value={tv_sec=0, tv_usec=0}}, {it_interval={tv_sec=0, tv_usec=0}, it_value={tv_sec=0, tv_usec=0}}) = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence from https://github.com/rrnewton/hermit/issues/447

Gathered on the same host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setitimer` on a direct `syscall()` harness, with non-destructive arguments. That issue also recorded that Hermit then forwarded `setitimer` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```text
setitimer(ITIMER_REAL, {it_interval={tv_sec=0, tv_usec=0}, it_value={tv_sec=0, tv_usec=0}}, NULL) = 0
```
