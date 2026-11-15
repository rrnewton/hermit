---
schema: syscall-pages/v1
syscall: clock_nanosleep
nr_x86_64: 230
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_nanosleep&body=Discussion+for+the+%60clock_nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_nanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_nanosleep%60%2C+or+in+what+the+page+says%3F%0A
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

`clock_nanosleep(clockid, flags, req, rem)` suspends the calling thread for a relative interval, or until an absolute deadline when `flags` has `TIMER_ABSTIME`, measured against the clock `clockid`. Detcore handles it together with `nanosleep` (`detcore/src/lib.rs:2601`, `handle_nanosleep_family` at `detcore/src/syscalls/time.rs:339`): when threads are sequentialized the sleep becomes a `SleepUntil` request on the virtual clock and the thread parks in the scheduler, so no host time passes and the wake-up point is a deterministic logical time. The handler never looks at `clockid`, and flag values other than 0 and `TIMER_ABSTIME` fall back to the host kernel.

Manual reference: Linux man-pages clock_nanosleep(2).

## Linux semantics

The kernel checks the clock first, then copies in `req`, then checks the interval, then sleeps. Measured on the host (Linux 7.1.3, raw `syscall()` probe traced with strace, see Additional information): an invalid clock id gives `EINVAL` even when `req` is a bad pointer, `CLOCK_MONOTONIC_RAW` with a bad `req` gives `EOPNOTSUPP`, and `CLOCK_MONOTONIC` with a bad `req` gives `EFAULT`. Unlike most syscalls, the glibc wrapper returns the error number instead of setting `errno`.

A relative sleep lasts at least the requested interval, rounded up to the clock's resolution and extended by the thread's timer slack and by scheduling delay. An absolute sleep returns when the clock reaches the deadline; a deadline already in the past returns 0 at once. Only the `TIMER_ABSTIME` bit is interpreted; the kernel ignores every other bit of `flags` (the probe got 0 for flags `2` and `0x100`).

If a caught signal interrupts the sleep, the call fails with `EINTR` and, for a relative sleep with non-NULL `rem`, writes the unslept remainder there. For an absolute sleep `rem` is never written. A signal with no handler, such as a stop and continue, does not end the sleep: the kernel restarts it, recomputing the remaining time for a relative sleep. A sleep on `CLOCK_REALTIME` with `TIMER_ABSTIME` follows changes to the realtime clock; a relative sleep does not.

