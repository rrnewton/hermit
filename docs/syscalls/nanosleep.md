---
schema: syscall-pages/v1
syscall: nanosleep
nr_x86_64: 35
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+nanosleep&body=Discussion+for+the+%60nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60nanosleep%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 9
---

## Summary

`nanosleep(req, rem)` suspends the calling thread for the relative interval in `*req`. If a signal handler interrupts it, it returns `EINTR` and writes the unslept time to `*rem`. Detcore handles it with `clock_nanosleep` in `handle_nanosleep_family` (`detcore/src/syscalls/time.rs:339`). The sleep becomes a wait in virtual time, `SleepUntil(now + req)`, which the deterministic scheduler satisfies by advancing logical time rather than by waiting on a host timer.

Manual reference: Linux man-pages nanosleep(2).

## Linux semantics

`req` is a relative interval. Linux measures it against `CLOCK_MONOTONIC` (nanosleep(2) NOTES), and the thread may sleep longer than asked because of timer slack and scheduling. `nanosleep` has no flags; `clock_nanosleep` is the variant that chooses the clock and allows `TIMER_ABSTIME`.

On interruption by a caught signal the call returns `-1`/`EINTR`. If `rem` is not NULL, the remaining time is written there. The kernel returns `ERESTART_RESTARTBLOCK`, which is never restarted after a handler, whatever `SA_RESTART` says. If the thread is only stopped and continued, or ptrace-stopped, with no handler run, the kernel restarts the sleep through `restart_syscall` for the remaining time.

A zero interval returns 0 at once. Before sleeping, the kernel copies `*req` (failure gives `EFAULT`) and validates it (`EINVAL`).

Measured on the work host (glibc 2.34) with strace on 2026-10-03: glibc's `nanosleep()`, `sleep()` and `usleep()` all issue `clock_nanosleep(CLOCK_REALTIME, 0, ...)`, not syscall 35. A guest reaches `SYS_nanosleep` only through a raw `syscall()`, the Go runtime, another libc, or an older glibc.

### Flags and options

- F1: relative interval: sleep for `*req`, then return 0
- F2: zero interval (`{0, 0}`): return 0 immediately
- F3: `req` NULL or unreadable: `EFAULT`
- F4: malformed `*req`: `tv_sec < 0`, `tv_nsec < 0` or `tv_nsec > 999999999` gives `EINVAL`, before any sleep
- F5: interrupted by a caught signal with `rem` not NULL: `EINTR`, and the unslept time is written to `*rem`
- F6: interrupted with `rem` NULL: `EINTR`, nothing written
- F7: interrupted with `rem` pointing at unwritable memory: `EFAULT`
- F8: `SA_RESTART` on the interrupting handler has no effect: the call still returns `EINTR`
- F9: a stop and continue, or a ptrace stop, with no handler run: the sleep resumes for the remaining time instead of returning
- F10: an ignored or blocked signal does not interrupt the sleep

## Determinism analysis

`linux_determinism: nondeterministic`, changed from the rendered `conditional`. The time at which the thread wakes depends on host timers, timer slack and CPU scheduling, so the program points other threads have reached by then differ from run to run. If a signal interrupts the sleep, whether it lands before the deadline, and the value written to `*rem`, both depend on host timing. Only the argument checks (F3, F4) and the zero interval (F2) repeat on their own.

## Hermit design

Target policy: `model`. The sleep is modeled in virtual time. The handler at `detcore/src/syscalls/time.rs:339`, reached from the dispatch arm at `detcore/src/lib.rs:2600` through reverie's `NanosleepFamily`, which reports flags 0 for `nanosleep`, does this:

- A NULL `req` gives `EFAULT` (`detcore/src/syscalls/time.rs:349`); an unreadable one fails the memory read (`detcore/src/syscalls/time.rs:350`). Both are F3.
- Malformed fields give `EINVAL` before any sleep (`detcore/src/syscalls/time.rs:367`), covering F4. The comment above that check records the defect it fixed: `tv_sec = -1` wrapped to an indefinite sleep and the container died.
- With threads sequentialized (`config.sequentialize_threads`, `hermit-cli/src/bin/hermit/run.rs:3667`), the interval is added to the thread's virtual time to build `SleepUntil` (`sleep_request`, `detcore/src/syscalls/time.rs:114`; call at `detcore/src/syscalls/time.rs:376`). The thread then parks with `ParkedWaitPolicy::NanosleepNoHandlerRestart` (`wait_and_return`, `detcore/src/syscalls/time.rs:294`). A normal wake returns 0 (`detcore/src/syscalls/time.rs:309`), covering F1 and F2.
- A signal wake computes the remaining virtual time, writes it to `rem` when `rem` is not NULL (`detcore/src/syscalls/time.rs:315` to `detcore/src/syscalls/time.rs:329`), and returns `EINTR` (`detcore/src/syscalls/time.rs:333`). A write fault on `rem` returns that error, as F7 requires. The call is never restarted, matching F8.
- Without sequentialization, the call is passed to the kernel unchanged (`detcore/src/syscalls/time.rs:384`).