Clock support differs by clock. `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` and `CLOCK_TAI` sleep normally. `CLOCK_PROCESS_CPUTIME_ID` (or another process's CPU clock from `clock_getcpuclockid`) sleeps until that process has consumed the requested CPU time; a single-threaded process that sleeps on its own CPU clock therefore never wakes (the host probe of that case hung). The raw syscall on the calling thread's own `CLOCK_THREAD_CPUTIME_ID` gives `EOPNOTSUPP`, while glibc's wrapper returns `EINVAL` for it without issuing the syscall. `CLOCK_MONOTONIC_RAW`, `CLOCK_REALTIME_COARSE` and `CLOCK_MONOTONIC_COARSE` give `EOPNOTSUPP`. The `_ALARM` clocks need `CAP_WAKE_ALARM` and give `EPERM` without it.

### Flags and options

- F1: `flags == 0`: relative sleep for `req`. A zero interval returns 0.
- F2: `flags == TIMER_ABSTIME`: sleep until the absolute time `req` on `clockid`. A deadline already in the past returns 0 without sleeping.
- F3: any other bit in `flags`: ignored by the kernel, so the call behaves as F1 or F2 according to the `TIMER_ABSTIME` bit alone.
- F4: `clockid` is `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` or `CLOCK_TAI`: ordinary sleep against that clock. An absolute `CLOCK_REALTIME` sleep follows clock changes.
- F5: CPU-time clocks: `CLOCK_PROCESS_CPUTIME_ID` and process CPU clocks sleep until the CPU time is consumed. The raw syscall on the caller's own `CLOCK_THREAD_CPUTIME_ID` gives `EOPNOTSUPP`; the glibc wrapper returns `EINVAL` without a syscall.
- F6: clocks the syscall does not support (`CLOCK_MONOTONIC_RAW`, `CLOCK_REALTIME_COARSE`, `CLOCK_MONOTONIC_COARSE`): `EOPNOTSUPP`.
- F7: `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM`: `EPERM` without `CAP_WAKE_ALARM`.
- F8: an undefined clock id: `EINVAL`, checked before `req` is read.
- F9: a malformed `req` (`tv_sec < 0`, `tv_nsec < 0` or `tv_nsec > 999999999`): `EINVAL`, for both relative and absolute sleeps.
- F10: `req` NULL or unreadable: `EFAULT`, after the clock check.
- F11: a caught signal interrupts the sleep: `EINTR`. A relative sleep writes the remainder to a non-NULL `rem`; an absolute sleep never writes `rem`; a NULL `rem` is allowed.
- F12: a signal with no handler (stop and continue, an ignored signal) does not end the sleep; the kernel restarts it with the remaining time.
- F13: an encoded per-thread CPU clock id (from `pthread_getcpuclockid`): `EINVAL` when it names the calling thread; for another thread of the process it sleeps until that thread has consumed the CPU time; `EINVAL` when it names no live task.
- F14: a dynamic FD clock id: `EOPNOTSUPP` before `req` is read, because the dynamic clock type has no sleep operation.

## Determinism analysis

`linux_determinism: nondeterministic`, changed from the migration placeholder `conditional`. The successful return value is fixed (0), but everything a program can observe around it depends on the host: when the thread wakes relative to other threads and processes, how much later than requested it wakes (timer slack, scheduling latency), whether a signal arrives during the sleep and so whether the call returns `EINTR`, and how much time is left in `rem` when it does. An absolute deadline is computed by the guest from a clock reading, which is itself host time. Even the argument check is host-dependent for F5 and F7: CPU-clock progress and `CAP_WAKE_ALARM` are properties of the host process.

### Nondeterminism sources

- Wake-up time: timer resolution, timer slack, scheduler latency and host load
- Interleaving with other threads and processes while the caller sleeps, which is how programs use sleeps to order work
- Signal arrival during the sleep, which decides between 0 and `EINTR` and fixes the value written to `rem`
- Clock readings that the guest turns into absolute deadlines, and realtime clock changes during an absolute sleep
- CPU-time consumption for CPU-clock sleeps, and the caller's capabilities for the alarm clocks

## Hermit design

Target policy: `model`. Detcore owns the clock that the guest reads and the scheduler that decides when sleepers wake, so it can answer every case of this syscall from its own state without host time passing. The handler already does this for the common cases:

- Malformed intervals (F9) give `EINVAL` before any sleep (`detcore/src/syscalls/time.rs:367`). Before that check existed, `tv_sec = -1` became an indefinite sleep and the container died (comment at `detcore/src/syscalls/time.rs:352`). A NULL `req` gives `EFAULT` (`detcore/src/syscalls/time.rs:349`).
- A relative sleep (F1) under sequentialized threads becomes `SleepUntil(now + interval)` on the thread's virtual time (`sleep_request`, `detcore/src/syscalls/time.rs:114`), and the thread parks through `parked_wait_request` with `ParkedWaitPolicy::NanosleepNoHandlerRestart` (`detcore/src/syscalls/time.rs:300`; policy enum at `detcore/src/scheduler/parked.rs:46`).
- An absolute sleep (F2) under sequentialized threads with virtual time sleeps until the given logical time (`sleep_request_abs`, `detcore/src/syscalls/time.rs:122`, called at `detcore/src/syscalls/time.rs:392`). `clock_gettime` returns the same virtual time for every clock (`detcore/src/syscalls/time.rs:205`), so a deadline computed from any clock reading is consistent with the sleep. A past deadline wakes at once.
- On a caught signal (F11), `wait_and_return` returns `EINTR` and writes the remaining virtual time to `rem` only for a relative sleep (`detcore/src/syscalls/time.rs:310` to `detcore/src/syscalls/time.rs:333`), matching Linux.
- Without sequentialized threads, a relative sleep is passed to the host (`detcore/src/syscalls/time.rs:383`), and an absolute sleep with virtual time is converted into a relative host `nanosleep`, clamped to zero for a past deadline (`detcore/src/syscalls/time.rs:406`, `relative_time_from_abs_target` at `detcore/src/syscalls/time.rs:427`). That mode is not deterministic by design.

Why `model` and not the alternatives: `passthrough` would sleep in real time inside the kernel, outside the scheduler, so the interleaving with other guest threads would depend on host timing; `emulate` as a no-op return would break programs that use sleeps to wait for timers or other threads; `refuse` would break almost every program.

What remains before the code fully realizes `model` (all from code reading at this SHA, not measured under Hermit):

- **`clockid` is ignored.** Nothing in `handle_nanosleep_family` reads it, so F5 to F8 differ from Linux: an undefined clock, `CLOCK_MONOTONIC_RAW`, the coarse clocks, the alarm clocks and the raw `CLOCK_THREAD_CPUTIME_ID` call all sleep on virtual time and return 0 under Hermit where Linux returns `EINVAL`, `EOPNOTSUPP` or `EPERM`. A `CLOCK_PROCESS_CPUTIME_ID` sleep returns after virtual wall time instead of after CPU time; that is arguably a reasonable model, but it is a choice nobody has written down. The `tests/c/clock_determinism.c` expectation that `CLOCK_THREAD_CPUTIME_ID` gives `EINVAL` (line 123) passes because glibc answers it without a syscall, not because of Hermit.
- **Error order.** The handler reads `req` before any clock check, so an invalid clock with a bad pointer would give `EFAULT` where Linux gives `EINVAL` (F8, F10). This matters only once the clock is checked at all.
- **Unknown flag bits leak real time.** `flags > TIMER_ABSTIME` is injected to the host unchanged (`detcore/src/syscalls/time.rs:344`), so a guest passing a stray bit sleeps in real time, outside the scheduler, where Linux would treat it as an ordinary relative or absolute sleep (F3).
- **Negative flags reach a panic.** `NanosleepFamily::flags()` is a signed `int`, so a negative value passes the `> TIMER_ABSTIME` test, matches neither `0` nor `TIMER_ABSTIME`, and reaches `unreachable!("Unexpected, unhandled flag value")` (`detcore/src/syscalls/time.rs:423`). Linux treats such a value by its `TIMER_ABSTIME` bit. The fix for this and the previous item is the same: mask `flags` with `TIMER_ABSTIME` before the match.
- **Absolute sleep without virtual time.** When threads are sequentialized but time is not virtualized, an absolute sleep only yields (`detcore/src/syscalls/time.rs:399`, `TODO T124594597`), so it returns at once whatever the deadline.
- **Restart without a handler (F12).** The policy name says the parked wait handles a no-handler restart, and the scheduler reads the absolute deadline from it (`detcore/src/scheduler/parked.rs:730`). No test at this SHA was found that stops and continues a sleeping guest thread.

Target (owner decisions 13, 6, 10 and 14): the sleep honours `clockid`, checks its arguments in the kernel's order, and stays a scheduler wait. The clock names are those defined in the `clock_gettime` body.

Kernel order (`SYSCALL_DEFINE4(clock_nanosleep)` in `kernel/time/posix-timers.c`):

1. `clockid_to_kclock` gives `EINVAL` for an unknown id (F8).
2. A clock without an `nsleep` operation gives `EOPNOTSUPP` (F6, F14, and `CLOCK_THREAD_CPUTIME_ID` in F5).
3. `get_timespec64` gives `EFAULT` (F10).
4. `timespec64_valid` gives `EINVAL` (F9).
5. `rmtp` is dropped for `TIMER_ABSTIME`, and the clock's `nsleep` runs:
   - `common_nsleep` reads only `flags & TIMER_ABSTIME` (F3);
   - `posix_cpu_nsleep` gives `EINVAL` for the caller's own per-thread clock and for a dead task (F13);
   - `alarm_timer_nsleep` gives `EOPNOTSUPP` without an RTC, then `EPERM` without `CAP_WAKE_ALARM` in the initial user namespace (F7).

Per clock, under the target:

- `CLOCK_REALTIME` and `CLOCK_TAI` sleep on virtual realtime. `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` sleep on virtual uptime. A relative sleep is the same on both. An absolute deadline on a virtual-uptime clock is converted to logical time by adding the virtual boot instant, so a deadline the guest computed from `clock_gettime(CLOCK_MONOTONIC)` lines up with the sleep.
- `CLOCK_PROCESS_CPUTIME_ID`, another process's CPU clock, and another thread's per-thread clock sleep until the target's virtual CPU time (`process_cpu_time`, `detcore/src/tool_local.rs:2146`; `thread_cpu_time`, `detcore/src/tool_local.rs:2161`) reaches the deadline. A single-threaded process sleeping on its own CPU clock never wakes, as on Linux, and the scheduler's deadlock handling applies.
- `CLOCK_THREAD_CPUTIME_ID`, `CLOCK_MONOTONIC_RAW`, the two coarse clocks and every FD clock id give `EOPNOTSUPP`. Unknown ids give `EINVAL`.
- The two alarm clocks give `EPERM` on every host. A container process has no `CAP_WAKE_ALARM` in the initial user namespace, and the host's RTC is not consulted.
- `flags` is masked with `TIMER_ABSTIME` before the match.
  - This removes the real-time host injection of stray bits (`detcore/src/syscalls/time.rs:344` to `detcore/src/syscalls/time.rs:346`).
  - It also removes the guest-reachable `unreachable!` for negative flags (`detcore/src/syscalls/time.rs:423`). Under decision 10 that panic is a defect.

Today the handler reads `req` first (`detcore/src/syscalls/time.rs:349`) and never reads `clockid`.

Decision 14: today's sequentialized form already is a scheduler wait. A relative sleep builds `SleepUntil` (`detcore/src/syscalls/time.rs:376`) and an absolute one `sleep_request_abs` (`detcore/src/syscalls/time.rs:392`), and both park in `wait_and_return` (`detcore/src/syscalls/time.rs:294`). The forms that leave the scheduler are:

- the stray-flag injection above;
- the unsequentialized host sleeps (`detcore/src/syscalls/time.rs:384`, `detcore/src/syscalls/time.rs:415`, `detcore/src/syscalls/time.rs:420`);
- the sequentialized absolute sleep without virtual time, which only yields (`detcore/src/syscalls/time.rs:403`).

`linux_determinism` stays `nondeterministic`: Hermit removes the wake-up and interleaving sources by making the sleep a scheduler event, and signal timing is already scheduler-ordered.

## Recommended tests

- T1 (covers F1, F2, F9): under `hermit run --verify` on ptrace, a relative sleep, a zero interval, an absolute sleep with a future and with a past deadline, and the three malformed shapes in both modes; check every return value and that virtual time advanced by the requested amount.
  Existing candidate: `nanosleep_rejects_malformed_timespec_but_not_a_past_deadline` at `detcore/tests/time/mod.rs:556`, which covers the malformed shapes, a zero interval and a past absolute deadline on `CLOCK_MONOTONIC` with virtual time. It does not check a future absolute deadline or the elapsed virtual time.
- T2 (covers F3): raw `syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, flags, ...)` with flags `2`, `0x100`, `TIMER_ABSTIME|2` and `-1`. Linux returns 0 for each. Under Hermit, check that none of them sleeps in real time (wall-clock bound on the run) and that none panics.
- T3 (covers F4, F5): a short relative sleep on each of `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`, `CLOCK_TAI` and `CLOCK_PROCESS_CPUTIME_ID`, plus raw and glibc calls on `CLOCK_THREAD_CPUTIME_ID`, compared with the native results recorded under Additional information.
  Existing candidate: `tests/c/clock_determinism.c` (cell `system-utils/clock-determinism`, `tests/e2e/manifests/system-utils.yaml:4`, verify ci on ptrace), which sleeps 1000 ns on five clocks through glibc (line 38) and expects `EINVAL` only for `CLOCK_THREAD_CPUTIME_ID`. It covers neither `CLOCK_TAI` nor the raw syscall.
- T4 (covers F6, F7, F8, F10): raw calls on `CLOCK_MONOTONIC_RAW`, `CLOCK_REALTIME_COARSE`, `CLOCK_MONOTONIC_COARSE`, both alarm clocks, clock id 99, and clock id 99 with `req` = 1. Linux gives `EOPNOTSUPP`, `EPERM` and `EINVAL`, in that order of checks; `CLOCK_MONOTONIC` with `req` NULL gives `EFAULT`. Expected to fail under Hermit at this SHA (see Hermit design).
- T5 (covers F11): a thread sleeps 10 s relative and another 10 s absolute; a sibling sends a caught signal through `pthread_kill` after a short sleep of its own. Both sleeps return `EINTR`; only the relative one writes `rem`, and its value is identical across `--verify` runs. Repeat with `rem` NULL.
- T6 (covers F12): stop and continue a thread in a relative sleep with `SIGSTOP`/`SIGCONT` from another process, and send an ignored signal; the sleep must complete with 0 after the full interval of virtual time.
- T7 (covers F1, F2, F4): the race tests in which a sleeping thread races a printing thread must give identical output across runs.
  Existing candidates: `default_cargo_clock_nanosleep_monotonic`, `default_cargo_clock_nanosleep_monotonic_abs` and `default_cargo_clock_nanosleep_realtime_abs`, generated by `default_workload_tests!` at `hermit-cli/tests/hermit_modes.rs:909` to `hermit-cli/tests/hermit_modes.rs:911` from `tests/rust/print_clock_nanosleep_monotonic_race.rs`, `tests/rust/print_clock_nanosleep_monotonic_abs_race.rs` and `tests/rust/print_clock_nanosleep_realtime_abs_race.rs`.
- T8 (covers F13, F14): raw calls with the caller's own `pthread_getcpuclockid` id (`EINVAL`), with a sibling thread's id while that thread spins (returns 0 after the sibling consumed the requested virtual CPU time), with the id of an exited thread (`EINVAL`), and with an FD clock id built from a descriptor for `/dev/null`, both with a valid and with a NULL `req` (`EOPNOTSUPP`). The results must be identical across `hermit run --verify` runs.
- T9 (covers F2, F4): read `clock_gettime(CLOCK_MONOTONIC)` and `clock_gettime(CLOCK_BOOTTIME)`, add 10 ms, and sleep with `TIMER_ABSTIME` on the same clock. Each sleep must return after 10 ms of virtual time, not at once and not after decades. A `CLOCK_REALTIME` sleep built the same way must behave the same.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (sleeps become scheduler SleepUntil requests on virtual time); linux_determinism conditional -> nondeterministic; 12 flag behaviours and 7 tests listed; gaps from code reading: clockid ignored, stray flag bits injected to the host, negative flags reach unreachable!; tracking_status stays todo, with open questions
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13, 6, 10 and 14 (sleep honours clockid on virtual realtime, uptime or CPU time; EOPNOTSUPP/EPERM/EINVAL in kernel order; flags masked, removing the host injection and the unreachable! panic; today's sequentialized form is already a scheduler wait); target_policy stays model; new F13, F14, T8, T9; removed all three open questions (clockid validation, CPU-clock meaning, flag masking)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `clock_nanosleep`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_nanosleep&body=Discussion+for+the+%60clock_nanosleep%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_nanosleep.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_nanosleep%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `clock_nanosleep`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### 2026-10-03 host probe

Host: Linux 7.1.3 x86_64. A C program issued raw `syscall(SYS_clock_nanosleep, ...)` with a 1 ms interval unless noted, traced with strace. Results: `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`, `CLOCK_TAI` → 0; `CLOCK_THREAD_CPUTIME_ID` → `EOPNOTSUPP`; `CLOCK_MONOTONIC_RAW`, `CLOCK_REALTIME_COARSE`, `CLOCK_MONOTONIC_COARSE` → `EOPNOTSUPP`; `CLOCK_REALTIME_ALARM`, `CLOCK_BOOTTIME_ALARM` → `EPERM`; clock id 99 → `EINVAL`; flags `2` and `0x100` → 0; past absolute deadline → 0; `tv_nsec = 1000000000` → `EINVAL`; clock id 99 with `req` = 1 → `EINVAL`; `CLOCK_MONOTONIC_RAW` with `req` = 1 → `EOPNOTSUPP`; `CLOCK_MONOTONIC` with `req` = 1 → `EFAULT`. glibc `clock_nanosleep(CLOCK_THREAD_CPUTIME_ID, ...)` returned 22 (`EINVAL`) and strace showed no syscall. A relative `CLOCK_PROCESS_CPUTIME_ID` sleep in a single-threaded process did not return and was killed.