Why `model`: a host sleep would make the wake time depend on real time. `emulate` with an immediate return would change how threads interleave around sleeps, and every polling loop would spin. `passthrough` hides the wait from the scheduler.

Not confirmed from the code here: how a group stop (F9) interacts with a parked virtual sleep, and whether an ignored signal (F10) can produce a `Signaled` wake. The same question is open for `pause`. T6 and T7 measure them.

Target (owner decisions 14, 13 and 6): `nanosleep` is a scheduler wait, and today's sequentialized form already is one. A zero or elapsed interval returns at once, and anything longer parks the thread until the scheduler reaches `SleepUntil(now + req)` (`detcore/src/syscalls/time.rs:376`, `detcore/src/syscalls/time.rs:294`). The interval is measured on the thread's virtual time. Under decision 13, `CLOCK_MONOTONIC` becomes virtual uptime (the clock names are those defined in the `clock_gettime` body), which advances in step with that virtual time. So the Linux rule that `nanosleep` measures `CLOCK_MONOTONIC` holds, and the relative deadline does not change. The one form that leaves the scheduler is the unsequentialized host injection (`detcore/src/syscalls/time.rs:384`), which is the mode with no logical clock. The kernel's check order, `EFAULT` for the copy of `req` and then `EINVAL` for its fields, already matches (`detcore/src/syscalls/time.rs:349`, `detcore/src/syscalls/time.rs:367`), as owner decision 6 requires. `nanosleep` always arrives with flags 0, so the stray-flag paths that the `clock_nanosleep` body removes (`detcore/src/syscalls/time.rs:344`, `detcore/src/syscalls/time.rs:423`) do not affect it.

Every recommended test must issue syscall 35 directly with `syscall(SYS_nanosleep, ...)`, because glibc's `nanosleep()` reaches `clock_nanosleep` instead.

## Recommended tests

- T1 (covers F1, F2): raw `nanosleep` of `{0, 0}`, `{0, 1}` and `{1, 0}` returns 0 each time, and `CLOCK_MONOTONIC` read before and after advances by at least the interval. Output must be identical across `hermit run --verify` runs.
- T2 (covers F3, F4): raw `nanosleep` with `req` NULL, `req` pointing at an unmapped page, and the three malformed shapes `{-1, 0}`, `{0, -1}`, `{0, 1000000000}`. Expect `EFAULT`, `EFAULT`, then `EINVAL` three times, with no sleep.
  Related existing test: `nanosleep_rejects_malformed_timespec_but_not_a_past_deadline` at `detcore/tests/time/mod.rs:556`. It checks the same shapes, but through `clock_nanosleep`, so it exercises the shared handler and not syscall 35.
- T3 (covers F5, F6): arm `alarm(1)` with a handler, then raw `nanosleep({5, 0}, &rem)`. Expect `EINTR` and `rem` close to 4 seconds of virtual time, the same value every run. Repeat with `rem` NULL and expect `EINTR`.
- T4 (covers F7): the same interruption with `rem` pointing at a read-only page. Expect `EFAULT`.
- T5 (covers F8): the interruption with the handler installed with `SA_RESTART`. Expect `EINTR`, not a restarted sleep.
- T6 (covers F9): a child sends `SIGSTOP` then `SIGCONT` to a thread in a raw `nanosleep({2, 0}, NULL)`. The call must return 0 after the full interval, not `EINTR`.
- T7 (covers F10): set `SIGUSR1` to `SIG_IGN` and send it from a sibling thread during a raw `nanosleep({1, 0}, NULL)`. The call must return 0.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (a relative SleepUntil on the thread's virtual clock; a signal wake returns EINTR and writes the virtual remainder to rem); linux_determinism conditional -> nondeterministic (the wake time and rem depend on host timers); 10 flag behaviours and 7 tests listed; no existing test issues raw syscall 35 under glibc, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 14 (today's sequentialized form is already a scheduler wait; the unsequentialized injection is the only host form), 13 (the relative interval on virtual time equals one on virtual uptime) and 6 (EFAULT then EINVAL already in kernel order); target_policy stays model; no new F/T ids; no questions removed (the group-stop and ignored-signal question is not decided)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `nanosleep`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+nanosleep&body=Discussion+for+the+%60nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60nanosleep%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `nanosleep`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Does a group stop (F9) or an ignored signal (F10) produce a `Signaled` wake from the parked virtual sleep, so that the guest sees an `EINTR` Linux would not return? This body did not trace it; T6 and T7 measure it. The same question is open in the `pause` body.

### Existing tests named after the syscall

Existing tests named after the syscall do not issue syscall 35 under glibc: `tests/c/nanosleep-par.c` (cell `c-programs/nanosleep-par`, `tests/e2e/manifests/c-programs.yaml:4134`) calls libc `nanosleep`. `tests/rust/nanosleep.rs` calls `libc::nanosleep` with a musl target in its header; whether that musl build issues syscall 35 was not checked.
